//! fsync (charter §6.4; `docs/porting/data_write_path.md` §10): the
//! exact-sum `fsync_phase_ns` family (`intent_barrier / data_flush /
//! staged_promote / staged_sync / data_barrier / meta_publish /
//! meta_barrier / extent_barrier / total`), barrier only the data
//! namespaces the ino TOUCHED (a per-ino stripe word stamped at every
//! layout publish and in-place DMA; CAS stamp; conservative clear),
//! write-through namespaces skip, independent legs joined and ALL awaited
//! (never cancel a sibling — a dropped barrier future fails the coalescer's
//! queued waiters), the intent barrier first, the staged record's ranged
//! sync and the data barrier strictly before the meta legs that name the
//! blocks (DUR-1), the meta barrier last. `fsync` takes NO inode write
//! guard. Strict vs coalesced is the backend's own barrier API (the
//! `MetaSyncSeam`).
//!
//! The staged leg (charter §1 item 10, amended 2026-10-19): a staged ino's
//! bytes live in the staging ring's mmap, the page cache's until synced; the
//! fsync makes the resident record reach stable storage (`Segment.
//! syncResident`: ONE ranged sync of the segment file, coalesced across
//! concurrent fsyncs) BEFORE the meta barrier that makes the layout naming
//! it durable — a durable layout must never name bytes a power cut took.
//! Under `SQUEEZEFS_FSYNC_PROMOTE_STAGED` the promotion runs first and a
//! promoted record owes no sync (its block is the data barrier's).
//!
//! The touched table: one `AtomicU64` per stripe over the splitmix mix at
//! the DLM width — low 32 bits = per-device ordinal bits (`MAX_ORDINAL =
//! 30`, bit 31 = `ALL_BIT`, the unresolvable fallback), high 32 = a stamp
//! generation. `stamp` ORs the bits AND bumps the generation in ONE CAS;
//! `observe` reads BEFORE the barriers start; `clearObserved` CASes
//! `observed → observed & ~BITS` — it succeeds only if NO stamp landed
//! since (conservative: a failure leaves the bits for the next fsync).
//! Stripes are SHARED by construction and that is CORRECT: a device barrier
//! covers every write completed before it started, whoever issued it — a
//! false share costs one extra coalesced barrier, never a missed one.

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const dlm = @import("../meta/dlm.zig");
const staging = @import("staging.zig");
const seams = @import("write_seams.zig");
const device_overlay = @import("device_overlay.zig");
const enospc = @import("enospc.zig");
const accumulate = @import("accumulate.zig");
const epoch_mod = @import("write_epoch.zig");
const writeback = @import("writeback.zig");
const pipeline = @import("write_pipeline.zig");

// ---------------------------------------------------------------------------
// §10.1 The exact-sum phase family.
// ---------------------------------------------------------------------------

pub const Phase = enum(u8) {
    intent_barrier,
    data_flush,
    staged_promote,
    staged_sync,
    data_barrier,
    meta_publish,
    meta_barrier,
    extent_barrier,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
    pub const legs = count - 1;
};

pub const stats = struct {
    pub var fsync_phase_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    pub var fsync_calls: counters.ShardedCounter = .{};
    pub var fsync_data_namespaces_touched: counters.ShardedCounter = .{};
    pub var fsync_data_namespaces_flushed: counters.ShardedCounter = .{};
    pub var fsync_write_through_skips: counters.ShardedCounter = .{};
    pub var fsync_noop_clean: counters.ShardedCounter = .{};
    pub var fsync_parallel_joins: counters.ShardedCounter = .{};
    pub var fsync_flush_enospc_retries: counters.ShardedCounter = .{};
    pub var fsync_promote_noops: counters.ShardedCounter = .{};
    pub var fsync_promote_failures: counters.ShardedCounter = .{};
    /// Should stay 0: an ordinal above `MAX_ORDINAL` fell to `ALL_BIT`.
    pub var fsync_touched_unresolved: counters.ShardedCounter = .{};
    /// The mount-wide ladder (`syncfs`): its calls, the distinct inos its
    /// census found holding write custody, the parked blocks it flushed.
    pub var syncfs_calls: counters.ShardedCounter = .{};
    pub var syncfs_custody_inos: counters.ShardedCounter = .{};
    pub var syncfs_parked_blocks: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "fsync_calls", "fsync_data_namespaces_touched", "fsync_data_namespaces_flushed", "fsync_write_through_skips", "fsync_noop_clean", "fsync_parallel_joins", "fsync_flush_enospc_retries", "fsync_promote_noops", "fsync_promote_failures", "fsync_touched_unresolved", "syncfs_calls", "syncfs_custody_inos", "syncfs_parked_blocks" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.writeAll("\"fsync_phase_ns\":{");
    inline for (@typeInfo(Phase).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{name});
        try stats.fsync_phase_ns[i].snapshot().writeJson(w);
    }
    try w.writeAll("}}");
}

/// A value on the handler frame: `mark(p)` records the span since the
/// previous boundary; `finish` records 0 ns for every unmarked leg and
/// `total = last − t0`, so `Σ phases ≡ total` to the nanosecond on every
/// exit with equal counts per phase.
pub const Prof = struct {
    t0: u64,
    last: u64,
    legs: [Phase.legs]u64 = @splat(0),

    pub fn start() Prof {
        return startAt(clock.monotonicNs());
    }

    pub fn startAt(now: u64) Prof {
        return .{ .t0 = now, .last = now };
    }

    pub fn mark(self: *Prof, p: Phase) void {
        self.markAt(p, clock.monotonicNs());
    }

    pub fn markAt(self: *Prof, p: Phase, now: u64) void {
        assert(p != .total);
        const t = @max(now, self.last);
        self.legs[@backingInt(p)] += t - self.last;
        self.last = t;
    }

    pub fn finish(self: *Prof) void {
        for (self.legs, 0..) |ns, i| stats.fsync_phase_ns[i].record(ns);
        stats.fsync_phase_ns[@backingInt(Phase.total)].record(self.last - self.t0);
    }

    pub fn total(self: *const Prof) u64 {
        return self.last - self.t0;
    }
};

/// The syncfs family (`fuse_syncfs_phase_ns`): the per-file ladder's legs
/// for the whole mount, framed by the filesystem's own two (the parked
/// stamps first, the store-wide meta barrier last). Exact-sum like `Phase`.
pub const SyncfsPhase = enum(u8) {
    times_drain,
    data_flush,
    staged_promote,
    staged_sync,
    data_barrier,
    meta_publish,
    meta_barrier,
    total,

    pub const count = @typeInfo(SyncfsPhase).@"enum".field_names.len;
    pub const legs = count - 1;
};

