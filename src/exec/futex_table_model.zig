//! Explorer model of the fiber-aware futex (`futex_table.zig` §3.5): one
//! fiber waiter (parks in a bucket list under the bucket lock), one raw
//! thread waiter (counts itself in `raw_waiters`, then the kernel's
//! compare-and-sleep), one waker (stores the word, wakes the bucket's
//! fibers, then `fence; load(raw_waiters)` decides whether the kernel wake
//! is paid).
//!
//! The Dekker pair: the raw waiter's increment precedes its kernel compare;
//! the waker's word store precedes its count read; both RMW the count, so
//! either the waker sees the count or the raw waiter's kernel compare sees
//! the new word (EAGAIN).
//!
//! Weakening ledger:
//!   `.count_after_wait` — the raw waiter increments the count AFTER the
//!     kernel wait (the increment step follows the sleep): the waker reads
//!     0, pays no kernel wake, and the raw waiter sleeps forever.
//!
//! The second model (`DrivenOwnerModel`) is the bounded wake — `wake(ptr, 1)`,
//! a mutex's release — when the raw waiter is the OWNER of the driven lane
//! the fiber waiter lives on (the FUSE drain worker, off a fiber in a probe,
//! behind a handler fiber of its own lane on the same word): the fiber can
//! resume only once the owner is back in its pass, so a wake handed to the
//! fiber alone leaves both asleep on a FREE word for ever
//! (`docs/evidence/rung-10-mdtest-wedge.md`). The law: a counted raw waiter
//! is kernel-woken whatever fibers took of the count.
//!
//!   `.fiber_takes_the_raw_wake` — the kernel wake is skipped once a fiber
//!     consumed the count (the pre-fix `remaining == 0` return): the owner
//!     sleeps forever and the fiber it would run with it.
//!
//! The third model (`ChainWalkModel`) is the wake list against a STRAY wake
//! (a timer pop, a late waker): the waker takes a fiber off the list under
//! the bucket lock while a stray wake has it running; the fiber leaves its
//! wait and re-parks — and a waiter queues behind it — while the wake is
//! still in flight. The law: after the lock a waker touches no fiber's link.
//! The product shape (`.batch`) collects the taken fibers under the lock and
//! wakes each after the release; a late wake on a fiber that re-parked is a
//! spurious wake of its new wait, and its resume takes it off the new bucket
//! (`LinkedOutsideWait` otherwise). A wake-all larger than the batch rescans
//! from the head for the matches its first scan counted, never more (a
//! fiber that re-parks behind them is not chased). `.cas_under_lock` (the
//! wake flipped under the lock, only won fibers carried out) holds too.
//!
//!   `.links_after_release` — the pre-fix shape: the taken fibers ride
//!     `futex_next` and are woken after the release, so the walk reads and
//!     clears the link of a fiber that re-parked: the bucket's list is torn
//!     (`TornBucketList`), and with the shape check off the waiter queued
//!     behind it is never found by its own release (`WaiterStranded` — the
//!     pass-recall wedge, `docs/evidence/rung-10-journal-full-wedge.md`).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = struct {
    count_after_wait: bool = false,
};

