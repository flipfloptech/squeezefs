//! Custody words and the custody revoke pair (charter §5.5 "A dead holder's
//! slot's custody grants are QUARANTINED for `T_owner + 2 × skew_max`",
//! §6.2 item 2 "a foreign-custody file declines; a non-holder joiner
//! declines", §9 "the custody revoke pair"; `docs/porting/sym_data_planes.md`
//! §6). PORTABLE; the two Dekker pairs are MODELS under
//! `model/explorer.zig` with their weakenings proven to fail.
//!
//! The custody epoch is ONE authorization point: `term_base | generation`
//! — the durable writer term in the high bits (a successor bumps it
//! durably before arming, so every token and every DMA authorization minted
//! in an earlier era is stale by construction) and the per-process custody
//! generation in the low bits (0 on every mount that holds no remote write
//! custody). `authorizeDma(carried)` is THE gate every data-plane write
//! passes: two relaxed loads on a solo mount. A revoked grant costs the
//! work authorized under it, never the process; saturation is a POISON,
//! not a wrap (a carry into the term field would forge a newer era); under
//! the symmetric plane the poison SPLITS — park for slot leases and the
//! appender region, poison only for non-reclaimable custody.
//!
//! "No release without a proof" is a TYPE: there is no `DrainProof.assumed`
//! and `preemptLanded` refuses to exist for a preempt that landed on zero
//! namespaces. ONE dead epoch is minted per CLIENT (a client holding three
//! grants has ONE custody), from a monotone sequence never reused.

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const explorer = @import("../../model/explorer.zig");
const jc = @import("../../meta/journal_core.zig");
const block_alloc = @import("../block_alloc.zig");
const sole_owner_patch = @import("../sole_owner_patch.zig");
const records = @import("records.zig");

pub const BlockAllocator = block_alloc.BlockAllocator;

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// The custody generation's width: 2^32 revocations per term before the
/// POISON (a mount revoking a grant every millisecond saturates in 49
/// days — and a saturated word is a fault, never a wrap).
pub const GRANT_SEQ_BITS: u6 = 32;
pub const GRANT_SEQ_MAX: u64 = (1 << GRANT_SEQ_BITS) - 1;
/// The owner's eviction TTL and the recorder-clock disagreement the
/// durable quarantine absorbs (0002 row 188; `DG:L6187-6216`).
pub const T_OWNER_MS: u64 = 45_000;

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var dlm_custody_epoch_advances: counters.ShardedCounter = .{};
    pub var data_dma_fence_refusals: counters.ShardedCounter = .{};
    pub var data_dma_epoch_refusals: counters.ShardedCounter = .{};
    pub var custody_proofs: counters.ShardedCounter = .{};
    pub var custody_quarantined: counters.ShardedCounter = .{};
    pub var slot_custody_quarantine_refusals: counters.ShardedCounter = .{};
    pub var accounting_plane_refusals: counters.ShardedCounter = .{};
    pub var custody_revokes: counters.ShardedCounter = .{};
    pub var custody_handler_reacquires: counters.ShardedCounter = .{};
    /// Must-stay-0: the custody generation saturated (poison).
    pub var custody_generation_poisoned: counters.Tripwire = .init("custody_generation_poisoned");
    /// Levels.
    pub var slot_custody_quarantined: counters.Gauge = .{};
    pub var dead_epoch_current: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// §6.1 The custody epoch — ONE authorization point.
// ---------------------------------------------------------------------------

pub const CustodyEpoch = u64;

pub fn termBase(term: u64) CustodyEpoch {
    return term << GRANT_SEQ_BITS;
}

pub fn composeEpoch(term: u64, generation: u64) CustodyEpoch {
    assert(generation <= GRANT_SEQ_MAX);
    return termBase(term) | generation;
}

pub fn epochTerm(e: CustodyEpoch) u64 {
    return e >> GRANT_SEQ_BITS;
}

pub fn epochGeneration(e: CustodyEpoch) u64 {
    return e & GRANT_SEQ_MAX;
}

pub const AuthorizeVerdict = enum {
    ok,
    /// The word is poisoned: `WriterGuardFenced`.
    fenced,
    /// `carried ≠ current`: the offsets it authorized may already be
    /// reallocated.
    stale,
};

/// Which fence class a poison event belongs to (the split).
pub const FenceClass = enum { slot_lease, appender_region, custody };

pub const PoisonAction = enum { park, poison };

/// Slot leases and the appender region PARK-AND-RECLAIM (charter §5.5);
/// only non-reclaimable custody poisons.
pub fn poisonSplitsByFenceClass(class: FenceClass) PoisonAction {
    return switch (class) {
        .slot_lease, .appender_region => .park,
        .custody => .poison,
    };
}

