//! The staging ring (charter §6.1 "Staged"; §1.2 the mmap exception;
//! `docs/porting/data_layouts.md` §3, §11 Q-7–Q-10). A staged file's whole
//! payload lives in the writing host's local NVMe ring — mmap'd segment files
//! under the format-declared cache paths (0700 dirs / 0600 files, opened
//! `O_NOFOLLOW|O_CLOEXEC`) — until ONE promotion primitive moves it to a
//! durable layout. `get`/`put` are zero-syscall over the mapping.
//!
//! PORTABLE: the segment record codec (the v4 header carries a checksum over
//! header + key + value — the reference trusted sane lengths and let a torn
//! payload decode as plausible bytes), the two-phase crash-safe replace over
//! a byte image, the pool budget with its 75 % high-water, the promotion
//! points as hooks, the recovery scan, the identity marker, and the reader's
//! view of an unpromoted entry (size-consistent zeros for OTHER clients —
//! `staged_payload_lost_reads`, stated and counted). LINUX-ONLY (`Ring`): the
//! directory law, `mmap`/`msync`/`fallocate(PUNCH_HOLE)`/`madvise`, the
//! marker files through the io_uring file layer.
//!
//! The ring key is `file_id[16] ‖ ino u64` so the recovery scan names the
//! inode without a path (Q-7/Q-10); the stage generation in the header makes
//! the scan's last-wins exact; a mount-recovered entry seeds the ledger at
//! generation 0, which no live assignment can forge (the counter starts at 1
//! — the generation ABA model below pins it).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");
const layout = @import("layout.zig");
const explorer = @import("../model/explorer.zig");

pub const FileId = layout.FileId;

pub const stats = struct {
    /// `current_staged_write_bytes`: the pool's charged footprint (the
    /// budget component `staging_mmap`'s gauge).
    pub var current_staged_write_bytes: counters.Gauge = .{};
    /// `staged_writes_in_flight`: ring keys resident or being placed.
    pub var staged_writes_in_flight: counters.Gauge = .{};
    /// `staging_over_cap_refusals`: a stage refused `StorageFull` at the
    /// pool cap (the caller's spill escalation follows).
    pub var staging_over_cap_refusals: counters.ShardedCounter = .{};
    /// `staging_ring_full_refusals`: a placement found no hole (loud
    /// backpressure; never destroys a live entry).
    pub var staging_ring_full_refusals: counters.ShardedCounter = .{};
    /// `staging_pressure_kicks`: high-water crossings that enqueued a
    /// promotion.
    pub var staging_pressure_kicks: counters.ShardedCounter = .{};
    /// `staging_recovery_records`: records a recovery scan adopted.
    pub var staging_recovery_records: counters.ShardedCounter = .{};
    /// `staging_recovery_torn_records`: headers whose checksum or geometry
    /// failed at recovery — dropped loud.
    pub var staging_recovery_torn_records: counters.ShardedCounter = .{};
    /// `staging_generation_discards`: a ring bound to a dead identity was
    /// wiped at bind.
    pub var staging_generation_discards: counters.ShardedCounter = .{};
    /// `staging_foreign_scope_refusals`: a PEER mount's live custody of the
    /// ring refused ours.
    pub var staging_foreign_scope_refusals: counters.ShardedCounter = .{};
    /// `staged_payload_lost_reads`: zeros served for an unreachable staged
    /// payload (another client's live custody, or a crash discarded it).
    /// 0 on any client of a cleanly unmounted set (gate G-PK2).
    pub var staged_payload_lost_reads: counters.ShardedCounter = .{};
    /// `dismount_promoted_files` / `dismount_promoted_bytes` /
    /// `dismount_promote_failures`: the clean-unmount promotion pass.
    pub var dismount_promoted_files: counters.ShardedCounter = .{};
    pub var dismount_promoted_bytes: counters.ShardedCounter = .{};
    pub var dismount_promote_failures: counters.ShardedCounter = .{};
    /// `fsync_promoted_files`: promotions the fsync lever dispatched.
    pub var fsync_promoted_files: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"current_staged_write_bytes\":{d},\"staged_writes_in_flight\":{d},\"staging_over_cap_refusals\":{d},\"staging_ring_full_refusals\":{d},\"staging_pressure_kicks\":{d},\"staging_recovery_records\":{d},\"staging_recovery_torn_records\":{d},\"staging_generation_discards\":{d},\"staging_foreign_scope_refusals\":{d},\"staged_payload_lost_reads\":{d},\"dismount_promoted_files\":{d},\"dismount_promoted_bytes\":{d},\"dismount_promote_failures\":{d},\"fsync_promoted_files\":{d}}}", .{
        stats.current_staged_write_bytes.load(),
        stats.staged_writes_in_flight.load(),
        stats.staging_over_cap_refusals.load(),
        stats.staging_ring_full_refusals.load(),
        stats.staging_pressure_kicks.load(),
        stats.staging_recovery_records.load(),
        stats.staging_recovery_torn_records.load(),
        stats.staging_generation_discards.load(),
        stats.staging_foreign_scope_refusals.load(),
        stats.staged_payload_lost_reads.load(),
        stats.dismount_promoted_files.load(),
        stats.dismount_promoted_bytes.load(),
        stats.dismount_promote_failures.load(),
        stats.fsync_promoted_files.load(),
    });
}

// ---------------------------------------------------------------------------
// §3.2 The segment record — v4, checksummed.
// ---------------------------------------------------------------------------

/// ASCII in disk order (0002 row 38).
pub const SEGMENT_MAGIC = "SQZS".*;
pub const SEGMENT_RECORD_VERSION: u16 = 1;
/// Records and their values sit on the LBA grain: the value is the zero-copy
/// DMA source of a promotion (`O_DIRECT` refuses an unaligned buffer) and a
/// header on the grain is what the recovery scan steps by.
pub const RECORD_ALIGN: u64 = layout.GRAIN;

/// 40 B, little-endian; `xxh3` over header (field zeroed) ‖ key ‖ value.
pub const SegmentRecordWire = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    /// = 0 (checked).
    flags_le: [2]u8,
    key_len_le: [4]u8,
    val_len_le: [4]u8,
    generation_le: [8]u8,
    fencing_le: [8]u8,
    xxh3_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = @sizeOf(SegmentRecordWire);
const CHECKSUM_OFF: usize = @offsetOf(SegmentRecordWire, "xxh3_le");

/// `file_id[16] ‖ ino u64 LE` — 24 B.
pub const StagingKey = extern struct {
    file_id: FileId,
    ino_le: [8]u8,

    pub fn init(file_id: FileId, ino_: u64) StagingKey {
        var k: StagingKey = .{ .file_id = file_id, .ino_le = undefined };
        std.mem.writeInt(u64, &k.ino_le, ino_, .little);
        return k;
    }

    pub fn ino(self: StagingKey) u64 {
        return std.mem.readInt(u64, &self.ino_le, .little);
    }

    pub fn bytes(self: *const StagingKey) *const [KEY_LEN]u8 {
        return @ptrCast(self);
    }
};

pub const KEY_LEN: usize = @sizeOf(StagingKey);

comptime {
    assert(RECORD_HEADER_LEN == 40);
    assert(CHECKSUM_OFF == 32);
    assert(KEY_LEN == 24);
}

/// The value's offset inside a record: header + key rounded to the grain.
pub fn valueOffset(key_len: u64) u64 {
    return derive.alignUp(RECORD_HEADER_LEN + key_len, RECORD_ALIGN);
}

/// Bytes a record occupies from its header to the end of its value.
pub fn footprint(key_len: u64, val_len: u64) u64 {
    return valueOffset(key_len) + val_len;
}

/// The next record slot after one of this footprint.
pub fn stride(key_len: u64, val_len: u64) u64 {
    return derive.alignUp(footprint(key_len, val_len), RECORD_ALIGN);
}

pub const RecordHeader = struct {
    key_len: u32,
    val_len: u32,
    generation: u64,
    fencing: u64,
    xxh3: u64,

    pub fn footprintBytes(self: RecordHeader) u64 {
        return footprint(self.key_len, self.val_len);
    }
};

/// Write `header ‖ key ‖ zero pad ‖ value` at `rec_off` and stamp the
/// checksum. The caller reserved `stride(key.len, value.len)` bytes.
pub fn writeRecord(image: []u8, rec_off: u64, key: []const u8, value: []const u8, generation: u64, fencing: u64) void {
    const voff = valueOffset(key.len);
    assert(rec_off + voff + value.len <= image.len);
    assert(derive.isAligned(rec_off, RECORD_ALIGN));
    var h: SegmentRecordWire = .{ .magic = SEGMENT_MAGIC, .version_le = undefined, .flags_le = .{ 0, 0 }, .key_len_le = undefined, .val_len_le = undefined, .generation_le = undefined, .fencing_le = undefined, .xxh3_le = @splat(0) };
    std.mem.writeInt(u16, &h.version_le, SEGMENT_RECORD_VERSION, .little);
    std.mem.writeInt(u32, &h.key_len_le, @intCast(key.len), .little);
    std.mem.writeInt(u32, &h.val_len_le, @intCast(value.len), .little);
    std.mem.writeInt(u64, &h.generation_le, generation, .little);
    std.mem.writeInt(u64, &h.fencing_le, fencing, .little);
    const rec = image[@intCast(rec_off)..];
    rec[0..RECORD_HEADER_LEN].* = std.mem.toBytes(h);
    @memcpy(rec[RECORD_HEADER_LEN..][0..key.len], key);
    @memset(rec[RECORD_HEADER_LEN + key.len .. @intCast(voff)], 0);
    @memcpy(rec[@intCast(voff)..][0..value.len], value);
    std.mem.writeInt(u64, rec[CHECKSUM_OFF..][0..8], recordChecksum(rec[0..RECORD_HEADER_LEN], key, rec[@intCast(voff)..][0..value.len]), .little);
}

fn recordChecksum(header: []const u8, key: []const u8, value: []const u8) u64 {
    var h = std.hash.XxHash3.init(0);
    h.update(header[0..CHECKSUM_OFF]);
    const zeros: [8]u8 = @splat(0);
    h.update(&zeros);
    h.update(key);
    h.update(value);
    return h.final();
}

