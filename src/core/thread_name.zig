//! Thread naming is part of the observability contract (charter §9):
//! `daemon_cpu_ns_by_class` attributes CPU per class by the thread's comm.
//!
//! Every thread name goes through `commName`: the base fits the kernel's 15
//! visible comm chars (`TASK_COMM_LEN − NUL`) with its index intact — every
//! charter pattern at its widest index is checked by `fitsBudget`, so a name
//! is never truncated silently. There is no per-process tag in a thread's
//! comm: process identity belongs to `argv[0]`, and the accounting contract
//! samples `/proc/self/task`, which never needs one (decision 0003 WP-C
//! row 3 — the reference's `m<x>` suffix cost `sqz-uring-fs1x` its digit and
//! could name 16 mounts).

const std = @import("std");
const builtin = @import("builtin");

/// `TASK_COMM_LEN − 1`: the kernel keeps 15 visible chars + NUL.
pub const comm_max = 15;

pub const CommBuf = [comm_max:0]u8;

/// Copy `base` into the comm buffer, NUL-terminated. A base wider than the
/// budget is a caller bug (`fitsBudget` is the tie for every pattern).
pub fn commName(buf: *CommBuf, base: []const u8) [:0]const u8 {
    std.debug.assert(base.len <= comm_max);
    @memset(buf[0..comm_max], 0);
    @memcpy(buf[0..base.len], base);
    buf[base.len] = 0;
    return buf[0..base.len :0];
}

/// Does `base` fit the comm budget (no silent truncation)?
pub fn fitsBudget(base: []const u8) bool {
    return base.len <= comm_max;
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

/// The CPU accounting classes (charter §9, amended 2026-10-05: the
/// per-core FUSE handler lanes are `sqz-fuse-lane<N>`, the ring workers
/// `sqz-fuse-ur<qid>`), first prefix match wins; `other` catches the rest
/// (main, `sqz-lease*`, `sqz-clw-*`, `sqz-fdwatch`, `sqz-signal`).
pub const Class = struct { prefix: []const u8, class: []const u8 };

pub const classes = [_]Class{
    .{ .prefix = "sqz-fuse-lane", .class = "sqz-fuse-lane" },
    .{ .prefix = "sqz-fuse-ur", .class = "sqz-fuse-ur" },
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

test "every charter pattern at its widest index fits the 15-char budget with the index intact" {
    // `sqz-fuse-ur` + a qid up to 4 digits (`possible_cpus` ≤ 8192 on the
    // kernel's `CONFIG_NR_CPUS` ceiling); `sqz-fuse-lane` + 2 digits is the
    // lane count a 15-char comm can carry.
    const worst = [_][]const u8{ "sqz-ipc-svc63", "sqz-jrnl9999", "sqz-fuse-lane99", "sqz-fuse-ur8191", "sqz-uring-fs63", "sqz-nvme1023", "sqz-membudget", "sqz-timer" };
    var buf: CommBuf = undefined;
    for (worst) |base| {
        try testing.expect(fitsBudget(base));
        const n = commName(&buf, base);
        try testing.expectEqualStrings(base, n);
        try testing.expectEqual(@as(u8, 0), n[n.len]);
        try testing.expect(n.len <= comm_max);
    }
    try testing.expect(!fitsBudget("sqz-fuse-lane100"));
    try testing.expectEqualStrings("sqz-timer", commName(&buf, "sqz-timer"));
}

test "classification: the amended charter names; first prefix wins; other catches the rest" {
    try testing.expectEqualStrings("sqz-fuse-ur", classify("sqz-fuse-ur17"));
    try testing.expectEqualStrings("sqz-fuse-lane", classify("sqz-fuse-lane3"));
    try testing.expectEqualStrings("sqz-nvme", classify("sqz-nvme0"));
    try testing.expectEqualStrings("sqz-jrnl", classify("sqz-jrnl1"));
    try testing.expectEqualStrings("other", classify("sqz-lease-io"));
    try testing.expectEqualStrings("other", classify("sqz-uring-fs3"));
    try testing.expectEqualStrings("other", classify("squeezefs"));
    // The reference's crate-derived names are not carried (decision 0003 Q6).
    try testing.expectEqualStrings("other", classify("fuse3-tpc3"));
    try testing.expectEqualStrings("other", classify("f3-ur17"));
}
