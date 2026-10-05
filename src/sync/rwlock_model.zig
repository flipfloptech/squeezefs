//! Explorer models of the lock core (`rwlock.zig`; the reference's loom
//! `sqz_sync_models`).
//!
//! Plain value types: the interior mutex is a flag with explicit acquire
//! (a no-op step while held, which the explorer prunes) and release steps;
//! the lock word is `writer` + `readers`; the FIFO is an id array; each
//! waiter's park word is a byte. A park is ONE atomic compare-and-sleep
//! step (the futex law): it proceeds when the word holds a token and is a
//! no-op otherwise — and a sleeper whose every possible waker has finished
//! is the LOST WAKEUP, flagged for the invariant (an explorer prunes a
//! no-op step, so a wedge has to be named, not waited for).
//!
//! Weakening ledger (which weakening fails which model):
//!   1 park_racing_release_never_strands — try-then-register as two
//!     critical sections (the retired shape: lock free, waiter queued, no
//!     token); the park's compare split from its sleep (a token landing
//!     between the two is lost)
//!   2 dead_waiter_never_wedges          — release ASSIGNS ownership to the
//!     (dead) front waiter instead of flipping free
//!   3 mutual_exclusion                  — the grant's check and set as two
//!     critical sections
//!   4 writer_preference                 — a fresh shared attempt ignores
//!     the queued writer

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Mode = enum(u8) { shared, exclusive };

const Weakening = enum {
    none,
    /// `acquire` runs the attempt and the enqueue as two interior holds.
    split_try_then_register,
    /// The park reads its word, then sleeps as a separate step.
    check_then_sleep,
    /// Release hands the lock to the front waiter (the tokio protocol).
    assign_on_release,
    /// The grant's check and its set are two interior holds.
    split_check_set,
    /// A fresh shared attempt does not yield to a queued exclusive waiter.
    no_write_preference,
};

/// Waiter ids 1..3; `modes[id]` is the waiter's mode.
const max_waiters = 4;

const Shared = struct {
    /// The interior mutex.
    locked: bool = false,
    writer: bool = false,
    readers: u8 = 0,
    /// FIFO of waiter ids.
    q: [max_waiters]u8 = @splat(0),
    qn: u8 = 0,
    /// Per-waiter park word: 0 parked, 1 token.
    token: [max_waiters]u8 = @splat(0),
    /// Futex wakes issued (the `.check_then_sleep` sleeper needs one issued
    /// AFTER it went to sleep).
    wakes: u8 = 0,

    fn isQueued(self: *const Shared, id: u8) bool {
        for (self.q[0..self.qn]) |x| if (x == id) return true;
        return false;
    }
    fn push(self: *Shared, id: u8) void {
        self.q[self.qn] = id;
        self.qn += 1;
    }
    fn remove(self: *Shared, id: u8) void {
        var out: [max_waiters]u8 = @splat(0);
        var n: u8 = 0;
        for (self.q[0..self.qn]) |x| if (x != id) {
            out[n] = x;
            n += 1;
        };
        self.q = out;
        self.qn = n;
    }
    fn queuedWriters(self: *const Shared, comptime modes: [max_waiters]Mode) u8 {
        var n: u8 = 0;
        for (self.q[0..self.qn]) |x| if (modes[x] == .exclusive) {
            n += 1;
        };
        return n;
    }
    fn wake(self: *Shared, id: u8) void {
        if (self.token[id] == 0) {
            self.token[id] = 1;
            self.wakes += 1;
        }
    }
    /// One exclusive front waiter, or every leading shared waiter.
    fn wakeFront(self: *Shared, comptime modes: [max_waiters]Mode) void {
        if (self.qn == 0) return;
        if (modes[self.q[0]] == .exclusive) {
            self.wake(self.q[0]);
            return;
        }
        for (self.q[0..self.qn]) |x| {
            if (modes[x] == .exclusive) return;
            self.wake(x);
        }
    }
    /// The shipped grant rule (`RwLock.grantLocked`).
    fn grant(self: *Shared, comptime modes: [max_waiters]Mode, mode: Mode, queued: bool, comptime weakening: Weakening) bool {
        switch (mode) {
            .shared => {
                const blocked_by_writer_wait = !queued and weakening != .no_write_preference and self.queuedWriters(modes) > 0;
                if (!self.writer and !blocked_by_writer_wait) {
                    self.readers += 1;
                    return true;
                }
            },
            .exclusive => {
                if (!self.writer and self.readers == 0 and (queued or self.qn == 0)) {
                    self.writer = true;
                    return true;
                }
            },
        }
        return false;
    }
};

