//! Exhaustive models of the two doorbells (`doorbell.zig`) with EXPLICIT
//! store buffers (TSO): a thread's store lands in its own buffer, is
//! visible to its own later loads (store forwarding) and to nobody else
//! until the FENCE step drains it — an SC RMW/store IS a drained store, so
//! "drop the fence" = let the buffered store land only after the thread's
//! later loads (a separate step the explorer can schedule last). A futex
//! bucket is explicit: a wake delivers only to a QUEUED sleeper; FUTEX_WAIT's
//! admission reads main memory (the kernel's latest-value read).
//!
//! Completion doorbell (`Cqe`): D1 = a prior op's LATE doorbell completion
//! (its slot was already reaped — nothing for the parker's scan to find),
//! D2 = the pending op's DONE + doorbell, P = one k=1 parker. Invariant
//! after every step: never (P queued ∧ D1 done ∧ D2 done) — the strand.
//! Final: the era paid ≤ 1 wake for the parker's mark. Weakenings proven
//! red: drop the completer's fence; drop the parker's fence; snapshot
//! before the register's drain (the Dekker load-before-store shape); the
//! 0/1 flag latch with a parker-side clear (the 2026-09-06 lost wake: D1's
//! pay consumes the era's flag toward a parker not yet asleep and D2's real
//! completion collapses).
//!
//! Submit doorbell (`Submit`): producer = ring push → bump → fence → parked
//! load → wake; consumer = parked := 1 → fence → snapshot → rescan → admit.
//! Invariant: an admitted park implies an empty ring. Weakenings proven
//! red: snapshot AFTER the rescan (the reference's pre-fix order); drop
//! either fence (the reference's production shape had no producer fence —
//! decision 0002 ledger Q2).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const NO_PAY: u8 = 0xff;

fn reached(seq: u8, at: u8) bool {
    return seq -% at < 128;
}

pub const CqeVariant = enum { shipped, no_completer_fence, no_parker_fence, snapshot_before_register, flag_latch };

