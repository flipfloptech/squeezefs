//! The ONE futex wrapper (charter §12; decision 0003 WP-C row 13). Three
//! faces over one law:
//!   - raw Linux, process-private (`wait`, `waitTimeout`, `wake`, `wakeAll`):
//!     for lane threads' own parks and the completion cells of a bare
//!     device (decision 0001 §4; a device with a wait route parks through
//!     the `Io` face below — the 2026-10-18 amendment);
//!   - raw Linux, cross-process (`waitShared`, `waitSharedTimeout`,
//!     `wakeShared`, `waitMany`): for words that live in a `MAP_SHARED`
//!     memfd two processes map — the IL session's slot states and doorbells
//!     (charter §7; decision 0003 §F). `FUTEX_PRIVATE_FLAG` keys the wait on
//!     the caller's mm, so a private wait on a shared word is a wait nobody
//!     else can wake: the shared face never sets it;
//!   - `Io` (`ioWaitTimeout`, `ioWake`): for the portable primitives in
//!     `src/sync/` (`tick.zig`), which work under any `Io`.
//!
//! The law every face carries: FUTEX_WAKE's count is a C int — anything above
//! INT_MAX reads as negative and wakes exactly ONE waiter (found red-first in
//! rung 1: a `close` that must wake every parked worker stranded the rest in
//! `join`). EAGAIN (the word changed) and EINTR (a signal) both mean
//! "re-check the word": every caller loops on its own predicate.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
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
    return waitTimeoutFlagged(word, expect, timeout_ns, true);
}

fn waitTimeoutFlagged(word: *const Word, expect: u32, timeout_ns: u64, private: bool) WaitVerdict {
    const ts: linux.timespec = .{
        .sec = @intCast(timeout_ns / std.time.ns_per_s),
        .nsec = @intCast(timeout_ns % std.time.ns_per_s),
    };
    return switch (linux.errno(linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = private }, expect, &ts))) {
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

// ---------------------------------------------------------------------------
// The cross-process face: a word in shared memory (a MAP_SHARED memfd both
// sides map at different addresses). The kernel keys a non-private futex by
// (inode, page offset), so the two mappings meet on one wait queue.
// ---------------------------------------------------------------------------

/// Park on a SHARED word while `word == expect`, bounded by a relative
/// timeout. Every wait on client-writable memory is bounded (charter §7),
/// so the shared face has no unbounded form.
pub fn waitSharedTimeout(word: *const Word, expect: u32, timeout_ns: u64) WaitVerdict {
    return waitTimeoutFlagged(word, expect, timeout_ns, false);
}

/// Wake up to `max_waiters` parked on a SHARED word (same clamp as `wake`).
pub fn wakeShared(word: *const Word, max_waiters: u32) void {
    if (max_waiters == 0) return;
    _ = linux.futex_3arg(&word.raw, .{ .cmd = .WAKE, .private = false }, @min(max_waiters, std.math.maxInt(i32)));
}

pub fn wakeSharedAll(word: *const Word) void {
    wakeShared(word, std.math.maxInt(i32));
}

/// One entry of a `waitMany`: park while `word == expect`.
pub const Waiter = struct {
    word: *const Word,
    expect: u32,
};

/// The kernel's `futex_waitv` entry cap (`FUTEX_WAITV_MAX`).
pub const wait_many_max = linux.FUTEX2_WAITONE_MAX;

pub const WaitManyVerdict = enum {
    /// One of the words changed, a wake arrived, or a signal: re-check all.
    returned,
    timed_out,
    /// The kernel has no `futex_waitv` (pre-5.16). The venue is 7.2.6
    /// (charter §1.9): the caller fails loud at session establish, never
    /// degrades to a single-word wait (decision 0002 ledger: Q15).
    unsupported,
};

/// Park on up to `wait_many_max` SHARED words at once (`futex_waitv(2)`):
/// returns as soon as ANY word differs from its expected value or is woken.
/// The timeout is relative here and converted to the absolute
/// `CLOCK_MONOTONIC` deadline the syscall takes. An empty set returns
/// `timed_out` after the bound (a session-less owner has nothing to hear).
pub fn waitMany(entries: []const Waiter, timeout_ns: u64) WaitManyVerdict {
    std.debug.assert(entries.len <= wait_many_max);
    var ones: [wait_many_max]linux.futex2_waitone = undefined;
    for (entries, 0..) |e, i| {
        ones[i] = .{
            .val = e.expect,
            .uaddr = @intFromPtr(&e.word.raw),
            .flags = .{ .size = .U32, .private = false },
        };
    }
    var now: linux.timespec = undefined;
    _ = linux.clock_gettime(.MONOTONIC, &now);
    const total_ns = @as(u64, @intCast(now.sec)) * std.time.ns_per_s + @as(u64, @intCast(now.nsec)) + timeout_ns;
    const deadline: linux.kernel_timespec = .{
        .sec = @intCast(total_ns / std.time.ns_per_s),
        .nsec = @intCast(total_ns % std.time.ns_per_s),
    };
    if (entries.len == 0) {
        // Nothing to hear: sleep the bound on a private word of our own.
        var idle: Word = .init(0);
        return switch (waitTimeout(&idle, 0, timeout_ns)) {
            .timed_out => .timed_out,
            .returned => .returned,
        };
    }
    return switch (linux.errno(linux.futex2_waitv(&ones, @intCast(entries.len), .{}, &deadline, .MONOTONIC))) {
        .TIMEDOUT => .timed_out,
        .NOSYS => .unsupported,
        else => .returned,
    };
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
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "futex: Linux only");
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
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "futex: Linux only");
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

