//! Shipped frees (charter §5.5 "terminal frees of a non-holder SHIP to the
//! holder"; `docs/porting/sym_data_planes.md` §4.1, §4.3). PORTABLE.
//!
//! The split: a free's DURABLE effect (the kind-6 delete) already rode the
//! displacing layout publish, whole-tx; what ships is the ACCOUNTING
//! ladder whose durable home is the holder's ledger. The verb `FreeBlocks
//! {vol_tag, blocks, lease_epoch, request_id}` is RETRIED and safe alone:
//! the holder's dedup window keys `(lease_epoch, request_id)`, and a retry
//! NEVER re-keys under a fresh epoch — that closes the freed-then-
//! reallocated ABA window. The shipper tries `FREE_SHIP_ATTEMPTS` times;
//! a transport failure re-resolves the holder venue ONCE and re-keys
//! (fresh request id — the old id never reached anyone); a lease-epoch
//! change mid-retry ABANDONS (leak-safe: the offsets are durably
//! unreferenced and the holder's next derivation returns them).
//!
//! The holder's executor runs the whole ladder with the holder's
//! accounting authority (a marker NEVER ambiently active) and ONE ledger
//! read per batch; per block the seven-row verdict table of §4.1 — the
//! two `Refused` shields guard the lineages the wire cannot carry an
//! incarnation witness for (a stale duplicate of a dead lifetime; a
//! freed-and-REALLOCATED offset whose durable population re-grew to its
//! RAM count). The non-holder's reclaimer never sees a shipped block: the
//! discard/punch is the HOLDER's, whose device reclaimer is live only
//! there; the shipper only purges tiers and retires tracking.

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");
const block_alloc = @import("../block_alloc.zig");
const reclaim = @import("../reclaim.zig");

pub const BlockAllocator = block_alloc.BlockAllocator;

// ---------------------------------------------------------------------------
// Constants (every one with its WHY).
// ---------------------------------------------------------------------------

/// Each resend is absorbed exactly-once by the dedup window; past three
/// the ship is leak-safe (the offsets are durably unreferenced) and the
/// holder's next derivation returns them.
pub const FREE_SHIP_ATTEMPTS: u32 = 3;
/// One batch = the reclaim drain's quantum (`HARVEST_BATCH`).
pub const FREE_BATCH_MAX: usize = 64;
/// The dedup window: `FREE_SHIP_ATTEMPTS` resends of every in-flight batch
/// across the appender ceiling a volume's joiners can reach; 256 keys ×
/// (16 B key + 16 B verdicts) = 8 KiB, sized once.
pub const FREE_DEDUP_WINDOW: usize = 256;

// ---------------------------------------------------------------------------
// The verb.
// ---------------------------------------------------------------------------

pub const FreeVerdict = enum(u2) { freed, nonterminal, refused };

pub const FreeBlocksRequest = struct {
    vol_tag: u64,
    blocks: []const u64,
    lease_epoch: u64,
    request_id: u64,
};

pub const Verdicts = struct {
    buf: [FREE_BATCH_MAX]FreeVerdict = undefined,
    n: u8 = 0,

    pub fn slice(self: *const Verdicts) []const FreeVerdict {
        return self.buf[0..self.n];
    }
};

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    // Shipper side.
    pub var free_shipped_blocks: counters.ShardedCounter = .{};
    pub var free_ship_failures: counters.ShardedCounter = .{};
    pub var free_ship_retries: counters.ShardedCounter = .{};
    pub var free_ship_rekeys: counters.ShardedCounter = .{};
    pub var free_ship_abandoned: counters.ShardedCounter = .{};
    pub var free_ship_freed: counters.ShardedCounter = .{};
    pub var free_ship_nonterminal: counters.ShardedCounter = .{};
    pub var free_ship_refused: counters.ShardedCounter = .{};
    // Holder side.
    pub var free_served_blocks: counters.ShardedCounter = .{};
    pub var free_refused_blocks: counters.ShardedCounter = .{};
    pub var free_served_requests: counters.ShardedCounter = .{};
    pub var free_dedup_replays: counters.ShardedCounter = .{};
    pub var free_seeded_references: counters.ShardedCounter = .{};
    /// Shipped frees a successor answered from its derivation (0003
    /// §AQ.2): untracked, unreferenced, the bit clear as its arm left it —
    /// the free sat in the writer's batch or wire across the failover.
    pub var free_derivation_settled: counters.ShardedCounter = .{};
    /// Must-stay-0: a served layout publish whose frame TAKES a block this
    /// mount released (free-listed, graced or quarantined).
    pub var served_publish_free_block_refusals: counters.Tripwire = .init("served_publish_free_block_refusals");
    /// Holder venue moves a joined writer followed across a failover.
    pub var joined_holder_venue_moves: counters.ShardedCounter = .{};
};