pub fn Cqe(comptime variant: CqeVariant) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const d1 = 0;
        const d2 = 1;
        const p = 2;

        // ---- main memory ----
        seq: u8 = 0,
        parked: u8 = 0,
        wake_at: u8 = 0,
        paid: u8 = NO_PAY,
        flag: bool = false,
        done_op: bool = false,
        // ---- store buffers ----
        /// Completer i's buffered bump (0 or 1).
        pend_bump: [2]u8 = @splat(0),
        /// The parker's buffered registration.
        pend_parked: bool = false,
        // ---- the futex bucket ----
        queued: bool = false,
        // ---- per-thread registers ----
        d_seq: [2]u8 = @splat(0),
        d_at: [2]u8 = @splat(0),
        snap: u8 = 0,
        /// The mark each completer paid for (NO_PAY = did not pay).
        paid_for: [2]u8 = @splat(NO_PAY),
        /// The parker's own mark once stored.
        my_mark: ?u8 = null,
        consumed: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == p) self.pc[p] == 9 else self.pc[t] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == p) self.stepParker() else self.stepCompleter(t);
        }

        fn seqMain(self: *const Self) u8 {
            return self.seq;
        }
        fn drainBump(self: *Self, i: usize) void {
            self.seq +%= self.pend_bump[i];
            self.pend_bump[i] = 0;
        }
        fn drainParked(self: *Self) void {
            if (self.pend_parked) {
                self.parked += 1;
                self.pend_parked = false;
            }
        }
        fn deliverWake(self: *Self, i: usize) void {
            self.paid_for[i] = self.d_at[i];
            if (self.queued) self.queued = false;
        }

        fn stepCompleter(self: *Self, i: usize) void {
            switch (self.pc[i]) {
                0 => {
                    // D2 publishes its op's DONE first; D1's op was reaped already.
                    if (i == d2) self.done_op = true;
                    self.pc[i] = 1;
                },
                1 => {
                    // seq' = seq + 1: buffered; forwarded to our own reads.
                    self.pend_bump[i] = 1;
                    self.d_seq[i] = self.seq +% 1;
                    self.pc[i] = 2;
                },
                2 => {
                    // The fence: drains the bump before the parked load.
                    if (variant != .no_completer_fence) self.drainBump(i);
                    self.pc[i] = 3;
                },
                3 => {
                    // "Are you registered?"
                    if (self.parked == 0) {
                        self.pc[i] = 8;
                    } else self.pc[i] = 4;
                },
                4 => {
                    self.d_at[i] = self.wake_at;
                    self.pc[i] = if (reached(self.d_seq[i], self.d_at[i])) 5 else 8;
                },
                5 => {
                    // The latch.
                    const pay = switch (variant) {
                        .flag_latch => blk: {
                            if (self.flag) break :blk false;
                            self.flag = true;
                            break :blk true;
                        },
                        else => blk: {
                            if (self.paid == self.d_at[i]) break :blk false;
                            self.paid = self.d_at[i];
                            break :blk true;
                        },
                    };
                    self.pc[i] = if (pay) 6 else 8;
                },
                6 => {
                    // FUTEX_WAKE(seq, INT_MAX): delivered only to a queued sleeper.
                    self.deliverWake(i);
                    self.pc[i] = 8;
                },
                8 => {
                    // A dropped fence: the buffered bump lands at last.
                    self.drainBump(i);
                    self.pc[i] = 9;
                },
                else => unreachable,
            }
        }

        fn stepParker(self: *Self) void {
            switch (self.pc[p]) {
                0 => {
                    // REGISTER FIRST — buffered. The permuted variant takes
                    // the snapshot here instead and registers at step 2.
                    if (variant == .snapshot_before_register) {
                        self.snap = self.seqMain();
                    } else {
                        self.pend_parked = true;
                        if (variant == .flag_latch) self.flag = false;
                    }
                    self.pc[p] = 1;
                },
                1 => {
                    // The fence: drains the registration before the snapshot.
                    if (variant != .no_parker_fence) self.drainParked();
                    self.pc[p] = 2;
                },
                2 => {
                    if (variant == .snapshot_before_register) {
                        self.pend_parked = true;
                    } else {
                        self.snap = self.seqMain();
                    }
                    self.pc[p] = 3;
                },
                3 => {
                    // First parker of the era: the mark is snapshot + 1.
                    self.wake_at = self.snap +% 1;
                    self.my_mark = self.wake_at;
                    self.pc[p] = 4;
                },
                4 => {
                    // The MANDATORY re-scan of the pending set.
                    if (self.done_op) self.pc[p] = 7 else self.pc[p] = 5;
                },
                5 => {
                    // FUTEX_WAIT admission on main memory.
                    if (self.seqMain() == self.snap) {
                        self.queued = true;
                        self.pc[p] = 6;
                    } else self.pc[p] = 4;
                },
                6 => {
                    // Asleep until a wake dequeues us; then re-scan.
                    if (!self.queued) self.pc[p] = 4;
                },
                7 => {
                    self.consumed = true;
                    self.pc[p] = 8;
                },
                8 => {
                    // parkEnd + a dropped fence's late landing.
                    self.drainParked();
                    self.parked -= 1;
                    self.pc[p] = 9;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            if (self.queued and self.pc[d1] == 9 and self.pc[d2] == 9) return error.ParkedReaperStranded;
        }
        pub fn final(self: *const Self) !void {
            if (!self.consumed) return error.NotConsumed;
            // The era's ONE wake: at most one completer paid for the
            // parker's mark (a pre-snapshot completer's pay for a STALE
            // mark is the benign over-wake, not a second pay of this era).
            var pays: u8 = 0;
            for (self.paid_for) |m| pays += @intFromBool(m == self.my_mark.?);
            if (pays > 1) return error.EraPaidTwice;
        }
    };
}

pub const SubmitVariant = enum { shipped, snapshot_after_rescan, no_producer_fence, no_consumer_fence };

