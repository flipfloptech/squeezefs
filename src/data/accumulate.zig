//! Accumulation — arms G–L of the write admission and the write-through
//! upload (charter §6.2 item 3; `docs/porting/data_write_path.md` §1.3,
//! §5.1, §5.3). ONE merge copy from the lease into the exclusive-owner,
//! copy-on-write 4 MiB buffer: pooled, 4 KiB-aligned, MEMSET-ELIDED —
//! uncovered bytes are recycled pool memory and must never escape:
//! coverage-aware reads zero-fill replies, and every stage/upload exit
//! zero-completes the complement under the block lock. Mutation requires
//! provable uniqueness (`uniqueOrCopy`); readers get immutable snapshots; a
//! shared buffer copies first. The write-through trigger is `union == whole
//! block` (`write_coverage.zig`).
//!
//! Write-through (the pipeline upload): snapshot under the block lock +
//! capture `write_epoch` → transform → allocate (incarnation UNSTABLE) →
//! aligned DMA → publish → re-lock, revalidate (entry present ∧ epoch
//! unchanged) ⇒ ONE block-map merge under the inode meta stripe, or the
//! rewrite-epoch feed for an overwrite; superseded ⇒ free the fresh offset,
//! publish nothing — LATEST-WINS supersession. The stamp and the
//! revalidation both run under (3): lock-serialized by design, no fence
//! protocol, no model owed. `write_epoch` is PROCESS-GLOBALLY unique (one
//! Relaxed `fetch_add`): global uniqueness closes the retire → re-park ABA.
//!
//! Transformed volumes use a worst-case scratch (`block + 128 KiB`, the
//! `transform.scratchLen` envelope — deliberately not the block pool, whose
//! exactly-block buffers cannot hold expanded compressor output) through the
//! `TransformSeam`; compression is best-effort per block (store-raw inside
//! `transform.processWrite`).

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const jc = @import("../meta/journal_core.zig");
const budget = @import("../mem/budget.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const enospc = @import("enospc.zig");
const cov = @import("write_coverage.zig");
const seams = @import("write_seams.zig");
const epoch_mod = @import("write_epoch.zig");
const pipeline = @import("write_pipeline.zig");
const extent = @import("extent_overlay.zig");

pub const Binding = seams.Binding;
pub const CoverageUnion = cov.CoverageUnion;
pub const PAGE: u32 = seams.PAGE;

// ---------------------------------------------------------------------------
// Derivations (every constant with its WHY).
// ---------------------------------------------------------------------------

/// The shipped parked-buffer floor: never regress below what shipped
/// (`MAX_ACTIVE_BLOCK_BUFFERS = 256`).
pub const PARKED_BUFFERS_FLOOR: u64 = 256;

/// `max(256, budget/16 ÷ block)`: R5 already gauges and sheds parked RAM;
/// a sixteenth of the budget is the parked write set's share beside the
/// node cache (`budget/16`) and the transport arena (`budget/8`).
pub fn parkedCapBuffers(budget_bytes: u64, block_size: u64) u64 {
    return @max(PARKED_BUFFERS_FLOOR, budget_bytes / 16 / block_size);
}

pub fn resolveParkedCap(explicit: ?u64, budget_bytes: u64, block_size: u64) u64 {
    return explicit orelse parkedCapBuffers(budget_bytes, block_size);
}

pub fn parkedCapFromEnv(env: *const std.process.Environ.Map, budget_bytes: u64, block_size: u64) u64 {
    return resolveParkedCap(knobs.optIntKnob("SQUEEZEFS_PARKED_BUFFERS", u64, env), budget_bytes, block_size);
}

/// Red halves the parked cap (the charter's "parked caps halve and become
/// bounds").
pub fn effectiveParkedCap(cap: u64, level: budget.Level) u64 {
    return if (level == .red) @max(cap / 2, 1) else cap;
}

/// The transform scratch pool: `(cpus / 4).clamp(4, 16)` — above the
/// concurrent-transform count every handout rotates through a cache-cold
/// buffer (+4..10 % measured).
pub fn scratchPoolCap(cpus: u32) u64 {
    return derive.clamp(cpus / 4, 4, 16);
}

// ---------------------------------------------------------------------------
// Stats (§11.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var write_through_blocks: counters.ShardedCounter = .{};
    pub var write_through_bytes: counters.ShardedCounter = .{};
    pub var write_through_fallbacks: counters.ShardedCounter = .{};
    /// THE one userspace copy's bytes.
    pub var nt_copy_bytes: counters.ShardedCounter = .{};
    pub var active_block_cow_copies: counters.ShardedCounter = .{};
    pub var active_block_memset_elided_bytes: counters.ShardedCounter = .{};
    pub var active_block_ooo_runs: counters.ShardedCounter = .{};
    pub var active_block_run_table_completions: counters.ShardedCounter = .{};
    pub var overwrite_seed_deferred: counters.ShardedCounter = .{};
    pub var overwrite_seed_materialized: counters.ShardedCounter = .{};
    pub var overwrite_seed_skipped: counters.ShardedCounter = .{};
    pub var write_block_revisits: counters.ShardedCounter = .{};
    pub var parked_full_buffer_bytes: counters.Gauge = .{};
    pub var parked_buffers: counters.Gauge = .{};
    pub var parked_pool_refusals: counters.ShardedCounter = .{};
    pub var durable_upload_bytes_escalation: counters.ShardedCounter = .{};
    // Tripwires.
    pub var coverage_complete_buffer_not_content_valid: counters.Tripwire = .init("coverage_complete_buffer_not_content_valid");
    pub var complete_union_with_extra_runs: counters.Tripwire = .init("complete_union_with_extra_runs");
    pub var zero_complete_on_deferred_seed: counters.Tripwire = .init("zero_complete_on_deferred_seed");
    pub var active_block_table_full: counters.Tripwire = .init("active_block_table_full");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "write_through_blocks", "write_through_bytes", "write_through_fallbacks", "nt_copy_bytes", "active_block_cow_copies", "active_block_memset_elided_bytes", "active_block_ooo_runs", "active_block_run_table_completions", "overwrite_seed_deferred", "overwrite_seed_materialized", "overwrite_seed_skipped", "write_block_revisits", "parked_pool_refusals", "durable_upload_bytes_escalation" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"parked_full_buffer_bytes\":{d},\"parked_buffers\":{d},", .{ stats.parked_full_buffer_bytes.load(), stats.parked_buffers.load() });
    inline for (.{ "coverage_complete_buffer_not_content_valid", "complete_union_with_extra_runs", "zero_complete_on_deferred_seed", "active_block_table_full" }, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, counters.tripwireCount(name) });
    }
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// §5.1 The aligned block pool and the CoW cell.
// ---------------------------------------------------------------------------

