//! The appender REGION — one writer's ring, page and seq words (charter
//! §5.5 "Appenders", "The record-seq space is monotone across rings";
//! `docs/porting/sym_appenders_manager.md` §2.5–§2.7, §3.3, §3.5, §6.3).
//! PORTABLE: the ring-size derivations with their ties, the segment table
//! over `journal.RingLayout`, the carve/zero plans, the seq-space
//! continuation and `SEQ_FRONTIER_SANE_MAX`, the per-cycle page plan under
//! the directory-first law, the region's tail law, the node-seq
//! incarnation mint, and the seq-offset MODEL. The device-bound runtime
//! (the lane over the segments, the backend's cache) is `bind.zig`'s.
//!
//! Region 0's ring IS the fixed ring and never grows; a region ≥ 1's ring
//! is a segment table of whole heap extents, REPLACED by `grownWith` on a
//! drained stall, never grown in place. Positions continue across a swap
//! (the head is a byte position over the table). Every carved extent is
//! ZEROED before a page names it: the heap hands a fresh ring the extents
//! a predecessor incarnation's ring occupied, with that ring's checksummed
//! entries still on the device at lap 0 like the new ring's own — the
//! replay chain would walk past its own head into them. A zeroed page
//! verifies nothing and replays to nothing.

const std = @import("std");
const derive = @import("../../core/derive.zig");
const journal = @import("../journal.zig");
const jc = @import("../journal_core.zig");
const node_seq = @import("../node_seq.zig");
const page = @import("appender_page.zig");
const explorer = @import("../../model/explorer.zig");

const assert = std.debug.assert;
pub const Extent = page.Extent;
pub const SegmentTable = page.SegmentTable;

// ---------------------------------------------------------------------------
// Constants with WHY.
// ---------------------------------------------------------------------------

/// `(256 KiB + MAX_ENTRY_LEN).next_power_of_two()` = 512 KiB: the checkpoint
/// carve-out plus one max entry must be admissible or a legal tx could never
/// commit; the power of two makes the segment count whole at every node
/// size (≈ 63 windows of user slice on a 4096-grain device).
pub const RING_FLOOR_BYTES: u64 = std.math.ceilPowerOfTwoAssert(u64, 256 * derive.KiB + journal.MAX_ENTRY_LEN);
/// `heap/16`: four times the solo ring's `volume/64` share; a quarter of
/// the heap stays for images at the peak (12,500 writers × 512 KiB = 6 GiB
/// of rings inside a 1 TiB heap's 64 GiB budget).
pub const RING_BUDGET_DIVISOR: u64 = 16;
/// Positions are journal bytes plus one per grant: 2^63 bytes is provably
/// not a frontier; a frontier past it would saturate every later stamp and
/// collapse the fold's LWW to insertion order.
pub const SEQ_FRONTIER_SANE_MAX: u64 = std.math.maxInt(u64) / 2;
/// One sequential write per carved extent at the zeroing (a control-plane
/// act at the carve, never a hot path).
pub const ZERO_CHUNK: u64 = 4 * derive.MiB;
/// The EWMA fold per checkpoint cycle: `7/8 · ewma + 1/8 · rate`.
pub const EWMA_SHIFT: u6 = 3;

comptime {
    assert(RING_FLOOR_BYTES == 512 * derive.KiB);
    assert(RING_FLOOR_BYTES % journal.PAGE_LEN == 0);
    assert(SEQ_FRONTIER_SANE_MAX == 0x7FFF_FFFF_FFFF_FFFF);
}

// ---------------------------------------------------------------------------
// Ring sizing (spec §3.5, §6.3).
// ---------------------------------------------------------------------------

/// The ring ceiling: an appender never needs more than the whole volume did
/// — `clamp(volume/64, 8 MiB, 32 MiB)` (the solo ring's clamp).
pub fn ringCeiling(volume_len: u64) u64 {
    return journal.journalRingLen(volume_len);
}

/// `clamp(2 × ewma_commit_bytes/s × CHECKPOINT_MAX_AGE, floor, ceiling)`
/// rounded down to pages (both bounds already are).
pub fn ringBytesDerived(ewma_commit_bytes_per_s: u64, max_age_ms: u64, volume_len: u64) u64 {
    const want = (2 *| ewma_commit_bytes_per_s) *| max_age_ms / std.time.ms_per_s;
    return derive.clamp(want, RING_FLOOR_BYTES, ringCeiling(volume_len)) / journal.PAGE_LEN * journal.PAGE_LEN;
}

