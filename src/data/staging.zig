//! The staging ring — the WRITE CACHE (charter §6.1 "Staged", the owner's
//! ruling of 2026-10-21; `docs/porting/data_layouts.md` §3, §11 Q-7–Q-10).
//! A staged file's whole payload lives in the writing host's RAM — a
//! kernel-held, size-sealed memfd of `SQUEEZEFS_WRITE_CACHE_PCT` of system
//! RAM (`Ring.Ram`), its first grain the marker, the rest the image — until
//! ONE promotion primitive moves it to a durable layout. `get`/`put` are
//! zero-syscall over the mapping: a grain write is a memcpy (shmem has no
//! `page_mkwrite`, no inode lock on the path, no host filesystem under it).
//!
//! PORTABLE: the segment record codec (the v4 header carries a checksum over
//! header + key + value — the reference trusted sane lengths and let a torn
//! payload decode as plausible bytes), the two-phase crash-safe replace over
//! a byte image, the pool budget with its 75 % high-water, the promotion
//! points as hooks, the recovery scan, the identity marker, and the reader's
//! view of an unpromoted entry (size-consistent zeros for OTHER clients —
//! `staged_payload_lost_reads`, stated and counted). LINUX-ONLY (`Ring`): the
//! memfd, its seals, `fallocate(PUNCH_HOLE)`/`madvise`, the claim socket and
//! the supervisor's handover.
//!
//! The ring key is `file_id[16] ‖ ino u64` so the recovery scan names the
//! inode without a path (Q-7/Q-10); the stage generation in the header makes
//! the scan's last-wins exact; a mount-recovered entry seeds the ledger at
//! generation 0, which no live assignment can forge (the counter starts at 1
//! — the generation ABA model below pins it — and, after a recovery, past
//! every generation the segment still carries).
//!
//! **Durability** (charter §1 item 10): a resident's bytes are RAM's — the
//! daemon's death keeps them (the supervisor holds the memfd and hands it
//! to the member's next mount), a power cut does not. fsync, fdatasync and
//! syncfs PROMOTE the ino's residents to the data volumes before their ack
//! (`promoteAtFsync` / `promoteAllAtSyncfs`; the plane's primitive flushes
//! the block before the layout naming it publishes — DUR-1); a promotion
//! the plane declines is an fsync that cannot be acked. A key leaves the
//! ring when the metadata stops naming its record — a promotion's layout
//! commit, a truncate to zero, a destroy — and its copies are released at
//! once: the layout publish rides the journal tail, which outlives the
//! daemon like the ring does.

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
const cache_mod = @import("../mem/cache.zig");

pub const FileId = layout.FileId;

