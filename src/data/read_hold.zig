//! The read-lane HOLD — the deep-queue cohort-stability store (charter §6.3
//! "read-lane hold"; `docs/porting/data_read_path.md` §2.3, §2.5, §11.3).
//! PORTABLE.
//!
//! NOT a cache tier: no admission, no ghost, no governor ledger, no
//! dehydration. A whole-block fill is HELD (one fill-pool reference) so the
//! sub-reads that follow it at depth — the cohort stragglers of a qd32
//! stream, the ahead lane's deposits — serve from RAM without a refetch
//! once the hot tier's clock has moved on. Memory CONVERGES BY CONSUMPTION:
//! every serve path that covers bytes of a held block (`credit`) adds to the
//! entry's `served`, and the server that observes `served` cross `len`
//! RETIRES it (`read_lane_hold_retired`); `trimTo` evicts oldest-first,
//! which IS the ahead-priority mechanism (ahead entries are the newest).
//!
//! Budget `clamp(4 × streams × depth × bs, 4 × bs, mem / 8)`: 4× the
//! aggregate pipeline (2× left half the deposits evicted unconsumed under
//! 38-stream FIFO skew); `/8` is junior to the write pipeline's `/4`.
//! Measured: qd8 +12.8 %, qd32 +14.5 %, `read_amp` 1.41 → 1.11.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const jc = @import("../meta/journal_core.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");
const hot_tier = @import("hot_tier.zig");

pub const BlockKey = rsc.BlockKey;
pub const Validate = hot_tier.Validate;

/// `/8`: junior to the write pipeline's `/4` (`read_lane.rs:105–109`).
pub const BUDGET_DIVISOR: u64 = 8;
/// 4× the aggregate pipeline (`read_lane.rs:206–226`).
pub const BUDGET_PIPELINE_MULTIPLE: u64 = 4;
/// Floor: 4 blocks — the smallest window a single stream at depth 1 needs
/// (the fill, its two sub-read stragglers and one ahead deposit).
pub const BUDGET_FLOOR_BLOCKS: u64 = 4;
/// FIFO reclaim slack below the `2 × live` term (`read_lane.rs:285–288`).
pub const FIFO_RECLAIM_SLACK: usize = 32;
/// `read_lane_hold`: floor 0 (a hold is pure cohort economy), weight 2.
pub const COMPONENT_FLOOR: u64 = 0;
pub const COMPONENT_WEIGHT: u64 = 2;

pub const stats = struct {
    pub var read_lane_holds: counters.ShardedCounter = .{};
    pub var read_lane_hold_retired: counters.ShardedCounter = .{};
    pub var read_lane_hold_bytes: counters.Gauge = .{};
    /// ≈ 0 on a healthy streaming row.
    pub var read_lane_hold_evicted_unconsumed: counters.ShardedCounter = .{};
    /// Lane-fetch (ledger-invisible) victims: the probe governor's pressure signal.
    pub var read_lane_hold_ahead_evictions: counters.ShardedCounter = .{};
    pub var read_lane_hold_budget_bytes: counters.Gauge = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_lane_holds\":{d},\"read_lane_hold_retired\":{d},\"read_lane_hold_bytes\":{d},\"read_lane_hold_evicted_unconsumed\":{d},\"read_lane_hold_ahead_evictions\":{d},\"read_lane_hold_budget_bytes\":{d}}}", .{
        stats.read_lane_holds.load(),
        stats.read_lane_hold_retired.load(),
        stats.read_lane_hold_bytes.load(),
        stats.read_lane_hold_evicted_unconsumed.load(),
        stats.read_lane_hold_ahead_evictions.load(),
        stats.read_lane_hold_budget_bytes.load(),
    });
}

/// `clamp(4 × streams × depth × bs, 4 × bs, mem / 8)`.
pub fn holdBudgetBytes(mem_budget: u64, block_size: u64, streams: u64, depth: u64) u64 {
    return derive.clamp(BUDGET_PIPELINE_MULTIPLE * streams * depth * block_size, BUDGET_FLOOR_BLOCKS * block_size, mem_budget / BUDGET_DIVISOR);
}

/// The hold's slot population for its largest budget: `mem/8 ÷ bs` + 1.
pub fn maxEntries(mem_budget: u64, block_size: u64) usize {
    return @intCast(@max(mem_budget / BUDGET_DIVISOR / block_size, BUDGET_FLOOR_BLOCKS) + 1);
}

