//! The O_DIRECT grain law (charter §5.1 "Device posture";
//! `docs/porting/uring_fs.md` §3.2, §3.4, §3.5): pure arithmetic every
//! direct metadata shape is checked against, plus the Linux probes that
//! derive a device's grain. The pure half runs on every host; the probes
//! compile only on Linux.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const derive = @import("../core/derive.zig");

/// The ladder's last rung: 4096 is the largest logical block size in the
/// field (4Kn drives), so an unknown device is never under-aligned.
pub const default_grain: u64 = 4096;

/// The admission ceiling: the ring's page arithmetic places every unit at a
/// 4 KiB multiple (superblock sector, appender pages, ledger slots, bitmap
/// pages, node images), so a grain above a page would leave every one of them
/// under-aligned. A grain within the page always divides a page boundary.
pub const max_grain: u64 = 4096;

pub const GrainError = error{GrainNotAdmissible};

/// The ONE grain law: a power of two ≤ 4096 is admitted verbatim; anything
/// else is refused, never clamped. WHY refuse: a wider or odd grain clamped to
/// the default would leave every write under-aligned, each then failing with
/// EINVAL under a misattributed message far from the cause.
pub fn admitMetaIoGrain(grain: u64) GrainError!u64 {
    if (grain == 0 or !std.math.isPowerOfTwo(grain) or grain > max_grain) return error.GrainNotAdmissible;
    return grain;
}

/// Where the grain came from; the label rides every log line so an operator
/// can tell a kernel answer from a sysfs answer from the default.
pub const ProbeSource = enum {
    statx_dioalign,
    sysfs_logical_block_size,
    default,

    pub fn label(self: ProbeSource) []const u8 {
        return switch (self) {
            .statx_dioalign => "statx(STATX_DIOALIGN)",
            .sysfs_logical_block_size => "sysfs logical_block_size",
            .default => "the default",
        };
    }
};

/// The refusal text: device, source, the grain in bytes, and REFUSING. It
/// never says "clamp" because nothing is clamped.
pub fn formatGrainRefusal(buf: []u8, path: []const u8, source: ProbeSource, grain: u64) []const u8 {
    return std.fmt.bufPrint(buf, "metadata device {s}: {s} reports a direct-I/O grain of {d} B, which is not a power of two within one 4096 B page — the ring's page arithmetic cannot honour it, REFUSING the registration", .{ path, source.label(), grain }) catch buf[0..0];
}

/// Buffer alignment for direct I/O: at least the grain (a buffer aligned to
/// less than the grain fails the kernel's O_DIRECT check on most drivers), and
/// a power of two so an allocator can honour it.
pub fn memAlignFor(mem_align: u64, grain: u64) usize {
    return @intCast(derive.nextPow2(@max(mem_align, grain)));
}

/// `/sys/dev/block/M:m/queue/logical_block_size` — the text is a decimal with
/// a trailing newline; returned VERBATIM (no clamp — whether the ring can
/// honour it is `admitMetaIoGrain`'s verdict).
pub fn parseLogicalBlockSize(text: []const u8) ?u64 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    return std.fmt.parseInt(u64, t, 10) catch null;
}

pub const SysfsRung = enum {
    /// The device's own queue (a whole disk).
    own,
    /// `../queue`: a partition has no queue of its own; its parent disk does.
    parent,
};

pub fn sysfsQueuePath(buf: []u8, major: u32, minor: u32, rung: SysfsRung) []const u8 {
    return switch (rung) {
        .own => std.fmt.bufPrint(buf, "/sys/dev/block/{d}:{d}/queue/logical_block_size", .{ major, minor }) catch buf[0..0],
        .parent => std.fmt.bufPrint(buf, "/sys/dev/block/{d}:{d}/../queue/logical_block_size", .{ major, minor }) catch buf[0..0],
    };
}