// ---------------------------------------------------------------------------
// 1. the park racing a release
// ---------------------------------------------------------------------------

/// The holder releases while a waiter runs the shipped grant-or-enqueue
/// attempt; the waiter must end holding the lock in every interleaving. A
/// waiter parked with the lock free and no token is the stranded state
/// (the 2 s-tick stall class).
///
/// Weakenings: `.split_try_then_register` — the release lands between the
/// refused attempt and the enqueue (empty queue ⇒ wakes nobody), the
/// waiter parks wakeless; `.check_then_sleep` — the token lands between
/// the park's compare and its sleep.
fn ParkRacingRelease(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const holder = 0;
        const waiter = 1;
        const wid: u8 = 1;
        const modes = [max_waiters]Mode{ .exclusive, .exclusive, .exclusive, .exclusive };

        sh: Shared = .{ .writer = true },
        hpc: u8 = 0,
        wpc: u8 = 0,
        granted: bool = false,
        /// `.check_then_sleep`: the compare's stale read and the wake count
        /// at the moment of sleeping.
        tmp: u8 = 0,
        wakes_at_sleep: u8 = 0,
        stranded: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                holder => self.hpc == 3,
                waiter => self.wpc == 9,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            const sh = &self.sh;
            switch (t) {
                holder => switch (self.hpc) {
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.hpc = 1;
                    },
                    1 => {
                        sh.writer = false;
                        sh.wakeFront(modes);
                        self.hpc = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.hpc = 3;
                    },
                    else => unreachable,
                },
                waiter => switch (self.wpc) {
                    // The first critical section.
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.wpc = 1;
                    },
                    1 => {
                        if (sh.grant(modes, .exclusive, false, weakening)) {
                            self.granted = true;
                        } else if (weakening != .split_try_then_register) {
                            sh.push(wid);
                        }
                        self.wpc = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.wpc = if (self.granted) 9 else if (weakening == .split_try_then_register) 10 else 3;
                    },
                    // The retired shape's second critical section: enqueue.
                    10 => if (!sh.locked) {
                        sh.locked = true;
                        self.wpc = 11;
                    },
                    11 => {
                        sh.push(wid);
                        self.wpc = 12;
                    },
                    12 => {
                        sh.locked = false;
                        self.wpc = 3;
                    },
                    // The park.
                    3 => if (weakening == .check_then_sleep) {
                        self.tmp = sh.token[wid];
                        self.wpc = 13;
                    } else if (sh.token[wid] != 0) {
                        self.wpc = 4;
                    } else if (self.hpc == 3) {
                        self.stranded = true;
                    },
                    // The weakened sleep: decided on the stale compare; once
                    // asleep it needs a wake issued AFTER it went to sleep.
                    13 => if (self.tmp != 0) {
                        self.wpc = 4;
                    } else {
                        self.wakes_at_sleep = sh.wakes;
                        self.wpc = 14;
                    },
                    14 => if (sh.wakes > self.wakes_at_sleep) {
                        self.wpc = 4;
                    } else if (self.hpc == 3) {
                        self.stranded = true;
                    },
                    // Re-contend as queued.
                    4 => if (!sh.locked) {
                        sh.locked = true;
                        self.wpc = 5;
                    },
                    5 => {
                        sh.token[wid] = 0;
                        if (sh.grant(modes, .exclusive, true, weakening)) {
                            sh.remove(wid);
                            self.granted = true;
                        }
                        self.wpc = 6;
                    },
                    6 => {
                        sh.locked = false;
                        self.wpc = if (self.granted) 9 else 3;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.stranded) return error.ParkedWaiterStranded;
        }
        pub fn final(self: *const Self) !void {
            if (!self.granted) return error.WaiterNeverGranted;
            if (!self.sh.writer) return error.GrantWithoutHold;
            if (self.sh.qn != 0) return error.GhostQueueEntry;
        }
    };
}

