//! Bsets: sorted, xxh3-checksummed record batches — build / verify / binary
//! search / n-way merge — and the node-level `KBSF` frame they ride in
//! (charter §5.1 bset `{…}` framing; `docs/porting/kv_record_bset.md` §4–§5).
//!
//! A bset is the unit node appends and compaction outputs are written in.
//! Verification happens ONCE at parse: after `BsetView.parse` succeeds,
//! record access is infallible zero-copy slicing. Binary search compares raw
//! big-endian key bytes and never decodes a value.
//!
//! Every length and count in the header is a CLAIM validated against its
//! container before any byte is dereferenced and never an allocation
//! authority (design §9; the TEST-4 find below).

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const key = @import("key.zig");
const rec = @import("record.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Record = rec.Record;
const RecordRef = rec.RecordRef;
const RecordKind = rec.RecordKind;

// ---------------------------------------------------------------------------
// §4.1 Bset image.
// ---------------------------------------------------------------------------

/// ASCII "BSET" read as a little-endian u32: appears in ASCII order on disk,
/// the ONE magic convention of every unit (format v4; decision 0002 row 36).
pub const BSET_MAGIC: u32 = std.mem.readInt(u32, "BSET", .little);
pub const BSET_VERSION: u16 = 1;
pub const BSET_HEADER_LEN: usize = 32;
/// The checksum field: xxh3 over `[0..24) ‖ 8 × 0x00 ‖ [32..)`.
pub const BSET_CHECKSUM_OFF: usize = 24;

/// `magic u32 ‖ version u16 ‖ reserved u16 ‖ record_count u32 ‖ data_len u32
/// ‖ journal_seq_horizon u64 ‖ checksum u64`, all LE. No `node_seq_at_write`
/// and no padded length live here — those are the node FRAME's.
pub const BsetHeader = extern struct {
    magic_le: [4]u8,
    version_le: [2]u8,
    reserved_le: [2]u8,
    record_count_le: [4]u8,
    data_len_le: [4]u8,
    journal_seq_horizon_le: [8]u8,
    checksum_le: [8]u8,

    pub fn magic(self: *const BsetHeader) u32 {
        return std.mem.readInt(u32, &self.magic_le, .little);
    }
    pub fn version(self: *const BsetHeader) u16 {
        return std.mem.readInt(u16, &self.version_le, .little);
    }
    pub fn reserved(self: *const BsetHeader) u16 {
        return std.mem.readInt(u16, &self.reserved_le, .little);
    }
    pub fn recordCount(self: *const BsetHeader) u32 {
        return std.mem.readInt(u32, &self.record_count_le, .little);
    }
    pub fn dataLen(self: *const BsetHeader) u32 {
        return std.mem.readInt(u32, &self.data_len_le, .little);
    }
    pub fn journalSeqHorizon(self: *const BsetHeader) u64 {
        return std.mem.readInt(u64, &self.journal_seq_horizon_le, .little);
    }
    pub fn storedChecksum(self: *const BsetHeader) u64 {
        return std.mem.readInt(u64, &self.checksum_le, .little);
    }
};

comptime {
    assert(@sizeOf(BsetHeader) == BSET_HEADER_LEN);
    assert(@offsetOf(BsetHeader, "checksum_le") == BSET_CHECKSUM_OFF);
}

/// xxh3 over a bset image with the checksum field zeroed. Public so tests
/// and fuzzers can RESTAMP a doctored image: the digest is integrity, not
/// authentication, and the structural gates behind it must be reachable.
pub fn checksumImage(buf: []const u8) u64 {
    assert(buf.len >= BSET_HEADER_LEN);
    return checksum.xxh3FieldZeroed(buf, BSET_CHECKSUM_OFF);
}

pub fn stampChecksum(buf: []u8) void {
    assert(buf.len >= BSET_HEADER_LEN);
    checksum.stamp(buf, BSET_CHECKSUM_OFF);
}

/// Writer-side refusals: every one is a writer bug, not device corruption.
pub const BuildError = error{
    /// A freeze writes only dirty deltas; an empty bset is never built.
    EmptyBset,
    /// Adjacent records not strictly `(key, seq)` ascending (duplicates
    /// included).
    RecordsNotAscending,
    /// `data_len` does not fit its u32.
    BsetTooLarge,
} || Allocator.Error;

/// `(key asc, seq asc)` strict order between adjacent records.
fn keySeqLess(a: RecordRef, b: RecordRef) bool {
    return switch (key.order(a.key, b.key)) {
        .lt => true,
        .gt => false,
        .eq => a.seq < b.seq,
    };
}

/// Serialize `records` (strictly `(key, seq)` ascending, non-empty) into a
/// checksummed bset image. `journal_seq_horizon` is the newest journal seq
/// this bset covers.
pub fn buildBset(gpa: Allocator, records: []const RecordRef, journal_seq_horizon: u64) BuildError![]u8 {
    if (records.len == 0) return error.EmptyBset;
    var data_len: usize = 0;
    for (records, 0..) |r, i| {
        if (i > 0 and !keySeqLess(records[i - 1], r)) return error.RecordsNotAscending;
        data_len += r.encodedLen();
    }
    if (data_len > std.math.maxInt(u32)) return error.BsetTooLarge;
    const out = try gpa.alloc(u8, BSET_HEADER_LEN + data_len);
    var h: BsetHeader = undefined;
    std.mem.writeInt(u32, &h.magic_le, BSET_MAGIC, .little);
    std.mem.writeInt(u16, &h.version_le, BSET_VERSION, .little);
    std.mem.writeInt(u16, &h.reserved_le, 0, .little);
    std.mem.writeInt(u32, &h.record_count_le, @intCast(records.len), .little);
    std.mem.writeInt(u32, &h.data_len_le, @intCast(data_len), .little);
    std.mem.writeInt(u64, &h.journal_seq_horizon_le, journal_seq_horizon, .little);
    std.mem.writeInt(u64, &h.checksum_le, 0, .little);
    out[0..BSET_HEADER_LEN].* = std.mem.toBytes(h);
    var pos: usize = BSET_HEADER_LEN;
    for (records) |r| pos += r.encodeInto(out[pos..]).len;
    assert(pos == out.len);
    stampChecksum(out);
    return out;
}

// ---------------------------------------------------------------------------
// §4.2 BsetView — verify ONCE, then infallible slicing.
// ---------------------------------------------------------------------------

pub const ParseError = error{
    Corrupt,
    ChecksumMismatch,
} || Allocator.Error;

/// Pre-validated record geometry (offsets relative to the data area, so
/// they fit the u32 `data_len` by construction): `record(i)` is infallible
/// zero-copy slicing after parse.
pub const RecordMeta = struct {
    key_off: u32,
    key_len: u16,
    kind: RecordKind,
    seq: u64,
    val_off: u32,
    val_len: u32,
};

pub const Range = struct { lo: usize, hi: usize };

pub const BsetView = struct {
    buf: []const u8,
    journal_seq_horizon: u64,
    metas: []RecordMeta,

    /// Parse and verify. The buffer must be EXACTLY `32 + data_len` bytes
    /// (the node layer slices `bset_len` before parsing). Gate order:
    /// length → magic → version → reserved → exact length → checksum → the
    /// record-count bound → the record walk (ordering) → exact end.
    pub fn parse(gpa: Allocator, buf: []const u8) ParseError!BsetView {
        if (buf.len < BSET_HEADER_LEN) return error.Corrupt;
        const h = std.mem.bytesToValue(BsetHeader, buf[0..BSET_HEADER_LEN]);
        if (h.magic() != BSET_MAGIC) return error.Corrupt;
        if (h.version() != BSET_VERSION) return error.Corrupt;
        if (h.reserved() != 0) return error.Corrupt;
        const record_count: usize = h.recordCount();
        const data_len: usize = h.dataLen();
        if (buf.len != BSET_HEADER_LEN + data_len) return error.Corrupt;
        if (h.storedChecksum() != checksumImage(buf)) return error.ChecksumMismatch;
        // `record_count` is a CLAIM, never an allocation authority. TEST-4
        // find: the metadata vector was once sized straight from this u32 —
        // a 32-byte image claiming u32::MAX records with a digest that
        // verifies was a 128 GiB `with_capacity` abort, one bad sector
        // taking the daemon down inside a decoder whose contract is that
        // corrupt bytes are DETECTED. The bound is exact and free: the
        // smallest record is a header plus a one-byte key.
        const max_records = data_len / rec.RECORD_MIN_LEN;
        if (record_count > max_records) return error.Corrupt;
        const data = buf[BSET_HEADER_LEN..];
        const metas = try gpa.alloc(RecordMeta, record_count);
        errdefer gpa.free(metas);
        var pos: usize = 0;
        var prev: ?RecordRef = null;
        for (metas) |*m| {
            const d = try rec.RecordRef.decode(data[pos..]);
            if (prev) |p| if (!keySeqLess(p, d.rec)) return error.Corrupt;
            prev = d.rec;
            m.* = .{
                .key_off = @intCast(pos + rec.RECORD_HEADER_LEN),
                .key_len = @intCast(d.rec.key.len),
                .kind = d.rec.kind,
                .seq = d.rec.seq,
                .val_off = @intCast(pos + rec.RECORD_HEADER_LEN + d.rec.key.len),
                .val_len = @intCast(d.rec.value.len),
            };
            pos += d.used;
        }
        if (pos != data.len) return error.Corrupt;
        return .{ .buf = buf, .journal_seq_horizon = h.journalSeqHorizon(), .metas = metas };
    }

    pub fn deinit(self: *BsetView, gpa: Allocator) void {
        gpa.free(self.metas);
        self.* = undefined;
    }

    pub fn len(self: *const BsetView) usize {
        return self.metas.len;
    }

    pub fn isEmpty(self: *const BsetView) bool {
        return self.metas.len == 0;
    }

    fn keyAt(self: *const BsetView, m: RecordMeta) []const u8 {
        return self.buf[BSET_HEADER_LEN + m.key_off ..][0..m.key_len];
    }

    /// The `i`-th record (strict `(key, seq)` ascending). Asserts the index
    /// like slice indexing.
    pub fn record(self: *const BsetView, i: usize) RecordRef {
        const m = self.metas[i];
        return .{
            .key = self.keyAt(m),
            .seq = m.seq,
            .kind = m.kind,
            .value = self.buf[BSET_HEADER_LEN + m.val_off ..][0..m.val_len],
        };
    }

    const FindCtx = struct { view: *const BsetView, k: []const u8 };

    fn keyLess(ctx: FindCtx, m: RecordMeta) bool {
        return key.order(ctx.view.keyAt(m), ctx.k) == .lt;
    }

    fn keyLessEq(ctx: FindCtx, m: RecordMeta) bool {
        return key.order(ctx.view.keyAt(m), ctx.k) != .gt;
    }

    /// Binary search on raw key bytes — no decoding: the index range of
    /// records whose key equals `k` (seq-ascending for a multi-seq key;
    /// empty at the insertion point when absent).
    pub fn find(self: *const BsetView, k: []const u8) Range {
        const ctx: FindCtx = .{ .view = self, .k = k };
        return .{
            .lo = std.sort.partitionPoint(RecordMeta, self.metas, ctx, keyLess),
            .hi = std.sort.partitionPoint(RecordMeta, self.metas, ctx, keyLessEq),
        };
    }

    pub const Iter = struct {
        view: *const BsetView,
        next_idx: usize = 0,

        pub fn next(self: *Iter) ?RecordRef {
            if (self.next_idx >= self.view.len()) return null;
            defer self.next_idx += 1;
            return self.view.record(self.next_idx);
        }
    };

    pub fn iter(self: *const BsetView) Iter {
        return .{ .view = self };
    }
};

// ---------------------------------------------------------------------------
// §4.4 The n-way merge. `sources[0]` is the NEWEST: a node's sources are its
// frames in REVERSE append order; newness is positional, never by horizon
// or seq.
// ---------------------------------------------------------------------------

/// `run_ends` sentinel: the current run's end has not been scanned yet.
const RUN_END_UNKNOWN: usize = std.math.maxInt(usize);

/// Yields the record multiset ordered `(key asc, seq desc, source index
/// asc)`: each key's group contiguous and newest-first — exactly the fold's
/// input shape. Equal `(key, seq)` across sources yields the newer source
/// first and BOTH survive (the fold is idempotent over identical-effect
/// records). A same-key multi-seq run WITHIN one source (stored ascending)
/// is emitted in reverse.
pub const MergeIter = struct {
    sources: []const BsetView,
    /// Start of the current same-key run per source.
    cursors: []usize,
    /// How many of the current run have been emitted (runs emit in reverse).
    run_emitted: []usize,
    /// The current run's exclusive end per source, scanned ONCE when the
    /// cursor lands on a new key. Finding 49: the per-candidate rescan made
    /// a same-key run cost O(run²) decodes — a directory inode's frozen
    /// Δtime overlay of 35 k records folded in 44 s on the checkpoint task.
    run_ends: []usize,

    pub fn init(gpa: Allocator, sources: []const BsetView) Allocator.Error!MergeIter {
        const cursors = try gpa.alloc(usize, sources.len);
        errdefer gpa.free(cursors);
        const run_emitted = try gpa.alloc(usize, sources.len);
        errdefer gpa.free(run_emitted);
        const run_ends = try gpa.alloc(usize, sources.len);
        @memset(cursors, 0);
        @memset(run_emitted, 0);
        @memset(run_ends, RUN_END_UNKNOWN);
        return .{ .sources = sources, .cursors = cursors, .run_emitted = run_emitted, .run_ends = run_ends };
    }

    pub fn deinit(self: *MergeIter, gpa: Allocator) void {
        gpa.free(self.cursors);
        gpa.free(self.run_emitted);
        gpa.free(self.run_ends);
        self.* = undefined;
    }

    /// The exclusive end of the same-key run starting at `cursors[si]`,
    /// memoized per run.
    fn runEnd(self: *MergeIter, si: usize) usize {
        if (self.run_ends[si] != RUN_END_UNKNOWN) return self.run_ends[si];
        const view = &self.sources[si];
        const start = self.cursors[si];
        const k = view.record(start).key;
        var end = start + 1;
        while (end < view.len() and std.mem.eql(u8, view.record(end).key, k)) end += 1;
        self.run_ends[si] = end;
        return end;
    }

    /// This source's candidate: the highest un-emitted seq of its current
    /// key run (seq-ascending within a key ⇒ the run scanned from the back).
    fn candidate(self: *MergeIter, si: usize) ?RecordRef {
        if (self.cursors[si] >= self.sources[si].len()) return null;
        const idx = self.runEnd(si) - 1 - self.run_emitted[si];
        return self.sources[si].record(idx);
    }

    pub fn next(self: *MergeIter) ?RecordRef {
        // Linear best-candidate scan: a node's bset log is ~32–56 deep at
        // 256 KiB and the comparison is a memcmp on ≤ 29-byte keys.
        var best: ?RecordRef = null;
        var best_si: usize = 0;
        for (0..self.sources.len) |si| {
            const r = self.candidate(si) orelse continue;
            const better = if (best) |b| switch (key.order(r.key, b.key)) {
                .lt => true,
                .gt => false,
                // Same key: newer seq first; on an exact tie the earlier
                // (newer) source already holds `best`.
                .eq => r.seq > b.seq,
            } else true;
            if (better) {
                best = r;
                best_si = si;
            }
        }
        const r = best orelse return null;
        const run_end = self.runEnd(best_si);
        self.run_emitted[best_si] += 1;
        if (self.cursors[best_si] + self.run_emitted[best_si] == run_end) {
            self.cursors[best_si] = run_end;
            self.run_emitted[best_si] = 0;
            self.run_ends[best_si] = RUN_END_UNKNOWN;
        }
        return r;
    }
};

pub fn merge(gpa: Allocator, sources: []const BsetView) Allocator.Error!MergeIter {
    return MergeIter.init(gpa, sources);
}

const Match = struct { si: usize, rec: RecordRef };

/// `(seq desc, source asc)` — a total order over gathered matches: one
/// source never holds two records with one `(key, seq)`.
fn matchNewestFirst(_: void, a: Match, b: Match) bool {
    if (a.rec.seq != b.rec.seq) return a.rec.seq > b.rec.seq;
    return a.si < b.si;
}

/// Point lookup across sources (`sources[0]` newest): gather `k`'s records
/// from every view, order newest-first, and run THE fold. The gathered list
/// is per-call scratch; the node cache's pools own it on the hot path.
pub fn lookup(gpa: Allocator, sources: []const BsetView, k: []const u8) rec.FoldError!rec.Folded {
    var matches: std.ArrayList(Match) = .empty;
    defer matches.deinit(gpa);
    for (sources, 0..) |*view, si| {
        const range = view.find(k);
        for (range.lo..range.hi) |i| try matches.append(gpa, .{ .si = si, .rec = view.record(i) });
    }
    std.sort.pdq(Match, matches.items, {}, matchNewestFirst);
    var refs = try gpa.alloc(RecordRef, matches.items.len);
    defer gpa.free(refs);
    for (matches.items, 0..) |m, i| refs[i] = m.rec;
    return rec.foldNewestFirst(gpa, refs);
}

/// Compact sources into the folded record set per surviving key: merge →
/// group by key → `compactFold` under the tombstone-elision and lineage
/// rules. The output is strictly `(key, seq)`-ascending — directly
/// buildable into a fresh bset.
pub fn compact(gpa: Allocator, sources: []const BsetView, durable_tail: u64, out: *std.ArrayList(Record)) rec.FoldError!void {
    var it = try merge(gpa, sources);
    defer it.deinit(gpa);
    var group: std.ArrayList(RecordRef) = .empty;
    defer group.deinit(gpa);
    while (it.next()) |r| {
        if (group.items.len != 0 and !std.mem.eql(u8, group.items[0].key, r.key)) {
            try rec.compactFold(gpa, group.items, durable_tail, out);
            group.clearRetainingCapacity();
        }
        try group.append(gpa, r);
    }
    try rec.compactFold(gpa, group.items, durable_tail, out);
}

// ---------------------------------------------------------------------------
// §4.3 The node-level bset FRAME (`KBSF`): the shape a bset lands in at a
// node's 4 KiB-aligned tail. The node layer owns placement; this is the
// codec.
// ---------------------------------------------------------------------------

/// Header page size AND append granularity.
pub const NODE_PAGE: usize = 4096;
/// ASCII "KBSF" read as a little-endian u32: appears in ASCII order on disk.
pub const BSET_FRAME_MAGIC: u32 = std.mem.readInt(u32, "KBSF", .little);
/// The ONE frame (format v4; decision 0002 row 37): `magic u32 ‖ version
/// u16 ‖ reserved u16 (= 0, checked) ‖ node_seq_at_write u64 ‖ padded_len
/// u32 ‖ bset_len u32 ‖ appender_id u32 ‖ g u32 ‖ xxh3 u64` over `[0..32)`.
/// The fencing stamp is on EVERY frame — charter §5.5 "Detection-grade
/// frame screen on every bset frame"; `(0, 0)` is the manager's structural
/// stamp and the one-member forest's.
pub const BSET_FRAME_VERSION: u16 = 1;
pub const BSET_FRAME_LEN: usize = 40;
pub const BSET_FRAME_CHECKSUM_OFF: usize = 32;

pub const FrameHeaderWire = extern struct {
    magic_le: [4]u8,
    version_le: [2]u8,
    reserved_le: [2]u8,
    node_seq_at_write_le: [8]u8,
    padded_len_le: [4]u8,
    bset_len_le: [4]u8,
    appender_id_le: [4]u8,
    g_le: [4]u8,
    checksum_le: [8]u8,
};

comptime {
    assert(@sizeOf(FrameHeaderWire) == BSET_FRAME_LEN);
    assert(@offsetOf(FrameHeaderWire, "appender_id_le") == 24);
    assert(@offsetOf(FrameHeaderWire, "g_le") == 28);
    assert(@offsetOf(FrameHeaderWire, "checksum_le") == BSET_FRAME_CHECKSUM_OFF);
}

/// The fencing stamp: the writing appender and the SLOT's lease generation
/// at the write (a slot's generations are ONE sequence, which is why the
/// stamp is the slot's `g` and never the appender's dlm_term). `(0, 0)` is
/// the manager's structural stamp for tree 0, the native slot and every
/// unleased tree it maintains — and so the one-member forest's only stamp.
pub const FrameStamp = struct {
    appender_id: u32 = 0,
    g: u32 = 0,

    pub fn eql(a: FrameStamp, b: FrameStamp) bool {
        return a.appender_id == b.appender_id and a.g == b.g;
    }
};

/// Round up to the append granularity.
pub fn pageAlign(n: usize) usize {
    return (n + NODE_PAGE - 1) / NODE_PAGE * NODE_PAGE;
}

/// On-disk length of the frame `record_bytes` of encoded records produce:
/// frame + bset headers, padded to 4 KiB; 0 for no records.
pub fn appendFrameLen(record_bytes: usize) usize {
    if (record_bytes == 0) return 0;
    return pageAlign(BSET_FRAME_LEN + BSET_HEADER_LEN + record_bytes);
}

pub const FrameEncodeError = BuildError || error{
    /// A record value over the node's `recordValueCap`.
    ValueTooLarge,
};

/// The append/write wire format: `frame header ‖ bset image ‖ zero pad to a
/// 4 KiB multiple`. The pad is deliberately NOT covered by any checksum (a
/// torn append may truncate it harmlessly); the bset carries its own. Every
/// `value.len > recordValueCap(node_size)` refuses before anything is built.
pub fn encodeBsetFrame(gpa: Allocator, stamp: FrameStamp, node_size: usize, node_seq_at_write: u64, records: []const RecordRef, journal_seq_horizon: u64) FrameEncodeError![]u8 {
    const cap = rec.recordValueCap(node_size);
    for (records) |r| if (r.value.len > cap) return error.ValueTooLarge;
    const bset = try buildBset(gpa, records, journal_seq_horizon);
    defer gpa.free(bset);
    // `buildBset` bounded `data_len` by u32; `32 + data_len` still fits.
    const bset_len: u32 = @intCast(bset.len);
    const padded_len = pageAlign(BSET_FRAME_LEN + bset.len);
    if (padded_len > std.math.maxInt(u32)) return error.BsetTooLarge;
    const out = try gpa.alloc(u8, padded_len);
    @memset(out, 0);
    var h: FrameHeaderWire = undefined;
    std.mem.writeInt(u32, &h.magic_le, BSET_FRAME_MAGIC, .little);
    std.mem.writeInt(u16, &h.version_le, BSET_FRAME_VERSION, .little);
    std.mem.writeInt(u16, &h.reserved_le, 0, .little);
    std.mem.writeInt(u64, &h.node_seq_at_write_le, node_seq_at_write, .little);
    std.mem.writeInt(u32, &h.padded_len_le, @intCast(padded_len), .little);
    std.mem.writeInt(u32, &h.bset_len_le, bset_len, .little);
    std.mem.writeInt(u32, &h.appender_id_le, stamp.appender_id, .little);
    std.mem.writeInt(u32, &h.g_le, stamp.g, .little);
    // The checksum follows the bytes it covers: everything before it.
    const covered = std.mem.asBytes(&h)[0..BSET_FRAME_CHECKSUM_OFF];
    std.mem.writeInt(u64, &h.checksum_le, checksum.xxh3(covered), .little);
    out[0..BSET_FRAME_LEN].* = std.mem.toBytes(h);
    @memcpy(out[BSET_FRAME_LEN..][0..bset.len], bset);
    return out;
}

/// A decoded, checksum-verified frame header (geometry not yet validated).
pub const FrameHeader = struct {
    node_seq_at_write: u64,
    padded_len: usize,
    bset_len: usize,
    /// The fencing stamp.
    stamp: FrameStamp,
};

/// Outcome of inspecting one 4 KiB-aligned frame slot.
pub const FrameProbe = union(enum) {
    /// No frame magic: not this node's log (unwritten tail, stale garbage,
    /// or a tear that kept less than the magic).
    clean_end,
    /// Magic present but the header does not verify, its version is foreign
    /// or its reserved field is not zero (a verified liar).
    garbage,
    /// A verified frame of ANOTHER incarnation (recycled extent residue —
    /// any other value, higher or lower): a clean end, never loud.
    stale_incarnation,
    frame: FrameHeader,
};

/// Probe the frame slot at `pos` (the caller guarantees `pos +
/// BSET_FRAME_LEN ≤ buf.len`). Order: magic → checksum over `[0..32)` →
/// version → reserved → `node_seq_at_write`. Branches ONLY on
/// checksum-verified bytes.
pub fn probeFrame(buf: []const u8, pos: usize, node_seq: u64) FrameProbe {
    const hdr = buf[pos..][0..BSET_FRAME_LEN];
    const h = std.mem.bytesToValue(FrameHeaderWire, hdr);
    if (std.mem.readInt(u32, &h.magic_le, .little) != BSET_FRAME_MAGIC) return .clean_end;
    if (std.mem.readInt(u64, &h.checksum_le, .little) != checksum.xxh3(hdr[0..BSET_FRAME_CHECKSUM_OFF])) return .garbage;
    if (std.mem.readInt(u16, &h.version_le, .little) != BSET_FRAME_VERSION) return .garbage;
    if (std.mem.readInt(u16, &h.reserved_le, .little) != 0) return .garbage;
    const node_seq_at_write = std.mem.readInt(u64, &h.node_seq_at_write_le, .little);
    if (node_seq_at_write != node_seq) return .stale_incarnation;
    return .{ .frame = .{
        .node_seq_at_write = node_seq_at_write,
        .padded_len = std.mem.readInt(u32, &h.padded_len_le, .little),
        .bset_len = std.mem.readInt(u32, &h.bset_len_le, .little),
        .stamp = .{
            .appender_id = std.mem.readInt(u32, &h.appender_id_le, .little),
            .g = std.mem.readInt(u32, &h.g_le, .little),
        },
    } };
}

/// A same-incarnation frame's geometry against its container (design §9):
/// 4 KiB-multiple `padded_len` within the extent, a bset no smaller than
/// its header and no larger than the frame. A VERIFIED liar is corruption,
/// never a tear.
pub fn frameGeometryOk(f: FrameHeader, pos: usize, node_size: usize) bool {
    return f.padded_len >= NODE_PAGE and
        f.padded_len % NODE_PAGE == 0 and
        pos <= node_size and f.padded_len <= node_size - pos and
        f.bset_len >= BSET_HEADER_LEN and
        BSET_FRAME_LEN + f.bset_len <= f.padded_len;
}

/// The lessee-side input to the frame screen for ONE leaf.
pub const FrameScreen = struct {
    /// The slot's CURRENT lease generation.
    g_current: u32,
    /// The appender LEASING the slot at `g_current`; null while UNLEASED
    /// (a release keeps `g`, so the former lessee's frames at `g` and the
    /// manager's `(0, g)` frames are both legitimate — rule 4 is inert).
    appender_current: ?u32,
    /// `(g at the release that recorded it, tail offset)`; null when no
    /// release ever recorded this leaf.
    recorded_tail: ?struct { g: u32, tail: u32 },
    /// Whether the substrate fences (rule 1 lands on the breach tripwire
    /// under a device fence, on `foreign_frames_screened` otherwise).
    pr_fenced: bool,

    /// The rules a foreign frame lands on the BREACH class for under a
    /// device fence: a generation above the current one (1) or a current-
    /// generation frame from an appender that is not the lessee (4) — a
    /// write the reservation should have rejected. Rules 2–3 are stale-
    /// lessee shapes.
    pub fn isBreachRule(rule: u8) bool {
        return rule == 1 or rule == 4;
    }

    /// Is `stamp` the CURRENT lessee's own: the current generation, and —
    /// while leased — the lessee's appender id.
    pub fn isCurrentOwn(self: FrameScreen, stamp: FrameStamp) bool {
        if (stamp.g != self.g_current) return false;
        return if (self.appender_current) |a| stamp.appender_id == a else true;
    }

    /// The four-rule verdict on one frame at node-relative `pos`: the rule
    /// that classified it FOREIGN, or null to keep it. `prev_g` is the
    /// nearest earlier frame's generation in the same log (rule 3).
    pub fn foreignRule(self: FrameScreen, stamp: FrameStamp, pos: usize, prev_g: ?u32) ?u8 {
        if (stamp.g > self.g_current) return 1;
        if (self.recorded_tail) |t| {
            if (stamp.g <= t.g and pos >= t.tail) return 2;
        }
        if (prev_g) |p| if (stamp.g < p) return 3;
        if (self.appender_current) |lessee| {
            if (stamp.g == self.g_current and stamp.appender_id != lessee) return 4;
        }
        return null;
    }
};

// ---------------------------------------------------------------------------
// Tests — build/verify/search/merge/frame contracts.
// ---------------------------------------------------------------------------

const testing = std.testing;
const InodeValue = rec.InodeValue;
const InodeDelta = rec.InodeDelta;

fn iv(seed: u64) InodeValue {
    return .{ .mode = 0o100644, .uid = @truncate(seed), .nlink = 1, .size = seed * 7, .atime = seed, .mtime = seed, .ctime = seed };
}

fn put(gpa: Allocator, ino: u64, seq: u64) !Record {
    return Record.put(gpa, &key.inodeKey(ino), seq, &iv(seq).encode());
}

fn dtimes(gpa: Allocator, ino: u64, seq: u64, m: u64, c: u64) !Record {
    return Record.delta(gpa, &key.inodeKey(ino), seq, InodeDelta.times(m, c));
}

fn del(gpa: Allocator, ino: u64, seq: u64) !Record {
    return Record.delete(gpa, &key.inodeKey(ino), seq);
}

fn refsOf(records: []const Record, out: []RecordRef) []RecordRef {
    for (records, 0..) |*r, i| out[i] = r.ref();
    return out[0..records.len];
}

fn buildFrom(gpa: Allocator, records: []const Record, horizon: u64) ![]u8 {
    var buf: [64]RecordRef = undefined;
    return buildBset(gpa, refsOf(records, &buf), horizon);
}

fn freeAll(gpa: Allocator, list: *std.ArrayList(Record)) void {
    rec.deinitRecords(gpa, list.items);
    list.deinit(gpa);
}

test "bset header magic is ASCII BSET on disk (one magic convention); KBSF too" {
    const gpa = testing.allocator;
    var recs = [_]Record{try put(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 1);
    defer gpa.free(img);
    try testing.expectEqualStrings("BSET", img[0..4]);
    try testing.expectEqual(@as(u32, 0x54455342), BSET_MAGIC);
    try testing.expectEqualSlices(u8, &.{ 1, 0 }, img[4..6]);
    try testing.expectEqualSlices(u8, &.{ 0, 0 }, img[6..8]);
    try testing.expectEqualSlices(u8, &.{ 1, 0, 0, 0 }, img[8..12]);
    try testing.expectEqual(@as(u32, @intCast(img.len - 32)), std.mem.readInt(u32, img[12..16], .little));
    try testing.expectEqualSlices(u8, &.{ 1, 0, 0, 0, 0, 0, 0, 0 }, img[16..24]);
    try testing.expectEqual(checksumImage(img), std.mem.readInt(u64, img[24..32], .little));
    try testing.expectEqual(@as(u32, 0x4653424B), BSET_FRAME_MAGIC);
    var magic: [4]u8 = undefined;
    std.mem.writeInt(u32, &magic, BSET_FRAME_MAGIC, .little);
    try testing.expectEqualStrings("KBSF", &magic);
}

test "bset build/parse roundtrip preserves records and header" {
    const gpa = testing.allocator;
    var recs = [_]Record{
        try put(gpa, 1, 4),
        try dtimes(gpa, 1, 9, 1, 2),
        try put(gpa, 2, 5),
        try del(gpa, 3, 6),
    };
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 9);
    defer gpa.free(img);
    var view = try BsetView.parse(gpa, img);
    defer view.deinit(gpa);
    try testing.expectEqual(@as(usize, 4), view.len());
    try testing.expect(!view.isEmpty());
    try testing.expectEqual(@as(u64, 9), view.journal_seq_horizon);
    var it = view.iter();
    var i: usize = 0;
    while (it.next()) |r| : (i += 1) try testing.expect(r.eql(recs[i].ref()));
    try testing.expectEqual(@as(usize, 4), i);
    try testing.expectEqual(RecordKind.delete, view.record(3).kind);
    // Zero-copy: the record's key points into the image.
    try testing.expectEqual(@intFromPtr(img.ptr) + BSET_HEADER_LEN + rec.RECORD_HEADER_LEN, @intFromPtr(view.record(0).key.ptr));
}

test "bset build rejects empty, unsorted and duplicate inputs" {
    const gpa = testing.allocator;
    try testing.expectError(error.EmptyBset, buildBset(gpa, &.{}, 0));
    var unsorted = [_]Record{ try put(gpa, 2, 1), try put(gpa, 1, 2) };
    defer rec.deinitRecords(gpa, &unsorted);
    try testing.expectError(error.RecordsNotAscending, buildFrom(gpa, &unsorted, 2));
    var seq_desc = [_]Record{ try put(gpa, 1, 5), try dtimes(gpa, 1, 3, 1, 2) };
    defer rec.deinitRecords(gpa, &seq_desc);
    try testing.expectError(error.RecordsNotAscending, buildFrom(gpa, &seq_desc, 5));
    var dup = [_]Record{ try put(gpa, 1, 5), try put(gpa, 1, 5) };
    defer rec.deinitRecords(gpa, &dup);
    try testing.expectError(error.RecordsNotAscending, buildFrom(gpa, &dup, 5));
}

test "bset parse detects corruption anywhere via xxh3; structural gates come first in order" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 2);
    defer gpa.free(img);
    const bad = try gpa.dupe(u8, img);
    defer gpa.free(bad);
    for ([_]usize{ 9, BSET_HEADER_LEN + 3, img.len - 1 }) |pos| {
        @memcpy(bad, img);
        bad[pos] ^= 0x40;
        try testing.expectError(error.ChecksumMismatch, BsetView.parse(gpa, bad));
    }
    @memcpy(bad, img);
    bad[0] ^= 0xFF;
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    @memcpy(bad, img);
    bad[4] = 0xEE;
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    @memcpy(bad, img);
    bad[6] = 1;
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, img[0..16]));
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, img[0..31]));
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, img[0 .. img.len - 1]));
    const padded = try std.mem.concat(gpa, u8, &.{ img, &@as([8]u8, @splat(0)) });
    defer gpa.free(padded);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, padded));
}

