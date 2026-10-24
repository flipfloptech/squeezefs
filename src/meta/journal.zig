//! The journal ring's on-disk format, write-image builder and replay
//! (charter §5.1 "journal ring", §5.2 "Failure", §5.1 "sector-pad law";
//! `docs/porting/kv_journal_ledger.md` §1, §3.1–3.2, §4, §8, §9).
//!
//! PORTABLE: everything here operates on byte images of the ring. A write is
//! produced as `(offset, bytes)` ops over the extent; replay consumes the
//! extent's image. The io task binds the ops to `uring_fs` and the journal
//! lane's `OwnedRing` (one OS thread per writable volume parking in its own
//! io_uring — the cross-thread wake hop it removes measured 746/664 µs of a
//! 1,209/1,130 µs journal-write round trip = 62 % / 59 % on the shared lane;
//! the lane takes it to 53–107 µs). Nothing here touches a device.
//!
//! Format in one picture (all little-endian; format v4):
//!
//!     page k (4096 B) = header (24 B) ‖ data (4072 B)
//!       header: magic "KVJP" u32 ‖ lap u32 ‖ first_entry_off u16 ‖
//!               reserved u16 (= 0, checked) ‖ appender_id u32 ‖
//!               hash64(bytes[0..16]) u64
//!     logical byte space = the data areas back to back, never wrapping
//!     entry at logical pos p: pos u64 (== p) ‖ len u32 (payload only) ‖
//!               kind u8 (0 = records, 1 = PAD) ‖ reserved[3] (= 0, checked) ‖
//!               hash64(bytes[0..16] ‖ payload) u64 ‖ payload
//!       payload = repeated ( tag u8 ‖ record frame )
//!       record frame (K1's codec; only its framing is needed here) =
//!               key_len u16 ‖ kind u8 ‖ flags u8 (= 0) ‖ val_len u32 ‖
//!               seq u64 ‖ key ‖ value
//!     PAD entry at a reservation's end: an ordinary checksummed entry of
//!     kind PAD whose payload is `pad − 24` zero bytes (charter §5.1)
//!
//! One transaction = one entry = the atomicity unit; an entry packs back to
//! back with the next (its header may straddle a page-data boundary) and
//! continues across pages with ONE header and ONE checksum. Page headers are
//! not part of the logical stream — neither counted nor skipped — the
//! `journal_core.CoreGeometry` mapping places the bytes.

