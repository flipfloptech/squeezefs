//! The data-block allocator — one per DATA volume (charter §6.2 "Block
//! reclaim", §5.5 "On a grant-armed allocator the bitmap IS the free list",
//! §6.1 striped placement; `docs/porting/data_alloc_reclaim.md` §1, §4.1,
//! §5, §10 Q3/Q8/Q9/Q11/Q13, §11). PORTABLE: no device, no `Io` — the
//! reclaim module (`reclaim.zig`) owns the device commands and the waits,
//! the ENOSPC law (`enospc.zig`) owns the parks.
//!
//! ONE dense per-block cell table + ONE free BITMAP replace the reference's
//! six offset-keyed hash maps (decision 0002 ledger, Q3): `cells[idx]` is
//! the block's whole state — the incarnation word, the lifetime stamp, the
//! RAM refcount, the fsck in-flight registrations, the state byte and the
//! flag byte (24 B/block; 25 M blocks of a 100 TB volume at 4 MiB = 600 MB,
//! charged to the budget as a non-sheddable component) — and a bit of
//! `bits` is SET exactly while the block is NOT reallocatable now
//! (allocated, in limbo between `beginFree` and `finishFree`, or inside a
//! trim claim window). Below the fresh-mint cursor a CLEAR bit IS the free
//! list; at/above it a clear bit is VIRGIN. This is rung 6's "the bitmap is
//! the free list" shape from birth (§11 items 2, 5): the holder's bitmap
//! and the local free list are one structure, so the finish-free hook
//! becomes "journal the delta".
//!
//! Claim = budget-then-bit, mirrored from `meta/alloc_core.zig` (never
//! shared with it): a `cmpxchg` on `free_budget` wins an entitlement, then
//! a scan `fetchOr`s the lowest clear bit below the cursor; every release
//! path CLEARS THE BIT BEFORE CREDITING THE BUDGET so the winner's clear bit
//! is already visible when its scan runs. A lost race rescans (never falls
//! to the fresh mint: the reference's lost race on a cursor-at-cap store
//! fired the ENOSPC valve per allocation and cost −33 % rewrite, BA:L2387).
//! Every release path also stores the cell's STATE BEFORE the bit: the
//! clear's acq_rel is the release a claimant's acq_rel take acquires, so
//! the claimant's own state store lands after it (the reverse order let a
//! claim between the two end at `free` on a live block). Every consumer
//! wins its entitlement BEFORE it takes a bit (a bit taken first could
//! debit a credit not yet landed) — except the holder's carve, which takes
//! its bits under the holder's ordering lock, the lock the publication's
//! bit and credit move under (`free_list_model.zig`, 0003 §W.1).
//!
//! The incarnation word (`gen << 1 | stable`) is a seqlock per block
//! (`IC:L1-83`). Zig 0.17 has no standalone fence (`@fence` left in 0.14),
//! so the two "fence" halves are expressed on the word itself: the WRITER
//! half is the `retire` RMW at SeqCst (its store is drained before any
//! later access; the caller's data write is a DMA behind a syscall, a full
//! barrier on every architecture); the READER half (`still`) is an acq_rel
//! RMW on the word — its release side orders the caller's prior payload
//! loads before it, which an acquire LOAD would not (an `ldar` lets an
//! earlier plain load pass it on ARMv8). The cross-word pair of W1
//! (`pinBlockValidated` / `beginPatchSoleOwner`) is four SeqCst accesses
//! over the two words, the Dekker shape C11's total order forbids from
//! both reading stale (`fuse/transport/slot.zig` records the same ruling).
//! `incarnation_model.zig` keeps every fence as an explicit store-buffer
//! drain step and proves each weakening fails.
//!
//! Every mutation of ownership accounting walks ONE `planeGate` (§11 item
//! 1): rung 4 implements the read-only arm and leaves the lease arm a hook
//! defaulting to "held". Every other rung-6 seam here is a `?vtable` whose
//! absent arm is one pointer test — the reference's "byte-identical unarmed
//! path" discipline (BA:L735-739).

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const errors = @import("../core/errors.zig");
const derive = @import("../core/derive.zig");
const budget = @import("../mem/budget.zig");
const sync_notify = @import("../sync/notify.zig");

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// The allocation stride and the physical block: offsets are minted as
/// `idx × block_size`; an image longer than it tramples the next block
/// (FIND-RW4-A, BA:L106-141). Configurable per volume at format; 4 MiB is
/// the charter's shipped stride.
pub const CHUNK_SIZE: u64 = 4 * derive.MiB;

/// Full-bitmap rescans a claim makes before declaring drift: a rescan is
/// only ever needed because a concurrent release landed behind the cursor
/// (O(racing releases)); 64 is a generous ceiling on that, not a knob, and
/// the pre-fix loop had no exit at all (RES-14, mirrored from alloc_core).
pub const claim_rescan_limit: u32 = 64;

/// The lifetime stamp is `era << 40 | seq`: 40 seq bits = 2^40 − 1 fresh
/// mints per era = 4 EiB of fresh blocks at 4 MiB per mount (RT:L2110-2137);
/// the era is the appender's durable term (rung 6; rung 4 engages with 1).
pub const STAMP_SEQ_BITS: u6 = 40;
pub const STAMP_SEQ_MAX: u64 = (1 << STAMP_SEQ_BITS) - 1;
/// Stamp 0 = no lifetime recorded (the unstamped POINT/RUN mapping).
pub const STAMP_NONE: u64 = 0;

/// The free-forensics tape: a double-release investigation needs only the
/// recent frees (BA:L5-12); 4096 records × 32 B = 128 KiB when armed.
pub const FREE_FORENSICS_TAPE_CAP: usize = 4096;

/// Incarnation word constants (IC:L27-33).
pub const UNSTABLE_FIRST: u64 = 1 << 1;
pub const STABLE_FIRST: u64 = 1;
/// Snapshot sentinel for a block whose word was never touched: treated
/// stable (a never-written block has no mid-flight bytes to serve).
pub const UNKNOWN_STABLE: u64 = std.math.maxInt(u64);

// ---------------------------------------------------------------------------
// The incarnation word — the seqlock, every atomic with its pairing.
// ---------------------------------------------------------------------------

pub const Incarnation = struct {
    /// `retire`: `gen + 1`, stable bit CLEARED. SeqCst RMW = the writer half
    /// of the seqlock AND the W1 patch side's half of the cross-word pair
    /// (paired with `refcountPeekFenced`'s SeqCst load and the clone side's
    /// SeqCst `tryAcquire` + `snapshotFenced`).
    pub fn retire(word: *Atomic(u64)) void {
        var cur = word.load(.monotonic);
        while (true) {
            const next = ((cur >> 1) + 1) << 1;
            cur = word.cmpxchgWeak(cur, next, .seq_cst, .monotonic) orelse return;
        }
    }

    /// `publish`: set the stable bit under the current generation. AcqRel:
    /// orders the caller's completed DMA (a syscall-fenced write) before the
    /// stable bit a reader's `snapshot` acquires.
    pub fn publish(word: *Atomic(u64)) void {
        _ = word.fetchOr(1, .acq_rel);
    }

    /// `snapshot`: the word if stable, else null. Acquire pairs with
    /// `publish`'s AcqRel: a stable word means the bytes it covers landed.
    pub fn snapshot(word: *const Atomic(u64)) ?u64 {
        const w = word.load(.acquire);
        return if (w & 1 == 0) null else w;
    }

    /// The clone side's snapshot: SeqCst so it is the fourth access of the
    /// cross-word Dekker pair (after the SeqCst `tryAcquire`).
    pub fn snapshotFenced(word: *const Atomic(u64)) ?u64 {
        const w = word.load(.seq_cst);
        return if (w & 1 == 0) null else w;
    }

    /// `still`: the reader half. An acq_rel RMW on the word: the release
    /// side orders the caller's prior payload loads before it (Boehm's
    /// seqlock paper — an acquire LOAD only orders later accesses; x86/TSO
    /// masks the bug, ARM/POWER do not), and it reads the word.
    pub fn still(word: *Atomic(u64), before: u64) bool {
        return word.fetchOr(0, .acq_rel) == before;
    }

    pub fn generation(word: u64) u64 {
        return word >> 1;
    }

    pub fn isStable(word: u64) bool {
        return word & 1 != 0;
    }
};

// ---------------------------------------------------------------------------
// The refcount cell protocol (RC:L1-62).
// ---------------------------------------------------------------------------

pub const Refcount = struct {
    pub fn peek(cell: *const Atomic(u32)) u32 {
        return cell.load(.acquire);
    }

    /// The patch side's peek: SeqCst, the second access of its half of the
    /// cross-word pair (after the SeqCst `retire`).
    pub fn peekFenced(cell: *const Atomic(u32)) u32 {
        return cell.load(.seq_cst);
    }

    /// CAS up from NONZERO only; false at 0 (a freed block is never
    /// resurrected by a late pin). SeqCst: the clone side's first access of
    /// the cross-word pair.
    pub fn tryAcquire(cell: *Atomic(u32)) bool {
        var cur = cell.load(.monotonic);
        while (true) {
            if (cur == 0) return false;
            cur = cell.cmpxchgWeak(cur, cur + 1, .seq_cst, .monotonic) orelse return true;
        }
    }

    /// CAS down; true exactly on the 1→0 transition; saturates at 0 (a
    /// second release of a freed cell is false, never a wrap).
    pub fn release(cell: *Atomic(u32)) bool {
        var cur = cell.load(.monotonic);
        while (true) {
            if (cur == 0) return false;
            cur = cell.cmpxchgWeak(cur, cur - 1, .acq_rel, .monotonic) orelse return cur == 1;
        }
    }

    /// CAS down from ≥ 2 only; false (the cell untouched) at 1 or 0 — the
    /// caller's reference is the LAST one (or none is tracked), so the
    /// terminal decision is not this caller's to take alone. Same orderings
    /// as `release` (the decrement publishes the caller's prior writes to
    /// the eventual 1→0 releaser).
    pub fn releaseUnlessLast(cell: *Atomic(u32)) bool {
        var cur = cell.load(.monotonic);
        while (true) {
            if (cur <= 1) return false;
            cur = cell.cmpxchgWeak(cur, cur - 1, .acq_rel, .monotonic) orelse return true;
        }
    }
};

// ---------------------------------------------------------------------------
// The dense cell.
// ---------------------------------------------------------------------------

/// Where a block is in its life. `virgin` (never minted) and `free` share a
/// CLEAR bit; the three set-bit states differ in who owns the block.
pub const State = enum(u8) {
    virgin = 0,
    free = 1,
    allocated = 2,
    /// `beginFree` ran (incarnation retired, refcount 0), `finishFree` has
    /// not: the reclaimer's durability hold, the queue, the elision path or
    /// the debt drainer owns it.
    limbo = 3,
    /// Taken out of the free list by a trim claim window; pending supply,
    /// never a deficit.
    trim_windowed = 4,
    /// Rung 6: a dead-epoch do-not-reallocate hold; `finishFree` defers.
    quarantined = 5,
};

pub const FLAG_ELIDED_DEBT: u8 = 1 << 0;
pub const FLAG_SHARED: u8 = 1 << 1;
/// Rung 6 (§4.4): the block belongs to a dead-epoch quarantine cohort. The
/// epoch itself rides the `stamp` word while the cell is `State.quarantined`
/// (the stamp is dead there: nothing minted the block under this holder);
/// a LIVE owner's block carries the flag alone and only gates its future
/// free (`custody.BlockQuarantine` keeps that cohort's epoch).
pub const FLAG_QUARANTINED: u8 = 1 << 2;
/// A `finishFree` the quarantine deferred: its publish is OWED to the
/// cohort's release.
pub const FLAG_QUARANTINE_OWED: u8 = 1 << 3;

pub const Cell = struct {
    /// `gen << 1 | stable` (module doc).
    incarnation: Atomic(u64) = .init(0),
    /// The lifetime stamp a map entry carries (`STAMP_NONE` = none).
    stamp: Atomic(u64) = .init(STAMP_NONE),
    /// RAM refcount; 0 = untracked.
    refcount: Atomic(u32) = .init(0),
    /// fsck visibility registrations (`InflightAllocGuard`).
    inflight: Atomic(u16) = .init(0),
    state: Atomic(u8) = .init(@backingInt(State.virgin)),
    flags: Atomic(u8) = .init(0),

    fn getState(self: *const Cell) State {
        return @fromBackingInt(@intCast(self.state.load(.acquire)));
    }

    fn setState(self: *Cell, s: State) void {
        self.state.store(@backingInt(s), .release);
    }

    /// The quarantine gate's Dekker pair (0003 §Y.3): the publication's
    /// state store then its flag re-check, against the admission's flag RMW
    /// then its state read — all four seq_cst, so at least one side sees
    /// the other (the store-buffering outcome is forbidden).
    fn setStateSeqCst(self: *Cell, s: State) void {
        self.state.store(@backingInt(s), .seq_cst);
    }

    fn getStateSeqCst(self: *const Cell) State {
        return @fromBackingInt(@intCast(self.state.load(.seq_cst)));
    }
};

comptime {
    // The dense table's per-block cost — the Q3 row's "24–32 B/block".
    assert(@sizeOf(Cell) == 24);
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

pub const AllocError = error{
    /// The allocator's own TERMINAL verdict: no free-listed block, no virgin
    /// block, no open grant window.
    StorageFull,
    /// A read-only mount refuses every ownership mutation.
    ReadOnly,
    /// Rung 6: this mount does not hold the volume's allocation lease, or
    /// its grant window is empty with the top-up owed by an unreachable or
    /// deferred holder; rung 8: its mint source is disarmed while a carved
    /// window holds the frontier (retryable; never latches `StorageFull`).
    PlaneRefused,
    /// `allocateSpecificBlock` on a block that is not free.
    NotFree,
};

/// The ONE table's names: `StorageFull` is ENOSPC (terminal), a plane
/// refusal is EAGAIN-class (an unreachable holder is retryable, charter
/// §6.2), read-only is EROFS, a specific claim of a taken block EINVAL.
pub fn coreError(e: AllocError) errors.Error {
    return switch (e) {
        error.StorageFull => error.NoSpaceLeft,
        error.ReadOnly => error.ReadOnlyFileSystem,
        error.PlaneRefused => error.WouldBlock,
        error.NotFree => error.InvalidArgument,
    };
}

// ---------------------------------------------------------------------------
// Rung-6 seams (interfaces only; §11).
// ---------------------------------------------------------------------------

/// What an ownership mutation is, for the plane gate and the forensics tape.
pub const Site = enum(u8) {
    mint,
    terminal_free,
    specific_claim,
    seed_free,
    patch_retire,
    trim_claim,
    abandon,
};

/// §11 item 1: the lease arm of the plane gate. Absent ⇒ "held".
pub const PlaneGate = struct {
    ctx: ?*anyopaque,
    holds: *const fn (?*anyopaque, Site) bool,
};

/// §11 item 2: on an armed allocator a terminal free CLEARS THE HOLDER'S
/// BIT and journals the kind-5 CLEAR delta instead of (as well as, in the
/// dense shape) the local insert; `false` ⇒ the bit was already clear — a
/// DOUBLE FREE.
/// `test_finish_station`'s hook: the finish of `idx` is past its gate.
pub const FinishStation = struct {
    ctx: *anyopaque,
    passedGate: *const fn (ctx: *anyopaque, idx: u64) void,
};

pub const FinishFreeSink = struct {
    ctx: ?*anyopaque,
    noteFinishFree: *const fn (?*anyopaque, vol_tag: u64, idx: u64) bool,
    /// A non-carve take — the trim claim, the quarantine's free and virgin
    /// arms, a publication's take-back — run under the holder's ordering
    /// lock: `holderTakeOutOfList` inside the lock the carve takes, so the
    /// carve never meets a bit taken between its scan and its take nor an
    /// entitlement won ahead of its debit (0003 §Y.4). `holds` = the bit
    /// stays set in the holder's bitmap (its population and a SET delta
    /// move with it); a trim window's transient take moves neither. Null =
    /// the allocator takes in place (a sink with no lock to share).
    takeOutOfList: ?*const fn (?*anyopaque, vol_tag: u64, idx: u64, listed: bool, holds: bool) bool = null,
    /// A trim window's return under the same lock: `holderReturnTrim`.
    returnTrim: ?*const fn (?*anyopaque, vol_tag: u64, idx: u64) bool = null,
};

/// A grant-armed mint's answer: the block, or WHY the window is empty —
/// `full` is the holder's terminal word (no clear bit remains: this
/// allocator's `StorageFull`, latched), `retry` the retryable class (the
/// holder unreachable or deferred, a top-up owed: `PlaneRefused`, never a
/// latch — "a manager failover on a joined writer with a drained window").
pub const MintAnswer = union(enum) { minted: u64, full, retry };

/// §11 item 3: the ranged-grant window as the fresh-mint source. Absent ⇒
/// the local cursor.
pub const GrantSource = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// The next block of the window (the PULL folded in), or the
        /// class of its emptiness.
        mint: *const fn (*anyopaque) MintAnswer,
        /// An unpublished mint returns to its window; false ⇒ not ours.
        giveBack: *const fn (*anyopaque, u64) bool,
        remaining: *const fn (*anyopaque) u64,
        /// The window crossed its 50 % pull mark.
        wantsTopup: *const fn (*anyopaque) bool,
        /// A `StorageFull` landed on an empty window (the proactive re-ask).
        noteExhausted: *const fn (*anyopaque) void,
    };
};