test "bset parse bounds-checks every length field behind a restamped checksum" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 2);
    defer gpa.free(img);
    const bad = try gpa.dupe(u8, img);
    defer gpa.free(bad);
    // A record's val_len lying past the data area.
    std.mem.writeInt(u32, bad[BSET_HEADER_LEN + 4 ..][0..4], std.math.maxInt(u32), .little);
    stampChecksum(bad);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    // record_count lying high (the walk runs out of data).
    @memcpy(bad, img);
    std.mem.writeInt(u32, bad[8..12], 3, .little);
    stampChecksum(bad);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    // record_count lying low (trailing garbage after the walk).
    @memcpy(bad, img);
    std.mem.writeInt(u32, bad[8..12], 1, .little);
    stampChecksum(bad);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
    // data_len disagreeing with the buffer.
    @memcpy(bad, img);
    std.mem.writeInt(u32, bad[12..16], @as(u32, @intCast(img.len - BSET_HEADER_LEN)) - 1, .little);
    stampChecksum(bad);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, bad));
}

test "TEST-4: a lying record_count is never an allocation authority (count ≤ data_len / RECORD_MIN_LEN)" {
    const gpa = testing.allocator;
    // A 32 B image claiming u32::MAX records with a digest that verifies.
    var img: [BSET_HEADER_LEN]u8 = @splat(0);
    std.mem.writeInt(u32, img[0..4], BSET_MAGIC, .little);
    std.mem.writeInt(u16, img[4..6], BSET_VERSION, .little);
    std.mem.writeInt(u32, img[8..12], std.math.maxInt(u32), .little);
    stampChecksum(&img);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, &img));
    // Zero records with zero data parses (the bound admits it).
    std.mem.writeInt(u32, img[8..12], 0, .little);
    stampChecksum(&img);
    var empty = try BsetView.parse(gpa, &img);
    defer empty.deinit(gpa);
    try testing.expect(empty.isEmpty());
    // The bound holds across data lengths: count = data_len/17 + 1 refuses
    // before the walk, count = data_len/17 reaches it.
    try testing.expectEqual(@as(usize, 17), rec.RECORD_MIN_LEN);
    for ([_]usize{ 17, 18, 33, 34, 170, 1000 }) |data_len| {
        const buf = try gpa.alloc(u8, BSET_HEADER_LEN + data_len);
        defer gpa.free(buf);
        @memset(buf, 0);
        buf[0..BSET_HEADER_LEN].* = img;
        std.mem.writeInt(u32, buf[12..16], @intCast(data_len), .little);
        std.mem.writeInt(u32, buf[8..12], @intCast(data_len / rec.RECORD_MIN_LEN + 1), .little);
        stampChecksum(buf);
        try testing.expectError(error.Corrupt, BsetView.parse(gpa, buf));
    }
}

