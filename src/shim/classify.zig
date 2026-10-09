//! Classification on `open` (spec `docs/porting/il_shim.md` §2, §4): the
//! 8-step ladder from an fd to a bound cell, the session registry, the
//! refusal table with its once-per-(mount, reason) line, the negative
//! device cache with the S1 backoff, and the shim's side of the engagement
//! ledger (every passthrough attributed to its §4 row).
//!
//! The ladder: sweep a stale entry on the number → `fstat` → the negative
//! cache → the MAP_SHARED inode set → the shard → the registry (miss:
//! `fstatfs` must say FUSE, `fgetxattr(user.squeezefs.il0)` must decode, the
//! credential must pass the screen, `Session.establish` runs the three
//! authentication rungs) → `F_GETFL` + the screen → BIND → the mirror seed.
//! Every exit is a passthrough with a reason; an eligible mount whose
//! establish was refused is cached per `st_dev` (permanent for the verdicts
//! that cannot change while the mount lives; a backoff from the nonce TTL
//! to 15 min for the transient ones) instead of paying the control-plane
//! round trip on every open (decision 0002 ledger S1).
//!
//! The ladder's probes (`fstat`, `fstatfs`, `F_GETFL`, the xattr read, the
//! seed) are a table of function pointers so the unit tests inject answers
//! for every rung without a mount.

const std = @import("std");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const sys = @import("sys.zig");
const fd_table = @import("fd_table.zig");
const lifecycle = @import("lifecycle.zig");
const client = @import("../ipc/client.zig");
const wire = @import("../ipc/wire.zig");
const layout = @import("../ipc/layout.zig");
const sizing = @import("../ipc/sizing.zig");
const rendezvous = @import("../ipc/rendezvous.zig");
const clock = @import("../core/clock.zig");

pub const Session = client.Session;
pub const Table = fd_table.Table;

/// The fixed static session table the alloc-free interposer needs; the
/// per-mount ceiling (`SQUEEZEFS_IL_SESSIONS` ≤ 16) is half of it — two
/// concurrently bound mounts' headroom.
pub const REGISTRY_SLOTS: u32 = 32;
/// The once-set's capacity: past it the set stops REMEMBERING, never stops
/// printing (a flood of distinct reasons is itself the diagnostic).
pub const REFUSAL_ONCE_CAP: usize = 128;
/// The S1 backoff for transient refusals: from the nonce TTL (a refused
/// nonce is current again after one rotation) to 15 min.
pub const REFUSAL_BACKOFF_FLOOR_NS: u64 = rendezvous.NONCE_TTL_NS;
pub const REFUSAL_BACKOFF_CAP_NS: u64 = 15 * 60 * std.time.ns_per_s;
/// The `il0` read buffer: the blob is 312 B; 512 leaves a decoder-refused
/// longer answer observable rather than ERANGE.
pub const BLOB_READ_BYTES: usize = 512;

// ---------------------------------------------------------------------------
// The passthrough attribution (spec §4 rows; S5).
// ---------------------------------------------------------------------------

pub const Reason = enum(u8) {
    /// Not FUSE / foreign FUSE / undecodable blob / a refused mount in the
    /// negative cache.
    not_ours = 0,
    /// The credential or bind screen: O_PATH, non-regular, O_APPEND, O_SYNC,
    /// O_DSYNC, O_TMPFILE, nlink 0.
    classify_flags = 1,
    establish_refused = 2,
    bind_refused = 3,
    registry_full = 4,
    /// A MAP_SHARED inode of this process.
    mapped_inode = 5,
    rwf_flags = 6,
    lane_gate = 7,
    /// No slot claimable, the daemon rejected OUR descriptor (EINVAL), or
    /// the op's shape (NULL buf, 0 count, offset < 0).
    no_slot_or_shape = 8,
    /// The session is poisoned (deadline, generation bump, ring full).
    poisoned = 9,
    mmap_unbind = 10,
    /// `F_SETLK*`/`F_OFD_SETLK*`, `F_SETFL` + O_APPEND, an O_DIRECT toggle.
    fcntl_unbind = 11,
    /// The lane gate's sticky latch on an offsetful description.
    kernel_latch = 12,
    /// A nested entry over an interposed frame (the guard).
    reentrant = 13,
    /// The real symbol is unresolvable: the kernel answer is synthesized.
    no_real_symbol = 14,
    /// An offsetful op on an unarmed, unseekable description.
    unseekable = 15,
    /// A fd-table cell could not be allocated at bind.
    no_cell = 16,
    /// An establish for this (mount, shard) is in flight on another thread.
    establish_in_progress = 17,
    /// `sendfile`/`copy_file_range`/`splice`/`*v2` demote triggers.
    fpos_consumer = 18,
    /// The wrong direction for the binding's rights (the kernel's EBADF).
    wrong_direction = 19,
    /// Bindings dropped in a fork child (its ops on them are kernel-served
    /// until it establishes its own sessions).
    fork_child = 20,

    pub const count = layout.PRELOAD_REASONS;
};

