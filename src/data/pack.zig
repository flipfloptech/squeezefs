//! GPFS-style small-file packing (charter §6.1 "Packed"; `docs/porting/
//! data_layouts.md` §4). PORTABLE over seams: the allocator, the device DMA,
//! the ranged read and the KV commit are vtables the data plane binds
//! (`data/block_refs.zig` + the allocator land concurrently; tests bind RAM
//! worlds), so the packing LAW is pinned here independent of any device.
//!
//! The law with two faces: `pack_max_slot = CHUNK/2` — an own block wastes
//! `CHUNK − s` forever (a staged file never grows in place), a packed tenant
//! costs one compaction copy of `s`, so packing wins iff `CHUNK − s ≥ s`;
//! the SAME break-even is the compaction victim line (a pack block whose live
//! bytes are ≤ it is a victim). Measured: 96,000 fsync-promoted 16 KiB files
//! = 375 blocks instead of 96,000 (exactly `ceil(Σ ceil(len) / CHUNK)`);
//! the dismount row 20,000 → 148; files/s +0.5 %, fsync −3.7 %.
//!
//! One OPEN PACK per `(data volume, pack scope)` is RAM only — `{base,
//! next_slot, committed, opened_at, sealed}`: a slot reservation is ONE
//! `fetch_add(slot_len)`, overflow ⇒ CAS-seal + single-flight refill. The
//! block is PINNED at `refcount = committed + in-flight + 1` (the packer's
//! own +1 minted by the placed allocation; every tenant adds one at reserve),
//! so no sequence of tenant deletes can free it under a DMA; the pin's
//! arithmetic and the seal's single sealer are model-checked below, and the
//! seal re-checks the arithmetic (`pack_pin_refcount_drift`, Q-15). Seals:
//! FULL, DISMOUNT, DRAIN — no BATCH (the co-writer frame it closed was
//! retired with the symmetric plane; the gauge family matches the enum,
//! Q-21/Q-22), no idle seal (one half-empty block per volume per scope is
//! the stated cost, visible through the open-block gauges).
//!
//! Per tenant: `prepare` (the staging image, ONE DMA of exactly `slot`
//! bytes at `base + off` — the completion mints the `DmaProof`) then
//! `commitLocked` (under the CALLER's (3.5) meta stripe + (3.7) layout
//! lock, `layout.PublishLocks.lockPromotion`: re-check staged + same
//! file_id + same generation; the packed layout with the tenant mapping,
//! its `+ref` and any displaced `−ref` in the SAME tx; release the ring
//! entry inside the section; the displaced block RETURNED for the caller
//! to free after its locks drop — RES-1). DMA strictly precedes commit:
//! `commitLocked` takes the `Prepared` the DMA minted.
//! Crash contract: nothing durable records the open block; ≥ 1 committed
//! tenant ⇒ the block recovers ALLOCATED at `refcount = N` from the refs
//! records; none ⇒ FREE.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const clock = @import("../core/clock.zig");
const counters = @import("../core/counters.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const tx = @import("../meta/tx.zig");
const jc = @import("../meta/journal_core.zig");
const sync = @import("../sync/sync.zig");
const explorer = @import("../model/explorer.zig");
const layout = @import("layout.zig");
const staging = @import("staging.zig");

pub const Mapping = layout.Mapping;
pub const DmaProof = layout.DmaProof;
pub const DmaSeam = layout.DmaSeam;
pub const RefsSeam = layout.RefsSeam;
pub const Geometry = layout.Geometry;

pub const stats = struct {
    /// `layout_promoted_{inline,block,packed}`: the promotion dispatch
    /// (`packed + block + inline ≡ promotions`).
    pub var layout_promoted_inline: counters.ShardedCounter = .{};
    pub var layout_promoted_block: counters.ShardedCounter = .{};
    pub var layout_promoted_packed: counters.ShardedCounter = .{};
    /// `pack_own_block_promotions`: slots above `pack_max_slot` (0 on a
    /// ≤ CHUNK/2 population).
    pub var pack_own_block_promotions: counters.ShardedCounter = .{};
    /// `pack_promoted_bytes` / `pack_slot_pad_bytes`: image bytes packed and
    /// the GRAIN pad (`pad/bytes` = the 4 KiB-grain tax).
    pub var pack_promoted_bytes: counters.ShardedCounter = .{};
    pub var pack_slot_pad_bytes: counters.ShardedCounter = .{};
    /// The pack life cycle (`abandoned ≈ 0`).
    pub var pack_blocks_opened: counters.ShardedCounter = .{};
    pub var pack_blocks_sealed_full: counters.ShardedCounter = .{};
    pub var pack_blocks_sealed_dismount: counters.ShardedCounter = .{};
    pub var pack_blocks_sealed_drain: counters.ShardedCounter = .{};
    /// The key-rotation sweep's confirming census seals every open pack
    /// (its tenants become classifiable; no old-key tenant lands after).
    pub var pack_blocks_sealed_rekey: counters.ShardedCounter = .{};
    pub var pack_blocks_abandoned: counters.ShardedCounter = .{};
    /// Refill / OQ-1 / dead slots (`refills ≈ opened`).
    pub var pack_reservation_refills: counters.ShardedCounter = .{};
    pub var pack_refill_storage_full: counters.ShardedCounter = .{};
    pub var pack_arm_stopped_promotions: counters.ShardedCounter = .{};
    pub var pack_slots_abandoned: counters.ShardedCounter = .{};
    /// Tenant releases (`partial + terminal ≡ releases`) and PK3's clip.
    pub var pack_partial_frees: counters.ShardedCounter = .{};
    pub var pack_terminal_frees: counters.ShardedCounter = .{};
    pub var pack_mapping_clips: counters.ShardedCounter = .{};
    /// The read funnel.
    pub var packed_reads: counters.ShardedCounter = .{};
    pub var packed_read_bytes: counters.ShardedCounter = .{};
    /// `staged_identity_retries`: a binding-validated read found the
    /// identity moved and re-resolved.
    pub var staged_identity_retries: counters.ShardedCounter = .{};
    /// Levels.
    pub var pack_open_blocks: counters.Gauge = .{};
    pub var pack_open_scopes: counters.Gauge = .{};
    pub var pack_max_slot_bytes: counters.Gauge = .{};
    /// `pack_release_untracked_noops`: a pin release on a block the
    /// allocator no longer tracks — must stay 0 single-writer.
    pub var pack_release_untracked_noops: counters.Tripwire = .init("pack_release_untracked_noops");
    /// `pack_pin_refcount_drift`: at seal `refcount + released ≠ committed
    /// + inflight + 1` — the words disagree (Q-15). Must stay 0.
    pub var pack_pin_refcount_drift: counters.Tripwire = .init("pack_pin_refcount_drift");
    /// `read_stale_bound_mapping`: a stale-incarnation refusal on a mapping
    /// the identity still binds (finding 51) — EIO, never zeros, never the
    /// reissued offset's bytes. Must stay 0.
    pub var read_stale_bound_mapping: counters.Tripwire = .init("read_stale_bound_mapping");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "layout_promoted_inline", "layout_promoted_block", "layout_promoted_packed", "pack_own_block_promotions", "pack_promoted_bytes", "pack_slot_pad_bytes", "pack_blocks_opened", "pack_blocks_sealed_full", "pack_blocks_sealed_dismount", "pack_blocks_sealed_drain", "pack_blocks_sealed_rekey", "pack_blocks_abandoned", "pack_reservation_refills", "pack_refill_storage_full", "pack_arm_stopped_promotions", "pack_slots_abandoned", "pack_partial_frees", "pack_terminal_frees", "pack_mapping_clips", "packed_reads", "packed_read_bytes", "staged_identity_retries", "pack_open_blocks", "pack_open_scopes", "pack_max_slot_bytes", "pack_release_untracked_noops", "pack_pin_refcount_drift", "read_stale_bound_mapping" }, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// §4.1 The size dispatch and the two-faced law.
// ---------------------------------------------------------------------------

/// `ceil(len)` to the LBA grain: `pack_slot_len(1) = 4096`, `(4096) =
/// 4096`, `(4097) = 8192`.
pub fn packSlotLen(stored_image_len: u64) u64 {
    return layout.ceilGrain(stored_image_len);
}

/// `explicit > derived CHUNK/2` (the break-even in the module doc).
pub fn packMaxSlot(explicit: ?u64, chunk: u64) u64 {
    return explicit orelse chunk / 2;
}

/// The compaction victim predicate — the same line as the own-block
/// threshold (`defrag::is_pack_victim`).
pub fn isPackVictim(live_bytes: u64, max_slot: u64) bool {
    return live_bytes <= max_slot;
}

/// The lever + the threshold in force. Publishes `pack_max_slot_bytes`.
pub const Policy = struct {
    enabled: bool,
    max_slot: u64,

    pub fn resolve(enabled: bool, explicit_max: ?u64, chunk: u64) Policy {
        const p: Policy = .{ .enabled = enabled, .max_slot = packMaxSlot(explicit_max, chunk) };
        stats.pack_max_slot_bytes.set(p.max_slot);
        return p;
    }

    pub fn fromEnv(env: *const std.process.Environ.Map, chunk: u64) Policy {
        return resolve(knobs.boolKnob("SQUEEZEFS_SMALL_FILE_PACKING", env), knobs.optIntKnob("SQUEEZEFS_PACK_MAX_SLOT_BYTES", u64, env), chunk);
    }
};

pub const Dispatch = enum { inline_, own_block, packed_ };

pub const DispatchError = error{
    /// A stored image above CHUNK (FIND-RW4-A): stays resident, loud.
    ImageExceedsChunk,
};

/// §4.4 `prepare_promotion`'s size dispatch: `logical ≤ inline_max` ⇒
/// inline; else by the stored (transformed) image's slot.
pub fn dispatch(policy: Policy, logical_len: u64, stored_len: u64, inline_max: u64, chunk: u64) DispatchError!Dispatch {
    if (logical_len <= inline_max) return .inline_;
    if (stored_len > chunk) return error.ImageExceedsChunk;
    if (policy.enabled and packSlotLen(stored_len) <= policy.max_slot) return .packed_;
    stats.pack_own_block_promotions.inc();
    return .own_block;
}

/// `ceil(Σ ceil(len_i) / CHUNK)`: the blocks a packed population of `files`
/// images of `len` bytes occupies when every pack fills (the 375 law).
pub fn packedBlocks(files: u64, len: u64, chunk: u64) u64 {
    return (files * packSlotLen(len) + chunk - 1) / chunk;
}

// ---------------------------------------------------------------------------
// §4.5 The pack scope.
// ---------------------------------------------------------------------------

/// `(meta_volume << 32) | forest_slot` on an armed set — the native slot is 0
/// on EVERY meta volume, so the ordinal keeps two volumes' natives apart; a
/// pack block's tenants are inos of ONE slot of ONE volume, so every
/// reference lives in one slot tree and `refcount(b)` is one range probe.
/// Unarmed: scope 0 = one open pack per data volume.
pub fn packScopeKey(meta_volume: u32, forest_slot: u32) u64 {
    return (@as(u64, meta_volume) << 32) | forest_slot;
}

pub const PackKey = struct { vol_tag: u64, scope: u64 };

// ---------------------------------------------------------------------------
// §4.2–§4.3 The open pack.
// ---------------------------------------------------------------------------

pub const SealKind = enum { full, dismount, drain, rekey };

pub const OpenPack = struct {
    vol_tag: u64,
    scope: u64,
    /// Device byte offset of the block (CHUNK-aligned).
    base: u64,
    incarnation: u64,
    chunk: u64,
    /// Process-monotone open sequence (gauges, tests).
    seq: u64,
    opened_at_ns: u64,
    /// The monotone cursor: every reservation after the overflowing one
    /// overflows too.
    next_slot: std.atomic.Value(u64) = .init(0),
    /// Tenants whose commit landed (a gauge; the durable count is the refs).
    committed: std.atomic.Value(u32) = .init(0),
    /// Tenants between reserve and commit/abandon.
    inflight: std.atomic.Value(u32) = .init(0),
    /// Committed tenants whose reference LEFT while the pack was open (an
    /// unlink's corpse, a rewrite's displaced copy, a compactor's refused
    /// republish): the pin arithmetic's fourth word.
    released: std.atomic.Value(u32) = .init(0),
    /// CAS false→true: the first sealer runs the seal.
    sealed: std.atomic.Value(bool) = .init(false),
    /// Serializes every (RAM refcount step, tenant-word step) pair of an
    /// OPEN pack with the seal's snapshot, so the seal's arithmetic is
    /// exact: `refcount + released ≡ committed + inflight + 1`. Held only
    /// while `!sealed` (a release under it is nonterminal by the pin — a
    /// RAM decrement, never the terminal ladder's park); once sealed the
    /// words are history and every release runs the plain ladder.
    pin_lock: jc.SpinLock = .{},

    /// `[prev, prev + slot_len)` iff it fits the chunk. acq_rel: the
    /// winner's slot is published to the sealer's occupancy read.
    pub fn tryReserve(self: *OpenPack, slot_len: u64) ?u64 {
        const prev = self.next_slot.fetchAdd(slot_len, .acq_rel);
        return if (prev + slot_len <= self.chunk) prev else null;
    }

    /// True for exactly one caller. acq_rel pairs with the loads in
    /// `runSeal`; the loser's acquire sees the winner's seal.
    pub fn trySeal(self: *OpenPack) bool {
        return self.sealed.cmpxchgStrong(false, true, .acq_rel, .acquire) == null;
    }

    pub fn isSealed(self: *const OpenPack) bool {
        return self.sealed.load(.acquire);
    }

    /// A tenant's commit landed: in-flight → committed as ONE step against
    /// the seal's snapshot.
    pub fn landed(self: *OpenPack) void {
        self.pin_lock.lock();
        defer self.pin_lock.unlock();
        _ = self.inflight.fetchSub(1, .acq_rel);
        _ = self.committed.fetchAdd(1, .acq_rel);
    }

    /// The pin lock, taken only while the pack is OPEN (the seal's CAS
    /// precedes its snapshot, so a holder that sees `!sealed` is inside
    /// the arithmetic the seal will read). False = sealed, nothing held.
    pub fn lockOpen(self: *OpenPack) bool {
        if (self.isSealed()) return false;
        self.pin_lock.lock();
        if (self.isSealed()) {
            self.pin_lock.unlock();
            return false;
        }
        return true;
    }

    /// Bytes reserved so far, clipped to the chunk (an overflowing cursor
    /// reads as full).
    pub fn reservedBytes(self: *const OpenPack) u64 {
        return @min(self.next_slot.load(.monotonic), self.chunk);
    }

    pub fn occupancyPermille(self: *const OpenPack) u64 {
        return self.reservedBytes() * 1000 / self.chunk;
    }

    pub fn ageMs(self: *const OpenPack, now_ns: u64) u64 {
        return (now_ns -| self.opened_at_ns) / std.time.ns_per_ms;
    }
};

// ---------------------------------------------------------------------------
// The seams.
// ---------------------------------------------------------------------------

pub const Placed = struct { vol_tag: u64, offset: u64, incarnation: u64 };

/// A block minted on the volume the caller named.
pub const Mint = struct { offset: u64, incarnation: u64 };

pub const AllocError = error{ StorageFull, VolumeFailed, OutOfMemory };

pub const ReleaseVerdict = enum {
    /// The last reference: the block goes through the ordinary free ladder.
    terminal,
    /// References remain (a dead slot inside a live block).
    nonterminal,
    /// The allocator does not track the block (must stay 0).
    untracked,
};

/// The block allocator's face (`data/block_refs.zig` + the allocator bind it;
/// the RAM refcount is theirs). `allocate_placed` mints ONE block on
/// `k.vol_tag` at refcount 1 = the packer's PIN. The key's volume is the one
/// the promotion picked and transformed the image for; the seam never picks
/// again (a second pick put key A's pack on B, and fsck missed it: leak67).
/// `pin` is acquire-from-nonzero (false ⇒ the count hit 0 under the
/// caller); `publish` is the idempotent `fetch_or` that stabilises the
/// block's word (the FIRST tenant's publish — a stable word keeps the block
/// movable and a peer's tenant free answerable); `abandon` returns a
/// never-published mint.
pub const AllocSeam = struct {
    ctx: *anyopaque,
    allocate_placed: *const fn (ctx: *anyopaque, k: PackKey) AllocError!Mint,
    pin: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64) bool,
    release: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64) ReleaseVerdict,
    publish: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64) void,
    abandon: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64) void,
    refcount: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64) u32,
};