/// §11 item 6: the holder's view for the supply arithmetic — `df`'s used
/// count is the bitmap population minus the holder's own unconsumed window;
/// the free supply is the CLEAR population plus that window; `frontier` =
/// the highest set bit + 1 (the holder's virgin tail starts there — the
/// elision watermark on a holder reads it, `sym_data_planes.md` §4.3 item 2).
/// A JOINER's view is the holder's last word (`population`, `blocks`) with
/// its own window.
pub const HolderView = struct { bitmap_blocks: u64, population: u64, window_remaining: u64, frontier: u64 };

/// §11 items 4, 6: the allocation lease.
pub const AllocationLease = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        holderView: *const fn (*anyopaque) ?HolderView,
        /// The mount seed handed over as ONE bitset so the arm never walks
        /// the ledger twice.
        noteMountSeed: *const fn (*anyopaque, vol_tag: u64, blocks: u64, set_words: []const u64) void,
    };
};

/// §11 item 11: the free-grace ring. Absent ⇒ `defer` is false, the
/// harvest a no-op, `holds` false (one pointer test on every path).
pub const GraceRing = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        defer_: *const fn (*anyopaque, idx: u64, bytes: u64) bool,
        harvest: *const fn (*anyopaque, max: usize, out: []u64) usize,
        holds: *const fn (*anyopaque, idx: u64) bool,
        len: *const fn (*anyopaque) u64,
    };
};

/// §11 item 8: a SHARED block's terminal free is decided at the index home.
pub const SharedVerdict = union(enum) { held: u32, freed, not_shared, unreachable_home };
pub const SharedFreeGate = struct {
    ctx: *anyopaque,
    ask: *const fn (*anyopaque, vol_tag: u64, idx: u64) SharedVerdict,
};

/// §11 item 9: an untracked free at a grant-armed holder is adjudicated
/// through the owner ladder instead of refused.
pub const UntrackedFreeGate = struct {
    ctx: *anyopaque,
    adjudicate: *const fn (*anyopaque, vol_tag: u64, idx: u64) bool,
};

/// "Reclaim still owed" — wired by the reclaim module; the valve and the
/// visibility predicate read it (BA:L245-260, L683-688).
pub const SpacePending = struct {
    ctx: ?*anyopaque,
    pending: *const fn (?*anyopaque) bool,
};

// ---------------------------------------------------------------------------
// Stats (spec §7; names are the registry).
// ---------------------------------------------------------------------------

pub const stats = struct {
    // Must-stay-0 tripwires.
    pub var block_double_frees: counters.Tripwire = .init("block_double_frees");
    pub var block_untracked_free_refusals: counters.Tripwire = .init("block_untracked_free_refusals");
    pub var block_claim_anomalies: counters.Tripwire = .init("block_claim_anomalies");
    pub var block_key_incarnation_exhausted: counters.Tripwire = .init("block_key_incarnation_exhausted");
    pub var block_live_free_refusals: counters.Tripwire = .init("block_live_free_refusals");
    /// An in-flight registration dropped with none held (decision 0003
    /// §AH.1: a release's hold and its drop are one pairing).
    pub var block_inflight_unheld_drops: counters.Tripwire = .init("block_inflight_unheld_drops");
    /// An unpublished mint returned by its rollback owner: a failed DMA, a
    /// publish refused, or an orphan's discard (an event count and never a
    /// tripwire; `≤ write_pipeline_discards` on a healthy device is the
    /// smoke's law — a superseded write-through publishes its snapshot,
    /// decision 0003 §AJ.7).
    pub var unpublished_mint_abandons: counters.ShardedCounter = .{};
    // Per-op counters.
    pub var alloc_from_freelist: counters.ShardedCounter = .{};
    pub var alloc_fresh_mints: counters.ShardedCounter = .{};
    pub var del_obj: counters.ShardedCounter = .{};
    pub var block_pin_stale_keys: counters.ShardedCounter = .{};
    pub var block_key_incarnation_unknown: counters.ShardedCounter = .{};
    pub var unpublished_offsets_recovered: counters.ShardedCounter = .{};
    pub var block_untracked_free_adjudicated: counters.ShardedCounter = .{};
    pub var shared_release_failures: counters.ShardedCounter = .{};
    pub var meta_kv_block_refs_recovered: counters.ShardedCounter = .{};
    /// Rung 6 (§3.6, §4.4): blocks admitted to a dead-epoch cohort, cohort
    /// releases, and `finishFree`s the cohort deferred (owed at release).
    pub var dlm_quarantined_offsets: counters.ShardedCounter = .{};
    pub var dlm_quarantine_releases: counters.ShardedCounter = .{};
    pub var block_quarantine_deferred_frees: counters.ShardedCounter = .{};
    /// A publication's re-check found its block admitted after the gate
    /// and took it back OWED (0003 §Y.3).
    pub var block_quarantine_takebacks: counters.ShardedCounter = .{};
    /// An allocation that met `StorageFull` with a trim claim window open
    /// and parked for the window's return (`enospc.awaitTrimReturn`): a
    /// full store under rewrite holds its free list inside the windows the
    /// trim passes claim, so the park is that regime's ordinary outcome —
    /// counted, never a tripwire (the fence-class ruling of 2026-10-19
    /// applied; the must-stay-0 it was fired ×3 under generic/750 on a
    /// 16 GiB scratch and ×7 under generic/475). ≈ 0 off that regime.
    pub var alloc_trim_window_parks: counters.ShardedCounter = .{};
    /// A park that reached the ceiling (`reclaim.PARK_BOUND_CEILING_MS`)
    /// with the window still open: a trim pass holds its window for about
    /// one manners tick (reclaim.zig's pacing), so the ceiling passing is
    /// the device stalled on its discards, not the store full — one warning
    /// per episode on the volume, the park continues (a window is supply).
    pub var alloc_trim_window_park_walls: counters.ShardedCounter = .{};
    /// Levels.
    pub var alloc_fresh_supply_exhausted_latched: counters.Gauge = .{};
    pub var alloc_fresh_supply_exhausted_candidates: counters.Gauge = .{};
    /// Allocations parked on a trim claim window right now.
    pub var alloc_trim_window_parked: counters.Gauge = .{};
};

/// Number of allocators whose `StorageFull` latch is set: the write hot
/// path does ONE relaxed load and runs no census while it reads 0
/// (BA:L163-176).
pub var fresh_supply_latched: Atomic(usize) = .init(0);

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "alloc_from_freelist", "alloc_fresh_mints", "del_obj", "block_pin_stale_keys", "block_key_incarnation_unknown", "unpublished_offsets_recovered", "block_untracked_free_adjudicated", "shared_release_failures", "meta_kv_block_refs_recovered", "unpublished_mint_abandons", "dlm_quarantined_offsets", "dlm_quarantine_releases", "block_quarantine_deferred_frees", "block_quarantine_takebacks", "alloc_trim_window_parks", "alloc_trim_window_park_walls", "alloc_trim_window_parked" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "block_double_frees", "block_untracked_free_refusals", "block_claim_anomalies", "block_key_incarnation_exhausted", "block_live_free_refusals", "block_inflight_unheld_drops" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, counters.tripwireCount(name) });
    }
    try w.print("\"alloc_fresh_supply_exhausted\":[{d},{d}]}}", .{ stats.alloc_fresh_supply_exhausted_latched.load(), stats.alloc_fresh_supply_exhausted_candidates.load() });
}

// ---------------------------------------------------------------------------
// Forensics tape (Q7: no strings, no backtraces; the pairing names SITES).
// ---------------------------------------------------------------------------

pub const ForensicRecord = struct {
    idx: u64,
    site: Site,
    thread_id: u64,
    monotonic_ns: u64,
};

const ForensicsTape = struct {
    records: []ForensicRecord,
    cursor: Atomic(usize) = .init(0),

    fn record(self: *ForensicsTape, idx: u64, site: Site) void {
        const pos = self.cursor.fetchAdd(1, .monotonic) % self.records.len;
        self.records[pos] = .{ .idx = idx, .site = site, .thread_id = @intCast(std.Thread.getCurrentId()), .monotonic_ns = clock.monotonicNs() };
    }
};

// ---------------------------------------------------------------------------
// The allocator.
// ---------------------------------------------------------------------------

pub const Config = struct {
    /// The volume's durable identity as key bytes (`key.volumeTag`).
    vol_tag: u64,
    /// The allocator's index in the mount's volume table (reclaim entries
    /// carry it instead of a device-path string, Q5).
    vol: u16,
    /// The physical block (`CHUNK_SIZE` shipped).
    block_size: u64 = CHUNK_SIZE,
    /// Device capacity in bytes; the table is sized from it.
    capacity_bytes: u64,
    read_only: bool = false,
    /// `SQUEEZEFS_FREE_FORENSICS`.
    forensics: bool = false,
    /// Rung 6: the appender's durable term; rung 4 engages with 1.
    stamp_era: u32 = 1,
};

pub const Minted = struct {
    idx: u64,
    offset: u64,
    stamp: u64,
};

pub const PinVerdict = enum { pinned, pinned_unstable, refused };

pub const BeginFree = enum {
    /// Refcount hit 0: incarnation retired, the block is in limbo until
    /// `finishFree`.
    terminal,
    /// Another owner remains.
    nonterminal,
    /// Untracked (the second half of a double release) or SHARED-held or
    /// shipped: nothing freed, counted at the site.
    refused,
};

pub const Snapshot = struct {
    highest: u64,
    /// One bit per block below `highest`: SET = not reallocatable now.
    set_words: []u64,
    free: u64,
};