pub const CustodyWord = struct {
    /// The durable writer term (bumped before arming).
    term: Atomic(u64) = .init(0),
    generation: Atomic(u64) = .init(0),
    poisoned: Atomic(bool) = .init(false),

    pub fn current(self: *const CustodyWord) CustodyEpoch {
        return composeEpoch(self.term.load(.acquire), @min(self.generation.load(.acquire), GRANT_SEQ_MAX));
    }

    pub fn setTerm(self: *CustodyWord, term: u64) void {
        self.term.store(term, .release);
    }

    /// THE gate: poisoned ⇒ fenced; `carried ≠ current` ⇒ stale; a solo
    /// mount (null carried) passes on two relaxed loads.
    pub fn authorizeDma(self: *const CustodyWord, carried: ?CustodyEpoch) AuthorizeVerdict {
        if (self.poisoned.load(.monotonic)) {
            stats.data_dma_fence_refusals.inc();
            return .fenced;
        }
        const c = carried orelse return .ok;
        if (c != self.current()) {
            stats.data_dma_epoch_refusals.inc();
            return .stale;
        }
        return .ok;
    }

    /// A revoked grant costs the work authorized under it, never the
    /// process; saturation POISONS (never a carry into the term field).
    pub fn advanceCustodyGeneration(self: *CustodyWord) void {
        const prev = self.generation.fetchAdd(1, .acq_rel);
        stats.dlm_custody_epoch_advances.inc();
        if (prev >= GRANT_SEQ_MAX) {
            self.generation.store(GRANT_SEQ_MAX, .release);
            _ = self.poison(.custody);
        }
    }

    /// Monotone adoption of a generation a peer named.
    pub fn adoptCustodyGeneration(self: *CustodyWord, generation: u64) void {
        _ = self.generation.fetchMax(@min(generation, GRANT_SEQ_MAX), .acq_rel);
    }

    /// Sticky; the split decides whether this class poisons at all.
    pub fn poison(self: *CustodyWord, class: FenceClass) PoisonAction {
        const action = poisonSplitsByFenceClass(class);
        if (action == .poison) {
            if (!self.poisoned.swap(true, .acq_rel)) {
                stats.custody_generation_poisoned.fire("the custody word was poisoned");
                log.err("custody: the custody word is POISONED (class {s}) — every DMA authorization is refused WriterGuardFenced from here; the park classes reclaim instead", .{@tagName(class)});
            }
        }
        return action;
    }

    pub fn isPoisoned(self: *const CustodyWord) bool {
        return self.poisoned.load(.acquire);
    }
};

// ---------------------------------------------------------------------------
// §6.2–§6.3 The dead epoch, the drain proof, the block quarantine.
// ---------------------------------------------------------------------------

pub const DeadEpoch = u64;

/// Monotone, never reused; 0 is never minted (the allocator's stamp word
/// reads 0 as "no lifetime").
pub const DeadEpochSequence = struct {
    next: Atomic(u64) = .init(1),

    pub fn mint(self: *DeadEpochSequence) DeadEpoch {
        const e = self.next.fetchAdd(1, .acq_rel);
        stats.dead_epoch_current.set(e);
        return e;
    }
};

/// "No release without a proof" as a type.
pub const DrainProof = union(enum) {
    /// The PR preempt landed on ≥ 1 namespaces.
    preempt_landed: struct { namespaces: u32 },
    /// The home shard proved the member dead (its eviction).
    proven_dead: struct { epoch: u64 },

    /// Refuses to exist for a preempt that landed on zero namespaces.
    pub fn preemptLanded(namespaces: u32) ?DrainProof {
        if (namespaces == 0) return null;
        stats.custody_proofs.inc();
        return .{ .preempt_landed = .{ .namespaces = namespaces } };
    }

    pub fn provenDead(epoch: u64) DrainProof {
        stats.custody_proofs.inc();
        return .{ .proven_dead = .{ .epoch = epoch } };
    }
};

/// The most live-owner admissions one cohort carries: a client's declared
/// in-flight destinations ride its renewal frame, bounded by the write
/// pipeline's BDP window (64 blocks) per data volume.
pub const LIVE_ADMISSIONS_MAX: usize = 256;

