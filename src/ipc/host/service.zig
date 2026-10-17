//! The service thread — `sqz-ipc-svc<N>` (charter §7 "dedicated service
//! threads … one session per thread for life"; spec `docs/porting/
//! il_daemon.md` §1, §2, §3). ONE thread owns every session pinned to its
//! owner index for the session's whole life (the ring core's single-
//! consumer PRECONDITION: no drain handoff, no rebalance of live
//! sessions). Per pass:
//!
//!   heartbeat stamp (every owned header; decision 0003 §F)
//!   → the epoch-cached owned set (the registry mutex is touched only when
//!     an admission or a teardown moved `session_epoch`)
//!   → `drainPass`: a budgeted round-robin from a rotating start — each
//!     session drains at most `slots` ops (VAL-5e: a fairness bound, never
//!     a throughput cap: one honest in-flight window drains in one pass)
//!   → per popped ring index: the slot bounds → `tryBeginServe` (a non-
//!     SUBMITTED slot ⇒ POISON) → the in-flight gate (`live > slots` ⇒
//!     POISON, VAL-5c) → THE ONE SNAPSHOT READ → one clock read `t0` → the
//!     validation (unknown op / dead binding / `len > max_op` / window out
//!     of the arena ⇒ `-EINVAL`; the wrong direction ⇒ `-EBADF`; all
//!     `ipc_descriptor_rejects`) → the sink's `serve`
//!   → `flush()` once per sweep (the liveness rule: work deferred during
//!     `serve` becomes kernel-visible here — the thread may park right after)
//!   → served > 0 ⇒ `ipc_drain_pass_ns` and loop; else the park ladder:
//!     the spin governor's window (`doorbell.SpinGovernor`), then
//!     `daemon_parked := 1` → SNAPSHOT every doorbell → RESCAN + flush →
//!     `futex_waitv` on ALL owned doorbells against the pre-rescan
//!     snapshots, bounded 5 ms (`doorbell.ServicePark`; the two lost-wake
//!     classes the order closes are the submit model's) → `daemon_parked :=
//!     0`. A session-less owner parks on a daemon-PRIVATE word, unbounded,
//!     woken by admission (decision 0002 row 310).
//!
//! The completion order is `session.Completion`'s. The sink is a vtable so
//! the loop runs over a stub in tests; `DataPlaneSink` is the product:
//! the SYNC READ FAST PATH (`WarmLadder.probe` into the arena — ONE copy;
//! EOF; a demote drops the guard FIRST — structural in the ladder — then
//! hands the FULL read handler to a FUSE lane homed on the arena's node,
//! never a runtime inject queue: the hop measured ~130 µs/op), WRITES by
//! sever-at-dequeue (the W1-shaped inline arm on the direct-drive lane,
//! else the ONE arena read into a pooled slab or a placed assembly BEFORE
//! the handoff counter moves — a client scribble after it is inert), the
//! placed handoffs deferred to the sweep's `flush` (a sibling landing in the
//! assembly before the first merge parks the entry is the whole win:
//! ~50 % sever engagement / ~28 % merge elision measured without the
//! deferral), the handoff fiber entering `kv_fs.write` through the level-1
//! admission exactly as a kernel write does, and the KD-11 invalidator
//! after every acked write.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const thread_name = @import("../../core/thread_name.zig");
const futex = @import("../../sync/futex.zig");
const placement = @import("../../exec/placement.zig");
const sqz_io = @import("../../exec/sqz_io.zig");
const layout = @import("../layout.zig");
const slot_core = @import("../slot_core.zig");
const doorbell = @import("../doorbell.zig");
const wire = @import("../wire.zig");
const session_mod = @import("session.zig");
const stats = @import("stats.zig").stats;
const severed_pool = @import("severed_pool.zig");
const placed = @import("placed.zig");
const inval_mod = @import("inval.zig");
const direct = @import("direct.zig");
const abi = @import("../../fuse/abi.zig");
const filesystem = @import("../../fuse/handler/filesystem.zig");
const op_guard = @import("../../fuse/handler/op_guard.zig");
const op_trace = @import("../../fuse/handler/op_trace.zig");
const open_table = @import("../../fuse/handler/open_table.zig");
const route_table = @import("../../meta/routing/route_table.zig");
const plane_mod = @import("../../data/plane.zig");
const warm_ladder = @import("../../data/warm_ladder.zig");
const write_admit = @import("../../data/write_admit.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host/service is Linux only; gate the import on builtin.os.tag");
}

pub const Session = session_mod.Session;
pub const Completion = session_mod.Completion;

/// `futex_waitv`'s entry cap: one park round covers this many sessions;
/// an owner with more parks in rotating rounds (decision 0002 quirk 11).
pub const PARK_ROUND = futex.wait_many_max;
/// A failed enter / a lost CAS re-reads `/proc/stat` this often at most:
/// the busy sample is a governor input, never per pass.
pub const BUSY_SAMPLE_NS: u64 = std.time.ns_per_s;

/// A validated data op: the private descriptor copy, the binding row (by
/// value — the row may die under the op; the serve works from this copy),
/// the dequeue instant and the arena window the snapshot named.
pub const DataOp = struct {
    op: layout.Op,
    desc: layout.SlotDescriptor,
    binding: session_mod.Binding,
    t0_ns: u64,
    window: []u8,
};

/// The per-op sink the thread dispatches to. `serve` must complete `c`
/// exactly once (synchronously, or from a handoff/CQE later); `flush`
/// runs once per sweep.
pub const Sink = struct {
    ctx: *anyopaque,
    serve: *const fn (ctx: *anyopaque, owner: u16, s: *Session, op: DataOp, c: Completion) void,
    flush: *const fn (ctx: *anyopaque, owner: u16) void,
};

/// The session registry as the thread sees it: an epoch, a collect that
/// RETAINS every session it hands out, the poison act.
pub const Registry = struct {
    ctx: *anyopaque,
    epoch: *const fn (ctx: *anyopaque) u64,
    collect: *const fn (ctx: *anyopaque, owner: u16, out: []*Session) usize,
    poison: *const fn (ctx: *anyopaque, s: *Session, reason: session_mod.PoisonReason) void,
};

pub const Config = struct {
    index: u16,
    /// The owner's partition node and its CPU set (the pin); null = unpinned.
    node: ?u16,
    cpus: ?placement.CpuSet,
    /// The governor's inputs: the lane count and the core count.
    lanes: u32,
    cores: u32,
    spin: doorbell.SpinGovernor.Config,
    /// The most sessions this owner may be handed (the registry's cap).
    max_sessions: usize,
};

