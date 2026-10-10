//! The data planes' durable records (charter §5.5 "Data-plane planes under
//! the symmetric model"; `docs/porting/sym_data_planes.md` §2.4, §5.1;
//! decision 0002 rows 238–239): the kind-5 data-allocation RUN delta and
//! the kind-9 shared-index record, plus the writer identity. The tree-0
//! control records (`alloc_lease`, `dead_member`, `recovered`,
//! `custody_quarantine`) are the appender/manager plane's codecs, re-
//! exported from `meta/sym/control_records.zig`. Every record is an
//! `extern struct` with little-endian values and big-endian keys (memcmp
//! order IS logical order), a comptime size assertion and a TOTAL decoder:
//! exactly the encoder's form decodes, a reserved byte that reads non-zero
//! is `Corrupt`, a version other than 1 is `BadVersion` (forward-only).
//! PORTABLE.
//!
//! The reference's quirks these codecs retire (0002 rows): one delta
//! record per BIT (a floor grant was 64 records), and an ASCII
//! `"shared_ref:"` prefix on a binary key in a tree that already has a
//! kind byte.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const assert = std.debug.assert;
const key = @import("../../meta/key.zig");

pub const DecodeError = error{ Truncated, BadVersion, Corrupt, BadLength, BadRef };

/// The one record version every codec here writes (forward-only).
pub const VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Writer identity (the lease holder, the dead member).
// ---------------------------------------------------------------------------

/// `{node_token, mount_slot, writer_id}` — the port's incarnation identity
/// (0002 row 185: `writer_id = xxh3(boot_id ‖ hostname ‖ mount_slot ‖ pid ‖
/// start_ticks)`, a u64, never a uuid).
pub const WriterIdentity = struct {
    node_token: u64,
    mount_slot: u32,
    writer_id: u64,

    pub fn eql(a: WriterIdentity, b: WriterIdentity) bool {
        return a.node_token == b.node_token and a.mount_slot == b.mount_slot and a.writer_id == b.writer_id;
    }

    /// The same NODE and MOUNT SLOT (the same-node takeover's identity
    /// test: a prior incarnation of this very mount point).
    pub fn sameMount(a: WriterIdentity, b: WriterIdentity) bool {
        return a.node_token == b.node_token and a.mount_slot == b.mount_slot;
    }

    /// The death ledger's key: the STABLE part (a rejoin after a storm
    /// must find its predecessor's words).
    pub fn stable(self: WriterIdentity) control.StableIdentity {
        return .{ .node_token = self.node_token, .mount_slot = self.mount_slot };
    }
};

// ---------------------------------------------------------------------------
// The tree-0 control records are the appender/manager plane's codecs
// (`meta/sym/control_records.zig`): `alloc_lease:{vol}`, `dead_member:{id}`,
// `recovered:{id, vol}`, `custody_quarantine:{slot}` — binary keys `kind ‖
// tag ‖ id`, LE values, one length each. Re-exported here so the data
// planes have ONE spelling of them.
// ---------------------------------------------------------------------------

pub const control = @import("../../meta/sym/control_records.zig");
pub const BitmapRef = control.BitmapRef;
pub const AllocLease = control.AllocLease;
pub const DeadMember = control.DeadMember;
pub const Recovered = control.Recovered;

/// The appender page names ≤ 4 extent runs (charter §5.5); the pages of one
/// data volume's bitmap live in extents of ONE grant, coalesced into runs,
/// so a lease record never needs more than the grant's run cap — a wire
/// integer above it is never an allocation authority.
pub const BITMAP_REFS_MAX: usize = 64;
pub const ALLOC_LEASE_MAX_LEN: usize = control.AllocLeaseHead.LEN + BITMAP_REFS_MAX * control.BitmapRefWire.LEN;

// ---------------------------------------------------------------------------
// §2.4 The kind-5 data-allocation RUN delta (0002 rows 44, 238).
// ---------------------------------------------------------------------------

pub const DELTA_KIND: u8 = key.TREE_DATA_ALLOC_RESERVED;
pub const DELTA_KEY_LEN: usize = 16;
pub const DELTA_LEN: usize = 16;
pub const TAG_SET: u8 = 0x11;
pub const TAG_CLEAR: u8 = 0x12;

/// `vol_tag u64 BE ‖ start_block u64 BE` (16 B).
pub fn deltaKey(vol_tag: u64, start: u64) [DELTA_KEY_LEN]u8 {
    var k: [DELTA_KEY_LEN]u8 = undefined;
    std.mem.writeInt(u64, k[0..8], vol_tag, .big);
    std.mem.writeInt(u64, k[8..16], start, .big);
    return k;
}

pub const DeltaKey = struct { vol_tag: u64, start: u64 };

pub fn decodeDeltaKey(k: []const u8) DecodeError!DeltaKey {
    if (k.len != DELTA_KEY_LEN) return error.BadLength;
    return .{ .vol_tag = std.mem.readInt(u64, k[0..8], .big), .start = std.mem.readInt(u64, k[8..16], .big) };
}

