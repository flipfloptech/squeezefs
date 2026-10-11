//! THE READ PATH — the ONE ordered procedure (charter §6.3; `docs/porting/
//! data_read_path.md` §1, §3, §4, §6.5, §7, §8.2, §9, §13). PORTABLE over
//! four seams the mount binds: the live layout map (`Meta`), the volume
//! table (`Volumes`: incarnation words + ranged device reads), the write
//! side's overlays (`WriteSide`) and the staging ring; the tiers, the
//! single-flight table, the admission instruments and the prefetch pipeline
//! are this module's own.
//!
//! Precedence: write-side overlays FIRST (a dirty local authority outranks
//! durable state), then by layout — inline from the record; staged from the
//! ring (this client's) or the durable copy; packed/striped resolve the
//! mapping BINDING-VALIDATED: bytes for key K serve block b only if (a) the
//! fill was incarnation-valid and (b) the CURRENT map still binds b → K
//! AFTER the bytes are in hand. If K were rebound to b after its observed
//! incarnation died it must have been freed (gen+1) and republished
//! (gen+1) in between, so (a) would have failed; (a) ∧ (b) proves the serve
//! is the block's current content, linearized at the recheck — which MUST
//! follow the read. The walk: hot tier → ≤ 256 KiB LRU → read-lane hold →
//! NVMe read cache → device, each hit rechecked; a rebind is a LOSS (never
//! an error) that re-resolves and retries up to the ladder cap, and
//! exhaustion escalates to ONE serialized settle under the inode's meta
//! stripe instead of EIO (`stale_binding_escalations`).
//!
//! Misses: a random ≤ `ranged_threshold` request on a passthrough volume
//! whose block is larger, not stream-classified, every probe missed ⇒ a
//! RANGED 4 KiB-outward DMA — straight into the registered reply window
//! when the window equals the request and the dest is 4 KiB-aligned (the
//! DEST LEASE: +28.6 % GB/s, daemon CPU/byte −50 %) — never published, never
//! single-flighted (a partial payload must not exist under a whole-block
//! key; deduping 4 KiB fetches under a 4 MiB key would serialize independent
//! sub-reads for no byte savings), ghost-exempt on the lease; else the
//! RESULT-CARRYING single-flight whole-block fetch, incarnation-validated
//! before (snapshot), during (every tier's insert-time `validate`) and
//! after (`still` after the puts; `!serve_valid ⇒ purge`).
//!
//! Hybrid I/O: O_DIRECT is NOT an admission class — the flag labels
//! observability; `direct_device_true ∧ O_DIRECT` is the measurement
//! RULER: no classifier, no tier serve, no ghost, no single-flight, every
//! passthrough request a ranged window. Overlay probes STAY (RYW is never
//! diagnostic-optional).
//!
//! The copy ledger attributes every daemon pass over payload bytes AT the
//! copy; the closure `dest + bounce + dest_dma + zc_serve + zc_pool_serve ≈
//! user bytes` and `warm_serve ≡ hot + hold + cache` are asserted in tests.
//!
//! `WriteSide` is the read seam over the write side. The write path's
//! `write_side_view.Composite` answers ONE block window (`serve` →
//! served / partial / redirect / settle / none); `data/plane.zig` binds it
//! to this vtable (`staged_block` composes a partial's gaps from the base
//! the write side named, follows a redirect to the shadow, runs the
//! serialized settle) — the import swap is that adapter (decision 0002).
//! The RAM double below is test-only.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const staging = @import("staging.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");
const hot_tier = @import("hot_tier.zig");
const read_lru = @import("read_lru.zig");
const read_hold = @import("read_hold.zig");
const nvme_read_cache = @import("nvme_read_cache.zig");
const purge = @import("purge.zig");
const single_flight = @import("single_flight_fill.zig");
const admission = @import("admission.zig");
const prefetch = @import("prefetch.zig");

pub const BlockKey = rsc.BlockKey;
pub const Mapping = layout.Mapping;
pub const Validate = hot_tier.Validate;
pub const FillClass = admission.FillClass;
pub const Error = errors.Error;

// ---------------------------------------------------------------------------
// Constants with their reasons (§7.2, §3.5, §11.3).
// ---------------------------------------------------------------------------

/// The correctness envelope of the non-escalating ladder.
pub const MAX_REBINDS: u32 = 24;
/// The latency cap of an escalating caller: after two stripe-held losses
/// more chases rarely win and the settle is one guaranteed fetch.
pub const SETTLE_HANDOFF: u32 = 8;
/// Losses before the walk goes device-direct: the shared cohort fill was
/// produced OUTSIDE the stripe, so inheriting it re-imports the invalidity.
pub const CONTENDED_BEFORE_ESCALATE: u32 = 2;
/// Attempts before the backoff tail: allocator free-list reuse cycles one
/// offset through alloc→DMA→publish; the loser needs the storm to pause.
pub const BACKOFF_AFTER: u32 = 4;
/// The ranged loop's own rebind cap; then the whole-block walk.
pub const RANGED_MAX_REBINDS: u32 = 8;
/// Retries of the serialized settle: exist ONLY for the invariant-violation
/// case (a mutator outside both lock disciplines).
pub const SETTLE_ATTEMPTS: u32 = 4;
/// Staged identity re-resolves before "identity kept moving".
pub const IDENTITY_LOOP: u32 = 64;
/// Flights per read path: one per concurrent cold fill; 1024 covers the
/// deepest device queue the transport can present (32 queues × 32 depth).
pub const MAX_FLIGHTS: usize = 1024;
/// The ranged bounce slab: 64 KiB slots (THP zeroing under GUP of 4 MiB
/// backings was 40 % of daemon CPU on the miss path), `max(cores × 64, 512)`
/// slots is the reference pool capacity.
pub const RANGED_BOUNCE_SLOT: usize = @intCast(rsc.INLINE_PUBLISH_MAX);
/// `ranged_buf_pool`: floor 4 MiB, weight 2.
pub const RANGED_POOL_FLOOR: u64 = 4 * derive.MiB;

pub fn rangedPoolSlots(cores: u32) u32 {
    return @intCast(@max(@as(u64, cores) * 64, 512));
}

// ---------------------------------------------------------------------------
// Instruments (§11.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    // Serve ladder.
    pub var cache_hits: counters.ShardedCounter = .{};
    pub var cache_misses: counters.ShardedCounter = .{};
    /// A primary that found its block resident after claiming the flight
    /// (the probe→enter window): served, never a second fetch.
    pub var read_fill_reprobe_hits: counters.ShardedCounter = .{};
    pub var read_lane_serves: counters.ShardedCounter = .{};
    pub var read_lane_serve_bytes: counters.ShardedCounter = .{};
    pub var hold_serve_admissions: counters.ShardedCounter = .{};
    // Hybrid I/O.
    pub var read_odirect_requests: counters.ShardedCounter = .{};
    pub var read_odirect_tier_serves: counters.ShardedCounter = .{};
    pub var read_device_true_reads: counters.ShardedCounter = .{};
    pub var direct_device_true: counters.Gauge = .{};
    // Ranged.
    pub var ranged_reads: counters.ShardedCounter = .{};
    pub var ranged_read_bytes: counters.ShardedCounter = .{};
    pub var ranged_read_unaligned_bounces: counters.ShardedCounter = .{};
    pub var ranged_read_rebinds: counters.ShardedCounter = .{};
    // Validation.
    pub var stale_binding_rebinds: counters.ShardedCounter = .{};
    pub var stale_binding_escalations: counters.ShardedCounter = .{};
    pub var staged_identity_retries: counters.ShardedCounter = .{};
    pub var read_settle_lost_serialized: counters.Tripwire = .init("read_settle_lost_serialized");
    pub var read_dest_overruns: counters.Tripwire = .init("read_dest_overruns");
    pub var mem_budget_dehydrate_paused: counters.ShardedCounter = .{};
    // The copy ledger (§8.2).
    pub var read_copy_dest_bytes: counters.ShardedCounter = .{};
    pub var read_copy_warm_serve_bytes: counters.ShardedCounter = .{};
    pub var read_copy_hot_serve_bytes: counters.ShardedCounter = .{};
    pub var read_copy_hold_serve_bytes: counters.ShardedCounter = .{};
    pub var read_copy_cache_serve_bytes: counters.ShardedCounter = .{};
    pub var read_copy_fill_slice_bytes: counters.ShardedCounter = .{};
    pub var read_copy_bounce_bytes: counters.ShardedCounter = .{};
    pub var read_dest_dma_bytes: counters.ShardedCounter = .{};
    pub var read_dest_lease_bytes: counters.ShardedCounter = .{};
    pub var read_zc_serve_bytes: counters.ShardedCounter = .{};
    pub var read_zc_pool_serve_bytes: counters.ShardedCounter = .{};
    pub var read_zc_pool_serve_warm_bytes: counters.ShardedCounter = .{};
    pub var read_fill_dma_bytes: counters.ShardedCounter = .{};
    pub var read_user_bytes: counters.ShardedCounter = .{};
    // The device-read ledger (decision 0003 §K): every byte a READ-side DMA
    // moves off a data device, partitioned by PURPOSE so `device ÷ user`
    // names its source. The WHERE partition above (`read_fill_dma_bytes`
    // pooled + `read_dest_dma_bytes` registered) sums to the same total:
    // `Σ read_dma_* ≡ read_fill_dma_bytes + read_dest_dma_bytes`.
    /// Ranged windows (direct into the dest or bounced), device-true included.
    pub var read_dma_ranged_bytes: counters.ShardedCounter = .{};
    /// Whole-block passthrough fills a reader waits for (demand, demand
    /// stream, escalation, device-true, the settle, the raw full-block leg).
    pub var read_dma_block_demand_bytes: counters.ShardedCounter = .{};
    /// Whole-block passthrough fills the prefetch pipeline issued.
    pub var read_dma_block_prefetch_bytes: counters.ShardedCounter = .{};
    /// Whole-block passthrough fills the ahead lane issued.
    pub var read_dma_block_lane_bytes: counters.ShardedCounter = .{};
    /// The stored image of a transformed block (`chunk` bytes per fill, any class).
    pub var read_dma_image_bytes: counters.ShardedCounter = .{};
    /// The ranged mapping window of a packed tenant / staged durable copy.
    pub var read_dma_window_bytes: counters.ShardedCounter = .{};
    // Phase families (exact-sum, span form).
    pub var read_serve_total_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_fetch_dma_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_decode_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_admission_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_deposit_ns: counters.ShardedLatencyHistogram = .{};
    pub var read_fill_total_ns: counters.ShardedLatencyHistogram = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"cache_hits\":{d},\"cache_misses\":{d},\"read_fill_reprobe_hits\":{d},\"read_lane_serves\":{d},\"read_lane_serve_bytes\":{d},\"hold_serve_admissions\":{d},\"read_odirect_requests\":{d},\"read_odirect_tier_serves\":{d},\"read_device_true_reads\":{d},\"direct_device_true\":{d},\"ranged_reads\":{d},\"ranged_read_bytes\":{d},\"ranged_read_unaligned_bounces\":{d},\"ranged_read_rebinds\":{d},\"stale_binding_rebinds\":{d},\"stale_binding_escalations\":{d},\"staged_identity_retries\":{d},\"read_settle_lost_serialized\":{d},\"read_dest_overruns\":{d},\"mem_budget_dehydrate_paused\":{d},", .{
        stats.cache_hits.load(),
        stats.cache_misses.load(),
        stats.read_fill_reprobe_hits.load(),
        stats.read_lane_serves.load(),
        stats.read_lane_serve_bytes.load(),
        stats.hold_serve_admissions.load(),
        stats.read_odirect_requests.load(),
        stats.read_odirect_tier_serves.load(),
        stats.read_device_true_reads.load(),
        stats.direct_device_true.load(),
        stats.ranged_reads.load(),
        stats.ranged_read_bytes.load(),
        stats.ranged_read_unaligned_bounces.load(),
        stats.ranged_read_rebinds.load(),
        stats.stale_binding_rebinds.load(),
        stats.stale_binding_escalations.load(),
        stats.staged_identity_retries.load(),
        stats.read_settle_lost_serialized.load(),
        stats.read_dest_overruns.load(),
        stats.mem_budget_dehydrate_paused.load(),
    });
    try w.print("\"read_copy_dest_bytes\":{d},\"read_copy_warm_serve_bytes\":{d},\"read_copy_hot_serve_bytes\":{d},\"read_copy_hold_serve_bytes\":{d},\"read_copy_cache_serve_bytes\":{d},\"read_copy_fill_slice_bytes\":{d},\"read_copy_bounce_bytes\":{d},\"read_dest_dma_bytes\":{d},\"read_dest_lease_bytes\":{d},\"read_zc_serve_bytes\":{d},\"read_zc_pool_serve_bytes\":{d},\"read_zc_pool_serve_warm_bytes\":{d},\"read_fill_dma_bytes\":{d},\"read_user_bytes\":{d},", .{
        stats.read_copy_dest_bytes.load(),
        stats.read_copy_warm_serve_bytes.load(),
        stats.read_copy_hot_serve_bytes.load(),
        stats.read_copy_hold_serve_bytes.load(),
        stats.read_copy_cache_serve_bytes.load(),
        stats.read_copy_fill_slice_bytes.load(),
        stats.read_copy_bounce_bytes.load(),
        stats.read_dest_dma_bytes.load(),
        stats.read_dest_lease_bytes.load(),
        stats.read_zc_serve_bytes.load(),
        stats.read_zc_pool_serve_bytes.load(),
        stats.read_zc_pool_serve_warm_bytes.load(),
        stats.read_fill_dma_bytes.load(),
        stats.read_user_bytes.load(),
    });
    try w.print("\"read_dma_ranged_bytes\":{d},\"read_dma_block_demand_bytes\":{d},\"read_dma_block_prefetch_bytes\":{d},\"read_dma_block_lane_bytes\":{d},\"read_dma_image_bytes\":{d},\"read_dma_window_bytes\":{d},", .{
        stats.read_dma_ranged_bytes.load(),
        stats.read_dma_block_demand_bytes.load(),
        stats.read_dma_block_prefetch_bytes.load(),
        stats.read_dma_block_lane_bytes.load(),
        stats.read_dma_image_bytes.load(),
        stats.read_dma_window_bytes.load(),
    });
    try w.writeAll("\"read_serve_phase_ns\":{\"total\":");
    try stats.read_serve_total_ns.snapshot().writeJson(w);
    try w.writeAll("},\"read_fill_phase_ns\":{\"fetch_dma\":");
    try stats.read_fill_fetch_dma_ns.snapshot().writeJson(w);
    try w.writeAll(",\"decode\":");
    try stats.read_fill_decode_ns.snapshot().writeJson(w);
    try w.writeAll(",\"admission\":");
    try stats.read_fill_admission_ns.snapshot().writeJson(w);
    try w.writeAll(",\"deposit\":");
    try stats.read_fill_deposit_ns.snapshot().writeJson(w);
    try w.writeAll(",\"fill_total\":");
    try stats.read_fill_total_ns.snapshot().writeJson(w);
    try w.writeAll("},\"fill_pool\":");
    try fill_pool.writeStatsJson(w);
    try w.writeAll(",\"hot_tier\":");
    try hot_tier.writeStatsJson(w);
    try w.writeAll(",\"read_lru\":");
    try read_lru.writeStatsJson(w);
    try w.writeAll(",\"read_hold\":");
    try read_hold.writeStatsJson(w);
    try w.writeAll(",\"nvme_read_cache\":");
    try nvme_read_cache.writeStatsJson(w);
    try w.writeAll(",\"purge\":");
    try purge.writeStatsJson(w);
    try w.writeAll(",\"single_flight\":");
    try single_flight.writeStatsJson(w);
    try w.writeAll(",\"admission\":");
    try admission.writeStatsJson(w);
    try w.writeAll(",\"prefetch\":");
    try prefetch.writeStatsJson(w);
    try w.writeByte('}');
}

/// Span-form phase recording: each `mark` records the phase from the last
/// clock read and returns the new one, so abutting phases sum EXACTLY to
/// the total measured from the first read.
const PhaseClock = struct {
    t0: u64,
    last: u64,

    fn start() PhaseClock {
        const t = clock.monotonicNs();
        return .{ .t0 = t, .last = t };
    }

    fn mark(self: *PhaseClock, h: *counters.ShardedLatencyHistogram) void {
        const now = clock.monotonicNs();
        h.record(now -| self.last);
        self.last = now;
    }

    /// The total is the LAST clock read: Σ phases ≡ total to the ns.
    fn total(self: *PhaseClock, h: *counters.ShardedLatencyHistogram) void {
        h.record(self.last -| self.t0);
    }
};