pub const ReadError = error{ InputOutput, StaleIncarnation, QueueFull, WorkerShutdown, OutOfMemory };

/// ONE ranged read `[device_off, device_off + out.len)` on `vol_tag` — the
/// mapping's OWN volume (FIND-PK-0), never a prefix from the block start
/// (FIND-PK-1). `StaleIncarnation` is the device layer's incarnation
/// refusal for `incarnation`.
pub const ReadSeam = struct {
    ctx: *anyopaque,
    read_range: *const fn (ctx: *anyopaque, vol_tag: u64, device_off: u64, incarnation: u64, out: []u8) ReadError!void,
};

/// The CURRENT layout identity of an ino, read under the caller's meta
/// stripe (borrowed until the guard drops).
pub const LookupSeam = struct {
    ctx: *anyopaque,
    current: *const fn (ctx: *anyopaque, ino: u64) ?layout.Layout,
};

pub const CommitError = error{ CommitFailed, OutOfMemory };

/// The KV backend's commit of a staged tx (`meta/backend.zig`'s conveyor).
/// The backend's contract: `commit` CONSUMES the tx on every return — a
/// failed commit has already abandoned it (`KvBackend.commit`), so the
/// caller never touches it again.
pub const CommitSeam = struct {
    ctx: *anyopaque,
    commit: *const fn (ctx: *anyopaque, t: *tx.KvTx) CommitError!void,
};

// ---------------------------------------------------------------------------
// The packer.
// ---------------------------------------------------------------------------

/// A reserved slot: the tenant's OWN registration on `base` (one pin).
pub const Tenant = struct {
    pack: *OpenPack,
    off: u64,
    slot_len: u64,
};

/// The prepared tenant: DMA done, proof minted, mapping named.
pub const Prepared = struct {
    tenant: Tenant,
    proof: DmaProof,
    mapping: Mapping,
    image_len: u64,
};

pub const ReserveError = error{ PackTableFull, StorageFull, VolumeFailed, OutOfMemory, Canceled, FlightTableFull };
pub const PrepareError = ReserveError || layout.DmaError;

pub const CommitOutcome = union(enum) {
    /// The layout flipped to packed; the ring entry released. The payload
    /// is the displaced durable copy (a re-staged file's earlier tenant),
    /// the caller's to `releaseTenantRef` AFTER its locks drop.
    committed: ?Mapping,
    /// The identity moved (re-staged, promoted elsewhere, gone): the slot is
    /// abandoned, nothing published; the tx is untouched (the caller's).
    stale,
    /// The slot abandoned, nothing published; who still owns the tx is the
    /// payload's word.
    failed: Failed,

    pub const Failed = enum {
        /// The layout publish failed before any commit: the tx is live and
        /// the caller's to abandon.
        publish,
        /// The KV commit failed: the conveyor CONSUMED the tx (refs
        /// retracted with it) — abandoning it again was the dismount
        /// double-free (a joiner promoting its packed files at `umount`).
        commit,
    };
};

pub const CommitCtx = struct {
    /// The LOCAL key ino.
    ino: u64,
    name_key: *const key.NameKey,
    coll_seq: u8 = 0,
    file_id: layout.FileId,
    /// The ring generation the image was read at.
    generation: u64,
    lookup: LookupSeam,
    refs: RefsSeam,
    commit: CommitSeam,
    pool: *staging.Pool,
    seg: *staging.Segment,
    /// The write cache's core (the released resident leaves it); null in
    /// the portable tests.
    cache: ?*staging.WriteCache = null,
    geo: Geometry,
};

const Row = struct {
    key: PackKey = .{ .vol_tag = 0, .scope = 0 },
    used: bool = false,
    pack: std.atomic.Value(?*OpenPack) = .init(null),
};

/// One refill in flight per key: every concurrent reserver for the same
/// key waits for the leader's block. 64 slots = the mint rotor's slot bound
/// (`MINT_SPREAD`): more concurrent refills than a mount has rotor slots
/// per volume means a volume population this table was not sized for.
pub const max_refills: usize = 64;
const Flight = sync.SingleFlight(PackKey, *OpenPack, max_refills);

