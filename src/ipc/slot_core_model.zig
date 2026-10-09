//! Exhaustive models of the slot core (`slot_core.zig`) for
//! `model/explorer.zig` (charter §9 "model-check every lock-free core").
//! Each model is a dependency-free value type: shared words as fields,
//! per-thread program counters, one atomic action per `step`. Each ships
//! with the WEAKENING the evidence note records as the one that breaks it:
//!
//!   1. `Completion(reordered)`: the result store lands AFTER the DONE swap
//!      — the Relaxed-swap shape (`complete`'s `.acq_rel` → `.monotonic`):
//!      a ready/woken consumer reads result 0. The futex bucket is explicit
//!      (a wake delivers only to a QUEUED sleeper), so the strand "parked ∧
//!      no wake pending ∧ DONE" is an invariant, never a hang.
//!   2. `TwoLives(bump_after_publish)`: the generation bump moved out of the
//!      release sequence (after `publishSubmitted`): the server can serve
//!      and complete life 2 before the bump, so a stale gen-1 probe reads
//!      DONE ∧ gen 1 and attributes life 2's DONE to life 1.
//!   3. `Snapshot(relaxed_publish)`: `publishSubmitted` Release → Relaxed,
//!      modelled with a per-thread store buffer the Release drains: the
//!      daemon's one snapshot reads the pre-submit seed.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const FREE: u8 = 0;
const CLAIMED: u8 = 1;
const SUBMITTED: u8 = 2;
const SERVING: u8 = 3;
const DONE: u8 = 4;
const WAITER: u8 = 1 << 5;

fn bits(w: u8) u8 {
    return w & 0x1f;
}

// ---------------------------------------------------------------------------
// Model 1: completion exactly once, parked waiter never stranded.
// ---------------------------------------------------------------------------

pub fn Completion(comptime reordered: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const server = 0;
        const client = 1;

        state: u8 = SUBMITTED,
        result: u8 = 0,
        /// The futex bucket: is the client queued on the word?
        queued: bool = false,
        /// Wakes paid (a wake delivers only to a queued sleeper).
        wakes: u8 = 0,
        expected: u8 = 0,
        consumed: ?u8 = null,
        saw_waiter: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == server) self.pc[server] == 4 else self.pc[client] == 5;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == server) self.stepServer() else self.stepClient();
        }
        fn stepServer(self: *Self) void {
            switch (self.pc[server]) {
                0 => {
                    // tryBeginServe: CAS SUBMITTED → SERVING preserving WAITER.
                    std.debug.assert(bits(self.state) == SUBMITTED);
                    self.state = SERVING | (self.state & WAITER);
                    self.pc[server] = 1;
                },
                1 => {
                    // Shipped: setResult, then the swap. Weakened: the swap
                    // first (the result store is free to land after it).
                    if (!reordered) self.result = 7 else self.swapDone();
                    self.pc[server] = 2;
                },
                2 => {
                    if (!reordered) self.swapDone() else self.result = 7;
                    self.pc[server] = 3;
                },
                3 => {
                    // The wake (iff WAITER was set): delivered only to a
                    // queued sleeper; a wake with nobody queued is a no-op.
                    if (self.saw_waiter and self.queued) {
                        self.queued = false;
                        self.wakes += 1;
                    }
                    self.pc[server] = 4;
                },
                else => unreachable,
            }
        }
        fn swapDone(self: *Self) void {
            self.saw_waiter = self.state & WAITER != 0;
            self.state = DONE;
        }
        fn stepClient(self: *Self) void {
            switch (self.pc[client]) {
                0 => {
                    // The spin check.
                    self.pc[client] = if (bits(self.state) == DONE) 3 else 1;
                },
                1 => {
                    // parkPrepare: ONE fetchOr(WAITER).
                    const prior = self.state;
                    self.state |= WAITER;
                    if (bits(prior) == DONE) {
                        self.pc[client] = 3;
                    } else {
                        self.expected = prior | WAITER;
                        self.pc[client] = 2;
                    }
                },
                2 => {
                    // FUTEX_WAIT admission: a changed word returns at once
                    // (EAGAIN → re-check); else the sleeper is queued.
                    if (self.state != self.expected) {
                        self.pc[client] = 0;
                    } else {
                        self.queued = true;
                        self.pc[client] = 4;
                    }
                },
                4 => {
                    // Asleep: steps only once dequeued by a wake.
                    if (!self.queued) self.pc[client] = 0;
                },
                3 => {
                    self.consumed = self.result;
                    self.pc[client] = 5;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // The strand: parked, the server finished, no wake delivered.
            if (self.queued and self.pc[server] == 4) return error.ParkedWaiterStranded;
        }
        pub fn final(self: *const Self) !void {
            if (self.consumed != 7) return error.ResultNotPublishedWithDone;
            if (self.wakes > 1) return error.DoubleWake;
        }
    };
}

// ---------------------------------------------------------------------------
// Model 2: two lives on one slot; a stale gen-1 probe never sees life 2's
// DONE.
// ---------------------------------------------------------------------------