/// A rejoin's size: `max(derived, hint)` clamped to `[floor, ceiling]`.
pub fn ringBytesHinted(derived_bytes: u64, hint_bytes: u64, volume_len: u64) u64 {
    return derive.clamp(@max(derived_bytes, hint_bytes), RING_FLOOR_BYTES, ringCeiling(volume_len)) / journal.PAGE_LEN * journal.PAGE_LEN;
}

/// `SQUEEZEFS_SYM_RING_KB` explicit: verbatim inside `[floor, ceiling]`;
/// above the volume's ceiling the gate REFUSES naming it (charter §1.6 —
/// never a clamp; spec Q-9), below the floor the registry range refused.
pub fn ringBytesExplicit(kib: u64, volume_len: u64) error{AboveCeiling}!u64 {
    const bytes = kib * derive.KiB;
    if (bytes > ringCeiling(volume_len)) return error.AboveCeiling;
    return @max(bytes, RING_FLOOR_BYTES) / journal.PAGE_LEN * journal.PAGE_LEN;
}

pub fn ringBudgetBytes(heap_len: u64) u64 {
    return heap_len / RING_BUDGET_DIVISOR;
}

/// `budget ÷ floor`: the ONE hard resource a join refuses on, published
/// live from the remainder (spec Q-10: one law — refuse on the remainder).
pub fn appendersCapacity(budget_remaining: u64) u64 {
    return budget_remaining / RING_FLOOR_BYTES;
}

/// The ring a join lands under the budget: `max(min(ask, remaining ↓ node),
/// floor)` — the count law budgets every admitted appender one FLOOR ring; a
/// hinted ask above the remainder lands the remainder and grows later. Null
/// when the remainder cannot reach the floor (the capacity refusal).
pub fn joinRingUnderBudget(ask_bytes: u64, remaining_bytes: u64, node_size: u64) ?u64 {
    const remaining = remaining_bytes / node_size * node_size;
    if (remaining < RING_FLOOR_BYTES) return null;
    return @max(@min(ask_bytes, remaining), RING_FLOOR_BYTES);
}

/// `min(ceiling − current, budget_remaining)`.
pub fn growRoomBytes(ceiling: u64, current: u64, budget_remaining: u64) u64 {
    return @min(ceiling -| current, budget_remaining);
}

/// A table slot is spent only on a run of the doubling class:
/// `ceil(want/2) ≥ 1` extents.
pub fn growSegmentFloorExtents(want_extents: u64) u64 {
    return @max((want_extents + 1) / 2, 1);
}

/// ONE step is at least a doubling: `clamp(max(derived, 2 × current),
/// current, ceiling)` — the table holds 8 segments; following a converging
/// EWMA by its increments would spend them on a storm's first second.
pub fn growTarget(derived: u64, current: u64, ceiling: u64) u64 {
    return derive.clamp(@max(derived, 2 *| current), current, @max(ceiling, current));
}

/// A run of consecutive extent indices.
pub const IndexRun = struct {
    start: u64,
    len: u64,

    pub fn end(self: IndexRun) u64 {
        return self.start + self.len;
    }
};

/// Sort + coalesce claimed extent indices into runs (in place; `out` holds
/// the runs, at most `extents.len`).
pub fn coalesceIndices(extents: []u64, out: []IndexRun) []IndexRun {
    std.mem.sort(u64, extents, {}, std.sort.asc(u64));
    var n: usize = 0;
    for (extents) |e| {
        if (n > 0 and out[n - 1].end() == e) {
            out[n - 1].len += 1;
        } else {
            out[n] = .{ .start = e, .len = 1 };
            n += 1;
        }
    }
    return out[0..n];
}

fn runLonger(_: void, a: IndexRun, b: IndexRun) bool {
    return a.len > b.len or (a.len == b.len and a.start < b.start);
}

/// A fragmented heap keeps what the page's table can name: all runs if ≤ 8;
/// else the LARGEST runs filling HALF the table when their total reaches
/// `floor_extents` (a ring that fits in four leaves four slots for growth),
/// else the largest 8. The rest is released — never a refusal. `kept` is
/// in ascending order; `released` lists the dropped runs.
pub const Fit = struct {
    kept: [page.RING_SEGMENTS_MAX]IndexRun = undefined,
    n_kept: u8 = 0,
    released: []IndexRun,

    pub fn keptSlice(self: *const Fit) []const IndexRun {
        return self.kept[0..self.n_kept];
    }
};