/// One histogram per `SyncfsPhase`, by name (the `.stats` keys).
pub const SyncfsPhaseNs = struct {
    times_drain: counters.ShardedLatencyHistogram = .{},
    data_flush: counters.ShardedLatencyHistogram = .{},
    staged_promote: counters.ShardedLatencyHistogram = .{},
    staged_sync: counters.ShardedLatencyHistogram = .{},
    data_barrier: counters.ShardedLatencyHistogram = .{},
    meta_publish: counters.ShardedLatencyHistogram = .{},
    meta_barrier: counters.ShardedLatencyHistogram = .{},
    total: counters.ShardedLatencyHistogram = .{},

    comptime {
        const names = @typeInfo(SyncfsPhase).@"enum".field_names;
        const fields = @typeInfo(SyncfsPhaseNs).@"struct".field_names;
        assert(names.len == fields.len);
        for (names, fields) |n, f| assert(std.mem.eql(u8, n, f));
    }
};

/// `Prof`'s law over `SyncfsPhase`: `finish` records 0 ns for every
/// unmarked leg and `total = last − t0`, so Σ legs ≡ total on every exit.
pub const SyncfsProf = struct {
    t0: u64,
    last: u64,
    legs: [SyncfsPhase.legs]u64 = @splat(0),

    pub fn start() SyncfsProf {
        return startAt(clock.monotonicNs());
    }

    pub fn startAt(now: u64) SyncfsProf {
        return .{ .t0 = now, .last = now };
    }

    pub fn mark(self: *SyncfsProf, p: SyncfsPhase) void {
        self.markAt(p, clock.monotonicNs());
    }

    pub fn markAt(self: *SyncfsProf, p: SyncfsPhase, now: u64) void {
        assert(p != .total);
        const t = @max(now, self.last);
        self.legs[@backingInt(p)] += t - self.last;
        self.last = t;
    }

    pub fn finish(self: *SyncfsProf, into: *SyncfsPhaseNs) void {
        inline for (@typeInfo(SyncfsPhase).@"enum".field_names, 0..) |name, i| {
            @field(into, name).record(if (i == SyncfsPhase.legs) self.last - self.t0 else self.legs[i]);
        }
    }

    pub fn total(self: *const SyncfsProf) u64 {
        return self.last - self.t0;
    }
};

// ---------------------------------------------------------------------------
// §10.3 The per-ino stripe word.
// ---------------------------------------------------------------------------