// ---------------------------------------------------------------------------
// The seams.
// ---------------------------------------------------------------------------

/// The file facts a read resolves ONCE (before any guard — 0002 row 84).
pub const FileInfo = struct {
    size: u64,
    kind: layout.Kind,
    file_id: layout.FileId = layout.zero_file_id,
    /// The staged file's durable copy / the packed tenant.
    durable_copy: ?Mapping = null,
};

/// The live layout map. `binding` is the CURRENT `block → mapping` (null =
/// hole); it is re-read at every binding check, never cached across a
/// fetch.
pub const Meta = struct {
    ctx: *anyopaque,
    file: *const fn (ctx: *anyopaque, ino: u64) ?FileInfo,
    binding: *const fn (ctx: *anyopaque, ino: u64, block: u64) ?Mapping,
    /// The inline payload copied into `out`; bytes copied.
    inline_data: *const fn (ctx: *anyopaque, ino: u64, offset: u64, out: []u8) usize,
};

pub const DeviceError = errors.Error;

/// What a device read's destination is: a pooled/bounce buffer of the
/// read path, or the CALLER's registered destination (the dest-lease and
/// raw full-block legs) — the zcrx lane counts the latter as its fused
/// gather subset (charter §6.3).
pub const DestShape = enum { pooled, registered };

/// Who served a `read_range`: the device DMA (counted in the DMA byte
/// instruments), or the zcrx lane's ONE gather (`zcrx_fill_bytes` is its
/// provenance; `read_fill_dma_bytes`/`read_dest_dma_bytes` are NOT
/// incremented — the extended closure: every served byte's provenance ∈
/// {pooled DMA, zcrx area, dest DMA}).
pub const Served = enum { dma, lane };

/// The volume table: the incarnation word per block and ranged device
/// reads. `read_range` fills `dest.len` bytes at `device_off` on `vol_tag`
/// into `dest` (4 KiB-aligned pointer, length and offset) — by DMA, or by
/// the zcrx lane sitting behind the seam in the data plane's volume
/// binding, judged per call; the device leg REFUSES a dead-lifetime key
/// through the caller's snapshot (null ⇒ no DMA).
pub const Volumes = struct {
    ctx: *anyopaque,
    allocator_of: *const fn (ctx: *anyopaque, vol_tag: u64) ?*block_alloc.BlockAllocator,
    read_range: *const fn (ctx: *anyopaque, vol_tag: u64, device_off: u64, dest: []u8, shape: DestShape) DeviceError!Served,
    /// The device fd for the zc direct leg (null = not resolvable).
    device_fd: *const fn (ctx: *anyopaque, vol_tag: u64) ?std.os.linux.fd_t,
};

/// The transform seam for non-passthrough volumes: decode the stored
/// `image` (the chunk read from the device) into `out` (≥ the scratch
/// length); returns the plaintext view (inside `out` or `image`).
pub const Decode = struct {
    ctx: *anyopaque,
    /// The stored image length to read per block (`chunk`).
    chunk: u64,
    decode: *const fn (ctx: *anyopaque, image: []u8, out: []u8) error{Corrupt}![]const u8,
};

pub const OverlayVerdict = union(enum) {
    /// No write-side authority covers the window.
    none,
    /// Composed into `dest`: this many bytes.
    served: usize,
    /// An overlay read failed terminally.
    failed: Error,
};

pub const ExtentCoverage = enum { none, partial, full };

/// The write side's view the read path consults FIRST; the data plane
/// adapts `write_side_view.Composite` to it (module doc).
/// `WriteSide.staged_block`'s answer: `served` n bytes (the rest of the
/// window is zeros), `none` = no write-side custody (the map's bytes
/// stand), `busy` = a try-only probe met custody it would have waited for.
pub const StagedBlock = union(enum) { served: usize, none, busy };

pub const WriteSide = struct {
    ctx: *anyopaque,
    /// The accumulation buffer's coverage-aware snapshot and any open
    /// device-overlay record over `[offset, offset + len)`: composed into
    /// `dest` when the window is covered (gaps from the base or zeros are
    /// the write side's), else `.none`.
    overlay_read: *const fn (ctx: *anyopaque, ino: u64, offset: u64, len: u64, dest: []u8) OverlayVerdict,
    /// W2 extent-record runs over `(ino, block)[rel_s, rel_e)`: the
    /// coverage class, and the runs copied into `dest` (NEWER than every
    /// base: applied after the base bytes).
    extent_runs: *const fn (ctx: *anyopaque, ino: u64, block: u64, rel_s: u64, rel_e: u64, dest: []u8) ExtentCoverage,
    /// The write side's block window (the write side's custody station):
    /// `[rel_s, rel_e)` composed into `dest`. `try_only` is the uring
    /// worker's warm-ladder probe: the write side answers `busy` instead
    /// of parking on its block lock, awaiting an overlay's pages or
    /// reading the device (charter §4 "the reap thread never blocks").
    staged_block: *const fn (ctx: *anyopaque, ino: u64, block: u64, rel_s: u64, rel_e: u64, dest: []u8, try_only: bool) StagedBlock,
    /// This client's ring entry for a staged file (`resolveStagedRead`).
    staged_payload: *const fn (ctx: *anyopaque, ino: u64, file_id: layout.FileId, offset: u64, len: u64, dest: []u8) StagedVerdict,
    /// Retained extents on the ino (the warm ladder demotes).
    has_retained_extents: *const fn (ctx: *anyopaque, ino: u64) bool,
    /// An OPEN/FROZEN device-overlay record on `(ino, block)` — ONE relaxed
    /// probe (the warm ladder demotes: the OVERWRITE population's old key
    /// has stale tier entries, R1).
    open_overlay: *const fn (ctx: *anyopaque, ino: u64, block: u64) bool,
    /// ANY write-side custody of `(ino, block)`: an accumulation or extent
    /// entry, an overlay record, an epoch shadow. The direct device leg
    /// declines on it — the device holds only what landed, never the
    /// parked newer bytes (gf27: a fresh W2 extent after the fold).
    has_custody: *const fn (ctx: *anyopaque, ino: u64, block: u64) bool,

    /// No write-side state at all (a read-only mount's view).
    pub const none: WriteSide = .{
        .ctx = @ptrCast(@constCast(&none_ctx)),
        .overlay_read = &noneOverlay,
        .extent_runs = &noneExtent,
        .staged_block = &noneStaged,
        .staged_payload = &noneStagedPayload,
        .has_retained_extents = &noneRetained,
        .open_overlay = &noneOpenOverlay,
        .has_custody = &noneCustody,
    };
    const none_ctx: u8 = 0;
    fn noneOverlay(_: *anyopaque, _: u64, _: u64, _: u64, _: []u8) OverlayVerdict {
        return .none;
    }
    fn noneExtent(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, _: []u8) ExtentCoverage {
        return .none;
    }
    fn noneStaged(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, _: []u8, _: bool) StagedBlock {
        return .none;
    }
    fn noneStagedPayload(_: *anyopaque, _: u64, _: layout.FileId, _: u64, _: u64, _: []u8) StagedVerdict {
        return .absent;
    }
    fn noneRetained(_: *anyopaque, _: u64) bool {
        return false;
    }
    fn noneCustody(_: *anyopaque, _: u64, _: u64) bool {
        return false;
    }
    fn noneOpenOverlay(_: *anyopaque, _: u64, _: u64) bool {
        return false;
    }
};

pub const StagedVerdict = union(enum) {
    /// This client's ring served `n` bytes (zero-padded to the request).
    served: usize,
    /// A stage/promotion is in flight: back off and re-resolve.
    in_flight,
    /// No ring entry here (the durable copy or the lost arm decides).
    absent,
};

/// The caller's destination: the reply window. `registered` = a kernel
/// payload window the device may DMA straight into (the lease).
pub const Dest = struct {
    buf: []u8,
    registered: bool = false,

    pub fn aligned(self: Dest) bool {
        return derive.isAligned(@intFromPtr(self.buf.ptr), rsc.LBA_GRAIN);
    }
};

pub const Flags = struct {
    odirect: bool = false,
    /// The ring feed already observed this request (the warm ladder's
    /// demote carries it); the pipeline is not touched twice.
    lane_pre_fed: bool = false,
};

pub const Config = struct {
    block_size: u64,
    /// `transform.isPassthrough`: no decode, ranged windows legal.
    passthrough: bool,
    /// `SQUEEZEFS_READ_RANGED_THRESHOLD` (0 = kill switch).
    ranged_threshold: u64,
    /// `SQUEEZEFS_READ_DEST_LEASE`.
    dest_lease: bool,
    /// `SQUEEZEFS_READ_LANE`.
    read_lane: bool,
    /// `SQUEEZEFS_DIRECT_DEVICE_TRUE` / `-o direct_device_true`.
    direct_device_true: bool,
    /// `SQUEEZEFS_READ_ZC_SERVE` ∧ a zero-copy session (`InitCaps.read_zc_
    /// serve`): the fill pool is the MAP_SHARED memfd slab so warm serves
    /// bridge `READ_FIXED(fd → slot)`; off = the heap slab (the copy arm).
    zc_serve: bool = false,
    /// The non-escalating ladder cap (24; the test seam).
    rebind_ladder_cap: u32 = MAX_REBINDS,
    cores: u32,
    hot_budget: u64,
    read_mem_limit: u64,
    mem_budget: u64,
    admission_policy: admission.Policy,
    fill_pct: u64,
    prefetch: prefetch.Config,

    pub fn fromEnv(env: *const std.process.Environ.Map, block_size: u64, passthrough: bool, facts: derive.Facts, mem_budget: u64) Config {
        const read_mem = hot_tier.resolveReadMemLimit(knobs.optIntKnob("SQUEEZEFS_READ_MEM_CACHE_MB", u64, env), facts, mem_budget);
        const hot = hot_tier.resolveHotBudget(knobs.optIntKnob("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", u64, env), block_size, read_mem);
        const cap_seam = knobs.intKnob("SQUEEZEFS_TEST_READ_REBIND_LADDER_CAP", u32, env);
        return .{
            .block_size = block_size,
            .passthrough = passthrough,
            .ranged_threshold = knobs.intKnob("SQUEEZEFS_READ_RANGED_THRESHOLD", u64, env),
            .dest_lease = knobs.boolKnob("SQUEEZEFS_READ_DEST_LEASE", env),
            .read_lane = knobs.boolKnob("SQUEEZEFS_READ_LANE", env),
            .direct_device_true = knobs.boolKnob("SQUEEZEFS_DIRECT_DEVICE_TRUE", env),
            .zc_serve = knobs.boolKnob("SQUEEZEFS_READ_ZC_SERVE", env),
            .rebind_ladder_cap = if (cap_seam == 0) MAX_REBINDS else cap_seam,
            .cores = facts.cpus,
            .hot_budget = hot,
            .read_mem_limit = read_mem,
            .mem_budget = mem_budget,
            .admission_policy = admission.Policy.fromEnv(env, hot),
            .fill_pct = knobs.intKnob("SQUEEZEFS_READ_ADMISSION_FILL_PCT", u64, env),
            .prefetch = prefetch.Config.fromEnv(env, hot, block_size, mem_budget),
        };
    }
};

/// `ranged_eligible`: threshold on ∧ `len ≤ threshold` ∧ `block >
/// threshold` ∧ passthrough ∧ not stream-classified (streams want whole
/// blocks: 1.0× amp + the pipeline).
pub fn rangedEligible(threshold: u64, len: u64, block_size: u64, passthrough: bool, streaming_fresh: bool) bool {
    return threshold != 0 and len <= threshold and block_size > threshold and passthrough and !streaming_fresh;
}

/// The ranged window: `[start & ~4095, min(ceil4k(end), block))`.
pub const Window = struct {
    aligned_start: u64,
    aligned_end: u64,

    pub fn of(rel_s: u64, rel_e: u64, block_size: u64) Window {
        return .{ .aligned_start = derive.alignDown(rel_s, rsc.LBA_GRAIN), .aligned_end = @min(derive.alignUp(rel_e, rsc.LBA_GRAIN), block_size) };
    }

    pub fn len(self: Window) u64 {
        return self.aligned_end - self.aligned_start;
    }

    /// The bounce leg copies ≤ request + 8 KiB; window padding is never served.
    pub fn bounced(self: Window, req_len: u64) bool {
        return self.len() != req_len;
    }
};

// ---------------------------------------------------------------------------
// The read path.
// ---------------------------------------------------------------------------

/// A whole block in hand: where its bytes live and whether they may serve.
pub const Block = struct {
    const Where = enum(u8) { pool, lru, bounce };
    where: Where,
    slot: u32 = 0,
    len: u32,
    bounce: []align(fill_pool.SLOT_ALIGN) u8 = &.{},
    serve_valid: bool,
    /// The hold's ledger-visible deposit: run the serve ceremony.
    hold_ledger_visible: bool = false,

    fn bytes(self: Block, rp: *ReadPath) []const u8 {
        return switch (self.where) {
            .pool => rp.pool.bytes(self.slot)[0..self.len],
            .lru => rp.lru.bytes(.{ .slot = self.slot, .len = self.len }),
            .bounce => self.bounce[0..self.len],
        };
    }

    fn release(self: Block, rp: *ReadPath) void {
        switch (self.where) {
            .pool => _ = rp.pool.unpin(self.slot),
            .lru => rp.lru.unpin(.{ .slot = self.slot, .len = self.len }),
            .bounce => rp.gpa.free(self.bounce),
        }
    }
};

/// The per-ino settle serialization: the (3.5) meta stripe every map
/// publish funnels through (`layout.MetaStripes`).
pub const Settle = layout.MetaStripes;

/// Test seams (process atomics, one relaxed load each; zero cost unset).
pub const Seams = struct {
    /// A hook run after a fill's DMA and before its binding recheck.
    after_dma: ?*const fn (ctx: ?*anyopaque, key: BlockKey) void = null,
    after_dma_ctx: ?*anyopaque = null,
    /// A hook run after every tier probe missed and before the flight is
    /// entered — the probe→enter window a whole sibling flight can fit in.
    before_flight: ?*const fn (ctx: ?*anyopaque, key: BlockKey) void = null,
    before_flight_ctx: ?*anyopaque = null,
};

