//! Thread naming is part of the observability contract (charter §9):
//! `daemon_cpu_ns_by_class` attributes CPU per class by the thread's comm.
//!
//! Every per-mount thread name goes through `commName`: when the process has
//! a mount tag (the mount slot, set once at mount identity), the 2-char suffix
//! `m<x>` is appended and the base truncated to 13 so the whole name fits the
//! kernel's 15 visible comm chars (`TASK_COMM_LEN − NUL`). The suffix never
//! truncates; the index sits before it so prefix classification tolerates it.

const std = @import("std");
const builtin = @import("builtin");

/// `TASK_COMM_LEN − 1`: the kernel keeps 15 visible chars + NUL.
pub const comm_max = 15;
const tag_width = 2;

var comm_tag: std.atomic.Value(u32) = .init(0);

/// Set once at mount identity; 0 = untagged (a CLI process, a single mount).
pub fn setMountTag(slot: u32) void {
    comm_tag.store(slot, .monotonic);
}

pub const CommBuf = [comm_max:0]u8;

pub fn commName(buf: *CommBuf, base: []const u8) [:0]const u8 {
    return commNameWith(buf, base, comm_tag.load(.monotonic));
}

pub fn commNameWith(buf: *CommBuf, base: []const u8, tag: u32) [:0]const u8 {
    @memset(buf[0..comm_max], 0);
    if (tag == 0) {
        const n = @min(base.len, comm_max);
        @memcpy(buf[0..n], base[0..n]);
        buf[n] = 0;
        return buf[0..n :0];
    }
    const base_max = comm_max - tag_width;
    const n = @min(base.len, base_max);
    @memcpy(buf[0..n], base[0..n]);
    buf[n] = 'm';
    buf[n + 1] = std.fmt.digitToChar(@intCast(tag & 0xf), .lower);
    buf[n + 2] = 0;
    return buf[0 .. n + 2 :0];
}

/// Name the calling thread (Linux `prctl(PR_SET_NAME)`); a no-op elsewhere.
/// Returns the name written so spawn sites can log it.
pub fn nameCurrentThread(buf: *CommBuf, base: []const u8) [:0]const u8 {
    const name = commName(buf, base);
    if (builtin.os.tag == .linux) {
        const PR_SET_NAME = 15;
        _ = std.os.linux.prctl(PR_SET_NAME, @intFromPtr(name.ptr), 0, 0, 0);
    }
    return name;
}

/// The CPU accounting classes, first prefix match wins; `other` catches the
/// rest (main, `sqz-lease*`, `sqz-clw-*`, `sqz-fdwatch`, `sqz-signal`).
pub const Class = struct { prefix: []const u8, class: []const u8 };

pub const classes = [_]Class{
    .{ .prefix = "fuse3-tpc", .class = "fuse3-tpc" },
    .{ .prefix = "f3-ur", .class = "fuse3-ur" },
    .{ .prefix = "sqz-ipc-svc", .class = "sqz-ipc-svc" },
    .{ .prefix = "sqz-ipc-dd", .class = "sqz-ipc-dd" },
    .{ .prefix = "sqz-meta", .class = "sqz-meta" },
    .{ .prefix = "sqz-jrnl", .class = "sqz-jrnl" },
    .{ .prefix = "sqz-blk", .class = "sqz-blk" },
    .{ .prefix = "sqz-nvme", .class = "sqz-nvme" },
    .{ .prefix = "sqz-zcrx", .class = "sqz-zcrx" },
    .{ .prefix = "sqz-timer", .class = "sqz-timer" },
};
pub const other = "other";

pub fn classify(comm: []const u8) []const u8 {
    for (classes) |c| if (std.mem.startsWith(u8, comm, c.prefix)) return c.class;
    return other;
}

const testing = std.testing;

test "comm names fit the 15-char budget and keep the tag suffix" {
    const worst = [_][]const u8{ "sqz-ipc-svc63", "sqz-jrnl9999", "fuse3-tpc511", "f3-ur0-511", "sqz-uring-fs63", "sqz-nvme1023" };
    var buf: CommBuf = undefined;
    for (worst) |base| {
        const n = commNameWith(&buf, base, 0xffff_ffff);
        try testing.expect(n.len <= comm_max);
        try testing.expect(std.mem.endsWith(u8, n, "mf"));
        try testing.expectEqual(@as(u8, 0), n[n.len]);
    }
    try testing.expectEqualStrings("sqz-timer", commNameWith(&buf, "sqz-timer", 0));
    try testing.expectEqualStrings("sqz-timerm3", commNameWith(&buf, "sqz-timer", 3));
    try testing.expectEqualStrings("sqz-ipc-svc63m1", commNameWith(&buf, "sqz-ipc-svc63", 1));
    try testing.expectEqualStrings("sqz-ipc-svc63", commNameWith(&buf, "sqz-ipc-svc63", 0));
}

test "classification: first prefix wins; tag suffix tolerated; other catches the rest" {
    try testing.expectEqualStrings("fuse3-ur", classify("f3-ur17m2"));
    try testing.expectEqualStrings("fuse3-tpc", classify("fuse3-tpc3"));
    try testing.expectEqualStrings("sqz-nvme", classify("sqz-nvme0"));
    try testing.expectEqualStrings("sqz-jrnl", classify("sqz-jrnl1"));
    try testing.expectEqualStrings("other", classify("sqz-lease-io"));
    try testing.expectEqualStrings("other", classify("squeezefs"));
}
