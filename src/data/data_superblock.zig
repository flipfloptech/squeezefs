//! The data-volume superblock (charter §5.4 "order-independent mount
//! discovery … URI-disagreement refusals", §8 "`device_uuid` recorded
//! once"; `docs/porting/data_transform_config.md` §5.6 e): every data
//! volume carries its identity at sector 0 AND in a self-located tail copy
//! — the reference's data volumes carried NOTHING (format zeroed 32 MiB,
//! the mount opened them by path), so a renumbered `/dev/nvmeXnY` bound a
//! `vol_tag` to the wrong device silently (decision 0002 row 125).
//!
//! ONE 128 B `extern struct`, little-endian, checksum LAST over the whole
//! 4 KiB sector with the field zeroed (`meta/superblock.zig`'s discipline):
//! `{magic "SQZDATLV", version, block, chunk, compression, encryption,
//! volume_len (the tail copy's self-location), vol_tag, set_uuid,
//! device_uuid, features_incompat (0 today; unknown ⇒ refuse loud),
//! sb_generation, reserved, xxh3}`.
//!
//! `format` per data volume: zero chunk 0 and the LAST chunk (both are
//! RESERVED from the allocator and both are where partition tables and
//! filesystem signatures live — the reference's 32 MiB wipe had no stated
//! reason, row 126) → the tail copy, barrier, sector 0, barrier. Sector 0
//! is zeroed first and written LAST, so no crash prefix is a data volume.
//! The loader's choice is the meta superblock's (DUR-5): a sector 0 that
//! classifies — Blank included — is authoritative; the tail serves only
//! when sector 0 ERRORS, and only if its own `volume_len` names the slot it
//! was read from. The generation continues across reformats so a repair
//! never picks a stale copy over the fresh primary.
//!
//! Portable codec + plans; the device binding (`nvme_dev.Device`) is Linux.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const checksum = @import("../core/checksum.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const transform = @import("transform.zig");
const set_config = @import("set_config.zig");
const nvme_dev = if (builtin.os.tag == .linux) @import("../io/nvme_dev.zig") else struct {};

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;

/// One O_DIRECT sector: the identity's durable unit.
pub const sector_len: usize = 4096;
pub const magic_value = "SQZDATLV";
pub const version_v1: u16 = 1;
/// No capability bit is defined today; an unknown one refuses loud.
pub const features_incompat_known: u64 = 0;

pub const DataSuperblockWire = extern struct {
    magic: [8]u8,
    version_le: [2]u8,
    /// Must be 0.
    reserved0_le: [2]u8,
    block_size_le: [4]u8,
    /// `transform.chunkSizeFor`: block + headroom | block.
    chunk_size_le: [4]u8,
    compression: u8,
    encryption: u8,
    reserved1: [2]u8,
    /// Device length at format: the backup copy's self-location.
    volume_len_le: [8]u8,
    /// The record's id.
    vol_tag_le: [8]u8,
    /// The META set's identity (the stamp's `set_uuid`).
    set_uuid: [16]u8,
    /// This device's own identity, minted once at format.
    device_uuid: [16]u8,
    features_incompat_le: [8]u8,
    /// DUR-5: continues across reformats.
    sb_generation_le: [8]u8,
    /// Must be all 0.
    reserved: [32]u8,
    /// xxh3 over the 4096 B sector with this field zeroed.
    checksum_le: [8]u8,
};

pub const fixed_len: usize = @sizeOf(DataSuperblockWire);
pub const off_version: usize = @offsetOf(DataSuperblockWire, "version_le");
pub const off_sb_generation: usize = @offsetOf(DataSuperblockWire, "sb_generation_le");
pub const off_checksum: usize = @offsetOf(DataSuperblockWire, "checksum_le");

comptime {
    assert(fixed_len == 128);
    assert(off_checksum == 120);
    assert(@offsetOf(DataSuperblockWire, "volume_len_le") == 24);
    assert(@offsetOf(DataSuperblockWire, "set_uuid") == 40);
    assert(@offsetOf(DataSuperblockWire, "device_uuid") == 56);
}