pub const BlockAllocator = struct {
    gpa: Allocator,
    cfg: Config,
    capacity_blocks: u64,
    cells: []Cell,
    /// SET = not reallocatable now (module doc).
    bits: []Atomic(u64),
    /// SET = an elided discard is owed for this block (strictly ⊆ free).
    debt_bits: []Atomic(u64),
    /// Clear bits below the cursor (the budget-then-bit entitlement).
    free_budget: Atomic(u64) align(std.atomic.cache_line) = .init(0),
    /// The dense fresh-mint cursor; never shrinks.
    highest: Atomic(u64) align(std.atomic.cache_line) = .init(0),
    hint: Atomic(u64) = .init(0),
    /// Blocks inside trim claim windows (`State.trim_windowed`).
    trim_windowed: Atomic(u64) = .init(0),
    /// Open trim claim windows (SeqCst with `freshSupplyVisible`'s read).
    trim_claimed: Atomic(u64) = .init(0),
    /// The return edge a parked allocation waits on (ONE `Notify` serves
    /// every supply edge of this allocator, Q10).
    supply_edge: sync_notify.Notify = .init,
    /// A trim-window park on this volume passed its ceiling and said so
    /// (`enospc.awaitTrimReturn`): one line per stall episode, cleared when
    /// the window closes.
    trim_wall_warned: Atomic(bool) = .init(false),
    /// Elided-discard debt, bytes (RAM-only).
    elided_debt_bytes: counters.Gauge = .{},
    /// The latched `StorageFull` word (0 = clear).
    fresh_supply_exhausted_since_ns: Atomic(u64) = .init(0),
    /// The last `StorageFull` a mint met (0 = never), latched or not: the
    /// parked-buffer gate's wall asks whether the store ran out INSIDE it
    /// (`storageFullSince`).
    last_storage_full_ns: Atomic(u64) = .init(0),
    /// `era << 40 | seq`, seq from 1 (Q11: a plain cursor, no lane stride).
    stamp_seq: Atomic(u64) = .init(0),
    /// `allocEpoch` side map, allocated at the first `fsckBeginScan`
    /// (separate from the word by design, BA:L220-233).
    fsck_scan_active: Atomic(bool) = .init(false),
    fsck_epoch: Atomic(u64) = .init(0),
    epoch_map: ?[]Atomic(u64) = null,
    /// Rung 6: cells in `State.quarantined`; the two-line `finishFree` gate.
    quarantine_count: Atomic(u64) = .init(0),
    /// Terminal frees in `limbo` the reclaimer's durability hold keeps
    /// until their records are durable (`reachableFreeBlocks`).
    durable_held: Atomic(u64) = .init(0),
    forensics: ?ForensicsTape = null,
    // ---- the seams (null = the unarmed, byte-identical path) ----
    plane_gate: ?PlaneGate = null,
    finish_free_sink: ?FinishFreeSink = null,
    grant_source: ?GrantSource = null,
    lease: ?AllocationLease = null,
    grace: ?GraceRing = null,
    shared_gate: ?SharedFreeGate = null,
    untracked_gate: ?UntrackedFreeGate = null,
    space_pending: ?SpacePending = null,
    /// Test seam (null in the product): `finishFree` calls it once its
    /// quarantine gate passed, before the publication — the window a
    /// concurrent admission lands in.
    test_finish_station: ?FinishStation = null,

    /// Sizes the table from `capacity_bytes / block_size`; every block
    /// virgin. The table bytes are the budget charge (`tableBytes`).
    pub fn init(gpa: Allocator, cfg: Config) Allocator.Error!*BlockAllocator {
        assert(cfg.block_size > 0);
        const cap = cfg.capacity_bytes / cfg.block_size;
        assert(cap > 0);
        const words: usize = @intCast((cap + 63) / 64);
        const self = try gpa.create(BlockAllocator);
        errdefer gpa.destroy(self);
        const cells = try gpa.alloc(Cell, @intCast(cap));
        errdefer gpa.free(cells);
        @memset(cells, .{});
        const bits = try gpa.alloc(Atomic(u64), words);
        errdefer gpa.free(bits);
        @memset(bits, .init(0));
        const debt_bits = try gpa.alloc(Atomic(u64), words);
        errdefer gpa.free(debt_bits);
        @memset(debt_bits, .init(0));
        var forensics: ?ForensicsTape = null;
        if (cfg.forensics) {
            const recs = try gpa.alloc(ForensicRecord, FREE_FORENSICS_TAPE_CAP);
            @memset(recs, .{ .idx = 0, .site = .mint, .thread_id = 0, .monotonic_ns = 0 });
            forensics = .{ .records = recs };
        }
        self.* = .{ .gpa = gpa, .cfg = cfg, .capacity_blocks = cap, .cells = cells, .bits = bits, .debt_bits = debt_bits, .forensics = forensics };
        return self;
    }

    /// `Drop` decrements the process latch count if latched (BA:L558-567).
    pub fn deinit(self: *BlockAllocator) void {
        if (self.fresh_supply_exhausted_since_ns.load(.monotonic) != 0) _ = fresh_supply_latched.fetchSub(1, .acq_rel);
        if (self.epoch_map) |m| self.gpa.free(m);
        if (self.forensics) |f| self.gpa.free(f.records);
        self.gpa.free(self.debt_bits);
        self.gpa.free(self.bits);
        self.gpa.free(self.cells);
        self.gpa.destroy(self);
    }

    /// Reads the forensics lever from the process environment.
    pub fn configFromEnv(env: *const std.process.Environ.Map, vol_tag: u64, vol: u16, block_size: u64, capacity_bytes: u64, read_only: bool) Config {
        return .{ .vol_tag = vol_tag, .vol = vol, .block_size = block_size, .capacity_bytes = capacity_bytes, .read_only = read_only, .forensics = knobs.boolKnob("SQUEEZEFS_FREE_FORENSICS", env) };
    }

    /// RAM the dense table + bitmaps pin: the budget component's floor.
    pub fn tableBytes(self: *const BlockAllocator) u64 {
        return @as(u64, self.cells.len) * @sizeOf(Cell) + @as(u64, self.bits.len + self.debt_bits.len) * @sizeOf(u64);
    }

    /// Non-sheddable (weight 0): the table is the volume's accounting, not a
    /// cache (Q3 "charged to the budget as a non-sheddable component").
    pub fn budgetComponent(self: *BlockAllocator) budget.Component {
        const Src = struct {
            fn read(ctx: *anyopaque) u64 {
                const a: *BlockAllocator = @ptrCast(@alignCast(ctx));
                return a.tableBytes();
            }
        };
        return budget.Component.init("block_alloc_table", self.tableBytes(), 0, .{ .func = .{ .ctx = self, .call = &Src.read } }, null);
    }

    pub fn volTag(self: *const BlockAllocator) u64 {
        return self.cfg.vol_tag;
    }

    pub fn blockSize(self: *const BlockAllocator) u64 {
        return self.cfg.block_size;
    }

    pub fn offsetOf(self: *const BlockAllocator, idx: u64) u64 {
        return idx * self.cfg.block_size;
    }

    pub fn idxOf(self: *const BlockAllocator, offset: u64) u64 {
        return offset / self.cfg.block_size;
    }

    fn cell(self: *BlockAllocator, idx: u64) *Cell {
        return &self.cells[@intCast(idx)];
    }

    // ---- the bitmap primitives (alloc_core's shapes, never its state) ----

    fn bitMask(idx: u64) u64 {
        return @as(u64, 1) << @intCast(idx % 64);
    }

    /// The unique 0→1 `fetchOr` winner. acq_rel pairs with `clearBit`.
    fn tryTakeBit(self: *BlockAllocator, idx: u64) bool {
        return self.bits[@intCast(idx / 64)].fetchOr(bitMask(idx), .acq_rel) & bitMask(idx) == 0;
    }

    /// 1→0; false if it was already clear (a double free).
    fn clearBit(self: *BlockAllocator, idx: u64) bool {
        return self.bits[@intCast(idx / 64)].fetchAnd(~bitMask(idx), .acq_rel) & bitMask(idx) != 0;
    }

    /// SET = not reallocatable now (allocated, granted, in limbo or inside
    /// a trim window); a census face for the holder's ladders.
    pub fn bitIsSet(self: *const BlockAllocator, idx: u64) bool {
        return self.bits[@intCast(idx / 64)].load(.acquire) & bitMask(idx) != 0;
    }

    /// Lowest clear bit in `[from, below)`, or null — the free-list pick
    /// and the striped placement's lowest-clear-run carve share it.
    fn lowestClear(self: *const BlockAllocator, from: u64, below: u64) ?u64 {
        if (from >= below) return null;
        var w: usize = @intCast(from / 64);
        const last_w: usize = @intCast((below - 1) / 64);
        while (w <= last_w) : (w += 1) {
            var inv = ~self.bits[w].load(.acquire);
            if (w == @as(usize, @intCast(from / 64))) inv &= ~(bitMask(from) - 1);
            if (inv != 0) {
                const idx = @as(u64, w) * 64 + @ctz(inv);
                return if (idx < below) idx else null;
            }
        }
        return null;
    }

    // ---- the plane gate (§11 item 1) ----

    fn planeGate(self: *BlockAllocator, site: Site) AllocError!void {
        if (self.cfg.read_only) return error.ReadOnly;
        if (self.plane_gate) |g| if (!g.holds(g.ctx, site)) return error.PlaneRefused;
    }

    // ---- lifetime stamps ----

    /// `era << 40 | seq`; exhaustion of the 40 seq bits is a tripwire and
    /// the mint carries `STAMP_NONE` (a fill then validates by the word
    /// alone, never serving another lifetime's bytes).
    fn mintStamp(self: *BlockAllocator) u64 {
        const seq = self.stamp_seq.fetchAdd(1, .monotonic) + 1;
        if (seq > STAMP_SEQ_MAX) {
            stats.block_key_incarnation_exhausted.fire("lifetime stamp seq space exhausted");
            return STAMP_NONE;
        }
        return (@as(u64, self.cfg.stamp_era) << STAMP_SEQ_BITS) | seq;
    }

    pub fn stampOf(self: *BlockAllocator, idx: u64) u64 {
        return self.cell(idx).stamp.load(.acquire);
    }

    /// A map entry's stamp names THIS lifetime iff it equals the cell's;
    /// `STAMP_NONE` on either side = unknown (counted, served).
    pub fn liveIncarnation(self: *BlockAllocator, idx: u64, stamp: u64) bool {
        const cur = self.stampOf(idx);
        if (stamp == STAMP_NONE or cur == STAMP_NONE) {
            stats.block_key_incarnation_unknown.inc();
            return true;
        }
        return cur == stamp;
    }

    // ---- claim (§1.2 ordering law) ----

    /// cancel elided debt → `retire` word → mint + store the stamp →
    /// refcount 1 LAST. The word and stamp move BEFORE the refcount entry
    /// exists: a pin landing between the two reads "untracked" and takes
    /// nothing; the reverse order let it raise the new count against the
    /// OLD stamp (BA:L2456-2461). A lingering count is an anomaly.
    fn claimBlockIdx(self: *BlockAllocator, idx: u64) Minted {
        const c = self.cell(idx);
        self.cancelElidedDebt(idx);
        Incarnation.retire(&c.incarnation);
        const stamp = self.mintStamp();
        c.stamp.store(stamp, .release);
        if (c.refcount.cmpxchgStrong(0, 1, .acq_rel, .monotonic)) |lingering| {
            stats.block_claim_anomalies.fire("a claimed block already carried a refcount");
            log.err("block allocator vol {d}: claim of block {d} found refcount {d} — the entry was never released (double-tracked lifetime); forcing 1", .{ self.cfg.vol, idx, lingering });
            c.refcount.store(1, .release);
        }
        c.setState(.allocated);
        if (self.fsck_scan_active.load(.acquire)) {
            if (self.epoch_map) |m| m[@intCast(idx)].store(self.fsck_epoch.load(.acquire), .release);
        }
        if (self.forensics) |*t| t.record(idx, .mint);
        return .{ .idx = idx, .offset = self.offsetOf(idx), .stamp = stamp };
    }

    /// The budget half of budget-then-bit: win one entitlement or report
    /// an empty list. acq_rel pairs with `creditFree`'s acq_rel increment,
    /// so the winner sees every bit a release cleared before its credit.
    fn winEntitlement(self: *BlockAllocator) bool {
        var free = self.free_budget.load(.acquire);
        while (true) {
            if (free == 0) return false;
            free = self.free_budget.cmpxchgWeak(free, free - 1, .acq_rel, .acquire) orelse return true;
        }
    }

    /// Free-list claim: budget-then-bit (module doc). Null ⇒ the list is empty.
    fn allocFromFreeList(self: *BlockAllocator) ?u64 {
        if (!self.winEntitlement()) return null;
        const highest = self.highest.load(.acquire);
        var pass: u32 = 0;
        while (pass < claim_rescan_limit) : (pass += 1) {
            const start = @min(self.hint.load(.monotonic), highest);
            var from = start;
            var wrapped = false;
            while (true) {
                const idx = self.lowestClear(from, highest) orelse {
                    if (wrapped or start == 0) break;
                    wrapped = true;
                    from = 0;
                    continue;
                };
                if (wrapped and idx >= start) break;
                if (self.tryTakeBit(idx)) {
                    self.hint.store(idx + 1, .monotonic);
                    stats.alloc_from_freelist.inc();
                    return idx;
                }
                from = idx + 1;
            }
            std.atomic.spinLoopHint();
        }
        // Budget and bitmap disagree: refund, count, and let the mint fall
        // to the fresh cursor (never a spurious StorageFull).
        self.creditFree();
        stats.block_claim_anomalies.fire("free budget won with no clear bit below the cursor");
        return null;
    }

    /// The fresh mint: the grant window when armed, else a CAS on the
    /// cursor; `StorageFull` iff `idx ≥ cap` — a refused racer never bumps
    /// the cursor (BA:L2099-2144). An armed window's emptiness carries its
    /// class: the holder's `full` is this allocator's terminal verdict, a
    /// top-up still owed (`retry`) is `PlaneRefused` and never latches.
    fn nextFreshBlock(self: *BlockAllocator) AllocError!u64 {
        if (self.grant_source) |g| {
            const idx = switch (g.vtable.mint(g.ctx)) {
                .minted => |idx| idx,
                .full => {
                    g.vtable.noteExhausted(g.ctx);
                    return error.StorageFull;
                },
                .retry => {
                    g.vtable.noteExhausted(g.ctx);
                    return error.PlaneRefused;
                },
            };
            _ = self.highest.fetchMax(idx + 1, .acq_rel);
            // The window's bits were SET at the carve (charter §5.5).
            if (!self.bitIsSet(idx)) _ = self.tryTakeBit(idx);
            stats.alloc_fresh_mints.inc();
            return idx;
        }
        var cur = self.highest.load(.monotonic);
        while (true) {
            if (cur >= self.capacity_blocks) return error.StorageFull;
            cur = self.highest.cmpxchgWeak(cur, cur + 1, .monotonic, .monotonic) orelse {
                if (self.tryTakeBit(cur)) {
                    stats.alloc_fresh_mints.inc();
                    return cur;
                }
                // Virgin ⇒ clear, so a SET bit at the cursor is a carved
                // grant window whose source was disarmed under us (the
                // dismount pass running after the grant plane's teardown,
                // rung 8). Walking past it would mint beyond the holder's
                // carve with no grant: the cursor is put back and the mint
                // refuses RETRYABLE (never the StorageFull latch) — the
                // caller's custody stays where it was (a staged resident
                // stays staged), loud.
                _ = self.highest.cmpxchgStrong(cur + 1, cur, .monotonic, .monotonic);
                stats.block_claim_anomalies.fire("the fresh cursor met a SET bit (a carved window with its mint source disarmed; the mint refuses PlaneRefused)");
                return error.PlaneRefused;
            };
        }
    }

    fn tryAllocateBlock(self: *BlockAllocator) AllocError!u64 {
        if (self.grace) |g| {
            // The funnel head's `harvest_grace`: a ring that defers nothing
            // (rung 4) yields nothing; one call, one pointer test.
            var buf: [8]u64 = undefined;
            const n = g.vtable.harvest(g.ctx, buf.len, &buf);
            for (buf[0..n]) |idx| self.returnFromGrace(idx);
        }
        if (self.grant_source != null) return self.nextFreshBlock();
        if (self.allocFromFreeList()) |idx| return idx;
        return self.nextFreshBlock();
    }

    fn returnFromGrace(self: *BlockAllocator, idx: u64) void {
        // A harvested offset becomes reallocatable exactly like a finished free.
        self.publishFreeList(idx, .seed_free);
    }

    /// `allocate_block`: the funnel plus the latch bookkeeping. The caller
    /// owns the mint through a `MintedBlockGuard` (Law 9) and registers an
    /// `InflightAllocGuard` before any DMA names it (Law 2).
    pub fn allocateBlock(self: *BlockAllocator) AllocError!Minted {
        try self.planeGate(.mint);
        const idx = self.tryAllocateBlock() catch |e| {
            if (e == error.StorageFull) self.noteFreshSupplyExhausted();
            return e;
        };
        const minted = self.claimBlockIdx(idx);
        self.noteFreshSupplyAvailable();
        return minted;
    }

    /// The defrag mover's mint: the lowest free idx below `below`, never a
    /// fresh mint (BA:L2531-2551). On a grant-armed allocator a clear bit is
    /// the HOLDER's supply, reallocated only through a carve (charter §5.5):
    /// nothing to pick locally.
    pub fn allocateBlockBelow(self: *BlockAllocator, below: u64) AllocError!?u64 {
        try self.planeGate(.mint);
        if (self.grant_source != null) return null;
        const bound = @min(below, self.highest.load(.acquire));
        var from: u64 = 0;
        while (self.lowestClear(from, bound)) |idx| {
            // Entitlement for THIS bit: a stolen bit refunds and moves on.
            if (!self.winEntitlement()) return null;
            if (self.tryTakeBit(idx)) {
                stats.alloc_from_freelist.inc();
                return self.claimBlockIdx(idx).idx;
            }
            self.creditFree();
            from = idx + 1;
        }
        return null;
    }

    /// Ascending convergence: the lowest free idx ≥ `min`, else fresh. Armed:
    /// the window is the ONE mint source (a clear bit is the holder's).
    pub fn allocateBlockAtOrAbove(self: *BlockAllocator, min: u64) AllocError!Minted {
        try self.planeGate(.mint);
        const highest = self.highest.load(.acquire);
        var from = if (self.grant_source != null) highest else min;
        while (self.lowestClear(from, highest)) |idx| {
            if (!self.winEntitlement()) break;
            if (self.tryTakeBit(idx)) {
                stats.alloc_from_freelist.inc();
                return self.claimBlockIdx(idx);
            }
            self.creditFree();
            from = idx + 1;
        }
        const idx = self.nextFreshBlock() catch |e| {
            if (e == error.StorageFull) self.noteFreshSupplyExhausted();
            return e;
        };
        const m = self.claimBlockIdx(idx);
        self.noteFreshSupplyAvailable();
        return m;
    }

    /// The recovery census arm (BA:L3067-3104): a specific block, refcount
    /// 1, debt cancelled. A gap `cur_highest..idx` is declared FREE only in
    /// the quiescent mount seed (`*BlockAllocator`, Q9: at runtime a live
    /// peer's minted-but-unpublished tail is never a free list).
    pub fn allocateSpecificBlock(self: *BlockAllocator, idx: u64) AllocError!Minted {
        try self.planeGate(.specific_claim);
        if (idx >= self.capacity_blocks) return error.NotFree;
        const highest = self.highest.load(.acquire);
        if (idx >= highest) {
            if (self.grant_source != null) return error.NotFree;
            var gap = highest;
            while (gap < idx) : (gap += 1) {
                self.cell(gap).setState(.free);
                self.creditFree();
            }
            stats.unpublished_offsets_recovered.add(idx - highest);
            self.highest.store(idx + 1, .release);
            const took = self.tryTakeBit(idx);
            assert(took);
            return self.claimBlockIdx(idx);
        }
        if (!self.winEntitlement()) return error.NotFree;
        if (!self.tryTakeBit(idx)) {
            self.creditFree();
            return error.NotFree;
        }
        return self.claimBlockIdx(idx);
    }

    /// The mount seed protocol (BA:L3119-3144): a referenced block becomes
    /// allocated with refcount = its reference count; the table is
    /// quiescent (plain stores — Q8: the loom-verified `tryAcquire` is the
    /// ONE runtime accessor family). Cells between the old cursor and the
    /// block are FREE (nobody references them).
    pub fn recoverBlock(self: *BlockAllocator, idx: u64) void {
        assert(idx < self.capacity_blocks);
        const c = self.cell(idx);
        const highest = self.highest.load(.monotonic);
        if (idx >= highest) {
            var gap = highest;
            while (gap < idx) : (gap += 1) {
                self.cell(gap).setState(.free);
                self.creditFree();
            }
            self.highest.store(idx + 1, .monotonic);
        }
        switch (c.getState()) {
            .allocated => {
                c.refcount.store(c.refcount.load(.monotonic) + 1, .monotonic);
            },
            .free, .virgin => {
                if (c.getState() == .free) self.debitFree();
                _ = self.tryTakeBit(idx);
                c.refcount.store(1, .monotonic);
                c.incarnation.store(STABLE_FIRST, .monotonic);
                c.setState(.allocated);
            },
            else => unreachable, // the seed runs before any free
        }
    }

    /// `seed_from_durable_refs`: ascending, one `recoverBlock` per
    /// reference (`meta_kv_block_refs_recovered += n`).
    pub fn seedFromDurableRefs(self: *BlockAllocator, idxs: []u64) void {
        std.mem.sort(u64, idxs, {}, std.sort.asc(u64));
        for (idxs) |idx| self.recoverBlock(idx);
        stats.meta_kv_block_refs_recovered.add(idxs.len);
        if (self.lease) |l| l.vtable.noteMountSeed(l.ctx, self.cfg.vol_tag, self.highest.load(.monotonic), self.bitWords());
    }

    // ---- the free budget's two edges ----

    /// Bit already clear by the caller; THEN the budget. acq_rel pairs with
    /// the claim's budget CAS (the cleared bit is visible to the winner).
    fn creditFree(self: *BlockAllocator) void {
        _ = self.free_budget.fetchAdd(1, .acq_rel);
    }

    fn debitFree(self: *BlockAllocator) void {
        const prev = self.free_budget.fetchSub(1, .acq_rel);
        assert(prev >= 1);
    }

    // ---- publish / snapshot / still (the fill's face) ----

    /// `mark_incarnation_unstable`: the retire; an untouched word becomes
    /// `UNSTABLE_FIRST` (gen 1, unstable) by the same arithmetic.
    pub fn markIncarnationUnstable(self: *BlockAllocator, idx: u64) void {
        Incarnation.retire(&self.cell(idx).incarnation);
    }

    /// `publish_block`: an untouched word becomes `STABLE_FIRST`.
    pub fn publishBlock(self: *BlockAllocator, idx: u64) void {
        Incarnation.publish(&self.cell(idx).incarnation);
    }

    /// `fill_incarnation`: the snapshot a fill takes before its bytes;
    /// a never-touched word reads `UNKNOWN_STABLE`.
    pub fn fillIncarnation(self: *BlockAllocator, idx: u64) ?u64 {
        const w = &self.cell(idx).incarnation;
        if (w.load(.monotonic) == 0) return UNKNOWN_STABLE;
        return Incarnation.snapshot(w);
    }

    /// `fill_incarnation_still`: the validation after the bytes.
    pub fn fillIncarnationStill(self: *BlockAllocator, idx: u64, before: u64) bool {
        const w = &self.cell(idx).incarnation;
        if (before == UNKNOWN_STABLE) return w.fetchOr(0, .acq_rel) == 0;
        return Incarnation.still(w, before);
    }

    /// May a validated fill of this block DEPOSIT into the read tiers? A
    /// tier entry lives exactly one LOCAL lifetime (the claim's publish
    /// and the free's purge bound it); a cell outside one (`limbo` once
    /// the free began, then free / virgin after a give-back or a carve /
    /// trim-windowed — word ≠ 0) is dying, in another member's custody or
    /// nobody's: a deposit there could be its own dead bytes, and no
    /// foreign publish purges this member's tiers. A never-touched cell
    /// (word 0) deposits as before: its foreign lifetime's end is the
    /// token recall's purge (decision 0003 §V). Called after the fill's
    /// `still` passed; a claim racing it moves the word, so the tiers'
    /// insert-time validate refuses.
    pub fn fillDepositable(self: *BlockAllocator, idx: u64) bool {
        const c = self.cell(idx);
        if (c.incarnation.load(.acquire) == 0) return true;
        return switch (c.getState()) {
            .free, .virgin, .trim_windowed, .limbo => false,
            .allocated, .quarantined => true,
        };
    }

    pub fn incarnationWord(self: *BlockAllocator, idx: u64) u64 {
        return self.cell(idx).incarnation.load(.acquire);
    }

    // ---- refcounts and the W1 pair ----

    /// `increment_refcount`: never fabricates a count for a possibly
    /// free-listed block (false + WARN on 0).
    pub fn incrementRefcount(self: *BlockAllocator, idx: u64) bool {
        if (Refcount.tryAcquire(&self.cell(idx).refcount)) return true;
        log.warn("block allocator vol {d}: refused to raise the refcount of untracked block {d} (a pin of a freed or never-published block)", .{ self.cfg.vol, idx });
        return false;
    }

    pub fn refcount(self: *BlockAllocator, idx: u64) ?u32 {
        const n = Refcount.peek(&self.cell(idx).refcount);
        return if (n == 0) null else n;
    }

    /// A reference released while another still holds the block: true =
    /// decremented (nonterminal by construction); false = this is the last
    /// tracked reference, a SHARED block (its terminality is the share
    /// home's) or an untracked one — nothing changed, the full ladder
    /// decides.
    pub fn releaseUnlessLast(self: *BlockAllocator, idx: u64) bool {
        if (self.isShared(idx)) return false;
        return Refcount.releaseUnlessLast(&self.cell(idx).refcount);
    }

    /// The clone side of W1: pin (SeqCst CAS) → snapshot (SeqCst load).
    /// Both are the cross-word pair's accesses; the snapshot sits AFTER the
    /// pin so the absent/unstable arm is fenced too (BA:L2047-2063).
    pub fn pinBlockValidated(self: *BlockAllocator, idx: u64) PinVerdict {
        const c = self.cell(idx);
        if (!Refcount.tryAcquire(&c.refcount)) return .refused;
        if (Incarnation.snapshotFenced(&c.incarnation) == null) return .pinned_unstable;
        return .pinned;
    }

    /// The write path's clone: pin; unstable ⇒ unpin and retry, bounded at
    /// 3 attempts (charter §6.2 item 2; `test_clone_retries_via_
    /// authoritative_map`'s `attempt ≥ 3`). WHY 3: an unstable snapshot
    /// means a patch is mid-flight on the block; a patch is one DMA, so a
    /// second attempt almost always lands and a third proves a stuck
    /// writer rather than a race — the caller then falls back to CoW.
    pub fn pinBlockRetrying(self: *BlockAllocator, idx: u64) PinVerdict {
        var attempt: u32 = 0;
        while (attempt < 3) : (attempt += 1) {
            switch (self.pinBlockValidated(idx)) {
                .pinned => return .pinned,
                .refused => return .refused,
                .pinned_unstable => {
                    _ = Refcount.release(&self.cell(idx).refcount);
                    stats.block_pin_stale_keys.inc();
                },
            }
        }
        return .pinned_unstable;
    }

    /// Drop a pin taken by `pinBlockValidated` (never terminal here: a pin
    /// is always above an owner's reference).
    pub fn unpin(self: *BlockAllocator, idx: u64) void {
        if (Refcount.release(&self.cell(idx).refcount)) {
            stats.block_live_free_refusals.fire("an unpin was the terminal release");
        }
    }

    /// The patch side of W1: plane gate → retire (SeqCst) → peek (SeqCst)
    /// `== 1 ∧ !SHARED`. On `false` the caller MUST `publishBlock` (re-
    /// stabilize under the NEW generation, content unchanged) and fall back
    /// to CoW (BA:L1889-1923).
    pub fn beginPatchSoleOwner(self: *BlockAllocator, idx: u64) AllocError!bool {
        try self.planeGate(.patch_retire);
        const c = self.cell(idx);
        Incarnation.retire(&c.incarnation);
        const sole = Refcount.peekFenced(&c.refcount) == 1;
        return sole and !self.isShared(idx);
    }

    // ---- SHARED mark (rung 6's bit; rung 4 keeps the accessor family) ----

    pub fn markShared(self: *BlockAllocator, idx: u64) void {
        _ = self.cell(idx).flags.fetchOr(FLAG_SHARED, .acq_rel);
    }

    pub fn clearShared(self: *BlockAllocator, idx: u64) void {
        _ = self.cell(idx).flags.fetchAnd(~FLAG_SHARED, .acq_rel);
    }

    pub fn isShared(self: *BlockAllocator, idx: u64) bool {
        return self.cell(idx).flags.load(.acquire) & FLAG_SHARED != 0;
    }

    // ---- free (§1.5) ----

    /// `begin_free`: plane gate → SHARED gate → release. Terminal ⇒ the
    /// incarnation is RETIRED before the block becomes reallocatable (an
    /// in-flight fill of the stale map fails `still`), the SHARED mark is
    /// cleared, the state is `limbo`. Untracked ⇒ REFUSED + ERROR +
    /// `block_untracked_free_refusals` (the second half of a double
    /// release; the leak-safe direction — fsck C6 reconciles a genuine
    /// limbo, BA:L2586-2600).
    pub fn beginFree(self: *BlockAllocator, idx: u64) AllocError!BeginFree {
        try self.planeGate(.terminal_free);
        const c = self.cell(idx);
        if (self.isShared(idx)) {
            if (self.shared_gate) |g| switch (g.ask(g.ctx, self.cfg.vol_tag, idx)) {
                .held => return .nonterminal,
                .freed, .not_shared => self.clearShared(idx),
                .unreachable_home => {
                    stats.shared_release_failures.inc();
                    return .refused;
                },
            };
        }
        if (Refcount.peek(&c.refcount) == 0) {
            if (self.untracked_gate) |g| {
                stats.block_untracked_free_adjudicated.inc();
                if (!g.adjudicate(g.ctx, self.cfg.vol_tag, idx)) return .refused;
                // Adjudicated as ours: fall through to the terminal path
                // with the count the ladder restored.
            }
            if (Refcount.peek(&c.refcount) == 0) {
                stats.block_untracked_free_refusals.fire("begin_free of an untracked block");
                log.err("block allocator vol {d}: REFUSED begin_free of block {d}: no RAM refcount (a double release upstream; nothing freed — fsck C6 reconciles a genuine limbo)", .{ self.cfg.vol, idx });
                return .refused;
            }
        }
        if (!Refcount.release(&c.refcount)) return .nonterminal;
        self.clearShared(idx);
        Incarnation.retire(&c.incarnation);
        c.setState(.limbo);
        if (self.forensics) |*t| t.record(idx, .terminal_free);
        return .terminal;
    }

    /// `finish_free`: quarantine gate (§4.4: the publish is OWED to the
    /// cohort's release — "a quarantine never force-drains") → grace defer
    /// → the free list. The block is reallocatable only NOW.
    pub fn finishFree(self: *BlockAllocator, idx: u64) void {
        if (self.quarantine_count.load(.acquire) != 0 and self.oweToCohort(idx)) {
            stats.block_quarantine_deferred_frees.inc();
            return;
        }
        if (self.test_finish_station) |st| st.passedGate(st.ctx, idx);
        if (self.grace) |g| if (g.vtable.defer_(g.ctx, idx, self.cfg.block_size)) {
            stats.del_obj.inc();
            return;
        };
        self.publishFreeList(idx, .terminal_free);
        stats.del_obj.inc();
    }

    /// `publish_free_list`: the word re-stabilized → the STATE (`free`
    /// below the cursor; a never-minted block — a quarantined grant
    /// released — reads `virgin`: a clear bit at/above the cursor is never
    /// a free-list entry) → the bit → the credit (module doc). Armed, the
    /// holder's sink clears the bit, credits and queues the kind-5 CLEAR in
    /// ONE critical section under the holder's ordering lock (the bitmap IS
    /// the free list, 0002 row 237) — the lock every carve takes, so a
    /// carve meeting the clear bit reads `free` with its credit landed.
    /// The state store is ordered before the bit by the clear's acq_rel
    /// (paired with the claimant's acq_rel take: `tryTakeBit`); stored
    /// after it, a carve + mint between the two ended at `free` on the
    /// minted block with a phantom credit, and a quarantine of it asserted
    /// on its lost take (0002 row 512). A bit already clear ⇒ ERROR
    /// "DOUBLE FREE" + `block_double_frees` (the state stored reads what a
    /// clear bit means).
    fn publishFreeList(self: *BlockAllocator, idx: u64, site: Site) void {
        const c = self.cell(idx);
        endLifetime(c);
        const listed = idx < self.highest.load(.acquire);
        c.setStateSeqCst(if (listed) .free else .virgin);
        if (self.finish_free_sink) |s| {
            if (!s.noteFinishFree(s.ctx, self.cfg.vol_tag, idx)) {
                stats.block_double_frees.fire("holder bitmap bit already clear");
                log.err("block allocator vol {d}: DOUBLE FREE of block {d} (the holder's bit was already clear)", .{ self.cfg.vol, idx });
                return;
            }
            if (self.forensics) |*t| t.record(idx, site);
            self.recheckQuarantine(idx, listed);
            return;
        }
        if (self.forensics) |*t| t.record(idx, site);
        if (!self.clearBit(idx)) {
            stats.block_double_frees.fire("free-list bit already clear");
            log.err("block allocator vol {d}: DOUBLE FREE of block {d} (already on the free list)", .{ self.cfg.vol, idx });
            return;
        }
        if (listed) self.creditListed(idx);
        self.recheckQuarantine(idx, listed);
    }

    /// The publication's half of the quarantine gate's Dekker pair (0003
    /// §Y.3; `free_list_model.zig` model 3): an admission that read the cell
    /// before the publication's state store (`limbo` — the gate long passed,
    /// or the block parked in the grace ring) took the flag alone; this
    /// re-check, AFTER the state store (both seq_cst), sees its flag and
    /// takes the block back out of the list — the entitlement, then the bit
    /// (a claim, or the admission's own free arm, that won it owns the
    /// block) — leaving it `limbo` with its publish OWED to the cohort's
    /// release (`oweOrRepublish`).
    fn recheckQuarantine(self: *BlockAllocator, idx: u64, listed: bool) void {
        const c = self.cell(idx);
        if (c.flags.load(.seq_cst) & FLAG_QUARANTINED == 0) return;
        if (!self.takeOutOfList(idx, listed, true)) return;
        c.setState(.limbo);
        stats.block_quarantine_takebacks.inc();
        self.oweOrRepublish(idx);
    }

    /// A taken-back block's publish is OWED to its cohort's release —
    /// only while the cohort holds it: a release that cleared the cohort
    /// first leaves the block nobody's, so the take-back publishes it again
    /// itself.
    fn oweOrRepublish(self: *BlockAllocator, idx: u64) void {
        if (!self.oweToCohort(idx)) self.publishFreeList(idx, .terminal_free);
    }

    /// OWED is set by a CAS that REQUIRES the flag, against the release's
    /// ONE `fetchAnd` of both (`releaseQuarantinedCell`): a mark after the
    /// release cleared the cohort would leave the block in limbo with
    /// nobody to publish it (model 3, `owe_unconditional`, `gate_two_steps`).
    /// False ⇒ the cell is not quarantined (the caller publishes).
    fn oweToCohort(self: *BlockAllocator, idx: u64) bool {
        const c = self.cell(idx);
        var f = c.flags.load(.acquire);
        while (true) {
            if (f & FLAG_QUARANTINED == 0) return false;
            f = c.flags.cmpxchgWeak(f, f | FLAG_QUARANTINE_OWED, .acq_rel, .acquire) orelse return true;
        }
    }

    /// A non-carve take (the trim claim, the quarantine's arms, the
    /// re-check's take-back): under the holder's ordering lock when the
    /// sink shares it (0003 §Y.4), else in place. False = lost.
    fn takeOutOfList(self: *BlockAllocator, idx: u64, listed: bool, holds: bool) bool {
        if (self.finish_free_sink) |s| if (s.takeOutOfList) |f| return f(s.ctx, self.cfg.vol_tag, idx, listed, holds);
        return self.holderTakeOutOfList(idx, listed);
    }

    /// A clear bit below the cursor joins the budget: AFTER the bit, so a
    /// claim that wins this credit finds a clear bit on its scan.
    fn creditListed(self: *BlockAllocator, idx: u64) void {
        self.creditFree();
        _ = self.hint.fetchMin(idx, .monotonic);
    }

    /// The local lifetime is over: the word re-stabilizes under the
    /// generation the retire minted. Unstable means a transition IN
    /// FLIGHT here; left unstable, a block another member re-minted from a
    /// wrapped carve read as mid-write on this member forever (every read
    /// EIO). Called with the cell already OUT of a lifetime (`limbo` after
    /// `beginFree`, `virgin` after a give-back) and before the free list
    /// can hand the block to a claim: `publish`'s release orders that state
    /// store before the stable bit a snapshot acquires, so `fillDepositable`
    /// refuses the deposit (model 7, `incarnation_model.zig`).
    fn endLifetime(c: *Cell) void {
        Incarnation.publish(&c.incarnation);
    }

    /// `free_block`: gate + `beginFree` + `finishFree` with nothing between.
    pub fn freeBlock(self: *BlockAllocator, idx: u64) AllocError!BeginFree {
        const v = try self.beginFree(idx);
        if (v == .terminal) self.finishFree(idx);
        return v;
    }

    /// Law 9 / Q13: an UNPUBLISHED mint returns to the allocator only
    /// through its rollback owner, synchronously — the grant window when
    /// the window minted it, else a terminal free (`unpublished_mint_
    /// abandons` counts the event: a failed DMA, a refused publish or a
    /// superseded write-through are the design working; growth past
    /// `supersessions + discards` under a healthy device is a leak upstream).
    pub fn abandonUnpublished(self: *BlockAllocator, idx: u64) void {
        stats.unpublished_mint_abandons.inc();
        if (self.forensics) |*t| t.record(idx, .abandon);
        if (self.grant_source) |g| {
            // Back in the window = unminted again; the window owns the bit
            // (set at the carve), so the local list never sees it. The cell
            // leaves its lifetime FIRST: the window's head is the next mint,
            // and a claim taking it before these stores met a live count
            // (`block_claim_anomalies`) that the release then zeroed.
            const c = self.cell(idx);
            _ = Refcount.release(&c.refcount);
            Incarnation.retire(&c.incarnation);
            c.setState(.virgin);
            endLifetime(c);
            if (g.vtable.giveBack(g.ctx, idx)) return;
            // Not taken back (already unconsumed, or no room): the mint is
            // still this thread's alone — its count and state return for
            // the terminal free.
            c.refcount.store(1, .release);
            c.setState(.allocated);
        }
        _ = self.freeBlock(idx) catch {};
    }

    // ---- elided debt (§3.1) ----

    /// `record_elided_debt`: RAM-only, strictly ⊆ the free list.
    pub fn recordElidedDebt(self: *BlockAllocator, idx: u64) void {
        const c = self.cell(idx);
        if (c.flags.fetchOr(FLAG_ELIDED_DEBT, .acq_rel) & FLAG_ELIDED_DEBT == 0) {
            _ = self.debt_bits[@intCast(idx / 64)].fetchOr(bitMask(idx), .acq_rel);
            self.elided_debt_bytes.add(self.cfg.block_size);
        }
    }

    /// Claim-cancels-debt (KD-4.6): a reused block owes no discard.
    pub fn cancelElidedDebt(self: *BlockAllocator, idx: u64) void {
        const c = self.cell(idx);
        if (c.flags.fetchAnd(~FLAG_ELIDED_DEBT, .acq_rel) & FLAG_ELIDED_DEBT != 0) {
            _ = self.debt_bits[@intCast(idx / 64)].fetchAnd(~bitMask(idx), .acq_rel);
            self.elided_debt_bytes.sub(self.cfg.block_size);
        }
    }

    pub fn elidedDebtBytes(self: *const BlockAllocator) u64 {
        return self.elided_debt_bytes.load();
    }

    /// `take_debt_batch`: up to `out.len` debt-carrying free blocks, lowest
    /// first, skipping what the grace ring holds (BA:L1250-1276). The
    /// debt bits are a bitmap scan, never a hash walk (Q3).
    pub fn takeDebtBatch(self: *BlockAllocator, out: []u64) usize {
        var n: usize = 0;
        for (self.debt_bits, 0..) |*w, wi| {
            var word = w.load(.acquire);
            while (word != 0 and n < out.len) : (word &= word - 1) {
                const idx = @as(u64, wi) * 64 + @ctz(word);
                if (self.grace) |g| if (g.vtable.holds(g.ctx, idx)) continue;
                out[n] = idx;
                n += 1;
            }
            if (n == out.len) break;
        }
        return n;
    }

    // ---- the trim claim window (KD-4.4, BA:L1290-1340) ----

    /// `claim_free_for_trim`: `trim_claimed += 1` (SeqCst) BEFORE the bit
    /// leaves the free list; a lost claim (reused, a racing trim, or a
    /// publication whose credit has not landed) closes the window at once.
    /// The entitlement is won BEFORE the bit, like every claim: the bit
    /// taken first debited a credit the publication had not yet made (an
    /// underflow — `free_list_model.zig` model 1, `trim_bit_first`). The
    /// caller holds the returned registration (fsck visibility) until
    /// `returnFromTrim`.
    pub fn claimFreeForTrim(self: *BlockAllocator, idx: u64) ?InflightAllocGuard {
        _ = self.trim_claimed.fetchAdd(1, .seq_cst);
        const c = self.cell(idx);
        // The take (state, entitlement, bit — under the holder's lock when
        // armed, 0003 §Y.4).
        if (!self.takeOutOfList(idx, true, false)) {
            self.endTrimClaim();
            return null;
        }
        _ = self.trim_windowed.fetchAdd(1, .acq_rel);
        c.setState(.trim_windowed);
        if (self.forensics) |*t| t.record(idx, .trim_claim);
        return InflightAllocGuard.register(self, idx);
    }

    /// `return_from_trim`: membership back BEFORE the window closes, so a
    /// parked allocation that reads the window closed finds the block on
    /// its rescan; the state before the bit, the credit after it (the
    /// publication's order, module doc) — the bit and the credit under the
    /// holder's lock when armed (a carve meeting the clear bit finds its
    /// credit, 0003 §Y.4).
    pub fn returnFromTrim(self: *BlockAllocator, idx: u64, guard: *InflightAllocGuard) void {
        const c = self.cell(idx);
        assert(c.getState() == .trim_windowed);
        _ = self.trim_windowed.fetchSub(1, .acq_rel);
        c.setState(.free);
        const cleared = if (self.finish_free_sink) |s| (if (s.returnTrim) |f| f(s.ctx, self.cfg.vol_tag, idx) else self.holderReturnTrim(idx)) else self.holderReturnTrim(idx);
        assert(cleared);
        guard.deinit();
        self.endTrimClaim();
    }

    /// `end_trim_claim`: the count's release, then the edge every parked
    /// allocation waits on.
    fn endTrimClaim(self: *BlockAllocator) void {
        _ = self.trim_claimed.fetchSub(1, .seq_cst);
    }

    pub fn trimClaimed(self: *const BlockAllocator) u64 {
        return self.trim_claimed.load(.seq_cst);
    }

    /// Blocks inside open trim claim windows.
    pub fn trimWindowedBlocks(self: *const BlockAllocator) u64 {
        return self.trim_windowed.load(.acquire);
    }

    // ---- supply arithmetic (§1.6; §11 item 6) ----

    pub fn capacityBlocks(self: *const BlockAllocator) u64 {
        return self.capacity_blocks;
    }

    pub fn highestBlockIndex(self: *const BlockAllocator) u64 {
        return self.highest.load(.acquire);
    }

    pub fn freeBlocksCount(self: *const BlockAllocator) u64 {
        return self.free_budget.load(.acquire);
    }

    /// `(cap − min(cursor, cap)) × block`. On a grant-armed allocator the
    /// virgin tail is the HOLDER's: this process holding the lease reads
    /// `blocks − frontier` (the bitmap's highest set bit + 1 — the elision
    /// watermark's input, `sym_data_planes.md` §4.3 item 2); a joiner reads
    /// 0 (its own supply is its window, `freeSupplyBlocks`).
    pub fn virginBytes(self: *const BlockAllocator) u64 {
        if (self.grant_source != null) {
            if (self.lease) |l| if (l.vtable.holderView(l.ctx)) |v| return (v.bitmap_blocks -| v.frontier) * self.cfg.block_size;
            return 0;
        }
        const cursor = @min(self.highest.load(.acquire), self.capacity_blocks);
        return (self.capacity_blocks - cursor) * self.cfg.block_size;
    }

    pub fn virginBlocks(self: *const BlockAllocator) u64 {
        return self.virginBytes() / self.cfg.block_size;
    }

    /// `virgin + free_list_len` unarmed. Armed: the window's remainder plus
    /// the CLEAR population of the holder's bitmap (`blocks − population`,
    /// the holder's own or the last word a grant reply carried — the flat
    /// inputs "read 0 on an armed allocator at a full cursor" and the
    /// free-grace valve would read them as a permanent trough).
    pub fn freeSupplyBlocks(self: *const BlockAllocator) u64 {
        if (self.grant_source) |g| {
            var n = g.vtable.remaining(g.ctx);
            if (self.lease) |l| if (l.vtable.holderView(l.ctx)) |v| {
                n += v.bitmap_blocks -| v.population;
            };
            return n;
        }
        return self.virginBlocks() + self.freeBlocksCount();
    }

    /// `virgin + free-listed + inside trim windows + held for durability`:
    /// a window is pending supply, never a deficit; a held free returns
    /// within one barrier interval, or at once under the ENOSPC valve's
    /// pulled barrier (`reclaim`'s durability hold) — `df` counts it free,
    /// as ext4 counts the blocks its running transaction freed.
    pub fn reachableFreeBlocks(self: *const BlockAllocator) u64 {
        return self.freeSupplyBlocks() + self.trim_windowed.load(.acquire) + self.durable_held.load(.acquire);
    }

    /// A terminal free in `limbo` waits for its record to be durable
    /// (`reclaim`'s durability hold): counted as supply in sight.
    pub fn holdForDurability(self: *BlockAllocator) void {
        _ = self.durable_held.fetchAdd(1, .acq_rel);
    }

    pub fn releaseFromDurability(self: *BlockAllocator) void {
        _ = self.durable_held.fetchSub(1, .acq_rel);
    }

    /// `highest − free` on a local allocator; on an armed one the bitmap
    /// population minus the holder's own unconsumed window.
    pub fn usedBlocks(self: *const BlockAllocator) u64 {
        if (self.lease) |l| if (l.vtable.holderView(l.ctx)) |v| return v.population -| v.window_remaining;
        return self.highest.load(.acquire) -| self.freeBlocksCount();
    }

    /// Clear-bit RUNS below the cursor over clear bits: 0 = one run, →1000
    /// = every free block isolated (per-mille).
    pub fn calculateFragmentation(self: *const BlockAllocator) u64 {
        const highest = self.highest.load(.acquire);
        var runs: u64 = 0;
        var free: u64 = 0;
        var in_run = false;
        var idx: u64 = 0;
        while (idx < highest) : (idx += 1) {
            if (!self.bitIsSet(idx)) {
                free += 1;
                if (!in_run) runs += 1;
                in_run = true;
            } else in_run = false;
        }
        if (free <= 1) return 0;
        return (runs - 1) * 1000 / (free - 1);
    }

    fn bitWords(self: *const BlockAllocator) []const u64 {
        return @ptrCast(self.bits);
    }

    /// §11 item 5: the bitmap's export — `highest`, a COPY of the set words
    /// below it, the free count. Caller frees `set_words`.
    pub fn derivedAllocationSnapshot(self: *const BlockAllocator, gpa: Allocator) Allocator.Error!Snapshot {
        const highest = self.highest.load(.acquire);
        const words: usize = @intCast((highest + 63) / 64);
        const out = try gpa.alloc(u64, words);
        for (out, 0..) |*w, i| {
            var v = self.bits[i].load(.acquire);
            if (i == words - 1 and highest % 64 != 0) v &= (@as(u64, 1) << @intCast(highest % 64)) - 1;
            w.* = v;
        }
        return .{ .highest = highest, .set_words = out, .free = self.freeBlocksCount() };
    }

    // ---- rung 6: the holder's faces over the ONE bitmap (0002 row 237) ----

    /// The bit words themselves: the allocation holder's page images are
    /// serialized from them (508 words per `KVDA` page) — there is no second
    /// bitmap and no drain between two.
    pub fn bitmapWords(self: *const BlockAllocator) []const Atomic(u64) {
        return self.bits;
    }

    /// A carve's SET of one block, called under the holder's ordering lock:
    /// the bit 0→1; a free-listed block leaves the list (budget debited, its
    /// elided debt cancelled — a reused block owes no discard) and reads
    /// `virgin` until a mint claims it. The one bit-before-budget consumer:
    /// every publication's bit and credit move under the same lock
    /// (`holderPublishFree`, `holderReturnBit`), so the credit is there.
    /// `false` ⇒ already set (not carvable).
    pub fn holderTakeBit(self: *BlockAllocator, idx: u64) bool {
        if (!self.tryTakeBit(idx)) return false;
        const c = self.cell(idx);
        if (c.getState() == .free) {
            self.debitFree();
            self.cancelElidedDebt(idx);
        }
        c.setState(.virgin);
        return true;
    }

    /// The finish-free sink's half, under the holder's ordering lock: the
    /// bit 1→0, then the credit when the cell reads `free` (the state
    /// `publishFreeList` stored before calling the sink) — both inside the
    /// lock a carve takes, so the carve's debit always finds its credit
    /// (`free_list_model.zig` model 2). `false` ⇒ already clear (a double
    /// free; nothing credited).
    pub fn holderPublishFree(self: *BlockAllocator, idx: u64) bool {
        if (!self.clearBit(idx)) return false;
        if (self.cell(idx).getState() == .free) self.creditListed(idx);
        return true;
    }

    /// The non-carve take's body, under the holder's ordering lock when
    /// armed (the sink calls it with the lock held), in place otherwise:
    /// the cell still `free` (`virgin` for an unlisted block, a clear bit
    /// at/above the cursor), an entitlement won for a listed block, then
    /// the bit 0→1 — entitlement before bit, a lost take refunded. False ⇒
    /// lost (the state moved, no credit, or the bit already set).
    pub fn holderTakeOutOfList(self: *BlockAllocator, idx: u64, listed: bool) bool {
        if (self.cell(idx).getStateSeqCst() != (if (listed) State.free else State.virgin)) return false;
        if (listed and !self.winEntitlement()) return false;
        if (self.tryTakeBit(idx)) return true;
        if (listed) self.creditFree();
        return false;
    }

    /// A trim window's return of its bit and credit (the state stored
    /// `free` before), under the holder's lock when armed. False ⇒ the bit
    /// was already clear.
    pub fn holderReturnTrim(self: *BlockAllocator, idx: u64) bool {
        if (!self.clearBit(idx)) return false;
        self.creditListed(idx);
        return true;
    }

    /// A `ReturnBlocks` / adopt-release CLEAR of a block never minted here:
    /// the state (free below the cursor — the local list sees it — virgin
    /// at/above), then the bit 1→0, then the credit (the publication's
    /// order, module doc). `false` ⇒ already clear: the state stored reads
    /// what a clear bit means.
    pub fn holderReturnBit(self: *BlockAllocator, idx: u64) bool {
        const listed = idx < self.highest.load(.acquire);
        self.cell(idx).setState(if (listed) .free else .virgin);
        if (!self.clearBit(idx)) return false;
        if (listed) self.creditListed(idx);
        return true;
    }

    /// The lowest clear run of length ≤ `want` starting at or above `from`
    /// (the carve's scan: all-ones words skipped, the run truncated at
    /// `want` or the first set bit); null when no clear bit remains.
    pub fn lowestClearRun(self: *const BlockAllocator, from: u64, want: u64) ?struct { start: u64, len: u64 } {
        const start = self.lowestClear(from, self.capacity_blocks) orelse return null;
        var end = start + 1;
        while (end < self.capacity_blocks and end - start < want and !self.bitIsSet(end)) : (end += 1) {}
        return .{ .start = start, .len = end - start };
    }

    /// The highest set bit + 1 (0 when none): the holder's virgin floor.
    pub fn highestSetPlusOne(self: *const BlockAllocator) u64 {
        var w: usize = self.bits.len;
        while (w > 0) {
            w -= 1;
            const v = self.bits[w].load(.acquire);
            if (v != 0) return @as(u64, w) * 64 + (64 - @clz(v));
        }
        return 0;
    }

    /// The popcount of the bitmap (O(words); the census oracle and the
    /// once-per-cycle population drift check — never the funnel).
    pub fn bitPopulation(self: *const BlockAllocator) u64 {
        var n: u64 = 0;
        for (self.bits) |*w| n += @popCount(w.load(.acquire));
        return n;
    }

    // ---- rung 6: the dead-epoch quarantine (§4.4; 0002 row 245) ----

    /// Admit a block to the cohort `dead_epoch`: a free-listed block leaves
    /// the free list (bit taken, budget debited), a never-minted granted
    /// block keeps its set bit; both read `quarantined` with the epoch in
    /// the dead stamp word and OWE a publish to the release. A block in
    /// limbo or with a live owner carries the flag alone — its future
    /// `finishFree` defers. Idempotent: `false` ⇒ already admitted.
    pub fn quarantineBlock(self: *BlockAllocator, idx: u64, dead_epoch: u64) bool {
        assert(idx < self.capacity_blocks);
        const c = self.cell(idx);
        // The admission's half of the gate's Dekker pair (0003 §Y.3): the
        // flag RMW, then the state read, both seq_cst.
        if (c.flags.fetchOr(FLAG_QUARANTINED, .seq_cst) & FLAG_QUARANTINED != 0) return false;
        _ = self.quarantine_count.fetchAdd(1, .acq_rel);
        stats.dlm_quarantined_offsets.inc();
        switch (c.getStateSeqCst()) {
            .free => {
                // The take: entitlement, then the bit (under the holder's
                // lock when armed). A lost take is a claim that won the
                // block (a live owner: the flag alone) or a publication
                // between its state store and its credit — the flag alone,
                // and that publication's re-check takes the block back.
                if (!self.takeOutOfList(idx, true, true)) return true;
                self.cancelElidedDebt(idx);
                c.stamp.store(dead_epoch, .release);
                c.setState(.quarantined);
                _ = c.flags.fetchOr(FLAG_QUARANTINE_OWED, .acq_rel);
            },
            .virgin => {
                // A granted, never-minted block keeps the set bit its grant
                // took; a clear one is taken (the take's lost verdict on a
                // set bit is that case).
                _ = self.takeOutOfList(idx, false, true);
                c.stamp.store(dead_epoch, .release);
                c.setState(.quarantined);
                _ = c.flags.fetchOr(FLAG_QUARANTINE_OWED, .acq_rel);
            },
            .allocated, .limbo, .trim_windowed, .quarantined => {},
        }
        return true;
    }

    pub fn isQuarantined(self: *BlockAllocator, idx: u64) bool {
        return self.cell(idx).flags.load(.acquire) & FLAG_QUARANTINED != 0;
    }

    /// Release the cohort `dead_epoch`: every `quarantined` cell carrying the
    /// epoch (a bitmap/state scan, never a hash walk) and every flagged
    /// cell in `live` (the live-owner admissions the caller kept) leave
    /// the quarantine; a cell that OWES a publish moves to `limbo` and its
    /// index goes to `out` — the caller runs `finishFree` on each (grace
    /// ring → sink → the bit clears). Returns the count written.
    pub fn releaseQuarantine(self: *BlockAllocator, dead_epoch: u64, live: []const u64, out: []u64) usize {
        var n: usize = 0;
        if (self.quarantine_count.load(.acquire) == 0) return 0;
        var idx: u64 = 0;
        while (idx < self.capacity_blocks) : (idx += 1) {
            const c = self.cell(idx);
            if (c.getState() != .quarantined or c.stamp.load(.acquire) != dead_epoch) continue;
            n = self.releaseQuarantinedCell(idx, out, n);
        }
        for (live) |l| if (l < self.capacity_blocks and self.isQuarantined(l)) {
            n = self.releaseQuarantinedCell(l, out, n);
        };
        stats.dlm_quarantine_releases.inc();
        return n;
    }

    fn releaseQuarantinedCell(self: *BlockAllocator, idx: u64, out: []u64, n: usize) usize {
        const c = self.cell(idx);
        const owed = c.flags.load(.acquire) & FLAG_QUARANTINE_OWED != 0;
        // A full `out` leaves the cell IN the cohort (flag and epoch kept)
        // for the caller's next pass — never a lost bit.
        if (owed and n >= out.len) return n;
        const prev = c.flags.fetchAnd(~(FLAG_QUARANTINED | FLAG_QUARANTINE_OWED), .acq_rel);
        if (prev & FLAG_QUARANTINED == 0) return n;
        _ = self.quarantine_count.fetchSub(1, .acq_rel);
        if (prev & FLAG_QUARANTINE_OWED == 0) return n;
        if (c.getState() == .quarantined) {
            c.stamp.store(STAMP_NONE, .release);
            Incarnation.retire(&c.incarnation);
        }
        c.setState(.limbo);
        out[n] = idx;
        return n + 1;
    }

    /// Sorted free indices below the cursor (a census face).
    pub fn freeBlockIndices(self: *const BlockAllocator, gpa: Allocator) Allocator.Error![]u64 {
        var list: std.ArrayList(u64) = .empty;
        errdefer list.deinit(gpa);
        var from: u64 = 0;
        const highest = self.highest.load(.acquire);
        while (self.lowestClear(from, highest)) |idx| : (from = idx + 1) try list.append(gpa, idx);
        return list.toOwnedSlice(gpa);
    }

    // ---- the StorageFull latch (§4.1) ----

    /// The TERMINAL verdict's landing: CAS 0→now, bump the process count;
    /// INFO only when no supply is visible (otherwise the latch is inert).
    fn noteFreshSupplyExhausted(self: *BlockAllocator) void {
        const now = @max(clock.monotonicNs(), 1);
        // Monotonic: a census hint read against the reader's own clock, no
        // paired fence (the wall's verdict is the latch census, not this).
        self.last_storage_full_ns.store(now, .monotonic);
        if (self.fresh_supply_exhausted_since_ns.cmpxchgStrong(0, now, .acq_rel, .monotonic) == null) {
            _ = fresh_supply_latched.fetchAdd(1, .acq_rel);
            if (!self.freshSupplyVisible()) log.info("data volume {d}: fresh supply EXHAUSTED (StorageFull latched; writes that would create custody are refused ENOSPC until an allocation lands)", .{self.cfg.vol});
        }
    }

    /// The next landed allocation clears the latch.
    fn noteFreshSupplyAvailable(self: *BlockAllocator) void {
        if (self.fresh_supply_exhausted_since_ns.swap(0, .acq_rel) != 0) {
            _ = fresh_supply_latched.fetchSub(1, .acq_rel);
        }
    }

    /// `free_supply > 0 ∨ grace non-empty ∨ trim windows open ∨ reclaim
    /// pending` — the conjuncts ARE the mint sources; a source added later
    /// must join them (BA:L683-688).
    pub fn freshSupplyVisible(self: *const BlockAllocator) bool {
        if (self.freeSupplyBlocks() > 0) return true;
        if (self.grace) |g| if (g.vtable.len(g.ctx) > 0) return true;
        if (self.trim_claimed.load(.seq_cst) > 0) return true;
        if (self.space_pending) |p| if (p.pending(p.ctx)) return true;
        return false;
    }

    /// PURE: a mint met `StorageFull` at or after `since_ns` (monotonic).
    pub fn storageFullSince(self: *const BlockAllocator, since_ns: u64) bool {
        const at = self.last_storage_full_ns.load(.monotonic);
        return at != 0 and at >= since_ns;
    }

    /// PURE: latched ∧ !visible.
    pub fn freshSupplyLatched(self: *const BlockAllocator) bool {
        return self.fresh_supply_exhausted_since_ns.load(.acquire) != 0 and !self.freshSupplyVisible();
    }

    /// The census conjunct: latched (inert while supply is visible); on a
    /// grant-armed writer kicks the proactive re-ask. There is deliberately
    /// NO time-bounded re-probe that admits a write: an admitted write on a
    /// still-full volume is acked custody that can never land (BA:L631-636).
    pub fn freshSupplyExhausted(self: *BlockAllocator) bool {
        if (!self.freshSupplyLatched()) return false;
        if (self.grant_source) |g| if (g.vtable.wantsTopup(g.ctx)) g.vtable.noteExhausted(g.ctx);
        return true;
    }

    // ---- fsck seams (§5; §11 item 12) ----

    pub fn fsckBeginScan(self: *BlockAllocator) Allocator.Error!void {
        if (self.epoch_map == null) {
            const m = try self.gpa.alloc(Atomic(u64), self.cells.len);
            @memset(m, .init(0));
            self.epoch_map = m;
        }
        _ = self.fsck_epoch.fetchAdd(1, .acq_rel);
        self.fsck_scan_active.store(true, .release);
    }

    pub fn fsckBumpEpoch(self: *BlockAllocator) u64 {
        return self.fsck_epoch.fetchAdd(1, .acq_rel) + 1;
    }

    pub fn fsckEndScan(self: *BlockAllocator) void {
        self.fsck_scan_active.store(false, .release);
    }

    /// The allocation epoch a block was minted under while a scan was
    /// latched (0 = none): fsck C2/C3 exempt blocks younger than the scan.
    pub fn allocationEpochOf(self: *const BlockAllocator, idx: u64) u64 {
        const m = self.epoch_map orelse return 0;
        return m[@intCast(idx)].load(.acquire);
    }

    pub fn inflightContains(self: *BlockAllocator, idx: u64) bool {
        return self.cell(idx).inflight.load(.acquire) != 0;
    }

    /// One registration in the in-flight registry — the list-shaped twin
    /// of `InflightAllocGuard` for a site that carries its released
    /// bindings from before its commit to after their frees (decision
    /// 0003 §AH.1). acq_rel like the guard's: ordered before the releasing
    /// commit a census may read.
    pub fn holdInflight(self: *BlockAllocator, idx: u64) void {
        _ = self.cell(idx).inflight.fetchAdd(1, .acq_rel);
    }

    /// The registration `holdInflight` took. A drop with none held is a
    /// pairing break upstream: refused (the count never wraps into a
    /// shield nobody drops) and counted.
    pub fn dropInflight(self: *BlockAllocator, idx: u64) void {
        const c = self.cell(idx);
        var cur = c.inflight.load(.monotonic);
        while (true) {
            if (cur == 0) {
                stats.block_inflight_unheld_drops.fire("an in-flight registration dropped with none held");
                return;
            }
            cur = c.inflight.cmpxchgWeak(cur, cur - 1, .acq_rel, .monotonic) orelse return;
        }
    }

    pub fn fsckSetRefcount(self: *BlockAllocator, idx: u64, count: u32) void {
        assert(count != 0);
        self.cell(idx).refcount.store(count, .release);
    }

    pub const Reconciled = struct { frees_completed: u64, free_list_evictions: u64 };

    /// `fsck_reconcile_accounting`: untracked ∧ allocated ∧ no in-flight
    /// owner ∧ not graced ⇒ complete the free; tracked ∧ free-listed ⇒
    /// evict from the free list (BA:L1517-1544).
    pub fn fsckReconcileAccounting(self: *BlockAllocator) Reconciled {
        var out: Reconciled = .{ .frees_completed = 0, .free_list_evictions = 0 };
        const highest = self.highest.load(.acquire);
        var idx: u64 = 0;
        while (idx < highest) : (idx += 1) {
            const c = self.cell(idx);
            const tracked = Refcount.peek(&c.refcount) != 0;
            switch (c.getState()) {
                .allocated => if (!tracked and !self.inflightContains(idx)) {
                    const graced = if (self.grace) |g| g.vtable.holds(g.ctx, idx) else false;
                    if (graced) continue;
                    Incarnation.retire(&c.incarnation);
                    c.setState(.limbo);
                    self.publishFreeList(idx, .seed_free);
                    out.frees_completed += 1;
                },
                .free => if (tracked and self.winEntitlement()) {
                    // Entitlement, then the bit (module doc).
                    if (self.tryTakeBit(idx)) {
                        c.setState(.allocated);
                        out.free_list_evictions += 1;
                    } else self.creditFree();
                },
                else => {},
            }
        }
        return out;
    }

    pub fn stateOf(self: *BlockAllocator, idx: u64) State {
        return self.cell(idx).getState();
    }

    pub fn forensicsTape(self: *const BlockAllocator) ?[]const ForensicRecord {
        const t = self.forensics orelse return null;
        return t.records[0..@min(t.cursor.load(.monotonic), t.records.len)];
    }
};

