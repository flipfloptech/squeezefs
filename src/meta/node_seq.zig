//! Node incarnation seqs — ONE SPACE per appender incarnation (charter §5.1
//! "Node"; `docs/porting/kv_node.md` §3). A node's `node_seq` is the CoW
//! law's identity word: the child/root pointer checks, the frame
//! incarnation check that ends a recycled extent's log at a previous node's
//! frames, and the residue ceiling all compare seqs for EQUALITY and rest
//! on "no two nodes ever carry one seq".
//!
//! The space law: the volume base `B = nodeSeqBase(uuid)` (top bit clear,
//! 2^63 of headroom) opens incarnation 0's space — the manager's and every
//! flat / unarmed mount's (legacy seeding `max(ledger.seq, watermark)`,
//! raised by every replayed root). Every JOINED appender takes incarnation
//! `o ≥ 1`, a DISJOINT space `[B + o·2^K, B + (o+1)·2^K)` whose ordinal the
//! manager mints from the durable tree-0 counter, barriered before the
//! reply; a rejoin is a new incarnation (its old space is dead, nothing
//! mints there again). A joiner's handle is never RAISED by a replayed
//! pointer (a fresh space has no residue of its own).
//!
//! Every ORDER comparison of node seqs is confined to one space or made
//! equality; a numerically HIGHER stale frame stamp still classifies as a
//! stale incarnation (node.zig), never loud.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");

/// `K = 38`: 2^38 ≈ 2.7 × 10^11 mints per incarnation — a joiner at
/// 200 SMOs/s (the storm rows' rate) for 43 years; leaves 25 bits =
/// 2^25 ≈ 33.5 M incarnations per volume (15 k mounts re-joining daily for
/// six years).
pub const INCARNATION_SPACE_BITS: u6 = 38;
/// One incarnation's span.
pub const INCARNATION_SPACE: u64 = @as(u64, 1) << INCARNATION_SPACE_BITS;
/// The bits left for incarnations above `B` (the top bit is clear).
pub const INCARNATION_ORDINAL_BITS: u6 = 63 - INCARNATION_SPACE_BITS;
/// The highest ordinal the volume can mint: `B + (o + 1) · 2^K` must fit
/// below 2^64 for every `B < 2^63`.
pub const INCARNATION_ORDINAL_MAX: u64 = (@as(u64, 1) << INCARNATION_ORDINAL_BITS) - 2;

comptime {
    std.debug.assert(@as(u32, INCARNATION_SPACE_BITS) + @as(u32, INCARNATION_ORDINAL_BITS) == 63);
}

/// `u64 LE of uuid[0..8] & (u64::MAX >> 1)`: quick format zeroes only
/// `[0, heap.start)`, so dead-generation frames survive on reused extents;
/// a uuid-derived base makes a cross-generation seq collision as
/// improbable as a checksum collision while keeping the builder
/// deterministic (fixed uuid ⇒ fixed image). The clear top bit is 2^63 of
/// monotonic headroom before wrap.
pub fn nodeSeqBase(uuid: [16]u8) u64 {
    return std.mem.readInt(u64, uuid[0..8], .little) & (std.math.maxInt(u64) >> 1);
}

/// The base of incarnation `ordinal`'s space over `volume_base`, or null
/// past the volume's capacity (the caller refuses loud). All checked
/// arithmetic: the whole span must fit.
pub fn incarnationBase(volume_base: u64, ordinal: u64) ?u64 {
    if (ordinal > INCARNATION_ORDINAL_MAX) return null;
    const offset = std.math.mul(u64, ordinal, INCARNATION_SPACE) catch return null;
    const start = std.math.add(u64, volume_base, offset) catch return null;
    _ = std.math.add(u64, start, INCARNATION_SPACE) catch return null;
    return start;
}

pub const ScreenRefusal = enum {
    below_base,
    manager_space,
    off_stride,
    past_capacity,

    pub fn why(self: ScreenRefusal) []const u8 {
        return switch (self) {
            .below_base => "below the volume's base",
            .manager_space => "incarnation 0 is the manager's own space",
            .off_stride => "not on the incarnation stride — it would straddle two spaces",
            .past_capacity => "past the volume's incarnation capacity",
        };
    }
};