pub const Packer = struct {
    gpa: Allocator,
    chunk: u64,
    policy: Policy,
    alloc: AllocSeam,
    dma: DmaSeam,
    read: ReadSeam,
    rows: []Row,
    rows_lock: jc.SpinLock = .{},
    /// Every pack ever opened lives until `deinit`: a vacated row's pointer
    /// may still be in a reserver's hand (the Arc the reference carried;
    /// 375 packs per 96,000 files makes the retention free).
    opened: std.ArrayList(*OpenPack) = .empty,
    refill: Flight = .init,
    /// OQ-1: a `StorageFull` refill stops the arm for the rest of the
    /// promotion batch (ONE aggregated WARN); `beginPromotionBatch` re-arms.
    stopped: std.atomic.Value(bool) = .init(false),
    seq: std.atomic.Value(u64) = .init(0),

    /// `capacity` rows: the data-volume count × the leased-slot bound.
    pub fn init(gpa: Allocator, chunk: u64, policy: Policy, alloc: AllocSeam, dma: DmaSeam, read: ReadSeam, capacity: usize) Allocator.Error!Packer {
        const rows = try gpa.alloc(Row, capacity);
        @memset(rows, .{});
        return .{ .gpa = gpa, .chunk = chunk, .policy = policy, .alloc = alloc, .dma = dma, .read = read, .rows = rows };
    }

    /// A packer torn down without a dismount pass (tests) returns its open
    /// blocks' share of the gauge.
    pub fn deinit(self: *Packer) void {
        for (self.rows) |*r| {
            if (!r.used) continue;
            if (r.pack.load(.acquire)) |p| if (!p.isSealed()) stats.pack_open_blocks.sub(1);
        }
        stats.pack_open_scopes.set(0);
        for (self.opened.items) |p| self.gpa.destroy(p);
        self.opened.deinit(self.gpa);
        self.gpa.free(self.rows);
        self.* = undefined;
    }

    pub fn beginPromotionBatch(self: *Packer) void {
        self.stopped.store(false, .release);
    }

    pub fn isStopped(self: *const Packer) bool {
        return self.stopped.load(.acquire);
    }

    fn rowFor(self: *Packer, k: PackKey) error{PackTableFull}!*Row {
        self.rows_lock.lock();
        defer self.rows_lock.unlock();
        var free: ?*Row = null;
        for (self.rows) |*r| {
            if (r.used) {
                if (r.key.vol_tag == k.vol_tag and r.key.scope == k.scope) return r;
            } else if (free == null) free = r;
        }
        const r = free orelse return error.PackTableFull;
        r.* = .{ .key = k, .used = true };
        return r;
    }

    /// The open pack of `k`, if any (gauges, tests).
    pub fn openPack(self: *Packer, k: PackKey) ?*OpenPack {
        const r = self.rowFor(k) catch return null;
        const p = r.pack.load(.acquire) orelse return null;
        return if (p.isSealed()) null else p;
    }

    /// Reserve `slot_len` in `k`'s open pack: an unsealed pack ⇒ reserve →
    /// pin (a refused pin means a concurrent overflow's seal released the
    /// LAST reference: vacate and retry); overflow ⇒ CAS-seal, the first
    /// sealer runs the FULL seal, then refill; no pack ⇒ single-flight
    /// refill. `null` when the arm is stopped for this batch.
    pub fn reserve(self: *Packer, io: Io, k: PackKey, slot_len: u64) ReserveError!?Tenant {
        assert(slot_len > 0 and slot_len <= self.chunk);
        while (true) {
            if (self.isStopped()) {
                stats.pack_arm_stopped_promotions.inc();
                return null;
            }
            const row = try self.rowFor(k);
            if (row.pack.load(.acquire)) |pack| {
                if (pack.isSealed()) {
                    _ = row.pack.cmpxchgStrong(pack, null, .acq_rel, .acquire);
                    continue;
                }
                if (pack.tryReserve(slot_len)) |off| {
                    pack.pin_lock.lock();
                    const pinned = self.alloc.pin(self.alloc.ctx, pack.vol_tag, pack.base);
                    if (pinned) _ = pack.inflight.fetchAdd(1, .acq_rel);
                    pack.pin_lock.unlock();
                    if (!pinned) {
                        if (pack.trySeal()) self.runSeal(row, pack, .full);
                        continue;
                    }
                    return .{ .pack = pack, .off = off, .slot_len = slot_len };
                }
                if (pack.trySeal()) self.runSeal(row, pack, .full);
                continue;
            }
            const ctx: RefillCtx = .{ .packer = self, .key = k, .row = row };
            _ = self.refill.run(io, k, &ctx, RefillCtx.compute) catch |e| switch (e) {
                error.StorageFull => {
                    // OQ-1: ONE aggregated line, the arm stops for the batch.
                    if (!self.stopped.swap(true, .acq_rel)) {
                        stats.pack_refill_storage_full.inc();
                        log.warn("pack refill: vol {x:0>16} has no free block for the open pack — the packed arm stops for the rest of this promotion batch (own-block arm continues)", .{k.vol_tag});
                    }
                    continue;
                },
                error.VolumeFailed => return error.VolumeFailed,
                error.OutOfMemory => return error.OutOfMemory,
                error.Canceled => return error.Canceled,
                error.FlightTableFull => return error.FlightTableFull,
                else => unreachable,
            };
        }
    }

    const RefillCtx = struct {
        packer: *Packer,
        key: PackKey,
        row: *Row,

        /// The leader opens ONE placed block and installs it; an existing
        /// UNSEALED pack wins and the fresh block is abandoned unpublished.
        fn compute(c: *const RefillCtx, _: Io) anyerror!*OpenPack {
            const self = c.packer;
            if (self.isStopped()) return error.StorageFull;
            if (c.row.pack.load(.acquire)) |existing| if (!existing.isSealed()) return existing;
            // The struct before the mint: an OOM leaves the allocator untouched.
            const p = try self.gpa.create(OpenPack);
            const minted = self.alloc.allocate_placed(self.alloc.ctx, c.key) catch |e| {
                self.gpa.destroy(p);
                return e;
            };
            p.* = .{
                .vol_tag = c.key.vol_tag,
                .scope = c.key.scope,
                .base = minted.offset,
                .incarnation = minted.incarnation,
                .chunk = self.chunk,
                .seq = self.seq.fetchAdd(1, .monotonic) + 1,
                .opened_at_ns = clock.monotonicNs(),
            };
            self.rows_lock.lock();
            self.opened.append(self.gpa, p) catch |e| {
                self.rows_lock.unlock();
                self.gpa.destroy(p);
                self.alloc.abandon(self.alloc.ctx, c.key.vol_tag, minted.offset);
                return e;
            };
            self.rows_lock.unlock();
            stats.pack_reservation_refills.inc();
            // Install: a sealed or empty row takes it.
            while (true) {
                const cur = c.row.pack.load(.acquire);
                if (cur) |existing| if (!existing.isSealed()) {
                    // Lost to a concurrent install: abandon the unpublished mint.
                    p.sealed.store(true, .release);
                    self.alloc.abandon(self.alloc.ctx, c.key.vol_tag, minted.offset);
                    return existing;
                };
                if (c.row.pack.cmpxchgStrong(cur, p, .acq_rel, .acquire) == null) break;
            }
            stats.pack_blocks_opened.inc();
            stats.pack_open_blocks.add(1);
            self.refreshScopeGauge();
            return p;
        }
    };

    /// Vacate the row, check the pin arithmetic, release the pin (the
    /// router ladder's verdict), count the seal kind. A terminal release
    /// with `committed == 0` is an ABANDONED pack.
    fn runSeal(self: *Packer, row: *Row, pack: *OpenPack, kind: SealKind) void {
        assert(pack.isSealed());
        _ = row.pack.cmpxchgStrong(pack, null, .acq_rel, .acquire);
        // The snapshot under the pin lock: every tenant step of the open
        // pack landed whole or not at all against it; `sealed` is already
        // set, so a release arriving after it runs the plain ladder.
        pack.pin_lock.lock();
        const committed = pack.committed.load(.acquire);
        const inflight = pack.inflight.load(.acquire);
        const released = pack.released.load(.acquire);
        const refcount = self.alloc.refcount(self.alloc.ctx, pack.vol_tag, pack.base);
        pack.pin_lock.unlock();
        if (refcount + released != committed + inflight + 1) {
            stats.pack_pin_refcount_drift.fire("pack seal: refcount + released ≠ committed + inflight + 1");
        }
        switch (self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base)) {
            .terminal => if (committed == 0 and inflight == 0) stats.pack_blocks_abandoned.inc(),
            .nonterminal => {},
            .untracked => stats.pack_release_untracked_noops.fire("pack seal: the pin's block is untracked"),
        }
        switch (kind) {
            .full => stats.pack_blocks_sealed_full.inc(),
            .dismount => stats.pack_blocks_sealed_dismount.inc(),
            .drain => stats.pack_blocks_sealed_drain.inc(),
            .rekey => stats.pack_blocks_sealed_rekey.inc(),
        }
        stats.pack_open_blocks.sub(1);
        self.refreshScopeGauge();
    }

    fn refreshScopeGauge(self: *Packer) void {
        var n: u64 = 0;
        for (self.rows) |*r| {
            if (!r.used) continue;
            if (r.pack.load(.acquire)) |p| if (!p.isSealed()) {
                n += 1;
            };
        }
        stats.pack_open_scopes.set(n);
    }

    /// DISMOUNT: seal every open pack (after the promotion pass, before
    /// "Dismount clean").
    pub fn sealAll(self: *Packer) void {
        self.sealAllAs(.dismount);
    }

    /// Every open pack sealed under `kind` (the rotation sweep's confirming
    /// census: `.rekey`).
    pub fn sealAllAs(self: *Packer, kind: SealKind) void {
        for (self.rows) |*r| {
            if (!r.used) continue;
            const p = r.pack.load(.acquire) orelse continue;
            if (p.trySeal()) self.runSeal(r, p, kind);
        }
    }

    /// Tenants between reserve and commit/abandon over EVERY pack this
    /// mount opened (sealed ones included: a reserved slot lands after the
    /// seal): the rotation sweep's quiescence word.
    pub fn tenantsInFlight(self: *Packer) u64 {
        self.rows_lock.lock();
        defer self.rows_lock.unlock();
        var n: u64 = 0;
        for (self.opened.items) |p| n += p.inflight.load(.acquire);
        return n;
    }

    /// DRAIN: the mover found the open pack on its victim block.
    pub fn sealBlock(self: *Packer, vol_tag: u64, base: u64) bool {
        for (self.rows) |*r| {
            if (!r.used) continue;
            const p = r.pack.load(.acquire) orelse continue;
            if (p.vol_tag == vol_tag and p.base == base) {
                if (p.trySeal()) {
                    self.runSeal(r, p, .drain);
                    return true;
                }
                return false;
            }
        }
        return false;
    }

    // ---- §4.4 prepare → commit ----

    /// Reserve the slot and run ONE DMA of exactly `slot` bytes at `base +
    /// off` (the device pads `[len, slot)` with zeros); the completion mints
    /// the proof; the FIRST tenant's publish stabilises the block's word. A
    /// DMA error releases the tenant (`pack_slots_abandoned`). `null` = the
    /// arm is stopped.
    pub fn prepare(self: *Packer, io: Io, k: PackKey, image: []const u8, now_ns: u64) PrepareError!?Prepared {
        const slot = packSlotLen(image.len);
        assert(slot <= self.policy.max_slot);
        const tenant = (try self.reserve(io, k, slot)) orelse return null;
        const pack = tenant.pack;
        const proof = self.dma.write(pack.vol_tag, pack.base + tenant.off, image, now_ns) catch |e| {
            self.releaseLandedSite(tenant);
            return e;
        };
        self.alloc.publish(self.alloc.ctx, pack.vol_tag, pack.base);
        stats.pack_promoted_bytes.add(image.len);
        stats.pack_slot_pad_bytes.add(slot - image.len);
        return .{
            .tenant = tenant,
            .proof = proof,
            .mapping = Mapping.tenant(pack.vol_tag, pack.base, pack.incarnation, @intCast(tenant.off), @intCast(image.len)),
            .image_len = image.len,
        };
    }

    /// Under the CALLER's locks — the ino's meta stripe (3.5) and the
    /// plane's layout lock (3.7), taken in that order by
    /// `layout.PublishLocks.lockPromotion` (0002 row 209: this function
    /// taking the stripe itself under the caller's layout lock inverted the
    /// charter's order against every upload): the identity must still be
    /// `staged ∧ same file_id ∧ same ring generation`; the packed layout
    /// (`size = max(size, image)` — never persist a size below the image)
    /// with its `+ref` and the displaced copy's `−ref` in ONE tx; the ring
    /// entry released INSIDE the section, generation-gated (released
    /// outside it landed inside an RMW's stage→commit window and freed the
    /// sole copy). The displaced durable copy is RETURNED for the caller to
    /// release once its locks drop (RES-1: a tenant release can be the
    /// block's terminal free). A failed save leaves the refs retracted with
    /// the tx and abandons the slot.
    pub fn commitLocked(self: *Packer, prepared: Prepared, c: CommitCtx, t: *tx.KvTx) CommitOutcome {
        const sk = staging.StagingKey.init(c.file_id, c.ino);
        const cur = c.lookup.current(c.lookup.ctx, c.ino);
        const bound = if (cur) |l| l.kind == .staged and std.mem.eql(u8, &l.file_id, &c.file_id) else false;
        if (!bound or c.seg.residentGeneration(sk) != c.generation) {
            self.releaseLandedSite(prepared.tenant);
            return .stale;
        }
        const prev = cur.?;
        const new = layout.Layout.packedTenant(@max(prev.size, prepared.image_len), c.file_id, prepared.mapping);
        layout.publishLayout(t, .{ .ino = c.ino, .name_key = c.name_key, .coll_seq = c.coll_seq, .prev = prev, .layout = new, .proofs = &.{prepared.proof} }, c.refs, c.geo) catch {
            self.releaseLandedSite(prepared.tenant);
            return .{ .failed = .publish };
        };
        c.commit.commit(c.commit.ctx, t) catch {
            self.releaseLandedSite(prepared.tenant);
            return .{ .failed = .commit };
        };
        _ = staging.releaseStaged(c.pool, c.seg, c.cache, sk, c.generation);
        prepared.tenant.pack.landed();
        stats.layout_promoted_packed.inc();
        return .{ .committed = prev.durableCopy() };
    }

    /// A prepared tenant the caller cannot commit (its barrier or its
    /// transaction failed before `commitLocked`): the slot is abandoned —
    /// never under the stripe.
    pub fn abandonPrepared(self: *Packer, prepared: Prepared) void {
        self.releaseLandedSite(prepared.tenant);
    }

    /// The tenant's own reference leaves without a commit: TERMINAL when the
    /// pack sealed and every sibling was deleted — so never under the
    /// stripe.
    fn releaseLandedSite(self: *Packer, tenant: Tenant) void {
        stats.pack_slots_abandoned.inc();
        const pack = tenant.pack;
        if (pack.lockOpen()) {
            defer pack.pin_lock.unlock();
            _ = pack.inflight.fetchSub(1, .acq_rel);
            self.countRelease(self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base));
            return;
        }
        _ = pack.inflight.fetchSub(1, .acq_rel);
        self.countRelease(self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base));
    }

    /// Release the reference a mapping holds (a displaced durable copy, an
    /// unlinked tenant): nonterminal = a dead slot inside a live block until
    /// compaction; terminal = the block's end of life through the ordinary
    /// ladder. A tenant of an OPEN pack is counted against the pin
    /// arithmetic first.
    pub fn releaseTenantRef(self: *Packer, m: Mapping) void {
        if (self.releaseOpenTenant(m)) return;
        self.countRelease(self.alloc.release(self.alloc.ctx, m.vol_tag, m.offset));
    }

    /// THE open-tenant release face (the plane's `freeBindings` asks here
    /// first): true iff `m` is a tenant window of a pack still open — its
    /// reference left under the pin lock, `released + 1`, the release
    /// nonterminal by the pin. False = not ours (a whole block, a sealed
    /// pack's tenant): the caller runs the ordinary ladder.
    pub fn releaseOpenTenant(self: *Packer, m: Mapping) bool {
        if (m.whole_block) return false;
        const pack = self.openPackAt(m.vol_tag, m.offset) orelse return false;
        if (!pack.lockOpen()) return false;
        defer pack.pin_lock.unlock();
        _ = pack.released.fetchAdd(1, .acq_rel);
        self.countRelease(self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base));
        return true;
    }

    /// Is `(vol_tag, base)` an open pack's block? The ONE answer every
    /// exemption face asks (fsck, the mover): matched by the pack's own
    /// placement, never through a row key (`openPack` claims a row).
    pub fn isOpenBlock(self: *Packer, vol_tag: u64, base: u64) bool {
        return self.openPackAt(vol_tag, base) != null;
    }

    /// The open pack whose block is `(vol_tag, base)`, if any.
    fn openPackAt(self: *Packer, vol_tag: u64, base: u64) ?*OpenPack {
        for (self.rows) |*r| {
            if (!r.used) continue;
            const p = r.pack.load(.acquire) orelse continue;
            if (p.vol_tag == vol_tag and p.base == base) return if (p.isSealed()) null else p;
        }
        return null;
    }

    fn countRelease(_: *Packer, v: ReleaseVerdict) void {
        switch (v) {
            .nonterminal => stats.pack_partial_frees.inc(),
            .terminal => stats.pack_terminal_frees.inc(),
            .untracked => stats.pack_release_untracked_noops.fire("tenant release: the block is untracked"),
        }
    }

    // ---- §4.6 reads ----

    /// ONE ranged window read on the mapping's own volume; `out` holds
    /// ≥ the window (`ceil(len)`); answers the caller's bytes.
    pub fn readTenant(self: *Packer, m: Mapping, rel: ?layout.Range, out: []u8) (ReadError || layout.MappingError)![]u8 {
        const w = try m.readWindow(self.chunk, rel);
        assert(out.len >= w.len);
        try self.read.read_range(self.read.ctx, w.vol_tag, w.device_off, m.incarnation, out[0..@intCast(w.len)]);
        stats.packed_reads.inc();
        stats.packed_read_bytes.add(w.len);
        return out[w.lo..w.hi];
    }

    /// The binding-validation law (charter §6.3): bytes for mapping `m`
    /// serve only if, AFTER the read, the CURRENT identity still binds the
    /// same kind, file id and mapping. Movement ⇒ re-resolve through
    /// `lookup` (counted), ≤ `BINDING_RETRIES` then loud `InputOutput` —
    /// never a silent zeros seed. A stale-incarnation refusal on a mapping
    /// the identity still binds is the finding-51 contradiction: EIO + the
    /// tripwire, never zeros, never the reissued offset's bytes.
    pub const BINDING_RETRIES: u32 = 64;

    pub fn readBound(self: *Packer, lookup: LookupSeam, ino: u64, rel: ?layout.Range, out: []u8) (ReadError || layout.MappingError || error{ IdentityUnbound, IdentityMoved })![]u8 {
        var attempts: u32 = 0;
        while (true) {
            const before = lookup.current(lookup.ctx, ino) orelse return error.IdentityUnbound;
            const m = before.durableCopy() orelse return error.IdentityUnbound;
            const bytes = self.readTenant(m, rel, out) catch |e| switch (e) {
                error.StaleIncarnation => {
                    if (stillBound(lookup, ino, before, m)) {
                        stats.read_stale_bound_mapping.fire("stale incarnation on a still-bound packed mapping");
                        return error.InputOutput;
                    }
                    try self.noteMoved(&attempts);
                    continue;
                },
                else => return e,
            };
            if (stillBound(lookup, ino, before, m)) return bytes;
            try self.noteMoved(&attempts);
        }
    }

    fn noteMoved(_: *Packer, attempts: *u32) error{IdentityMoved}!void {
        stats.staged_identity_retries.inc();
        attempts.* += 1;
        if (attempts.* >= BINDING_RETRIES) {
            log.err("packed read: the layout identity moved {d} times under the binding-validated read — refusing EIO rather than seeding zeros", .{attempts.*});
            return error.IdentityMoved;
        }
    }

    fn stillBound(lookup: LookupSeam, ino: u64, before: layout.Layout, m: Mapping) bool {
        const now = lookup.current(lookup.ctx, ino) orelse return false;
        if (now.kind != before.kind or !std.mem.eql(u8, &now.file_id, &before.file_id)) return false;
        const cur = now.durableCopy() orelse return false;
        return cur.eql(m);
    }

    // ---- the compaction's faces (`defrag --pack`, charter §6.1) ----

    /// A prepared window the COMPACTOR owns (no ring entry, no layout flip
    /// of its own): the in-flight reservation becomes a committed slot
    /// whose references are its referencers' records (`pack_compact`
    /// raises one per referencer and republishes each). The pin arithmetic
    /// the seal checks (`refcount ≡ committed + inflight + 1`) is kept by
    /// `tenantRaised` / `tenantReleased` around every raise and release.
    pub fn landWindow(_: *Packer, tenant: Tenant) void {
        tenant.pack.landed();
    }

    /// A referencer's reference raised on a landed window: `raise` is the
    /// caller's `incrementRefcount`, run under the pin lock while the pack
    /// is open so the seal's snapshot never splits the pair.
    pub fn tenantRaised(_: *Packer, pack: *OpenPack, ctx: *anyopaque, raise: *const fn (ctx: *anyopaque) bool) bool {
        if (pack.lockOpen()) {
            defer pack.pin_lock.unlock();
            if (!raise(ctx)) return false;
            _ = pack.committed.fetchAdd(1, .acq_rel);
            return true;
        }
        return raise(ctx);
    }

    /// A referencer refused its republish: its reference leaves the window
    /// (`release` is the caller's `releaseRef`, paired the same way).
    pub fn tenantReleased(_: *Packer, pack: *OpenPack, ctx: *anyopaque, release: *const fn (ctx: *anyopaque) void) void {
        if (pack.lockOpen()) {
            defer pack.pin_lock.unlock();
            _ = pack.committed.fetchSub(1, .acq_rel);
            release(ctx);
            return;
        }
        release(ctx);
    }

    /// A landed window nobody took (every referencer refused): the slot's
    /// own reference leaves — `committed − 1` and the pin released
    /// (terminal when the pack sealed and every sibling left).
    pub fn abandonLandedWindow(self: *Packer, tenant: Tenant) void {
        stats.pack_slots_abandoned.inc();
        const pack = tenant.pack;
        if (pack.lockOpen()) {
            defer pack.pin_lock.unlock();
            _ = pack.committed.fetchSub(1, .acq_rel);
            self.countRelease(self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base));
            return;
        }
        _ = pack.committed.fetchSub(1, .acq_rel);
        self.countRelease(self.alloc.release(self.alloc.ctx, pack.vol_tag, pack.base));
    }

    /// Every open pack's block and the room left in it (the compaction's
    /// plan excludes the open packs and budgets the open room).
    pub fn openBlocks(self: *Packer, gpa: Allocator, out: *std.ArrayList(Placed)) Allocator.Error!u64 {
        var room: u64 = 0;
        for (self.rows) |*r| {
            if (!r.used) continue;
            const p = r.pack.load(.acquire) orelse continue;
            if (p.isSealed()) continue;
            try out.append(gpa, .{ .vol_tag = p.vol_tag, .offset = p.base, .incarnation = p.incarnation });
            room += self.chunk -| p.reservedBytes();
        }
        return room;
    }

    // ---- gauges ----

    pub const OpenCensus = struct { blocks: u64, max_age_ms: u64, occupancy_permille_sum: u64 };

    /// The idle-tail instrument: open blocks, the oldest's age, Σ occupancy.
    pub fn census(self: *Packer, now_ns: u64) OpenCensus {
        var c: OpenCensus = .{ .blocks = 0, .max_age_ms = 0, .occupancy_permille_sum = 0 };
        for (self.rows) |*r| {
            if (!r.used) continue;
            const p = r.pack.load(.acquire) orelse continue;
            if (p.isSealed()) continue;
            c.blocks += 1;
            c.max_age_ms = @max(c.max_age_ms, p.ageMs(now_ns));
            c.occupancy_permille_sum += p.occupancyPermille();
        }
        return c;
    }
};

