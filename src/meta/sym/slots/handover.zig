//! HANDOVER — holder-decided, ONE requester, never thrashing (charter §5.5
//! "Handover"; `docs/porting/sym_slot_leases.md` §4): the dominance window
//! over two half-window buckets (`T_idle = T_owner`), the offer rule
//! `ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor` with `N_floor = max(2,
//! ceil(ewma_handover_ns / ewma_ship_ns))` (cold start `4 × ewma_barrier
//! + 3 × ewma_ship_rtt`), the directory-parent memo walk that credits a
//! holder's namespace op to its SUBTREE, the cooldown valve, and the
//! holder's FLUSH-THEN-TRANSFER in its five durable steps over a `Steps`
//! seam — gate `releasing` → drain the door → cycle until the region's
//! `reusable_upto` passes the slot's record frontier and the root is
//! published (`COVER_CYCLES_MAX`; a MOVING tail at the bound defers, an
//! unmoved one is Corrupt — spec §9 Q12's one law) → the page with the
//! slot `Releasing{root, cursor, g}` + barrier → `ReleaseSlot{root, cursor,
//! tails, seq_floor}` (tree 0 `Unleased`) → the page drops the slot. Two
//! durable homes at every instant — never zero. PORTABLE; the measured
//! cost beside what it justifies: 5.5–7.8 ms per idle-arm handover on the
//! fleet (flush 1.9–4.1 / page 0.15–0.18 / tree 0 3.5 ms), `N_floor ≈ 18`
//! after the first handover fed the EWMA (5.5 ms ÷ ≈ 0.3 ms ships).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const log = @import("../../../core/log.zig");
const checkpoint = @import("../../checkpoint.zig");
const jc = @import("../../journal_core.zig");
const cr = @import("../control_records.zig");
const lt = @import("lease_table.zig");
const door_mod = @import("door.zig");

const assert = std.debug.assert;
pub const LeaseTable = lt.LeaseTable;
pub const Door = door_mod.Door;
pub const AppenderId = lt.AppenderId;
pub const SlotWords = lt.SlotWords;

pub const stats = struct {
    pub var slot_offers: counters.ShardedCounter = .{};
    pub var slot_offers_idle: counters.ShardedCounter = .{};
    pub var slot_offers_dominated: counters.ShardedCounter = .{};
    pub var slot_offers_expired: counters.ShardedCounter = .{};
    /// A second dominating ship before the accept (legal).
    pub var slot_offers_busy: counters.ShardedCounter = .{};
    /// Releases that spent a recall + in-process accepts.
    pub var slot_handovers: counters.ShardedCounter = .{};
    /// A moving tail at the cover bound: the release aborted, retried.
    pub var slot_handover_deferrals: counters.ShardedCounter = .{};
    /// Served ships / foreign touches evaluated.
    pub var slot_ships: counters.ShardedCounter = .{};
    /// Exact-sum `flush / page / tree0 / grant / total`.
    pub var slot_handover_phase_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// `N_floor` in force.
    pub var slot_offer_n_floor: counters.Gauge = .{};
    /// Offers to a WIRE requester pushed onto its renewal carriage (the
    /// manager-held slot's handover over the wire; `slot_offers` counts
    /// the offer itself).
    pub var slot_offers_pushed: counters.ShardedCounter = .{};
    /// Pending-times refinements dropped at the drain's door: the slot had
    /// moved to another appender before they were journaled. The handover's
    /// step 0 (`TimesDrain`) exists so this reads 0 across a handover.
    pub var meta_kv_times_echo_foreign_dropped: counters.ShardedCounter = .{};
    /// Parked refinements the handover's step 0 made durable (one batched
    /// Δ entry per `dlm.max_requests` inos) before the slot's door closed.
    pub var meta_kv_times_handover_drained: counters.ShardedCounter = .{};
};

/// The filesystem's parked-times drain scoped to ONE routing slot (the
/// handover's step 0; `kv_fs.drainPendingTimesOfSlot`): every stamp parked
/// on an inode of the slot committed as a Δ while the departing holder's
/// door still admits its commits — a `utimes`-visible mtime is never lost
/// across a handover. Answers the refinements made durable; a failure is
/// the filesystem's to log (the step is best effort: the drain's door
/// refusal catches what it misses, counted).
pub const TimesDrain = struct {
    ctx: *anyopaque,
    drainSlot: *const fn (ctx: *anyopaque, slot: u16) u64,
};

pub const Phase = enum(u8) {
    flush,
    page,
    tree0,
    grant,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
    pub const legs = count - 1;
};

pub const Phases = struct {
    flush_ns: u64 = 0,
    page_ns: u64 = 0,
    tree0_ns: u64 = 0,
    grant_ns: u64 = 0,

    pub fn total(self: Phases) u64 {
        return self.flush_ns + self.page_ns + self.tree0_ns + self.grant_ns;
    }

    /// `total ≡ flush + page + tree0 + grant`, one sample per leg.
    pub fn record(self: Phases) void {
        stats.slot_handover_phase_ns[@backingInt(Phase.flush)].record(self.flush_ns);
        stats.slot_handover_phase_ns[@backingInt(Phase.page)].record(self.page_ns);
        stats.slot_handover_phase_ns[@backingInt(Phase.tree0)].record(self.tree0_ns);
        stats.slot_handover_phase_ns[@backingInt(Phase.grant)].record(self.grant_ns);
        stats.slot_handover_phase_ns[@backingInt(Phase.total)].record(self.total());
    }
};