pub const ReadPath = struct {
    gpa: Allocator,
    cfg: Config,
    meta: Meta,
    vols: Volumes,
    write_side: WriteSide,
    decode: ?Decode,
    mem: ?*const budget.MemBudget,
    pool: *fill_pool.Pool,
    ranged_bounce: *fill_pool.Pool,
    hot: *hot_tier.HotTier,
    lru: *read_lru.ReadLru,
    hold: *read_hold.Hold,
    nvme: nvme_read_cache.Cache,
    tiers: purge.Tiers,
    flights: *single_flight.FlightTable(MAX_FLIGHTS),
    adm: admission.Admission,
    /// One pipeline per ino (bounded table keyed by ino; a miss claims).
    pipelines: PipelineTable,
    settle: Settle,
    seams: Seams = .{},
    /// The speculative-fill issuer (null = plans are bookkept, never run).
    issuer: ?Issuer = null,

    const PipelineTable = struct {
        /// 64 files in flight per read path: the stream-lane population a
        /// box's readers present at once (the reference's moka entry
        /// capacity was unbounded; 64 bounds the RAM at 64 × ~2 KiB).
        const slots = 64;
        lock: @import("../meta/journal_core.zig").SpinLock = .{},
        inos: [slots]u64 = @splat(0),
        last_ns: [slots]u64 = @splat(0),
        pipes: [slots]prefetch.Pipeline,

        fn get(self: *PipelineTable, ino: u64, claim: bool, now: u64, cfg: prefetch.Config) ?*prefetch.Pipeline {
            self.lock.lock();
            defer self.lock.unlock();
            var victim: usize = 0;
            for (&self.inos, 0..) |*i, idx| {
                if (i.* == ino) {
                    self.last_ns[idx] = now;
                    return &self.pipes[idx];
                }
                if (self.last_ns[idx] < self.last_ns[victim]) victim = idx;
            }
            if (!claim) return null;
            self.inos[victim] = ino;
            self.last_ns[victim] = now;
            self.pipes[victim] = prefetch.Pipeline.init(cfg);
            return &self.pipes[victim];
        }
    };

    pub const InitError = Allocator.Error || fill_pool.InitError;

    /// `nvme` is adopted (the caller built it: the RAM double, the Linux
    /// `Ring.open`, or `initDisabled`). `decode` null = passthrough.
    pub fn init(gpa: Allocator, cfg: Config, meta: Meta, vols: Volumes, write_side: WriteSide, decode: ?Decode, nvme: nvme_read_cache.Cache, mem: ?*const budget.MemBudget) InitError!*ReadPath {
        assert(cfg.block_size > 0 and derive.isAligned(cfg.block_size, rsc.LBA_GRAIN));
        const self = try gpa.create(ReadPath);
        errdefer gpa.destroy(self);
        // The slot holds the decode scratch on a transformed volume
        // (`transform.scratchLen`: block + 128 KiB) and exactly the block
        // otherwise.
        const slot_size: usize = if (decode) |d| @intCast(derive.alignUp(@max(d.chunk, cfg.block_size + 128 * derive.KiB), fill_pool.SLOT_ALIGN)) else @intCast(cfg.block_size);
        const hot_blocks = cfg.hot_budget / cfg.block_size;
        const hold_blocks = read_hold.maxEntries(cfg.mem_budget, cfg.block_size);
        const pool_slots = fill_pool.poolSlots(cfg.cores, hot_blocks, hold_blocks);
        // The memfd slab when the zc serve is armed; a refusal declines the
        // lever LOUDLY to the heap slab (copy economics, never correctness).
        const pool = if (cfg.zc_serve and builtin.os.tag == .linux)
            fill_pool.Pool.initMemfd(gpa, slot_size, pool_slots) catch |e| blk: {
                log.err("read fill pool: memfd slab refused ({t}); the zc serve lever declines to the heap slab (copy path)", .{e});
                break :blk try fill_pool.Pool.initRam(gpa, slot_size, pool_slots);
            }
        else
            try fill_pool.Pool.initRam(gpa, slot_size, pool_slots);
        errdefer pool.deinit();
        const ranged_bounce = try fill_pool.Pool.initRam(gpa, RANGED_BOUNCE_SLOT, rangedPoolSlots(cfg.cores));
        errdefer ranged_bounce.deinit();
        const hot = try hot_tier.HotTier.init(gpa, pool, cfg.block_size, cfg.hot_budget, cfg.cores);
        errdefer hot.deinit();
        const lru = try read_lru.ReadLru.init(gpa, cfg.read_mem_limit, cfg.cores);
        errdefer lru.deinit();
        const hold = try read_hold.Hold.init(gpa, pool, hold_blocks);
        errdefer hold.deinit();
        const flights = try gpa.create(single_flight.FlightTable(MAX_FLIGHTS));
        errdefer gpa.destroy(flights);
        flights.* = single_flight.FlightTable(MAX_FLIGHTS).init(pool);
        var adm = try admission.Admission.init(gpa, cfg.admission_policy, cfg.fill_pct);
        errdefer adm.deinit();
        self.* = .{
            .gpa = gpa,
            .cfg = cfg,
            .meta = meta,
            .vols = vols,
            .write_side = write_side,
            .decode = decode,
            .mem = mem,
            .pool = pool,
            .ranged_bounce = ranged_bounce,
            .hot = hot,
            .lru = lru,
            .hold = hold,
            .nvme = nvme,
            .tiers = undefined,
            .flights = flights,
            .adm = adm,
            .pipelines = .{ .pipes = undefined },
            .settle = .{},
        };
        self.tiers = .{ .hot = hot, .lru = lru, .hold = hold, .nvme = &self.nvme };
        hot.sink = .{ .ctx = self, .on_evict = &hotVictim };
        lru.sink = .{ .ctx = self, .on_evict = &lruVictim };
        if (cfg.read_lane) hold.setBudget(read_hold.holdBudgetBytes(cfg.mem_budget, cfg.block_size, 1, 0));
        stats.direct_device_true.set(@intFromBool(cfg.direct_device_true));
        prefetch.stats.read_lane_armed.set(@intFromBool(cfg.read_lane));
        return self;
    }

    pub fn deinit(self: *ReadPath) void {
        self.hot.deinit();
        self.lru.deinit();
        self.hold.deinit();
        self.nvme.deinit();
        self.adm.deinit();
        self.gpa.destroy(self.flights);
        self.ranged_bounce.deinit();
        self.pool.deinit();
        self.gpa.destroy(self);
    }

    /// The write side's purge seam (W1, the overlay's displacement, …).
    pub fn purgeSeam(self: *ReadPath) purge.PurgeSeam {
        return purge.seam(&self.tiers);
    }

    /// THE one fd-source probe (`zc_fd_source`): the slab's `(fd, off)` for
    /// a served view iff the view lies on the memfd slab — the handler's
    /// `replyFdBody` leg; null ⇒ the copy path (counted by the caller).
    pub fn fdSource(self: *ReadPath, view: []const u8) ?fill_pool.FdOffset {
        const loc = self.pool.locate(view.ptr, view.len) orelse return null;
        return loc.fd_off;
    }

    /// The ring feed (`ring_read_lane_touch`): a warm serve observed ONCE
    /// at the sink — silent consumption must advance the consume edge; a
    /// warm serve never mints a lane (`claim = false`).
    pub fn ringReadLaneTouch(self: *ReadPath, ino: u64, offset: u64, len: u64, size: u64) void {
        if (len == 0) return;
        const now = clock.monotonicNs();
        const p = self.pipelineFor(ino, false, now) orelse return;
        const bs = self.cfg.block_size;
        _ = p.touch(self.prefetchEnv(ino), .{
            .offset = offset,
            .len = len,
            .block_size = bs,
            .start_block = offset / bs,
            .end_block = (offset + len - 1) / bs,
            .total_blocks = derive.ceilDiv(size, bs),
            .will_wait = false,
            .lane_claim = false,
            .dest_leaseable = false,
            .level = self.level(),
            .now = now,
        });
    }

    /// Register every sheddable component (§2.5). The two slabs gauge their
    /// idle-resident bytes and carry the idle trim (`read_fill_pool`).
    pub fn registerComponents(self: *ReadPath, mem: *budget.MemBudget) void {
        mem.register(self.hot.component());
        mem.register(self.lru.component());
        mem.register(self.hold.component());
        mem.register(self.pool.component());
        mem.register(self.nvme.component());
        mem.register(self.ranged_bounce.componentNamed("ranged_buf_pool", RANGED_POOL_FLOOR, 2));
    }

    fn level(self: *const ReadPath) budget.Level {
        const m = self.mem orelse return .green;
        return m.level();
    }

    fn publishPaused(self: *const ReadPath) bool {
        const m = self.mem orelse return false;
        return m.tierPublishPaused();
    }

    // ---- incarnation helpers (§7.1) ----

    fn allocatorOf(self: *ReadPath, key: BlockKey) ?*block_alloc.BlockAllocator {
        return self.vols.allocator_of(self.vols.ctx, key.vol_tag);
    }

    /// `fill_incarnation`: null = unstable (never publish); an unknown
    /// volume is `UNKNOWN_STABLE` (vacuously valid, never published).
    fn fillIncarnation(self: *ReadPath, key: BlockKey) ?u64 {
        const a = self.allocatorOf(key) orelse return block_alloc.UNKNOWN_STABLE;
        return a.fillIncarnation(key.blockIdx(self.cfg.block_size));
    }

    fn tracked(self: *ReadPath, key: BlockKey) bool {
        return self.allocatorOf(key) != null;
    }

    fn still(self: *ReadPath, key: BlockKey, before: u64) bool {
        const a = self.allocatorOf(key) orelse return true;
        return a.fillIncarnationStill(key.blockIdx(self.cfg.block_size), before);
    }

    /// The warm ladder's direct-leg pair: the pre-DMA snapshot (null =
    /// unstable: never DMA) and the post-DMA `still`.
    pub fn incarnationSnapshot(self: *ReadPath, key: BlockKey) ?u64 {
        return self.fillIncarnation(key);
    }

    pub fn incarnationStill(self: *ReadPath, key: BlockKey, before: u64) bool {
        return self.still(key, before);
    }

    const StillCtx = struct { rp: *ReadPath, key: BlockKey, before: u64 };

    fn stillValidate(ctx: ?*anyopaque) bool {
        const c: *StillCtx = @ptrCast(@alignCast(ctx.?));
        return c.rp.still(c.key, c.before);
    }

    fn currentBinding(self: *ReadPath, ino: u64, block: u64) ?BlockKey {
        const m = self.meta.binding(self.meta.ctx, ino, block) orelse return null;
        return BlockKey.fromMapping(m);
    }

    fn bindingIs(self: *ReadPath, ino: u64, block: u64, key: BlockKey) bool {
        const cur = self.currentBinding(ino, block) orelse return false;
        return cur.eql(key);
    }

    fn pipelineFor(self: *ReadPath, ino: u64, claim: bool, now: u64) ?*prefetch.Pipeline {
        return self.pipelines.get(ino, claim, now, self.cfg.prefetch);
    }

    fn streamingFresh(self: *ReadPath, ino: u64, now: u64) bool {
        const p = self.pipelineFor(ino, false, now) orelse return false;
        return p.lanes.anyStreamingFresh(now);
    }

    // ---- §1.1 / §1.2 the entry ----

    /// `read`: `[offset, offset + len)` of `ino` into `dest`; the bytes
    /// served (0 at/after EOF). `dest.buf.len ≥ min(len, size − offset)`
    /// or `BufferTooSmall` (`read_dest_overruns` — FUSE-4e: never a short
    /// read below EOF).
    pub fn read(self: *ReadPath, io: Io, ino: u64, offset: u64, len: u64, dest: Dest, flags: Flags) Error!usize {
        var pc = PhaseClock.start();
        defer {
            pc.last = clock.monotonicNs();
            pc.total(&stats.read_serve_total_ns);
        }
        if (flags.odirect) stats.read_odirect_requests.inc();
        const info = self.meta.file(self.meta.ctx, ino) orelse return error.FileNotFound;
        if (offset >= info.size or len == 0) return 0;
        const want: usize = @intCast(@min(len, info.size - offset));
        if (dest.buf.len < want) {
            stats.read_dest_overruns.fire("a serve longer than the reply window was refused");
            return error.BufferTooSmall;
        }
        const out = dest.buf[0..want];
        // 1. Write-side overlays FIRST.
        switch (self.write_side.overlay_read(self.write_side.ctx, ino, offset, want, out)) {
            .served => |n| {
                stats.read_copy_dest_bytes.add(n);
                stats.read_user_bytes.add(n);
                return n;
            },
            .failed => |e| return e,
            .none => {},
        }
        const n = switch (info.kind) {
            .inline_ => self.readInline(ino, offset, out),
            .staged => try self.readStaged(io, ino, info, offset, out),
            .packed_ => try self.readPacked(ino, info, offset, out),
            .striped => try self.readStriped(io, ino, info, offset, out, dest, flags),
        };
        stats.read_user_bytes.add(n);
        return n;
    }

    fn readInline(self: *ReadPath, ino: u64, offset: u64, out: []u8) usize {
        const n = self.meta.inline_data(self.meta.ctx, ino, offset, out);
        @memset(out[n..], 0);
        stats.read_copy_dest_bytes.add(out.len);
        return out.len;
    }

    // ---- §1.3 staged ----

    fn readStaged(self: *ReadPath, io: Io, ino: u64, info: FileInfo, offset: u64, out: []u8) Error!usize {
        var attempt: u32 = 0;
        var cur = info;
        while (attempt < IDENTITY_LOOP) : (attempt += 1) {
            switch (self.write_side.staged_payload(self.write_side.ctx, ino, cur.file_id, offset, out.len, out)) {
                .served => |n| {
                    stats.cache_hits.inc();
                    stats.read_copy_dest_bytes.add(n);
                    return n;
                },
                .in_flight => {
                    stats.staged_identity_retries.inc();
                    if (attempt < 4) std.Thread.yield() catch {} else io.sleep(.fromMilliseconds(2), .awake) catch {};
                    cur = self.meta.file(self.meta.ctx, ino) orelse return error.FileNotFound;
                    if (cur.kind != .staged) return self.read(io, ino, offset, out.len, .{ .buf = out }, .{});
                    continue;
                },
                .absent => {},
            }
            if (cur.durable_copy) |m| {
                // ONE ranged routed window on the mapping's own volume.
                const n = try self.readMappingWindow(m, offset, out);
                // Still bound to this identity after the bytes are in hand?
                const fresh = self.meta.file(self.meta.ctx, ino) orelse return error.FileNotFound;
                if (fresh.kind == .staged and std.mem.eql(u8, &fresh.file_id, &cur.file_id) and fresh.durable_copy != null and fresh.durable_copy.?.eql(m)) return n;
                stats.staged_identity_retries.inc();
                cur = fresh;
                if (cur.kind != .staged) return self.read(io, ino, offset, out.len, .{ .buf = out }, .{});
                continue;
            }
            // STABLE lost identity: size-consistent zeros, counted and warned.
            const zeros = staging.lostStagedRangeLen(offset, out.len, cur.size);
            staging.stats.staged_payload_lost_reads.inc();
            log.warn("staged payload unreachable for ino {d}: serving {d} zero bytes at offset {d} (another client's live staged custody, or a crash discarded it)", .{ ino, zeros, offset });
            @memset(out, 0);
            stats.read_copy_dest_bytes.add(out.len);
            return out.len;
        }
        log.err("read of ino {d}: the staged identity kept moving across {d} re-resolves", .{ ino, IDENTITY_LOOP });
        return error.InputOutput;
    }

    /// A packed tenant / durable copy: the window on the mapping's OWN
    /// volume through the ONE choke point.
    fn readMappingWindow(self: *ReadPath, m: Mapping, rel_start: u64, out: []u8) Error!usize {
        const w = m.readWindow(self.cfg.block_size, .{ .start = rel_start, .end = rel_start + out.len }) catch return error.InputOutput;
        const key = BlockKey.fromMapping(m);
        const before = self.fillIncarnation(key) orelse return error.InputOutput;
        const got = w.hi - w.lo;
        if (got == 0) {
            @memset(out, 0);
            return out.len;
        }
        // The window into a bounce (≤ 64 KiB slab, else the fill pool), then the slice.
        const b = try self.acquireBounce(w.len);
        defer self.releaseBounce(b);
        if (try self.vols.read_range(self.vols.ctx, w.vol_tag, w.device_off, b.bytes[0..@intCast(w.len)], .pooled) == .dma) {
            stats.read_fill_dma_bytes.add(w.len);
            stats.read_dma_window_bytes.add(w.len);
        }
        if (!self.still(key, before)) return error.InputOutput;
        @memcpy(out[0..got], b.bytes[w.lo..w.hi]);
        @memset(out[got..], 0);
        stats.read_copy_dest_bytes.add(out.len);
        return out.len;
    }

    fn readPacked(self: *ReadPath, ino: u64, info: FileInfo, offset: u64, out: []u8) Error!usize {
        _ = ino;
        const m = info.durable_copy orelse return error.InputOutput;
        return self.readMappingWindow(m, offset, out);
    }

    const Bounce = struct { bytes: []align(fill_pool.SLOT_ALIGN) u8, pool: *fill_pool.Pool, slot: u32 };

    /// `read_bounce_pool(size)`: ≤ 64 KiB the ranged slab, else the fill pool.
    fn acquireBounce(self: *ReadPath, len: u64) Error!Bounce {
        const p = if (len <= RANGED_BOUNCE_SLOT) self.ranged_bounce else self.pool;
        if (len > p.slot_size) return error.ReadSizeExceedsPool;
        const slot = p.acquire() orelse return error.QueueFull;
        return .{ .bytes = p.bytes(slot), .pool = p, .slot = slot };
    }

    fn releaseBounce(_: *ReadPath, b: Bounce) void {
        _ = b.pool.unpin(b.slot);
    }

    // ---- §1.4 striped ----

    fn readStriped(self: *ReadPath, io: Io, ino: u64, info: FileInfo, offset: u64, out: []u8, dest: Dest, flags: Flags) Error!usize {
        const bs = self.cfg.block_size;
        var served: usize = 0;
        var off = offset;
        while (served < out.len) {
            const b = off / bs;
            const rel_s = off - b * bs;
            const rel_e = @min(rel_s + (out.len - served), bs);
            const piece = out[served..][0..@intCast(rel_e - rel_s)];
            const piece_dest: Dest = .{ .buf = piece, .registered = dest.registered };
            try self.readSingleBlock(io, ino, info, b, rel_s, rel_e, piece, piece_dest, flags, off, out.len);
            served += piece.len;
            off += piece.len;
        }
        return served;
    }

    /// §1.4 — the single-block arm.
    fn readSingleBlock(self: *ReadPath, io: Io, ino: u64, info: FileInfo, b: u64, rel_s: u64, rel_e: u64, out: []u8, dest: Dest, flags: Flags, req_offset: u64, req_len: u64) Error!void {
        const bs = self.cfg.block_size;
        const slice_len = rel_e - rel_s;
        const now = clock.monotonicNs();
        // (1) W2 extent runs — NEWER than every base.
        var scratch_cov: ExtentCoverage = .none;
        {
            // Probe coverage without copying yet: the runs are applied after the base.
            scratch_cov = self.write_side.extent_runs(self.write_side.ctx, ino, b, rel_s, rel_e, &.{});
            if (scratch_cov == .full) {
                _ = self.write_side.extent_runs(self.write_side.ctx, ino, b, rel_s, rel_e, out);
                stats.read_copy_dest_bytes.add(out.len);
                return;
            }
        }
        defer if (scratch_cov == .partial) {
            _ = self.write_side.extent_runs(self.write_side.ctx, ino, b, rel_s, rel_e, out);
        };
        // (2) The write side's custody station (a lane: blocking, so `busy`
        //     is never answered here).
        switch (self.write_side.staged_block(self.write_side.ctx, ino, b, rel_s, rel_e, out, false)) {
            .served => |n| {
                @memset(out[n..], 0);
                stats.read_copy_dest_bytes.add(out.len);
                return;
            },
            .none, .busy => {},
        }
        // (3) Key resolve straight out of the live map.
        const key = self.currentBinding(ino, b);
        const device_true = self.cfg.direct_device_true and flags.odirect;
        if (device_true) stats.read_device_true_reads.inc();
        const geometry_aligned = derive.isAligned(rel_s, rsc.LBA_GRAIN) and derive.isAligned(slice_len, rsc.LBA_GRAIN) and slice_len > 0;
        const dest_leaseable = self.cfg.dest_lease and dest.registered and !device_true and geometry_aligned and slice_len < bs and dest.aligned() and self.cfg.passthrough;
        // (4) The pipeline driver — once, BEFORE the serve probes.
        if (!device_true and !flags.lane_pre_fed) {
            const will_wait = if (key) |k| self.flights.inFlight(io, k) else false;
            if (self.pipelineFor(ino, true, now)) |p| {
                _ = p.touch(self.prefetchEnv(ino), .{
                    .offset = req_offset,
                    .len = req_len,
                    .block_size = bs,
                    .start_block = b,
                    .end_block = b,
                    .total_blocks = derive.ceilDiv(info.size, bs),
                    .will_wait = will_wait,
                    .lane_claim = true,
                    .dest_leaseable = dest_leaseable,
                    .level = self.level(),
                    .now = now,
                });
            }
        }
        // (5) HOT tier — refcount hit, binding recheck.
        if (key != null and !device_true) {
            if (self.hot.getServing(key.?, slice_len)) |hit| {
                defer self.hot.unpin(hit);
                @memcpy(out, self.pool.bytes(hit.slot)[@intCast(rel_s)..@intCast(rel_e)]);
                stats.read_copy_dest_bytes.add(out.len);
                stats.read_copy_warm_serve_bytes.add(out.len);
                stats.read_copy_hot_serve_bytes.add(out.len);
                if (self.bindingIs(ino, b, key.?)) {
                    stats.cache_hits.inc();
                    hot_tier.stats.hot_block_hits.inc();
                    if (flags.odirect) stats.read_odirect_tier_serves.inc();
                    if (self.cfg.read_lane) self.hold.credit(key.?, slice_len);
                    return;
                }
                stats.stale_binding_rebinds.inc();
            }
        }
        // (6) The read-lane HOLD — strictly after hot.
        if (self.cfg.read_lane and key != null and !device_true) {
            if (self.hold.serve(key.?, 0)) |sv| {
                defer self.hold.unpin(sv);
                @memcpy(out, self.pool.bytes(sv.slot)[@intCast(rel_s)..@intCast(rel_e)]);
                stats.read_copy_dest_bytes.add(out.len);
                stats.read_copy_warm_serve_bytes.add(out.len);
                stats.read_copy_hold_serve_bytes.add(out.len);
                if (self.bindingIs(ino, b, key.?)) {
                    stats.cache_hits.inc();
                    stats.read_lane_serves.inc();
                    stats.read_lane_serve_bytes.add(out.len);
                    if (flags.odirect) stats.read_odirect_tier_serves.inc();
                    self.hold.credit(key.?, slice_len);
                    if (sv.ledger_visible) self.holdServeAdmission(key.?, sv.slot, sv.len, self.fillClass(ino, device_true, now), now);
                    return;
                }
                stats.stale_binding_rebinds.inc();
            }
        }
        // (7) NVMe read cache — copied out before anything else.
        if (key != null and !device_true) {
            if (self.nvme.getRange(key.?, rel_s, rel_e, out)) |n| {
                @memset(out[n..], 0);
                stats.read_copy_dest_bytes.add(out.len);
                stats.read_copy_warm_serve_bytes.add(out.len);
                stats.read_copy_cache_serve_bytes.add(out.len);
                if (self.bindingIs(ino, b, key.?)) {
                    stats.cache_hits.inc();
                    if (flags.odirect) stats.read_odirect_tier_serves.inc();
                    if (self.cfg.read_lane) self.hold.credit(key.?, slice_len);
                    return;
                }
                stats.stale_binding_rebinds.inc();
            }
        }
        // (9) RANGED dispatch — strictly after the probes missed.
        const lease_admits = dest_leaseable and key != null and !self.flights.inFlight(io, key.?);
        const ranged = key != null and ((device_true and self.cfg.passthrough) or
            (!device_true and rangedEligible(self.cfg.ranged_threshold, slice_len, bs, self.cfg.passthrough, self.streamingFresh(ino, now))) or
            lease_admits);
        if (ranged) {
            const win = Window.of(rel_s, rel_e, bs);
            const direct = geometry_aligned and dest.aligned() and !win.bounced(slice_len);
            return self.rangedRead(io, ino, b, key, rel_s, rel_e, out, direct, lease_admits, device_true, flags.odirect);
        }
        // (10) RAW FULL-BLOCK dest leg: slice == block, passthrough, aligned dest.
        if (key != null and dest.registered and rel_s == 0 and slice_len == bs and self.cfg.passthrough and dest.aligned()) {
            const before = self.fillIncarnation(key.?);
            if (before != null) {
                if (try self.vols.read_range(self.vols.ctx, key.?.vol_tag, key.?.offset, out, .registered) == .dma) {
                    stats.read_dest_dma_bytes.add(bs);
                    stats.read_dma_block_demand_bytes.add(bs);
                }
                if (self.still(key.?, before.?) and self.bindingIs(ino, b, key.?)) {
                    if (!device_true and self.cfg.read_lane) self.hold.credit(key.?, bs);
                    return;
                }
                stats.stale_binding_rebinds.inc();
                // The validated loop below overwrites the dest.
            }
        }
        // (11) The binding-validated WHOLE-BLOCK walk.
        const blk = try self.getBlockForIndex(io, ino, b, key, device_true, true, null);
        if (blk) |bl| {
            defer bl.release(self);
            if (!device_true and self.cfg.read_lane) self.hold.credit(key orelse BlockKey.init(0, 0), slice_len);
            const src = bl.bytes(self);
            const avail = if (rel_s < src.len) @min(rel_e, src.len) - rel_s else 0;
            @memcpy(out[0..@intCast(avail)], src[@intCast(rel_s)..][0..@intCast(avail)]);
            @memset(out[@intCast(avail)..], 0);
            stats.read_copy_dest_bytes.add(out.len);
            stats.read_copy_fill_slice_bytes.add(out.len);
            return;
        }
        // A hole.
        @memset(out, 0);
        stats.read_copy_dest_bytes.add(out.len);
    }

    fn fillClass(self: *ReadPath, ino: u64, device_true: bool, now: u64) FillClass {
        if (!device_true and self.streamingFresh(ino, now)) return .demand_stream;
        return .demand;
    }

    // ---- §3 ranged ----

    fn rangedRead(self: *ReadPath, io: Io, ino: u64, b: u64, key0: ?BlockKey, rel_s: u64, rel_e: u64, out: []u8, direct: bool, lease: bool, device_true: bool, odirect: bool) Error!void {
        const bs = self.cfg.block_size;
        const req_len = rel_e - rel_s;
        const win = Window.of(rel_s, rel_e, bs);
        var key = key0;
        var attempt: u32 = 0;
        while (attempt < RANGED_MAX_REBINDS) : (attempt += 1) {
            const k = key orelse {
                @memset(out, 0);
                stats.read_copy_dest_bytes.add(out.len);
                return;
            };
            if (attempt >= BACKOFF_AFTER) io.sleep(.fromMilliseconds(@as(i64, 1) << @intCast(@min(attempt - BACKOFF_AFTER, 3))), .awake) catch {};
            // The ghost second touch escalates (not device-true, not a lease).
            if (!device_true and !lease and self.adm.rangedEscalation(k, bs, self.level() == .red, odirect, clock.monotonicNs())) {
                const blk = try self.getBlockForIndex(io, ino, b, key, false, true, .escalation);
                if (blk) |bl| {
                    defer bl.release(self);
                    const src = bl.bytes(self);
                    @memcpy(out, src[@intCast(rel_s)..@intCast(rel_e)]);
                    stats.read_copy_dest_bytes.add(out.len);
                    stats.read_copy_fill_slice_bytes.add(out.len);
                    return;
                }
                @memset(out, 0);
                stats.read_copy_dest_bytes.add(out.len);
                return;
            }
            // The device leg refuses a dead-lifetime key BEFORE the DMA.
            const before = self.fillIncarnation(k) orelse {
                stats.stale_binding_rebinds.inc();
                stats.ranged_read_rebinds.inc();
                key = self.currentBinding(ino, b);
                continue;
            };
            stats.ranged_reads.inc();
            stats.ranged_read_bytes.add(win.len());
            self.adm.governor.noteForeground(win.len(), clock.monotonicNs());
            if (win.bounced(req_len)) stats.ranged_read_unaligned_bounces.inc();
            if (direct) {
                if (try self.vols.read_range(self.vols.ctx, k.vol_tag, k.offset + win.aligned_start, out, .registered) == .dma) {
                    stats.read_dest_dma_bytes.add(win.len());
                    stats.read_dma_ranged_bytes.add(win.len());
                }
            }
            var bounce: ?Bounce = null;
            defer if (bounce) |bb| self.releaseBounce(bb);
            if (!direct) {
                const bb = try self.acquireBounce(win.len());
                bounce = bb;
                if (try self.vols.read_range(self.vols.ctx, k.vol_tag, k.offset + win.aligned_start, bb.bytes[0..@intCast(win.len())], .pooled) == .dma) {
                    stats.read_fill_dma_bytes.add(win.len());
                    stats.read_dma_ranged_bytes.add(win.len());
                }
            }
            if (self.seams.after_dma) |f| f(self.seams.after_dma_ctx, k);
            // Bytes in hand FIRST, then the two checks.
            const incarnation_ok = self.still(k, before);
            const current = self.currentBinding(ino, b);
            if (incarnation_ok and current != null and current.?.eql(k)) {
                if (direct) {
                    if (lease) stats.read_dest_lease_bytes.add(req_len);
                } else {
                    const from: usize = @intCast(rel_s - win.aligned_start);
                    @memcpy(out, bounce.?.bytes[from..][0..out.len]);
                    stats.read_copy_dest_bytes.add(out.len);
                }
                return;
            }
            stats.stale_binding_rebinds.inc();
            stats.ranged_read_rebinds.inc();
            key = current;
        }
        // Exhaustion ⇒ the whole-block validated walk, then the slice.
        const blk = try self.getBlockForIndex(io, ino, b, key, device_true, true, null);
        if (blk) |bl| {
            defer bl.release(self);
            const src = bl.bytes(self);
            @memcpy(out, src[@intCast(rel_s)..@intCast(rel_e)]);
            stats.read_copy_dest_bytes.add(out.len);
            stats.read_copy_fill_slice_bytes.add(out.len);
            return;
        }
        @memset(out, 0);
        stats.read_copy_dest_bytes.add(out.len);
    }

    // ---- §7.2 the binding-validated walk ----

    /// Bytes for `b` under a validated fill AND the current binding; a
    /// rebind is a loss (re-resolve, retry); past `CONTENDED_BEFORE_ESCALATE`
    /// losses the fetch goes device-direct; exhaustion ⇒ the settle. Null =
    /// a fresh-map hole.
    pub fn getBlockForIndex(self: *ReadPath, io: Io, ino: u64, b: u64, resolved: ?BlockKey, device_true: bool, escalate_contended: bool, class_override: ?FillClass) Error!?Block {
        const ladder_cap = if (escalate_contended) SETTLE_HANDOFF else self.cfg.rebind_ladder_cap;
        const now = clock.monotonicNs();
        const class: FillClass = class_override orelse self.fillClass(ino, device_true, now);
        var key = resolved orelse self.currentBinding(ino, b);
        var losses: u32 = 0;
        var attempt: u32 = 0;
        while (attempt < ladder_cap) : (attempt += 1) {
            const k = key orelse return null;
            if (attempt >= BACKOFF_AFTER) io.sleep(.fromMilliseconds(@as(i64, 1) << @intCast(@min(attempt - BACKOFF_AFTER, 3))), .awake) catch {};
            const escalated = escalate_contended and losses >= CONTENDED_BEFORE_ESCALATE;
            const fetched = (if (device_true or escalated) self.fetchBlockDeviceTrue(k) else self.getCachedOrFetch(io, k, class)) catch |e| {
                if (self.bindingIs(ino, b, k)) return e;
                // A dead incarnation fails AEAD/LZ4 on transformed volumes — a LOSS.
                losses += 1;
                stats.stale_binding_rebinds.inc();
                key = self.currentBinding(ino, b);
                continue;
            };
            if (fetched.serve_valid and self.bindingIs(ino, b, k)) return fetched;
            fetched.release(self);
            losses += 1;
            stats.stale_binding_rebinds.inc();
            key = self.currentBinding(ino, b);
        }
        if (!escalate_contended) {
            log.err("read of ino {d} block {d}: did not settle after {d} binding rebinds", .{ ino, b, ladder_cap });
            return error.InputOutput;
        }
        return self.getBlockSettled(io, ino, b);
    }

    /// §7.3: ONE serialized settle under the inode's meta stripe (the one
    /// domain every map publish funnels through); retries exist only for
    /// the invariant-violation case.
    fn getBlockSettled(self: *ReadPath, io: Io, ino: u64, b: u64) Error!?Block {
        stats.stale_binding_escalations.inc();
        var attempt: u32 = 0;
        while (attempt < SETTLE_ATTEMPTS) : (attempt += 1) {
            if (attempt > 0) io.sleep(.fromMilliseconds(@as(i64, 1) << @intCast(@min(attempt - 1, 3))), .awake) catch {};
            const guard = self.settle.acquire(io, ino);
            defer guard.release(io);
            const k = self.currentBinding(ino, b) orelse return null;
            const fetched = try self.fetchBlockDeviceTrue(k);
            if (fetched.serve_valid and self.bindingIs(ino, b, k)) return fetched;
            fetched.release(self);
            stats.read_settle_lost_serialized.fire("a binding moved under the settle's stripe — a mutator outside both lock disciplines");
        }
        log.err("read of ino {d} block {d}: did not settle after {d} serialized settle attempts — see invariant_tripwires", .{ ino, b, SETTLE_ATTEMPTS });
        return error.InputOutput;
    }

    // ---- the fills ----

    /// The device-true fetch: no tiers, no single-flight, no ghost, no
    /// `cache_hits/misses`; decode still applies; the same verdict shape.
    /// Counts into the fill phases (`fill_total` covers every fill).
    fn fetchBlockDeviceTrue(self: *ReadPath, key: BlockKey) Error!Block {
        var pc = PhaseClock.start();
        const before = self.fillIncarnation(key);
        var blk = try self.fetchIntoFresh(key, .demand, &pc);
        blk.serve_valid = if (before) |bf| self.still(key, bf) else false;
        pc.mark(&stats.read_fill_admission_ns);
        pc.mark(&stats.read_fill_deposit_ns);
        pc.total(&stats.read_fill_total_ns);
        return blk;
    }

    /// DMA (and decode) a whole block into a fresh pool slot — or an
    /// over-capacity bounce the tiers never adopt. Phases on the caller's
    /// clock (span form); the device bytes land in the ledger arm `class`
    /// names (a transformed image is its own arm whatever the class).
    fn fetchIntoFresh(self: *ReadPath, key: BlockKey, class: FillClass, pc: *PhaseClock) Error!Block {
        const bs: usize = @intCast(self.cfg.block_size);
        var blk: Block = if (self.pool.acquire()) |slot| .{ .where = .pool, .slot = slot, .len = @intCast(bs), .serve_valid = false } else blk: {
            const b = self.gpa.alignedAlloc(u8, .fromByteUnits(fill_pool.SLOT_ALIGN), self.pool.slot_size) catch return error.OutOfMemory;
            stats.read_copy_bounce_bytes.add(bs);
            break :blk .{ .where = .bounce, .bounce = b, .len = @intCast(bs), .serve_valid = false };
        };
        errdefer blk.release(self);
        const target: []align(fill_pool.SLOT_ALIGN) u8 = switch (blk.where) {
            .pool => self.pool.bytes(blk.slot),
            .bounce => blk.bounce,
            .lru => unreachable,
        };
        if (self.decode) |d| {
            // The stored image into a second slot, decoded into the target.
            const img = try self.acquireBounce(d.chunk);
            defer self.releaseBounce(img);
            if (try self.vols.read_range(self.vols.ctx, key.vol_tag, key.offset, img.bytes[0..@intCast(d.chunk)], .pooled) == .dma) {
                stats.read_fill_dma_bytes.add(d.chunk);
                stats.read_dma_image_bytes.add(d.chunk);
            }
            if (self.seams.after_dma) |f| f(self.seams.after_dma_ctx, key);
            pc.mark(&stats.read_fill_fetch_dma_ns);
            const view = d.decode(d.ctx, img.bytes[0..@intCast(d.chunk)], target) catch return error.InputOutput;
            if (view.ptr != target.ptr) @memcpy(target[0..view.len], view);
            blk.len = @intCast(view.len);
            pc.mark(&stats.read_fill_decode_ns);
        } else {
            if (try self.vols.read_range(self.vols.ctx, key.vol_tag, key.offset, target[0..bs], .pooled) == .dma) {
                stats.read_fill_dma_bytes.add(bs);
                (switch (class) {
                    .prefetch => &stats.read_dma_block_prefetch_bytes,
                    .lane => &stats.read_dma_block_lane_bytes,
                    .demand, .demand_stream, .escalation => &stats.read_dma_block_demand_bytes,
                }).add(bs);
            }
            if (self.seams.after_dma) |f| f(self.seams.after_dma_ctx, key);
            pc.mark(&stats.read_fill_fetch_dma_ns);
            pc.mark(&stats.read_fill_decode_ns);
        }
        return blk;
    }

    /// §4 `get_cached_or_fetch_block`: the tier probes (no RAM re-promote
    /// of an NVMe hit), then the result-carrying single-flight fill. The
    /// PRIMARY re-probes after claiming its flight: a whole sibling flight
    /// fits in the probe→enter window (probe miss → preempted → the
    /// sibling fills, deposits, publishes, closes → this caller claims a
    /// fresh flight for a block the hot tier already holds), and without
    /// the re-probe that was a second device fetch of a resident block
    /// (the cohort test's one-in-forty under load).
    fn getCachedOrFetch(self: *ReadPath, io: Io, key: BlockKey, class: FillClass) Error!Block {
        if (self.probeTiers(key)) |hit| return hit;
        if (self.seams.before_flight) |f| f(self.seams.before_flight_ctx, key);
        switch (self.flights.enter(io, key)) {
            .result => |r| {
                const slot = r.slot orelse return self.fetchBlockDeviceTrue(key);
                const valid = r.serve_valid and (r.verdict_word == null or self.still(key, r.verdict_word.?));
                return .{ .where = .pool, .slot = slot, .len = r.len, .serve_valid = valid };
            },
            .table_full => return self.fetchBlockDeviceTrue(key),
            .primary => |g| {
                if (self.probeTiers(key)) |hit| {
                    stats.read_fill_reprobe_hits.inc();
                    // A pool-backed hit carries to the cohort like a fill's
                    // result; an LRU hit (its own slab) dissolves the flight
                    // — the elected front waiter re-probes and hits it too.
                    if (hit.where == .pool) {
                        g.publish(io, .{ .slot = hit.slot, .len = hit.len, .serve_valid = true, .verdict_word = null });
                        g.release(io);
                    } else {
                        g.fail(io);
                    }
                    return hit;
                }
                return self.fillAsPrimary(io, g, key, class);
            },
        }
    }

    /// The tier probes in ladder order: hot (no promote) → read LRU →
    /// the read-lane hold → the NVMe tier (bounced into a serve slot). A
    /// hit carries one reference for the caller.
    fn probeTiers(self: *ReadPath, key: BlockKey) ?Block {
        if (self.hot.getNoPromote(key)) |hit| {
            stats.cache_hits.inc();
            hot_tier.stats.hot_block_hits.inc();
            return .{ .where = .pool, .slot = hit.slot, .len = hit.len, .serve_valid = true };
        }
        if (self.lru.get(key)) |v| {
            stats.cache_hits.inc();
            return .{ .where = .lru, .slot = v.slot, .len = v.len, .serve_valid = true };
        }
        if (self.cfg.read_lane) {
            if (self.hold.serve(key, 0)) |sv| {
                stats.cache_hits.inc();
                stats.read_lane_serves.inc();
                return .{ .where = .pool, .slot = sv.slot, .len = sv.len, .serve_valid = true, .hold_ledger_visible = sv.ledger_visible };
            }
        }
        if (self.nvme.has(key)) {
            // Copied into a serve buffer, NEVER into a RAM tier (the no-RAM-
            // repromote rule: the incarnation word cannot prove ENTRY
            // provenance of a tier entry from a dying incarnation).
            if (self.pool.acquire()) |slot| {
                const dst = self.pool.bytes(slot);
                if (self.nvme.getRange(key, 0, self.cfg.block_size, dst)) |n| {
                    stats.cache_hits.inc();
                    stats.read_copy_bounce_bytes.add(n);
                    return .{ .where = .pool, .slot = slot, .len = @intCast(n), .serve_valid = true };
                }
                _ = self.pool.unpin(slot);
            }
        }
        return null;
    }

    /// The primary's fill: validated before/during/after, deposited by
    /// class, published to the cohort, the deferred NVMe publish riding
    /// the guard.
    fn fillAsPrimary(self: *ReadPath, io: Io, g: single_flight.FlightTable(MAX_FLIGHTS).Guard, key: BlockKey, class: FillClass) Error!Block {
        stats.cache_misses.inc();
        var pc = PhaseClock.start();
        const before = self.fillIncarnation(key);
        var blk = self.fetchIntoFresh(key, class, &pc) catch |e| {
            g.fail(io);
            return e;
        };
        if (class == .lane) {
            prefetch.stats.read_lane_fetches.inc();
            prefetch.stats.read_lane_fetch_bytes.add(blk.len);
        }
        const is_tracked = self.tracked(key);
        var serve_valid = !is_tracked;
        var publishable: ?u64 = null;
        if (before) |bf| if (self.still(key, bf)) {
            publishable = bf;
        };
        var publish_to_nvme = false;
        var still_ctx: StillCtx = .{ .rp = self, .key = key, .before = publishable orelse 0 };
        const validate: Validate = .{ .ctx = &still_ctx, .call = &stillValidate };
        const now = clock.monotonicNs();
        if (publishable != null and blk.where == .pool) {
            const len: u64 = blk.len;
            if (class == .demand_stream) self.adm.governor.noteForeground(len, now);
            const decision = self.adm.decideFill(key, len, class, now);
            pc.mark(&stats.read_fill_admission_ns);
            const lvl = self.level();
            const freeze = lvl != .green;
            // The hold: demand primaries deposit ledger-visible (the serve
            // ceremony stands in for the refetch), the lane's fetch
            // ledger-INVISIBLE (`insert_validated`); prefetch never.
            if (self.cfg.read_lane and class != .prefetch and !rsc.isSmallFill(len) and lvl != .red) {
                _ = self.hold.insert(key, blk.slot, blk.len, class != .lane, validate);
            }
            if (rsc.isSmallFill(len)) {
                _ = self.lru.putValidated(key, blk.bytes(self), validate, freeze);
            } else {
                hot_tier.stats.hot_block_misses.inc();
                _ = self.hot.putClassValidated(key, blk.slot, blk.len, decision.hot_class, validate, freeze);
            }
            serve_valid = self.still(key, publishable.?);
            if (!serve_valid) {
                _ = purge.purgeBlockKey(self.tiers, key);
            } else {
                publish_to_nvme = decision.publish;
            }
            pc.mark(&stats.read_fill_deposit_ns);
        } else {
            pc.mark(&stats.read_fill_admission_ns);
            pc.mark(&stats.read_fill_deposit_ns);
        }
        blk.serve_valid = serve_valid;
        const verdict: ?u64 = if (serve_valid) publishable else null;
        // The cohort is served from the carried result; the publish rides.
        if (publish_to_nvme) g.hold(io);
        g.publish(io, .{ .slot = if (blk.where == .pool) blk.slot else null, .len = blk.len, .serve_valid = serve_valid, .verdict_word = verdict });
        g.release(io);
        if (publish_to_nvme) {
            defer g.release(io);
            if (self.still(key, publishable.?)) {
                _ = self.nvme.putValidated(key, blk.bytes(self), publishable.?, validate, self.publishPaused());
            }
        }
        pc.total(&stats.read_fill_total_ns);
        self.noteDelivered(key, blk.len);
        return blk;
    }

    fn noteDelivered(self: *ReadPath, _: BlockKey, len: u64) void {
        // Every pipeline's governor sees total delivery; the per-ino table
        // is small, so the walk is a few loads.
        for (&self.pipelines.pipes, &self.pipelines.inos) |*p, ino| if (ino != 0) p.probe.noteDelivered(len);
    }

    /// §6.5 the hold-serve ceremony: a serve of a DEMAND deposit after every
    /// tier missed stands in for the refetch the pre-lane read would have
    /// paid, so it runs the admission arm — no `noteForeground` (zero device
    /// bytes), no re-deposit.
    fn holdServeAdmission(self: *ReadPath, key: BlockKey, slot: u32, len: u32, class: FillClass, now: u64) void {
        const before = self.fillIncarnation(key) orelse return;
        if (!self.still(key, before)) return;
        stats.hold_serve_admissions.inc();
        const decision = self.adm.decideFill(key, len, class, now);
        var still_ctx: StillCtx = .{ .rp = self, .key = key, .before = before };
        const validate: Validate = .{ .ctx = &still_ctx, .call = &stillValidate };
        if (decision.publish) _ = self.nvme.putValidated(key, self.pool.bytes(slot)[0..len], before, validate, self.publishPaused());
        hot_tier.stats.hot_block_misses.inc();
        _ = self.hot.putClassValidated(key, slot, len, decision.hot_class, validate, self.level() != .green);
        if (!self.still(key, before)) _ = purge.purgeBlockKey(self.tiers, key);
    }

    // ---- the dehydration sinks ----

    fn hotVictim(ctx: ?*anyopaque, v: hot_tier.Victim) void {
        const self: *ReadPath = @ptrCast(@alignCast(ctx.?));
        self.adm.onHotVictim(v);
        if (v.class != .protected) return;
        if (self.level() != .green) {
            stats.mem_budget_dehydrate_paused.inc();
            return;
        }
        if (self.nvme.has(v.key)) {
            hot_tier.stats.hot_block_dehydrate_skips.inc();
            return;
        }
        self.dehydrate(v.key, self.pool.bytes(v.slot)[0..v.len]);
    }

    fn lruVictim(ctx: ?*anyopaque, v: hot_tier.Victim) void {
        const self: *ReadPath = @ptrCast(@alignCast(ctx.?));
        if (self.level() != .green) {
            stats.mem_budget_dehydrate_paused.inc();
            return;
        }
        self.dehydrate(v.key, self.lru.slab.bytes(v.slot)[0..v.len]);
    }

    /// A validated publish of a victim: the CURRENT stable word is the
    /// fill's (a retire would have purged the entry through the ONE purge).
    fn dehydrate(self: *ReadPath, key: BlockKey, data: []const u8) void {
        const before = self.fillIncarnation(key) orelse return;
        var still_ctx: StillCtx = .{ .rp = self, .key = key, .before = before };
        _ = self.nvme.putValidated(key, data, before, .{ .ctx = &still_ctx, .call = &stillValidate }, self.publishPaused());
    }

    // ---- the prefetch Env ----

    const EnvCtx = struct { rp: *ReadPath, ino: u64 };

    fn prefetchEnv(self: *ReadPath, ino: u64) prefetch.Env {
        // The Env is consumed synchronously inside `touch`; the ctx lives on
        // the caller's frame for that call.
        const S = struct {
            threadlocal var ctx: EnvCtx = undefined;
        };
        S.ctx = .{ .rp = self, .ino = ino };
        return .{ .ctx = &S.ctx, .resident = &envResident, .issue_prefetch = &envIssuePrefetch, .issue_lane = &envIssueLane };
    }

    fn envResident(ctx: *anyopaque, block: u64) bool {
        const c: *EnvCtx = @ptrCast(@alignCast(ctx));
        const key = c.rp.currentBinding(c.ino, block) orelse return true;
        return c.rp.hot.touchNoPromote(key) or c.rp.lru.touchNoPromote(key) or c.rp.nvme.has(key) or (c.rp.cfg.read_lane and c.rp.hold.contains(key));
    }

    /// Speculative fills are issued through the `Issuer` the mount installs
    /// (a lane fiber around `warm`); without one the pipeline's plan is
    /// bookkept and settled as wasted — the demand path is never blocked by
    /// speculation.
    fn envIssuePrefetch(ctx: *anyopaque, _: u8, _: u32, block: u64) bool {
        const c: *EnvCtx = @ptrCast(@alignCast(ctx));
        const is = c.rp.issuer orelse return false;
        return is.issue(is.ctx, c.ino, block, .prefetch);
    }

    fn envIssueLane(ctx: *anyopaque, _: u8, _: u32, block: u64) bool {
        const c: *EnvCtx = @ptrCast(@alignCast(ctx));
        const is = c.rp.issuer orelse return false;
        return is.issue(is.ctx, c.ino, block, .lane);
    }

    /// The non-serving warmer the prefetch/lane tasks run: a fill whose
    /// result is dropped (legal — a warmer serves no one).
    pub fn warm(self: *ReadPath, io: Io, ino: u64, block: u64, class: FillClass) void {
        const key = self.currentBinding(ino, block) orelse return;
        var env_ctx: EnvCtx = .{ .rp = self, .ino = ino };
        if (envResident(@ptrCast(&env_ctx), block)) return;
        const blk = self.getCachedOrFetch(io, key, class) catch return;
        blk.release(self);
    }

    /// The hold-serve ceremony DEFERRED from the warm ladder's probe (the
    /// queue thread may not publish — a 4 MiB memcpy into the mmap tier can
    /// fault): re-serve the hold entry on the lane and run the admission iff
    /// the deposit is still ledger-visible.
    pub fn holdCeremony(self: *ReadPath, key: BlockKey, class: FillClass) void {
        const sv = self.hold.serve(key, 0) orelse return;
        defer self.hold.unpin(sv);
        if (!sv.ledger_visible) return;
        self.holdServeAdmission(key, sv.slot, sv.len, class, clock.monotonicNs());
    }
};