/// 4 KiB-aligned (`POOLED_BUF_ALIGN`: O_DIRECT / `WriteData::Aligned`),
/// block-sized buffers, allocated lazily up to `cap` and recycled through a
/// free list. Pool memory is zeroed at OS-allocation birth and only ever
/// written through covered slices, so an uncovered buffer holds STALE BUT
/// INITIALIZED bytes — sound to view, private by coverage.
pub const Pool = struct {
    gpa: Allocator,
    size: usize,
    cap: u64,
    lock: jc.SpinLock = .{},
    free: std.ArrayList(*Cell) = .empty,
    live: Atomic(u64) = .init(0),

    pub const Cell = struct {
        data: []align(PAGE) u8,
        /// Readers' shares + the owner's; 1 = unique (mutable in place).
        refs: Atomic(u32) = .init(1),

        /// `share`: an immutable reference (Relaxed: the owner's lock orders).
        pub fn share(self: *Cell) *Cell {
            _ = self.refs.fetchAdd(1, .monotonic);
            return self;
        }
    };

    pub fn init(gpa: Allocator, size: usize, cap: u64) Pool {
        assert(size % PAGE == 0 and size != 0);
        return .{ .gpa = gpa, .size = size, .cap = cap };
    }

    pub fn deinit(self: *Pool) void {
        for (self.free.items) |c| {
            self.gpa.free(c.data);
            self.gpa.destroy(c);
        }
        self.free.deinit(self.gpa);
        self.* = undefined;
    }

    /// A unique cell (refs 1), contents UNDEFINED (memset elided). Null at
    /// the cap (the caller's never-lossy exit: stage the piece).
    pub fn acquire(self: *Pool) ?*Cell {
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.free.pop()) |c| {
                c.refs.store(1, .monotonic);
                return c;
            }
            if (self.live.load(.monotonic) >= self.cap) {
                stats.parked_pool_refusals.inc();
                return null;
            }
            _ = self.live.fetchAdd(1, .monotonic);
        }
        const c = self.gpa.create(Cell) catch {
            _ = self.live.fetchSub(1, .monotonic);
            return null;
        };
        const data = self.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), self.size) catch {
            self.gpa.destroy(c);
            _ = self.live.fetchSub(1, .monotonic);
            return null;
        };
        c.* = .{ .data = data };
        return c;
    }

    /// Drop one reference; the last one returns the cell to the pool.
    /// acq_rel pairs with `uniqueOrCopy`'s acquire load: a dropped reader's
    /// reads complete before the owner mutates in place.
    pub fn release(self: *Pool, c: *Cell) void {
        if (c.refs.fetchSub(1, .acq_rel) != 1) return;
        self.lock.lock();
        defer self.lock.unlock();
        self.free.append(self.gpa, c) catch {
            self.gpa.free(c.data);
            self.gpa.destroy(c);
            _ = self.live.fetchSub(1, .monotonic);
        };
    }

    pub fn liveCells(self: *const Pool) u64 {
        return self.live.load(.monotonic);
    }

    /// `owned_mut`: unique ⇒ the same cell (acquire pairs with `release`'s
    /// acq_rel); shared ⇒ copy the covered runs into a fresh never-shared
    /// cell, drop our reference to the old one (`active_block_cow_copies`).
    /// Null at the cap.
    pub fn uniqueOrCopy(self: *Pool, c: *Cell, u: *const CoverageUnion) ?*Cell {
        if (c.refs.load(.acquire) == 1) return c;
        const fresh = self.acquire() orelse return null;
        var runs: [CoverageUnion.max_runs]cov.Run = undefined;
        for (u.runsSorted(&runs)) |r| @memcpy(fresh.data[r.start..r.end], c.data[r.start..r.end]);
        stats.active_block_cow_copies.inc();
        self.release(c);
        return fresh;
    }
};

// ---------------------------------------------------------------------------
// The entry.
// ---------------------------------------------------------------------------

/// Birth classes (§5.1).
pub const Seed = enum(u8) {
    /// No existing bytes: the complement is zeros by definition (served as
    /// zeros, zeroed lazily by `zeroComplete`).
    fresh,
    /// Existing device data; the seed read is postponed — full coverage in
    /// ANY order skips it forever.
    deferred,
    /// Content-valid at birth (the complement was materialized).
    seeded,
};

pub const Key = struct { ino: u64, block: u32 };

pub const Repr = union(enum) {
    full: *Pool.Cell,
    extent: extent.ExtentOverlay,
};

pub const Entry = struct {
    key: Key,
    repr: Repr,
    coverage: CoverageUnion = .empty,
    seed: Seed,
    /// Every byte of the buffer is correct (`deferred ⇒ !content_valid`).
    content_valid: bool,
    /// The PROCESS-GLOBALLY-UNIQUE stamp of the newest write (module doc).
    write_epoch: u64 = 0,
    /// The map's binding this block displaces (overwrite), null = fresh/hole.
    old_binding: ?Binding,
    /// The highest app-written end within the block (`min_size` floor).
    app_end: u32 = 0,
    born_ns: u64,
    last_touch_ns: u64,
    /// An upload task holds a snapshot (the entry is being written through).
    upload_inflight: bool = false,
    in_use: bool = false,

    pub fn isFull(self: *const Entry) bool {
        return self.repr == .full;
    }
};

pub const RecordError = error{
    /// The run table is full on a DEFERRED-seed buffer: the caller
    /// materializes the complement from the old image (`fillComplementFrom`)
    /// and retries — never a lost byte, never a zeroed overwrite.
    NeedsSeed,
    /// The pool is at its cap (a CoW copy could not be made).
    PoolExhausted,
};

pub const RecordVerdict = struct {
    /// The union is the whole block: the write-through trigger.
    completed: bool,
    out_of_order: bool,
};

/// A reader's immutable view: a shared cell + the coverage at snapshot time.
pub const Snapshot = struct {
    cell: *Pool.Cell,
    coverage: CoverageUnion,
    write_epoch: u64,
    len: u32,

    pub fn release(self: Snapshot, pool: *Pool) void {
        pool.release(self.cell);
    }

    pub fn bytes(self: Snapshot) []const u8 {
        return self.cell.data[0..self.len];
    }

    /// `[rel, rel + out.len)`: covered runs from the buffer, everything
    /// between them ZEROS (never buffer bytes).
    pub fn coveredRead(self: Snapshot, rel: u32, out: []u8) void {
        const end: u32 = rel + @as(u32, @intCast(out.len));
        var runs: [CoverageUnion.max_runs]cov.Run = undefined;
        var cursor: u32 = rel;
        for (self.coverage.coveredRunsIn(rel, end, &runs)) |r| {
            if (r.start > cursor) @memset(out[cursor - rel .. r.start - rel], 0);
            @memcpy(out[r.start - rel .. r.end - rel], self.cell.data[r.start..r.end]);
            cursor = r.end;
        }
        if (cursor < end) @memset(out[cursor - rel ..], 0);
    }
};

// ---------------------------------------------------------------------------
// The table.
// ---------------------------------------------------------------------------

var next_write_epoch: Atomic(u64) = .init(1);

pub fn nextWriteEpoch() u64 {
    return next_write_epoch.fetchAdd(1, .monotonic);
}