// ---------------------------------------------------------------------------
// §4.1 The holder's executor.
// ---------------------------------------------------------------------------

/// The holder's accounting-authority marker: a thread-local the executor
/// sets for the duration of a batch, read by the reclaimer's `FreeTarget`
/// seam so the ladder it runs locally never ships back to itself.
threadlocal var authority_accounting_active: bool = false;

pub fn authorityAccountingActive() bool {
    return authority_accounting_active;
}

/// The durable ledger's refcount per block (ONE read per batch is the
/// caller's duty; the seam answers per block from that read).
pub const DurableRefcounts = struct {
    ctx: *anyopaque,
    refcount: *const fn (*anyopaque, vol_tag: u64, idx: u64) u32,
};

/// The reclaimer's terminal ladder, run with the holder's authority.
pub const FreeLadder = struct {
    ctx: *anyopaque,
    free: *const fn (*anyopaque, idx: u64) reclaim.FreeOutcome,
};

/// "Graced" for the untracked arm's unseeded ladder.
pub const GraceProbe = struct {
    ctx: ?*anyopaque,
    holds: *const fn (?*anyopaque, idx: u64) bool,
};

/// A successor's derivation (0003 §AQ.2): `settle` answers true iff the
/// arm's census, pages and fold left the block's bit clear and this term
/// never moved it — and marks it moved (the settle is the term's act).
/// Null on a holder that succeeded nobody — every block a writer can free
/// there was carved under its own term.
pub const DerivationProbe = struct {
    ctx: *anyopaque,
    settle: *const fn (*anyopaque, idx: u64) bool,
};

const DedupEntry = struct { lease_epoch: u64, request_id: u64, verdicts: Verdicts, used: bool = false };