pub const DataSuperblock = struct {
    block_size: u32,
    chunk_size: u32,
    compression: transform.Compression,
    encryption: transform.Encryption,
    volume_len: u64,
    vol_tag: u64,
    set_uuid: [16]u8,
    device_uuid: [16]u8,
    features_incompat: u64,

    pub fn transformCfg(self: DataSuperblock) transform.VolumeTransform {
        return .{ .compression = self.compression, .encryption = self.encryption, .block_size = self.block_size };
    }

    /// What discovery compares against the record.
    pub fn identity(self: DataSuperblock) set_config.DeviceIdentity {
        return .{ .vol_tag = self.vol_tag, .device_uuid = self.device_uuid, .set_uuid = self.set_uuid, .block_size = self.block_size, .chunk_size = self.chunk_size, .compression = self.compression, .encryption = self.encryption };
    }

    /// Whole chunks on the device; the first and last are reserved.
    pub fn chunkCount(self: DataSuperblock) u64 {
        return self.volume_len / self.chunk_size;
    }

    pub fn capacityBlocks(self: DataSuperblock) u64 {
        return transform.capacityBlocks(self.volume_len, self.chunk_size);
    }

    pub fn backupOffset(self: DataSuperblock) u64 {
        return backupOffsetFor(self.volume_len).?;
    }

    /// The geometry every encoder and decoder enforces: a valid block, the
    /// chunk the transform derives, unknown bits refused, and ≥ 3 chunks
    /// (two reserved + one usable block).
    pub fn validate(self: DataSuperblock, diag: ?*Refusal) DecodeError!void {
        if (!transform.blockSizeValid(self.block_size)) return refuse(diag, .{ .invalid_block_size = self.block_size });
        if (self.chunk_size != transform.chunkSizeFor(self.transformCfg())) return refuse(diag, .{ .chunk_mismatch = .{ .chunk = self.chunk_size, .want = transform.chunkSizeFor(self.transformCfg()) } });
        const unknown = self.features_incompat & ~features_incompat_known;
        if (unknown != 0) return refuse(diag, .{ .unknown_incompat_bits = unknown });
        if (self.chunkCount() < 3) return refuse(diag, .{ .volume_too_small = .{ .volume_len = self.volume_len, .chunk = self.chunk_size } });
    }

    pub fn encodeSector(self: DataSuperblock, generation: u64, out: *[sector_len]u8, diag: ?*Refusal) DecodeError!void {
        try self.validate(diag);
        @memset(out, 0);
        var w: DataSuperblockWire = undefined;
        w.magic = magic_value.*;
        std.mem.writeInt(u16, &w.version_le, version_v1, .little);
        std.mem.writeInt(u16, &w.reserved0_le, 0, .little);
        std.mem.writeInt(u32, &w.block_size_le, self.block_size, .little);
        std.mem.writeInt(u32, &w.chunk_size_le, self.chunk_size, .little);
        w.compression = @backingInt(self.compression);
        w.encryption = @backingInt(self.encryption);
        w.reserved1 = @splat(0);
        std.mem.writeInt(u64, &w.volume_len_le, self.volume_len, .little);
        std.mem.writeInt(u64, &w.vol_tag_le, self.vol_tag, .little);
        w.set_uuid = self.set_uuid;
        w.device_uuid = self.device_uuid;
        std.mem.writeInt(u64, &w.features_incompat_le, self.features_incompat, .little);
        std.mem.writeInt(u64, &w.sb_generation_le, generation, .little);
        w.reserved = @splat(0);
        w.checksum_le = @splat(0);
        out[0..fixed_len].* = std.mem.toBytes(w);
        checksum.stamp(out, off_checksum);
    }

    /// Length → magic → version → checksum → fields → reserved → geometry.
    pub fn decodeSector(buf: []const u8, diag: ?*Refusal) DecodeError!DataSuperblock {
        if (buf.len != sector_len) return refuse(diag, .{ .bad_length = buf.len });
        if (!std.mem.eql(u8, buf[0..8], magic_value)) return refuse(diag, .bad_magic);
        const version = std.mem.readInt(u16, buf[off_version..][0..2], .little);
        if (version != version_v1) return refuse(diag, .{ .wrong_version = version });
        if (!checksum.verify(buf, off_checksum)) return refuse(diag, .checksum_mismatch);
        const w = std.mem.bytesToValue(DataSuperblockWire, buf[0..fixed_len]);
        if (std.mem.readInt(u16, &w.reserved0_le, .little) != 0 or !std.mem.allEqual(u8, &w.reserved1, 0) or !std.mem.allEqual(u8, &w.reserved, 0) or !std.mem.allEqual(u8, buf[fixed_len..], 0)) {
            return refuse(diag, .reserved_not_zero);
        }
        const sb: DataSuperblock = .{
            .block_size = std.mem.readInt(u32, &w.block_size_le, .little),
            .chunk_size = std.mem.readInt(u32, &w.chunk_size_le, .little),
            .compression = transform.Compression.fromWire(w.compression) orelse return refuse(diag, .bad_mode),
            .encryption = transform.Encryption.fromWire(w.encryption) orelse return refuse(diag, .bad_mode),
            .volume_len = std.mem.readInt(u64, &w.volume_len_le, .little),
            .vol_tag = std.mem.readInt(u64, &w.vol_tag_le, .little),
            .set_uuid = w.set_uuid,
            .device_uuid = w.device_uuid,
            .features_incompat = std.mem.readInt(u64, &w.features_incompat_le, .little),
        };
        try sb.validate(diag);
        return sb;
    }
};