const std = @import("std");
const core_sort = @import("../core/sort.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const jc = @import("journal_core.zig");
const guard = @import("guard.zig");

pub const CoreGeometry = jc.CoreGeometry;
pub const Reservation = jc.Reservation;

// ---------------------------------------------------------------------------
// Constants (`docs/porting/kv_journal_ledger.md` §8 — value and WHY).
// ---------------------------------------------------------------------------

/// The page = the atomic sector unit and the O_DIRECT grain ceiling.
pub const PAGE_LEN: u64 = 4096;
/// `{magic u32, lap u32, first_entry_off u16, reserved u16, appender_id u32,
/// hash64 u64}`: naturally aligned, the checksum over the first 16 B.
pub const PAGE_HDR_LEN: u64 = 24;
pub const PAGE_DATA_LEN: u64 = PAGE_LEN - PAGE_HDR_LEN;
/// ASCII "KVJP" read as a little-endian u32: ASCII order on disk.
pub const PAGE_MAGIC: u32 = std.mem.readInt(u32, "KVJP", .little);
/// An impossible physical in-page offset (> 4095): "no entry starts here" —
/// the page is continuation bytes of an entry begun earlier.
pub const FIRST_ENTRY_NONE: u16 = 0xFFFF;
/// `pos u64 ‖ len u32 ‖ kind u8 ‖ reserved[3] ‖ hash64 u64` (format v4,
/// decision 0003 Q1): naturally aligned, the entry kind explicit.
pub const ENTRY_HDR_LEN: u64 = 24;
/// Whole-entry cap, header included — a charter law (§5.1 "max 128 KiB").
/// WHY it holds: ONE maximal record (a `recordValueCap(1 MiB)` = 65,792 B
/// value under a 17 B xattr key: 65,826 B framed) plus the refs and Δtimes
/// that ride its transaction fits with ~62 KiB to spare, and 128 KiB is one
/// page-aligned run of 32 pages on a 4 KiB-grain ring. It is NOT 2× a
/// maximal record: two maximal values do not fit one entry (the typed
/// `EntryTooLarge` refusal at admission; the tie test below pins both
/// facts). Replay's bound too: a `len` implying more is dropped without
/// ever being dereferenced.
pub const MAX_ENTRY_LEN: u64 = 128 * 1024;
/// The record frame header K1 writes: `key_len u16 ‖ kind u8 ‖ flags u8 ‖
/// val_len u32 ‖ seq u64` (`record.RECORD_HEADER_LEN` asserts equality).
pub const RECORD_HEADER_LEN: u64 = 16;
/// Appender 0 = the format-time fixed ring (charter §5.5); the one-member
/// forest's only writer, and the manager's id on every ring page it owns.
pub const FIXED_RING_APPENDER: u32 = 0;
/// Consecutive journal-write (or barrier) failures that latch the volume
/// FAILED (mutations EIO until remount).
pub const JOURNAL_FAILURE_LATCH: u32 = 3;

/// Ring runway: 32 MiB ≈ 139 K create-sized entries ≈ 4.3 s at 32 K/s; 8 MiB
/// ≈ 35 K ≈ 1.1 s, still past the 1 s checkpoint cadence. 32 MiB is 12 % of
/// a 256 MiB volume.
pub const RING_MIN: u64 = 8 * derive.MiB;
pub const RING_MAX: u64 = 32 * derive.MiB;
/// `--meta-journal-mb` floor: the reserve carve-out (256 KiB at the floor)
/// plus one max-size entry must always be admissible, or a legal
/// transaction could never commit.
pub const RING_OVERRIDE_FLOOR: u64 = 256 * derive.KiB + MAX_ENTRY_LEN;
/// Superblock sector 0; the root ledger follows at 4096 for 128 KiB; the
/// journal extent begins right behind it (135168).
pub const SUPERBLOCK_LEN: u64 = 4096;
pub const ROOT_LEDGER_START: u64 = SUPERBLOCK_LEN;
pub const ROOT_LEDGER_LEN: u64 = 32 * 4096;
pub const JOURNAL_START: u64 = ROOT_LEDGER_START + ROOT_LEDGER_LEN;

/// What an entry carries (the header's `kind` byte).
pub const EntryKind = enum(u8) {
    /// Records: `payload = (tag ‖ record frame)*`.
    records = 0,
    /// The sector-pad law's PAD entry: `payload = 0^len`, skipped as a chain
    /// link by replay.
    pad = 1,
};

comptime {
    // The pad law's two constants live in the dependency-free core; drift
    // between them and the framing here is refused at compile time.
    std.debug.assert(jc.PAGE_HDR_LEN == PAGE_HDR_LEN);
    std.debug.assert(jc.PAD_MIN == ENTRY_HDR_LEN);
    std.debug.assert(PAGE_DATA_LEN == 4072);
    std.debug.assert(JOURNAL_START == 135168);
    std.debug.assert(RING_OVERRIDE_FLOOR == 393216);
}

pub const Error = error{
    /// A structural refusal (mismatched reservation, unaligned run, bad
    /// payload) — never loud at the byte layer.
    Corrupt,
    EntryTooLarge,
    /// `--meta-journal-mb` not a 4 KiB multiple of at least the floor.
    RingOverrideInvalid,
    /// The ring stands within `maxPad` of full and admits not even a pad.
    JournalReserveExhausted,
    OutOfMemory,
};

// ---------------------------------------------------------------------------
// Sizing and placement (`docs/porting/kv_journal_ledger.md` §1.1).
// ---------------------------------------------------------------------------

/// `clamp(volume/64, 8 MiB, 32 MiB)` rounded down to whole pages (the clamp
/// bounds already are, so the rounding never dips below the floor).
pub fn journalRingLen(volume_len: u64) u64 {
    return derive.clamp(volume_len / 64, RING_MIN, RING_MAX) / PAGE_LEN * PAGE_LEN;
}

/// The `--meta-journal-mb` override: REFUSED unless a 4 KiB multiple of at
/// least `RING_OVERRIDE_FLOOR`.
pub fn journalLenOverride(len: u64) Error!u64 {
    if (len % PAGE_LEN != 0 or len < RING_OVERRIDE_FLOOR) return error.RingOverrideInvalid;
    return len;
}

/// The checkpoint-task ring reserve for a ring of `ring_len` PHYSICAL bytes:
/// `max(256 KiB, ring/64)` — 256 KiB at the 8/16 MiB floor, 512 KiB at
/// 32 MiB. Sized in record bytes; on a 4096-grain ring it binds in WINDOWS
/// (one page per SMO record): 64 / 128 per pass.
pub fn checkpointReserveBytes(ring_len: u64) u64 {
    return @max(256 * derive.KiB, ring_len / 64);
}

/// The grain the ring pads to for a device whose direct-I/O grain is
/// `meta_io_grain`: verbatim when a power of two ≤ the page, else 1 (no
/// padding — the buffered posture and a legacy ring).
pub fn ringGrainFor(meta_io_grain: u64) u64 {
    return if (meta_io_grain != 0 and std.math.isPowerOfTwo(meta_io_grain) and meta_io_grain <= PAGE_LEN) meta_io_grain else 1;
}

/// The slack every admission on such a ring claims (`maxPad`): what a "can
/// this ever be admitted" clamp subtracts from the user capacity.
pub fn ringMaxPadFor(meta_io_grain: u64) u64 {
    const g = ringGrainFor(meta_io_grain);
    return if (g <= 1) 0 else jc.PAD_MIN + g;
}

/// The production geometry of a `pages`-page ring on a `grain` device.
pub fn productionGeometry(pages: u64, grain: u64) CoreGeometry {
    return .{ .page_data_len = PAGE_DATA_LEN, .pages = pages, .reserve_bytes = checkpointReserveBytes(pages * PAGE_LEN), .grain = ringGrainFor(grain) };
}

pub const ExtentRef = struct {
    start: u64,
    len: u64,

    pub fn end(self: ExtentRef) u64 {
        return self.start + self.len;
    }
};

/// The journal extent of a `volume_len` volume: at `JOURNAL_START`, sized by
/// the formula or the validated override.
pub fn journalExtent(volume_len: u64, override: ?u64) Error!ExtentRef {
    const len = if (override) |o| try journalLenOverride(o) else journalRingLen(volume_len);
    return .{ .start = JOURNAL_START, .len = len };
}

/// A ring may span up to this many whole heap extents (charter §5.5 "ring
/// segment table ≤ 8": one 256 KiB segment = 64 pages, eight = the 2 MiB
/// largest growth step before the appender page's bytes bind).
pub const RING_SEGMENTS_MAX: u8 = 8;

/// One segment of a ring: `pages` consecutive device pages from `start`.
pub const RingSegment = struct { start: u64, pages: u64 };

/// Where the ring's pages live: `pages` × 4 KiB, either ONE run at byte
/// `base` of the device (the fixed ring; `n_segments == 0`) or a SEGMENT
/// TABLE of whole heap extents in ring order (a joiner's ring, charter
/// §5.5) — logical page `i` maps to the segment that holds it. Positions
/// are a byte space over the pages in table order, so a head continues on
/// any table (replace-when-drained keeps the position). A trailing partial
/// page of any segment is unusable.
pub const RingLayout = struct {
    base: u64,
    pages: u64,
    segments: [RING_SEGMENTS_MAX]RingSegment = @splat(.{ .start = 0, .pages = 0 }),
    /// 0 = the one implicit segment at `base`.
    n_segments: u8 = 0,

    pub fn fromExtent(ext: ExtentRef) RingLayout {
        return .{ .base = ext.start, .pages = ext.len / PAGE_LEN };
    }

    /// A segmented ring: every extent a non-empty whole-page run, ≤ 8.
    pub fn fromSegments(exts: []const ExtentRef) error{ TooManySegments, EmptySegment }!RingLayout {
        if (exts.len == 0 or exts.len > RING_SEGMENTS_MAX) return error.TooManySegments;
        var l: RingLayout = .{ .base = exts[0].start, .pages = 0, .n_segments = @intCast(exts.len) };
        for (exts, 0..) |e, i| {
            const pages = e.len / PAGE_LEN;
            if (pages == 0) return error.EmptySegment;
            l.segments[i] = .{ .start = e.start, .pages = pages };
            l.pages += pages;
        }
        return l;
    }

    pub fn ringBytes(self: RingLayout) u64 {
        return self.pages * PAGE_LEN;
    }

    pub fn pageOffset(self: RingLayout, page: u64) u64 {
        std.debug.assert(page < self.pages);
        if (self.n_segments == 0) return self.base + page * PAGE_LEN;
        var p = page;
        for (self.segments[0..self.n_segments]) |s| {
            if (p < s.pages) return s.start + p * PAGE_LEN;
            p -= s.pages;
        }
        unreachable;
    }

    /// Device offset of logical position `pos`: `page_off + 24 + pos % 4072`
    /// (the crash harness arms write faults at exact entry offsets).
    pub fn physicalOffsetOf(self: RingLayout, geo: CoreGeometry, pos: u64) u64 {
        return self.pageOffset(geo.pageIndex(pos)) + PAGE_HDR_LEN + geo.inPageOff(pos);
    }
};

// ---------------------------------------------------------------------------
// Page header (`docs/porting/kv_journal_ledger.md` §1.2).
// ---------------------------------------------------------------------------

pub const PageHeader = struct {
    /// Low 32 bits of the page's lap (2^32 laps × 8 MiB ≈ 36 PB of journal
    /// writes before a wrap — bounded, stated).
    lap: u32,
    /// PHYSICAL in-page offset (24..4095) of the first entry that STARTS in
    /// this page; `FIRST_ENTRY_NONE` = none.
    first_entry_off: u16,
    /// The appender whose ring segment this page belongs to: a detection-
    /// grade screen of the same class as the frame stamp (charter §5.5
    /// zeroes every carved ring extent, so a foreign page is never
    /// legitimate). u32, the one appender-id width everywhere.
    appender_id: u32,
};

/// The 24 B header image: `[10..12)` is the explicit zero reserved field,
/// all 16 covered by the checksum.
pub fn pageHeaderImage(lap: u32, first_entry_off: u16, appender_id: u32) [PAGE_HDR_LEN]u8 {
    var hdr: [PAGE_HDR_LEN]u8 = @splat(0);
    std.mem.writeInt(u32, hdr[0..4], PAGE_MAGIC, .little);
    std.mem.writeInt(u32, hdr[4..8], lap, .little);
    std.mem.writeInt(u16, hdr[8..10], first_entry_off, .little);
    std.mem.writeInt(u32, hdr[12..16], appender_id, .little);
    std.mem.writeInt(u64, hdr[16..24], checksum.hash64(hdr[0..16]), .little);
    return hdr;
}

/// Total: magic mismatch ⇒ null; checksum mismatch ⇒ null; a non-zero
/// reserved field ⇒ null (a verified liar is not a header).
pub fn parsePageHeader(page: []const u8) ?PageHeader {
    if (page.len < PAGE_HDR_LEN) return null;
    if (std.mem.readInt(u32, page[0..4], .little) != PAGE_MAGIC) return null;
    if (std.mem.readInt(u64, page[16..24], .little) != checksum.hash64(page[0..16])) return null;
    if (std.mem.readInt(u16, page[10..12], .little) != 0) return null;
    return .{
        .lap = std.mem.readInt(u32, page[4..8], .little),
        .first_entry_off = std.mem.readInt(u16, page[8..10], .little),
        .appender_id = std.mem.readInt(u32, page[12..16], .little),
    };
}

// ---------------------------------------------------------------------------
// Tags and record frames (`docs/porting/kv_journal_ledger.md` §1.5).
// ---------------------------------------------------------------------------

/// The tree ids a record tag may carry (charter §5.1 keys: the kind byte IS
/// the historical tree id).
pub const TREE_INODES: u8 = 1;
pub const TREE_ID_MAX: u8 = 9;
/// Interior records of a slot tree: legal only at `level >= 1`.
pub const KIND_INTERIOR: u8 = 0;

/// `tree_id | level << 4`: the high nibble is the node level the record
/// targets (0 = leaf content; > 0 = the SMO task's journaled interior
/// pointers).
pub fn tagFor(tree_id: u8, level: u8) u8 {
    std.debug.assert(tagTreeIdAdmissible(tree_id, level));
    std.debug.assert(level <= 0x0F);
    return tree_id | (level << 4);
}

pub fn tagTreeIdAdmissible(tree_id: u8, level: u8) bool {
    return (tree_id >= TREE_INODES and tree_id <= TREE_ID_MAX) or (tree_id == KIND_INTERIOR and level >= 1);
}

pub const Tag = struct { tree_id: u8, level: u8 };

pub fn untag(tag: u8) Tag {
    return .{ .tree_id = tag & 0x0F, .level = tag >> 4 };
}

pub const RecordKind = enum(u8) { put = 1, delta = 2, delete = 3 };

/// One payload element as the journal sees it: the tag plus K1's record
/// frame, borrowed. Only the framing the journal needs to size, split and
/// route records lives here — the record codec and the fold are K1's.
pub const RecordFrame = struct {
    tag: u8,
    key: []const u8,
    kind: RecordKind,
    seq: u64,
    value: []const u8,

    /// `1 + 16 + key + value`.
    pub fn frameLen(self: RecordFrame) u64 {
        return recordFrameLen(self.key.len, self.value.len);
    }

    pub fn encodeInto(self: RecordFrame, buf: []u8) void {
        std.debug.assert(buf.len == self.frameLen());
        std.debug.assert(self.key.len >= 1 and self.key.len <= std.math.maxInt(u16));
        std.debug.assert(self.kind != .delete or self.value.len == 0);
        buf[0] = self.tag;
        std.mem.writeInt(u16, buf[1..3], @intCast(self.key.len), .little);
        buf[3] = @backingInt(self.kind);
        buf[4] = 0;
        std.mem.writeInt(u32, buf[5..9], @intCast(self.value.len), .little);
        std.mem.writeInt(u64, buf[9..17], self.seq, .little);
        @memcpy(buf[17 .. 17 + self.key.len], self.key);
        @memcpy(buf[17 + self.key.len ..], self.value);
    }
};

/// The journal bytes ONE staged record costs: the tag plus the frame.
pub fn recordFrameLen(key_len: usize, value_len: usize) u64 {
    return 1 + RECORD_HEADER_LEN + @as(u64, key_len) + @as(u64, value_len);
}

/// The payload bytes one entry can carry.
pub fn entryPayloadCap() u64 {
    return MAX_ENTRY_LEN - ENTRY_HDR_LEN;
}

/// Exact whole-entry size for staged records — the admission size (a
/// committer's staged records fix its exact entry size before any lock).
pub fn entryLenFor(records: []const RecordFrame) Error!u64 {
    var payload: u64 = 0;
    for (records) |r| payload += r.frameLen();
    const len = ENTRY_HDR_LEN + payload;
    if (len > MAX_ENTRY_LEN) return error.EntryTooLarge;
    return len;
}

pub fn payloadLenFor(records: []const RecordFrame) u64 {
    var n: u64 = 0;
    for (records) |r| n += r.frameLen();
    return n;
}

/// Encode staged records back to back into `buf` (sized `payloadLenFor`).
pub fn encodeEntryPayload(buf: []u8, records: []const RecordFrame) void {
    var pos: usize = 0;
    for (records) |r| {
        const n: usize = @intCast(r.frameLen());
        r.encodeInto(buf[pos .. pos + n]);
        pos += n;
    }
    std.debug.assert(pos == buf.len);
}

pub const Decoded = struct { frame: RecordFrame, used: usize };

/// Decode one `tag ‖ frame` element. Total (§9): the tag must be admissible,
/// the header must fit, `key_len ≠ 0`, `kind ∈ {1,2,3}`, `flags == 0`,
/// Delete ⇒ `val_len == 0`, and `16 + key_len + val_len` (no overflow in
/// u64) must fit.
pub fn decodeRecordFrame(buf: []const u8) Error!Decoded {
    if (buf.len < 1 + RECORD_HEADER_LEN) return error.Corrupt;
    const tag = buf[0];
    const t = untag(tag);
    if (!tagTreeIdAdmissible(t.tree_id, t.level)) return error.Corrupt;
    const hdr = buf[1 .. 1 + RECORD_HEADER_LEN];
    const key_len: u64 = std.mem.readInt(u16, hdr[0..2], .little);
    if (key_len == 0) return error.Corrupt;
    const kind: RecordKind = switch (hdr[2]) {
        1 => .put,
        2 => .delta,
        3 => .delete,
        else => return error.Corrupt,
    };
    if (hdr[3] != 0) return error.Corrupt;
    const val_len: u64 = std.mem.readInt(u32, hdr[4..8], .little);
    const seq = std.mem.readInt(u64, hdr[8..16], .little);
    if (kind == .delete and val_len != 0) return error.Corrupt;
    const total = 1 + RECORD_HEADER_LEN + key_len + val_len;
    if (total > buf.len) return error.Corrupt;
    const key_start: usize = @intCast(1 + RECORD_HEADER_LEN);
    const key_end: usize = @intCast(key_start + key_len);
    return .{
        .frame = .{ .tag = tag, .key = buf[key_start..key_end], .kind = kind, .seq = seq, .value = buf[key_end..@intCast(total)] },
        .used = @intCast(total),
    };
}

/// Validate a whole entry payload: every element decodes and they tile the
/// buffer exactly. Answers the record count.
pub fn validateEntryPayload(buf: []const u8) Error!u32 {
    var pos: usize = 0;
    var n: u32 = 0;
    while (pos < buf.len) : (n += 1) {
        const d = try decodeRecordFrame(buf[pos..]);
        pos += d.used;
    }
    return n;
}

/// Iterate the frames of a VALIDATED payload.
pub const FrameIter = struct {
    buf: []const u8,
    pos: usize = 0,

    pub fn next(self: *FrameIter) ?RecordFrame {
        if (self.pos >= self.buf.len) return null;
        const d = decodeRecordFrame(self.buf[self.pos..]) catch unreachable; // validated at parse
        self.pos += d.used;
        return d.frame;
    }
};

// ---------------------------------------------------------------------------
// Entry framing (`docs/porting/kv_journal_ledger.md` §1.4, §1.6).
// ---------------------------------------------------------------------------

/// The entry header's checksummed prefix: `pos u64 ‖ len u32 ‖ kind u8 ‖
/// reserved[3]` — the 16 B before the checksum.
pub const ENTRY_HDR_CHECKSUM_OFF: usize = 16;

fn entryHeaderPrefix(pos: u64, len: u32, kind: EntryKind) [ENTRY_HDR_CHECKSUM_OFF]u8 {
    var prefix: [ENTRY_HDR_CHECKSUM_OFF]u8 = @splat(0);
    std.mem.writeInt(u64, prefix[0..8], pos, .little);
    std.mem.writeInt(u32, prefix[8..12], len, .little);
    prefix[12] = @backingInt(kind);
    return prefix;
}

/// `hash64(pos ‖ len ‖ kind ‖ reserved ‖ payload)` with the payload as
/// slices (no copy).
pub fn entryChecksumParts(pos: u64, len: u32, kind: EntryKind, parts: []const []const u8) u64 {
    var h: checksum.Hasher = .init();
    h.update(&entryHeaderPrefix(pos, len, kind));
    for (parts) |p| h.update(p);
    return h.finalLow64();
}

pub fn entryChecksum(pos: u64, kind: EntryKind, payload: []const u8) u64 {
    return entryChecksumParts(pos, @intCast(payload.len), kind, &.{payload});
}

/// Frame one entry at logical `start` into `buf` (sized `24 + payload.len`).
pub fn frameEntry(buf: []u8, start: u64, kind: EntryKind, payload: []const u8) void {
    std.debug.assert(buf.len == ENTRY_HDR_LEN + payload.len);
    buf[0..ENTRY_HDR_CHECKSUM_OFF].* = entryHeaderPrefix(start, @intCast(payload.len), kind);
    std.mem.writeInt(u64, buf[ENTRY_HDR_CHECKSUM_OFF..][0..8], entryChecksum(start, kind, payload), .little);
    @memcpy(buf[ENTRY_HDR_LEN..], payload);
}

/// `first_entry_off` for the header of the page starting at `page_start`
/// owned by the entry `[start, end)`: 24 when the entry starts here; the
/// successor's start when it ENDS inside this page (a successor never written
/// still gets its predicted start — replay treats a `feo` pointing at garbage
/// as end-of-log); `FIRST_ENTRY_NONE` when it spans the page entirely.
pub fn firstEntryOffFor(geo: CoreGeometry, start: u64, end: u64, page_start: u64) u16 {
    if (start == page_start) return @intCast(PAGE_HDR_LEN);
    if (end < page_start + geo.page_data_len) return @intCast(PAGE_HDR_LEN + (end - page_start));
    return FIRST_ENTRY_NONE;
}

// ---------------------------------------------------------------------------
// Write ops and the aligned image builder (§1.8, §1.9, §3.1, §3.2).
// ---------------------------------------------------------------------------

/// One device write: `bytes` at `offset`. Bytes are owned by the builder's
/// arena (slices of one whole-entry buffer; a 24 B header image; or one
/// zero-filled aligned run).
pub const WriteOp = struct {
    offset: u64,
    bytes: []const u8,

    pub fn end(self: WriteOp) u64 {
        return self.offset + self.bytes.len;
    }
};

/// What ONE submission puts on the device, by class — the metadata-WRITE
/// ledger's journal terms (decision 0003 §L). Exact-sum by construction:
/// `run_bytes ≡ entry_bytes + pad_bytes + header_bytes + hole_bytes`
/// (`closes`), where `run_bytes` is what the device sees (Σ the aligned
/// runs' lengths), `hole_bytes` a rolled-out member's zero-filled range
/// inside the window (0 on an unpadded ring, where nothing is written for
/// it), `header_bytes` the 24 B page headers the run carries.
pub const WriteAccounting = struct {
    entry_bytes: u64 = 0,
    pad_bytes: u64 = 0,
    header_bytes: u64 = 0,
    hole_bytes: u64 = 0,
    run_bytes: u64 = 0,

    pub fn closes(self: WriteAccounting) bool {
        return self.run_bytes == self.entry_bytes + self.pad_bytes + self.header_bytes + self.hole_bytes;
    }

    /// The window carried more pad than entries: the pad law's cost
    /// exceeded its payload (the conveyor's hold judges the SAME predicate
    /// before issuing: `conveyor.padMajority`).
    pub fn padMajority(self: WriteAccounting) bool {
        return self.pad_bytes > self.entry_bytes;
    }
};

/// The ops of one submission plus the arena that owns their bytes, and
/// the submission's device accounting.
pub const WriteImage = struct {
    arena: std.heap.ArenaAllocator,
    ops: []WriteOp,
    acct: WriteAccounting,

    pub fn deinit(self: *WriteImage) void {
        self.arena.deinit();
    }
};

/// The page headers a span's aligned runs carry: one per page whose first
/// byte lies inside `[start, start + len)` (that page's run begins at the
/// page BASE). On an unpadded ring the headers written are the entries'
/// own owned ones (counted at framing), never a hole's.
fn headerBytesOfSpan(geo: CoreGeometry, start: u64, len: u64) u64 {
    var n: u64 = 0;
    var it = geo.ownedPageStarts(start, len);
    while (it.next()) |_| n += 1;
    return n * PAGE_HDR_LEN;
}

fn runBytesOf(ops: []const WriteOp) u64 {
    var n: u64 = 0;
    for (ops) |op| n += op.bytes.len;
    return n;
}

/// The accounting of a window `window` whose survivors sum to
/// `entry_bytes`, over its aligned (or verbatim) `ops`.
fn accountWindow(geo: CoreGeometry, window: Reservation, entry_bytes: u64, header_ops_bytes: u64, ops: []const WriteOp) WriteAccounting {
    const padded = geo.grain > 1;
    const acct: WriteAccounting = .{
        .entry_bytes = entry_bytes,
        .pad_bytes = window.pad,
        .header_bytes = if (padded) headerBytesOfSpan(geo, window.start, window.paddedLen()) else header_ops_bytes,
        .hole_bytes = if (padded) window.len - entry_bytes else 0,
        .run_bytes = runBytesOf(ops),
    };
    std.debug.assert(acct.closes());
    return acct;
}

fn opLessThan(_: void, a: WriteOp, b: WriteOp) bool {
    return a.offset < b.offset;
}

/// One framed entry (24 B header + `payload`) at logical `start`: payload
/// segment ops (slices of ONE whole-entry buffer) then the owned page
/// headers. The order within one batch carries no durability meaning
/// (unordered writeback is the crash model either way); it lets the torn-
/// batch harness exercise payload-landed/header-lost shapes. Answers the
/// page-header bytes appended (the unpadded ring's header accounting).
fn framedEntryOps(arena: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, start: u64, kind: EntryKind, payload: []const u8, ops: *std.ArrayList(WriteOp)) Allocator.Error!u64 {
    const whole = ENTRY_HDR_LEN + payload.len;
    const entry = try arena.alloc(u8, @intCast(whole));
    frameEntry(entry, start, kind, payload);
    const end = start + whole;
    var consumed: usize = 0;
    var segs = geo.segments(start, whole);
    while (segs.next()) |seg| {
        const file_off = layout.pageOffset(seg.page) + PAGE_HDR_LEN + seg.data_off;
        const n: usize = @intCast(seg.len);
        try ops.append(arena, .{ .offset = file_off, .bytes = entry[consumed .. consumed + n] });
        consumed += n;
    }
    std.debug.assert(consumed == entry.len);
    var header_bytes: u64 = 0;
    var owned = geo.ownedPageStarts(start, whole);
    while (owned.next()) |page_start| {
        const feo = firstEntryOffFor(geo, start, end, page_start);
        const hdr = try arena.alloc(u8, PAGE_HDR_LEN);
        hdr[0..PAGE_HDR_LEN].* = pageHeaderImage(@truncate(geo.lap(page_start)), feo, appender_id);
        try ops.append(arena, .{ .offset = layout.pageOffset(geo.pageIndex(page_start)), .bytes = hdr });
        header_bytes += PAGE_HDR_LEN;
    }
    return header_bytes;
}

/// The entry's own bytes: `entryLenFor(records)` must equal `res.len`.
/// Answers the page-header bytes appended.
fn entryOps(arena: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, res: Reservation, records: []const RecordFrame, ops: *std.ArrayList(WriteOp)) Error!u64 {
    const need = try entryLenFor(records);
    if (need != res.len) return error.Corrupt;
    const payload = try arena.alloc(u8, @intCast(payloadLenFor(records)));
    encodeEntryPayload(payload, records);
    return framedEntryOps(arena, layout, geo, appender_id, res.start, .records, payload, ops);
}

/// The PAD entry behind a reservation: an ordinary checksummed entry of kind
/// PAD at `end` spanning `pad` bytes (a zero payload of `pad − 24`), owning
/// (and writing the headers of) the pages whose first byte it covers exactly
/// as an entry would. Nothing when `pad == 0`. Answers the page-header
/// bytes appended.
fn padOps(arena: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, end: u64, pad: u64, ops: *std.ArrayList(WriteOp)) Allocator.Error!u64 {
    if (pad == 0) return 0;
    std.debug.assert(pad >= jc.PAD_MIN);
    const payload = try arena.alloc(u8, @intCast(pad - ENTRY_HDR_LEN));
    @memset(payload, 0);
    return framedEntryOps(arena, layout, geo, appender_id, end, .pad, payload, ops);
}

/// THE ring's aligned form (charter §5.1 "a window's ops coalesce into one
/// aligned run per contiguous span"): on a sector-padded ring the ops of one
/// logical span `[start, end)` — entries, their pad, the owned page headers —
/// are COALESCED into one image per physically contiguous run (adjacent
/// pages merge; a segment boundary or the ring wrap starts a new run),
/// zero-filled where nothing was written (a rolled-back member's hole, a
/// header no survivor owns — garbage to the chain either way, exactly as the
/// unwritten bytes were). A run beginning at a page's data start begins at
/// the PAGE BASE so the header op lands in the same image. The pad law makes
/// every run's ends grain-aligned; a run that is not is a protocol bug
/// refused here (`error.Corrupt`) and NOTHING is written. On an unpadded
/// ring the ops pass through verbatim (the pre-pad-law bytes, op for op).
pub fn alignedOps(arena: Allocator, layout: RingLayout, geo: CoreGeometry, start: u64, end: u64, ops: []WriteOp) Error![]WriteOp {
    if (geo.grain <= 1 or end <= start) return ops;
    const grain = geo.grain;
    const Run = struct { lo: u64, hi: u64 };
    var runs: std.ArrayList(Run) = .empty;
    var segs = geo.segments(start, end - start);
    while (segs.next()) |seg| {
        const page_base = layout.pageOffset(seg.page);
        const lo = if (seg.data_off == 0) page_base else page_base + PAGE_HDR_LEN + seg.data_off;
        const hi = page_base + PAGE_HDR_LEN + seg.data_off + seg.len;
        if (runs.items.len > 0 and runs.items[runs.items.len - 1].hi == lo) {
            runs.items[runs.items.len - 1].hi = hi;
        } else try runs.append(arena, .{ .lo = lo, .hi = hi });
    }
    for (runs.items) |run| {
        if (run.lo % grain != 0 or run.hi % grain != 0) return error.Corrupt;
    }
    core_sort.stable(WriteOp, ops, {}, opLessThan);
    const out = try arena.alloc(WriteOp, runs.items.len);
    for (runs.items, 0..) |run, i| {
        // Grain-aligned so a direct write needs no bounce; the io layer's
        // posture check accepts it verbatim.
        const image = try arena.alignedAlloc(u8, .fromByteUnits(4096), @intCast(run.hi - run.lo));
        @memset(image, 0);
        for (ops) |op| {
            if (op.offset < run.lo or op.offset >= run.hi) continue;
            const at: usize = @intCast(op.offset - run.lo);
            if (at + op.bytes.len > image.len) return error.Corrupt;
            @memcpy(image[at .. at + op.bytes.len], op.bytes);
        }
        out[i] = .{ .offset = run.lo, .bytes = image };
    }
    return out;
}

/// One entry's write: its bytes plus the reservation's PAD entry, in the
/// ring's aligned form. One op ⇒ `write_at`; more ⇒ `write_at_batch`
/// (the io binding's choice).
pub fn buildEntryWrite(gpa: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, res: Reservation, records: []const RecordFrame) Error!WriteImage {
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    var ops: std.ArrayList(WriteOp) = .empty;
    var header_ops_bytes = try entryOps(a, layout, geo, appender_id, res, records, &ops);
    header_ops_bytes += try padOps(a, layout, geo, appender_id, res.end(), res.pad, &ops);
    const aligned = try alignedOps(a, layout, geo, res.start, res.paddedEnd(), ops.items);
    return .{ .arena = arena, .ops = aligned, .acct = accountWindow(geo, res, res.len, header_ops_bytes, aligned) };
}

/// One surviving member of a conveyor batch: its exact-sized sub-reservation
/// inside the window and its records.
pub const BatchPart = struct {
    res: Reservation,
    records: []const RecordFrame,
    /// The member rolled out at apply: a PAD entry over its exact range
    /// (no hole — the chain walks past it, so a survivor behind it that
    /// was acked ahead of the landing is reachable at replay).
    pad: bool = false,
};

/// One member's ops into `ops` (the self-applying committer builds its
/// own, §BC): its framed entry, or a PAD entry over its range when it
/// rolled out. Answers the page-header bytes appended.
pub fn partOps(arena: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, part: BatchPart, ops: *std.ArrayList(WriteOp)) Error!u64 {
    if (part.pad) return padOps(arena, layout, geo, appender_id, part.res.start, part.res.len, ops);
    return entryOps(arena, layout, geo, appender_id, part.res, part.records, ops);
}

/// A conveyor window's write from its members' ops already built
/// (`partOps`, each on its committer): the window's PAD entry, then the
/// ring's aligned form. `entry_bytes` = Σ the surviving members' lengths.
pub fn buildWindowFromOps(gpa: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, window: Reservation, member_ops: []const []const WriteOp, entry_bytes: u64, header_bytes: u64) Error!WriteImage {
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    var n: usize = 0;
    for (member_ops) |m| n += m.len;
    var ops: std.ArrayList(WriteOp) = try .initCapacity(a, n + 8);
    for (member_ops) |m| ops.appendSliceAssumeCapacity(m);
    const header_ops_bytes = header_bytes + try padOps(a, layout, geo, appender_id, window.end(), window.pad, &ops);
    const aligned = try alignedOps(a, layout, geo, window.start, window.paddedEnd(), ops.items);
    return .{ .arena = arena, .ops = aligned, .acct = accountWindow(geo, window, entry_bytes, header_ops_bytes, aligned) };
}

/// A conveyor batch's write: N ordinary checksummed entries at back-to-back
/// reservations inside ONE registered `window` (its span and its pad — the
/// pad entry is written with the batch whatever its last member's fate), as
/// ONE submission. `parts` are the survivors' sub-ranges: non-overlapping,
/// each exact-sized, NOT necessarily contiguous — a rolled-back member is
/// simply absent (an unwritten hole; its page headers are written iff a
/// survivor owns the page's first byte). Each member's bytes are BY
/// CONSTRUCTION identical to a solo `buildEntryWrite` of the same
/// reservation (shared `entryOps`) — the batch-of-1 equivalence.
pub fn buildBatchWrite(gpa: Allocator, layout: RingLayout, geo: CoreGeometry, appender_id: u32, window: Reservation, parts: []const BatchPart) Error!WriteImage {
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    var ops: std.ArrayList(WriteOp) = .empty;
    var header_ops_bytes: u64 = 0;
    var entry_bytes: u64 = 0;
    for (parts) |p| {
        header_ops_bytes += try partOps(a, layout, geo, appender_id, p, &ops);
        if (!p.pad) entry_bytes += p.res.len;
    }
    header_ops_bytes += try padOps(a, layout, geo, appender_id, window.end(), window.pad, &ops);
    const aligned = try alignedOps(a, layout, geo, window.start, window.paddedEnd(), ops.items);
    return .{ .arena = arena, .ops = aligned, .acct = accountWindow(geo, window, entry_bytes, header_ops_bytes, aligned) };
}

/// Land `ops` on an image of the extent whose first byte is device offset
/// `base` — the "device" of the image tests and the harness.
pub fn applyOps(image: []u8, base: u64, ops: []const WriteOp) void {
    for (ops) |op| {
        const at: usize = @intCast(op.offset - base);
        @memcpy(image[at .. at + op.bytes.len], op.bytes);
    }
}

// ---------------------------------------------------------------------------
// Recovery head alignment — the ring's one honest RMW (§1.10).
// ---------------------------------------------------------------------------

/// A grain-covering device span `[lo, hi)`.
pub const Span = struct { lo: u64, hi: u64 };

/// The merged grain-aligned spans covering `ops`: each is read fresh, the ops
/// overlaid (`overlayOps`), the span written whole.
pub fn rmwSpans(arena: Allocator, grain: u64, ops: []const WriteOp) Allocator.Error![]Span {
    const spans = try arena.alloc(Span, ops.len);
    for (ops, 0..) |op, i| spans[i] = .{ .lo = op.offset - op.offset % grain, .hi = derive.ceilDiv(op.end(), grain) * grain };
    core_sort.stable(Span, spans, {}, struct {
        fn lt(_: void, a: Span, b: Span) bool {
            return a.lo < b.lo or (a.lo == b.lo and a.hi < b.hi);
        }
    }.lt);
    var n: usize = 0;
    for (spans) |s| {
        if (n > 0 and s.lo <= spans[n - 1].hi) {
            spans[n - 1].hi = @max(spans[n - 1].hi, s.hi);
        } else {
            spans[n] = s;
            n += 1;
        }
    }
    return spans[0..n];
}

/// Overlay the ops that fall inside the span whose fresh bytes are `dst`
/// (`dst[0]` is device offset `lo`).
pub fn overlayOps(dst: []u8, lo: u64, ops: []const WriteOp) void {
    for (ops) |op| {
        if (op.offset < lo or op.offset >= lo + dst.len) continue;
        const at: usize = @intCast(op.offset - lo);
        @memcpy(dst[at .. at + op.bytes.len], op.bytes);
    }
}

/// A planned recovery pad: the registered zero-length reservation carrying
/// the pad, its PAD-entry ops, and the covering spans to read-modify-write.
/// The caller lands the spans, then `finishHeadAlignment` on EITHER outcome.
pub const HeadAlignment = struct {
    arena: std.heap.ArenaAllocator,
    res: Reservation,
    /// The PAD entry's ops (unaligned offsets: the pad begins mid-sector).
    ops: []WriteOp,
    /// Empty on an unpadded/buffered ring (the ops land as they are).
    spans: []Span,
    /// The spans' device cost (the pad, its headers, the rewritten prefix).
    acct: WriteAccounting,

    pub fn deinit(self: *HeadAlignment) void {
        self.arena.deinit();
    }
};

/// A WRITER's ring whose replayed head stands mid-sector — a window torn
/// behind its entry (the entry landed, its pad did not), or a ring written
/// buffered before this binary — writes ONE PAD entry from the head to the
/// next boundary so its first reservation begins aligned. Null when the head
/// is aligned (every unpadded ring). `error.JournalReserveExhausted` when the
/// ring stands within `maxPad` of full and admits nothing: the caller DEFERS
/// the pad to the guarded checkpoint cycles that move the tail (up to 64 of
/// them); nothing is written on that answer. The RMW is honest here alone:
/// at open, no reservation open, single writer, the sector's prefix the
/// durable image just replayed. A reader never calls this.
pub fn planHeadAlignment(gpa: Allocator, core: *jc.JournalCore, reg: *jc.InflightRegistry, layout: RingLayout, appender_id: u32) Error!?HeadAlignment {
    const geo = core.geometry();
    const head = core.headPos();
    const pad = geo.padOf(head);
    if (pad == 0) return null;
    const adm = core.tryAdmit(0, .checkpoint) orelse return error.JournalReserveExhausted;
    const r = try reg.reserveRegistered(core, adm);
    std.debug.assert(r.res.start == head and r.res.len == 0 and r.res.pad == pad);
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    var ops: std.ArrayList(WriteOp) = .empty;
    const header_bytes = try padOps(a, layout, geo, appender_id, r.res.start, r.res.pad, &ops);
    const spans = try rmwSpans(a, geo.grain, ops.items);
    var run_bytes: u64 = 0;
    for (spans) |s| run_bytes += s.hi - s.lo;
    // The RMW rewrites the grain's prefix (the replayed sector's own bytes)
    // beside the pad: that prefix is neither entry nor pad of THIS write —
    // it rides `hole_bytes` so the pad's device cost still closes exactly.
    const acct: WriteAccounting = .{ .pad_bytes = r.res.pad, .header_bytes = header_bytes, .hole_bytes = run_bytes - r.res.pad - header_bytes, .run_bytes = run_bytes };
    return .{ .arena = arena, .res = r.res, .ops = ops.items, .spans = spans, .acct = acct };
}

/// Complete the pad's reservation (on BOTH outcomes — an open reservation
/// would wedge the completed-prefix watermark) and account a landed pad.
pub fn finishHeadAlignment(plan: *const HeadAlignment, core: *const jc.JournalCore, reg: *jc.InflightRegistry, stats: *RingStats, landed: bool) void {
    reg.complete(core, plan.res);
    if (!landed) return;
    stats.noteWritten(plan.acct, 0);
    log.info("journal ring: recovered head {d} stood mid-sector — a {d} B pad entry aligns the first reservation to the {d} B grain", .{ plan.res.start, plan.res.pad, core.geometry().grain });
}

// ---------------------------------------------------------------------------
// Replay (`docs/porting/kv_journal_ledger.md` §4, §9).
// ---------------------------------------------------------------------------

/// One replayed transaction: the entry seq (== its start position) and its
/// validated payload (records in entry order via `records()`).
pub const ReplayedEntry = struct {
    seq: u64,
    payload: []const u8,

    pub fn records(self: ReplayedEntry) FrameIter {
        return .{ .buf = self.payload };
    }
};

pub const JournalRecovery = struct {
    arena: std.heap.ArenaAllocator,
    /// Seq-sorted.
    entries: []ReplayedEntry,
    /// One past the last verified chain link, ≥ tail: the core resumes here
    /// with `reusable_upto = tail`.
    head_pos: u64,
    /// Confirmed drops: a failure followed by a LATER verified link. A
    /// trailing failure is never confirmed, so a clean unmount replays with 0.
    dropped_torn: u64,
    /// Verifying page headers naming another appender (whole ring).
    foreign_pages: u64,

    pub fn deinit(self: *JournalRecovery) void {
        self.arena.deinit();
    }
};

const Parsed = union(enum) {
    /// A verified entry: its payload (in the recovery arena) and end.
    entry: struct { payload: []const u8, end: u64 },
    /// A verified PAD: a chain link with no records.
    pad: u64,
    /// No valid entry at this position (torn / garbage / stale / end-of-log).
    fail,
};

fn imageOffset(seg: jc.PageSegment) usize {
    return @intCast(seg.page * PAGE_LEN + PAGE_HDR_LEN + seg.data_off);
}

/// Read the logical range `[pos, pos + out.len)` out of the ring image.
fn readLogical(image: []const u8, geo: CoreGeometry, pos: u64, out: []u8) void {
    var filled: usize = 0;
    var segs = geo.segments(pos, out.len);
    while (segs.next()) |seg| {
        const at = imageOffset(seg);
        const n: usize = @intCast(seg.len);
        @memcpy(out[filled .. filled + n], image[at .. at + n]);
        filled += n;
    }
}

/// Parse + verify the entry at logical `pos`. Check ORDER: the header fits the
/// window; `pos` field == pos (the stale-lap / end-of-log rejector); `len`
/// bounded by the cap AND the window BEFORE any byte it governs is read; the
/// checksum straight out of the image segments (no copy); then the kind
/// decides: PAD ⇒ an all-zero payload; records ⇒ the payload decodes. The
/// reserved bytes and an unknown kind fail like any non-canonical bytes
/// (they are under the checksum, so a verified one is a writer bug).
fn parseEntryAt(arena: Allocator, image: []const u8, geo: CoreGeometry, pos: u64, chain_end: u64) Allocator.Error!Parsed {
    if (pos + ENTRY_HDR_LEN > chain_end) return .fail;
    var hdr: [ENTRY_HDR_LEN]u8 = undefined;
    readLogical(image, geo, pos, &hdr);
    const stamped_pos = std.mem.readInt(u64, hdr[0..8], .little);
    const len = std.mem.readInt(u32, hdr[8..12], .little);
    const stored = std.mem.readInt(u64, hdr[ENTRY_HDR_CHECKSUM_OFF..][0..8], .little);
    if (stamped_pos != pos) return .fail;
    const whole = ENTRY_HDR_LEN + @as(u64, len);
    // The garbage-len guard: rejected on bounds alone.
    if (whole > MAX_ENTRY_LEN or pos + whole > chain_end) return .fail;
    const payload_pos = pos + ENTRY_HDR_LEN;
    var h: checksum.Hasher = .init();
    h.update(hdr[0..ENTRY_HDR_CHECKSUM_OFF]);
    var segs = geo.segments(payload_pos, len);
    while (segs.next()) |seg| {
        const at = imageOffset(seg);
        h.update(image[at .. at + @as(usize, @intCast(seg.len))]);
    }
    if (h.finalLow64() != stored) return .fail;
    if (!std.mem.allEqual(u8, hdr[13..16], 0)) return .fail;
    const kind: EntryKind = switch (hdr[12]) {
        @backingInt(EntryKind.records) => .records,
        @backingInt(EntryKind.pad) => .pad,
        else => return .fail,
    };
    const payload = try arena.alloc(u8, len);
    readLogical(image, geo, payload_pos, payload);
    switch (kind) {
        .pad => {
            if (!std.mem.allEqual(u8, payload, 0)) return .fail;
            return .{ .pad = pos + whole };
        },
        .records => {
            // The checksum verified but the records do not decode: a
            // writer-bug class — dropped like any invalid chain structure,
            // never loud.
            _ = validateEntryPayload(payload) catch return .fail;
            return .{ .entry = .{ .payload = payload, .end = pos + whole } };
        },
    }
}

/// The replay scan over a ring image (`pages × 4096` bytes; a short device
/// read is zero-extended by the caller — zeros verify nothing). Pure over
/// bytes; never fails on contents (the only error is allocation).
///
/// Chain-PRIMARY: the tail is always an entry boundary (the tail rule rounds
/// to entry starts), so the walk starts AT `tail`; page headers are consulted
/// only to RESYNC after a failure — at the next window page whose header
/// verifies for its expected lap, names this writer, and carries a
/// `first_entry_off ≠ 0xFFFF`. An entry continuing through a dead-header page
/// is still read at chain-known offsets and its checksum decides. Failures
/// are PENDING until a later link confirms them (`dropped_torn`); a trailing
/// failure is end-of-log.
pub fn replayScanImage(gpa: Allocator, image: []const u8, geo: CoreGeometry, tail: u64, expect_appender: u32) Allocator.Error!JournalRecovery {
    std.debug.assert(image.len >= geo.pages * PAGE_LEN);
    var arena = std.heap.ArenaAllocator.init(gpa);
    errdefer arena.deinit();
    const a = arena.allocator();
    const l = geo.logicalLen();
    const window_base = geo.pageStartPos(tail);
    // Entries live in [tail, tail + L). Discovery slots cover every page once
    // from the tail page onward, plus — for a mid-page tail — the tail page's
    // second occurrence, whose sliver [window_base + L, tail + L) is writable.
    const chain_end = tail + l;
    const slots: u64 = if (window_base < tail) geo.pages + 1 else geo.pages;

    // Verify every physical page header once.
    const headers = try a.alloc(?PageHeader, @intCast(geo.pages));
    var foreign_pages: u64 = 0;
    for (headers, 0..) |*h, k| {
        h.* = parsePageHeader(image[k * @as(usize, PAGE_LEN) ..][0..PAGE_HDR_LEN]);
        if (h.*) |ph| {
            if (ph.appender_id != expect_appender) foreign_pages += 1;
        }
    }

    var entries: std.ArrayList(ReplayedEntry) = .empty;
    var dropped: u64 = 0;
    var pending: u64 = 0;
    var head_pos = tail;
    var cursor: ?u64 = tail;
    var next_slot: u64 = 0;
    while (true) {
        if (cursor) |pos| {
            // The chain ran exactly to the window end: done.
            if (pos >= chain_end) break;
            switch (try parseEntryAt(a, image, geo, pos, chain_end)) {
                .entry => |e| {
                    // A recovered link confirms every pending drop before it.
                    dropped += pending;
                    pending = 0;
                    head_pos = e.end;
                    if (pos >= tail) try entries.append(a, .{ .seq = pos, .payload = e.payload });
                    cursor = e.end;
                },
                .pad => |end| {
                    dropped += pending;
                    pending = 0;
                    head_pos = end;
                    cursor = end;
                },
                .fail => {
                    pending += 1;
                    next_slot = (geo.pageStartPos(pos) - window_base) / geo.page_data_len + 1;
                    cursor = null;
                },
            }
            continue;
        }
        // Resync.
        var found: ?u64 = null;
        while (next_slot < slots) {
            const pos_i = window_base + next_slot * geo.page_data_len;
            next_slot += 1;
            const page: usize = @intCast(geo.pageIndex(pos_i));
            const ph = headers[page] orelse {
                // Unverifiable: entries starting here are lost — a discovery
                // hole, counted iff a later link confirms.
                pending += 1;
                continue;
            };
            if (ph.appender_id != expect_appender) {
                // Foreign (checked BEFORE the lap gate so a foreign page whose
                // lap matches is never followed): a discovery hole, attributed
                // in `foreign_pages`, never a tear.
                pending += 1;
                continue;
            }
            if (ph.lap != @as(u32, @truncate(geo.lap(pos_i)))) {
                pending += 1;
                continue;
            }
            if (ph.first_entry_off == FIRST_ENTRY_NONE) continue; // attested continuation page
            if (ph.first_entry_off < PAGE_HDR_LEN or ph.first_entry_off >= PAGE_LEN) {
                // Checksummed but impossible: a discovery hole.
                pending += 1;
                continue;
            }
            found = pos_i + (ph.first_entry_off - PAGE_HDR_LEN);
            break;
        }
        cursor = found orelse break;
    }
    // Positions increase along the walk, so this is already sorted; the
    // contract promises seq order, so keep the defensive sort.
    std.mem.sort(ReplayedEntry, entries.items, {}, struct {
        fn lt(_: void, x: ReplayedEntry, y: ReplayedEntry) bool {
            return x.seq < y.seq;
        }
    }.lt);
    return .{ .arena = arena, .entries = entries.items, .head_pos = head_pos, .dropped_torn = dropped, .foreign_pages = foreign_pages };
}

/// The record-seq OFFSET a replay window attests: every record was stamped
/// `entry position + offset + index`, so `seq − (entry.seq + index)` reads the
/// offset in force at its stamp; the max over the window is the newest. 0 on
/// every window a ring stamped at its positions (the shipped ring).
pub fn seqOffsetOfWindow(entries: []const ReplayedEntry) u64 {
    var best: u64 = 0;
    for (entries) |e| {
        var it = e.records();
        var i: u64 = 0;
        while (it.next()) |r| : (i += 1) best = @max(best, r.seq -| (e.seq +| i));
    }
    return best;
}

/// One record of the replay window addressed for the two-phase apply.
pub const ReplayRecordRef = struct {
    /// The containing entry's start — the record's floor contribution.
    entry_seq: u64,
    /// Index within the entry (`record_gate(index)`).
    index: u32,
    level: u8,
    tree_id: u8,
    record: RecordFrame,
};

fn interiorLessThan(_: void, a: ReplayRecordRef, b: ReplayRecordRef) bool {
    if (a.level != b.level) return a.level > b.level;
    return a.record.seq < b.record.seq;
}

/// Two-phase replay's phase-1 order (charter §5.3 "phase 1 interior records
/// `(level DESC, seq)`"): every `level > 0` record of the window sorted by
/// level descending, then record seq ascending. Phase 2 walks the same
/// `entries` applying `level == 0` content records by key through the final
/// routing with their original seqs (per-key LWW).
pub fn interiorRecordsSorted(gpa: Allocator, entries: []const ReplayedEntry) Allocator.Error![]ReplayRecordRef {
    var out: std.ArrayList(ReplayRecordRef) = .empty;
    errdefer out.deinit(gpa);
    for (entries) |e| {
        var it = e.records();
        var i: u32 = 0;
        while (it.next()) |r| : (i += 1) {
            const t = untag(r.tag);
            if (t.level == 0) continue;
            try out.append(gpa, .{ .entry_seq = e.seq, .index = i, .level = t.level, .tree_id = t.tree_id, .record = r });
        }
    }
    std.mem.sort(ReplayRecordRef, out.items, {}, interiorLessThan);
    return out.toOwnedSlice(gpa);
}

// ---------------------------------------------------------------------------
// Observability (charter §10; spec §3.1, §4).
// ---------------------------------------------------------------------------

/// Entry bytes written into ring pages (`res.len` per committed entry,
/// headers included) — one term of the metadata-WRITE ledger.
pub var meta_kv_journal_bytes: counters.ShardedCounter = .{};
pub var meta_kv_journal_entries: counters.ShardedCounter = .{};
/// The sector pad's cost, kept apart from the entry economy so
/// `journal_entries` per op keeps its meaning.
pub var meta_kv_journal_pad_entries: counters.ShardedCounter = .{};
pub var meta_kv_journal_pad_bytes: counters.ShardedCounter = .{};
/// The rest of the metadata-WRITE ledger's journal terms (decision 0003 §L;
/// exact-sum `meta_kv_journal_device_bytes ≡ meta_kv_journal_bytes +
/// meta_kv_journal_pad_bytes + meta_kv_journal_header_bytes +
/// meta_kv_journal_hole_bytes`): the windows landed (one aligned run set
/// each — the pad law's unit of device cost), the 24 B page headers the
/// runs carried, a rolled-out member's zero-filled range, and the bytes
/// the device saw. `pad_bytes ÷ device_bytes` is the pad fraction.
pub var meta_kv_journal_windows: counters.ShardedCounter = .{};
pub var meta_kv_journal_header_bytes: counters.ShardedCounter = .{};
pub var meta_kv_journal_hole_bytes: counters.ShardedCounter = .{};
pub var meta_kv_journal_device_bytes: counters.ShardedCounter = .{};
/// Windows that carried more pad than entries (the hold's own predicate,
/// judged on what landed): the pacing law bounds THESE to one per device
/// round trip (`conveyor.zig`).
pub var meta_kv_journal_windows_pad_majority: counters.ShardedCounter = .{};
/// The last mount's confirmed drops. After a crash nonzero is working as
/// designed; after a CLEAN unmount it is the corruption alert.
pub var replay_dropped_torn: counters.Gauge = .{};
pub var replay_dropped_after_clean_unmount: counters.Tripwire = .init("journal_replay_dropped_after_clean_unmount");
/// Checkpoints past a journal hole that could not cover it (the lane's
/// post-failure and post-isolation covers, the conveyor's abandon arm).
pub var meta_kv_journal_hole_checkpoint_failures: counters.ShardedCounter = .{};
/// Failure lines a FAILED volume kept out of the log (`FailureLatch.admitsLine`);
/// each failure is still counted by its class's own counter.
pub var meta_failed_volume_lines_quieted: counters.ShardedCounter = .{};

/// Per-volume mirrors of the global accounting: the process-global counter
/// cannot attribute journal traffic to a volume — the blindness that let a
/// one-volume meta-plane ceiling hide from the stats inode.
pub const RingStats = struct {
    written_entries: counters.Gauge = .{},
    written_bytes: counters.Gauge = .{},
    written_pad_bytes: counters.Gauge = .{},
    written_windows: counters.Gauge = .{},
    written_device_bytes: counters.Gauge = .{},

    /// ONE landed submission (a conveyor window, a structural entry, the
    /// recovery pad): its `entries` and its device accounting, called once
    /// the write's outcome is known. The ONE site that moves the journal
    /// terms of the metadata-WRITE ledger, so they close by construction.
    pub fn noteWritten(self: *RingStats, acct: WriteAccounting, entries: u64) void {
        std.debug.assert(acct.closes());
        meta_kv_journal_bytes.add(acct.entry_bytes);
        meta_kv_journal_entries.add(entries);
        if (acct.pad_bytes != 0) {
            meta_kv_journal_pad_entries.inc();
            meta_kv_journal_pad_bytes.add(acct.pad_bytes);
        }
        meta_kv_journal_windows.inc();
        meta_kv_journal_header_bytes.add(acct.header_bytes);
        meta_kv_journal_hole_bytes.add(acct.hole_bytes);
        meta_kv_journal_device_bytes.add(acct.run_bytes);
        if (acct.padMajority()) meta_kv_journal_windows_pad_majority.inc();
        self.written_bytes.add(acct.entry_bytes);
        self.written_entries.add(entries);
        self.written_pad_bytes.add(acct.pad_bytes);
        self.written_windows.add(1);
        self.written_device_bytes.add(acct.run_bytes);
    }
};

/// Publish a mount's replay verdict: the gauge, and the alert when a CLEAN
/// unmount's window dropped anything (a clean unmount leaves no torn link —
/// nothing inside the window may fail a mount loud, so the alert is a counted
/// tripwire, never a refusal).
pub fn noteReplay(recovery: *const JournalRecovery, clean_unmount: bool) void {
    replay_dropped_torn.set(recovery.dropped_torn);
    if (clean_unmount and recovery.dropped_torn > 0) {
        replay_dropped_after_clean_unmount.fire("replay_dropped_torn > 0 after a CLEAN unmount: the ring's bytes changed without a writer");
    }
}

/// The FAILED latch: `JOURNAL_FAILURE_LATCH` CONSECUTIVE failures of one
/// class latch the volume (mutations EIO until remount). Barrier failures
/// have a SEPARATE counter with the same latch because the entry-write
/// success path resets `journal_failures` right before the strict-mode
/// barrier — a barrier failing three times in a row would never latch on a
/// shared counter.
///
/// The third rung is the FENCE (charter §5.7, §1.8; `guard_claims.md`
/// §1.6): a reservation-conflict errno (`error.ReservationConflict`, EBADE
/// = the block layer's `BLK_STS_RESV_CONFLICT`) at ANY entry-write CQE or
/// barrier means another writer holds the device's persistent reservation
/// and this holder's writes are refused at the device — FAILED at once, no
/// count. Under O_DIRECT the conflict lands on the entry write's CQE as
/// readily as on the barrier, so both sites classify (`noteFence`).
pub const FailureLatch = struct {
    journal_failures: std.atomic.Value(u32) = .init(0),
    barrier_failures: std.atomic.Value(u32) = .init(0),
    failed: std.atomic.Value(bool) = .init(false),
    /// The typed cause: FAILED by the fence (vs the generic rungs).
    fenced: std.atomic.Value(bool) = .init(false),
    /// The classes that logged a line on this latch (this mount).
    marks: LineMarks = .{},
    /// A JOINED incarnation RETIRED because its home shard evicted it and
    /// the mount re-joins in place (decision 0003 §BS): it admits no new
    /// mutation — every one it still meets answers `Retired` (the
    /// conveyor's `Deferred`: re-issued on the successor incarnation, never
    /// EIO) — and, once its last window landed, `failStop` ends its writes.
    retired: std.atomic.Value(bool) = .init(false),

    pub const Class = enum { journal_write, barrier };

    /// The per-operation failure lines of a volume (the lane's windows and
    /// hole covers, the device binding's sites, the checkpoint task's
    /// passes; the data plane's sync legs, in-place DMAs, block-map commits
    /// and lost staged reads, keyed to the plane's set). Once the volume
    /// is FAILED every operation behind it fails the same way, so each
    /// class logs once per FAILED episode and counts the rest on its own
    /// counter (`admitsLine`).
    pub const LogClass = enum(u5) {
        window_write,
        hole_checkpoint,
        hole_cover_smo_held,
        node_read,
        bset_append,
        node_image,
        ledger_slot,
        bitmap_page,
        fdatasync,
        journal_entry,
        range_read,
        format_step,
        checkpoint_tick,
        maintenance_pass,
        fsync_leg,
        syncfs_leg,
        inplace_dma,
        edge_seed_read,
        staged_payload_lost,
        block_map_commit,
    };

    /// The one-line law (`LineMarks.admits`) over this volume's latch.
    pub fn admitsLine(self: *FailureLatch, class: LogClass) bool {
        return self.marks.admits(class, self);
    }

    /// The law as a seam (a data-path fixture binds a bare latch).
    pub fn lineSeam(self: *FailureLatch) LineSeam {
        return .{ .ctx = self, .admits = admitsLineFn };
    }

    fn admitsLineFn(ctx: *anyopaque, class: LogClass) bool {
        const self: *FailureLatch = @ptrCast(@alignCast(ctx));
        return self.admitsLine(class);
    }

    /// The one-line law's marks: one bit per `LogClass` (a u5 tag: every
    /// class fits) that logged a line in its owner's lifetime (a mount).
    pub const LineMarks = struct {
        logged: std.atomic.Value(u32) = .init(0),

        /// True iff a failure of `class` logs its line: always while
        /// `owner` (anything with `isFailed()`) is healthy; once it is
        /// FAILED only when the class never logged here — every later
        /// failure until remount is counted, not logged
        /// (`meta_failed_volume_lines_quieted`). A line logged while
        /// healthy marks its class too, so FAILED adds no second line.
        pub fn admits(self: *LineMarks, class: LogClass, owner: anytype) bool {
            const bit = @as(u32, 1) << @backingInt(class);
            // The AcqRel mark is taken BEFORE `isFailed`'s Acquire read
            // (paired with the latch's AcqRel swap): a class that logged
            // while healthy finds its bit set once FAILED.
            const prior = self.logged.fetchOr(bit, .acq_rel);
            if (!owner.isFailed() or prior & bit == 0) return true;
            meta_failed_volume_lines_quieted.inc();
            return false;
        }
    };

    /// The law behind a seam: the data path asks its set (`MetaSet`, any
    /// member FAILED) without naming the meta types.
    pub const LineSeam = struct {
        ctx: *anyopaque,
        admits: *const fn (ctx: *anyopaque, class: LogClass) bool,
    };

    /// Why a volume is FAILED (the stats face and the teardown log).
    pub const Verdict = enum { healthy, failed, fenced, retired };

    /// The fence rung: FAILED at once; true iff THIS call latched (the one
    /// guard log line and `writer_guard_fenced` are paid here). `site`
    /// names the CQE class for the log.
    pub fn noteFence(self: *FailureLatch, site: []const u8) bool {
        // Release pairs with `verdict`'s acquire: a reader that sees FAILED
        // sees the cause.
        self.fenced.store(true, .release);
        if (self.failed.swap(true, .acq_rel)) return false;
        guard.stats.fenced.inc();
        guard.stats.volume_fenced.add(1);
        log.err("journal: reservation-conflict errno at {s} — this holder's writes are fenced at the device (usurped or paused-then-preempted); latching the volume FAILED at once; mutations answer EIO until remount (single-writer guard)", .{site});
        return true;
    }

    pub const FenceVerdict = enum { not_fence, latched, already };

    /// The ONE classifier of a write/barrier outcome for the fence rung:
    /// the conflict latches here (`latched` iff by THIS call); the caller
    /// then runs its generic rung as before.
    pub fn classifyFence(self: *FailureLatch, e: anyerror, site: []const u8) FenceVerdict {
        if (e != error.ReservationConflict) return .not_fence;
        return if (self.noteFence(site)) .latched else .already;
    }

    pub fn isFenced(self: *const FailureLatch) bool {
        return self.fenced.load(.acquire);
    }

    pub fn verdict(self: *const FailureLatch) Verdict {
        if (self.isRetired()) return .retired;
        if (!self.isFailed()) return .healthy;
        return if (self.isFenced()) .fenced else .failed;
    }

    /// The incarnation's end (a re-join in place): no new mutation is
    /// admitted. Release pairs with `isRetired`'s Acquire; the writes stop
    /// at the later `failStop`, which the refusals read as retired too.
    pub fn retire(self: *FailureLatch) void {
        self.retired.store(true, .release);
    }

    pub fn isRetired(self: *const FailureLatch) bool {
        return self.retired.load(.acquire);
    }

    fn counter(self: *FailureLatch, class: Class) *std.atomic.Value(u32) {
        return switch (class) {
            .journal_write => &self.journal_failures,
            .barrier => &self.barrier_failures,
        };
    }

    /// Count a failure; true when THIS call latched the volume (logged once).
    pub fn noteFailure(self: *FailureLatch, class: Class) bool {
        // AcqRel: the count is a cross-thread tally (lane + checkpoint task).
        const n = self.counter(class).fetchAdd(1, .acq_rel) + 1;
        if (n < JOURNAL_FAILURE_LATCH) return false;
        // The swap pairs with `isFailed`'s Acquire: a mutation that sees the
        // latch sees every failure that raised it.
        if (self.failed.swap(true, .acq_rel)) return false;
        log.err("journal: {d} consecutive {s} failures — latching the volume FAILED; mutations answer EIO until remount", .{ n, @tagName(class) });
        return true;
    }

    /// A success resets its class's run.
    pub fn noteSuccess(self: *FailureLatch, class: Class) void {
        self.counter(class).store(0, .release);
    }

    /// The protocol-terminal latch for non-write, non-barrier terminals (the
    /// §4.7 wedged-tail bound): FAILED at once; true iff THIS call latched
    /// (the caller logs once on true).
    pub fn failStop(self: *FailureLatch) bool {
        return !self.failed.swap(true, .acq_rel);
    }

    pub fn isFailed(self: *const FailureLatch) bool {
        return self.failed.load(.acquire);
    }

    /// The mutation path's classifier: a latched volume refuses every
    /// mutation (EIO at the FUSE edge); a retired incarnation refuses it
    /// `Retired` (re-issued on its successor).
    pub fn admitMutation(self: *const FailureLatch) error{ VolumeFailed, Retired }!void {
        if (self.isRetired()) return error.Retired;
        if (self.isFailed()) return error.VolumeFailed;
    }
};

test "the retired latch (a re-join in place, decision 0003 §BS): retirement refuses every new mutation `Retired` while the volume still writes; the later fail-stop ends the writes and the verdict stays `retired`" {
    var latch: FailureLatch = .{};
    try latch.admitMutation();
    latch.retire();
    try testing.expectError(error.Retired, latch.admitMutation());
    try testing.expect(!latch.isFailed());
    try testing.expectEqual(FailureLatch.Verdict.retired, latch.verdict());
    try testing.expect(latch.failStop());
    try testing.expect(latch.isFailed() and latch.isRetired());
    try testing.expectError(error.Retired, latch.admitMutation());
    try testing.expectEqual(FailureLatch.Verdict.retired, latch.verdict());
}

// ---------------------------------------------------------------------------
// Torn-write / power-cut harness as pure image mutations (§7 "HARNESS
// MECHANISM"). The device-level twin lands with the io task.
// ---------------------------------------------------------------------------

/// `arm_torn_write(offset, keep)` over an image: ops land in order; the FIRST
/// op intersecting `torn_at` persists only its first `keep` bytes and the
/// write fails (EIO) — every op after it in the submission is lost (the path
/// is poisoned until the faults clear). True iff the tear fired.
pub fn applyOpsTorn(image: []u8, base: u64, ops: []const WriteOp, torn_at: u64, keep: usize) bool {
    for (ops) |op| {
        const at: usize = @intCast(op.offset - base);
        if (torn_at >= op.offset and torn_at < op.end()) {
            const n = @min(keep, op.bytes.len);
            @memcpy(image[at .. at + n], op.bytes[0..n]);
            return true;
        }
        @memcpy(image[at .. at + op.bytes.len], op.bytes);
    }
    return false;
}

/// `arm_power_cut` / `power_cut` over an image: every write's PRIOR bytes are
/// journaled until an `fdatasync` marks them durable; a power cut restores
/// every still-volatile write in reverse order and answers how many.
pub const VolatileImage = struct {
    gpa: Allocator,
    image: []u8,
    base: u64,
    volatile_writes: std.ArrayList(Prior) = .empty,
    /// The harness's device serializes its writers as a device would: the
    /// threaded contracts land windows from a device thread while the pass
    /// writes structural entries from its own (test-support code; the
    /// product's device writes are the lane's and the io pool's). The
    /// core's portable spin lock: an `Io` mutex needs an `Io` this image
    /// never had.
    lock: jc.SpinLock = .{},

    const Prior = struct { at: usize, bytes: []u8 };

    pub fn init(gpa: Allocator, image: []u8, base: u64) VolatileImage {
        return .{ .gpa = gpa, .image = image, .base = base };
    }

    pub fn deinit(self: *VolatileImage) void {
        for (self.volatile_writes.items) |p| self.gpa.free(p.bytes);
        self.volatile_writes.deinit(self.gpa);
    }

    pub fn write(self: *VolatileImage, offset: u64, bytes: []const u8) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        return self.writeLocked(offset, bytes);
    }

    fn writeLocked(self: *VolatileImage, offset: u64, bytes: []const u8) Allocator.Error!void {
        const at: usize = @intCast(offset - self.base);
        const prior = try self.gpa.dupe(u8, self.image[at .. at + bytes.len]);
        errdefer self.gpa.free(prior);
        try self.volatile_writes.append(self.gpa, .{ .at = at, .bytes = prior });
        @memcpy(self.image[at .. at + bytes.len], bytes);
    }

    /// One submission's ops, atomically against the other writers.
    pub fn writeOps(self: *VolatileImage, ops: []const WriteOp) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        for (ops) |op| try self.writeLocked(op.offset, op.bytes);
    }

    /// Everything written so far is durable.
    pub fn fdatasync(self: *VolatileImage) void {
        self.lock.lock();
        defer self.lock.unlock();
        for (self.volatile_writes.items) |p| self.gpa.free(p.bytes);
        self.volatile_writes.clearRetainingCapacity();
    }

    /// Restore every volatile write (newest first); answers the count.
    pub fn powerCut(self: *VolatileImage) usize {
        self.lock.lock();
        defer self.lock.unlock();
        const n = self.volatile_writes.items.len;
        while (self.volatile_writes.pop()) |p| {
            @memcpy(self.image[p.at .. p.at + p.bytes.len], p.bytes);
            self.gpa.free(p.bytes);
        }
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests (`docs/porting/kv_journal_ledger.md` §7, as image tests).
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A test ring: `pages` pages at `base`, unpadded unless `grain` says so.
const TestRing = struct {
    gpa: Allocator,
    layout: RingLayout,
    geo: CoreGeometry,
    core: jc.JournalCore,
    reg: jc.InflightRegistry,
    image: []u8,
    stats: RingStats = .{},

    fn init(gpa: Allocator, base: u64, pages: u64, reserve: u64, grain: u64) !TestRing {
        const image = try gpa.alloc(u8, @intCast(pages * PAGE_LEN));
        @memset(image, 0);
        const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA_LEN, .pages = pages, .reserve_bytes = reserve, .grain = grain };
        return .{
            .gpa = gpa,
            .layout = .{ .base = base, .pages = pages },
            .geo = geo,
            .core = jc.JournalCore.init(geo, 0, 0),
            .reg = try jc.InflightRegistry.init(gpa, 0, 16),
            .image = image,
        };
    }

    fn deinit(self: *TestRing) void {
        self.reg.deinit();
        self.gpa.free(self.image);
    }

    /// Admit → reserve → write one entry of `entry_len` whole bytes carrying
    /// ONE Put on inode key `ino` whose value pads the entry to size.
    fn append(self: *TestRing, ino: u64, entry_len: u64, marker: u8) !Reservation {
        var key: [9]u8 = undefined;
        var value: [MAX_ENTRY_LEN]u8 = undefined;
        const recs = sizedRecords(ino, entry_len, marker, 0, &key, &value);
        try testing.expectEqual(entry_len, try entryLenFor(&recs));
        const adm = self.core.tryAdmit(entry_len, .user) orelse return error.RingFull;
        const r = try self.reg.reserveRegistered(&self.core, adm);
        const stamped = sizedRecords(ino, entry_len, marker, r.seq_base, &key, &value);
        var img = try buildEntryWrite(self.gpa, self.layout, self.geo, 0, r.res, &stamped);
        defer img.deinit();
        applyOps(self.image, self.layout.base, img.ops);
        self.reg.complete(&self.core, r.res);
        self.stats.noteWritten(img.acct, 1);
        return r.res;
    }

    fn phys(self: *const TestRing, pos: u64) usize {
        return @intCast(self.layout.physicalOffsetOf(self.geo, pos) - self.layout.base);
    }

    /// Post-hoc damage: `0x5A` garbage over `len` image bytes at `off`.
    fn smash(self: *TestRing, off: usize, len: usize) void {
        @memset(self.image[off .. off + len], 0x5A);
    }

    fn replay(self: *TestRing, tail: u64) !JournalRecovery {
        return replayScanImage(self.gpa, self.image, self.geo, tail, 0);
    }
};