/// The write law's shape check: a direct write must start and end on the
/// grain. There is no RMW for a product write — the aligned form belongs
/// beside the shape, so a miss here is a caller bug. Grain ≤ 1 is buffered.
pub fn writeShapeAligned(offset: u64, len: usize, grain: u64) bool {
    if (grain <= 1) return true;
    return offset % grain == 0 and len % grain == 0;
}

/// Only the POINTER is checked for a buffer; its length was already checked
/// against the grain. `align <= 1` is always aligned.
pub fn ptrAligned(ptr: *const anyopaque, alignment: usize) bool {
    if (alignment <= 1) return true;
    return @intFromPtr(ptr) % alignment == 0;
}

/// A narrow read's widened form: `[start, start+len)` covers the grain
/// multiples around `[offset, offset+size)`; the caller's bytes begin `skip`
/// in. `widened` is the cold-path instrument (`meta_io_read_widened`).
pub const Span = struct {
    start: u64,
    len: u64,
    skip: usize,
    widened: bool,
};

pub fn readSpan(offset: u64, size: u64, grain: u64) Span {
    if (grain <= 1) return .{ .start = offset, .len = size, .skip = 0, .widened = false };
    const start = offset - offset % grain;
    const end = derive.ceilDiv(offset + size, grain) * grain;
    return .{
        .start = start,
        .len = end - start,
        .skip = @intCast(offset - start),
        .widened = start != offset or end != offset + size,
    };
}

pub const SliceBounds = struct { lo: usize, hi: usize };

/// Completion slices `[skip, min(filled, skip+want))` out of the widened
/// buffer; a short read shortens the answer and an EOF before `skip` empties
/// it (reads past the end of a file are not an error).
pub fn readSliceBounds(filled: usize, skip: usize, want: usize) SliceBounds {
    const end = @min(filled, skip + want);
    if (end <= skip) return .{ .lo = 0, .hi = 0 };
    return .{ .lo = skip, .hi = end };
}

/// A direct continuation must resume on the grain; a short read that ends off
/// it is the file's end (the kernel never splits a direct read mid-sector).
pub fn shouldContinueRead(filled: usize, total: usize, grain: u64) bool {
    if (filled >= total) return false;
    if (grain <= 1) return true;
    return filled % grain == 0;
}

/// `pad_to_grain`'s length: zero-extend to the next grain multiple. Grain ≤ 1
/// (buffered) keeps the length verbatim.
pub fn paddedLen(len: usize, grain: u64) usize {
    if (grain <= 1) return len;
    return @intCast(derive.ceilDiv(len, grain) * grain);
}

/// What the ladder found: the grain and the buffer alignment, and which rung
/// answered.
pub const Probe = struct {
    source: ProbeSource,
    grain: u64,
    mem_align: u64,
};

/// The facts about a path the registration needs: its type and (for a block
/// device) its device number for the sysfs rung.
pub const PathStat = struct {
    is_file: bool,
    is_bdev: bool,
    rdev_major: u32,
    rdev_minor: u32,
    size: u64,
};

pub const StatError = error{ Unexpected, NameTooLong };

/// `statx` the path (BASIC_STATS + DIOALIGN in one syscall). Null when the
/// path does not exist; the DIOALIGN fields are read by `probeGrain`.
pub fn statPath(path: []const u8) StatError!?PathStat {
    if (builtin.os.tag != .linux) return null;
    var stx: linux.Statx = undefined;
    if (!try statxPath(path, &stx, .{ .TYPE = true, .SIZE = true })) return null;
    return .{
        .is_file = linux.S.ISREG(stx.mode),
        .is_bdev = linux.S.ISBLK(stx.mode),
        .rdev_major = stx.rdev_major,
        .rdev_minor = stx.rdev_minor,
        .size = stx.size,
    };
}