/// Parse the header at `rec_off`: magic, version, zero flags, a geometry
/// that fits the image. Null = no record starts here.
pub fn readHeader(image: []const u8, rec_off: u64) ?RecordHeader {
    if (rec_off + RECORD_HEADER_LEN > image.len) return null;
    const h = std.mem.bytesToValue(SegmentRecordWire, image[@intCast(rec_off)..][0..RECORD_HEADER_LEN]);
    if (!std.mem.eql(u8, &h.magic, &SEGMENT_MAGIC)) return null;
    if (std.mem.readInt(u16, &h.version_le, .little) != SEGMENT_RECORD_VERSION) return null;
    if (std.mem.readInt(u16, &h.flags_le, .little) != 0) return null;
    const r: RecordHeader = .{
        .key_len = std.mem.readInt(u32, &h.key_len_le, .little),
        .val_len = std.mem.readInt(u32, &h.val_len_le, .little),
        .generation = std.mem.readInt(u64, &h.generation_le, .little),
        .fencing = std.mem.readInt(u64, &h.fencing_le, .little),
        .xxh3 = std.mem.readInt(u64, &h.xxh3_le, .little),
    };
    if (rec_off + r.footprintBytes() > image.len) return null;
    return r;
}

/// Does the record at `rec_off` carry the checksum its bytes compute?
pub fn verifyRecord(image: []const u8, rec_off: u64, h: RecordHeader) bool {
    const rec = image[@intCast(rec_off)..];
    const voff: usize = @intCast(valueOffset(h.key_len));
    return h.xxh3 == recordChecksum(rec[0..RECORD_HEADER_LEN], rec[RECORD_HEADER_LEN..][0..h.key_len], rec[voff..][0..h.val_len]);
}

/// Whether the record header at `rec_off` reads as a tombstone (magic
/// zeroed by a removal).
pub fn isTombstone(image: []const u8, rec_off: u64) bool {
    return std.mem.allEqual(u8, image[@intCast(rec_off)..][0..4], 0);
}

// ---------------------------------------------------------------------------
// §3.2 The segment — placement, two-phase replace, leases, recovery.
// ---------------------------------------------------------------------------

/// What backs the image: the Linux ring binds `msync`/`fallocate`+`madvise`;
/// a RAM image (tests, the portable core) binds nothing.
pub const Backing = struct {
    ctx: ?*anyopaque = null,
    /// Flush `[off, off + len)` (MS_ASYNC: the write is ordered, the
    /// promotion's own DMA reads the mapping, never the file).
    sync: ?*const fn (ctx: ?*anyopaque, off: u64, len: u64) void = null,
    /// Return `[off, off + len)` to the host (punch + drop pages) after a
    /// tombstone; a cleanly unmounted dir scans EMPTY.
    reclaim: ?*const fn (ctx: ?*anyopaque, off: u64, len: u64) void = null,

    pub const none: Backing = .{};
};

/// A placed record.
pub const Extent = struct {
    rec_off: u64,
    key_len: u32,
    val_len: u32,
    generation: u64,

    pub fn valOff(self: Extent) u64 {
        return self.rec_off + valueOffset(self.key_len);
    }

    pub fn end(self: Extent) u64 {
        return self.rec_off + stride(self.key_len, self.val_len);
    }
};

/// One live entry's cell: readers hold references; the index holds one.
/// The LAST reference out reclaims a retired cell's bytes — so a same-key
/// replace never destroys the copy a reader is inside, and a reader never
/// sees a torn record (one intact copy at every instant).
const Cell = struct {
    refs: std.atomic.Value(u32) = .init(0),
    retired: std.atomic.Value(bool) = .init(false),
    extent: Extent = undefined,
    key: [KEY_LEN]u8 = undefined,
};

pub const PutError = error{
    /// No hole fits (the hot-path refusal the caller escalates to a
    /// durable spill); counted.
    RingFull,
    OutOfMemory,
};

pub const Segment = struct {
    gpa: Allocator,
    image: []u8,
    backing: Backing,
    lock: jc.SpinLock = .{},
    index: std.AutoHashMapUnmanaged([KEY_LEN]u8, *Cell) = .empty,
    pending: std.ArrayList(Extent) = .empty,
    /// The placement cursor: records go at/after it, wrapping.
    write_offset: u64 = 0,
    cells: []Cell,
    free_cells: std.ArrayList(u32) = .empty,
    /// Placement scratch (live extents, sorted) — sized once.
    scratch: []Extent,

    /// Every live or pending record occupies ≥ 1 grain of its own, so the
    /// cell population is bounded by `capacity / grain`; one more for the
    /// replacement copy in flight.
    pub fn maxCells(cap: u64) usize {
        return @intCast(cap / RECORD_ALIGN + 1);
    }

    pub fn init(gpa: Allocator, image: []u8, backing: Backing) Allocator.Error!Segment {
        assert(derive.isAligned(image.len, RECORD_ALIGN));
        const n = maxCells(image.len);
        const cells = try gpa.alloc(Cell, n);
        errdefer gpa.free(cells);
        @memset(cells, .{});
        var free = try std.ArrayList(u32).initCapacity(gpa, n);
        errdefer free.deinit(gpa);
        var i: u32 = @intCast(n);
        while (i > 0) : (i -= 1) free.appendAssumeCapacity(i - 1);
        const scratch = try gpa.alloc(Extent, n + 1);
        errdefer gpa.free(scratch);
        var self: Segment = .{ .gpa = gpa, .image = image, .backing = backing, .cells = cells, .free_cells = free, .scratch = scratch };
        try self.index.ensureTotalCapacity(gpa, @intCast(n));
        try self.pending.ensureTotalCapacity(gpa, n);
        return self;
    }

    pub fn deinit(self: *Segment) void {
        self.index.deinit(self.gpa);
        self.pending.deinit(self.gpa);
        self.free_cells.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.scratch);
        self.* = undefined;
    }

    pub fn capacity(self: *const Segment) u64 {
        return self.image.len;
    }

    pub fn live(self: *Segment) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.count();
    }

    // ---- leases ----

    /// A reader's view over the mapping: `bytes()` is the value in place.
    pub const Lease = struct {
        seg: *Segment,
        cell: *Cell,

        pub fn bytes(self: Lease) []const u8 {
            const e = self.cell.extent;
            return self.seg.image[@intCast(e.valOff())..][0..e.val_len];
        }

        pub fn generation(self: Lease) u64 {
            return self.cell.extent.generation;
        }

        /// Drop the reference; the last one out of a retired cell
        /// reclaims it. acq_rel pairs with `retire`'s release store.
        pub fn release(self: Lease) void {
            if (self.cell.refs.fetchSub(1, .acq_rel) == 1 and self.cell.retired.load(.acquire)) self.seg.reclaimCell(self.cell);
        }
    };

    /// Zero-syscall: one O(1) locked lookup + a reference. The lease must
    /// be released.
    pub fn get(self: *Segment, key: StagingKey) ?Lease {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return null;
        _ = cell.refs.fetchAdd(1, .monotonic);
        return .{ .seg = self, .cell = cell };
    }

    /// The resident copy's generation (the promotion's identity probe).
    pub fn residentGeneration(self: *Segment, key: StagingKey) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return null;
        return cell.extent.generation;
    }

    // ---- the two-phase replace ----

    /// Phase 1 under the lock: `live` = every indexed extent (INCLUDING the
    /// current copy of this key) ∪ the pending placements, sorted;
    /// candidates = the aligned cursor, 0, and the end of every live extent
    /// (first fit over the holes promotion/unlink punched); at/after the
    /// cursor first, then wrapped. No fit ⇒ `RingFull`, loud — never
    /// destroys a live entry. Phase 1b outside the lock: the bytes.
    /// Phase 2 under the lock: flip the index, drop the pending entry,
    /// retire the superseded copy. A same-key replace is ATOMIC for
    /// readers and torn-write immune (a crash between the phases leaves two
    /// copies; recovery dedups by generation).
    pub fn put(self: *Segment, key: StagingKey, value: []const u8, generation: u64, fencing: u64) PutError!void {
        const need = stride(KEY_LEN, value.len);
        if (need > self.image.len) {
            stats.staging_ring_full_refusals.inc();
            return error.RingFull;
        }
        const placed: Extent = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const rec_off = self.findHoleLocked(need) orelse {
                stats.staging_ring_full_refusals.inc();
                return error.RingFull;
            };
            const e: Extent = .{ .rec_off = rec_off, .key_len = KEY_LEN, .val_len = @intCast(value.len), .generation = generation };
            self.pending.appendAssumeCapacity(e);
            self.write_offset = e.end();
            if (self.write_offset >= self.image.len) self.write_offset = 0;
            break :blk e;
        };
        stats.staged_writes_in_flight.add(1);
        writeRecord(self.image, placed.rec_off, key.bytes(), value, generation, fencing);
        if (self.backing.sync) |s| s(self.backing.ctx, placed.rec_off, need);
        self.lock.lock();
        defer self.lock.unlock();
        self.dropPendingLocked(placed.rec_off);
        const cell = self.allocCellLocked();
        cell.* = .{ .refs = .init(1), .extent = placed, .key = key.bytes().* };
        const gop = self.index.getOrPutAssumeCapacity(key.bytes().*);
        if (gop.found_existing) {
            const old = gop.value_ptr.*;
            gop.value_ptr.* = cell;
            self.retireLocked(old);
        } else {
            gop.value_ptr.* = cell;
        }
        stats.staged_writes_in_flight.sub(1);
    }

    /// Remove `key` iff present (and, when `expect_generation` is given,
    /// iff the resident copy carries it — the promotion's generation gate:
    /// a re-stage that landed since keeps its entry). True iff removed.
    pub fn remove(self: *Segment, key: StagingKey, expect_generation: ?u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return false;
        if (expect_generation) |g| if (cell.extent.generation != g) return false;
        _ = self.index.remove(key.bytes().*);
        self.retireLocked(cell);
        return true;
    }

    fn retireLocked(self: *Segment, cell: *Cell) void {
        // Tombstone the magic so a recovery scan never adopts the dead copy,
        // then let the last reference reclaim the bytes. release pairs with
        // `Lease.release`'s acq_rel.
        @memset(self.image[@intCast(cell.extent.rec_off)..][0..4], 0);
        if (self.backing.sync) |s| s(self.backing.ctx, cell.extent.rec_off, RECORD_ALIGN);
        cell.retired.store(true, .release);
        if (cell.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(cell);
    }

    fn reclaimCell(self: *Segment, cell: *Cell) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.reclaimCellLocked(cell);
    }

    fn reclaimCellLocked(self: *Segment, cell: *Cell) void {
        const e = cell.extent;
        if (self.backing.reclaim) |r| r(self.backing.ctx, e.rec_off, e.end() - e.rec_off);
        cell.* = .{};
        const idx: usize = (@intFromPtr(cell) - @intFromPtr(self.cells.ptr)) / @sizeOf(Cell);
        self.free_cells.appendAssumeCapacity(@intCast(idx));
    }

    fn allocCellLocked(self: *Segment) *Cell {
        // Bounded by construction (`maxCells`): a hole for a record exists
        // only while a cell for it does.
        const idx = self.free_cells.pop().?;
        return &self.cells[idx];
    }

    fn dropPendingLocked(self: *Segment, rec_off: u64) void {
        for (self.pending.items, 0..) |p, i| {
            if (p.rec_off == rec_off) {
                _ = self.pending.swapRemove(i);
                return;
            }
        }
        unreachable;
    }

    fn extentLess(_: void, a: Extent, b: Extent) bool {
        return a.rec_off < b.rec_off;
    }

    /// First fit over the live set; null when nothing fits.
    fn findHoleLocked(self: *Segment, need: u64) ?u64 {
        var n: usize = 0;
        var it = self.index.valueIterator();
        while (it.next()) |c| {
            self.scratch[n] = c.*.extent;
            n += 1;
        }
        for (self.pending.items) |p| {
            self.scratch[n] = p;
            n += 1;
        }
        const live_set = self.scratch[0..n];
        std.mem.sort(Extent, live_set, {}, extentLess);
        const cap = self.image.len;
        const cursor = derive.alignUp(self.write_offset, RECORD_ALIGN);
        // Pass 1: candidates at/after the cursor; pass 2: wrapped below it.
        // A candidate's fit is always judged against the whole ring, so a
        // hole straddling the cursor is found from its own start.
        if (self.tryCandidatesLocked(live_set, need, cursor, cap, cap)) |off| return off;
        if (self.tryCandidatesLocked(live_set, need, 0, cursor, cap)) |off| return off;
        return null;
    }

    /// Candidates inside `[lo, hi)`: `lo` itself and the end of every live
    /// extent; the first that fits `[c, c + need)` inside `[0, cap)` with no
    /// live overlap.
    fn tryCandidatesLocked(_: *Segment, live_set: []const Extent, need: u64, lo: u64, hi: u64, cap: u64) ?u64 {
        if (fits(live_set, lo, need, cap)) return lo;
        for (live_set) |e| {
            const c = e.end();
            if (c < lo or c >= hi) continue;
            if (fits(live_set, c, need, cap)) return c;
        }
        return null;
    }

    fn fits(live_set: []const Extent, c: u64, need: u64, hi: u64) bool {
        if (c + need > hi) return false;
        for (live_set) |e| {
            if (c < e.end() and e.rec_off < c + need) return false;
        }
        return true;
    }

    // ---- recovery ----

    pub const RecoverReport = struct {
        adopted: u64 = 0,
        torn: u64 = 0,
        superseded: u64 = 0,
        bytes: u64 = 0,
    };

    /// §3.6: a linear header scan at the grain over an EMPTY index. A valid,
    /// checksummed record is adopted and its whole footprint skipped (value
    /// interiors are never scanned for headers); a magic whose header or
    /// checksum fails is dropped loud and the scan resyncs at the next
    /// grain; a tombstone or a zero grain steps one grain. Same-key copies:
    /// the higher generation wins (a crash between the replace's phases
    /// left both), equal ⇒ the later offset. `write_offset` = the max end.
    pub fn recover(self: *Segment) RecoverReport {
        self.lock.lock();
        defer self.lock.unlock();
        assert(self.index.count() == 0 and self.pending.items.len == 0);
        var rep: RecoverReport = .{};
        var off: u64 = 0;
        var max_end: u64 = 0;
        while (off + RECORD_HEADER_LEN <= self.image.len) {
            const h = readHeader(self.image, off) orelse {
                if (!isTombstone(self.image, off) and std.mem.eql(u8, self.image[@intCast(off)..][0..4], &SEGMENT_MAGIC)) rep.torn += 1;
                off += RECORD_ALIGN;
                continue;
            };
            if (h.key_len != KEY_LEN or !verifyRecord(self.image, off, h)) {
                rep.torn += 1;
                off += RECORD_ALIGN;
                continue;
            }
            const e: Extent = .{ .rec_off = off, .key_len = h.key_len, .val_len = h.val_len, .generation = h.generation };
            const k: [KEY_LEN]u8 = self.image[@intCast(off + RECORD_HEADER_LEN)..][0..KEY_LEN].*;
            const gop = self.index.getOrPutAssumeCapacity(k);
            if (gop.found_existing) {
                const old = gop.value_ptr.*;
                // Later offset wins a tie: the scan is ascending.
                if (h.generation >= old.extent.generation) {
                    gop.value_ptr.* = self.allocCellLocked();
                    gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k };
                    self.retireLocked(old);
                } else {
                    @memset(self.image[@intCast(off)..][0..4], 0);
                    if (self.backing.reclaim) |r| r(self.backing.ctx, off, e.end() - off);
                }
                rep.superseded += 1;
            } else {
                gop.value_ptr.* = self.allocCellLocked();
                gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k };
                rep.adopted += 1;
                rep.bytes += stride(h.key_len, h.val_len);
            }
            max_end = @max(max_end, e.end());
            off = e.end();
        }
        self.write_offset = if (max_end >= self.image.len) 0 else max_end;
        stats.staging_recovery_records.add(rep.adopted);
        stats.staging_recovery_torn_records.add(rep.torn);
        if (rep.torn != 0) log.warn("staging recovery: dropped {d} torn record(s) (bad header or checksum) in a {d} B segment; adopted {d}", .{ rep.torn, self.image.len, rep.adopted });
        return rep;
    }

    /// Every resident key with its footprint (the ledger seed, the
    /// dismount pass). `out` holds ≥ `live()` entries.
    pub fn residents(self: *Segment, out: []Resident) []Resident {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        var it = self.index.valueIterator();
        while (it.next()) |c| {
            const e = c.*.extent;
            out[n] = .{ .key = std.mem.bytesToValue(StagingKey, &c.*.key), .cost = stride(e.key_len, e.val_len), .generation = e.generation };
            n += 1;
        }
        return out[0..n];
    }

    /// The live extents (tests: the no-overlap law).
    pub fn liveExtents(self: *Segment, out: []Extent) []Extent {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        var it = self.index.valueIterator();
        while (it.next()) |c| {
            out[n] = c.*.extent;
            n += 1;
        }
        return out[0..n];
    }
};

