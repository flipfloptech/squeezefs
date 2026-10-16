//! The device overlay — arm B of the write admission (charter §6.2 item 1
//! "Device overlay (zero daemon copies)"; `docs/porting/data_write_path.md`
//! §2.2, §3). A reader-visible RAM record `{old_binding | Hole, dest (fresh,
//! unpublished, incarnation unstable, fsck visibility guard, mint rollback
//! owner), completed coverage, in-flight range claims + generations, state
//! Open → Frozen → Published | Superseded | FenceDropped | Fed}` installed
//! BEFORE the ack; the destination reserved BEFORE the DMA; `WRITE_FIXED
//! (device fd ← slot)` straight from the registered zero-copy slot (or ONE
//! DMA from an owned buffer); CQE ⇒ the range joins coverage; completion ⇒
//! ONE transaction flips map + refs, or FEEDS the rewrite epoch for an
//! overwrite; the displaced binding is freed only after the durable
//! publish. Always a fresh destination, never in place: ACK-early + a crash
//! mid-DMA into a mapped offset would tear durable bytes. Volatile: no
//! on-disk form, no incompat bit, no journal record.
//!
//! Measured (`design-overlay-overwrite.md:1216`): 4 MiB aligned overwrite
//! ingest **31.0 → 32.1 GiB/s** (A-B-B-A both orders: 32.1 vs 31.0 / 31.6
//! vs 30.9) at HALF the daemon CPU (26.5–27.3 vs 52.7–53.1 jiffies/GiB),
//! `nt_copy` merge share **1.000 → 0.001**, `rewrite_amp 1.0000`.
//!
//! THE DEKKER PAIR (store buffering): the writer publishes `inflight += 1`
//! THEN reads `state`; the freezer publishes `state = Frozen` THEN reads
//! `inflight`. Zig 0.17 has no standalone fence, so the four accesses are
//! themselves SeqCst — C11's single total order over seq_cst operations is
//! the guarantee (ARMv8 `stlr`/`ldar` RCsc; x86 locked `xchg`): at least one
//! side observes the other, so a store either backs out on the freeze or is
//! visible to the freezer's drain. `device_overlay_model.zig` proves it and
//! proves the §3.3 stale-DMA counterexample (two overlapping stores in
//! flight) is unrepresentable under the range claims.
//!
//! The settle (`FREEZE → COMPLETE → SEED GAPS → FLUSH → PUBLISH → META SYNC
//! → deferred frees`) is the fsync sequence; the write arm's form retries
//! transients forever on the ONE ladder (`write_retry.zig`), fence
//! immediate — on EVERY leg: a `VolumeFailed`/`WriterGuardFenced` from the
//! seed read, the seed write or the flush is the fence, never a transient.
//! Each attempt is one (3) acquisition: a transient leaves the record
//! FROZEN never-lossy and the ladder's backoff runs with the block guard
//! DROPPED (charter §2: no park under the block guard). Before that a
//! settler sleeping under (3) on a device that kept answering EIO convoyed
//! every RELEASE of the ino and every write to the block behind it (the
//! 2026-10-07 wedge: five releases parked on one block's guard, `lock_
//! ticked_reregisters` climbing, no holder the watchdog could name).
//! Must-stay-0: `overlay_fence_drops`, `overlay_unpublished_at_fsync`,
//! `overlay_ack_early_lost`; tripwires `overlay_teardown_nonterminal`,
//! `overlay_install_race_under_guard`, `overlay_store_superseded_under_
//! guard`, `overlay_state_word_corrupt` (Q15).

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const jc = @import("../meta/journal_core.zig");
const sync_notify = @import("../sync/notify.zig");
const taskset = @import("../sync/taskset.zig");
const layout = @import("layout.zig");
const block_alloc = @import("block_alloc.zig");
const enospc = @import("enospc.zig");
const cov = @import("write_coverage.zig");
const seams = @import("write_seams.zig");
const epoch_mod = @import("write_epoch.zig");
const pipeline = @import("write_pipeline.zig");
const retry = @import("write_retry.zig");

pub const Binding = seams.Binding;
pub const CoverageUnion = cov.CoverageUnion;

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// The v1 gate: LBA-aligned segments; one claim bit and one coverage bit per
/// page.
pub const OVERLAY_PAGE: u32 = seams.PAGE;
/// The shipped 4 MiB block at 4 KiB pages: 1024 bits = 16 words per face.
pub const MAX_OVERLAY_PAGES: usize = @intCast(block_alloc.CHUNK_SIZE / OVERLAY_PAGE);
pub const CLAIM_WORDS: usize = MAX_OVERLAY_PAGES / 64;
/// The write arm's settle retries forever: `min(1 ms << min(a, 6), 50 ms)`
/// — 1 ms is one device service time, 50 ms one reclaim manners tick (a
/// transient that outlives it is a device event the watchdog names).
pub const SETTLE_LADDER: retry.Ladder = .{ .base_ns = 1 * std.time.ns_per_ms, .cap_ns = 50 * std.time.ns_per_ms };
/// The read/drain venue's bounded form: 24 attempts (the rebind ladder's own
/// `MAX_REBINDS`: past it the read escalates to the serialized settle).
pub const SETTLE_RETRIES: u32 = 24;
/// ACK-early continuation: acked custody retries until it lands —
/// `min(10 ms << min(a, 7), 1 s)`; 1 s is the fence bound's floor.
pub const ACK_EARLY_LADDER: retry.Ladder = .{ .base_ns = 10 * std.time.ns_per_ms, .cap_ns = 1 * std.time.ns_per_s };
/// A settle waiting this long on an in-flight store names the leaked
/// claim loudly — never a hang (the 2026-08-13 fused-lane wedge class).
pub const SETTLE_BARK_NS: u64 = 30 * std.time.ns_per_s;

/// The registry's records (decision 0003 §AP): `max(parked_cap, ⌈pipe ÷
/// block⌉)`. An open record is write custody the write-back timer lands —
/// at most one block each (the gap seed writes the complement), the parked
/// buffer's unit and drain cost — so the parked cap is its floor; and every
/// overlay store in flight holds a record, so the write pipeline's byte cap
/// in blocks keeps the governor, not the registry, the bound on a streaming
/// writer. Past it a fresh install declines to the accumulation arm, whose
/// parked-buffer gate parks the writer.
pub fn registryCapacity(parked_cap: u64, pipe_cap_bytes: u64, block_size: u64) usize {
    return @intCast(@max(parked_cap, std.math.divCeil(u64, pipe_cap_bytes, block_size) catch unreachable));
}

comptime {
    assert(MAX_OVERLAY_PAGES == 1024 and CLAIM_WORDS == 16);
}

// ---------------------------------------------------------------------------
// Stats (§11.2 names).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var overlay_open: counters.Gauge = .{};
    /// R5 non-sheddable.
    pub var overlay_inflight_bytes: counters.Gauge = .{};
    pub var overlay_installs: counters.ShardedCounter = .{};
    pub var overlay_overwrite_installs: counters.ShardedCounter = .{};
    pub var overlay_stores: counters.ShardedCounter = .{};
    /// CQE-counted: the closure quantity (≈ user bytes on the eligible row).
    pub var overlay_store_bytes: counters.ShardedCounter = .{};
    pub var overlay_overwrite_bytes: counters.ShardedCounter = .{};
    pub var overlay_ack_early_stores: counters.ShardedCounter = .{};
    pub var overlay_ack_early_bytes: counters.ShardedCounter = .{};
    pub var overlay_ack_early_retries: counters.ShardedCounter = .{};
    pub var overlay_short_stores: counters.ShardedCounter = .{};
    pub var overlay_store_fallbacks: counters.ShardedCounter = .{};
    pub var overlay_claim_conflicts: counters.ShardedCounter = .{};
    pub var overlay_governed_stores: counters.ShardedCounter = .{};
    pub var overlay_dma_passthrough_bytes: counters.ShardedCounter = .{};
    pub var overlay_dma_pool_copy_bytes: counters.ShardedCounter = .{};
    pub var overlay_ineligible_sub_cap: counters.ShardedCounter = .{};
    pub var overlay_ineligible_shadow_bound: counters.ShardedCounter = .{};
    pub var overlay_ineligible_range_shared: counters.ShardedCounter = .{};
    /// A block the RAM table holds (a parked buffer or extent) declines:
    /// one write-side authority per block (the state screen's clause).
    pub var overlay_ineligible_ram_custody: counters.ShardedCounter = .{};
    pub var overlay_sub_cap_joins: counters.ShardedCounter = .{};
    pub var overlay_enospc_declines: counters.ShardedCounter = .{};
    /// A fresh install found every record taken (`registryCapacity`): the
    /// piece took the next arm, nothing minted.
    pub var overlay_registry_declines: counters.ShardedCounter = .{};
    pub var overlay_unreachable_declines: counters.ShardedCounter = .{};
    pub var overlay_read_serves: counters.ShardedCounter = .{};
    pub var overlay_read_serve_bytes: counters.ShardedCounter = .{};
    pub var overlay_read_gap_serves: counters.ShardedCounter = .{};
    pub var overlay_read_gap_bytes: counters.ShardedCounter = .{};
    pub var overlay_read_drains: counters.ShardedCounter = .{};
    pub var overlay_window_escalations: counters.ShardedCounter = .{};
    pub var overlay_gap_seeds: counters.ShardedCounter = .{};
    pub var overlay_gap_seed_bytes: counters.ShardedCounter = .{};
    pub var overlay_gap_seed_old_bytes: counters.ShardedCounter = .{};
    pub var overlay_gap_seed_ranged_bytes: counters.ShardedCounter = .{};
    pub var overlay_gap_seed_read_bytes: counters.ShardedCounter = .{};
    pub var overlay_publishes: counters.ShardedCounter = .{};
    pub var overlay_published_bytes: counters.ShardedCounter = .{};
    pub var overlay_epoch_feeds: counters.ShardedCounter = .{};
    /// Growth with the shadow lever ON is a bug.
    pub var overlay_feed_fallbacks: counters.ShardedCounter = .{};
    pub var overlay_supersessions: counters.ShardedCounter = .{};
    pub var overlay_superseded_by_merge: counters.ShardedCounter = .{};
    pub var overlay_superseded_dead_old_binding: counters.ShardedCounter = .{};
    pub var overlay_settle_retries: counters.ShardedCounter = .{};
    pub var overlay_teardown_waits: counters.ShardedCounter = .{};
    pub var writeback_orphan_discards_overlay: counters.ShardedCounter = .{};
    // Must-stay-0.
    pub var overlay_fence_drops: counters.Tripwire = .init("overlay_fence_drops");
    pub var overlay_unpublished_at_fsync: counters.Tripwire = .init("overlay_unpublished_at_fsync");
    pub var overlay_ack_early_lost: counters.Tripwire = .init("overlay_ack_early_lost");
    pub var overlay_teardown_wait_overlong: counters.Tripwire = .init("overlay_teardown_wait_overlong");
    pub var overlay_teardown_nonterminal: counters.Tripwire = .init("overlay_teardown_nonterminal");
    pub var overlay_install_race_under_guard: counters.Tripwire = .init("overlay_install_race_under_guard");
    pub var overlay_store_superseded_under_guard: counters.Tripwire = .init("overlay_store_superseded_under_guard");
    pub var overlay_state_word_corrupt: counters.Tripwire = .init("overlay_state_word_corrupt");
    pub var overlay_registry_full: counters.Tripwire = .init("overlay_registry_full");
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"overlay_open\":{d},\"overlay_inflight_bytes\":{d},", .{ stats.overlay_open.load(), stats.overlay_inflight_bytes.load() });
    inline for (.{ "overlay_installs", "overlay_overwrite_installs", "overlay_stores", "overlay_store_bytes", "overlay_overwrite_bytes", "overlay_ack_early_stores", "overlay_ack_early_bytes", "overlay_ack_early_retries", "overlay_short_stores", "overlay_store_fallbacks", "overlay_claim_conflicts", "overlay_governed_stores", "overlay_dma_passthrough_bytes", "overlay_dma_pool_copy_bytes", "overlay_ineligible_sub_cap", "overlay_ineligible_shadow_bound", "overlay_ineligible_range_shared", "overlay_ineligible_ram_custody", "overlay_sub_cap_joins", "overlay_enospc_declines", "overlay_registry_declines", "overlay_unreachable_declines", "overlay_read_serves", "overlay_read_serve_bytes", "overlay_read_gap_serves", "overlay_read_gap_bytes", "overlay_read_drains", "overlay_window_escalations", "overlay_gap_seeds", "overlay_gap_seed_bytes", "overlay_gap_seed_old_bytes", "overlay_gap_seed_ranged_bytes", "overlay_gap_seed_read_bytes", "overlay_publishes", "overlay_published_bytes", "overlay_epoch_feeds", "overlay_feed_fallbacks", "overlay_supersessions", "overlay_superseded_by_merge", "overlay_superseded_dead_old_binding", "overlay_settle_retries", "overlay_teardown_waits", "writeback_orphan_discards_overlay" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "overlay_fence_drops", "overlay_unpublished_at_fsync", "overlay_ack_early_lost", "overlay_teardown_wait_overlong", "overlay_teardown_nonterminal", "overlay_install_race_under_guard", "overlay_store_superseded_under_guard", "overlay_state_word_corrupt", "overlay_registry_full" }, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":{d}", .{ name, counters.tripwireCount(name) });
    }
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// §3.2 The pure state core.
// ---------------------------------------------------------------------------

pub const State = enum(u8) {
    open = 0,
    frozen = 1,
    published = 2,
    superseded = 3,
    fence_dropped = 4,
    fed = 5,

    pub fn isTerminal(self: State) bool {
        return switch (self) {
            .open, .frozen => false,
            .published, .superseded, .fence_dropped, .fed => true,
        };
    }
};

/// Q15: an impossible word is a tripwire and reads as the free-NOTHING
/// terminal (never a disposition the design admits).
fn decodeState(w: u8) State {
    return switch (w) {
        0 => .open,
        1 => .frozen,
        2 => .published,
        3 => .superseded,
        4 => .fence_dropped,
        5 => .fed,
        else => blk: {
            stats.overlay_state_word_corrupt.fire("overlay state word outside the enum");
            break :blk .fence_dropped;
        },
    };
}

pub const StoreRefusal = error{ OutOfRange, NotOpen, Overlap, RunTableFull };

pub const Ticket = struct {
    first_page: u32,
    pages: u32,
    generation: u64,
};

pub const CompleteVerdict = union(enum) {
    /// The record is terminal: publish nothing (the claim is released).
    superseded,
    covered: struct { coverage_complete: bool },
    not_covered,
};

