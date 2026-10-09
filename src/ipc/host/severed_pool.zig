//! The severed-buffer pool (charter §7 "severed buffers recycle through a
//! pool — deleted a 1.76 M faults/s engine"; spec `docs/porting/
//! il_daemon.md` §3.1). PORTABLE.
//!
//! WHY a pool: the deleted engine allocated `max_op_bytes` per ring write
//! — a slab-sized allocation past the allocator's thread cache — so a
//! streaming write paid the arena mutex + extent recycle + MADV purge +
//! ~256 page re-faults (the kernel re-zeroing pages the sever memcpy
//! overwrote at once) PER OP: measured 1.76 M minor faults/s across 5
//! saturated service threads at 12.7 GB/s — the field's 81–87 % %system
//! signature. A recycled slab keeps its pages mapped and warm, so the
//! sever is ONE user-space memcpy and zero new copies.
//!
//! Sizing is derived: every slab is `max_op_bytes` (one class; the op's
//! `len` is set per use) and 4 KiB-aligned so a pooled slab can be an
//! O_DIRECT DMA source on the direct-drive lane's unaligned leg; the free
//! list holds `clamp(arena_cap / max_op, 1, 65536)` slabs — the STRUCTURAL
//! in-flight bound: severed bytes in flight can never exceed what the live
//! arenas the R5 cap admitted could hold, so retention is bounded by the
//! session-shm cap; 65536 rails the pointer array. `get` pops or allocates
//! (`ipc_severed_pool_{hits,misses}`); `put` recycles only a full-class
//! slab, and a full free list frees (the pre-pool behaviour, never a
//! leak). Custody crosses the handoff and the handler freely: the fiber
//! that owns a slab `put`s it at exit.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;

const derive = @import("../../core/derive.zig");
const mpmc = @import("../../sync/mpmc.zig");
const budget = @import("../../mem/budget.zig");
const stats = @import("stats.zig").stats;

/// O_DIRECT's grain: a slab must be a DMA source on the dd lane.
pub const SLAB_ALIGN: usize = 4096;
/// The free list's pointer-array rail.
pub const POOL_SLOTS_MAX: u64 = 65536;

/// `clamp(arena_cap / max_op, 1, 65536)` (module doc).
pub fn poolSlots(arena_cap_bytes: u64, max_op_bytes: u64) u64 {
    return derive.clamp(arena_cap_bytes / @max(max_op_bytes, 1), 1, POOL_SLOTS_MAX);
}

pub const Slab = []align(SLAB_ALIGN) u8;