/// The speculative-fill issuer the mount installs: spawn `warm(ino, block,
/// class)` on a handler lane; false = refused (the pipeline rolls back).
pub const Issuer = struct {
    ctx: ?*anyopaque,
    issue: *const fn (ctx: ?*anyopaque, ino: u64, block: u64, class: FillClass) bool,
};

// ---------------------------------------------------------------------------
// Tests — the portable fixture: a RAM device per volume behind `Volumes`,
// a map double behind `Meta`, the incarnation words in a real allocator.
// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("../sync/testing.zig");

/// The portable fixture, shared with the warm ladder's tests.
pub const TestFixture = if (builtin.is_test) Fixture else @compileError("TestFixture is a test double");

const Fixture = struct {
    pub const bs: u64 = 1 << 20;
    pub const vol_tag: u64 = 0x7001;
    const capacity: u64 = 16 * bs;
    const MaxBlocks = 8;

    gpa: Allocator,
    device: []align(4096) u8,
    alloc: *block_alloc.BlockAllocator,
    size: u64 = 0,
    kind: layout.Kind = .striped,
    bindings: [MaxBlocks]?Mapping = @splat(null),
    inline_data: []const u8 = &.{},
    device_reads: Atomic(u64) = .init(0),
    device_read_bytes: Atomic(u64) = .init(0),
    fail_reads: Atomic(u32) = .init(0),
    rp: *ReadPath = undefined,

    pub fn init(gpa: Allocator, cfg_over: struct { passthrough: bool = true, ranged_threshold: u64 = 256 * derive.KiB, direct_device_true: bool = false, read_lane: bool = true, rebind_cap: u32 = MAX_REBINDS, hot_budget: u64 = 4 * bs, policy: admission.Policy = .second_touch }) !*Fixture {
        const self = try gpa.create(Fixture);
        errdefer gpa.destroy(self);
        const device = try gpa.alignedAlloc(u8, .fromByteUnits(4096), @intCast(capacity));
        errdefer gpa.free(device);
        for (device, 0..) |*b, i| b.* = @truncate((i / 4096) * 31 + i);
        const alloc = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = vol_tag, .vol = 0, .block_size = bs, .capacity_bytes = capacity });
        errdefer alloc.deinit();
        self.* = .{ .gpa = gpa, .device = device, .alloc = alloc };
        const cfg: Config = .{
            .block_size = bs,
            .passthrough = cfg_over.passthrough,
            .ranged_threshold = cfg_over.ranged_threshold,
            .dest_lease = true,
            .read_lane = cfg_over.read_lane,
            .direct_device_true = cfg_over.direct_device_true,
            .rebind_ladder_cap = cfg_over.rebind_cap,
            .cores = 1,
            .hot_budget = cfg_over.hot_budget,
            .read_mem_limit = 4 * rsc.SMALL_FILL_MAX,
            .mem_budget = 64 * bs,
            .admission_policy = cfg_over.policy,
            .fill_pct = 5,
            .prefetch = .{ .window_cap = 8, .share_pct = 50, .hot_budget = cfg_over.hot_budget, .lane_armed = cfg_over.read_lane, .depth_pin = null, .lane_budget_cap = 8 * bs },
        };
        const nvme = try nvme_read_cache.Cache.initRam(gpa, 1, 8 * bs);
        self.rp = try ReadPath.init(gpa, cfg, self.meta(), self.vols(), WriteSide.none, null, nvme, null);
        return self;
    }

    pub fn deinit(self: *Fixture) void {
        self.rp.deinit();
        self.alloc.deinit();
        self.gpa.free(self.device);
        self.gpa.destroy(self);
    }

    /// Bind block `b` to device block `dev_idx` (published stable).
    pub fn bind(self: *Fixture, b: usize, dev_idx: u64) void {
        self.bindings[b] = Mapping.wholeBlock(vol_tag, dev_idx * bs, 1);
        self.alloc.publishBlock(dev_idx);
    }

    pub fn expected(dev_off: u64, out: []const u8) !void {
        for (out, 0..) |got, i| {
            const abs = dev_off + i;
            const want: u8 = @truncate((abs / 4096) * 31 + abs);
            if (got != want) {
                std.debug.print("byte {d} (dev {d}): got {d} want {d}\n", .{ i, abs, got, want });
                return error.TestExpectedEqual;
            }
        }
    }

    // Meta seam.
    fn metaFile(ctx: *anyopaque, _: u64) ?FileInfo {
        const self: *Fixture = @ptrCast(@alignCast(ctx));
        return .{ .size = self.size, .kind = self.kind, .durable_copy = if (self.kind == .packed_) self.bindings[0] else null };
    }
    fn metaBinding(ctx: *anyopaque, _: u64, block: u64) ?Mapping {
        const self: *Fixture = @ptrCast(@alignCast(ctx));
        if (block >= MaxBlocks) return null;
        return self.bindings[block];
    }
    fn metaInline(ctx: *anyopaque, _: u64, offset: u64, out: []u8) usize {
        const self: *Fixture = @ptrCast(@alignCast(ctx));
        if (offset >= self.inline_data.len) return 0;
        const n = @min(out.len, self.inline_data.len - offset);
        @memcpy(out[0..n], self.inline_data[offset..][0..n]);
        return n;
    }
    fn meta(self: *Fixture) Meta {
        return .{ .ctx = self, .file = &metaFile, .binding = &metaBinding, .inline_data = &metaInline };
    }

    // Volumes seam.
    fn volAlloc(ctx: *anyopaque, tag: u64) ?*block_alloc.BlockAllocator {
        const self: *Fixture = @ptrCast(@alignCast(ctx));
        return if (tag == vol_tag) self.alloc else null;
    }
    fn volRead(ctx: *anyopaque, tag: u64, device_off: u64, dest: []u8, _: DestShape) DeviceError!Served {
        const self: *Fixture = @ptrCast(@alignCast(ctx));
        assert(tag == vol_tag);
        assert(derive.isAligned(device_off, 4096) and derive.isAligned(dest.len, 4096) and derive.isAligned(@intFromPtr(dest.ptr), 4096));
        if (self.fail_reads.load(.monotonic) > 0) {
            _ = self.fail_reads.fetchSub(1, .monotonic);
            return error.InputOutput;
        }
        _ = self.device_reads.fetchAdd(1, .monotonic);
        _ = self.device_read_bytes.fetchAdd(dest.len, .monotonic);
        @memcpy(dest, self.device[@intCast(device_off)..][0..dest.len]);
        return .dma;
    }
    fn volFd(_: *anyopaque, _: u64) ?std.os.linux.fd_t {
        return null;
    }
    fn vols(self: *Fixture) Volumes {
        return .{ .ctx = self, .allocator_of = &volAlloc, .read_range = &volRead, .device_fd = &volFd };
    }
};