/// The refusal text for a rejected `Joined.node_seq_base` wire word.
pub fn writeScreenRefusal(w: *std.Io.Writer, volume_base: u64, word: u64, r: ScreenRefusal) std.Io.Writer.Error!void {
    try w.print("Joined.node_seq_base 0x{x} rejected: {s} (the volume's base is 0x{x}, a joiner's base is B + o · 2^{d} for 1 ≤ o ≤ {d} — a buggy or hostile manager frame; nothing installed)", .{ word, r.why(), volume_base, INCARNATION_SPACE_BITS, INCARNATION_ORDINAL_MAX });
}

/// The joiner's screen of the `Joined.node_seq_base` wire word: a
/// legitimate base is `B + o · 2^K` for `1 ≤ o ≤ ORDINAL_MAX` and nothing
/// else — `B` itself would put the joiner back into the manager's space, a
/// word off the stride straddles two spaces. Pure and total.
pub fn screenIncarnationBase(volume_base: u64, word: u64, diag: ?*ScreenRefusal) error{Rejected}!u64 {
    const refuse = struct {
        fn f(d: ?*ScreenRefusal, r: ScreenRefusal) error{Rejected} {
            if (d) |p| p.* = r;
            return error.Rejected;
        }
    }.f;
    if (word < volume_base) return refuse(diag, .below_base);
    const offset = word - volume_base;
    if (offset == 0) return refuse(diag, .manager_space);
    if (offset % INCARNATION_SPACE != 0) return refuse(diag, .off_stride);
    const base = incarnationBase(volume_base, offset / INCARNATION_SPACE) orelse return refuse(diag, .past_capacity);
    if (base != word) return refuse(diag, .past_capacity);
    return word;
}

// ---------------------------------------------------------------------------
// The tree-0 incarnation counter record.
// ---------------------------------------------------------------------------

/// The tree-0 incarnation counter's VALUE: how many joiner incarnations
/// this volume has minted (volume-local — every volume of a set has its
/// own base and its own counter). Its binary key is `sym/control_records
/// .nodeSeqIncarnationsKey` (one key convention for tree 0).
pub const NODE_SEQ_INCARNATIONS_VERSION: u8 = 1;
/// `version u8 ‖ minted u64 LE`.
pub const NODE_SEQ_INCARNATIONS_LEN: usize = 1 + 8;

pub fn encodeIncarnations(minted: u64) [NODE_SEQ_INCARNATIONS_LEN]u8 {
    var out: [NODE_SEQ_INCARNATIONS_LEN]u8 = undefined;
    out[0] = NODE_SEQ_INCARNATIONS_VERSION;
    std.mem.writeInt(u64, out[1..9], minted, .little);
    return out;
}

/// Total: empty ⇒ Corrupt; version ≠ 1 ⇒ Corrupt (forward-only); length
/// ≠ 9 ⇒ Corrupt.
pub fn decodeIncarnations(value: []const u8) error{Corrupt}!u64 {
    if (value.len == 0) return error.Corrupt;
    if (value[0] != NODE_SEQ_INCARNATIONS_VERSION) return error.Corrupt;
    if (value.len != NODE_SEQ_INCARNATIONS_LEN) return error.Corrupt;
    return std.mem.readInt(u64, value[1..9], .little);
}

// ---------------------------------------------------------------------------
// The mint handle.
// ---------------------------------------------------------------------------

