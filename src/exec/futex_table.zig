//! The fiber-aware futex (`docs/porting/exec.md` §3.5): how every primitive
//! in `src/sync/` parks a FIBER, not the lane thread, under `SqzIo`.
//!
//! `src/sync/` waits through `tick.park` → `io.futexWaitTimeout` and its
//! interior `Io.Mutex` through `futexWait/futexWake`. On a lane fiber the
//! wait registers the fiber in a bucket list (256 buckets, each a spin lock
//! and an intrusive list of fibers keyed by the word's address), re-reads
//! the word under the bucket lock (a waker's store precedes its bucket
//! scan, so a waiter either sees the new word or is linked when the waker
//! looks), parks, and on resume unlinks itself if still linked (a timeout
//! or a cancel woke it). Off a lane the wait is the raw `futex(2)` with the
//! kernel's own timeout.
//!
//! The raw-waiter Dekker: a raw waiter increments `raw_waiters` BEFORE its
//! kernel compare-and-wait; the waker stores the word (the primitive's
//! token) BEFORE it reads the count — both as RMWs on the count word, so one
//! of them observes the other: either the waker pays the kernel wake or the
//! raw waiter's compare sees the new word (`futex_table_model.zig`; the
//! weakening is counting after the wait). A kernel `FUTEX_WAKE` is paid
//! only when a raw waiter may exist: the common all-fiber case costs no
//! syscall per wake.
//!
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const assert = std.debug.assert;
const futex = @import("../sync/futex.zig");
const fiber_mod = @import("fiber.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/futex_table is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;

/// 256 buckets: the hot words (one interior mutex and one waiter node per
/// parked primitive, a few dozen live at once) spread so two unrelated
/// primitives rarely share a lock; a collision costs one spin, never
/// correctness. 256 × 24 B = 6 KiB.
pub const bucket_count = 256;

const SpinLock = struct {
    locked: std.atomic.Value(bool) = .init(false),

    fn acquire(self: *SpinLock) void {
        // acquire pairs with `release`'s release store: the list is visible.
        while (self.locked.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn release(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

const Bucket = struct {
    lock: SpinLock = .{},
    head: ?*Fiber = null,
    tail: ?*Fiber = null,

    fn link(b: *Bucket, f: *Fiber) void {
        assert(!f.futex_linked);
        f.futex_next = null;
        f.futex_prev = b.tail;
        if (b.tail) |t| t.futex_next = f else b.head = f;
        b.tail = f;
        // release pairs with `unlinkIfLinked`'s acquire load outside the lock.
        @atomicStore(bool, &f.futex_linked, true, .release);
    }

    fn unlink(b: *Bucket, f: *Fiber) void {
        assert(f.futex_linked);
        if (f.futex_prev) |p| p.futex_next = f.futex_next else b.head = f.futex_next;
        if (f.futex_next) |n| n.futex_prev = f.futex_prev else b.tail = f.futex_prev;
        f.futex_next = null;
        f.futex_prev = null;
        @atomicStore(bool, &f.futex_linked, false, .release);
    }
};

pub const FutexTable = struct {
    buckets: [bucket_count]Bucket = @splat(.{}),
    /// Raw (non-lane) threads inside a kernel wait or about to enter one.
    raw_waiters: std.atomic.Value(u32) = .init(0),

    pub const init: FutexTable = .{};

    fn bucketOf(self: *FutexTable, ptr: *const u32) *Bucket {
        // Fibonacci hashing of the word address (4-byte granules).
        const h = (@as(u64, @intFromPtr(ptr)) >> 2) *% 0x9E3779B97F4A7C15;
        return &self.buckets[@intCast(h >> (64 - 8))];
    }

    /// The fiber path. `deadline_ns` is monotonic (null = unbounded).
    /// `cancelable` makes this a cancellation point: a request that lands
    /// before or during the park returns `error.Canceled` once.
    pub fn fiberWait(self: *FutexTable, f: *Fiber, ptr: *const u32, expected: u32, deadline_ns: ?u64, comptime cancelable: bool) Io.Cancelable!void {
        if (cancelable) try f.checkCancel();
        const b = self.bucketOf(ptr);
        b.lock.acquire();
        // The re-read under the lock: the token stored before the waker's
        // scan is visible here (seq_cst load vs the primitive's release
        // store + the waker's bucket acquire).
        if (@atomicLoad(u32, ptr, .seq_cst) != expected) {
            b.lock.release();
            return;
        }
        f.futex_ptr = ptr;
        b.link(f);
        b.lock.release();
        // Registered: from here a cancel request wakes us.
        if (f.changeAwaiting(.nothing, .wakeable) and cancelable and f.protection.check() == .unblocked) {
            self.unlinkIfLinked(f);
            _ = f.changeAwaiting(.wakeable, .nothing);
            f.protection.acknowledged = true;
            return error.Canceled;
        }
        if (deadline_ns) |d| {
            f.lane.timers.arm(f, d);
            stats.timer_arms.inc();
        }
        f.park();
        if (deadline_ns != null) _ = f.lane.timers.cancel(f);
        self.unlinkIfLinked(f);
        const requested = f.changeAwaiting(.wakeable, .nothing);
        if (cancelable and requested and f.protection.check() == .unblocked) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
    }

    /// A timeout or a cancel woke the fiber while a waker had not unlinked
    /// it: take the bucket lock and leave.
    fn unlinkIfLinked(self: *FutexTable, f: *Fiber) void {
        // acquire pairs with the waker's release (the lock release after
        // its unlink): a false here means the waker already moved us.
        if (!@atomicLoad(bool, &f.futex_linked, .acquire)) return;
        const b = self.bucketOf(f.futex_ptr.?);
        b.lock.acquire();
        if (f.futex_linked) b.unlink(f);
        b.lock.release();
    }

    /// The raw-thread path: counted, then the kernel wait (`timeout_ns`
    /// relative, null = unbounded). seq_cst RMWs on the count pair with
    /// `wake`'s read (the Dekker, module doc).
    pub fn rawWait(self: *FutexTable, ptr: *const u32, expected: u32, timeout_ns: ?u64) void {
        _ = self.raw_waiters.fetchAdd(1, .seq_cst);
        defer _ = self.raw_waiters.fetchSub(1, .seq_cst);
        const word: *const futex.Word = @ptrCast(@alignCast(ptr));
        if (timeout_ns) |ns| {
            _ = futex.waitTimeout(word, expected, ns);
        } else {
            futex.wait(word, expected);
        }
    }

    /// Wake up to `max` waiters on `ptr`: matching fibers first (moved off
    /// the list under the lock, woken outside it — the enqueue may write an
    /// eventfd), then the kernel wake for the raw waiters iff any exist.
    pub fn wake(self: *FutexTable, ptr: *const u32, max: u32) void {
        if (max == 0) return;
        var remaining = max;
        var chain: ?*Fiber = null;
        const b = self.bucketOf(ptr);
        b.lock.acquire();
        var cur = b.head;
        while (cur) |f| {
            const next = f.futex_next;
            if (f.futex_ptr == ptr) {
                b.unlink(f);
                f.futex_next = chain;
                chain = f;
                remaining -= 1;
                if (remaining == 0) break;
            }
            cur = next;
        }
        b.lock.release();
        while (chain) |f| {
            chain = f.futex_next;
            f.futex_next = null;
            if (f.state.wake()) f.lane.enqueue(f);
        }
        if (remaining == 0) return;
        // The Dekker read: a seq_cst RMW (fetchAdd 0) is the fence AND the
        // load — it is totally ordered against every raw waiter's increment.
        if (self.raw_waiters.fetchAdd(0, .seq_cst) != 0) {
            futex.wake(@ptrCast(@alignCast(ptr)), remaining);
        }
    }

    /// Raw waiters counted right now (tests).
    pub fn rawWaiters(self: *const FutexTable) u32 {
        return self.raw_waiters.load(.seq_cst);
    }
};

const testing = std.testing;

test "raw waiters: a wake from another thread ends a counted wait; expected mismatch returns at once; a timed wait expires; the count returns to 0" {
    var t: FutexTable = .init;
    var word: futex.Word = .init(0);
    t.rawWait(&word.raw, 1, null);
    try testing.expectEqual(@as(u32, 0), t.rawWaiters());
    const Waker = struct {
        fn run(tab: *FutexTable, w: *futex.Word) void {
            while (tab.rawWaiters() == 0) std.Thread.yield() catch {};
            w.store(1, .release);
            tab.wake(&w.raw, 1);
        }
    };
    const th = try std.Thread.spawn(.{}, Waker.run, .{ &t, &word });
    while (word.load(.acquire) == 0) t.rawWait(&word.raw, 0, null);
    th.join();
    try testing.expectEqual(@as(u32, 0), t.rawWaiters());
    word.store(0, .monotonic);
    t.rawWait(&word.raw, 0, 2 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u32, 0), t.rawWaiters());
    // A wake with no waiters pays nothing and is harmless.
    t.wake(&word.raw, std.math.maxInt(u32));
    t.wake(&word.raw, 0);
}
