//! xxh3-64 — the ONE checksum of the on-disk and wire formats (charter §5.1
//! "Checksums everywhere").
//!
//! Every on-disk unit (superblock, ledger slot, journal page header, journal
//! entry, node header, bset, bitmap page, appender page) carries an xxh3-64
//! computed over its bytes with the checksum field itself ZEROED. Verified once
//! at load/replay, never per read (RAM-authoritative).
//!
//! The seeded form is what the dentry/xattr hashes use (`hash54`, `hash56`):
//! seeded with the volume's random `hash_seed` so names cannot be chosen to
//! collide.

const std = @import("std");

pub const Seed = u64;

/// Unseeded xxh3-64 over `bytes`.
pub inline fn xxh3(bytes: []const u8) u64 {
    return std.hash.XxHash3.hash(0, bytes);
}

/// Seeded xxh3-64 over `bytes`.
pub inline fn xxh3Seeded(seed: Seed, bytes: []const u8) u64 {
    return std.hash.XxHash3.hash(seed, bytes);
}

/// Checksum of a unit whose checksum field lives at `field_off` (8 bytes,
/// little-endian) within `bytes`: hashes `bytes` with that field read as zero,
/// without modifying the caller's buffer. This is the canonical "with the field
/// zeroed" form the charter names for the superblock and every other unit.
pub fn xxh3FieldZeroed(bytes: []const u8, field_off: usize) u64 {
    std.debug.assert(field_off + 8 <= bytes.len);
    var h = std.hash.XxHash3.init(0);
    h.update(bytes[0..field_off]);
    const zeros: [8]u8 = @splat(0);
    h.update(&zeros);
    h.update(bytes[field_off + 8 ..]);
    return h.final();
}

/// Stamp the checksum field of a unit in place: compute over the field-zeroed
/// image and write the result little-endian at `field_off`.
pub fn stamp(bytes: []u8, field_off: usize) void {
    const sum = xxh3FieldZeroed(bytes, field_off);
    std.mem.writeInt(u64, bytes[field_off..][0..8], sum, .little);
}

/// Verify a stamped unit. Total: never panics on arbitrary bytes once the
/// caller has established `field_off + 8 <= bytes.len`.
pub fn verify(bytes: []const u8, field_off: usize) bool {
    const stored = std.mem.readInt(u64, bytes[field_off..][0..8], .little);
    return stored == xxh3FieldZeroed(bytes, field_off);
}

test "field-zeroed hash equals hashing a copy with the field zeroed" {
    var buf: [64]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    var copy = buf;
    @memset(copy[16..24], 0);
    try std.testing.expectEqual(xxh3(&copy), xxh3FieldZeroed(&buf, 16));
}

test "stamp then verify round-trips; any flipped bit fails" {
    var buf: [128]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i * 31);
    stamp(&buf, 40);
    try std.testing.expect(verify(&buf, 40));
    // Flip a payload bit outside the field.
    buf[3] ^= 0x10;
    try std.testing.expect(!verify(&buf, 40));
    buf[3] ^= 0x10;
    try std.testing.expect(verify(&buf, 40));
    // Flip a bit inside the field.
    buf[41] ^= 0x01;
    try std.testing.expect(!verify(&buf, 40));
}

test "seeded hashes differ by seed (the hash_seed collision defence)" {
    const name = "the-same-name";
    try std.testing.expect(xxh3Seeded(1, name) != xxh3Seeded(2, name));
    try std.testing.expectEqual(xxh3Seeded(0, name), xxh3(name));
}
