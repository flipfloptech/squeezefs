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
//!
//! RESIDENCY (charter §6.2 "memory returns under pressure"): both slabs
//! are LAZILY populated mappings — the kernel holds pages only for slots a
//! fill has touched — never a `gpa.alloc`, whose `@memset(undefined)` in
//! the safe modes touches every byte at birth (rung 9 measured a 1.6 GiB
//! read-LRU slab fully resident at an idle mount). A slot stays resident
//! after its release (the next fill reuses it without faults) and is
//! counted on the pool's `idle_resident_bytes` gauge — the pool's OWN
//! retention, what its budget component gauges and its idle trim returns:
//! `madvise(DONTNEED)` on the heap slab, `fallocate(PUNCH_HOLE)` on the
//! memfd, slot by slot, each popped from the free list first (no reader
//! or DMA can hold a free slot) and pushed back after, so a concurrent
//! acquirer sees at most one slot missing at a time.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
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
    /// `read_fill_pool_idle_resident_bytes`: free slots whose pages the
    /// kernel still holds, over every slab (the trim's domain).
    pub var read_fill_pool_idle_resident_bytes: counters.Gauge = .{};
    /// `read_fill_pool_trimmed_bytes`: slot bytes returned to the kernel by
    /// the idle trim (Yellow/Red ticks and Red sheds).
    pub var read_fill_pool_trimmed_bytes: counters.ShardedCounter = .{};
    /// `read_fill_pool_refcount_underflow`: a release past zero (must stay 0).
    pub var refcount_underflow: counters.Tripwire = .init("read_fill_pool_refcount_underflow");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_fill_pool_slots\":{d},\"read_fill_pool_bytes\":{d},\"read_fill_pool_exhausted\":{d},\"read_fill_pool_memfd\":{d},\"read_fill_pool_idle_resident_bytes\":{d},\"read_fill_pool_trimmed_bytes\":{d},\"read_fill_pool_refcount_underflow\":{d}}}", .{
        stats.read_fill_pool_slots.load(),
        stats.read_fill_pool_bytes.load(),
        stats.read_fill_pool_exhausted.load(),
        stats.read_fill_pool_memfd.load(),
        stats.read_fill_pool_idle_resident_bytes.load(),
        stats.read_fill_pool_trimmed_bytes.load(),
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
/// on the smallest box (16 × 4 MiB), the weight the shared pool class. The
/// gauge is the pool's idle-resident bytes (the tiers gauge the checked-out
/// slots they hold; the in-flight headroom is the one uncounted part).
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
    /// Per slot: the kernel may hold pages for it (touched since its last
    /// trim). Set at acquire (the fill lands next), cleared by the trim.
    resident: []Atomic(bool),
    /// Per slot: the budget tick of its last release (the idle clock).
    freed_tick: []Atomic(u64),
    free: mpmc.BoundedMpmc(u32),
    /// The memfd (Linux slab) or null (anonymous mapping / no fd source).
    fd: ?linux.fd_t,
    /// The idle clock: the sampler's tick count (a test points it at its own).
    clock: *const Atomic(u64) = &budget.authority.tick_count,
    /// Slots checked out × `slot_size`.
    checked_out_bytes: counters.Gauge = .{},
    /// Free slots still resident × `slot_size`: the component's gauge.
    idle_resident_bytes: counters.Gauge = .{},

    /// The portable slab: ONE anonymous page-aligned mapping, lazily
    /// populated (every host).
    pub fn initRam(gpa: Allocator, slot_size: usize, capacity: u32) InitError!*Pool {
        assert(capacity > 0 and derive.isAligned(slot_size, SLOT_ALIGN));
        const len = slot_size * capacity;
        const mem = std.posix.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0) catch return error.MapFailed;
        errdefer std.posix.munmap(mem);
        return try build(gpa, slot_size, capacity, @alignCast(mem.ptr), len, null);
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
        const resident = try gpa.alloc(Atomic(bool), capacity);
        errdefer gpa.free(resident);
        @memset(resident, .init(false));
        const freed_tick = try gpa.alloc(Atomic(u64), capacity);
        errdefer gpa.free(freed_tick);
        @memset(freed_tick, .init(0));
        var free = try mpmc.BoundedMpmc(u32).init(gpa, capacity);
        errdefer free.deinit(gpa);
        var i: u32 = 0;
        while (i < capacity) : (i += 1) assert(free.tryPush(i));
        self.* = .{ .gpa = gpa, .slot_size = slot_size, .capacity = capacity, .base = base, .len = len, .refs = refs, .resident = resident, .freed_tick = freed_tick, .free = free, .fd = fd };
        stats.read_fill_pool_slots.set(capacity);
        stats.read_fill_pool_memfd.set(@intFromBool(fd != null));
        return self;
    }

    pub fn deinit(self: *Pool) void {
        stats.read_fill_pool_idle_resident_bytes.sub(self.idle_resident_bytes.load());
        if (self.fd) |fd| {
            if (builtin.os.tag == .linux) {
                _ = linux.munmap(self.base, self.len);
                _ = linux.close(fd);
            }
        } else {
            std.posix.munmap(@alignCast(self.base[0..self.len]));
        }
        self.free.deinit(self.gpa);
        self.gpa.free(self.freed_tick);
        self.gpa.free(self.resident);
        self.gpa.free(self.refs);
        self.gpa.destroy(self);
    }

    /// Test seam: the idle clock.
    pub fn setClock(self: *Pool, clock: *const Atomic(u64)) void {
        self.clock = clock;
    }

    /// Check out a free slot at refcount 1, or null (counted).
    pub fn acquire(self: *Pool) ?Slot {
        const slot = self.free.tryPop() orelse {
            stats.read_fill_pool_exhausted.inc();
            return null;
        };
        assert(self.refs[slot].load(.monotonic) == 0);
        self.refs[slot].store(1, .monotonic);
        // The fill lands next: the slot is resident from here; a free slot
        // that was already resident leaves the idle gauge.
        if (self.resident[slot].swap(true, .monotonic)) {
            self.idle_resident_bytes.sub(self.slot_size);
            stats.read_fill_pool_idle_resident_bytes.sub(self.slot_size);
        }
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
        // Stamp the idle clock BEFORE the push: a trimmer that pops the slot
        // reads a stamp no older than the release.
        self.freed_tick[slot].store(self.clock.load(.monotonic), .monotonic);
        self.idle_resident_bytes.add(self.slot_size);
        stats.read_fill_pool_idle_resident_bytes.add(self.slot_size);
        assert(self.free.tryPush(slot));
        return true;
    }

    /// The idle trim (`budget.TrimFn`): return every free slot idle for
    /// ≥ `min_idle_ticks` ticks (`now − freed_tick`).
    pub fn trim(self: *Pool, min_idle_ticks: u64) void {
        self.trimToward(min_idle_ticks, 0);
    }

    /// The shed face: return idle slots (oldest release first — the free
    /// list is FIFO) until `idle_resident_bytes ≤ target`. Each slot is
    /// popped, returned and pushed back before the next, so the pool is
    /// never more than one slot short for a concurrent acquirer; one lap
    /// over the capacity bounds the pass.
    pub fn trimToward(self: *Pool, min_idle_ticks: u64, target_idle_bytes: u64) void {
        const now = self.clock.load(.monotonic);
        var seen: u32 = 0;
        while (seen < self.capacity and self.idle_resident_bytes.load() > target_idle_bytes) : (seen += 1) {
            const slot = self.free.tryPop() orelse break;
            if (self.resident[slot].load(.monotonic) and now -| self.freed_tick[slot].load(.monotonic) >= min_idle_ticks) {
                self.returnPages(slot);
                self.resident[slot].store(false, .monotonic);
                self.idle_resident_bytes.sub(self.slot_size);
                stats.read_fill_pool_idle_resident_bytes.sub(self.slot_size);
                stats.read_fill_pool_trimmed_bytes.add(self.slot_size);
            }
            assert(self.free.tryPush(slot));
        }
    }

    /// Give a slot's pages back: a hole in the memfd (the pages were shmem;
    /// `KEEP_SIZE` keeps the fd offsets), `MADV_DONTNEED` on the anonymous
    /// mapping (the next touch is a zero page). A refusal leaves the slot
    /// resident and counted — never a correctness matter. Off-Linux the
    /// bookkeeping runs and the pages stay (the product is Linux-only;
    /// the portable tests check the ledger, the Linux test the residency).
    fn returnPages(self: *Pool, slot: Slot) void {
        if (builtin.os.tag != .linux) return;
        const b = self.bytes(slot);
        if (self.fd) |fd| {
            const mode = linux.FALLOC.FL_PUNCH_HOLE | linux.FALLOC.FL_KEEP_SIZE;
            _ = linux.fallocate(fd, mode, @intCast(@as(u64, slot) * self.slot_size), @intCast(self.slot_size));
            return;
        }
        _ = linux.madvise(b.ptr, b.len, linux.MADV.DONTNEED);
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

    /// Free slots the kernel still holds pages for × `slot_size`.
    pub fn idleResidentBytes(self: *const Pool) u64 {
        return self.idle_resident_bytes.load();
    }

    /// Everything the kernel may hold for this slab: checked out + idle.
    pub fn residentBytes(self: *const Pool) u64 {
        return self.checked_out_bytes.load() + self.idle_resident_bytes.load();
    }

    pub fn freeSlots(self: *const Pool) usize {
        return self.capacity - @as(usize, @intCast(self.checked_out_bytes.load() / self.slot_size));
    }

    /// `zc_fill_pool`: floor 64 MiB, weight 2 over the idle-resident gauge;
    /// the shed trims idle slots toward the target (a checked-out slot is a
    /// tier entry the tier's own shed releases), the trim hook the idle law.
    pub fn component(self: *Pool) budget.Component {
        return self.componentNamed("zc_fill_pool", COMPONENT_FLOOR, COMPONENT_WEIGHT);
    }

    /// The same shape under another registry name (the ranged bounce slab).
    pub fn componentNamed(self: *Pool, name: []const u8, floor: u64, weight: u64) budget.Component {
        return budget.Component.init(name, floor, weight, .{ .gauge = &self.idle_resident_bytes }, .{ .ctx = self, .call = &shedFn }).withTrim(.{ .ctx = self, .call = &trimFn });
    }

    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *Pool = @ptrCast(@alignCast(ctx));
        self.trimToward(budget.trim_red_min_idle_ticks, target);
    }

    fn trimFn(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *Pool = @ptrCast(@alignCast(ctx));
        self.trim(min_idle_ticks);
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

test "the idle-trim law on a slab: a slot is RESIDENT from its first acquire; a release stamps the idle clock and moves its bytes to the idle-resident gauge; trim(min) returns only slots idle ≥ min ticks (Yellow 2, Red 0), one at a time so an acquirer never sees a false exhaustion; a trimmed slot re-acquired is resident again" {
    const p = try Pool.initRam(testing.allocator, 4096, 3);
    defer p.deinit();
    var clock: std.atomic.Value(u64) = .init(10);
    p.setClock(&clock);
    try testing.expectEqual(@as(u64, 0), p.idleResidentBytes());
    const a = p.acquire().?;
    const b = p.acquire().?;
    p.bytes(a)[0] = 1;
    p.bytes(b)[0] = 2;
    try testing.expectEqual(@as(u64, 0), p.idleResidentBytes());
    try testing.expectEqual(@as(u64, 8192), p.residentBytes());
    try testing.expect(p.unpin(a)); // freed at tick 10
    try testing.expectEqual(@as(u64, 4096), p.idleResidentBytes());
    clock.store(11, .monotonic);
    try testing.expect(p.unpin(b)); // freed at tick 11
    try testing.expectEqual(@as(u64, 8192), p.idleResidentBytes());
    try testing.expectEqual(@as(u64, 8192), p.residentBytes());
    // Yellow at tick 12: only `a` (idle 2) qualifies; `b` (idle 1) stays.
    clock.store(12, .monotonic);
    const trimmed0 = stats.read_fill_pool_trimmed_bytes.load();
    p.trim(budget.trim_yellow_min_idle_ticks);
    try testing.expectEqual(@as(u64, 4096), p.idleResidentBytes());
    try testing.expectEqual(trimmed0 + 4096, stats.read_fill_pool_trimmed_bytes.load());
    try testing.expectEqual(@as(usize, 3), p.freeSlots());
    // Red: everything idle, including a slot freed in this very tick.
    const c = p.acquire().?;
    p.bytes(c)[0] = 3;
    try testing.expect(p.unpin(c));
    p.trim(budget.trim_red_min_idle_ticks);
    try testing.expectEqual(@as(u64, 0), p.idleResidentBytes());
    try testing.expectEqual(@as(u64, 0), p.residentBytes());
    try testing.expectEqual(@as(usize, 3), p.freeSlots());
    // A trimmed slot is resident again once re-acquired.
    const d = p.acquire().?;
    try testing.expectEqual(@as(u64, 4096), p.residentBytes());
    try testing.expect(p.unpin(d));
    // The shed face: trimToward(target) returns idle slots down to the target, oldest first.
    const e = p.acquire().?;
    const f = p.acquire().?;
    try testing.expect(p.unpin(e));
    try testing.expect(p.unpin(f));
    try testing.expectEqual(@as(u64, 3 * 4096), p.idleResidentBytes());
    p.trimToward(budget.trim_red_min_idle_ticks, 4096);
    try testing.expectEqual(@as(u64, 4096), p.idleResidentBytes());
}

test "a RAM slab is lazily resident: no byte of a fresh slab is touched at init (the slab is the kernel's zero page until a fill lands); Linux reads it back through mincore" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "mincore: Linux only");
    const page = std.heap.pageSize();
    const p = try Pool.initRam(testing.allocator, page, 64);
    defer p.deinit();
    var vec: [64]u8 = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall3(.mincore, @intFromPtr(p.base), p.len, @intFromPtr(&vec))));
    for (vec) |v| try testing.expectEqual(@as(u8, 0), v & 1);
    const s = p.acquire().?;
    p.bytes(s)[0] = 1;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall3(.mincore, @intFromPtr(p.base), p.len, @intFromPtr(&vec))));
    try testing.expectEqual(@as(u8, 1), vec[s] & 1);
    try testing.expect(p.unpin(s));
    p.trim(budget.trim_red_min_idle_ticks);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.syscall3(.mincore, @intFromPtr(p.base), p.len, @intFromPtr(&vec))));
    try testing.expectEqual(@as(u8, 0), vec[s] & 1);
    // The returned slot reads as zeros (the kernel's zero page), never the old fill.
    try testing.expectEqual(@as(u8, 0), p.bytes(s)[0]);
}

test "memfd slab (Linux): the same bytes reachable by VA and by fd offset" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "read_fill_pool: Linux only");
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