comptime {
    std.debug.assert(@backingInt(Reason.fork_child) < Reason.count);
}

/// Process-global attribution (the truth for a session-less passthrough);
/// mirrored onto the session's stats page when one is involved.
pub var passthrough_ops: [Reason.count]Atomic(u64) = @splat(.init(0));
pub var bind_refused: [layout.PRELOAD_REFUSE_CLASSES]Atomic(u64) = @splat(.init(0));
/// Sessions this process established / refused (any reason).
pub var sessions_established: Atomic(u64) = .init(0);
pub var sessions_refused: Atomic(u64) = .init(0);

pub fn note(reason: Reason, session: ?*Session) void {
    _ = passthrough_ops[@backingInt(reason)].fetchAdd(1, .monotonic);
    if (session) |s| _ = s.statsPage().preload_passthrough_ops[@backingInt(reason)].fetchAdd(1, .monotonic);
}

pub fn passthroughCount(reason: Reason) u64 {
    return passthrough_ops[@backingInt(reason)].load(.monotonic);
}

fn noteBindRefused(class: wire.RefuseClass, session: *Session) void {
    const i: usize = @backingInt(class);
    const idx = if (i < layout.PRELOAD_REFUSE_CLASSES) i else 0;
    _ = bind_refused[idx].fetchAdd(1, .monotonic);
    _ = session.statsPage().preload_bind_refused[idx].fetchAdd(1, .monotonic);
}

// ---------------------------------------------------------------------------
// Refusal reasons: codes, texts, the once set.
// ---------------------------------------------------------------------------

/// The spec's reason codes: 1–8 the client's own verdicts, `0x1000 | errno`
/// socket, `0x2000 | errno` map, `0x4000 | class` a HELLO refusal,
/// `0x8000 | class` a BIND refusal (a BIND refusal and a HELLO refusal of
/// one class are distinct events).
pub const ReasonCode = u32;
pub const CODE_VERSION_SKEW: ReasonCode = 1;
pub const CODE_PROTOCOL: ReasonCode = 2;
pub const CODE_POISONED: ReasonCode = 3;
pub const CODE_PEER_UNTRUSTED: ReasonCode = 4;
pub const CODE_UNSEALED: ReasonCode = 5;
pub const CODE_UNTRUSTED_RENDEZVOUS: ReasonCode = 6;
pub const CODE_FUTEX_WAITV: ReasonCode = 7;
pub const CODE_REGISTRY_FULL: ReasonCode = 8;
pub const CODE_SOCKET: ReasonCode = 0x1000;
pub const CODE_MAP: ReasonCode = 0x2000;
pub const CODE_REFUSED: ReasonCode = 0x4000;
pub const CODE_BIND_REFUSED: ReasonCode = 0x8000;

pub fn refusalCode(e: client.EstablishError, refusal: ?client.Refusal) ReasonCode {
    return switch (e) {
        error.VersionSkew => CODE_VERSION_SKEW,
        error.Protocol => CODE_PROTOCOL,
        error.PeerUntrusted => CODE_PEER_UNTRUSTED,
        error.Unsealed => CODE_UNSEALED,
        error.UntrustedRendezvous => CODE_UNTRUSTED_RENDEZVOUS,
        error.FutexWaitvUnsupported => CODE_FUTEX_WAITV,
        error.Socket => CODE_SOCKET,
        error.Map => CODE_MAP,
        error.Refused => CODE_REFUSED | @backingInt((refusal orelse client.Refusal{ .class = .internal }).class),
    };
}

/// Is the verdict permanent for the mount's life (re-mounting changes
/// `st_dev`), or transient (backoff)?
pub fn refusalPermanent(code: ReasonCode) bool {
    return switch (code) {
        CODE_VERSION_SKEW, CODE_PEER_UNTRUSTED, CODE_UNSEALED, CODE_UNTRUSTED_RENDEZVOUS, CODE_FUTEX_WAITV, CODE_REGISTRY_FULL => true,
        CODE_REFUSED | @backingInt(wire.RefuseClass.disabled), CODE_REFUSED | @backingInt(wire.RefuseClass.version) => true,
        else => false,
    };
}