/// A shared word for the cross-process tests: one page of a memfd mapped
/// `MAP_SHARED` (the IL session's shape).
fn mapSharedWord() !*Word {
    const rc = linux.memfd_create("sqz-futex-test", linux.MFD.CLOEXEC);
    if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, 4096)) != .SUCCESS) return error.MemfdFailed;
    const m = linux.mmap(null, 4096, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
    if (linux.errno(m) != .SUCCESS) return error.MapFailed;
    const w: *Word = @ptrFromInt(m);
    w.* = .init(0);
    return w;
}

test "shared face: a forked child parked on a MAP_SHARED word is woken by the parent (the private flag would strand it)" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "futex: Linux only");
    const word = try mapSharedWord();
    defer _ = linux.munmap(@ptrCast(word), 4096);
    // Two words: `word` is the park word, `*ack` the child's "I am parked"
    // signal; the parent waits for the ack, then stores + wakes.
    const ack: *Word = @ptrFromInt(@intFromPtr(word) + 64);
    ack.* = .init(0);
    const pid_rc = linux.fork();
    if (linux.errno(pid_rc) != .SUCCESS) return error.ForkFailed;
    if (pid_rc == 0) {
        // Child: announce, then park until the word moves. A bound of 5 s
        // means a lost wake exits 2 instead of hanging the gate.
        ack.store(1, .release);
        wakeSharedAll(ack);
        var deadline_slices: u32 = 0;
        while (word.load(.acquire) == 0) {
            if (waitSharedTimeout(word, 0, 100 * std.time.ns_per_ms) == .timed_out) deadline_slices += 1;
            if (deadline_slices > 50) linux.exit_group(2);
        }
        linux.exit_group(0);
    }
    const pid: linux.pid_t = @intCast(pid_rc);
    while (ack.load(.acquire) == 0) _ = waitSharedTimeout(ack, 0, 100 * std.time.ns_per_ms);
    word.store(1, .release);
    wakeSharedAll(word);
    var status: i32 = 0;
    _ = linux.waitpid(pid, &status, 0);
    try testing.expect(linux.W.IFEXITED(@bitCast(status)));
    try testing.expectEqual(@as(u8, 0), linux.W.EXITSTATUS(@bitCast(status)));
}

test "waitMany: a wake on ANY of several shared words ends the wait; a stale expectation returns at once; an empty set sleeps the bound" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "futex: Linux only");
    const base = try mapSharedWord();
    defer _ = linux.munmap(@ptrCast(base), 4096);
    const a: *Word = base;
    const b: *Word = @ptrFromInt(@intFromPtr(base) + 128);
    const c: *Word = @ptrFromInt(@intFromPtr(base) + 256);
    b.* = .init(0);
    c.* = .init(0);
    const entries = [_]Waiter{ .{ .word = a, .expect = 0 }, .{ .word = b, .expect = 0 }, .{ .word = c, .expect = 0 } };
    const Waker = struct {
        fn run(w: *Word) void {
            w.store(1, .release);
            wakeSharedAll(w);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{c});
    var verdict: WaitManyVerdict = .timed_out;
    while (c.load(.acquire) == 0) verdict = waitMany(&entries, std.time.ns_per_s);
    t.join();
    if (verdict == .unsupported) return error.FutexWaitvUnsupported;
    // `c` moved: the admission check fails at once.
    try testing.expectEqual(WaitManyVerdict.returned, waitMany(&entries, std.time.ns_per_s));
    const fresh = [_]Waiter{ .{ .word = a, .expect = 0 }, .{ .word = b, .expect = 0 } };
    try testing.expectEqual(WaitManyVerdict.timed_out, waitMany(&fresh, 2 * std.time.ns_per_ms));
    try testing.expectEqual(WaitManyVerdict.timed_out, waitMany(&.{}, 2 * std.time.ns_per_ms));
}

fn ioFace(io: Io) !void {
    var word: Word = .init(0);
    try ioWaitTimeout(io, &word, 1, 0);
    const Waker = struct {
        fn run(io_: Io, w: *Word) void {
            w.store(1, .release);
            ioWake(io_, w, 1);
        }
    };
    const t = try std.Thread.spawn(.{}, Waker.run, .{ io, &word });
    while (word.load(.acquire) == 0) try ioWaitTimeout(io, &word, 0, std.time.ns_per_s);
    t.join();
    // A zero count is a no-op on both faces.
    ioWake(io, &word, 0);
}

test "io face: a wake ends a timed wait; a timeout returns without a wake" {
    try @import("testing.zig").underBoth(ioFace);
}