test "bset parse rejects out-of-order records behind a valid checksum" {
    const gpa = testing.allocator;
    var ra = [_]Record{try put(gpa, 2, 1)};
    defer rec.deinitRecords(gpa, &ra);
    var rb = [_]Record{try put(gpa, 1, 2)};
    defer rec.deinitRecords(gpa, &rb);
    const a = try buildFrom(gpa, &ra, 1);
    defer gpa.free(a);
    const b = try buildFrom(gpa, &rb, 2);
    defer gpa.free(b);
    var spliced: std.ArrayList(u8) = .empty;
    defer spliced.deinit(gpa);
    try spliced.appendSlice(gpa, a[0..8]);
    var w: [4]u8 = undefined;
    std.mem.writeInt(u32, &w, 2, .little);
    try spliced.appendSlice(gpa, &w);
    std.mem.writeInt(u32, &w, @intCast(a.len - BSET_HEADER_LEN + b.len - BSET_HEADER_LEN), .little);
    try spliced.appendSlice(gpa, &w);
    try spliced.appendSlice(gpa, a[16..24]);
    try spliced.appendSlice(gpa, &@as([8]u8, @splat(0)));
    try spliced.appendSlice(gpa, a[BSET_HEADER_LEN..]);
    try spliced.appendSlice(gpa, b[BSET_HEADER_LEN..]);
    stampChecksum(spliced.items);
    try testing.expectError(error.Corrupt, BsetView.parse(gpa, spliced.items));
}