pub const FreeExecutor = struct {
    alloc: *BlockAllocator,
    durable: DurableRefcounts,
    ladder: FreeLadder,
    grace: ?GraceProbe = null,
    derivation: ?DerivationProbe = null,
    lock: jc.SpinLock = .{},
    dedup: [FREE_DEDUP_WINDOW]DedupEntry = @splat(.{ .lease_epoch = 0, .request_id = 0, .verdicts = .{} }),
    dedup_next: usize = 0,

    fn lookupDedup(self: *FreeExecutor, lease_epoch: u64, request_id: u64) ?Verdicts {
        for (&self.dedup) |*e| if (e.used and e.lease_epoch == lease_epoch and e.request_id == request_id) return e.verdicts;
        return null;
    }

    fn rememberDedup(self: *FreeExecutor, lease_epoch: u64, request_id: u64, v: Verdicts) void {
        self.dedup[self.dedup_next] = .{ .lease_epoch = lease_epoch, .request_id = request_id, .verdicts = v, .used = true };
        self.dedup_next = (self.dedup_next + 1) % FREE_DEDUP_WINDOW;
    }

    /// Serve one batch: dedup by `(lease_epoch, request_id)` (a resend
    /// gets the same verdicts, nothing re-executed), then the seven-row
    /// table per block under the authority marker.
    pub fn execute(self: *FreeExecutor, req: FreeBlocksRequest) Verdicts {
        assert(req.blocks.len <= FREE_BATCH_MAX);
        self.lock.lock();
        defer self.lock.unlock();
        if (self.lookupDedup(req.lease_epoch, req.request_id)) |v| {
            stats.free_dedup_replays.inc();
            return v;
        }
        stats.free_served_requests.inc();
        authority_accounting_active = true;
        defer authority_accounting_active = false;
        var out: Verdicts = .{};
        for (req.blocks) |idx| {
            const v = self.verdictFor(req.vol_tag, idx);
            out.buf[out.n] = v;
            out.n += 1;
            switch (v) {
                .freed, .nonterminal => stats.free_served_blocks.inc(),
                .refused => stats.free_refused_blocks.inc(),
            }
        }
        self.rememberDedup(req.lease_epoch, req.request_id, out);
        return out;
    }

    /// The seven rows (§4.1): `tracked` = a RAM refcount at the holder;
    /// `population` = the DURABLE ledger's count after the shipper's
    /// publish deleted its record.
    fn verdictFor(self: *FreeExecutor, vol_tag: u64, idx: u64) FreeVerdict {
        if (idx >= self.alloc.capacityBlocks()) return .refused;
        const durable_pop = self.durable.refcount(self.durable.ctx, vol_tag, idx);
        if (self.alloc.refcount(idx)) |ram| {
            // Row 1: claimed / mid-write by its current owner — a stale
            // duplicate of a dead lifetime.
            if (!block_alloc.Incarnation.isStable(self.alloc.incarnationWord(idx)) and self.alloc.incarnationWord(idx) != 0) {
                block_alloc.stats.block_live_free_refusals.fire("a shipped free of a block mid-write by its current owner");
                return .refused;
            }
            // Row 2: a legitimate shipped free follows its publish, so the
            // durable population runs BELOW the RAM count; equality names
            // a freed-and-REALLOCATED offset.
            if (durable_pop >= ram) {
                block_alloc.stats.block_live_free_refusals.fire("a shipped free whose durable population re-grew to its RAM count (freed-and-reallocated)");
                return .refused;
            }
            // Row 3: the standard ladder; Freed iff the count was 1.
            return switch (self.ladder.free(self.ladder.ctx, idx)) {
                .terminal => .freed,
                .nonterminal => .nonterminal,
                .refused => .refused,
            };
        }
        // Untracked at the holder.
        if (durable_pop > 0) return .nonterminal; // the release already happened durably
        // Row 6: already free / bit CLEAR / graced / quarantined / in flight
        // — the double-release lineage … unless this successor's derivation
        // returned the block and its term never moved it (0003 §AQ.2): the
        // writer's free sat in its batch or wire across the failover
        // (consumed, unpublished, unanswered — no declaration names it), so
        // the derivation already made it nobody's. Freed: the writer's word
        // retires, nothing moves here.
        if (!formerLesseeBlock(self.alloc, self.grace, idx)) {
            if (self.derivation) |d| if (d.settle(d.ctx, idx)) {
                stats.free_derivation_settled.inc();
                return .freed;
            };
            block_alloc.stats.block_untracked_free_refusals.fire("a shipped free of an untracked, unreferenced block (the double-release lineage)");
            return .refused;
        }
        // Row 7: a granted, never-minted-here block nothing references — a
        // former lessee's mint: seed the reference and run the ladder
        // (deliberately NOT `recoverBlock`, whose gap-filling arm would
        // declare a live peer's unpublished tail free).
        self.alloc.fsckSetRefcount(idx, 1);
        stats.free_seeded_references.inc();
        return switch (self.ladder.free(self.ladder.ctx, idx)) {
            .terminal => .freed,
            .nonterminal => .nonterminal,
            .refused => .refused,
        };
    }

    /// The holder's `UntrackedFreeGate` for its LOCAL frees: the untracked
    /// arm of the same ladder — a block a FORMER lessee minted from its
    /// window, in a slot released or recovered to this mount, is
    /// adjudicated as ours (count seeded) instead of refused.
    pub fn untrackedGate(self: *FreeExecutor) block_alloc.UntrackedFreeGate {
        const G = struct {
            fn adjudicate(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
                const ex: *FreeExecutor = @ptrCast(@alignCast(ctx));
                if (ex.durable.refcount(ex.durable.ctx, vol_tag, idx) > 0) return false;
                if (!formerLesseeBlock(ex.alloc, ex.grace, idx)) return false;
                ex.alloc.fsckSetRefcount(idx, 1);
                stats.free_seeded_references.inc();
                return true;
            }
        };
        return .{ .ctx = self, .adjudicate = &G.adjudicate };
    }

    /// The reclaimer's `FreeTarget` on the HOLDER: nothing ships (the
    /// authority marker or the holding itself decides locally).
    pub fn localTarget(self: *FreeExecutor) reclaim.FreeTarget {
        const T = struct {
            fn ship(_: *anyopaque, _: u64, _: u64) bool {
                return false;
            }
            fn active(_: *anyopaque) bool {
                return true;
            }
        };
        return .{ .ctx = self, .ship = &T.ship, .authorityActive = &T.active };
    }
};

/// A block a FORMER lessee minted from its grant window, in a slot
/// released, handed over or recovered to this mount: its bit is SET (the
/// carve), nothing here ever minted it (`virgin`, or `allocated` with no
/// RAM count), it is not graced, quarantined or in flight. Everything else
/// unreferenced is the double-release lineage.
fn formerLesseeBlock(alloc: *BlockAllocator, grace: ?GraceProbe, idx: u64) bool {
    if (!alloc.bitIsSet(idx)) return false;
    const st = alloc.stateOf(idx);
    if (st != .virgin and st != .allocated) return false;
    if (alloc.isQuarantined(idx) or alloc.inflightContains(idx)) return false;
    if (grace) |g| if (g.holds(g.ctx, idx)) return false;
    return true;
}