/// The daemon class's remedy text (`refuse_reason`).
pub fn refuseClassText(class: ?wire.RefuseClass) []const u8 {
    const c = class orelse return "unknown refusal class";
    return switch (c) {
        .version => "build or abi mismatch (ship the shim with its daemon)",
        .nonce => "stale rendezvous nonce",
        .flags => "credential fd refused by the daemon's screen",
        .mount => "credential fd is not on this mount",
        .budget => "session budget exhausted",
        .peercred => "peer credentials refused",
        .internal => "daemon failed to build the session",
        .disabled => "interception not armed on this mount (mount with -o interception)",
        .rights => "credential access mode refused",
    };
}

/// One line per `(st_dev, reason_code)` for the process lifetime.
pub const RefusalOnce = struct {
    pairs: [REFUSAL_ONCE_CAP]Atomic(u64) = @splat(.init(0)),
    n: Atomic(u32) = .init(0),

    fn keyOf(dev: u64, code: ReasonCode) u64 {
        // dev (low 32 bits suffice: majors/minors) × code, never 0.
        return ((dev & 0xffff_ffff) << 32) | code | (1 << 31);
    }

    /// True = first time (print). Full ⇒ true always (never silent).
    pub fn first(self: *RefusalOnce, dev: u64, code: ReasonCode) bool {
        const key = keyOf(dev, code);
        const n = self.n.load(.acquire);
        var i: u32 = 0;
        while (i < @min(n, REFUSAL_ONCE_CAP)) : (i += 1) {
            if (self.pairs[i].load(.acquire) == key) return false;
        }
        while (true) {
            const cur = self.n.load(.acquire);
            if (cur >= REFUSAL_ONCE_CAP) return true;
            // A racing thread may have appended our key meanwhile.
            var j: u32 = i;
            while (j < cur) : (j += 1) {
                if (self.pairs[j].load(.acquire) == key) return false;
            }
            i = cur;
            if (self.n.cmpxchgWeak(cur, cur + 1, .acq_rel, .acquire) == null) {
                self.pairs[cur].store(key, .release);
                return true;
            }
        }
    }
};

// ---------------------------------------------------------------------------
// Process state.
// ---------------------------------------------------------------------------

const RegistrySlot = struct {
    const EMPTY: u8 = 0;
    const CLAIMING: u8 = 1;
    const READY: u8 = 2;

    state: Atomic(u8) = .init(EMPTY),
    dev: u64 = 0,
    shard: u32 = 0,
    session: Session = undefined,
};