// ---------------------------------------------------------------------------
// Constants, with WHY.
// ---------------------------------------------------------------------------

/// At most one requester can dominate (≥ 2× the holder, one named
/// identity); 8 equal requesters each hold ≤ 1/8 and none reaches 2×
/// unless the holder is idle, where `N_floor` decides.
pub const REQUESTERS_PER_SLOT_MAX: usize = 8;
/// `T_idle = T_owner`: the membership lease the slot lease lives as long
/// as (`claim.STALE_TTL_S` = 45 s).
pub const T_IDLE_DEFAULT_NS: u64 = 45 * std.time.ns_per_s;
/// The cooldown in windows: bounds the worst-case residual thrash duty
/// cycle at `cycles / (cycles + 8) ≈ 27 %`.
pub const RECALL_COOLDOWN_WINDOWS: u64 = 8;
/// The EWMA shift: `ewma − ewma/8 + sample/8` (the first sample seeds).
pub const EWMA_SHIFT: u6 = 3;
/// A single touch never moves anything.
pub const N_FLOOR_MIN: u64 = 2;

pub fn ewmaFold(ewma: u64, sample: u64) u64 {
    if (ewma == 0) return sample;
    return ewma - (ewma >> EWMA_SHIFT) + (sample >> EWMA_SHIFT);
}

/// `N_floor = max(2, ceil(ewma_handover / max(ewma_ship, 1)))`: a handover
/// must pay for itself in ships saved.
pub fn nFloor(ewma_handover_ns: u64, ewma_ship_ns: u64) u64 {
    const ship = @max(ewma_ship_ns, 1);
    return @max(N_FLOOR_MIN, (ewma_handover_ns + ship - 1) / ship);
}

/// The cold-start seed: what a handover IS before one was measured — the
/// idle arm ≈ 4 barriers + 3 round trips.
pub fn coldStartHandoverNs(ewma_barrier_ns: u64, ewma_ship_rtt_ns: u64) u64 {
    return 4 * ewma_barrier_ns + 3 * ewma_ship_rtt_ns;
}

/// `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` verbatim, else `8 × T_idle`.
pub fn recallCooldownNs(explicit_ms: ?u64, t_idle_ns: u64) u64 {
    if (explicit_ms) |ms| return ms * std.time.ns_per_ms;
    return RECALL_COOLDOWN_WINDOWS * t_idle_ns;
}

pub const Decision = enum { serve, offer_idle, offer_dominated };

/// The rule: `ops_q ≥ N_floor ∧ ops_q ≥ 2 × ops_h` ⇒ idle iff `ops_h == 0`
/// else dominated; else serve. No sustain clause, no immunity clause.
pub fn decide(ops_q: u64, ops_h: u64, n_floor: u64) Decision {
    if (ops_q < n_floor or ops_q < 2 * ops_h) return .serve;
    return if (ops_h == 0) .offer_idle else .offer_dominated;
}

// ---------------------------------------------------------------------------
// The dominance window (spec §4.1 `DominanceWindow`).
// ---------------------------------------------------------------------------

const RequesterOps = struct {
    id: AppenderId = 0,
    epoch: u32 = 0,
    cur: u32 = 0,
    prev: u32 = 0,
    touched: u32 = 0,

    fn total(self: RequesterOps, epoch: u32) u64 {
        if (self.epoch == epoch) return @as(u64, self.cur) + self.prev;
        if (self.epoch +% 1 == epoch) return self.cur;
        return 0;
    }
};

const SlotRequesters = struct {
    rows: [REQUESTERS_PER_SLOT_MAX]RequesterOps = @splat(.{}),
    n: u8 = 0,
    /// Ships SERVED on the slot (every requester): each one committed
    /// under the holder's own door and counted there as a holder op, so
    /// the rule reads the holder's ops NET of them (`servedOps`).
    served: RequesterOps = .{},
};

