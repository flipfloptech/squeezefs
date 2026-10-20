//! The write path's seams and lock level 3 (charter §6.2; §2 lock order;
//! `docs/porting/data_write_path.md` §14). PORTABLE: every arm of the write
//! path (`device_overlay`, `sole_owner_patch`, `accumulate`, `extent_overlay`,
//! `writeback`, `fsync`, `write_admit`) reaches the device, the block-map
//! authority, the read tiers and the terminal free through the vtables here,
//! so the cores run over RAM fakes on every host and the Linux binding
//! (`nvme_dev.Device`, the KV layout publish, the reclaimer) is one struct
//! of function pointers per mount.
//!
//! The binding value is the rung-4 `layout.Mapping` (Q12: a 40 B `extern
//! struct` on the wire, a plain value in RAM — never a string parsed per
//! use). `vol_tag` resolves through `Volumes` to the allocator and the dense
//! ordinal the governor's lanes and the fsync touched-word use (Q11).

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const dlm = @import("../meta/dlm.zig");
const journal = @import("../meta/journal.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const sync_mutex = @import("../sync/mutex.zig");
const lock_order = @import("../sync/lock_order.zig");
const slot = @import("../fuse/transport/slot.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const reclaim = @import("reclaim.zig");
const router = @import("router.zig");

pub const Binding = layout.Mapping;
pub const DmaProof = layout.DmaProof;
pub const DmaError = layout.DmaError;
pub const HeldSource = slot.HeldSource;

/// The LBA grain every piece, claim and seed is judged in.
pub const PAGE: u32 = layout.GRAIN;

pub const ReadError = error{ InputOutput, ShortRead, QueueFull, WorkerShutdown, VolumeFailed, OutOfMemory };

// ---------------------------------------------------------------------------
// The device.
// ---------------------------------------------------------------------------

/// The data device per volume: `write` is ONE DMA of `data` at `offset`
/// (the device pads to the grain — `nvme_dev.Device.writeBlock`);
/// `write_fixed` is `WRITE_FIXED(device fd ← slot)` on the FUSE queue ring
/// that registered the slot (the transport exposes it; null where the
/// session is not zero-copy — the caller falls to the pooled vehicle);
/// `read` is one ranged read; `flush` the DUR-2 barrier. The completion of
/// a write MINTS the `DmaProof` the publish consumes.
pub const DeviceSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        write: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64, data: []const u8) DmaError!void,
        write_fixed: ?*const fn (ctx: *anyopaque, vol_tag: u64, offset: u64, src: HeldSource) DmaError!void,
        read: *const fn (ctx: *anyopaque, vol_tag: u64, offset: u64, out: []u8) ReadError!void,
        flush: *const fn (ctx: *anyopaque, vol_tag: u64) DmaError!void,
    };

    pub fn write(self: DeviceSeam, vol_tag: u64, offset: u64, data: []const u8) DmaError!DmaProof {
        try self.vtable.write(self.ctx, vol_tag, offset, data);
        return .{ .vol_tag = vol_tag, .offset = offset, .len = layout.ceilGrain(data.len), .completed_ns = clock.monotonicNs() };
    }

    pub fn writeFixed(self: DeviceSeam, vol_tag: u64, offset: u64, src: HeldSource) DmaError!DmaProof {
        const f = self.vtable.write_fixed orelse return error.WorkerShutdown;
        try f(self.ctx, vol_tag, offset, src);
        return .{ .vol_tag = vol_tag, .offset = offset, .len = layout.ceilGrain(src.len), .completed_ns = clock.monotonicNs() };
    }

    pub fn canWriteFixed(self: DeviceSeam) bool {
        return self.vtable.write_fixed != null;
    }

    pub fn read(self: DeviceSeam, vol_tag: u64, offset: u64, out: []u8) ReadError!void {
        return self.vtable.read(self.ctx, vol_tag, offset, out);
    }

    pub fn flush(self: DeviceSeam, vol_tag: u64) DmaError!void {
        return self.vtable.flush(self.ctx, vol_tag);
    }
};

/// The PLAINTEXT whole-block image a binding names: on a transformed volume
/// the device holds a frame, so every write-side seed (a deferred buffer's
/// complement, the W2 CoW escalation, the seeded checkout) reads through
/// here, never raw. The plane binds the decode; an all-passthrough set and
/// the tests leave it unbound (raw device bytes ARE the image).
pub const ImageSeam = struct {
    ctx: ?*anyopaque,
    read: *const fn (ctx: ?*anyopaque, binding: Binding, out: []u8) ReadError!void,
};