pub const State = struct {
    env: std.process.Environ.Map,
    sessions_per_mount: u32,
    table: Table,
    dev_cache: fd_table.DevCache = .{},
    mapped: fd_table.MappedInoSet = .{},
    registry: [REGISTRY_SLOTS]RegistrySlot = @splat(.{}),
    once: RefusalOnce = .{},
    /// Unbinds `close` could not send without blocking (the ctl lock was
    /// busy): drained by the next ctl-lock holder. 256 = far more than a
    /// burst of closes can race one bind; an overflow is dropped (the
    /// daemon reaps the binding with the session).
    pending_unbinds: [256]Atomic(u64) = @splat(.init(0)),
    pending_sessions: [256]Atomic(u32) = @splat(.init(0)),
    pending_n: Atomic(u32) = .init(0),
    pending_dropped: Atomic(u64) = .init(0),

    pub fn sessionAt(self: *State, index: u32) *Session {
        return &self.registry[index].session;
    }

    fn find(self: *State, dev: u64, shard: u32) ?union(enum) { ready: u32, claiming } {
        for (&self.registry, 0..) |*s, i| {
            const st = s.state.load(.acquire);
            if (st == RegistrySlot.EMPTY) continue;
            if (s.dev == dev and s.shard == shard) return if (st == RegistrySlot.READY) .{ .ready = @intCast(i) } else .claiming;
        }
        return null;
    }

    fn claim(self: *State, dev: u64, shard: u32) ?u32 {
        for (&self.registry, 0..) |*s, i| {
            if (s.state.cmpxchgStrong(RegistrySlot.EMPTY, RegistrySlot.CLAIMING, .acq_rel, .acquire) == null) {
                s.dev = dev;
                s.shard = shard;
                return @intCast(i);
            }
        }
        return null;
    }

    /// Every READY session (the panic handler's and the fork child's walk).
    pub fn eachSession(self: *State, ctx: anytype, comptime f: fn (@TypeOf(ctx), *Session) void) void {
        for (&self.registry) |*s| {
            if (s.state.load(.acquire) == RegistrySlot.READY) f(ctx, &s.session);
        }
    }

    /// The fork child: every slot back to EMPTY (the parent's sessions are
    /// not the child's; their poisoned structs are overwritten by the
    /// child's own establishes). The bound-fd census goes to `fork_child`.
    pub fn resetRegistryInChild(self: *State) void {
        for (&self.registry) |*s| s.state.store(RegistrySlot.EMPTY, .release);
        self.pending_n.store(0, .release);
    }

    /// Any READY session on this device (the `mmap` POSIX-7 screen).
    pub fn hasSessionOn(self: *State, dev: u64) bool {
        for (&self.registry) |*s| {
            if (s.state.load(.acquire) == RegistrySlot.READY and s.dev == dev) return true;
        }
        return false;
    }

    pub fn readySessions(self: *State) u32 {
        var n: u32 = 0;
        for (&self.registry) |*s| {
            if (s.state.load(.acquire) == RegistrySlot.READY) n += 1;
        }
        return n;
    }

    /// Send the Unbind ctl for a last-ref drop without ever blocking: a
    /// busy ctl lock defers it to the next holder.
    pub fn release(self: *State, drop: fd_table.Drop) void {
        if (!drop.last) return;
        const s = self.sessionAt(drop.binding.session);
        self.drainPendingUnbinds();
        if (s.tryUnbind(drop.binding.id)) return;
        const i = self.pending_n.fetchAdd(1, .acq_rel);
        if (i >= self.pending_unbinds.len) {
            _ = self.pending_n.fetchSub(1, .acq_rel);
            _ = self.pending_dropped.fetchAdd(1, .monotonic);
            return;
        }
        self.pending_sessions[i].store(drop.binding.session, .monotonic);
        self.pending_unbinds[i].store(drop.binding.id, .release);
    }

    fn drainPendingUnbinds(self: *State) void {
        while (true) {
            const n = self.pending_n.load(.acquire);
            if (n == 0) return;
            const id = self.pending_unbinds[n - 1].load(.acquire);
            if (id == 0) return; // the appender has not published yet
            const sess = self.pending_sessions[n - 1].load(.monotonic);
            if (self.pending_n.cmpxchgStrong(n, n - 1, .acq_rel, .acquire) != null) continue;
            self.pending_unbinds[n - 1].store(0, .monotonic);
            if (!self.sessionAt(sess).tryUnbind(id)) {
                // Still busy: put it back and stop.
                const j = self.pending_n.fetchAdd(1, .acq_rel);
                if (j < self.pending_unbinds.len) {
                    self.pending_sessions[j].store(sess, .monotonic);
                    self.pending_unbinds[j].store(id, .release);
                } else {
                    _ = self.pending_n.fetchSub(1, .acq_rel);
                    _ = self.pending_dropped.fetchAdd(1, .monotonic);
                }
                return;
            }
        }
    }
};

/// THE process state, built lazily by `lifecycle.state()` (ctor-free).
pub fn initState(st: *State, gpa: std.mem.Allocator, map: fd_table.MapFn) std.mem.Allocator.Error!void {
    st.* = .{
        .env = try sys.environMap(gpa),
        .sessions_per_mount = 0,
        .table = .init(map),
    };
    st.sessions_per_mount = sizing.ilSessionsFromEnv(&st.env, sys.cpuCount());
}

// ---------------------------------------------------------------------------
// The probes (injected by tests).
// ---------------------------------------------------------------------------

pub const Probes = struct {
    fstat: *const fn (sys.fd_t) ?sys.Stat,
    fsType: *const fn (sys.fd_t) ?i64,
    getfl: *const fn (sys.fd_t) ?u32,
    readBlob: *const fn (sys.fd_t, []u8) ?usize,
    tell: *const fn (sys.fd_t) ?u64,

    pub const real: Probes = .{
        .fstat = sys.fstat,
        .fsType = sys.fsType,
        .getfl = sys.getfl,
        .readBlob = readBlobReal,
        .tell = sys.tell,
    };

    fn readBlobReal(fd: sys.fd_t, buf: []u8) ?usize {
        return sys.fgetxattr(fd, wire.IL0_XATTR, buf);
    }
};

