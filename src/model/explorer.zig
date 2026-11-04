//! Exhaustive interleaving explorer for lock-free cores (charter §9
//! "Model-check every lock-free core").
//!
//! A core is written as a standalone, dependency-free MODEL: a plain value
//! type whose shared words are ordinary fields and whose threads are explicit
//! program counters. Each `step(t)` performs exactly ONE atomic action of
//! thread `t` (a load, a store, an RMW, or a fence boundary). The explorer then
//! enumerates every schedule by depth-first search over the thread choice at
//! every step, checking `invariant` after each action and `final` when all
//! threads have finished. States are memoised by content hash, so the search
//! runs over the STATE GRAPH rather than the schedule tree — the same
//! interleaving-equivalent prefixes collapse, which is what makes exhaustive
//! exploration of 3–4 thread models tractable.
//!
//! Memory model: sequential consistency. Every atomic in the product carries
//! an explicit ordering, and the models encode the FENCE as a step, so a
//! "weakening" (dropping a fence) is modelled by deleting that step — the
//! model must then FAIL, and the evidence note records which weakening fails
//! each model. Store buffering (TSO) is modelled explicitly where a core's
//! correctness depends on it (a model carries a per-thread pending store the
//! fence step drains), never assumed.
//!
//! Model contract (checked at comptime):
//!   pub const thread_count: usize;
//!   pub fn init() Model;
//!   pub fn done(self: *const Model, t: usize) bool;   // thread t finished
//!   pub fn step(self: *Model, t: usize) void;         // ONE atomic action
//!   pub fn invariant(self: *const Model) !void;       // after every step
//!   pub fn final(self: *const Model) !void;           // when all done
//! Optional:
//!   pub fn key(self: *const Model) u64;               // state hash; default = hash of the bytes
//!   pub fn stuck(self: *const Model) !void;           // at an unfinished quiescent state
//!
//! QUIESCENCE AND CYCLES (opt-in, decision 0003 §DO / decision 0004 §7.5.1).
//! A model that declares `stuck` is explored with two more checks. A step
//! that leaves the state key unchanged is a BLOCKED step and is not recursed
//! into; at a state where some thread is unfinished and no thread's step
//! changes the key, `stuck()` runs and its error fails the search as
//! `error.StuckViolated` with the schedule. Such a state is only reached on
//! an acyclic state graph (a memoised search returns at a seen key, so a
//! strand inside a non-progress cycle never looks quiescent), so the same
//! models are checked to BE acyclic: the memo marks a state on-path when the
//! search enters it and done when it leaves, and a step to an on-path state
//! (a back edge; self-loops are blocked steps) fails as `error.CycleViolated`.
//! A complete depth-first search meets a back edge for every cycle, so the
//! check is complete up to a 64-bit key collision, the memo's own tolerance.
//! A model without `stuck` is explored exactly as before.

const std = @import("std");

pub const Limits = struct {
    /// Refuse to run away: a model whose state graph exceeds this many
    /// distinct states is too large for exhaustive exploration here.
    max_states: usize = 2_000_000,
    /// The longest schedule explored (the sum of all threads' step counts).
    max_depth: usize = 256,
};

pub const Report = struct {
    states: usize,
    /// The schedule (thread ids in order) that reached the failure; empty on success.
    schedule: []const u8,
    max_depth_seen: usize,
};

pub const Error = error{
    InvariantViolated,
    FinalViolated,
    /// An opted-in model's `stuck()` failed at an unfinished quiescent state.
    StuckViolated,
    /// An opted-in model stepped to a state on the current search path.
    CycleViolated,
    StateSpaceExceeded,
    DepthExceeded,
    OutOfMemory,
};

fn checkContract(comptime Model: type) void {
    comptime {
        if (!@hasDecl(Model, "thread_count")) @compileError("model needs `pub const thread_count`");
        if (!@hasDecl(Model, "init")) @compileError("model needs `pub fn init() Model`");
        if (!@hasDecl(Model, "done")) @compileError("model needs `pub fn done(*const Model, usize) bool`");
        if (!@hasDecl(Model, "step")) @compileError("model needs `pub fn step(*Model, usize) void`");
        if (!@hasDecl(Model, "invariant")) @compileError("model needs `pub fn invariant(*const Model) !void`");
        if (!@hasDecl(Model, "final")) @compileError("model needs `pub fn final(*const Model) !void`");
    }
}

