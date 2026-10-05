//! The metadata plane's reader-writer lock and its core (charter §9;
//! `docs/decisions/0001` item 3; the reference's `sqz_sync_core.rs` +
//! `sqz_sync.rs`), written over `std.Io`'s futex ops so it works under any
//! `Io`.
//!
//! The laws:
//!
//! 1. **No ownership assignment to sleeping waiters.** A release flips the
//!    lock FREE and wakes the queue front (every leading shared waiter, or
//!    one exclusive waiter) with a wake TOKEN; woken waiters re-contend. A
//!    waiter whose wake is lost owns nothing and blocks nobody — the
//!    assign-to-the-popped-waiter protocol wedged the world on a never-polled
//!    assignee (the OQ-5 class); this cannot.
//! 2. **Bounded barging** (the 2026-08-13 fairness fix). A FRESH attempt
//!    takes a free lock only when nobody is queued: exclusive yields to ANY
//!    queued waiter, shared yields to a queued exclusive waiter (write
//!    preference). A QUEUED waiter's re-contend barges unconditionally — so a
//!    dead front waiter costs contenders one tick, never a wedge. Without the
//!    bound a 100 %-duty release→relock writer reacquired inline nanoseconds
//!    after its release and starved the woken FIFO front for 40 minutes
//!    (`lock_ticked_reregisters` = 1351 in the rw5a capture).
//! 3. **Every unbounded park is a 2 s ticked wait** (`tick.zig`): on a tick
//!    the SAME waiter re-contends with its queue position intact and counts
//!    `lock_ticked_reregisters`.
//! 4. **Grant-or-enqueue is ONE critical section** (the park-racing-release
//!    fix): the retired try-then-register shape let a release land between
//!    the two — it saw an empty queue, woke nobody, flipped the lock free, and
//!    the waiter parked wakeless; under FIFO courtesy every fresh contender
//!    then queued behind the stranded front until its tick (the field's
//!    2 s-bucket `write_lock_wait_exclusive` stalls, amplified into a full
//!    convoy freeze). `rwlock_model.zig` proves the split shape fails.
//!
//! Shape: an interior short-hold `Io.Mutex` (the reference's interior
//! `std::sync::Mutex`, nanosecond critical sections, never held across a
//! park) guards the lock word and an intrusive FIFO of waiter nodes that
//! live on the acquirer's frame — nothing is allocated per operation. A
//! waiter parks on its own node's futex word. Tokens are stored AND the
//! futex woken under the interior mutex: a node is unlinked only by its
//! owner, under that mutex, so the waker never touches a node whose frame
//! has gone.

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;
const tick = @import("tick.zig");
const clock = @import("../core/clock.zig");

pub const Mode = enum(u8) { shared, exclusive };

/// One queued acquirer. Lives on the acquiring frame (or, for the dead-waiter
/// harness, wherever the test keeps it); never moves while linked.
pub const Waiter = struct {
    mode: Mode,
    next: ?*Waiter = null,
    prev: ?*Waiter = null,
    linked: bool = false,
    /// The park word: 0 = parked, 1 = a wake token is pending. Written under
    /// the interior mutex by both sides; read atomically by the futex wait.
    state: std.atomic.Value(u32) = .init(0),
};