// ---------------------------------------------------------------------------
// §4.7 Tenant ops (pure).
// ---------------------------------------------------------------------------

/// Passthrough truncate-shrink: the pure mapping clip — zero device writes,
/// the same reference (population and RAM refcount unchanged; the old
/// window is NOT freed).
pub fn clipTenant(m: Mapping, new_len: u32) Mapping {
    stats.pack_mapping_clips.inc();
    return m.clip(new_len);
}

/// A clone of a promoted tenant SHARES the window verbatim (the dest's
/// publish adds its own +ref under the dest ino): the legal identical-window
/// class; composed with a clip ⇒ nested same-`off` windows, the second legal
/// class.
pub fn shareTenant(m: Mapping) Mapping {
    return m;
}

/// Do two tenant windows of one block constitute a C12 OVERLAP finding? Two
/// live mappings on one block with DIFFERENT `rel_off` whose windows
/// intersect — the only shape a packer bug or a torn recovery produces (a
/// `fetch_add` mints no two windows sharing a start; clone/clip never move
/// a start). Same-`off` windows of any lengths are the two legal classes.
pub fn windowsOverlap(a: Mapping, b: Mapping) bool {
    if (a.vol_tag != b.vol_tag or a.offset != b.offset or a.whole_block or b.whole_block) return false;
    if (a.rel_off == b.rel_off) return false;
    const a_end = @as(u64, a.rel_off) + layout.ceilGrain(a.len);
    const b_end = @as(u64, b.rel_off) + layout.ceilGrain(b.len);
    return a.rel_off < b_end and b.rel_off < a_end;
}

// ---------------------------------------------------------------------------
// MODELS (charter §9): the pin arithmetic and the single sealer.
// ---------------------------------------------------------------------------

/// `refcount = committed + inflight + 1` while open. Threads: a deleter
/// releases the one committed tenant's reference, the sealer releases the
/// pin, a reserver pins acquire-from-nonzero. Invariant: a reservation never
/// lands on a FREED block. Weakening: a plain increment (no nonzero check)
/// lets a terminal pin release race the reservation.
pub const PinModel = struct {
    pub const thread_count: usize = 3;
    pub const Weakening = enum { none, pin_without_nonzero_check };
    pub var weakening: Weakening = .none;

    /// 1 committed tenant + the pin.
    refcount: u32 = 2,
    freed: bool = false,
    reserved: bool = false,
    pc: [3]u8 = @splat(0),

    pub fn init() PinModel {
        return .{};
    }

    pub fn done(self: *const PinModel, t: usize) bool {
        return self.pc[t] == 1;
    }

    fn release(self: *PinModel) void {
        // One fetchSub; the last reference frees.
        self.refcount -= 1;
        if (self.refcount == 0) self.freed = true;
    }

    pub fn step(self: *PinModel, t: usize) void {
        switch (t) {
            0 => self.release(),
            1 => self.release(),
            2 => switch (weakening) {
                // One increment-if-nonzero CAS.
                .none => if (self.refcount != 0) {
                    self.refcount += 1;
                    self.reserved = true;
                },
                .pin_without_nonzero_check => {
                    self.refcount += 1;
                    self.reserved = true;
                },
            },
            else => unreachable,
        }
        self.pc[t] += 1;
    }

    pub fn invariant(self: *const PinModel) !void {
        if (self.reserved and self.freed) return error.ReservationOnFreedBlock;
    }

    pub fn final(_: *const PinModel) !void {}
};