// ---------------------------------------------------------------------------
// 2. the dead waiter
// ---------------------------------------------------------------------------

/// The wedge rail. The holder takes exclusive; T2 races one fresh attempt
/// and, if refused, registers and DIES (its token is dropped). The holder
/// releases, joins T2, then: if T2 won, a fresh attempt must fail
/// (exclusion) and after T2's release succeed (lock not lost); else a
/// fresh attempt must FAIL (it queues behind the corpse — FIFO courtesy) and
/// a QUEUED re-contend (the tick) must barge past the corpse — never a
/// wedge.
///
/// Weakening `.assign_on_release`: the release hands the lock to the dead
/// front waiter; the ticked re-contend finds it held forever.
fn DeadWaiter(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const main = 0;
        const t2 = 1;
        const modes = [max_waiters]Mode{ .exclusive, .exclusive, .exclusive, .exclusive };

        sh: Shared = .{ .writer = true },
        mpc: u8 = 0,
        tpc: u8 = 0,
        t2_won: bool = false,
        exclusion_broken: bool = false,
        lock_lost: bool = false,
        courtesy_broken: bool = false,
        wedged: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                main => self.mpc == 7,
                t2 => self.tpc == 3,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            const sh = &self.sh;
            switch (t) {
                t2 => switch (self.tpc) {
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.tpc = 1;
                    },
                    1 => {
                        if (sh.grant(modes, .exclusive, false, weakening)) self.t2_won = true else sh.push(1);
                        self.tpc = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.tpc = 3;
                    },
                    else => unreachable,
                },
                main => switch (self.mpc) {
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.mpc = 1;
                    },
                    1 => {
                        // Release: the tokens go to the floor (T2 never acts).
                        sh.writer = false;
                        if (weakening == .assign_on_release and sh.qn > 0) sh.writer = true;
                        sh.wakeFront(modes);
                        self.mpc = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.mpc = 3;
                    },
                    // join(T2)
                    3 => if (self.tpc == 3) {
                        self.mpc = 4;
                    },
                    4 => if (!sh.locked) {
                        sh.locked = true;
                        self.mpc = 5;
                    },
                    5 => {
                        if (self.t2_won) {
                            if (sh.grant(modes, .exclusive, false, weakening)) self.exclusion_broken = true;
                            // T2's release.
                            sh.writer = false;
                            sh.wakeFront(modes);
                            if (!sh.grant(modes, .exclusive, false, weakening)) self.lock_lost = true;
                        } else {
                            if (sh.grant(modes, .exclusive, false, weakening)) self.courtesy_broken = true;
                            sh.push(2);
                            if (!sh.grant(modes, .exclusive, true, weakening)) self.wedged = true else sh.remove(2);
                        }
                        self.mpc = 6;
                    },
                    6 => {
                        sh.locked = false;
                        self.mpc = 7;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.exclusion_broken) return error.TwoExclusiveHolders;
            if (self.lock_lost) return error.LockLostAfterRelease;
            if (self.courtesy_broken) return error.FreshAttemptBargedPastWaiter;
            if (self.wedged) return error.DeadWaiterWedgedTheLock;
        }
        pub fn final(_: *const Self) !void {}
    };
}

// ---------------------------------------------------------------------------
// 3. mutual exclusion
// ---------------------------------------------------------------------------

