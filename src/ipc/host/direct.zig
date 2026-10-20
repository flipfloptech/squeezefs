//! The direct-drive lane (charter §7 "per-shard 512-entry io_uring with
//! `COOP_TASKRUN`, lane-scoped flush (+40 %/+28 %), reaper/drain fusion;
//! the W1-shaped write serves inline on the service thread under a
//! try-locked block stripe"; spec `docs/porting/il_daemon.md` §4). Linux.
//!
//! Shards = the service threads' width (`ipc/sizing.ilDrainLanesDefault`,
//! ONE function for both halves), so shard `i` is the lane of owner `i`:
//! every production submitter IS a lane owner and its own sweep-end flush
//! carries its SQEs. Per shard: one `Ring128` of `RING_ENTRIES`, a
//! pre-sized in-flight slab recycled through a free list (both submit
//! paths allocation-free), a `pending_submits` word the flush claims, a
//! `cq_gate` the two consumers try-lock, and the reaper `sqz-ipc-dd<N>`.
//!
//! WHY `COOP_TASKRUN` and NOT the FUSE rings' `SINGLE_ISSUER |
//! DEFER_TASKRUN`: a dd ring has TWO issuers — the service thread submits,
//! the reaper waits — so `SINGLE_ISSUER` is structurally violated and
//! `DEFER_TASKRUN` requires it; with NO flag the kernel delivers each
//! completion's task-work by `TWA_SIGNAL`, interrupting whichever thread
//! last touched the ring and running `io_handle_tw_list` under the ring's
//! `uring_lock` ON THE SERVICE THREAD — the funnel profile's 9.5 %
//! `get_signal → task_work_run → __mutex_lock` term; under `COOP_TASKRUN`
//! task-work runs only when a task enters the ring: the reaper's own
//! bounded wait, where it belongs. The trade: a CQE submitted by a service
//! thread posts only at that thread's NEXT ring entry — one sweep away.
//!
//! Lane-scoped flush: a service thread enters ONLY its own lane's ring. The
//! flush-ALL sweep put every service thread on every shard's kernel
//! `uring_lock` whenever that shard had unflushed SQEs — 31.2 % of service
//! cycles in `mutex_spin_on_owner`/`osq_lock` under `io_uring_enter` at
//! 32×32, 12 threads racing 12 rings; counted A-B-B-A (tcp devsub, 32-CPU,
//! rand-4k il, 60 s): 32×8 931.3 k vs 666.7 k (+39.7 %), 32×32 1,185.6 k
//! vs 926.6 k (+27.9 %), clat mean −28 %/−22 %. A straggler on a foreign
//! lane is carried by that lane's reaper on its 100 ms cadence.
//!
//! Reaper/drain fusion: after its flush the owning service thread
//! try-locks its lane's `cq_gate` and drains its own CQ INLINE — a
//! userspace CQ peek, zero syscall, zero context switch — where the
//! dedicated reaper paid an `io_uring_enter` wake + a context switch per
//! ~1.4 CQEs (0.72 enters/op + 2.36 ctx/op at the 32×8 ceiling shape); the
//! reaper stays the blocking BACKSTOP; `try_lock` never blocks (contention
//! means the reaper is already draining); other lanes' CQs are never
//! touched; the fusion is OFF while shutting down (teardown CQEs belong to
//! the reaper's drain contract). Lock order: `cq_gate` then `state`, never
//! the reverse.
//!
//! The reaper's wait is `submit_and_wait(1)` bounded by `EXT_ARG` 100 ms —
//! a liveness re-check cadence, not tuning (idle cost 10 wakes/s/shard).
//! MEM-7c, the stall/leak law: a failed enter while ops are in flight NEVER
//! exits the reaper — every pending op pins its session mapping and
//! kernel DMA may still land in it; retry at 1 ms (`ipc_dd_reap_stalls`);
//! past `REAP_STALL_LIMIT` consecutive stalls the in-flight entries are
//! LEAKED (their mapping shares never released: the pages stay mapped for
//! the process's life), their block guards released with the words
//! re-stabilized (decision 0002: never a wedged stripe) and their clients
//! answered `-EIO`, then the reaper exits. Unmapping under DMA is never an
//! option.
//!
//! THE W1-SHAPED WRITE INLINE (§4.5): an il write paid the ring hop AND the
//! full handler — on the engaged rand-4k row the handler lanes took 77.8 %
//! of CPU (46.4 µs/op) vs 7.8 % on the service threads, over half of it
//! bucket contention, clock ceremony and `clear_page_erms`, not write
//! work. Here the shape runs ON the service thread, device-true, no
//! handoff: `WritePath.beginDirectWrite` (the lock-free shape half → ONE
//! `BlockLocks.tryAcquire`, never a blocking acquire → the state half and
//! the §5.1 fence pair under the held guard) → the DMA source (the arena
//! window verbatim when 4 KiB-aligned — zero daemon copies; a racing
//! client scribble is torn CONTENT on the device, the client's own POSIX
//! hazard: the payload is uninterpreted bytes DMA'd exactly once — else a
//! pooled slab, ONE copy) → ONE `WRITE` SQE on MY shard with the guard
//! HELD (level 3 only; nothing acquired under it) → the CQE postlude
//! `finishDirectWrite` (re-stabilize under a new generation, the 4-tier
//! purge, the touched stamp, the ledger, the EIO law) → the ACK → the KD-11
//! invalidation. Every non-submitted outcome hands the guard back
//! (`abortDirectWrite`) and the write takes the sever → handoff path with
//! its clause counted.
//!
//! THE DIRECT-DRIVE READ (§4.2; `SQUEEZEFS_IL_DIRECT_READ`): a ring read
//! every tier missed paid a lane handoff — a fiber spawn, a lane wake and
//! the FULL read handler per cold 4 KiB read — where the FUSE worker
//! bridges the same block as `READ_FIXED(fd → slot)` on its own ring with
//! no lane at all (the whole 4k randread row of
//! `rung-10-localfs-comparison.md` is direct legs). Here the leg the warm
//! ladder resolved (`WarmLadder.resolveDirectLeg`: cold ∧ passthrough ∧
//! 4 KiB-aligned ∧ single block ∧ no write-side custody, the incarnation
//! snapshot taken BEFORE the DMA) becomes ONE `READ` SQE on MY shard with
//! the arena window as the destination — zero daemon copies — and the CQE
//! runs the leg's recheck (incarnation still ∧ the map still binds; charter
//! §6.3's validation after the bytes are in hand): valid ⇒ the ACK; else
//! the handler re-serves the op through the ordinary read handoff.
//!
//! Completion ordering: none is promised across ops — a slot completes
//! when ITS CQE is drained, the shard CQ is consumed in CQE order by
//! whichever consumer holds `cq_gate`; within one op the postlude precedes
//! the ACK, the doorbell follows. The libaio shim decomposes a batch per
//! iocb and `io_getevents` merges per-slot DONE states: the daemon sees one
//! ring op per iocb and knows no batch.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;

const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const thread_name = @import("../../core/thread_name.zig");
const knobs = @import("../../core/knobs.zig");
const futex = @import("../../sync/futex.zig");
const uring128 = @import("../../io/uring128.zig");
const open_table = @import("../../fuse/handler/open_table.zig");
const fsv = @import("../../fuse/transport/filesystem_vtable.zig");
const write_admit = @import("../../data/write_admit.zig");
const seams = @import("../../data/write_seams.zig");
const session_mod = @import("session.zig");
const severed_pool = @import("severed_pool.zig");
const inval_mod = @import("inval.zig");
const stats = @import("stats.zig").stats;

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host/direct is Linux only; gate the import on builtin.os.tag");
}

pub const Session = session_mod.Session;
pub const Completion = session_mod.Completion;
pub const DataOp = @import("service.zig").DataOp;

