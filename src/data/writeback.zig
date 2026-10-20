//! Never-lossy writeback: staging flush units, the RAM-pressure spill and
//! the promotion primitive's binding (charter §6.2 item 5 "Staging absorbs
//! partial tails, RAM-pressure spills, write-through failures (never-lossy)
//! … Staged flushes DMA straight from the mmap through a write-only
//! consume-by-value guard"; "Never-lossy writeback: flush units re-validate
//! staged ownership per attempt and present the ino's CURRENT fencing
//! generation; fencing-stale ⇒ contractual no-op; verified-NotFound ⇒
//! discard orphans; transient ⇒ retry forever"; §6.1 the ONE promotion
//! primitive; `docs/porting/data_write_path.md` §7).
//!
//! The flush unit: up to `FLUSH_ATTEMPTS` passes (the layout-prune epoch
//! moving), each under (3): the staged generation is the SUPERSESSION
//! test only — the merge credential is the ino's CURRENT fencing generation
//! read at the last responsible moment (the incident_013 livelock: a staged
//! stamp presented as the credential never converged). `StagedDmaSource` is
//! the write-only guard over the mmap: consumed BY VALUE by the DMA, so
//! retention past it is unexpressible (Zig's move; the reference's
//! `compile_fail`). The worker retries forever on the ONE ladder (Q14: a
//! unit past the ladder's cap is COUNTED, `writeback_retry_exhaustions`,
//! never a wrapping counter as control flow, never a sticky per-ino error
//! — the multi-volume bench-suite EIO cascade). The queue cap is DERIVED
//! from the parked budget (Q7), no knob.

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const mpmc = @import("../sync/mpmc.zig");
const sync_notify = @import("../sync/notify.zig");
const layout = @import("layout.zig");
const staging = @import("staging.zig");
const pack = @import("pack.zig");
const block_alloc = @import("block_alloc.zig");
const enospc = @import("enospc.zig");
const seams = @import("write_seams.zig");
const accumulate = @import("accumulate.zig");
const retry = @import("write_retry.zig");
const pipeline = @import("write_pipeline.zig");

pub const Binding = seams.Binding;
pub const PAGE: u32 = seams.PAGE;

/// The layout-prune epoch moving under a unit is the only reason to loop:
/// eight passes outlast any truncate storm a test or fstests produces; past
/// it the unit requeues (never an error).
pub const FLUSH_ATTEMPTS: u32 = 8;
/// The requeue ladder: `50 ms << min(a, 6)` capped at 3.2 s — the fast
/// retries then the cap; a unit past the cap is counted, never wrapped.
pub const REQUEUE_LADDER: retry.Ladder = .{ .base_ns = 50 * std.time.ns_per_ms, .cap_ns = 3200 * std.time.ns_per_ms, .attempts_max = 4 };

/// The queue cap = the parked budget in blocks (every spilled block is
/// one unit; the budget bounds the parked set, so it bounds the queue).
pub fn queueCap(parked_cap_buffers: u64) usize {
    return @intCast(@max(parked_cap_buffers, 1));
}

