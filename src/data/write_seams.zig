//! The write path's seams and lock level 3 (charter §6.2; §2 lock order;
//! `docs/porting/data_write_path.md` §14). PORTABLE: every arm of the write
//! path (`device_overlay`, `patch_w1`, `accumulate`, `extent_overlay`,
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
const derive = @import("../core/derive.zig");
const dlm = @import("../meta/dlm.zig");
const sync_mutex = @import("../sync/mutex.zig");
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
    /// `router.VolumeState.active` (a draining volume takes no new custody).
    active: bool = true,
};

/// The mount's data volumes, indexed by ordinal (≤ `router.MAX_VOLUMES`).
pub const Volumes = struct {
    rows: [router.MAX_VOLUMES]?VolumeInfo = @splat(null),
    count: u32 = 0,

    pub fn add(self: *Volumes, v: VolumeInfo) void {
        assert(v.ordinal == self.count and self.count < router.MAX_VOLUMES);
        self.rows[self.count] = v;
        self.count += 1;
    }

    pub fn byTag(self: *const Volumes, vol_tag: u64) ?*const VolumeInfo {
        for (self.rows[0..self.count]) |*r| if (r.*.?.vol_tag == vol_tag) return &r.*.?;
        return null;
    }

    pub fn byOrdinal(self: *const Volumes, ordinal: u32) ?*const VolumeInfo {
        if (ordinal >= self.count) return null;
        return &self.rows[ordinal].?;
    }

    /// Every placement-eligible allocator (the ENOSPC census' candidates).
    pub fn candidates(self: *const Volumes, out: *[router.MAX_VOLUMES]*block_alloc.BlockAllocator) []*block_alloc.BlockAllocator {
        var n: usize = 0;
        for (self.rows[0..self.count]) |*r| {
            if (r.*.?.active) {
                out[n] = r.*.?.alloc;
                n += 1;
            }
        }
        return out[0..n];
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
    /// The block map is resident (an indirect/kvmap head declines the
    /// in-place arms).
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
    inserts: []const MapInsert,
    /// The size floor the merge establishes (0 = none; SIZE NEVER LEADS DATA).
    min_size: u64,
    /// The completed DMAs behind every new binding.
    proofs: []const DmaProof,
    /// The ino's CURRENT fencing generation (the merge credential).
    fencing: u64,
};

pub const MergeOutcome = struct {
    /// Bindings the merge displaced, freed by the caller AFTER its level-3.5
    /// guard drops (RES-1).
    displaced: [max_displaced]Binding = undefined,
    n_displaced: u8 = 0,

    pub const max_displaced = 64;

    pub fn slice(self: *const MergeOutcome) []const Binding {
        return self.displaced[0..self.n_displaced];
    }
};

/// The block-map authority. BINDING (wave 4): `lookup`/`view` read the RAM
/// layout cache; `merge` is ONE transaction — `layout.publishLayout` with
/// the `+ref`/`−ref` records through `block_refs` in the SAME tx, committed
/// through the conveyor — run under the caller's meta stripe (3.5).
/// `current_fencing` is the ino's DLM generation (rung 6; rung 4 engages
/// with 1).
pub const MapSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        lookup: *const fn (ctx: *anyopaque, ino: u64, block: u32) ?Binding,
        view: *const fn (ctx: *anyopaque, ino: u64) ?FileView,
        merge: *const fn (ctx: *anyopaque, req: MergeRequest) MergeError!MergeOutcome,
        current_fencing: *const fn (ctx: *anyopaque, ino: u64) u64,
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
        stats.block_lock_acquires.inc();
        return .{ .lock = l };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

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
    v.add(.{ .vol_tag = 0x22, .ordinal = 1, .alloc = b, .passthrough = false, .write_cache_volatile = false, .active = false });
    try testing.expectEqual(@as(u32, 1), v.byTag(0x22).?.ordinal);
    try testing.expectEqual(a, v.byOrdinal(0).?.alloc);
    try testing.expect(v.byTag(0x33) == null);
    var out: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expectEqual(@as(usize, 1), v.candidates(&out).len);
    try testing.expectEqual(a, v.candidates(&out)[0]);
}
