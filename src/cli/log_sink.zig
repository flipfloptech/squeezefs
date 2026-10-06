//! Where `std.log` goes (charter §8 `mount --daemon --log-file`): stderr
//! by default (`std.log.defaultLog`), or — once `install` names a fd — one
//! `write(2)` per line to that fd (a control path, never a product I/O
//! path: charter §1.2's exception class). The daemon opens the file
//! `O_WRONLY|O_CREAT|O_APPEND|O_NOFOLLOW|O_CLOEXEC`, mode 0600 (the log
//! carries paths and uids; `O_NOFOLLOW` so a symlink planted at the path
//! can never redirect a root daemon's writes), and `O_APPEND` makes every
//! line one atomic append across the daemon's threads.
//!
//! The exe's root (`src/main.zig`) binds `std_options.logFn = logFn`; the
//! unit-test runner keeps its own sink, so the seam `core/log.zig` relies on
//! is untouched. A line is `<realtime s>.<ms> <level>(<scope>): <msg>`
//! formatted into a stack buffer; a line past the buffer is truncated with
//! an ellipsis rather than split (one `write` per line is the contract).

const std = @import("std");
const builtin = @import("builtin");

/// One line's bound: the longest product lines (the INIT negotiation, a
/// wedge census) fit in a quarter of it; a 4 KiB stack frame per log call
/// is admissible on every thread class (the smallest fiber stack is 512 KiB).
pub const LINE_MAX: usize = 4096;
/// The daemon's log-file mode: owner read/write only (module doc).
pub const LOG_FILE_MODE: u32 = 0o600;

/// The sink fd; `-1` = stderr through `std.log.defaultLog`. Written once by
/// the daemon before any thread exists; read relaxed by every logger.
var sink_fd: std.atomic.Value(i32) = .init(-1);

/// Route every later log line to `fd` (the daemon's log file).
pub fn install(fd: i32) void {
    sink_fd.store(fd, .monotonic);
}

pub fn installed() bool {
    return sink_fd.load(.monotonic) >= 0;
}

/// `std.Options.logFn`.
pub fn logFn(comptime level: std.log.Level, comptime scope: @EnumLiteral(), comptime format: []const u8, args: anytype) void {
    const fd = sink_fd.load(.monotonic);
    if (fd < 0 or builtin.os.tag != .linux) return std.log.defaultLog(level, scope, format, args);
    var buf: [LINE_MAX]u8 = undefined;
    const line = formatLine(&buf, realtimeNs(), level.asText(), @tagName(scope), format, args);
    writeAll(fd, line);
}

/// The line as written (public for the tie test).
pub fn formatLine(buf: *[LINE_MAX]u8, now_ns: i128, level: []const u8, scope: []const u8, comptime format: []const u8, args: anytype) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    const secs: i128 = @divFloor(now_ns, std.time.ns_per_s);
    const ms: u64 = @intCast(@divFloor(@mod(now_ns, std.time.ns_per_s), std.time.ns_per_ms));
    w.print("{d}.{d:0>3} {s}({s}): ", .{ secs, ms, level, scope }) catch {};
    w.print(format, args) catch {
        // Truncated: mark it and keep the one-write contract.
        const mark = "…\n";
        @memcpy(buf[LINE_MAX - mark.len ..], mark);
        return buf[0..LINE_MAX];
    };
    w.writeByte('\n') catch {
        const mark = "…\n";
        @memcpy(buf[LINE_MAX - mark.len ..], mark);
        return buf[0..LINE_MAX];
    };
    return w.buffered();
}

fn realtimeNs() i128 {
    if (builtin.os.tag != .linux) return 0;
    var ts: std.os.linux.timespec = undefined;
    _ = std.os.linux.clock_gettime(.REALTIME, &ts);
    return @as(i128, ts.sec) * std.time.ns_per_s + ts.nsec;
}

fn writeAll(fd: i32, bytes: []const u8) void {
    if (builtin.os.tag != .linux) return;
    var off: usize = 0;
    while (off < bytes.len) {
        const rc = std.os.linux.write(fd, bytes[off..].ptr, bytes.len - off);
        switch (std.os.linux.errno(rc)) {
            .SUCCESS => off += rc,
            .INTR => continue,
            // A full disk or a closed fd cannot be reported anywhere; the
            // line is dropped, never a panic in a logger.
            else => return,
        }
    }
}

const testing = std.testing;

test "a line is `<s>.<ms> level(scope): msg\\n`; an overlong message truncates to one write with a mark" {
    var buf: [LINE_MAX]u8 = undefined;
    const line = formatLine(&buf, 1_700_000_000_123_456_789, "info", "squeezefs", "ready at {s} ({d} queues)", .{ "/mnt/x", @as(u32, 8) });
    try testing.expectEqualStrings("1700000000.123 info(squeezefs): ready at /mnt/x (8 queues)\n", line);
    const long: [LINE_MAX * 2]u8 = @splat('x');
    const t = formatLine(&buf, 0, "error", "squeezefs", "{s}", .{long[0..]});
    try testing.expectEqual(LINE_MAX, t.len);
    try testing.expect(std.mem.endsWith(u8, t, "…\n"));
    try testing.expect(std.mem.startsWith(u8, t, "0.000 error(squeezefs): xxx"));
}

test "the sink starts on stderr; install flips it" {
    try testing.expect(!installed());
    try testing.expectEqual(@as(u32, 0o600), LOG_FILE_MODE);
}
