//! `SqzIo`: our own `std.Io` implementation (decision 0001; charter §9;
//! `docs/porting/exec.md` §3.1). ONE process-wide instance, ONE vtable:
//! `Io.Threaded` is embedded as the FIRST field and its vtable entries are
//! delegated for every blocking dir/file/process/net operation (their
//! syscalls run uncancelable on our threads — `Thread.current` is null off a
//! Threaded worker, which is exactly the committer posture the charter
//! wants); OURS are the task protocol (`async/concurrent/await/cancel`, the
//! groups), cancel protection, the futex ops (a fiber parks, never the lane
//! thread — `futex_table.zig`), `sleep`/`now`, `operate` (lane-resident file
//! streaming is an SQE on the lane's own ring), and `Batch` (fails loud on a
//! lane — decision 0003 §D Q-D).
//!
//! Placement (§3.4): a spawn from a lane thread lands on THAT lane
//! (`SQUEEZEFS_FUSE_SAME_LANE_DISPATCH`; the rotation is the control arm); a
//! spawn from a non-lane thread lands on the `sqz-meta` pool round-robin;
//! `spawnHome(cpu, node)` is the transport's handoff to the lane homed on
//! the requester's CPU, else its node, else the rotation — dead lanes are
//! walked around (`exec_dead_lane_redispatches`) and a class with every lane
//! dead aborts loud: a daemon that can never again answer a request must
//! fail, not wedge every mount user.
//!
//! `run` makes the calling thread a `main`-class lane and drives the root
//! task to completion: the CLI's "thread-park `block_on`" IS a ring park.
//!
//! Linux only.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const posix = std.posix;
const Io = std.Io;
const Alignment = std.mem.Alignment;
const assert = std.debug.assert;