fn FutexModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// 0 = the fiber waiter, 1 = the raw waiter, 2 = the waker.
        pub const thread_count: usize = 3;
        const fiber = 0;
        const raw = 1;
        const waker = 2;

        word: u8 = 0,
        bucket_locked: bool = false,
        fiber_linked: bool = false,
        raw_count: u8 = 0,
        fiber_sleeping: bool = false,
        fiber_woken: bool = false,
        raw_sleeping: bool = false,
        raw_woken: bool = false,
        fpc: u8 = 0,
        rpc: u8 = 0,
        wpc: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                fiber => self.fpc == 9,
                raw => self.rpc == 9,
                waker => self.wpc == 9,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                fiber => switch (self.fpc) {
                    0 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.fpc = 1;
                    },
                    // Under the lock: re-read the word, else link.
                    1 => {
                        if (self.word != 0) {
                            self.fpc = 8;
                        } else {
                            self.fiber_linked = true;
                            self.fpc = 2;
                        }
                    },
                    2 => {
                        self.bucket_locked = false;
                        self.fpc = 3;
                    },
                    // park: the delivery word absorbs a wake that already landed
                    3 => if (self.fiber_woken) {
                        self.fpc = 9;
                    } else {
                        self.fiber_sleeping = true;
                    },
                    // the word changed under the lock: return at once
                    8 => {
                        self.bucket_locked = false;
                        self.fpc = 9;
                    },
                    else => unreachable,
                },
                raw => switch (self.rpc) {
                    0 => {
                        if (weakening.count_after_wait) {
                            self.rpc = 1;
                        } else {
                            self.raw_count += 1;
                            self.rpc = 1;
                        }
                    },
                    // the kernel's compare-and-sleep (one atomic step); a
                    // sleeper leaves only through a kernel wake
                    1 => {
                        if (self.raw_sleeping) {
                            if (self.raw_woken) {
                                self.raw_sleeping = false;
                                self.rpc = 2;
                            }
                        } else if (self.word != 0) {
                            // EAGAIN: the word already changed
                            self.rpc = 2;
                        } else {
                            self.raw_sleeping = true;
                        }
                    },
                    // after the wait: the decrement (WEAKENED: the increment
                    // only lands here, after the wait returned — so a sleeper
                    // was never counted)
                    2 => {
                        if (weakening.count_after_wait) self.raw_count += 1;
                        self.raw_count -= 1;
                        self.rpc = 9;
                    },
                    else => unreachable,
                },
                waker => switch (self.wpc) {
                    // the primitive's store (the token)
                    0 => {
                        self.word = 1;
                        self.wpc = 1;
                    },
                    1 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.wpc = 2;
                    },
                    // move the matching fibers off the list (their wake)
                    2 => {
                        if (self.fiber_linked) {
                            self.fiber_linked = false;
                            self.fiber_woken = true;
                        }
                        self.bucket_locked = false;
                        self.wpc = 3;
                    },
                    // fence + load(raw_waiters): pay the kernel wake iff counted
                    3 => {
                        if (self.raw_count != 0) self.raw_woken = true;
                        self.wpc = 9;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // The waker is gone: a sleeper nobody woke sleeps forever.
            if (self.wpc == 9) {
                if (self.fiber_sleeping and !self.fiber_woken) return error.FiberSleepsForever;
                if (self.raw_sleeping and !self.raw_woken) return error.RawWaiterSleepsForever;
            }
        }
        pub fn final(self: *const Self) !void {
            if (self.fpc != 9 or self.rpc != 9) return error.WaiterNeverReturned;
        }
    };
}

const DrivenWeakening = struct {
    fiber_takes_the_raw_wake: bool = false,
};

