//! The ONE checksum of the on-disk and wire formats (charter §5.1
//! "Checksums everywhere"; decision 0003 §M): sqzhash128
//! (`core/sqzhash.zig`, the pinned definition), projected to its low 64
//! bits wherever a format field is 8 bytes.
//!
//! Every on-disk unit (superblock, ledger slot, journal page header, journal
//! entry, node header, bset, bitmap page, appender page) carries the low 64
//! bits of sqzhash128 computed under the FORMAT KEY over its bytes with the
//! checksum field itself ZEROED. Verified once at load/replay, never per
//! read (RAM-authoritative).
//!
//! Two keys, never more: the fixed `FORMAT_KEY` for every checksum and
//! identity hash (a checksum detects corruption; it keeps no secret, and the
//! superblock must verify before any per-volume value is known), and the
//! per-volume NAME key (`keyFromSeed(hash_seed)`, expanded ONCE at mount —
//! `meta/key.zig` `NameKey`) that the dentry/xattr hashes `hash54`/`hash56`
//! use so names cannot be chosen off-line to collide.
//!
//! The facade is algorithm-neutral: the names say what a hash is for
//! (`hash64`, `hash64Keyed`, `hash64FieldZeroed`, `stamp`, `verify`,
//! `Hasher`), never which algorithm computes it — the algorithm's name lives
//! in `sqzhash.zig` and in the format version it defines.

const std = @import("std");
const sqzhash = @import("sqzhash.zig").native;

/// The expanded form of a 128-bit secret: made once per secret and reused
/// (per mount for the name key; at comptime for the format key).
pub const Key = sqzhash.Key;

/// The format key's secret: the ASCII of "SQZFS FORMAT KEY" as a big-endian
/// u128 — nothing up the sleeve, and public ON PURPOSE: a checksum detects
/// corruption and keeps no secret, and sector 0 must verify before any
/// per-volume value exists. Its upper 64 bits are non-zero, so no volume's
/// zero-extended `hash_seed` (`keyFromSeed`) can ever expand to it.
pub const FORMAT_KEY: u128 = 0x5351_5A46_5320_464F_524D_4154_204B_4559;

/// The format key, expanded at comptime (40 software AES rounds, once per
/// build; `sqzhash.zig` proves the comptime expansion equals the runtime one).
pub const format_key: Key = .init(FORMAT_KEY);

/// A volume's 64-bit `hash_seed` widened to the hash's 128-bit secret: ZERO-
/// EXTENDED. The rule is the format's (the name hashes on disk depend on it):
/// the seed is the whole secret, the key schedule's two AES rounds per vector
/// spread its 64 bits over every lane, and the upper half being zero keeps
/// every seed disjoint from `FORMAT_KEY`.
pub fn keyFromSeed(hash_seed: u64) Key {
    return .init(@as(u128, hash_seed));
}

/// The 8-byte projection: the LOW 64 bits of sqzhash128 — the half
/// SMHasher3 verified on its own (`sqzhash.zig`'s header).
pub inline fn low64(h: u128) u64 {
    return @truncate(h);
}

/// The format-keyed 64-bit hash of `bytes`.
pub inline fn hash64(bytes: []const u8) u64 {
    return sqzhash.hash64(&format_key, bytes);
}

/// The format-keyed 128-bit hash of `bytes`.
pub inline fn hash128(bytes: []const u8) u128 {
    return sqzhash.hash(&format_key, bytes);
}

/// The 64-bit hash of `bytes` under a caller's key (the per-mount name key).
pub inline fn hash64Keyed(key: *const Key, bytes: []const u8) u64 {
    return sqzhash.hash64(key, bytes);
}

/// Streaming sqzhash128 under the format key (`init`) or a caller's key
/// (`initKeyed`): `update` any split, then `final` (u128) or `finalLow64`
/// (the 8-byte field). Streaming equals one-shot whatever the split.
///
/// The state holds the key by POINTER and the construction's minimum
/// (`sqzhash.zig`'s `Hasher`: ≤ 1056 B against the 4.7 KiB a 16-chunk
/// buffer and a key copy cost) — one lives per staged cell (charter R5).
pub const Hasher = struct {
    inner: sqzhash.Hasher,

    pub fn init() Hasher {
        return .{ .inner = .initWithKey(&format_key) };
    }

    /// `key` must outlive the hasher: the per-mount name key a store owns.
    pub fn initKeyed(key: *const Key) Hasher {
        return .{ .inner = .initWithKey(key) };
    }

    pub fn update(self: *Hasher, bytes: []const u8) void {
        self.inner.update(bytes);
    }

    pub fn final(self: *const Hasher) u128 {
        return self.inner.final();
    }

    pub fn finalLow64(self: *const Hasher) u64 {
        return self.inner.finalLow64();
    }
};

/// Checksum of a unit whose checksum field lives at `field_off` (8 bytes,
/// little-endian) within `bytes`: hashes `bytes` with that field read as zero,
/// without modifying the caller's buffer. This is the canonical "with the field
/// zeroed" form the charter names for the superblock and every other unit.
pub fn hash64FieldZeroed(bytes: []const u8, field_off: usize) u64 {
    std.debug.assert(field_off + 8 <= bytes.len);
    var h: Hasher = .init();
    h.update(bytes[0..field_off]);
    const zeros: [8]u8 = @splat(0);
    h.update(&zeros);
    h.update(bytes[field_off + 8 ..]);
    return h.finalLow64();
}

