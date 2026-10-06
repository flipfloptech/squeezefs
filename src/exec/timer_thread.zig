//! The ONE `sqz-timer` thread (charter §9, amended 2026-10-06; decision 0003
//! §D Q-B): a binary heap + a futex condvar + cancel-safe tombstones,
//! serving the NON-LANE contexts only — a raw thread's `sleep` and the
//! blocking pool's idle reap. Lane fibers never come here: their timers live
//! on their lane's own heap (`timer_heap.zig`), so a parked lane is woken
//! only by its ring (the reference's global timer paid a lock + a cross-thread
//! hop on every ticked park: 0.38 arms per kernel READ, `exec.md` §1.8).
//!
//! Tombstones: `cancel(id)` removes the id from the LIVE map and leaves the
//! heap entry in place; the service pops it later and skips it
//! (`exec_timer_thread_tombstones`). The fire STORES the sleeper's word
//! under the lock (so a `cancel` that lost the race returns false only
//! after the store landed and the sleeper's frame may go), and pays the
//! futex wake outside it.
//!
//! Linux only (raw futex).

const std = @import("std");
const builtin = @import("builtin");
const assert = std.debug.assert;
const futex = @import("../sync/futex.zig");
const clock = @import("../core/clock.zig");
const thread_name = @import("../core/thread_name.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/timer_thread is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const comm = "sqz-timer";

const Entry = struct { deadline_ns: u64, id: u64 };

fn earlier(_: void, a: Entry, b: Entry) std.math.Order {
    return std.math.order(a.deadline_ns, b.deadline_ns);
}

const SpinLock = struct {
    locked: std.atomic.Value(bool) = .init(false),
    fn acquire(self: *SpinLock) void {
        while (self.locked.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn release(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

pub const TimerThread = struct {
    gpa: std.mem.Allocator,
    lock: SpinLock = .{},
    heap: std.PriorityQueue(Entry, void, earlier),
    /// Live registrations: id → the sleeper's word. An id absent at pop is a
    /// tombstone.
    live: std.AutoHashMapUnmanaged(u64, *futex.Word) = .empty,
    next_id: u64 = 1,
    /// The condvar word: bumped (and woken) when a new earliest deadline
    /// lands or the thread must stop.
    seq: futex.Word = .init(0),
    stopping: std.atomic.Value(bool) = .init(false),
    thread: ?std.Thread = null,

    pub fn init(gpa: std.mem.Allocator) *TimerThread {
        const self = gpa.create(TimerThread) catch @panic("exec: out of memory creating the timer thread");
        self.* = .{ .gpa = gpa, .heap = .initContext({}) };
        return self;
    }

    pub fn start(self: *TimerThread) std.Thread.SpawnError!void {
        assert(self.thread == null);
        self.thread = try std.Thread.spawn(.{}, serviceLoop, .{self});
    }

    /// Stop, join, free. Every registration must be gone (a sleeper still
    /// parked would never wake).
    pub fn deinit(self: *TimerThread) void {
        if (self.thread) |t| {
            self.stopping.store(true, .release);
            self.bump();
            t.join();
        }
        self.live.deinit(self.gpa);
        self.heap.deinit(self.gpa);
        self.gpa.destroy(self);
    }

    fn bump(self: *TimerThread) void {
        _ = self.seq.fetchAdd(1, .release);
        futex.wakeOne(&self.seq);
    }

    /// Register: at `deadline_ns` (monotonic) the service stores 1 into
    /// `word` and wakes it. Returns the id for `cancel`.
    pub fn arm(self: *TimerThread, deadline_ns: u64, word: *futex.Word) u64 {
        self.lock.acquire();
        const id = self.next_id;
        self.next_id += 1;
        self.live.put(self.gpa, id, word) catch @panic("exec: out of memory arming a timer");
        const was_earliest = if (self.heap.peek()) |top| deadline_ns < top.deadline_ns else true;
        self.heap.push(self.gpa, .{ .deadline_ns = deadline_ns, .id = id }) catch @panic("exec: out of memory arming a timer");
        self.lock.release();
        // Only a new earliest deadline shortens the service's wait.
        if (was_earliest) self.bump();
        return id;
    }

    /// Cancel: true ⇔ removed before it fired (the heap entry becomes a
    /// tombstone); false ⇔ it already fired (the word was stored).
    pub fn cancel(self: *TimerThread, id: u64) bool {
        self.lock.acquire();
        defer self.lock.release();
        return self.live.remove(id);
    }

    /// A raw thread's sleep until `deadline_ns` (monotonic): uncancelable
    /// (there is no cancel off a lane).
    pub fn sleepUntil(self: *TimerThread, deadline_ns: u64) void {
        var word: futex.Word = .init(0);
        if (clock.monotonicNs() >= deadline_ns) return;
        _ = self.arm(deadline_ns, &word);
        while (word.load(.acquire) == 0) futex.wait(&word, 0);
    }

    /// Live registrations (tests).
    pub fn liveCount(self: *TimerThread) usize {
        self.lock.acquire();
        defer self.lock.release();
        return self.live.count();
    }

    /// Heap entries incl. tombstones (tests).
    pub fn heapCount(self: *TimerThread) usize {
        self.lock.acquire();
        defer self.lock.release();
        return self.heap.count();
    }

    fn serviceLoop(self: *TimerThread) void {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, comm);
        // Fired words are woken outside the lock; 64 per round bounds the
        // frame, more take another round.
        var fired: [64]*futex.Word = undefined;
        while (!self.stopping.load(.acquire)) {
            const seq_seen = self.seq.load(.acquire);
            var n: usize = 0;
            var next: ?u64 = null;
            const now = clock.monotonicNs();
            self.lock.acquire();
            while (self.heap.peek()) |top| {
                if (top.deadline_ns > now) {
                    next = top.deadline_ns;
                    break;
                }
                _ = self.heap.pop();
                if (self.live.fetchRemove(top.id)) |kv| {
                    // Under the lock: a `cancel` racing this sees `false` only
                    // once the store landed.
                    kv.value.store(1, .release);
                    fired[n] = kv.value;
                    n += 1;
                    if (n == fired.len) break;
                } else {
                    stats.timer_thread_tombstones.inc();
                }
            }
            self.lock.release();
            for (fired[0..n]) |w| futex.wakeAll(w);
            if (n == fired.len) continue;
            // Park until the next deadline or a bump (a new earliest, a stop).
            if (next) |d| {
                const now2 = clock.monotonicNs();
                if (d > now2) _ = futex.waitTimeout(&self.seq, seq_seen, d - now2);
            } else {
                futex.wait(&self.seq, seq_seen);
            }
        }
    }
};

const testing = std.testing;

test "sleep_fires: a raw sleep returns at or after its deadline; many sleeps from many threads all fire in order" {
    const t = TimerThread.init(testing.allocator);
    defer t.deinit();
    try t.start();
    const t0 = clock.monotonicNs();
    t.sleepUntil(t0 + 5 * std.time.ns_per_ms);
    try testing.expect(clock.monotonicNs() - t0 >= 5 * std.time.ns_per_ms);
    // Already passed: returns at once.
    t.sleepUntil(0);
    const Sleeper = struct {
        fn run(tt: *TimerThread, delay_ms: u64, done: *std.atomic.Value(u32)) void {
            const start = clock.monotonicNs();
            tt.sleepUntil(start + delay_ms * std.time.ns_per_ms);
            std.debug.assert(clock.monotonicNs() - start >= delay_ms * std.time.ns_per_ms);
            _ = done.fetchAdd(1, .acq_rel);
        }
    };
    var done: std.atomic.Value(u32) = .init(0);
    var ts: [16]std.Thread = undefined;
    for (&ts, 0..) |*th, i| th.* = try std.Thread.spawn(.{}, Sleeper.run, .{ t, @as(u64, 1 + (i % 5)), &done });
    for (ts) |th| th.join();
    try testing.expectEqual(@as(u32, 16), done.load(.acquire));
    try testing.expectEqual(@as(usize, 0), t.liveCount());
}

test "dropped_sleep_cancels_cleanly: a cancelled hour-long arm leaves a tombstone the service skips; the thread stays live; a cancel after the fire reports false" {
    const t = TimerThread.init(testing.allocator);
    defer t.deinit();
    try t.start();
    var never: futex.Word = .init(0);
    const id = t.arm(clock.monotonicNs() + std.time.ns_per_hour, &never);
    try testing.expectEqual(@as(usize, 1), t.liveCount());
    try testing.expect(t.cancel(id));
    try testing.expect(!t.cancel(id));
    try testing.expectEqual(@as(usize, 0), t.liveCount());
    try testing.expectEqual(@as(usize, 1), t.heapCount());
    // A short arm BEFORE the tombstone's deadline fires; the service is live.
    const before = stats.timer_thread_tombstones.load();
    var soon: futex.Word = .init(0);
    const id2 = t.arm(clock.monotonicNs() + 2 * std.time.ns_per_ms, &soon);
    while (soon.load(.acquire) == 0) futex.wait(&soon, 0);
    try testing.expect(!t.cancel(id2));
    // A tombstone whose deadline passes is popped and counted.
    var gone: futex.Word = .init(0);
    const id3 = t.arm(clock.monotonicNs() + 2 * std.time.ns_per_ms, &gone);
    try testing.expect(t.cancel(id3));
    var probe: futex.Word = .init(0);
    _ = t.arm(clock.monotonicNs() + 4 * std.time.ns_per_ms, &probe);
    while (probe.load(.acquire) == 0) futex.wait(&probe, 0);
    try testing.expectEqual(@as(u32, 0), gone.load(.acquire));
    try testing.expectEqual(before + 1, stats.timer_thread_tombstones.load());
    // The hour-long tombstone is still in the heap, harmless.
    try testing.expectEqual(@as(usize, 1), t.heapCount());
}
