//! Explorer models of the conveyor handoff core (`conveyor_core.zig`;
//! survey §3.1; the loom models `conveyor_*` in the reference's
//! `loom-models/src/lib.rs`).
//!
//! Plain value types. The queue mutex is a flag with explicit acquire
//! (a spin that is a no-op step the explorer prunes) and release steps; the
//! leadership word is a `bool`; the ring is four slots. Every `step` is ONE
//! atomic action: a lock acquire/release, one lock-protected queue
//! mutation, one CAS attempt, one store, one load. The pass loop
//! (`drain → empty ⇒ unlead-then-recheck → re-elect or exit`) and the
//! committer's two uninterruptible steps (`enqueue`, `tryLead`) are
//! sub-machines shared by the six models.
//!
//! Weakening ledger (which weakening fails which model):
//!   1 leader_unique_and_loser_entry_drained     — `tryLead` as load-then-store
//!   2 no_lost_wakeup_across_unlead              — recheck BEFORE release; the unlead store
//!                                                 not seq_cst (store-buffered past the recheck)
//!   3 fifo_apply_order_under_race               — enqueue's push outside the queue mutex
//!   4 budget_conserved_across_committer_drop    — a dropped committer retracts its entry
//!   5 guard_outlives_dropped_committer          — the queue entry does not co-own the guard
//!   6 group_never_drains_partially              — `enqueueMany` takes the mutex per item

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = enum {
    none,
    /// `tryLead` as a load of the leader word then a store (the CAS split).
    split_try_lead,
    /// `unleadAndRecheck` re-checks the queue BEFORE releasing leadership.
    recheck_before_release,
    /// The unlead store is release, not seq_cst: it sits in the store buffer
    /// while the recheck runs and lands after it (Dekker's shape).
    unlead_store_not_seq_cst,
    /// Enqueue's push does not hold the queue mutex (a split length RMW).
    push_outside_mutex,
    /// A committer that disappears retracts its still-queued entry.
    retract_on_drop,
    /// The queue entry holds no reference to the guard set.
    no_entry_coown,
    /// `enqueueMany` acquires the mutex once per item.
    group_per_item_lock,
};

const Item = struct { id: u8, len: u8 };
/// Ring capacity: the largest population any model queues is 3 (+1 slack
/// so a wrap is exercised).
const cap = 4;

/// The shared words of one core, as plain fields.
const Shared = struct {
    /// The queue mutex.
    locked: bool = false,
    /// The leadership word.
    leader: bool = false,
    /// The leader word's store buffer (`.unlead_store_not_seq_cst` only).
    pending_unlead: bool = false,
    /// `.spawn`: the detached pass task exists (set by the elected committer).
    spawned: bool = false,
    slots: [cap]Item = @splat(.{ .id = 0, .len = 0 }),
    head: u8 = 0,
    len: u8 = 0,

    fn push(self: *Shared, item: Item) void {
        self.slots[(self.head + self.len) % cap] = item;
        self.len += 1;
    }
    fn pop(self: *Shared) Item {
        const it = self.slots[self.head];
        self.head = (self.head + 1) % cap;
        self.len -= 1;
        return it;
    }
    /// Remove a queued entry by id (the `.retract_on_drop` weakening).
    fn removeId(self: *Shared, id: u8) void {
        var kept: [cap]Item = undefined;
        var n: u8 = 0;
        while (self.len > 0) {
            const it = self.pop();
            if (it.id != id) {
                kept[n] = it;
                n += 1;
            }
        }
        self.head = 0;
        for (kept[0..n]) |it| self.push(it);
    }
};