pub const DecodeError = error{ BadLength, BadMagic, WrongVersion, ChecksumMismatch, ReservedNotZero, BadMode, InvalidBlockSize, ChunkMismatch, UnknownIncompatBits, VolumeTooSmall, InvalidMagic };

pub const Refusal = union(enum) {
    bad_length: usize,
    bad_magic,
    wrong_version: u16,
    checksum_mismatch,
    reserved_not_zero,
    bad_mode,
    invalid_block_size: u32,
    chunk_mismatch: struct { chunk: u32, want: u32 },
    unknown_incompat_bits: u64,
    volume_too_small: struct { volume_len: u64, chunk: u32 },
    invalid_magic: [8]u8,

    pub fn err(self: Refusal) DecodeError {
        return switch (self) {
            .bad_length => error.BadLength,
            .bad_magic => error.BadMagic,
            .wrong_version => error.WrongVersion,
            .checksum_mismatch => error.ChecksumMismatch,
            .reserved_not_zero => error.ReservedNotZero,
            .bad_mode => error.BadMode,
            .invalid_block_size => error.InvalidBlockSize,
            .chunk_mismatch => error.ChunkMismatch,
            .unknown_incompat_bits => error.UnknownIncompatBits,
            .volume_too_small => error.VolumeTooSmall,
            .invalid_magic => error.InvalidMagic,
        };
    }

    pub fn write(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .bad_length => |n| try w.print("data superblock sector must be {d} bytes, got {d}", .{ sector_len, n }),
            .bad_magic => try w.writeAll("bad data superblock magic (corrupted or foreign volume)"),
            .wrong_version => |v| try w.print("data superblock version {d} is not {d} — upgrade squeezefs", .{ v, version_v1 }),
            .checksum_mismatch => try w.writeAll("data superblock checksum mismatch — corrupted superblock"),
            .reserved_not_zero => try w.writeAll("data superblock reserved bytes are not zero (a writer this binary does not know, or corruption under a valid checksum)"),
            .bad_mode => try w.writeAll("data superblock names a compression or encryption mode this binary does not know — upgrade squeezefs"),
            .invalid_block_size => |b| try w.print("data superblock block size {d} is not a power of two in [{d}, {d}]", .{ b, transform.MIN_BLOCK_SIZE, transform.MAX_BLOCK_SIZE }),
            .chunk_mismatch => |c| try w.print("data superblock chunk {d} is not the {d} its block and transform derive", .{ c.chunk, c.want }),
            .unknown_incompat_bits => |u| try w.print("unknown incompatible feature bits on the data superblock (0x{x}) — upgrade squeezefs to mount this volume", .{u}),
            .volume_too_small => |v| try w.print("data volume of {d} bytes holds fewer than 3 chunks of {d} bytes (two are reserved; at least one usable block is required) — grow the volume or choose a smaller --block-size", .{ v.volume_len, v.chunk }),
            .invalid_magic => |m| try w.print("invalid data superblock magic {x} (expected \"{s}\"): not a squeezefs data volume — a filesystem, a partition table or another product's signature", .{ &m, magic_value }),
        }
    }
};

fn refuse(diag: ?*Refusal, r: Refusal) DecodeError {
    if (diag) |d| d.* = r;
    return r.err();
}

// ---------------------------------------------------------------------------
// Classification and the DUR-5 copy.
// ---------------------------------------------------------------------------

pub const VolumeFormat = union(enum) {
    /// All-zero magic: never formatted.
    blank,
    v1: DataSuperblock,
};

pub fn classifySector0(sector: []const u8, diag: ?*Refusal) DecodeError!VolumeFormat {
    if (sector.len != sector_len) return refuse(diag, .{ .bad_length = sector.len });
    const magic = sector[0..8];
    if (std.mem.allEqual(u8, magic, 0)) return .blank;
    if (!std.mem.eql(u8, magic, magic_value)) return refuse(diag, .{ .invalid_magic = magic.* });
    return .{ .v1 = try DataSuperblock.decodeSector(sector, diag) };
}

/// The generation stamped in an image (0 on anything shorter).
pub fn sectorGeneration(sector: []const u8) u64 {
    if (sector.len < off_sb_generation + 8) return 0;
    return std.mem.readInt(u64, sector[off_sb_generation..][0..8], .little);
}