pub fn ringSegmentsThatFit(runs: []IndexRun, floor_extents: u64) Fit {
    var fit: Fit = .{ .released = runs[0..0] };
    if (runs.len <= page.RING_SEGMENTS_MAX) {
        for (runs, 0..) |r, i| fit.kept[i] = r;
        fit.n_kept = @intCast(runs.len);
        return fit;
    }
    std.mem.sort(IndexRun, runs, {}, runLonger);
    const half = page.RING_SEGMENTS_MAX / 2;
    var total: u64 = 0;
    for (runs[0..half]) |r| total += r.len;
    const keep: usize = if (total >= floor_extents) half else page.RING_SEGMENTS_MAX;
    for (runs[0..keep], 0..) |r, i| fit.kept[i] = r;
    fit.n_kept = @intCast(keep);
    std.mem.sort(IndexRun, fit.kept[0..keep], {}, struct {
        fn asc(_: void, a: IndexRun, b: IndexRun) bool {
            return a.start < b.start;
        }
    }.asc);
    fit.released = runs[keep..];
    return fit;
}

/// The device extent of an index run.
pub fn runExtent(heap_start: u64, node_size: u64, r: IndexRun) Extent {
    return .{ .start = heap_start + r.start * node_size, .len = r.len * node_size };
}

/// The zeroing plan: ≤ 4 MiB chunks, one sequential run per extent.
pub const ZeroChunk = struct { offset: u64, len: u64 };

pub fn zeroPlan(gpa: std.mem.Allocator, extents: []const Extent) std.mem.Allocator.Error![]ZeroChunk {
    var out: std.ArrayList(ZeroChunk) = .empty;
    errdefer out.deinit(gpa);
    for (extents) |e| {
        var off = e.start;
        while (off < e.end()) {
            const n = @min(e.end() - off, ZERO_CHUNK);
            try out.append(gpa, .{ .offset = off, .len = n });
            off += n;
        }
    }
    return out.toOwnedSlice(gpa);
}

// ---------------------------------------------------------------------------
// The seq space.
// ---------------------------------------------------------------------------

/// A fresh incarnation's seq space continues from `max(page.head_hint,
/// ring 0 head)`: record seqs are ring positions and per-key LWW compares
/// them raw across incarnations, so a fresh ring must start above every
/// position its id ever stamped AND above ring 0's head (the manager's
/// structural records on the slots it hands out).
pub fn seqContinuation(page_head_hint: u64, ring0_head: u64) u64 {
    return @max(page_head_hint, ring0_head);
}

/// A frontier a wire word may name: `(floor, SEQ_FRONTIER_SANE_MAX]` is
/// the admissible band.
pub fn frontierSane(frontier: u64) bool {
    return frontier <= SEQ_FRONTIER_SANE_MAX;
}

/// The EWMA fold: `ewma × 7/8 + rate / 8`.
pub fn ewmaFold(ewma: u64, rate: u64) u64 {
    return ewma - (ewma >> EWMA_SHIFT) + (rate >> EWMA_SHIFT);
}

/// The node-seq incarnation mint (§1.5): the durable counter read, `minted
/// + 1` written as ONE control entry and BARRIERED before the base is
/// answered; exhausting the ordinals REFUSES loud, never wraps.
pub const Minted = struct { minted: u64, base: u64 };

pub fn mintIncarnation(volume_base: u64, minted_so_far: u64) error{IncarnationsExhausted}!Minted {
    const ordinal = minted_so_far + 1;
    const base = node_seq.incarnationBase(volume_base, ordinal) orelse return error.IncarnationsExhausted;
    return .{ .minted = ordinal, .base = base };
}

// ---------------------------------------------------------------------------
// The region core: ring + page words + the tail law.
// ---------------------------------------------------------------------------

pub const PageWrite = struct {
    /// 0/1 directory, 2/3 ring-side.
    slot: u2,
    offset: u64,
    image: [page.PAGE_LEN]u8,
};

/// The plan of one page write under the directory-first law: every slot
/// `pageWriteSlots` names, one generation apart, each image stamped with
/// its own generation. The caller executes the writes in order, barriers,
/// then `pageWritten`.
pub const PagePlan = struct {
    writes: [2]PageWrite = undefined,
    len: u8 = 0,
    /// The generation after the last write.
    generation: u64,

    pub fn slice(self: *const PagePlan) []const PageWrite {
        return self.writes[0..self.len];
    }
};

