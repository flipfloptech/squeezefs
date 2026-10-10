//! The exhaustive MODELS of `span_ledger.zig` (AGENTS.md "Model checks";
//! `docs/porting/ops_zcrx.md` §3.2 — the two loom models ported to
//! `model/explorer.zig`). Standalone value types, every field a byte, ONE
//! atomic action per `step`.
//!
//! Memory model: the explorer is sequentially consistent, so a data race
//! between a consumer's payload READ and the driver's recycle WRITE cannot
//! be seen by value ordering alone — under SC the write always lands after
//! the reads in any schedule the counter admits. The race is therefore
//! tracked the way loom tracks `UnsafeCell` accesses: HAPPENS-BEFORE by
//! vector clocks. Every thread carries a clock; a Release store/RMW
//! publishes the thread's clock into the location; an Acquire load/RMW/
//! fence joins the location's clock into the thread's; a non-atomic
//! access records the accessor's clock. The driver's overwrite is a RACE
//! iff some recorded read's clock is not ≤ the driver's clock. Each
//! weakening deletes exactly one synchronisation edge and the model FAILS:
//!
//! | model | threads | invariant | weakening that fails it |
//! |---|---|---|---|
//! | 1 `chunk_recycle_never_races_consumers` | driver grant→write→publish→release→re-grant→overwrite; two consumers read→release | every payload read happens-before the recycle write | `fetch_sub_relaxed` (the releaser publishes no clock), `no_acquire_fence` (the last releaser joins nothing before pushing), `push_relaxed` (the push publishes no clock) |
//! | 2 `free_stack_returns_exactly_once_no_loss` | two last-ref releases of distinct slots race the single popper | each slot pushed once and popped once; the packed head's tag/top arithmetic never corrupts | — (SC structural; pins the encoding) |

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

pub const Weakening = enum(u8) {
    none,
    /// `release`'s `fetchSub` demoted to Relaxed.
    fetch_sub_relaxed,
    /// The 0-crossing acquire deleted.
    no_acquire_fence,
    /// The free-stack push CAS demoted to Relaxed.
    push_relaxed,
};

const Clock = [3]u8;

fn join(into: *Clock, from: Clock) void {
    for (into, from) |*a, b| a.* = @max(a.*, b);
}

fn leq(a: Clock, b: Clock) bool {
    for (a, b) |x, y| if (x > y) return false;
    return true;
}