/// One Put on key `ino ‖ 01` sized so the whole entry is exactly `entry_len`.
fn sizedRecords(ino: u64, entry_len: u64, marker: u8, seq: u64, key: *[9]u8, value: *[MAX_ENTRY_LEN]u8) [1]RecordFrame {
    std.mem.writeInt(u64, key[0..8], ino, .big);
    key[8] = TREE_INODES;
    const value_len: usize = @intCast(entry_len - ENTRY_HDR_LEN - 1 - RECORD_HEADER_LEN - key.len);
    @memset(value[0..value_len], marker);
    return .{.{ .tag = tagFor(TREE_INODES, 0), .key = key, .kind = .put, .seq = seq, .value = value[0..value_len] }};
}

/// The recovered inode numbers in replay order (every test entry is one Put).
fn recoveredInos(rec: *const JournalRecovery, out: []u64) []u64 {
    var n: usize = 0;
    for (rec.entries) |e| {
        var it = e.records();
        while (it.next()) |r| : (n += 1) out[n] = std.mem.readInt(u64, r.key[0..8], .big);
    }
    return out[0..n];
}

test "constants: 24 B page header, 4072 B data, ASCII KVJP, sentinel, 24 B entry header, entry kinds, cap, extent placement" {
    try testing.expectEqual(@as(u64, 4096), PAGE_LEN);
    try testing.expectEqual(@as(u64, 24), PAGE_HDR_LEN);
    try testing.expectEqual(@as(u64, 4072), PAGE_DATA_LEN);
    try testing.expectEqual(@as(u32, 0x504A564B), PAGE_MAGIC);
    const hdr = pageHeaderImage(0, 24, 0);
    try testing.expectEqualStrings("KVJP", hdr[0..4]);
    try testing.expectEqual(@as(u16, 0xFFFF), FIRST_ENTRY_NONE);
    try testing.expectEqual(@as(u64, 24), ENTRY_HDR_LEN);
    try testing.expectEqual(@as(usize, 16), ENTRY_HDR_CHECKSUM_OFF);
    try testing.expectEqual(@as(u8, 0), @backingInt(EntryKind.records));
    try testing.expectEqual(@as(u8, 1), @backingInt(EntryKind.pad));
    try testing.expectEqual(@as(u64, 16), RECORD_HEADER_LEN);
    try testing.expectEqual(@as(u64, 131072), MAX_ENTRY_LEN);
    try testing.expectEqual(@as(u64, 131048), entryPayloadCap());
    try testing.expectEqual(@as(u32, 0), FIXED_RING_APPENDER);
    try testing.expectEqual(ExtentRef{ .start = 135168, .len = 8 * derive.MiB }, try journalExtent(100 * derive.MiB, null));
}