// ---------------------------------------------------------------------------
// Guards (§5).
// ---------------------------------------------------------------------------

/// fsck VISIBILITY ONLY: a live owner of a block whose tracking and durable
/// references disagree by construction — a mint before its publish lands,
/// a RELEASE from before its releasing commit until its free (decision
/// 0003 §AH.1), a queued free. Dropping it frees NOTHING (KD-OV-11); a
/// crashed owner drops it on unwind so a genuinely leaked block is never
/// shielded.
pub const InflightAllocGuard = struct {
    alloc: *BlockAllocator,
    idx: u64,
    armed: bool,

    pub fn register(alloc: *BlockAllocator, idx: u64) InflightAllocGuard {
        _ = alloc.cell(idx).inflight.fetchAdd(1, .acq_rel);
        return .{ .alloc = alloc, .idx = idx, .armed = true };
    }

    pub fn deinit(self: *InflightAllocGuard) void {
        if (!self.armed) return;
        self.armed = false;
        _ = self.alloc.cell(self.idx).inflight.fetchSub(1, .acq_rel);
    }
};

/// The mint's ROLLBACK owner (Law 9): disarmed solely when durable map/ref
/// publication transfers ownership; an armed guard's `abandon` is the one
/// path an unpublished mint returns by (synchronous — Q13: every mint owner
/// returns its block through an explicit `errdefer`, never a detached task).
pub const MintedBlockGuard = struct {
    alloc: *BlockAllocator,
    minted: Minted,
    armed: bool,

    pub fn init(alloc: *BlockAllocator, minted: Minted) MintedBlockGuard {
        return .{ .alloc = alloc, .minted = minted, .armed = true };
    }

    /// Durable publication landed: the map/refs own the block now.
    pub fn disarm(self: *MintedBlockGuard) void {
        self.armed = false;
    }

    /// The error path: nothing published, no DMA naming the block can
    /// still land (the caller awaited its CQEs).
    pub fn abandon(self: *MintedBlockGuard) void {
        if (!self.armed) return;
        self.armed = false;
        self.alloc.abandonUnpublished(self.minted.idx);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testAlloc(blocks: u64) !*BlockAllocator {
    return BlockAllocator.init(testing.allocator, .{ .vol_tag = 7, .vol = 0, .block_size = 4096, .capacity_bytes = blocks * 4096 });
}

test "the incarnation word: retire/publish/snapshot/still arithmetic (IC:L27-83)" {
    var w: Atomic(u64) = .init(0);
    try testing.expectEqual(@as(?u64, null), Incarnation.snapshot(&w));
    Incarnation.retire(&w);
    try testing.expectEqual(UNSTABLE_FIRST, w.load(.monotonic));
    try testing.expectEqual(@as(?u64, null), Incarnation.snapshot(&w));
    Incarnation.publish(&w);
    try testing.expectEqual(UNSTABLE_FIRST | 1, w.load(.monotonic));
    const snap = Incarnation.snapshot(&w).?;
    try testing.expect(Incarnation.still(&w, snap));
    Incarnation.retire(&w);
    try testing.expect(!Incarnation.still(&w, snap));
    try testing.expectEqual(@as(u64, 2), Incarnation.generation(w.load(.monotonic)));
    var fresh: Atomic(u64) = .init(0);
    Incarnation.publish(&fresh);
    try testing.expectEqual(STABLE_FIRST, fresh.load(.monotonic));
}

test "the refcount cell: acquire never resurrects 0; release is terminal exactly once and saturates" {
    var c: Atomic(u32) = .init(0);
    try testing.expect(!Refcount.tryAcquire(&c));
    c.store(2, .monotonic);
    try testing.expect(Refcount.tryAcquire(&c));
    try testing.expectEqual(@as(u32, 3), Refcount.peek(&c));
    try testing.expect(!Refcount.release(&c));
    try testing.expect(!Refcount.release(&c));
    try testing.expect(Refcount.release(&c));
    try testing.expect(!Refcount.release(&c));
    try testing.expectEqual(@as(u32, 0), Refcount.peek(&c));
}

test "allocate_block_respects_device_capacity: three mints then StorageFull at cap; a free reopens exactly one slot" {
    const a = try testAlloc(3);
    defer a.deinit();
    const m0 = try a.allocateBlock();
    const m1 = try a.allocateBlock();
    const m2 = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 0), m0.idx);
    try testing.expectEqual(@as(u64, 4096), m1.offset);
    try testing.expectEqual(@as(u64, 2), m2.idx);
    try testing.expectError(error.StorageFull, a.allocateBlock());
    try testing.expect(a.freshSupplyLatched());
    try testing.expectEqual(@as(u64, 0), a.virginBytes());
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(1));
    try testing.expect(!a.freshSupplyLatched()); // supply visible ⇒ inert
    const again = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 1), again.idx);
    try testing.expect(again.stamp != m1.stamp);
    try testing.expectError(error.StorageFull, a.allocateBlock());
    try testing.expectEqual(@as(u64, 3), a.usedBlocks());
}