fn sleepNs(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

/// 512 in-flight ops per shard ≫ any observed per-lane governed depth
/// (t32qd32 offers ≤ 1024 across 8 service threads, each owning its own
/// shard); an SQ-full refusal falls back to the handoff — counted, never
/// stranded. The slab is pre-sized to it (~360 KiB/shard).
pub const RING_ENTRIES: u16 = 512;
/// The reaper's `EXT_ARG` bound: a liveness re-check cadence.
pub const REAPER_WAIT_NS: u64 = 100 * std.time.ns_per_ms;
/// MEM-7c: 5 000 × 1 ms of consecutive failed enters with ops in flight
/// before the leak arm — leak rather than unmap under DMA or hang the join.
pub const REAP_STALL_LIMIT: u32 = 5000;
pub const REAP_STALL_RETRY_NS: u64 = std.time.ns_per_ms;
/// The DMA grain: the arena is the source only when the window sits on it.
pub const DMA_ALIGN: usize = 4096;
/// The ring park's own SQEs (module doc): never a slab index (`>=
/// RING_ENTRIES`), below `ring_park.reserved_ud` (this ring has no wake READ).
pub const PARK_UD: u64 = std.math.maxInt(u64) - 2;
pub const PARK_CANCEL_UD: u64 = std.math.maxInt(u64) - 3;
/// The dd ring ladder: `COOP_TASKRUN`, then the plain setup (an EINVAL on
/// a kernel without the flag degrades once, loudly).
pub const ring_ladder = [_]u32{ linux.IORING_SETUP_COOP_TASKRUN, 0 };

pub const Config = struct {
    shards: u16,
    /// `SQUEEZEFS_IPC_DD_COOP_TASKRUN` (0 = signal-delivered task-work).
    coop_taskrun: bool,
    /// `SQUEEZEFS_IPC_DD_LANE_FLUSH` (0 = the reaper alone carries SQEs).
    lane_flush: bool,
    /// `SQUEEZEFS_IPC_DD_INLINE_REAP` (0 = the reaper alone drains).
    inline_reap: bool,
    /// `SQUEEZEFS_IPC_DD_RING_PARK` (0 = the owner parks on the doorbells
    /// alone and the reaper consumes every CQE).
    ring_park: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map, shards: u16) Config {
        return .{
            .shards = shards,
            .coop_taskrun = knobs.boolKnob("SQUEEZEFS_IPC_DD_COOP_TASKRUN", env),
            .lane_flush = knobs.boolKnob("SQUEEZEFS_IPC_DD_LANE_FLUSH", env),
            .inline_reap = knobs.boolKnob("SQUEEZEFS_IPC_DD_INLINE_REAP", env),
            .ring_park = knobs.boolKnob("SQUEEZEFS_IPC_DD_RING_PARK", env),
        };
    }
};

/// One in-flight op.
const Pending = struct {
    c: Completion,
    kind: union(enum) {
        write: struct {
            direct: write_admit.WritePath.DirectBegun,
            /// The bounce source (the unaligned leg); null = the arena itself.
            slab: ?severed_pool.Slab,
        },
        read: struct {
            /// The leg's post-DMA recheck (`warm_ladder.directLegRecheck`).
            hook: fsv.CompletionHook,
            /// The op as dequeued: the fallback's handoff re-runs the FULL
            /// handler from it.
            s: *Session,
            op: DataOp,
        },
    },
    len: u32,
    nodeid: u64,
    offset: u64,
    in_use: bool,
};

/// The sink's read handoff, for a direct read whose recheck failed: the
/// handler owns the truth (never lossy, never fabricating).
pub const ReadFallback = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, s: *Session, op: DataOp, c: Completion) void,
};

pub const Shard = struct {
    index: u16,
    engine: *Engine,
    ring: uring128.Ring128,
    /// The slab + the SQ (the submitter and the reaper's free-list push):
    /// `std.Io.Mutex`, the sanctioned short-section class between two OS
    /// threads (no fiber ever parks on it).
    state: std.Io.Mutex = .init,
    /// The CQ's one consumer at a time (the inline reap vs the reaper).
    cq_gate: std.Io.Mutex = .init,
    slab: []Pending,
    free: []u16,
    free_n: usize,
    /// SQEs taken and not yet flushed by a lane flush (claimed by swap).
    pending_submits: Atomic(u32) = .init(0),
    inflight: Atomic(u32) = .init(0),
    /// The ring park's `FUTEX_WAITV` is armed (its CQE not yet seen). A
    /// hint: a stale reading arms one extra waiter or cancels a finished
    /// one (ENOENT), never a lost wake.
    park_armed: Atomic(bool) = .init(false),
    /// The owned-set epoch the armed waiter's words belong to.
    park_epoch: u64 = 0,
    thread: ?std.Thread = null,

    fn init(self: *Shard, gpa: Allocator, engine: *Engine, index: u16, coop: bool) !void {
        var ring: ?uring128.Ring128 = null;
        var plain = false;
        for (ring_ladder) |flags| {
            if (flags != 0 and !coop) continue;
            ring = uring128.Ring128.init(RING_ENTRIES, flags) catch |e| switch (e) {
                error.InvalidArgument, error.NotSupported => {
                    plain = flags != 0;
                    continue;
                },
                else => return e,
            };
            break;
        }
        if (plain and coop) {
            stats.ipc_dd_plain_rings.inc();
            log.warn("ipc dd shard {d}: COOP_TASKRUN refused by the kernel; the plain setup delivers task-work by signal", .{index});
        }
        const r = ring orelse return error.NotSupported;
        self.* = .{ .index = index, .engine = engine, .ring = r, .slab = undefined, .free = undefined, .free_n = 0 };
        errdefer self.ring.deinit();
        self.slab = try gpa.alloc(Pending, RING_ENTRIES);
        errdefer gpa.free(self.slab);
        for (self.slab) |*p| p.in_use = false;
        self.free = try gpa.alloc(u16, RING_ENTRIES);
        var i: u16 = RING_ENTRIES;
        while (i > 0) : (i -= 1) {
            self.free[self.free_n] = i - 1;
            self.free_n += 1;
        }
    }

    fn deinit(self: *Shard, gpa: Allocator) void {
        gpa.free(self.free);
        gpa.free(self.slab);
        self.ring.deinit();
    }

    /// The owner's sweep-end flush: claim the pending tail, ONE enter.
    fn flush(self: *Shard) void {
        if (self.pending_submits.swap(0, .acq_rel) == 0) return;
        self.state.lockUncancelable(self.engine.io);
        defer self.state.unlock(self.engine.io);
        self.submitLocked();
        stats.ipc_dd_lane_flushes.inc();
    }

    /// `ring.submit()` with 3 retries on the transient enter classes; a
    /// failed enter is loud and the reaper's own enter carries the SQEs.
    fn submitLocked(self: *Shard) void {
        var attempt: u8 = 0;
        while (attempt < 3) : (attempt += 1) {
            _ = self.ring.submit() catch |e| switch (e) {
                error.Interrupted, error.WouldBlock, error.Busy => continue,
                else => {
                    log.warn("ipc dd shard {d}: io_uring_enter failed ({s}); the reaper carries the SQEs", .{ self.index, @errorName(e) });
                    return;
                },
            };
            return;
        }
    }

    /// The fusion: drain my own CQ if nobody holds the gate.
    fn inlineReap(self: *Shard) void {
        if (!self.cq_gate.tryLock()) return;
        defer self.cq_gate.unlock(self.engine.io);
        self.drainCq(&stats.ipc_dd_inline_reaps);
    }

    /// Under `cq_gate`: every posted CQE → its op's postlude. A pass that
    /// retires any op counts in `reaps` BEFORE its first postlude publishes
    /// (counted after, a waiter that saw its write done could read the
    /// count one short).
    fn drainCq(self: *Shard, reaps: *counters.ShardedCounter) void {
        var cqes: [64]uring128.Cqe32 = undefined;
        var counted = false;
        while (true) {
            const n = self.ring.copyCqes(&cqes, 0) catch break;
            if (n == 0) break;
            for (cqes[0..n]) |cqe| {
                // A fired waiter (its index, or -EAGAIN at arm) is spent; a
                // cancelled one was retired at its cancel, its successor armed.
                if (cqe.user_data == PARK_UD and cqe.res != -@as(i32, @backingInt(linux.E.CANCELED))) self.park_armed.store(false, .release);
                if (uring128.Tag.isReserved(cqe.user_data) or cqe.user_data >= RING_ENTRIES) continue;
                const idx: u16 = @intCast(cqe.user_data);
                const p = blk: {
                    self.state.lockUncancelable(self.engine.io);
                    defer self.state.unlock(self.engine.io);
                    const e = &self.slab[idx];
                    if (!e.in_use) break :blk null;
                    e.in_use = false;
                    self.free[self.free_n] = idx;
                    self.free_n += 1;
                    _ = self.inflight.fetchSub(1, .monotonic);
                    break :blk e.*;
                } orelse {
                    log.err("ipc dd shard {d}: a CQE for a vacant slab entry {d} (res {d}) was dropped", .{ self.index, idx, cqe.res });
                    continue;
                };
                if (!counted) reaps.inc();
                counted = true;
                switch (p.kind) {
                    .write => self.engine.finishWrite(p, cqe.res),
                    .read => self.engine.finishRead(p, cqe.res),
                }
            }
        }
    }

    /// `sqz-ipc-dd<N>`: the blocking backstop (module doc).
    fn reapLoop(self: *Shard) void {
        var name_buf: thread_name.CommBuf = undefined;
        var base_buf: [thread_name.comm_max]u8 = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, std.fmt.bufPrint(&base_buf, "sqz-ipc-dd{d}", .{self.index}) catch "sqz-ipc-dd");
        var stalls: u32 = 0;
        while (true) {
            if (self.engine.stop.load(.acquire) and self.inflight.load(.monotonic) == 0) return;
            // The unclaimed tail rides this enter.
            _ = self.pending_submits.swap(0, .acq_rel);
            const to_submit = blk: {
                self.state.lockUncancelable(self.engine.io);
                defer self.state.unlock(self.engine.io);
                break :blk self.ring.flushSq();
            };
            var ts: linux.kernel_timespec = .{ .sec = 0, .nsec = @intCast(REAPER_WAIT_NS) };
            const arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
            // The BACKSTOP posture (module doc): while the owner parks in
            // its ring, the reaper wakes only on a full CQ or the cadence —
            // never per CQE (a wake + a context switch per op, racing the
            // owner for the gate); the per-CQE wait returns for the control
            // arms (a foreign-lane straggler has no owner enter) and the
            // shutdown drain (the owner is gone). `RING_ENTRIES` = half the
            // CQ: an owner that stopped reaping shows as a full SQ of CQEs
            // (`u32::MAX` returns at once on 7.2 — measured, not clamped).
            const min_complete: u32 = if (self.engine.ownerParksInRing() and !self.engine.stop.load(.acquire)) RING_ENTRIES else 1;
            _ = self.ring.enter(to_submit, min_complete, linux.IORING_ENTER_GETEVENTS, &arg) catch |e| {
                if (e == error.Interrupted) continue;
                if (self.inflight.load(.monotonic) == 0) {
                    // Nothing pinned: the bound re-checks the stop flag.
                    sleepNs(REAP_STALL_RETRY_NS);
                    continue;
                }
                stalls += 1;
                stats.ipc_dd_reap_stalls.inc();
                if (stalls < REAP_STALL_LIMIT) {
                    sleepNs(REAP_STALL_RETRY_NS);
                    continue;
                }
                self.leakInflight(e);
                return;
            };
            stalls = 0;
            self.cq_gate.lockUncancelable(self.engine.io);
            defer self.cq_gate.unlock(self.engine.io);
            self.drainCq(&stats.ipc_dd_reaper_reaps);
        }
    }

    /// MEM-7c's terminal arm (module doc).
    fn leakInflight(self: *Shard, e: anyerror) void {
        const io = self.engine.io;
        self.cq_gate.lockUncancelable(io);
        defer self.cq_gate.unlock(io);
        self.state.lockUncancelable(io);
        defer self.state.unlock(io);
        var leaked: u32 = 0;
        for (self.slab) |*p| {
            if (!p.in_use) continue;
            p.in_use = false;
            leaked += 1;
            // The guard back with the word re-stabilized; the client -EIO;
            // the mapping share stays out for the process's life.
            switch (p.kind) {
                .write => |w| self.engine.wp.abortDirectWrite(self.engine.io, w.direct),
                .read => {},
            }
            const slot = p.c.session.m.slot(p.c.slot_index) catch unreachable;
            _ = p.c.session.inflight.fetchSub(1, .monotonic);
            @import("../slot_core.zig").completeAndWake(slot, -@as(i64, @backingInt(linux.E.IO)));
        }
        stats.ipc_dd_leaked_inflight.add(leaked);
        log.err("ipc dd shard {d}: io_uring_enter failed {d} times in a row ({s}) with {d} op(s) in flight — their arena pages stay mapped for the process's life (never an unmap under DMA), their clients answered -EIO; the reaper exits", .{ self.index, REAP_STALL_LIMIT, @errorName(e), leaked });
    }
};