pub const stats = struct {
    /// `current_staged_write_bytes`: the pool's charged footprint (the
    /// budget component `write_cache`'s gauge).
    pub var current_staged_write_bytes: counters.Gauge = .{};
    /// `staged_writes_in_flight`: ring keys resident or being placed.
    pub var staged_writes_in_flight: counters.Gauge = .{};
    /// `staging_cell_state_bytes`: the live cells' value-hash states
    /// (`@sizeOf(ValueHash)` each — the budget component
    /// `staging_cell_state`'s gauge); `≤ Segment.cellStateBound(capacity)`.
    pub var staging_cell_state_bytes: counters.Gauge = .{};
    /// `staging_tables_bytes`: the segment's per-cell tables (the cells in
    /// their chunks, the free list, the two grain bitmaps, the key index)
    /// — the budget component `staging_tables`' gauge; `≤ Segment.
    /// tablesBound(capacity)` per open segment.
    pub var staging_tables_bytes: counters.Gauge = .{};
    /// `staging_over_cap_refusals`: a stage refused `StorageFull` at the
    /// pool cap (the caller's spill escalation follows).
    pub var staging_over_cap_refusals: counters.ShardedCounter = .{};
    /// `staging_ring_full_refusals`: a placement found no hole (never
    /// destroys a live entry; the write takes its own-block arm — the ring
    /// is a cache, never the set's capacity). Every refusal counts here;
    /// `staging_ring_full_episodes` counts the EPISODES (`Pool.RingFull`):
    /// the first refusal opens one (ONE warning), the drain that frees a
    /// quarter of the ring closes it (one info line).
    pub var staging_ring_full_refusals: counters.ShardedCounter = .{};
    pub var staging_ring_full_episodes: counters.ShardedCounter = .{};
    /// `staging_pressure_kicks`: high-water crossings that kicked the write
    /// cache's drain (`evictUntil` through the promote sink).
    pub var staging_pressure_kicks: counters.ShardedCounter = .{};
    /// `promotion_pass_concurrent`: bulk passes (syncfs, dismount, the
    /// drain) that ran more than one promotion at once (`runPass`).
    pub var promotion_pass_concurrent: counters.ShardedCounter = .{};
    /// The write cache's sink: `write_cache_evict_promotions` — residents
    /// the LRU eviction promoted; `write_cache_inline_evictions_declined` —
    /// an insert at the bound met a victim and declined to promote it under
    /// the writer's lock (the write took its own-block arm).
    pub var write_cache_evict_promotions: counters.ShardedCounter = .{};
    pub var write_cache_inline_evictions_declined: counters.ShardedCounter = .{};
    /// The budget's Red sheds the write cache answered (one drain pass to
    /// the authority's target each; the bound is untouched).
    pub var write_cache_budget_sheds: counters.ShardedCounter = .{};
    /// `staging_recovery_records`: records a recovery scan adopted.
    pub var staging_recovery_records: counters.ShardedCounter = .{};
    /// `staging_recovery_torn_records`: headers whose checksum or geometry
    /// failed at recovery — dropped loud.
    pub var staging_recovery_torn_records: counters.ShardedCounter = .{};
    /// `staging_generation_discards`: a ring handed over under a dead
    /// identity (another set, a newer epoch, an unreadable marker) was
    /// wiped at bind.
    pub var staging_generation_discards: counters.ShardedCounter = .{};
    /// `staging_foreign_scope_refusals`: a ring handed over whose marker
    /// names ANOTHER member refused ours — never adopted, never wiped
    /// (0003 §T.1).
    pub var staging_foreign_scope_refusals: counters.ShardedCounter = .{};
    /// `staging_owner_live_refusals`: a claim another live mount holds
    /// refused ours — two mounts presenting one member scope.
    pub var staging_owner_live_refusals: counters.ShardedCounter = .{};
    /// `staged_payload_lost_reads`: zeros served for an unreachable staged
    /// payload (another client's live custody, or a crash discarded it).
    /// 0 on any client of a cleanly unmounted set (gate G-PK2).
    pub var staged_payload_lost_reads: counters.ShardedCounter = .{};
    /// `staging_backing_phase_ns`: the wall of the ring's backing syscalls
    /// (`fallocate(PUNCH_HOLE)` + `madvise` on a sweep's run) — they run
    /// on the caller's lane thread, so their tail IS a lane stall.
    pub var backing_reclaim_ns: counters.ShardedLatencyHistogram = .{};
    /// The lazy reclaim (`Segment.sweepDirty`): `staging_dirty_free_bytes`
    /// — grains freed and not yet punched nor reused; `staging_sweep_runs`
    /// / `staging_sweep_punched_bytes` — the free runs a sweep punched, one
    /// syscall pair per run (a per-record punch under the segment lock was
    /// 56–150 µs on every re-stage: the staged arm's second serialization
    /// point).
    pub var staging_dirty_free_bytes: counters.Gauge = .{};
    pub var staging_sweep_runs: counters.ShardedCounter = .{};
    pub var staging_sweep_punched_bytes: counters.ShardedCounter = .{};
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
    /// `fsync_promoted_files`: residents the fsync law promoted.
    /// `fsync_promote_superseded`: a sync point's promotion met a record
    /// the layout no longer names (a publish away from it committed — a
    /// truncate to zero, a destroy — its ring release following after the
    /// writer's lock dropped): superseded bytes, nothing owed to the point.
    /// `fsync_promote_corpses`: a sync point's promotion the store refused
    /// met a resident whose ino has no live inode (unlinked, its FORGET's
    /// destroy on its way): nothing owed to the point either.
    pub var fsync_promoted_files: counters.ShardedCounter = .{};
    pub var fsync_promote_superseded: counters.ShardedCounter = .{};
    pub var fsync_promote_corpses: counters.ShardedCounter = .{};
    /// The in-place overwrite (`Segment.overwriteInPlace`):
    /// `staging_inplace_overwrites` / `staging_inplace_bytes` — writes that
    /// landed in the resident record's own grain (header-first, no new
    /// slot, no image moved); `staging_read_retries` — a reader's copy that
    /// met a grain mid-write and copied again.
    pub var staging_inplace_overwrites: counters.ShardedCounter = .{};
    pub var staging_inplace_bytes: counters.ShardedCounter = .{};
    pub var staging_read_retries: counters.ShardedCounter = .{};
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
    inline for (.{ "current_staged_write_bytes", "staged_writes_in_flight", "staging_cell_state_bytes", "staging_tables_bytes", "staging_over_cap_refusals", "staging_ring_full_refusals", "staging_ring_full_episodes", "staging_pressure_kicks", "promotion_pass_concurrent", "write_cache_evict_promotions", "write_cache_inline_evictions_declined", "write_cache_budget_sheds", "staging_recovery_records", "staging_recovery_torn_records", "staging_generation_discards", "staging_foreign_scope_refusals", "staging_owner_live_refusals", "staged_payload_lost_reads", "dismount_promoted_files", "dismount_promoted_bytes", "dismount_promote_failures", "dismount_corpse_residents_destroyed", "dismount_unnamed_residents_destroyed", "fsync_promoted_files", "fsync_promote_superseded", "fsync_promote_corpses", "staging_dirty_free_bytes", "staging_sweep_runs", "staging_sweep_punched_bytes", "staging_inplace_overwrites", "staging_inplace_bytes", "staging_read_retries", "staging_appends", "staging_append_bytes", "staging_restages", "staging_restage_bytes" }, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("}");
}

pub fn writePhasesJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"backing_reclaim\":");
    try stats.backing_reclaim_ns.snapshot().writeJson(w);
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

    /// The write cache's index hash: the whole key (one entry per key).
    pub fn hash(self: StagingKey) u64 {
        return checksum.hash64(self.bytes());
    }

    /// The write cache's shard: the ino alone, so every key of an inode
    /// shares a shard (`invalidateIno` takes one lock).
    pub fn shardHash(self: StagingKey) u64 {
        return checksum.hash64(&self.ino_le);
    }

    pub fn eql(a: StagingKey, b: StagingKey) bool {
        return std.mem.eql(u8, a.bytes(), b.bytes());
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

/// The write cache's low water as a divisor of its bound: the drain
/// (`DataPlane.drainTask`) evicts to `bound − bound/4` — a quarter of the
/// bound of headroom is a burst's worth at the promotion's pace — and a
/// ring-full episode (`Pool.RingFull`) closes once the charged footprint
/// fell a quarter of the ring below its open: the drain's own target,
/// so the two never disagree on what "drained" means.
pub const WRITE_CACHE_DRAIN_DIVISOR: u64 = 4;

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

/// What backs the image: the write cache's memfd binds the punch and the
/// hole scan; a RAM image (tests, the portable core) binds nothing — or a
/// test's counting seam. The image is written directly in every form (a
/// grain write is a memcpy into the mapping).
pub const Backing = struct {
    ctx: ?*anyopaque = null,
    /// Return `[off, off + len)` to the kernel (punch + drop pages): a free
    /// run of tombstoned records, from a sweep (`Segment.sweepDirty`) —
    /// never per record on a hot path.
    reclaim: ?*const fn (ctx: ?*anyopaque, off: u64, len: u64) void = null,
    /// The first offset ≥ `off` that holds DATA (`SEEK_DATA`), null past
    /// the last. The recovery scan skips holes with it: reading a hole
    /// through the mapping would populate a page per grain (a drained
    /// ring is all holes). Null = scan every grain.
    nextData: ?*const fn (ctx: ?*anyopaque, off: u64) ?u64 = null,

    pub const none: Backing = .{};
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
/// LIVE for placement there: a leased copy is never overlaid.
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
    /// The record's seqlock against an in-place overwrite: odd while a
    /// grain is being rewritten; a reader's copy (`Lease.copyRange`) that
    /// saw it move copies again — a grain is read whole or not at all.
    seq: std.atomic.Value(u32) = .init(0),
    /// The retired list (under the segment lock).
    prev_retired: ?*Cell = null,
    next_retired: ?*Cell = null,
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

pub const OverwriteError = error{
    OutOfMemory,
    NotResident,
    Stale,
    /// Not inside one grain of the image: the caller re-stages.
    NoRoom,
};

/// One grain-bitmap word covers this many grains (the word type's width).
const WORD_BITS: usize = @bitSizeOf(u64);

pub const Segment = struct {
    gpa: Allocator,
    image: []u8,
    backing: Backing,
    lock: jc.SpinLock = .{},
    index: std.AutoHashMapUnmanaged([KEY_LEN]u8, *Cell) = .empty,
    /// The placement cursor: records go at/after it, wrapping.
    write_offset: u64 = 0,
    /// The cells, in chunks of `CELL_CHUNK` allocated as the population
    /// grows (a chunk never moves: the index and the leases hold cell
    /// pointers); `maxCells(capacity)` is the bound, never the allocation —
    /// a write cache of a quarter of RAM would pin a gigabyte of empty
    /// cells at mount.
    cells: std.ArrayList([]Cell) = .empty,
    free_cells: std.ArrayList(*Cell) = .empty,
    /// The live set at the grain: bit `g` set = grain `g` belongs to an
    /// indexed record, a placement in flight or a retired copy not yet
    /// reclaimed (a shadowed cell shares its successor's grains). Placement
    /// is a first-fit scan of clear runs from the cursor, wrapping — O(need)
    /// where the cursor leads free space, one pass over the words on a full
    /// ring — never a sort of the live extents: that scan (O(n log n) + an
    /// O(n²) fit check under the lock, per put) serialized every staged
    /// write of a mount at ≈ 1 k/s once the held copies grew the set.
    occupied: []u64,
    /// Grains freed (tombstoned, their cell reclaimed) and not yet punched
    /// nor reused: `dirty ⊆ ¬occupied`. A placement over them clears the
    /// bits (the record's bytes overwrite the host's); `sweepDirty` punches
    /// whole runs under a reservation, off every hot path.
    dirty: []u64,
    /// One sweep at a time (under the lock): a second caller returns.
    sweeping: bool = false,
    /// In-place overwrites between their check and their flip (seq_cst):
    /// a reader that met a grain mid-write parks on `inplace_drained`
    /// until the last one is out (`Lease.copyRange`) — a fiber park, never
    /// a spin on the lane thread (the writer may be a fiber of the same
    /// lane).
    inplace_in_flight: std.atomic.Value(u32) = .init(0),
    inplace_drained: std.atomic.Value(u32) = .init(0),
    /// Retired cells not yet reclaimed (`Cell` doc), under the lock.
    retired_head: ?*Cell = null,

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

    /// Cells per chunk: 480 KiB of cells, one allocation per 4,096 live
    /// records — a growth step a put pays once in four thousand, under the
    /// lock (the index doubles on its own law beside it).
    pub const CELL_CHUNK: usize = 4096;

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
        return @intCast((cap / RECORD_ALIGN + WORD_BITS - 1) / WORD_BITS);
    }

    /// The MOST RAM the per-cell tables can take at `cap`: `n = maxCells(cap)`
    /// cells, `n` free-list slots, the two grain bitmaps and the key index's
    /// slot arrays (its 3-word header is not counted) — the ceiling a full
    /// ring of one-grain records reaches; the tables grow toward it a chunk
    /// at a time (`tablesBytes` is the live figure).
    pub fn tablesBound(cap: u64) u64 {
        const n: u64 = maxCells(cap);
        return n * @sizeOf(Cell) + n * @sizeOf(*Cell) + 2 * @as(u64, occupiedWords(cap)) * @sizeOf(u64) + indexSlots(@intCast(n)) * INDEX_SLOT_BYTES;
    }

    /// What this segment's tables take now — the chunks allocated, the free
    /// list's capacity, the two bitmaps and the index's capacity.
    pub fn tablesBytes(self: *const Segment) u64 {
        var cells: u64 = 0;
        for (self.cells.items) |chunk| cells += chunk.len;
        return cells * @sizeOf(Cell) + @as(u64, self.free_cells.capacity) * @sizeOf(*Cell) + @as(u64, self.occupied.len + self.dirty.len) * @sizeOf(u64) + @as(u64, self.index.capacity()) * INDEX_SLOT_BYTES;
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
        const occupied = try gpa.alloc(u64, occupiedWords(image.len));
        errdefer gpa.free(occupied);
        @memset(occupied, 0);
        const dirty = try gpa.alloc(u64, occupiedWords(image.len));
        errdefer gpa.free(dirty);
        @memset(dirty, 0);
        var self: Segment = .{ .gpa = gpa, .image = image, .backing = backing, .occupied = occupied, .dirty = dirty };
        errdefer self.index.deinit(gpa);
        errdefer self.free_cells.deinit(gpa);
        errdefer {
            for (self.cells.items) |chunk| gpa.free(chunk);
            self.cells.deinit(gpa);
        }
        try self.growCellsLocked();
        try self.index.ensureTotalCapacity(gpa, @intCast(@min(CELL_CHUNK, maxCells(image.len))));
        stats.staging_tables_bytes.add(self.tablesBytes());
        return self;
    }

    /// One more chunk of cells onto the free list (under the lock; never
    /// past `maxCells`, which a hole for a record guarantees).
    fn growCellsLocked(self: *Segment) Allocator.Error!void {
        const before = self.tablesBytes();
        const want: usize = @min(CELL_CHUNK, maxCells(self.image.len) - self.cells.items.len * CELL_CHUNK);
        assert(want != 0);
        try self.cells.ensureUnusedCapacity(self.gpa, 1);
        // Precise: the free list holds every cell at once, no more.
        try self.free_cells.ensureTotalCapacityPrecise(self.gpa, self.cells.items.len * CELL_CHUNK + want);
        const chunk = try self.gpa.alloc(Cell, want);
        @memset(chunk, .{});
        self.cells.appendAssumeCapacity(chunk);
        var i: usize = want;
        while (i > 0) : (i -= 1) self.free_cells.appendAssumeCapacity(&chunk[i - 1]);
        if (self.cells.items.len > 1) stats.staging_tables_bytes.add(self.tablesBytes() - before);
    }

    pub fn deinit(self: *Segment) void {
        stats.staging_tables_bytes.sub(self.tablesBytes());
        for (self.cells.items) |chunk| {
            for (chunk) |*c| if (c.hash) |h| self.destroyState(h);
            self.gpa.free(chunk);
        }
        self.cells.deinit(self.gpa);
        self.index.deinit(self.gpa);
        self.free_cells.deinit(self.gpa);
        self.gpa.free(self.occupied);
        stats.staging_dirty_free_bytes.sub(self.dirtyFreeBytesLocked());
        self.gpa.free(self.dirty);
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

        /// `dst` ← the value at `off` (clipped to the value's end; the
        /// bytes copied), whole against a racing in-place overwrite: the
        /// record's seqlock read before and after the copy (the second
        /// read a seq_cst RMW: it orders the copy's loads before it), a
        /// moved or odd seq copies again — a few spins, then a FIBER park
        /// until a write drains (the writer may be a fiber of this very
        /// lane: a thread yield here starved its CQE and wedged the pair).
        pub fn copyRange(self: Lease, io: Io, dst: []u8, off: u64) usize {
            const e = self.cell.extent;
            if (off >= e.val_len) return 0;
            const n: usize = @intCast(@min(dst.len, e.val_len - off));
            const src = self.seg.image[@intCast(e.valOff() + off)..][0..n];
            var spins: u32 = 0;
            while (true) {
                const s0 = self.cell.seq.load(.acquire);
                if (s0 & 1 == 0) {
                    @memcpy(dst[0..n], src);
                    if (self.cell.seq.fetchAdd(0, .seq_cst) == s0) return n;
                }
                stats.staging_read_retries.inc();
                if (spins < READ_SPINS) {
                    spins += 1;
                    std.atomic.spinLoopHint();
                    continue;
                }
                self.seg.inplace_drained.store(0, .monotonic);
                if (self.seg.inplace_in_flight.load(.seq_cst) != 0) _ = tick.parkUntilUncancelable(io, &self.seg.inplace_drained, clock.monotonicNs() + READ_PARK_NS, null);
            }
        }

        /// Spins before a reader parks on a grain mid-write: a write's
        /// window is one page copy on another lane; the park's bound is
        /// the write's own order of magnitude (the drain's wake ends it).
        const READ_SPINS: u32 = 64;
        const READ_PARK_NS: u64 = 200 * std.time.ns_per_us;

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

    fn residentExtent(self: *Segment, key: StagingKey) ?Extent {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return null;
        return cell.extent;
    }

    // ---- the two-phase replace ----

    /// Phase 1 under the lock: the first clear run of the grain bitmap that
    /// holds the record (the live set INCLUDES the current copy of this key
    /// and every placement in flight), at/after the cursor first, then
    /// wrapped; its grains are taken. No fit ⇒ `RingFull`, loud — never
    /// destroys a live entry. Phase 1b outside the lock: the bytes. Phase 2
    /// under the lock: flip the index, retire the superseded copy (its
    /// grains free once the last reader leaves it). A same-key replace is
    /// ATOMIC for readers and torn-write immune (a crash between the
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
            self.reuseDirtyLocked(rec_off, need);
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
        errdefer self.destroyState(state);
        state.* = valueHash(value);
        writeRecord(self.image, placed.rec_off, key.bytes(), value, generation, fencing, state);
        self.lock.lock();
        defer self.lock.unlock();
        const cell = try self.allocCellLocked();
        const gop = self.index.getOrPut(self.gpa, key.bytes().*) catch |e| {
            self.free_cells.appendAssumeCapacity(cell);
            return e;
        };
        cell.* = .{ .refs = .init(1), .extent = placed, .key = key.bytes().*, .hash = state };
        if (gop.found_existing) {
            const old = gop.value_ptr.*;
            gop.value_ptr.* = cell;
            self.retireLocked(old);
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
        const cell = self.allocCellLocked() catch |e| {
            self.destroyState(state);
            if (old.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(old);
            return e;
        };
        var ne = ext;
        ne.val_len = @intCast(new_len);
        ne.generation = generation;
        ne.slot = slot;
        cell.* = .{ .refs = .init(1), .extent = ne, .key = key.bytes().*, .hash = state };
        self.index.putAssumeCapacity(key.bytes().*, cell); // the key is present: a replace never grows the map
        old.shadowed = true;
        old.retired.store(true, .release);
        self.linkRetiredLocked(old);
        // The index's reference and our lease leave; the last one out frees
        // the cell alone (shadowed: the record lives on under the successor).
        if (old.refs.fetchSub(2, .acq_rel) == 2) self.reclaimCellLocked(old);
    }

    /// The in-place overwrite: `data` lands in the resident record's OWN
    /// grain (`at` and `at + data.len` inside one grain of the value, the
    /// value's length unchanged). The order is what makes the write atomic
    /// against the daemon's death (the memfd outlives it): (1) the header
    /// with the NEW checksum — over the old grains plus the new one,
    /// hashed from the mapping and `data` — into the slot that is NOT
    /// current (the current slot still names the old bytes whole: a kill
    /// here recovers the old record); (2) the grain's memcpy (a kill after
    /// it recovers the new record, the new slot the valid one — a kill
    /// INSIDE it leaves the new header over a half-written grain, which
    /// verifies on neither slot: recovery drops the record, the same loss
    /// a kill mid-write of an unsynced file is allowed); (3) the RAM flip.
    /// Readers see old or new, never torn (`Lease.copyRange`'s seqlock);
    /// writers of a key are the caller's to serialize (the ino's layout
    /// lock). Not a new slot, not a copy of the image, not a superseded
    /// cell, nothing to punch — the 4 KiB rewrite of a small file at the
    /// cost of one hash of its image.
    pub fn overwriteInPlace(self: *Segment, io: Io, key: StagingKey, at: u64, data: []const u8, expect_generation: u64, generation: u64, fencing: u64) OverwriteError!void {
        assert(data.len != 0);
        const cell: *Cell, const ext: Extent = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const c = self.index.get(key.bytes().*) orelse return error.NotResident;
            const e = c.extent;
            if (e.generation != expect_generation) return error.Stale;
            if (at + data.len > e.val_len or at / RECORD_ALIGN != (at + data.len - 1) / RECORD_ALIGN) return error.NoRoom;
            // Registered BEFORE the seqlock steps (seq_cst): a reader that
            // meets the odd seq parks until this write is out.
            _ = self.inplace_in_flight.fetchAdd(1, .seq_cst);
            _ = c.refs.fetchAdd(1, .monotonic);
            self.bumpSeqLocked(c); // odd: a grain is moving
            break :blk .{ c, e };
        };
        errdefer {
            self.lock.lock();
            self.bumpSeqLocked(cell);
            self.inplaceDoneLocked(io);
            if (cell.refs.fetchSub(1, .acq_rel) == 1 and cell.retired.load(.acquire)) self.reclaimCellLocked(cell);
            self.lock.unlock();
        }
        const voff: usize = @intCast(ext.valOff());
        const value = self.image[voff .. voff + ext.val_len];
        // The new value's hash from the parts that stay and the part that
        // comes (the record's writers are serialized by the caller).
        const state = try self.createState();
        errdefer self.destroyState(state);
        state.* = .init();
        state.update(value[0..@intCast(at)]);
        state.update(data);
        state.update(value[@intCast(at + data.len)..]);
        const slot = ext.slot.other();
        writeHeader(self.image, ext.rec_off, slot, key.bytes(), ext.val_len, generation, fencing, state);
        @memcpy(self.image[@intCast(ext.valOff() + at)..][0..data.len], data);
        self.lock.lock();
        defer self.lock.unlock();
        self.bumpSeqLocked(cell); // even: the grain is whole
        self.inplaceDoneLocked(io);
        if (cell.retired.load(.acquire)) {
            // Gone meanwhile (the key left): the bytes belong to a record
            // nothing names.
            self.destroyState(state);
            if (cell.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(cell);
            return error.Stale;
        }
        if (cell.hash) |h| self.destroyState(h);
        cell.hash = state;
        cell.extent.slot = slot;
        cell.extent.generation = generation;
        _ = cell.refs.fetchSub(1, .acq_rel);
        stats.staging_inplace_overwrites.inc();
        stats.staging_inplace_bytes.add(data.len);
    }

    /// The record's seqlock step, on the cell and on every shadowed cell
    /// still leased over the same record (an in-place append's
    /// predecessors share its bytes): release pairs with `copyRange`'s
    /// acquire.
    fn bumpSeqLocked(self: *Segment, cell: *Cell) void {
        _ = cell.seq.fetchAdd(1, .release);
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) {
            if (c.shadowed and c.extent.rec_off == cell.extent.rec_off) _ = c.seq.fetchAdd(1, .release);
        }
    }

    /// One in-place write out; the last one wakes the readers parked on
    /// the drain (`Lease.copyRange`).
    fn inplaceDoneLocked(self: *Segment, io: Io) void {
        if (self.inplace_in_flight.fetchSub(1, .seq_cst) == 1) tick.wake(io, &self.inplace_drained);
    }

    /// Remove `key` iff present (and, when `expect_generation` is given,
    /// iff the resident copy carries it — the promotion's generation gate:
    /// a re-stage that landed since keeps its entry). True iff removed. The
    /// copy is tombstoned and retired; its grains free once the last reader
    /// leaves it (the layout publish that stopped naming it rides the
    /// journal tail: nothing durable is owed to the record).
    pub fn remove(self: *Segment, key: StagingKey, expect_generation: ?u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const cell = self.index.get(key.bytes().*) orelse return false;
        if (expect_generation) |g| if (cell.extent.generation != g) return false;
        _ = self.index.remove(key.bytes().*);
        self.retireLocked(cell);
        return true;
    }

    /// Retire a cell: tombstoned (BOTH slots' magic zeroed so a recovery
    /// scan never adopts the copy), onto the retired list, out of the
    /// index's reference; the last reference reclaims the bytes. release
    /// pairs with `Lease.release`'s acq_rel.
    fn retireLocked(self: *Segment, cell: *Cell) void {
        self.tombstoneLocked(cell.extent.rec_off);
        cell.retired.store(true, .release);
        self.linkRetiredLocked(cell);
        if (cell.refs.fetchSub(1, .acq_rel) == 1) self.reclaimCellLocked(cell);
    }

    fn tombstoneLocked(self: *Segment, rec_off: u64) void {
        self.tombstone(rec_off);
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

    fn reclaimCell(self: *Segment, cell: *Cell) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.reclaimCellLocked(cell);
    }

    fn reclaimCellLocked(self: *Segment, cell: *Cell) void {
        const e = cell.extent;
        self.unlinkRetiredLocked(cell);
        // A shadowed cell's bytes live on under its successor: free the cell
        // alone. Otherwise the grains are free for placement at once and
        // DIRTY for the sweep (the punch is batched, never here under the
        // lock).
        if (!cell.shadowed) {
            self.markLocked(e.rec_off, e.end() - e.rec_off, false);
            self.markDirtyLocked(e.rec_off, e.end() - e.rec_off);
        }
        if (cell.hash) |h| self.destroyState(h);
        cell.* = .{};
        self.free_cells.appendAssumeCapacity(cell);
    }

    /// A free cell, the next chunk allocated when the list is empty
    /// (bounded by construction: a hole for a record exists only while a
    /// cell for it can).
    fn allocCellLocked(self: *Segment) Allocator.Error!*Cell {
        if (self.free_cells.items.len == 0) try self.growCellsLocked();
        return self.free_cells.pop().?;
    }

    /// Take or return the grains of `[off, off + len)` in the live set.
    fn markLocked(self: *Segment, off: u64, len: u64, occupied: bool) void {
        assert(derive.isAligned(off, RECORD_ALIGN) and derive.isAligned(len, RECORD_ALIGN));
        var g: usize = @intCast(off / RECORD_ALIGN);
        const end: usize = @intCast((off + len) / RECORD_ALIGN);
        while (g < end) : (g += 1) {
            const mask = @as(u64, 1) << @intCast(g % WORD_BITS);
            if (occupied) {
                assert(self.occupied[g / WORD_BITS] & mask == 0);
                self.occupied[g / WORD_BITS] |= mask;
            } else {
                assert(self.occupied[g / WORD_BITS] & mask != 0);
                self.occupied[g / WORD_BITS] &= ~mask;
            }
        }
    }

    fn grainOccupied(self: *const Segment, g: usize) bool {
        return self.occupied[g / WORD_BITS] & (@as(u64, 1) << @intCast(g % WORD_BITS)) != 0;
    }

    /// Freed grains await the sweep.
    fn markDirtyLocked(self: *Segment, off: u64, len: u64) void {
        var g: usize = @intCast(off / RECORD_ALIGN);
        const end: usize = @intCast((off + len) / RECORD_ALIGN);
        while (g < end) : (g += 1) self.dirty[g / WORD_BITS] |= @as(u64, 1) << @intCast(g % WORD_BITS);
        stats.staging_dirty_free_bytes.add(len);
    }

    /// A placement over dirty grains: the record's bytes overwrite the
    /// host's, nothing to punch.
    fn reuseDirtyLocked(self: *Segment, off: u64, len: u64) void {
        var g: usize = @intCast(off / RECORD_ALIGN);
        const end: usize = @intCast((off + len) / RECORD_ALIGN);
        var reused: u64 = 0;
        while (g < end) : (g += 1) {
            const mask = @as(u64, 1) << @intCast(g % WORD_BITS);
            if (self.dirty[g / WORD_BITS] & mask != 0) {
                self.dirty[g / WORD_BITS] &= ~mask;
                reused += RECORD_ALIGN;
            }
        }
        stats.staging_dirty_free_bytes.sub(reused);
    }

    fn dirtyFreeBytesLocked(self: *const Segment) u64 {
        var n: u64 = 0;
        for (self.dirty) |w| n += @popCount(w);
        return n * RECORD_ALIGN;
    }

    /// Grains freed and not yet punched nor reused (the sweep's trigger).
    pub fn dirtyFreeBytes(self: *Segment) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return self.dirtyFreeBytesLocked();
    }

    /// The lazy reclaim: every dirty free run punched and its pages
    /// dropped (`Backing.reclaim`), ONE syscall pair per run — the run is
    /// RESERVED (its grains occupied) while the host works on it, so no
    /// placement lands under the punch; the lock is never held across the
    /// syscall. Run off the hot path: at the dirty threshold (the plane's
    /// task), at every dismount (a cleanly unmounted ring scans EMPTY).
    pub fn sweepDirty(self: *Segment) void {
        const reclaim = self.backing.reclaim orelse {
            // A RAM image: the grains are clean by definition.
            self.lock.lock();
            defer self.lock.unlock();
            stats.staging_dirty_free_bytes.sub(self.dirtyFreeBytesLocked());
            @memset(self.dirty, 0);
            return;
        };
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.sweeping) return;
            self.sweeping = true;
        }
        defer {
            self.lock.lock();
            self.sweeping = false;
            self.lock.unlock();
        }
        while (true) {
            const run: struct { off: u64, len: u64 } = blk: {
                self.lock.lock();
                defer self.lock.unlock();
                const total: usize = @intCast(self.image.len / RECORD_ALIGN);
                var g: usize = 0;
                // The first dirty grain, a word at a time.
                while (g < total and self.dirty[g / WORD_BITS] == 0) g += WORD_BITS;
                while (g < total and self.dirty[g / WORD_BITS] & (@as(u64, 1) << @intCast(g % WORD_BITS)) == 0) g += 1;
                if (g >= total) return;
                var e = g;
                while (e < total and self.dirty[e / WORD_BITS] & (@as(u64, 1) << @intCast(e % WORD_BITS)) != 0) : (e += 1) {
                    const mask = @as(u64, 1) << @intCast(e % WORD_BITS);
                    assert(self.occupied[e / WORD_BITS] & mask == 0);
                    self.occupied[e / WORD_BITS] |= mask; // reserved for the punch
                }
                break :blk .{ .off = @as(u64, g) * RECORD_ALIGN, .len = @as(u64, e - g) * RECORD_ALIGN };
            };
            reclaim(self.backing.ctx, run.off, run.len);
            stats.staging_sweep_runs.inc();
            stats.staging_sweep_punched_bytes.add(run.len);
            self.lock.lock();
            defer self.lock.unlock();
            var g: usize = @intCast(run.off / RECORD_ALIGN);
            const end: usize = @intCast((run.off + run.len) / RECORD_ALIGN);
            while (g < end) : (g += 1) {
                const mask = @as(u64, 1) << @intCast(g % WORD_BITS);
                self.occupied[g / WORD_BITS] &= ~mask;
                self.dirty[g / WORD_BITS] &= ~mask;
            }
            stats.staging_dirty_free_bytes.sub(run.len);
        }
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
            if (g % WORD_BITS == 0 and g + WORD_BITS <= total) {
                const word = self.occupied[g / WORD_BITS];
                if (word == std.math.maxInt(u64)) {
                    run = 0;
                    g += WORD_BITS;
                    continue;
                }
                if (word == 0) {
                    run += WORD_BITS;
                    g += WORD_BITS;
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
        /// The scan stopped short: no RAM for another cell (the records
        /// past the stop stay on the segment unadopted — loud).
        out_of_memory: bool = false,
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
            const gop = self.index.getOrPut(self.gpa, k) catch {
                rep.out_of_memory = true;
                break;
            };
            if (gop.found_existing) {
                const old = gop.value_ptr.*;
                // Later offset wins a tie: the scan is ascending.
                if (h.generation >= old.extent.generation) {
                    gop.value_ptr.* = self.allocCellLocked() catch {
                        rep.out_of_memory = true;
                        break;
                    };
                    gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k };
                    self.markLocked(e.rec_off, e.reserved, true);
                    self.retireLocked(old);
                } else {
                    self.tombstone(off);
                    self.markDirtyLocked(off, e.end() - off);
                }
                rep.superseded += 1;
            } else {
                gop.value_ptr.* = self.allocCellLocked() catch {
                    _ = self.index.remove(k);
                    rep.out_of_memory = true;
                    break;
                };
                gop.value_ptr.*.* = .{ .refs = .init(1), .extent = e, .key = k };
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
        if (rep.out_of_memory) log.err("staging recovery: out of memory after {d} record(s) — the rest of the {d} B segment stays unadopted until a mount with more RAM", .{ rep.adopted, self.image.len });
        return rep;
    }

    /// Every resident key with its footprint (the ledger seed, the dismount
    /// and syncfs passes, fsck), sized AND filled under the ONE lock hold:
    /// a caller that sized by `live()` and then filled met a stage landing
    /// in between (syncfs under four fsstress writers) one entry past its
    /// buffer. The caller frees the slice.
    pub fn residentsAlloc(self: *Segment, gpa: Allocator) Allocator.Error![]Census {
        self.lock.lock();
        defer self.lock.unlock();
        const out = try gpa.alloc(Census, self.index.count());
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

    /// The highest generation any resident carries (the pool's seed after a
    /// recovery: a fresh generation must beat every one still on the
    /// segment, or a superseded copy a cut left would win the next scan).
    pub fn maxGeneration(self: *Segment) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        var g: u64 = 0;
        var it = self.index.valueIterator();
        while (it.next()) |c| g = @max(g, c.*.extent.generation);
        var cur = self.retired_head;
        while (cur) |c| : (cur = c.next_retired) g = @max(g, c.extent.generation);
        return g;
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

pub const Census = struct { key: StagingKey, cost: u64, generation: u64 };

/// A byte range of the image.
pub const Span = struct { off: u64, len: u64 };

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

/// A live promotion's verdict on one resident (`PromotionHooks.promote`).
pub const LiveVerdict = enum {
    /// Promoted: the entry left the ring onto its durable home.
    promoted,
    /// Declined for a resource reason (no volume, the arm refused, a
    /// commit failed): the entry stays resident; a sync point cannot ack.
    declined,
    /// Declined because NO DATA VOLUME COULD ALLOCATE the block (the
    /// arm's `StorageFull`: the own-block mint, the pack refill): the entry
    /// stays resident and dirty; a sync point answers ENOSPC — the store's
    /// honest word, never EIO — and the next sync point retries.
    no_space,
    /// The promotion failed and the ino has no live inode (`nlink == 0`:
    /// unlinked, its FORGET's destroy releases the record; an unlinked-open
    /// file reads it until then): nothing is owed to a sync point — the
    /// application removed the file; a full store answered ENOSPC to the
    /// syncfs right after the `rm` of what it could not sync.
    corpse,
    /// The ino's layout no longer names this record (kind not staged,
    /// another file id, no layout at all): a publish away from it committed
    /// under the writer's lock — a truncate to zero, a promotion, a destroy
    /// — and the ring release follows after that lock drops, so a promotion
    /// in between meets superseded bytes. Nothing is owed to a sync point
    /// (the metadata that supersedes them is what the point's meta barrier
    /// lands); the record stays until its release (an unlinked-open file
    /// reads its custody until its FORGET; the dismount pass judges what a
    /// crash left).
    unnamed,
};

/// The three promotion points reach the ONE primitive through this seam
/// (`data/pack.zig`'s prepare → commit); the ring never promotes inline.
pub const PromotionHooks = struct {
    ctx: ?*anyopaque = null,
    /// Promote now (fsync, syncfs, the write cache's eviction).
    promote: ?*const fn (ctx: ?*anyopaque, key: StagingKey) LiveVerdict = null,
    /// The dismount pass: promote, keep, or destroy a corpse's or an
    /// unnamed record (nothing is in flight: the session is closed).
    dismount: ?*const fn (ctx: ?*anyopaque, key: StagingKey) DismountVerdict = null,
    /// A promotion BATCH begins (one fsync, one syncfs pass, one dismount
    /// pass, one drain pass, one recall): the packed arm a `StorageFull`
    /// refill stopped for the previous batch re-arms (`pack.Packer.
    /// beginPromotionBatch`, OQ-1) — never re-armed, the arm stayed dead
    /// for the mount after the first full-store refill, and every small
    /// file's sync point answered ENOSPC long after space returned.
    begin_batch: ?*const fn (ctx: ?*anyopaque) void = null,
    /// The bulk passes' promotions in flight (`runPass`): the plane's
    /// derivation (`DataPlane.promotionPassWidth`); 1 = one at a time.
    width: usize = 1,

    pub const none: PromotionHooks = .{};

    pub fn beginBatch(self: PromotionHooks) void {
        if (self.begin_batch) |f| f(self.ctx);
    }
};

/// `claim`: the resident's claim on the STORE (`StoreClaim.of` the image).
const LedgerRow = struct { cost: u64, generation: u64, claim: u64 };

/// A resident's claim on the STORE: the bytes its promotion lands on, by
/// the primitive's dispatch on the image's length (`pack.dispatch`): fits
/// inline ⇒ nothing; a packed slot (`layout.ceilGrain`, `pack.packSlotLen`)
/// ⇒ the slot; past the pack's max slot ⇒ one whole block. The pool sums it
/// (`claimedBytes`) for the admission against the store and for `df`
/// (plane.zig: `writeCacheAdmits`, `statfs`; `enospc.storeCoversRing`);
/// the ring footprint (`cost`) is the RAM side. A pool with no store
/// behind it (the portable rigs) claims nothing.
pub const StoreClaim = struct {
    inline_max: u64 = 0,
    /// 0 = packing off: every image above inline takes a block.
    max_slot: u64 = 0,
    /// 0 = no store behind the ring.
    block: u64 = 0,

    pub const none: StoreClaim = .{};

    pub fn of(self: StoreClaim, image_len: u64) u64 {
        if (self.block == 0 or image_len <= self.inline_max) return 0;
        const slot = layout.ceilGrain(image_len);
        return if (slot <= self.max_slot) slot else self.block;
    }
};

pub const Pool = struct {
    gpa: Allocator,
    hooks: PromotionHooks,
    lock: jc.SpinLock = .{},
    /// Every charge/credit goes through the ledger so the gauge can never
    /// ratchet: `Σ cost == current_staged_write_bytes`.
    ledger: std.AutoHashMapUnmanaged(FileId, LedgerRow) = .empty,
    current: std.atomic.Value(u64) = .init(0),
    /// `Σ claim` over the ledger: the residents' claim on the store.
    claimed: std.atomic.Value(u64) = .init(0),
    /// The plane sets it before the ring is seeded (`StoreClaim.none` on a
    /// pool with no store behind it).
    claim_rule: StoreClaim = .none,
    next_generation: std.atomic.Value(u64) = .init(FIRST_GENERATION),
    /// The ring-full EPISODE (under `lock`): opened by the first placement
    /// that found no hole, closed by the drain that frees a quarter of the
    /// ring (`WRITE_CACHE_DRAIN_DIVISOR`) since it opened — never by a
    /// placement that lands: a near-full ring lands its small records
    /// between the large ones it refuses, and an episode ended by any
    /// landing logged one line per refusal (generic/475: 5,790 lines in
    /// 161 s, every one already counted). One warning at the open, one
    /// info line with the tally at the close.
    ring_full: RingFull = .{},

    pub const RingFull = struct {
        open: bool = false,
        since_ns: u64 = 0,
        refusals: u64 = 0,
        /// The charged footprint at the open; the episode closes once the
        /// footprint is a quarter of the ring below it.
        charged_at_open: u64 = 0,
        drain_below: u64 = 0,
    };

    pub fn init(gpa: Allocator, hooks: PromotionHooks) Pool {
        return .{ .gpa = gpa, .hooks = hooks };
    }

    /// A placement found no hole for a `need` B record in `seg`: the first
    /// refusal of an episode is the one that logs; every refusal counts.
    fn noteRingFull(self: *Pool, seg: *Segment, need: u64) void {
        const opened: ?RingFull = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            self.ring_full.refusals += 1;
            if (self.ring_full.open) break :blk null;
            const charged = self.current.load(.monotonic);
            self.ring_full = .{ .open = true, .since_ns = clock.monotonicNs(), .refusals = 1, .charged_at_open = charged, .drain_below = charged -| seg.capacity() / WRITE_CACHE_DRAIN_DIVISOR };
            break :blk self.ring_full;
        };
        const e = opened orelse return;
        stats.staging_ring_full_episodes.inc();
        log.warn("staging ring full: a {d} B record found no hole in a {d} B segment ({d} live entries, {d} B charged) — writes take their own-block arm while the ring stays full (counted staging_ring_full_refusals; one line per episode: the next is the drain's)", .{ need, seg.capacity(), seg.live(), e.charged_at_open });
    }

    /// Under `lock`, after a credit: the drain reached a quarter of the
    /// ring below the open's footprint — the episode closes; the caller
    /// logs the tally outside the lock.
    fn ringFullDrainedLocked(self: *Pool) ?RingFull {
        if (!self.ring_full.open or self.current.load(.monotonic) > self.ring_full.drain_below) return null;
        const e = self.ring_full;
        self.ring_full = .{};
        return e;
    }

    pub fn deinit(self: *Pool) void {
        stats.current_staged_write_bytes.sub(self.current.load(.monotonic));
        self.ledger.deinit(self.gpa);
        self.* = undefined;
    }

    pub fn currentBytes(self: *const Pool) u64 {
        return self.current.load(.monotonic);
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

    /// The key's resident claim on the store (the plane's admission reads
    /// the growth over it; none = no resident).
    pub fn claimOf(self: *Pool, file_id: FileId) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const row = self.ledger.get(file_id) orelse return null;
        return row.claim;
    }

    /// The residents' claim on the store (`StoreClaim`), summed.
    pub fn claimedBytes(self: *const Pool) u64 {
        return self.claimed.load(.monotonic);
    }

    /// Charge `cost` to `file_id` (replacing a prior row) at `generation`;
    /// `image_len` sets the row's store claim (null keeps the prior row's:
    /// an in-place overwrite changes no length).
    pub fn charge(self: *Pool, file_id: FileId, cost: u64, generation: u64, image_len: ?u64) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.ledger.getOrPut(self.gpa, file_id);
        const prior: u64 = if (gop.found_existing) gop.value_ptr.cost else 0;
        const prior_claim: u64 = if (gop.found_existing) gop.value_ptr.claim else 0;
        const claim = if (image_len) |n| self.claim_rule.of(n) else prior_claim;
        gop.value_ptr.* = .{ .cost = cost, .generation = generation, .claim = claim };
        _ = self.current.fetchSub(prior, .monotonic);
        _ = self.current.fetchAdd(cost, .monotonic);
        _ = self.claimed.fetchSub(prior_claim, .monotonic);
        _ = self.claimed.fetchAdd(claim, .monotonic);
        stats.current_staged_write_bytes.sub(prior);
        stats.current_staged_write_bytes.add(cost);
    }

    /// Credit `file_id`'s row (promotion released it, unlink, a stale
    /// re-stage). Generation-gated when given. Answers the credited cost.
    pub fn credit(self: *Pool, file_id: FileId, expect_generation: ?u64) ?u64 {
        const cost: u64, const drained: ?RingFull = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const row = self.ledger.get(file_id) orelse return null;
            if (expect_generation) |g| if (row.generation != g) return null;
            _ = self.ledger.remove(file_id);
            _ = self.current.fetchSub(row.cost, .monotonic);
            _ = self.claimed.fetchSub(row.claim, .monotonic);
            stats.current_staged_write_bytes.sub(row.cost);
            break :blk .{ row.cost, self.ringFullDrainedLocked() };
        };
        if (drained) |e| log.info("staging ring drained: the full episode is over after {d} refusal(s) in {d} ms ({d} B charged at its open, {d} B now) — placements resume", .{ e.refusals, (clock.monotonicNs() -| e.since_ns) / std.time.ns_per_ms, e.charged_at_open, self.current.load(.monotonic) });
        return cost;
    }

    /// Seed the ledger from a recovered segment at generation 0, and the
    /// generation counter PAST every generation the segment still carries:
    /// a re-stage of a recovered record must out-rank it on the next scan
    /// (a cut between the re-stage's fsync and the old copy's release left
    /// both; the scan takes the higher generation — a counter restarting at
    /// 1 handed the OLD bytes back).
    pub fn seedFromRecovery(self: *Pool, seg: *Segment) Allocator.Error!void {
        const out = try seg.residentsAlloc(self.gpa);
        defer self.gpa.free(out);
        // A recovered record claims NOTHING on the store until a live write
        // re-stages it: its standing is unjudged until a promotion point
        // meets it (named and owed, or superseded by the publish the kill
        // landed before its release — the two-ring [C4] row: the record of
        // a file already on its own block held `df` a block above its
        // baseline after the file's rm, until the dismount pass destroyed
        // it). What a kill leaves in flight is a handful of records; a
        // named one's sync point keeps the terminal verdict.
        for (out) |r| try self.charge(r.key.file_id, r.cost, RECOVERED_GENERATION, null);
        const floor = seg.maxGeneration() + 1;
        if (floor > self.next_generation.load(.monotonic)) self.next_generation.store(floor, .monotonic);
    }
};