fn stateKey(comptime Model: type, m: *const Model) u64 {
    if (@hasDecl(Model, "key")) return m.key();
    return std.hash.Wyhash.hash(0, std.mem.asBytes(m));
}

/// The model opted into the quiescence and cycle checks.
fn optsIn(comptime Model: type) bool {
    return @hasDecl(Model, "stuck");
}

/// The memo's mark of an opted-in model's state: on the current search path,
/// or left.
const Mark = enum(u8) { on_path, done };

/// Explore every schedule of `Model`. On failure the returned error carries
/// the schedule in `report.schedule` (caller frees with `gpa.free`).
pub fn explore(comptime Model: type, gpa: std.mem.Allocator, limits: Limits, report: *Report) Error!void {
    checkContract(Model);
    var schedule: std.ArrayList(u8) = .empty;
    errdefer schedule.deinit(gpa);
    report.* = .{ .states = 0, .schedule = &.{}, .max_depth_seen = 0 };

    const root = Model.init();
    root.invariant() catch return error.InvariantViolated;
    if (comptime optsIn(Model)) {
        var marks = std.AutoHashMap(u64, Mark).init(gpa);
        defer marks.deinit();
        dfsQuiescent(Model, gpa, &marks, &schedule, root, limits, report) catch |err| {
            report.schedule = try schedule.toOwnedSlice(gpa);
            return err;
        };
    } else {
        var seen = std.AutoHashMap(u64, void).init(gpa);
        defer seen.deinit();
        dfs(Model, gpa, &seen, &schedule, root, limits, report) catch |err| {
            report.schedule = try schedule.toOwnedSlice(gpa);
            return err;
        };
    }
    schedule.deinit(gpa);
}

fn dfs(
    comptime Model: type,
    gpa: std.mem.Allocator,
    seen: *std.AutoHashMap(u64, void),
    schedule: *std.ArrayList(u8),
    state: Model,
    limits: Limits,
    report: *Report,
) Error!void {
    const gop = try seen.getOrPut(stateKey(Model, &state));
    if (gop.found_existing) return;
    report.states += 1;
    if (report.states > limits.max_states) return error.StateSpaceExceeded;
    if (schedule.items.len > report.max_depth_seen) report.max_depth_seen = schedule.items.len;

    var all_done = true;
    var t: usize = 0;
    while (t < Model.thread_count) : (t += 1) {
        if (state.done(t)) continue;
        all_done = false;
        if (schedule.items.len >= limits.max_depth) return error.DepthExceeded;
        var next = state;
        next.step(t);
        try schedule.append(gpa, @intCast(t));
        next.invariant() catch return error.InvariantViolated;
        try dfs(Model, gpa, seen, schedule, next, limits, report);
        _ = schedule.pop();
    }
    if (all_done) state.final() catch return error.FinalViolated;
}