/// The served-publish SCREEN at the holder: a served layout publish whose
/// frame TAKES a block this mount released (free-listed, graced or
/// quarantined) is refused before anything is staged.
pub fn servedPublishTakesReleasedBlock(alloc: *BlockAllocator, grace: ?GraceProbe, idx: u64) bool {
    const st = alloc.stateOf(idx);
    const graced = if (grace) |g| g.holds(g.ctx, idx) else false;
    // A clear bit (free-listed below the cursor, virgin above it), a limbo
    // between begin and finish, a quarantined or a graced block: nobody
    // may publish a frame that takes it.
    const released = !alloc.bitIsSet(idx) or st == .limbo or alloc.isQuarantined(idx) or graced;
    if (released) {
        stats.served_publish_free_block_refusals.fire("a served publish takes a block this mount released");
        log.err("served publish REFUSED: its frame takes block {d}, which this mount released (state {s}); nothing staged", .{ idx, @tagName(st) });
    }
    return released;
}

// ---------------------------------------------------------------------------
// §4.1 The shipper (the non-holder).
// ---------------------------------------------------------------------------

pub const ShipError = error{ Transport, NoTarget };

/// The holder's listener (the wire client, or the in-process loopback):
/// `send` answers the verdicts or a transport failure; `resolve` re-reads
/// the holder venue off durable state after a failure.
pub const HolderClient = struct {
    ctx: *anyopaque,
    send: *const fn (*anyopaque, FreeBlocksRequest) ShipError!Verdicts,
    resolve: *const fn (*anyopaque) bool,
};

/// The three tracking seams and the tier purge.
pub const TrackingSeams = struct {
    ctx: *anyopaque,
    purge: *const fn (*anyopaque, vol_tag: u64, idx: u64) void,
    /// `Freed`: the entry gone, the word retired.
    retire: *const fn (*anyopaque, vol_tag: u64, idx: u64) void,
    /// `NonTerminal`: this reference only, never the word.
    release: *const fn (*anyopaque, vol_tag: u64, idx: u64) void,
};

/// The current allocation-lease epoch of the volume (the holder's term as
/// this writer knows it); a change mid-retry abandons.
pub const LeaseEpochSource = struct {
    ctx: *anyopaque,
    epoch: *const fn (*anyopaque, vol_tag: u64) u64,
};