const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const counters = @import("../core/counters.zig");
const futex = @import("../sync/futex.zig");
const mem_budget = @import("../mem/budget.zig");
const fiber_mod = @import("fiber.zig");
const lane_mod = @import("lane.zig");
const lock_order = @import("../sync/lock_order.zig");
const group_mod = @import("group.zig");
const futex_table = @import("futex_table.zig");
const placement = @import("placement.zig");
const blocking = @import("blocking.zig");
const timer_thread = @import("timer_thread.zig");
const fdwatch = @import("fdwatch.zig");
const timer_heap = @import("timer_heap.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/sqz_io is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;
pub const Lane = lane_mod.Lane;
pub const LaneClass = stats.LaneClass;

/// The transport geometry the fuse lanes are sized from (charter §4):
/// `fiber_capacity(fuse lane) = q_depth × ceil(nqueues / fuse_lanes) + slack`
/// — resident handler fibers cannot exceed the slot count (RES-18:
/// backpressure is the kernel's), plus `spawn_slack` for the lane's own
/// detached helpers.
pub const FuseGeometry = struct {
    nqueues: u32,
    q_depth: u32,
    /// The most rings (drain workers, each driving a lane of its own) the
    /// transport registers on one queue: the driven-lane slots reserved per
    /// queue (`fuse/geometry.MAX_RINGS_PER_QUEUE`; the mount probes the
    /// kernel for the count only after this executor exists).
    rings_per_queue: u32 = 1,
};

/// The fibers a lane holds beyond its slot count for its own detached
/// helpers (the prefetch issuer, the hold ceremony; charter §4's capacity
/// law above).
pub const spawn_slack: u32 = 16;

/// Default populations (a handful of mostly-parked loops — tie-tested).
pub const meta_capacity_default: u32 = 256;
pub const lease_capacity_default: u32 = 16;
pub const main_capacity_default: u32 = 16;

/// The fiber-stack virtual reserve may claim at most an eighth of the R5
/// budget (the share charter §4 gives the transport payload arena); floor
/// 16 so a tiny budget still yields a usable lane.
pub fn capacityFromBudget(requested: u32, budget_bytes: u64, stack_size: usize) u32 {
    const by_budget: u64 = @max(16, budget_bytes / 8 / stack_size);
    return @intCast(@min(requested, by_budget));
}

pub fn fuseCapacity(geometry: FuseGeometry, fuse_lanes: u32) u32 {
    return @intCast(geometry.q_depth * derive.ceilDiv(geometry.nqueues, @max(fuse_lanes, 1)) + spawn_slack);
}

pub const Options = struct {
    /// The process environment (knobs); null = an empty map owned here.
    env: ?*const std.process.Environ.Map = null,
    /// The machine; null = `derive.facts()`.
    facts: ?derive.Facts = null,
    /// The R5 budget the fiber reserve derives from.
    budget_bytes: u64,
    /// Register the fiber-stack component here (non-sheddable).
    budget: ?*mem_budget.MemBudget = null,
    /// Fuse handler lanes: one per lane CPU of the topology; null = none.
    fuse: ?FuseGeometry = null,
    /// null = `placement.metaLanesFrom(cpus)`.
    meta_lanes: ?u16 = null,
    meta_capacity: u32 = meta_capacity_default,
    lease_lane: bool = false,
    /// null = `blocking.poolCapFrom(cpus)`.
    blocking_cap: ?u32 = null,
    blocking_idle_reap_ns: ?u64 = blocking.idle_reap_ns,
};

pub const InitError = std.mem.Allocator.Error || Lane.StartError || std.Thread.SpawnError;

pub const SqzIo = struct {
    /// FIRST: `userdata` points here; the delegated Threaded functions get
    /// a valid `*Threaded`, ours recover the owner with `@fieldParentPtr`.
    threaded: Io.Threaded,
    gpa: std.mem.Allocator,
    owned_env: ?*std.process.Environ.Map,
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    topo: placement.Topology,
    config: placement.Config,
    same_lane_dispatch: bool,
    /// Every lane: `[fuse…][meta…][lease?][main]`.
    lanes: []*Lane,
    fuse_count: u16,
    meta_start: u16,
    meta_count: u16,
    lease_index: ?u16,
    main_index: u16,
    fuse_placement: placement.Placement,
    meta_next: std.atomic.Value(u32) = .init(0),
    futexes: futex_table.FutexTable = .init,
    timer: *timer_thread.TimerThread,
    blocking_pool: *blocking.BlockingPool,
    /// `sqz-lease-io` (the dedicated worker), with the lease lane.
    lease_io: ?*blocking.BlockingPool,
    fuse_alive: []*const std.atomic.Value(bool),
    fuse_homes: []placement.Placement.LaneHome,
    stack_component: mem_budget.Component,
    /// The DRIVEN lanes (decision 0003 §AB.3): owned and run by the FUSE
    /// drain workers, listed here for the stats, the stack gauge and the
    /// idle trim — one slot per queue (a group drives at most one).
    driven: []?*Lane = &.{},
    /// Held for a list edit or one walk of it (never across a park).
    driven_lock: std.atomic.Value(bool) = .init(false),

    fn fiberContext() usize {
        return @intFromPtr(lane_mod.currentFiber() orelse return 0);
    }

    fn residentStackBytesHook(ctx: *anyopaque) u64 {
        const self: *SqzIo = @ptrCast(@alignCast(ctx));
        var n: u64 = 0;
        for (self.lanes) |l| n += l.residentStackBytes();
        self.lockDriven();
        defer self.unlockDriven();
        for (self.driven) |d| if (d) |l| {
            n += l.residentStackBytes();
        };
        stats.fiber_stack_resident_bytes.set(n);
        return n;
    }

    fn lockDriven(self: *SqzIo) void {
        // acquire pairs with `unlockDriven`'s release.
        while (self.driven_lock.swap(true, .acquire)) std.atomic.spinLoopHint();
    }

    fn unlockDriven(self: *SqzIo) void {
        self.driven_lock.store(false, .release);
    }

    /// List a driven lane (its owner keeps it); false = no slot left.
    pub fn adoptDriven(self: *SqzIo, lane: *Lane) bool {
        self.lockDriven();
        defer self.unlockDriven();
        for (self.driven) |*d| if (d.* == null) {
            d.* = lane;
            return true;
        };
        return false;
    }

    /// Before its owner frees it.
    pub fn releaseDriven(self: *SqzIo, lane: *Lane) void {
        self.lockDriven();
        defer self.unlockDriven();
        for (self.driven) |*d| if (d.* == lane) {
            d.* = null;
        };
    }

    /// The idle-trim hook (`mem_budget.TrimFn`): every lane's free slots.
    fn trimIdleStacksHook(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *SqzIo = @ptrCast(@alignCast(ctx));
        _ = self.trimIdleStacks(min_idle_ticks);
    }

    /// Return the stack pages of every free slot idle ≥ `min_idle_ticks` on
    /// every lane; the count trimmed.
    pub fn trimIdleStacks(self: *SqzIo, min_idle_ticks: u64) u32 {
        var n: u32 = 0;
        for (self.lanes) |l| n += l.trimIdleStacks(min_idle_ticks);
        self.lockDriven();
        defer self.unlockDriven();
        for (self.driven) |d| if (d) |l| {
            n += l.trimIdleStacks(min_idle_ticks);
        };
        return n;
    }

    pub fn init(gpa: std.mem.Allocator, options: Options) InitError!*SqzIo {
        const self = try gpa.create(SqzIo);
        errdefer gpa.destroy(self);
        self.* = .{
            .threaded = undefined,
            .gpa = gpa,
            .owned_env = null,
            .env = undefined,
            .facts = options.facts orelse derive.facts(),
            .topo = .empty,
            .config = undefined,
            .same_lane_dispatch = true,
            .lanes = &.{},
            .fuse_count = 0,
            .meta_start = 0,
            .meta_count = 0,
            .lease_index = null,
            .main_index = 0,
            .fuse_placement = undefined,
            .timer = undefined,
            .blocking_pool = undefined,
            .lease_io = null,
            .fuse_alive = &.{},
            .fuse_homes = &.{},
            .stack_component = undefined,
        };
        if (options.env) |e| {
            self.env = e;
        } else {
            const m = try gpa.create(std.process.Environ.Map);
            m.* = .init(gpa);
            self.owned_env = m;
            self.env = m;
        }
        errdefer if (self.owned_env) |m| {
            m.deinit();
            gpa.destroy(m);
        };
        self.config = placement.Config.fromEnv(self.env);
        self.same_lane_dispatch = knobs.boolKnob("SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", self.env);
        // The lock-order rail keys its ledger by fiber on a lane thread
        // (fibers interleave on one thread and never migrate).
        lock_order.current_context = &fiberContext;
        // Neither limit admits a pool thread: a stray delegate path can never
        // spawn Threaded workers (risk §3.12.6).
        self.threaded = Io.Threaded.init(gpa, .{ .async_limit = .nothing, .concurrent_limit = .nothing });
        errdefer self.threaded.deinit();

        self.timer = timer_thread.TimerThread.init(gpa);
        errdefer self.timer.deinit();
        try self.timer.start();
        self.blocking_pool = try blocking.BlockingPool.init(gpa, self.timer, .{
            .cap = options.blocking_cap orelse blocking.poolCapFrom(self.facts.cpus),
            .idle_reap_ns = options.blocking_idle_reap_ns,
        });
        errdefer self.blocking_pool.deinit();

        const page: usize = self.facts.page_size;
        self.topo = placement.Topology.probe(self.threaded.io());
        var lane_cpus = placement.CpuSet.empty;
        if (options.fuse != null) lane_cpus = self.topo.laneCpus();
        const fuse_count: u16 = @intCast(lane_cpus.count());
        const meta_count: u16 = options.meta_lanes orelse placement.metaLanesFrom(self.facts.cpus);
        const lease_count: u16 = @intFromBool(options.lease_lane);
        const total: usize = @as(usize, fuse_count) + meta_count + lease_count + 1;
        const lanes = try gpa.alloc(*Lane, total);
        errdefer gpa.free(lanes);
        var built: usize = 0;
        errdefer for (lanes[0..built]) |l| l.deinit();

        var it = lane_cpus.iterator(.{});
        var i: u16 = 0;
        while (it.next()) |cpu| : (i += 1) {
            const home: u16 = @intCast(cpu);
            const node = self.topo.node_of[cpu];
            const stack_size = fiber_mod.stackSizeFor(.fuse, page);
            lanes[built] = try Lane.init(gpa, .{
                .index = i,
                .class = .fuse,
                .home_cpu = home,
                .node = if (node == placement.no_node) null else node,
                .fiber_capacity = capacityFromBudget(fuseCapacity(options.fuse.?, fuse_count), options.budget_bytes, stack_size),
                .stack_size = stack_size,
                .page = page,
                .affinity = if (self.config.numa) placement.scopedAffinityCpus(self.config.scope, home, lane_cpus, &self.topo) else null,
                .drop_first_wake = knobs.boolKnob("SQUEEZEFS_TEST_EXEC_DROP_WAKE", self.env),
            });
            built += 1;
        }
        self.fuse_count = fuse_count;
        self.meta_start = fuse_count;
        self.meta_count = meta_count;
        var m: u16 = 0;
        while (m < meta_count) : (m += 1) {
            const stack_size = fiber_mod.stackSizeFor(.meta, page);
            lanes[built] = try Lane.init(gpa, .{
                .index = m,
                .class = .meta,
                .fiber_capacity = capacityFromBudget(options.meta_capacity, options.budget_bytes, stack_size),
                .stack_size = stack_size,
                .page = page,
                .drop_first_wake = knobs.boolKnob("SQUEEZEFS_TEST_EXEC_DROP_WAKE", self.env),
            });
            built += 1;
        }
        if (options.lease_lane) {
            const stack_size = fiber_mod.stackSizeFor(.lease, page);
            lanes[built] = try Lane.init(gpa, .{
                .index = 0,
                .class = .lease,
                .fiber_capacity = capacityFromBudget(lease_capacity_default, options.budget_bytes, stack_size),
                .stack_size = stack_size,
                .page = page,
            });
            self.lease_index = @intCast(built);
            built += 1;
            self.lease_io = try blocking.BlockingPool.init(gpa, self.timer, .{ .cap = 1, .idle_reap_ns = null, .name = "sqz-lease-io", .numbered = false });
        }
        errdefer if (self.lease_io) |p| p.deinit();
        {
            const stack_size = fiber_mod.stackSizeFor(.main, page);
            lanes[built] = try Lane.init(gpa, .{
                .index = 0,
                .class = .main,
                .fiber_capacity = capacityFromBudget(main_capacity_default, options.budget_bytes, stack_size),
                .stack_size = stack_size,
                .page = page,
            });
            self.main_index = @intCast(built);
            built += 1;
        }
        self.lanes = lanes;

        // Start every threaded lane (main attaches in `run`); a refused ring
        // fails init loud.
        var started: usize = 0;
        errdefer for (lanes[0..started]) |l| l.stopAndJoin();
        for (lanes[0 .. total - 1]) |l| {
            try l.start();
            started += 1;
        }

        const homes = try gpa.alloc(placement.Placement.LaneHome, fuse_count);
        errdefer gpa.free(homes);
        const alive = try gpa.alloc(*const std.atomic.Value(bool), fuse_count);
        errdefer gpa.free(alive);
        for (lanes[0..fuse_count], 0..) |l, k| {
            homes[k] = .{ .home_cpu = l.home_cpu, .node = if (self.config.numa) l.node else null };
            alive[k] = &l.alive;
        }
        self.fuse_alive = alive;
        self.fuse_homes = homes;
        if (options.fuse) |g| {
            self.driven = try gpa.alloc(?*Lane, g.nqueues * g.rings_per_queue);
            @memset(self.driven, null);
        }
        errdefer gpa.free(self.driven);
        self.fuse_placement = try placement.Placement.init(gpa, homes, alive, self.topo.node_count);

        // Non-sheddable (the live frames are the handlers'), but the idle
        // slots' pages return under the idle-trim law; the gauge is the
        // measured residency, published for the stats at every read.
        self.stack_component = mem_budget.Component.init("exec_fiber_stacks", 0, 0, .{ .func = .{ .ctx = self, .call = &residentStackBytesHook } }, null).withTrim(.{ .ctx = self, .call = &trimIdleStacksHook }).kernelWalk();
        if (options.budget) |b| b.register(self.stack_component);
        return self;
    }

    /// Stop and join every lane and thread (quiescent executors only: every
    /// fiber must have been awaited or recycled), free everything.
    pub fn deinit(self: *SqzIo) void {
        for (self.lanes) |l| l.stopAndJoin();
        if (self.lease_io) |p| p.deinit();
        self.blocking_pool.deinit();
        self.timer.deinit();
        self.fuse_placement.deinit(self.gpa);
        self.gpa.free(self.fuse_homes);
        self.gpa.free(self.fuse_alive);
        self.gpa.free(self.driven);
        for (self.lanes) |l| l.deinit();
        self.gpa.free(self.lanes);
        self.threaded.deinit();
        if (self.owned_env) |m| {
            m.deinit();
            self.gpa.destroy(m);
        }
        self.gpa.destroy(self);
    }

    /// The ONE `Io` handle.
    pub fn io(self: *SqzIo) Io {
        return .{ .userdata = &self.threaded, .vtable = &vtable };
    }

    /// The unit-test executor: 2 meta lanes, no fuse lanes, an empty
    /// environment, a 1 GiB synthetic budget (`SqzIo.testInstance(lanes = 2)`
    /// in `exec.md` §3.10).
    pub fn testInstance() InitError!*SqzIo {
        assert(builtin.is_test);
        return init(std.testing.allocator, .{
            .budget_bytes = derive.GiB,
            .meta_lanes = 2,
            .blocking_cap = 8,
        });
    }

    fn ownerOf(userdata: ?*anyopaque) *SqzIo {
        const t: *Io.Threaded = @ptrCast(@alignCast(userdata));
        return @fieldParentPtr("threaded", t);
    }

    /// The executor behind an `Io` handle this instance issued (a module
    /// holding only the `Io` — the backend — reaches `spawnMeta` /
    /// `runBlocking` through it). Asserts the handle is ours.
    pub fn fromIo(io_handle: Io) *SqzIo {
        assert(io_handle.vtable == &vtable);
        return ownerOf(io_handle.userdata);
    }

    pub fn fuseLanes(self: *SqzIo) []*Lane {
        return self.lanes[0..self.fuse_count];
    }

    pub fn metaLanes(self: *SqzIo) []*Lane {
        return self.lanes[self.meta_start .. self.meta_start + self.meta_count];
    }

    pub fn mainLane(self: *SqzIo) *Lane {
        return self.lanes[self.main_index];
    }

    // ---- placement ----

    /// The lane an anonymous `concurrent` lands on (§3.4): the current lane
    /// (same-lane dispatch), the fuse rotation when the lever is off on a
    /// fuse lane, the meta round-robin off a lane. A DRIVEN lane is its
    /// owner's ring thread (the drain worker, which never waits on a
    /// device — decision 0003 §AB.3): an anonymous task spawned from it —
    /// a write handler's upload, fold or overlay drain, whose DMAs park the
    /// calling thread — goes to the fuse lane homed on the owner's CPU, as
    /// it would from a handler there.
    fn pickLane(self: *SqzIo) ?*Lane {
        if (lane_mod.current_lane) |l| {
            if (l.driven) return self.homeLane(l.home_cpu, l.node);
            if (self.same_lane_dispatch or l.class != .fuse) return l;
            if (self.fuse_placement.pickRotation()) |idx| return self.lanes[idx];
            return l;
        }
        return self.pickMeta();
    }

    fn pickMeta(self: *SqzIo) ?*Lane {
        if (self.meta_count == 0) {
            if (self.fuse_placement.pickRotation()) |idx| return self.lanes[idx];
            return null;
        }
        // Walk the meta lanes from the cursor, skipping dead ones.
        const first = self.meta_next.fetchAdd(1, .monotonic) % self.meta_count;
        var attempt: u16 = 0;
        while (attempt < self.meta_count) : (attempt += 1) {
            const l = self.lanes[self.meta_start + (first + attempt) % self.meta_count];
            if (l.alive.load(.acquire)) {
                if (attempt > 0) placement.dead_lane_redispatches.add(attempt);
                return l;
            }
        }
        return null;
    }

    /// Every lane of the class is dead: the daemon can never answer again.
    fn abortAllDead(what: []const u8) noreturn {
        log.err("exec: every {s} lane is dead — a daemon that can never again answer a request must fail loud, not wedge every mount user (charter §1.8)", .{what});
        std.process.abort();
    }

    fn ReturnOf(comptime function: anytype) type {
        return @typeInfo(@TypeOf(function)).@"fn".return_type.?;
    }

    fn TypeErasedStart(comptime function: anytype) type {
        const Args = std.meta.ArgsTuple(@TypeOf(function));
        const R = ReturnOf(function);
        return struct {
            fn start(context: *const anyopaque, result: *anyopaque) void {
                const args: *const Args = @ptrCast(@alignCast(context));
                const r: *R = @ptrCast(@alignCast(result));
                r.* = @call(.auto, function, args.*);
            }
        };
    }

    fn spawnTyped(lane: *Lane, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function)), protection: fiber_mod.Protection) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        const R = ReturnOf(function);
        comptime assert(@sizeOf(R) <= fiber_mod.max_result_size);
        comptime assert(@sizeOf(@TypeOf(args)) <= fiber_mod.max_context_size);
        comptime assert(@alignOf(R) <= fiber_mod.max_result_align.toByteUnits());
        comptime assert(@alignOf(@TypeOf(args)) <= fiber_mod.max_context_align.toByteUnits());
        const f = try lane.spawn(.future, .{ .future = TypeErasedStart(function).start }, std.mem.asBytes(&args), null, protection);
        return .{ .any_future = @ptrCast(f), .result = undefined };
    }

    /// Spawn onto a specific lane (the transport's own lane, a journal lane).
    pub fn spawnOn(self: *SqzIo, lane: *Lane, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        _ = self;
        return spawnTyped(lane, function, args, .unblocked);
    }

    /// `spawn_boxed_home`: the fuse lane homed on `cpu`, else the node's
    /// round-robin, else the fuse rotation; dead lanes are walked around.
    pub fn spawnHome(self: *SqzIo, cpu: ?u16, node: ?u16, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        const idx = self.fuse_placement.pickHome(cpu, node) orelse {
            if (self.fuse_count == 0) return spawnTyped(self.pickMeta() orelse abortAllDead("meta"), function, args, .unblocked);
            abortAllDead("fuse");
        };
        return spawnTyped(self.lanes[idx], function, args, .unblocked);
    }

    /// The lane `spawnHome` would pick (the transport's handoff target
    /// resolved once per delivery): the fuse lane homed on `cpu`, else the
    /// node's round-robin, else the rotation; the meta pool when no fuse
    /// lanes exist; dead lanes walked around, every lane dead aborts loud.
    pub fn homeLane(self: *SqzIo, cpu: ?u16, node: ?u16) *Lane {
        const idx = self.fuse_placement.pickHome(cpu, node) orelse {
            if (self.fuse_count == 0) return self.pickMeta() orelse abortAllDead("meta");
            abortAllDead("fuse");
        };
        return self.lanes[idx];
    }

    /// A GROUP member on a chosen lane: the transport's fire-and-forget
    /// handler fiber (the lane recycles it at finish; the session's
    /// `Io.Group` is the join at teardown). Allocates nothing.
    pub fn spawnGroupOn(self: *SqzIo, lane: *Lane, g: *Io.Group, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!void {
        _ = self;
        const Args = @TypeOf(args);
        comptime assert(@sizeOf(Args) <= fiber_mod.max_context_size);
        comptime assert(@alignOf(Args) <= fiber_mod.max_context_align.toByteUnits());
        const TypeErased = struct {
            fn start(context: *const anyopaque) void {
                const a: *const Args = @ptrCast(@alignCast(context));
                @call(.auto, function, a.*);
            }
        };
        group_mod.addMember(g);
        _ = lane.spawn(.group, .{ .group = TypeErased.start }, std.mem.asBytes(&args), g, .unblocked) catch |err| {
            group_mod.removeMember(g);
            return err;
        };
    }

    /// The `sqz-meta` pool, round-robin.
    pub fn spawnMeta(self: *SqzIo, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        return spawnTyped(self.pickMeta() orelse abortAllDead("meta"), function, args, .unblocked);
    }

    /// A GROUP member on the `sqz-meta` pool, round-robin (the conveyor's
    /// applier fibers, decision 0003 §AW): fire-and-forget, joined through
    /// the group at teardown.
    pub fn spawnMetaGroup(self: *SqzIo, g: *Io.Group, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!void {
        return self.spawnGroupOn(self.pickMeta() orelse return error.ConcurrencyUnavailable, g, function, args);
    }

    /// The pool's width (the applier venue's parallelism).
    pub fn metaLaneCount(self: *const SqzIo) u32 {
        return self.meta_count;
    }

    /// The lease lane (the heartbeat venue).
    pub fn spawnLease(self: *SqzIo, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        const idx = self.lease_index orelse return error.ConcurrencyUnavailable;
        return spawnTyped(self.lanes[idx], function, args, .unblocked);
    }

    /// A committer: starts with cancel protection BLOCKED (charter §9:
    /// cancellation of a committer is designed out).
    pub fn spawnUncancelable(self: *SqzIo, lane: *Lane, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) Io.ConcurrentError!Io.Future(ReturnOf(function)) {
        _ = self;
        return spawnTyped(lane, function, args, .blocked);
    }

    /// A blocking job from a fiber (the fiber parks) or inline off a lane.
    pub fn runBlocking(self: *SqzIo, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) blocking.BlockingPool.SubmitError!ReturnOf(function) {
        return blocking.runBlocking(self.blocking_pool, function, args);
    }

    /// `readable(fd)` (`fdwatch.zig`).
    pub fn readable(self: *SqzIo, fd: linux.fd_t) fdwatch.Error!fdwatch.Readiness {
        _ = self;
        return fdwatch.readable(fd);
    }

    pub const FsyncRangeError = error{ InputOutput, NoSpaceLeft, DiskQuota, Unexpected };

    /// `[off, off + len)` of `fd` durable: `IORING_OP_FSYNC` with the range
    /// in `off`/`len` and `IORING_FSYNC_DATASYNC` — the kernel's
    /// `vfs_fsync_range`: the range's dirty pages written and waited for,
    /// the metadata that reaches them committed, the device's cache
    /// flushed; the same path `msync(MS_SYNC)` takes for a shared mapping
    /// of the range. On a lane fiber the SQE rides its lane's ring and the
    /// fiber parks until the CQE (the kernel runs FSYNC on io-wq, so the
    /// lane thread never blocks); off a lane (a raw thread) the raw
    /// `fdatasync(2)` of the whole file — a superset of the range. Never a
    /// cancellation point (charter §9: a durability leg is never
    /// cancelled); a cancelled SQE reports as EIO.
    pub fn fsyncRange(io_handle: Io, fd: linux.fd_t, off: u64, len: u64) FsyncRangeError!void {
        const f = lane_mod.currentFiber() orelse {
            return switch (linux.errno(linux.fdatasync(fd))) {
                .SUCCESS => {},
                .IO => error.InputOutput,
                .NOSPC => error.NoSpaceLeft,
                .DQUOT => error.DiskQuota,
                else => error.Unexpected,
            };
        };
        const old = io_handle.swapCancelProtection(.blocked);
        defer _ = io_handle.swapCancelProtection(old);
        if (!f.lane.armRingOp(f, prepFsyncRange, .{ fd, off, len })) return error.Unexpected;
        const res = awaitRingOp(f) catch |err| switch (err) {
            error.Canceled => unreachable, // protection is blocked above
        };
        if (res >= 0) return;
        return switch (@as(linux.E, @fromBackingInt(@as(u16, @intCast(-res))))) {
            .NOSPC => error.NoSpaceLeft,
            .DQUOT => error.DiskQuota,
            .IO, .CANCELED => error.InputOutput,
            else => error.Unexpected,
        };
    }

    pub const WriteAtError = error{ InputOutput, NoSpaceLeft, DiskQuota, ShortWrite, Unexpected };

    /// `buf` into `fd` at `off`, whole: `IORING_OP_WRITE` on the fiber's
    /// lane ring (the kernel's buffered write — a page's copy is one atom
    /// against the daemon's death, where a memcpy into a shared mapping is
    /// not; no page fault, no `mmap_lock`), the fiber parked until the CQE;
    /// off a lane the raw `pwrite(2)`. Never a cancellation point (a write
    /// a durability law orders is never left half-issued).
    pub fn writeAt(io_handle: Io, fd: linux.fd_t, buf: []const u8, off: u64) WriteAtError!void {
        const f = lane_mod.currentFiber() orelse {
            var done: usize = 0;
            while (done < buf.len) {
                const rc = linux.pwrite(fd, buf.ptr + done, buf.len - done, @intCast(off + done));
                switch (linux.errno(rc)) {
                    .SUCCESS => {
                        if (rc == 0) return error.ShortWrite;
                        done += @intCast(rc);
                    },
                    .INTR => {},
                    .IO => return error.InputOutput,
                    .NOSPC => return error.NoSpaceLeft,
                    .DQUOT => return error.DiskQuota,
                    else => return error.Unexpected,
                }
            }
            return;
        };
        const old = io_handle.swapCancelProtection(.blocked);
        defer _ = io_handle.swapCancelProtection(old);
        if (!f.lane.armRingOp(f, prepWriteAt, .{ fd, buf, off })) return error.Unexpected;
        const res = awaitRingOp(f) catch |err| switch (err) {
            error.Canceled => unreachable, // protection is blocked above
        };
        if (res >= 0) return if (@as(usize, @intCast(res)) == buf.len) {} else error.ShortWrite;
        return switch (@as(linux.E, @fromBackingInt(@as(u16, @intCast(-res))))) {
            .NOSPC => error.NoSpaceLeft,
            .DQUOT => error.DiskQuota,
            .IO, .CANCELED => error.InputOutput,
            else => error.Unexpected,
        };
    }

    fn prepWriteAt(sqe: *linux.io_uring_sqe, fd: linux.fd_t, buf: []const u8, off: u64) void {
        sqe.prep_write(fd, buf, off);
    }

    /// `prep_fsync` then the range: std's prep resets every field, and
    /// `io_fsync_prep` reads the range from `off`/`len` (0/0 = the whole
    /// file). The SQE's `len` is 32 bits: a span past it is the whole file.
    fn prepFsyncRange(sqe: *linux.io_uring_sqe, fd: linux.fd_t, off: u64, len: u64) void {
        sqe.prep_fsync(fd, linux.IORING_FSYNC_DATASYNC);
        if (len <= std.math.maxInt(u32)) {
            sqe.off = off;
            sqe.len = @intCast(len);
        }
    }

    /// `main`: the calling thread becomes the `main` lane and drives
    /// `function(args)` to completion; returns its result.
    pub fn run(self: *SqzIo, comptime function: anytype, args: std.meta.ArgsTuple(@TypeOf(function))) (Lane.StartError || Io.ConcurrentError)!ReturnOf(function) {
        const R = ReturnOf(function);
        const lane = self.mainLane();
        try lane.attachCurrentThread();
        defer lane.detachCurrentThread();
        var fut = try spawnTyped(lane, function, args, .unblocked);
        const f: *Fiber = @ptrCast(@alignCast(fut.any_future.?));
        lane.runUntilFinished(f);
        var result: R = undefined;
        @memcpy(std.mem.asBytes(&result), f.result[0..@sizeOf(R)]);
        lane.recycle(f);
        fut.any_future = null;
        return result;
    }

    /// Every lane's view for the stats JSON.
    pub fn writeStatsJson(self: *SqzIo, w: *Io.Writer) Io.Writer.Error!void {
        var views: [64]stats.LaneView = undefined;
        var n: usize = 0;
        for (self.lanes) |l| {
            if (n == views.len) break;
            views[n] = l.view();
            n += 1;
        }
        {
            self.lockDriven();
            defer self.unlockDriven();
            for (self.driven) |d| if (d) |l| {
                if (n == views.len) break;
                views[n] = l.view();
                n += 1;
            };
        }
        try stats.writeStatsJson(w, views[0..n]);
    }

    // ---- the vtable ----

    const base_vtable: Io.VTable = Io.Threaded.io(@as(*Io.Threaded, undefined)).vtable.*;

    pub const vtable: Io.VTable = blk: {
        var v = base_vtable;
        v.crashHandler = crashHandler;
        v.async = async;
        v.concurrent = concurrent;
        v.await = await;
        v.cancel = cancel;
        v.groupAsync = groupAsync;
        v.groupConcurrent = groupConcurrent;
        v.groupAwait = groupAwait;
        v.groupCancel = groupCancel;
        v.recancel = recancel;
        v.swapCancelProtection = swapCancelProtection;
        v.checkCancel = checkCancel;
        v.futexWait = futexWait;
        v.futexWaitUncancelable = futexWaitUncancelable;
        v.futexWake = futexWake;
        v.operate = operate;
        v.batchAwaitAsync = batchAwaitAsync;
        v.batchAwaitConcurrent = batchAwaitConcurrent;
        v.batchCancel = batchCancel;
        v.now = now;
        v.sleep = sleep;
        break :blk v;
    };

    fn crashHandler(userdata: ?*anyopaque) void {
        // A crashing fiber must not observe a cancel while the panic handler
        // runs (std's `Io.Uring` posture); then Threaded's handler.
        if (lane_mod.currentFiber()) |f| f.protection = .{ .user = .blocked, .acknowledged = true };
        base_vtable.crashHandler(userdata);
    }

    fn spawnFuture(self: *SqzIo, context: []const u8, start: *const fn (*const anyopaque, *anyopaque) void) Io.ConcurrentError!*Fiber {
        const lane = self.pickLane() orelse abortAllDead("meta");
        return lane.spawn(.future, .{ .future = start }, context, null, .unblocked);
    }

    fn async(userdata: ?*anyopaque, result: []u8, result_alignment: Alignment, context: []const u8, context_alignment: Alignment, start: *const fn (*const anyopaque, *anyopaque) void) ?*Io.AnyFuture {
        const self = ownerOf(userdata);
        if (result.len > fiber_mod.max_result_size or context.len > fiber_mod.max_context_size or result_alignment.compare(.gt, fiber_mod.max_result_align) or context_alignment.compare(.gt, fiber_mod.max_context_align)) {
            start(context.ptr, result.ptr);
            return null;
        }
        const f = self.spawnFuture(context, start) catch {
            // `async` may run inline: the result is populated, `await` is a no-op.
            start(context.ptr, result.ptr);
            return null;
        };
        return @ptrCast(f);
    }

    fn concurrent(userdata: ?*anyopaque, result_len: usize, result_alignment: Alignment, context: []const u8, context_alignment: Alignment, start: *const fn (*const anyopaque, *anyopaque) void) Io.ConcurrentError!*Io.AnyFuture {
        const self = ownerOf(userdata);
        // std's own limits (`Io.Uring.concurrent` asserts the same).
        assert(result_len <= fiber_mod.max_result_size);
        assert(context.len <= fiber_mod.max_context_size);
        assert(result_alignment.compare(.lte, fiber_mod.max_result_align));
        assert(context_alignment.compare(.lte, fiber_mod.max_context_align));
        return @ptrCast(try self.spawnFuture(context, start));
    }

    /// Park until `f` published its result: a fiber awaiter registers
    /// itself in `f.awaiter` (and the lane's `finalize` wakes it), a raw
    /// thread registers the sentinel and its word.
    fn awaitFiber(f: *Fiber) void {
        if (lane_mod.currentFiber()) |me| {
            _ = me.changeAwaiting(.nothing, .future);
            // acq_rel pairs with `finalize`'s swap: the result bytes are visible.
            const prev = f.awaiter.swap(me, .acq_rel);
            if (prev != Fiber.finished) {
                while (f.awaiter.load(.acquire) != Fiber.finished) me.park();
            }
            _ = me.changeAwaiting(.future, .nothing);
            return;
        }
        var word: futex.Word = .init(0);
        f.raw_word = &word;
        const prev = f.awaiter.swap(Fiber.raw_awaiter, .acq_rel);
        if (prev != Fiber.finished) {
            while (word.load(.acquire) == 0) futex.wait(&word, 0);
        }
    }

    fn await(userdata: ?*anyopaque, any_future: *Io.AnyFuture, result: []u8, result_alignment: Alignment) void {
        _ = userdata;
        _ = result_alignment;
        const f: *Fiber = @ptrCast(@alignCast(any_future));
        awaitFiber(f);
        @memcpy(result, f.result[0..result.len]);
        f.lane.recycle(f);
    }

    fn cancel(userdata: ?*anyopaque, any_future: *Io.AnyFuture, result: []u8, result_alignment: Alignment) void {
        const self = ownerOf(userdata);
        const f: *Fiber = @ptrCast(@alignCast(any_future));
        group_mod.requestCancel(f, null, self.lanes);
        await(userdata, any_future, result, result_alignment);
    }

    fn spawnGroupMember(self: *SqzIo, g: *Io.Group, context: []const u8, start: *const fn (*const anyopaque) void) Io.ConcurrentError!void {
        const lane = self.pickLane() orelse abortAllDead("meta");
        group_mod.addMember(g);
        _ = lane.spawn(.group, .{ .group = start }, context, g, .unblocked) catch |err| {
            group_mod.removeMember(g);
            return err;
        };
    }

    fn groupAsync(userdata: ?*anyopaque, g: *Io.Group, context: []const u8, context_alignment: Alignment, start: *const fn (*const anyopaque) void) void {
        const self = ownerOf(userdata);
        if (context.len > fiber_mod.max_context_size or context_alignment.compare(.gt, fiber_mod.max_context_align)) return start(context.ptr);
        self.spawnGroupMember(g, context, start) catch start(context.ptr);
    }

    fn groupConcurrent(userdata: ?*anyopaque, g: *Io.Group, context: []const u8, context_alignment: Alignment, start: *const fn (*const anyopaque) void) Io.ConcurrentError!void {
        const self = ownerOf(userdata);
        assert(context.len <= fiber_mod.max_context_size);
        assert(context_alignment.compare(.lte, fiber_mod.max_context_align));
        return self.spawnGroupMember(g, context, start);
    }

    fn groupAwait(userdata: ?*anyopaque, g: *Io.Group, token: *anyopaque) Io.Cancelable!void {
        _ = token;
        const self = ownerOf(userdata);
        return group_mod.awaitGroup(g, lane_mod.currentFiber(), self.lanes);
    }

    fn groupCancel(userdata: ?*anyopaque, g: *Io.Group, token: *anyopaque) void {
        _ = token;
        const self = ownerOf(userdata);
        group_mod.cancelGroup(g, lane_mod.currentFiber(), self.lanes);
    }

    fn recancel(userdata: ?*anyopaque) void {
        _ = userdata;
        // Off a lane there is no cancellation to re-arm (Threaded's posture
        // for a foreign thread).
        const f = lane_mod.currentFiber() orelse return;
        assert(f.protection.acknowledged); // called `recancel` but was not canceled
        f.protection.acknowledged = false;
    }

    fn swapCancelProtection(userdata: ?*anyopaque, new: Io.CancelProtection) Io.CancelProtection {
        _ = userdata;
        const f = lane_mod.currentFiber() orelse return .unblocked;
        const old = f.protection.user;
        f.protection.user = new;
        return old;
    }

    fn checkCancel(userdata: ?*anyopaque) Io.Cancelable!void {
        _ = userdata;
        const f = lane_mod.currentFiber() orelse return;
        return f.checkCancel();
    }

    // ---- clocks and timeouts ----

    fn clockId(c: Io.Clock) posix.clockid_t {
        return switch (c) {
            .real => .REALTIME,
            .awake => .MONOTONIC,
            .boot => .BOOTTIME,
            .cpu_process => .PROCESS_CPUTIME_ID,
            .cpu_thread => .THREAD_CPUTIME_ID,
        };
    }

    fn nowNs(c: Io.Clock) i96 {
        var ts: posix.timespec = undefined;
        if (posix.errno(posix.system.clock_gettime(clockId(c), &ts)) != .SUCCESS) return 0;
        return @as(i96, ts.sec) * std.time.ns_per_s + ts.nsec;
    }

    fn now(userdata: ?*anyopaque, c: Io.Clock) Io.Timestamp {
        _ = userdata;
        return .{ .nanoseconds = nowNs(c) };
    }

    /// A timeout as a monotonic (`clock.monotonicNs`) deadline; null = never.
    /// Deadlines on other clocks are converted by their distance from now.
    fn monotonicDeadline(timeout: Io.Timeout) ?u64 {
        const now_mono = clock.monotonicNs();
        switch (timeout) {
            .none => return null,
            .duration => |d| {
                const ns = d.raw.nanoseconds;
                if (ns <= 0) return now_mono;
                return now_mono +| std.math.lossyCast(u64, ns);
            },
            .deadline => |ts| {
                if (ts.clock == .awake) {
                    if (ts.raw.nanoseconds <= 0) return 0;
                    return std.math.lossyCast(u64, ts.raw.nanoseconds);
                }
                const delta = ts.raw.nanoseconds - nowNs(ts.clock);
                if (delta <= 0) return now_mono;
                return now_mono +| std.math.lossyCast(u64, delta);
            },
        }
    }

    fn relativeNs(deadline: ?u64) ?u64 {
        const d = deadline orelse return null;
        return d -| clock.monotonicNs();
    }

    // ---- futex ----

    fn futexWait(userdata: ?*anyopaque, ptr: *const u32, expected: u32, timeout: Io.Timeout) Io.Cancelable!void {
        const self = ownerOf(userdata);
        const deadline = monotonicDeadline(timeout);
        if (lane_mod.currentFiber()) |f| return self.futexes.fiberWait(f, ptr, expected, deadline, true);
        // A timed raw park is a primitive's queued wait (`sync/tick.zig`);
        // on the thread that drives a lane it is a wait its own fibers
        // cannot end (`stats.driven_owner_queued_parks`).
        if (deadline != null) if (lane_mod.current_lane) |l| if (l.driven) stats.driven_owner_queued_parks.inc();
        self.futexes.rawWait(ptr, expected, relativeNs(deadline));
    }

    fn futexWaitUncancelable(userdata: ?*anyopaque, ptr: *const u32, expected: u32) void {
        const self = ownerOf(userdata);
        if (lane_mod.currentFiber()) |f| {
            self.futexes.fiberWait(f, ptr, expected, null, false) catch unreachable;
            return;
        }
        self.futexes.rawWait(ptr, expected, null);
    }

    fn futexWake(userdata: ?*anyopaque, ptr: *const u32, max_waiters: u32) void {
        ownerOf(userdata).futexes.wake(ptr, max_waiters);
    }

    // ---- sleep ----

    fn sleep(userdata: ?*anyopaque, timeout: Io.Timeout) Io.Cancelable!void {
        const self = ownerOf(userdata);
        const deadline = monotonicDeadline(timeout);
        const f = lane_mod.currentFiber() orelse {
            const d = deadline orelse {
                // `.none` waits forever by contract; nothing wakes a raw thread.
                var never: futex.Word = .init(0);
                while (true) futex.wait(&never, 0);
            };
            self.timer.sleepUntil(d);
            return;
        };
        try f.checkCancel();
        if (deadline) |d| if (d <= clock.monotonicNs()) return;
        if (f.changeAwaiting(.nothing, .wakeable) and f.protection.check() == .unblocked) {
            _ = f.changeAwaiting(.wakeable, .nothing);
            f.protection.acknowledged = true;
            return error.Canceled;
        }
        if (deadline) |d| {
            f.lane.timers.arm(f, d);
            stats.timer_arms.inc();
        }
        while (true) {
            f.park();
            // Fired: the heap popped our slot. Cancelled: the bit is set.
            if (deadline != null and f.timer_slot == timer_heap.no_slot) break;
            if (f.cancelWord().requested and f.protection.check() == .unblocked) break;
        }
        if (deadline != null) _ = f.lane.timers.cancel(f);
        const requested = f.changeAwaiting(.wakeable, .nothing);
        if (requested and f.protection.check() == .unblocked) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
    }

    // ---- operate ----

    fn operate(userdata: ?*anyopaque, operation: Io.Operation) Io.Cancelable!Io.Operation.Result {
        const f = lane_mod.currentFiber() orelse return base_vtable.operate(userdata, operation);
        switch (operation) {
            .file_read_streaming => |o| return .{ .file_read_streaming = try fiberReadv(f, o) },
            .file_write_streaming => |o| return .{ .file_write_streaming = try fiberWritev(f, o) },
            else => {
                // Inline on the lane thread: acceptable on main/meta, a
                // tripwire on a handler lane (it blocks every CQE behind it).
                if (f.lane.class == .fuse or f.lane.class == .fuse_ur) stats.blocking_op_on_fuse_lane.fire(@tagName(operation));
                return base_vtable.operate(userdata, operation);
            },
        }
    }

    /// The streaming iovec ceiling: the 16 on-frame iovecs cover every std
    /// writer's buffered + direct vectors; more take a short transfer the
    /// stream layer continues.
    const max_iovecs = 16;
    /// `-1` as the offset = the fd's current position (streaming).
    const streaming_offset: u64 = std.math.maxInt(u64);

    /// Arm the prepared SQE, park until its CQE, return `res` (`-errno`
    /// negative) or `error.Canceled` when a cancel request interrupted it.
    fn awaitRingOp(f: *Fiber) Io.Cancelable!i32 {
        _ = f.changeAwaiting(.nothing, .ring_op);
        while (f.op_pending) f.park();
        const requested = f.changeAwaiting(.ring_op, .nothing);
        if (f.cqe_res == -@as(i32, @intCast(@backingInt(linux.E.CANCELED))) and requested and f.protection.check() == .unblocked) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
        return f.cqe_res;
    }

    fn fiberReadv(f: *Fiber, o: Io.Operation.FileReadStreaming) Io.Cancelable!Io.Operation.FileReadStreaming.Result {
        const Result = Io.Operation.FileReadStreaming.Result;
        var iovs: [max_iovecs]posix.iovec = undefined;
        var n: usize = 0;
        for (o.data) |buf| {
            if (n == iovs.len) break;
            if (buf.len == 0) continue;
            iovs[n] = .{ .base = buf.ptr, .len = buf.len };
            n += 1;
        }
        if (n == 0) return @as(Result, 0);
        try f.checkCancel();
        if (!f.lane.armRingOp(f, linux.io_uring_sqe.prep_readv, .{ o.file.handle, iovs[0..n], streaming_offset })) return @as(Result, error.Unexpected);
        const res = try awaitRingOp(f);
        if (res > 0) return @as(Result, @intCast(res));
        if (res == 0) return @as(Result, error.EndOfStream);
        const inner: Result = switch (@as(linux.E, @fromBackingInt(@as(u16, @intCast(-res))))) {
            .BADF => error.NotOpenForReading,
            .AGAIN => error.WouldBlock,
            .IO => error.InputOutput,
            .ISDIR => error.IsDir,
            .NOBUFS, .NOMEM => error.SystemResources,
            .NOTCONN => error.SocketUnconnected,
            .CONNRESET => error.ConnectionResetByPeer,
            else => error.Unexpected,
        };
        return inner;
    }

    fn fiberWritev(f: *Fiber, o: Io.Operation.FileWriteStreaming) Io.Cancelable!Io.Operation.FileWriteStreaming.Result {
        const Result = Io.Operation.FileWriteStreaming.Result;
        var iovs: [max_iovecs]posix.iovec_const = undefined;
        var n: usize = 0;
        if (o.header.len > 0) {
            iovs[n] = .{ .base = o.header.ptr, .len = o.header.len };
            n += 1;
        }
        if (o.data.len > 0) {
            for (o.data[0 .. o.data.len - 1]) |bytes| {
                if (n == iovs.len or bytes.len == 0) continue;
                iovs[n] = .{ .base = bytes.ptr, .len = bytes.len };
                n += 1;
            }
            const pattern = o.data[o.data.len - 1];
            var k: usize = 0;
            while (k < o.splat and n < iovs.len and pattern.len > 0) : (k += 1) {
                iovs[n] = .{ .base = pattern.ptr, .len = pattern.len };
                n += 1;
            }
        }
        if (n == 0) return @as(Result, 0);
        try f.checkCancel();
        if (!f.lane.armRingOp(f, linux.io_uring_sqe.prep_writev, .{ o.file.handle, iovs[0..n], streaming_offset })) return @as(Result, error.Unexpected);
        const res = try awaitRingOp(f);
        if (res >= 0) return @as(Result, @intCast(res));
        const inner: Result = switch (@as(linux.E, @fromBackingInt(@as(u16, @intCast(-res))))) {
            .AGAIN => error.WouldBlock,
            .BADF => error.NotOpenForWriting,
            .DQUOT => error.DiskQuota,
            .FBIG => error.FileTooBig,
            .IO => error.InputOutput,
            .NOSPC => error.NoSpaceLeft,
            .PERM => error.PermissionDenied,
            .PIPE => error.BrokenPipe,
            .BUSY => error.DeviceBusy,
            .ACCES => error.AccessDenied,
            else => error.Unexpected,
        };
        return inner;
    }

    // ---- batch (decision 0003 §D Q-D: unsupported on a lane in v1) ----

    fn batchAwaitAsync(userdata: ?*anyopaque, b: *Io.Batch) Io.Cancelable!void {
        if (lane_mod.currentFiber() != null) {
            // The error set has no refusal: the only loud outcome is the
            // tripwire plus an abort (a programming error, 0002 row 9).
            stats.batch_on_lane.fire("Io.Batch.awaitAsync on a lane fiber");
            @panic("Io.Batch on a lane fiber is unsupported (decision 0003 Q-D): run it off a lane or through the ring directly");
        }
        return base_vtable.batchAwaitAsync(userdata, b);
    }

    fn batchAwaitConcurrent(userdata: ?*anyopaque, b: *Io.Batch, timeout: Io.Timeout) Io.Batch.AwaitConcurrentError!void {
        if (lane_mod.currentFiber() != null) {
            stats.batch_on_lane.fire("Io.Batch.awaitConcurrent on a lane fiber");
            return error.ConcurrencyUnavailable;
        }
        return base_vtable.batchAwaitConcurrent(userdata, b, timeout);
    }

    fn batchCancel(userdata: ?*anyopaque, b: *Io.Batch) void {
        if (lane_mod.currentFiber() != null) {
            // Nothing was ever submitted on a lane: the batch is already quiescent.
            assert(b.pending.head == .none);
            b.userdata = null;
            return;
        }
        base_vtable.batchCancel(userdata, b);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Every slot back in its pool (a group member's slot is recycled by its lane
/// right after the join it just completed, so this is a wait, not a read).
fn waitQuiescent(sqz: *SqzIo) void {
    for (sqz.lanes) |l| while (l.live.load(.monotonic) != 0) std.Thread.yield() catch {};
}

test "capacity derivations: the fuse formula from the geometry, the budget eighth, the meta default" {
    try testing.expectEqual(@as(u32, 32 * 2 + spawn_slack), fuseCapacity(.{ .nqueues = 8, .q_depth = 32 }, 4));
    try testing.expectEqual(@as(u32, 4 * 1 + spawn_slack), fuseCapacity(.{ .nqueues = 4, .q_depth = 4 }, 4));
    // 1 GiB / 8 / 1 MiB = 128 < 256 requested.
    try testing.expectEqual(@as(u32, 128), capacityFromBudget(256, derive.GiB, derive.MiB));
    try testing.expectEqual(@as(u32, 64), capacityFromBudget(64, derive.GiB, derive.MiB));
    // A tiny budget keeps the floor.
    try testing.expectEqual(@as(u32, 16), capacityFromBudget(256, derive.MiB, derive.MiB));
    try testing.expectEqual(@as(u32, 256), meta_capacity_default);
}

test "the stack reserve of the venue's population is the per-lane in-flight ceilings summed: 9 queues × depth 32 on 9 lanes = 9 × (32 + slack) fuse slots, 2 × 256 meta, 16 main (+16 lease); the reserve is virtual, the gauge counts what the kernel may hold" {
    const page: usize = 4096;
    const fuse_lanes: u32 = 9;
    const fuse_slots = fuse_lanes * fuseCapacity(.{ .nqueues = 9, .q_depth = 32 }, fuse_lanes);
    try testing.expectEqual(@as(u32, 9 * 48), fuse_slots);
    const meta_slots = placement.metaLanesFrom(9) * meta_capacity_default;
    try testing.expectEqual(@as(u32, 512), meta_slots);
    const handler_stack = fiber_mod.stackSizeFor(.fuse, page) + page;
    const big_stack = fiber_mod.stackSizeFor(.main, page) + page;
    // The whole-population reserve the venue's 752 MB gauge was converging
    // to (every slot touched once): (432 + 512 + 16) handler-class stacks
    // + 16 main-class = 1,077,739,520 B ≈ 1.0 GiB of VIRTUAL reserve; the
    // kernel holds only touched pages (the measured deepest frame under
    // fsstress: 205 KiB), and the idle trim returns those.
    const reserve = (fuse_slots + meta_slots + lease_capacity_default) * handler_stack + main_capacity_default * big_stack;
    try testing.expectEqual(@as(usize, 1_077_739_520), reserve);
    // Capacity never exceeds the budget's eighth: a 4 GiB budget holds the
    // full meta population at 1 MiB stacks, a 1 GiB budget halves it.
    try testing.expectEqual(meta_capacity_default, capacityFromBudget(meta_capacity_default, 4 * derive.GiB, fiber_mod.stackSizeFor(.meta, page)));
    try testing.expectEqual(@as(u32, 128), capacityFromBudget(meta_capacity_default, derive.GiB, fiber_mod.stackSizeFor(.meta, page)));
}

test "testInstance: 2 meta lanes, a main lane, a running timer thread and pool; the vtable delegates Threaded's file ops" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    try testing.expectEqual(@as(usize, 2), sqz.metaLanes().len);
    try testing.expectEqual(@as(usize, 0), sqz.fuseLanes().len);
    try testing.expectEqual(LaneClass.main, sqz.mainLane().class);
    const io = sqz.io();
    // A delegated op: Threaded's `random` and a file read through `Io.Dir`.
    var buf: [16]u8 = undefined;
    io.vtable.random(io.userdata, &buf);
    var small: [64]u8 = undefined;
    const text = try Io.Dir.cwd().readFile(io, "/proc/self/comm", &small);
    try testing.expect(text.len > 0);
    // `now` is ours and monotone.
    const a = io.vtable.now(io.userdata, .awake);
    const b = io.vtable.now(io.userdata, .awake);
    try testing.expect(b.nanoseconds >= a.nanoseconds);
    try testing.expect(io.vtable.now(io.userdata, .real).nanoseconds > 1_577_836_800 * std.time.ns_per_s);
}

test "concurrent + await from the test thread: a fiber on a meta lane runs and its result lands; async may run inline past capacity" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn add(a: u32, b: u32) u32 {
            return a + b;
        }
    };
    var fut = try io.concurrent(S.add, .{ 3, 4 });
    try testing.expectEqual(@as(u32, 7), fut.await(io));
    var fut2 = io.async(S.add, .{ 10, 20 });
    try testing.expectEqual(@as(u32, 30), fut2.await(io));
    // Every fiber recycled (the awaiter recycles a future's slot itself).
    for (sqz.lanes) |l| try testing.expectEqual(@as(u32, 0), l.live.load(.monotonic));
}

test "a fiber awaiting a fiber: the awaiter parks on its lane and resumes with the result; nested concurrency across both meta lanes" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn leaf(x: u32) u32 {
            return x * 2;
        }
        fn parent(io_: Io, x: u32) u32 {
            var a = io_.concurrent(leaf, .{x}) catch return 0;
            var b = io_.concurrent(leaf, .{x + 1}) catch return 0;
            return a.await(io_) + b.await(io_);
        }
    };
    var fut = try io.concurrent(S.parent, .{ io, 5 });
    try testing.expectEqual(@as(u32, 22), fut.await(io));
}