/// `dfs` for a model that declares `stuck`: blocked steps are not recursed
/// into, an unfinished state with no key-changing step runs `stuck()`, and a
/// step back onto the search path is a cycle.
fn dfsQuiescent(
    comptime Model: type,
    gpa: std.mem.Allocator,
    marks: *std.AutoHashMap(u64, Mark),
    schedule: *std.ArrayList(u8),
    state: Model,
    limits: Limits,
    report: *Report,
) Error!void {
    const here = stateKey(Model, &state);
    const gop = try marks.getOrPut(here);
    // The back-edge test runs before the seen-state return.
    if (gop.found_existing) return if (gop.value_ptr.* == .on_path) error.CycleViolated else {};
    gop.value_ptr.* = .on_path;
    report.states += 1;
    if (report.states > limits.max_states) return error.StateSpaceExceeded;
    if (schedule.items.len > report.max_depth_seen) report.max_depth_seen = schedule.items.len;

    var all_done = true;
    var moved = false;
    var t: usize = 0;
    while (t < Model.thread_count) : (t += 1) {
        if (state.done(t)) continue;
        all_done = false;
        if (schedule.items.len >= limits.max_depth) return error.DepthExceeded;
        var next = state;
        next.step(t);
        if (stateKey(Model, &next) == here) continue;
        moved = true;
        try schedule.append(gpa, @intCast(t));
        next.invariant() catch return error.InvariantViolated;
        try dfsQuiescent(Model, gpa, marks, schedule, next, limits, report);
        _ = schedule.pop();
    }
    if (all_done) {
        state.final() catch return error.FinalViolated;
    } else if (!moved) {
        state.stuck() catch return error.StuckViolated;
    }
    // The entry is still there: the subtree only adds keys.
    marks.getPtr(here).?.* = .done;
}

/// No unfinished thread of `m` has a step that changes the state key.
fn quiescent(comptime Model: type, m: *const Model) bool {
    const here = stateKey(Model, m);
    for (0..Model.thread_count) |t| {
        if (m.done(t)) continue;
        var next = m.*;
        next.step(t);
        if (stateKey(Model, &next) != here) return false;
    }
    return true;
}

/// The longest schedule `replayError` replays (the default depth bound is 256).
pub const max_replay_steps = 4096;

/// The error a failing schedule names, mirroring the search that found it:
/// the first `invariant()` error along the path, the root state's included; for a model that declares
/// `stuck`, a step back onto the replayed path is `error.CycleViolated` (the
/// search recursed only into key-changing steps, so a replayed path holds no
/// self-loop); at the end, `final()` when every thread is done, `stuck()`
/// only at a quiescent end, else `error.NoInvariantNamed`.
pub fn replayError(comptime Model: type, schedule: []const u8) anyerror {
    checkContract(Model);
    if (schedule.len >= max_replay_steps) return error.ScheduleTooLong;
    var path: [max_replay_steps]u64 = undefined;
    var m = Model.init();
    m.invariant() catch |e| return e;
    path[0] = stateKey(Model, &m);
    for (schedule, 1..) |t, i| {
        m.step(t);
        m.invariant() catch |e| return e;
        const k = stateKey(Model, &m);
        if (comptime optsIn(Model)) {
            if (std.mem.indexOfScalar(u64, path[0..i], k) != null) return error.CycleViolated;
        }
        path[i] = k;
    }
    var all_done = true;
    for (0..Model.thread_count) |t| {
        if (!m.done(t)) all_done = false;
    }
    if (all_done) {
        m.final() catch |e| return e;
    } else if (comptime optsIn(Model)) {
        if (quiescent(Model, &m)) m.stuck() catch |e| return e;
    }
    return error.NoInvariantNamed;
}

/// Test helper: explore and expect success; on failure print the schedule.
pub fn expectHolds(comptime Model: type, limits: Limits) !void {
    var report: Report = undefined;
    explore(Model, std.testing.allocator, limits, &report) catch |err| {
        defer std.testing.allocator.free(report.schedule);
        std.debug.print("model {s} failed with {s} after {d} states; schedule: {any}\n", .{ @typeName(Model), @errorName(err), report.states, report.schedule });
        return err;
    };
}

/// Test helper: the WEAKENED model must fail — a model that cannot fail when
/// its fence is removed is not checking anything.
pub fn expectFails(comptime Model: type, limits: Limits, expected: Error) !void {
    var report: Report = undefined;
    const result = explore(Model, std.testing.allocator, limits, &report);
    if (result) |_| {
        std.debug.print("weakened model {s} unexpectedly holds over {d} states\n", .{ @typeName(Model), report.states });
        return error.WeakeningNotDetected;
    } else |err| {
        defer std.testing.allocator.free(report.schedule);
        try std.testing.expectEqual(expected, err);
    }
}