// ---------------------------------------------------------------------------
// §3.4 The write cache: the ONE cache core over the ring (`mem/cache.zig`),
// `Cache(Resident, PromoteSink)` — the recency order (a resident's touch is
// its last write or read), the bound from `SQUEEZEFS_WRITE_CACHE_PCT`, and
// the eviction that PROMOTES the least recently used resident to the data
// volumes before its grains are released (owner ruling 2026-10-21: LRU,
// the sharding the one approximation).
// ---------------------------------------------------------------------------

/// One resident as the cache core sees it: the ring key, its ino (the
/// shard), the bytes it charges (its reserved stride) and the generation
/// the record carries (the sink's gate: a superseded entry owes nothing).
/// No epoch: a resident's bytes are the truth whatever landed on the inode
/// since (the staging key's generation gate is the identity law), so the
/// instance binds no `EpochSource`.
pub const Resident = struct {
    key: StagingKey,
    ino: u64,
    epoch: u64 = 0,
    bytes: u32,
    generation: u64,

    pub const Key = StagingKey;
};

/// The write direction's sink: an eviction for the BOUND promotes the
/// resident through the plane's primitive (the data volume's DMA and its
/// flush, the layout publish) and releases it iff it left the ring —
/// `kept` backs the pass off (a declined promotion loses no byte); an
/// `invalidate` (the caller superseded or removed the record: a re-stage, a
/// promotion that landed, an unlink) and `deinit` release at once. A sink
/// run INLINE by an insert (`stageWrite` under a writer's layout lock)
/// never promotes — a promotion takes ANOTHER ino's layout lock, and two
/// writers evicting each other's residents would deadlock — it answers
/// `kept`, the insert refuses, the writer takes its own-block arm and the
/// drain task (`evictUntil` off every lock) makes the room.
pub const PromoteSink = struct {
    pool: *Pool,
    hooks: PromotionHooks,

    pub const runs_under_lock = false;

    /// Set around `Cache.insert` by `stageWrite` (thread-local: the insert
    /// never parks between the set and the clear).
    threadlocal var inline_insert: bool = false;

    pub fn onEvict(self: *PromoteSink, e: *const Resident, why: cache_mod.Why) cache_mod.Outcome {
        switch (why) {
            .invalidate, .deinit, .stale => return .released,
            .bound => {},
        }
        if (inline_insert) {
            stats.write_cache_inline_evictions_declined.inc();
            return .kept;
        }
        const g = self.pool.generationOf(e.key.file_id) orelse return .released;
        if (g != e.generation) return .released;
        const promote = self.hooks.promote orelse return .kept;
        if (promote(self.hooks.ctx, e.key) == .promoted) {
            stats.write_cache_evict_promotions.inc();
            return .released;
        }
        // Declined, or unnamed (its release is on its way after the
        // writer's lock): the pass backs off; the entry leaves on its release.
        return if (self.pool.generationOf(e.key.file_id) == null) .released else .kept;
    }
};