// ---------------------------------------------------------------------------
// The volumes.
// ---------------------------------------------------------------------------

pub const VolumeInfo = struct {
    vol_tag: u64,
    /// The dense ordinal (governor lane, touched-word bit).
    ordinal: u32,
    alloc: *block_alloc.BlockAllocator,
    /// `transform.isPassthrough()`: the image IS the payload (W1 and the
    /// overlay are passthrough-only — a frame cannot be patched in place).
    passthrough: bool,
    /// The device acknowledges writes into a volatile cache: an fsync's
    /// data barrier must flush it (a write-through device skips).
    write_cache_volatile: bool,
    /// `router.VolumeState.active` (a draining volume takes no new custody);
    /// flipped by the lifecycle verbs under live traffic (`setActive` /
    /// `isActive` pair: release / acquire — the write path reads it before
    /// taking custody on the volume).
    active: std.atomic.Value(bool) = .init(true),

    pub fn isActive(self: *const VolumeInfo) bool {
        return self.active.load(.acquire);
    }
};

/// The mount's data volumes, indexed by ordinal (≤ `router.MAX_VOLUMES`).
/// Append-only and read lock-free by every hot path: `add` writes the row
/// THEN publishes the count with a release store, so a reader whose
/// acquire load (`n`) sees the count sees the row (the online `volume
/// add-data` appends under live traffic; the pair is `add` / `n`).
pub const Volumes = struct {
    rows: [router.MAX_VOLUMES]?VolumeInfo = @splat(null),
    count: std.atomic.Value(u32) = .init(0),

    pub fn add(self: *Volumes, v: VolumeInfo) void {
        const cur = self.count.load(.monotonic);
        assert(v.ordinal == cur and cur < router.MAX_VOLUMES);
        self.rows[cur] = v;
        self.count.store(cur + 1, .release);
    }

    /// The published row count (acquire: pairs with `add`'s release).
    pub fn n(self: *const Volumes) u32 {
        return self.count.load(.acquire);
    }

    /// The lifecycle flip (`Active` ⇔ custody admissible); false = no such
    /// volume. Release: pairs with `isActive`'s acquire.
    pub fn setActive(self: *Volumes, vol_tag: u64, active: bool) bool {
        for (self.rows[0..self.n()]) |*r| if (r.*.?.vol_tag == vol_tag) {
            r.*.?.active.store(active, .release);
            return true;
        };
        return false;
    }

    pub fn byTag(self: *const Volumes, vol_tag: u64) ?*const VolumeInfo {
        for (self.rows[0..self.n()]) |*r| if (r.*.?.vol_tag == vol_tag) return &r.*.?;
        return null;
    }

    pub fn byOrdinal(self: *const Volumes, ordinal: u32) ?*const VolumeInfo {
        if (ordinal >= self.n()) return null;
        return &self.rows[ordinal].?;
    }

    /// Every placement-eligible allocator (the ENOSPC census' candidates).
    pub fn candidates(self: *const Volumes, out: *[router.MAX_VOLUMES]*block_alloc.BlockAllocator) []*block_alloc.BlockAllocator {
        var n_out: usize = 0;
        for (self.rows[0..self.n()]) |*r| {
            if (r.*.?.isActive()) {
                out[n_out] = r.*.?.alloc;
                n_out += 1;
            }
        }
        return out[0..n_out];
    }
};

// ---------------------------------------------------------------------------
// The block-map authority (per ino).
// ---------------------------------------------------------------------------

pub const FileKind = enum(u8) { inline_, staged, packed_, striped };

/// The RAM view of an ino's layout the admission reads (lock-free peeks;
/// the authority's own coherence).
pub const FileView = struct {
    kind: FileKind,
    size: u64,
    /// The block map resolves through the authority's `lookup` (the inline
    /// record or the block-map tree alike); false only for a view the
    /// authority could not read, which declines the in-place arms.
    map_resident: bool,
};

pub const MapInsert = struct {
    block: u32,
    binding: Binding,
};

