//! The ONE retry ladder of the write path (`docs/porting/data_write_path.md`
//! §13 Q13/Q14): a bounded-exponential backoff value type — `base << min(
//! attempt, shift_cap)` clamped at `cap`, barking at power-of-two attempts
//! — used by the overlay settle and the writeback requeue, each instance
//! naming its reason. The reference carried
//! three shapes for one condition (`SETTLE_RETRIES = 24` borrowed from the
//! rebind ladder, `min(1 ms << a, 50 ms)`, `WRITEBACK_MAX_ATTEMPTS = 4` with
//! a wrapping counter as control flow); here a ladder either retries forever
//! (`attempts_max = null`, the never-lossy class) or refuses at its bound,
//! and "past the cap" is a COUNT (`exhaustions`), never a wrap.

const std = @import("std");
const assert = std.debug.assert;

pub const Ladder = struct {
    /// The first backoff.
    base_ns: u64,
    /// The backoff ceiling (the physical reason on the instance's line).
    cap_ns: u64,
    /// `null` = retry forever; else the attempt count past which `next`
    /// answers `.exhausted`.
    attempts_max: ?u32 = null,
    attempt: u32 = 0,

    pub const Step = union(enum) {
        /// Park this long, then try again.
        backoff_ns: u64,
        /// The bound is reached: the caller takes its terminal arm.
        exhausted,
    };

    /// The backoff for the NEXT attempt (and advance the count).
    pub fn next(self: *Ladder) Step {
        if (self.attempts_max) |m| if (self.attempt >= m) return .exhausted;
        const shift: u6 = @intCast(@min(self.attempt, 63));
        // A shift that loses bits is already past any cap.
        const raw = std.math.shlExact(u64, self.base_ns, shift) catch self.cap_ns;
        self.attempt += 1;
        return .{ .backoff_ns = @min(raw, self.cap_ns) };
    }

    /// Bark on attempts 1, 2, 4, 8, … (one log line per doubling, never per
    /// attempt — a retry-forever ladder must not flood the log).
    pub fn shouldBark(self: *const Ladder) bool {
        return self.attempt != 0 and std.math.isPowerOfTwo(self.attempt);
    }

    pub fn attempts(self: *const Ladder) u32 {
        return self.attempt;
    }
};

const testing = std.testing;

test "the ladder doubles from base, clamps at the cap, barks at powers of two and exhausts only when bounded" {
    var l: Ladder = .{ .base_ns = 1_000_000, .cap_ns = 50_000_000, .attempts_max = 8 };
    try testing.expectEqual(Ladder.Step{ .backoff_ns = 1_000_000 }, l.next());
    try testing.expect(l.shouldBark());
    try testing.expectEqual(Ladder.Step{ .backoff_ns = 2_000_000 }, l.next());
    try testing.expect(l.shouldBark());
    try testing.expectEqual(Ladder.Step{ .backoff_ns = 4_000_000 }, l.next());
    try testing.expect(!l.shouldBark());
    _ = l.next(); // 8 ms
    _ = l.next(); // 16 ms
    _ = l.next(); // 32 ms
    try testing.expectEqual(Ladder.Step{ .backoff_ns = 50_000_000 }, l.next()); // 64 ms clamps to the cap
    try testing.expectEqual(Ladder.Step{ .backoff_ns = 50_000_000 }, l.next());
    try testing.expectEqual(Ladder.Step.exhausted, l.next());
    try testing.expectEqual(@as(u32, 8), l.attempts());
    // Forever: never exhausts, stays at the cap, no overflow at deep attempt counts.
    var f: Ladder = .{ .base_ns = 10_000_000, .cap_ns = 1_000_000_000 };
    var i: u32 = 0;
    while (i < 200) : (i += 1) {
        switch (f.next()) {
            .backoff_ns => |b| try testing.expect(b <= 1_000_000_000 and b >= 10_000_000),
            .exhausted => return error.TestUnexpectedResult,
        }
    }
}
