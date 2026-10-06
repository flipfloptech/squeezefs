//! The drain-group worker (charter §4 "Bring-up", "Drain groups",
//! "Delivery", "Worker pass", "Teardown"; `docs/porting/
//! fuse_uring_transport.md` §1, §2, §5, §8, §11): one OS thread
//! `sqz-fuse-ur<qid>` per drain group owning ONE SQE128 io_uring
//! (`ParkedRing128`), the member queues' slot tables, the 288 B ent
//! headers, ONE payload arena and the group's commit-cell queue. Linux only.
//!
//! The pass, verbatim in the model-checked order (§8.1): drain → DISARM →
//! pump the commit cells through the per-ent gate → unpark scan → re-arm the
//! standing READ → the 100 ms backstop iff the group owes any reply (derived
//! from the slot states, never a shadow count) → ONE `io_uring_enter`
//! carrying every SQE this pass pushed and the wait (unbounded only when idle
//! and owing nothing; non-blocking when work is in hand) → reap → per CQE:
//! the wake, the error ladder, or a DELIVERY → re-REGISTER and the backoff
//! retries. Commits BATCH: a push only appends to the SQ, the loop-bottom
//! enter flushes (31 → 9 `io_uring_enter` per create on the reference).
//!
//! The wake: the ring's standing READ on the group eventfd both wakes and
//! drains (the completed read zeroes the level-triggered counter), so the
//! reference's PollAdd + `read(2)` + peek economy collapse to ONE syscall
//! (decision 0002 ledger: Q7); `service()` disarms; a timed park is submit
//! first then wait (Q8: a submitting enter discards the wait's ETIME).
//!
//! THE ARM BARRIER COUNTS SUBMISSIONS, NOT COMPLETIONS: a REGISTER's CQE is
//! not an ack — the kernel parks the ent and completes it only when it
//! DELIVERS a request on it, and it routes requests to the ring only once
//! `is_ring_ready()` observes EVERY queue armed. Counting completions is a
//! deadlock (no readiness ⇒ no delivery ⇒ no CQE ⇒ no readiness).
//!
//! Why a dedicated thread and not an `exec.Lane`: `Lane` owns std's 64 B
//! `ParkedRing`; the FUSE uring command needs the 80 B command area of an
//! SQE128 ring (`io/uring128.zig`). Making the lane generic over the ring
//! face is the owed cutover (the fused handler poll — `poll_budget = 1` on
//! a `.fuse_ur` lane — is a zc-session feature that lands with it); until
//! then the worker hands every handler to the lane homed on the queue's
//! CPU and polls none itself.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const assert = std.debug.assert;

