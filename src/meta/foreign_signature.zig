//! The FOREIGN on-disk signature probe `squeezefs format` runs before it
//! writes a byte (charter §1.6 "a loud refusal, never a silent fallback";
//! §8 `format`). PORTABLE: a pure table lookup over the device's first
//! window; the reader is the caller's (`cli/status.zig` for the metadata
//! volume, `cli/format.zig` for the data devices).
//!
//! WHY a probe beside the superblock classifier: `superblock.classifySector0`
//! is the MOUNT's law and reads our own 8 magic bytes at offset 0 — "all
//! zero" is Blank. ext2/3/4 (1024 + 56), btrfs (65536 + 64), f2fs (1024),
//! jfs (32768), reiserfs (65536 + 52), minix (1024 + 16), swap (page − 10)
//! and every other filesystem whose superblock lives past byte 8 read as
//! Blank there, and a bare `format` overwrote them (fstests generic/740:
//! "Failed - overwrote fs type ext2/bfs/cramfs"). `mkfs.ext4` refuses the
//! same way ("contains a ext4 file system") unless forced; so do we.
//!
//! The table is libblkid's well-known set, offsets as the formats define
//! them; a hit NAMES the filesystem and the offset so the operator knows
//! what `--force` destroys. Signatures past the window (ZFS, mdraid's tail
//! superblock, bcachefs' second copy) are not probed — the window is what a
//! format reads anyway; `--force` stays the operator's attestation.

const std = @import("std");

/// The bytes a format's preflight reads from offset 0: covers the farthest
/// table entry (btrfs at 65,600 + 8, reiserfs at 65,588 + 9).
pub const window_len: usize = 128 * 1024;

pub const Hit = struct {
    name: []const u8,
    offset: u64,

    pub fn format(self: Hit, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("a {s} signature at offset {d}", .{ self.name, self.offset });
    }
};

const Sig = struct {
    name: []const u8,
    offset: u64,
    bytes: []const u8,
    /// A second window that must ALSO match (the FAT family's boot-sector
    /// trailer: "FAT1" alone is four printable bytes).
    also_offset: u64 = 0,
    also: []const u8 = "",
};

/// Order: our own data-volume magic first (a data volume offered as a
/// metadata volume), then the filesystems, then the block-layer labels.
const table = [_]Sig{
    .{ .name = "squeezefs data volume", .offset = 0, .bytes = "SQZDATLV" },
    .{ .name = "xfs", .offset = 0, .bytes = "XFSB" },
    .{ .name = "ext2/3/4", .offset = 1024 + 56, .bytes = &.{ 0x53, 0xEF } },
    .{ .name = "btrfs", .offset = 65536 + 64, .bytes = "_BHRfS_M" },
    .{ .name = "f2fs", .offset = 1024, .bytes = &.{ 0x10, 0x20, 0xF5, 0xF2 } },
    .{ .name = "erofs", .offset = 1024, .bytes = &.{ 0xE2, 0xE1, 0xF5, 0xE0 } },
    .{ .name = "nilfs2", .offset = 1024 + 6, .bytes = &.{ 0x34, 0x34 } },
    .{ .name = "hfs+", .offset = 1024, .bytes = "H+" },
    .{ .name = "hfsx", .offset = 1024, .bytes = "HX" },
    .{ .name = "hfs", .offset = 1024, .bytes = "BD" },
    .{ .name = "minix", .offset = 1024 + 16, .bytes = &.{ 0x7F, 0x13 } },
    .{ .name = "minix", .offset = 1024 + 16, .bytes = &.{ 0x8F, 0x13 } },
    .{ .name = "minix", .offset = 1024 + 16, .bytes = &.{ 0x68, 0x24 } },
    .{ .name = "minix", .offset = 1024 + 16, .bytes = &.{ 0x78, 0x24 } },
    .{ .name = "minix", .offset = 1024 + 24, .bytes = &.{ 0x5A, 0x4D } },
    .{ .name = "ntfs", .offset = 3, .bytes = "NTFS    " },
    .{ .name = "exfat", .offset = 3, .bytes = "EXFAT   " },
    .{ .name = "fat", .offset = 54, .bytes = "FAT1", .also_offset = 510, .also = &.{ 0x55, 0xAA } },
    .{ .name = "fat32", .offset = 82, .bytes = "FAT32", .also_offset = 510, .also = &.{ 0x55, 0xAA } },
    .{ .name = "bfs", .offset = 0, .bytes = &.{ 0xCE, 0xFA, 0xAD, 0x1B } },
    .{ .name = "cramfs", .offset = 0, .bytes = &.{ 0x45, 0x3D, 0xCD, 0x28 } },
    .{ .name = "cramfs", .offset = 0, .bytes = &.{ 0x28, 0xCD, 0x3D, 0x45 } },
    .{ .name = "squashfs", .offset = 0, .bytes = "hsqs" },
    .{ .name = "romfs", .offset = 0, .bytes = "-rom1fs-" },
    .{ .name = "ubifs", .offset = 0, .bytes = &.{ 0x31, 0x18, 0x10, 0x06 } },
    .{ .name = "jfs", .offset = 32768, .bytes = "JFS1" },
    .{ .name = "iso9660", .offset = 32768 + 1, .bytes = "CD001" },
    .{ .name = "reiserfs", .offset = 65536 + 52, .bytes = "ReIsErFs" },
    .{ .name = "reiserfs", .offset = 65536 + 52, .bytes = "ReIsEr2Fs" },
    .{ .name = "reiserfs", .offset = 65536 + 52, .bytes = "ReIsEr3Fs" },
    .{ .name = "reiserfs", .offset = 8192 + 52, .bytes = "ReIsErFs" },
    .{ .name = "gfs2", .offset = 65536, .bytes = &.{ 0x01, 0x16, 0x19, 0x70 } },
    .{ .name = "ocfs2", .offset = 2048, .bytes = "OCFSV2" },
    .{ .name = "ocfs2", .offset = 4096, .bytes = "OCFSV2" },
    .{ .name = "ocfs2", .offset = 8192, .bytes = "OCFSV2" },
    .{ .name = "swap", .offset = 4096 - 10, .bytes = "SWAPSPACE2" },
    .{ .name = "swap", .offset = 4096 - 10, .bytes = "SWAP-SPACE" },
    .{ .name = "swap", .offset = 16384 - 10, .bytes = "SWAPSPACE2" },
    .{ .name = "swap", .offset = 65536 - 10, .bytes = "SWAPSPACE2" },
    .{ .name = "LUKS", .offset = 0, .bytes = "LUKS\xba\xbe" },
    .{ .name = "LVM2 physical volume", .offset = 512, .bytes = "LABELONE" },
    .{ .name = "GPT partition table", .offset = 512, .bytes = "EFI PART" },
};