pub const SeveredPool = struct {
    gpa: Allocator,
    slab_bytes: usize,
    free: mpmc.BoundedMpmc(Slab),
    /// Slabs allocated and not freed (pooled or in flight).
    live: Atomic(u64) = .init(0),

    pub fn init(gpa: Allocator, slab_bytes: usize, slots: u64) Allocator.Error!SeveredPool {
        std.debug.assert(slab_bytes != 0 and slots != 0);
        return .{ .gpa = gpa, .slab_bytes = slab_bytes, .free = try mpmc.BoundedMpmc(Slab).init(gpa, @intCast(slots)) };
    }

    /// Frees every pooled slab; a slab still in flight is its owner's bug
    /// (the host joins every handoff before the pool goes).
    pub fn deinit(self: *SeveredPool) void {
        while (self.free.tryPop()) |s| {
            self.gpa.free(s);
            _ = self.live.fetchSub(1, .monotonic);
            stats.ipc_severed_pool_bytes.sub(self.slab_bytes);
        }
        self.free.deinit(self.gpa);
        self.* = undefined;
    }

    /// A slab of the pool's class (contents undefined — the sever
    /// overwrites exactly `len` bytes and the payload view is `[0..len]`).
    /// Null only when the allocator refuses.
    pub fn get(self: *SeveredPool) ?Slab {
        if (self.free.tryPop()) |s| {
            stats.ipc_severed_pool_hits.inc();
            return s;
        }
        stats.ipc_severed_pool_misses.inc();
        const s = self.gpa.alignedAlloc(u8, .fromByteUnits(SLAB_ALIGN), self.slab_bytes) catch return null;
        _ = self.live.fetchAdd(1, .monotonic);
        stats.ipc_severed_pool_bytes.add(self.slab_bytes);
        return s;
    }

    /// Recycle; a full free list frees the slab instead.
    pub fn put(self: *SeveredPool, s: Slab) void {
        std.debug.assert(s.len == self.slab_bytes);
        if (self.free.tryPush(s)) return;
        self.gpa.free(s);
        _ = self.live.fetchSub(1, .monotonic);
        stats.ipc_severed_pool_bytes.sub(self.slab_bytes);
    }

    pub fn liveSlabs(self: *const SeveredPool) u64 {
        return self.live.load(.monotonic);
    }

    /// The R5 component: severed bytes are NON-sheddable (in flight toward
    /// a merge or a DMA; they drain by completion), weight 0, floor 0.
    pub fn component(_: *SeveredPool) budget.Component {
        return budget.Component.init("ipc_severed_buffers", 0, 0, .{ .gauge = &stats.ipc_severed_pool_bytes }, null);
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "pool slots = clamp(arena_cap / max_op, 1, 65536): the default arena cap over the 1 MiB op; the rails" {
    try testing.expectEqual(@as(u64, 22 * 1024), poolSlots(22 * derive.GiB, derive.MiB));
    try testing.expectEqual(@as(u64, 1), poolSlots(4096, derive.MiB));
    try testing.expectEqual(POOL_SLOTS_MAX, poolSlots(1 << 50, derive.MiB));
    try testing.expectEqual(@as(u64, 64), poolSlots(64 * derive.MiB, derive.MiB));
}

test "the pool: a miss allocates an aligned slab, a put recycles it, the next get is a hit; the bound frees past the free list; hits + misses ≡ gets; the gauge tracks live bytes" {
    var p = try SeveredPool.init(testing.allocator, 8192, 2);
    defer p.deinit();
    const hits0 = stats.ipc_severed_pool_hits.load();
    const misses0 = stats.ipc_severed_pool_misses.load();
    const bytes0 = stats.ipc_severed_pool_bytes.load();
    const a = p.get().?;
    try testing.expectEqual(@as(usize, 0), @intFromPtr(a.ptr) % SLAB_ALIGN);
    try testing.expectEqual(@as(usize, 8192), a.len);
    const b = p.get().?;
    const c = p.get().?;
    try testing.expectEqual(misses0 + 3, stats.ipc_severed_pool_misses.load());
    try testing.expectEqual(bytes0 + 3 * 8192, stats.ipc_severed_pool_bytes.load());
    try testing.expectEqual(@as(u64, 3), p.liveSlabs());
    p.put(a);
    p.put(b);
    // The free list holds 2: the third put frees (live drops).
    p.put(c);
    try testing.expectEqual(@as(u64, 2), p.liveSlabs());
    try testing.expectEqual(bytes0 + 2 * 8192, stats.ipc_severed_pool_bytes.load());
    const d = p.get().?;
    try testing.expectEqual(hits0 + 1, stats.ipc_severed_pool_hits.load());
    const e = p.get().?;
    try testing.expectEqual(hits0 + 2, stats.ipc_severed_pool_hits.load());
    const f = p.get().?;
    try testing.expectEqual(misses0 + 4, stats.ipc_severed_pool_misses.load());
    const gets: u64 = 6;
    try testing.expectEqual(gets, (stats.ipc_severed_pool_hits.load() - hits0) + (stats.ipc_severed_pool_misses.load() - misses0));
    p.put(d);
    p.put(e);
    p.put(f);
    try testing.expectEqual(@as(u64, 2), p.liveSlabs());
    const comp = p.component();
    try testing.expectEqualStrings("ipc_severed_buffers", comp.name);
    try testing.expectEqual(@as(u64, 0), comp.weight);
}