// ---------------------------------------------------------------------------
// Self-tests: a lost-update counter (must fail), a CAS counter (must hold),
// and Dekker's flag pair with and without its store-buffer fence.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Two threads each do `tmp = load(c); store(c, tmp + 1)` — the classic lost
/// update. The explorer must find the schedule where both load 0.
const RacyCounter = struct {
    pub const thread_count: usize = 2;
    c: u32 = 0,
    pc: [2]u8 = @splat(0),
    tmp: [2]u32 = @splat(0),

    pub fn init() RacyCounter {
        return .{};
    }
    pub fn done(self: *const RacyCounter, t: usize) bool {
        return self.pc[t] == 2;
    }
    pub fn step(self: *RacyCounter, t: usize) void {
        switch (self.pc[t]) {
            0 => self.tmp[t] = self.c,
            1 => self.c = self.tmp[t] + 1,
            else => unreachable,
        }
        self.pc[t] += 1;
    }
    pub fn invariant(_: *const RacyCounter) !void {}
    pub fn final(self: *const RacyCounter) !void {
        if (self.c != 2) return error.LostUpdate;
    }
};

/// Same shape with a CAS retry loop: every schedule ends at 2.
const CasCounter = struct {
    pub const thread_count: usize = 3;
    c: u32 = 0,
    pc: [3]u8 = @splat(0),
    tmp: [3]u32 = @splat(0),

    pub fn init() CasCounter {
        return .{};
    }
    pub fn done(self: *const CasCounter, t: usize) bool {
        return self.pc[t] == 2;
    }
    pub fn step(self: *CasCounter, t: usize) void {
        switch (self.pc[t]) {
            0 => {
                self.tmp[t] = self.c;
                self.pc[t] = 1;
            },
            1 => {
                // cmpxchg(c, tmp, tmp+1): success advances, failure retries.
                if (self.c == self.tmp[t]) {
                    self.c = self.tmp[t] + 1;
                    self.pc[t] = 2;
                } else {
                    self.pc[t] = 0;
                }
            },
            else => unreachable,
        }
    }
    pub fn invariant(self: *const CasCounter) !void {
        if (self.c > 3) return error.Overshoot;
    }
    pub fn final(self: *const CasCounter) !void {
        if (self.c != 3) return error.LostUpdate;
    }
};

/// Dekker's flag pair under TSO: each thread stores its flag, FENCES, then
/// loads the other's flag. Without the fence (a store still in the store
/// buffer when the load runs) both can read 0 and both enter — the exact
/// skip-wake ∧ park shape the payload-lease commit gate and the completion
/// doorbell rely on SeqCst fences to forbid (charter §4, §7).
fn Dekker(comptime with_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        flag: [2]u8 = @splat(0),
        /// Per-thread store buffer: a flag store lands here until the fence
        /// (or a later step) drains it. Models TSO's visible reordering.
        pending: [2]bool = @splat(false),
        saw_other: [2]u8 = @splat(0),
        entered: [2]bool = @splat(false),
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }
        fn drain(self: *Self, t: usize) void {
            if (self.pending[t]) {
                self.flag[t] = 1;
                self.pending[t] = false;
            }
        }
        pub fn step(self: *Self, t: usize) void {
            const other = 1 - t;
            switch (self.pc[t]) {
                0 => {
                    // store(flag[t], 1): buffered under TSO.
                    self.pending[t] = true;
                    self.pc[t] = 1;
                },
                1 => {
                    // fence(SeqCst) drains the store buffer before the load;
                    // the weakened model skips the drain.
                    if (with_fence) self.drain(t);
                    self.pc[t] = 2;
                },
                2 => {
                    // load(flag[other]).
                    self.saw_other[t] = self.flag[other];
                    if (self.saw_other[t] == 0) self.entered[t] = true;
                    self.pc[t] = 3;
                },
                3 => {
                    // The buffered store lands at some later point — a
                    // separate step, so the explorer can place it after the
                    // OTHER thread's load (the TSO reordering).
                    self.drain(t);
                    self.pc[t] = 4;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.entered[0] and self.entered[1]) return error.MutualExclusionBroken;
        }
        pub fn final(_: *const Self) !void {}
    };
}