fn alignedBuf(gpa: Allocator, n: usize) ![]align(4096) u8 {
    return gpa.alignedAlloc(u8, .fromByteUnits(4096), n);
}

test "cold whole-block read fills ONCE (hot probation, publish skipped), the second read is a hot hit, the ghost second touch publishes; purge empties every tier; ledger closes" {
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.size = 3 * Fixture.bs;
    f.bind(0, 2);
    f.bind(1, 5);
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    const io = testing.io;
    const user0 = stats.read_user_bytes.load();
    const dest0 = stats.read_copy_dest_bytes.load();
    const dma0 = stats.read_dest_dma_bytes.load();
    const bounce0 = stats.read_copy_bounce_bytes.load();
    const warm0 = stats.read_copy_warm_serve_bytes.load();
    const hot0 = stats.read_copy_hot_serve_bytes.load();
    const hold0 = stats.read_copy_hold_serve_bytes.load();
    const cache0 = stats.read_copy_cache_serve_bytes.load();
    const skipped0 = admission.stats.read_fill_publishes_skipped.load();
    // Cold: the whole block (1 MiB > 256 KiB) — a single-flight fill.
    const n = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n);
    try Fixture.expected(2 * Fixture.bs, buf);
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
    try testing.expect(f.rp.hot.contains(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    try testing.expectEqual(skipped0 + 1, admission.stats.read_fill_publishes_skipped.load());
    try testing.expect(!f.rp.nvme.has(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    // Warm: a sub-read is a hot hit, no device read.
    const n2 = try f.rp.read(io, 7, 4096, 8192, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 8192), n2);
    try Fixture.expected(2 * Fixture.bs + 4096, buf[0..8192]);
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
    try testing.expectEqual(hot0 + 8192, stats.read_copy_hot_serve_bytes.load());
    // Purge: every tier drops it; the next read is cold again (ghost SECOND
    // touch ⇒ protected + published to the NVMe tier).
    try testing.expect(purge.purgeBlockKey(f.rp.tiers, .init(Fixture.vol_tag, 2 * Fixture.bs)) >= 1);
    try testing.expect(!f.rp.hot.contains(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    _ = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(u64, 2), f.device_reads.load(.monotonic));
    try testing.expect(f.rp.nvme.has(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    try testing.expect(f.rp.hot.cache.bitsOf(.init(Fixture.vol_tag, 2 * Fixture.bs)).?.protected);
    // Purge the RAM tiers only: the NVMe tier serves the sub-read (no RAM re-promote).
    try testing.expect(f.rp.hot.purge(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    _ = f.rp.hold.purge(.init(Fixture.vol_tag, 2 * Fixture.bs));
    const n3 = try f.rp.read(io, 7, 12288, 4096, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 4096), n3);
    try Fixture.expected(2 * Fixture.bs + 12288, buf[0..4096]);
    try testing.expectEqual(@as(u64, 2), f.device_reads.load(.monotonic));
    try testing.expectEqual(cache0 + 4096, stats.read_copy_cache_serve_bytes.load());
    try testing.expect(!f.rp.hot.contains(.init(Fixture.vol_tag, 2 * Fixture.bs)));
    // A multi-block read assembles across blocks; EOF clamps.
    const n4 = try f.rp.read(io, 7, Fixture.bs - 4096, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n4);
    try Fixture.expected(2 * Fixture.bs + Fixture.bs - 4096, buf[0..4096]);
    try Fixture.expected(5 * Fixture.bs, buf[4096..]);
    try testing.expectEqual(@as(usize, 0), try f.rp.read(io, 7, 3 * Fixture.bs, 10, .{ .buf = buf }, .{}));
    try testing.expectEqual(@as(usize, 4096), try f.rp.read(io, 7, 3 * Fixture.bs - 4096, 1 << 20, .{ .buf = buf }, .{}));
    // The ledger closes: dest + bounce + dest_dma + zc ≡ user bytes; warm ≡ hot + hold + cache.
    const user = stats.read_user_bytes.load() - user0;
    const dest = stats.read_copy_dest_bytes.load() - dest0;
    const dma = stats.read_dest_dma_bytes.load() - dma0;
    try testing.expectEqual(user, dest + dma);
    try testing.expectEqual(bounce0, stats.read_copy_bounce_bytes.load());
    const warm = stats.read_copy_warm_serve_bytes.load() - warm0;
    try testing.expectEqual(warm, (stats.read_copy_hot_serve_bytes.load() - hot0) + (stats.read_copy_hold_serve_bytes.load() - hold0) + (stats.read_copy_cache_serve_bytes.load() - cache0));
    // Every serve pin released: the pool holds only tier entries (a slot
    // may sit in the hot tier AND the hold), and emptying the tiers frees
    // every slot.
    const held = f.rp.pool.checkedOutBytes() / f.rp.pool.slot_size;
    try testing.expect(held <= f.rp.hot.cache.liveEntries() + f.rp.hold.liveCount());
    f.rp.hot.shedTo(0);
    f.rp.hold.trimTo(0);
    try testing.expectEqual(@as(usize, f.rp.pool.capacity), f.rp.pool.freeSlots());
    // The fill phases sum exactly to the total.
    const s = stats.read_fill_total_ns.snapshot();
    const sum = stats.read_fill_fetch_dma_ns.snapshot().sum_ns + stats.read_fill_decode_ns.snapshot().sum_ns + stats.read_fill_admission_ns.snapshot().sum_ns + stats.read_fill_deposit_ns.snapshot().sum_ns;
    try testing.expectEqual(s.sum_ns, sum);
}

test "the ranged-vs-whole decision table: a random 4 KiB cold read on a passthrough volume is a ranged window into an aligned dest (the lease) and is never published; an unaligned dest bounces; a stream-classified reader takes whole blocks; threshold 0 kills ranging; a transform volume never ranges" {
    const io = testing.io;
    {
        const f = try Fixture.init(testing.allocator, .{});
        defer f.deinit();
        f.size = 2 * Fixture.bs;
        f.bind(0, 3);
        const buf = try alignedBuf(testing.allocator, 1 << 20);
        defer testing.allocator.free(buf);
        const lease0 = stats.read_dest_lease_bytes.load();
        const dma0 = stats.read_dest_dma_bytes.load();
        const ranged0 = stats.ranged_reads.load();
        // Registered, aligned dest: the lease — 4 KiB of device bytes straight in.
        const n = try f.rp.read(io, 7, 8192, 4096, .{ .buf = buf[0..4096], .registered = true }, .{});
        try testing.expectEqual(@as(usize, 4096), n);
        try Fixture.expected(3 * Fixture.bs + 8192, buf[0..4096]);
        try testing.expectEqual(@as(u64, 4096), f.device_read_bytes.load(.monotonic));
        try testing.expectEqual(lease0 + 4096, stats.read_dest_lease_bytes.load());
        try testing.expectEqual(dma0 + 4096, stats.read_dest_dma_bytes.load());
        try testing.expectEqual(ranged0 + 1, stats.ranged_reads.load());
        const key = BlockKey.init(Fixture.vol_tag, 3 * Fixture.bs);
        try testing.expect(!f.rp.hot.contains(key) and !f.rp.nvme.has(key) and !f.rp.hold.contains(key));
        // An unaligned request (odd offset) into an unregistered dest: the
        // window is 4 KiB-outward and bounced; the request bytes are copied.
        const bounces0 = stats.ranged_read_unaligned_bounces.load();
        const n2 = try f.rp.read(io, 7, 4097, 100, .{ .buf = buf[0..100] }, .{});
        try testing.expectEqual(@as(usize, 100), n2);
        try Fixture.expected(3 * Fixture.bs + 4097, buf[0..100]);
        try testing.expectEqual(bounces0 + 1, stats.ranged_read_unaligned_bounces.load());
        try testing.expectEqual(@as(u64, 8192), f.device_read_bytes.load(.monotonic));
        // The ghost second touch of a ranged key escalates to ONE admitted whole-block fetch.
        const esc0 = admission.stats.ranged_read_ghost_escalations.load();
        const n3 = try f.rp.read(io, 7, 16384, 4096, .{ .buf = buf[0..4096] }, .{});
        try testing.expectEqual(@as(usize, 4096), n3);
        try Fixture.expected(3 * Fixture.bs + 16384, buf[0..4096]);
        try testing.expectEqual(esc0 + 1, admission.stats.ranged_read_ghost_escalations.load());
        try testing.expect(f.rp.hot.contains(key));
        try testing.expect(f.rp.nvme.has(key));
        try testing.expectEqual(@as(u64, 8192 + Fixture.bs), f.device_read_bytes.load(.monotonic));
    }
    {
        // Streaming: 4 contiguous 1 MiB-block sub-reads classify; the 5th
        // random-sized request still takes a whole block (the ranged veto).
        const f = try Fixture.init(testing.allocator, .{});
        defer f.deinit();
        f.size = 8 * Fixture.bs;
        for (0..8) |i| f.bind(i, @intCast(i + 4));
        const buf = try alignedBuf(testing.allocator, 1 << 20);
        defer testing.allocator.free(buf);
        const classified0 = prefetch.stats.read_streams_classified.load();
        for (0..4) |i| {
            const n = try f.rp.read(io, 9, @as(u64, i) * Fixture.bs, Fixture.bs, .{ .buf = buf }, .{});
            try testing.expectEqual(@as(usize, 1 << 20), n);
            try Fixture.expected((i + 4) * Fixture.bs, buf);
        }
        try testing.expectEqual(classified0 + 1, prefetch.stats.read_streams_classified.load());
        try testing.expect(f.rp.streamingFresh(9, clock.monotonicNs()));
        // The ranged VETO: a stream's sub-block request into an unregistered
        // dest takes the whole block (1.0× amp + the pipeline).
        const ranged0 = stats.ranged_reads.load();
        _ = try f.rp.read(io, 9, 4 * Fixture.bs, 4096, .{ .buf = buf[0..4096] }, .{});
        try testing.expectEqual(ranged0, stats.ranged_reads.load());
        try testing.expect(f.rp.hot.contains(.init(Fixture.vol_tag, 8 * Fixture.bs)) or f.rp.hold.contains(.init(Fixture.vol_tag, 8 * Fixture.bs)));
        // The LEASE wins over the veto on a registered aligned dest for a
        // COLD block: lease traffic is device-true by design (the +28.6 %
        // streaming row is exactly this shape).
        const lease0 = stats.read_dest_lease_bytes.load();
        _ = try f.rp.read(io, 9, 5 * Fixture.bs, 4096, .{ .buf = buf[0..4096], .registered = true }, .{});
        try testing.expectEqual(ranged0 + 1, stats.ranged_reads.load());
        try testing.expectEqual(lease0 + 4096, stats.read_dest_lease_bytes.load());
        try testing.expect(!f.rp.hot.contains(.init(Fixture.vol_tag, 9 * Fixture.bs)));
    }
    {
        const f = try Fixture.init(testing.allocator, .{ .ranged_threshold = 0 });
        defer f.deinit();
        f.size = Fixture.bs;
        f.bind(0, 1);
        const buf = try alignedBuf(testing.allocator, 4096);
        defer testing.allocator.free(buf);
        const ranged0 = stats.ranged_reads.load();
        _ = try f.rp.read(io, 7, 0, 4096, .{ .buf = buf }, .{});
        try testing.expectEqual(ranged0, stats.ranged_reads.load());
        try testing.expectEqual(@as(u64, Fixture.bs), f.device_read_bytes.load(.monotonic));
    }
    try testing.expect(!rangedEligible(262144, 4096, 1 << 22, false, false));
    try testing.expect(!rangedEligible(262144, 4096, 1 << 22, true, true));
    try testing.expect(!rangedEligible(262144, 4096, 262144, true, false));
    try testing.expect(rangedEligible(262144, 262144, 1 << 22, true, false));
}

/// The device-read ledger's two partitions of the read path's DMA bytes.
const DmaLedger = struct {
    ranged: u64,
    demand: u64,
    prefetch_: u64,
    lane: u64,
    image: u64,
    window: u64,
    pooled: u64,
    registered: u64,

    fn snap() DmaLedger {
        return .{
            .ranged = stats.read_dma_ranged_bytes.load(),
            .demand = stats.read_dma_block_demand_bytes.load(),
            .prefetch_ = stats.read_dma_block_prefetch_bytes.load(),
            .lane = stats.read_dma_block_lane_bytes.load(),
            .image = stats.read_dma_image_bytes.load(),
            .window = stats.read_dma_window_bytes.load(),
            .pooled = stats.read_fill_dma_bytes.load(),
            .registered = stats.read_dest_dma_bytes.load(),
        };
    }

    fn purposeSum(self: DmaLedger) u64 {
        return self.ranged + self.demand + self.prefetch_ + self.lane + self.image + self.window;
    }

    fn whereSum(self: DmaLedger) u64 {
        return self.pooled + self.registered;
    }
};

test "the device-read ledger: every READ-side DMA byte lands in exactly one PURPOSE arm (ranged / demand block / prefetch block / lane block / image / window), the purpose arms sum to the WHERE arms (pooled + registered) and to the device's own count; a hit moves no arm" {
    const io = testing.io;
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.size = 4 * Fixture.bs;
    f.bind(0, 1);
    f.bind(1, 2);
    f.bind(2, 6);
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    const l0 = DmaLedger.snap();
    const dev0 = f.device_read_bytes.load(.monotonic);
    // A cold whole-block read: one demand block.
    _ = try f.rp.read(io, 7, 0, Fixture.bs, .{ .buf = buf }, .{});
    var l = DmaLedger.snap();
    try testing.expectEqual(Fixture.bs, l.demand - l0.demand);
    try testing.expectEqual(@as(u64, 0), (l.ranged - l0.ranged) + (l.prefetch_ - l0.prefetch_) + (l.lane - l0.lane) + (l.image - l0.image) + (l.window - l0.window));
    // The hot hit moves nothing.
    _ = try f.rp.read(io, 7, 4096, 4096, .{ .buf = buf[0..4096] }, .{});
    try testing.expectEqual(l.purposeSum(), DmaLedger.snap().purposeSum());
    // A cold 4 KiB read of another block: a ranged window, lease and bounce alike.
    _ = try f.rp.read(io, 7, Fixture.bs + 8192, 4096, .{ .buf = buf[0..4096], .registered = true }, .{});
    _ = try f.rp.read(io, 7, Fixture.bs + 100, 50, .{ .buf = buf[0..50] }, .{});
    l = DmaLedger.snap();
    try testing.expectEqual(@as(u64, 8192), l.ranged - l0.ranged);
    // The raw full-block dest leg is a demand block too.
    _ = try f.rp.read(io, 7, 2 * Fixture.bs, Fixture.bs, .{ .buf = buf, .registered = true }, .{});
    l = DmaLedger.snap();
    try testing.expectEqual(2 * Fixture.bs, l.demand - l0.demand);
    // A prefetch warm of a cold block lands in the prefetch arm; a lane warm
    // in the lane arm (`read_lane_fetch_bytes` counts it too).
    f.bind(3, 7);
    f.size = 4 * Fixture.bs;
    const lane_bytes0 = prefetch.stats.read_lane_fetch_bytes.load();
    f.rp.warm(io, 7, 3, .prefetch);
    l = DmaLedger.snap();
    try testing.expectEqual(Fixture.bs, l.prefetch_ - l0.prefetch_);
    _ = purge.purgeBlockKey(f.rp.tiers, .init(Fixture.vol_tag, 7 * Fixture.bs));
    f.rp.warm(io, 7, 3, .lane);
    l = DmaLedger.snap();
    try testing.expectEqual(Fixture.bs, l.lane - l0.lane);
    try testing.expectEqual(lane_bytes0 + Fixture.bs, prefetch.stats.read_lane_fetch_bytes.load());
    // A packed tenant's window.
    f.kind = .packed_;
    f.bindings[0] = Mapping.tenant(Fixture.vol_tag, 5 * Fixture.bs, 1, 4096, 300);
    f.alloc.publishBlock(5);
    f.size = 300;
    _ = try f.rp.read(io, 8, 0, 300, .{ .buf = buf[0..300] }, .{});
    l = DmaLedger.snap();
    try testing.expectEqual(@as(u64, 4096), l.window - l0.window);
    // The two partitions agree with each other and with the device.
    try testing.expectEqual(l.whereSum() - l0.whereSum(), l.purposeSum() - l0.purposeSum());
    try testing.expectEqual(f.device_read_bytes.load(.monotonic) - dev0, l.purposeSum() - l0.purposeSum());
}

test "binding validation: a rebind between the fetch and the serve is a loss that re-resolves (never foreign bytes); exhaustion settles under the stripe instead of EIO; a device error on the current binding stays loud" {
    const io = testing.io;
    const f = try Fixture.init(testing.allocator, .{ .rebind_cap = 2 });
    defer f.deinit();
    f.size = Fixture.bs;
    f.bind(0, 1);
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    // The hook: after each DMA, move the binding to the next device block
    // for the first `moves` fetches.
    const Hook = struct {
        var fx: *Fixture = undefined;
        var moves: u32 = 0;
        var next: u64 = 2;
        fn run(_: ?*anyopaque, _: BlockKey) void {
            if (moves == 0) return;
            moves -= 1;
            fx.bind(0, next);
            next += 1;
        }
    };
    Hook.fx = f;
    Hook.moves = 1;
    f.rp.seams.after_dma = &Hook.run;
    const rebinds0 = stats.stale_binding_rebinds.load();
    const esc0 = stats.stale_binding_escalations.load();
    // ONE move: the first fetch (dev 1) is a loss; the retry serves dev 2.
    const n = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n);
    try Fixture.expected(2 * Fixture.bs, buf);
    try testing.expectEqual(rebinds0 + 1, stats.stale_binding_rebinds.load());
    try testing.expectEqual(esc0, stats.stale_binding_escalations.load());
    // A storm: every fetch loses until the ladder (SETTLE_HANDOFF = 8) is
    // exhausted — the hook moves the binding under each of the 8 fetches —
    // and the settle under the stripe serves the CURRENT binding (the
    // stripe excludes every legal mutator, so the settle's fetch sees no
    // further move).
    _ = purge.purgeBlockKey(f.rp.tiers, .init(Fixture.vol_tag, 2 * Fixture.bs));
    Hook.moves = SETTLE_HANDOFF;
    Hook.next = 3;
    const rebinds1 = stats.stale_binding_rebinds.load();
    const n2 = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n2);
    const final = f.bindings[0].?.offset;
    try testing.expectEqual(@as(u64, (3 + SETTLE_HANDOFF - 1) * Fixture.bs), final);
    try Fixture.expected(final, buf);
    try testing.expectEqual(esc0 + 1, stats.stale_binding_escalations.load());
    try testing.expectEqual(rebinds1 + SETTLE_HANDOFF, stats.stale_binding_rebinds.load());
    try testing.expectEqual(@as(u64, 0), stats.read_settle_lost_serialized.load());
    f.rp.seams.after_dma = null;
    // A device error on the current binding is loud.
    _ = purge.purgeBlockKey(f.rp.tiers, .init(Fixture.vol_tag, final));
    f.fail_reads.store(1, .monotonic);
    try testing.expectError(error.InputOutput, f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{}));
    // The flight closed on the failure: the next read fills.
    try testing.expectEqual(@as(usize, 0), f.rp.flights.openCount(io));
    _ = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try Fixture.expected(final, buf);
    // A stale incarnation (retired, republished) between fetch and serve is
    // a loss too: the fill is never published under the dead word.
    const dead_key = BlockKey.init(Fixture.vol_tag, final);
    _ = purge.purgeBlockKey(f.rp.tiers, dead_key);
    const Retire = struct {
        var fx: *Fixture = undefined;
        var armed: bool = true;
        fn run(_: ?*anyopaque, k: BlockKey) void {
            if (!armed) return;
            armed = false;
            fx.alloc.markIncarnationUnstable(k.blockIdx(Fixture.bs));
            fx.alloc.publishBlock(k.blockIdx(Fixture.bs));
        }
    };
    Retire.fx = f;
    f.rp.seams.after_dma = &Retire.run;
    const rb = stats.stale_binding_rebinds.load();
    _ = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try Fixture.expected(final, buf);
    try testing.expectEqual(rb + 1, stats.stale_binding_rebinds.load());
    try testing.expect(f.rp.hot.contains(dead_key));
}

fn cohortBody(io: Io) !void {
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.size = Fixture.bs;
    f.bind(0, 6);
    const Ctx = struct {
        fx: *Fixture,
        io: Io,
        ok: Atomic(u32) = .init(0),
        fn run(c: *@This()) void {
            const buf = alignedBuf(testing.allocator, 1 << 20) catch unreachable;
            defer testing.allocator.free(buf);
            const n = c.fx.rp.read(c.io, 7, 0, 1 << 20, .{ .buf = buf }, .{}) catch unreachable;
            std.debug.assert(n == 1 << 20);
            Fixture.expected(6 * Fixture.bs, buf) catch unreachable;
            _ = c.ok.fetchAdd(1, .monotonic);
        }
    };
    var ctx: Ctx = .{ .fx = f, .io = io };
    var threads: [8]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Ctx.run, .{&ctx});
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 8), ctx.ok.load(.monotonic));
    // ≤ 1 device fetch per unique cold block (every racer that missed the
    // flight found the hot entry; a racer that caught it was a cohort member).
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
    try testing.expectEqual(@as(usize, 0), f.rp.flights.openCount(io));
}

