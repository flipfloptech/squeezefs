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
//!
//! **Durability** (charter §1 item 10, the owner's ruling of 2026-10-19): a
//! resident's bytes are the page cache's until something syncs them — the
//! daemon's death keeps them, a power cut does not. fsync, fdatasync and
//! syncfs make the ino's resident record reach stable storage before their
//! ack (`Segment.syncResident` / `syncAll`): ONE ranged sync of the segment
//! file over the record's span (`Backing.durable`), coalesced across
//! concurrent callers by `SyncCoalescer` — every request that arrived before
//! a sync started is answered by it, so a storm of fsyncs costs one sync
//! per batch, never one per call.
//!
//! **Departure** (the same law on the way OUT): a key leaves the ring when
//! the metadata stops naming its record — a promotion's layout commit, a
//! truncate to zero, a destroy. The two homes of a synced file must agree
//! at every cut: as long as the publish that stopped naming the record is
//! not durable, recovery reads the OLD layout and expects the record whole,
//! so the departed copy stays on the segment (no tombstone, no punch, its
//! slot live for placement) until a COVER ROUND — the meta barrier past that
//! publish (`CoverSeam`, the plane binds it) — lands (`Segment.cover`). The
//! block a promotion DMA'd is the plane's: it flushes the volume BEFORE the
//! publish (DUR-1), so a durable layout never names bytes a cut took.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const checksum = @import("../core/checksum.zig");
const clock = @import("../core/clock.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");
const tick = @import("../sync/tick.zig");
const futex = @import("../sync/futex.zig");
const layout = @import("layout.zig");
const explorer = @import("../model/explorer.zig");

pub const FileId = layout.FileId;

pub const stats = struct {
    /// `current_staged_write_bytes`: the pool's charged footprint (the
    /// budget component `staging_mmap`'s gauge).
    pub var current_staged_write_bytes: counters.Gauge = .{};
    /// `staged_writes_in_flight`: ring keys resident or being placed.
    pub var staged_writes_in_flight: counters.Gauge = .{};
    /// `staging_cell_state_bytes`: the live cells' value-hash states
    /// (`@sizeOf(ValueHash)` each — the budget component
    /// `staging_cell_state`'s gauge); `≤ Segment.cellStateBound(capacity)`.
    pub var staging_cell_state_bytes: counters.Gauge = .{};
    /// `staging_tables_bytes`: the segment's fixed per-cell tables (the
    /// cells, the free list, the pending list, the placement scratch, the
    /// key index), preallocated at `maxCells` for the ring's life — the
    /// budget component `staging_tables`' gauge; `= Segment.tablesBound(
    /// capacity)` per open segment.
    pub var staging_tables_bytes: counters.Gauge = .{};
    /// `staging_over_cap_refusals`: a stage refused `StorageFull` at the
    /// pool cap (the caller's spill escalation follows).
    pub var staging_over_cap_refusals: counters.ShardedCounter = .{};
    /// `staging_ring_full_refusals`: a placement found no hole (never
    /// destroys a live entry; the write takes its own-block arm — the ring
    /// is a cache, never the set's capacity).
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
    /// `staging_foreign_scope_refusals`: a root whose marker names ANOTHER
    /// member (its owner dead) refused ours — never adopted, never wiped
    /// (0003 §T.1).
    pub var staging_foreign_scope_refusals: counters.ShardedCounter = .{};
    /// `staging_owner_live_refusals`: a root whose live claim another
    /// process holds refused ours — two mounts presenting one member scope.
    pub var staging_owner_live_refusals: counters.ShardedCounter = .{};
    /// `staging_member_residue_roots`: other members' roots under this
    /// mount's cache path with no live owner and ring bytes on disk at its
    /// bind — residue only that member's successor recovers (reported loud,
    /// never touched).
    pub var staging_member_residue_roots: counters.Gauge = .{};
    /// `staged_payload_lost_reads`: zeros served for an unreachable staged
    /// payload (another client's live custody, or a crash discarded it).
    /// 0 on any client of a cleanly unmounted set (gate G-PK2).
    pub var staged_payload_lost_reads: counters.ShardedCounter = .{};
    /// `staging_backing_phase_ns`: the wall of the ring's backing syscalls
    /// (`msync` on a put, `fallocate(PUNCH_HOLE)` + `madvise` on a release)
    /// — they run on the caller's lane thread, so their tail IS a lane stall.
    pub var backing_sync_ns: counters.ShardedLatencyHistogram = .{};
    pub var backing_reclaim_ns: counters.ShardedLatencyHistogram = .{};
    /// `dismount_promoted_files` / `dismount_promoted_bytes` /
    /// `dismount_promote_failures`: the clean-unmount promotion pass.
    pub var dismount_promoted_files: counters.ShardedCounter = .{};
    pub var dismount_promoted_bytes: counters.ShardedCounter = .{};
    pub var dismount_promote_failures: counters.ShardedCounter = .{};
    /// `dismount_corpse_residents_destroyed`: residents the dismount pass
    /// destroyed because their ino has no live inode record (absent, or
    /// `nlink == 0`) — a corpse's custody is destroyed, never promoted.
    /// `dismount_unnamed_residents_destroyed`: residents of a live ino whose
    /// layout does not name them (a crash between a promotion's commit and
    /// its ring release, or a stage whose commit never landed) — residue
    /// no read can reach, destroyed. Both leave the ring without a
    /// promotion; neither is a `dismount_promote_failures` row.
    pub var dismount_corpse_residents_destroyed: counters.ShardedCounter = .{};
    pub var dismount_unnamed_residents_destroyed: counters.ShardedCounter = .{};
    /// `fsync_promoted_files`: promotions the fsync lever dispatched.
    pub var fsync_promoted_files: counters.ShardedCounter = .{};
    /// The staged bytes' durability at fsync (module doc):
    /// `staging_fsync_syncs` — ranged syncs of the segment file issued (one
    /// per batch); `staging_fsync_bytes` — the spans they covered;
    /// `staging_fsync_coalesced` — requests answered by a sync another
    /// request led (requests ≡ syncs + coalesced); `staging_fsync_sync_ns`
    /// — the wall of one sync, from issue to its completion.
    pub var staging_fsync_syncs: counters.ShardedCounter = .{};
    pub var staging_fsync_bytes: counters.ShardedCounter = .{};
    pub var staging_fsync_coalesced: counters.ShardedCounter = .{};
    pub var staging_fsync_sync_ns: counters.ShardedLatencyHistogram = .{};
    /// `staging_superseded_held` / `staging_superseded_held_bytes`: copies a
    /// same-key re-stage superseded, kept intact until their successor is
    /// durable or their key leaves (`Cell` doc); `staging_superseded_released`:
    /// the ones released so far.
    pub var staging_superseded_held: counters.Gauge = .{};
    pub var staging_superseded_held_bytes: counters.Gauge = .{};
    pub var staging_superseded_released: counters.ShardedCounter = .{};
    /// The departure law (module doc): `staging_departures_deferred` —
    /// keys that left the ring with their copies held for a cover round;
    /// `staging_departed_held` / `staging_departed_held_bytes` — the copies
    /// held now; `staging_departed_released` — copies a round let go;
    /// `staging_cover_rounds` — rounds run (one meta barrier each, the
    /// lane's coalescing across concurrent rounds).
    pub var staging_departures_deferred: counters.ShardedCounter = .{};
    pub var staging_departed_held: counters.Gauge = .{};
    pub var staging_departed_held_bytes: counters.Gauge = .{};
    pub var staging_departed_released: counters.ShardedCounter = .{};
    pub var staging_cover_rounds: counters.ShardedCounter = .{};
    /// §J.2's ring ledger: `staging_appends` / `staging_append_bytes` — the
    /// extending writes that landed IN PLACE (bytes moved = the bytes);
    /// `staging_restages` / `staging_restage_bytes` — whole images re-staged
    /// under a resident key (bytes moved = 2 × the image: out of the ring,
    /// back in). `Σ ÷ appended ≤ STAGED_APPEND_MOVED_BOUND` on an append
    /// stream.
    pub var staging_appends: counters.ShardedCounter = .{};
    pub var staging_append_bytes: counters.ShardedCounter = .{};
    pub var staging_restages: counters.ShardedCounter = .{};
    pub var staging_restage_bytes: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "current_staged_write_bytes", "staged_writes_in_flight", "staging_cell_state_bytes", "staging_tables_bytes", "staging_over_cap_refusals", "staging_ring_full_refusals", "staging_pressure_kicks", "staging_recovery_records", "staging_recovery_torn_records", "staging_generation_discards", "staging_foreign_scope_refusals", "staging_owner_live_refusals", "staging_member_residue_roots", "staged_payload_lost_reads", "dismount_promoted_files", "dismount_promoted_bytes", "dismount_promote_failures", "dismount_corpse_residents_destroyed", "dismount_unnamed_residents_destroyed", "fsync_promoted_files", "staging_fsync_syncs", "staging_fsync_bytes", "staging_fsync_coalesced", "staging_superseded_held", "staging_superseded_held_bytes", "staging_superseded_released", "staging_departures_deferred", "staging_departed_held", "staging_departed_held_bytes", "staging_departed_released", "staging_cover_rounds", "staging_appends", "staging_append_bytes", "staging_restages", "staging_restage_bytes" }, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("}");
}

pub fn writePhasesJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"backing_sync\":");
    try stats.backing_sync_ns.snapshot().writeJson(w);
    try w.writeAll(",\"backing_reclaim\":");
    try stats.backing_reclaim_ns.snapshot().writeJson(w);
    try w.writeAll(",\"fsync_sync\":");
    try stats.staging_fsync_sync_ns.snapshot().writeJson(w);
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// §3.2 The segment record — v4, checksummed.
// ---------------------------------------------------------------------------

/// ASCII in disk order (0002 row 38).
pub const SEGMENT_MAGIC = "SQZS".*;
/// v3: v2's two header slots (A at the grain, B at `HEADER_B_OFF`, so an
/// in-place append rewrites the slot that is NOT current and recovery takes
/// the valid slot with the higher generation — a kill between any two
/// stores leaves the old record intact, decision 0003 §J.2) with the
/// checksum sqzhash128's low 64 bits (0003 §M). A v1 (one slot) or v2
/// (xxh3-64) record is a torn record to this scan: the ring is a cache,
/// wiped loud.
pub const SEGMENT_RECORD_VERSION: u16 = 3;
/// Records and their values sit on the LBA grain: the value is the zero-copy
/// DMA source of a promotion (`O_DIRECT` refuses an unaligned buffer) and a
/// header on the grain is what the recovery scan steps by.
pub const RECORD_ALIGN: u64 = layout.GRAIN;
/// Slot B's offset inside the record: past slot A and the 24 B key, on the
/// next cache line.
pub const HEADER_B_OFF: u64 = 64;

pub const HeaderSlot = enum(u8) {
    a,
    b,

    pub fn offset(self: HeaderSlot) u64 {
        return if (self == .a) 0 else HEADER_B_OFF;
    }

    pub fn other(self: HeaderSlot) HeaderSlot {
        return if (self == .a) .b else .a;
    }
};

/// 40 B, little-endian; `hash64` over header (field zeroed) ‖ key ‖ value.
pub const SegmentRecordWire = extern struct {
    magic: [4]u8,
    version_le: [2]u8,
    /// = 0 (checked).
    flags_le: [2]u8,
    key_len_le: [4]u8,
    val_len_le: [4]u8,
    generation_le: [8]u8,
    fencing_le: [8]u8,
    csum_le: [8]u8,
};

pub const RECORD_HEADER_LEN: usize = @sizeOf(SegmentRecordWire);
const CHECKSUM_OFF: usize = @offsetOf(SegmentRecordWire, "csum_le");

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
    // Slot A ‖ key fit before slot B; slot B fits before the value.
    assert(RECORD_HEADER_LEN + KEY_LEN <= HEADER_B_OFF);
    assert(HEADER_B_OFF + RECORD_HEADER_LEN <= RECORD_ALIGN);
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

/// What a stage reserves beyond its footprint (§J.2).
pub const Slack = enum {
    /// A whole-image stage (nothing grows in place).
    none,
    /// The write EXTENDS the image (an append, a write at or past the end):
    /// `max(GRAIN, V/4)` of slack so the next appends land in place.
    append,
};

/// The append slack for an image of `val_len`: a quarter of the image, at
/// least one grain. WHY a quarter: the pool's high-water already keeps a
/// quarter of the ring free for the drain's lag, so the slack never exceeds
/// what the ring has promised to hold; a re-stage at `V` moves `2V` and
/// buys `V/4` of appends, so bytes moved per appended byte is bounded at
/// `1 + 2(1 + ¼)/¼ = 11` over the geometric chain.
pub fn appendSlack(val_len: u64) u64 {
    return @max(RECORD_ALIGN, val_len / 4);
}

/// The slot a stage reserves: the stride, plus the slack rounded to the grain.
pub fn reserveFor(key_len: u64, val_len: u64, slack: Slack) u64 {
    return switch (slack) {
        .none => stride(key_len, val_len),
        .append => derive.alignUp(footprint(key_len, val_len) + appendSlack(val_len), RECORD_ALIGN),
    };
}

/// §J.2's bound on ring bytes moved per appended byte (the tie test
/// restates the chain): `1 + 2(1 + s)/s` at slack fraction `s = ¼`.
pub const STAGED_APPEND_MOVED_BOUND: u64 = 11;

pub const RecordHeader = struct {
    key_len: u32,
    val_len: u32,
    generation: u64,
    fencing: u64,
    csum: u64,

    pub fn footprintBytes(self: RecordHeader) u64 {
        return footprint(self.key_len, self.val_len);
    }
};

/// The checksum's streaming state over the VALUE (`hash64` consumes the value
/// FIRST, the key and the header LAST): an in-place append extends the
/// state by the appended bytes and finishes it — O(appended), never a
/// rehash of a 4 MiB image per 4-byte append. The live cell keeps the state
/// (one per live record, `gpa`-owned, gauged `staging_cell_state_bytes`).
pub const ValueHash = checksum.Hasher;

comptime {
    // A record occupies at least one grain of the ring; the RAM state kept
    // beside it must stay under that, or the states outweigh the ring they
    // describe (the 16-chunk hasher did: 4.7 KiB per cell against 4 KiB).
    assert(@sizeOf(ValueHash) < RECORD_ALIGN);
}

/// The R5 component over the cells' states: attribution-only (weight 0) —
/// a state leaves with its cell, and the never-lossy lever that empties
/// cells is `staging_mmap`'s promotion kick; heap bytes, never page cache.
pub fn cellStateComponent() budget.Component {
    return budget.Component.init("staging_cell_state", 0, 0, .{ .gauge = &stats.staging_cell_state_bytes }, null);
}

/// The R5 component over the segment's fixed per-cell tables: attribution-
/// only (weight 0) — they are allocated once at `Segment.init` for the
/// ring's life and leave with it; nothing short of closing the ring returns
/// them. Heap, never page cache.
pub fn tablesComponent() budget.Component {
    return budget.Component.init("staging_tables", 0, 0, .{ .gauge = &stats.staging_tables_bytes }, null);
}

pub fn valueHash(value: []const u8) ValueHash {
    var h: ValueHash = .init();
    h.update(value);
    return h;
}

/// Finish the record checksum from the value's state: `hash64(value ‖ key ‖
/// header[0..32] ‖ 0⁸)` (the checksum field hashed as zeros).
fn finishChecksum(state: *const ValueHash, header: []const u8, key: []const u8) u64 {
    var h = state.*;
    h.update(key);
    h.update(header[0..CHECKSUM_OFF]);
    const zeros: [8]u8 = @splat(0);
    h.update(&zeros);
    return h.finalLow64();
}

/// Write `header A ‖ key ‖ zero pad (slot B cleared) ‖ value` at `rec_off`
/// and stamp the checksum. The caller reserved `≥ stride(key.len,
/// value.len)` bytes; `state` is `valueHash(value)`.
pub fn writeRecord(image: []u8, rec_off: u64, key: []const u8, value: []const u8, generation: u64, fencing: u64, state: *const ValueHash) void {
    const voff = valueOffset(key.len);
    assert(rec_off + voff + value.len <= image.len);
    assert(derive.isAligned(rec_off, RECORD_ALIGN));
    const rec = image[@intCast(rec_off)..];
    @memcpy(rec[RECORD_HEADER_LEN..][0..key.len], key);
    @memset(rec[RECORD_HEADER_LEN + key.len .. @intCast(voff)], 0);
    @memcpy(rec[@intCast(voff)..][0..value.len], value);
    writeHeader(image, rec_off, .a, key, value.len, generation, fencing, state);
}

/// Write one header slot (the checksum from the value's state, stamped
/// last); the other slot is untouched.
fn writeHeader(image: []u8, rec_off: u64, slot: HeaderSlot, key: []const u8, val_len: usize, generation: u64, fencing: u64, state: *const ValueHash) void {
    var h: SegmentRecordWire = .{ .magic = SEGMENT_MAGIC, .version_le = undefined, .flags_le = .{ 0, 0 }, .key_len_le = undefined, .val_len_le = undefined, .generation_le = undefined, .fencing_le = undefined, .csum_le = @splat(0) };
    std.mem.writeInt(u16, &h.version_le, SEGMENT_RECORD_VERSION, .little);
    std.mem.writeInt(u32, &h.key_len_le, @intCast(key.len), .little);
    std.mem.writeInt(u32, &h.val_len_le, @intCast(val_len), .little);
    std.mem.writeInt(u64, &h.generation_le, generation, .little);
    std.mem.writeInt(u64, &h.fencing_le, fencing, .little);
    const rec = image[@intCast(rec_off)..];
    const hdr = rec[@intCast(slot.offset())..][0..RECORD_HEADER_LEN];
    hdr.* = std.mem.toBytes(h);
    std.mem.writeInt(u64, hdr[CHECKSUM_OFF..][0..8], finishChecksum(state, hdr, key), .little);
}

fn recordChecksum(header: []const u8, key: []const u8, value: []const u8) u64 {
    const state = valueHash(value);
    return finishChecksum(&state, header, key);
}

/// Parse slot A's header at `rec_off`: magic, version, zero flags, a
/// geometry that fits the image. Null = no record starts here.
pub fn readHeader(image: []const u8, rec_off: u64) ?RecordHeader {
    return readHeaderAt(image, rec_off, .a);
}

/// Parse one header slot of the record at `rec_off`.
pub fn readHeaderAt(image: []const u8, rec_off: u64, slot: HeaderSlot) ?RecordHeader {
    const hoff = rec_off + slot.offset();
    if (hoff + RECORD_HEADER_LEN > image.len) return null;
    const h = std.mem.bytesToValue(SegmentRecordWire, image[@intCast(hoff)..][0..RECORD_HEADER_LEN]);
    if (!std.mem.eql(u8, &h.magic, &SEGMENT_MAGIC)) return null;
    if (std.mem.readInt(u16, &h.version_le, .little) != SEGMENT_RECORD_VERSION) return null;
    if (std.mem.readInt(u16, &h.flags_le, .little) != 0) return null;
    const r: RecordHeader = .{
        .key_len = std.mem.readInt(u32, &h.key_len_le, .little),
        .val_len = std.mem.readInt(u32, &h.val_len_le, .little),
        .generation = std.mem.readInt(u64, &h.generation_le, .little),
        .fencing = std.mem.readInt(u64, &h.fencing_le, .little),
        .csum = std.mem.readInt(u64, &h.csum_le, .little),
    };
    if (RECORD_HEADER_LEN + r.key_len > HEADER_B_OFF) return null;
    if (rec_off + r.footprintBytes() > image.len) return null;
    return r;
}

/// Does the record at `rec_off` carry the checksum its bytes compute (slot A)?
pub fn verifyRecord(image: []const u8, rec_off: u64, h: RecordHeader) bool {
    return verifyRecordAt(image, rec_off, h, .a);
}