test "explorer finds the lost update in a racy counter" {
    try expectFails(RacyCounter, .{}, error.FinalViolated);
}

test "explorer proves the CAS counter over every schedule" {
    try expectHolds(CasCounter, .{});
}

test "Dekker holds with the SeqCst fence and fails without it (the recorded weakening)" {
    try expectHolds(Dekker(true), .{});
    try expectFails(Dekker(false), .{}, error.InvariantViolated);
}

/// `Base` opted into the quiescence and cycle checks: every quiescent
/// unfinished state is a strand.
fn DeclaresStuck(comptime Base: type) type {
    return struct {
        const Self = @This();
        pub const thread_count = Base.thread_count;
        base: Base = .{},

        pub fn init() Self {
            return .{ .base = Base.init() };
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.base.done(t);
        }
        pub fn step(self: *Self, t: usize) void {
            self.base.step(t);
        }
        pub fn invariant(self: *const Self) !void {
            return self.base.invariant();
        }
        pub fn final(self: *const Self) !void {
            return self.base.final();
        }
        pub fn stuck(_: *const Self) !void {
            return error.Stranded;
        }
    };
}

/// Two threads, each waiting for the other's flag before it sets its own: a
/// circular wait. Every step is blocked from the first state on.
const CircularWait = struct {
    pub const thread_count: usize = 2;
    flag: [2]bool = @splat(false),
    pc: [2]u8 = @splat(0),

    pub fn init() CircularWait {
        return .{};
    }
    pub fn done(self: *const CircularWait, t: usize) bool {
        return self.pc[t] == 2;
    }
    pub fn step(self: *CircularWait, t: usize) void {
        switch (self.pc[t]) {
            0 => if (self.flag[1 - t]) {
                self.pc[t] = 1;
            },
            1 => {
                self.flag[t] = true;
                self.pc[t] = 2;
            },
            else => unreachable,
        }
    }
    pub fn invariant(_: *const CircularWait) !void {}
    pub fn final(_: *const CircularWait) !void {}
};

/// A waiter released by a bounded environment thread: the environment sets
/// the flag once and finishes; the waiter then finishes.
const ReleasedWaiter = struct {
    pub const thread_count: usize = 2;
    flag: bool = false,
    pc: [2]u8 = @splat(0),

    pub fn init() ReleasedWaiter {
        return .{};
    }
    pub fn done(self: *const ReleasedWaiter, t: usize) bool {
        return self.pc[t] == 1;
    }
    pub fn step(self: *ReleasedWaiter, t: usize) void {
        switch (t) {
            0 => if (self.flag) {
                self.pc[0] = 1;
            },
            1 => {
                self.flag = true;
                self.pc[1] = 1;
            },
            else => unreachable,
        }
    }
    pub fn invariant(_: *const ReleasedWaiter) !void {}
    pub fn final(self: *const ReleasedWaiter) !void {
        if (!self.flag) return error.NeverReleased;
    }
    pub fn stuck(_: *const ReleasedWaiter) !void {
        return error.Stranded;
    }
};

/// A blocked waiter (thread 0, waiting for a flag nobody sets) beside a
/// two-thread ping-pong on another flag: every state has a key-changing
/// step, so the state is never quiescent — the strand hides in a
/// non-progress cycle, which the cycle check finds.
const PingPong = struct {
    pub const thread_count: usize = 3;
    ball: bool = false,
    go: bool = false,
    waited: bool = false,

    pub fn init() PingPong {
        return .{};
    }
    pub fn done(self: *const PingPong, t: usize) bool {
        return t == 0 and self.waited;
    }
    pub fn step(self: *PingPong, t: usize) void {
        switch (t) {
            0 => if (self.go) {
                self.waited = true;
            },
            1 => if (!self.ball) {
                self.ball = true;
            },
            2 => if (self.ball) {
                self.ball = false;
            },
            else => unreachable,
        }
    }
    pub fn invariant(_: *const PingPong) !void {}
    pub fn final(_: *const PingPong) !void {}
};

