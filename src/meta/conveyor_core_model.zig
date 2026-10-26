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
//!   7 pass_fiber_wake_not_lost                   — the pass fiber parks without re-reading the
//!                                                 leader latch (the winner's wake coalesced into
//!                                                 the fiber's own mutex-park wake)
//!   8 hold_park_never_outlives_the_landing        — the window-fill hold reads the in-flight
//!                                                 count BEFORE registering on the landing notify
//!                                                 (the landing between the read and the park is
//!                                                 lost: the pass parks on an empty flight), with
//!                                                 one and with two windows in flight
//!   9 window_gate_never_meets_a_full_lane         — the gate admits `window_cells` windows (the hook →
//!                                                 give-back gap forgotten); no gate (the joined
//!                                                 appender's pass panic); a barrier taking a window
//!                                                 cell; the gate checking before it registers; the
//!                                                 lane notifying before it untracks (both: the
//!                                                 parked pass never wakes)
//!  10 room_park_never_strands_a_committer         — the committer counts itself after its failed
//!                                                 re-try; it re-tries before it registers on
//!                                                 `room`; the pass reads the count before its
//!                                                 drain frees the cell (each: parked, the room
//!                                                 free, no wake owed)
//!  11 fsync_inbox_loses_no_fsync_and_covers_its_window — the push as a load then a store (two
//!                                                 pushers: one node lost); the lane scanning the
//!                                                 inbound windows BEFORE it takes the inbox (an
//!                                                 fsync admitted without its submitter's window
//!                                                 in its cover)
//!  12 leafwise_destroy_never_waits_holding_a_leaf  — the destroy takes its leaf set as a union
//!                                                 (every leaf locked ascending, held across the
//!                                                 apply): a committer on its first leaf waits
//!                                                 while the destroy waits for its second

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
    /// The pass FIBER parks after a pass without re-reading the leader
    /// latch: a winner's wake that landed while the fiber was SCHEDULED
    /// (woken from its park on the queue mutex) was a no-op on the word.
    park_without_leader_recheck,
    /// The window-fill hold checks `windows_inflight` BEFORE it registers
    /// on the landing notify (check-then-register): a landing between the
    /// two is a notify with no waiter, and the pass parks on a flight that
    /// is already empty.
    hold_check_before_register,
    /// The window gate admits while fewer than `window_cells` windows are
    /// tracked: the cell an untracked window still holds between its hook
    /// and its give-back is not counted.
    window_gate_at_cells,
    /// No window gate: the ring's admission was trusted to bound the lane
    /// (it does not — a sub-page grain, a grown joined ring).
    no_window_gate,
    /// A barrier request takes its cell from the windows' pool.
    requests_share_window_cells,
    /// The gate reads the flight BEFORE it registers on the landing notify.
    window_gate_check_before_register,
    /// The window's terminal outcome notifies BEFORE it untracks.
    notify_before_untrack,
    /// A committer on a full queue counts itself in `room_waiters` only
    /// after its failed re-try.
    room_count_after_try,
    /// The committer re-tries BEFORE it registers on `room`.
    room_check_before_register,
    /// The pass reads `room_waiters` BEFORE its drain frees the cell.
    room_read_before_drain,
    /// The fsync inbox push is a load of the head then a store.
    inbox_push_not_cas,
    /// The lane's scan drains the inbound windows BEFORE it takes the
    /// fsync inbox.
    fsyncs_after_inbound_scan,
    /// A leafwise member (a corpse's destroy) locks its whole leaf set
    /// ascending and holds it across its apply, as a name's change does.
    destroy_union_hold,
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
    /// `.parkable`: a lock step that finds the mutex held PARKS the fiber
    /// (the production `std.Io.Mutex`); the model's fiber word records it.
    /// Set by `PassFiber`; the committer-run passes spin.
    parked_on_mutex: bool = false,
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
// 7. the pass fiber's wake is never lost
// ---------------------------------------------------------------------------