/// Two overflowers each run `if trySeal() { release the pin }`. The CAS
/// admits exactly one; a load-then-store seal admits both and the pin is
/// released twice.
pub const SealModel = struct {
    pub const thread_count: usize = 2;
    pub const Weakening = enum { none, seal_load_then_store };
    pub var weakening: Weakening = .none;

    sealed: bool = false,
    pin_releases: u32 = 0,
    saw_unsealed: [2]bool = @splat(false),
    pc: [2]u8 = @splat(0),

    pub fn init() SealModel {
        return .{};
    }

    pub fn done(self: *const SealModel, t: usize) bool {
        return switch (weakening) {
            .none => self.pc[t] == 1,
            .seal_load_then_store => self.pc[t] == 2,
        };
    }

    pub fn step(self: *SealModel, t: usize) void {
        switch (weakening) {
            .none => {
                // ONE CAS false→true + the release the winner runs.
                if (!self.sealed) {
                    self.sealed = true;
                    self.pin_releases += 1;
                }
            },
            .seal_load_then_store => switch (self.pc[t]) {
                0 => self.saw_unsealed[t] = !self.sealed,
                1 => if (self.saw_unsealed[t]) {
                    self.sealed = true;
                    self.pin_releases += 1;
                },
                else => unreachable,
            },
        }
        self.pc[t] += 1;
    }

    pub fn invariant(self: *const SealModel) !void {
        if (self.pin_releases > 1) return error.PinReleasedTwice;
    }

    pub fn final(self: *const SealModel) !void {
        if (!self.sealed or self.pin_releases != 1) return error.NoSingleSealer;
    }
};

/// The seal's CENSUS against a tenant release. One committed tenant (the
/// pin + 1 = refcount 2); the sealer takes its census `{refcount, released}`
/// and raises `sealed`; the releaser reads `sealed` unlocked (`openPackAt`),
/// then decides under the pin lock (`lockOpen`): an OPEN pack counts the
/// release (`released + 1`, the refcount down, one locked step), a SEALED
/// one runs the plain ladder outside the lock (the refcount down alone).
/// Invariant: the census satisfies `refcount + released = committed +
/// inflight + 1`. Weakening (the shipped order): the flag goes up BEFORE the
/// census's critical section — a releaser that sees it runs the ladder, and
/// the census then reads a refcount the ladder already lowered (LTP fs,
/// 2026-10-22: `pack_pin_refcount_drift` once in 72 tests).
pub const SealCensusWeakening = enum { none, flag_before_census };

pub fn SealCensusModel(comptime weakening: SealCensusWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const S = 0;
        const R = 1;

        refcount: u8 = 2,
        released: u8 = 0,
        sealed: bool = false,
        census_taken: bool = false,
        census_refcount: u8 = 0,
        census_released: u8 = 0,
        /// The releaser's unlocked read of `sealed`.
        r_saw_sealed: bool = false,
        ladder_releases: u8 = 0,
        s_pc: u8 = 0,
        r_pc: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                S => self.s_pc == switch (weakening) {
                    .none => 2, // lock{census, flag}; the pin's release
                    .flag_before_census => 3, // flag; lock{census}; the pin's release
                },
                R => self.r_pc == 3, // the unlocked read, the locked decision, the ladder (the decision ends at 3 when it counted the release)
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                S => self.stepSealer(),
                R => self.stepReleaser(),
                else => unreachable,
            }
        }

        fn census(self: *Self) void {
            self.census_refcount = self.refcount;
            self.census_released = self.released;
            self.census_taken = true;
        }

        fn stepSealer(self: *Self) void {
            switch (weakening) {
                .none => switch (self.s_pc) {
                    // lock{ census; sealed }: the census precedes the flag.
                    0 => {
                        self.census();
                        self.sealed = true;
                    },
                    // The pin's release, outside the lock.
                    1 => self.refcount -= 1,
                    else => unreachable,
                },
                .flag_before_census => switch (self.s_pc) {
                    // The CAS outside the lock, then lock{ census }, then
                    // the pin's release.
                    0 => self.sealed = true,
                    1 => self.census(),
                    2 => self.refcount -= 1,
                    else => unreachable,
                },
            }
            self.s_pc += 1;
        }

        fn stepReleaser(self: *Self) void {
            switch (self.r_pc) {
                // `openPackAt`: the unlocked read.
                0 => {
                    self.r_saw_sealed = self.sealed;
                    self.r_pc = if (self.r_saw_sealed) 2 else 1;
                },
                // `lockOpen`: lock{ sealed? → the ladder : released + 1,
                // refcount − 1 }.
                1 => {
                    if (self.sealed) {
                        self.r_pc = 2;
                        return;
                    }
                    self.released += 1;
                    self.refcount -= 1;
                    self.r_pc = 3;
                },
                // The plain ladder, outside the lock.
                2 => {
                    self.refcount -= 1;
                    self.ladder_releases += 1;
                    self.r_pc = 3;
                },
                else => unreachable,
            }
        }

        pub fn invariant(self: *const Self) !void {
            // committed 1 + inflight 0 + the pin.
            if (self.census_taken and @as(u32, self.census_refcount) + self.census_released != 2) return error.CensusDrift;
        }

        pub fn final(self: *const Self) !void {
            if (!self.sealed or !self.census_taken) return error.NoSeal;
            if (self.refcount != 0) return error.PinOrTenantLeaked;
            if (@as(u32, self.released) + self.ladder_releases != 1) return error.TenantReleasedTwice;
        }
    };
}

// ---------------------------------------------------------------------------
// Tests — the law, the core, the crash contract over RAM seams.
// ---------------------------------------------------------------------------

const testing = std.testing;
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const derive = @import("../core/derive.zig");

const CHUNK: u64 = layout.DEFAULT_CHUNK;

test "the two-faced law: slot = ceil_4k (1 → 4096, 4096 → 4096, 4097 → 8192); pack_max_slot derives CHUNK/2 = 2 MiB; the victim line is the same number; the registry row's range is GRAIN..=CHUNK; 96,000 × 16 KiB = exactly 375 blocks" {
    try testing.expectEqual(@as(u64, 4096), packSlotLen(1));
    try testing.expectEqual(@as(u64, 4096), packSlotLen(4096));
    try testing.expectEqual(@as(u64, 8192), packSlotLen(4097));
    try testing.expectEqual(@as(u64, 2 * derive.MiB), packMaxSlot(null, CHUNK));
    try testing.expectEqual(@as(u64, 1 << 20), packMaxSlot(1 << 20, CHUNK));
    const max = packMaxSlot(null, CHUNK);
    try testing.expect(isPackVictim(max, max));
    try testing.expect(!isPackVictim(max + 1, max));
    // The break-even: packing wins iff CHUNK − s ≥ s.
    try testing.expect(CHUNK - max >= max);
    try testing.expect(CHUNK - (max + 4096) < max + 4096);
    try testing.expectEqual(@as(i128, layout.GRAIN), knobs.row("SQUEEZEFS_PACK_MAX_SLOT_BYTES").kind.int.lo);
    try testing.expectEqual(@as(i128, CHUNK), knobs.row("SQUEEZEFS_PACK_MAX_SLOT_BYTES").kind.int.hi);
    try testing.expect(knobs.boolDefault("SQUEEZEFS_SMALL_FILE_PACKING"));
    try testing.expectEqual(@as(u64, 375), packedBlocks(96_000, 16 * derive.KiB, CHUNK));
    try testing.expectEqual(@as(u64, 79), packedBlocks(20_000, 16 * derive.KiB, CHUNK));
    // The dispatch: inline by LOGICAL size; packed by the STORED slot; own
    // block above the line (counted); an image above CHUNK is the FIND-RW4-A
    // breach.
    const p = Policy.resolve(true, null, CHUNK);
    try testing.expectEqual(max, stats.pack_max_slot_bytes.load());
    try testing.expectEqual(Dispatch.inline_, try dispatch(p, 4096, 4096, 4096, CHUNK));
    try testing.expectEqual(Dispatch.packed_, try dispatch(p, 4097, 4097, 4096, CHUNK));
    try testing.expectEqual(Dispatch.packed_, try dispatch(p, 3 * derive.MiB, max, 4096, CHUNK));
    const own0 = stats.pack_own_block_promotions.load();
    try testing.expectEqual(Dispatch.own_block, try dispatch(p, 3 * derive.MiB, max + 1, 4096, CHUNK));
    try testing.expectEqual(own0 + 1, stats.pack_own_block_promotions.load());
    try testing.expectError(error.ImageExceedsChunk, dispatch(p, 5 * derive.MiB, CHUNK + 1, 4096, CHUNK));
    const off = Policy.resolve(false, null, CHUNK);
    try testing.expectEqual(Dispatch.own_block, try dispatch(off, 8192, 8192, 4096, CHUNK));
    try testing.expectEqual(@as(u64, (7 << 32) | 3), packScopeKey(7, 3));
    try testing.expectEqual(@as(u64, 0), packScopeKey(0, 0));
}

test "the open pack: reservations are one fetch_add and every reservation after the overflowing one overflows; exactly one sealer; occupancy and age" {
    var p: OpenPack = .{ .vol_tag = 1, .scope = 0, .base = 0, .incarnation = 0, .chunk = CHUNK, .seq = 1, .opened_at_ns = 1_000_000 };
    try testing.expectEqual(@as(?u64, 0), p.tryReserve(2 * derive.MiB));
    try testing.expectEqual(@as(?u64, 2 * derive.MiB), p.tryReserve(2 * derive.MiB));
    try testing.expectEqual(@as(?u64, null), p.tryReserve(4096));
    try testing.expectEqual(@as(?u64, null), p.tryReserve(4096));
    try testing.expectEqual(@as(u64, 1000), p.occupancyPermille());
    try testing.expectEqual(CHUNK, p.reservedBytes());
    try testing.expect(p.trySeal());
    try testing.expect(!p.trySeal());
    try testing.expect(p.isSealed());
    try testing.expectEqual(@as(u64, 2), p.ageMs(3_000_000));
    var q: OpenPack = .{ .vol_tag = 1, .scope = 0, .base = 0, .incarnation = 0, .chunk = CHUNK, .seq = 2, .opened_at_ns = 0 };
    _ = q.tryReserve(1 * derive.MiB);
    try testing.expectEqual(@as(u64, 250), q.occupancyPermille());
}