pub const stats = struct {
    pub var writeback_queue_cap: counters.Gauge = .{};
    pub var writeback_queue_depth: counters.Gauge = .{};
    pub var writeback_enqueued_spill: counters.ShardedCounter = .{};
    pub var writeback_enqueued_wt_fallback: counters.ShardedCounter = .{};
    pub var writeback_enqueued_flush: counters.ShardedCounter = .{};
    pub var writeback_sync_flushes_on_full_queue: counters.ShardedCounter = .{};
    pub var writeback_superseded_noops: counters.ShardedCounter = .{};
    pub var writeback_stale_token_retries: counters.ShardedCounter = .{};
    pub var writeback_retry_exhaustions: counters.ShardedCounter = .{};
    pub var writeback_orphan_discards: counters.ShardedCounter = .{};
    /// Units a truncate made dead before their flush (`Disposition.eof_discarded`).
    pub var writeback_eof_discards: counters.ShardedCounter = .{};
    pub var writeback_fence_noops: counters.ShardedCounter = .{};
    pub var writeback_errors_latched: counters.ShardedCounter = .{};
    pub var writeback_errors_reported: counters.ShardedCounter = .{};
    pub var writeback_flushed: counters.ShardedCounter = .{};
    pub var durable_upload_bytes_writeback: counters.ShardedCounter = .{};
    pub var staging_put_bytes_spill: counters.ShardedCounter = .{};
    pub var layout_promoted_inline: counters.ShardedCounter = .{};
    pub var layout_promoted_own_block: counters.ShardedCounter = .{};
    /// A drain's cover round (`drainOnce`) whose meta barrier failed: the
    /// released ring records stay held for the next round.
    pub var writeback_cover_rounds_failed: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"writeback_queue_cap\":{d},\"writeback_queue_depth\":{d}", .{ stats.writeback_queue_cap.load(), stats.writeback_queue_depth.load() });
    inline for (.{ "writeback_enqueued_spill", "writeback_enqueued_wt_fallback", "writeback_enqueued_flush", "writeback_sync_flushes_on_full_queue", "writeback_superseded_noops", "writeback_stale_token_retries", "writeback_retry_exhaustions", "writeback_orphan_discards", "writeback_eof_discards", "writeback_fence_noops", "writeback_errors_latched", "writeback_errors_reported", "writeback_flushed", "durable_upload_bytes_writeback", "staging_put_bytes_spill", "layout_promoted_inline", "layout_promoted_own_block", "writeback_cover_rounds_failed" }) |name| {
        try w.print(",\"{s}\":{d}", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// The spilled-block identity and the write-only DMA source.
// ---------------------------------------------------------------------------

/// A spilled parked block's ring key: `file_id = "SQZBLOCK" ‖ block u32 ‖
/// 0…` — never collides with a staged FILE's random 16-byte id (a file id
/// is minted from `Io.randomSecure`; this prefix is reserved).
pub fn blockKey(ino: u64, block: u32) staging.StagingKey {
    var id: layout.FileId = @splat(0);
    @memcpy(id[0..8], "SQZBLOCK");
    std.mem.writeInt(u32, id[8..12], block, .little);
    return staging.StagingKey.init(id, ino);
}

pub fn blockOfKey(k: staging.StagingKey) ?u32 {
    if (!std.mem.eql(u8, k.file_id[0..8], "SQZBLOCK")) return null;
    return std.mem.readInt(u32, k.file_id[8..12], .little);
}

/// The write-only, consume-by-value guard over the staging mmap: `bytes`
/// is the DMA source (4 KiB-aligned by the ring's grain), `consume` ends
/// the hold. Taken by VALUE by `dmaFromStaging` so no retention past the
/// DMA is expressible.
pub const StagedDmaSource = struct {
    lease: staging.Segment.Lease,
    generation: u64,

    pub fn bytes(self: *const StagedDmaSource) []const u8 {
        return self.lease.bytes();
    }

    fn consume(self: StagedDmaSource) void {
        self.lease.release();
    }
};

/// `write_block_from_staging`: passthrough DMAs straight off the mmap (the
/// record value sits on the grain); a transform severs the guard pre-DMA
/// (the image is in the scratch). The source is CONSUMED here.
pub fn dmaFromStaging(device: seams.DeviceSeam, transform: accumulate.TransformSeam, scratch: []u8, vol_tag: u64, offset: u64, source: StagedDmaSource) anyerror!seams.DmaProof {
    defer source.consume();
    const image = try transform.process_write(transform.ctx, vol_tag, source.bytes(), scratch);
    return device.write(vol_tag, offset, image);
}

// ---------------------------------------------------------------------------
// The flush unit.
// ---------------------------------------------------------------------------

pub const Unit = struct {
    ino: u64,
    block: u32,
    /// The ring key the unit flushes (`blockKey` for a spill, the file's own
    /// key for a promotion).
    key: staging.StagingKey,
    /// The staged generation at enqueue: the supersession test.
    owner_generation: u64,
    /// The logical file size the spilled block establishes (`min_size`).
    min_size: u64,
    /// Entries this unit displaced (the overwrite's old binding).
    old_binding: ?Binding,
    /// The ino's size epoch when the unit was formed (`MergeRequest.
    /// size_epoch`): a truncate that lands before the flush makes the
    /// unit's reach dead — its merge inserts nothing at or past the new
    /// EOF and raises no size. Null = formed inside the setter's ordering
    /// (a promotion: one shot under its own re-check).
    size_epoch: ?u64 = null,
    ladder: retry.Ladder = REQUEUE_LADDER,
};

pub const Disposition = enum {
    flushed,
    /// A newer re-stage superseded the unit: the contractual no-op.
    superseded_noop,
    /// Nothing staged under the key: nothing to flush.
    nothing,
    /// Verified: the ino is gone; the orphan was discarded.
    orphan_discarded,
    /// Verified: a truncate landed after the unit was formed and its block
    /// lies at or past the new EOF — the bytes are dead by the truncate,
    /// the mint is returned, the ring entry released.
    eof_discarded,
    /// The fence latched with custody poisoned: a VERIFIED no-op (the
    /// bytes stay staged for the remount).
    fence_noop,
    /// Transient: requeue.
    retry,
};

pub const Deps = struct {
    device: seams.DeviceSeam,
    map: seams.MapSeam,
    free: seams.FreeSeam,
    purge: seams.PurgeSeam,
    vols: *const seams.Volumes,
    picker: enospc.Picker,
    valve: ?enospc.Valve = null,
    locks: *seams.BlockLocks,
    stripes: *layout.MetaStripes,
    pool: *staging.Pool,
    seg: *staging.Segment,
    transform: accumulate.TransformSeam = .passthrough,
    scratch: []u8 = &.{},
    /// The D0 latch's custody poison (`nvme_dev` custody hooks): a fenced
    /// merge is a verified no-op only while poisoned.
    custody_poisoned: ?*const fn () bool = null,
    /// Dismount in progress: the unit is left staged for the successor.
    dismounted: ?*const fn () bool = null,
    touched: ?*const fn (ctx: ?*anyopaque, ino: u64, ordinal: u32) void = null,
    touched_ctx: ?*anyopaque = null,
    /// The plaintext whole-block image of a binding (the spill's seed);
    /// null = raw device bytes.
    image: ?seams.ImageSeam = null,
};

/// The whole plaintext image of `b` into `out`.
fn readImage(deps: Deps, b: Binding, out: []u8) seams.ReadError!void {
    if (deps.image) |im| return im.read(im.ctx, b, out);
    return deps.device.read(b.vol_tag, b.offset, out);
}

/// `flush_one_active_block`: §7.1's seven steps, up to `FLUSH_ATTEMPTS`.
pub fn flushOne(deps: Deps, io: Io, unit: *Unit) Disposition {
    const key = unit.key;
    var attempt: u32 = 0;
    while (attempt < FLUSH_ATTEMPTS) : (attempt += 1) {
        const g = deps.locks.acquire(io, unit.ino, unit.block, .writeback_flush);
        defer g.release(io);
        // 1. Ownership re-validated per attempt.
        const staged_gen = deps.seg.residentGeneration(key) orelse return .nothing;
        if (staged_gen != unit.owner_generation) {
            stats.writeback_superseded_noops.inc();
            return .superseded_noop;
        }
        // 2. The placed mint (no shard guard across it) + the owners.
        const pm = enospc.allocatePlacedMint(deps.picker, .{ .ino = unit.ino, .block = unit.block }, io, deps.valve, null, true, .none) catch return .retry;
        var mint = block_alloc.MintedBlockGuard.init(pm.alloc, pm.minted);
        var fsck_guard = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx);
        const vol_tag = pm.alloc.volTag();
        const vol = deps.vols.byTag(vol_tag) orelse {
            fsck_guard.deinit();
            mint.abandon();
            return .retry;
        };
        // 3. The write-only source; gone ⇒ abandon, nothing.
        const lease = deps.seg.get(key) orelse {
            fsck_guard.deinit();
            mint.abandon();
            return .nothing;
        };
        const source: StagedDmaSource = .{ .lease = lease, .generation = staged_gen };
        // 4. The DMA straight off the mmap (the source is consumed inside).
        const proof = dmaFromStaging(deps.device, deps.transform, deps.scratch, vol_tag, pm.minted.offset, source) catch {
            fsck_guard.deinit();
            mint.abandon();
            if (deps.dismounted) |d| if (d()) {
                log.info("writeback ino {d} block {d}: dismount in progress — left staged for the next mount", .{ unit.ino, unit.block });
                return .retry;
            };
            return .retry;
        };
        // 5. Publish; the merge credential is the CURRENT generation; the
        //    size epoch is the unit's own (a truncate since ⇒ clipped).
        pm.alloc.publishBlock(pm.minted.idx);
        const new_key = Binding.wholeBlock(vol_tag, pm.minted.offset, pm.minted.stamp);
        stats.durable_upload_bytes_writeback.add(proof.len);
        var displaced: seams.MergeOutcome = .{};
        {
            const sg = deps.stripes.acquire(io, unit.ino);
            defer sg.release(io);
            while (true) {
                displaced = deps.map.merge(.{ .ino = unit.ino, .inserts = &.{.{ .block = unit.block, .binding = new_key }}, .min_size = unit.min_size, .proofs = &.{proof}, .fencing = deps.map.currentFencing(unit.ino), .size_epoch = unit.size_epoch }) catch |e| switch (e) {
                    error.FencingTokenExpired => {
                        stats.writeback_stale_token_retries.inc();
                        continue;
                    },
                    error.NotFound => {
                        // 6. Verified-NotFound: never resurrect a dead inode.
                        stats.writeback_orphan_discards.inc();
                        fsck_guard.deinit();
                        mint.abandon();
                        _ = staging.releaseStaged(deps.pool, deps.seg, key, staged_gen);
                        return .orphan_discarded;
                    },
                    error.WriterGuardFenced => {
                        fsck_guard.deinit();
                        mint.disarm(); // W5: free nothing
                        const poisoned = if (deps.custody_poisoned) |p| p() else false;
                        if (poisoned) {
                            stats.writeback_fence_noops.inc();
                            return .fence_noop;
                        }
                        stats.writeback_errors_reported.inc();
                        return .retry;
                    },
                    error.Transient, error.OutOfMemory => {
                        fsck_guard.deinit();
                        mint.abandon();
                        return .retry;
                    },
                };
                break;
            }
        }
        // 6b. The unit predates a truncate and its block lies at or past the
        //     new EOF: nothing names the mint — returned; the dead bytes
        //     leave the ring (the never-lossy law covered no byte below EOF).
        if (displaced.clipped != 0) {
            stats.writeback_eof_discards.inc();
            fsck_guard.deinit();
            mint.abandon();
            _ = staging.releaseStaged(deps.pool, deps.seg, key, staged_gen);
            return .eof_discarded;
        }
        // 7. Disarm, purge the recycled key, free displaced, release the
        //    ring entry iff the generation still matches.
        fsck_guard.deinit();
        mint.disarm();
        deps.purge.purge(new_key);
        if (deps.touched) |t| t(deps.touched_ctx, unit.ino, vol.ordinal);
        for (displaced.slice()) |d| {
            deps.purge.purge(d);
            _ = deps.free.free(io, d);
        }
        displaced.dropHolds();
        _ = staging.releaseStaged(deps.pool, deps.seg, key, staged_gen);
        stats.writeback_flushed.inc();
        return .flushed;
    }
    return .retry;
}

// ---------------------------------------------------------------------------
// The RAM-pressure spill.
// ---------------------------------------------------------------------------

pub const SpillError = error{ StorageFull, OutOfMemory, PoolExhausted, NotParked };

/// Spill a parked RAM block into the staging ring (never-lossy, no DMA):
/// complete the complement under (3) (zeros on a fresh buffer; a deferred
/// seed keeps its old-binding so the flush's merge displaces it — its
/// complement is seeded from the old image by ONE read), stage the image,
/// retire the entry, hand the unit to the queue.
pub fn spillParkedBlock(table: *accumulate.Table, deps: Deps, io: Io, worker: *Worker, ino: u64, block: u32) SpillError!void {
    const bs: u64 = table.block_size;
    const g = deps.locks.acquire(io, ino, block, .writeback_flush);
    defer g.release(io);
    const e = table.get(ino, block) orelse return error.NotParked;
    if (e.repr == .extent) {
        // The overlay escalates whole: the untouched pages from the base
        // image (ONE read) when deferred over a binding, zeros otherwise.
        if (e.repr.extent.base_deferred and e.old_binding != null) {
            const img = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch return error.OutOfMemory;
            defer table.gpa.free(img);
            readImage(deps, e.old_binding.?, img) catch return error.StorageFull;
            table.escalate(e, img) orelse return error.PoolExhausted;
        } else {
            table.escalate(e, null) orelse return error.PoolExhausted;
        }
    }
    if (!e.content_valid) {
        if (e.seed == .deferred and e.old_binding != null) {
            const old = e.old_binding.?;
            const img = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch return error.OutOfMemory;
            defer table.gpa.free(img);
            readImage(deps, old, img) catch return error.StorageFull;
            table.fillComplementFrom(e, img, old) catch return error.PoolExhausted;
        } else {
            e.seed = .fresh;
            table.zeroComplete(e);
        }
    }
    return spillValid(table, deps, io, worker, e, bs);
}

fn spillValid(table: *accumulate.Table, deps: Deps, io: Io, worker: *Worker, e: *accumulate.Entry, bs: u64) SpillError!void {
    const key = blockKey(e.key.ino, e.key.block);
    const fencing = deps.map.currentFencing(e.key.ino);
    const staged = staging.stageWrite(deps.pool, deps.seg, key, e.repr.full.data[0..@intCast(bs)], fencing, .none) catch |err| switch (err) {
        error.StorageFull => return error.StorageFull,
        error.OutOfMemory => return error.OutOfMemory,
    };
    stats.staging_put_bytes_spill.add(bs);
    // The unit's reach is this moment's: a truncate that lands before the
    // flush shows in the epoch and the merge clips the unit to the new EOF.
    const unit: Unit = .{ .ino = e.key.ino, .block = e.key.block, .key = key, .owner_generation = staged.generation, .min_size = @as(u64, e.key.block) * bs + e.app_end, .old_binding = e.old_binding, .size_epoch = deps.map.sizeEpoch(e.key.ino) };
    table.retire(e);
    worker.enqueue(io, deps, unit, .spill);
}

// ---------------------------------------------------------------------------
// The worker.
// ---------------------------------------------------------------------------

pub const EnqueueSite = enum { spill, wt_fallback, flush };

pub const Worker = struct {
    gpa: Allocator,
    queue: mpmc.BoundedMpmc(Unit),
    wake: sync_notify.Notify = .init,
    stop: Atomic(bool) = .init(false),
    depth: Atomic(u64) = .init(0),

    pub fn init(gpa: Allocator, cap: usize) Allocator.Error!Worker {
        stats.writeback_queue_cap.set(cap);
        return .{ .gpa = gpa, .queue = try mpmc.BoundedMpmc(Unit).init(gpa, cap) };
    }

    pub fn deinit(self: *Worker) void {
        self.queue.deinit(self.gpa);
        self.* = undefined;
    }

    /// `enqueue_writeback`: a full queue flushes SYNCHRONOUSLY (P1-2 — the
    /// writer pays the flush rather than losing custody or deadlocking the
    /// pass).
    pub fn enqueue(self: *Worker, io: Io, deps: Deps, unit: Unit, site: EnqueueSite) void {
        switch (site) {
            .spill => stats.writeback_enqueued_spill.inc(),
            .wt_fallback => stats.writeback_enqueued_wt_fallback.inc(),
            .flush => stats.writeback_enqueued_flush.inc(),
        }
        if (self.queue.tryPush(unit)) {
            stats.writeback_queue_depth.set(self.depth.fetchAdd(1, .monotonic) + 1);
            self.wake.notifyOne(io);
            return;
        }
        stats.writeback_sync_flushes_on_full_queue.inc();
        var u = unit;
        _ = self.runUnit(io, deps, &u);
    }

    /// One unit through the ladder: a transient requeues (a unit past the
    /// ladder's cap is counted, then requeues at the cap forever).
    fn runUnit(self: *Worker, io: Io, deps: Deps, u: *Unit) Disposition {
        while (true) {
            const d = flushOne(deps, io, u);
            if (d != .retry) return d;
            switch (u.ladder.next()) {
                .backoff_ns => |b| io.sleep(.{ .nanoseconds = @intCast(b) }, .awake) catch {},
                .exhausted => {
                    stats.writeback_retry_exhaustions.inc();
                    io.sleep(.{ .nanoseconds = @intCast(u.ladder.cap_ns) }, .awake) catch {};
                },
            }
            if (self.queue.tryPush(u.*)) {
                stats.writeback_queue_depth.set(self.depth.fetchAdd(1, .monotonic) + 1);
                return d;
            }
            // Full on the requeue: run it again inline (never dropped).
        }
    }

    /// Drain every queued unit once (the test's deterministic pass and the
    /// fsync's "flush what is staged for this ino"), then ONE cover round
    /// for the ring records the units released (`staging.Segment.cover`:
    /// their publishes durable, their slots punched — the write-back's
    /// barrier batching; a no-op when nothing departed).
    pub fn drainOnce(self: *Worker, io: Io, deps: Deps) u32 {
        var n: u32 = 0;
        var budget_units = self.depth.load(.monotonic);
        while (budget_units > 0) : (budget_units -= 1) {
            var u = self.queue.tryPop() orelse break;
            stats.writeback_queue_depth.set(self.depth.fetchSub(1, .monotonic) - 1);
            _ = self.runUnit(io, deps, &u);
            n += 1;
        }
        if (n != 0) deps.seg.cover(io) catch {
            stats.writeback_cover_rounds_failed.inc();
        };
        return n;
    }

    /// The constant worker loop (a `sqz-meta` task): pop → run; park on the
    /// wake with a tick (a lost wake self-heals).
    pub fn run(self: *Worker, io: Io, deps: Deps) Io.Cancelable!void {
        while (!self.stop.load(.acquire)) {
            if (self.drainOnce(io, deps) != 0) continue;
            var w = self.wake.notified();
            _ = w.enable(io);
            if (self.stop.load(.acquire) or self.depth.load(.monotonic) != 0) {
                w.cancel(io);
                continue;
            }
            _ = w.waitUntilUncancelable(io, clock.monotonicNs() + REQUEUE_LADDER.base_ns);
        }
    }

    pub fn requestStop(self: *Worker, io: Io) void {
        self.stop.store(true, .release);
        self.wake.notifyWaiters(io);
    }
};

// ---------------------------------------------------------------------------
// The promotion primitive's binding (charter §6.1).
// ---------------------------------------------------------------------------

/// The inline arm: the payload IS the layout record (`layout.publishLayout`
/// through the KV; the binding's own tx). The seam keeps the core portable.
pub const InlineSeam = struct {
    ctx: *anyopaque,
    publish_inline: *const fn (ctx: *anyopaque, ino: u64, data: []const u8) anyerror!void,
};

pub const PromoteDeps = struct {
    flush: Deps,
    policy: pack.Policy,
    inline_max: u64,
    chunk: u64,
    inline_seam: InlineSeam,
};

pub const Promoted = enum { inline_, own_block, stale, failed };

/// The portable half of the ONE primitive, dispatching on size
/// (`pack.dispatch`): fits inline ⇒ into inline; else its own block through
/// the flush. The packed arm lives in the plane alone (`plane.promoteStaged`
/// → `pack.commitLocked` under the (3.5) + (3.7) section the plane owns),
/// so a packed verdict lands as an own block here.
pub fn promoteStaged(pd: PromoteDeps, io: Io, key: staging.StagingKey, size: u64) Promoted {
    const d = pd.flush;
    const gen = d.seg.residentGeneration(key) orelse return .stale;
    const lease = d.seg.get(key) orelse return .stale;
    const image_len = lease.bytes().len;
    const stored_len = if (pd.flush.transform.scratch_len(pd.flush.transform.ctx, 0) == 0) image_len else image_len + layout.GRAIN;
    const disp = pack.dispatch(pd.policy, size, stored_len, pd.inline_max, pd.chunk) catch {
        lease.release();
        return .failed;
    };
    switch (disp) {
        .inline_ => {
            pd.inline_seam.publish_inline(pd.inline_seam.ctx, key.ino(), lease.bytes()[0..@intCast(@min(size, image_len))]) catch {
                lease.release();
                return .failed;
            };
            lease.release();
            _ = staging.releaseStaged(d.pool, d.seg, key, gen);
            stats.layout_promoted_inline.inc();
            return .inline_;
        },
        .packed_, .own_block => {
            lease.release();
            return promoteOwnBlock(pd, io, key, gen, size);
        },
    }
}

/// `bk:0:len`: the staged image lands as block 0 of a striped layout.
fn promoteOwnBlock(pd: PromoteDeps, io: Io, key: staging.StagingKey, gen: u64, size: u64) Promoted {
    var unit: Unit = .{ .ino = key.ino(), .block = 0, .key = key, .owner_generation = gen, .min_size = size, .old_binding = null };
    return switch (flushOne(pd.flush, io, &unit)) {
        .flushed => blk: {
            stats.layout_promoted_own_block.inc();
            break :blk .own_block;
        },
        .superseded_noop, .nothing, .orphan_discarded, .eof_discarded => .stale,
        .fence_noop, .retry => .failed,
    };
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

const WbWorld = struct {
    f: *wt.Fixture,
    table: accumulate.Table,
    stripes: layout.MetaStripes = .{},
    locks: seams.BlockLocks,
    image: []u8,
    seg: staging.Segment,
    pool: staging.Pool,
    worker: Worker,

    fn init(gpa: Allocator) !*WbWorld {
        const w = try gpa.create(WbWorld);
        errdefer gpa.destroy(w);
        w.f = try wt.Fixture.init(gpa, 32);
        w.table = try accumulate.Table.init(gpa, @intCast(wt.BS), 8);
        w.locks = try seams.BlockLocks.init(gpa, 64);
        w.image = try gpa.alloc(u8, 64 * wt.BS);
        @memset(w.image, 0);
        w.seg = try staging.Segment.init(gpa, w.image, .none);
        w.pool = staging.Pool.init(gpa, 32 * wt.BS, .none);
        w.worker = try Worker.init(gpa, 4);
        w.stripes = .{};
        return w;
    }

    fn deinit(self: *WbWorld, gpa: Allocator) void {
        self.worker.deinit();
        self.pool.deinit();
        self.seg.deinit();
        gpa.free(self.image);
        self.locks.deinit(gpa);
        self.table.deinit();
        self.f.deinit();
        gpa.destroy(self);
    }

    fn pickA(ctx: ?*anyopaque, _: enospc.Place) ?*block_alloc.BlockAllocator {
        return @ptrCast(@alignCast(ctx.?));
    }

    fn deps(self: *WbWorld) Deps {
        return .{ .device = self.f.dev.seam(), .map = self.f.map.seam(), .free = self.f.frees.seam(), .purge = .none, .vols = &self.f.vols, .picker = .{ .ctx = self.f.alloc_a, .pick = &pickA }, .locks = &self.locks, .stripes = &self.stripes, .pool = &self.pool, .seg = &self.seg };
    }
};

test "the spill → flush unit: a parked partial block zero-completes, stages (no DMA), retires; the flush DMAs straight off the ring (one write, one merge), releases the ring entry generation-gated, stamps the touched word; the queue depth settles" {
    const io = testing.io;
    const w = try WbWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(1, .striped, 0);
    const e = w.table.checkout(1, 3, false, null).?;
    var half: [wt.BS / 2]u8 = @splat(0x7A);
    _ = try w.table.recordWrite(e, 0, &half);
    try spillParkedBlock(&w.table, w.deps(), io, &w.worker, 1, 3);
    try testing.expect(!w.table.has(1, 3));
    try testing.expectEqual(@as(usize, 0), w.f.dev.writes.items.len);
    try testing.expect(w.seg.residentGeneration(blockKey(1, 3)) != null);
    try testing.expectEqual(@as(u64, 1), w.worker.depth.load(.monotonic));
    try testing.expectEqual(@as(u32, 1), w.worker.drainOnce(io, w.deps()));
    try testing.expectEqual(@as(u64, 0), w.worker.depth.load(.monotonic));
    try testing.expectEqual(@as(usize, 1), w.f.dev.writes.items.len);
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(3 * wt.BS + wt.BS / 2, w.f.map.merges.items[0].min_size);
    try testing.expect(w.seg.residentGeneration(blockKey(1, 3)) == null); // released
    try testing.expectEqual(@as(u64, 0), w.pool.currentBytes());
    const b = w.f.map.seam().lookup(1, 3).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x7A));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
    try testing.expectEqual(@as(?u32, 3), blockOfKey(blockKey(1, 3)));
    try testing.expect(blockOfKey(staging.StagingKey.init(.{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 }, 1)) == null);
}

