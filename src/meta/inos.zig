//! Monotonic inos (charter §5.3 "Inodes": "monotonic `next_ino` watermark,
//! never reused, recovered as `max(ledger, replayed window max) + 1`; ino 1
//! = root"; §5.4 the slot ↔ ino arithmetic; §5.2 "Journal economy": batched
//! `destroy_inodes`; survey `docs/porting/kv_tree_cache_commit_survey.md`
//! §2.14, §2.16, §7 row 15). PORTABLE: atomics and arithmetic; the open path
//! feeds it the replayed window, the committer the mint, `statfs` the
//! derivation.
//!
//! Two laws share this module:
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
//!
//! Every appender mints in its OWN slot's keyspace (`key.guestLocalIno`;
//! charter §5.5, §5.9 "cursor = max(recorded, page, window max + 1)" per
//! slot), so one cursor per slot is the whole partition — there is no
//! residue-class lane over a shared space (decision 0003 Q3).

const std = @import("std");
const Allocator = std.mem.Allocator;
const counters = @import("../core/counters.zig");
const key = @import("key.zig");
const record = @import("record.zig");
const journal = @import("journal.zig");
const geometry = @import("geometry.zig");
const tx = @import("tx.zig");

const assert = std.debug.assert;

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
    /// The slot whose locals are this volume's native keyspace (the
    /// membership stamp's `native_slot`; null on an all-guest member).
    native_slot: ?u16 = 0,
    /// Native locals (`< 2^40`): the watermark's input.
    native_max: u64 = 0,
    /// Per guest slot: the RAW local maxima (each slot is its own cursor).
    guest_max: std.AutoHashMapUnmanaged(u16, u64) = .empty,

    pub fn deinit(self: *RecoveryFold, gpa: Allocator) void {
        self.guest_max.deinit(gpa);
        self.* = undefined;
    }

    /// Fold ONE replayed record frame. Interior records (level > 0) and
    /// non-slot-tree kinds mention no ino. A slot-tree key carries its kind
    /// byte, which must agree with the tag (a disagreeing pair is counted as
    /// a forest-key violation and refuses the mount). A value that does not
    /// decode (a dentry `Delete` has none) mentions nothing — the surviving
    /// dentry's PUT is what names the child. Keys mention KEY inos (native
    /// locals or `guestLocalIno` forms); a dentry value names the GLOBAL
    /// child ino (charter §5.4), routed back to `(slot, local)` first.
    pub fn noteRecord(self: *RecoveryFold, gpa: Allocator, frame: journal.RecordFrame) (Allocator.Error || error{Corrupt})!void {
        const t = journal.untag(frame.tag);
        if (t.level > 0) return;
        if (!key.isSlotTreeKind(t.tree_id)) return;
        _ = key.checkSlotTreeKey(t.tree_id, frame.key) catch {
            tx.stats.meta_kv_forest_key_violations.inc();
            return error.Corrupt;
        };
        switch (t.tree_id) {
            key.TREE_INODES => try self.noteKeyIno(gpa, key.decodeInodeKey(frame.key) catch return),
            key.TREE_XATTRS => try self.noteKeyIno(gpa, (key.XattrKey.decode(frame.key) catch return).ino()),
            key.TREE_DENTRIES => {
                const d = record.DentryValue.decode(frame.value) catch return;
                const r = routeIno(d.child_ino, ROUTING_WIDTH);
                if (self.native_slot != null and self.native_slot.? == r.slot) {
                    self.native_max = @max(self.native_max, r.local);
                } else {
                    try self.noteGuest(gpa, @intCast(r.slot), r.local);
                }
            },
            else => {},
        }
    }

    fn noteKeyIno(self: *RecoveryFold, gpa: Allocator, ino: u64) Allocator.Error!void {
        if (key.splitGuestLocal(ino)) |g| {
            try self.noteGuest(gpa, g.slot, g.local);
        } else {
            self.native_max = @max(self.native_max, ino);
        }
    }

    fn noteGuest(self: *RecoveryFold, gpa: Allocator, slot: u16, local: u64) Allocator.Error!void {
        const e = try self.guest_max.getOrPutValue(gpa, slot, 0);
        e.value_ptr.* = @max(e.value_ptr.*, local);
    }

    /// Fold every content record of the replayed entries (seq order).
    pub fn foldEntries(self: *RecoveryFold, gpa: Allocator, entries: []const journal.ReplayedEntry) (Allocator.Error || error{Corrupt})!void {
        for (entries) |e| {
            var it = e.records();
            while (it.next()) |frame| try self.noteRecord(gpa, frame);
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
/// ONCE by the caller across the set) — minus the destroys committed on the
/// volume. An over-report, never an under-report: `statfs` may only be
/// pessimistic about free slots, and burned inos count as the §4.8 law
/// describes.
pub fn liveInodes(native_next: u64, guest_cursors: []const u64, destroyed: u64) u64 {
    var allocated: u64 = native_next -| LOCAL_INO_BASE;
    for (guest_cursors) |c| allocated +|= c -| LOCAL_INO_BASE;
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
/// `xattr_keys` xattrs (`tx.entryLenFor`'s arithmetic) — the chunk
/// planner's input, so a planned chunk fits ONE entry by the arithmetic the
/// commit enforces rather than a guessed constant.
pub fn destroyEntryBytes(xattr_keys: usize) u64 {
    return journal.recordFrameLen(key.INODE_KEY_LEN, 0) + @as(u64, xattr_keys) * journal.recordFrameLen(key.XATTR_KEY_LEN, 0);
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
// Tests — the watermark, the DUR-8c fold, the destroy economy, the §5.4
// ties.
// ---------------------------------------------------------------------------

const testing = std.testing;

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
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &key.inodeKey(40), .put, &iv));
    try testing.expectEqual(@as(u64, 40), fold.native_max);
    // A dentry whose child outlives its torn-dropped inode record: the
    // value names the GLOBAL ino; the fold routes it back to local 77.
    var dbuf: [64]u8 = undefined;
    const dv = try (record.DentryValue{ .child_ino = makeGlobalIno(77, 0, ROUTING_WIDTH), .file_type = 8, .name = "survivor" }).encodeInto(&dbuf);
    try testing.expect(makeGlobalIno(77, 0, ROUTING_WIDTH) > 1 << 22);
    try fold.noteRecord(gpa, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 5, 0), .put, dv));
    try testing.expectEqual(@as(u64, 77), fold.native_max);
    // An xattr naming a yet-higher ino (the crash leg's shape).
    try fold.noteRecord(gpa, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(90, 3, 0), .put, "\x04userv"));
    try testing.expectEqual(@as(u64, 90), fold.native_max);
    // A dentry Delete carries no value: mentions nothing.
    try fold.noteRecord(gpa, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 6, 0), .delete, &.{}));
    // An inode Delete DOES mention its key's ino (never re-mint a corpse).
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &key.inodeKey(95), .delete, &.{}));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    // Interior records and non-slot kinds: nothing.
    try fold.noteRecord(gpa, testFrame(key.KIND_INTERIOR, 1, &key.inodeKey(5000), .put, &key.encodeInteriorValue(1, 2)));
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 2, &key.inodeKey(6000), .put, &iv));
    try fold.noteRecord(gpa, testFrame(key.TREE_CONTROL, 0, "slot_state:9", .put, "x"));
    try fold.noteRecord(gpa, testFrame(key.TREE_BLOCK_REFS, 0, &key.blockRefKey(1, 2, 7000, 0), .put, &.{ 1, 0, 0, 0 }));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    // Guest keyspaces: raw locals per slot, never the native watermark.
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &key.inodeKey(key.guestLocalIno(7, 300)), .put, &iv));
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &key.inodeKey(key.guestLocalIno(7, 120)), .put, &iv));
    try fold.noteRecord(gpa, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(key.guestLocalIno(9, 8), 1, 0), .put, "\x01av"));
    // A dentry naming a child of slot 7 (global form) raises slot 7's cursor.
    var gbuf: [64]u8 = undefined;
    const gdv = try (record.DentryValue{ .child_ino = makeGlobalIno(400, 7, ROUTING_WIDTH), .file_type = 8, .name = "g" }).encodeInto(&gbuf);
    try fold.noteRecord(gpa, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 9, 0), .put, gdv));
    try testing.expectEqual(@as(u64, 95), fold.native_max);
    try testing.expectEqual(@as(u64, 401), fold.guestCursor(7, 0));
    try testing.expectEqual(@as(u64, 500), fold.guestCursor(7, 500));
    // An all-guest member routes every dentry child to a guest cursor.
    var all_guest: RecoveryFold = .{ .native_slot = null };
    defer all_guest.deinit(gpa);
    try all_guest.noteRecord(gpa, testFrame(key.TREE_DENTRIES, 0, &key.dentryKey(1, 5, 0), .put, dv));
    try testing.expectEqual(@as(u64, 0), all_guest.native_max);
    try testing.expectEqual(@as(u64, 78), all_guest.guestCursor(0, 0));
    try testing.expectEqual(@as(u64, 9), fold.guestCursor(9, 0));
    try testing.expectEqual(LOCAL_INO_BASE, fold.guestCursor(11, 0));
    try testing.expectEqual(@as(u64, 96), fold.nextIno(10));
    try testing.expectEqual(@as(u64, 500), fold.nextIno(500));
}