/// `classify_fd`: the client mirror of the daemon's screen (an optimization,
/// never the boundary), through the ONE rule set in `rendezvous.screenFd`.
pub fn classifyFd(flags: u32, st: sys.Stat) rendezvous.ScreenVerdict {
    const facts: rendezvous.FdFacts = .{
        .flags = .{
            .accmode = @fromBackingInt(@intCast(@as(u2, @intCast(flags & sys.O.ACCMODE)))),
            .path = flags & sys.O.PATH != 0,
            .append = flags & sys.O.APPEND != 0,
            .sync = flags & sys.O.SYNC == sys.O.SYNC,
            .dsync = flags & sys.O.DSYNC != 0,
            .tmpfile = flags & sys.O.TMPFILE != 0,
            .direct = flags & sys.O.DIRECT != 0,
        },
        .stat = .{
            .kind = switch (st.kind) {
                .regular => .regular,
                .directory => .directory,
                .other => .other,
            },
            .dev = st.dev,
            .ino = st.ino,
            .nlink = st.nlink,
        },
    };
    return rendezvous.screenFd(facts, st.dev, false);
}

// ---------------------------------------------------------------------------
// The ladder.
// ---------------------------------------------------------------------------

pub const Verdict = enum {
    bound,
    /// Passthrough; the reason was noted.
    passthrough,
};

fn line(comptime fmt: []const u8, args: anytype) void {
    var buf: [512]u8 = undefined;
    const s = std.fmt.bufPrint(&buf, "squeezefs-il: " ++ fmt ++ "\n", args) catch return;
    sys.writeStderr(s);
}

/// Steps 5–6 on a registry miss: the FUSE check, the blob, the credential
/// screen, the establish. Null = passthrough (noted and, where the verdict
/// is a property of the mount, cached).
fn establishFor(st: *State, p: Probes, fd: sys.fd_t, stat: sys.Stat, shard: u32, now: u64) ?u32 {
    const ft = p.fsType(fd) orelse {
        note(.not_ours, null);
        return null;
    };
    if (ft != sys.FUSE_SUPER_MAGIC) {
        st.dev_cache.insert(stat.dev, fd_table.DevCache.PERMANENT, now);
        note(.not_ours, null);
        return null;
    }
    var raw: [BLOB_READ_BYTES]u8 = undefined;
    const n = p.readBlob(fd, &raw) orelse 0;
    const blob = wire.Il0Blob.decode(raw[0..n]) catch {
        // ENODATA/ENOTSUP (a foreign FUSE mount, an unarmed SqueezeFS mount)
        // or an undecodable answer: not ours for the mount's life.
        st.dev_cache.insert(stat.dev, fd_table.DevCache.PERMANENT, now);
        note(.not_ours, null);
        return null;
    };
    lifecycle.announceLoadModeOnce();
    // The credential must itself pass the screen; an ineligible first fd
    // only DEFERS establishment (the mount is ours, never negative-cached).
    const fl = p.getfl(fd) orelse {
        note(.classify_flags, null);
        return null;
    };
    switch (classifyFd(fl, stat)) {
        .ok => {},
        .refuse => {
            note(.classify_flags, null);
            return null;
        },
    }
    const slot = st.claim(stat.dev, shard) orelse {
        if (st.once.first(stat.dev, CODE_REGISTRY_FULL)) line("session registry full ({d} slots): mount dev {d} stays kernel-served", .{ REGISTRY_SLOTS, stat.dev });
        st.dev_cache.insert(stat.dev, fd_table.DevCache.PERMANENT, now);
        note(.registry_full, null);
        return null;
    };
    var refusal: ?client.Refusal = null;
    const opts: client.Options = .{
        .kind = .shim,
        .build_commit = lifecycle.build_commit,
        .mount_uid = stat.uid,
        .env = &st.env,
    };
    const session = Session.establish(std.heap.c_allocator, &blob, fd, opts, &refusal) catch |e| {
        st.registry[slot].state.store(RegistrySlot.EMPTY, .release);
        const code = refusalCode(e, refusal);
        _ = sessions_refused.fetchAdd(1, .monotonic);
        if (st.once.first(stat.dev, code)) printRefusal(stat.dev, e, code, refusal, &blob);
        if (refusalPermanent(code)) {
            st.dev_cache.insert(stat.dev, fd_table.DevCache.PERMANENT, now);
        } else {
            _ = st.dev_cache.insertBackoff(stat.dev, now, REFUSAL_BACKOFF_FLOOR_NS, REFUSAL_BACKOFF_CAP_NS);
        }
        note(.establish_refused, null);
        return null;
    };
    st.registry[slot].session = session;
    st.registry[slot].state.store(RegistrySlot.READY, .release);
    _ = sessions_established.fetchAdd(1, .monotonic);
    return slot;
}