const uring128 = @import("../../io/uring128.zig");
const ring_park = @import("../../io/ring_park.zig");
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const geometry = @import("../geometry.zig");
const slot = @import("slot.zig");
const commit_queue = @import("commit_queue.zig");
const fsv = @import("filesystem_vtable.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const derive = @import("../../core/derive.zig");
const thread_name = @import("../../core/thread_name.zig");
const futex = @import("../../sync/futex.zig");
const completion = @import("../../sync/completion.zig");
const exec = @import("../../exec/exec.zig");
const placement = @import("../../exec/placement.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("fuse/transport/worker is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const SqzIo = exec.SqzIo;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The park backstop while the group owes a reply: the ipc dd-reaper's
/// shipped cadence. Field captures proved cross-thread wake chains lose
/// exactly one wake under load (two delivered LOOKUPs whose handlers
/// REPLIED sat unpumped for 23 min while every worker slept in cq-wait,
/// generic/795); idle workers keep the zero-cost unbounded park.
pub const PARK_BACKSTOP_NS: u64 = 100 * std.time.ns_per_ms;
/// The attribution WARN rate limit for a tick rescue (per worker).
pub const RESCUE_LOG_INTERVAL_S: u64 = 60;
/// The whole-group wait for live leases at the drain (was 1 ms…100 ms PER
/// ENT in series = 3.2 s at depth 32).
pub const DRAIN_LEASE_BUDGET_NS: u64 = 100 * std.time.ns_per_ms;
/// Reply residency after the drain (finding 32): the unmount's own
/// FLUSH/RELEASE against this session arrives AFTER the drain, and an ent
/// left in userspace is `waiting ≥ 1` — the 59-minute cloud wedge. Bounded
/// parks on the worker's own ring, never a sleep-poll (Q14).
pub const RESIDENCY_NS: u64 = 30 * std.time.ns_per_s;
pub const RESIDENCY_PARK_NS: u64 = 10 * std.time.ns_per_ms;
/// Read fusion ceiling: `max(payload / 8, 4096)` — two wakes and two
/// schedules ≈ 5–10 µs ≈ 50–100 KiB at ~10 GB/s of merge copy (128 KiB at
/// a 1 MiB payload).
pub fn fusionCeiling(payload_sz: u32) u32 {
    return @max(payload_sz / 8, 4096);
}
/// The arena stride: each ent's payload page-aligned so the kernel's GUP
/// and a later fixed-buffer registration see whole pages (one vma).
pub const ARENA_ALIGN: usize = 4096;

/// CQE `res` errnos that mean the connection is gone.
pub fn isDisconnect(e: linux.E) bool {
    return switch (e) {
        .NOTCONN, .CONNABORTED, .NODEV, .PIPE, .BADF, .SHUTDOWN => true,
        else => false,
    };
}

/// The kernel rejected the protocol itself (not this request).
fn isProtocolRejection(e: linux.E) bool {
    return switch (e) {
        .OPNOTSUPP, .INVAL, .NOSYS => true,
        else => false,
    };
}

// ---------------------------------------------------------------------------
// Counters (stats-inode keys).
// ---------------------------------------------------------------------------

/// `transport_ring_requests` / `transport_ring_replies`: deliveries
/// dispatched / COMMIT_AND_FETCH pushed (auto-commits and inline serves
/// included).
pub var ring_requests: counters.ShardedCounter = .{};
pub var ring_replies: counters.ShardedCounter = .{};
/// `transport_ring_reads`: READs delivered — the partition law's left side.
pub var ring_reads: counters.ShardedCounter = .{};
/// `transport_registers`: REGISTER SQEs pushed (arm + re-REGISTER + retries).
pub var registers: counters.ShardedCounter = .{};
/// `transport_cqe_errors`: negative-result CQEs.
pub var cqe_errors: counters.ShardedCounter = .{};
/// `transport_forget_autocommits`: FORGET/BATCH_FORGET deliveries committed
/// by the worker; `transport_forget_entries` the nlookup records inside them.
pub var forget_autocommits: counters.ShardedCounter = .{};
pub var forget_entries: counters.ShardedCounter = .{};
/// `transport_fast_dispatch_serves` / `_demotes`: the READ law's arms (a) / (c).
pub var fast_dispatch_serves: counters.ShardedCounter = .{};
pub var fast_dispatch_demotes: counters.ShardedCounter = .{};
/// `transport_fused_read_fusions` / `_demotions`: arm (b) taken / refused
/// (fusible by the lever and the ceiling, but no fused lane on this session).
pub var fused_read_fusions: counters.ShardedCounter = .{};
pub var fused_read_demotions: counters.ShardedCounter = .{};
/// `transport_dispatch_refused`: a handler lane refused the spawn (its
/// capacity); the request was answered EAGAIN-class (0002 row 10).
pub var dispatch_refused: counters.ShardedCounter = .{};
/// `transport_park_backstop_ticks`: bounded parks that ticked out
/// (non-zero under traffic = the backstop WORKING).
pub var park_backstop_ticks: counters.ShardedCounter = .{};
/// `transport_park_tick_commit_rescues` / `_cqe_rescues` (≈ 0): messages
/// pumped / CQEs surfaced by a tick with no wake delivered.
pub var park_tick_commit_rescues: counters.ShardedCounter = .{};
pub var park_tick_cqe_rescues: counters.ShardedCounter = .{};
/// `transport_enters`: `io_uring_enter` calls by the workers (the
/// enters-per-op instrument).
pub var enters: counters.ShardedCounter = .{};
/// `transport_passes`: worker passes run.
pub var passes: counters.ShardedCounter = .{};
/// `transport_residency_replies`: fresh deliveries answered ENOTCONN
/// after the drain.
pub var residency_replies: counters.ShardedCounter = .{};
/// `transport_cq_overflows` (must-stay-0): kernel-dropped CQEs.
pub var cq_overflows: counters.Tripwire = .init("transport_cq_overflows");
/// `transport_cqe_tag_unknown` (must-stay-0): a CQE tag no push of ours
/// produced (decision 0002 ledger Q6: a tripwire, not a REGISTER).
pub var cqe_tag_unknown: counters.Tripwire = .init("transport_cqe_tag_unknown");
/// `transport_delivery_on_leased_ent` (must-stay-0): a delivery landed on
/// an ent whose previous payload lease is still live.
pub var delivery_on_leased: counters.Tripwire = .init("transport_delivery_on_leased_ent");
/// `transport_sq_full` (must-stay-0): the SQ stayed full after a flush.
pub var sq_full: counters.Tripwire = .init("transport_sq_full");
/// `transport_commit_batch_flushes` / `_commits`: COMMIT SQEs per enter;
/// `commits / flushes ≈ 1` under a storm is the regression.
pub var commit_batch_flushes: counters.ShardedCounter = .{};
pub var commit_batch_commits: counters.ShardedCounter = .{};
/// `transport_commit_batch` buckets: exact 1–8, ≤ 16, ≤ 32, > 32 — a
/// "≈ 1" verdict never rides rounding.
pub const commit_batch_labels = [_][]const u8{ "1", "2", "3", "4", "5", "6", "7", "8", "le16", "le32", "gt32" };
pub var commit_batch: [commit_batch_labels.len]std.atomic.Value(u64) = @splat(.init(0));

fn noteCommitBatch(n: u32) void {
    if (n == 0) return;
    commit_batch_flushes.inc();
    commit_batch_commits.add(n);
    const idx: usize = if (n <= 8) n - 1 else if (n <= 16) 8 else if (n <= 32) 9 else 10;
    _ = commit_batch[idx].fetchAdd(1, .monotonic);
}

// ---------------------------------------------------------------------------
// user_data tags
// ---------------------------------------------------------------------------

pub const OpKind = enum(u8) { register = 1, commit = 2, fetch = 3, cancel = 4, release = 5 };

pub fn tagOf(kind: OpKind, gent: u32) u64 {
    return (uring128.Tag{ .kind = @backingInt(kind), .a = @intCast(gent), .b = 0, .c = 0 }).pack();
}

pub const DecodedTag = struct { kind: OpKind, gent: u32 };

pub fn decodeTag(ud: u64) ?DecodedTag {
    const t = uring128.Tag.unpack(ud);
    const kind = std.enums.fromInt(OpKind, t.kind) orelse return null;
    if (t.b != 0 or t.c != 0) return null;
    return .{ .kind = kind, .gent = t.a };
}

// ---------------------------------------------------------------------------
// The FUSE uring command (byte-exact; porting spec §1.5–1.6)
// ---------------------------------------------------------------------------

fn cmdBytes(commit_id: u64, qid: u16) [uring128.cmd_len]u8 {
    var cmd: [uring128.cmd_len]u8 = @splat(0);
    const req: abi.fuse_uring_cmd_req = .{ .flags = 0, .commit_id = commit_id, .qid = qid, .init_flags = 0, .queue_depth = 0, .padding = @splat(0) };
    cmd[0..@sizeOf(abi.fuse_uring_cmd_req)].* = abi.encode(req);
    return cmd;
}

/// A REGISTER for a UserEnts ent: `addr` = the 2-iovec array, `len` = 2,
/// `buf_index` 0, the command `{flags 0, commit_id 0, qid}`.
pub fn prepRegister(sqe: *uring128.Sqe128, qid: u16, iovs: *const [2]std.posix.iovec, gent: u32) void {
    const cmd = cmdBytes(0, qid);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_REGISTER, &cmd);
    sqe.addr = @intFromPtr(iovs);
    sqe.len = abi.REGISTER_IOVEC_COUNT;
    sqe.buf_index = 0;
    sqe.user_data = tagOf(.register, gent);
}

/// COMMIT_AND_FETCH: no iovecs; the reply rides in the ent.
pub fn prepCommit(sqe: *uring128.Sqe128, qid: u16, commit_id: u64, gent: u32) void {
    const cmd = cmdBytes(commit_id, qid);
    uring128.prepUringCmd(sqe, .{ .fixed = 0 }, abi.FUSE_IO_URING_CMD_COMMIT_AND_FETCH, &cmd);
    sqe.user_data = tagOf(.commit, gent);
}

/// The zc fd→slot bridge SQE shape (`READ_FIXED(fd @ off → slot ent_idx)`,
/// addr 0 = the slot's byte offset 0 under a kernel-registered zc slot):
/// only the bytes here; the leg that pushes it lands with the zero-copy
/// buffer mode.
pub fn prepBridgeFetch(sqe: *uring128.Sqe128, fd: linux.fd_t, off: u64, len: u32, ent_idx: u16, gent: u32) void {
    uring128.prepReadFixed(sqe, .{ .raw = fd }, ent_idx, off, len, 0);
    sqe.user_data = tagOf(.fetch, gent);
}

// ---------------------------------------------------------------------------
// Shared session-level state every group reads.
// ---------------------------------------------------------------------------

pub const BarrierOutcome = enum(u8) { armed, failed };

pub const Shared = struct {
    fuse_fd: linux.fd_t,
    plan: geometry.Plan,
    fs: fsv.Filesystem,
    exec: *SqzIo,
    handler_group: *Io.Group,
    /// The session's reply face (handles point at it).
    tx: *const fsv.Tx,
    nqueues: u32,
    /// qid == CPU id (the kernel's possible-CPU count): home the handler on it.
    qid_is_cpu: bool,
    /// `SQUEEZEFS_FUSE_READ_FUSION` ∧ a zero-copy session (never on UserEnts).
    read_fusion_on: bool,
    /// `SQUEEZEFS_FUSE_SQPOLL`: the kernel submission poller (A/B lever).
    sqpoll: bool,
    /// The topology for lane homing (null node when unknown).
    node_of: *const fn (ctx: ?*const anyopaque, cpu: u32) ?u32,
    node_ctx: ?*const anyopaque,
    /// The session's liveness: flipped once by `shutdown`; every worker
    /// drains when it reads false.
    active: std.atomic.Value(bool) = .init(true),
    /// The arm barrier: queues whose REGISTERs were all PUSHED.
    queues_registered: std.atomic.Value(u32) = .init(0),
    /// Completed by the worker that pushes the last queue's REGISTERs, or by
    /// a failing worker.
    barrier: completion.Completion(BarrierOutcome) = .{},
    /// `SQUEEZEFS_TEST_FUSE_REGISTER_REFUSE`: the next N REGISTER pushes are
    /// answered as refused without reaching the kernel.
    register_refuse_remaining: std.atomic.Value(u32) = .init(0),
    /// Error logs each worker thread expects in a test (`log.testing`).
    test_error_logs: usize = 0,

    pub fn nodeOf(self: *const Shared, qid: u16) ?u16 {
        const n = self.node_of(self.node_ctx, qid) orelse return null;
        return @intCast(n);
    }

    /// Idempotent: the first caller flips it.
    pub fn shutdown(self: *Shared) bool {
        return self.active.swap(false, .acq_rel);
    }

    pub fn isActive(self: *const Shared) bool {
        return self.active.load(.acquire);
    }
};

pub const Member = struct {
    qid: u16,
    slots: slot.SlotTable,
    /// Per ent: a commit cell is parked behind a live lease.
    parked: []bool,
    /// REGISTER pushes still owed before this queue counts for the barrier.
    initial_pending: u32,
    counted: bool = false,
};

pub const Group = struct {
    gpa: std.mem.Allocator,
    shared: *Shared,
    index: u16,
    members: []Member,
    depth: u32,
    payload_len: usize,
    stride: usize,
    arena: []align(ARENA_ALIGN) u8,
    headers: []abi.fuse_uring_req_header,
    iovs: [][2]std.posix.iovec,
    cells: commit_queue.Queue,
    /// The group's ring: built ON the worker thread (`SINGLE_ISSUER`).
    ring: uring128.ParkedRing128 = undefined,
    ring_rung: usize = 0,
    affinity: ?placement.CpuSet,
    thread: ?std.Thread = null,
    /// 0 starting, 1 armed, 2 failed.
    started: futex.Word = .init(0),
    /// 1 once the drain flushed its synthesized commits (the session's
    /// `umount2` waits for it so no blocked caller still holds the mount).
    drained: futex.Word = .init(0),
    name_buf: thread_name.CommBuf = undefined,
    // ---- pass state (worker thread only) ----
    wake_cqe_seen: bool = false,
    tick_rescue_pending: bool = false,
    commits_pushed: u32 = 0,
    rescue_log_last_s: u64 = 0,
    cq_overflow_seen: u32 = 0,
    disconnected: bool = false,
    protocol_rejected: bool = false,

    pub const InitError = std.mem.Allocator.Error || error{ArenaMapFailed};
    pub const StartError = error{ ThreadSpawnFailed, RingSetupFailed };

    /// Allocate the group's tables and arena (startup; nothing per op).
    pub fn init(gpa: std.mem.Allocator, shared: *Shared, index: u16, qids: []const u16, affinity: ?placement.CpuSet) InitError!*Group {
        const depth = shared.plan.depth;
        const gents: usize = @as(usize, qids.len) * depth;
        const payload_len: usize = shared.plan.payload_sz;
        const stride: usize = @intCast(derive.alignUp(payload_len, ARENA_ALIGN));

        const self = try gpa.create(Group);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .shared = shared,
            .index = index,
            .members = &.{},
            .depth = depth,
            .payload_len = payload_len,
            .stride = stride,
            .arena = &.{},
            .headers = &.{},
            .iovs = &.{},
            .cells = undefined,
            .affinity = affinity,
        };
        const members = try gpa.alloc(Member, qids.len);
        errdefer gpa.free(members);
        var built: usize = 0;
        errdefer for (members[0..built]) |*m| {
            m.slots.deinit(gpa);
            gpa.free(m.parked);
        };
        for (qids, 0..) |qid, i| {
            const parked = try gpa.alloc(bool, depth);
            errdefer gpa.free(parked);
            @memset(parked, false);
            members[i] = .{ .qid = qid, .slots = try slot.SlotTable.init(gpa, qid, depth), .parked = parked, .initial_pending = depth };
            built += 1;
        }
        self.members = members;
        self.headers = try gpa.alloc(abi.fuse_uring_req_header, gents);
        errdefer gpa.free(self.headers);
        self.iovs = try gpa.alloc([2]std.posix.iovec, gents);
        errdefer gpa.free(self.iovs);
        // ONE anonymous mapping for every member's payload buffers: born
        // zeroed, page-aligned, non-aliasing; THP-hinted.
        const span = stride * gents;
        const rc = linux.mmap(null, span, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
        if (linux.errno(rc) != .SUCCESS) {
            log.err("transport: group {d}: mmap of the {d}-byte payload arena failed ({t})", .{ index, span, linux.errno(rc) });
            return error.ArenaMapFailed;
        }
        const base: [*]align(ARENA_ALIGN) u8 = @ptrFromInt(rc);
        self.arena = base[0..span];
        errdefer _ = linux.munmap(self.arena.ptr, self.arena.len);
        _ = linux.madvise(self.arena.ptr, self.arena.len, linux.MADV.HUGEPAGE);
        for (self.headers, 0..) |*h, g| {
            h.* = abi.zeroed(abi.fuse_uring_req_header);
            // The kernel reads the ent's payload length at REGISTER.
            h.ring_ent_in_out.payload_sz = @intCast(payload_len);
            self.iovs[g] = .{
                .{ .base = @ptrCast(h), .len = @sizeOf(abi.fuse_uring_req_header) },
                .{ .base = self.payloadPtr(@intCast(g)), .len = payload_len },
            };
        }
        self.cells = try commit_queue.Queue.init(gpa, gents, self.waker());
        return self;
    }

    /// After `join` (or a never-started group) and after every handler that
    /// could hold a lease has finished.
    pub fn deinit(self: *Group) void {
        assert(self.thread == null);
        if (self.started.load(.acquire) == 1) self.ring.deinit();
        self.cells.deinit(self.gpa);
        _ = linux.munmap(self.arena.ptr, self.arena.len);
        self.gpa.free(self.iovs);
        self.gpa.free(self.headers);
        for (self.members) |*m| {
            m.slots.deinit(self.gpa);
            self.gpa.free(m.parked);
        }
        self.gpa.free(self.members);
        self.gpa.destroy(self);
    }

    pub fn arenaBytes(self: *const Group) u64 {
        return self.arena.len;
    }

    fn payloadPtr(self: *Group, gent: u32) [*]u8 {
        return self.arena.ptr + @as(usize, gent) * self.stride;
    }

    /// The ent's payload window (`payload_sz` bytes).
    pub fn window(self: *Group, gent: u32) []u8 {
        return self.payloadPtr(gent)[0..self.payload_len];
    }

    pub fn gentOf(self: *const Group, member_slot: u32, ent_idx: u16) u32 {
        return member_slot * self.depth + ent_idx;
    }

    fn memberOf(self: *Group, gent: u32) *Member {
        return &self.members[gent / self.depth];
    }

    fn entIdxOf(self: *const Group, gent: u32) u16 {
        return @intCast(gent % self.depth);
    }

    fn headerOf(self: *Group, gent: u32) *abi.fuse_uring_req_header {
        return &self.headers[gent];
    }

    /// The foreign face of this group's wake (lease drops, commit cells).
    pub fn waker(self: *Group) slot.Waker {
        return .{ .ctx = self, .wake = wakeFromForeign };
    }

    fn wakeFromForeign(ctx: ?*anyopaque) bool {
        const self: *Group = @ptrCast(@alignCast(ctx.?));
        return self.ring.wake();
    }

    /// The unconditional wake (shutdown): the worker must wake to read
    /// `active == false` whatever the coalescer says.
    pub fn forceWake(self: *Group) void {
        if (self.started.load(.acquire) == 1) self.ring.forceWake();
    }

    // ---- lifecycle ----

    /// Spawn the thread; returns once its ring stands and every REGISTER
    /// is pushed (or the ring was refused).
    pub fn start(self: *Group) StartError!void {
        assert(self.thread == null);
        self.thread = std.Thread.spawn(.{}, threadMain, .{self}) catch return error.ThreadSpawnFailed;
        while (self.started.load(.acquire) == 0) futex.wait(&self.started, 0);
        if (self.started.load(.acquire) == 2) {
            self.thread.?.join();
            self.thread = null;
            return error.RingSetupFailed;
        }
    }

    /// Join the worker (the session flipped `active` and force-woke it).
    pub fn join(self: *Group) void {
        const t = self.thread orelse return;
        t.join();
        self.thread = null;
    }

    pub fn owedCount(self: *const Group) u32 {
        var n: u32 = 0;
        for (self.members) |*m| n += m.slots.owedCount();
        return n;
    }

    fn threadMain(self: *Group) void {
        var base: [24]u8 = undefined;
        _ = thread_name.nameCurrentThread(&self.name_buf, std.fmt.bufPrint(&base, "sqz-fuse-ur{d}", .{self.members[0].qid}) catch unreachable);
        if (builtin.is_test) log.testing.expectErrorLogs(self.shared.test_error_logs);
        self.applyAffinity();
        self.initRing() catch {
            _ = self.shared.barrier.complete(.failed);
            self.started.store(2, .release);
            futex.wakeAll(&self.started);
            return;
        };
        self.registerAll();
        self.started.store(1, .release);
        futex.wakeAll(&self.started);
        self.runLoop();
        self.drain();
        self.ring.quiesce();
    }

    fn applyAffinity(self: *Group) void {
        const set = self.affinity orelse return;
        var cpu_set: linux.cpu_set_t = @splat(0);
        var it = set.iterator(.{});
        while (it.next()) |cpu| {
            cpu_set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        }
        // Best-effort: a refused mask leaves the worker where the scheduler put it.
        linux.sched_setaffinity(0, &cpu_set) catch {};
    }

    /// The SQPOLL ladder never composes with DEFER_TASKRUN (the poller owns
    /// the task work); the lever is "measured not recommended" and exists as
    /// the control arm only.
    const sqpoll_ladder = [_]u32{
        linux.IORING_SETUP_SQPOLL | linux.IORING_SETUP_SINGLE_ISSUER,
        linux.IORING_SETUP_SQPOLL,
    };

    fn initRing(self: *Group) error{RingSetupFailed}!void {
        const entries: u16 = @intCast(geometry.sqEntries(@intCast(self.members.len), self.depth));
        const ladder: []const u32 = if (self.shared.sqpoll) &sqpoll_ladder else &uring128.taskrun_ladder_flags;
        self.ring = uring128.ParkedRing128.initProbing(self.gpa, entries, ladder, &self.ring_rung) catch return error.RingSetupFailed;
        errdefer self.ring.deinit();
        // Fixed files `[fuse_fd, wake_eventfd]`: the uring command needs
        // `Fixed(0)`; a refusal fails the worker — and the mount.
        self.ring.ring.registerFiles(&.{ self.shared.fuse_fd, self.ring.event_fd }) catch |e| {
            log.err("transport: group {d}: register_files([fuse_fd, eventfd]) refused ({s}) — the ring cannot carry the FUSE command", .{ self.index, @errorName(e) });
            return error.RingSetupFailed;
        };
        if (!self.ring.armWake()) return error.RingSetupFailed;
        const posture = if (self.shared.sqpoll) "sqpoll" else uring128.taskrun_ladder[self.ring_rung].name;
        log.info("transport: group {d} (qids {d}..{d}, {d} members × depth {d}) ring armed on rung {d} ({s}), sq {d}, payload {d} B, arena {d} B", .{ self.index, self.members[0].qid, self.members[self.members.len - 1].qid, self.members.len, self.depth, self.ring_rung, posture, entries, self.payload_len, self.arena.len });
    }

    // ---- SQE pushes (append only; the loop-bottom enter flushes) ----

    fn takeSqe(self: *Group) ?*uring128.Sqe128 {
        return self.ring.getSqe() orelse {
            sq_full.fire("transport ring SQ full after a flush");
            return null;
        };
    }

    /// Push one REGISTER (or answer the test seam's refusal) and account
    /// the arm barrier: a queue counts once every ent's first REGISTER push
    /// happened — SUBMISSIONS, never completions (module doc).
    fn pushRegister(self: *Group, member_slot: u32, ent_idx: u16) void {
        const m = &self.members[member_slot];
        const gent = self.gentOf(member_slot, ent_idx);
        const now = clock.monotonicNs();
        const refused = blk: {
            var left = self.shared.register_refuse_remaining.load(.monotonic);
            while (left > 0) {
                if (self.shared.register_refuse_remaining.cmpxchgWeak(left, left - 1, .acq_rel, .monotonic)) |actual| {
                    left = actual;
                    continue;
                }
                break :blk true;
            }
            break :blk false;
        };
        if (refused) {
            self.onRegisterRefused(member_slot, ent_idx, .BUSY, now);
            return;
        }
        const sqe = self.takeSqe() orelse {
            self.onRegisterRefused(member_slot, ent_idx, .AGAIN, now);
            return;
        };
        prepRegister(sqe, m.qid, &self.iovs[gent], gent);
        m.slots.onRegisterSubmitted(ent_idx);
        registers.inc();
        self.noteInitialPush(m);
    }

    fn noteInitialPush(self: *Group, m: *Member) void {
        if (m.counted) return;
        if (m.initial_pending > 0) m.initial_pending -= 1;
        if (m.initial_pending != 0) return;
        m.counted = true;
        const n = self.shared.queues_registered.fetchAdd(1, .acq_rel) + 1;
        if (n == self.shared.nqueues) _ = self.shared.barrier.complete(.armed);
    }

    fn onRegisterRefused(self: *Group, member_slot: u32, ent_idx: u16, errno: linux.E, now: u64) void {
        const m = &self.members[member_slot];
        switch (m.slots.noteRegisterFailure(ent_idx, now)) {
            .retry => |r| log.debug("transport: queue {d} ent {d}: REGISTER refused ({t}); retry in {d} ms", .{ m.qid, ent_idx, errno, r.after_ns / std.time.ns_per_ms }),
            .retire => {
                log.err("transport: queue {d} ent {d}: 8 consecutive REGISTER refusals ({t}) — ent retired", .{ m.qid, ent_idx, errno });
                if (m.slots.allRetired()) {
                    log.err("transport: queue {d}: every ent retired — the kernel routes by CPU, so this queue would strand its CPU; the session fails", .{m.qid});
                    _ = self.shared.shutdown();
                    _ = self.shared.barrier.complete(.failed);
                }
            },
        }
    }

    /// The initial REGISTER of every ent of every member, then ONE enter so
    /// the kernel holds them before the barrier reports this group.
    fn registerAll(self: *Group) void {
        for (self.members, 0..) |_, ms| {
            var e: u16 = 0;
            while (e < self.depth) : (e += 1) self.pushRegister(@intCast(ms), e);
        }
        self.flushNonBlocking();
    }

    fn pushCommit(self: *Group, gent: u32, commit_id: u64) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const sqe = self.takeSqe() orelse {
            m.slots.abandon(e);
            return;
        };
        prepCommit(sqe, m.qid, commit_id, gent);
        m.slots.onCommitSubmitted(e);
        ring_replies.inc();
        self.commits_pushed += 1;
    }

    /// `fail_ent`: the header-only `-errno` for an owing slot; nothing
    /// otherwise (THE LAW's non-reply exit).
    fn failEnt(self: *Group, gent: u32, errno: linux.E) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        switch (m.slots.failEnt(e)) {
            .synthesize => |o| {
                if (m.parked[e]) {
                    m.parked[e] = false;
                    self.cells.recycle(gent);
                }
                slot.applyErrorHeader(self.headerOf(gent), o.unique, errno);
                self.pushCommit(gent, o.commit_id);
            },
            .nothing => {},
        }
    }

    // ---- the commit path ----

    fn commitReady(self: *Group, gent: u32, msg: *const commit_queue.Msg) void {
        const hdr = self.headerOf(gent);
        const payload = self.window(gent);
        switch (msg.kind) {
            .reply => _ = slot.applyReply(hdr, payload, msg.asReply()),
            .fd_body => {
                // A UserEnts queue has no zc slot to bridge into: the body
                // view is the reply.
                commit_queue.fd_body_fallbacks.inc();
                _ = slot.applyReply(hdr, payload, msg.asReply());
            },
            .prefilled => {
                if (msg.prefilled > payload.len) {
                    slot.replies_oversize.fire("prefilled length larger than the ent payload");
                    slot.applyErrorHeader(hdr, msg.header.unique, .IO);
                } else {
                    hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(msg.header);
                    hdr.ring_ent_in_out.payload_sz = msg.prefilled;
                }
            },
            .fail => slot.applyErrorHeader(hdr, msg.header.unique, msg.errno),
        }
        self.pushCommit(gent, msg.slot.commit_id);
    }

    /// One pumped cell: admit → the per-ent gate → commit now or park.
    fn handleCommitCell(self: *Group, gent: u32) void {
        const m = self.memberOf(gent);
        const e = self.entIdxOf(gent);
        const msg = self.cells.msg(gent);
        if (msg.slot.qid != m.qid or msg.slot.ent_idx != e) {
            slot.replies_refused_stale.inc();
            self.cells.recycle(gent);
            return;
        }
        switch (m.slots.admitCommit(e, msg.slot.commit_id)) {
            .refuse_stale => {
                log.warn("transport: queue {d} ent {d}: reply for commit {d} refused — the slot does not owe it (double, late or foreign reply)", .{ m.qid, e, msg.slot.commit_id });
                self.cells.recycle(gent);
            },
            .accept => switch (m.slots.ent(e).lease.tryCommit()) {
                .ready => {
                    self.commitReady(gent, msg);
                    self.cells.recycle(gent);
                },
                .parked => {
                    m.slots.onCommitParked(e);
                    m.parked[e] = true;
                },
            },
        }
    }

    fn unparkScan(self: *Group) void {
        for (self.members, 0..) |*m, ms| {
            for (m.parked, 0..) |p, e| {
                if (!p) continue;
                const ent_idx: u16 = @intCast(e);
                if (!m.slots.ent(ent_idx).lease.tryUnpark()) continue;
                const gent = self.gentOf(@intCast(ms), ent_idx);
                slot.unparked_commits.inc();
                m.parked[e] = false;
                self.commitReady(gent, self.cells.msg(gent));
                self.cells.recycle(gent);
            }
        }
    }

    // ---- the pass ----

    fn runLoop(self: *Group) void {
        while (self.shared.isActive() and !self.disconnected and !self.protocol_rejected) {
            passes.inc();
            // 1. Rescue capture.
            const after_tick = self.tick_rescue_pending;
            self.tick_rescue_pending = false;
            const tick_wake_seen = self.wake_cqe_seen;
            self.wake_cqe_seen = false;
            // 2–3. The READ CQE already drained the counter; disarm, then scan.
            _ = self.ring.service();
            // 4. Pump every published cell through the gate.
            var pass_msgs: u32 = 0;
            while (self.cells.pop()) |gent| {
                pass_msgs += 1;
                self.handleCommitCell(gent);
            }
            // 5. Unpark scan.
            self.unparkScan();
            // 6. Exit if the session died under us.
            if (!self.shared.isActive()) break;
            // 9. Re-arm the standing READ.
            var armed = true;
            if (!self.ring.armWake()) {
                sq_full.fire("no SQE for the wake READ");
                armed = false;
            }
            // 10. The backstop iff the group owes any reply (slot states).
            const owed = self.owedCount();
            const backstop: ?u64 = if (owed > 0 or !armed) PARK_BACKSTOP_NS else null;
            // 11. Rescue ledger, commit half.
            if (after_tick and pass_msgs > 0 and !tick_wake_seen) {
                park_tick_commit_rescues.add(pass_msgs);
                self.rescueWarn("commit", pass_msgs);
            }
            // 12. Work in hand ⇒ a non-blocking flush and loop.
            const now = clock.monotonicNs();
            var retry_left: ?u64 = null;
            for (self.members) |*m| if (m.slots.nextRetryDeadline()) |d| {
                const left = d -| now;
                retry_left = if (retry_left) |r| @min(r, left) else left;
            };
            if (pass_msgs > 0) {
                self.flushNonBlocking();
            } else {
                // 13. THE enter: every SQE pushed this pass and the wait.
                var bound: ?u64 = backstop;
                if (retry_left) |r| bound = if (bound) |b| @min(b, r) else r;
                const verdict = self.park(bound);
                if (verdict == .tick and backstop != null and (retry_left == null or backstop.? <= retry_left.?)) {
                    park_backstop_ticks.inc();
                    self.tick_rescue_pending = true;
                }
            }
            if (self.ring.isDead()) {
                self.disconnected = true;
                break;
            }
            // 14–15. Reap and act on every CQE.
            self.reap();
            // 16. Backoff retries due.
            self.pushRetriesDue();
        }
    }

    fn rescueWarn(self: *Group, what: []const u8, n: u32) void {
        const now_s = clock.realtimeCoarseS();
        if (now_s -| self.rescue_log_last_s < RESCUE_LOG_INTERVAL_S) return;
        self.rescue_log_last_s = now_s;
        log.warn("transport: group {d}: the 100 ms park backstop rescued {d} {s}(s) with no wake delivered — a lost wake in the chain (counted transport_park_tick_{s}_rescues)", .{ self.index, n, what, what });
    }

    /// The eager non-blocking GETEVENTS flush (a `DEFER_TASKRUN` ring runs
    /// its deferred completions only inside an enter).
    fn flushNonBlocking(self: *Group) void {
        noteCommitBatch(self.commits_pushed);
        self.commits_pushed = 0;
        enters.inc();
        _ = self.ring.ring.submitAndWaitTimed(1, 0) catch |e| switch (e) {
            error.Interrupted => {},
            else => {
                log.err("transport: group {d}: io_uring_enter failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                self.ring.markDead();
            },
        };
    }

    /// ONE enter: submit first, then the (bounded) wait. A timed park with
    /// NOTHING to submit (the common owed park: the handler is out on a
    /// lane, this pass pushed no SQE) is ONE enter whose ETIME is
    /// trustworthy because it submits nothing (Q8); otherwise the ring's
    /// two-enter timed park. Measured on the venue storm (4 threads, 10 s):
    /// 4.34 → 2.82 enters per ring request.
    fn park(self: *Group, bound: ?u64) ring_park.Verdict {
        noteCommitBatch(self.commits_pushed);
        self.commits_pushed = 0;
        if (bound) |b| {
            if (!self.ring.isDead() and self.ring.ring.sqReady() == 0 and self.ring.ring.features & linux.IORING_FEAT_EXT_ARG != 0) {
                enters.inc();
                const entered = self.ring.ring.submitAndWaitTimed(1, b) catch |e| switch (e) {
                    error.Interrupted => return .event,
                    else => {
                        log.err("transport: group {d}: io_uring_enter failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                        self.ring.markDead();
                        return .event;
                    },
                };
                return if (entered.timed_out) .tick else .event;
            }
            enters.add(2);
        } else enters.inc();
        return self.ring.park(bound);
    }

    fn reap(self: *Group) void {
        var cqes: [64]uring128.Cqe32 = undefined;
        var total: u32 = 0;
        while (true) {
            const n = self.ring.ring.copyCqes(&cqes, 0) catch |e| {
                log.err("transport: group {d}: reaping CQEs failed ({s}) — the ring is dead", .{ self.index, @errorName(e) });
                self.ring.markDead();
                self.disconnected = true;
                return;
            };
            for (cqes[0..n]) |cqe| self.handleCqe(cqe);
            total += n;
            if (n < cqes.len) break;
        }
        // The overflow watch: a wrapping delta of the kernel's counter.
        const overflow = self.ring.ring.cq.overflow.*;
        if (overflow != self.cq_overflow_seen) {
            cq_overflows.fire("kernel dropped CQEs on a transport ring");
            self.cq_overflow_seen = overflow;
        }
        if (self.tick_rescue_pending and total > 0) {
            park_tick_cqe_rescues.add(total);
            self.rescueWarn("cqe", total);
        }
    }

    fn handleCqe(self: *Group, cqe: uring128.Cqe32) void {
        if (self.ring.consumeWake(cqe)) {
            self.wake_cqe_seen = true;
            return;
        }
        if (uring128.Tag.isReserved(cqe.user_data)) return;
        const tag = decodeTag(cqe.user_data) orelse {
            cqe_tag_unknown.fire("CQE tag no transport push produced");
            return;
        };
        if (tag.gent >= self.headers.len) {
            cqe_tag_unknown.fire("CQE gent out of range");
            return;
        }
        if (cqe.res < 0) return self.handleCqeError(tag, cqe.err());
        switch (tag.kind) {
            .register, .commit => self.deliver(tag.gent, tag.kind == .register),
            .fetch, .cancel, .release => {},
        }
    }

    /// The CQE error ladder (porting spec §8.2), in order.
    fn handleCqeError(self: *Group, tag: DecodedTag, e: linux.E) void {
        cqe_errors.inc();
        const m = self.memberOf(tag.gent);
        const ent_idx = self.entIdxOf(tag.gent);
        const now = clock.monotonicNs();
        if (tag.kind == .fetch or tag.kind == .cancel or tag.kind == .release) return;
        if (isDisconnect(e)) {
            self.disconnected = true;
            return;
        }
        if (isProtocolRejection(e)) {
            log.err("transport: queue {d} ent {d}: the kernel rejected the FUSE uring protocol ({t} on {s}) — the session fails (charter §1.2: no classical fallback)", .{ m.qid, ent_idx, e, @tagName(tag.kind) });
            self.protocol_rejected = true;
            _ = self.shared.shutdown();
            _ = self.shared.barrier.complete(.failed);
            return;
        }
        switch (tag.kind) {
            .commit => {
                if (e == .AGAIN or e == .INTR) {
                    // Re-push the SAME commit: the ent still holds the applied reply.
                    if (m.slots.onCommitRetry(ent_idx, now)) |cid| {
                        self.pushCommit(tag.gent, cid);
                        return;
                    }
                }
                // The kernel discarded the reply: re-REGISTER the ent.
                log.warn("transport: queue {d} ent {d}: COMMIT_AND_FETCH failed ({t}); re-REGISTER", .{ m.qid, ent_idx, e });
                self.failEnt(tag.gent, .IO);
                self.pushRegister(tag.gent / self.depth, ent_idx);
            },
            .register => {
                self.failEnt(tag.gent, .IO);
                self.onRegisterRefused(tag.gent / self.depth, ent_idx, e, now);
            },
            else => unreachable,
        }
    }

    fn pushRetriesDue(self: *Group) void {
        const now = clock.monotonicNs();
        for (self.members, 0..) |*m, ms| {
            while (m.slots.nextRetryDue(now)) |e| self.pushRegister(@intCast(ms), e);
        }
    }

    // ---- delivery (porting spec §5) ----

    fn deliver(self: *Group, gent: u32, from_register: bool) void {
        const m = self.memberOf(gent);
        const ent_idx = self.entIdxOf(gent);
        const hdr = self.headerOf(gent);
        const now = clock.monotonicNs();
        if (from_register) m.slots.noteRegisterSuccess(ent_idx);
        const unique = std.mem.readInt(u64, hdr.in_out[8..16], .little);
        var commit_id = hdr.ring_ent_in_out.commit_id;
        if (commit_id == 0) commit_id = unique;
        if (unique == 0) {
            if (commit_id != 0) {
                // The forced EIO commit is the machine's normal exit here.
                m.slots.onDeliverDegenerate(ent_idx, commit_id, now);
                slot.applyErrorHeader(hdr, 0, .IO);
                self.pushCommit(gent, commit_id);
            } else {
                self.pushRegister(gent / self.depth, ent_idx);
            }
            return;
        }
        const announced: usize = hdr.ring_ent_in_out.payload_sz;
        const capped = @min(announced, self.payload_len);
        const payload = self.window(gent)[0..capped];
        m.slots.onDeliver(ent_idx, unique, commit_id, now);
        ring_requests.inc();
        const ent = m.slots.ent(ent_idx);
        if (ent.lease.leased()) {
            // A previous lease still reads this buffer: never hand it out again.
            delivery_on_leased.fire("delivery on an ent whose payload lease is live");
            return self.failEnt(gent, .IO);
        }
        const delivery = codec.Delivery.fromRing(&hdr.in_out, &hdr.op_in, payload, capped) catch return self.failEnt(gent, .INVAL);
        const parsed = codec.parse(delivery) catch |e| return self.failEnt(gent, codec.parseErrno(e));
        const req: fsv.Request = .{ .header = delivery.header, .op = parsed };
        const sref: slot.SlotRef = .{ .qid = m.qid, .ent_idx = ent_idx, .commit_id = commit_id };
        switch (parsed) {
            .forget => |f| {
                forget_entries.inc();
                self.shared.fs.forget(req.nodeid(), f.nlookup);
                self.autoCommit(gent, unique, commit_id);
            },
            .batch_forget => |b| {
                forget_entries.add(b.entries.len);
                for (b.entries) |e| self.shared.fs.forget(e.nodeid, e.nlookup);
                self.autoCommit(gent, unique, commit_id);
            },
            // No-reply opcodes that reached the ring: the ent must still be re-armed.
            .interrupt, .notify_reply => self.autoCommit(gent, unique, commit_id),
            .read => |in| self.dispatchRead(gent, req, in, sref),
            .write => |w| {
                const lease = slot.PayloadLease.acquire(&ent.lease, w.data, self.waker());
                self.dispatch(gent, req, sref, lease);
            },
            else => self.dispatch(gent, req, sref, null),
        }
    }

    /// FORGET/BATCH_FORGET (and the other no-reply opcodes): an empty
    /// reply commits the ent right here — over the ring an ent stays in
    /// userspace until COMMIT even for a "no reply" opcode.
    fn autoCommit(self: *Group, gent: u32, unique: u64, commit_id: u64) void {
        const hdr = self.headerOf(gent);
        hdr.in_out[0..codec.OUT_HEADER_LEN].* = abi.encode(codec.okHeader(unique, 0));
        hdr.ring_ent_in_out.payload_sz = 0;
        forget_autocommits.inc();
        self.pushCommit(gent, commit_id);
    }

    /// THE READ DISPATCH LAW at the delivery CQE: (a) the filesystem's
    /// try-only warm ladder serves inline; (b) a zc session fuses a READ at
    /// or under the ceiling onto the worker's lane (lever default OFF — a
    /// UserEnts session has no fused lane: counted as a demotion); (c) hand
    /// directly to the handler lane homed on the queue's CPU. Partition:
    /// `serves + fusions + demotes ≡ READs`.
    fn dispatchRead(self: *Group, gent: u32, req: fsv.Request, in: abi.fuse_read_in, sref: slot.SlotRef) void {
        ring_reads.inc();
        const win = self.window(gent);
        if (self.shared.fs.probeRead(req, win)) |reply| {
            fast_dispatch_serves.inc();
            _ = slot.applyReply(self.headerOf(gent), win, codec.replyRead(req.unique(), reply.body, in.size));
            self.pushCommit(gent, sref.commit_id);
            return;
        }
        const fusible = self.shared.read_fusion_on and in.size > 0 and in.size <= fusionCeiling(self.shared.plan.payload_sz);
        if (fusible) fused_read_demotions.inc();
        fast_dispatch_demotes.inc();
        self.dispatch(gent, req, sref, null);
    }

    /// Arm (c) and every other opcode: the lane homed on the queue's CPU.
    fn dispatch(self: *Group, gent: u32, req: fsv.Request, sref: slot.SlotRef, lease: ?slot.PayloadLease) void {
        const m = self.memberOf(gent);
        const handle: fsv.Handle = .{ .tx = self.shared.tx, .slot = .{ .ring = sref }, .unique = req.unique(), .window = self.window(gent) };
        const cpu: ?u16 = if (self.shared.qid_is_cpu) m.qid else null;
        const lane = self.shared.exec.homeLane(cpu, self.shared.nodeOf(m.qid));
        self.shared.exec.spawnGroupOn(lane, self.shared.handler_group, handlerMain, .{ self.shared.fs, req, handle, lease }) catch {
            dispatch_refused.inc();
            // Nothing is parked yet: the drop wakes nobody.
            if (lease) |l| l.release();
            self.failEnt(gent, .AGAIN);
        };
    }

    // ---- teardown (porting spec §11.2) ----

    fn drain(self: *Group) void {
        // 2. Final messages: every published cell still queued joins the
        //    parked ones; a leased ent gets the whole-group bounded wait.
        while (self.cells.pop()) |gent| self.handleCommitCell(gent);
        self.awaitLeases();
        // 4–5. Parked and still leased ⇒ header-only EIO (never the payload,
        //    never counted as an unpark); every other owing slot ⇒ fail_ent.
        for (self.members, 0..) |*m, ms| {
            for (m.parked, 0..) |p, e| {
                const ent_idx: u16 = @intCast(e);
                const gent = self.gentOf(@intCast(ms), ent_idx);
                if (p) {
                    if (m.slots.ent(ent_idx).lease.tryUnpark()) {
                        slot.unparked_commits.inc();
                        m.parked[e] = false;
                        self.commitReady(gent, self.cells.msg(gent));
                        self.cells.recycle(gent);
                        continue;
                    }
                    self.failEnt(gent, .IO);
                    continue;
                }
                if (m.slots.ent(ent_idx).owing() != null) self.failEnt(gent, .IO);
            }
        }
        // 6–7. One flush for the whole drain; the session may unmount now.
        self.flushNonBlocking();
        self.drained.store(1, .release);
        futex.wakeAll(&self.drained);
        // 8. Residency.
        self.residency();
        // Anything still owed when residency lapsed is the honest tripwire.
        for (self.members) |*m| {
            for (m.slots.ents, 0..) |*ent, e| if (ent.owing() != null) m.slots.abandon(@intCast(e));
        }
    }

    /// ONE bounded, event-driven wait for every parked ent's lease: the
    /// drops arrive through the eventfd; budget 100 ms for the group.
    fn awaitLeases(self: *Group) void {
        const deadline = clock.monotonicNs() + DRAIN_LEASE_BUDGET_NS;
        while (true) {
            var leased_parked = false;
            for (self.members) |*m| {
                for (m.parked, 0..) |p, e| if (p and m.slots.ent(@intCast(e)).lease.leased()) {
                    leased_parked = true;
                };
            }
            if (!leased_parked) return;
            const now = clock.monotonicNs();
            if (now >= deadline) return;
            if (self.ring.isDead()) return;
            _ = self.ring.armWake();
            _ = self.park(deadline - now);
            self.reapQuiet();
            _ = self.ring.service();
        }
    }

    /// Reap during the drain: wakes consumed, deliveries answered ENOTCONN
    /// header-only, errors noted for the disconnect verdict.
    fn reapQuiet(self: *Group) void {
        var cqes: [64]uring128.Cqe32 = undefined;
        while (true) {
            const n = self.ring.ring.copyCqes(&cqes, 0) catch {
                self.ring.markDead();
                self.disconnected = true;
                return;
            };
            for (cqes[0..n]) |cqe| {
                if (self.ring.consumeWake(cqe)) {
                    self.wake_cqe_seen = true;
                    continue;
                }
                if (uring128.Tag.isReserved(cqe.user_data)) continue;
                const tag = decodeTag(cqe.user_data) orelse continue;
                if (tag.gent >= self.headers.len) continue;
                if (cqe.res < 0) {
                    cqe_errors.inc();
                    if (isDisconnect(cqe.err())) self.disconnected = true;
                    continue;
                }
                if (tag.kind != .register and tag.kind != .commit) continue;
                // A fresh delivery on a still-registered ent: ENOTCONN,
                // header-only (a leased payload is never written).
                const m = self.memberOf(tag.gent);
                const ent_idx = self.entIdxOf(tag.gent);
                const hdr = self.headerOf(tag.gent);
                const unique = std.mem.readInt(u64, hdr.in_out[8..16], .little);
                var cid = hdr.ring_ent_in_out.commit_id;
                if (cid == 0) cid = unique;
                if (cid == 0) continue;
                m.slots.onDeliver(ent_idx, unique, cid, clock.monotonicNs());
                residency_replies.inc();
                self.failEnt(tag.gent, .NOTCONN);
            }
            if (n < cqes.len) return;
        }
    }

    /// Stay resident up to 30 s answering fresh deliveries ENOTCONN, in
    /// bounded parks on this ring; exit on a disconnect.
    fn residency(self: *Group) void {
        const deadline = clock.monotonicNs() + RESIDENCY_NS;
        while (!self.disconnected and !self.ring.isDead()) {
            const now = clock.monotonicNs();
            if (now >= deadline) {
                log.warn("transport: group {d}: residency backstop lapsed with {d} slot(s) owing", .{ self.index, self.owedCount() });
                return;
            }
            _ = self.ring.armWake();
            _ = self.park(@min(RESIDENCY_PARK_NS, deadline - now));
            self.reapQuiet();
            _ = self.ring.service();
        }
    }
};

/// The handler fiber: the filesystem answers the handle exactly once.
fn handlerMain(fs: fsv.Filesystem, req: fsv.Request, handle: fsv.Handle, lease: ?slot.PayloadLease) void {
    fs.dispatch(req, handle, lease);
}

// ---------------------------------------------------------------------------
// Tests: the SQE bytes and the tag space (the live transport is
// `session.zig`'s).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "REGISTER SQE bytes: URING_CMD on fixed file 0, cmd_op 1 at +8, the 2-iovec array at +16 with len 2 at +24, buf_index 0, qid at +64, the union u16 at +66 zero; COMMIT carries commit_id at +56 and cmd_op 2" {
    var sqe: uring128.Sqe128 = undefined;
    var iovs: [2]std.posix.iovec = .{ .{ .base = @ptrFromInt(0x1000), .len = 288 }, .{ .base = @ptrFromInt(0x2000), .len = 1 << 20 } };
    prepRegister(&sqe, 7, &iovs, 3);
    const raw: *const [128]u8 = @ptrCast(&sqe);
    try testing.expectEqual(@backingInt(linux.IORING_OP.URING_CMD), raw[0]);
    try testing.expect(raw[1] & linux.IOSQE_FIXED_FILE != 0);
    try testing.expectEqual(@as(i32, 0), std.mem.readInt(i32, raw[4..8], .little));
    try testing.expectEqual(@as(u32, 1), std.mem.readInt(u32, raw[8..12], .little));
    try testing.expectEqual(@intFromPtr(&iovs), std.mem.readInt(u64, raw[16..24], .little));
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, raw[24..28], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[40..42], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, raw[48..56], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, raw[56..64], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, raw[64..66], .little));
    try testing.expectEqual(@as(u16, 0), std.mem.readInt(u16, raw[66..68], .little));
    for (raw[72..128]) |b| try testing.expectEqual(@as(u8, 0), b);
    try testing.expectEqual(tagOf(.register, 3), sqe.user_data);

    prepCommit(&sqe, 7, 0xDEAD_BEEF, 3);
    const c: *const [128]u8 = @ptrCast(&sqe);
    try testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, c[8..12], .little));
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, c[16..24], .little));
    try testing.expectEqual(@as(u32, 0), std.mem.readInt(u32, c[24..28], .little));
    try testing.expectEqual(@as(u64, 0xDEAD_BEEF), std.mem.readInt(u64, c[56..64], .little));
    try testing.expectEqual(@as(u16, 7), std.mem.readInt(u16, c[64..66], .little));
    try testing.expectEqual(tagOf(.commit, 3), sqe.user_data);

    prepBridgeFetch(&sqe, 9, 8192, 4096, 5, 11);
    try testing.expectEqual(linux.IORING_OP.READ_FIXED, sqe.opcode);
    try testing.expectEqual(@as(i32, 9), sqe.fd);
    try testing.expectEqual(@as(u64, 8192), sqe.off);
    try testing.expectEqual(@as(u32, 4096), sqe.len);
    try testing.expectEqual(@as(u16, 5), sqe.buf_index);
    try testing.expectEqual(tagOf(.fetch, 11), sqe.user_data);
}