/// A region's portable core: the ring geometry over its segment table, the
/// lock-free journal core and in-flight registry over it, the page words,
/// and the directory-first bookkeeping. Owns nothing device-bound.
pub const RegionCore = struct {
    appender_id: u32,
    /// The segment table as maintained at open/growth (never derived from
    /// page ranges).
    table: SegmentTable,
    /// The four slots' device offsets: `[A, B, R0, R1]`.
    slots: [page.PAGE_SLOTS]u64,
    ring_layout: journal.RingLayout,
    geo: jc.CoreGeometry,
    core: jc.JournalCore,
    reg: jc.InflightRegistry,
    /// The page words as last written / as the join declared them.
    page: page.Page,
    /// Bit i: directory slot i's image names `table`. Reset to 0 by every
    /// table change (a join onto a fresh ring, a growth, a leave).
    dir_named: u2 = 0,
    /// The head captured at the cycle's start the tail law judges.
    captured_head: u64 = 0,

    pub const Init = struct {
        appender_id: u32,
        table: SegmentTable,
        /// Directory slots A/B (appender 0: the fixed ring's first two pages).
        dir_slots: [2]u64,
        grain: u64,
        /// Resume words: the recovered head (a fresh join: the seq
        /// continuation), the replay tail, the page as read (or the join's).
        head_pos: u64,
        tail_pos: u64,
        page_words: page.Page,
    };

    pub fn init(gpa: std.mem.Allocator, in: Init) error{ OutOfMemory, TooManySegments, EmptySegment }!RegionCore {
        const part = page.ringPartOf(in.appender_id, &in.table);
        var exts: [page.RING_SEGMENTS_MAX]journal.ExtentRef = undefined;
        for (part.slice(), 0..) |s, i| exts[i] = .{ .start = s.start, .len = s.len };
        const layout = try journal.RingLayout.fromSegments(exts[0..part.len]);
        const geo = journal.productionGeometry(layout.pages, in.grain);
        const first = in.table.items[0];
        const ring_side: [2]u64 = if (in.appender_id == journal.FIXED_RING_APPENDER) .{ page.appender0PageOffsets(first.start)[2], page.appender0PageOffsets(first.start)[3] } else page.ringSideOffsets(first);
        var reg = try jc.InflightRegistry.init(gpa, in.head_pos, 64);
        reg.recoverSeqOffset(in.page_words.seq_offset);
        return .{
            .appender_id = in.appender_id,
            .table = in.table,
            .slots = .{ in.dir_slots[0], in.dir_slots[1], ring_side[0], ring_side[1] },
            .ring_layout = layout,
            .geo = geo,
            .core = jc.JournalCore.init(geo, in.head_pos, in.tail_pos),
            .reg = reg,
            .page = in.page_words,
            .captured_head = in.head_pos,
        };
    }

    pub fn deinit(self: *RegionCore) void {
        self.reg.deinit();
    }

    pub fn ringBytes(self: *const RegionCore) u64 {
        return self.table.totalBytes();
    }

    /// The next page write's plan for the words in `self.page` (the caller
    /// set state/tail/ckpt/segments first). Under the directory-first law.
    pub fn planPageWrite(self: *const RegionCore) error{ TooManySegments, TooManyGrants, TooManySlots, Unsorted }!PagePlan {
        var plan: PagePlan = .{ .generation = self.page.generation };
        const slots = page.pageWriteSlots(self.dir_named, plan.generation + 1);
        for (slots.slice()) |s| {
            plan.generation += 1;
            var img = self.page;
            img.generation = plan.generation;
            img.appender_id = self.appender_id;
            img.segments = self.table;
            img.seq_offset = self.reg.seqOffset();
            var w: PageWrite = .{ .slot = s, .offset = self.slots[s], .image = undefined };
            try img.encode(&w.image);
            plan.writes[plan.len] = w;
            plan.len += 1;
        }
        return plan;
    }

    /// The plan landed and was barriered: the generation moves, every
    /// directory slot written now names the table.
    pub fn pageWritten(self: *RegionCore, plan: *const PagePlan) void {
        self.page.generation = plan.generation;
        self.page.segments = self.table;
        for (plan.slice()) |w| if (w.slot < 2) {
            self.dir_named |= @as(u2, 1) << @intCast(w.slot);
        };
    }

    /// A table change: the next page write lands in both directory slots.
    pub fn setTable(self: *RegionCore, table: SegmentTable) void {
        self.table = table;
        self.dir_named = 0;
    }

    /// The cycle's start: the head the tail law may advance to.
    pub fn captureHead(self: *RegionCore) void {
        self.captured_head = self.core.headPos();
    }

    /// The region's tail law in force: its ring is reclaimed at a cycle
    /// whose flush left NO dirty node in the volume's cache (the fixpoint —
    /// every record of every ring up to the captured heads is then in
    /// barriered images the ledger names). A region's own records floor the
    /// shared cache's nodes in ring 0's position domain (`bind.zig`), so
    /// this rule never reads a mid-entry position as its tail. Answers the
    /// new tail.
    pub fn tailAfterCycle(self: *RegionCore, cache_clean: bool) u64 {
        if (cache_clean) {
            self.page.ledger_tail_seq = self.captured_head;
        }
        return self.page.ledger_tail_seq;
    }

    /// The page words a checkpoint cycle writes (§2.5).
    pub fn stampCycle(self: *RegionCore, ckpt_seq: u64) void {
        self.page.head_hint = self.core.headPos();
        self.page.ckpt_seq = ckpt_seq;
    }

    /// The seq-space frontier: the next stamp.
    pub fn seqFrontier(self: *RegionCore) u64 {
        return self.reg.seqFrontier(&self.core);
    }
};