pub const MergeError = error{
    /// The presented fencing generation is stale: re-present the current one.
    FencingTokenExpired,
    /// The D0 latch: publish nothing, free nothing (W5).
    WriterGuardFenced,
    /// Verified: the ino is gone (reclaimed) — discard orphans, never resurrect.
    NotFound,
    /// A device/commit transient: the caller's never-lossy ladder.
    Transient,
    OutOfMemory,
};

pub const MergeRequest = struct {
    ino: u64,
    /// At most `MergeOutcome.max_displaced` (the ONE save's buffer; every
    /// other publisher sends one), each a distinct block: a merge displaces
    /// at most one binding per insert, so `MergeOutcome` never overflows.
    inserts: []const MapInsert,
    /// The size floor the merge establishes (0 = none; SIZE NEVER LEADS DATA).
    min_size: u64,
    /// The completed DMAs behind every new binding.
    proofs: []const DmaProof,
    /// The ino's CURRENT fencing generation (the merge credential).
    fencing: u64,
    /// The ino's size epoch (`MapSeam.sizeEpoch`) read when the unit was
    /// FORMED — a write-back unit carries its `min_size` and its block from
    /// that moment. A size-setting commit landed since ⇒ the unit is
    /// STALE: the authority inserts only the blocks still below the ino's
    /// current size, raises no size, and reports the rest `clipped` (never
    /// lossy: a byte below EOF is published; only the dead reach is
    /// dropped). Null = the caller's own ordering keeps it current (an
    /// upload the truncate's settle lands or joins, an overlay drain, a
    /// rewrite epoch's save — every one of them inside a section the size
    /// setter waits for).
    size_epoch: ?u64 = null,
};

pub const MergeOutcome = struct {
    /// Bindings the merge displaced, freed by the caller AFTER its level-3.5
    /// guard drops (RES-1).
    displaced: [max_displaced]Binding = undefined,
    n_displaced: u8 = 0,
    /// Inserts of a STALE unit (`MergeRequest.size_epoch`) the authority
    /// dropped as at or past the ino's current EOF: the caller frees their
    /// fresh bindings (nothing names them). 0 for a current unit.
    clipped: u8 = 0,
    /// The registrations the merge took on `displaced` before its commit
    /// (decision 0003 §AH.1); the caller drops them AFTER its frees
    /// (`dropHolds`). Null = nothing held (a merge that displaced nothing,
    /// a fake map).
    holds: ?ReleaseHolds = null,

    /// The ONE save's width: the most blocks one merge inserts, and so the
    /// most a rewrite epoch shadows before its close (`write_epoch`) —
    /// DERIVED from the journal's whole-entry cap (charter §5.1: one
    /// transaction = one entry ≤ 128 KiB; the save is ONE tx). Half the
    /// entry is the window's (`kvmap.CROSSING_CHUNK`'s convention: the
    /// other half holds the layout record at its cap, or the kvmap head
    /// and a straddler's re-Puts), less four map frames of headroom for
    /// the neighbourhood records a window touches, divided by the widest
    /// per-block cost of either map shape: a tree-mapped insert
    /// re-canonicalizes its run into at most three kind-7 Puts (the left
    /// remainder, the point, the right remainder) and one Delete beside
    /// its `+ref`/`−ref`; an inline-map insert costs the two refs alone.
    /// The port's first buffer was 64: a streaming overwrite filled it in
    /// a quarter of the write-back cadence and every later block paid a
    /// full-record publish of its own (`docs/evidence/rung-10-publish-
    /// churn.md`: 57 % of the blocks, 3,600 publishes/s at 27 GB/s).
    pub const max_displaced: usize = (journal.MAX_ENTRY_LEN / 2 - 4 * map_record_frame_bytes) / save_bytes_per_block_kvmap;

    /// The widest kind-7 record frame: a RUN2 value under the 13 B key.
    const map_record_frame_bytes: u64 = journal.recordFrameLen(key.BLOCK_MAP_KEY_LEN, record.BLOCK_MAP_RUN_STAMPED_VALUE_LEN);
    const map_delete_frame_bytes: u64 = journal.recordFrameLen(key.BLOCK_MAP_KEY_LEN, 0);
    /// `+ref` is a Put of the 4 B ref value, `−ref` its Delete.
    const save_bytes_per_block_inline: u64 = journal.recordFrameLen(key.BLOCK_REF_KEY_LEN, record.BLOCK_REF_VALUE_LEN) + journal.recordFrameLen(key.BLOCK_REF_KEY_LEN, 0);
    const save_bytes_per_block_kvmap: u64 = 3 * map_record_frame_bytes + map_delete_frame_bytes + save_bytes_per_block_inline;

    comptime {
        // The inline shape: the layout record at the format's cap plus the
        // window's refs fit one entry.
        const layout_frame_at_cap = journal.recordFrameLen(key.XATTR_KEY_LEN, record.RECORD_VALUE_CAP_CEILING + record.XATTR_RECORD_ENVELOPE_MAX);
        assert(journal.ENTRY_HDR_LEN + layout_frame_at_cap + max_displaced * save_bytes_per_block_inline <= journal.MAX_ENTRY_LEN);
        // The tree-mapped shape: the window's records and refs, the
        // neighbourhood's four frames and the head Put (a sweep cursor and
        // a generation at most) fit one entry with the other half spare.
        const kvmap_head_frame = journal.recordFrameLen(key.XATTR_KEY_LEN, 1 + layout.LAYOUT_XATTR_NAME.len + layout.LAYOUT_HEADER_LEN + 4 + 8);
        assert(journal.ENTRY_HDR_LEN + kvmap_head_frame + max_displaced * save_bytes_per_block_kvmap + 4 * map_record_frame_bytes <= journal.MAX_ENTRY_LEN / 2);
        // The displaced count and the clipped count ride u8 fields.
        assert(max_displaced <= std.math.maxInt(u8));
    }

    pub fn slice(self: *const MergeOutcome) []const Binding {
        return self.displaced[0..self.n_displaced];
    }

    /// The displaced releases are over: every binding freed.
    pub fn dropHolds(self: *MergeOutcome) void {
        const h = self.holds orelse return;
        h.drop(self.slice());
        self.holds = null;
    }
};