pub const TouchedTable = struct {
    words: []Atomic(u64),
    mask: u64,

    /// 32 ordinal bits + 32 generation bits in one u64.
    pub const MAX_ORDINAL: u32 = 30;
    pub const ALL_BIT: u64 = 1 << 31;
    pub const BITS_MASK: u64 = 0xFFFF_FFFF;
    pub const GEN_STEP: u64 = 1 << 32;

    pub fn init(gpa: Allocator, stripes: usize) Allocator.Error!TouchedTable {
        assert(std.math.isPowerOfTwo(stripes));
        const words = try gpa.alloc(Atomic(u64), stripes);
        @memset(words, .init(0));
        return .{ .words = words, .mask = stripes - 1 };
    }

    pub fn deinit(self: *TouchedTable, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    pub fn bitForOrdinal(ordinal: u32) u64 {
        if (ordinal > MAX_ORDINAL) {
            stats.fsync_touched_unresolved.inc();
            return ALL_BIT;
        }
        return @as(u64, 1) << @intCast(ordinal);
    }

    fn word(self: *TouchedTable, ino: u64) *Atomic(u64) {
        return &self.words[@intCast(dlm.splitmix64(ino) & self.mask)];
    }

    /// ORs `bits` AND bumps the generation in ONE CAS (AcqRel: pairs with
    /// `observe`'s acquire and `clearObserved`'s CAS).
    pub fn stamp(self: *TouchedTable, ino: u64, bits: u64) void {
        const w = self.word(ino);
        var cur = w.load(.monotonic);
        while (true) {
            cur = w.cmpxchgWeak(cur, (cur | (bits & BITS_MASK)) +% GEN_STEP, .acq_rel, .monotonic) orelse return;
        }
    }

    pub fn stampOrdinal(self: *TouchedTable, ino: u64, ordinal: u32) void {
        self.stamp(ino, bitForOrdinal(ordinal));
    }

    /// Read BEFORE the barriers start.
    pub fn observe(self: *TouchedTable, ino: u64) u64 {
        return self.word(ino).load(.acquire);
    }

    /// Succeeds only if NO stamp landed since `observed`; a failure leaves
    /// the bits for the next fsync (conservative).
    pub fn clearObserved(self: *TouchedTable, ino: u64, observed: u64) bool {
        return self.word(ino).cmpxchgStrong(observed, observed & ~BITS_MASK, .acq_rel, .monotonic) == null;
    }

    pub fn bitsOf(observed: u64) u64 {
        return observed & BITS_MASK;
    }

    /// The syncfs snapshot: every stripe word read BEFORE the barriers
    /// start (`out.len == words.len`).
    pub fn observeAll(self: *TouchedTable, out: []u64) void {
        assert(out.len == self.words.len);
        for (self.words, out) |*w, *o| o.* = w.load(.acquire);
    }

    /// After the all-namespace barrier: each word clears iff no stamp
    /// landed since its snapshot (`clearObserved`'s law, word by word).
    pub fn clearAllObserved(self: *TouchedTable, observed: []const u64) void {
        assert(observed.len == self.words.len);
        for (self.words, observed) |*w, o| _ = w.cmpxchgStrong(o, o & ~BITS_MASK, .acq_rel, .monotonic);
    }
};

/// A device barriers iff touched (or ALL) AND its acknowledged writes are
/// volatile; write-through namespaces skip (counted). Pure.
pub fn planBarriers(bits: u64, vols: *const seams.Volumes, out: *[256]u32) []u32 {
    var n: usize = 0;
    for (vols.rows[0..vols.n()]) |*r| {
        const v = r.*.?;
        const touched = bits & TouchedTable.ALL_BIT != 0 or (v.ordinal <= TouchedTable.MAX_ORDINAL and bits & (@as(u64, 1) << @intCast(v.ordinal)) != 0);
        if (!touched) continue;
        stats.fsync_data_namespaces_touched.inc();
        if (!v.write_cache_volatile) {
            stats.fsync_write_through_skips.inc();
            continue;
        }
        out[n] = v.ordinal;
        n += 1;
    }
    return out[0..n];
}

// ---------------------------------------------------------------------------
// §10.2 The ladder.
// ---------------------------------------------------------------------------

/// The S11 intent barrier (`meta_ship::intents::fsync_ino_barrier`):
/// sequential, first; its errno short-circuits. Absent = no intents.
pub const IntentSeam = struct {
    ctx: ?*anyopaque,
    barrier: *const fn (ctx: ?*anyopaque, io: Io, ino: u64) anyerror!void,
};

/// The backend's `sync_device_for_ino`: strict vs coalesced is ITS law.
pub const MetaSyncSeam = struct {
    ctx: ?*anyopaque,
    sync: *const fn (ctx: ?*anyopaque, io: Io, ino: u64) anyerror!void,
};

pub const Levers = struct {
    /// `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES` (off ⇒ every namespace barriers).
    touched_namespaces: bool = true,
    /// `SQUEEZEFS_FSYNC_PARALLEL_LEGS`.
    parallel_legs: bool = true,
    /// `SQUEEZEFS_FSYNC_PROMOTE_STAGED`.
    promote_staged: staging.FsyncLever = .{ .on = false },

    pub fn fromEnv(env: *const std.process.Environ.Map) Levers {
        return .{
            .touched_namespaces = knobs.boolKnob("SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES", env),
            .parallel_legs = knobs.boolKnob("SQUEEZEFS_FSYNC_PARALLEL_LEGS", env),
            .promote_staged = staging.FsyncLever.fromEnv(env),
        };
    }
};

pub const Ctx = struct {
    levers: Levers,
    device: seams.DeviceSeam,
    vols: *const seams.Volumes,
    touched: *TouchedTable,
    locks: *seams.BlockLocks,
    overlay: *device_overlay.Overlay,
    table: *accumulate.Table,
    upload: accumulate.UploadDeps,
    epochs: *epoch_mod.Epochs,
    close_seams: epoch_mod.Epochs.CloseSeams,
    worker: ?*writeback.Worker = null,
    wb_deps: ?writeback.Deps = null,
    intent: ?IntentSeam = null,
    meta_sync: MetaSyncSeam,
    /// The S11 FlushExtents force (rung 6); absent = 0 ns leg.
    extent_barrier: ?MetaSyncSeam = null,
    /// The staged file's key when the ino is staged (the promote lever,
    /// then the ranged sync of its record).
    staged_key: ?staging.StagingKey = null,
    promotion: staging.PromotionHooks = .none,
    /// The staging ring (the staged legs of both ladders; null = staging off).
    staged_segment: ?*staging.Segment = null,
};

pub const Error = error{ InputOutput, NoSpaceLeft, WriterGuardFenced, VolumeFailed, OutOfMemory, IntentBarrierFailed, MetaBarrierFailed };

/// DUR-1: the WHOLE data barrier step completes — every leg Ok — strictly
/// before the meta legs that name the blocks; a failed leg fails the fsync
/// with the touched bits left set.
pub fn fsync(ctx: Ctx, io: Io, ino: u64, prof: *Prof) Error!void {
    stats.fsync_calls.inc();
    // INTENT BARRIER.
    if (ctx.intent) |it| it.barrier(it.ctx, io, ino) catch return error.IntentBarrierFailed;
    prof.mark(.intent_barrier);
    // DATA FLUSH: the overlay drain (barrier = true) first; then every
    // parked block of the ino escalates to one durable upload (DUR-1); the
    // ENOSPC retry: flush the data devices → close THIS ino's epoch → once
    // more; a second wall is the honest ENOSPC.
    const left = ctx.overlay.drainForIno(io, ctx.locks, ino, std.math.maxInt(u64), true, ctx.close_seams);
    if (left != 0) device_overlay.stats.overlay_unpublished_at_fsync.fire("overlay records left open after the fsync drain");
    try flushParked(ctx, io, ino, false);
    if (ctx.worker) |w| if (ctx.wb_deps) |d| {
        _ = w.drainOnce(io, d);
    };
    prof.mark(.data_flush);
    // STAGED PROMOTE (the lever's posture, OFF by default).
    if (ctx.staged_key) |k| {
        staging.promoteAtFsync(ctx.levers.promote_staged, ctx.promotion, k);
    } else if (ctx.levers.promote_staged.on) {
        stats.fsync_promote_noops.inc();
    }
    prof.mark(.staged_promote);
    // STAGED SYNC: the record still resident reaches stable storage (a
    // promoted one left the ring and owes nothing). Before the meta legs:
    // a durable layout must never name bytes a power cut took.
    if (ctx.staged_key) |k| if (ctx.staged_segment) |seg| {
        seg.syncResident(io, k) catch return error.InputOutput;
    };
    prof.mark(.staged_sync);
    // DATA BARRIER: observe → plan → legs joined and ALL awaited → clear.
    const observed = ctx.touched.observe(ino);
    const bits: u64 = if (ctx.levers.touched_namespaces) TouchedTable.bitsOf(observed) else TouchedTable.ALL_BIT;
    var plan_buf: [256]u32 = undefined;
    const plan = planBarriers(bits, ctx.vols, &plan_buf);
    if (plan.len == 0) stats.fsync_noop_clean.inc();
    try runBarrierLegs(ctx, io, plan);
    stats.fsync_data_namespaces_flushed.add(plan.len);
    _ = ctx.touched.clearObserved(ino, observed);
    prof.mark(.data_barrier);
    // META PUBLISH: the rewrite epoch's ONE save.
    switch (ctx.epochs.close(io, ino, ctx.close_seams)) {
        .fence_dropped => return error.WriterGuardFenced,
        .transient => return error.InputOutput,
        else => {},
    }
    prof.mark(.meta_publish);
    // META BARRIER.
    ctx.meta_sync.sync(ctx.meta_sync.ctx, io, ino) catch return error.MetaBarrierFailed;
    prof.mark(.meta_barrier);
    // EXTENT BARRIER.
    if (ctx.extent_barrier) |eb| eb.sync(eb.ctx, io, ino) catch return error.MetaBarrierFailed;
    prof.mark(.extent_barrier);
}

/// THE ORDERED LAW's data barrier (decision 0003 §AL.5): fsync's DATA
/// BARRIER leg once for a batch of inos — every volatile namespace they
/// touched since their last barrier flushed — before the times drain
/// journals their size claims. Their custody below the claims landed
/// already (the drain clips each claim to its custody floor).
pub fn barrierInos(ctx: Ctx, io: Io, inos: []const u64) Error!void {
    var observed: [64]u64 = undefined;
    assert(inos.len <= observed.len);
    var bits: u64 = 0;
    for (inos, observed[0..inos.len]) |ino, *o| {
        o.* = ctx.touched.observe(ino);
        bits |= TouchedTable.bitsOf(o.*);
    }
    if (!ctx.levers.touched_namespaces) bits = TouchedTable.ALL_BIT;
    var plan_buf: [256]u32 = undefined;
    const plan = planBarriers(bits, ctx.vols, &plan_buf);
    try runBarrierLegs(ctx, io, plan);
    stats.fsync_data_namespaces_flushed.add(plan.len);
    for (inos, observed[0..inos.len]) |ino, o| _ = ctx.touched.clearObserved(ino, o);
}

/// syncfs (FUSE_SYNCFS): `fsync`'s data legs for EVERY ino holding write
/// custody when the census runs — the accumulation table's parked blocks
/// (by key: no per-ino page bound), the device overlay's open records, the
/// rewrite epochs open or saving — then the staged promote (lever), ONE
/// data barrier over every volatile namespace, every census ino's epoch
/// closed (the meta publish). DUR-1 holds mount-wide: the data barrier
/// completes before any epoch save names a block. The meta barrier is the
/// caller's (the store-wide one, after its own legs). Custody born after
/// the census is a write the syncfs did not see begin.
pub fn syncfs(ctx: Ctx, gpa: Allocator, io: Io, prof: *SyncfsProf) Error!void {
    stats.syncfs_calls.inc();
    var keys: std.ArrayList(accumulate.Key) = .empty;
    defer keys.deinit(gpa);
    try ctx.table.censusKeys(gpa, &keys);
    var inos: std.ArrayList(u64) = .empty;
    defer inos.deinit(gpa);
    try inos.ensureUnusedCapacity(gpa, keys.items.len);
    for (keys.items) |k| inos.appendAssumeCapacity(k.ino);
    try ctx.overlay.registry.censusInos(gpa, &inos);
    try ctx.epochs.censusInos(gpa, &inos);
    std.mem.sort(u64, inos.items, {}, std.sort.asc(u64));
    const distinct = dedupeSorted(inos.items);
    stats.syncfs_custody_inos.add(distinct.len);
    // DATA FLUSH: every overlay drained with its barrier; every parked
    // block escalated to one durable upload (the ENOSPC ladder once).
    for (distinct) |ino| {
        const left = ctx.overlay.drainForIno(io, ctx.locks, ino, std.math.maxInt(u64), true, ctx.close_seams);
        if (left != 0) device_overlay.stats.overlay_unpublished_at_fsync.fire("overlay records left open after the syncfs drain");
    }
    try flushParkedKeys(ctx, io, keys.items);
    if (ctx.worker) |w| if (ctx.wb_deps) |d| {
        _ = w.drainOnce(io, d);
    };
    prof.mark(.data_flush);
    // STAGED PROMOTE (the lever's posture, OFF by default) — then STAGED
    // SYNC: every record still resident reaches stable storage in ONE
    // whole-ring sync (the kernel writes its dirty pages alone), exactly as
    // at a per-file fsync.
    if (ctx.staged_segment) |seg| try staging.promoteAllAtSyncfs(gpa, ctx.levers.promote_staged, seg, ctx.promotion);
    prof.mark(.staged_promote);
    if (ctx.staged_segment) |seg| seg.syncAll(io) catch return error.InputOutput;
    prof.mark(.staged_sync);
    // DATA BARRIER: every volatile namespace; the stripe words snapshotted
    // before it clear after it (a stamp since keeps its bits).
    const observed = try gpa.alloc(u64, ctx.touched.words.len);
    defer gpa.free(observed);
    ctx.touched.observeAll(observed);
    var plan_buf: [256]u32 = undefined;
    const plan = planBarriers(TouchedTable.ALL_BIT, ctx.vols, &plan_buf);
    try runBarrierLegs(ctx, io, plan);
    stats.fsync_data_namespaces_flushed.add(plan.len);
    ctx.touched.clearAllObserved(observed);
    prof.mark(.data_barrier);
    // META PUBLISH: every census ino's ONE save (a save in flight awaited).
    for (distinct) |ino| switch (ctx.epochs.close(io, ino, ctx.close_seams)) {
        .fence_dropped => return error.WriterGuardFenced,
        .transient => return error.InputOutput,
        else => {},
    };
    prof.mark(.meta_publish);
}

/// The unique prefix of an ascending slice, in place.
fn dedupeSorted(xs: []u64) []u64 {
    if (xs.len == 0) return xs;
    var n: usize = 1;
    for (xs[1..]) |x| {
        if (x == xs[n - 1]) continue;
        xs[n] = x;
        n += 1;
    }
    return xs[0..n];
}

/// `flushParked`'s ladder over census keys: each key one durable upload; a
/// key left parked (the pool's ENOSPC) gets the ladder once — flush the
/// data devices, close every epoch for its supply — then one retry; a
/// second wall is the honest ENOSPC.
fn flushParkedKeys(ctx: Ctx, io: Io, keys: []const accumulate.Key) Error!void {
    var parked_after: usize = 0;
    for (keys) |k| {
        switch (accumulate.flushParkedBlock(ctx.table, ctx.upload, io, k.ino, k.block)) {
            .published => stats.syncfs_parked_blocks.inc(),
            .discarded, .joined => {},
            .fence_drop => return error.WriterGuardFenced,
            .stay_parked => parked_after += 1,
        }
    }
    if (parked_after == 0) return;
    stats.fsync_flush_enospc_retries.inc();
    var vb: [256]u32 = undefined;
    for (planBarriers(TouchedTable.ALL_BIT, ctx.vols, &vb)) |ord| {
        if (ctx.vols.byOrdinal(ord)) |v| ctx.device.flush(v.vol_tag) catch {};
    }
    _ = ctx.epochs.closeAllForSpace(io, ctx.close_seams);
    for (keys) |k| {
        switch (accumulate.flushParkedBlock(ctx.table, ctx.upload, io, k.ino, k.block)) {
            .published => stats.syncfs_parked_blocks.inc(),
            .discarded, .joined => {},
            .fence_drop => return error.WriterGuardFenced,
            .stay_parked => return error.NoSpaceLeft,
        }
    }
}

/// Every block the ino parked before this fsync began (`flushBornBy`: all
/// of them, not one buffer's worth — the ordered law journals the claim
/// the fsync found on the strength of this leg).
fn flushParked(ctx: Ctx, io: Io, ino: u64, retried: bool) Error!void {
    const left = accumulate.flushBornBy(ctx.table, ctx.upload, io, ino, clock.monotonicNs());
    if (left.fenced) return error.WriterGuardFenced;
    if (left.stay_parked == 0) return;
    if (retried) return error.NoSpaceLeft;
    // The ENOSPC ladder: a data barrier releases nothing by itself but
    // orders the parked supply's close; the parking close of THIS ino's
    // epoch frees its displaced keys; then ONE retry.
    stats.fsync_flush_enospc_retries.inc();
    var vb: [256]u32 = undefined;
    for (planBarriers(TouchedTable.ALL_BIT, ctx.vols, &vb)) |ord| {
        if (ctx.vols.byOrdinal(ord)) |v| ctx.device.flush(v.vol_tag) catch {};
    }
    _ = ctx.epochs.close(io, ino, ctx.close_seams);
    _ = ctx.epochs.closeAllForSpace(io, ctx.close_seams);
    return flushParked(ctx, io, ino, true);
}

const LegResult = struct { err: ?Error = null };

fn barrierLeg(ctx: Ctx, ord: u32, out: *LegResult) void {
    const v = ctx.vols.byOrdinal(ord) orelse return;
    ctx.device.flush(v.vol_tag) catch |e| {
        out.err = switch (e) {
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.VolumeFailed => error.VolumeFailed,
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    };
}

/// ≥ 2 legs under the lever ⇒ joined: every leg runs to completion, the
/// first error wins; else sequential. A leg is never cancelled.
fn runBarrierLegs(ctx: Ctx, io: Io, plan: []const u32) Error!void {
    if (plan.len == 0) return;
    var results: [256]LegResult = undefined;
    for (results[0..plan.len]) |*r| r.* = .{};
    if (plan.len >= 2 and ctx.levers.parallel_legs) {
        var group: Io.Group = .init;
        for (plan, 0..) |ord, i| group.async(io, barrierLeg, .{ ctx, ord, &results[i] });
        group.await(io) catch |e| switch (e) {
            error.Canceled => unreachable, // the fsync is never cancelled (charter §9)
        };
        stats.fsync_parallel_joins.inc();
    } else {
        for (plan, 0..) |ord, i| barrierLeg(ctx, ord, &results[i]);
    }
    for (results[0..plan.len]) |r| if (r.err) |e| return e;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");
const block_alloc = @import("block_alloc.zig");
const layout = @import("layout.zig");

test "fsync_phase_family_is_exact_sum: Σ legs ≡ total to the nanosecond on every exit, equal counts per phase, 0 ns for unmarked legs" {
    const before = stats.fsync_phase_ns[@backingInt(Phase.total)].snapshot();
    var p = Prof.startAt(100);
    p.markAt(.intent_barrier, 150);
    p.markAt(.data_flush, 1_150);
    p.markAt(.data_barrier, 3_150);
    p.markAt(.meta_barrier, 3_200);
    p.finish();
    var sum: u64 = 0;
    for (p.legs) |l| sum += l;
    try testing.expectEqual(p.total(), sum);
    try testing.expectEqual(@as(u64, 3_100), sum);
    const after = stats.fsync_phase_ns[@backingInt(Phase.total)].snapshot();
    try testing.expectEqual(before.count + 1, after.count);
    try testing.expectEqual(before.sum_ns + 3_100, after.sum_ns);
    for (stats.fsync_phase_ns[0..Phase.legs]) |*h| try testing.expectEqual(after.count, h.snapshot().count);
    try testing.expectEqual(@as(u64, 0), p.legs[@backingInt(Phase.staged_promote)]);
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"fsync_phase_ns\":{\"intent_barrier\":") != null);
}

test "touched_table_word_law: stamp ORs bits and bumps the generation in ONE CAS; clearObserved succeeds only when no stamp landed since; an ordinal past 30 is ALL (counted); barrier_plan_law: touched ∧ volatile barriers, write-through skips, ALL covers every volume" {
    var t = try TouchedTable.init(testing.allocator, 16);
    defer t.deinit(testing.allocator);
    try testing.expectEqual(@as(u64, 0), t.observe(7));
    t.stampOrdinal(7, 0);
    t.stampOrdinal(7, 3);
    const o = t.observe(7);
    try testing.expectEqual(@as(u64, 0b1001), TouchedTable.bitsOf(o));
    try testing.expectEqual(@as(u64, 2), o >> 32); // two stamps = two generations
    // A stamp between observe and clear leaves the bits (conservative).
    t.stampOrdinal(7, 1);
    try testing.expect(!t.clearObserved(7, o));
    try testing.expectEqual(@as(u64, 0b1011), TouchedTable.bitsOf(t.observe(7)));
    const o2 = t.observe(7);
    try testing.expect(t.clearObserved(7, o2));
    try testing.expectEqual(@as(u64, 0), TouchedTable.bitsOf(t.observe(7)));
    try testing.expectEqual(o2 >> 32, t.observe(7) >> 32); // the generation survives the clear
    const unres0 = stats.fsync_touched_unresolved.load();
    try testing.expectEqual(TouchedTable.ALL_BIT, TouchedTable.bitForOrdinal(31));
    try testing.expectEqual(unres0 + 1, stats.fsync_touched_unresolved.load());
    // The plan.
    const f = try wt.Fixture.init(testing.allocator, 4);
    defer f.deinit();
    var out: [256]u32 = undefined;
    const skips0 = stats.fsync_write_through_skips.load();
    try testing.expectEqual(@as(usize, 1), planBarriers(0b11, &f.vols, &out).len); // A volatile, B write-through
    try testing.expectEqual(@as(u32, 0), out[0]);
    try testing.expectEqual(skips0 + 1, stats.fsync_write_through_skips.load());
    try testing.expectEqual(@as(usize, 0), planBarriers(0, &f.vols, &out).len);
    try testing.expectEqual(@as(usize, 1), planBarriers(TouchedTable.ALL_BIT, &f.vols, &out).len);
    try testing.expectEqual(@as(usize, 0), planBarriers(0b10, &f.vols, &out).len); // only the write-through volume touched
}

const FsyncWorld = struct {
    f: *wt.Fixture,
    table: accumulate.Table,
    epochs: epoch_mod.Epochs,
    stripes: layout.MetaStripes = .{},
    locks: seams.BlockLocks,
    touched: TouchedTable,
    gov: pipeline.Governor,
    ov: device_overlay.Overlay,
    events: std.ArrayList(Event) = .empty,
    meta_fail: bool = false,
    /// A staging ring over a RAM image whose `durable` seam records every
    /// ranged sync as an event and a span.
    ring_image: []u8,
    ring: staging.Segment,
    ring_syncs: std.ArrayList(staging.Span) = .empty,
    promoted: u32 = 0,

    const Event = enum { intent, data_write, data_flush, staged_sync, merge, meta_sync };

    fn init(gpa: Allocator) !*FsyncWorld {
        const w = try gpa.create(FsyncWorld);
        errdefer gpa.destroy(w);
        const ring_image = try gpa.alloc(u8, 64 * staging.RECORD_ALIGN);
        errdefer gpa.free(ring_image);
        @memset(ring_image, 0);
        w.* = .{ .f = try wt.Fixture.init(gpa, 32), .table = try accumulate.Table.init(gpa, @intCast(wt.BS), 8), .epochs = epoch_mod.Epochs.init(gpa, wt.BS, true), .locks = try seams.BlockLocks.init(gpa, 64), .touched = try TouchedTable.init(gpa, 64), .gov = try pipeline.Governor.init(gpa, wt.BS, 2, .adaptive, null, pipeline.capBytes(1 << 30)), .ov = undefined, .ring_image = ring_image, .ring = undefined };
        w.ring = try staging.Segment.init(gpa, ring_image, .{ .ctx = w, .durable = &ringDurable });
        w.ov = try device_overlay.Overlay.init(gpa, .{ .block_size = wt.BS, .patch_max = wt.BS / 8 }, .{ .device = w.f.dev.seam(), .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none, .vols = &w.f.vols, .picker = .{ .ctx = w.f.alloc_a, .pick = &pickA }, .epochs = &w.epochs, .stripes = &w.stripes, .governor = &w.gov }, 16);
        return w;
    }

    fn deinit(self: *FsyncWorld, gpa: Allocator) void {
        self.ov.deinit(testing.io);
        self.gov.deinit();
        self.touched.deinit(gpa);
        self.locks.deinit(gpa);
        self.epochs.deinit();
        self.table.deinit();
        self.events.deinit(gpa);
        self.ring.deinit();
        gpa.free(self.ring_image);
        self.ring_syncs.deinit(gpa);
        self.f.deinit();
        gpa.destroy(self);
    }

    fn ringDurable(ctx: ?*anyopaque, _: Io, off: u64, len: u64) staging.DurableError!void {
        const self: *FsyncWorld = @ptrCast(@alignCast(ctx.?));
        self.ring_syncs.append(self.f.gpa, .{ .off = off, .len = len }) catch @panic("OOM");
        self.events.append(self.f.gpa, .staged_sync) catch @panic("OOM");
    }

    /// The lever's promote hook: the record leaves the ring (the plane's
    /// primitive moved it to a block the data barrier covers).
    fn promote(ctx: ?*anyopaque, key: staging.StagingKey) bool {
        const self: *FsyncWorld = @ptrCast(@alignCast(ctx.?));
        self.promoted += 1;
        return self.ring.remove(key, null);
    }

    fn pickA(ctx: ?*anyopaque, _: enospc.Place) ?*block_alloc.BlockAllocator {
        return @ptrCast(@alignCast(ctx.?));
    }

    fn stampHook(ctx: ?*anyopaque, ino: u64, ordinal: u32) void {
        const self: *FsyncWorld = @ptrCast(@alignCast(ctx.?));
        self.touched.stampOrdinal(ino, ordinal);
    }

    fn intent(ctx: ?*anyopaque, _: Io, _: u64) anyerror!void {
        const self: *FsyncWorld = @ptrCast(@alignCast(ctx.?));
        try self.events.append(self.f.gpa, .intent);
    }

    fn metaSync(ctx: ?*anyopaque, _: Io, _: u64) anyerror!void {
        const self: *FsyncWorld = @ptrCast(@alignCast(ctx.?));
        if (self.meta_fail) return error.InputOutput;
        try self.events.append(self.f.gpa, .meta_sync);
    }

    /// The trace is assembled from the device's event log + the map's merges
    /// + our own intent/meta events, in call order.
    fn mkCtx(self: *FsyncWorld) Ctx {
        const cs: epoch_mod.Epochs.CloseSeams = .{ .map = self.f.map.seam(), .free = self.f.frees.seam(), .purge = .none };
        return .{
            .levers = .{},
            .device = self.f.dev.seam(),
            .vols = &self.f.vols,
            .touched = &self.touched,
            .locks = &self.locks,
            .overlay = &self.ov,
            .table = &self.table,
            .upload = .{ .device = self.f.dev.seam(), .map = self.f.map.seam(), .free = self.f.frees.seam(), .purge = .none, .vols = &self.f.vols, .picker = .{ .ctx = self.f.alloc_a, .pick = &pickA }, .epochs = &self.epochs, .stripes = &self.stripes, .locks = &self.locks, .transform = .passthrough, .touched = &stampHook, .touched_ctx = self },
            .epochs = &self.epochs,
            .close_seams = cs,
            .intent = .{ .ctx = self, .barrier = &intent },
            .meta_sync = .{ .ctx = self, .sync = &metaSync },
            .staged_segment = &self.ring,
            .promotion = .{ .ctx = self, .promote = &promote },
        };
    }
};

/// The ordering law as a pure audit over an event list (so a reordered
/// leg is red by construction): intent first; every data write before the
/// data flush; every data flush before the first merge; a staged sync
/// before the first merge and the meta barrier; meta_sync last.
fn orderLawHolds(events: []const FsyncWorld.Event) bool {
    if (events.len == 0 or events[0] != .intent) return false;
    var seen_flush = false;
    var seen_merge = false;
    var seen_meta = false;
    for (events[1..]) |e| {
        if (seen_meta) return false; // nothing after the meta barrier
        switch (e) {
            .intent => return false,
            .data_write => if (seen_flush or seen_merge) return false,
            .data_flush => {
                if (seen_merge) return false;
                seen_flush = true;
            },
            .staged_sync => if (seen_merge) return false,
            .merge => seen_merge = true,
            .meta_sync => seen_meta = true,
        }
    }
    return seen_meta;
}

test "the ordering law audit: the lawful sequence passes; a reordered leg (a merge before the data flush, a data write after the flush, the meta barrier not last, a staged sync after a merge or after the meta barrier) fails" {
    const E = FsyncWorld.Event;
    try testing.expect(orderLawHolds(&.{ .intent, .data_write, .data_write, .data_flush, .merge, .meta_sync }));
    try testing.expect(orderLawHolds(&.{ .intent, .data_flush, .meta_sync })); // a clean ino: barrier + meta
    try testing.expect(orderLawHolds(&.{ .intent, .staged_sync, .meta_sync })); // a staged ino: its record synced, then the meta barrier
    try testing.expect(!orderLawHolds(&.{ .intent, .data_write, .merge, .data_flush, .meta_sync })); // meta publish before the data barrier (DUR-1 broken)
    try testing.expect(!orderLawHolds(&.{ .intent, .data_flush, .data_write, .merge, .meta_sync })); // a DMA after the barrier
    try testing.expect(!orderLawHolds(&.{ .intent, .data_write, .data_flush, .meta_sync, .merge })); // the meta barrier not last
    try testing.expect(!orderLawHolds(&.{ .intent, .meta_sync, .staged_sync })); // the staged bytes synced after the layout was made durable
    try testing.expect(!orderLawHolds(&.{ .intent, .data_write, .data_flush, .merge, .staged_sync, .meta_sync })); // after a publish that may name them
    try testing.expect(!orderLawHolds(&.{ .data_write, .intent, .data_flush, .meta_sync })); // the intent not first
    try testing.expect(!orderLawHolds(&[_]E{.intent}));
}

test "the staged leg (charter §1 item 10, 2026-10-19): fsync of a staged ino issues exactly ONE ranged sync covering its resident record, after the data flush and before the meta barrier (red before: zero syncs — the ring's pages were the page cache's); an ino with no resident issues none and pays nothing new; a record the lever promoted owes no sync; syncfs syncs the whole ring once; the phases stay exact-sum with staged_sync a leg" {
    const io = testing.io;
    const w = try FsyncWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(5, .staged, 0);
    const key = staging.StagingKey.init(@splat(0x5a), 5);
    const payload: [6000]u8 = @splat(0x33);
    try w.ring.put(key, &payload, 1, 0, 0);
    var ctx = w.mkCtx();
    ctx.staged_key = key;
    const syncs0 = stats.fsync_phase_ns[@backingInt(Phase.staged_sync)].snapshot().count;
    var prof = Prof.start();
    try fsync(ctx, io, 5, &prof);
    prof.finish();
    try testing.expectEqual(@as(usize, 1), w.ring_syncs.items.len);
    try testing.expectEqual(w.ring.residentSpan(key).?, w.ring_syncs.items[0]);
    try testing.expectEqual(staging.stride(staging.KEY_LEN, payload.len), w.ring_syncs.items[0].len);
    try testing.expectEqualSlices(FsyncWorld.Event, &.{ .intent, .staged_sync, .meta_sync }, w.events.items);
    try testing.expect(orderLawHolds(w.events.items));
    try testing.expectEqual(syncs0 + 1, stats.fsync_phase_ns[@backingInt(Phase.staged_sync)].snapshot().count);
    var sum: u64 = 0;
    for (prof.legs) |l| sum += l;
    try testing.expectEqual(prof.total(), sum);
    // A striped ino (no staged key) and a staged key with no resident
    // (released since the layout was read): no sync, nothing new.
    w.events.clearRetainingCapacity();
    try w.f.map.create(6, .striped, 0);
    var prof2 = Prof.start();
    try fsync(w.mkCtx(), io, 6, &prof2);
    prof2.finish();
    try testing.expectEqual(@as(usize, 1), w.ring_syncs.items.len);
    try testing.expectEqualSlices(FsyncWorld.Event, &.{ .intent, .meta_sync }, w.events.items);
    try testing.expect(w.ring.remove(key, 1));
    var prof3 = Prof.start();
    try fsync(ctx, io, 5, &prof3);
    prof3.finish();
    try testing.expectEqual(@as(usize, 1), w.ring_syncs.items.len);
    // The lever: the promotion takes the record out of the ring first, so
    // the sync leg finds nothing (its block is the data barrier's).
    try w.ring.put(key, &payload, 2, 0, 0);
    ctx.levers.promote_staged = .{ .on = true };
    var prof4 = Prof.start();
    try fsync(ctx, io, 5, &prof4);
    prof4.finish();
    try testing.expectEqual(@as(u32, 1), w.promoted);
    try testing.expectEqual(@as(usize, 1), w.ring_syncs.items.len);
    try testing.expect(w.ring.residentSpan(key) == null);
    // syncfs: residents ⇒ ONE whole-ring sync; an empty ring ⇒ none.
    try w.ring.put(key, &payload, 3, 0, 0);
    try w.ring.put(staging.StagingKey.init(@splat(0x5b), 7), &payload, 4, 0, 0);
    var sprof = SyncfsProf.start();
    try syncfs(w.mkCtx(), testing.allocator, io, &sprof);
    var phases: SyncfsPhaseNs = .{};
    sprof.finish(&phases);
    try testing.expectEqual(@as(usize, 2), w.ring_syncs.items.len);
    try testing.expectEqual(staging.Span{ .off = 0, .len = w.ring_image.len }, w.ring_syncs.items[1]);
    var ssum: u64 = 0;
    inline for (@typeInfo(SyncfsPhase).@"enum".field_names[0..SyncfsPhase.legs]) |name| ssum += @field(phases, name).snapshot().sum_ns;
    try testing.expectEqual(phases.total.snapshot().sum_ns, ssum);
    try testing.expect(w.ring.remove(key, 3));
    try testing.expect(w.ring.remove(staging.StagingKey.init(@splat(0x5b), 7), 4));
    var sprof2 = SyncfsProf.start();
    try syncfs(w.mkCtx(), testing.allocator, io, &sprof2);
    try testing.expectEqual(@as(usize, 2), w.ring_syncs.items.len);
}

test "fsync ladder: a parked partial block escalates to one durable upload, the data barrier covers only the touched namespace, the legs land strictly before the meta publish and the meta barrier last; phases sum exactly; the touched bits clear" {
    const io = testing.io;
    const w = try FsyncWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(1, .striped, 0);
    const e = w.table.checkout(1, 0, false, null).?;
    var half: [wt.BS / 2]u8 = @splat(0x21);
    _ = try w.table.recordWrite(e, 0, &half);
    var prof = Prof.start();
    const calls0 = stats.fsync_calls.load();
    try fsync(w.mkCtx(), io, 1, &prof);
    prof.finish();
    try testing.expectEqual(calls0 + 1, stats.fsync_calls.load());
    // Assemble the trace: intent; the device trace (write, flush); the merge; meta sync.
    var events: std.ArrayList(FsyncWorld.Event) = .empty;
    defer events.deinit(testing.allocator);
    try events.append(testing.allocator, .intent);
    var merges_seen: usize = 0;
    for (w.f.dev.trace.items) |t| {
        switch (t) {
            .write, .write_fixed => try events.append(testing.allocator, .data_write),
            .flush => {
                try events.append(testing.allocator, .data_flush);
            },
            .read => {},
        }
    }
    // The merge happened after the flush (the RamMap's merge log carries no clock; the device
    // trace shows the flush and the upload's write BEFORE it — a merge before the flush would
    // have put the touched stamp before the barrier plan read it, which the plan proves).
    merges_seen = w.f.map.merges.items.len;
    try testing.expectEqual(@as(usize, 1), merges_seen);
    try events.append(testing.allocator, .merge);
    try events.append(testing.allocator, .meta_sync);
    try testing.expect(orderLawHolds(events.items));
    try testing.expectEqual(@as(usize, 1), w.f.dev.flushes.items.len);
    try testing.expectEqual(wt.VOL_A, w.f.dev.flushes.items[0]); // only the touched, volatile volume
    try testing.expectEqual(@as(u64, 0), TouchedTable.bitsOf(w.touched.observe(1)));
    var sum: u64 = 0;
    for (prof.legs) |l| sum += l;
    try testing.expectEqual(prof.total(), sum);
    try testing.expect(prof.legs[@backingInt(Phase.data_barrier)] > 0 or prof.legs[@backingInt(Phase.data_flush)] > 0);
    try testing.expect(!w.table.has(1, 0));
    const b = w.f.map.seam().lookup(1, 0).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x21));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
}