pub const WriteCache = cache_mod.Cache(Resident, PromoteSink);

/// The smallest record: two grains, the header and one of value.
const WRITE_CACHE_ENTRY_FLOOR: u64 = 2 * RECORD_ALIGN;

/// The cache's per-shard sizing: the item table one item per possible
/// resident at the smallest record (at least 16); the index grows on
/// demand to the same count (`cache.Items`: an idle ring's index is the
/// seed's, not the sealed ring's — 336 MiB of maps at a 27 GiB ring).
pub fn writeCacheItems(bound: u64, shards: u32) cache_mod.Items {
    return .{ .max = @intCast(@max(bound / WRITE_CACHE_ENTRY_FLOOR / shards, 16)), .entry_floor = WRITE_CACHE_ENTRY_FLOOR, .slack = 16 };
}

/// Touch `key` in the cache (its last write or read): a shard lock, a map
/// probe, a relink. A key the cache does not hold is a miss (the portable
/// tests, a double) — nothing else happens.
pub fn touch(cache: ?*WriteCache, key: StagingKey) void {
    const c = cache orelse return;
    _ = c.lookup(key, {}, noVisit);
}

fn noVisit(_: void, _: *const Resident) void {}

/// §3.4 Admission: the cost is the record's ring footprint; the cache core
/// admits it at the head of its shard (`insert` — a key the core holds is
/// invalidated first: the re-stage supersedes it) or refuses `StorageFull`
/// at the bound (the caller's own-block arm — never a byte recorded the
/// ring cannot hold; the drain task makes room off every lock); place
/// (two-phase, never destroying a live entry); ledger row `(cost,
/// generation)`.
pub fn stageWrite(pool: *Pool, seg: *Segment, cache: ?*WriteCache, key: StagingKey, payload: []const u8, fencing: u64, slack: Slack) StageError!Staged {
    const padded = reserveFor(KEY_LEN, payload.len, slack);
    const prior_row = pool.costOf(key.file_id);
    const generation = pool.nextGeneration();
    if (cache) |c| {
        if (prior_row != null) _ = c.invalidateKey(key);
        PromoteSink.inline_insert = true;
        defer PromoteSink.inline_insert = false;
        switch (c.insert(.{ .key = key, .ino = key.ino(), .bytes = @intCast(padded), .generation = generation }, .{})) {
            .inserted => {},
            .exists, .refused => {
                stats.staging_over_cap_refusals.inc();
                return error.StorageFull;
            },
        }
    }
    errdefer if (cache) |c| {
        _ = c.invalidateKey(key);
    };
    seg.put(key, payload, generation, fencing, padded) catch |e| switch (e) {
        error.RingFull => {
            pool.noteRingFull(seg, padded);
            return error.StorageFull;
        },
        error.OutOfMemory => return error.OutOfMemory,
    };
    if (prior_row != null) {
        // A re-stage under a resident key moved the image out of the ring
        // (the caller's RMW source) and back in.
        stats.staging_restages.inc();
        stats.staging_restage_bytes.add(2 * payload.len);
    }
    try pool.charge(key.file_id, padded, generation, payload.len);
    return .{ .generation = generation };
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
    try pool.charge(key.file_id, cost, generation, at + data.len);
    stats.staging_appends.inc();
    stats.staging_append_bytes.add(data.len);
    return .{ .generation = generation };
}

/// The in-place overwrite inside one grain of the resident record (a
/// fresh generation, the ledger row re-stamped at the same cost); `null`
/// = not in place (no ring, no resident copy, a span outside one grain,
/// the copy moved under the caller): the caller re-stages the whole
/// image. Never kicks the drain: the footprint is the same.
pub fn overwriteStaged(pool: *Pool, seg: *Segment, io: Io, key: StagingKey, at: u64, data: []const u8, fencing: u64) Allocator.Error!?Staged {
    const resident = pool.generationOf(key.file_id) orelse return null;
    const generation = pool.nextGeneration();
    seg.overwriteInPlace(io, key, at, data, resident, generation, fencing) catch |e| switch (e) {
        error.NotResident, error.Stale, error.NoRoom => return null,
        error.OutOfMemory => return error.OutOfMemory,
    };
    const cost = pool.costOf(key.file_id) orelse 0;
    try pool.charge(key.file_id, cost, generation, null);
    return .{ .generation = generation };
}

/// The promotion released the ring entry (generation-gated: a re-stage that
/// landed since keeps its copy and its charge): the key leaves the ring
/// and the pool at once; its copies depart (`Segment.remove`) — the slot is
/// the ring's again at the cover round. True iff released.
pub fn releaseStaged(pool: *Pool, seg: *Segment, cache: ?*WriteCache, key: StagingKey, generation: u64) bool {
    if (!seg.remove(key, generation)) return false;
    _ = pool.credit(key.file_id, generation);
    if (cache) |c| _ = c.invalidateKey(key);
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
/// before "Dismount clean". `hooks.width` residents at once (`runPass`).
pub fn promoteAllAtDismount(gpa: Allocator, io: Io, seg: *Segment, hooks: PromotionHooks) Allocator.Error!DismountTally {
    const judge = hooks.dismount orelse return .{};
    const out = try seg.residentsAlloc(gpa);
    defer gpa.free(out);
    hooks.beginBatch();
    var pass: DismountPass = .{ .residents = out, .hooks = hooks, .judge = judge };
    runPass(io, &pass, hooks.width);
    return .{ .promoted = pass.promoted.load(.monotonic), .promoted_bytes = pass.promoted_bytes.load(.monotonic), .kept = pass.kept.load(.monotonic), .corpses = pass.corpses.load(.monotonic), .unnamed = pass.unnamed.load(.monotonic) };
}

const DismountPass = struct {
    residents: []const Census,
    hooks: PromotionHooks,
    judge: *const fn (ctx: ?*anyopaque, key: StagingKey) DismountVerdict,
    next: std.atomic.Value(usize) = .init(0),
    promoted: std.atomic.Value(u64) = .init(0),
    promoted_bytes: std.atomic.Value(u64) = .init(0),
    kept: std.atomic.Value(u64) = .init(0),
    corpses: std.atomic.Value(u64) = .init(0),
    unnamed: std.atomic.Value(u64) = .init(0),

    fn work(self: *DismountPass) void {
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= self.residents.len) return;
            const r = self.residents[i];
            switch (self.judge(self.hooks.ctx, r.key)) {
                .promoted => {
                    _ = self.promoted.fetchAdd(1, .monotonic);
                    _ = self.promoted_bytes.fetchAdd(r.cost, .monotonic);
                    stats.dismount_promoted_files.inc();
                    stats.dismount_promoted_bytes.add(r.cost);
                },
                .kept => {
                    _ = self.kept.fetchAdd(1, .monotonic);
                    stats.dismount_promote_failures.inc();
                },
                .corpse => {
                    _ = self.corpses.fetchAdd(1, .monotonic);
                    stats.dismount_corpse_residents_destroyed.inc();
                },
                .unnamed => {
                    _ = self.unnamed.fetchAdd(1, .monotonic);
                    stats.dismount_unnamed_residents_destroyed.inc();
                },
            }
        }
    }
};

/// The bulk passes (syncfs, dismount) run `width` promotions at once, each
/// worker taking the next resident off one shared cursor (decision 0003
/// §CD). WHY: a promotion waits on the device twice — its tenant's DMA and
/// its volume's flush before the layout publishes (DUR-1) — around one
/// commit. One at a time, every small resident paid a flush of its own;
/// in flight together, the volume's barrier coalescer answers them with
/// one (`fsync.BarrierCoalescer`). The workers never cancel: a pass is a
/// sync point or the unmount (charter §9).
/// The cursor and the tallies are monotonic: the residents are written
/// before the group spawns its workers and the tallies read after its
/// await — the spawn and the join are the two fences; the cursor's one RMW
/// hands each index out exactly once.
fn runPass(io: Io, pass: anytype, width: usize) void {
    const T = @TypeOf(pass.*);
    const w = @max(1, @min(width, pass.residents.len));
    if (w == 1) return pass.work();
    stats.promotion_pass_concurrent.inc();
    var group: Io.Group = .init;
    for (0..w) |_| group.async(io, T.work, .{pass});
    group.await(io) catch |e| switch (e) {
        error.Canceled => unreachable, // a sync point and the unmount are never cancelled (charter §9)
    };
}

/// What the fsync point's promotion found.
pub const FsyncPromote = enum {
    /// The resident left the ring for its durable home.
    promoted,
    /// Nothing owed: nothing resident under the key (promoted already,
    /// never staged here), or a resident the layout no longer names
    /// (`LiveVerdict.unnamed`: superseded by a committed publish whose ring
    /// release is on its way).
    absent,
    /// Resident and NOT promoted (a resource decline, a commit failure):
    /// on the RAM write cache the fsync cannot be acked — EIO.
    kept,
    /// Resident and NOT promoted because the store could not allocate its
    /// block (`LiveVerdict.no_space`): the fsync answers ENOSPC; the bytes
    /// stay resident and dirty, the next sync point retries.
    kept_no_space,
};

/// The fsync point (the write cache's law, module doc): promote BEFORE the
/// data barrier, so the barrier covers the block; the caller answers
/// `kept` with EIO and `kept_no_space` with ENOSPC. One fsync is one
/// promotion batch.
pub fn promoteAtFsync(seg: *Segment, hooks: PromotionHooks, key: StagingKey) FsyncPromote {
    if (seg.residentGeneration(key) == null) return .absent;
    hooks.beginBatch();
    return promoteOne(seg, hooks, key);
}

fn promoteOne(seg: *Segment, hooks: PromotionHooks, key: StagingKey) FsyncPromote {
    const promote = hooks.promote orelse return .kept;
    const kept: FsyncPromote = switch (promote(hooks.ctx, key)) {
        .promoted => {
            stats.fsync_promoted_files.inc();
            return .promoted;
        },
        .unnamed => {
            stats.fsync_promote_superseded.inc();
            return .absent;
        },
        .corpse => {
            stats.fsync_promote_corpses.inc();
            return .absent;
        },
        .declined => .kept,
        .no_space => .kept_no_space,
    };
    // Gone meanwhile (a pressure promotion raced): nothing owed.
    return if (seg.residentGeneration(key) != null) kept else .absent;
}

/// What the syncfs point left resident.
pub const SyncfsKept = struct { declined: u64 = 0, no_space: u64 = 0 };

/// The syncfs point: `promoteAtFsync` for EVERY resident entry, `hooks.
/// width` at once (`runPass`); answers how many stayed resident by class
/// (the caller answers EIO for any declined, else ENOSPC for any the store
/// could not take).
pub fn promoteAllAtSyncfs(gpa: Allocator, io: Io, seg: *Segment, hooks: PromotionHooks) Allocator.Error!SyncfsKept {
    const out = try seg.residentsAlloc(gpa);
    defer gpa.free(out);
    hooks.beginBatch();
    var pass: SyncfsPass = .{ .residents = out, .seg = seg, .hooks = hooks };
    runPass(io, &pass, hooks.width);
    return .{ .declined = pass.declined.load(.monotonic), .no_space = pass.no_space.load(.monotonic) };
}

const SyncfsPass = struct {
    residents: []const Census,
    seg: *Segment,
    hooks: PromotionHooks,
    next: std.atomic.Value(usize) = .init(0),
    declined: std.atomic.Value(u64) = .init(0),
    no_space: std.atomic.Value(u64) = .init(0),

    fn work(self: *SyncfsPass) void {
        while (true) {
            const i = self.next.fetchAdd(1, .monotonic);
            if (i >= self.residents.len) return;
            const key = self.residents[i].key;
            switch (if (self.seg.residentGeneration(key) == null) FsyncPromote.absent else promoteOne(self.seg, self.hooks, key)) {
                .kept => _ = self.declined.fetchAdd(1, .monotonic),
                .kept_no_space => _ = self.no_space.fetchAdd(1, .monotonic),
                .promoted, .absent => {},
            }
        }
    }
};

// ---------------------------------------------------------------------------
// §3.7 The reader's view of an unpromoted entry.
// ---------------------------------------------------------------------------

/// The zeros a lost staged payload reads as: size-consistent, never past
/// the file.
pub fn lostStagedRangeLen(offset: u64, size: u64, meta_size: u64) u64 {
    return @min(offset +| size, meta_size) -| offset;
}

// ---------------------------------------------------------------------------
// §3.1 Identity: the binary marker.
// ---------------------------------------------------------------------------

pub const MARKER_MAGIC = "SQZM".*;
/// v4: the v3 marker naming its OWNER — the member's appender id and the
/// incarnation's pid beside the `(node, mount slot)` pair (0003 §T.1) —
/// plus the MEMBER identity the mount presents on the fabric (0003 §BG: the
/// PR registrant; 0 on a local device); an older marker is Corrupt to this
/// binary and its ring is wiped loud.
pub const MARKER_VERSION: u16 = 4;
/// The marker names the previous set epoch too (an `add-meta` rebind in
/// flight).
pub const MARKER_FLAG_DUAL: u16 = 1 << 0;
const MARKER_FLAGS_KNOWN: u16 = MARKER_FLAG_DUAL;

/// 88 B, checksummed; compared as BYTES.
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
    member_le: [8]u8,
    csum_le: [8]u8,
};

pub const MARKER_LEN: usize = @sizeOf(StagingMarkerWire);
const MARKER_CHECKSUM_OFF: usize = @offsetOf(StagingMarkerWire, "csum_le");