pub const DominanceWindow = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    slots: std.AutoHashMapUnmanaged(u16, SlotRequesters) = .empty,
    t_idle_ns: u64,

    pub fn init(gpa: Allocator, t_idle_ns: u64) DominanceWindow {
        return .{ .gpa = gpa, .t_idle_ns = t_idle_ns };
    }

    pub fn deinit(self: *DominanceWindow) void {
        self.slots.deinit(self.gpa);
    }

    fn epochOf(self: *const DominanceWindow, now_ns: u64) u32 {
        return @truncate(now_ns / @max(self.t_idle_ns / 2, 1));
    }

    /// One requester op on `slot` at `now`; answers the requester's total
    /// over the window. An evicted requester (the LRU victim = min(touched
    /// epoch, ops)) re-enters at 0 and pays at most one more window — the
    /// dominator is the most active and never the victim.
    pub fn note(self: *DominanceWindow, slot: u16, q: AppenderId, now_ns: u64) Allocator.Error!u64 {
        const epoch = self.epochOf(now_ns);
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.slots.getOrPut(self.gpa, slot);
        if (!gop.found_existing) gop.value_ptr.* = .{};
        const sr = gop.value_ptr;
        var row: ?*RequesterOps = null;
        for (sr.rows[0..sr.n]) |*r| if (r.id == q) {
            row = r;
        };
        if (row == null) {
            if (sr.n < REQUESTERS_PER_SLOT_MAX) {
                row = &sr.rows[sr.n];
                sr.n += 1;
            } else {
                var victim = &sr.rows[0];
                for (sr.rows[1..]) |*r| {
                    if (r.touched < victim.touched or (r.touched == victim.touched and r.total(epoch) < victim.total(epoch))) victim = r;
                }
                row = victim;
            }
            row.?.* = .{ .id = q };
        }
        const r = row.?;
        if (r.epoch == epoch) {
            r.cur +|= 1;
        } else if (r.epoch +% 1 == epoch) {
            r.prev = r.cur;
            r.cur = 1;
            r.epoch = epoch;
        } else {
            r.prev = 0;
            r.cur = 1;
            r.epoch = epoch;
        }
        r.touched = epoch;
        return r.total(epoch);
    }

    /// One SERVED ship on `slot` at `now` (the apply committed under the
    /// holder's door); answers the slot's served total over the window.
    pub fn noteServed(self: *DominanceWindow, slot: u16, now_ns: u64) Allocator.Error!u64 {
        const epoch = self.epochOf(now_ns);
        self.lock.lock();
        defer self.lock.unlock();
        const gop = try self.slots.getOrPut(self.gpa, slot);
        if (!gop.found_existing) gop.value_ptr.* = .{};
        const r = &gop.value_ptr.served;
        if (r.epoch == epoch) {
            r.cur +|= 1;
        } else if (r.epoch +% 1 == epoch) {
            r.prev = r.cur;
            r.cur = 1;
            r.epoch = epoch;
        } else {
            r.prev = 0;
            r.cur = 1;
            r.epoch = epoch;
        }
        return r.total(epoch);
    }

    pub fn servedOps(self: *DominanceWindow, slot: u16, now_ns: u64) u64 {
        const epoch = self.epochOf(now_ns);
        self.lock.lock();
        defer self.lock.unlock();
        const sr = self.slots.get(slot) orelse return 0;
        return sr.served.total(epoch);
    }

    pub fn opsOf(self: *DominanceWindow, slot: u16, q: AppenderId, now_ns: u64) u64 {
        const epoch = self.epochOf(now_ns);
        self.lock.lock();
        defer self.lock.unlock();
        const sr = self.slots.get(slot) orelse return 0;
        for (sr.rows[0..sr.n]) |r| if (r.id == q) return r.total(epoch);
        return 0;
    }

    /// The slot left this mount: its requesters forgotten.
    pub fn forget(self: *DominanceWindow, slot: u16) void {
        self.lock.lock();
        defer self.lock.unlock();
        _ = self.slots.remove(slot);
    }
};

// ---------------------------------------------------------------------------
// The offer judge: the EWMAs, N_floor, the cooldown, the rule.
// ---------------------------------------------------------------------------

pub const Judge = struct {
    table: *LeaseTable,
    window: DominanceWindow,
    t_idle_ns: u64,
    cooldown_ns: u64,
    /// Single-writer words under the handover mutex (spec §9 Q15: the
    /// folds are CAS-free because the mutex serializes them).
    ewma_handover_ns: std.atomic.Value(u64) = .init(0),
    ewma_ship_ns: std.atomic.Value(u64) = .init(0),

    pub fn init(gpa: Allocator, table: *LeaseTable, t_idle_ns: u64, cooldown_ns: u64) Judge {
        return .{ .table = table, .window = DominanceWindow.init(gpa, t_idle_ns), .t_idle_ns = t_idle_ns, .cooldown_ns = cooldown_ns };
    }

    pub fn deinit(self: *Judge) void {
        self.window.deinit();
    }

    /// The cold start: `4 × barrier + 3 × rtt` for the handover, one ship
    /// round trip for the ship (seeded once, never over a measurement).
    pub fn seed(self: *Judge, ewma_barrier_ns: u64, ewma_ship_rtt_ns: u64) void {
        if (self.ewma_handover_ns.load(.acquire) == 0) self.ewma_handover_ns.store(coldStartHandoverNs(ewma_barrier_ns, ewma_ship_rtt_ns), .release);
        if (self.ewma_ship_ns.load(.acquire) == 0) self.ewma_ship_ns.store(@max(ewma_ship_rtt_ns, 1), .release);
        stats.slot_offer_n_floor.set(self.nFloorNow());
    }

    pub fn nFloorNow(self: *const Judge) u64 {
        return nFloor(self.ewma_handover_ns.load(.acquire), self.ewma_ship_ns.load(.acquire));
    }

    pub fn foldHandover(self: *Judge, wall_ns: u64) void {
        self.ewma_handover_ns.store(ewmaFold(self.ewma_handover_ns.load(.acquire), wall_ns), .release);
        stats.slot_offer_n_floor.set(self.nFloorNow());
    }

    pub fn foldShip(self: *Judge, wall_ns: u64) void {
        self.ewma_ship_ns.store(ewmaFold(self.ewma_ship_ns.load(.acquire), @max(wall_ns, 1)), .release);
        stats.slot_offer_n_floor.set(self.nFloorNow());
    }

    /// A served ship (or, in one process, a refused foreign touch) by `q`
    /// on `slot` held by this mount: the requester's op noted, the
    /// holder's read over the SAME window, the rule, the cooldown valve.
    /// `served`: the op was APPLIED here under the holder's door (a wire
    /// ship) — the door counted it as a holder op, so the holder's ops are
    /// read net of every served ship on the slot; a refused in-process
    /// touch committed nothing and subtracts nothing.
    pub fn noteShip(self: *Judge, slot: u16, q: AppenderId, now_ns: u64, served: bool) Allocator.Error!Decision {
        stats.slot_ships.inc();
        const ops_q = try self.window.note(slot, q, now_ns);
        const served_total = if (served) try self.window.noteServed(slot, now_ns) else self.window.servedOps(slot, now_ns);
        const ops_h = self.table.holderOps(slot, now_ns, self.t_idle_ns) -| served_total;
        const d = decide(ops_q, ops_h, self.nFloorNow());
        if (d == .serve) return .serve;
        const at = self.table.handoverAt(slot);
        if (at != 0 and now_ns -| at < self.cooldown_ns) return .serve;
        return d;
    }
};

