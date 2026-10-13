//! The lane: ONE OS thread + ONE fiber scheduler + ONE `ParkedRing`
//! (charter §9 per-core handler lanes, the metadata pool, the journal lane
//! that "parks IN its ring"; `docs/porting/exec.md` §1.2 and §3.2; the
//! reference's `LaneExec::run` fused with the charter §4 worker pass).
//!
//! The pass:
//!   1. `ring.service()` (drain → DISARM → scan) and drain the inbox into
//!      the ready FIFO — every popped fiber is SCHEDULED by construction;
//!   2. reap CQEs: a fiber's `user_data` stores `{res, flags}` in its result
//!      slot and `state.wake()` decides who enqueues (a CQE is just another
//!      wake, so the word also absorbs a futex wake racing a CQE);
//!   3. expire timers from the per-lane heap;
//!   4. run ready fibers (`begin_poll` false ⇒ stale, skip; `.parked` ⇒
//!      `end_poll_pending`, NOTIFIED requeues; `.finished` ⇒ the result to
//!      the awaiter; `.yielded` ⇒ the tail) — `poll_budget` fibers at most
//!      (1 on a `sqz-fuse-ur` lane: the measured ~50 µs/600 µs DMA overlap
//!      wants one handler poll between submits);
//!   5. ONE park: `tick = min(2 s, next timer − now)` and the 100 ms backstop
//!      ONLY while the lane owes a completion (an armed SQE whose CQE has not
//!      landed — "wake chains lose exactly one wake under load", charter
//!      §4); the loop-bottom enter flushes every SQE the pass prepared (the
//!      31 → 9 enters-per-create law). A timeout that finds inbox work
//!      disarms the coalescer so a lost write cannot silence later
//!      producers; it is a counted RESCUE (`exec_lane_tick_rescues`, must
//!      stay 0) only when the park ran the full tick and no wake CQE sits
//!      in the CQ — a message landing right after a timer deadline's
//!      timeout is the ordinary race, not a lost write.
//!
//! A lane may carry ONE `Resident` (the journal lane's stage B): a protocol
//! whose ring ops are tagged below the fiber space and routed to it at
//! step 2, and which is told the thread's start and exit. Its own waiting
//! is a fiber's (the lane's park IS its park); only the CQE routing and the
//! two thread edges are the hook.
//!
//! Lost-wake freedom (the reference's L33–37 argument, re-based on the
//! coalescer): every foreign push is followed by `arm` (a SeqCst RMW); the
//! lane's `service` disarm is a SeqCst RMW followed by the scan. A push whose
//! arm returned false is ordered before a disarm the lane's next scan
//! follows; a push whose arm returned true wrote the eventfd and the
//! level-triggered standing READ completes the park. The tick is the
//! backstop for the one thing the argument cannot cover — a write the kernel
//! or a seam lost.
//!
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const assert = std.debug.assert;