/// The last aligned sector: distance from sector 0 protects the copy
/// against partition-table and `mkfs` collateral; null below two sectors.
pub fn backupOffsetFor(volume_len: u64) ?u64 {
    const aligned = (volume_len / sector_len) * sector_len;
    if (aligned < 2 * sector_len) return null;
    return aligned - sector_len;
}

/// `max(gen of VALID primary, gen of VALID tail) + 1`.
pub fn nextGeneration(primary: ?[]const u8, backup: ?[]const u8) u64 {
    var newest: u64 = 0;
    for ([_]?[]const u8{ primary, backup }) |img| {
        const sector = img orelse continue;
        _ = classifySector0(sector, null) catch continue;
        newest = @max(newest, sectorGeneration(sector));
    }
    return newest + 1;
}

pub const Slot = enum { primary, backup };

pub const Classified = struct { format: VolumeFormat, slot: Slot };

/// The loader's choice (the meta superblock's law): a primary that
/// classifies — Blank included — is authoritative; the tail is read only
/// when the primary ERRORS and accepted iff it classifies v1 and its own
/// `volume_len` names the slot it came from; otherwise the primary's error
/// stands.
pub fn classifyImages(primary: []const u8, backup: ?[]const u8, device_len: u64, diag: ?*Refusal) DecodeError!Classified {
    var primary_refusal: Refusal = undefined;
    const primary_err = blk: {
        const fmt = classifySector0(primary, &primary_refusal) catch |e| break :blk e;
        return .{ .format = fmt, .slot = .primary };
    };
    if (backup) |tail| {
        if (backupOffsetFor(device_len)) |off| {
            if (classifySector0(tail, null)) |fmt| {
                if (fmt == .v1 and fmt.v1.backupOffset() == off) return .{ .format = fmt, .slot = .backup };
            } else |_| {}
        }
    }
    if (diag) |d| d.* = primary_refusal;
    return primary_err;
}

/// What discovery feeds `set_config.discover` for one uri.
pub fn observation(classified: anyerror!Classified) set_config.Observation {
    const c = classified catch return .refused;
    return switch (c.format) {
        .blank => .blank,
        .v1 => |sb| .{ .identity = sb.identity() },
    };
}

// ---------------------------------------------------------------------------
// The format plan: zero chunk 0 and the last chunk → tail, barrier → sector
// 0, barrier.
// ---------------------------------------------------------------------------

/// The wipe grain: one DMA-aligned zero buffer every zero step views.
pub const zero_chunk: usize = 1 << 20;

pub const Step = struct {
    offset: u64,
    bytes: []const u8,
    barrier_after: bool,
};

pub const FormatPlan = struct {
    arena: std.heap.ArenaAllocator,
    steps: []Step,
    sector: *[sector_len]u8,
    generation: u64,

    pub fn deinit(self: *FormatPlan) void {
        self.arena.deinit();
        self.* = undefined;
    }

    pub fn apply(self: *const FormatPlan, image: []u8) void {
        for (self.steps) |s| @memcpy(image[@intCast(s.offset)..][0..s.bytes.len], s.bytes);
    }

    pub fn barrierCount(self: *const FormatPlan) usize {
        var n: usize = 0;
        for (self.steps) |s| n += @intFromBool(s.barrier_after);
        return n;
    }
};

pub fn planFormat(gpa: Allocator, sb: DataSuperblock, generation: u64, diag: ?*Refusal) (DecodeError || Allocator.Error)!FormatPlan {
    try sb.validate(diag);
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    const zeros = try a.alignedAlloc(u8, .fromByteUnits(sector_len), zero_chunk);
    @memset(zeros, 0);
    var steps: std.ArrayList(Step) = .empty;
    const chunk: u64 = sb.chunk_size;
    const last_chunk_start = (sb.chunkCount() - 1) * chunk;
    try pushZero(a, &steps, zeros, 0, chunk);
    try pushZero(a, &steps, zeros, last_chunk_start, chunk);
    const sector = try a.alignedAlloc(u8, .fromByteUnits(sector_len), sector_len);
    try sb.encodeSector(generation, sector[0..sector_len], diag);
    try steps.append(a, .{ .offset = sb.backupOffset(), .bytes = sector, .barrier_after = true });
    try steps.append(a, .{ .offset = 0, .bytes = sector, .barrier_after = true });
    return .{ .arena = arena, .steps = try steps.toOwnedSlice(a), .sector = sector[0..sector_len], .generation = generation };
}

