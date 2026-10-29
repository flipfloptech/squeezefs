//! FS_IOC_FIEMAP (charter §4 "FIEMAP"): the law that turns a file's
//! layout into extents, the request the interposer's verb carries, and the
//! reply. Portable and pure: `kv_fs` reads the layout (and pages a
//! tree-mapped map through the data plane) and feeds its runs to an
//! `Emitter`; nothing here touches a store, a ring or a device.
//!
//! Who asks: Linux never forwards FIEMAP to a FUSE server — `do_vfs_ioctl`
//! (`fs/ioctl.c`) catches `FS_IOC_FIEMAP` for every file and
//! `ioctl_fiemap` answers EOPNOTSUPP because FUSE registers no `->fiemap`
//! (linux 7.3; no kernel patch is carried, owner ruling 2026-10-28). The
//! LD_PRELOAD interposer answers it instead (decision 0003 §CJ): its
//! `ioctl` hook sends the IPC verb `FiemapReq` with the fd, the IPC host
//! runs `prep` (`fiemap_prep` restated for a FUSE superblock: EINVAL,
//! EFBIG, the length clamp, EBADR with `fm_flags` = the refused bits) and
//! hands one `FUSE_IOCTL`-shaped request per batch to the handler: `cmd`
//! `FS_IOC_FIEMAP`, the in bytes one `struct fiemap` (32 B; `fm_flags`
//! SYNC or 0, `fm_extent_count` = the extents the batch may carry, 0 = the
//! count alone), `out_size` = 32 + 56 × that; the reply is `struct fiemap`
//! (`fm_mapped_extents`) ‖ that many `struct fiemap_extent`. A batch whose
//! last extent lacks `FIEMAP_EXTENT_LAST` continues from that extent's end
//! (every emitted extent is maximal, so a batch edge never splits one).
//!
//! The law: the file's mapping at its TRUE allocation unit, clipped to the
//! range at the LBA grain (4 KiB, `FIGETBSZ` on a FUSE mount) as iomap
//! clips:
//! - a striped block bound WHOLE is one stripe block of extent (4 MiB by
//!   default) at its device address — a punch inside a block leaves it
//!   mapped (its bytes are written zeros, `rung-10-punch-edge.md`), a
//!   block punched whole is a hole — UNWRITTEN when it is a fallocate
//!   reservation (`M_UNWRITTEN`); an unbound block is a gap; bindings that
//!   continue each other logically and physically on one volume, with
//!   equal flags, merge; a block past EOF is reported (allocation truth);
//! - a packed tenant (and a staged file's tenant-shaped durable copy) is
//!   `[0, ceil_4k(image))` at `offset + rel_off`: its slot is its own 4 KiB
//!   grains of a stripe block other files share, so at FIEMAP's block size
//!   (FIGETBSZ) no block holds another file's byte — an ordinary aligned
//!   extent, never DATA_TAIL (whose NOT_ALIGNED would be false: xfstests'
//!   fiemap-tester refuses NOT_ALIGNED on an aligned extent);
//! - inline data is `[0, len)` with `DATA_INLINE | NOT_ALIGNED`, physical
//!   0 and UNKNOWN (the bytes are a metadata record's in a CoW tree node
//!   that moves);
//! - a staged write-cache resident not yet promoted is `[0, ceil_4k(size))`
//!   with `DELALLOC | UNKNOWN` and physical 0 — `FIEMAP_FLAG_SYNC` promotes
//!   it first (the fsync data leg, as the kernel's `filemap_write_and_wait`
//!   writes the page cache back), so a synced map never shows one;
//! - `fe_physical` = `(volume ordinal + 1) << 48 | device byte offset`: the
//!   mount's ordinal of the volume (the set config's order of its live
//!   volumes) above a 256 TiB device address; physical 0 ⇔ UNKNOWN — a
//!   binding whose volume is not bound here, whose address does not fit or
//!   which is DAMAGED, and every extent with no device address; a
//!   transformed (compressed/encrypted) volume's extents are ENCODED, one
//!   per block (its device stride is not the logical one);
//! - SHARED never (no reflink is exposed); LAST on the last extent of the
//!   range; the kernel's implied flags are set here (`fiemap_fill_next_extent`
//!   adds UNKNOWN to DELALLOC and NOT_ALIGNED to DATA_INLINE/DATA_TAIL —
//!   no kernel stands between this reply and the caller).

const std = @import("std");
const linux = std.os.linux;
const layout = @import("../../data/layout.zig");
const derive = @import("../../core/derive.zig");
const fuzzkit = @import("../../core/fuzzkit.zig");

