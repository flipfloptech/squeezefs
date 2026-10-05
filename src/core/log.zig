//! Product logging. One scope, four levels, and a test seam that turns "this
//! path logs loudly" into an assertable contract: a test declares how many
//! error logs it expects (`testing.expectErrorLogs(n)`) and the seam routes
//! exactly those to the warn level while counting them, so the loudness is
//! asserted rather than suppressed. Outside tests the seam compiles away.

const std = @import("std");
const builtin = @import("builtin");

const scoped = std.log.scoped(.squeezefs);

pub fn err(comptime fmt: []const u8, args: anytype) void {
    if (builtin.is_test) {
        if (expected_errors > 0) {
            expected_errors -= 1;
            captured_errors += 1;
            scoped.warn("(expected error) " ++ fmt, args);
            return;
        }
    }
    scoped.err(fmt, args);
}

pub fn warn(comptime fmt: []const u8, args: anytype) void {
    scoped.warn(fmt, args);
}

pub fn info(comptime fmt: []const u8, args: anytype) void {
    scoped.info(fmt, args);
}

pub fn debug(comptime fmt: []const u8, args: anytype) void {
    scoped.debug(fmt, args);
}

threadlocal var expected_errors: usize = 0;
threadlocal var captured_errors: usize = 0;

pub const testing = struct {
    /// Expect exactly `n` error logs on this thread from now on; an
    /// unexpected error log still fails the test (the runner counts it).
    pub fn expectErrorLogs(n: usize) void {
        expected_errors = n;
        captured_errors = 0;
    }
    /// How many expected error logs actually fired.
    pub fn capturedErrorLogs() usize {
        return captured_errors;
    }
    /// Assert every expected error log fired and no expectation is left.
    pub fn expectAllFired() !void {
        try std.testing.expectEqual(@as(usize, 0), expected_errors);
    }
};

test "the seam routes exactly the expected error logs and counts them" {
    testing.expectErrorLogs(2);
    err("first {d}", .{1});
    err("second {d}", .{2});
    try testing.expectAllFired();
    try std.testing.expectEqual(@as(usize, 2), testing.capturedErrorLogs());
}