test "MAX_ENTRY_LEN tie: one maximal record (the 1 MiB node's value cap under a 17 B xattr key) plus a 4 KiB-grain pad fits with margin; two maximal values do not — the cap is a law, not 2× a record" {
    const record = @import("record.zig");
    const key = @import("key.zig");
    const max_value = record.recordValueCap(1024 * 1024);
    try testing.expectEqual(@as(usize, 65_792), max_value);
    const one = ENTRY_HDR_LEN + recordFrameLen(key.XATTR_KEY_LEN, max_value);
    try testing.expectEqual(@as(u64, 24 + 1 + 16 + 17 + 65_792), one);
    try testing.expect(one + ringMaxPadFor(4096) <= MAX_ENTRY_LEN);
    // The margin left for the refs and Δtimes that ride the transaction.
    try testing.expect(MAX_ENTRY_LEN - one > 60 * 1024);
    const two = ENTRY_HDR_LEN + 2 * recordFrameLen(key.XATTR_KEY_LEN, max_value);
    try testing.expect(two > MAX_ENTRY_LEN);
    // 128 KiB is one page-aligned run of 32 ring pages.
    try testing.expectEqual(@as(u64, 0), MAX_ENTRY_LEN % PAGE_LEN);
    try testing.expectEqual(@as(u64, 32), MAX_ENTRY_LEN / PAGE_LEN);
}