/// `struct fiemap` (`include/uapi/linux/fiemap.h`, 32 B).
pub const Fiemap = extern struct {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
};

/// `struct fiemap_extent` (56 B).
pub const Extent = extern struct {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [2]u64,
    fe_flags: u32,
    fe_reserved: [3]u32,
};

comptime {
    std.debug.assert(@sizeOf(Fiemap) == 32);
    std.debug.assert(@sizeOf(Extent) == 56);
    std.debug.assert(@offsetOf(Extent, "fe_flags") == 40);
}

/// `_IOWR('f', 11, struct fiemap)`.
pub const FS_IOC_FIEMAP: u32 = linux.IOCTL.IOWR('f', 11, Fiemap);

/// `fm_flags` bits (`include/uapi/linux/fiemap.h`).
pub const FIEMAP_FLAG_SYNC: u32 = 0x1;
pub const FIEMAP_FLAG_XATTR: u32 = 0x2;
pub const FIEMAP_FLAG_CACHE: u32 = 0x4;

/// `fe_flags` bits (`include/uapi/linux/fiemap.h`).
pub const FIEMAP_EXTENT_LAST: u32 = 0x1;
pub const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
pub const FIEMAP_EXTENT_DELALLOC: u32 = 0x4;
pub const FIEMAP_EXTENT_ENCODED: u32 = 0x8;
pub const FIEMAP_EXTENT_DATA_ENCRYPTED: u32 = 0x80;
pub const FIEMAP_EXTENT_NOT_ALIGNED: u32 = 0x100;
pub const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;
pub const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x400;
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;
pub const FIEMAP_EXTENT_MERGED: u32 = 0x1000;
pub const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

/// The grain extents are reported at: the LBA grain every mapping law is
/// written in, and the block size a FUSE mount's `FIGETBSZ` answers.
pub const GRAIN: u64 = layout.GRAIN;

/// `fe_physical`'s volume half: the ordinal + 1 above the 256 TiB device
/// address (`set_config.MAX_DATA_VOLUMES` = 4096 ordinals fit the 16 bits).
pub const PHYS_VOLUME_SHIFT: u6 = 48;
pub const PHYS_OFFSET_LIMIT: u64 = @as(u64, 1) << PHYS_VOLUME_SHIFT;

/// The request decoded.
pub const Request = struct {
    start: u64,
    length: u64,
    flags: u32,
    /// 0 = the count alone.
    extent_count: u32,

    pub fn sync(self: Request) bool {
        return self.flags & FIEMAP_FLAG_SYNC != 0;
    }
};

/// `MAX_LFS_FILESIZE` on a 64-bit kernel: a FUSE superblock's `s_maxbytes`
/// (`fuse_fill_super_common`), the bound `fiemap_prep` clips a range to.
pub const MAX_BYTES: u64 = std.math.maxInt(i64);
/// `FIEMAP_MAX_EXTENTS` (`include/linux/fiemap.h`): `ioctl_fiemap`'s EINVAL
/// bound on `fm_extent_count`.
pub const MAX_EXTENTS: u32 = std.math.maxInt(u32) / @sizeOf(Extent) - 1;

/// `prep`'s verdict: the request to serve, or the errno the caller answers
/// with the `fm_flags` it reads back (EBADR: exactly the refused bits).
pub const Prepared = union(enum) {
    ok: Request,
    refused: struct { errno: linux.E, flags: u32 },
};

/// `ioctl_fiemap`'s count bound, then `fiemap_prep` with no supported flag
/// but SYNC (iomap's posture): a zero length is EINVAL, a start at or past
/// `s_maxbytes` EFBIG, a length past it is clipped, any flag but SYNC
/// (XATTR, CACHE, an unknown bit) EBADR with `fm_flags` = those bits.
pub fn prep(f: Fiemap) Prepared {
    if (f.fm_extent_count > MAX_EXTENTS) return .{ .refused = .{ .errno = .INVAL, .flags = f.fm_flags } };
    if (f.fm_length == 0) return .{ .refused = .{ .errno = .INVAL, .flags = f.fm_flags } };
    if (f.fm_start >= MAX_BYTES) return .{ .refused = .{ .errno = .FBIG, .flags = f.fm_flags } };
    const len = if (f.fm_length > MAX_BYTES or MAX_BYTES - f.fm_length < f.fm_start) MAX_BYTES - f.fm_start else f.fm_length;
    const incompat = f.fm_flags & ~FIEMAP_FLAG_SYNC;
    if (incompat != 0) return .{ .refused = .{ .errno = .BADR, .flags = incompat } };
    return .{ .ok = .{ .start = f.fm_start, .length = len, .flags = f.fm_flags, .extent_count = f.fm_extent_count } };
}