pub fn verifyRecordAt(image: []const u8, rec_off: u64, h: RecordHeader, slot: HeaderSlot) bool {
    const rec = image[@intCast(rec_off)..];
    const voff: usize = @intCast(valueOffset(h.key_len));
    const hdr = rec[@intCast(slot.offset())..][0..RECORD_HEADER_LEN];
    return h.csum == recordChecksum(hdr, rec[RECORD_HEADER_LEN..][0..h.key_len], rec[voff..][0..h.val_len]);
}

pub const CurrentHeader = struct { h: RecordHeader, slot: HeaderSlot };

/// The record's CURRENT header: of the slots that verify, the one with the
/// higher generation (an append writes the alternate slot last; a kill
/// mid-write leaves the other intact). Null = no valid header here.
pub fn currentHeader(image: []const u8, rec_off: u64) ?CurrentHeader {
    var best: ?CurrentHeader = null;
    inline for (.{ HeaderSlot.a, HeaderSlot.b }) |slot| {
        if (readHeaderAt(image, rec_off, slot)) |h| {
            if (verifyRecordAt(image, rec_off, h, slot) and (best == null or h.generation > best.?.h.generation)) best = .{ .h = h, .slot = slot };
        }
    }
    return best;
}

/// Whether the record header at `rec_off` reads as a tombstone (magic
/// zeroed by a removal).
pub fn isTombstone(image: []const u8, rec_off: u64) bool {
    return std.mem.allEqual(u8, image[@intCast(rec_off)..][0..4], 0);
}

// ---------------------------------------------------------------------------
// §3.2 The segment — placement, two-phase replace, leases, recovery.
// ---------------------------------------------------------------------------

/// A sync of the backing file failed: the bytes are the page cache's still
/// (the daemon serves them), but a power cut may lose them — the fsync
/// answers EIO.
pub const DurableError = error{InputOutput};

/// What backs the image: the Linux ring binds `msync`/`fallocate`+`madvise`
/// and the ranged sync; a RAM image (tests, the portable core) binds
/// nothing — or a test's counting seam.
pub const Backing = struct {
    ctx: ?*anyopaque = null,
    /// Flush `[off, off + len)` (MS_ASYNC: the write is ordered, the
    /// promotion's own DMA reads the mapping, never the file).
    sync: ?*const fn (ctx: ?*anyopaque, off: u64, len: u64) void = null,
    /// `[off, off + len)` on stable storage before this returns (the
    /// ranged `fdatasync` of the segment file: its dirty pages written and
    /// waited for, the metadata that reaches them committed, the device's
    /// cache flushed). The fsync law's leg; a caller parks through `io`.
    durable: ?*const fn (ctx: ?*anyopaque, io: Io, off: u64, len: u64) DurableError!void = null,
    /// Return `[off, off + len)` to the host (punch + drop pages) after a
    /// tombstone; a cleanly unmounted dir scans EMPTY.
    reclaim: ?*const fn (ctx: ?*anyopaque, off: u64, len: u64) void = null,
    /// The first offset ≥ `off` that holds DATA (`SEEK_DATA`), null past
    /// the last. The recovery scan skips holes with it: reading a hole
    /// through the shared mapping would populate a page-cache page per
    /// grain (a cleanly unmounted 546 MiB ring is all holes and sat fully
    /// resident at every mount before this). Null = scan every grain.
    nextData: ?*const fn (ctx: ?*anyopaque, off: u64) ?u64 = null,

    pub const none: Backing = .{};
};

/// The departure law's round (module doc): every layout publish committed
/// before the call is durable on return — the meta barrier (the plane binds
/// the backend's coalesced barrier). A segment with no seam (a RAM image,
/// the portable tests) releases a departed copy at once: nothing durable
/// is owed to it.
pub const CoverSeam = struct {
    ctx: ?*anyopaque = null,
    run: *const fn (ctx: ?*anyopaque, io: Io) DurableError!void,
};

/// A placed record.
pub const Extent = struct {
    rec_off: u64,
    key_len: u32,
    val_len: u32,
    generation: u64,
    /// The slot's length: `≥ stride(key_len, val_len)`; the slack past the
    /// stride is what an in-place append grows into. Recovery sets the
    /// stride (the slack is RAM-only knowledge).
    reserved: u64,
    /// The header slot that is current.
    slot: HeaderSlot = .a,

    pub fn valOff(self: Extent) u64 {
        return self.rec_off + valueOffset(self.key_len);
    }

    pub fn end(self: Extent) u64 {
        return self.rec_off + self.reserved;
    }

    /// Bytes the value can grow to in place.
    pub fn valueCapacity(self: Extent) u64 {
        return self.reserved - valueOffset(self.key_len);
    }
};

/// One live entry's cell: readers hold references; the index holds one.
/// The LAST reference out reclaims a retired cell's bytes — so a same-key
/// replace never destroys the copy a reader is inside, and a reader never
/// sees a torn record (one intact copy at every instant). A cell SHADOWED
/// by an in-place append shares its bytes with its successor: the last
/// reference frees the cell alone, never the record. A retired cell stays
/// on the segment's retired list until its reclaim, and its extent stays
/// LIVE for placement there: a leased copy is never overlaid, and a
/// SUPERSEDED copy (a same-key re-stage) keeps its bytes intact — no
/// tombstone, no punch, no reuse — until its successor is durable or its
/// key leaves the ring (the deferral holds one reference): a power cut
/// after an unsynced overwrite must find the old, synced copy whole
/// (recovery takes the higher generation that survived), never a hole. A
/// DEPARTED copy (its key left: promoted, truncated away, destroyed) keeps
/// its bytes the same way until a cover round past its departure lands
/// (module doc "Departure"); the superseded copies of a departing key
/// become departed with it — the old synced image is what a cut before
/// the publish's durability must find.
const Cell = struct {
    refs: std.atomic.Value(u32) = .init(0),
    retired: std.atomic.Value(bool) = .init(false),
    shadowed: bool = false,
    extent: Extent = undefined,
    key: [KEY_LEN]u8 = undefined,
    /// The value's checksum state (`valueHash` over `[0, val_len)`): the
    /// append extends it. Null for a recovered record until its first
    /// append rehashes the value once.
    hash: ?*ValueHash = null,
    /// The segment's index sequence at this cell's index flip: a sync that
    /// started after it covers the bytes written before it.
    indexed_seq: u64 = 0,
    /// A sync covered this record (the key's fsync, a syncAll begun after
    /// its index flip, or it was recovered off the file): what the
    /// supersession and departure deferrals protect. An unsynced copy is
    /// released the moment it is superseded or departs.
    synced: bool = false,
    /// The retired list (under the segment lock).
    prev_retired: ?*Cell = null,
    next_retired: ?*Cell = null,
    /// The deferral that holds one reference (module doc of the struct):
    /// `.superseded` waits for a sync covering this key's copy at or past
    /// `hold_seq` (its successor's generation) or for the key to leave;
    /// `.departed` waits for the cover round numbered `hold_seq` (or a
    /// later one) to land. One reason at a time: a superseded copy whose
    /// key departs becomes departed.
    hold: Hold = .none,
    hold_seq: u64 = 0,

    const Hold = enum(u8) { none, superseded, departed };
};

pub const PutError = error{
    /// No hole fits (the hot-path refusal the caller escalates to a
    /// durable spill); counted.
    RingFull,
    OutOfMemory,
};

pub const AppendError = error{
    OutOfMemory,
    /// No resident copy under the key.
    NotResident,
    /// The resident copy's generation is not the one the caller read.
    Stale,
    /// The write does not extend the image, or the slack cannot hold it:
    /// the caller re-stages the whole image.
    NoRoom,
};