pub const Core = struct {
    state: Atomic(u8) = .init(@backingInt(State.open)),
    /// Law 6: bumped by EVERY accepted segment; readers revalidate on EQUALITY.
    generation: Atomic(u64) = .init(0),
    /// Stores submitted, CQE unseen — law 9's wait set.
    inflight: Atomic(u32) = .init(0),
    /// §3.3 range claims: one bit per page, granted at submit, released
    /// ONLY at the CQE.
    claims: [CLAIM_WORDS]Atomic(u64) = @splat(.init(0)),
    /// The lock-free published coverage face — set only by a full-success CQE.
    covered: [CLAIM_WORDS]Atomic(u64) = @splat(.init(0)),
    /// The coverage LAW (shared with RAM accumulation); every access under
    /// `union_lock` but the settle's, which reads it Frozen with nothing in
    /// flight (stores land holding no (3): their CQEs meet only here).
    union_: CoverageUnion = .empty,
    union_lock: jc.SpinLock = .{},
    block_len: u32,
    pages: u32,
    /// Registry-monotone identity (offsets are reused; births are not).
    birth: u64,

    pub fn init(block_len: u32, birth: u64) Core {
        assert(block_len % OVERLAY_PAGE == 0 and block_len / OVERLAY_PAGE <= MAX_OVERLAY_PAGES);
        return .{ .block_len = block_len, .pages = block_len / OVERLAY_PAGE, .birth = birth };
    }

    pub fn stateNow(self: *const Core) State {
        return decodeState(self.state.load(.acquire));
    }

    fn wordBit(page: u32) struct { w: usize, m: u64 } {
        return .{ .w = page / 64, .m = @as(u64, 1) << @intCast(page % 64) };
    }

    fn anyBit(words: *const [CLAIM_WORDS]Atomic(u64), first: u32, n: u32) bool {
        var p = first;
        while (p < first + n) : (p += 1) {
            const wb = wordBit(p);
            if (words[wb.w].load(.seq_cst) & wb.m != 0) return true;
        }
        return false;
    }

    fn allBits(words: *const [CLAIM_WORDS]Atomic(u64), first: u32, n: u32) bool {
        var p = first;
        while (p < first + n) : (p += 1) {
            const wb = wordBit(p);
            if (words[wb.w].load(.seq_cst) & wb.m == 0) return false;
        }
        return true;
    }

    fn setBits(words: *[CLAIM_WORDS]Atomic(u64), first: u32, n: u32) void {
        var p = first;
        while (p < first + n) : (p += 1) {
            const wb = wordBit(p);
            _ = words[wb.w].fetchOr(wb.m, .seq_cst);
        }
    }

    fn clearBits(words: *[CLAIM_WORDS]Atomic(u64), first: u32, n: u32) void {
        var p = first;
        while (p < first + n) : (p += 1) {
            const wb = wordBit(p);
            _ = words[wb.w].fetchAnd(~wb.m, .seq_cst);
        }
    }

    /// Under (3): range check → claim (an in-flight overlap refuses — law
    /// 6's mandatory clause) → `inflight += 1` (SeqCst) → re-check `state ==
    /// Open` (SeqCst load: the Dekker pair's second access) else back out →
    /// `generation += 1`.
    pub fn beginStore(self: *Core, first_page: u32, pages: u32) StoreRefusal!Ticket {
        if (pages == 0 or first_page + pages > self.pages) return error.OutOfRange;
        if (self.stateNow() != .open) return error.NotOpen;
        if (anyBit(&self.claims, first_page, pages)) return error.Overlap;
        // A pending claim is a run-to-be: the union must have room for it
        // and for every other in-flight claim (conservative: inflight runs
        // are counted as disjoint). `inflight` is read under the lock: a
        // CQE records its run before it decrements, so either side is seen.
        self.union_lock.lock();
        const room = self.union_.hasHeadroom(@as(usize, self.inflight.load(.monotonic)) + 1);
        self.union_lock.unlock();
        if (!room) return error.RunTableFull;
        setBits(&self.claims, first_page, pages);
        // Dekker, writer side: publish inflight, THEN read state (both SeqCst).
        _ = self.inflight.fetchAdd(1, .seq_cst);
        if (decodeState(self.state.load(.seq_cst)) != .open) {
            _ = self.inflight.fetchSub(1, .seq_cst);
            clearBits(&self.claims, first_page, pages);
            return error.NotOpen;
        }
        const g = self.generation.fetchAdd(1, .seq_cst) + 1;
        return .{ .first_page = first_page, .pages = pages, .generation = g };
    }

    /// At the CQE (no block guard: disjoint stores land concurrently, §3.3):
    /// a terminal record ⇒ superseded; success ⇒ the union records the run
    /// under `union_lock`, then the covered bits publish (SeqCst
    /// `fetch_or`); failure ⇒ nothing covered. The claim releases ONLY here;
    /// `inflight −= 1` last (the freezer's drain observes the final coverage).
    pub fn completeStore(self: *Core, t: Ticket, success: bool) CompleteVerdict {
        defer _ = self.inflight.fetchSub(1, .seq_cst);
        defer clearBits(&self.claims, t.first_page, t.pages);
        if (self.stateNow().isTerminal()) return .superseded;
        if (!success) return .not_covered;
        const start = t.first_page * OVERLAY_PAGE;
        const end = (t.first_page + t.pages) * OVERLAY_PAGE;
        self.union_lock.lock();
        // `beginStore` reserved the headroom; a refusal here is unreachable.
        const v = self.union_.record(start, end, self.block_len) catch unreachable;
        const full = v.completed or self.union_.isFull(self.block_len);
        self.union_lock.unlock();
        setBits(&self.covered, t.first_page, t.pages);
        // Frozen while this claim still holds the record live: past the
        // decrement a settle may retire it and the slot host a new record.
        if (full) _ = self.freeze();
        return .{ .covered = .{ .coverage_complete = full } };
    }

    /// Claims first (an in-flight range is never served, even a re-write of
    /// a covered range), then every covered bit.
    pub fn coveredProbe(self: *const Core, first_page: u32, pages: u32) bool {
        if (anyBit(&self.claims, first_page, pages)) return false;
        return allBits(&self.covered, first_page, pages);
    }

    pub fn rangeInflight(self: *const Core, first_page: u32, pages: u32) bool {
        return anyBit(&self.claims, first_page, pages);
    }

    pub fn readBegin(self: *const Core) u64 {
        return self.generation.load(.seq_cst);
    }

    /// EQUALITY, not ≥.
    pub fn readValid(self: *const Core, snap: u64) bool {
        return self.generation.load(.seq_cst) == snap;
    }

    /// Dekker, freezer side: CAS `Open → Frozen` (SeqCst), then the caller's
    /// `inflightEmpty` load (SeqCst) is the pair's fourth access.
    pub fn freeze(self: *Core) bool {
        return self.state.cmpxchgStrong(@backingInt(State.open), @backingInt(State.frozen), .seq_cst, .seq_cst) == null;
    }

    fn fromNonTerminal(self: *Core, to: State) bool {
        var cur = self.state.load(.seq_cst);
        while (true) {
            if (decodeState(cur).isTerminal()) return false;
            cur = self.state.cmpxchgWeak(cur, @backingInt(to), .seq_cst, .seq_cst) orelse return true;
        }
    }

    pub fn supersede(self: *Core) bool {
        return self.fromNonTerminal(.superseded);
    }

    pub fn fenceDrop(self: *Core) bool {
        return self.fromNonTerminal(.fence_dropped);
    }

    pub fn markPublished(self: *Core) bool {
        return self.state.cmpxchgStrong(@backingInt(State.frozen), @backingInt(State.published), .seq_cst, .seq_cst) == null;
    }

    pub fn markFed(self: *Core) bool {
        return self.state.cmpxchgStrong(@backingInt(State.frozen), @backingInt(State.fed), .seq_cst, .seq_cst) == null;
    }

    /// SeqCst load: after the SeqCst freeze CAS this is the fence the pair
    /// needs (a writer that incremented before our CAS is visible here; one
    /// that incremented after sees Frozen and backs out).
    pub fn inflightEmpty(self: *const Core) bool {
        return self.inflight.load(.seq_cst) == 0;
    }

    pub fn rollbackAdmissible(self: *const Core) bool {
        return self.stateNow().isTerminal() and self.inflightEmpty();
    }
};

// ---------------------------------------------------------------------------
// §3.1 The record and the registry.
// ---------------------------------------------------------------------------

pub const Key = struct { ino: u64, block: u32 };

pub const Record = struct {
    key: Key,
    core: Core,
    /// `null` ⇔ fresh/hole (gaps are zeros — law 5); IMMUTABLE.
    old_binding: ?Binding,
    /// The unpublished destination.
    dest: Binding,
    dest_idx: u64,
    alloc: *block_alloc.BlockAllocator,
    ordinal: u32,
    /// fsck C2/C3 VISIBILITY ONLY — dropping it frees nothing.
    fsck_guard: ?block_alloc.InflightAllocGuard,
    /// Law 9's rollback owner; disposition per terminal state.
    mint: ?block_alloc.MintedBlockGuard,
    /// The ino's fencing generation at install; publication re-presents
    /// the CURRENT one.
    fence_token: u64,
    /// Waiters on the in-flight set changing (the settle's and the read's
    /// event wait — never a yield spin, Q4).
    inflight_change: sync_notify.Notify = .init,
    /// ACK-early stores acked but not yet landed.
    acked_pending: Atomic(u32) = .init(0),
    /// The install's monotonic stamp: an ino drain owes every record
    /// opened BEFORE it began; one opened during the drain is the next
    /// fsync's (never a stranded record, never an endless drain under a
    /// steady writer).
    opened_ns: u64 = 0,
    in_use: bool = false,
};

/// Fixed pool + an index under a spin lock (records are rare: one per block
/// in flight); `open_count` is the one relaxed load every mount with no
/// live overlay pays.
pub const Registry = struct {
    gpa: Allocator,
    records: []Record,
    free: std.ArrayList(u32) = .empty,
    index: std.AutoHashMapUnmanaged(Key, *Record) = .empty,
    lock: jc.SpinLock = .{},
    open_count: Atomic(u32) = .init(0),
    births: Atomic(u64) = .init(0),
    /// Free records promised to installs whose mint runs outside the lock
    /// (`reserve`): `reserved ≤ free.items.len`, so an install holding a
    /// reservation always finds its record.
    reserved: u32 = 0,

    pub fn init(gpa: Allocator, capacity: usize) Allocator.Error!Registry {
        const recs = try gpa.alloc(Record, capacity);
        errdefer gpa.free(recs);
        var free = try std.ArrayList(u32).initCapacity(gpa, capacity);
        errdefer free.deinit(gpa);
        var i: u32 = @intCast(capacity);
        while (i > 0) : (i -= 1) free.appendAssumeCapacity(i - 1);
        var self: Registry = .{ .gpa = gpa, .records = recs, .free = free };
        try self.index.ensureTotalCapacity(gpa, @intCast(capacity));
        return self;
    }

    pub fn deinit(self: *Registry) void {
        self.index.deinit(self.gpa);
        self.free.deinit(self.gpa);
        self.gpa.free(self.records);
        self.* = undefined;
    }

    pub fn anyOpenFast(self: *const Registry) bool {
        return self.open_count.load(.monotonic) != 0;
    }

    pub fn get(self: *Registry, k: Key) ?*Record {
        self.lock.lock();
        defer self.lock.unlock();
        return self.index.get(k);
    }

    /// Promise one free record to an install about to mint; false = every
    /// record is live or promised.
    fn reserve(self: *Registry) bool {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.free.items.len <= self.reserved) return false;
        self.reserved += 1;
        return true;
    }

    /// Return a promise the install never used.
    fn unreserve(self: *Registry) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.reserved -= 1;
    }

    /// Install a fresh record (the caller filled everything but `in_use`),
    /// spending the caller's reservation whatever the outcome.
    /// `error.Exists` = a race the caller's guard should have excluded;
    /// `error.Full` = the reservation accounting broke.
    fn install(self: *Registry, k: Key, fill: *const Record) error{ Exists, Full }!*Record {
        self.lock.lock();
        defer self.lock.unlock();
        self.reserved -= 1;
        if (self.index.contains(k)) return error.Exists;
        const idx = self.free.pop() orelse return error.Full;
        const r = &self.records[idx];
        r.* = fill.*;
        r.in_use = true;
        r.inflight_change = .init;
        r.opened_ns = clock.monotonicNs();
        self.index.putAssumeCapacity(k, r);
        _ = self.open_count.fetchAdd(1, .monotonic);
        stats.overlay_open.add(1);
        return r;
    }

    fn retire(self: *Registry, r: *Record) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.index.remove(r.key);
        r.in_use = false;
        const idx: u32 = @intCast((@intFromPtr(r) - @intFromPtr(self.records.ptr)) / @sizeOf(Record));
        self.free.appendAssumeCapacity(idx);
        _ = self.open_count.fetchSub(1, .monotonic);
        stats.overlay_open.sub(1);
    }

    /// The syncfs census: every live record's ino appended to `out`
    /// (duplicates are the caller's). The room is the registry's fixed
    /// capacity, reserved OUTSIDE the lock.
    pub fn censusInos(self: *Registry, gpa: Allocator, out: *std.ArrayList(u64)) Allocator.Error!void {
        try out.ensureUnusedCapacity(gpa, self.records.len);
        self.lock.lock();
        defer self.lock.unlock();
        var it = self.index.keyIterator();
        while (it.next()) |k| out.appendAssumeCapacity(k.ino);
    }

    /// The lowest block `ino` holds an open record on (the ordered law's
    /// floor, `WritePath.custodyFloor`).
    pub fn lowestBlockOf(self: *Registry, ino: u64) ?u32 {
        if (!self.anyOpenFast()) return null;
        self.lock.lock();
        defer self.lock.unlock();
        var low: ?u32 = null;
        var it = self.index.keyIterator();
        while (it.next()) |k| if (k.ino == ino and (low == null or k.block < low.?)) {
            low = k.block;
        };
        return low;
    }

    /// Whether `ino` holds a live record — the no-open FLUSH's try-only
    /// probe (decision 0003 §AF); O(1) with nothing open (the install's
    /// increment precedes its ACK, the probe follows that ACK through the
    /// open table's stripe lock), else one scan under the spin lock.
    pub fn holdsIno(self: *Registry, ino: u64) bool {
        if (!self.anyOpenFast()) return false;
        self.lock.lock();
        defer self.lock.unlock();
        var it = self.index.keyIterator();
        while (it.next()) |k| if (k.ino == ino) return true;
        return false;
    }

    /// Every live record of `ino` (for fsync / release / truncate drains).
    pub fn blocksOf(self: *Registry, ino: u64, out: []u32) []u32 {
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
};

// ---------------------------------------------------------------------------
// The overlay: config, deps, store, settle, serve.
// ---------------------------------------------------------------------------

pub const Config = struct {
    block_size: u64,
    /// `SQUEEZEFS_DEVICE_OVERLAY` (off = the accumulation control arm).
    enabled: bool = true,
    /// `SQUEEZEFS_OVERLAY_OVERWRITE`.
    overwrite: bool = true,
    /// The W1 ceiling = the overlay length floor (`SQUEEZEFS_PATCH_MAX_BYTES`;
    /// 0 makes every length overlay-eligible).
    patch_max: u64,
    /// `SQUEEZEFS_GAP_SEED_RANGED`.
    gap_seed_ranged: bool = true,
    /// `SQUEEZEFS_OVERLAY_CLOSE_BARRIER` (OQ-5; the OW-8 posture).
    close_barrier: bool = false,
    /// `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR`.
    depth_governor: bool = true,
    /// `SQUEEZEFS_ZC_ACK_EARLY` / `_ODIRECT`.
    ack_early: bool = true,
    ack_early_odirect: bool = false,
};