pub const RequestError = error{
    /// The in bytes are not one `struct fiemap`, or `out_size` cannot hold
    /// its header and whole extents.
    InvalidArgument,
    /// A flag `prep` refuses before any request (only `FIEMAP_FLAG_SYNC`
    /// reaches the handler).
    NotSupported,
};

/// The request one batch carries (`prep` already passed it).
pub fn decodeRequest(in: []const u8, out_size: u32) RequestError!Request {
    if (in.len != @sizeOf(Fiemap)) return error.InvalidArgument;
    if (out_size < @sizeOf(Fiemap) or (out_size - @sizeOf(Fiemap)) % @sizeOf(Extent) != 0) return error.InvalidArgument;
    const f = std.mem.bytesToValue(Fiemap, in[0..@sizeOf(Fiemap)]);
    if (f.fm_flags & ~FIEMAP_FLAG_SYNC != 0) return error.NotSupported;
    if (f.fm_length == 0) return error.InvalidArgument;
    if (f.fm_extent_count != (out_size - @sizeOf(Fiemap)) / @sizeOf(Extent)) return error.InvalidArgument;
    return .{ .start = f.fm_start, .length = f.fm_length, .flags = f.fm_flags, .extent_count = f.fm_extent_count };
}

/// The extents a reply window of `window_len` bytes (after the ioctl
/// header) can carry for `r`: the request's batch, never past the window.
pub fn batchCap(r: Request, window_len: usize) u32 {
    if (window_len < @sizeOf(Fiemap)) return 0;
    const fits = (window_len - @sizeOf(Fiemap)) / @sizeOf(Extent);
    return @intCast(@min(@as(usize, r.extent_count), fits));
}

/// The reply laid into `out` (the ioctl's data window): the header with
/// `mapped` and the `extents` already written behind it by the `Emitter`
/// (`extentsOf`). Returns the reply's byte length.
pub fn encodeReply(r: Request, mapped: u32, written: u32, out: []u8) usize {
    const h: Fiemap = .{ .fm_start = r.start, .fm_length = r.length, .fm_flags = r.flags, .fm_mapped_extents = mapped, .fm_extent_count = r.extent_count, .fm_reserved = 0 };
    @memcpy(out[0..@sizeOf(Fiemap)], std.mem.asBytes(&h));
    return @sizeOf(Fiemap) + @as(usize, written) * @sizeOf(Extent);
}

/// The extent slots of a reply window (after the header): `cap` of them.
pub fn extentsOf(out: []u8, cap: u32) []align(1) Extent {
    const bytes = out[@sizeOf(Fiemap)..][0 .. @as(usize, cap) * @sizeOf(Extent)];
    return std.mem.bytesAsSlice(Extent, bytes);
}

/// One contiguous piece of the file's mapping, ascending by `logical`.
pub const Run = struct {
    logical: u64,
    len: u64,
    /// The encoded device address; null = none (inline, delalloc,
    /// unknown) — reported as 0.
    phys: ?u64,
    flags: u32,
    /// A transformed block: never merged with its neighbour (its device
    /// stride is not its logical one).
    merge: bool = true,
};

/// `fe_physical` of `len` device bytes at `dev_off` on the volume at
/// `ordinal`; null when the volume is not bound here or the span does not
/// fit below the volume bits.
pub fn physOf(ordinal: ?u16, dev_off: u64, len: u64) ?u64 {
    const o = ordinal orelse return null;
    const end = std.math.add(u64, dev_off, len) catch return null;
    if (end > PHYS_OFFSET_LIMIT) return null;
    return ((@as(u64, o) + 1) << PHYS_VOLUME_SHIFT) | dev_off;
}

/// `len` bytes of whole blocks consecutive on one device from `dev_off` (a
/// block-map tree RUN record) at logical `base`.
pub fn blocksRun(base: u64, len: u64, ordinal: ?u16, dev_off: u64) Run {
    const phys = physOf(ordinal, dev_off, len);
    return .{ .logical = base, .len = len, .phys = phys, .flags = if (phys == null) FIEMAP_EXTENT_UNKNOWN else 0 };
}