pub const Table = struct {
    gpa: Allocator,
    block_size: u32,
    pool: Pool,
    entries: []Entry,
    free: std.ArrayList(u32) = .empty,
    index: std.AutoHashMapUnmanaged(Key, *Entry) = .empty,
    lock: jc.SpinLock = .{},
    count: Atomic(u32) = .init(0),

    pub fn init(gpa: Allocator, block_size: u32, cap_buffers: u64) Allocator.Error!Table {
        const cap: usize = @intCast(cap_buffers);
        const entries = try gpa.alloc(Entry, cap);
        errdefer gpa.free(entries);
        var free = try std.ArrayList(u32).initCapacity(gpa, cap);
        errdefer free.deinit(gpa);
        var i: u32 = @intCast(cap);
        while (i > 0) : (i -= 1) free.appendAssumeCapacity(i - 1);
        var self: Table = .{ .gpa = gpa, .block_size = block_size, .pool = Pool.init(gpa, block_size, cap_buffers), .entries = entries, .free = free };
        try self.index.ensureTotalCapacity(gpa, @intCast(cap));
        return self;
    }

    pub fn deinit(self: *Table) void {
        for (self.entries) |*e| if (e.in_use) self.releaseRepr(e);
        self.index.deinit(self.gpa);
        self.free.deinit(self.gpa);
        self.gpa.free(self.entries);
        self.pool.deinit();
        self.* = undefined;
    }

    /// The R5 component: parked RAM is SHEDDABLE (the never-lossy mechanism
    /// is the flush/spill the owner binds); kernel-visible RSS.
    pub fn component(_: *Table, shed: ?budget.ShedFn) budget.Component {
        return budget.Component.init("parked_full_buffers", 0, 2, .{ .gauge = &stats.parked_full_buffer_bytes }, shed);
    }

    pub fn parkedCount(self: *const Table) u32 {
        return self.count.load(.monotonic);
    }

    /// Lock-free-ish custody probe (one spin-locked lookup; the W1 and
    /// overlay predicates ask it).
    pub fn has(self: *Table, ino: u64, block: u32) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.contains(.{ .ino = ino, .block = block });
    }

    /// The entry under the caller's level-3 guard (null = none).
    pub fn get(self: *Table, ino: u64, block: u32) ?*Entry {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.get(.{ .ino = ino, .block = block });
    }

    /// Arm G: the checkout. Existing ⇒ `write_block_revisits`; else a new
    /// entry of the seed class the caller judged from the LIVE map under
    /// the guard (`has_existing_bytes ⇒ deferred` with the displaced
    /// binding, else `fresh`). Null = the pool/table is at its cap (the
    /// caller stages the piece).
    pub fn checkout(self: *Table, ino: u64, block: u32, has_existing_bytes: bool, old_binding: ?Binding) ?*Entry {
        self.lock.lock();
        defer self.lock.unlock();
        const key: Key = .{ .ino = ino, .block = block };
        if (self.index.get(key)) |e| {
            stats.write_block_revisits.inc();
            return e;
        }
        const cell = self.pool.acquire() orelse return null;
        const idx = self.free.pop() orelse {
            stats.active_block_table_full.fire("entry table full below the pool cap");
            self.pool.release(cell);
            return null;
        };
        const e = &self.entries[idx];
        const now = clock.monotonicNs();
        e.* = .{
            .key = key,
            .repr = .{ .full = cell },
            .seed = if (has_existing_bytes) .deferred else .fresh,
            .content_valid = false,
            .old_binding = old_binding,
            .born_ns = now,
            .last_touch_ns = now,
            .in_use = true,
        };
        if (has_existing_bytes) stats.overwrite_seed_deferred.inc();
        self.index.putAssumeCapacity(key, e);
        _ = self.count.fetchAdd(1, .monotonic);
        stats.parked_full_buffer_bytes.add(self.block_size);
        stats.parked_buffers.add(1);
        return e;
    }

    /// Adopt an extent overlay's slabs as a full buffer (the 25 %
    /// escalation): a new full entry whose coverage and deferral are the
    /// extent's, slabs laid in place. Null at the cap.
    pub fn escalate(self: *Table, e: *Entry) ?void {
        assert(e.repr == .extent);
        const cell = self.pool.acquire() orelse return null;
        var ext = e.repr.extent;
        ext.layInto(cell.data);
        stats.parked_full_buffer_bytes.add(self.block_size);
        extent.stats.parked_extent_bytes.sub(ext.bytes);
        ext.deinit(self.gpa);
        e.repr = .{ .full = cell };
        if (e.seed == .deferred) stats.overwrite_seed_deferred.inc();
    }

    /// An extent entry (arm F): the compact sorted list, no block buffer.
    pub fn checkoutExtent(self: *Table, ino: u64, block: u32, has_existing_bytes: bool, old_binding: ?Binding) ?*Entry {
        self.lock.lock();
        defer self.lock.unlock();
        const key: Key = .{ .ino = ino, .block = block };
        if (self.index.get(key)) |e| return e;
        const idx = self.free.pop() orelse return null;
        const e = &self.entries[idx];
        const now = clock.monotonicNs();
        e.* = .{
            .key = key,
            .repr = .{ .extent = extent.ExtentOverlay.init(has_existing_bytes) },
            .seed = if (has_existing_bytes) .deferred else .fresh,
            .content_valid = false,
            .old_binding = old_binding,
            .born_ns = now,
            .last_touch_ns = now,
            .in_use = true,
        };
        self.index.putAssumeCapacity(key, e);
        _ = self.count.fetchAdd(1, .monotonic);
        stats.parked_buffers.add(1);
        return e;
    }

    /// Arm J: the merge — `record_write(rel, rel + len)`, then the ONE
    /// userspace copy into the unique (or copied) cell; the write epoch
    /// advances. A full run table on a FRESH buffer zero-completes the
    /// complement and completes (the never-lossy, exact exit); on a
    /// DEFERRED one the caller seeds first (`NeedsSeed`).
    pub fn recordWrite(self: *Table, e: *Entry, rel: u32, data: []const u8) RecordError!RecordVerdict {
        assert(e.repr == .full);
        const end: u32 = rel + @as(u32, @intCast(data.len));
        assert(end <= self.block_size);
        const v = e.coverage.record(rel, end, self.block_size) catch |err| switch (err) {
            error.RunTableFull => blk: {
                if (e.seed == .deferred) return error.NeedsSeed;
                self.zeroCompleteLocked(e);
                stats.active_block_run_table_completions.inc();
                break :blk cov.RecordVerdict{ .completed = true };
            },
        };
        const cell = self.pool.uniqueOrCopy(e.repr.full, &e.coverage) orelse return error.PoolExhausted;
        e.repr = .{ .full = cell };
        @memcpy(cell.data[rel..end], data);
        stats.nt_copy_bytes.add(data.len);
        if (v.out_of_order) stats.active_block_ooo_runs.inc();
        e.write_epoch = nextWriteEpoch();
        e.app_end = @max(e.app_end, end);
        e.last_touch_ns = clock.monotonicNs();
        const full = e.coverage.isFull(self.block_size);
        if (full) {
            if (e.coverage.n_extras != 0) stats.complete_union_with_extra_runs.fire("a complete union still holds extra runs");
            if (e.seed == .deferred) stats.overwrite_seed_skipped.inc();
            e.seed = .seeded;
            e.content_valid = true;
        }
        return .{ .completed = full, .out_of_order = v.out_of_order };
    }

    fn zeroCompleteLocked(self: *Table, e: *Entry) void {
        if (e.seed == .deferred) stats.zero_complete_on_deferred_seed.fire("zero_complete on a deferred-seed buffer");
        var gaps_buf: [CoverageUnion.max_gaps]cov.Run = undefined;
        var zeroed: u64 = 0;
        const cell = e.repr.full;
        for (e.coverage.gaps(self.block_size, &gaps_buf)) |g| {
            @memset(cell.data[g.start..g.end], 0);
            zeroed += g.len();
        }
        e.coverage.setFull(self.block_size);
        e.seed = .seeded;
        e.content_valid = true;
        stats.active_block_memset_elided_bytes.add(self.block_size - zeroed);
    }

    /// Every stage/upload exit on a FRESH buffer: zero EVERY gap, set full,
    /// content valid — the complement was zeros by definition. On a deferred
    /// seed the tripwire reports (never panics) and validity is still
    /// established (the caller should have seeded).
    pub fn zeroComplete(self: *Table, e: *Entry) void {
        assert(e.repr == .full);
        if (e.coverage.isFull(self.block_size)) return;
        self.zeroCompleteLocked(e);
    }

    /// Materialize the complement from the old image (the deferred seed's
    /// exit): copy the gaps, set full, content valid (`overwrite_seed_
    /// materialized`). Needs a unique cell.
    pub fn fillComplementFrom(self: *Table, e: *Entry, image: []const u8) RecordError!void {
        assert(e.repr == .full and image.len >= self.block_size);
        const cell = self.pool.uniqueOrCopy(e.repr.full, &e.coverage) orelse return error.PoolExhausted;
        e.repr = .{ .full = cell };
        var gaps_buf: [CoverageUnion.max_gaps]cov.Run = undefined;
        for (e.coverage.gaps(self.block_size, &gaps_buf)) |g| @memcpy(cell.data[g.start..g.end], image[g.start..g.end]);
        e.coverage.setFull(self.block_size);
        e.seed = .seeded;
        e.content_valid = true;
        stats.overwrite_seed_materialized.inc();
    }

    /// An immutable snapshot for a reader or the upload (released by the
    /// taker). Under the caller's guard.
    pub fn snapshot(self: *Table, e: *Entry) Snapshot {
        assert(e.repr == .full);
        return .{ .cell = e.repr.full.share(), .coverage = e.coverage, .write_epoch = e.write_epoch, .len = self.block_size };
    }

    fn releaseRepr(self: *Table, e: *Entry) void {
        switch (e.repr) {
            .full => |c| {
                self.pool.release(c);
                stats.parked_full_buffer_bytes.sub(self.block_size);
            },
            .extent => |*x| {
                extent.stats.parked_extent_bytes.sub(x.bytes);
                x.deinit(self.gpa);
            },
        }
    }

    /// Retire the entry (uploaded, truncated, spilled). Under the guard.
    pub fn retire(self: *Table, e: *Entry) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.index.remove(e.key);
        self.releaseRepr(e);
        e.in_use = false;
        const idx: u32 = @intCast((@intFromPtr(e) - @intFromPtr(self.entries.ptr)) / @sizeOf(Entry));
        self.free.appendAssumeCapacity(idx);
        _ = self.count.fetchSub(1, .monotonic);
        stats.parked_buffers.sub(1);
    }

    /// Every parked key of `ino` (fsync/release/truncate walks).
    pub fn blocksOf(self: *Table, ino: u64, out: []u32) []u32 {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        var it = self.index.keyIterator();
        while (it.next()) |k| {
            if (k.ino == ino and n < out.len) {
                out[n] = k.block;
                n += 1;
            }
        }
        return out[0..n];
    }

    /// Every parked key (the pressure pass).
    pub fn allKeys(self: *Table, out: []Key) []Key {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        var it = self.index.keyIterator();
        while (it.next()) |k| {
            if (n < out.len) {
                out[n] = k.*;
                n += 1;
            }
        }
        return out[0..n];
    }
};