/// The per-volume node-seq mint handle: the counter, its space's exclusive
/// ceiling, and whether replayed pointers may RAISE it (the shared
/// incarnation-0 space's watermark law) or not (a joined incarnation's
/// fresh space).
pub const NodeSeqHandle = struct {
    next: std.atomic.Value(u64),
    ceiling: u64,
    raises: bool,

    /// The legacy shared handle — a flat volume's, every unarmed mount's:
    /// seeded from the ledger, no ceiling, raised by every replayed root.
    pub fn shared(seed: u64) NodeSeqHandle {
        return .{ .next = .init(seed), .ceiling = std.math.maxInt(u64), .raises = true };
    }

    /// The manager's handle on a forest volume: the legacy seeding and the
    /// raise law, bounded by incarnation 0's span over `volume_base` (a
    /// raise past it — a dead joiner's root, residue stamps — is a foreign
    /// space and is ignored).
    pub fn sharedBounded(seed: u64, volume_base: u64) NodeSeqHandle {
        return .{ .next = .init(seed), .ceiling = incarnationBase(volume_base, 1) orelse std.math.maxInt(u64), .raises = true };
    }

    /// A JOINED appender's handle: incarnation `base`'s fresh space, never
    /// raised.
    pub fn incarnation(base: u64) NodeSeqHandle {
        return .{ .next = .init(base), .ceiling = base +| INCARNATION_SPACE, .raises = false };
    }

    /// Mint the next node seq (first mint = base + 1) — REFUSED loud at the
    /// space's ceiling, never wrapped into the next incarnation's (or, on
    /// the legacy handle, past u64::MAX). The counter is parked AT the
    /// ceiling so a retry refuses too.
    pub fn mint(self: *NodeSeqHandle) error{SpaceExhausted}!u64 {
        // acq_rel pairs with `raiseTo`'s fetchMax and every other mint.
        const seq = self.next.fetchAdd(1, .acq_rel) + 1;
        if (seq >= self.ceiling) {
            _ = self.next.fetchMin(self.ceiling, .acq_rel);
            return error.SpaceExhausted;
        }
        return seq;
    }

    pub fn writeExhausted(self: *const NodeSeqHandle, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("node-seq space exhausted: this appender incarnation minted 2^{d} node seqs (ceiling 0x{x}) — remount to take a fresh incarnation (design-symmetric-metadata §5.3.3, PR 13)", .{ INCARNATION_SPACE_BITS, self.ceiling });
    }

    /// The watermark raise: a replayed root/pointer seq floors the counter —
    /// INSIDE this handle's own space only. A joined handle never raises;
    /// the shared handle ignores a target in another incarnation's space
    /// (the disjointness makes a collision there impossible by construction).
    pub fn raiseTo(self: *NodeSeqHandle, seq: u64) void {
        if (self.raises and seq < self.ceiling) _ = self.next.fetchMax(seq, .acq_rel);
    }

    /// The counter's current value (the ledger's `node_seq_watermark`).
    pub fn load(self: *const NodeSeqHandle) u64 {
        return self.next.load(.acquire);
    }

    /// The space's exclusive ceiling (`u64::MAX` on the legacy handle).
    pub fn ceilingValue(self: *const NodeSeqHandle) u64 {
        return self.ceiling;
    }
};

/// The incarnation ordinal whose space holds `seq` over `volume_base`: 0
/// for incarnation 0's `[B, B + 2^K)` and for a legacy seed below `B`
/// (nothing else mints there). Two seqs order only inside one ordinal.
pub fn spaceOf(volume_base: u64, seq: u64) u64 {
    if (seq < volume_base) return 0;
    return (seq - volume_base) >> INCARNATION_SPACE_BITS;
}

/// The writer's handle on its own volume (a flat volume's, the manager's),
/// seeded at the ledger's `max(seq, node_seq_watermark)`: bounded by
/// incarnation 0's span over the volume's base, so a raise to a stamp of
/// another space — a recycled extent's residue from an earlier format
/// (`KvTree.create`), a dead joiner's root — is ignored and the volume
/// never re-mints another space's seqs. The frame incarnation check is an
/// equality: an earlier format's frame past a new node's log end at the
/// same extent would verify as the node's own (xfstests generic/753). A
/// seed already past the span (a pre-bound writer lifted the counter out
/// and its watermark persisted it) keeps the unbounded handle — bounded,
/// it could mint nothing; the open names it (`seedEscaped`).
pub fn managerHandle(seed: u64, volume_base: u64) NodeSeqHandle {
    if (seedEscaped(seed, volume_base)) return NodeSeqHandle.shared(seed);
    return NodeSeqHandle.sharedBounded(seed, volume_base);
}

/// Whether `seed` lies at or past incarnation 0's span over `volume_base`
/// (the bounded handle's ceiling).
pub fn seedEscaped(seed: u64, volume_base: u64) bool {
    return seed >= (incarnationBase(volume_base, 1) orelse std.math.maxInt(u64));
}

