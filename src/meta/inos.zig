//! Monotonic inos (charter §5.3 "Inodes": "monotonic `next_ino` watermark,
//! never reused, recovered as `max(ledger, replayed window max) + 1`; ino 1
//! = root"; §5.4 the slot ↔ ino arithmetic; §5.2 "Journal economy": batched
//! `destroy_inodes`; survey `docs/porting/kv_tree_cache_commit_survey.md`
//! §2.14, §2.16, §7 row 15; reference `ino_lane.rs` + `lane_core.rs`).
//! PORTABLE: atomics and arithmetic; the open path feeds it the replayed
//! window, the committer the mint, `statfs` the derivation.
//!
//! Three laws share this module:
//!
//! - **The watermark.** One `fetch_add` per create, no reuse, no
//!   free-on-failure (a failed create burns the ino; crash-skipped ranges
//!   waste nothing that matters). WHY never reused: the daemon's IPC binding
//!   table and the FUSE nodeid space rest on it — a reused ino aliases a
//!   file (and an fd binding) immediately. No reuse is also what lets
//!   `destroyInodes` be ONE batched entry of inode `Delete`s + xattr
//!   `Delete`s with no free-ordering rule at all.
//! - **Recovery (DUR-8c).** `next_ino = max(ledger.next_ino, max ino the
//!   replayed window MENTIONS + 1)` where "mentions" = inode keys, dentry
//!   values' `child_ino`, xattr keys — not just surviving inode records: a
//!   torn-dropped create whose dentry or xattr survived would otherwise let
//!   the watermark fall back and RE-MINT that ino over the survivor. Guest
//!   keyspaces keep per-slot maxima.
//! - **Lanes** (`lane_core`): over a space based at `base` and partitioned
//!   `writers` ways, value `v` belongs to lane `(v − base) % writers` and
//!   appender `w` mints only lane-`w` values — a residue class, so the
//!   partition is arbitration-free (an appender needs only its own id) and
//!   every ino is ATTRIBUTABLE to its minter. Solo (`writers == 1`) is the
//!   shipped path and is arithmetically identical to the dense counter
//!   (a tie test, not a comment).

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../core/counters.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const journal = @import("journal.zig");
const geometry = @import("geometry.zig");
const tx = @import("tx.zig");

const assert = std.debug.assert;

pub const AppendPartition = journal.AppendPartition;

/// Ino 1 is the root (pinned to routing slot 0).
pub const ROOT_INO: u64 = 1;
/// The first ino a local space hands out: ino 1 is the root pin and locals
/// below 2 are reserved — the watermark and every slot cursor start here.
pub const LOCAL_INO_BASE: u64 = 2;