test "an anonymous concurrent spawned from a fiber on a DRIVEN lane lands on a threaded lane, never the owner's (decision 0003 §AB.3: the drain worker never waits on a device); the spawner awaits it across the lanes" {
    var budget: mem_budget.MemBudget = .{};
    const sqz = try SqzIo.init(testing.allocator, .{
        .budget_bytes = derive.GiB,
        .budget = &budget,
        .fuse = .{ .nqueues = 2, .q_depth = 4 },
        .meta_lanes = 1,
        .blocking_cap = 4,
    });
    defer sqz.deinit();
    const page = std.heap.pageSize();
    const driven = try Lane.init(testing.allocator, .{ .index = 0, .class = .fuse_ur, .fiber_capacity = 8, .stack_size = fiber_mod.stackSizeFor(.fuse_ur, page), .page = page, .driven = true });
    defer driven.deinit();
    try testing.expect(sqz.adoptDriven(driven));
    defer sqz.releaseDriven(driven);
    const Owner = struct {
        sqz: *SqzIo,
        lane: *Lane,
        group: Io.Group = .init,
        /// Whether the anonymous task's lane was the driven one (2 = unset).
        task_driven: std.atomic.Value(u8) = .init(2),
        result: anyerror!void = {},

        fn task() bool {
            return lane_mod.current_lane.?.driven;
        }
        fn spawner(self: *@This()) void {
            const io_ = self.sqz.io();
            var fut = io_.concurrent(task, .{}) catch return;
            self.task_driven.store(@intFromBool(fut.await(io_)), .release);
        }
        fn main(self: *@This()) void {
            self.result = self.body();
        }
        fn body(self: *@This()) !void {
            try self.lane.attachDriven();
            defer self.lane.detachDriven();
            try self.sqz.spawnGroupOn(self.lane, &self.group, spawner, .{self});
            // The owner's pump stands in for the worker's pass: the task's
            // completion reaches the spawner through the lane's inbox.
            while (self.lane.liveFibers() != 0) {
                self.lane.pumpDriven();
                try std.Thread.yield();
            }
        }
    };
    var o: Owner = .{ .sqz = sqz, .lane = driven };
    const t = try std.Thread.spawn(.{}, Owner.main, .{&o});
    t.join();
    try o.result;
    try testing.expectEqual(@as(u8, 0), o.task_driven.load(.acquire));
}