/// The run of a binding at logical `base`: a whole block covers `block`
/// bytes, a tenant window its slot `ceil_4k(len)` at `offset + rel_off`. A
/// quarantined (DAMAGED) binding is no readable address: UNKNOWN at 0.
pub fn mappingRun(base: u64, m: layout.Mapping, block: u64, ordinal: ?u16, encoded: bool) Run {
    const len = if (m.whole_block) block else layout.ceilGrain(m.len);
    const phys = if (m.damaged) null else physOf(ordinal, m.offset + m.rel_off, len);
    var flags: u32 = 0;
    if (m.unwritten) flags |= FIEMAP_EXTENT_UNWRITTEN;
    if (phys == null) flags |= FIEMAP_EXTENT_UNKNOWN;
    if (encoded) flags |= FIEMAP_EXTENT_ENCODED;
    return .{ .logical = base, .len = len, .phys = phys, .flags = flags, .merge = !encoded };
}

/// Inline data: the record's bytes, `[0, len)`, no stable device address.
pub fn inlineRun(len: u64) ?Run {
    if (len == 0) return null;
    return .{ .logical = 0, .len = len, .phys = null, .flags = FIEMAP_EXTENT_DATA_INLINE | FIEMAP_EXTENT_NOT_ALIGNED | FIEMAP_EXTENT_UNKNOWN };
}

/// A write-cache resident not yet promoted: `[0, ceil_4k(size))`, no
/// device address yet.
pub fn delallocRun(size: u64) ?Run {
    if (size == 0) return null;
    return .{ .logical = 0, .len = derive.alignUp(size, GRAIN), .phys = null, .flags = FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_UNKNOWN };
}

/// The range a request maps, clipped outward to the grain (saturating).
pub const Range = struct {
    lo: u64,
    hi: u64,

    pub fn of(start: u64, length: u64) Range {
        const end = std.math.add(u64, start, length) catch std.math.maxInt(u64);
        const hi = std.math.add(u64, end, GRAIN - 1) catch std.math.maxInt(u64);
        return .{ .lo = derive.alignDown(start, GRAIN), .hi = if (hi == std.math.maxInt(u64)) hi else derive.alignDown(hi, GRAIN) };
    }

    pub fn firstBlock(self: Range, block: u64) u64 {
        return self.lo / block;
    }
};

/// Turns ascending runs into the reply's extents: clips each to the range,
/// merges the ones that continue each other, writes them into `out` (or
/// counts them when the request asked for the count alone) and marks the
/// last extent of the range LAST. A full `out` stops the walk without LAST:
/// the caller asks again from the last extent's end.
pub const Emitter = struct {
    range: Range,
    out: []align(1) Extent,
    count_only: bool,
    /// Extents written (or counted).
    n: u32 = 0,
    pending: ?Run = null,
    full: bool = false,

    pub const Verdict = enum { more, done };

    pub fn init(range: Range, out: []align(1) Extent, count_only: bool) Emitter {
        return .{ .range = range, .out = out, .count_only = count_only };
    }

    /// Feed the next run (ascending, disjoint). `.done`: stop feeding (the
    /// run lies past the range, or `out` is full).
    pub fn push(self: *Emitter, r: Run) Verdict {
        if (self.full) return .done;
        if (r.len == 0) return .more;
        const r_end = r.logical +| r.len;
        if (r.logical >= self.range.hi) return .done;
        if (r_end <= self.range.lo) return .more;
        const s = @max(r.logical, self.range.lo);
        const e = @min(r_end, self.range.hi);
        const clipped: Run = .{ .logical = s, .len = e - s, .phys = if (r.phys) |p| p + (s - r.logical) else null, .flags = r.flags, .merge = r.merge };
        if (self.pending) |*p| {
            if (continues(p.*, clipped)) {
                p.len += clipped.len;
                return .more;
            }
            if (!self.emit(p.*, 0)) {
                self.full = true;
                return .done;
            }
        }
        self.pending = clipped;
        return .more;
    }

    /// The walk ended (the file's mapping, or the range, is exhausted):
    /// the held extent is the range's last. Returns the extents written or
    /// counted.
    pub fn finish(self: *Emitter) u32 {
        if (!self.full) if (self.pending) |p| {
            if (self.emit(p, FIEMAP_EXTENT_LAST)) self.pending = null;
        };
        return self.n;
    }

    fn emit(self: *Emitter, r: Run, last: u32) bool {
        if (self.count_only) {
            self.n += 1;
            return true;
        }
        if (self.n == self.out.len) return false;
        self.out[self.n] = .{ .fe_logical = r.logical, .fe_physical = r.phys orelse 0, .fe_length = r.len, .fe_reserved64 = @splat(0), .fe_flags = r.flags | last, .fe_reserved = @splat(0) };
        self.n += 1;
        return true;
    }

    fn continues(a: Run, b: Run) bool {
        if (!a.merge or !b.merge or a.flags != b.flags or a.logical + a.len != b.logical) return false;
        if (a.phys == null or b.phys == null) return a.phys == null and b.phys == null;
        return a.phys.? + a.len == b.phys.?;
    }
};