pub const Resident = struct { key: StagingKey, cost: u64, generation: u64 };

// ---------------------------------------------------------------------------
// §3.3–§3.4 The pool: budget, ledger, generations, the high-water.
// ---------------------------------------------------------------------------

/// Generation 0 is RESERVED for mount-recovered entries: a live stage can
/// never mint it, so a stale promotion holding 0 can never match a fresh
/// entry (the ABA model below).
pub const RECOVERED_GENERATION: u64 = 0;
pub const FIRST_GENERATION: u64 = 1;

pub const StageError = error{
    /// Over the pool cap (after kicking the promoter) or no ring hole:
    /// the caller escalates to the durable spill; the write never records
    /// a byte it cannot hold.
    StorageFull,
    OutOfMemory,
};

pub const Staged = struct {
    generation: u64,
    /// The high-water fired and a promotion was enqueued.
    kicked: bool,
};

/// The three promotion points reach the ONE primitive through this seam
/// (`data/pack.zig`'s prepare → commit); the ring never promotes inline.
pub const PromotionHooks = struct {
    ctx: ?*anyopaque = null,
    /// Pressure: enqueue `file_id` to the merge worker (the hot path itself
    /// never promotes).
    pressure: ?*const fn (ctx: ?*anyopaque, key: StagingKey) void = null,
    /// Promote now (dismount, fsync): true iff the entry left the ring.
    promote: ?*const fn (ctx: ?*anyopaque, key: StagingKey) bool = null,

    pub const none: PromotionHooks = .{};
};

const LedgerRow = struct { cost: u64, generation: u64 };

pub const Pool = struct {
    gpa: Allocator,
    /// The write half of the ring budget (¼ of the dirs' aggregate capacity
    /// by default — `cache/mod.rs`'s config, a set fact).
    max_write_bytes: u64,
    hooks: PromotionHooks,
    lock: jc.SpinLock = .{},
    /// Every charge/credit goes through the ledger so the gauge can never
    /// ratchet: `Σ cost == current_staged_write_bytes`.
    ledger: std.AutoHashMapUnmanaged(FileId, LedgerRow) = .empty,
    current: std.atomic.Value(u64) = .init(0),
    next_generation: std.atomic.Value(u64) = .init(FIRST_GENERATION),

    pub fn init(gpa: Allocator, max_write_bytes: u64, hooks: PromotionHooks) Pool {
        return .{ .gpa = gpa, .max_write_bytes = max_write_bytes, .hooks = hooks };
    }

    pub fn deinit(self: *Pool) void {
        stats.current_staged_write_bytes.sub(self.current.load(.monotonic));
        self.ledger.deinit(self.gpa);
        self.* = undefined;
    }

    /// `max − max/4`: arm the drain at 75 % so the merge worker credits
    /// space BEFORE the pool hard-fills and a writer meets `StorageFull`;
    /// the quarter is one promotion batch's worth of headroom at the
    /// 4 MiB/500 ms batch law.
    pub fn highWater(self: *const Pool) u64 {
        return self.max_write_bytes - self.max_write_bytes / 4;
    }

    pub fn currentBytes(self: *const Pool) u64 {
        return self.current.load(.monotonic);
    }

    /// A re-stage charges the DELTA over its prior cost.
    pub fn overCap(self: *const Pool, prior_cost: u64, padded: u64) bool {
        return self.current.load(.monotonic) - prior_cost + padded > self.max_write_bytes;
    }

    pub fn nextGeneration(self: *Pool) u64 {
        return self.next_generation.fetchAdd(1, .monotonic);
    }

    pub fn generationOf(self: *Pool, file_id: FileId) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const row = self.ledger.get(file_id) orelse return null;
        return row.generation;
    }

    fn costOf(self: *Pool, file_id: FileId) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const row = self.ledger.get(file_id) orelse return null;
        return row.cost;
    }

    /// Charge `cost` to `file_id` (replacing a prior row) at `generation`.
    pub fn charge(self: *Pool, file_id: FileId, cost: u64, generation: u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.ledger.getOrPut(self.gpa, file_id);
        const prior: u64 = if (gop.found_existing) gop.value_ptr.cost else 0;
        gop.value_ptr.* = .{ .cost = cost, .generation = generation };
        _ = self.current.fetchSub(prior, .monotonic);
        _ = self.current.fetchAdd(cost, .monotonic);
        stats.current_staged_write_bytes.sub(prior);
        stats.current_staged_write_bytes.add(cost);
    }

    /// Credit `file_id`'s row (promotion released it, unlink, a stale
    /// re-stage). Generation-gated when given. Answers the credited cost.
    pub fn credit(self: *Pool, file_id: FileId, expect_generation: ?u64) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const row = self.ledger.get(file_id) orelse return null;
        if (expect_generation) |g| if (row.generation != g) return null;
        _ = self.ledger.remove(file_id);
        _ = self.current.fetchSub(row.cost, .monotonic);
        stats.current_staged_write_bytes.sub(row.cost);
        return row.cost;
    }

    /// The R5 component: sheddable (the never-lossy mechanism is the
    /// promotion kick), kernel-reclaimable (the bytes are page cache over
    /// the mapping), weight = its share of the Red shed.
    pub fn component(self: *Pool) budget.Component {
        return budget.Component.init("staging_mmap", 0, 1, .{ .gauge = &stats.current_staged_write_bytes }, .{ .ctx = self, .call = shedToTarget }).kernelReclaimable();
    }

    fn shedToTarget(ctx: *anyopaque, _: u64) void {
        const self: *Pool = @ptrCast(@alignCast(ctx));
        if (self.hooks.pressure) |p| p(self.hooks.ctx, StagingKey.init(layout.zero_file_id, 0));
    }

    /// Seed the ledger from a recovered segment at generation 0.
    pub fn seedFromRecovery(self: *Pool, seg: *Segment) Allocator.Error!void {
        const out = try self.gpa.alloc(Resident, seg.live());
        defer self.gpa.free(out);
        for (seg.residents(out)) |r| try self.charge(r.key.file_id, r.cost, RECOVERED_GENERATION);
    }
};