// ---------------------------------------------------------------------------
// The seq-offset model: {reserve-stamp, raise, recover}.
// ---------------------------------------------------------------------------

/// Two stampers and one raiser over ONE ring: a stamp is `position +
/// offset` read under the registry lock; `raiseSeqFloor` is a monotone
/// fetch_max under the SAME lock. The invariant: every stamp whose
/// reservation was taken AFTER the raise exceeds the floor, and stamp order
/// equals reservation order. The weakening — the stamper reads the offset
/// OUTSIDE the lock (after its reservation) — lets a raise land between the
/// reservation and the read, so two stampers' seqs invert.
pub fn SeqOffsetModel(comptime weakened: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const FLOOR: u64 = 100;

        head: u64 = 0,
        offset: u64 = 0,
        lock: bool = false,
        raised: bool = false,
        /// Per stamper: pc, its reserved position, its stamp.
        pc: [3]u8 = @splat(0),
        pos: [2]u64 = @splat(0),
        seq: [2]u64 = @splat(0),
        /// Reservation order: who reserved first.
        order: [2]u8 = @splat(0),
        reserved_count: u8 = 0,
        /// Did the reservation happen after the raise?
        after_raise: [2]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return if (t < 2) self.pc[t] >= 4 else self.pc[2] >= 2;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == 2) {
                // The raiser: lock, fetch_max(need), unlock — one lock
                // section.
                switch (self.pc[2]) {
                    0 => if (!self.lock) {
                        self.lock = true;
                        self.pc[2] = 1;
                    },
                    1 => {
                        const need = (FLOOR + 1) -| self.head;
                        self.offset = @max(self.offset, need);
                        self.raised = true;
                        self.lock = false;
                        self.pc[2] = 2;
                    },
                    else => {},
                }
                return;
            }
            switch (self.pc[t]) {
                0 => if (!self.lock) {
                    self.lock = true;
                    self.pc[t] = 1;
                },
                1 => {
                    // reserve: bump the head; record order.
                    self.pos[t] = self.head;
                    self.head += 10;
                    self.order[t] = self.reserved_count;
                    self.reserved_count += 1;
                    self.after_raise[t] = self.raised;
                    if (weakened) {
                        // Unlock BEFORE reading the offset.
                        self.lock = false;
                        self.pc[t] = 2;
                    } else {
                        self.seq[t] = self.pos[t] + self.offset;
                        self.lock = false;
                        self.pc[t] = 4;
                    }
                },
                2 => {
                    self.seq[t] = self.pos[t] + self.offset;
                    self.pc[t] = 4;
                },
                else => {},
            }
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            for (0..2) |t| if (self.after_raise[t] and self.seq[t] <= FLOOR) return error.StampBelowFloor;
            // Stamp order ≡ reservation order.
            if (self.order[0] < self.order[1] and self.seq[0] >= self.seq[1]) return error.StampOrderInverted;
            if (self.order[1] < self.order[0] and self.seq[1] >= self.seq[0]) return error.StampOrderInverted;
            // Recovery: `seq − position` is the offset in force at the stamp.
            for (0..2) |t| if (self.seq[t] - self.pos[t] > self.offset) return error.RecoveryOverstates;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const MiB = derive.MiB;
const KiB = derive.KiB;