/// The grain ladder: `statx STATX_DIOALIGN` → sysfs `logical_block_size`
/// (own queue, then the parent disk's) → the 4096 default. The result is NOT
/// admitted here — `admitMetaIoGrain` is the one place that refuses.
pub fn probeGrain(path: []const u8) Probe {
    if (builtin.os.tag != .linux) return .{ .source = .default, .grain = default_grain, .mem_align = default_grain };
    if (statxDioAlign(path)) |p| return p;
    if (sysfsLogicalBlockSize(path)) |g| return .{ .source = .sysfs_logical_block_size, .grain = g, .mem_align = g };
    return .{ .source = .default, .grain = default_grain, .mem_align = default_grain };
}

/// Rung 1. Valid iff the kernel set the DIOALIGN bit and a non-zero offset
/// alignment (a filesystem without direct I/O reports zeros).
fn statxDioAlign(path: []const u8) ?Probe {
    var stx: linux.Statx = undefined;
    const ok = statxPath(path, &stx, .{ .DIOALIGN = true }) catch return null;
    if (!ok or !stx.mask.DIOALIGN or stx.dio_offset_align == 0) return null;
    return .{
        .source = .statx_dioalign,
        .grain = stx.dio_offset_align,
        .mem_align = @max(stx.dio_mem_align, 1),
    };
}

/// Rung 2. Block devices only: `/sys/dev/block/M:m/queue/logical_block_size`,
/// then `../queue/…` because a partition's sysfs node has no queue directory
/// of its own.
fn sysfsLogicalBlockSize(path: []const u8) ?u64 {
    const st = (statPath(path) catch return null) orelse return null;
    if (!st.is_bdev) return null;
    var pbuf: [128]u8 = undefined;
    inline for (.{ SysfsRung.own, SysfsRung.parent }) |rung| {
        const p = sysfsQueuePath(&pbuf, st.rdev_major, st.rdev_minor, rung);
        if (readSmallFile(p)) |text| if (parseLogicalBlockSize(text)) |g| return g;
    }
    return null;
}

threadlocal var small_file_buf: [64]u8 = undefined;

fn readSmallFile(path: []const u8) ?[]const u8 {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = pathZ(&zbuf, path) catch return null;
    const fd_raw = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_raw) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(fd_raw);
    defer _ = linux.close(fd);
    const n = linux.read(fd, &small_file_buf, small_file_buf.len);
    if (linux.errno(n) != .SUCCESS) return null;
    return small_file_buf[0..n];
}

/// True when the path exists; false on ENOENT/ENOTDIR; any other errno is
/// unexpected. `AT_STATX_SYNC_AS_STAT`: the same answer `stat` gives.
fn statxPath(path: []const u8, stx: *linux.Statx, mask: linux.STATX) StatError!bool {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = try pathZ(&zbuf, path);
    const rc = linux.statx(linux.AT.FDCWD, z, linux.AT.STATX_SYNC_AS_STAT, mask, stx);
    return switch (linux.errno(rc)) {
        .SUCCESS => true,
        .NOENT, .NOTDIR => false,
        else => error.Unexpected,
    };
}

/// Null-terminate a path for a raw syscall.
pub fn pathZ(buf: *[linux.PATH_MAX]u8, path: []const u8) error{NameTooLong}![:0]const u8 {
    if (path.len >= buf.len) return error.NameTooLong;
    @memcpy(buf[0..path.len], path);
    buf[path.len] = 0;
    return buf[0..path.len :0];
}

const testing = std.testing;

test "grain admission: powers of two up to 4096 verbatim; wider, odd, zero refuse" {
    for ([_]u64{ 512, 1024, 2048, 4096, 1, 2, 4 }) |g| {
        try testing.expectEqual(g, try admitMetaIoGrain(g));
    }
    for ([_]u64{ 8192, 16384, 65536, 3072, 520, 0 }) |g| {
        try testing.expectError(error.GrainNotAdmissible, admitMetaIoGrain(g));
    }
}