/// The pass loop: drain until empty, then unlead-and-recheck; exit on an
/// empty recheck or a lost re-election, loop on a won one.
const Pass = struct {
    pc: Pc = .drain_lock,
    /// This task believes it holds leadership.
    leading: bool,
    drained: u8 = 0,
    budget: u8 = 0,
    /// Terminal outcomes delivered (fan-outs of non-empty batches).
    fanned: u8 = 0,
    /// The last batch's ids, for the FIFO/group assertions.
    batch: [cap]u8 = @splat(0),
    batch_n: u8 = 0,
    saw_empty: bool = false,

    const Pc = enum(u8) {
        drain_lock,
        drain_pop,
        drain_unlock,
        fan_out,
        unlead_store,
        unlead_lock,
        unlead_check,
        unlead_unlock,
        unlead_land,
        unlead_cas,
        done,
    };

    fn step(self: *Pass, sh: *Shared, comptime weakening: Weakening) void {
        switch (self.pc) {
            .drain_lock => if (!sh.locked) {
                sh.locked = true;
                self.pc = .drain_pop;
            },
            .drain_pop => {
                // One lock-protected pop of the whole batch (caps ≥ the ring).
                self.batch_n = 0;
                while (sh.len > 0) {
                    const it = sh.pop();
                    self.batch[self.batch_n] = it.id;
                    self.batch_n += 1;
                    self.budget += it.len;
                }
                self.drained += self.batch_n;
                self.pc = .drain_unlock;
            },
            .drain_unlock => {
                sh.locked = false;
                self.pc = if (self.batch_n > 0) .fan_out else if (weakening == .recheck_before_release) .unlead_lock else .unlead_store;
            },
            .fan_out => {
                // The batch's terminal outcomes: guards drop, acks go out.
                self.fanned += self.batch_n;
                self.pc = .drain_lock;
            },
            .unlead_store => {
                // leader.store(false, seq_cst) — or, weakened, a buffered store.
                if (weakening == .unlead_store_not_seq_cst) sh.pending_unlead = true else sh.leader = false;
                self.leading = false;
                self.pc = if (weakening == .recheck_before_release) .done else .unlead_lock;
            },
            .unlead_lock => if (!sh.locked) {
                sh.locked = true;
                self.pc = .unlead_check;
            },
            .unlead_check => {
                self.saw_empty = sh.len == 0;
                self.pc = .unlead_unlock;
            },
            .unlead_unlock => {
                sh.locked = false;
                self.pc = switch (weakening) {
                    .recheck_before_release => if (self.saw_empty) .unlead_store else .drain_lock,
                    .unlead_store_not_seq_cst => .unlead_land,
                    else => if (self.saw_empty) .done else .unlead_cas,
                };
            },
            .unlead_land => {
                // The buffered store lands AFTER the recheck.
                sh.leader = false;
                sh.pending_unlead = false;
                self.pc = if (self.saw_empty) .done else .unlead_cas;
            },
            .unlead_cas => {
                if (!sh.leader) {
                    sh.leader = true;
                    self.leading = true;
                    self.pc = .drain_lock;
                } else {
                    // A racing enqueuer's tryLead won and owns the drain.
                    self.pc = .done;
                }
            },
            .done => unreachable,
        }
    }
};

/// `enqueue`/`enqueueMany`: the pushes under one lock hold (or, weakened,
/// one hold per item, or no hold at all).
const Enqueue = struct {
    pc: Pc,
    pushed: u8 = 0,
    tmp_len: u8 = 0,

    const Pc = enum(u8) { lock, push, unlock, outside_load, outside_slot, outside_len, done };

    fn start(comptime weakening: Weakening) Enqueue {
        return .{ .pc = if (weakening == .push_outside_mutex) .outside_load else .lock };
    }

    fn step(self: *Enqueue, sh: *Shared, items: []const Item, comptime weakening: Weakening) void {
        switch (self.pc) {
            .lock => if (!sh.locked) {
                sh.locked = true;
                self.pc = .push;
            },
            .push => {
                sh.push(items[self.pushed]);
                self.pushed += 1;
                if (self.pushed == items.len or weakening == .group_per_item_lock) self.pc = .unlock;
            },
            .unlock => {
                sh.locked = false;
                self.pc = if (self.pushed < items.len) .lock else .done;
            },
            // The unlocked push: load the length, write the slot, store the length.
            .outside_load => {
                self.tmp_len = sh.len;
                self.pc = .outside_slot;
            },
            .outside_slot => {
                sh.slots[(sh.head + self.tmp_len) % cap] = items[self.pushed];
                self.pc = .outside_len;
            },
            .outside_len => {
                sh.len = self.tmp_len + 1;
                self.pushed += 1;
                self.pc = if (self.pushed < items.len) .outside_load else .done;
            },
            .done => unreachable,
        }
    }
};

