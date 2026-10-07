//! The read FILL POOL — ONE refcounted slab of block-size slots every
//! whole-block fill lands in and every RAM tier (hot tier, read-lane hold)
//! holds its entries in (`docs/porting/data_read_path.md` §8.1, §13 Q12;
//! charter §6.3 "On a zc session the whole-block fill pool is ONE MAP_SHARED
//! memfd slab so warm serves bridge READ_FIXED(fd → slot)").
//!
//! One slab, two ways in: by VIRTUAL ADDRESS (`bytes(slot)`: the device DMA
//! lands here, the transform decodes here, a serve copies from here) and by
//! FD (`fdOffset(slot)`: the transport bridges `READ_FIXED(memfd @ off →
//! slot)` with zero daemon passes) — the two-way-reachability law. On
//! Linux the slab is a `memfd_create` + `mmap(MAP_SHARED)`; the portable
//! double (tests on every host) is a page-aligned heap slab whose
//! `fdOffset` is null, exactly the control arm's copy path.
//!
//! Slots are REFCOUNTED (the "refcounted puts" of charter §6.3): a tier
//! holds one reference while the slot is its entry, a reader pins one for
//! the duration of its serve, the last release returns the slot to the free
//! list. Nothing is allocated per op: `acquire` pops a preallocated index.
//! The pool is a SEPARATE slab from the write path's buffers on purpose: the
//! A/B (`SQUEEZEFS_READ_ZC_SERVE`) moves the READ serve and nothing else.
//!
//! Exhaustion is a counted refusal (`read_fill_pool_exhausted`): the caller
//! fetches into an over-capacity backing it serves ONCE and never caches
//! (the reference's "over-capacity fresh backings keep the copy path").

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const mpmc = @import("../sync/mpmc.zig");
const nd = @import("../io/nvme_derive.zig");

/// The DMA alignment of every slot: the O_DIRECT grain (`nvme_derive`).
pub const SLOT_ALIGN: usize = nd.dma_align;

pub const stats = struct {
    /// `read_fill_pool_slots`: slots the slab holds.
    pub var read_fill_pool_slots: counters.Gauge = .{};
    /// `read_fill_pool_bytes`: bytes of slots currently checked out
    /// (tier entries + pinned fills); the budget gauge.
    pub var read_fill_pool_bytes: counters.Gauge = .{};
    /// `read_fill_pool_exhausted`: acquires refused for want of a slot.
    pub var read_fill_pool_exhausted: counters.ShardedCounter = .{};
    /// `read_fill_pool_memfd`: 1 when the slab is fd-addressable.
    pub var read_fill_pool_memfd: counters.Gauge = .{};
    /// `read_fill_pool_refcount_underflow`: a release past zero (must stay 0).
    pub var refcount_underflow: counters.Tripwire = .init("read_fill_pool_refcount_underflow");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_fill_pool_slots\":{d},\"read_fill_pool_bytes\":{d},\"read_fill_pool_exhausted\":{d},\"read_fill_pool_memfd\":{d},\"read_fill_pool_refcount_underflow\":{d}}}", .{
        stats.read_fill_pool_slots.load(),
        stats.read_fill_pool_bytes.load(),
        stats.read_fill_pool_exhausted.load(),
        stats.read_fill_pool_memfd.load(),
        stats.refcount_underflow.load(),
    });
}

/// The slab's slot population: every hot-tier block + every hold block +
/// the in-flight headroom `max(cores × 16, 64)` (the reference's
/// `block_pool_capacity_from(cores)`: 16 concurrent fills per core is the
/// deepest device queue a core drives; 64 the small-box floor). The tiers
/// hold their entries IN the slab, so a slab smaller than the tiers would
/// evict by exhaustion instead of by the clock.
pub fn poolSlots(cores: u32, hot_blocks: u64, hold_blocks: u64) u32 {
    const headroom: u64 = @max(@as(u64, cores) * 16, 64);
    return @intCast(hot_blocks + hold_blocks + headroom);
}