test "single-flight cohort through the read path: 8 concurrent readers of one cold block ⇒ exactly 1 device fetch" {
    try sync_testing.underBoth(cohortBody);
}

// The cohort test's one-in-forty under load, pinned: a reader whose tier
// probes missed is held in the probe→enter window while a sibling's whole
// flight runs to completion (filled, deposited, published, closed); the
// reader then claims a FRESH flight for a block the hot tier already holds.
fn reprobeBody(io: Io) !void {
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.size = Fixture.bs;
    f.bind(0, 4);
    const Gap = struct {
        var fx: *Fixture = undefined;
        var io_: Io = undefined;
        var armed: bool = true;
        fn sibling(fx_: *Fixture, io__: Io) void {
            const buf = alignedBuf(testing.allocator, 1 << 20) catch unreachable;
            defer testing.allocator.free(buf);
            _ = fx_.rp.read(io__, 7, 0, 1 << 20, .{ .buf = buf }, .{}) catch unreachable;
        }
        fn run(_: ?*anyopaque, _: BlockKey) void {
            if (!armed) return;
            armed = false;
            const t = std.Thread.spawn(.{}, sibling, .{ fx, io_ }) catch unreachable;
            t.join();
        }
    };
    Gap.fx = f;
    Gap.io_ = io;
    Gap.armed = true;
    f.rp.seams.before_flight = &Gap.run;
    defer f.rp.seams.before_flight = null;
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    const reprobe0 = stats.read_fill_reprobe_hits.load();
    const n = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n);
    try Fixture.expected(4 * Fixture.bs, buf);
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
    try testing.expectEqual(reprobe0 + 1, stats.read_fill_reprobe_hits.load());
    try testing.expectEqual(@as(usize, 0), f.rp.flights.openCount(io));
}