/// A bounded retry loop whose counter changes the state: thread 0 retries a
/// CAS against thread 1's increments, counting its attempts, and gives up at
/// three. Never a back edge, never quiescent before every thread finished.
const BoundedRetry = struct {
    pub const thread_count: usize = 2;
    c: u8 = 0,
    tmp: u8 = 0,
    attempts: u8 = 0,
    pc: [2]u8 = @splat(0),

    pub fn init() BoundedRetry {
        return .{};
    }
    pub fn done(self: *const BoundedRetry, t: usize) bool {
        return self.pc[t] == 2;
    }
    pub fn step(self: *BoundedRetry, t: usize) void {
        if (t == 1) {
            self.c += 1;
            self.pc[1] = if (self.c >= 3) 2 else 0;
            return;
        }
        switch (self.pc[0]) {
            0 => {
                self.tmp = self.c;
                self.pc[0] = 1;
            },
            1 => {
                self.attempts += 1;
                if (self.c == self.tmp) {
                    self.c += 10;
                    self.pc[0] = 2;
                } else self.pc[0] = if (self.attempts >= 3) 2 else 0;
            },
            else => unreachable,
        }
    }
    pub fn invariant(self: *const BoundedRetry) !void {
        if (self.attempts > 3) return error.Unbounded;
    }
    pub fn final(_: *const BoundedRetry) !void {}
    pub fn stuck(_: *const BoundedRetry) !void {
        return error.Stranded;
    }
};

test "the quiescence check (opt-in): a circular wait that declares `stuck` fails StuckViolated; the same model without it holds, as before" {
    try expectFails(DeclaresStuck(CircularWait), .{}, error.StuckViolated);
    try expectHolds(CircularWait, .{});
}

test "the quiescence check: a waiter released by a bounded environment thread holds with `stuck` declared" {
    try expectHolds(ReleasedWaiter, .{});
}

test "the cycle check (opt-in): a blocked waiter beside a ping-pong is never quiescent — with `stuck` declared the back edge fails CycleViolated; without it the model holds, as before" {
    try expectFails(DeclaresStuck(PingPong), .{}, error.CycleViolated);
    try expectHolds(PingPong, .{});
}

test "the cycle check: a bounded retry loop whose counter changes the state holds with `stuck` declared" {
    try expectHolds(BoundedRetry, .{});
}

test "replayError mirrors the search: the ping-pong's schedule names CycleViolated, never the model's stuck() error; the circular wait's names the model's stuck() error" {
    var report: Report = undefined;
    try testing.expectError(error.CycleViolated, explore(DeclaresStuck(PingPong), testing.allocator, .{}, &report));
    try testing.expectEqual(@as(anyerror, error.CycleViolated), replayError(DeclaresStuck(PingPong), report.schedule));
    testing.allocator.free(report.schedule);
    try testing.expectError(error.StuckViolated, explore(DeclaresStuck(CircularWait), testing.allocator, .{}, &report));
    try testing.expectEqual(@as(anyerror, error.Stranded), replayError(DeclaresStuck(CircularWait), report.schedule));
    testing.allocator.free(report.schedule);
    // A model that does not opt in replays as before: its final() at an
    // all-done end, else no invariant named.
    try testing.expectError(error.FinalViolated, explore(RacyCounter, testing.allocator, .{}, &report));
    try testing.expectEqual(@as(anyerror, error.LostUpdate), replayError(RacyCounter, report.schedule));
    testing.allocator.free(report.schedule);
    try testing.expectEqual(@as(anyerror, error.NoInvariantNamed), replayError(PingPong, &.{ 1, 2 }));
}

test "state-space and depth limits are refusals, not hangs" {
    var report: Report = undefined;
    try testing.expectError(error.StateSpaceExceeded, explore(CasCounter, testing.allocator, .{ .max_states = 3 }, &report));
    testing.allocator.free(report.schedule);
    try testing.expectError(error.DepthExceeded, explore(CasCounter, testing.allocator, .{ .max_depth = 2 }, &report));
    testing.allocator.free(report.schedule);
}