/// One fiber waiter on a DRIVEN lane, its owner thread as the raw waiter
/// on the same word, one waker with a count of ONE. The owner reaches its
/// own wait only after the fiber switched out (they share the thread), and
/// the fiber resumes only while the owner is awake (its pass pumps the lane).
fn DrivenOwnerModel(comptime weakening: DrivenWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const fiber = 0;
        const owner = 1;
        const waker = 2;

        word: u8 = 0,
        bucket_locked: bool = false,
        fiber_linked: bool = false,
        raw_count: u8 = 0,
        fiber_sleeping: bool = false,
        fiber_woken: bool = false,
        owner_sleeping: bool = false,
        owner_woken: bool = false,
        fpc: u8 = 0,
        opc: u8 = 0,
        wpc: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                fiber => self.fpc == 9,
                owner => self.opc == 9,
                waker => self.wpc == 9,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                fiber => switch (self.fpc) {
                    0 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.fpc = 1;
                    },
                    1 => {
                        if (self.word != 0) {
                            self.fpc = 8;
                        } else {
                            self.fiber_linked = true;
                            self.fpc = 2;
                        }
                    },
                    2 => {
                        self.bucket_locked = false;
                        self.fpc = 3;
                    },
                    // park: switch out to the owner's pass (a wake that
                    // already landed is absorbed by the delivery word)
                    3 => if (self.fiber_woken) {
                        self.fpc = 9;
                    } else {
                        self.fiber_sleeping = true;
                        self.fpc = 4;
                    },
                    // resume: the owner's pass runs the enqueued fiber —
                    // only while the owner is not asleep in a kernel wait
                    4 => if (self.fiber_woken and !self.owner_sleeping) {
                        self.fiber_sleeping = false;
                        self.fpc = 9;
                    },
                    8 => {
                        self.bucket_locked = false;
                        self.fpc = 9;
                    },
                    else => unreachable,
                },
                owner => switch (self.opc) {
                    // the pass continues once the fiber switched out (or
                    // never parked): the probe's own contended acquire
                    0 => if (self.fiber_sleeping or self.fpc == 9) {
                        self.raw_count += 1;
                        self.opc = 1;
                    },
                    1 => {
                        if (self.owner_sleeping) {
                            if (self.owner_woken) {
                                self.owner_sleeping = false;
                                self.opc = 2;
                            }
                        } else if (self.word != 0) {
                            self.opc = 2;
                        } else {
                            self.owner_sleeping = true;
                        }
                    },
                    2 => {
                        self.raw_count -= 1;
                        self.opc = 9;
                    },
                    else => unreachable,
                },
                waker => switch (self.wpc) {
                    0 => {
                        self.word = 1;
                        self.wpc = 1;
                    },
                    1 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.wpc = 2;
                    },
                    // count ONE: the first matching fiber takes it
                    2 => {
                        if (self.fiber_linked) {
                            self.fiber_linked = false;
                            self.fiber_woken = true;
                            self.wpc = if (weakening.fiber_takes_the_raw_wake) 9 else 3;
                        } else {
                            self.wpc = 3;
                        }
                        self.bucket_locked = false;
                    },
                    // fence + load(raw_waiters): the kernel wake for a
                    // counted raw waiter, whatever the fibers took
                    3 => {
                        if (self.raw_count != 0) self.owner_woken = true;
                        self.wpc = 9;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            if (self.wpc == 9) {
                if (self.fiber_sleeping and !self.fiber_woken) return error.FiberSleepsForever;
                if (self.owner_sleeping and !self.owner_woken) return error.OwnerSleepsForever;
            }
        }
        pub fn final(self: *const Self) !void {
            if (self.fpc != 9 or self.opc != 9) return error.WaiterNeverReturned;
        }
    };
}

const ChainShape = enum {
    /// The product: the taken fibers collected under the lock, each woken
    /// (`state.wake()`, the winner enqueuing) after the release.
    batch,
    /// The delivery words flipped under the lock; only the fibers whose
    /// IDLE→SCHEDULED the wake won carried out to the enqueue.
    cas_under_lock,
    /// The weakening — the pre-fix wake: the taken fibers threaded through
    /// `futex_next` and woken AFTER the release, the walk reading and
    /// clearing the link of a fiber a stray wake may have resumed and
    /// re-parked.
    links_after_release,
};

const ChainConfig = struct {
    shape: ChainShape = .batch,
    /// `.batch`'s capacity (`futex_table.wake_batch`): 1 makes Q's wake-all
    /// rescan for its second waiter (the overflow path).
    batch_cap: u2 = 2,
    /// Check the bucket's shape whenever its lock is free. Off, the
    /// weakened model must still fail — on the stranded waiter alone.
    check_shape: bool = true,
    /// A probe, not a weakening: fail once P's wake lands late on f while f
    /// is linked on Q — the interleaving the batch shape must survive is
    /// one the explorer reaches.
    probe_late_wake_on_reparked: bool = false,
    /// The resume path's weakening: a resumed fiber skips `unlinkIfLinked`
    /// — the `LinkedOutsideWait` check must catch it.
    resume_keeps_link: bool = false,
};