/// Budget component shape (`docs/porting/data_read_path.md` §2.5
/// `zc_fill_pool`: floor 64 MiB, weight 2): the floor is one 16-fill burst
/// on the smallest box (16 × 4 MiB), the weight the shared pool class.
pub const COMPONENT_FLOOR: u64 = 64 * derive.MiB;
pub const COMPONENT_WEIGHT: u64 = 2;

pub const Slot = u32;

/// The fd-source view of a slot (`READ_FIXED(fd @ off → slot)`).
pub const FdOffset = struct { fd: linux.fd_t, off: u64 };

pub const InitError = Allocator.Error || error{ MemfdFailed, MapFailed };

pub const Pool = struct {
    gpa: Allocator,
    slot_size: usize,
    capacity: u32,
    base: [*]align(SLOT_ALIGN) u8,
    len: usize,
    refs: []Atomic(u32),
    free: mpmc.BoundedMpmc(u32),
    /// The memfd (Linux slab) or null (heap double / no fd source).
    fd: ?linux.fd_t,
    /// Slots checked out × `slot_size`.
    checked_out_bytes: counters.Gauge = .{},

    /// The portable slab: a page-aligned heap allocation (every host).
    pub fn initRam(gpa: Allocator, slot_size: usize, capacity: u32) InitError!*Pool {
        assert(capacity > 0 and derive.isAligned(slot_size, SLOT_ALIGN));
        const len = slot_size * capacity;
        const mem = try gpa.alignedAlloc(u8, .fromByteUnits(SLOT_ALIGN), len);
        errdefer gpa.free(mem);
        return try build(gpa, slot_size, capacity, mem.ptr, len, null);
    }

    /// The Linux slab: `memfd_create` + `ftruncate` + `mmap(MAP_SHARED)` —
    /// the same bytes reachable by VA and by fd. A failure refuses (the
    /// caller falls back to `initRam` loudly: copy economics, never
    /// correctness).
    pub const initMemfd = if (builtin.os.tag == .linux) initMemfdLinux else @compileError("the memfd fill pool is Linux-only");

    fn initMemfdLinux(gpa: Allocator, slot_size: usize, capacity: u32) InitError!*Pool {
        assert(capacity > 0 and derive.isAligned(slot_size, SLOT_ALIGN));
        const len = slot_size * capacity;
        const rc = linux.memfd_create("sqz-read-fill-pool", linux.MFD.CLOEXEC);
        if (linux.errno(rc) != .SUCCESS) return error.MemfdFailed;
        const fd: linux.fd_t = @intCast(rc);
        errdefer _ = linux.close(fd);
        if (linux.errno(linux.ftruncate(fd, @intCast(len))) != .SUCCESS) return error.MemfdFailed;
        const m = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .SHARED }, fd, 0);
        if (linux.errno(m) != .SUCCESS) return error.MapFailed;
        const ptr: [*]align(SLOT_ALIGN) u8 = @ptrFromInt(m);
        errdefer _ = linux.munmap(ptr, len);
        return try build(gpa, slot_size, capacity, ptr, len, fd);
    }

    fn build(gpa: Allocator, slot_size: usize, capacity: u32, base: [*]align(SLOT_ALIGN) u8, len: usize, fd: ?linux.fd_t) InitError!*Pool {
        const self = try gpa.create(Pool);
        errdefer gpa.destroy(self);
        const refs = try gpa.alloc(Atomic(u32), capacity);
        errdefer gpa.free(refs);
        @memset(refs, .init(0));
        var free = try mpmc.BoundedMpmc(u32).init(gpa, capacity);
        errdefer free.deinit(gpa);
        var i: u32 = 0;
        while (i < capacity) : (i += 1) assert(free.tryPush(i));
        self.* = .{ .gpa = gpa, .slot_size = slot_size, .capacity = capacity, .base = base, .len = len, .refs = refs, .free = free, .fd = fd };
        stats.read_fill_pool_slots.set(capacity);
        stats.read_fill_pool_memfd.set(@intFromBool(fd != null));
        return self;
    }

    pub fn deinit(self: *Pool) void {
        if (self.fd) |fd| {
            if (builtin.os.tag == .linux) {
                _ = linux.munmap(self.base, self.len);
                _ = linux.close(fd);
            }
        } else {
            self.gpa.free(self.base[0..self.len]);
        }
        self.free.deinit(self.gpa);
        self.gpa.free(self.refs);
        self.gpa.destroy(self);
    }

    /// Check out a free slot at refcount 1, or null (counted).
    pub fn acquire(self: *Pool) ?Slot {
        const slot = self.free.tryPop() orelse {
            stats.read_fill_pool_exhausted.inc();
            return null;
        };
        assert(self.refs[slot].load(.monotonic) == 0);
        self.refs[slot].store(1, .monotonic);
        self.checked_out_bytes.add(self.slot_size);
        stats.read_fill_pool_bytes.add(self.slot_size);
        return slot;
    }

    /// One more reference (a tier adopting the fill, a reader serving it).
    /// Relaxed: the pinner already holds a reference through the tier's
    /// lock, so the slot cannot be freed under it.
    pub fn pin(self: *Pool, slot: Slot) void {
        const prev = self.refs[slot].fetchAdd(1, .monotonic);
        assert(prev != 0);
    }

    /// Drop a reference; true when this was the last and the slot went
    /// home. acq_rel: the last release orders every reader's loads before
    /// the free, so a re-acquirer's DMA never races a straggling read.
    pub fn unpin(self: *Pool, slot: Slot) bool {
        const prev = self.refs[slot].fetchSub(1, .acq_rel);
        if (prev == 0) {
            stats.refcount_underflow.fire("a fill-pool slot was released past zero");
            self.refs[slot].store(0, .monotonic);
            return false;
        }
        if (prev != 1) return false;
        self.checked_out_bytes.sub(self.slot_size);
        stats.read_fill_pool_bytes.sub(self.slot_size);
        assert(self.free.tryPush(slot));
        return true;
    }

    pub fn refCount(self: *const Pool, slot: Slot) u32 {
        return self.refs[slot].load(.monotonic);
    }

    /// The slot's bytes (`slot_size` long, DMA-aligned).
    pub fn bytes(self: *Pool, slot: Slot) []align(SLOT_ALIGN) u8 {
        return @alignCast(self.base[@as(usize, slot) * self.slot_size ..][0..self.slot_size]);
    }

    /// The fd-source view, null when the slab has no fd (the heap double).
    pub fn fdOffset(self: *const Pool, slot: Slot) ?FdOffset {
        const fd = self.fd orelse return null;
        return .{ .fd = fd, .off = @as(u64, slot) * self.slot_size };
    }

    /// THE one probe every fd-source site runs: is `[ptr, ptr+len)` on the
    /// slab? Returns the slot and the fd offset of `ptr` when it is.
    pub fn locate(self: *const Pool, ptr: [*]const u8, len: usize) ?struct { slot: Slot, fd_off: ?FdOffset } {
        const p = @intFromPtr(ptr);
        const b = @intFromPtr(self.base);
        if (p < b or p + len > b + self.len) return null;
        const rel = p - b;
        const slot: Slot = @intCast(rel / self.slot_size);
        if (rel % self.slot_size + len > self.slot_size) return null;
        const fd_off: ?FdOffset = if (self.fd) |fd| .{ .fd = fd, .off = rel } else null;
        return .{ .slot = slot, .fd_off = fd_off };
    }

    pub fn checkedOutBytes(self: *const Pool) u64 {
        return self.checked_out_bytes.load();
    }

    pub fn freeSlots(self: *const Pool) usize {
        return self.capacity - @as(usize, @intCast(self.checked_out_bytes.load() / self.slot_size));
    }

    /// `zc_fill_pool`: floor 64 MiB, weight 2; the shed is a no-op — slab
    /// slots are one mapping and a checked-out slot is a tier entry the
    /// tier's own shed releases.
    pub fn component(self: *Pool) budget.Component {
        return budget.Component.init("zc_fill_pool", COMPONENT_FLOOR, COMPONENT_WEIGHT, .{ .gauge = &self.checked_out_bytes }, null);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: poolSlots = hot + hold + max(cores × 16, 64)" {
    try testing.expectEqual(@as(u32, 64 + 10 + 2), poolSlots(1, 10, 2));
    try testing.expectEqual(@as(u32, 512 + 32), poolSlots(32, 32, 0));
    try testing.expectEqual(@as(u32, 64), poolSlots(4, 0, 0));
}