pub const Owner = struct {
    gpa: Allocator,
    cfg: Config,
    registry: Registry,
    sink: Sink,
    stop: *const Atomic(bool),
    /// The session-less park's private word (a generation; admission bumps).
    wake: futex.Word = .init(0),
    sessions: []*Session,
    n: usize = 0,
    epoch_seen: u64 = std.math.maxInt(u64),
    headers: [PARK_ROUND]*layout.SessionHeader = undefined,
    park: doorbell.ServicePark = .{},
    park_round: usize = 0,
    rr: usize = 0,
    ewma_park_ns: u64 = 0,
    busy_pct: u32 = 0,
    busy_sampled_ns: u64 = 0,
    busy_prev: [2]u64 = .{ 0, 0 },
    thread: ?std.Thread = null,
    /// Owned sessions right now (the owner-pick's load word).
    load: Atomic(u32) = .init(0),
    /// `SQUEEZEFS_IPC_CQE_WAKE_LATCH`.
    latch: bool = true,
    /// Why the last `drain` answered null.
    last_poison: session_mod.PoisonReason = .slot_out_of_range,

    pub fn init(gpa: Allocator, cfg: Config, registry: Registry, sink: Sink, stop: *const Atomic(bool)) Allocator.Error!*Owner {
        const self = try gpa.create(Owner);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .cfg = cfg, .registry = registry, .sink = sink, .stop = stop, .sessions = try gpa.alloc(*Session, @max(cfg.max_sessions, 1)) };
        return self;
    }

    pub fn deinit(self: *Owner) void {
        self.releaseSet();
        self.gpa.free(self.sessions);
        self.gpa.destroy(self);
    }

    pub fn spawn(self: *Owner) std.Thread.SpawnError!void {
        self.thread = try std.Thread.spawn(.{}, run, .{self});
    }

    /// Admission / teardown / shutdown: the owner re-collects at its next
    /// pass top; a session-less owner wakes from its private park.
    pub fn kick(self: *Owner) void {
        _ = self.wake.fetchAdd(1, .release);
        futex.wakeAll(&self.wake);
    }

    pub fn join(self: *Owner) void {
        if (self.thread) |t| t.join();
        self.thread = null;
    }

    fn releaseSet(self: *Owner) void {
        for (self.sessions[0..self.n]) |s| _ = s.map.releaseRef();
        self.n = 0;
    }

    /// The epoch-cached owned set: re-collect only when the registry moved.
    fn refresh(self: *Owner) void {
        const e = self.registry.epoch(self.registry.ctx);
        if (e == self.epoch_seen) return;
        self.releaseSet();
        self.n = self.registry.collect(self.registry.ctx, self.cfg.index, self.sessions);
        self.epoch_seen = e;
        self.load.store(@intCast(self.n), .monotonic);
    }

    fn run(self: *Owner) void {
        var name_buf: thread_name.CommBuf = undefined;
        var base_buf: [thread_name.comm_max]u8 = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, std.fmt.bufPrint(&base_buf, "sqz-ipc-svc{d}", .{self.cfg.index}) catch "sqz-ipc-svc");
        if (self.cfg.cpus) |set| pinTo(set);
        while (!self.stop.load(.acquire)) {
            const now = clock.monotonicNs();
            for (self.sessions[0..self.n]) |s| s.stampHeartbeat(now);
            self.refresh();
            const served = self.drainPass();
            const t1 = clock.monotonicNs();
            self.sink.flush(self.sink.ctx, self.cfg.index);
            if (served > 0) {
                stats.ipc_drain_pass_ns.record(t1 -| now);
                stats.ipc_drain_flush_ns.record(clock.monotonicNs() -| t1);
                continue;
            }
            stats.ipc_drain_empty_passes.inc();
            if (self.n == 0) {
                // Nothing to hear: the private word, unbounded, woken by
                // admission or the stop.
                const w = self.wake.load(.acquire);
                if (self.registry.epoch(self.registry.ctx) != self.epoch_seen or self.stop.load(.acquire)) continue;
                futex.wait(&self.wake, w);
                continue;
            }
            if (self.spinWindow(t1)) {
                stats.ipc_spin_absorbed_parks.inc();
                continue;
            }
            self.parkOnce();
        }
        self.releaseSet();
    }

    fn pinTo(set: placement.CpuSet) void {
        if (set.count() == 0) return;
        var cpu_set: linux.cpu_set_t = @splat(0);
        var it = set.iterator(.{});
        while (it.next()) |cpu| cpu_set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        // Best-effort: a refused mask leaves the thread where the scheduler put it.
        linux.sched_setaffinity(0, &cpu_set) catch {};
    }

    /// The governor's window for this empty pass; true when a doorbell
    /// moved inside it (the park absorbed).
    fn spinWindow(self: *Owner, now: u64) bool {
        self.sampleBusy(now);
        const us = doorbell.SpinGovernor.windowUs(self.cfg.spin, self.ewma_park_ns, self.busy_pct, self.cfg.lanes, self.cfg.cores, @intCast(self.n));
        stats.ipc_spin_window_us.set(us);
        if (us == 0) {
            if (self.cfg.spin.explicit_us == null and self.cfg.spin.adaptive and self.n >= 2 and self.ewma_park_ns != 0 and self.busy_pct > doorbell.SpinGovernor.busyCeilingPct(self.cfg.lanes, self.cfg.cores)) stats.ipc_spin_disengaged_busy.inc();
            return false;
        }
        var snaps: [PARK_ROUND]u32 = undefined;
        const round = self.roundSlice();
        for (round, 0..) |s, i| snaps[i] = s.header().doorbell.load(.seq_cst);
        const deadline = now + us * std.time.ns_per_us;
        while (clock.monotonicNs() < deadline) {
            for (round, 0..) |s, i| if (s.header().doorbell.load(.seq_cst) != snaps[i]) return true;
            std.atomic.spinLoopHint();
        }
        return false;
    }

    /// `/proc/stat`'s first line → busy %, at most once per second.
    fn sampleBusy(self: *Owner, now: u64) void {
        if (now -| self.busy_sampled_ns < BUSY_SAMPLE_NS) return;
        self.busy_sampled_ns = now;
        var buf: [512]u8 = undefined;
        const rc = linux.open("/proc/stat", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const n = linux.read(fd, &buf, buf.len);
        if (linux.errno(n) != .SUCCESS) return;
        const sample = parseProcStatCpu(buf[0..n]) orelse return;
        const d_total = sample[0] -| self.busy_prev[0];
        const d_idle = sample[1] -| self.busy_prev[1];
        if (self.busy_prev[0] != 0 and d_total != 0) self.busy_pct = @intCast(100 - @min(100, 100 * d_idle / d_total));
        self.busy_prev = sample;
    }

    /// The sessions of the current park round (≤ `PARK_ROUND`).
    fn roundSlice(self: *Owner) []*Session {
        if (self.n <= PARK_ROUND) return self.sessions[0..self.n];
        const start = (self.park_round * PARK_ROUND) % self.n;
        const end = @min(start + PARK_ROUND, self.n);
        return self.sessions[start..end];
    }

    /// The park ladder's parked half: arm → rescan → wait → disarm.
    fn parkOnce(self: *Owner) void {
        const round = self.roundSlice();
        for (round, 0..) |s, i| self.headers[i] = s.header();
        self.park.arm(self.headers[0..round.len]);
        const t0 = clock.monotonicNs();
        const served = self.drainPass();
        self.sink.flush(self.sink.ctx, self.cfg.index);
        if (served == 0) {
            switch (self.park.wait()) {
                .returned => stats.ipc_service_wakes.inc(),
                .timed_out => stats.ipc_service_park_timeouts.inc(),
                // ENOSYS is refused at the client's establish; a late one
                // reads as a timed park (the loop re-scans).
                .unsupported => stats.ipc_service_park_timeouts.inc(),
            }
        }
        self.park.disarm();
        const now = clock.monotonicNs();
        for (self.sessions[0..self.n]) |s| s.stampHeartbeat(now);
        self.ewma_park_ns = doorbell.SpinGovernor.foldParkNs(self.ewma_park_ns, now -| t0);
        self.park_round +%= 1;
    }

    /// One pass over the owned set from a rotating start.
    fn drainPass(self: *Owner) u32 {
        if (self.n == 0) return 0;
        var served: u32 = 0;
        const start = self.rr % self.n;
        self.rr +%= 1;
        var i: usize = 0;
        while (i < self.n) : (i += 1) {
            const s = self.sessions[(start + i) % self.n];
            if (s.torn_down.load(.acquire) or s.poisoned.load(.acquire)) continue;
            served += self.drain(s) orelse {
                self.registry.poison(self.registry.ctx, s, self.last_poison);
                continue;
            };
        }
        return served;
    }

    /// Drain one session up to its budget; null = poison (`last_poison`).
    fn drain(self: *Owner, s: *Session) ?u32 {
        const budget: u32 = s.layout.geometry.slots;
        var served: u32 = 0;
        while (served < budget) {
            const idx = s.consumer.pop(s.ring) orelse break;
            const slot = s.m.slot(idx) catch {
                self.last_poison = .slot_out_of_range;
                return null;
            };
            if (!slot_core.tryBeginServe(slot)) {
                self.last_poison = .slot_not_submitted;
                return null;
            }
            if (!s.beginInflight()) {
                self.last_poison = .over_admission;
                return null;
            }
            // THE one linearization read, then the one clock read.
            const desc = slot_core.snapshot(slot);
            const t0 = clock.monotonicNs();
            if (layout.ingressDeltaNs(slot.ingress_stamp_ns.load(.monotonic), @truncate(t0))) |d| stats.ipc_ingress_ns.record(d);
            s.map.retain();
            const c: Completion = .{ .session = s, .slot_index = idx, .latch = self.latch };
            self.serveValidated(s, desc, t0, c);
            s.touch(t0);
            served += 1;
        }
        return served;
    }

    fn serveValidated(self: *Owner, s: *Session, desc: layout.SlotDescriptor, t0: u64, c: Completion) void {
        const op: layout.Op = switch (desc.op) {
            @backingInt(layout.Op.read) => .read,
            @backingInt(layout.Op.write) => .write,
            else => return reject(c, .INVAL, "unknown op"),
        };
        const b = s.binding(desc.binding) orelse return reject(c, .INVAL, "dead binding id");
        if (desc.len > s.layout.geometry.max_op_bytes) return reject(c, .INVAL, "len > max_op_bytes");
        const window = s.m.arenaWindow(desc.arena_off, desc.len) catch return reject(c, .INVAL, "arena window out of bounds");
        switch (op) {
            .read => if (!b.rights.read) return reject(c, .BADF, "read on a write-only binding"),
            .write => if (!b.rights.write) return reject(c, .BADF, "write on a read-only binding"),
            _ => unreachable,
        }
        self.sink.serve(self.sink.ctx, self.cfg.index, s, .{ .op = op, .desc = desc, .binding = b.*, .t0_ns = t0, .window = window }, c);
    }

    fn reject(c: Completion, e: linux.E, why: []const u8) void {
        stats.ipc_descriptor_rejects.fire(why);
        c.fail(e);
    }
};

