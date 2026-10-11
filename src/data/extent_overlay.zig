//! W2 — the extent overlay, arm F of the write admission (charter §6.2
//! item 4; `docs/porting/data_write_path.md` §6; decision 0003 §J). For
//! sub-cap patch-ineligible shapes (a transformed volume, a shared block, a
//! hole, an unaligned write): a compact PAGE-INDEXED overlay charged to the
//! budget as `parked_extent_bytes` — a 512 B write parks one 4 KiB page, not
//! a 4 MiB block. A touched page is content-valid at every byte: zeros on a
//! hole block, the base's page read ONCE at first touch on a mapped
//! passthrough block (the seed at the page grain, never a whole-block
//! image). Coverage is therefore page-granular and EXACT (a 128 B page set
//! over the format's largest block), so a reader composing the window never
//! meets a run table that silently dropped runs — "uncovered bytes never
//! escape" holds by construction.
//!
//! The three landings and their bounds (§J.1; the tie tests restate them):
//! a HOLE block lands once, whole, at its flush (write-before-publish; it
//! escalates at `pages ≥ block/4P` to a zero-completed full buffer and
//! parks: `amp = block / u_flush`); a MAPPED sole-owner passthrough block
//! folds IN PLACE at the page grain (`amp ≤ pages(e, a) × P / e` — 8× for
//! 512 B sector-aligned, 16× arbitrary, 2× for an unaligned 4 KiB write);
//! a CoW block (transformed, shared, epoch-bound) escalates to a full
//! buffer seeded from the base ONCE and parks (`amp = block / u_flush`).
//! The former count cap (`FOLD_MAX_EXTENTS` 64) folded a block WHOLE every
//! 64 extents — 128× at 512 B (generic/074: 109 GB for 855 MB) — and is
//! retired with `FOLD_MAX_BYTES`; the one trigger left is the RAM bound.
//!
//! The spill record (versioned + checksummed, NO seed read at spill) carries
//! the touched pages as contiguous runs; the fold composes seed → record
//! (older) → pages (newest) for the whole-block arm.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const checksum = @import("../core/checksum.zig");
const transform = @import("transform.zig");
const seams = @import("write_seams.zig");

pub const PAGE: u32 = seams.PAGE;

/// The format's largest block (`transform.MAX_BLOCK_SIZE`) in pages: the
/// page set's width (128 B at 4 MiB).
pub const MAX_PAGES: usize = transform.MAX_BLOCK_SIZE / PAGE;
pub const PageSet = std.StaticBitSet(MAX_PAGES);

/// `[start, end)` block-relative bytes.
pub const Run = struct { start: u32, end: u32 };

// ---------------------------------------------------------------------------
// Derivations (every constant with its WHY).
// ---------------------------------------------------------------------------

/// The extent-eligible class: `len × 4 < block` (a quarter block and up
/// is accumulation's shape).
pub fn isSmall(len: u64, block_size: u64) bool {
    return len * 4 < block_size;
}

pub fn pagesPerBlock(block_size: u64) u32 {
    return @intCast(block_size / PAGE);
}

/// The one trigger: a quarter block of page RAM (`block/4P` pages, floor 1
/// at a one-page block). The former 25 % coverage edge measured in what the
/// overlay actually holds — a touched page costs a page whatever the write's
/// length — so the RAM bound no longer depends on the extent size.
pub fn foldPagesDerived(block_size: u64) u32 {
    return @max(1, pagesPerBlock(block_size) / 4);
}

pub fn foldDue(pages_touched: usize, block_size: u64) bool {
    return pages_touched >= foldPagesDerived(block_size);
}

/// Pages a write of `len` bytes at offsets `≡ 0 (mod align)` can touch
/// (`align | PAGE`; 1 = arbitrary): the worst start is `PAGE − align` into
/// a page, so `1 + ⌊(len + PAGE − align − 1) / PAGE⌋`.
pub fn pagesWorst(len: u64, offset_align: u64) u64 {
    assert(len != 0 and offset_align != 0 and PAGE % offset_align == 0);
    return 1 + (len + PAGE - offset_align - 1) / PAGE;
}

/// The in-place fold's write amplification bound in ppm (§J.1 landing 2):
/// every touched page is written once per fold whatever it received.
pub fn inPlaceBoundPpm(len: u64, offset_align: u64) u64 {
    return pagesWorst(len, offset_align) * PAGE * 1_000_000 / len;
}

/// A hole block's amplification (§J.1 landing 1): ONE whole-block write
/// per block, so `block / u_flush`, in ppm.
pub fn holeBoundPpm(block_size: u64, user_bytes_at_flush: u64) u64 {
    assert(user_bytes_at_flush != 0);
    return block_size * 1_000_000 / user_bytes_at_flush;
}