/// The allocator's quarantine faces over ONE allocator, plus the cohort
/// bookkeeping the allocator cannot keep (a LIVE owner's block carries the
/// flag alone — the epoch is here).
pub const BlockQuarantine = struct {
    alloc: *BlockAllocator,
    lock: jc.SpinLock = .{},
    live: [LIVE_ADMISSIONS_MAX]struct { epoch: DeadEpoch, idx: u64 } = undefined,
    live_n: usize = 0,

    /// Idempotent admission; `false` ⇒ already in a cohort.
    pub fn admit(self: *BlockQuarantine, idx: u64, epoch: DeadEpoch) bool {
        assert(epoch != 0);
        const was_live = self.alloc.stateOf(idx) == .allocated or self.alloc.stateOf(idx) == .limbo or self.alloc.stateOf(idx) == .trim_windowed;
        if (!self.alloc.quarantineBlock(idx, epoch)) return false;
        stats.custody_quarantined.inc();
        if (was_live) {
            self.lock.lock();
            defer self.lock.unlock();
            if (self.live_n < LIVE_ADMISSIONS_MAX) {
                self.live[self.live_n] = .{ .epoch = epoch, .idx = idx };
                self.live_n += 1;
            } else {
                // Past the cap the block stays flagged (its free defers);
                // fsck's reconciliation completes it — never a lost bit.
                log.warn("block quarantine: the live-admission table is full; block {d} of epoch {d} stays flagged until fsck reconciles it", .{ idx, epoch });
            }
        }
        return true;
    }

    pub fn contains(self: *BlockQuarantine, idx: u64) bool {
        return self.alloc.isQuarantined(idx);
    }

    /// Release the cohort under a PROOF: the owed publishes go to `out`
    /// (the caller runs `finishFree` on each — grace ring, then the sink).
    pub fn release(self: *BlockQuarantine, epoch: DeadEpoch, proof: DrainProof, out: []u64) usize {
        _ = proof;
        var live_idx: [LIVE_ADMISSIONS_MAX]u64 = undefined;
        var n: usize = 0;
        self.lock.lock();
        var i: usize = 0;
        while (i < self.live_n) {
            if (self.live[i].epoch == epoch) {
                live_idx[n] = self.live[i].idx;
                n += 1;
                self.live[i] = self.live[self.live_n - 1];
                self.live_n -= 1;
            } else i += 1;
        }
        self.lock.unlock();
        return self.alloc.releaseQuarantine(epoch, live_idx[0..n], out);
    }
};

// ---------------------------------------------------------------------------
// §6.4 Pair 1 — revoke / enter (the process plane's core).
// ---------------------------------------------------------------------------

/// A handler `enter()` = `uses += 1` (SeqCst RMW), then `isCached()` (a
/// SeqCst load) at the ONE cached-lease accessor; the recall `revoke()` =
/// `cached = 0` (SeqCst RMW), then `settleReady()` = `uses == 0` (SeqCst
/// load). Either the handler reads the lease revoked and re-acquires, or
/// the settle counts it in flight and the release waits — never "a write
/// under a released grant". Zig 0.17 has no standalone fence: the fences
/// are SeqCst RMWs on the words themselves (the W1 pair's ruling).
pub const CustodyUseCore = struct {
    uses: Atomic(u32) = .init(0),
    cached: Atomic(u8) = .init(1),

    pub fn enter(self: *CustodyUseCore) void {
        _ = self.uses.fetchAdd(1, .seq_cst);
    }

    pub fn leave(self: *CustodyUseCore) void {
        _ = self.uses.fetchSub(1, .seq_cst);
    }

    /// The ONE cached-lease accessor; `false` ⇒ re-acquire at the holder.
    pub fn isCached(self: *const CustodyUseCore) bool {
        const c = self.cached.load(.seq_cst) != 0;
        if (!c) stats.custody_handler_reacquires.inc();
        return c;
    }

    pub fn revoke(self: *CustodyUseCore) void {
        _ = self.cached.swap(0, .seq_cst);
        stats.custody_revokes.inc();
    }

    pub fn settleReady(self: *const CustodyUseCore) bool {
        return self.uses.load(.seq_cst) == 0;
    }

    pub fn restore(self: *CustodyUseCore) void {
        self.cached.store(1, .seq_cst);
    }
};