comptime {
    assert(MARKER_LEN == 88);
    assert(MARKER_CHECKSUM_OFF == 80);
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

/// The MEMBER identity a mount presents (0003 §BG): `hash64(hostnqn ‖ 0 ‖
/// hostid)` of its fabric host identity — the PR registrant the device
/// fences between, stable across the member's incarnations (an explicit
/// pair, `/etc/nvme`'s, or the machine-id derivation salted by the mount
/// point) and different for two identities on one host. WHY a separate
/// member beside the mount slot: a `--single-writer` D0 winner and the
/// manager both present slot 0, and a second identity on the same host
/// that preempts a stopped holder's key past T_owner is ANOTHER member —
/// its ring is its own; the fenced holder's residents stay the holder's
/// (the flush-window exposure, loud). A local device (no fabric record)
/// has one member per host: 0.
pub fn memberIdentity(hostnqn: []const u8, hostid: []const u8) u64 {
    var h: checksum.Hasher = .init();
    h.update(hostnqn);
    h.update(&[_]u8{0});
    h.update(hostid);
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
    /// The member identity (`memberIdentity`; 0 on a local device): with
    /// the mount slot, the member scope — a different member under the
    /// same slot is a peer, never a successor.
    member: u64 = 0,
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
            .member_le = undefined,
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
        std.mem.writeInt(u64, &w.member_le, self.member, .little);
        var bytes = std.mem.toBytes(w);
        checksum.stamp(&bytes, MARKER_CHECKSUM_OFF);
        return bytes;
    }

    /// Total: exactly 88 B, magic, version, known flags, the checksum, and
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
            .member = std.mem.readInt(u64, &w.member_le, .little),
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
/// (0003 §T.1, §BG): the member is the mount slot AND the member identity
/// the mount presents; a different node token under the same member is
/// that member's next incarnation — a reboot's successor — never a peer.
pub fn bindVerdict(found: ?Marker, ours: Marker) BindVerdict {
    const f = found orelse return .absent;
    if (f.identity() != ours.identity()) return .dead;
    const same_epoch = f.set_epoch == ours.set_epoch or (f.prev_set_epoch != null and f.prev_set_epoch.? == ours.set_epoch);
    if (!same_epoch and f.set_epoch > ours.set_epoch) return .dead;
    if (f.mount_slot != ours.mount_slot or f.member != ours.member) return .foreign_owner;
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
    /// `lseek(2)`'s hole-reporting whence (std's `SEEK` carries only
    /// SET/CUR/END).
    const SEEK_DATA: u32 = 3;

    /// The memfd's mapping: the punch and the hole scan over it.
    pub const MappedSegment = struct {
        fd: linux.fd_t,
        map: []align(std.heap.page_size_min) u8,

        /// Punch the hole (the kernel frees the pages) and drop the
        /// mapping's PTEs; neither is correctness — a tombstoned record is
        /// never adopted — so their errnos are ignored.
        fn reclaim(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            const t0 = clock.monotonicNs();
            _ = linux.fallocate(self.fd, linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE, @intCast(off), @intCast(len));
            _ = linux.madvise(self.map.ptr + @as(usize, @intCast(off)), @intCast(len), linux.MADV.DONTNEED);
            stats.backing_reclaim_ns.record(clock.monotonicNs() - t0);
        }

        /// `lseek(SEEK_DATA)`: the next data offset, null at or past the
        /// end (`ENXIO`).
        fn nextData(ctx: ?*anyopaque, off: u64) ?u64 {
            const self: *MappedSegment = @ptrCast(@alignCast(ctx.?));
            const rc = linux.lseek(self.fd, @intCast(off), SEEK_DATA);
            if (linux.errno(rc) != .SUCCESS) return null;
            const data: u64 = @intCast(rc);
            return if (data >= self.map.len) null else data;
        }

        pub fn close(self: *MappedSegment) void {
            _ = linux.munmap(self.map.ptr, self.map.len);
            self.* = undefined;
        }
    };

    pub const BindError = error{ OpenFailed, MapFailed, GrowFailed, OwnerLive, ForeignOwner };

    /// What a refused bind found (the log names the holder).
    pub const BindRefusal = struct { owner: ?Marker = null };

    pub const Ram = struct {
        memfd: linux.fd_t,
        /// The bound claim socket (the live claim; handed to the supervisor).
        claim: linux.fd_t,
        mapped: MappedSegment,
        verdict: BindVerdict,
        found: ?Marker,

        /// The marker's grain; the image follows.
        pub const HEADER: u64 = RECORD_ALIGN;
        pub const MEMFD_NAME = "sqz-write-cache";
        pub const CLAIM_PREFIX = "squeezefs-staging-";
        /// The successor's wait for the supervisor's handover frame, and the
        /// bound retries while the supervisor's socket closes behind it.
        const HANDOVER_WAIT_MS: i32 = 5_000;
        const CLAIM_RETRIES: u32 = 200;
        const CLAIM_RETRY_NS: u64 = 10 * std.time.ns_per_ms;

        /// The claim's name: the ring's identity (the set), the mount slot
        /// and the member identity — the member scope (0003 §T.1, §BG).
        pub fn claimName(buf: *[64]u8, identity: u64, slot: u32, member: u64) []const u8 {
            return std.fmt.bufPrint(buf, CLAIM_PREFIX ++ "{x:0>16}-{x:0>8}-{x:0>16}", .{ identity, slot, member }) catch unreachable;
        }

        fn claimAddr(ours: Marker, addr: *linux.sockaddr.un) linux.socklen_t {
            var nbuf: [64]u8 = undefined;
            const name = claimName(&nbuf, ours.identity(), ours.mount_slot, ours.member);
            addr.* = .{ .path = @splat(0) };
            @memcpy(addr.path[1 .. 1 + name.len], name);
            return @intCast(@sizeOf(linux.sa_family_t) + 1 + name.len);
        }

        fn seqpacket() ?linux.fd_t {
            const rc = linux.socket(linux.AF.UNIX, linux.SOCK.SEQPACKET | linux.SOCK.CLOEXEC, 0);
            if (linux.errno(rc) != .SUCCESS) return null;
            return @intCast(rc);
        }

        /// `bind` the claim; null = held by another (a live daemon or a
        /// supervisor after a death).
        fn tryClaim(ours: Marker) BindError!?linux.fd_t {
            const fd = seqpacket() orelse return error.OpenFailed;
            var addr: linux.sockaddr.un = undefined;
            const len = claimAddr(ours, &addr);
            switch (linux.errno(linux.bind(fd, @ptrCast(&addr), len))) {
                .SUCCESS => return fd,
                .ADDRINUSE => {
                    _ = linux.close(fd);
                    return null;
                },
                else => {
                    _ = linux.close(fd);
                    return error.OpenFailed;
                },
            }
        }

        /// The handover: connect to the claim's holder; a holder that does
        /// not listen is a LIVE daemon (`OwnerLive` — after the retries: a
        /// supervisor between its daemon's death and its `listen` is caught
        /// by them); a listening one is the supervisor of a dead one, which
        /// answers one frame with the memfd.
        fn receiveFromSupervisor(ours: Marker) BindError!linux.fd_t {
            const rl = @import("../ipc/rendezvous_linux.zig");
            const c = seqpacket() orelse return error.OpenFailed;
            defer _ = linux.close(c);
            var addr: linux.sockaddr.un = undefined;
            const len = claimAddr(ours, &addr);
            var tries: u32 = 0;
            while (true) : (tries += 1) {
                switch (linux.errno(linux.connect(c, @ptrCast(&addr), len))) {
                    .SUCCESS => break,
                    .CONNREFUSED, .AGAIN => {
                        if (tries == CLAIM_RETRIES) return error.OwnerLive;
                        const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(CLAIM_RETRY_NS) };
                        _ = linux.nanosleep(&ts, null);
                    },
                    else => return error.OpenFailed,
                }
            }
            var pfd = [_]linux.pollfd{.{ .fd = c, .events = linux.POLL.IN, .revents = 0 }};
            const prc = linux.poll(&pfd, 1, HANDOVER_WAIT_MS);
            if (linux.errno(prc) != .SUCCESS or prc == 0) return error.OwnerLive;
            var buf: [16]u8 = undefined;
            const got = rl.recvCtl(c, &buf) catch return error.OwnerLive;
            return got.fd orelse error.OwnerLive;
        }

        /// Bind THIS member's write cache: the claim taken (fresh: a new
        /// memfd of `capacity`, the marker stamped); the claim held by a
        /// supervisor: its memfd adopted — the marker read off grain 0 and
        /// judged (`bindVerdict`: a foreign member's ring is refused, never
        /// adopted; a dead one wiped), the recovered size kept whatever
        /// `capacity` says now (the pool and the tables follow the image);
        /// held by a live daemon: refused `OwnerLive`.
        pub fn bind(ours: Marker, want_capacity: u64, refusal: *BindRefusal) BindError!Ram {
            refusal.* = .{};
            assert(derive.isAligned(want_capacity, RECORD_ALIGN) and want_capacity >= RECORD_ALIGN);
            if (try tryClaim(ours)) |fresh_claim| {
                errdefer _ = linux.close(fresh_claim);
                const memfd = try createMemfd(HEADER + want_capacity);
                errdefer _ = linux.close(memfd);
                var mapped = try mapMemfd(memfd);
                errdefer mapped.close();
                stampMarker(mapped.map, ours);
                return .{ .memfd = memfd, .claim = fresh_claim, .mapped = mapped, .verdict = .absent, .found = null };
            }
            const memfd = receiveFromSupervisor(ours) catch |e| {
                if (e == error.OwnerLive) stats.staging_owner_live_refusals.inc();
                return e;
            };
            errdefer _ = linux.close(memfd);
            var mapped = try mapMemfd(memfd);
            errdefer mapped.close();
            const found: ?Marker = Marker.decode(mapped.map[0..MARKER_LEN]) catch null;
            const verdict: BindVerdict = if (found == null) .dead else bindVerdict(found, ours);
            switch (verdict) {
                .absent, .match => {},
                .upgrade => stampMarker(mapped.map, ours),
                .dead => {
                    wipeRam(&mapped);
                    stampMarker(mapped.map, ours);
                    stats.staging_generation_discards.inc();
                },
                .foreign_owner => {
                    refusal.owner = found;
                    stats.staging_foreign_scope_refusals.inc();
                    return error.ForeignOwner;
                },
            }
            // Our own claim: the supervisor closes its socket right after
            // the handover; the name frees with it.
            var tries: u32 = 0;
            const own_claim: linux.fd_t = while (true) : (tries += 1) {
                if (try tryClaim(ours)) |fd| break fd;
                if (tries == CLAIM_RETRIES) {
                    refusal.owner = found;
                    stats.staging_owner_live_refusals.inc();
                    return error.OwnerLive;
                }
                const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(CLAIM_RETRY_NS) };
                _ = linux.nanosleep(&ts, null);
            };
            return .{ .memfd = memfd, .claim = own_claim, .mapped = mapped, .verdict = verdict, .found = found };
        }

        /// Whether another holds this member's claim (a supervisor of a
        /// dead predecessor, or a live mount): a free name is bound and
        /// dropped at once; a held one is retried for the handover's bound
        /// (a supervisor discarding an empty ring frees it within it). Never
        /// connects: a connect takes the ring from its holder.
        pub fn claimHeld(ours: Marker) BindError!bool {
            var tries: u32 = 0;
            while (true) : (tries += 1) {
                if (try tryClaim(ours)) |fd| {
                    _ = linux.close(fd);
                    return false;
                }
                if (tries == CLAIM_RETRIES) return true;
                const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(CLAIM_RETRY_NS) };
                _ = linux.nanosleep(&ts, null);
            }
        }

        /// The image: the memfd past the marker's grain.
        pub fn image(self: *Ram) []align(std.heap.page_size_min) u8 {
            return @alignCast(self.mapped.map[HEADER..]);
        }

        pub fn capacity(self: *const Ram) u64 {
            return self.mapped.map.len - HEADER;
        }

        /// The segment's backing over the image (offsets are the image's:
        /// the seams add the header).
        pub fn backing(self: *Ram) Backing {
            return .{ .ctx = self, .reclaim = reclaimRam, .nextData = nextDataRam };
        }

        fn reclaimRam(ctx: ?*anyopaque, off: u64, len: u64) void {
            const self: *Ram = @ptrCast(@alignCast(ctx.?));
            MappedSegment.reclaim(&self.mapped, HEADER + off, len);
        }

        fn nextDataRam(ctx: ?*anyopaque, off: u64) ?u64 {
            const self: *Ram = @ptrCast(@alignCast(ctx.?));
            const d = MappedSegment.nextData(&self.mapped, HEADER + off) orelse return null;
            return d - HEADER;
        }

        /// Any byte of data past the marker (`SEEK_DATA`): what the
        /// supervisor asks before it holds a dead daemon's ring for a
        /// successor — a drained ring is discarded.
        pub fn holdsData(memfd: linux.fd_t) bool {
            const rc = linux.lseek(memfd, @intCast(HEADER), SEEK_DATA);
            return linux.errno(rc) == .SUCCESS;
        }

        pub fn release(self: *Ram) void {
            self.mapped.close();
            _ = linux.close(self.memfd);
            _ = linux.close(self.claim);
            self.* = undefined;
        }

        fn createMemfd(len: u64) BindError!linux.fd_t {
            const rc = linux.memfd_create(MEMFD_NAME, linux.MFD.CLOEXEC | linux.MFD.ALLOW_SEALING);
            if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
            const fd: linux.fd_t = @intCast(rc);
            errdefer _ = linux.close(fd);
            if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.GrowFailed;
            // Size-sealed: a holder maps a length that cannot change under
            // it; writes stay the daemon's (the punch frees pages, not size).
            const seals: u32 = linux.F.SEAL_SHRINK | linux.F.SEAL_GROW | linux.F.SEAL_SEAL;
            if (linux.errno(linux.fcntl(fd, linux.F.ADD_SEALS, seals)) != .SUCCESS) return error.OpenFailed;
            return fd;
        }

        fn mapMemfd(fd: linux.fd_t) BindError!MappedSegment {
            var st: linux.Statx = undefined;
            if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.OpenFailed;
            const len: usize = @intCast(st.size);
            if (len < HEADER + RECORD_ALIGN or !derive.isAligned(len, RECORD_ALIGN)) return error.OpenFailed;
            const m = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
            if (linux.errno(m) != .SUCCESS) return error.MapFailed;
            const ptr: [*]align(std.heap.page_size_min) u8 = @ptrFromInt(m);
            return .{ .fd = fd, .map = ptr[0..len] };
        }

        fn stampMarker(map: []align(std.heap.page_size_min) u8, ours: Marker) void {
            const bytes = ours.encode();
            @memcpy(map[0..MARKER_LEN], &bytes);
            @memset(map[MARKER_LEN..HEADER], 0);
        }

        /// `dead`: every page past the marker punched.
        fn wipeRam(mapped: *MappedSegment) void {
            MappedSegment.reclaim(mapped, HEADER, mapped.map.len - HEADER);
        }
    };
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
    var pool = Pool.init(gpa, .none);
    defer pool.deinit();
    const s1 = try stageWrite(&pool, &third, null, k, &v, 0, .append);
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
    _ = try stageWrite(&pool, &third, null, k, &v, 0, .none);
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

/// The oracle of the hole search: a clear run of `grains` grains anywhere
/// in the live set, read bit by bit (no words, no cursor).
fn oracleHasHole(seg: *const Segment, grains: usize) bool {
    const total: usize = @intCast(seg.image.len / RECORD_ALIGN);
    var run: usize = 0;
    var g: usize = 0;
    while (g < total) : (g += 1) {
        if (seg.grainOccupied(g)) run = 0 else run += 1;
        if (run >= grains) return true;
    }
    return false;
}