test "ring sizing: clamp(volume/64, 8 MiB, 32 MiB) rounded to pages; the override floor refuses" {
    try testing.expectEqual(RING_MIN, journalRingLen(100 * derive.MiB));
    try testing.expectEqual(RING_MIN, journalRingLen(512 * derive.MiB));
    try testing.expectEqual(@as(u64, 16 * derive.MiB), journalRingLen(derive.GiB));
    try testing.expectEqual(RING_MAX, journalRingLen(2 * derive.GiB));
    try testing.expectEqual(RING_MAX, journalRingLen(derive.TiB));
    // 1 GiB + 1000 B: /64 = 16 MiB + 15.6 — rounds down to pages.
    try testing.expectEqual(@as(u64, 16 * derive.MiB), journalRingLen(derive.GiB + 1000));
    try testing.expectEqual(@as(u64, 16 * derive.MiB + 4096), journalRingLen(derive.GiB + 64 * 4096));
    try testing.expectEqual(@as(u64, 393216), try journalLenOverride(393216));
    try testing.expectError(error.RingOverrideInvalid, journalLenOverride(393216 - 4096));
    try testing.expectError(error.RingOverrideInvalid, journalLenOverride(393216 + 1));
    try testing.expectEqual(@as(u64, 393216), (try journalExtent(derive.GiB, 393216)).len);
}

test "a segmented ring maps logical pages through its segment table in order; positions continue across the segment boundary; the one-segment form is the fixed ring's; ≤ 8 non-empty segments" {
    const one = RingLayout.fromExtent(.{ .start = 135_168, .len = 1 << 20 });
    try testing.expectEqual(@as(u64, 256), one.pages);
    try testing.expectEqual(@as(u64, 135_168 + 7 * 4096), one.pageOffset(7));
    const seg = try RingLayout.fromSegments(&.{ .{ .start = 0x400000, .len = 65_536 }, .{ .start = 0xC00000, .len = 131_072 }, .{ .start = 0x800000, .len = 65_536 } });
    try testing.expectEqual(@as(u64, 16 + 32 + 16), seg.pages);
    try testing.expectEqual(@as(u64, 0x400000), seg.base);
    try testing.expectEqual(@as(u64, 0x400000 + 15 * 4096), seg.pageOffset(15));
    try testing.expectEqual(@as(u64, 0xC00000), seg.pageOffset(16));
    try testing.expectEqual(@as(u64, 0xC00000 + 31 * 4096), seg.pageOffset(47));
    try testing.expectEqual(@as(u64, 0x800000), seg.pageOffset(48));
    try testing.expectEqual(@as(u64, 0x800000 + 15 * 4096), seg.pageOffset(63));
    // A position inside the second segment's first page.
    const geo = productionGeometry(seg.pages, 1);
    try testing.expectEqual(@as(u64, 0xC00000 + PAGE_HDR_LEN + 10), seg.physicalOffsetOf(geo, 16 * PAGE_DATA_LEN + 10));
    try testing.expectError(error.TooManySegments, RingLayout.fromSegments(&.{}));
    var nine: [9]ExtentRef = undefined;
    for (&nine, 0..) |*e, i| e.* = .{ .start = (i + 1) * 65_536, .len = 65_536 };
    try testing.expectError(error.TooManySegments, RingLayout.fromSegments(&nine));
    try testing.expectError(error.EmptySegment, RingLayout.fromSegments(&.{.{ .start = 0x400000, .len = 100 }}));
}