test "DUR-8c fold: a slot-tree key's kind byte must agree with the tag; a disagreeing or malformed pair is a counted violation that refuses" {
    var fold: RecoveryFold = .{};
    defer fold.deinit(testing.allocator);
    const gpa = testing.allocator;
    try fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &key.inodeKey(1234), .delete, &.{}));
    try testing.expectEqual(@as(u64, 1234), fold.native_max);
    try fold.noteRecord(gpa, testFrame(key.TREE_XATTRS, 0, &key.xattrKey(key.guestLocalIno(2, 55), 9, 1), .put, "\x01av"));
    try testing.expectEqual(@as(u64, 56), fold.guestCursor(2, 0));
    const before = tx.stats.meta_kv_forest_key_violations.load();
    // An inode key tagged as a dentry: the kind byte disagrees.
    try testing.expectError(error.Corrupt, fold.noteRecord(gpa, testFrame(key.TREE_DENTRIES, 0, &key.inodeKey(1234), .put, "x")));
    // A kindless 8-byte key under the inode tag: malformed for its kind.
    var kindless: [8]u8 = undefined;
    std.mem.writeInt(u64, &kindless, 5, .big);
    try testing.expectError(error.Corrupt, fold.noteRecord(gpa, testFrame(key.TREE_INODES, 0, &kindless, .put, "x")));
    try testing.expectEqual(before + 2, tx.stats.meta_kv_forest_key_violations.load());
}