pub fn Submit(comptime variant: SubmitVariant) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const producer = 0;
        const consumer = 1;

        // ---- main memory ----
        pushed: bool = false,
        doorbell: u8 = 0,
        parked: u8 = 0,
        // ---- store buffers ----
        pend_bump: u8 = 0,
        pend_parked: bool = false,
        // ---- the futex bucket ----
        queued: bool = false,
        // ---- registers ----
        snap: u8 = 0,
        consumed: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 9;
        }
        fn drainBump(self: *Self) void {
            self.doorbell +%= self.pend_bump;
            self.pend_bump = 0;
        }
        fn drainParked(self: *Self) void {
            if (self.pend_parked) {
                self.parked = 1;
                self.pend_parked = false;
            }
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == producer) switch (self.pc[producer]) {
                0 => {
                    // The ring push (its Release publication is the cell seq;
                    // a drained store for the Dekker's purposes).
                    self.pushed = true;
                    self.pc[producer] = 1;
                },
                1 => {
                    self.pend_bump = 1;
                    self.pc[producer] = 2;
                },
                2 => {
                    if (variant != .no_producer_fence) self.drainBump();
                    self.pc[producer] = 3;
                },
                3 => {
                    // Wake iff the daemon is parked.
                    if (self.parked != 0 and self.queued) self.queued = false;
                    self.pc[producer] = 8;
                },
                8 => {
                    self.drainBump();
                    self.pc[producer] = 9;
                },
                else => unreachable,
            } else switch (self.pc[consumer]) {
                0 => {
                    self.pend_parked = true;
                    self.pc[consumer] = 1;
                },
                1 => {
                    if (variant != .no_consumer_fence) self.drainParked();
                    self.pc[consumer] = 2;
                },
                2 => {
                    // Shipped: SNAPSHOT, then rescan. Pre-fix: rescan first.
                    if (variant == .snapshot_after_rescan) self.rescan() else self.snap = self.doorbell;
                    self.pc[consumer] = 3;
                },
                3 => {
                    if (variant == .snapshot_after_rescan) self.snap = self.doorbell else self.rescan();
                    self.pc[consumer] = if (self.consumed) 7 else 4;
                },
                4 => {
                    // futex_waitv admission against the snapshot.
                    if (self.doorbell == self.snap) {
                        self.queued = true;
                        self.pc[consumer] = 5;
                    } else self.pc[consumer] = 6;
                },
                5 => {
                    if (!self.queued) self.pc[consumer] = 6;
                },
                6 => {
                    // Woken or refused: rescan.
                    self.rescan();
                    self.pc[consumer] = if (self.consumed) 7 else 4;
                },
                7 => {
                    self.drainParked();
                    self.parked = 0;
                    self.pc[consumer] = 9;
                },
                else => unreachable,
            }
        }
        fn rescan(self: *Self) void {
            if (self.pushed and !self.consumed) self.consumed = true;
        }
        pub fn invariant(self: *const Self) !void {
            // An admitted park with a published, unconsumed push and the
            // producer finished = the daemon sleeps on work it will never
            // be woken for.
            if (self.queued and self.pushed and !self.consumed and self.pc[producer] == 9) return error.ServiceThreadStranded;
        }
        pub fn final(self: *const Self) !void {
            if (!self.consumed) return error.PushLost;
        }
    };
}

// ---------------------------------------------------------------------------

test "completion doorbell: a parked reaper is never stranded and the era pays ≤ 1 wake for its mark — and each recorded weakening strands it" {
    try explorer.expectHolds(Cqe(.shipped), .{ .max_depth = 512 });
    try explorer.expectFails(Cqe(.no_completer_fence), .{ .max_depth = 512 }, error.InvariantViolated);
    try explorer.expectFails(Cqe(.no_parker_fence), .{ .max_depth = 512 }, error.InvariantViolated);
    try explorer.expectFails(Cqe(.snapshot_before_register), .{ .max_depth = 512 }, error.InvariantViolated);
    try explorer.expectFails(Cqe(.flag_latch), .{ .max_depth = 512 }, error.InvariantViolated);
}

test "submit doorbell: an admitted service park implies an empty ring — and snapshot-after-rescan or a dropped fence on either side strands the thread" {
    try explorer.expectHolds(Submit(.shipped), .{});
    try explorer.expectFails(Submit(.snapshot_after_rescan), .{}, error.InvariantViolated);
    try explorer.expectFails(Submit(.no_producer_fence), .{}, error.InvariantViolated);
    try explorer.expectFails(Submit(.no_consumer_fence), .{}, error.InvariantViolated);
}