test "a count-one mutex wake reaches a DRIVEN lane's owner parked raw behind one of its own fibers: the owner wakes, its pass runs the fiber, both acquire (the mdtest wedge: the fiber took the one wake and the owner slept on a FREE mutex for ever)" {
    var budget: mem_budget.MemBudget = .{};
    const sqz = try SqzIo.init(testing.allocator, .{
        .budget_bytes = derive.GiB,
        .budget = &budget,
        .fuse = .{ .nqueues = 2, .q_depth = 4 },
        .meta_lanes = 1,
        .blocking_cap = 4,
    });
    defer sqz.deinit();
    const page = std.heap.pageSize();
    const driven = try Lane.init(testing.allocator, .{ .index = 0, .class = .fuse_ur, .fiber_capacity = 8, .stack_size = fiber_mod.stackSizeFor(.fuse_ur, page), .page = page, .driven = true });
    defer driven.deinit();
    try testing.expect(sqz.adoptDriven(driven));
    defer sqz.releaseDriven(driven);
    const Rig = struct {
        sqz: *SqzIo,
        lane: *Lane,
        group: Io.Group = .init,
        m: Io.Mutex = .init,
        held: std.atomic.Value(bool) = .init(false),
        owner_tid: std.atomic.Value(i32) = .init(0),
        fiber_acquired: std.atomic.Value(u32) = .init(0),
        owner_acquired: std.atomic.Value(bool) = .init(false),
        /// The holder had to wake the owner by hand: the wedge.
        rescued: std.atomic.Value(bool) = .init(false),
        result: anyerror!void = {},

        fn fiber(self: *@This()) void {
            const io_ = self.sqz.io();
            self.m.lockUncancelable(io_);
            _ = self.fiber_acquired.fetchAdd(1, .acq_rel);
            self.m.unlock(io_);
        }
        /// Whether the owner thread sleeps in the kernel (its `stat` state
        /// is S): a counted raw waiter whose compare has not run yet would
        /// see the release itself — the wedge needs it asleep.
        fn ownerAsleep(self: *@This(), io_: Io) bool {
            var pb: [64]u8 = undefined;
            var buf: [512]u8 = undefined;
            const path = std.fmt.bufPrint(&pb, "/proc/self/task/{d}/stat", .{self.owner_tid.load(.acquire)}) catch return false;
            const text = Io.Dir.cwd().readFile(io_, path, &buf) catch return false;
            const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return false;
            return close + 2 < text.len and text[close + 2] == 'S';
        }
        /// The holder: a third thread that releases once the owner is a
        /// counted raw waiter asleep behind its parked fiber.
        fn holder(self: *@This()) void {
            const io_ = self.sqz.io();
            const word: *const futex.Word = @ptrCast(&self.m.state);
            self.m.lockUncancelable(io_);
            self.held.store(true, .release);
            while (self.sqz.futexes.rawWaiters(&word.raw) == 0 or !self.ownerAsleep(io_)) std.Thread.yield() catch {};
            self.m.unlock(io_);
            // The owner must wake on that release alone. A bound turns a
            // wedge into a failure instead of a hung shard: the rescue
            // wakes the owner by hand and is the red verdict.
            const deadline = clock.monotonicNs() + 2 * std.time.ns_per_s;
            while (!self.owner_acquired.load(.acquire)) {
                if (clock.monotonicNs() >= deadline) {
                    self.rescued.store(true, .release);
                    futex.wake(word, 1);
                    return;
                }
                std.Thread.yield() catch {};
            }
        }
        fn main(self: *@This()) void {
            self.result = self.body();
        }
        fn body(self: *@This()) !void {
            const io_ = self.sqz.io();
            try self.lane.attachDriven();
            defer self.lane.detachDriven();
            while (!self.held.load(.acquire)) try std.Thread.yield();
            try self.sqz.spawnGroupOn(self.lane, &self.group, fiber, .{self});
            // The fiber runs here, meets the held mutex and parks on it.
            self.lane.pumpDriven();
            if (self.fiber_acquired.load(.acquire) != 0) return error.FiberNotParked;
            // The owner's own contended acquire, off a fiber: the raw wait.
            self.owner_tid.store(linux.gettid(), .release);
            self.m.lockUncancelable(io_);
            self.owner_acquired.store(true, .release);
            self.m.unlock(io_);
            while (self.lane.liveFibers() != 0) {
                self.lane.pumpDriven();
                try std.Thread.yield();
            }
        }
    };
    var r: Rig = .{ .sqz = sqz, .lane = driven };
    const h = try std.Thread.spawn(.{}, Rig.holder, .{&r});
    const t = try std.Thread.spawn(.{}, Rig.main, .{&r});
    t.join();
    h.join();
    try r.result;
    try testing.expect(!r.rescued.load(.acquire));
    try testing.expectEqual(@as(u32, 1), r.fiber_acquired.load(.acquire));
}