/// One record per RUN: a carve is contiguous by construction, so a floor
/// grant is ONE record instead of 64; a `finish_free` CLEAR is `len = 1`.
pub const DataAllocDeltaV4 = extern struct {
    /// `TAG_SET` / `TAG_CLEAR`.
    tag: u8,
    reserved: [3]u8 = @splat(0),
    /// Blocks in the run, ≥ 1.
    len: u32,
    /// The holder term that journaled it: a recovery folds the deltas of
    /// exactly the term it recovers, never another holder's stale residue.
    term: u64,

    pub fn isSet(self: DataAllocDeltaV4) bool {
        return self.tag == TAG_SET;
    }
};

comptime {
    assert(@sizeOf(DataAllocDeltaV4) == DELTA_LEN);
    assert(@offsetOf(DataAllocDeltaV4, "len") == 4);
    assert(@offsetOf(DataAllocDeltaV4, "term") == 8);
}

pub fn encodeDelta(d: DataAllocDeltaV4) [DELTA_LEN]u8 {
    assert(d.tag == TAG_SET or d.tag == TAG_CLEAR);
    assert(d.len >= 1);
    var r = d;
    r.reserved = @splat(0);
    return std.mem.toBytes(r);
}

/// Total: exactly 16 B, tag ∈ {SET, CLEAR}, `len ≥ 1`, reserved 0.
pub fn decodeDelta(bytes: []const u8) DecodeError!DataAllocDeltaV4 {
    if (bytes.len < DELTA_LEN) return error.Truncated;
    if (bytes.len != DELTA_LEN) return error.BadLength;
    const d = std.mem.bytesToValue(DataAllocDeltaV4, bytes[0..DELTA_LEN]);
    if (d.tag != TAG_SET and d.tag != TAG_CLEAR) return error.Corrupt;
    if (d.len == 0) return error.Corrupt;
    if (!std.mem.allEqual(u8, &d.reserved, 0)) return error.Corrupt;
    return d;
}

// ---------------------------------------------------------------------------
// §5.1 The kind-9 shared-index record (0002 row 239): the kind-6 ref key's
// body under kind 9, in the holder's HOME volume's tree 0.
// ---------------------------------------------------------------------------

pub const SHARED_INDEX_KIND: u8 = key.TREE_SHARED_INDEX;
pub const SHARED_INDEX_KEY_LEN: usize = key.BLOCK_REF_KEY_LEN;
pub const SHARED_INDEX_VALUE_LEN: usize = 4;

/// `09 ‖ vol_tag ‖ block_idx ‖ owner_ino ‖ block_index` (29 B) — one range
/// probe per block exactly like kind 6.
pub fn sharedIndexKey(vol_tag: u64, block_idx: u64, owner_ino: u64, block_index: u32) [SHARED_INDEX_KEY_LEN]u8 {
    var k = key.blockRefKey(vol_tag, block_idx, owner_ino, block_index);
    k[0] = SHARED_INDEX_KIND;
    return k;
}

pub fn decodeSharedIndexKey(k: []const u8) DecodeError!key.BlockRefKey {
    if (k.len != SHARED_INDEX_KEY_LEN) return error.BadLength;
    if (k[0] != SHARED_INDEX_KIND) return error.Corrupt;
    var body: [SHARED_INDEX_KEY_LEN]u8 = k[0..SHARED_INDEX_KEY_LEN].*;
    body[0] = key.TREE_BLOCK_REFS;
    return key.BlockRefKey.decode(&body) catch error.Corrupt;
}

/// The block prefix range `[09 ‖ vol_tag ‖ block_idx ‖ 0…, 09 ‖ vol_tag ‖
/// block_idx + 1 ‖ 0…)` as two key buffers.
pub fn sharedIndexBlockRange(vol_tag: u64, block_idx: u64) key.KeyRange {
    var r = key.blockRange(vol_tag, block_idx);
    r.lo.buf[0] = SHARED_INDEX_KIND;
    r.hi.buf[0] = SHARED_INDEX_KIND;
    return r;
}

pub const SharedIndexValueV4 = extern struct {
    version: u8 = VERSION,
    reserved: [3]u8 = @splat(0),
};

comptime {
    assert(@sizeOf(SharedIndexValueV4) == SHARED_INDEX_VALUE_LEN);
}

pub fn encodeSharedIndexValue() [SHARED_INDEX_VALUE_LEN]u8 {
    return std.mem.toBytes(SharedIndexValueV4{});
}

