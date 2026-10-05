//! The checkpoint machinery's PURE half (charter §5.3 "Checkpoint", "Space",
//! "fsync", §5.5 "Flush ceiling (KD-SYM-10)"; survey
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.10, §3.9, §4.1, §5.6;
//! `docs/porting/kv_journal_ledger.md` §5): every derivation with its tie
//! test, the cadence DECISION as a pure function of what the tick observed,
//! the drain budget, the term horizon, the flush-pass sample, the per-cycle
//! TAPE (exact-sum phases) and the flush-ceiling audit.
//!
//! The cycle protocol, the DUR-3 barrier tickets and the task shape live in
//! `checkpoint_cycle.zig` (re-exported here as `meta.checkpoint.*`).
//!
//! ## The cadence (§5.3)
//!
//! A cycle is due at most `CHECKPOINT_MAX_AGE_MS` after the last one, or when
//! the journal distance `head − reusable_upto` exceeds half the ring, or when
//! more than `max(4096, budget/32 ÷ node_size)` nodes are dirty, or when a
//! committer PARKED at ring admission kicked the task with the park mark
//! (which makes the cycle due with an immediate barrier: the parked
//! committer waits on `reusable_upto`, which only the barrier advances — and
//! the pressure law `distance > L/2` is unreachable on a ring whose reserve is
//! half of it), or at shutdown. The deferred-mode `needs_flush` (a commit
//! group's trailing coalesced barrier) rides the same tick as its own term.
//!
//! ## The anticipating trigger (KD-SYM-10)
//!
//! The LANDING ceiling `max_age + 2 × tick` (1,100 ms shipped) is a promise
//! about when a commit's checkpoint LANDS; the tick wait and one period of
//! pre-decision work are its two ticks. The cycle's own TERM (its wall from
//! the decision to barrier #1) sits outside them, so a forest volume fires
//! its cadence at `max_age − max(horizon term, live projection)`: the horizon
//! is the MAXIMUM term over the last 64 cycles (a ceiling is a bound, so the
//! term it anticipates must be one — a mean under-prices every above-mean
//! cycle), the projection what the next cycle CARRIES (dirty nodes × the
//! per-node unit + promised images × the per-image unit + the covering
//! barriers × the barrier unit, every unit a horizon maximum). A flat volume
//! keeps the shipped law verbatim (`flatAgeDue`: `elapsed ≥ max_age` since
//! the last cycle's END), which is this trigger at term 0.