test "Io.Mutex contended across two lanes and the test thread: every increment lands" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const Shared = struct {
        m: Io.Mutex = .init,
        n: u64 = 0,
        fn worker(io_: Io, s: *@This()) void {
            var i: usize = 0;
            while (i < 500) : (i += 1) {
                s.m.lockUncancelable(io_);
                s.n += 1;
                s.m.unlock(io_);
                if (i % 50 == 0) io_.sleep(.fromNanoseconds(10_000), .awake) catch {};
            }
        }
    };
    var s: Shared = .{};
    var futs: [6]Io.Future(void) = undefined;
    for (&futs) |*f| f.* = try io.concurrent(Shared.worker, .{ io, &s });
    // The raw thread contends too.
    Shared.worker(io, &s);
    for (&futs) |*f| f.await(io);
    try testing.expectEqual(@as(u64, 3500), s.n);
}

test "sleep: a fiber sleeps at least the requested time on its lane timer; a raw thread sleeps through sqz-timer; a passed deadline returns at once" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn nap(io_: Io, ns: u64) u64 {
            const t0 = clock.monotonicNs();
            io_.sleep(.fromNanoseconds(@intCast(ns)), .awake) catch return 0;
            return clock.monotonicNs() - t0;
        }
    };
    const arms_before = stats.timer_arms.load();
    var fut = try io.concurrent(S.nap, .{ io, 5 * std.time.ns_per_ms });
    try testing.expect(fut.await(io) >= 5 * std.time.ns_per_ms);
    try testing.expectEqual(arms_before + 1, stats.timer_arms.load());
    try testing.expect(S.nap(io, 3 * std.time.ns_per_ms) >= 3 * std.time.ns_per_ms);
    // A deadline in the past, on the real clock too.
    try io.vtable.sleep(io.userdata, .{ .deadline = .{ .raw = .{ .nanoseconds = 1 }, .clock = .awake } });
    try io.vtable.sleep(io.userdata, .{ .deadline = .{ .raw = .{ .nanoseconds = 1 }, .clock = .real } });
}