pub const stats = struct {
    /// `destroyed_inodes`: inodes whose destroy COMMITTED (Σ over volumes);
    /// turns the never-decreasing watermark into a live-population gauge.
    pub var destroyed_inodes: counters.Gauge = .{};
    /// `live_inodes`: the population `statfs` reports as used (the set-wide
    /// Σ of `liveInodes` plus the root), published by the statfs path.
    pub var live_inodes: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// The watermark.
// ---------------------------------------------------------------------------

/// The per-volume `next_ino` (native space). The publication edge: a
/// `snapshot` taken after a mint's record reached the tree is strictly
/// greater than that minted value, so a ledger record never under-declares
/// an ino it covers; a mint that commits after the snapshot rides the
/// journal and is recovered by the replay fold; a mint that never commits
/// is burned. `.acq_rel` on the mint pairs with `snapshot`'s `.acquire`.
pub const InoWatermark = struct {
    next: std.atomic.Value(u64),

    /// Seeded at `max(next_ino, LOCAL_INO_BASE)`: a virgin volume mints 2.
    pub fn init(next_ino: u64) InoWatermark {
        return .{ .next = .init(@max(next_ino, LOCAL_INO_BASE)) };
    }

    /// One `fetch_add`, no reuse, no free-on-failure.
    pub fn allocate(self: *InoWatermark) u64 {
        return self.next.fetchAdd(1, .acq_rel);
    }

    /// Reserve `n` consecutive locals (an intent-supply grant): one
    /// `fetch_add`; unused ones burn exactly as a failed create's ino.
    pub fn reserveRange(self: *InoWatermark, n: u64) u64 {
        return self.next.fetchAdd(n, .acq_rel);
    }

    /// The value a ledger record carries.
    pub fn snapshot(self: *const InoWatermark) u64 {
        return self.next.load(.acquire);
    }

    /// Raise to at least `floor` (recovery, a dominating peer watermark).
    /// Monotone: a stale floor never regresses a fresher mint.
    pub fn installFloor(self: *InoWatermark, floor: u64) void {
        _ = self.next.fetchMax(@max(floor, LOCAL_INO_BASE), .acq_rel);
    }
};

/// §4.8: `next_ino = max(ledger watermark, replayed inos + 1)`. `replayed_max`
/// is 0 when the window mentions nothing (then the ledger alone decides).
pub fn recoverNextIno(ledger_next_ino: u64, replayed_max: u64) u64 {
    return @max(ledger_next_ino, replayed_max +| 1);
}

// ---------------------------------------------------------------------------
// The DUR-8c recovery fold over the replayed window.
// ---------------------------------------------------------------------------

/// The maxima the replayed window mentions, per keyspace.
pub const RecoveryFold = struct {
    /// Native locals (`< 2^40`): the watermark's input.
    native_max: u64 = 0,
    /// Per guest slot: the RAW local maxima (each slot is its own cursor).
    guest_max: std.AutoHashMapUnmanaged(u16, u64) = .empty,

    pub fn deinit(self: *RecoveryFold, gpa: Allocator) void {
        self.guest_max.deinit(gpa);
        self.* = undefined;
    }

    /// Fold ONE replayed record frame. Interior records (level > 0) and
    /// non-slot-tree kinds mention no ino. On a forest volume the key is
    /// the forest key: its kind byte must agree with the tag (a disagreeing
    /// pair is counted as a forest-key violation and refuses the mount).
    /// A value that does not decode (a dentry `Delete` has none) mentions
    /// nothing — the surviving dentry's PUT is what names the child.
    pub fn noteRecord(self: *RecoveryFold, gpa: Allocator, framing: tx.KeyFraming, frame: journal.RecordFrame) (Allocator.Error || error{Corrupt})!void {
        const t = journal.untag(frame.tag);
        if (t.level > 0) return;
        if (!key.isSlotTreeKind(t.tree_id)) return;
        var legacy_buf: key.KeyBuf = undefined;
        const legacy: []const u8 = switch (framing) {
            .flat => frame.key,
            .forest => blk: {
                const split = key.splitForestKey(frame.key) catch {
                    tx.stats.meta_kv_forest_key_violations.inc();
                    return error.Corrupt;
                };
                if (split.kind != t.tree_id) {
                    tx.stats.meta_kv_forest_key_violations.inc();
                    return error.Corrupt;
                }
                legacy_buf = split.legacy;
                break :blk legacy_buf.slice();
            },
        };
        const mentioned: ?u64 = switch (t.tree_id) {
            key.TREE_INODES => key.decodeInodeKey(legacy) catch null,
            key.TREE_DENTRIES => if (record.DentryValue.decode(frame.value)) |d| d.child_ino else |_| null,
            key.TREE_XATTRS => if (key.XattrKey.decode(legacy)) |x| x.ino() else |_| null,
            else => null,
        };
        const ino = mentioned orelse return;
        if (key.splitGuestLocal(ino)) |g| {
            const e = try self.guest_max.getOrPutValue(gpa, g.slot, 0);
            e.value_ptr.* = @max(e.value_ptr.*, g.local);
        } else {
            self.native_max = @max(self.native_max, ino);
        }
    }

    /// Fold every content record of the replayed entries (seq order).
    pub fn foldEntries(self: *RecoveryFold, gpa: Allocator, framing: tx.KeyFraming, entries: []const journal.ReplayedEntry) (Allocator.Error || error{Corrupt})!void {
        for (entries) |e| {
            var it = e.records();
            while (it.next()) |frame| try self.noteRecord(gpa, framing, frame);
        }
    }

    /// The recovered native watermark.
    pub fn nextIno(self: *const RecoveryFold, ledger_next_ino: u64) u64 {
        return recoverNextIno(ledger_next_ino, self.native_max);
    }

    /// The recovered cursor of guest slot `slot` given its stamped cursor
    /// (0 when the stamp carries none).
    pub fn guestCursor(self: *const RecoveryFold, slot: u16, stamped: u64) u64 {
        const seen = self.guest_max.get(slot) orelse 0;
        return @max(recoverNextIno(stamped, seen), LOCAL_INO_BASE);
    }
};

// ---------------------------------------------------------------------------
// The live population (POSIX-1 `statfs`).
// ---------------------------------------------------------------------------

/// The live inode population of one volume: the progression of EVERY
/// allocation cursor — the native watermark and each hosted guest cursor
/// (`cursor − 2`: the reserved base is 2 everywhere; the root is added back
/// ONCE by the caller across the set) plus each lane cursor's EXACT mint
/// count (a lane strides by `writers`, so `cursor − 2` would over-report by
/// that factor) — minus the destroys committed on the volume. An
/// over-report, never an under-report: `statfs` may only be pessimistic
/// about free slots, and burned inos count as the §4.8 law describes.
pub fn liveInodes(native_next: u64, guest_cursors: []const u64, lane_minted: []const u64, destroyed: u64) u64 {
    var allocated: u64 = native_next -| LOCAL_INO_BASE;
    for (guest_cursors) |c| allocated +|= c -| LOCAL_INO_BASE;
    for (lane_minted) |m| allocated +|= m;
    return allocated -| destroyed;
}

/// Per-volume destroy tally — moves only on a COMMITTED destroy (a failed
/// commit leaves the records live; a bisect retry re-counts the halves it
/// actually lands).
pub const DestroyTally = struct {
    destroyed: std.atomic.Value(u64) = .init(0),

    pub fn noteCommitted(self: *DestroyTally, n: u64) void {
        _ = self.destroyed.fetchAdd(n, .monotonic);
        stats.destroyed_inodes.add(n);
    }

    pub fn load(self: *const DestroyTally) u64 {
        return self.destroyed.load(.monotonic);
    }
};

/// The set-wide used count `statfs` reports: Σ per-volume live + the root.
pub fn publishLiveInodes(per_volume_live: []const u64) u64 {
    var used: u64 = ROOT_INO;
    for (per_volume_live) |n| used +|= n;
    stats.live_inodes.set(used);
    return used;
}

// ---------------------------------------------------------------------------
// Batched destroys (§5.2 "Journal economy": ≈ 1.0 entries/op, gate ≤ 1.05).
// ---------------------------------------------------------------------------

/// Stage one corpse into the batch's ONE entry: its inode `Delete` plus one
/// `Delete` per xattr key (the keys are contiguous in the xattr kind;
/// typically just `layout`). No ino free: never reused.
pub fn stageDestroy(t: *tx.KvTx, ino: u64, xattr_keys: []const []const u8) Allocator.Error!void {
    try t.stageDelete(key.TREE_INODES, &key.inodeKey(ino));
    for (xattr_keys) |k| try t.stageDelete(key.TREE_XATTRS, k);
}

/// The journal payload bytes `stageDestroy` costs for an ino with
/// `xattr_keys` xattrs, in the admission's own framing (`tx.entryLenFor`'s
/// arithmetic) — the chunk planner's input, so a planned chunk fits ONE
/// entry by the arithmetic the commit enforces rather than a guessed
/// constant. Forest keys are one kind byte longer.
pub fn destroyEntryBytes(framing: tx.KeyFraming, xattr_keys: usize) u64 {
    const extra: usize = if (framing == .forest) 1 else 0;
    return journal.recordFrameLen(key.INODE_KEY_LEN + extra, 0) + @as(u64, xattr_keys) * journal.recordFrameLen(key.XATTR_KEY_LEN + extra, 0);
}

pub const Range = struct { start: usize, end: usize };

/// Greedy packing of destroy members into entries under the whole-entry
/// cap: `payloads[i]` is member `i`'s framed bytes. Members stay in order;
/// a chunk grows while the next member still fits `journal.entryPayloadCap`;
/// ONE member always goes — a member past the cap alone is the entry
/// write's own loud refusal, never a silent split.
pub fn packDestroyEntries(gpa: Allocator, payloads: []const u64) Allocator.Error![]Range {
    var chunks: std.ArrayList(Range) = .empty;
    errdefer chunks.deinit(gpa);
    const cap = journal.entryPayloadCap();
    var start: usize = 0;
    while (start < payloads.len) {
        var end = start + 1;
        var sum = payloads[start];
        while (end < payloads.len and sum +| payloads[end] <= cap) : (end += 1) sum += payloads[end];
        try chunks.append(gpa, .{ .start = start, .end = end });
        start = end;
    }
    return chunks.toOwnedSlice(gpa);
}

/// The unlink economy gate: `1 + 1/fill + drain ≤ 1.05` entries/op.
/// Measured 1.018/op on 100 k-op storms (fill ≈ 62, the reclaim gather's
/// shape); 1.05 leaves the drain amortization and ambient entries room
/// without admitting a second entry per op (the 2.020 the SETATTR echo
/// used to cost).
pub const UNLINK_ENTRIES_PER_OP_GATE: f64 = 1.05;

/// Entries per unlink over a storm of `ops` unlinks whose destroys ride
/// gather batches of `gather_fill` inos and whose Δtime drains commit
/// `drain_commits` entries: the op tx (1 each) + one destroy entry per
/// batch + the drains.
pub fn unlinkEntriesPerOp(ops: u64, gather_fill: u64, drain_commits: u64) f64 {
    assert(ops > 0 and gather_fill > 0);
    const destroy_entries = (ops + gather_fill - 1) / gather_fill;
    return @as(f64, @floatFromInt(ops + destroy_entries + drain_commits)) / @as(f64, @floatFromInt(ops));
}

// ---------------------------------------------------------------------------
// Lanes (`lane_core`): one law, two consumers (inos here; block-key
// incarnation stamps in the data plane).
// ---------------------------------------------------------------------------

/// The lane `value` belongs to — the ATTRIBUTION function. `writers ≤ 1`
/// is not a partition; it reads as solo.
pub fn laneOf(value: u64, base: u64, writers: u64) u64 {
    if (writers <= 1) return 0;
    return (value -% base) % writers;
}

/// The first value appender `writer` may ever mint.
pub fn firstInLane(base: u64, writers: u64, writer: u64) u64 {
    if (writers <= 1) return base;
    return base + (writer % writers);
}

/// The smallest lane-`writer` value `≥ floor` — the recovery/migration
/// rounding. Idempotent: a floor already in the lane is returned
/// unchanged, so re-seeding never advances a cursor.
pub fn nextInLaneAtOrAbove(floor: u64, base: u64, writers: u64, writer: u64) u64 {
    if (writers <= 1) return @max(floor, base);
    const first = firstInLane(base, writers, writer);
    if (floor <= first) return first;
    const ahead = laneOf(floor, base, writers);
    const want = writer % writers;
    if (ahead == want) return floor;
    // Distance forward from `floor`'s lane to ours, in the ring of lanes.
    return floor + (want + writers - ahead) % writers;
}

/// Values a lane cursor SEEDED at `start` has minted standing at `cursor`.
/// `start` is load-bearing (POSIX-1): a lane cursor is seeded from the
/// space's recovered DENSE watermark, which already counts every value
/// below it in every lane — counting from the lane's absolute first value
/// would count the seeding gap as mints and over-report `IUsed`.
pub fn mintedBetween(cursor: u64, start: u64, writers: u64) u64 {
    return (cursor -| start) / @max(writers, 1);
}

/// One appender's monotone cursor over its own lane.
pub const LaneCursor = struct {
    next: std.atomic.Value(u64),
    /// Where this cursor was seeded (raised in lockstep by `installFloor`,
    /// which is recovery, not minting) — the base `minted` counts from.
    start: std.atomic.Value(u64),
    base: u64,
    writers: u64,
    writer: u64,

    /// For appender `writer` of `writers` over the space based at `base`,
    /// seeded at the smallest lane value `≥ floor`.
    pub fn init(base: u64, writers: u64, writer: u64, floor: u64) LaneCursor {
        const w = @max(writers, 1);
        const id = writer % w;
        const seed = nextInLaneAtOrAbove(floor, base, w, id);
        return .{ .next = .init(seed), .start = .init(seed), .base = base, .writers = w, .writer = id };
    }

    /// `fetch_add(writers)`: no reuse, no free-on-failure. Solo is
    /// `fetch_add(1)` — the dense counter, instruction for instruction.
    pub fn mint(self: *LaneCursor) u64 {
        return self.next.fetchAdd(self.writers, .acq_rel);
    }

    /// The publication snapshot (what a durable watermark carries).
    pub fn snapshot(self: *const LaneCursor) u64 {
        return self.next.load(.acquire);
    }

    /// Raise to at least the smallest lane value `≥ floor`; monotone and
    /// idempotent. Re-bases the mint count too: the installed gap is not
    /// this cursor's work.
    pub fn installFloor(self: *LaneCursor, floor: u64) void {
        const want = nextInLaneAtOrAbove(floor, self.base, self.writers, self.writer);
        _ = self.next.fetchMax(want, .acq_rel);
        _ = self.start.fetchMax(want, .acq_rel);
    }

    pub fn minted(self: *const LaneCursor) u64 {
        return mintedBetween(self.snapshot(), self.startValue(), self.writers);
    }

    pub fn startValue(self: *const LaneCursor) u64 {
        return self.start.load(.acquire);
    }
};

// ---- the ino face (`ino_lane.rs`) ----

/// Which local-ino space a lane cursor belongs to.
pub const InoSpace = union(enum) {
    /// The volume's native watermark space.
    native,
    /// A hosted guest slot's space.
    guest: u16,
};

/// The appender that minted local ino `local` in a `writers`-way partition.
pub fn inoLaneOf(local: u64, writers: u16) u64 {
    return laneOf(local, LOCAL_INO_BASE, writers);
}

/// The first local ino appender `part` may ever mint.
pub fn firstInoInLane(part: AppendPartition) u64 {
    return firstInLane(LOCAL_INO_BASE, part.writers, part.writer_id);
}

/// This appender's ino-cursor floor after a mount: `max(ledger watermark,
/// replayed + 1)` rounded UP into the lane. `ledger_next_ino` is the
/// watermark of THIS appender's own newest ledger record; `replayed_max`
/// the highest local the merged replay window mentions for the space (0 if
/// none). The values the rounding skips are burned.
pub fn recoverInoFloor(ledger_next_ino: u64, replayed_max: u64, part: AppendPartition) u64 {
    return nextInLaneAtOrAbove(recoverNextIno(ledger_next_ino, replayed_max), LOCAL_INO_BASE, part.writers, part.writer_id);
}

/// A fresh lane cursor for `part`, seeded from a dense floor.
pub fn laneCursor(part: AppendPartition, floor: u64) LaneCursor {
    return LaneCursor.init(LOCAL_INO_BASE, part.writers, part.writer_id, @max(floor, LOCAL_INO_BASE));
}

/// Inos `part` has minted with a cursor seeded at `start` now at `cursor` —
/// the exact count `statfs` needs. Solo collapses to `cursor − start`.
pub fn mintedInInoLane(cursor: u64, start: u64, part: AppendPartition) u64 {
    return mintedBetween(cursor, start, part.writers);
}

// ---------------------------------------------------------------------------
// Global ino routing (charter §5.4).
// ---------------------------------------------------------------------------

/// `W = 2^16`: DERIVED from the u16 slot-id wire type (the guest keyspace
/// encoding, the membership stamp and the cutover map all carry slot ids as
/// u16), frozen forever — W is baked into every global ino.
pub const ROUTING_WIDTH: u64 = geometry.routing_width;

pub const Routed = struct { slot: u64, local: u64 };

/// Global ino → `(slot, local)` over the frozen width: `slot = (ino−2) % W`,
/// `local = (ino−2)/W + 2`; ino 1 → slot 0 (the root pin). `W ≤ 1` is the
/// exact identity `(0, ino)` of a single-volume set (the general arithmetic
/// coincides: `(ino−2)/1 + 2 = ino`).
pub fn routeIno(ino: u64, width: u64) Routed {
    if (width <= 1) return .{ .slot = 0, .local = ino };
    if (ino == ROOT_INO) return .{ .slot = 0, .local = ROOT_INO };
    assert(ino >= LOCAL_INO_BASE);
    return .{ .slot = (ino - LOCAL_INO_BASE) % width, .local = (ino - LOCAL_INO_BASE) / width + LOCAL_INO_BASE };
}

/// `routeIno`'s exact inverse: `ino = (local−2)·W + slot + 2`; local 1 on
/// slot 0 is the root pin; identity for `W ≤ 1`.
pub fn makeGlobalIno(local: u64, slot: u64, width: u64) u64 {
    if (width <= 1) return local;
    if (local == ROOT_INO and slot == 0) return ROOT_INO;
    assert(local >= LOCAL_INO_BASE and slot < width);
    return (local - LOCAL_INO_BASE) * width + slot + LOCAL_INO_BASE;
}

// ---------------------------------------------------------------------------
// Tests — `mw_ino_lane_tests.rs`'s pure contracts, the DUR-8c fold, the
// destroy economy, the §5.4 ties.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testPart(writers: u16, writer: u16) AppendPartition {
    return AppendPartition.new(writers, writer) catch unreachable;
}