pub const Deps = struct {
    device: seams.DeviceSeam,
    map: seams.MapSeam,
    free: seams.FreeSeam,
    purge: seams.PurgeSeam,
    vols: *const seams.Volumes,
    picker: enospc.Picker,
    valve: ?enospc.Valve = null,
    pressure_hook: ?enospc.PressureSupplyHook = null,
    epochs: *epoch_mod.Epochs,
    stripes: *layout.MetaStripes,
    governor: ?*pipeline.Governor = null,
    /// Rung 6: `overlay_range_shared` (a shared custody range declines).
    range_shared: ?*const fn (ino: u64, block: u32) bool = null,
    /// The state screen's "no RAM/extent custody" clause: a block the
    /// accumulation table holds (a parked buffer, an extent — its upload
    /// in flight or not) declines, so one block never has two write-side
    /// authorities each publishing its complement over the other's bytes
    /// (fstests generic/551: a 1 MiB run of zeros in the verify).
    ram_custody: ?*const fn (ctx: ?*anyopaque, ino: u64, block: u32) bool = null,
    ram_custody_ctx: ?*anyopaque = null,
};

/// The payload's stability class at mint (the §3.4 stability law).
pub const PayloadClass = enum { page_cache, odirect };

pub const SlotVehicle = struct {
    src: seams.HeldSource,
    class: PayloadClass,
    /// The transport's retention probe is present (COMMIT_RETAIN).
    commit_retain: bool,
    ctx: ?*anyopaque = null,
    /// The fallback bytes (one bridge, memoized by the transport).
    extract: ?*const fn (ctx: ?*anyopaque, io: Io) anyerror![]const u8 = null,
    /// `z.release_payload()` exactly once, last (the ACK-early continuation).
    release: ?*const fn (ctx: ?*anyopaque) void = null,
};

/// Q8: the store takes a vehicle from the request; tests construct the
/// bytes vehicle directly.
pub const Vehicle = union(enum) {
    slot: SlotVehicle,
    bytes: []const u8,

    pub fn len(self: Vehicle) u32 {
        return switch (self) {
            .slot => |s| s.src.len,
            .bytes => |b| @intCast(b.len),
        };
    }
};

pub const StoreRequest = struct {
    ino: u64,
    block: u32,
    /// Byte offset within the block (page-aligned, checked).
    rel: u32,
    vehicle: Vehicle,
    file: seams.FileView,
    /// The map's current binding of the block (the overwrite shape); the
    /// install re-derives it under (3.5).
    overwrite: bool,
    /// A depth-governor permit for `vehicle.len()` the caller admitted
    /// HOLDING NO BLOCK GUARD after a `.governor_full` answer; the store
    /// spends it (its CQE completes it) or returns it unspent on any
    /// decline. Null: the store asks the governor itself, without parking.
    permit: ?pipeline.Permit = null,
};

pub const StoreOutcome = union(enum) {
    stored: struct {
        /// The block's coverage completed on this CQE: the caller drops its
        /// guard and drains (publishes) the block.
        coverage_complete: bool,
        /// ACK-early: the DMA lands on a detached continuation.
        acked_early: bool,
    },
    /// The range is claimed under the caller's (3); the caller DROPS (3)
    /// and `land`s it: the claim, not the guard, excludes an overlapping
    /// DMA (§3.3), so a sequential stream's writes into one block land
    /// side by side (rung-10-seqwrite: the guard held across the DMA
    /// serialized 1M seq write ×4 four writes per block, 54 % of the
    /// writes parked 7 ms).
    claimed: Claimed,
    declined,
    /// The pipe is full and the store will not park under the caller's (3):
    /// the in-flight permits belong to uploads that re-take that guard to
    /// publish (generic/551: four writers parked here holding the file's
    /// four block locks, ten uploads parked on those locks holding the
    /// whole depth, 53 writes in flight for 30 min). The caller drops its
    /// guard, parks at the governor holding no lock, and retries the ladder
    /// with the permit in `StoreRequest.permit`.
    governor_full,
};

/// A granted claim owed its DMA and CQE (`Overlay.land`). The record stays
/// live: every teardown awaits `inflight`, which this claim holds.
pub const Claimed = struct {
    rec: *Record,
    ticket: Ticket,
    permit: ?pipeline.Permit,
    rel: u32,
    vehicle: Vehicle,
    overwrite: bool,
};

pub const StoreError = error{ InputOutput, VolumeFailed, WriterGuardFenced, OutOfMemory, WorkerShutdown, QueueFull, NoSpaceLeft, ReadOnlyFileSystem };

pub const SettleOutcome = union(enum) {
    published,
    /// FED into the rewrite epoch; `coverage_complete` is the close-owed
    /// HINT (latency/economy, never correctness — the idle sweep and the
    /// next fsync are the backstops).
    fed: struct { coverage_complete: bool },
    superseded,
    already_terminal,
};

pub const SettleError = error{ Transient, WriterGuardFenced, OutOfMemory };