// ---------------------------------------------------------------------------
// `ops_h` on the SUBTREE: the directory-parent memo walk (spec §4.2).
// ---------------------------------------------------------------------------

/// The filesystem's directory-parent memo alone (global inos): the plane
/// composes the routing and lease predicates around it.
pub const ParentMemo = struct {
    ctx: *anyopaque,
    parentOf: *const fn (ctx: *anyopaque, ino: u64) ?u64,
};

/// The routed layer's `dir_parents` memo and the cross-volume credit.
pub const Memo = struct {
    ctx: *anyopaque,
    /// The parent of directory `ino` (key ino), null on a miss (the walk
    /// ends: a miss under-credits, which is the safe direction).
    parentOf: *const fn (ctx: *anyopaque, ino: u64) ?u64,
    /// The slot of a key ino and whether THIS mount leases it on THIS
    /// volume; an ancestor on ANOTHER volume is credited there.
    slotOf: *const fn (ctx: *anyopaque, ino: u64) u16,
    leasesHere: *const fn (ctx: *anyopaque, slot: u16) bool,
    creditElsewhere: ?*const fn (ctx: *anyopaque, ino: u64, now_ns: u64) void = null,
    onThisVolume: *const fn (ctx: *anyopaque, ino: u64) bool,
};

/// Credit one namespace op under `parent` to every ancestor's slot this
/// mount leases (the tx's own slots skipped; a foreign slot's ops are its
/// holder's; a memo miss ends the walk). Bounded by `MAX_DEPTH` against a
/// memo cycle. Answers the levels credited.
pub const MAX_DEPTH: usize = 4096;

pub fn creditAncestors(table: *LeaseTable, memo: Memo, parent: u64, own_slots: []const u16, now_ns: u64, t_idle_ns: u64) usize {
    var ino = parent;
    var credited: usize = 0;
    var depth: usize = 0;
    while (depth < MAX_DEPTH) : (depth += 1) {
        if (!memo.onThisVolume(memo.ctx, ino)) {
            if (memo.creditElsewhere) |f| f(memo.ctx, ino, now_ns);
        } else {
            const slot = memo.slotOf(memo.ctx, ino);
            var own = false;
            for (own_slots) |s| own = own or s == slot;
            if (!own and memo.leasesHere(memo.ctx, slot)) {
                table.noteHolderOp(slot, now_ns, t_idle_ns);
                credited += 1;
            }
        }
        if (ino <= 1) break;
        ino = memo.parentOf(memo.ctx, ino) orelse break;
    }
    return credited;
}

// ---------------------------------------------------------------------------
// Flush-then-transfer (spec §4.6).
// ---------------------------------------------------------------------------

pub const Error = error{
    /// A concurrent release or transfer took the slot.
    Busy,
    /// The cover loop reached the bound with a MOVING tail: the release
    /// aborted, the cadence retries.
    HandoverDeferred,
    Io,
    Corrupt,
    OutOfMemory,
};

/// The kill-matrix seams (`SQUEEZEFS_TEST_HANDOVER_HOLD`): die or park
/// after a named step. A field of the plane, never a static.
pub const Hold = enum(u8) { off, after_page, after_tree0, park_after_page };

pub const Seam = struct {
    hold: Hold = .off,
    /// `park_after_page`: the engine parks here until released.
    reached: Io.Event = .unset,
    release: Io.Event = .unset,
    /// `after_*`: the run answers this instead of proceeding (the test's
    /// "kill -9 here" — the durable state is what a death leaves).
    pub const Died = error{HeldAtSeam};
};

