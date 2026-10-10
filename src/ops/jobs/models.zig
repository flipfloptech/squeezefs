//! The fabric's lock-free cores as standalone MODELS (charter §9 "model-
//! check every lock-free core"; spec §5 "Models"), explored exhaustively by
//! `model/explorer.zig`. Each ships with its WEAKENING proven to fail:
//!
//!   1. THE CLAIM WORD: two workers run `claim_next` over two queued movers
//!      with intersecting scopes. The claim is a CAS on `claimed`; the
//!      scope check reads every OTHER job's claimed word under the table
//!      lock. Invariant: never two claimed movers on one volume.
//!      Weakening: drop the scope check (claim on `queued` alone).
//!   2. THE PARK/RESUME DEKKER: the worker checkpoints `Paused` then
//!      `settle_park` re-checks the intent under the state lock; `resume`
//!      clears the intent and flips `Queued` under the same lock. Invariant
//!      at the end: never `intent = false ∧ state = Paused` (the stranded
//!      job). Weakening: the worker flips `Paused` AFTER its re-check
//!      (checkpoint-then-settle inverted) — the stale flip overwrites the
//!      resume.
//!   3. THE SHARD FENCING PAIR: expiry bumps the fencing and re-leases;
//!      a late submit compares its fencing then publishes. Invariant:
//!      a stale fencing never publishes. Weakening: the submit compares
//!      BEFORE the bump lands — modelled as the expiry publishing its bump
//!      through a store buffer the submit's compare may read stale, i.e.
//!      the compare and the publish are not under the one table lock.
//!
//! Every `step` is ONE atomic action; the "lock" is modelled as a word the
//! threads CAS (so a lock-protected section is a sequence of steps nobody
//! else can enter). Portable.

const std = @import("std");
const explorer = @import("../../model/explorer.zig");

// ---------------------------------------------------------------------------
// 1. The claim word.
// ---------------------------------------------------------------------------

fn ClaimModel(comptime weakened: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        // Two jobs, both movers on volume 1, both queued.
        claimed: [2]bool = .{ false, false },
        table_lock: bool = false,
        /// Per worker: the index it is trying (worker 0 tries job 0 first,
        /// worker 1 tries job 1 first), its pc, and what it holds.
        pc: [2]u8 = .{ 0, 0 },
        cand: [2]u8 = .{ 0, 1 },
        holds: [2]?u8 = .{ null, null },
        busy_seen: [2]bool = .{ false, false },

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= 5;
        }

        // pc 0: take the table lock (CAS).
        // pc 1: scope check: is the OTHER job claimed? (the weakening skips it)
        // pc 2: CAS claimed[cand] false→true (skip if busy)
        // pc 3: release the table lock
        // pc 4: done
        pub fn step(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => if (!self.table_lock) {
                    self.table_lock = true;
                    self.pc[t] = 1;
                },
                1 => {
                    const other = 1 - self.cand[t];
                    self.busy_seen[t] = if (weakened) false else self.claimed[other];
                    self.pc[t] = 2;
                },
                2 => {
                    if (!self.busy_seen[t] and !self.claimed[self.cand[t]]) {
                        self.claimed[self.cand[t]] = true;
                        self.holds[t] = self.cand[t];
                    }
                    self.pc[t] = 3;
                },
                3 => {
                    self.table_lock = false;
                    self.pc[t] = 5;
                },
                else => {},
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.claimed[0] and self.claimed[1]) return error.TwoMoversOnOneVolume;
        }

        pub fn final(self: *const Self) !void {
            try self.invariant();
            // Progress: at least one worker claimed.
            if (self.holds[0] == null and self.holds[1] == null) return error.NobodyClaimed;
        }
    };
}

test "claim word: with the scope check under the table lock never two claimed movers on one volume; dropping the scope check admits the double claim" {
    try explorer.expectHolds(ClaimModel(false), .{});
    try explorer.expectFails(ClaimModel(true), .{}, error.InvariantViolated);
}

// ---------------------------------------------------------------------------
// 2. The park/resume Dekker pair.
// ---------------------------------------------------------------------------

const State = enum(u8) { running, paused, queued };