test "the free list is the lowest clear bit below the cursor; the fresh mint never runs while it is non-empty" {
    const a = try testAlloc(64);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 10) : (i += 1) _ = try a.allocateBlock();
    _ = try a.freeBlock(7);
    _ = try a.freeBlock(3);
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 3), (try a.allocateBlock()).idx);
    try testing.expectEqual(@as(u64, 7), (try a.allocateBlock()).idx);
    try testing.expectEqual(@as(u64, 10), (try a.allocateBlock()).idx);
    try testing.expectEqual(@as(u64, 11), a.highestBlockIndex());
}

test "claim ordering law: the stamp and the word move before the refcount; a reallocated block names a new lifetime" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    try testing.expect(m.stamp != STAMP_NONE);
    try testing.expectEqual(@as(?u32, 1), a.refcount(m.idx));
    try testing.expect(a.liveIncarnation(m.idx, m.stamp));
    try testing.expectEqual(@as(?u64, null), a.fillIncarnation(m.idx)); // unstable until publish
    a.publishBlock(m.idx);
    const snap = a.fillIncarnation(m.idx).?;
    try testing.expect(a.fillIncarnationStill(m.idx, snap));
    _ = try a.freeBlock(m.idx);
    try testing.expect(!a.fillIncarnationStill(m.idx, snap)); // retired before reallocatable
    const m2 = try a.allocateBlock();
    try testing.expectEqual(m.idx, m2.idx);
    try testing.expect(!a.liveIncarnation(m2.idx, m.stamp));
    try testing.expect(a.liveIncarnation(m2.idx, m2.stamp));
    // A never-touched block snapshots UNKNOWN_STABLE and validates as such.
    try testing.expectEqual(UNKNOWN_STABLE, a.fillIncarnation(3).?);
    try testing.expect(a.fillIncarnationStill(3, UNKNOWN_STABLE));
}