// ---------------------------------------------------------------------------
// §5.3 Write-through: the pipeline upload.
// ---------------------------------------------------------------------------

/// `process_write` at every write path (charter §6.5): the volume's
/// transform (`transform.Transform.processWrite`) through a seam so the
/// core stays portable; passthrough returns `plain` itself.
pub const TransformSeam = struct {
    ctx: ?*anyopaque,
    process_write: *const fn (ctx: ?*anyopaque, vol_tag: u64, plain: []const u8, scratch: []u8) anyerror![]const u8,
    /// `transform.scratchLen(block)` for the volume (0 = passthrough: no
    /// scratch needed).
    scratch_len: *const fn (ctx: ?*anyopaque, vol_tag: u64) usize,

    pub const passthrough: TransformSeam = .{ .ctx = null, .process_write = &passWrite, .scratch_len = &noScratch };

    fn passWrite(_: ?*anyopaque, _: u64, plain: []const u8, _: []u8) anyerror![]const u8 {
        return plain;
    }

    fn noScratch(_: ?*anyopaque, _: u64) usize {
        return 0;
    }
};

pub const UploadDeps = struct {
    device: seams.DeviceSeam,
    map: seams.MapSeam,
    free: seams.FreeSeam,
    purge: seams.PurgeSeam,
    vols: *const seams.Volumes,
    picker: enospc.Picker,
    valve: ?enospc.Valve = null,
    epochs: *epoch_mod.Epochs,
    stripes: *layout.MetaStripes,
    locks: *seams.BlockLocks,
    transform: seams_transform,
    /// The scratch for a transformed image (`scratchLen` bytes, 4 KiB-
    /// aligned); null on an all-passthrough set.
    scratch: ?[]align(PAGE) u8 = null,
    /// The fsync touched word (every publish stamps it).
    touched: ?*const fn (ctx: ?*anyopaque, ino: u64, ordinal: u32) void = null,
    touched_ctx: ?*anyopaque = null,
};

const seams_transform = TransformSeam;

pub const Disposition = pipeline.Disposition;