/// The holder-side steps bound by the plane (bind.zig) or a RAM harness.
pub const Steps = struct {
    ctx: *anyopaque,
    /// Step 0: drain the slot's RAM-parked pending-times refinements while
    /// the door still admits them (best effort).
    drainPendingTimes: *const fn (ctx: *anyopaque, slot: u16) void,
    /// One barriered checkpoint cycle; answers the LESSEE's ring's
    /// `reusable_upto` (the frontier is a position in that ring).
    cycleOnce: *const fn (ctx: *anyopaque, slot: u16) Error!u64,
    /// The lessee's ring's `reusable_upto` now.
    reusableUpto: *const fn (ctx: *anyopaque, slot: u16) u64,
    /// Is the slot's live root named by a durable home (page or tree 0)?
    rootPublished: *const fn (ctx: *anyopaque, slot: u16) bool,
    /// Step 2b: every read token this holder granted on the slot's
    /// objects recalled and waited for (the tokens wave; inert until).
    recallTokens: *const fn (ctx: *anyopaque, slot: u16) void,
    /// `slot_words_now`: the live root, the cell's cursor, the extents,
    /// `max(the ring's stamp frontier, the recorded floor)`.
    words: *const fn (ctx: *anyopaque, slot: u16) SlotWords,
    /// Every reachable leaf's flushed tail (owned by `gpa`).
    tails: *const fn (ctx: *anyopaque, gpa: Allocator, slot: u16) Error![]cr.TailEntry,
    /// Step 3: the page with the slot `Releasing{root, cursor, g}` + barrier.
    pageReleasing: *const fn (ctx: *anyopaque, slot: u16, g: u32, words: SlotWords) Error!void,
    /// Step 4: tree 0 `Unleased` + `slot_tails` (the manager's verb or the
    /// wire `ReleaseSlot`), barriered by its lane.
    releaseDurably: *const fn (ctx: *anyopaque, slot: u16, g: u32, words: SlotWords, tails: []const cr.TailEntry) Error!void,
    /// Step 5: RAM first (the region's lease, the rotor, the window, the
    /// cursor cell), then the page without the slot.
    pageDrop: *const fn (ctx: *anyopaque, slot: u16) Error!void,
    seam: *Seam,
};