const ring_park = @import("../io/ring_park.zig");
const mpmc = @import("../sync/mpmc.zig");
const futex = @import("../sync/futex.zig");
const tick_law = @import("../sync/tick.zig");
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const thread_name = @import("../core/thread_name.zig");
const errors = @import("../core/errors.zig");
const fiber_mod = @import("fiber.zig");
const timer_heap = @import("timer_heap.zig");
const placement = @import("placement.zig");
const stats = @import("stats.zig");
const mem_budget = @import("../mem/budget.zig");
const group_mod = @import("group.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/lane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;
pub const LaneClass = stats.LaneClass;

/// The park tick on every lane: the lock plane's liveness posture
/// (`sync/tick.zig`) — a lost notify costs one tick, never a wedge.
pub const tick_ns: u64 = tick_law.tick_ns;
/// The backstop while a completion is owed: the ipc dd-reaper's shipped
/// 100 ms (`fuse_over_uring.rs` L342–346) — long enough that a healthy lane
/// never takes it, short enough that a lost CQE wake is a hiccup.
pub const backstop_ns: u64 = 100 * std.time.ns_per_ms;

/// `user_data` values below this are the lane's own tags or its resident's,
/// never a fiber (fibers live above the first page).
const first_fiber_ud: u64 = 4096;
/// The tag of an `ASYNC_CANCEL` SQE the lane issued (its CQE is ignored:
/// the original op's CQE releases the fiber).
const cancel_ud: u64 = 1;
/// The resident's tag space: `[resident_ud_min, resident_ud_max]` — above
/// the lane's own tags, below any fiber.
pub const resident_ud_min: u64 = cancel_ud + 1;
pub const resident_ud_max: u64 = first_fiber_ud - 1;

/// A protocol that LIVES on a lane and owns ring ops of its own (the
/// journal lane's stage B: many WRITE SQEs per window plus the one FSYNC,
/// tagged below `first_fiber_ud`): the lane routes their CQEs to it and
/// gives it the thread's start and exit. Everything else it does — issuing
/// SQEs on `lane.ring`, parking, timers — it does as an ordinary fiber on
/// the lane, so there is ONE park protocol (the lane's pass).
pub const Resident = struct {
    ctx: *anyopaque,
    /// On the lane thread once the ring stands, before `start` returns.
    started: *const fn (ctx: *anyopaque) void,
    /// A CQE under a resident tag.
    cqe: *const fn (ctx: *anyopaque, cqe: linux.io_uring_cqe) void,
    /// The ring died (an enter or a CQE read failed): fail every owed op.
    ringDead: *const fn (ctx: *anyopaque) void,
    /// On the lane thread after the last pass, before the ring is quiesced.
    stopping: *const fn (ctx: *anyopaque) void,
};

/// What a cross-thread `enqueue` paid: nothing (same lane: the ready FIFO),
/// an eventfd write, or an elided one (the coalescer saw a wake pending).
pub const WakeCost = enum { same_lane, written, elided };

/// Ring entries from the fiber population: one SQE per fiber in flight +
/// the standing wake READ + cancels, rounded to the power of two the kernel
/// requires; floor 8 (std's own default), ceiling 4096 (past it the SQ/CQ
/// arrays dominate the ring's memory for no measured gain).
pub fn ringEntriesFor(fiber_capacity: u32) u16 {
    return @intCast(derive.clamp(derive.nextPow2(fiber_capacity + 1 + 8), 8, 4096));
}

/// The setup ladder (charter §3: a runtime probe, never a kernel-version
/// table): `DEFER_TASKRUN` defers completion work to the lane's own enter
/// (only the issuer thread touches the ring), `COOP_TASKRUN` the lesser
/// posture, plain the floor.
const setup_ladder = [_]u32{
    linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_DEFER_TASKRUN | linux.IORING_SETUP_COOP_TASKRUN,
    linux.IORING_SETUP_SINGLE_ISSUER | linux.IORING_SETUP_COOP_TASKRUN,
    0,
};

/// Cross-thread messages. The bound is 2 × fiber_capacity: a fiber is queued
/// at most once (`task_state` law 2) plus at most one pending cancel.
pub const Msg = union(enum) {
    run: *Fiber,
    cancel_op: *Fiber,
};

pub const Options = struct {
    index: u16,
    class: LaneClass,
    home_cpu: ?u16 = null,
    node: ?u16 = null,
    fiber_capacity: u32,
    /// `fiber.stackSizeFor(class, page)` unless the caller derives otherwise.
    stack_size: usize,
    page: usize,
    /// The CPUs this lane's thread may run on (`placement.scopedAffinityCpus`);
    /// null = no affinity call (`SQUEEZEFS_NUMA=0`, or a homeless class).
    affinity: ?placement.CpuSet = null,
    /// 0 = unbounded; 1 on the transport workers.
    poll_budget: u16 = 0,
    /// `SQUEEZEFS_TEST_EXEC_DROP_WAKE`: drop the first foreign wake write.
    drop_first_wake: bool = false,
    /// The ring's SQ depth; null = `ringEntriesFor(fiber_capacity)`. A
    /// resident that issues many SQEs per fiber (the journal lane's runs)
    /// sizes the ring from its own population.
    ring_entries: ?u16 = null,
    resident: ?Resident = null,
};

/// Set on a lane thread for its lifetime.
pub threadlocal var current_lane: ?*Lane = null;

/// Whether the calling fiber's lane has NOTHING else to run — no ready
/// fiber, no message in its inbox (a hint: the inbox may fill a moment
/// later). False off a lane. A fiber that would spin before it parks asks
/// this first: a spin on a lane with queued work delays that work
/// (decision 0003 §AB.4).
pub fn laneIdleBesidesCaller() bool {
    const l = current_lane orelse return false;
    return l.ready_head == null and l.inbox.isEmpty();
}

/// The calling thread's lane's poll count (0 off a lane): a fiber that
/// reads it before a park and after its resume sees exactly one more iff
/// its own resume was the only poll between — the wake ran alone
/// (decision 0003 §AB.4).
pub fn lanePolls() u64 {
    const l = current_lane orelse return 0;
    return l.polls_run;
}

/// The fiber running on this thread, if this thread is a lane running one.
pub fn currentFiber() ?*Fiber {
    const l = current_lane orelse return null;
    return l.current;
}

pub const Lane = struct {
    index: u16,
    class: LaneClass,
    home_cpu: ?u16,
    node: ?u16,
    /// The comm base, built at init; `name_buf` is the prctl buffer.
    name_src: thread_name.CommBuf,
    name_buf: thread_name.CommBuf,
    name: []const u8,
    gpa: std.mem.Allocator,

    /// Built ON the lane thread (`SINGLE_ISSUER`); valid once `started == 1`.
    ring: ring_park.ParkedRing,
    inbox: mpmc.BoundedMpmc(Msg),
    /// Lane-thread-only intrusive FIFO.
    ready_head: ?*Fiber = null,
    ready_tail: ?*Fiber = null,
    ready_len: u32 = 0,
    timers: timer_heap.TimerHeap(Fiber),
    /// The slot pool (any thread pops at spawn, any thread pushes at recycle).
    free: mpmc.BoundedMpmc(*Fiber),
    fibers: []Fiber,
    /// One claim word per slot (`fibers[i]` ↔ `claims[i]`): `CLAIM_FREE`
    /// (quiescent — in the free ring or on its way back), `CLAIM_ARMED` (a
    /// spawn owns the stack), `CLAIM_TRIMMING` (the idle trim is returning
    /// its pages). Outside the `Fiber` so a recycle's reset never writes
    /// it under a trimmer's CAS (`slot_pool_model.zig`).
    claims: []std.atomic.Value(u8),
    fiber_capacity: u32,
    stack_size: usize,
    page: usize,
    idle_ctx: Io.fiber.Context = undefined,
    /// The fiber on the CPU (lane thread only).
    current: ?*Fiber = null,
    /// Fiber polls run (lane thread only; `lanePolls`).
    polls_run: u64 = 0,
    /// Armed ring ops whose CQE has not landed: the owed completions that
    /// engage the 100 ms backstop.
    in_flight: u32 = 0,
    /// Fibers between spawn and recycle.
    live: std.atomic.Value(u32) = .init(0),
    /// The idle clock the recycled slots are stamped with: the budget
    /// sampler's tick count (a test points it at its own).
    clock: *const std.atomic.Value(u64) = &mem_budget.authority.tick_count,
    /// The idle trim's station seam (null in the product): `holding` runs
    /// once per slot a trim pass has taken, before its trim — the pool
    /// pin's rendezvous.
    trim_station: ?TrimStation = null,
    /// `exec_lane_queue_depth`: ready + inbox depth as of the last pass.
    queue_depth: counters.Gauge = .{},
    tick_rescues: std.atomic.Value(u64) = .init(0),

    stop: std.atomic.Value(bool) = .init(false),
    /// False once the thread exited or the ring died (placement skips it).
    alive: std.atomic.Value(bool) = .init(true),
    /// 0 = starting, 1 = ring ready, 2 = ring setup failed.
    started: futex.Word = .init(0),
    thread: ?std.Thread = null,
    affinity: ?placement.CpuSet,
    poll_budget: u16,
    drop_next_wake: std.atomic.Value(bool),
    ring_entries: u16,
    resident: ?Resident,
    ring_dead_logged: bool = false,

    pub const InitError = std.mem.Allocator.Error;
    pub const StartError = error{ RingSetupFailed, ThreadSpawnFailed };

    pub const CLAIM_FREE: u8 = 0;
    pub const CLAIM_ARMED: u8 = 1;
    pub const CLAIM_TRIMMING: u8 = 2;

    pub const TrimStation = struct {
        ctx: *anyopaque,
        holding: *const fn (ctx: *anyopaque, f: *Fiber) void,
    };
    pub const SpawnError = error{ConcurrencyUnavailable};

    /// Allocate the lane and its fiber table (stacks map lazily at first
    /// spawn — the virtual reserve is the only cost of a large population).
    pub fn init(gpa: std.mem.Allocator, options: Options) InitError!*Lane {
        const self = try gpa.create(Lane);
        errdefer gpa.destroy(self);
        self.* = .{
            .index = options.index,
            .class = options.class,
            .home_cpu = options.home_cpu,
            .node = options.node,
            .name_src = undefined,
            .name_buf = undefined,
            .name = "",
            .gpa = gpa,
            .ring = undefined,
            .inbox = undefined,
            .timers = undefined,
            .free = undefined,
            .fibers = &.{},
            .claims = &.{},
            .fiber_capacity = options.fiber_capacity,
            .stack_size = options.stack_size,
            .page = options.page,
            .affinity = options.affinity,
            .poll_budget = options.poll_budget,
            .drop_next_wake = .init(options.drop_first_wake),
            .ring_entries = options.ring_entries orelse ringEntriesFor(options.fiber_capacity),
            .resident = options.resident,
        };
        self.name = commNameFor(&self.name_src, options.class, options.index);
        self.inbox = try mpmc.BoundedMpmc(Msg).init(gpa, 2 * options.fiber_capacity);
        errdefer self.inbox.deinit(gpa);
        self.free = try mpmc.BoundedMpmc(*Fiber).init(gpa, options.fiber_capacity);
        errdefer self.free.deinit(gpa);
        self.timers = try timer_heap.TimerHeap(Fiber).init(gpa, options.fiber_capacity);
        errdefer self.timers.deinit(gpa);
        self.claims = try gpa.alloc(std.atomic.Value(u8), options.fiber_capacity);
        errdefer gpa.free(self.claims);
        @memset(self.claims, .init(CLAIM_FREE));
        self.fibers = try gpa.alloc(Fiber, options.fiber_capacity);
        for (self.fibers, 0..) |*f, i| {
            f.stack = null;
            f.idle_tick = 0;
            f.reset(self, @intCast(i));
            assert(self.free.tryPush(f));
        }
        return self;
    }

    /// Join (if started) and free everything. Every fiber must be recycled
    /// (quiescent lanes only — the product's lanes are process-lifetime).
    pub fn deinit(self: *Lane) void {
        self.stopAndJoin();
        for (self.fibers) |*f| if (f.stack) |s| s.unmap();
        self.gpa.free(self.fibers);
        self.gpa.free(self.claims);
        self.timers.deinit(self.gpa);
        self.free.deinit(self.gpa);
        self.inbox.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    /// The charter's names: `sqz-fuse-ln<N>` (decision 0003 Q-A),
    /// `sqz-fuse-ur<qid>`, `sqz-meta<i>`, `sqz-lease`, `sqz-jrnl<N>`; `main`
    /// keeps the process's own comm.
    pub fn commNameFor(buf: *thread_name.CommBuf, class: LaneClass, index: u16) []const u8 {
        var tmp: [32]u8 = undefined;
        const base = switch (class) {
            .fuse => std.fmt.bufPrint(&tmp, "sqz-fuse-ln{d}", .{index}) catch unreachable,
            .fuse_ur => std.fmt.bufPrint(&tmp, "sqz-fuse-ur{d}", .{index}) catch unreachable,
            .meta => std.fmt.bufPrint(&tmp, "sqz-meta{d}", .{index}) catch unreachable,
            .lease => "sqz-lease",
            .journal => std.fmt.bufPrint(&tmp, "sqz-jrnl{d}", .{index}) catch unreachable,
            .main => return "",
        };
        assert(thread_name.fitsBudget(base));
        return thread_name.commName(buf, base);
    }

    // ---- lifecycle ----

    /// Spawn the lane thread and wait until its ring stands (so a wake
    /// handle is valid the moment this returns). A ring the kernel refuses
    /// is the one loud error (charter §1.2: no classical fallback).
    pub fn start(self: *Lane) StartError!void {
        assert(self.thread == null);
        self.thread = std.Thread.spawn(.{}, threadMain, .{self}) catch return error.ThreadSpawnFailed;
        while (self.started.load(.acquire) == 0) futex.wait(&self.started, 0);
        if (self.started.load(.acquire) == 2) {
            self.thread.?.join();
            self.thread = null;
            return error.RingSetupFailed;
        }
    }

    /// Request the loop to exit at its next pass top and join it; idempotent.
    pub fn stopAndJoin(self: *Lane) void {
        const t = self.thread orelse return;
        // release pairs with the loop's acquire load at the pass top.
        self.stop.store(true, .release);
        self.ring.forceWake();
        t.join();
        self.thread = null;
        // `quiesce` ran on the lane thread; the ring and eventfd go here.
        self.ring.deinit();
    }

    fn threadMain(self: *Lane) void {
        current_lane = self;
        defer current_lane = null;
        if (self.name.len > 0) _ = thread_name.nameCurrentThread(&self.name_buf, self.name);
        self.applyAffinity();
        self.initRingHere() catch {
            self.alive.store(false, .release);
            self.started.store(2, .release);
            futex.wakeAll(&self.started);
            return;
        };
        // The resident's start (a fixed-file registration, its thread-local
        // seams) precedes the ready signal so `start` returns a lane whose
        // ring is fully set.
        if (self.resident) |r| r.started(r.ctx);
        self.started.store(1, .release);
        futex.wakeAll(&self.started);
        self.runLoop();
        if (self.resident) |r| r.stopping(r.ctx);
        self.ring.quiesce();
        self.alive.store(false, .release);
    }

    fn initRingHere(self: *Lane) error{RingSetupFailed}!void {
        var rung: usize = undefined;
        self.ring = ring_park.ParkedRing.initProbing(self.gpa, self.ring_entries, &setup_ladder, &rung) catch return error.RingSetupFailed;
        if (rung != 0) log.debug("exec: lane {d} ({s}) ring on setup rung {d}", .{ self.index, self.name, rung });
    }

    /// `io_uring_enter` calls this lane's ring paid so far.
    pub fn enters(self: *const Lane) u64 {
        return self.ring.enters.load(.monotonic);
    }

    /// The `main` class: the CALLING thread is the lane (`SqzIo.run`'s
    /// thread-park `block_on` is a ring park). Builds the ring here;
    /// `detachCurrentThread` undoes it.
    pub fn attachCurrentThread(self: *Lane) StartError!void {
        assert(self.thread == null and current_lane == null);
        try self.initRingHere();
        self.started.store(1, .release);
        current_lane = self;
    }

    /// Run the pass loop on the calling (attached) thread until `root`'s
    /// result was published (`finalize` swapped `finished` in).
    pub fn runUntilFinished(self: *Lane, root: *Fiber) void {
        assert(current_lane == self);
        while (true) {
            // acquire pairs with `finalize`'s acq_rel swap.
            if (root.awaiter.load(.acquire) == Fiber.finished) return;
            self.passTop();
            self.runReady();
            if (root.awaiter.load(.acquire) == Fiber.finished) return;
            self.parkOnce();
        }
    }

    pub fn detachCurrentThread(self: *Lane) void {
        assert(current_lane == self);
        self.ring.quiesce();
        self.ring.deinit();
        self.alive.store(false, .release);
        self.started.store(0, .release);
        current_lane = null;
    }

    fn applyAffinity(self: *Lane) void {
        const set = self.affinity orelse return;
        var cpu_set: linux.cpu_set_t = @splat(0);
        var it = set.iterator(.{});
        while (it.next()) |cpu| {
            cpu_set[cpu / @bitSizeOf(usize)] |= @as(usize, 1) << @intCast(cpu % @bitSizeOf(usize));
        }
        // Best-effort (`affinity.rs` L97–115): a refused mask leaves the
        // lane where the scheduler put it.
        linux.sched_setaffinity(0, &cpu_set) catch {};
    }

    // ---- spawn ----

    /// Take a slot, arm it and enqueue it (spawn IS the first wake). The
    /// refusal is the lane's capacity — `error.ConcurrencyUnavailable`, which
    /// the transport answers EAGAIN-class (0002 row 10).
    pub fn spawn(self: *Lane, kind: fiber_mod.Kind, start_fn: fiber_mod.Start, context: []const u8, group: ?*Io.Group, protection: fiber_mod.Protection) SpawnError!*Fiber {
        if (!self.alive.load(.acquire)) return error.ConcurrencyUnavailable;
        const f = self.free.tryPop() orelse {
            stats.spawn_refused.of(self.class).inc();
            return error.ConcurrencyUnavailable;
        };
        self.claimForSpawn(f);
        const group_canceled = if (group) |g| group_mod.isCanceled(g) else false;
        f.arm(kind, start_fn, context, group, group_canceled, protection, self.stack_size, self.page) catch {
            self.claims[f.slot].store(CLAIM_FREE, .release);
            self.free.pushSpin(f);
            stats.spawn_refused.of(self.class).inc();
            return error.ConcurrencyUnavailable;
        };
        _ = self.live.fetchAdd(1, .monotonic);
        stats.fibers_live.of(self.class).add(1);
        self.enqueue(f);
        return f;
    }

    /// The popped slot's claim, FREE → ARMED (acquire: pairs with the
    /// trim's release, so a trimmed stack reads `trimmed` and re-arms). A
    /// slot the idle trim holds is waited out — one madvise, counted
    /// (`exec_spawn_trim_waits`) — never refused: the trim never took it
    /// out of the pool.
    fn claimForSpawn(self: *Lane, f: *Fiber) void {
        const c = &self.claims[f.slot];
        var waited = false;
        while (c.cmpxchgWeak(CLAIM_FREE, CLAIM_ARMED, .acquire, .monotonic)) |cur| {
            assert(cur == CLAIM_TRIMMING);
            if (!waited) stats.spawn_trim_waits.inc();
            waited = true;
            std.atomic.spinLoopHint();
        }
    }

    /// Return a finished fiber's slot to the pool (any thread; the fiber's
    /// stack is quiescent: the lane published `finished` after it switched
    /// out). The claim goes FREE only now (release: the reset and the idle
    /// stamp are what a trimmer's claim acquires), then the push, and the
    /// live count LAST: `live == 0` means every slot is back in the pool
    /// (the quiescence barrier `finalize` and the tests' waits read).
    pub fn recycle(self: *Lane, f: *Fiber) void {
        f.recordHighWater();
        f.reset(self, f.slot);
        f.idle_tick = self.clock.load(.monotonic);
        stats.fibers_live.of(self.class).sub(1);
        self.claims[f.slot].store(CLAIM_FREE, .release);
        // The ring holds the whole population; a refusal is a spawner's pop
        // mid-flight on the wrap cell (`mpmc.pushSpin`).
        self.free.pushSpin(f);
        _ = self.live.fetchSub(1, .monotonic);
    }

    /// The pages the kernel holds for this lane's stacks, measured
    /// (`Stack.residentBytes` per mapped slot; any thread — a slot being
    /// mapped concurrently reads torn and counts 0 for this tick).
    pub fn residentStackBytes(self: *Lane) u64 {
        var n: u64 = 0;
        for (self.fibers) |*f| if (f.stack) |s| {
            n += s.residentBytes();
        };
        return n;
    }

    /// The idle-trim law over this lane's slot pool (any thread; the
    /// sampler's): return the stack pages of every FREE slot idle for
    /// ≥ `min_idle_ticks` ticks. Each slot is claimed IN PLACE (FREE →
    /// TRIMMING, acquire: the recycle's reset and stamp are visible) and
    /// released after its madvise (release: the spawn's claim sees
    /// `trimmed`) — never popped out of the ring: a popped slot's push in
    /// flight hid every slot queued behind it from spawn (the ring's head
    /// answered empty; 121 of 300 loops refused a spawn under load). A
    /// spawn that pops a slot under trim waits out the one madvise.
    /// Returns the stacks trimmed.
    pub fn trimIdleStacks(self: *Lane, min_idle_ticks: u64) u32 {
        const now = self.clock.load(.monotonic);
        var trimmed: u32 = 0;
        for (self.fibers, self.claims) |*f, *c| {
            if (c.cmpxchgStrong(CLAIM_FREE, CLAIM_TRIMMING, .acquire, .monotonic) != null) continue;
            defer c.store(CLAIM_FREE, .release);
            if (self.trim_station) |st| st.holding(st.ctx, f);
            if (f.stack) |*s| if (!s.trimmed and now -| f.idle_tick >= min_idle_ticks) {
                s.trim();
                trimmed += 1;
            };
        }
        return trimmed;
    }

    /// Queue a SCHEDULED fiber (the caller's `state.wake()` returned true).
    /// Same lane thread ⇒ the ready FIFO with no eventfd write (the
    /// `CURRENT_FUSED_LANE` elision); otherwise the inbox + the coalesced wake.
    pub fn enqueue(self: *Lane, f: *Fiber) void {
        _ = self.enqueueReporting(f);
    }

    /// `enqueue` that reports what the wake cost (a producer's own
    /// `wake_{writes,elided}` mirror).
    pub fn enqueueReporting(self: *Lane, f: *Fiber) WakeCost {
        assert(f.lane == self);
        if (current_lane == self) {
            stats.same_lane_wakes.inc();
            self.pushReady(f);
            return .same_lane;
        }
        stats.cross_lane_wakes.inc();
        if (!self.inbox.tryPush(.{ .run = f })) stats.inbox_overflow.fire("lane inbox full on a run message");
        return self.wakeFromForeign();
    }

    /// Ask the OWNING lane to `ASYNC_CANCEL` the ring op `f` is parked on.
    pub fn postCancelOp(self: *Lane, f: *Fiber) void {
        if (current_lane == self) {
            self.issueCancel(f);
            return;
        }
        if (!self.inbox.tryPush(.{ .cancel_op = f })) stats.inbox_overflow.fire("lane inbox full on a cancel message");
        _ = self.wakeFromForeign();
    }

    fn wakeFromForeign(self: *Lane) WakeCost {
        // The seam: one dropped write, healed by the tick rescue.
        if (self.drop_next_wake.load(.monotonic) and self.drop_next_wake.swap(false, .acq_rel)) return .elided;
        if (self.ring.wake()) {
            stats.wake_writes.inc();
            return .written;
        }
        stats.wake_elided.inc();
        return .elided;
    }

    // ---- the lane thread's SQE face ----

    /// An SQE on this lane's ring for the running fiber: `prep(sqe, args…)`
    /// fills it (std's `prep_*` reset every field, so the tag goes on
    /// after), `user_data` is the fiber, the completion is owed until its
    /// CQE. False when the SQ stayed full after a flush (the caller fails the
    /// op `SubmissionQueueOverflow`).
    pub fn armRingOp(self: *Lane, f: *Fiber, comptime prep: anytype, args: anytype) bool {
        assert(current_lane == self and self.current == f);
        if (self.ring.isDead()) return false;
        const sqe = self.ring.getSqe() orelse return false;
        @call(.auto, prep, .{sqe} ++ args);
        sqe.user_data = @intFromPtr(f);
        f.op_pending = true;
        self.in_flight += 1;
        stats.ring_ops.inc();
        return true;
    }

    fn issueCancel(self: *Lane, f: *Fiber) void {
        if (f.cancelWord().awaiting != .ring_op) return;
        const sqe = self.ring.getSqe() orelse return;
        sqe.prep_cancel(@intFromPtr(f), 0);
        sqe.user_data = cancel_ud;
    }

    // ---- the pass ----

    fn runLoop(self: *Lane) void {
        while (true) {
            // acquire pairs with `stopAndJoin`'s release store.
            const stopping = self.stop.load(.acquire);
            self.passTop();
            if (stopping) return;
            self.runReady();
            self.parkOnce();
        }
    }

    /// Steps 1–3 in the model-checked order: reap (the wake CQE is
    /// consumed) → `service` DISARMS → scan the inbox. Disarming before the
    /// scan is what lets a producer that pushes during the scan write the
    /// eventfd again (`wake_core_model.zig`: disarm after the scan loses a
    /// push).
    fn passTop(self: *Lane) void {
        self.reapCqes();
        _ = self.ring.service();
        self.drainInbox();
        self.expireTimers();
    }

    fn drainInbox(self: *Lane) void {
        while (self.inbox.tryPop()) |m| self.handleMsg(m);
    }

    fn handleMsg(self: *Lane, m: Msg) void {
        switch (m) {
            .run => |f| self.pushReady(f),
            .cancel_op => |f| self.issueCancel(f),
        }
    }

    fn reapCqes(self: *Lane) void {
        if (self.ring.isDead()) return;
        var cqes: [64]linux.io_uring_cqe = undefined;
        while (true) {
            const n = self.ring.ring.copy_cqes(&cqes, 0) catch {
                self.ring.markDead();
                self.onRingDead();
                return;
            };
            for (cqes[0..n]) |cqe| {
                if (self.ring.consumeWake(cqe)) continue;
                if (cqe.user_data < first_fiber_ud) {
                    if (cqe.user_data >= resident_ud_min) if (self.resident) |r| r.cqe(r.ctx, cqe);
                    continue;
                }
                const f: *Fiber = @ptrFromInt(cqe.user_data);
                f.cqe_res = cqe.res;
                f.cqe_flags = cqe.flags;
                f.op_pending = false;
                self.in_flight -= 1;
                if (f.state.wake()) self.pushReady(f);
            }
            if (n < cqes.len) return;
        }
    }

    fn expireTimers(self: *Lane) void {
        const now = clock.monotonicNs();
        while (self.timers.popDue(now)) |f| {
            stats.timer_fires.inc();
            // The pop is a tick only when it is the wake that schedules the
            // fiber; a CQE or message reaped earlier in this pass already did.
            f.timer_won = f.state.wake();
            if (f.timer_won) self.pushReady(f);
        }
    }

    fn runReady(self: *Lane) void {
        var budget: u32 = if (self.poll_budget == 0) std.math.maxInt(u32) else self.poll_budget;
        while (budget > 0) : (budget -= 1) {
            const f = self.popReady() orelse break;
            if (!f.state.beginPoll()) continue;
            self.current = f;
            self.polls_run +%= 1;
            stats.polls.of(self.class).inc();
            const t0 = clock.monotonicNs();
            f.switchIn();
            stats.poll_ns.record(clock.monotonicNs() - t0);
            self.current = null;
            switch (f.msg) {
                .parked => if (f.state.endPollPending()) self.pushReady(f),
                .yielded => {
                    if (!f.state.endPollPending()) assert(f.state.wake());
                    self.pushReady(f);
                },
                .finished => self.finalize(f),
            }
        }
        self.queue_depth.set(self.ready_len);
    }

    /// The fiber returned: hand the result over. Group members have no
    /// awaiter (the group's token is the join); a future's awaiter is a
    /// fiber, a raw thread, or not there yet (then IT recycles on arrival).
    fn finalize(self: *Lane, f: *Fiber) void {
        switch (f.kind) {
            .group => {
                // The join first, the slot last: `live` reaching 0 is the
                // quiescence barrier and must follow the group's own.
                const g = f.cancelWord().groupPtr().?;
                group_mod.memberFinished(g);
                self.recycle(f);
            },
            .future => {
                // acq_rel pairs with the awaiter's swap: its `raw_word` store
                // is visible here, and the result bytes are visible to it.
                const prev = f.awaiter.swap(Fiber.finished, .acq_rel);
                if (prev == null) return;
                if (prev.? == Fiber.raw_awaiter) {
                    const w = f.raw_word.?;
                    // After this store the awaiter may recycle `f`: no access past it.
                    w.store(1, .release);
                    futex.wakeAll(w);
                    return;
                }
                const a = prev.?;
                if (a.state.wake()) a.lane.enqueue(a);
            },
        }
    }

    fn parkOnce(self: *Lane) void {
        const now = clock.monotonicNs();
        var tick: u64 = tick_ns;
        if (self.in_flight > 0) {
            tick = backstop_ns;
            stats.lane_backstop_ticks.inc();
        }
        if (self.timers.peekDeadline()) |d| tick = @min(tick, d -| now);
        // A standing READ is the only way a foreign wake reaches the park;
        // an SQ still full after a flush leaves the tick as the sole transport.
        if (!self.ring.armWake()) tick = @min(tick, backstop_ns);
        if (self.ready_head != null or tick == 0) {
            // Work in hand: a non-blocking flush of this pass's SQEs and loop.
            self.ring.flush();
            return;
        }
        if (self.inbox.tryPop()) |m| {
            self.handleMsg(m);
            self.ring.flush();
            return;
        }
        const verdict = self.ring.park(tick);
        if (self.ring.isDead()) self.onRingDead();
        if (verdict == .tick) {
            // A lost write leaves the coalescer armed with no CQE to disarm
            // it: disarm here so later producers write again, then look.
            self.ring.coalescer.disarm();
            if (self.inbox.tryPop()) |m| {
                self.handleMsg(m);
                // A RESCUE only when nothing could have woken us: the park
                // ran the full tick (not a timer's or the backstop's
                // deadline) and no wake CQE sits in the CQ. A message
                // landing between a short deadline's timeout and this pop
                // is the ordinary race, not a lost write (the 2026-10-07
                // phantom `exec_lane_tick_rescues = 1` under the storm).
                if (tick == tick_ns and self.ring.cqReady() == 0) {
                    stats.lane_tick_rescues.inc();
                    _ = self.tick_rescues.fetchAdd(1, .monotonic);
                }
            }
        }
    }

    /// The ring broke: fail every owed ring op with EIO and let placement
    /// route around this lane (charter §1.8: loud, never a wedge).
    fn onRingDead(self: *Lane) void {
        self.alive.store(false, .release);
        if (!self.ring_dead_logged) {
            self.ring_dead_logged = true;
            log.err("exec: lane {d} ({s}) ring is dead — {d} in-flight ring ops fail EIO; the lane keeps serving its parked fibers on the eventfd and takes no new spawns", .{ self.index, self.name, self.in_flight });
            if (self.resident) |r| r.ringDead(r.ctx);
        }
        for (self.fibers) |*f| {
            if (f.cancelWord().awaiting != .ring_op) continue;
            f.cqe_res = -@as(i32, @intCast(@backingInt(errors.toErrno(error.RingFailed))));
            f.cqe_flags = 0;
            f.op_pending = false;
            if (self.in_flight > 0) self.in_flight -= 1;
            if (f.state.wake()) self.pushReady(f);
        }
    }

    fn pushReady(self: *Lane, f: *Fiber) void {
        f.ready_next = null;
        if (self.ready_tail) |t| t.ready_next = f else self.ready_head = f;
        self.ready_tail = f;
        self.ready_len += 1;
    }

    fn popReady(self: *Lane) ?*Fiber {
        const f = self.ready_head orelse return null;
        self.ready_head = f.ready_next;
        if (self.ready_head == null) self.ready_tail = null;
        f.ready_next = null;
        self.ready_len -= 1;
        return f;
    }

    pub fn view(self: *const Lane) stats.LaneView {
        return .{
            .index = self.index,
            .class = self.class,
            .alive = self.alive.load(.acquire),
            .queue_depth = self.queue_depth.load(),
            .fibers_live = self.live.load(.monotonic),
            .tick_rescues = self.tick_rescues.load(.monotonic),
        };
    }

    /// Test seam: mark the ring dead as a device fence would (the next pass
    /// fails the owed ops and flips `alive`).
    pub fn markRingDeadForTest(self: *Lane) void {
        assert(builtin.is_test);
        self.ring.markDead();
    }
};

// ---------------------------------------------------------------------------
// Tests: the §1.9 ports at the lane level. Fibers here are group-kind
// (recycled by the lane at finish) so the tests need no awaiter protocol;
// `sqz_io.zig` tests the future face.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testLane(class: LaneClass, capacity: u32, drop_first_wake: bool) !*Lane {
    const page = std.heap.pageSize();
    const l = try Lane.init(testing.allocator, .{
        .index = 0,
        .class = class,
        .fiber_capacity = capacity,
        .stack_size = fiber_mod.stackSizeFor(class, page),
        .page = page,
        .drop_first_wake = drop_first_wake,
    });
    errdefer l.deinit();
    try l.start();
    return l;
}

/// A group whose token admits `n` members without an awaiter (the lane's
/// `memberFinished` decrements it; nothing waits).
fn membersGroup(n: usize) Io.Group {
    var g: Io.Group = .init;
    group_mod.statusPtr(&g).store(group_mod.Token.running(n).toInt(), .monotonic);
    return g;
}

const Probe = struct {
    done: futex.Word = .init(0),
    polled: std.atomic.Value(u32) = .init(0),
    hits: std.atomic.Value(u32) = .init(0),

    fn finish(p: *Probe) void {
        _ = p.hits.fetchAdd(1, .monotonic);
        p.done.store(1, .release);
        futex.wakeAll(&p.done);
    }
    fn waitDone(p: *Probe) void {
        while (p.done.load(.acquire) == 0) futex.wait(&p.done, 0);
    }
};

const Ctx = struct { probe: *Probe, lane: *Lane };

fn ctxOf(context: *const anyopaque) Ctx {
    const c: *const Ctx = @ptrCast(@alignCast(context));
    return c.*;
}

fn spawnTest(l: *Lane, g: *Io.Group, start_fn: *const fn (*const anyopaque) void, ctx: Ctx) !*Fiber {
    return l.spawn(.group, .{ .group = start_fn }, std.mem.asBytes(&ctx), g, .unblocked);
}

fn waitQuiescent(l: *Lane) void {
    while (l.live.load(.monotonic) != 0) std.Thread.yield() catch {};
}

test "spawn_runs_to_completion: a foreign spawn completes and its slot returns to the pool; the thread carries the class name" {
    const l = try testLane(.meta, 8, false);
    defer l.deinit();
    try testing.expectEqualStrings("sqz-meta0", l.name);
    var g = membersGroup(1);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            c.probe.finish();
        }
    };
    _ = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    p.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(@as(u32, 1), p.hits.load(.monotonic));
    try testing.expectEqual(@as(usize, 0), group_mod.load(&g).num_running);
    try testing.expectEqual(@as(u64, 0), l.tick_rescues.load(.monotonic));
}