/// `pipeline_upload_parked_block`: the detached upload of `(ino, block)`.
/// Returns the disposition (counted by the caller through
/// `pipeline.countDisposition`). `prof` is the caller's phase profile
/// (admit_wait/detach_lag already marked).
pub fn uploadParkedBlock(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32, prof: *pipeline.Prof) Disposition {
    const bs: u64 = table.block_size;
    while (true) {
        // The snapshot + the epoch stamp under (3).
        var snap: Snapshot = undefined;
        var old_binding: ?Binding = null;
        var app_end: u32 = 0;
        {
            const g = deps.locks.acquire(io, ino, block, .pipeline_upload);
            defer g.release(io);
            prof.mark(.lock_wait);
            const e = table.get(ino, block) orelse return .discarded;
            if (e.repr != .full or !e.content_valid or e.seed == .deferred) return .stay_parked;
            e.upload_inflight = true;
            snap = table.snapshot(e);
            old_binding = e.old_binding;
            app_end = e.app_end;
        }
        defer snap.release(&table.pool);
        // UNLOCKED device phase: transform → allocate (unstable) → DMA → publish.
        const pm = enospc.allocatePlacedMint(deps.picker, io, deps.valve, null, false) catch |e| switch (e) {
            error.StorageFull => {
                // KD-1.7: the ENOSPC early-close frees parked supply; a
                // release lets the next pass land, else the block stays
                // parked (the admission's terminal law refuses NEW custody).
                if (deps.epochs.closeAllForSpace(io, .{ .map = deps.map, .free = deps.free, .purge = deps.purge })) {
                    epoch_mod.stats.rewrite_shadow_fallbacks.inc();
                    continue;
                }
                stats.write_through_fallbacks.inc();
                return .stay_parked;
            },
            else => {
                stats.write_through_fallbacks.inc();
                return .stay_parked;
            },
        };
        var mint = block_alloc.MintedBlockGuard.init(pm.alloc, pm.minted);
        var fsck_guard = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx);
        const vol_tag = pm.alloc.volTag();
        const vol = deps.vols.byTag(vol_tag) orelse {
            fsck_guard.deinit();
            mint.abandon();
            return .stay_parked;
        };
        const need = deps.transform.scratch_len(deps.transform.ctx, vol_tag);
        const scratch: []u8 = if (need == 0) &.{} else (deps.scratch orelse {
            fsck_guard.deinit();
            mint.abandon();
            stats.write_through_fallbacks.inc();
            return .stay_parked;
        });
        const image = deps.transform.process_write(deps.transform.ctx, vol_tag, snap.bytes(), scratch) catch {
            fsck_guard.deinit();
            mint.abandon();
            stats.write_through_fallbacks.inc();
            return .stay_parked;
        };
        prof.mark(.crypto);
        prof.mark(.allocate);
        const proof = deps.device.write(vol_tag, pm.minted.offset, image) catch |e| {
            fsck_guard.deinit();
            mint.abandon();
            stats.write_through_fallbacks.inc();
            return switch (e) {
                error.WriterGuardFenced, error.VolumeFailed => .fence_drop,
                else => .stay_parked,
            };
        };
        prof.mark(.dma);
        pm.alloc.publishBlock(pm.minted.idx);
        const new_key = Binding.wholeBlock(vol_tag, pm.minted.offset, pm.minted.stamp);
        deps.purge.purge(new_key);
        // Re-lock, revalidate: entry present ∧ epoch unchanged ⇒ publish;
        // superseded ⇒ free the fresh offset, publish nothing (latest-wins).
        var displaced: seams.MergeOutcome = .{};
        var fed_hint = false;
        var fed = false;
        {
            const g = deps.locks.acquire(io, ino, block, .pipeline_upload);
            defer g.release(io);
            const e = table.get(ino, block) orelse {
                fsck_guard.deinit();
                mint.abandon();
                return .discarded;
            };
            if (e.write_epoch != snap.write_epoch) {
                e.upload_inflight = false;
                fsck_guard.deinit();
                mint.abandon();
                pipeline.countDisposition(.superseded, bs);
                continue; // re-drive with the newest bytes
            }
            const min_size = @as(u64, block) * bs + app_end;
            const striped = if (deps.map.view(ino)) |v| v.kind == .striped else false;
            if (old_binding != null and striped) {
                const v = deps.epochs.record(ino, block, old_binding, new_key, proof, min_size, fsck_guard, true) catch {
                    mint.abandon();
                    return .stay_parked;
                };
                switch (v) {
                    .shadowed => |s| {
                        if (s.displaced_prev) |d| deps.purge.purge(d);
                        mint.disarm();
                        fed = true;
                        fed_hint = s.coverage_complete;
                    },
                    .not_shadowed => fsck_guard = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx),
                }
            }
            if (!fed) {
                const sg = deps.stripes.acquire(ino);
                defer sg.release();
                while (true) {
                    displaced = deps.map.merge(.{ .ino = ino, .inserts = &.{.{ .block = block, .binding = new_key }}, .min_size = min_size, .proofs = &.{proof}, .fencing = deps.map.currentFencing(ino) }) catch |err| switch (err) {
                        error.FencingTokenExpired => continue,
                        error.WriterGuardFenced => {
                            fsck_guard.deinit();
                            mint.disarm(); // W5: free nothing
                            return .fence_drop;
                        },
                        error.NotFound => {
                            fsck_guard.deinit();
                            mint.abandon();
                            table.retire(e);
                            return .discarded;
                        },
                        error.Transient, error.OutOfMemory => {
                            fsck_guard.deinit();
                            mint.abandon();
                            e.upload_inflight = false;
                            stats.write_through_fallbacks.inc();
                            return .stay_parked;
                        },
                    };
                    break;
                }
                fsck_guard.deinit();
                mint.disarm();
                var orphans: [8]Binding = undefined;
                for (deps.epochs.supersede(ino, block, new_key, &orphans)) |o| _ = deps.free.free(io, o);
            }
            prof.mark(.publish);
            if (deps.touched) |t| t(deps.touched_ctx, ino, vol.ordinal);
            // `upload_invalidation_tail` UNDER the guard: the displaced
            // keys leave the read tiers before a new writer's fresh entry
            // can race the retire (the 2026-08-04 crash).
            for (displaced.slice()) |d| deps.purge.purge(d);
            table.retire(e);
            prof.mark(.inval_tail);
        }
        stats.write_through_blocks.inc();
        stats.write_through_bytes.add(bs);
        // Displaced frees OUTSIDE the guard (RES-1), then the epoch close
        // hint OUTSIDE the guard.
        for (displaced.slice()) |d| _ = deps.free.free(io, d);
        prof.mark(.displaced_free);
        if (fed_hint) _ = deps.epochs.close(io, ino, .park, .{ .map = deps.map, .free = deps.free, .purge = deps.purge });
        return .published;
    }
}

/// The synchronous flush of a PARTIAL parked block (an fsync's DUR-1
/// escalation, a release, a spill): complete the complement under (3) —
/// zeros on a fresh buffer, the old image on a deferred seed (one whole-
/// block read) — then upload. `durable_upload_bytes_escalation` counts it.
pub fn flushParkedBlock(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32) Disposition {
    const bs: u64 = table.block_size;
    {
        const g = deps.locks.acquire(io, ino, block, .writeback_flush);
        defer g.release(io);
        const e = table.get(ino, block) orelse return .discarded;
        if (e.repr == .extent) {
            if (table.escalate(e) == null) return .stay_parked;
        }
        if (!e.content_valid) {
            if (e.seed == .deferred and e.old_binding != null) {
                const old = e.old_binding.?;
                const img = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch return .stay_parked;
                defer table.gpa.free(img);
                deps.device.read(old.vol_tag, old.offset, img) catch return .stay_parked;
                table.fillComplementFrom(e, img) catch return .stay_parked;
            } else {
                // Fresh, or deferred without a binding (the existing bytes
                // were below EOF in a hole): the complement is zeros.
                e.seed = .fresh;
                table.zeroComplete(e);
            }
            stats.durable_upload_bytes_escalation.add(bs);
        }
    }
    // The guard is dropped: the upload takes its own.
    return uploadWith(table, deps, io, ino, block);
}