test "tags: every op kind round-trips for ents 0, 1, 31, 255 and 16383; the wake/reserved values and a foreign kind decode to null (the unknown-tag tripwire's input)" {
    for ([_]OpKind{ .register, .commit, .fetch, .cancel, .release }) |k| {
        for ([_]u32{ 0, 1, 31, 255, 16383 }) |g| {
            const d = decodeTag(tagOf(k, g)).?;
            try testing.expectEqual(k, d.kind);
            try testing.expectEqual(g, d.gent);
        }
    }
    try testing.expect(decodeTag(0) == null);
    try testing.expect(decodeTag((uring128.Tag{ .kind = 9, .a = 1, .b = 0, .c = 0 }).pack()) == null);
    try testing.expect(uring128.Tag.isReserved(ring_park.wake_ud));
    try testing.expectEqual(@as(u32, 128 << 10), fusionCeiling(1 << 20));
    try testing.expectEqual(@as(u32, 512 << 10), fusionCeiling(4 << 20));
    try testing.expectEqual(@as(u32, 4096), fusionCeiling(8192));
    try testing.expect(isDisconnect(.NOTCONN) and isDisconnect(.NODEV) and !isDisconnect(.AGAIN));
}

test "the commit-batch histogram buckets: 1–8 exact, ≤ 16, ≤ 32, > 32" {
    const before = commit_batch_flushes.load();
    const snap = blk: {
        var s: [commit_batch_labels.len]u64 = undefined;
        for (&s, 0..) |*v, i| v.* = commit_batch[i].load(.monotonic);
        break :blk s;
    };
    noteCommitBatch(0);
    noteCommitBatch(1);
    noteCommitBatch(8);
    noteCommitBatch(9);
    noteCommitBatch(16);
    noteCommitBatch(17);
    noteCommitBatch(32);
    noteCommitBatch(33);
    try testing.expectEqual(before + 7, commit_batch_flushes.load());
    try testing.expectEqual(snap[0] + 1, commit_batch[0].load(.monotonic));
    try testing.expectEqual(snap[7] + 1, commit_batch[7].load(.monotonic));
    try testing.expectEqual(snap[8] + 2, commit_batch[8].load(.monotonic));
    try testing.expectEqual(snap[9] + 2, commit_batch[9].load(.monotonic));
    try testing.expectEqual(snap[10] + 1, commit_batch[10].load(.monotonic));
}