test "bset binary search finds ranges without decoding; misses land at insertion points" {
    const gpa = testing.allocator;
    var recs = [_]Record{
        try put(gpa, 10, 1),
        try put(gpa, 20, 2),
        try dtimes(gpa, 20, 5, 1, 1),
        try dtimes(gpa, 20, 8, 2, 2),
        try put(gpa, 30, 3),
    };
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 8);
    defer gpa.free(img);
    var view = try BsetView.parse(gpa, img);
    defer view.deinit(gpa);
    try testing.expectEqual(Range{ .lo = 0, .hi = 1 }, view.find(&key.inodeKey(10)));
    const multi = view.find(&key.inodeKey(20));
    try testing.expectEqual(Range{ .lo = 1, .hi = 4 }, multi);
    try testing.expectEqual(@as(u64, 2), view.record(1).seq);
    try testing.expectEqual(@as(u64, 5), view.record(2).seq);
    try testing.expectEqual(@as(u64, 8), view.record(3).seq);
    try testing.expectEqual(Range{ .lo = 4, .hi = 5 }, view.find(&key.inodeKey(30)));
    try testing.expectEqual(Range{ .lo = 0, .hi = 0 }, view.find(&key.inodeKey(5)));
    try testing.expectEqual(Range{ .lo = 4, .hi = 4 }, view.find(&key.inodeKey(25)));
    try testing.expectEqual(Range{ .lo = 5, .hi = 5 }, view.find(&key.inodeKey(99)));
}