test "foreign_thread_wake_repolls: a fiber parked Pending is re-polled by a wake from a foreign OS thread" {
    const l = try testLane(.meta, 8, false);
    defer l.deinit();
    var g = membersGroup(1);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            const f = currentFiber().?;
            _ = c.probe.polled.fetchAdd(1, .acq_rel);
            f.park();
            _ = c.probe.polled.fetchAdd(1, .acq_rel);
            c.probe.finish();
        }
    };
    const f = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    while (f.state.load() != .idle) try std.Thread.yield();
    try testing.expectEqual(@as(u32, 1), p.polled.load(.acquire));
    // The foreign wake: the word decides (IDLE→SCHEDULED) and we enqueue.
    try testing.expect(f.state.wake());
    l.enqueue(f);
    p.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(@as(u32, 2), p.polled.load(.acquire));
}

test "mid_poll_wake_repolls: a self-wake during the poll re-polls exactly once more (polled == 2)" {
    const l = try testLane(.meta, 8, false);
    defer l.deinit();
    var g = membersGroup(1);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            const f = currentFiber().?;
            _ = c.probe.polled.fetchAdd(1, .acq_rel);
            // RUNNING→NOTIFIED: no enqueue by us; the runner requeues and
            // the park returns once more without any foreign wake.
            std.debug.assert(!f.state.wake());
            f.park();
            _ = c.probe.polled.fetchAdd(1, .acq_rel);
            c.probe.finish();
        }
    };
    _ = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    p.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(@as(u32, 2), p.polled.load(.acquire));
}