pub const RwLock = struct {
    interior: Io.Mutex = .init,
    /// Readers inside (0 with `writer == false` is FREE).
    readers: u32 = 0,
    writer: bool = false,
    /// FIFO wait queue — wake order only, NEVER ownership.
    head: ?*Waiter = null,
    tail: ?*Waiter = null,
    queue_len: u32 = 0,
    /// Exclusive waiters in the queue: the write-preference test for a fresh
    /// shared attempt, kept as a count so the check is O(1).
    queued_writers: u32 = 0,

    pub const init: RwLock = .{};

    /// A held lock, movable by value (the conveyor's queue entries co-own
    /// guard sets): release exactly once.
    pub const Guard = struct {
        lock: *RwLock,
        mode: Mode,

        pub fn release(g: Guard, io: Io) void {
            switch (g.mode) {
                .shared => g.lock.unlockShared(io),
                .exclusive => g.lock.unlock(io),
            }
        }
    };

    // ---- exclusive ----

    pub fn lock(self: *RwLock, io: Io) Io.Cancelable!void {
        return self.acquire(io, .exclusive, true);
    }

    pub fn lockUncancelable(self: *RwLock, io: Io) void {
        self.acquire(io, .exclusive, false) catch |err| switch (err) {
            error.Canceled => unreachable,
        };
    }

    /// FIFO courtesy applies: refused while anyone is queued.
    pub fn tryLock(self: *RwLock, io: Io) bool {
        return self.tryAcquire(io, .exclusive);
    }

    pub fn unlock(self: *RwLock, io: Io) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        assert(self.writer);
        self.writer = false;
        self.wakeFrontLocked(io);
    }

    // ---- shared ----

    pub fn lockShared(self: *RwLock, io: Io) Io.Cancelable!void {
        return self.acquire(io, .shared, true);
    }

    pub fn lockSharedUncancelable(self: *RwLock, io: Io) void {
        self.acquire(io, .shared, false) catch |err| switch (err) {
            error.Canceled => unreachable,
        };
    }

    pub fn tryLockShared(self: *RwLock, io: Io) bool {
        return self.tryAcquire(io, .shared);
    }

    pub fn unlockShared(self: *RwLock, io: Io) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        assert(self.readers > 0);
        self.readers -= 1;
        if (self.readers == 0) self.wakeFrontLocked(io);
    }

    // ---- guard faces ----

    pub fn acquireGuard(self: *RwLock, io: Io, mode: Mode) Io.Cancelable!Guard {
        try self.acquire(io, mode, true);
        return .{ .lock = self, .mode = mode };
    }

    pub fn acquireGuardUncancelable(self: *RwLock, io: Io, mode: Mode) Guard {
        self.acquire(io, mode, false) catch |err| switch (err) {
            error.Canceled => unreachable,
        };
        return .{ .lock = self, .mode = mode };
    }

    pub fn tryAcquireGuard(self: *RwLock, io: Io, mode: Mode) ?Guard {
        if (!self.tryAcquire(io, mode)) return null;
        return .{ .lock = self, .mode = mode };
    }

    /// Parked acquirers — the deterministic "the writer is parked on this
    /// lock" observable tests use in place of a sleep.
    pub fn waiters(self: *RwLock, io: Io) u32 {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        return self.queue_len;
    }

    /// Link a waiter WITHOUT an acquisition attempt — the dead-waiter
    /// harness (a registered waiter that is never parked and never
    /// re-contends: the wake its releaser delivers dies on the floor).
    pub fn register(self: *RwLock, io: Io, w: *Waiter) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        self.pushLocked(w);
    }

    /// Unlink a registered waiter; a cancelled FRONT waiter may have absorbed
    /// a release's wake, so the new front is woken in its stead (a spurious
    /// wake costs one re-contend; a lost one would cost a tick).
    pub fn unregister(self: *RwLock, io: Io, w: *Waiter) void {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        const was_front = self.head == w;
        self.unlinkLocked(w);
        if (was_front) self.wakeFrontLocked(io);
    }

    // ---- the core ----

    /// One acquisition attempt under the interior mutex. `queued` is the
    /// barging switch: a queued re-contend takes any free lock; a fresh
    /// attempt yields (exclusive to any waiter, shared to a queued writer).
    fn grantLocked(self: *RwLock, mode: Mode, queued: bool) bool {
        switch (mode) {
            .shared => {
                const blocked_by_writer_wait = !queued and self.queued_writers > 0;
                if (!self.writer and !blocked_by_writer_wait) {
                    self.readers += 1;
                    return true;
                }
            },
            .exclusive => {
                if (!self.writer and self.readers == 0 and (queued or self.queue_len == 0)) {
                    self.writer = true;
                    return true;
                }
            },
        }
        return false;
    }

    fn tryAcquire(self: *RwLock, io: Io, mode: Mode) bool {
        self.interior.lockUncancelable(io);
        defer self.interior.unlock(io);
        if (!self.grantLocked(mode, false)) return false;
        if (mode == .shared) self.wakeLeadingSharedLocked(io);
        return true;
    }

    fn acquire(self: *RwLock, io: Io, mode: Mode, comptime cancelable: bool) Io.Cancelable!void {
        var w: Waiter = .{ .mode = mode };
        {
            // Grant-or-enqueue: ONE critical section (law 4).
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            if (self.grantLocked(mode, false)) {
                if (mode == .shared) self.wakeLeadingSharedLocked(io);
                return;
            }
            self.pushLocked(&w);
        }
        while (true) {
            if (cancelable) {
                _ = tick.park(io, &w.state, &tick.lock_ticked_reregisters) catch |err| switch (err) {
                    error.Canceled => {
                        self.unregister(io, &w);
                        return err;
                    },
                };
            } else {
                _ = tick.parkUncancelable(io, &w.state, &tick.lock_ticked_reregisters);
            }
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            // Consume the token under the mutex: a release that lands after
            // this unlock stores a fresh one the next park sees at once.
            w.state.store(0, .monotonic);
            if (self.grantLocked(mode, true)) {
                self.unlinkLocked(&w);
                // A granted shared acquire lets the contiguous leading shared
                // waiters through with it.
                if (mode == .shared) self.wakeLeadingSharedLocked(io);
                return;
            }
        }
    }

    /// Release's wake: one exclusive front waiter, or every leading shared
    /// waiter. Tokens only — ownership is taken on re-contend.
    fn wakeFrontLocked(self: *RwLock, io: Io) void {
        const front = self.head orelse return;
        if (front.mode == .exclusive) {
            tick.wake(io, &front.state);
            return;
        }
        self.wakeLeadingSharedLocked(io);
    }

    /// Every leading shared waiter (stops at the first exclusive one, which a
    /// shared grant cannot serve — it is woken by the last reader's release).
    fn wakeLeadingSharedLocked(self: *RwLock, io: Io) void {
        var cur = self.head;
        while (cur) |w| : (cur = w.next) {
            if (w.mode == .exclusive) return;
            tick.wake(io, &w.state);
        }
    }

    fn pushLocked(self: *RwLock, w: *Waiter) void {
        assert(!w.linked);
        w.next = null;
        w.prev = self.tail;
        if (self.tail) |t| t.next = w else self.head = w;
        self.tail = w;
        w.linked = true;
        self.queue_len += 1;
        if (w.mode == .exclusive) self.queued_writers += 1;
    }

    fn unlinkLocked(self: *RwLock, w: *Waiter) void {
        assert(w.linked);
        if (w.prev) |p| p.next = w.next else self.head = w.next;
        if (w.next) |n| n.prev = w.prev else self.tail = w.prev;
        w.next = null;
        w.prev = null;
        w.linked = false;
        self.queue_len -= 1;
        if (w.mode == .exclusive) self.queued_writers -= 1;
    }
};