/// The production venue (`backend.passEntry`): ONE long-lived pass fiber,
/// woken by the election winner through the task-state word (IDLE →
/// SCHEDULED enqueues; RUNNING → NOTIFIED; SCHEDULED/NOTIFIED absorb), that
/// parks on the queue mutex inside `unleadAndRecheck`'s `pending()` under
/// contention. Two committers enqueue and elect; the fiber starts parked.
///
/// Weakening `.park_without_leader_recheck` (the 2026-10-10 wedge: 16
/// committers parked, no pass, every thread asleep): the fiber releases
/// leadership, parks on the mutex a committer holds; the committer's unlock
/// wakes it (IDLE → SCHEDULED), the committer's `tryLead` wins and its wake
/// is a no-op on SCHEDULED; the fiber resumes, re-checks, loses its own
/// `tryLead` to the committer, exits the pass and PARKS — the latch is
/// held, the entry queued, nobody runs. The fix: the fiber re-reads the
/// leader latch before every park (a wake arriving between the read and
/// the park finds it RUNNING and lands as NOTIFIED — law 1 — so the park
/// returns at once).
fn PassFiber(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const fiber = 0;
        const items = [2][1]Item{ .{.{ .id = 1, .len = 1 }}, .{.{ .id = 2, .len = 1 }} };

        const Word = enum(u8) { idle, scheduled, running, notified };
        const Fpc = enum(u8) { parked, run_check, in_pass, before_park, park };

        sh: Shared = .{},
        word: Word = .idle,
        fpc: Fpc = .parked,
        pass: Pass = .{ .leading = false },
        drained: u8 = 0,
        c: [2]Committer = @splat(.start(weakening)),
        woke: [2]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }

        /// `f.state.wake()` + the enqueue: the ONE wake primitive every
        /// producer uses (the election winner, the mutex's unlock).
        fn wake(self: *Self) void {
            self.word = switch (self.word) {
                .idle => .scheduled,
                .running => .notified,
                .scheduled, .notified => self.word,
            };
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                fiber => self.fpc == .parked and self.word == .idle and self.c[0].phase == .done and self.c[1].phase == .done,
                else => self.c[t - 1].phase == .done,
            };
        }

        fn stepFiber(self: *Self) void {
            switch (self.word) {
                .idle => return, // not runnable
                .scheduled => {
                    // The lane runs it: resume where it parked.
                    self.word = .running;
                    if (self.fpc == .parked) self.fpc = .run_check;
                    return;
                },
                .running, .notified => {},
            }
            switch (self.fpc) {
                .parked => unreachable,
                .run_check => {
                    // `runScheduledPass`: the pass iff the latch is set.
                    if (self.sh.leader) {
                        self.pass = .{ .leading = true };
                        self.fpc = .in_pass;
                    } else self.fpc = .before_park;
                },
                .in_pass => {
                    // A lock step on a held mutex parks the fiber (the
                    // unlocker wakes it); every other step is the pass's.
                    const wants_lock = self.pass.pc == .drain_lock or self.pass.pc == .unlead_lock;
                    if (wants_lock and self.sh.locked) {
                        self.pass.parked_on_mutex = true;
                        self.word = .idle;
                        return;
                    }
                    self.pass.step(&self.sh, weakening);
                    if (self.pass.pc == .done) {
                        self.drained += self.pass.drained;
                        self.fpc = .before_park;
                    }
                },
                .before_park => {
                    self.fpc = if (weakening != .park_without_leader_recheck and self.sh.leader) .run_check else .park;
                },
                .park => {
                    // `f.park()`: NOTIFIED requeues (law 1), RUNNING parks.
                    if (self.word == .notified) {
                        self.word = .scheduled;
                        self.fpc = .parked;
                    } else {
                        self.word = .idle;
                        self.fpc = .parked;
                    }
                },
            }
        }

        fn stepCommitter(self: *Self, i: usize) void {
            const c = &self.c[i];
            const was_locked = self.sh.locked;
            c.step(&self.sh, &items[i], weakening, false);
            // The mutex's unlock wakes the fiber parked on it.
            if (was_locked and !self.sh.locked and self.pass.parked_on_mutex) {
                self.pass.parked_on_mutex = false;
                self.wake();
            }
            // The winner's ONE wake, right after its CAS.
            if (c.elect.pc == .done and c.elect.won and !self.woke[i]) {
                self.woke[i] = true;
                self.wake();
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                fiber => self.stepFiber(),
                else => self.stepCommitter(t - 1),
            }
        }
        pub fn invariant(_: *const Self) !void {}
        pub fn final(self: *const Self) !void {
            if (self.sh.len != 0) return error.EntryLeftQueued;
            if (self.drained != 2) return error.EntriesNotDrainedExactlyOnce;
            if (self.sh.leader) return error.LeadershipNotReleased;
        }
    };
}

// ---------------------------------------------------------------------------
// 8. the window-fill hold never parks on an empty flight
// ---------------------------------------------------------------------------