test "cancel: a parked fiber returns error.Canceled exactly once at its next point; recancel re-arms; blocked protection survives a cancel; a running fiber observes it at checkCancel" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const Probe = struct {
        parked: std.atomic.Value(u32) = .init(0),
        fn sleeper(io_: Io, p: *@This()) Io.Cancelable!u32 {
            _ = p.parked.fetchAdd(1, .release);
            io_.sleep(.fromNanoseconds(std.time.ns_per_hour), .awake) catch |err| switch (err) {
                error.Canceled => {
                    // Spent: the next point does not re-signal …
                    io_.sleep(.fromNanoseconds(1000), .awake) catch return 1;
                    // … until recancel re-arms it.
                    io_.recancel();
                    io_.sleep(.fromNanoseconds(1000), .awake) catch return 2;
                    return 3;
                },
            };
            return 0;
        }
        fn shielded(io_: Io, p: *@This()) u32 {
            const old = io_.swapCancelProtection(.blocked);
            _ = p.parked.fetchAdd(1, .release);
            io_.sleep(.fromNanoseconds(20 * std.time.ns_per_ms), .awake) catch return 99;
            _ = io_.swapCancelProtection(old);
            // Unblocked now: the pending request fires here, once.
            io_.checkCancel() catch return 7;
            return 8;
        }
    };
    var p: Probe = .{};
    var fut = try io.concurrent(Probe.sleeper, .{ io, &p });
    while (p.parked.load(.acquire) < 1) try std.Thread.yield();
    try testing.expectEqual(@as(Io.Cancelable!u32, 2), fut.cancel(io));

    var fut2 = try io.concurrent(Probe.shielded, .{ io, &p });
    while (p.parked.load(.acquire) < 2) try std.Thread.yield();
    try testing.expectEqual(@as(u32, 7), fut2.cancel(io));
    // Cancelling a finished future is the await.
    var fut3 = try io.concurrent(Probe.shielded, .{ io, &p });
    _ = fut3.await(io);
    try testing.expectEqual(@as(u32, 8), fut3.cancel(io));
}