test "n-way merge orders key asc, seq desc, source priority; a cross-source duplicate survives" {
    const gpa = testing.allocator;
    var newest_r = [_]Record{ try dtimes(gpa, 1, 9, 9, 9), try put(gpa, 2, 7) };
    defer rec.deinitRecords(gpa, &newest_r);
    var mid_r = [_]Record{ try put(gpa, 1, 5), try put(gpa, 3, 6) };
    defer rec.deinitRecords(gpa, &mid_r);
    var oldest_r = [_]Record{ try put(gpa, 1, 2), try put(gpa, 2, 7), try put(gpa, 4, 1) };
    defer rec.deinitRecords(gpa, &oldest_r);
    const newest = try buildFrom(gpa, &newest_r, 9);
    defer gpa.free(newest);
    const mid = try buildFrom(gpa, &mid_r, 6);
    defer gpa.free(mid);
    const oldest = try buildFrom(gpa, &oldest_r, 7);
    defer gpa.free(oldest);
    var sources = [_]BsetView{ try BsetView.parse(gpa, newest), try BsetView.parse(gpa, mid), try BsetView.parse(gpa, oldest) };
    defer for (&sources) |*s| s.deinit(gpa);
    var it = try merge(gpa, &sources);
    defer it.deinit(gpa);
    const want = [_]struct { u64, u64 }{ .{ 1, 9 }, .{ 1, 5 }, .{ 1, 2 }, .{ 2, 7 }, .{ 2, 7 }, .{ 3, 6 }, .{ 4, 1 } };
    for (want) |w| {
        const r = it.next().?;
        try testing.expectEqualSlices(u8, &key.inodeKey(w[0]), r.key);
        try testing.expectEqual(w[1], r.seq);
    }
    try testing.expectEqual(@as(?RecordRef, null), it.next());
}

test "n-way merge reverses same-key runs within one source; compaction keeps the tombstone at tail 4" {
    const gpa = testing.allocator;
    var single_r = [_]Record{ try put(gpa, 1, 3), try del(gpa, 1, 8), try put(gpa, 2, 5) };
    defer rec.deinitRecords(gpa, &single_r);
    var older_r = [_]Record{try put(gpa, 1, 1)};
    defer rec.deinitRecords(gpa, &older_r);
    const single = try buildFrom(gpa, &single_r, 8);
    defer gpa.free(single);
    const older = try buildFrom(gpa, &older_r, 1);
    defer gpa.free(older);
    var sources = [_]BsetView{ try BsetView.parse(gpa, single), try BsetView.parse(gpa, older) };
    defer for (&sources) |*s| s.deinit(gpa);
    var it = try merge(gpa, &sources);
    defer it.deinit(gpa);
    const want = [_]struct { u64, u64 }{ .{ 1, 8 }, .{ 1, 3 }, .{ 1, 1 }, .{ 2, 5 } };
    for (want) |w| {
        const r = it.next().?;
        try testing.expectEqualSlices(u8, &key.inodeKey(w[0]), r.key);
        try testing.expectEqual(w[1], r.seq);
    }
    try testing.expectEqual(@as(?RecordRef, null), it.next());
    var out: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &out);
    try compact(gpa, &sources, 4, &out);
    try testing.expectEqual(@as(usize, 2), out.items.len);
    try testing.expectEqual(RecordKind.delete, out.items[0].kind);
    try testing.expectEqual(@as(u64, 8), out.items[0].seq);
    try testing.expectEqualSlices(u8, &key.inodeKey(2), out.items[1].key);
}

/// The independent reference order: `(key asc, seq desc, source asc)`.
fn matchReferenceOrder(_: void, a: Match, b: Match) bool {
    switch (key.order(a.rec.key, b.rec.key)) {
        .lt => return true,
        .gt => return false,
        .eq => {},
    }
    if (a.rec.seq != b.rec.seq) return a.rec.seq > b.rec.seq;
    return a.si < b.si;
}

test "property: the memoized n-way merge matches the reference order under long same-key runs, and compact ≡ sort + compactFold (xorshift64, 400 trials)" {
    const gpa = testing.allocator;
    var state: u64 = 0x9E37_79B9_7F4A_7C15;
    const Rnd = struct {
        fn next(s: *u64, m: u64) u64 {
            s.* ^= s.* << 13;
            s.* ^= s.* >> 7;
            s.* ^= s.* << 17;
            return s.* % m;
        }
    };
    for (0..400) |_| {
        const nsrc: usize = 1 + @as(usize, @intCast(Rnd.next(&state, 6)));
        var imgs: std.ArrayList([]u8) = .empty;
        defer {
            for (imgs.items) |i| gpa.free(i);
            imgs.deinit(gpa);
        }
        var seq: u64 = 1;
        for (0..nsrc) |_| {
            const nkeys = 1 + Rnd.next(&state, 5);
            var recs: std.ArrayList(Record) = .empty;
            defer freeAll(gpa, &recs);
            var k = Rnd.next(&state, 4) + 1;
            for (0..nkeys) |_| {
                const run = 1 + Rnd.next(&state, 9);
                for (0..run) |_| {
                    // A Put every so often so the compaction comparison folds
                    // real chains, not only orphans.
                    if (Rnd.next(&state, 4) == 0) {
                        try recs.append(gpa, try put(gpa, k, seq));
                    } else {
                        try recs.append(gpa, try dtimes(gpa, k, seq, seq, seq));
                    }
                    seq += 1;
                }
                k += 1 + Rnd.next(&state, 3);
            }
            var rbuf: [64]RecordRef = undefined;
            try imgs.append(gpa, try buildBset(gpa, refsOf(recs.items, &rbuf), seq));
        }
        const views = try gpa.alloc(BsetView, nsrc);
        defer gpa.free(views);
        for (views, imgs.items) |*v, img| v.* = try BsetView.parse(gpa, img);
        defer for (views) |*v| v.deinit(gpa);
        // The independent reference: every record tagged with its source,
        // sorted `(key asc, seq desc, source asc)`.
        var all: std.ArrayList(Match) = .empty;
        defer all.deinit(gpa);
        for (views, 0..) |*v, si| {
            for (0..v.len()) |i| try all.append(gpa, .{ .si = si, .rec = v.record(i) });
        }
        std.sort.pdq(Match, all.items, {}, matchReferenceOrder);
        var it = try merge(gpa, views);
        defer it.deinit(gpa);
        for (all.items) |want| {
            const got = it.next().?;
            try testing.expect(want.rec.eql(got));
        }
        try testing.expectEqual(@as(?RecordRef, null), it.next());
        // merge + compact ≡ per-key groups of the sorted reference + compactFold.
        var via_merge: std.ArrayList(Record) = .empty;
        defer freeAll(gpa, &via_merge);
        try compact(gpa, views, 0, &via_merge);
        var via_sort: std.ArrayList(Record) = .empty;
        defer freeAll(gpa, &via_sort);
        var group: std.ArrayList(RecordRef) = .empty;
        defer group.deinit(gpa);
        for (all.items) |m| {
            if (group.items.len != 0 and !std.mem.eql(u8, group.items[0].key, m.rec.key)) {
                try rec.compactFold(gpa, group.items, 0, &via_sort);
                group.clearRetainingCapacity();
            }
            try group.append(gpa, m.rec);
        }
        try rec.compactFold(gpa, group.items, 0, &via_sort);
        try testing.expectEqual(via_sort.items.len, via_merge.items.len);
        for (via_sort.items, via_merge.items) |a, b| try testing.expect(a.ref().eql(b.ref()));
    }
}

test "lookup folds across sources with the single algebra: Δ over an older Put, tombstone shadow, borrowed untouched key, absent" {
    const gpa = testing.allocator;
    var old_r = [_]Record{ try put(gpa, 1, 2), try put(gpa, 2, 3), try put(gpa, 3, 4) };
    defer rec.deinitRecords(gpa, &old_r);
    var new_r = [_]Record{ try dtimes(gpa, 1, 9, 90, 99), try del(gpa, 2, 8) };
    defer rec.deinitRecords(gpa, &new_r);
    const old = try buildFrom(gpa, &old_r, 4);
    defer gpa.free(old);
    const new = try buildFrom(gpa, &new_r, 9);
    defer gpa.free(new);
    var sources = [_]BsetView{ try BsetView.parse(gpa, new), try BsetView.parse(gpa, old) };
    defer for (&sources) |*s| s.deinit(gpa);
    var f1 = try lookup(gpa, &sources, &key.inodeKey(1));
    defer f1.deinit(gpa);
    try testing.expectEqual(@as(u64, 9), f1.put.seq);
    try testing.expect(f1.put.owned);
    const v = try InodeValue.decode(f1.put.value);
    try testing.expectEqual(@as(u64, 90), v.mtime);
    try testing.expectEqual(@as(u64, 99), v.ctime);
    var f2 = try lookup(gpa, &sources, &key.inodeKey(2));
    defer f2.deinit(gpa);
    try testing.expectEqual(rec.Folded{ .tombstone = .{ .seq = 8 } }, f2);
    var f3 = try lookup(gpa, &sources, &key.inodeKey(3));
    defer f3.deinit(gpa);
    try testing.expectEqual(@as(u64, 4), f3.put.seq);
    try testing.expect(!f3.put.owned);
    // Zero-copy: the value points into the old image.
    try testing.expect(@intFromPtr(f3.put.value.ptr) >= @intFromPtr(old.ptr) and @intFromPtr(f3.put.value.ptr) < @intFromPtr(old.ptr) + old.len);
    var f4 = try lookup(gpa, &sources, &key.inodeKey(99));
    defer f4.deinit(gpa);
    try testing.expectEqual(rec.Folded.absent, f4);
}