/// The pass has drained a pad-majority batch while `inflight0` windows
/// are in flight (`conveyor.holdForFill`): it registers on the landing
/// notify (snapshots the epoch), re-reads the in-flight count, and parks
/// iff it is still non-zero; a landing (the lane's `finishWindow`:
/// `inflight −= 1` then `notifyWaiters`: `epoch += 1`, waking a registered
/// waiter) ends the park and the pass registers and re-reads AGAIN —
/// until the flight is empty — after which it re-drains, issues, and
/// runs on as the leader (the `Pass` loop: drain → unlead → recheck). A
/// committer enqueues during the hold and loses its election to the
/// holding pass (or, enqueuing after the unlead, wins and drains itself):
/// its entry is drained exactly once — by the re-drain into the held
/// window, by the leader's next drain, or by its own pass. The deadline is
/// not modelled: it is the latency bound, never the liveness backstop — a
/// hold that only the deadline could end IS the lost-wake failure.
///
/// Weakening `.hold_check_before_register`: the pass reads `inflight ==
/// 1`, the lane lands and bumps the epoch, the pass registers (snapshot =
/// the NEW epoch) and parks: nothing else will ever notify — the batch
/// sits on a quiet ring with nothing in flight. With two in flight the
/// same race sits between any landing's wake and the next registration.
fn HoldPark(comptime weakening: Weakening, comptime inflight0: u8) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const pass_t = 0;
        const lane_t = 1;
        const committer_t = 2;
        const items = [1]Item{.{ .id = 9, .len = 1 }};

        const Ppc = enum(u8) { drained, register, check, park, parked, redrain_lock, redrain_pop, redrain_unlock, issued };
        const Lpc = enum(u8) { landing, notify, done };

        sh: Shared = .{ .leader = true },
        /// The landing notify: its epoch and whether the pass is linked.
        epoch: u8 = 0,
        pass_linked: bool = false,
        pass_epoch: u8 = 0,
        inflight: u8 = inflight0,
        ppc: Ppc = .drained,
        lpc: Lpc = .landing,
        /// The held window's batch: the drained entry + the re-drain.
        batch_n: u8 = 1,
        /// The leader's loop after the issue.
        pass: Pass = .{ .leading = true },
        c: Committer = .start(weakening),

        pub fn init() Self {
            return .{};
        }
        /// A parked pass with no pending wake and nobody left to deliver one.
        fn stuck(self: *const Self) bool {
            return self.ppc == .parked and self.epoch == self.pass_epoch and self.lpc == .done and self.c.phase == .done;
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                // A pass parked with nobody left to wake it is where the
                // schedule ends (the wedge `final` names).
                pass_t => (self.ppc == .issued and self.pass.pc == .done) or self.stuck(),
                lane_t => self.lpc == .done,
                committer_t => self.c.phase == .done,
                else => unreachable,
            };
        }

        /// The step after a wake (or at the start): the sound order
        /// registers first, the weakened one reads the flight first.
        fn restart(comptime w: Weakening) Ppc {
            return if (w == .hold_check_before_register) .check else .register;
        }

        fn stepPass(self: *Self) void {
            switch (self.ppc) {
                .drained => self.ppc = restart(weakening),
                .register => {
                    // `n.enable(io)`: link + snapshot the epoch.
                    self.pass_linked = true;
                    self.pass_epoch = self.epoch;
                    self.ppc = if (weakening == .hold_check_before_register) .park else .check;
                },
                .check => {
                    if (self.inflight == 0) {
                        // The flight is empty: cancel the registration, issue.
                        self.pass_linked = false;
                        self.ppc = .redrain_lock;
                    } else self.ppc = if (weakening == .hold_check_before_register) .register else .park;
                },
                .park => {
                    // `waitUntilUncancelable`: a passed epoch returns at once
                    // and the pass registers (or, weakened, reads) again.
                    if (self.epoch != self.pass_epoch) {
                        self.pass_linked = false;
                        self.ppc = restart(weakening);
                    } else self.ppc = .parked;
                },
                .parked => {
                    // Woken by the notify (unlinked there) or still parked.
                    if (self.epoch != self.pass_epoch) self.ppc = restart(weakening);
                },
                .redrain_lock => if (!self.sh.locked) {
                    self.sh.locked = true;
                    self.ppc = .redrain_pop;
                },
                .redrain_pop => {
                    while (self.sh.len > 0) {
                        _ = self.sh.pop();
                        self.batch_n += 1;
                    }
                    self.ppc = .redrain_unlock;
                },
                .redrain_unlock => {
                    self.sh.locked = false;
                    self.ppc = .issued;
                },
                // Issued: the leader's loop runs on.
                .issued => self.pass.step(&self.sh, weakening),
            }
        }

        fn stepLane(self: *Self) void {
            switch (self.lpc) {
                .landing => {
                    // `untrack`: the in-flight count drops first.
                    self.inflight -= 1;
                    self.lpc = .notify;
                },
                .notify => {
                    // `notifyWaiters`: bump the epoch, wake the linked waiter;
                    // the next window's landing follows while any is in flight.
                    self.epoch += 1;
                    self.pass_linked = false;
                    self.lpc = if (self.inflight == 0) .done else .landing;
                },
                .done => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                pass_t => self.stepPass(),
                lane_t => self.stepLane(),
                // The committer enqueues and elects: against the holding
                // leader it loses and parks on its oneshot; after the
                // leader's unlead it wins and runs its own pass.
                committer_t => self.c.step(&self.sh, &items, weakening, true),
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // A parked pass with nothing in flight and no pending wake: the
            // hold outlived the landing it was waiting for.
            if (self.ppc == .parked and self.inflight == 0 and self.lpc == .done and self.epoch == self.pass_epoch) return error.HoldOutlivedLanding;
            // A committer never leads while the holding pass holds the latch.
            if (self.ppc != .issued and self.c.elect.won) return error.CommitterOutranTheHoldingLeader;
        }
        pub fn final(self: *const Self) !void {
            if (self.ppc != .issued) return error.WindowNeverIssued;
            if (self.sh.len != 0) return error.EntryLeftQueued;
            // The two entries (the pre-drained one and the committer's) are
            // drained exactly once between the held window, the leader's
            // loop and the committer's own pass.
            if (self.batch_n + self.pass.drained + self.c.pass.drained != 2) return error.EntriesNotDrainedExactlyOnce;
            if (self.sh.leader) return error.LeadershipNotReleased;
        }
    };
}