test "pluggable_park_never_loses_a_wake: 2000 spawns from 4 threads all complete in < 1 s with 0 tick rescues; same-lane spawns write no eventfd" {
    const l = try testLane(.meta, 64, false);
    defer l.deinit();
    const total = 2000;
    var g = membersGroup(total);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            if (c.probe.hits.fetchAdd(1, .acq_rel) + 1 == total) {
                c.probe.done.store(1, .release);
                futex.wakeAll(&c.probe.done);
            }
        }
        fn producer(lane: *Lane, grp: *Io.Group, probe: *Probe) void {
            var i: u32 = 0;
            while (i < total / 4) {
                _ = spawnTest(lane, grp, run, .{ .probe = probe, .lane = lane }) catch {
                    std.Thread.yield() catch {};
                    continue;
                };
                i += 1;
            }
        }
    };
    const t0 = clock.monotonicNs();
    var ts: [4]std.Thread = undefined;
    for (&ts) |*t| t.* = try std.Thread.spawn(.{}, S.producer, .{ l, &g, &p });
    p.waitDone();
    const elapsed = clock.monotonicNs() - t0;
    for (ts) |t| t.join();
    waitQuiescent(l);
    try testing.expectEqual(@as(u32, total), p.hits.load(.acquire));
    try testing.expect(elapsed < std.time.ns_per_s);
    try testing.expectEqual(@as(u64, 0), l.tick_rescues.load(.monotonic));

    // A spawn from the lane thread itself goes to the ready FIFO: no eventfd write.
    var g2 = membersGroup(2);
    var p2: Probe = .{};
    const Inner = struct {
        fn child(context: *const anyopaque) void {
            ctxOf(context).probe.finish();
        }
        fn parent(context: *const anyopaque) void {
            const c = ctxOf(context);
            const writes = stats.wake_writes.load();
            const same = stats.same_lane_wakes.load();
            var grp = membersGroup(1);
            _ = c.lane.spawn(.group, .{ .group = child }, std.mem.asBytes(&c), &grp, .unblocked) catch unreachable;
            std.debug.assert(stats.wake_writes.load() == writes);
            std.debug.assert(stats.same_lane_wakes.load() == same + 1);
            // The child ran only after we switched out: its group is drained below.
            while (group_mod.load(&grp).num_running != 0) currentFiber().?.yieldNow();
        }
    };
    _ = try spawnTest(l, &g2, Inner.parent, .{ .probe = &p2, .lane = l });
    p2.waitDone();
    waitQuiescent(l);
}