/// `cpu  user nice system idle …` → `(total, idle)` jiffies.
pub fn parseProcStatCpu(text: []const u8) ?[2]u64 {
    if (!std.mem.startsWith(u8, text, "cpu ")) return null;
    const line_end = std.mem.indexOfScalar(u8, text, '\n') orelse text.len;
    var it = std.mem.tokenizeScalar(u8, text[4..line_end], ' ');
    var total: u64 = 0;
    var idle: u64 = 0;
    var i: usize = 0;
    while (it.next()) |tok| : (i += 1) {
        const v = std.fmt.parseInt(u64, tok, 10) catch return null;
        total += v;
        // idle (3) + iowait (4) are the idle classes.
        if (i == 3 or i == 4) idle += v;
    }
    if (i < 4) return null;
    return .{ total, idle };
}

// ---------------------------------------------------------------------------
// The product sink.
// ---------------------------------------------------------------------------

/// What one write severed: a pooled slab (`[0..len]`) or a placed payload.
pub const Severed = union(enum) {
    pooled: struct { slab: severed_pool.Slab, len: u32 },
    placed: placed.Placed,

    fn bytes(self: Severed) []const u8 {
        return switch (self) {
            .pooled => |p| p.slab[0..p.len],
            .placed => |p| p.bytes(),
        };
    }
};