/// The window gate (`Conveyor.awaitWindowCell`, `journal_lane.windowCells`):
/// a lane holding `W = 2` cells for windows, one more for requests; the
/// pass issues three windows one at a time — gate (register on `landed`,
/// read the tracked count, park while it is `≥ windowCap(W) = 1`), track,
/// pop a window cell (an empty pool is the handoff's `LaneQueueFull`: the
/// pass panic); the lane delivers each window in order — untrack, notify
/// `landed`, give the cell back (the hook runs before the give-back); a
/// barrier request takes a request cell, holds it, gives it back.
///
/// Holds: a window's pop never meets an empty pool, the pass is never
/// left parked with nothing in flight, all three windows issue and every
/// cell comes home. Weakenings: `.window_gate_at_cells` (the pass pops
/// between the lane's notify and its give-back), `.no_window_gate` (the
/// third window pops an empty pool), `.requests_share_window_cells` (the
/// barrier holds the cell the gate counted on) meet a full lane;
/// `.window_gate_check_before_register` and `.notify_before_untrack` park
/// the pass on a landing already past, with room in the flight and
/// nothing left to wake it.
fn WindowCells(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const pass_t = 0;
        const lane_t = 1;
        const request_t = 2;
        const window_cells: u8 = 2;
        const request_cells: u8 = 1;
        const windows: u8 = 3;
        const gate_cap: u8 = switch (weakening) {
            .window_gate_at_cells => window_cells,
            .no_window_gate => windows + 1,
            else => window_cells - 1,
        };
        const check_first = weakening == .window_gate_check_before_register;

        const Ppc = enum(u8) { register, check, park, parked, track, pop, done };
        const Lpc = enum(u8) { idle, untrack, notify, give_back };
        const Rpc = enum(u8) { take, give_back, done };

        free_windows: u8 = window_cells,
        free_requests: u8 = request_cells,
        /// Windows between `track` and their untrack.
        tracked: u8 = 0,
        /// Windows holding a cell, not yet delivered (FIFO).
        in_lane: u8 = 0,
        issued: u8 = 0,
        /// `landed`: its epoch and the pass's registration.
        epoch: u8 = 0,
        pass_epoch: u8 = 0,
        pass_registered: bool = false,
        /// A window's pop found the pool empty (the pass panic).
        met_full: bool = false,
        /// The request's cell came from the windows' pool.
        request_holds_window_cell: bool = false,
        ppc: Ppc = if (check_first) .check else .register,
        lpc: Lpc = .idle,
        rpc: Rpc = .take,

        pub fn init() Self {
            return .{};
        }

        /// Parked with no wake owed and nobody left to deliver one.
        fn stuck(self: *const Self) bool {
            return self.ppc == .parked and self.epoch == self.pass_epoch and self.lpc == .idle and self.in_lane == 0;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                pass_t => self.ppc == .done or self.stuck(),
                lane_t => self.lpc == .idle and self.in_lane == 0 and (self.ppc == .done or self.stuck()),
                request_t => self.rpc == .done,
                else => unreachable,
            };
        }

        fn gateStart() Ppc {
            return if (check_first) .check else .register;
        }

        fn stepPass(self: *Self) void {
            switch (self.ppc) {
                .register => {
                    // `n.enable(io)`: snapshot the epoch.
                    self.pass_registered = true;
                    self.pass_epoch = self.epoch;
                    self.ppc = if (check_first) .park else .check;
                },
                .check => {
                    if (self.tracked < gate_cap) {
                        self.pass_registered = false;
                        self.ppc = .track;
                    } else self.ppc = if (check_first) .register else .park;
                },
                .park => self.ppc = if (self.epoch != self.pass_epoch) gateStart() else .parked,
                .parked => if (self.epoch != self.pass_epoch) {
                    self.ppc = gateStart();
                },
                .track => {
                    self.tracked += 1;
                    self.ppc = .pop;
                },
                .pop => {
                    if (self.free_windows == 0) {
                        self.met_full = true;
                    } else {
                        self.free_windows -= 1;
                        self.in_lane += 1;
                    }
                    self.issued += 1;
                    self.ppc = if (self.issued == windows) .done else gateStart();
                },
                .done => unreachable,
            }
        }

        fn stepLane(self: *Self) void {
            switch (self.lpc) {
                .idle => if (self.in_lane > 0) {
                    self.lpc = if (weakening == .notify_before_untrack) .notify else .untrack;
                },
                .untrack => {
                    // `finishWindow` → `untrack`.
                    self.tracked -= 1;
                    self.in_lane -= 1;
                    self.lpc = if (weakening == .notify_before_untrack) .give_back else .notify;
                },
                .notify => {
                    // `landed.notifyWaiters`.
                    self.epoch +%= 1;
                    self.pass_registered = false;
                    self.lpc = if (weakening == .notify_before_untrack) .untrack else .give_back;
                },
                .give_back => {
                    // The hook returned: `freeCell`.
                    self.free_windows += 1;
                    self.lpc = .idle;
                },
            }
        }

        fn stepRequest(self: *Self) void {
            switch (self.rpc) {
                .take => {
                    if (weakening == .requests_share_window_cells) {
                        // A refused request leaves (retryable); a taken
                        // cell is the window pool's.
                        if (self.free_windows > 0) {
                            self.free_windows -= 1;
                            self.request_holds_window_cell = true;
                            self.rpc = .give_back;
                        } else self.rpc = .done;
                    } else {
                        self.free_requests -= 1;
                        self.rpc = .give_back;
                    }
                },
                .give_back => {
                    if (self.request_holds_window_cell) self.free_windows += 1 else self.free_requests += 1;
                    self.request_holds_window_cell = false;
                    self.rpc = .done;
                },
                .done => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                pass_t => self.stepPass(),
                lane_t => self.stepLane(),
                request_t => self.stepRequest(),
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.met_full) return error.WindowMetAFullLane;
            if (self.ppc == .parked and self.tracked < gate_cap and self.stuck()) return error.GateOutlivedTheLanding;
        }

        pub fn final(self: *const Self) !void {
            if (self.ppc != .done) return error.WindowNeverIssued;
            if (self.tracked != 0 or self.in_lane != 0) return error.WindowLeftInFlight;
            if (self.free_windows != window_cells or self.free_requests != request_cells) return error.CellNotReturned;
        }
    };
}