pub fn TwoLives(comptime bump_after_publish: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const client = 0;
        const server = 1;
        const probe = 2;

        state: u8 = FREE,
        generation: u8 = 0,
        /// Which life's DONE the word currently carries (model bookkeeping).
        done_life: u8 = 0,
        probe_state: u8 = 0,
        pc: [3]u8 = @splat(0),
        /// The client's life counter (1, 2) and the server's.
        life: u8 = 0,
        served: u8 = 0,
        stale_hit: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                client => self.pc[client] == 10,
                server => self.pc[server] == 4,
                else => self.pc[probe] == 8,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                client => self.stepClient(),
                server => self.stepServer(),
                else => self.stepProbe(),
            }
        }
        fn stepClient(self: *Self) void {
            // Two lives of five steps: claim, (bump), publish, wait, consume+release.
            const phase = self.pc[client] % 5;
            switch (phase) {
                0 => {
                    if (self.state != FREE) return; // CAS failed: retry (same state, memoised)
                    self.state = CLAIMED;
                    self.life += 1;
                    if (!bump_after_publish) self.generation = self.life;
                    self.pc[client] += 1;
                },
                1 => {
                    self.state = SUBMITTED;
                    self.pc[client] += 1;
                },
                2 => {
                    if (bump_after_publish) self.generation = self.life;
                    self.pc[client] += 1;
                },
                3 => {
                    // isDoneFor(my gen): state then generation.
                    if (bits(self.state) == DONE and self.generation == self.life) self.pc[client] += 1;
                },
                4 => {
                    self.state = FREE;
                    self.done_life = 0;
                    self.pc[client] += 1;
                },
                else => unreachable,
            }
        }
        fn stepServer(self: *Self) void {
            switch (self.pc[server] % 2) {
                0 => {
                    if (bits(self.state) != SUBMITTED) return;
                    self.state = SERVING;
                    self.pc[server] += 1;
                },
                1 => {
                    self.served += 1;
                    self.done_life = self.served;
                    self.state = DONE;
                    self.pc[server] += 1;
                },
                else => unreachable,
            }
        }
        fn stepProbe(self: *Self) void {
            // Four probes of isDoneFor(1): load state, then load generation.
            switch (self.pc[probe] % 2) {
                0 => {
                    self.probe_state = self.state;
                    self.pc[probe] += 1;
                },
                1 => {
                    if (bits(self.probe_state) == DONE and self.generation == 1) self.stale_hit = self.done_life == 2;
                    self.pc[probe] += 1;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.stale_hit) return error.StaleDoneAttributedToOldGeneration;
        }
        pub fn final(self: *const Self) !void {
            if (self.served != 2) return error.NotBothLivesServed;
        }
    };
}

// ---------------------------------------------------------------------------
// Model 3: the one post-begin_serve snapshot never reads the pre-submit
// seed, even with a hostile sibling scribbling the descriptor.
// ---------------------------------------------------------------------------

pub fn Snapshot(comptime relaxed_publish: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const client = 0;
        const server = 1;
        const hostile = 2;

        state: u8 = CLAIMED,
        desc: u8 = 0,
        /// The client's store buffer: the descriptor write lands here; the
        /// Release publish drains it first.
        pending_desc: ?u8 = null,
        snapshot: ?u8 = null,
        served: ?u8 = null,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                client => self.pc[client] == 3,
                server => self.pc[server] == 3,
                else => self.pc[hostile] == 1,
            };
        }
        fn drain(self: *Self) void {
            if (self.pending_desc) |d| {
                self.desc = d;
                self.pending_desc = null;
            }
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                client => switch (self.pc[client]) {
                    0 => {
                        self.pending_desc = 7;
                        self.pc[client] = 1;
                    },
                    1 => {
                        // publishSubmitted: Release drains the buffer first;
                        // Relaxed lets the state store go ahead of it.
                        if (!relaxed_publish) self.drain();
                        self.state = SUBMITTED;
                        self.pc[client] = 2;
                    },
                    2 => {
                        self.drain();
                        self.pc[client] = 3;
                    },
                    else => unreachable,
                },
                server => switch (self.pc[server]) {
                    0 => {
                        if (bits(self.state) != SUBMITTED) return;
                        self.state = SERVING;
                        self.pc[server] = 1;
                    },
                    1 => {
                        self.snapshot = self.desc;
                        self.pc[server] = 2;
                    },
                    2 => {
                        // The serve works from the private copy.
                        self.served = self.snapshot;
                        self.pc[server] = 3;
                    },
                    else => unreachable,
                },
                else => {
                    // The hostile sibling: one scribble at any time.
                    self.desc = 9;
                    self.pc[hostile] = 1;
                },
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.snapshot) |s| if (s != 7 and s != 9) return error.SnapshotReadPreSubmitSeed;
        }
        pub fn final(self: *const Self) !void {
            if (self.served != self.snapshot) return error.ServedNotTheSnapshot;
        }
    };
}

// ---------------------------------------------------------------------------

test "slot completion: exactly once, a parked waiter is never stranded, ≤ 1 wake — and the Relaxed-swap shape (result after DONE) reads 0" {
    try explorer.expectHolds(Completion(false), .{});
    try explorer.expectFails(Completion(true), .{}, error.FinalViolated);
}

test "slot two lives: a stale gen-1 probe never consumes life 2's DONE — and the bump moved after publishSubmitted attributes it" {
    try explorer.expectHolds(TwoLives(false), .{ .max_depth = 512 });
    try explorer.expectFails(TwoLives(true), .{ .max_depth = 512 }, error.InvariantViolated);
}

test "slot snapshot: the one post-begin_serve read is the submitted or the hostile value, never the seed; the serve equals the snapshot — and a Relaxed publish reads the seed" {
    try explorer.expectHolds(Snapshot(false), .{});
    try explorer.expectFails(Snapshot(true), .{}, error.InvariantViolated);
}