// ---------------------------------------------------------------------------
// Tests (node_seq.rs:249–298 and the spec's §3 laws).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the space arithmetic partitions the 63 bits and refuses past them" {
    try testing.expectEqual(@as(u64, 1 << 38), INCARNATION_SPACE);
    try testing.expectEqual(@as(u64, (1 << 25) - 2), INCARNATION_ORDINAL_MAX);
    const b: u64 = 0x1234_5678_9abc_def0 & (std.math.maxInt(u64) >> 1);
    try testing.expectEqual(@as(?u64, b), incarnationBase(b, 0));
    try testing.expectEqual(@as(?u64, b + INCARNATION_SPACE), incarnationBase(b, 1));
    try testing.expectEqual(@as(?u64, b + 2 * INCARNATION_SPACE), incarnationBase(b, 2));
    // Disjoint, adjacent.
    try testing.expectEqual(incarnationBase(b, 1).? + INCARNATION_SPACE, incarnationBase(b, 2).?);
    // The widest base still fits every admitted ordinal; one past refuses.
    const top: u64 = std.math.maxInt(u64) >> 1;
    try testing.expect(incarnationBase(top, INCARNATION_ORDINAL_MAX) != null);
    try testing.expectEqual(@as(?u64, null), incarnationBase(top, INCARNATION_ORDINAL_MAX + 1));
    try testing.expectEqual(@as(?u64, null), incarnationBase(top, std.math.maxInt(u64)));
}

test "node_seq_base: uuid[0..8] LE with the top bit clear; agrees with the geometry module's copy" {
    const geometry = @import("geometry.zig");
    const uuid: [16]u8 = "kv-backend-test!".*;
    try testing.expectEqual(std.mem.readInt(u64, "kv-backe", .little), nodeSeqBase(uuid));
    try testing.expectEqual(geometry.nodeSeqBase(uuid), nodeSeqBase(uuid));
    const ones: [16]u8 = @splat(0xFF);
    try testing.expectEqual(std.math.maxInt(u64) >> 1, nodeSeqBase(ones));
    try testing.expectEqual(@as(u64, 0), nodeSeqBase(@splat(0)));
    // The reference flat volume's uuid (ref-sw.img, 2026-10-05): base + 1 is
    // its first root's seq.
    const ref_uuid = [16]u8{ 0x6b, 0x4e, 0xab, 0xfd, 0x7a, 0xeb, 0x4b, 0xd7, 0x96, 0xca, 0x10, 0xd7, 0x73, 0xc4, 0x9f, 0x7b };
    try testing.expectEqual(@as(u64, 0x574beb7afdab4e6c), nodeSeqBase(ref_uuid) + 1);
}

test "the handle mints inside its space (first mint = base + 1) and parks at the ceiling" {
    var h = NodeSeqHandle.incarnation(1000);
    try testing.expectEqual(@as(u64, 1001), try h.mint());
    h.raiseTo(5_000_000); // a joined handle never raises
    try testing.expectEqual(@as(u64, 1002), try h.mint());
    try testing.expectEqual(1000 + INCARNATION_SPACE, h.ceilingValue());
    var tiny: NodeSeqHandle = .{ .next = .init(10), .ceiling = 12, .raises = true };
    try testing.expectEqual(@as(u64, 11), try tiny.mint());
    try testing.expectError(error.SpaceExhausted, tiny.mint());
    try testing.expectError(error.SpaceExhausted, tiny.mint());
    tiny.raiseTo(100); // outside the space: ignored
    try testing.expectEqual(@as(u64, 12), tiny.load());
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try tiny.writeExhausted(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "node-seq space exhausted: this appender incarnation minted 2^38 node seqs (ceiling 0xc)"));
    var sh = NodeSeqHandle.shared(7);
    sh.raiseTo(40);
    try testing.expectEqual(@as(u64, 41), try sh.mint());
    try testing.expectEqual(std.math.maxInt(u64), sh.ceilingValue());
    // The bounded shared handle: ceiling = incarnation 1's base; a raise
    // into a joiner's space is ignored, one inside lands.
    const b: u64 = 0x100;
    var sb = NodeSeqHandle.sharedBounded(5, b);
    try testing.expectEqual(b + INCARNATION_SPACE, sb.ceilingValue());
    sb.raiseTo(b + INCARNATION_SPACE + 7);
    try testing.expectEqual(@as(u64, 5), sb.load());
    sb.raiseTo(b + 9);
    try testing.expectEqual(b + 9, sb.load());
    // A joined handle at the saturating edge never overflows.
    const edge = NodeSeqHandle.incarnation(std.math.maxInt(u64) - 1);
    try testing.expectEqual(std.math.maxInt(u64), edge.ceilingValue());
}