/// The MODEL of pair 1 under TSO: each side's store sits in a per-thread
/// buffer until its fence (or a later step) drains it. `handler_fence` /
/// `revoke_fence` false = the weakening.
pub fn CustodyUseModel(comptime handler_fence: bool, comptime revoke_fence: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const H = 0;
        const R = 1;

        uses: u8 = 0,
        cached: u8 = 1,
        /// Buffered stores (TSO): the handler's `uses = 1`, the recall's
        /// `cached = 0`.
        uses_pending: bool = false,
        cached_pending: bool = false,
        served_under_cached: bool = false,
        released: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }

        fn drainH(self: *Self) void {
            if (self.uses_pending) {
                self.uses = 1;
                self.uses_pending = false;
            }
        }

        fn drainR(self: *Self) void {
            if (self.cached_pending) {
                self.cached = 0;
                self.cached_pending = false;
            }
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                H => switch (self.pc[H]) {
                    0 => {
                        self.uses_pending = true; // uses += 1 (buffered)
                        self.pc[H] = 1;
                    },
                    1 => {
                        if (handler_fence) self.drainH();
                        self.pc[H] = 2;
                    },
                    2 => {
                        if (self.cached == 1) self.served_under_cached = true;
                        self.pc[H] = 3;
                    },
                    3 => {
                        self.drainH();
                        self.pc[H] = 4;
                    },
                    else => unreachable,
                },
                R => switch (self.pc[R]) {
                    0 => {
                        self.cached_pending = true; // cached = 0 (buffered)
                        self.pc[R] = 1;
                    },
                    1 => {
                        if (revoke_fence) self.drainR();
                        self.pc[R] = 2;
                    },
                    2 => {
                        if (self.uses == 0) self.released = true;
                        self.pc[R] = 3;
                    },
                    3 => {
                        self.drainR();
                        self.pc[R] = 4;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        /// Never "a write under a released grant".
        pub fn invariant(self: *const Self) !void {
            if (self.served_under_cached and self.released) return error.WriteUnderReleasedGrant;
        }

        pub fn final(_: *const Self) !void {}
    };
}

// ---------------------------------------------------------------------------
// §6.4 Pair 2 — the handover's mark / the grant's re-check.
// ---------------------------------------------------------------------------

/// The handover ARMS its slot's mark under the mark table's lock (the
/// fast-path count stored) BEFORE its census under the grant table's lock;
/// a grant INSERTS under the grant table's lock BEFORE it re-reads the mark
/// (the relaxed count first, the table under its lock only when non-zero).
/// Both sides take both locks, one per step, so the locks' acquire/release
/// chains order the steps. `clearIf(seq)` clears only the mark it armed
/// (stamps are monotone).
pub const HandoverMarkCore = struct {
    mark_lock: jc.SpinLock = .{},
    grant_lock: jc.SpinLock = .{},
    /// The relaxed fast-path count (inside the happens-before chain).
    marks: Atomic(u32) = .init(0),
    mark_seq: u64 = 0,
    mark_armed: bool = false,
    grants: u32 = 0,

    /// Returns the seq stamp of this arm.
    pub fn arm(self: *HandoverMarkCore) u64 {
        self.mark_lock.lock();
        defer self.mark_lock.unlock();
        self.mark_seq += 1;
        self.mark_armed = true;
        self.marks.store(1, .monotonic);
        return self.mark_seq;
    }

    pub fn census(self: *HandoverMarkCore) u32 {
        self.grant_lock.lock();
        defer self.grant_lock.unlock();
        return self.grants;
    }

    pub fn insertGrant(self: *HandoverMarkCore) void {
        self.grant_lock.lock();
        defer self.grant_lock.unlock();
        self.grants += 1;
    }

    /// `true` ⇒ a handover is in flight: the grant defers to the next holder.
    pub fn defers(self: *HandoverMarkCore) bool {
        if (self.marks.load(.monotonic) == 0) return false;
        self.mark_lock.lock();
        defer self.mark_lock.unlock();
        return self.mark_armed;
    }

    /// Clears only the mark it armed: an older guard's drop never clears a
    /// newer held mark.
    pub fn clearIf(self: *HandoverMarkCore, seq: u64) bool {
        self.mark_lock.lock();
        defer self.mark_lock.unlock();
        if (!self.mark_armed or self.mark_seq != seq) return false;
        self.mark_armed = false;
        self.marks.store(0, .monotonic);
        return true;
    }
};

/// The MODEL of pair 2 (sequential consistency: the locks' chains are what
/// order the steps). `swapped` = the grant's two steps reversed.
pub fn HandoverMarkModel(comptime swapped: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const HO = 0;
        const G = 1;

        mark: u8 = 0,
        grant: u8 = 0,
        census_saw_grant: bool = false,
        grant_defers: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 2;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                HO => switch (self.pc[HO]) {
                    0 => {
                        self.mark = 1; // arm under the mark lock
                        self.pc[HO] = 1;
                    },
                    1 => {
                        self.census_saw_grant = self.grant == 1; // census under the grant lock
                        self.pc[HO] = 2;
                    },
                    else => unreachable,
                },
                G => switch (self.pc[G]) {
                    0 => {
                        if (swapped) self.grant_defers = self.mark == 1 else self.grant = 1;
                        self.pc[G] = 1;
                    },
                    1 => {
                        if (swapped) self.grant = 1 else self.grant_defers = self.mark == 1;
                        self.pc[G] = 2;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }

        pub fn invariant(_: *const Self) !void {}

        /// A grant is either censused or deferred — never spanning the move.
        pub fn final(self: *const Self) !void {
            if (!self.census_saw_grant and !self.grant_defers) return error.GrantSpansTheMove;
        }
    };
}

// ---------------------------------------------------------------------------
// §6.5 The W1 sole-owner posture verdict.
// ---------------------------------------------------------------------------

/// The kind-6 probe's fold (`refcount(block)` = ONE range probe; `shared`
/// = OR of the records' SHARED bits).
pub const RefProbe = struct { count: u32, shared: bool };

/// Decided BEFORE any probe or the incarnation retire — ONE function so the
/// write path's pre-ack probe and W1 cannot drift: unarmed ⇒ sole (the
/// RAM pair alone); not holding the ownership plane ⇒ `NonHolder` (a
/// JOINED appender: the patch retires a lifetime that plane accounts; the
/// CoW path + the shipped free); the file's custody held from its SLOT
/// HOLDER ⇒ `ForeignCustody` (the block's references live in the holder's
/// tree, which this mount cannot read); then the durable clause: `count ==
/// 1 ∧ !shared` ⇒ sole, anything else — or a probe that cannot read, the
/// leak-safe direction — ⇒ shared.
pub fn soleOwnerVerdict(armed: bool, holds_plane: bool, foreign_custody: bool, probe: ?RefProbe) sole_owner_patch.SoleOwnerVerdict {
    if (!armed) return .sole;
    if (!holds_plane) return .non_holder;
    if (foreign_custody) return .foreign_custody;
    const p = probe orelse return .shared;
    return if (p.count == 1 and !p.shared) .sole else .shared;
}

/// The seams the posture verdict reads on an armed set.
pub const PostureSource = struct {
    ctx: *anyopaque,
    vtable: *const VTable,
    /// The data volumes' block size (the mapping's offset → block index).
    block_size: u64,

    pub const VTable = struct {
        /// `PlaneGate.holds(.patch_retire)` read WITHOUT the gate's ERROR
        /// line (the silent accessor).
        holdsPlane: *const fn (*anyopaque) bool,
        /// The slot-custody table has a home for the ino.
        foreignCustody: *const fn (*anyopaque, ino: u64) bool,
        /// ONE range probe of the patching ino's slot tree; null = cannot read.
        probe: *const fn (*anyopaque, ino: u64, vol_tag: u64, block_idx: u64) ?RefProbe,
    };

    fn soleOwner(ctx: ?*anyopaque, ino: u64, binding: sole_owner_patch.Binding) sole_owner_patch.SoleOwnerVerdict {
        const self: *const PostureSource = @ptrCast(@alignCast(ctx.?));
        const holds = self.vtable.holdsPlane(self.ctx);
        const foreign = self.vtable.foreignCustody(self.ctx, ino);
        // The durable probe is asked only once the postures passed (a
        // non-holder never reads a slot tree it cannot read).
        const probe = if (holds and !foreign) self.vtable.probe(self.ctx, ino, binding.vol_tag, binding.blockIdx(self.block_size)) else null;
        return soleOwnerVerdict(true, holds, foreign, probe);
    }

    /// The `CustodyProbe.sole_owner` seam `sole_owner_patch` consults.
    pub fn soleOwnerFn() *const fn (?*anyopaque, u64, sole_owner_patch.Binding) sole_owner_patch.SoleOwnerVerdict {
        return &soleOwner;
    }
};

// ---------------------------------------------------------------------------
// The allocation-plane gate (0002 row 244: ONE reader of one word).
// ---------------------------------------------------------------------------

pub const PlaneGateCore = struct {
    /// Does this mount hold the volume's allocation lease (or run under
    /// the holder's accounting authority)?
    held: Atomic(bool) = .init(false),
    last_log_s: Atomic(u64) = .init(0),

    pub fn setHeld(self: *PlaneGateCore, held: bool) void {
        self.held.store(held, .release);
    }

    /// The silent accessor (the posture verdict).
    pub fn holdsSilently(self: *const PlaneGateCore) bool {
        return self.held.load(.acquire);
    }

    /// The loud accessor (the mutation sites): a refusal is counted and
    /// logged once per second.
    pub fn holdsLoud(self: *PlaneGateCore, site: block_alloc.Site) bool {
        if (self.held.load(.acquire)) return true;
        stats.accounting_plane_refusals.inc();
        const now = @import("../../core/clock.zig").realtimeCoarseS();
        if (self.last_log_s.swap(now, .monotonic) != now) {
            log.err("allocation plane: ownership mutation ({s}) refused — this mount does not hold the volume's allocation lease (retryable; allocate through block grants from the holder)", .{@tagName(site)});
        }
        return false;
    }

    fn holdsFn(ctx: ?*anyopaque, site: block_alloc.Site) bool {
        const self: *PlaneGateCore = @ptrCast(@alignCast(ctx.?));
        return self.holdsLoud(site);
    }

    pub fn gate(self: *PlaneGateCore) block_alloc.PlaneGate {
        return .{ .ctx = self, .holds = &holdsFn };
    }
};

// ---------------------------------------------------------------------------
// §6.6 The durable slot custody quarantine.
// ---------------------------------------------------------------------------

/// `until = ts + T_owner + 2 × skew_max` (CLOCK_REALTIME ns — a deadline
/// compared on another host): `T_owner` is the OWNER's view past which a
/// holder may re-grant; `2 × skew_max` absorbs the recorder's clock.
pub fn custodyQuarantineUntilNs(recorded_realtime_ns: u64, skew_max_ns: u64) u64 {
    return recorded_realtime_ns + T_OWNER_MS * std.time.ns_per_ms + 2 * skew_max_ns;
}

/// The grant paths' question behind one relaxed load: remaining ns, or
/// null once expired (the record is then deleted).
pub fn custodyQuarantineRemainingNs(now_realtime_ns: u64, until_realtime_ns: u64) ?u64 {
    if (now_realtime_ns >= until_realtime_ns) return null;
    stats.slot_custody_quarantine_refusals.inc();
    return until_realtime_ns - now_realtime_ns;
}

/// The durable `custody_quarantine:{slot}` record (the control plane's codec).
pub fn encodeCustodyQuarantine(until_realtime_ns: u64) [records.control.CustodyQuarantineWire.LEN]u8 {
    return records.control.CustodyQuarantineWire.init(until_realtime_ns).encode();
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the custody epoch: term in the high bits, generation low; authorize_dma passes a solo mount on relaxed loads, refuses a stale carried epoch and a poisoned word; the advance saturates into a POISON, never a carry; adoption is monotone; the split" {
    var w: CustodyWord = .{};
    w.setTerm(3);
    try testing.expectEqual(composeEpoch(3, 0), w.current());
    try testing.expectEqual(@as(u64, 3), epochTerm(w.current()));
    try testing.expectEqual(AuthorizeVerdict.ok, w.authorizeDma(null));
    try testing.expectEqual(AuthorizeVerdict.ok, w.authorizeDma(w.current()));
    const stale0 = stats.data_dma_epoch_refusals.load();
    w.advanceCustodyGeneration();
    try testing.expectEqual(@as(u64, 1), epochGeneration(w.current()));
    try testing.expectEqual(AuthorizeVerdict.stale, w.authorizeDma(composeEpoch(3, 0)));
    try testing.expectEqual(stale0 + 1, stats.data_dma_epoch_refusals.load());
    w.adoptCustodyGeneration(7);
    w.adoptCustodyGeneration(5);
    try testing.expectEqual(@as(u64, 7), epochGeneration(w.current()));
    // The park classes never poison; custody does, sticky.
    try testing.expectEqual(PoisonAction.park, poisonSplitsByFenceClass(.slot_lease));
    try testing.expectEqual(PoisonAction.park, poisonSplitsByFenceClass(.appender_region));
    try testing.expectEqual(PoisonAction.park, w.poison(.appender_region));
    try testing.expect(!w.isPoisoned());
    // Saturation: one short of the max, the next advance poisons.
    w.generation.store(GRANT_SEQ_MAX, .release);
    const trip0 = counters.tripwireCount("custody_generation_poisoned");
    stats.custody_generation_poisoned.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    w.advanceCustodyGeneration();
    try log.testing.expectAllFired();
    try testing.expect(w.isPoisoned());
    try testing.expectEqual(GRANT_SEQ_MAX, epochGeneration(w.current()));
    try testing.expectEqual(@as(u64, 3), epochTerm(w.current())); // no carry into the term
    try testing.expectEqual(trip0 + 1, counters.tripwireCount("custody_generation_poisoned"));
    const fenced0 = stats.data_dma_fence_refusals.load();
    try testing.expectEqual(AuthorizeVerdict.fenced, w.authorizeDma(null));
    try testing.expectEqual(fenced0 + 1, stats.data_dma_fence_refusals.load());
    try testing.expectEqual(PoisonAction.poison, w.poison(.custody)); // sticky, silent the second time
}

test "DrainProof is a type: a preempt that landed on zero namespaces refuses to exist; the dead-epoch sequence is monotone and never 0" {
    try testing.expect(DrainProof.preemptLanded(0) == null);
    const p = DrainProof.preemptLanded(2).?;
    try testing.expectEqual(@as(u32, 2), p.preempt_landed.namespaces);
    const d = DrainProof.provenDead(9);
    try testing.expectEqual(@as(u64, 9), d.proven_dead.epoch);
    var seq: DeadEpochSequence = .{};
    const e1 = seq.mint();
    const e2 = seq.mint();
    try testing.expect(e1 != 0 and e2 == e1 + 1);
}

test "the block quarantine keeps the live cohort's epoch the allocator cannot: a live owner's block admitted under epoch 5 defers its free and is released with 5's proof, not 6's" {
    const a = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = 32 * 4096 });
    defer a.deinit();
    var i: u64 = 0;
    while (i < 4) : (i += 1) _ = try a.allocateBlock();
    var q: BlockQuarantine = .{ .alloc = a };
    try testing.expect(q.admit(2, 5)); // live
    try testing.expect(!q.admit(2, 5));
    try testing.expect(q.admit(20, 5)); // never minted: the epoch rides the stamp word
    try testing.expect(q.admit(3, 6)); // another cohort
    try testing.expect(q.contains(2) and q.contains(20) and q.contains(3));
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(2));
    try testing.expectEqual(block_alloc.State.limbo, a.stateOf(2)); // deferred
    var out: [8]u64 = undefined;
    const n6 = q.release(6, DrainProof.provenDead(1), &out);
    try testing.expectEqual(@as(usize, 0), n6); // 3 is live and owes nothing yet
    try testing.expect(!q.contains(3) and q.contains(2));
    const n5 = q.release(5, DrainProof.preemptLanded(1).?, &out);
    try testing.expectEqual(@as(usize, 2), n5);
    std.mem.sort(u64, out[0..n5], {}, std.sort.asc(u64));
    try testing.expectEqualSlices(u64, &.{ 2, 20 }, out[0..n5]);
    for (out[0..n5]) |idx| a.finishFree(idx);
    try testing.expectEqual(block_alloc.State.free, a.stateOf(2));
    try testing.expectEqual(block_alloc.State.virgin, a.stateOf(20));
    try testing.expectEqual(@as(usize, 0), q.live_n);
}