test "placement completeness: a hole exists ⇒ the first-fit search finds it — 4,000 random puts (1–12 grains of footprint, with and without the append slack), replaces, removes, held leases (retired copies stay live), sweeps and random cursor positions; each RingFull is checked against a bit-by-bit oracle, each placement is inside the image and overlaps nothing; a hole below a cursor in the partial tail word is found by the wrapped leg (the ring-full line of generic/475 named 2,827–5,049 live entries in a 7.4 GB segment: the records were there, the search was not the defect)" {
    const gpa = testing.allocator;
    const cap_grains: usize = 300; // not a multiple of the 64-grain word: the tail word is partial
    var r = try ramSegment(gpa, cap_grains * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    var prng = std.Random.DefaultPrng.init(0x5eed_4a11);
    const rnd = prng.random();
    const n_keys: usize = 64;
    var resident: [n_keys]bool = @splat(false);
    var leases: [n_keys]?Segment.Lease = @splat(null);
    defer for (&leases) |*l| if (l.*) |x| x.release();
    const buf = try gpa.alloc(u8, 12 * 4096);
    defer gpa.free(buf);
    var refusals: u32 = 0;
    var placements: u32 = 0;
    var generation: u64 = 1;
    for (0..4000) |_| {
        const i = rnd.uintLessThan(usize, n_keys);
        const k = tkey(@intCast(i), i);
        switch (rnd.uintLessThan(u8, 10)) {
            0...5 => {
                // A put: 1–12 grains of footprint, with or without slack.
                const val_len = rnd.intRangeAtMost(usize, 1, 11 * 4096);
                const slack: Slack = if (rnd.boolean()) .append else .none;
                const need = reserveFor(KEY_LEN, val_len, slack);
                const grains: usize = @intCast(need / RECORD_ALIGN);
                @memset(buf[0..val_len], @intCast(i));
                r.seg.put(k, buf[0..val_len], generation, 0, need) catch |e| {
                    try testing.expectEqual(error.RingFull, e);
                    // The law: no hole of that size exists anywhere.
                    try testing.expect(!oracleHasHole(&r.seg, grains));
                    refusals += 1;
                    continue;
                };
                generation += 1;
                placements += 1;
                resident[i] = true;
                const l = r.seg.get(k).?;
                defer l.release();
                try testing.expect(l.cell.extent.end() <= r.seg.capacity());
                try testing.expectEqual(need, l.cell.extent.reserved);
            },
            6...7 => {
                if (resident[i]) {
                    try testing.expect(r.seg.remove(k, null));
                    resident[i] = false;
                }
            },
            8 => {
                // Hold a lease (a retired copy under it stays live for
                // placement) or drop one.
                if (leases[i]) |l| {
                    l.release();
                    leases[i] = null;
                } else if (resident[i]) leases[i] = r.seg.get(k);
            },
            else => {
                r.seg.sweepDirty();
                // The cursor at a random grain: every position is a legal
                // start for the two-leg scan.
                r.seg.lock.lock();
                r.seg.write_offset = rnd.uintLessThan(u64, cap_grains) * RECORD_ALIGN;
                r.seg.lock.unlock();
            },
        }
        // The live extents never overlap (the leased retired copies are
        // not in the index; `markLocked` asserts their grains apart).
        var ext: [n_keys]Extent = undefined;
        const live = r.seg.liveExtents(&ext);
        std.mem.sort(Extent, live, {}, Segment.extentLess);
        for (live[1..], 0..) |e, j| try testing.expect(live[j].end() <= e.rec_off);
    }
    // The walk exercised both verdicts.
    try testing.expect(refusals > 50);
    try testing.expect(placements > 1000);
    // A wrapped hole: records fill the ring, the cursor sits past the
    // LAST free run, and a record of exactly that run's size lands there.
    for (&leases) |*l| if (l.*) |x| {
        x.release();
        l.* = null;
    };
    for (0..n_keys) |i| if (resident[i]) {
        _ = r.seg.remove(tkey(@intCast(i), i), null);
        resident[i] = false;
    };
    r.seg.sweepDirty();
    try testing.expectEqual(@as(usize, 0), r.seg.live());
    for (r.seg.occupied) |w| try testing.expectEqual(@as(u64, 0), w); // every grain back
    // The fill from grain 0 (a cursor the random walk left at an odd grain
    // would leave two one-grain remainders: first fit, not a defect).
    r.seg.lock.lock();
    r.seg.write_offset = 0;
    r.seg.lock.unlock();
    const two: [10]u8 = @splat(9);
    var placed: usize = 0;
    while (true) : (placed += 1) r.seg.put(tkey(1, 1000 + placed), &two, 1, 0, 0) catch break;
    try testing.expectEqual(cap_grains / 2, placed);
    try testing.expect(r.seg.remove(tkey(1, 1000 + 3), null)); // the hole at grains [6, 8)
    r.seg.lock.lock();
    r.seg.write_offset = (cap_grains - 1) * RECORD_ALIGN; // the cursor in the partial tail word, past the hole
    r.seg.lock.unlock();
    try r.seg.put(tkey(2, 2000), &two, 1, 0, 0);
    const l2 = r.seg.get(tkey(2, 2000)).?;
    defer l2.release();
    try testing.expectEqual(@as(u64, 6 * RECORD_ALIGN), l2.cell.extent.rec_off);
    try testing.expectError(error.RingFull, r.seg.put(tkey(3, 3000), &two, 1, 0, 0));
    try testing.expect(!oracleHasHole(&r.seg, 2));
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
    promoted: u32 = 0,
    fail_next: bool = false,
    no_space_next: bool = false,
    /// The dismount judgement of these inos (by the key's ino): a corpse's
    /// record, an unnamed one; every other key promotes (or fails as the
    /// promote hook does). The judge removes what it destroys, as the
    /// plane's does.
    seg: ?*Segment = null,
    corpse_ino: ?u64 = null,
    unnamed_ino: ?u64 = null,

    fn hooks(self: *Kicks) PromotionHooks {
        return .{ .ctx = self, .promote = onPromote, .dismount = onDismount };
    }

    fn onPromote(ctx: ?*anyopaque, k: StagingKey) LiveVerdict {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        if (self.fail_next) {
            self.fail_next = false;
            return .declined;
        }
        if (self.no_space_next) {
            self.no_space_next = false;
            return .no_space;
        }
        if (self.unnamed_ino == k.ino()) return .unnamed;
        self.promoted += 1;
        if (self.seg) |s| _ = s.remove(k, null);
        return .promoted;
    }

    fn onDismount(ctx: ?*anyopaque, k: StagingKey) DismountVerdict {
        const self: *Kicks = @ptrCast(@alignCast(ctx.?));
        if (self.corpse_ino == k.ino() or self.unnamed_ino == k.ino()) {
            if (self.seg) |s| _ = s.remove(k, null);
            return if (self.corpse_ino == k.ino()) .corpse else .unnamed;
        }
        return if (onPromote(ctx, k) == .promoted) .promoted else .kept;
    }
};

/// A thread-safe promote double for the bulk passes' width: every key's
/// promotions counted per ino, the promotions in flight at once observed.
const PassKicks = struct {
    seg: *Segment,
    per_ino: [64]std.atomic.Value(u32) = @splat(.init(0)),
    in_flight: std.atomic.Value(u32) = .init(0),
    max_in_flight: std.atomic.Value(u32) = .init(0),
    width: usize,

    fn hooks(self: *PassKicks) PromotionHooks {
        return .{ .ctx = self, .promote = onPromote, .dismount = onDismount, .width = self.width };
    }

    fn onPromote(ctx: ?*anyopaque, k: StagingKey) LiveVerdict {
        const self: *PassKicks = @ptrCast(@alignCast(ctx.?));
        const now = self.in_flight.fetchAdd(1, .acq_rel) + 1;
        _ = self.max_in_flight.fetchMax(now, .acq_rel);
        _ = self.per_ino[@intCast(k.ino())].fetchAdd(1, .acq_rel);
        _ = self.seg.remove(k, null);
        _ = self.in_flight.fetchSub(1, .acq_rel);
        return .promoted;
    }

    fn onDismount(ctx: ?*anyopaque, k: StagingKey) DismountVerdict {
        return if (onPromote(ctx, k) == .promoted) .promoted else .kept;
    }
};

test "the bulk passes run `hooks.width` promotions at once off one cursor (decision 0003 §CD): syncfs and the dismount pass each promote every resident EXACTLY once at width 8, the tallies whole, never more in flight than the width, the ring empty after; width 1 is the sequential pass" {
    const gpa = testing.allocator;
    for ([_]usize{ 1, 8 }) |width| {
        var r = try ramSegment(gpa, 256 * 4096);
        defer gpa.free(r.image);
        defer r.seg.deinit();
        var kicks: PassKicks = .{ .seg = &r.seg, .width = width };
        const v: [100]u8 = @splat(7);
        for (0..48) |i| try r.seg.put(tkey(@intCast(i), i), &v, i + 1, 0, 0);
        const c0 = stats.promotion_pass_concurrent.load();
        try testing.expectEqual(SyncfsKept{}, try promoteAllAtSyncfs(gpa, testing.io, &r.seg, kicks.hooks()));
        for (kicks.per_ino[0..48]) |*n| try testing.expectEqual(@as(u32, 1), n.load(.acquire));
        try testing.expectEqual(@as(usize, 0), r.seg.live());
        for (0..48) |i| try r.seg.put(tkey(@intCast(i), i), &v, 100 + i, 0, 0);
        const tally = try promoteAllAtDismount(gpa, testing.io, &r.seg, kicks.hooks());
        try testing.expectEqual(DismountTally{ .promoted = 48, .promoted_bytes = 48 * 8192 }, tally);
        for (kicks.per_ino[0..48]) |*n| try testing.expectEqual(@as(u32, 2), n.load(.acquire));
        try testing.expectEqual(@as(usize, 0), r.seg.live());
        try testing.expect(kicks.max_in_flight.load(.acquire) <= width);
        try testing.expectEqual(c0 + @as(u64, if (width > 1) 2 else 0), stats.promotion_pass_concurrent.load());
    }
}

test "THE RESIDENTS' CLAIM ON THE STORE (StoreClaim): inline claims nothing, a packable image its 4 KiB slot, one past the pack's max slot a whole block, packing off a block for anything above inline, no store nothing; the pool sums it — a stage charges it, a re-stage replaces it, an in-place append re-reads it off the grown image (across the pack bound: slot → block), an overwrite keeps it, a release credits it — and recovery seeds it off the footprint" {
    const gpa = testing.allocator;
    const rule: StoreClaim = .{ .inline_max = 4096, .max_slot = 512 << 10, .block = 1 << 20 };
    try testing.expectEqual(@as(u64, 0), rule.of(0));
    try testing.expectEqual(@as(u64, 0), rule.of(4096));
    try testing.expectEqual(@as(u64, 8192), rule.of(4097));
    try testing.expectEqual(@as(u64, 100 << 10), rule.of(100 << 10));
    try testing.expectEqual(@as(u64, 512 << 10), rule.of(512 << 10));
    try testing.expectEqual(@as(u64, 1 << 20), rule.of((512 << 10) + 1));
    try testing.expectEqual(@as(u64, 1 << 20), rule.of(1 << 20));
    try testing.expectEqual(@as(u64, 1 << 20), (StoreClaim{ .inline_max = 4096, .max_slot = 0, .block = 1 << 20 }).of(4097));
    try testing.expectEqual(@as(u64, 0), StoreClaim.none.of(1 << 20));
    var kicks: Kicks = .{};
    var pool = Pool.init(gpa, kicks.hooks());
    defer pool.deinit();
    pool.claim_rule = rule;
    var r = try ramSegment(gpa, 1024 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const k = tkey(1, 1);
    const small: [100]u8 = @splat(1);
    _ = try stageWrite(&pool, &r.seg, null, k, &small, 0, .none);
    try testing.expectEqual(@as(u64, 0), pool.claimedBytes()); // inline
    const mid = try gpa.alloc(u8, 500 << 10);
    defer gpa.free(mid);
    @memset(mid, 2);
    _ = try stageWrite(&pool, &r.seg, null, k, mid, 0, .append);
    try testing.expectEqual(@as(u64, 500 << 10), pool.claimedBytes()); // a packed slot
    try testing.expectEqual(@as(?u64, 500 << 10), pool.claimOf(k.file_id));
    // The in-place append carries the image past the pack bound: a block.
    const tail = try gpa.alloc(u8, 20 << 10);
    defer gpa.free(tail);
    @memset(tail, 3);
    try testing.expect((try appendStaged(&pool, &r.seg, k, 500 << 10, tail, 0)) != null);
    try testing.expectEqual(@as(u64, 1 << 20), pool.claimedBytes());
    // An overwrite inside the image keeps it.
    try testing.expect((try overwriteStaged(&pool, &r.seg, testing.io, k, 0, &small, 0)) != null);
    try testing.expectEqual(@as(u64, 1 << 20), pool.claimedBytes());
    // A second resident adds; the release credits each.
    const k2 = tkey(2, 2);
    _ = try stageWrite(&pool, &r.seg, null, k2, mid[0 .. 10 << 10], 0, .none);
    try testing.expectEqual(@as(u64, (1 << 20) + (12 << 10)), pool.claimedBytes()); // the slot is grain-aligned
    try testing.expect(releaseStaged(&pool, &r.seg, null, k2, pool.generationOf(k2.file_id).?));
    try testing.expectEqual(@as(u64, 1 << 20), pool.claimedBytes());
    try testing.expect(releaseStaged(&pool, &r.seg, null, k, pool.generationOf(k.file_id).?));
    try testing.expectEqual(@as(u64, 0), pool.claimedBytes());
    // Recovery seeds no claim (a recovered record's standing is unjudged);
    // the re-stage at a live generation charges it.
    _ = try stageWrite(&pool, &r.seg, null, k2, mid[0 .. 10 << 10], 0, .none);
    var pool2 = Pool.init(gpa, kicks.hooks());
    defer pool2.deinit();
    pool2.claim_rule = rule;
    try pool2.seedFromRecovery(&r.seg);
    try testing.expectEqual(@as(u64, 0), pool2.claimedBytes());
    try testing.expect(pool2.currentBytes() > 0); // the footprint is charged
    _ = try stageWrite(&pool2, &r.seg, null, k2, mid[0 .. 10 << 10], 0, .none);
    try testing.expectEqual(@as(u64, 12 << 10), pool2.claimedBytes());
    try testing.expect(releaseStaged(&pool2, &r.seg, null, k2, pool2.generationOf(k2.file_id).?));
    try testing.expectEqual(@as(u64, 0), pool2.claimedBytes());
}

test "pool: the ledger charges the delta on a re-stage and credits on release so the gauge never ratchets; generations start at 1 and recovery seeds 0; a stale release is a no-op" {
    const gpa = testing.allocator;
    var kicks: Kicks = .{};
    var pool = Pool.init(gpa, kicks.hooks());
    defer pool.deinit();
    try testing.expectEqual(FIRST_GENERATION, pool.next_generation.load(.monotonic));
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const k = tkey(1, 1);
    const g0 = stats.current_staged_write_bytes.load();
    const v: [100]u8 = @splat(1);
    const s1 = try stageWrite(&pool, &r.seg, null, k, &v, 0, .none);
    try testing.expectEqual(@as(u64, 1), s1.generation);
    try testing.expectEqual(@as(u64, 8192), pool.currentBytes());
    try testing.expectEqual(g0 + 8192, stats.current_staged_write_bytes.load());
    // Re-stage the same file_id larger: the delta is charged, one row.
    const v2: [5000]u8 = @splat(2);
    const s2 = try stageWrite(&pool, &r.seg, null, k, &v2, 0, .none);
    try testing.expectEqual(@as(u64, 2), s2.generation);
    try testing.expectEqual(@as(u64, 12288), pool.currentBytes());
    try testing.expectEqual(@as(?u64, 2), pool.generationOf(k.file_id));
    const big: [9 * 4096 - 10]u8 = @splat(3);
    _ = try stageWrite(&pool, &r.seg, null, tkey(2, 2), &big, 0, .none);
    const cur = pool.currentBytes();
    // A stale generation release is a no-op; the live one credits and removes.
    try testing.expect(!releaseStaged(&pool, &r.seg, null, k, 1));
    try testing.expectEqual(cur, pool.currentBytes());
    try testing.expect(releaseStaged(&pool, &r.seg, null, k, 2));
    try testing.expectEqual(cur - 12288, pool.currentBytes());
    try testing.expect(r.seg.get(k) == null);
    try testing.expectEqual(@as(?u64, null), pool.generationOf(k.file_id));
    // Recovery seeds the ledger at generation 0 with the exact footprints.
    var pool2 = Pool.init(gpa, .none);
    defer pool2.deinit();
    try pool2.seedFromRecovery(&r.seg);
    try testing.expectEqual(@as(?u64, RECOVERED_GENERATION), pool2.generationOf(tkey(2, 2).file_id));
    try testing.expectEqual(@as(u64, 10 * 4096), pool2.currentBytes());
}

test "the ring-full EPISODE: a full ring refusing large records while small ones land between them is ONE episode (one warning line, every refusal counted) — 60 refusals interleaved with 60 landings open exactly one; a release that leaves the footprint above the drain mark keeps it open; the release that brings the footprint a quarter of the ring below the open closes it (the info line's tally); the next refusal opens a second (generic/475: 5,790 lines in 161 s from an episode ended by any landing)" {
    const gpa = testing.allocator;
    var pool = Pool.init(gpa, .none);
    defer pool.deinit();
    const cap_grains = 16;
    var r = try ramSegment(gpa, cap_grains * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    const small: [10]u8 = @splat(1); // 2 grains
    const large: [3 * 4096]u8 = @splat(2); // 4 grains
    var held: [cap_grains / 2]StagingKey = undefined;
    for (&held, 0..) |*k, i| {
        k.* = tkey(@intCast(i), i);
        _ = try stageWrite(&pool, &r.seg, null, k.*, &small, 0, .none);
    }
    try testing.expectEqual(@as(u64, cap_grains * 4096), pool.currentBytes());
    const episodes0 = stats.staging_ring_full_episodes.load();
    const refusals0 = stats.staging_ring_full_refusals.load();
    // The thrash: a 4-grain record never fits, a 2-grain one lands in the
    // hole a release opens, 60 times over.
    var next: usize = 100;
    for (0..60) |i| {
        try testing.expectError(error.StorageFull, stageWrite(&pool, &r.seg, null, tkey(9, 9), &large, 0, .none));
        try testing.expect(pool.ring_full.open);
        const slot = &held[i % held.len];
        try testing.expect(releaseStaged(&pool, &r.seg, null, slot.*, pool.generationOf(slot.file_id).?));
        try testing.expect(pool.ring_full.open); // one record freed is not a quarter of the ring
        slot.* = tkey(@intCast(next), next);
        _ = try stageWrite(&pool, &r.seg, null, slot.*, &small, 0, .none);
        next += 1;
    }
    try testing.expectEqual(episodes0 + 1, stats.staging_ring_full_episodes.load());
    try testing.expectEqual(refusals0 + 60, stats.staging_ring_full_refusals.load());
    try testing.expectEqual(@as(u64, 60), pool.ring_full.refusals);
    try testing.expectEqual(@as(u64, cap_grains * 4096), pool.ring_full.charged_at_open);
    try testing.expectEqual(@as(u64, cap_grains * 4096 - cap_grains * 4096 / WRITE_CACHE_DRAIN_DIVISOR), pool.ring_full.drain_below);
    // The drain: the footprint must fall a quarter of the ring (4 grains =
    // two small records) below the open before the episode closes.
    const residents = try r.seg.residentsAlloc(gpa);
    defer gpa.free(residents);
    try testing.expect(residents.len >= 3);
    try testing.expect(releaseStaged(&pool, &r.seg, null, residents[0].key, residents[0].generation));
    try testing.expect(pool.ring_full.open);
    try testing.expect(releaseStaged(&pool, &r.seg, null, residents[1].key, residents[1].generation));
    try testing.expect(!pool.ring_full.open);
    try testing.expectEqual(@as(u64, 0), pool.ring_full.refusals);
    // Below the mark a refusal is a NEW episode.
    _ = try stageWrite(&pool, &r.seg, null, tkey(50, 50), &small, 0, .none);
    _ = try stageWrite(&pool, &r.seg, null, tkey(51, 51), &small, 0, .none);
    try testing.expectError(error.StorageFull, stageWrite(&pool, &r.seg, null, tkey(9, 9), &large, 0, .none));
    try testing.expectEqual(episodes0 + 2, stats.staging_ring_full_episodes.load());
    try testing.expectEqual(refusals0 + 61, stats.staging_ring_full_refusals.load());
}

test "the write cache over the ONE cache core (owner ruling 2026-10-21): a stage inserts the resident at the head of its shard; a touch (a read, an in-place write) moves it to the head; the drain's eviction promotes the LEAST RECENTLY USED resident first through the sink and releases it iff it left the ring (a declined promotion keeps it and backs the pass off); an insert at the bound never promotes under the writer's lock — it declines, the stage answers StorageFull (the own-block arm) — and the drain makes the room; a re-stage supersedes its entry without a promotion; a release invalidates without one; the stats family names the instance" {
    const gpa = testing.allocator;
    var kicks: Kicks = .{};
    var r = try ramSegment(gpa, 64 * 4096);
    defer gpa.free(r.image);
    defer r.seg.deinit();
    kicks.seg = &r.seg;
    var pool = Pool.init(gpa, kicks.hooks());
    defer pool.deinit();
    var sink: PromoteSink = .{ .pool = &pool, .hooks = kicks.hooks() };
    // One shard: the order is exact; four records of 8 KiB fit the bound.
    const bound: u64 = 4 * 8192;
    const c = try WriteCache.init(gpa, &sink, bound, 1, writeCacheItems(bound, 1), null);
    defer c.deinit();
    const v: [100]u8 = @splat(1);
    for (1..5) |i| _ = try stageWrite(&pool, &r.seg, c, tkey(@intCast(i), i), &v, 0, .none);
    try testing.expectEqual(bound, c.currentBytes());
    try testing.expectEqual(@as(u64, 4), c.stats.fills.load());
    // The fifth at the bound: the inline eviction declines (the writer's
    // lock), the stage refuses; nothing promoted, nothing placed.
    const declined0 = stats.write_cache_inline_evictions_declined.load();
    const refusals0 = stats.staging_over_cap_refusals.load();
    try testing.expectError(error.StorageFull, stageWrite(&pool, &r.seg, c, tkey(5, 5), &v, 0, .none));
    try testing.expectEqual(declined0 + 1, stats.write_cache_inline_evictions_declined.load());
    try testing.expectEqual(refusals0 + 1, stats.staging_over_cap_refusals.load());
    try testing.expectEqual(@as(u32, 0), kicks.promoted);
    try testing.expectEqual(@as(usize, 4), r.seg.live());
    try testing.expect(r.seg.get(tkey(5, 5)) == null);
    // A touch of the oldest (key 1: a read) moves it to the head; the drain
    // to half the bound promotes keys 2 then 3 — LRU first — and releases
    // them; 1 and 4 stay.
    touch(c, tkey(1, 1));
    const p = c.evictUntil(bound / 2);
    try testing.expectEqual(@as(u32, 2), p.released);
    try testing.expectEqual(@as(u32, 0), p.kept);
    try testing.expectEqual(@as(u32, 2), kicks.promoted);
    try testing.expect(r.seg.get(tkey(2, 2)) == null and r.seg.get(tkey(3, 3)) == null);
    try testing.expect(r.seg.get(tkey(1, 1)) != null and r.seg.get(tkey(4, 4)) != null);
    try testing.expectEqual(bound / 2, c.currentBytes());
    // A declined promotion keeps the victim and backs the pass off.
    kicks.fail_next = true;
    const q = c.evictUntil(0);
    try testing.expectEqual(@as(u32, 0), q.released);
    try testing.expectEqual(@as(u32, 1), q.kept);
    try testing.expectEqual(@as(usize, 2), r.seg.live());
    // A re-stage of key 4 (larger) supersedes its entry: no promotion, the
    // new bytes charged, the segment's copy replaced.
    const v2: [5000]u8 = @splat(2);
    const promoted_before = kicks.promoted;
    _ = try stageWrite(&pool, &r.seg, c, tkey(4, 4), &v2, 0, .none);
    try testing.expectEqual(promoted_before, kicks.promoted);
    try testing.expectEqual(@as(u64, 8192 + 12288), c.currentBytes());
    try testing.expectEqual(@as(usize, 2), r.seg.live());
    // A release (the promotion landed elsewhere, an unlink) invalidates
    // the entry without a promotion.
    try testing.expect(releaseStaged(&pool, &r.seg, c, tkey(1, 1), pool.generationOf(tkey(1, 1).file_id).?));
    try testing.expectEqual(promoted_before, kicks.promoted);
    try testing.expectEqual(@as(u64, 12288), c.currentBytes());
    try testing.expect(!c.contains(tkey(1, 1)));
    // The instance's family.
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try c.stats.writeJson(&w, "write_cache");
    // Four left through the sink (two promoted at the bound, two
    // invalidated: the re-stage and the release), two were kept (the
    // declined inline eviction, the declined promotion), one hit (the
    // touch), one insert refused (the bound), five fills.
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_hits\":1,") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_fills\":5,") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_evictions\":4,") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_invalidations\":2,") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_sink_kept\":2,") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_insert_refusals\":1,") != null);
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