test "Io.Group over fibers: await joins members; cancel wakes parked members and a member spawned into a cancelled group never runs its body" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const Probe = struct {
        ran: std.atomic.Value(u32) = .init(0),
        parked: std.atomic.Value(u32) = .init(0),
        resumed: std.atomic.Value(u32) = .init(0),
        fn quick(p: *@This()) Io.Cancelable!void {
            _ = p.ran.fetchAdd(1, .acq_rel);
        }
        fn parkForever(io_: Io, p: *@This()) Io.Cancelable!void {
            try io_.checkCancel();
            _ = p.ran.fetchAdd(1, .acq_rel);
            _ = p.parked.fetchAdd(1, .acq_rel);
            try io_.sleep(.fromNanoseconds(std.time.ns_per_hour), .awake);
            _ = p.resumed.fetchAdd(1, .acq_rel);
        }
    };
    var p: Probe = .{};
    var g: Io.Group = .init;
    for (0..5) |_| try g.concurrent(io, Probe.quick, .{&p});
    try g.await(io);
    try testing.expectEqual(@as(u32, 5), p.ran.load(.acquire));
    try testing.expect(g.token.load(.acquire) == null);

    var g2: Io.Group = .init;
    for (0..4) |_| try g2.concurrent(io, Probe.parkForever, .{ io, &p });
    while (p.parked.load(.acquire) < 4) try std.Thread.yield();
    g2.cancel(io);
    try testing.expectEqual(@as(u32, 0), p.resumed.load(.acquire));
    try testing.expectEqual(@as(u32, 9), p.ran.load(.acquire));
    try testing.expect(g2.token.load(.acquire) == null);
    // A member spawned after the cancel: the group is empty again (token
    // null), so it is a fresh group; `checkCancel` sees no request.
    try g2.concurrent(io, Probe.parkForever, .{ io, &p });
    while (p.parked.load(.acquire) < 5) try std.Thread.yield();
    g2.cancel(io);
    try testing.expectEqual(@as(u32, 0), p.resumed.load(.acquire));
    waitQuiescent(sqz);
}

test "operate: a READV and a WRITEV streaming op on a lane fiber ride the lane ring; off a lane they are Threaded's syscalls" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    const file = try tmp.dir.createFile(io, "ring.txt", .{ .read = true });
    defer file.close(io);
    const S = struct {
        fn roundTrip(io_: Io, f: Io.File) anyerror!usize {
            const wrote = try io_.vtable.operate(io_.userdata, .{ .file_write_streaming = .{ .file = f, .header = "hello ", .data = &.{"ring"}, .splat = 1 } });
            const n = try wrote.file_write_streaming;
            try io_.vtable.fileSeekTo(io_.userdata, f, 0);
            var buf: [32]u8 = undefined;
            var slices = [_][]u8{buf[0..32]};
            const read = try io_.vtable.operate(io_.userdata, .{ .file_read_streaming = .{ .file = f, .data = &slices } });
            const m = try read.file_read_streaming;
            if (!std.mem.eql(u8, buf[0..m], "hello ring")) return error.Mismatch;
            // At EOF the stream reports it.
            const eof = try io_.vtable.operate(io_.userdata, .{ .file_read_streaming = .{ .file = f, .data = &slices } });
            if (eof.file_read_streaming) |_| return error.ExpectedEof else |err| if (err != error.EndOfStream) return err;
            return n + m;
        }
    };
    const ops_before = stats.ring_ops.load();
    var fut = try io.concurrent(S.roundTrip, .{ io, file });
    try testing.expectEqual(@as(usize, 20), try fut.await(io));
    try testing.expectEqual(ops_before + 3, stats.ring_ops.load());
    try io.vtable.fileSeekTo(io.userdata, file, 0);
    try testing.expectEqual(@as(usize, 20), try S.roundTrip(io, file));
    try testing.expectEqual(ops_before + 3, stats.ring_ops.load());
}

test "readable(fd) on a lane fiber is a POLL_ADD on the lane ring; a second awaiter displaces the first (tripwire, .replaced)" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const efd_rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
    const efd: linux.fd_t = @intCast(efd_rc);
    defer _ = linux.close(efd);
    const S = struct {
        fn wait(s: *SqzIo, fd: linux.fd_t) fdwatch.Error!fdwatch.Readiness {
            return s.readable(fd);
        }
        /// The displacing awaiter: the tripwire logs on the LANE thread, so
        /// the log seam is armed there.
        fn displace(s: *SqzIo, fd: linux.fd_t) fdwatch.Error!fdwatch.Readiness {
            log.testing.expectErrorLogs(1);
            const r = s.readable(fd);
            log.testing.expectAllFired() catch return error.Unexpected;
            return r;
        }
        fn poke(fd: linux.fd_t) void {
            const one: u64 = 1;
            _ = linux.write(fd, @ptrCast(&one), 8);
        }
    };
    const lane = sqz.metaLanes()[0];
    const ops_before = stats.ring_ops.load();
    var a = try sqz.spawnOn(lane, S.wait, .{ sqz, efd });
    // The awaiter armed its SQE (and parked) before the second registers.
    while (stats.ring_ops.load() == ops_before) try std.Thread.yield();
    const trips_before = stats.fdwatch_double_await.load();
    stats.fdwatch_double_await.resetLogBudgetForTest();
    var b = try sqz.spawnOn(lane, S.displace, .{ sqz, efd });
    try testing.expectEqual(fdwatch.Readiness.replaced, try a.await(io));
    try testing.expectEqual(trips_before + 1, stats.fdwatch_double_await.load());
    S.poke(efd);
    try testing.expectEqual(fdwatch.Readiness.ready, try b.await(io));
}

test "Batch on a lane fails loud: awaitConcurrent is ConcurrencyUnavailable and trips; off a lane it is Threaded's" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn onLane(io_: Io) Io.Batch.AwaitConcurrentError!void {
            var storage: [1]Io.Operation.Storage = undefined;
            var batch = Io.Batch.init(&storage);
            defer batch.cancel(io_);
            // The tripwire logs on the lane thread: the seam is armed here.
            log.testing.expectErrorLogs(1);
            const r = batch.awaitConcurrent(io_, .none);
            log.testing.expectAllFired() catch return error.Timeout;
            return r;
        }
    };
    const before = stats.batch_on_lane.load();
    stats.batch_on_lane.resetLogBudgetForTest();
    var fut = try io.concurrent(S.onLane, .{io});
    try testing.expectError(error.ConcurrencyUnavailable, fut.await(io));
    try testing.expectEqual(before + 1, stats.batch_on_lane.load());
}

test "runBlocking from a fiber parks it and the sqz-blk worker wakes it; the job's result returns" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn slow(x: u32) u32 {
            testing.io.sleep(.fromNanoseconds(2 * std.time.ns_per_ms), .awake) catch {};
            return x + 1;
        }
        fn task(s: *SqzIo, x: u32) u32 {
            return s.runBlocking(slow, .{x}) catch 0;
        }
    };
    var fut = try io.concurrent(S.task, .{ sqz, 41 });
    try testing.expectEqual(@as(u32, 42), fut.await(io));
    try testing.expect(sqz.blocking_pool.threads() >= 1);
}