test "ring size derivation: floor 512 KiB, ceiling = the solo clamp, capacity = heap/16 ÷ floor; the explicit knob refuses above the ceiling, never clamps" {
    try testing.expectEqual(512 * KiB, RING_FLOOR_BYTES);
    try testing.expectEqual(8 * MiB, ringCeiling(256 * MiB));
    try testing.expectEqual(32 * MiB, ringCeiling(1 << 40));
    // ewma 0 ⇒ the floor; 100 creates/s × 230 B ⇒ the floor; the 8 k peak
    // (≈ 1.84 MB/s × 2 × 1 s) ⇒ ≈ 3.7 MB, page-aligned, under the 1 GiB
    // volume's 16 MiB ceiling; a torrent ⇒ the ceiling.
    try testing.expectEqual(RING_FLOOR_BYTES, ringBytesDerived(0, 1000, 1 << 30));
    try testing.expectEqual(RING_FLOOR_BYTES, ringBytesDerived(100 * 230, 1000, 1 << 30));
    const peak = ringBytesDerived(8000 * 230, 1000, 1 << 30);
    try testing.expectEqual(@as(u64, 2 * 8000 * 230 / 4096 * 4096), peak);
    try testing.expectEqual(16 * MiB, ringBytesDerived(1 << 40, 1000, 1 << 30));
    try testing.expectEqual(4 * MiB, ringBytesHinted(RING_FLOOR_BYTES, 4 * MiB, 1 << 30));
    try testing.expectEqual(16 * MiB, ringBytesHinted(RING_FLOOR_BYTES, 1 << 40, 1 << 30));
    try testing.expectEqual(2 * MiB, try ringBytesExplicit(2048, 1 << 30));
    try testing.expectEqual(RING_FLOOR_BYTES, try ringBytesExplicit(512, 1 << 30));
    try testing.expectError(error.AboveCeiling, ringBytesExplicit(32 * 1024, 256 * MiB));
    // Budget and capacity: a 1 TiB heap ⇒ 64 GiB ⇒ 131,072 at the floor.
    try testing.expectEqual(64 * derive.GiB, ringBudgetBytes(1 << 40));
    try testing.expectEqual(@as(u64, 131_072), appendersCapacity(ringBudgetBytes(1 << 40)));
    // The 64 MiB test volume: ≈ 62 MiB heap /16 ≈ 3.9 MiB ⇒ 7 at the floor.
    try testing.expectEqual(@as(u64, 7), appendersCapacity(ringBudgetBytes(62 * MiB)));
    // The join under the budget: the ask bounded by the remainder, the
    // remainder below the floor refuses.
    try testing.expectEqual(@as(?u64, 4 * MiB), joinRingUnderBudget(4 * MiB, 10 * MiB, 65_536));
    try testing.expectEqual(@as(?u64, 1 * MiB), joinRingUnderBudget(4 * MiB, 1 * MiB + 1000, 65_536));
    try testing.expectEqual(@as(?u64, RING_FLOOR_BYTES), joinRingUnderBudget(100 * KiB, 10 * MiB, 65_536));
    try testing.expectEqual(@as(?u64, null), joinRingUnderBudget(4 * MiB, RING_FLOOR_BYTES - 1, 65_536));
    try testing.expectEqual(@as(u64, 3 * MiB), growRoomBytes(8 * MiB, 5 * MiB, 10 * MiB));
    try testing.expectEqual(@as(u64, 1 * MiB), growRoomBytes(8 * MiB, 5 * MiB, 1 * MiB));
    try testing.expectEqual(@as(u64, 0), growRoomBytes(4 * MiB, 5 * MiB, 1 * MiB));
    try testing.expectEqual(@as(u64, 1), growSegmentFloorExtents(1));
    try testing.expectEqual(@as(u64, 4), growSegmentFloorExtents(8));
    try testing.expectEqual(@as(u64, 4), growSegmentFloorExtents(7));
    try testing.expectEqual(@as(u64, 1), growSegmentFloorExtents(0));
    // One step is at least a doubling, never past the ceiling.
    try testing.expectEqual(2 * MiB, growTarget(RING_FLOOR_BYTES, 1 * MiB, 8 * MiB));
    try testing.expectEqual(5 * MiB, growTarget(5 * MiB, 1 * MiB, 8 * MiB));
    try testing.expectEqual(8 * MiB, growTarget(20 * MiB, 5 * MiB, 8 * MiB));
    try testing.expectEqual(@as(u64, 1 << 40), ewmaFold(1 << 40, 1 << 40));
    try testing.expectEqual(@as(u64, 7000 / 8 * 8 / 8 * 7 + 1000 / 8), ewmaFold(7000, 1000));
}