/// The wake list against a stray wake. Fiber f is parked on word P and
/// linked; fiber g is about to wait on word Q; waker A is P's release
/// (`wake(P, 1)`); waker B is Q's (store, then `wake(Q, all)`); S is one
/// stray wake of f (a timer pop, a late waker — the futex contract allows
/// spurious wakes). Both waits share ONE bucket (the worst case; a walk
/// that leaves its own bucket tears another the same way). Each fiber
/// loops: link on Q unless Q's word is set, park (`endPollPending`),
/// resume when queued, unlink itself if still linked, re-check.
///
/// The law: after the bucket lock a waker touches no fiber's link. A
/// fiber's link is written only under the lock, and a fiber is linked only
/// inside a wait (`LinkedOutsideWait`).
fn ChainWalkModel(comptime config: ChainConfig) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 5;
        const ff = 0;
        const fg = 1;
        const wa = 2;
        const wb = 3;
        const ws = 4;
        const P: u1 = 0;
        const Q: u1 = 1;
        /// `task_state.zig`'s delivery word (COMPLETE never arises here).
        const St = enum(u2) { idle, scheduled, running, notified };
        /// Node ids: 0 = none, 1 = f, 2 = g.
        const Id = u2;

        head: Id = 1,
        tail: Id = 1,
        next: [2]Id = .{ 0, 0 },
        prev: [2]Id = .{ 0, 0 },
        linked: [2]bool = .{ true, false },
        ptr: [2]u1 = .{ P, Q },
        st: [2]St = .{ .idle, .running },
        queued: [2]bool = .{ false, false },
        q_word: bool = false,
        /// Fiber pcs: 0 wait on Q (locked), 1 park, 2 resume, 3 unlink-if-linked, 9 done.
        fpc: [2]u8 = .{ 2, 0 },
        /// Waker pcs (A, B): 0 store (B), 1 scan (locked), 2.. enqueue / walk, 9 done.
        wpc: [2]u8 = .{ 1, 0 },
        /// `.batch`: the fibers a scan took (each woken after the lock);
        /// `.cas_under_lock`: the ones whose wake it won.
        owned: [2][2]bool = .{ .{ false, false }, .{ false, false } },
        /// `.batch`: the matches a waker still owes (A wakes 1, B all 2);
        /// each scan leaves only the matches it counted behind its batch.
        remaining: [2]u2 = .{ 1, 2 },
        cur: [2]Id = .{ 0, 0 },
        nxt: [2]Id = .{ 0, 0 },
        hops: [2]u2 = .{ 0, 0 },
        spc: u8 = 0,
        late_on_reparked: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                ff, fg => self.fpc[t] == 9,
                wa, wb => self.wpc[t - wa] == 9,
                ws => self.spc == 9,
                else => unreachable,
            };
        }

        fn wakeState(self: *Self, i: usize) bool {
            switch (self.st[i]) {
                .idle => {
                    self.st[i] = .scheduled;
                    return true;
                },
                .running => self.st[i] = .notified,
                .scheduled, .notified => {},
            }
            return false;
        }
        fn link(self: *Self, i: usize) void {
            const id: Id = @intCast(i + 1);
            self.next[i] = 0;
            self.prev[i] = self.tail;
            if (self.tail != 0) self.next[self.tail - 1] = id else self.head = id;
            self.tail = id;
            self.linked[i] = true;
            self.ptr[i] = Q;
        }
        fn unlink(self: *Self, i: usize) void {
            const p = self.prev[i];
            const n = self.next[i];
            if (p != 0) self.next[p - 1] = n else self.head = n;
            if (n != 0) self.prev[n - 1] = p else self.tail = p;
            self.next[i] = 0;
            self.prev[i] = 0;
            self.linked[i] = false;
        }

        fn fiberStep(self: *Self, i: usize) void {
            switch (self.fpc[i]) {
                // fiberWait's registration under the lock (one step, as
                // every locked section here): re-read the word, else link.
                0 => {
                    if (self.q_word) {
                        self.fpc[i] = 9;
                    } else {
                        self.link(i);
                        self.fpc[i] = 1;
                    }
                },
                // park: the runner's endPollPending (a wake that landed
                // mid-poll requeues at once)
                1 => {
                    if (self.st[i] == .notified) {
                        self.st[i] = .scheduled;
                        self.queued[i] = true;
                    } else {
                        self.st[i] = .idle;
                    }
                    self.fpc[i] = 2;
                },
                // the lane runs a queued fiber
                2 => if (self.queued[i] and self.st[i] == .scheduled) {
                    self.queued[i] = false;
                    self.st[i] = .running;
                    self.fpc[i] = 3;
                },
                // unlinkIfLinked, then the caller re-checks its predicate
                3 => {
                    if (!config.resume_keeps_link and self.linked[i]) self.unlink(i);
                    self.fpc[i] = 0;
                },
                else => unreachable,
            }
        }

        /// `.batch`'s scan under the lock: take up to `batch_cap` matches,
        /// count the matches behind them up to what the wake still owes, and
        /// owe the next pass only those (the first scan's snapshot bounds
        /// the wake: a fiber that re-parks behind it is not chased).
        fn scanBatch(self: *Self, w: usize, target: u1) void {
            var taken: u2 = 0;
            var left: u2 = 0;
            var c = self.head;
            var hops: u8 = 0;
            while (c != 0 and hops < 4) : (hops += 1) {
                const i: usize = c - 1;
                const after = self.next[i];
                if (self.ptr[i] == target) {
                    if (taken < config.batch_cap) {
                        self.unlink(i);
                        self.owned[w][i] = true;
                        taken += 1;
                    } else {
                        left += 1;
                    }
                    if (taken + left == self.remaining[w]) break;
                }
                c = after;
            }
            self.remaining[w] = left;
            self.wpc[w] = 2;
        }

        /// The one-pass scan of `.cas_under_lock` and the weakening.
        fn scanOnce(self: *Self, w: usize, target: u1, max: u2) void {
            var taken: u2 = 0;
            var chain: Id = 0;
            var c = self.head;
            var hops: u8 = 0;
            while (c != 0 and taken < max and hops < 4) : (hops += 1) {
                const i: usize = c - 1;
                const after = self.next[i];
                if (self.ptr[i] == target) {
                    self.unlink(i);
                    if (config.shape == .links_after_release) {
                        self.next[i] = chain;
                        chain = c;
                    } else if (self.wakeState(i)) {
                        self.owned[w][i] = true;
                    }
                    taken += 1;
                }
                c = after;
            }
            self.cur[w] = chain;
            self.wpc[w] = 2;
        }

        fn wakerStep(self: *Self, w: usize) void {
            const target: u1 = if (w == 0) P else Q;
            const max: u2 = if (w == 0) 1 else 2;
            switch (self.wpc[w]) {
                // the primitive's store (B only)
                0 => {
                    self.q_word = true;
                    self.wpc[w] = 1;
                },
                // the scan under the lock (acquire .. release is one step:
                // every other list access holds the lock, save the
                // weakened walk below)
                1 => switch (config.shape) {
                    .batch => self.scanBatch(w, target),
                    .cas_under_lock, .links_after_release => self.scanOnce(w, target, max),
                },
                2 => switch (config.shape) {
                    // one taken fiber's wake after the lock (the winner
                    // enqueues; a fiber a stray wake resumed takes it as a
                    // spurious wake), then a pass for what the scan counted
                    .batch => {
                        for (0..2) |i| if (self.owned[w][i]) {
                            self.owned[w][i] = false;
                            if (w == 0 and i == ff and self.linked[ff] and self.ptr[ff] == Q) self.late_on_reparked = true;
                            if (self.wakeState(i)) self.queued[i] = true;
                            return;
                        };
                        self.wpc[w] = if (self.remaining[w] > 0) 1 else 9;
                    },
                    // the owned fibers' enqueue
                    .cas_under_lock => {
                        for (0..2) |i| if (self.owned[w][i]) {
                            self.owned[w][i] = false;
                            self.queued[i] = true;
                        };
                        self.wpc[w] = 9;
                    },
                    .links_after_release => if (self.cur[w] == 0) {
                        self.wpc[w] = 9;
                    } else {
                        // the weakened walk: read the link …
                        self.nxt[w] = self.next[self.cur[w] - 1];
                        self.wpc[w] = 3;
                    },
                },
                // … clear it …
                3 => {
                    self.next[self.cur[w] - 1] = 0;
                    self.wpc[w] = 4;
                },
                // … and wake
                4 => {
                    if (self.wakeState(self.cur[w] - 1)) self.queued[self.cur[w] - 1] = true;
                    self.cur[w] = self.nxt[w];
                    self.hops[w] += 1;
                    self.wpc[w] = if (self.cur[w] != 0 and self.hops[w] < 3) 2 else 9;
                },
                else => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                ff, fg => self.fiberStep(t),
                wa, wb => self.wakerStep(t - wa),
                ws => {
                    if (self.wakeState(ff)) self.queued[ff] = true;
                    self.spc = 9;
                },
                else => unreachable,
            }
        }

        fn shapeHolds(self: *const Self) bool {
            var prev: Id = 0;
            var c = self.head;
            var seen: [2]bool = .{ false, false };
            var hops: u8 = 0;
            while (c != 0) : (hops += 1) {
                if (hops >= 2) return false;
                const i: usize = c - 1;
                if (self.prev[i] != prev or !self.linked[i] or seen[i]) return false;
                seen[i] = true;
                prev = c;
                c = self.next[i];
            }
            if (prev != self.tail) return false;
            for (0..2) |i| if (self.linked[i] and !seen[i]) return false;
            return true;
        }

        pub fn invariant(self: *const Self) !void {
            if (config.check_shape and !self.shapeHolds()) return error.TornBucketList;
            // A fiber is linked only inside a wait: a resume — a late wake
            // on a fiber that re-parked included — leaves the bucket before
            // the caller re-checks its predicate.
            for (0..2) |i| {
                if ((self.fpc[i] == 0 or self.fpc[i] == 9) and self.linked[i]) return error.LinkedOutsideWait;
            }
            if (config.probe_late_wake_on_reparked and self.late_on_reparked) return error.LateWakeOnReparked;
            // Every waker gone: a parked fiber nobody queued sleeps forever.
            if (self.wpc[0] == 9 and self.wpc[1] == 9 and self.spc == 9) {
                for (0..2) |i| {
                    if (self.fpc[i] != 9 and self.st[i] == .idle and !self.queued[i]) return error.WaiterStranded;
                }
            }
        }
        pub fn final(self: *const Self) !void {
            if (self.fpc[ff] != 9 or self.fpc[fg] != 9) return error.WaiterNeverReturned;
        }
    };
}