/// The committer's room park (`Conveyor.enqueueParking`, 0003 §BO.2): a
/// queue of ONE cell, full, its entry the pass's; the committer found no
/// room on its fast path, so it counts itself (`room_waiters`), registers
/// on `room` (the epoch snapshot), re-tries under the queue mutex and
/// parks; the pass drains under the mutex, then reads the count and
/// notifies while it is non-zero.
///
/// Holds: the committer always enqueues — never parked with the cell free
/// and no wake owed. Weakenings, each stranding it: `.room_count_after_try`
/// (the pass's read between the failed re-try and the count sees no
/// waiter), `.room_check_before_register` (the notify between the re-try
/// and the registration is spent on nobody), `.room_read_before_drain`
/// (the read precedes the committer's count, the drain its re-try).
fn RoomPark(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const committer_t = 0;
        const pass_t = 1;

        const Cpc = enum(u8) { count, register, lock, attempt, unlock, park, parked, enqueued };
        const Ppc = enum(u8) { early_read, lock, drain, unlock, read, notify, done };

        /// Entries in the one-cell queue (starts full).
        len: u8 = 1,
        mutex: bool = false,
        waiters: u8 = 0,
        epoch: u8 = 0,
        c_epoch: u8 = 0,
        c_counted: bool = false,
        c_got_room: bool = false,
        p_read: u8 = 0,
        cpc: Cpc = switch (weakening) {
            .room_count_after_try => .register,
            .room_check_before_register => .count,
            else => .count,
        },
        ppc: Ppc = if (weakening == .room_read_before_drain) .early_read else .lock,

        pub fn init() Self {
            return .{};
        }

        /// Parked with no wake owed and nobody left to deliver one.
        fn stranded(self: *const Self) bool {
            return self.cpc == .parked and self.epoch == self.c_epoch and self.ppc == .done;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                committer_t => self.cpc == .enqueued or self.stranded(),
                pass_t => self.ppc == .done,
                else => unreachable,
            };
        }

        fn stepCommitter(self: *Self) void {
            switch (self.cpc) {
                .count => {
                    self.waiters += 1;
                    self.c_counted = true;
                    self.cpc = switch (weakening) {
                        .room_check_before_register => .lock,
                        .room_count_after_try => .park,
                        else => .register,
                    };
                },
                .register => {
                    // `n.enable(io)`: the epoch snapshot.
                    self.c_epoch = self.epoch;
                    self.cpc = if (weakening == .room_check_before_register) .park else .lock;
                },
                .lock => if (!self.mutex) {
                    self.mutex = true;
                    self.cpc = .attempt;
                },
                .attempt => {
                    self.c_got_room = self.len == 0;
                    if (self.c_got_room) self.len += 1;
                    self.cpc = .unlock;
                },
                .unlock => {
                    self.mutex = false;
                    if (self.c_got_room) {
                        self.cpc = .enqueued;
                    } else if (weakening == .room_count_after_try and !self.c_counted) {
                        self.cpc = .count;
                    } else if (weakening == .room_check_before_register) {
                        self.cpc = .register;
                    } else self.cpc = .park;
                },
                .park => self.cpc = if (self.epoch != self.c_epoch) self.retry() else .parked,
                .parked => if (self.epoch != self.c_epoch) {
                    self.cpc = self.retry();
                },
                .enqueued => unreachable,
            }
        }

        fn retry(_: *const Self) Cpc {
            return if (weakening == .room_check_before_register) .lock else .register;
        }

        fn stepPass(self: *Self) void {
            switch (self.ppc) {
                .early_read => {
                    self.p_read = self.waiters;
                    self.ppc = .lock;
                },
                .lock => if (!self.mutex) {
                    self.mutex = true;
                    self.ppc = .drain;
                },
                .drain => {
                    self.len = 0;
                    self.ppc = .unlock;
                },
                .unlock => {
                    self.mutex = false;
                    self.ppc = if (weakening == .room_read_before_drain) .notify else .read;
                },
                .read => {
                    self.p_read = self.waiters;
                    self.ppc = .notify;
                },
                .notify => {
                    // `room.notifyWaiters` while the count read non-zero.
                    if (self.p_read != 0) self.epoch +%= 1;
                    self.ppc = .done;
                },
                .done => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                committer_t => self.stepCommitter(),
                pass_t => self.stepPass(),
                else => unreachable,
            }
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            if (self.cpc != .enqueued) return error.CommitterStrandedWithRoom;
        }
    };
}