pub const DataPlaneSink = struct {
    gpa: Allocator,
    io: Io,
    sqz: *sqz_io.SqzIo,
    plane: *plane_mod.DataPlane,
    fs: filesystem.Filesystem,
    guards: *op_guard.Table,
    probe: op_guard.Probe,
    shared_enabled: bool,
    opens: *open_table.OpenTable,
    pool: *severed_pool.SeveredPool,
    placed: *placed.Registry,
    inval: *inval_mod.Invalidator,
    dd: ?*direct.Engine,
    /// `SQUEEZEFS_IL_DIRECT_WRITE`.
    direct_write: bool,
    max_file_size: u64,
    /// The synthetic request identity: the daemon's own (authorization
    /// happened at the fd screen).
    uid: u32,
    gid: u32,
    pid: u32,
    per_owner: []PerOwner,

    /// The deferral queue of one owner: a pass defers at most its budget of
    /// placed handoffs (decision 0002 quirk 10: a fixed array, no boxed
    /// future, no thread-local).
    pub const PerOwner = struct {
        pending: []Pending,
        n: usize = 0,
    };

    const Pending = struct {
        s: *Session,
        op: DataOp,
        c: Completion,
        sev: Severed,
    };

    pub const Deps = struct {
        io: Io,
        sqz: *sqz_io.SqzIo,
        plane: *plane_mod.DataPlane,
        fs: filesystem.Filesystem,
        guards: *op_guard.Table,
        probe: op_guard.Probe,
        shared_enabled: bool,
        opens: *open_table.OpenTable,
        pool: *severed_pool.SeveredPool,
        placed: *placed.Registry,
        inval: *inval_mod.Invalidator,
        dd: ?*direct.Engine,
        direct_write: bool,
        max_file_size: u64,
        owners: u16,
        /// The deferral depth per owner (= the geometry's slots).
        drain_budget: u32,
    };

    pub fn init(gpa: Allocator, d: Deps) Allocator.Error!*DataPlaneSink {
        const self = try gpa.create(DataPlaneSink);
        errdefer gpa.destroy(self);
        const per_owner = try gpa.alloc(PerOwner, d.owners);
        errdefer gpa.free(per_owner);
        var made: usize = 0;
        errdefer for (per_owner[0..made]) |po| gpa.free(po.pending);
        for (per_owner) |*po| {
            po.* = .{ .pending = try gpa.alloc(Pending, d.drain_budget) };
            made += 1;
        }
        self.* = .{
            .gpa = gpa,
            .io = d.io,
            .sqz = d.sqz,
            .plane = d.plane,
            .fs = d.fs,
            .guards = d.guards,
            .probe = d.probe,
            .shared_enabled = d.shared_enabled,
            .opens = d.opens,
            .pool = d.pool,
            .placed = d.placed,
            .inval = d.inval,
            .dd = d.dd,
            .direct_write = d.direct_write,
            .max_file_size = d.max_file_size,
            .uid = linux.geteuid(),
            .gid = linux.getegid(),
            .pid = @intCast(linux.getpid()),
            .per_owner = per_owner,
        };
        return self;
    }

    pub fn deinit(self: *DataPlaneSink) void {
        for (self.per_owner) |po| self.gpa.free(po.pending);
        self.gpa.free(self.per_owner);
        self.gpa.destroy(self);
    }

    pub fn sink(self: *DataPlaneSink) Sink {
        return .{ .ctx = self, .serve = &serveFn, .flush = &flushFn };
    }

    fn serveFn(ctx: *anyopaque, owner: u16, s: *Session, op: DataOp, c: Completion) void {
        const self: *DataPlaneSink = @ptrCast(@alignCast(ctx));
        // The kernel nodeid → the KEY ino the plane's tables are keyed by
        // (the lanes' law, applied here so a nodeid never reaches the plane).
        const key_ino = if (route_table.isRoutable(op.binding.ino)) route_table.effectiveIno(op.binding.ino) else return c.fail(.INVAL);
        switch (op.op) {
            .read => self.serveRead(s, op, key_ino, c),
            .write => self.serveWrite(owner, s, op, key_ino, c),
            _ => unreachable,
        }
    }

    fn flushFn(ctx: *anyopaque, owner: u16) void {
        const self: *DataPlaneSink = @ptrCast(@alignCast(ctx));
        const po = &self.per_owner[owner];
        // The deferred placed handoffs: every sibling chunk of the pass is
        // in its assembly now.
        var i: usize = 0;
        while (i < po.n) : (i += 1) {
            const p = po.pending[i];
            self.spawnWriteHandoff(p.s, p.op, p.c, p.sev);
        }
        po.n = 0;
        if (self.dd) |dd| dd.flushLane(owner);
    }

    // ---- the sync READ fast path (§2) ----

    fn serveRead(self: *DataPlaneSink, s: *Session, op: DataOp, key_ino: u64, c: Completion) void {
        if (op.desc.len == 0) {
            // The liveness ping: nothing to read, nothing to demote.
            stats.ipc_ops_read.inc();
            stats.ipc_fast_path_serves.inc();
            return c.complete(0);
        }
        switch (self.plane.ladder.probe(key_ino, op.desc.offset, op.desc.len, op.binding.rights.odirect, op.window)) {
            .served => |sv| {
                if (sv.view) |view| {
                    // A memfd-pool view (the zc session's fd-source arm): the
                    // ring has no bridge — the one copy lands here.
                    @memcpy(op.window[0..sv.n], view);
                    self.plane.ladder.unpinServed(sv.pinned.?);
                }
                stats.ipc_fast_path_serves.inc();
                stats.ipc_read_arena_copy_bytes.add(sv.n);
                stats.ipc_ops_read.inc();
                stats.ipc_bytes_out.add(sv.n);
                c.complete(@intCast(sv.n));
            },
            .eof => {
                stats.ipc_fast_path_serves.inc();
                stats.ipc_ops_read.inc();
                c.complete(0);
            },
            .demote => |why| {
                switch (why) {
                    .contended => stats.ipc_fast_path_lock_demotions.inc(),
                    .miss, .write_side => stats.ipc_fast_path_miss_demotions.inc(),
                    .pre_screen => stats.ipc_fast_path_policy_demotions.inc(),
                }
                // The guard is already dropped (structural in the ladder):
                // the FULL handler on a lane homed on the arena's node.
                stats.ipc_read_handoffs.inc();
                self.spawnOnLane(s, c, readHandoff, .{ self, op, c });
            },
        }
    }

    /// A handoff onto a FUSE-class lane homed on the session's node; a lane
    /// at capacity walks the rotation; every lane refusing is EAGAIN to the
    /// client (honest backpressure, counted).
    fn spawnOnLane(self: *DataPlaneSink, s: *Session, c: Completion, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) void {
        var attempt: u8 = 0;
        while (attempt < HANDOFF_LANE_ATTEMPTS) : (attempt += 1) {
            const lane = self.sqz.homeLane(null, if (attempt == 0) s.node else null);
            self.sqz.spawnGroupOn(lane, &s.group, function, args) catch continue;
            return;
        }
        stats.ipc_handoff_refusals.inc();
        c.fail(.AGAIN);
    }

    /// Lanes tried before a handoff answers EAGAIN: the node's home, then
    /// two of the rotation (a third refusal is a saturated lane class).
    const HANDOFF_LANE_ATTEMPTS: u8 = 3;

    fn ctxFor(self: *DataPlaneSink, trace: *op_trace.OpTrace, window: []u8) filesystem.Ctx {
        return .{ .unique = 0, .uid = self.uid, .gid = self.gid, .pid = self.pid, .io = self.io, .trace = trace, .wd_slot = null, .window = window };
    }

    fn readHandoff(self: *DataPlaneSink, op: DataOp, c: Completion) void {
        var trace = op_trace.OpTrace.begin(0, .read);
        defer trace.finish();
        var ctx = self.ctxFor(&trace, op.window);
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        const r = self.fs.vtable.read(self.fs.impl, &ctx, .{ .ino = op.binding.ino, .fh = 0, .offset = op.desc.offset, .size = op.desc.len, .flags = if (op.binding.rights.odirect) o_direct else 0 }) catch |e| return c.fail(errors.toErrno(e));
        // The handler's window IS the arena: landed in place.
        stats.ipc_read_dest_serves.inc();
        stats.ipc_read_demote_bytes.add(r.len);
        stats.ipc_ops_read.inc();
        stats.ipc_bytes_out.add(r.len);
        c.complete(@intCast(r.len));
    }

    // ---- WRITES (§3) ----

    fn serveWrite(self: *DataPlaneSink, owner: u16, s: *Session, op: DataOp, key_ino: u64, c: Completion) void {
        const len = op.desc.len;
        if (len == 0) {
            stats.ipc_ops_write.inc();
            stats.ipc_write_handoffs.inc();
            return c.complete(0);
        }
        // The W1-shaped inline arm on MY direct-drive shard.
        if (self.direct_write) if (self.dd) |dd| {
            if (dd.tryInlineWrite(owner, op, key_ino, c)) return;
        };
        // Sever at dequeue: the ONE arena read, BEFORE the handoff moves.
        const sev: Severed = blk: {
            if (self.plane.wp.placedSeverEligible(key_ino, op.desc.offset, len, self.max_file_size)) {
                const bs = self.plane.block_size;
                switch (self.placed.sever(key_ino, @intCast(op.desc.offset / bs), @intCast(op.desc.offset % bs), op.window)) {
                    .placed => |p| break :blk .{ .placed = p },
                    .fallback => stats.ipc_placed_sever_fallbacks.inc(),
                }
            }
            const slab = self.pool.get() orelse return c.fail(.NOMEM);
            @memcpy(slab[0..len], op.window[0..len]);
            break :blk .{ .pooled = .{ .slab = slab, .len = len } };
        };
        stats.ipc_severs.inc();
        switch (sev) {
            // Placed handoffs wait for the sweep's flush (module doc).
            .placed => {
                const po = &self.per_owner[owner];
                if (po.n < po.pending.len) {
                    po.pending[po.n] = .{ .s = s, .op = op, .c = c, .sev = sev };
                    po.n += 1;
                    return;
                }
                self.spawnWriteHandoff(s, op, c, sev);
            },
            .pooled => self.spawnWriteHandoff(s, op, c, sev),
        }
    }

    fn spawnWriteHandoff(self: *DataPlaneSink, s: *Session, op: DataOp, c: Completion, sev: Severed) void {
        stats.ipc_write_handoffs.inc();
        var attempt: u8 = 0;
        while (attempt < HANDOFF_LANE_ATTEMPTS) : (attempt += 1) {
            const lane = self.sqz.homeLane(null, if (attempt == 0) s.node else null);
            self.sqz.spawnGroupOn(lane, &s.group, writeHandoff, .{ self, op, c, sev }) catch continue;
            return;
        }
        stats.ipc_handoff_refusals.inc();
        self.dropSevered(sev);
        c.fail(.AGAIN);
    }

    fn dropSevered(self: *DataPlaneSink, sev: Severed) void {
        switch (sev) {
            .pooled => |p| self.pool.put(p.slab),
            .placed => |p| self.placed.drop(p),
        }
    }

    /// The write handler on a lane with the severed payload as its source:
    /// the level-1 admission exactly as a kernel write, the same inode
    /// lock, the same arms (`kv_fs.write` → `WritePath.write`), the
    /// binding's `kill_priv` as `FUSE_WRITE_KILL_SUIDGID`, a `sync`
    /// durability binding's barrier BEFORE its DONE, the ledger, the
    /// completion, THEN the KD-11 invalidation.
    fn writeHandoff(self: *DataPlaneSink, op: DataOp, c: Completion, sev: Severed) void {
        defer self.dropSevered(sev);
        var trace = op_trace.OpTrace.begin(0, .write);
        defer trace.finish();
        var ctx = self.ctxFor(&trace, &.{});
        const nodeid = op.binding.ino;
        const end = op_guard.validateRange(op.desc.offset, op.desc.len, self.max_file_size) catch |e| return c.fail(errors.toErrno(e));
        const bytes = sev.bytes();
        const o_direct: u32 = @bitCast(linux.O{ .DIRECT = true });
        const written = blk: {
            var adm = op_guard.admitWrite(self.guards, self.io, self.probe, self.shared_enabled, nodeid, op.desc.offset, end);
            defer adm.release(self.io);
            const candidate: op_guard.Class = if (adm.shared_snapshot) |snap| op_guard.classify(snap.classifyInput(end)) else .entire_op;
            const class = adm.finalize(candidate);
            const placed_payload: ?write_admit.PlacedPayload = switch (sev) {
                .placed => |p| .{ .cell = p.cell, .rel = p.rel, .len = p.len, .token = p.gen, .seam = self.placedSeam() },
                .pooled => null,
            };
            const wr = self.fs.vtable.write(self.fs.impl, &ctx, .{
                .ino = nodeid,
                .fh = 0,
                .offset = op.desc.offset,
                .data = bytes,
                .placed = placed_payload,
                .class = class,
                .write_flags = if (op.binding.kill_priv) abi.FUSE_WRITE_KILL_SUIDGID else 0,
                .flags = if (op.binding.rights.odirect) o_direct else 0,
            }) catch |e| return c.fail(errors.toErrno(e));
            break :blk wr.size;
        };
        if (op.binding.durability == .sync) {
            self.fs.vtable.fsync(self.fs.impl, &ctx, .{ .ino = nodeid, .fh = 0, .datasync = false }) catch |e| return c.fail(errors.toErrno(e));
        }
        self.opens.noteWrite(nodeid, clock.monotonicNs()) catch {
            // The owed set could not grow (decision 0003 §AF): the drain
            // no close may cover runs before the DONE.
            self.fs.vtable.flush(self.fs.impl, &ctx, .{ .ino = nodeid, .fh = 0, .lock_owner = 0, .drain = true }) catch |e| return c.fail(errors.toErrno(e));
        };
        stats.ipc_ops_write.inc();
        stats.ipc_bytes_in.add(written);
        // A grow announces its size to the kernel BEFORE the ack (POSIX-8).
        _ = self.inval.onWrite(nodeid, op.desc.offset + written, clock.realtimeCoarseS());
        c.complete(@intCast(written));
    }

    /// The registry's face on the write path (`write_admit.PlacedSeam`).
    pub fn placedSeam(self: *DataPlaneSink) write_admit.PlacedSeam {
        return .{ .ctx = self.placed, .take = &placedTake, .adopted = &placedAdopted };
    }

    fn placedTake(ctx: *anyopaque, ino: u64, block: u32, rel: u32, ptr: [*]const u8, runs: *[write_admit.PlacedSeam.max_runs]placed.Run) ?write_admit.Adoption {
        const r: *placed.Registry = @ptrCast(@alignCast(ctx));
        const ad = r.takeForAdoption(ino, block, rel, ptr) orelse return null;
        @memcpy(runs[0..ad.n_runs], ad.runs());
        return .{ .cell = ad.cell, .n_runs = ad.n_runs };
    }

    fn placedAdopted(ctx: *anyopaque, ino: u64, block: u32, token: u64) bool {
        const r: *placed.Registry = @ptrCast(@alignCast(ctx));
        return r.adoptedByToken(.{ .ino = ino, .block = block }, token);
    }
};