/// `tryLead`: one CAS (or, weakened, a load then a store).
const Elect = struct {
    pc: Pc,
    tmp: bool = false,
    won: bool = false,

    const Pc = enum(u8) { cas, load, store, done };

    fn start(comptime weakening: Weakening) Elect {
        return .{ .pc = if (weakening == .split_try_lead) .load else .cas };
    }

    fn step(self: *Elect, sh: *Shared) void {
        switch (self.pc) {
            .cas => {
                if (!sh.leader) {
                    sh.leader = true;
                    self.won = true;
                }
                self.pc = .done;
            },
            .load => {
                self.tmp = sh.leader;
                self.pc = .store;
            },
            .store => {
                if (!self.tmp) {
                    sh.leader = true;
                    self.won = true;
                }
                self.pc = .done;
            },
            .done => unreachable,
        }
    }
};

/// A committer: enqueue, elect, and — if it won — run the pass (or stop and
/// let the model spawn a detached pass).
const Committer = struct {
    phase: Phase = .enqueue,
    enq: Enqueue,
    elect: Elect,
    pass: Pass = .{ .leading = false },

    const Phase = enum(u8) { enqueue, elect, pass, done };

    fn start(comptime weakening: Weakening) Committer {
        return .{ .enq = .start(weakening), .elect = .start(weakening) };
    }

    fn step(self: *Committer, sh: *Shared, items: []const Item, comptime weakening: Weakening, comptime pass_if_won: bool) void {
        switch (self.phase) {
            .enqueue => {
                self.enq.step(sh, items, weakening);
                if (self.enq.pc == .done) self.phase = .elect;
            },
            .elect => {
                self.elect.step(sh);
                if (self.elect.pc == .done) {
                    if (self.elect.won and pass_if_won) {
                        self.pass.leading = true;
                        self.phase = .pass;
                    } else self.phase = .done;
                }
            },
            .pass => {
                self.pass.step(sh, weakening);
                if (self.pass.pc == .done) self.phase = .done;
            },
            .done => unreachable,
        }
    }
};

// ---------------------------------------------------------------------------
// 1. leader uniqueness
// ---------------------------------------------------------------------------

/// Two committers race enqueue+elect: exactly one holds leadership at any
/// instant, and both entries drain exactly once — by the winner's pass, or
/// by the loser's own later election after the winner unleads.
///
/// Weakening `.split_try_lead`: both load `leader == false`, both store
/// true — two leaders.
fn LeaderUnique(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const items = [2][1]Item{ .{.{ .id = 1, .len = 1 }}, .{.{ .id = 2, .len = 1 }} };

        sh: Shared = .{},
        c: [2]Committer = @splat(.start(weakening)),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.c[t].phase == .done;
        }
        pub fn step(self: *Self, t: usize) void {
            self.c[t].step(&self.sh, &items[t], weakening, true);
        }
        pub fn invariant(self: *const Self) !void {
            if (self.c[0].pass.leading and self.c[1].pass.leading) return error.TwoLeaders;
        }
        pub fn final(self: *const Self) !void {
            if (!self.c[0].elect.won and !self.c[1].elect.won) return error.NobodyLed;
            if (self.c[0].pass.drained + self.c[1].pass.drained != 2) return error.EntriesNotDrainedExactlyOnce;
            if (self.sh.len != 0) return error.EntryLeftQueued;
            if (self.sh.leader) return error.LeadershipNotReleased;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. no lost wakeup
// ---------------------------------------------------------------------------

/// A live leader with an empty queue retires (empty drain → unlead →
/// recheck) while a committer enqueues and elects: the entry is drained
/// exactly once — by the leader's re-election or by the committer's own
/// pass. Both declining is the lost wakeup.
///
/// Weakenings: `.recheck_before_release` — the leader sees an empty queue,
/// the committer enqueues and loses the CAS to the still-live leader, the
/// leader releases and exits; `.unlead_store_not_seq_cst` — the leader's
/// release sits in its store buffer while it rechecks an empty queue, the
/// committer enqueues and loses the CAS to the stale `true`, the store
/// lands, the leader exits.
fn NoLostWakeup(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const seeded = 0;
        const committer = 1;
        const items = [1]Item{.{ .id = 7, .len = 1 }};

        sh: Shared = .{ .leader = true },
        pass: Pass = .{ .leading = true },
        c: Committer = .start(weakening),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                seeded => self.pass.pc == .done,
                committer => self.c.phase == .done,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                seeded => self.pass.step(&self.sh, weakening),
                committer => self.c.step(&self.sh, &items, weakening, true),
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.pass.leading and self.c.pass.leading) return error.TwoLeaders;
        }
        pub fn final(self: *const Self) !void {
            const total = self.pass.drained + self.c.pass.drained;
            if (total == 0) return error.LostWakeup;
            if (total > 1) return error.DoubleDrain;
            if (self.sh.len != 0) return error.EntryLeftQueued;
        }
    };
}