test "the watermark mints one fetch_add from max(seed, 2), never below 2, snapshots dominate every mint, and a stale floor never regresses" {
    var w = InoWatermark.init(0);
    try testing.expectEqual(@as(u64, 2), w.snapshot());
    try testing.expectEqual(@as(u64, 2), w.allocate());
    try testing.expectEqual(@as(u64, 3), w.allocate());
    try testing.expectEqual(@as(u64, 4), w.reserveRange(10));
    try testing.expectEqual(@as(u64, 14), w.snapshot());
    w.installFloor(5);
    try testing.expectEqual(@as(u64, 14), w.snapshot());
    w.installFloor(100);
    try testing.expectEqual(@as(u64, 100), w.allocate());
    var seeded = InoWatermark.init(4096);
    try testing.expectEqual(@as(u64, 4096), seeded.allocate());
}

test "recovery: next_ino = max(ledger, replayed max + 1); an empty window leaves the ledger's watermark" {
    try testing.expectEqual(@as(u64, 1000), recoverNextIno(1000, 0));
    try testing.expectEqual(@as(u64, 1000), recoverNextIno(1000, 998));
    try testing.expectEqual(@as(u64, 1000), recoverNextIno(1000, 999));
    try testing.expectEqual(@as(u64, 1001), recoverNextIno(1000, 1000));
    try testing.expectEqual(@as(u64, 5001), recoverNextIno(2, 5000));
    try testing.expectEqual(std.math.maxInt(u64), recoverNextIno(0, std.math.maxInt(u64)));
}