fn printRefusal(dev: u64, e: client.EstablishError, code: ReasonCode, refusal: ?client.Refusal, blob: *const wire.Il0Blob) void {
    switch (e) {
        error.VersionSkew => if (blob.abi != wire.IPC_ABI) {
            line("mount dev {d}: abi mismatch (shim {d}, daemon {d}) — kernel-served", .{ dev, wire.IPC_ABI, blob.abi });
        } else if (wire.buildCommitDegenerate(blob.buildCommit()) or wire.buildCommitDegenerate(lifecycle.build_commit)) {
            line("mount dev {d}: dev build identity (shim {s}, daemon {s}) — set SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1 on both ends; kernel-served", .{ dev, lifecycle.build_commit, blob.buildCommit() });
        } else {
            line("mount dev {d}: build mismatch (shim {s}, daemon {s}) — kernel-served", .{ dev, lifecycle.build_commit, blob.buildCommit() });
        },
        error.Protocol => line("mount dev {d}: malformed ctl reply / session layout — kernel-served", .{dev}),
        error.PeerUntrusted => line("mount dev {d}: socket peer uid is neither root nor the mount owner — not sending the credential fd", .{dev}),
        error.Unsealed => line("mount dev {d}: session memfd is not sealed (need SEAL|SHRINK|GROW) — kernel-served", .{dev}),
        error.UntrustedRendezvous => line("mount dev {d}: the advertised socket path does not resolve under a root- or owner-private directory — kernel-served", .{dev}),
        error.FutexWaitvUnsupported => line("mount dev {d}: kernel has no futex_waitv — kernel-served", .{dev}),
        error.Socket => line("mount dev {d}: ctl socket unreachable or closed — kernel-served (code {x})", .{ dev, code }),
        error.Map => line("mount dev {d}: session shm map failed — kernel-served", .{dev}),
        error.Refused => line("mount dev {d}: daemon refused the session: {s} (class {d})", .{ dev, refuseClassText(if (refusal) |r| r.class else null), code & 0xff }),
    }
}

/// `classify_and_bind(fd, epoch)`: the whole ladder. `epoch` is the fork-
/// epoch snapshot taken BEFORE the fd's `open` syscall (`maxInt` = never
/// arm).
pub fn classifyAndBind(st: *State, p: Probes, fd: sys.fd_t, epoch: u64) Verdict {
    if (fd < 0) return .passthrough;
    // 1. A stale entry on the number (a raw close + reuse).
    if (st.table.onClose(fd)) |d| st.release(d);
    // 2. fstat; the negative cache.
    const stat = p.fstat(fd) orelse return .passthrough;
    const now = clock.monotonicNs();
    if (st.dev_cache.contains(stat.dev, now)) {
        note(.not_ours, null);
        return .passthrough;
    }
    // 3. POSIX-7.
    if (st.mapped.contains(stat.dev, stat.ino)) {
        note(.mapped_inode, null);
        return .passthrough;
    }
    // 4. The shard.
    const shard: u32 = @as(u32, @intCast(fd)) % @max(st.sessions_per_mount, 1);
    // 5–6. The registry.
    const index: u32 = blk: {
        if (st.find(stat.dev, shard)) |f| switch (f) {
            .ready => |i| break :blk i,
            .claiming => {
                // Another thread is establishing this (mount, shard): this
                // open stays kernel-served; the next one binds.
                note(.establish_in_progress, null);
                return .passthrough;
            },
        };
        break :blk establishFor(st, p, fd, stat, shard, now) orelse return .passthrough;
    };
    const session = st.sessionAt(index);
    // 7. The screen (bind-time passthrough).
    const fl = p.getfl(fd) orelse {
        note(.classify_flags, session);
        return .passthrough;
    };
    switch (classifyFd(fl, stat)) {
        .ok => {},
        .refuse => {
            note(.classify_flags, session);
            return .passthrough;
        },
    }
    // 8. BIND; the mirror seed.
    const b = session.bind(fd, .mount) catch |e| {
        switch (e) {
            error.Refused => {
                const class = session.last_refusal;
                noteBindRefused(class orelse .internal, session);
                const code = CODE_BIND_REFUSED | @backingInt(class orelse .internal);
                if (st.once.first(stat.dev, code)) line("mount dev {d}: daemon refused the bind: {s} (class {d})", .{ stat.dev, refuseClassText(class), code & 0xff });
                note(.bind_refused, session);
            },
            // Socket/poison-class errors are silent: the poison machinery
            // owns the loudness.
            error.Poisoned, error.Socket, error.Protocol => note(.poisoned, session),
        }
        return .passthrough;
    };
    const binding: fd_table.Binding = .{ .session = index, .id = b.id, .dev = stat.dev, .ino = b.ino, .rights = b.rights };
    const mirror: ?Table.Mirror = if (epoch != std.math.maxInt(u64)) (if (p.tell(fd)) |seed| .{ .seed = seed, .epoch = epoch } else null) else null;
    const bound = st.table.bind(fd, binding, mirror) orelse {
        // No cell: the binding is released at once (the daemon's view
        // must not outlive ours).
        session.unbind(b.id);
        note(.no_cell, session);
        return .passthrough;
    };
    if (bound.displaced) |d| st.release(d);
    return .bound;
}