test "a_handler_that_serves_the_cached_lease_is_counted_before_the_release_departs / two_handlers_against_one_recall: pair 1 holds with both fences; EITHER fence removed fails (the recorded weakening — the pair needs BOTH)" {
    try explorer.expectHolds(CustodyUseModel(true, true), .{});
    try explorer.expectFails(CustodyUseModel(false, true), .{}, error.InvariantViolated);
    try explorer.expectFails(CustodyUseModel(true, false), .{}, error.InvariantViolated);
    try explorer.expectFails(CustodyUseModel(false, false), .{}, error.InvariantViolated);
    // The process core: a handler counted before the recall's settle holds the release.
    var c: CustodyUseCore = .{};
    c.enter();
    try testing.expect(c.isCached());
    c.revoke();
    try testing.expect(!c.settleReady()); // the handler is in flight
    c.leave();
    try testing.expect(c.settleReady());
    try testing.expect(!c.isCached()); // a later handler re-acquires
    c.restore();
    try testing.expect(c.isCached());
}

test "a_grant_racing_the_handover_is_censused_or_deferred: pair 2 holds in the locks' order and fails with the grant's steps SWAPPED; an_older_guards_drop_never_clears_a_newer_held_mark" {
    try explorer.expectHolds(HandoverMarkModel(false), .{});
    try explorer.expectFails(HandoverMarkModel(true), .{}, error.FinalViolated);
    var core: HandoverMarkCore = .{};
    try testing.expect(!core.defers()); // the relaxed fast path: no mark, no lock
    const s1 = core.arm();
    core.insertGrant();
    try testing.expect(core.defers());
    try testing.expectEqual(@as(u32, 1), core.census());
    const s2 = core.arm(); // a newer handover re-arms
    try testing.expect(!core.clearIf(s1)); // the older guard's drop clears nothing
    try testing.expect(core.defers());
    try testing.expect(core.clearIf(s2));
    try testing.expect(!core.defers());
    try testing.expect(!core.clearIf(s2));
}