fn testFrame(tree_id: u8, level: u8, k: []const u8, kind: journal.RecordKind, v: []const u8) journal.RecordFrame {
    return .{ .tag = journal.tagFor(tree_id, level), .key = k, .kind = kind, .seq = 0, .value = v };
}

test "DUR-8c fold: inode keys, dentry child inos and xattr keys all raise the watermark; deletes without a value, interior records and other kinds mention nothing; guest inos keep per-slot maxima" {
    var fold: RecoveryFold = .{};
    defer fold.deinit(testing.allocator);
    const gpa = testing.allocator;
    const iv = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_INODES, 0, &key.inodeKey(40), .put, &iv));
    try testing.expectEqual(@as(u64, 40), fold.native_max);
    // A dentry whose child outlives its torn-dropped inode record.
    var dbuf: [64]u8 = undefined;
    const dv = try (record.DentryValue{ .child_ino = 77, .file_type = 8, .name = "survivor" }).encodeInto(&dbuf);
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 5, 0), .put, dv));
    try testing.expectEqual(@as(u64, 77), fold.native_max);
    // An xattr naming a yet-higher ino (the crash leg's shape).
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(90, 3, 0), .put, "\x04userv"));
    try testing.expectEqual(@as(u64, 90), fold.native_max);
    // A dentry Delete carries no value: mentions nothing.
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 6, 0), .delete, &.{}));
    // An inode Delete DOES mention its key's ino (never re-mint a corpse).
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_INODES, 0, &key.inodeKey(95), .delete, &.{}));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    // Interior records and non-slot kinds: nothing.
    try fold.noteRecord(gpa, .flat, testFrame(key.KIND_INTERIOR, 1, &key.inodeKey(5000), .put, &key.encodeInteriorValue(1, 2)));
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_INODES, 2, &key.inodeKey(6000), .put, &iv));
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_CONTROL, 0, "slot_state:9", .put, "x"));
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_BLOCK_REFS, 0, &key.blockRefKey(1, 2, 7000, 0), .put, &.{ 1, 0, 0, 0 }));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    // Guest keyspaces: raw locals per slot, never the native watermark.
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_INODES, 0, &key.inodeKey(key.guestLocalIno(7, 300)), .put, &iv));
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_INODES, 0, &key.inodeKey(key.guestLocalIno(7, 120)), .put, &iv));
    try fold.noteRecord(gpa, .flat, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(key.guestLocalIno(9, 8), 1, 0), .put, "\x01av"));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    try testing.expectEqual(@as(u64, 301), fold.guestCursor(7, 0));
    try testing.expectEqual(@as(u64, 500), fold.guestCursor(7, 500));
    try testing.expectEqual(@as(u64, 9), fold.guestCursor(9, 0));
    try testing.expectEqual(LOCAL_INO_BASE, fold.guestCursor(11, 0));
    try testing.expectEqual(@as(u64, 96), fold.nextIno(10));
    try testing.expectEqual(@as(u64, 500), fold.nextIno(500));
}