/// A RELEASE is a live owner of its block from before its releasing commit
/// until its free (decision 0003 §AH.1): between the two the allocator
/// still tracks a block no durable reference names and no record names the
/// departed owner, so an online census reads it as leaked unless the
/// in-flight registry holds it. `hold` before the commit, `drop` after the
/// free (or on the path that abandons the release), one pairing per binding.
pub const ReleaseHolds = struct {
    ctx: *anyopaque,
    hold_fn: *const fn (ctx: *anyopaque, binding: Binding) void,
    drop_fn: *const fn (ctx: *anyopaque, binding: Binding) void,

    pub fn hold(self: ReleaseHolds, bindings: []const Binding) void {
        for (bindings) |b| self.hold_fn(self.ctx, b);
    }

    pub fn drop(self: ReleaseHolds, bindings: []const Binding) void {
        for (bindings) |b| self.drop_fn(self.ctx, b);
    }
};

/// The block-map authority. BINDING (wave 4): `lookup`/`view` read the RAM
/// layout cache; `merge` is ONE transaction — `layout.publishLayout` with
/// the `+ref`/`−ref` records through `block_refs` in the SAME tx, committed
/// through the conveyor — run under the caller's meta stripe (3.5).
/// `current_fencing` is the ino's DLM generation (rung 6; rung 4 engages
/// with 1). `size_epoch` is the ino's size epoch as it stands
/// (`size_epoch.SizeEpochs.current`): what a unit records at its formation
/// and a write's landing stamps into its outcome, so the raise that follows
/// can tell a size-setting commit that crossed it.
pub const MapSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        lookup: *const fn (ctx: *anyopaque, ino: u64, block: u32) ?Binding,
        view: *const fn (ctx: *anyopaque, ino: u64) ?FileView,
        merge: *const fn (ctx: *anyopaque, req: MergeRequest) MergeError!MergeOutcome,
        current_fencing: *const fn (ctx: *anyopaque, ino: u64) u64,
        size_epoch: *const fn (ctx: *anyopaque, ino: u64) u64,
    };

    pub fn lookup(self: MapSeam, ino: u64, block: u32) ?Binding {
        return self.vtable.lookup(self.ctx, ino, block);
    }

    pub fn view(self: MapSeam, ino: u64) ?FileView {
        return self.vtable.view(self.ctx, ino);
    }

    pub fn merge(self: MapSeam, req: MergeRequest) MergeError!MergeOutcome {
        return self.vtable.merge(self.ctx, req);
    }

    pub fn currentFencing(self: MapSeam, ino: u64) u64 {
        return self.vtable.current_fencing(self.ctx, ino);
    }

    pub fn sizeEpoch(self: MapSeam, ino: u64) u64 {
        return self.vtable.size_epoch(self.ctx, ino);
    }
};

