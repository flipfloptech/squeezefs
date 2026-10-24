//! Lazily materialized tables (the idle-RSS law, `docs/evidence/rung-10-
//! idle-rss.md`): an anonymous mapping the kernel backs page by page as it
//! is first touched, so a table sized for a reservation costs the entries
//! the workload reached, never the reservation.
//!
//! Never `Allocator.alloc` for such a table: the interface's
//! `@memset(undefined)` writes every byte at birth in the safe modes, and
//! ReleaseSafe is the product's build — an idle daemon's read cache paid
//! 1.4 GB of item tables, index and slot arrays sized by the machine's RAM
//! that way (page-allocated or not: the memset is the interface's).
//!
//! The pages are born zero, so a table whose empty state is the zero value
//! needs no initialization pass (that pass would touch it all again).
//! `MADV_NOHUGEPAGE`: the tables are touched sparsely by design, and under
//! THP `always` (the Omarchy venue's default) a first touch would fault a
//! whole 2 MiB page — one entry per stripe of a 32-stripe table would be
//! 64 MiB resident. PORTABLE (the advice is Linux's).

const std = @import("std");
const builtin = @import("builtin");
const Alignment = std.mem.Alignment;

/// `n` zero-filled, unbacked `T`s (page-aligned).
pub fn alloc(comptime T: type, n: usize) error{OutOfMemory}![]T {
    if (n == 0) return &.{};
    const len = std.math.mul(usize, @sizeOf(T), n) catch return error.OutOfMemory;
    const raw = std.heap.page_allocator.rawAlloc(len, .fromByteUnits(@alignOf(T)), @returnAddress()) orelse return error.OutOfMemory;
    if (builtin.os.tag == .linux) _ = std.os.linux.madvise(@alignCast(raw), std.mem.alignForward(usize, len, std.heap.pageSize()), std.os.linux.MADV.NOHUGEPAGE);
    const ptr: [*]T = @ptrCast(@alignCast(raw));
    return ptr[0..n];
}

pub fn free(comptime T: type, s: []T) void {
    if (s.len == 0) return;
    const bytes: [*]u8 = @ptrCast(s.ptr);
    std.heap.page_allocator.rawFree(bytes[0 .. s.len * @sizeOf(T)], .fromByteUnits(@alignOf(T)), @returnAddress());
}

const testing = std.testing;

test "a lazy table is zero-filled and unbacked until touched: 64 MiB reserved, one page written, one page resident" {
    const t = try alloc(u64, 8 << 20);
    defer free(u64, t);
    try testing.expectEqual(@as(usize, 8 << 20), t.len);
    try testing.expectEqual(@as(u64, 0), t[0]);
    try testing.expectEqual(@as(u64, 0), t[t.len - 1]);
    t[12345] = 7;
    try testing.expectEqual(@as(u64, 7), t[12345]);
    if (builtin.os.tag != .linux) return;
    const page = std.heap.pageSize();
    const bytes: [*]u8 = @ptrCast(t.ptr);
    const len = t.len * @sizeOf(u64);
    const vec = try testing.allocator.alloc(u8, len / page);
    defer testing.allocator.free(vec);
    try testing.expectEqual(std.os.linux.E.SUCCESS, std.os.linux.errno(std.os.linux.syscall3(.mincore, @intFromPtr(bytes), len, @intFromPtr(vec.ptr))));
    var resident: usize = 0;
    for (vec) |v| resident += v & 1;
    // The three pages read or written above (index 0, the last, 12345):
    // reading a never-written page maps the shared zero page, which
    // mincore reports resident too — never the table.
    try testing.expect(resident <= 3);
}

test "zero entries is the empty slice; free of it is a no-op" {
    const t = try alloc(u32, 0);
    try testing.expectEqual(@as(usize, 0), t.len);
    free(u32, t);
}
