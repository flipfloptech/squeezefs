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
    return std.hash.XxHash3.hash(0, std.mem.asBytes(m));
}

/// Explore every schedule of `Model`. On failure the returned error carries
/// the schedule in `report.schedule` (caller frees with `gpa.free`).
pub fn explore(comptime Model: type, gpa: std.mem.Allocator, limits: Limits, report: *Report) Error!void {
    checkContract(Model);
    var seen = std.AutoHashMap(u64, void).init(gpa);
    defer seen.deinit();
    var schedule: std.ArrayList(u8) = .empty;
    errdefer schedule.deinit(gpa);
    report.* = .{ .states = 0, .schedule = &.{}, .max_depth_seen = 0 };

    const root = Model.init();
    root.invariant() catch return error.InvariantViolated;
    dfs(Model, gpa, &seen, &schedule, root, limits, report) catch |err| {
        report.schedule = try schedule.toOwnedSlice(gpa);
        return err;
    };
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

test "state-space and depth limits are refusals, not hangs" {
    var report: Report = undefined;
    try testing.expectError(error.StateSpaceExceeded, explore(CasCounter, testing.allocator, .{ .max_states = 3 }, &report));
    testing.allocator.free(report.schedule);
    try testing.expectError(error.DepthExceeded, explore(CasCounter, testing.allocator, .{ .max_depth = 2 }, &report));
    testing.allocator.free(report.schedule);
}