// ---------------------------------------------------------------------------
// Tests: the ladder with injected answers; the registry; the once set.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testMap(len: usize) ?[*]align(4096) u8 {
    return sys.mapAnon(len);
}

/// Injected probe answers (one fd at a time).
const Fake = struct {
    var stat: ?sys.Stat = null;
    var fstype: ?i64 = null;
    var flags: ?u32 = null;
    var blob: ?[]const u8 = null;
    var seed: ?u64 = null;
    var fsType_calls: u32 = 0;
    var blob_calls: u32 = 0;

    fn fstat(_: sys.fd_t) ?sys.Stat {
        return stat;
    }
    fn fsType(_: sys.fd_t) ?i64 {
        fsType_calls += 1;
        return fstype;
    }
    fn getfl(_: sys.fd_t) ?u32 {
        return flags;
    }
    fn readBlob(_: sys.fd_t, buf: []u8) ?usize {
        blob_calls += 1;
        const b = blob orelse return null;
        const n = @min(b.len, buf.len);
        @memcpy(buf[0..n], b[0..n]);
        return n;
    }
    fn tell(_: sys.fd_t) ?u64 {
        return seed;
    }
    const probes: Probes = .{ .fstat = fstat, .fsType = fsType, .getfl = getfl, .readBlob = readBlob, .tell = tell };

    fn reset() void {
        stat = .{ .kind = .regular, .dev = 99, .ino = 5, .nlink = 1, .uid = linux.geteuid() };
        fstype = sys.FUSE_SUPER_MAGIC;
        flags = sys.O.RDWR;
        blob = null;
        seed = 0;
        fsType_calls = 0;
        blob_calls = 0;
    }
};

fn snapshot() [Reason.count]u64 {
    var out: [Reason.count]u64 = undefined;
    for (&out, 0..) |*o, i| o.* = passthrough_ops[i].load(.monotonic);
    return out;
}

fn delta(before: [Reason.count]u64, r: Reason) u64 {
    return passthrough_ops[@backingInt(r)].load(.monotonic) - before[@backingInt(r)];
}

test "the ladder with injected answers: not FUSE ⇒ negative-cached (the second open pays no fstatfs); a foreign FUSE mount (no blob) ⇒ cached; a decodable blob with an ineligible credential DEFERS (never cached); a MAP_SHARED inode never binds" {
    const st = try testing.allocator.create(State);
    defer testing.allocator.destroy(st);
    try initState(st, testing.allocator, testMap);
    defer st.env.deinit();
    Fake.reset();
    // Not FUSE.
    Fake.fstype = 0xEF53;
    var before = snapshot();
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u64, 1), delta(before, .not_ours));
    try testing.expectEqual(@as(u32, 1), Fake.fsType_calls);
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 8, 0));
    try testing.expectEqual(@as(u32, 1), Fake.fsType_calls); // cached: no second fstatfs
    try testing.expectEqual(@as(u64, 2), delta(before, .not_ours));
    // A foreign FUSE mount on another device: ENODATA ⇒ cached.
    Fake.reset();
    Fake.stat.?.dev = 100;
    before = snapshot();
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u32, 1), Fake.blob_calls);
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u32, 1), Fake.blob_calls);
    try testing.expectEqual(@as(u64, 2), delta(before, .not_ours));
    // Ours, but the credential is O_APPEND: deferred, not cached.
    Fake.reset();
    Fake.stat.?.dev = 101;
    var nonce: rendezvous.Nonce = @splat(1);
    const blob = try rendezvous.buildBlob(.{ .build_commit = lifecycle.build_commit, .socket_name = "sqz-il0-dead-00000000" }, &nonce);
    Fake.blob = blob.bytes();
    Fake.flags = sys.O.WRONLY | sys.O.APPEND;
    before = snapshot();
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u64, 1), delta(before, .classify_flags));
    try testing.expect(!st.dev_cache.contains(101, clock.monotonicNs()));
    // The MAP_SHARED set.
    Fake.reset();
    Fake.stat.?.dev = 102;
    st.mapped.insert(102, 5);
    before = snapshot();
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u64, 1), delta(before, .mapped_inode));
}