// ---------------------------------------------------------------------------
// Tests: the reference's `sqz_sync` suite, under `std.testing.io`.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "rwlock_readers_share_writers_exclude: readers coexist, writers exclude, write preference holds" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lockShared(io);
    try l.lockShared(io);
    try testing.expect(!l.tryLock(io));
    l.unlockShared(io);
    l.unlockShared(io);
    try l.lock(io);
    try testing.expect(!l.tryLockShared(io));
    try testing.expect(!l.tryLock(io));
    l.unlock(io);
    try testing.expect(l.tryLockShared(io));
    l.unlockShared(io);
    try testing.expectEqual(RwLock.init, l);
}

test "a fresh exclusive attempt queues behind ANY waiter and a fresh shared attempt behind a queued writer (bounded barging)" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lockShared(io);
    var dead: Waiter = .{ .mode = .exclusive };
    l.register(io, &dead);
    // Readers hold; a queued writer exists: a fresh shared attempt yields
    // (write preference) and a fresh exclusive attempt is excluded anyway.
    try testing.expect(!l.tryLockShared(io));
    try testing.expect(!l.tryLock(io));
    l.unlockShared(io);
    // Lock FREE, writer queued: a fresh exclusive attempt still yields.
    try testing.expect(!l.tryLock(io));
    try testing.expect(!l.tryLockShared(io));
    l.unregister(io, &dead);
    try testing.expect(l.tryLock(io));
    l.unlock(io);
    // Only shared waiters queued: a fresh shared attempt is NOT blocked (they
    // are all going through together), a fresh exclusive attempt is.
    var dead_r: Waiter = .{ .mode = .shared };
    l.register(io, &dead_r);
    try testing.expect(l.tryLockShared(io));
    try testing.expect(!l.tryLock(io));
    l.unlockShared(io);
    l.unregister(io, &dead_r);
    try testing.expectEqual(RwLock.init, l);
}