pub const Overlay = struct {
    gpa: Allocator,
    cfg: Config,
    deps: Deps,
    registry: Registry,
    tasks: taskset.TaskSet = .init,
    /// Test seam: `SQUEEZEFS_TEST_OVERLAY_SETTLE_TRANSIENTS` — inject N
    /// transient settle failures after the freeze, before device work.
    settle_transients: Atomic(u32) = .init(0),

    pub fn init(gpa: Allocator, cfg: Config, deps: Deps, capacity: usize) Allocator.Error!Overlay {
        return .{ .gpa = gpa, .cfg = cfg, .deps = deps, .registry = try Registry.init(gpa, capacity) };
    }

    /// Teardown: records still live are the next mount's census (their
    /// mints are disarmed, never freed here); the accounting is released.
    pub fn deinit(self: *Overlay, io: Io) void {
        self.tasks.deinit(io);
        for (self.registry.records) |*r| {
            if (!r.in_use) continue;
            if (r.mint) |*m| m.disarm();
            if (r.fsck_guard) |*g| g.deinit();
            self.registry.retire(r);
        }
        self.registry.deinit();
        self.* = undefined;
    }

    pub fn anyOpenFast(self: *const Overlay) bool {
        return self.registry.anyOpenFast();
    }

    /// `overlay_length_eligible`: `cap == 0 ∨ len > patch_max` (finding 47:
    /// six 4 KiB writes minted six CoW dests and paid six whole-block seeds).
    pub fn lengthEligible(self: *const Overlay, len: u64) bool {
        return self.cfg.patch_max == 0 or len > self.cfg.patch_max;
    }

    /// The write-wide shape (§1.2 step 6): the lever, a non-empty payload,
    /// a passthrough volume (a frame cannot be stored by range).
    pub fn shapeEligible(self: *const Overlay, len: u64, passthrough: bool) bool {
        return self.cfg.enabled and len != 0 and passthrough;
    }

    /// The per-piece shape (§1.3 row B): page-aligned start and length.
    pub fn pieceEligible(rel: u64, len: u64) bool {
        return rel % OVERLAY_PAGE == 0 and len % OVERLAY_PAGE == 0 and len != 0;
    }

    // ---- §3.4 install / store / CQE ----

    /// Arm B, landed inline: for a caller holding no block guard (tests).
    pub fn store(self: *Overlay, io: Io, req: StoreRequest) StoreError!StoreOutcome {
        return switch (try self.claim(io, req)) {
            .claimed => |c| self.land(io, c),
            else => |o| o,
        };
    }

    /// Arm B under (3). `.claimed` is the caller's to `land` after it
    /// drops (3); `.declined` falls through to the next arm; an error
    /// fails THIS write loud (nothing acked, nothing parked, nothing lost).
    pub fn claim(self: *Overlay, io: Io, req: StoreRequest) StoreError!StoreOutcome {
        // The permit is UNSPENT in `pre` until the claim lands (step 5):
        // every earlier exit returns it, the caller's pre-admission too.
        var pre = req.permit;
        defer if (pre) |p| if (self.deps.governor) |g| g.returnUnused(io, p);
        const len: u64 = req.vehicle.len();
        if (!self.cfg.enabled or !pieceEligible(req.rel, len)) return .declined;
        const key: Key = .{ .ino = req.ino, .block = req.block };
        const existing = self.registry.get(key);
        if (existing) |r| if (r.core.stateNow() != .open) {
            // Steal the detached publisher's work under OUR guard (one
            // attempt: a transient leaves it FROZEN for the one-authority
            // screen's guard-dropping ladder), then decline.
            _ = self.settleOnceLocked(io, r, false);
            return .declined;
        };
        // 1. Length floor: below it the piece declines unless it joins an
        //    Open record (the tail piece of a boundary-straddling write on a
        //    misaligned writeback grid).
        if (!self.lengthEligible(len)) {
            if (existing == null) {
                stats.overlay_ineligible_sub_cap.inc();
                return .declined;
            }
            stats.overlay_sub_cap_joins.inc();
        }
        // 2. State screen.
        if (req.file.kind != .striped or !req.file.map_resident) return .declined;
        if (req.overwrite and !self.cfg.overwrite) return .declined;
        if (self.deps.epochs.bindsBlock(req.ino, req.block)) {
            stats.overlay_ineligible_shadow_bound.inc();
            return .declined;
        }
        if (self.deps.range_shared) |rs| if (rs(req.ino, req.block)) {
            stats.overlay_ineligible_range_shared.inc();
            return .declined;
        };
        if (self.deps.ram_custody) |rc| if (rc(self.deps.ram_custody_ctx, req.ino, req.block)) {
            stats.overlay_ineligible_ram_custody.inc();
            return .declined;
        };
        // 4. The governor permit — BEFORE a fresh install (a full pipe
        //    leaves no zero-coverage Open record behind for a sibling's
        //    one-authority settle to seed whole) and NEVER a park under the
        //    caller's (3): a full pipe answers `.governor_full` and the
        //    caller parks holding no lock (the reference parked here, after
        //    its install; decision 0002).
        if (self.cfg.depth_governor) if (self.deps.governor) |g| {
            if (pre == null) pre = g.tryAdmit(len) orelse return .governor_full;
        };
        // 3. Registry: join or install.
        const rec = existing orelse (try self.installRecord(io, req)) orelse return .declined;
        // 5. Claim + accept: the permit is spent here (its CQE completes it).
        const first_page: u32 = req.rel / OVERLAY_PAGE;
        const pages: u32 = @intCast(len / OVERLAY_PAGE);
        const ticket = rec.core.beginStore(first_page, pages) catch {
            stats.overlay_claim_conflicts.inc();
            _ = self.settleOnceLocked(io, rec, false);
            return .declined;
        };
        const permit = pre;
        pre = null;
        if (permit != null) stats.overlay_governed_stores.inc();
        stats.overlay_inflight_bytes.add(len);
        // 6. ACK-early: a sound (page-cache) held slot under retention
        //    lands on a detached continuation; the O_DIRECT class only
        //    under its opt-in, as a SNAPSHOT (the 2026-08-09 aliasing).
        if (req.vehicle == .slot) early: {
            const sv = req.vehicle.slot;
            const early = self.cfg.ack_early and sv.commit_retain and (sv.class == .page_cache or (self.cfg.ack_early_odirect and sv.extract != null));
            if (early) {
                _ = rec.acked_pending.fetchAdd(1, .acq_rel);
                stats.overlay_ack_early_stores.inc();
                stats.overlay_ack_early_bytes.add(len);
                const snapshot: ?[]const u8 = if (sv.class == .odirect) blk: {
                    const b = sv.extract.?(sv.ctx, io) catch {
                        _ = rec.acked_pending.fetchSub(1, .acq_rel);
                        _ = self.finishStore(io, rec, ticket, permit, len, req.overwrite, false);
                        return error.InputOutput;
                    };
                    break :blk b;
                } else null;
                self.tasks.spawn(io, ackEarlyContinuation, .{ self, io, rec, ticket, permit, sv, snapshot, len, req.overwrite }) catch {
                    // No unit of concurrency: the caller lands it (still
                    // acked only after the CQE — the sound fallback).
                    _ = rec.acked_pending.fetchSub(1, .acq_rel);
                    break :early;
                };
                return .{ .stored = .{ .coverage_complete = false, .acked_early = true } };
            }
        }
        return .{ .claimed = .{ .rec = rec, .ticket = ticket, .permit = permit, .rel = req.rel, .vehicle = req.vehicle, .overwrite = req.overwrite } };
    }

    /// The claimed store's DMA and CQE, holding NO block guard (a settle
    /// under (3) awaits this CQE: landing under a guard would deadlock it).
    pub fn land(self: *Overlay, io: Io, c: Claimed) StoreError!StoreOutcome {
        const len: u64 = c.vehicle.len();
        self.dmaVehicle(io, c.rec, c.rec.dest.offset + c.rel, c.vehicle) catch |e| {
            _ = self.finishStore(io, c.rec, c.ticket, c.permit, len, c.overwrite, false);
            return e;
        };
        return self.finishStore(io, c.rec, c.ticket, c.permit, len, c.overwrite, true);
    }

    /// The two vehicles, one contract: the slot's direct leg (`WRITE_FIXED`),
    /// a short/failed direct store falls to the pooled vehicle (the same
    /// window's pooled DMA overwrites it — self-healing); pooled bytes pass
    /// through when 4 KiB-aligned, else ONE pool copy (the device bounces).
    fn dmaVehicle(self: *Overlay, io: Io, rec: *Record, dest_off: u64, v: Vehicle) StoreError!void {
        switch (v) {
            .slot => |sv| {
                if (self.deps.device.writeFixed(rec.dest.vol_tag, dest_off, sv.src)) |_| {
                    return;
                } else |_| {
                    stats.overlay_short_stores.inc();
                    const ex = sv.extract orelse return error.InputOutput;
                    const bytes = ex(sv.ctx, io) catch return error.InputOutput;
                    stats.overlay_store_fallbacks.inc();
                    return self.dmaBytes(rec, dest_off, bytes);
                }
            },
            .bytes => |b| return self.dmaBytes(rec, dest_off, b),
        }
    }

    fn dmaBytes(self: *Overlay, rec: *Record, dest_off: u64, b: []const u8) StoreError!void {
        if (@intFromPtr(b.ptr) % OVERLAY_PAGE == 0 and b.len % OVERLAY_PAGE == 0) {
            stats.overlay_dma_passthrough_bytes.add(b.len);
        } else {
            stats.overlay_dma_pool_copy_bytes.add(b.len);
        }
        _ = self.deps.device.write(rec.dest.vol_tag, dest_off, b) catch |e| return mapDma(e);
    }

    fn mapDma(e: seams.DmaError) StoreError {
        return switch (e) {
            error.InputOutput => error.InputOutput,
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.QueueFull => error.QueueFull,
            error.WorkerShutdown => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    /// The CQE: the governor's permit release FIRST (never waits on a block
    /// guard), then `complete_store`, the gauge, the counters, the wake.
    fn finishStore(self: *Overlay, io: Io, rec: *Record, ticket: Ticket, permit: ?pipeline.Permit, len: u64, overwrite: bool, success: bool) StoreOutcome {
        if (permit) |p| if (self.deps.governor) |g| g.complete(io, p, rec.ordinal, clock.monotonicNs() -| p.admitted_ns);
        const verdict = rec.core.completeStore(ticket, success);
        stats.overlay_inflight_bytes.sub(len);
        rec.inflight_change.notifyWaiters(io);
        if (!success) return .{ .stored = .{ .coverage_complete = false, .acked_early = false } };
        stats.overlay_stores.inc();
        stats.overlay_store_bytes.add(len);
        if (overwrite) stats.overlay_overwrite_bytes.add(len);
        return switch (verdict) {
            .covered => |c| .{ .stored = .{ .coverage_complete = c.coverage_complete, .acked_early = false } },
            .superseded => blk: {
                // Every terminal transition awaits `inflight`, which this store held.
                stats.overlay_store_superseded_under_guard.fire("a store completed on a record that went terminal while it was in flight");
                break :blk .{ .stored = .{ .coverage_complete = false, .acked_early = false } };
            },
            .not_covered => .{ .stored = .{ .coverage_complete = false, .acked_early = false } },
        };
    }

    /// The ACK-early continuation: retry-forever on the ladder; the fence
    /// latch is the ONE legal drop (`overlay_fence_drops`); a dead ring
    /// falls back to the pooled vehicle with the ACK-time bytes, else
    /// `overlay_ack_early_lost` loud; coverage publishes at the REAL CQE;
    /// the payload is released exactly once, last.
    fn ackEarlyContinuation(self: *Overlay, io: Io, rec: *Record, ticket: Ticket, permit: ?pipeline.Permit, sv: SlotVehicle, snapshot: ?[]const u8, len: u64, overwrite: bool) Io.Cancelable!void {
        defer if (sv.release) |r| r(sv.ctx);
        defer _ = rec.acked_pending.fetchSub(1, .acq_rel);
        var ladder = ACK_EARLY_LADDER;
        const dest_off = rec.dest.offset + ticket.first_page * OVERLAY_PAGE;
        while (true) {
            const attempt: StoreError!void = if (snapshot) |b| self.dmaBytes(rec, dest_off, b) else self.dmaVehicle(io, rec, dest_off, .{ .slot = sv });
            if (attempt) |_| {
                _ = self.finishStore(io, rec, ticket, permit, len, overwrite, true);
                return;
            } else |e| switch (e) {
                error.WriterGuardFenced, error.VolumeFailed => {
                    stats.overlay_fence_drops.fire("an acked store met the fence before landing");
                    _ = self.finishStore(io, rec, ticket, permit, len, overwrite, false);
                    return;
                },
                error.WorkerShutdown => {
                    // The ring died: the ACK-time bytes through the pooled vehicle.
                    if (snapshot == null and sv.extract == null) {
                        stats.overlay_ack_early_lost.fire("acked bytes unreachable after the ring died");
                        _ = self.finishStore(io, rec, ticket, permit, len, overwrite, false);
                        return;
                    }
                },
                else => {},
            }
            stats.overlay_ack_early_retries.inc();
            const step = ladder.next();
            if (ladder.shouldBark()) log.warn("overlay ino {d} block {d}: acked store not landed after {d} attempts — retrying (acked custody is never dropped)", .{ rec.key.ino, rec.key.block, ladder.attempts() });
            try io.sleep(.{ .nanoseconds = @intCast(step.backoff_ns) }, .awake);
        }
    }

    /// INSTALL: `allocate_placed_block` (reserve-before-DMA; `StorageFull`
    /// declines to accumulation — KD-B4-8 both shapes; the fresh mint never
    /// waits for supply: under (3) and with the accumulation arm to fall
    /// to, it YIELDS scarce supply to the flushes that return RAM), fsck
    /// guard + mint owner, the slot vehicle needs the direct leg, the
    /// OVERWRITE capture in ONE (3.5) section with the install (§5.1). The
    /// record is reserved BEFORE the mint: a full registry declines with
    /// nothing minted (`overlay_registry_declines`).
    fn installRecord(self: *Overlay, io: Io, req: StoreRequest) StoreError!?*Record {
        if (!self.registry.reserve()) {
            stats.overlay_registry_declines.inc();
            return null;
        }
        var promised = true;
        defer if (promised) self.registry.unreserve();
        const pm = enospc.allocatePlacedMint(self.deps.picker, .{ .ino = req.ino, .block = req.block }, io, self.deps.valve, self.deps.pressure_hook, true, .none) catch |e| switch (e) {
            error.StorageFull => {
                stats.overlay_enospc_declines.inc();
                return null;
            },
            error.PlaneRefused, error.NoHealthyBackends => {
                stats.overlay_unreachable_declines.inc();
                return null;
            },
            error.ReadOnly => return error.ReadOnlyFileSystem,
            error.NotFree => return error.InputOutput,
        };
        var mint = block_alloc.MintedBlockGuard.init(pm.alloc, pm.minted);
        errdefer mint.abandon();
        const vol = self.deps.vols.byTag(pm.alloc.volTag()) orelse return error.VolumeFailed;
        // The stored image IS the payload: a transformed destination (or a
        // transformed old image to seed gaps from) cannot be overlaid.
        if (!vol.passthrough or (req.vehicle == .slot and !self.deps.device.canWriteFixed())) {
            mint.abandon();
            return null;
        }
        var fill: Record = .{
            .key = .{ .ino = req.ino, .block = req.block },
            .core = Core.init(@intCast(self.cfg.block_size), self.registry.births.fetchAdd(1, .monotonic) + 1),
            .old_binding = null,
            .dest = Binding.wholeBlock(pm.alloc.volTag(), pm.minted.offset, pm.minted.stamp),
            .dest_idx = pm.minted.idx,
            .alloc = pm.alloc,
            .ordinal = vol.ordinal,
            .fsck_guard = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx),
            .mint = mint,
            .fence_token = self.deps.map.currentFencing(req.ino),
        };
        if (req.overwrite) {
            // ONE (3.5) section: capture the old binding ∧ re-check the
            // epoch ∧ install — a capture outside the section races the
            // epoch's feed (hazard 1).
            const g = self.deps.stripes.acquire(io, req.ino);
            defer g.release(io);
            const old = self.deps.map.lookup(req.ino, req.block) orelse {
                fill.fsck_guard.?.deinit();
                mint.abandon();
                return null;
            };
            const old_vol = self.deps.vols.byTag(old.vol_tag);
            if (old_vol == null or !old_vol.?.passthrough or !old.whole_block or self.deps.epochs.bindsBlock(req.ino, req.block)) {
                stats.overlay_ineligible_shadow_bound.inc();
                fill.fsck_guard.?.deinit();
                mint.abandon();
                return null;
            }
            fill.old_binding = old;
            promised = false;
            const r = self.registry.install(fill.key, &fill) catch |e| return self.installFailed(e, &fill, &mint);
            stats.overlay_installs.inc();
            stats.overlay_overwrite_installs.inc();
            return r;
        }
        promised = false;
        const r = self.registry.install(fill.key, &fill) catch |e| return self.installFailed(e, &fill, &mint);
        stats.overlay_installs.inc();
        return r;
    }

    fn installFailed(_: *Overlay, e: error{ Exists, Full }, fill: *Record, mint: *block_alloc.MintedBlockGuard) ?*Record {
        switch (e) {
            error.Exists => stats.overlay_install_race_under_guard.fire("a record appeared under the writer's block guard"),
            error.Full => stats.overlay_registry_full.fire("an install holding a reservation found the overlay registry full"),
        }
        fill.fsck_guard.?.deinit();
        mint.abandon();
        return null;
    }

    // ---- §3.7 the settle ----

    /// Register-then-recheck on `inflight_change` until the in-flight set
    /// is empty (law 9's wait set); a 30 s wait names the leaked ticket's
    /// claim words — never a hang.
    fn awaitInflight(_: *Overlay, io: Io, rec: *Record) void {
        var waited = false;
        const t0 = clock.monotonicNs();
        while (!rec.core.inflightEmpty()) {
            var w = rec.inflight_change.notified();
            _ = w.enable(io);
            if (rec.core.inflightEmpty()) {
                w.cancel(io);
                break;
            }
            if (!waited) {
                waited = true;
                stats.overlay_teardown_waits.inc();
            }
            if (w.waitUntilUncancelable(io, clock.monotonicNs() + SETTLE_BARK_NS) == .deadline) {
                stats.overlay_teardown_wait_overlong.fire("settle waited 30 s on an in-flight overlay store");
                log.err("overlay ino {d} block {d}: {d} store(s) still in flight after {d} s — claims {x}", .{ rec.key.ino, rec.key.block, rec.core.inflight.load(.monotonic), (clock.monotonicNs() - t0) / std.time.ns_per_s, rec.core.claims[0].load(.monotonic) });
            }
        }
    }

    /// ONE write-arm attempt under the CALLER's (3) — the venues that hold
    /// the block guard for their own ladder (the store's steal and claim
    /// conflict, the write admission's one-authority screen). Fence
    /// immediate. `.transient` leaves the record FROZEN never-lossy: the
    /// caller drops its guard and takes `settleWriteArm`, never the ladder
    /// under its own lock.
    pub const AttemptVerdict = union(enum) {
        /// Terminal: the close-owed hint.
        settled: bool,
        transient,
        fenced,
    };

    pub fn settleOnceLocked(self: *Overlay, io: Io, rec: *Record, barrier: bool) AttemptVerdict {
        const out = self.settleLocked(io, rec, barrier) catch |e| switch (e) {
            error.WriterGuardFenced => return .fenced,
            error.Transient, error.OutOfMemory => {
                stats.overlay_settle_retries.inc();
                return .transient;
            },
        };
        return .{ .settled = switch (out) {
            .fed => |f| f.coverage_complete,
            .published, .superseded, .already_terminal => false,
        } };
    }

    /// The write arm's form: retry transients forever on the ladder, fence
    /// immediate; ONE (3) acquisition per attempt — the backoff runs with
    /// the guard dropped (module doc), the record re-looked-up under the
    /// next acquisition (a sibling settler may have landed or torn it
    /// down). Returns the close-owed hint.
    pub fn settleWriteArm(self: *Overlay, io: Io, locks: *seams.BlockLocks, ino: u64, block: u32, barrier: bool) bool {
        var ladder = SETTLE_LADDER;
        while (true) {
            const step: retry.Ladder.Step = blk: {
                const g = locks.acquire(io, ino, block, .settle);
                defer g.release(io);
                const rec = self.registry.get(.{ .ino = ino, .block = block }) orelse return false;
                switch (self.settleOnceLocked(io, rec, barrier)) {
                    .settled => |hint| return hint,
                    .fenced => return false,
                    .transient => {
                        const s = ladder.next();
                        if (ladder.shouldBark()) log.warn("overlay ino {d} block {d}: settle transient, attempt {d}", .{ ino, block, ladder.attempts() });
                        break :blk s;
                    },
                }
            };
            io.sleep(.{ .nanoseconds = @intCast(step.backoff_ns) }, .awake) catch {};
        }
    }

    /// The fence class of a device leg (the seed read/write, the flush):
    /// W5 — publish nothing, free nothing (successor recovery owns the
    /// dest), the record torn down FenceDropped, the must-stay-0 fired.
    /// Every other device error is a transient the ladder retries.
    const DeviceLegError = seams.ReadError || seams.DmaError;

    fn deviceLegFailed(self: *Overlay, rec: *Record, e: DeviceLegError, leg: []const u8) SettleError {
        switch (e) {
            error.WriterGuardFenced, error.VolumeFailed => {
                stats.overlay_fence_drops.fire("an overlay settle's device leg met the fence");
                log.err("overlay ino {d} block {d}: the settle's {s} met the fence ({s}) — publishing nothing, freeing nothing", .{ rec.key.ino, rec.key.block, leg, @errorName(e) });
                _ = rec.core.fenceDrop();
                self.teardown(rec);
                return error.WriterGuardFenced;
            },
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.Transient,
        }
    }

    /// FREEZE → COMPLETE → SEED GAPS → FLUSH → PUBLISH (feed | durable merge)
    /// → teardown. Under (3). A `Transient` leaves the record FROZEN
    /// never-lossy; the fence tears it down publishing nothing, freeing
    /// nothing. Returns the outcome; `fed`/`published` with a complete
    /// coverage is the close-owed hint for the guard-dropping caller.
    pub fn settleLocked(self: *Overlay, io: Io, rec: *Record, barrier: bool) SettleError!SettleOutcome {
        switch (rec.core.stateNow()) {
            .open => _ = rec.core.freeze(),
            .frozen => {},
            .published, .superseded, .fence_dropped, .fed => {
                self.teardown(rec);
                return .already_terminal;
            },
        }
        if (self.settle_transients.load(.monotonic) != 0) {
            _ = self.settle_transients.fetchSub(1, .monotonic);
            return error.Transient;
        }
        // COMPLETE.
        self.awaitInflight(io, rec);
        // SEED GAPS.
        const bs: u32 = rec.core.block_len;
        var gaps_buf: [CoverageUnion.max_gaps]cov.Run = undefined;
        const gaps = rec.core.union_.gaps(bs, &gaps_buf);
        const app_complete = gaps.len == 0;
        if (rec.old_binding) |old| {
            // The dead-capture belt (finding 51): a displaced old key whose
            // lifetime ended is never retried.
            const ov = self.deps.vols.byTag(old.vol_tag);
            const alive = if (ov) |v| blk: {
                const idx = v.alloc.idxOf(old.offset);
                break :blk v.alloc.liveIncarnation(idx, old.incarnation) and v.alloc.refcount(idx) != null;
            } else false;
            if (!alive) {
                stats.overlay_superseded_dead_old_binding.inc();
                stats.overlay_supersessions.inc();
                _ = rec.core.supersede();
                self.teardown(rec);
                return .superseded;
            }
        }
        if (gaps.len != 0) self.seedGaps(rec, gaps) catch |e| return self.deviceLegFailed(rec, e, "gap seed");
        // FLUSH (fsync class only).
        if (barrier) self.deps.device.flush(rec.dest.vol_tag) catch |e| return self.deviceLegFailed(rec, e, "flush");
        // PUBLISH.
        rec.alloc.publishBlock(rec.dest_idx);
        const new_key = rec.dest;
        self.deps.purge.purge(new_key);
        const min_size: u64 = if (app_complete) (@as(u64, rec.key.block) + 1) * bs else 0;
        const covered_app_bytes = rec.core.union_.coveredBytes();
        const proof: seams.DmaProof = .{ .vol_tag = new_key.vol_tag, .offset = new_key.offset, .len = bs, .completed_ns = clock.monotonicNs() };
        if (rec.old_binding) |old| {
            // Arm (a) FEED: the fsck guard travels record → epoch (continuous
            // C2/C3 visibility).
            const guard = rec.fsck_guard.?;
            rec.fsck_guard = null;
            const v = self.deps.epochs.record(rec.key.ino, rec.key.block, old, new_key, proof, min_size, guard, true) catch |e| switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
            };
            switch (v) {
                .shadowed => |s| {
                    if (s.displaced_prev) |d| self.deps.purge.purge(d);
                    epoch_mod.stats.rewrite_blocks.inc();
                    epoch_mod.stats.rewrite_user_bytes.add(covered_app_bytes);
                    epoch_mod.stats.rewrite_device_write_bytes.add(bs);
                    if (!rec.core.markFed()) {
                        // A guard-less discarding belt superseded the record
                        // post-freeze: disarm — never free the epoch's live B.
                        if (rec.mint) |*m| m.disarm();
                    }
                    stats.overlay_epoch_feeds.inc();
                    self.teardown(rec);
                    return .{ .fed = .{ .coverage_complete = s.coverage_complete } };
                },
                .not_shadowed => {
                    // The guard was consumed by the epoch; the durable arm
                    // registers its own visibility for the merge window.
                    rec.fsck_guard = block_alloc.InflightAllocGuard.register(rec.alloc, rec.dest_idx);
                },
                .save_in_flight => {
                    // The ino's running save must land (or fold) first
                    // (`RecordVerdict.save_in_flight`): a TRANSIENT — the record stays FROZEN, the
                    // settler's ladder retries with the guard dropped
                    // (the save lands within its own merge).
                    rec.fsck_guard = block_alloc.InflightAllocGuard.register(rec.alloc, rec.dest_idx);
                    return error.Transient;
                },
            }
        }
        // The durable-merge arm (fresh records + the shadow-off degenerate).
        var displaced: seams.MergeOutcome = .{};
        while (true) {
            const token = @max(self.deps.map.currentFencing(rec.key.ino), rec.fence_token);
            displaced = self.deps.map.merge(.{ .ino = rec.key.ino, .inserts = &.{.{ .block = rec.key.block, .binding = new_key }}, .min_size = min_size, .proofs = &.{proof}, .fencing = token }) catch |e| switch (e) {
                error.FencingTokenExpired => {
                    rec.fence_token = self.deps.map.currentFencing(rec.key.ino);
                    continue;
                },
                error.WriterGuardFenced => {
                    stats.overlay_fence_drops.fire("overlay publish met the writer-guard fence");
                    _ = rec.core.fenceDrop();
                    self.teardown(rec);
                    return error.WriterGuardFenced;
                },
                error.NotFound => {
                    stats.writeback_orphan_discards_overlay.inc();
                    stats.overlay_supersessions.inc();
                    _ = rec.core.supersede();
                    self.teardown(rec);
                    return .superseded;
                },
                error.Transient, error.OutOfMemory => return error.Transient,
            };
            break;
        }
        if (rec.core.markPublished()) {
            if (rec.mint) |*m| m.disarm();
        }
        stats.overlay_publishes.inc();
        stats.overlay_published_bytes.add(bs);
        if (rec.old_binding != null) stats.overlay_feed_fallbacks.inc();
        self.teardown(rec);
        // Deferred frees: only AFTER the publish and the retire.
        for (displaced.slice()) |d| {
            self.deps.purge.purge(d);
            _ = self.deps.free.free(io, d);
        }
        displaced.dropHolds();
        return .published;
    }

    /// Each gap: ONE pooled buffer from the old binding (ranged per gap when
    /// the lever is on and Σ gaps < the block window — else the whole old
    /// image once) or zeros (fresh), written at `dest + gap.start`. A device
    /// error passes through in its class (the caller's fence test).
    fn seedGaps(self: *Overlay, rec: *Record, gaps: []const cov.Run) DeviceLegError!void {
        const bs: u64 = rec.core.block_len;
        var total: u64 = 0;
        for (gaps) |g| total += g.len();
        const old = rec.old_binding;
        const ranged = old != null and self.cfg.gap_seed_ranged and total < bs;
        // The alignment travels with the slice: a free at align 1 of a
        // page-aligned allocation is the safe allocator's panic (the
        // whole-image leg first ran on the venue in generic/551).
        var whole: ?[]align(OVERLAY_PAGE) u8 = null;
        defer if (whole) |w| self.gpa.free(w);
        if (old != null and !ranged) {
            const w = self.gpa.alignedAlloc(u8, .fromByteUnits(OVERLAY_PAGE), @intCast(bs)) catch return error.OutOfMemory;
            whole = w;
            try self.deps.device.read(old.?.vol_tag, old.?.offset, w);
            stats.overlay_gap_seed_read_bytes.add(bs);
        }
        for (gaps) |g| {
            const n: usize = g.len();
            const buf = self.gpa.alignedAlloc(u8, .fromByteUnits(OVERLAY_PAGE), n) catch return error.OutOfMemory;
            defer self.gpa.free(buf);
            if (old) |o| {
                if (ranged) {
                    try self.deps.device.read(o.vol_tag, o.offset + g.start, buf);
                    stats.overlay_gap_seed_ranged_bytes.add(n);
                } else {
                    @memcpy(buf, whole.?[g.start..g.end]);
                }
                stats.overlay_gap_seed_old_bytes.add(n);
            } else {
                @memset(buf, 0);
            }
            _ = try self.deps.device.write(rec.dest.vol_tag, rec.dest.offset + g.start, buf);
            stats.overlay_gap_seeds.inc();
            stats.overlay_gap_seed_bytes.add(n);
        }
    }

    /// Teardown disposition (exhaustive): Published ⇒ disarm; Fed ⇒ disarm
    /// WITHOUT freeing (the dest is the epoch's pending B key); FenceDropped
    /// ⇒ disarm without freeing (successor recovery owns the accounting);
    /// Superseded ⇒ the armed guard FREES; Open|Frozen ⇒ tripwire + disarm.
    fn teardown(self: *Overlay, rec: *Record) void {
        switch (rec.core.stateNow()) {
            .published, .fed, .fence_dropped => if (rec.mint) |*m| m.disarm(),
            .superseded => if (rec.mint) |*m| m.abandon(),
            .open, .frozen => {
                stats.overlay_teardown_nonterminal.fire("overlay record torn down while non-terminal");
                if (rec.mint) |*m| m.disarm();
            },
        }
        rec.mint = null;
        if (rec.fsck_guard) |*g| g.deinit();
        rec.fsck_guard = null;
        self.registry.retire(rec);
    }

    /// The guard-dropping caller's drain of one block (the detached
    /// publisher): the write-arm settle (one (3) acquisition per attempt)
    /// → the close hint acted on. `barrier` = the fsync class.
    pub fn drainBlock(self: *Overlay, io: Io, locks: *seams.BlockLocks, ino: u64, block: u32, barrier: bool, cs: epoch_mod.Epochs.CloseSeams) void {
        const hint = self.settleWriteArm(io, locks, ino, block, barrier);
        // The epoch-close venue law: never under the block guard.
        if (hint) _ = self.deps.epochs.close(io, ino, cs);
    }

    pub fn spawnDrain(self: *Overlay, io: Io, locks: *seams.BlockLocks, ino: u64, block: u32, cs: epoch_mod.Epochs.CloseSeams) void {
        self.tasks.spawn(io, drainTask, .{ self, io, locks, ino, block, cs }) catch self.drainBlock(io, locks, ino, block, false, cs);
    }

    fn drainTask(self: *Overlay, io: Io, locks: *seams.BlockLocks, ino: u64, block: u32, cs: epoch_mod.Epochs.CloseSeams) Io.Cancelable!void {
        self.drainBlock(io, locks, ino, block, false, cs);
    }

    /// §3.8: every record of `ino` opened before the drain began — blocks
    /// at or past `keep_below` SUPERSEDE (a shape-change op owns the
    /// discarded data; a Frozen one is still drained, never stranded);
    /// below ⇒ drain with `barrier`. The registry enumerates a page of
    /// blocks at a time; the drain repeats until a page holds nothing it
    /// owes (LTP fs_fill — xfs on a loop device over a 300 MiB file —
    /// kept > 64 records open at once: the single page left the rest
    /// unpublished at fsync). Returns the owed records still open
    /// afterwards (counted by the fsync as `overlay_unpublished_at_fsync`).
    pub fn drainForIno(self: *Overlay, io: Io, locks: *seams.BlockLocks, ino: u64, keep_below: u64, barrier: bool, cs: epoch_mod.Epochs.CloseSeams) u32 {
        const began = clock.monotonicNs();
        var blocks: [64]u32 = undefined;
        // Bounded: each pass retires what it enumerated; a pass that owes
        // nothing ends the drain. The cap is the registry's capacity in
        // pages plus the passes a concurrent writer's re-opens could add.
        var passes: u32 = 0;
        while (passes < 1024) : (passes += 1) {
            var owed: u32 = 0;
            for (self.registry.blocksOf(ino, &blocks)) |b| {
                if (!self.owedSince(ino, b, began)) continue;
                owed += 1;
                if (b >= keep_below) {
                    const frozen = blk: {
                        const g = locks.acquire(io, ino, b, .overlay_prune);
                        defer g.release(io);
                        const rec = self.registry.get(.{ .ino = ino, .block = b }) orelse break :blk false;
                        if (rec.core.stateNow() == .frozen) break :blk true;
                        self.awaitInflight(io, rec);
                        stats.overlay_supersessions.inc();
                        _ = rec.core.supersede();
                        self.teardown(rec);
                        break :blk false;
                    };
                    // A Frozen record is still drained, never stranded — on
                    // the guard-dropping ladder like every settle.
                    if (frozen) _ = self.settleWriteArm(io, locks, ino, b, barrier);
                } else {
                    self.drainBlock(io, locks, ino, b, barrier, cs);
                }
            }
            if (owed == 0) break;
        }
        var left: u32 = 0;
        var left_blocks: [64]u32 = undefined;
        for (self.registry.blocksOf(ino, &left_blocks)) |b| {
            if (self.owedSince(ino, b, began)) left += 1;
        }
        return left;
    }

    /// Whether `(ino, block)` holds a record opened at or before `since`.
    fn owedSince(self: *Overlay, ino: u64, block: u32, since: u64) bool {
        const rec = self.registry.get(.{ .ino = ino, .block = block }) orelse return false;
        return rec.opened_ns <= since;
    }

    // ---- §3.5 the read composition ----

    pub const ServeOutcome = union(enum) {
        /// `out` holds the window: covered ranges from dest, gaps from the
        /// old binding or zeros.
        served,
        /// No open record for the block.
        none,
        /// Validation failed past the bounded retries, or the record went
        /// terminal mid-read: the caller takes the serialized settle.
        settle,
    };

    /// `[rel, rel + out.len)` of `(ino, block)` from the overlay (the read
    /// path consults it FIRST). Snapshot `g` → await in-flight pages →
    /// covered runs from dest, gaps from the old binding (short-tolerant)
    /// or zeros (law 5) → REVALIDATE (terminal re-check AFTER every device
    /// fetch, generation equality, no range in flight) else loop; bounded
    /// by `SETTLE_RETRIES`.
    pub fn serve(self: *Overlay, io: Io, ino: u64, block: u32, rel: u32, out: []u8) ServeOutcome {
        if (!self.anyOpenFast()) return .none;
        const rec = self.registry.get(.{ .ino = ino, .block = block }) orelse return .none;
        const first_page: u32 = rel / OVERLAY_PAGE;
        const end: u32 = rel + @as(u32, @intCast(out.len));
        const pages: u32 = (end + OVERLAY_PAGE - 1) / OVERLAY_PAGE - first_page;
        var attempt: u32 = 0;
        while (attempt < SETTLE_RETRIES) : (attempt += 1) {
            const st = rec.core.stateNow();
            if (st.isTerminal()) return .settle;
            const snap = rec.core.readBegin();
            self.awaitPages(io, rec, first_page, pages);
            if (rec.core.stateNow().isTerminal()) return .settle;
            // Covered runs from dest; gaps collected between them.
            var runs_buf: [CoverageUnion.max_runs]cov.Run = undefined;
            rec.core.union_lock.lock();
            const runs = rec.core.union_.coveredRunsIn(rel, end, &runs_buf);
            rec.core.union_lock.unlock();
            var cursor: u32 = rel;
            var gap_bytes: u64 = 0;
            var ok = true;
            for (runs) |r| {
                if (r.start > cursor) {
                    if (!self.serveGap(rec, cursor, out[cursor - rel .. r.start - rel])) {
                        ok = false;
                        break;
                    }
                    gap_bytes += r.start - cursor;
                }
                self.deps.device.read(rec.dest.vol_tag, rec.dest.offset + r.start, out[r.start - rel .. r.end - rel]) catch {
                    ok = false;
                    break;
                };
                cursor = r.end;
            }
            if (ok and cursor < end) {
                if (!self.serveGap(rec, cursor, out[cursor - rel ..])) ok = false;
                gap_bytes += end - cursor;
            }
            if (!ok) return .settle;
            // REVALIDATE.
            if (rec.core.stateNow().isTerminal()) return .settle;
            if (!rec.core.readValid(snap) or rec.core.rangeInflight(first_page, pages)) continue;
            stats.overlay_read_serves.inc();
            stats.overlay_read_serve_bytes.add(out.len);
            if (gap_bytes != 0) {
                stats.overlay_read_gap_serves.inc();
                stats.overlay_read_gap_bytes.add(gap_bytes);
            }
            return .served;
        }
        stats.overlay_window_escalations.inc();
        return .settle;
    }

    fn serveGap(self: *Overlay, rec: *Record, start: u32, out: []u8) bool {
        if (rec.old_binding) |old| {
            self.deps.device.read(old.vol_tag, old.offset + start, out) catch return false;
            return true;
        }
        @memset(out, 0);
        return true;
    }

    /// The future law: a read intersecting an acked-but-incomplete store
    /// waits for the store — never an uncovered old-map gap. Event wait on
    /// `inflight_change` (Q4: no yield spin).
    fn awaitPages(_: *Overlay, io: Io, rec: *Record, first_page: u32, pages: u32) void {
        while (rec.core.rangeInflight(first_page, pages)) {
            var w = rec.inflight_change.notified();
            _ = w.enable(io);
            if (!rec.core.rangeInflight(first_page, pages)) {
                w.cancel(io);
                return;
            }
            if (w.waitUntilUncancelable(io, clock.monotonicNs() + SETTLE_BARK_NS) == .deadline) {
                stats.overlay_teardown_wait_overlong.fire("a read waited 30 s on an in-flight overlay store");
            }
        }
    }

    /// The write-side custody probe for the read seam.
    pub fn hasCustody(self: *Overlay, ino: u64, block: u32) bool {
        if (!self.anyOpenFast()) return false;
        const r = self.registry.get(.{ .ino = ino, .block = block }) orelse return false;
        return !r.core.stateNow().isTerminal();
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

test "overlay_core: coverage publishes only on a successful complete (law 3); the completion fires exactly once at full union; overlapping in-flight stores are unrepresentable (§3.3); generations are strictly monotone per accepted segment" {
    var c = Core.init(8 * OVERLAY_PAGE, 1);
    const t0 = try c.beginStore(0, 2);
    try testing.expectEqual(@as(u64, 1), t0.generation);
    try testing.expectError(error.Overlap, c.beginStore(1, 1)); // in flight
    try testing.expectError(error.OutOfRange, c.beginStore(7, 2));
    try testing.expect(!c.coveredProbe(0, 2)); // claimed ⇒ never served
    try testing.expect(c.rangeInflight(0, 2));
    const t1 = try c.beginStore(2, 6); // disjoint ranges run concurrently (the 4 × 1 MiB cohort)
    try testing.expectEqual(@as(u64, 2), t1.generation);
    try testing.expectEqual(@as(u32, 2), c.inflight.load(.monotonic));
    // A failed CQE covers nothing but releases its claim.
    try testing.expectEqual(CompleteVerdict.not_covered, c.completeStore(t0, false));
    try testing.expect(!c.coveredProbe(0, 2));
    try testing.expect(!c.rangeInflight(0, 2));
    const t2 = try c.beginStore(0, 2); // re-write of the released range
    try testing.expectEqual(@as(u64, 3), t2.generation);
    try testing.expectEqual(CompleteVerdict{ .covered = .{ .coverage_complete = false } }, c.completeStore(t1, true));
    try testing.expect(c.coveredProbe(2, 6));
    try testing.expectEqual(CompleteVerdict{ .covered = .{ .coverage_complete = true } }, c.completeStore(t2, true));
    try testing.expect(c.union_.isFull(8 * OVERLAY_PAGE));
    try testing.expectEqual(State.frozen, c.stateNow()); // the completing CQE froze it before releasing its claim
    try testing.expect(c.inflightEmpty());
    // A snapshot taken before an accept is invalid afterwards (equality).
    var e = Core.init(8 * OVERLAY_PAGE, 2);
    const snap = e.readBegin();
    try testing.expect(e.readValid(snap));
    _ = try e.beginStore(0, 1);
    try testing.expect(!e.readValid(snap));
}

test "overlay_core: freeze refuses new stores and the in-flight set drains to publishable; law 9 rollback requires terminal ∧ empty; fence_drop is terminal and never publishable; fed is reachable only from the freeze" {
    var c = Core.init(4 * OVERLAY_PAGE, 1);
    const t = try c.beginStore(0, 1);
    try testing.expect(c.freeze());
    try testing.expect(!c.freeze());
    try testing.expectError(error.NotOpen, c.beginStore(1, 1));
    try testing.expect(!c.inflightEmpty());
    try testing.expect(!c.rollbackAdmissible());
    try testing.expectEqual(CompleteVerdict{ .covered = .{ .coverage_complete = false } }, c.completeStore(t, true)); // a store accepted before the freeze still covers
    try testing.expect(c.inflightEmpty());
    try testing.expect(c.rollbackAdmissible() == false); // frozen is not terminal
    var d = Core.init(4 * OVERLAY_PAGE, 2);
    try testing.expect(d.fenceDrop());
    try testing.expectEqual(State.fence_dropped, d.stateNow());
    try testing.expect(!d.markPublished());
    try testing.expect(!d.freeze());
    try testing.expect(d.rollbackAdmissible());
    var e = Core.init(4 * OVERLAY_PAGE, 3);
    try testing.expect(!e.markFed()); // open → fed is not a transition
    try testing.expect(e.freeze());
    try testing.expect(e.markFed());
    try testing.expectEqual(State.fed, e.stateNow());
    try testing.expect(!e.supersede());
    // A store completing on a terminal record is superseded and publishes nothing.
    var f = Core.init(4 * OVERLAY_PAGE, 4);
    const tf = try f.beginStore(0, 4);
    try testing.expect(f.freeze());
    try testing.expect(f.markPublished());
    try testing.expectEqual(CompleteVerdict.superseded, f.completeStore(tf, true));
    try testing.expect(!f.coveredProbe(0, 4));
}

const TestWorld = struct {
    f: *wt.Fixture,
    epochs: epoch_mod.Epochs,
    stripes: layout.MetaStripes,
    locks: seams.BlockLocks,
    gov: pipeline.Governor,
    ov: Overlay,

    fn init(gpa: Allocator, cfg_over: struct { patch_max: u64 = wt.BS / 8, overwrite: bool = true, shadow: bool = true, close_barrier: bool = false, gap_seed_ranged: bool = true, blocks: u32 = 32, registry_capacity: usize = 16 }) !*TestWorld {
        const w = try gpa.create(TestWorld);
        errdefer gpa.destroy(w);
        w.f = try wt.Fixture.init(gpa, cfg_over.blocks);
        w.epochs = epoch_mod.Epochs.init(gpa, wt.BS, cfg_over.shadow);
        w.stripes = .{};
        w.locks = try seams.BlockLocks.init(gpa, 64);
        w.gov = try pipeline.Governor.init(gpa, wt.BS, 2, .adaptive, null, pipeline.capBytes(1 * derive.GiB));
        w.ov = try Overlay.init(gpa, .{ .block_size = wt.BS, .patch_max = cfg_over.patch_max, .overwrite = cfg_over.overwrite, .close_barrier = cfg_over.close_barrier, .gap_seed_ranged = cfg_over.gap_seed_ranged }, .{
            .device = w.f.dev.seam(),
            .map = w.f.map.seam(),
            .free = w.f.frees.seam(),
            .purge = .none,
            .vols = &w.f.vols,
            .picker = .{ .ctx = w.f.alloc_a, .pick = &pickA },
            .epochs = &w.epochs,
            .stripes = &w.stripes,
            .governor = &w.gov,
        }, cfg_over.registry_capacity);
        return w;
    }

    fn pickA(ctx: ?*anyopaque, _: enospc.Place) ?*block_alloc.BlockAllocator {
        return @ptrCast(@alignCast(ctx.?));
    }

    fn deinit(self: *TestWorld, gpa: Allocator) void {
        self.ov.deinit(testing.io);
        self.gov.deinit();
        self.locks.deinit(gpa);
        self.epochs.deinit();
        self.f.deinit();
        gpa.destroy(self);
    }

    fn cs(self: *TestWorld) epoch_mod.Epochs.CloseSeams {
        return .{ .map = self.f.map.seam(), .free = self.f.frees.seam(), .purge = .none };
    }

    fn striped(size: u64) seams.FileView {
        return .{ .kind = .striped, .size = size, .map_resident = true };
    }
};

fn pattern(buf: []u8, seed: u8) void {
    for (buf, 0..) |*b, i| b.* = seed +% @as(u8, @truncate(i * 7));
}

test "fresh_block_segments_ride_the_overlay_and_publish_at_completion: two disjoint pieces (end first) → one record, coverage completes, ONE merge, stores ≈ user bytes, no extraction, zero gap seeds" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(40, .striped, 0);
    const half: usize = wt.BS / 2;
    var p1: [wt.BS / 2]u8 align(4096) = undefined;
    var p0: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&p1, 0x10);
    pattern(&p0, 0x20);
    const bytes0 = stats.overlay_store_bytes.load();
    const o1 = try w.ov.store(io, .{ .ino = 40, .block = 0, .rel = @intCast(half), .vehicle = .{ .bytes = &p1 }, .file = TestWorld.striped(0), .overwrite = false });
    try testing.expect(!o1.stored.coverage_complete);
    try testing.expectEqual(@as(u32, 1), w.ov.registry.open_count.load(.monotonic));
    try testing.expect(w.ov.hasCustody(40, 0));
    // The record is reader-visible BEFORE publication: covered bytes from dest, the gap zeros (law 5).
    var out: [wt.BS]u8 = undefined;
    try testing.expectEqual(Overlay.ServeOutcome.served, w.ov.serve(io, 40, 0, 0, &out));
    try testing.expectEqualSlices(u8, &p1, out[half..]);
    try testing.expect(std.mem.allEqual(u8, out[0..half], 0));
    w.f.dev.reads = 0; // the serve's dest read is a READ-path byte, not a write-path one
    const o0 = try w.ov.store(io, .{ .ino = 40, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p0 }, .file = TestWorld.striped(0), .overwrite = false });
    try testing.expect(o0.stored.coverage_complete);
    try testing.expectEqual(bytes0 + wt.BS, stats.overlay_store_bytes.load());
    // The guard-dropping caller drains: FREEZE (done at the CQE) → COMPLETE → no gaps → PUBLISH: one merge, the map binds dest, min_size = the block end.
    w.ov.drainBlock(io, &w.locks, 40, 0, false, w.cs());
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(wt.BS, w.f.map.merges.items[0].min_size);
    const bound = w.f.map.seam().lookup(40, 0).?;
    try testing.expectEqual(wt.VOL_A, bound.vol_tag);
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    try testing.expectEqual(@as(usize, 0), w.f.frees.freed.items.len);
    // Device bytes = user bytes (no seed, no RMW).
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.writeBytesTotal());
    try testing.expectEqual(@as(u64, 0), w.f.dev.reads);
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, bound.offset, &got);
    try testing.expectEqualSlices(u8, &p0, got[0..half]);
    try testing.expectEqualSlices(u8, &p1, got[half..]);
    try testing.expect(w.f.alloc_a.fillIncarnation(w.f.alloc_a.idxOf(bound.offset)) != null); // published (stable)
}