pub const Engine = struct {
    gpa: Allocator,
    io: Io,
    wp: *write_admit.WritePath,
    vols: *const seams.Volumes,
    /// The device fd per volume ordinal (`nvme_dev.Device.directFd`).
    vol_fds: []const linux.fd_t,
    /// `registerFiles` took the table on every shard: SQEs name fixed indices.
    fixed_files: bool,
    pool: *severed_pool.SeveredPool,
    inval: *inval_mod.Invalidator,
    opens: ?*open_table.OpenTable,
    cfg: Config,
    latch: bool,
    shards: []Shard,
    stop: Atomic(bool) = .init(false),
    /// Set by the sink before `start` (the sink is built after the engine).
    read_fallback: ?ReadFallback = null,

    pub const Deps = struct {
        io: Io,
        wp: *write_admit.WritePath,
        vols: *const seams.Volumes,
        vol_fds: []const linux.fd_t,
        pool: *severed_pool.SeveredPool,
        inval: *inval_mod.Invalidator,
        opens: ?*open_table.OpenTable,
        cfg: Config,
        latch: bool,
    };

    /// Build every shard's ring (at HOST spawn — decision 0002 quirk 5: no
    /// mount-time work on the data path) and register the device fds.
    pub fn init(gpa: Allocator, d: Deps) !*Engine {
        const self = try gpa.create(Engine);
        errdefer gpa.destroy(self);
        const shards = try gpa.alloc(Shard, d.cfg.shards);
        errdefer gpa.free(shards);
        self.* = .{ .gpa = gpa, .io = d.io, .wp = d.wp, .vols = d.vols, .vol_fds = d.vol_fds, .fixed_files = true, .pool = d.pool, .inval = d.inval, .opens = d.opens, .cfg = d.cfg, .latch = d.latch, .shards = shards };
        var made: usize = 0;
        errdefer for (shards[0..made]) |*s| s.deinit(gpa);
        for (shards, 0..) |*s, i| {
            try s.init(gpa, self, @intCast(i), d.cfg.coop_taskrun);
            made += 1;
            if (d.vol_fds.len != 0) s.ring.registerFiles(d.vol_fds) catch {
                self.fixed_files = false;
            };
        }
        stats.ipc_dd_shards.set(d.cfg.shards);
        return self;
    }

    pub fn start(self: *Engine) std.Thread.SpawnError!void {
        for (self.shards) |*s| s.thread = try std.Thread.spawn(.{}, Shard.reapLoop, .{s});
    }

    /// Flag, then join every reaper — each drains its in-flight CQEs first,
    /// bounded by device latency and the 100 ms re-check.
    pub fn shutdown(self: *Engine) void {
        self.stop.store(true, .release);
        for (self.shards) |*s| if (s.thread) |t| {
            t.join();
            s.thread = null;
        };
    }

    pub fn deinit(self: *Engine) void {
        self.shutdown();
        for (self.shards) |*s| s.deinit(self.gpa);
        self.gpa.free(self.shards);
        stats.ipc_dd_shards.set(0);
        self.gpa.destroy(self);
    }

    /// Owner `i` rides shard `i % width` (1:1 by construction).
    pub fn shardOf(self: *Engine, owner: u16) *Shard {
        return &self.shards[owner % self.shards.len];
    }

    /// The owner's sweep-end flush + the inline reap on ITS shard alone.
    pub fn flushLane(self: *Engine, owner: u16) void {
        const s = self.shardOf(owner);
        if (self.cfg.lane_flush) s.flush();
        if (self.cfg.inline_reap and !self.stop.load(.acquire)) s.inlineReap();
    }

    /// The ring park is the owner's wait iff its own flush carries its SQEs
    /// and its own reap drains them (the shipped arms); under a control arm
    /// the reaper stays the per-CQE consumer.
    pub fn ownerParksInRing(self: *const Engine) bool {
        return self.cfg.lane_flush and self.cfg.inline_reap and self.cfg.ring_park;
    }

    /// Ops of owner `owner` in flight on its shard.
    pub fn inflightOf(self: *Engine, owner: u16) u32 {
        return self.shardOf(owner).inflight.load(.monotonic);
    }

    /// THE RING PARK (module doc): the owner with ops in flight waits for
    /// their CQEs AND its doorbells in ONE `io_uring_enter` — a `FUTEX_WAITV`
    /// SQE over the doorbell words (the park ladder's snapshot, the same
    /// `daemon_parked := 1` contract: a client rings the futex while it
    /// reads 1) submitted with any unflushed tail, then `GETEVENTS` for one
    /// CQE bounded by `bound_ns`. On return every posted CQE is drained
    /// here, on the owner's thread. The waiter PERSISTS across parks until
    /// it fires (a `futex_wake` wakes a queued waiter whatever the word's
    /// value, and a client only rings while `daemon_parked` reads 1, so a
    /// stale-valued waiter is exactly as deaf as a fresh one between parks
    /// and exactly as awake inside one — cancelling and re-arming per park
    /// cost two spurious CQEs that ended the next park at once: 2.9 parks
    /// per op measured); it is cancelled and re-armed when the owned set's
    /// `epoch` moved (its words may be unmapped). Null = nothing in flight:
    /// the caller's futex park.
    pub fn parkInRing(self: *Engine, owner: u16, waiters: []const futex.Waiter, epoch: u64, bound_ns: u64) ?futex.WaitManyVerdict {
        if (!self.ownerParksInRing() or self.stop.load(.acquire)) return null;
        const shard = self.shardOf(owner);
        if (shard.inflight.load(.monotonic) == 0) return null;
        std.debug.assert(waiters.len <= futex.wait_many_max);
        var ones: [futex.wait_many_max]linux.futex2_waitone = undefined;
        for (waiters, 0..) |w, i| ones[i] = .{ .val = w.expect, .uaddr = @intFromPtr(&w.word.raw), .flags = .{ .size = .U32, .private = false } };
        const to_submit = blk: {
            shard.state.lockUncancelable(self.io);
            defer shard.state.unlock(self.io);
            if (shard.park_armed.load(.acquire) and shard.park_epoch != epoch) {
                // The set moved under the armed waiter: retire it.
                if (shard.ring.getSqe()) |sqe| {
                    uring128.prepAsyncCancel(sqe, .{ .user_data = PARK_UD });
                    sqe.user_data = PARK_CANCEL_UD;
                    shard.park_armed.store(false, .release);
                }
            }
            if (waiters.len != 0 and !shard.park_armed.load(.acquire)) {
                const sqe = shard.ring.getSqe() orelse inner: {
                    shard.submitLocked();
                    break :inner shard.ring.getSqe() orelse return null;
                };
                uring128.prepFutexWaitv(sqe, ones[0..waiters.len]);
                sqe.user_data = PARK_UD;
                shard.park_epoch = epoch;
                shard.park_armed.store(true, .release);
            }
            _ = shard.pending_submits.swap(0, .acq_rel);
            break :blk shard.ring.flushSq();
        };
        stats.ipc_dd_ring_parks.inc();
        var ts: linux.kernel_timespec = .{ .sec = @intCast(bound_ns / std.time.ns_per_s), .nsec = @intCast(bound_ns % std.time.ns_per_s) };
        const arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
        // The kernel copied the waiters at submission: `ones` may die here.
        const entered: uring128.Ring128.Entered = shard.ring.enter(to_submit, 1, linux.IORING_ENTER_GETEVENTS, &arg) catch |e| blk: {
            if (e != error.Interrupted) {
                // The reaper's own enter carries the SQEs (its stall law
                // owns a failing ring); this park reads as timed.
                log.warn("ipc dd shard {d}: the ring park's io_uring_enter failed ({s})", .{ shard.index, @errorName(e) });
            }
            break :blk .{ .submitted = 0, .timed_out = e != error.Interrupted };
        };
        if (shard.cq_gate.tryLock()) {
            defer shard.cq_gate.unlock(self.io);
            shard.drainCq(&stats.ipc_dd_park_reaps);
        }
        return if (entered.timed_out) .timed_out else .returned;
    }

    /// The W1-shaped inline arm (module doc). True = taken (the completion
    /// is the CQE's); false = declined with its clause counted, the caller
    /// severs and hands off.
    pub fn tryInlineWrite(self: *Engine, owner: u16, op: DataOp, key_ino: u64, c: Completion) bool {
        const len = op.desc.len;
        const verdict = self.wp.beginDirectWrite(self.io, key_ino, op.desc.offset, len) catch |e| {
            // The fence pair's device verdict is terminal for THIS write.
            c.fail(errors.toErrno(e));
            return true;
        };
        const d = switch (verdict) {
            .declined => |why| {
                switch (why) {
                    .shape => stats.ipc_dd_write_ineligible_shape.inc(),
                    .block_lock => stats.ipc_dd_write_ineligible_block_lock.inc(),
                    .custody => stats.ipc_dd_write_ineligible_custody.inc(),
                }
                stats.ipc_dd_write_fallbacks.inc();
                return false;
            },
            .begun => |b| b,
        };
        const vol = self.vols.byTag(d.begun.vol_tag) orelse {
            self.wp.abortDirectWrite(self.io, d);
            stats.ipc_dd_write_ineligible_backend.inc();
            stats.ipc_dd_write_fallbacks.inc();
            return false;
        };
        if (vol.ordinal >= self.vol_fds.len) {
            self.wp.abortDirectWrite(self.io, d);
            stats.ipc_dd_write_ineligible_backend.inc();
            stats.ipc_dd_write_fallbacks.inc();
            return false;
        }
        // The DMA source: the arena verbatim when aligned, else ONE copy.
        var slab: ?severed_pool.Slab = null;
        var src: []const u8 = op.window[0..len];
        if (@intFromPtr(src.ptr) % DMA_ALIGN != 0) {
            const sl = self.pool.get() orelse {
                self.wp.abortDirectWrite(self.io, d);
                stats.ipc_dd_write_ineligible_backend.inc();
                stats.ipc_dd_write_fallbacks.inc();
                return false;
            };
            @memcpy(sl[0..len], src);
            slab = sl;
            src = sl[0..len];
        }
        const shard = self.shardOf(owner);
        {
            shard.state.lockUncancelable(self.io);
            defer shard.state.unlock(self.io);
            if (shard.free_n == 0) return self.refuse(d, slab);
            const sqe = shard.ring.getSqe() orelse blk: {
                // SQ full: flush once, retry once, else refuse.
                shard.submitLocked();
                break :blk shard.ring.getSqe() orelse return self.refuse(d, slab);
            };
            shard.free_n -= 1;
            const idx = shard.free[shard.free_n];
            const file: uring128.File = if (self.fixed_files) .{ .fixed = vol.ordinal } else .{ .raw = self.vol_fds[vol.ordinal] };
            uring128.prepWrite(sqe, file, src, d.begun.dest);
            sqe.user_data = idx;
            shard.slab[idx] = .{ .c = c, .kind = .{ .write = .{ .direct = d, .slab = slab } }, .len = len, .nodeid = op.binding.ino, .offset = op.desc.offset, .in_use = true };
            _ = shard.inflight.fetchAdd(1, .monotonic);
        }
        _ = shard.pending_submits.fetchAdd(1, .release);
        return true;
    }

    /// The direct-drive READ (spec §4.2; `SQUEEZEFS_IL_DIRECT_READ`): the
    /// leg the warm ladder resolved after every tier missed — `{fd, off,
    /// len}` of a cold, passthrough, 4 KiB-aligned single-block window —
    /// becomes ONE `READ` SQE on MY shard with the arena window as the DMA
    /// destination (the ladder validated the window against the arena;
    /// `len == op.desc.len` by the leg's contract). The CQE runs the leg's
    /// recheck (incarnation ∧ binding, after the bytes are in hand) and
    /// completes `len`, or hands the op to the handler. True = taken;
    /// false = refused before any DMA (no slab entry / no SQE), counted —
    /// the caller hands off as if no leg had been offered.
    pub fn tryDirectRead(self: *Engine, owner: u16, s: *Session, op: DataOp, leg: fsv.DirectLeg, c: Completion) bool {
        const hook = leg.hook orelse return false;
        if (leg.len != op.desc.len or leg.len > op.window.len) return false;
        // The arena page is the DMA destination only on the device grain.
        if (@intFromPtr(op.window.ptr) % DMA_ALIGN != 0) return false;
        // The leg names a raw device fd; the shard registered the same
        // table at init, so the fixed index is the fd's ordinal.
        const file: uring128.File = blk: {
            if (self.fixed_files) for (self.vol_fds, 0..) |fd, i| {
                if (fd == leg.fd) break :blk .{ .fixed = @intCast(i) };
            };
            break :blk .{ .raw = leg.fd };
        };
        const shard = self.shardOf(owner);
        {
            shard.state.lockUncancelable(self.io);
            defer shard.state.unlock(self.io);
            if (shard.free_n == 0) {
                stats.ipc_dd_read_refusals.inc();
                return false;
            }
            const sqe = shard.ring.getSqe() orelse blk: {
                shard.submitLocked();
                break :blk shard.ring.getSqe() orelse {
                    stats.ipc_dd_read_refusals.inc();
                    return false;
                };
            };
            shard.free_n -= 1;
            const idx = shard.free[shard.free_n];
            uring128.prepRead(sqe, file, op.window[0..leg.len], leg.off);
            sqe.user_data = idx;
            shard.slab[idx] = .{ .c = c, .kind = .{ .read = .{ .hook = hook, .s = s, .op = op } }, .len = leg.len, .nodeid = op.binding.ino, .offset = op.desc.offset, .in_use = true };
            _ = shard.inflight.fetchAdd(1, .monotonic);
        }
        _ = shard.pending_submits.fetchAdd(1, .release);
        return true;
    }

    /// The read's CQE postlude: exact ∧ the recheck holds ⇒ the bytes in the
    /// arena are the block's, the ACK; else the handler re-serves the op
    /// (the window is rewritten in full: a torn DMA is never surfaced).
    fn finishRead(self: *Engine, p: Pending, res: i32) void {
        const r = p.kind.read;
        const exact = res == @as(i32, @intCast(p.len));
        if (r.hook.run(exact)) {
            stats.ipc_dd_read_serves.inc();
            stats.ipc_dd_read_bytes.add(p.len);
            stats.ipc_read_arena_dma_bytes.add(p.len);
            stats.ipc_ops_read.inc();
            stats.ipc_bytes_out.add(p.len);
            p.c.complete(@intCast(p.len));
            return;
        }
        stats.ipc_dd_read_fallbacks_post.inc();
        const fb = self.read_fallback orelse {
            // No handler to fall back to (a lab): the client's EIO.
            p.c.fail(.IO);
            return;
        };
        fb.call(fb.ctx, r.s, r.op, p.c);
    }

    /// No slab entry or no SQE: the guard back, the handoff path.
    fn refuse(self: *Engine, d: write_admit.WritePath.DirectBegun, slab: ?severed_pool.Slab) bool {
        if (slab) |sl| self.pool.put(sl);
        self.wp.abortDirectWrite(self.io, d);
        stats.ipc_dd_write_ineligible_sq_full.inc();
        stats.ipc_dd_write_fallbacks.inc();
        return false;
    }

    /// The CQE postlude (module doc): exact ⇒ the patch's postlude, the
    /// ledger, the ACK, the invalidation; short or failed ⇒ the EIO law.
    fn finishWrite(self: *Engine, p: Pending, res: i32) void {
        const w = p.kind.write;
        const dma: seams.DmaError!void = if (res == @as(i32, @intCast(p.len))) {} else error.InputOutput;
        if (self.wp.finishDirectWrite(self.io, w.direct, dma)) {
            stats.ipc_dd_inline_serves.inc();
            stats.ipc_dd_write_bytes.add(p.len);
            stats.ipc_ops_write.inc();
            stats.ipc_bytes_in.add(p.len);
            if (self.opens) |o| o.noteDurableWrite(p.nodeid);
            // A grow announces its size to the kernel BEFORE the ack (POSIX-8).
            _ = self.inval.onWrite(p.nodeid, p.offset + p.len, clock.realtimeCoarseS());
            p.c.complete(@intCast(p.len));
        } else |e| {
            stats.ipc_dd_write_dma_errors.inc();
            p.c.fail(errors.toErrno(e));
        }
        if (w.slab) |sl| self.pool.put(sl);
    }

    pub fn inflightTotal(self: *Engine) u32 {
        var n: u32 = 0;
        for (self.shards) |*s| n += s.inflight.load(.monotonic);
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests: the lane over a RAM write path (the `write_testing` fixture) with
// a scratch FILE as the device the ring DMAs to.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("../../data/write_testing.zig");
const layout_mod = @import("../../data/layout.zig");
const ipc_layout = @import("../layout.zig");
const slot_core = @import("../slot_core.zig");

const Lab = struct {
    f: *wt.Fixture,
    stripes: layout_mod.MetaStripes = .{},
    wp: *write_admit.WritePath,
    pool: severed_pool.SeveredPool,
    inval: inval_mod.Invalidator,
    fd: linux.fd_t,
    path_buf: [96]u8 = undefined,
    path_len: usize = 0,

    fn init(gpa: Allocator) !*Lab {
        const l = try gpa.create(Lab);
        errdefer gpa.destroy(l);
        l.f = try wt.Fixture.init(gpa, 32);
        errdefer l.f.deinit();
        l.stripes = .{};
        const cfg: write_admit.Config = .{
            .block_size = wt.BS,
            .patch_max = wt.BS / 8,
            .overlay = .{ .block_size = wt.BS, .patch_max = wt.BS / 8, .enabled = false },
            .rewrite_shadow = true,
            .parked_cap = 8,
            .depth_override = .sync_inline,
            .fsync = .{},
        };
        l.wp = try write_admit.WritePath.init(gpa, cfg, .{ .io = testing.io, .vols = &l.f.vols, .device = l.f.dev.seam(), .map = l.f.map.seam(), .free = l.f.frees.seam(), .stripes = &l.stripes, .budget_bytes = 1 << 30 });
        errdefer l.wp.deinit(testing.io);
        l.pool = try severed_pool.SeveredPool.init(gpa, wt.BS, 8);
        errdefer l.pool.deinit();
        l.inval = try inval_mod.Invalidator.init(gpa, 64, .{ .ctx = null, .call = &noInval, .sync = &OrderProbe.sync }, 1000);
        errdefer l.inval.deinit(gpa);
        _ = linux.mkdir("/build/tmp", 0o755);
        const p = try std.fmt.bufPrintSentinel(&l.path_buf, "/build/tmp/sqz-dd-lane-{d}.img", .{linux.getpid()}, 0);
        l.path_len = p.len;
        const rc = linux.open(p, .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
        l.fd = @intCast(rc);
        if (linux.errno(linux.ftruncate(l.fd, 32 * wt.BS)) != .SUCCESS) return error.CannotSizeDevice;
        return l;
    }

    fn noInval(_: ?*anyopaque, _: u64, _: i64, _: i64) error{QueueFull}!void {}

    fn deinit(self: *Lab, gpa: Allocator) void {
        _ = linux.close(self.fd);
        _ = linux.unlink(self.path_buf[0..self.path_len :0]);
        self.inval.deinit(gpa);
        self.pool.deinit();
        self.wp.deinit(testing.io);
        self.f.deinit();
        gpa.destroy(self);
    }

    fn readBack(self: *Lab, off: u64, out: []u8) !void {
        const n = linux.pread(self.fd, out.ptr, out.len, @intCast(off));
        if (linux.errno(n) != .SUCCESS or n != out.len) return error.ShortRead;
    }
};

/// The lab's awaited size announcement: records whether the write under
/// watch was already ACKED (its slot result published) when the kernel was
/// told the size — the order POSIX-8 forbids.
const OrderProbe = struct {
    var slot: ?*const ipc_layout.IpcSlot = null;
    var gen: u64 = 0;
    var fired: u32 = 0;
    var acked_first: bool = false;

    fn sync(_: ?*anyopaque, _: u64) void {
        fired += 1;
        if (slot) |sl| if (slot_core.isDoneFor(sl, gen)) {
            acked_first = true;
        };
    }
};

const lab_geometry: ipc_layout.Geometry = .{ .ring_entries = 16, .slots = 16, .arena_bytes = 16 * 4 * ipc_layout.PAGE_BYTES, .max_op_bytes = 4 * ipc_layout.PAGE_BYTES };

fn submitWrite(s: *Session, idx: u32, binding: u64, offset: u64, len: u32, fill: u8) !struct { op: DataOp, c: Completion, gen: u64 } {
    const slot = try s.m.slot(idx);
    const gen = slot_core.tryClaim(slot).?;
    const window = try s.m.slotWindow(idx);
    @memset(window[0..len], fill);
    slot_core.publishDescriptor(slot, .{ .op = .write, .binding = binding, .offset = offset, .len = len, .arena_off = @as(u64, idx) * s.layout.slab });
    slot_core.publishSubmitted(slot);
    try testing.expect(slot_core.tryBeginServe(slot));
    try testing.expect(s.beginInflight());
    s.map.retain();
    const desc = slot_core.snapshot(slot);
    return .{ .op = .{ .op = .write, .desc = desc, .binding = s.binding(binding).?.*, .t0_ns = clock.monotonicNs(), .window = window[0..len] }, .c = .{ .session = s, .slot_index = idx, .latch = true }, .gen = gen };
}

test "the dd lane: a W1-shaped 4 KiB overwrite serves INLINE — one SQE on the owner's shard, the lane-scoped flush, the inline reap under the try-locked gate — the bytes land on the device at the mapping, the word re-stabilizes, the client gets DONE; an unaligned shape, a stream-adjacent write and a held block stripe each decline with their clause and take the handoff path; the reaper backstop drains what the fusion did not" {
    const gpa = testing.allocator;
    const lab = try Lab.init(gpa);
    defer lab.deinit(gpa);
    const fds = [_]linux.fd_t{lab.fd};
    const engine = try Engine.init(gpa, .{ .io = testing.io, .wp = lab.wp, .vols = &lab.f.vols, .vol_fds = &fds, .pool = &lab.pool, .inval = &lab.inval, .opens = null, .cfg = .{ .shards = 2, .coop_taskrun = true, .lane_flush = true, .inline_reap = true, .ring_park = true }, .latch = true });
    defer engine.deinit();
    try engine.start();
    try testing.expectEqual(@as(u64, 2), stats.ipc_dd_shards.load());
    var ts = try session_mod.TestSession.init(gpa, lab_geometry);
    defer ts.deinit();
    const s = ts.s;
    _ = s.bind(.{ .ino = 2, .rights = .{ .read = true, .write = true } }, false, .mount).?;
    // A published block of ino 2 at block 0 on volume A, 0xAA on the "device".
    const b = try lab.f.publishBlock(2, 0, 0xAA);
    var img: [wt.BS]u8 = @splat(0xAA);
    try testing.expectEqual(@as(usize, wt.BS), linux.pwrite(lab.fd, &img, img.len, @intCast(b.offset)));
    const serves0 = stats.ipc_dd_inline_serves.load();
    const flushes0 = stats.ipc_dd_lane_flushes.load();
    const reaps0 = stats.ipc_dd_inline_reaps.load();
    const rmw0 = @import("../../data/sole_owner_patch.zig").stats.patch_edge_rmw_reads.load();
    // The W1 shape: page 3 of block 0, 4 KiB, aligned, non-extending.
    const w = try submitWrite(s, 1, 1, 3 * 4096, 4096, 0x55);
    // The size announcement (no bind seeded the high-water: a grow) lands
    // BEFORE the ack: when the kernel is told, the slot is not done yet.
    OrderProbe.slot = try s.m.slot(1);
    OrderProbe.gen = w.gen;
    OrderProbe.fired = 0;
    OrderProbe.acked_first = false;
    try testing.expect(engine.tryInlineWrite(0, w.op, 2, w.c));
    try testing.expectEqual(@as(u32, 1), engine.shards[0].inflight.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), engine.shards[1].inflight.load(.monotonic));
    // The sweep-end flush on MY lane, then the inline reap (bounded poll:
    // the file write completes in µs; the reaper would otherwise carry it).
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(1), w.gen) and clock.monotonicNs() < deadline) {
        engine.flushLane(0);
        sleepNs(100 * std.time.ns_per_us);
    }
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(1)));
    try testing.expectEqual(@as(u32, 1), OrderProbe.fired);
    try testing.expect(!OrderProbe.acked_first);
    OrderProbe.slot = null;
    slot_core.release(try s.m.slot(1));
    try testing.expectEqual(serves0 + 1, stats.ipc_dd_inline_serves.load());
    try testing.expect(stats.ipc_dd_lane_flushes.load() > flushes0);
    try testing.expect(stats.ipc_dd_inline_reaps.load() + stats.ipc_dd_reaper_reaps.load() > reaps0);
    try testing.expectEqual(rmw0, @import("../../data/sole_owner_patch.zig").stats.patch_edge_rmw_reads.load());
    var back: [4096]u8 = undefined;
    try lab.readBack(b.offset + 3 * 4096, &back);
    for (back) |v| try testing.expectEqual(@as(u8, 0x55), v);
    try lab.readBack(b.offset + 2 * 4096, &back);
    for (back) |v| try testing.expectEqual(@as(u8, 0xAA), v);
    try testing.expectEqual(@as(u32, 0), s.inflight.load(.monotonic));
    // The block stripe was released: a fresh try-lock takes it.
    const g = lab.wp.locks.tryAcquire(testing.io, 2, 0).?;
    // Declines: a held stripe.
    const fb0 = stats.ipc_dd_write_fallbacks.load();
    const w2 = try submitWrite(s, 2, 1, 5 * 4096, 4096, 0x66);
    try testing.expect(!engine.tryInlineWrite(0, w2.op, 2, w2.c));
    try testing.expectEqual(fb0 + 1, stats.ipc_dd_write_fallbacks.load());
    g.release(testing.io);
    w2.c.complete(0);
    slot_core.release(try s.m.slot(2));
    // An unaligned shape.
    const shape0 = stats.ipc_dd_write_ineligible_shape.load();
    const w3 = try submitWrite(s, 3, 1, 5 * 4096 + 100, 4096, 0x66);
    try testing.expect(!engine.tryInlineWrite(0, w3.op, 2, w3.c));
    try testing.expectEqual(shape0 + 1, stats.ipc_dd_write_ineligible_shape.load());
    w3.c.complete(0);
    slot_core.release(try s.m.slot(3));
    // Stream-adjacent: the previous inline write ended at 4 × 4096.
    const w4 = try submitWrite(s, 4, 1, 4 * 4096, 4096, 0x77);
    try testing.expect(!engine.tryInlineWrite(0, w4.op, 2, w4.c));
    try testing.expectEqual(shape0 + 2, stats.ipc_dd_write_ineligible_shape.load());
    w4.c.complete(0);
    slot_core.release(try s.m.slot(4));
    // The reaper backstop: a write whose flush the owner never follows
    // with a reap (lane flush only) completes through `sqz-ipc-dd1`.
    const reaper0 = stats.ipc_dd_reaper_reaps.load();
    const w5 = try submitWrite(s, 5, 1, 7 * 4096, 4096, 0x99);
    try testing.expect(engine.tryInlineWrite(1, w5.op, 2, w5.c));
    const deadline2 = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(5), w5.gen) and clock.monotonicNs() < deadline2) sleepNs(std.time.ns_per_ms);
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(5)));
    slot_core.release(try s.m.slot(5));
    try testing.expectEqual(reaper0 + 1, stats.ipc_dd_reaper_reaps.load());
    try lab.readBack(b.offset + 7 * 4096, &back);
    for (back) |v| try testing.expectEqual(@as(u8, 0x99), v);
    try testing.expectEqual(@as(u32, 0), engine.inflightTotal());
}