// ---------------------------------------------------------------------------
// 3. FIFO under a racing producer
// ---------------------------------------------------------------------------

/// Entries drain in enqueue order even when the drain races a producer, and
/// a capped drain takes a strict prefix: with [1, 2] queued and 3 arriving,
/// `drain(2)` is exactly [1, 2] and the drain after the producer finishes is
/// exactly [3].
///
/// Weakening `.push_outside_mutex`: the producer's length update is a split
/// RMW racing the drain's pop — the racing entry is lost or a phantom slot
/// appears.
fn FifoUnderRace(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const producer = 0;
        const drainer = 1;
        const items = [1]Item{.{ .id = 3, .len = 1 }};

        sh: Shared,
        enq: Enqueue = .start(weakening),
        pc: u8 = 0,
        first: [cap]u8 = @splat(0),
        first_n: u8 = 0,
        rest: [cap]u8 = @splat(0),
        rest_n: u8 = 0,

        pub fn init() Self {
            var self: Self = .{ .sh = .{} };
            self.sh.push(.{ .id = 1, .len = 1 });
            self.sh.push(.{ .id = 2, .len = 1 });
            return self;
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                producer => self.enq.pc == .done,
                drainer => self.pc == 7,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                producer => self.enq.step(&self.sh, &items, weakening),
                drainer => switch (self.pc) {
                    0, 4 => if (!self.sh.locked) {
                        self.sh.locked = true;
                        self.pc += 1;
                    },
                    1 => {
                        // drain(max_txs = 2): a strict FIFO prefix.
                        while (self.first_n < 2 and self.sh.len > 0) {
                            self.first[self.first_n] = self.sh.pop().id;
                            self.first_n += 1;
                        }
                        self.pc = 2;
                    },
                    2, 6 => {
                        self.sh.locked = false;
                        self.pc += 1;
                    },
                    // join the producer: proceeds only once it has finished.
                    3 => if (self.enq.pc == .done) {
                        self.pc = 4;
                    },
                    5 => {
                        while (self.sh.len > 0) {
                            self.rest[self.rest_n] = self.sh.pop().id;
                            self.rest_n += 1;
                        }
                        self.pc = 6;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.pc >= 2 and !(self.first_n == 2 and self.first[0] == 1 and self.first[1] == 2)) return error.PrefixNotFifo;
        }
        pub fn final(self: *const Self) !void {
            if (!(self.rest_n == 1 and self.rest[0] == 3)) return error.RacingEntryNotDrainedAfterPrefix;
        }
    };
}

// ---------------------------------------------------------------------------
// 4. budget conservation across a committer drop
// ---------------------------------------------------------------------------