test "SQUEEZEFS_TEST_EXEC_DROP_WAKE: a dropped eventfd write is healed by the park tick, counted once, and the lane stays responsive afterwards" {
    const l = try testLane(.meta, 8, true);
    defer l.deinit();
    var g = membersGroup(2);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            ctxOf(context).probe.finish();
        }
    };
    const rescues_before = stats.lane_tick_rescues.load();
    const t0 = clock.monotonicNs();
    _ = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    p.waitDone();
    const waited = clock.monotonicNs() - t0;
    // The spawn landed only when the 2 s tick looked at the inbox.
    try testing.expect(waited >= tick_ns - 50 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u64, 1), l.tick_rescues.load(.monotonic));
    try testing.expectEqual(rescues_before + 1, stats.lane_tick_rescues.load());
    // The coalescer was disarmed by the rescue: the next wake is written and
    // the second spawn lands within the tick.
    p.done.store(0, .monotonic);
    const t1 = clock.monotonicNs();
    _ = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    p.waitDone();
    try testing.expect(clock.monotonicNs() - t1 < tick_ns / 2);
    waitQuiescent(l);
}

test "a timer deadline's timeout that finds a fresh inbox message is NOT a rescue: 1 ms timer parks under a foreign spawn storm count 0 tick rescues (the phantom exec_lane_tick_rescues under the smoke's storm)" {
    const l = try testLane(.meta, 64, false);
    defer l.deinit();
    const Timed = struct {
        /// A fiber keeping a 1 ms timer armed on the lane for ~300 ms: every
        /// park's bound is the timer, never the full tick.
        fn ticker(context: *const anyopaque) void {
            const c = ctxOf(context);
            const f = currentFiber().?;
            const until = clock.monotonicNs() + 300 * std.time.ns_per_ms;
            while (clock.monotonicNs() < until) {
                c.lane.timers.arm(f, clock.monotonicNs() + std.time.ns_per_ms);
                f.park();
                _ = c.lane.timers.cancel(f);
            }
            c.probe.finish();
        }
        fn noop(context: *const anyopaque) void {
            _ = ctxOf(context).probe.hits.fetchAdd(1, .monotonic);
        }
        fn producer(lane: *Lane, grp: *Io.Group, probe: *Probe, stop: *std.atomic.Value(bool)) void {
            while (!stop.load(.acquire)) {
                _ = spawnTest(lane, grp, noop, .{ .probe = probe, .lane = lane }) catch {};
                std.Thread.yield() catch {};
            }
        }
    };
    var gt = membersGroup(1);
    var pt: Probe = .{};
    _ = try spawnTest(l, &gt, Timed.ticker, .{ .probe = &pt, .lane = l });
    var gp = membersGroup(1 << 20);
    var pp: Probe = .{};
    var stop: std.atomic.Value(bool) = .init(false);
    const t = try std.Thread.spawn(.{}, Timed.producer, .{ l, &gp, &pp, &stop });
    pt.waitDone();
    stop.store(true, .release);
    t.join();
    waitQuiescent(l);
    try testing.expect(pp.hits.load(.monotonic) > 100);
    try testing.expectEqual(@as(u64, 0), l.tick_rescues.load(.monotonic));
}