const std = @import("std");
const counters = @import("../core/counters.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const ledger = @import("ledger.zig");
const alloc_core = @import("alloc_core.zig");

pub const cycle = @import("checkpoint_cycle.zig");
pub const Checkpointer = cycle.Checkpointer;
pub const CheckpointTask = cycle.CheckpointTask;
pub const FlushSink = cycle.FlushSink;
pub const BarrierSink = cycle.BarrierSink;
pub const LedgerSink = cycle.LedgerSink;
pub const PageSink = cycle.PageSink;
pub const RecordSource = cycle.RecordSource;
pub const SmoSeam = cycle.SmoSeam;
pub const CacheFlusher = cycle.CacheFlusher;

// ---------------------------------------------------------------------------
// Constants, each with its WHY.
// ---------------------------------------------------------------------------

/// Charter §5.3 "every ≤ 1 s": the cadence ceiling a cycle runs at even when
/// no other trigger fires; also the READER's staleness term (a coherent
/// reader cannot see records faster than the writer mints them). The knob
/// `SQUEEZEFS_CHECKPOINT_MAX_AGE_MS` overrides it verbatim.
pub const CHECKPOINT_MAX_AGE_MS: u64 = ledger.CHECKPOINT_MAX_AGE_MS;
comptime {
    std.debug.assert(CHECKPOINT_MAX_AGE_MS == 1000);
}

/// The shipped dirty-node cap: the derived default's FLOOR. Never regress
/// below shipped — a lower cap checkpoints more often than any box ever ran
/// (pure overhead, no RAM won).
pub const CHECKPOINT_MAX_DIRTY_NODES_FLOOR: u64 = 4096;

/// The budget share dirty nodes may pin: 1/32 of the R5 budget — the mount-
/// replay working set is the same population, so the cap scales with the
/// resolved budget instead of a 512 MiB-era constant.
pub const DIRTY_NODES_BUDGET_DIVISOR: u64 = 32;

/// Strict mode (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`) has no flusher period
/// to ride; the task still needs a tick for ring reclamation and SMO service.
/// 100 ms = a tenth of the max age: ten decisions per cadence ceiling.
pub const STRICT_TICK_PERIOD_MS: u64 = 100;

/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` default (the lane's; one source).
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 50;

/// §4.7 wedged-tail bound: consecutive barriered cycles with retirements
/// parked, none released and no tail advance before the volume fails loud.
/// A resolvable pinned floor converges in 2 cycles (the first discharges
/// dying floors, the second's tail covers the parked frees); 8 leaves only a
/// tail nothing can discharge. ONE definition: the allocator core's.
pub const PENDING_FREE_FORCE_CYCLES: u64 = alloc_core.pending_free_force_cycles;
comptime {
    std.debug.assert(PENDING_FREE_FORCE_CYCLES == 8);
}

/// The ONE bound every "cycle until the tail covers X" loop runs to: eight
/// clause-b rounds. A tail that does not move fails loud INSIDE the eighth
/// cycle by the audit itself, so a loop reaching this bound has a MOVING tail
/// that still has not covered its target — a schedule, never a wedge.
pub const COVER_CYCLES_MAX: u32 = 8 * @as(u32, @intCast(PENDING_FREE_FORCE_CYCLES));

/// The shutdown fixpoint's bound: TWO convergent terms (the tail, then the
/// bitmap pages the covering release dirtied), each bounded by the audit.
pub const FIXPOINT_COVER_CYCLES_MAX: u64 = 2 * PENDING_FREE_FORCE_CYCLES;

/// The hole discipline's bound (`checkpointPast`): one clause-b round — the
/// dying-floor clamp can hold the first cycle's tail below the hole's end;
/// the second clears it; a tail stuck past eight is a defect.
pub const HOLE_COVER_CYCLES_MAX: u32 = @intCast(PENDING_FREE_FORCE_CYCLES);

/// The term horizon in CYCLES: `COVER_CYCLES_MAX`, so a burst is remembered
/// for as long as any cover loop would wait on it. A count, never a
/// duration (at a trigger of 0 the horizon passes in seconds).
pub const TERM_HORIZON_CYCLES: usize = COVER_CYCLES_MAX;

// ---------------------------------------------------------------------------
// Derivations.
// ---------------------------------------------------------------------------

/// `explicit` verbatim, else `max(4096, budget/32 ÷ node_size)`. A zero
/// `node_size` is defensively floored (never a division by zero on a
/// malformed superblock read).
pub fn resolveMaxDirtyNodes(budget_bytes: u64, node_size: u64, explicit: ?u64) u64 {
    if (explicit) |n| return n;
    if (node_size == 0) return CHECKPOINT_MAX_DIRTY_NODES_FLOOR;
    return @max(budget_bytes / DIRTY_NODES_BUDGET_DIVISOR / node_size, CHECKPOINT_MAX_DIRTY_NODES_FLOOR);
}

/// The registry face of `resolveMaxDirtyNodes`.
pub fn maxDirtyNodesFromEnv(env: *const std.process.Environ.Map, budget_bytes: u64, node_size: u64) u64 {
    return resolveMaxDirtyNodes(budget_bytes, node_size, knobs.optIntKnob(u64, env, "SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES"));
}

/// `SQUEEZEFS_CHECKPOINT_MAX_AGE_MS` (1..=600000, default 1000).
pub fn maxAgeMsFromEnv(env: *const std.process.Environ.Map) u64 {
    return knobs.intKnob(u64, env, "SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", CHECKPOINT_MAX_AGE_MS);
}

/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (0..=600000, default 50; 0 = strict) —
/// the tick period's input; the same row the journal lane reads.
pub fn flushIntervalMsFromEnv(env: *const std.process.Environ.Map) u64 {
    return knobs.intKnob(u64, env, "SQUEEZEFS_META_FLUSH_INTERVAL_MS", DEFAULT_FLUSH_INTERVAL_MS);
}

/// The task's tick period: the flush interval verbatim; strict (`0`) reads
/// as the task's own 100 ms tick. The ONE derivation the task's spawn and
/// the reader's poll cadence both ride.
pub fn checkpointTickPeriodMs(flush_interval_ms: u64) u64 {
    return if (flush_interval_ms == 0) STRICT_TICK_PERIOD_MS else flush_interval_ms;
}

/// The §4.6a merge sweep's per-cycle work budget = one tick period: the SAME
/// law the threshold drain rides (one period per pass, leftovers re-arm).
/// Never a constant of its own.
pub fn mergeSweepBudgetMs(flush_interval_ms: u64) u64 {
    return checkpointTickPeriodMs(flush_interval_ms);
}

/// The writer's checkpoint LANDING ceiling for a commit: `max_age + 2 ×
/// tick` — the decision is taken once per period (one tick) behind the
/// bounded pre-decision drain (one more). 1,100 ms at the shipped 50 ms
/// flush; on a slow-flush venue the tick IS the landing term.
pub fn checkpointLandingCeilingMs(max_age_ms: u64, flush_interval_ms: u64) u64 {
    return max_age_ms + 2 * checkpointTickPeriodMs(flush_interval_ms);
}

/// The cadence TRIGGER for a MAX AGE: `max_age − anticipated_term`,
/// saturating. The input is the age the tick fires AT, never the landing
/// ceiling (fed the ceiling, every cycle would land `2 × tick` past the
/// promise). A term at or past the max age makes a cycle due every tick —
/// the honest response to a device that cannot land the promise.
pub fn checkpointTriggerMs(max_age_ms: u64, anticipated_term_ms: u64) u64 {
    return max_age_ms -| anticipated_term_ms;
}

/// One cycle's landing TERM: its wall from the age decision to barrier #1's
/// completion plus the decision's lateness past the trigger BEYOND one tick
/// (one tick of lateness is the wake quantization, the ceiling's first
/// priced tick; the excess is the tick's own device work ahead of its
/// decision). `late` is CAPPED at `late_cap_ns` — one landing ceiling: a
/// decision later than the whole ceiling is a stall the audit counts on the
/// cycle it happens, never a term the next 64 cycles anticipate.
pub fn checkpointCycleTermNs(prebarrier_wall_ns: u64, late_ns: u64, tick_ns: u64, late_cap_ns: u64) u64 {
    return prebarrier_wall_ns +| (@min(late_ns, late_cap_ns) -| tick_ns);
}

/// One flush pass's unit for a class: `wall / count` when the pass ran the
/// class, null when it did not (a pass without the class measures nothing;
/// the unit in force keeps what the passes that ran it measured). The mean
/// per ITEM over the count it ran — a one-image pass at 30 ms IS 30 ms per
/// image (a grain floor read a quarter of the cost and under-priced a wave).
pub fn flushUnitNs(wall_ns: u64, count: u64) ?u64 {
    return if (count > 0) wall_ns / count else null;
}

/// The LIVE projection of the next cycle's flush wall: dirty nodes × the
/// per-node append unit + promised images × the per-image SMO unit. The
/// image unit is FLOORED at the node unit: an image is one node write at
/// least, so a promised image no SMO pass has priced yet is priced as the
/// append it cannot cost less than, never at 0. Saturating.
pub fn projectedFlushWallNs(dirty_nodes: u64, node_unit_ns: u64, promised_extents: u64, image_unit_ns: u64) u64 {
    return (dirty_nodes *| node_unit_ns) +| (promised_extents *| @max(image_unit_ns, node_unit_ns));
}

/// The covering barriers a due cycle pays between its decision and its
/// landing: barrier #1 always; on a DEFERRED-mode volume the tick's
/// deferred-flush barrier too (a strict volume's commits barrier themselves).
pub fn coveringBarriers(strict: bool) u64 {
    return if (strict) 1 else 2;
}

/// The LIVE projection of the next cycle's wall from decision to landing:
/// the flush projection plus the covering barriers at the measured barrier
/// unit (0 before the first barrier — the shipped posture). Saturating.
pub fn projectedCycleWallNs(flush_ns: u64, barriers: u64, barrier_unit_ns: u64) u64 {
    return flush_ns +| (barriers *| barrier_unit_ns);
}

/// The term the forest cadence anticipates: the LARGER of what the horizon
/// remembers and what the pending work projects.
pub fn anticipatedTermMs(horizon_term_ms: u64, projected_ms: u64) u64 {
    return @max(horizon_term_ms, projected_ms);
}

/// The FLAT volume's age verdict — the shipped law verbatim: due when the max
/// age has elapsed since the last cycle's END, and nothing else.
pub fn flatAgeDue(elapsed_ms: u64, max_age_ms: u64) bool {
    return elapsed_ms >= max_age_ms;
}

pub const ringUnderPressure = ledger.ringUnderPressure;

// ---------------------------------------------------------------------------
// The flush-ceiling audit (KD-SYM-10): pure.
// ---------------------------------------------------------------------------

/// `appender_flush_ceiling_overruns`: a dirty leaf reached its covering
/// barrier older than the landing ceiling with every structural hold's
/// capped overlap excluded. Must stay 0.
pub var appender_flush_ceiling_overruns: counters.Tripwire = .init("appender_flush_ceiling_overruns");
/// Leaves whose age past the ceiling was excused by a structural hold.
pub var appender_flush_ceiling_extensions: counters.ShardedCounter = .{};

/// One region's oldest dirty leaf as the flush pass began: dirtied at
/// `since_ns` (the record that dirtied it), with the SMO-mutex structural
/// holds it aged under already capped (recovery class at the recovery
/// bound, service class at one ceiling) in `excused_ns`.
pub const DirtyLeafAge = struct {
    region: u32,
    since_ns: u64,
    excused_ns: u64 = 0,

    /// Fold a younger/older leaf of the same region: the OLDEST wins.
    pub fn foldOldest(self: *DirtyLeafAge, since_ns: u64, excused_ns: u64) void {
        if (since_ns < self.since_ns) {
            self.since_ns = since_ns;
            self.excused_ns = excused_ns;
        }
    }
};

pub const CeilingVerdict = enum {
    /// Inside the ceiling.
    inside,
    /// Past the ceiling only by an excused structural hold: counted as an
    /// extension, never a trip.
    extended,
    /// Past the ceiling with the excused overlap removed: the tripwire.
    overrun,
};

/// The audit of ONE leaf at the covering barrier `now_ns`.
pub fn judgeFlushCeiling(leaf: DirtyLeafAge, now_ns: u64, ceiling_ns: u64) CeilingVerdict {
    const age_ns = now_ns -| leaf.since_ns;
    if (age_ns <= ceiling_ns) return .inside;
    if (age_ns -| leaf.excused_ns > ceiling_ns) return .overrun;
    return .extended;
}

pub const CeilingAudit = struct { overruns: u64 = 0, extensions: u64 = 0 };

/// Judge every region's oldest leaf; fire the tripwire once per overrun
/// (the WARN line carries the cycle's tape on the caller's side).
pub fn auditFlushCeiling(leaves: []const DirtyLeafAge, now_ns: u64, ceiling_ns: u64) CeilingAudit {
    var out: CeilingAudit = .{};
    for (leaves) |leaf| switch (judgeFlushCeiling(leaf, now_ns, ceiling_ns)) {
        .inside => {},
        .extended => out.extensions += 1,
        .overrun => {
            out.overruns += 1;
            appender_flush_ceiling_overruns.fire("a dirty leaf reached its covering barrier past the landing ceiling with every structural hold's capped overlap excluded");
        },
    };
    if (out.extensions > 0) appender_flush_ceiling_extensions.add(out.extensions);
    return out;
}

// ---------------------------------------------------------------------------
// The decision.
// ---------------------------------------------------------------------------

/// What the tick observed, read once; the decision is a pure function of it.
pub const Observations = struct {
    now_ns: u64,
    /// The age law's reference: the last cycle's END on a flat volume, its
    /// COLLECTION on a forest (every leaf dirtied after the collection is
    /// the next cycle's — the interval the landing ceiling bounds).
    last_cycle_at_ns: u64,
    /// `head − reusable_upto`.
    journal_distance: u64,
    ring_len: u64,
    dirty_nodes: u64,
    max_dirty: u64,
    /// A committer parked at ring admission kicked the task.
    park_mark: bool,
    /// The deferred-mode `needs_flush`: a commit group asked for the trailing
    /// coalesced barrier.
    fsync_pending: bool,
    /// `meta_kv_heap_full`: the merge sweep arm is due at the cycle's end.
    heap_full: bool,
    /// `max(horizon term, live projection)`, ms — the forest's anticipated
    /// term; 0 on a flat volume (its trigger is the max age verbatim).
    term_projection_ms: u64,
    max_age_ms: u64 = CHECKPOINT_MAX_AGE_MS,
    /// The shutdown tick.
    final_cycle: bool = false,
    /// The tick's wait for the SMO mutex, left out of the lateness (a wait
    /// behind another holder is that holder's hold, never a cadence term).
    mutex_wait_ns: u64 = 0,
};

/// What made the cycle due, highest priority first.
pub const Reason = enum { none, shutdown, ring_pressure, park_kick, dirty_cap, age };

pub const Decision = struct {
    due: bool,
    reason: Reason,
    /// Barrier the record inside this cycle: a parker waits on `reusable_upto`.
    immediate_barrier: bool,
    /// Something to cover: dirty nodes, an uncovered ring, or the final cycle.
    covers_something: bool,
    /// The deferred-mode barrier the tick runs whether or not a cycle does.
    deferred_barrier: bool,
    /// The heap-full merge sweep runs at the end of the cycle.
    sweep_due: bool,
    /// The trigger in force, ms.
    trigger_ms: u64,
    /// Due by age: the decision's lateness past the trigger less the mutex
    /// wait (the cycle it runs folds it into its term). Null otherwise.
    age_late_ns: ?u64,

    pub fn runsACycle(self: Decision) bool {
        return self.due and self.covers_something;
    }
};

/// The cadence decision (§5.3): `due = final ∨ ring pressure ∨ park mark ∨
/// dirty > cap ∨ age`, in that priority; the barrier is immediate for the
/// first three (a parker or shutdown waits on reclamation); a due tick with
/// nothing to cover runs no cycle.
pub fn decideCheckpoint(obs: Observations) Decision {
    const ring_pressure = obs.journal_distance > obs.ring_len / 2;
    const trigger_ms = checkpointTriggerMs(obs.max_age_ms, obs.term_projection_ms);
    const since_ns = obs.now_ns -| obs.last_cycle_at_ns;
    const age_late_ns: ?u64 = if (since_ns / std.time.ns_per_ms >= trigger_ms)
        (since_ns -| trigger_ms *| std.time.ns_per_ms) -| obs.mutex_wait_ns
    else
        null;
    const reason: Reason = if (obs.final_cycle)
        .shutdown
    else if (ring_pressure)
        .ring_pressure
    else if (obs.park_mark)
        .park_kick
    else if (obs.dirty_nodes > obs.max_dirty)
        .dirty_cap
    else if (age_late_ns != null)
        .age
    else
        .none;
    return .{
        .due = reason != .none,
        .reason = reason,
        .immediate_barrier = obs.final_cycle or ring_pressure or obs.park_mark,
        .covers_something = obs.final_cycle or obs.dirty_nodes > 0 or obs.journal_distance > 0,
        .deferred_barrier = obs.fsync_pending,
        .sweep_due = obs.heap_full,
        .trigger_ms = trigger_ms,
        .age_late_ns = age_late_ns,
    };
}

// ---------------------------------------------------------------------------
// The drain budget, the term horizon, the flush-pass sample, the tape.
// ---------------------------------------------------------------------------

/// One threshold-drain pass's budget: the FIRST item of the PASS is admitted
/// unconditionally (progress) and every later one only while the deadline
/// stands, whatever tree it belongs to — so a pass is bounded by `period +
/// ONE item's service time` (a per-tree free item put a forest's tick
/// `trees × one item` late). `unbounded` is the shutdown tick's form.
pub const DrainBudget = struct {
    deadline_ns: ?u64,
    progressed: bool = false,

    pub const unbounded: DrainBudget = .{ .deadline_ns = null };

    pub fn bounded(deadline_ns: u64) DrainBudget {
        return .{ .deadline_ns = deadline_ns };
    }

    /// Whether the next item may run at `now_ns`. Marks the pass progressed.
    pub fn admits(self: *DrainBudget, now_ns: u64) bool {
        const admit = !self.progressed or self.deadline_ns == null or self.deadline_ns.? > now_ns;
        self.progressed = true;
        return admit;
    }
};

/// The last `TERM_HORIZON_CYCLES` samples; the anticipated value is their
/// MAXIMUM (0 before the first sample). A fixed ring of u64s, no allocation.
pub const CycleTermWindow = struct {
    samples: [TERM_HORIZON_CYCLES]u64 = @splat(0),
    next: usize = 0,

    pub fn push(self: *CycleTermWindow, sample_ns: u64) void {
        self.samples[self.next] = sample_ns;
        self.next = (self.next + 1) % TERM_HORIZON_CYCLES;
    }

    pub fn anticipatedNs(self: *const CycleTermWindow) u64 {
        var max: u64 = 0;
        for (self.samples) |s| max = @max(max, s);
        return max;
    }
};

/// One flush pass's measured work split by its two cost classes: nodes whose
/// flush wrote NO fresh image (an append — `node_ns` over `nodes`) and nodes
/// whose flush ran an SMO (`image_ns` over the fresh IMAGES written — the
/// unit the promise ledger counts in).
pub const FlushPassSample = struct {
    node_ns: u64 = 0,
    nodes: u64 = 0,
    image_ns: u64 = 0,
    images: u64 = 0,
    smo_nodes: u64 = 0,

    pub fn fold(self: *FlushPassSample, other: FlushPassSample) void {
        self.node_ns +|= other.node_ns;
        self.nodes +|= other.nodes;
        self.image_ns +|= other.image_ns;
        self.images +|= other.images;
        self.smo_nodes +|= other.smo_nodes;
    }

    /// One node's flush: `images` fresh images in `wall_ns` — SMO work when
    /// any, an append otherwise.
    pub fn note(self: *FlushPassSample, wall_ns: u64, images: u64) void {
        if (images > 0) {
            self.image_ns +|= wall_ns;
            self.images +|= images;
            self.smo_nodes += 1;
        } else {
            self.node_ns +|= wall_ns;
            self.nodes += 1;
        }
    }
};

/// The per-cycle TAPE: what the last cycle DECIDED with and PAID, word by
/// word, so an overrun attributes itself from the WARN line it rides and
/// from `.stats meta_kv_checkpoint_last_cycle`. The paid phases are
/// EXACT-SUM (`fsync_phase_ns` style): consecutive monotonic-clock
/// differences from the cycle's start to its end, so `Σ phases ≡ total_ns`.
pub const CycleTape = struct {
    /// The checkpoint seq the cycle landed as.
    seq: u64 = 0,
    // ---- the decision's words (0 for a cycle no age decision fired) ----
    late_ms: u64 = 0,
    dirty_at_decision: u64 = 0,
    promised_at_decision: u64 = 0,
    node_unit_us: u64 = 0,
    image_unit_us: u64 = 0,
    projected_ms: u64 = 0,
    anticipated_ms: u64 = 0,
    trigger_ms: u64 = 0,
    // ---- paid: exact-sum phases, ns ----
    /// Choose the dirty set (collect + order).
    collect_ns: u64 = 0,
    /// The flush pass.
    flush_ns: u64 = 0,
    /// The allocation bitmap pages.
    pages_ns: u64 = 0,
    /// Barrier #1.
    barrier_ns: u64 = 0,
    /// The tail rule.
    tail_ns: u64 = 0,
    /// The ledger slot write.
    ledger_ns: u64 = 0,
    /// Barrier #2 + the watermark advances + the audit + the sweep.
    reclaim_ns: u64 = 0,
    total_ns: u64 = 0,
    // ---- the landing words ----
    /// The decision → the cycle's first instruction.
    pre_start_ns: u64 = 0,
    /// The decision (else the start) → barrier #1 landed.
    landing_ns: u64 = 0,
    /// The term folded into the horizon.
    term_ns: u64 = 0,
    // ---- the pass's classes ----
    dirty_collected: u64 = 0,
    nodes_appended: u64 = 0,
    smo_nodes: u64 = 0,
    images: u64 = 0,
    append_ns: u64 = 0,
    image_ns: u64 = 0,

    pub fn phasesSum(self: CycleTape) u64 {
        return self.collect_ns + self.flush_ns + self.pages_ns + self.barrier_ns + self.tail_ns + self.ledger_ns + self.reclaim_ns;
    }

    /// The WARN line's form: the decision's words, then what the cycle paid.
    pub fn format(self: CycleTape, w: *std.Io.Writer) std.Io.Writer.Error!void {
        const ms = struct {
            fn f(ns: u64) u64 {
                return ns / std.time.ns_per_ms;
            }
        }.f;
        try w.print("cycle {d} decided at trigger {d} ms (anticipated {d} ms, projected {d} ms over {d} dirty nodes × {d} µs + {d} promised images × {d} µs) {d} ms late; paid pre-start {d} + collect {d} + flush {d} ({d} found: {d} appended in {d} ms, {d} SMO'd writing {d} images in {d} ms) + pages {d} + barrier {d} = landing {d} ms, term {d} ms; tail {d} + ledger {d} + reclaim {d}, total {d} ms", .{
            self.seq,            self.trigger_ms,       self.anticipated_ms, self.projected_ms,   self.dirty_at_decision, self.node_unit_us,   self.promised_at_decision, self.image_unit_us,
            self.late_ms,        ms(self.pre_start_ns), ms(self.collect_ns), ms(self.flush_ns),   self.dirty_collected,   self.nodes_appended, ms(self.append_ns),        self.smo_nodes,
            self.images,         ms(self.image_ns),     ms(self.pages_ns),   ms(self.barrier_ns), ms(self.landing_ns),    ms(self.term_ns),    ms(self.tail_ns),          ms(self.ledger_ns),
            ms(self.reclaim_ns), ms(self.total_ns),
        });
    }
};

// ---------------------------------------------------------------------------
// Observability (survey §5.6 names). The per-volume faces — `reusable_upto`,
// `meta_kv_dirty_nodes`, the term machinery's gauges and the tape — live on
// the `Checkpointer`.
// ---------------------------------------------------------------------------

/// `meta_kv_checkpoint_phase_ns`: the cycle's exact-sum phases.
pub const PhaseHistograms = struct {
    collect: counters.ShardedLatencyHistogram = .{},
    flush: counters.ShardedLatencyHistogram = .{},
    pages: counters.ShardedLatencyHistogram = .{},
    barrier: counters.ShardedLatencyHistogram = .{},
    tail: counters.ShardedLatencyHistogram = .{},
    ledger: counters.ShardedLatencyHistogram = .{},
    reclaim: counters.ShardedLatencyHistogram = .{},
    total: counters.ShardedLatencyHistogram = .{},

    pub fn record(self: *PhaseHistograms, tape: CycleTape) void {
        self.collect.record(tape.collect_ns);
        self.flush.record(tape.flush_ns);
        self.pages.record(tape.pages_ns);
        self.barrier.record(tape.barrier_ns);
        self.tail.record(tape.tail_ns);
        self.ledger.record(tape.ledger_ns);
        self.reclaim.record(tape.reclaim_ns);
        self.total.record(tape.total_ns);
    }
};

pub const stats = struct {
    /// Ledger records written by cycles.
    pub var meta_kv_checkpoints: counters.ShardedCounter = .{};
    /// Every ledger slot write (cycles + DENSE restatements).
    pub var meta_kv_ledger_writes: counters.ShardedCounter = .{};
    pub var meta_kv_checkpoint_phase_ns: PhaseHistograms = .{};
    /// Deferred-mode tick barriers.
    pub var meta_flush_deferred: counters.ShardedCounter = .{};
    /// Every `syncDevice`.
    pub var meta_device_syncs: counters.ShardedCounter = .{};
    /// One barrier's wall.
    pub var meta_barrier: counters.ShardedLatencyHistogram = .{};
    /// Cycles that ran with the park mark (`ring_pressure` class).
    pub var meta_kv_checkpoint_park_cycles: counters.ShardedCounter = .{};
    /// Threshold-maintenance items drained outside a cycle.
    pub var meta_kv_maintenance_items: counters.ShardedCounter = .{};
    /// Bounded heap-full sweep calls from the cycle's arm.
    pub var meta_kv_merge_sweeps: counters.ShardedCounter = .{};
    /// The wedge rung (`pending_free_stalled_cycles`) as the last barriered cycle left it.
    pub var pending_free_stalled_cycles: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// Tests: ties for every derivation, the decision matrix, the pure pieces.
// ---------------------------------------------------------------------------

const testing = std.testing;

test {
    _ = cycle;
}

const ms_ns: u64 = std.time.ns_per_ms;

test "dirty-node cap tie: max(4096, budget/32 ÷ node_size); explicit verbatim; node_size 0 floors; the registry rows" {
    const GiB: u64 = 1 << 30;
    const node: u64 = 256 * 1024;
    // Field: 176 GiB / 32 / 256 KiB = 22528 nodes (5.5 GiB dirty ceiling).
    try testing.expectEqual(@as(u64, 22528), resolveMaxDirtyNodes(176 * GiB, node, null));
    // Floor shape: 2.8 GiB / 32 = 89.6 MiB ⇒ 358 nodes < 4096 ⇒ floor.
    const floor_budget: u64 = 2_800 * (1 << 20);
    try testing.expectEqual(@as(u64, 4096), resolveMaxDirtyNodes(floor_budget, node, null));
    try testing.expectEqual(@as(u64, 5632), resolveMaxDirtyNodes(176 * GiB, 1024 * 1024, null));
    try testing.expectEqual(@as(u64, 4096), resolveMaxDirtyNodes(176 * GiB, node, 4096));
    try testing.expectEqual(@as(u64, 64), resolveMaxDirtyNodes(176 * GiB, node, 64));
    try testing.expectEqual(@as(u64, 4096), resolveMaxDirtyNodes(floor_budget, 0, null));
    try testing.expectEqual(@max(176 * GiB / 32 / node, CHECKPOINT_MAX_DIRTY_NODES_FLOOR), resolveMaxDirtyNodes(176 * GiB, node, null));
    const row = knobs.lookup("SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES").?;
    try testing.expectEqual(@as(i128, 1), row.kind.int.lo);
    try testing.expectEqual(@as(i128, 1 << 32), row.kind.int.hi);
    try testing.expectEqual(knobs.Class.derived_override, row.class);
    const age = knobs.lookup("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS").?;
    try testing.expectEqual(@as(i128, 1), age.kind.int.lo);
    try testing.expectEqual(knobs.MS_MAX, age.kind.int.hi);
    try testing.expectEqual(knobs.Class.tuning, age.class);
    try testing.expectEqualStrings("1000", age.default);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u64, 22528), maxDirtyNodesFromEnv(&env, 176 * GiB, node));
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS, maxAgeMsFromEnv(&env));
    try testing.expectEqual(DEFAULT_FLUSH_INTERVAL_MS, flushIntervalMsFromEnv(&env));
    try env.put("SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES", "64");
    try env.put("SQUEEZEFS_CHECKPOINT_MAX_AGE_MS", "500");
    try env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    try testing.expectEqual(@as(u64, 64), maxDirtyNodesFromEnv(&env, 176 * GiB, node));
    try testing.expectEqual(@as(u64, 500), maxAgeMsFromEnv(&env));
    try testing.expectEqual(@as(u64, 0), flushIntervalMsFromEnv(&env));
}