fn uploadWith(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32) Disposition {
    var prof = pipeline.Prof.start();
    const d = uploadParkedBlock(table, deps, io, ino, block, &prof);
    prof.finish();
    pipeline.countDisposition(d, table.block_size);
    return d;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

test "derivations: parked cap max(256, budget/16 ÷ bs), Red halves; scratch pool clamp(cpus/4, 4, 16)" {
    try testing.expectEqual(@as(u64, 256), parkedCapBuffers(4 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 1024), parkedCapBuffers(64 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 9), resolveParkedCap(9, 64 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 512), effectiveParkedCap(1024, .red));
    try testing.expectEqual(@as(u64, 1024), effectiveParkedCap(1024, .yellow));
    try testing.expectEqual(@as(u64, 4), scratchPoolCap(8));
    try testing.expectEqual(@as(u64, 8), scratchPoolCap(32));
    try testing.expectEqual(@as(u64, 16), scratchPoolCap(256));
}

test "the pool and the CoW cell: unique mutates in place; a shared cell copies the covered runs into a fresh one and drops the share; the cap refuses; recycled cells return" {
    var p = Pool.init(testing.allocator, 2 * PAGE, 2);
    defer p.deinit();
    const ref0 = stats.parked_pool_refusals.load();
    const cowp0 = stats.active_block_cow_copies.load();
    const a = p.acquire().?;
    const b = p.acquire().?;
    try testing.expect(p.acquire() == null); // at the cap
    try testing.expectEqual(ref0 + 1, stats.parked_pool_refusals.load());
    p.release(b);
    var u: CoverageUnion = .empty;
    _ = try u.record(0, PAGE, 2 * PAGE);
    @memset(a.data[0..PAGE], 7);
    try testing.expectEqual(a, p.uniqueOrCopy(a, &u).?); // unique: in place
    const shared = a.share();
    const c = p.uniqueOrCopy(a, &u).?; // shared: a copy
    try testing.expect(c != a);
    try testing.expect(std.mem.allEqual(u8, c.data[0..PAGE], 7));
    try testing.expectEqual(@as(u32, 1), shared.refs.load(.monotonic)); // our share dropped; the reader's remains
    try testing.expectEqual(cowp0 + 1, stats.active_block_cow_copies.load());
    p.release(shared);
    p.release(c);
    try testing.expectEqual(@as(u64, 2), p.liveCells());
}

test "the buffer: fresh seed is memset-elided and uncovered bytes never escape (coverage-aware reads zero-fill); out-of-order pieces complete the union (RW3b) and content becomes valid; zeroComplete establishes validity; the write epoch advances per record" {
    var t = try Table.init(testing.allocator, @intCast(wt.BS), 4);
    defer t.deinit();
    const cow0 = stats.active_block_cow_copies.load();
    const ooo0 = stats.active_block_ooo_runs.load();
    const copy0 = stats.nt_copy_bytes.load();
    const elided0 = stats.active_block_memset_elided_bytes.load();
    const e = t.checkout(1, 0, false, null).?;
    try testing.expectEqual(Seed.fresh, e.seed);
    try testing.expect(!e.content_valid);
    // Poison the recycled cell to prove the complement never escapes.
    @memset(e.repr.full.data, 0xEE);
    var p: [wt.BS / 4]u8 = undefined;
    for (&p, 0..) |*b, i| b.* = @truncate(i);
    const v1 = try t.recordWrite(e, wt.BS / 2, &p); // tail-ish piece first (out of order relative to nothing: it is the primary)
    try testing.expect(!v1.completed);
    const ep1 = e.write_epoch;
    const v2 = try t.recordWrite(e, 0, &p); // disjoint: an extra
    try testing.expect(v2.out_of_order and !v2.completed);
    try testing.expect(e.write_epoch > ep1);
    try testing.expectEqual(ooo0 + 1, stats.active_block_ooo_runs.load());
    // A reader: covered runs from the buffer, gaps ZEROS (never 0xEE).
    var snap = t.snapshot(e);
    var out: [wt.BS]u8 = undefined;
    snap.coveredRead(0, &out);
    try testing.expectEqualSlices(u8, &p, out[0 .. wt.BS / 4]);
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 4 .. wt.BS / 2], 0));
    try testing.expectEqualSlices(u8, &p, out[wt.BS / 2 .. 3 * wt.BS / 4]);
    try testing.expect(std.mem.allEqual(u8, out[3 * wt.BS / 4 ..], 0));
    try testing.expectEqual(@as(u32, 2), e.repr.full.refs.load(.monotonic));
    // A write while the snapshot is live copies (CoW); the snapshot is unchanged.
    const v3 = try t.recordWrite(e, wt.BS / 4, &p);
    try testing.expect(!v3.completed);
    try testing.expectEqual(cow0 + 1, stats.active_block_cow_copies.load());
    try testing.expect(std.mem.allEqual(u8, snap.cell.data[wt.BS / 4 .. wt.BS / 2], 0xEE)); // the reader's cell untouched
    snap.release(&t.pool);
    // The last piece completes; content valid; the seed class resolves.
    const v4 = try t.recordWrite(e, 3 * wt.BS / 4, &p);
    try testing.expect(v4.completed);
    try testing.expect(e.content_valid);
    try testing.expectEqual(Seed.seeded, e.seed);
    try testing.expectEqual(copy0 + 4 * (wt.BS / 4), stats.nt_copy_bytes.load());
    try testing.expectEqual(@as(u32, 1), t.parkedCount());
    const parked_gauge = stats.parked_full_buffer_bytes.load();
    t.retire(e);
    try testing.expectEqual(parked_gauge - wt.BS, stats.parked_full_buffer_bytes.load());
    // zeroComplete on a partial fresh buffer: gaps zeroed, full, valid, elided bytes counted.
    const f = t.checkout(1, 1, false, null).?;
    @memset(f.repr.full.data, 0xEE);
    _ = try t.recordWrite(f, 0, &p);
    t.zeroComplete(f);
    try testing.expect(f.content_valid and f.coverage.isFull(@intCast(wt.BS)));
    try testing.expect(std.mem.allEqual(u8, f.repr.full.data[wt.BS / 4 ..], 0));
    try testing.expectEqual(elided0 + wt.BS / 4, stats.active_block_memset_elided_bytes.load());
    t.retire(f);
}