test "partial_overlay_fsync_seeds_zeros_and_publishes_whole_block; open_overlay_reads_serve_acked_bytes_and_zero_gaps; the fsync sequence runs FREEZE → COMPLETE → SEED → FLUSH → PUBLISH with the barrier before the merge" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(41, .striped, 0);
    var p: [wt.BS / 4]u8 align(4096) = undefined;
    pattern(&p, 0x33);
    const unpub0 = counters.tripwireCount("overlay_unpublished_at_fsync");
    _ = try w.ov.store(io, .{ .ino = 41, .block = 0, .rel = wt.BS / 4, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.resetLedger();
    const seeds0 = stats.overlay_gap_seeds.load();
    const left = w.ov.drainForIno(io, &w.locks, 41, std.math.maxInt(u64), true, w.cs());
    try testing.expectEqual(@as(u32, 0), left);
    try testing.expectEqual(seeds0 + 2, stats.overlay_gap_seeds.load()); // the gap before and after
    // Trace: seed writes, then the flush, then (the merge) — the barrier precedes the meta publish.
    const tr = w.f.dev.trace.items;
    try testing.expectEqual(wt.FakeDevice.Event.write, tr[0]);
    try testing.expectEqual(wt.FakeDevice.Event.write, tr[1]);
    try testing.expectEqual(wt.FakeDevice.Event.flush, tr[2]);
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    // Seeded zeros were never app-written: SIZE NEVER LEADS DATA — min_size 0.
    try testing.expectEqual(@as(u64, 0), w.f.map.merges.items[0].min_size);
    const b = w.f.map.seam().lookup(41, 0).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, b.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 4], 0));
    try testing.expectEqualSlices(u8, &p, got[wt.BS / 4 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
    try testing.expectEqual(unpub0, counters.tripwireCount("overlay_unpublished_at_fsync"));
}

test "the fsync drain covers EVERY open record of the ino, however many: 70 blocks each with an open record (more than one enumeration page) all publish, none left (repro-port: LTP fs_fill — xfs on a loop device over a 300 MiB file — fired overlay_unpublished_at_fsync five times)" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .blocks = 160, .registry_capacity = 96 });
    defer w.deinit(testing.allocator);
    try w.f.map.create(44, .striped, 0);
    var p: [wt.BS / 4]u8 align(4096) = undefined;
    pattern(&p, 0x44);
    const n: u32 = 70;
    var b: u32 = 0;
    while (b < n) : (b += 1) {
        const o = try w.ov.store(io, .{ .ino = 44, .block = b, .rel = wt.BS / 4, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(@as(u64, b) * wt.BS), .overwrite = false });
        try testing.expect(o == .stored);
    }
    try testing.expectEqual(@as(u32, n), w.ov.registry.open_count.load(.monotonic));
    const unpub0 = counters.tripwireCount("overlay_unpublished_at_fsync");
    const left = w.ov.drainForIno(io, &w.locks, 44, std.math.maxInt(u64), true, w.cs());
    try testing.expectEqual(@as(u32, 0), left);
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    try testing.expectEqual(unpub0, counters.tripwireCount("overlay_unpublished_at_fsync"));
    b = 0;
    while (b < n) : (b += 1) try testing.expect(w.f.map.seam().lookup(44, b) != null);
}