/// A RAM world binding every seam: an allocator with refcounts, a device of
/// blocks, a KV of records (the layout xattr + the refs), the staging ring.
const World = struct {
    gpa: Allocator,
    /// `(vol_tag, offset)` → RAM refcount (0 = free / untracked).
    refcounts: std.AutoHashMapUnmanaged([2]u64, u32) = .empty,
    published: std.AutoHashMapUnmanaged([2]u64, void) = .empty,
    next_block: u64 = 0,
    allocs: u32 = 0,
    abandons: u32 = 0,
    storage_full: bool = false,
    /// Device bytes by `(vol_tag, offset)` of a GRAIN.
    device: std.AutoHashMapUnmanaged([2]u64, [layout.GRAIN]u8) = .empty,
    dma_fail_next: bool = false,
    dmas: u32 = 0,
    stale_reads: u32 = 0,
    /// The durable KV: raw key → value.
    kv: std.StringHashMapUnmanaged([]u8) = .empty,
    commit_fail_next: bool = false,
    commits: u32 = 0,
    geo: Geometry = .default,
    /// The test's pin: runs inside the seal's census, at its refcount read
    /// (a tenant release arriving mid-census).
    on_refcount: ?struct { ctx: *anyopaque, call: *const fn (*anyopaque) void } = null,

    fn deinit(self: *World) void {
        self.refcounts.deinit(self.gpa);
        self.published.deinit(self.gpa);
        self.device.deinit(self.gpa);
        var it = self.kv.iterator();
        while (it.next()) |e| {
            self.gpa.free(e.key_ptr.*);
            self.gpa.free(e.value_ptr.*);
        }
        self.kv.deinit(self.gpa);
    }

    fn allocSeam(self: *World) AllocSeam {
        return .{ .ctx = self, .allocate_placed = allocatePlaced, .pin = pin, .release = release, .publish = publish, .abandon = abandon, .refcount = refcount };
    }

    fn allocatePlaced(ctx: *anyopaque, k: PackKey) AllocError!Mint {
        const self: *World = @ptrCast(@alignCast(ctx));
        if (self.storage_full) return error.StorageFull;
        const off = self.next_block * CHUNK;
        self.next_block += 1;
        self.allocs += 1;
        try self.refcounts.put(self.gpa, .{ k.vol_tag, off }, 1);
        return .{ .offset = off, .incarnation = 5 };
    }

    fn pin(ctx: *anyopaque, vol_tag: u64, offset: u64) bool {
        const self: *World = @ptrCast(@alignCast(ctx));
        const p = self.refcounts.getPtr(.{ vol_tag, offset }) orelse return false;
        if (p.* == 0) return false;
        p.* += 1;
        return true;
    }

    fn release(ctx: *anyopaque, vol_tag: u64, offset: u64) ReleaseVerdict {
        const self: *World = @ptrCast(@alignCast(ctx));
        const p = self.refcounts.getPtr(.{ vol_tag, offset }) orelse return .untracked;
        if (p.* == 0) return .untracked;
        p.* -= 1;
        return if (p.* == 0) .terminal else .nonterminal;
    }

    fn publish(ctx: *anyopaque, vol_tag: u64, offset: u64) void {
        const self: *World = @ptrCast(@alignCast(ctx));
        self.published.put(self.gpa, .{ vol_tag, offset }, {}) catch unreachable;
    }

    fn abandon(ctx: *anyopaque, vol_tag: u64, offset: u64) void {
        const self: *World = @ptrCast(@alignCast(ctx));
        self.abandons += 1;
        _ = self.refcounts.remove(.{ vol_tag, offset });
    }

    fn refcount(ctx: *anyopaque, vol_tag: u64, offset: u64) u32 {
        const self: *World = @ptrCast(@alignCast(ctx));
        if (self.on_refcount) |h| h.call(h.ctx);
        return self.refcounts.get(.{ vol_tag, offset }) orelse 0;
    }

    fn dmaSeam(self: *World) DmaSeam {
        return .{ .ctx = self, .write_fn = dmaWrite };
    }

    fn dmaWrite(ctx: *anyopaque, vol_tag: u64, offset: u64, data: []const u8) layout.DmaError!void {
        const self: *World = @ptrCast(@alignCast(ctx));
        if (self.dma_fail_next) {
            self.dma_fail_next = false;
            return error.InputOutput;
        }
        self.dmas += 1;
        var pos: usize = 0;
        var off = offset;
        while (pos < data.len) : (off += layout.GRAIN) {
            var grain: [layout.GRAIN]u8 = @splat(0);
            const n = @min(layout.GRAIN, data.len - pos);
            @memcpy(grain[0..n], data[pos..][0..n]);
            try self.device.put(self.gpa, .{ vol_tag, off }, grain);
            pos += n;
        }
    }

    fn readSeam(self: *World) ReadSeam {
        return .{ .ctx = self, .read_range = readRange };
    }

    fn readRange(ctx: *anyopaque, vol_tag: u64, device_off: u64, incarnation: u64, out: []u8) ReadError!void {
        const self: *World = @ptrCast(@alignCast(ctx));
        if (self.stale_reads > 0) {
            self.stale_reads -= 1;
            return error.StaleIncarnation;
        }
        if (incarnation != 5) return error.StaleIncarnation;
        var pos: usize = 0;
        var off = device_off;
        while (pos < out.len) : (off += layout.GRAIN) {
            const grain = self.device.get(.{ vol_tag, off }) orelse return error.InputOutput;
            const n = @min(layout.GRAIN, out.len - pos);
            @memcpy(out[pos..][0..n], grain[0..n]);
            pos += n;
        }
    }

    fn refsSeam(self: *World) RefsSeam {
        return .{ .ctx = self, .stage_add = stageAdd, .stage_release = stageRelease };
    }

    /// What `data/block_refs.zig` binds: the ref record's Put / Delete in
    /// the SAME tx.
    fn stageAdd(_: *anyopaque, t: *tx.KvTx, op: layout.RefOp) Allocator.Error!void {
        try t.stagePut(key.TREE_BLOCK_REFS, &key.blockRefKey(op.vol_tag, op.block_idx, op.owner_ino, op.block_index), &(record.BlockRefValue{ .map_blob = false, .shared = false }).encode());
    }

    fn stageRelease(_: *anyopaque, t: *tx.KvTx, op: layout.RefOp) Allocator.Error!void {
        try t.stageDelete(key.TREE_BLOCK_REFS, &key.blockRefKey(op.vol_tag, op.block_idx, op.owner_ino, op.block_index));
    }

    fn commitSeam(self: *World) CommitSeam {
        return .{ .ctx = self, .commit = commitTx };
    }

    /// Apply the tx's records to the RAM KV (the conveyor's effect); the
    /// tx is CONSUMED on every return (`CommitSeam`'s contract — the
    /// backend's conveyor does the same).
    fn commitTx(ctx: *anyopaque, t: *tx.KvTx) CommitError!void {
        const self: *World = @ptrCast(@alignCast(ctx));
        defer t.deinit();
        if (self.commit_fail_next) {
            self.commit_fail_next = false;
            return error.CommitFailed;
        }
        for (t.staged.items) |s| {
            if (self.kv.fetchRemove(s.key)) |old| {
                self.gpa.free(old.key);
                self.gpa.free(old.value);
            }
            if (s.kind == .delete) continue;
            const k = try self.gpa.dupe(u8, s.key);
            errdefer self.gpa.free(k);
            const v = try self.gpa.dupe(u8, s.value);
            errdefer self.gpa.free(v);
            try self.kv.put(self.gpa, k, v);
        }
        self.commits += 1;
    }

    fn lookupSeam(self: *World) LookupSeam {
        return .{ .ctx = self, .current = current };
    }

    fn layoutKey(ino: u64) [key.XATTR_KEY_LEN]u8 {
        return key.xattrKey(ino, key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, &seed_key), 0);
    }

    fn current(ctx: *anyopaque, ino: u64) ?layout.Layout {
        const self: *World = @ptrCast(@alignCast(ctx));
        const v = self.kv.get(&layoutKey(ino)) orelse return null;
        return layout.XattrLayout.decode(v, self.geo) catch null;
    }

    /// Install a layout directly (the write path's staged publish).
    fn setLayout(self: *World, ino: u64, l: layout.Layout) !void {
        var t = tx.KvTx.init(self.gpa, @src());
        const buf = self.gpa.alloc(u8, (layout.XattrLayout{ .layout = l }).encodedLen()) catch |e| {
            t.deinit();
            return e;
        };
        defer self.gpa.free(buf);
        t.stagePut(key.TREE_XATTRS, &layoutKey(ino), (layout.XattrLayout{ .layout = l }).encodeInto(buf)) catch |e| {
            t.deinit();
            return e;
        };
        try commitTx(self, &t);
    }

    /// The crash oracle: the refs naming `(vol_tag, offset)` = the recovered
    /// refcount.
    fn durableRefcount(self: *World, vol_tag: u64, offset: u64) u32 {
        const range = key.blockRange(vol_tag, offset / CHUNK);
        var n: u32 = 0;
        var it = self.kv.keyIterator();
        while (it.next()) |k| {
            if (key.order(range.lo.slice(), k.*) != .gt and key.order(k.*, range.hi.slice()) == .lt) n += 1;
        }
        return n;
    }
};

const SEED: u64 = 0x5EED;
const seed_key: key.NameKey = .init(SEED);

/// A `len`-byte payload of one byte (the `"x" ** n` the 0.17 grammar lost).
fn fill(comptime len: usize, b: u8) [len]u8 {
    return @splat(b);
}

const Fixture = struct {
    world: World,
    packer: Packer,
    locks: *layout.PublishLocks,
    pool: staging.Pool,
    image: []u8,
    seg: staging.Segment,

    fn init(gpa: Allocator) !*Fixture {
        const f = try gpa.create(Fixture);
        errdefer gpa.destroy(f);
        f.world = .{ .gpa = gpa };
        f.packer = try Packer.init(gpa, CHUNK, Policy.resolve(true, null, CHUNK), f.world.allocSeam(), f.world.dmaSeam(), f.world.readSeam(), 8);
        f.locks = try gpa.create(layout.PublishLocks);
        f.locks.* = .{};
        f.pool = staging.Pool.init(gpa, .none);
        f.image = try gpa.alloc(u8, 8 * derive.MiB);
        @memset(f.image, 0);
        f.seg = try staging.Segment.init(gpa, f.image, .none);
        return f;
    }

    fn deinit(self: *Fixture) void {
        const gpa = self.world.gpa;
        self.seg.deinit();
        gpa.free(self.image);
        self.pool.deinit();
        gpa.destroy(self.locks);
        self.packer.deinit();
        self.world.deinit();
        gpa.destroy(self);
    }

    fn fid(seed: u8) layout.FileId {
        var f: layout.FileId = undefined;
        for (&f, 0..) |*b, i| b.* = seed +% @as(u8, @intCast(i * 5));
        return f;
    }

    /// Stage a file: the ring entry + the staged layout record.
    fn stage(self: *Fixture, ino: u64, seed: u8, payload: []const u8) !staging.Staged {
        const s = try staging.stageWrite(&self.pool, &self.seg, null, staging.StagingKey.init(fid(seed), ino), payload, 0, .none);
        try self.world.setLayout(ino, layout.Layout.staged(payload.len, fid(seed), null));
        return s;
    }

    fn ctx(self: *Fixture, ino: u64, seed: u8, generation: u64) CommitCtx {
        return .{ .ino = ino, .name_key = &seed_key, .file_id = fid(seed), .generation = generation, .lookup = self.world.lookupSeam(), .refs = self.world.refsSeam(), .commit = self.world.commitSeam(), .pool = &self.pool, .seg = &self.seg, .geo = self.world.geo };
    }

    /// The plane's commit section: `commitLocked` under (3.5) then (3.7),
    /// the displaced copy released after both drop.
    fn commit(self: *Fixture, io: Io, prepared: Prepared, c: CommitCtx, t: *tx.KvTx) CommitOutcome {
        const g = self.locks.lockPromotion(io, c.ino);
        const out = self.packer.commitLocked(prepared, c, t);
        g.release(io);
        if (out == .committed) if (out.committed) |d| self.packer.releaseTenantRef(d);
        return out;
    }

    /// The plane's arm over the outcome: the tx is abandoned only where the
    /// outcome says the caller still owns it (`stale`, a failed publish);
    /// the committed and commit-failed arms were consumed by the seam.
    fn settleTx(out: CommitOutcome, t: *tx.KvTx) void {
        switch (out) {
            .committed => {},
            .stale => t.deinit(),
            .failed => |why| if (why == .publish) t.deinit(),
        }
    }

    /// The ONE primitive: read the ring image, prepare, commit.
    fn promote(self: *Fixture, io: Io, ino: u64, seed: u8, generation: u64) !CommitOutcome {
        const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
        const lease = self.seg.get(staging.StagingKey.init(fid(seed), ino)) orelse return error.NotResident;
        defer lease.release();
        const prepared = (try self.packer.prepare(io, k, lease.bytes(), 1)) orelse return error.ArmStopped;
        var t = tx.KvTx.init(self.world.gpa, @src());
        const out = self.commit(io, prepared, self.ctx(ino, seed, generation), &t);
        settleTx(out, &t);
        return out;
    }
};