/// Model 1.
pub fn RecycleModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const D = 0;

        // Shared words.
        refs: u8 = 0,
        /// 1 = slot 0 on the free stack, 0 = empty.
        head_top: u8 = 1,
        ready: u8 = 0,
        payload: u8 = 0,
        // Location release clocks.
        refs_clk: Clock = @splat(0),
        head_clk: Clock = @splat(0),
        ready_clk: Clock = @splat(0),
        // Thread clocks and program counters.
        clk: [3]Clock = @splat(@splat(0)),
        pc: [3]u8 = @splat(0),
        // Consumer read records.
        read_done: [2]u8 = @splat(0),
        read_clk: [2]Clock = @splat(@splat(0)),
        read_val: [2]u8 = @splat(0),
        /// The driver's recycle write did not happen-after some read.
        raced: u8 = 0,
        /// The slot was pushed free more than once.
        double_push: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        fn tick(self: *Self, t: usize) void {
            self.clk[t][t] += 1;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return if (t == D) self.pc[D] == 7 else self.pc[t] == 5;
        }

        // Shared primitives.

        /// `fetchSub(1, release)` (or relaxed under the weakening); returns
        /// the previous value.
        fn releaseRef(self: *Self, t: usize) u8 {
            if (weakening != .fetch_sub_relaxed) join(&self.refs_clk, self.clk[t]);
            const prev = self.refs;
            self.refs -= 1;
            self.tick(t);
            return prev;
        }

        fn acquireFence(self: *Self, t: usize) void {
            if (weakening != .no_acquire_fence) join(&self.clk[t], self.refs_clk);
            self.tick(t);
        }

        fn pushFree(self: *Self, t: usize) void {
            if (self.head_top != 0) self.double_push = 1;
            self.head_top = 1;
            if (weakening != .push_relaxed) {
                self.head_clk = self.clk[t];
            } else {
                // A relaxed store heads no release sequence: the location
                // carries no clock for a popper to acquire.
                self.head_clk = @splat(0);
            }
            self.tick(t);
        }

        /// The acquire pop; false when the stack is empty (the thread stays
        /// at its pc — a blocked step is a no-op).
        fn pop(self: *Self, t: usize) bool {
            if (self.head_top == 0) return false;
            self.head_top = 0;
            join(&self.clk[t], self.head_clk);
            self.refs = 1;
            self.tick(t);
            return true;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == D) return self.stepDriver();
            const c = t - 1;
            switch (self.pc[t]) {
                0 => {
                    // Acquire load of `ready`; blocked while 0.
                    if (self.ready == 0) return;
                    join(&self.clk[t], self.ready_clk);
                    self.tick(t);
                    self.pc[t] = 1;
                },
                1 => {
                    // The non-atomic payload read.
                    self.read_val[c] = self.payload;
                    self.read_clk[c] = self.clk[t];
                    self.read_done[c] = 1;
                    self.tick(t);
                    self.pc[t] = 2;
                },
                2 => {
                    const prev = self.releaseRef(t);
                    self.pc[t] = if (prev == 1) 3 else 5;
                },
                3 => {
                    self.acquireFence(t);
                    self.pc[t] = 4;
                },
                4 => {
                    self.pushFree(t);
                    self.pc[t] = 5;
                },
                else => unreachable,
            }
        }

        fn stepDriver(self: *Self) void {
            switch (self.pc[D]) {
                0 => {
                    // grant: the initial pop.
                    std.debug.assert(self.pop(D));
                    self.pc[D] = 1;
                },
                1 => {
                    // The epoch-1 payload write.
                    self.payload = 1;
                    self.tick(D);
                    self.pc[D] = 2;
                },
                2 => {
                    // Two clone edges (relaxed fetchAdd ×2 collapsed: the
                    // consumers cannot observe the slot before `ready`).
                    self.refs += 2;
                    self.tick(D);
                    self.pc[D] = 3;
                },
                3 => {
                    // Publish: release store of `ready`.
                    self.ready_clk = self.clk[D];
                    self.ready = 1;
                    self.tick(D);
                    self.pc[D] = 4;
                },
                4 => {
                    // The driver drops its own ref; if both consumers already
                    // released, the driver is the last releaser.
                    const prev = self.releaseRef(D);
                    self.pc[D] = if (prev == 1) 40 else 5;
                },
                40 => {
                    self.acquireFence(D);
                    self.pc[D] = 41;
                },
                41 => {
                    self.pushFree(D);
                    self.pc[D] = 5;
                },
                5 => {
                    // Re-grant: blocked until the slot is back on the stack.
                    if (!self.pop(D)) return;
                    self.pc[D] = 6;
                },
                6 => {
                    // The epoch-2 overwrite: THE race check.
                    for (0..2) |c| if (self.read_done[c] != 0 and !leq(self.read_clk[c], self.clk[D])) {
                        self.raced = 1;
                    };
                    self.payload = 2;
                    self.tick(D);
                    self.pc[D] = 7;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.raced != 0) return error.RecycleRacedAConsumerRead;
            if (self.double_push != 0) return error.SlotPushedTwice;
        }

        pub fn final(self: *const Self) !void {
            // Both consumers read the epoch-1 bytes and released; the slot
            // was granted twice; nothing is outstanding.
            for (0..2) |c| {
                if (self.read_done[c] == 0) return error.ConsumerNeverRead;
                if (self.read_val[c] != 1) return error.ConsumerReadRecycledBytes;
            }
            if (self.refs != 1) return error.RefsNotOne;
            if (self.head_top != 0) return error.StackNotEmpty;
        }
    };
}