pub const InsertOutcome = enum {
    inserted,
    /// Occupied: KEPT (same incarnation ⇒ identical bytes; the existing
    /// provenance wins).
    kept,
    /// The validation refused at the insert.
    refused,
    /// `len == 0 ∨ budget == 0 ∨ no cell`: nothing held.
    skipped,
};

pub const Served = struct {
    slot: u32,
    len: u32,
    ledger_visible: bool,
};

const Cell = struct {
    key: BlockKey = undefined,
    slot: u32 = 0,
    len: u32 = 0,
    seq: u64 = 0,
    live: bool = false,
    ledger_visible: bool = false,
    served: Atomic(u64) = .init(0),
};

pub const Hold = struct {
    gpa: Allocator,
    pool: *fill_pool.Pool,
    lock: jc.SpinLock = .{},
    index: rsc.KeyMap(u32) = .empty,
    cells: []Cell,
    free: []u32,
    free_len: usize,
    /// `(seq, cell)` oldest-first; a retired/purged cell's node is a
    /// tombstone the trim skips by `seq` mismatch.
    fifo: []FifoNode,
    fifo_head: usize = 0,
    fifo_len: usize = 0,
    next_seq: u64 = 1,
    bytes: counters.Gauge = .{},
    live: Atomic(usize) = .init(0),
    /// The budget the issue path publishes (fast up, slow down).
    budget_bytes: Atomic(u64) = .init(0),

    const FifoNode = struct { seq: u64, cell: u32 };

    pub fn init(gpa: Allocator, pool: *fill_pool.Pool, max_entries_: usize) Allocator.Error!*Hold {
        assert(max_entries_ > 0);
        const self = try gpa.create(Hold);
        errdefer gpa.destroy(self);
        const cells = try gpa.alloc(Cell, max_entries_);
        errdefer gpa.free(cells);
        @memset(cells, .{});
        const free = try gpa.alloc(u32, max_entries_);
        errdefer gpa.free(free);
        for (free, 0..) |*f, i| f.* = @intCast(max_entries_ - 1 - i);
        const fifo = try gpa.alloc(FifoNode, 2 * max_entries_ + 2 * FIFO_RECLAIM_SLACK);
        errdefer gpa.free(fifo);
        self.* = .{ .gpa = gpa, .pool = pool, .cells = cells, .free = free, .free_len = max_entries_, .fifo = fifo };
        try self.index.ensureTotalCapacity(gpa, @intCast(max_entries_));
        return self;
    }

    pub fn deinit(self: *Hold) void {
        for (self.cells) |*c| if (c.live) {
            _ = self.pool.unpin(c.slot);
        };
        self.index.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.free);
        self.gpa.free(self.fifo);
        self.gpa.destroy(self);
    }

    pub fn setBudget(self: *Hold, b: u64) void {
        self.budget_bytes.store(b, .monotonic);
        stats.read_lane_hold_budget_bytes.set(b);
    }

    pub fn budgetBytes(self: *const Hold) u64 {
        return self.budget_bytes.load(.monotonic);
    }

    pub fn currentBytes(self: *const Hold) u64 {
        return self.bytes.load();
    }

    pub fn liveCount(self: *const Hold) usize {
        return self.live.load(.monotonic);
    }

    fn fifoPush(self: *Hold, n: FifoNode) void {
        assert(self.fifo_len < self.fifo.len);
        self.fifo[(self.fifo_head + self.fifo_len) % self.fifo.len] = n;
        self.fifo_len += 1;
    }

    fn fifoPop(self: *Hold) ?FifoNode {
        if (self.fifo_len == 0) return null;
        const n = self.fifo[self.fifo_head];
        self.fifo_head = (self.fifo_head + 1) % self.fifo.len;
        self.fifo_len -= 1;
        return n;
    }

    /// The cell `n` names iff it is still that entry.
    fn resolve(self: *Hold, n: FifoNode) ?*Cell {
        const c = &self.cells[n.cell];
        if (!c.live or c.seq != n.seq) return null;
        return c;
    }

    /// Unindex + unaccount under the lock; returns the slot to release
    /// AFTER the lock drops.
    fn removeLocked(self: *Hold, idx: u32) u32 {
        const c = &self.cells[idx];
        assert(self.index.remove(c.key));
        self.bytes.sub(c.len);
        _ = self.live.fetchSub(1, .monotonic);
        c.live = false;
        self.free[self.free_len] = idx;
        self.free_len += 1;
        return c.slot;
    }

    /// `insert`: `len == 0 ∨ budget == 0 ⇒ skipped`; occupied ⇒ kept;
    /// vacant ⇒ validate under the lock, insert, push FIFO, trim, reclaim.
    /// The pool reference on `slot` is pinned once more for the hold (the
    /// caller keeps its own). `ledger_visible` = a demand primary's deposit
    /// (runs the hold-serve admission ceremony on serve); false = a lane fetch.
    pub fn insert(self: *Hold, key: BlockKey, slot: u32, len: u32, ledger_visible: bool, validate: Validate) InsertOutcome {
        const b = self.budgetBytes();
        if (len == 0 or b == 0) return .skipped;
        const out: InsertOutcome = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.index.contains(key)) break :blk .kept;
            if (self.free_len == 0) break :blk .skipped;
            if (!validate.run()) break :blk .refused;
            self.free_len -= 1;
            const idx = self.free[self.free_len];
            const c = &self.cells[idx];
            const seq = self.next_seq;
            self.next_seq += 1;
            c.* = .{ .key = key, .slot = slot, .len = len, .seq = seq, .live = true, .ledger_visible = ledger_visible, .served = .init(0) };
            self.pool.pin(slot);
            self.index.putAssumeCapacity(key, idx);
            self.fifoPush(.{ .seq = seq, .cell = idx });
            self.bytes.add(len);
            _ = self.live.fetchAdd(1, .monotonic);
            break :blk .inserted;
        };
        if (out == .inserted) {
            stats.read_lane_holds.inc();
            stats.read_lane_hold_bytes.set(self.bytes.load());
            self.trimTo(b);
            self.reclaimFifo();
        }
        return out;
    }

    /// Serve: pins the slot; `credit > 0` is the consumption of this very
    /// serve — the server that crosses `len` retires the entry (exactly
    /// one observes the crossing).
    pub fn serve(self: *Hold, key: BlockKey, credit_bytes: u64) ?Served {
        var retire_slot: ?u32 = null;
        const served: ?Served = blk: {
            self.lock.lock();
            defer self.lock.unlock();
            const idx = self.index.get(key) orelse break :blk null;
            const c = &self.cells[idx];
            self.pool.pin(c.slot);
            const s: Served = .{ .slot = c.slot, .len = c.len, .ledger_visible = c.ledger_visible };
            if (credit_bytes > 0 and self.creditLocked(c, credit_bytes)) retire_slot = self.removeLocked(idx);
            break :blk s;
        };
        if (retire_slot) |sl| {
            _ = self.pool.unpin(sl);
            stats.read_lane_hold_retired.inc();
            stats.read_lane_hold_bytes.set(self.bytes.load());
        }
        return served;
    }

    /// True iff this credit crossed `len`.
    fn creditLocked(_: *Hold, c: *Cell, n: u64) bool {
        const before = c.served.fetchAdd(n, .monotonic);
        return before < c.len and before + n >= c.len;
    }

    /// Consumption through ANOTHER path (the primary's slice, a hot/NVMe
    /// serve, the raw dest leg): the hold converges by consumption.
    pub fn credit(self: *Hold, key: BlockKey, n: u64) void {
        if (n == 0) return;
        var retire_slot: ?u32 = null;
        {
            self.lock.lock();
            defer self.lock.unlock();
            const idx = self.index.get(key) orelse return;
            if (self.creditLocked(&self.cells[idx], n)) retire_slot = self.removeLocked(idx);
        }
        if (retire_slot) |sl| {
            _ = self.pool.unpin(sl);
            stats.read_lane_hold_retired.inc();
            stats.read_lane_hold_bytes.set(self.bytes.load());
        }
    }

    pub fn unpin(self: *Hold, s: Served) void {
        _ = self.pool.unpin(s.slot);
    }

    pub fn contains(self: *Hold, key: BlockKey) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.contains(key);
    }

    /// The purge arm: one `contains` probe first (absent-key economy — W1
    /// purges keys the write path never held, 400 k+/s).
    pub fn purge(self: *Hold, key: BlockKey) bool {
        var slot: ?u32 = null;
        {
            self.lock.lock();
            defer self.lock.unlock();
            const idx = self.index.get(key) orelse return false;
            slot = self.removeLocked(idx);
            self.reclaimFifoLocked();
        }
        _ = self.pool.unpin(slot.?);
        stats.read_lane_hold_bytes.set(self.bytes.load());
        return true;
    }

    /// Oldest-first trim to `target` bytes: an unconsumed victim counts
    /// `evicted_unconsumed`; a lane-fetch victim additionally `ahead_evictions`.
    pub fn trimTo(self: *Hold, target: u64) void {
        while (self.bytes.load() > target) {
            var victim_slot: ?u32 = null;
            {
                self.lock.lock();
                defer self.lock.unlock();
                while (true) {
                    const n = self.fifoPop() orelse break;
                    const c = self.resolve(n) orelse continue;
                    const unconsumed = c.served.load(.monotonic) < c.len;
                    const ahead = !c.ledger_visible;
                    victim_slot = self.removeLocked(n.cell);
                    if (unconsumed) stats.read_lane_hold_evicted_unconsumed.inc();
                    if (ahead) stats.read_lane_hold_ahead_evictions.inc();
                    break;
                }
            }
            const sl = victim_slot orelse break;
            _ = self.pool.unpin(sl);
        }
        stats.read_lane_hold_bytes.set(self.bytes.load());
    }

    fn reclaimFifoLocked(self: *Hold) void {
        const live = self.live.load(.monotonic);
        if (self.fifo_len <= 2 * live + FIFO_RECLAIM_SLACK) return;
        // A head-only skim cannot work: a long-lived unconsumed entry sits at
        // the head while tombstones pile up behind it — drain and refill.
        var kept: usize = 0;
        const n = self.fifo_len;
        var i: usize = 0;
        while (i < n) : (i += 1) {
            const node = self.fifo[(self.fifo_head + i) % self.fifo.len];
            if (self.resolve(node) != null) {
                self.fifo[(self.fifo_head + kept) % self.fifo.len] = node;
                kept += 1;
            }
        }
        self.fifo_len = kept;
    }

    pub fn reclaimFifo(self: *Hold) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.reclaimFifoLocked();
    }

    pub fn fifoLen(self: *Hold) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.fifo_len;
    }

    fn shedFn(ctx: *anyopaque, target: u64) void {
        const self: *Hold = @ptrCast(@alignCast(ctx));
        self.trimTo(target);
    }

    pub fn component(self: *Hold) budget.Component {
        return budget.Component.init("read_lane_hold", COMPONENT_FLOOR, COMPONENT_WEIGHT, .{ .gauge = &self.bytes }, .{ .ctx = self, .call = &shedFn });
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "tie: hold budget = clamp(4 × streams × depth × bs, 4 bs, mem/8)" {
    const bs: u64 = 4 * derive.MiB;
    try testing.expectEqual(4 * bs, holdBudgetBytes(derive.GiB, bs, 0, 0));
    try testing.expectEqual(4 * 2 * 3 * bs, holdBudgetBytes(derive.GiB, bs, 2, 3));
    try testing.expectEqual(derive.GiB / 8, holdBudgetBytes(derive.GiB, bs, 38, 8));
    // The floor wins over an inverted cap on a tiny budget.
    try testing.expectEqual(4 * bs, holdBudgetBytes(bs, bs, 1, 1));
    try testing.expectEqual(@as(usize, 32 + 1), maxEntries(derive.GiB, bs));
}

test "hold store semantics: insert/kept/skipped; serve pins; credit retires exactly at the crossing; purge; oldest-first trim classifies victims; FIFO reclaim bounds tombstones" {
    const bs: usize = 4096;
    const pool = try fill_pool.Pool.initRam(testing.allocator, bs, 8);
    defer pool.deinit();
    const hold = try Hold.init(testing.allocator, pool, 4);
    defer hold.deinit();
    const k0 = BlockKey.init(1, 0);
    const s0 = pool.acquire().?;
    // Budget 0: nothing held.
    try testing.expectEqual(InsertOutcome.skipped, hold.insert(k0, s0, @intCast(bs), true, .always));
    hold.setBudget(2 * bs);
    try testing.expectEqual(InsertOutcome.skipped, hold.insert(k0, s0, 0, true, .always));
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(k0, s0, @intCast(bs), true, .always));
    try testing.expectEqual(@as(u32, 2), pool.refCount(s0));
    try testing.expectEqual(InsertOutcome.kept, hold.insert(k0, s0, @intCast(bs), false, .always));
    _ = pool.unpin(s0);
    try testing.expect(hold.contains(k0));
    try testing.expectEqual(@as(u64, bs), hold.currentBytes());
    // Serve half with credit, then the other half: the second server retires.
    const sv = hold.serve(k0, bs / 2).?;
    try testing.expect(sv.ledger_visible);
    try testing.expectEqual(@as(u32, 2), pool.refCount(sv.slot));
    hold.unpin(sv);
    try testing.expect(hold.contains(k0));
    const retired_before = stats.read_lane_hold_retired.load();
    const sv2 = hold.serve(k0, bs / 2).?;
    hold.unpin(sv2);
    try testing.expect(!hold.contains(k0));
    try testing.expectEqual(retired_before + 1, stats.read_lane_hold_retired.load());
    try testing.expectEqual(@as(u32, 0), pool.refCount(s0));
    // Credit through another path retires too.
    const s1 = pool.acquire().?;
    const k1 = BlockKey.init(1, bs);
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(k1, s1, @intCast(bs), false, .always));
    _ = pool.unpin(s1);
    hold.credit(k1, bs - 1);
    try testing.expect(hold.contains(k1));
    hold.credit(k1, 1);
    try testing.expect(!hold.contains(k1));
    hold.credit(.init(9, 9), 100); // absent: no-op
    // Oldest-first trim: k2 (ledger-visible), k3 (lane fetch) inserted; a
    // third insert over the 2-block budget evicts k2 (unconsumed) first.
    const k2 = BlockKey.init(1, 2 * bs);
    const k3 = BlockKey.init(1, 3 * bs);
    const k4 = BlockKey.init(1, 4 * bs);
    const s2 = pool.acquire().?;
    const s3 = pool.acquire().?;
    const s4 = pool.acquire().?;
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(k2, s2, @intCast(bs), true, .always));
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(k3, s3, @intCast(bs), false, .always));
    const unc_before = stats.read_lane_hold_evicted_unconsumed.load();
    const ahead_before = stats.read_lane_hold_ahead_evictions.load();
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(k4, s4, @intCast(bs), true, .always));
    try testing.expect(!hold.contains(k2));
    try testing.expect(hold.contains(k3) and hold.contains(k4));
    try testing.expectEqual(unc_before + 1, stats.read_lane_hold_evicted_unconsumed.load());
    try testing.expectEqual(ahead_before, stats.read_lane_hold_ahead_evictions.load());
    // Red: trim to 0 evicts k3 (a lane fetch ⇒ ahead eviction) then k4.
    hold.trimTo(0);
    try testing.expectEqual(ahead_before + 1, stats.read_lane_hold_ahead_evictions.load());
    try testing.expectEqual(@as(u64, 0), hold.currentBytes());
    _ = pool.unpin(s2);
    _ = pool.unpin(s3);
    _ = pool.unpin(s4);
    try testing.expectEqual(@as(usize, 8), pool.freeSlots());
    // Purge and FIFO reclaim: 200 insert/purge pairs leave ≤ 2 × live + 32 nodes.
    const anchor = pool.acquire().?;
    try testing.expectEqual(InsertOutcome.inserted, hold.insert(.init(2, 0), anchor, 1, true, .always));
    var i: u64 = 1;
    while (i <= 200) : (i += 1) {
        const kk = BlockKey.init(2, i * bs);
        const sl = pool.acquire().?;
        try testing.expectEqual(InsertOutcome.inserted, hold.insert(kk, sl, 1, true, .always));
        _ = pool.unpin(sl);
        try testing.expect(hold.purge(kk));
        try testing.expect(!hold.purge(kk));
    }
    try testing.expect(hold.fifoLen() <= 2 * 1 + FIFO_RECLAIM_SLACK + 1);
    try testing.expect(hold.contains(.init(2, 0)));
    _ = pool.unpin(anchor);
    const comp = hold.component();
    try testing.expectEqualStrings("read_lane_hold", comp.name);
    try testing.expectEqual(COMPONENT_WEIGHT, comp.weight);
    comp.shed.?.call(comp.shed.?.ctx, 0);
    try testing.expectEqual(@as(u64, 0), comp.current());
}

test "a refusing validate inserts nothing and leaves the caller's reference alone" {
    const pool = try fill_pool.Pool.initRam(testing.allocator, 4096, 2);
    defer pool.deinit();
    const hold = try Hold.init(testing.allocator, pool, 2);
    defer hold.deinit();
    hold.setBudget(8192);
    const Refuse = struct {
        fn no(_: ?*anyopaque) bool {
            return false;
        }
    };
    const s = pool.acquire().?;
    try testing.expectEqual(InsertOutcome.refused, hold.insert(.init(1, 0), s, 4096, true, .{ .call = &Refuse.no }));
    try testing.expectEqual(@as(u32, 1), pool.refCount(s));
    try testing.expect(pool.unpin(s));
}
