//! Directory placement by depth (charter §5.4 "Mint spread", amended
//! 2026-10-27; decision 0003 §BZ) — the Lustre DNE `max-inherit-rr` shape
//! (`lfs setdirstripe -D -i -1 --max-inherit-rr=N`): a new DIRECTORY at
//! depth ≤ D (the root's children are depth 1) is homed on the next
//! healthy member of the set's rotor; a directory deeper than D, and
//! every file, stays on its parent's member (§AA.1: a child on another
//! member than its parent makes every create, unlink and rename of it a
//! cross-owner intent with two device barriers). So a set spreads by the
//! top D levels of its tree — mdtest's `#test-dir.0/mdtest_tree.N.0`
//! rank directories (depth 3) over every member, their 10,000 children
//! each and every file local — with no per-mkdir load guess (an idle
//! setup shows no load; a hot phase would make every mkdir an intent).
//!
//! `D` is the SET's (`set_config.dir_spread_depth`, written by `format
//! --dir-spread-depth`, changed by `config set-dir-spread-depth` offline,
//! read by every mount of the set — never a per-mount option, so a fleet
//! follows one policy). A directory's depth rides its inode record
//! (`record.InodeValue.flags2` low byte: set at mkdir from the parent's,
//! saturating; informational after birth — a renamed directory keeps its
//! member and its depth, so its subtree's placement never changes under
//! it). PURE: every rule here is a function of its arguments.

const std = @import("std");

/// The depth byte's saturation: a directory this deep (or whose depth is
/// unknown — see `childDepth`) is deeper than ANY spread depth. The
/// format field's ceiling `DEPTH_MAX` sits one below it so the two never
/// meet.
pub const SATURATED: u8 = std.math.maxInt(u8);
/// The set field's range `0 … DEPTH_MAX`: 0 = never spread below the
/// root's children (§AA.1's behaviour: the control arm).
pub const DEPTH_MAX: u8 = SATURATED - 1;
/// Lustre's shipped `max-inherit-rr` default (`lfs setdirstripe -D -i -1`
/// on `/`: round-robin three levels deep); mdtest's rank directories sit
/// at depth 3.
pub const DEFAULT_DEPTH: u8 = 3;

pub fn depthValid(depth: u64) bool {
    return depth <= DEPTH_MAX;
}

/// A directory child as the mint sees it: the depth it will record —
/// `SATURATED` when the caller did not read it because the set cannot
/// spread (`dirsMaySpread`): the rule's answer is the same and the
/// directory still counts as one — and its ORDINAL among the directories
/// placed under its parent (`pickOrdinal`).
pub const Dir = struct { depth: u8, ordinal: u32 = 0 };

/// What the mint (`MetaSet.allocateIno`) is told about the child: a file
/// (a stripe, a whiteout) or a directory.
pub const Child = union(enum) {
    file,
    dir: Dir,

    pub fn depth(self: Child) u8 {
        return switch (self) {
            .file => 0,
            .dir => |d| d.depth,
        };
    }
};

/// The member pick's ordinal for a directory (`router.pickMintVolumeAt`):
/// a parent's directory children walk the healthy band from the parent's
/// OWN member — the first child beside its parent (a lone chain of
/// directories costs no intent), the next on the following member, and
/// so on — so siblings split evenly whatever else the set places
/// meanwhile, and a tree whose creation interleaves levels (each
/// directory, then its one subdirectory) never lands a whole level on one
/// member, as a counter shared by every pick did.
pub fn pickOrdinal(parent_member: u32, ordinal: u32) u64 {
    return @as(u64, parent_member) + ordinal;
}

/// The depth a NEW DIRECTORY records: the root's children 1; a parent at
/// `p ≥ 1` gives `min(p + 1, SATURATED)`; a parent whose depth is 0 — a
/// directory made before the field existed — gives `SATURATED` (its
/// subtree is deeper than any D: it was placed under §AA.1 and stays
/// local, never a root the rule would spread three levels under). Files
/// carry 0 and never call this.
pub fn childDepth(parent_is_root: bool, parent_depth: u8) u8 {
    if (parent_is_root) return 1;
    if (parent_depth == 0) return SATURATED;
    return if (parent_depth == SATURATED) SATURATED else parent_depth + 1;
}

/// Does a directory at `child_depth` take the rotor (`true`) or its
/// parent's member (`false`)? `child_depth` 0 is a file.
pub fn spreads(child_depth: u8, spread_depth: u8) bool {
    return child_depth != 0 and child_depth <= spread_depth;
}

/// The whole placement law of `MetaSet.allocateIno` for a child whose
/// parent this mount homes: on the parent's member iff it is a file, or a
/// directory deeper than D, or a directory under a legacy parent; a
/// parent on the NATIVE slot (`/`) or one this mount does not lease is the
/// caller's (the root's children and a foreign directory's child take
/// the member pick as before — §AA.1).
pub fn homesOnParent(parent_native: bool, parent_leased: bool, child_depth: u8, spread_depth: u8) bool {
    if (parent_native or !parent_leased) return false;
    return !spreads(child_depth, spread_depth);
}

const testing = std.testing;

/// A synthetic tree: `path` as a list of depths from the root; the depth
/// each directory records and whether it spreads under `d`.
fn walk(spread_depth: u8, levels: usize, out_depths: []u8, out_spreads: []bool) void {
    var parent_depth: u8 = 0;
    for (0..levels) |i| {
        const child = childDepth(i == 0, parent_depth);
        out_depths[i] = child;
        out_spreads[i] = spreads(child, spread_depth);
        parent_depth = child;
    }
}

