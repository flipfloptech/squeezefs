//! THE ONE CLIENT LIBRARY (charter §7; decision 0003 §F "ONE client
//! library"; spec `docs/porting/il_shim.md` §3, `il_sdk.md` §3): the session
//! client both the LD_PRELOAD shim and the native SDK consume — the
//! rendezvous and the three authentication rungs, the map, the one-op
//! protocol, slot runs and pipelined flights, no-wait tickets, the
//! completion-doorbell park, the heartbeat watch, poison and teardown. The
//! shim adds interposition + the fd table + the offset mirror; the SDK adds
//! buffer leases; neither re-implements a byte of this.
//!
//! Raw `std.os.linux` syscalls ONLY (and `std.atomic`): the shim links this
//! library into an interposing `.so`, where `std.posix` would resolve to
//! the shim's OWN exported `read`/`write`/`close`/… — a recursion into the
//! interposer. Allocation-free per op: every op works in the slot's slab
//! window and a fixed flight table on the stack.
//!
//! The one-op protocol (spec §3.3): poisoned? → claim (CAS FREE→CLAIMED +
//! the generation) → fill (payload into the slab, the descriptor, the
//! ingress stamp) → publish SUBMITTED (Release) → ring push → doorbell
//! (SeqCst bump, wake iff the daemon is parked) → spin → park (WAITER bit,
//! 50 ms futex slices re-checking poison, the heartbeat and the deadline)
//! → consume (the result after the DONE Acquire; the payload out of the
//! slab) → release (FREE, Release). An ABANDONED slot (deadline, poison) is
//! never reused: the daemon may complete it arbitrarily late and recycling
//! would hand a stale completion to a future op; it is GC'd with the
//! session.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const layout = @import("layout.zig");
const slot_core = @import("slot_core.zig");
const ring_core = @import("ring_core.zig");
const doorbell = @import("doorbell.zig");
const wire = @import("wire.zig");
const sizing = @import("sizing.zig");
const rendezvous = @import("rendezvous.zig");
const rl = @import("rendezvous_linux.zig");
const shm = @import("shm.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
pub const direct_client = @import("direct_client.zig");
pub const direct_client_core = @import("write_lease_core.zig");
pub const futex = @import("../sync/futex.zig");
const derive = @import("../core/derive.zig");

pub const Kind = wire.ClientKind;
pub const Durability = wire.Durability;
pub const Rights = wire.Rights;
pub const RefuseClass = wire.RefuseClass;

/// Adaptive spin before the park: the full window, or the parky floor once
/// the session's LAST op parked (a parked op means daemon-side async work —
/// the full window is CPU theft: 336 k → 605 k IOPS at 64 threads, 256 the
/// measured best floor; shorter over-parks just-completing ops).
pub const WAIT_SPINS_DEFAULT: u32 = 4096;
pub const WAIT_SPINS_PARKY: u32 = 256;
/// Flights staged under ONE doorbell per batch of a large op: 64 × the
/// 1 MiB default window = 64 MiB per batch, the whole default arena.
pub const MAX_FLIGHTS: usize = 64;
/// `futex_waitv`'s entry cap, the `waitAny` bound.
pub const WAIT_ANY_MAX = futex.wait_many_max;

pub const EstablishError = error{
    /// The blob's abi or build commit disagree with ours (the pre-check;
    /// the daemon enforces the same law authoritatively).
    VersionSkew,
    /// Rung 1: the path rendezvous directory is not trustworthy.
    UntrustedRendezvous,
    /// Rung 2: the daemon's uid is neither root nor the mount owner.
    PeerUntrusted,
    /// Rung 3: the session memfd lacks a seal.
    Unsealed,
    /// The daemon answered REFUSE (`Session.last_refusal`).
    Refused,
    /// A malformed reply, a bad header, the memfd's size ≠ the layout's.
    Protocol,
    /// The ctl socket failed (connect, EOF).
    Socket,
    Map,
    /// `futex_waitv` is missing (pre-5.16): the venue is 7.2.6 — fail loud.
    FutexWaitvUnsupported,
};

pub const BindError = error{ Poisoned, Refused, Protocol, Socket };

pub const Options = struct {
    kind: Kind = .shim,
    build_commit: []const u8,
    /// The mount root's `st_uid` from the credential's own `fstat` — the
    /// trust anchor of rungs 1 and 2, never re-stat'ed.
    mount_uid: u32,
    env: *const std.process.Environ.Map,
    sdk_abi: u32 = 0,
    arena_bytes_req: u64 = 0,
    slots_req: u32 = 0,
};

/// A served op's outcome, the three answers an interposer needs.
pub const Outcome = union(enum) {
    /// Bytes moved.
    served: usize,
    /// A kernel-identical errno (e.g. EBADF for the wrong direction).
    errno: linux.E,
    /// The real call is the correct answer: the session is poisoned, no slot
    /// was free, the daemon rejected OUR descriptor (EINVAL = our bookkeeping
    /// diverged), or the op was abandoned with nothing done.
    fallthrough,
};

pub const Binding = struct {
    id: u64,
    ino: u64,
    rights: Rights,
    durability: Durability,
};

/// A no-wait op in flight: the slot and the generation that owns it.
pub const Ticket = struct {
    slot: u32,
    generation: u64,
};

/// The daemon's REFUSE, for the consumer's reason line (an SDK version
/// refusal names the daemon's accepted window).
pub const Refusal = struct {
    class: RefuseClass,
    sdk_abi_min: u32 = 0,
    sdk_abi_max: u32 = 0,
};

/// A `waitAny` entry: a session's completion doorbell and the snapshot its
/// park returned.
pub const WaitEntry = struct {
    session: *Session,
    expected: u32,
};

/// A futex mutex (Drepper's three-state) for the ctl socket: BIND/UNBIND
/// round trips from many app threads serialize here; never an `Io`. `pub`:
/// the SDK's batch bookkeeping takes the same lock shape.
pub const CtlLock = struct {
    word: futex.Word = .init(0),

    pub fn tryLock(self: *CtlLock) bool {
        return self.word.cmpxchgStrong(0, 1, .acquire, .monotonic) == null;
    }

    pub fn lock(self: *CtlLock) void {
        if (self.tryLock()) return;
        while (self.word.swap(2, .acquire) != 0) futex.wait(&self.word, 2);
    }

    pub fn unlock(self: *CtlLock) void {
        if (self.word.swap(0, .release) == 2) futex.wakeOne(&self.word);
    }
};

pub const Session = struct {
    ctl: linux.fd_t,
    ctl_lock: CtlLock = .{},
    mapped: shm.Mapped,
    layout: layout.SessionLayout,
    m: layout.Mapping,
    ring: ring_core.View,
    kind: Kind,
    features: u64,
    my_generation: u64,
    poison_flag: Atomic(bool) = .init(false),
    /// A BIND's REFUSE class.
    last_refusal: ?RefuseClass = null,
    slot_hint: Atomic(u32) = .init(0),
    last_op_parked: Atomic(bool) = .init(false),
    /// `SQUEEZEFS_IL_SPINS` pins both regimes.
    spins_explicit: ?u32,
    op_timeout_ns: u64,
    /// `SQUEEZEFS_TEST_IL_MAX_RUN_SLOTS` (0 = uncapped).
    max_run_slots: u32,
    /// The hybrid lane gate's threshold (0 = off).
    lane_gate_min: u64,
    heartbeat: rendezvous.HeartbeatWatch,
    /// The impossible observation: a full ring under `slots ≤ ring_entries`
    /// means OUR bookkeeping is corrupt (the session poisons; the shim's
    /// refusal line names `il_ring_full_impossible`).
    il_ring_full_impossible: Atomic(u64) = .init(0),
    /// Ops abandoned to the deadline or a poison (their slots are dead).
    il_abandoned_slots: Atomic(u64) = .init(0),
    /// The direct arm (`direct_client.zig`): granted, mapped and the
    /// devices open — else null and `direct_state` says why.
    direct: ?direct_client.Direct = null,
    direct_state: DirectState = .off,

    pub const DirectState = enum(u8) {
        /// The daemon did not grant it (not `-o interception=direct`, or
        /// the device screen), or `SQUEEZEFS_IL_DIRECT=0`.
        off,
        on,
        /// Granted at HELLO but refused at `DirectHello`.
        refused,
        /// A data device node would not open for this uid (EACCES-class):
        /// the kernel's final word.
        open_denied,
        /// The lease page failed its seals or its geometry.
        protocol,
    };

    // ---------------------------------------------------------------------
    // Establish / teardown.
    // ---------------------------------------------------------------------

    /// The whole ladder: pre-check → connect (rung 1 before the path rung) →
    /// rung 2 → HELLO with the credential → rung 3 → map PMD-aligned →
    /// validate the sealed header against the memfd's size → the lane gate.
    /// `cred_fd` stays the caller's (the kernel dups it across). A
    /// `Refused` failure leaves the daemon's class in `refusal`.
    pub fn establish(gpa: std.mem.Allocator, blob: *const wire.Il0Blob, cred_fd: linux.fd_t, opts: Options, refusal: *?Refusal) EstablishError!Session {
        const allow_dirty = rendezvous.allowDirtyFromEnv(opts.env);
        if (rendezvous.preCheck(blob, opts.build_commit, opts.kind, allow_dirty) != null) return error.VersionSkew;
        const sock = rl.connectLadder(blob, opts.mount_uid) catch |e| return switch (e) {
            error.UntrustedRendezvous => error.UntrustedRendezvous,
            else => error.Socket,
        };
        errdefer _ = linux.close(sock);
        rl.authenticatePeer(sock, opts.mount_uid) catch return error.PeerUntrusted;
        const reply = rl.hello(sock, cred_fd, .{
            .kind = opts.kind,
            .build_commit = opts.build_commit,
            .nonce = blob.nonce,
            .sdk_abi = opts.sdk_abi,
            .arena_bytes_req = opts.arena_bytes_req,
            .slots_req = opts.slots_req,
        }) catch |e| return switch (e) {
            error.Eof, error.SendFailed => error.Socket,
            else => error.Protocol,
        };
        const memfd, const features = switch (reply) {
            .session => |s| .{ s.memfd, s.features },
            .refused => |r| {
                refusal.* = .{ .class = r.class, .sdk_abi_min = r.sdk_abi_min, .sdk_abi_max = r.sdk_abi_max };
                return error.Refused;
            },
        };
        defer _ = linux.close(memfd);
        shm.checkSeals(memfd) catch return error.Unsealed;
        // Geometry authority is the SEALED header: read it off the first
        // page, compute the layout, and require the memfd's size to be
        // exactly the layout's total before the whole thing is mapped.
        const size = shm.fileSize(memfd) catch return error.Protocol;
        if (size < layout.PAGE_BYTES) return error.Protocol;
        const page = linux.mmap(null, layout.PAGE_BYTES, .{ .READ = true }, .{ .TYPE = .SHARED }, memfd, 0);
        if (linux.errno(page) != .SUCCESS) return error.Map;
        const header_copy = layout.SessionHeader.decode(@as([*]const u8, @ptrFromInt(page))[0..layout.PAGE_BYTES]) catch {
            _ = linux.munmap(@ptrFromInt(page), layout.PAGE_BYTES);
            return error.Protocol;
        };
        _ = linux.munmap(@ptrFromInt(page), layout.PAGE_BYTES);
        const l = layout.SessionLayout.compute(header_copy.geometry) catch return error.Protocol;
        if (l.total_bytes != size or l.slab == 0) return error.Protocol;
        const mapped = shm.mapSharedPmdAligned(memfd, l) catch return error.Map;
        errdefer shm.unmap(mapped);
        if (shm.thpFromEnv(opts.env)) shm.advise(mapped);
        const m = mapped.mapping;
        // The live header must still validate (a scribble between the two
        // reads is a hostile daemon: refuse).
        m.header().validate() catch return error.Protocol;
        const my_generation = m.header().generation.load(.acquire);
        // Require futex_waitv: a one-entry wait against a stale expectation
        // returns at once, or ENOSYS.
        const probe = [_]futex.Waiter{.{ .word = &m.header().cqe.seq, .expect = m.header().cqe.seq.load(.monotonic) +% 1 }};
        if (futex.waitMany(&probe, 0) == .unsupported) return error.FutexWaitvUnsupported;
        // The hybrid lane gate's threshold, published to the stats page.
        const membw = sizing.probeMemBandwidth(gpa) catch sizing.MEMBW_PROBE_FLOOR_BYTES_PER_S;
        const lane_gate_min = sizing.kernelLaneMinFromEnv(opts.env, membw, l.slab, l.geometry.max_op_bytes);
        m.statsPage().lane_gate_threshold_bytes.store(lane_gate_min, .monotonic);
        // The direct arm: granted at HELLO, established over the same
        // socket before any op; a failure leaves a ring session.
        var direct: ?direct_client.Direct = null;
        var direct_state: DirectState = .off;
        if (features & wire.FEATURE_DIRECT != 0 and knobs.boolKnob("SQUEEZEFS_IL_DIRECT", opts.env)) {
            const write = features & wire.FEATURE_DIRECT_WRITE != 0 and knobs.boolKnob("SQUEEZEFS_IL_DIRECT_WRITE_ARM", opts.env);
            if (direct_client.Direct.establish(sock, write)) |d| {
                direct = d;
                direct_state = .on;
            } else |e| direct_state = switch (e) {
                error.Refused => .refused,
                error.DeviceOpen => .open_denied,
                error.Protocol, error.Unsealed, error.Map => .protocol,
            };
        }
        return .{
            .ctl = sock,
            .direct = direct,
            .direct_state = direct_state,
            .mapped = mapped,
            .layout = l,
            .m = m,
            .ring = ring_core.View.fromMapping(m),
            .kind = opts.kind,
            .features = features,
            .my_generation = my_generation,
            .spins_explicit = knobs.optIntKnob("SQUEEZEFS_IL_SPINS", u32, opts.env),
            .op_timeout_ns = rendezvous.opTimeoutFromEnv(opts.env),
            .max_run_slots = knobs.intKnob("SQUEEZEFS_TEST_IL_MAX_RUN_SLOTS", u32, opts.env),
            .lane_gate_min = lane_gate_min,
            .heartbeat = rendezvous.HeartbeatWatch.init(m.header().heartbeat.load(.monotonic), clock.monotonicNs()),
        };
    }

    /// Same-process poison: the flag, then `shutdown(SHUT_RDWR)` — never
    /// `close`, so the fd number cannot be reused under a racing ctl call.
    /// A poisoned session never emits ctl traffic again.
    pub fn poison(self: *Session) void {
        if (self.poison_flag.swap(true, .acq_rel)) return;
        _ = linux.shutdown(self.ctl, linux.SHUT.RDWR);
    }

    /// The fork child's poison (async-signal-safe: a store and a `close`):
    /// `close(2)` the child's inherited fd COPY — never `shutdown`, which
    /// acts on the description fork SHARES and would sever the parent's
    /// live session.
    pub fn poisonChild(self: *Session) void {
        self.poison_flag.store(true, .release);
        _ = linux.close(self.ctl);
    }

    /// Poisoned = the flag, or the daemon bumped the generation (its
    /// poison/reap broadcast): every op falls through to the real call.
    pub fn poisoned(self: *Session) bool {
        if (self.poison_flag.load(.acquire)) return true;
        if (self.m.header().generation.load(.acquire) != self.my_generation) {
            self.poison_flag.store(true, .release);
            return true;
        }
        return false;
    }

    /// Final teardown (process exit, a lost registry race, tests): unmap and
    /// close the ctl socket. Sessions otherwise live for the process.
    pub fn teardown(self: *Session) void {
        if (self.direct) |*d| d.teardown();
        shm.unmap(self.mapped);
        _ = linux.close(self.ctl);
        self.* = undefined;
    }

    pub fn header(self: *Session) *layout.SessionHeader {
        return self.m.header();
    }

    pub fn statsPage(self: *Session) *layout.ClientStatsPage {
        return self.m.statsPage();
    }

    pub fn slab(self: *const Session) u64 {
        return self.layout.slab;
    }

    // ---------------------------------------------------------------------
    // Bind / unbind.
    // ---------------------------------------------------------------------

    /// BIND with the fd (the daemon screens the DESCRIPTION and closes its
    /// dup right after) and the SDK's durability word.
    pub fn bind(self: *Session, fd: linux.fd_t, durability: Durability) BindError!Binding {
        if (self.poisoned()) return error.Poisoned;
        self.ctl_lock.lock();
        defer self.ctl_lock.unlock();
        const req: wire.Bind = .{ .durability = @backingInt(durability) };
        rl.sendCtl(self.ctl, wire.frameBytes(&req), fd) catch return error.Socket;
        var buf: [wire.CTL_MSG_MAX]u8 = undefined;
        const r = rl.recvCtl(self.ctl, &buf) catch |e| return switch (e) {
            error.Eof => error.Socket,
            else => error.Protocol,
        };
        // A reply carrying a descriptor is not this protocol: close it.
        if (r.fd) |f| {
            _ = linux.close(f);
            return error.Protocol;
        }
        const frame = buf[0..r.len];
        switch (wire.tagOf(frame) catch return error.Protocol) {
            .bind_ok => {
                const ok = wire.decodeFixed(wire.BindOk, frame) catch return error.Protocol;
                return .{ .id = ok.binding_id, .ino = ok.ino, .rights = ok.rights, .durability = @fromBackingInt(@intCast(ok.durability_granted)) };
            },
            .bind_refused => {
                const ref = wire.decodeFixed(wire.BindRefused, frame) catch return error.Protocol;
                self.last_refusal = ref.refuseClass() catch return error.Protocol;
                return error.Refused;
            },
            else => return error.Protocol,
        }
    }

    /// Best-effort, silent, NEVER on a poisoned session (the atfork child
    /// must not unbind a binding the parent still uses).
    pub fn unbind(self: *Session, id: u64) void {
        const lease = if (self.direct) |*d| d.forget(id) else null;
        defer if (lease) |l| l.drop();
        if (self.poisoned()) return;
        self.ctl_lock.lock();
        defer self.ctl_lock.unlock();
        if (lease) |l| self.sendNoteLocked(l, id, false);
        const req: wire.Unbind = .{ .binding_id = id };
        rl.sendCtl(self.ctl, wire.frameBytes(&req), null) catch {};
    }

    /// `unbind` that never blocks: false = the ctl lock is held by another
    /// thread (the shim's `close` is async-signal-safe by POSIX, so it
    /// defers the unbind instead of parking on a lock the interrupted
    /// thread may hold). A poisoned session answers true (nothing to send).
    pub fn tryUnbind(self: *Session, id: u64) bool {
        if (self.poisoned()) {
            if (self.direct) |*d| if (d.forget(id)) |l| l.drop();
            return true;
        }
        if (!self.ctl_lock.tryLock()) return false;
        defer self.ctl_lock.unlock();
        if (self.direct) |*d| if (d.forget(id)) |l| {
            self.sendNoteLocked(l, id, false);
            l.drop();
        };
        const req: wire.Unbind = .{ .binding_id = id };
        rl.sendCtl(self.ctl, wire.frameBytes(&req), null) catch {};
        return true;
    }

    // ---------------------------------------------------------------------
    // The hybrid lane gate.
    // ---------------------------------------------------------------------

    /// Strictly larger than the threshold routes to kernel FUSE (0 = off).
    pub fn routesKernel(self: *const Session, len: u64) bool {
        return self.lane_gate_min != 0 and len > self.lane_gate_min;
    }

    /// The consumer declined to ring an op: counted on the stats page.
    pub fn noteKernelRoute(self: *Session, len: u64) void {
        _ = self.statsPage().lane_gate_kernel_routes.fetchAdd(1, .monotonic);
        _ = self.statsPage().lane_gate_kernel_bytes.fetchAdd(len, .monotonic);
    }

    // ---------------------------------------------------------------------
    // Slots: claim, runs.
    // ---------------------------------------------------------------------

    /// One FREE slot from a rotating hint, or null (counted
    /// `il_slot_reroutes`: the consumer runs the op on the real fd).
    fn claimSlot(self: *Session) ?struct { index: u32, generation: u64 } {
        const n = self.layout.geometry.slots;
        const start = self.slot_hint.fetchAdd(1, .monotonic) % n;
        var i: u32 = 0;
        while (i < n) : (i += 1) {
            const idx = (start + i) % n;
            if (slot_core.tryClaim(&self.m.slots()[idx])) |gen| return .{ .index = idx, .generation = gen };
        }
        _ = self.statsPage().il_slot_reroutes.fetchAdd(1, .monotonic);
        return null;
    }

    pub const Run = struct { base: u32, len: u32, generation: u64 };

    /// A contiguous, non-wrapping run of up to `want` slots: pass 1 tries
    /// bases aligned at multiples of `want` (concurrent large writers PACK
    /// runs instead of interleaving — ring/op 1.86 instead of 1.00 measured
    /// on the 16-thread 1 MiB row), pass 2 greedy from any FREE base. Only
    /// the BASE is ever submitted; the extension slots are CLAIMED arena
    /// holds the daemon never sees. A shorter run under fragmentation is
    /// progress, never a refusal; null = no slot at all.
    pub fn claimRun(self: *Session, want_raw: u32) ?Run {
        const n = self.layout.geometry.slots;
        const want = @max(1, @min(want_raw, if (self.max_run_slots == 0) n else self.max_run_slots));
        const slots = self.m.slots();
        if (want > 1) {
            var base: u32 = 0;
            while (base + want <= n) : (base += want) {
                if (self.extendRun(slots, base, want)) |r| return r;
            }
        }
        var base: u32 = 0;
        while (base < n) : (base += 1) {
            if (self.extendRun(slots, base, want)) |r| return r;
        }
        _ = self.statsPage().il_slot_reroutes.fetchAdd(1, .monotonic);
        return null;
    }

    fn extendRun(self: *Session, slots: []layout.IpcSlot, base: u32, want: u32) ?Run {
        const gen = slot_core.tryClaim(&slots[base]) orelse return null;
        var len: u32 = 1;
        while (len < want and base + len < self.layout.geometry.slots) : (len += 1) {
            if (slot_core.tryClaim(&slots[base + len]) == null) break;
        }
        return .{ .base = base, .len = len, .generation = gen };
    }

    fn releaseRun(self: *Session, run: Run, base_done: bool) void {
        const slots = self.m.slots();
        if (base_done) slot_core.release(&slots[run.base]);
        var i: u32 = 1;
        while (i < run.len) : (i += 1) slot_core.releaseClaimed(&slots[run.base + i]);
    }

    // ---------------------------------------------------------------------
    // The one-op protocol.
    // ---------------------------------------------------------------------

    fn spins(self: *const Session) u32 {
        if (self.spins_explicit) |s| return s;
        return if (self.last_op_parked.load(.monotonic)) WAIT_SPINS_PARKY else WAIT_SPINS_DEFAULT;
    }

    /// Fill + publish + ring push (NO doorbell: a batch rings once).
    fn publish(self: *Session, index: u32, op: layout.Op, binding: u64, offset: u64, len: u32) bool {
        const slot = &self.m.slots()[index];
        slot_core.publishDescriptor(slot, .{ .op = op, .binding = binding, .offset = offset, .len = len, .arena_off = @as(u64, index) * self.layout.slab });
        slot_core.stampIngress(slot, clock.monotonicNs());
        slot_core.publishSubmitted(slot);
        if (!self.ring.push(index)) {
            _ = self.il_ring_full_impossible.fetchAdd(1, .monotonic);
            self.poison();
            return false;
        }
        return true;
    }

    fn ringDoorbell(self: *Session) void {
        _ = doorbell.ringSubmit(self.header());
    }

    const Wait = enum { done, abandoned };

    /// Spin, then park in 50 ms slices re-checking DONE, poison, the
    /// heartbeat ladder and the deadline. Returns `abandoned` with the slot
    /// left as it is (never recycled).
    fn waitDone(self: *Session, index: u32, generation: u64) Wait {
        const slot = &self.m.slots()[index];
        var spin: u32 = 0;
        const budget = self.spins();
        while (spin < budget) : (spin += 1) {
            if (slot_core.isDoneFor(slot, generation)) return .done;
            std.atomic.spinLoopHint();
        }
        const deadline = clock.monotonicNs() + self.op_timeout_ns;
        self.last_op_parked.store(true, .monotonic);
        while (true) {
            switch (slot_core.parkPrepare(slot)) {
                .ready => return .done,
                .park => |expected| {
                    const now = clock.monotonicNs();
                    if (now >= deadline) break;
                    _ = futex.waitSharedTimeout(slot_core.word(slot), expected, @min(deadline - now, rendezvous.WAIT_SLICE_NS));
                },
            }
            if (slot_core.isDoneFor(slot, generation)) return .done;
            if (self.poisoned()) break;
            if (!self.heartbeatAlive(clock.monotonicNs())) break;
        }
        self.poison();
        _ = self.il_abandoned_slots.fetchAdd(1, .monotonic);
        return .abandoned;
    }

    /// The heartbeat ladder's first two rungs: a word unchanged for 1 s asks
    /// the ctl socket; EOF/ECONNRESET there is a dead daemon. The caller's
    /// deadline is the last rung.
    pub fn heartbeatAlive(self: *Session, now_ns: u64) bool {
        return switch (self.heartbeat.observe(self.header().heartbeat.load(.monotonic), now_ns)) {
            .alive => true,
            .probe => rl.socketAlive(self.ctl),
        };
    }

    /// `result ≥ 0` ⇒ the payload (reads) out of the slab AFTER the DONE
    /// Acquire, the slot released; `-EINVAL` ⇒ a descriptor reject = OUR
    /// bookkeeping diverged ⇒ fallthrough; any other errno kernel-identical.
    fn consume(self: *Session, index: u32, dst: ?[]u8) Outcome {
        const slot = &self.m.slots()[index];
        const r = slot_core.result(slot);
        if (r >= 0) {
            const n: usize = @intCast(r);
            if (dst) |d| {
                // The op's window starts at the base slot's slab and spans
                // the RUN (a multi-slot flight is `len > slab`); the daemon
                // validated `result ≤ len` against the same arena bounds.
                const take = @min(n, d.len);
                const window = self.m.arenaWindow(@as(u64, index) * self.layout.slab, take) catch unreachable;
                @memcpy(d[0..take], window);
            }
            slot_core.release(slot);
            return .{ .served = n };
        }
        slot_core.release(slot);
        const e: linux.E = @fromBackingInt(@intCast(@as(u16, @intCast(-r))));
        if (e == .INVAL) return .fallthrough;
        return .{ .errno = e };
    }

    /// One slab-sized op, start to finish.
    fn singleOp(self: *Session, op: layout.Op, binding: u64, offset: u64, buf: []u8) Outcome {
        if (self.poisoned()) return .fallthrough;
        const c = self.claimSlot() orelse return .fallthrough;
        if (op == .write) {
            const window = self.m.slotWindow(c.index) catch unreachable;
            @memcpy(window[0..buf.len], buf);
        }
        if (!self.publish(c.index, op, binding, offset, @intCast(buf.len))) return .fallthrough;
        self.ringDoorbell();
        switch (self.waitDone(c.index, c.generation)) {
            .done => return self.consume(c.index, if (op == .read) buf else null),
            .abandoned => return .fallthrough,
        }
    }

    const Flight = struct {
        run: Run,
        offset: u64,
        len: u32,
        dst_off: usize,
    };

    /// A large op: chunks sized to the full `max_op` window via slot runs,
    /// every chunk staged (claim + copy + publish) then the batch ring-
    /// pushed under ONE doorbell (sibling chunks landing in one drain pass
    /// is what lets the daemon's placed sever adopt a whole block: ~50 %
    /// sever engagement pre-batch at t16×4 MiB). Flights reap oldest-offset
    /// first with POSIX prefix semantics: the count is the contiguous prefix
    /// ending at the first short or failed chunk; later flights are drained
    /// and ignored. Abandoned mid-way ⇒ `served(done)` if done > 0 else
    /// fallthrough; a first-chunk reject ⇒ fallthrough.
    fn multiOp(self: *Session, op: layout.Op, binding: u64, offset: u64, buf: []u8) Outcome {
        if (self.poisoned()) return .fallthrough;
        const slab_bytes = self.layout.slab;
        const max_op: u64 = self.layout.geometry.max_op_bytes;
        var done: usize = 0;
        var cursor: usize = 0;
        var first = true;
        while (cursor < buf.len) {
            // Stage a batch of flights.
            var flights: [MAX_FLIGHTS]Flight = undefined;
            var n: usize = 0;
            while (n < MAX_FLIGHTS and cursor < buf.len) {
                const remaining = buf.len - cursor;
                const want_bytes: u64 = @min(remaining, max_op);
                const want_slots: u32 = @intCast(derive.ceilDiv(want_bytes, slab_bytes));
                const run = self.claimRun(want_slots) orelse break;
                // The flight is the run's bytes capped at `max_op` (the
                // daemon's per-op screen): a slab that does not divide
                // `max_op` — an arena that yielded to the population floor,
                // 12 KiB slabs under a 1 MiB ceiling — rounds the run UP.
                const chunk: u32 = @intCast(@min(want_bytes, @as(u64, run.len) * slab_bytes));
                if (op == .write) {
                    const window = self.m.arenaWindow(@as(u64, run.base) * slab_bytes, chunk) catch unreachable;
                    @memcpy(window, buf[cursor..][0..chunk]);
                }
                flights[n] = .{ .run = run, .offset = offset + cursor, .len = chunk, .dst_off = cursor };
                if (!self.publish(run.base, op, binding, offset + cursor, chunk)) {
                    self.releaseRun(run, false);
                    return if (done > 0) .{ .served = done } else .fallthrough;
                }
                n += 1;
                cursor += chunk;
            }
            if (n == 0) return if (done > 0) .{ .served = done } else .fallthrough;
            self.ringDoorbell();
            // Reap in offset order.
            var stop = false;
            var i: usize = 0;
            while (i < n) : (i += 1) {
                const f = flights[i];
                switch (self.waitDone(f.run.base, f.run.generation)) {
                    .abandoned => {
                        // The base slot is dead; the holds go back.
                        self.releaseRun(f.run, false);
                        return if (done > 0) .{ .served = done } else .fallthrough;
                    },
                    .done => {},
                }
                const outcome = self.consume(f.run.base, if (op == .read) buf[f.dst_off..][0..f.len] else null);
                self.releaseRun(f.run, false);
                if (stop) continue;
                switch (outcome) {
                    .served => |k| {
                        done += k;
                        if (k < f.len) stop = true;
                    },
                    .fallthrough => {
                        if (first) return .fallthrough;
                        stop = true;
                    },
                    .errno => |e| {
                        if (done == 0) return .{ .errno = e };
                        stop = true;
                    },
                }
                first = false;
            }
            if (stop) break;
        }
        return .{ .served = done };
    }

    /// `pread`: the direct arm first (the device itself, under the binding's
    /// layout lease), then the ring — `len ≤ slab` is the allocation-free
    /// single flight; larger lengths pipeline flights.
    pub fn pread(self: *Session, binding: u64, dst: []u8, offset: u64) Outcome {
        if (dst.len == 0) return .{ .served = 0 };
        if (self.directPread(binding, dst, offset)) |o| return o;
        return self.preadRing(binding, dst, offset);
    }

    /// The ring alone (the shim asked the direct arm itself, before its gate).
    pub fn preadRing(self: *Session, binding: u64, dst: []u8, offset: u64) Outcome {
        if (dst.len == 0) return .{ .served = 0 };
        if (dst.len <= self.layout.slab) return self.singleOp(.read, binding, offset, dst);
        return self.multiOp(.read, binding, offset, dst);
    }

    /// The direct arm alone: `served` (the device's bytes in `dst`), a
    /// `fallthrough` (the session poisoned under the heartbeat ladder), or
    /// null (no arm, or the op is the ring's / the kernel's). The shim asks
    /// BEFORE its lane gate: the gate prices the ring's copies, which the
    /// arm never pays, so a 1 MiB read within one block is one device read.
    pub fn directPread(self: *Session, binding: u64, dst: []u8, offset: u64) ?Outcome {
        const d = &(self.direct orelse return null);
        if (self.poisoned()) return null;
        // A direct-only client never parks on a slot, so the heartbeat
        // ladder runs here: a dead daemon can no longer move the epochs its
        // leases rest on — poison, and the op falls through to the kernel
        // (loud: the mount is gone with its daemon).
        if (!self.heartbeatAlive(clock.monotonicNs())) {
            self.poison();
            return .fallthrough;
        }
        return switch (d.pread(binding, dst, offset, self.directStats(), self)) {
            .served => |n| .{ .served = n },
            .ring => null,
        };
    }

    fn directStats(self: *Session) direct_client.Stats {
        const page = self.statsPage();
        return .{
            .ops = &page.il_direct_ops,
            .bytes = &page.il_direct_bytes,
            .rechecks_failed = &page.il_direct_rechecks_failed,
            .asks = &page.il_lease_asks,
            .writes = &page.il_direct_writes,
            .write_bytes = &page.il_direct_write_bytes,
            .write_notes = &page.il_direct_write_notes,
            .writes_recalled = &page.il_direct_writes_recalled,
            .ledger = &page.lease_ledger,
        };
    }

    /// The direct arm's lease ask under the ctl lock (`Direct.pread`'s asker).
    pub fn ask(self: *Session, d: *direct_client.Direct, binding: u64, stats: direct_client.Stats, want_write: bool) ?*direct_client.Lease {
        if (self.poisoned()) return null;
        self.ctl_lock.lock();
        defer self.ctl_lock.unlock();
        return d.ask(self.ctl, binding, stats, want_write);
    }

    /// The write arm's batched postlude note under the ctl lock
    /// (`Direct.pwrite`'s noter); `ack` waits for the daemon's `WriteNoteOk`.
    pub fn note(self: *Session, l: *direct_client.Lease, binding: u64, stats: direct_client.Stats, ack: bool) void {
        if (self.poisoned()) return;
        self.ctl_lock.lock();
        defer self.ctl_lock.unlock();
        self.sendNoteLocked(l, binding, ack);
        _ = stats.write_notes.fetchAdd(1, .monotonic);
    }

    fn sendNoteLocked(self: *Session, l: *direct_client.Lease, binding: u64, ack: bool) void {
        const n = direct_client.Direct.takeNote(l, binding, ack) orelse return;
        rl.sendCtl(self.ctl, wire.frameBytes(&n), null) catch return;
        if (!ack) return;
        var buf: [wire.CTL_MSG_MAX]u8 = undefined;
        const r = rl.recvCtl(self.ctl, &buf) catch return;
        if (r.fd) |f| _ = linux.close(f);
    }

    /// The direct write: `served`, `errno` (a write landed under a forced
    /// recall: EIO), a `fallthrough` (poisoned under the heartbeat), or null
    /// (the ring's op).
    pub fn directPwrite(self: *Session, binding: u64, src: []const u8, offset: u64) ?Outcome {
        const d = &(self.direct orelse return null);
        if (self.poisoned()) return null;
        if (!self.heartbeatAlive(clock.monotonicNs())) {
            self.poison();
            return .fallthrough;
        }
        return switch (d.pwrite(binding, src, offset, self.directStats(), self)) {
            .served => |n| .{ .served = n },
            .eio => .{ .errno = .IO },
            .ring => null,
        };
    }

    /// `fsync`/`fdatasync` of a bound fd on the direct write arm: the
    /// pending note sent and ACKED (the stamp lands before the kernel's
    /// FSYNC the caller makes next), then `fdatasync` on every device
    /// written since the last. False = a device sync failed (the caller's
    /// real fsync reports the kernel's own verdict regardless).
    pub fn directFsync(self: *Session, binding: u64) bool {
        const d = &(self.direct orelse return true);
        const l = d.leaseFor(binding) orelse return true;
        defer l.drop();
        if (!l.writable) return true;
        self.note(l, binding, self.directStats(), true);
        return d.fdatasyncDirty(l);
    }

    pub fn pwrite(self: *Session, binding: u64, src: []const u8, offset: u64) Outcome {
        if (src.len == 0) return .{ .served = 0 };
        if (self.directPwrite(binding, src, offset)) |o| return o;
        return self.pwriteRing(binding, src, offset);
    }

    /// The ring alone (the shim asked the direct arm itself, before its gate).
    pub fn pwriteRing(self: *Session, binding: u64, src: []const u8, offset: u64) Outcome {
        if (src.len == 0) return .{ .served = 0 };
        // The write path only reads `src`; the shared body takes a mutable
        // slice for the read direction's copy-out.
        const buf: []u8 = @constCast(src);
        if (src.len <= self.layout.slab) return self.singleOp(.write, binding, offset, buf);
        return self.multiOp(.write, binding, offset, buf);
    }

    // ---------------------------------------------------------------------
    // No-wait tickets (the libaio ring lane / the SDK's submit).
    // ---------------------------------------------------------------------

    /// `len ≤ slab`; the payload (writes) is copied into the slab NOW — the
    /// caller's buffer is free when this returns. Null = no slot / poisoned
    /// (the caller serves the op on the real fd).
    pub fn submitNoWait(self: *Session, op: layout.Op, binding: u64, offset: u64, len: u32, src: ?[]const u8) ?Ticket {
        if (len > self.layout.slab or self.poisoned()) return null;
        const c = self.claimSlot() orelse return null;
        if (src) |s| {
            const window = self.m.slotWindow(c.index) catch unreachable;
            @memcpy(window[0..len], s[0..len]);
        }
        if (!self.publish(c.index, op, binding, offset, len)) return null;
        self.ringDoorbell();
        return .{ .slot = c.index, .generation = c.generation };
    }

    /// Non-consuming probe.
    pub fn ticketDone(self: *Session, t: Ticket) bool {
        return slot_core.isDoneFor(&self.m.slots()[t.slot], t.generation);
    }

    /// Consumes on DONE: `res > 0` copies `min(res, out.len)` bytes out
    /// (reads), releases the slot, returns `res` (bytes or `-errno`
    /// VERBATIM — post-acceptance there is no fallthrough: even EINVAL
    /// surfaces). Null = in flight.
    pub fn pollTicket(self: *Session, t: Ticket, out: ?[]u8) ?i64 {
        const slot = &self.m.slots()[t.slot];
        if (!slot_core.isDoneFor(slot, t.generation)) return null;
        const r = slot_core.result(slot);
        if (r > 0) {
            if (out) |o| {
                const n: usize = @min(@as(usize, @intCast(r)), o.len);
                const window = self.m.slotWindow(t.slot) catch unreachable;
                @memcpy(o[0..n], window[0..n]);
            }
        }
        slot_core.release(slot);
        return r;
    }

    /// Mark a ticket abandoned (deadline): its slot is never released.
    pub fn abandonTicket(self: *Session, _: Ticket) void {
        _ = self.il_abandoned_slots.fetchAdd(1, .monotonic);
    }

    // ---------------------------------------------------------------------
    // Arena-native tickets (the SDK's leased buffers, `il_sdk.md` §2.5):
    // the descriptor names an APP-CHOSEN arena window — no slab copy in
    // either direction; the daemon validates `arena_off + len` against its
    // own geometry exactly as it does a slab window. NO doorbell here: a
    // batch rings once through `kickDoorbell`.
    // ---------------------------------------------------------------------

    /// Claim + publish + ring push of an arena-native op (`len ≤
    /// max_op_bytes`, the window inside the arena — the caller's lease law
    /// guarantees both). Null = no slot / poisoned; a ring-push failure
    /// poisons the session (the impossible observation) and answers null.
    pub fn submitArena(self: *Session, op: layout.Op, binding: u64, offset: u64, len: u32, arena_off: u64, flags: u32) ?Ticket {
        if (self.poisoned()) return null;
        const c = self.claimSlot() orelse return null;
        const slot = &self.m.slots()[c.index];
        slot_core.publishDescriptor(slot, .{ .op = op, .flags = flags, .binding = binding, .offset = offset, .len = len, .arena_off = arena_off });
        slot_core.stampIngress(slot, clock.monotonicNs());
        slot_core.publishSubmitted(slot);
        if (!self.ring.push(c.index)) {
            _ = self.il_ring_full_impossible.fetchAdd(1, .monotonic);
            self.poison();
            return null;
        }
        return .{ .slot = c.index, .generation = c.generation };
    }

    /// The ONE doorbell of a batch (SeqCst bump, wake iff the daemon parks).
    pub fn kickDoorbell(self: *Session) void {
        self.ringDoorbell();
    }

    pub const TicketWait = enum { done, abandoned };

    /// The one-op wait on a ticket: the adaptive spin, then the 50 ms park
    /// slices re-checking poison, the heartbeat ladder and the deadline.
    /// `abandoned` leaves the slot as it is — never recycled — and the
    /// session poisoned.
    pub fn waitTicket(self: *Session, t: Ticket) TicketWait {
        return switch (self.waitDone(t.slot, t.generation)) {
            .done => .done,
            .abandoned => .abandoned,
        };
    }

    /// The result of a DONE ticket (bytes or `-errno` verbatim) WITHOUT
    /// releasing its slot — a chunked op folds its flights first and
    /// releases them together (`releaseTicket`). Null = in flight.
    pub fn ticketResult(self: *Session, t: Ticket) ?i64 {
        const slot = &self.m.slots()[t.slot];
        if (!slot_core.isDoneFor(slot, t.generation)) return null;
        return slot_core.result(slot);
    }

    /// DONE → FREE for a ticket whose result was read by `ticketResult`.
    pub fn releaseTicket(self: *Session, t: Ticket) void {
        slot_core.release(&self.m.slots()[t.slot]);
    }

    /// The session's granted geometry.
    pub fn geometry(self: *const Session) layout.Geometry {
        return self.layout.geometry;
    }

    /// The arena (the SDK's buddy allocator covers all of it).
    pub fn arena(self: *Session) []u8 {
        return self.m.arena();
    }

    // ---------------------------------------------------------------------
    // The completion-doorbell park (register → snapshot → re-scan).
    // ---------------------------------------------------------------------

    /// Register on this session's completion doorbell with the batch mark
    /// `k` (`doorbell.reapBatchWakeThreshold(pending)` for the deep regime,
    /// 1 for the sparse one); returns the futex `expected`. The caller MUST
    /// re-scan its tickets before `waitAny`, and `cqeParkEnd` on every exit.
    pub fn cqeParkBeginBatch(self: *Session, k: u32) u32 {
        _ = self.statsPage().il_park_eras.fetchAdd(1, .monotonic);
        return doorbell.parkBeginBatch(&self.header().cqe, k);
    }

    pub fn cqeParkEnd(self: *Session) void {
        doorbell.parkEnd(&self.header().cqe);
    }

    pub const WaitVerdict = enum { returned, timed_out };

    /// Park on the completion doorbells of up to `WAIT_ANY_MAX` sessions
    /// against their snapshots, bounded. A userspace admission pre-check
    /// first (any word already moved ⇒ return — at qd32 saturation 42 % of
    /// waits failed kernel admission); one entry ⇒ `FUTEX_WAIT`; many ⇒
    /// `futex_waitv`.
    pub fn waitAny(entries: []const WaitEntry, timeout_ns: u64) WaitVerdict {
        std.debug.assert(entries.len <= WAIT_ANY_MAX);
        for (entries) |e| if (e.session.header().cqe.seq.load(.seq_cst) != e.expected) return .returned;
        if (entries.len == 1) {
            return switch (futex.waitSharedTimeout(&entries[0].session.header().cqe.seq, entries[0].expected, timeout_ns)) {
                .returned => .returned,
                .timed_out => .timed_out,
            };
        }
        var ws: [WAIT_ANY_MAX]futex.Waiter = undefined;
        for (entries, 0..) |e, i| ws[i] = .{ .word = &e.session.header().cqe.seq, .expect = e.expected };
        return switch (futex.waitMany(ws[0..entries.len], timeout_ns)) {
            .timed_out => .timed_out,
            // ENOSYS was refused at establish; a late one reads as a return
            // (the caller re-scans, bounded by its own deadline).
            .returned, .unsupported => .returned,
        };
    }
};

// ---------------------------------------------------------------------------
// Tests: the full ladder end to end over a real SEQPACKET pair, SCM_RIGHTS
// and a real sealed memfd, against an in-process daemon-side host stub that
// admits one session and serves its ring.
// ---------------------------------------------------------------------------

const testing = std.testing;
const ipc = @import("ipc.zig");

fn sleepNs(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

/// The in-process daemon stub: one abstract listener, one accepted session,
/// a service thread running the drain → serve → complete → park loop with
/// the heartbeat stamped every pass. READ fills the window with
/// `offset + i` bytes — or, with `store` set, the bytes a WRITE of the same
/// binding left at that offset (a byte-exact round trip for the SDK's
/// arena-native ops); WRITE answers the byte sum (or stores); a 0-byte READ
/// is the liveness ping. BIND echoes the requested durability word. `pub`
/// for the SDK's own tests and its C smoke (no daemon needed).
pub const StubHost = struct {
    const Self = @This();

    name_buf: [rendezvous.SOCKET_NAME_MAX]u8 = undefined,
    name_len: usize = 0,
    listener: linux.fd_t = -1,
    policy: rendezvous.Policy,
    nonces: rendezvous.NonceState,
    mount_dev: ?u64,
    thread: ?std.Thread = null,
    // ---- knobs the tests turn ----
    /// Serve with the mark-valued latch (the shipped arm).
    latch: bool = true,
    /// Stop stamping the heartbeat and serving (a frozen daemon).
    frozen: Atomic(bool) = .init(false),
    /// Sever the ctl socket (a dead daemon).
    sever: Atomic(bool) = .init(false),
    stop: Atomic(bool) = .init(false),
    /// Randomised serve delay (ns) for the timing stress.
    jitter_ns: u64 = 0,
    /// A flat file image: WRITE stores into it, READ serves from it (a
    /// read past its end is short; a write past it is `-EFBIG`). Null =
    /// the pattern/ack arms.
    store: ?[]u8 = null,
    // ---- outcomes ----
    accepted: ?rl.Accepted = null,
    refused: ?RefuseClass = null,
    ctl: linux.fd_t = -1,
    served: Atomic(u64) = .init(0),
    binds: Atomic(u64) = .init(0),
    /// UNBIND frames received (the shim's refcount law: one per binding).
    unbinds: Atomic(u64) = .init(0),
    next_binding: u64 = 1,
    /// The last BIND's rights (the per-op EBADF arm reads them).
    rights: Rights = .{ .read = true, .write = true },

    pub fn init(self: *Self, policy: rendezvous.Policy, mount_dev: ?u64) !void {
        self.* = .{ .policy = policy, .nonces = rendezvous.NonceState.init(&rl.fillNonce, clock.monotonicNs()), .mount_dev = mount_dev };
        var prng = std.Random.DefaultPrng.init(clock.monotonicNs());
        const name = rendezvous.socketName(&self.name_buf, @intCast(linux.getpid()), prng.random().int(u32));
        self.name_len = name.len;
        self.listener = try rl.listenAbstract(name, sizing.listenBacklog(64));
    }

    pub fn blob(self: *Self, commit: []const u8) !wire.Il0Blob {
        return rendezvous.buildBlob(.{ .build_commit = commit, .socket_name = self.name_buf[0..self.name_len] }, &self.nonces.current);
    }

    /// Wait (bounded) for a connection so a client that fails before it
    /// connects leaves a joinable stub, never a hung test.
    pub fn awaitConnection(self: *Self) !void {
        var pfd = [_]linux.pollfd{.{ .fd = self.listener, .events = linux.POLL.IN, .revents = 0 }};
        const rc = linux.poll(&pfd, 1, 5000);
        if (linux.errno(rc) != .SUCCESS or rc == 0) return error.NoClient;
    }

    /// Accept ONE connection and run its HELLO; on admission, serve on a
    /// thread until stopped.
    pub fn acceptOne(self: *Self) !void {
        try self.awaitConnection();
        self.ctl = try rl.accept(self.listener);
        const t0 = clock.monotonicNs();
        switch (rl.acceptHello(self.ctl, self.policy, &self.nonces, self.mount_dev, false)) {
            .accepted => |a| {
                self.accepted = a;
                ipc.stats.ipc_sessions_total.inc();
                ipc.stats.ipc_session_admission_ns.record(clock.monotonicNs() - t0);
                self.thread = try std.Thread.spawn(.{}, serveLoop, .{self});
            },
            .refused => |c| self.refused = c,
            .dropped => return error.Dropped,
        }
    }

    pub fn deinit(self: *Self) void {
        self.stop.store(true, .release);
        if (self.thread) |t| t.join();
        if (self.accepted) |a| shm.unmap(a.mapped);
        if (self.ctl >= 0) _ = linux.close(self.ctl);
        _ = linux.close(self.listener);
    }

    fn serveLoop(self: *Self) void {
        const a = self.accepted.?;
        const m = a.mapped.mapping;
        const ring = ring_core.View.fromMapping(m);
        var consumer: ring_core.Consumer = .{};
        var park: doorbell.ServicePark = .{};
        var owned = [_]*layout.SessionHeader{m.header()};
        var prng = std.Random.DefaultPrng.init(0x5e4e);
        while (!self.stop.load(.acquire)) {
            if (self.sever.swap(false, .acq_rel)) _ = linux.shutdown(self.ctl, linux.SHUT.RDWR);
            if (self.frozen.load(.acquire)) {
                sleepNs(std.time.ns_per_ms);
                continue;
            }
            m.header().heartbeat.store(clock.monotonicNs(), .release);
            self.pollCtl();
            var served: u32 = 0;
            while (consumer.pop(ring)) |idx| {
                if (self.jitter_ns != 0) {
                    const d = prng.random().uintLessThan(u64, self.jitter_ns);
                    if (d > 2000) sleepNs(d);
                }
                self.serveOne(m, idx);
                served += 1;
            }
            if (served == 0) {
                park.arm(&owned);
                // The rescan.
                while (consumer.pop(ring)) |idx| {
                    self.serveOne(m, idx);
                    served += 1;
                }
                if (served == 0) _ = park.wait();
                park.disarm();
                m.header().heartbeat.store(clock.monotonicNs(), .release);
            }
        }
    }

    fn serveOne(self: *Self, m: layout.Mapping, idx: u32) void {
        const slot = m.slot(idx) catch return;
        if (!slot_core.tryBeginServe(slot)) return;
        const d = slot_core.snapshot(slot);
        var result: i64 = undefined;
        if (d.len > m.layout.geometry.max_op_bytes) {
            result = -@as(i64, @backingInt(linux.E.INVAL));
        } else if (m.arenaWindow(d.arena_off, d.len)) |window| {
            switch (d.op) {
                @backingInt(layout.Op.read) => if (!self.rights.read) {
                    result = -@as(i64, @backingInt(linux.E.BADF));
                } else if (self.store) |st| {
                    const start: usize = @intCast(@min(d.offset, st.len));
                    const n: usize = @min(window.len, st.len - start);
                    @memcpy(window[0..n], st[start..][0..n]);
                    result = @intCast(n);
                } else {
                    for (window, 0..) |*b, i| b.* = @truncate(d.offset + i);
                    result = @intCast(d.len);
                },
                @backingInt(layout.Op.write) => if (!self.rights.write) {
                    result = -@as(i64, @backingInt(linux.E.BADF));
                } else if (self.store) |st| {
                    if (d.offset + d.len > st.len) {
                        result = -@as(i64, @backingInt(linux.E.FBIG));
                    } else {
                        @memcpy(st[@intCast(d.offset)..][0..window.len], window);
                        result = @intCast(d.len);
                    }
                } else {
                    var sum: u64 = 0;
                    for (window) |b| sum += b;
                    result = @intCast(d.len);
                    std.mem.doNotOptimizeAway(sum);
                },
                else => result = -@as(i64, @backingInt(linux.E.INVAL)),
            }
        } else |_| {
            result = -@as(i64, @backingInt(linux.E.INVAL));
        }
        // The account BEFORE the completion: the DONE swap's Release half
        // publishes it, so a client that acquires the completion reads a
        // `served` that includes it — the engagement law (`served == ops`)
        // is asserted on a completion's heels. Counted after the wake, the
        // woken reader beat this thread to the read under load (7/300).
        _ = self.served.fetchAdd(1, .monotonic);
        slot_core.completeAndWake(slot, result);
        _ = doorbell.completeAndWake(&m.header().cqe, self.latch);
    }

    /// BIND/UNBIND on the ctl socket, non-blocking.
    fn pollCtl(self: *Self) void {
        var buf: [256]u8 = undefined;
        var iov = [_]std.posix.iovec{.{ .base = &buf, .len = buf.len }};
        var control: [64]u8 align(8) = undefined;
        var msg: linux.msghdr = .{ .name = null, .namelen = 0, .iov = &iov, .iovlen = 1, .control = &control, .controllen = control.len, .flags = 0 };
        const rc = linux.recvmsg(self.ctl, &msg, linux.MSG.DONTWAIT | linux.MSG.CMSG_CLOEXEC);
        if (linux.errno(rc) != .SUCCESS or rc == 0) return;
        // Own the fd if one rode along.
        var fd: ?linux.fd_t = null;
        if (msg.controllen >= @sizeOf(linux.cmsghdr)) {
            const hdr: *const linux.cmsghdr = @ptrCast(@alignCast(&control));
            if (hdr.type == linux.SCM.RIGHTS) {
                var f: linux.fd_t = undefined;
                @memcpy(std.mem.asBytes(&f), control[std.mem.alignForward(usize, @sizeOf(linux.cmsghdr), 8)..][0..4]);
                fd = f;
            }
        }
        defer if (fd) |f| {
            _ = linux.close(f);
        };
        const frame = buf[0..rc];
        switch (wire.tagOf(frame) catch return) {
            .bind => {
                const f = fd orelse return;
                const req = wire.decodeFixed(wire.Bind, frame) catch return;
                const screened = rendezvous.screenFd(rl.fdFacts(f), self.mount_dev, false);
                switch (screened) {
                    .ok => |ok| {
                        self.rights = ok.rights;
                        // The durability word travels back as granted (the
                        // product daemon's barrier-before-DONE is the host's).
                        const reply: wire.BindOk = .{ .rights = ok.rights, .binding_id = self.next_binding, .ino = ok.ino, .durability_granted = req.durability };
                        self.next_binding += 1;
                        _ = self.binds.fetchAdd(1, .monotonic);
                        rl.sendCtl(self.ctl, wire.frameBytes(&reply), null) catch {};
                    },
                    .refuse => |c| {
                        const reply: wire.BindRefused = .{ .class = @backingInt(c) };
                        rl.sendCtl(self.ctl, wire.frameBytes(&reply), null) catch {};
                    },
                }
            },
            .unbind => _ = self.unbinds.fetchAdd(1, .monotonic),
            else => {},
        }
    }
};

const Fixture = struct {
    host: StubHost,
    dir: rl.PathListener = undefined,
    scratch_buf: [64]u8 = undefined,
    scratch_len: usize = 0,
    file_fd: linux.fd_t = -1,
    env: std.process.Environ.Map,
    commit: []const u8 = "0123456789ab",
    /// The daemon's REFUSE of the last `establish`.
    refusal: ?Refusal = null,

    /// 64 slots of 16 pages under a 4-page max_op: the slab is the max_op
    /// window (one slot = one flight).
    const default_geometry: layout.Geometry = .{ .ring_entries = 64, .slots = 64, .arena_bytes = 64 * 16 * layout.PAGE_BYTES, .max_op_bytes = 4 * layout.PAGE_BYTES };

    fn init(self: *Fixture) !void {
        return self.initGeometry(default_geometry);
    }

    fn initGeometry(self: *Fixture, geometry: layout.Geometry) !void {
        self.* = .{ .host = undefined, .env = std.process.Environ.Map.init(testing.allocator) };
        errdefer self.env.deinit();
        // A regular file on a local filesystem as the credential + the
        // bound file; its device is "the mount".
        const p = try std.fmt.bufPrintSentinel(&self.scratch_buf, "/tmp/sqz-il-client-{d}", .{linux.getpid()}, 0);
        self.scratch_len = p.len;
        const rc = linux.openat(linux.AT.FDCWD, p, .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        self.file_fd = @intCast(rc);
        const dev = rl.deviceOf(self.file_fd).?;
        try self.host.init(.{ .armed = true, .our_commit = self.commit, .arena_cap_bytes = 1 << 30, .per_uid_session_cap = 64, .default_geometry = geometry }, dev);
    }

    fn deinit(self: *Fixture) void {
        self.host.deinit();
        if (self.file_fd >= 0) _ = linux.close(self.file_fd);
        _ = linux.unlinkat(linux.AT.FDCWD, self.scratch_buf[0..self.scratch_len :0], 0);
        self.env.deinit();
    }

    fn opts(self: *Fixture) Options {
        return .{ .build_commit = self.commit, .mount_uid = linux.geteuid(), .env = &self.env };
    }

    /// Establish a session: the client runs the ladder on this thread while
    /// the host accepts on another.
    fn establish(self: *Fixture, cred_fd: linux.fd_t, o: Options) !Session {
        const Acceptor = struct {
            fn run(h: *StubHost) void {
                h.acceptOne() catch {};
            }
        };
        const t = try std.Thread.spawn(.{}, Acceptor.run, .{&self.host});
        defer t.join();
        const b = self.host.blob(self.commit) catch return error.Protocol;
        return Session.establish(testing.allocator, &b, cred_fd, o, &self.refusal);
    }
};

test "e2e: the HELLO ladder admits a shim over a real SEQPACKET socket, SCM_RIGHTS and a sealed memfd; bind serves reads (pattern) and writes (ack); a 0-byte read is the liveness ping; the lane gate is published" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    try testing.expect(fx.host.accepted != null);
    try testing.expectEqual(@as(u64, 4 * layout.PAGE_BYTES), s.layout.slab);
    try testing.expectEqual(s.lane_gate_min, s.statsPage().lane_gate_threshold_bytes.load(.monotonic));
    try testing.expect(s.lane_gate_min >= s.layout.slab and s.lane_gate_min <= s.layout.geometry.max_op_bytes);
    const b = try s.bind(fx.file_fd, .mount);
    try testing.expect(b.rights.read and b.rights.write);
    try testing.expectEqual(@as(u64, 1), b.id);
    var out: [4096]u8 = undefined;
    switch (s.pread(b.id, &out, 4096)) {
        .served => |n| try testing.expectEqual(@as(usize, 4096), n),
        else => return error.TestNotServed,
    }
    for (out, 0..) |v, i| try testing.expectEqual(@as(u8, @truncate(4096 + i)), v);
    switch (s.pread(b.id, out[0..0], 0)) {
        .served => |n| try testing.expectEqual(@as(usize, 0), n),
        else => return error.TestNotServed,
    }
    switch (s.pwrite(b.id, &out, 0)) {
        .served => |n| try testing.expectEqual(@as(usize, 4096), n),
        else => return error.TestNotServed,
    }
    // Every slot went back to FREE.
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    s.unbind(b.id);
    try testing.expect(!s.poisoned());
}

test "e2e: a large op pipelines flights over slot runs under one doorbell per batch, reaps in offset order and releases every slot; a capped run still serves via smaller flights" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    const b = try s.bind(fx.file_fd, .mount);
    // 40 pages: 10 flights of the 4-page max_op window.
    const big = try testing.allocator.alloc(u8, 40 * 4096 + 77);
    defer testing.allocator.free(big);
    switch (s.pread(b.id, big, 1000)) {
        .served => |n| try testing.expectEqual(big.len, n),
        else => return error.TestNotServed,
    }
    for (big, 0..) |v, i| try testing.expectEqual(@as(u8, @truncate(1000 + i)), v);
    switch (s.pwrite(b.id, big, 0)) {
        .served => |n| try testing.expectEqual(big.len, n),
        else => return error.TestNotServed,
    }
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    // Fragmentation: cap runs at 1 slot ⇒ 41 single-slab flights, same bytes.
    s.max_run_slots = 1;
    @memset(big, 0);
    switch (s.pread(b.id, big, 5)) {
        .served => |n| try testing.expectEqual(big.len, n),
        else => return error.TestNotServed,
    }
    for (big, 0..) |v, i| try testing.expectEqual(@as(u8, @truncate(5 + i)), v);
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
}

test "e2e: a slab that does not divide max_op (64 slots of 3 pages under a 4-page ceiling — the shape of an arena that yielded to the population floor): every flight is capped at max_op, never rounded up to the run's slab multiple, and the stub's per-op screen rejects nothing" {
    var fx: Fixture = undefined;
    try fx.initGeometry(.{ .ring_entries = 64, .slots = 64, .arena_bytes = 64 * 3 * layout.PAGE_BYTES, .max_op_bytes = 4 * layout.PAGE_BYTES });
    defer fx.deinit();
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    try testing.expectEqual(@as(u64, 3 * layout.PAGE_BYTES), s.layout.slab);
    const b = try s.bind(fx.file_fd, .mount);
    // 40 pages + a tail: ten 4-page flights (two slots each, the second
    // slot one page used) and a short one.
    const big = try testing.allocator.alloc(u8, 40 * layout.PAGE_BYTES + 77);
    defer testing.allocator.free(big);
    const served_before = fx.host.served.load(.monotonic);
    switch (s.pread(b.id, big, 1000)) {
        .served => |n| try testing.expectEqual(big.len, n),
        else => return error.TestNotServed,
    }
    for (big, 0..) |v, i| try testing.expectEqual(@as(u8, @truncate(1000 + i)), v);
    try testing.expectEqual(served_before + 11, fx.host.served.load(.monotonic));
    switch (s.pwrite(b.id, big, 0)) {
        .served => |n| try testing.expectEqual(big.len, n),
        else => return error.TestNotServed,
    }
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    try testing.expect(!s.poisoned());
}

test "e2e: the daemon's screen at BIND — a wrong-direction op completes EBADF kernel-identical; an O_PATH fd, an O_APPEND fd, a directory and a foreign-device memfd each refuse with their class" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    const ro: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, fx.scratch_buf[0..fx.scratch_len :0], .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0));
    defer _ = linux.close(ro);
    const b = try s.bind(ro, .mount);
    try testing.expect(b.rights.read and !b.rights.write);
    var buf: [16]u8 = undefined;
    switch (s.pwrite(b.id, &buf, 0)) {
        .errno => |e| try testing.expectEqual(linux.E.BADF, e),
        else => return error.TestExpectedEbadf,
    }
    const op: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, fx.scratch_buf[0..fx.scratch_len :0], .{ .ACCMODE = .RDONLY, .PATH = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(op);
    try testing.expectError(error.Refused, s.bind(op, .mount));
    try testing.expectEqual(RefuseClass.flags, s.last_refusal.?);
    const ap: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, fx.scratch_buf[0..fx.scratch_len :0], .{ .ACCMODE = .WRONLY, .APPEND = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(ap);
    try testing.expectError(error.Refused, s.bind(ap, .mount));
    try testing.expectEqual(RefuseClass.flags, s.last_refusal.?);
    const dfd: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/tmp", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(dfd);
    try testing.expectError(error.Refused, s.bind(dfd, .mount));
    try testing.expectEqual(RefuseClass.flags, s.last_refusal.?);
    const mrc = linux.memfd_create("sqz-foreign", linux.MFD.CLOEXEC);
    const mfd: linux.fd_t = @intCast(mrc);
    defer _ = linux.close(mfd);
    try testing.expectError(error.Refused, s.bind(mfd, .mount));
    try testing.expectEqual(RefuseClass.mount, s.last_refusal.?);
}

fn expectHelloRefusal(fx: *Fixture, cred_fd: linux.fd_t, o: Options, class: RefuseClass) !void {
    const before = ipc.refusalCount(class);
    fx.refusal = null;
    try testing.expectError(error.Refused, fx.establish(cred_fd, o));
    try testing.expectEqual(class, fx.host.refused.?);
    try testing.expectEqual(class, fx.refusal.?.class);
    try testing.expectEqual(before + 1, ipc.refusalCount(class));
    // The host stub holds ONE connection at a time; reopen for the next row.
    _ = linux.close(fx.host.ctl);
    fx.host.ctl = -1;
    fx.host.refused = null;
}

test "e2e: every HELLO refusal — a stale nonce, a wrong commit, a -dirty commit, an O_PATH credential, an O_APPEND credential, a non-regular credential, a foreign st_dev, a full budget — each counted on its class" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    const o = fx.opts();
    // Stale nonce: rotate twice behind the client's back.
    var stale_blob = try fx.host.blob(fx.commit);
    _ = fx.host.nonces.rotateIfStale(&rl.fillNonce, clock.monotonicNs() + 2 * rendezvous.NONCE_TTL_NS);
    _ = fx.host.nonces.rotateIfStale(&rl.fillNonce, clock.monotonicNs() + 4 * rendezvous.NONCE_TTL_NS);
    {
        const Acceptor = struct {
            fn run(h: *StubHost) void {
                h.acceptOne() catch {};
            }
        };
        const t = try std.Thread.spawn(.{}, Acceptor.run, .{&fx.host});
        const before = ipc.refusalCount(.nonce);
        try testing.expectError(error.Refused, Session.establish(testing.allocator, &stale_blob, fx.file_fd, o, &fx.refusal));
        t.join();
        try testing.expectEqual(RefuseClass.nonce, fx.refusal.?.class);
        try testing.expectEqual(RefuseClass.nonce, fx.host.refused.?);
        try testing.expectEqual(before + 1, ipc.refusalCount(.nonce));
        _ = linux.close(fx.host.ctl);
        fx.host.ctl = -1;
        fx.host.refused = null;
        // A fresh blob carries the current nonce again.
        stale_blob = try fx.host.blob(fx.commit);
    }
    // Wrong commit: the client's pre-check refuses without a round trip…
    var wrong = o;
    wrong.build_commit = "0123456789ac";
    try testing.expectError(error.VersionSkew, Session.establish(testing.allocator, &stale_blob, fx.file_fd, wrong, &fx.refusal));
    // …and the daemon refuses it authoritatively when the pre-check is bypassed (a dirty daemon vs a clean shim).
    fx.host.policy.our_commit = "0123456789ab-dirty";
    try expectHelloRefusal(&fx, fx.file_fd, o, .version);
    // -dirty on both sides refuses too, unless the seam admits it.
    var dirty = o;
    dirty.build_commit = "0123456789ab-dirty";
    fx.commit = "0123456789ab-dirty";
    try testing.expectError(error.VersionSkew, Session.establish(testing.allocator, &(try fx.host.blob(fx.commit)), fx.file_fd, dirty, &fx.refusal));
    try fx.env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
    try expectHelloRefusal(&fx, fx.file_fd, dirty, .version); // the daemon's side still refuses: its policy has no seam
    fx.host.policy.allow_dirty = true;
    const before_dev = ipc.stats.ipc_binds_dev_override.load();
    {
        var s = try fx.establish(fx.file_fd, dirty);
        s.teardown();
        try testing.expectEqual(before_dev + 1, ipc.stats.ipc_binds_dev_override.load());
        fx.host.stop.store(true, .release);
        if (fx.host.thread) |t| t.join();
        fx.host.thread = null;
        fx.host.stop.store(false, .release);
        shm.unmap(fx.host.accepted.?.mapped);
        fx.host.accepted = null;
        _ = linux.close(fx.host.ctl);
        fx.host.ctl = -1;
    }
    fx.host.policy.allow_dirty = false;
    fx.host.policy.our_commit = "0123456789ab";
    fx.commit = "0123456789ab";
    _ = fx.env.swapRemove("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY");
    // The credential screen's rows.
    const op: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, fx.scratch_buf[0..fx.scratch_len :0], .{ .ACCMODE = .RDONLY, .PATH = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(op);
    try expectHelloRefusal(&fx, op, o, .flags);
    const ap: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, fx.scratch_buf[0..fx.scratch_len :0], .{ .ACCMODE = .WRONLY, .APPEND = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(ap);
    try expectHelloRefusal(&fx, ap, o, .flags);
    const dfd: linux.fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/tmp", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0));
    defer _ = linux.close(dfd);
    try expectHelloRefusal(&fx, dfd, o, .flags);
    const mfd: linux.fd_t = @intCast(linux.memfd_create("sqz-foreign", linux.MFD.CLOEXEC));
    defer _ = linux.close(mfd);
    try expectHelloRefusal(&fx, mfd, o, .mount);
    // Budget full.
    fx.host.policy.arena_cap_bytes = 4096;
    try expectHelloRefusal(&fx, fx.file_fd, o, .budget);
}

test "e2e: the client refuses the daemon — an unsealed memfd (rung 3), a world-writable path rendezvous (rung 1, no connect), a foreign peer uid (rung 2, no HELLO)" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    // Rung 3: a stub that answers SESSION_OK with an UNSEALED memfd.
    const Unsealing = struct {
        fn run(h: *StubHost) void {
            h.awaitConnection() catch return;
            h.ctl = rl.accept(h.listener) catch return;
            var buf: [wire.CTL_MSG_MAX]u8 = undefined;
            const r = rl.recvCtl(h.ctl, &buf) catch return;
            if (r.fd) |f| _ = linux.close(f);
            const rc = linux.memfd_create("sqz-unsealed", linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
            const m: linux.fd_t = @intCast(rc);
            defer _ = linux.close(m);
            _ = linux.ftruncate(m, 1 << 20);
            const ok: wire.SessionOk = .{};
            rl.sendCtl(h.ctl, wire.frameBytes(&ok), m) catch {};
        }
    };
    {
        const t = try std.Thread.spawn(.{}, Unsealing.run, .{&fx.host});
        const b = try fx.host.blob(fx.commit);
        try testing.expectError(error.Unsealed, Session.establish(testing.allocator, &b, fx.file_fd, fx.opts(), &fx.refusal));
        t.join();
        _ = linux.close(fx.host.ctl);
        fx.host.ctl = -1;
    }
    // Rung 1: a blob whose abstract name is dead and whose path socket sits
    // in a world-writable directory: refused BEFORE any connect.
    {
        var wz: [64]u8 = undefined;
        const ww = try std.fmt.bufPrintSentinel(&wz, "/tmp/sqz-il-ww-{d}", .{linux.getpid()}, 0);
        _ = linux.mkdirat(linux.AT.FDCWD, ww, 0o777);
        _ = linux.fchmodat(linux.AT.FDCWD, ww, 0o777);
        defer _ = linux.unlinkat(linux.AT.FDCWD, ww, linux.AT.REMOVEDIR);
        var pbuf: [96]u8 = undefined;
        const sock_path = try std.fmt.bufPrint(&pbuf, "{s}/x.sock", .{ww});
        const b = try rendezvous.buildBlob(.{ .build_commit = fx.commit, .socket_name = "sqz-il0-dead-00000000", .socket_path = sock_path }, &fx.host.nonces.current);
        try testing.expectError(error.UntrustedRendezvous, Session.establish(testing.allocator, &b, fx.file_fd, fx.opts(), &fx.refusal));
    }
    // Rung 2: a mount owned by a uid that is neither root nor us; the peer
    // (us) is refused before HELLO. Root is trusted for every owner, so the
    // row is observable only as a non-root tester.
    if (linux.geteuid() != 0) {
        const Silent = struct {
            fn run(h: *StubHost) void {
                h.awaitConnection() catch return;
                h.ctl = rl.accept(h.listener) catch return;
                var buf: [64]u8 = undefined;
                _ = rl.recvCtl(h.ctl, &buf) catch {};
            }
        };
        const t = try std.Thread.spawn(.{}, Silent.run, .{&fx.host});
        var o = fx.opts();
        o.mount_uid = linux.geteuid() + 1;
        const b = try fx.host.blob(fx.commit);
        try testing.expectError(error.PeerUntrusted, Session.establish(testing.allocator, &b, fx.file_fd, o, &fx.refusal));
        _ = linux.close(fx.host.listener);
        fx.host.listener = try rl.listenAbstract(fx.host.name_buf[0..fx.host.name_len], 64);
        t.join();
        _ = linux.close(fx.host.ctl);
        fx.host.ctl = -1;
    }
}

test "e2e: no-wait tickets + the completion-doorbell park over 100 k ops with randomised server timing — nothing lost, ≤ 1 wake paid per park era, every slot back to FREE" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    fx.host.jitter_ns = 20_000;
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    const b = try s.bind(fx.file_fd, .mount);
    const total: u32 = 100_000;
    const depth: u32 = 32;
    var tickets: [depth]?Ticket = @splat(null);
    var submitted: u32 = 0;
    var completed: u32 = 0;
    var prng = std.Random.DefaultPrng.init(0x100c);
    var eras_over_one: u32 = 0;
    var eras: u32 = 0;
    var out: [512]u8 = undefined;
    while (completed < total) {
        // Keep the window full.
        for (&tickets) |*t| {
            if (t.* == null and submitted < total) {
                const len: u32 = prng.random().uintLessThan(u32, 512) + 1;
                t.* = s.submitNoWait(.read, b.id, @as(u64, submitted) * 7, len, null) orelse break;
                submitted += 1;
            }
        }
        // Reap what is done.
        var reaped = false;
        var pending: u32 = 0;
        for (&tickets) |*t| {
            const tk = t.* orelse continue;
            if (s.pollTicket(tk, &out)) |r| {
                try testing.expect(r >= 0);
                completed += 1;
                reaped = true;
                t.* = null;
            } else pending += 1;
        }
        if (reaped or pending == 0) continue;
        // Park: register → snapshot → re-scan → wait, bounded.
        const k = doorbell.reapBatchWakeThreshold(pending);
        const writes_before = ipc.stats.ipc_cqe_wake_writes.load();
        const snap = s.cqeParkBeginBatch(k);
        eras += 1;
        var ready = false;
        for (tickets) |t| if (t != null and s.ticketDone(t.?)) {
            ready = true;
        };
        if (!ready) {
            var entries = [_]WaitEntry{.{ .session = &s, .expected = snap }};
            _ = Session.waitAny(&entries, 50 * std.time.ns_per_ms);
        }
        s.cqeParkEnd();
        // The latch: the era paid at most one wake (a completion landing
        // after parkEnd sees parked == 0 and elides).
        if (ipc.stats.ipc_cqe_wake_writes.load() - writes_before > 1) eras_over_one += 1;
    }
    try testing.expectEqual(total, completed);
    try testing.expectEqual(@as(u32, 0), eras_over_one);
    try testing.expect(eras > 0);
    try testing.expectEqual(@as(u64, eras), s.statsPage().il_park_eras.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), s.il_ring_full_impossible.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), s.il_abandoned_slots.load(.monotonic));
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    try testing.expect(!s.poisoned());
}

