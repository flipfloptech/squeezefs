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
//! The base of an overwrite — the key its complement is seeded from and
//! the key its feed displaces — is read from the WRITE-SIDE AUTHORITY at
//! the moment it is used, under (3): the rewrite epoch's newest shadow
//! (open or saving) else the durable map (`Epochs.baseBinding`). The
//! checkout's `old_binding` is that authority's answer at birth, kept for
//! the read seam; the flush re-derives it (an epoch may have saved
//! meanwhile — the same key, now the map's) and the feed hands the live
//! map key to `record`, which prefers its shadows. A base snapshotted from
//! the durable map while the epoch shadowed the block was the 2026-10-07
//! lost overwrite (`write_epoch.zig` module doc).
//!
//! Transformed volumes take a worst-case scratch (`transform.scratchLen`,
//! block + 4 KiB — deliberately not the block pool, whose exactly-block
//! buffers cannot hold the table and tags) from the write path's
//! `transform.ScratchPool` for the transform AND the DMA that lands the
//! image: the ONE shared scratch it replaces was overwritten by the next
//! upload's transform while the previous upload's DMA still read it (two
//! uploads in flight on a transformed volume = a torn image on the device).
//! Compression is best-effort per block (store-raw inside `processWrite`).

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
const sync_notify = @import("../sync/notify.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const enospc = @import("enospc.zig");
const cov = @import("write_coverage.zig");
const seams = @import("write_seams.zig");
const epoch_mod = @import("write_epoch.zig");
const pipeline = @import("write_pipeline.zig");
const extent = @import("extent_overlay.zig");
const sole_owner_patch = @import("sole_owner_patch.zig");
const transform = @import("transform.zig");

pub const Binding = seams.Binding;
pub const CoverageUnion = cov.CoverageUnion;
pub const PAGE: u32 = seams.PAGE;
/// The join's bark bound: an upload is one bounded DMA plus one
/// transaction; 30 s is the overlay settle's own bound (`SETTLE_BARK_NS`)
/// — loud, never a hang.
pub const UPLOAD_JOIN_BARK_NS: u64 = 30 * std.time.ns_per_s;

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

// ---------------------------------------------------------------------------
// Stats (§11.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var write_through_blocks: counters.ShardedCounter = .{};
    pub var write_through_bytes: counters.ShardedCounter = .{};
    pub var write_through_fallbacks: counters.ShardedCounter = .{};
    /// A flush waited the bark bound on another task's upload (must-stay-0:
    /// an upload is a bounded DMA + one transaction).
    pub var upload_join_overlong: counters.Tripwire = .init("upload_join_overlong");
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
    /// `parked_pool_trimmed_cells`: cached cells the idle trim freed.
    pub var parked_pool_trimmed_cells: counters.ShardedCounter = .{};
    pub var durable_upload_bytes_escalation: counters.ShardedCounter = .{};
    /// A merge whose bytes already lived in the entry's cell (a placed
    /// payload of an adopted assembly — the pointer proof): the copy elided.
    pub var placed_merge_elides: counters.ShardedCounter = .{};
    // Tripwires.
    pub var coverage_complete_buffer_not_content_valid: counters.Tripwire = .init("coverage_complete_buffer_not_content_valid");
    pub var complete_union_with_extra_runs: counters.Tripwire = .init("complete_union_with_extra_runs");
    pub var zero_complete_on_deferred_seed: counters.Tripwire = .init("zero_complete_on_deferred_seed");
    pub var active_block_table_full: counters.Tripwire = .init("active_block_table_full");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "write_through_blocks", "write_through_bytes", "write_through_fallbacks", "nt_copy_bytes", "active_block_cow_copies", "active_block_memset_elided_bytes", "active_block_ooo_runs", "active_block_run_table_completions", "overwrite_seed_deferred", "overwrite_seed_materialized", "overwrite_seed_skipped", "write_block_revisits", "parked_pool_refusals", "parked_pool_trimmed_cells", "durable_upload_bytes_escalation", "placed_merge_elides" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"parked_full_buffer_bytes\":{d},\"parked_buffers\":{d},", .{ stats.parked_full_buffer_bytes.load(), stats.parked_buffers.load() });
    inline for (.{ "coverage_complete_buffer_not_content_valid", "complete_union_with_extra_runs", "zero_complete_on_deferred_seed", "active_block_table_full", "upload_join_overlong" }, 0..) |name, i| {
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
        /// The budget tick of the cell's last release (the idle clock).
        idle_tick: u64 = 0,

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
        c.idle_tick = budget.authority.ticks();
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

    /// The pool's RAM: every cell it allocated (parked + cached) × size —
    /// a whole touched block each.
    pub fn liveBytes(self: *const Pool) u64 {
        return self.live.load(.monotonic) * self.size;
    }

    /// The idle-trim law: free the cached cells idle ≥ `min_idle_ticks`
    /// ticks. The free list is taken in batches under the lock and freed
    /// outside it (a 4 MiB unmap per cell must not stall a writer's acquire
    /// on the spinlock). Returns the cells freed.
    pub fn trimIdle(self: *Pool, min_idle_ticks: u64) u32 {
        const now = budget.authority.ticks();
        var freed: u32 = 0;
        var batch: [64]*Cell = undefined;
        while (true) {
            var n: usize = 0;
            {
                self.lock.lock();
                defer self.lock.unlock();
                var i: usize = 0;
                while (i < self.free.items.len and n < batch.len) {
                    const c = self.free.items[i];
                    if (now -| c.idle_tick >= min_idle_ticks) {
                        batch[n] = self.free.swapRemove(i);
                        n += 1;
                    } else {
                        i += 1;
                    }
                }
            }
            if (n == 0) return freed;
            for (batch[0..n]) |c| {
                self.gpa.free(c.data);
                self.gpa.destroy(c);
            }
            _ = self.live.fetchSub(n, .monotonic);
            stats.parked_pool_trimmed_cells.add(n);
            freed += @intCast(n);
            if (n < batch.len) return freed;
        }
    }

    /// A cell is available now (a free one, or room below the cap): the
    /// gate's re-check — a hint, not a reservation (the acquire decides).
    pub fn hasRoom(self: *Pool) bool {
        self.lock.lock();
        defer self.lock.unlock();
        return self.free.items.len != 0 or self.live.load(.monotonic) < self.cap;
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
    /// Notified whenever an upload task ends (publish, discard, supersede,
    /// a transient): the JOIN of a second flush of the same block (an
    /// fsync or a RELEASE racing the write-through) waits here instead of
    /// minting a second block for one image.
    upload_settled: sync_notify.Notify = .init,
    /// Notified when a parked entry RETIRES (a cell and an entry slot
    /// returned): the parked-buffer gate's wake. Not `upload_settled` — an
    /// upload that failed settles too, and three writers at the cap whose
    /// cold flushes all fail would wake one another forever (the 3.75 M
    /// gate rounds of 2026-10-10).
    capacity: sync_notify.Notify = .init,

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

    /// The R5 component: the pool's RAM (parked entries AND the cached
    /// cells a release left for the next writer — whole touched blocks,
    /// kernel-visible RSS). The idle-trim law frees the cached cells. The
    /// shed is the owner's never-lossy drain (flush/spill) when it binds
    /// one; the parked cap's own Red response is `effectiveParkedCap`.
    pub fn component(self: *Table, shed: ?budget.ShedFn) budget.Component {
        return budget.Component.init("parked_full_buffers", 0, if (shed != null) 2 else 0, .{ .func = .{ .ctx = self, .call = &poolBytesFn } }, shed).withTrim(.{ .ctx = self, .call = &trimFn });
    }

    fn poolBytesFn(ctx: *anyopaque) u64 {
        const self: *Table = @ptrCast(@alignCast(ctx));
        return self.pool.liveBytes();
    }

    fn trimFn(ctx: *anyopaque, min_idle_ticks: u64) void {
        const self: *Table = @ptrCast(@alignCast(ctx));
        _ = self.pool.trimIdle(min_idle_ticks);
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
        return self.checkoutWith(ino, block, has_existing_bytes, old_binding, null);
    }

    /// Arm G's ADOPTING checkout (charter §7 "placed sever … a shared
    /// assembly the first merge ADOPTS"): the entry's backing is `adopt` —
    /// a placed-sever assembly's cell whose share the caller hands over —
    /// instead of a fresh pool cell. Adoption changes WHERE the backing
    /// memory came from, never the coverage/seed law; the cell is non-
    /// unique while placed payloads are out, so an in-place mutation copies
    /// away (`uniqueOrCopy`). The caller holds (3) and saw no entry; an
    /// entry present after all is a logic error (asserted). Null = the
    /// entry table is full — the caller releases its share.
    pub fn checkoutAdopting(self: *Table, ino: u64, block: u32, has_existing_bytes: bool, old_binding: ?Binding, adopt: *Pool.Cell, landed: []const cov.Run) ?*Entry {
        const e = self.checkoutWith(ino, block, has_existing_bytes, old_binding, adopt) orelse return null;
        // The landed runs ARE coverage (the bytes are in the cell): at most
        // `max_runs` of them (the registry refused the adoption otherwise),
        // so the record cannot overflow.
        for (landed) |r| {
            _ = e.coverage.record(r.start, r.end, self.block_size) catch unreachable;
            e.app_end = @max(e.app_end, r.end);
        }
        e.write_epoch = nextWriteEpoch();
        if (e.coverage.isFull(self.block_size)) {
            if (e.seed == .deferred) stats.overwrite_seed_skipped.inc();
            e.seed = .seeded;
            e.content_valid = true;
        }
        return e;
    }

    fn checkoutWith(self: *Table, ino: u64, block: u32, has_existing_bytes: bool, old_binding: ?Binding, adopt: ?*Pool.Cell) ?*Entry {
        self.lock.lock();
        defer self.lock.unlock();
        const key: Key = .{ .ino = ino, .block = block };
        if (self.index.get(key)) |e| {
            assert(adopt == null);
            stats.write_block_revisits.inc();
            return e;
        }
        const cell = adopt orelse (self.pool.acquire() orelse return null);
        const idx = self.free.pop() orelse {
            stats.active_block_table_full.fire("entry table full below the pool cap");
            if (adopt == null) self.pool.release(cell);
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

    /// Adopt an extent overlay's pages as a CONTENT-VALID full buffer (the
    /// escalation, §J.1): the touched pages laid in place, the untouched
    /// ones from `base` (the whole plaintext image of the block's binding —
    /// the CoW arm's ONE seed read) or zeros (a hole, a deferred block
    /// without a binding). The buffer is complete: coverage full, seeded,
    /// content-valid — every later write is a memcpy and the block lands
    /// ONCE at its flush. Null at the pool cap (nothing changed).
    pub fn escalate(self: *Table, e: *Entry, base: ?[]const u8) ?void {
        assert(e.repr == .extent);
        const cell = self.pool.acquire() orelse return null;
        var ext = e.repr.extent;
        ext.completeInto(cell.data[0..self.block_size], base);
        stats.parked_full_buffer_bytes.add(self.block_size);
        ext.deinit(self.gpa);
        e.repr = .{ .full = cell };
        e.coverage.setFull(self.block_size);
        e.seed = .seeded;
        e.content_valid = true;
        e.last_touch_ns = clock.monotonicNs();
        extent.stats.extent_escalations.inc();
    }

    /// Arm G's SEEDED checkout (the reference's `seeded(existing, bs)`
    /// birth class): a new full entry whose cell already holds the block's
    /// whole plaintext image — content-valid, coverage full — for the small
    /// write into a block whose base cannot be seeded at the page grain (a
    /// transformed volume: a plaintext page costs a whole-frame decode). The
    /// piece then merges by memcpy and the block lands ONCE at its flush.
    /// `fill` reads the image into the cell; a failed read leaves no entry.
    pub fn checkoutSeeded(self: *Table, ino: u64, block: u32, old_binding: Binding, fill: *const fn (ctx: ?*anyopaque, b: Binding, out: []u8) seams.ReadError!void, fill_ctx: ?*anyopaque) error{ PoolExhausted, InputOutput }!*Entry {
        const cell = self.pool.acquire() orelse return error.PoolExhausted;
        fill(fill_ctx, old_binding, cell.data[0..self.block_size]) catch {
            self.pool.release(cell);
            return error.InputOutput;
        };
        const e = self.checkoutWith(ino, block, false, old_binding, cell) orelse {
            self.pool.release(cell);
            return error.PoolExhausted;
        };
        e.coverage.setFull(self.block_size);
        e.seed = .seeded;
        e.content_valid = true;
        stats.overwrite_seed_materialized.inc();
        return e;
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
            // The overlay defers to a base only when there IS one: existing
            // bytes below EOF with no binding are a hole (zeros).
            .repr = .{ .extent = extent.ExtentOverlay.init(has_existing_bytes and old_binding != null) },
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
    /// DEFERRED one the caller seeds first (`NeedsSeed`). A payload whose
    /// bytes ARE the cell's region at `rel` (the pointer proof: a placed
    /// sever into an assembly this entry adopted) is a coverage record
    /// alone — no uniqueness copy, no memcpy (`placed_merge_elides`); the
    /// bytes landed under the claim before the seal, and nothing mutates a
    /// claimed region.
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
        if (@intFromPtr(data.ptr) == @intFromPtr(e.repr.full.data.ptr) + rel) {
            stats.placed_merge_elides.inc();
        } else {
            const cell = self.pool.uniqueOrCopy(e.repr.full, &e.coverage) orelse return error.PoolExhausted;
            e.repr = .{ .full = cell };
            @memcpy(cell.data[rel..end], data);
            stats.nt_copy_bytes.add(data.len);
        }
        if (v.out_of_order) stats.active_block_ooo_runs.inc();
        e.write_epoch = nextWriteEpoch();
        e.app_end = @max(e.app_end, end);
        e.last_touch_ns = clock.monotonicNs();
        // The write-through trigger is the TRANSITION to a full union (a
        // re-write of an already complete entry never double-fires: an
        // upload in flight re-drives on the epoch it stamped, and a
        // content-valid buffer parked with a full union — the W2
        // escalation's write-back class — lands once, at its flush).
        if (v.completed) {
            if (e.coverage.n_extras != 0) stats.complete_union_with_extra_runs.fire("a complete union still holds extra runs");
            if (e.seed == .deferred) stats.overwrite_seed_skipped.inc();
            e.seed = .seeded;
            e.content_valid = true;
        }
        return .{ .completed = v.completed, .out_of_order = v.out_of_order };
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
            .extent => |*x| x.deinit(self.gpa), // credits the pages' charge
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
    /// Whether any block of `ino` is parked (or uploading) — the no-open
    /// FLUSH's try-only probe (decision 0003 §AF); O(1) on an empty table,
    /// else the `blocksOf` scan under the spin lock.
    pub fn holdsIno(self: *Table, ino: u64) bool {
        // Monotonic suffices: a write's checkout increments under the lock
        // before its ACK, and the probe's FLUSH reaches here after that ACK
        // through the open table's stripe lock (`flushBegin`).
        if (self.count.load(.monotonic) == 0) return false;
        self.lock.lock();
        defer self.lock.unlock();
        var it = self.index.keyIterator();
        while (it.next()) |k| if (k.ino == ino) return true;
        return false;
    }

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

    /// The gate's re-check: an entry slot AND a cell are available now (a
    /// hint; the checkout decides).
    pub fn hasRoom(self: *Table) bool {
        {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.free.items.len == 0) return false;
        }
        return self.pool.hasRoom();
    }

    /// Some parked entry's upload is in flight: a cell will return without
    /// a flush (the gate waits for it); none ⇒ only a flush can.
    pub fn anyUploadInflight(self: *Table) bool {
        self.lock.lock();
        defer self.lock.unlock();
        for (self.entries) |*e| if (e.in_use and e.upload_inflight) return true;
        return false;
    }

    /// A gate victim's identity and how long it has sat untouched.
    pub const Coldest = struct { ino: u64, block: u32, age_ns: u64 };

    /// The least recently touched parked entry other than `(ino, block)`
    /// and not mid-upload — a FULL one when the pool is out of cells (an
    /// extent flush needs a cell to escalate into), any repr when only an
    /// entry slot is wanted. Null: nothing flushable is parked.
    pub fn coldestParked(self: *Table, ino: u64, block: u32, now_ns: u64) ?Coldest {
        const need_cell = !self.pool.hasRoom();
        self.lock.lock();
        defer self.lock.unlock();
        var best: ?*const Entry = null;
        for (self.entries) |*e| {
            if (!e.in_use or e.upload_inflight) continue;
            if (e.key.ino == ino and e.key.block == block) continue;
            if (need_cell and e.repr != .full) continue;
            if (best == null or e.last_touch_ns < best.?.last_touch_ns) best = e;
        }
        const b = best orelse return null;
        return .{ .ino = b.key.ino, .block = b.key.block, .age_ns = now_ns -| b.last_touch_ns };
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

    /// The syncfs census: every parked key appended to `out`. The room is
    /// the table's fixed capacity, reserved OUTSIDE the lock, so the fill
    /// under it never allocates and never truncates.
    pub fn censusKeys(self: *Table, gpa: Allocator, out: *std.ArrayList(Key)) Allocator.Error!void {
        try out.ensureUnusedCapacity(gpa, self.entries.len);
        self.lock.lock();
        defer self.lock.unlock();
        var it = self.index.keyIterator();
        while (it.next()) |k| out.appendAssumeCapacity(k.*);
    }

    /// Every parked EXTENT entry's key (the D3 fold targets: the W2
    /// overlays charged as `parked_extent_bytes`), owned by `gpa`. A
    /// snapshot under the table lock: an entry that escalates or retires
    /// between the census and the kick is the kick's clean no-op.
    pub fn extentKeys(self: *Table, gpa: Allocator) Allocator.Error![]Key {
        var out: std.ArrayList(Key) = .empty;
        errdefer out.deinit(gpa);
        self.lock.lock();
        defer self.lock.unlock();
        var it = self.index.iterator();
        while (it.next()) |kv| {
            if (kv.value_ptr.*.repr == .extent) try out.append(gpa, kv.key_ptr.*);
        }
        return out.toOwnedSlice(gpa);
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
    /// The stored-image scratches (`scratchLen` bytes each, DMA-aligned),
    /// one held per upload from its transform through its DMA; null on an
    /// all-passthrough set.
    scratch: ?*transform.ScratchPool = null,
    /// The fsync touched word (every publish stamps it).
    touched: ?*const fn (ctx: ?*anyopaque, ino: u64, ordinal: u32) void = null,
    touched_ctx: ?*anyopaque = null,
    /// The PLAINTEXT whole-block image a binding names (a transformed
    /// volume's frame decoded) — every whole-block seed reads through it;
    /// null = raw through `device` (an all-passthrough set, the tests).
    image: ?seams.ImageSeam = null,
    /// The in-place fold's state half (W1's: custody, the sole-owner
    /// verdict, the fence pair); null = the in-place arm is never taken.
    custody: ?sole_owner_patch.CustodyProbe = null,
};

/// The whole plaintext image of `b` into `out` (`image` when bound, else
/// the raw device bytes).
pub fn readImage(deps: UploadDeps, b: Binding, out: []u8) seams.ReadError!void {
    if (deps.image) |im| return im.read(im.ctx, b, out);
    return deps.device.read(b.vol_tag, b.offset, out);
}

const seams_transform = TransformSeam;

pub const Disposition = pipeline.Disposition;

/// `pipeline_upload_parked_block`: the detached upload of `(ino, block)`.
/// Returns the disposition (counted by the caller through
/// `pipeline.countDisposition`). `prof` is the caller's phase profile
/// (admit_wait/detach_lag already marked).
pub fn uploadParkedBlock(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32, prof: *pipeline.Prof) Disposition {
    const bs: u64 = table.block_size;
    // ONE upload per entry at a time: a second flush of the same block (an
    // fsync or a RELEASE racing the write-through, two RELEASEs of one
    // file) JOINS the in-flight one — register-then-recheck on the table's
    // settle notify — instead of minting a second block for the same image
    // and abandoning it at the publish (`unpublished_mint_abandons`).
    var joined = false;
    // `upload_inflight` is cleared on EVERY exit that leaves the entry
    // parked, and the settle is notified on every exit.
    var retired_by_us = false;
    defer {
        if (retired_by_us) {
            table.capacity.notifyWaiters(io);
        } else {
            const g = deps.locks.acquire(io, ino, block, .pipeline_upload);
            if (table.get(ino, block)) |e| e.upload_inflight = false;
            g.release(io);
        }
        table.upload_settled.notifyWaiters(io);
    }
    drive: while (true) {
        // The snapshot + the epoch stamp under (3).
        var snap: Snapshot = undefined;
        var app_end: u32 = 0;
        {
            var settle = table.upload_settled.notified();
            _ = settle.enable(io);
            var inflight_elsewhere = false;
            {
                const g = deps.locks.acquire(io, ino, block, .pipeline_upload);
                defer g.release(io);
                prof.mark(.lock_wait);
                const e = table.get(ino, block) orelse {
                    settle.cancel(io);
                    return if (joined) .joined else .discarded;
                };
                if (e.upload_inflight) {
                    inflight_elsewhere = true;
                } else {
                    settle.cancel(io);
                    if (e.repr != .full or !e.content_valid or e.seed == .deferred) return .stay_parked;
                    e.upload_inflight = true;
                    snap = table.snapshot(e);
                    app_end = e.app_end;
                }
            }
            if (inflight_elsewhere) {
                joined = true;
                pipeline.stats.write_pipeline_join_waits.inc();
                if (settle.waitUntilUncancelable(io, clock.monotonicNs() + UPLOAD_JOIN_BARK_NS) == .deadline) {
                    stats.upload_join_overlong.fire("a flush waited 30 s on another task's upload of the same block");
                    log.err("ino {d} block {d}: an upload has been in flight for 30 s while a second flush waits on it", .{ ino, block });
                }
                continue;
            }
        }
        defer snap.release(&table.pool);
        // UNLOCKED device phase: transform → allocate (unstable) → DMA → publish.
        // The mint WAITS for visible supply (a window being refilled, a
        // trim window, a pending drain): this upload returns a parked cell
        // and has no cheaper arm, so it outranks a fresh overlay mint
        // (which declines to accumulation at once) under scarce supply.
        const pm = enospc.allocatePlacedMint(deps.picker, .{ .ino = ino, .block = block }, io, deps.valve, null, false, .visible_supply) catch |e| switch (e) {
            error.StorageFull => {
                // KD-1.7: the ENOSPC early-close frees parked supply; a
                // release lets the next pass land, else the block stays
                // parked (the admission's terminal law refuses NEW custody).
                if (deps.epochs.closeAllForSpace(io, .{ .map = deps.map, .free = deps.free, .purge = deps.purge })) {
                    epoch_mod.stats.rewrite_shadow_fallbacks.inc();
                    clearInflight(table, deps, io, ino, block);
                    continue; // re-drive: the re-check above would otherwise join OUR OWN flag
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
        const scratch_pool: ?*transform.ScratchPool = if (need == 0) null else (deps.scratch orelse {
            fsck_guard.deinit();
            mint.abandon();
            stats.write_through_fallbacks.inc();
            return .stay_parked;
        });
        // The scratch is held from the transform through the DMA that
        // reads the image out of it, and not an instant longer.
        const proof = blk: {
            const scratch: []align(PAGE) u8 = if (scratch_pool) |sp| sp.acquire(io) else &.{};
            defer if (scratch_pool) |sp| sp.release(io, scratch);
            const image = deps.transform.process_write(deps.transform.ctx, vol_tag, snap.bytes(), scratch) catch {
                fsck_guard.deinit();
                mint.abandon();
                stats.write_through_fallbacks.inc();
                return .stay_parked;
            };
            prof.mark(.crypto);
            prof.mark(.allocate);
            break :blk deps.device.write(vol_tag, pm.minted.offset, image) catch |e| {
                fsck_guard.deinit();
                mint.abandon();
                stats.write_through_fallbacks.inc();
                return switch (e) {
                    error.WriterGuardFenced, error.VolumeFailed => .fence_drop,
                    else => .stay_parked,
                };
            };
        };
        prof.mark(.dma);
        pm.alloc.publishBlock(pm.minted.idx);
        const new_key = Binding.wholeBlock(vol_tag, pm.minted.offset, pm.minted.stamp);
        deps.purge.purge(new_key);
        // Re-lock, revalidate: entry present ∧ epoch unchanged ⇒ publish;
        // superseded ⇒ free the fresh offset, publish nothing (latest-wins).
        // A record the epoch answers `save_in_flight` releases the guard,
        // awaits the ino's save (never under (3)) and re-enters here with
        // the same minted image — one DMA, the publish deferred past the
        // save it must not land under.
        var displaced: seams.MergeOutcome = .{};
        var fed_hint = false;
        var fed = false;
        var await_save = false;
        publish: while (true) {
            if (await_save) {
                await_save = false;
                deps.epochs.awaitSaveSettled(io, ino);
            }
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
                continue :drive; // re-drive with the newest bytes
            }
            const min_size = @as(u64, block) * bs + app_end;
            const striped = if (deps.map.view(ino)) |v| v.kind == .striped else false;
            // An OVERWRITE displaces the write-side authority's key: the
            // live map's, or an epoch's shadow (`record` prefers its own
            // and the saving epoch's) — never the checkout's snapshot.
            const live = deps.map.lookup(ino, block);
            const overwrite = live != null or deps.epochs.bindsBlock(ino, block);
            if (overwrite and striped) {
                const v = deps.epochs.record(ino, block, live, new_key, proof, min_size, fsck_guard, true) catch {
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
                    .save_in_flight => {
                        // The mint stays visible to fsck across the wait.
                        fsck_guard = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx);
                        await_save = true;
                        continue :publish;
                    },
                }
            }
            if (!fed) {
                const sg = deps.stripes.acquire(io, ino);
                defer sg.release(io);
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
                            retired_by_us = true;
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
            retired_by_us = true;
            prof.mark(.inval_tail);
            break;
        }
        stats.write_through_blocks.inc();
        stats.write_through_bytes.add(bs);
        // Displaced frees OUTSIDE the guard (RES-1), then the epoch close
        // hint OUTSIDE the guard.
        for (displaced.slice()) |d| _ = deps.free.free(io, d);
        prof.mark(.displaced_free);
        if (fed_hint) _ = deps.epochs.close(io, ino, .{ .map = deps.map, .free = deps.free, .purge = deps.purge });
        return .published;
    }
}

/// The in-flight flag cleared under (3) for a re-drive of the same task.
fn clearInflight(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32) void {
    const g = deps.locks.acquire(io, ino, block, .pipeline_upload);
    defer g.release(io);
    if (table.get(ino, block)) |e| e.upload_inflight = false;
}

/// The synchronous flush of a PARTIAL parked block (an fsync's DUR-1
/// escalation, a release, a spill): complete the complement under (3) —
/// zeros on a fresh buffer, the old image on a deferred seed (one whole-
/// block read) — then upload. `durable_upload_bytes_escalation` counts it.
/// An extent entry folds IN PLACE when its block is a sole-owner
/// passthrough mapping (§J.1 landing 2: the touched pages, nothing else,
/// no upload); else it escalates (seeded whole for a CoW base) and uploads.
pub fn flushParkedBlock(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32) Disposition {
    const bs: u64 = table.block_size;
    {
        const g = deps.locks.acquire(io, ino, block, .writeback_flush);
        defer g.release(io);
        const e = table.get(ino, block) orelse return .discarded;
        if (e.repr == .extent) {
            if (foldInPlaceLocked(table, deps, e)) |d| {
                if (d == .published) table.capacity.notifyWaiters(io);
                return d;
            }
            escalateExtentLocked(table, deps, e) catch |err| switch (err) {
                error.PoolExhausted => return .stay_parked,
                error.InputOutput, error.OutOfMemory => {
                    stats.write_through_fallbacks.inc();
                    return .stay_parked;
                },
            };
            stats.durable_upload_bytes_escalation.add(bs);
        }
        if (!e.content_valid) {
            // The complement's base is the write-side authority's key NOW
            // (module doc): an epoch's newest shadow, else the map's.
            const base: ?Binding = if (e.seed == .deferred) deps.epochs.baseBinding(deps.map, ino, block) else null;
            if (base) |old| {
                const img = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch return .stay_parked;
                defer table.gpa.free(img);
                readImage(deps, old, img) catch return .stay_parked;
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
// §J.1 The W2 fold: in place at the page grain, or the whole-block escalation.
// ---------------------------------------------------------------------------

/// The RAM trigger's act (`extent.foldDue`, after the write's guard
/// dropped): a deferred overlay over a sole-owner passthrough mapping folds
/// IN PLACE and retires; every other shape escalates to a content-valid full
/// buffer (seeded whole from the base for a CoW block, zeros for a hole) and
/// stays PARKED — the block lands once, at its flush. An entry that is no
/// longer an extent (escalated by a sibling, flushed) is left alone.
pub fn foldExtentBlock(table: *Table, deps: UploadDeps, io: Io, ino: u64, block: u32) void {
    const g = deps.locks.acquire(io, ino, block, .fold);
    defer g.release(io);
    const e = table.get(ino, block) orelse return;
    if (e.repr != .extent) return;
    if (foldInPlaceLocked(table, deps, e)) |d| {
        if (d == .published) table.capacity.notifyWaiters(io);
        return;
    }
    escalateExtentLocked(table, deps, e) catch |err| switch (err) {
        // The pool is at its cap: the extent stays parked (the next write
        // or the flush escalates once a cell exists) — never a park under (3).
        error.PoolExhausted => extent.stats.extent_escalations_deferred.inc(),
        error.InputOutput, error.OutOfMemory => stats.write_through_fallbacks.inc(),
    };
}

/// The CoW arm's escalation under the caller's (3): the whole plaintext
/// base image (ONE read; `fold_seed_reads`) under the touched pages for a
/// deferred overlay with a binding, zeros otherwise — a content-valid full
/// buffer that parks.
pub fn escalateExtentLocked(table: *Table, deps: UploadDeps, e: *Entry) error{ PoolExhausted, InputOutput, OutOfMemory }!void {
    assert(e.repr == .extent);
    const bs: u64 = table.block_size;
    const x = &e.repr.extent;
    const k = x.count();
    const base: ?Binding = if (x.base_deferred) deps.epochs.baseBinding(deps.map, e.key.ino, e.key.block) else null;
    if (base) |old| {
        const img = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch return error.OutOfMemory;
        defer table.gpa.free(img);
        readImage(deps, old, img) catch return error.InputOutput;
        extent.stats.fold_seed_reads.inc();
        table.escalate(e, img) orelse return error.PoolExhausted;
    } else {
        table.escalate(e, null) orelse return error.PoolExhausted;
    }
    extent.noteFold(k);
}

/// §J.1 landing 2, under the caller's (3): a deferred overlay (every
/// touched page content-valid: the base's page + the user's bytes) over a
/// block W1's state half passes for — the undecorated whole-block mapping
/// on a passthrough volume, `refcount == 1 ∧ !SHARED` through the fence
/// pair, no overlay record, no epoch binding — lands as ONE DMA per
/// contiguous run of touched pages, at `mapping.offset + run.start`: zero
/// allocation, free, map merge, journal. Then the W1 postlude (re-stabilize
/// under the new generation, purge the four tiers, the touched stamp) and
/// the entry retires. Null = declined (`fold_inplace_declines`): a hole, a
/// CoW base, or no custody probe bound — the whole-block arm. A failed DMA
/// leaves the entry PARKED (never lossy; the pages it holds are the truth
/// and the next flush retries), counted and loud.
/// The in-place fold's base: the mapped block's own device bytes.
const InPlaceBase = struct {
    device: seams.DeviceSeam,
    vol_tag: u64,
    dest: u64,

    fn read(ctx: ?*anyopaque, rel: u32, dst: []u8) seams.ReadError!void {
        const self: *const InPlaceBase = @ptrCast(@alignCast(ctx.?));
        return self.device.read(self.vol_tag, self.dest + rel, dst);
    }
};

fn foldInPlaceLocked(table: *Table, deps: UploadDeps, e: *Entry) ?Disposition {
    const x = &e.repr.extent;
    if (!x.base_deferred) return null;
    const probe = deps.custody orelse return null;
    const pdeps: sole_owner_patch.Deps = .{ .device = deps.device, .map = deps.map, .vols = deps.vols, .purge = deps.purge, .custody = probe, .touched = if (deps.touched) |t| .{ .ctx = deps.touched_ctx, .stamp = t } else null };
    const begun = switch (sole_owner_patch.beginInPlace(pdeps, e.key.ino, e.key.block, 0) catch {
        extent.stats.fold_inplace_declines.inc();
        return null;
    }) {
        .begun => |b| b,
        .declined => {
            extent.stats.fold_inplace_declines.inc();
            return null;
        },
    };
    const bs: u64 = table.block_size;
    // The page runs assembled into a page-aligned scratch (the pages are
    // separate allocations; a run is ONE DMA).
    const scratch = table.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(bs)) catch {
        sole_owner_patch.abandonInPlace(pdeps, begun);
        return null;
    };
    defer table.gpa.free(scratch);
    // The seed: the base's bytes under the unseeded pages, ONE ranged read
    // per contiguous run, from the mapping itself (the block is the base).
    const base_ctx: InPlaceBase = .{ .device = deps.device, .vol_tag = begun.vol_tag, .dest = begun.dest };
    x.seedRuns(table.gpa, .{ .ctx = @constCast(&base_ctx), .read = &InPlaceBase.read }, scratch) catch {
        sole_owner_patch.abandonInPlace(pdeps, begun);
        stats.write_through_fallbacks.inc();
        return .stay_parked;
    };
    var runs_buf: [extent.MAX_PAGES]extent.Run = undefined;
    const runs = x.pageRuns(&runs_buf);
    x.layInto(scratch);
    var written: u64 = 0;
    var dma: seams.DmaError!void = {};
    for (runs) |r| {
        dma = if (deps.device.write(begun.vol_tag, begun.dest + r.start, scratch[r.start..r.end])) |_| {} else |err| err;
        if (dma) |_| {
            written += r.end - r.start;
            extent.stats.fold_inplace_dmas.inc();
        } else |_| break;
    }
    sole_owner_patch.finishInPlace(pdeps, begun, dma, "W2 in-place fold") catch {
        extent.stats.fold_inplace_dma_errors.inc();
        extent.stats.fold_inplace_write_bytes.add(written);
        return .stay_parked;
    };
    extent.stats.fold_inplace_passes.inc();
    extent.stats.fold_inplace_pages.add(x.pageCount());
    extent.stats.fold_inplace_write_bytes.add(written);
    extent.noteFold(x.count());
    table.retire(e);
    return .published;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

test "derivations: parked cap max(256, budget/16 ÷ bs), Red halves" {
    try testing.expectEqual(@as(u64, 256), parkedCapBuffers(4 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 1024), parkedCapBuffers(64 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 9), resolveParkedCap(9, 64 * derive.GiB, 4 * derive.MiB));
    try testing.expectEqual(@as(u64, 512), effectiveParkedCap(1024, .red));
    try testing.expectEqual(@as(u64, 1024), effectiveParkedCap(1024, .yellow));
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
    // Both cells are cached: the pool's RAM stays 2 cells until the idle
    // trim frees them (Red: everything idle); a Yellow trim leaves cells
    // released in this tick.
    try testing.expectEqual(@as(u64, 4 * PAGE), p.liveBytes());
    try testing.expectEqual(@as(u32, 0), p.trimIdle(budget.trim_yellow_min_idle_ticks));
    try testing.expectEqual(@as(u32, 2), p.trimIdle(budget.trim_red_min_idle_ticks));
    try testing.expectEqual(@as(u64, 0), p.liveBytes());
    const d = p.acquire().?; // the pool refills below the cap
    p.release(d);
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

    fn pickA(ctx: ?*anyopaque, _: enospc.Place) ?*block_alloc.BlockAllocator {
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
    const abandons0 = block_alloc.stats.unpublished_mint_abandons.load();
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
    try testing.expectEqual(Disposition.published, d);
    try testing.expectEqual(abandons0 + 1, block_alloc.stats.unpublished_mint_abandons.load()); // the superseded mint went back
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
    try testing.expect(w.epochs.close(io, 8, .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none }) == .swapped);
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
    _ = w.epochs.close(io, 11, .{ .map = w.f.map.seam(), .free = w.f.frees.seam(), .purge = .none });
    const nb = w.f.map.seam().lookup(11, 0).?;
    w.f.dev.peek(wt.VOL_A, nb.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x44));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0xAA));
}

test "two flushes of one parked block (a RELEASE racing an fsync): the second JOINS the in-flight upload — one mint, one merge, no abandoned mint; the joiner answers `joined`" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(12, .striped, 0);
    const e = w.table.checkout(12, 0, false, null).?;
    var data: [wt.BS]u8 = @splat(0x55);
    _ = try w.table.recordWrite(e, 0, &data);
    // The first upload's DMA parks on the hook until the joiner is waiting.
    const Gate = struct {
        var hold: std.atomic.Value(bool) = .init(true);
        var in_dma: std.atomic.Value(bool) = .init(false);
        fn hook(_: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
            in_dma.store(true, .release);
            while (hold.load(.acquire)) std.Thread.yield() catch {};
        }
    };
    var dummy: u8 = 0;
    w.f.dev.hook = .{ .ctx = &dummy, .call = &Gate.hook };
    const Uploader = struct {
        fn run(io_: Io, table: *Table, deps: UploadDeps, out: *Disposition) void {
            out.* = uploadWith(table, deps, io_, 12, 0);
        }
    };
    const used0 = w.f.alloc_a.usedBlocks();
    const abandons0 = block_alloc.stats.unpublished_mint_abandons.load();
    const joins0 = pipeline.stats.write_pipeline_joins.load();
    const join_waits0 = pipeline.stats.write_pipeline_join_waits.load();
    var first: Disposition = .stay_parked;
    const th = try std.Thread.spawn(.{}, Uploader.run, .{ io, &w.table, w.deps(), &first });
    while (!Gate.in_dma.load(.acquire)) try std.Thread.yield();
    // The joiner: flushParkedBlock on the same block while the DMA is held.
    const Joiner = struct {
        fn run(io_: Io, table: *Table, deps: UploadDeps, out: *Disposition) void {
            out.* = flushParkedBlock(table, deps, io_, 12, 0);
        }
    };
    var second: Disposition = .stay_parked;
    const tj = try std.Thread.spawn(.{}, Joiner.run, .{ io, &w.table, w.deps(), &second });
    // The rendezvous is the joiner's OBSERVATION of the in-flight upload
    // (`write_pipeline_join_waits`), never its settle registration: the
    // waiter registers BEFORE it takes the lock, so a hold released on
    // `waiters() != 0` let the first upload publish and retire the entry
    // ahead of the joiner's first look — `.discarded`, a correct word for
    // an entry gone, where this row asserts the join.
    while (pipeline.stats.write_pipeline_join_waits.load() == join_waits0) try std.Thread.yield();
    Gate.hold.store(false, .release);
    th.join();
    tj.join();
    w.f.dev.hook = null;
    try testing.expectEqual(Disposition.published, first);
    try testing.expectEqual(Disposition.joined, second);
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(used0 + 1, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(abandons0, block_alloc.stats.unpublished_mint_abandons.load());
    try testing.expectEqual(joins0 + 1, pipeline.stats.write_pipeline_joins.load());
    try testing.expect(!w.table.has(12, 0));
}

/// A pseudo-transform for the scratch-pool law: the image is the plaintext
/// XOR a marker, built IN the scratch (as every real transform's is).
const XorSeam = struct {
    const MARK: u8 = 0x5A;

    fn processWrite(_: ?*anyopaque, _: u64, plain: []const u8, scratch: []u8) anyerror![]const u8 {
        if (scratch.len < plain.len) return error.ScratchTooSmall;
        for (plain, scratch[0..plain.len]) |p, *s| s.* = p ^ MARK;
        return scratch[0..plain.len];
    }

    fn scratchLen(_: ?*anyopaque, _: u64) usize {
        return wt.BS;
    }

    const seam: TransformSeam = .{ .ctx = null, .process_write = &processWrite, .scratch_len = &scratchLen };
};

/// A device whose FIRST write is a DMA in flight: it reads its source
/// bytes only once a second write has been issued (the second upload has
/// transformed and reached its DMA), then forwards to the fake device.
const InFlightDevice = struct {
    inner: seams.DeviceSeam,
    entered: std.atomic.Value(u32) = .init(0),

    fn seam(self: *InFlightDevice) seams.DeviceSeam {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: seams.DeviceSeam.VTable = .{ .write = &write, .write_fixed = null, .read = &read, .flush = &flush };

    fn write(ctx: *anyopaque, vol: u64, off: u64, data: []const u8) seams.DmaError!void {
        const self: *InFlightDevice = @ptrCast(@alignCast(ctx));
        if (self.entered.fetchAdd(1, .acq_rel) == 0) {
            while (self.entered.load(.acquire) < 2) std.Thread.yield() catch {};
        }
        return self.inner.vtable.write(self.inner.ctx, vol, off, data);
    }

    fn read(ctx: *anyopaque, vol: u64, off: u64, out: []u8) seams.ReadError!void {
        const self: *InFlightDevice = @ptrCast(@alignCast(ctx));
        return self.inner.vtable.read(self.inner.ctx, vol, off, out);
    }

    fn flush(ctx: *anyopaque, vol: u64) seams.DmaError!void {
        const self: *InFlightDevice = @ptrCast(@alignCast(ctx));
        return self.inner.vtable.flush(self.inner.ctx, vol);
    }
};

test "the transform scratch is held through the DMA (decision 0003 §K10): two uploads of a transformed volume in flight at once land their OWN images — the second's transform never overwrites the scratch the first's DMA is still reading (red on the one shared scratch: block 1 landed block 2's image)" {
    const io = testing.io;
    const w = try UploadWorld.init(testing.allocator);
    defer w.deinit(testing.allocator);
    try w.f.map.create(21, .striped, 0);
    var data1: [wt.BS]u8 = undefined;
    var data2: [wt.BS]u8 = undefined;
    for (&data1, 0..) |*b, i| b.* = @truncate(i * 3);
    for (&data2, 0..) |*b, i| b.* = @truncate(i * 7 + 1);
    _ = try w.table.recordWrite(w.table.checkout(21, 1, false, null).?, 0, &data1);
    _ = try w.table.recordWrite(w.table.checkout(21, 2, false, null).?, 0, &data2);
    var pool = try transform.ScratchPool.init(testing.allocator, 2, wt.BS);
    defer pool.deinit(testing.allocator);
    var dev: InFlightDevice = .{ .inner = w.f.dev.seam() };
    var deps = w.deps();
    deps.device = dev.seam();
    deps.transform = XorSeam.seam;
    deps.scratch = &pool;
    const Uploader = struct {
        fn run(io_: Io, table: *Table, d: UploadDeps, block: u32, out: *Disposition) void {
            var prof = pipeline.Prof.start();
            out.* = uploadParkedBlock(table, d, io_, 21, block, &prof);
        }
    };
    var first: Disposition = .stay_parked;
    var second: Disposition = .stay_parked;
    const t1 = try std.Thread.spawn(.{}, Uploader.run, .{ io, &w.table, deps, 1, &first });
    // The first upload is in its DMA before the second starts (its scratch
    // is the one at risk).
    while (dev.entered.load(.acquire) == 0) try std.Thread.yield();
    const t2 = try std.Thread.spawn(.{}, Uploader.run, .{ io, &w.table, deps, 2, &second });
    t1.join();
    t2.join();
    try testing.expectEqual(Disposition.published, first);
    try testing.expectEqual(Disposition.published, second);
    // Every block landed ITS plaintext's image.
    var got: [wt.BS]u8 = undefined;
    for ([_]struct { block: u32, data: *const [wt.BS]u8 }{ .{ .block = 1, .data = &data1 }, .{ .block = 2, .data = &data2 } }) |row| {
        const b = w.f.map.seam().lookup(21, row.block).?;
        w.f.dev.peek(wt.VOL_A, b.offset, &got);
        for (&got) |*g| g.* ^= XorSeam.MARK;
        try testing.expectEqualSlices(u8, row.data, &got);
    }
    // Both scratches came back.
    try testing.expectEqual(@as(usize, 2), pool.permits.available(io));
}