/// §3.4 Admission: the cost is the record's ring footprint; over the cap
/// ⇒ kick the promoter and refuse `StorageFull` (the caller's spill
/// escalation — never a byte recorded that the pool cannot hold); place
/// (two-phase, never destroying a live entry); ledger row `(cost,
/// generation)`; past the high-water enqueue the entry to the merge worker.
pub fn stageWrite(pool: *Pool, seg: *Segment, key: StagingKey, payload: []const u8, fencing: u64) StageError!Staged {
    const padded = stride(KEY_LEN, payload.len);
    const prior = pool.costOf(key.file_id) orelse 0;
    if (pool.overCap(prior, padded)) {
        stats.staging_over_cap_refusals.inc();
        if (pool.hooks.pressure) |p| p(pool.hooks.ctx, key);
        return error.StorageFull;
    }
    const generation = pool.nextGeneration();
    seg.put(key, payload, generation, fencing) catch |e| switch (e) {
        error.RingFull => {
            log.err("staging ring full: a {d} B record found no hole in a {d} B segment with {d} live entries — refusing StorageFull (never destroying a live entry)", .{ padded, seg.capacity(), seg.live() });
            return error.StorageFull;
        },
        error.OutOfMemory => return error.OutOfMemory,
    };
    try pool.charge(key.file_id, padded, generation);
    var kicked = false;
    if (pool.currentBytes() > pool.highWater()) {
        stats.staging_pressure_kicks.inc();
        if (pool.hooks.pressure) |p| {
            p(pool.hooks.ctx, key);
            kicked = true;
        }
    }
    return .{ .generation = generation, .kicked = kicked };
}

/// The promotion released the ring entry (generation-gated: a re-stage that
/// landed since keeps its copy and its charge). True iff released.
pub fn releaseStaged(pool: *Pool, seg: *Segment, key: StagingKey, generation: u64) bool {
    if (!seg.remove(key, generation)) return false;
    _ = pool.credit(key.file_id, generation);
    return true;
}

// ---------------------------------------------------------------------------
// §3.5 The promotion points.
// ---------------------------------------------------------------------------

/// The clean-unmount pass: every resident entry through the promote hook
/// (failures leave the entry resident — never-wrong), then the caller seals
/// the open packs before "Dismount clean".
pub fn promoteAllAtDismount(gpa: Allocator, seg: *Segment, hooks: PromotionHooks) Allocator.Error!void {
    const promote = hooks.promote orelse return;
    const out = try gpa.alloc(Resident, seg.live());
    defer gpa.free(out);
    for (seg.residents(out)) |r| {
        if (promote(hooks.ctx, r.key)) {
            stats.dismount_promoted_files.inc();
            stats.dismount_promoted_bytes.add(r.cost);
        } else {
            stats.dismount_promote_failures.inc();
        }
    }
}

/// `SQUEEZEFS_FSYNC_PROMOTE_STAGED`: fsync of a staged file also promotes it.
pub const FsyncLever = struct {
    on: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map) FsyncLever {
        return .{ .on = knobs.boolKnob("SQUEEZEFS_FSYNC_PROMOTE_STAGED", env) };
    }
};

/// The fsync point: iff the lever is on, promote BEFORE the data barrier
/// (so the barrier covers the block). A failure never fails the fsync.
pub fn promoteAtFsync(lever: FsyncLever, hooks: PromotionHooks, key: StagingKey) void {
    if (!lever.on) return;
    const promote = hooks.promote orelse return;
    if (promote(hooks.ctx, key)) stats.fsync_promoted_files.inc();
}

// ---------------------------------------------------------------------------
// §3.7 The reader's view of an unpromoted entry.
// ---------------------------------------------------------------------------

pub const StagedRead = union(enum) {
    /// This client's ring has the bytes.
    resident: Segment.Lease,
    /// Budget-counted but not resident: a stage or promotion is IN FLIGHT;
    /// the caller backs off (bounded) and re-resolves.
    in_flight,
    /// STABLE lost identity (ring gone, no durable copy, nothing in
    /// flight): serve this many ZEROS — `min(offset + size, meta_size) −
    /// offset` — counted and warned.
    lost: u64,
};

/// The zeros a lost staged payload reads as: size-consistent, never past
/// the file.
pub fn lostStagedRangeLen(offset: u64, size: u64, meta_size: u64) u64 {
    return @min(offset +| size, meta_size) -| offset;
}

/// Resolve a staged read on THIS client (the durable-copy leg is the
/// caller's through `layout.Mapping.readWindow`).
pub fn resolveStagedRead(pool: *Pool, seg: *Segment, key: StagingKey, offset: u64, size: u64, meta_size: u64) StagedRead {
    if (seg.get(key)) |lease| return .{ .resident = lease };
    if (pool.generationOf(key.file_id) != null) return .in_flight;
    const zeros = lostStagedRangeLen(offset, size, meta_size);
    stats.staged_payload_lost_reads.inc();
    log.warn("staged payload unreachable for ino {d}: serving {d} zero bytes at offset {d} (another client's live staged custody, or a crash discarded it)", .{ key.ino(), zeros, offset });
    return .{ .lost = zeros };
}

// ---------------------------------------------------------------------------
// §3.1 Identity: the binary marker.
// ---------------------------------------------------------------------------

pub const MARKER_MAGIC = "SQZM".*;
pub const MARKER_VERSION: u16 = 1;
/// The marker names the previous set epoch too (an `add-meta` rebind in
/// flight).
pub const MARKER_FLAG_DUAL: u16 = 1 << 0;
const MARKER_FLAGS_KNOWN: u16 = MARKER_FLAG_DUAL;

/// 72 B, checksummed; compared as BYTES.
pub const StagingMarkerWire = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    flags_le: [2]u8,
    set_uuid: [16]u8,
    set_epoch_le: [8]u8,
    node_token_le: [8]u8,
    mount_slot_le: [4]u8,
    content_version_le: [4]u8,
    identity_le: [8]u8,
    prev_set_epoch_le: [8]u8,
    xxh3_le: [8]u8,
};

pub const MARKER_LEN: usize = @sizeOf(StagingMarkerWire);
const MARKER_CHECKSUM_OFF: usize = @offsetOf(StagingMarkerWire, "xxh3_le");

comptime {
    assert(MARKER_LEN == 72);
    assert(MARKER_CHECKSUM_OFF == 64);
}

/// The ring's identity: `xxh3(set_uuid ‖ "staging")`. WHY: a ring binds to
/// ONE volume set for its life, and the set uuid is the one durable identity
/// every member's superblock carries (so a reformat — a new uuid — or a
/// different set never adopts a stale ring); the domain-separating suffix
/// keeps the value distinct from every other derivation of the same uuid
/// (`hash_seed` once was `xxh3(uuid)`, 0002 row 42), so no two identities
/// can be forged from one another; xxh3-64 is the format's ONE checksum.
/// Printed as 16 hex digits in logs and directory names; compared as a u64.
pub fn stagingIdentity(set_uuid: [16]u8) u64 {
    var h = std.hash.XxHash3.init(0);
    h.update(&set_uuid);
    h.update("staging");
    return h.final();
}

pub const MarkerError = error{Corrupt};

pub const Marker = struct {
    set_uuid: [16]u8,
    set_epoch: u64,
    node_token: u64,
    mount_slot: u32,
    content_version: u32 = 1,
    /// Present iff dual: the epoch this ring was bound to before a rebind.
    prev_set_epoch: ?u64 = null,

    pub fn identity(self: Marker) u64 {
        return stagingIdentity(self.set_uuid);
    }

    pub fn encode(self: Marker) [MARKER_LEN]u8 {
        var w: StagingMarkerWire = .{
            .magic = MARKER_MAGIC,
            .version_le = undefined,
            .flags_le = undefined,
            .set_uuid = self.set_uuid,
            .set_epoch_le = undefined,
            .node_token_le = undefined,
            .mount_slot_le = undefined,
            .content_version_le = undefined,
            .identity_le = undefined,
            .prev_set_epoch_le = undefined,
            .xxh3_le = @splat(0),
        };
        std.mem.writeInt(u16, &w.version_le, MARKER_VERSION, .little);
        std.mem.writeInt(u16, &w.flags_le, if (self.prev_set_epoch != null) MARKER_FLAG_DUAL else 0, .little);
        std.mem.writeInt(u64, &w.set_epoch_le, self.set_epoch, .little);
        std.mem.writeInt(u64, &w.node_token_le, self.node_token, .little);
        std.mem.writeInt(u32, &w.mount_slot_le, self.mount_slot, .little);
        std.mem.writeInt(u32, &w.content_version_le, self.content_version, .little);
        std.mem.writeInt(u64, &w.identity_le, self.identity(), .little);
        std.mem.writeInt(u64, &w.prev_set_epoch_le, self.prev_set_epoch orelse 0, .little);
        var bytes = std.mem.toBytes(w);
        checksum.stamp(&bytes, MARKER_CHECKSUM_OFF);
        return bytes;
    }

    /// Total: exactly 72 B, magic, version, known flags, the checksum, and
    /// an identity that IS the derivation of the uuid it carries (a marker
    /// whose identity disagrees with its own uuid was not written by us).
    pub fn decode(bytes: []const u8) MarkerError!Marker {
        if (bytes.len != MARKER_LEN) return error.Corrupt;
        if (!checksum.verify(bytes, MARKER_CHECKSUM_OFF)) return error.Corrupt;
        const w = std.mem.bytesToValue(StagingMarkerWire, bytes[0..MARKER_LEN]);
        if (!std.mem.eql(u8, &w.magic, &MARKER_MAGIC)) return error.Corrupt;
        if (std.mem.readInt(u16, &w.version_le, .little) != MARKER_VERSION) return error.Corrupt;
        const flags = std.mem.readInt(u16, &w.flags_le, .little);
        if (flags & ~MARKER_FLAGS_KNOWN != 0) return error.Corrupt;
        const m: Marker = .{
            .set_uuid = w.set_uuid,
            .set_epoch = std.mem.readInt(u64, &w.set_epoch_le, .little),
            .node_token = std.mem.readInt(u64, &w.node_token_le, .little),
            .mount_slot = std.mem.readInt(u32, &w.mount_slot_le, .little),
            .content_version = std.mem.readInt(u32, &w.content_version_le, .little),
            .prev_set_epoch = if (flags & MARKER_FLAG_DUAL != 0) std.mem.readInt(u64, &w.prev_set_epoch_le, .little) else null,
        };
        if (std.mem.readInt(u64, &w.identity_le, .little) != m.identity()) return error.Corrupt;
        if (flags & MARKER_FLAG_DUAL == 0 and std.mem.readInt(u64, &w.prev_set_epoch_le, .little) != 0) return error.Corrupt;
        return m;
    }
};