test "the probe→enter window: a reader that claims the flight after a sibling's flight closed finds the block resident and never fetches it a second time" {
    try sync_testing.underBoth(reprobeBody);
}

test "direct_device_true: an O_DIRECT read is device-true (no tier serve, no publish, every request a ranged window, counted); a buffered read on the same mount rides the tiers" {
    const io = testing.io;
    const f = try Fixture.init(testing.allocator, .{ .direct_device_true = true });
    defer f.deinit();
    f.size = Fixture.bs;
    f.bind(0, 7);
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    try testing.expectEqual(@as(u64, 1), stats.direct_device_true.load());
    const dt0 = stats.read_device_true_reads.load();
    const od0 = stats.read_odirect_requests.load();
    // Buffered first: a whole-block fill lands hot.
    _ = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expect(f.rp.hot.contains(.init(Fixture.vol_tag, 7 * Fixture.bs)));
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
    // O_DIRECT: device-true — a ranged window even though the block is hot.
    const n = try f.rp.read(io, 7, 4096, 4096, .{ .buf = buf[0..4096] }, .{ .odirect = true });
    try testing.expectEqual(@as(usize, 4096), n);
    try Fixture.expected(7 * Fixture.bs + 4096, buf[0..4096]);
    try testing.expectEqual(@as(u64, 2), f.device_reads.load(.monotonic));
    try testing.expectEqual(dt0 + 1, stats.read_device_true_reads.load());
    try testing.expectEqual(od0 + 1, stats.read_odirect_requests.load());
    // A whole-block O_DIRECT read: a ranged window of the whole block, no
    // single-flight, no RAM/NVMe side effect.
    const misses0 = stats.cache_misses.load();
    _ = f.rp.hot.purge(.init(Fixture.vol_tag, 7 * Fixture.bs));
    _ = try f.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{ .odirect = true });
    try testing.expectEqual(misses0, stats.cache_misses.load());
    try testing.expect(!f.rp.hot.contains(.init(Fixture.vol_tag, 7 * Fixture.bs)));
    try testing.expect(!f.rp.nvme.has(.init(Fixture.vol_tag, 7 * Fixture.bs)));
    // Hybrid I/O without the ruler: O_DIRECT serves from the tier (counted).
    const g = try Fixture.init(testing.allocator, .{});
    defer g.deinit();
    g.size = Fixture.bs;
    g.bind(0, 7);
    _ = try g.rp.read(io, 7, 0, 1 << 20, .{ .buf = buf }, .{});
    const ts0 = stats.read_odirect_tier_serves.load();
    _ = try g.rp.read(io, 7, 0, 4096, .{ .buf = buf[0..4096] }, .{ .odirect = true });
    try testing.expectEqual(ts0 + 1, stats.read_odirect_tier_serves.load());
    try testing.expectEqual(@as(u64, 1), g.device_reads.load(.monotonic));
}