/// The lane's fsync inbox (`JournalLane.requestBarrier` / the stage-B
/// scan, 0003 §BO.1): a submitter pushes a window on the inbound queue,
/// then its fsync on the inbox (a CAS push); a second submitter pushes an
/// fsync of its own; the lane's scan takes the inbox whole (one swap),
/// drains the inbound windows, then admits the fsyncs it took, each with
/// the cover of the windows admitted so far, and scans again until it has
/// both.
///
/// Holds: no fsync is lost and the first submitter's fsync is never
/// admitted without its window in the cover (§BB.4). Weakenings:
/// `.inbox_push_not_cas` (both pushers read the same head: one node is
/// lost — the lane waits for it forever), `.fsyncs_after_inbound_scan` (the
/// window lands in the inbound queue after the drain, its fsync in the
/// inbox before the swap: admitted uncovered).
fn FsyncInbox(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const first_t = 0;
        const second_t = 1;
        const lane_t = 2;
        /// The fsync nodes as bits; the inbox head is the set it links.
        const f1: u8 = 1;
        const f2: u8 = 2;

        const Spc = enum(u8) { push_window, load, publish, done };
        const Lpc = enum(u8) { swap, drain, admit };

        inbound: u8 = 0,
        window_admitted: bool = false,
        inbox: u8 = 0,
        taken: u8 = 0,
        admitted: u8 = 0,
        uncovered: bool = false,
        loaded: [2]u8 = @splat(0),
        spc: [2]Spc = .{ .push_window, .load },
        lpc: Lpc = if (weakening == .fsyncs_after_inbound_scan) .drain else .swap,

        pub fn init() Self {
            return .{};
        }

        /// Both submitters finished and nothing is left to take: an fsync
        /// still unadmitted was lost.
        fn starved(self: *const Self) bool {
            return self.spc[0] == .done and self.spc[1] == .done and self.inbox == 0 and self.inbound == 0 and self.taken == 0 and self.lpc == self.scanStart();
        }

        fn scanStart(_: *const Self) Lpc {
            return if (weakening == .fsyncs_after_inbound_scan) .drain else .swap;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                first_t, second_t => self.spc[t] == .done,
                lane_t => (self.admitted == f1 | f2 and self.window_admitted) or self.starved(),
                else => unreachable,
            };
        }

        fn stepSubmitter(self: *Self, t: usize) void {
            const mine: u8 = if (t == first_t) f1 else f2;
            switch (self.spc[t]) {
                .push_window => {
                    self.inbound += 1;
                    self.spc[t] = .load;
                },
                .load => {
                    self.loaded[t] = self.inbox;
                    self.spc[t] = .publish;
                },
                .publish => {
                    if (weakening == .inbox_push_not_cas) {
                        self.inbox = self.loaded[t] | mine;
                        self.spc[t] = .done;
                    } else if (self.inbox == self.loaded[t]) {
                        self.inbox |= mine;
                        self.spc[t] = .done;
                    } else self.spc[t] = .load;
                },
                .done => unreachable,
            }
        }

        fn stepLane(self: *Self) void {
            switch (self.lpc) {
                .swap => {
                    self.taken = self.inbox;
                    self.inbox = 0;
                    self.lpc = if (weakening == .fsyncs_after_inbound_scan) .admit else .drain;
                },
                .drain => {
                    if (self.inbound != 0) self.window_admitted = true;
                    self.inbound = 0;
                    self.lpc = if (weakening == .fsyncs_after_inbound_scan) .swap else .admit;
                },
                .admit => {
                    if (self.taken & f1 != 0 and !self.window_admitted) self.uncovered = true;
                    self.admitted |= self.taken;
                    self.taken = 0;
                    self.lpc = self.scanStart();
                },
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                first_t, second_t => self.stepSubmitter(t),
                lane_t => self.stepLane(),
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.uncovered) return error.FsyncAdmittedWithoutItsWindow;
        }

        pub fn final(self: *const Self) !void {
            if (self.admitted != f1 | f2) return error.FsyncLost;
            if (!self.window_admitted) return error.WindowNeverAdmitted;
        }
    };
}