/// What a bind found.
pub const BindVerdict = enum {
    /// No marker: a fresh dir — stamp ours.
    absent,
    /// Our set, our epoch (or a dual marker naming it), our scope: keep.
    match,
    /// Our set and scope, an unscoped or older-epoch marker: adopt and
    /// restamp (the upgrade path).
    upgrade,
    /// Another mount's live scope on the same set: REFUSE the mount loud.
    foreign_scope,
    /// Another set, or an unreadable marker: wipe both segment trees and
    /// restamp.
    dead,
};

/// §3.1 `bind_staging_generation`'s ladder as a pure verdict.
pub fn bindVerdict(found: ?Marker, ours: Marker, peer_live: bool) BindVerdict {
    const f = found orelse return .absent;
    if (f.identity() != ours.identity()) return .dead;
    const same_epoch = f.set_epoch == ours.set_epoch or (f.prev_set_epoch != null and f.prev_set_epoch.? == ours.set_epoch);
    if (!same_epoch and f.set_epoch > ours.set_epoch) return .dead;
    const same_scope = f.node_token == ours.node_token and f.mount_slot == ours.mount_slot;
    if (!same_scope and f.node_token != 0 and peer_live) return .foreign_scope;
    if (same_epoch and same_scope and f.prev_set_epoch == null) return .match;
    return .upgrade;
}

/// Shards per device: `max(next_pow2(cores), 16)`, halved while
/// `capacity / shards < 2 × max_entry + SHARD_REPLACE_SLACK` (a same-key
/// crash-safe REPLACE holds two copies — FIND-VS-B); a pool under 10 MiB has
/// no lock parallelism worth having and takes one shard.
pub const SHARD_REPLACE_SLACK: u64 = 64 * derive.KiB;
pub const TINY_POOL: u64 = 10 * derive.MiB;

pub fn shardPlan(per_device_capacity: u64, cores: u32, max_entry: u64) u32 {
    if (per_device_capacity < TINY_POOL) return 1;
    var shards: u64 = @max(derive.nextPow2(cores), 16);
    while (shards > 1 and per_device_capacity / shards < 2 * max_entry + SHARD_REPLACE_SLACK) shards /= 2;
    return @intCast(shards);
}

/// `xxh3(key) % shards`.
pub fn shardOf(key: StagingKey, shards: u32) u32 {
    return @intCast(checksum.xxh3(key.bytes()) % shards);
}

// ---------------------------------------------------------------------------
// The generation ABA MODEL (charter §9): a promotion's release is gated on
// the generation it prepared against; a re-stage mints a fresh one, so a
// stale release can never free a newer entry's sole copy. Weakening:
// recycling generations (the re-stage reuses the released number) lets the
// stale release match.
// ---------------------------------------------------------------------------

pub const GenerationAbaModel = struct {
    pub const thread_count: usize = 2;
    pub const Weakening = enum { none, recycled_generation };
    pub var weakening: Weakening = .none;

    /// 0 = absent; else the resident copy's generation.
    resident: u64 = 1,
    next_gen: u64 = 2,
    /// Thread 0 (the promoter) prepared against generation 1.
    prepared: u64 = 1,
    /// Thread 1 (the writer) re-stages: a new copy at a fresh generation.
    restaged_gen: u64 = 0,
    pc: [2]u8 = @splat(0),
    lost_sole_copy: bool = false,

    pub fn init() GenerationAbaModel {
        return .{};
    }

    pub fn done(self: *const GenerationAbaModel, t: usize) bool {
        return self.pc[t] == 1;
    }

    pub fn step(self: *GenerationAbaModel, t: usize) void {
        switch (t) {
            // The promoter's generation-gated release (one CAS-shaped action).
            0 => {
                if (self.resident == self.prepared) {
                    // Released a copy the writer re-staged: its bytes are gone.
                    if (self.restaged_gen != 0 and self.resident == self.restaged_gen) self.lost_sole_copy = true;
                    self.resident = 0;
                }
            },
            // The writer re-stages under the same key.
            1 => {
                const g = switch (weakening) {
                    .none => self.next_gen,
                    .recycled_generation => self.prepared,
                };
                self.restaged_gen = g;
                self.resident = g;
            },
            else => unreachable,
        }
        self.pc[t] += 1;
    }

    pub fn invariant(self: *const GenerationAbaModel) !void {
        if (self.lost_sole_copy) return error.StaleReleaseFreedTheSoleCopy;
    }

    pub fn final(self: *const GenerationAbaModel) !void {
        // The re-staged copy must survive whatever order the two ran in.
        if (self.restaged_gen != 0 and self.resident != self.restaged_gen) return error.ReStagedCopyLost;
    }
};

// ---------------------------------------------------------------------------
// Linux binding: directories, mmap segments, markers.
// ---------------------------------------------------------------------------