test "the carve: coalesce, then what the table can name — all when ≤ 8 runs, the largest half when it reaches the floor, else the largest 8; the rest released" {
    var ext = [_]u64{ 5, 3, 4, 10, 11, 20 };
    var runs: [6]IndexRun = undefined;
    const r = coalesceIndices(&ext, &runs);
    try testing.expectEqualSlices(IndexRun, &.{ .{ .start = 3, .len = 3 }, .{ .start = 10, .len = 2 }, .{ .start = 20, .len = 1 } }, r);
    const fit = ringSegmentsThatFit(r, 6);
    try testing.expectEqual(@as(u8, 3), fit.n_kept);
    try testing.expectEqual(@as(usize, 0), fit.released.len);
    // 10 singletons + two long runs: the largest 4 reach a floor of 10.
    var many: [12]IndexRun = undefined;
    for (0..10) |i| many[i] = .{ .start = 100 + i * 2, .len = 1 };
    many[10] = .{ .start = 1000, .len = 6 };
    many[11] = .{ .start = 2000, .len = 5 };
    const half = ringSegmentsThatFit(&many, 10);
    try testing.expectEqual(@as(u8, 4), half.n_kept);
    try testing.expectEqual(@as(usize, 8), half.released.len);
    try testing.expectEqual(IndexRun{ .start = 100, .len = 1 }, half.kept[0]);
    try testing.expectEqual(IndexRun{ .start = 2000, .len = 5 }, half.kept[3]);
    // A floor the half cannot reach keeps the largest 8.
    var singles: [12]IndexRun = undefined;
    for (0..12) |i| singles[i] = .{ .start = i * 3, .len = 1 };
    const eight = ringSegmentsThatFit(&singles, 10);
    try testing.expectEqual(@as(u8, 8), eight.n_kept);
    try testing.expectEqual(@as(usize, 4), eight.released.len);
    try testing.expectEqual(Extent{ .start = 0x400000 + 3 * 65_536, .len = 3 * 65_536 }, runExtent(0x400000, 65_536, .{ .start = 3, .len = 3 }));
    const chunks = try zeroPlan(testing.allocator, &.{ .{ .start = 0, .len = 9 * MiB }, .{ .start = 100 * MiB, .len = 64 * KiB } });
    defer testing.allocator.free(chunks);
    try testing.expectEqual(@as(usize, 4), chunks.len);
    try testing.expectEqual(ZeroChunk{ .offset = 8 * MiB, .len = 1 * MiB }, chunks[2]);
    try testing.expectEqual(ZeroChunk{ .offset = 100 * MiB, .len = 64 * KiB }, chunks[3]);
}

test "seq continuation, the sane frontier, the incarnation mint" {
    try testing.expectEqual(@as(u64, 500), seqContinuation(200, 500));
    try testing.expectEqual(@as(u64, 900), seqContinuation(900, 500));
    try testing.expect(frontierSane(SEQ_FRONTIER_SANE_MAX) and !frontierSane(SEQ_FRONTIER_SANE_MAX + 1));
    const b: u64 = 0x1000;
    const m = try mintIncarnation(b, 0);
    try testing.expectEqual(@as(u64, 1), m.minted);
    try testing.expectEqual(b + node_seq.INCARNATION_SPACE, m.base);
    try testing.expectEqual(b + 3 * node_seq.INCARNATION_SPACE, (try mintIncarnation(b, 2)).base);
    try testing.expectError(error.IncarnationsExhausted, mintIncarnation(std.math.maxInt(u64) >> 1, node_seq.INCARNATION_ORDINAL_MAX));
    // The minted base passes the joiner's screen.
    _ = try node_seq.screenIncarnationBase(b, m.base, null);
}