// ---------------------------------------------------------------------------
// Tests: the request, the reply, the translation.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB: u64 = 1 << 20;
const block4m: u64 = 4 * MiB;

fn reqBytes(f: Fiemap) [@sizeOf(Fiemap)]u8 {
    return std.mem.toBytes(f);
}

test "fiemap: a batch decodes only as `prep` passes it — one 32 B struct fiemap, out_size = 32 + 56 × fm_extent_count, fm_flags SYNC or 0 (XATTR/CACHE are EOPNOTSUPP: prep refused them EBADR first), fm_length non-zero" {
    try testing.expectEqual(@as(u32, 0xC020660B), FS_IOC_FIEMAP);
    const f: Fiemap = .{ .fm_start = 4096, .fm_length = 1 << 30, .fm_flags = FIEMAP_FLAG_SYNC, .fm_mapped_extents = 0, .fm_extent_count = 32, .fm_reserved = 0 };
    const r = try decodeRequest(&reqBytes(f), 32 + 32 * 56);
    try testing.expectEqual(@as(u64, 4096), r.start);
    try testing.expect(r.sync());
    try testing.expectEqual(@as(u32, 32), r.extent_count);
    // The count alone: out_size is the header.
    var c = f;
    c.fm_extent_count = 0;
    try testing.expectEqual(@as(u32, 0), (try decodeRequest(&reqBytes(c), 32)).extent_count);
    try testing.expectError(error.InvalidArgument, decodeRequest(&reqBytes(f), 32 + 31 * 56));
    try testing.expectError(error.InvalidArgument, decodeRequest(&reqBytes(f), 32 + 32 * 56 + 1));
    try testing.expectError(error.InvalidArgument, decodeRequest(reqBytes(f)[0..31], 32 + 32 * 56));
    var x = f;
    x.fm_flags = FIEMAP_FLAG_XATTR;
    try testing.expectError(error.NotSupported, decodeRequest(&reqBytes(x), 32 + 32 * 56));
    var z = f;
    z.fm_length = 0;
    try testing.expectError(error.InvalidArgument, decodeRequest(&reqBytes(z), 32 + 32 * 56));
    // The window bounds the batch, never the other way round.
    try testing.expectEqual(@as(u32, 32), batchCap(r, 1 << 20));
    try testing.expectEqual(@as(u32, 2), batchCap(r, 32 + 2 * 56 + 55));
    try testing.expectEqual(@as(u32, 0), batchCap(r, 16));
}

fn requestFuzz(_: void, smith: *std.testing.Smith) !void {
    var in: [64]u8 = undefined;
    const len = smith.slice(&in);
    const out_size = smith.value(u32);
    const r = decodeRequest(in[0..len], out_size) catch return;
    // Whatever decodes is one struct whose batch the out size carries.
    try testing.expectEqual(@as(usize, @sizeOf(Fiemap)), len);
    try testing.expectEqual(@as(u64, out_size), @sizeOf(Fiemap) + @as(u64, r.extent_count) * @sizeOf(Extent));
    try testing.expect(r.flags & ~FIEMAP_FLAG_SYNC == 0 and r.length != 0);
    _ = Range.of(r.start, r.length);
}

test "fuzz: decodeRequest is total on arbitrary bytes and out sizes" {
    try fuzzkit.fuzz({}, requestFuzz, .{});
}

test "fiemap: the range clips outward to the 4 KiB grain and saturates at the top (fm_length ~0 maps to the end of the address space)" {
    const r = Range.of(5000, 100);
    try testing.expectEqual(@as(u64, 4096), r.lo);
    try testing.expectEqual(@as(u64, 8192), r.hi);
    const all = Range.of(0, std.math.maxInt(u64));
    try testing.expectEqual(@as(u64, 0), all.lo);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), all.hi);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), Range.of(4096, std.math.maxInt(u64) - 100).hi);
}