/// Total: exactly 4 B, version 1, reserved 0.
pub fn decodeSharedIndexValue(bytes: []const u8) DecodeError!void {
    if (bytes.len < SHARED_INDEX_VALUE_LEN) return error.Truncated;
    if (bytes.len != SHARED_INDEX_VALUE_LEN) return error.BadLength;
    const v = std.mem.bytesToValue(SharedIndexValueV4, bytes[0..SHARED_INDEX_VALUE_LEN]);
    if (v.version != VERSION) return error.BadVersion;
    if (!std.mem.allEqual(u8, &v.reserved, 0)) return error.Corrupt;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the kind-5 run delta: 16 B key ‖ 16 B value, SET/CLEAR tags, len ≥ 1, the term LE (0002 rows 44, 238)" {
    const k = deltaKey(0xDEAD, 640);
    try testing.expectEqual(@as(usize, 16), k.len);
    try testing.expect(key.lessThan(&deltaKey(0xDEAD, 639), &k));
    const dk = try decodeDeltaKey(&k);
    try testing.expectEqual(@as(u64, 0xDEAD), dk.vol_tag);
    try testing.expectEqual(@as(u64, 640), dk.start);
    try testing.expectError(error.BadLength, decodeDeltaKey(k[0..8]));
    const v = encodeDelta(.{ .tag = TAG_SET, .len = 64, .term = 3 });
    try testing.expectEqualSlices(u8, &.{ 0x11, 0, 0, 0, 64, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0 }, &v);
    const d = try decodeDelta(&v);
    try testing.expect(d.isSet());
    try testing.expectEqual(@as(u32, 64), d.len);
    var bad_tag = v;
    bad_tag[0] = 0x13;
    try testing.expectError(error.Corrupt, decodeDelta(&bad_tag));
    var zero = v;
    @memset(zero[4..8], 0);
    try testing.expectError(error.Corrupt, decodeDelta(&zero));
    var rsv = v;
    rsv[2] = 1;
    try testing.expectError(error.Corrupt, decodeDelta(&rsv));
    try testing.expectError(error.Truncated, decodeDelta(v[0..15]));
    try testing.expectError(error.BadLength, decodeDelta(&(v ++ [_]u8{0})));
    try testing.expectEqual(@as(u8, 5), DELTA_KIND);
}

test "the kind-9 shared-index key is the kind-6 body under kind 9 (29 B); the value is 4 B; the block range is one probe" {
    const k = sharedIndexKey(0xAB, 12, 0x1_0000_0005, 3);
    try testing.expectEqual(@as(usize, 29), k.len);
    try testing.expectEqual(@as(u8, 9), k[0]);
    const dec = try decodeSharedIndexKey(&k);
    try testing.expectEqual(@as(u64, 0xAB), dec.volTag());
    try testing.expectEqual(@as(u64, 12), dec.blockIdx());
    try testing.expectEqual(@as(u64, 0x1_0000_0005), dec.ownerIno());
    try testing.expectEqual(@as(u32, 3), dec.blockIndex());
    var wrong = k;
    wrong[0] = 6;
    try testing.expectError(error.Corrupt, decodeSharedIndexKey(&wrong));
    try testing.expectError(error.BadLength, decodeSharedIndexKey(k[0..28]));
    const r = sharedIndexBlockRange(0xAB, 12);
    try testing.expect(!key.lessThan(&k, r.lo.slice()));
    try testing.expect(key.lessThan(&k, r.hi.slice()));
    try testing.expect(!key.lessThan(&sharedIndexKey(0xAB, 13, 1, 0), r.hi.slice()));
    const v = encodeSharedIndexValue();
    try decodeSharedIndexValue(&v);
    try testing.expectError(error.BadLength, decodeSharedIndexValue(&(v ++ [_]u8{0})));
    var vv = v;
    vv[3] = 1;
    try testing.expectError(error.Corrupt, decodeSharedIndexValue(&vv));
}

fn checkDecodersTotal(bytes: []const u8) !void {
    _ = decodeDeltaKey(bytes) catch {};
    if (decodeDelta(bytes)) |d| try testing.expectEqualSlices(u8, bytes, &encodeDelta(d)) else |_| {}
    _ = decodeSharedIndexKey(bytes) catch {};
    decodeSharedIndexValue(bytes) catch {};
}

fn decodersTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    try checkDecodersTotal(buf[0..len]);
}

fn fuzzSlice(comptime bytes: []const u8) []const u8 {
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, bytes.len, .little);
        break :blk len ++ bytes[0..bytes.len].*;
    };
    return &framed;
}

test "fuzz: every data-plane record decoder is total and encode∘decode is the identity" {
    try fuzzkit.fuzz({}, decodersTotalFuzz, .{ .corpus = &.{
        fuzzSlice(&encodeDelta(.{ .tag = TAG_CLEAR, .len = 1, .term = 9 })),
        fuzzSlice(&sharedIndexKey(1, 2, 3, 4)),
        fuzzSlice(&encodeSharedIndexValue()),
    } });
}

test "property: the decoders are total under a PRNG, including mutations of valid encodings" {
    var prng = std.Random.DefaultPrng.init(0x5E4D);
    const r = prng.random();
    var buf: [256]u8 = undefined;
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        try checkDecodersTotal(buf[0..len]);
    }
    const enc = encodeDelta(.{ .tag = TAG_SET, .len = 64, .term = 3 });
    for (0..5_000) |_| {
        var m = enc;
        m[r.intRangeLessThan(usize, 0, m.len)] ^= @as(u8, 1) << r.int(u3);
        try checkDecodersTotal(&m);
    }
}