/// Committer A enqueues 3 budget bytes and DIES right after electing (the
/// dropped committer); B enqueues 5 and does the same. A winner spawns the
/// detached pass task (a later winner, after that pass retired, spawns the
/// next one); the passes must see every byte: drained exactly twice, budget
/// exactly 8, nothing queued, nobody leading at the end.
///
/// Weakening `.retract_on_drop`: the dying committer removes its own
/// still-queued entry — its bytes never reach a pass.
fn BudgetConserved(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const detached = 2;
        const items = [2][1]Item{ .{.{ .id = 10, .len = 3 }}, .{.{ .id = 20, .len = 5 }} };

        sh: Shared = .{},
        c: [2]Committer = @splat(.start(weakening)),
        /// Post-election program of each committer: 0 spawn/skip, 1 die, 2 dead.
        after: [2]u8 = @splat(0),
        /// The detached pass task (one thread stands in for every spawn;
        /// a spawn restarts it with the totals kept).
        pass: Pass = .{ .leading = true, .pc = .done },

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0, 1 => self.after[t] == 2,
                detached => self.pass.pc == .done,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0, 1 => if (self.c[t].phase != .done) {
                    self.c[t].step(&self.sh, &items[t], weakening, false);
                } else switch (self.after[t]) {
                    0 => {
                        // The winner spawns the detached pass task.
                        if (self.c[t].elect.won) {
                            self.pass.pc = .drain_lock;
                            self.pass.leading = true;
                        }
                        self.after[t] = 1;
                    },
                    1 => {
                        // The committer dies: its result channel drops, nothing else.
                        if (weakening == .retract_on_drop) self.sh.removeId(items[t][0].id);
                        self.after[t] = 2;
                    },
                    else => unreachable,
                },
                detached => self.pass.step(&self.sh, weakening),
                else => unreachable,
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (!self.c[0].elect.won and !self.c[1].elect.won) return error.NobodyLed;
            if (self.pass.drained != 2) return error.EntriesNotDrainedExactlyOnce;
            if (self.pass.budget != 8) return error.BudgetNotConserved;
            if (self.sh.len != 0) return error.EntryLeftQueued;
            if (self.sh.leader) return error.LeadershipNotReleased;
        }
    };
}

// ---------------------------------------------------------------------------
// 5. guard lifetime ≥ staged-record lifetime
// ---------------------------------------------------------------------------

/// The committer's guard set is co-owned by its queue entry: the committer
/// enqueues, elects (the sole elector wins), spawns the pass and DIES — its
/// own reference drops — yet the guard stays held until the pass delivers
/// the entry's terminal outcome (the fan-out), and is released then.
///
/// Weakening `.no_entry_coown`: the entry holds no reference, so the
/// committer's death releases the guard while its records sit queued — the
/// same-key exclusion is lost.
fn GuardOutlives(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const committer = 0;
        const detached = 1;
        const items = [1]Item{.{ .id = 1, .len = 1 }};

        sh: Shared = .{},
        c: Committer = .start(weakening),
        after: u8 = 0,
        /// The committer frame's own reference is alive.
        committer_ref: bool = true,
        pass: Pass = .{ .leading = true },

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                committer => self.after == 2,
                detached => self.pass.pc == .done,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer => if (self.c.phase != .done) {
                    self.c.step(&self.sh, &items, weakening, false);
                } else switch (self.after) {
                    0 => {
                        self.sh.spawned = true;
                        self.after = 1;
                    },
                    1 => {
                        self.committer_ref = false;
                        self.after = 2;
                    },
                    else => unreachable,
                },
                detached => if (self.sh.spawned) self.pass.step(&self.sh, weakening),
                else => unreachable,
            }
        }
        /// The entry's reference lives from its construction to its terminal outcome.
        fn entryRef(self: *const Self) bool {
            return weakening != .no_entry_coown and self.pass.fanned == 0;
        }
        fn released(self: *const Self) bool {
            return !self.committer_ref and !self.entryRef();
        }
        pub fn invariant(self: *const Self) !void {
            if (self.released() and self.pass.fanned == 0) return error.GuardReleasedBeforeTerminalOutcome;
        }
        pub fn final(self: *const Self) !void {
            if (!self.c.elect.won) return error.SoleElectorLost;
            if (self.pass.fanned != 1) return error.NoTerminalOutcome;
            if (!self.released()) return error.GuardLeaked;
        }
    };
}

