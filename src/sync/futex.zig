//! The ONE futex wrapper (charter §12; decision 0003 WP-C row 13). Two
//! faces over one law:
//!   - raw Linux (`wait`, `waitTimeout`, `wake`, `wakeAll`): for lane threads
//!     and the hot-path completion cells, which never take an `Io`
//!     (decision 0001 §4);
//!   - `Io` (`ioWaitTimeout`, `ioWake`): for the portable primitives in
//!     `src/sync/` (`tick.zig`), which work under any `Io`.
//!
//! The law both faces carry: FUTEX_WAKE's count is a C int — anything above
//! INT_MAX reads as negative and wakes exactly ONE waiter (found red-first in
//! rung 1: a `close` that must wake every parked worker stranded the rest in
//! `join`). EAGAIN (the word changed) and EINTR (a signal) both mean
//! "re-check the word": every caller loops on its own predicate.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

/// The futex word every primitive parks on.
pub const Word = std.atomic.Value(u32);

pub const WaitVerdict = enum {
    /// The word changed, a wake arrived, or a signal interrupted: re-check.
    returned,
    /// The timeout elapsed with no wake observed.
    timed_out,
};

/// Park while `word == expect`. Returns on a wake, a changed word (EAGAIN)
/// or a signal (EINTR); the caller re-checks its predicate and re-parks.
pub fn wait(word: *const Word, expect: u32) void {
    _ = linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = true }, expect, null);
}

/// `wait` bounded by a relative timeout.
pub fn waitTimeout(word: *const Word, expect: u32, timeout_ns: u64) WaitVerdict {
    const ts: linux.timespec = .{
        .sec = @intCast(timeout_ns / std.time.ns_per_s),
        .nsec = @intCast(timeout_ns % std.time.ns_per_s),
    };
    return switch (linux.errno(linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = true }, expect, &ts))) {
        .TIMEDOUT => .timed_out,
        else => .returned,
    };
}

/// Wake up to `max_waiters` parked on `word`; the count is clamped to the
/// kernel's C-int range (see the module doc).
pub fn wake(word: *const Word, max_waiters: u32) void {
    if (max_waiters == 0) return;
    _ = linux.futex_3arg(&word.raw, .{ .cmd = .WAKE, .private = true }, @min(max_waiters, std.math.maxInt(i32)));
}

pub fn wakeOne(word: *const Word) void {
    wake(word, 1);
}

pub fn wakeAll(word: *const Word) void {
    wake(word, std.math.maxInt(i32));
}

/// The `Io` face of `waitTimeout`: `timeout_ns == null` waits unbounded.
pub fn ioWaitTimeout(io: Io, word: *const Word, expect: u32, timeout_ns: ?u64) Io.Cancelable!void {
    const timeout: Io.Timeout = if (timeout_ns) |ns| .{ .duration = .{
        .raw = .fromNanoseconds(@intCast(ns)),
        .clock = .awake,
    } } else .none;
    return io.futexWaitTimeout(u32, &word.raw, expect, timeout);
}

/// The `Io` face of `wake`, under the same clamp.
pub fn ioWake(io: Io, word: *const Word, max_waiters: u32) void {
    if (max_waiters == 0) return;
    io.futexWake(u32, &word.raw, @min(max_waiters, std.math.maxInt(i32)));
}

const testing = std.testing;

test "raw: a wake from another thread ends the wait; a changed word returns at once; a timeout reports itself" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    var word: Word = .init(0);
    const Waker = struct {
        fn run(w: *Word) void {
            w.store(1, .release);
            wakeAll(w);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{&word});
    while (word.load(.acquire) == 0) wait(&word, 0);
    t.join();
    // The word no longer matches: EAGAIN, an immediate return.
    wait(&word, 0);
    try testing.expectEqual(WaitVerdict.returned, waitTimeout(&word, 0, std.time.ns_per_s));
    try testing.expectEqual(WaitVerdict.timed_out, waitTimeout(&word, 1, 2 * std.time.ns_per_ms));
}

test "raw: wakeAll wakes EVERY parked waiter (the C-int clamp), not just one" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    var word: Word = .init(0);
    var parked: std.atomic.Value(u32) = .init(0);
    const Sleeper = struct {
        fn run(w: *Word, p: *std.atomic.Value(u32)) void {
            _ = p.fetchAdd(1, .seq_cst);
            while (w.load(.acquire) == 0) wait(w, 0);
        }
    };
    var threads: [8]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Sleeper.run, .{ &word, &parked });
    while (parked.load(.seq_cst) != threads.len) try std.Thread.yield();
    word.store(1, .release);
    wakeAll(&word);
    // Every sleeper exits on its own: a one-waiter wake would strand seven in `join`.
    for (threads) |t| t.join();
}

test "io face: a wake ends a timed wait; a timeout returns without a wake" {
    const io = testing.io;
    var word: Word = .init(0);
    try ioWaitTimeout(io, &word, 1, 0);
    const Waker = struct {
        fn run(w: *Word) void {
            w.store(1, .release);
            ioWake(testing.io, w, 1);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{&word});
    while (word.load(.acquire) == 0) try ioWaitTimeout(io, &word, 0, std.time.ns_per_s);
    t.join();
    // A zero count is a no-op on both faces.
    ioWake(io, &word, 0);
}