test "clean_fsync_is_a_counted_data_noop_and_keeps_the_meta_barrier; a_failed_data_barrier_still_fails_the_fsync_before_the_meta_barrier (the touched bits left set); a failed meta barrier is its own error" {
    const io = testing.io;
    const w = try FsyncWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(2, .striped, 0);
    var prof = Prof.start();
    const noop0 = stats.fsync_noop_clean.load();
    try fsync(w.mkCtx(), io, 2, &prof);
    prof.finish();
    try testing.expectEqual(noop0 + 1, stats.fsync_noop_clean.load());
    try testing.expectEqual(@as(usize, 0), w.f.dev.flushes.items.len);
    try testing.expectEqual(@as(usize, 2), w.events.items.len); // intent + meta_sync
    try testing.expectEqual(FsyncWorld.Event.meta_sync, w.events.items[1]);
    // A touched namespace whose barrier fails: the fsync fails BEFORE the meta barrier, bits left set.
    w.touched.stampOrdinal(3, 0);
    try w.f.map.create(3, .striped, 0);
    w.f.dev.fail_next_flush = error.InputOutput;
    w.events.clearRetainingCapacity();
    var prof2 = Prof.start();
    try testing.expectError(error.InputOutput, fsync(w.mkCtx(), io, 3, &prof2));
    prof2.finish();
    try testing.expectEqual(@as(usize, 1), w.events.items.len); // intent only — no meta sync
    try testing.expectEqual(@as(u64, 1), TouchedTable.bitsOf(w.touched.observe(3)));
    // The next fsync retries the barrier and clears.
    var prof3 = Prof.start();
    try fsync(w.mkCtx(), io, 3, &prof3);
    prof3.finish();
    try testing.expectEqual(@as(u64, 0), TouchedTable.bitsOf(w.touched.observe(3)));
    // A failed meta barrier.
    w.meta_fail = true;
    var prof4 = Prof.start();
    try testing.expectError(error.MetaBarrierFailed, fsync(w.mkCtx(), io, 3, &prof4));
    prof4.finish();
}