test "DUR-8c fold on a forest volume: the kind byte is split off and must agree with the tag; a disagreeing pair is a counted violation that refuses" {
    var fold: RecoveryFold = .{};
    defer fold.deinit(testing.allocator);
    const gpa = testing.allocator;
    const fk = try key.forestKey(key.TREE_INODES, &key.inodeKey(1234));
    try fold.noteRecord(gpa, .forest, testFrame(key.TREE_INODES, 0, fk.slice(), .delete, &.{}));
    try testing.expectEqual(@as(u64, 1234), fold.native_max);
    const xk = try key.forestKey(key.TREE_XATTRS, &key.xattrKey(key.guestLocalIno(2, 55), 9, 1));
    try fold.noteRecord(gpa, .forest, testFrame(key.TREE_XATTRS, 0, xk.slice(), .put, "\x01av"));
    try testing.expectEqual(@as(u64, 56), fold.guestCursor(2, 0));
    const before = tx.stats.meta_kv_forest_key_violations.load();
    try testing.expectError(error.Corrupt, fold.noteRecord(gpa, .forest, testFrame(key.TREE_DENTRIES, 0, fk.slice(), .put, "x")));
    try testing.expectError(error.Corrupt, fold.noteRecord(gpa, .forest, testFrame(key.TREE_INODES, 0, &key.inodeKey(5), .put, "x")));
    try testing.expectEqual(before + 2, tx.stats.meta_kv_forest_key_violations.load());
}

test "the fold over a replayed window (entries end to end) equals the per-record fold, through the journal's own frames" {
    const gpa = testing.allocator;
    const iv = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();
    var dbuf: [64]u8 = undefined;
    const dv = try (record.DentryValue{ .child_ino = 2001, .file_type = 8, .name = "n" }).encodeInto(&dbuf);
    const frames = [_]journal.RecordFrame{
        testFrame(key.TREE_INODES, 0, &key.inodeKey(1500), .put, &iv),
        testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 1, 0), .put, dv),
        testFrame(key.TREE_XATTRS, 0, &key.xattrKey(1700, 1, 0), .put, "\x01av"),
    };
    const payload = try gpa.alloc(u8, @intCast(journal.payloadLenFor(&frames)));
    defer gpa.free(payload);
    journal.encodeEntryPayload(payload, &frames);
    _ = try journal.validateEntryPayload(payload);
    const entries = [_]journal.ReplayedEntry{.{ .seq = 0, .payload = payload }};
    var fold: RecoveryFold = .{};
    defer fold.deinit(gpa);
    try fold.foldEntries(gpa, .flat, &entries);
    try testing.expectEqual(@as(u64, 2001), fold.native_max);
    try testing.expectEqual(@as(u64, 2002), fold.nextIno(100));
}