// ---------------------------------------------------------------------------
// The read tiers' purge and the terminal free.
// ---------------------------------------------------------------------------

/// ONE purge helper covers all four read tiers (charter §6.3: a forgotten
/// tier is a stale-serve class). The read path fills it; absent = nothing
/// is cached yet.
pub const PurgeSeam = struct {
    ctx: ?*anyopaque = null,
    purge_fn: ?*const fn (ctx: ?*anyopaque, binding: Binding) void = null,

    pub const none: PurgeSeam = .{};

    pub fn purge(self: PurgeSeam, binding: Binding) void {
        if (self.purge_fn) |f| f(self.ctx, binding);
    }
};

pub const FreeVerdict = enum { terminal, nonterminal, refused };

/// The terminal free of a displaced binding (`reclaim.Reclaimer.freeBlock`:
/// begin_free → tier purge → finish_free/queue), never under (3.5).
pub const FreeSeam = struct {
    ctx: *anyopaque,
    free_fn: *const fn (ctx: *anyopaque, io: Io, binding: Binding) FreeVerdict,

    pub fn free(self: FreeSeam, io: Io, binding: Binding) FreeVerdict {
        return self.free_fn(self.ctx, io, binding);
    }
};

/// The product binding: the mount owns one and hands out `seam()`.
pub const ReclaimFree = struct {
    r: *reclaim.Reclaimer,
    vols: *const Volumes,

    pub fn seam(self: *ReclaimFree) FreeSeam {
        return .{ .ctx = self, .free_fn = &free };
    }

    /// The in-flight registrations of releases on these volumes (decision
    /// 0003 §AH.1); a binding on no mounted volume holds nothing.
    pub fn holds(self: *ReclaimFree) ReleaseHolds {
        return .{ .ctx = self, .hold_fn = &holdOne, .drop_fn = &dropOne };
    }

    fn holdOne(ctx: *anyopaque, b: Binding) void {
        const self: *ReclaimFree = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(b.vol_tag) orelse return;
        v.alloc.holdInflight(v.alloc.idxOf(b.offset));
    }

    fn dropOne(ctx: *anyopaque, b: Binding) void {
        const self: *ReclaimFree = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(b.vol_tag) orelse return;
        v.alloc.dropInflight(v.alloc.idxOf(b.offset));
    }

    fn free(ctx: *anyopaque, io: Io, b: Binding) FreeVerdict {
        const self: *ReclaimFree = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(b.vol_tag) orelse return .refused;
        return switch (self.r.freeBlock(io, @intCast(v.ordinal), v.alloc.idxOf(b.offset), b.incarnation)) {
            .terminal => .terminal,
            .nonterminal => .nonterminal,
            .refused => .refused,
        };
    }
};

// ---------------------------------------------------------------------------
// The fence's class.
// ---------------------------------------------------------------------------

/// The volume's FAILED/fenced probe (the plane's `fencePoll`: the meta
/// set's `is_failed` or a fenced device) — the reclaimer's type, so every
/// fence arm of the write path reads the ONE verdict the reclaimer halts on.
/// Sticky by construction: FAILED never clears before the remount.
pub const FenceProbe = reclaim.FenceProbe;

pub const FenceClass = enum {
    /// The fail-stop working: counted, one info line, never a tripwire.
    failed_volume,
    /// A fence with no latch behind it: the must-stay-0.
    healthy,
};

/// A fence taken by a W5 arm (publish nothing, free nothing), classified
/// (owner ruling 2026-10-19): while the volume is FAILED or a device is
/// fenced the fence is the fail-stop's ordinary consequence — `failed`
/// counts it and the FAILED latch's own log already said why; on a HEALTHY
/// volume it is a fence nothing explains, and `must_stay_0` fires with
/// `detail`. A probe-less venue (the RAM fixtures) is healthy.
pub fn classifyFence(probe: ?FenceProbe, failed: *counters.ShardedCounter, must_stay_0: *counters.Tripwire, detail: []const u8) FenceClass {
    if (probe) |p| if (p.poll(p.ctx)) {
        failed.inc();
        return .failed_volume;
    };
    must_stay_0.fire(detail);
    return .healthy;
}