const testing = std.testing;

test "the fiber/raw futex Dekker holds; counting the raw waiter after its kernel wait sleeps it forever" {
    try explorer.expectHolds(FutexModel(.{}), .{});
    try explorer.expectFails(FutexModel(.{ .count_after_wait = true }), .{}, error.InvariantViolated);
}

test "a count-one wake reaches a counted raw waiter that drives the woken fiber's lane; a wake the fiber takes alone sleeps the owner — and the fiber — for ever" {
    try explorer.expectHolds(DrivenOwnerModel(.{}), .{});
    try explorer.expectFails(DrivenOwnerModel(.{ .fiber_takes_the_raw_wake = true }), .{}, error.InvariantViolated);
}

test "futex table model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(FutexModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 500);
}

test "a wake touches no fiber's link after the bucket lock: the batch — taken under the lock, woken after it — holds against a stray-woken fiber that re-parks behind the wake with a waiter queued behind it (the late wake there is a spurious one its resume survives), at a batch of two and of one (the overflow pass); the wake flipped under the lock holds too; the pre-fix walk after the release tears the list and strands the waiter" {
    try explorer.expectHolds(ChainWalkModel(.{}), .{});
    try explorer.expectHolds(ChainWalkModel(.{ .batch_cap = 1 }), .{});
    try explorer.expectHolds(ChainWalkModel(.{ .shape = .cas_under_lock }), .{});
    // The late wake on the re-parked fiber is reached, and a resume that
    // kept its link would be caught.
    try explorer.expectFails(ChainWalkModel(.{ .probe_late_wake_on_reparked = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(ChainWalkModel(.{ .resume_keeps_link = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(ChainWalkModel(.{ .shape = .links_after_release }), .{}, error.InvariantViolated);
    try explorer.expectFails(ChainWalkModel(.{ .shape = .links_after_release, .check_shape = false }), .{}, error.InvariantViolated);
}
