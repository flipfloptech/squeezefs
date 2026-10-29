//! The blocking pool (charter §9 "a blocking pool (`cap = cpus × 16`, floor
//! 8) for offload"; `docs/porting/exec.md` §1.4(f), §3.7): `sqz-blk<N>` OS
//! threads for work that must block (an `fsync(2)` on a path the rings do
//! not cover, a DNS lookup, a child process), so a lane never does.
//!
//! Sizing (`pool_cap_from`, `sqz_blocking.rs` L60–62): blocking work is
//! I/O-parked, not CPU-bound — the ×16 oversubscription mirrors the class;
//! floor 8 = the minimum useful parallel blocking capacity on any host. The
//! spawn law (L155–168, the displacement-storm lesson): a submit hands the
//! job to ONE idle worker if there is one, else spawns a thread while under
//! the cap — notifying one idle worker for a BURST double-books it. Idle
//! workers reap after 10 s with an empty queue (cached-thread hygiene); the
//! reap timer is the `sqz-timer` thread's (a worker's park is a plain futex
//! word; the timer fires it, a job cancels the timer — the tombstone).
//!
//! A `DedicatedWorker` is a pool with cap 1 and no reap: liveness-class
//! work that must never queue behind the bulk population (`sqz-lease-io`).
//!
//! From a fiber, `runBlocking` parks the fiber (the job cell lives on its
//! frame — nothing is allocated) and the worker wakes it through the
//! delivery word; from a raw thread it runs inline (nothing to yield to).
//! There is no panic boundary (0002 row 9): a job's error is its own result.
//!
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const assert = std.debug.assert;
const mpmc = @import("../sync/mpmc.zig");
const futex = @import("../sync/futex.zig");
const clock = @import("../core/clock.zig");
const thread_name = @import("../core/thread_name.zig");
const log = @import("../core/log.zig");
const fiber_mod = @import("fiber.zig");
const lane_mod = @import("lane.zig");
const timer_thread = @import("timer_thread.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/blocking is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;

/// `cpus × 16`, floor 8 (module doc). `cpus` is the fleet-share-divided root.
pub fn poolCapFrom(cpus: u32) u32 {
    return @max(cpus * 16, 8);
}

/// Idle workers exit after this long with an empty queue.
pub const idle_reap_ns: u64 = 10 * std.time.ns_per_s;

/// The queue holds `cap × 4` jobs: a burst four deep per worker is absorbed;
/// past it the refusal is the caller's retry class.
pub const queue_depth_per_worker = 4;

/// A job cell: on the submitter's frame (a fiber's, or a raw thread's that
/// runs it inline and never queues it).
pub const Job = struct {
    run: *const fn (*Job) void,
    /// The parked fiber to wake, or null (inline run).
    fiber: ?*Fiber,
    done: futex.Word = .init(0),
};

pub const Options = struct {
    cap: u32,
    idle_reap_ns: ?u64 = idle_reap_ns,
    /// `sqz-blk` or `sqz-lease-io` (the dedicated worker).
    name: []const u8 = "sqz-blk",
    /// The dedicated worker's single thread carries the name verbatim.
    numbered: bool = true,
};

const Worker = struct {
    pool: *BlockingPool,
    seq: u32,
    park: futex.Word = .init(0),
    next_idle: ?*Worker = null,
    /// In the idle stack (under the pool lock).
    idle: bool = false,
};

const SpinLock = struct {
    locked: std.atomic.Value(bool) = .init(false),
    fn acquire(self: *SpinLock) void {
        while (self.locked.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn release(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

pub const BlockingPool = struct {
    gpa: std.mem.Allocator,
    timer: *timer_thread.TimerThread,
    options: Options,
    queue: mpmc.BoundedMpmc(*Job),
    pending: std.atomic.Value(u32) = .init(0),
    lock: SpinLock = .{},
    /// LIFO of parked workers (the most recently parked is the warmest).
    idle_head: ?*Worker = null,
    total: std.atomic.Value(u32) = .init(0),
    next_seq: std.atomic.Value(u32) = .init(0),
    /// Threads alive (for the join at `deinit`).
    live: futex.Word = .init(0),
    stopping: std.atomic.Value(bool) = .init(false),

    pub const SubmitError = error{ConcurrencyUnavailable};

    pub fn init(gpa: std.mem.Allocator, timer: *timer_thread.TimerThread, options: Options) std.mem.Allocator.Error!*BlockingPool {
        const self = try gpa.create(BlockingPool);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .timer = timer, .options = options, .queue = undefined };
        self.queue = try mpmc.BoundedMpmc(*Job).init(gpa, options.cap * queue_depth_per_worker);
        return self;
    }

    /// Stop every worker (parked ones wake and exit; busy ones exit after
    /// their job), join them all, free.
    pub fn deinit(self: *BlockingPool) void {
        self.stopping.store(true, .release);
        while (true) {
            self.lock.acquire();
            const w = self.idle_head;
            if (w) |worker| {
                self.idle_head = worker.next_idle;
                worker.idle = false;
            }
            self.lock.release();
            const worker = w orelse break;
            worker.park.store(1, .release);
            futex.wakeAll(&worker.park);
        }
        while (true) {
            const n = self.live.load(.acquire);
            if (n == 0) break;
            futex.wait(&self.live, n);
        }
        self.queue.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    /// Queue a job: one idle worker takes it, else a thread spawns while
    /// under the cap, else it waits for the next worker to finish a job.
    pub fn submit(self: *BlockingPool, job: *Job) SubmitError!void {
        if (self.stopping.load(.acquire)) return error.ConcurrencyUnavailable;
        const depth = self.pending.fetchAdd(1, .acq_rel) + 1;
        if (!self.queue.tryPush(job)) {
            _ = self.pending.fetchSub(1, .acq_rel);
            stats.blocking_refused.inc();
            return error.ConcurrencyUnavailable;
        }
        stats.blocking_queue_depth.set(depth);
        self.lock.acquire();
        if (self.idle_head) |w| {
            self.idle_head = w.next_idle;
            w.idle = false;
            self.lock.release();
            w.park.store(1, .release);
            futex.wakeAll(&w.park);
            return;
        }
        self.lock.release();
        if (self.total.load(.acquire) < self.options.cap) self.spawnWorker();
    }

    fn spawnWorker(self: *BlockingPool) void {
        const prev = self.total.fetchAdd(1, .acq_rel);
        if (prev >= self.options.cap) {
            _ = self.total.fetchSub(1, .acq_rel);
            return;
        }
        _ = self.live.fetchAdd(1, .acq_rel);
        const seq = self.next_seq.fetchAdd(1, .monotonic);
        const t = std.Thread.spawn(.{}, workerMain, .{ self, seq }) catch |err| {
            _ = self.total.fetchSub(1, .acq_rel);
            _ = self.live.fetchSub(1, .acq_rel);
            log.err("exec: blocking pool thread spawn failed ({s}); the queued job waits for an existing worker", .{@errorName(err)});
            return;
        };
        t.detach();
        stats.blocking_spawns.inc();
        stats.blocking_threads.set(self.total.load(.monotonic));
    }

    fn workerMain(self: *BlockingPool, seq: u32) void {
        var name_buf: thread_name.CommBuf = undefined;
        var tmp: [32]u8 = undefined;
        const base = if (self.options.numbered) std.fmt.bufPrint(&tmp, "{s}{d}", .{ self.options.name, seq }) catch unreachable else self.options.name;
        _ = thread_name.nameCurrentThread(&name_buf, base);
        var me: Worker = .{ .pool = self, .seq = seq };
        defer {
            _ = self.total.fetchSub(1, .acq_rel);
            stats.blocking_threads.set(self.total.load(.monotonic));
            _ = self.live.fetchSub(1, .acq_rel);
            futex.wakeAll(&self.live);
        }
        while (true) {
            while (self.queue.tryPop()) |job| self.runOne(job);
            if (self.stopping.load(.acquire)) return;
            // Park: onto the idle stack, under the reap timer.
            me.park.store(0, .monotonic);
            self.lock.acquire();
            me.next_idle = self.idle_head;
            me.idle = true;
            self.idle_head = &me;
            self.lock.release();
            // A job pushed between our last pop and the push onto the idle
            // stack: the submitter saw no idle worker; take it ourselves.
            if (self.queue.tryPop()) |job| {
                if (self.unpark(&me)) {
                    self.runOne(job);
                    continue;
                }
                // A submit popped us concurrently: it will flip `park`; the
                // job we took is ours, the submit's wake is spurious.
                self.runOne(job);
            }
            const timer_id: ?u64 = if (self.options.idle_reap_ns) |reap| self.timer.arm(clock.monotonicNs() + reap, &me.park) else null;
            while (me.park.load(.acquire) == 0) futex.wait(&me.park, 0);
            const fired = if (timer_id) |id| !self.timer.cancel(id) else false;
            // Woken by a submit: it took us off the idle stack; loop and pop.
            if (!fired) continue;
            // The reap fired. Still idle ⇒ nothing arrived; a job that
            // raced the fire is ours to run.
            if (!self.unpark(&me)) continue;
            if (self.queue.tryPop()) |job| {
                self.runOne(job);
                continue;
            }
            stats.blocking_reaped.inc();
            return;
        }
    }

    fn runOne(self: *BlockingPool, job: *Job) void {
        stats.blocking_queue_depth.set(self.pending.fetchSub(1, .acq_rel) - 1);
        job.run(job);
        // release pairs with the awaiter's acquire load of `done`.
        job.done.store(1, .release);
        if (job.fiber) |f| {
            if (f.state.wake()) f.lane.enqueue(f);
        } else {
            futex.wakeAll(&job.done);
        }
    }

    /// Remove `w` from the idle stack if still there; false ⇔ a submit took it.
    fn unpark(self: *BlockingPool, w: *Worker) bool {
        self.lock.acquire();
        defer self.lock.release();
        if (!w.idle) return false;
        var cur = &self.idle_head;
        while (cur.*) |x| : (cur = &x.next_idle) {
            if (x == w) {
                cur.* = w.next_idle;
                w.idle = false;
                return true;
            }
        }
        unreachable;
    }

    pub fn threads(self: *const BlockingPool) u32 {
        return self.total.load(.acquire);
    }

    /// Parked workers (tests).
    pub fn idleWorkers(self: *BlockingPool) u32 {
        self.lock.acquire();
        defer self.lock.release();
        var n: u32 = 0;
        var cur = self.idle_head;
        while (cur) |w| : (cur = w.next_idle) n += 1;
        return n;
    }
};

fn ReturnOf(comptime function: anytype) type {
    return @typeInfo(@TypeOf(function)).@"fn".return_type.?;
}

/// Run `function(args...)` on the pool and return its result. On a lane
/// fiber the fiber parks (`Awaiting.blocking`: a cancel waits for the job);
/// off a lane it runs inline.
pub fn runBlocking(pool: *BlockingPool, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) BlockingPool.SubmitError!ReturnOf(function) {
    const R = ReturnOf(function);
    const Args = @TypeOf(args);
    const Closure = struct {
        job: Job,
        args: Args,
        result: R = undefined,
        fn run(j: *Job) void {
            const c: *@This() = @fieldParentPtr("job", j);
            c.result = @call(.auto, function, c.args);
        }
    };
    const f = lane_mod.currentFiber() orelse return @call(.auto, function, args);
    var c: Closure = .{ .job = .{ .run = Closure.run, .fiber = f }, .args = args };
    _ = f.changeAwaiting(.nothing, .blocking);
    pool.submit(&c.job) catch |err| {
        _ = f.changeAwaiting(.blocking, .nothing);
        return err;
    };
    while (c.job.done.load(.acquire) == 0) f.park();
    _ = f.changeAwaiting(.blocking, .nothing);
    return c.result;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

fn testPool(cap: u32, reap_ns: ?u64) !struct { pool: *BlockingPool, timer: *timer_thread.TimerThread } {
    const timer = timer_thread.TimerThread.init(testing.allocator);
    errdefer timer.deinit();
    try timer.start();
    const pool = try BlockingPool.init(testing.allocator, timer, .{ .cap = cap, .idle_reap_ns = reap_ns });
    return .{ .pool = pool, .timer = timer };
}

const Counter = struct {
    job: Job,
    hits: *std.atomic.Value(u32),
    tid: *std.atomic.Value(u64),
    sleep_ms: u64,

    fn run(j: *Job) void {
        const c: *Counter = @fieldParentPtr("job", j);
        if (c.sleep_ms > 0) testing.io.sleep(.fromNanoseconds(@intCast(c.sleep_ms * std.time.ns_per_ms)), .awake) catch {};
        c.tid.store(std.Thread.getCurrentId(), .release);
        _ = c.hits.fetchAdd(1, .acq_rel);
    }
    fn wait(c: *Counter) void {
        while (c.job.done.load(.acquire) == 0) futex.wait(&c.job.done, 0);
    }
};

test "pool_cap_from: cpus × 16, floor 8" {
    try testing.expectEqual(@as(u32, 8), poolCapFrom(0));
    try testing.expectEqual(@as(u32, 16), poolCapFrom(1));
    try testing.expectEqual(@as(u32, 512), poolCapFrom(32));
}

test "run_blocking_returns_and_reuses_threads: two sequential jobs run on ONE thread; the pool spawned once" {
    const p = try testPool(4, idle_reap_ns);
    defer p.timer.deinit();
    defer p.pool.deinit();
    var hits: std.atomic.Value(u32) = .init(0);
    var tid1: std.atomic.Value(u64) = .init(0);
    var tid2: std.atomic.Value(u64) = .init(0);
    const spawns_before = stats.blocking_spawns.load();
    var a: Counter = .{ .job = .{ .run = Counter.run, .fiber = null }, .hits = &hits, .tid = &tid1, .sleep_ms = 0 };
    try p.pool.submit(&a.job);
    a.wait();
    var b: Counter = .{ .job = .{ .run = Counter.run, .fiber = null }, .hits = &hits, .tid = &tid2, .sleep_ms = 0 };
    // The first worker is back on the idle stack before this submit lands
    // only if it got there; either way a second thread is not needed once
    // it is idle. Wait for it to park.
    while (p.pool.idleWorkers() == 0) try std.Thread.yield();
    try p.pool.submit(&b.job);
    b.wait();
    try testing.expectEqual(@as(u32, 2), hits.load(.acquire));
    try testing.expectEqual(tid1.load(.acquire), tid2.load(.acquire));
    try testing.expectEqual(spawns_before + 1, stats.blocking_spawns.load());
    try testing.expectEqual(@as(u32, 1), p.pool.threads());
}

test "burst_submits_run_in_parallel_past_one_idle_worker: 4 jobs that each wait for all 4 to be running all finish" {
    const p = try testPool(8, idle_reap_ns);
    defer p.timer.deinit();
    defer p.pool.deinit();
    var hits: std.atomic.Value(u32) = .init(0);
    var tid: std.atomic.Value(u64) = .init(0);
    // One idle worker exists before the burst (the displacement-storm shape).
    var warm: Counter = .{ .job = .{ .run = Counter.run, .fiber = null }, .hits = &hits, .tid = &tid, .sleep_ms = 0 };
    try p.pool.submit(&warm.job);
    warm.wait();
    while (p.pool.idleWorkers() == 0) try std.Thread.yield();
    // Concurrency is the law, judged by its observable: each job holds its
    // thread until all four are running at once — a pool that ran the burst
    // on fewer threads would never let the first one finish (never a wall
    // bound on the jobs, whose threads the host schedules).
    const Rendezvous = struct {
        job: Job,
        running: *std.atomic.Value(u32),
        hits: *std.atomic.Value(u32),
        fn run(j: *Job) void {
            const r: *@This() = @fieldParentPtr("job", j);
            _ = r.running.fetchAdd(1, .acq_rel);
            while (r.running.load(.acquire) < 4) std.Thread.yield() catch {};
            _ = r.hits.fetchAdd(1, .acq_rel);
        }
        fn wait(r: *@This()) void {
            while (r.job.done.load(.acquire) == 0) futex.wait(&r.job.done, 0);
        }
    };
    var running: std.atomic.Value(u32) = .init(0);
    var jobs: [4]Rendezvous = undefined;
    for (&jobs) |*j| {
        j.* = .{ .job = .{ .run = Rendezvous.run, .fiber = null }, .running = &running, .hits = &hits };
        try p.pool.submit(&j.job);
    }
    for (&jobs) |*j| j.wait();
    try testing.expectEqual(@as(u32, 5), hits.load(.acquire));
    try testing.expect(p.pool.threads() >= 4);
}

test "idle reap: workers exit after the reap period with an empty queue (through the sqz-timer thread); a dedicated worker never reaps" {
    const p = try testPool(4, 20 * std.time.ns_per_ms);
    defer p.timer.deinit();
    defer p.pool.deinit();
    var hits: std.atomic.Value(u32) = .init(0);
    var tid: std.atomic.Value(u64) = .init(0);
    var a: Counter = .{ .job = .{ .run = Counter.run, .fiber = null }, .hits = &hits, .tid = &tid, .sleep_ms = 0 };
    try p.pool.submit(&a.job);
    a.wait();
    try testing.expectEqual(@as(u32, 1), p.pool.threads());
    const reaped_before = stats.blocking_reaped.load();
    // The reap is awaited, never timed: a worker that never reaps is the
    // hang watchdog's.
    while (p.pool.threads() != 0) try std.Thread.yield();
    try testing.expectEqual(reaped_before + 1, stats.blocking_reaped.load());
    // The pool respawns on the next job.
    var b: Counter = .{ .job = .{ .run = Counter.run, .fiber = null }, .hits = &hits, .tid = &tid, .sleep_ms = 0 };
    try p.pool.submit(&b.job);
    b.wait();
    try testing.expectEqual(@as(u32, 2), hits.load(.acquire));

    // Dedicated: cap 1, no reap, the verbatim name.
    const dedicated = try BlockingPool.init(testing.allocator, p.timer, .{ .cap = 1, .idle_reap_ns = null, .name = "sqz-lease-io", .numbered = false });
    defer dedicated.deinit();
    var order: std.atomic.Value(u32) = .init(0);
    const Fifo = struct {
        job: Job,
        order: *std.atomic.Value(u32),
        expect: u32,
        ok: bool = false,
        fn run(j: *Job) void {
            const s: *@This() = @fieldParentPtr("job", j);
            testing.io.sleep(.fromNanoseconds(2 * std.time.ns_per_ms), .awake) catch {};
            s.ok = s.order.fetchAdd(1, .acq_rel) == s.expect;
        }
    };
    var jobs: [4]Fifo = undefined;
    for (&jobs, 0..) |*j, i| {
        j.* = .{ .job = .{ .run = Fifo.run, .fiber = null }, .order = &order, .expect = @intCast(i) };
        try dedicated.submit(&j.job);
    }
    for (&jobs) |*j| while (j.job.done.load(.acquire) == 0) futex.wait(&j.job.done, 0);
    for (&jobs) |*j| try testing.expect(j.ok);
    try testing.expectEqual(@as(u32, 1), dedicated.threads());
    testing.io.sleep(.fromNanoseconds(40 * std.time.ns_per_ms), .awake) catch {};
    try testing.expectEqual(@as(u32, 1), dedicated.threads());
}

test "refusal at cap × 4 queued jobs is error.ConcurrencyUnavailable, counted; inline run off a lane" {
    const p = try testPool(1, idle_reap_ns);
    defer p.timer.deinit();
    defer p.pool.deinit();
    var gate: futex.Word = .init(0);
    var hits: std.atomic.Value(u32) = .init(0);
    const Hold = struct {
        job: Job,
        gate: *futex.Word,
        hits: *std.atomic.Value(u32),
        fn run(j: *Job) void {
            const s: *@This() = @fieldParentPtr("job", j);
            while (s.gate.load(.acquire) == 0) futex.wait(s.gate, 0);
            _ = s.hits.fetchAdd(1, .acq_rel);
        }
    };
    // The one worker holds the first job; 4 more fill the queue; the 6th is refused.
    var jobs: [5]Hold = undefined;
    jobs[0] = .{ .job = .{ .run = Hold.run, .fiber = null }, .gate = &gate, .hits = &hits };
    try p.pool.submit(&jobs[0].job);
    while (p.pool.pending.load(.acquire) != 0) try std.Thread.yield();
    for (jobs[1..]) |*j| {
        j.* = .{ .job = .{ .run = Hold.run, .fiber = null }, .gate = &gate, .hits = &hits };
        try p.pool.submit(&j.job);
    }
    try testing.expectEqual(@as(u32, 4), p.pool.pending.load(.acquire));
    var extra: Hold = .{ .job = .{ .run = Hold.run, .fiber = null }, .gate = &gate, .hits = &hits };
    const refused_before = stats.blocking_refused.load();
    try testing.expectError(error.ConcurrencyUnavailable, p.pool.submit(&extra.job));
    try testing.expectEqual(refused_before + 1, stats.blocking_refused.load());
    gate.store(1, .release);
    futex.wakeAll(&gate);
    for (&jobs) |*j| while (j.job.done.load(.acquire) == 0) futex.wait(&j.job.done, 0);
    try testing.expectEqual(@as(u32, 5), hits.load(.acquire));
    // Off a lane, runBlocking is the call itself.
    const Inline = struct {
        fn add(a: u32, b: u32) u32 {
            return a + b;
        }
    };
    try testing.expectEqual(@as(u32, 7), try runBlocking(p.pool, Inline.add, .{ 3, 4 }));
}