test "registryCapacity ties to its formula: max(parked_cap, ⌈pipe ÷ block⌉)" {
    const MiB = derive.MiB;
    const GiB = derive.GiB;
    // The shipped derivations at a 4 GiB budget: 256 parked buffers, a
    // 1 GiB pipe = 256 blocks of 4 MiB.
    try testing.expectEqual(@as(usize, 256), registryCapacity(256, pipeline.capBytes(4 * GiB), 4 * MiB));
    // 64 GiB: the pipe's 4,096 blocks outrun the parked cap's 1,024.
    try testing.expectEqual(@as(usize, 4096), registryCapacity(1024, pipeline.capBytes(64 * GiB), 4 * MiB));
    // An explicit small parked cap (the storm's 8) keeps the pipe's room.
    try testing.expectEqual(@as(usize, 256), registryCapacity(8, pipeline.capBytes(4 * GiB), 4 * MiB));
    // A pipe smaller than the parked set: the parked cap; a partial block rounds up.
    try testing.expectEqual(@as(usize, 300), registryCapacity(300, 8 * MiB, 4 * MiB));
    try testing.expectEqual(@as(usize, 3), registryCapacity(1, 8 * MiB + 1, 4 * MiB));
}

// The write-back storm (16 writers, a registry the parked cap of 8 sized)
// minted a destination, found the registry full and fired the must-stay-0
// `overlay_registry_full` on every run.
test "a full registry declines a fresh install BEFORE it mints (`overlay_registry_declines`; the next arm's parked gate is the writer's backpressure): no destination minted, nothing trips, a join of an open record still lands, and the install after a retire succeeds" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .registry_capacity = 2 });
    defer w.deinit(testing.allocator);
    try w.f.map.create(45, .striped, 0);
    var p: [wt.BS / 4]u8 align(4096) = undefined;
    pattern(&p, 0x45);
    const full0 = counters.tripwireCount("overlay_registry_full");
    const declines0 = stats.overlay_registry_declines.load();
    for (0..2) |b| {
        const o = try w.ov.store(io, .{ .ino = 45, .block = @intCast(b), .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(@as(u64, b) * wt.BS), .overwrite = false });
        try testing.expect(o == .stored);
    }
    const free0 = w.f.alloc_a.freeBlocksCount();
    const o3 = try w.ov.store(io, .{ .ino = 45, .block = 2, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(2 * wt.BS), .overwrite = false });
    try testing.expect(o3 == .declined);
    try testing.expectEqual(declines0 + 1, stats.overlay_registry_declines.load());
    try testing.expectEqual(full0, counters.tripwireCount("overlay_registry_full"));
    try testing.expectEqual(free0, w.f.alloc_a.freeBlocksCount());
    // A join needs no slot.
    const oj = try w.ov.store(io, .{ .ino = 45, .block = 1, .rel = wt.BS / 4, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(2 * wt.BS), .overwrite = false });
    try testing.expect(oj == .stored);
    // A retire returns the slot.
    w.ov.drainBlock(io, &w.locks, 45, 0, false, w.cs());
    const o4 = try w.ov.store(io, .{ .ino = 45, .block = 2, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(2 * wt.BS), .overwrite = false });
    try testing.expect(o4 == .stored);
    try testing.expectEqual(@as(u32, 2), w.ov.registry.open_count.load(.monotonic));
    try testing.expectEqual(@as(u32, 0), w.ov.drainForIno(io, &w.locks, 45, std.math.maxInt(u64), true, w.cs()));
    try testing.expectEqual(full0, counters.tripwireCount("overlay_registry_full"));
}