/// A holder releases while a fresh exclusive attempt and a fresh shared
/// attempt race it: never a writer beside readers, never two writers.
///
/// Weakening `.split_check_set`: both contenders read the free lock in one
/// hold and take it in another.
fn MutualExclusion(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const holder = 0;
        const w2 = 1;
        const r = 2;
        const modes = [max_waiters]Mode{ .exclusive, .exclusive, .shared, .shared };

        sh: Shared = .{ .writer = true },
        pc: [3]u8 = @splat(0),
        tmp: [3]bool = @splat(false),
        holder_in: bool = true,
        entered: [3]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                holder => self.pc[t] == 3,
                w2, r => self.pc[t] == 6,
                else => unreachable,
            };
        }
        fn contend(self: *Self, t: usize, mode: Mode) void {
            const sh = &self.sh;
            switch (self.pc[t]) {
                0, 3 => if (!sh.locked) {
                    sh.locked = true;
                    self.pc[t] += 1;
                },
                1 => {
                    if (weakening == .split_check_set) {
                        self.tmp[t] = !sh.writer and sh.qn == 0 and (mode == .shared or sh.readers == 0);
                    } else if (sh.grant(modes, mode, false, weakening)) {
                        self.entered[t] = true;
                    }
                    self.pc[t] = 2;
                },
                2 => {
                    sh.locked = false;
                    self.pc[t] = if (weakening == .split_check_set) 3 else 6;
                },
                4 => {
                    if (self.tmp[t]) {
                        switch (mode) {
                            .exclusive => sh.writer = true,
                            .shared => sh.readers += 1,
                        }
                        self.entered[t] = true;
                    }
                    self.pc[t] = 5;
                },
                5 => {
                    sh.locked = false;
                    self.pc[t] = 6;
                },
                else => unreachable,
            }
        }
        pub fn step(self: *Self, t: usize) void {
            const sh = &self.sh;
            switch (t) {
                holder => switch (self.pc[t]) {
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        sh.writer = false;
                        self.holder_in = false;
                        sh.wakeFront(modes);
                        self.pc[t] = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.pc[t] = 3;
                    },
                    else => unreachable,
                },
                w2 => self.contend(t, .exclusive),
                r => self.contend(t, .shared),
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            const writers: u8 = @as(u8, @intFromBool(self.holder_in)) + @intFromBool(self.entered[w2]);
            if (writers > 1) return error.TwoWriters;
            if (writers > 0 and self.entered[r]) return error.WriterBesideReader;
            if (self.sh.writer and self.sh.readers > 0) return error.LockWordInconsistent;
        }
        pub fn final(_: *const Self) !void {}
    };
}

// ---------------------------------------------------------------------------
// 4. writer preference
// ---------------------------------------------------------------------------