test "a spill unit formed BEFORE a truncate merges after it: its block at or past the new EOF is never re-inserted and the size stays the truncate's (the mint returned, the ring entry released, `writeback_eof_discards`); a unit of the same vintage below EOF still lands its bytes and raises no size (red: block 3 came back past EOF with `min_size = 3·bs + app_end` over a size inside block 1 — a later grow reads its bytes instead of zeros; the class of fstests generic/112's size race)" {
    const io = testing.io;
    const w = try WbWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(1, .striped, 0);
    // Two parked half blocks spill — both units formed under this epoch.
    var half: [wt.BS / 2]u8 = @splat(0x7A);
    for ([_]u32{ 0, 3 }) |b| {
        const e = w.table.checkout(1, b, false, null).?;
        _ = try w.table.recordWrite(e, 0, &half);
        try spillParkedBlock(&w.table, w.deps(), io, &w.worker, 1, b);
    }
    try testing.expectEqual(@as(u64, 2), w.worker.depth.load(.monotonic));
    try testing.expectEqual(@as(u64, 0), w.f.map.sizeOf(1));
    // The truncate lands first: a size inside block 1 (block 0 kept, block 3 dead).
    const cut = wt.BS + wt.BS / 4;
    try w.f.map.truncate(1, cut);
    const used = w.f.alloc_a.usedBlocks();
    const eof0 = stats.writeback_eof_discards.load();
    try testing.expectEqual(@as(u32, 2), w.worker.drainOnce(io, w.deps()));
    // Block 0's bytes landed (below EOF), block 3 did not; the size is the truncate's.
    try testing.expect(w.f.map.seam().lookup(1, 0) != null);
    try testing.expect(w.f.map.seam().lookup(1, 3) == null);
    try testing.expectEqual(cut, w.f.map.sizeOf(1));
    try testing.expectEqual(@as(usize, 2), w.f.map.merges.items.len);
    try testing.expectEqual(@as(u8, 1), w.f.map.merges.items[0].blocks);
    try testing.expectEqual(@as(u64, 0), w.f.map.merges.items[0].min_size);
    try testing.expectEqual(@as(u8, 0), w.f.map.merges.items[1].blocks);
    // One block named; the clipped unit's mint went back.
    try testing.expectEqual(used + 1, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(eof0 + 1, stats.writeback_eof_discards.load());
    // Both ring entries released: the dead bytes leave with the unit.
    try testing.expect(w.seg.residentGeneration(blockKey(1, 0)) == null);
    try testing.expect(w.seg.residentGeneration(blockKey(1, 3)) == null);
    try testing.expectEqual(@as(u64, 0), w.pool.currentBytes());
    // A unit formed AFTER the truncate is current: it lands and raises.
    const e5 = w.table.checkout(1, 5, false, null).?;
    _ = try w.table.recordWrite(e5, 0, &half);
    try spillParkedBlock(&w.table, w.deps(), io, &w.worker, 1, 5);
    try testing.expectEqual(@as(u32, 1), w.worker.drainOnce(io, w.deps()));
    try testing.expect(w.f.map.seam().lookup(1, 5) != null);
    try testing.expectEqual(5 * wt.BS + wt.BS / 2, w.f.map.sizeOf(1));
}

test "never-lossy dispositions: a superseded unit is a contractual no-op (newest bytes win); a stale fencing token converges by re-presenting; a verified-NotFound discards the orphan and the ring entry; a transient requeues (never sticky); the fence with custody poisoned is a verified no-op" {
    const io = testing.io;
    const w = try WbWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(2, .striped, 0);
    var data: [wt.BS]u8 = @splat(0x01);
    // Stage twice: the first unit's generation is superseded.
    const g1 = try staging.stageWrite(&w.pool, &w.seg, blockKey(2, 0), &data, 1, .none);
    var unit_a: Unit = .{ .ino = 2, .block = 0, .key = blockKey(2, 0), .owner_generation = g1.generation, .min_size = wt.BS, .old_binding = null };
    @memset(&data, 0x02);
    const g2 = try staging.stageWrite(&w.pool, &w.seg, blockKey(2, 0), &data, 1, .none);
    try testing.expectEqual(Disposition.superseded_noop, flushOne(w.deps(), io, &unit_a));
    var unit_b: Unit = .{ .ino = 2, .block = 0, .key = blockKey(2, 0), .owner_generation = g2.generation, .min_size = wt.BS, .old_binding = null };
    // Stale token: the first merge refuses, the unit re-presents the current generation.
    w.f.map.fail_next_merge = error.FencingTokenExpired;
    const stale0 = stats.writeback_stale_token_retries.load();
    try testing.expectEqual(Disposition.flushed, flushOne(w.deps(), io, &unit_b));
    try testing.expectEqual(stale0 + 1, stats.writeback_stale_token_retries.load());
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, w.f.map.seam().lookup(2, 0).?.offset, &got);
    try testing.expect(std.mem.allEqual(u8, &got, 0x02));
    // NotFound: the ino is gone — orphan discarded, ring entry released, mint returned.
    const g3 = try staging.stageWrite(&w.pool, &w.seg, blockKey(3, 0), &data, 1, .none);
    var unit_c: Unit = .{ .ino = 3, .block = 0, .key = blockKey(3, 0), .owner_generation = g3.generation, .min_size = wt.BS, .old_binding = null };
    const used = w.f.alloc_a.usedBlocks();
    try testing.expectEqual(Disposition.orphan_discarded, flushOne(w.deps(), io, &unit_c));
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expect(w.seg.residentGeneration(blockKey(3, 0)) == null);
    // Transient: requeue, bytes stay staged, the next pass lands.
    try w.f.map.create(4, .striped, 0);
    const g4 = try staging.stageWrite(&w.pool, &w.seg, blockKey(4, 0), &data, 1, .none);
    var unit_d: Unit = .{ .ino = 4, .block = 0, .key = blockKey(4, 0), .owner_generation = g4.generation, .min_size = wt.BS, .old_binding = null };
    w.f.map.fail_next_merge = error.Transient;
    try testing.expectEqual(Disposition.retry, flushOne(w.deps(), io, &unit_d));
    try testing.expect(w.seg.residentGeneration(blockKey(4, 0)) != null);
    try testing.expectEqual(Disposition.flushed, flushOne(w.deps(), io, &unit_d));
    // Fence + poisoned custody: a verified no-op, the bytes stay staged for the remount.
    try w.f.map.create(5, .striped, 0);
    const g5 = try staging.stageWrite(&w.pool, &w.seg, blockKey(5, 0), &data, 1, .none);
    var unit_e: Unit = .{ .ino = 5, .block = 0, .key = blockKey(5, 0), .owner_generation = g5.generation, .min_size = wt.BS, .old_binding = null };
    const P = struct {
        fn poisoned() bool {
            return true;
        }
    };
    var d = w.deps();
    d.custody_poisoned = &P.poisoned;
    w.f.map.fail_next_merge = error.WriterGuardFenced;
    const fn0 = stats.writeback_fence_noops.load();
    try testing.expectEqual(Disposition.fence_noop, flushOne(d, io, &unit_e));
    try testing.expect(w.seg.residentGeneration(blockKey(5, 0)) != null);
    try testing.expectEqual(fn0 + 1, stats.writeback_fence_noops.load());
}