test "poll budget 1 (the fuse_ur posture): one fiber per pass, every fiber still completes" {
    const page = std.heap.pageSize();
    const l = try Lane.init(testing.allocator, .{ .index = 3, .class = .fuse_ur, .fiber_capacity = 8, .stack_size = fiber_mod.stackSizeFor(.fuse_ur, page), .page = page, .poll_budget = 1 });
    defer l.deinit();
    try l.start();
    try testing.expectEqualStrings("sqz-fuse-ur3", l.name);
    var g = membersGroup(4);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            if (c.probe.hits.fetchAdd(1, .acq_rel) + 1 == 4) {
                c.probe.done.store(1, .release);
                futex.wakeAll(&c.probe.done);
            }
        }
    };
    for (0..4) |_| _ = try spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l });
    p.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(@as(u32, 4), p.hits.load(.acquire));
}

test "the exit edges: a stop while parked returns at once; a stop with ready work exits at the next pass top; a ring refusal is a loud start error; capacity is a refusal" {
    const l = try testLane(.meta, 2, false);
    const t0 = clock.monotonicNs();
    l.stopAndJoin();
    try testing.expect(clock.monotonicNs() - t0 < tick_ns / 2);
    try testing.expect(!l.alive.load(.acquire));
    // Spawning onto a stopped lane is refused, not queued forever.
    var g = membersGroup(1);
    var p: Probe = .{};
    const S = struct {
        fn run(context: *const anyopaque) void {
            ctxOf(context).probe.finish();
        }
    };
    try testing.expectError(error.ConcurrencyUnavailable, spawnTest(l, &g, S.run, .{ .probe = &p, .lane = l }));
    l.deinit();

    // Capacity: 2 slots, the third spawn is refused and counted.
    const l2 = try testLane(.meta, 2, false);
    defer l2.deinit();
    var g2 = membersGroup(3);
    var gate: Probe = .{};
    var p2: Probe = .{};
    const Hold = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            while (c.probe.done.load(.acquire) == 0) currentFiber().?.yieldNow();
            _ = c.probe.hits.fetchAdd(1, .acq_rel);
        }
    };
    const refused_before = stats.spawn_refused.of(.meta).load();
    _ = try spawnTest(l2, &g2, Hold.run, .{ .probe = &gate, .lane = l2 });
    _ = try spawnTest(l2, &g2, Hold.run, .{ .probe = &gate, .lane = l2 });
    try testing.expectError(error.ConcurrencyUnavailable, spawnTest(l2, &g2, S.run, .{ .probe = &p2, .lane = l2 }));
    try testing.expectEqual(refused_before + 1, stats.spawn_refused.of(.meta).load());
    gate.done.store(1, .release);
    waitQuiescent(l2);
    try testing.expectEqual(@as(u32, 2), gate.hits.load(.acquire));
}