test "live inodes: every cursor's progression counts from its reserved base 2, lanes count exactly their mints, destroys subtract, never below 0; statfs adds the root once" {
    try testing.expectEqual(@as(u64, 0), liveInodes(2, &.{}, &.{}, 0));
    try testing.expectEqual(@as(u64, 5), liveInodes(7, &.{}, &.{}, 0));
    try testing.expectEqual(@as(u64, 5 + 3 + 10), liveInodes(7, &.{ 5, 12 }, &.{}, 0));
    try testing.expectEqual(@as(u64, 5 + 3 + 10 + 4), liveInodes(7, &.{ 5, 12 }, &.{ 1, 3 }, 0));
    try testing.expectEqual(@as(u64, 2), liveInodes(7, &.{}, &.{}, 3));
    try testing.expectEqual(@as(u64, 0), liveInodes(7, &.{}, &.{}, 50));
    var tally: DestroyTally = .{};
    const g0 = stats.destroyed_inodes.load();
    tally.noteCommitted(3);
    tally.noteCommitted(2);
    try testing.expectEqual(@as(u64, 5), tally.load());
    try testing.expectEqual(g0 + 5, stats.destroyed_inodes.load());
    try testing.expectEqual(@as(u64, 1 + 10 + 20), publishLiveInodes(&.{ 10, 20 }));
    try testing.expectEqual(@as(u64, 31), stats.live_inodes.load());
}

// ---- mw_ino_lane_tests ----

test "lanes_are_disjoint_monotone_and_attributable: four appenders minting from their own lanes produce no duplicate, every ino names its minter, each sequence is strictly monotone" {
    const W: u16 = 4;
    const N: u64 = 1_000;
    var seen: std.AutoHashMapUnmanaged(u64, void) = .empty;
    defer seen.deinit(testing.allocator);
    var writer: u16 = 0;
    while (writer < W) : (writer += 1) {
        const p = testPart(W, writer);
        var cursor = LaneCursor.init(LOCAL_INO_BASE, W, writer, LOCAL_INO_BASE);
        var prev: u64 = 0;
        var i: u64 = 0;
        while (i < N) : (i += 1) {
            const ino = cursor.mint();
            try testing.expect(ino > prev);
            prev = ino;
            try testing.expectEqual(@as(u64, writer), inoLaneOf(ino, W));
            const gop = try seen.getOrPut(testing.allocator, ino);
            try testing.expect(!gop.found_existing);
        }
        try testing.expectEqual(LOCAL_INO_BASE + writer, firstInoInLane(p));
        try testing.expectEqual(N, mintedInInoLane(cursor.snapshot(), cursor.startValue(), p));
        try testing.expectEqual(N, cursor.minted());
    }
    try testing.expectEqual(N * W, seen.count());
}

test "solo_lane_cursor_ties_the_shipped_cursor: solo is arithmetically identical to the dense watermark from any floor (drift is red)" {
    for ([_]u64{ 2, 3, 17, 4096, 1 << 20 }) |floor| {
        var lane = LaneCursor.init(LOCAL_INO_BASE, 1, 0, floor);
        var dense = InoWatermark.init(floor);
        var expect = @max(floor, LOCAL_INO_BASE);
        var i: usize = 0;
        while (i < 64) : (i += 1) {
            const l = lane.mint();
            const d = dense.allocate();
            try testing.expectEqual(d, l);
            try testing.expectEqual(expect, l);
            expect += 1;
        }
        try testing.expectEqual(lane.snapshot() - LOCAL_INO_BASE, mintedInInoLane(lane.snapshot(), LOCAL_INO_BASE, AppendPartition.solo));
    }
}

test "recovery_rounds_into_the_lane_and_never_re_mints: the recovered floor is in the lane, dominates every committed ino, is idempotent, and a stale install never regresses" {
    const W: u16 = 4;
    var writer: u16 = 0;
    while (writer < W) : (writer += 1) {
        const p = testPart(W, writer);
        var cursor = LaneCursor.init(LOCAL_INO_BASE, W, writer, LOCAL_INO_BASE);
        var committed: [37]u64 = undefined;
        for (&committed) |*c| c.* = cursor.mint();
        const max = std.mem.max(u64, &committed);
        const floor = recoverInoFloor(max + 1, max, p);
        try testing.expectEqual(@as(u64, writer), inoLaneOf(floor, W));
        for (committed) |c| try testing.expect(floor > c);
        try testing.expectEqual(floor, recoverInoFloor(floor, 0, p));
        var fresh = LaneCursor.init(LOCAL_INO_BASE, W, writer, floor);
        const ahead = fresh.mint();
        fresh.installFloor(LOCAL_INO_BASE);
        try testing.expect(fresh.mint() > ahead);
        // The install re-bases the mint count: the gap is not minting.
        var seeded = LaneCursor.init(LOCAL_INO_BASE, W, writer, LOCAL_INO_BASE);
        seeded.installFloor(1000);
        try testing.expectEqual(@as(u64, 0), seeded.minted());
        _ = seeded.mint();
        try testing.expectEqual(@as(u64, 1), seeded.minted());
    }
}

test "laned_locals_keep_global_ino_stability: sparse laned locals round-trip through the frozen width and every global ino is unique" {
    const W: u16 = 4;
    var globals: std.AutoHashMapUnmanaged(u64, void) = .empty;
    defer globals.deinit(testing.allocator);
    var writer: u16 = 0;
    while (writer < W) : (writer += 1) {
        var cursor = LaneCursor.init(LOCAL_INO_BASE, W, writer, LOCAL_INO_BASE);
        for ([_]u64{ 0, 1, 63, 65_535 }) |slot| {
            var i: usize = 0;
            while (i < 8) : (i += 1) {
                const local = cursor.mint();
                const global = makeGlobalIno(local, slot, ROUTING_WIDTH);
                const back = routeIno(global, ROUTING_WIDTH);
                try testing.expectEqual(slot, back.slot);
                try testing.expectEqual(local, back.local);
                const gop = try globals.getOrPut(testing.allocator, global);
                try testing.expect(!gop.found_existing);
            }
        }
    }
}