test "w1_declines_a_foreign_custody_file_and_keeps_the_durable_probe_for_own_files; the_w1_ladders_decline_a_non_holders_patch_as_a_counted_posture_decision: the verdict order NonHolder → ForeignCustody → the durable probe; unreadable is shared (leak-safe); unarmed is the RAM pair alone" {
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.sole, soleOwnerVerdict(false, false, true, null));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.non_holder, soleOwnerVerdict(true, false, true, .{ .count = 1, .shared = false }));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.foreign_custody, soleOwnerVerdict(true, true, true, .{ .count = 1, .shared = false }));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.sole, soleOwnerVerdict(true, true, false, .{ .count = 1, .shared = false }));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.shared, soleOwnerVerdict(true, true, false, .{ .count = 2, .shared = false }));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.shared, soleOwnerVerdict(true, true, false, .{ .count = 1, .shared = true }));
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.shared, soleOwnerVerdict(true, true, false, null));
    // The seam over a posture source: the probe is asked only once the
    // postures passed.
    const Src = struct {
        holds: bool = true,
        foreign: bool = false,
        probes: u32 = 0,
        answer: ?RefProbe = .{ .count = 1, .shared = false },
        fn holdsPlane(ctx: *anyopaque) bool {
            const s: *@This() = @ptrCast(@alignCast(ctx));
            return s.holds;
        }
        fn foreignCustody(ctx: *anyopaque, _: u64) bool {
            const s: *@This() = @ptrCast(@alignCast(ctx));
            return s.foreign;
        }
        fn probe(ctx: *anyopaque, _: u64, _: u64, _: u64) ?RefProbe {
            const s: *@This() = @ptrCast(@alignCast(ctx));
            s.probes += 1;
            return s.answer;
        }
        const vtable: PostureSource.VTable = .{ .holdsPlane = &holdsPlane, .foreignCustody = &foreignCustody, .probe = &probe };
    };
    var src: Src = .{};
    var ps: PostureSource = .{ .ctx = &src, .vtable = &Src.vtable, .block_size = 4096 };
    const f = PostureSource.soleOwnerFn();
    const b = sole_owner_patch.Binding.wholeBlock(1, 8192, 0);
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.sole, f(&ps, 7, b));
    try testing.expectEqual(@as(u32, 1), src.probes);
    src.holds = false;
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.non_holder, f(&ps, 7, b));
    try testing.expectEqual(@as(u32, 1), src.probes); // no probe for a non-holder
    src.holds = true;
    src.foreign = true;
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.foreign_custody, f(&ps, 7, b));
    try testing.expectEqual(@as(u32, 1), src.probes);
    src.foreign = false;
    src.answer = .{ .count = 1, .shared = true };
    try testing.expectEqual(sole_owner_patch.SoleOwnerVerdict.shared, f(&ps, 7, b));
}