test "compact emits a buildable folded output; a second compaction is a fixed point" {
    const gpa = testing.allocator;
    var old_r = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2), try put(gpa, 3, 3), try put(gpa, 4, 4) };
    defer rec.deinitRecords(gpa, &old_r);
    var new_r = [_]Record{ try dtimes(gpa, 1, 9, 90, 99), try del(gpa, 2, 5), try del(gpa, 3, 8) };
    defer rec.deinitRecords(gpa, &new_r);
    const old = try buildFrom(gpa, &old_r, 4);
    defer gpa.free(old);
    const new = try buildFrom(gpa, &new_r, 9);
    defer gpa.free(new);
    var sources = [_]BsetView{ try BsetView.parse(gpa, new), try BsetView.parse(gpa, old) };
    defer for (&sources) |*s| s.deinit(gpa);
    // durable_tail = 6: the seq-5 tombstone (and the key it shadows) is
    // checkpoint-covered ⇒ elided; the seq-8 tombstone survives.
    var out: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &out);
    try compact(gpa, &sources, 6, &out);
    try testing.expectEqual(@as(usize, 3), out.items.len);
    try testing.expectEqualSlices(u8, &key.inodeKey(1), out.items[0].key);
    try testing.expectEqual(RecordKind.put, out.items[0].kind);
    try testing.expectEqual(@as(u64, 9), out.items[0].seq);
    try testing.expectEqualSlices(u8, &key.inodeKey(3), out.items[1].key);
    try testing.expectEqual(RecordKind.delete, out.items[1].kind);
    try testing.expectEqual(@as(u64, 8), out.items[1].seq);
    try testing.expectEqualSlices(u8, &key.inodeKey(4), out.items[2].key);
    try testing.expectEqual(RecordKind.put, out.items[2].kind);
    try testing.expectEqual(@as(u64, 4), out.items[2].seq);
    const folded = try InodeValue.decode(out.items[0].value);
    try testing.expectEqual(@as(u64, 90), folded.mtime);
    try testing.expectEqual(@as(u64, 99), folded.ctime);
    const rebuilt = try buildFrom(gpa, out.items, 9);
    defer gpa.free(rebuilt);
    var reparsed = try BsetView.parse(gpa, rebuilt);
    defer reparsed.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), reparsed.len());
    var twice: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &twice);
    try compact(gpa, &.{reparsed}, 6, &twice);
    try testing.expectEqual(out.items.len, twice.items.len);
    for (out.items, twice.items) |a, b| try testing.expect(a.ref().eql(b.ref()));
}

test "compaction of an inode-delta chain stays a single record" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try put(gpa, 9, 1), try dtimes(gpa, 9, 2, 5, 6), try dtimes(gpa, 9, 3, 7, 8) };
    defer rec.deinitRecords(gpa, &recs);
    const img = try buildFrom(gpa, &recs, 3);
    defer gpa.free(img);
    var view = try BsetView.parse(gpa, img);
    defer view.deinit(gpa);
    var out: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &out);
    try compact(gpa, &.{view}, 0, &out);
    try testing.expectEqual(@as(usize, 1), out.items.len);
    try testing.expectEqual(RecordKind.put, out.items[0].kind);
    try testing.expectEqual(@as(u64, 3), out.items[0].seq);
}

test "compaction preserves the lineage a live link claims (versioned layout chain through a bset)" {
    const gpa = testing.allocator;
    const k = "xattr-layout-key";
    var base_l: rec.Layout = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .block_map = .empty };
    defer base_l.deinit(gpa);
    try base_l.insert(gpa, 0, "be://data:a");
    const base_layout = try base_l.encodeAlloc(gpa);
    defer gpa.free(base_layout);
    const base_v = try (rec.XattrValue{ .name = "layout", .value = base_layout }).encodeAlloc(gpa);
    defer gpa.free(base_v);
    var d1: rec.LayoutDelta = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .entries = &.{.{ .block = 1, .key = "be://data:b" }} };
    d1.setVersions(0, 0x11);
    const d1_v = try d1.encodeAlloc(gpa);
    defer gpa.free(d1_v);
    var d2: rec.LayoutDelta = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .entries = &.{.{ .block = 2, .key = "be://data:c" }} };
    d2.setVersions(0x11, 0x12);
    const d2_v = try d2.encodeAlloc(gpa);
    defer gpa.free(d2_v);
    const chain = [_]RecordRef{
        .{ .key = k, .seq = 1, .kind = .put, .value = base_v },
        .{ .key = k, .seq = 2, .kind = .delta, .value = d1_v },
        .{ .key = k, .seq = 3, .kind = .delta, .value = d2_v },
    };
    const img = try buildBset(gpa, &chain, 3);
    defer gpa.free(img);
    var view = try BsetView.parse(gpa, img);
    defer view.deinit(gpa);
    var folded: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &folded);
    try compact(gpa, &.{view}, 0, &folded);
    try testing.expectEqual(@as(usize, 2), folded.items.len);
    // A LIVE later link committed against the pre-compaction head (0x12).
    var d3: rec.LayoutDelta = .{ .file_type = "striped", .size = 4096, .block_prefix = "be://data", .entries = &.{.{ .block = 3, .key = "be://data:d" }} };
    d3.setVersions(0x12, 0x13);
    const d3_v = try d3.encodeAlloc(gpa);
    defer gpa.free(d3_v);
    var f = try rec.foldNewestFirst(gpa, &.{ .{ .key = k, .seq = 4, .kind = .delta, .value = d3_v }, folded.items[1].ref(), folded.items[0].ref() });
    defer f.deinit(gpa);
    const x = try rec.XattrValue.decode(f.put.value);
    var l = try rec.Layout.decodeBase(gpa, x.value);
    defer l.deinit(gpa);
    for ([_]struct { u32, []const u8 }{ .{ 0, "be://data:a" }, .{ 1, "be://data:b" }, .{ 2, "be://data:c" }, .{ 3, "be://data:d" } }) |e| {
        try testing.expectEqualStrings(e[1], l.get(e[0]).?);
    }
}

test "forced collision chain resolves by full name and orders cookies like keys" {
    const gpa = testing.allocator;
    const parent: u64 = 1;
    const hash54: u64 = 0xABCDEF;
    const first_seq = try key.assignDentryCollSeq(&.{});
    try testing.expectEqual(@as(u8, 0), first_seq);
    const second_seq = try key.assignDentryCollSeq(&.{first_seq});
    try testing.expectEqual(@as(u8, 1), second_seq);
    const alpha: rec.DentryValue = .{ .child_ino = 100, .file_type = 8, .name = "alpha" };
    const beta: rec.DentryValue = .{ .child_ino = 200, .file_type = 8, .name = "beta" };
    var ab: [32]u8 = undefined;
    var bb: [32]u8 = undefined;
    const recs = [_]RecordRef{
        .{ .key = &key.dentryKey(parent, hash54, first_seq), .seq = 1, .kind = .put, .value = try alpha.encodeInto(&ab) },
        .{ .key = &key.dentryKey(parent, hash54, second_seq), .seq = 2, .kind = .put, .value = try beta.encodeInto(&bb) },
    };
    const img = try buildBset(gpa, &recs, 2);
    defer gpa.free(img);
    var view = try BsetView.parse(gpa, img);
    defer view.deinit(gpa);
    var resolved: usize = 0;
    var beta_ino: ?u64 = null;
    for (0..256) |c| {
        const range = view.find(&key.dentryKey(parent, hash54, @intCast(c)));
        for (range.lo..range.hi) |i| {
            const d = try rec.DentryValue.decode(view.record(i).value);
            if (std.mem.eql(u8, d.name, "beta")) beta_ino = d.child_ino;
            try testing.expectEqual(@as(usize, c), resolved);
            resolved += 1;
        }
    }
    try testing.expectEqual(@as(usize, 2), resolved);
    try testing.expectEqual(@as(?u64, 200), beta_ino);
    const c0 = key.encodeReaddirCookie(hash54, 0);
    const c1 = key.encodeReaddirCookie(hash54, 1);
    try testing.expectEqual(c0 + 1, c1);
    try testing.expect(key.lessThan(&key.dentryKey(parent, hash54, 0), &key.dentryKey(parent, hash54, 1)));
}

// ---------------------------------------------------------------------------
// The FIND-VS-A fixture: a populated predecessor leaf of the shape the
// reference's capture had (a directory's dentries interleaved with their
// inodes, Puts and Δtimes, no tombstones), generated deterministically here
// so the committed bytes under `tests/fixtures/` never depend on the
// reference's image (decision 0002 §3). `tests/fixtures/README.md` records
// the recipe.
// ---------------------------------------------------------------------------

/// The committed fixture's path, relative to the repository root (the test
/// binary's cwd).
pub const findvsa_fixture_path = "tests/fixtures/findvsa_pred_bset.v4.bin";
pub const findvsa_fixture_seed: u64 = 0xF1D5_A000_0000_0004;
pub const findvsa_fixture_horizon: u64 = 72_220_661;
/// Files under the fixture's one directory: enough that the leaf is a
/// real multi-page image (≈ 250 KiB) like the capture's.
pub const findvsa_fixture_files: usize = 1640;