test "live_mapped_overwrite_rides_the_overlay_end_to_end: the install captures the old binding in ONE (3.5) section; reads serve gaps from the OLD binding; the settle seeds old bytes RANGED and FEEDS the rewrite epoch; the epoch close frees the displaced A key AFTER the save" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(42, 0, 0xAA);
    var p: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&p, 0x55);
    const ow0 = stats.overlay_overwrite_installs.load();
    const o = try w.ov.store(io, .{ .ino = 42, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    try testing.expect(!o.stored.coverage_complete);
    try testing.expectEqual(ow0 + 1, stats.overlay_overwrite_installs.load());
    // Read: the covered half from dest, the gap from the OLD binding (never the dest's recycled bytes).
    var out: [wt.BS]u8 = undefined;
    try testing.expectEqual(Overlay.ServeOutcome.served, w.ov.serve(io, 42, 0, 0, &out));
    try testing.expectEqualSlices(u8, &p, out[0 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 2 ..], 0xAA));
    // Settle (release class, no barrier): the gap is seeded from the old binding by ONE ranged read of exactly the gap, then FED.
    w.f.dev.resetLedger();
    const feeds0 = stats.overlay_epoch_feeds.load();
    const ranged0 = stats.overlay_gap_seed_ranged_bytes.load();
    const ff0 = stats.overlay_feed_fallbacks.load();
    w.ov.drainBlock(io, &w.locks, 42, 0, false, w.cs());
    try testing.expectEqual(feeds0 + 1, stats.overlay_epoch_feeds.load());
    try testing.expectEqual(ranged0 + wt.BS / 2, stats.overlay_gap_seed_ranged_bytes.load());
    try testing.expectEqual(@as(u64, wt.BS / 2), w.f.dev.read_bytes);
    // Fed: no merge yet, the epoch shadows the block, nothing freed, the overlay is gone.
    try testing.expectEqual(@as(usize, 0), w.f.map.merges.items.len);
    try testing.expect(w.epochs.bindsBlock(42, 0));
    try testing.expectEqual(@as(usize, 0), w.f.frees.freed.items.len);
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    try testing.expect(w.f.map.seam().lookup(42, 0).?.eql(a));
    // The close: ONE save, then the A key is freed.
    try testing.expect(w.epochs.close(io, 42, w.cs()) == .swapped);
    try testing.expectEqual(@as(usize, 1), w.f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a));
    // Held across its free, not after (decision 0003 §AH.1).
    try testing.expectEqual(@as(u32, 0), w.f.frees.unheld);
    try testing.expect(!w.f.alloc_a.inflightContains(w.f.alloc_a.idxOf(a.offset)));
    const nb = w.f.map.seam().lookup(42, 0).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, nb.offset, &got);
    try testing.expectEqualSlices(u8, &p, got[0 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0xAA));
    try testing.expectEqual(ff0, stats.overlay_feed_fallbacks.load());
}

// The whole-image seed leg (the ranged lever OFF, or a gap set that is the
// whole block) released its page-aligned image at align 1 — the safe
// allocator's panic, first met on the venue in generic/551 once a sibling's
// one-authority settle found a zero-coverage overwrite record.
test "the whole-image gap seed (SQUEEZEFS_GAP_SEED_RANGED=0): ONE whole read of the old binding seeds every gap byte-exact and the image is released at its own alignment" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .gap_seed_ranged = false });
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(45, 0, 0xAA);
    var p: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&p, 0x66);
    const o = try w.ov.store(io, .{ .ino = 45, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    try testing.expect(!o.stored.coverage_complete);
    w.f.dev.resetLedger();
    const whole0 = stats.overlay_gap_seed_read_bytes.load();
    const ranged0 = stats.overlay_gap_seed_ranged_bytes.load();
    w.ov.drainBlock(io, &w.locks, 45, 0, false, w.cs());
    try testing.expectEqual(whole0 + wt.BS, stats.overlay_gap_seed_read_bytes.load());
    try testing.expectEqual(ranged0, stats.overlay_gap_seed_ranged_bytes.load());
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.read_bytes);
    try testing.expect(w.epochs.close(io, 45, w.cs()) == .swapped);
    try testing.expect(w.f.frees.freed.items[0].eql(a));
    const nb = w.f.map.seam().lookup(45, 0).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, nb.offset, &got);
    try testing.expectEqualSlices(u8, &p, got[0 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0xAA));
}

test "ineligible_shapes_decline_and_stay_byte_exact: sub-cap declines unless joining an Open record; a shadow-bound block declines; a non-striped file declines; a non-page-aligned piece declines; the overwrite lever off declines overwrites; ENOSPC declines (KD-B4-8) to accumulation" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(43, .striped, 0);
    var small: [OVERLAY_PAGE]u8 align(4096) = @splat(1);
    const sub0 = stats.overlay_ineligible_sub_cap.load();
    try testing.expectEqual(StoreOutcome.declined, try w.ov.store(io, .{ .ino = 43, .block = 0, .rel = 0, .vehicle = .{ .bytes = &small }, .file = TestWorld.striped(0), .overwrite = false }));
    try testing.expectEqual(sub0 + 1, stats.overlay_ineligible_sub_cap.load());
    var big: [wt.BS / 2]u8 align(4096) = @splat(2);
    _ = try w.ov.store(io, .{ .ino = 43, .block = 0, .rel = 0, .vehicle = .{ .bytes = &big }, .file = TestWorld.striped(0), .overwrite = false });
    const joins0 = stats.overlay_sub_cap_joins.load();
    try testing.expect((try w.ov.store(io, .{ .ino = 43, .block = 0, .rel = wt.BS / 2, .vehicle = .{ .bytes = &small }, .file = TestWorld.striped(0), .overwrite = false })) == .stored);
    try testing.expectEqual(joins0 + 1, stats.overlay_sub_cap_joins.load());
    // Non-aligned piece and a non-striped file.
    try testing.expectEqual(StoreOutcome.declined, try w.ov.store(io, .{ .ino = 43, .block = 1, .rel = 100, .vehicle = .{ .bytes = &big }, .file = TestWorld.striped(0), .overwrite = false }));
    try testing.expectEqual(StoreOutcome.declined, try w.ov.store(io, .{ .ino = 43, .block = 1, .rel = 0, .vehicle = .{ .bytes = &big }, .file = .{ .kind = .staged, .size = 0, .map_resident = true }, .overwrite = false }));
    // Shadow-bound: a block the epoch shadows declines.
    const a = try w.f.publishBlock(44, 0, 0xAA);
    const m = try w.f.alloc_a.allocateBlock();
    const bnew = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    _ = try w.epochs.record(44, 0, a, bnew, .{ .vol_tag = wt.VOL_A, .offset = m.offset, .len = wt.BS, .completed_ns = 1 }, wt.BS, block_alloc.InflightAllocGuard.register(w.f.alloc_a, m.idx), true);
    const sb0 = stats.overlay_ineligible_shadow_bound.load();
    try testing.expectEqual(StoreOutcome.declined, try w.ov.store(io, .{ .ino = 44, .block = 0, .rel = 0, .vehicle = .{ .bytes = &big }, .file = TestWorld.striped(wt.BS), .overwrite = true }));
    try testing.expectEqual(sb0 + 1, stats.overlay_ineligible_shadow_bound.load());
    _ = w.epochs.close(io, 44, w.cs());
    // The overwrite lever off.
    const w2 = try TestWorld.init(testing.allocator, .{ .overwrite = false });
    defer w2.deinit(testing.allocator);
    _ = try w2.f.publishBlock(45, 0, 0xAA);
    try testing.expectEqual(StoreOutcome.declined, try w2.ov.store(io, .{ .ino = 45, .block = 0, .rel = 0, .vehicle = .{ .bytes = &big }, .file = TestWorld.striped(wt.BS), .overwrite = true }));
    // ENOSPC: every block minted ⇒ the install declines to accumulation, counted; nothing installed.
    while (w2.f.alloc_a.allocateBlock()) |_| {} else |_| {}
    try w2.f.map.create(46, .striped, 0);
    const en0 = stats.overlay_enospc_declines.load();
    try testing.expectEqual(StoreOutcome.declined, try w2.ov.store(io, .{ .ino = 46, .block = 0, .rel = 0, .vehicle = .{ .bytes = &big }, .file = TestWorld.striped(0), .overwrite = false }));
    try testing.expectEqual(en0 + 1, stats.overlay_enospc_declines.load());
    try testing.expect(!w2.ov.hasCustody(46, 0));
    // Drain 43 so nothing stays open.
    w.ov.drainBlock(io, &w.locks, 43, 0, false, w.cs());
}

test "the slot vehicle: WRITE_FIXED(device ← slot) lands the held bytes with zero daemon copies; a session without the direct leg declines (the mint returns); a failed direct store falls back to the extracted bytes (self-healing)" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(47, .striped, 0);
    var held: [wt.BS]u8 align(4096) = undefined;
    pattern(&held, 0x77);
    try w.f.dev.registerSlot(5, &held);
    const Ex = struct {
        fn extract(ctx: ?*anyopaque, _: Io) anyerror![]const u8 {
            const b: *[wt.BS]u8 = @ptrCast(@alignCast(ctx.?));
            return b;
        }
    };
    const sv: SlotVehicle = .{ .src = .{ .qid = 0, .buf_index = 5, .len = wt.BS }, .class = .page_cache, .commit_retain = false, .ctx = &held, .extract = &Ex.extract };
    const fallbacks0 = stats.overlay_store_fallbacks.load();
    const o = try w.ov.store(io, .{ .ino = 47, .block = 0, .rel = 0, .vehicle = .{ .slot = sv }, .file = TestWorld.striped(0), .overwrite = false });
    try testing.expect(o.stored.coverage_complete);
    try testing.expect(w.f.dev.writes.items[0].fixed);
    try testing.expectEqual(fallbacks0, stats.overlay_store_fallbacks.load());
    w.ov.drainBlock(io, &w.locks, 47, 0, false, w.cs());
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, w.f.map.seam().lookup(47, 0).?.offset, &got);
    try testing.expectEqualSlices(u8, &held, &got);
    // A failed direct store: the fallback DMA lands the same bytes.
    try w.f.map.create(48, .striped, 0);
    w.f.dev.fail_next = error.InputOutput;
    const fb0 = stats.overlay_store_fallbacks.load();
    const o2 = try w.ov.store(io, .{ .ino = 48, .block = 0, .rel = 0, .vehicle = .{ .slot = sv }, .file = TestWorld.striped(0), .overwrite = false });
    try testing.expect(o2.stored.coverage_complete);
    try testing.expectEqual(fb0 + 1, stats.overlay_store_fallbacks.load());
    w.ov.drainBlock(io, &w.locks, 48, 0, false, w.cs());
    // No direct leg: the install declines and the mint goes back.
    w.f.dev.no_write_fixed = true;
    w.ov.deps.device = w.f.dev.seam();
    try w.f.map.create(49, .striped, 0);
    const used = w.f.alloc_a.usedBlocks();
    const abandons0 = block_alloc.stats.unpublished_mint_abandons.load();
    try testing.expectEqual(StoreOutcome.declined, try w.ov.store(io, .{ .ino = 49, .block = 0, .rel = 0, .vehicle = .{ .slot = sv }, .file = TestWorld.striped(0), .overwrite = false }));
    try testing.expectEqual(abandons0 + 1, block_alloc.stats.unpublished_mint_abandons.load()); // the mint went back: the design working
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
}