test "the segment's per-cell tables are gauged (R5): tablesBound(cap) = maxCells × (Cell + ptr) + the two grain bitmaps + indexSlots × (1 + KEY_LEN + ptr) is the CEILING; a small segment carries exactly it (one chunk holds every cell), a large one grows toward it a chunk of 4,096 cells at a time; puts inside a chunk move nothing, deinit returns it; the component is attribution-only; the ceiling is ≈ 4.6 % of the ring" {
    const gpa = testing.allocator;
    // The tie, restated: n = cap / grain + 1 cells, one bit per grain.
    const cap: u64 = 64 * 4096;
    const n: u64 = cap / RECORD_ALIGN + 1;
    try testing.expectEqual(n, Segment.maxCells(cap));
    try testing.expectEqual(@as(u64, 128), Segment.indexSlots(@intCast(n)));
    try testing.expectEqual(@as(usize, 1), Segment.occupiedWords(cap));
    try testing.expectEqual(n * @sizeOf(Cell) + n * @sizeOf(*Cell) + 2 * 8 + 128 * (1 + KEY_LEN + @sizeOf(*Cell)), Segment.tablesBound(cap));
    try testing.expectEqual(@as(usize, 104), @sizeOf(Cell));
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
    // A ring past one chunk starts with ONE chunk and grows: 8 MiB = 2,049
    // cells' ceiling, the first 4,096-cell chunk holds them all; 64 MiB =
    // 16,385 cells — the open segment carries one chunk (4,096 cells, the
    // free list and the index sized to it) and the bitmaps, under the bound.
    {
        const big: u64 = 64 * derive.MiB;
        const image = try gpa.alloc(u8, big);
        defer gpa.free(image);
        @memset(image[0..4096], 0);
        var bseg = try Segment.init(gpa, image, .none);
        defer bseg.deinit();
        try testing.expectEqual(@as(usize, 1), bseg.cells.items.len);
        try testing.expectEqual(Segment.CELL_CHUNK, bseg.cells.items[0].len);
        try testing.expectEqual(Segment.CELL_CHUNK * (@sizeOf(Cell) + @sizeOf(*Cell)) + 2 * Segment.occupiedWords(big) * 8 + Segment.indexSlots(Segment.CELL_CHUNK) * (1 + KEY_LEN + @sizeOf(*Cell)), bseg.tablesBytes());
        try testing.expect(bseg.tablesBytes() < Segment.tablesBound(big));
    }
    // The ceiling's share of the ring: 24,340,720 B at a 546 MiB ring
    // (139,777 cells × (104 + 8) B + 2 × 2,184 bitmap words × 8 B +
    // 262,144 index slots × 33 B; the cell lost its sync and hold words
    // with the write cache, 120 → 104 B, and the free list holds cell
    // POINTERS since the chunks, 2026-10-21) and 2,920,560 B at 64 MiB —
    // 4.2–4.3 % of either, the index's power-of-two rounding the swing.
    const venue_ring: u64 = 546 * derive.MiB;
    try testing.expectEqual(@as(u64, 24_340_720), Segment.tablesBound(venue_ring));
    try testing.expectEqual(@as(u64, 2_920_560), Segment.tablesBound(64 * derive.MiB));
    try testing.expect(Segment.tablesBound(venue_ring) * 1000 / venue_ring == 42);
    try testing.expect(Segment.tablesBound(64 * derive.MiB) * 1000 / (64 * derive.MiB) == 43);
    // The R5 component: attribution-only (the tables live with the segment), heap, not page cache.
    const c = tablesComponent();
    try testing.expectEqualStrings("staging_tables", c.name);
    try testing.expectEqual(@as(u64, 0), c.weight);
    try testing.expectEqual(@as(u64, 0), c.floor);
    try testing.expect(!c.kernel_reclaimable);
    try testing.expectEqual(stats.staging_tables_bytes.load(), c.current());
}

test "the promotion points: the dismount pass judges every resident — promoted, kept (a resource decline stays resident, counted FAILED), a corpse's record and an unnamed one destroyed (counted apart, never FAILED) — and the ring ends with the kept one alone; fsync promotes only under the lever (default off), a failure never fails it" {
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
    const tally = try promoteAllAtDismount(gpa, testing.io, &r.seg, kicks.hooks());
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
    // The fsync point takes the promote hook alone: no dismount verdicts.
    // A declined promotion is `kept`, a key with no resident is `absent`
    // and never calls the hook.
    kicks.seg = null;
    const p0 = stats.fsync_promoted_files.load();
    kicks.fail_next = true;
    try testing.expectEqual(FsyncPromote.kept, promoteAtFsync(&r.seg, kicks.hooks(), tkey(1, 1)));
    try testing.expectEqual(p0, stats.fsync_promoted_files.load());
    try testing.expectEqual(FsyncPromote.absent, promoteAtFsync(&r.seg, kicks.hooks(), tkey(9, 9)));
    try testing.expectEqual(@as(u32, 2), kicks.promoted);
    // The hook promotes: the record is gone (the hook removed it).
    kicks.seg = &r.seg;
    try testing.expectEqual(FsyncPromote.promoted, promoteAtFsync(&r.seg, kicks.hooks(), tkey(1, 1)));
    try testing.expectEqual(p0 + 1, stats.fsync_promoted_files.load());
    try testing.expectEqual(@as(u32, 3), kicks.promoted);
    try testing.expectEqual(FsyncPromote.absent, promoteAtFsync(&r.seg, kicks.hooks(), tkey(1, 1)));
    // A resident the layout no longer names (`unnamed`): nothing owed —
    // `absent`, counted superseded, the record left for its release.
    const s0 = stats.fsync_promote_superseded.load();
    try r.seg.put(tkey(5, 5), &v, 6, 0, 0);
    try testing.expectEqual(FsyncPromote.absent, promoteAtFsync(&r.seg, kicks.hooks(), tkey(5, 5)));
    try testing.expectEqual(s0 + 1, stats.fsync_promote_superseded.load());
    try testing.expectEqual(p0 + 1, stats.fsync_promoted_files.load());
    try testing.expectEqual(@as(?u64, 6), r.seg.residentGeneration(tkey(5, 5)));
    try testing.expectEqual(SyncfsKept{}, try promoteAllAtSyncfs(gpa, testing.io, &r.seg, kicks.hooks()));
    try testing.expect(r.seg.remove(tkey(5, 5), 6));
    // The store's refusal (`LiveVerdict.no_space`): `kept_no_space` at the
    // fsync point, counted apart by the syncfs point — the caller's ENOSPC.
    try r.seg.put(tkey(7, 7), &v, 7, 0, 0);
    kicks.no_space_next = true;
    try testing.expectEqual(FsyncPromote.kept_no_space, promoteAtFsync(&r.seg, kicks.hooks(), tkey(7, 7)));
    try testing.expectEqual(@as(?u64, 7), r.seg.residentGeneration(tkey(7, 7)));
    kicks.no_space_next = true;
    try testing.expectEqual(SyncfsKept{ .no_space = 1 }, try promoteAllAtSyncfs(gpa, testing.io, &r.seg, kicks.hooks()));
    kicks.fail_next = true;
    try testing.expectEqual(SyncfsKept{ .declined = 1 }, try promoteAllAtSyncfs(gpa, testing.io, &r.seg, kicks.hooks()));
    try testing.expect(r.seg.remove(tkey(7, 7), 7));
}