/// The first signature `image` (the device's bytes from offset 0, at most
/// `window_len`, fewer on a shorter device) carries; null = none known. A
/// signature past the image's end is simply not there.
pub fn probe(image: []const u8) ?Hit {
    for (table) |s| {
        if (s.offset + s.bytes.len > image.len) continue;
        if (!std.mem.eql(u8, image[@intCast(s.offset)..][0..s.bytes.len], s.bytes)) continue;
        if (s.also.len != 0) {
            if (s.also_offset + s.also.len > image.len) continue;
            if (!std.mem.eql(u8, image[@intCast(s.also_offset)..][0..s.also.len], s.also)) continue;
        }
        return .{ .name = s.name, .offset = s.offset };
    }
    return null;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "every table signature is found at its offset and named; a zero window, our own meta magic and an image too short for the offset are not foreign (fstests generic/740: a bare format must refuse ext2/bfs/cramfs… and name it)" {
    const image = try testing.allocator.alloc(u8, window_len);
    defer testing.allocator.free(image);
    @memset(image, 0);
    try testing.expectEqual(@as(?Hit, null), probe(image));
    @memcpy(image[0..8], "SQZMETLV");
    try testing.expectEqual(@as(?Hit, null), probe(image));
    @memset(image, 0);
    for (table) |s| {
        @memset(image, 0);
        @memcpy(image[@intCast(s.offset)..][0..s.bytes.len], s.bytes);
        if (s.also.len != 0) @memcpy(image[@intCast(s.also_offset)..][0..s.also.len], s.also);
        const hit = probe(image) orelse return error.TestExpectedHit;
        try testing.expectEqualStrings(s.name, hit.name);
        try testing.expectEqual(s.offset, hit.offset);
        // A device that ends before the signature carries none.
        try testing.expectEqual(@as(?Hit, null), probe(image[0..@intCast(s.offset)]));
    }
    // The FAT trailer is required: "FAT1" alone is four printable bytes.
    @memset(image, 0);
    @memcpy(image[54..58], "FAT1");
    try testing.expectEqual(@as(?Hit, null), probe(image));
    // The report line names both.
    @memset(image, 0);
    image[1080] = 0x53;
    image[1081] = 0xEF;
    var buf: [128]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try w.print("{f}", .{probe(image).?});
    try testing.expectEqualStrings("a ext2/3/4 signature at offset 1080", w.buffered());
}