test "a resident: started/stopping run on the lane thread, a NOP under a resident tag completes to its cqe hook (never to a fiber), the ring depth override holds, enters are counted" {
    const Res = struct {
        started_on: std.atomic.Value(u64) = .init(0),
        stopped_on: std.atomic.Value(u64) = .init(0),
        cqe_ud: std.atomic.Value(u64) = .init(0),
        done: futex.Word = .init(0),

        fn started(ctx: *anyopaque) void {
            const r: *@This() = @ptrCast(@alignCast(ctx));
            r.started_on.store(@intCast(linux.gettid()), .release);
        }
        fn cqe(ctx: *anyopaque, c: linux.io_uring_cqe) void {
            const r: *@This() = @ptrCast(@alignCast(ctx));
            r.cqe_ud.store(c.user_data, .release);
            r.done.store(1, .release);
            futex.wakeAll(&r.done);
        }
        fn ringDead(_: *anyopaque) void {}
        fn stopping(ctx: *anyopaque) void {
            const r: *@This() = @ptrCast(@alignCast(ctx));
            r.stopped_on.store(@intCast(linux.gettid()), .release);
        }
        fn resident(r: *@This()) Resident {
            return .{ .ctx = r, .started = started, .cqe = cqe, .ringDead = ringDead, .stopping = stopping };
        }
        /// Issued from a fiber on the lane (the resident's own thread).
        fn issue(context: *const anyopaque) void {
            const l = ctxOf(context).lane;
            const sqe = l.ring.getSqe().?;
            sqe.prep_nop();
            sqe.user_data = resident_ud_max;
        }
    };
    var r: Res = .{};
    const page = std.heap.pageSize();
    const l = try Lane.init(testing.allocator, .{ .index = 5, .class = .journal, .fiber_capacity = 4, .stack_size = fiber_mod.stackSizeFor(.journal, page), .page = page, .ring_entries = 256, .resident = r.resident() });
    defer l.deinit();
    try l.start();
    try testing.expectEqualStrings("sqz-jrnl5", l.name);
    try testing.expect(r.started_on.load(.acquire) != 0);
    try testing.expect(r.started_on.load(.acquire) != @as(u64, @intCast(linux.gettid())));
    try testing.expectEqual(@as(u32, 256), l.ring.ring.sq.sqes.len);
    const enters_before = l.enters();
    var g = membersGroup(1);
    var p: Probe = .{};
    _ = try spawnTest(l, &g, Res.issue, .{ .probe = &p, .lane = l });
    while (r.done.load(.acquire) == 0) futex.wait(&r.done, 0);
    try testing.expectEqual(resident_ud_max, r.cqe_ud.load(.acquire));
    try testing.expect(l.enters() > enters_before);
    waitQuiescent(l);
    l.stopAndJoin();
    try testing.expectEqual(r.started_on.load(.acquire), r.stopped_on.load(.acquire));
}

test "the idle-trim law on a lane's stacks: every slot a spawn ever touched stays mapped; trimIdleStacks returns the pages of free slots idle ≥ min ticks (the mapped-not-returned gauge falls by the usable region), mincore agrees, a re-spawn re-arms the slot and the gauge rises again" {
    // Capacity 4 = the population: after four spawns every slot is mapped,
    // so the fifth must re-arm a trimmed one.
    const l = try testLane(.meta, 4, false);
    defer l.deinit();
    var idle_clock: std.atomic.Value(u64) = .init(100);
    l.clock = &idle_clock;
    const S = struct {
        fn run(context: *const anyopaque) void {
            // A frame deep enough to touch a few pages, then finish.
            var pad: [20_000]u8 = undefined;
            @memset(&pad, 7);
            std.mem.doNotOptimizeAway(&pad);
            ctxOf(context).probe.finish();
        }
    };
    const before = stats.fiber_stack_bytes.load();
    var g = membersGroup(4);
    var probes: [4]Probe = @splat(.{});
    for (&probes) |*p| _ = try spawnTest(l, &g, S.run, .{ .probe = p, .lane = l });
    for (&probes) |*p| p.waitDone();
    waitQuiescent(l);
    // 4 slots touched: 4 stacks mapped and counted (the FIFO pool hands out
    // distinct slots), none trimmed yet.
    var mapped: u32 = 0;
    var resident: u32 = 0;
    for (l.fibers) |*f| if (f.stack) |s| {
        mapped += 1;
        if (!s.trimmed) resident += 1;
    };
    try testing.expectEqual(@as(u32, 4), mapped);
    try testing.expectEqual(@as(u32, 4), resident);
    try testing.expectEqual(before + 4 * (l.stack_size + l.page), stats.fiber_stack_bytes.load());
    // Yellow at tick 101: idle 1 < 2 — nothing returns.
    idle_clock.store(101, .monotonic);
    try testing.expectEqual(@as(u32, 0), l.trimIdleStacks(2));
    // Tick 102: every free touched stack returns its usable region; the
    // guard and the reserve stay mapped.
    idle_clock.store(102, .monotonic);
    try testing.expect(l.residentStackBytes() >= 4 * 20_000);
    try testing.expectEqual(@as(u32, 4), l.trimIdleStacks(2));
    try testing.expectEqual(before + 4 * l.page, stats.fiber_stack_bytes.load());
    try testing.expectEqual(@as(u64, 0), l.residentStackBytes());
    var vec: [1 << 10]u8 = undefined;
    for (l.fibers) |*f| if (f.stack) |s| {
        try testing.expect(s.trimmed);
        const u = s.usable();
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall3(.mincore, @intFromPtr(u.ptr), u.len, @intFromPtr(&vec))));
        for (vec[0 .. u.len / l.page]) |v| try testing.expectEqual(@as(u8, 0), v & 1);
    };
    // A second pass finds nothing to do.
    try testing.expectEqual(@as(u32, 0), l.trimIdleStacks(0));
    // Re-spawn: the slot re-arms (the canary is refilled in test builds), the gauge counts it again.
    var g2 = membersGroup(1);
    var p2: Probe = .{};
    _ = try spawnTest(l, &g2, S.run, .{ .probe = &p2, .lane = l });
    p2.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(before + 4 * l.page + l.stack_size, stats.fiber_stack_bytes.load());
    var rearmed: u32 = 0;
    for (l.fibers) |*f| if (f.stack) |s| {
        if (!s.trimmed) rearmed += 1;
    };
    try testing.expectEqual(@as(u32, 1), rearmed);
}