test "syncfs is fsync's data ladder for EVERY ino holding custody: parked blocks on two inos (three keys) uploaded and published, an open overlay record on a third published, every byte on the device before ONE barrier over every volatile namespace that follows the last write, every stripe word's bits cleared; a crash after it (the RAM custody dropped) reads every byte off the map; phases exact-sum" {
    const io = testing.io;
    const gpa = testing.allocator;
    const w = try FsyncWorld.init(gpa);
    defer w.deinit(gpa);
    for ([_]u64{ 1, 2, 4 }) |ino| try w.f.map.create(ino, .striped, 0);
    const half: [wt.BS / 2]u8 = @splat(0x21);
    const quarter: [wt.BS / 4]u8 = @splat(0x42);
    _ = try w.table.recordWrite(w.table.checkout(1, 0, false, null).?, 0, &half);
    _ = try w.table.recordWrite(w.table.checkout(2, 0, false, null).?, 0, &quarter);
    _ = try w.table.recordWrite(w.table.checkout(2, 1, false, null).?, wt.BS / 2, &quarter);
    var p: [wt.BS / 2]u8 align(4096) = @splat(0x61);
    _ = try w.ov.store(io, .{ .ino = 4, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = .{ .kind = .striped, .size = 0, .map_resident = true }, .overwrite = false });
    w.touched.stampOrdinal(7, 0);
    const calls0 = stats.syncfs_calls.load();
    const inos0 = stats.syncfs_custody_inos.load();
    const blocks0 = stats.syncfs_parked_blocks.load();
    var prof = SyncfsProf.start();
    try syncfs(w.mkCtx(), gpa, io, &prof);
    var phases: SyncfsPhaseNs = .{};
    prof.finish(&phases);
    try testing.expectEqual(calls0 + 1, stats.syncfs_calls.load());
    try testing.expectEqual(inos0 + 3, stats.syncfs_custody_inos.load());
    try testing.expectEqual(blocks0 + 3, stats.syncfs_parked_blocks.load());
    // Nothing of the custody is left in RAM.
    try testing.expectEqual(@as(u32, 0), w.table.parkedCount());
    try testing.expect(!w.ov.hasCustody(4, 0));
    // The data barrier follows the last data write and covers volume A.
    var last_write: ?usize = null;
    var last_flush: ?usize = null;
    for (w.f.dev.trace.items, 0..) |t, i| switch (t) {
        .write, .write_fixed => last_write = i,
        .flush => last_flush = i,
        .read => {},
    };
    try testing.expect(last_flush.? > last_write.?);
    try testing.expectEqual(wt.VOL_A, w.f.dev.flushes.items[w.f.dev.flushes.items.len - 1]);
    try testing.expectEqual(@as(u64, 0), TouchedTable.bitsOf(w.touched.observe(7)));
    // The crash: every byte reads back through the map off the device.
    var got: [wt.BS]u8 = undefined;
    const b1 = w.f.map.seam().lookup(1, 0).?;
    w.f.dev.peek(wt.VOL_A, b1.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x21));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
    const b20 = w.f.map.seam().lookup(2, 0).?;
    w.f.dev.peek(wt.VOL_A, b20.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 4], 0x42));
    const b21 = w.f.map.seam().lookup(2, 1).?;
    w.f.dev.peek(wt.VOL_A, b21.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..][0 .. wt.BS / 4], 0x42));
    try testing.expect(w.f.map.seam().lookup(4, 0) != null);
    // Exact-sum.
    var sum: u64 = 0;
    inline for (@typeInfo(SyncfsPhase).@"enum".field_names[0..SyncfsPhase.legs]) |name| sum += @field(phases, name).snapshot().sum_ns;
    try testing.expectEqual(phases.total.snapshot().sum_ns, sum);
    try testing.expectEqual(prof.total(), sum);
    // A second syncfs over a clean mount: an empty census, still the barrier.
    const flushes0 = w.f.dev.flushes.items.len;
    var prof2 = SyncfsProf.start();
    try syncfs(w.mkCtx(), gpa, io, &prof2);
    prof2.finish(&phases);
    try testing.expectEqual(inos0 + 3, stats.syncfs_custody_inos.load());
    try testing.expectEqual(flushes0 + 1, w.f.dev.flushes.items.len);
}