fn submitRead(s: *Session, idx: u32, binding: u64, offset: u64, len: u32) !struct { op: DataOp, c: Completion, gen: u64 } {
    const slot = try s.m.slot(idx);
    const gen = slot_core.tryClaim(slot).?;
    const window = try s.m.slotWindow(idx);
    @memset(window[0..len], 0);
    slot_core.publishDescriptor(slot, .{ .op = .read, .binding = binding, .offset = offset, .len = len, .arena_off = @as(u64, idx) * s.layout.slab });
    slot_core.publishSubmitted(slot);
    try testing.expect(slot_core.tryBeginServe(slot));
    try testing.expect(s.beginInflight());
    s.map.retain();
    const desc = slot_core.snapshot(slot);
    return .{ .op = .{ .op = .read, .desc = desc, .binding = s.binding(binding).?.*, .t0_ns = clock.monotonicNs(), .window = window[0..len] }, .c = .{ .session = s, .slot_index = idx, .latch = true }, .gen = gen };
}

/// The lab's leg recheck: records every call and answers what the test armed.
const RecheckProbe = struct {
    var calls: u32 = 0;
    var last_ok: bool = false;
    var verdict: bool = true;

    fn call(_: ?*anyopaque, _: *const [6]u64, ok: bool) bool {
        calls += 1;
        last_ok = ok;
        return verdict and ok;
    }
};