// ---------------------------------------------------------------------------
// Tests: the dequeue loop over a RAM session with a stub client and sink.
// ---------------------------------------------------------------------------

const testing = std.testing;
const TestSession = session_mod.TestSession;
const ipc_stats = @import("../ipc.zig").stats;

fn sleepNs(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

/// A one-owner RAM registry (the host's face without the host).
const RamRegistry = struct {
    lock: std.Io.Mutex = .init,
    sessions: [8]?*Session = @splat(null),
    epoch: Atomic(u64) = .init(1),
    poisoned: Atomic(u32) = .init(0),

    fn add(self: *RamRegistry, s: *Session) void {
        self.lock.lockUncancelable(testing.io);
        defer self.lock.unlock(testing.io);
        for (&self.sessions) |*slot| if (slot.* == null) {
            slot.* = s;
            break;
        };
        _ = self.epoch.fetchAdd(1, .release);
    }

    fn remove(self: *RamRegistry, s: *Session) void {
        self.lock.lockUncancelable(testing.io);
        defer self.lock.unlock(testing.io);
        for (&self.sessions) |*slot| if (slot.* == s) {
            slot.* = null;
        };
        _ = self.epoch.fetchAdd(1, .release);
    }

    fn epochFn(ctx: *anyopaque) u64 {
        const self: *RamRegistry = @ptrCast(@alignCast(ctx));
        return self.epoch.load(.acquire);
    }

    fn collectFn(ctx: *anyopaque, _: u16, out: []*Session) usize {
        const self: *RamRegistry = @ptrCast(@alignCast(ctx));
        self.lock.lockUncancelable(testing.io);
        defer self.lock.unlock(testing.io);
        var n: usize = 0;
        for (self.sessions) |maybe| if (maybe) |s| {
            s.map.retain();
            out[n] = s;
            n += 1;
        };
        return n;
    }

    fn poisonFn(ctx: *anyopaque, s: *Session, reason: session_mod.PoisonReason) void {
        const self: *RamRegistry = @ptrCast(@alignCast(ctx));
        s.poison(reason);
        _ = self.poisoned.fetchAdd(1, .monotonic);
        self.remove(s);
    }

    fn registry(self: *RamRegistry) Registry {
        return .{ .ctx = self, .epoch = &epochFn, .collect = &collectFn, .poison = &poisonFn };
    }
};

/// The stub sink: READ fills `offset + i`, WRITE answers its length; every
/// op completes synchronously; a counted flush per sweep.
const StubSink = struct {
    served: Atomic(u64) = .init(0),
    flushes: Atomic(u64) = .init(0),
    reads: Atomic(u64) = .init(0),
    writes: Atomic(u64) = .init(0),

    fn serve(ctx: *anyopaque, _: u16, _: *Session, op: DataOp, c: Completion) void {
        const self: *StubSink = @ptrCast(@alignCast(ctx));
        _ = self.served.fetchAdd(1, .monotonic);
        switch (op.op) {
            .read => {
                for (op.window, 0..) |*b, i| b.* = @truncate(op.desc.offset + i);
                _ = self.reads.fetchAdd(1, .monotonic);
            },
            .write => _ = self.writes.fetchAdd(1, .monotonic),
            _ => unreachable,
        }
        c.complete(@intCast(op.desc.len));
    }

    fn flush(ctx: *anyopaque, _: u16) void {
        const self: *StubSink = @ptrCast(@alignCast(ctx));
        _ = self.flushes.fetchAdd(1, .monotonic);
    }

    fn sink(self: *StubSink) Sink {
        return .{ .ctx = self, .serve = &serve, .flush = &flush };
    }
};

const test_geometry: layout.Geometry = .{ .ring_entries = 64, .slots = 64, .arena_bytes = 64 * 4 * layout.PAGE_BYTES, .max_op_bytes = 4 * layout.PAGE_BYTES };

fn testOwner(reg: *RamRegistry, sk: *StubSink, stop: *Atomic(bool)) !*Owner {
    return Owner.init(testing.allocator, .{ .index = 0, .node = null, .cpus = null, .lanes = 1, .cores = 1, .spin = .{ .explicit_us = 0, .adaptive = false }, .max_sessions = 8 }, reg.registry(), sk.sink(), stop);
}

/// The client half over the RAM session: claim → fill → publish → push →
/// doorbell, then wait DONE (a bounded spin: the thread under test is the
/// one that completes).
const StubClient = struct {
    s: *Session,

    fn submit(self: StubClient, slot_idx: u32, op: layout.Op, binding: u64, offset: u64, len: u32) u64 {
        const slot = self.s.m.slot(slot_idx) catch unreachable;
        const gen = slot_core.tryClaim(slot).?;
        slot_core.publishDescriptor(slot, .{ .op = op, .binding = binding, .offset = offset, .len = len, .arena_off = @as(u64, slot_idx) * self.s.layout.slab });
        slot_core.stampIngress(slot, clock.monotonicNs());
        slot_core.publishSubmitted(slot);
        std.debug.assert(self.s.ring.push(slot_idx));
        _ = doorbell.ringSubmit(self.s.header());
        return gen;
    }

    fn awaitDone(self: StubClient, slot_idx: u32, gen: u64) !i64 {
        const slot = self.s.m.slot(slot_idx) catch unreachable;
        const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
        while (!slot_core.isDoneFor(slot, gen)) {
            if (clock.monotonicNs() > deadline) return error.TestTimeout;
            _ = futex.waitSharedTimeout(slot_core.word(slot), slot.core.state.load(.monotonic), std.time.ns_per_ms);
        }
        const r = slot_core.result(slot);
        slot_core.release(slot);
        return r;
    }
};

test "the dequeue loop over a RAM session: a session-less owner parks on its private word; admission wakes it; reads and writes are served through the sink with the completion order (ledger → result → DONE → doorbell); every slot returns FREE; the flush runs once per sweep; the heartbeat moves" {
    var reg: RamRegistry = .{};
    var sk: StubSink = .{};
    var stop: Atomic(bool) = .init(false);
    const o = try testOwner(&reg, &sk, &stop);
    defer o.deinit();
    try o.spawn();
    // Session-less: the thread parks on its private word (no busy loop).
    sleepNs(20 * std.time.ns_per_ms);
    const empty0 = stats.ipc_drain_empty_passes.load();
    sleepNs(20 * std.time.ns_per_ms);
    try testing.expect(stats.ipc_drain_empty_passes.load() - empty0 < 4);
    var ts = try TestSession.init(testing.allocator, test_geometry);
    const s = ts.s;
    _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
    reg.add(s);
    o.kick();
    const cl: StubClient = .{ .s = s };
    const seq0 = s.header().cqe.seq.load(.monotonic);
    // A read and a write served.
    const g1 = cl.submit(3, .read, 1, 4096, 2048);
    try testing.expectEqual(@as(i64, 2048), try cl.awaitDone(3, g1));
    const window = s.m.slotWindow(3) catch unreachable;
    for (window[0..2048], 0..) |v, i| try testing.expectEqual(@as(u8, @truncate(4096 + i)), v);
    const g2 = cl.submit(5, .write, 1, 0, 4096);
    try testing.expectEqual(@as(i64, 4096), try cl.awaitDone(5, g2));
    try testing.expectEqual(seq0 + 2, s.header().cqe.seq.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), s.inflight.load(.monotonic));
    try testing.expectEqual(@as(u64, 2), sk.served.load(.monotonic));
    try testing.expect(sk.flushes.load(.monotonic) >= 2);
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), reg.poisoned.load(.monotonic));
    try testing.expect(s.header().heartbeat.load(.monotonic) != 0);
    // Teardown: the registry drops the session; the owner releases its
    // share at the next refresh; the last share frees it.
    reg.remove(s);
    o.kick();
    stop.store(true, .release);
    o.kick();
    o.join();
    ts.deinit();
}