test "a dead rendezvous (socket error) is a TRANSIENT refusal: one line, a backoff cache entry that expires, and the once set dedups the line per (mount, code)" {
    const st = try testing.allocator.create(State);
    defer testing.allocator.destroy(st);
    try initState(st, testing.allocator, testMap);
    defer st.env.deinit();
    Fake.reset();
    Fake.stat.?.dev = 200;
    var nonce: rendezvous.Nonce = @splat(1);
    const blob = try rendezvous.buildBlob(.{ .build_commit = lifecycle.build_commit, .socket_name = "sqz-il0-dead-00000000" }, &nonce);
    Fake.blob = blob.bytes();
    const before = snapshot();
    const refused_before = sessions_refused.load(.monotonic);
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expectEqual(@as(u64, 1), delta(before, .establish_refused));
    try testing.expectEqual(refused_before + 1, sessions_refused.load(.monotonic));
    const now = clock.monotonicNs();
    try testing.expect(st.dev_cache.contains(200, now));
    try testing.expect(!st.dev_cache.contains(200, now + REFUSAL_BACKOFF_FLOOR_NS + 1));
    try testing.expect(!refusalPermanent(CODE_SOCKET | 111));
    try testing.expect(refusalPermanent(CODE_VERSION_SKEW));
    try testing.expect(refusalPermanent(CODE_REFUSED | @backingInt(wire.RefuseClass.disabled)));
    try testing.expect(!refusalPermanent(CODE_REFUSED | @backingInt(wire.RefuseClass.budget)));
    // The once set.
    try testing.expect(!st.once.first(200, CODE_SOCKET));
    try testing.expect(st.once.first(200, CODE_VERSION_SKEW));
    try testing.expect(st.once.first(201, CODE_SOCKET));
    // A version skew is permanent.
    Fake.reset();
    Fake.stat.?.dev = 202;
    const skew = try rendezvous.buildBlob(.{ .build_commit = "ffffffffffff", .socket_name = "sqz-il0-dead-00000000" }, &nonce);
    Fake.blob = skew.bytes();
    try testing.expectEqual(Verdict.passthrough, classifyAndBind(st, Fake.probes, 7, 0));
    try testing.expect(st.dev_cache.contains(202, std.math.maxInt(u64) - 1));
}

test "classify_fd mirrors the daemon's screen rows: O_PATH, non-regular, O_APPEND/O_SYNC/O_DSYNC/O_TMPFILE, nlink 0 refuse; rights strictly from O_ACCMODE" {
    const reg: sys.Stat = .{ .kind = .regular, .dev = 1, .ino = 2, .nlink = 1, .uid = 0 };
    try testing.expect(classifyFd(sys.O.RDONLY | sys.O.PATH, reg) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR, .{ .kind = .directory, .dev = 1, .ino = 2, .nlink = 1, .uid = 0 }) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR | sys.O.APPEND, reg) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR | sys.O.SYNC, reg) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR | sys.O.DSYNC, reg) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR | sys.O.TMPFILE, reg) == .refuse);
    try testing.expect(classifyFd(sys.O.RDWR, .{ .kind = .regular, .dev = 1, .ino = 2, .nlink = 0, .uid = 0 }) == .refuse);
    const ro = classifyFd(sys.O.RDONLY, reg).ok;
    try testing.expect(ro.rights.read and !ro.rights.write);
    const wo = classifyFd(sys.O.WRONLY, reg).ok;
    try testing.expect(!wo.rights.read and wo.rights.write);
    const rw = classifyFd(sys.O.RDWR | sys.O.DIRECT, reg).ok;
    try testing.expect(rw.rights.read and rw.rights.write and rw.rights.odirect);
    try testing.expect(classifyFd(3, reg) == .refuse);
}

test "the registry ceiling is half the static table; the once set stops remembering at capacity but never stops printing" {
    try testing.expectEqual(REGISTRY_SLOTS / 2, sizing.ilSessionsDefault(4096));
    const o = try testing.allocator.create(RefusalOnce);
    defer testing.allocator.destroy(o);
    o.* = .{};
    var i: u64 = 0;
    while (i < REFUSAL_ONCE_CAP) : (i += 1) try testing.expect(o.first(i, 1));
    try testing.expect(!o.first(5, 1));
    try testing.expect(o.first(5000, 1));
    try testing.expect(o.first(5000, 1));
}