test "cadence ties: tick = flush (0 ⇒ 100); sweep budget = tick; landing ceiling = max_age + 2 × tick (1,100 at 50; 1,200 strict); trigger = max_age − term saturating; trigger + 2 × tick + term ≡ ceiling" {
    for ([_]u64{ 0, 1, 50, 250, 1000, 5000 }) |flush| {
        const tick = checkpointTickPeriodMs(flush);
        try testing.expectEqual(if (flush == 0) STRICT_TICK_PERIOD_MS else flush, tick);
        try testing.expectEqual(tick, mergeSweepBudgetMs(flush));
        try testing.expectEqual(CHECKPOINT_MAX_AGE_MS + 2 * tick, checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, flush));
        for ([_]u64{ 0, 9, 150, 600 }) |term| {
            try testing.expectEqual(checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, flush) - term, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, term) + 2 * tick);
            // Fed the landing ceiling the trigger would eat the 2-tick margin.
            try testing.expectEqual(checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, flush) - term + 2 * tick, checkpointTriggerMs(checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, flush), term) + 2 * tick);
        }
    }
    try testing.expectEqual(@as(u64, 1_200), checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, 0));
    try testing.expectEqual(@as(u64, 1_100), checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, 50));
    try testing.expectEqual(@as(u64, 11_000), checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, 5_000));
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, 0));
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS - 150, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, 150));
    try testing.expectEqual(@as(u64, 0), checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, CHECKPOINT_MAX_AGE_MS));
    try testing.expectEqual(@as(u64, 0), checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, 5_000));
    try testing.expectEqual(@as(u64, 380), checkpointTriggerMs(500, 120));
    // The flat law is the trigger at term 0.
    try testing.expect(!flatAgeDue(0, CHECKPOINT_MAX_AGE_MS));
    try testing.expect(!flatAgeDue(CHECKPOINT_MAX_AGE_MS - 1, CHECKPOINT_MAX_AGE_MS));
    try testing.expect(flatAgeDue(CHECKPOINT_MAX_AGE_MS, CHECKPOINT_MAX_AGE_MS));
    try testing.expect(flatAgeDue(CHECKPOINT_MAX_AGE_MS * 7, CHECKPOINT_MAX_AGE_MS));
    try testing.expect(flatAgeDue(500, 500) and !flatAgeDue(499, 500));
    for ([_]u64{ 0, 499, 500, 999, 1000, 7000 }) |elapsed| {
        try testing.expectEqual(flatAgeDue(elapsed, CHECKPOINT_MAX_AGE_MS), elapsed >= checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, 0));
    }
    // The cover bounds derive from the wedge rung.
    try testing.expectEqual(@as(u32, 64), COVER_CYCLES_MAX);
    try testing.expectEqual(@as(u64, 16), FIXPOINT_COVER_CYCLES_MAX);
    try testing.expectEqual(@as(u32, 8), HOLE_COVER_CYCLES_MAX);
    try testing.expectEqual(@as(usize, 64), TERM_HORIZON_CYCLES);
}