test "the fold over a replayed window (entries end to end) equals the per-record fold, through the journal's own frames" {
    const gpa = testing.allocator;
    const iv = (record.InodeValue{ .mode = 0o100644, .nlink = 1 }).encode();
    var dbuf: [64]u8 = undefined;
    const dv = try (record.DentryValue{ .child_ino = makeGlobalIno(2001, 0, ROUTING_WIDTH), .file_type = 8, .name = "n" }).encodeInto(&dbuf);
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
    try fold.foldEntries(gpa, &entries);
    try testing.expectEqual(@as(u64, 2001), fold.native_max);
    try testing.expectEqual(@as(u64, 2002), fold.nextIno(100));
}

test "live inodes: every cursor's progression counts from its reserved base 2, destroys subtract, never below 0; statfs adds the root once" {
    try testing.expectEqual(@as(u64, 0), liveInodes(2, &.{}, 0));
    try testing.expectEqual(@as(u64, 5), liveInodes(7, &.{}, 0));
    try testing.expectEqual(@as(u64, 5 + 3 + 10), liveInodes(7, &.{ 5, 12 }, 0));
    try testing.expectEqual(@as(u64, 2), liveInodes(7, &.{}, 3));
    try testing.expectEqual(@as(u64, 0), liveInodes(7, &.{}, 50));
    var tally: DestroyTally = .{};
    const g0 = stats.destroyed_inodes.load();
    tally.noteCommitted(3);
    tally.noteCommitted(2);
    try testing.expectEqual(@as(u64, 5), tally.load());
    try testing.expectEqual(g0 + 5, stats.destroyed_inodes.load());
    try testing.expectEqual(@as(u64, 1 + 10 + 20), publishLiveInodes(&.{ 10, 20 }));
    try testing.expectEqual(@as(u64, 31), stats.live_inodes.load());
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

test "destroy staging is one entry of inode + xattr Deletes priced by the admission's own arithmetic" {
    var t = tx.KvTx.init(testing.allocator, @src());
    const xk1 = key.xattrKey(500, 1, 0);
    const xk2 = key.xattrKey(500, 2, 0);
    try stageDestroy(&t, 500, &.{ &xk1, &xk2 });
    try stageDestroy(&t, 501, &.{});
    try testing.expectEqual(@as(usize, 4), t.staged.items.len);
    for (t.staged.items) |s| try testing.expectEqual(record.RecordKind.delete, s.kind);
    var q = try tx.buildQueuedTx(&t, 256 * 1024, null);
    defer q.deinit();
    try testing.expectEqual(journal.ENTRY_HDR_LEN + destroyEntryBytes(2) + destroyEntryBytes(0), q.len);
    try testing.expectEqual(@as(u64, 1 + 16 + 9), destroyEntryBytes(0));
    try testing.expectEqual(@as(u64, 1 + 16 + 9 + 2 * (1 + 16 + 17)), destroyEntryBytes(2));
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
    // entry.
    var batch: [64]u64 = @splat(destroyEntryBytes(1));
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