/// Model 2: the packed head word `(pop_tag << 32) | (top + 1)` over TWO
/// slots, both held (`refs = 1`) by one consumer each; the consumers
/// release concurrently (push), the driver pops twice.
pub const FreeStackModel = struct {
    const Self = @This();
    pub const thread_count: usize = 3;
    const D = 0;

    refs: [2]u8 = .{ 1, 1 },
    /// `successor + 1`, 0 = none.
    next: [2]u8 = .{ 0, 0 },
    /// The low word `top + 1` (0 = empty) and the pop tag.
    head_low: u8 = 0,
    head_tag: u8 = 0,
    pc: [3]u8 = @splat(0),
    popped: [2]u8 = @splat(0),
    pop_count: u8 = 0,
    pushes: [2]u8 = @splat(0),

    pub fn init() Self {
        return .{};
    }

    pub fn done(self: *const Self, t: usize) bool {
        return if (t == D) self.pop_count == 2 else self.pc[t] == 2;
    }

    pub fn step(self: *Self, t: usize) void {
        if (t == D) {
            // One acquire CAS pop per step; blocked while empty.
            if (self.head_low == 0) return;
            const top = self.head_low - 1;
            self.head_low = self.next[top];
            self.head_tag +%= 1;
            self.refs[top] = 1;
            self.popped[top] += 1;
            self.pop_count += 1;
            return;
        }
        const slot = t - 1;
        switch (self.pc[t]) {
            0 => {
                // fetchSub 1 → 0: the last ref.
                self.refs[slot] -= 1;
                self.pc[t] = 1;
            },
            1 => {
                // The release CAS push: `next[slot] = head_low; head_low = slot + 1`.
                self.next[slot] = self.head_low;
                self.head_low = @intCast(slot + 1);
                self.pushes[slot] += 1;
                self.pc[t] = 2;
            },
            else => unreachable,
        }
    }

    pub fn invariant(self: *const Self) !void {
        for (self.pushes) |p| if (p > 1) return error.PushedTwice;
        for (self.popped) |p| if (p > 1) return error.PoppedTwice;
        // The chain never names a slot still held (refs 1 and not pushed).
        var cur = self.head_low;
        var hops: u8 = 0;
        while (cur != 0) : (hops += 1) {
            if (hops > 2) return error.ChainCycle;
            if (self.pushes[cur - 1] == 0) return error.ChainNamesHeldSlot;
            cur = self.next[cur - 1];
        }
    }

    pub fn final(self: *const Self) !void {
        for (self.popped) |p| if (p != 1) return error.SlotNotRecycledExactlyOnce;
        for (self.pushes) |p| if (p != 1) return error.SlotNotPushedExactlyOnce;
        if (self.head_low != 0) return error.StackNotEmpty;
        if (self.head_tag != 2) return error.TagDidNotCountPops;
    }
};

const testing = std.testing;
const limits: explorer.Limits = .{ .max_states = 200_000, .max_depth = 64 };

test "model 1: the chunk recycle never races a consumer's read — every payload read happens-before the driver's re-grant write" {
    try explorer.expectHolds(RecycleModel(.none), limits);
}

test "model 1 weakening: `fetchSub` demoted to Relaxed FAILS (the releaser publishes no clock the last releaser can acquire)" {
    try explorer.expectFails(RecycleModel(.fetch_sub_relaxed), limits, error.InvariantViolated);
}

test "model 1 weakening: the 0-crossing acquire removed FAILS (the last releaser pushes without having joined the others' reads)" {
    try explorer.expectFails(RecycleModel(.no_acquire_fence), limits, error.InvariantViolated);
}

test "model 1 weakening: the push CAS demoted to Relaxed FAILS (the popper acquires nothing)" {
    try explorer.expectFails(RecycleModel(.push_relaxed), limits, error.InvariantViolated);
}

test "model 2: racing last-ref releases of distinct slots vs the single popper recycle each slot exactly once; the packed head's tag counts pops" {
    try explorer.expectHolds(FreeStackModel, limits);
}