fn ParkModel(comptime weakened: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        state: State = .running,
        intent: bool = true, // the operator already asked for the pause
        lock: bool = false,
        pc: [2]u8 = .{ 0, 0 },
        /// The worker's snapshot of the intent at its re-check.
        saw_intent: bool = true,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= 10;
        }

        // Thread 0 (the worker), the correct order:
        //   0: lock; 1: state = paused (the durable checkpoint); 2: unlock;
        //   3: lock (settle); 4: read intent; 5: if !intent ⇒ state = running
        //      (resumed under the same claim) else leave paused; 6: unlock; done.
        // Weakened: the checkpoint flip happens AFTER the re-check —
        //   0: lock; 1: read intent; 2: unlock; 3: lock; 4: if intent ⇒ state =
        //      paused (stale) else state = running; 5: unlock.
        // Thread 1 (resume): 0: lock; 1: intent = false; 2: if state == paused
        //   ⇒ state = queued; 3: unlock; done.
        pub fn step(self: *Self, t: usize) void {
            if (t == 1) {
                switch (self.pc[1]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[1] = 1;
                    },
                    1 => {
                        self.intent = false;
                        self.pc[1] = 2;
                    },
                    2 => {
                        if (self.state == .paused) self.state = .queued;
                        self.pc[1] = 3;
                    },
                    3 => {
                        self.lock = false;
                        self.pc[1] = 10;
                    },
                    else => {},
                }
                return;
            }
            if (!weakened) {
                switch (self.pc[0]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        self.state = .paused;
                        self.pc[0] = 2;
                    },
                    2 => {
                        self.lock = false;
                        self.pc[0] = 3;
                    },
                    3 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 4;
                    },
                    4 => {
                        self.saw_intent = self.intent;
                        self.pc[0] = 5;
                    },
                    5 => {
                        if (!self.saw_intent) self.state = .running;
                        self.pc[0] = 6;
                    },
                    6 => {
                        self.lock = false;
                        self.pc[0] = 10;
                    },
                    else => {},
                }
            } else {
                switch (self.pc[0]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        self.saw_intent = self.intent;
                        self.pc[0] = 2;
                    },
                    2 => {
                        self.lock = false;
                        self.pc[0] = 3;
                    },
                    3 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 4;
                    },
                    4 => {
                        self.state = if (self.saw_intent) .paused else .running;
                        self.pc[0] = 5;
                    },
                    5 => {
                        self.lock = false;
                        self.pc[0] = 10;
                    },
                    else => {},
                }
            }
        }

        pub fn invariant(_: *const Self) !void {}

        /// The stranded job: nobody intends the pause, yet the state is
        /// Paused — unclaimable forever.
        pub fn final(self: *const Self) !void {
            if (!self.intent and self.state == .paused) return error.StrandedPaused;
            if (self.lock) return error.LockLeaked;
        }
    };
}

test "park/resume Dekker: checkpoint-then-settle under the state lock never strands a job; flipping Paused after the re-check admits intent = false ∧ state = Paused" {
    try explorer.expectHolds(ParkModel(false), .{});
    try explorer.expectFails(ParkModel(true), .{}, error.FinalViolated);
}

// ---------------------------------------------------------------------------
// 3. The shard fencing pair.
// ---------------------------------------------------------------------------

fn FencingModel(comptime weakened: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;

        /// The table's fencing word and lock.
        fencing: u8 = 0,
        lock: bool = false,
        pc: [2]u8 = .{ 0, 0 },
        /// The zombie's compare result (its submit carries fencing 0).
        compare_ok: bool = false,
        /// A publish issued while the table's fencing had already moved.
        stale_publish: bool = false,
        published: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= 10;
        }

        // Thread 0 (expiry): 0: lock; 1: fencing += 1 (the bump + the
        //   re-lease); 2: unlock.
        // Thread 1 (the late submit at fencing 0), correct: 0: lock;
        //   1: compare fencing == 0; 2: publish if ok; 3: unlock — the
        //   compare and the publish are ONE locked section.
        // Weakened: 0: compare (outside the lock); 1: lock; 2: publish if
        //   ok; 3: unlock — the bump may land between the compare and the
        //   publish.
        pub fn step(self: *Self, t: usize) void {
            if (t == 0) {
                switch (self.pc[0]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[0] = 1;
                    },
                    1 => {
                        self.fencing += 1;
                        self.pc[0] = 2;
                    },
                    2 => {
                        self.lock = false;
                        self.pc[0] = 10;
                    },
                    else => {},
                }
                return;
            }
            switch (self.pc[1]) {
                0 => {
                    if (weakened) {
                        self.compare_ok = self.fencing == 0;
                        self.pc[1] = 1;
                    } else if (!self.lock) {
                        self.lock = true;
                        self.pc[1] = 1;
                    }
                },
                1 => {
                    if (weakened) {
                        if (!self.lock) {
                            self.lock = true;
                            self.pc[1] = 2;
                        }
                    } else {
                        self.compare_ok = self.fencing == 0;
                        self.pc[1] = 2;
                    }
                },
                2 => {
                    if (self.compare_ok) {
                        self.published = true;
                        if (self.fencing != 0) self.stale_publish = true;
                    }
                    self.pc[1] = 3;
                },
                3 => {
                    self.lock = false;
                    self.pc[1] = 10;
                },
                else => {},
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.stale_publish) return error.StalePublish;
        }

        pub fn final(self: *const Self) !void {
            try self.invariant();
            if (self.lock) return error.LockLeaked;
        }
    };
}

test "shard fencing pair: compare-and-publish under the table lock never publishes a stale fencing; comparing outside the lock (before the bump lands) admits the zombie's publish" {
    try explorer.expectHolds(FencingModel(false), .{});
    try explorer.expectFails(FencingModel(true), .{}, error.InvariantViolated);
}