/// A reader holds and an exclusive waiter is parked behind it; a fresh
/// shared attempt arrives as the reader releases. The fresh reader must
/// never enter while the writer is still queued; the release wakes the
/// writer, the writer's release wakes the queued reader, and every thread
/// ends with the lock free.
///
/// Weakening `.no_write_preference`: the fresh shared attempt takes the
/// lock past the queued writer.
fn WriterPreference(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const r1 = 0;
        const r2 = 1;
        const e = 2;
        const eid: u8 = 1;
        const r2id: u8 = 2;
        const modes = [max_waiters]Mode{ .shared, .exclusive, .shared, .shared };

        sh: Shared,
        pc: [3]u8 = @splat(0),
        r2_granted: bool = false,
        r2_barged: bool = false,
        e_granted: bool = false,
        lost: bool = false,

        pub fn init() Self {
            var self: Self = .{ .sh = .{ .readers = 1 } };
            self.sh.push(eid);
            return self;
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                r1 => self.pc[t] == 3,
                r2 => self.pc[t] == 9,
                e => self.pc[t] == 7,
                else => unreachable,
            };
        }
        fn othersDone(self: *const Self, t: usize) bool {
            var i: usize = 0;
            while (i < thread_count) : (i += 1) {
                if (i != t and !self.done(i)) return false;
            }
            return true;
        }
        /// A shared release under the lock.
        fn releaseShared(sh: *Shared) void {
            sh.readers -= 1;
            if (sh.readers == 0) sh.wakeFront(modes);
        }
        pub fn step(self: *Self, t: usize) void {
            const sh = &self.sh;
            switch (t) {
                r1 => switch (self.pc[t]) {
                    0 => if (!sh.locked) {
                        sh.locked = true;
                        self.pc[t] = 1;
                    },
                    1 => {
                        releaseShared(sh);
                        self.pc[t] = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.pc[t] = 3;
                    },
                    else => unreachable,
                },
                r2 => switch (self.pc[t]) {
                    0, 4, 6 => if (!sh.locked) {
                        sh.locked = true;
                        self.pc[t] += 1;
                    },
                    1 => {
                        if (sh.grant(modes, .shared, false, weakening)) {
                            self.r2_granted = true;
                            if (sh.isQueued(eid)) self.r2_barged = true;
                        } else sh.push(r2id);
                        self.pc[t] = 2;
                    },
                    2 => {
                        sh.locked = false;
                        self.pc[t] = if (self.r2_granted) 6 else 3;
                    },
                    3 => if (sh.token[r2id] != 0) {
                        self.pc[t] = 4;
                    } else if (self.othersDone(t)) {
                        self.lost = true;
                    },
                    5 => {
                        sh.token[r2id] = 0;
                        if (sh.grant(modes, .shared, true, weakening)) {
                            sh.remove(r2id);
                            self.r2_granted = true;
                        }
                        sh.locked = false;
                        self.pc[t] = if (self.r2_granted) 6 else 3;
                    },
                    7 => {
                        releaseShared(sh);
                        self.pc[t] = 8;
                    },
                    8 => {
                        sh.locked = false;
                        self.pc[t] = 9;
                    },
                    else => unreachable,
                },
                e => switch (self.pc[t]) {
                    0 => if (sh.token[eid] != 0) {
                        self.pc[t] = 1;
                    } else if (self.othersDone(t)) {
                        self.lost = true;
                    },
                    1, 4 => if (!sh.locked) {
                        sh.locked = true;
                        self.pc[t] += 1;
                    },
                    2 => {
                        sh.token[eid] = 0;
                        if (sh.grant(modes, .exclusive, true, weakening)) {
                            sh.remove(eid);
                            self.e_granted = true;
                        }
                        self.pc[t] = 3;
                    },
                    3 => {
                        sh.locked = false;
                        self.pc[t] = if (self.e_granted) 4 else 0;
                    },
                    5 => {
                        sh.writer = false;
                        sh.wakeFront(modes);
                        self.pc[t] = 6;
                    },
                    6 => {
                        sh.locked = false;
                        self.pc[t] = 7;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.r2_barged) return error.FreshReaderEnteredPastQueuedWriter;
            if (self.lost) return error.LostWakeup;
            if (self.sh.writer and self.sh.readers > 0) return error.WriterBesideReader;
        }
        pub fn final(self: *const Self) !void {
            if (!self.e_granted or !self.r2_granted) return error.SomeoneNeverGranted;
            if (self.sh.writer or self.sh.readers != 0 or self.sh.qn != 0) return error.LockNotFreeAtEnd;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "sqz_sync a_parking_waiter_racing_release_is_never_stranded: holds; try-then-register split and check-then-sleep fail" {
    try explorer.expectHolds(ParkRacingRelease(.none), .{});
    try explorer.expectFails(ParkRacingRelease(.split_try_then_register), .{}, error.InvariantViolated);
    try explorer.expectFails(ParkRacingRelease(.check_then_sleep), .{}, error.InvariantViolated);
}

test "sqz_sync a_dead_waiter_never_wedges_and_exclusion_holds: holds; assigning ownership on release wedges" {
    try explorer.expectHolds(DeadWaiter(.none), .{});
    try explorer.expectFails(DeadWaiter(.assign_on_release), .{}, error.InvariantViolated);
}

test "sqz_sync shared_grant_never_observes_live_writer (mutual exclusion): holds; a split check/set fails" {
    try explorer.expectHolds(MutualExclusion(.none), .{});
    try explorer.expectFails(MutualExclusion(.split_check_set), .{}, error.InvariantViolated);
}

test "sqz_sync writer preference: a fresh reader never enters past a queued writer; holds; without the yield it fails" {
    try explorer.expectHolds(WriterPreference(.none), .{});
    try explorer.expectFails(WriterPreference(.no_write_preference), .{}, error.InvariantViolated);
}

// The state counts the evidence note records.
test "rwlock models: exhaustive within the explorer's default limits" {
    inline for (.{ ParkRacingRelease(.none), DeadWaiter(.none), MutualExclusion(.none), WriterPreference(.none) }) |Model| {
        var report: explorer.Report = undefined;
        try explorer.explore(Model, testing.allocator, .{}, &report);
        try testing.expect(report.states > 1);
        try testing.expect(report.states < 5000);
    }
}