test "prepare → commit: ONE DMA of the slot before the type flip, the packed layout + its ref in ONE tx, the ring entry released inside the guard; N tenants share one block at refcount N + 1 while open; the tenant reads back byte-exact through the window" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const pa = fill(5000, 'a');
    const pb = fill(200, 'b');
    const pc = fill(12_288, 'c');
    const payloads = [_][]const u8{ &pa, &pb, &pc };
    var gens: [3]u64 = undefined;
    for (payloads, 0..) |p, i| gens[i] = (try f.stage(@intCast(10 + i), @intCast(i), p)).generation;
    const before_dmas = f.world.dmas;
    const packed0 = stats.layout_promoted_packed.load();
    for (payloads, 0..) |_, i| try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, @intCast(10 + i), @intCast(i), gens[i])));
    try testing.expectEqual(before_dmas + 3, f.world.dmas);
    try testing.expectEqual(packed0 + 3, stats.layout_promoted_packed.load());
    // One block opened; the pin + 3 committed = 4; every tenant's ref is durable.
    try testing.expectEqual(@as(u32, 1), f.world.allocs);
    try testing.expectEqual(@as(u32, 4), World.refcount(&f.world, 0x11, 0));
    try testing.expectEqual(@as(u32, 3), f.world.durableRefcount(0x11, 0));
    try testing.expect(f.world.published.contains(.{ 0x11, 0 }));
    const pack = f.packer.openPack(.{ .vol_tag = 0x11, .scope = 0 }).?;
    try testing.expectEqual(@as(u32, 3), pack.committed.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), pack.inflight.load(.monotonic));
    try testing.expectEqual(@as(u64, 8192 + 4096 + 12_288), pack.reservedBytes());
    // The layouts: packed, tenant windows at consecutive slots, sizes kept.
    const l0 = World.current(&f.world, 10).?;
    try testing.expectEqual(layout.Kind.packed_, l0.kind);
    try testing.expectEqual(@as(u64, 5000), l0.size);
    const m0 = l0.durableCopy().?;
    try testing.expectEqual(@as(u32, 0), m0.rel_off);
    try testing.expectEqual(@as(u32, 5000), m0.len);
    const m1 = World.current(&f.world, 11).?.durableCopy().?;
    try testing.expectEqual(@as(u32, 8192), m1.rel_off);
    const m2 = World.current(&f.world, 12).?.durableCopy().?;
    try testing.expectEqual(@as(u32, 12_288), m2.rel_off);
    try testing.expect(!windowsOverlap(m0, m1) and !windowsOverlap(m1, m2));
    // The ring entries left; the pool credited.
    for (0..3) |i| try testing.expect(f.seg.get(staging.StagingKey.init(Fixture.fid(@intCast(i)), @intCast(10 + i))) == null);
    try testing.expectEqual(@as(u64, 0), f.pool.currentBytes());
    // Reads: the tenant's own window, not the block prefix; the pad reads zero.
    var out: [16_384]u8 = undefined;
    const r1 = try f.packer.readTenant(m1, null, &out);
    try testing.expectEqualSlices(u8, payloads[1], r1);
    const sub = try f.packer.readTenant(m0, .{ .start = 4090, .end = 4100 }, &out);
    try testing.expectEqualSlices(u8, &fill(10, 'a'), sub);
    const bound = try f.packer.readBound(f.world.lookupSeam(), 12, null, &out);
    try testing.expectEqualSlices(u8, payloads[2], bound);
    try testing.expect(stats.packed_reads.load() >= 3);
    // The gauges.
    const c = f.packer.census(2_000_000_000);
    try testing.expectEqual(@as(u64, 1), c.blocks);
    try testing.expect(c.occupancy_permille_sum > 0);
    try testing.expectEqual(@as(u64, 1), stats.pack_open_scopes.load());
}

test "commitLocked re-checks the identity under the caller's stripe + layout lock: a re-stage (new generation), a different file id or a non-staged layout ⇒ stale, the slot abandoned, nothing published; a failed KV commit retracts the refs and abandons the slot" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
    const g1 = (try f.stage(20, 1, &fill(100, 'x'))).generation;
    // Prepare against g1, then the writer re-stages (g2) before the commit.
    const lease = f.seg.get(staging.StagingKey.init(Fixture.fid(1), 20)).?;
    const prepared = (try f.packer.prepare(io, k, lease.bytes(), 1)).?;
    lease.release();
    _ = try f.stage(20, 1, &fill(200, 'y'));
    const abandoned0 = stats.pack_slots_abandoned.load();
    var t = tx.KvTx.init(gpa, @src());
    defer t.deinit();
    try testing.expectEqual(std.meta.Tag(CommitOutcome).stale, std.meta.activeTag(f.commit(io, prepared, f.ctx(20, 1, g1), &t)));
    try testing.expectEqual(abandoned0 + 1, stats.pack_slots_abandoned.load());
    try testing.expectEqual(layout.Kind.staged, World.current(&f.world, 20).?.kind);
    try testing.expectEqual(@as(u32, 0), f.world.durableRefcount(0x11, 0));
    // The pin is intact (refcount = 0 committed + 0 inflight + 1).
    try testing.expectEqual(@as(u32, 1), World.refcount(&f.world, 0x11, 0));
    // A different file id.
    const g2 = f.pool.generationOf(Fixture.fid(1)).?;
    const lease2 = f.seg.get(staging.StagingKey.init(Fixture.fid(1), 20)).?;
    const prepared2 = (try f.packer.prepare(io, k, lease2.bytes(), 1)).?;
    lease2.release();
    var t2 = tx.KvTx.init(gpa, @src());
    defer t2.deinit();
    try testing.expectEqual(std.meta.Tag(CommitOutcome).stale, std.meta.activeTag(f.commit(io, prepared2, f.ctx(20, 9, g2), &t2)));
    // A failed KV commit: refs retracted (nothing durable), slot abandoned, the layout unchanged, the ring entry kept.
    const lease3 = f.seg.get(staging.StagingKey.init(Fixture.fid(1), 20)).?;
    const prepared3 = (try f.packer.prepare(io, k, lease3.bytes(), 1)).?;
    lease3.release();
    f.world.commit_fail_next = true;
    // The failed COMMIT consumed the tx (the seam's contract): the outcome
    // says so, and the caller abandons nothing — the dismount double-free.
    var t3 = tx.KvTx.init(gpa, @src());
    const failed = f.commit(io, prepared3, f.ctx(20, 1, g2), &t3);
    try testing.expectEqual(CommitOutcome{ .failed = .commit }, failed);
    Fixture.settleTx(failed, &t3);
    try testing.expectEqual(@as(u32, 0), f.world.durableRefcount(0x11, 0));
    try testing.expectEqual(layout.Kind.staged, World.current(&f.world, 20).?.kind);
    try testing.expect(f.seg.get(staging.StagingKey.init(Fixture.fid(1), 20)) != null);
    f.seg.get(staging.StagingKey.init(Fixture.fid(1), 20)).?.release();
    // Finally the real commit lands.
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 20, 1, g2)));
    try testing.expectEqual(@as(u32, 1), f.world.durableRefcount(0x11, 0));
    try testing.expectEqual(@as(u32, 2), World.refcount(&f.world, 0x11, 0));
}

test "a DMA error releases the tenant and errors (nothing published); a StorageFull refill stops the arm for the batch (ONE warn) and beginPromotionBatch re-arms it" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
    f.world.dma_fail_next = true;
    const abandoned0 = stats.pack_slots_abandoned.load();
    try testing.expectError(error.InputOutput, f.packer.prepare(io, k, &fill(10, 'q'), 1));
    try testing.expectEqual(abandoned0 + 1, stats.pack_slots_abandoned.load());
    try testing.expect(!f.world.published.contains(.{ 0x11, 0 }));
    const pack = f.packer.openPack(k).?;
    try testing.expectEqual(@as(u32, 0), pack.inflight.load(.monotonic));
    // The arm stop: a fresh scope with no free block.
    f.world.storage_full = true;
    const stopped0 = stats.pack_arm_stopped_promotions.load();
    const sf0 = stats.pack_refill_storage_full.load();
    const k2: PackKey = .{ .vol_tag = 0x11, .scope = 1 };
    try testing.expectEqual(@as(?Prepared, null), try f.packer.prepare(io, k2, &fill(10, 'q'), 1));
    try testing.expect(f.packer.isStopped());
    try testing.expectEqual(sf0 + 1, stats.pack_refill_storage_full.load());
    try testing.expectEqual(@as(?Prepared, null), try f.packer.prepare(io, k, &fill(10, 'q'), 1));
    try testing.expectEqual(stopped0 + 2, stats.pack_arm_stopped_promotions.load());
    f.world.storage_full = false;
    f.packer.beginPromotionBatch();
    try testing.expect((try f.packer.prepare(io, k, &fill(10, 'q'), 1)) != null);
}

test "a refill whose OpenPack allocation fails mints nothing: OutOfMemory reaches the caller, the allocator tracks no block, and a retry opens the pack" {
    const gpa = testing.allocator;
    var world: World = .{ .gpa = gpa };
    defer world.deinit();
    // Allocation 0 is the row table; 1 is the refill's OpenPack.
    var failing = std.testing.FailingAllocator.init(gpa, .{ .fail_index = 1 });
    var packer = try Packer.init(failing.allocator(), CHUNK, Policy.resolve(true, null, CHUNK), world.allocSeam(), world.dmaSeam(), world.readSeam(), 8);
    defer packer.deinit();
    const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
    try testing.expectError(error.OutOfMemory, packer.reserve(testing.io, k, layout.GRAIN));
    try testing.expectEqual(@as(u32, 0), world.refcounts.count());
    failing.fail_index = std.math.maxInt(usize);
    try testing.expect((try packer.reserve(testing.io, k, layout.GRAIN)) != null);
    try testing.expectEqual(@as(u32, 1), world.refcounts.count());
}

test "overflow seals FULL exactly once and the next reservation refills into a fresh block; dismount seals every open pack; a drain seal names its block; the seal checks the pin arithmetic and releases the pin" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
    // Fill the first block with two half-chunk tenants, then one more.
    var gens: [3]u64 = undefined;
    const half = try gpa.alloc(u8, 2 * derive.MiB);
    defer gpa.free(half);
    @memset(half, 7);
    for (0..3) |i| gens[i] = (try f.stage(@intCast(30 + i), @intCast(i), half)).generation;
    const full0 = stats.pack_blocks_sealed_full.load();
    const opened0 = stats.pack_blocks_opened.load();
    for (0..3) |i| try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, @intCast(30 + i), @intCast(i), gens[i])));
    try testing.expectEqual(full0 + 1, stats.pack_blocks_sealed_full.load());
    try testing.expectEqual(opened0 + 2, stats.pack_blocks_opened.load());
    try testing.expectEqual(@as(u32, 2), f.world.allocs);
    // Block 0: sealed, pin released ⇒ refcount = 2 committed; block 1 open: 1 + 1.
    try testing.expectEqual(@as(u32, 2), World.refcount(&f.world, 0x11, 0));
    try testing.expectEqual(@as(u32, 2), World.refcount(&f.world, 0x11, CHUNK));
    try testing.expectEqual(@as(u64, 1), stats.pack_open_blocks.load());
    const open = f.packer.openPack(k).?;
    try testing.expectEqual(CHUNK, open.base);
    // Drain seal of a block that is not open: false; of the open one: true.
    try testing.expect(!f.packer.sealBlock(0x11, 0));
    const drain0 = stats.pack_blocks_sealed_drain.load();
    try testing.expect(f.packer.sealBlock(0x11, CHUNK));
    try testing.expectEqual(drain0 + 1, stats.pack_blocks_sealed_drain.load());
    try testing.expectEqual(@as(u32, 1), World.refcount(&f.world, 0x11, CHUNK));
    try testing.expect(f.packer.openPack(k) == null);
    // A new tenant opens block 2; dismount seals it; an abandoned (empty) pack counts.
    const g = (try f.stage(40, 7, &fill(10, 'z'))).generation;
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 40, 7, g)));
    try testing.expect(f.packer.openPack(k) != null);
    const k2: PackKey = .{ .vol_tag = 0x11, .scope = 9 };
    _ = (try f.packer.reserve(io, k2, 4096)).?;
    const dis0 = stats.pack_blocks_sealed_dismount.load();
    const ab0 = stats.pack_blocks_abandoned.load();
    const drift0 = stats.pack_pin_refcount_drift.load();
    f.packer.sealAll();
    try testing.expectEqual(dis0 + 2, stats.pack_blocks_sealed_dismount.load());
    // Scope 9's reservation is still in flight: not abandoned (its own
    // release decides); block 2 has a committed tenant: not abandoned.
    try testing.expectEqual(ab0, stats.pack_blocks_abandoned.load());
    try testing.expectEqual(drift0, stats.pack_pin_refcount_drift.load());
    try testing.expectEqual(@as(u64, 0), stats.pack_open_blocks.load());
    try testing.expect(f.packer.openPack(k) == null);
    // Unlink of block 0's two tenants: the first is partial, the last terminal.
    const m30 = World.current(&f.world, 30).?.durableCopy().?;
    const m31 = World.current(&f.world, 31).?.durableCopy().?;
    const partial0 = stats.pack_partial_frees.load();
    const term0 = stats.pack_terminal_frees.load();
    f.packer.releaseTenantRef(m30);
    try testing.expectEqual(partial0 + 1, stats.pack_partial_frees.load());
    f.packer.releaseTenantRef(m31);
    try testing.expectEqual(term0 + 1, stats.pack_terminal_frees.load());
    try testing.expectEqual(@as(u32, 0), World.refcount(&f.world, 0x11, 0));
}