test "inline and packed layouts: inline from the record zero-padded; a packed tenant is ONE window on its own volume through the choke point; EOF; a too-small dest is refused" {
    const io = testing.io;
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.kind = .inline_;
    f.inline_data = "hello inline";
    f.size = 100;
    var buf: [128]u8 = undefined;
    const n = try f.rp.read(io, 7, 6, 50, .{ .buf = &buf }, .{});
    try testing.expectEqual(@as(usize, 50), n);
    try testing.expectEqualStrings("inline", buf[0..6]);
    try testing.expectEqualSlices(u8, &@as([44]u8, @splat(0)), buf[6..50]);
    try testing.expectEqual(@as(usize, 0), try f.rp.read(io, 7, 100, 10, .{ .buf = &buf }, .{}));
    stats.read_dest_overruns.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.BufferTooSmall, f.rp.read(io, 7, 0, 100, .{ .buf = buf[0..10] }, .{}));
    try log.testing.expectAllFired();
    // Packed: tenant at dev block 1, rel_off 8192, len 6000.
    f.kind = .packed_;
    f.size = 6000;
    f.bindings[0] = Mapping.tenant(Fixture.vol_tag, 1 * Fixture.bs, 1, 8192, 6000);
    f.alloc.publishBlock(1);
    var out: [1000]u8 = undefined;
    const m = try f.rp.read(io, 7, 5000, 1000, .{ .buf = &out }, .{});
    try testing.expectEqual(@as(usize, 1000), m);
    try Fixture.expected(Fixture.bs + 8192 + 5000, &out);
    // `[5000, 6000)` of the image is ONE grain `[4096, 8192)`.
    try testing.expectEqual(@as(u64, 4096), f.device_read_bytes.load(.monotonic));
    // A DAMAGED tenant refuses at the choke point.
    f.bindings[0].?.damaged = true;
    layout.stats.packed_mapping_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.InputOutput, f.rp.read(io, 7, 0, 10, .{ .buf = &out }, .{}));
    try log.testing.expectAllFired();
}

test "the write side outranks every tier: an overlay serve returns its bytes; full extent runs serve without a base; partial runs compose over the fetched base; a staged custody block serves from the ring" {
    const io = testing.io;
    const f = try Fixture.init(testing.allocator, .{});
    defer f.deinit();
    f.size = Fixture.bs;
    f.bind(0, 2);
    const WS = struct {
        var mode: enum { overlay, full_runs, partial_runs, staged } = .overlay;
        fn overlay(_: *anyopaque, _: u64, _: u64, len: u64, dest: []u8) OverlayVerdict {
            if (mode != .overlay) return .none;
            @memset(dest[0..@intCast(len)], 0xEE);
            return .{ .served = @intCast(len) };
        }
        fn runs(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, dest: []u8) ExtentCoverage {
            switch (mode) {
                .full_runs => {
                    if (dest.len > 0) @memset(dest, 0xFF);
                    return .full;
                },
                .partial_runs => {
                    if (dest.len > 0) @memset(dest[0..16], 0xAA);
                    return .partial;
                },
                else => return .none,
            }
        }
        fn stagedBlock(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, dest: []u8, _: bool) StagedBlock {
            if (mode != .staged) return .none;
            @memset(dest[0..100], 0x55);
            return .{ .served = 100 };
        }
        fn stagedPayload(_: *anyopaque, _: u64, _: layout.FileId, _: u64, _: u64, _: []u8) StagedVerdict {
            return .absent;
        }
        fn retained(_: *anyopaque, _: u64) bool {
            return false;
        }
        fn openOverlay(_: *anyopaque, _: u64, _: u64) bool {
            return false;
        }
        fn custody(_: *anyopaque, _: u64, _: u64) bool {
            return false;
        }
    };
    var dummy: u8 = 0;
    f.rp.write_side = .{ .ctx = &dummy, .overlay_read = &WS.overlay, .extent_runs = &WS.runs, .staged_block = &WS.stagedBlock, .staged_payload = &WS.stagedPayload, .has_retained_extents = &WS.retained, .open_overlay = &WS.openOverlay, .has_custody = &WS.custody };
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    WS.mode = .overlay;
    _ = try f.rp.read(io, 7, 0, 4096, .{ .buf = buf[0..4096] }, .{});
    try testing.expectEqualSlices(u8, &@as([4096]u8, @splat(0xEE)), buf[0..4096]);
    try testing.expectEqual(@as(u64, 0), f.device_reads.load(.monotonic));
    WS.mode = .full_runs;
    _ = try f.rp.read(io, 7, 0, 4096, .{ .buf = buf[0..4096] }, .{});
    try testing.expectEqualSlices(u8, &@as([4096]u8, @splat(0xFF)), buf[0..4096]);
    try testing.expectEqual(@as(u64, 0), f.device_reads.load(.monotonic));
    WS.mode = .staged;
    const n = try f.rp.read(io, 7, 0, 4096, .{ .buf = buf[0..4096] }, .{});
    try testing.expectEqual(@as(usize, 4096), n);
    try testing.expectEqualSlices(u8, &@as([100]u8, @splat(0x55)), buf[0..100]);
    try testing.expectEqualSlices(u8, &@as([3996]u8, @splat(0)), buf[100..4096]);
    try testing.expectEqual(@as(u64, 0), f.device_reads.load(.monotonic));
    WS.mode = .partial_runs;
    _ = try f.rp.read(io, 7, 0, 4096, .{ .buf = buf[0..4096] }, .{});
    try testing.expectEqualSlices(u8, &@as([16]u8, @splat(0xAA)), buf[0..16]);
    try Fixture.expected(2 * Fixture.bs + 16, buf[16..4096]);
    try testing.expectEqual(@as(u64, 1), f.device_reads.load(.monotonic));
}

test "writeStatsJson emits one object naming every family" {
    var buf: [16384]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    try testing.expect(s[0] == '{' and s[s.len - 1] == '}');
    for ([_][]const u8{ "cache_hits", "ranged_reads", "stale_binding_escalations", "read_copy_dest_bytes", "read_dest_lease_bytes", "read_fill_phase_ns", "hot_block_hits", "read_lane_holds", "read_cache_hits", "fill_cohort_joins", "read_tier_admission_ghost_hits", "prefetch_issued", "read_tier_purges" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}

// ---------------------------------------------------------------------------
// The ONE Linux e2e: a real `nvme_dev.Device` over a file-backed volume.
// ---------------------------------------------------------------------------

const nvme_dev = if (builtin.os.tag == .linux) @import("../io/nvme_dev.zig") else struct {};

const DeviceFixture = if (builtin.os.tag == .linux) struct {
    const bs: u64 = 1 << 20;
    const vol_tag: u64 = 0x7777;
    const capacity: u64 = 16 * bs;
    const linux = std.os.linux;

    gpa: Allocator,
    path_buf: [96]u8 = undefined,
    path: [:0]const u8 = "",
    file: std.Io.File = undefined,
    dev: *nvme_dev.Device = undefined,
    alloc: *block_alloc.BlockAllocator = undefined,
    size: u64 = 0,
    bindings: [8]?Mapping = @splat(null),
    rp: *ReadPath = undefined,

    var seq: Atomic(u32) = .init(0);

    fn create(self: *DeviceFixture, direct_device_true: bool) !void {
        const dir: []const u8 = blk: {
            std.Io.Dir.accessAbsolute(testing.io, "/build", .{}) catch break :blk "/tmp";
            break :blk "/build";
        };
        self.path = try std.mem.printSentinel(&self.path_buf, "{s}/sqz-readpath-{d}-{d}.img", .{ dir, linux.getpid(), seq.fetchAdd(1, .monotonic) }, 0);
        self.file = try std.Io.Dir.createFileAbsolute(testing.io, self.path, .{ .read = true, .truncate = true });
        try self.file.setLength(testing.io, capacity);
        // The device pattern: byte = f(absolute offset), written buffered
        // before the O_DIRECT device opens.
        const chunk = try self.gpa.alloc(u8, 1 << 20);
        defer self.gpa.free(chunk);
        var off: u64 = 0;
        while (off < capacity) : (off += chunk.len) {
            for (chunk, 0..) |*b, i| b.* = @truncate(((off + i) / 4096) * 31 + (off + i));
            try self.file.writePositionalAll(testing.io, chunk, off);
        }
        try self.file.sync(testing.io);
        self.dev = try nvme_dev.Device.open(self.gpa, self.path);
        errdefer self.dev.deinit();
        self.alloc = try block_alloc.BlockAllocator.init(self.gpa, .{ .vol_tag = vol_tag, .vol = 0, .block_size = bs, .capacity_bytes = capacity });
        errdefer self.alloc.deinit();
        const cfg: Config = .{
            .block_size = bs,
            .passthrough = true,
            .ranged_threshold = 256 * derive.KiB,
            .dest_lease = true,
            .read_lane = true,
            .direct_device_true = direct_device_true,
            .zc_serve = true,
            .cores = 1,
            .hot_budget = 4 * bs,
            .read_mem_limit = 4 * rsc.SMALL_FILL_MAX,
            .mem_budget = 64 * bs,
            .admission_policy = .second_touch,
            .fill_pct = 5,
            .prefetch = .{ .window_cap = 8, .share_pct = 50, .hot_budget = 4 * bs, .lane_armed = true, .depth_pin = null, .lane_budget_cap = 8 * bs },
        };
        const nvme = try nvme_read_cache.Cache.initRam(self.gpa, 1, 8 * bs);
        self.rp = try ReadPath.init(self.gpa, cfg, .{ .ctx = self, .file = &metaFile, .binding = &metaBinding, .inline_data = &metaInline }, .{ .ctx = self, .allocator_of = &volAlloc, .read_range = &volRead, .device_fd = &volFd }, WriteSide.none, null, nvme, null);
    }

    fn destroy(self: *DeviceFixture) void {
        self.rp.deinit();
        self.alloc.deinit();
        self.dev.deinit();
        self.file.close(testing.io);
        std.Io.Dir.deleteFileAbsolute(testing.io, self.path) catch {};
    }

    fn bind(self: *DeviceFixture, b: usize, dev_idx: u64) void {
        self.bindings[b] = Mapping.wholeBlock(vol_tag, dev_idx * bs, 1);
        self.alloc.publishBlock(dev_idx);
    }

    fn metaFile(ctx: *anyopaque, _: u64) ?FileInfo {
        const self: *DeviceFixture = @ptrCast(@alignCast(ctx));
        return .{ .size = self.size, .kind = .striped };
    }
    fn metaBinding(ctx: *anyopaque, _: u64, block: u64) ?Mapping {
        const self: *DeviceFixture = @ptrCast(@alignCast(ctx));
        if (block >= self.bindings.len) return null;
        return self.bindings[block];
    }
    fn metaInline(_: *anyopaque, _: u64, _: u64, _: []u8) usize {
        return 0;
    }
    fn volAlloc(ctx: *anyopaque, tag: u64) ?*block_alloc.BlockAllocator {
        const self: *DeviceFixture = @ptrCast(@alignCast(ctx));
        return if (tag == vol_tag) self.alloc else null;
    }
    /// THE device binding: `nvme_dev.Device.readInto` — the ranged window
    /// and the whole-block fill land where the read path points.
    fn volRead(ctx: *anyopaque, _: u64, device_off: u64, dest: []u8, _: DestShape) DeviceError!Served {
        const self: *DeviceFixture = @ptrCast(@alignCast(ctx));
        try self.dev.readInto(device_off, dest);
        return .dma;
    }
    fn volFd(_: *anyopaque, _: u64) ?linux.fd_t {
        return null;
    }
} else struct {};

test "e2e (Linux, nvme_dev.Device): cold whole-block fill → hot hit → purge → ranged 4 KiB lease DMA into a registered aligned buffer → streaming classification over 4 contiguous requests → direct_device_true; the memfd fill pool is fd-addressable; get_obj counts every device read" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "read_path: Linux only");
    const io = testing.io;
    var f: DeviceFixture = .{ .gpa = testing.allocator };
    try f.create(false);
    defer f.destroy();
    f.size = 8 * DeviceFixture.bs;
    for (0..8) |i| f.bind(i, @intCast(i + 2));
    const buf = try alignedBuf(testing.allocator, 1 << 20);
    defer testing.allocator.free(buf);
    const get_obj0 = nvme_dev.stats.get_obj.load();
    const key0 = BlockKey.init(DeviceFixture.vol_tag, 2 * DeviceFixture.bs);
    // Cold whole-block fill: ONE device read, lands hot (probation).
    const n = try f.rp.read(io, 11, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expectEqual(@as(usize, 1 << 20), n);
    try Fixture.expected(2 * DeviceFixture.bs, buf);
    try testing.expectEqual(get_obj0 + 1, nvme_dev.stats.get_obj.load());
    try testing.expect(f.rp.hot.contains(key0));
    // The fill pool is the memfd slab: the hot entry is fd-addressable.
    const hit = f.rp.hot.getNoPromote(key0).?;
    const view = f.rp.pool.bytes(hit.slot)[0..4096];
    const fdo = f.rp.fdSource(view) orelse return error.TestUnexpectedResult;
    var back: [4096]u8 = undefined;
    try testing.expectEqual(@as(usize, 4096), std.os.linux.pread(fdo.fd, &back, back.len, @intCast(fdo.off)));
    try testing.expectEqualSlices(u8, view, &back);
    f.rp.hot.unpin(hit);
    // Hot hit: no device read.
    _ = try f.rp.read(io, 11, 8192, 4096, .{ .buf = buf[0..4096] }, .{});
    try Fixture.expected(2 * DeviceFixture.bs + 8192, buf[0..4096]);
    try testing.expectEqual(get_obj0 + 1, nvme_dev.stats.get_obj.load());
    // Purge, then a ranged 4 KiB read into a registered aligned buffer: the
    // dest LEASE — exactly 4 KiB of device DMA straight into `buf`.
    try testing.expect(purge.purgeBlockKey(f.rp.tiers, key0) >= 1);
    const lease0 = stats.read_dest_lease_bytes.load();
    const ranged0 = stats.ranged_reads.load();
    _ = try f.rp.read(io, 11, 16384, 4096, .{ .buf = buf[0..4096], .registered = true }, .{});
    try Fixture.expected(2 * DeviceFixture.bs + 16384, buf[0..4096]);
    try testing.expectEqual(get_obj0 + 2, nvme_dev.stats.get_obj.load());
    try testing.expectEqual(lease0 + 4096, stats.read_dest_lease_bytes.load());
    try testing.expectEqual(ranged0 + 1, stats.ranged_reads.load());
    try testing.expect(!f.rp.hot.contains(key0) and !f.rp.nvme.has(key0));
    // Streaming: 4 contiguous whole-block requests classify the lane.
    const classified0 = prefetch.stats.read_streams_classified.load();
    for (1..5) |i| {
        _ = try f.rp.read(io, 11, @as(u64, i) * DeviceFixture.bs, DeviceFixture.bs, .{ .buf = buf }, .{});
        try Fixture.expected((i + 2) * DeviceFixture.bs, buf);
    }
    try testing.expectEqual(classified0 + 1, prefetch.stats.read_streams_classified.load());
    try testing.expect(f.rp.streamingFresh(11, clock.monotonicNs()));
    try testing.expectEqual(get_obj0 + 6, nvme_dev.stats.get_obj.load());
    // direct_device_true on a second mount: O_DIRECT reads are device-true.
    var g: DeviceFixture = .{ .gpa = testing.allocator };
    try g.create(true);
    defer g.destroy();
    g.size = DeviceFixture.bs;
    g.bind(0, 9);
    const dt0 = stats.read_device_true_reads.load();
    _ = try g.rp.read(io, 12, 0, 1 << 20, .{ .buf = buf }, .{});
    try testing.expect(g.rp.hot.contains(.init(DeviceFixture.vol_tag, 9 * DeviceFixture.bs)));
    const before_dt = nvme_dev.stats.get_obj.load();
    _ = try g.rp.read(io, 12, 4096, 4096, .{ .buf = buf[0..4096] }, .{ .odirect = true });
    try Fixture.expected(9 * DeviceFixture.bs + 4096, buf[0..4096]);
    try testing.expectEqual(before_dt + 1, nvme_dev.stats.get_obj.load());
    try testing.expectEqual(dt0 + 1, stats.read_device_true_reads.load());
}