test "cycle term tie: prebarrier_wall + (min(late, cap) − tick)⁺; the cap is one landing ceiling; saturating" {
    const cap = checkpointLandingCeilingMs(CHECKPOINT_MAX_AGE_MS, 50) * ms_ns;
    try testing.expectEqual(60 * ms_ns, checkpointCycleTermNs(60 * ms_ns, 0, 50 * ms_ns, cap));
    try testing.expectEqual(60 * ms_ns, checkpointCycleTermNs(60 * ms_ns, 30 * ms_ns, 50 * ms_ns, cap));
    try testing.expectEqual(60 * ms_ns, checkpointCycleTermNs(60 * ms_ns, 50 * ms_ns, 50 * ms_ns, cap));
    try testing.expectEqual(142 * ms_ns, checkpointCycleTermNs(60 * ms_ns, 132 * ms_ns, 50 * ms_ns, cap));
    try testing.expectEqual(82 * ms_ns, checkpointCycleTermNs(0, 132 * ms_ns, 50 * ms_ns, cap));
    try testing.expectEqual(60 * ms_ns + cap - 50 * ms_ns, checkpointCycleTermNs(60 * ms_ns, 4_000 * ms_ns, 50 * ms_ns, cap));
    try testing.expectEqual(std.math.maxInt(u64), checkpointCycleTermNs(std.math.maxInt(u64), 132 * ms_ns, 50 * ms_ns, cap));
}