test "RAM slab: acquire/pin/unpin refcounts, free-list return, exhaustion counted, bytes aligned and disjoint, no fd source" {
    const p = try Pool.initRam(testing.allocator, 4096, 2);
    defer p.deinit();
    try testing.expectEqual(@as(usize, 2), p.freeSlots());
    const a = p.acquire().?;
    const b = p.acquire().?;
    try testing.expect(a != b);
    const before = stats.read_fill_pool_exhausted.load();
    try testing.expectEqual(@as(?Slot, null), p.acquire());
    try testing.expectEqual(before + 1, stats.read_fill_pool_exhausted.load());
    try testing.expectEqual(@as(u64, 8192), p.checkedOutBytes());
    try testing.expect(derive.isAligned(@intFromPtr(p.bytes(a).ptr), SLOT_ALIGN));
    try testing.expect(@intFromPtr(p.bytes(b).ptr) - @intFromPtr(p.bytes(a).ptr) == 4096 or @intFromPtr(p.bytes(a).ptr) - @intFromPtr(p.bytes(b).ptr) == 4096);
    p.bytes(a)[0] = 0xAB;
    try testing.expectEqual(@as(u8, 0xAB), p.bytes(a)[0]);
    p.pin(a);
    try testing.expectEqual(@as(u32, 2), p.refCount(a));
    try testing.expect(!p.unpin(a));
    try testing.expect(p.unpin(a));
    try testing.expectEqual(@as(u32, 0), p.refCount(a));
    try testing.expectEqual(@as(u64, 4096), p.checkedOutBytes());
    try testing.expect(p.acquire() != null);
    try testing.expectEqual(@as(?FdOffset, null), p.fdOffset(b));
    const loc = p.locate(p.bytes(b).ptr + 16, 100).?;
    try testing.expectEqual(b, loc.slot);
    try testing.expectEqual(@as(?FdOffset, null), loc.fd_off);
    try testing.expect(p.locate(p.bytes(b).ptr + 4000, 200) == null);
    var foreign: [8]u8 = undefined;
    try testing.expect(p.locate(&foreign, 8) == null);
    try testing.expect(p.unpin(b));
    const c = p.acquire().?;
    try testing.expect(p.unpin(c));
    const comp = p.component();
    try testing.expectEqualStrings("zc_fill_pool", comp.name);
    try testing.expectEqual(COMPONENT_FLOOR, comp.floor);
    try testing.expectEqual(COMPONENT_WEIGHT, comp.weight);
}