test "the in-place overwrite: a write inside ONE grain of a resident record lands in the record's own grain — header-first (the other slot carries the new checksum before the grain moves: a kill between recovers the OLD record, a kill after the NEW), no new slot, no superseded cell, nothing freed, the generation and hash state move on (an append continues from it); a span across two grains and a write past the value decline (the caller re-stages); readers copy a grain whole under a racing writer (never a torn grain) — red before: every rewrite moved the image into a fresh slot" {
    const gpa = testing.allocator;
    const io = testing.io;
    const image = try gpa.alloc(u8, 64 * 4096);
    defer gpa.free(image);
    @memset(image, 0);
    var seg = try Segment.init(gpa, image, .none);
    defer seg.deinit();
    const k = tkey(1, 11);
    var value: [16384]u8 = @splat('a');
    try seg.put(k, &value, 1, 0, 0);
    const e0 = seg.residentExtent(k).?;
    const ov0 = stats.staging_inplace_overwrites.load();
    const free0 = seg.free_cells.items.len;
    // Grain 1 (bytes 4096..8192) rewritten in place.
    const patch: [4096]u8 = @splat('b');
    try seg.overwriteInPlace(io, k, 4096, &patch, 1, 2, 0);
    const e1 = seg.residentExtent(k).?;
    try testing.expectEqual(e0.rec_off, e1.rec_off); // the same slot …
    try testing.expectEqual(e0.slot.other(), e1.slot); // … the other header
    try testing.expectEqual(@as(u64, 2), e1.generation);
    try testing.expectEqual(free0, seg.free_cells.items.len); // no cell moved
    try testing.expectEqual(@as(u64, 0), seg.dirtyFreeBytes());
    try testing.expectEqual(@as(?*Cell, null), seg.retired_head);
    try testing.expectEqual(ov0 + 1, stats.staging_inplace_overwrites.load());
    @memcpy(value[4096..8192], &patch);
    {
        const l = seg.get(k).?;
        defer l.release();
        try testing.expectEqualSlices(u8, &value, l.bytes());
        var out: [16384]u8 = undefined;
        try testing.expectEqual(@as(usize, 16384), l.copyRange(io, &out, 0));
        try testing.expectEqualSlices(u8, &value, &out);
    }
    // The record verifies under its new header; recovery adopts generation 2.
    try testing.expect(verifyRecordAt(image, e1.rec_off, readHeaderAt(image, e1.rec_off, e1.slot).?, e1.slot));
    {
        const cut = try gpa.dupe(u8, image);
        defer gpa.free(cut);
        var rec = try Segment.init(gpa, cut, .none);
        defer rec.deinit();
        _ = rec.recover();
        const l = rec.get(k).?;
        defer l.release();
        try testing.expectEqual(@as(u64, 2), l.generation());
        try testing.expectEqualSlices(u8, &value, l.bytes());
    }
    // Header-first: a kill after the new header but before the grain
    // leaves the OLD record whole — recovery over an image with the new
    // header and the old grain adopts generation 2's predecessor's bytes
    // under the still-valid old slot.
    {
        const cut = try gpa.dupe(u8, image);
        defer gpa.free(cut);
        const patch2: [4096]u8 = @splat('c');
        // The header of generation 3 lands (slot e1.slot.other()); the grain does not.
        var state: ValueHash = .init();
        state.update(value[0..8192]);
        state.update(&patch2);
        state.update(value[12288..]);
        writeHeader(cut, e1.rec_off, e1.slot.other(), k.bytes(), 16384, 3, 0, &state);
        var rec2 = try Segment.init(gpa, cut, .none);
        defer rec2.deinit();
        _ = rec2.recover();
        const l = rec2.get(k).?;
        defer l.release();
        try testing.expectEqual(@as(u64, 2), l.generation());
        try testing.expectEqualSlices(u8, &value, l.bytes());
        // … and once the grain lands, generation 3 is the record.
        @memcpy(cut[@intCast(e1.valOff() + 8192)..][0..4096], &patch2);
        var rec3 = try Segment.init(gpa, cut, .none);
        defer rec3.deinit();
        _ = rec3.recover();
        const l3 = rec3.get(k).?;
        defer l3.release();
        try testing.expectEqual(@as(u64, 3), l3.generation());
        try testing.expectEqualSlices(u8, &patch2, l3.bytes()[8192..12288]);
    }
    // The hash state continues: an in-place append after the overwrite verifies.
    try seg.put(k, &value, 4, 0, reserveFor(KEY_LEN, value.len, .append)); // a fresh slot with append slack
    try seg.overwriteInPlace(io, k, 0, &patch, 4, 5, 0);
    const tail: [100]u8 = @splat('t');
    try seg.appendInPlace(k, 16384, &tail, 5, 6, 0);
    const e6 = seg.residentExtent(k).?;
    try testing.expect(verifyRecordAt(image, e6.rec_off, readHeaderAt(image, e6.rec_off, e6.slot).?, e6.slot));
    // Declines: across two grains, past the value, a stale generation.
    try testing.expectError(error.NoRoom, seg.overwriteInPlace(io, k, 4000, &patch, 6, 7, 0));
    try testing.expectError(error.NoRoom, seg.overwriteInPlace(io, k, 16384, &patch, 6, 7, 0));
    try testing.expectError(error.Stale, seg.overwriteInPlace(io, k, 0, &patch, 5, 7, 0));
    // Readers never see a torn grain: a writer alternates grain 0 between
    // all-'x' and all-'y' while a reader copies it and checks uniformity.
    const k3 = tkey(3, 33);
    var v3: [8192]u8 = @splat('x');
    try seg.put(k3, &v3, 10, 0, 0);
    const Race = struct {
        fn writer(s: *Segment, io_: Io, key: StagingKey, stop: *std.atomic.Value(bool)) void {
            var gen: u64 = 10;
            var fill: u8 = 'y';
            while (!stop.load(.acquire)) {
                const g: [4096]u8 = @splat(fill);
                s.overwriteInPlace(io_, key, 0, &g, gen, gen + 1, 0) catch return;
                gen += 1;
                fill = if (fill == 'y') 'x' else 'y';
            }
        }
        fn reader(s: *Segment, io_: Io, key: StagingKey, stop: *std.atomic.Value(bool), torn: *u32, copies: *u32) void {
            var out: [4096]u8 = undefined;
            var n: u32 = 0;
            while (n < 4000) : (n += 1) {
                const l = s.get(key) orelse break;
                defer l.release();
                _ = l.copyRange(io_, &out, 0);
                if (!std.mem.allEqual(u8, &out, out[0])) torn.* += 1;
            }
            copies.* = n;
            stop.store(true, .release);
        }
    };
    var stop: std.atomic.Value(bool) = .init(false);
    var torn: u32 = 0;
    var copies: u32 = 0;
    var race: Io.Group = .init;
    race.async(io, Race.writer, .{ &seg, io, k3, &stop });
    race.async(io, Race.reader, .{ &seg, io, k3, &stop, &torn, &copies });
    race.await(io) catch |err| switch (err) {
        error.Canceled => unreachable,
    };
    try testing.expectEqual(@as(u32, 0), torn);
    try testing.expectEqual(@as(u32, 4000), copies);
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

test "a lost staged payload reads size-consistent zeros: never past the file (the read path's lost arm)" {
    try testing.expectEqual(@as(u64, 200), lostStagedRangeLen(100, 4096, 300));
    try testing.expectEqual(@as(u64, 0), lostStagedRangeLen(500, 10, 300));
    try testing.expectEqual(@as(u64, 10), lostStagedRangeLen(0, 10, 300));
    try testing.expectEqual(@as(u64, 300), lostStagedRangeLen(0, std.math.maxInt(u64), 300));
}

test "the marker: 88 B, checksummed, names its owner (node, mount slot, member, appender, pid), identity = hash64(set_uuid ‖ \"staging\") and checked against its own uuid; every refusal; the member-scoped bind ladder's verdicts (0003 §T.1, §BG: a different member under the same slot is a peer)" {
    const uuid: [16]u8 = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    const m: Marker = .{ .set_uuid = uuid, .set_epoch = 3, .node_token = 0xABC, .mount_slot = 2, .member = 0xB0B0, .appender_id = 5, .owner_pid = 31337 };
    const bytes = m.encode();
    try testing.expectEqual(@as(usize, 88), bytes.len);
    try testing.expectEqual(@as(u32, 5), std.mem.readInt(u32, bytes[64..68], .little));
    try testing.expectEqual(@as(u32, 31337), std.mem.readInt(u32, bytes[68..72], .little));
    try testing.expectEqual(@as(u64, 0xB0B0), std.mem.readInt(u64, bytes[72..80], .little));
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
    try testing.expectError(error.Corrupt, Marker.decode(bytes[0..87]));
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
    // The same slot under ANOTHER member identity (a second fabric
    // identity on this host — the one that preempts a stopped holder's
    // key): a peer, never adopted (0003 §BG).
    var second_identity = m;
    second_identity.member = 0xC0C0;
    try testing.expectEqual(BindVerdict.foreign_owner, bindVerdict(second_identity, m));
    // The claim's name is the member scope: the set, the slot, the member.
    var nb1: [64]u8 = undefined;
    var nb2: [64]u8 = undefined;
    var nb3: [64]u8 = undefined;
    const n_ours = Ring.Ram.claimName(&nb1, m.identity(), m.mount_slot, m.member);
    try testing.expectEqualStrings(n_ours, Ring.Ram.claimName(&nb2, predecessor.identity(), predecessor.mount_slot, predecessor.member));
    try testing.expect(!std.mem.eql(u8, n_ours, Ring.Ram.claimName(&nb3, second_identity.identity(), second_identity.mount_slot, second_identity.member)));
    try testing.expect(!std.mem.eql(u8, n_ours, Ring.Ram.claimName(&nb3, peer.identity(), peer.mount_slot, peer.member)));
    // The member identity: the pair hashed with a separator (no
    // concatenation ambiguity), 0 is a local device's.
    try testing.expect(memberIdentity("nqn.a", "id") != memberIdentity("nqn.", "aid"));
    try testing.expectEqual(memberIdentity("nqn.a", "id"), memberIdentity("nqn.a", "id"));
    try testing.expect(memberIdentity("nqn.a", "id") != 0);
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
    try testing.expect(std.mem.indexOf(u8, out, "\"fsync_promoted_files\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_inplace_overwrites\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_fsync_") == null);
    try testing.expect(std.mem.indexOf(u8, out, "\"staging_departed") == null);
    try testing.expect(std.mem.endsWith(u8, out, "}"));
    var pbuf: [4096]u8 = undefined;
    var pw: std.Io.Writer = .fixed(&pbuf);
    try writePhasesJson(&pw);
    try testing.expect(std.mem.indexOf(u8, pw.buffered(), "\"backing_reclaim\":") != null);
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

test "linux: the write cache (charter §6.1 'Staged', 2026-10-21) — the ring in a sealed memfd behind the member's claim: a second bind of the same member while the first lives is OwnerLive (never a second ring) while a SECOND MEMBER identity at the same slot binds its own ring beside it (0003 §BG); the first dies (its fds gone, its supervisor holding the memfd and the claim) and hands the ring to the successor over the claim: every resident recovered whole, the generation counter seeded past the recovered ones (a kill that keeps both copies of a re-stage finds the newer higher); a clean release frees the claim and the next bind is fresh" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "staging: Linux only");
    const gpa = testing.allocator;
    const rl = @import("../ipc/rendezvous_linux.zig");
    // A uuid of this process and this test: the claim is a host-wide name.
    var uuid: [16]u8 = @splat(0x7C);
    std.mem.writeInt(u32, uuid[0..4], @intCast(linux.getpid()), .little);
    std.mem.writeInt(u64, uuid[4..12], @bitCast(clock.monotonicNs()), .little);
    const cap: u64 = 64 * 4096;
    const owner: Marker = .{ .set_uuid = uuid, .set_epoch = 1, .node_token = 0xA0, .mount_slot = 7, .appender_id = 0, .owner_pid = 1111 };
    const next: Marker = .{ .set_uuid = uuid, .set_epoch = 1, .node_token = 0xA0, .mount_slot = 7, .appender_id = 0, .owner_pid = 2222 };
    const live0 = stats.staging_owner_live_refusals.load();
    var refusal: Ring.BindRefusal = .{};
    var a = try Ring.Ram.bind(owner, cap, &refusal);
    try testing.expectEqual(BindVerdict.absent, a.verdict);
    try testing.expectEqual(cap, a.capacity());
    try testing.expectEqual(@as(?Marker, owner), Marker.decode(a.mapped.map[0..MARKER_LEN]) catch null);
    // The daemon's own ring: residents, one of them re-staged (the
    // superseded copy is released at once: nothing is held).
    {
        var seg = try Segment.init(gpa, a.image(), a.backing());
        defer seg.deinit();
        var p = Pool.init(gpa, .none);
        defer p.deinit();
        for (1..4) |i| _ = try stageWrite(&p, &seg, null, tkey(@intCast(i), i), "a resident", 0, .none);
        _ = try stageWrite(&p, &seg, null, tkey(2, 2), "re-staged", 0, .none);
        try testing.expectEqual(@as(?*Cell, null), seg.retired_head);
        try testing.expectEqual(@as(usize, 3), seg.live());
    }
    // A second mount of this member while the first lives: refused.
    try testing.expectError(error.OwnerLive, Ring.Ram.bind(next, cap, &refusal));
    try testing.expectEqual(live0 + 1, stats.staging_owner_live_refusals.load());
    // A second MEMBER (another fabric identity on this host, the same set
    // and slot — the one that preempts a stopped holder past T_owner):
    // its own fresh ring beside the live one, never the holder's.
    {
        var second = next;
        second.member = memberIdentity("nqn.2014-08.org.nvmexpress:uuid:b0b0", "b0b0");
        var ring_b = try Ring.Ram.bind(second, cap, &refusal);
        defer ring_b.release();
        try testing.expectEqual(BindVerdict.absent, ring_b.verdict);
        try testing.expect(ring_b.memfd != a.memfd);
        try testing.expectEqual(@as(?Marker, second), Marker.decode(ring_b.mapped.map[0..MARKER_LEN]) catch null);
        try testing.expectEqual(live0 + 1, stats.staging_owner_live_refusals.load());
    }
    // The death: the supervisor's copies of the memfd and the claim outlive
    // the daemon's fds; it listens and hands the memfd to ONE successor.
    const sup_memfd: linux.fd_t = @intCast(linux.dup(a.memfd));
    const sup_claim: linux.fd_t = @intCast(linux.dup(a.claim));
    a.release();
    try testing.expect(Ring.Ram.holdsData(sup_memfd));
    const Supervisor = struct {
        fn run(claim: linux.fd_t, memfd: linux.fd_t) void {
            _ = linux.listen(claim, 1);
            const rc = linux.accept4(claim, null, null, linux.SOCK.CLOEXEC);
            if (linux.errno(rc) != .SUCCESS) return;
            const conn: linux.fd_t = @intCast(rc);
            rl.sendCtl(conn, &.{ 3, 0, 0 }, memfd) catch {};
            _ = linux.close(conn);
            _ = linux.close(claim);
            _ = linux.close(memfd);
        }
    };
    const t = try std.Thread.spawn(.{}, Supervisor.run, .{ sup_claim, sup_memfd });
    var b = try Ring.Ram.bind(next, cap / 2, &refusal);
    t.join();
    var b_open = true;
    defer if (b_open) b.release();
    try testing.expectEqual(BindVerdict.match, b.verdict);
    try testing.expectEqual(owner, b.found.?);
    try testing.expectEqual(cap, b.capacity()); // the recovered size, not the new derivation
    {
        var seg = try Segment.init(gpa, b.image(), b.backing());
        defer seg.deinit();
        const rep = seg.recover();
        try testing.expectEqual(@as(u64, 3), rep.adopted);
        try testing.expectEqual(@as(u64, 0), rep.torn);
        const l = seg.get(tkey(2, 2)).?;
        try testing.expectEqualStrings("re-staged", l.bytes());
        l.release();
        // The seed: generations 1..4 were minted; the next is 5.
        var p = Pool.init(gpa, .none);
        defer p.deinit();
        try p.seedFromRecovery(&seg);
        try testing.expectEqual(@as(u64, 5), p.next_generation.load(.monotonic));
        // A re-stage of a recovered record out-ranks it.
        _ = try stageWrite(&p, &seg, null, tkey(2, 2), "again", 0, .none);
        try testing.expectEqual(@as(?u64, 5), seg.residentGeneration(tkey(2, 2)));
    }
    // A clean release frees the claim: the next bind of the member is fresh.
    b.release();
    b_open = false;
    var c = try Ring.Ram.bind(next, cap, &refusal);
    defer c.release();
    try testing.expectEqual(BindVerdict.absent, c.verdict);
    try testing.expect(!Ring.Ram.holdsData(c.memfd));
}

test "tie: the write cache's sizing — the table one item per smallest record (2 grains) a shard, at least 16; the index grows on demand to exactly the table at the sealed ring's bound (27 GiB over 32 shards: 110,592 a shard)" {
    const items = writeCacheItems(27 * derive.GiB, 32);
    try testing.expectEqual(@as(usize, 27 * derive.GiB / (2 * RECORD_ALIGN) / 32), items.max);
    try testing.expectEqual(@as(usize, 110_592), items.max);
    try testing.expectEqual(items.max, items.indexFor(27 * derive.GiB, 32));
    try testing.expectEqual(@as(usize, 16), writeCacheItems(64 * derive.MiB, 1024).max);
    try testing.expectEqual(@as(usize, 16), writeCacheItems(64 * derive.MiB, 1024).indexFor(64 * derive.MiB, 1024));
}