test "a store whose DMA fails never joins coverage (law 3) and fails THIS write; a torn DMA leaves the uncovered range served from zeros; the record stays Open for the sibling" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(50, .striped, 0);
    var p: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&p, 0x99);
    _ = try w.ov.store(io, .{ .ino = 50, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.fail_next = error.InputOutput;
    w.f.dev.torn_grains = 3; // 3 pages land, then the error: a torn DMA
    try testing.expectError(error.InputOutput, w.ov.store(io, .{ .ino = 50, .block = 0, .rel = wt.BS / 2, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false }));
    const rec = w.ov.registry.get(.{ .ino = 50, .block = 0 }).?;
    try testing.expectEqual(State.open, rec.core.stateNow());
    try testing.expect(!rec.core.coveredProbe(wt.BS / 2 / OVERLAY_PAGE, 1));
    try testing.expect(rec.core.inflightEmpty());
    var out: [wt.BS]u8 = undefined;
    try testing.expectEqual(Overlay.ServeOutcome.served, w.ov.serve(io, 50, 0, 0, &out));
    try testing.expect(std.mem.allEqual(u8, out[wt.BS / 2 ..], 0)); // the torn bytes never escape
    // The sibling retries and completes.
    const o = try w.ov.store(io, .{ .ino = 50, .block = 0, .rel = wt.BS / 2, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    try testing.expect(o.stored.coverage_complete);
    w.ov.drainBlock(io, &w.locks, 50, 0, false, w.cs());
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
}

test "the settle's never-lossy and fence arms: a transient leaves the record FROZEN and retries on the ladder; a dead old binding supersedes and frees the dest; a fence publishes nothing, frees nothing (must-stay-0 fired); a reclaimed ino discards the orphan" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .shadow = false });
    defer w.deinit(testing.allocator);
    var p: [wt.BS]u8 align(4096) = undefined;
    pattern(&p, 0x42);
    // Transient (the injected seam): the write arm retries and lands.
    try w.f.map.create(60, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 60, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.ov.settle_transients.store(2, .monotonic);
    const r0 = stats.overlay_settle_retries.load();
    w.ov.drainBlock(io, &w.locks, 60, 0, false, w.cs());
    try testing.expectEqual(r0 + 2, stats.overlay_settle_retries.load());
    try testing.expect(w.f.map.seam().lookup(60, 0) != null);
    // Shadow off: the overwrite publishes through the durable-merge arm and frees the displaced key only after.
    const a = try w.f.publishBlock(61, 0, 0xAA);
    _ = try w.ov.store(io, .{ .ino = 61, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    const ff0 = stats.overlay_feed_fallbacks.load();
    w.ov.drainBlock(io, &w.locks, 61, 0, false, w.cs());
    try testing.expectEqual(ff0 + 1, stats.overlay_feed_fallbacks.load());
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a));
    // Dead old binding: the displaced key's lifetime ended under us (a truncate freed it) ⇒ supersede, dest freed.
    const a2 = try w.f.publishBlock(62, 0, 0xBB);
    _ = try w.ov.store(io, .{ .ino = 62, .block = 0, .rel = 0, .vehicle = .{ .bytes = p[0 .. wt.BS / 2] }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    _ = try w.f.alloc_a.freeBlock(w.f.alloc_a.idxOf(a2.offset));
    const used = w.f.alloc_a.usedBlocks();
    const dead0 = stats.overlay_superseded_dead_old_binding.load();
    w.ov.drainBlock(io, &w.locks, 62, 0, false, w.cs());
    try testing.expectEqual(dead0 + 1, stats.overlay_superseded_dead_old_binding.load());
    try testing.expectEqual(used - 1, w.f.alloc_a.usedBlocks()); // the dest went back (unpublished_mint_abandons is the design working)
    // Fence: must-stay-0 fires, nothing published, nothing freed, the dest NOT freed (successor recovery owns it).
    try w.f.map.create(63, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 63, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.map.fail_next_merge = error.WriterGuardFenced;
    const merges = w.f.map.merges.items.len;
    const used2 = w.f.alloc_a.usedBlocks();
    const fence0 = counters.tripwireCount("overlay_fence_drops");
    stats.overlay_fence_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    w.ov.drainBlock(io, &w.locks, 63, 0, false, w.cs());
    try log.testing.expectAllFired();
    try testing.expectEqual(fence0 + 1, counters.tripwireCount("overlay_fence_drops"));
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    try testing.expectEqual(used2, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    // Reclaimed ino: the orphan dest is discarded (freed), never resurrected.
    try w.f.map.create(64, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 64, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.map.remove(64);
    const used3 = w.f.alloc_a.usedBlocks();
    const orph0 = stats.writeback_orphan_discards_overlay.load();
    w.ov.drainBlock(io, &w.locks, 64, 0, false, w.cs());
    try testing.expectEqual(orph0 + 1, stats.writeback_orphan_discards_overlay.load());
    try testing.expectEqual(used3 - 1, w.f.alloc_a.usedBlocks());
}

// The 2026-10-07 release wedge, deterministically: a settler whose gap
// seed keeps failing (the venue's device answered EIO on every write)
// must back off with the block guard DROPPED, so a sibling (the next
// RELEASE's drain, a write to the block) takes the guard between
// attempts instead of parking behind a sleeper nobody can name.
test "the settle's backoff never parks under (3): a device that keeps failing the gap seed leaves the record FROZEN never-lossy and the settler sleeps with the guard dropped — a sibling takes the guard mid-ladder, a store's steal is ONE attempt; when the device returns the settle lands once with the gap zero-seeded" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .shadow = false });
    defer w.deinit(testing.allocator);
    var p: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&p, 0x5A);
    try w.f.map.create(90, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 90, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.setFailWrites(std.math.maxInt(u32), error.InputOutput);
    const r0 = stats.overlay_settle_retries.load();
    const pub0 = stats.overlay_publishes.load();
    const seeds0 = stats.overlay_gap_seeds.load();
    const Drainer = struct {
        fn run(world: *TestWorld, io_: Io) void {
            world.ov.drainBlock(io_, &world.locks, 90, 0, false, world.cs());
        }
    };
    const t = try std.Thread.spawn(.{}, Drainer.run, .{ w, io });
    // The settler has failed at least once and is on its ladder.
    var deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (stats.overlay_settle_retries.load() < r0 + 1) {
        if (clock.monotonicNs() > deadline) break;
        try std.Thread.yield();
    }
    // The sibling takes the guard between attempts (bounded by the
    // ladder's 50 ms cap, generously); a settler sleeping under (3) never
    // lets it through.
    var guard: ?seams.BlockLocks.Guard = null;
    deadline = clock.monotonicNs() + 3 * std.time.ns_per_s;
    while (guard == null and clock.monotonicNs() < deadline) {
        guard = w.locks.tryAcquire(io, 90, 0);
        if (guard == null) try std.Thread.yield();
    }
    const taken = guard != null;
    var frozen_under_guard = false;
    var steal_declined = false;
    var retries_at_steal: u64 = 0;
    if (guard) |g| {
        const rec = w.ov.registry.get(.{ .ino = 90, .block = 0 });
        frozen_under_guard = rec != null and rec.?.core.stateNow() == .frozen;
        // Under OUR guard: a store meeting the frozen record steals ONE
        // attempt and declines — never the ladder under the caller's lock.
        const r1 = stats.overlay_settle_retries.load();
        const o = try w.ov.store(io, .{ .ino = 90, .block = 0, .rel = wt.BS / 2, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(0), .overwrite = false });
        steal_declined = o == .declined;
        retries_at_steal = stats.overlay_settle_retries.load() - r1;
        // The device returns; the settler's next attempt lands.
        w.f.dev.setFailWrites(0, error.InputOutput);
        g.release(io);
    } else {
        w.f.dev.setFailWrites(0, error.InputOutput);
    }
    t.join();
    try testing.expect(taken);
    try testing.expect(frozen_under_guard);
    try testing.expect(steal_declined);
    try testing.expectEqual(@as(u64, 1), retries_at_steal);
    try testing.expectEqual(pub0 + 1, stats.overlay_publishes.load());
    try testing.expectEqual(seeds0 + 1, stats.overlay_gap_seeds.load());
    const b = w.f.map.seam().lookup(90, 0) orelse return error.TestUnexpectedResult;
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &p, got[0 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
}

test "fence immediate on every settle leg: a gap seed or a flush answering VolumeFailed/WriterGuardFenced tears the record down FenceDropped at once — no retry, nothing published, the dest not freed, the must-stay-0 fired" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{ .shadow = false });
    defer w.deinit(testing.allocator);
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    pattern(&half, 0x3C);
    var whole: [wt.BS]u8 align(4096) = undefined;
    pattern(&whole, 0x3D);
    // The gap seed meets the fence.
    try w.f.map.create(91, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 91, .block = 0, .rel = 0, .vehicle = .{ .bytes = &half }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.fail_next = error.VolumeFailed;
    var fence0 = counters.tripwireCount("overlay_fence_drops");
    var retries0 = stats.overlay_settle_retries.load();
    var used0 = w.f.alloc_a.usedBlocks();
    stats.overlay_fence_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    w.ov.drainBlock(io, &w.locks, 91, 0, false, w.cs());
    try log.testing.expectAllFired();
    try testing.expectEqual(fence0 + 1, counters.tripwireCount("overlay_fence_drops"));
    try testing.expectEqual(retries0, stats.overlay_settle_retries.load());
    try testing.expect(w.f.map.seam().lookup(91, 0) == null);
    try testing.expectEqual(used0, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    // The fsync-class flush meets the fence.
    try w.f.map.create(92, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 92, .block = 0, .rel = 0, .vehicle = .{ .bytes = &whole }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.fail_next_flush = error.WriterGuardFenced;
    fence0 = counters.tripwireCount("overlay_fence_drops");
    retries0 = stats.overlay_settle_retries.load();
    used0 = w.f.alloc_a.usedBlocks();
    stats.overlay_fence_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    w.ov.drainBlock(io, &w.locks, 92, 0, true, w.cs());
    try log.testing.expectAllFired();
    try testing.expectEqual(fence0 + 1, counters.tripwireCount("overlay_fence_drops"));
    try testing.expectEqual(retries0, stats.overlay_settle_retries.load());
    try testing.expect(w.f.map.seam().lookup(92, 0) == null);
    try testing.expectEqual(used0, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(@as(u32, 0), w.ov.registry.open_count.load(.monotonic));
    // A plain EIO on the same legs stays a transient the ladder retries (the never-lossy class).
    try w.f.map.create(93, .striped, 0);
    _ = try w.ov.store(io, .{ .ino = 93, .block = 0, .rel = 0, .vehicle = .{ .bytes = &half }, .file = TestWorld.striped(0), .overwrite = false });
    w.f.dev.fail_next = error.InputOutput;
    retries0 = stats.overlay_settle_retries.load();
    w.ov.drainBlock(io, &w.locks, 93, 0, false, w.cs());
    try testing.expectEqual(retries0 + 1, stats.overlay_settle_retries.load());
    try testing.expect(w.f.map.seam().lookup(93, 0) != null);
}

test "crash windows OW-1..OW-8 as RAM cases: an unpublished dest is reclaimable by the census (refcount 1, never named by the map), the old image intact; the fed-but-unsaved epoch ≡ OW-1; a swap durable with frees not run leaves a tracked A key the census free-lists; OW-7 durably written ≠ durably named" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    var p: [wt.BS]u8 align(4096) = undefined;
    pattern(&p, 0x5A);
    // OW-1: ACK, DMA landed, no publish — crash here (a half block: the
    // coverage stays incomplete so the feed does not auto-close).
    const a = try w.f.publishBlock(70, 0, 0xAA);
    _ = try w.ov.store(io, .{ .ino = 70, .block = 0, .rel = 0, .vehicle = .{ .bytes = p[0 .. wt.BS / 2] }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    const rec = w.ov.registry.get(.{ .ino = 70, .block = 0 }).?;
    const dest_idx = rec.dest_idx;
    // The map still binds A (durable truth); the dest is allocated but the map never named it ⇒ a census frees it.
    try testing.expect(w.f.map.seam().lookup(70, 0).?.eql(a));
    try testing.expect(w.f.alloc_a.inflightContains(dest_idx)); // visible to a LIVE fsck as in flight
    var old: [wt.BS]u8 = undefined;
    w.f.dev.peek(wt.VOL_A, a.offset, &old);
    try testing.expect(std.mem.allEqual(u8, &old, 0xAA)); // the old image intact
    // OW-2: fed, no save ≡ OW-1 — the map still binds A after the feed.
    w.ov.drainBlock(io, &w.locks, 70, 0, false, w.cs());
    try testing.expect(w.f.map.seam().lookup(70, 0).?.eql(a));
    try testing.expect(w.epochs.bindsBlock(70, 0));
    try testing.expect(w.f.alloc_a.inflightContains(dest_idx)); // the guard travelled record → epoch
    // OW-5: swap durable, frees not run — the save landed, A still allocated until the free runs (the census free-lists a tracked key the map no longer names).
    const closed = w.epochs.close(io, 70, w.cs());
    try testing.expect(closed == .swapped);
    try testing.expect(!w.f.map.seam().lookup(70, 0).?.eql(a));
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    // OW-7: mid-fsync between the data barrier and the meta publish — the dest is durably WRITTEN but not NAMED: the map still binds the old key, the dest is an unpublished mint.
    const a2 = try w.f.publishBlock(71, 0, 0xBB);
    _ = try w.ov.store(io, .{ .ino = 71, .block = 0, .rel = 0, .vehicle = .{ .bytes = &p }, .file = TestWorld.striped(wt.BS), .overwrite = true });
    const rec2 = w.ov.registry.get(.{ .ino = 71, .block = 0 }).?;
    try testing.expectEqual(State.frozen, rec2.core.stateNow()); // the full-coverage CQE froze it
    try w.f.dev.seam().flush(wt.VOL_A); // the data barrier
    try testing.expect(w.f.map.seam().lookup(71, 0).?.eql(a2));
    try testing.expectEqual(@as(?u64, null), w.f.alloc_a.fillIncarnation(rec2.dest_idx)); // unstable = unpublished
    w.ov.drainBlock(io, &w.locks, 71, 0, false, w.cs());
    _ = w.epochs.close(io, 71, w.cs());
}

test "ack_early_write_returns_before_store_cqe; fsync_awaits_acked_inflight_store; read_waits_for_acked_inflight_store (the future law); the payload is released exactly once" {
    const io = testing.io;
    const w = try TestWorld.init(testing.allocator, .{});
    defer w.deinit(testing.allocator);
    try w.f.map.create(80, .striped, 0);
    var held: [wt.BS]u8 align(4096) = undefined;
    pattern(&held, 0x3C);
    try w.f.dev.registerSlot(9, &held);
    const Gate = struct {
        var hold: std.atomic.Value(bool) = .init(true);
        var released: std.atomic.Value(u32) = .init(0);
        var landed: std.atomic.Value(bool) = .init(false);
        fn hook(_: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
            // Hold the CQE until the test lets it go.
            while (hold.load(.acquire)) std.Thread.yield() catch {};
            landed.store(true, .release);
        }
        fn release(_: ?*anyopaque) void {
            _ = released.fetchAdd(1, .acq_rel);
        }
    };
    var dummy: u8 = 0;
    w.f.dev.hook = .{ .ctx = &dummy, .call = &Gate.hook };
    const early0 = stats.overlay_ack_early_stores.load();
    const lost0 = counters.tripwireCount("overlay_ack_early_lost");
    const sv: SlotVehicle = .{ .src = .{ .qid = 0, .buf_index = 9, .len = wt.BS }, .class = .page_cache, .commit_retain = true, .release = &Gate.release };
    const o = w.ov.store(io, .{ .ino = 80, .block = 0, .rel = 0, .vehicle = .{ .slot = sv }, .file = TestWorld.striped(0), .overwrite = false }) catch |e| switch (e) {
        else => return e,
    };
    if (!o.stored.acked_early) {
        // No unit of concurrency on this executor: the sound fallback acked after the CQE.
        Gate.hold.store(false, .release);
        w.ov.drainBlock(io, &w.locks, 80, 0, false, w.cs());
        return;
    }
    // ACKed before the CQE landed.
    try testing.expect(!Gate.landed.load(.acquire));
    try testing.expectEqual(early0 + 1, stats.overlay_ack_early_stores.load());
    const rec = w.ov.registry.get(.{ .ino = 80, .block = 0 }).?;
    try testing.expect(rec.core.rangeInflight(0, 1));
    // A reader intersecting the acked store WAITS for it (never an uncovered gap); the fsync drain awaits it too.
    const Reader = struct {
        fn run(io_: Io, ov: *Overlay, out: *[wt.BS]u8, res: *Overlay.ServeOutcome) void {
            res.* = ov.serve(io_, 80, 0, 0, out);
        }
    };
    var out: [wt.BS]u8 = undefined;
    var res: Overlay.ServeOutcome = .none;
    const th = try std.Thread.spawn(.{}, Reader.run, .{ io, &w.ov, &out, &res });
    while (rec.inflight_change.waiters(io) == 0) try std.Thread.yield();
    Gate.hold.store(false, .release);
    th.join();
    try testing.expectEqual(Overlay.ServeOutcome.served, res);
    try testing.expectEqualSlices(u8, &held, &out);
    w.ov.drainBlock(io, &w.locks, 80, 0, true, w.cs());
    try testing.expect(w.f.map.seam().lookup(80, 0) != null);
    try w.ov.tasks.wait(io);
    try testing.expectEqual(@as(u32, 1), Gate.released.load(.acquire));
    try testing.expectEqual(lost0, counters.tripwireCount("overlay_ack_early_lost"));
    w.f.dev.hook = null;
}

test {
    _ = @import("device_overlay_model.zig");
}