test "the validation on the dequeue thread: a dead binding, an oversize len, an out-of-window arena, an unknown op reject -EINVAL and the wrong direction -EBADF — each a counted, logged descriptor-reject tripwire through the ONE errno table" {
    var reg: RamRegistry = .{};
    var sk: StubSink = .{};
    var stop: Atomic(bool) = .init(false);
    const o = try testOwner(&reg, &sk, &stop);
    defer o.deinit();
    var ts = try TestSession.init(testing.allocator, test_geometry);
    defer ts.deinit();
    const s = ts.s;
    _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
    _ = s.bind(.{ .ino = 8, .rights = .{ .read = true } }, false, .mount).?;
    const rejects0 = stats.ipc_descriptor_rejects.load();
    // Five rejects, each its own log line (the per-site budget reset
    // between them), fired on THIS thread.
    log.testing.expectErrorLogs(5);
    defer log.testing.expectAllFired() catch unreachable;
    const Row = struct { desc: layout.SlotDescriptor, want: linux.E };
    const rows = [_]Row{
        .{ .desc = .{ .op = 1, .flags = 0, .binding = 9, .offset = 0, .len = 16, .arena_off = 0, .generation = 1 }, .want = .INVAL },
        .{ .desc = .{ .op = 1, .flags = 0, .binding = 1, .offset = 0, .len = test_geometry.max_op_bytes + 1, .arena_off = 0, .generation = 1 }, .want = .INVAL },
        .{ .desc = .{ .op = 1, .flags = 0, .binding = 1, .offset = 0, .len = 4096, .arena_off = test_geometry.arena_bytes - 100, .generation = 1 }, .want = .INVAL },
        .{ .desc = .{ .op = 9, .flags = 0, .binding = 1, .offset = 0, .len = 16, .arena_off = 0, .generation = 1 }, .want = .INVAL },
        .{ .desc = .{ .op = 2, .flags = 0, .binding = 2, .offset = 0, .len = 16, .arena_off = 0, .generation = 1 }, .want = .BADF },
    };
    for (rows, 0..) |row, i| {
        const idx: u32 = @intCast(i);
        const slot = s.m.slot(idx) catch unreachable;
        const gen = slot_core.tryClaim(slot).?;
        slot_core.publishSubmitted(slot);
        try testing.expect(slot_core.tryBeginServe(slot));
        try testing.expect(s.beginInflight());
        s.map.retain();
        stats.ipc_descriptor_rejects.resetLogBudgetForTest();
        o.serveValidated(s, row.desc, clock.monotonicNs(), .{ .session = s, .slot_index = idx, .latch = true });
        try testing.expect(slot_core.isDoneFor(slot, gen));
        try testing.expectEqual(-@as(i64, @backingInt(row.want)), slot_core.result(slot));
        slot_core.release(slot);
    }
    try testing.expectEqual(rejects0 + 5, stats.ipc_descriptor_rejects.load());
    try testing.expectEqual(@as(u64, 0), sk.served.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), s.inflight.load(.monotonic));
}