test "the region core: a joiner's ring over its table with the ring-side pages carved off; the page plan lands in both directory slots first, then rotates; a table change resets the law; the tail advances only at the fixpoint" {
    const gpa = testing.allocator;
    var table = SegmentTable.one(.{ .start = 0x400000, .len = RING_FLOOR_BYTES });
    try table.push(.{ .start = 0x800000, .len = RING_FLOOR_BYTES });
    var rc = try RegionCore.init(gpa, .{ .appender_id = 3, .table = table, .dir_slots = .{ 0x100000, 0x101000 }, .grain = 4096, .head_pos = 777, .tail_pos = 777, .page_words = .{ .appender_id = 3, .generation = 0, .state = .live, .term = 1, .head_hint = 777, .ledger_tail_seq = 777, .seq_offset = 50 } });
    defer rc.deinit();
    try testing.expectEqual(@as(u64, 126 + 128), rc.ring_layout.pages);
    try testing.expectEqual(@as(u64, 0x400000 + 8192), rc.ring_layout.pageOffset(0));
    try testing.expectEqual(@as(u64, 0x800000), rc.ring_layout.pageOffset(126));
    try testing.expectEqual([4]u64{ 0x100000, 0x101000, 0x400000, 0x401000 }, rc.slots);
    try testing.expectEqual(@as(u64, 50), rc.reg.seqOffset());
    try testing.expectEqual(2 * RING_FLOOR_BYTES, rc.ringBytes());
    // First write: both directory slots, generations 1 and 2.
    var plan = try rc.planPageWrite();
    try testing.expectEqual(@as(u8, 2), plan.len);
    try testing.expectEqual(@as(u64, 0x100000), plan.writes[0].offset);
    try testing.expectEqual(@as(u64, 0x101000), plan.writes[1].offset);
    try testing.expectEqual(@as(u64, 1), (try page.decode(&plan.writes[0].image)).generation);
    try testing.expectEqual(@as(u64, 2), (try page.decode(&plan.writes[1].image)).generation);
    try testing.expect((try page.decode(&plan.writes[1].image)).segments.eql(&table));
    try testing.expectEqual(@as(u64, 50), (try page.decode(&plan.writes[1].image)).seq_offset);
    rc.pageWritten(&plan);
    try testing.expectEqual(@as(u2, 0b11), rc.dir_named);
    // Then the rotation: generation 3 ⇒ slot 3 (R1).
    plan = try rc.planPageWrite();
    try testing.expectEqual(@as(u8, 1), plan.len);
    try testing.expectEqual(@as(u2, 3), plan.writes[0].slot);
    try testing.expectEqual(@as(u64, 0x401000), plan.writes[0].offset);
    rc.pageWritten(&plan);
    try testing.expectEqual(@as(u64, 3), rc.page.generation);
    // Growth: a new table ⇒ both directory slots again.
    var grown = table;
    try grown.push(.{ .start = 0xC00000, .len = RING_FLOOR_BYTES });
    rc.setTable(grown);
    plan = try rc.planPageWrite();
    try testing.expectEqual(@as(u8, 2), plan.len);
    try testing.expect((try page.decode(&plan.writes[0].image)).segments.eql(&grown));
    // The tail law.
    rc.captureHead();
    try testing.expectEqual(@as(u64, 777), rc.tailAfterCycle(false));
    const adm = rc.core.tryAdmit(100, .user).?;
    const r = try rc.reg.reserveRegistered(&rc.core, adm);
    try testing.expectEqual(@as(u64, 777 + 50), r.seq_base);
    rc.reg.complete(&rc.core, r.res);
    rc.captureHead();
    try testing.expectEqual(@as(u64, 777), rc.tailAfterCycle(false));
    try testing.expectEqual(rc.core.headPos(), rc.tailAfterCycle(true));
    rc.stampCycle(9);
    try testing.expectEqual(@as(u64, 9), rc.page.ckpt_seq);
    try testing.expectEqual(rc.core.headPos(), rc.page.head_hint);
    // Appender 0's core maps the fixed ring behind its four pages.
    var r0 = try RegionCore.init(gpa, .{ .appender_id = 0, .table = SegmentTable.one(.{ .start = 135_168, .len = 1 << 20 }), .dir_slots = .{ 135_168, 135_168 + 4096 }, .grain = 4096, .head_pos = 0, .tail_pos = 0, .page_words = .{ .appender_id = 0, .generation = 1 } });
    defer r0.deinit();
    try testing.expectEqual(@as(u64, 135_168 + 16_384), r0.ring_layout.pageOffset(0));
    try testing.expectEqual(@as(u64, 256 - 4), r0.ring_layout.pages);
    try testing.expectEqual([4]u64{ 135_168, 135_168 + 4096, 135_168 + 8192, 135_168 + 12_288 }, r0.slots);
}

test "model: a stamp reserved after a raise exceeds the floor and stamp order is reservation order; the weakening (the offset read outside the lock) fails" {
    try explorer.expectHolds(SeqOffsetModel(false), .{});
    try explorer.expectFails(SeqOffsetModel(true), .{}, error.FinalViolated);
}