pub const Shipper = struct {
    vol_tag: u64,
    client: ?HolderClient = null,
    tracking: TrackingSeams,
    lease_epoch: LeaseEpochSource,
    next_request_id: Atomic(u64) = .init(1),
    /// The pending batch (the reclaimer's shipped blocks between flushes).
    lock: jc.SpinLock = .{},
    pending: [FREE_BATCH_MAX]u64 = undefined,
    pending_n: u8 = 0,

    /// The reclaimer's `FreeTarget` seam on a NON-holder: every terminal
    /// free of this volume joins the batch; a full batch flushes inline.
    pub fn freeTarget(self: *Shipper) reclaim.FreeTarget {
        const T = struct {
            fn ship(ctx: *anyopaque, vol_tag: u64, idx: u64) bool {
                const s: *Shipper = @ptrCast(@alignCast(ctx));
                assert(vol_tag == s.vol_tag);
                return s.enqueue(idx);
            }
            fn active(_: *anyopaque) bool {
                return authority_accounting_active;
            }
        };
        return .{ .ctx = self, .ship = &T.ship, .authorityActive = &T.active };
    }

    fn enqueue(self: *Shipper, idx: u64) bool {
        if (self.client == null) {
            stats.free_ship_failures.inc();
            log.err("shipped free of block {d} on vol {x}: no free target installed — nothing moved (leak-safe: the offset is durably unreferenced and the holder's next derivation returns it)", .{ idx, self.vol_tag });
            return false;
        }
        self.lock.lock();
        self.pending[self.pending_n] = idx;
        self.pending_n += 1;
        const full = self.pending_n == FREE_BATCH_MAX;
        self.lock.unlock();
        stats.free_shipped_blocks.inc();
        if (full) _ = self.flush();
        return true;
    }

    pub const FlushOutcome = enum { nothing, served, abandoned, failed };

    /// Ship the pending batch: ≤ 3 attempts; a transport failure re-resolves
    /// the venue once and re-keys; a lease-epoch change abandons; per
    /// verdict the tracking seams act (Refused touches NOTHING).
    pub fn flush(self: *Shipper) FlushOutcome {
        var batch: [FREE_BATCH_MAX]u64 = undefined;
        self.lock.lock();
        const n = self.pending_n;
        @memcpy(batch[0..n], self.pending[0..n]);
        self.pending_n = 0;
        self.lock.unlock();
        if (n == 0) return .nothing;
        const client = self.client orelse return .failed;
        const epoch0 = self.lease_epoch.epoch(self.lease_epoch.ctx, self.vol_tag);
        var request_id = self.next_request_id.fetchAdd(1, .monotonic);
        var attempt: u32 = 0;
        var resolved_once = false;
        while (attempt < FREE_SHIP_ATTEMPTS) : (attempt += 1) {
            if (attempt > 0) stats.free_ship_retries.inc();
            if (self.lease_epoch.epoch(self.lease_epoch.ctx, self.vol_tag) != epoch0) {
                stats.free_ship_abandoned.inc();
                log.warn("shipped frees on vol {x}: the lease epoch moved mid-retry ({d} blocks abandoned, leak-safe)", .{ self.vol_tag, n });
                return .abandoned;
            }
            const verdicts = client.send(client.ctx, .{ .vol_tag = self.vol_tag, .blocks = batch[0..n], .lease_epoch = epoch0, .request_id = request_id }) catch |e| switch (e) {
                error.Transport => {
                    if (!resolved_once) {
                        resolved_once = true;
                        if (client.resolve(client.ctx)) stats.joined_holder_venue_moves.inc();
                        // The old id never reached anyone: a fresh client, a fresh id.
                        request_id = self.next_request_id.fetchAdd(1, .monotonic);
                        stats.free_ship_rekeys.inc();
                    }
                    continue;
                },
                error.NoTarget => {
                    stats.free_ship_failures.inc();
                    return .failed;
                },
            };
            for (batch[0..n], verdicts.slice()) |idx, v| switch (v) {
                .freed => {
                    stats.free_ship_freed.inc();
                    self.tracking.purge(self.tracking.ctx, self.vol_tag, idx);
                    self.tracking.retire(self.tracking.ctx, self.vol_tag, idx);
                },
                .nonterminal => {
                    stats.free_ship_nonterminal.inc();
                    self.tracking.purge(self.tracking.ctx, self.vol_tag, idx);
                    self.tracking.release(self.tracking.ctx, self.vol_tag, idx);
                },
                .refused => stats.free_ship_refused.inc(),
            };
            return .served;
        }
        stats.free_ship_failures.inc();
        log.err("shipped frees on vol {x}: {d} attempts failed at the holder's venue ({d} blocks; leak-safe — the holder's next derivation returns them)", .{ self.vol_tag, FREE_SHIP_ATTEMPTS, n });
        return .failed;
    }
};