test "flush units and the projection: wall/count per class, none without the class; the horizon MAXIMUM over 64 passes; image unit floored at the node unit; covering barriers 2 deferred / 1 strict; saturating" {
    try testing.expectEqual(@as(?u64, null), flushUnitNs(0, 0));
    try testing.expectEqual(@as(?u64, null), flushUnitNs(999 * ms_ns, 0));
    try testing.expectEqual(@as(?u64, 2 * ms_ns), flushUnitNs(80 * ms_ns, 40));
    try testing.expectEqual(@as(?u64, 30 * ms_ns), flushUnitNs(30 * ms_ns, 1));
    try testing.expectEqual(@as(?u64, 15 * ms_ns), flushUnitNs(30 * ms_ns, 2));
    try testing.expectEqual(@as(?u64, 10 * ms_ns), flushUnitNs(30 * ms_ns, 3));
    try testing.expectEqual(@as(?u64, 6 * ms_ns), flushUnitNs(30 * ms_ns, 5));
    // The box's arithmetic: a two-image pass at 3.97 ms each prices a 38-image wave at its wall.
    try testing.expectEqual(@as(u64, 150), 38 * flushUnitNs(2 * 3_970_000, 2).? / ms_ns);
    var w: CycleTermWindow = .{};
    try testing.expectEqual(@as(u64, 0), w.anticipatedNs());
    for ([_]?u64{ flushUnitNs(80 * ms_ns, 40), flushUnitNs(400 * ms_ns, 40), flushUnitNs(0, 0) }) |u| if (u) |unit| w.push(unit);
    try testing.expectEqual(10 * ms_ns, w.anticipatedNs());
    for (0..TERM_HORIZON_CYCLES - 1) |_| w.push(2 * ms_ns);
    try testing.expectEqual(10 * ms_ns, w.anticipatedNs());
    w.push(2 * ms_ns);
    try testing.expectEqual(2 * ms_ns, w.anticipatedNs());
    // The sample's split by class.
    var s: FlushPassSample = .{};
    s.note(300_000, 0);
    s.note(200_000, 0);
    s.note(7 * ms_ns, 1);
    s.note(15 * ms_ns, 3);
    try testing.expectEqual(@as(u64, 2), s.nodes);
    try testing.expectEqual(@as(u64, 500_000), s.node_ns);
    try testing.expectEqual(@as(u64, 2), s.smo_nodes);
    try testing.expectEqual(@as(u64, 4), s.images);
    try testing.expectEqual(22 * ms_ns, s.image_ns);
    var folded: FlushPassSample = .{};
    folded.fold(s);
    folded.fold(s);
    try testing.expectEqual(@as(u64, 8), folded.images);
    try testing.expectEqual(@as(u64, 4), folded.nodes);
    // The projection.
    try testing.expectEqual(@as(u64, 0), projectedFlushWallNs(0, 250_000, 0, 2 * ms_ns));
    try testing.expectEqual(100 * 250_000 + 40 * 2 * ms_ns, projectedFlushWallNs(100, 250_000, 40, 2 * ms_ns));
    try testing.expectEqual(@as(u64, 140 * 250_000), projectedFlushWallNs(100, 250_000, 40, 0));
    try testing.expectEqual(@as(u64, 140 * 250_000), projectedFlushWallNs(100, 250_000, 40, 100_000));
    try testing.expectEqual(std.math.maxInt(u64), projectedFlushWallNs(std.math.maxInt(u64), 2, 1, 1));
    try testing.expectEqual(@as(u64, 2), coveringBarriers(false));
    try testing.expectEqual(@as(u64, 1), coveringBarriers(true));
    const flush = projectedFlushWallNs(100, 250_000, 40, 2 * ms_ns);
    try testing.expectEqual(flush + 300 * ms_ns, projectedCycleWallNs(flush, coveringBarriers(false), 150 * ms_ns));
    try testing.expectEqual(flush + 150 * ms_ns, projectedCycleWallNs(flush, coveringBarriers(true), 150 * ms_ns));
    try testing.expectEqual(@as(u64, 0), projectedCycleWallNs(0, 2, 0));
    try testing.expectEqual(std.math.maxInt(u64), projectedCycleWallNs(std.math.maxInt(u64) - 1, 2, 150 * ms_ns));
    // The trigger anticipates the larger of the horizon and the projection.
    const projected_ms = flush / ms_ns;
    try testing.expectEqual(@as(u64, 105), projected_ms);
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS - 105, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, anticipatedTermMs(0, projected_ms)));
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS - 133, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, anticipatedTermMs(133, projected_ms)));
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS - 302, checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, projectedCycleWallNs(2 * ms_ns, coveringBarriers(false), 150 * ms_ns) / ms_ns));
}