/// The lab's read fallback: completes the op with a sentinel length.
const FallbackProbe = struct {
    var calls: u32 = 0;

    fn call(_: *anyopaque, _: *Session, _: DataOp, c: Completion) void {
        calls += 1;
        c.complete(7);
    }
};

test "the dd lane's READ: the leg the ladder resolved becomes one READ SQE on the owner's shard with the arena window as the DMA destination — the device bytes land in the slot, the recheck runs with the bytes in hand, the client gets DONE(len); a failed recheck hands the op to the read fallback (nothing fabricated), and without a fallback the client reads -EIO" {
    const gpa = testing.allocator;
    const lab = try Lab.init(gpa);
    defer lab.deinit(gpa);
    const fds = [_]linux.fd_t{lab.fd};
    const engine = try Engine.init(gpa, .{ .io = testing.io, .wp = lab.wp, .vols = &lab.f.vols, .vol_fds = &fds, .pool = &lab.pool, .inval = &lab.inval, .opens = null, .cfg = .{ .shards = 2, .coop_taskrun = true, .lane_flush = true, .inline_reap = true, .ring_park = true }, .latch = true });
    defer engine.deinit();
    try engine.start();
    var ts = try session_mod.TestSession.init(gpa, lab_geometry);
    defer ts.deinit();
    const s = ts.s;
    _ = s.bind(.{ .ino = 2, .rights = .{ .read = true, .write = true, .odirect = true } }, false, .mount).?;
    const b = try lab.f.publishBlock(2, 0, 0xAA);
    // Page 5 of the block carries its own pattern on the "device".
    var img: [wt.BS]u8 = @splat(0xAA);
    @memset(img[5 * 4096 ..][0..4096], 0x5A);
    try testing.expectEqual(@as(usize, wt.BS), linux.pwrite(lab.fd, &img, img.len, @intCast(b.offset)));
    const serves0 = stats.ipc_dd_read_serves.load();
    const ops0 = stats.ipc_ops_read.load();
    const dma0 = stats.ipc_read_arena_dma_bytes.load();
    RecheckProbe.calls = 0;
    RecheckProbe.verdict = true;
    const hook: fsv.CompletionHook = .{ .ctx = null, .token = .{ 2, 0, 0, 0, 0, 0 }, .call = &RecheckProbe.call };
    const r = try submitRead(s, 1, 1, 5 * 4096, 4096);
    try testing.expect(engine.tryDirectRead(0, s, r.op, .{ .fd = lab.fd, .off = b.offset + 5 * 4096, .len = 4096, .hook = hook }, r.c));
    try testing.expectEqual(@as(u32, 1), engine.shards[0].inflight.load(.monotonic));
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(1), r.gen) and clock.monotonicNs() < deadline) {
        engine.flushLane(0);
        sleepNs(100 * std.time.ns_per_us);
    }
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(1)));
    try testing.expectEqual(@as(u32, 1), RecheckProbe.calls);
    try testing.expect(RecheckProbe.last_ok);
    for (r.op.window[0..4096]) |v| try testing.expectEqual(@as(u8, 0x5A), v);
    slot_core.release(try s.m.slot(1));
    try testing.expectEqual(serves0 + 1, stats.ipc_dd_read_serves.load());
    try testing.expectEqual(ops0 + 1, stats.ipc_ops_read.load());
    try testing.expectEqual(dma0 + 4096, stats.ipc_read_arena_dma_bytes.load());
    try testing.expectEqual(@as(u32, 0), s.inflight.load(.monotonic));
    // A moved block: the recheck refuses, the fallback owns the op.
    const post0 = stats.ipc_dd_read_fallbacks_post.load();
    RecheckProbe.verdict = false;
    FallbackProbe.calls = 0;
    engine.read_fallback = .{ .ctx = @ptrCast(engine), .call = &FallbackProbe.call };
    const r2 = try submitRead(s, 2, 1, 5 * 4096, 4096);
    try testing.expect(engine.tryDirectRead(1, s, r2.op, .{ .fd = lab.fd, .off = b.offset + 5 * 4096, .len = 4096, .hook = hook }, r2.c));
    const deadline2 = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(2), r2.gen) and clock.monotonicNs() < deadline2) {
        engine.flushLane(1);
        sleepNs(100 * std.time.ns_per_us);
    }
    try testing.expectEqual(@as(i64, 7), slot_core.result(try s.m.slot(2)));
    try testing.expectEqual(@as(u32, 1), FallbackProbe.calls);
    try testing.expectEqual(post0 + 1, stats.ipc_dd_read_fallbacks_post.load());
    try testing.expectEqual(serves0 + 1, stats.ipc_dd_read_serves.load());
    slot_core.release(try s.m.slot(2));
    // No fallback wired (a lab): the EIO law.
    engine.read_fallback = null;
    const r3 = try submitRead(s, 3, 1, 5 * 4096, 4096);
    try testing.expect(engine.tryDirectRead(0, s, r3.op, .{ .fd = lab.fd, .off = b.offset + 5 * 4096, .len = 4096, .hook = hook }, r3.c));
    const deadline3 = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(3), r3.gen) and clock.monotonicNs() < deadline3) {
        engine.flushLane(0);
        sleepNs(100 * std.time.ns_per_us);
    }
    try testing.expectEqual(-@as(i64, @backingInt(linux.E.IO)), slot_core.result(try s.m.slot(3)));
    slot_core.release(try s.m.slot(3));
    // A leg without a hook, or a window off the DMA grain, is never taken.
    const r4 = try submitRead(s, 4, 1, 5 * 4096, 4096);
    try testing.expect(!engine.tryDirectRead(0, s, r4.op, .{ .fd = lab.fd, .off = b.offset + 5 * 4096, .len = 4096, .hook = null }, r4.c));
    var shifted = r4.op;
    shifted.window = r4.op.window[512..];
    shifted.desc.len = 4096 - 512;
    try testing.expect(!engine.tryDirectRead(0, s, shifted, .{ .fd = lab.fd, .off = b.offset + 5 * 4096 + 512, .len = 4096 - 512, .hook = hook }, r4.c));
    r4.c.complete(0);
    slot_core.release(try s.m.slot(4));
    try testing.expectEqual(@as(u32, 0), engine.inflightTotal());
}