test "a tenant released while its pack is still OPEN (an unlink, a rewrite's displaced copy) is counted against the pin arithmetic: the seal sees refcount + released = committed + inflight + 1 — drift 0, partial free counted; the seal's pin release then lands exactly (repro-port: fsstress — every rewrite of a packed file before the FULL seal fired pack_pin_refcount_drift)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    var gens: [2]u64 = undefined;
    for (0..2) |i| gens[i] = (try f.stage(@intCast(60 + i), @intCast(i), &fill(3000, 'r'))).generation;
    for (0..2) |i| try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, @intCast(60 + i), @intCast(i), gens[i])));
    try testing.expectEqual(@as(u32, 3), World.refcount(&f.world, 0x11, 0));
    const open = f.packer.openPack(.{ .vol_tag = 0x11, .scope = 0 }).?;
    try testing.expectEqual(@as(u64, 0), open.base);
    // Tenant 60 leaves while the pack is open: the plane's `freeBindings`
    // face (an unlink's corpse, a re-stage's displaced tenant).
    const m60 = World.current(&f.world, 60).?.durableCopy().?;
    const partial0 = stats.pack_partial_frees.load();
    try testing.expect(f.packer.releaseOpenTenant(m60));
    try testing.expectEqual(partial0 + 1, stats.pack_partial_frees.load());
    try testing.expectEqual(@as(u32, 2), World.refcount(&f.world, 0x11, 0));
    try testing.expectEqual(@as(u32, 1), open.released.load(.monotonic));
    // The seal: no drift; the pin leaves; tenant 61 alone holds the block.
    const drift0 = stats.pack_pin_refcount_drift.load();
    stats.pack_pin_refcount_drift.resetLogBudgetForTest();
    try testing.expect(f.packer.sealBlock(0x11, 0));
    try testing.expectEqual(drift0, stats.pack_pin_refcount_drift.load());
    try testing.expectEqual(@as(u32, 1), World.refcount(&f.world, 0x11, 0));
    // After the seal the face declines (the ordinary ladder owns the
    // block); the plain release is terminal.
    const m61 = World.current(&f.world, 61).?.durableCopy().?;
    try testing.expect(!f.packer.releaseOpenTenant(m61));
    const term0 = stats.pack_terminal_frees.load();
    f.packer.releaseTenantRef(m61);
    try testing.expectEqual(term0 + 1, stats.pack_terminal_frees.load());
    try testing.expectEqual(@as(u32, 0), World.refcount(&f.world, 0x11, 0));
}

test "the seal's census and its flag are ONE step against a tenant release: a release that observes the pack SEALED (and so runs the plain ladder) left strictly after the census read the refcount — the census never meets the flag already up, drift 0, the tenant's reference leaves exactly once (LTP fs 2026-10-22, ftest01: a packed file re-staged while its pack sealed FULL fired pack_pin_refcount_drift once in 72 tests; the shipped seal raised the flag first and its census then read a refcount the ladder had already lowered)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const g = (try f.stage(70, 1, &fill(3000, 's'))).generation;
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 70, 1, g)));
    try testing.expectEqual(@as(u32, 2), World.refcount(&f.world, 0x11, 0));
    const open = f.packer.openPack(.{ .vol_tag = 0x11, .scope = 0 }).?;
    const m70 = World.current(&f.world, 70).?.durableCopy().?;
    // The releaser (`freeBindings` of the displaced tenant) arrives while
    // the seal's census reads the refcount: it runs the release face only
    // if the pack it sees is already SEALED — the ladder's own decision
    // (`openPackAt`); an OPEN pack would have it wait for the pin lock the
    // census holds, and then it runs after the seal.
    const Racer = struct {
        packer: *Packer,
        pack: *OpenPack,
        m: Mapping,
        ran_inside_census: bool = false,

        fn call(ctx: *anyopaque) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            if (self.ran_inside_census or !self.pack.isSealed()) return;
            self.ran_inside_census = true;
            self.packer.releaseTenantRef(self.m);
        }
    };
    var racer: Racer = .{ .packer = &f.packer, .pack = open, .m = m70 };
    f.world.on_refcount = .{ .ctx = &racer, .call = &Racer.call };
    const drift0 = stats.pack_pin_refcount_drift.load();
    stats.pack_pin_refcount_drift.resetLogBudgetForTest();
    try testing.expect(f.packer.sealBlock(0x11, 0));
    f.world.on_refcount = null;
    try testing.expectEqual(drift0, stats.pack_pin_refcount_drift.load());
    // The flag went up after the census: the racer never saw it inside.
    try testing.expect(!racer.ran_inside_census);
    // The pin left at the seal; the tenant's release after it is the
    // plain ladder, terminal — once.
    try testing.expectEqual(@as(u32, 1), World.refcount(&f.world, 0x11, 0));
    const term0 = stats.pack_terminal_frees.load();
    f.packer.releaseTenantRef(m70);
    try testing.expectEqual(term0 + 1, stats.pack_terminal_frees.load());
    try testing.expectEqual(@as(u32, 0), World.refcount(&f.world, 0x11, 0));
    try testing.expectEqual(@as(u32, 0), open.released.load(.monotonic));
}

test "model: the seal's census against a tenant release holds with the census inside the flag's critical section and fails with the flag raised first (the shipped order: the ladder lowers the refcount the census then reads)" {
    try explorer.expectHolds(SealCensusModel(.none), .{});
    try explorer.expectFails(SealCensusModel(.flag_before_census), .{}, error.InvariantViolated);
}

test "crash contract over the RAM seams: ≥ 1 committed tenant ⇒ the block recovers ALLOCATED at refcount N from the refs; a prepared-but-uncommitted tenant leaves nothing durable (the torn slot DMA is invisible); no committed tenant ⇒ FREE" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const k: PackKey = .{ .vol_tag = 0x11, .scope = 0 };
    var gens: [4]u64 = undefined;
    for (0..4) |i| gens[i] = (try f.stage(@intCast(50 + i), @intCast(i), &fill(3000, 'p'))).generation;
    // Two commit, one is prepared (DMA done) when the "crash" hits, one never started.
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 50, 0, gens[0])));
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 51, 1, gens[1])));
    const lease = f.seg.get(staging.StagingKey.init(Fixture.fid(2), 52)).?;
    _ = (try f.packer.prepare(io, k, lease.bytes(), 1)).?;
    lease.release();
    // Crash: RAM is gone. Recovery = the durable records alone.
    try testing.expectEqual(@as(u32, 2), f.world.durableRefcount(0x11, 0));
    try testing.expectEqual(layout.Kind.packed_, World.current(&f.world, 50).?.kind);
    try testing.expectEqual(layout.Kind.packed_, World.current(&f.world, 51).?.kind);
    try testing.expectEqual(layout.Kind.staged, World.current(&f.world, 52).?.kind);
    try testing.expectEqual(layout.Kind.staged, World.current(&f.world, 53).?.kind);
    // A block nothing names recovers FREE.
    const g = try Fixture.init(gpa);
    defer g.deinit();
    _ = (try g.packer.reserve(io, k, 4096)).?;
    try testing.expectEqual(@as(u32, 1), g.world.allocs);
    try testing.expectEqual(@as(u32, 0), g.world.durableRefcount(0x11, 0));
}

test "tenant ops: the clip keeps the reference and counts; a share is the mapping verbatim; the C12 overlap predicate fires only for intersecting windows at DIFFERENT rel_off" {
    const m = Mapping.tenant(0x11, 0, 5, 8192, 10_000);
    const c0 = stats.pack_mapping_clips.load();
    const clipped = clipTenant(m, 100);
    try testing.expectEqual(c0 + 1, stats.pack_mapping_clips.load());
    try testing.expect(clipped.sameRef(m, CHUNK));
    try testing.expectEqual(@as(u32, 8192), clipped.rel_off);
    try testing.expect(shareTenant(m).eql(m));
    try testing.expect(!windowsOverlap(m, clipped));
    try testing.expect(!windowsOverlap(m, m));
    try testing.expect(windowsOverlap(m, Mapping.tenant(0x11, 0, 5, 12_288, 1)));
    try testing.expect(!windowsOverlap(m, Mapping.tenant(0x11, 0, 5, 20_480, 1)));
    try testing.expect(!windowsOverlap(m, Mapping.tenant(0x11, CHUNK, 5, 8192, 1)));
    try testing.expect(!windowsOverlap(m, Mapping.wholeBlock(0x11, 0, 5)));
}

test "binding validation: a moved identity re-resolves (counted) and a 64th move is a loud EIO class; a stale incarnation on a still-bound mapping trips read_stale_bound_mapping and answers EIO, never zeros" {
    const gpa = testing.allocator;
    const io = testing.io;
    const f = try Fixture.init(gpa);
    defer f.deinit();
    const g = (try f.stage(60, 3, &fill(2000, 'r'))).generation;
    try testing.expectEqual(std.meta.Tag(CommitOutcome).committed, std.meta.activeTag(try f.promote(io, 60, 3, g)));
    var out: [8192]u8 = undefined;
    // Stale incarnation while still bound: EIO + the tripwire.
    f.world.stale_reads = 1;
    const tw0 = stats.read_stale_bound_mapping.load();
    log.testing.expectErrorLogs(1);
    stats.read_stale_bound_mapping.resetLogBudgetForTest();
    try testing.expectError(error.InputOutput, f.packer.readBound(f.world.lookupSeam(), 60, null, &out));
    try log.testing.expectAllFired();
    try testing.expectEqual(tw0 + 1, stats.read_stale_bound_mapping.load());
    // An unbound ino.
    try testing.expectError(error.IdentityUnbound, f.packer.readBound(f.world.lookupSeam(), 999, null, &out));
    try testing.expectError(error.IdentityUnbound, f.packer.readBound(f.world.lookupSeam(), 61, null, &out));
}

test "models: the pin arithmetic holds and fails without the nonzero check; the CAS seal admits one sealer and a load-then-store seal releases the pin twice" {
    PinModel.weakening = .none;
    try explorer.expectHolds(PinModel, .{});
    PinModel.weakening = .pin_without_nonzero_check;
    try explorer.expectFails(PinModel, .{}, error.InvariantViolated);
    PinModel.weakening = .none;
    SealModel.weakening = .none;
    try explorer.expectHolds(SealModel, .{});
    SealModel.weakening = .seal_load_then_store;
    try explorer.expectFails(SealModel, .{}, error.InvariantViolated);
    SealModel.weakening = .none;
}

test "stats JSON names every instrument" {
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try testing.expect(std.mem.startsWith(u8, out, "{\"layout_promoted_inline\":"));
    try testing.expect(std.mem.indexOf(u8, out, "\"pack_pin_refcount_drift\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"pack_blocks_sealed_drain\":") != null);
    try testing.expect(std.mem.indexOf(u8, out, "sealed_batch") == null);
    try testing.expect(std.mem.endsWith(u8, out, "}"));
}