test "a local lifetime that ENDED leaves the word stable under its retired generation: another member's re-mint of the block validates here (never a tier deposit), a local re-claim retires it under the racing fill, a never-touched cell keeps its deposit" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    a.publishBlock(m.idx);
    const live = a.fillIncarnation(m.idx).?;
    try testing.expect(a.fillDepositable(m.idx));
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(m.idx));
    try testing.expect(!a.fillIncarnationStill(m.idx, live));
    // The free list holds it: no transition runs here, and a wrapped carve
    // may hand it to another member whose mint this word never sees — the
    // snapshot validates (the binding proves the serve).
    const foreign = a.fillIncarnation(m.idx) orelse return error.TestUnexpectedResult;
    try testing.expectEqual(Incarnation.generation(live) + 1, Incarnation.generation(foreign));
    try testing.expect(a.fillIncarnationStill(m.idx, foreign));
    try testing.expect(!a.fillDepositable(m.idx));
    // This member's own re-claim retires the word: the racing fill fails.
    const m2 = try a.allocateBlock();
    try testing.expectEqual(m.idx, m2.idx);
    try testing.expectEqual(@as(?u64, null), a.fillIncarnation(m2.idx));
    try testing.expect(!a.fillIncarnationStill(m2.idx, foreign));
    a.publishBlock(m2.idx);
    try testing.expect(a.fillDepositable(m2.idx));
    try testing.expectEqual(UNKNOWN_STABLE, a.fillIncarnation(3).?);
    try testing.expect(a.fillDepositable(3));
}