/// Run the holder's flush-then-transfer of `slot` (held by `holder`) to
/// nobody (the cadence) or to a requester (the accept — the grant follows
/// under the caller's handover hold). Answers the phases. Abort at any
/// step before 4 reopens the door (`endRelease`): the slot accepts commits
/// again, the parkers wake.
pub fn transfer(gpa: Allocator, door: *Door, table: *LeaseTable, steps: Steps, io: Io, holder: AppenderId, slot: u16) (Error || Seam.Died)!Phases {
    const t_start = clock.monotonicNs();
    // Step 0: BEFORE the gate — the refinements must still pass the door.
    steps.drainPendingTimes(steps.ctx, slot);
    // Step 1: the gate FIRST (no new commit passes), then the table.
    const g = door.beginRelease(slot, holder, false) orelse return error.Busy;
    var committed = false;
    defer if (!committed) door.endRelease(slot);
    // Step 1b: every admitted commit at its terminal outcome.
    _ = door.drain(slot, std.math.maxInt(u64));
    // Step 2: cycle until the tail passes the slot's frontier and the
    // root is published; bounded; the moving/unmoved split at the bound.
    const tail_start = steps.reusableUpto(steps.ctx, slot);
    var tail = tail_start;
    var cycles: u32 = 0;
    while (true) {
        if (tail >= table.frontierOf(slot) and steps.rootPublished(steps.ctx, slot)) break;
        if (cycles >= checkpoint.COVER_CYCLES_MAX) {
            if (tail == tail_start) {
                log.err("slot leases: the handover of slot {d} cycled {d} times and the region's tail never moved from {d} — a stuck reservation (a defect, never a longer wait)", .{ slot, cycles, tail });
                return error.Corrupt;
            }
            stats.slot_handover_deferrals.inc();
            return error.HandoverDeferred;
        }
        tail = try steps.cycleOnce(steps.ctx, slot);
        cycles += 1;
    }
    // Step 2b: the token half.
    steps.recallTokens(steps.ctx, slot);
    const words = steps.words(steps.ctx, slot);
    const tails = try steps.tails(steps.ctx, gpa, slot);
    defer gpa.free(tails);
    const t_flushed = clock.monotonicNs();
    // Step 3: the page with the slot Releasing + barrier.
    try steps.pageReleasing(steps.ctx, slot, g, words);
    const t_paged = clock.monotonicNs();
    switch (steps.seam.hold) {
        .after_page => return error.HeldAtSeam,
        .park_after_page => {
            steps.seam.reached.set(io);
            steps.seam.release.waitUncancelable(io);
        },
        else => {},
    }
    // Step 4: tree 0 (the release lands; a crash past this is row 6).
    try steps.releaseDurably(steps.ctx, slot, g, words, tails);
    committed = true;
    const t_tree0 = clock.monotonicNs();
    if (steps.seam.hold == .after_tree0) return error.HeldAtSeam;
    // Step 5: RAM first, then the page without the slot.
    try steps.pageDrop(steps.ctx, slot);
    door.wakeParkers();
    const ph: Phases = .{ .flush_ns = t_flushed - t_start, .page_ns = t_paged - t_flushed, .tree0_ns = t_tree0 - t_paged, .grant_ns = 0 };
    ph.record();
    log.info("slot leases: slot {d} released by appender {d} at g {d} — flush {d} µs ({d} cycles), page {d} µs, tree 0 {d} µs, {d} tails", .{ slot, holder, g, ph.flush_ns / 1000, cycles, ph.page_ns / 1000, ph.tree0_ns / 1000, tails.len });
    return ph;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "derivations: the EWMA seeds then folds by eighths; N_floor floors at 2 and ceils the ratio (≈ 18 at the fleet's 5.5 ms ÷ 0.3 ms); the cold start is 4 barriers + 3 round trips; the cooldown is 8 windows unless explicit; the rule has no sustain clause" {
    try testing.expectEqual(@as(u64, 100), ewmaFold(0, 100));
    try testing.expectEqual(@as(u64, 100 - 12 + 25), ewmaFold(100, 200));
    try testing.expectEqual(@as(u64, 2), nFloor(0, 300));
    try testing.expectEqual(@as(u64, 2), nFloor(500, 300));
    try testing.expectEqual(@as(u64, 19), nFloor(5_500_000, 300_000));
    try testing.expectEqual(@as(u64, 7), coldStartHandoverNs(1, 1));
    try testing.expectEqual(@as(u64, 4 * 900 + 3 * 300), coldStartHandoverNs(900, 300));
    try testing.expectEqual(@as(u64, 360 * std.time.ns_per_s), recallCooldownNs(null, T_IDLE_DEFAULT_NS));
    try testing.expectEqual(@as(u64, 5 * std.time.ns_per_ms), recallCooldownNs(5, T_IDLE_DEFAULT_NS));
    try testing.expectEqual(Decision.serve, decide(1, 0, 2));
    try testing.expectEqual(Decision.offer_idle, decide(2, 0, 2));
    try testing.expectEqual(Decision.serve, decide(3, 2, 2));
    try testing.expectEqual(Decision.offer_dominated, decide(4, 2, 2));
    try testing.expectEqual(Decision.serve, decide(10, 0, 11));
}

test "dominance over a common window: a requester's ops rotate with the holder's buckets; 8 equal requesters never dominate a live holder; the 9th evicts the LRU and re-enters at 0; the judge serves inside the cooldown and offers idle past N_floor" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    const half: u64 = 1000;
    var j = Judge.init(gpa, t, 2 * half, 10 * half);
    defer j.deinit();
    j.seed(100, 100);
    try testing.expectEqual(@as(u64, 7), j.nFloorNow());
    _ = try t.acquire(5, 1, false);
    // The holder does 3 ops; requester 2 does 6: not 2× and below N_floor.
    for (0..3) |_| t.noteHolderOp(5, 100, 2 * half);
    var d: Decision = .serve;
    for (0..6) |_| d = try j.noteShip(5, 2, 100, false);
    try testing.expectEqual(Decision.serve, d);
    // Seven more: 13 ≥ 7 and ≥ 2 × 3 ⇒ dominated.
    for (0..7) |_| d = try j.noteShip(5, 2, 100, false);
    try testing.expectEqual(Decision.offer_dominated, d);
    // Two epochs later everything zeroes; an idle holder is offered at
    // N_floor ships.
    for (0..6) |_| d = try j.noteShip(5, 2, 100 + 2 * half, false);
    try testing.expectEqual(Decision.serve, d);
    d = try j.noteShip(5, 2, 100 + 2 * half, false);
    try testing.expectEqual(Decision.offer_idle, d);
    // The cooldown: a handover at this instant silences the next window.
    try t.noteHandover(5, 100 + 2 * half);
    for (0..10) |_| d = try j.noteShip(5, 2, 200 + 2 * half, false);
    try testing.expectEqual(Decision.serve, d);
    for (0..10) |_| d = try j.noteShip(5, 2, 100 + 13 * half, false);
    try testing.expectEqual(Decision.offer_idle, d);
    // 8 requesters each at 1/8: none reaches 2× a holder of 1 op.
    t.noteHolderOp(7, 100 + 20 * half, 2 * half);
    _ = try t.acquire(7, 1, false);
    var i: AppenderId = 10;
    while (i < 18) : (i += 1) d = try j.noteShip(7, i, 100 + 20 * half, false);
    try testing.expectEqual(Decision.serve, d);
    // The 9th evicts the LRU (all equal: the lowest ops/touched) and starts at 0.
    try testing.expectEqual(@as(u64, 1), try j.window.note(7, 99, 100 + 20 * half));
    try testing.expectEqual(@as(u64, 0), j.window.opsOf(7, 10, 100 + 20 * half));
    j.window.forget(7);
    try testing.expectEqual(@as(u64, 0), j.window.opsOf(7, 99, 100 + 20 * half));
}

test "a SERVED wire ship commits under the holder's door and the door counts it as a holder op (rung 10): the judge reads the holder's ops net of the slot's served ships, so a wire requester dominates exactly as an in-process one does; the holder's OWN ops still count" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    const half: u64 = 1000;
    var j = Judge.init(gpa, t, 2 * half, 10 * half);
    defer j.deinit();
    j.seed(100, 100);
    try testing.expectEqual(@as(u64, 7), j.nFloorNow());
    _ = try t.acquire(5, 0, false);
    // The holder's own 3 ops, then 13 ships from requester 2 each applied
    // under the holder's door (the door's count climbs with them).
    for (0..3) |_| t.noteHolderOp(5, 100, 2 * half);
    var d: Decision = .serve;
    for (0..13) |_| {
        t.noteHolderOp(5, 100, 2 * half);
        d = try j.noteShip(5, 2, 100, true);
    }
    // Gross the holder read 16 ≥ 13/2; net of the 13 served it reads 3:
    // 13 ≥ N_floor and ≥ 2 × 3 ⇒ dominated.
    try testing.expectEqual(@as(u64, 16), t.holderOps(5, 100, 2 * half));
    try testing.expectEqual(@as(u64, 13), j.window.servedOps(5, 100));
    try testing.expectEqual(Decision.offer_dominated, d);
    // The holder's own ops still defend: 20 more of its own and the next
    // served ship serves (14 < 2 × 23).
    for (0..20) |_| t.noteHolderOp(5, 100, 2 * half);
    t.noteHolderOp(5, 100, 2 * half);
    try testing.expectEqual(Decision.serve, try j.noteShip(5, 2, 100, true));
    // An in-process refusal subtracts nothing of its own but reads the
    // same served total.
    try testing.expectEqual(Decision.serve, try j.noteShip(5, 3, 100, false));
    try testing.expectEqual(@as(u64, 14), j.window.servedOps(5, 100));
}

