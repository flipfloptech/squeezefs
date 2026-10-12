//! W2 — the extent overlay, arm F of the write admission (charter §6.2
//! item 4; `docs/porting/data_write_path.md` §6; decision 0003 §J). For
//! sub-cap patch-ineligible shapes (a transformed volume, a shared block, a
//! hole, an unaligned write): a compact PAGE-INDEXED overlay charged to the
//! budget as `parked_extent_bytes` — a 512 B write parks one 4 KiB page, not
//! a 4 MiB block, and does NO I/O. A hole block's page is zeros (content-
//! valid at birth); a mapped passthrough block's page carries a byte MASK
//! until its base arrives — ONE ranged read per contiguous run of unseeded
//! pages at the fold (off the write's latency path; the seed at the page
//! grain, never a whole-block image), or page by page when the block is
//! READ before it folds. Coverage is then page-granular and EXACT (a 128 B
//! page set over the format's largest block), so a reader composing the
//! window never meets a run table that silently dropped runs — "uncovered
//! bytes never escape" holds by construction (a masked page serves, lays
//! and spills only its covered bytes).
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
    /// The base's bytes under a deferred overlay's touched pages: one
    /// ranged read per run of unseeded pages at the fold, one per page at a
    /// read of the block — the seed at the page grain.
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

/// One touched page. `mask == null` ⇒ content-valid at every byte (a hole
/// block's zero page; a deferred page after its base arrived). With a mask —
/// a deferred overlay's page before its base arrived — only the bytes whose
/// bit is set are the user's; the rest is uninitialized and never served,
/// laid or spilled. The base arrives at the FOLD (one ranged read per run
/// of unseeded pages, off the write's latency path) or at a READ of the
/// block (`seedPagewise`), never on the write path: W2 writes do no I/O.
pub const Page = struct {
    idx: u32,
    data: []u8,
    /// One bit per byte of `data`.
    mask: ?*Mask,
    /// Bytes set in `mask` (the page is content-valid at PAGE).
    covered: u32,
};

/// The base's bytes at `[rel, rel + dst.len)` of the block (block-relative;
/// a passthrough mapping's device bytes — the only base a page overlay has).
pub const BaseReader = struct {
    ctx: ?*anyopaque,
    read: *const fn (ctx: ?*anyopaque, rel: u32, dst: []u8) seams.ReadError!void,
};

/// What one page costs the budget: its buffer and its struct (+ the mask
/// while it carries one).
pub const PAGE_CHARGE: u64 = PAGE + @sizeOf(Page);
pub const MASK_CHARGE: u64 = PAGE / BYTE_BITS;
/// The mask is one bit per byte of the page.
const BYTE_BITS: u32 = 8;
const Mask = [PAGE / BYTE_BITS]u8;
/// A mask byte whose eight bytes are all covered.
const MASK_FULL: u8 = 0xFF;