// ---------------------------------------------------------------------------
// 6. group atomicity
// ---------------------------------------------------------------------------

/// A group enqueued through `enqueueMany` is atomic with respect to the
/// drain: a retiring leader (empty drain → unlead → recheck) and the group
/// committer's election race, and every non-empty batch either pass drains
/// is the WHOLE group [1, 2, 3] in order, exactly once.
///
/// Weakening `.group_per_item_lock`: the leader's drain slips between two
/// of the group's lock holds and returns [1] or [1, 2].
fn GroupAtomic(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const seeded = 0;
        const committer = 1;
        const group = [3]Item{ .{ .id = 1, .len = 1 }, .{ .id = 2, .len = 1 }, .{ .id = 3, .len = 1 } };

        sh: Shared = .{ .leader = true },
        pass: Pass = .{ .leading = true },
        c: Committer = .start(weakening),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                seeded => self.pass.pc == .done,
                committer => self.c.phase == .done,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                seeded => self.pass.step(&self.sh, weakening),
                committer => self.c.step(&self.sh, &group, weakening, true),
                else => unreachable,
            }
        }
        fn wholeGroup(p: *const Pass) bool {
            return p.batch_n == 0 or (p.batch_n == 3 and p.batch[0] == 1 and p.batch[1] == 2 and p.batch[2] == 3);
        }
        pub fn invariant(self: *const Self) !void {
            if (!wholeGroup(&self.pass) or !wholeGroup(&self.c.pass)) return error.PartialGroupDrained;
        }
        pub fn final(self: *const Self) !void {
            const total = self.pass.drained + self.c.pass.drained;
            if (total == 0) return error.LostWakeup;
            if (total != 3) return error.GroupNotDrainedExactlyOnce;
            if (self.sh.len != 0) return error.EntryLeftQueued;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "conveyor_leader_unique_and_loser_entry_drained: holds; tryLead as load-then-store fails" {
    try explorer.expectHolds(LeaderUnique(.none), .{});
    try explorer.expectFails(LeaderUnique(.split_try_lead), .{}, error.InvariantViolated);
}

test "conveyor_no_lost_wakeup_across_unlead: holds; recheck-before-release and a non-seq_cst unlead store fail" {
    try explorer.expectHolds(NoLostWakeup(.none), .{});
    try explorer.expectFails(NoLostWakeup(.recheck_before_release), .{}, error.FinalViolated);
    try explorer.expectFails(NoLostWakeup(.unlead_store_not_seq_cst), .{}, error.FinalViolated);
}

test "conveyor_fifo_apply_order_under_race: holds; a push outside the queue mutex fails" {
    try explorer.expectHolds(FifoUnderRace(.none), .{});
    try explorer.expectFails(FifoUnderRace(.push_outside_mutex), .{}, error.FinalViolated);
}

test "conveyor_budget_conserved_across_committer_drop: holds; retract-on-drop fails" {
    try explorer.expectHolds(BudgetConserved(.none), .{});
    try explorer.expectFails(BudgetConserved(.retract_on_drop), .{}, error.FinalViolated);
}

test "conveyor_guard_outlives_dropped_committer_until_terminal: holds; no entry co-ownership fails" {
    try explorer.expectHolds(GuardOutlives(.none), .{});
    try explorer.expectFails(GuardOutlives(.no_entry_coown), .{}, error.InvariantViolated);
}

test "conveyor_group_never_drains_partially: holds; a per-item lock hold fails" {
    try explorer.expectHolds(GroupAtomic(.none), .{});
    try explorer.expectFails(GroupAtomic(.group_per_item_lock), .{}, error.InvariantViolated);
}

// The state counts the evidence note records.
test "conveyor models: exhaustive within the explorer's default limits" {
    inline for (.{ LeaderUnique(.none), NoLostWakeup(.none), FifoUnderRace(.none), BudgetConserved(.none), GuardOutlives(.none), GroupAtomic(.none) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 5000);
    }
}