const MemoFixture = struct {
    /// parent[ino]; 1 is the root.
    parents: [8]u64 = .{ 0, 0, 1, 2, 3, 1, 5, 0 },
    /// slot = ino for the fixture; leased here: 1, 2, 3 not 5.
    leased: [8]bool = .{ false, true, true, true, false, false, true, false },
    foreign_vol: [8]bool = .{ false, false, false, false, false, true, false, false },
    elsewhere: u32 = 0,

    fn parentOf(ctx: *anyopaque, ino: u64) ?u64 {
        const self: *MemoFixture = @ptrCast(@alignCast(ctx));
        if (ino >= 8 or self.parents[ino] == 0) return null;
        return self.parents[ino];
    }
    fn slotOf(_: *anyopaque, ino: u64) u16 {
        return @intCast(ino);
    }
    fn leasesHere(ctx: *anyopaque, slot: u16) bool {
        return @as(*MemoFixture, @ptrCast(@alignCast(ctx))).leased[slot];
    }
    fn onThisVolume(ctx: *anyopaque, ino: u64) bool {
        return !@as(*MemoFixture, @ptrCast(@alignCast(ctx))).foreign_vol[ino];
    }
    fn creditElsewhere(ctx: *anyopaque, _: u64, _: u64) void {
        @as(*MemoFixture, @ptrCast(@alignCast(ctx))).elsewhere += 1;
    }
    fn memo(self: *MemoFixture) Memo {
        return .{ .ctx = self, .parentOf = parentOf, .slotOf = slotOf, .leasesHere = leasesHere, .creditElsewhere = creditElsewhere, .onThisVolume = onThisVolume };
    }
};

test "the directory-parent memo: a create under /2/3/4 credits the ancestors 3, 2 and 1 this mount leases (the tx's own slot 4 skipped; a holder live below a directory is never idle at it); an ancestor on another volume is credited there; a memo miss ends the walk" {
    const gpa = testing.allocator;
    const t = try LeaseTable.init(gpa);
    defer t.deinit();
    var f: MemoFixture = .{};
    const t_idle: u64 = 1 << 40;
    try testing.expectEqual(@as(usize, 3), creditAncestors(t, f.memo(), 4, &.{4}, 100, t_idle));
    try testing.expectEqual(@as(u64, 0), t.holderOps(4, 100, t_idle));
    try testing.expectEqual(@as(u64, 1), t.holderOps(3, 100, t_idle));
    try testing.expectEqual(@as(u64, 1), t.holderOps(2, 100, t_idle));
    try testing.expectEqual(@as(u64, 1), t.holderOps(1, 100, t_idle));
    // /1/5(foreign volume)/6: 6 credited here, 5 elsewhere, 1 here.
    try testing.expectEqual(@as(usize, 2), creditAncestors(t, f.memo(), 6, &.{}, 100, t_idle));
    try testing.expectEqual(@as(u32, 1), f.elsewhere);
    try testing.expectEqual(@as(u64, 2), t.holderOps(1, 100, t_idle));
    // A miss (7 has no memo parent) credits nothing past itself.
    try testing.expectEqual(@as(usize, 0), creditAncestors(t, f.memo(), 7, &.{}, 100, t_idle));
}

