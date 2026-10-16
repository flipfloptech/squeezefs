//! Clocks for hot paths: one `clock_gettime` call, no `std.Io` handle.
//!
//! `monotonicNs` is the phase-histogram and deadline clock. `realtimeCoarseS`
//! is the rate-limiter clock (one log line per site per second): COARSE on
//! Linux because a tripwire fire must not pay a vDSO fine-clock read on top of
//! being a bug; plain REALTIME elsewhere.

const std = @import("std");
const builtin = @import("builtin");
const posix = std.posix;

fn read(clock: posix.clockid_t) posix.timespec {
    var ts: posix.timespec = undefined;
    // clock_gettime on a valid clock id cannot fail; a failure here would be
    // a kernel bug, and zeroes are a safe answer for both callers (a phase of
    // 0 ns, a rate limiter that logs).
    switch (posix.errno(posix.system.clock_gettime(clock, &ts))) {
        .SUCCESS => return ts,
        else => return .{ .sec = 0, .nsec = 0 },
    }
}

pub fn monotonicNs() u64 {
    const ts = read(.MONOTONIC);
    return @as(u64, @intCast(ts.sec)) * std.time.ns_per_s + @as(u64, @intCast(ts.nsec));
}

pub fn realtimeCoarseS() u64 {
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .REALTIME_COARSE else .REALTIME;
    const ts = read(clock);
    return if (ts.sec < 0) 0 else @intCast(ts.sec);
}

/// The i64-ns realtime stamp a metadata mutation carries (atime/mtime/
/// ctime): COARSE on Linux because the kernel authors its own cmtime from
/// the same coarse clock — a fine daemon clock could LEAD the kernel's
/// writeback-cache stamp by a tick and invert cross-inode ctime order
/// (fstests generic/423).
pub fn realtimeCoarseNs() i64 {
    const clock: posix.clockid_t = if (builtin.os.tag == .linux) .REALTIME_COARSE else .REALTIME;
    const ts = read(clock);
    return @as(i64, @intCast(ts.sec)) *| std.time.ns_per_s +| @as(i64, @intCast(ts.nsec));
}

test "monotonic never goes backwards; realtime is after 2020" {
    const a = monotonicNs();
    const b = monotonicNs();
    try std.testing.expect(b >= a);
    try std.testing.expect(realtimeCoarseS() > 1_577_836_800);
    try std.testing.expect(realtimeCoarseNs() > 1_577_836_800 * std.time.ns_per_s);
}