test "fiemap: fe_physical = (ordinal + 1) << 48 | device offset; an unbound volume or a span past 256 TiB is null (UNKNOWN at 0); a RUN record is one run" {
    try testing.expectEqual(@as(?u64, (1 << 48) | (8 * MiB)), physOf(0, 8 * MiB, block4m));
    try testing.expectEqual(@as(?u64, (3 << 48) | 4096), physOf(2, 4096, 4096));
    try testing.expectEqual(@as(?u64, null), physOf(null, 0, 4096));
    try testing.expectEqual(@as(?u64, null), physOf(0, PHYS_OFFSET_LIMIT, 4096));
    try testing.expectEqual(@as(?u64, null), physOf(0, PHYS_OFFSET_LIMIT - 4096, 8192));
    try testing.expectEqual(@as(?u64, 0xFFFF << 48 | (PHYS_OFFSET_LIMIT - 4096)), physOf(0xFFFE, PHYS_OFFSET_LIMIT - 4096, 4096));
    const r = blocksRun(8 * block4m, 3 * block4m, 1, 40 * MiB);
    try testing.expectEqual(physOf(1, 40 * MiB, 3 * block4m), r.phys);
    try testing.expectEqual(@as(u32, 0), r.flags);
    try testing.expectEqual(FIEMAP_EXTENT_UNKNOWN, blocksRun(0, block4m, null, 0).flags);
}

/// The extents a list of runs translates to over `range`, into `buf`.
fn run(range: Range, runs: []const Run, buf: []Extent, count_only: bool) struct { n: u32, full: bool } {
    var e = Emitter.init(range, buf, count_only);
    for (runs) |r| if (e.push(r) == .done) break;
    const n = e.finish();
    return .{ .n = n, .full = e.full };
}

test "fiemap: a striped map — two contiguous whole blocks merge, a hole is a gap, an UNWRITTEN reservation is its own extent, a block on another volume does not merge; LAST on the last; SHARED never" {
    const m0 = layout.Mapping.wholeBlock(0xA, 16 * MiB, 1);
    const m1 = layout.Mapping.wholeBlock(0xA, 20 * MiB, 2);
    const m3 = layout.Mapping.unwrittenBlock(0xA, 24 * MiB, 3);
    const m4 = layout.Mapping.wholeBlock(0xB, 0, 4);
    const runs = [_]Run{
        mappingRun(0 * block4m, m0, block4m, 0, false),
        mappingRun(1 * block4m, m1, block4m, 0, false),
        // block 2: a hole
        mappingRun(3 * block4m, m3, block4m, 0, false),
        mappingRun(4 * block4m, m4, block4m, 1, false),
    };
    var buf: [8]Extent = undefined;
    const got = run(Range.of(0, std.math.maxInt(u64)), &runs, &buf, false);
    try testing.expectEqual(@as(u32, 3), got.n);
    try testing.expectEqual(@as(u64, 0), buf[0].fe_logical);
    try testing.expectEqual(2 * block4m, buf[0].fe_length);
    try testing.expectEqual(physOf(0, 16 * MiB, 0).?, buf[0].fe_physical);
    try testing.expectEqual(@as(u32, 0), buf[0].fe_flags);
    try testing.expectEqual(3 * block4m, buf[1].fe_logical);
    try testing.expectEqual(block4m, buf[1].fe_length);
    try testing.expectEqual(FIEMAP_EXTENT_UNWRITTEN, buf[1].fe_flags);
    try testing.expectEqual(4 * block4m, buf[2].fe_logical);
    try testing.expectEqual(physOf(1, 0, 0).?, buf[2].fe_physical);
    try testing.expectEqual(FIEMAP_EXTENT_LAST, buf[2].fe_flags);
    for (buf[0..got.n]) |x| try testing.expect(x.fe_flags & FIEMAP_EXTENT_SHARED == 0);
    // The count alone: the same three, nothing written.
    var none: [0]Extent = .{};
    try testing.expectEqual(@as(u32, 3), run(Range.of(0, std.math.maxInt(u64)), &runs, &none, true).n);
    // An unwritten twin of a written block's neighbour does not merge with it.
    const twin = [_]Run{ mappingRun(0, m0, block4m, 0, false), mappingRun(block4m, layout.Mapping.unwrittenBlock(0xA, 20 * MiB, 2), block4m, 0, false) };
    try testing.expectEqual(@as(u32, 2), run(Range.of(0, std.math.maxInt(u64)), &twin, &buf, false).n);
}