test "an abandoned mint leaves its lifetime BEFORE the grant window can hand it out: a mint that takes it from the window head at once claims a clean cell (refcount 1, allocated, unstable) and nothing clobbers it after" {
    const a = try testAlloc(16);
    defer a.deinit();
    const Window = struct {
        alloc: *BlockAllocator,
        next: u64 = 4,
        end: u64 = 8,
        racer: ?Minted = null,
        fn mint(ctx: *anyopaque) MintAnswer {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            if (w.next >= w.end) return .full;
            w.next += 1;
            return .{ .minted = w.next - 1 };
        }
        /// The block is back at the window head; another minter takes it
        /// before the abandoning thread returns (the window's lock is all
        /// that orders them).
        fn giveBack(ctx: *anyopaque, idx: u64) bool {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            w.next = idx;
            w.racer = w.alloc.allocateBlock() catch unreachable;
            return true;
        }
        fn remaining(ctx: *anyopaque) u64 {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            return w.end - w.next;
        }
        fn wantsTopup(_: *anyopaque) bool {
            return false;
        }
        fn noteExhausted(_: *anyopaque) void {}
        const vtable: GrantSource.VTable = .{ .mint = &mint, .giveBack = &giveBack, .remaining = &remaining, .wantsTopup = &wantsTopup, .noteExhausted = &noteExhausted };
    };
    var w: Window = .{ .alloc = a };
    a.grant_source = .{ .ctx = &w, .vtable = &Window.vtable };
    const m = try a.allocateBlock();
    const anomalies = counters.tripwireCount("block_claim_anomalies");
    var g = MintedBlockGuard.init(a, m);
    g.abandon();
    const r = w.racer orelse return error.TestUnexpectedResult;
    try testing.expectEqual(m.idx, r.idx);
    try testing.expectEqual(anomalies, counters.tripwireCount("block_claim_anomalies"));
    try testing.expectEqual(@as(?u32, 1), a.refcount(r.idx));
    try testing.expectEqual(State.allocated, a.stateOf(r.idx));
    try testing.expectEqual(@as(?u64, null), a.fillIncarnation(r.idx)); // the racer's DMA is in flight
    try testing.expect(a.liveIncarnation(r.idx, r.stamp));
}

test "double release: the second begin_free is refused loud and frees nothing; a double finish is a tripwire" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(m.idx));
    const before = counters.tripwireCount("block_untracked_free_refusals");
    stats.block_untracked_free_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2); // the tripwire's line + the refusal's
    try testing.expectEqual(BeginFree.refused, try a.freeBlock(m.idx));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, counters.tripwireCount("block_untracked_free_refusals"));
    try testing.expectEqual(@as(u64, 1), a.freeBlocksCount());
    // A direct second finish_free on a free block is the DOUBLE FREE tripwire.
    const dbl = counters.tripwireCount("block_double_frees");
    stats.block_double_frees.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    a.finishFree(m.idx);
    try log.testing.expectAllFired();
    try testing.expectEqual(dbl + 1, counters.tripwireCount("block_double_frees"));
    try testing.expectEqual(@as(u64, 1), a.freeBlocksCount());
}

test "W1 pair: pin → snapshot sees unstable mid-patch and retries bounded; sole-owner patch refuses a pinned block" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    a.publishBlock(m.idx);
    try testing.expectEqual(PinVerdict.pinned, a.pinBlockValidated(m.idx));
    try testing.expectEqual(@as(?u32, 2), a.refcount(m.idx));
    // Pinned ⇒ not a sole owner; the patch must re-publish and CoW.
    try testing.expect(!(try a.beginPatchSoleOwner(m.idx)));
    a.publishBlock(m.idx);
    a.unpin(m.idx);
    try testing.expect(try a.beginPatchSoleOwner(m.idx));
    // Mid-patch (unstable): a clone pins, sees unstable, unpins; 3 attempts.
    const before = stats.block_pin_stale_keys.load();
    try testing.expectEqual(PinVerdict.pinned_unstable, a.pinBlockRetrying(m.idx));
    try testing.expectEqual(before + 3, stats.block_pin_stale_keys.load());
    try testing.expectEqual(@as(?u32, 1), a.refcount(m.idx));
    a.publishBlock(m.idx);
    try testing.expectEqual(PinVerdict.pinned, a.pinBlockRetrying(m.idx));
    a.unpin(m.idx);
    // A freed block is never resurrected by a pin.
    _ = try a.freeBlock(m.idx);
    try testing.expectEqual(PinVerdict.refused, a.pinBlockValidated(m.idx));
    try testing.expect(!a.incrementRefcount(m.idx));
}

test "elided debt rides the cell and the debt bitmap; a claim cancels it; the batch is lowest-first" {
    const a = try testAlloc(64);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 6) : (i += 1) _ = try a.allocateBlock();
    for ([_]u64{ 5, 1, 3 }) |idx| {
        _ = try a.beginFree(idx);
        a.recordElidedDebt(idx);
        a.finishFree(idx);
    }
    try testing.expectEqual(@as(u64, 3 * 4096), a.elidedDebtBytes());
    var buf: [8]u64 = undefined;
    try testing.expectEqual(@as(usize, 3), a.takeDebtBatch(&buf));
    try testing.expectEqualSlices(u64, &.{ 1, 3, 5 }, buf[0..3]);
    try testing.expectEqual(@as(u64, 1), (try a.allocateBlock()).idx);
    try testing.expectEqual(@as(u64, 2 * 4096), a.elidedDebtBytes());
    try testing.expectEqual(@as(usize, 2), a.takeDebtBatch(&buf));
    try testing.expectEqualSlices(u64, &.{ 3, 5 }, buf[0..2]);
    a.recordElidedDebt(3); // idempotent
    try testing.expectEqual(@as(u64, 2 * 4096), a.elidedDebtBytes());
}

test "trim claim window: the claimed block leaves the free list, stays reachable, and returns before the window closes" {
    const a = try testAlloc(8);
    defer a.deinit();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    _ = try a.freeBlock(0);
    try testing.expectEqual(@as(u64, 7), a.freeSupplyBlocks());
    var g = a.claimFreeForTrim(0).?;
    try testing.expectEqual(@as(u64, 1), a.trimClaimed());
    try testing.expectEqual(@as(u64, 6), a.freeSupplyBlocks());
    try testing.expectEqual(@as(u64, 7), a.reachableFreeBlocks());
    try testing.expectEqual(State.trim_windowed, a.stateOf(0));
    try testing.expect(a.inflightContains(0));
    try testing.expect(a.claimFreeForTrim(0) == null); // a racing trim loses
    try testing.expect(a.claimFreeForTrim(1) == null); // an allocated block is never trimmed
    try testing.expectEqual(@as(u64, 1), a.trimClaimed());
    a.returnFromTrim(0, &g);
    try testing.expectEqual(@as(u64, 0), a.trimClaimed());
    try testing.expect(!a.inflightContains(0));
    try testing.expectEqual(@as(u64, 0), (try a.allocateBlock()).idx);
}

test "StorageFull latch: inert while supply is visible, terminal when nothing is; a trim window is supply" {
    const a = try testAlloc(2);
    defer a.deinit();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    const before = fresh_supply_latched.load(.monotonic);
    try testing.expectError(error.StorageFull, a.allocateBlock());
    try testing.expectEqual(before + 1, fresh_supply_latched.load(.monotonic));
    try testing.expect(a.freshSupplyExhausted());
    _ = try a.freeBlock(1);
    try testing.expect(!a.freshSupplyExhausted()); // visible ⇒ inert, still latched
    var g = a.claimFreeForTrim(1).?;
    try testing.expect(a.freshSupplyVisible()); // the window is pending supply
    a.returnFromTrim(1, &g);
    _ = try a.allocateBlock(); // clears the latch
    try testing.expectEqual(before, fresh_supply_latched.load(.monotonic));
    try testing.expect(!a.freshSupplyExhausted());
}

test "a dropped latched allocator leaves the process count" {
    const before = fresh_supply_latched.load(.monotonic);
    {
        const a = try testAlloc(1);
        defer a.deinit();
        _ = try a.allocateBlock();
        try testing.expectError(error.StorageFull, a.allocateBlock());
        try testing.expectEqual(before + 1, fresh_supply_latched.load(.monotonic));
    }
    try testing.expectEqual(before, fresh_supply_latched.load(.monotonic));
}

test "guards: the in-flight guard is visibility only; the mint guard's abandon returns the block synchronously, disarm does not" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    var inflight = InflightAllocGuard.register(a, m.idx);
    var mint = MintedBlockGuard.init(a, m);
    try testing.expect(a.inflightContains(m.idx));
    inflight.deinit();
    try testing.expect(!a.inflightContains(m.idx));
    try testing.expectEqual(State.allocated, a.stateOf(m.idx)); // dropping visibility freed nothing
    const before = stats.unpublished_mint_abandons.load();
    mint.abandon();
    try testing.expectEqual(before + 1, stats.unpublished_mint_abandons.load());
    try testing.expectEqual(State.free, a.stateOf(m.idx));
    mint.abandon(); // idempotent
    const m2 = try a.allocateBlock();
    var g2 = MintedBlockGuard.init(a, m2);
    g2.disarm();
    g2.abandon();
    try testing.expectEqual(State.allocated, a.stateOf(m2.idx));
}

test "a release's in-flight registrations (decision 0003 §AH.1): holds stack with the guard's on one cell, each drop ends one, and a drop with none held is refused (the count never wraps into a shield nobody drops) and counted" {
    const a = try testAlloc(4);
    defer a.deinit();
    const m = try a.allocateBlock();
    var mint = InflightAllocGuard.register(a, m.idx);
    a.holdInflight(m.idx);
    a.holdInflight(m.idx);
    mint.deinit();
    a.dropInflight(m.idx);
    try testing.expect(a.inflightContains(m.idx));
    a.dropInflight(m.idx);
    try testing.expect(!a.inflightContains(m.idx));
    const before = counters.tripwireCount("block_inflight_unheld_drops");
    stats.block_inflight_unheld_drops.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    a.dropInflight(m.idx);
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, counters.tripwireCount("block_inflight_unheld_drops"));
    try testing.expect(!a.inflightContains(m.idx));
    a.holdInflight(m.idx);
    try testing.expect(a.inflightContains(m.idx));
    a.dropInflight(m.idx);
    try testing.expectEqual(State.allocated, a.stateOf(m.idx)); // visibility only
}

test "mount seed: recover_block gap-fills quiescently, counts references, and the runtime specific claim refuses a taken block" {
    const a = try testAlloc(16);
    defer a.deinit();
    var idxs = [_]u64{ 9, 2, 9, 5 };
    const before = stats.meta_kv_block_refs_recovered.load();
    a.seedFromDurableRefs(&idxs);
    try testing.expectEqual(before + 4, stats.meta_kv_block_refs_recovered.load());
    try testing.expectEqual(@as(u64, 10), a.highestBlockIndex());
    try testing.expectEqual(@as(?u32, 2), a.refcount(9));
    try testing.expectEqual(@as(?u32, 1), a.refcount(2));
    try testing.expectEqual(@as(u64, 7), a.freeBlocksCount()); // 0,1,3,4,6,7,8
    try testing.expectEqual(@as(u64, 3), a.usedBlocks());
    try testing.expectError(error.NotFree, a.allocateSpecificBlock(9));
    const m = try a.allocateSpecificBlock(4);
    try testing.expectEqual(@as(u64, 4), m.idx);
    try testing.expectEqual(@as(u64, 6), a.freeBlocksCount());
    // A specific claim above the cursor: the gap is counted recovered.
    const rec = stats.unpublished_offsets_recovered.load();
    _ = try a.allocateSpecificBlock(12);
    try testing.expectEqual(rec + 2, stats.unpublished_offsets_recovered.load());
    try testing.expectEqual(@as(u64, 13), a.highestBlockIndex());
    try testing.expectEqual(@as(u64, 8), a.freeBlocksCount());
    const free = try a.freeBlockIndices(testing.allocator);
    defer testing.allocator.free(free);
    try testing.expectEqualSlices(u64, &.{ 0, 1, 3, 6, 7, 8, 10, 11 }, free);
}

test "below / at-or-above picks; fragmentation; the derived snapshot is a copy below the cursor" {
    const a = try testAlloc(64);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 8) : (i += 1) _ = try a.allocateBlock();
    for ([_]u64{ 1, 2, 5 }) |idx| _ = try a.freeBlock(idx);
    try testing.expectEqual(@as(?u64, 1), try a.allocateBlockBelow(4));
    try testing.expectEqual(@as(?u64, null), try a.allocateBlockBelow(1));
    try testing.expectEqual(@as(u64, 5), (try a.allocateBlockAtOrAbove(3)).idx);
    try testing.expectEqual(@as(u64, 8), (try a.allocateBlockAtOrAbove(6)).idx);
    try testing.expectEqual(@as(u64, 0), a.calculateFragmentation()); // one free block ⇒ 0
    _ = try a.freeBlock(7);
    try testing.expectEqual(@as(u64, 1000), a.calculateFragmentation()); // 2 and 7: two runs of one
    const snap = try a.derivedAllocationSnapshot(testing.allocator);
    defer testing.allocator.free(snap.set_words);
    try testing.expectEqual(@as(u64, 9), snap.highest);
    try testing.expectEqual(@as(u64, 2), snap.free);
    try testing.expectEqual(@as(u64, 0b1_0111_1011), snap.set_words[0]); // 2 and 7 clear
}

test "fsck seams: the epoch side map stamps mints under a latched scan; reconciliation completes untracked allocated blocks and evicts tracked free ones" {
    const a = try testAlloc(8);
    defer a.deinit();
    const m0 = try a.allocateBlock();
    try a.fsckBeginScan();
    const m1 = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 0), a.allocationEpochOf(m0.idx));
    try testing.expectEqual(@as(u64, 1), a.allocationEpochOf(m1.idx));
    try testing.expectEqual(@as(u64, 2), a.fsckBumpEpoch());
    a.fsckEndScan();
    const m2 = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 0), a.allocationEpochOf(m2.idx));
    // An allocated block whose count fsck judged 0, and a free block fsck
    // judged referenced.
    a.cell(m0.idx).refcount.store(0, .release);
    _ = try a.freeBlock(m1.idx);
    a.fsckSetRefcount(m1.idx, 1);
    var g = InflightAllocGuard.register(a, m2.idx);
    a.cell(m2.idx).refcount.store(0, .release);
    const r = a.fsckReconcileAccounting();
    try testing.expectEqual(@as(u64, 1), r.frees_completed);
    try testing.expectEqual(@as(u64, 1), r.free_list_evictions);
    try testing.expectEqual(State.free, a.stateOf(m0.idx));
    try testing.expectEqual(State.allocated, a.stateOf(m1.idx));
    try testing.expectEqual(State.allocated, a.stateOf(m2.idx)); // the in-flight owner shields it
    g.deinit();
}

test "seams: read-only refuses every ownership mutation; a plane gate refusal is retryable and never latches StorageFull" {
    const ro = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = 4 * 4096, .read_only = true });
    defer ro.deinit();
    try testing.expectError(error.ReadOnly, ro.allocateBlock());
    try testing.expectEqual(@as(errors.Error, error.ReadOnlyFileSystem), coreError(error.ReadOnly));
    try testing.expectEqual(@as(errors.Error, error.NoSpaceLeft), coreError(error.StorageFull));
    try testing.expectEqual(errors.Class.retryable, errors.classOf(coreError(error.PlaneRefused)));
    const a = try testAlloc(4);
    defer a.deinit();
    const Gate = struct {
        fn refuse(_: ?*anyopaque, _: Site) bool {
            return false;
        }
    };
    a.plane_gate = .{ .ctx = null, .holds = &Gate.refuse };
    try testing.expectError(error.PlaneRefused, a.allocateBlock());
    try testing.expect(!a.freshSupplyExhausted());
    a.plane_gate = null;
    _ = try a.allocateBlock();
}