pub const Ring = if (builtin.os.tag == .linux) struct {
    const uring_fs = @import("../io/uring_fs.zig");

    /// Plaintext user payload on passthrough volumes and every staged key
    /// in the headers: private to the daemon's uid.
    pub const DIR_MODE: linux.mode_t = 0o700;
    pub const FILE_MODE: linux.mode_t = 0o600;
    pub const SEGMENT_DIR = "staging_segment";
    pub const MARKER_NAME = ".squeezefs_staging";
    pub const MARKER_TMP = ".squeezefs_staging.tmp";

    pub const Error = error{ OpenFailed, MapFailed, GrowFailed, NameTooLong };

    pub const MappedSegment = struct {
        fd: linux.fd_t,
        map: []align(4096) u8,

        fn sync(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            _ = linux.msync(self.map.ptr + @as(usize, @intCast(off)), @intCast(len), linux.MSF.ASYNC);
        }

        /// Punch the hole (the host reclaims the blocks) and drop the pages
        /// (the RSS the dead extent held); neither is correctness — a
        /// tombstoned record is never adopted — so their errnos are ignored.
        fn reclaim(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            _ = linux.fallocate(self.fd, linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE, @intCast(off), @intCast(len));
            _ = linux.madvise(self.map.ptr + @as(usize, @intCast(off)), @intCast(len), linux.MADV.DONTNEED);
        }

        pub fn backing(self: *MappedSegment) Backing {
            return .{ .ctx = self, .sync = sync, .reclaim = reclaim };
        }

        pub fn close(self: *MappedSegment) void {
            _ = linux.munmap(self.map.ptr, self.map.len);
            _ = linux.close(self.fd);
            self.* = undefined;
        }
    };

    fn zPath(buf: *[linux.PATH_MAX]u8, path: []const u8) Error![:0]const u8 {
        if (path.len >= buf.len) return error.NameTooLong;
        @memcpy(buf[0..path.len], path);
        buf[path.len] = 0;
        return buf[0..path.len :0];
    }

    /// Create (0700) or open the root and its `staging_segment/` tree;
    /// re-assert the mode on every open (a pre-existing dir keeps the law).
    pub fn openRoot(path: []const u8) Error!linux.fd_t {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = try zPath(&zbuf, path);
        _ = linux.mkdir(z, DIR_MODE);
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        const fd: linux.fd_t = @intCast(rc);
        _ = linux.fchmod(fd, DIR_MODE);
        _ = linux.mkdirat(fd, SEGMENT_DIR, DIR_MODE);
        const sub = linux.openat(fd, SEGMENT_DIR, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(sub) == .SUCCESS) {
            _ = linux.fchmod(@intCast(sub), DIR_MODE);
            _ = linux.close(@intCast(sub));
        }
        return fd;
    }

    /// Open `staging_segment/<name>` (0600, re-asserted), grow it to
    /// `capacity` — NEVER shrink (a mapper past the new end would SIGBUS,
    /// FIND-VS-A) — and map it shared read/write.
    pub fn openSegment(root_fd: linux.fd_t, name: [:0]const u8, capacity: u64) Error!MappedSegment {
        assert(derive.isAligned(capacity, RECORD_ALIGN));
        const dir = linux.openat(root_fd, SEGMENT_DIR, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(dir) != .SUCCESS) return error.OpenFailed;
        defer _ = linux.close(@intCast(dir));
        const rc = linux.openat(@intCast(dir), name, .{ .ACCMODE = .RDWR, .CREAT = true, .NOFOLLOW = true, .CLOEXEC = true }, FILE_MODE);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        _ = linux.fchmod(fd, FILE_MODE);
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.OpenFailed;
        const have: u64 = st.size;
        const len = @max(have, capacity);
        if (have < capacity and linux.errno(linux.ftruncate(fd, @intCast(capacity))) != .SUCCESS) return error.GrowFailed;
        const m = linux.mmap(null, @intCast(len), .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(m) != .SUCCESS) return error.MapFailed;
        const ptr: [*]align(4096) u8 = @ptrFromInt(m);
        return .{ .fd = fd, .map = ptr[0..@intCast(len)] };
    }

    /// Two-phase marker write: the bytes to a temp file + `fdatasync`, a
    /// `rename` over the marker, `fsync` of the directory — a crash leaves
    /// either the old marker or the new one, never a torn one (the decode's
    /// checksum is the second line).
    pub fn writeMarker(pool: *uring_fs.Pool, root_path: []const u8, root_fd: linux.fd_t, m: Marker) !void {
        var tmp_buf: [linux.PATH_MAX]u8 = undefined;
        const tmp = try std.fmt.bufPrint(&tmp_buf, "{s}/{s}", .{ root_path, MARKER_TMP });
        const bytes = m.encode();
        try pool.writeAll(tmp, &bytes);
        try pool.fdatasync(tmp);
        if (linux.errno(linux.renameat(root_fd, MARKER_TMP, root_fd, MARKER_NAME)) != .SUCCESS) return error.OpenFailed;
        _ = linux.fsync(root_fd);
        _ = linux.fchmodat(root_fd, MARKER_NAME, FILE_MODE);
    }

    /// The marker, null when absent or unreadable (the bind treats an
    /// unreadable marker as `dead`).
    pub fn readMarker(pool: *uring_fs.Pool, root_path: []const u8) !?Marker {
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = try std.fmt.bufPrint(&buf, "{s}/{s}", .{ root_path, MARKER_NAME });
        var r = pool.readAll(p) catch |e| switch (e) {
            error.FileNotFound => return null,
            else => return e,
        };
        defer r.deinit();
        return Marker.decode(r.bytes()) catch null;
    }

    /// `dead`: wipe the segment tree (every file under it) before restamping.
    pub fn wipeSegments(root_fd: linux.fd_t, names: []const [:0]const u8) void {
        const dir = linux.openat(root_fd, SEGMENT_DIR, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(dir) != .SUCCESS) return;
        defer _ = linux.close(@intCast(dir));
        for (names) |n| _ = linux.unlinkat(@intCast(dir), n, 0);
        stats.staging_generation_discards.inc();
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn tkey(seed: u8, ino: u64) StagingKey {
    var f: FileId = undefined;
    for (&f, 0..) |*b, i| b.* = seed +% @as(u8, @intCast(i * 3));
    return StagingKey.init(f, ino);
}

fn ramSegment(gpa: Allocator, cap: usize) !struct { image: []u8, seg: Segment } {
    const image = try gpa.alloc(u8, cap);
    @memset(image, 0);
    return .{ .image = image, .seg = try Segment.init(gpa, image, .none) };
}

test "segment record: 40 B header {SQZS, version, flags 0, key_len, val_len, generation, fencing, xxh3} ‖ key ‖ pad to 4 KiB ‖ value; the checksum covers header+key+value; a flipped payload byte is torn" {
    try testing.expectEqual(@as(u64, 4096), valueOffset(KEY_LEN));
    try testing.expectEqual(@as(u64, 4096 + 100), footprint(KEY_LEN, 100));
    try testing.expectEqual(@as(u64, 8192), stride(KEY_LEN, 100));
    try testing.expectEqual(@as(u64, 8192), stride(KEY_LEN, 4096));
    try testing.expectEqual(@as(u64, 12288), stride(KEY_LEN, 4097));
    var image: [16384]u8 = @splat(0xEE);
    const k = tkey(1, 77);
    const v: [100]u8 = @splat(0x5A);
    writeRecord(&image, 4096, k.bytes(), &v, 9, 0xF0F0);
    try testing.expectEqualSlices(u8, "SQZS", image[4096..4100]);
    try testing.expectEqualSlices(u8, &.{ 1, 0, 0, 0 }, image[4100..4104]);
    try testing.expectEqualSlices(u8, &.{ 24, 0, 0, 0, 100, 0, 0, 0 }, image[4104..4112]);
    try testing.expectEqualSlices(u8, &.{ 9, 0, 0, 0, 0, 0, 0, 0 }, image[4112..4120]);
    try testing.expectEqualSlices(u8, &.{ 0xF0, 0xF0, 0, 0, 0, 0, 0, 0 }, image[4120..4128]);
    try testing.expectEqualSlices(u8, k.bytes(), image[4136..4160]);
    try testing.expect(std.mem.allEqual(u8, image[4160..8192], 0));
    try testing.expectEqualSlices(u8, &v, image[8192..8292]);
    const h = readHeader(&image, 4096).?;
    try testing.expectEqual(@as(u32, 100), h.val_len);
    try testing.expectEqual(@as(u64, 9), h.generation);
    try testing.expect(verifyRecord(&image, 4096, h));
    image[8200] ^= 1;
    try testing.expect(!verifyRecord(&image, 4096, h));
    image[8200] ^= 1;
    image[4140] ^= 1;
    try testing.expect(!verifyRecord(&image, 4096, h));
    image[4140] ^= 1;
    try testing.expect(verifyRecord(&image, 4096, h));
    // A header whose footprint overruns the image is no header; wrong version / flags neither.
    std.mem.writeInt(u32, image[4108..4112], 1 << 20, .little);
    try testing.expect(readHeader(&image, 4096) == null);
    std.mem.writeInt(u32, image[4108..4112], 100, .little);
    image[4102] = 1;
    try testing.expect(readHeader(&image, 4096) == null);
    image[4102] = 0;
    try testing.expect(readHeader(&image, 4096) != null);
    try testing.expect(readHeader(&image, 0) == null);
    try testing.expect(!isTombstone(&image, 4096));
    @memset(image[4096..4100], 0);
    try testing.expect(isTombstone(&image, 4096));
    try testing.expect(readHeader(&image, 4096) == null);
}

test "two-phase replace: the old copy stays live until the replacement is placed; a reader's lease across a replace sees intact old bytes; the last reference reclaims; a remove is generation-gated" {
    const gpa = testing.allocator;
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const k = tkey(7, 42);
    const a: [5000]u8 = @splat('a');
    const b: [3000]u8 = @splat('b');
    try r.seg.put(k, &a, 1, 0);
    try testing.expectEqual(@as(usize, 1), r.seg.live());
    try testing.expectEqual(@as(?u64, 1), r.seg.residentGeneration(k));
    const lease = r.seg.get(k).?;
    try testing.expectEqualSlices(u8, &a, lease.bytes());
    // Replace while the lease is held: the index flips, the old bytes stay.
    try r.seg.put(k, &b, 2, 0);
    try testing.expectEqualSlices(u8, &a, lease.bytes());
    try testing.expect(lease.cell.retired.load(.acquire));
    try testing.expectEqual(@as(?u64, 2), r.seg.residentGeneration(k));
    const fresh = r.seg.get(k).?;
    try testing.expectEqualSlices(u8, &b, fresh.bytes());
    fresh.release();
    // The two copies never overlapped.
    var ext: [8]Extent = undefined;
    const old_ext = lease.cell.extent;
    const live_ext = r.seg.liveExtents(&ext);
    try testing.expectEqual(@as(usize, 1), live_ext.len);
    try testing.expect(old_ext.end() <= live_ext[0].rec_off or live_ext[0].end() <= old_ext.rec_off);
    // The old record is tombstoned on retire; its cell returns to the pool on the last release.
    try testing.expect(isTombstone(r.image, old_ext.rec_off));
    const free_before = r.seg.free_cells.items.len;
    lease.release();
    try testing.expectEqual(free_before + 1, r.seg.free_cells.items.len);
    // Generation-gated remove: the wrong generation keeps the entry.
    try testing.expect(!r.seg.remove(k, 1));
    try testing.expectEqual(@as(usize, 1), r.seg.live());
    try testing.expect(r.seg.remove(k, 2));
    try testing.expectEqual(@as(usize, 0), r.seg.live());
    try testing.expect(r.seg.get(k) == null);
    try testing.expect(!r.seg.remove(k, null));
}

test "placement never overlaps and reuses punched holes first-fit; a record that fits nowhere is a loud RingFull that destroys nothing" {
    const gpa = testing.allocator;
    var r = try ramSegment(gpa, 32 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    var prng = std.Random.DefaultPrng.init(42);
    const rnd = prng.random();
    var sizes: [16]usize = undefined;
    for (&sizes) |*s| s.* = rnd.intRangeAtMost(usize, 1, 3 * 4096);
    var placed: usize = 0;
    for (sizes, 0..) |s, i| {
        const v = try gpa.alloc(u8, s);
        defer gpa.free(v);
        @memset(v, @intCast(i));
        r.seg.put(tkey(@intCast(i), i), v, i + 1, 0) catch |e| {
            try testing.expectEqual(error.RingFull, e);
            break;
        };
        placed += 1;
    }
    try testing.expect(placed >= 8);
    var ext: [32]Extent = undefined;
    const live_ext = r.seg.liveExtents(&ext);
    std.mem.sort(Extent, live_ext, {}, Segment.extentLess);
    for (live_ext[1..], 0..) |e, i| try testing.expect(live_ext[i].end() <= e.rec_off);
    // Every placed value reads back intact.
    for (0..placed) |i| {
        const l = r.seg.get(tkey(@intCast(i), i)).?;
        defer l.release();
        try testing.expectEqual(sizes[i], l.bytes().len);
        try testing.expect(std.mem.allEqual(u8, l.bytes(), @intCast(i)));
    }
    // A record larger than the ring is refused before any placement.
    const huge = try gpa.alloc(u8, 40 * 4096);
    defer gpa.free(huge);
    try testing.expectError(error.RingFull, r.seg.put(tkey(1, 1), huge, 1, 0));

    // A full ring of 2-grain records: the 9th is a loud refusal that
    // destroys nothing; removing the 4th opens the ONE hole, and the next
    // put lands exactly there (first fit over the wrapped candidates).
    var f = try ramSegment(gpa, 16 * 4096);
    defer gpa.free(f.image);
    defer f.seg.deinit();
    const ten: [10]u8 = @splat(2);
    for (0..8) |i| try f.seg.put(tkey(@intCast(i), i), &ten, i + 1, 0);
    const before = stats.staging_ring_full_refusals.load();
    try testing.expectError(error.RingFull, f.seg.put(tkey(99, 99), &ten, 99, 0));
    try testing.expectEqual(before + 1, stats.staging_ring_full_refusals.load());
    try testing.expectEqual(@as(usize, 8), f.seg.live());
    const fourth = f.seg.get(tkey(3, 3)).?;
    const hole = fourth.cell.extent.rec_off;
    fourth.release();
    try testing.expectEqual(@as(u64, 3 * 8192), hole);
    try testing.expect(f.seg.remove(tkey(3, 3), null));
    try f.seg.put(tkey(50, 50), &ten, 50, 0);
    const l = f.seg.get(tkey(50, 50)).?;
    defer l.release();
    try testing.expectEqual(hole, l.cell.extent.rec_off);
    try testing.expectEqualSlices(u8, &ten, l.bytes());
}

test "recovery scan: the true geometry survives (every live record adopted, bytes intact, write_offset = max end); false magic inside a value interior is never a header; a torn header or payload is dropped loud and the scan resyncs; two copies of one key keep the higher generation" {
    const gpa = testing.allocator;
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    var seg = r.seg;
    // A value that CONTAINS a plausible record header at a grain boundary.
    var decoy: [3 * 4096]u8 = @splat(0x11);
    writeRecord(decoy[4096..], 0, tkey(0xAA, 1).bytes(), "decoy", 777, 0);
    try seg.put(tkey(1, 10), &decoy, 1, 0);
    const v2: [100]u8 = @splat(2);
    try seg.put(tkey(2, 20), &v2, 2, 0);
    const v3: [5000]u8 = @splat(3);
    try seg.put(tkey(3, 30), &v3, 3, 0);
    // A same-key replace with a crash between phases: both copies present.
    const v2b: [200]u8 = @splat(4);
    const old2 = seg.get(tkey(2, 20)).?;
    const off2 = old2.cell.extent.rec_off;
    old2.release();
    try seg.put(tkey(2, 20), &v2b, 5, 0);
    // Resurrect the old copy's magic (as if phase 2 never ran).
    @memcpy(r.image[@intCast(off2)..][0..4], &SEGMENT_MAGIC);
    var ext: [8]Extent = undefined;
    const want_live = seg.liveExtents(&ext).len;
    seg.deinit();
    // A torn record: a header with a bad checksum in a free grain.
    var fresh = try Segment.init(gpa, r.image, .none);
    defer fresh.deinit();
    const torn_off: u64 = 60 * 4096;
    writeRecord(r.image, torn_off, tkey(9, 90).bytes(), "torn", 9, 0);
    r.image[@intCast(torn_off + 4096)] ^= 0xFF;
    const before_torn = stats.staging_recovery_torn_records.load();
    const rep = fresh.recover();
    try testing.expectEqual(@as(u64, 3), rep.adopted);
    try testing.expectEqual(@as(u64, 1), rep.superseded);
    try testing.expectEqual(@as(u64, 1), rep.torn);
    try testing.expectEqual(before_torn + 1, stats.staging_recovery_torn_records.load());
    try testing.expectEqual(want_live, fresh.live());
    const l1 = fresh.get(tkey(1, 10)).?;
    defer l1.release();
    try testing.expectEqualSlices(u8, &decoy, l1.bytes());
    try testing.expect(fresh.get(tkey(0xAA, 1)) == null);
    const l2 = fresh.get(tkey(2, 20)).?;
    defer l2.release();
    try testing.expectEqualSlices(u8, &v2b, l2.bytes());
    try testing.expectEqual(@as(u64, 5), l2.generation());
    try testing.expect(fresh.get(tkey(9, 90)) == null);
    // Post-recovery writes land past every adopted record and never overlap.
    const v6: [10]u8 = @splat(6);
    try fresh.put(tkey(6, 60), &v6, 6, 0);
    const live_ext = fresh.liveExtents(&ext);
    std.mem.sort(Extent, live_ext, {}, Segment.extentLess);
    for (live_ext[1..], 0..) |e, i| try testing.expect(live_ext[i].end() <= e.rec_off);
    // A cleanly removed entry is a tombstone: a re-scan does not resurrect it.
    try testing.expect(fresh.remove(tkey(3, 30), null));
    var again = try Segment.init(gpa, r.image, .none);
    defer again.deinit();
    _ = again.recover();
    try testing.expect(again.get(tkey(3, 30)) == null);
    try testing.expectEqual(@as(usize, 3), again.live());
}

const Kicks = struct {
    pressure: u32 = 0,
    promoted: u32 = 0,
    fail_next: bool = false,

    fn hooks(self: *Kicks) PromotionHooks {
        return .{ .ctx = self, .pressure = onPressure, .promote = onPromote };
    }

    fn onPressure(ctx: ?*anyopaque, _: StagingKey) void {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        self.pressure += 1;
    }

    fn onPromote(ctx: ?*anyopaque, _: StagingKey) bool {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        if (self.fail_next) {
            self.fail_next = false;
            return false;
        }
        self.promoted += 1;
        return true;
    }
};

test "pool: the ledger charges the delta on a re-stage and credits on release so the gauge never ratchets; over the cap refuses StorageFull after a kick; the 75 % high-water (max − max/4) enqueues a promotion; generations start at 1 and recovery seeds 0" {
    const gpa = testing.allocator;
    var kicks: Kicks = .{};
    var pool = Pool.init(gpa, 16 * 4096, kicks.hooks());
    defer pool.deinit();
    try testing.expectEqual(@as(u64, 12 * 4096), pool.highWater());
    try testing.expectEqual(FIRST_GENERATION, pool.next_generation.load(.monotonic));
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const k = tkey(1, 1);
    const g0 = stats.current_staged_write_bytes.load();
    const v: [100]u8 = @splat(1);
    const s1 = try stageWrite(&pool, &r.seg, k, &v, 0);
    try testing.expectEqual(@as(u64, 1), s1.generation);
    try testing.expect(!s1.kicked);
    try testing.expectEqual(@as(u64, 8192), pool.currentBytes());
    try testing.expectEqual(g0 + 8192, stats.current_staged_write_bytes.load());
    // Re-stage the same file_id larger: the delta is charged, one row.
    const v2: [5000]u8 = @splat(2);
    const s2 = try stageWrite(&pool, &r.seg, k, &v2, 0);
    try testing.expectEqual(@as(u64, 2), s2.generation);
    try testing.expectEqual(@as(u64, 12288), pool.currentBytes());
    try testing.expectEqual(@as(?u64, 2), pool.generationOf(k.file_id));
    // Past the high-water: kicked.
    const big: [9 * 4096 - 10]u8 = @splat(3);
    const s3 = try stageWrite(&pool, &r.seg, tkey(2, 2), &big, 0);
    try testing.expect(pool.currentBytes() > pool.highWater());
    try testing.expect(s3.kicked);
    try testing.expectEqual(@as(u32, 1), kicks.pressure);
    // Over the cap: refused, kicked again, nothing placed, no charge.
    const cur = pool.currentBytes();
    const live_before = r.seg.live();
    const refusals = stats.staging_over_cap_refusals.load();
    try testing.expectError(error.StorageFull, stageWrite(&pool, &r.seg, tkey(3, 3), &big, 0));
    try testing.expectEqual(cur, pool.currentBytes());
    try testing.expectEqual(live_before, r.seg.live());
    try testing.expectEqual(@as(u32, 2), kicks.pressure);
    try testing.expectEqual(refusals + 1, stats.staging_over_cap_refusals.load());
    // A stale generation release is a no-op; the live one credits and removes.
    try testing.expect(!releaseStaged(&pool, &r.seg, k, 1));
    try testing.expectEqual(cur, pool.currentBytes());
    try testing.expect(releaseStaged(&pool, &r.seg, k, 2));
    try testing.expectEqual(cur - 12288, pool.currentBytes());
    try testing.expect(r.seg.get(k) == null);
    try testing.expectEqual(@as(?u64, null), pool.generationOf(k.file_id));
    // Recovery seeds the ledger at generation 0 with the exact footprints.
    var pool2 = Pool.init(gpa, 16 * 4096, .none);
    defer pool2.deinit();
    try pool2.seedFromRecovery(&r.seg);
    try testing.expectEqual(@as(?u64, RECOVERED_GENERATION), pool2.generationOf(tkey(2, 2).file_id));
    try testing.expectEqual(@as(u64, 10 * 4096), pool2.currentBytes());
    // The budget component is sheddable and kernel-reclaimable; its shed kicks the promoter.
    const c = pool.component();
    try testing.expectEqualStrings("staging_mmap", c.name);
    try testing.expect(c.kernel_reclaimable);
    try testing.expectEqual(@as(u64, 1), c.weight);
    c.shed.?.call(c.shed.?.ctx, 0);
    try testing.expectEqual(@as(u32, 3), kicks.pressure);
}

test "the three promotion points: pressure rides stageWrite; the dismount pass promotes every resident (failures stay resident, counted); fsync promotes only under the lever (default off), a failure never fails it" {
    const gpa = testing.allocator;
    var kicks: Kicks = .{};
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const v: [10]u8 = @splat(1);
    try r.seg.put(tkey(1, 1), &v, 1, 0);
    try r.seg.put(tkey(2, 2), &v, 2, 0);
    try r.seg.put(tkey(3, 3), &v, 3, 0);
    kicks.fail_next = true;
    const f0 = stats.dismount_promoted_files.load();
    const b0 = stats.dismount_promoted_bytes.load();
    const x0 = stats.dismount_promote_failures.load();
    try promoteAllAtDismount(gpa, &r.seg, kicks.hooks());
    try testing.expectEqual(@as(u32, 2), kicks.promoted);
    try testing.expectEqual(f0 + 2, stats.dismount_promoted_files.load());
    try testing.expectEqual(b0 + 2 * 8192, stats.dismount_promoted_bytes.load());
    try testing.expectEqual(x0 + 1, stats.dismount_promote_failures.load());
    try testing.expect(!knobs.boolDefault("SQUEEZEFS_FSYNC_PROMOTE_STAGED"));
    const p0 = stats.fsync_promoted_files.load();
    promoteAtFsync(.{ .on = false }, kicks.hooks(), tkey(1, 1));
    try testing.expectEqual(@as(u32, 2), kicks.promoted);
    kicks.fail_next = true;
    promoteAtFsync(.{ .on = true }, kicks.hooks(), tkey(1, 1));
    try testing.expectEqual(p0, stats.fsync_promoted_files.load());
    promoteAtFsync(.{ .on = true }, kicks.hooks(), tkey(1, 1));
    try testing.expectEqual(p0 + 1, stats.fsync_promoted_files.load());
    try testing.expectEqual(@as(u32, 3), kicks.promoted);
}

test "the reader's view: resident ⇒ the bytes; ledger-only ⇒ in flight; neither ⇒ size-consistent zeros, counted and warned" {
    const gpa = testing.allocator;
    var pool = Pool.init(gpa, 16 * 4096, .none);
    defer pool.deinit();
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const k = tkey(5, 55);
    const v: [300]u8 = @splat(9);
    _ = try stageWrite(&pool, &r.seg, k, &v, 0);
    switch (resolveStagedRead(&pool, &r.seg, k, 0, 100, 300)) {
        .resident => |l| {
            defer l.release();
            try testing.expectEqualSlices(u8, &v, l.bytes());
        },
        else => return error.TestExpectedResident,
    }
    // A promotion consumed the ring entry but the ledger row is still live: in flight.
    try testing.expect(r.seg.remove(k, null));
    try testing.expectEqual(StagedRead.in_flight, resolveStagedRead(&pool, &r.seg, k, 0, 100, 300));
    // Stable lost identity.
    _ = pool.credit(k.file_id, null);
    const lost0 = stats.staged_payload_lost_reads.load();
    const out = resolveStagedRead(&pool, &r.seg, k, 100, 4096, 300);
    try testing.expectEqual(StagedRead{ .lost = 200 }, out);
    try testing.expectEqual(lost0 + 1, stats.staged_payload_lost_reads.load());
    try testing.expectEqual(@as(u64, 0), lostStagedRangeLen(500, 10, 300));
    try testing.expectEqual(@as(u64, 10), lostStagedRangeLen(0, 10, 300));
    try testing.expectEqual(@as(u64, 300), lostStagedRangeLen(0, std.math.maxInt(u64), 300));
}

test "the marker: 72 B, checksummed, identity = xxh3(set_uuid ‖ \"staging\") and checked against its own uuid; every refusal; the bind ladder's verdicts" {
    const uuid: [16]u8 = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    const m: Marker = .{ .set_uuid = uuid, .set_epoch = 3, .node_token = 0xABC, .mount_slot = 2 };
    const bytes = m.encode();
    try testing.expectEqual(@as(usize, 72), bytes.len);
    try testing.expectEqualSlices(u8, "SQZM", bytes[0..4]);
    try testing.expectEqualSlices(u8, &uuid, bytes[8..24]);
    var want = std.hash.XxHash3.init(0);
    want.update(&uuid);
    want.update("staging");
    try testing.expectEqual(want.final(), stagingIdentity(uuid));
    try testing.expectEqual(stagingIdentity(uuid), std.mem.readInt(u64, bytes[48..56], .little));
    try testing.expect(stagingIdentity(uuid) != checksum.xxh3(&uuid));
    const back = try Marker.decode(&bytes);
    try testing.expectEqual(m, back);
    // Refusals: length, a flipped byte, a foreign identity for the carried uuid, an unknown flag, a non-dual prev epoch.
    try testing.expectError(error.Corrupt, Marker.decode(bytes[0..71]));
    var bad = bytes;
    bad[20] ^= 1;
    try testing.expectError(error.Corrupt, Marker.decode(&bad));
    bad = bytes;
    bad[48] ^= 1;
    checksum.stamp(&bad, MARKER_CHECKSUM_OFF);
    try testing.expectError(error.Corrupt, Marker.decode(&bad));
    bad = bytes;
    bad[6] = 2;
    checksum.stamp(&bad, MARKER_CHECKSUM_OFF);
    try testing.expectError(error.Corrupt, Marker.decode(&bad));
    bad = bytes;
    bad[56] = 1;
    checksum.stamp(&bad, MARKER_CHECKSUM_OFF);
    try testing.expectError(error.Corrupt, Marker.decode(&bad));
    // Dual rebind round-trips.
    var dual = m;
    dual.prev_set_epoch = 2;
    try testing.expectEqual(dual, try Marker.decode(&dual.encode()));
    // The bind ladder.
    try testing.expectEqual(BindVerdict.absent, bindVerdict(null, m, false));
    try testing.expectEqual(BindVerdict.match, bindVerdict(m, m, true));
    var other_set = m;
    other_set.set_uuid[0] = 99;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(other_set, m, false));
    var peer = m;
    peer.node_token = 0xDEF;
    try testing.expectEqual(BindVerdict.foreign_scope, bindVerdict(peer, m, true));
    try testing.expectEqual(BindVerdict.upgrade, bindVerdict(peer, m, false));
    var unscoped = m;
    unscoped.node_token = 0;
    try testing.expectEqual(BindVerdict.upgrade, bindVerdict(unscoped, m, true));
    // A dual marker naming our epoch as its previous one matches the set (upgrade restamps it canonical).
    var ours_next = m;
    ours_next.set_epoch = 4;
    var dual_to_4 = m;
    dual_to_4.set_epoch = 4;
    dual_to_4.prev_set_epoch = 3;
    try testing.expectEqual(BindVerdict.upgrade, bindVerdict(dual_to_4, ours_next, false));
    // A newer epoch than ours on the same set is dead to us (we are the stale mount).
    var newer = m;
    newer.set_epoch = 9;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(newer, m, false));
}

test "shard plan: max(next_pow2(cores), 16) halved to fit two copies + slack; a tiny pool takes one shard; the key hashes to a shard" {
    try testing.expectEqual(@as(u32, 16), shardPlan(1 * derive.GiB, 8, 4 * derive.MiB));
    try testing.expectEqual(@as(u32, 64), shardPlan(1 * derive.GiB, 64, 4 * derive.MiB));
    try testing.expectEqual(@as(u32, 128), shardPlan(2 * derive.GiB, 100, 4 * derive.MiB));
    // 64 MiB / 16 = 4 MiB < 2 × 4 MiB + 64 KiB ⇒ halve to 8 (8 MiB ≥ 8 MiB + 64 KiB? no ⇒ 4): 16 MiB per shard fits.
    try testing.expectEqual(@as(u32, 4), shardPlan(64 * derive.MiB, 8, 4 * derive.MiB));
    try testing.expectEqual(@as(u32, 1), shardPlan(9 * derive.MiB, 64, 4 * derive.MiB));
    try testing.expectEqual(@as(u32, 1), shardPlan(12 * derive.MiB, 64, 4 * derive.MiB));
    const k = tkey(1, 1);
    try testing.expect(shardOf(k, 16) < 16);
    try testing.expectEqual(@as(u32, 0), shardOf(k, 1));
}

test "model: the generation gate holds; recycled generations fail (a stale release frees the re-staged sole copy)" {
    GenerationAbaModel.weakening = .none;
    try explorer.expectHolds(GenerationAbaModel, .{});
    GenerationAbaModel.weakening = .recycled_generation;
    defer GenerationAbaModel.weakening = .none;
    try explorer.expectFails(GenerationAbaModel, .{}, error.InvariantViolated);
}

test "stats JSON names every instrument" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try testing.expect(std.mem.startsWith(u8, out, "{\"current_staged_write_bytes\":"));
    try testing.expect(std.mem.indexOf(u8, out, "\"staged_payload_lost_reads\":") != null);
    try testing.expect(std.mem.endsWith(u8, out, "}"));
}