test "e2e: heartbeat death detection — a daemon that stops stamping and severs its socket is detected within the 1 s ladder, far inside the 30 s deadline; the session is poisoned and every later op falls through" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    try fx.env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "20000");
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    const b = try s.bind(fx.file_fd, .mount);
    // Freeze the daemon (no serve, no heartbeat) and sever its socket on
    // its last pass, then submit: the op must poison in ≈ 1 s.
    fx.host.sever.store(true, .release);
    fx.host.frozen.store(true, .release);
    sleepNs(20 * std.time.ns_per_ms);
    var buf: [64]u8 = undefined;
    const t0 = clock.monotonicNs();
    try testing.expect(s.pread(b.id, &buf, 0) == .fallthrough);
    const elapsed = clock.monotonicNs() - t0;
    try testing.expect(elapsed >= rendezvous.HEARTBEAT_STALE_NS);
    try testing.expect(elapsed < 5 * std.time.ns_per_s);
    try testing.expect(s.poisoned());
    try testing.expectEqual(@as(u64, 1), s.il_abandoned_slots.load(.monotonic));
    try testing.expect(s.pread(b.id, &buf, 0) == .fallthrough);
    try testing.expectError(error.Poisoned, s.bind(fx.file_fd, .mount));
    // A poisoned session emits no ctl traffic.
    s.unbind(b.id);
}

test "a generation bump by the daemon poisons the client lazily; the fork child's poison closes, the parent's shuts down" {
    var fx: Fixture = undefined;
    try fx.init();
    defer fx.deinit();
    var s = try fx.establish(fx.file_fd, fx.opts());
    defer s.teardown();
    try testing.expect(!s.poisoned());
    _ = fx.host.accepted.?.mapped.mapping.header().generation.fetchAdd(1, .release);
    try testing.expect(s.poisoned());
    var buf: [8]u8 = undefined;
    try testing.expect(s.pread(1, &buf, 0) == .fallthrough);
    // A second session: poison() shuts the socket down (the fd stays
    // valid: a later close succeeds); poisonChild closes it outright.
    fx.host.stop.store(true, .release);
    if (fx.host.thread) |t| t.join();
    fx.host.thread = null;
    fx.host.stop.store(false, .release);
    shm.unmap(fx.host.accepted.?.mapped);
    fx.host.accepted = null;
    _ = linux.close(fx.host.ctl);
    fx.host.ctl = -1;
    var s2 = try fx.establish(fx.file_fd, fx.opts());
    s2.poison();
    try testing.expect(s2.poisoned());
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.fcntl(s2.ctl, linux.F.GETFD, 0)));
    s2.teardown();
}