test "seams: a grant window is the ONE mint source when armed; a finish-free sink that answers false is a double free" {
    const a = try testAlloc(16);
    defer a.deinit();
    const Window = struct {
        next: u64 = 8,
        end: u64 = 10,
        given_back: u64 = 0,
        exhausted: u32 = 0,
        fn mint(ctx: *anyopaque) MintAnswer {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            if (w.next >= w.end) return if (w.exhausted == 0) .full else .retry;
            w.next += 1;
            return .{ .minted = w.next - 1 };
        }
        fn giveBack(ctx: *anyopaque, _: u64) bool {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            w.given_back += 1;
            return true;
        }
        fn remaining(ctx: *anyopaque) u64 {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            return w.end - w.next;
        }
        fn wantsTopup(_: *anyopaque) bool {
            return true;
        }
        fn noteExhausted(ctx: *anyopaque) void {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            w.exhausted += 1;
        }
        const vtable: GrantSource.VTable = .{ .mint = &mint, .giveBack = &giveBack, .remaining = &remaining, .wantsTopup = &wantsTopup, .noteExhausted = &noteExhausted };
    };
    var w: Window = .{};
    a.grant_source = .{ .ctx = &w, .vtable = &Window.vtable };
    try testing.expectEqual(@as(u64, 8), (try a.allocateBlock()).idx);
    try testing.expectEqual(@as(u64, 9), a.highestBlockIndex());
    try testing.expectEqual(@as(u64, 1), a.freeSupplyBlocks()); // the window's remainder (virgin tail is the holder's)
    const m9 = try a.allocateBlock();
    try testing.expectError(error.StorageFull, a.allocateBlock()); // the window's `full`: terminal
    try testing.expectEqual(@as(u32, 1), w.exhausted);
    try testing.expectError(error.PlaneRefused, a.allocateBlock()); // the window's `retry`: never a latch
    try testing.expectEqual(@as(u32, 2), w.exhausted);
    var g = MintedBlockGuard.init(a, m9);
    g.abandon();
    try testing.expectEqual(@as(u64, 1), w.given_back);
    try testing.expectEqual(State.virgin, a.stateOf(9)); // back in the window: unminted
    // The abandoned mint is no local lifetime: stable, never a deposit
    // (the window may return to the holder and another member mint it).
    try testing.expect(a.fillIncarnation(9) != null);
    try testing.expect(!a.fillDepositable(9));
    try testing.expectError(error.NotFree, a.allocateSpecificBlock(12)); // Q9: no runtime gap fill on an armed allocator
    a.grant_source = null;
    const Sink = struct {
        fn no(_: ?*anyopaque, _: u64, _: u64) bool {
            return false;
        }
    };
    a.finish_free_sink = .{ .ctx = null, .noteFinishFree = &Sink.no };
    const dbl = counters.tripwireCount("block_double_frees");
    stats.block_double_frees.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(8));
    try log.testing.expectAllFired();
    try testing.expectEqual(dbl + 1, counters.tripwireCount("block_double_frees"));
    // Nothing credited; the state, stored before the bit (0003 §W.1),
    // reads what the holder's already-clear bit means.
    try testing.expectEqual(State.free, a.stateOf(8));
    try testing.expectEqual(@as(u64, 0), a.freeBlocksCount());
}

test "the holder's free-list publication: a carve and a mint landing the instant the sink's critical section ends find the block published — the minted block reads allocated, the carve debited the one credit, nothing is clobbered (0003 §W.1)" {
    const a = try testAlloc(16);
    defer a.deinit();
    // The holder's own window over block 0; the same window serves the
    // re-mint the seam runs.
    const Window = struct {
        next: u64 = 0,
        end: u64 = 1,
        fn mint(ctx: *anyopaque) MintAnswer {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            if (w.next >= w.end) return .full;
            w.next += 1;
            return .{ .minted = w.next - 1 };
        }
        fn giveBack(_: *anyopaque, _: u64) bool {
            return false;
        }
        fn remaining(ctx: *anyopaque) u64 {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            return w.end - w.next;
        }
        fn wantsTopup(_: *anyopaque) bool {
            return false;
        }
        fn noteExhausted(_: *anyopaque) void {}
        const vtable: GrantSource.VTable = .{ .mint = &mint, .giveBack = &giveBack, .remaining = &remaining, .wantsTopup = &wantsTopup, .noteExhausted = &noteExhausted };
    };
    var w: Window = .{};
    a.grant_source = .{ .ctx = &w, .vtable = &Window.vtable };
    const first = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 0), first.idx);
    // The seam holds the publication's window: the sink runs the holder's
    // critical section, then — before `publishFreeList` resumes — the
    // carve takes the block (`DataBitmap.carveRun`'s take) and the holder's
    // window mints it.
    const Seam = struct {
        alloc: *BlockAllocator,
        window: *Window,
        reminted: ?u64 = null,
        fn note(ctx: ?*anyopaque, _: u64, idx: u64) bool {
            const s: *@This() = @ptrCast(@alignCast(ctx.?));
            if (!s.alloc.holderPublishFree(idx)) return false;
            if (!s.alloc.holderTakeBit(idx)) return true;
            s.window.* = .{ .next = idx, .end = idx + 1 };
            s.reminted = (s.alloc.allocateBlock() catch return true).idx;
            return true;
        }
    };
    var seam: Seam = .{ .alloc = a, .window = &w };
    a.finish_free_sink = .{ .ctx = &seam, .noteFinishFree = &Seam.note };
    const anomalies0 = counters.tripwireCount("block_claim_anomalies");
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(0));
    try testing.expectEqual(@as(?u64, 0), seam.reminted);
    // The re-minted lifetime stands: allocated, counted once, no credit.
    try testing.expectEqual(State.allocated, a.stateOf(0));
    try testing.expectEqual(@as(?u32, 1), a.refcount(0));
    try testing.expectEqual(@as(u64, 0), a.freeBlocksCount());
    try testing.expect(a.bitIsSet(0));
    try testing.expect(a.fillDepositable(0));
    try testing.expectEqual(anomalies0, counters.tripwireCount("block_claim_anomalies"));
    // A dead-epoch quarantine of the live block flags it and nothing else
    // (a `free` reading would have asserted on its lost take).
    try testing.expect(a.quarantineBlock(0, 3));
    try testing.expectEqual(State.allocated, a.stateOf(0));
    var out: [1]u64 = undefined;
    try testing.expectEqual(@as(usize, 0), a.releaseQuarantine(3, &.{0}, &out));
    a.finish_free_sink = null;
    a.grant_source = null;
}

test "the forensics tape pairs the two SITES of a double free without strings; the budget component is non-sheddable" {
    const a = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 3, .block_size = 4096, .capacity_bytes = 4 * 4096, .forensics = true });
    defer a.deinit();
    const m = try a.allocateBlock();
    _ = try a.freeBlock(m.idx);
    const tape = a.forensicsTape().?;
    try testing.expectEqual(@as(usize, 3), tape.len);
    try testing.expectEqual(Site.mint, tape[0].site);
    try testing.expectEqual(Site.terminal_free, tape[1].site);
    try testing.expectEqual(Site.terminal_free, tape[2].site);
    try testing.expectEqual(m.idx, tape[2].idx);
    try testing.expectEqual(@as(u64, @intCast(std.Thread.getCurrentId())), tape[1].thread_id);
    const comp = a.budgetComponent();
    try testing.expectEqual(@as(u64, 0), comp.weight);
    try testing.expectEqual(a.tableBytes(), comp.floor);
    try testing.expectEqual(@as(u64, 4 * 24 + 2 * 8), a.tableBytes());
}

test "concurrent claimers never share a block and the budget stays exact (budget-then-bit under contention)" {
    const a = try testAlloc(256);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 128) : (i += 1) _ = try a.allocateBlock();
    i = 0;
    while (i < 128) : (i += 2) _ = try a.freeBlock(i);
    const Worker = struct {
        fn run(alloc: *BlockAllocator, got: []u64) void {
            for (got) |*g| g.* = (alloc.allocateBlock() catch unreachable).idx;
        }
    };
    var results: [8][16]u64 = undefined;
    var threads: [8]std.Thread = undefined;
    for (&threads, 0..) |*t, k| t.* = try std.Thread.spawn(.{}, Worker.run, .{ a, &results[k] });
    for (threads) |t| t.join();
    var seen: [256]bool = @splat(false);
    for (results) |r| for (r) |idx| {
        try testing.expect(!seen[idx]);
        seen[idx] = true;
    };
    try testing.expectEqual(@as(u64, 0), a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 192), a.highestBlockIndex());
    try testing.expectEqual(@as(u64, 192), a.usedBlocks());
}

test "rung 6 holder faces: a carve's take leaves the free list and cancels debt, the return re-lists below the cursor, the run scan and the frontier read the ONE bitmap" {
    const a = try testAlloc(128);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 10) : (i += 1) _ = try a.allocateBlock();
    _ = try a.beginFree(3);
    a.recordElidedDebt(3);
    a.finishFree(3);
    _ = try a.freeBlock(7);
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 4096), a.elidedDebtBytes());
    // The lowest clear run at/above the frontier is the virgin tail; below
    // it the holes 3 and 7 are single-block runs.
    try testing.expectEqual(@as(u64, 10), a.highestSetPlusOne());
    const tail = a.lowestClearRun(10, 64).?;
    try testing.expectEqual(@as(u64, 10), tail.start);
    try testing.expectEqual(@as(u64, 64), tail.len);
    const hole = a.lowestClearRun(0, 64).?;
    try testing.expectEqual(@as(u64, 3), hole.start);
    try testing.expectEqual(@as(u64, 1), hole.len);
    try testing.expect(a.holderTakeBit(3));
    try testing.expect(!a.holderTakeBit(3));
    try testing.expectEqual(@as(u64, 1), a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 0), a.elidedDebtBytes());
    try testing.expectEqual(State.virgin, a.stateOf(3));
    try testing.expect(a.holderTakeBit(40));
    try testing.expectEqual(@as(u64, 41), a.highestSetPlusOne());
    try testing.expectEqual(@as(u64, 10), a.bitPopulation());
    // Returns: below the cursor the block is free-listed again; above it
    // the bit is simply clear (virgin).
    try testing.expect(a.holderReturnBit(3));
    try testing.expect(!a.holderReturnBit(3));
    try testing.expectEqual(State.free, a.stateOf(3));
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount());
    try testing.expect(a.holderReturnBit(40));
    try testing.expectEqual(State.virgin, a.stateOf(40));
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount());
    try testing.expectEqual(@as(usize, 2), a.bitmapWords().len);
    // The finish-free sink's half: the bit, and a credit only for a cell
    // its publication stored `free` (9 still reads allocated: none).
    try testing.expect(a.holderPublishFree(9));
    try testing.expect(!a.holderPublishFree(9));
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount());
}

test "rung 6 quarantine: a free block leaves the list under the epoch, a live block's finish defers, the cohort release hands the owed publishes back and nothing else" {
    const a = try testAlloc(16);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 6) : (i += 1) _ = try a.allocateBlock();
    _ = try a.freeBlock(1);
    const admitted0 = stats.dlm_quarantined_offsets.load();
    try testing.expect(a.quarantineBlock(1, 7)); // free ⇒ out of the list
    try testing.expect(!a.quarantineBlock(1, 7)); // idempotent
    try testing.expect(a.quarantineBlock(9, 7)); // never minted (a revoked grant's block)
    try testing.expect(a.quarantineBlock(4, 7)); // a live owner's block: flag only
    try testing.expectEqual(admitted0 + 3, stats.dlm_quarantined_offsets.load());
    try testing.expectEqual(@as(u64, 0), a.freeBlocksCount());
    try testing.expectEqual(State.quarantined, a.stateOf(1));
    try testing.expectEqual(State.quarantined, a.stateOf(9));
    try testing.expectEqual(State.allocated, a.stateOf(4));
    try testing.expect(a.isQuarantined(4));
    // A fresh mint never lands on a quarantined block (its bit is set).
    try testing.expectEqual(@as(u64, 6), (try a.allocateBlock()).idx);
    // The live block's terminal free defers its publish.
    const deferred0 = stats.block_quarantine_deferred_frees.load();
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(4));
    try testing.expectEqual(deferred0 + 1, stats.block_quarantine_deferred_frees.load());
    try testing.expectEqual(State.limbo, a.stateOf(4));
    try testing.expectEqual(@as(u64, 0), a.freeBlocksCount());
    // Another epoch's release touches nothing of this cohort.
    var out: [8]u64 = undefined;
    try testing.expectEqual(@as(usize, 0), a.releaseQuarantine(8, &.{}, &out));
    try testing.expect(a.isQuarantined(1));
    // The cohort release: 1 and 9 (the epoch in the stamp) plus the live
    // admission the caller names; every one owed a publish.
    const n = a.releaseQuarantine(7, &.{4}, &out);
    try testing.expectEqual(@as(usize, 3), n);
    std.mem.sort(u64, out[0..n], {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 1, 4, 9 }, out[0..n]);
    for (out[0..n]) |idx| {
        try testing.expectEqual(State.limbo, a.stateOf(idx));
        try testing.expect(!a.isQuarantined(idx));
        a.finishFree(idx);
    }
    try testing.expectEqual(@as(u64, 0), a.quarantine_count.load(.monotonic));
    try testing.expectEqual(@as(u64, 2), a.freeBlocksCount()); // 1 and 4 below the cursor; 9 is virgin again
    try testing.expectEqual(State.virgin, a.stateOf(9));
    try testing.expectEqual(@as(u64, 1), (try a.allocateBlock()).idx);
    // A bounded `out` leaves the remainder in the cohort for the next pass.
    _ = try a.freeBlock(2);
    try testing.expect(a.quarantineBlock(2, 11));
    try testing.expect(a.quarantineBlock(12, 11));
    var one: [1]u64 = undefined;
    try testing.expectEqual(@as(usize, 1), a.releaseQuarantine(11, &.{}, &one));
    try testing.expectEqual(@as(u64, 1), a.quarantine_count.load(.monotonic));
    try testing.expectEqual(@as(usize, 1), a.releaseQuarantine(11, &.{}, &one));
    try testing.expectEqual(@as(u64, 0), a.quarantine_count.load(.monotonic));
}

test "a quarantine admitted between a finish's quarantine gate and its publication leaves no flagged block on the free list: the finish takes it back OWED and the cohort's release publishes it (0003 §Y.3)" {
    const a = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = 32 * 4096 });
    defer a.deinit();
    var i: u64 = 0;
    while (i < 4) : (i += 1) _ = try a.allocateBlock();
    const Admit = struct {
        a: *BlockAllocator,
        epoch: u64,
        fired: bool = false,
        fn passed(ctx: *anyopaque, idx: u64) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.fired = true;
            // The block reads `limbo` here: the admission is the flag alone.
            _ = self.a.quarantineBlock(idx, self.epoch);
        }
    };
    var adm: Admit = .{ .a = a, .epoch = 9 };
    a.test_finish_station = .{ .ctx = &adm, .passedGate = &Admit.passed };
    const free0 = a.freeBlocksCount();
    try testing.expectEqual(BeginFree.terminal, try a.freeBlock(2));
    a.test_finish_station = null;
    try testing.expect(adm.fired and a.isQuarantined(2));
    // The law: a quarantined block is never reallocatable.
    try testing.expect(a.bitIsSet(2));
    try testing.expect(a.stateOf(2) != .free);
    try testing.expectEqual(free0, a.freeBlocksCount());
    // The cohort's release (a live-owner admission: the caller names it) publishes it.
    var out: [4]u64 = undefined;
    const n = a.releaseQuarantine(9, &.{2}, &out);
    try testing.expectEqual(@as(usize, 1), n);
    for (out[0..n]) |idx| a.finishFree(idx);
    try testing.expectEqual(State.free, a.stateOf(2));
    try testing.expect(!a.bitIsSet(2) and !a.isQuarantined(2));
    try testing.expectEqual(free0 + 1, a.freeBlocksCount());
    try testing.expectEqual(@as(u64, 0), a.quarantine_count.load(.acquire));
}

test "stats JSON names every family" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    try testing.expect(std.mem.indexOf(u8, s, "\"alloc_fresh_mints\":") != null);
    try testing.expect(std.mem.indexOf(u8, s, "\"block_double_frees\":") != null);
    try testing.expect(std.mem.endsWith(u8, s, "]}"));
}