test "poison: a published slot index past the table, a non-SUBMITTED slot, and a resubmit past the in-flight bound each stop the drain with their reason (the generation bumps; the tripwire counts and logs); an honest full window never does" {
    var reg: RamRegistry = .{};
    var sk: StubSink = .{};
    var stop: Atomic(bool) = .init(false);
    const o = try testOwner(&reg, &sk, &stop);
    defer o.deinit();
    const poisoned0 = stats.ipc_sessions_poisoned.load();
    log.testing.expectErrorLogs(3);
    defer log.testing.expectAllFired() catch unreachable;
    // The three violations, drained on THIS thread.
    {
        var ts = try TestSession.init(testing.allocator, test_geometry);
        defer ts.deinit();
        const s = ts.s;
        _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
        // Out of range.
        try testing.expect(s.ring.push(999));
        try testing.expect(o.drain(s) == null);
        try testing.expectEqual(session_mod.PoisonReason.slot_out_of_range, o.last_poison);
        stats.ipc_sessions_poisoned.resetLogBudgetForTest();
        s.poison(o.last_poison);
        try testing.expectEqual(poisoned0 + 1, stats.ipc_sessions_poisoned.load());
        try testing.expectEqual(@as(u64, 2), s.header().generation.load(.acquire));
    }
    {
        var ts = try TestSession.init(testing.allocator, test_geometry);
        defer ts.deinit();
        const s = ts.s;
        // Not SUBMITTED (a FREE slot's index published).
        try testing.expect(s.ring.push(3));
        try testing.expect(o.drain(s) == null);
        try testing.expectEqual(session_mod.PoisonReason.slot_not_submitted, o.last_poison);
        stats.ipc_sessions_poisoned.resetLogBudgetForTest();
        s.poison(o.last_poison);
    }
    {
        var ts = try TestSession.init(testing.allocator, test_geometry);
        defer ts.deinit();
        const s = ts.s;
        _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
        // Over-admission: the ledger already holds every slot; one more
        // published index is a slot the client cannot own.
        var i: u32 = 0;
        while (i < test_geometry.slots) : (i += 1) try testing.expect(s.beginInflight());
        const slot = s.m.slot(0) catch unreachable;
        _ = slot_core.tryClaim(slot).?;
        slot_core.publishSubmitted(slot);
        try testing.expect(s.ring.push(0));
        try testing.expect(o.drain(s) == null);
        try testing.expectEqual(session_mod.PoisonReason.over_admission, o.last_poison);
        stats.ipc_sessions_poisoned.resetLogBudgetForTest();
        s.poison(o.last_poison);
        try testing.expectEqual(poisoned0 + 3, stats.ipc_sessions_poisoned.load());
    }
    // An honest full window on the owner thread: every slot submitted at
    // once, every one served, nothing poisoned.
    try o.spawn();
    {
        var ts = try TestSession.init(testing.allocator, test_geometry);
        const s = ts.s;
        _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
        reg.add(s);
        o.kick();
        const cl: StubClient = .{ .s = s };
        var gens: [64]u64 = undefined;
        var i: u32 = 0;
        while (i < 64) : (i += 1) gens[i] = cl.submit(i, .read, 1, i, 64);
        i = 0;
        while (i < 64) : (i += 1) try testing.expectEqual(@as(i64, 64), try cl.awaitDone(i, gens[i]));
        try testing.expectEqual(poisoned0 + 3, stats.ipc_sessions_poisoned.load());
        try testing.expectEqual(@as(u32, 0), reg.poisoned.load(.monotonic));
        reg.remove(s);
        o.kick();
        stop.store(true, .release);
        o.kick();
        o.join();
        ts.deinit();
    }
}