test "the deferred seed: an overwrite's partial coverage parks with the seed postponed; full coverage in any order skips it forever; a partial exit materializes the complement from the old image; zeroComplete on a deferred seed trips the wire" {
    var t = try Table.init(testing.allocator, @intCast(wt.BS), 4);
    defer t.deinit();
    const old = Binding.wholeBlock(wt.VOL_A, 0, 1);
    const def0 = stats.overwrite_seed_deferred.load();
    const skip0 = stats.overwrite_seed_skipped.load();
    const mat0 = stats.overwrite_seed_materialized.load();
    const e = t.checkout(2, 0, true, old).?;
    try testing.expectEqual(Seed.deferred, e.seed);
    try testing.expectEqual(def0 + 1, stats.overwrite_seed_deferred.load());
    var half: [wt.BS / 2]u8 = @splat(0x11);
    _ = try t.recordWrite(e, wt.BS / 2, &half);
    try testing.expect(!e.content_valid);
    const v = try t.recordWrite(e, 0, &half);
    try testing.expect(v.completed and e.content_valid);
    try testing.expectEqual(skip0 + 1, stats.overwrite_seed_skipped.load());
    t.retire(e);
    // Partial + materialize.
    const f = t.checkout(2, 1, true, old).?;
    _ = try t.recordWrite(f, 0, &half);
    var image: [wt.BS]u8 = @splat(0xAA);
    try t.fillComplementFrom(f, &image);
    try testing.expect(f.content_valid);
    try testing.expect(std.mem.allEqual(u8, f.repr.full.data[0 .. wt.BS / 2], 0x11));
    try testing.expect(std.mem.allEqual(u8, f.repr.full.data[wt.BS / 2 ..], 0xAA));
    try testing.expectEqual(mat0 + 1, stats.overwrite_seed_materialized.load());
    t.retire(f);
    // The tripwire.
    const g = t.checkout(2, 2, true, old).?;
    _ = try t.recordWrite(g, 0, &half);
    const zc0 = counters.tripwireCount("zero_complete_on_deferred_seed");
    stats.zero_complete_on_deferred_seed.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    t.zeroComplete(g);
    try log.testing.expectAllFired();
    try testing.expectEqual(zc0 + 1, counters.tripwireCount("zero_complete_on_deferred_seed"));
    try testing.expect(g.content_valid); // validity is still established
    t.retire(g);
}

test "the run-table overflow exit: a fresh buffer zero-completes exactly and completes; a deferred one answers NeedsSeed with the state unchanged" {
    var t = try Table.init(testing.allocator, 1024 * PAGE, 2);
    defer t.deinit();
    const e = t.checkout(3, 0, false, null).?;
    var page: [PAGE]u8 = @splat(0x5A);
    var i: u32 = 0;
    while (i <= cov.MAX_EXTRA_RUNS) : (i += 1) _ = try t.recordWrite(e, (2 * i) * PAGE, &page); // 65 disjoint runs: primary + 64 extras
    try testing.expectEqual(@as(u8, cov.MAX_EXTRA_RUNS), e.coverage.n_extras);
    const v = try t.recordWrite(e, 1000 * PAGE, &page); // the 66th disjoint run
    try testing.expect(v.completed and e.content_valid);
    try testing.expect(stats.active_block_run_table_completions.load() >= 1);
    try testing.expect(std.mem.allEqual(u8, e.repr.full.data[1000 * PAGE .. 1001 * PAGE], 0x5A));
    try testing.expect(std.mem.allEqual(u8, e.repr.full.data[1 * PAGE .. 2 * PAGE], 0));
    t.retire(e);
    const d = t.checkout(3, 1, true, Binding.wholeBlock(wt.VOL_A, 0, 1)).?;
    i = 0;
    while (i <= cov.MAX_EXTRA_RUNS) : (i += 1) _ = try t.recordWrite(d, (2 * i) * PAGE, &page);
    const before = d.coverage;
    try testing.expectError(error.NeedsSeed, t.recordWrite(d, 1000 * PAGE, &page));
    try testing.expectEqual(before.n_extras, d.coverage.n_extras);
    try testing.expect(!d.content_valid);
    t.retire(d);
}

const UploadWorld = struct {
    f: *wt.Fixture,
    table: Table,
    epochs: epoch_mod.Epochs,
    stripes: layout.MetaStripes,
    locks: seams.BlockLocks,
    touched: u32 = 0,

    fn init(gpa: Allocator) !*UploadWorld {
        const w = try gpa.create(UploadWorld);
        errdefer gpa.destroy(w);
        w.* = .{ .f = try wt.Fixture.init(gpa, 32), .table = try Table.init(gpa, @intCast(wt.BS), 8), .epochs = epoch_mod.Epochs.init(gpa, wt.BS, true), .stripes = .{}, .locks = try seams.BlockLocks.init(gpa, 64) };
        return w;
    }

    fn deinit(self: *UploadWorld, gpa: Allocator) void {
        self.locks.deinit(gpa);
        self.epochs.deinit();
        self.table.deinit();
        self.f.deinit();
        gpa.destroy(self);
    }

    fn pickA(ctx: ?*anyopaque) ?*block_alloc.BlockAllocator {
        return @ptrCast(@alignCast(ctx.?));
    }

    fn stamp(ctx: ?*anyopaque, _: u64, _: u32) void {
        const self: *UploadWorld = @ptrCast(@alignCast(ctx.?));
        self.touched += 1;
    }

    fn deps(self: *UploadWorld) UploadDeps {
        return .{ .device = self.f.dev.seam(), .map = self.f.map.seam(), .free = self.f.frees.seam(), .purge = .none, .vols = &self.f.vols, .picker = .{ .ctx = self.f.alloc_a, .pick = &pickA }, .epochs = &self.epochs, .stripes = &self.stripes, .locks = &self.locks, .transform = TransformSeam.passthrough, .touched = &stamp, .touched_ctx = self };
    }
};

test "write-through of a completed fresh block: 1 copy + 1 DMA, allocate unstable → DMA → publish → revalidate → ONE merge with the app-written size floor; the entry retires; the touched word is stamped; the phase legs sum to total" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(5, .striped, 0);
    const e = w.table.checkout(5, 1, false, null).?;
    var data: [wt.BS]u8 = undefined;
    for (&data, 0..) |*b, i| b.* = @truncate(i * 3);
    const v = try w.table.recordWrite(e, 0, &data);
    try testing.expect(v.completed);
    const copies0 = stats.nt_copy_bytes.load();
    var prof = pipeline.Prof.start();
    const d = uploadParkedBlock(&w.table, w.deps(), io, 5, 1, &prof);
    prof.finish();
    try testing.expectEqual(Disposition.published, d);
    try testing.expectEqual(copies0, stats.nt_copy_bytes.load()); // the upload copies nothing more
    try testing.expectEqual(@as(usize, 1), w.f.dev.writes.items.len);
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.writes.items[0].len);
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(@as(u64, 2 * wt.BS), w.f.map.merges.items[0].min_size); // block 1's end
    const b = w.f.map.seam().lookup(5, 1).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expectEqualSlices(u8, &data, &got);
    try testing.expect(w.f.alloc_a.fillIncarnation(w.f.alloc_a.idxOf(b.offset)) != null); // published
    try testing.expect(!w.table.has(5, 1));
    try testing.expectEqual(@as(u32, 1), w.touched);
    try testing.expect(stats.write_through_blocks.load() >= 1);
    var sum: u64 = 0;
    for (prof.legs) |l| sum += l;
    try testing.expectEqual(prof.last - prof.t0, sum);
}