test "release_wakes_leading_shared_or_one_exclusive: the FIFO wake shape, tokens only" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lock(io);
    var w1: Waiter = .{ .mode = .shared };
    var w2: Waiter = .{ .mode = .shared };
    var w3: Waiter = .{ .mode = .exclusive };
    var w4: Waiter = .{ .mode = .shared };
    l.register(io, &w1);
    l.register(io, &w2);
    l.register(io, &w3);
    l.register(io, &w4);
    l.unlock(io);
    // Every LEADING shared waiter holds a token; the trailing ones do not.
    try testing.expectEqual(@as(u32, 1), w1.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), w2.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), w3.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), w4.state.load(.monotonic));
    // The lock is FREE, not assigned: the woken waiters re-contend as queued
    // (they barge); a fresh exclusive attempt yields to the queue.
    try testing.expect(!l.tryLock(io));
    l.interior.lockUncancelable(io);
    try testing.expect(l.grantLocked(.shared, true));
    try testing.expect(l.grantLocked(.shared, true));
    l.unlinkLocked(&w1);
    l.unlinkLocked(&w2);
    l.interior.unlock(io);
    l.unlockShared(io);
    try testing.expectEqual(@as(u32, 0), w3.state.load(.monotonic));
    // The last reader wakes ONE exclusive waiter.
    l.unlockShared(io);
    try testing.expectEqual(@as(u32, 1), w3.state.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), w4.state.load(.monotonic));
    l.unregister(io, &w3);
    // Unregistering the front hands the wake to the next waiter.
    try testing.expectEqual(@as(u32, 1), w4.state.load(.monotonic));
    l.unregister(io, &w4);
    try testing.expectEqual(RwLock.init, l);
}

test "cancelling_the_front_waiter_hands_the_wake_to_the_next: a cancelled parked acquire unlinks and passes its token on" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lock(io);
    var next: Waiter = .{ .mode = .exclusive };
    const Parked = struct {
        fn run(lk: *RwLock) Io.Cancelable!void {
            try lk.lock(testing.io);
            lk.unlock(testing.io);
        }
    };
    var fut = io.concurrent(Parked.run, .{&l}) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return error.SkipZigTest,
    };
    // The task is queued behind the holder; the harness waiter behind it.
    while (l.waiters(io) == 0) try std.Thread.yield();
    l.register(io, &next);
    try testing.expectEqual(@as(u32, 2), l.waiters(io));
    try testing.expectEqual(error.Canceled, fut.cancel(io));
    // cancelled_acquire_unlinks: no ghost entry; the token moved to `next`.
    try testing.expectEqual(@as(u32, 1), l.waiters(io));
    try testing.expectEqual(@as(u32, 1), next.state.load(.monotonic));
    l.unregister(io, &next);
    l.unlock(io);
    try testing.expectEqual(RwLock.init, l);
}

test "a_dead_waiter_never_wedges_the_lock: a registered waiter that never re-contends costs a fresh contender one tick, never a wedge" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lock(io);
    var dead: Waiter = .{ .mode = .exclusive };
    l.register(io, &dead);
    // Release: the wake goes to the dead waiter and dies on the floor. The
    // lock is FREE (never assigned) but a fresh contender queues behind the
    // corpse (FIFO courtesy) and its tick re-contend barges past it.
    l.unlock(io);
    const before = tick.lock_ticked_reregisters.load();
    const t0 = clock.monotonicNs();
    try l.lock(io);
    const waited = clock.monotonicNs() - t0;
    l.unlock(io);
    try testing.expect(tick.lock_ticked_reregisters.load() > before);
    try testing.expect(waited >= tick.tick_ns);
    l.unregister(io, &dead);
    try testing.expectEqual(RwLock.init, l);
}

test "tick_backstop_reregisters_and_recovers: a waiter parked past a tick counts a recovery and still acquires on release" {
    const io = testing.io;
    var l: RwLock = .init;
    try l.lock(io);
    const before = tick.lock_ticked_reregisters.load();
    const Parked = struct {
        fn run(lk: *RwLock) void {
            lk.lockUncancelable(testing.io);
            lk.unlock(testing.io);
        }
    };
    const t = try std.Thread.spawn(.{}, Parked.run, .{&l});
    while (l.waiters(io) == 0) try std.Thread.yield();
    // The hold window is longer than a tick, so the parked waiter MUST tick
    // at least once before the release lets it in.
    try io.sleep(.fromNanoseconds(@intCast(tick.tick_ns + 200 * std.time.ns_per_ms)), .awake);
    try testing.expect(tick.lock_ticked_reregisters.load() > before);
    l.unlock(io);
    t.join();
    try testing.expectEqual(RwLock.init, l);
}