test "fiemap: the range clips at the grain (iomap's posture) and the extent's physical moves with its start; a run past the range stops the walk and the clipped last is LAST" {
    const m0 = layout.Mapping.wholeBlock(0xA, 16 * MiB, 1);
    const m1 = layout.Mapping.wholeBlock(0xA, 40 * MiB, 2);
    const runs = [_]Run{ mappingRun(0, m0, block4m, 0, false), mappingRun(block4m, m1, block4m, 0, false) };
    var buf: [4]Extent = undefined;
    const got = run(Range.of(MiB + 100, MiB), &runs, &buf, false);
    try testing.expectEqual(@as(u32, 1), got.n);
    try testing.expectEqual(MiB, buf[0].fe_logical);
    try testing.expectEqual(MiB + 4096, buf[0].fe_length);
    try testing.expectEqual(physOf(0, 16 * MiB + MiB, 0).?, buf[0].fe_physical);
    try testing.expectEqual(FIEMAP_EXTENT_LAST, buf[0].fe_flags);
    // A range wholly inside the hole past the map: nothing.
    try testing.expectEqual(@as(u32, 0), run(Range.of(8 * MiB, MiB), &runs, &buf, false).n);
}

test "fiemap: a full reply stops WITHOUT LAST and the next batch from the last extent's end finds the rest, LAST on the final one" {
    var runs: [5]Run = undefined;
    for (&runs, 0..) |*r, i| r.* = mappingRun(2 * i * block4m, layout.Mapping.wholeBlock(0xA, 2 * i * block4m, 1), block4m, 0, false);
    var buf: [2]Extent = undefined;
    var got = run(Range.of(0, std.math.maxInt(u64)), &runs, &buf, false);
    try testing.expectEqual(@as(u32, 2), got.n);
    try testing.expect(got.full);
    try testing.expectEqual(@as(u32, 0), buf[1].fe_flags & FIEMAP_EXTENT_LAST);
    var start = buf[1].fe_logical + buf[1].fe_length;
    var seen: u32 = got.n;
    while (true) {
        got = run(Range.of(start, std.math.maxInt(u64) - start), &runs, &buf, false);
        seen += got.n;
        if (buf[got.n - 1].fe_flags & FIEMAP_EXTENT_LAST != 0) break;
        start = buf[got.n - 1].fe_logical + buf[got.n - 1].fe_length;
    }
    try testing.expectEqual(@as(u32, 5), seen);
    try testing.expectEqual(8 * block4m, buf[got.n - 1].fe_logical);
    // Exactly as many slots as extents: the held last one still fits.
    var exact: [5]Extent = undefined;
    got = run(Range.of(0, std.math.maxInt(u64)), &runs, &exact, false);
    try testing.expectEqual(@as(u32, 5), got.n);
    try testing.expectEqual(FIEMAP_EXTENT_LAST, exact[4].fe_flags);
}

test "fiemap: the non-striped shapes — inline [0, len) DATA_INLINE|NOT_ALIGNED|UNKNOWN at 0; a staged resident [0, ceil_4k(size)) DELALLOC|UNKNOWN at 0; a packed tenant [0, slot) at offset + rel_off, aligned, never DATA_TAIL (no 4 KiB block holds another file's byte); a transformed block ENCODED and never merged; a DAMAGED binding UNKNOWN at 0" {
    var buf: [4]Extent = undefined;
    var got = run(Range.of(0, std.math.maxInt(u64)), &.{inlineRun(1000).?}, &buf, false);
    try testing.expectEqual(@as(u32, 1), got.n);
    try testing.expectEqual(@as(u64, 1000), buf[0].fe_length);
    try testing.expectEqual(@as(u64, 0), buf[0].fe_physical);
    try testing.expectEqual(FIEMAP_EXTENT_DATA_INLINE | FIEMAP_EXTENT_NOT_ALIGNED | FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_LAST, buf[0].fe_flags);
    try testing.expectEqual(@as(?Run, null), inlineRun(0));

    got = run(Range.of(0, std.math.maxInt(u64)), &.{delallocRun(10_000).?}, &buf, false);
    try testing.expectEqual(@as(u64, 12_288), buf[0].fe_length);
    try testing.expectEqual(FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_LAST, buf[0].fe_flags);
    try testing.expectEqual(@as(?Run, null), delallocRun(0));

    const tenant = layout.Mapping.tenant(0xA, 8 * MiB, 7, 12 * 4096, 20_000);
    got = run(Range.of(0, std.math.maxInt(u64)), &.{mappingRun(0, tenant, block4m, 0, false)}, &buf, false);
    try testing.expectEqual(@as(u64, 20_480), buf[0].fe_length);
    try testing.expectEqual(physOf(0, 8 * MiB + 12 * 4096, 0).?, buf[0].fe_physical);
    try testing.expectEqual(FIEMAP_EXTENT_LAST, buf[0].fe_flags);
    try testing.expect(buf[0].fe_physical % GRAIN == 0 and buf[0].fe_length % GRAIN == 0);

    const t0 = layout.Mapping.wholeBlock(0xA, 0, 1);
    const t1 = layout.Mapping.wholeBlock(0xA, block4m + 4096, 2);
    got = run(Range.of(0, std.math.maxInt(u64)), &.{ mappingRun(0, t0, block4m, 0, true), mappingRun(block4m, t1, block4m, 0, true) }, &buf, false);
    try testing.expectEqual(@as(u32, 2), got.n);
    try testing.expectEqual(FIEMAP_EXTENT_ENCODED, buf[0].fe_flags);

    var dmg = layout.Mapping.wholeBlock(0xA, 0, 1);
    dmg.damaged = true;
    got = run(Range.of(0, std.math.maxInt(u64)), &.{mappingRun(0, dmg, block4m, 0, false)}, &buf, false);
    try testing.expectEqual(@as(u64, 0), buf[0].fe_physical);
    try testing.expectEqual(FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_LAST, buf[0].fe_flags);
}