test "the depth chain: the root's children are 1, each level one deeper, saturating at 255; a legacy parent's child is saturated and so is every descendant" {
    var depths: [300]u8 = undefined;
    var sp: [300]bool = undefined;
    walk(3, 300, &depths, &sp);
    try testing.expectEqual(@as(u8, 1), depths[0]);
    try testing.expectEqual(@as(u8, 2), depths[1]);
    try testing.expectEqual(@as(u8, 254), depths[253]);
    try testing.expectEqual(SATURATED, depths[254]);
    try testing.expectEqual(SATURATED, depths[299]);
    try testing.expectEqual(SATURATED, childDepth(false, 0));
    try testing.expectEqual(SATURATED, childDepth(false, SATURATED));
    try testing.expectEqual(SATURATED, childDepth(false, childDepth(false, 0)));
    // The root's own child is 1 whatever the root's record says.
    try testing.expectEqual(@as(u8, 1), childDepth(true, 0));
    try testing.expectEqual(@as(u8, 1), childDepth(true, 7));
}

test "tie: D = 0 spreads nothing below the root's children (§AA.1), D = 1 the root's children only, D = 3 three levels (mdtest's rank directories), D = 10 ten (the owner's Lustre habit), D = 254 every level but a saturated one; a file never spreads" {
    const Case = struct { d: u8, spread_levels: usize };
    for ([_]Case{ .{ .d = 0, .spread_levels = 0 }, .{ .d = 1, .spread_levels = 1 }, .{ .d = 3, .spread_levels = 3 }, .{ .d = 10, .spread_levels = 10 }, .{ .d = DEPTH_MAX, .spread_levels = 254 } }) |c| {
        var depths: [300]u8 = undefined;
        var sp: [300]bool = undefined;
        walk(c.d, 300, &depths, &sp);
        var n: usize = 0;
        for (sp, 0..) |s, i| {
            if (s) n += 1;
            // Spreading is a prefix of the chain: once a level is local every deeper one is.
            if (i > 0 and s) try testing.expect(sp[i - 1]);
        }
        try testing.expectEqual(c.spread_levels, n);
        try testing.expect(!spreads(0, c.d));
        try testing.expect(!spreads(SATURATED, c.d));
    }
    try testing.expect(depthValid(0) and depthValid(DEFAULT_DEPTH) and depthValid(DEPTH_MAX));
    try testing.expect(!depthValid(SATURATED) and !depthValid(1000));
    try testing.expectEqual(@as(u8, 3), DEFAULT_DEPTH);
}

test "the whole law: a native or foreign parent is never homed on; a leased parent homes a file, a deep directory and a legacy subtree's directory, and hands a directory at depth ≤ D to the rotor" {
    // `/`'s children and a foreign directory's child: the member pick, as before.
    try testing.expect(!homesOnParent(true, true, 1, 3));
    try testing.expect(!homesOnParent(true, true, 0, 3));
    try testing.expect(!homesOnParent(false, false, 0, 3));
    try testing.expect(!homesOnParent(false, false, 2, 3));
    // A leased directory: files and deep or legacy-rooted directories stay.
    try testing.expect(homesOnParent(false, true, 0, 3));
    try testing.expect(homesOnParent(false, true, 4, 3));
    try testing.expect(homesOnParent(false, true, SATURATED, 3));
    try testing.expect(homesOnParent(false, true, SATURATED, DEPTH_MAX));
    try testing.expect(homesOnParent(false, true, 2, 0));
    // The rotor takes a directory at depth 2 and 3 under D = 3, 2 … 10 under D = 10.
    try testing.expect(!homesOnParent(false, true, 2, 3));
    try testing.expect(!homesOnParent(false, true, 3, 3));
    try testing.expect(!homesOnParent(false, true, 10, 10));
    try testing.expect(homesOnParent(false, true, 11, 10));
}

test "the pick's ordinal: a parent's first directory child sits beside it and the next ones alternate; 32 siblings split 16/16 from either parent; a lone chain stays on one member; the `a, a/s, b, b/s` interleaving splits every level (the shared counter put each level on one member)" {
    // Two equal members: the ordinal mod 2 is the member.
    for ([_]u32{ 0, 1 }) |parent| {
        var on: [2]u32 = @splat(0);
        for (0..32) |n| on[@intCast(pickOrdinal(parent, @intCast(n)) % 2)] += 1;
        try testing.expectEqual(@as(u32, 16), on[0]);
        try testing.expectEqual(@as(u32, 16), on[1]);
        try testing.expectEqual(parent, @as(u32, @intCast(pickOrdinal(parent, 0) % 2)));
    }
    // A chain /a/b/c/d of first children: every level on member 0, no intent.
    var m: u32 = 0;
    for (0..8) |_| m = @intCast(pickOrdinal(m, 0) % 2);
    try testing.expectEqual(@as(u32, 0), m);
    // `r0, r0/s, r1, r1/s, …` under `/` on member 0: the r's alternate (their
    // ordinals 0, 1, 2 …) and each `s` sits beside its parent — 4/4 per level.
    var level1: [2]u32 = @splat(0);
    var level2: [2]u32 = @splat(0);
    for (0..8) |n| {
        const r: u32 = @intCast(pickOrdinal(0, @intCast(n)) % 2);
        level1[r] += 1;
        level2[@intCast(pickOrdinal(r, 0) % 2)] += 1;
    }
    try testing.expectEqual(@as(u32, 4), level1[0]);
    try testing.expectEqual(@as(u32, 4), level2[0]);
    // A shared counter stepped once per pick: r's at the even steps, s's at
    // the odd — one member per level.
    var shared: [2]u32 = @splat(0);
    var step: u64 = 0;
    for (0..8) |_| {
        shared[@intCast(step % 2)] += 1;
        step += 2;
    }
    try testing.expectEqual(@as(u32, 8), shared[0]);
}