/// Model 12 (§BV): a corpse's destroy under self-apply — two leaves L0 < L1,
/// a record on each — beside two committers, C0 on L0 and C1 on L1, each
/// its own apply under its one leaf. A leaf is a lock word (its owner, or
/// free); a take of a held leaf is a no-op step (the explorer prunes it).
/// Shipped: the destroy takes L0, applies, releases, THEN takes L1 — it
/// never waits for a leaf while holding one, so a committer it blocks
/// waits for one apply. The invariant: no committer waits for a leaf the
/// destroy holds while the destroy itself waits (the chain that put every
/// unlinker behind a destroy's whole acquisition). The final: every record
/// applied once.
fn LeafwiseDestroy(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const destroy_t = 0;
        const c0_t = 1;
        const c1_t = 2;
        const free: u8 = 0xff;

        const Dpc = enum(u8) { take0, apply0, give0, take1, apply1, give1, done };
        const Cpc = enum(u8) { take, apply, give, done };

        owner: [2]u8 = .{ free, free },
        dpc: Dpc = .take0,
        cpc: [2]Cpc = .{ .take, .take },
        applied: [4]u8 = .{ 0, 0, 0, 0 },

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                destroy_t => self.dpc == .done,
                c0_t, c1_t => self.cpc[t - 1] == .done,
                else => unreachable,
            };
        }

        fn take(self: *Self, leaf: usize, who: u8) bool {
            if (self.owner[leaf] != free) return false;
            self.owner[leaf] = who;
            return true;
        }

        fn stepDestroy(self: *Self) void {
            switch (weakening) {
                .destroy_union_hold => switch (self.dpc) {
                    .take0 => if (self.take(0, destroy_t)) {
                        self.dpc = .take1;
                    },
                    .take1 => if (self.take(1, destroy_t)) {
                        self.dpc = .apply0;
                    },
                    .apply0 => {
                        self.applied[0] += 1;
                        self.dpc = .apply1;
                    },
                    .apply1 => {
                        self.applied[1] += 1;
                        self.dpc = .give0;
                    },
                    .give0 => {
                        self.owner[0] = free;
                        self.dpc = .give1;
                    },
                    .give1 => {
                        self.owner[1] = free;
                        self.dpc = .done;
                    },
                    .done => unreachable,
                },
                else => switch (self.dpc) {
                    .take0 => if (self.take(0, destroy_t)) {
                        self.dpc = .apply0;
                    },
                    .apply0 => {
                        self.applied[0] += 1;
                        self.dpc = .give0;
                    },
                    .give0 => {
                        self.owner[0] = free;
                        self.dpc = .take1;
                    },
                    .take1 => if (self.take(1, destroy_t)) {
                        self.dpc = .apply1;
                    },
                    .apply1 => {
                        self.applied[1] += 1;
                        self.dpc = .give1;
                    },
                    .give1 => {
                        self.owner[1] = free;
                        self.dpc = .done;
                    },
                    .done => unreachable,
                },
            }
        }

        fn stepCommitter(self: *Self, c: usize) void {
            const who: u8 = @intCast(c + 1);
            switch (self.cpc[c]) {
                .take => if (self.take(c, who)) {
                    self.cpc[c] = .apply;
                },
                .apply => {
                    self.applied[2 + c] += 1;
                    self.cpc[c] = .give;
                },
                .give => {
                    self.owner[c] = free;
                    self.cpc[c] = .done;
                },
                .done => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                destroy_t => self.stepDestroy(),
                c0_t, c1_t => self.stepCommitter(t - 1),
                else => unreachable,
            }
        }

        /// The destroy is parked on a leaf another holds.
        fn destroyWaits(self: *const Self) bool {
            return switch (self.dpc) {
                .take0 => self.owner[0] != free and self.owner[0] != destroy_t,
                .take1 => self.owner[1] != free and self.owner[1] != destroy_t,
                else => false,
            };
        }

        pub fn invariant(self: *const Self) !void {
            for (0..2) |c| {
                const blocked_by_destroy = self.cpc[c] == .take and self.owner[c] == destroy_t;
                if (blocked_by_destroy and self.destroyWaits()) return error.CommitterBehindAWaitingDestroy;
            }
        }

        pub fn final(self: *const Self) !void {
            for (self.applied) |n| if (n != 1) return error.RecordNotAppliedOnce;
            for (self.owner) |o| if (o != free) return error.LeafLeftHeld;
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

test "conveyor_pass_fiber_wake_not_lost: holds; a park without the leader re-read fails (the winner's wake coalesced into the mutex-park wake)" {
    try explorer.expectHolds(PassFiber(.none), .{});
    try explorer.expectFails(PassFiber(.park_without_leader_recheck), .{}, error.FinalViolated);
}

test "conveyor_hold_park_never_outlives_the_landing: holds with one and with two windows in flight (register-then-check per landing; the late entry is drained exactly once); check-before-register fails both (the landing between the read and the park is lost)" {
    try explorer.expectHolds(HoldPark(.none, 1), .{});
    try explorer.expectHolds(HoldPark(.none, 2), .{});
    try explorer.expectFails(HoldPark(.hold_check_before_register, 1), .{}, error.InvariantViolated);
    try explorer.expectFails(HoldPark(.hold_check_before_register, 2), .{}, error.InvariantViolated);
}

test "conveyor_window_gate_never_meets_a_full_lane: holds (a gate at window_cells − 1, registered before it reads, the lane untracking before it notifies, the requests on their own cells); a gate at window_cells, no gate and a barrier on a window cell meet a full lane; check-before-register and notify-before-untrack strand the parked pass" {
    try explorer.expectHolds(WindowCells(.none), .{});
    try explorer.expectFails(WindowCells(.window_gate_at_cells), .{}, error.InvariantViolated);
    try explorer.expectFails(WindowCells(.no_window_gate), .{}, error.InvariantViolated);
    try explorer.expectFails(WindowCells(.requests_share_window_cells), .{}, error.InvariantViolated);
    try explorer.expectFails(WindowCells(.window_gate_check_before_register), .{}, error.InvariantViolated);
    try explorer.expectFails(WindowCells(.notify_before_untrack), .{}, error.InvariantViolated);
}

test "conveyor_room_park_never_strands_a_committer: holds (count, register, re-try under the queue mutex, park; the pass drains, then reads the count and notifies); counting after the re-try, re-trying before registering and reading before the drain each strand the committer with the room free" {
    try explorer.expectHolds(RoomPark(.none), .{});
    try explorer.expectFails(RoomPark(.room_count_after_try), .{}, error.FinalViolated);
    try explorer.expectFails(RoomPark(.room_check_before_register), .{}, error.FinalViolated);
    try explorer.expectFails(RoomPark(.room_read_before_drain), .{}, error.FinalViolated);
}

test "conveyor_fsync_inbox_loses_no_fsync_and_covers_its_window: holds (CAS push; the scan takes the inbox, drains the inbound windows, admits); a load-then-store push loses an fsync; draining the windows before taking the inbox admits an fsync without its window" {
    try explorer.expectHolds(FsyncInbox(.none), .{});
    try explorer.expectFails(FsyncInbox(.inbox_push_not_cas), .{}, error.FinalViolated);
    try explorer.expectFails(FsyncInbox(.fsyncs_after_inbound_scan), .{}, error.InvariantViolated);
}

test "conveyor_leafwise_destroy_never_waits_holding_a_leaf (§BV): holds (one leaf at a time — no committer ever waits for a leaf the destroy holds while the destroy itself waits; every record applied once); the union hold fails (C0 waits on L0 while the destroy, holding it, waits for L1 under C1)" {
    try explorer.expectHolds(LeafwiseDestroy(.none), .{});
    try explorer.expectFails(LeafwiseDestroy(.destroy_union_hold), .{}, error.InvariantViolated);
}

// The state counts the evidence note records.
test "conveyor models: exhaustive within the explorer's default limits" {
    inline for (.{ LeaderUnique(.none), NoLostWakeup(.none), FifoUnderRace(.none), BudgetConserved(.none), GuardOutlives(.none), GroupAtomic(.none), PassFiber(.none), HoldPark(.none, 1), HoldPark(.none, 2), WindowCells(.none), RoomPark(.none), FsyncInbox(.none), LeafwiseDestroy(.none) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 5000);
    }
}