test "checkpoint reserve formula: 8 MiB→256 KiB, 16 MiB→256 KiB, 32 MiB→512 KiB, 64 MiB→1 MiB" {
    try testing.expectEqual(@as(u64, 256 * derive.KiB), checkpointReserveBytes(8 * derive.MiB));
    try testing.expectEqual(@as(u64, 256 * derive.KiB), checkpointReserveBytes(16 * derive.MiB));
    try testing.expectEqual(@as(u64, 512 * derive.KiB), checkpointReserveBytes(32 * derive.MiB));
    try testing.expectEqual(@as(u64, derive.MiB), checkpointReserveBytes(64 * derive.MiB));
    const geo = productionGeometry(2048, 4096);
    try testing.expectEqual(@as(u64, 256 * derive.KiB), geo.reserve_bytes);
    try testing.expectEqual(@as(u64, 4096), geo.grain);
}

test "ring grain: a power of two ≤ 4096 verbatim, anything else is the unpadded ring; max pad follows" {
    try testing.expectEqual(@as(u64, 512), ringGrainFor(512));
    try testing.expectEqual(@as(u64, 4096), ringGrainFor(4096));
    try testing.expectEqual(@as(u64, 1), ringGrainFor(1));
    try testing.expectEqual(@as(u64, 1), ringGrainFor(8192));
    try testing.expectEqual(@as(u64, 1), ringGrainFor(3072));
    try testing.expectEqual(@as(u64, 1), ringGrainFor(0));
    try testing.expectEqual(@as(u64, 4120), ringMaxPadFor(4096));
    try testing.expectEqual(@as(u64, 536), ringMaxPadFor(512));
    try testing.expectEqual(@as(u64, 0), ringMaxPadFor(1));
}

test "page header: byte layout {magic, lap, first_entry_off, reserved = 0, appender_id u32, hash64 over [0..16)}; parse total; a non-zero reserved field is refused" {
    const hdr = pageHeaderImage(7, 24, 0);
    try testing.expectEqual(@as(u32, 7), std.mem.readInt(u32, hdr[4..8], .little));
    try testing.expectEqual(@as(u16, 24), std.mem.readInt(u16, hdr[8..10], .little));
    try testing.expectEqualSlices(u8, &.{ 0, 0 }, hdr[10..12]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0, 0 }, hdr[12..16]);
    try testing.expectEqual(checksum.hash64(hdr[0..16]), std.mem.readInt(u64, hdr[16..24], .little));
    const parsed = parsePageHeader(&hdr).?;
    try testing.expectEqual(PageHeader{ .lap = 7, .first_entry_off = 24, .appender_id = 0 }, parsed);
    const w = pageHeaderImage(1, FIRST_ENTRY_NONE, 0x0001_0003);
    try testing.expectEqualSlices(u8, &.{ 3, 0, 1, 0 }, w[12..16]);
    try testing.expectEqual(@as(u32, 0x0001_0003), parsePageHeader(&w).?.appender_id);
    var bad = hdr;
    bad[0] ^= 1;
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(&bad));
    bad = hdr;
    bad[9] ^= 1;
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(&bad));
    // Reserved is CHECKED: restamped so only the reserved rule rejects.
    bad = hdr;
    bad[10] = 1;
    std.mem.writeInt(u64, bad[16..24], checksum.hash64(bad[0..16]), .little);
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(&bad));
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(hdr[0..20]));
    const zeros: [24]u8 = @splat(0);
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(&zeros));
}

test "tags: tree | level << 4; admissibility; untag" {
    try testing.expectEqual(@as(u8, 0x21), tagFor(1, 2));
    try testing.expectEqual(@as(u8, 0x10), tagFor(KIND_INTERIOR, 1));
    try testing.expectEqual(Tag{ .tree_id = 1, .level = 2 }, untag(0x21));
    try testing.expect(tagTreeIdAdmissible(9, 0));
    try testing.expect(tagTreeIdAdmissible(1, 15));
    try testing.expect(!tagTreeIdAdmissible(0, 0));
    try testing.expect(!tagTreeIdAdmissible(10, 0));
    try testing.expect(!tagTreeIdAdmissible(15, 15));
}

test "entry payload round trip; truncations, tag 0 / tag 10, empty key, bad kind, tombstone with value are errors; cap enforced writer-side" {
    const recs = [_]RecordFrame{
        .{ .tag = tagFor(1, 0), .key = "k1", .kind = .put, .seq = 7, .value = "hello" },
        .{ .tag = tagFor(2, 0), .key = "key-two", .kind = .delete, .seq = 8, .value = "" },
        .{ .tag = tagFor(0, 1), .key = "sep", .kind = .put, .seq = 9, .value = "ptr" },
    };
    const n = payloadLenFor(&recs);
    try testing.expectEqual(@as(u64, (17 + 2 + 5) + (17 + 7) + (17 + 3 + 3)), n);
    var buf: [128]u8 = undefined;
    encodeEntryPayload(buf[0..@intCast(n)], &recs);
    try testing.expectEqual(@as(u32, 3), try validateEntryPayload(buf[0..@intCast(n)]));
    var it = FrameIter{ .buf = buf[0..@intCast(n)] };
    const r0 = it.next().?;
    try testing.expectEqualStrings("k1", r0.key);
    try testing.expectEqualStrings("hello", r0.value);
    try testing.expectEqual(@as(u64, 7), r0.seq);
    try testing.expectEqual(RecordKind.put, r0.kind);
    const r1 = it.next().?;
    try testing.expectEqual(RecordKind.delete, r1.kind);
    const r2 = it.next().?;
    try testing.expectEqual(Tag{ .tree_id = 0, .level = 1 }, untag(r2.tag));
    try testing.expectEqual(@as(?RecordFrame, null), it.next());
    // Truncations.
    try testing.expectError(error.Corrupt, validateEntryPayload(buf[0 .. @as(usize, @intCast(n)) - 1]));
    try testing.expectError(error.Corrupt, validateEntryPayload(buf[0..10]));
    // Tag 0 at level 0, tag 10.
    var bad = buf;
    bad[0] = 0;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    bad[0] = 10;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // Empty key.
    bad = buf;
    bad[1] = 0;
    bad[2] = 0;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // Kind 0 / 4.
    bad = buf;
    bad[3] = 0;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    bad[3] = 4;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // A non-zero flags byte.
    bad = buf;
    bad[4] = 1;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // A tombstone with a value.
    bad = buf;
    bad[24 + 5] = 1; // second record's val_len low byte
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // Lengths beyond the buffer are refused without being dereferenced.
    bad = buf;
    bad[5] = 0xFF;
    bad[6] = 0xFF;
    bad[7] = 0xFF;
    bad[8] = 0xFF;
    try testing.expectError(error.Corrupt, validateEntryPayload(bad[0..@intCast(n)]));
    // The cap: exactly 128 KiB legal, one more refused.
    var big: [MAX_ENTRY_LEN]u8 = undefined;
    const exact = [_]RecordFrame{.{ .tag = 1, .key = "k", .kind = .put, .seq = 1, .value = big[0..@intCast(MAX_ENTRY_LEN - ENTRY_HDR_LEN - 17 - 1)] }};
    try testing.expectEqual(MAX_ENTRY_LEN, try entryLenFor(&exact));
    const over = [_]RecordFrame{.{ .tag = 1, .key = "k", .kind = .put, .seq = 1, .value = big[0..@intCast(MAX_ENTRY_LEN - ENTRY_HDR_LEN - 17)] }};
    try testing.expectError(error.EntryTooLarge, entryLenFor(&over));
}

test "entry checksum: streaming over pos ‖ len ‖ kind ‖ reserved ‖ payload parts equals the one-shot hash of the concatenation; the kind is under the checksum" {
    const payload = "some payload bytes that cross the 16 B boundary";
    var concat: [16 + payload.len]u8 = undefined;
    @memset(&concat, 0);
    std.mem.writeInt(u64, concat[0..8], 12345, .little);
    std.mem.writeInt(u32, concat[8..12], payload.len, .little);
    @memcpy(concat[16..], payload);
    try testing.expectEqual(checksum.hash64(&concat), entryChecksum(12345, .records, payload));
    try testing.expectEqual(checksum.hash64(&concat), entryChecksumParts(12345, payload.len, .records, &.{ payload[0..10], payload[10..] }));
    concat[12] = 1;
    try testing.expectEqual(checksum.hash64(&concat), entryChecksum(12345, .pad, payload));
    try testing.expect(entryChecksum(12345, .pad, payload) != entryChecksum(12345, .records, payload));
}

test "single-page entry: page-0 header magic / lap 0 / feo 24 / checksum; replay equals; head_pos == end" {
    var ring = try TestRing.init(testing.allocator, 4096, 4, 0, 1);
    defer ring.deinit();
    const r = try ring.append(1, 300, 0xA1);
    try testing.expectEqual(@as(u64, 0), r.start);
    const hdr = parsePageHeader(ring.image[0..24]).?;
    try testing.expectEqual(PageHeader{ .lap = 0, .first_entry_off = 24, .appender_id = 0 }, hdr);
    // The entry header at physical 24: pos 0, len 276, kind records, reserved 0.
    try testing.expectEqual(@as(u64, 0), std.mem.readInt(u64, ring.image[24..32], .little));
    try testing.expectEqual(@as(u32, 276), std.mem.readInt(u32, ring.image[32..36], .little));
    try testing.expectEqual(@backingInt(EntryKind.records), ring.image[36]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0 }, ring.image[37..40]);
    try testing.expectEqual(ring.image[24 + 24], tagFor(TREE_INODES, 0));
    // Page 1 untouched.
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(ring.image[4096..][0..24]));
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{1}, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 300), rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    try testing.expectEqual(@as(u64, 0), rec.foreign_pages);
    try testing.expectEqual(@as(u64, 1), ring.stats.written_entries.load());
    try testing.expectEqual(@as(u64, 300), ring.stats.written_bytes.load());
}

test "packed entries: 17 × 230 B fit page 0; page 1 untouched; seq-sorted replay" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    for (1..18) |i| _ = try ring.append(i, 230, @intCast(i));
    try testing.expectEqual(@as(u64, 17 * 230), ring.core.headPos());
    try testing.expect(17 * 230 <= PAGE_DATA_LEN);
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(ring.image[4096..][0..24]));
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [32]u64 = undefined;
    const got = recoveredInos(&rec, &inos);
    try testing.expectEqual(@as(usize, 17), got.len);
    for (got, 1..) |g, i| try testing.expectEqual(@as(u64, i), g);
}

test "multi-page entry: 64 KiB over 17 pages; continuation feo 0xFFFF; end page feo = successor start; successor lands there" {
    var ring = try TestRing.init(testing.allocator, 0, 32, 0, 1);
    defer ring.deinit();
    const big = try ring.append(1, 64 * 1024, 0xB1);
    const next = try ring.append(2, 500, 0xB2);
    try testing.expectEqual(big.end(), next.start);
    // 65536 / 4072 = 16.09 ⇒ pages 0..16: page 0 feo 24, pages 1..15 0xFFFF, page 16 feo = 24 + (65536 − 16·4072).
    try testing.expectEqual(@as(u16, 24), parsePageHeader(ring.image[0..24]).?.first_entry_off);
    for (1..16) |p| try testing.expectEqual(FIRST_ENTRY_NONE, parsePageHeader(ring.image[p * 4096 ..][0..24]).?.first_entry_off);
    const end_in_page: u16 = @intCast(24 + (65536 - 16 * 4072));
    try testing.expectEqual(end_in_page, parsePageHeader(ring.image[16 * 4096 ..][0..24]).?.first_entry_off);
    try testing.expectEqual(@as(u64, 65536), std.mem.readInt(u64, ring.image[16 * 4096 + @as(usize, end_in_page) ..][0..8], .little));
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(next.end(), rec.head_pos);
}

test "entry header straddles a page-data boundary: first entry 4060 B, second header split 12/12; exact page fill hands the next header to the successor" {
    {
        var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
        defer ring.deinit();
        _ = try ring.append(1, 4060, 0xC1);
        const b = try ring.append(2, 300, 0xC2);
        try testing.expectEqual(@as(u64, 4060), b.start);
        // B's header: 12 bytes at the end of page 0's data, 12 at the start of page 1's.
        try testing.expectEqual(@as(u64, 4060), std.mem.readInt(u64, ring.image[24 + 4060 ..][0..8], .little));
        try testing.expectEqual(@as(u16, 24 + 4060 - 4072 + 300), parsePageHeader(ring.image[4096..][0..24]).?.first_entry_off);
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
    }
    {
        var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
        defer ring.deinit();
        _ = try ring.append(1, PAGE_DATA_LEN, 0xC3);
        _ = try ring.append(2, 300, 0xC4);
        try testing.expectEqual(@as(u16, 24), parsePageHeader(ring.image[0..24]).?.first_entry_off);
        try testing.expectEqual(@as(u16, 24), parsePageHeader(ring.image[4096..][0..24]).?.first_entry_off);
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
    }
}

test "wraparound replay window: 4 × 4000 B; admission refused at 16000; watermark to rs[2].start; the 5th wraps into lap 1; replay from tail = [3,4,5], dropped 0" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1); // L = 16288
    defer ring.deinit();
    var rs: [5]Reservation = undefined;
    for (0..4) |i| rs[i] = try ring.append(@intCast(i + 1), 4000, @intCast(0x10 + i));
    try testing.expectEqual(@as(u64, 16000), ring.core.headPos());
    try testing.expectEqual(@as(?jc.Admission, null), ring.core.tryAdmit(4000, .user)); // 16000 + 4000 > 16288
    try testing.expectEqual(@as(?jc.Admission, null), ring.core.tryAdmit(289, .user));
    ring.core.advanceReusableUpto(rs[2].start);
    rs[4] = try ring.append(5, 4000, 0x15);
    try testing.expect(rs[4].end() > ring.geo.logicalLen());
    try testing.expectEqual(@as(u64, 1), ring.geo.lap(rs[4].end() - 1));
    var rec = try ring.replay(rs[2].start);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 3, 4, 5 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    try testing.expectEqual(rs[4].end(), rec.head_pos);
    // Replay skips pre-tail entries: from rs[3].start only 4 and 5.
    var rec2 = try ring.replay(rs[3].start);
    defer rec2.deinit();
    try testing.expectEqualSlices(u64, &.{ 4, 5 }, recoveredInos(&rec2, &inos));
}

test "empty ring replays zero entries; recover resumes head and watermark and appends continue" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    {
        var rec = try ring.replay(0);
        defer rec.deinit();
        try testing.expectEqual(@as(usize, 0), rec.entries.len);
        try testing.expectEqual(@as(u64, 0), rec.head_pos);
        try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    }
    _ = try ring.append(1, 1000, 0xD1);
    _ = try ring.append(2, 1000, 0xD2);
    var rec = try ring.replay(0);
    defer rec.deinit();
    // The remounted core: head = recovered head, reusable_upto = tail.
    var core2 = jc.JournalCore.init(ring.geo, rec.head_pos, 0);
    try testing.expectEqual(@as(u64, 2000), core2.headPos());
    try testing.expectEqual(@as(u64, 0), core2.reusableUpto());
    const r = core2.reserve(core2.tryAdmit(500, .user).?);
    try testing.expectEqual(@as(u64, 2000), r.start);
}