pub const Segment = struct {
    gpa: Allocator,
    image: []u8,
    backing: Backing,
    lock: jc.SpinLock = .{},
    index: std.AutoHashMapUnmanaged([KEY_LEN]u8, *Cell) = .empty,
    /// The placement cursor: records go at/after it, wrapping.
    write_offset: u64 = 0,
    cells: []Cell,
    free_cells: std.ArrayList(u32) = .empty,
    /// The live set at the grain: bit `g` set = grain `g` belongs to an
    /// indexed record, a placement in flight or a retired copy not yet
    /// reclaimed (a shadowed cell shares its successor's grains). Placement
    /// is a first-fit scan of clear runs from the cursor, wrapping — O(need)
    /// where the cursor leads free space, one pass over the words on a full
    /// ring — never a sort of the live extents: that scan (O(n log n) + an
    /// O(n²) fit check under the lock, per put) serialized every staged
    /// write of a mount at ≈ 1 k/s once the held copies grew the set.
    occupied: []u64,
    /// The fsync law's batcher over `backing.durable` (module doc).
    durable: SyncCoalescer = .{},
    /// Retired cells not yet reclaimed (`Cell` doc), under the lock.
    retired_head: ?*Cell = null,
    /// The index sequence: bumped at every index flip (release); a sync
    /// batch reads it at its start (acquire) — `Cell.indexed_seq`.
    indexed_seq: std.atomic.Value(u64) = .init(1),
    /// The departure law's round (module doc); null = a departed copy
    /// releases at once.
    cover_seam: ?CoverSeam = null,
    /// Cover rounds started so far (under the lock): a departure waits for
    /// round `cover_started + 1` — one that starts after it.
    cover_started: u64 = 0,

    /// Every live or pending record occupies ≥ 1 grain of its own, so the
    /// cell population is bounded by `capacity / grain`; one more for the
    /// replacement copy in flight.
    pub fn maxCells(cap: u64) usize {
        return @intCast(cap / RECORD_ALIGN + 1);
    }

    /// The most RAM the live cells' value-hash states can take at `cap`:
    /// one state per cell (`staging_cell_state_bytes` never exceeds it).
    pub fn cellStateBound(cap: u64) u64 {
        return @as(u64, maxCells(cap)) * @sizeOf(ValueHash);
    }

    /// Bytes of one key-index slot: the std hash map's metadata byte
    /// (state + fingerprint) beside the key and the cell pointer.
    const INDEX_SLOT_BYTES: u64 = 1 + KEY_LEN + @sizeOf(*Cell);

    /// The key index's slot count for `n` entries — the std hash map's own
    /// law (`capacityForSize`): `n / load + 1` rounded up to a power of two
    /// at its default load (`std.hash_map.default_max_load_percentage`).
    /// Restated here so the tables' gauge is a pure derivation; the tie test
    /// pins it against the live map's `capacity()`.
    pub fn indexSlots(n: usize) u64 {
        const percent: u64 = 100;
        return std.math.ceilPowerOfTwo(u64, @as(u64, n) * percent / std.hash_map.default_max_load_percentage + 1) catch unreachable;
    }

    /// The grain bitmap's words at `cap`: one bit per grain.
    pub fn occupiedWords(cap: u64) usize {
        return @intCast((cap / RECORD_ALIGN + 63) / 64);
    }

    /// The RAM the fixed per-cell tables take at `cap`: `n = maxCells(cap)`
    /// cells, `n` free-list slots, the grain bitmap and the key index's slot
    /// arrays (its 3-word header is not counted). Every table is sized ONCE
    /// at `init` and lives with the segment.
    pub fn tablesBound(cap: u64) u64 {
        const n: u64 = maxCells(cap);
        return n * @sizeOf(Cell) + n * @sizeOf(u32) + @as(u64, occupiedWords(cap)) * @sizeOf(u64) + indexSlots(@intCast(n)) * INDEX_SLOT_BYTES;
    }

    /// What this segment's tables take — the same terms read off the live
    /// tables (`tablesBound(capacity)` by construction; the tie test says so).
    pub fn tablesBytes(self: *const Segment) u64 {
        return @as(u64, self.cells.len) * @sizeOf(Cell) + @as(u64, self.free_cells.capacity) * @sizeOf(u32) + @as(u64, self.occupied.len) * @sizeOf(u64) + @as(u64, self.index.capacity()) * INDEX_SLOT_BYTES;
    }

    /// A cell's value-hash state, gauged at birth; `destroyState` undoes it.
    fn createState(self: *Segment) Allocator.Error!*ValueHash {
        const state = try self.gpa.create(ValueHash);
        stats.staging_cell_state_bytes.add(@sizeOf(ValueHash));
        return state;
    }

    fn destroyState(self: *Segment, state: *ValueHash) void {
        stats.staging_cell_state_bytes.sub(@sizeOf(ValueHash));
        self.gpa.destroy(state);
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
        const occupied = try gpa.alloc(u64, occupiedWords(image.len));
        errdefer gpa.free(occupied);
        @memset(occupied, 0);
        var self: Segment = .{ .gpa = gpa, .image = image, .backing = backing, .cells = cells, .free_cells = free, .occupied = occupied };
        errdefer self.index.deinit(gpa);
        try self.index.ensureTotalCapacity(gpa, @intCast(n));
        stats.staging_tables_bytes.add(self.tablesBytes());
        return self;
    }

    pub fn deinit(self: *Segment) void {
        assert(self.durable.head == null and !self.durable.running);
        stats.staging_tables_bytes.sub(self.tablesBytes());
        // Superseded and departed copies still held leave with the session
        // (the next mount's recovery takes the higher generation; a
        // departed record its layout no longer names is the dismount
        // pass's `unnamed`).
        var r = self.retired_head;
        while (r) |c| : (r = c.next_retired) {
            switch (c.hold) {
                .none => {},
                .superseded => {
                    stats.staging_superseded_held.sub(1);
                    stats.staging_superseded_held_bytes.sub(c.extent.end() - c.extent.rec_off);
                },
                .departed => {
                    stats.staging_departed_held.sub(1);
                    stats.staging_departed_held_bytes.sub(c.extent.end() - c.extent.rec_off);
                },
            }
        }
        for (self.cells) |*c| if (c.hash) |h| self.destroyState(h);
        self.index.deinit(self.gpa);
        self.free_cells.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.occupied);
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

    /// The resident record's bytes on the image — both header slots, the
    /// key and the value, grain-rounded — or null without a resident copy.
    pub fn residentSpan(self: *Segment, key: StagingKey) ?Span {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return null;
        const e = cell.extent;
        return .{ .off = e.rec_off, .len = stride(e.key_len, e.val_len) };
    }

    // ---- the fsync law (module doc) ----

    /// fsync / fdatasync of `key`'s ino: its resident record on stable
    /// storage before the return — one coalesced ranged sync over the
    /// record's span; the copies it superseded let go. No resident copy
    /// (promoted, released, never staged here) ⇒ nothing is owed. A RAM
    /// image with no `durable` binding owes nothing either (the portable
    /// tests' segments).
    pub fn syncResident(self: *Segment, io: Io, key: StagingKey) DurableError!void {
        if (self.backing.durable == null) return;
        const e = self.residentExtent(key) orelse return;
        _ = try self.durable.request(self, io, .{ .off = e.rec_off, .len = stride(e.key_len, e.val_len) }, key);
        self.lock.lock();
        defer self.lock.unlock();
        self.releaseSupersededLocked(key.bytes(), e.generation);
    }

    fn residentExtent(self: *Segment, key: StagingKey) ?Extent {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return null;
        return cell.extent;
    }

    /// syncfs: every resident record on stable storage — one coalesced
    /// sync over the whole image (the kernel writes only its dirty pages);
    /// every superseded copy whose successor the sync covered lets go. An
    /// empty ring owes nothing.
    pub fn syncAll(self: *Segment, io: Io) DurableError!void {
        if (self.backing.durable == null) return;
        if (self.live() == 0) return;
        const start_seq = try self.durable.request(self, io, .{ .off = 0, .len = self.image.len }, null);
        self.lock.lock();
        defer self.lock.unlock();
        self.releaseSupersededCoveredLocked(start_seq);
    }

    /// The leader's marking after a sync landed (under the segment lock,
    /// BEFORE the batch is handed over): every copy of `key` indexed
    /// before the sync began is synced — the resident one and its held
    /// predecessors (a re-stage that landed meanwhile is not: its pages
    /// may have missed the kernel's dirty walk); `null` = the whole image.
    fn markSyncedLocked(self: *Segment, key: ?[KEY_LEN]u8, start_seq: u64) void {
        if (key) |k| {
            if (self.index.get(k)) |c| if (c.indexed_seq < start_seq) {
                c.synced = true;
            };
            var cur = self.retired_head;
            while (cur) |c| : (cur = c.next_retired) {
                if (c.indexed_seq < start_seq and std.mem.eql(u8, &c.key, &k)) c.synced = true;
            }
            return;
        }
        var it = self.index.valueIterator();
        while (it.next()) |c| if (c.*.indexed_seq < start_seq) {
            c.*.synced = true;
        };
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) if (c.indexed_seq < start_seq) {
            c.synced = true;
        };
    }

    /// A copy a sync IN FLIGHT will cover (indexed before it began, inside
    /// its span): as good as synced for the deferrals — the sync's caller
    /// is about to be told so.
    fn coveredByRunningSync(self: *Segment, cell: *const Cell) bool {
        return self.durable.covers(cell.extent.rec_off, cell.extent.end(), cell.indexed_seq);
    }

    // ---- the two-phase replace ----

    /// Phase 1 under the lock: the first clear run of the grain bitmap that
    /// holds the record (the live set INCLUDES the current copy of this key
    /// and every placement in flight), at/after the cursor first, then
    /// wrapped; its grains are taken. No fit ⇒ `RingFull`, loud — never
    /// destroys a live entry. Phase 1b outside the lock: the bytes. Phase 2
    /// under the lock: flip the index, retire the superseded copy — HELD
    /// (no tombstone, no punch) iff a sync covered it, released at once
    /// otherwise (an unsynced copy owes nothing to a cut; held, it would
    /// grow the live set until the key's first fsync). A same-key replace
    /// is ATOMIC for readers and torn-write immune (a crash between the
    /// phases leaves two copies; recovery dedups by generation).
    pub fn put(self: *Segment, key: StagingKey, value: []const u8, generation: u64, fencing: u64, reserve: u64) PutError!void {
        const need = @max(reserve, stride(KEY_LEN, value.len));
        assert(derive.isAligned(need, RECORD_ALIGN));
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
            const e: Extent = .{ .rec_off = rec_off, .key_len = KEY_LEN, .val_len = @intCast(value.len), .generation = generation, .reserved = need };
            self.markLocked(rec_off, need, true);
            self.write_offset = e.end();
            if (self.write_offset >= self.image.len) self.write_offset = 0;
            break :blk e;
        };
        stats.staged_writes_in_flight.add(1);
        errdefer {
            self.lock.lock();
            self.markLocked(placed.rec_off, placed.reserved, false);
            self.lock.unlock();
            stats.staged_writes_in_flight.sub(1);
        }
        const state = try self.createState();
        state.* = valueHash(value);
        writeRecord(self.image, placed.rec_off, key.bytes(), value, generation, fencing, state);
        if (self.backing.sync) |s| s(self.backing.ctx, placed.rec_off, stride(KEY_LEN, value.len));
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.allocCellLocked();
        cell.* = .{ .refs = .init(1), .extent = placed, .key = key.bytes().*, .hash = state, .indexed_seq = self.indexed_seq.fetchAdd(1, .release) };
        const gop = self.index.getOrPutAssumeCapacity(key.bytes().*);
        if (gop.found_existing) {
            const old = gop.value_ptr.*;
            gop.value_ptr.* = cell;
            if (old.synced or self.coveredByRunningSync(old)) {
                // The superseded SYNCED copy stays whole until this one is
                // durable (`Cell` doc): the deferral's reference, then the
                // retire.
                old.hold = .superseded;
                old.hold_seq = generation;
                _ = old.refs.fetchAdd(1, .monotonic);
                stats.staging_superseded_held.add(1);
                stats.staging_superseded_held_bytes.add(old.extent.end() - old.extent.rec_off);
            }
            self.retireLocked(old, old.hold != .superseded);
        } else {
            gop.value_ptr.* = cell;
        }
        stats.staged_writes_in_flight.sub(1);
    }

    /// §J.2: grow the resident record under `key` IN PLACE — `data` lands at
    /// value offset `at ≥ val_len` (the gap zero-filled), inside the slot's
    /// reserved slack — then the record's header is rewritten into the slot
    /// that is NOT current with the new length, `generation` and checksum.
    /// The old slot stays intact and valid for the old length until the
    /// new one is complete, so a kill between any two stores leaves the old
    /// record to recovery; the RAM cell is REPLACED (a reader's lease on the
    /// old cell keeps seeing the old length), the old one shadowed. The
    /// caller serializes writers per key (the ino's layout lock).
    pub fn appendInPlace(self: *Segment, key: StagingKey, at: u64, data: []const u8, expect_generation: u64, generation: u64, fencing: u64) AppendError!void {
        assert(data.len != 0);
        // Phase 1: validate under the lock and hold a lease across the write.
        const old: *Cell, const ext: Extent = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const cell = self.index.get(key.bytes().*) orelse return error.NotResident;
            const e = cell.extent;
            if (e.generation != expect_generation) return error.Stale;
            if (at < e.val_len or at + data.len > e.valueCapacity()) return error.NoRoom;
            _ = cell.refs.fetchAdd(1, .monotonic);
            break :blk .{ cell, e };
        };
        const voff: usize = @intCast(ext.valOff());
        const new_len: usize = @intCast(at + data.len);
        // The gap and the bytes, beyond the old length (the old checksum
        // still verifies over [0, val_len)).
        @memset(self.image[voff + ext.val_len .. voff + @as(usize, @intCast(at))], 0);
        @memcpy(self.image[voff + @as(usize, @intCast(at)) ..][0..data.len], data);
        // The checksum state extends by exactly the new bytes (a recovered
        // record hashes its value once, here).
        const state = try self.createState();
        errdefer self.destroyState(state);
        state.* = if (old.hash) |h| h.* else valueHash(self.image[voff .. voff + ext.val_len]);
        state.update(self.image[voff + ext.val_len .. voff + new_len]);
        const slot = ext.slot.other();
        writeHeader(self.image, ext.rec_off, slot, key.bytes(), new_len, generation, fencing, state);
        if (self.backing.sync) |s| {
            s(self.backing.ctx, ext.rec_off, RECORD_ALIGN);
            const tail_lo = derive.alignDown(ext.valOff() + ext.val_len, RECORD_ALIGN);
            const tail_hi = derive.alignUp(ext.valOff() + new_len, RECORD_ALIGN);
            s(self.backing.ctx, tail_lo, tail_hi - tail_lo);
        }
        // Phase 2: the RAM flip.
        self.lock.lock();
        defer self.lock.unlock();
        if (old.retired.load(.acquire)) {
            // Unlinked or promoted meanwhile: the bytes we grew belong to a
            // record that is gone; drop the lease (the last one reclaims).
            self.destroyState(state);
            if (old.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(old);
            return error.Stale;
        }
        const cell = self.allocCellLocked();
        var ne = ext;
        ne.val_len = @intCast(new_len);
        ne.generation = generation;
        ne.slot = slot;
        // The record keeps the bytes a sync covered: the successor is as
        // synced as the copy it grew (its tail is not, but the old header's
        // slot still names the synced image whole).
        cell.* = .{ .refs = .init(1), .extent = ne, .key = key.bytes().*, .hash = state, .indexed_seq = self.indexed_seq.fetchAdd(1, .release), .synced = old.synced };
        self.index.putAssumeCapacity(key.bytes().*, cell);
        old.shadowed = true;
        old.retired.store(true, .release);
        self.linkRetiredLocked(old);
        // The index's reference and our lease leave; the last one out frees
        // the cell alone (shadowed: the record lives on under the successor).
        if (old.refs.fetchSub(2, .acq_rel) == 2) self.reclaimCellLocked(old);
    }

    /// Remove `key` iff present (and, when `expect_generation` is given,
    /// iff the resident copy carries it — the promotion's generation gate:
    /// a re-stage that landed since keeps its entry). True iff removed. The
    /// key leaves the ring; with a cover seam bound its copies — the
    /// resident one and every superseded one — DEPART: whole on the
    /// segment until the round that starts after this instant lands
    /// (`cover`; the `Cell` doc). Without one they are released at once.
    pub fn remove(self: *Segment, key: StagingKey, expect_generation: ?u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return false;
        if (expect_generation) |g| if (cell.extent.generation != g) return false;
        _ = self.index.remove(key.bytes().*);
        // Nothing to protect without a seam, or when no copy of the key was
        // ever synced (a superseded copy is held only when synced): release
        // at once.
        if (self.cover_seam == null or !(cell.synced or self.coveredByRunningSync(cell) or self.supersededSyncedLocked(key.bytes()))) {
            self.retireLocked(cell, true);
            self.releaseSupersededLocked(key.bytes(), std.math.maxInt(u64));
            return true;
        }
        stats.staging_departures_deferred.inc();
        const seq = self.cover_started + 1;
        // The deferral's reference, then the retire (no tombstone).
        _ = cell.refs.fetchAdd(1, .monotonic);
        self.departLocked(cell, seq);
        self.retireLocked(cell, false);
        // A superseded copy's deferral reference carries over to its
        // departure: the sync that would have released it will never come.
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) {
            if (c.hold != .superseded or !std.mem.eql(u8, &c.key, key.bytes())) continue;
            stats.staging_superseded_held.sub(1);
            stats.staging_superseded_held_bytes.sub(c.extent.end() - c.extent.rec_off);
            self.departLocked(c, seq);
        }
        return true;
    }

    fn supersededSyncedLocked(self: *Segment, key: *const [KEY_LEN]u8) bool {
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) {
            if (c.hold == .superseded and c.synced and std.mem.eql(u8, &c.key, key)) return true;
        }
        return false;
    }

    fn departLocked(_: *Segment, cell: *Cell, seq: u64) void {
        cell.hold = .departed;
        cell.hold_seq = seq;
        stats.staging_departed_held.add(1);
        stats.staging_departed_held_bytes.add(cell.extent.end() - cell.extent.rec_off);
    }

    /// Departed copies held now (the dismount's and the tests' observable).
    pub fn departedHeld(self: *Segment) u32 {
        self.lock.lock();
        defer self.lock.unlock();
        return self.departedHeldLocked();
    }

    fn departedHeldLocked(self: *Segment) u32 {
        var n: u32 = 0;
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) n += @intFromBool(c.hold == .departed);
        return n;
    }

    /// The departure law's round (module doc): ONE `cover` run — the meta
    /// barrier past every publish committed before this call — then every
    /// departed copy that left before the round started is released
    /// (tombstoned, punched, its cell back in the pool). A copy that
    /// departed WHILE the round ran waits for the next one. Nothing held
    /// ⇒ no round. A failed round releases nothing (the next one retries);
    /// the error is the caller's to count.
    pub fn cover(self: *Segment, io: Io) DurableError!void {
        const seam = self.cover_seam orelse return;
        const ticket: u64 = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.departedHeldLocked() == 0) return;
            self.cover_started += 1;
            break :blk self.cover_started;
        };
        try seam.run(seam.ctx, io);
        stats.staging_cover_rounds.inc();
        self.lock.lock();
        defer self.lock.unlock();
        var cur = self.retired_head;
        while (cur) |c| {
            cur = c.next_retired;
            if (c.hold != .departed or c.hold_seq > ticket) continue;
            c.hold = .none;
            stats.staging_departed_held.sub(1);
            stats.staging_departed_held_bytes.sub(c.extent.end() - c.extent.rec_off);
            stats.staging_departed_released.inc();
            self.tombstoneLocked(c.extent.rec_off);
            if (c.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(c);
        }
    }

    /// Retire a cell: onto the retired list, out of the index's reference;
    /// `tombstone` = BOTH slots' magic zeroed so a recovery scan never
    /// adopts the copy (a superseded or departed copy keeps its magic until
    /// its release — `Cell` doc). The last reference reclaims the bytes.
    /// release pairs with `Lease.release`'s acq_rel.
    fn retireLocked(self: *Segment, cell: *Cell, tombstone_now: bool) void {
        if (tombstone_now) self.tombstoneLocked(cell.extent.rec_off);
        cell.retired.store(true, .release);
        self.linkRetiredLocked(cell);
        if (cell.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(cell);
    }

    fn tombstoneLocked(self: *Segment, rec_off: u64) void {
        self.tombstone(rec_off);
        if (self.backing.sync) |s| s(self.backing.ctx, rec_off, RECORD_ALIGN);
    }

    fn tombstone(self: *Segment, rec_off: u64) void {
        @memset(self.image[@intCast(rec_off)..][0..4], 0);
        @memset(self.image[@intCast(rec_off + HEADER_B_OFF)..][0..4], 0);
    }

    fn linkRetiredLocked(self: *Segment, cell: *Cell) void {
        assert(cell.prev_retired == null and cell.next_retired == null);
        cell.next_retired = self.retired_head;
        if (self.retired_head) |h| h.prev_retired = cell;
        self.retired_head = cell;
    }

    fn unlinkRetiredLocked(self: *Segment, cell: *Cell) void {
        if (cell.prev_retired) |p| p.next_retired = cell.next_retired else self.retired_head = cell.next_retired;
        if (cell.next_retired) |n| n.prev_retired = cell.prev_retired;
        cell.prev_retired = null;
        cell.next_retired = null;
    }

    /// Release the superseded copies of `key` whose successor generation
    /// is at most `up_to` (the copy a sync just made durable, or every one
    /// when the key leaves): tombstoned now, the deferral's reference
    /// dropped — the last reference reclaims.
    fn releaseSupersededLocked(self: *Segment, key: *const [KEY_LEN]u8, up_to: u64) void {
        var cur = self.retired_head;
        while (cur) |c| {
            cur = c.next_retired;
            if (c.hold != .superseded or c.hold_seq > up_to or !std.mem.eql(u8, &c.key, key)) continue;
            self.releaseOneSupersededLocked(c);
        }
    }

    /// `syncAll`'s release: every superseded copy whose key is gone, or
    /// whose current copy was indexed before the sync began (`start_seq`),
    /// is durable-or-gone and lets go.
    fn releaseSupersededCoveredLocked(self: *Segment, start_seq: u64) void {
        var cur = self.retired_head;
        while (cur) |c| {
            cur = c.next_retired;
            if (c.hold != .superseded) continue;
            if (self.index.get(c.key)) |live_cell| if (live_cell.indexed_seq >= start_seq) continue;
            self.releaseOneSupersededLocked(c);
        }
    }

    fn releaseOneSupersededLocked(self: *Segment, c: *Cell) void {
        c.hold = .none;
        stats.staging_superseded_held.sub(1);
        stats.staging_superseded_held_bytes.sub(c.extent.end() - c.extent.rec_off);
        stats.staging_superseded_released.inc();
        self.tombstoneLocked(c.extent.rec_off);
        if (c.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(c);
    }

    fn reclaimCell(self: *Segment, cell: *Cell) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.reclaimCellLocked(cell);
    }

    fn reclaimCellLocked(self: *Segment, cell: *Cell) void {
        assert(cell.hold == .none);
        const e = cell.extent;
        self.unlinkRetiredLocked(cell);
        // A shadowed cell's bytes live on under its successor: free the cell alone.
        if (!cell.shadowed) {
            if (self.backing.reclaim) |r| r(self.backing.ctx, e.rec_off, e.end() - e.rec_off);
            self.markLocked(e.rec_off, e.end() - e.rec_off, false);
        }
        if (cell.hash) |h| self.destroyState(h);
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

    /// Take or return the grains of `[off, off + len)` in the live set.
    fn markLocked(self: *Segment, off: u64, len: u64, occupied: bool) void {
        assert(derive.isAligned(off, RECORD_ALIGN) and derive.isAligned(len, RECORD_ALIGN));
        var g: usize = @intCast(off / RECORD_ALIGN);
        const end: usize = @intCast((off + len) / RECORD_ALIGN);
        while (g < end) : (g += 1) {
            const mask = @as(u64, 1) << @intCast(g % 64);
            if (occupied) {
                assert(self.occupied[g / 64] & mask == 0);
                self.occupied[g / 64] |= mask;
            } else {
                assert(self.occupied[g / 64] & mask != 0);
                self.occupied[g / 64] &= ~mask;
            }
        }
    }

    fn grainOccupied(self: *const Segment, g: usize) bool {
        return self.occupied[g / 64] & (@as(u64, 1) << @intCast(g % 64)) != 0;
    }

    /// First fit over the live set — the grain bitmap — for `need` bytes:
    /// the first clear run at/after the cursor, else the first one starting
    /// below it (a run may straddle the cursor); null when nothing fits.
    fn findHoleLocked(self: *Segment, need: u64) ?u64 {
        const grains: usize = @intCast(need / RECORD_ALIGN);
        const total: usize = @intCast(self.image.len / RECORD_ALIGN);
        const cursor: usize = @intCast(derive.alignUp(self.write_offset, RECORD_ALIGN) / RECORD_ALIGN);
        if (self.findRunLocked(cursor, total, grains, total)) |g| return @as(u64, g) * RECORD_ALIGN;
        if (self.findRunLocked(0, cursor, grains, total)) |g| return @as(u64, g) * RECORD_ALIGN;
        return null;
    }

    /// The first run of `grains` clear bits that STARTS in `[lo, hi)` and
    /// ends at or before `total`; a word of ones is skipped whole, a word of
    /// zeros counted whole.
    fn findRunLocked(self: *const Segment, lo: usize, hi: usize, grains: usize, total: usize) ?usize {
        var g = lo;
        var run: usize = 0;
        while (g < total) {
            // The run's start must lie below `hi`.
            if (g - run >= hi) return null;
            if (g % 64 == 0 and g + 64 <= total) {
                const word = self.occupied[g / 64];
                if (word == std.math.maxInt(u64)) {
                    run = 0;
                    g += 64;
                    continue;
                }
                if (word == 0) {
                    run += 64;
                    g += 64;
                    if (run >= grains) {
                        const start = g - run;
                        return if (start < hi) start else null;
                    }
                    continue;
                }
            }
            if (self.grainOccupied(g)) run = 0 else run += 1;
            g += 1;
            if (run == grains) {
                const start = g - run;
                return if (start < hi) start else null;
            }
        }
        return null;
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
    /// grain; a tombstone or a zero grain steps one grain; a HOLE is
    /// skipped whole (`Backing.nextData`). Same-key copies: the higher
    /// generation wins (a crash between the replace's phases left both),
    /// equal ⇒ the later offset. `write_offset` = the max end.
    pub fn recover(self: *Segment) RecoverReport {
        self.lock.lock();
        defer self.lock.unlock();
        assert(self.index.count() == 0);
        var rep: RecoverReport = .{};
        var off: u64 = 0;
        var max_end: u64 = 0;
        while (off + RECORD_HEADER_LEN <= self.image.len) {
            if (self.backing.nextData) |nd| {
                const data = nd(self.backing.ctx, off) orelse break;
                // Data is reported at the host's block grain; the record
                // grain divides it, so the scan resumes on a grain inside it.
                off = @max(off, data - data % RECORD_ALIGN);
                if (off + RECORD_HEADER_LEN > self.image.len) break;
            }
            // A magic in either slot names a candidate record; the current
            // header is the valid slot with the higher generation (an
            // append's half-written alternate slot loses to the intact one).
            const magic_a = std.mem.eql(u8, self.image[@intCast(off)..][0..4], &SEGMENT_MAGIC);
            const magic_b = off + HEADER_B_OFF + 4 <= self.image.len and std.mem.eql(u8, self.image[@intCast(off + HEADER_B_OFF)..][0..4], &SEGMENT_MAGIC);
            const cur = currentHeader(self.image, off) orelse {
                if (magic_a or magic_b) rep.torn += 1;
                off += RECORD_ALIGN;
                continue;
            };
            const h = cur.h;
            if (h.key_len != KEY_LEN) {
                rep.torn += 1;
                off += RECORD_ALIGN;
                continue;
            }
            const e: Extent = .{ .rec_off = off, .key_len = h.key_len, .val_len = h.val_len, .generation = h.generation, .reserved = stride(h.key_len, h.val_len), .slot = cur.slot };
            const k: [KEY_LEN]u8 = self.image[@intCast(off + RECORD_HEADER_LEN)..][0..KEY_LEN].*;
            const gop = self.index.getOrPutAssumeCapacity(k);
            if (gop.found_existing) {
                const old = gop.value_ptr.*;
                // Later offset wins a tie: the scan is ascending.
                if (h.generation >= old.extent.generation) {
                    gop.value_ptr.* = self.allocCellLocked();
                    gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k, .synced = true };
                    self.markLocked(e.rec_off, e.reserved, true);
                    self.retireLocked(old, true);
                } else {
                    self.tombstone(off);
                    if (self.backing.reclaim) |r| r(self.backing.ctx, off, e.end() - off);
                }
                rep.superseded += 1;
            } else {
                gop.value_ptr.* = self.allocCellLocked();
                gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k, .synced = true };
                self.markLocked(e.rec_off, e.reserved, true);
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

    /// Every resident key with its footprint (the ledger seed, the dismount
    /// and syncfs passes, fsck), sized AND filled under the ONE lock hold:
    /// a caller that sized by `live()` and then filled met a stage landing
    /// in between (syncfs under four fsstress writers) one entry past its
    /// buffer. The caller frees the slice.
    pub fn residentsAlloc(self: *Segment, gpa: Allocator) Allocator.Error![]Resident {
        self.lock.lock();
        defer self.lock.unlock();
        const out = try gpa.alloc(Resident, self.index.count());
        var n: usize = 0;
        var it = self.index.valueIterator();
        while (it.next()) |c| {
            const e = c.*.extent;
            out[n] = .{ .key = std.mem.bytesToValue(StagingKey, &c.*.key), .cost = stride(e.key_len, e.val_len), .generation = e.generation };
            n += 1;
        }
        assert(n == out.len);
        return out;
    }

    /// Extent order by offset (the tests' no-overlap law).
    pub fn extentLess(_: void, a: Extent, b: Extent) bool {
        return a.rec_off < b.rec_off;
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

/// A byte range of the image.
pub const Span = struct { off: u64, len: u64 };

/// The fsync law's batcher (module doc; the journal lane's barrier law —
/// "answered once a sync that STARTED after this request completed"): a
/// request joins the PENDING batch (an intrusive FIFO of on-frame waiters
/// and the union span of their records); with no sync running it leads
/// that batch at once, else it parks. The leader issues ONE `durable` over
/// the batch's span, hands every member its result and, if requests
/// arrived meanwhile, makes the oldest of them the next leader (one wake
/// per waiter, no herd) — otherwise the batcher idles. A sync in flight
/// never covers a request that arrived after it started: the request's
/// record may have landed after the kernel's dirty walk. Nothing is
/// allocated: the waiter lives on the caller's frame; the lock guards the
/// lists alone and is never held across the sync.
pub const SyncCoalescer = struct {
    lock: jc.SpinLock = .{},
    /// The pending batch: requests no sync has started for.
    head: ?*Waiter = null,
    tail: ?*Waiter = null,
    lo: u64 = std.math.maxInt(u64),
    hi: u64 = 0,
    /// A leader is between taking a batch and handing it over; the batch
    /// it leads (`covers`).
    running: bool = false,
    run_lo: u64 = 0,
    run_hi: u64 = 0,
    run_start_seq: u64 = 0,

    pub const Waiter = struct {
        next: ?*Waiter = null,
        /// The tick word: 0 parked, 1 woken (`tick.wake`).
        state: std.atomic.Value(u32) = .init(0),
        /// Under the lock: pending → leader (with `batch`/`lo`/`hi`) or done
        /// (with `result`). Read after the wake's acquire.
        role: enum { pending, leader, done } = .pending,
        batch: ?*Waiter = null,
        lo: u64 = 0,
        hi: u64 = 0,
        /// The segment's index sequence when the batch was taken: a cell
        /// indexed before it was written before the sync began.
        start_seq: u64 = 0,
        /// The key whose record the span is (null: the whole image) — what
        /// the leader marks synced once the sync landed.
        key: ?[KEY_LEN]u8 = null,
        result: DurableError!void = {},
    };

    /// Whether the sync in flight covers `[off, end)` for a cell indexed at
    /// `indexed_seq` — a copy the leader is about to mark synced.
    pub fn covers(self: *SyncCoalescer, off: u64, end: u64, indexed_seq: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.running and indexed_seq < self.run_start_seq and off < self.run_hi and self.run_lo < end;
    }

    /// Pending requests (tests: the deterministic park observable).
    pub fn pending(self: *SyncCoalescer) u32 {
        self.lock.lock();
        defer self.lock.unlock();
        var n: u32 = 0;
        var cur = self.head;
        while (cur) |w| : (cur = w.next) n += 1;
        return n;
    }

    /// `span` durable; answers the index sequence the covering sync began
    /// at (`Waiter.start_seq`).
    pub fn request(self: *SyncCoalescer, seg: *Segment, io: Io, span: Span, key: ?StagingKey) DurableError!u64 {
        var w: Waiter = .{ .key = if (key) |k| k.bytes().* else null };
        self.lock.lock();
        if (self.tail) |t| t.next = &w else self.head = &w;
        self.tail = &w;
        self.lo = @min(self.lo, span.off);
        self.hi = @max(self.hi, span.off + span.len);
        if (!self.running) {
            self.running = true;
            self.takeBatchLocked(seg, &w);
            self.lock.unlock();
        } else {
            self.lock.unlock();
            // acquire (in `park`) pairs with the leader's `tick.wake`
            // release: `role`, `result` and the batch are visible.
            while (w.state.load(.acquire) == 0) _ = tick.parkUncancelable(io, &w.state, null);
            if (w.role == .done) {
                stats.staging_fsync_coalesced.inc();
                try w.result;
                return w.start_seq;
            }
            assert(w.role == .leader);
        }
        return self.lead(seg, io, &w);
    }

    /// The pending batch becomes `leader`'s; the pending list empties.
    fn takeBatchLocked(self: *SyncCoalescer, seg: *Segment, leader: *Waiter) void {
        leader.role = .leader;
        leader.batch = self.head;
        leader.lo = self.lo;
        leader.hi = self.hi;
        // acquire pairs with the index flip's release `fetchAdd`.
        leader.start_seq = seg.indexed_seq.load(.acquire);
        self.run_lo = leader.lo;
        self.run_hi = leader.hi;
        self.run_start_seq = leader.start_seq;
        self.head = null;
        self.tail = null;
        self.lo = std.math.maxInt(u64);
        self.hi = 0;
    }

    fn lead(self: *SyncCoalescer, seg: *Segment, io: Io, w: *Waiter) DurableError!u64 {
        const durable = seg.backing.durable.?;
        const t0 = clock.monotonicNs();
        const result = durable(seg.backing.ctx, io, w.lo, w.hi - w.lo);
        stats.staging_fsync_sync_ns.record(clock.monotonicNs() - t0);
        stats.staging_fsync_syncs.inc();
        stats.staging_fsync_bytes.add(w.hi - w.lo);
        if (result) |_| {
            // The copies this sync covered are synced — marked before the
            // batch is handed over, so a re-stage sees either the running
            // sync (`covers`) or the mark, never a gap.
            seg.lock.lock();
            var m = w.batch;
            while (m) |mw| : (m = mw.next) seg.markSyncedLocked(mw.key, w.start_seq);
            seg.lock.unlock();
        } else |_| {}
        self.lock.lock();
        var cur = w.batch;
        while (cur) |m| {
            // A woken member returns and its frame dies: read `next` first.
            const next = m.next;
            if (m != w) {
                m.result = result;
                m.start_seq = w.start_seq;
                m.role = .done;
                tick.wake(io, &m.state);
            }
            cur = next;
        }
        if (self.head) |successor| {
            self.takeBatchLocked(seg, successor);
            tick.wake(io, &successor.state);
        } else {
            self.running = false;
        }
        self.lock.unlock();
        try result;
        return w.start_seq;
    }
};

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

/// The dismount pass's verdict on one resident (`PromotionHooks.dismount`).
pub const DismountVerdict = enum {
    /// Promoted: the entry left the ring onto its durable home.
    promoted,
    /// The promotion declined for a resource reason (no volume, the arm
    /// refused, a commit failed): the entry stays resident — never-wrong,
    /// the next mount adopts it.
    kept,
    /// The ino has no live inode record (absent, or `nlink == 0`): a
    /// corpse's custody, destroyed — never promoted.
    corpse,
    /// The ino lives but its layout does not name this record (kind not
    /// staged, another file id, no layout at all): residue of a crash
    /// between a stage or promotion and the metadata that names it; no
    /// read can reach it, so it is destroyed.
    unnamed,
};

/// The three promotion points reach the ONE primitive through this seam
/// (`data/pack.zig`'s prepare → commit); the ring never promotes inline.
pub const PromotionHooks = struct {
    ctx: ?*anyopaque = null,
    /// Pressure: enqueue `file_id` to the merge worker (the hot path itself
    /// never promotes).
    pressure: ?*const fn (ctx: ?*anyopaque, key: StagingKey) void = null,
    /// Promote now (fsync, syncfs): true iff the entry left the ring. A
    /// record the layout does not name stays: under a live mount an
    /// unlinked-open file's custody is still read, and the dismount pass
    /// judges what a crash left.
    promote: ?*const fn (ctx: ?*anyopaque, key: StagingKey) bool = null,
    /// The dismount pass: promote, keep, or destroy a corpse's or an
    /// unnamed record (nothing is in flight: the session is closed).
    dismount: ?*const fn (ctx: ?*anyopaque, key: StagingKey) DismountVerdict = null,

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
    /// The ring is in a FULL episode (a placement found no hole and the
    /// caller took its own-block arm): one warning per episode, cleared by
    /// the next placement that lands — 291 identical lines per burst said
    /// nothing 1 did not.
    ring_full_episode: std.atomic.Value(bool) = .init(false),

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
        const out = try seg.residentsAlloc(self.gpa);
        defer self.gpa.free(out);
        for (out) |r| try self.charge(r.key.file_id, r.cost, RECOVERED_GENERATION);
    }
};

/// §3.4 Admission: the cost is the record's ring footprint; over the cap
/// ⇒ kick the promoter and refuse `StorageFull` (the caller's spill
/// escalation — never a byte recorded that the pool cannot hold); place
/// (two-phase, never destroying a live entry); ledger row `(cost,
/// generation)`; past the high-water enqueue the entry to the merge worker.
pub fn stageWrite(pool: *Pool, seg: *Segment, key: StagingKey, payload: []const u8, fencing: u64, slack: Slack) StageError!Staged {
    const padded = reserveFor(KEY_LEN, payload.len, slack);
    const prior_row = pool.costOf(key.file_id);
    const prior = prior_row orelse 0;
    if (pool.overCap(prior, padded)) {
        stats.staging_over_cap_refusals.inc();
        if (pool.hooks.pressure) |p| p(pool.hooks.ctx, key);
        return error.StorageFull;
    }
    const generation = pool.nextGeneration();
    seg.put(key, payload, generation, fencing, padded) catch |e| switch (e) {
        error.RingFull => {
            if (!pool.ring_full_episode.swap(true, .acq_rel)) log.warn("staging ring full: a {d} B record found no hole in a {d} B segment with {d} live entries — the write takes its own-block arm until the ring drains (counted staging_ring_full_refusals; never destroying a live entry)", .{ padded, seg.capacity(), seg.live() });
            return error.StorageFull;
        },
        error.OutOfMemory => return error.OutOfMemory,
    };
    pool.ring_full_episode.store(false, .release);
    if (prior_row != null) {
        // A re-stage under a resident key moved the image out of the ring
        // (the caller's RMW source) and back in.
        stats.staging_restages.inc();
        stats.staging_restage_bytes.add(2 * payload.len);
    }
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

/// §J.2: an extending write into the resident record's slack, in place —
/// a fresh generation (the promotion's identity probe sees the change), the
/// ledger row re-stamped at the same cost. `null` = not in place (no
/// resident copy, no slack for it, or the copy moved under the caller): the
/// caller re-stages the whole image. Never kicks the drain: the ring's
/// footprint did not grow.
pub fn appendStaged(pool: *Pool, seg: *Segment, key: StagingKey, at: u64, data: []const u8, fencing: u64) Allocator.Error!?Staged {
    const resident = pool.generationOf(key.file_id) orelse return null;
    const generation = pool.nextGeneration();
    seg.appendInPlace(key, at, data, resident, generation, fencing) catch |e| switch (e) {
        error.NotResident, error.Stale, error.NoRoom => return null,
        error.OutOfMemory => return error.OutOfMemory,
    };
    const cost = pool.costOf(key.file_id) orelse 0;
    try pool.charge(key.file_id, cost, generation);
    stats.staging_appends.inc();
    stats.staging_append_bytes.add(data.len);
    return .{ .generation = generation, .kicked = false };
}

/// The promotion released the ring entry (generation-gated: a re-stage that
/// landed since keeps its copy and its charge): the key leaves the ring
/// and the pool at once; its copies depart (`Segment.remove`) — the slot is
/// the ring's again at the cover round. True iff released.
pub fn releaseStaged(pool: *Pool, seg: *Segment, key: StagingKey, generation: u64) bool {
    if (!seg.remove(key, generation)) return false;
    _ = pool.credit(key.file_id, generation);
    return true;
}

// ---------------------------------------------------------------------------
// §3.5 The promotion points.
// ---------------------------------------------------------------------------

/// The dismount pass's verdicts, counted (the log line's numbers).
pub const DismountTally = struct { promoted: u64 = 0, promoted_bytes: u64 = 0, kept: u64 = 0, corpses: u64 = 0, unnamed: u64 = 0 };

/// The clean-unmount pass: every resident entry through the dismount hook
/// — promoted, kept (a resource decline leaves the entry resident —
/// never-wrong), or destroyed as a corpse's or an unnamed record (a clean
/// unmount leaves no custody for an inode that does not exist, and none
/// the metadata does not name) — then the caller seals the open packs
/// before "Dismount clean".
pub fn promoteAllAtDismount(gpa: Allocator, seg: *Segment, hooks: PromotionHooks) Allocator.Error!DismountTally {
    var tally: DismountTally = .{};
    const judge = hooks.dismount orelse return tally;
    const out = try seg.residentsAlloc(gpa);
    defer gpa.free(out);
    for (out) |r| switch (judge(hooks.ctx, r.key)) {
        .promoted => {
            tally.promoted += 1;
            tally.promoted_bytes += r.cost;
            stats.dismount_promoted_files.inc();
            stats.dismount_promoted_bytes.add(r.cost);
        },
        .kept => {
            tally.kept += 1;
            stats.dismount_promote_failures.inc();
        },
        .corpse => {
            tally.corpses += 1;
            stats.dismount_corpse_residents_destroyed.inc();
        },
        .unnamed => {
            tally.unnamed += 1;
            stats.dismount_unnamed_residents_destroyed.inc();
        },
    };
    return tally;
}

/// `SQUEEZEFS_FSYNC_PROMOTE_STAGED`: the alternative posture of the fsync
/// law — fsync of a staged file PROMOTES it (the bytes leave the ring for
/// the data volume, visible to every client; the data barrier covers the
/// block) instead of syncing its ring record in place; off (shipped) the
/// record stays staged and the ranged sync makes it durable.
pub const FsyncLever = struct {
    on: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map) FsyncLever {
        return .{ .on = knobs.boolKnob("SQUEEZEFS_FSYNC_PROMOTE_STAGED", env) };
    }
};

/// The fsync point: iff the lever is on, promote BEFORE the data barrier
/// (so the barrier covers the block). A failure never fails the fsync: the
/// entry stays resident and the ranged sync that follows makes it durable.
pub fn promoteAtFsync(lever: FsyncLever, hooks: PromotionHooks, key: StagingKey) void {
    if (!lever.on) return;
    const promote = hooks.promote orelse return;
    if (promote(hooks.ctx, key)) stats.fsync_promoted_files.inc();
}

/// The syncfs point: `promoteAtFsync` for EVERY resident entry (the lever
/// gates it the same way; a failure leaves the entry resident for the
/// whole-ring sync that follows).
pub fn promoteAllAtSyncfs(gpa: Allocator, lever: FsyncLever, seg: *Segment, hooks: PromotionHooks) Allocator.Error!void {
    if (!lever.on) return;
    if (hooks.promote == null) return;
    const out = try seg.residentsAlloc(gpa);
    defer gpa.free(out);
    for (out) |r| promoteAtFsync(lever, hooks, r.key);
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
/// v3: the v2 marker (sqzhash128's low 64 bits, 0003 §M) naming its OWNER —
/// the member's appender id and the incarnation's pid beside the `(node,
/// mount slot)` pair (0003 §T.1); a v1/v2 marker is Corrupt to this binary
/// and its ring is wiped loud.
pub const MARKER_VERSION: u16 = 3;
/// The marker names the previous set epoch too (an `add-meta` rebind in
/// flight).
pub const MARKER_FLAG_DUAL: u16 = 1 << 0;
const MARKER_FLAGS_KNOWN: u16 = MARKER_FLAG_DUAL;

/// 80 B, checksummed; compared as BYTES.
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
    appender_id_le: [4]u8,
    owner_pid_le: [4]u8,
    csum_le: [8]u8,
};

pub const MARKER_LEN: usize = @sizeOf(StagingMarkerWire);
const MARKER_CHECKSUM_OFF: usize = @offsetOf(StagingMarkerWire, "csum_le");

comptime {
    assert(MARKER_LEN == 80);
    assert(MARKER_CHECKSUM_OFF == 72);
}

/// The ring's identity: `hash64(set_uuid ‖ "staging")`. WHY: a ring binds to
/// ONE volume set for its life, and the set uuid is the one durable identity
/// every member's superblock carries (so a reformat — a new uuid — or a
/// different set never adopts a stale ring); the domain-separating suffix
/// keeps the value distinct from every other derivation of the same uuid
/// (`hash_seed` once was `hash64(uuid)`, 0002 row 42), so no two identities
/// can be forged from one another; hash64 is the format's ONE checksum.
/// Printed as 16 hex digits in logs and directory names; compared as a u64.
pub fn stagingIdentity(set_uuid: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update(&set_uuid);
    h.update("staging");
    return h.finalLow64();
}

pub const MarkerError = error{Corrupt};

pub const Marker = struct {
    set_uuid: [16]u8,
    set_epoch: u64,
    /// The owner (0003 §T.1): the member's `(node, mount slot)` — the mount
    /// slot IS the member scope (the root's directory name); the node token
    /// names the incarnation's boot — its appender id and the pid of the
    /// incarnation that stamped it (the refusal names them).
    node_token: u64,
    mount_slot: u32,
    appender_id: u32 = 0,
    owner_pid: u32 = 0,
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
            .appender_id_le = undefined,
            .owner_pid_le = undefined,
            .csum_le = @splat(0),
        };
        std.mem.writeInt(u16, &w.version_le, MARKER_VERSION, .little);
        std.mem.writeInt(u16, &w.flags_le, if (self.prev_set_epoch != null) MARKER_FLAG_DUAL else 0, .little);
        std.mem.writeInt(u64, &w.set_epoch_le, self.set_epoch, .little);
        std.mem.writeInt(u64, &w.node_token_le, self.node_token, .little);
        std.mem.writeInt(u32, &w.mount_slot_le, self.mount_slot, .little);
        std.mem.writeInt(u32, &w.content_version_le, self.content_version, .little);
        std.mem.writeInt(u64, &w.identity_le, self.identity(), .little);
        std.mem.writeInt(u64, &w.prev_set_epoch_le, self.prev_set_epoch orelse 0, .little);
        std.mem.writeInt(u32, &w.appender_id_le, self.appender_id, .little);
        std.mem.writeInt(u32, &w.owner_pid_le, self.owner_pid, .little);
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
            .appender_id = std.mem.readInt(u32, &w.appender_id_le, .little),
            .owner_pid = std.mem.readInt(u32, &w.owner_pid_le, .little),
            .content_version = std.mem.readInt(u32, &w.content_version_le, .little),
            .prev_set_epoch = if (flags & MARKER_FLAG_DUAL != 0) std.mem.readInt(u64, &w.prev_set_epoch_le, .little) else null,
        };
        if (std.mem.readInt(u64, &w.identity_le, .little) != m.identity()) return error.Corrupt;
        if (flags & MARKER_FLAG_DUAL == 0 and std.mem.readInt(u64, &w.prev_set_epoch_le, .little) != 0) return error.Corrupt;
        return m;
    }
};

/// What a bind found under the claim (0003 §T.1: the root's live claim is
/// held before its marker is read, so nothing here is a live owner's).
pub const BindVerdict = enum {
    /// No marker: a fresh root — stamp ours.
    absent,
    /// Our set, our epoch (or a dual marker naming it), our member: adopt
    /// the ring (a predecessor incarnation's residue is ours to recover)
    /// and restamp the owner.
    match,
    /// Our set and member under an older epoch, or a dual marker: adopt and
    /// restamp canonical (the KD-8 rebind path).
    upgrade,
    /// Our set, ANOTHER member's root (its owner dead — the claim proved
    /// it): REFUSE the mount loud; never adopted, never wiped — its
    /// residue is that member's successor's.
    foreign_owner,
    /// Another set, a newer epoch than ours, or an unreadable marker: wipe
    /// the segment tree and restamp.
    dead,
};

/// §3.1 `bind_staging_generation`'s ladder as a pure verdict, member-scoped
/// (0003 §T.1): the member is the mount slot (the root's name); a different
/// node token under the same slot is that member's next incarnation — a
/// reboot's successor — never a peer.
pub fn bindVerdict(found: ?Marker, ours: Marker) BindVerdict {
    const f = found orelse return .absent;
    if (f.identity() != ours.identity()) return .dead;
    const same_epoch = f.set_epoch == ours.set_epoch or (f.prev_set_epoch != null and f.prev_set_epoch.? == ours.set_epoch);
    if (!same_epoch and f.set_epoch > ours.set_epoch) return .dead;
    if (f.mount_slot != ours.mount_slot) return .foreign_owner;
    if (same_epoch and f.prev_set_epoch == null) return .match;
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

/// `hash64(key) % shards`.
pub fn shardOf(key: StagingKey, shards: u32) u32 {
    return @intCast(checksum.hash64(key.bytes()) % shards);
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
    const sqz_io = @import("../exec/sqz_io.zig");

    /// Plaintext user payload on passthrough volumes and every staged key
    /// in the headers: private to the daemon's uid.
    pub const DIR_MODE: linux.mode_t = 0o700;
    pub const FILE_MODE: linux.mode_t = 0o600;
    pub const SEGMENT_DIR = "staging_segment";
    pub const MARKER_NAME = ".squeezefs_staging";
    pub const MARKER_TMP = ".squeezefs_staging.tmp";

    pub const Error = error{ OpenFailed, MapFailed, GrowFailed, NameTooLong };

    /// `lseek(2)`'s hole-reporting whence (std's `SEEK` carries only
    /// SET/CUR/END).
    const SEEK_DATA: u32 = 3;

    pub const MappedSegment = struct {
        fd: linux.fd_t,
        map: []align(4096) u8,

        fn sync(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            const t0 = clock.monotonicNs();
            _ = linux.msync(self.map.ptr + @as(usize, @intCast(off)), @intCast(len), linux.MSF.ASYNC);
            stats.backing_sync_ns.record(clock.monotonicNs() - t0);
        }

        /// The fsync law's sync (`Backing.durable`): `IORING_OP_FSYNC` over
        /// the span on the caller's lane ring — the ranged `vfs_fsync_range`
        /// a shared mapping's `msync(MS_SYNC)` runs, with the fiber parked
        /// instead of the lane thread (`SqzIo.fsyncRange`). Measured on
        /// this host (`docs/evidence/rung-10-staged-fsync-durability.md`):
        /// the range costs what a whole-file `fdatasync` costs on a clean
        /// ring and never pays for another writer's unsynced pages where
        /// the host filesystem honours it (ext4: 1.0 vs 2.8 ms under 64
        /// dirty pages).
        fn durable(ctx: ?*anyopaque, io: Io, off: u64, len: u64) DurableError!void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            sqz_io.SqzIo.fsyncRange(io, self.fd, off, len) catch |e| {
                log.err("staging segment: the ranged sync of [{d}, +{d}) failed ({s}) — the staged bytes stay the page cache's (served, not power-cut durable); the fsync answers EIO", .{ off, len, @errorName(e) });
                return error.InputOutput;
            };
        }

        /// Punch the hole (the host reclaims the blocks) and drop the pages
        /// (the RSS the dead extent held); neither is correctness — a
        /// tombstoned record is never adopted — so their errnos are ignored.
        fn reclaim(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            const t0 = clock.monotonicNs();
            _ = linux.fallocate(self.fd, linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE, @intCast(off), @intCast(len));
            _ = linux.madvise(self.map.ptr + @as(usize, @intCast(off)), @intCast(len), linux.MADV.DONTNEED);
            stats.backing_reclaim_ns.record(clock.monotonicNs() - t0);
        }

        /// `lseek(SEEK_DATA)`: the next data offset, null at or past the
        /// end (`ENXIO`). A filesystem without hole reporting answers the
        /// query offset itself (every byte is data): the scan degrades to
        /// the grain walk.
        fn nextData(ctx: ?*anyopaque, off: u64) ?u64 {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            const rc = linux.lseek(self.fd, @intCast(off), SEEK_DATA);
            if (linux.errno(rc) != .SUCCESS) return null;
            const data: u64 = @intCast(rc);
            return if (data >= self.map.len) null else data;
        }

        pub fn backing(self: *MappedSegment) Backing {
            return .{ .ctx = self, .sync = sync, .reclaim = reclaim, .nextData = nextData, .durable = durable };
        }

        /// The census face of an `inspectSegment` view: hole-skipping
        /// only — no flush, no punch (the file is another member's).
        pub fn inspectBacking(self: *MappedSegment) Backing {
            return .{ .ctx = self, .nextData = nextData };
        }

        pub fn close(self: *MappedSegment) void {
            _ = linux.munmap(self.map.ptr, self.map.len);
            _ = linux.close(self.fd);
            self.* = undefined;
        }
    };

    fn zPath(buf: *[linux.PATH_MAX]u8, path: []const u8) error{NameTooLong}![:0]const u8 {
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
        return openSegmentIn(root_fd, SEGMENT_DIR, name, capacity);
    }

    /// `openSegment` under another segment dir of the same root (the NVMe
    /// read cache's `cache_segment/` shares the staging root's identity,
    /// modes and never-shrink law).
    pub fn openSegmentIn(root_fd: linux.fd_t, segment_dir: [:0]const u8, name: [:0]const u8, capacity: u64) Error!MappedSegment {
        assert(derive.isAligned(capacity, RECORD_ALIGN));
        const dir = linux.openat(root_fd, segment_dir, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
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

    /// A PRIVATE read-only view of `staging_segment/<name>` (`O_RDONLY`,
    /// `MAP_PRIVATE`): a recovery scan over it (the `staging list` census)
    /// tombstones superseded copies in its own pages only — never a byte of
    /// the member's file. Null when absent or empty.
    pub fn inspectSegment(root_fd: linux.fd_t, name: [:0]const u8) ?MappedSegment {
        const dir = linux.openat(root_fd, SEGMENT_DIR, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(dir) != .SUCCESS) return null;
        defer _ = linux.close(@intCast(dir));
        const rc = linux.openat(@intCast(dir), name, .{ .ACCMODE = .RDONLY, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return null;
        const fd: linux.fd_t = @intCast(rc);
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS or st.size == 0) {
            _ = linux.close(fd);
            return null;
        }
        const m = linux.mmap(null, @intCast(st.size), .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE }, fd, 0);
        if (linux.errno(m) != .SUCCESS) {
            _ = linux.close(fd);
            return null;
        }
        const ptr: [*]align(4096) u8 = @ptrFromInt(m);
        return .{ .fd = fd, .map = ptr[0..@intCast(st.size)] };
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

    /// A member's root under the format's cache path (0003 §T.1):
    /// `<cache path>/member-<mount slot as 8 hex>` — its marker, its staging
    /// segment and its NVMe read-cache segments. The mount slot is the
    /// member scope a mount presents (the manager's home-volume index, a
    /// joiner's mount-point derivation), stable across its incarnations.
    pub const MEMBER_DIR_PREFIX = "member-";
    const MEMBER_DIR_LEN = MEMBER_DIR_PREFIX.len + 8;

    pub fn memberRootPath(buf: []u8, container: []const u8, mount_slot: u32) error{NameTooLong}![]const u8 {
        return std.fmt.bufPrint(buf, "{s}/" ++ MEMBER_DIR_PREFIX ++ "{x:0>8}", .{ container, mount_slot }) catch error.NameTooLong;
    }

    /// The mount slot a member root's directory name carries; null for any
    /// other entry.
    pub fn memberSlotOf(name: []const u8) ?u32 {
        if (name.len != MEMBER_DIR_LEN or !std.mem.startsWith(u8, name, MEMBER_DIR_PREFIX)) return null;
        return std.fmt.parseInt(u32, name[MEMBER_DIR_PREFIX.len..], 16) catch null;
    }

    pub const BindError = error{ OpenFailed, NameTooLong, OwnerLive, ForeignOwner, MarkerFailed };

    /// What a refused bind found: the root's owner as its marker names it
    /// (null when the marker is absent or unreadable).
    pub const BindRefusal = struct { owner: ?Marker = null };

    /// A bound member root: the directory fd whose exclusive `flock` IS the
    /// live claim (held until `release`; the kernel drops it at the
    /// process's death, a `kill -9` included), its path, the marker found
    /// and the verdict that admitted it.
    pub const Bound = struct {
        fd: linux.fd_t,
        path_buf: [linux.PATH_MAX]u8,
        path_len: usize,
        verdict: BindVerdict,
        found: ?Marker,

        pub fn path(self: *const Bound) []const u8 {
            return self.path_buf[0..self.path_len];
        }

        pub fn release(self: *Bound) void {
            _ = linux.close(self.fd);
            self.* = undefined;
        }
    };

    /// The live claim: `flock(LOCK_EX | LOCK_NB)` on the root's fd; false =
    /// another open file description holds it (another process, or another
    /// bind of this one).
    fn claim(fd: linux.fd_t) BindError!bool {
        return switch (linux.errno(linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB))) {
            .SUCCESS => true,
            .AGAIN => false,
            else => error.OpenFailed,
        };
    }

    /// A marker file is present (an unreadable one binds `dead`, never
    /// `absent` — its ring is not ours to adopt).
    fn markerPresent(root_fd: linux.fd_t) bool {
        var st: linux.Statx = undefined;
        return linux.errno(linux.statx(root_fd, MARKER_NAME, linux.AT.SYMLINK_NOFOLLOW, .{ .TYPE = true }, &st)) == .SUCCESS;
    }

    /// The container (the format's cache path): created 0700, the mode
    /// re-asserted (plaintext payload under every member root).
    fn openContainer(path: []const u8) BindError!void {
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = try zPath(&zbuf, path);
        _ = linux.mkdir(z, DIR_MODE);
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        _ = linux.fchmod(@intCast(rc), DIR_MODE);
        _ = linux.close(@intCast(rc));
    }

    /// Bind THIS member's root under `container` (`ours.mount_slot` names
    /// it): two mounts of one set on one host bind two roots, never one.
    pub fn bindMember(pool: *uring_fs.Pool, container: []const u8, ours: Marker, segments: []const [:0]const u8, refusal: *BindRefusal) BindError!Bound {
        try openContainer(container);
        var buf: [linux.PATH_MAX]u8 = undefined;
        return bindRoot(pool, try memberRootPath(&buf, container, ours.mount_slot), ours, segments, refusal);
    }

    /// Bind `root` for `ours` (0003 §T.1): the live claim taken BEFORE the
    /// marker is read — a root another process holds is refused
    /// `OwnerLive`, never adopted; then the ladder — another member's dead
    /// root refused `ForeignOwner` (never adopted, never wiped), a dead ring
    /// wiped; ours stamped two-phase (the owner's pid and appender id).
    pub fn bindRoot(pool: *uring_fs.Pool, root: []const u8, ours: Marker, segments: []const [:0]const u8, refusal: *BindRefusal) BindError!Bound {
        refusal.* = .{};
        if (root.len > linux.PATH_MAX) return error.NameTooLong;
        const fd = openRoot(root) catch |e| return switch (e) {
            error.NameTooLong => error.NameTooLong,
            else => error.OpenFailed,
        };
        errdefer _ = linux.close(fd);
        if (!try claim(fd)) {
            refusal.owner = readMarker(pool, root) catch null;
            stats.staging_owner_live_refusals.inc();
            return error.OwnerLive;
        }
        const found = readMarker(pool, root) catch null;
        const verdict: BindVerdict = if (found == null and markerPresent(fd)) .dead else bindVerdict(found, ours);
        switch (verdict) {
            .absent, .match, .upgrade => {},
            .dead => wipeSegments(fd, segments),
            .foreign_owner => {
                refusal.owner = found;
                stats.staging_foreign_scope_refusals.inc();
                return error.ForeignOwner;
            },
        }
        writeMarker(pool, root, fd, ours) catch return error.MarkerFailed;
        var b: Bound = .{ .fd = fd, .path_buf = undefined, .path_len = root.len, .verdict = verdict, .found = found };
        @memcpy(b.path_buf[0..root.len], root);
        return b;
    }

    /// Is another process's live claim held on the member root at `path`
    /// (read off `/proc/locks` — the probe takes NO lock: a transient one
    /// would refuse a member's own bind racing it).
    pub fn ownerLive(path: []const u8) bool {
        return @import("../meta/guard/proc.zig").exclusiveLocker(path) != null; // a directory: udev never locks one, so no wait
    }

    /// The member roots under a container, straight off `getdents64`.
    pub const MemberIter = struct {
        fd: linux.fd_t,
        buf: [4096]u8 = undefined,
        len: usize = 0,
        off: usize = 0,

        /// Null when the container cannot be opened (absent: no roots).
        pub fn open(container: []const u8) ?MemberIter {
            var zbuf: [linux.PATH_MAX]u8 = undefined;
            const z = zPath(&zbuf, container) catch return null;
            const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .NOFOLLOW = true, .CLOEXEC = true }, 0);
            if (linux.errno(rc) != .SUCCESS) return null;
            return .{ .fd = @intCast(rc) };
        }

        /// The next member root's mount slot; null at the end.
        pub fn next(self: *MemberIter) ?u32 {
            while (true) {
                if (self.off >= self.len) {
                    const n = linux.getdents64(self.fd, &self.buf, self.buf.len);
                    if (linux.errno(n) != .SUCCESS or n == 0) return null;
                    self.len = n;
                    self.off = 0;
                }
                const d: *align(1) linux.dirent64 = @ptrCast(&self.buf[self.off]);
                self.off += d.reclen;
                const name = std.mem.sliceTo(@as([*:0]u8, @ptrCast(&d.name)), 0);
                if (memberSlotOf(name)) |slot| return slot;
            }
        }

        pub fn close(self: *MemberIter) void {
            _ = linux.close(self.fd);
            self.* = undefined;
        }
    };

    /// `statx(2)`'s `stx_blocks` unit: 512 B whatever the filesystem's
    /// block size (the kernel ABI's sector count).
    const STATX_BLOCK_BYTES: u64 = 512;

    /// The other members' roots under `container` with no live owner and
    /// ring bytes on disk (`segment` allocated past 0 blocks: a cleanly
    /// unmounted ring is all punched holes): each logged loud with its
    /// owner, the count published as `staging_member_residue_roots`. Never
    /// touched — that member's successor recovers it.
    pub fn residueCensus(pool: *uring_fs.Pool, container: []const u8, ours_slot: u32, segment: [:0]const u8) u32 {
        var it = MemberIter.open(container) orelse return 0;
        defer it.close();
        var n: u32 = 0;
        while (it.next()) |slot| {
            if (slot == ours_slot) continue;
            var rbuf: [linux.PATH_MAX]u8 = undefined;
            const root = memberRootPath(&rbuf, container, slot) catch continue;
            if (ownerLive(root)) continue;
            var sbuf: [linux.PATH_MAX:0]u8 = undefined;
            const seg_path = std.fmt.bufPrintSentinel(&sbuf, "{s}/{s}/{s}", .{ root, SEGMENT_DIR, segment }, 0) catch continue;
            var st: linux.Statx = undefined;
            if (linux.errno(linux.statx(linux.AT.FDCWD, seg_path.ptr, 0, .{ .BLOCKS = true }, &st)) != .SUCCESS or st.blocks == 0) continue;
            n += 1;
            const owner = readMarker(pool, root) catch null;
            if (owner) |o| {
                log.warn("staging root {s}: member slot {x:0>8} (appender {d}, node {x:0>16}, last pid {d}) holds {d} B of ring on disk and no live owner — its unpromoted custody is that member's: a mount presenting its member scope recovers it (the same mount point; the manager's for slot 0); nothing here touches it", .{ root, slot, o.appender_id, o.node_token, o.owner_pid, st.blocks * STATX_BLOCK_BYTES });
            } else {
                log.warn("staging root {s}: member slot {x:0>8} holds {d} B of ring on disk, no live owner and no readable marker — nothing here touches it", .{ root, slot, st.blocks * STATX_BLOCK_BYTES });
            }
        }
        stats.staging_member_residue_roots.set(n);
        return n;
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

test "segment record v3: 40 B header {SQZS, version 3, flags 0, key_len, val_len, generation, fencing, csum} ‖ key ‖ (slot B cleared) pad to 4 KiB ‖ value; the checksum covers header+key+value; a flipped payload byte is torn; the append slack reserves a quarter of the image (one grain at least) — §J.2's chain bound is 11" {
    try testing.expectEqual(@as(u64, 4096), valueOffset(KEY_LEN));
    try testing.expectEqual(@as(u64, 4096 + 100), footprint(KEY_LEN, 100));
    try testing.expectEqual(@as(u64, 8192), stride(KEY_LEN, 100));
    try testing.expectEqual(@as(u64, 8192), stride(KEY_LEN, 4096));
    try testing.expectEqual(@as(u64, 12288), stride(KEY_LEN, 4097));
    // The slack law (tie): max(GRAIN, V/4); the reserve is the footprint +
    // slack on the grain.
    try testing.expectEqual(@as(u64, 4096), appendSlack(100));
    try testing.expectEqual(@as(u64, 4096), appendSlack(16384));
    try testing.expectEqual(@as(u64, 1 << 20), appendSlack(4 << 20));
    try testing.expectEqual(@as(u64, 8192), reserveFor(KEY_LEN, 100, .none));
    try testing.expectEqual(@as(u64, 12288), reserveFor(KEY_LEN, 100, .append));
    try testing.expectEqual(@as(u64, 4096 + (4 << 20) + (1 << 20)), reserveFor(KEY_LEN, 4 << 20, .append));
    // The chain (tie, §J.2): re-stages at V, 1.25V, … each move 2V and buy
    // V/4 of appends; over the chain from 4 KiB to 4 MiB bytes moved ÷
    // appended stays under the bound, and the bound is `1 + 2(1 + ¼)/¼`.
    var v_img: u64 = 4096;
    var moved: u64 = 0;
    var appended: u64 = 0;
    while (v_img < (4 << 20)) {
        const slack = appendSlack(v_img);
        appended += slack; // in place, one byte moved per byte
        moved += slack;
        v_img += slack;
        moved += 2 * v_img; // the re-stage when the slack is exhausted
    }
    try testing.expect(moved <= STAGED_APPEND_MOVED_BOUND * appended);
    try testing.expect(moved > (STAGED_APPEND_MOVED_BOUND - 2) * appended); // the bound is tight, not slack
    try testing.expectEqual(@as(u64, 1 + 2 * (4 + 1)), STAGED_APPEND_MOVED_BOUND);
    var image: [16384]u8 = @splat(0xEE);
    const k = tkey(1, 77);
    const v: [100]u8 = @splat(0x5A);
    const vst = valueHash(&v);
    writeRecord(&image, 4096, k.bytes(), &v, 9, 0xF0F0, &vst);
    try testing.expectEqualSlices(u8, "SQZS", image[4096..4100]);
    try testing.expectEqualSlices(u8, &.{ 3, 0, 0, 0 }, image[4100..4104]);
    try testing.expect(readHeaderAt(&image, 4096, .b) == null); // slot B cleared at the put
    try testing.expectEqual(HeaderSlot.a, currentHeader(&image, 4096).?.slot);
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
    try r.seg.put(k, &a, 1, 0, 0);
    try testing.expectEqual(@as(usize, 1), r.seg.live());
    try testing.expectEqual(@as(?u64, 1), r.seg.residentGeneration(k));
    const lease = r.seg.get(k).?;
    try testing.expectEqualSlices(u8, &a, lease.bytes());
    // Replace while the lease is held: the index flips, the old bytes stay.
    try r.seg.put(k, &b, 2, 0, 0);
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

test "in-place append (§J.2): the appended bytes land in the reserved slack and the ALTERNATE header slot carries the new length/generation/checksum (A, B, A, …); a reader's lease across the append keeps the old length; a stale generation, a non-extending write and an exhausted slack refuse (the caller re-stages); recovery takes the valid slot with the higher generation and a torn alternate slot leaves the old record intact; a removed record's both slots are tombstoned; the pool's ledger re-stamps the generation at the same cost" {
    const gpa = testing.allocator;
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    var seg = r.seg;
    const k = tkey(3, 33);
    const v: [5000]u8 = @splat('v');
    // Reserved with append slack: 4096 + 5000 + max(4096, 1250) → 16384 (capacity 12288).
    try seg.put(k, &v, 1, 0, reserveFor(KEY_LEN, v.len, .append));
    const l0 = seg.get(k).?;
    try testing.expectEqual(@as(u64, 16384), l0.cell.extent.reserved);
    try testing.expectEqual(HeaderSlot.a, l0.cell.extent.slot);
    // Append 100 bytes at the end: slot B becomes current; the old lease still sees 5000.
    const a1: [100]u8 = @splat('1');
    try seg.appendInPlace(k, 5000, &a1, 1, 2, 0);
    try testing.expectEqual(@as(usize, 5000), l0.bytes().len);
    try testing.expect(l0.cell.retired.load(.acquire) and l0.cell.shadowed);
    const l1 = seg.get(k).?;
    try testing.expectEqual(@as(usize, 5100), l1.bytes().len);
    try testing.expectEqual(@as(u64, 2), l1.generation());
    try testing.expectEqual(HeaderSlot.b, l1.cell.extent.slot);
    try testing.expectEqualSlices(u8, &a1, l1.bytes()[5000..5100]);
    try testing.expect(std.mem.allEqual(u8, l1.bytes()[0..5000], 'v'));
    try testing.expectEqual(l0.cell.extent.rec_off, l1.cell.extent.rec_off); // in place
    try testing.expectEqual(@as(usize, 1), seg.live());
    // The old cell frees alone when its lease leaves (no tombstone: both slots still name the record).
    const rec_off = l1.cell.extent.rec_off;
    l0.release();
    try testing.expect(currentHeader(r.image, rec_off) != null);
    try testing.expectEqual(HeaderSlot.b, currentHeader(r.image, rec_off).?.slot);
    // A gap append (at past the end) zero-fills; the slot alternates back to A.
    const a2: [8]u8 = @splat('2');
    try seg.appendInPlace(k, 5200, &a2, 2, 3, 0);
    const l2 = seg.get(k).?;
    try testing.expectEqual(@as(usize, 5208), l2.bytes().len);
    try testing.expect(std.mem.allEqual(u8, l2.bytes()[5100..5200], 0));
    try testing.expectEqualSlices(u8, &a2, l2.bytes()[5200..]);
    try testing.expectEqual(HeaderSlot.a, l2.cell.extent.slot);
    l1.release();
    // Refusals: a stale generation; a write inside the image; past the slack.
    try testing.expectError(error.Stale, seg.appendInPlace(k, 5208, &a2, 2, 4, 0));
    try testing.expectError(error.NoRoom, seg.appendInPlace(k, 100, &a2, 3, 4, 0));
    const big: [8000]u8 = @splat('b');
    try testing.expectError(error.NoRoom, seg.appendInPlace(k, 5208, &big, 3, 4, 0));
    try testing.expectError(error.NotResident, seg.appendInPlace(tkey(9, 9), 0, &a2, 1, 4, 0));
    try testing.expectEqual(@as(usize, 5208), l2.bytes().len); // nothing of a refusal landed
    try testing.expectEqual(@as(u64, 3), seg.residentGeneration(k).?);
    l2.release();
    // Recovery over the image: the higher-generation slot (A, gen 3) wins at the appended length.
    var ext: [8]Extent = undefined;
    _ = seg.liveExtents(&ext);
    seg.deinit();
    var fresh = try Segment.init(gpa, r.image, .none);
    const rep = fresh.recover();
    try testing.expectEqual(@as(u64, 1), rep.adopted);
    try testing.expectEqual(@as(u64, 0), rep.torn);
    const lr = fresh.get(k).?;
    try testing.expectEqual(@as(usize, 5208), lr.bytes().len);
    try testing.expectEqual(@as(u64, 3), lr.generation());
    try testing.expectEqual(stride(KEY_LEN, 5208), lr.cell.extent.reserved); // the slack is RAM-only knowledge
    lr.release();
    fresh.deinit();
    // A kill mid-append: the alternate slot (B, gen 4) half-written — a bad
    // checksum — loses to the intact A; the old length stands, nothing torn
    // is adopted, the record is NOT counted torn (one slot verifies).
    const hst = valueHash(r.image[@intCast(rec_off + valueOffset(KEY_LEN))..][0..5300]);
    writeHeader(r.image, rec_off, .b, k.bytes(), 5300, 4, 0, &hst);
    r.image[@intCast(rec_off + HEADER_B_OFF + CHECKSUM_OFF)] ^= 0xFF;
    var again = try Segment.init(gpa, r.image, .none);
    const rep2 = again.recover();
    try testing.expectEqual(@as(u64, 1), rep2.adopted);
    try testing.expectEqual(@as(u64, 0), rep2.torn);
    const la = again.get(k).?;
    try testing.expectEqual(@as(usize, 5208), la.bytes().len);
    try testing.expectEqual(@as(u64, 3), la.generation());
    la.release();
    // Remove: both slots' magic zeroed; a rescan adopts nothing.
    try testing.expect(again.remove(k, 3));
    try testing.expect(isTombstone(r.image, rec_off));
    try testing.expect(std.mem.allEqual(u8, r.image[@intCast(rec_off + HEADER_B_OFF)..][0..4], 0));
    again.deinit();
    var third = try Segment.init(gpa, r.image, .none);
    defer third.deinit();
    try testing.expectEqual(@as(u64, 0), third.recover().adopted);
    // The pool face: appendStaged re-stamps the ledger's generation at the
    // same cost, counts the append; a missing row is null.
    var pool = Pool.init(gpa, 1 << 20, .none);
    defer pool.deinit();
    const s1 = try stageWrite(&pool, &third, k, &v, 0, .append);
    const cost = pool.costOf(k.file_id).?;
    const ap0 = stats.staging_appends.load();
    const ab0 = stats.staging_append_bytes.load();
    const s2 = (try appendStaged(&pool, &third, k, 5000, &a1, 0)).?;
    try testing.expect(s2.generation > s1.generation);
    try testing.expectEqual(s2.generation, pool.generationOf(k.file_id).?);
    try testing.expectEqual(cost, pool.costOf(k.file_id).?);
    try testing.expectEqual(ap0 + 1, stats.staging_appends.load());
    try testing.expectEqual(ab0 + 100, stats.staging_append_bytes.load());
    try testing.expect((try appendStaged(&pool, &third, tkey(8, 8), 0, &a1, 0)) == null);
    // A re-stage under the resident key counts 2 × the image moved.
    const rs0 = stats.staging_restages.load();
    const rb0 = stats.staging_restage_bytes.load();
    _ = try stageWrite(&pool, &third, k, &v, 0, .none);
    try testing.expectEqual(rs0 + 1, stats.staging_restages.load());
    try testing.expectEqual(rb0 + 2 * v.len, stats.staging_restage_bytes.load());
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
        r.seg.put(tkey(@intCast(i), i), v, i + 1, 0, 0) catch |e| {
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
    try testing.expectError(error.RingFull, r.seg.put(tkey(1, 1), huge, 1, 0, 0));

    // A full ring of 2-grain records: the 9th is a loud refusal that
    // destroys nothing; removing the 4th opens the ONE hole, and the next
    // put lands exactly there (first fit over the wrapped candidates).
    var f = try ramSegment(gpa, 16 * 4096);
    defer gpa.free(f.image);
    defer f.seg.deinit();
    const ten: [10]u8 = @splat(2);
    for (0..8) |i| try f.seg.put(tkey(@intCast(i), i), &ten, i + 1, 0, 0);
    const before = stats.staging_ring_full_refusals.load();
    try testing.expectError(error.RingFull, f.seg.put(tkey(99, 99), &ten, 99, 0, 0));
    try testing.expectEqual(before + 1, stats.staging_ring_full_refusals.load());
    try testing.expectEqual(@as(usize, 8), f.seg.live());
    const fourth = f.seg.get(tkey(3, 3)).?;
    const hole = fourth.cell.extent.rec_off;
    fourth.release();
    try testing.expectEqual(@as(u64, 3 * 8192), hole);
    try testing.expect(f.seg.remove(tkey(3, 3), null));
    try f.seg.put(tkey(50, 50), &ten, 50, 0, 0);
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
    const dst = valueHash("decoy");
    writeRecord(decoy[4096..], 0, tkey(0xAA, 1).bytes(), "decoy", 777, 0, &dst);
    try seg.put(tkey(1, 10), &decoy, 1, 0, 0);
    const v2: [100]u8 = @splat(2);
    try seg.put(tkey(2, 20), &v2, 2, 0, 0);
    const v3: [5000]u8 = @splat(3);
    try seg.put(tkey(3, 30), &v3, 3, 0, 0);
    // A same-key replace with a crash between phases: both copies present.
    const v2b: [200]u8 = @splat(4);
    const old2 = seg.get(tkey(2, 20)).?;
    const off2 = old2.cell.extent.rec_off;
    old2.release();
    try seg.put(tkey(2, 20), &v2b, 5, 0, 0);
    // Resurrect the old copy's magic (as if phase 2 never ran).
    @memcpy(r.image[@intCast(off2)..][0..4], &SEGMENT_MAGIC);
    var ext: [8]Extent = undefined;
    const want_live = seg.liveExtents(&ext).len;
    seg.deinit();
    // A torn record: a header with a bad checksum in a free grain.
    var fresh = try Segment.init(gpa, r.image, .none);
    defer fresh.deinit();
    const torn_off: u64 = 60 * 4096;
    const tst = valueHash("torn");
    writeRecord(r.image, torn_off, tkey(9, 90).bytes(), "torn", 9, 0, &tst);
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
    try fresh.put(tkey(6, 60), &v6, 6, 0, 0);
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
    /// The dismount judgement of these inos (by the key's ino): a corpse's
    /// record, an unnamed one; every other key promotes (or fails as the
    /// promote hook does). The judge removes what it destroys, as the
    /// plane's does.
    seg: ?*Segment = null,
    corpse_ino: ?u64 = null,
    unnamed_ino: ?u64 = null,

    fn hooks(self: *Kicks) PromotionHooks {
        return .{ .ctx = self, .pressure = onPressure, .promote = onPromote, .dismount = onDismount };
    }

    fn onPressure(ctx: ?*anyopaque, _: StagingKey) void {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        self.pressure += 1;
    }

    fn onPromote(ctx: ?*anyopaque, k: StagingKey) bool {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        if (self.fail_next) {
            self.fail_next = false;
            return false;
        }
        self.promoted += 1;
        if (self.seg) |s| _ = s.remove(k, null);
        return true;
    }

    fn onDismount(ctx: ?*anyopaque, k: StagingKey) DismountVerdict {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        if (self.corpse_ino == k.ino() or self.unnamed_ino == k.ino()) {
            if (self.seg) |s| _ = s.remove(k, null);
            return if (self.corpse_ino == k.ino()) .corpse else .unnamed;
        }
        return if (onPromote(ctx, k)) .promoted else .kept;
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
    const s1 = try stageWrite(&pool, &r.seg, k, &v, 0, .none);
    try testing.expectEqual(@as(u64, 1), s1.generation);
    try testing.expect(!s1.kicked);
    try testing.expectEqual(@as(u64, 8192), pool.currentBytes());
    try testing.expectEqual(g0 + 8192, stats.current_staged_write_bytes.load());
    // Re-stage the same file_id larger: the delta is charged, one row.
    const v2: [5000]u8 = @splat(2);
    const s2 = try stageWrite(&pool, &r.seg, k, &v2, 0, .none);
    try testing.expectEqual(@as(u64, 2), s2.generation);
    try testing.expectEqual(@as(u64, 12288), pool.currentBytes());
    try testing.expectEqual(@as(?u64, 2), pool.generationOf(k.file_id));
    // Past the high-water: kicked.
    const big: [9 * 4096 - 10]u8 = @splat(3);
    const s3 = try stageWrite(&pool, &r.seg, tkey(2, 2), &big, 0, .none);
    try testing.expect(pool.currentBytes() > pool.highWater());
    try testing.expect(s3.kicked);
    try testing.expectEqual(@as(u32, 1), kicks.pressure);
    // Over the cap: refused, kicked again, nothing placed, no charge.
    const cur = pool.currentBytes();
    const live_before = r.seg.live();
    const refusals = stats.staging_over_cap_refusals.load();
    try testing.expectError(error.StorageFull, stageWrite(&pool, &r.seg, tkey(3, 3), &big, 0, .none));
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

test "the live cells' value-hash states are gauged (R5): N staged records carry N × @sizeOf(ValueHash) in staging_cell_state_bytes, an in-place append replaces a state for a state, a release frees it, deinit frees the rest; the bound is the cell population × the state, under one grain per record; the component is attribution-only" {
    const gpa = testing.allocator;
    const cap = 64 * 4096;
    const r = try ramSegment(gpa, cap);
    defer gpa.free(r.image);
    var seg = r.seg;
    const state_bytes: u64 = @sizeOf(ValueHash);
    // A state is smaller than the grain its record occupies on the device:
    // the RAM beside a record never outweighs the record (4.7 KiB did).
    try testing.expect(state_bytes < RECORD_ALIGN);
    try testing.expectEqual(Segment.maxCells(cap) * state_bytes, Segment.cellStateBound(cap));
    try testing.expect(Segment.cellStateBound(cap) < cap / 2);
    const g0 = stats.staging_cell_state_bytes.load();
    const v: [100]u8 = @splat(7);
    const n_live: u64 = 12;
    for (0..n_live) |i| try seg.put(tkey(@intCast(i + 1), i + 1), &v, i + 1, 0, 0);
    try testing.expectEqual(g0 + n_live * state_bytes, stats.staging_cell_state_bytes.load());
    try testing.expect(stats.staging_cell_state_bytes.load() - g0 <= Segment.cellStateBound(cap));
    // An in-place append mints the successor's state and frees the shadowed cell's.
    try seg.appendInPlace(tkey(1, 1), 100, &v, 1, 100, 0);
    try testing.expectEqual(g0 + n_live * state_bytes, stats.staging_cell_state_bytes.load());
    // A same-key replace: the old cell's state goes with the last reference.
    const lease = seg.get(tkey(2, 2)).?;
    try seg.put(tkey(2, 2), &v, 200, 0, 0);
    try testing.expectEqual(g0 + (n_live + 1) * state_bytes, stats.staging_cell_state_bytes.load());
    lease.release();
    try testing.expectEqual(g0 + n_live * state_bytes, stats.staging_cell_state_bytes.load());
    // A release frees the state; a recovered record has none until its first append.
    try testing.expect(seg.remove(tkey(3, 3), null));
    try testing.expectEqual(g0 + (n_live - 1) * state_bytes, stats.staging_cell_state_bytes.load());
    seg.deinit();
    try testing.expectEqual(g0, stats.staging_cell_state_bytes.load());
    var again = try Segment.init(gpa, r.image, .none);
    defer again.deinit();
    _ = again.recover();
    try testing.expectEqual(n_live - 1, again.live());
    try testing.expectEqual(g0, stats.staging_cell_state_bytes.load());
    try again.appendInPlace(tkey(4, 4), 100, &v, 4, 400, 0);
    try testing.expectEqual(g0 + state_bytes, stats.staging_cell_state_bytes.load());
    // The R5 component: attribution-only (its states leave with their cells —
    // the never-lossy lever is `staging_mmap`'s promotion kick), heap, not page cache.
    const c = cellStateComponent();
    try testing.expectEqualStrings("staging_cell_state", c.name);
    try testing.expectEqual(@as(u64, 0), c.weight);
    try testing.expect(!c.kernel_reclaimable);
    try testing.expectEqual(stats.staging_cell_state_bytes.load(), c.current());
}

test "the segment's fixed per-cell tables are gauged (R5): tablesBound(cap) = maxCells × (Cell + u32) + the grain bitmap + indexSlots × (1 + KEY_LEN + ptr); an open segment carries exactly it in staging_tables_bytes (the live tables agree term by term, the std map's capacity law pinned), puts move nothing, deinit returns it; the component is attribution-only; the tables are ≈ 4.4 % of the ring" {
    const gpa = testing.allocator;
    // The tie, restated: n = cap / grain + 1 cells, one bit per grain.
    const cap: u64 = 64 * 4096;
    const n: u64 = cap / RECORD_ALIGN + 1;
    try testing.expectEqual(n, Segment.maxCells(cap));
    try testing.expectEqual(@as(u64, 128), Segment.indexSlots(@intCast(n)));
    try testing.expectEqual(@as(usize, 1), Segment.occupiedWords(cap));
    try testing.expectEqual(n * @sizeOf(Cell) + n * @sizeOf(u32) + 8 + 128 * (1 + KEY_LEN + @sizeOf(*Cell)), Segment.tablesBound(cap));
    // The std map's capacity law at the boundary: 80 % load, power of two.
    try testing.expectEqual(@as(u64, 16), Segment.indexSlots(12));
    try testing.expectEqual(@as(u64, 32), Segment.indexSlots(13));
    const g0 = stats.staging_tables_bytes.load();
    const r = try ramSegment(gpa, cap);
    defer gpa.free(r.image);
    var seg = r.seg;
    try testing.expectEqual(Segment.tablesBound(cap), seg.tablesBytes());
    try testing.expectEqual(g0 + Segment.tablesBound(cap), stats.staging_tables_bytes.load());
    try testing.expectEqual(@as(u64, Segment.indexSlots(@intCast(n))), @as(u64, seg.index.capacity()));
    try testing.expectEqual(n, @as(u64, seg.free_cells.capacity));
    // Puts and removes move cells between the fixed tables; the gauge is still.
    const v: [100]u8 = @splat(7);
    for (0..8) |i| try seg.put(tkey(@intCast(i + 1), i + 1), &v, i + 1, 0, 0);
    try testing.expect(seg.remove(tkey(3, 3), null));
    try testing.expectEqual(g0 + Segment.tablesBound(cap), stats.staging_tables_bytes.load());
    seg.deinit();
    try testing.expectEqual(g0, stats.staging_tables_bytes.load());
    // The share of the ring: 24,882,356 B at the venue's 546 MiB ring
    // (139,777 cells × (112 + 4) B + 2,184 bitmap words × 8 B + 262,144
    // index slots × 33 B; the sorted-extent scratch and the pending list
    // — 80 B per cell — left with the grain bitmap, 2026-10-20) and
    // 2,984,052 B at 64 MiB — 4.3–4.4 % of either, the index's
    // power-of-two rounding the swing.
    const venue_ring: u64 = 546 * derive.MiB;
    try testing.expectEqual(@as(u64, 24_882_356), Segment.tablesBound(venue_ring));
    try testing.expectEqual(@as(u64, 2_984_052), Segment.tablesBound(64 * derive.MiB));
    try testing.expect(Segment.tablesBound(venue_ring) * 1000 / venue_ring == 43);
    try testing.expect(Segment.tablesBound(64 * derive.MiB) * 1000 / (64 * derive.MiB) == 44);
    // The R5 component: attribution-only (the tables live with the segment), heap, not page cache.
    const c = tablesComponent();
    try testing.expectEqualStrings("staging_tables", c.name);
    try testing.expectEqual(@as(u64, 0), c.weight);
    try testing.expectEqual(@as(u64, 0), c.floor);
    try testing.expect(!c.kernel_reclaimable);
    try testing.expectEqual(stats.staging_tables_bytes.load(), c.current());
}

test "the three promotion points: pressure rides stageWrite; the dismount pass judges every resident — promoted, kept (a resource decline stays resident, counted FAILED), a corpse's record and an unnamed one destroyed (counted apart, never FAILED) — and the ring ends with the kept one alone; fsync promotes only under the lever (default off), a failure never fails it" {
    const gpa = testing.allocator;
    var kicks: Kicks = .{};
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    kicks.seg = &r.seg;
    const v: [10]u8 = @splat(1);
    try r.seg.put(tkey(1, 1), &v, 1, 0, 0);
    try r.seg.put(tkey(2, 2), &v, 2, 0, 0);
    try r.seg.put(tkey(3, 3), &v, 3, 0, 0);
    // The rm'd file's record (its inode destroyed) and a record no layout
    // names (a kill between a promotion's commit and its ring release).
    try r.seg.put(tkey(4, 4), &v, 4, 0, 0);
    try r.seg.put(tkey(5, 5), &v, 5, 0, 0);
    kicks.corpse_ino = 4;
    kicks.unnamed_ino = 5;
    kicks.fail_next = true;
    const f0 = stats.dismount_promoted_files.load();
    const b0 = stats.dismount_promoted_bytes.load();
    const x0 = stats.dismount_promote_failures.load();
    const c0 = stats.dismount_corpse_residents_destroyed.load();
    const n0 = stats.dismount_unnamed_residents_destroyed.load();
    const tally = try promoteAllAtDismount(gpa, &r.seg, kicks.hooks());
    try testing.expectEqual(DismountTally{ .promoted = 2, .promoted_bytes = 2 * 8192, .kept = 1, .corpses = 1, .unnamed = 1 }, tally);
    try testing.expectEqual(@as(u32, 2), kicks.promoted);
    try testing.expectEqual(f0 + 2, stats.dismount_promoted_files.load());
    try testing.expectEqual(b0 + 2 * 8192, stats.dismount_promoted_bytes.load());
    try testing.expectEqual(x0 + 1, stats.dismount_promote_failures.load());
    try testing.expectEqual(c0 + 1, stats.dismount_corpse_residents_destroyed.load());
    try testing.expectEqual(n0 + 1, stats.dismount_unnamed_residents_destroyed.load());
    // The kept one is the ring's only resident; a destroyed record is gone.
    try testing.expectEqual(@as(usize, 1), r.seg.live());
    try testing.expect(r.seg.get(tkey(4, 4)) == null);
    try testing.expect(r.seg.get(tkey(5, 5)) == null);
    // The fsync lever takes the promote hook alone: no dismount verdicts.
    kicks.seg = null;
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

/// The fsync law's counting seam: every `durable` call recorded; `hold`
/// parks the call until the test releases it (the in-flight sync a storm
/// piles up behind); `fail_next` answers one call EIO.
const SyncSeam = struct {
    calls: std.ArrayList(Span) = .empty,
    reclaims: std.ArrayList(Span) = .empty,
    hold: futex.Word = .init(0),
    in_flight: std.atomic.Value(u32) = .init(0),
    fail_next: bool = false,

    fn backing(self: *SyncSeam) Backing {
        return .{ .ctx = self, .durable = durable, .reclaim = reclaim };
    }

    fn reclaim(ctx: ?*anyopaque, off: u64, len: u64) void {
        const self: *SyncSeam = @ptrCast(@alignCast(ctx.?));
        self.reclaims.append(testing.allocator, .{ .off = off, .len = len }) catch @panic("OOM");
    }

    fn durable(ctx: ?*anyopaque, _: Io, off: u64, len: u64) DurableError!void {
        const self: *SyncSeam = @ptrCast(@alignCast(ctx.?));
        _ = self.in_flight.fetchAdd(1, .acq_rel);
        defer _ = self.in_flight.fetchSub(1, .acq_rel);
        while (self.hold.load(.acquire) != 0) futex.wait(&self.hold, 1);
        self.calls.append(testing.allocator, .{ .off = off, .len = len }) catch @panic("OOM");
        if (self.fail_next) {
            self.fail_next = false;
            return error.InputOutput;
        }
    }

    fn release(self: *SyncSeam) void {
        self.hold.store(0, .release);
        futex.wakeAll(&self.hold);
    }
};

test "the fsync law (charter §1 item 10, 2026-10-19): fsync of a resident issues ONE ranged sync covering exactly its record — both header slots, the key and the value, grain-rounded —, counted with its bytes; a key with no resident issues none; a RAM image with no durable binding owes nothing; syncfs syncs the whole image once and an empty ring not at all; a failed sync is the caller's EIO" {
    const gpa = testing.allocator;
    const io = testing.io;
    var seam: SyncSeam = .{};
    defer seam.calls.deinit(gpa);
    defer seam.reclaims.deinit(gpa);
    const image = try gpa.alloc(u8, 64 * 4096);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try Segment.init(gpa, image, seam.backing());
    defer seg.deinit();
    const v: [5000]u8 = @splat(0x42);
    try seg.put(tkey(1, 11), &v, 7, 0, 0);
    const syncs0 = stats.staging_fsync_syncs.load();
    const bytes0 = stats.staging_fsync_bytes.load();
    const co0 = stats.staging_fsync_coalesced.load();
    const hist0 = stats.staging_fsync_sync_ns.snapshot().count;
    // The record: headers + key in the first grain, 5000 B of value in two
    // more — exactly the stride.
    try seg.syncResident(io, tkey(1, 11));
    try testing.expectEqual(@as(usize, 1), seam.calls.items.len);
    const span = seg.residentSpan(tkey(1, 11)).?;
    try testing.expectEqual(span, seam.calls.items[0]);
    try testing.expectEqual(stride(KEY_LEN, 5000), span.len);
    try testing.expectEqual(syncs0 + 1, stats.staging_fsync_syncs.load());
    try testing.expectEqual(bytes0 + span.len, stats.staging_fsync_bytes.load());
    try testing.expectEqual(co0, stats.staging_fsync_coalesced.load());
    try testing.expectEqual(hist0 + 1, stats.staging_fsync_sync_ns.snapshot().count);
    // No resident under the key: nothing owed, nothing issued.
    try seg.syncResident(io, tkey(2, 22));
    try testing.expectEqual(@as(usize, 1), seam.calls.items.len);
    try testing.expect(seg.residentSpan(tkey(2, 22)) == null);
    // syncfs: the whole image, once; empty ⇒ nothing.
    try seg.syncAll(io);
    try testing.expectEqual(@as(usize, 2), seam.calls.items.len);
    try testing.expectEqual(Span{ .off = 0, .len = image.len }, seam.calls.items[1]);
    try testing.expect(seg.remove(tkey(1, 11), 7));
    try seg.syncAll(io);
    try testing.expectEqual(@as(usize, 2), seam.calls.items.len);
    // A failed sync: EIO to the caller; the next one is clean again.
    try seg.put(tkey(3, 33), &v, 8, 0, 0);
    seam.fail_next = true;
    try testing.expectError(error.InputOutput, seg.syncResident(io, tkey(3, 33)));
    try seg.syncResident(io, tkey(3, 33));
    try testing.expectEqual(@as(usize, 4), seam.calls.items.len);
    // A RAM image with no binding (the portable tests' segments).
    var r = try ramSegment(gpa, 16 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    try r.seg.put(tkey(4, 44), &v, 9, 0, 0);
    const syncs1 = stats.staging_fsync_syncs.load();
    try r.seg.syncResident(io, tkey(4, 44));
    try r.seg.syncAll(io);
    try testing.expectEqual(syncs1, stats.staging_fsync_syncs.load());
}

test "a synced copy survives an unsynced re-stage: the superseded copy keeps its magic, its cell and its slot (no tombstone, no punch, not a placement hole) until the key's next sync — a cut that takes the new copy recovers the old one whole; a copy NEVER synced is released the moment it is superseded (held, it grew the live set until the first fsync: the 1 k/s staged arm); the key's sync releases every synced copy up to the generation it covered; a key that leaves releases its copies; syncAll releases a copy only if its successor was indexed before the sync began, and a copy superseded WHILE a sync that covers it runs is held" {
    const gpa = testing.allocator;
    const io = testing.io;
    var seam: SyncSeam = .{};
    defer seam.calls.deinit(gpa);
    defer seam.reclaims.deinit(gpa);
    const image = try gpa.alloc(u8, 64 * 4096);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try Segment.init(gpa, image, seam.backing());
    defer seg.deinit();
    const k = tkey(1, 11);
    const a: [5000]u8 = @splat('a');
    const b: [3000]u8 = @splat('b');
    const held0 = stats.staging_superseded_held.load();
    const hb0 = stats.staging_superseded_held_bytes.load();
    const rel0 = stats.staging_superseded_released.load();
    try seg.put(k, &a, 1, 0, 0);
    try seg.syncResident(io, k);
    const a_ext = seg.residentExtent(k).?;
    // The unsynced overwrite: a whole-image re-stage under the same key.
    try seg.put(k, &b, 2, 0, 0);
    const b_ext = seg.residentExtent(k).?;
    try testing.expect(a_ext.end() <= b_ext.rec_off or b_ext.end() <= a_ext.rec_off);
    try testing.expect(!isTombstone(image, a_ext.rec_off)); // the old copy keeps its magic …
    try testing.expectEqual(@as(usize, 0), seam.reclaims.items.len); // … its slot is not punched …
    try testing.expectEqual(held0 + 1, stats.staging_superseded_held.load());
    try testing.expectEqual(hb0 + (a_ext.end() - a_ext.rec_off), stats.staging_superseded_held_bytes.load());
    // … and is no placement hole: a third record of its size lands elsewhere.
    try seg.put(tkey(2, 22), &a, 3, 0, 0);
    const c_ext = seg.residentExtent(tkey(2, 22)).?;
    try testing.expect(c_ext.end() <= a_ext.rec_off or a_ext.end() <= c_ext.rec_off);
    // The cut takes the new copy's pages (never written back): recovery
    // over the image adopts the OLD, synced copy whole.
    {
        var cut = try gpa.dupe(u8, image);
        defer gpa.free(cut);
        @memset(cut[@intCast(b_ext.rec_off)..@intCast(b_ext.end())], 0);
        var rec = try Segment.init(gpa, cut, .none);
        defer rec.deinit();
        const rep = rec.recover();
        try testing.expectEqual(@as(u64, 2), rep.adopted); // k's old copy and tkey(2, 22)
        try testing.expectEqual(@as(u64, 0), rep.torn);
        const l = rec.get(k).?;
        defer l.release();
        try testing.expectEqualSlices(u8, &a, l.bytes());
        try testing.expectEqual(@as(u64, 1), l.generation());
    }
    // A second re-stage before any sync: the UNSYNCED copy (generation 2)
    // is released at once — tombstoned, punched — while the synced one
    // stays held (red before: the chain A → B → C held whole, so a stream
    // of overwrites with no fsync grew the live set without bound).
    try seg.put(k, &a, 4, 0, 0);
    try testing.expectEqual(held0 + 1, stats.staging_superseded_held.load());
    try testing.expect(!isTombstone(image, a_ext.rec_off));
    try testing.expect(isTombstone(image, b_ext.rec_off));
    try testing.expectEqual(@as(usize, 1), seam.reclaims.items.len);
    // The key's sync covers generation 4: the synced copy lets go —
    // tombstoned, punched, its cell back in the pool.
    const free_before = seg.free_cells.items.len;
    try seg.syncResident(io, k);
    try testing.expect(isTombstone(image, a_ext.rec_off));
    try testing.expectEqual(@as(usize, 2), seam.reclaims.items.len);
    try testing.expectEqual(free_before + 1, seg.free_cells.items.len);
    try testing.expectEqual(held0, stats.staging_superseded_held.load());
    try testing.expectEqual(hb0, stats.staging_superseded_held_bytes.load());
    try testing.expectEqual(rel0 + 1, stats.staging_superseded_released.load());
    try testing.expectEqual(@as(?*Cell, null), seg.retired_head);
    // A key that leaves (unlink, a promotion's release) releases its held
    // copies at once.
    try seg.put(k, &b, 5, 0, 0);
    try testing.expectEqual(held0 + 1, stats.staging_superseded_held.load());
    try testing.expect(seg.remove(k, 5));
    try testing.expectEqual(held0, stats.staging_superseded_held.load());
    try testing.expectEqual(@as(usize, 4), seam.reclaims.items.len);
    try testing.expectEqual(@as(?*Cell, null), seg.retired_head);
    // syncAll: a copy whose successor was indexed before the sync began
    // lets go; one re-staged WHILE the sync ran stays held (its successor's
    // pages may have missed the kernel's dirty walk) — and so does a copy
    // never synced before that the running sync covers (its caller is
    // about to be told it is durable).
    const k2 = tkey(3, 33);
    try seg.put(k, &a, 6, 0, 0);
    try seg.syncResident(io, k);
    try seg.put(k, &b, 7, 0, 0); // k: one held copy, successor indexed before the sync
    try seg.put(k2, &a, 8, 0, 0); // k2: never synced
    try testing.expectEqual(held0 + 1, stats.staging_superseded_held.load());
    seam.hold.store(1, .release);
    const Req = struct {
        fn run(s: *Segment, io_: Io, out: *anyerror!void) void {
            out.* = s.syncAll(io_);
        }
    };
    var result: anyerror!void = {};
    var group: Io.Group = .init;
    group.async(io, Req.run, .{ &seg, io, &result });
    while (seam.in_flight.load(.acquire) == 0) try std.Thread.yield();
    try seg.put(k2, &b, 9, 0, 0); // k2's re-stage during the sync: its old copy must stay held
    try testing.expectEqual(held0 + 2, stats.staging_superseded_held.load());
    seam.release();
    group.await(io) catch |e| switch (e) {
        error.Canceled => unreachable,
    };
    try result;
    try testing.expectEqual(held0 + 1, stats.staging_superseded_held.load()); // k's released, k2's held
    try seg.syncResident(io, k2);
    try testing.expectEqual(held0, stats.staging_superseded_held.load());
    // A re-stage of a never-synced key under NO sync: released at once,
    // nothing held, the slot a hole again.
    const k3 = tkey(4, 44);
    try seg.put(k3, &a, 10, 0, 0);
    const d_ext = seg.residentExtent(k3).?;
    const reclaims_before = seam.reclaims.items.len;
    try seg.put(k3, &b, 11, 0, 0);
    try testing.expect(isTombstone(image, d_ext.rec_off));
    try testing.expectEqual(reclaims_before + 1, seam.reclaims.items.len);
    try testing.expectEqual(held0, stats.staging_superseded_held.load());
}

/// The departure law's counting seam: every cover round recorded; `hold`
/// parks the round until the test releases it; `fail_next` answers one EIO.
const CoverCounter = struct {
    rounds: u32 = 0,
    hold: futex.Word = .init(0),
    in_flight: std.atomic.Value(u32) = .init(0),
    fail_next: bool = false,

    fn seam(self: *CoverCounter) CoverSeam {
        return .{ .ctx = self, .run = run };
    }

    fn run(ctx: ?*anyopaque, _: Io) DurableError!void {
        const self: *CoverCounter = @ptrCast(@alignCast(ctx.?));
        _ = self.in_flight.fetchAdd(1, .acq_rel);
        defer _ = self.in_flight.fetchSub(1, .acq_rel);
        while (self.hold.load(.acquire) != 0) futex.wait(&self.hold, 1);
        self.rounds += 1;
        if (self.fail_next) {
            self.fail_next = false;
            return error.InputOutput;
        }
    }

    fn release(self: *CoverCounter) void {
        self.hold.store(0, .release);
        futex.wakeAll(&self.hold);
    }
};

test "the departure law: a key that leaves the ring (a promotion's release) keeps its copies — the resident one AND the superseded one — whole on the segment (magic, slot, no punch, not a placement hole) until a cover round that STARTED after the departure lands (red before: tombstoned and punched at once — a cut before the publish's durability found the old layout naming a hole); the cut model: recovery after the departure adopts the synced copy; the round releases what departed before it began and not what departed while it ran; a failed round releases nothing; nothing departed ⇒ no round; a RAM image with no seam releases at once" {
    const gpa = testing.allocator;
    const io = testing.io;
    var seam: SyncSeam = .{};
    defer seam.calls.deinit(gpa);
    defer seam.reclaims.deinit(gpa);
    var rounds: CoverCounter = .{};
    const image = try gpa.alloc(u8, 64 * 4096);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try Segment.init(gpa, image, seam.backing());
    defer seg.deinit();
    seg.cover_seam = rounds.seam();
    const k = tkey(1, 11);
    const a: [5000]u8 = @splat('a');
    const b: [3000]u8 = @splat('b');
    const def0 = stats.staging_departures_deferred.load();
    const held0 = stats.staging_departed_held.load();
    const hb0 = stats.staging_departed_held_bytes.load();
    const rel0 = stats.staging_departed_released.load();
    const rounds0 = stats.staging_cover_rounds.load();
    const sup0 = stats.staging_superseded_held.load();
    // A synced copy, an unsynced re-stage over it (superseded, held), then
    // the promotion's release: the layout commit stopped naming the key.
    try seg.put(k, &a, 1, 0, 0);
    try seg.syncResident(io, k);
    const a_ext = seg.residentExtent(k).?;
    try seg.put(k, &b, 2, 0, 0);
    const b_ext = seg.residentExtent(k).?;
    try testing.expectEqual(sup0 + 1, stats.staging_superseded_held.load());
    try testing.expect(seg.remove(k, 2));
    try testing.expect(seg.get(k) == null); // the key is gone from the index …
    try testing.expect(!isTombstone(image, a_ext.rec_off)); // … but both copies keep their magic …
    try testing.expect(!isTombstone(image, b_ext.rec_off));
    try testing.expectEqual(@as(usize, 0), seam.reclaims.items.len); // … and their slots.
    try testing.expectEqual(def0 + 1, stats.staging_departures_deferred.load());
    try testing.expectEqual(held0 + 2, stats.staging_departed_held.load());
    try testing.expectEqual(hb0 + (a_ext.end() - a_ext.rec_off) + (b_ext.end() - b_ext.rec_off), stats.staging_departed_held_bytes.load());
    try testing.expectEqual(sup0, stats.staging_superseded_held.load()); // the superseded hold became the departure's
    try testing.expectEqual(@as(u32, 2), seg.departedHeld());
    // Not a placement hole: a third record of their size lands elsewhere.
    try seg.put(tkey(2, 22), &a, 3, 0, 0);
    const c_ext = seg.residentExtent(tkey(2, 22)).?;
    try testing.expect(c_ext.end() <= a_ext.rec_off or a_ext.end() <= c_ext.rec_off);
    try testing.expect(c_ext.end() <= b_ext.rec_off or b_ext.end() <= c_ext.rec_off);
    // The cut before the publish is durable: the old (staged) layout names
    // the key, and recovery over the image finds it — the unsynced copy's
    // pages lost, the synced one whole.
    {
        var cut = try gpa.dupe(u8, image);
        defer gpa.free(cut);
        @memset(cut[@intCast(b_ext.rec_off)..@intCast(b_ext.end())], 0);
        var rec = try Segment.init(gpa, cut, .none);
        defer rec.deinit();
        const rep = rec.recover();
        try testing.expectEqual(@as(u64, 2), rep.adopted);
        const l = rec.get(k).?;
        defer l.release();
        try testing.expectEqualSlices(u8, &a, l.bytes());
    }
    // The round: both copies let go — tombstoned, punched, their cells back.
    const free_before = seg.free_cells.items.len;
    try seg.cover(io);
    try testing.expectEqual(@as(u32, 1), rounds.rounds);
    try testing.expect(isTombstone(image, a_ext.rec_off));
    try testing.expect(isTombstone(image, b_ext.rec_off));
    try testing.expectEqual(@as(usize, 2), seam.reclaims.items.len);
    try testing.expectEqual(free_before + 2, seg.free_cells.items.len);
    try testing.expectEqual(held0, stats.staging_departed_held.load());
    try testing.expectEqual(hb0, stats.staging_departed_held_bytes.load());
    try testing.expectEqual(rel0 + 2, stats.staging_departed_released.load());
    try testing.expectEqual(rounds0 + 1, stats.staging_cover_rounds.load());
    try testing.expectEqual(@as(u32, 0), seg.departedHeld());
    // Nothing departed: no round is run.
    try seg.cover(io);
    try testing.expectEqual(@as(u32, 1), rounds.rounds);
    // A failed round releases nothing; the next one does.
    try seg.syncResident(io, tkey(2, 22));
    try testing.expect(seg.remove(tkey(2, 22), 3));
    rounds.fail_next = true;
    try testing.expectError(error.InputOutput, seg.cover(io));
    try testing.expect(!isTombstone(image, c_ext.rec_off));
    try testing.expectEqual(@as(u32, 1), seg.departedHeld());
    try seg.cover(io);
    try testing.expect(isTombstone(image, c_ext.rec_off));
    try testing.expectEqual(@as(u32, 0), seg.departedHeld());
    try testing.expectEqual(@as(u32, 3), rounds.rounds);
    // A departure WHILE a round runs waits for the next round: its publish
    // may have committed after the barrier started.
    const k3 = tkey(3, 33);
    const k4 = tkey(4, 44);
    try seg.put(k3, &a, 4, 0, 0);
    try seg.put(k4, &b, 5, 0, 0);
    try seg.syncResident(io, k3);
    try seg.syncResident(io, k4);
    try testing.expect(seg.remove(k3, 4));
    rounds.hold.store(1, .release);
    const Req = struct {
        fn run(s: *Segment, io_: Io, out: *anyerror!void) void {
            out.* = s.cover(io_);
        }
    };
    var result: anyerror!void = {};
    var group: Io.Group = .init;
    group.async(io, Req.run, .{ &seg, io, &result });
    while (rounds.in_flight.load(.acquire) == 0) try std.Thread.yield();
    try testing.expect(seg.remove(k4, 5));
    try testing.expectEqual(@as(u32, 2), seg.departedHeld());
    rounds.release();
    group.await(io) catch |e| switch (e) {
        error.Canceled => unreachable,
    };
    try result;
    try testing.expectEqual(@as(u32, 1), seg.departedHeld()); // k3 released, k4 held
    try testing.expect(seg.get(k4) == null);
    try seg.cover(io);
    try testing.expectEqual(@as(u32, 0), seg.departedHeld());
    try testing.expectEqual(@as(?*Cell, null), seg.retired_head);
    // A key never synced owes nothing to a cut: its removal releases at
    // once, seam or not.
    const k5 = tkey(5, 55);
    try seg.put(k5, &a, 12, 0, 0);
    const k5_ext = seg.residentExtent(k5).?;
    try testing.expect(seg.remove(k5, 12));
    try testing.expect(isTombstone(image, k5_ext.rec_off));
    try testing.expectEqual(@as(u32, 0), seg.departedHeld());
    // A RAM image with no seam: the release is immediate, as before.
    var r = try ramSegment(gpa, 16 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    try r.seg.put(k, &a, 9, 0, 0);
    const e = r.seg.residentExtent(k).?;
    try testing.expect(r.seg.remove(k, 9));
    try testing.expect(isTombstone(r.image, e.rec_off));
    try testing.expectEqual(@as(u32, 0), r.seg.departedHeld());
    try testing.expectEqual(def0 + 4, stats.staging_departures_deferred.load());
}

test "the sync coalescer: a sync in flight absorbs every request that arrives meanwhile into ONE following sync over their union span — 1 + 16 requests cost 2 syncs with 15 coalesced; the leader returns on its own sync; a failure reaches every member of its batch and no other" {
    const gpa = testing.allocator;
    const io = testing.io;
    var seam: SyncSeam = .{};
    defer seam.calls.deinit(gpa);
    defer seam.reclaims.deinit(gpa);
    const image = try gpa.alloc(u8, 256 * 4096);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try Segment.init(gpa, image, seam.backing());
    defer seg.deinit();
    const v: [100]u8 = @splat(0x11);
    const n = 16;
    var i: u64 = 0;
    while (i <= n) : (i += 1) try seg.put(tkey(@intCast(i), i + 1), &v, i + 1, 0, 0);
    const Req = struct {
        fn run(s: *Segment, io_: Io, key: StagingKey, out: *anyerror!void) void {
            out.* = s.syncResident(io_, key);
        }
    };
    const syncs0 = stats.staging_fsync_syncs.load();
    const co0 = stats.staging_fsync_coalesced.load();
    // The leader's sync is held in flight; sixteen more arrive and park.
    seam.hold.store(1, .release);
    var results: [n + 1]anyerror!void = undefined;
    var group: Io.Group = .init;
    group.async(io, Req.run, .{ &seg, io, tkey(0, 1), &results[0] });
    while (seam.in_flight.load(.acquire) == 0) try std.Thread.yield();
    i = 1;
    while (i <= n) : (i += 1) group.async(io, Req.run, .{ &seg, io, tkey(@intCast(i), i + 1), &results[i] });
    while (seg.durable.pending() != n) try std.Thread.yield();
    try testing.expectEqual(@as(usize, 0), seam.calls.items.len);
    seam.release();
    group.await(io) catch |e| switch (e) {
        error.Canceled => unreachable,
    };
    for (results) |r| try r;
    // Two syncs: the leader's own record, then ONE over the sixteen.
    try testing.expectEqual(@as(usize, 2), seam.calls.items.len);
    try testing.expectEqual(seg.residentSpan(tkey(0, 1)).?, seam.calls.items[0]);
    var lo: u64 = std.math.maxInt(u64);
    var hi: u64 = 0;
    i = 1;
    while (i <= n) : (i += 1) {
        const s = seg.residentSpan(tkey(@intCast(i), i + 1)).?;
        lo = @min(lo, s.off);
        hi = @max(hi, s.off + s.len);
    }
    try testing.expectEqual(Span{ .off = lo, .len = hi - lo }, seam.calls.items[1]);
    try testing.expectEqual(syncs0 + 2, stats.staging_fsync_syncs.load());
    try testing.expectEqual(co0 + n - 1, stats.staging_fsync_coalesced.load());
    try testing.expectEqual(@as(u32, 0), seg.durable.pending());
    try testing.expect(!seg.durable.running);
    // A failure reaches its batch alone: the held leader's batch fails,
    // the followers' batch (its own sync) succeeds.
    seam.hold.store(1, .release);
    seam.fail_next = true;
    var group2: Io.Group = .init;
    group2.async(io, Req.run, .{ &seg, io, tkey(0, 1), &results[0] });
    while (seam.in_flight.load(.acquire) == 0) try std.Thread.yield();
    i = 1;
    while (i <= 3) : (i += 1) group2.async(io, Req.run, .{ &seg, io, tkey(@intCast(i), i + 1), &results[i] });
    while (seg.durable.pending() != 3) try std.Thread.yield();
    seam.release();
    group2.await(io) catch |e| switch (e) {
        error.Canceled => unreachable,
    };
    try testing.expectError(error.InputOutput, results[0]);
    i = 1;
    while (i <= 3) : (i += 1) try results[i];
    try testing.expectEqual(@as(usize, 4), seam.calls.items.len);
}

test "the residents census under stages landing: sized and filled under ONE lock hold, every census is whole — a census sized by live() and filled after met a stage in between one entry past its buffer (xfstests generic/013: fsstress's sync under four writers, syncfs's promote pass)" {
    const gpa = testing.allocator;
    var r = try ramSegment(gpa, 8192 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const Writer = struct {
        fn run(seg: *Segment, done: *std.atomic.Value(bool)) void {
            const v: [10]u8 = @splat(7);
            var i: u64 = 1;
            while (i <= 1500) : (i += 1) seg.put(tkey(@intCast(i % 200), i), &v, i, 0, 0) catch break;
            done.store(true, .release);
        }
    };
    var done = std.atomic.Value(bool).init(false);
    const t = try std.Thread.spawn(.{}, Writer.run, .{ &r.seg, &done });
    var censuses: usize = 0;
    var last: usize = 0;
    while (!done.load(.acquire) or censuses == 0) {
        const out = try r.seg.residentsAlloc(gpa);
        defer gpa.free(out);
        // Whole: every entry filled, never fewer than the previous census
        // (nothing is removed here), never beyond the writer's total.
        for (out) |res| try testing.expect(res.cost != 0);
        try testing.expect(out.len >= last and out.len <= 1500);
        last = out.len;
        censuses += 1;
    }
    t.join();
    try testing.expect(censuses > 1);
    const final = try r.seg.residentsAlloc(gpa);
    defer gpa.free(final);
    try testing.expectEqual(@as(usize, 1500), final.len);
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
    _ = try stageWrite(&pool, &r.seg, k, &v, 0, .none);
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

test "the marker: 80 B, checksummed, names its owner (node, mount slot, appender, pid), identity = hash64(set_uuid ‖ \"staging\") and checked against its own uuid; every refusal; the member-scoped bind ladder's verdicts (0003 §T.1)" {
    const uuid: [16]u8 = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    const m: Marker = .{ .set_uuid = uuid, .set_epoch = 3, .node_token = 0xABC, .mount_slot = 2, .appender_id = 5, .owner_pid = 31337 };
    const bytes = m.encode();
    try testing.expectEqual(@as(usize, 80), bytes.len);
    try testing.expectEqual(@as(u32, 5), std.mem.readInt(u32, bytes[64..68], .little));
    try testing.expectEqual(@as(u32, 31337), std.mem.readInt(u32, bytes[68..72], .little));
    try testing.expectEqualSlices(u8, "SQZM", bytes[0..4]);
    try testing.expectEqualSlices(u8, &uuid, bytes[8..24]);
    var want: checksum.Hasher = .init();
    want.update(&uuid);
    want.update("staging");
    try testing.expectEqual(want.finalLow64(), stagingIdentity(uuid));
    try testing.expectEqual(stagingIdentity(uuid), std.mem.readInt(u64, bytes[48..56], .little));
    try testing.expect(stagingIdentity(uuid) != checksum.hash64(&uuid));
    const back = try Marker.decode(&bytes);
    try testing.expectEqual(m, back);
    // Refusals: length, a flipped byte, a foreign identity for the carried uuid, an unknown flag, a non-dual prev epoch.
    try testing.expectError(error.Corrupt, Marker.decode(bytes[0..79]));
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
    // The bind ladder: the member is the mount slot.
    try testing.expectEqual(BindVerdict.absent, bindVerdict(null, m));
    try testing.expectEqual(BindVerdict.match, bindVerdict(m, m));
    var other_set = m;
    other_set.set_uuid[0] = 99;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(other_set, m));
    // The same member's next incarnation (a new boot's node token, a new
    // pid, another appender id): its predecessor's ring is ours to adopt.
    var predecessor = m;
    predecessor.node_token = 0xDEF;
    predecessor.owner_pid = 4242;
    predecessor.appender_id = 9;
    try testing.expectEqual(BindVerdict.match, bindVerdict(predecessor, m));
    // Another member's root: never ours, whatever its node.
    var peer = m;
    peer.mount_slot = 0x8000_0003;
    try testing.expectEqual(BindVerdict.foreign_owner, bindVerdict(peer, m));
    peer.node_token = m.node_token;
    try testing.expectEqual(BindVerdict.foreign_owner, bindVerdict(peer, m));
    // A dual marker naming our epoch as its previous one matches the set (upgrade restamps it canonical).
    var ours_next = m;
    ours_next.set_epoch = 4;
    var dual_to_4 = m;
    dual_to_4.set_epoch = 4;
    dual_to_4.prev_set_epoch = 3;
    try testing.expectEqual(BindVerdict.upgrade, bindVerdict(dual_to_4, ours_next));
    var older = m;
    older.set_epoch = 2;
    try testing.expectEqual(BindVerdict.upgrade, bindVerdict(older, m));
    // A newer epoch than ours on the same set is dead to us (we are the stale mount).
    var newer = m;
    newer.set_epoch = 9;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(newer, m));
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
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try testing.expect(std.mem.startsWith(u8, out, "{\"current_staged_write_bytes\":"));
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_cell_state_bytes\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_tables_bytes\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staged_payload_lost_reads\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_fsync_syncs\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_fsync_bytes\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_fsync_coalesced\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_departures_deferred\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_departed_held_bytes\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_departed_released\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_cover_rounds\":") != null);
    try testing.expect(std.mem.endsWith(u8, out, "}"));
    var pbuf: [4096]u8 = undefined;
    var pw: std.Io.Writer = .fixed(&pbuf);
    try writePhasesJson(&pw);
    try testing.expect(std.mem.indexOf(u8, pw.buffered(), "\"fsync_sync\":") != null);
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
    try fuzzkit.fuzz({}, recordFuzz, .{});
}

test "linux: the member scope (0003 §T.1) — two owners over ONE cache path: each binds its own member root and B never adopts A's residents; a second claim on A's LIVE root refuses naming A; A dead, a FOREIGN member on A's root refuses (never adopted, never wiped); A's successor (the same member, a new incarnation) adopts every resident" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "staging: Linux only");
    const gpa = testing.allocator;
    var tmp = testing.tmpDir(.{});
    defer tmp.cleanup();
    var path_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const n = try tmp.dir.realPath(testing.io, &path_buf);
    var c_buf: [std.Io.Dir.max_path_bytes]u8 = undefined;
    const container = try std.fmt.bufPrint(&c_buf, "{s}/cache", .{path_buf[0..n]});
    const uring_fs = @import("../io/uring_fs.zig");
    const pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, 1024));
    defer pool.deinit();
    const segs: []const [:0]const u8 = &.{"seg-0"};
    const cap: u64 = 64 * 4096;
    const uuid: [16]u8 = @splat(0x5A);
    // A = the manager (mount slot 0, appender 0), B = a joiner on the same
    // node (its mount-point slot, appender 1).
    const a_owner: Marker = .{ .set_uuid = uuid, .set_epoch = 1, .node_token = 0xA0, .mount_slot = 0, .appender_id = 0, .owner_pid = 1111 };
    const b_owner: Marker = .{ .set_uuid = uuid, .set_epoch = 1, .node_token = 0xA0, .mount_slot = 0x8000_1234, .appender_id = 1, .owner_pid = 2222 };
    const live0 = stats.staging_owner_live_refusals.load();
    const foreign0 = stats.staging_foreign_scope_refusals.load();
    var refusal: Ring.BindRefusal = .{};
    var a = try Ring.bindMember(pool, container, a_owner, segs, &refusal);
    var a_open = true;
    defer if (a_open) a.release();
    {
        var ms = try Ring.openSegment(a.fd, "seg-0", cap);
        defer ms.close();
        var seg = try Segment.init(gpa, ms.map, ms.backing());
        defer seg.deinit();
        for (1..4) |i| try seg.put(tkey(@intCast(i), i), "a resident", 1, 0, 0);
        _ = linux.fsync(ms.fd);
    }
    // B over the same cache path: its own root; A's residents are never B's.
    {
        var b = try Ring.bindMember(pool, container, b_owner, segs, &refusal);
        defer b.release();
        try testing.expect(!std.mem.eql(u8, a.path(), b.path()));
        var ms = try Ring.openSegment(b.fd, "seg-0", cap);
        defer ms.close();
        var seg = try Segment.init(gpa, ms.map, ms.backing());
        defer seg.deinit();
        try testing.expectEqual(@as(u64, 0), seg.recover().adopted);
    }
    // A second claim on A's LIVE root refuses naming A: B forced onto it,
    // and A's own member identity presented by a second incarnation.
    try testing.expectError(error.OwnerLive, Ring.bindRoot(pool, a.path(), b_owner, segs, &refusal));
    try testing.expectEqual(a_owner, refusal.owner.?);
    var a_next = a_owner;
    a_next.node_token = 0xA1;
    a_next.owner_pid = 3333;
    try testing.expectError(error.OwnerLive, Ring.bindMember(pool, container, a_next, segs, &refusal));
    try testing.expectEqual(@as(u32, 1111), refusal.owner.?.owner_pid);
    try testing.expectEqual(live0 + 2, stats.staging_owner_live_refusals.load());
    try testing.expect(Ring.ownerLive(a.path()));
    // A dies (its claim goes with its process); the residents stay.
    var a_root_buf: [linux.PATH_MAX]u8 = undefined;
    const a_root = a_root_buf[0..a.path().len];
    @memcpy(a_root, a.path());
    a.release();
    a_open = false;
    try testing.expect(!Ring.ownerLive(a_root));
    // The census from B's side: A's root, ownerless, holds ring bytes.
    try testing.expectEqual(@as(u32, 1), Ring.residueCensus(pool, container, b_owner.mount_slot, "seg-0"));
    // A foreign member on A's dead root: refused, never adopted, never wiped.
    try testing.expectError(error.ForeignOwner, Ring.bindRoot(pool, a_root, b_owner, segs, &refusal));
    try testing.expectEqual(a_owner, refusal.owner.?);
    try testing.expectEqual(foreign0 + 1, stats.staging_foreign_scope_refusals.load());
    // A's successor adopts every resident (the same member, a new boot's
    // node token and pid); its marker now names it.
    var succ = try Ring.bindMember(pool, container, a_next, segs, &refusal);
    defer succ.release();
    try testing.expectEqualStrings(a_root, succ.path());
    try testing.expectEqual(BindVerdict.match, succ.verdict);
    try testing.expectEqual(a_owner, succ.found.?);
    try testing.expectEqual(a_next, (try Ring.readMarker(pool, a_root)).?);
    var ms = try Ring.openSegment(succ.fd, "seg-0", cap);
    defer ms.close();
    var seg = try Segment.init(gpa, ms.map, ms.backing());
    defer seg.deinit();
    try testing.expectEqual(@as(u64, 3), seg.recover().adopted);
}

test "linux: a ring under a tmp dir — 0700/0600 modes, mmap put/get, a remount recovers the entries, the marker round-trips two-phase through the io_uring file layer" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "staging: Linux only");
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
    try seg.put(tkey(1, 11), &v, 7, 0, 0);
    const l = seg.get(tkey(1, 11)).?;
    try testing.expectEqualSlices(u8, &v, l.bytes());
    l.release();
    // The fsync law's sync through the real binding (off a lane: the raw
    // fdatasync), counted with the record's span.
    const syncs0 = stats.staging_fsync_syncs.load();
    const bytes0 = stats.staging_fsync_bytes.load();
    try seg.syncResident(testing.io, tkey(1, 11));
    try testing.expectEqual(syncs0 + 1, stats.staging_fsync_syncs.load());
    try testing.expectEqual(bytes0 + stride(KEY_LEN, 5000), stats.staging_fsync_bytes.load());
    try seg.put(tkey(2, 22), "two", 8, 0, 0);
    try testing.expect(seg.remove(tkey(2, 22), 8));
    seg.deinit();
    ms.close();
    // Remount: a shorter requested capacity never shrinks the file.
    var ms2 = try Ring.openSegment(root_fd, "seg-00", 8 * 4096);
    defer ms2.close();
    try testing.expectEqual(cap, ms2.map.len);
    var seg2 = try Segment.init(gpa, ms2.map, ms2.backing());
    defer seg2.deinit();
    // Residency below must count the SCAN's touches alone: drop the first
    // session's pages (written back first — a dirty page is never dropped) and
    // fault without readahead. A write fault on a sparse shared mapping reads
    // its readahead window in (32 pages at the default 128 KiB: holes cached as
    // zero pages), which on a hole-reporting host left 30 of the 64 grains
    // resident before the scan ran (x86_64, ext4, 7.0).
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.fdatasync(ms2.fd)));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.fadvise(ms2.fd, 0, 0, linux.POSIX_FADV.DONTNEED)));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.madvise(ms2.map.ptr, ms2.map.len, linux.MADV.RANDOM)));
    const rep = seg2.recover();
    try testing.expectEqual(@as(u64, 1), rep.adopted);
    try testing.expectEqual(@as(u64, 0), rep.torn);
    // The scan skipped the holes: only the live record's pages (and the
    // tombstoned second record's punched run, none) are resident — never
    // the whole 64-grain ring. Asserted where the host filesystem reports
    // holes (ext4; the venue's virtiofs answers SEEK_DATA with the query
    // offset, and the scan honestly degrades to the grain walk there).
    var vec: [64]u8 = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall3(.mincore, @intFromPtr(ms2.map.ptr), ms2.map.len, @intFromPtr(&vec))));
    var resident: u32 = 0;
    for (vec) |b| resident += b & 1;
    try testing.expect(resident >= 1);
    const reports_holes = Ring.MappedSegment.nextData(&ms2, 4 * 4096) == null;
    if (reports_holes) try testing.expect(resident <= 4); // the 5000 B record spans two grains (+ host block rounding)
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
    try testing.expectEqual(BindVerdict.match, bindVerdict(try Ring.readMarker(pool, root), m));
    var other = m;
    other.set_uuid[0] = 8;
    try testing.expectEqual(BindVerdict.dead, bindVerdict(try Ring.readMarker(pool, root), other));
    const d0 = stats.staging_generation_discards.load();
    Ring.wipeSegments(root_fd, &.{"seg-00"});
    try testing.expectEqual(d0 + 1, stats.staging_generation_discards.load());
    var ms3 = try Ring.openSegment(root_fd, "seg-00", 8 * 4096);
    defer ms3.close();
    try testing.expectEqual(@as(usize, 8 * 4096), ms3.map.len);
}