test "the writer's handle is bounded by incarnation 0's span: an earlier format's residue stamp (another base) and a joiner's root are ignored, a stamp of its own space lands; a seed a pre-bound writer already lifted past the span keeps the unbounded handle; the space ordinal of a seq" {
    const b: u64 = 1 << 40;
    var h = managerHandle(b + 2, b);
    try testing.expect(!seedEscaped(b + 2, b));
    try testing.expectEqual(b + INCARNATION_SPACE, h.ceilingValue());
    h.raiseTo(b + 64 * INCARNATION_SPACE + 1);
    h.raiseTo(b + INCARNATION_SPACE + 3);
    try testing.expectEqual(b + 3, try h.mint());
    h.raiseTo(b + 9);
    try testing.expectEqual(b + 10, try h.mint());
    // A legacy seed below the base (a ledger seq) is inside the bound.
    try testing.expectEqual(b + INCARNATION_SPACE, managerHandle(7, b).ceilingValue());
    // The space ordinal: 0 below and inside incarnation 0, o inside o.
    try testing.expectEqual(@as(u64, 0), spaceOf(b, 7));
    try testing.expectEqual(@as(u64, 0), spaceOf(b, b + INCARNATION_SPACE - 1));
    try testing.expectEqual(@as(u64, 3), spaceOf(b, b + 3 * INCARNATION_SPACE + 9));
    const escaped = b + 64 * INCARNATION_SPACE + 5;
    try testing.expect(seedEscaped(escaped, b));
    var legacy = managerHandle(escaped, b);
    try testing.expectEqual(std.math.maxInt(u64), legacy.ceilingValue());
    try testing.expectEqual(escaped + 1, try legacy.mint());
}

test "the incarnation record round-trips and refuses other shapes" {
    try testing.expectEqual(@as(u64, 77), try decodeIncarnations(&encodeIncarnations(77)));
    try testing.expectEqual(@as(usize, 9), encodeIncarnations(0).len);
    try testing.expectEqualSlices(u8, &.{ 1, 77, 0, 0, 0, 0, 0, 0, 0 }, &encodeIncarnations(77));
    try testing.expectError(error.Corrupt, decodeIncarnations(&.{}));
    try testing.expectError(error.Corrupt, decodeIncarnations(&.{ 2, 0, 0, 0, 0, 0, 0, 0, 0 }));
    try testing.expectError(error.Corrupt, decodeIncarnations(&.{ 1, 0, 0 }));
    try testing.expectError(error.Corrupt, decodeIncarnations(&.{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0 }));
}

test "the joiner's screen admits exactly B + o·2^K for 1 ≤ o ≤ ORDINAL_MAX" {
    const b: u64 = 0x0abc_0000_0000_0000;
    var diag: ScreenRefusal = undefined;
    try testing.expectEqual(b + INCARNATION_SPACE, try screenIncarnationBase(b, b + INCARNATION_SPACE, &diag));
    try testing.expectEqual(b + 3 * INCARNATION_SPACE, try screenIncarnationBase(b, b + 3 * INCARNATION_SPACE, &diag));
    try testing.expectError(error.Rejected, screenIncarnationBase(b, b - 1, &diag));
    try testing.expectEqual(ScreenRefusal.below_base, diag);
    try testing.expectError(error.Rejected, screenIncarnationBase(b, b, &diag));
    try testing.expectEqual(ScreenRefusal.manager_space, diag);
    try testing.expectError(error.Rejected, screenIncarnationBase(b, b + INCARNATION_SPACE + 1, &diag));
    try testing.expectEqual(ScreenRefusal.off_stride, diag);
    const top: u64 = std.math.maxInt(u64) >> 1;
    try testing.expectError(error.Rejected, screenIncarnationBase(top, top + (INCARNATION_ORDINAL_MAX + 1) * INCARNATION_SPACE, &diag));
    try testing.expectEqual(ScreenRefusal.past_capacity, diag);
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeScreenRefusal(&w, b, b, .manager_space);
    try testing.expectEqualStrings("Joined.node_seq_base 0xabc000000000000 rejected: incarnation 0 is the manager's own space (the volume's base is 0xabc000000000000, a joiner's base is B + o · 2^38 for 1 ≤ o ≤ 33554430 — a buggy or hostile manager frame; nothing installed)", w.buffered());
}

fn incarnationsFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [32]u8 = undefined;
    const len = smith.slice(&buf);
    if (decodeIncarnations(buf[0..len])) |minted| try std.testing.expectEqualSlices(u8, buf[0..len], &encodeIncarnations(minted)) else |_| {}
}

test "fuzz: the node-seq incarnations record decoder is total and a decoded value re-encodes to its bytes" {
    try fuzzkit.fuzz({}, incarnationsFuzz, .{});
}