test "fsyncRange from a fiber is ONE SQE on its lane's ring (the fiber parks, the lane thread never blocks), off a lane the raw fdatasync; a closed fd is a loud error on both" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    const file = try tmp.dir.createFile(testing.io, "seg", .{ .truncate = true });
    defer file.close(testing.io);
    try file.writeStreamingAll(testing.io, "staged bytes");
    const S = struct {
        fn task(io_: Io, fd: linux.fd_t) SqzIo.FsyncRangeError!void {
            return SqzIo.fsyncRange(io_, fd, 0, 4096);
        }
    };
    const ops0 = stats.ring_ops.load();
    var fut = try io.concurrent(S.task, .{ io, file.handle });
    try fut.await(io);
    try testing.expectEqual(ops0 + 1, stats.ring_ops.load());
    // Off a lane: the raw syscall, no ring op.
    try SqzIo.fsyncRange(io, file.handle, 0, 4096);
    try testing.expectEqual(ops0 + 1, stats.ring_ops.load());
    // A fd nobody holds.
    try testing.expectError(error.Unexpected, SqzIo.fsyncRange(io, -1, 0, 4096));
    var fut2 = try io.concurrent(S.task, .{ io, @as(linux.fd_t, -1) });
    try testing.expectError(error.Unexpected, fut2.await(io));
}

test "run: the calling thread becomes the main lane, drives the root to completion and returns its result; the root may spawn and await" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const S = struct {
        fn leaf(x: u32) u32 {
            return x + 1;
        }
        fn root(io_: Io, s: *SqzIo) u32 {
            std.debug.assert(lane_mod.current_lane.?.class == .main);
            var a = io_.concurrent(leaf, .{1}) catch return 0;
            io_.sleep(.fromNanoseconds(std.time.ns_per_ms), .awake) catch return 0;
            var b = s.spawnMeta(leaf, .{10}) catch return 0;
            return a.await(io_) + b.await(io_);
        }
    };
    try testing.expectEqual(@as(u32, 13), try sqz.run(S.root, .{ io, sqz }));
    try testing.expect(lane_mod.current_lane == null);
    waitQuiescent(sqz);
}

test "spawnUncancelable: a committer's park survives a cancel request; the request is observed only after it unblocks" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const Probe = struct {
        parked: std.atomic.Value(u32) = .init(0),
        fn committer(io_: Io, p: *@This()) u32 {
            _ = p.parked.fetchAdd(1, .release);
            io_.sleep(.fromNanoseconds(20 * std.time.ns_per_ms), .awake) catch return 1;
            _ = io_.swapCancelProtection(.unblocked);
            io_.checkCancel() catch return 2;
            return 3;
        }
    };
    var p: Probe = .{};
    var fut = try sqz.spawnUncancelable(sqz.metaLanes()[1], Probe.committer, .{ io, &p });
    while (p.parked.load(.acquire) == 0) try std.Thread.yield();
    try testing.expectEqual(@as(u32, 2), fut.cancel(io));
}

test "placement with fuse lanes on this machine: spawnHome lands on the lane homed on the CPU; a dead lane is walked around and counted; the mem_budget component is registered and tracks mapped stacks" {
    var budget: mem_budget.MemBudget = .{};
    const sqz = try SqzIo.init(testing.allocator, .{
        .budget_bytes = derive.GiB,
        .budget = &budget,
        .fuse = .{ .nqueues = 4, .q_depth = 4 },
        .meta_lanes = 1,
        .lease_lane = true,
        .blocking_cap = 8,
    });
    defer sqz.deinit();
    const io = sqz.io();
    try testing.expectEqual(@as(usize, 1), budget.components().len);
    try testing.expectEqualStrings("exec_fiber_stacks", budget.components()[0].name);
    // The lease venue: its lane and its dedicated `sqz-lease-io` worker.
    const Lease = struct {
        fn beat() LaneClass {
            return lane_mod.current_lane.?.class;
        }
        fn io_job(x: u32) u32 {
            return x * 3;
        }
        fn renew(s: *SqzIo) u32 {
            return blocking.runBlocking(s.lease_io.?, io_job, .{5}) catch 0;
        }
    };
    var beat = try sqz.spawnLease(Lease.beat, .{});
    try testing.expectEqual(LaneClass.lease, beat.await(io));
    var renew = try sqz.spawnLease(Lease.renew, .{sqz});
    try testing.expectEqual(@as(u32, 15), renew.await(io));
    try testing.expectEqual(@as(u32, 1), sqz.lease_io.?.threads());
    const fuse = sqz.fuseLanes();
    if (fuse.len == 0) return testkit.skip(@src(), .no_device, "the test executor built no FUSE lanes on this host");
    const S = struct {
        fn which() u16 {
            return lane_mod.current_lane.?.index;
        }
    };
    const stack_bytes_before = stats.fiber_stack_bytes.load();
    const home = fuse[0].home_cpu.?;
    var fut = try sqz.spawnHome(home, fuse[0].node, S.which, .{});
    try testing.expectEqual(@as(u16, 0), fut.await(io));
    try testing.expect(stats.fiber_stack_bytes.load() >= stack_bytes_before);
    // The component gauges the MEASURED residency (≤ the mapped reserve) and publishes it.
    const c = &budget.components()[0];
    const resident = c.current();
    try testing.expect(resident > 0 and resident <= stats.fiber_stack_bytes.load());
    try testing.expectEqual(resident, stats.fiber_stack_resident_bytes.load());
    // The component carries the idle trim: with min 0 every free touched
    // stack on every lane returns its pages and both gauges fall.
    const before_trim = stats.fiber_stack_bytes.load();
    c.trim.?.call(c.trim.?.ctx, 0);
    try testing.expect(stats.fiber_stack_bytes.load() < before_trim);
    try testing.expect(c.current() < resident);
    if (fuse.len < 2) return;
    // Kill lane 0's ring from its own thread (the lane's loud log lands
    // there): the next spawn homed on it is re-routed, counted, logged once.
    const redispatch_before = placement.dead_lane_redispatches.load();
    const Kill = struct {
        fn run(l: *Lane) void {
            log.testing.expectErrorLogs(1);
            l.markRingDeadForTest();
        }
    };
    var kill = try sqz.spawnOn(fuse[0], Kill.run, .{fuse[0]});
    kill.await(io);
    while (fuse[0].alive.load(.acquire)) try std.Thread.yield();
    log.testing.expectErrorLogs(1);
    var fut2 = try sqz.spawnHome(home, fuse[0].node, S.which, .{});
    try testing.expect(fut2.await(io) != 0);
    try log.testing.expectAllFired();
    try testing.expect(placement.dead_lane_redispatches.load() > redispatch_before);
}

test "every fuse lane dead aborts loud (a forked child dies with SIGABRT)" {
    const pid_rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(pid_rc));
    if (pid_rc == 0) {
        const sqz = SqzIo.init(std.heap.page_allocator, .{ .budget_bytes = derive.GiB, .fuse = .{ .nqueues = 2, .q_depth = 4 }, .meta_lanes = 1, .blocking_cap = 8 }) catch linux.exit(3);
        if (sqz.fuseLanes().len == 0) linux.exit(5);
        // The child's loud lines are its own; route them through the seam.
        log.testing.expectErrorLogs(sqz.fuseLanes().len + 1);
        for (sqz.fuseLanes()) |l| l.alive.store(false, .release);
        const S = struct {
            fn nothing() void {}
        };
        _ = sqz.spawnHome(null, null, S.nothing, .{}) catch linux.exit(6);
        linux.exit(4);
    }
    const pid: linux.pid_t = @intCast(pid_rc);
    var status: i32 = 0;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.waitpid(pid, &status, 0)));
    const st: u32 = @bitCast(status);
    if (linux.W.IFEXITED(st) and linux.W.EXITSTATUS(st) == 5) return testkit.skip(@src(), .no_device, "the forked child built no FUSE lanes on this host");
    try testing.expect(linux.W.IFSIGNALED(st));
    try testing.expectEqual(linux.SIG.ABRT, linux.W.TERMSIG(st));
}

// rung-10-open-count-stranded: a syncfs handler's bulk promotion pass
// fanned out one `Io.Group.async` member per CPU on its own lane, and the
// members took the fibers the lane holds for the handlers its slots
// deliver — the transport answered the next RELEASE EAGAIN (lost).
test "optional fan-out from a fiber on a FUSE lane (Io.Group.async past every bound) never takes the fibers the lane holds for its handlers: with the fan-out parked, the lane still admits its slot share and its helpers' slack" {
    const sqz = try SqzIo.init(testing.allocator, .{ .budget_bytes = derive.GiB, .fuse = .{ .nqueues = 1, .q_depth = 4 }, .meta_lanes = 1, .blocking_cap = 4 });
    defer sqz.deinit();
    const io = sqz.io();
    if (sqz.fuseLanes().len == 0) return testkit.skip(@src(), .no_device, "the test executor built no FUSE lanes on this host");
    const lane = sqz.fuseLanes()[0];
    const S = struct {
        gate: Io.Event = .unset,
        handler_fiber: std.atomic.Value(?*Fiber) = .init(null),
        inline_parked: std.atomic.Value(bool) = .init(false),
        fn member(s: *@This(), io_: Io) void {
            // A member past the bound runs inline, on the handler's fiber.
            if (lane_mod.currentFiber() == s.handler_fiber.load(.acquire)) s.inline_parked.store(true, .release);
            s.gate.wait(io_) catch {};
        }
        fn handler(s: *@This(), io_: Io, n: usize) void {
            s.handler_fiber.store(lane_mod.currentFiber(), .release);
            var g: Io.Group = .init;
            for (0..n) |_| g.async(io_, member, .{ s, io_ });
            g.await(io_) catch {};
        }
        fn hold(s: *@This(), io_: Io) void {
            s.gate.wait(io_) catch {};
        }
        fn parked(s: *@This()) bool {
            return s.inline_parked.load(.acquire);
        }
    };
    var s: S = .{};
    var handlers: Io.Group = .init;
    defer {
        s.gate.set(io);
        handlers.await(io) catch {};
    }
    try sqz.spawnGroupOn(lane, &handlers, S.handler, .{ &s, io, 2 * @as(usize, lane.fiber_capacity) });
    const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
    while (!S.parked(&s)) {
        try testing.expect(clock.monotonicNs() < deadline);
        std.Thread.yield() catch {};
    }
    // The slot share (q_depth × ceil(1 queue / lanes) = 4) less the handler
    // above, then the lane's own helpers' slack: all admitted.
    const share: usize = 4;
    for (0..share - 1 + spawn_slack) |_| try sqz.spawnGroupOn(lane, &handlers, S.hold, .{ &s, io });
}

test "stats JSON names every lane of the instance" {
    const sqz = try SqzIo.testInstance();
    defer sqz.deinit();
    var buf: [16384]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try sqz.writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"class\":\"meta\"") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"class\":\"main\"") != null);
}