test "grain refusal text names the device, the source, the grain in bytes and REFUSING, never clamp" {
    var buf: [512]u8 = undefined;
    const msg = formatGrainRefusal(&buf, "/dev/nvme9n1", ProbeSource.sysfs_logical_block_size, 8192);
    try testing.expect(std.mem.indexOf(u8, msg, "/dev/nvme9n1") != null);
    try testing.expect(std.mem.indexOf(u8, msg, ProbeSource.sysfs_logical_block_size.label()) != null);
    try testing.expect(std.mem.indexOf(u8, msg, "8192 B") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "REFUSING") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "clamp") == null);
}

test "mem_align derivation: max(mem, grain) rounded up to a power of two" {
    try testing.expectEqual(@as(usize, 4096), memAlignFor(0, 4096));
    try testing.expectEqual(@as(usize, 512), memAlignFor(512, 512));
    try testing.expectEqual(@as(usize, 4096), memAlignFor(4096, 512));
    try testing.expectEqual(@as(usize, 1024), memAlignFor(520, 512));
    try testing.expectEqual(@as(usize, 1), memAlignFor(1, 1));
}

test "probe source labels are the reference's three strings" {
    try testing.expectEqualStrings("statx(STATX_DIOALIGN)", ProbeSource.statx_dioalign.label());
    try testing.expectEqualStrings("sysfs logical_block_size", ProbeSource.sysfs_logical_block_size.label());
    try testing.expectEqualStrings("the default", ProbeSource.default.label());
    try testing.expectEqual(@as(u64, 4096), default_grain);
}

test "sysfs logical_block_size parses trimmed decimal verbatim; junk yields null" {
    try testing.expectEqual(@as(?u64, 512), parseLogicalBlockSize("512\n"));
    try testing.expectEqual(@as(?u64, 4096), parseLogicalBlockSize("  4096  "));
    try testing.expectEqual(@as(?u64, 8192), parseLogicalBlockSize("8192"));
    try testing.expectEqual(@as(?u64, null), parseLogicalBlockSize(""));
    try testing.expectEqual(@as(?u64, null), parseLogicalBlockSize("abc"));
    try testing.expectEqual(@as(?u64, null), parseLogicalBlockSize("-1"));
}

test "sysfs ladder paths: the device's own queue, then the parent disk's" {
    var buf: [128]u8 = undefined;
    try testing.expectEqualStrings("/sys/dev/block/259:3/queue/logical_block_size", sysfsQueuePath(&buf, 259, 3, .own));
    try testing.expectEqualStrings("/sys/dev/block/259:3/../queue/logical_block_size", sysfsQueuePath(&buf, 259, 3, .parent));
}

test "write shape: offset and length must both land on the grain; grain 1 admits every shape" {
    try testing.expect(writeShapeAligned(0, 4096, 4096));
    try testing.expect(writeShapeAligned(8192, 512, 512));
    try testing.expect(!writeShapeAligned(1, 4096, 4096));
    try testing.expect(!writeShapeAligned(0, 4095, 4096));
    try testing.expect(writeShapeAligned(4096, 0, 4096));
    try testing.expect(writeShapeAligned(3, 3, 1));
}

test "pointer alignment: align <= 1 is always aligned; otherwise the address modulo the alignment" {
    var storage: [8192]u8 align(4096) = undefined;
    try testing.expect(ptrAligned(&storage[0], 4096));
    try testing.expect(!ptrAligned(&storage[1], 4096));
    try testing.expect(ptrAligned(&storage[512], 512));
    try testing.expect(!ptrAligned(&storage[512], 4096));
    try testing.expect(ptrAligned(&storage[1], 1));
    try testing.expect(ptrAligned(&storage[7], 0));
}