test "a_crash_and_remount_never_re_mints_a_lane_ino: records that only MENTION laned inos (xattr keys) feed the fold; the recovered floor rounds into the lane above every committed ino" {
    const gpa = testing.allocator;
    const p = testPart(4, 2);
    var committed: [6]u64 = undefined;
    var fold: RecoveryFold = .{};
    defer fold.deinit(gpa);
    {
        var cursor = laneCursor(p, LOCAL_INO_BASE);
        for (&committed) |*c| {
            c.* = cursor.mint();
            // The crash skips the final checkpoint: recovery comes from the
            // window, where only an xattr names the ino (no inode record).
            try fold.noteRecord(gpa, .flat, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(c.*, 0x55, 0), .put, "\x09user.lane1"));
        }
    }
    // Remount: the ledger's watermark is stale (the pre-crash record).
    const next_ino = fold.nextIno(LOCAL_INO_BASE);
    try testing.expectEqual(std.mem.max(u64, &committed) + 1, next_ino);
    const floor = recoverInoFloor(next_ino, 0, p);
    for (committed) |c| try testing.expect(floor > c);
    var cursor = laneCursor(p, LOCAL_INO_BASE);
    cursor.installFloor(next_ino);
    const next = cursor.mint();
    for (committed) |c| try testing.expect(next > c);
    try testing.expectEqual(@as(u64, 2), inoLaneOf(next, 4));
}

test "live_inode_count_is_exact_under_lanes: dense, lane and guest-lane mints each count exactly once" {
    var dense = InoWatermark.init(LOCAL_INO_BASE);
    var minted: u64 = 0;
    var i: usize = 0;
    while (i < 5) : (i += 1) {
        _ = dense.allocate();
        minted += 1;
    }
    var lanes: [4]LaneCursor = undefined;
    var guest_lanes: [4]LaneCursor = undefined;
    var lane_minted: [8]u64 = undefined;
    for (&lanes, &guest_lanes, 0..) |*l, *g, w| {
        const p = testPart(4, @intCast(w));
        l.* = laneCursor(p, dense.snapshot());
        g.* = laneCursor(p, LOCAL_INO_BASE);
        var j: usize = 0;
        while (j < 7) : (j += 1) {
            _ = l.mint();
            minted += 1;
        }
        j = 0;
        while (j < 3) : (j += 1) {
            _ = g.mint();
            minted += 1;
        }
        lane_minted[w] = l.minted();
        lane_minted[4 + w] = g.minted();
    }
    try testing.expectEqual(minted, liveInodes(dense.snapshot(), &.{}, &lane_minted, 0));
    // A dense count of a strided cursor would over-report by `writers`.
    try testing.expect(lanes[0].snapshot() - lanes[0].startValue() > lanes[0].minted());
}

test "lane arithmetic: solo reads lane 0 / base; rounding forward wraps the ring of lanes; minted_between divides by writers" {
    try testing.expectEqual(@as(u64, 0), laneOf(12345, 2, 1));
    try testing.expectEqual(@as(u64, 0), laneOf(12345, 2, 0));
    try testing.expectEqual(@as(u64, 3), laneOf(9, 2, 4));
    try testing.expectEqual(@as(u64, 2), firstInLane(2, 1, 5));
    try testing.expectEqual(@as(u64, 3), firstInLane(2, 4, 5));
    try testing.expectEqual(@as(u64, 7), nextInLaneAtOrAbove(7, 2, 1, 0));
    try testing.expectEqual(@as(u64, 2), nextInLaneAtOrAbove(0, 2, 1, 0));
    try testing.expectEqual(@as(u64, 5), nextInLaneAtOrAbove(0, 2, 4, 3));
    try testing.expectEqual(@as(u64, 9), nextInLaneAtOrAbove(9, 2, 4, 3));
    try testing.expectEqual(@as(u64, 13), nextInLaneAtOrAbove(10, 2, 4, 3));
    try testing.expectEqual(@as(u64, 10), nextInLaneAtOrAbove(10, 2, 4, 0));
    try testing.expectEqual(@as(u64, 11), nextInLaneAtOrAbove(10, 2, 4, 1));
    try testing.expectEqual(@as(u64, 25), mintedBetween(102, 2, 4));
    try testing.expectEqual(@as(u64, 100), mintedBetween(102, 2, 1));
    try testing.expectEqual(@as(u64, 0), mintedBetween(2, 102, 4));
}

// ---- §5.4 ties ----