test "stale_inflight_upload_never_publishes_and_newest_bytes_win: a write landing between the snapshot and the revalidation supersedes the upload (the fresh offset is freed, nothing published) and the re-drive publishes the newest bytes" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(6, .striped, 0);
    const e = w.table.checkout(6, 0, false, null).?;
    var first: [wt.BS]u8 = @splat(0x01);
    var second: [wt.BS]u8 = @splat(0x02);
    _ = try w.table.recordWrite(e, 0, &first);
    // The upload-stall seam: a racing writer lands during the unlocked device phase.
    const Racer = struct {
        var tbl: *Table = undefined;
        var newest: []const u8 = undefined;
        fn hook(_: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
            // The hook runs during the upload's DMA (no block guard held): a sibling write records the newest bytes.
            const g = tbl.get(6, 0).?;
            _ = tbl.recordWrite(g, 0, newest) catch unreachable;
        }
    };
    Racer.tbl = &w.table;
    Racer.newest = &second;
    var dummy: u8 = 0;
    w.f.dev.hook = .{ .ctx = &dummy, .call = &Racer.hook };
    const sup0 = pipeline.stats.write_pipeline_supersessions.load();
    const used0 = w.f.alloc_a.usedBlocks();
    block_alloc.stats.unpublished_mint_abandons.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1); // the superseded mint's abandon
    var prof = pipeline.Prof.start();
    // The hook fires on EVERY DMA: disarm it after the first.
    const Once = struct {
        var fired: bool = false;
        fn hook(ctx: *anyopaque, dev: *wt.FakeDevice, a: u64, b: u64, c: u64) void {
            if (fired) return;
            fired = true;
            Racer.hook(ctx, dev, a, b, c);
        }
    };
    w.f.dev.hook = .{ .ctx = &dummy, .call = &Once.hook };
    const d = uploadParkedBlock(&w.table, w.deps(), io, 6, 0, &prof);
    prof.finish();
    try log.testing.expectAllFired();
    try testing.expectEqual(Disposition.published, d);
    try testing.expectEqual(sup0 + 1, pipeline.stats.write_pipeline_supersessions.load());
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len); // the stale pass published nothing
    try testing.expectEqual(used0 + 1, w.f.alloc_a.usedBlocks()); // the orphan offset returned; one block owned
    const b = w.f.map.seam().lookup(6, 0).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, &got, 0x02)); // the newest bytes won
    w.f.dev.hook = null;
}

test "the overwrite write-through FEEDS the rewrite epoch (no per-block merge); a full-coverage feed closes the epoch; ENOSPC early-close frees parked supply and the upload lands; a fenced merge is a counted fence drop freeing nothing" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(7, 0, 0xAA);
    const e = w.table.checkout(7, 0, true, a).?;
    var data: [wt.BS]u8 = @splat(0x33);
    _ = try w.table.recordWrite(e, 0, &data);
    const d = uploadWith(&w.table, w.deps(), io, 7, 0);
    try testing.expectEqual(Disposition.published, d);
    // Fed + full coverage (size BS, one block) ⇒ the close ran: ONE save, A freed.
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a));
    try testing.expect(std.mem.allEqual(u8, &data, 0x33));
    // ENOSPC: fill the volume; park an epoch with a displaced key; the upload's StorageFull closes it and lands.
    const a2 = try w.f.publishBlock(8, 0, 0xBB);
    const a3 = try w.f.publishBlock(8, 1, 0xCC);
    // Shadow block 0 of ino 8 with a mint, leaving a2 parked.
    const m = try w.f.alloc_a.allocateBlock();
    _ = try w.epochs.record(8, 0, a2, Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp), .{ .vol_tag = wt.VOL_A, .offset = m.offset, .len = wt.BS, .completed_ns = 1 }, 2 * wt.BS, block_alloc.InflightAllocGuard.register(w.f.alloc_a, m.idx), true);
    while (w.f.alloc_a.allocateBlock()) |_| {} else |_| {}
    const e2 = w.table.checkout(8, 1, true, a3).?;
    _ = try w.table.recordWrite(e2, 0, &data);
    const fb0 = epoch_mod.stats.rewrite_shadow_fallbacks.load();
    try testing.expectEqual(Disposition.published, uploadWith(&w.table, w.deps(), io, 8, 1));
    try testing.expectEqual(fb0 + 1, epoch_mod.stats.rewrite_shadow_fallbacks.load());
    try testing.expect(w.f.map.seam().lookup(8, 1).?.eql(a3)); // fed, not yet saved (half the file shadowed)
    try testing.expect(w.epochs.bindsBlock(8, 1));
    try testing.expect(w.epochs.close(io, 8, .park, .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none }) == .swapped);
    try testing.expect(!w.f.map.seam().lookup(8, 1).?.eql(a3));
    // Fence on a fresh block's merge: fence_drop, nothing published, the mint not freed (W5).
    try w.f.map.create(9, .striped, 0);
    const e3 = w.table.checkout(9, 0, false, null).?;
    _ = try w.table.recordWrite(e3, 0, &data);
    w.f.map.fail_next_merge = error.WriterGuardFenced;
    const used = w.f.alloc_a.usedBlocks();
    pipeline.stats.write_pipeline_fence_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Disposition.fence_drop, uploadWith(&w.table, w.deps(), io, 9, 0));
    try log.testing.expectAllFired();
    try testing.expectEqual(used + 1, w.f.alloc_a.usedBlocks()); // W5: the mint is NOT freed — successor recovery owns the accounting
    try testing.expect(w.table.has(9, 0)); // the entry stays (publish nothing, free nothing)
}

test "flushParkedBlock (DUR-1 escalation): a partial fresh block zero-completes and uploads; a partial deferred block seeds its complement from the old image by one read, then uploads; the seed never reads for a complete block" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(10, .striped, 0);
    const e = w.table.checkout(10, 2, false, null).?;
    var half: [wt.BS / 2]u8 = @splat(0x44);
    _ = try w.table.recordWrite(e, 0, &half);
    const esc0 = stats.durable_upload_bytes_escalation.load();
    try testing.expectEqual(Disposition.published, flushParkedBlock(&w.table, w.deps(), io, 10, 2));
    try testing.expectEqual(esc0 + wt.BS, stats.durable_upload_bytes_escalation.load());
    try testing.expectEqual(@as(u64, 0), w.f.dev.reads);
    const b = w.f.map.seam().lookup(10, 2).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x44));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
    try testing.expectEqual(2 * wt.BS + wt.BS / 2, w.f.map.merges.items[0].min_size); // SIZE NEVER LEADS DATA: the app-written end
    // Deferred: one seed read of the old image.
    const a = try w.f.publishBlock(11, 0, 0xAA);
    const f = w.table.checkout(11, 0, true, a).?;
    _ = try w.table.recordWrite(f, 0, &half);
    w.f.dev.resetLedger();
    try testing.expectEqual(Disposition.published, flushParkedBlock(&w.table, w.deps(), io, 11, 0));
    try testing.expectEqual(@as(u64, 1), w.f.dev.reads);
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.read_bytes);
    _ = w.epochs.close(io, 11, .park, .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none });
    const nb = w.f.map.seam().lookup(11, 0).?;
    w.f.dev.peek(wt.VOL_A, nb.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x44));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0xAA));
}