test "writeback_requeue_on_full_queue_never_goes_sticky: a full queue flushes synchronously; exhausted retries are counted and the unit still lands" {
    const io = testing.io;
    const w = try WbWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    var data: [wt.BS]u8 = @splat(0x33);
    var i: u64 = 10;
    // Fill the 4-slot queue with units for inos that exist.
    while (i < 14) : (i += 1) {
        try w.f.map.create(i, .striped, 0);
        const g = try staging.stageWrite(&w.pool, &w.seg, blockKey(i, 0), &data, 1, .none);
        w.worker.enqueue(io, w.deps(), .{ .ino = i, .block = 0, .key = blockKey(i, 0), .owner_generation = g.generation, .min_size = wt.BS, .old_binding = null }, .flush);
    }
    try testing.expectEqual(@as(u64, 4), w.worker.depth.load(.monotonic));
    // The fifth: the queue is full ⇒ flushed synchronously now.
    try w.f.map.create(14, .striped, 0);
    const g = try staging.stageWrite(&w.pool, &w.seg, blockKey(14, 0), &data, 1, .none);
    const sync0 = stats.writeback_sync_flushes_on_full_queue.load();
    w.worker.enqueue(io, w.deps(), .{ .ino = 14, .block = 0, .key = blockKey(14, 0), .owner_generation = g.generation, .min_size = wt.BS, .old_binding = null }, .flush);
    try testing.expectEqual(sync0 + 1, stats.writeback_sync_flushes_on_full_queue.load());
    try testing.expect(w.f.map.seam().lookup(14, 0) != null);
    try testing.expectEqual(@as(u32, 4), w.worker.drainOnce(io, w.deps()));
    // A unit whose ladder is exhausted still requeues and lands: five transients then success.
    try w.f.map.create(15, .striped, 0);
    const g15 = try staging.stageWrite(&w.pool, &w.seg, blockKey(15, 0), &data, 1, .none);
    var u: Unit = .{ .ino = 15, .block = 0, .key = blockKey(15, 0), .owner_generation = g15.generation, .min_size = wt.BS, .old_binding = null, .ladder = .{ .base_ns = 1, .cap_ns = 1, .attempts_max = 2 } };
    const ex0 = stats.writeback_retry_exhaustions.load();
    var k: u32 = 0;
    while (k < 3) : (k += 1) {
        w.f.map.fail_next_merge = error.Transient;
        _ = w.worker.runUnit(io, w.deps(), &u);
        _ = w.worker.queue.tryPop();
        _ = w.worker.depth.fetchSub(1, .monotonic);
    }
    try testing.expectEqual(ex0 + 1, stats.writeback_retry_exhaustions.load());
    try testing.expectEqual(Disposition.flushed, w.worker.runUnit(io, w.deps(), &u));
}