// ---------------------------------------------------------------------------
// Lock level 3: the per-(inode, block) flush lock.
// ---------------------------------------------------------------------------

/// Where a level-3 acquire came from (`block_lock_wait` labels).
pub const LockSite = enum(u8) {
    write_checkout,
    pipeline_upload,
    writeback_flush,
    fold,
    overlay_prune,
    settle,
    fsync,
    /// The read seam's short shared section around the active buffer.
    read,

    pub const count = @typeInfo(LockSite).@"enum".field_names.len;
};

pub const stats = struct {
    /// `block_lock_wait[site]`: a contended acquire's park, always-on.
    pub var block_lock_wait: [LockSite.count]counters.ShardedLatencyHistogram = @splat(.{});
    pub var block_lock_acquires: counters.ShardedCounter = .{};
    pub var block_lock_contended: counters.ShardedCounter = .{};
};

/// 4096-way like every other striped level: a collision is one short hold
/// in 4096 at full handler parallelism; the stripe is a parking mutex
/// (the settle awaits in-flight CQEs under it, the merge copy runs under
/// it), never a spin.
pub const BLOCK_LOCK_STRIPES: usize = 4096;

pub const BlockLocks = struct {
    locks: []sync_mutex.Mutex,
    mask: u64,

    pub const Guard = struct {
        lock: *sync_mutex.Mutex,

        pub fn release(self: Guard, io: Io) void {
            self.lock.unlock(io);
            lock_order.released(.block);
        }
    };

    pub fn init(gpa: Allocator, stripes: usize) Allocator.Error!BlockLocks {
        assert(std.math.isPowerOfTwo(stripes));
        const ls = try gpa.alloc(sync_mutex.Mutex, stripes);
        @memset(ls, .init);
        return .{ .locks = ls, .mask = stripes - 1 };
    }

    pub fn deinit(self: *BlockLocks, gpa: Allocator) void {
        gpa.free(self.locks);
        self.* = undefined;
    }

    pub fn stripeOf(self: *const BlockLocks, ino: u64, block: u32) usize {
        return @intCast(dlm.splitmix64(ino ^ std.math.rotl(u64, @as(u64, block) | 1, 32)) & self.mask);
    }

    /// Acquire (3) for `(ino, block)`; a contended acquire's wait is
    /// recorded against `site`.
    pub fn acquire(self: *BlockLocks, io: Io, ino: u64, block: u32, site: LockSite) Guard {
        const l = &self.locks[self.stripeOf(ino, block)];
        stats.block_lock_acquires.inc();
        lock_order.acquiring(.block);
        if (l.tryLock(io)) return .{ .lock = l };
        stats.block_lock_contended.inc();
        const t0 = clock.monotonicNs();
        l.lockUncancelable(io);
        stats.block_lock_wait[@backingInt(site)].record(clock.monotonicNs() -| t0);
        return .{ .lock = l };
    }

    pub fn tryAcquire(self: *BlockLocks, io: Io, ino: u64, block: u32) ?Guard {
        const l = &self.locks[self.stripeOf(ino, block)];
        if (!l.tryLock(io)) return null;
        lock_order.acquiredTry(.block);
        stats.block_lock_acquires.inc();
        return .{ .lock = l };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: the ONE save's width = (half the 128 KiB entry − four 60 B map frames) ÷ the tree-mapped insert's 306 B (three 60 B Puts, a 30 B Delete, a 50 B +ref, a 46 B −ref) = 213; the inline record at its 65,826 B frame plus 213 × 96 B of refs fits the entry with room; 213 ≤ u8" {
    try testing.expectEqual(@as(u64, 60), journal.recordFrameLen(key.BLOCK_MAP_KEY_LEN, record.BLOCK_MAP_RUN_STAMPED_VALUE_LEN));
    try testing.expectEqual(@as(u64, 306), 3 * 60 + 30 + 50 + 46);
    try testing.expectEqual(@as(usize, 213), MergeOutcome.max_displaced);
    try testing.expectEqual(@as(usize, (64 * 1024 - 4 * 60) / 306), MergeOutcome.max_displaced);
    const layout_frame_at_cap = journal.recordFrameLen(key.XATTR_KEY_LEN, record.RECORD_VALUE_CAP_CEILING + record.XATTR_RECORD_ENVELOPE_MAX);
    try testing.expectEqual(@as(u64, 65_826), layout_frame_at_cap);
    try testing.expect(journal.ENTRY_HDR_LEN + layout_frame_at_cap + 213 * 96 <= journal.MAX_ENTRY_LEN);
    // One more block of either shape and the kvmap window would cross its half.
    try testing.expect(journal.ENTRY_HDR_LEN + 85 + 214 * 306 + 4 * 60 > journal.MAX_ENTRY_LEN / 2);
}

test "the block lock table: a held stripe parks a second acquirer of the same block, counted against its site; distinct blocks of one ino spread" {
    const io = testing.io;
    var t = try BlockLocks.init(testing.allocator, 64);
    defer t.deinit(testing.allocator);
    const g = t.acquire(io, 7, 3, .write_checkout);
    try testing.expect(t.tryAcquire(io, 7, 3) == null);
    var spread: u32 = 0;
    var b: u32 = 0;
    while (b < 16) : (b += 1) {
        if (t.stripeOf(7, b) != t.stripeOf(7, 3)) spread += 1;
    }
    try testing.expect(spread >= 12);
    const contended0 = stats.block_lock_contended.load();
    const Waiter = struct {
        fn run(io_: Io, tbl: *BlockLocks) void {
            const g2 = tbl.acquire(io_, 7, 3, .settle);
            g2.release(io_);
        }
    };
    const th = try std.Thread.spawn(.{}, Waiter.run, .{ io, &t });
    while (t.locks[t.stripeOf(7, 3)].waiters(io) == 0) try std.Thread.yield();
    g.release(io);
    th.join();
    try testing.expectEqual(contended0 + 1, stats.block_lock_contended.load());
    try testing.expect(stats.block_lock_wait[@backingInt(LockSite.settle)].snapshot().count >= 1);
}

test "volumes: ordinal and tag lookups; candidates are the active allocators" {
    const a = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 0x11, .vol = 0, .block_size = 4096, .capacity_bytes = 8 * 4096 });
    defer a.deinit();
    const b = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 0x22, .vol = 1, .block_size = 4096, .capacity_bytes = 8 * 4096 });
    defer b.deinit();
    var v: Volumes = .{};
    v.add(.{ .vol_tag = 0x11, .ordinal = 0, .alloc = a, .passthrough = true, .write_cache_volatile = true });
    v.add(.{ .vol_tag = 0x22, .ordinal = 1, .alloc = b, .passthrough = false, .write_cache_volatile = false, .active = .init(false) });
    try testing.expectEqual(@as(u32, 1), v.byTag(0x22).?.ordinal);
    try testing.expectEqual(a, v.byOrdinal(0).?.alloc);
    try testing.expect(v.byTag(0x33) == null);
    var out: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expectEqual(@as(usize, 1), v.candidates(&out).len);
    try testing.expectEqual(a, v.candidates(&out)[0]);
}

