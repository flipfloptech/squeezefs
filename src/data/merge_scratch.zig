//! The block-map publish's working set, pooled (AGENTS.md: a hot path owns
//! pools and allocates nothing per op; `docs/evidence/rung-10-publish-
//! churn.md`). One slab per merge in flight carries everything a publish
//! builds: the layout record read off the node cache's borrowed view, the
//! folded map, and the tx's arena — the staged record, its `±ref` records,
//! the queue entry and its journal frames — carved from the same slab
//! through `tx.ArenaBacking`, so the slab returns when the tx reaches its
//! terminal outcome on the journal lane. Every one of those was a process-
//! allocator call per publish, and a 3 GiB file's 37 KiB record is past
//! the allocator's 32 KiB slab classes: each was an `mmap`, its first-touch
//! faults, ReleaseSafe's `undefined` memset and an `munmap` under the
//! process mmap_lock — 16 % + 13 % + 12 % of the lanes' samples at 3,600
//! publishes/s.
//!
//! The pool is the transform's shape (`transform.ScratchPool`): a Vyukov
//! free list under a semaphore — one pop and one push on the fast path; an
//! acquirer past the capacity parks (counted) and never allocates. A slab
//! that overflows (a record at the format cap beside the widest save) falls
//! through to the process allocator for the excess (`BufferFirstAllocator`):
//! slower, never a refusal.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const lazy = @import("../mem/lazy.zig");
const journal = @import("../meta/journal.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tx = @import("../meta/tx.zig");
const mpmc = @import("../sync/mpmc.zig");
const semaphore = @import("../sync/semaphore.zig");
const seams = @import("write_seams.zig");

pub const stats = struct {
    /// A merge that found every slab out and parked for one.
    pub var layout_publish_scratch_waits: counters.ShardedCounter = .{};
    /// Bytes a publish needed past its slab (the process allocator's).
    pub var layout_publish_scratch_overflow_bytes: counters.ShardedCounter = .{};
};

/// Arena bytes one staged `±ref` costs the widest save: its key and value,
/// its `Staged` twice (the list doubles as it grows, the old buffers
/// stranded in the arena), the queue entry's `Rec` and its journal frame.
const save_ref_arena_bytes: usize = key.BLOCK_REF_KEY_LEN + record.BLOCK_REF_VALUE_LEN + 2 * @sizeOf(tx.Staged) + @sizeOf(tx.Rec) + @sizeOf(journal.RecordFrame);

/// Arena bytes the widest save's tx needs beside its record: two refs per
/// inserted block (`max_displaced` of them) and the record's own key and
/// entry.
pub fn saveTxBytes() usize {
    return 2 * seams.MergeOutcome.max_displaced * save_ref_arena_bytes + key.XATTR_KEY_LEN + 2 * @sizeOf(tx.Staged) + @sizeOf(tx.Rec) + @sizeOf(journal.RecordFrame);
}

/// Bytes one slab holds: the record at the volume's cap twice on the lane's
/// side — the view's copy and the folded map (which grows by the save's
/// inserts, within the cap or it crosses to the tree) — then the tx's
/// arena node: the staged record and the widest save's tx, at the 3/2 the
/// arena sizes a node over its first request (`Slab.primeArena`); page-
/// aligned.
pub fn slabLen(node_size: usize) usize {
    const cap = record.recordValueCap(node_size);
    return @intCast(derive.alignUp(2 * cap + (cap + saveTxBytes()) * 3 / 2, slab_page));
}

/// The slab's grain and alignment: the smallest page this target maps (a
/// slab is whole pages, so the allocator hands it out without a split).
const slab_page: usize = std.heap.page_size_min;

/// Slabs a mount owns: two conveyor batches' worth (`conveyor.deriveBatch
/// Txs(cpus)` = `max(64, 2 × cpus)` each) — a merge holds its slab from
/// the fold until its tx's ack, and the pass builds one window while the
/// durability lane lands the one before it. The count follows the
/// CONCURRENCY, not the budget (a merge needs its slab whatever the RAM;
/// the budget's derivations size caches and parked custody, no pool tier):
/// 128 slabs up to 32 CPUs (the 64 floor), 256 at 64 — 44.5 MiB at 256 KiB
/// nodes — registered with the budget as the attribution-only component
/// `layout_publish_scratch` (`Pool.component`: floor = its bytes, never
/// shed — a slab in flight backs a queued tx).
pub fn poolCap(cpus: u32) usize {
    return 2 * @max(64, @as(usize, cpus) * 2);
}

pub const Slab = struct {
    pool: *Pool,
    /// Lazily materialized (`mem/lazy.zig`): a slab no merge has taken
    /// holds no page (the pool was 44.5 MiB of an idle daemon, written at
    /// birth by the allocator's `undefined` fill).
    buf: []align(slab_page) u8,
    /// A merge has taken it (its pages are the gauge's).
    touched: bool = false,
    first: std.heap.BufferFirstAllocator,

    /// The slab-first allocator: the merge's own working set and the tx's
    /// arena chunks.
    pub fn allocator(self: *Slab) Allocator {
        return self.first.allocator();
    }

    /// Hand the slab to a tx (`MetaSet.beginBacked`): released at the tx's
    /// terminal outcome instead of by the merge.
    pub fn backing(self: *Slab) tx.ArenaBacking {
        return .{ .ctx = self, .release_fn = &releaseBacking };
    }

    /// Give `arena` (a tx's, carved from this slab) ONE node of the slab's
    /// remainder up front. An arena sizes each node at 3/2 of its first
    /// request plus the previous node, so the tx's 56 KiB node for a
    /// 37 KiB record was followed by an 85 KiB and a 128 KiB one for a
    /// save's few KiB of refs — past the slab, into the heap (163 MB per
    /// 20 s row before this). A raw probe, no `undefined` fill.
    pub fn primeArena(self: *Slab, arena: *std.heap.ArenaAllocator) void {
        const remaining = self.buf.len -| self.first.fixed_buffer_allocator.end_index;
        // The node = 3/2 × (request + the arena's node header, its
        // alignment and 16): the request that lands the node on the
        // remainder.
        const n = remaining * 2 / 3 -| 128;
        if (n < slab_page) return;
        const a = arena.allocator();
        const probe = a.rawAlloc(n, .@"1", @returnAddress()) orelse return;
        a.rawFree(probe[0..n], .@"1", @returnAddress());
    }

    pub fn release(self: *Slab) void {
        self.pool.release(self);
    }

    fn releaseBacking(ctx: *anyopaque) void {
        const self: *Slab = @ptrCast(@alignCast(ctx));
        self.release();
    }
};

pub const Pool = struct {
    gpa: Allocator,
    io: Io,
    slabs: []Slab,
    free: mpmc.BoundedMpmc(*Slab),
    permits: semaphore.Semaphore,
    /// The slabs a merge has touched × `slab_len` (the budget's gauge: the
    /// pool's resident bytes); 0 after deinit (the registry is append-only).
    bytes: counters.Gauge = .{},
    /// `slabs × slab_len`: what the pool may hold (its component's floor).
    reserved_bytes: u64,

    pub fn init(gpa: Allocator, io: Io, n: usize, slab_len: usize) Allocator.Error!Pool {
        const count: usize = @max(n, 1);
        const slabs = try gpa.alloc(Slab, count);
        errdefer gpa.free(slabs);
        var built: usize = 0;
        errdefer for (slabs[0..built]) |s| lazy.free(u8, s.buf);
        for (slabs) |*s| {
            const buf: []align(slab_page) u8 = @alignCast(try lazy.alloc(u8, slab_len));
            s.* = .{ .pool = undefined, .buf = buf, .first = undefined };
            built += 1;
        }
        var free = try mpmc.BoundedMpmc(*Slab).init(gpa, count);
        errdefer free.deinit(gpa);
        for (slabs) |*s| free.pushSpin(s);
        return .{ .gpa = gpa, .io = io, .slabs = slabs, .free = free, .permits = semaphore.Semaphore.init(count), .reserved_bytes = count * slab_len };
    }

    /// Waits for every slab to come home first: a tx's deinit on the
    /// journal lane trails the ack its committer already returned on.
    pub fn deinit(self: *Pool) void {
        _ = self.permits.acquireUncancelable(self.io, self.slabs.len);
        for (self.slabs) |s| lazy.free(u8, s.buf);
        self.gpa.free(self.slabs);
        self.free.deinit(self.gpa);
        self.bytes.set(0);
        self.* = undefined;
    }

    /// The budget component (charter §6.2 R5: every pool is the budget's):
    /// `layout_publish_scratch`, the pool's bytes as both gauge and floor,
    /// weight 0 and no shed — a fixed pool whose slabs in flight back queued
    /// txs cannot be returned under pressure; attribution, like
    /// `nvme_ranged_slab`.
    pub fn component(self: *Pool) budget.Component {
        return budget.Component.init("layout_publish_scratch", self.reserved_bytes, 0, .{ .gauge = &self.bytes }, null);
    }

    pub fn capacity(self: *const Pool) usize {
        return self.slabs.len;
    }

    pub fn slabLen(self: *const Pool) usize {
        return self.slabs[0].buf.len;
    }

    /// A slab of our own until its release (ours, or the tx's): parks only
    /// past the capacity (counted), never allocates. The slab's allocator
    /// starts empty.
    pub fn acquire(self: *Pool) *Slab {
        if (self.permits.tryAcquire(self.io, 1) == null) {
            stats.layout_publish_scratch_waits.inc();
            _ = self.permits.acquireUncancelable(self.io, 1);
        }
        while (true) {
            if (self.free.tryPop()) |s| {
                if (!s.touched) {
                    s.touched = true;
                    self.bytes.add(s.buf.len);
                }
                s.pool = self;
                s.first = .init(s.buf, Overflow.allocator(self));
                return s;
            }
            std.atomic.spinLoopHint();
        }
    }

    fn release(self: *Pool, s: *Slab) void {
        self.free.pushSpin(s);
        self.permits.release(self.io, 1);
    }

    /// The fallback past the slab: the process allocator, with the bytes
    /// counted (a slab sized by `slabLen` overflows only on a record near
    /// the format cap beside a wide save).
    const Overflow = struct {
        fn allocator(pool: *Pool) Allocator {
            return .{ .ptr = pool, .vtable = &vtable };
        }

        const vtable: Allocator.VTable = .{ .alloc = alloc, .resize = resize, .remap = remap, .free = free };

        fn alloc(ctx: *anyopaque, len: usize, alignment: std.mem.Alignment, ra: usize) ?[*]u8 {
            const pool: *Pool = @ptrCast(@alignCast(ctx));
            stats.layout_publish_scratch_overflow_bytes.add(len);
            return pool.gpa.rawAlloc(len, alignment, ra);
        }

        fn resize(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ra: usize) bool {
            const pool: *Pool = @ptrCast(@alignCast(ctx));
            return pool.gpa.rawResize(memory, alignment, new_len, ra);
        }

        fn remap(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, new_len: usize, ra: usize) ?[*]u8 {
            const pool: *Pool = @ptrCast(@alignCast(ctx));
            return pool.gpa.rawRemap(memory, alignment, new_len, ra);
        }

        fn free(ctx: *anyopaque, memory: []u8, alignment: std.mem.Alignment, ra: usize) void {
            const pool: *Pool = @ptrCast(@alignCast(ctx));
            pool.gpa.rawFree(memory, alignment, ra);
        }
    };
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "ties: a slab is two records at the volume's cap (2 × 65,792 B at 256 KiB nodes) plus 3/2 of a third with the widest save's tx (2 × 213 refs, each its 33 B of key and value, two `Staged`, a `Rec` and a frame; the record's own entry), page-aligned — under 400 KiB; the pool is two conveyor batches (128 at 32 CPUs, 128 at 8, 256 at 64)" {
    try testing.expectEqual(@as(usize, 65_792), record.recordValueCap(256 * 1024));
    try testing.expectEqual(key.BLOCK_REF_KEY_LEN + record.BLOCK_REF_VALUE_LEN + 2 * @sizeOf(tx.Staged) + @sizeOf(tx.Rec) + @sizeOf(journal.RecordFrame), save_ref_arena_bytes);
    try testing.expectEqual(@as(usize, 33), key.BLOCK_REF_KEY_LEN + record.BLOCK_REF_VALUE_LEN);
    try testing.expectEqual(426 * save_ref_arena_bytes + key.XATTR_KEY_LEN + 2 * @sizeOf(tx.Staged) + @sizeOf(tx.Rec) + @sizeOf(journal.RecordFrame), saveTxBytes());
    const want = 2 * 65_792 + (65_792 + saveTxBytes()) * 3 / 2;
    try testing.expect(slabLen(256 * 1024) >= want and slabLen(256 * 1024) < want + slab_page and slabLen(256 * 1024) % slab_page == 0);
    try testing.expect(slabLen(256 * 1024) <= 400 * 1024);
    try testing.expectEqual(slabLen(256 * 1024), slabLen(1024 * 1024)); // the cap ceiling is 64 KiB either way
    try testing.expectEqual(@as(usize, 128), poolCap(32));
    try testing.expectEqual(@as(usize, 128), poolCap(8));
    try testing.expectEqual(@as(usize, 256), poolCap(64));
}

test "tie: the budget component `layout_publish_scratch` reads the slabs a merge has touched × slab_bytes (an idle pool 0 — its slabs are lazily materialized), floor pool_count × slab_bytes (128 × 364,544 B = 44.5 MiB at 32 CPUs and 256 KiB nodes), weight 0, no shed; a pool's bytes are its own gauge" {
    var p = try Pool.init(testing.allocator, testing.io, 3, 2 * slab_page);
    defer p.deinit();
    const c = p.component();
    try testing.expectEqualStrings("layout_publish_scratch", c.name);
    try testing.expectEqual(@as(u64, 0), c.source.current());
    try testing.expectEqual(@as(u64, 3 * 2 * slab_page), c.floor);
    try testing.expectEqual(@as(u64, 0), c.weight);
    try testing.expect(c.shed == null and c.trim == null and !c.kernel_reclaimable);
    const a = p.acquire();
    try testing.expectEqual(@as(u64, 2 * slab_page), c.source.current());
    a.release();
    const b = p.acquire();
    const d = p.acquire();
    const e = p.acquire();
    try testing.expectEqual(p.capacity() * p.slabLen(), c.source.current());
    for ([_]*Slab{ b, d, e }) |x| x.release();
    try testing.expectEqual(p.capacity() * p.slabLen(), c.source.current());
    try testing.expectEqual(@as(u64, 128 * 364_544), @as(u64, poolCap(32)) * slabLen(256 * 1024));
}

test "a primed arena over a slab takes ONE node of the slab's remainder: a record at the cap and the widest save's worth of small allocations all land in the slab (overflow bytes +0), and the next merge's slab starts empty again" {
    const io = testing.io;
    var p = try Pool.init(testing.allocator, io, 1, slabLen(256 * 1024));
    defer p.deinit();
    const s = p.acquire();
    const sa = s.allocator();
    // The lane's side: the record's copy and the folded map at the cap.
    _ = try sa.alloc(u8, record.recordValueCap(256 * 1024));
    _ = try sa.alloc(u8, record.recordValueCap(256 * 1024) + seams.MergeOutcome.max_displaced * 48);
    const overflow0 = stats.layout_publish_scratch_overflow_bytes.load();
    var t = tx.KvTx.initBacked(sa, @src(), s.backing());
    s.primeArena(&t.arena);
    try testing.expectEqual(overflow0, stats.layout_publish_scratch_overflow_bytes.load());
    const staged = try t.arena.allocator().alloc(u8, record.recordValueCap(256 * 1024));
    try t.stagePutArena(key.TREE_XATTRS, &key.xattrKey(1, 1, 0), staged);
    var i: usize = 0;
    while (i < 2 * seams.MergeOutcome.max_displaced) : (i += 1) try t.stagePut(key.TREE_BLOCK_REFS, &key.blockRefKey(1, i, 1, @intCast(i)), "vvvv");
    var q = try tx.buildQueuedTx(&t, 256 * 1024, null);
    _ = try q.framesAlloc();
    try testing.expectEqual(overflow0, stats.layout_publish_scratch_overflow_bytes.load());
    q.deinit();
    const again = p.acquire();
    try testing.expectEqual(@as(usize, 0), again.first.fixed_buffer_allocator.end_index);
    again.release();
}

test "the pool: capacity handouts are distinct slabs whose allocator starts empty each time; a slab's bytes are the slab's until it overflows (counted); a backing release returns it; the one past the cap parks (counted) until a release" {
    const io = testing.io;
    var p = try Pool.init(testing.allocator, io, 2, 8192);
    defer p.deinit();
    const a = p.acquire();
    const b = p.acquire();
    try testing.expect(a != b);
    try testing.expect(a.buf.ptr != b.buf.ptr);
    const overflow0 = stats.layout_publish_scratch_overflow_bytes.load();
    const x = try a.allocator().alloc(u8, 4096);
    try testing.expect(@intFromPtr(x.ptr) >= @intFromPtr(a.buf.ptr) and @intFromPtr(x.ptr) < @intFromPtr(a.buf.ptr) + a.buf.len);
    try testing.expectEqual(overflow0, stats.layout_publish_scratch_overflow_bytes.load());
    // Past the slab: the process allocator, counted, freed by the same face.
    const y = try a.allocator().alloc(u8, 8192);
    try testing.expectEqual(overflow0 + 8192, stats.layout_publish_scratch_overflow_bytes.load());
    a.allocator().free(y);
    // A tx backed by the slab returns it at its deinit.
    const waits0 = stats.layout_publish_scratch_waits.load();
    var t = tx.KvTx.initBacked(a.allocator(), @src(), a.backing());
    try t.stagePut(1, "k", "v");
    const Waiter = struct {
        fn run(pool: *Pool, got: *std.atomic.Value(bool)) void {
            const s = pool.acquire();
            got.store(true, .release);
            s.release();
        }
    };
    var got: std.atomic.Value(bool) = .init(false);
    const th = try std.Thread.spawn(.{}, Waiter.run, .{ &p, &got });
    while (p.permits.waiters(io) == 0) try std.Thread.yield();
    try testing.expect(!got.load(.acquire));
    t.deinit();
    th.join();
    try testing.expect(got.load(.acquire));
    try testing.expectEqual(waits0 + 1, stats.layout_publish_scratch_waits.load());
    b.release();
    // A reacquired slab's allocator starts empty: the same bytes again.
    const c = p.acquire();
    const z = try c.allocator().alloc(u8, 4096);
    try testing.expect(z.ptr == x.ptr or z.ptr == b.buf.ptr);
    c.release();
    try testing.expectEqual(@as(usize, 2), p.permits.available(io));
}