test "the promotion primitive dispatches on size: fits inline ⇒ inline; a large image ⇒ its own block (bk:0:len) with the ring entry released; the stale identity is a no-op" {
    const io = testing.io;
    const w = try WbWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    const Inl = struct {
        var published: u32 = 0;
        var last_len: usize = 0;
        fn publish(_: *anyopaque, _: u64, data: []const u8) anyerror!void {
            published += 1;
            last_len = data.len;
        }
    };
    var dummy: u8 = 0;
    const pd: PromoteDeps = .{ .flush = w.deps(), .policy = pack.Policy.resolve(false, null, wt.BS), .inline_max = 4096, .chunk = wt.BS, .inline_seam = .{ .ctx = &dummy, .publish_inline = &Inl.publish } };
    // Inline.
    try w.f.map.create(20, .staged, 100);
    const fid: layout.FileId = .{ 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9 };
    const k = staging.StagingKey.init(fid, 20);
    var small: [100]u8 = @splat(0x5C);
    _ = try staging.stageWrite(&w.pool, &w.seg, k, &small, 1, .none);
    try testing.expectEqual(Promoted.inline_, promoteStaged(pd, io, k, 100));
    try testing.expectEqual(@as(u32, 1), Inl.published);
    try testing.expectEqual(@as(usize, 100), Inl.last_len);
    try testing.expect(w.seg.residentGeneration(k) == null);
    // Own block.
    try w.f.map.create(21, .staged, wt.BS / 2);
    const k2 = staging.StagingKey.init(fid, 21);
    var big: [wt.BS / 2]u8 = @splat(0x6D);
    _ = try staging.stageWrite(&w.pool, &w.seg, k2, &big, 1, .none);
    try testing.expectEqual(Promoted.own_block, promoteStaged(pd, io, k2, wt.BS / 2));
    try testing.expect(w.seg.residentGeneration(k2) == null);
    const b = w.f.map.seam().lookup(21, 0).?;
    try testing.expect(b.whole_block);
    try testing.expectEqual(seams.FileKind.striped, w.f.map.seam().view(21).?.kind);
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x6D));
    try testing.expectEqual(@as(u64, 0), w.pool.currentBytes());
    // Stale identity: nothing staged.
    try testing.expectEqual(Promoted.stale, promoteStaged(pd, io, k2, wt.BS / 2));
}