test "the plane gate: ONE word, a silent accessor for the posture and a loud one for the mutation sites; an allocator behind it refuses retryably" {
    var g: PlaneGateCore = .{};
    try testing.expect(!g.holdsSilently());
    const refusals0 = stats.accounting_plane_refusals.load();
    log.testing.expectErrorLogs(1);
    try testing.expect(!g.holdsLoud(.mint));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals0 + 1, stats.accounting_plane_refusals.load());
    const a = try BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = 4 * 4096 });
    defer a.deinit();
    a.plane_gate = g.gate();
    log.testing.expectErrorLogs(0); // the second refusal within the second is quiet
    try testing.expectError(error.PlaneRefused, a.allocateBlock());
    try testing.expect(!a.freshSupplyExhausted());
    g.setHeld(true);
    try testing.expect(g.holdsSilently());
    _ = try a.allocateBlock();
}

test "the durable custody quarantine bound is T_owner + 2 × skew_max past the record; the grant path reads remaining or expired; the record encodes" {
    const ms = std.time.ns_per_ms;
    try testing.expectEqual(@as(u64, (1000 + 45_000 + 2 * 500) * ms), custodyQuarantineUntilNs(1000 * ms, 500 * ms));
    const until = custodyQuarantineUntilNs(1000 * ms, 500 * ms);
    const refusals0 = stats.slot_custody_quarantine_refusals.load();
    try testing.expectEqual(@as(?u64, 46_000 * ms), custodyQuarantineRemainingNs(1000 * ms, until));
    try testing.expectEqual(refusals0 + 1, stats.slot_custody_quarantine_refusals.load());
    try testing.expectEqual(@as(?u64, null), custodyQuarantineRemainingNs(until, until));
    const rec = encodeCustodyQuarantine(until);
    try testing.expectEqual(until, try records.control.CustodyQuarantineWire.decode(&rec));
    try testing.expectEqual(@as(usize, 4), records.control.custodyQuarantineKey(7).len);
}