/// The fixture image: directory ino 2 with `findvsa_fixture_files` children
/// — one inode Put each, one dentry Put each (seeded hash54 so the keys
/// hash-shuffle as a real directory's do), and a Δtime on every seventh
/// inode — all records seq-ascending through one PRNG, memcmp-sorted.
pub fn findVsAFixtureImage(gpa: Allocator) (BuildError || rec.EncodeError)![]u8 {
    var prng = std.Random.DefaultPrng.init(findvsa_fixture_seed);
    const r = prng.random();
    var recs: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &recs);
    const dir_ino: u64 = 2;
    var seq: u64 = findvsa_fixture_horizon - 4 * findvsa_fixture_files;
    var name_buf: [32]u8 = undefined;
    var dbuf: [64]u8 = undefined;
    for (0..findvsa_fixture_files) |i| {
        const ino = dir_ino + 1 + @as(u64, i);
        const name = std.fmt.bufPrint(&name_buf, "f{d}-{x}", .{ i, r.int(u32) }) catch unreachable;
        const v: rec.InodeValue = .{ .mode = 0o100644, .uid = 1000, .gid = 1000, .nlink = 1, .size = r.intRangeAtMost(u64, 0, 1 << 30), .atime = seq, .mtime = seq, .ctime = seq };
        try recs.append(gpa, try Record.put(gpa, &key.inodeKey(ino), seq, &v.encode()));
        seq += 1;
        const dv = try (rec.DentryValue{ .child_ino = ino, .file_type = 8, .name = name }).encodeInto(&dbuf);
        try recs.append(gpa, try Record.put(gpa, &key.dentryKey(dir_ino, key.dentryNameHash54(name, findvsa_fixture_seed), 0), seq, dv));
        seq += 1;
        if (i % 7 == 0) {
            try recs.append(gpa, try Record.delta(gpa, &key.inodeKey(ino), seq, rec.InodeDelta.times(seq, seq)));
            seq += 1;
        }
    }
    std.mem.sort(Record, recs.items, {}, struct {
        fn lt(_: void, a: Record, b: Record) bool {
            return keySeqLess(a.ref(), b.ref());
        }
    }.lt);
    const refs = try gpa.alloc(RecordRef, recs.items.len);
    defer gpa.free(refs);
    for (recs.items, refs) |*rr, *o| o.* = rr.ref();
    return buildBset(gpa, refs, findvsa_fixture_horizon);
}

test "the committed FIND-VS-A fixture is the generator's bytes; it parses, every key is a v4 inode or dentry key, no tombstones; compaction at the harshest legal tail keeps every live key" {
    const gpa = testing.allocator;
    const generated = try findVsAFixtureImage(gpa);
    defer gpa.free(generated);
    // The pin: the committed file never drifts from the in-tree generator.
    const bytes = try std.Io.Dir.cwd().readFileAlloc(testing.io, findvsa_fixture_path, gpa, .limited(1 << 20));
    defer gpa.free(bytes);
    try testing.expectEqualSlices(u8, generated, bytes);
    try testing.expectEqual(@as(usize, 254_932), bytes.len);
    var view = try BsetView.parse(gpa, bytes);
    defer view.deinit(gpa);
    try testing.expectEqual(@as(usize, 3515), view.len());
    try testing.expectEqual(findvsa_fixture_horizon, view.journal_seq_horizon);
    var it = view.iter();
    var i: usize = 0;
    while (it.next()) |rr| : (i += 1) {
        const kind = try key.slotTreeKeyKind(rr.key);
        try testing.expect(kind == key.TREE_DENTRIES or kind == key.TREE_INODES);
        try testing.expect(rr.kind != .delete);
        const range = view.find(rr.key);
        try testing.expect(range.lo <= i and i < range.hi);
    }
    // Compaction at a durable tail higher than every input seq (the harshest
    // legal elision) keeps every live key.
    var out: std.ArrayList(Record) = .empty;
    defer freeAll(gpa, &out);
    try compact(gpa, &.{view}, findvsa_fixture_horizon + 1, &out);
    var distinct: usize = 1;
    for (1..view.len()) |j| {
        if (!std.mem.eql(u8, view.record(j - 1).key, view.record(j).key)) distinct += 1;
    }
    try testing.expectEqual(distinct, out.items.len);
}

test "frame: 40 B header {KBSF, version 1, reserved 0, node_seq, padded_len, bset_len, appender_id, g, xxh3 over [0..32)}, bset at 40, zero pad to a 4 KiB multiple; probe order" {
    const gpa = testing.allocator;
    var recs = [_]Record{ try put(gpa, 1, 1), try put(gpa, 2, 2) };
    defer rec.deinitRecords(gpa, &recs);
    var rbuf: [8]RecordRef = undefined;
    const refs = refsOf(&recs, &rbuf);
    const node_size: usize = 64 * 1024;
    const stamp: FrameStamp = .{ .appender_id = 0x0102, .g = 0x0304 };
    const frame = try encodeBsetFrame(gpa, stamp, node_size, 0xABCD, refs, 2);
    defer gpa.free(frame);
    try testing.expectEqual(@as(usize, 40), BSET_FRAME_LEN);
    try testing.expectEqual(@as(usize, NODE_PAGE), frame.len);
    try testing.expectEqualStrings("KBSF", frame[0..4]);
    try testing.expectEqualSlices(u8, &.{ 1, 0 }, frame[4..6]);
    try testing.expectEqualSlices(u8, &.{ 0, 0 }, frame[6..8]);
    try testing.expectEqual(@as(u64, 0xABCD), std.mem.readInt(u64, frame[8..16], .little));
    try testing.expectEqual(@as(u32, NODE_PAGE), std.mem.readInt(u32, frame[16..20], .little));
    const bset_len = std.mem.readInt(u32, frame[20..24], .little);
    try testing.expectEqualSlices(u8, &.{ 2, 1, 0, 0 }, frame[24..28]);
    try testing.expectEqualSlices(u8, &.{ 4, 3, 0, 0 }, frame[28..32]);
    try testing.expectEqual(checksum.xxh3(frame[0..32]), std.mem.readInt(u64, frame[32..40], .little));
    const bset = try buildBset(gpa, refs, 2);
    defer gpa.free(bset);
    try testing.expectEqual(@as(u32, @intCast(bset.len)), bset_len);
    try testing.expectEqualSlices(u8, bset, frame[40..][0..bset.len]);
    for (frame[40 + bset.len ..]) |b| try testing.expectEqual(@as(u8, 0), b);
    try testing.expectEqual(frame.len, appendFrameLen(bset.len - BSET_HEADER_LEN));
    try testing.expectEqual(@as(usize, 0), appendFrameLen(0));
    // The pad is not covered: a torn pad still probes as the frame.
    const torn = try gpa.dupe(u8, frame);
    defer gpa.free(torn);
    torn[torn.len - 1] = 0xFF;
    const p = probeFrame(torn, 0, 0xABCD);
    try testing.expectEqual(@as(usize, NODE_PAGE), p.frame.padded_len);
    try testing.expectEqual(@as(usize, bset.len), p.frame.bset_len);
    try testing.expect(p.frame.stamp.eql(stamp));
    try testing.expect(frameGeometryOk(p.frame, 0, node_size));
    var parsed = try BsetView.parse(gpa, torn[BSET_FRAME_LEN..][0..p.frame.bset_len]);
    defer parsed.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), parsed.len());
    // Probe order: magic ⇒ clean end; checksum ⇒ garbage; version ⇒ garbage;
    // reserved ⇒ garbage; node_seq ⇒ stale incarnation.
    @memcpy(torn, frame);
    torn[0] ^= 0xFF;
    try testing.expectEqual(FrameProbe.clean_end, probeFrame(torn, 0, 0xABCD));
    @memcpy(torn, frame);
    torn[9] ^= 0x01;
    try testing.expectEqual(FrameProbe.garbage, probeFrame(torn, 0, 0xABCD));
    @memcpy(torn, frame);
    torn[4] = 2;
    std.mem.writeInt(u64, torn[32..40], checksum.xxh3(torn[0..32]), .little);
    try testing.expectEqual(FrameProbe.garbage, probeFrame(torn, 0, 0xABCD));
    // Reserved is CHECKED: a restamped non-zero reserved is a verified liar.
    @memcpy(torn, frame);
    torn[6] = 1;
    std.mem.writeInt(u64, torn[32..40], checksum.xxh3(torn[0..32]), .little);
    try testing.expectEqual(FrameProbe.garbage, probeFrame(torn, 0, 0xABCD));
    try testing.expectEqual(FrameProbe.stale_incarnation, probeFrame(frame, 0, 0xABCE));
    try testing.expectEqual(FrameProbe.stale_incarnation, probeFrame(frame, 0, 0xABCC));
    // The verified frame carries its stamp; another stamp is distinguishable.
    try testing.expect(probeFrame(frame, 0, 0xABCD).frame.stamp.eql(stamp));
    const other: FrameStamp = .{ .appender_id = 9, .g = 0x0304 };
    try testing.expect(!probeFrame(frame, 0, 0xABCD).frame.stamp.eql(other));
    // Default stamp is the manager's (0, 0).
    try testing.expect((FrameStamp{}).eql(.{ .appender_id = 0, .g = 0 }));
    // A frame whose bset spills past the 4 KiB page pads to the next.
    var big: [100]Record = undefined;
    for (&big, 0..) |*rr, idx| rr.* = try put(gpa, idx + 1, idx + 1);
    defer rec.deinitRecords(gpa, &big);
    var bigbuf: [100]RecordRef = undefined;
    const big_frame = try encodeBsetFrame(gpa, .{}, node_size, 1, refsOf(&big, &bigbuf), 100);
    defer gpa.free(big_frame);
    try testing.expectEqual(@as(usize, 3 * NODE_PAGE), big_frame.len);
    try testing.expectEqual(big_frame.len, pageAlign(BSET_FRAME_LEN + std.mem.readInt(u32, big_frame[20..24], .little)));
}