test "replay: a torn last entry recovers the prefix (never loud), head before the tear, dropped 0" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    _ = try ring.append(1, 1000, 0xE1);
    const b = try ring.append(2, 1000, 0xE2);
    // The third write tears: only 10 bytes of its first op land.
    var key: [9]u8 = undefined;
    var value: [MAX_ENTRY_LEN]u8 = undefined;
    const adm = ring.core.tryAdmit(1000, .user).?;
    const r = try ring.reg.reserveRegistered(&ring.core, adm);
    const stamped = sizedRecords(3, 1000, 0xE3, r.seq_base, &key, &value);
    var img = try buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, r.res, &stamped);
    defer img.deinit();
    try testing.expect(applyOpsTorn(ring.image, 0, img.ops, ring.layout.physicalOffsetOf(ring.geo, r.res.start), 10));
    ring.reg.complete(&ring.core, r.res); // a failed write still completes
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(b.end(), rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
}

test "replay: a torn entry mid-log drops it and resyncs at the next verified page header: [1,3,4], dropped 1" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1);
    defer ring.deinit();
    _ = try ring.append(1, 1000, 0xF1);
    const b = try ring.append(2, 4000, 0xF2); // spans into page 1
    _ = try ring.append(3, 1000, 0xF3); // starts in page 1
    _ = try ring.append(4, 1000, 0xF4);
    ring.smash(ring.phys(b.start + 100), 16);
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 3, 4 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
}

test "replay: a dead page header under a 9000 B entry is read straight through (dropped 0); plus damaged payload ⇒ [2,3], dropped 2" {
    {
        var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1);
        defer ring.deinit();
        _ = try ring.append(1, 9000, 0xC1);
        _ = try ring.append(2, 2000, 0xC2);
        _ = try ring.append(3, 2000, 0xC3);
        ring.smash(4096, 24); // page 1's header
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 1, 2, 3 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    }
    {
        var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1);
        defer ring.deinit();
        _ = try ring.append(1, 9000, 0xC4);
        _ = try ring.append(2, 2000, 0xC5);
        _ = try ring.append(3, 2000, 0xC6);
        ring.smash(4096, 24);
        ring.smash(ring.phys(200), 16); // A's payload
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 2, 3 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(@as(u64, 2), rec.dropped_torn); // A's torn entry + page 1's dead header
    }
}

test "replay: a garbage len (0xFFFFFFFF, and 100000 < cap but past the window) is never dereferenced: [1,3], dropped 1" {
    for ([_]u32{ 0xFFFF_FFFF, 100_000 }) |bad_len| {
        var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
        defer ring.deinit();
        _ = try ring.append(1, 1000, 0xD1);
        const b = try ring.append(2, PAGE_DATA_LEN - 1000, 0xD2);
        _ = try ring.append(3, 1000, 0xD3);
        const at = ring.phys(b.start + 8);
        std.mem.writeInt(u32, ring.image[at..][0..4], bad_len, .little);
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 1, 3 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
    }
}

test "replay: a whole zeroed page is a hole; resync recovers the later entries: [1,3], dropped 1" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    _ = try ring.append(1, 1000, 0xA1);
    _ = try ring.append(2, 4000, 0xA2); // [1000, 5000): crosses into page 1
    _ = try ring.append(3, 1000, 0xA3); // [5000, 6000) in page 1 — lost with the page
    _ = try ring.append(4, 4000, 0xA4); // [6000, 10000): starts in page 1, lost too
    _ = try ring.append(5, 1000, 0xA5); // [10000, 11000) in page 2
    @memset(ring.image[4096 .. 2 * 4096], 0);
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 5 }, recoveredInos(&rec, &inos));
    // The chain broke at 2 (one pending), page 1's dead header is a hole
    // (one more); page 2's header names entry 5's start — both confirmed.
    try testing.expectEqual(@as(u64, 2), rec.dropped_torn);
}

test "replay: a torn middle page of a multi-page entry drops it whole, no fragment survives: [2,3], dropped 1" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1);
    defer ring.deinit();
    const e = try ring.append(1, 9000, 0xB1);
    _ = try ring.append(2, 1000, 0xB2);
    _ = try ring.append(3, 1000, 0xB3);
    ring.smash(ring.phys(e.start + 5000), 16);
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 2, 3 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
}

test "replay: a stale previous-lap entry at the window end is rejected by seq ≡ position; foreign pages are counted, never followed" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    for (1..5) |i| _ = try ring.append(@intCast(i), 4000, @intCast(i));
    // A foreign writer's header on page 3 (verifies, writer 7).
    ring.image[3 * 4096 ..][0..24].* = pageHeaderImage(0, 24, 7);
    var rec = try ring.replay(12000);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    // Entry 4 at 12000 is chain-read (the chain never consults the header).
    try testing.expectEqualSlices(u64, &.{4}, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 1), rec.foreign_pages);
    // 16000 is unwritten ⇒ the chain breaks; the lap-1 discovery slots land
    // on pages 0..2 whose headers carry lap 0 ⇒ every one is a hole, none is
    // followed: entry 1's lap-0 bytes are never replayed as lap 1's.
    try testing.expectEqual(@as(u64, 16000), rec.head_pos);
}

test "PAD entry bytes: pos = end, len = pad − 24, kind PAD, zero payload; replay skips it as a chain link; aligned runs are page-based images" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 4096);
    defer ring.deinit();
    const a = try ring.append(1, 300, 0x51);
    try testing.expectEqual(PAGE_DATA_LEN - 300, a.pad);
    try testing.expectEqual(PAGE_DATA_LEN, a.paddedEnd());
    // The PAD entry at logical 300: physical 24 + 300.
    const at = ring.phys(300);
    try testing.expectEqual(@as(u64, 300), std.mem.readInt(u64, ring.image[at..][0..8], .little));
    try testing.expectEqual(@as(u32, @intCast(a.pad - 24)), std.mem.readInt(u32, ring.image[at + 8 ..][0..4], .little));
    try testing.expectEqual(@backingInt(EntryKind.pad), ring.image[at + 12]);
    try testing.expectEqualSlices(u8, &.{ 0, 0, 0 }, ring.image[at + 13 .. at + 16]);
    for (ring.image[at + 24 .. 4096]) |b| try testing.expectEqual(@as(u8, 0), b);
    const b = try ring.append(2, 100, 0x52);
    try testing.expectEqual(PAGE_DATA_LEN, b.start);
    try testing.expectEqual(@as(u64, 2), ring.stats.written_entries.load());
    try testing.expectEqual(a.pad + b.pad, ring.stats.written_pad_bytes.load());
    // The per-volume ledger (§L): two windows, one page each — the device
    // saw 8192 B for 400 B of entries; both windows pad-majority.
    try testing.expectEqual(@as(u64, 2), ring.stats.written_windows.load());
    try testing.expectEqual(@as(u64, 2 * PAGE_LEN), ring.stats.written_device_bytes.load());
    try testing.expectEqual(@as(u64, 400), ring.stats.written_bytes.load());
    try testing.expectEqual(ring.stats.written_device_bytes.load(), ring.stats.written_bytes.load() + ring.stats.written_pad_bytes.load() + 2 * PAGE_HDR_LEN);
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(b.paddedEnd(), rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    // The aligned form of one entry on a 4096 grain: ONE 4096 B run at the page base.
    var key: [9]u8 = undefined;
    var value: [MAX_ENTRY_LEN]u8 = undefined;
    const res = ring.core.reserve(ring.core.tryAdmit(300, .user).?);
    const recs = sizedRecords(3, 300, 0x53, res.start, &key, &value);
    var img = try buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, res, &recs);
    defer img.deinit();
    try testing.expectEqual(@as(usize, 1), img.ops.len);
    try testing.expectEqual(@as(u64, 2 * 4096), img.ops[0].offset);
    try testing.expectEqual(@as(usize, 4096), img.ops[0].bytes.len);
    try testing.expect(parsePageHeader(img.ops[0].bytes[0..24]) != null);
}

test "a PAD entry with a nonzero payload byte, an unknown entry kind or a non-zero reserved byte is a Fail, not a link (all three restamped)" {
    for ([_]u8{ 0, 1, 2 }) |variant| {
        var ring = try TestRing.init(testing.allocator, 0, 8, 0, 4096);
        defer ring.deinit();
        const a = try ring.append(1, 300, 0x61);
        _ = try ring.append(2, 300, 0x62);
        // Doctor the pad entry AND restamp its checksum so only the
        // structural rule can reject it.
        const at = ring.phys(a.end());
        const pad_len: usize = @intCast(a.pad);
        const entry = ring.image[at .. at + pad_len];
        switch (variant) {
            0 => entry[ENTRY_HDR_LEN + 5] = 1,
            1 => entry[12] = 2,
            else => entry[14] = 1,
        }
        var h: checksum.Hasher = .init();
        h.update(entry[0..ENTRY_HDR_CHECKSUM_OFF]);
        h.update(entry[ENTRY_HDR_LEN..]);
        std.mem.writeInt(u64, entry[ENTRY_HDR_CHECKSUM_OFF..][0..8], h.finalLow64(), .little);
        var rec = try ring.replay(0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        // The chain breaks at the pad; page 1's header names entry 2 ⇒ resync; one confirmed drop.
        try testing.expectEqualSlices(u64, &.{ 1, 2 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
    }
}

test "aligned runs: a window spanning pages coalesces adjacent pages into one image, a rolled-back member is a zero hole, a misaligned run is refused" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 512);
    defer ring.deinit();
    // Three back-to-back members in one window; the middle one rolled back.
    var k1: [9]u8 = undefined;
    var k3: [9]u8 = undefined;
    var v1: [MAX_ENTRY_LEN]u8 = undefined;
    var v3: [MAX_ENTRY_LEN]u8 = undefined;
    const lens = [_]u64{ 1000, 4000, 1000 };
    const total = lens[0] + lens[1] + lens[2];
    const adm = ring.core.tryAdmit(total, .user).?;
    const window = ring.core.reserve(adm);
    try testing.expect(window.pad > 0);
    try testing.expect(ring.geo.sectorAligned(window.paddedEnd()));
    const m1 = Reservation.unpadded(window.start, lens[0]);
    const m3 = Reservation.unpadded(window.start + lens[0] + lens[1], lens[2]);
    const r1 = sizedRecords(1, lens[0], 0x71, m1.start, &k1, &v1);
    const r3 = sizedRecords(3, lens[2], 0x73, m3.start, &k3, &v3);
    var img = try buildBatchWrite(testing.allocator, ring.layout, ring.geo, 0, window, &.{ .{ .res = m1, .records = &r1 }, .{ .res = m3, .records = &r3 } });
    defer img.deinit();
    // [0, 6000 + pad) covers pages 0 and 1 contiguously ⇒ ONE run from page 0's base.
    try testing.expectEqual(@as(usize, 1), img.ops.len);
    try testing.expectEqual(@as(u64, 0), img.ops[0].offset);
    try testing.expectEqual(@as(u64, 0), img.ops[0].bytes.len % 512);
    // The metadata-WRITE ledger's exact sum over the run (decision 0003 §L):
    // the two survivors, the window's pad, the headers of the two pages the
    // span begins, the rolled-back member's zero hole — ≡ the run's bytes.
    try testing.expectEqual(lens[0] + lens[2], img.acct.entry_bytes);
    try testing.expectEqual(window.pad, img.acct.pad_bytes);
    try testing.expectEqual(2 * PAGE_HDR_LEN, img.acct.header_bytes);
    try testing.expectEqual(lens[1], img.acct.hole_bytes);
    try testing.expectEqual(img.ops[0].bytes.len, img.acct.run_bytes);
    try testing.expect(img.acct.closes());
    // The hole and the pad outweigh the entries: a pad-majority window by the
    // hold's predicate only if pad > entries — here pad < 2000, so it is not.
    try testing.expectEqual(window.pad > lens[0] + lens[2], img.acct.padMajority());
    applyOps(ring.image, 0, img.ops);
    // The hole: member 2's bytes are zero.
    for (ring.image[ring.phys(m1.end() + ENTRY_HDR_LEN) .. ring.phys(m1.end() + ENTRY_HDR_LEN) + 100]) |b| try testing.expectEqual(@as(u8, 0), b);
    // Page 1's header: owned by the hole's range (member 2 spans 1000..5000 ⇒ page 1 starts at 4072) — no survivor owns it ⇒ zeros.
    try testing.expectEqual(@as(?PageHeader, null), parsePageHeader(ring.image[4096..][0..24]));
    var rec = try ring.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    // Member 1 replays; the hole breaks the chain; member 3 starts mid-page 1
    // whose header the hole owned ⇒ unreachable (the chain rule); the pad at
    // 6000 is unreachable too ⇒ head = member 1's end, nothing confirmed.
    try testing.expectEqualSlices(u64, &.{1}, recoveredInos(&rec, &inos));
    try testing.expectEqual(m1.end(), rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    // A misaligned run (a window whose end is not sector-aligned) is refused.
    const bad = Reservation.unpadded(window.paddedEnd(), 300);
    const rb = sizedRecords(9, 300, 0x79, bad.start, &k1, &v1);
    try testing.expectError(error.Corrupt, buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, bad, &rb));
    // A reservation whose length does not match its records is refused.
    const wrong = Reservation.unpadded(window.paddedEnd(), 301);
    try testing.expectError(error.Corrupt, buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, wrong, &rb));
}

test "batch of one equals a solo write byte for byte; torn first member drops alone; torn last member is end-of-log" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1);
    defer ring.deinit();
    var k: [3][9]u8 = undefined;
    var v: [3][MAX_ENTRY_LEN]u8 = undefined;
    const lens = [_]u64{ 500, 700, 600 };
    const window = ring.core.reserve(ring.core.tryAdmit(1800, .user).?);
    var parts: [3]BatchPart = undefined;
    var recs: [3][1]RecordFrame = undefined;
    var cursor = window.start;
    for (0..3) |i| {
        const res = Reservation.unpadded(cursor, lens[i]);
        recs[i] = sizedRecords(101 + i, lens[i], @intCast(0x81 + i), res.start, &k[i], &v[i]);
        parts[i] = .{ .res = res, .records = &recs[i] };
        cursor += lens[i];
    }
    var batch = try buildBatchWrite(testing.allocator, ring.layout, ring.geo, 0, window, &parts);
    defer batch.deinit();
    var solo = try buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, parts[1].res, parts[1].records);
    defer solo.deinit();
    // The unpadded ring's accounting: no pad, no hole; the headers are the
    // ones the entries own (page 0's); the run is the ops verbatim.
    try testing.expectEqual(@as(u64, 1800), batch.acct.entry_bytes);
    try testing.expectEqual(@as(u64, 0), batch.acct.pad_bytes);
    try testing.expectEqual(@as(u64, 0), batch.acct.hole_bytes);
    try testing.expectEqual(PAGE_HDR_LEN, batch.acct.header_bytes);
    try testing.expectEqual(@as(u64, 1800 + PAGE_HDR_LEN), batch.acct.run_bytes);
    try testing.expect(batch.acct.closes() and !batch.acct.padMajority());
    // The member's ops appear verbatim inside the batch.
    for (solo.ops) |s| {
        var found = false;
        for (batch.ops) |b| if (b.offset == s.offset and std.mem.eql(u8, b.bytes, s.bytes)) {
            found = true;
        };
        try testing.expect(found);
    }
    // Torn first member (its first op cut to 10 bytes): the chain breaks at
    // the window's start and its same-page successors are unreachable until
    // the next verified page header (chain-only discovery inside a page) —
    // nothing replays, nothing is confirmed.
    {
        const image = try testing.allocator.alloc(u8, ring.image.len);
        defer testing.allocator.free(image);
        @memset(image, 0);
        try testing.expect(applyOpsTorn(image, 0, batch.ops, ring.layout.physicalOffsetOf(ring.geo, parts[0].res.start), 10));
        var rec = try replayScanImage(testing.allocator, image, ring.geo, 0, 0);
        defer rec.deinit();
        try testing.expectEqual(@as(usize, 0), rec.entries.len);
        try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    }
    // Torn last member: [101, 102], head = parts[1].end, dropped 0.
    {
        const image = try testing.allocator.alloc(u8, ring.image.len);
        defer testing.allocator.free(image);
        @memset(image, 0);
        // Ops are sorted only on padded rings; land all but cut the last member's payload op.
        for (batch.ops) |op| {
            const at: usize = @intCast(op.offset);
            if (op.offset == ring.layout.physicalOffsetOf(ring.geo, parts[2].res.start)) {
                @memcpy(image[at .. at + 10], op.bytes[0..10]);
            } else @memcpy(image[at .. at + op.bytes.len], op.bytes);
        }
        var rec = try replayScanImage(testing.allocator, image, ring.geo, 0, 0);
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 101, 102 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(parts[1].res.end(), rec.head_pos);
        try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    }
}