test "fiemap: prep restates ioctl_fiemap + fiemap_prep for a FUSE superblock — fm_extent_count past FIEMAP_MAX_EXTENTS and a zero length are EINVAL, a start at or past s_maxbytes EFBIG, a length past it clipped, XATTR/CACHE/unknown bits EBADR with fm_flags = exactly those bits (SYNC kept), SYNC alone passes" {
    const base: Fiemap = .{ .fm_start = 0, .fm_length = 1 << 20, .fm_flags = 0, .fm_mapped_extents = 0, .fm_extent_count = 32, .fm_reserved = 0 };
    try testing.expectEqual(@as(u32, 76695843), MAX_EXTENTS);
    var f = base;
    f.fm_extent_count = MAX_EXTENTS + 1;
    try testing.expectEqual(linux.E.INVAL, prep(f).refused.errno);
    f = base;
    f.fm_extent_count = MAX_EXTENTS;
    try testing.expectEqual(MAX_EXTENTS, prep(f).ok.extent_count);
    f = base;
    f.fm_length = 0;
    try testing.expectEqual(linux.E.INVAL, prep(f).refused.errno);
    f = base;
    f.fm_start = MAX_BYTES;
    try testing.expectEqual(linux.E.FBIG, prep(f).refused.errno);
    f = base;
    f.fm_start = 4096;
    f.fm_length = std.math.maxInt(u64);
    try testing.expectEqual(MAX_BYTES - 4096, prep(f).ok.length);
    f.fm_length = MAX_BYTES - 4095;
    try testing.expectEqual(MAX_BYTES - 4096, prep(f).ok.length);
    f.fm_length = MAX_BYTES - 4096;
    try testing.expectEqual(MAX_BYTES - 4096, prep(f).ok.length);
    f = base;
    f.fm_flags = FIEMAP_FLAG_SYNC | FIEMAP_FLAG_XATTR;
    const x = prep(f).refused;
    try testing.expectEqual(linux.E.BADR, x.errno);
    try testing.expectEqual(FIEMAP_FLAG_XATTR, x.flags);
    f.fm_flags = FIEMAP_FLAG_CACHE | 0x8000_0000;
    try testing.expectEqual(FIEMAP_FLAG_CACHE | 0x8000_0000, prep(f).refused.flags);
    f.fm_flags = FIEMAP_FLAG_SYNC;
    const ok = prep(f).ok;
    try testing.expect(ok.sync());
    try testing.expectEqual(@as(u64, 1 << 20), ok.length);
}

test "fiemap: the reply is the header (start, length, flags echoed; fm_mapped_extents) followed by exactly the written extents" {
    const f: Fiemap = .{ .fm_start = 0, .fm_length = 1 << 20, .fm_flags = 0, .fm_mapped_extents = 0, .fm_extent_count = 4, .fm_reserved = 0 };
    const r = try decodeRequest(&reqBytes(f), 32 + 4 * 56);
    var window: [32 + 4 * 56]u8 align(8) = undefined;
    const slots = extentsOf(&window, batchCap(r, window.len));
    var e = Emitter.init(Range.of(r.start, r.length), slots, false);
    _ = e.push(inlineRun(100).?);
    const n = e.finish();
    const len = encodeReply(r, n, n, &window);
    try testing.expectEqual(@as(usize, 32 + 56), len);
    const h = std.mem.bytesToValue(Fiemap, window[0..32]);
    try testing.expectEqual(@as(u32, 1), h.fm_mapped_extents);
    try testing.expectEqual(@as(u32, 4), h.fm_extent_count);
    const x = std.mem.bytesToValue(Extent, window[32..][0..56]);
    try testing.expectEqual(@as(u64, 100), x.fe_length);
}