pub const ExtentOverlay = struct {
    /// Sorted by `idx`.
    pages: std.ArrayList(Page) = .empty,
    /// Item B: existing device data under the extents (the untouched pages
    /// come from the base; a touched page's uncovered bytes do too, at the
    /// seed).
    base_deferred: bool,
    /// User bytes merged (a re-write of the same bytes counts again: the
    /// bound's denominator is what the user WROTE, which only brings the
    /// fold earlier, never later).
    bytes: u64 = 0,
    /// Writes merged (`fold_fill`'s k).
    extents: u32 = 0,
    /// Pages still carrying a mask.
    unseeded: u32 = 0,

    pub fn init(base_deferred: bool) ExtentOverlay {
        return .{ .base_deferred = base_deferred };
    }

    pub fn deinit(self: *ExtentOverlay, gpa: Allocator) void {
        for (self.pages.items) |p| {
            gpa.free(p.data);
            if (p.mask) |m| gpa.destroy(m);
        }
        stats.parked_extent_bytes.sub(self.chargedBytes());
        self.pages.deinit(gpa);
        self.* = undefined;
    }

    pub fn count(self: *const ExtentOverlay) usize {
        return self.extents;
    }

    pub fn pageCount(self: *const ExtentOverlay) usize {
        return self.pages.items.len;
    }

    pub fn hasUnseeded(self: *const ExtentOverlay) bool {
        return self.unseeded != 0;
    }

    /// The RAM the overlay holds (the budget charge).
    pub fn chargedBytes(self: *const ExtentOverlay) u64 {
        return self.pages.items.len * PAGE_CHARGE + @as(u64, self.unseeded) * MASK_CHARGE;
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

    /// Set bits `[lo, hi)` of a mask; answers how many were clear.
    fn setBits(m: *Mask, lo: u32, hi: u32) u32 {
        var newly: u32 = 0;
        var b = lo;
        while (b < hi) {
            if (b % BYTE_BITS == 0 and hi - b >= BYTE_BITS) {
                newly += @popCount(~m[b / BYTE_BITS]);
                m[b / BYTE_BITS] = MASK_FULL;
                b += BYTE_BITS;
            } else {
                const bit: u8 = @as(u8, 1) << @intCast(b % BYTE_BITS);
                if (m[b / BYTE_BITS] & bit == 0) {
                    m[b / BYTE_BITS] |= bit;
                    newly += 1;
                }
                b += 1;
            }
        }
        return newly;
    }

    fn bitSet(m: *const Mask, b: u32) bool {
        return m[b / BYTE_BITS] & (@as(u8, 1) << @intCast(b % BYTE_BITS)) != 0;
    }

    /// Merge `[rel, rel + data.len)`: every page it touches is found or
    /// created — zeros (content-valid) on a hole block, an uninitialized
    /// page under a mask on a deferred one — then the bytes land and the
    /// mask records them. No I/O.
    pub fn merge(self: *ExtentOverlay, gpa: Allocator, rel: u32, data: []const u8) Allocator.Error!void {
        assert(data.len != 0);
        const end: u32 = rel + @as(u32, @intCast(data.len));
        var pos: u32 = rel;
        while (pos < end) {
            const pidx = pos / PAGE;
            const in_page = pos % PAGE;
            const take: u32 = @min(PAGE - in_page, end - pos);
            const f = self.find(pidx);
            if (!f.found) {
                const buf = try gpa.alloc(u8, PAGE);
                errdefer gpa.free(buf);
                var mask: ?*Mask = null;
                if (self.base_deferred) {
                    const m = try gpa.create(Mask);
                    m.* = @splat(0);
                    mask = m;
                } else {
                    @memset(buf, 0);
                }
                errdefer if (mask) |m| gpa.destroy(m);
                try self.pages.insert(gpa, f.pos, .{ .idx = pidx, .data = buf, .mask = mask, .covered = 0 });
                stats.parked_extent_bytes.add(PAGE_CHARGE);
                if (mask != null) {
                    self.unseeded += 1;
                    stats.parked_extent_bytes.add(MASK_CHARGE);
                }
            }
            const p = &self.pages.items[self.find(pidx).pos];
            @memcpy(p.data[in_page .. in_page + take], data[pos - rel .. pos - rel + take]);
            if (p.mask) |m| {
                p.covered += setBits(m, in_page, in_page + take);
                if (p.covered == PAGE) self.unmask(gpa, p);
            }
            pos += take;
        }
        self.bytes += data.len;
        self.extents += 1;
    }

    /// The page is content-valid: the mask leaves (and its charge).
    fn unmask(self: *ExtentOverlay, gpa: Allocator, p: *Page) void {
        gpa.destroy(p.mask.?);
        p.mask = null;
        self.unseeded -= 1;
        stats.parked_extent_bytes.sub(MASK_CHARGE);
    }

    /// Fill a masked page's uncovered bytes from `base` (its PAGE bytes).
    fn fillPage(p: *Page, base: []const u8) void {
        const m = p.mask.?;
        var b: u32 = 0;
        while (b < PAGE) : (b += BYTE_BITS) {
            const byte = m[b / BYTE_BITS];
            if (byte == MASK_FULL) continue;
            if (byte == 0) {
                @memcpy(p.data[b .. b + BYTE_BITS], base[b .. b + BYTE_BITS]);
                continue;
            }
            var i: u32 = 0;
            while (i < BYTE_BITS) : (i += 1) if (!bitSet(m, b + i)) {
                p.data[b + i] = base[b + i];
            };
        }
    }

    /// The seed, page by page (a READ of the block: one base read per
    /// unseeded page from a 4 KiB stack buffer, no allocation). After it
    /// every touched page is content-valid.
    pub fn seedPagewise(self: *ExtentOverlay, gpa: Allocator, base: BaseReader) seams.ReadError!void {
        if (self.unseeded == 0) return;
        var buf: [PAGE]u8 = undefined;
        for (self.pages.items) |*p| {
            if (p.mask == null) continue;
            try base.read(base.ctx, p.idx * PAGE, &buf);
            stats.extent_page_seed_reads.inc();
            stats.extent_page_seed_bytes.add(PAGE);
            fillPage(p, &buf);
            self.unmask(gpa, p);
        }
    }

    /// The seed at the fold: ONE ranged base read per contiguous run of
    /// unseeded pages into `scratch` (≥ the block), then the pages fill.
    pub fn seedRuns(self: *ExtentOverlay, gpa: Allocator, base: BaseReader, scratch: []u8) seams.ReadError!void {
        if (self.unseeded == 0) return;
        var i: usize = 0;
        while (i < self.pages.items.len) {
            if (self.pages.items[i].mask == null) {
                i += 1;
                continue;
            }
            var j = i;
            while (j + 1 < self.pages.items.len and self.pages.items[j + 1].mask != null and self.pages.items[j + 1].idx == self.pages.items[j].idx + 1) : (j += 1) {}
            const start = self.pages.items[i].idx * PAGE;
            const len = (j - i + 1) * PAGE;
            try base.read(base.ctx, start, scratch[start .. start + len]);
            stats.extent_page_seed_reads.inc();
            stats.extent_page_seed_bytes.add(len);
            var k = i;
            while (k <= j) : (k += 1) {
                const p = &self.pages.items[k];
                fillPage(p, scratch[p.idx * PAGE ..][0..PAGE]);
                self.unmask(gpa, p);
            }
            i = j + 1;
        }
    }

    /// Every touched page as a set (the read seam's exact coverage, once
    /// seeded).
    pub fn pageSet(self: *const ExtentOverlay) PageSet {
        var s = PageSet.empty;
        for (self.pages.items) |p| s.set(p.idx);
        return s;
    }

    /// Every page of the block is touched.
    pub fn isComplete(self: *const ExtentOverlay, block_size: u64) bool {
        return self.pages.items.len == pagesPerBlock(block_size);
    }

    /// Lay every touched page into a block buffer — a content-valid page
    /// whole, a masked page's covered bytes only (the escalation; the
    /// untouched bytes are the caller's — zeros or the base).
    pub fn layInto(self: *const ExtentOverlay, dst: []u8) void {
        for (self.pages.items) |p| {
            const at = dst[p.idx * PAGE ..][0..PAGE];
            if (p.mask) |m| {
                var b: u32 = 0;
                while (b < PAGE) : (b += BYTE_BITS) {
                    const byte = m[b / BYTE_BITS];
                    if (byte == 0) continue;
                    if (byte == MASK_FULL) {
                        @memcpy(at[b .. b + BYTE_BITS], p.data[b .. b + BYTE_BITS]);
                        continue;
                    }
                    var i: u32 = 0;
                    while (i < BYTE_BITS) : (i += 1) if (bitSet(m, b + i)) {
                        at[b + i] = p.data[b + i];
                    };
                }
            } else @memcpy(at, p.data);
        }
    }

    /// The whole block: touched pages from the overlay, the rest from
    /// `base` (null = zeros).
    pub fn completeInto(self: *const ExtentOverlay, dst: []u8, base: ?[]const u8) void {
        if (base) |b| @memcpy(dst, b[0..dst.len]) else @memset(dst, 0);
        self.layInto(dst);
    }

    /// `[rel, rel + out.len)`: touched pages' bytes (a masked page's covered
    /// bytes only), zeros between.
    pub fn coveredRead(self: *const ExtentOverlay, rel: u32, out: []u8) void {
        @memset(out, 0);
        const end: u32 = rel + @as(u32, @intCast(out.len));
        for (self.pages.items) |p| {
            const ps = p.idx * PAGE;
            const a = @max(ps, rel);
            const b = @min(ps + PAGE, end);
            if (a >= b) continue;
            if (p.mask) |m| {
                var i = a;
                while (i < b) : (i += 1) if (bitSet(m, i - ps)) {
                    out[i - rel] = p.data[i - ps];
                };
            } else @memcpy(out[a - rel .. b - rel], p.data[a - ps .. b - ps]);
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

    /// The covered byte runs of the whole overlay in order (a content-valid
    /// page is one run; a masked page its set-bit runs), coalesced across
    /// abutting neighbours — the spill record's table.
    pub fn coveredRuns(self: *const ExtentOverlay, gpa: Allocator) Allocator.Error!std.ArrayList(Run) {
        var out: std.ArrayList(Run) = .empty;
        errdefer out.deinit(gpa);
        for (self.pages.items) |p| {
            const ps = p.idx * PAGE;
            if (p.mask) |m| {
                var b: u32 = 0;
                while (b < PAGE) {
                    if (!bitSet(m, b)) {
                        b += 1;
                        continue;
                    }
                    var e = b;
                    while (e < PAGE and bitSet(m, e)) : (e += 1) {}
                    try pushRun(&out, gpa, ps + b, ps + e);
                    b = e;
                }
            } else try pushRun(&out, gpa, ps, ps + PAGE);
        }
        return out;
    }

    fn pushRun(out: *std.ArrayList(Run), gpa: Allocator, start: u32, end: u32) Allocator.Error!void {
        if (out.items.len != 0 and out.items[out.items.len - 1].end == start) {
            out.items[out.items.len - 1].end = end;
        } else try out.append(gpa, .{ .start = start, .end = end });
    }
};

// ---------------------------------------------------------------------------
// The spill record (versioned + checksummed; NO seed read at spill).
// ---------------------------------------------------------------------------

pub const RECORD_MAGIC = "SQZX".*;
pub const EXTENT_RECORD_VERSION: u32 = 2;
pub const RECORD_FLAG_BASE_DEFERRED: u32 = 1 << 0;

/// 40 B little-endian header; then `count` × `(start u32, len u32)`, then the
/// payloads concatenated. The hash64 covers header (field zeroed) ‖ table ‖
/// payloads.
pub const ExtentRecordWire = extern struct {
    magic: [4]u8,
    version_le: [4]u8,
    fencing_le: [8]u8,
    block_le: [4]u8,
    flags_le: [4]u8,
    count_le: [4]u8,
    reserved_le: [4]u8,
    csum_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = @sizeOf(ExtentRecordWire);
pub const RECORD_ENTRY_LEN: usize = 8;

comptime {
    assert(RECORD_HEADER_LEN == 40);
    assert(@offsetOf(ExtentRecordWire, "csum_le") == 32);
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

/// The record's length for `runs` (`coveredRuns`): header, table, payloads.
pub fn encodedLen(runs: []const Run) usize {
    var payload: usize = 0;
    for (runs) |r| payload += r.end - r.start;
    return RECORD_HEADER_LEN + runs.len * RECORD_ENTRY_LEN + payload;
}

/// Encode the overlay as a spill record into `out` (`≥ encodedLen(runs)`):
/// `runs` = `coveredRuns(x)` — every covered byte, nothing uninitialized —
/// its bytes in order.
pub fn encode(x: *const ExtentOverlay, runs: []const Run, fencing: u64, block: u32, out: []u8) []u8 {
    const n = encodedLen(runs);
    assert(out.len >= n);
    var h: ExtentRecordWire = undefined;
    h.magic = RECORD_MAGIC;
    std.mem.writeInt(u32, &h.version_le, EXTENT_RECORD_VERSION, .little);
    std.mem.writeInt(u64, &h.fencing_le, fencing, .little);
    std.mem.writeInt(u32, &h.block_le, block, .little);
    std.mem.writeInt(u32, &h.flags_le, if (x.base_deferred) RECORD_FLAG_BASE_DEFERRED else 0, .little);
    std.mem.writeInt(u32, &h.count_le, @intCast(runs.len), .little);
    h.reserved_le = @splat(0);
    h.csum_le = @splat(0);
    @memcpy(out[0..RECORD_HEADER_LEN], std.mem.asBytes(&h));
    var pos: usize = RECORD_HEADER_LEN;
    for (runs) |r| {
        std.mem.writeInt(u32, out[pos..][0..4], r.start, .little);
        std.mem.writeInt(u32, out[pos + 4 ..][0..4], r.end - r.start, .little);
        pos += RECORD_ENTRY_LEN;
    }
    var payload: u64 = 0;
    for (runs) |r| {
        // A run never straddles an uncovered byte, so its bytes are exactly
        // the overlay's at those offsets.
        x.coveredRead(r.start, out[pos .. pos + (r.end - r.start)]);
        pos += r.end - r.start;
        payload += r.end - r.start;
    }
    assert(pos == n);
    checksum.stamp(out[0..n], @offsetOf(ExtentRecordWire, "csum_le"));
    stats.extent_spills.inc();
    stats.extent_spill_bytes.add(payload);
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
    // Disjoint non-abutting runs: at most one per two bytes of the block.
    if (count > block_size / 2) return torn();
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
    if (!checksum.verify(bytes[0..@intCast(total)], @offsetOf(ExtentRecordWire, "csum_le"))) return torn();
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
    try x.merge(testing.allocator, 10 * PAGE + 1024, &a);
    try testing.expectEqual(@as(usize, 1), x.pageCount());
    try testing.expectEqual(@as(usize, 1), x.count());
    try testing.expectEqual(@as(u64, 512), x.bytes);
    try testing.expect(!x.hasUnseeded());
    try testing.expectEqual(g0 + PAGE_CHARGE, stats.parked_extent_bytes.load());
    try x.merge(testing.allocator, 2 * PAGE, &b);
    try x.merge(testing.allocator, 3 * PAGE + 100, a[0..100]);
    try testing.expectEqual(@as(usize, 3), x.pageCount());
    try testing.expectEqual(@as(u32, 2), x.pages.items[0].idx);
    try testing.expectEqual(@as(u32, 3), x.pages.items[1].idx);
    try testing.expectEqual(@as(u32, 10), x.pages.items[2].idx);
    var runs: [MAX_PAGES]Run = undefined;
    const rs = x.pageRuns(&runs);
    try testing.expectEqual(@as(usize, 2), rs.len);
    try testing.expectEqual(Run{ .start = 2 * PAGE, .end = 4 * PAGE }, rs[0]);
    try testing.expectEqual(Run{ .start = 10 * PAGE, .end = 11 * PAGE }, rs[1]);
    // A straddling write touches two pages; a re-write counts in bytes again.
    var wide: [PAGE]u8 = @splat(9);
    try x.merge(testing.allocator, 5 * PAGE + 2048, &wide);
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
    // The covered runs of a hole overlay are its page runs.
    var cr = try x.coveredRuns(testing.allocator);
    defer cr.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 3), cr.items.len);
    try testing.expectEqual(Run{ .start = 5 * PAGE, .end = 7 * PAGE }, cr.items[1]);
}

const FakeBase = struct {
    fill: u8,
    reads: u32 = 0,
    read_bytes: u64 = 0,
    fail: bool = false,

    fn read(ctx: ?*anyopaque, rel: u32, dst: []u8) seams.ReadError!void {
        const self: *FakeBase = @ptrCast(@alignCast(ctx.?));
        if (self.fail) return error.InputOutput;
        self.reads += 1;
        self.read_bytes += dst.len;
        for (dst, 0..) |*d, i| d.* = self.fill +% @as(u8, @intCast((rel + i) / PAGE));
    }

    fn reader(self: *FakeBase) BaseReader {
        return .{ .ctx = self, .read = &read };
    }
};

test "a deferred overlay's write does NO I/O: the page lands under a byte mask (uncovered bytes never served, laid or spilled — zeros to a reader of the window), the mask is charged and leaves when the page fills; the seed arrives at the fold (ONE ranged read per contiguous run of unseeded pages) or page by page at a read; a failed seed leaves the overlay as it was" {
    var base: FakeBase = .{ .fill = 0x40 };
    var x = ExtentOverlay.init(true);
    defer x.deinit(testing.allocator);
    const g0 = stats.parked_extent_bytes.load();
    var a: [512]u8 = @splat(0xAA);
    try x.merge(testing.allocator, 7 * PAGE + 1000, &a);
    try x.merge(testing.allocator, 7 * PAGE + 3000, &a);
    try testing.expectEqual(@as(u32, 0), base.reads); // nothing read on the write path
    try testing.expect(x.hasUnseeded());
    try testing.expectEqual(g0 + PAGE_CHARGE + MASK_CHARGE, stats.parked_extent_bytes.load());
    var out: [PAGE]u8 = undefined;
    x.coveredRead(7 * PAGE, &out);
    try testing.expectEqual(@as(u8, 0), out[0]); // uncovered: zeros, never the page's uninitialized bytes
    try testing.expectEqual(@as(u8, 0xAA), out[1000]);
    try testing.expectEqual(@as(u8, 0), out[1512]);
    try testing.expectEqual(@as(u8, 0xAA), out[3000]);
    var laid: [BS]u8 = @splat(0x11);
    x.layInto(&laid);
    try testing.expectEqual(@as(u8, 0x11), laid[7 * PAGE]); // uncovered untouched
    try testing.expectEqual(@as(u8, 0xAA), laid[7 * PAGE + 1000]);
    // The covered runs are byte-exact for a masked page.
    var cr = try x.coveredRuns(testing.allocator);
    try testing.expectEqual(@as(usize, 2), cr.items.len);
    try testing.expectEqual(Run{ .start = 7 * PAGE + 1000, .end = 7 * PAGE + 1512 }, cr.items[0]);
    cr.deinit(testing.allocator);
    // A page that fills completely drops its mask.
    var full: [PAGE]u8 = @splat(0xF0);
    try x.merge(testing.allocator, 9 * PAGE, &full);
    try testing.expect(x.pages.items[1].mask == null);
    try testing.expectEqual(@as(u32, 1), x.unseeded);
    // Pages 20, 21 (a run) and 30 unseeded: the fold's seed is 2 ranged reads (8 KiB + 4 KiB), page 9 none.
    try x.merge(testing.allocator, 20 * PAGE + 10, &a);
    try x.merge(testing.allocator, 21 * PAGE + 10, &a);
    try x.merge(testing.allocator, 30 * PAGE + 10, &a);
    try testing.expectEqual(@as(u32, 4), x.unseeded);
    // A failed seed: the overlay is unchanged.
    base.fail = true;
    const r0 = stats.extent_page_seed_reads.load();
    var scratch: [BS]u8 = undefined;
    try testing.expectError(error.InputOutput, x.seedRuns(testing.allocator, base.reader(), &scratch));
    try testing.expectEqual(@as(u32, 4), x.unseeded);
    base.fail = false;
    try x.seedRuns(testing.allocator, base.reader(), &scratch);
    try testing.expectEqual(@as(u32, 3), base.reads); // page 7; pages 20–21 as ONE read; page 30
    try testing.expectEqual(@as(u64, 4 * PAGE), base.read_bytes);
    try testing.expectEqual(r0 + 3, stats.extent_page_seed_reads.load());
    try testing.expect(!x.hasUnseeded());
    try testing.expectEqual(g0 + 5 * PAGE_CHARGE, stats.parked_extent_bytes.load()); // the masks' charge left
    x.coveredRead(7 * PAGE, &out);
    try testing.expectEqual(@as(u8, 0x47), out[0]); // the base's page 7 under the user's bytes
    try testing.expectEqual(@as(u8, 0xAA), out[1000]);
    try testing.expectEqual(@as(u8, 0x47), out[1512]);
    x.coveredRead(21 * PAGE, &out);
    try testing.expectEqual(@as(u8, 0x40 +% 21), out[0]);
    try testing.expectEqual(@as(u8, 0xAA), out[10]);
    // The page-wise seed (a read of the block): one read per unseeded page.
    var y = ExtentOverlay.init(true);
    defer y.deinit(testing.allocator);
    try y.merge(testing.allocator, 3 * PAGE + 5, &a);
    try y.merge(testing.allocator, 4 * PAGE + 5, &a);
    var base2: FakeBase = .{ .fill = 0x80 };
    try y.seedPagewise(testing.allocator, base2.reader());
    try testing.expectEqual(@as(u32, 2), base2.reads);
    try testing.expect(!y.hasUnseeded());
    var img: [BS]u8 = @splat(0x11);
    y.completeInto(&laid, &img);
    try testing.expectEqual(@as(u8, 0x11), laid[0]);
    try testing.expectEqual(@as(u8, 0x83), laid[3 * PAGE]);
    try testing.expectEqual(@as(u8, 0xAA), laid[3 * PAGE + 5]);
}

test "spill_writes_extent_records_without_seed_reads: the record round-trips exactly the covered bytes (a masked page's runs, a content-valid page whole), every refusal class is detected (torn, future, malformed), the fold seeds once and applies record extents then pages newest-wins" {
    var x = ExtentOverlay.init(true);
    defer x.deinit(testing.allocator);
    var a: [PAGE]u8 = @splat(0xA1);
    var b: [2 * PAGE]u8 = @splat(0xB2);
    var c: [100]u8 = @splat(0xC0);
    try x.merge(testing.allocator, 5 * PAGE, &a);
    try x.merge(testing.allocator, 20 * PAGE, &b);
    try x.merge(testing.allocator, 30 * PAGE + 50, &c); // a masked page: 100 covered bytes
    var runs = try x.coveredRuns(testing.allocator);
    defer runs.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 3), runs.items.len);
    const buf = try testing.allocator.alloc(u8, encodedLen(runs.items));
    defer testing.allocator.free(buf);
    const rec = encode(&x, runs.items, 77, 3, buf);
    try testing.expectEqual(RECORD_HEADER_LEN + 3 * RECORD_ENTRY_LEN + 3 * PAGE + 100, rec.len);
    const d = try decode(rec, BS);
    try testing.expectEqual(@as(u64, 77), d.fencing);
    try testing.expectEqual(@as(u32, 3), d.block);
    try testing.expect(d.base_deferred);
    try testing.expectEqual(@as(usize, 3), d.table.len);
    try testing.expectEqual([2]u32{ 5 * PAGE, PAGE }, d.table[0]);
    try testing.expectEqual([2]u32{ 20 * PAGE, 2 * PAGE }, d.table[1]);
    try testing.expectEqual([2]u32{ 30 * PAGE + 50, 100 }, d.table[2]);
    try testing.expect(std.mem.allEqual(u8, d.payloads[3 * PAGE ..], 0xC0));
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
        fn read(ctx: ?*anyopaque, rel: u32, dst: []u8) seams.ReadError!void {
            const img: *const [BS]u8 = @ptrCast(@alignCast(ctx.?));
            @memcpy(dst, img[rel..][0..dst.len]);
        }
    };
    var newer = ExtentOverlay.init(true);
    defer newer.deinit(testing.allocator);
    var cc: [PAGE]u8 = @splat(0xC3);
    try newer.merge(testing.allocator, 5 * PAGE + 100, &cc); // overlaps the record's first extent: newest wins
    try newer.seedPagewise(testing.allocator, .{ .ctx = &composed, .read = &Composed.read });
    var out: [BS]u8 = undefined;
    const reads0 = stats.fold_seed_reads.load();
    const k = fold(&seed, d2, &newer, &out);
    try testing.expectEqual(@as(u64, 4), k);
    try testing.expectEqual(reads0 + 1, stats.fold_seed_reads.load());
    try testing.expectEqual(@as(u8, 0x11), out[0]);
    try testing.expectEqual(@as(u8, 0xA1), out[5 * PAGE + 50]); // the record's bytes under the page, through the seed
    try testing.expectEqual(@as(u8, 0xC3), out[5 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0xC3), out[6 * PAGE + 99]);
    try testing.expectEqual(@as(u8, 0x11), out[6 * PAGE + 100]);
    try testing.expectEqual(@as(u8, 0x11), out[7 * PAGE]);
    try testing.expectEqual(@as(u8, 0xB2), out[21 * PAGE]);
    try testing.expectEqual(@as(u8, 0xC0), out[30 * PAGE + 60]);
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