test "head alignment: a mid-sector recovered head gets a zero-length pad reservation whose RMW spans cover the sector; aligned head ⇒ null; full ring ⇒ reserve exhausted" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 4096);
    defer ring.deinit();
    const a = try ring.append(1, 300, 0x91);
    // Simulate the window torn behind its entry: the pad never landed, so a
    // remount's head stands at the entry's end (300).
    @memset(ring.image[ring.phys(a.end())..4096], 0);
    var rec = try ring.replay(0);
    defer rec.deinit();
    try testing.expectEqual(a.end(), rec.head_pos);
    var core = jc.JournalCore.init(ring.geo, rec.head_pos, 0);
    var reg = try jc.InflightRegistry.init(testing.allocator, rec.head_pos, 4);
    defer reg.deinit();
    var plan = (try planHeadAlignment(testing.allocator, &core, &reg, ring.layout, 0)).?;
    defer plan.deinit();
    try testing.expectEqual(Reservation{ .start = 300, .len = 0, .pad = PAGE_DATA_LEN - 300 }, plan.res);
    try testing.expectEqual(@as(usize, 1), plan.spans.len);
    try testing.expectEqual(Span{ .lo = 0, .hi = 4096 }, plan.spans[0]);
    try testing.expectEqual(@as(usize, 1), reg.openCount());
    // The RMW: read the span fresh (the image), overlay, write whole.
    var fresh: [4096]u8 = undefined;
    @memcpy(&fresh, ring.image[0..4096]);
    overlayOps(&fresh, 0, plan.ops);
    @memcpy(ring.image[0..4096], &fresh);
    var stats: RingStats = .{};
    finishHeadAlignment(&plan, &core, &reg, &stats, true);
    try testing.expectEqual(@as(usize, 0), reg.openCount());
    try testing.expectEqual(plan.res.pad, stats.written_pad_bytes.load());
    try testing.expectEqual(PAGE_DATA_LEN, core.headPos());
    var rec2 = try ring.replay(0);
    defer rec2.deinit();
    try testing.expectEqual(PAGE_DATA_LEN, rec2.head_pos);
    try testing.expectEqual(@as(usize, 1), rec2.entries.len);
    // Aligned already ⇒ nothing to do.
    try testing.expect((try planHeadAlignment(testing.allocator, &core, &reg, ring.layout, 0)) == null);
    // A ring within max_pad of full admits not even a pad.
    var full = jc.JournalCore.init(ring.geo, ring.geo.logicalLen() - 10, 0);
    var reg2 = try jc.InflightRegistry.init(testing.allocator, 0, 4);
    defer reg2.deinit();
    try testing.expectError(error.JournalReserveExhausted, planHeadAlignment(testing.allocator, &full, &reg2, ring.layout, 0));
}

test "rmw spans merge overlapping covering sectors" {
    var arena = std.heap.ArenaAllocator.init(testing.allocator);
    defer arena.deinit();
    const fifty: [50]u8 = @splat(0);
    const ten: [10]u8 = @splat(0);
    const ops = [_]WriteOp{
        .{ .offset = 100, .bytes = &fifty },
        .{ .offset = 600, .bytes = &ten },
        .{ .offset = 5000, .bytes = &ten },
    };
    const spans = try rmwSpans(arena.allocator(), 512, &ops);
    try testing.expectEqual(@as(usize, 2), spans.len);
    try testing.expectEqual(Span{ .lo = 0, .hi = 1024 }, spans[0]);
    try testing.expectEqual(Span{ .lo = 4608, .hi = 5120 }, spans[1]);
}

test "two-phase order: interior records sorted (level DESC, seq ASC); content records stay in entry order" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    const recs = [_]RecordFrame{
        .{ .tag = tagFor(1, 0), .key = "a", .kind = .put, .seq = 0, .value = "x" },
        .{ .tag = tagFor(1, 2), .key = "b", .kind = .put, .seq = 1, .value = "y" },
        .{ .tag = tagFor(0, 1), .key = "c", .kind = .put, .seq = 2, .value = "z" },
        .{ .tag = tagFor(2, 1), .key = "d", .kind = .delete, .seq = 3, .value = "" },
        .{ .tag = tagFor(1, 2), .key = "e", .kind = .put, .seq = 4, .value = "w" },
    };
    const len = try entryLenFor(&recs);
    const res = ring.core.reserve(ring.core.tryAdmit(len, .user).?);
    var img = try buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, res, &recs);
    defer img.deinit();
    applyOps(ring.image, 0, img.ops);
    var rec = try ring.replay(0);
    defer rec.deinit();
    const interior = try interiorRecordsSorted(testing.allocator, rec.entries);
    defer testing.allocator.free(interior);
    try testing.expectEqual(@as(usize, 4), interior.len);
    try testing.expectEqualStrings("b", interior[0].record.key);
    try testing.expectEqualStrings("e", interior[1].record.key);
    try testing.expectEqualStrings("c", interior[2].record.key);
    try testing.expectEqualStrings("d", interior[3].record.key);
    try testing.expectEqual(@as(u8, 2), interior[0].level);
    try testing.expectEqual(@as(u32, 1), interior[0].index);
    try testing.expectEqual(@as(u64, 0), interior[0].entry_seq);
    try testing.expectEqual(@as(u64, 0), seqOffsetOfWindow(rec.entries));
}

test "seq offset of a window: the max of (record seq − entry seq − index), saturating" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    const recs = [_]RecordFrame{
        .{ .tag = 1, .key = "a", .kind = .put, .seq = 1000, .value = "x" },
        .{ .tag = 1, .key = "b", .kind = .put, .seq = 1001, .value = "y" },
    };
    const len = try entryLenFor(&recs);
    const res = ring.core.reserve(ring.core.tryAdmit(len, .user).?);
    var img = try buildEntryWrite(testing.allocator, ring.layout, ring.geo, 0, res, &recs);
    defer img.deinit();
    applyOps(ring.image, 0, img.ops);
    var rec = try ring.replay(0);
    defer rec.deinit();
    try testing.expectEqual(@as(u64, 1000), seqOffsetOfWindow(rec.entries));
}

test "replay verdict: the gauge publishes dropped_torn; a clean unmount with drops fires the alert" {
    var ring = try TestRing.init(testing.allocator, 0, 4, 0, 1);
    defer ring.deinit();
    _ = try ring.append(1, 1000, 0x11);
    const b = try ring.append(2, 1000, 0x12);
    _ = try ring.append(3, 1000, 0x13);
    _ = try ring.append(4, 2000, 0x14); // crosses into page 1 so page 1's header names a start
    _ = try ring.append(5, 300, 0x15);
    ring.smash(ring.phys(b.start + 40), 8);
    var rec = try ring.replay(0);
    defer rec.deinit();
    try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
    const before = replay_dropped_after_clean_unmount.load();
    noteReplay(&rec, false);
    try testing.expectEqual(@as(u64, 1), replay_dropped_torn.load());
    try testing.expectEqual(before, replay_dropped_after_clean_unmount.load());
    log.testing.expectErrorLogs(1);
    noteReplay(&rec, true);
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, replay_dropped_after_clean_unmount.load());
}

test "failure latch: three consecutive journal failures latch once; a success resets; barriers count apart" {
    var latch: FailureLatch = .{};
    try testing.expect(!latch.noteFailure(.journal_write));
    try testing.expect(!latch.noteFailure(.journal_write));
    latch.noteSuccess(.journal_write);
    try testing.expect(!latch.noteFailure(.journal_write));
    try testing.expect(!latch.noteFailure(.journal_write));
    try testing.expect(!latch.isFailed());
    try latch.admitMutation();
    // Two barrier failures do not add to the journal run.
    try testing.expect(!latch.noteFailure(.barrier));
    try testing.expect(!latch.noteFailure(.barrier));
    log.testing.expectErrorLogs(1);
    try testing.expect(latch.noteFailure(.journal_write));
    try log.testing.expectAllFired();
    try testing.expect(latch.isFailed());
    try testing.expectError(error.VolumeFailed, latch.admitMutation());
    // Already latched: no second log, no second "latched now".
    try testing.expect(!latch.noteFailure(.barrier));
    try testing.expectEqual(JOURNAL_FAILURE_LATCH, @as(u32, 3));
    // The protocol-terminal latch: once, then inert.
    var wedge: FailureLatch = .{};
    try testing.expect(wedge.failStop());
    try testing.expect(wedge.isFailed());
    try testing.expect(!wedge.failStop());
    try testing.expectError(error.VolumeFailed, wedge.admitMutation());
    try testing.expectEqual(FailureLatch.Verdict.failed, wedge.verdict());
    try testing.expectEqual(FailureLatch.Verdict.healthy, @as(FailureLatch, .{}).verdict());
}

test "the fence rung: ONE reservation conflict at any site latches FAILED at once (no 3-count), typed fenced, counted on writer_guard_fenced and the volume_fenced gauge; every other errno is not the fence; a second conflict logs nothing" {
    var latch: FailureLatch = .{};
    const fenced0 = guard.stats.fenced.load();
    const gauge0 = guard.stats.volume_fenced.load();
    try testing.expectEqual(FailureLatch.FenceVerdict.not_fence, latch.classifyFence(error.InputOutput, "entry write"));
    try testing.expect(!latch.isFailed());
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(FailureLatch.FenceVerdict.latched, latch.classifyFence(error.ReservationConflict, "entry write"));
    try log.testing.expectAllFired();
    try testing.expect(latch.isFailed());
    try testing.expect(latch.isFenced());
    try testing.expectEqual(FailureLatch.Verdict.fenced, latch.verdict());
    try testing.expectError(error.VolumeFailed, latch.admitMutation());
    try testing.expectEqual(fenced0 + 1, guard.stats.fenced.load());
    try testing.expectEqual(gauge0 + 1, guard.stats.volume_fenced.load());
    // Already latched: classified, not re-logged, not re-counted.
    try testing.expectEqual(FailureLatch.FenceVerdict.already, latch.classifyFence(error.ReservationConflict, "barrier"));
    try testing.expect(!latch.noteFence("barrier"));
    try testing.expectEqual(fenced0 + 1, guard.stats.fenced.load());
    // The generic rung on a fenced volume adds nothing new.
    try testing.expect(!latch.noteFailure(.barrier));
}

test "the one-line law of a FAILED volume: every failure line logs while healthy; once FAILED each class logs at most once per latch (none when it already logged while healthy) and the rest are counted, not logged; a remount's fresh latch logs again" {
    var latch: FailureLatch = .{};
    const quieted0 = meta_failed_volume_lines_quieted.load();
    // Healthy: every line, however many.
    for (0..5) |_| try testing.expect(latch.admitsLine(.node_read));
    try testing.expect(latch.failStop());
    // FAILED: node_read already logged on this latch — quiet from here on;
    // bset_append and window_write log their first line, then quiet.
    const n_ops = 64;
    var lines: [3]usize = @splat(0);
    for (0..n_ops) |_| {
        if (latch.admitsLine(.node_read)) lines[0] += 1;
        if (latch.admitsLine(.bset_append)) lines[1] += 1;
        if (latch.admitsLine(.window_write)) lines[2] += 1;
    }
    try testing.expectEqualSlices(usize, &.{ 0, 1, 1 }, &lines);
    try testing.expectEqual(quieted0 + 3 * n_ops - 2, meta_failed_volume_lines_quieted.load());
    // The next mount's latch is a new episode.
    var remount: FailureLatch = .{};
    try testing.expect(remount.failStop());
    try testing.expect(remount.admitsLine(.bset_append));
    try testing.expect(!remount.admitsLine(.bset_append));
}

test "power-cut harness: unsynced writes revert in reverse order; synced writes survive" {
    var image: [64]u8 = @splat(0);
    var vol = VolatileImage.init(testing.allocator, &image, 0);
    defer vol.deinit();
    try vol.write(0, "aaaa");
    vol.fdatasync();
    try vol.write(2, "bb");
    try vol.write(3, "c");
    try testing.expectEqualStrings("aabc", image[0..4]);
    try testing.expectEqual(@as(usize, 2), vol.powerCut());
    try testing.expectEqualStrings("aaaa", image[0..4]);
    try testing.expectEqual(@as(usize, 0), vol.powerCut());
}

test "ring reuse never overwrites the fallback window: the head bounds against reusable_upto, not the in-RAM tail" {
    var ring = try TestRing.init(testing.allocator, 0, 8, 0, 1); // L = 32,576
    defer ring.deinit();
    var rs: [6]Reservation = undefined;
    for (0..5) |i| rs[i] = try ring.append(@intCast(i + 1), 6000, @intCast(0x60 + i));
    try testing.expectEqual(@as(u64, 30000), ring.core.headPos());
    // Checkpoint 2 moved the in-RAM tail to e4 but is NOT known durable: the
    // watermark stays, and admission is refused.
    try testing.expectEqual(@as(?jc.Admission, null), ring.core.tryAdmit(6000, .user));
    // A mount falling back to checkpoint 1 (tail e1) replays the intact window.
    {
        var rec = try ring.replay(rs[0].seq());
        defer rec.deinit();
        var inos: [8]u64 = undefined;
        try testing.expectEqualSlices(u64, &.{ 1, 2, 3, 4, 5 }, recoveredInos(&rec, &inos));
        try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    }
    // Now durable: the watermark advances, the 6th wraps over retired pages.
    ring.core.advanceReusableUpto(rs[3].seq());
    rs[5] = try ring.append(6, 6000, 0x66);
    try testing.expect(rs[5].end() > ring.geo.logicalLen());
    var rec = try ring.replay(rs[3].seq());
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 4, 5, 6 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
}

test "concurrent appenders through the image path: 8 threads × 4 entries, unique sorted seqs, 32 replayed" {
    const Shared = struct { ring: TestRing, lock: jc.SpinLock = .{} };
    var sh: Shared = .{ .ring = try TestRing.init(testing.allocator, 0, 64, 0, 1) };
    defer sh.ring.deinit();
    const Worker = struct {
        fn run(s: *Shared, t: usize) void {
            for (0..4) |i| {
                // The image is shared; ops land under a lock (the device
                // serialises its own writes).
                var key: [9]u8 = undefined;
                var value: [MAX_ENTRY_LEN]u8 = undefined;
                const adm = s.ring.core.tryAdmit(230, .user).?;
                const r = s.ring.reg.reserveRegistered(&s.ring.core, adm) catch unreachable;
                const recs = sizedRecords(t * 4 + i + 1, 230, 0x33, r.seq_base, &key, &value);
                var img = buildEntryWrite(s.ring.gpa, s.ring.layout, s.ring.geo, 0, r.res, &recs) catch unreachable;
                defer img.deinit();
                s.lock.lock();
                applyOps(s.ring.image, 0, img.ops);
                s.lock.unlock();
                s.ring.reg.complete(&s.ring.core, r.res);
            }
        }
    };
    var threads: [8]std.Thread = undefined;
    for (&threads, 0..) |*th, t| th.* = try std.Thread.spawn(.{}, Worker.run, .{ &sh, t });
    for (threads) |th| th.join();
    try testing.expectEqual(@as(u64, 32 * 230), sh.ring.core.headPos());
    try testing.expectEqual(@as(u64, 0), sh.ring.core.admitted());
    var rec = try sh.ring.replay(0);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 32), rec.entries.len);
    for (rec.entries, 0..) |e, i| try testing.expectEqual(@as(u64, i) * 230, e.seq);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
}

fn journalFramesFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    const bytes = buf[0..len];
    _ = parsePageHeader(bytes);
    if (len >= PAGE_HDR_LEN) {
        // Restamped: the page header's checksum over the fuzzed first 16 bytes.
        std.mem.writeInt(u64, buf[16..24], checksum.hash64(buf[0..16]), .little);
        _ = parsePageHeader(bytes);
    }
    if (decodeRecordFrame(bytes)) |d| {
        try std.testing.expect(d.used <= bytes.len);
        const re = try std.testing.allocator.alloc(u8, @intCast(d.frame.frameLen()));
        defer std.testing.allocator.free(re);
        d.frame.encodeInto(re);
        try std.testing.expectEqualSlices(u8, bytes[0..d.used], re);
    } else |_| {}
}

test "fuzz: the journal page header and record frame decoders are total; a decoded frame re-encodes to the bytes it consumed" {
    try fuzzkit.fuzz({}, journalFramesFuzz, .{});
}