/// Stamp the checksum field of a unit in place: compute over the field-zeroed
/// image and write the result little-endian at `field_off`.
pub fn stamp(bytes: []u8, field_off: usize) void {
    const sum = hash64FieldZeroed(bytes, field_off);
    std.mem.writeInt(u64, bytes[field_off..][0..8], sum, .little);
}

/// Verify a stamped unit. Total: never panics on arbitrary bytes once the
/// caller has established `field_off + 8 <= bytes.len`.
pub fn verify(bytes: []const u8, field_off: usize) bool {
    const stored = std.mem.readInt(u64, bytes[field_off..][0..8], .little);
    return stored == hash64FieldZeroed(bytes, field_off);
}

const testing = std.testing;
const sqzhash_mod = @import("sqzhash.zig");

test "the format key: its secret is the pinned one, its comptime expansion equals a runtime expansion, and hash64/hash128 under it reproduce the pinned vectors (low 64 = the u128's low half)" {
    try testing.expectEqual(sqzhash_mod.pinned_format_key, FORMAT_KEY);
    const runtime: Key = .init(FORMAT_KEY);
    try testing.expectEqualSlices(@TypeOf(runtime.v[0]), &runtime.v, &format_key.v);
    var buf: [4096]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i *% 31 +% 7);
    for (sqzhash_mod.pinned_format_vectors) |v| {
        if (v.len > buf.len) continue;
        try testing.expectEqual(v.h128, hash128(buf[0..v.len]));
        try testing.expectEqual(v.low64, hash64(buf[0..v.len]));
        try testing.expectEqual(low64(v.h128), hash64(buf[0..v.len]));
    }
    // The empty string under the format key (the source implementation's value).
    try testing.expectEqual(@as(u64, 0x60faebc5c5bda514), hash64(""));
}

test "keyFromSeed zero-extends: a seed's key is the key of the widened secret, differs from the format key, and reproduces the pinned seed vectors" {
    const k = keyFromSeed(sqzhash_mod.pinned_seed_key);
    const widened: Key = .init(@as(u128, 0xF1D5_A000_0000_0004));
    try testing.expectEqualSlices(@TypeOf(k.v[0]), &widened.v, &k.v);
    try testing.expect(!std.mem.eql(u8, std.mem.asBytes(&k.v), std.mem.asBytes(&format_key.v)));
    var buf: [4096]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i *% 31 +% 7);
    for (sqzhash_mod.pinned_seed_vectors) |v| {
        if (v.len > buf.len) continue;
        try testing.expectEqual(v.low64, hash64Keyed(&k, buf[0..v.len]));
    }
}

test "the streaming Hasher equals the one-shot hash across split points, under the format key and a caller's key" {
    var buf: [10_000]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i *% 13 +% 1);
    const k = keyFromSeed(77);
    for ([_]usize{ 0, 1, 15, 16, 17, 255, 256, 257, 4095, 4096, 4097, 10_000 }) |len| {
        for ([_]usize{ 1, 7, 16, 100, 4096, 10_000 }) |chunk| {
            var h: Hasher = .init();
            var hk: Hasher = .initKeyed(&k);
            var rest = buf[0..len];
            while (rest.len > 0) {
                const n = @min(chunk, rest.len);
                h.update(rest[0..n]);
                hk.update(rest[0..n]);
                rest = rest[n..];
            }
            try testing.expectEqual(hash128(buf[0..len]), h.final());
            try testing.expectEqual(hash64(buf[0..len]), h.finalLow64());
            try testing.expectEqual(hash64Keyed(&k, buf[0..len]), hk.finalLow64());
        }
    }
}

test "field-zeroed hash equals hashing a copy with the field zeroed" {
    var buf: [64]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i * 7 + 3);
    var copy = buf;
    @memset(copy[16..24], 0);
    try testing.expectEqual(hash64(&copy), hash64FieldZeroed(&buf, 16));
}

test "stamp then verify round-trips; any flipped bit fails" {
    var buf: [128]u8 = undefined;
    for (&buf, 0..) |*b, i| b.* = @truncate(i * 31);
    stamp(&buf, 40);
    try testing.expect(verify(&buf, 40));
    // Flip a payload bit outside the field.
    buf[3] ^= 0x10;
    try testing.expect(!verify(&buf, 40));
    buf[3] ^= 0x10;
    try testing.expect(verify(&buf, 40));
    // Flip a bit inside the field.
    buf[41] ^= 0x01;
    try testing.expect(!verify(&buf, 40));
    buf[41] ^= 0x01;
    // Every single-byte flip over a 4 KiB page fails verify.
    var page: [4096]u8 = undefined;
    for (&page, 0..) |*b, i| b.* = @truncate(i *% 97 +% 5);
    stamp(&page, 32);
    try testing.expect(verify(&page, 32));
    var i: usize = 0;
    while (i < page.len) : (i += 61) {
        page[i] ^= 0x80;
        try testing.expect(!verify(&page, 32));
        page[i] ^= 0x80;
    }
    try testing.expect(verify(&page, 32));
}

test "keyed hashes differ by key (the hash_seed collision defence) and from the format key's" {
    const name = "the-same-name";
    const k1 = keyFromSeed(1);
    const k2 = keyFromSeed(2);
    try testing.expect(hash64Keyed(&k1, name) != hash64Keyed(&k2, name));
    try testing.expect(hash64Keyed(&k1, name) != hash64(name));
    // The source implementation's value under the format key.
    try testing.expectEqual(@as(u64, 0x9617df09d9f0e11c), hash64(name));
}