test "an outlier flush unit saturates the trigger for at most the horizon: 63 sane pushes keep it, the 64th evicts it" {
    const sane = flushUnitNs(4 * ms_ns, 1).?;
    const outlier = flushUnitNs(200 * ms_ns, 1).?;
    const T = struct {
        /// The box's wave: 38 promised images.
        const promised: u64 = 38;
        fn triggerAt(image_unit: u64) u64 {
            return checkpointTriggerMs(CHECKPOINT_MAX_AGE_MS, projectedFlushWallNs(0, 0, promised, image_unit) / ms_ns);
        }
    };
    // 38 × 4 ms = 152 ms leaves a trigger.
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS - 152, T.triggerAt(sane));
    var w: CycleTermWindow = .{};
    w.push(outlier);
    try testing.expectEqual(@as(u64, 0), T.triggerAt(w.anticipatedNs()));
    for (0..TERM_HORIZON_CYCLES - 1) |_| {
        w.push(sane);
        try testing.expectEqual(outlier, w.anticipatedNs());
    }
    try testing.expectEqual(@as(u64, 0), T.triggerAt(w.anticipatedNs()));
    w.push(sane);
    try testing.expectEqual(sane, w.anticipatedNs());
    try testing.expectEqual(T.triggerAt(sane), T.triggerAt(w.anticipatedNs()));
}