test "a release past zero fires the tripwire and never frees twice" {
    const p = try Pool.initRam(testing.allocator, 4096, 1);
    defer p.deinit();
    const a = p.acquire().?;
    try testing.expect(p.unpin(a));
    stats.refcount_underflow.resetLogBudgetForTest();
    @import("../core/log.zig").testing.expectErrorLogs(1);
    try testing.expect(!p.unpin(a));
    try @import("../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(@as(usize, 1), p.freeSlots());
}

test "memfd slab (Linux): the same bytes reachable by VA and by fd offset" {
    if (builtin.os.tag != .linux) return error.SkipZigTest;
    const p = try Pool.initMemfd(testing.allocator, 4096, 3);
    defer p.deinit();
    const s = p.acquire().?;
    const fo = p.fdOffset(s).?;
    try testing.expectEqual(@as(u64, s) * 4096, fo.off);
    @memset(p.bytes(s), 0x5A);
    var back: [4096]u8 = undefined;
    const n = linux.pread(fo.fd, &back, back.len, @intCast(fo.off));
    try testing.expectEqual(@as(usize, 4096), n);
    try testing.expectEqualSlices(u8, &@as([4096]u8, @splat(0x5A)), &back);
    try testing.expectEqual(@as(u64, 1), stats.read_fill_pool_memfd.load());
    try testing.expect(p.unpin(s));
}