/// A doorbell rung from another thread after `delay_ns`.
const Ringer = struct {
    word: *futex.Word,
    delay_ns: u64,

    fn run(self: Ringer) void {
        sleepNs(self.delay_ns);
        _ = self.word.fetchAdd(1, .seq_cst);
        futex.wakeSharedAll(self.word);
    }
};

test "the dd lane's RING PARK: an owner with ops in flight waits in its ring — a doorbell rung while a read is stuck returns it inside the bound (the FUTEX_WAITV arm), the CQE that lands later is reaped at the next park on the owner's thread (never the reaper), nothing in flight answers null (the futex park), and the reaper's backstop posture still drains an op whose owner never parks" {
    const gpa = testing.allocator;
    const lab = try Lab.init(gpa);
    defer lab.deinit(gpa);
    const fds = [_]linux.fd_t{lab.fd};
    const engine = try Engine.init(gpa, .{ .io = testing.io, .wp = lab.wp, .vols = &lab.f.vols, .vol_fds = &fds, .pool = &lab.pool, .inval = &lab.inval, .opens = null, .cfg = .{ .shards = 2, .coop_taskrun = true, .lane_flush = true, .inline_reap = true, .ring_park = true }, .latch = true });
    defer engine.deinit();
    try engine.start();
    var ts = try session_mod.TestSession.init(gpa, lab_geometry);
    defer ts.deinit();
    const s = ts.s;
    _ = s.bind(.{ .ino = 2, .rights = .{ .read = true, .write = true, .odirect = true } }, false, .mount).?;
    var bell: futex.Word = .init(0);
    const waiters = [_]futex.Waiter{.{ .word = &bell, .expect = 0 }};
    // Nothing in flight: the futex park is the caller's.
    try testing.expect(engine.parkInRing(0, &waiters, 1, 50 * std.time.ns_per_ms) == null);
    // A read STUCK on a pipe (the device of this test), in flight on shard 0.
    var pipe_fds: [2]linux.fd_t = undefined;
    try testing.expectEqual(@as(usize, 0), linux.pipe2(&pipe_fds, .{ .CLOEXEC = true }));
    defer _ = linux.close(pipe_fds[0]);
    defer _ = linux.close(pipe_fds[1]);
    RecheckProbe.calls = 0;
    RecheckProbe.verdict = true;
    const hook: fsv.CompletionHook = .{ .ctx = null, .token = @splat(0), .call = &RecheckProbe.call };
    const r = try submitRead(s, 1, 1, 0, 4096);
    const shard = &engine.shards[0];
    {
        shard.state.lockUncancelable(testing.io);
        defer shard.state.unlock(testing.io);
        const sqe = shard.ring.getSqe().?;
        shard.free_n -= 1;
        const idx = shard.free[shard.free_n];
        uring128.prepRead(sqe, .{ .raw = pipe_fds[0] }, r.op.window[0..4096], 0);
        sqe.user_data = idx;
        shard.slab[idx] = .{ .c = r.c, .kind = .{ .read = .{ .hook = hook, .s = s, .op = r.op } }, .len = 4096, .nodeid = 2, .offset = 0, .in_use = true };
        _ = shard.inflight.fetchAdd(1, .monotonic);
    }
    _ = shard.pending_submits.fetchAdd(1, .release);
    engine.flushLane(0);
    try testing.expectEqual(@as(u32, 1), shard.inflight.load(.monotonic));
    // The ring park, rung by a doorbell 20 ms in: back well inside the
    // 2 s bound with the read still stuck.
    const parks0 = stats.ipc_dd_ring_parks.load();
    const reaper0 = stats.ipc_dd_reaper_reaps.load();
    const ringer = try std.Thread.spawn(.{}, Ringer.run, .{Ringer{ .word = &bell, .delay_ns = 20 * std.time.ns_per_ms }});
    const t0 = clock.monotonicNs();
    const v = engine.parkInRing(0, &waiters, 1, 2 * std.time.ns_per_s).?;
    const waited = clock.monotonicNs() - t0;
    ringer.join();
    try testing.expectEqual(futex.WaitManyVerdict.returned, v);
    try testing.expect(waited < std.time.ns_per_s);
    try testing.expectEqual(parks0 + 1, stats.ipc_dd_ring_parks.load());
    try testing.expectEqual(@as(u32, 1), shard.inflight.load(.monotonic));
    try testing.expect(!slot_core.isDoneFor(try s.m.slot(1), r.gen));
    // The device answers: the CQE is reaped by the OWNER's next park.
    var page: [4096]u8 = @splat(0x3C);
    try testing.expectEqual(page.len, linux.write(pipe_fds[1], &page, page.len));
    const park_reaps0 = stats.ipc_dd_park_reaps.load();
    const waiters2 = [_]futex.Waiter{.{ .word = &bell, .expect = bell.load(.seq_cst) }};
    const v2 = engine.parkInRing(0, &waiters2, 1, 2 * std.time.ns_per_s).?;
    try testing.expectEqual(futex.WaitManyVerdict.returned, v2);
    try testing.expect(slot_core.isDoneFor(try s.m.slot(1), r.gen));
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(1)));
    try testing.expectEqual(park_reaps0 + 1, stats.ipc_dd_park_reaps.load());
    try testing.expectEqual(reaper0, stats.ipc_dd_reaper_reaps.load());
    for (r.op.window[0..4096]) |b| try testing.expectEqual(@as(u8, 0x3C), b);
    slot_core.release(try s.m.slot(1));
    try testing.expectEqual(@as(u32, 0), shard.inflight.load(.monotonic));
    // The waiter armed at the second park persists (no cancel churn): a
    // park with nothing in flight is null whatever is armed.
    try testing.expect(shard.park_armed.load(.acquire));
    try testing.expectEqual(@as(u64, 1), shard.park_epoch);
    try testing.expect(engine.parkInRing(0, &waiters2, 1, 50 * std.time.ns_per_ms) == null);
    // The owned set moved (epoch 2) with a read stuck again: the stale
    // waiter is cancelled and a fresh one armed in the same enter; the
    // cancel's CQE ends that park, the new waiter stays armed and answers
    // the next doorbell.
    const r5 = try submitRead(s, 5, 1, 0, 4096);
    {
        shard.state.lockUncancelable(testing.io);
        defer shard.state.unlock(testing.io);
        const sqe = shard.ring.getSqe().?;
        shard.free_n -= 1;
        const idx = shard.free[shard.free_n];
        uring128.prepRead(sqe, .{ .raw = pipe_fds[0] }, r5.op.window[0..4096], 0);
        sqe.user_data = idx;
        shard.slab[idx] = .{ .c = r5.c, .kind = .{ .read = .{ .hook = hook, .s = s, .op = r5.op } }, .len = 4096, .nodeid = 2, .offset = 0, .in_use = true };
        _ = shard.inflight.fetchAdd(1, .monotonic);
    }
    _ = shard.pending_submits.fetchAdd(1, .release);
    const waiters3 = [_]futex.Waiter{.{ .word = &bell, .expect = bell.load(.seq_cst) }};
    _ = engine.parkInRing(0, &waiters3, 2, 200 * std.time.ns_per_ms).?;
    try testing.expect(shard.park_armed.load(.acquire));
    try testing.expectEqual(@as(u64, 2), shard.park_epoch);
    const ringer2 = try std.Thread.spawn(.{}, Ringer.run, .{Ringer{ .word = &bell, .delay_ns = 20 * std.time.ns_per_ms }});
    const t2 = clock.monotonicNs();
    const v3 = engine.parkInRing(0, &waiters3, 2, 2 * std.time.ns_per_s).?;
    ringer2.join();
    try testing.expectEqual(futex.WaitManyVerdict.returned, v3);
    try testing.expect(clock.monotonicNs() - t2 < std.time.ns_per_s);
    try testing.expect(!shard.park_armed.load(.acquire));
    try testing.expectEqual(page.len, linux.write(pipe_fds[1], &page, page.len));
    _ = engine.parkInRing(0, &waiters3, 2, 2 * std.time.ns_per_s).?;
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(5)));
    slot_core.release(try s.m.slot(5));
    try testing.expectEqual(@as(u32, 0), shard.inflight.load(.monotonic));
    // The backstop: an owner that submits on shard 1 and never parks or
    // reaps is drained by `sqz-ipc-dd1` within its cadence.
    const b = try lab.f.publishBlock(2, 0, 0xAA);
    var img: [wt.BS]u8 = @splat(0xAA);
    try testing.expectEqual(@as(usize, wt.BS), linux.pwrite(lab.fd, &img, img.len, @intCast(b.offset)));
    const w = try submitWrite(s, 2, 1, 3 * 4096, 4096, 0x55);
    try testing.expect(engine.tryInlineWrite(1, w.op, 2, w.c));
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!slot_core.isDoneFor(try s.m.slot(2), w.gen) and clock.monotonicNs() < deadline) sleepNs(std.time.ns_per_ms);
    try testing.expectEqual(@as(i64, 4096), slot_core.result(try s.m.slot(2)));
    try testing.expectEqual(reaper0 + 1, stats.ipc_dd_reaper_reaps.load());
    slot_core.release(try s.m.slot(2));
    try testing.expectEqual(@as(u32, 0), engine.inflightTotal());
}