test "drain budget is the PASS's: the first item runs past the deadline, every later one waits; inside the deadline every item runs; unbounded admits all" {
    var passed = DrainBudget.bounded(1_000);
    try testing.expect(!passed.progressed);
    try testing.expect(passed.admits(2_000));
    try testing.expect(passed.progressed);
    for (0..64) |_| try testing.expect(!passed.admits(2_000));
    var open = DrainBudget.bounded(1_000_000);
    for (0..64) |_| try testing.expect(open.admits(5));
    try testing.expect(!open.admits(1_000_000));
    var unbounded = DrainBudget.unbounded;
    for (0..64) |_| try testing.expect(unbounded.admits(std.math.maxInt(u64)));
}

fn observed(now_ms: u64) Observations {
    return .{
        .now_ns = now_ms * ms_ns,
        .last_cycle_at_ns = 0,
        .journal_distance = 0,
        .ring_len = 1 << 20,
        .dirty_nodes = 0,
        .max_dirty = 4096,
        .park_mark = false,
        .fsync_pending = false,
        .heap_full = false,
        .term_projection_ms = 0,
    };
}

test "decision matrix: every trigger alone, their priorities, the immediate-barrier set, covers_something, the deferred barrier and the sweep as their own terms" {
    // Nothing: not due.
    const idle = decideCheckpoint(observed(10));
    try testing.expect(!idle.due);
    try testing.expectEqual(Reason.none, idle.reason);
    try testing.expect(!idle.immediate_barrier and !idle.covers_something and !idle.deferred_barrier and !idle.sweep_due);
    try testing.expectEqual(CHECKPOINT_MAX_AGE_MS, idle.trigger_ms);
    try testing.expectEqual(@as(?u64, null), idle.age_late_ns);
    // Age alone: due at exactly the max age, not before; no immediate barrier;
    // a due tick with nothing to cover runs no cycle.
    var o = observed(999);
    try testing.expect(!decideCheckpoint(o).due);
    o = observed(1000);
    var d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .age and !d.immediate_barrier and !d.covers_something and !d.runsACycle());
    try testing.expectEqual(@as(?u64, 0), d.age_late_ns);
    o.dirty_nodes = 1;
    d = decideCheckpoint(o);
    try testing.expect(d.runsACycle());
    o = observed(1_015);
    o.journal_distance = 1;
    o.mutex_wait_ns = 5 * ms_ns;
    d = decideCheckpoint(o);
    try testing.expect(d.runsACycle());
    try testing.expectEqual(@as(?u64, 10 * ms_ns), d.age_late_ns);
    // The forest trigger anticipates the term: due at max_age − term.
    o = observed(850);
    o.term_projection_ms = 150;
    o.dirty_nodes = 3;
    d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .age);
    try testing.expectEqual(@as(u64, 850), d.trigger_ms);
    o = observed(849);
    o.term_projection_ms = 150;
    try testing.expect(!decideCheckpoint(o).due);
    // A term at the max age: due every tick.
    o = observed(1);
    o.term_projection_ms = CHECKPOINT_MAX_AGE_MS;
    try testing.expect(decideCheckpoint(o).due);
    // The elastic max age verbatim.
    o = observed(500);
    o.max_age_ms = 500;
    try testing.expect(decideCheckpoint(o).due);
    o = observed(499);
    o.max_age_ms = 500;
    try testing.expect(!decideCheckpoint(o).due);
    // Dirty cap alone: strictly above the cap; no immediate barrier.
    o = observed(10);
    o.dirty_nodes = 4096;
    try testing.expect(!decideCheckpoint(o).due);
    o.dirty_nodes = 4097;
    d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .dirty_cap and !d.immediate_barrier and d.covers_something);
    // Ring pressure alone: strictly past L/2; immediate barrier.
    o = observed(10);
    o.journal_distance = o.ring_len / 2;
    try testing.expect(!decideCheckpoint(o).due);
    o.journal_distance = o.ring_len / 2 + 1;
    d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .ring_pressure and d.immediate_barrier and d.covers_something);
    // Park mark alone: due with an immediate barrier even with the pressure
    // law far from reached (the reserve is half of a floor-sized ring).
    o = observed(10);
    o.park_mark = true;
    o.journal_distance = 1;
    d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .park_kick and d.immediate_barrier and d.runsACycle());
    // Shutdown: due, immediate, covers (the final cycle runs on an idle volume).
    o = observed(10);
    o.final_cycle = true;
    d = decideCheckpoint(o);
    try testing.expect(d.due and d.reason == .shutdown and d.immediate_barrier and d.covers_something);
    // Priorities: shutdown > ring > park > cap > age.
    o = observed(5000);
    o.dirty_nodes = 9000;
    o.park_mark = true;
    o.journal_distance = o.ring_len;
    o.final_cycle = true;
    try testing.expectEqual(Reason.shutdown, decideCheckpoint(o).reason);
    o.final_cycle = false;
    try testing.expectEqual(Reason.ring_pressure, decideCheckpoint(o).reason);
    o.journal_distance = 1;
    try testing.expectEqual(Reason.park_kick, decideCheckpoint(o).reason);
    o.park_mark = false;
    d = decideCheckpoint(o);
    try testing.expectEqual(Reason.dirty_cap, d.reason);
    try testing.expect(!d.immediate_barrier);
    // Age is still carried beside a higher-priority reason.
    try testing.expect(d.age_late_ns != null);
    o.dirty_nodes = 1;
    try testing.expectEqual(Reason.age, decideCheckpoint(o).reason);
    // The deferred barrier and the sweep are their own terms, never `due`.
    o = observed(10);
    o.fsync_pending = true;
    o.heap_full = true;
    d = decideCheckpoint(o);
    try testing.expect(!d.due and d.deferred_barrier and d.sweep_due);
}