/// An in-process loopback to a `FreeExecutor` (the same process holds the
/// lease, or the contracts' two-writer fixtures).
pub const LocalClient = struct {
    executor: *FreeExecutor,
    /// Test seams: transport failures to inject, venue moves to report.
    fail_next: u32 = 0,
    resolves: u32 = 0,
    sent: u32 = 0,

    fn send(ctx: *anyopaque, req: FreeBlocksRequest) ShipError!Verdicts {
        const c: *LocalClient = @ptrCast(@alignCast(ctx));
        if (c.fail_next > 0) {
            c.fail_next -= 1;
            return error.Transport;
        }
        c.sent += 1;
        return c.executor.execute(req);
    }
    fn resolve(ctx: *anyopaque) bool {
        const c: *LocalClient = @ptrCast(@alignCast(ctx));
        c.resolves += 1;
        return true;
    }

    pub fn client(self: *LocalClient) HolderClient {
        return .{ .ctx = self, .send = &send, .resolve = &resolve };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// A RAM durable ledger: refcounts per block.
const RamLedger = struct {
    counts: [64]u32 = @splat(0),
    fn refcount(ctx: *anyopaque, _: u64, idx: u64) u32 {
        const l: *RamLedger = @ptrCast(@alignCast(ctx));
        return l.counts[idx];
    }
    fn seam(self: *RamLedger) DurableRefcounts {
        return .{ .ctx = self, .refcount = &refcount };
    }
};

/// The holder's ladder over its allocator (begin + finish; the reclaimer
/// proper is the Linux binding).
const LocalLadder = struct {
    alloc: *BlockAllocator,
    fn free(ctx: *anyopaque, idx: u64) reclaim.FreeOutcome {
        const l: *LocalLadder = @ptrCast(@alignCast(ctx));
        return switch (l.alloc.freeBlock(idx) catch return .refused) {
            .terminal => .terminal,
            .nonterminal => .nonterminal,
            .refused => .refused,
        };
    }
    fn seam(self: *LocalLadder) FreeLadder {
        return .{ .ctx = self, .free = &free };
    }
};

const Tracking = struct {
    purged: u32 = 0,
    retired: u32 = 0,
    released: u32 = 0,
    fn purge(ctx: *anyopaque, _: u64, _: u64) void {
        const t: *Tracking = @ptrCast(@alignCast(ctx));
        t.purged += 1;
    }
    fn retire(ctx: *anyopaque, _: u64, _: u64) void {
        const t: *Tracking = @ptrCast(@alignCast(ctx));
        t.retired += 1;
    }
    fn release(ctx: *anyopaque, _: u64, _: u64) void {
        const t: *Tracking = @ptrCast(@alignCast(ctx));
        t.released += 1;
    }
    fn seams(self: *Tracking) TrackingSeams {
        return .{ .ctx = self, .purge = &purge, .retire = &retire, .release = &release };
    }
};

const Epoch = struct {
    epoch: u64 = 1,
    fn get(ctx: *anyopaque, _: u64) u64 {
        const e: *Epoch = @ptrCast(@alignCast(ctx));
        return e.epoch;
    }
    fn seam(self: *Epoch) LeaseEpochSource {
        return .{ .ctx = self, .epoch = &get };
    }
};

fn testAlloc() !*BlockAllocator {
    return BlockAllocator.init(testing.allocator, .{ .vol_tag = 0xF1, .vol = 0, .block_size = 4096, .capacity_bytes = 64 * 4096 });
}

test "the holder's seven-row verdict table: Freed iff the RAM count was 1, NonTerminal above, the two live shields refuse, untracked-but-durable is nonterminal, the double-release lineage refuses, a former lessee's block is seeded and freed" {
    const a = try testAlloc();
    defer a.deinit();
    var i: u64 = 0;
    while (i < 8) : (i += 1) _ = try a.allocateBlock();
    for (0..8) |k| a.publishBlock(k);
    var ledger: RamLedger = .{};
    var ladder: LocalLadder = .{ .alloc = a };
    var ex: FreeExecutor = .{ .alloc = a, .durable = ledger.seam(), .ladder = ladder.seam() };
    // Block 0: RAM 1, durable 0 (the shipper's publish deleted its record) ⇒ Freed.
    // Block 1: RAM 2 (pinned by another owner), durable 1 ⇒ NonTerminal.
    try testing.expect(a.incrementRefcount(1));
    ledger.counts[1] = 1;
    // Block 2: RAM 1, durable 1 ⇒ freed-and-reallocated shield ⇒ Refused.
    ledger.counts[2] = 1;
    // Block 3: mid-write (unstable) ⇒ Refused.
    a.markIncarnationUnstable(3);
    // Block 4: freed locally before (untracked, free-listed), durable 0 ⇒ Refused (double release).
    _ = try a.freeBlock(4);
    // Block 20: a former lessee's granted block (bit SET at the carve, never
    // minted here), durable 1 ⇒ NonTerminal (the release already happened durably).
    try testing.expect(a.holderTakeBit(20));
    ledger.counts[20] = 1;
    // Block 21: the same shape, durable 0 ⇒ seeded and Freed.
    try testing.expect(a.holderTakeBit(21));
    // Block 40: never granted (bit clear) ⇒ Refused.
    const live0 = counters.tripwireCount("block_live_free_refusals");
    const untracked0 = counters.tripwireCount("block_untracked_free_refusals");
    block_alloc.stats.block_live_free_refusals.resetLogBudgetForTest();
    block_alloc.stats.block_untracked_free_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    const v = ex.execute(.{ .vol_tag = 0xF1, .blocks = &.{ 0, 1, 2, 3, 4, 20, 21, 40 }, .lease_epoch = 1, .request_id = 1 });
    try log.testing.expectAllFired();
    try testing.expectEqualSlices(FreeVerdict, &.{ .freed, .nonterminal, .refused, .refused, .refused, .nonterminal, .freed, .refused }, v.slice());
    try testing.expectEqual(live0 + 2, counters.tripwireCount("block_live_free_refusals"));
    try testing.expectEqual(untracked0 + 2, counters.tripwireCount("block_untracked_free_refusals")); // 4 (free-listed) and 40 (never granted)
    try testing.expectEqual(block_alloc.State.free, a.stateOf(0));
    try testing.expect(!a.bitIsSet(21)); // the seeded free cleared the carved bit
    try testing.expectEqual(@as(?u32, 1), a.refcount(1));
    try testing.expectEqual(block_alloc.State.allocated, a.stateOf(2));
    try testing.expect(!authorityAccountingActive());
    // The dedup window: the same (epoch, id) replays the verdicts, re-executing nothing.
    const replays0 = stats.free_dedup_replays.load();
    const v2 = ex.execute(.{ .vol_tag = 0xF1, .blocks = &.{ 0, 1, 2, 3, 4, 20, 21, 40 }, .lease_epoch = 1, .request_id = 1 });
    try testing.expectEqualSlices(FreeVerdict, v.slice(), v2.slice());
    try testing.expectEqual(replays0 + 1, stats.free_dedup_replays.load());
    // A fresh id under the same epoch re-executes: block 0 is now untracked
    // + free ⇒ Refused (the lineage the dedup exists to absorb).
    block_alloc.stats.block_untracked_free_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const v3 = ex.execute(.{ .vol_tag = 0xF1, .blocks = &.{0}, .lease_epoch = 1, .request_id = 2 });
    try log.testing.expectAllFired();
    try testing.expectEqual(FreeVerdict.refused, v3.buf[0]);
}

test "the untracked gate adjudicates a former lessee's allocated, unreferenced block as ours and refuses everything else; the served-publish screen refuses a released block" {
    const a = try testAlloc();
    defer a.deinit();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    var ledger: RamLedger = .{};
    var ladder: LocalLadder = .{ .alloc = a };
    var ex: FreeExecutor = .{ .alloc = a, .durable = ledger.seam(), .ladder = ladder.seam() };
    a.untracked_gate = ex.untrackedGate();
    // A former lessee's granted block (carved for a joiner, never minted here).
    try testing.expect(a.holderTakeBit(10));
    const seeded0 = stats.free_seeded_references.load();
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(10)); // adjudicated: the owner ladder, not a refusal
    try testing.expectEqual(seeded0 + 1, stats.free_seeded_references.load());
    try testing.expect(!a.bitIsSet(10));
    // Durably referenced ⇒ not ours: the gate's refusal is silent (counted
    // `block_untracked_free_adjudicated`), never the double-release tripwire.
    try testing.expect(a.holderTakeBit(11));
    ledger.counts[11] = 1;
    const adjudicated0 = block_alloc.stats.block_untracked_free_adjudicated.load();
    try testing.expectEqual(block_alloc.BeginFree.refused, try a.freeBlock(11));
    try testing.expectEqual(adjudicated0 + 1, block_alloc.stats.block_untracked_free_adjudicated.load());
    try testing.expect(a.bitIsSet(11));
    // The served-publish screen.
    try testing.expect(!servedPublishTakesReleasedBlock(a, null, 2));
    const trip0 = counters.tripwireCount("served_publish_free_block_refusals");
    stats.served_publish_free_block_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
    try testing.expect(servedPublishTakesReleasedBlock(a, null, 10));
    try log.testing.expectAllFired();
    try testing.expectEqual(trip0 + 1, counters.tripwireCount("served_publish_free_block_refusals"));
    // The holder's local target never ships.
    const t = ex.localTarget();
    try testing.expect(t.authorityActive(t.ctx));
    try testing.expect(!t.ship(t.ctx, 0xF1, 2));
}

test "the shipper: a non-holder's terminal free ships (never its own reclaimer), verdicts drive purge/retire/release and Refused touches NOTHING; a transport failure re-resolves once and re-keys; three failures are leak-safe; an epoch change abandons; the reclaimer's FreeTarget seam" {
    const holder_alloc = try testAlloc();
    defer holder_alloc.deinit();
    var i: u64 = 0;
    while (i < 4) : (i += 1) _ = try holder_alloc.allocateBlock();
    for (0..4) |k| holder_alloc.publishBlock(k);
    var ledger: RamLedger = .{};
    ledger.counts[1] = 1; // another owner remains on 1 (RAM 2 below)
    try testing.expect(holder_alloc.incrementRefcount(1));
    var ladder: LocalLadder = .{ .alloc = holder_alloc };
    var ex: FreeExecutor = .{ .alloc = holder_alloc, .durable = ledger.seam(), .ladder = ladder.seam() };
    var lc: LocalClient = .{ .executor = &ex };
    var tracking: Tracking = .{};
    var epoch: Epoch = .{};
    var shipper: Shipper = .{ .vol_tag = 0xF1, .client = lc.client(), .tracking = tracking.seams(), .lease_epoch = epoch.seam() };
    const target = shipper.freeTarget();
    try testing.expect(!target.authorityActive(target.ctx));
    const shipped0 = stats.free_shipped_blocks.load();
    try testing.expect(target.ship(target.ctx, 0xF1, 0));
    try testing.expect(target.ship(target.ctx, 0xF1, 1));
    try testing.expect(target.ship(target.ctx, 0xF1, 2));
    ledger.counts[2] = 1; // 2: freed-and-reallocated shield ⇒ Refused
    try testing.expectEqual(shipped0 + 3, stats.free_shipped_blocks.load());
    block_alloc.stats.block_live_free_refusals.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Shipper.FlushOutcome.served, shipper.flush());
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u32, 2), tracking.purged); // Freed + NonTerminal purge; Refused touches nothing
    try testing.expectEqual(@as(u32, 1), tracking.retired);
    try testing.expectEqual(@as(u32, 1), tracking.released);
    try testing.expectEqual(block_alloc.State.free, holder_alloc.stateOf(0));
    try testing.expectEqual(Shipper.FlushOutcome.nothing, shipper.flush());
    // A transport failure: re-resolve once, re-key, the retry lands.
    _ = try holder_alloc.allocateBlock(); // block 0 again
    holder_alloc.publishBlock(0);
    lc.fail_next = 1;
    const rekeys0 = stats.free_ship_rekeys.load();
    const moves0 = stats.joined_holder_venue_moves.load();
    try testing.expect(target.ship(target.ctx, 0xF1, 0));
    try testing.expectEqual(Shipper.FlushOutcome.served, shipper.flush());
    try testing.expectEqual(rekeys0 + 1, stats.free_ship_rekeys.load());
    try testing.expectEqual(moves0 + 1, stats.joined_holder_venue_moves.load());
    try testing.expectEqual(@as(u32, 1), lc.resolves);
    // Three transport failures: leak-safe failure, nothing moved.
    _ = try holder_alloc.allocateBlock();
    lc.fail_next = 3;
    const failures0 = stats.free_ship_failures.load();
    try testing.expect(target.ship(target.ctx, 0xF1, 0));
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(Shipper.FlushOutcome.failed, shipper.flush());
    try log.testing.expectAllFired();
    try testing.expectEqual(failures0 + 1, stats.free_ship_failures.load());
    try testing.expectEqual(block_alloc.State.allocated, holder_alloc.stateOf(0));
    // An epoch change mid-retry abandons.
    lc.fail_next = 1;
    try testing.expect(target.ship(target.ctx, 0xF1, 0));
    const abandoned0 = stats.free_ship_abandoned.load();
    // Fail the first attempt, then move the epoch before the retry.
    const Bump = struct {
        var ep: *Epoch = undefined;
        var inner: *LocalClient = undefined;
        fn send(_: *anyopaque, req: FreeBlocksRequest) ShipError!Verdicts {
            if (inner.fail_next > 0) {
                inner.fail_next -= 1;
                ep.epoch += 1;
                return error.Transport;
            }
            return inner.executor.execute(req);
        }
        fn resolve(_: *anyopaque) bool {
            return false;
        }
    };
    Bump.ep = &epoch;
    Bump.inner = &lc;
    shipper.client = .{ .ctx = &lc, .send = &Bump.send, .resolve = &Bump.resolve };
    try testing.expectEqual(Shipper.FlushOutcome.abandoned, shipper.flush());
    try testing.expectEqual(abandoned0 + 1, stats.free_ship_abandoned.load());
    // No client installed: the enqueue fails loud and leak-safe.
    shipper.client = null;
    log.testing.expectErrorLogs(1);
    try testing.expect(!target.ship(target.ctx, 0xF1, 0));
    try log.testing.expectAllFired();
    // The reclaimer's ladder on a non-holder ships and answers nonterminal
    // (nothing freed locally).
    const joiner = try testAlloc();
    defer joiner.deinit();
    _ = try joiner.allocateBlock();
    shipper.client = lc.client();
    const Skip = struct {
        fn issue(_: ?*anyopaque, _: u64, _: u64) @import("../../core/errors.zig").Error!void {}
    };
    const r = try reclaim.Reclaimer.init(testing.allocator, .{ .budget_bytes = 1 << 30, .batch_ms = 0 });
    defer r.deinit();
    r.registerTarget(0, joiner, .{ .ctx = null, .substrate = .skip, .issue = &Skip.issue });
    r.setFreeTarget(0, shipper.freeTarget());
    try testing.expectEqual(reclaim.FreeOutcome.nonterminal, r.freeBlock(testing.io, 0, 0, joiner.stampOf(0)));
    try testing.expectEqual(block_alloc.State.allocated, joiner.stateOf(0)); // the joiner's allocator moved nothing
    try testing.expectEqual(@as(u8, 1), shipper.pending_n);
    r.setFreeTarget(0, null);
    try testing.expectEqual(reclaim.FreeOutcome.terminal, r.freeBlock(testing.io, 0, 0, joiner.stampOf(0)));
    _ = r.drainSync(testing.io);
}