test "frame encoder refuses a record value over the node's cap; geometry refuses impossible shapes" {
    const gpa = testing.allocator;
    const node_size: usize = 64 * 1024;
    const cap = rec.recordValueCap(node_size);
    const over = try gpa.alloc(u8, cap + 1);
    defer gpa.free(over);
    @memset(over, 0xAA);
    try testing.expectError(error.ValueTooLarge, encodeBsetFrame(gpa, .{}, node_size, 1, &.{.{ .key = "k", .seq = 1, .kind = .put, .value = over }}, 1));
    const fits = try encodeBsetFrame(gpa, .{}, node_size, 1, &.{.{ .key = "k", .seq = 1, .kind = .put, .value = over[0..cap] }}, 1);
    defer gpa.free(fits);
    try testing.expectError(error.EmptyBset, encodeBsetFrame(gpa, .{}, node_size, 1, &.{}, 1));
    const ok: FrameHeader = .{ .node_seq_at_write = 1, .padded_len = NODE_PAGE, .bset_len = 100, .stamp = .{} };
    try testing.expect(frameGeometryOk(ok, 0, node_size));
    try testing.expect(frameGeometryOk(ok, node_size - NODE_PAGE, node_size));
    try testing.expect(!frameGeometryOk(ok, node_size - NODE_PAGE + 1, node_size));
    try testing.expect(!frameGeometryOk(ok, node_size + 1, node_size));
    var bad = ok;
    bad.padded_len = NODE_PAGE - 1;
    try testing.expect(!frameGeometryOk(bad, 0, node_size));
    bad.padded_len = NODE_PAGE + 1;
    try testing.expect(!frameGeometryOk(bad, 0, node_size));
    bad = ok;
    bad.bset_len = BSET_HEADER_LEN - 1;
    try testing.expect(!frameGeometryOk(bad, 0, node_size));
    bad = ok;
    bad.bset_len = NODE_PAGE - BSET_FRAME_LEN + 1;
    try testing.expect(!frameGeometryOk(bad, 0, node_size));
    bad.bset_len = NODE_PAGE - BSET_FRAME_LEN;
    try testing.expect(frameGeometryOk(bad, 0, node_size));
}

test "frame screen: the four foreign rules in order; breach class is rules 1 and 4; current-own" {
    const leased: FrameScreen = .{ .g_current = 5, .appender_current = 42, .recorded_tail = .{ .g = 3, .tail = 8192 }, .pr_fenced = true };
    // Rule 1: a generation above the current one.
    try testing.expectEqual(@as(?u8, 1), leased.foreignRule(.{ .appender_id = 42, .g = 6 }, 0, null));
    // Rule 2: a stale lessee's frame at/past the recorded tail of its generation.
    try testing.expectEqual(@as(?u8, 2), leased.foreignRule(.{ .appender_id = 7, .g = 3 }, 8192, null));
    try testing.expectEqual(@as(?u8, 2), leased.foreignRule(.{ .appender_id = 7, .g = 2 }, 9000, null));
    try testing.expectEqual(@as(?u8, null), leased.foreignRule(.{ .appender_id = 7, .g = 3 }, 4096, null));
    try testing.expectEqual(@as(?u8, null), leased.foreignRule(.{ .appender_id = 7, .g = 4 }, 8192, null));
    // Rule 3: a generation below the nearest earlier frame's.
    try testing.expectEqual(@as(?u8, 3), leased.foreignRule(.{ .appender_id = 7, .g = 4 }, 0, 5));
    try testing.expectEqual(@as(?u8, null), leased.foreignRule(.{ .appender_id = 42, .g = 5 }, 0, 5));
    // Rule 4: a current-generation frame from an appender that is not the lessee.
    try testing.expectEqual(@as(?u8, 4), leased.foreignRule(.{ .appender_id = 7, .g = 5 }, 0, null));
    try testing.expectEqual(@as(?u8, null), leased.foreignRule(.{ .appender_id = 42, .g = 5 }, 0, null));
    // Unleased: rule 4 is inert; the (0, g) manager frame and the former lessee's both keep.
    const unleased: FrameScreen = .{ .g_current = 5, .appender_current = null, .recorded_tail = null, .pr_fenced = false };
    try testing.expectEqual(@as(?u8, null), unleased.foreignRule(.{ .appender_id = 7, .g = 5 }, 0, null));
    try testing.expectEqual(@as(?u8, null), unleased.foreignRule(.{ .appender_id = 0, .g = 5 }, 0, null));
    // The (0,0) first-frame priming law: a (0,0) frame above frames at g ≥ 1 trips rule 3.
    try testing.expectEqual(@as(?u8, 3), unleased.foreignRule(.{}, 4096, 1));
    try testing.expectEqual(@as(?u8, null), unleased.foreignRule(.{}, 0, null));
    try testing.expect(FrameScreen.isBreachRule(1));
    try testing.expect(!FrameScreen.isBreachRule(2));
    try testing.expect(!FrameScreen.isBreachRule(3));
    try testing.expect(FrameScreen.isBreachRule(4));
    try testing.expect(leased.isCurrentOwn(.{ .appender_id = 42, .g = 5 }));
    try testing.expect(!leased.isCurrentOwn(.{ .appender_id = 7, .g = 5 }));
    try testing.expect(!leased.isCurrentOwn(.{ .appender_id = 42, .g = 4 }));
    try testing.expect(unleased.isCurrentOwn(.{ .appender_id = 7, .g = 5 }));
}

test "property: bset round-trips under a deterministic PRNG and parse is total on arbitrary and restamped bytes" {
    const gpa = testing.allocator;
    var prng = std.Random.DefaultPrng.init(0xB5E7);
    const r = prng.random();
    var buf: [1024]u8 = undefined;
    for (0..3000) |_| {
        // Arbitrary bytes.
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        if (BsetView.parse(gpa, buf[0..len])) |*v| {
            var vv = v.*;
            vv.deinit(gpa);
        } else |_| {}
        // Restamped: a valid header prefix with a random body reaches the
        // structural gates behind the checksum.
        if (len >= BSET_HEADER_LEN) {
            std.mem.writeInt(u32, buf[0..4], BSET_MAGIC, .little);
            std.mem.writeInt(u16, buf[4..6], BSET_VERSION, .little);
            std.mem.writeInt(u16, buf[6..8], 0, .little);
            std.mem.writeInt(u32, buf[12..16], @intCast(len - BSET_HEADER_LEN), .little);
            if (r.boolean()) std.mem.writeInt(u32, buf[8..12], @intCast(r.intRangeAtMost(usize, 0, (len - BSET_HEADER_LEN) / rec.RECORD_MIN_LEN + 1)), .little);
            stampChecksum(buf[0..len]);
            if (BsetView.parse(gpa, buf[0..len])) |*v| {
                var vv = v.*;
                defer vv.deinit(gpa);
                // After a successful parse every record is in bounds,
                // strictly ascending, and `find` locates it.
                var prev: ?RecordRef = null;
                for (0..vv.len()) |i| {
                    const cur = vv.record(i);
                    if (prev) |p| try testing.expect(keySeqLess(p, cur));
                    prev = cur;
                    const range = vv.find(cur.key);
                    try testing.expect(range.lo <= i and i < range.hi);
                }
            } else |_| {}
        }
    }
    // Round trip: random sorted record sets build, parse, and read back equal.
    for (0..200) |_| {
        var recs: std.ArrayList(Record) = .empty;
        defer freeAll(gpa, &recs);
        const n = r.intRangeAtMost(usize, 1, 40);
        var ino: u64 = r.intRangeAtMost(u64, 0, 100);
        var seq: u64 = 1;
        for (0..n) |_| {
            const made = switch (r.intRangeLessThan(u8, 0, 3)) {
                0 => try put(gpa, ino, seq),
                1 => try dtimes(gpa, ino, seq, seq, seq),
                else => try del(gpa, ino, seq),
            };
            try recs.append(gpa, made);
            seq += 1 + r.intRangeAtMost(u64, 0, 3);
            if (r.boolean()) ino += 1 + r.intRangeAtMost(u64, 0, 5);
        }
        var rbuf: [40]RecordRef = undefined;
        const refs = refsOf(recs.items, &rbuf);
        const img = try buildBset(gpa, refs, seq);
        defer gpa.free(img);
        var view = try BsetView.parse(gpa, img);
        defer view.deinit(gpa);
        try testing.expectEqual(refs.len, view.len());
        for (refs, 0..) |want, i| try testing.expect(want.eql(view.record(i)));
        // The frame round-trips through the probe.
        const frame = try encodeBsetFrame(gpa, .{ .appender_id = 1, .g = 2 }, 64 * 1024, 99, refs, seq);
        defer gpa.free(frame);
        const p = probeFrame(frame, 0, 99);
        try testing.expectEqual(img.len, p.frame.bset_len);
        try testing.expect(p.frame.stamp.eql(.{ .appender_id = 1, .g = 2 }));
        try testing.expectEqualSlices(u8, img, frame[BSET_FRAME_LEN..][0..img.len]);
    }
}

fn bsetParseFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [1024]u8 = undefined;
    const len = smith.slice(&buf);
    const gpa = testing.allocator;
    if (BsetView.parse(gpa, buf[0..len])) |*v| {
        var vv = v.*;
        vv.deinit(gpa);
    } else |_| {}
    if (len >= BSET_HEADER_LEN) {
        stampChecksum(buf[0..len]);
        if (BsetView.parse(gpa, buf[0..len])) |*v| {
            var vv = v.*;
            defer vv.deinit(gpa);
            for (0..vv.len()) |i| {
                const range = vv.find(vv.record(i).key);
                try testing.expect(range.lo <= i and i < range.hi);
            }
        } else |_| {}
    }
    if (len >= BSET_FRAME_LEN) _ = probeFrame(buf[0..len], 0, 0);
}

test "fuzz: bset parse and the frame probe are total on arbitrary and restamped bytes" {
    try std.testing.fuzz({}, bsetParseFuzz, .{
        .corpus = &.{
            // u32 LE len ‖ bytes: a 32 B header claiming u32::MAX records.
            &([_]u8{ 32, 0, 0, 0 } ++ "BSET".* ++ [_]u8{ 1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF } ++ @as([20]u8, @splat(0))),
            &([_]u8{ 40, 0, 0, 0 } ++ "KBSF".* ++ [_]u8{ 1, 0, 0, 0 } ++ @as([32]u8, @splat(0))),
        },
    });
}