fn pushZero(a: Allocator, steps: *std.ArrayList(Step), zeros: []const u8, start: u64, len: u64) Allocator.Error!void {
    var off = start;
    const end = start + len;
    while (off < end) {
        const n: usize = @intCast(@min(end - off, zeros.len));
        try steps.append(a, .{ .offset = off, .bytes = zeros[0..n], .barrier_after = false });
        off += n;
    }
}

// ---------------------------------------------------------------------------
// The device binding (Linux): nvme_dev's workers, O_DIRECT from the first
// byte, the write read back.
// ---------------------------------------------------------------------------

pub const linux_dev = if (builtin.os.tag == .linux) struct {
    pub const DeviceError = error{ DeviceRefused, Io, OutOfMemory, VolumeTooSmall, InvalidBlockSize, ChunkMismatch, UnknownIncompatBits, ReadbackMismatch, Unexpected };

    pub const Opened = struct {
        classified: Classified,
        volume_len: u64,
        /// The next generation to write (`nextGeneration` of what was read).
        next_generation: u64,
    };

    fn pathZ(buf: *[std.os.linux.PATH_MAX]u8, path: []const u8) error{NameTooLong}![:0]const u8 {
        if (path.len >= buf.len) return error.NameTooLong;
        @memcpy(buf[0..path.len], path);
        buf[path.len] = 0;
        return buf[0..path.len :0];
    }

    fn devErr(e: anyerror) DeviceError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.DirectOpenRefused, error.DirectIoUnsupported, error.NotADeviceOrFile, error.CannotStat, error.FileNotFound, error.AccessDenied => error.DeviceRefused,
            else => error.Io,
        };
    }

    /// Read both copies and classify. `Blank` is a legal answer (a device
    /// `add-data` may take); a refusal is logged by name and surfaces as
    /// the decode error.
    pub fn open(gpa: Allocator, path: []const u8) (DeviceError || DecodeError)!Opened {
        var zb: [std.os.linux.PATH_MAX]u8 = undefined;
        const z = pathZ(&zb, path) catch return error.DeviceRefused;
        const volume_len = nvme_dev.capacityBytes(z) catch |e| return devErr(e);
        const dev = nvme_dev.Device.open(gpa, z) catch |e| return devErr(e);
        defer dev.deinit();
        return readAndClassify(dev, path, volume_len);
    }

    fn readAndClassify(dev: *nvme_dev.Device, path: []const u8, volume_len: u64) (DeviceError || DecodeError)!Opened {
        const primary = dev.probeReadBlock(0, sector_len) catch |e| return devErr(e);
        defer primary.release();
        var tail: ?nvme_dev.Buf = null;
        defer if (tail) |t| t.release();
        if (backupOffsetFor(volume_len)) |off| tail = dev.probeReadBlock(off, sector_len) catch |e| return devErr(e);
        const next_generation = nextGeneration(primary.bytes(), if (tail) |t| t.bytes() else null);
        var diag: Refusal = undefined;
        const classified = classifyImages(primary.bytes(), if (tail) |t| t.bytes() else null, volume_len, &diag) catch |e| {
            var buf: [512]u8 = undefined;
            var w: std.Io.Writer = .fixed(&buf);
            diag.write(&w) catch {};
            log.err("data volume {s}: {s}", .{ path, w.buffered() });
            return e;
        };
        return .{ .classified = classified, .volume_len = volume_len, .next_generation = next_generation };
    }

    /// `format`'s per-data-volume act: the plan's steps in order with their
    /// barriers, then both copies read back and compared byte-for-byte (the
    /// reference's random-4 KiB write/readback probe survives as the
    /// superblock write itself read back). `sb.volume_len` must be the
    /// device's length.
    pub fn format(gpa: Allocator, path: []const u8, sb: DataSuperblock, generation: u64) (DeviceError || DecodeError)!void {
        var zb: [std.os.linux.PATH_MAX]u8 = undefined;
        const z = pathZ(&zb, path) catch return error.DeviceRefused;
        const volume_len = nvme_dev.capacityBytes(z) catch |e| return devErr(e);
        if (volume_len != sb.volume_len) return error.VolumeTooSmall;
        var plan = try planFormat(gpa, sb, generation, null);
        defer plan.deinit();
        const dev = nvme_dev.Device.open(gpa, z) catch |e| return devErr(e);
        defer dev.deinit();
        for (plan.steps) |s| {
            dev.writeBlock(s.offset, s.bytes) catch |e| return devErr(e);
            if (s.barrier_after) dev.flush() catch |e| return devErr(e);
        }
        for ([_]u64{ sb.backupOffset(), 0 }) |off| {
            const back = dev.probeReadBlock(off, sector_len) catch |e| return devErr(e);
            defer back.release();
            if (!std.mem.eql(u8, back.bytes(), plan.sector)) return error.ReadbackMismatch;
        }
        log.info("formatted data volume {s}: vol-{x:0>16}, {d} B, block {d} B, chunk {d} B, {d} usable blocks, generation {d}", .{ path, sb.vol_tag, sb.volume_len, sb.block_size, sb.chunk_size, sb.capacityBlocks(), generation });
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn fixture() DataSuperblock {
    return .{
        .block_size = 4 << 20,
        .chunk_size = 4_198_400,
        .compression = .lz4,
        .encryption = .aes256gcm,
        .volume_len = 1 << 30,
        .vol_tag = 0x0123_4567_89ab_cdef,
        .set_uuid = "set-uuid-fixture".*,
        .device_uuid = "device-uuid-fixt".*,
        .features_incompat = 0,
    };
}

test "the wire is 128 B at the spec's offsets; encode∘decode = id; the sector's padding is under the checksum; capacity reserves two chunks; a passthrough volume's chunk is its block" {
    try testing.expectEqual(@as(usize, 128), fixed_len);
    const sb = fixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(7, &img, null);
    try testing.expectEqualStrings("SQZDATLV", img[0..8]);
    try testing.expectEqual(@as(u16, 1), std.mem.readInt(u16, img[8..10], .little));
    try testing.expectEqual(@as(u32, 4 << 20), std.mem.readInt(u32, img[12..16], .little));
    try testing.expectEqual(@as(u32, 4_198_400), std.mem.readInt(u32, img[16..20], .little));
    try testing.expectEqual(@as(u64, 1 << 30), std.mem.readInt(u64, img[24..32], .little));
    try testing.expectEqual(@as(u64, 7), sectorGeneration(&img));
    try testing.expect(std.mem.allEqual(u8, img[128..], 0));
    const back = try DataSuperblock.decodeSector(&img, null);
    try testing.expectEqual(sb, back);
    try testing.expectEqual(@as(u64, 255), sb.chunkCount());
    try testing.expectEqual(@as(u64, 253), sb.capacityBlocks());
    try testing.expectEqual(@as(u64, (1 << 30) - 4096), sb.backupOffset());
    const id = sb.identity();
    try testing.expectEqual(sb.vol_tag, id.vol_tag);
    try testing.expectEqual(sb.device_uuid, id.device_uuid);
    var plain = sb;
    plain.compression = .none;
    plain.encryption = .none;
    plain.chunk_size = 4 << 20;
    try plain.validate(null);
    try testing.expectEqual(@as(u64, 254), plain.capacityBlocks());
}

test "decode refuses by class in order: length, magic, version, checksum, reserved (incl. padding), mode, block size, chunk, unknown bits, too small; classifySector0 tells Blank from a foreign magic" {
    const sb = fixture();
    var img: [sector_len]u8 = undefined;
    try sb.encodeSector(1, &img, null);
    var diag: Refusal = undefined;
    try testing.expectError(error.BadLength, DataSuperblock.decodeSector(img[0..100], &diag));
    var bad = img;
    bad[0] = 'X';
    try testing.expectError(error.BadMagic, DataSuperblock.decodeSector(&bad, &diag));
    try testing.expectError(error.InvalidMagic, classifySector0(&bad, &diag));
    try testing.expect(diag == .invalid_magic);
    bad = img;
    std.mem.writeInt(u16, bad[8..10], 2, .little);
    try testing.expectError(error.WrongVersion, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    bad[40] ^= 1;
    try testing.expectError(error.ChecksumMismatch, DataSuperblock.decodeSector(&bad, &diag));
    const Restamp = struct {
        fn go(b: *[sector_len]u8) void {
            checksum.stamp(b, off_checksum);
        }
    };
    bad = img;
    bad[10] = 1;
    Restamp.go(&bad);
    try testing.expectError(error.ReservedNotZero, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    bad[2000] = 1;
    Restamp.go(&bad);
    try testing.expectError(error.ReservedNotZero, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    bad[20] = 9;
    Restamp.go(&bad);
    try testing.expectError(error.BadMode, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    std.mem.writeInt(u32, bad[12..16], 3 << 20, .little);
    Restamp.go(&bad);
    try testing.expectError(error.InvalidBlockSize, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    std.mem.writeInt(u32, bad[16..20], 4 << 20, .little);
    Restamp.go(&bad);
    try testing.expectError(error.ChunkMismatch, DataSuperblock.decodeSector(&bad, &diag));
    bad = img;
    std.mem.writeInt(u64, bad[72..80], 1 << 5, .little);
    Restamp.go(&bad);
    try testing.expectError(error.UnknownIncompatBits, DataSuperblock.decodeSector(&bad, &diag));
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try diag.write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "0x20") != null);
    bad = img;
    std.mem.writeInt(u64, bad[24..32], 2 * 4_198_400, .little);
    Restamp.go(&bad);
    try testing.expectError(error.VolumeTooSmall, DataSuperblock.decodeSector(&bad, &diag));
    const zero: [sector_len]u8 = @splat(0);
    try testing.expect((try classifySector0(&zero, &diag)) == .blank);
    try testing.expect((try classifySector0(&img, &diag)) == .v1);
    try testing.expectError(error.BadLength, classifySector0(zero[0..4095], &diag));
    // The encoder refuses the same laws.
    var small = sb;
    small.volume_len = 2 * 4_198_400;
    try testing.expectError(error.VolumeTooSmall, small.encodeSector(1, &bad, &diag));
}

test "DUR-5: a classifying primary (Blank included) is authoritative; the tail serves only when sector 0 errors and only from its own slot; the generation continues across reformats" {
    const sb = fixture();
    var primary: [sector_len]u8 = undefined;
    try sb.encodeSector(3, &primary, null);
    var tail: [sector_len]u8 = undefined;
    try sb.encodeSector(9, &tail, null);
    const len = sb.volume_len;
    var diag: Refusal = undefined;
    // Both valid: the primary (older generation!) wins — the tail is never consulted.
    const both = try classifyImages(&primary, &tail, len, &diag);
    try testing.expectEqual(Slot.primary, both.slot);
    // A Blank primary is authoritative: a crash between the tail and sector 0 is not a data volume.
    const zero: [sector_len]u8 = @splat(0);
    const blank = try classifyImages(&zero, &tail, len, &diag);
    try testing.expect(blank.format == .blank and blank.slot == .primary);
    // A corrupt primary: the tail serves, from its own slot only.
    var corrupt = primary;
    corrupt[50] ^= 1;
    const from_tail = try classifyImages(&corrupt, &tail, len, &diag);
    try testing.expectEqual(Slot.backup, from_tail.slot);
    try testing.expectEqual(sb.vol_tag, from_tail.format.v1.vol_tag);
    try testing.expectError(error.ChecksumMismatch, classifyImages(&corrupt, &tail, len + 4096, &diag));
    try testing.expectError(error.ChecksumMismatch, classifyImages(&corrupt, null, len, &diag));
    try testing.expectError(error.ChecksumMismatch, classifyImages(&corrupt, &zero, len, &diag));
    try testing.expect(diag == .checksum_mismatch);
    // The generation.
    try testing.expectEqual(@as(u64, 10), nextGeneration(&primary, &tail));
    try testing.expectEqual(@as(u64, 4), nextGeneration(&primary, &zero));
    try testing.expectEqual(@as(u64, 1), nextGeneration(&zero, null));
    try testing.expectEqual(@as(u64, 1), nextGeneration(&corrupt, &corrupt));
    try testing.expectEqual(@as(?u64, null), backupOffsetFor(4096));
    try testing.expectEqual(@as(?u64, 4096), backupOffsetFor(8192 + 100));
    // Observations.
    try testing.expect(observation(both) == .identity);
    try testing.expect(observation(blank) == .blank);
    try testing.expect(observation(error.ChecksumMismatch) == .refused);
}

test "the format plan: zero chunk 0 (sector 0 first) and the last chunk, the tail copy with a barrier, sector 0 last with a barrier; applied to an image it classifies from the primary; the prefix before the last step reads Blank" {
    const gpa = testing.allocator;
    var sb = fixture();
    sb.volume_len = 5 * 4_198_400 + 1000; // a slack tail past the last whole chunk
    var plan = try planFormat(gpa, sb, 2, null);
    defer plan.deinit();
    const s = plan.steps;
    // chunk 0: 4,198,400 B = 4 × 1 MiB + 4096 → 5 zero steps; the last chunk likewise; tail; sector 0.
    try testing.expectEqual(@as(usize, 5 + 5 + 2), s.len);
    try testing.expectEqual(@as(u64, 0), s[0].offset);
    try testing.expect(std.mem.allEqual(u8, s[0].bytes, 0));
    try testing.expectEqual(@as(u64, 4 * 4_198_400), s[5].offset);
    try testing.expectEqual(sb.backupOffset(), s[10].offset);
    try testing.expect(s[10].barrier_after);
    try testing.expectEqual(@as(u64, 0), s[11].offset);
    try testing.expect(s[11].barrier_after);
    try testing.expectEqual(@as(usize, 2), plan.barrierCount());
    try testing.expectEqual(@as(u64, 2), sectorGeneration(plan.sector));
    // Every zero step is DMA-aligned (the zero-copy branch of writeBlock).
    for (s[0..10]) |z| try testing.expect(z.offset % 4096 == 0 and z.bytes.len % 4096 == 0 and @intFromPtr(z.bytes.ptr) % 4096 == 0);
    const image = try gpa.alloc(u8, @intCast(sb.volume_len));
    defer gpa.free(image);
    @memset(image, 0xee);
    // The prefix (every step but the last) reads Blank.
    for (s[0 .. s.len - 1]) |st| @memcpy(image[@intCast(st.offset)..][0..st.bytes.len], st.bytes);
    var diag: Refusal = undefined;
    const prefix = try classifyImages(image[0..4096], image[@intCast(sb.backupOffset())..][0..4096], sb.volume_len, &diag);
    try testing.expect(prefix.format == .blank);
    plan.apply(image);
    const done = try classifyImages(image[0..4096], image[@intCast(sb.backupOffset())..][0..4096], sb.volume_len, &diag);
    try testing.expectEqual(Slot.primary, done.slot);
    try testing.expectEqual(sb, done.format.v1);
    // The slack past the last whole chunk and the data chunks are untouched.
    try testing.expect(std.mem.allEqual(u8, image[4_198_400 .. 4 * 4_198_400], 0xee));
    try testing.expectEqual(@as(u64, 3), done.format.v1.capacityBlocks());
}

fn sectorFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [sector_len]u8 = undefined;
    const len = smith.slice(&buf);
    _ = DataSuperblock.decodeSector(buf[0..len], null) catch {};
    _ = classifySector0(buf[0..len], null) catch {};
    _ = classifyImages(buf[0..len], buf[0..len], 1 << 30, null) catch {};
    _ = sectorGeneration(buf[0..len]);
    _ = nextGeneration(buf[0..len], null);
}

test "fuzz: the data superblock decoders are total on arbitrary bytes" {
    try fuzzkit.fuzz({}, sectorFuzz, .{});
    var prng = std.Random.DefaultPrng.init(0xd474);
    const r = prng.random();
    var img: [sector_len]u8 = undefined;
    try fixture().encodeSector(1, &img, null);
    var mut: [sector_len]u8 = undefined;
    for (0..5000) |_| {
        mut = img;
        for (0..r.intRangeAtMost(usize, 1, 3)) |_| mut[r.intRangeLessThan(usize, 0, 128)] ^= @as(u8, 1) << r.int(u3);
        checksum.stamp(&mut, off_checksum);
        if (DataSuperblock.decodeSector(&mut, null)) |sb| {
            var again: [sector_len]u8 = undefined;
            try sb.encodeSector(sectorGeneration(&mut), &again, null);
            try testing.expectEqualSlices(u8, &mut, &again);
        } else |_| {}
    }
}

test "Linux: formatDataVolume on a file writes both copies and reads them back; open classifies v1; a blank file classifies Blank; a too-small file refuses; a reformat continues the generation" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "data_superblock: Linux only");
    const io = testing.io;
    const gpa = testing.allocator;
    var dir = testing.tmpDir(.{});
    defer dir.cleanup();
    var pb: [std.fs.max_path_bytes]u8 = undefined;
    const base = pb[0..try dir.dir.realPath(io, &pb)];
    const path = try std.fmt.allocPrint(gpa, "{s}/data.img", .{base});
    defer gpa.free(path);
    const len: u64 = 8 << 20;
    {
        const f = try dir.dir.createFile(io, "data.img", .{ .truncate = true });
        defer f.close(io);
        try f.setLength(io, len);
    }
    const blank = try linux_dev.open(gpa, path);
    try testing.expect(blank.classified.format == .blank);
    try testing.expectEqual(@as(u64, 1), blank.next_generation);
    var sb = fixture();
    sb.block_size = 1 << 20;
    sb.chunk_size = (1 << 20) + 4096;
    sb.volume_len = len;
    try linux_dev.format(gpa, path, sb, blank.next_generation);
    const opened = try linux_dev.open(gpa, path);
    try testing.expectEqual(Slot.primary, opened.classified.slot);
    try testing.expectEqual(sb, opened.classified.format.v1);
    try testing.expectEqual(@as(u64, 2), opened.next_generation);
    try testing.expectEqual(len, opened.volume_len);
    // A reformat continues the generation.
    try linux_dev.format(gpa, path, sb, opened.next_generation);
    try testing.expectEqual(@as(u64, 3), (try linux_dev.open(gpa, path)).next_generation);
    // Too small: 2 chunks.
    var small = sb;
    small.volume_len = 2 * sb.chunk_size;
    try testing.expectError(error.VolumeTooSmall, linux_dev.format(gpa, path, small, 1));
}