test "the park ladder has no lost wake: 20 k ops from a client thread with randomised gaps over a parking owner — every op completes within the 5 ms bound's many, no op is stranded, parks and wakes are counted" {
    var reg: RamRegistry = .{};
    var sk: StubSink = .{};
    var stop: Atomic(bool) = .init(false);
    const o = try testOwner(&reg, &sk, &stop);
    defer o.deinit();
    try o.spawn();
    var ts = try TestSession.init(testing.allocator, test_geometry);
    const s = ts.s;
    _ = s.bind(.{ .ino = 7, .rights = .{ .read = true, .write = true } }, false, .mount).?;
    reg.add(s);
    o.kick();
    const parks0 = ipc_stats.ipc_service_parks.load();
    const Client = struct {
        fn run(sess: *Session, failures: *Atomic(u32)) void {
            const cl: StubClient = .{ .s = sess };
            var prng = std.Random.DefaultPrng.init(0x9a7c);
            var i: u32 = 0;
            while (i < 20_000) : (i += 1) {
                const idx: u32 = i % 64;
                const gen = cl.submit(idx, .read, 1, i, 128);
                const t0 = clock.monotonicNs();
                const r = cl.awaitDone(idx, gen) catch {
                    _ = failures.fetchAdd(1, .monotonic);
                    return;
                };
                if (r != 128) _ = failures.fetchAdd(1, .monotonic);
                // A stranded op would wait the 5 s test bound; a parked one
                // wakes within the 5 ms bound plus scheduling.
                if (clock.monotonicNs() - t0 > std.time.ns_per_s) _ = failures.fetchAdd(1, .monotonic);
                // Randomised gaps straddle the spin/park boundary.
                const gap = prng.random().uintLessThan(u64, 40_000);
                if (gap > 20_000) sleepNs(gap);
            }
        }
    };
    var failures: Atomic(u32) = .init(0);
    const t = try std.Thread.spawn(.{}, Client.run, .{ s, &failures });
    t.join();
    try testing.expectEqual(@as(u32, 0), failures.load(.monotonic));
    try testing.expectEqual(@as(u64, 20_000), sk.reads.load(.monotonic));
    try testing.expect(ipc_stats.ipc_service_parks.load() > parks0);
    try testing.expect(stats.ipc_service_wakes.load() + stats.ipc_service_park_timeouts.load() > 0);
    for (s.m.slots()) |*sl| try testing.expectEqual(slot_core.FREE, sl.core.state.load(.monotonic));
    reg.remove(s);
    stop.store(true, .release);
    o.kick();
    o.join();
    ts.deinit();
}

test "/proc/stat's cpu line parses to (total, idle + iowait); garbage is null" {
    const r = parseProcStatCpu("cpu  100 0 50 800 20 0 10 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n").?;
    try testing.expectEqual(@as(u64, 980), r[0]);
    try testing.expectEqual(@as(u64, 820), r[1]);
    try testing.expect(parseProcStatCpu("intr 1 2 3") == null);
    try testing.expect(parseProcStatCpu("cpu  1 2") == null);
}

fn procStatCpuFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseProcStatCpu(buf[0..len]);
}

test "fuzz: the /proc/<pid>/stat cpu-field parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, procStatCpuFuzz, .{ .corpus = &.{ fuzzkit.slice("1 (a) b) S 0 0 0 0 0 0 0 0 0 0 11 22 0 0\n"), fuzzkit.slice("(") } });
}