// ---------------------------------------------------------------------------
// Stats.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Sheddable via the fold: `pages × PAGE` + the page structs.
    pub var parked_extent_bytes: counters.Gauge = .{};
    pub var extent_parks: counters.ShardedCounter = .{};
    pub var extent_escalations: counters.ShardedCounter = .{};
    /// An escalation due at the parked-buffer cap, left to the block's next
    /// write or its flush (the extent stays parked; `write_admit`'s gate).
    pub var extent_escalations_deferred: counters.ShardedCounter = .{};
    /// The base's page read ONCE at first touch (a deferred overlay on a
    /// passthrough block) — the seed at the page grain.
    pub var extent_page_seed_reads: counters.ShardedCounter = .{};
    pub var extent_page_seed_bytes: counters.ShardedCounter = .{};
    pub var extent_spills: counters.ShardedCounter = .{};
    pub var extent_spill_bytes: counters.ShardedCounter = .{};
    pub var extent_records_recovered: counters.ShardedCounter = .{};
    pub var extent_records_torn_discarded: counters.ShardedCounter = .{};
    pub var extent_records_future_refused: counters.ShardedCounter = .{};
    /// Every fold (in place or whole); `fold_fill` is the writes it absorbed.
    pub var fold_passes: counters.ShardedCounter = .{};
    pub var fold_fill: counters.ShardedLatencyHistogram = .{};
    /// Whole-block seeds (the CoW escalation): ≈ the CoW folds.
    pub var fold_seed_reads: counters.ShardedCounter = .{};
    pub var fold_extents_folded: counters.ShardedCounter = .{};
    /// The in-place arm (§J.1 landing 2): folds, pages, DMAs, device bytes
    /// — `fold_inplace_write_bytes` is a device-write arm of the bracket
    /// ledger (`brk.py` `DEVICE_WRITE_ARMS`, `bench.zig`).
    pub var fold_inplace_passes: counters.ShardedCounter = .{};
    pub var fold_inplace_pages: counters.ShardedCounter = .{};
    pub var fold_inplace_dmas: counters.ShardedCounter = .{};
    pub var fold_inplace_write_bytes: counters.ShardedCounter = .{};
    /// The in-place arm was asked and declined (CoW: transformed, shared,
    /// epoch-bound, an open overlay record, a hole) — the whole-block arm.
    pub var fold_inplace_declines: counters.ShardedCounter = .{};
    /// A failed in-place DMA: the entry stays parked (never lossy), loud.
    pub var fold_inplace_dma_errors: counters.ShardedCounter = .{};
    pub var small_first_write_completed_block: counters.Tripwire = .init("small_first_write_completed_block");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"parked_extent_bytes\":{d},", .{stats.parked_extent_bytes.load()});
    inline for (.{ "extent_parks", "extent_escalations", "extent_escalations_deferred", "extent_page_seed_reads", "extent_page_seed_bytes", "extent_spills", "extent_spill_bytes", "extent_records_recovered", "extent_records_torn_discarded", "extent_records_future_refused", "fold_passes", "fold_seed_reads", "fold_extents_folded", "fold_inplace_passes", "fold_inplace_pages", "fold_inplace_dmas", "fold_inplace_write_bytes", "fold_inplace_declines", "fold_inplace_dma_errors" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("\"fold_fill\":");
    try stats.fold_fill.snapshot().writeJson(w);
    try w.print(",\"small_first_write_completed_block\":{d}}}", .{counters.tripwireCount("small_first_write_completed_block")});
}

// ---------------------------------------------------------------------------
// The page overlay.
// ---------------------------------------------------------------------------

/// One touched page: content-valid at every byte.
pub const Page = struct {
    idx: u32,
    data: []u8,
};

/// The base's page at first touch (a deferred overlay over a passthrough
/// mapped block); null = zeros (a hole, or a deferred block without a
/// binding — the existing bytes below EOF were a hole).
pub const Seeder = struct {
    ctx: ?*anyopaque,
    /// The base's page `idx` (PAGE bytes) into `dst`.
    read: *const fn (ctx: ?*anyopaque, idx: u32, dst: []u8) seams.ReadError!void,
};

/// What one page costs the budget: its buffer and its struct.
pub const PAGE_CHARGE: u64 = PAGE + @sizeOf(Page);

pub const ExtentOverlay = struct {
    /// Sorted by `idx`.
    pages: std.ArrayList(Page) = .empty,
    /// Item B: existing device data under the extents (the untouched pages
    /// come from the base; the touched ones were seeded at first touch).
    base_deferred: bool,
    /// User bytes merged (a re-write of the same bytes counts again: the
    /// bound's denominator is what the user WROTE, which only brings the
    /// fold earlier, never later).
    bytes: u64 = 0,
    /// Writes merged (`fold_fill`'s k).
    extents: u32 = 0,

    pub const MergeError = error{ OutOfMemory, InputOutput };

    pub fn init(base_deferred: bool) ExtentOverlay {
        return .{ .base_deferred = base_deferred };
    }

    pub fn deinit(self: *ExtentOverlay, gpa: Allocator) void {
        for (self.pages.items) |p| gpa.free(p.data);
        stats.parked_extent_bytes.sub(self.pages.items.len * PAGE_CHARGE);
        self.pages.deinit(gpa);
        self.* = undefined;
    }

    pub fn count(self: *const ExtentOverlay) usize {
        return self.extents;
    }

    pub fn pageCount(self: *const ExtentOverlay) usize {
        return self.pages.items.len;
    }

    /// The RAM the overlay holds (the budget charge).
    pub fn chargedBytes(self: *const ExtentOverlay) u64 {
        return self.pages.items.len * PAGE_CHARGE;
    }

    /// The index of page `idx` in `pages`, or its insertion point.
    fn find(self: *const ExtentOverlay, idx: u32) struct { pos: usize, found: bool } {
        var lo: usize = 0;
        var hi: usize = self.pages.items.len;
        while (lo < hi) {
            const mid = lo + (hi - lo) / 2;
            const p = self.pages.items[mid].idx;
            if (p == idx) return .{ .pos = mid, .found = true };
            if (p < idx) lo = mid + 1 else hi = mid;
        }
        return .{ .pos = lo, .found = false };
    }

    /// Merge `[rel, rel + data.len)`: every page it touches is found or
    /// created (zeros, or the base's page through `seeder` — one ranged
    /// read, the seed at the page grain), then the bytes land. A seed that
    /// fails leaves the overlay as it was (nothing of this write recorded).
    pub fn merge(self: *ExtentOverlay, gpa: Allocator, rel: u32, data: []const u8, seeder: ?Seeder) MergeError!void {
        assert(data.len != 0);
        const end: u32 = rel + @as(u32, @intCast(data.len));
        var idx: u32 = rel / PAGE;
        const last: u32 = (end - 1) / PAGE;
        // Pass 1: every page present (the seeds before any byte lands, so a
        // failed seed records nothing).
        while (idx <= last) : (idx += 1) {
            const f = self.find(idx);
            if (f.found) continue;
            const buf = try gpa.alloc(u8, PAGE);
            errdefer gpa.free(buf);
            if (seeder) |s| {
                s.read(s.ctx, idx, buf) catch return error.InputOutput;
                stats.extent_page_seed_reads.inc();
                stats.extent_page_seed_bytes.add(PAGE);
            } else {
                @memset(buf, 0);
            }
            try self.pages.insert(gpa, f.pos, .{ .idx = idx, .data = buf });
            stats.parked_extent_bytes.add(PAGE_CHARGE);
        }
        // Pass 2: the bytes.
        var pos: u32 = rel;
        while (pos < end) {
            const pidx = pos / PAGE;
            const in_page = pos % PAGE;
            const take: u32 = @min(PAGE - in_page, end - pos);
            const p = self.pages.items[self.find(pidx).pos];
            @memcpy(p.data[in_page .. in_page + take], data[pos - rel .. pos - rel + take]);
            pos += take;
        }
        self.bytes += data.len;
        self.extents += 1;
    }

    /// Every touched page as a set (the read seam's exact coverage).
    pub fn pageSet(self: *const ExtentOverlay) PageSet {
        var s = PageSet.empty;
        for (self.pages.items) |p| s.set(p.idx);
        return s;
    }

    /// Every page of the block is touched: the block is a complete,
    /// content-valid image.
    pub fn isComplete(self: *const ExtentOverlay, block_size: u64) bool {
        return self.pages.items.len == pagesPerBlock(block_size);
    }

    /// Lay every touched page into a block buffer (the escalation; the
    /// untouched pages are the caller's — zeros or the base).
    pub fn layInto(self: *const ExtentOverlay, dst: []u8) void {
        for (self.pages.items) |p| @memcpy(dst[p.idx * PAGE ..][0..PAGE], p.data);
    }

    /// The whole block: touched pages from the overlay, the rest from
    /// `base` (null = zeros).
    pub fn completeInto(self: *const ExtentOverlay, dst: []u8, base: ?[]const u8) void {
        if (base) |b| @memcpy(dst, b[0..dst.len]) else @memset(dst, 0);
        self.layInto(dst);
    }

    /// `[rel, rel + out.len)`: touched pages' bytes, zeros between.
    pub fn coveredRead(self: *const ExtentOverlay, rel: u32, out: []u8) void {
        @memset(out, 0);
        const end: u32 = rel + @as(u32, @intCast(out.len));
        for (self.pages.items) |p| {
            const ps = p.idx * PAGE;
            const a = @max(ps, rel);
            const b = @min(ps + PAGE, end);
            if (a < b) @memcpy(out[a - rel .. b - rel], p.data[a - ps .. b - ps]);
        }
    }

    /// The touched pages as contiguous runs (the in-place fold's DMAs, the
    /// spill record's table); `out.len ≥ pageCount()`.
    pub fn pageRuns(self: *const ExtentOverlay, out: []Run) []Run {
        var n: usize = 0;
        for (self.pages.items) |p| {
            const s = p.idx * PAGE;
            if (n != 0 and out[n - 1].end == s) {
                out[n - 1].end = s + PAGE;
            } else {
                out[n] = .{ .start = s, .end = s + PAGE };
                n += 1;
            }
        }
        return out[0..n];
    }

    pub fn pageRunCount(self: *const ExtentOverlay) usize {
        var n: usize = 0;
        var prev_end: u32 = 0;
        for (self.pages.items, 0..) |p, i| {
            const s = p.idx * PAGE;
            if (i == 0 or prev_end != s) n += 1;
            prev_end = s + PAGE;
        }
        return n;
    }
};

// ---------------------------------------------------------------------------
// The spill record (versioned + checksummed; NO seed read at spill).
// ---------------------------------------------------------------------------

pub const RECORD_MAGIC = "SQZX".*;
pub const EXTENT_RECORD_VERSION: u32 = 2;
pub const RECORD_FLAG_BASE_DEFERRED: u32 = 1 << 0;

/// 40 B little-endian header; then `count` × `(start u32, len u32)`, then the
/// payloads concatenated. The xxh3 covers header (field zeroed) ‖ table ‖
/// payloads.
pub const ExtentRecordWire = extern struct {
    magic: [4]u8,
    version_le: [4]u8,
    fencing_le: [8]u8,
    block_le: [4]u8,
    flags_le: [4]u8,
    count_le: [4]u8,
    reserved_le: [4]u8,
    xxh3_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = @sizeOf(ExtentRecordWire);
pub const RECORD_ENTRY_LEN: usize = 8;

comptime {
    assert(RECORD_HEADER_LEN == 40);
    assert(@offsetOf(ExtentRecordWire, "xxh3_le") == 32);
}

pub const DecodeError = error{ Torn, FutureVersion, Malformed };

pub const Decoded = struct {
    fencing: u64,
    block: u32,
    base_deferred: bool,
    /// `(start, len)` ascending (a view into the record bytes).
    table: []align(1) const [2]u32,
    payloads: []const u8,

    /// Apply the record's extents onto `dst` (older than the RAM pages).
    pub fn applyTo(self: Decoded, dst: []u8) void {
        var pos: usize = 0;
        for (self.table) |e| {
            @memcpy(dst[e[0] .. e[0] + e[1]], self.payloads[pos .. pos + e[1]]);
            pos += e[1];
        }
    }
};

pub fn encodedLen(x: *const ExtentOverlay) usize {
    return RECORD_HEADER_LEN + x.pageRunCount() * RECORD_ENTRY_LEN + x.pageCount() * PAGE;
}

/// Encode the overlay as a spill record into `out` (`≥ encodedLen`): the
/// touched pages as contiguous runs, their bytes in order.
pub fn encode(x: *const ExtentOverlay, fencing: u64, block: u32, out: []u8) []u8 {
    const n = encodedLen(x);
    assert(out.len >= n);
    const n_runs = x.pageRunCount();
    var h: ExtentRecordWire = undefined;
    h.magic = RECORD_MAGIC;
    std.mem.writeInt(u32, &h.version_le, EXTENT_RECORD_VERSION, .little);
    std.mem.writeInt(u64, &h.fencing_le, fencing, .little);
    std.mem.writeInt(u32, &h.block_le, block, .little);
    std.mem.writeInt(u32, &h.flags_le, if (x.base_deferred) RECORD_FLAG_BASE_DEFERRED else 0, .little);
    std.mem.writeInt(u32, &h.count_le, @intCast(n_runs), .little);
    h.reserved_le = @splat(0);
    h.xxh3_le = @splat(0);
    @memcpy(out[0..RECORD_HEADER_LEN], std.mem.asBytes(&h));
    var pos: usize = RECORD_HEADER_LEN;
    var prev_end: u32 = 0;
    var run_pos: usize = RECORD_HEADER_LEN;
    for (x.pages.items, 0..) |p, i| {
        const s = p.idx * PAGE;
        if (i == 0 or prev_end != s) {
            run_pos = pos;
            std.mem.writeInt(u32, out[pos..][0..4], s, .little);
            std.mem.writeInt(u32, out[pos + 4 ..][0..4], PAGE, .little);
            pos += RECORD_ENTRY_LEN;
        } else {
            const len = std.mem.readInt(u32, out[run_pos + 4 ..][0..4], .little);
            std.mem.writeInt(u32, out[run_pos + 4 ..][0..4], len + PAGE, .little);
        }
        prev_end = s + PAGE;
    }
    assert(pos == RECORD_HEADER_LEN + n_runs * RECORD_ENTRY_LEN);
    for (x.pages.items) |p| {
        @memcpy(out[pos .. pos + PAGE], p.data);
        pos += PAGE;
    }
    assert(pos == n);
    checksum.stamp(out[0..n], @offsetOf(ExtentRecordWire, "xxh3_le"));
    stats.extent_spills.inc();
    stats.extent_spill_bytes.add(x.pageCount() * PAGE);
    return out[0..n];
}

/// Total decode: torn (bad digest, short, geometry) / future-version /
/// malformed (unsorted or overlapping extents, an extent past `block_size`),
/// each counted and ignored loudly by the recovery scan.
pub fn decode(bytes: []const u8, block_size: u32) DecodeError!Decoded {
    if (bytes.len < RECORD_HEADER_LEN) return torn();
    const h: *align(1) const ExtentRecordWire = @ptrCast(bytes.ptr);
    if (!std.mem.eql(u8, &h.magic, &RECORD_MAGIC)) return torn();
    const version = std.mem.readInt(u32, &h.version_le, .little);
    if (version > EXTENT_RECORD_VERSION) {
        stats.extent_records_future_refused.inc();
        return error.FutureVersion;
    }
    if (version != EXTENT_RECORD_VERSION) return torn();
    const count = std.mem.readInt(u32, &h.count_le, .little);
    // At most one run per touched page.
    if (count > MAX_PAGES) return torn();
    const table_len = @as(usize, count) * RECORD_ENTRY_LEN;
    if (bytes.len < RECORD_HEADER_LEN + table_len) return torn();
    var payload_len: u64 = 0;
    var prev_end: u32 = 0;
    var i: usize = 0;
    while (i < count) : (i += 1) {
        const off = RECORD_HEADER_LEN + i * RECORD_ENTRY_LEN;
        const start = std.mem.readInt(u32, bytes[off..][0..4], .little);
        const len = std.mem.readInt(u32, bytes[off + 4 ..][0..4], .little);
        if (len == 0 or start < prev_end or (i != 0 and start == prev_end) or @as(u64, start) + len > block_size) {
            stats.extent_records_torn_discarded.inc();
            return error.Malformed;
        }
        prev_end = start + len;
        payload_len += len;
    }
    const total = RECORD_HEADER_LEN + table_len + payload_len;
    if (bytes.len < total) return torn();
    if (!checksum.verify(bytes[0..@intCast(total)], @offsetOf(ExtentRecordWire, "xxh3_le"))) return torn();
    const flags = std.mem.readInt(u32, &h.flags_le, .little);
    if (flags & ~RECORD_FLAG_BASE_DEFERRED != 0 or !std.mem.allEqual(u8, &h.reserved_le, 0)) return torn();
    stats.extent_records_recovered.inc();
    return .{
        .fencing = std.mem.readInt(u64, &h.fencing_le, .little),
        .block = std.mem.readInt(u32, &h.block_le, .little),
        .base_deferred = flags & RECORD_FLAG_BASE_DEFERRED != 0,
        .table = @as([*]align(1) const [2]u32, @ptrCast(bytes.ptr + RECORD_HEADER_LEN))[0..count],
        .payloads = bytes[RECORD_HEADER_LEN + table_len .. @intCast(total)],
    };
}

fn torn() DecodeError {
    stats.extent_records_torn_discarded.inc();
    return error.Torn;
}

// ---------------------------------------------------------------------------
// The whole-block fold (the CoW arm's composition).
// ---------------------------------------------------------------------------

/// Seed base in authority order: the caller resolved it (a staged FULL
/// sibling > the durable block image — only when the complement is owed —
/// > zeros for a hole, NO seed read); apply the record's extents (older)
/// then the RAM pages (newest) into `out` (the whole block). Returns k.
pub fn fold(seed: ?[]const u8, record: ?Decoded, x: *const ExtentOverlay, out: []u8) u64 {
    if (seed) |s| {
        @memcpy(out, s[0..out.len]);
        stats.fold_seed_reads.inc();
    } else {
        @memset(out, 0);
    }
    var k: u64 = 0;
    if (record) |r| {
        r.applyTo(out);
        k += r.table.len;
    }
    x.layInto(out);
    k += x.extents;
    noteFold(k);
    return k;
}

/// Every fold's ledger: the pass, its fill, the writes folded.
pub fn noteFold(k: u64) void {
    stats.fold_passes.inc();
    stats.fold_fill.record(k);
    stats.fold_extents_folded.add(k);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const BS: u32 = 64 * PAGE;

test "derivations (tie, §J.1): small = len×4 < bs; the fold trigger is block/4P pages (256 at 4 MiB, floor 1); pages(e, a) = 1 + ⌊(e + P − a − 1)/P⌋; in-place bound 8× for 512 B sector-aligned, 16× arbitrary, 2× for an unaligned 4 KiB, 1× page-aligned; a hole block's bound is block/u_flush (2× at half fill)" {
    try testing.expect(isSmall(PAGE, BS));
    try testing.expect(!isSmall(BS / 4, BS));
    try testing.expectEqual(@as(u32, 1024), pagesPerBlock(4 << 20));
    try testing.expectEqual(@as(u32, 256), foldPagesDerived(4 << 20));
    try testing.expectEqual(@as(u32, 16), foldPagesDerived(BS));
    try testing.expectEqual(@as(u32, 1), foldPagesDerived(4096));
    try testing.expect(!foldDue(255, 4 << 20) and foldDue(256, 4 << 20));
    try testing.expectEqual(@as(u64, 1), pagesWorst(512, 512));
    try testing.expectEqual(@as(u64, 2), pagesWorst(512, 1));
    try testing.expectEqual(@as(u64, 2), pagesWorst(1536, 512));
    try testing.expectEqual(@as(u64, 1), pagesWorst(4096, 4096));
    try testing.expectEqual(@as(u64, 2), pagesWorst(4096, 1));
    try testing.expectEqual(@as(u64, 1), pagesWorst(1, 1));
    try testing.expectEqual(@as(u64, 8_000_000), inPlaceBoundPpm(512, 512));
    try testing.expectEqual(@as(u64, 16_000_000), inPlaceBoundPpm(512, 1));
    try testing.expectEqual(@as(u64, 2_000_000), inPlaceBoundPpm(4096, 1));
    try testing.expectEqual(@as(u64, 1_000_000), inPlaceBoundPpm(4096, 4096));
    try testing.expectEqual(@as(u64, 2_000_000), holeBoundPpm(4 << 20, 2 << 20));
    try testing.expectEqual(@as(u64, 8192_000_000), holeBoundPpm(4 << 20, 512));
}

test "small_nonadjacent_write_parks_compactly: a 512 B write parks ONE content-valid page (zeros around it on a hole), the budget charge is pages × PAGE_CHARGE, pages stay sorted and coalesce into runs, coveredRead zero-fills untouched pages, the page set is exact, completeness is every page" {
    var x = ExtentOverlay.init(false);
    defer x.deinit(testing.allocator);
    const g0 = stats.parked_extent_bytes.load();
    var a: [512]u8 = @splat(1);
    var b: [PAGE]u8 = @splat(2);
    try x.merge(testing.allocator, 10 * PAGE + 1024, &a, null);
    try testing.expectEqual(@as(usize, 1), x.pageCount());
    try testing.expectEqual(@as(usize, 1), x.count());
    try testing.expectEqual(@as(u64, 512), x.bytes);
    try testing.expectEqual(g0 + PAGE_CHARGE, stats.parked_extent_bytes.load());
    try x.merge(testing.allocator, 2 * PAGE, &b, null);
    try x.merge(testing.allocator, 3 * PAGE + 100, a[0..100], null);
    try testing.expectEqual(@as(usize, 3), x.pageCount());
    try testing.expectEqual(@as(u32, 2), x.pages.items[0].idx);
    try testing.expectEqual(@as(u32, 3), x.pages.items[1].idx);
    try testing.expectEqual(@as(u32, 10), x.pages.items[2].idx);
    var runs: [MAX_PAGES]Run = undefined;
    const rs = x.pageRuns(&runs);
    try testing.expectEqual(@as(usize, 2), rs.len);
    try testing.expectEqual(Run{ .start = 2 * PAGE, .end = 4 * PAGE }, rs[0]);
    try testing.expectEqual(Run{ .start = 10 * PAGE, .end = 11 * PAGE }, rs[1]);
    try testing.expectEqual(@as(usize, 2), x.pageRunCount());
    // A straddling write touches two pages; a re-write counts in bytes again.
    var wide: [PAGE]u8 = @splat(9);
    try x.merge(testing.allocator, 5 * PAGE + 2048, &wide, null);
    try testing.expectEqual(@as(usize, 5), x.pageCount());
    try testing.expectEqual(@as(u64, 512 + PAGE + 100 + PAGE), x.bytes);
    try testing.expectEqual(g0 + 5 * PAGE_CHARGE, stats.parked_extent_bytes.load());
    var out: [12 * PAGE]u8 = undefined;
    x.coveredRead(0, &out);
    try testing.expect(std.mem.allEqual(u8, out[0 .. 2 * PAGE], 0));
    try testing.expectEqual(@as(u8, 2), out[2 * PAGE + 50]);
    try testing.expectEqual(@as(u8, 0), out[3 * PAGE + 99]); // the hole inside a touched page is zeros
    try testing.expectEqual(@as(u8, 1), out[3 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 9), out[5 * PAGE + 2048]);
    try testing.expectEqual(@as(u8, 9), out[6 * PAGE + 2047]);
    try testing.expectEqual(@as(u8, 0), out[6 * PAGE + 2048]);
    try testing.expectEqual(@as(u8, 1), out[10 * PAGE + 1024]);
    try testing.expectEqual(@as(u8, 0), out[10 * PAGE + 1023]);
    const set = x.pageSet();
    try testing.expect(set.isSet(2) and set.isSet(3) and set.isSet(5) and set.isSet(6) and set.isSet(10));
    try testing.expect(!set.isSet(4) and !set.isSet(0));
    try testing.expectEqual(@as(usize, 5), set.count());
    try testing.expect(!x.isComplete(BS));
    var laid: [BS]u8 = @splat(0xEE);
    x.layInto(&laid);
    try testing.expectEqual(@as(u8, 0xEE), laid[0]);
    try testing.expectEqual(@as(u8, 2), laid[2 * PAGE]);
    x.completeInto(&laid, null);
    try testing.expectEqual(@as(u8, 0), laid[0]);
    try testing.expectEqual(@as(u8, 9), laid[6 * PAGE]);
}

const FakeBase = struct {
    fill: u8,
    reads: u32 = 0,
    fail: bool = false,

    fn read(ctx: ?*anyopaque, idx: u32, dst: []u8) seams.ReadError!void {
        const self: *FakeBase = @ptrCast(@alignCast(ctx.?));
        if (self.fail) return error.InputOutput;
        self.reads += 1;
        @memset(dst, self.fill +% @as(u8, @intCast(idx)));
    }

    fn seeder(self: *FakeBase) Seeder {
        return .{ .ctx = self, .read = &read };
    }
};

test "a deferred overlay seeds each page from the base ONCE at first touch (the seed at the page grain: reads == pages, never a whole-block image); a seed that fails records nothing of the write; the page then holds base + user bytes" {
    var base: FakeBase = .{ .fill = 0x40 };
    var x = ExtentOverlay.init(true);
    defer x.deinit(testing.allocator);
    const r0 = stats.extent_page_seed_reads.load();
    const b0 = stats.extent_page_seed_bytes.load();
    var a: [512]u8 = @splat(0xAA);
    try x.merge(testing.allocator, 7 * PAGE + 1000, &a, base.seeder());
    try x.merge(testing.allocator, 7 * PAGE + 3000, &a, base.seeder()); // the same page: no second seed
    try testing.expectEqual(@as(u32, 1), base.reads);
    try testing.expectEqual(r0 + 1, stats.extent_page_seed_reads.load());
    try testing.expectEqual(b0 + PAGE, stats.extent_page_seed_bytes.load());
    var out: [PAGE]u8 = undefined;
    x.coveredRead(7 * PAGE, &out);
    try testing.expectEqual(@as(u8, 0x47), out[0]);
    try testing.expectEqual(@as(u8, 0xAA), out[1000]);
    try testing.expectEqual(@as(u8, 0x47), out[1512]);
    try testing.expectEqual(@as(u8, 0xAA), out[3000]);
    // A failed seed: the write lands nothing, the overlay is unchanged.
    base.fail = true;
    try testing.expectError(error.InputOutput, x.merge(testing.allocator, 9 * PAGE, &a, base.seeder()));
    try testing.expectEqual(@as(usize, 1), x.pageCount());
    try testing.expectEqual(@as(u64, 1024), x.bytes);
    try testing.expectEqual(@as(usize, 2), x.count());
    // Completeness with a base: untouched pages from it.
    var img: [BS]u8 = @splat(0x11);
    var laid: [BS]u8 = undefined;
    x.completeInto(&laid, &img);
    try testing.expectEqual(@as(u8, 0x11), laid[0]);
    try testing.expectEqual(@as(u8, 0x47), laid[7 * PAGE]);
    try testing.expectEqual(@as(u8, 0xAA), laid[7 * PAGE + 1000]);
}

test "spill_writes_extent_records_without_seed_reads: the record round-trips the touched pages as runs, every refusal class is detected (torn, future, malformed), the fold seeds once and applies record extents then pages newest-wins" {
    var x = ExtentOverlay.init(true);
    defer x.deinit(testing.allocator);
    var a: [PAGE]u8 = @splat(0xA1);
    var b: [2 * PAGE]u8 = @splat(0xB2);
    try x.merge(testing.allocator, 5 * PAGE, &a, null);
    try x.merge(testing.allocator, 20 * PAGE, &b, null);
    const buf = try testing.allocator.alloc(u8, encodedLen(&x));
    defer testing.allocator.free(buf);
    const rec = encode(&x, 77, 3, buf);
    try testing.expectEqual(RECORD_HEADER_LEN + 2 * RECORD_ENTRY_LEN + 3 * PAGE, rec.len);
    const d = try decode(rec, BS);
    try testing.expectEqual(@as(u64, 77), d.fencing);
    try testing.expectEqual(@as(u32, 3), d.block);
    try testing.expect(d.base_deferred);
    try testing.expectEqual(@as(usize, 2), d.table.len);
    try testing.expectEqual([2]u32{ 5 * PAGE, PAGE }, d.table[0]);
    try testing.expectEqual([2]u32{ 20 * PAGE, 2 * PAGE }, d.table[1]);
    // Torn: a flipped payload byte.
    rec[rec.len - 1] ^= 1;
    try testing.expectError(error.Torn, decode(rec, BS));
    rec[rec.len - 1] ^= 1;
    // Future version refuses loud.
    std.mem.writeInt(u32, rec[4..8], EXTENT_RECORD_VERSION + 1, .little);
    try testing.expectError(error.FutureVersion, decode(rec, BS));
    std.mem.writeInt(u32, rec[4..8], EXTENT_RECORD_VERSION, .little);
    // Malformed: an extent past the block (the checksum is re-stamped so geometry is what refuses).
    std.mem.writeInt(u32, rec[RECORD_HEADER_LEN..][0..4], BS - 10, .little);
    checksum.stamp(rec, 32);
    try testing.expectError(error.Malformed, decode(rec, BS));
    try testing.expectError(error.Torn, decode(rec[0..20], BS));
    // The fold: seed once (an old image), record extents (older), then RAM
    // pages (newest). A page touched over a spilled record is seeded through
    // the record (base + record: the record's bytes are newer than the base),
    // so the page's untouched bytes carry the record's, never the base's.
    std.mem.writeInt(u32, rec[RECORD_HEADER_LEN..][0..4], 5 * PAGE, .little);
    checksum.stamp(rec, 32);
    const d2 = try decode(rec, BS);
    var seed: [BS]u8 = @splat(0x11);
    var composed: [BS]u8 = seed;
    d2.applyTo(&composed);
    const Composed = struct {
        fn read(ctx: ?*anyopaque, idx: u32, dst: []u8) seams.ReadError!void {
            const img: *const [BS]u8 = @ptrCast(@alignCast(ctx.?));
            @memcpy(dst, img[idx * PAGE ..][0..PAGE]);
        }
    };
    var newer = ExtentOverlay.init(true);
    defer newer.deinit(testing.allocator);
    var c: [PAGE]u8 = @splat(0xC3);
    try newer.merge(testing.allocator, 5 * PAGE + 100, &c, .{ .ctx = &composed, .read = &Composed.read }); // overlaps the record's first extent: newest wins
    var out: [BS]u8 = undefined;
    const reads0 = stats.fold_seed_reads.load();
    const k = fold(&seed, d2, &newer, &out);
    try testing.expectEqual(@as(u64, 3), k);
    try testing.expectEqual(reads0 + 1, stats.fold_seed_reads.load());
    try testing.expectEqual(@as(u8, 0x11), out[0]);
    try testing.expectEqual(@as(u8, 0xA1), out[5 * PAGE + 50]); // the record's bytes under the page, through the seed
    try testing.expectEqual(@as(u8, 0xC3), out[5 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0xC3), out[6 * PAGE + 99]);
    try testing.expectEqual(@as(u8, 0x11), out[6 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0x11), out[7 * PAGE]);
    try testing.expectEqual(@as(u8, 0xB2), out[21 * PAGE]);
    // A hole folds with NO seed read (G-RW6).
    const k2 = fold(null, null, &newer, &out);
    try testing.expectEqual(@as(u64, 1), k2);
    try testing.expectEqual(reads0 + 1, stats.fold_seed_reads.load());
    try testing.expectEqual(@as(u8, 0), out[0]);
}

fn decodeTotalFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [RECORD_HEADER_LEN + 8 * RECORD_ENTRY_LEN + 4 * PAGE]u8 = undefined;
    const len = smith.slice(&buf);
    _ = decode(buf[0..len], BS) catch return;
}

test "fuzz: the spill record decoder never panics on arbitrary bytes" {
    try fuzzkit.fuzz({}, decodeTotalFuzz, .{});
}