test "read span: narrow and straddling reads widen to the grain and remember the skip" {
    const g: u64 = 4096;
    // Narrow read at 2g+7 of 21 bytes: one page, skip 7.
    const narrow = readSpan(2 * g + 7, 21, g);
    try testing.expectEqual(2 * g, narrow.start);
    try testing.expectEqual(g, narrow.len);
    try testing.expectEqual(@as(usize, 7), narrow.skip);
    try testing.expect(narrow.widened);
    // Straddle 3g-4 of 8: two pages, skip g-4.
    const straddle = readSpan(3 * g - 4, 8, g);
    try testing.expectEqual(2 * g, straddle.start);
    try testing.expectEqual(2 * g, straddle.len);
    try testing.expectEqual(@as(usize, g - 4), straddle.skip);
    try testing.expect(straddle.widened);
    // Exactly aligned: nothing widens.
    const exact = readSpan(g, 2 * g, g);
    try testing.expectEqual(g, exact.start);
    try testing.expectEqual(2 * g, exact.len);
    try testing.expectEqual(@as(usize, 0), exact.skip);
    try testing.expect(!exact.widened);
    // Buffered (grain 1): identity.
    const buffered = readSpan(5, 3, 1);
    try testing.expectEqual(@as(u64, 5), buffered.start);
    try testing.expectEqual(@as(u64, 3), buffered.len);
    try testing.expectEqual(@as(usize, 0), buffered.skip);
    try testing.expect(!buffered.widened);
}

test "read slice: [skip, min(filled, skip+want)); a short read shortens, an EOF before skip empties" {
    try testing.expectEqual(SliceBounds{ .lo = 7, .hi = 28 }, readSliceBounds(4096, 7, 21));
    try testing.expectEqual(SliceBounds{ .lo = 7, .hi = 10 }, readSliceBounds(10, 7, 21));
    try testing.expectEqual(SliceBounds{ .lo = 0, .hi = 0 }, readSliceBounds(5, 7, 21));
    try testing.expectEqual(SliceBounds{ .lo = 0, .hi = 0 }, readSliceBounds(0, 0, 0));
    try testing.expectEqual(SliceBounds{ .lo = 0, .hi = 3 }, readSliceBounds(3, 0, 10));
}

test "read continuation resumes only on the grain: a short read off the grain is the file's end" {
    try testing.expect(shouldContinueRead(4096, 8192, 4096));
    try testing.expect(!shouldContinueRead(4000, 8192, 4096));
    try testing.expect(!shouldContinueRead(8192, 8192, 4096));
    try testing.expect(shouldContinueRead(3, 10, 1));
    try testing.expect(!shouldContinueRead(0, 0, 1));
}

test "pad to grain: lengths round up to the grain; grain <= 1 is verbatim" {
    try testing.expectEqual(@as(usize, 4096), paddedLen(1, 4096));
    try testing.expectEqual(@as(usize, 4096), paddedLen(4096, 4096));
    try testing.expectEqual(@as(usize, 8192), paddedLen(4097, 4096));
    try testing.expectEqual(@as(usize, 0), paddedLen(0, 4096));
    try testing.expectEqual(@as(usize, 1024), paddedLen(513, 512));
    try testing.expectEqual(@as(usize, 123), paddedLen(123, 1));
    try testing.expectEqual(@as(usize, 123), paddedLen(123, 0));
}

test "the grain ladder on a regular file lands on a power of two <= 4096 from one of the three sources" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    const io = testing.io;
    var f = try tmp.dir.createFile(io, "grain.dat", .{});
    f.close(io);
    var path_buf: [std.fs.max_path_bytes]u8 = undefined;
    const dir_len = try tmp.dir.realPath(io, &path_buf);
    var full: [std.fs.max_path_bytes]u8 = undefined;
    const path = try std.fmt.bufPrint(&full, "{s}/grain.dat", .{path_buf[0..dir_len]});
    const probe = probeGrain(path);
    try testing.expect(std.math.isPowerOfTwo(probe.grain));
    try testing.expect(probe.grain <= default_grain);
    try testing.expect(probe.mem_align >= 1);
    const st = (try statPath(path)).?;
    try testing.expect(st.is_file);
    try testing.expect(!st.is_bdev);
    try testing.expectEqual(@as(?PathStat, null), try statPath("/definitely/not/here"));
}