test "flush-ceiling audit: inside at the ceiling, an overrun one ns past it, a structural hold's excused overlap turns an overrun into an extension; the tripwire fires per overrun" {
    const ceiling = 1_100 * ms_ns;
    const since: u64 = 10 * ms_ns;
    try testing.expectEqual(CeilingVerdict.inside, judgeFlushCeiling(.{ .region = 1, .since_ns = since }, since + ceiling, ceiling));
    try testing.expectEqual(CeilingVerdict.overrun, judgeFlushCeiling(.{ .region = 1, .since_ns = since }, since + ceiling + 1, ceiling));
    try testing.expectEqual(CeilingVerdict.extended, judgeFlushCeiling(.{ .region = 1, .since_ns = since, .excused_ns = 50 * ms_ns }, since + ceiling + 40 * ms_ns, ceiling));
    try testing.expectEqual(CeilingVerdict.overrun, judgeFlushCeiling(.{ .region = 1, .since_ns = since, .excused_ns = 50 * ms_ns }, since + ceiling + 51 * ms_ns, ceiling));
    var leaf: DirtyLeafAge = .{ .region = 2, .since_ns = 100 };
    leaf.foldOldest(200, 7);
    try testing.expectEqual(@as(u64, 100), leaf.since_ns);
    leaf.foldOldest(50, 7);
    try testing.expectEqual(@as(u64, 50), leaf.since_ns);
    try testing.expectEqual(@as(u64, 7), leaf.excused_ns);
    const before = appender_flush_ceiling_overruns.load();
    const ext_before = appender_flush_ceiling_extensions.load();
    const leaves = [_]DirtyLeafAge{
        .{ .region = 0, .since_ns = since },
        .{ .region = 1, .since_ns = since - 1 },
        // Aged 2 ns past the ceiling, 2 ns of it under a structural hold.
        .{ .region = 2, .since_ns = since - 1, .excused_ns = 2 },
    };
    // Exactly at the ceiling for region 0: nothing fires.
    const quiet = auditFlushCeiling(leaves[0..1], since + ceiling, ceiling);
    try testing.expectEqual(@as(u64, 0), quiet.overruns);
    try testing.expectEqual(before, appender_flush_ceiling_overruns.load());
    // One ns later: region 0 and region 1 overrun, region 2 is excused.
    appender_flush_ceiling_overruns.last_log_s.store(0, .monotonic);
    log.testing.expectErrorLogs(1);
    const out = auditFlushCeiling(&leaves, since + ceiling + 1, ceiling);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 2), out.overruns);
    try testing.expectEqual(@as(u64, 1), out.extensions);
    try testing.expectEqual(before + 2, appender_flush_ceiling_overruns.load());
    try testing.expectEqual(ext_before + 1, appender_flush_ceiling_extensions.load());
}

test "the tape's phases are exact-sum and it formats as the WARN line" {
    var tape: CycleTape = .{ .seq = 7, .collect_ns = 1, .flush_ns = 2, .pages_ns = 3, .barrier_ns = 4, .tail_ns = 5, .ledger_ns = 6, .reclaim_ns = 7 };
    tape.total_ns = tape.phasesSum();
    try testing.expectEqual(@as(u64, 28), tape.total_ns);
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try w.print("{f}", .{tape});
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "cycle 7 decided at trigger 0 ms"));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "paid pre-start 0 + collect 0 + flush 0") != null);
    var hist: PhaseHistograms = .{};
    hist.record(tape);
    try testing.expectEqual(@as(u64, 1), hist.total.snapshot().count);
    try testing.expectEqual(@as(u64, 28), hist.total.snapshot().sum_ns);
    try testing.expectEqual(@as(u64, 7), hist.reclaim.snapshot().sum_ns);
}