fn recordFuzz(_: void, smith: *std.testing.Smith) !void {
    var image: [4 * 4096]u8 = undefined;
    const len = smith.slice(&image);
    @memset(image[len..], 0);
    var off: u64 = 0;
    while (off + RECORD_HEADER_LEN <= image.len) : (off += RECORD_ALIGN) {
        if (readHeader(&image, off)) |h| _ = verifyRecord(&image, off, h);
    }
    _ = Marker.decode(image[0..@min(len, MARKER_LEN)]) catch {};
}

test "fuzz: the record header reader, the verifier and the marker decoder are total" {
    try std.testing.fuzz({}, recordFuzz, .{});
}

test "linux: a ring under a tmp dir — 0700/0600 modes, mmap put/get, a remount recovers the entries, the marker round-trips two-phase through the io_uring file layer" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    const gpa = testing.allocator;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    var path_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const n = try tmp.dir.realPath(testing.io, &path_buf);
    var root_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const root = try std.fmt.bufPrint(&root_buf, "{s}/staging0", .{path_buf[0..n]});
    const root_fd = try Ring.openRoot(root);
    defer _ = linux.close(root_fd);
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(root_fd, "", linux.AT.EMPTY_PATH, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o700), st.mode & 0o777);
    const cap: u64 = 64 * 4096;
    var ms = try Ring.openSegment(root_fd, "seg-00", cap);
    try testing.expectEqual(cap, ms.map.len);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(ms.fd, "", linux.AT.EMPTY_PATH, .{ .MODE = true }, &st)));
    try testing.expectEqual(@as(u16, 0o600), st.mode & 0o777);
    var seg = try Segment.init(gpa, ms.map, ms.backing());
    const v: [5000]u8 = @splat(0x42);
    try seg.put(tkey(1, 11), &v, 7, 0);
    const l = seg.get(tkey(1, 11)).?;
    try testing.expectEqualSlices(u8, &v, l.bytes());
    l.release();
    try seg.put(tkey(2, 22), "two", 8, 0);
    try testing.expect(seg.remove(tkey(2, 22), 8));
    seg.deinit();
    ms.close();
    // Remount: a shorter requested capacity never shrinks the file.
    var ms2 = try Ring.openSegment(root_fd, "seg-00", 8 * 4096);
    defer ms2.close();
    try testing.expectEqual(cap, ms2.map.len);
    var seg2 = try Segment.init(gpa, ms2.map, ms2.backing());
    defer seg2.deinit();
    const rep = seg2.recover();
    try testing.expectEqual(@as(u64, 1), rep.adopted);
    try testing.expectEqual(@as(u64, 0), rep.torn);
    const l2 = seg2.get(tkey(1, 11)).?;
    defer l2.release();
    try testing.expectEqualSlices(u8, &v, l2.bytes());
    try testing.expectEqual(@as(u64, 7), l2.generation());
    try testing.expect(seg2.get(tkey(2, 22)) == null);
    // The marker through the io_uring file layer.
    const uring_fs = @import("../io/uring_fs.zig");
    const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, 1024));
    defer pool.deinit();
    try testing.expectEqual(@as(?Marker, null), try Ring.readMarker(pool, root));
    const m: Marker = .{ .set_uuid = @splat(7), .set_epoch = 1, .node_token = 5, .mount_slot = 0 };
    try Ring.writeMarker(pool, root, root_fd, m);
    try testing.expectEqual(m, (try Ring.readMarker(pool, root)).?);
    try testing.expectEqual(BindVerdict.match, bindVerdict(try Ring.readMarker(pool, root), m, true));
    var other = m;
    other.set_uuid[0] = 8;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(try Ring.readMarker(pool, root), other, false));
    const d0 = stats.staging_generation_discards.load();
    Ring.wipeSegments(root_fd, &.{"seg-00"});
    try testing.expectEqual(d0 + 1, stats.staging_generation_discards.load());
    var ms3 = try Ring.openSegment(root_fd, "seg-00", 8 * 4096);
    defer ms3.close();
    try testing.expectEqual(@as(usize, 8 * 4096), ms3.map.len);
}