test "the idle trim never takes a slot out of the spawn pool: while a pass holds a free slot for its trim, every slot no fiber lives on is still where a spawn looks (0003 §W.4)" {
    const l = try testLane(.meta, 2, false);
    defer l.deinit();
    const Station = struct {
        lane: *Lane,
        held: u32 = 0,
        /// The fewest slots a spawn could pop while a trim held one.
        least_poppable: u32 = std.math.maxInt(u32),
        fn holding(ctx: *anyopaque, _: *Fiber) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.held += 1;
            var got: [2]*Fiber = undefined;
            var n: u32 = 0;
            while (self.lane.free.tryPop()) |g| : (n += 1) got[n] = g;
            for (got[0..n]) |g| self.lane.free.pushSpin(g);
            self.least_poppable = @min(self.least_poppable, n);
        }
    };
    var st: Station = .{ .lane = l };
    l.trim_station = .{ .ctx = &st, .holding = &Station.holding };
    defer l.trim_station = null;
    // No fiber lives: both slots are free and the pass takes each in turn.
    _ = l.trimIdleStacks(0);
    try testing.expect(st.held >= 1);
    try testing.expectEqual(@as(u32, 2), st.least_poppable);
}

test "the stack trimmer races spawns and recycles: a foreign thread trims at min 0 in a tight loop while fibers spawn, park, are woken and finish; every state transition holds and every spawn lands" {
    const l = try testLane(.meta, 8, false);
    defer l.deinit();
    const Shared = struct {
        lane: *Lane,
        stop: std.atomic.Value(bool) = .init(false),
        trims: std.atomic.Value(u64) = .init(0),
        fn trimmer(sh: *@This()) void {
            while (!sh.stop.load(.acquire)) _ = sh.trims.fetchAdd(sh.lane.trimIdleStacks(0), .monotonic);
        }
    };
    var sh: Shared = .{ .lane = l };
    const t = try std.Thread.spawn(.{}, Shared.trimmer, .{&sh});
    // The trimmer stops and joins on EVERY exit: a failing spawn's early
    // return used to free the lane under it (its `tryPop` on the freed
    // ring — the gate's SIGABRT).
    var joined = false;
    defer if (!joined) {
        sh.stop.store(true, .release);
        t.join();
    };
    const S = struct {
        fn run(context: *const anyopaque) void {
            const c = ctxOf(context);
            var pad: [8192]u8 = undefined;
            @memset(&pad, 3);
            std.mem.doNotOptimizeAway(&pad);
            const f = currentFiber().?;
            _ = c.probe.polled.fetchAdd(1, .acq_rel);
            f.park();
            c.probe.finish();
        }
    };
    var round: u32 = 0;
    while (round < 200) : (round += 1) {
        var g = membersGroup(6);
        var probes: [6]Probe = @splat(.{});
        var fibers: [6]*Fiber = undefined;
        for (&probes, 0..) |*p, i| fibers[i] = try spawnTest(l, &g, S.run, .{ .probe = p, .lane = l });
        for (&probes, 0..) |*p, i| {
            while (p.polled.load(.acquire) == 0) std.Thread.yield() catch {};
            if (fibers[i].state.wake()) l.enqueue(fibers[i]);
            p.waitDone();
        }
        waitQuiescent(l);
    }
    sh.stop.store(true, .release);
    t.join();
    joined = true;
    try testing.expect(sh.trims.load(.monotonic) > 0);
    try testing.expectEqual(@as(u32, 0), l.live.load(.monotonic));
}

test "a spawn that pops a slot the idle trim holds waits the trim out and lands (counted), never refused; the trim's release re-arms nothing it should not (0003 §W.4)" {
    const l = try testLane(.meta, 1, false);
    defer l.deinit();
    const Station = struct {
        held: std.atomic.Value(u32) = .init(0),
        go: std.atomic.Value(u32) = .init(0),
        fn holding(ctx: *anyopaque, _: *Fiber) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.held.store(1, .release);
            futex.wakeAll(&self.held);
            while (self.go.load(.acquire) == 0) futex.wait(&self.go, 0);
        }
    };
    var st: Station = .{};
    l.trim_station = .{ .ctx = &st, .holding = &Station.holding };
    defer l.trim_station = null;
    const Trim = struct {
        fn run(lane: *Lane) void {
            _ = lane.trimIdleStacks(0);
        }
    };
    const t = try std.Thread.spawn(.{}, Trim.run, .{l});
    var t_joined = false;
    defer if (!t_joined) {
        st.go.store(1, .release);
        futex.wakeAll(&st.go);
        t.join();
    };
    while (st.held.load(.acquire) == 0) futex.wait(&st.held, 0);
    // The lane's one slot is under trim: a spawn pops it and waits.
    const S = struct {
        fn run(context: *const anyopaque) void {
            ctxOf(context).probe.finish();
        }
        fn spawner(lane: *Lane, g: *Io.Group, p: *Probe, out: *?anyerror, returned: *std.atomic.Value(u32)) void {
            _ = spawnTest(lane, g, run, .{ .probe = p, .lane = lane }) catch |e| {
                out.* = e;
            };
            returned.store(1, .release);
        }
    };
    const waits0 = stats.spawn_trim_waits.load();
    var g = membersGroup(1);
    var p: Probe = .{};
    var spawn_err: ?anyerror = null;
    var returned: std.atomic.Value(u32) = .init(0);
    const sp = try std.Thread.spawn(.{}, S.spawner, .{ l, &g, &p, &spawn_err, &returned });
    // The rendezvous: the spawner is waiting on the claim (or returned —
    // the shipped trimmer's refusal).
    while (stats.spawn_trim_waits.load() == waits0 and returned.load(.acquire) == 0) std.Thread.yield() catch {};
    st.go.store(1, .release);
    futex.wakeAll(&st.go);
    t.join();
    t_joined = true;
    sp.join();
    try testing.expectEqual(@as(?anyerror, null), spawn_err);
    p.waitDone();
    waitQuiescent(l);
    try testing.expectEqual(waits0 + 1, stats.spawn_trim_waits.load());
    try testing.expectEqual(Lane.CLAIM_FREE, l.claims[0].load(.acquire));
}

test "ring entries derivation: power of two above the fiber population + wake + cancels, floor 8, ceiling 4096" {
    try testing.expectEqual(@as(u16, 16), ringEntriesFor(4));
    try testing.expectEqual(@as(u16, 128), ringEntriesFor(64));
    try testing.expectEqual(@as(u16, 512), ringEntriesFor(256));
    try testing.expectEqual(@as(u16, 4096), ringEntriesFor(100_000));
    // The wake READ + cancel slack alone want 9 entries: 16 is the least ring.
    try testing.expectEqual(@as(u16, 16), ringEntriesFor(0));
}

test "comm names: every class at its widest index fits the 15-char budget" {
    var buf: thread_name.CommBuf = undefined;
    try testing.expectEqualStrings("sqz-fuse-ln9999", Lane.commNameFor(&buf, .fuse, 9999));
    try testing.expectEqualStrings("sqz-fuse-ur8191", Lane.commNameFor(&buf, .fuse_ur, 8191));
    try testing.expectEqualStrings("sqz-meta1", Lane.commNameFor(&buf, .meta, 1));
    try testing.expectEqualStrings("sqz-lease", Lane.commNameFor(&buf, .lease, 0));
    try testing.expectEqualStrings("sqz-jrnl9999", Lane.commNameFor(&buf, .journal, 9999));
    try testing.expectEqualStrings("", Lane.commNameFor(&buf, .main, 0));
}