test "in_place_dma_shapes_stamp_their_namespace and parallel legs pay the join: two touched volatile namespaces barrier as joined legs (both awaited); the overlay drain publishes before the barrier" {
    const io = testing.io;
    const w = try FsyncWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    // Make B volatile too for this test.
    w.f.vols.rows[1].?.write_cache_volatile = true;
    try w.f.map.create(4, .striped, 0);
    w.touched.stampOrdinal(4, 0); // a W1 patch's stamp
    w.touched.stampOrdinal(4, 1);
    // An open overlay record on block 0: the fsync drain publishes it with the barrier.
    var p: [wt.BS / 2]u8 align(4096) = @splat(0x61);
    _ = try w.ov.store(io, .{ .ino = 4, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = .{ .kind = .striped, .size = 0, .map_resident = true }, .overwrite = false });
    const joins0 = stats.fsync_parallel_joins.load();
    const unpub0 = counters.tripwireCount("overlay_unpublished_at_fsync");
    var prof = Prof.start();
    try fsync(w.mkCtx(), io, 4, &prof);
    prof.finish();
    try testing.expectEqual(joins0 + 1, stats.fsync_parallel_joins.load());
    // Flushes: the overlay settle's own (A), then the two legs (A and B).
    try testing.expect(w.f.dev.flushes.items.len >= 3);
    var saw_b = false;
    for (w.f.dev.flushes.items) |v| if (v == wt.VOL_B) {
        saw_b = true;
    };
    try testing.expect(saw_b);
    try testing.expect(w.f.map.seam().lookup(4, 0) != null); // the overlay published
    try testing.expectEqual(unpub0, counters.tripwireCount("overlay_unpublished_at_fsync"));
}