test "writer_storm_never_starves_a_waiter: a 100 %-duty release→relock writer cannot barge past queued waiters" {
    const Shared = struct {
        l: RwLock = .init,
        stop: std.atomic.Value(bool) = .init(false),
        acquired: std.atomic.Value(u32) = .init(0),
        fn storm(s: *@This()) void {
            while (!s.stop.load(.acquire)) {
                s.l.lockUncancelable(testing.io);
                std.Thread.yield() catch {};
                s.l.unlock(testing.io);
            }
        }
        fn waiter(s: *@This()) void {
            s.l.lockUncancelable(testing.io);
            _ = s.acquired.fetchAdd(1, .release);
            s.l.unlock(testing.io);
        }
    };
    var s: Shared = .{};
    const storm = try std.Thread.spawn(.{}, Shared.storm, .{&s});
    var ws: [4]std.Thread = undefined;
    for (&ws) |*w| w.* = try std.Thread.spawn(.{}, Shared.waiter, .{&s});
    const t0 = clock.monotonicNs();
    for (ws) |w| w.join();
    const waited = clock.monotonicNs() - t0;
    s.stop.store(true, .release);
    storm.join();
    try testing.expectEqual(@as(u32, 4), s.acquired.load(.acquire));
    // Every waiter got in via FIFO courtesy, not the tick backstop.
    try testing.expect(waited < tick.tick_ns);
}

test "owned_guards_work: guards acquire, exclude, travel by value and release" {
    const io = testing.io;
    var l: RwLock = .init;
    const w = try l.acquireGuard(io, .exclusive);
    try testing.expect(!l.tryLockShared(io));
    const moved = w;
    moved.release(io);
    const r = l.acquireGuardUncancelable(io, .shared);
    try testing.expect(l.tryAcquireGuard(io, .exclusive) == null);
    const r2 = l.tryAcquireGuard(io, .shared).?;
    r.release(io);
    r2.release(io);
    try testing.expectEqual(RwLock.init, l);
}

test "readers and writers under contention keep two words equal (exclusion across threads)" {
    const Runner = struct {
        l: RwLock = .init,
        a: usize = 0,
        b: usize = 0,
        reads: std.atomic.Value(usize) = .init(0),
        writes: usize = 0,
        const num_writes = 500;
        const num_reads = 2000;
        fn reader(r: *@This(), seed: usize) !void {
            var prng = std.Random.DefaultPrng.init(seed);
            const rnd = prng.random();
            while (true) {
                r.l.lockSharedUncancelable(testing.io);
                defer r.l.unlockShared(testing.io);
                if (r.reads.fetchAdd(1, .monotonic) >= num_reads) break;
                const a: *const volatile usize = &r.a;
                const b: *const volatile usize = &r.b;
                const old_a = a.*;
                if (rnd.boolean()) try std.Thread.yield();
                try testing.expectEqual(old_a, b.*);
            }
        }
        fn writer(r: *@This(), seed: usize) !void {
            var prng = std.Random.DefaultPrng.init(seed);
            const rnd = prng.random();
            while (true) {
                r.l.lockUncancelable(testing.io);
                defer r.l.unlock(testing.io);
                if (r.writes == num_writes) break;
                const a: *volatile usize = &r.a;
                const b: *volatile usize = &r.b;
                const v = rnd.int(usize);
                const old_a = a.*;
                a.* = v;
                if (rnd.boolean()) try std.Thread.yield();
                try testing.expectEqual(old_a, b.*);
                b.* = v;
                r.writes += 1;
            }
        }
    };
    var run: Runner = .{};
    var ws: [2]std.Thread = undefined;
    var rs: [4]std.Thread = undefined;
    for (&ws, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Runner.writer, .{ &run, i });
    for (&rs, 10..) |*t, i| t.* = try std.Thread.spawn(.{}, Runner.reader, .{ &run, i });
    for (ws) |t| t.join();
    for (rs) |t| t.join();
    try testing.expectEqual(@as(usize, Runner.num_writes), run.writes);
    try testing.expectEqual(RwLock.init, run.l);
}