test "the fence's class: a probe reading FAILED counts the fence and leaves the must-stay-0 alone; a healthy probe (or none) fires the must-stay-0 and counts nothing" {
    const S = struct {
        var failed: counters.ShardedCounter = .{};
        var tw: counters.Tripwire = .init("write_seams_test_fence_site");
        var latched: Atomic(bool) = .init(false);
        fn poll(_: ?*anyopaque) bool {
            return latched.load(.monotonic);
        }
    };
    const probe: FenceProbe = .{ .ctx = null, .poll = &S.poll };
    const tw0 = counters.tripwireCount("write_seams_test_fence_site");
    S.latched.store(true, .monotonic);
    try testing.expectEqual(FenceClass.failed_volume, classifyFence(probe, &S.failed, &S.tw, "never fired"));
    try testing.expectEqual(@as(u64, 1), S.failed.load());
    try testing.expectEqual(tw0, counters.tripwireCount("write_seams_test_fence_site"));
    S.latched.store(false, .monotonic);
    S.tw.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    try testing.expectEqual(FenceClass.healthy, classifyFence(probe, &S.failed, &S.tw, "a fence on a HEALTHY volume"));
    S.tw.resetLogBudgetForTest();
    try testing.expectEqual(FenceClass.healthy, classifyFence(null, &S.failed, &S.tw, "a fence with no probe"));
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), S.failed.load());
    try testing.expectEqual(tw0 + 2, counters.tripwireCount("write_seams_test_fence_site"));
}