/// A RAM harness for the transfer: every step a counter, the tail moving
/// per cycle or stuck, the root published after N cycles.
const TransferFixture = struct {
    gpa: Allocator,
    table: *LeaseTable,
    door: Door,
    seam: Seam = .{},
    tail: u64 = 0,
    tail_moves: bool = true,
    cycles_until_published: u32 = 0,
    cycles: u32 = 0,
    paged: u32 = 0,
    released: u32 = 0,
    dropped: u32 = 0,
    drained_times: u32 = 0,
    recalled: u32 = 0,
    last_g: u32 = 0,
    last_words: SlotWords = .{},

    fn init(gpa: Allocator) !*TransferFixture {
        const self = try gpa.create(TransferFixture);
        self.* = .{ .gpa = gpa, .table = try LeaseTable.init(gpa), .door = undefined };
        self.door = .{ .table = self.table, .io = testing.io, .armed = true, .t_idle_ns = 1 << 40 };
        return self;
    }

    fn deinit(self: *TransferFixture) void {
        self.table.deinit();
        self.gpa.destroy(self);
    }

    fn self_(ctx: *anyopaque) *TransferFixture {
        return @ptrCast(@alignCast(ctx));
    }
    fn drainPendingTimes(ctx: *anyopaque, _: u16) void {
        self_(ctx).drained_times += 1;
    }
    fn cycleOnce(ctx: *anyopaque, _: u16) Error!u64 {
        const s = self_(ctx);
        s.cycles += 1;
        if (s.tail_moves) s.tail += 10;
        if (s.cycles_until_published > 0) s.cycles_until_published -= 1;
        return s.tail;
    }
    fn reusableUpto(ctx: *anyopaque, _: u16) u64 {
        return self_(ctx).tail;
    }
    fn rootPublished(ctx: *anyopaque, _: u16) bool {
        return self_(ctx).cycles_until_published == 0;
    }
    fn recallTokens(ctx: *anyopaque, _: u16) void {
        self_(ctx).recalled += 1;
    }
    fn words(_: *anyopaque, _: u16) SlotWords {
        return .{ .root = .{ .addr = 4096, .seq = 3 }, .cursor = 9, .extents = 1, .seq_floor = 77 };
    }
    fn tails(_: *anyopaque, gpa: Allocator, _: u16) Error![]cr.TailEntry {
        return gpa.dupe(cr.TailEntry, &.{.{ .leaf_addr = 4096, .tail = 100 }}) catch return error.OutOfMemory;
    }
    fn pageReleasing(ctx: *anyopaque, _: u16, g: u32, w: SlotWords) Error!void {
        const s = self_(ctx);
        s.paged += 1;
        s.last_g = g;
        s.last_words = w;
    }
    fn releaseDurably(ctx: *anyopaque, slot: u16, g: u32, w: SlotWords, t: []const cr.TailEntry) Error!void {
        const s = self_(ctx);
        s.released += 1;
        if (t.len != 1) return error.Corrupt;
        _ = s.table.release(slot, 1, g, w, 0) catch return error.OutOfMemory;
    }
    fn pageDrop(ctx: *anyopaque, _: u16) Error!void {
        self_(ctx).dropped += 1;
    }
    fn steps(self: *TransferFixture) Steps {
        return .{ .ctx = self, .drainPendingTimes = drainPendingTimes, .cycleOnce = cycleOnce, .reusableUpto = reusableUpto, .rootPublished = rootPublished, .recallTokens = recallTokens, .words = words, .tails = tails, .pageReleasing = pageReleasing, .releaseDurably = releaseDurably, .pageDrop = pageDrop, .seam = &self.seam };
    }
};

test "flush-then-transfer: the steps run in order (times drained, gate, drain, cycles until the tail passes the frontier and the root is published, page Releasing, tree 0, page drop); the phases are exact-sum; a slot not the holder's is Busy; a moving tail at the bound defers (door reopened), a stuck one is Corrupt; the kill seams stop after the page / after tree 0" {
    const gpa = testing.allocator;
    const f = try TransferFixture.init(gpa);
    defer f.deinit();
    _ = try f.table.acquire(3, 1, false);
    f.table.noteFrontier(3, 25);
    f.cycles_until_published = 1;
    const ph = try transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 3);
    try testing.expectEqual(@as(u32, 3), f.cycles);
    try testing.expectEqual(@as(u32, 1), f.drained_times);
    try testing.expectEqual(@as(u32, 1), f.paged);
    try testing.expectEqual(@as(u32, 1), f.released);
    try testing.expectEqual(@as(u32, 1), f.dropped);
    try testing.expectEqual(@as(u32, 1), f.recalled);
    try testing.expectEqual(@as(u32, 1), f.last_g);
    try testing.expectEqual(@as(u64, 77), f.last_words.seq_floor);
    try testing.expectEqual(lt.State.unleased, f.table.view(3).state);
    try testing.expectEqual(ph.total(), ph.flush_ns + ph.page_ns + ph.tree0_ns + ph.grant_ns);
    const total = stats.slot_handover_phase_ns[@backingInt(Phase.total)].snapshot();
    for (stats.slot_handover_phase_ns[0..Phase.legs]) |*h| try testing.expectEqual(total.count, h.snapshot().count);
    // Not the holder's.
    try testing.expectError(error.Busy, transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 3));
    // A moving tail that never catches the frontier: deferred at the
    // bound, the door reopened.
    _ = try f.table.acquire(4, 1, false);
    f.table.noteFrontier(4, std.math.maxInt(u64) / 2);
    const d0 = stats.slot_handover_deferrals.load();
    try testing.expectError(error.HandoverDeferred, transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 4));
    try testing.expectEqual(d0 + 1, stats.slot_handover_deferrals.load());
    try testing.expect(f.table.view(4).leasedBy(1) and f.table.view(4).state == .leased);
    // A stuck tail is Corrupt.
    f.tail_moves = false;
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 4));
    try log.testing.expectAllFired();
    f.tail_moves = true;
    f.table.resetFrontier(4, 0);
    // The kill seams: after the page the lease stands (row 5's state);
    // after tree 0 the release landed, the page drop did not run.
    f.seam.hold = .after_page;
    try testing.expectError(error.HeldAtSeam, transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 4));
    try testing.expectEqual(lt.State.leased, f.table.view(4).state);
    try testing.expectEqual(@as(u32, 2), f.paged);
    f.seam.hold = .after_tree0;
    const dropped0 = f.dropped;
    try testing.expectError(error.HeldAtSeam, transfer(gpa, &f.door, f.table, f.steps(), testing.io, 1, 4));
    try testing.expectEqual(lt.State.unleased, f.table.view(4).state);
    try testing.expectEqual(dropped0, f.dropped);
}