test "W = 2^16 is the u16 slot-id wire type, frozen; slot/local/global formulas tie charter §5.4; ino 1 pins slot 0; W ≤ 1 is the identity" {
    try testing.expectEqual(@as(u64, std.math.maxInt(u16)) + 1, ROUTING_WIDTH);
    try testing.expectEqual(@as(u64, 65_536), ROUTING_WIDTH);
    try testing.expectEqual(@as(u64, geometry.routing_width), ROUTING_WIDTH);
    try testing.expectEqual(@as(u64, key.FOREST_SLOT_MAX), ROUTING_WIDTH);
    try testing.expectEqual(Routed{ .slot = 0, .local = 1 }, routeIno(1, ROUTING_WIDTH));
    try testing.expectEqual(Routed{ .slot = 0, .local = 2 }, routeIno(2, ROUTING_WIDTH));
    try testing.expectEqual(Routed{ .slot = 1, .local = 2 }, routeIno(3, ROUTING_WIDTH));
    try testing.expectEqual(Routed{ .slot = 0, .local = 3 }, routeIno(2 + ROUTING_WIDTH, ROUTING_WIDTH));
    try testing.expectEqual(@as(u64, 1), makeGlobalIno(1, 0, ROUTING_WIDTH));
    try testing.expectEqual(@as(u64, 3), makeGlobalIno(2, 1, ROUTING_WIDTH));
    try testing.expectEqual(2 + ROUTING_WIDTH, makeGlobalIno(3, 0, ROUTING_WIDTH));
    // Inverse property over a spread of inos, including the top of u48.
    var prng = std.Random.DefaultPrng.init(0x5EED);
    const rnd = prng.random();
    var i: usize = 0;
    while (i < 10_000) : (i += 1) {
        const ino = rnd.intRangeAtMost(u64, 1, 1 << 48);
        const r = routeIno(ino, ROUTING_WIDTH);
        try testing.expect(r.slot < ROUTING_WIDTH);
        try testing.expectEqual(ino, makeGlobalIno(r.local, r.slot, ROUTING_WIDTH));
        if (ino != 1) try testing.expectEqual((ino - 2) % ROUTING_WIDTH, r.slot);
    }
    // W ≤ 1: identity (the pre-routing single-volume st_ino stability).
    try testing.expectEqual(Routed{ .slot = 0, .local = 777 }, routeIno(777, 1));
    try testing.expectEqual(@as(u64, 777), makeGlobalIno(777, 0, 1));
    try testing.expectEqual(@as(u64, 777), makeGlobalIno(777, 0, 0));
}

// ---- the destroy economy ----

test "destroy staging is one entry of inode + xattr Deletes priced by the admission's own framing (flat and forest)" {
    var t = tx.KvTx.init(testing.allocator, @src());
    const xk1 = key.xattrKey(500, 1, 0);
    const xk2 = key.xattrKey(500, 2, 0);
    try stageDestroy(&t, 500, &.{ &xk1, &xk2 });
    try stageDestroy(&t, 501, &.{});
    try testing.expectEqual(@as(usize, 4), t.staged.items.len);
    for (t.staged.items) |s| try testing.expectEqual(record.RecordKind.delete, s.kind);
    var q = try tx.buildQueuedTx(&t, .flat, 256 * 1024, null);
    defer q.deinit();
    try testing.expectEqual(journal.ENTRY_HDR_LEN + destroyEntryBytes(.flat, 2) + destroyEntryBytes(.flat, 0), q.len);
    try testing.expectEqual(@as(u64, 1 + 15 + 8), destroyEntryBytes(.flat, 0));
    try testing.expectEqual(@as(u64, 1 + 15 + 9 + 2 * (1 + 15 + 17)), destroyEntryBytes(.forest, 2));

    var f = tx.KvTx.init(testing.allocator, @src());
    try stageDestroy(&f, 500, &.{&xk1});
    var fq = try tx.buildQueuedTx(&f, .forest, 256 * 1024, null);
    defer fq.deinit();
    try testing.expectEqual(journal.ENTRY_HDR_LEN + destroyEntryBytes(.forest, 1), fq.len);
}

test "destroy chunks pack greedily under the payload cap, in order, one member always goes" {
    const gpa = testing.allocator;
    const cap = journal.entryPayloadCap();
    const payloads = [_]u64{ 100, cap - 100, 1, cap, cap + 5, 10, 20 };
    const chunks = try packDestroyEntries(gpa, &payloads);
    defer gpa.free(chunks);
    try testing.expectEqualSlices(Range, &.{
        .{ .start = 0, .end = 2 },
        .{ .start = 2, .end = 3 },
        .{ .start = 3, .end = 4 },
        .{ .start = 4, .end = 5 },
        .{ .start = 5, .end = 7 },
    }, chunks);
    const none = try packDestroyEntries(gpa, &.{});
    defer gpa.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
    // A realistic batch — 64 corpses with one `layout` xattr each — is ONE
    // entry on either layout.
    var batch: [64]u64 = @splat(destroyEntryBytes(.forest, 1));
    const one = try packDestroyEntries(gpa, &batch);
    defer gpa.free(one);
    try testing.expectEqual(@as(usize, 1), one.len);
}

test "unlink economy tie: 1 + 1/fill + drain ≤ 1.05; the measured 1.018/op at fill 62 with the drain's amortization; fill 1 (no gather) breaks the gate" {
    try testing.expectEqual(@as(f64, 1.05), UNLINK_ENTRIES_PER_OP_GATE);
    const ops: u64 = 100_000;
    // fill ≈ 62 (measured gather fill), 200 drain commits over the storm.
    const measured = unlinkEntriesPerOp(ops, 62, 200);
    try testing.expect(measured > 1.017 and measured < 1.019);
    try testing.expect(measured <= UNLINK_ENTRIES_PER_OP_GATE);
    // The gather cap (64) alone: 1 + 1/64.
    try testing.expect(@abs(unlinkEntriesPerOp(64_000, 64, 0) - (1.0 + 1.0 / 64.0)) < 1e-9);
    // Fill 20 sits exactly AT the gate with no room for a drain; 19 breaks
    // it — the gather's fill is what the economy rides on.
    try testing.expect(unlinkEntriesPerOp(ops, 21, 0) < UNLINK_ENTRIES_PER_OP_GATE);
    try testing.expect(@abs(unlinkEntriesPerOp(ops, 20, 0) - UNLINK_ENTRIES_PER_OP_GATE) < 1e-9);
    try testing.expect(unlinkEntriesPerOp(ops, 19, 0) > UNLINK_ENTRIES_PER_OP_GATE);
    // No gather (one destroy entry per unlink): 2.0 — the pre-M6 shape.
    try testing.expectEqual(@as(f64, 2.0), unlinkEntriesPerOp(ops, 1, 0));
    // A synthetic storm: 1000 unlinks, destroys gathered 62 at a time,
    // 2 drains.
    const storm = unlinkEntriesPerOp(1000, 62, 2);
    try testing.expect(storm <= UNLINK_ENTRIES_PER_OP_GATE);
}
