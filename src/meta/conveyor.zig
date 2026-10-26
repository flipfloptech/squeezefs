//! The commit conveyor: stage A (the apply pass) and the stage-B handoff
//! (charter §5.2 "The commit conveyor", "Failure"; §2 lock order 4a/4b/4c;
//! survey `docs/porting/kv_tree_cache_commit_survey.md` §2.2–§2.4, §2.7,
//! §3.1, §3.5, §3.6, §5.1, §7 row 12; `docs/porting/kv_journal_ledger.md`
//! §3.3–§3.4; the reference's `backend.rs` `commit_tx` / `commit_tx_group`
//! / `conveyor_pass_task` / `run_batch_pipeline` / `rollback_failed_tx`).
//! PORTABLE: the device is behind the `LaneSeam` (the Linux journal lane
//! in `backend.zig`; the in-RAM `RamLane` below in the contracts).
//!
//! ## The protocol
//!
//! A committer builds its queue entry (`tx.buildQueuedTx`: the exact entry
//! length, the value cap — an over-cap tx fails ALONE), enqueues it with
//! its completion channel, leader-elects (`conveyor_core`: no suspension
//! point between the two) and parks on the channel. The election is the
//! "is a pass already scheduled" latch: the winner WAKES the pass through
//! the `PassSeam` (one wake; a loser's entry is drained by the pass the
//! winner woke — the no-lost-wakeup theorem) and parks like every follower.
//! The pass (`runScheduledPass`) runs until the queue is empty and
//! leadership is released (`unleadAndRecheck`). On a mounted volume the
//! pass is a fiber on the volume's journal lane (`backend.zig`; charter
//! §5.2, decision 0002 row 6): it orders, admits and reserves on the
//! thread that owns the ring; since decision 0003 §AW the RAM apply, the
//! encode and the submit are an APPLIER's — a fiber the pass spawns per
//! window on the applier venue (the backend: a fixed set of handler lanes,
//! `applierParallelism` wide) — so the lane's serial work per tx is the
//! reservation and the ack, and the applies of several windows overlap on
//! other cores. The portable contracts bind the pass seam to an inline
//! call and the applier to the pass's own thread (stage A whole, as the
//! reference ran it).
//!
//! Stage A per drained batch, the survey's 14 steps in §AW's order:
//!   A0 drain (caps: `max(64, cpus × 2)` txs / `max(256 KiB, ring/16)`
//!   bytes clamped to the ring's admissible capacity; NO timers) → A1 the
//!   liveness flags → A2 ONE ring admission for Σ sizes BEFORE any node
//!   lock (a parked committer holds no node lock; the park kicks the
//!   checkpoint task with the park mark and escalates through the
//!   journal-failure lattice past `PARK_ESCALATION_NS`) → A8 ONE
//!   contiguous reservation, per-tx seqs stamped back-to-back (the
//!   registry's open entry holds the checkpoint tail back until the lane
//!   completes it, after the apply and the landing) → the handoff to the
//!   applier, which runs: A3 resolve every record's leaf latch-free → A4
//!   lock the UNION ascending by address, deduped → A5 revalidate every
//!   member (any member moved ⇒ drop ALL, re-resolve ALL; the seqs are the
//!   window's) → A7 pre-images, first touch per `(tree, key)` → A9 apply
//!   in queue order (a member whose apply fails is rolled OUT in-window and
//!   its range is left an unwritten hole) → A10 the writeback threshold →
//!   unlock → A11 the threshold kick → A12 SUBMIT the survivors as ordinary
//!   checksummed entries in the one reservation (no mega-entry) → A13 hand
//!   the window to stage B. A6, the heap admission (`heap_law.
//!   admitHeapLocked`: a refused tx answers `NoSpace`; one retry behind ≤ 2
//!   return-room cycles when budget can return), is the COMMITTER's before
//!   it enqueues: a refusal reserves nothing, and its retry's checkpoint
//!   cycles never run behind an open reservation of its own.
//!
//! Stage B is the lane's: awaits the writes in handoff order, completes the
//! reservations in journal order, the completed-prefix wait, the hole
//! checkpoints, ONE barrier (strict: before any ack), acks in journal
//! order. The lane calls back `Window.finish`: GUARDS DROP BEFORE THE ACK
//! (D-3: 11 % of many-dirs renames at 32 writers self-waited on their own
//! previous tx when the guards outlived the answer), then the oneshot. A
//! failed write runs `rollbackFailedTx` ON THE LANE THREAD before the
//! verdict: leaf locks re-acquired ascending, every record of the window's
//! STAMPED span removed seq-conditionally (a newer committed record
//! stands), the latch counted; a record that ESCAPED the open overlay (a
//! freeze raced the failed write) gets a compensating record through the
//! checkpoint-class direct writer so a crash after the escaped bset still
//! replays to the rolled-back state. `JOURNAL_FAILURE_LATCH` consecutive
//! failures ⇒ FAILED: every later commit answers `VolumeFailed`.
//!
//! WHY two stages (D-2): the serialized M7 pass measured 752–776 µs, of
//! which `journal_ring_write` was 650–672 µs and the leaf-lock window
//! 86–91 µs (ρ ≈ 0.97, committers queued 764–801 µs); handing the device
//! wait to stage B took the conveyor from ρ 1.0 to 0.13 and `pass_total`
//! to 97–147 µs. On the shared pool the completion's cross-thread wake hop
//! was 746/664 µs of a 1,209/1,130 µs round trip (62 % / 59 %) — the
//! journal lane owns the ring and acks on the thread that reaps the CQE
//! (`journal_lane.zig`).
//!
//! The reference's `PassSentinel`/`LaneSentinel` (Drop guards for an
//! unwinding pass) have no counterpart: Zig does not unwind, and every
//! failure path here is an explicit arm (`failBatch`, `abandonWindow`)
//! with its admission released or its reservation completed — a
//! reservation that never completes would wedge `completed_upto`, an
//! admission dropped on the floor leaks ring budget forever.

const std = @import("std");
const core_sort = @import("../core/sort.zig");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../core/counters.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const core_rtt = @import("../core/rtt.zig");
const knobs = @import("../core/knobs.zig");
const sync = @import("../sync/sync.zig");
const conveyor_core = @import("conveyor_core.zig");
const tx = @import("tx.zig");
const token_table = @import("sym/tokens/token_table.zig");
const lease_epoch = @import("../data/lease_epoch.zig");
const tree = @import("tree.zig");
const node = @import("node.zig");
const node_cache = @import("node_cache.zig");
const snapshot = @import("node_snapshot.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");
const alloc_ext = @import("alloc_ext.zig");
const heap_law = @import("heap_law.zig");
const smo = @import("smo.zig");
const key = @import("key.zig");
const record = @import("record.zig");

const assert = std.debug.assert;

pub const KvTree = tree.KvTree;
pub const CachedNode = node_cache.CachedNode;
pub const NodeDirty = node_cache.NodeDirty;
pub const NodeCache = node_cache.NodeCache;
pub const OwnedRec = node_cache.OwnedRec;
pub const QueuedTx = tx.QueuedTx;
pub const Reservation = jc.Reservation;
pub const SeqSpan = jc.SeqSpan;

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// Batch tx cap floor: the shipped M7 value (a 32-CPU box derives exactly
/// 64), never regressed below.
pub const BATCH_TXS_FLOOR: usize = 64;
/// Txs per CPU: arrivals scale with handler parallelism (one FUSE queue per
/// possible CPU), two in flight per lane under the serve/commit overlap.
pub const BATCH_TXS_PER_CPU: usize = 2;
/// Batch byte cap floor: the shipped M7 value.
pub const BATCH_BYTES_FLOOR: u64 = 256 * derive.KiB;
/// `ring_user_capacity / 16`: at least sixteen batch reservations always
/// cycle the ring (the liveness margin against the checkpoint lag).
pub const BATCH_BYTES_RING_DIVISOR: u64 = 16;
/// The lock-revalidate retry budget (`COMMIT_RETRY_BUDGET`): SMOs are rare
/// and serialized, so exhaustion is a protocol bug.
pub const COMMIT_RETRY_BUDGET: u32 = node.RETRY_BUDGET;
/// A pass parked longer than this at ring admission is the wedged-not-failed
/// class (D1.b audit row 2): the watchdog's escalation threshold. 30 s is the
/// reference's measured ceiling on a healthy drain (three orders above the
/// 1 s checkpoint cadence that frees ring space); a knob for it was a free-
/// floating constant with an operator in the loop (decision 0003 Q7), and
/// the per-op timeout wrapper it could resemble is banned (charter §4).
pub const PARK_ESCALATION_NS: u64 = 30 * std.time.ns_per_s;
/// The admission park's tick: a LOST-WAKE BACKSTOP only — every barrier
/// that moves `reusable_upto` notifies the parkers itself
/// (`checkpoint_cycle.afterDurableBarrier`), whichever thread ran it, so a
/// tick that finds room is counted (`park_tick_rescues`, ≈ 0). 10 ms is the
/// reference's floor: five barrier walls (≈ 2 ms measured) of dead time at
/// worst if a wake were ever lost, far inside the 1 s cadence.
pub const PARK_TICK_NS: u64 = 10 * std.time.ns_per_ms;

// ---------------------------------------------------------------------------
// The window-fill hold (decision 0003 §L — the journal's pad economy).
//
// The sector-pad law prices every window at ≥ one grain of device write
// whatever it carries: `n` entries of `s` bytes cost `g/n` bytes per op and
// a pad fraction `1 − n·s/g` on a sub-page window. Without a timer the pass
// drains what arrived during its own latency (`n = λ·L_pass`): 1.46
// entries per 4 KiB window at 12.4 k IOPS on the venue — 0.715 × the user
// bytes landed on the META namespace as pad (F14; generic/074: 430 B/op).
//
// THE LAW: a pad-majority window (its pad > its entries) is never issued
// while a window is in flight; the pass PARKS until that window's terminal
// outcome, bounded by the round trip's own predictor, then re-drains. So:
//   - pad-majority windows number at most one per device round trip —
//     the journal's pad can never saturate its device (≤ `g/R` B/s of pad);
//   - a committer's commit latency at most doubles (it waits for one
//     round trip the device would have charged its own write anyway) and
//     a LONE committer never waits (nothing in flight ⇒ no hold);
//   - an entry-majority window pipelines as before: its device write is
//     majority payload and the batching has nothing left to recover.
// Strict mode holds nothing: a per-commit-barrier committer declared
// latency over bytes (`META_FLUSH_INTERVAL_MS=0`, the control arm).
// ---------------------------------------------------------------------------

/// The hold law in force on a volume: null = never hold (strict cadence;
/// the portable contracts unless a test arms it).
pub const WindowHold = struct {
    /// The deadline's ceiling: the coalesced cadence's flush interval — a
    /// hold past the barrier cadence would carry a window across the
    /// barrier that was due to cover it.
    ceiling_ns: u64,
    /// Test seam: every eligible window is held as if the worth gate were
    /// open, so a row measures the hold's pacing law apart from the gate's
    /// learning (the worth-gate tie rows judge that).
    pin_gate_open: bool = false,
    /// `SQUEEZEFS_META_WINDOW_HOLD=pacing`: the pad-majority hold off, the
    /// applier pacing kept.
    pad_hold: bool = true,
};

/// The law from the ONE flush-interval knob (`checkpoint.flushIntervalMsFromEnv`):
/// 0 = strict = no hold; else the interval is the deadline's ceiling.
pub fn windowHoldFor(flush_interval_ms: u64) ?WindowHold {
    if (flush_interval_ms == 0) return null;
    return .{ .ceiling_ns = flush_interval_ms * std.time.ns_per_ms };
}

/// The grain at and above which the §L pad hold is worth its wait: a
/// pad-majority window on a 4 KiB-grain device wastes a page of ring per
/// round trip; on a 512-B grain it wastes bytes (65 → 87 B per create
/// measured) while the hold costs every member of the held batch a landing
/// (`docs/evidence/rung-10-create-path.md`, wave 3; decision 0003 §BA).
pub const PAD_HOLD_GRAIN_MIN: u64 = 4096;

/// The §L pad hold's derivation from the device grain.
pub fn padHoldFor(grain: u64) bool {
    return grain >= PAD_HOLD_GRAIN_MIN;
}

/// `windowHoldFor` under the `SQUEEZEFS_META_WINDOW_HOLD` arm (`derived`
/// = `padHoldFor(grain)` with the pacing hold).
pub fn windowHoldFromEnv(env: *const std.process.Environ.Map, flush_interval_ms: u64, grain: u64) ?WindowHold {
    const arm = knobs.enumKnob("SQUEEZEFS_META_WINDOW_HOLD", env);
    if (std.mem.eql(u8, arm, "none")) return null;
    var law = windowHoldFor(flush_interval_ms) orelse return null;
    law.pad_hold = if (std.mem.eql(u8, arm, "derived")) padHoldFor(grain) else !std.mem.eql(u8, arm, "pacing");
    return law;
}

/// The applier venue's width, `clamp(cpus / 8, 2, 4)` (decision 0003
/// §AW.3): a fixed set of handler lanes an eighth of the cores wide — the
/// windows of one hot directory queue on one lane and keep its leaf lock
/// off the cross-lane handoff; the floor of 2 is the width at which a
/// window's apply overlaps the next's reservation at all (a lone window
/// applies inline anyway); the cap of 4 is where the measured gain stopped
/// (`docs/evidence/rung-10-create-path.md`: four pinned lanes against a
/// rotation over 32 doubled the apply's CPU on cold caches).
pub fn applierParallelism(cpus: u32) u32 {
    return @min(4, @max(2, cpus / 8));
}

/// The window round-trip estimator (handoff → terminal outcome; `core/rtt.zig`,
/// RFC 6298 — the hold's deadline is where a round trip stops being a round
/// trip and starts being a stall), fed on the lane thread at every window's
/// finish and read by the pass fiber on the same thread; the RAM contracts'
/// device thread feeds it from another. The same estimator smooths the
/// committers' inter-arrival interval (fed by the pass alone) for the worth
/// gate.
pub const RttEstimator = core_rtt.RttEstimator;

/// THE WORTH GATE: a hold pays for itself iff it collects at least one
/// entry that would otherwise have been its own window (one more entry on
/// the same page is one grain saved; the hold's cost is its members' wait
/// of ≤ one round trip). The gate is the MEASURED gain — entries the
/// re-drain collected per hold, in `GAIN_UNIT` fixed point — because the
/// arrival rate alone misjudges a closed loop: a QD1 committer delayed by
/// the hold arrives later, and the rate the hold needs is the rate it
/// suppresses. Measured ungated on the venue: `fsync-4k` (4 QD1 jobs)
/// held 30 % of its windows for 48–55 µs, collected 0.09 per hold and lost
/// 12 % of the fsyncs/s; `frag-512b-write` (4 QD1 jobs) collected 0.35 and
/// wrote 8 % MORE META bytes at −11 % IOPS (the hold broke the jobs'
/// natural coincidence); `rand-4k-write` (QD32 × 4) collected 1.3 and took
/// the META bytes from 0.70 to 0.46 × user at +20 % IOPS.
///
/// The gain is a property of the PACED regime — committers acked at one
/// landing re-arrive as a cohort during the next window's flight — and the
/// regime forms over two landings, so an OFF gate measures it with a PROBE
/// BURST: `HOLD_PROBE_FORMING` holds unsampled, then `HOLD_PROBE_BURST`
/// holds whose arithmetic mean decides (a lone probe in the unpaced
/// regime measured 0.61–0.65 of `rand-4k-write`'s 1.3; an EWMA seeded by
/// the burst's first, unpaced hold still weighed it 0.39 after eight and
/// read 0.92–0.94). An ON gate tracks the RFC 6298 mean per hold and
/// closes below `GAIN_CLOSE` (opened at `GAIN_OPEN`: the band).
pub fn holdWorthIt(gain_mean: ?u64, open: bool, batch_len: usize) bool {
    return (gain_mean orelse 0) >= gainLine(open, batch_len);
}

/// The line a hold's collection must clear, scaled by the batch it would
/// lengthen: a hold removes `g·c/(n+c)` of the window's pad, so the ½-entry
/// line §L measured at its n̄ of 1.56 entries is a QUARTER of the pad — the
/// same quarter at any n is `c ≥ n/3` (the close line keeps the 2:1 band).
/// A seven-entry batch of creates collected 0.4 per hold (5 % of its pad)
/// and the ½ line alone kept the gate open: every window waited one round
/// trip for the applier-fed pipeline to drain (62 % of the passes held,
/// 170 µs each), the committers' closed loop ran at the hold's pace.
pub fn gainLine(open: bool, batch_len: usize) u64 {
    const n: u64 = @intCast(batch_len);
    return if (open) @max(GAIN_CLOSE, n * GAIN_UNIT / 6) else @max(GAIN_OPEN, n * GAIN_UNIT / 3);
}

/// The gain estimator's fixed point: one collected entry = 1024 (the
/// EWMA's eighth of a difference stays whole down to 1/128 of an entry).
pub const GAIN_UNIT: u64 = 1024;
/// The gate OPENS at half an entry per hold: the measured dividing line
/// between the shapes where the hold waited for nobody (`fsync-4k` 0.09,
/// `frag-512b-write` 0.35 — both lost throughput ungated) and the one
/// where it paid (`rand-4k-write` 0.9–1.3 per hold ungated: −35 % META
/// bytes at unchanged IOPS and p50; its probe bursts read 0.4–0.9 on the
/// venue). Below ½ a hold saves less than a grain per two round trips; a
/// venue whose round trip is cheap against a grain's device time moves the
/// line with a counted A/B, never by hand.
pub const GAIN_OPEN: u64 = GAIN_UNIT / 2;
/// … and CLOSES at a quarter: a 2:1 band, because a hold's collection is
/// Poisson-shaped (variance ≈ mean) and an open gate tracking an EWMA at
/// the line itself flapped (`rand-4k-write` at a single ½ line: open a
/// third of the time, 0.71 → 0.62 × user; ungated 0.46).
pub const GAIN_CLOSE: u64 = GAIN_UNIT / 4;
/// The holds a probe burst pays before it samples: the cohort forms at the
/// first held window's landing (its members acked together) and re-arrives
/// during the second's flight — the first sampled hold is the third.
pub const HOLD_PROBE_FORMING: u32 = 2;
/// The sampled holds of a probe burst: one estimator memory (`1/α = 8`),
/// so the mean the burst answers and the EWMA it seeds carry the same
/// weight of evidence.
pub const HOLD_PROBE_BURST: u32 = 1 << core_rtt.ALPHA_SHIFT;
/// A probe burst every this many eligible windows while the gate is off:
/// sixteen bursts' worth, so a workload that starts paying is seen within
/// ≈ 6 % of its windows and an off gate's cost is one hold in sixteen (each
/// ≤ one round trip).
pub const HOLD_PROBE_PERIOD: u32 = 16 * (HOLD_PROBE_FORMING + HOLD_PROBE_BURST);

/// The gate's state (pass-fiber-owned): the EWMA that governs an ON gate,
/// the countdown to the next probe burst while OFF, and the burst's own
/// tally.
pub const WorthGate = struct {
    gain: RttEstimator = .{},
    /// The gate is open: holds are paid and tracked until the EWMA falls
    /// under `GAIN_CLOSE`.
    open: bool = false,
    /// Eligible windows declined since the last burst.
    countdown: u32 = 0,
    /// Holds left in the running burst (0 = none running).
    burst_left: u32 = 0,
    burst_sum: u64 = 0,
    /// The sampled holds' batch sizes (the line the burst's mean is held to).
    burst_batch_sum: usize = 0,
    burst_n: u32 = 0,

    pub const Verdict = enum { hold, probe, decline };

    /// Whether an eligible pad-majority window of `batch_len` entries is
    /// held: by the ON gate, by a running or due probe burst, or declined.
    pub fn decide(self: *WorthGate, batch_len: usize) Verdict {
        if (self.open) {
            if (holdWorthIt(self.gain.mean(), true, batch_len)) return .hold;
            self.open = false;
        }
        if (self.burst_left == 0) {
            self.countdown += 1;
            if (self.countdown < HOLD_PROBE_PERIOD) return .decline;
            self.countdown = 0;
            self.burst_left = HOLD_PROBE_FORMING + HOLD_PROBE_BURST;
            self.burst_sum = 0;
            self.burst_batch_sum = 0;
            self.burst_n = 0;
        }
        self.burst_left -= 1;
        return .probe;
    }

    /// What the hold collected (`GAIN_UNIT` fixed point), after a `.hold`
    /// (the EWMA tracks) or a `.probe` (the burst tallies its sampled
    /// holds; at its end the mean seeds the EWMA afresh — the gate opens
    /// iff the mean says so).
    pub fn observe(self: *WorthGate, verdict: Verdict, collected: u64, batch_len: usize) void {
        switch (verdict) {
            .hold => self.gain.sample(collected),
            .probe => {
                const position = HOLD_PROBE_FORMING + HOLD_PROBE_BURST - 1 - self.burst_left;
                if (position >= HOLD_PROBE_FORMING) {
                    self.burst_sum += collected;
                    self.burst_batch_sum += batch_len;
                    self.burst_n += 1;
                }
                if (self.burst_left == 0) {
                    self.gain = .{};
                    self.gain.sample(if (self.burst_n == 0) 0 else self.burst_sum / self.burst_n);
                    self.open = holdWorthIt(self.gain.mean(), false, if (self.burst_n == 0) batch_len else self.burst_batch_sum / self.burst_n);
                }
            },
            .decline => unreachable,
        }
    }

    /// The live gain (entries per hold × 1000) for the stats face.
    pub fn gainMilli(self: *const WorthGate) u64 {
        return (self.gain.mean() orelse 0) * 1000 / GAIN_UNIT;
    }
};

/// The pad arithmetic the §L bound restates (tie tests below): a window
/// of `n` entries of `s` bytes landing at the data start of a page on a
/// `geo`-grained ring — its device bytes, its pad, its pad fraction in
/// parts per million.
pub const WindowCost = struct {
    device_bytes: u64,
    pad_bytes: u64,

    /// The ratio unit of the ledgers: parts per million (an integer; no
    /// floats on a wire — `bench.Row.PPM` is the same unit).
    pub const PPM: u64 = 1_000_000;

    pub fn padFractionPpm(self: WindowCost) u64 {
        if (self.device_bytes == 0) return 0;
        return @intCast((@as(u128, self.pad_bytes) * PPM) / self.device_bytes);
    }
};

pub fn windowCost(geo: jc.CoreGeometry, n: u64, s: u64) WindowCost {
    const entry_bytes = n * s;
    const pad = geo.padOf(entry_bytes);
    const span = entry_bytes + pad;
    // Pages whose first byte the span covers carry a 24 B header each.
    var pages: u64 = 0;
    var it = geo.ownedPageStarts(0, span);
    while (it.next()) |_| pages += 1;
    return .{ .device_bytes = span + pages * jc.PAGE_HDR_LEN, .pad_bytes = pad };
}

/// The §L bound on journal device bytes per op for an arrival rate
/// `lambda_per_s` of `s`-byte entries against a window round trip `rtt_ns`
/// (+ the pass latency `pass_ns`): pad-majority windows are paced one per
/// round trip, so each carries at least `n = λ·(R + L_pass)` entries
/// (floor 1: a lone committer's window) and costs `windowCost(n, s)`;
/// an entry-majority window costs under `2s` per entry. The pacing law
/// the harness checks is the COUNT form (`windows_held + windows_unshadowed
/// ≤ landings + deadlines + 1`); this is its byte reading at a shape.
pub fn bytesPerOpBound(geo: jc.CoreGeometry, lambda_per_s: u64, s: u64, rtt_ns: u64, pass_ns: u64) u64 {
    const per_window: u64 = @intCast(@max(1, (@as(u128, lambda_per_s) * (rtt_ns + pass_ns)) / std.time.ns_per_s));
    return @max(windowCost(geo, per_window, s).device_bytes / per_window, 2 * s);
}

// ---------------------------------------------------------------------------
// Batch caps (survey §4.1 rows 1–2; tie tests below).
// ---------------------------------------------------------------------------

pub const Caps = conveyor_core.ConveyorCore(Queued).Caps;

/// `max(64, cpus × 2)`.
pub fn deriveBatchTxs(cpus: u32) usize {
    return @max(BATCH_TXS_FLOOR, @as(usize, cpus) * BATCH_TXS_PER_CPU);
}

/// `max(256 KiB, ring_user_capacity / 16)`.
pub fn deriveBatchBytes(ring_user_capacity: u64) u64 {
    return @max(BATCH_BYTES_FLOOR, ring_user_capacity / BATCH_BYTES_RING_DIVISOR);
}

/// The user-admissible capacity one Σ-admission must fit: the logical ring
/// less the checkpoint reserve less the pad slack every admission claims.
pub fn ringUserCapacity(geo: jc.CoreGeometry) u64 {
    return (geo.logicalLen() -| geo.reserve_bytes) -| geo.maxPad();
}

/// Explicit wins verbatim; derived otherwise.
pub fn resolveBatchTxs(explicit: ?usize, cpus: u32) usize {
    return explicit orelse deriveBatchTxs(cpus);
}

/// Explicit or derived, CLAMPED to the admissible capacity so a batch's one
/// Σ-admission can always eventually succeed once the drain catches up.
pub fn resolveBatchBytes(explicit: ?u64, ring_user_capacity: u64) u64 {
    return @min(explicit orelse deriveBatchBytes(ring_user_capacity), @max(ring_user_capacity, 1));
}

/// The caps in force: the two knobs over the derived laws, the byte cap
/// clamped to the ring.
pub fn resolveCaps(env: *const std.process.Environ.Map, cpus: u32, geo: jc.CoreGeometry) Caps {
    const cap = ringUserCapacity(geo);
    return .{
        .max_txs = resolveBatchTxs(knobs.optIntKnob("SQUEEZEFS_META_COMMIT_BATCH_TXS", usize, env), cpus),
        .max_bytes = resolveBatchBytes(knobs.optIntKnob("SQUEEZEFS_META_COMMIT_BATCH_BYTES", u64, env), cap),
    };
}

/// Queue cells: one per ring page, floored for tiny rings, capped at a
/// 32 MiB ring's 8192 pages. NOT a bound on committers: a committer
/// queues BEFORE ring admission, so nothing upstream bounds how many
/// arrive at once (FUSE requests in flight, IPC sessions, tools). A
/// committer that finds every cell taken PARKS for room (`enqueueOne`,
/// 0003 §BO.2) — the queue's size is a batching depth, never an errno.
pub fn queueCapacity(pages: u64) usize {
    return @intCast(derive.clamp(derive.nextPow2(pages), 64, 8192));
}

/// The windows the pass keeps in flight over a lane holding `window_cells`
/// for them: one fewer — the lane runs a window's terminal hook (whose
/// `finishWindow` untracks it) BEFORE it gives the cell back, so one
/// untracked window may still hold its cell at the next handoff
/// (`conveyor_core_model.WindowCells`: a cap of `window_cells` fails).
pub fn windowCap(window_cells: usize) usize {
    return window_cells - 1;
}

// ---------------------------------------------------------------------------
// Errors: the typed classes (survey §2.19) with ONE errno each.
// ---------------------------------------------------------------------------

pub const Error = error{
    /// The heap law refused the member's projected SMO (`ENOSPC`).
    NoSpace,
    /// A staged value above `min(64 KiB, node_size/4)` + envelope.
    ValueTooLarge,
    /// The whole entry above the 128 KiB cap.
    EntryTooLarge,
    /// A forest key the codec refused.
    ForestKeyRefused,
    /// The mutation gate refused (a reader mount, an unleased leaf).
    PartitionRefused,
    /// The volume is closing: nothing new is admitted.
    VolumeClosed,
    /// A bounded resource refused an INTERNAL caller: a window's handoff
    /// met a full lane (the pass panic), a checkpoint-class admission (a
    /// deferred root publish), a group wider than the whole queue. Never a
    /// committer's or an fsync's answer under load — they wait (0003 §BO).
    ConveyorFull,
    OutOfMemory,
    /// The FAILED latch: `JOURNAL_FAILURE_LATCH` consecutive journal
    /// failures; every later commit answers this until remount.
    VolumeFailed,
    /// This window's journal write failed; the tx was rolled back.
    JournalWriteFailed,
    /// A hole this ack depended on could not be checkpointed past.
    HoleCheckpointFailed,
    /// A device read failed resolving a leaf.
    Io,
    /// A try-only read (`node_cache.try_only`, decision 0003 §AB.1) met a
    /// node the cache does not hold: the caller asks again blocking.
    WouldBlock,
    /// A routing/protocol inconsistency.
    Corrupt,
    /// The lock-revalidate budget exhausted (an SMO protocol bug).
    RetryBudgetExhausted,
    /// The commit door refused: another appender leases a slot the entry
    /// names ("ship to the holder" — EAGAIN), or the slot left this volume.
    SlotBusy,
    /// The door's first touch was deferred (ring 0's window, the
    /// structural door's moving tail): EAGAIN, nothing written.
    Deferred,
    /// A JOINED appender's grant pool could not back the member's
    /// projected SMO: the manager dependency (EAGAIN; the cadence's refill
    /// owns the retry), never ENOSPC (charter §5.5 "Extent grants").
    GrantExhausted,
};

pub const ErrorClass = errors.Class;

/// This module's names onto the ONE error set (`core/errors.zig`): the
/// errno and the class are the table's. Backpressure is `QueueFull`
/// (EAGAIN); a closing volume is the dismount (EROFS); a reader mount's or
/// an unleased tree's mutation is read-only; protocol failures are EIO.
pub fn coreError(e: Error) errors.Error {
    return switch (e) {
        error.NoSpace => error.NoSpaceLeft,
        error.ValueTooLarge, error.EntryTooLarge => error.ValueTooLarge,
        error.ForestKeyRefused => error.MalformedKey,
        error.PartitionRefused => error.ReadOnlyFileSystem,
        error.VolumeClosed => error.DismountRefused,
        error.ConveyorFull => error.QueueFull,
        error.OutOfMemory => error.OutOfMemory,
        error.VolumeFailed => error.VolumeFailed,
        error.HoleCheckpointFailed => error.HoleCheckpointFailed,
        error.SlotBusy => error.SlotMoved,
        error.Deferred => error.Deferred,
        error.GrantExhausted => error.GrantExhausted,
        error.JournalWriteFailed, error.Io, error.Corrupt, error.RetryBudgetExhausted => error.InputOutput,
        error.WouldBlock => error.WouldBlock,
    };
}

pub fn classOf(e: Error) ErrorClass {
    return errors.classOf(coreError(e));
}

pub fn errnoOf(e: Error) std.os.linux.E {
    return errors.toErrno(coreError(e));
}

fn mapBuildError(e: tx.BuildError) Error {
    return switch (e) {
        error.ValueTooLarge => error.ValueTooLarge,
        error.EntryTooLarge => error.EntryTooLarge,
        error.ForestKeyRefused => error.ForestKeyRefused,
        error.OutOfMemory => error.OutOfMemory,
    };
}

fn mapTreeError(e: tree.Error) Error {
    return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        error.Io => error.Io,
        error.RetryBudgetExhausted => error.RetryBudgetExhausted,
        error.PartitionRefused => error.PartitionRefused,
        error.ValueTooLarge => error.ValueTooLarge,
        error.WouldBlock => error.WouldBlock,
        else => error.Corrupt,
    };
}

// ---------------------------------------------------------------------------
// Observability (survey §5.1 names).
// ---------------------------------------------------------------------------

/// `meta_commit_group_size`: txs per drained batch — buckets 1–8 exact,
/// ≤16, ≤32, ≤64, >64 (the reference's 12). `median_lower_bound ≥ 4` under
/// an 8-writer strict storm is the G3 gate; ≈ 1 = the conveyor regressed.
pub const GroupSizeHistogram = struct {
    pub const buckets = 12;
    pub const labels = [buckets][]const u8{ "1", "2", "3", "4", "5", "6", "7", "8", "le16", "le32", "le64", "gt64" };
    cells: [buckets]counters.ShardedCounter = @splat(.{}),

    pub fn bucketOf(n: usize) usize {
        if (n <= 8) return @max(n, 1) - 1;
        if (n <= 16) return 8;
        if (n <= 32) return 9;
        if (n <= 64) return 10;
        return 11;
    }

    pub fn record(self: *GroupSizeHistogram, n: usize) void {
        self.cells[bucketOf(n)].inc();
    }

    /// Σ over the buckets (≡ `leader_passes`).
    pub fn total(self: *const GroupSizeHistogram) u64 {
        var sum: u64 = 0;
        for (&self.cells) |*c| sum += c.load();
        return sum;
    }

    pub fn load(self: *const GroupSizeHistogram, bucket: usize) u64 {
        return self.cells[bucket].load();
    }
};

/// Per-volume faces (the stats inode folds them per mount).
pub const Stats = struct {
    group_size: GroupSizeHistogram = .{},
    /// `meta_commit_group_bytes`: Σ entry bytes drained.
    group_bytes: counters.ShardedCounter = .{},
    /// `meta_conveyor_leader_passes`: one per drained batch.
    leader_passes: counters.ShardedCounter = .{},
    /// `meta_conveyor_group_commits` / `_group_txs`: `enqueueMany` groups
    /// and their members (`txs ÷ commits` = the live group size).
    group_commits: counters.ShardedCounter = .{},
    group_txs: counters.ShardedCounter = .{},
    /// `meta_conveyor_lane_handoffs`: windows handed to stage B (≡ the
    /// lane's `lane_writes` on a lane mount — the engagement identity).
    lane_handoffs: counters.ShardedCounter = .{},
    /// `meta_conveyor_windows_inflight` / `_hwm`: windows between handoff
    /// and terminal outcome; HWM ≥ 2 under load is the D-2 engagement
    /// instrument.
    windows_inflight: counters.Gauge = .{},
    windows_inflight_hwm: counters.Gauge = .{},
    /// `meta_conveyor_queued`: entries queued and not drained (the wedge
    /// census; the core maintains it).
    queued: counters.Gauge = .{},
    /// `meta_commit_parked`: committers parked on their oneshot right now.
    commit_parked: counters.Gauge = .{},
    /// `meta_conveyor_drained_txs`: members drained into passes;
    /// `meta_conveyor_acks`: members answered Ok; `meta_conveyor_refusals`:
    /// members answered an error (any class). `acks ≡ drained − refusals`
    /// at quiesce (a build-time refusal never enters the queue).
    drained_txs: counters.ShardedCounter = .{},
    acks: counters.ShardedCounter = .{},
    refusals: counters.ShardedCounter = .{},
    /// `meta_conveyor_room_parks`: committers that found every queue cell
    /// taken and parked for room (one per committer, however many wakes
    /// its wait spans) — a burst past `queueCapacity`, never a refusal.
    commit_room_parks: counters.ShardedCounter = .{},
    /// `meta_kv_journal_full_stalls`: ring-admission parks (one per park
    /// entry); steady growth ⇒ the checkpoint lags.
    full_stalls: counters.ShardedCounter = .{},
    /// `meta_conveyor_window_gate_parks`: passes parked on the lane's
    /// window cells (`awaitWindowCell`: a flight of `window_cells − 1`) —
    /// the lane, not the ring, was the bound; the next window batches what
    /// queued meanwhile.
    window_gate_parks: counters.ShardedCounter = .{},
    /// `meta_conveyor_park_tick_rescues`: a backstop tick that found the
    /// work already there with no wake delivered — the admission park's
    /// 10 ms tick finding ring room, or the pass VENUE's 2 s tick finding
    /// the leader latch set (a lost election wake; `backend.zig`
    /// `passEntry`). ≈ 0 healthy: every room-making barrier and every
    /// election wakes its waiter itself.
    park_tick_rescues: counters.ShardedCounter = .{},
    /// `manager_dependency_stalls` (charter §5.5): a grant-fed conveyor's
    /// heap refusal answered `GrantExhausted` — the appender out of grant
    /// with its manager unable to refill. Must stay 0 at the sized grant
    /// (the pool holds a failover's worth of images at the measured rate).
    manager_dependency_stalls: counters.ShardedCounter = .{},
    /// RAM ≠ replay: a roll-out, rollback or compensation that could not
    /// complete (allocation, resolution, a lock budget) left RAM holding
    /// what the ring never will (or the reverse). Each fires once per
    /// exit and latches the volume FAILED (`divergedFromReplay`). On a
    /// volume ALREADY FAILED the same exit is the fail-stop's consequence
    /// (the rollback's own writes meet the dead device): counted as
    /// `meta_conveyor_divergences_failed_volume`, never the tripwire
    /// (owner ruling 2026-10-19).
    divergence_rollout: counters.Tripwire = .init("conveyor_divergence_rollout"),
    divergence_rollback: counters.Tripwire = .init("conveyor_divergence_rollback"),
    divergence_compensation: counters.Tripwire = .init("conveyor_divergence_compensation"),
    divergences_failed_volume: counters.ShardedCounter = .{},
    /// `meta_conveyor_compensation_write_failures`: the compensation
    /// entry's device write failed — the fail-stop lattice's own event
    /// (FAILED at once), a device outcome and never a tripwire.
    compensation_write_failures: counters.ShardedCounter = .{},
    /// `meta_conveyor_windows_abandoned_failed_volume`: a window the lane
    /// refused `VolumeFailed` — the latch landed between the pass's screen
    /// and the submit; rolled back like a failed write, never
    /// `meta_conveyor_pass_panics`.
    windows_abandoned_failed_volume: counters.ShardedCounter = .{},
    /// `meta_conveyor_rollbacks`: windows rolled back after a failed write;
    /// `meta_conveyor_compensations`: escaped records compensated.
    rollbacks: counters.ShardedCounter = .{},
    compensations: counters.ShardedCounter = .{},
    /// The window-fill hold (decision 0003 §L). `meta_conveyor_holds`:
    /// passes that parked on the flight; `_hold_landings` /
    /// `_hold_deadlines`: how the park ended (the flight emptied, or the
    /// round-trip predictor's deadline — a deadline is the device running
    /// late, counted so the pacing law's slack is visible). Windows at
    /// handoff by class: `meta_conveyor_windows_held` (issued after a
    /// hold), `_windows_unshadowed` (pad-majority, not held: nothing in
    /// flight — a lone committer's — or the worth gate declined),
    /// `_windows_pipelined` (entry-majority, issued beside whatever is in
    /// flight). The pacing law: `held + unshadowed − holds_not_worth ≤
    /// landings + deadlines + 1`, so pad-majority windows the gate admits
    /// are at most one per round trip. `meta_conveyor_hold_bound_ns`: the
    /// live deadline (`srtt + 4·rttvar`, capped).
    holds: counters.ShardedCounter = .{},
    hold_landings: counters.ShardedCounter = .{},
    hold_deadlines: counters.ShardedCounter = .{},
    /// `meta_conveyor_holds_not_worth`: pad-majority batches issued at once
    /// with a window in flight because the measured gain said a hold
    /// collects nothing (`holdWorthIt`) — the QD1 shapes' windows;
    /// `meta_conveyor_hold_probes`: the holds paid while the gate was off
    /// to re-measure it (one in `HOLD_PROBE_PERIOD`); `meta_conveyor_hold_
    /// gain_milli`: the live gain (entries collected per hold × 1000).
    holds_not_worth: counters.ShardedCounter = .{},
    hold_probes: counters.ShardedCounter = .{},
    /// `meta_conveyor_holds_pacing`: holds for the applier venue's width
    /// (§AW) — the windows in apply, not the pad, held the batch.
    holds_pacing: counters.ShardedCounter = .{},
    hold_gain_milli: counters.Gauge = .{},
    windows_held: counters.ShardedCounter = .{},
    windows_unshadowed: counters.ShardedCounter = .{},
    windows_pipelined: counters.ShardedCounter = .{},
    hold_bound_ns: counters.Gauge = .{},
    /// `meta_conveyor_resolve_seeded` / `_descended`: the applier's leaf
    /// resolutions served by the committer's heap-admission resolve (the
    /// leaf still mapped and covering the key) against those that
    /// descended the tree (no seed; the leaf superseded, split or moved).
    resolve_seeded: counters.ShardedCounter = .{},
    resolve_descended: counters.ShardedCounter = .{},
    /// `meta_conveyor_acks_at_apply` / `_at_landing`: windows answered by
    /// the applier ahead of the landing (§BB.1: the coalesced cadence, every
    /// member this mount's kernel's own op, no apply hole, a tail slot)
    /// against windows answered by the lane's verdict (strict mode; a
    /// wire-served, control, job or tool commit; a window with an apply
    /// hole; a tail slot missed; a verdict that beat the applier's answer).
    acks_at_apply: counters.ShardedCounter = .{},
    acks_at_landing: counters.ShardedCounter = .{},
    /// `meta_conveyor_self_handoffs` / `meta_conveyor_self_applies`:
    /// windows whose members applied themselves (§BC), and the members.
    self_handoffs: counters.ShardedCounter = .{},
    self_applies: counters.ShardedCounter = .{},
    /// `meta_conveyor_pass_yields`: lane turns the pass gave between two
    /// of its batches (`PassSeam.yield`).
    pass_yields: counters.ShardedCounter = .{},
    /// `meta_conveyor_leafwise_applies` / `_leaves`: self-applied members
    /// that took their leaves one at a time (§BV: a corpse's destroy), and
    /// the leaves they took (`leaves ÷ applies` = a destroy's spread).
    leafwise_applies: counters.ShardedCounter = .{},
    leafwise_leaves: counters.ShardedCounter = .{},
    /// `meta_conveyor_heap_admit_fast` / `_locked`: heap admissions by
    /// the lock-free fast path (§BD) against the locked law.
    heap_admit_fast: counters.ShardedCounter = .{},
    heap_admit_locked: counters.ShardedCounter = .{},
    /// `meta_conveyor_turn_spin_hits` / `_falls`: turns found by the
    /// committer's spin against those that parked (§BC).
    turn_spin_hits: counters.ShardedCounter = .{},
    turn_spin_falls: counters.ShardedCounter = .{},
    /// `meta_conveyor_acked_write_failures`: a journal write that failed
    /// AFTER its window was acked (§BB.5): no rollback, the volume FAILED.
    acked_write_failures: counters.ShardedCounter = .{},
    /// `meta_conveyor_tail_slot_misses` ★: the tail's slot table refused a
    /// window (it holds twice the lane's cells; the window is acked at
    /// the landing instead, so nothing acked escapes the device alone).
    tail_slot_misses: counters.Tripwire = .init("meta_conveyor_tail_slot_misses"),
    /// The committer's ack spin (decision 0003 §AB.4): acks that landed
    /// inside the spin (no park, no wake), spins that fell through to the
    /// park, parks with no spin: no bound (no wake measured yet, or an ack
    /// predicted past it) / work queued on the caller's lane (or off a
    /// lane); the bound last used. Closure: hits + falls + skips + busy ≡
    /// parks.
    ack_spin_hits: counters.ShardedCounter = .{},
    ack_spin_falls: counters.ShardedCounter = .{},
    ack_spin_skips: counters.ShardedCounter = .{},
    ack_spin_busy: counters.ShardedCounter = .{},
    ack_spin_bound_ns: counters.Gauge = .{},
    /// The rule's two inputs (RFC 6298 means): ack → resumed on an idle
    /// park (W) and enqueue → ack.
    ack_spin_wake_ns: counters.Gauge = .{},
    ack_spin_ack_ns: counters.Gauge = .{},
    /// `meta_conveyor_pass_panics` ★: a window abandoned without its
    /// outcome observed on a HEALTHY volume (every member failed loud, the
    /// reservation completed as abandoned).
    pass_panics: counters.Tripwire = .init("meta_conveyor_pass_panics"),
    /// `meta_txpass_phase_ns`: queue residence per tx, the Σ admission, the
    /// union-lock window, the pass (drain → handoff), the window (handoff →
    /// terminal outcome).
    tx_queue_wait: counters.ShardedLatencyHistogram = .{},
    pass_admission: counters.ShardedLatencyHistogram = .{},
    pass_leaf_locks: counters.ShardedLatencyHistogram = .{},
    /// `pass_token_recall`: A1½ — the holder's recall of the batch's
    /// token union, before ring admission (so `pass_total` decomposes).
    pass_token_recall: counters.ShardedLatencyHistogram = .{},
    /// `pass_hold`: the window-fill hold's park, per held pass (the §L
    /// latency the law spends; its tail sits under `window_total`'s).
    pass_hold: counters.ShardedLatencyHistogram = .{},
    pass_total: counters.ShardedLatencyHistogram = .{},
    /// `apply_total`: the applier's leg (§AW) — the pass's handoff → the
    /// window submitted (resolve, the union lock, the apply, the encode).
    apply_total: counters.ShardedLatencyHistogram = .{},
    window_total: counters.ShardedLatencyHistogram = .{},
    /// `lock_phase_ns.leaf_lock_hold`: first acquire → every guard dropped
    /// (its tail must sit ≪ device latency — the "never across I/O" law).
    leaf_lock_hold: counters.ShardedLatencyHistogram = .{},
    /// `lock_phase_ns.leaf_takes_{admission,apply}`: every leaf write lock
    /// taken, by phase, with the wait to acquire it — the leaf set of an
    /// op (a create: two takes at the apply, none at the admission on a
    /// volume with room, §BD) and the serializer's cost (§BC).
    leaf_takes_admission: counters.ShardedLatencyHistogram = .{},
    leaf_takes_apply: counters.ShardedLatencyHistogram = .{},
    /// `lock_phase_ns.leafwise_leaf_hold`: one leaf a leafwise member held
    /// (§BV) — the most a committer on that leaf waits behind a destroy.
    leafwise_leaf_hold: counters.ShardedLatencyHistogram = .{},
    /// `meta_leafwise_apply`: a leafwise member's whole apply, its first
    /// take to its last release (its waits included, no leaf held across
    /// one).
    leafwise_apply: counters.ShardedLatencyHistogram = .{},
    /// `meta_self_window_span`: a self-applied window's turns to its submit
    /// (§BC) — how long its slowest member keeps the reservation, and so
    /// the completed prefix every later window's verdict waits on, open.
    self_window_span: counters.ShardedLatencyHistogram = .{},

    pub fn writeJson(self: *const Stats, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"meta_commit_group_size\":{{", .{});
        for (GroupSizeHistogram.labels, 0..) |l, i| {
            try w.print("{s}\"{s}\":{d}", .{ if (i == 0) "" else ",", l, self.group_size.load(i) });
        }
        try w.print("}},\"meta_commit_group_bytes\":{d},\"meta_conveyor_leader_passes\":{d},\"meta_conveyor_group_commits\":{d},\"meta_conveyor_group_txs\":{d},\"meta_conveyor_lane_handoffs\":{d},\"meta_conveyor_windows_inflight\":{d},\"meta_conveyor_windows_inflight_hwm\":{d},\"meta_conveyor_queued\":{d},\"meta_commit_parked\":{d},\"meta_conveyor_drained_txs\":{d},\"meta_conveyor_acks\":{d},\"meta_conveyor_refusals\":{d},\"meta_kv_journal_full_stalls\":{d},\"meta_conveyor_park_tick_rescues\":{d},\"manager_dependency_stalls\":{d},\"meta_conveyor_rollbacks\":{d},\"meta_conveyor_compensations\":{d},\"meta_conveyor_divergences_failed_volume\":{d},\"meta_conveyor_compensation_write_failures\":{d},\"meta_conveyor_pass_panics\":{d},\"meta_conveyor_windows_abandoned_failed_volume\":{d},\"meta_conveyor_holds\":{d},\"meta_conveyor_hold_landings\":{d},\"meta_conveyor_hold_deadlines\":{d},\"meta_conveyor_holds_not_worth\":{d},\"meta_conveyor_hold_probes\":{d},\"meta_conveyor_hold_gain_milli\":{d},\"meta_conveyor_windows_held\":{d},\"meta_conveyor_windows_unshadowed\":{d},\"meta_conveyor_windows_pipelined\":{d},\"meta_conveyor_hold_bound_ns\":{d},\"meta_conveyor_holds_pacing\":{d}", .{
            self.group_bytes.load(),
            self.leader_passes.load(),
            self.group_commits.load(),
            self.group_txs.load(),
            self.lane_handoffs.load(),
            self.windows_inflight.load(),
            self.windows_inflight_hwm.load(),
            self.queued.load(),
            self.commit_parked.load(),
            self.drained_txs.load(),
            self.acks.load(),
            self.refusals.load(),
            self.full_stalls.load(),
            self.park_tick_rescues.load(),
            self.manager_dependency_stalls.load(),
            self.rollbacks.load(),
            self.compensations.load(),
            self.divergences_failed_volume.load(),
            self.compensation_write_failures.load(),
            self.pass_panics.load(),
            self.windows_abandoned_failed_volume.load(),
            self.holds.load(),
            self.hold_landings.load(),
            self.hold_deadlines.load(),
            self.holds_not_worth.load(),
            self.hold_probes.load(),
            self.hold_gain_milli.load(),
            self.windows_held.load(),
            self.windows_unshadowed.load(),
            self.windows_pipelined.load(),
            self.hold_bound_ns.load(),
            self.holds_pacing.load(),
        });
        try w.print(",\"meta_conveyor_resolve_seeded\":{d},\"meta_conveyor_resolve_descended\":{d},\"meta_conveyor_acks_at_apply\":{d},\"meta_conveyor_acks_at_landing\":{d},\"meta_conveyor_acked_write_failures\":{d},\"meta_conveyor_tail_slot_misses\":{d},\"meta_conveyor_self_handoffs\":{d},\"meta_conveyor_self_applies\":{d},\"meta_conveyor_leafwise_applies\":{d},\"meta_conveyor_leafwise_leaves\":{d},\"meta_conveyor_pass_yields\":{d},\"meta_conveyor_turn_spin_hits\":{d},\"meta_conveyor_turn_spin_falls\":{d},\"meta_conveyor_heap_admit_fast\":{d},\"meta_conveyor_heap_admit_locked\":{d}", .{ self.resolve_seeded.load(), self.resolve_descended.load(), self.acks_at_apply.load(), self.acks_at_landing.load(), self.acked_write_failures.load(), self.tail_slot_misses.load(), self.self_handoffs.load(), self.self_applies.load(), self.leafwise_applies.load(), self.leafwise_leaves.load(), self.pass_yields.load(), self.turn_spin_hits.load(), self.turn_spin_falls.load(), self.heap_admit_fast.load(), self.heap_admit_locked.load() });
        try w.print(",\"meta_conveyor_window_gate_parks\":{d},\"meta_conveyor_room_parks\":{d},\"meta_commit_ack_spin_hits\":{d},\"meta_commit_ack_spin_falls\":{d},\"meta_commit_ack_spin_skips\":{d},\"meta_commit_ack_spin_busy\":{d},\"meta_commit_ack_spin_bound_ns\":{d},\"meta_commit_ack_spin_wake_ns\":{d},\"meta_commit_ack_spin_ack_ns\":{d}}}", .{
            self.window_gate_parks.load(),
            self.commit_room_parks.load(),
            self.ack_spin_hits.load(),
            self.ack_spin_falls.load(),
            self.ack_spin_skips.load(),
            self.ack_spin_busy.load(),
            self.ack_spin_bound_ns.load(),
            self.ack_spin_wake_ns.load(),
            self.ack_spin_ack_ns.load(),
        });
    }

    /// `meta_txpass_phase_ns` + `lock_phase_ns.leaf_lock_hold`: the
    /// residence histograms (a stalled commit names its leg here).
    pub fn writePhasesJson(self: *const Stats, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.writeAll("{\"meta_tx_queue_wait\":");
        try self.tx_queue_wait.snapshot().writeJson(w);
        try w.writeAll(",\"meta_pass_admission\":");
        try self.pass_admission.snapshot().writeJson(w);
        try w.writeAll(",\"meta_pass_leaf_locks\":");
        try self.pass_leaf_locks.snapshot().writeJson(w);
        try w.writeAll(",\"meta_pass_token_recall\":");
        try self.pass_token_recall.snapshot().writeJson(w);
        try w.writeAll(",\"meta_pass_hold\":");
        try self.pass_hold.snapshot().writeJson(w);
        try w.writeAll(",\"meta_pass_total\":");
        try self.pass_total.snapshot().writeJson(w);
        try w.writeAll(",\"meta_apply_total\":");
        try self.apply_total.snapshot().writeJson(w);
        try w.writeAll(",\"meta_window_total\":");
        try self.window_total.snapshot().writeJson(w);
        try w.writeAll(",\"leaf_lock_hold\":");
        try self.leaf_lock_hold.snapshot().writeJson(w);
        try w.writeAll(",\"leaf_takes_admission\":");
        try self.leaf_takes_admission.snapshot().writeJson(w);
        try w.writeAll(",\"leaf_takes_apply\":");
        try self.leaf_takes_apply.snapshot().writeJson(w);
        try w.writeAll(",\"leafwise_leaf_hold\":");
        try self.leafwise_leaf_hold.snapshot().writeJson(w);
        try w.writeAll(",\"meta_leafwise_apply\":");
        try self.leafwise_apply.snapshot().writeJson(w);
        try w.writeAll(",\"meta_self_window_span\":");
        try self.self_window_span.snapshot().writeJson(w);
        try w.writeAll("}");
    }
};

// ---------------------------------------------------------------------------
// Test seams (one relaxed load each when unarmed; never a sleep).
// ---------------------------------------------------------------------------

pub const TestSeams = struct {
    /// Fail ONE member's apply after it applied (the in-window roll-out
    /// machinery runs for real): the member whose records name this inode.
    poison_apply_ino: std.atomic.Value(u64) = .init(0),
    /// 1 ⇒ a pass parked at ring admission holds between its registration
    /// on `space` and its wait (the window a wake must reach); `park_held`
    /// publishes the hold.
    hold_at_park: std.atomic.Value(u32) = .init(0),
    park_held: std.atomic.Value(u32) = .init(0),
    /// N ⇒ the next N election winners' wakes of the pass venue are DROPPED
    /// (the lost-wake shape the venue's tick backstop must absorb and
    /// `park_tick_rescues` must count).
    drop_pass_wakes: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ `Done.send` holds right after its channel send (the receiver
    /// may take the outcome and leave); `send_held` publishes the hold.
    hold_after_send: std.atomic.Value(u32) = .init(0),
    send_held: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ the NEXT window's applier parks before its resolve (2 while it
    /// holds; `apply_held` publishes it) until the test stores 0 and
    /// notifies `apply_release` — the §AW.4 shape pinned: a later window
    /// applies, a freeze runs, THEN the held window lands its older seqs.
    hold_next_apply: std.atomic.Value(u32) = .init(0),
    apply_held: std.atomic.Value(u32) = .init(0),
    apply_release: sync.Notify = .init,
};

pub var test_seams: TestSeams = .{};

// ---------------------------------------------------------------------------
// Queue entries, windows, the seams.
// ---------------------------------------------------------------------------

pub const Outcome = Error!void;
/// The completion channel per tx: the committer parks on it; the lane
/// sends the terminal outcome after dropping the guards.
/// A committer's channel: the outcome, and the instant the lane answered
/// (stored BEFORE the send — non-zero ⇒ the outcome is on its way). The
/// committer's spin polls it lock-free; its park learns the ack → resume
/// wake from it (decision 0003 §AB.4).
pub const Done = struct {
    ch: sync.Oneshot(Signal) = .init,
    /// The terminal outcome of a member that applied itself and waits for
    /// the landing (§BC: a served verb, a control record, a tool).
    landing: sync.Oneshot(Outcome) = .init,
    acked_ns: std.atomic.Value(u64) = .init(0),
    /// `ch` carried the turn: the terminal outcome goes to `landing`.
    turned: bool = false,

    pub const init: Done = .{};

    /// What the committer's park receives: its terminal outcome, or its
    /// TURN — the window is reserved, its seq stamped; the committer
    /// applies its own transaction (`Conveyor.applyOwn`, §BC).
    pub const Signal = union(enum) {
        outcome: Outcome,
        turn: Turn,
    };

    pub const Turn = struct { w: *Window, idx: u32 };

    pub fn send(self: *Done, io: Io, outcome: Outcome) error{ReceiverGone}!void {
        // The stamp BEFORE the send: once the channel holds the outcome the
        // receiver may take it and return, and its Done (a frame of its
        // stack) is no longer ours to write. Release: pairs with the
        // spinner's acquire — a non-zero stamp is an outcome on its way
        // (the receiver's recv waits out the channel's lock for it).
        self.acked_ns.store(@max(clock.monotonicNs(), 1), .release);
        if (self.turned) {
            try self.landing.send(io, outcome);
        } else {
            try self.ch.send(io, .{ .outcome = outcome });
        }
        holdAfterSend();
    }

    /// The pass hands the member its turn (§BC); `send` after it answers
    /// through `landing`.
    pub fn sendTurn(self: *Done, io: Io, w: *Window, idx: u32) error{ReceiverGone}!void {
        self.turned = true;
        self.acked_ns.store(@max(clock.monotonicNs(), 1), .release);
        try self.ch.send(io, .{ .turn = .{ .w = w, .idx = idx } });
    }

    fn holdAfterSend() void {
        if (test_seams.hold_after_send.load(.acquire) == 0) return;
        test_seams.send_held.store(1, .release);
        while (test_seams.hold_after_send.load(.acquire) != 0) std.Thread.yield() catch {};
    }

    pub fn recvUncancelable(self: *Done, io: Io) error{SenderGone}!Signal {
        return self.ch.recvUncancelable(io);
    }

    /// The terminal outcome of a member on the applier venue (the
    /// contracts' receivers: no turn is ever sent to them).
    pub fn recvOutcome(self: *Done, io: Io) error{SenderGone}!Outcome {
        return switch (try self.ch.recvUncancelable(io)) {
            .outcome => |o| o,
            .turn => unreachable,
        };
    }
};

/// The committer's bounded spin before it parks on its ack (decision 0003
/// §AB.4). A park costs one cross-thread wake when the ack lands (the
/// journal lane writes the committer lane's eventfd, the lane is scheduled
/// — measured 17.5 µs of a 90 µs QD1 create on the dev container); a spin
/// costs CPU. The bound is the classic spin-then-park competitive rule on
/// the MEASURED costs: spin at most `2 × W` (W = the ack → resume wake's
/// RFC 6298 mean — the CPU a spin may burn is at most twice the wake it
/// saves), and only while the ack's predicted latency (`srtt` of enqueue →
/// ack) is inside that bound — an ack predicted later would make every spin
/// a fall-through. No wake measured yet (W = 0) ⇒ no spin: the first parks
/// measure it. W is sampled only on a park that left its lane idle and
/// whose resume was the lane's next poll (another fiber run between puts
/// its work in the gap — measured: W read 252 µs instead of 18 µs and the
/// bound let a committer burn half a millisecond); fall-through parks keep
/// measuring it.
pub const AckSpin = struct {
    /// Enqueue → ack (sampled by the lane at the answer).
    ack: RttEstimator = .{},
    /// Ack → the committer resumed after a park.
    wake: RttEstimator = .{},
    /// Enqueue → the TURN (§BC): the pass's drain, recall, admission and
    /// reservation ahead of this member — what the turn spin waits out.
    turn: RttEstimator = .{},

    pub const Reading = struct { wake_ns: u64, ack_ns: u64, bound_ns: u64 };

    /// The two means (0 before a sample) and the bound they give.
    pub fn read(self: *const AckSpin) Reading {
        const w = self.wake.mean() orelse 0;
        const a = self.ack.mean() orelse 0;
        return .{ .wake_ns = w, .ack_ns = a, .bound_ns = spinBound(w, a) };
    }
};

/// Spin on `done` for at most `bound_ns`; true = the ack landed inside it
/// (its outcome is on its way: a `recv` takes it, at worst after the
/// sender's channel lock).
pub fn spinForAck(done: *const Done, bound_ns: u64) bool {
    const t0 = clock.monotonicNs();
    while (done.acked_ns.load(.acquire) == 0) {
        if (clock.monotonicNs() -% t0 >= bound_ns) return done.acked_ns.load(.acquire) != 0;
        std.atomic.spinLoopHint();
    }
    return true;
}

/// §BC: the turn spin's cap — twice the park/resume wake measured for
/// §AB.4 (17.5 µs of a QD1 create): the CPU a spin may burn is at most
/// twice the wake it saves, and a committer waiting for its turn on a busy
/// FUSE lane has no idle park to measure the wake with. A send to a
/// spinning receiver costs the pass no wake syscall (a parked waiter alone
/// is woken), so the spin pays on both sides of the turn.
pub const TURN_SPIN_CAP_NS: u64 = 2 * 17_500;

/// The pure rule (tie-tested): twice the turn's predicted latency, within
/// the cap; the cap itself before the first sample (it bootstraps the
/// predictor); a turn predicted past the cap is parked for (every spin
/// would fall through).
pub fn turnSpinBound(turn_srtt_ns: ?u64) u64 {
    const t = turn_srtt_ns orelse return TURN_SPIN_CAP_NS;
    if (t > TURN_SPIN_CAP_NS) return 0;
    return @min(2 * t, TURN_SPIN_CAP_NS);
}

/// The pure rule (tie-tested): `2·W` when the ack's mean is within it, else 0.
pub fn spinBound(wake_srtt_ns: u64, ack_srtt_ns: u64) u64 {
    if (wake_srtt_ns == 0) return 0;
    const s = 2 * wake_srtt_ns;
    return if (ack_srtt_ns <= s) s else 0;
}

/// One queue entry: the tx, its channel, its enqueue instant.
pub const Queued = struct {
    q: QueuedTx,
    done: *Done,
    enqueued_ns: u64,
    /// Per record, the leaf the committer's heap admission resolved it to
    /// (a retained reference; in `q`'s arena): the applier's resolve starts
    /// here and descends only for a leaf superseded, split or moved since.
    seeds: []*CachedNode = &.{},
    /// A `commitGroup` member: its committer parks on its siblings in
    /// turn, so it never applies itself (§BC).
    grouped: bool = false,
    /// The terminal outcome's teardown: the seeds' references, then the tx.
    fn deinit(self: *Queued) void {
        for (self.seeds) |n| n.release();
        self.q.deinit();
    }
};

const Core = conveyor_core.ConveyorCore(Queued);

/// A reserved batch: the pass's from the reservation to the handoff, its
/// APPLIER's (the first member's committer, §AW) through the RAM apply,
/// the encode and the submit, then in the lane's hands until `finish`
/// (the reference's `ConveyorWindow`). Heap-allocated: the lane and the
/// hooks address it by pointer; its arena owns the members' copies and the
/// undo pre-images — the pass writes it before the handoff, the applier
/// after, never both.
pub const Window = struct {
    conveyor: *Conveyor,
    arena: std.heap.ArenaAllocator,
    /// Queue order = journal order.
    members: []Queued,
    /// Per member: the in-window apply failure that rolled it out (its
    /// sub-range is an unwritten hole), else null.
    failed: []?Error,
    res: Reservation,
    seq_base: u64,
    undo: []tx.UndoKey,
    /// The survivors' aligned runs (the DMA source; alive until `finish`)
    /// and their device accounting; null when nothing survived the apply.
    image: ?journal.WriteImage,
    /// Surviving entries (the lane's accounting).
    entries: u64,
    t_handoff_ns: u64,
    /// The pass's drain instant (the applier's phase accounting).
    t_pass_ns: u64 = 0,
    /// The window-fill hold preceded the batch (the handoff's class).
    held: bool = false,
    /// The applier venue's affinity (§AW): the tree of the first member's
    /// LAST record — a namespace op stages its parent-side record last,
    /// the object every sibling's op shares — so windows on one hot leaf
    /// take one applier lane's FIFO (an in-order apply, no leaf-lock
    /// handoff between lanes) while windows on different directories
    /// spread over the venue.
    affinity: u64 = 0,
    /// The token union the pass took in flight at A1½ (arena-owned);
    /// settled at the terminal outcome.
    token_union: []const u64 = &.{},
    /// Who answers the members (§BB.1): the applier ahead of the landing,
    /// or the lane at its verdict — ONE of them, decided by a CAS from
    /// `none`; the lane's rollback claims it before it rolls anything
    /// back, so a window acked is never rolled back.
    ack_owner: std.atomic.Value(u8) = .init(ACK_NONE),
    /// The tail slot holding the window's image (§BB.2); released at the
    /// lane's verdict.
    tail_slot: ?u32 = null,
    /// Parties that must leave before the window is freed: the lane's
    /// verdict, plus the applier while it answers ahead of the landing.
    parties: std.atomic.Value(u8) = .init(1),
    /// §BC: the members applied themselves (one `Part` each, arena-owned);
    /// the last to finish built the image and submitted. `pending` counts
    /// the members still applying.
    self_applied: bool = false,
    parts: []Part = &.{},
    pending: std.atomic.Value(u32) = .init(0),
    /// The pass handed the members their turns (`meta_self_window_span`).
    t_turns_ns: u64 = 0,

    /// One self-applying member's share of the window (§BC): its own
    /// arena (the members apply concurrently), its sub-range and seq, its
    /// ops (the window's image is their concatenation), its pre-images,
    /// its tail slot, and whether it answered itself at the apply.
    pub const Part = struct {
        arena: std.heap.ArenaAllocator,
        start: u64,
        seq: u64,
        /// The member's entry length (its `QueuedTx` dies with its ack).
        len: u64,
        ops: []const journal.WriteOp = &.{},
        header_bytes: u64 = 0,
        undo: []tx.UndoKey = &.{},
        tail_slot: ?u32 = null,
        answered: bool = false,
    };

    /// A self-applied window with a member answered ahead of the landing:
    /// nothing in it may be rolled back (§BB.5).
    fn anyAnswered(self: *const Window) bool {
        for (self.parts) |p| if (p.answered) return true;
        return false;
    }

    pub const ACK_NONE: u8 = 0;
    pub const ACK_APPLIER: u8 = 1;
    pub const ACK_LANE: u8 = 2;

    /// Claim the answering role for `who`; true if this call won it.
    fn claimAck(self: *Window, who: u8) bool {
        // acq_rel: the winner's answer is ordered after the loser's last
        // write to the members; the loser sees the winner's claim.
        return self.ack_owner.cmpxchgStrong(ACK_NONE, who, .acq_rel, .acquire) == null;
    }

    /// One party done with the window; the last frees it.
    fn release(self: *Window) void {
        // acq_rel: the last party sees every other party's writes.
        if (self.parties.fetchSub(1, .acq_rel) == 1) {
            if (self.image) |*img| img.deinit();
            for (self.parts) |*p| p.arena.deinit();
            self.arena.deinit();
            self.conveyor.deps.gpa.destroy(self);
        }
    }

    pub fn span(self: *const Window) SeqSpan {
        return SeqSpan.stamped(self.seq_base, self.res.len);
    }

    /// Whether a member was rolled out at apply leaving a HOLE the
    /// survivors' ack must wait a checkpoint past. A self-applied window
    /// (§BC) writes a PAD entry over a rolled-out member's range — a chain
    /// link, never a hole.
    pub fn applyHoles(self: *const Window) bool {
        if (self.self_applied) return false;
        for (self.failed) |f| if (f != null) return true;
        return false;
    }

    /// The lane's write ops (empty when nothing survived).
    pub fn ops(self: *const Window) []const journal.WriteOp {
        return if (self.image) |*img| img.ops else &.{};
    }

    /// The image's device accounting (zero when nothing survived).
    pub fn acct(self: *const Window) journal.WriteAccounting {
        return if (self.image) |*img| img.acct else .{};
    }

    /// The terminal outcome (the lane's verdict for the window): fan out in
    /// journal order — guards drop BEFORE the ack — and free the window.
    pub fn finish(self: *Window, result: Error!void) void {
        self.conveyor.finishWindow(self, result);
    }
};

/// The delta tee a slot migration arms on the volume (charter §5.4
/// "delta tee in the conveyor pass"): `note` sees the records of every
/// tx that reached its terminal SUCCESS — the lane IS the key tee, so one
/// armed-tee load per window captures every migrating-slot key at zero
/// cost when no migration runs (`routing/migration_tee.zig` is the one
/// binding; the hook keeps the conveyor independent of it).
pub const CommitTee = struct {
    ctx: *anyopaque,
    note: *const fn (ctx: *anyopaque, recs: []const tx.Rec) void,
};

pub const DoorError = error{ SlotBusy, SlotMoved, Deferred, Io, Corrupt, OutOfMemory };

/// THE COMMIT DOOR (charter §5.5; `sym/slots/door.zig`): a token per
/// distinct slot the entry's records name, taken BEFORE the enqueue
/// (step 0 of the commit — before any ring admission or node lock) and
/// held in `QueuedTx.door` to the terminal outcome. The seam parks a
/// committer on its own slot's handover, refuses a foreign lease, and
/// acquires an unleased slot first-touch. Null = no door (a volume
/// without the plane; the portable contracts).
pub const DoorSeam = struct {
    ctx: *anyopaque,
    enter: *const fn (ctx: *anyopaque, recs: []const tx.Rec, out: *tx.DoorTokens) DoorError!void,
    /// A12: the member's records are stamped into the window ending at
    /// `pos` — the slot RECORD FRONTIER the handover's cover loop waits on
    /// (one store per slot per member, never per record).
    frontier: *const fn (ctx: *anyopaque, tokens: *const tx.DoorTokens, pos: u64) void,
};

/// THE TOKEN HOOK (charter §5.5 "the holder's conveyor pass recalls the
/// union of its batch's objects' tokens ONCE, under the batch's 4a
/// guards, BEFORE ring admission"; `sym/tokens/token_table.zig`): A1½ —
/// after the liveness flags, before `admitUserBudget` and any node lock
/// (a recall's round trip is never held under a region-growth Dekker or a
/// leaf lock). `recall` takes the union in flight and returns on every
/// ack or the reader's lease expiry; `settle` runs at the window's
/// TERMINAL outcome for both arms (fan-out AND rollback), so an object
/// leaves the flight with its LAST user. Unarmed = one acquire load per
/// pass (the null probe).
pub const TokenHook = struct {
    ctx: *anyopaque,
    /// `names`: the dentries the batch changes, so a reader drops exactly
    /// those from its kernel (decision 0003 §AB.2).
    recall: *const fn (ctx: *anyopaque, objects: []const u64, names: BatchNames) Allocator.Error!void,
    settle: *const fn (ctx: *anyopaque, objects: []const u64) void,
};

/// The names a batch changes (decision 0003 §AB.2): every member's notes;
/// `unnamed` = a member changed a dentry its notes do not name.
pub const BatchNames = token_table.RecallNames;

/// Whether a member changes a dentry its notes do not name.
fn unnamedDentries(q: *const tx.QueuedTx) bool {
    if (q.names.overflow) return true;
    if (q.names.len != 0) return false;
    for (q.recs) |r| if (r.tree_id == key.TREE_DENTRIES) return true;
    return false;
}

/// THIS mount's kernel as the conveyor sees it (decision 0003 §AB.2): a
/// change the kernel did not ask for — a peer's served step or ship, a job,
/// a tool (`!kernel_origin`), unless its own channel tells the kernel
/// (`kernel_told`) — and every member of a window that failed
/// (its records were readable in RAM until the rollback) is invalidated in
/// it at the window's terminal outcome: after the RAM apply, so a lookup
/// that follows reads the new records, and before the member's ack. The
/// FUSE layer arms it once its session is up.
pub const KernelView = struct {
    ctx: *anyopaque,
    /// One member: its names (`unnamed` ⇒ the whole dentry cache) and the
    /// objects whose inode or xattrs it changed (their attributes).
    invalidate: *const fn (ctx: *anyopaque, names: []const tx.NameNote, unnamed: bool, objects: []const u64) void,
};

/// The pass's venue: `wake` is called by the committer that won the
/// election and must make `runScheduledPass` run — on the journal lane's
/// pass fiber (the backend), inline on the caller (the portable contracts).
/// Never a suspension point for the caller.
pub const PassSeam = struct {
    ctx: *anyopaque,
    wake: *const fn (ctx: *anyopaque) void,
    /// Between two batches of one pass (more is queued): give the venue's
    /// thread one turn of whatever else it runs before the next drain — on
    /// the journal lane, its stage B (the windows' issues, the CQE reaps,
    /// the verdicts that release the tail's slots and settle the token
    /// unions). Leadership is held across it. Null = the pass's thread
    /// runs nothing else (the portable contracts, the committer venue).
    yield: ?*const fn (ctx: *anyopaque) void = null,
};

/// The applier venue (§AW): where a reserved window's second half — the
/// RAM apply, the encode, the submit — runs. `spawn` starts
/// `Conveyor.applyWindow(w)` as its own fiber and answers true; false =
/// the venue refuses (the pass applies inline). `parallelism` is the
/// venue's width: the pass holds a drained batch while that many windows
/// are in apply (another one would only queue behind them and contend
/// for their leaves). The backend binds the `sqz-meta` pool; the portable
/// contracts bind the inline applier (`inline_applier`).
pub const ApplySeam = struct {
    ctx: *anyopaque,
    spawn: *const fn (ctx: *anyopaque, w: *Window) bool,
    parallelism: u32,
};

/// The contracts' applier: refused, so the pass applies on its own thread
/// before it submits — stage A whole, as the reference ran it.
pub const inline_applier: ApplySeam = .{ .ctx = undefined, .spawn = &refuseSpawn, .parallelism = 1 };

fn refuseSpawn(_: *anyopaque, _: *Window) bool {
    return false;
}

/// The journal tail (decision 0003 §BB.2): the window's image copied into
/// the kernel-held mirror before its members are acked ahead of the
/// landing. `claim` returns the slot (null: the table refused), `release`
/// frees it at the lane's verdict. `pos_base` is the reservation's start
/// (the position a held tail is judged by against the ledger, §BR.3).
pub const TailSeam = struct {
    ctx: *anyopaque,
    claim: *const fn (ctx: *anyopaque, seq_base: u64, pos_base: u64, ops: []const journal.WriteOp) ?u32,
    release: *const fn (ctx: *anyopaque, slot: u32) void,
};

/// Stage B's binding: the journal lane (Linux) or the in-RAM lane.
pub const LaneSeam = struct {
    ctx: *anyopaque,
    /// Hand a window to stage B. The lane owns `w` until `w.finish`. An
    /// error answers synchronously with NOTHING queued (the caller abandons
    /// the window: reservation completed, records rolled back).
    submit: *const fn (ctx: *anyopaque, w: *Window) Error!void,
    /// `fsync`: the trailing coalesced barrier, answered once a barrier
    /// that started after the request completed.
    barrier: *const fn (ctx: *anyopaque) Error!void,
    /// The cells the lane holds for windows alone (`journal_lane.
    /// windowCells`; never a barrier's or a structural entry's): the pass
    /// keeps at most `window_cells − 1` windows in flight (`awaitWindowCell`).
    window_cells: usize,
};

/// The checkpoint task's faces the pass needs (the backend binds them to
/// `checkpoint_cycle`; the contracts to the test volume).
pub const CheckpointHooks = struct {
    ctx: *anyopaque,
    /// A committer parked at ring admission: the kick carrying the park
    /// mark (a cycle due with an immediate barrier).
    kickPark: *const fn (ctx: *anyopaque) void,
    /// A leaf crossed the writeback threshold: a maintenance pass now.
    kickThreshold: *const fn (ctx: *anyopaque) void,
    /// One barriered return-room cycle (the space retry); false = failed.
    checkpointNow: *const fn (ctx: *anyopaque) bool,
    /// Checkpoint past a hole ending at `pos` (zero ring bytes); false =
    /// the cycle could not drain (the volume escalates).
    checkpointPast: *const fn (ctx: *anyopaque, pos: u64) bool,
    /// Wakes a pass parked at ring admission when `reusable_upto` moves.
    space: *sync.Notify,
    posture: *heap_law.HeapFullPosture,
    space_retry: *heap_law.SpaceRetryHold,
};

pub const Deps = struct {
    io: Io,
    gpa: Allocator,
    /// The volume's name in log lines.
    volume: []const u8,
    /// The volume's LIVE tree set (a slot tree may be minted or migrated
    /// in or out during the mount; every pass routes through the current
    /// list).
    trees: *const tree.TreeTable,
    cache: *NodeCache,
    alloc: *const alloc_ext.ExtentAllocator,
    core: *jc.JournalCore,
    reg: *jc.InflightRegistry,
    ring_layout: journal.RingLayout,
    geo: jc.CoreGeometry,
    /// The appender the ring's page headers name (`journal.FIXED_RING_APPENDER`
    /// on the fixed ring).
    appender_id: u32,
    latch: *journal.FailureLatch,
    lane: LaneSeam,
    pass: PassSeam,
    checkpoint: CheckpointHooks,
    /// The checkpoint-class direct writer: the rollback's compensation
    /// entry (never parks behind user admissions; never the lane).
    smo_journal: *smo.SmoJournal,
    caps: Caps,
    /// `PARK_ESCALATION_NS`: the park escalation threshold.
    park_escalation_ns: u64,
    /// The commit door (null = none).
    door: ?DoorSeam = null,
    /// A JOINED appender's posture: the allocator is its grant mirror, so
    /// a heap refusal is `GrantExhausted` (EAGAIN) — never ENOSPC.
    grant_fed: bool = false,
    /// The park-and-reclaim gate consulted BEFORE ring admission (null =
    /// the manager, whose home shard is itself): a parked joiner's
    /// committers wait here holding nothing; an expired park fails them.
    pre_admission: ?PreAdmission = null,
    /// A JOINED appender's ring-growth gate (`sym/ring_growth.zig`): the
    /// pass enters it BEFORE it reads the ring (`holdForFill` reads the
    /// geometry and the head) and leaves it after `runBatch`; a window's
    /// terminal outcome rings `windowLeft`. A growth swaps `ring_layout`,
    /// `geo` and `caps` only with no pass inside and no window in flight.
    /// Null = the manager (region 0's fixed ring never grows).
    growth_gate: ?GrowthGate = null,
    /// The window-fill hold (§L): armed on a coalesced cadence (the
    /// backend passes the flush interval as the deadline's ceiling); null
    /// on the strict cadence and in the portable contracts' default.
    window_hold: ?WindowHold = null,
    /// The committer's executor (decision 0003 §AB.4); null = never spin
    /// (the portable contracts' inline pass, a thread off any lane).
    caller: ?CallerSeam = null,
    /// The applier venue (§AW); the inline applier by default.
    applier: ApplySeam = inline_applier,
    /// §BB.1: the members answered at the apply (the coalesced cadence);
    /// false = at the lane's verdict (strict mode; the portable contracts'
    /// default, which every durability-at-ack contract still asserts).
    ack_at_apply: bool = false,
    /// The journal tail (§BB.2); null = none (the contracts, a reader).
    tail: ?TailSeam = null,
    /// §BC: every member applies its OWN transaction on its committer's
    /// thread once the pass reserved and stamped the window (no applier
    /// hop, per-tx leaf locks); false = the applier venue (§AW; the
    /// portable contracts' default, `SQUEEZEFS_META_APPLY=venue`).
    self_apply: bool = false,
};

/// The committer's thread as the ack spin sees it (the backend binds the
/// lane's two reads).
pub const CallerSeam = struct {
    /// Parking would leave the thread idle (no other ready fiber, nothing
    /// in its inbox) — the only posture where a spin can beat the park.
    idle: *const fn () bool,
    /// The thread's poll count: one more at the resume than at the park ⇔
    /// nothing else ran between, so `ack → resumed` is the wake alone.
    polls: *const fn () u64,
};

/// The pre-admission gate's seam (`park_reclaim.ParkGate.preAdmission`).
pub const PreAdmission = struct {
    ctx: *anyopaque,
    wait: *const fn (ctx: *anyopaque) error{ Expired, Retired }!void,
};

/// The ring-growth gate's seam (`sym/ring_growth.zig` `Gate`).
pub const GrowthGate = struct {
    ctx: *anyopaque,
    /// Parks while a growth drains the ring; returns with the pass inside.
    enter: *const fn (ctx: *anyopaque) void,
    leave: *const fn (ctx: *anyopaque) void,
    /// A stage-B window reached its terminal outcome.
    windowLeft: *const fn (ctx: *anyopaque) void,
};

// ---------------------------------------------------------------------------
// The conveyor.
// ---------------------------------------------------------------------------

pub const Conveyor = struct {
    /// The committer's ack spin estimators (decision 0003 §AB.4).
    ack_spin: AckSpin = .{},
    deps: Deps,
    slots: []Core.Entry,
    core: Core,
    /// The drain buffer: one leader at a time, so one buffer.
    batch_buf: []Queued,
    closing: std.atomic.Value(bool) = .init(false),
    /// Windows between reservation and terminal outcome, by reservation
    /// start (the rollback hook addresses a window by its reservation).
    /// Sized ONCE at init for `inflight_cap` entries — the window gate's
    /// bound (`awaitWindowCell`), so `track` never allocates under the
    /// spin lock (charter §1.3).
    inflight_lock: jc.SpinLock = .{},
    /// The end of the furthest window reserved (§BB.4 under §BC: what an
    /// fsync must see landed, read by the lane through `Hooks.cover_upto`).
    reserved_hwm: std.atomic.Value(u64) = .init(0),
    inflight: std.AutoHashMapUnmanaged(u64, *Window) = .empty,
    inflight_cap: usize,
    /// The thread the last pass ran on (`std.Thread.getCurrentId`): the
    /// venue witness — ≡ the journal lane's tid on a mounted volume.
    pass_thread: std.atomic.Value(u64) = .init(0),
    /// The armed delta tee (null = no migration): one acquire load per
    /// window's terminal outcome. The engine keeps the tee alive until it
    /// has disarmed and the lane has no window in flight.
    tee: std.atomic.Value(?*const CommitTee) = .init(null),
    /// The armed token hook (null = no holder plane): one acquire load
    /// per pass. The plane keeps the hook alive until it disarmed and no
    /// window is in flight.
    tokens: std.atomic.Value(?*const TokenHook) = .init(null),
    /// The armed kernel view (null before the FUSE session is up, and in
    /// the portable contracts): one acquire load per window.
    kernel: std.atomic.Value(?*const KernelView) = .init(null),
    /// The window-fill hold's wake: every window's terminal outcome
    /// notifies (`finishWindow`, after `untrack`); the pass registers
    /// BEFORE it re-reads the in-flight count (`sync/notify.zig`'s law —
    /// `conveyor_core_model.HoldPark` proves the reversed order loses the
    /// landing).
    landed: sync.Notify = .init,
    rtt: RttEstimator = .{},
    /// The worth gate (pass-fiber-owned).
    gate: WorthGate = .{},
    /// Windows handed to an applier and not yet submitted (§AW): the
    /// pass's pacing input; `applied` notifies every leave.
    in_apply: std.atomic.Value(u32) = .init(0),
    applied: sync.Notify = .init,
    /// Committers parked for queue room (`enqueueParking`) and their wake:
    /// a drain notifies `room` only while the count is non-zero (one
    /// relaxed load per drain otherwise).
    room_waiters: std.atomic.Value(u32) = .init(0),
    room: sync.Notify = .init,
    stats: Stats = .{},

    pub fn init(gpa: Allocator, deps: Deps) Allocator.Error!*Conveyor {
        const self = try gpa.create(Conveyor);
        errdefer gpa.destroy(self);
        const slots = try gpa.alloc(Core.Entry, queueCapacity(deps.geo.pages));
        errdefer gpa.free(slots);
        const batch_buf = try gpa.alloc(Queued, @max(deps.caps.max_txs, 1));
        errdefer gpa.free(batch_buf);
        std.debug.assert(deps.lane.window_cells >= 2);
        const inflight_cap = windowCap(deps.lane.window_cells);
        self.* = .{ .deps = deps, .slots = slots, .core = undefined, .batch_buf = batch_buf, .inflight_cap = inflight_cap };
        errdefer self.inflight.deinit(gpa);
        try self.inflight.ensureTotalCapacity(gpa, @intCast(inflight_cap));
        self.core = Core.init(deps.io, slots, &self.stats.queued);
        return self;
    }

    /// The caller quiesced first (`quiesce`): nothing queued, nothing in
    /// flight, no leader.
    pub fn deinit(self: *Conveyor) void {
        const gpa = self.deps.gpa;
        assert(self.core.pending() == 0);
        assert(self.inflight.count() == 0);
        self.inflight.deinit(gpa);
        gpa.free(self.batch_buf);
        gpa.free(self.slots);
        gpa.destroy(self);
    }

    /// Refuse every later commit; the queued ones fail out through the
    /// live leader (or the next election).
    pub fn close(self: *Conveyor) void {
        self.closing.store(true, .release);
    }

    pub fn isClosing(self: *const Conveyor) bool {
        return self.closing.load(.acquire);
    }

    pub fn pending(self: *Conveyor) usize {
        return self.core.pending();
    }

    pub fn windowsInflight(self: *const Conveyor) u64 {
        return self.stats.windows_inflight.load();
    }

    /// A JOINED ring's growth landed (`sym/joined.zig` `growRingIfDue`):
    /// the pass reads the layout, the geometry and the caps on its next
    /// entry. The caller holds the growth gate closed with no pass inside
    /// and no window in flight — the only state in which these words have
    /// no concurrent reader. The byte cap follows the ring (`resolveCaps`'
    /// law); the tx cap is the CPU's.
    pub fn swapRing(self: *Conveyor, env: *const std.process.Environ.Map, layout: journal.RingLayout, geo: jc.CoreGeometry) void {
        std.debug.assert(self.deps.growth_gate != null and self.windowsInflight() == 0);
        self.deps.ring_layout = layout;
        self.deps.geo = geo;
        self.deps.caps.max_bytes = resolveBatchBytes(knobs.optIntKnob("SQUEEZEFS_META_COMMIT_BATCH_BYTES", u64, env), ringUserCapacity(geo));
    }

    /// Yield until nothing is queued, no committer waits for room, nobody
    /// leads and no window is in flight (the close path, the quiesce
    /// assertions).
    pub fn quiesce(self: *Conveyor) void {
        while (self.core.pending() != 0 or self.room_waiters.load(.monotonic) != 0 or self.core.leader.load(.seq_cst) or self.windowsInflight() != 0) std.Thread.yield() catch std.atomic.spinLoopHint();
    }

    /// `fsync`: the trailing coalesced barrier.
    pub fn barrier(self: *Conveyor) Error!void {
        return self.deps.lane.barrier(self.deps.lane.ctx);
    }

    /// Arm the delta tee (release pairs with the window's acquire load):
    /// every tx acked from here on is noted.
    pub fn armTee(self: *Conveyor, tee: *const CommitTee) void {
        self.tee.store(tee, .release);
    }

    pub fn disarmTee(self: *Conveyor) void {
        self.tee.store(null, .release);
    }

    /// Arm the token hook (release pairs with the pass's acquire load).
    pub fn armTokens(self: *Conveyor, hook: *const TokenHook) void {
        self.tokens.store(hook, .release);
    }

    pub fn disarmTokens(self: *Conveyor) void {
        self.tokens.store(null, .release);
    }

    /// Arm the kernel view (release pairs with `finishWindow`'s acquire).
    pub fn armKernel(self: *Conveyor, view: *const KernelView) void {
        self.kernel.store(view, .release);
    }

    pub fn disarmKernel(self: *Conveyor) void {
        self.kernel.store(null, .release);
    }

    // ---- committers ----

    /// Commit one tx: CONSUMES it. An empty tx answers Ok with nothing
    /// queued; a tx the admission refuses fails alone.
    pub fn commit(self: *Conveyor, t: *tx.KvTx) Error!void {
        if (t.isEmpty()) {
            t.deinit();
            return;
        }
        var done: Done = .init;
        var entry = try self.build(t, &done);
        const len = entry.q.len;
        self.enqueueOne(&entry, len) catch |e| {
            entry.deinit();
            return e;
        };
        self.leadIfElected();
        return self.park(&done);
    }

    /// Group commit: every member built first, the set enqueued under ONE
    /// queue lock (a drain never sees part of it), ONE election, every
    /// member's outcome awaited in order. Per member: empty ⇒ Ok inline; a
    /// refused build ⇒ its own error, siblings unaffected. Each `txs[i]`
    /// is consumed; `results[i]` is its outcome.
    pub fn commitGroup(self: *Conveyor, txs: []const *tx.KvTx, results: []Outcome) void {
        assert(results.len == txs.len);
        const gpa = self.deps.gpa;
        const Scratch = struct {
            dones: []Done,
            entries: []Core.Entry,
            tx_of: []usize,

            fn alloc(a: Allocator, n: usize) Allocator.Error!@This() {
                const dones = try a.alloc(Done, n);
                errdefer a.free(dones);
                const entries = try a.alloc(Core.Entry, n);
                errdefer a.free(entries);
                const tx_of = try a.alloc(usize, n);
                return .{ .dones = dones, .entries = entries, .tx_of = tx_of };
            }

            fn free(s: @This(), a: Allocator) void {
                a.free(s.dones);
                a.free(s.entries);
                a.free(s.tx_of);
            }
        };
        const scratch = Scratch.alloc(gpa, txs.len) catch {
            for (txs, results) |t, *r| {
                t.deinit();
                r.* = error.OutOfMemory;
            }
            return;
        };
        defer scratch.free(gpa);
        // Build every member first: an empty one answers inline, a refused
        // one its own error; the rest become entries (entry k ↔ tx
        // `tx_of[k]`).
        var n: usize = 0;
        for (txs, results, scratch.dones, 0..) |t, *r, *d, i| {
            if (t.isEmpty()) {
                t.deinit();
                r.* = {};
                continue;
            }
            d.* = .init;
            var entry = self.build(t, d) catch |e| {
                r.* = e;
                continue;
            };
            entry.grouped = true;
            scratch.entries[n] = .{ .item = entry, .len = entry.q.len };
            scratch.tx_of[n] = i;
            n += 1;
        }
        if (n == 0) return;
        self.enqueueParking(scratch.entries[0..n]) catch |err| {
            for (scratch.entries[0..n], scratch.tx_of[0..n]) |*e, i| {
                e.item.deinit();
                results[i] = err;
            }
            return;
        };
        self.stats.group_commits.inc();
        self.stats.group_txs.add(n);
        self.leadIfElected();
        for (scratch.tx_of[0..n]) |i| results[i] = self.park(&scratch.dones[i]);
    }

    fn build(self: *Conveyor, t: *tx.KvTx, done: *Done) Error!Queued {
        if (self.isClosing()) {
            t.deinit();
            return error.VolumeClosed;
        }
        self.deps.latch.admitMutation() catch |e| {
            t.deinit();
            return switch (e) {
                // A retired incarnation (a re-join in place): nothing
                // written, the op re-issued on the successor.
                error.Retired => error.Deferred,
                error.VolumeFailed => error.VolumeFailed,
            };
        };
        var q = tx.buildQueuedTx(t, self.deps.cache.cfg.layout.node_size, null) catch |e| return mapBuildError(e);
        // Step 0: the commit door — tokens per slot BEFORE the enqueue,
        // held to the terminal outcome (a refusal releases the guards
        // with the entry).
        if (!q.door_bypass) if (self.deps.door) |d| d.enter(d.ctx, q.recs, &q.door) catch |e| {
            q.deinit();
            return switch (e) {
                error.SlotBusy, error.SlotMoved => error.SlotBusy,
                error.Deferred => error.Deferred,
                error.Io => error.Io,
                error.Corrupt => error.Corrupt,
                error.OutOfMemory => error.OutOfMemory,
            };
        };
        // A6, the committer's (§AW): the heap promise BEFORE the enqueue,
        // so a refusal reserves nothing and the space retry's checkpoint
        // cycles never run behind an open reservation (a cycle's barrier
        // waits on the completed prefix, which would wait on this tx).
        const seeds = self.admitHeapPreQueue(&q) catch |e| {
            q.deinit();
            return e;
        };
        return .{ .q = q, .done = done, .enqueued_ns = clock.monotonicNs(), .seeds = seeds };
    }

    /// The heap admission of ONE tx on the committer's thread, under its
    /// leaves' write locks (the same law the pass ran for the batch:
    /// `heap_law.admitHeapLocked`, one promise per leaf, the space retry
    /// behind ≤ 2 return-room cycles, the final refusal typed). A stale
    /// leaf is re-resolved; the budget's exhaustion is the SMO protocol's
    /// bug as it was. Answers the admitted tx's leaves per record — the
    /// references kept, in `q`'s arena — as the applier's seeds.
    fn admitHeapPreQueue(self: *Conveyor, q: *QueuedTx) Error![]*CachedNode {
        var arena_state = std.heap.ArenaAllocator.init(self.deps.gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        const one = [_]Queued{.{ .q = q.*, .done = undefined, .enqueued_ns = 0 }};
        // §BD: the lock-free fast path — on a volume with room no promise is
        // owed (every leaf an in-flight transaction could push past its
        // node splits at writeback from extents that exist), so the
        // admission takes NO lock: the resolved leaves are the seeds and
        // the apply is the first take. A volume near its reserve, or in
        // the heap-full posture, takes the locked law below.
        if (heap_law.fastPathRoom(self.deps.alloc.freeExtents(), self.deps.alloc.reserveExtents(), self.deps.caps.max_txs) and !self.deps.checkpoint.posture.isFull()) {
            var r = try self.resolve(arena, &one, .admission);
            self.stats.heap_admit_fast.inc();
            return q.arena.allocator().dupe(*CachedNode, r.leaves[0]) catch {
                r.releaseRefs();
                return error.OutOfMemory;
            };
        }
        self.stats.heap_admit_locked.inc();
        var cycled_for_space = false;
        var space_retry_started: u64 = 0;
        var hold_active = false;
        defer if (hold_active) self.deps.checkpoint.space_retry.end();
        var attempt: u32 = 0;
        while (attempt < COMMIT_RETRY_BUDGET) {
            var r = try self.resolve(arena, &one, .admission);
            const dirties = arena.alloc(*NodeDirty, r.lock_set.len) catch {
                r.releaseRefs();
                return error.OutOfMemory;
            };
            self.lockAll(r.lock_set, dirties, .admission);
            if (stale(&one, &r)) {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                tree.stats.meta_kv_commit_smo_retries.inc();
                attempt += 1;
                continue;
            }
            var refused: std.ArrayList(heap_law.Refused) = .empty;
            defer refused.deinit(self.deps.gpa);
            self.admitHeap(arena, &one, &r, dirties, &refused) catch |e| {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                return e;
            };
            self.unlockAll(r.lock_set);
            if (hold_active) {
                self.deps.checkpoint.space_retry.end();
                hold_active = false;
            }
            if (refused.items.len == 0) {
                // Admitted: the resolved leaves are the applier's seeds (the
                // references move; a copy that fails leaves none).
                return q.arena.allocator().dupe(*CachedNode, r.leaves[0]) catch {
                    r.releaseRefs();
                    return &.{};
                };
            }
            r.releaseRefs();
            const now = clock.monotonicNs();
            if (space_retry_started == 0) space_retry_started = now;
            const decision = heap_law.decideSpaceRetry(.{
                .cycled_for_space = cycled_for_space,
                .heap_promised = self.deps.cache.heapPromised(),
                .pending_free = self.deps.alloc.pendingCount(),
                .elapsed_ns = now - space_retry_started,
                .wall_bound_ns = self.deps.park_escalation_ns,
            });
            switch (decision) {
                .retry_after_cycles => {
                    cycled_for_space = true;
                    self.deps.checkpoint.space_retry.begin();
                    hold_active = true;
                    var runs: u32 = 0;
                    while (true) {
                        if (!self.deps.checkpoint.checkpointNow(self.deps.checkpoint.ctx)) {
                            log.warn("meta volume {s}: checkpoint cycle for heap headroom failed (the refused tx will be answered on the retry)", .{self.deps.volume});
                            break;
                        }
                        runs += 1;
                        if (!heap_law.continueSpaceRetryCycles(runs, self.deps.alloc.pendingCount())) break;
                    }
                },
                .final_refusal => {
                    const rf = refused.items[0];
                    const e = heap_law.finalizeRefusal(self.deps.checkpoint.posture, self.deps.volume, rf.why, self.deps.cache.heapPromised());
                    if (self.deps.grant_fed) self.stats.manager_dependency_stalls.inc();
                    return if (self.deps.grant_fed) error.GrantExhausted else e;
                },
            }
            // A space retry spends the budget as a stale leaf does (the
            // pass's shape).
            attempt += 1;
        }
        log.err("meta volume {s}: commit retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.deps.volume});
        return error.RetryBudgetExhausted;
    }

    fn enqueueOne(self: *Conveyor, entry: *Queued, len: u64) Error!void {
        return self.enqueueParking(&.{.{ .item = entry.*, .len = len }});
    }

    /// Enqueue `entries` (all or nothing), PARKING for room while every
    /// cell is taken — never a refusal (0003 §BO.2: a full queue reached a
    /// create, an mkdir, an unlink as EAGAIN). The wait ends: a full queue
    /// holds entries, so a leader owns them (`conveyor_core`'s theorem)
    /// and its next drain frees room and notifies (`drain`) — the wait is
    /// the pass's progress, which already bounds every queued committer's
    /// (the window gate ends at a landing, ring admission escalates to
    /// FAILED after `park_escalation_ns`). A closing or FAILED volume
    /// answers its own error. Registration precedes the re-try
    /// (`sync/notify.zig`'s law). A group wider than the whole queue can
    /// never fit: refused — no product path commits one.
    fn enqueueParking(self: *Conveyor, entries: []const Core.Entry) Error!void {
        if (entries.len > self.slots.len) return error.ConveyorFull;
        if (self.core.enqueueMany(entries)) |_| return else |_| {}
        self.stats.commit_room_parks.inc();
        const io = self.deps.io;
        // Monotonic: the queue mutex orders it — this RMW precedes the
        // failed attempts' unlocks, and the drain that frees the room
        // locks before it reads the count (`drain`).
        _ = self.room_waiters.fetchAdd(1, .monotonic);
        defer _ = self.room_waiters.fetchSub(1, .monotonic);
        while (true) {
            var n = self.room.notified();
            _ = n.enable(io);
            if (self.core.enqueueMany(entries)) |_| {
                n.cancel(io);
                return;
            } else |_| {}
            if (self.isClosing() or self.deps.latch.isFailed()) {
                n.cancel(io);
                return if (self.deps.latch.isFailed()) error.VolumeFailed else error.VolumeClosed;
            }
            n.waitUncancelable(io);
        }
    }

    /// Drain a batch, then announce the room it freed to the committers
    /// parked on a full queue (`enqueueParking`).
    fn drain(self: *Conveyor, caps: Caps, out: []Queued) []Queued {
        const batch = self.core.drainBatch(caps, out);
        // Monotonic: paired with `enqueueParking`'s RMW through the queue
        // mutex (its attempt unlocked before this drain locked).
        if (batch.len != 0 and self.room_waiters.load(.monotonic) != 0) self.room.notifyWaiters(self.deps.io);
        return batch;
    }

    /// Enqueue + elect — no suspension point between: a committer that
    /// disappears can never strand an entry leaderless. The winner wakes
    /// the pass (ONE wake per election; the pass holds leadership until an
    /// empty drain releases it).
    fn leadIfElected(self: *Conveyor) void {
        if (!self.core.tryLead()) return;
        if (test_seams.drop_pass_wakes.load(.acquire) != 0 and test_seams.drop_pass_wakes.fetchSub(1, .acq_rel) != 0) return;
        self.deps.pass.wake(self.deps.pass.ctx);
    }

    /// The pass venue's body: run the pass iff an election scheduled one
    /// (the leader latch is set). A wake that finds no leader is spurious
    /// (the previous pass drained the entry before the wake landed).
    pub fn runScheduledPass(self: *Conveyor) void {
        if (self.leaderHeld()) self.pass();
    }

    /// The leader latch (seq_cst, the election's own order). The pass
    /// VENUE re-reads it before every park: an election whose wake landed
    /// while the venue's fiber was SCHEDULED (woken from its own park on
    /// the queue mutex inside `unleadAndRecheck`) was a no-op on the
    /// task-state word — the latch is the truth the word may have
    /// coalesced away (`conveyor_core_model.PassFiber`; `conveyor_core.zig`
    /// module doc; the 2026-10-10 wedge: 16 committers parked, no pass,
    /// every thread asleep — pinned on the real venue in `backend.zig`).
    pub fn leaderHeld(self: *const Conveyor) bool {
        return self.core.leader.load(.seq_cst);
    }

    /// The venue's park backstop found the leader latch set with no wake
    /// delivered (`backend.zig` `passEntry`): a lost election wake, absorbed
    /// by the tick and counted here — the same must-stay-≈0 tripwire as the
    /// admission park's.
    pub fn noteVenueTickRescue(self: *Conveyor) void {
        self.stats.park_tick_rescues.inc();
    }

    /// The thread the last pass ran on (0 before any).
    pub fn passThread(self: *const Conveyor) u64 {
        return self.pass_thread.load(.acquire);
    }

    fn park(self: *Conveyor, done: *Done) Error!void {
        self.stats.commit_parked.add(1);
        defer self.stats.commit_parked.sub(1);
        if (self.deps.self_apply) {
            // §BC: the signal is the TURN, a pass's drain away; spin for it
            // within the rule, else park.
            const bound = turnSpinBound(self.ack_spin.turn.mean());
            if (bound != 0 and spinForAck(done, bound)) {
                self.stats.turn_spin_hits.inc();
            } else {
                self.stats.turn_spin_falls.inc();
            }
            return self.takeSignal(done.recvUncancelable(self.deps.io) catch return error.Corrupt);
        }
        const rd = self.ack_spin.read();
        const bound = rd.bound_ns;
        // Published on change only: a store per park would bounce the
        // gauges' lines between every committing lane.
        inline for (.{ .{ &self.stats.ack_spin_bound_ns, bound }, .{ &self.stats.ack_spin_wake_ns, rd.wake_ns }, .{ &self.stats.ack_spin_ack_ns, rd.ack_ns } }) |g| {
            if (g[0].load() != g[1]) g[0].set(g[1]);
        }
        // The wake is learned only from a park on an IDLE lane: a lane with
        // queued work resumes the committer after that work, which is the
        // work's latency, not the wake's.
        const caller = self.deps.caller;
        const idle = if (caller) |c| c.idle() else false;
        const polls0 = if (idle) caller.?.polls() else 0;
        if (!idle) {
            self.stats.ack_spin_busy.inc();
        } else if (bound == 0) {
            self.stats.ack_spin_skips.inc();
        } else {
            if (spinForAck(done, bound)) {
                self.stats.ack_spin_hits.inc();
                return self.takeSignal(done.recvUncancelable(self.deps.io) catch return error.Corrupt);
            }
            self.stats.ack_spin_falls.inc();
        }
        const r = done.recvUncancelable(self.deps.io) catch return error.Corrupt;
        if (idle) {
            const acked = done.acked_ns.load(.acquire);
            const resumed = clock.monotonicNs();
            // Another fiber polled between the park and this resume: the
            // gap holds its work, not the wake's cost.
            if (acked != 0 and resumed > acked and caller.?.polls() == polls0 +% 1) self.ack_spin.wake.sample(resumed - acked);
        }
        return self.takeSignal(r);
    }

    /// The park's signal: the outcome, or the turn — the committer applies
    /// its own transaction here, on its thread (§BC).
    fn takeSignal(self: *Conveyor, sig: Done.Signal) Error!void {
        return switch (sig) {
            .outcome => |o| o,
            .turn => |t| blk: {
                self.ack_spin.turn.sample(clock.monotonicNs() -| t.w.members[t.idx].enqueued_ns);
                break :blk self.applyOwn(t.w, t.idx);
            },
        };
    }

    // ---- the pass (stage A) ----

    /// The leader's loop: drain → `runBatch` until an empty drain releases
    /// leadership (release THEN re-check: the no-lost-wakeup theorem). The
    /// caller holds leadership.
    pub fn pass(self: *Conveyor) void {
        self.pass_thread.store(@intCast(std.Thread.getCurrentId()), .release);
        while (true) {
            // The growth gate before the pass's first read of the ring (the
            // caps at the drain, the geometry and the head at the hold): a
            // growth swaps them only while no pass is inside.
            if (self.deps.growth_gate) |g| g.enter(g.ctx);
            defer if (self.deps.growth_gate) |g| g.leave(g.ctx);
            const batch = self.drain(self.deps.caps, self.batch_buf);
            if (batch.len == 0) {
                if (!self.core.unleadAndRecheck()) return;
                continue;
            }
            const held = self.holdForFill(batch);
            self.runBatch(held.batch, held.held);
        }
    }

    const HeldBatch = struct { batch: []Queued, held: bool };

    /// Whether a window of `entry_bytes` reserved at the ring's current
    /// head would carry more pad than entries (the §L predicate; exact on
    /// what lands — `WriteAccounting.padMajority` — up to a structural
    /// reservation interleaving before A8).
    pub fn padMajority(self: *const Conveyor, entry_bytes: u64) bool {
        return self.deps.geo.padOf(self.deps.core.headPos() + entry_bytes) > entry_bytes;
    }

    /// The window-fill hold (§L, amended §AW): a pad-majority batch waits
    /// for the NEXT landing — a park on `landed`, bounded by the round-trip
    /// predictor's deadline from the hold's start; then the pass re-drains
    /// what arrived meanwhile into the same window. In the paced regime
    /// (one window in flight) that landing empties the flight, as §L
    /// measured it; in the pipelined regime the appliers keep several in
    /// flight and a wait for the newest's landing held the pass — the one
    /// serial stage left — for the whole depth (62 % of mdtest's passes,
    /// 141 µs each: the closed loop ran at the hold's pace). §L's probe of
    /// the unpaced regime under the serial pass (`rand-4k-write` 0.77–0.9
    /// per first-landing probe against 2.3 per paced hold) predates §L.6,
    /// which parks that shape's stamps: it commits no entry to hold.
    /// Register-then-check: a landing's wake between the in-flight read and
    /// the park is never lost. Holds nothing when no law is armed, nothing
    /// is in flight (a lone committer never waits), the batch is
    /// entry-majority, the predictor has no sample yet, the measured gain
    /// says a hold collects less than its batch's quarter of the pad
    /// (`holdWorthIt`, probed one window in `HOLD_PROBE_PERIOD`), or the
    /// volume is closing.
    fn holdForFill(self: *Conveyor, batch: []Queued) HeldBatch {
        const unheld: HeldBatch = .{ .batch = batch, .held = false };
        const law = self.deps.window_hold orelse return unheld;
        if (self.isClosing()) return unheld;
        // The applier pacing (§AW): with the venue's width of windows in
        // apply, another window would queue behind them and contend for
        // their leaves — the pass holds until one leaves (or the
        // predictor's deadline) and re-drains what arrived.
        if (self.in_apply.load(.acquire) >= self.deps.applier.parallelism) {
            const bound = self.rtt.bound(law.ceiling_ns);
            if (bound == 0) return unheld;
            const io = self.deps.io;
            var n = self.applied.notified();
            _ = n.enable(io);
            if (self.in_apply.load(.acquire) < self.deps.applier.parallelism) {
                n.cancel(io);
                return unheld;
            }
            self.stats.holds_pacing.inc();
            const t0 = clock.monotonicNs();
            switch (n.waitUntilUncancelable(io, t0 + bound)) {
                .deadline => self.stats.hold_deadlines.inc(),
                .notified => self.stats.hold_landings.inc(),
            }
            self.stats.pass_hold.record(clock.monotonicNs() -| t0);
            return self.redrainHeld(batch);
        }
        // The pad hold (§L) rides a window ON THE DEVICE: one in apply is
        // the appliers' to overlap with, not the device's busy time.
        if (!law.pad_hold) return unheld;
        if (self.windowsInflight() <= self.in_apply.load(.acquire)) return unheld;
        if (self.isClosing()) return unheld;
        var sum: u64 = 0;
        for (batch) |m| sum += m.q.len;
        if (!self.padMajority(sum)) return unheld;
        const bound = self.rtt.bound(law.ceiling_ns);
        self.stats.hold_bound_ns.set(bound);
        if (bound == 0) return unheld;
        const io = self.deps.io;
        var n = self.landed.notified();
        _ = n.enable(io);
        if (self.windowsInflight() == 0) {
            n.cancel(io);
            return unheld;
        }
        const verdict: WorthGate.Verdict = if (law.pin_gate_open) .hold else self.gate.decide(batch.len);
        switch (verdict) {
            .decline => {
                n.cancel(io);
                self.stats.holds_not_worth.inc();
                return unheld;
            },
            .probe => self.stats.hold_probes.inc(),
            .hold => {},
        }
        self.stats.holds.inc();
        const t0 = clock.monotonicNs();
        const deadline = t0 + bound;
        // ONE landing ends the hold (§AW): in the paced regime it is the
        // flight's emptying, as before; in the pipelined regime — the
        // appliers keep several windows in flight — it is the next window's
        // landing, the cohort's re-arrival, while a wait for an EMPTY flight
        // held the pass (the one serial stage left) for the whole depth:
        // 62 % of mdtest's passes held 141 µs each and the closed loop ran
        // at the hold's pace.
        const landed = switch (n.waitUntilUncancelable(io, deadline)) {
            .deadline => false,
            .notified => true,
        };
        if (landed) self.stats.hold_landings.inc() else self.stats.hold_deadlines.inc();
        self.stats.pass_hold.record(clock.monotonicNs() -| t0);
        const held = self.redrainHeld(batch);
        // The hold's gain: what the re-drain collected (the gate's sample).
        self.gate.observe(verdict, @as(u64, held.batch.len - batch.len) * GAIN_UNIT, batch.len);
        self.stats.hold_gain_milli.set(self.gate.gainMilli());
        return held;
    }

    /// The re-drain after a hold, under the caps' remainder: FIFO is kept
    /// (the new members follow the held ones); the caps bound the whole
    /// window.
    fn redrainHeld(self: *Conveyor, batch: []Queued) HeldBatch {
        var sum: u64 = 0;
        for (batch) |m| sum += m.q.len;
        const rest: Caps = .{ .max_txs = self.deps.caps.max_txs -| batch.len, .max_bytes = self.deps.caps.max_bytes -| sum };
        if (rest.max_txs == 0 or batch.len >= self.batch_buf.len) return .{ .batch = batch, .held = true };
        const more = self.drain(rest, self.batch_buf[batch.len..]);
        return .{ .batch = self.batch_buf[0 .. batch.len + more.len], .held = true };
    }

    /// THE WINDOW GATE — why a pass never meets a full lane: the lane holds
    /// `window_cells` cells for windows alone (barriers and structural
    /// entries have their own), and the pass is the ONLY tracker of
    /// windows, so a pass that sees fewer than `windowCap` in flight keeps
    /// that true until its own `track` (the count only falls meanwhile);
    /// every tracked window holds at most one cell and one untracked
    /// window at most one more (`windowCap`), so its handoff always finds
    /// a cell. The ring cannot be the bound: a sub-page grain packs several
    /// windows into a page and a joined ring grows past the pages its lane
    /// was spawned for. A full flight parks the pass holding nothing — no
    /// union, no ring budget, no reservation — until a window's terminal
    /// outcome: register on `landed`, then re-read the count (`finishWindow`
    /// untracks, then notifies; `conveyor_core_model.WindowCells` proves
    /// the reversed order loses the wake). Every tracked window reaches its
    /// terminal outcome (the lane answers each, a FAILED volume and a
    /// stopping lane included), so the park always ends — save on a pass
    /// that runs on a committer whose own self-applied member is in flight
    /// (the `committer` venue with `self` apply, which the startup gate
    /// refuses: `knobs.exclusions`, 0003 §BO.3).
    fn awaitWindowCell(self: *Conveyor) void {
        const cap = windowCap(self.deps.lane.window_cells);
        if (self.windowsInflight() < cap) return;
        self.stats.window_gate_parks.inc();
        const io = self.deps.io;
        while (true) {
            var n = self.landed.notified();
            _ = n.enable(io);
            if (self.windowsInflight() < cap) {
                n.cancel(io);
                return;
            }
            n.waitUncancelable(io);
        }
    }

    /// Resolve a record's tree by `tree.route` (the ONE routing law); a
    /// record no mounted tree takes is the batch's `Corrupt`.
    fn treeFor(self: *const Conveyor, tree_id: u8, k: []const u8) ?*KvTree {
        return tree.routeContent(self.deps.trees.load(), tree_id, k);
    }

    fn treeOf(self: *const Conveyor, n: *const CachedNode) ?*KvTree {
        for (self.deps.trees.load()) |t| if (t.owns(n)) return t;
        return null;
    }

    /// A pre-reserve terminal outcome for every member: nothing reserved,
    /// nothing applied; the guards drop, the committers are answered.
    fn failBatch(self: *Conveyor, members: []Queued, err: Error) void {
        for (members) |*m| self.answer(m, err);
    }

    /// One member's terminal answer: guards BEFORE the send.
    fn answer(self: *Conveyor, m: *Queued, outcome: Outcome) void {
        m.q.guards.release();
        if (outcome) |_| self.stats.acks.inc() else |_| self.stats.refusals.inc();
        self.ack_spin.ack.sample(clock.monotonicNs() -| m.enqueued_ns);
        m.done.send(self.deps.io, outcome) catch {};
        m.deinit();
    }

    const Resolved = struct {
        /// Per member, per record: the leaf (a retained reference).
        leaves: [][]*CachedNode,
        /// The union, ascending by address, deduped.
        lock_set: []*CachedNode,
        /// Per member, per record: the index into `lock_set`.
        leaf_of: [][]usize,

        fn releaseRefs(self: *Resolved) void {
            for (self.leaves) |ls| for (ls) |n| n.release();
        }
    };

    fn addrLess(_: void, a: *CachedNode, b: *CachedNode) bool {
        return a.addr < b.addr;
    }

    /// A3: every record's leaf, latch-free; the union sorted and deduped.
    /// `phase`: the applier's resolve counts its seeded and descended
    /// records; the committer's admission resolve (no seeds yet) is not a
    /// measure of the seed.
    fn resolve(self: *Conveyor, arena: Allocator, members: []const Queued, comptime phase: enum { admission, apply }) Error!Resolved {
        var leaves = try arena.alloc([]*CachedNode, members.len);
        var leaf_of = try arena.alloc([]usize, members.len);
        var total: usize = 0;
        for (members) |m| total += m.q.recs.len;
        var all = try arena.alloc(*CachedNode, total);
        var n: usize = 0;
        errdefer for (all[0..n]) |l| l.release();
        for (members, 0..) |m, qi| {
            leaves[qi] = try arena.alloc(*CachedNode, m.q.recs.len);
            leaf_of[qi] = try arena.alloc(usize, m.q.recs.len);
            for (m.q.recs, 0..) |r, ri| {
                const leaf = blk: {
                    if (ri < m.seeds.len) {
                        const seed = m.seeds[ri];
                        if (!seed.state.isSuperseded() and !key.lessThan(r.key, seed.min_key) and !key.lessThan(seed.max_key, r.key)) {
                            if (phase == .apply) self.stats.resolve_seeded.inc();
                            break :blk seed.retain();
                        }
                    }
                    if (phase == .apply) self.stats.resolve_descended.inc();
                    const t = self.treeFor(r.tree_id, r.key) orelse return error.Corrupt;
                    break :blk t.resolveLeaf(r.key) catch |e| return mapTreeError(e);
                };
                leaves[qi][ri] = leaf;
                all[n] = leaf;
                n += 1;
            }
        }
        const lock_set = try arena.dupe(*CachedNode, all);
        core_sort.stable(*CachedNode, lock_set, {}, addrLess);
        var out: usize = 0;
        for (lock_set) |l| {
            if (out > 0 and lock_set[out - 1].addr == l.addr) continue;
            lock_set[out] = l;
            out += 1;
        }
        const set = lock_set[0..out];
        for (members, 0..) |m, qi| {
            for (m.q.recs, 0..) |_, ri| {
                const addr = leaves[qi][ri].addr;
                leaf_of[qi][ri] = for (set, 0..) |l, gi| {
                    if (l.addr == addr) break gi;
                } else unreachable;
            }
        }
        return .{ .leaves = leaves, .lock_set = set, .leaf_of = leaf_of };
    }

    /// A5: every member's every leaf still bound and still covering its key.
    fn stale(members: []const Queued, r: *const Resolved) bool {
        for (members, 0..) |m, qi| {
            for (m.q.recs, 0..) |rec, ri| {
                const leaf = r.leaves[qi][ri];
                if (leaf.state.isSuperseded() or key.lessThan(rec.key, leaf.min_key) or key.lessThan(leaf.max_key, rec.key)) return true;
            }
        }
        return false;
    }

    fn lockAll(self: *Conveyor, lock_set: []*CachedNode, dirties: []*NodeDirty, phase: enum { admission, apply }) void {
        const h = switch (phase) {
            .admission => &self.stats.leaf_takes_admission,
            .apply => &self.stats.leaf_takes_apply,
        };
        for (lock_set, dirties) |n, *d| {
            const t0 = clock.monotonicNs();
            d.* = n.lockWrite(self.deps.io);
            h.record(clock.monotonicNs() -| t0);
        }
    }

    fn unlockAll(self: *Conveyor, lock_set: []*CachedNode) void {
        for (lock_set) |n| n.unlockWrite(self.deps.io);
    }

    /// A2 / the re-admission: ONE admission for Σ sizes, parking with NO
    /// node lock held; the park kicks the checkpoint task with the park
    /// mark and escalates through the journal-failure lattice at the
    /// `PARK_ESCALATION_NS` crossing (a wedged-not-failed drain). Null ⇒ the
    /// batch fails (`err` names why).
    fn admitUserBudget(self: *Conveyor, len: u64, err: *Error) ?jc.Admission {
        const io = self.deps.io;
        const core = self.deps.core;
        // The park gate first (charter §5.5 PARK-AND-RECLAIM: "acks held
        // at a pre-admission gate"): a parked joiner admits nothing until
        // the reclaim lands; an expired park is the terminal poison; a
        // retired incarnation (a re-join in place) admits nothing more —
        // `Deferred`, nothing written, the op re-issued on its successor.
        if (self.deps.pre_admission) |g| g.wait(g.ctx) catch |e| {
            err.* = switch (e) {
                error.Retired => error.Deferred,
                error.Expired => error.VolumeFailed,
            };
            return null;
        };
        if (core.tryAdmit(len, .user)) |a| return a;
        // ONE stall per park entry, however many wakes and ticks it spans.
        self.stats.full_stalls.inc();
        var since = clock.monotonicNs();
        // The waiter node is kept across a TICK-ended wait (it stays
        // registered, unlinked), so the next `enable` answers whether a wake
        // landed since that wait's registration — a late wake vs none; a
        // wake-ended wait consumed it and a fresh node registers.
        var n = self.deps.checkpoint.space.notified();
        var ticked = false;
        while (true) {
            const woken = n.enable(io);
            if (core.tryAdmit(len, .user)) |a| {
                n.cancel(io);
                // The backstop tick found room no wake announced (≈ 0
                // healthy — every barrier that moves `reusable_upto`
                // notifies `space` itself).
                if (ticked and !woken) self.stats.park_tick_rescues.inc();
                return a;
            }
            if (self.isClosing() or self.deps.latch.isFailed() or self.deps.latch.isRetired()) {
                n.cancel(io);
                err.* = if (self.deps.latch.isRetired()) error.Deferred else if (self.deps.latch.isFailed()) error.VolumeFailed else error.VolumeClosed;
                return null;
            }
            self.deps.checkpoint.kickPark(self.deps.checkpoint.ctx);
            while (test_seams.hold_at_park.load(.acquire) != 0) {
                test_seams.park_held.store(1, .release);
                std.Thread.yield() catch std.atomic.spinLoopHint();
            }
            const now = clock.monotonicNs();
            const until_crossing = self.deps.park_escalation_ns -| (now - since);
            switch (n.waitUntilUncancelable(io, now + @min(PARK_TICK_NS, @max(until_crossing, 1)))) {
                .deadline => ticked = true,
                .notified => {
                    ticked = false;
                    n = self.deps.checkpoint.space.notified();
                },
            }
            if (clock.monotonicNs() - since >= self.deps.park_escalation_ns) {
                log.err("meta volume {s}: conveyor pass parked {d} ms (≥ {d} ms) waiting for journal-ring admission ({d} B) — the drain is not advancing (wedged-not-failed class); escalating through the journal-failure lattice (D1.b audit row 2)", .{ self.deps.volume, (clock.monotonicNs() - since) / std.time.ns_per_ms, self.deps.park_escalation_ns / std.time.ns_per_ms, len });
                _ = self.deps.latch.noteFailure(.journal_write);
                since = clock.monotonicNs();
            }
        }
    }

    /// The §4.7 walker over the locked leaves' snapshots.
    const FoldWalker = struct {
        gpa: Allocator,
        layout: node.NodeLayout,
        lock_set: []*CachedNode,
        oom: bool = false,

        pub fn foldUpper(self: *FoldWalker, gi: usize, pending_records: []const heap_law.PendingRecord) heap_law.FoldUpper {
            const extra = self.gpa.alloc(snapshot.Extra, pending_records.len) catch {
                self.oom = true;
                return .{ .bytes = 0, .parts = 1 };
            };
            defer self.gpa.free(extra);
            for (pending_records, extra) |p, *x| x.* = .{ .key = p.key, .kind = p.kind, .len = p.encoded_len };
            const snap = self.lock_set[gi].snapshotLoad();
            defer snap.release();
            const p = snap.value.foldBytesUpperWith(extra, self.layout.splitPartCapacity(), 0);
            return .{ .bytes = p.bytes, .parts = p.parts };
        }
    };

    /// A6 under the union locks: the leaves' promise ledgers borrowed into
    /// `heap_law`'s shape, the admission, the ledgers written back.
    fn admitHeap(self: *Conveyor, arena: Allocator, members: []const Queued, r: *const Resolved, dirties: []*NodeDirty, refused: *std.ArrayList(heap_law.Refused)) Error!void {
        const layout = self.deps.cache.cfg.layout;
        const promises = try arena.alloc(heap_law.LeafPromise, r.lock_set.len);
        const views = try arena.alloc(heap_law.LeafView, r.lock_set.len);
        for (r.lock_set, dirties, promises, views) |n, d, *p, *v| {
            p.* = .{ .promised = d.promised, .basis = d.promise_basis, .added = d.promise_added };
            const t = self.treeOf(n);
            v.* = .{
                .addr = n.addr,
                .tail_offset = d.tailOffset(),
                .frozen_frame_len = if (d.frozen) |*f| f.frame_len else 0,
                .overlay_bytes = d.overlayBytes(),
                .is_root = if (t) |tr| tr.isRoot(n) else false,
                .promise = p,
            };
        }
        const hl_members = try arena.alloc(heap_law.Member, members.len);
        for (members, r.leaf_of, hl_members) |m, lo, *hm| hm.* = .{ .recs = m.q.recs, .leaf_of = lo };
        var heap_ledger = heap_law.HeapLedger.bound(&self.deps.cache.heap_promised);
        var walker: FoldWalker = .{ .gpa = self.deps.gpa, .layout = layout, .lock_set = r.lock_set };
        try heap_law.admitHeapLocked(self.deps.gpa, layout, self.deps.alloc, &walker, &heap_ledger, self.deps.checkpoint.posture, self.deps.volume, hl_members, views, refused);
        if (walker.oom) return error.OutOfMemory;
        // The ledger moved the volume gauge; the leaves take the new words.
        for (dirties, promises) |d, p| {
            d.promised = p.promised;
            d.promise_basis = p.basis;
            d.promise_added = p.added;
        }
    }

    /// Run the batch pipeline (stage A) over one drained batch (`held`: the
    /// window-fill hold preceded it — the handoff's class).
    fn runBatch(self: *Conveyor, batch: []Queued, held: bool) void {
        const gpa = self.deps.gpa;
        const t_pass = clock.monotonicNs();
        self.stats.leader_passes.inc();
        self.stats.group_size.record(batch.len);
        self.stats.drained_txs.add(batch.len);
        var sum: u64 = 0;
        for (batch) |m| {
            sum += m.q.len;
            self.stats.tx_queue_wait.record(t_pass -| m.enqueued_ns);
        }
        self.stats.group_bytes.add(sum);

        var arena_state = std.heap.ArenaAllocator.init(gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        // The live members (refused ones leave; survivors move into the window).
        var members: std.ArrayList(Queued) = .empty;
        members.appendSlice(arena, batch) catch {
            self.failBatch(batch, error.OutOfMemory);
            return;
        };

        // A1.
        if (self.isClosing() or self.deps.latch.isFailed() or self.deps.latch.isRetired()) {
            self.failBatch(members.items, if (self.deps.latch.isRetired()) error.Deferred else if (self.deps.latch.isFailed()) error.VolumeFailed else error.VolumeClosed);
            return;
        }
        // A1¼: the window's lane cell, before the pass takes anything.
        self.awaitWindowCell();
        // A1½: the token recall — the union of the batch's objects (every
        // record of every non-`token_quiet` member) taken in flight ONCE,
        // under the members' 4a guards, BEFORE ring admission and any node
        // lock; the pass proceeds on every ack or the reader's lease
        // expiry. The union settles at the window's terminal outcome; a
        // batch that dies before its window exists settles here.
        var token_union: []const u64 = &.{};
        const tokens = self.tokens.load(.acquire);
        var union_settled = tokens == null;
        defer if (!union_settled) tokens.?.settle(tokens.?.ctx, token_union);
        if (tokens) |hook| {
            var u: token_table.ObjectUnion = .{};
            var notes: std.ArrayList(tx.NameNote) = .empty;
            var unnamed = false;
            for (members.items) |*m| {
                u.addMember(arena, &m.q) catch {
                    self.failBatch(members.items, error.OutOfMemory);
                    return;
                };
                if (m.q.token_quiet) continue;
                notes.appendSlice(arena, m.q.names.slice()) catch {
                    self.failBatch(members.items, error.OutOfMemory);
                    return;
                };
                unnamed = unnamed or unnamedDentries(&m.q);
            }
            token_union = u.finish();
            const t_recall = clock.monotonicNs();
            hook.recall(hook.ctx, token_union, .{ .notes = notes.items, .unnamed = unnamed }) catch {
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            self.stats.pass_token_recall.record(clock.monotonicNs() -| t_recall);
        }
        // A2.
        var err: Error = error.Corrupt;
        const t_adm = clock.monotonicNs();
        var admission: ?jc.Admission = self.admitUserBudget(sum, &err) orelse {
            self.failBatch(members.items, err);
            return;
        };
        self.stats.pass_admission.record(clock.monotonicNs() -| t_adm);
        // Every exit below either transfers the admission or releases it.
        defer if (admission) |a| self.deps.core.release(a);

        // A7': the window — its arena takes the members, the failure slots
        // and the token union BEFORE anything is reserved: past this point
        // no allocation can fail a batch whose range is on the ring.
        const w = gpa.create(Window) catch {
            self.failBatch(members.items, error.OutOfMemory);
            return;
        };
        w.* = .{ .conveyor = self, .arena = std.heap.ArenaAllocator.init(gpa), .members = &.{}, .failed = &.{}, .res = undefined, .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .t_handoff_ns = 0, .t_pass_ns = t_pass, .held = held };
        const wa = w.arena.allocator();
        const window_members = wa.dupe(Queued, members.items) catch {
            w.arena.deinit();
            gpa.destroy(w);
            self.failBatch(members.items, error.OutOfMemory);
            return;
        };
        const failed = wa.alloc(?Error, members.items.len) catch {
            w.arena.deinit();
            gpa.destroy(w);
            self.failBatch(members.items, error.OutOfMemory);
            return;
        };
        @memset(failed, null);
        w.token_union = wa.dupe(u64, token_union) catch {
            w.arena.deinit();
            gpa.destroy(w);
            self.failBatch(members.items, error.OutOfMemory);
            return;
        };
        // A8: ONE contiguous reservation; the members' seqs stamped
        // back-to-back. From here the batch is a window: every exit
        // completes the reservation (§AW: the apply follows the
        // reservation, on the applier's thread; the registry's open entry
        // holds the checkpoint tail back until the lane completes it,
        // which is after the apply and the landing).
        const reg = self.deps.reg.reserveRegistered(self.deps.core, admission.?) catch {
            // The core transferred the claim before the registry's
            // append failed (the one way a reservation fails): the range
            // is the ring's, unregistered — complete nothing, fail loud.
            w.arena.deinit();
            gpa.destroy(w);
            admission = null;
            log.err("meta volume {s}: the batch reservation could not be registered (out of memory) — its range stays an unwritten hole", .{self.deps.volume});
            _ = self.deps.latch.noteFailure(.journal_write);
            self.failBatch(members.items, error.OutOfMemory);
            return;
        };
        admission = null;
        // The reservation high-water mark the lane's fsync cover reads
        // (§BB.4 under §BC); monotonic: positions only grow.
        _ = self.reserved_hwm.fetchMax(reg.res.paddedEnd(), .release);
        var stamp_cursor = reg.seq_base;
        for (window_members) |*m| {
            _ = m.q.stampSeqs(stamp_cursor);
            stamp_cursor += m.q.len;
        }
        w.members = window_members;
        w.failed = failed;
        w.res = reg.res;
        w.seq_base = reg.seq_base;
        if (window_members[0].q.recs.len != 0) {
            const last = window_members[0].q.recs[window_members[0].q.recs.len - 1];
            // Fibonacci hashing: neighbouring tree objects spread over the lanes.
            if (self.treeFor(last.tree_id, last.key)) |t| w.affinity = (@intFromPtr(t) *% 0x9E3779B97F4A7C15) >> 32;
        }
        // From here every exit finishes the window: the union is its.
        union_settled = true;
        // Tracked before the handoff: the hold law and the window gate
        // count it in flight, and the lane's rollback hook can name it.
        self.track(w) catch {
            self.abandonWindow(w, error.OutOfMemory);
            return;
        };
        self.stats.pass_total.record(clock.monotonicNs() -| t_pass);
        // §BC: every member applies its own transaction on its committer's
        // thread — the pass only reserved, stamped and serialized. A group
        // commit's members park one after another on ONE thread, so a
        // window holding one takes the applier venue (its members' turns
        // could not all be taken while the first waits for the landing).
        if (self.deps.self_apply and !anyGrouped(window_members)) return self.handToMembers(w);
        // A8½: the handoff to an APPLIER (§AW): a fiber on the applier
        // venue runs the second half while this pass drains the next
        // batch and the lane lands the earlier windows. A window with
        // NOTHING in flight beside it — a serial chain of ops, one at a
        // time — is applied here instead: the handoff's wake and the
        // venue's scheduling cost that chain ≈ 30 µs per op (a shared
        // directory's mkdirs, serialized by its exclusive guard, ran 0.72×)
        // and overlap nothing. The portable contracts apply inline too, as
        // does a venue that refuses the spawn.
        const alone = self.windowsInflight() == 1;
        _ = self.in_apply.fetchAdd(1, .acq_rel);
        const a = self.deps.applier;
        if (alone or !a.spawn(a.ctx, w)) self.applyWindow(w);
    }

    /// §BC: the members' turns. Each gets its sub-range, its seq and its
    /// own arena, then the signal; the last to finish applying submits the
    /// window (`submitSelfApplied`). A member whose committer is gone (it
    /// cannot be: the park is uncancelable) is rolled out as a PAD.
    fn handToMembers(self: *Conveyor, w: *Window) void {
        const gpa = self.deps.gpa;
        const wa = w.arena.allocator();
        const n = w.members.len;
        w.parts = wa.alloc(Window.Part, n) catch {
            self.abandonWindow(w, error.OutOfMemory);
            return;
        };
        var pos = w.res.start;
        var seq = w.seq_base;
        for (w.parts, w.members) |*p, *m| {
            p.* = .{ .arena = std.heap.ArenaAllocator.init(gpa), .start = pos, .seq = seq, .len = m.q.len };
            pos += m.q.len;
            seq += m.q.len;
        }
        w.self_applied = true;
        w.t_turns_ns = clock.monotonicNs();
        w.pending.store(@intCast(n), .release);
        self.stats.self_handoffs.inc();
        // The turns fan out as a binary tree: the pass wakes member 0, each
        // member wakes 2i+1 and 2i+2 before its own apply — the pass pays
        // one send per batch and the fan-out's depth is log₂ n (at 64 ranks
        // a serial fan-out on the lane fiber took 254 µs of every member's
        // queue wait and parked every spinner past its bound).
        self.relayTurn(w, 0);
    }

    /// Hand member `idx` its turn; a member whose committer is gone (it
    /// cannot be: the park is uncancelable) is rolled out as a PAD, its
    /// share of `pending` taken here, and its subtree relayed by us.
    fn relayTurn(self: *Conveyor, w: *Window, idx: u32) void {
        if (idx >= w.members.len) return;
        w.members[idx].done.sendTurn(self.deps.io, w, idx) catch {
            w.failed[idx] = error.Corrupt;
            self.relayTurn(w, 2 * idx + 1);
            self.relayTurn(w, 2 * idx + 2);
            if (w.pending.fetchSub(1, .acq_rel) == 1) self.submitSelfApplied(w);
        };
    }

    /// §BC: ONE member's apply on its own committer's thread — A3 resolve
    /// (seeded) → A4 lock ITS leaves (ascending) → A5 revalidate → A7 its
    /// pre-images → A9 apply → A10 the threshold → unlock → A12 its own
    /// ops (a PAD over its range if it rolled out) → the tail → the ack
    /// (this mount's kernel's own op, §BB.1) → the window's submit when it
    /// is the last member. A served verb, a control record or a tool waits
    /// for the landing. Returns the member's terminal outcome.
    pub fn applyOwn(self: *Conveyor, w: *Window, idx: u32) Outcome {
        // The fan-out's next level first: two siblings start while we apply.
        self.relayTurn(w, 2 * idx + 1);
        self.relayTurn(w, 2 * idx + 2);
        const m = &w.members[idx];
        const p = &w.parts[idx];
        const a = p.arena.allocator();
        const members = w.members[idx .. idx + 1];
        const failed = w.failed[idx .. idx + 1];
        const done = m.done;
        const kernel_origin = m.q.kernel_origin;
        const len = m.q.len;
        const t_apply = clock.monotonicNs();
        self.stats.self_applies.inc();
        if (m.q.leafwise) {
            failed[0] = self.applyLeafwise(a, m, p);
        } else {
            self.applyUnion(a, members, p, failed);
        }
        self.stats.pass_leaf_locks.record(clock.monotonicNs() -| t_apply);
        // A12, the member's own: its entry, or a PAD over its range.
        const sub = Reservation.unpadded(p.start, len);
        var ops: std.ArrayList(journal.WriteOp) = .empty;
        if (failed[0] == null) {
            const encoded: Error!u64 = blk: {
                const frames = m.q.framesAlloc() catch break :blk error.OutOfMemory;
                break :blk journal.partOps(a, self.deps.ring_layout, self.deps.geo, self.deps.appender_id, .{ .res = sub, .records = frames }, &ops) catch |e| switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Corrupt,
                };
            };
            if (encoded) |hb| {
                p.header_bytes = hb;
            } else |e| {
                // Applied, unencodable: its records leave as a failed write's
                // would (seq-conditional), its range becomes a PAD.
                self.rollbackFailedTx(SeqSpan.stamped(p.seq, len), p.undo);
                failed[0] = e;
                ops.clearRetainingCapacity();
            }
        }
        if (failed[0] != null) {
            p.header_bytes = journal.partOps(a, self.deps.ring_layout, self.deps.geo, self.deps.appender_id, .{ .res = sub, .records = &.{}, .pad = true }, &ops) catch 0;
        }
        p.ops = ops.items;
        if (self.deps.door) |d| d.frontier(d.ctx, &m.q.door, p.start + len);
        self.stats.apply_total.record(clock.monotonicNs() -| t_apply);
        // The ack ahead of the landing (§BB.1): this mount's kernel's own
        // op, its bytes in the tail first; a failed member's error is its
        // outcome now whatever its origin — nothing of it will land.
        const outcome: Outcome = if (failed[0]) |e| e else {};
        var early = kernel_origin and (self.deps.ack_at_apply or failed[0] != null);
        if (early and failed[0] == null) if (self.deps.tail) |t| {
            p.tail_slot = t.claim(t.ctx, p.seq, p.start, p.ops);
            if (p.tail_slot == null) {
                self.stats.tail_slot_misses.fire("the journal tail's slot table refused a member; acked at the landing");
                early = false;
            }
        };
        if (early) {
            p.answered = true;
            self.stats.acks_at_apply.inc();
            self.answerOne(m, outcome, false);
        }
        // The last member to finish writes the window. Nothing of `w` is
        // touched past this point by a member that answered itself: the
        // lane may finish and free the window at once.
        if (w.pending.fetchSub(1, .acq_rel) == 1) self.submitSelfApplied(w);
        if (early) return outcome;
        return done.landing.recvUncancelable(self.deps.io) catch error.Corrupt;
    }

    /// §BC, a member's apply under ITS leaf set: every leaf locked
    /// ascending and held across the whole apply (a name's change is seen
    /// whole — the dentry and the inode together).
    fn applyUnion(self: *Conveyor, a: Allocator, members: []Queued, p: *Window.Part, failed: []?Error) void {
        var attempt: u32 = 0;
        while (failed[0] == null) : (attempt += 1) {
            if (attempt >= COMMIT_RETRY_BUDGET) {
                log.err("meta volume {s}: commit retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.deps.volume});
                failed[0] = error.RetryBudgetExhausted;
                return;
            }
            var r = self.resolve(a, members, .apply) catch |e| {
                failed[0] = e;
                return;
            };
            const dirties = a.alloc(*NodeDirty, r.lock_set.len) catch {
                r.releaseRefs();
                failed[0] = error.OutOfMemory;
                return;
            };
            const t_leaf = clock.monotonicNs();
            self.lockAll(r.lock_set, dirties, .apply);
            if (stale(members, &r)) {
                self.unlockAll(r.lock_set);
                self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
                r.releaseRefs();
                tree.stats.meta_kv_commit_smo_retries.inc();
                continue;
            }
            p.undo = self.captureUndo(a, members, &r) catch |e| {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                failed[0] = e;
                return;
            };
            self.applyMembers(members, &r, dirties, p.start, p.seq, failed);
            var threshold_crossed = false;
            for (r.lock_set, dirties) |n, d| {
                if (d.overlayBytes() >= self.deps.cache.cfg.writeback_delta_bytes) {
                    if (self.treeOf(n)) |t| {
                        t.enqueueMaintenance(n.addr);
                        threshold_crossed = true;
                    }
                }
            }
            self.unlockAll(r.lock_set);
            self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
            r.releaseRefs();
            if (threshold_crossed) self.deps.checkpoint.kickThreshold(self.deps.checkpoint.ctx);
            return;
        }
    }

    /// §BV, a LEAFWISE member's apply (`QueuedTx.leafwise`: a corpse's
    /// destroy — no name reaches its objects, no kernel holds them, so no
    /// reader can tell this apply from an atomic one): ONE leaf at a time,
    /// ascending — lock it, revalidate it against the records resolved to
    /// it (a moved leaf re-resolves those alone), their pre-images, apply,
    /// the threshold, unlock — then the next. THE LAW: it never waits for a
    /// leaf while holding one, so a committer on any of its leaves waits
    /// behind it for at most that leaf's share of its records, never for
    /// its acquisition of the others (the union hold put every unlinker on
    /// a 64-corpse destroy's leaves behind its whole acquisition, and its
    /// window's reservation — the completed prefix — open as long). Its
    /// entry is still ONE entry: replay is all or nothing. A failure after
    /// a leaf was released (or the poison seam) rolls the member's applied
    /// records back seq-conditionally (`rollbackFailedTx`: a freeze may
    /// have taken one into the base). Answers the member's failure.
    fn applyLeafwise(self: *Conveyor, a: Allocator, m: *const Queued, p: *Window.Part) ?Error {
        const gpa = self.deps.gpa;
        const io = self.deps.io;
        const recs = m.q.recs;
        const span = SeqSpan.stamped(p.seq, m.q.len);
        self.stats.leafwise_applies.inc();
        const t0 = clock.monotonicNs();
        defer self.stats.leafwise_apply.record(clock.monotonicNs() -| t0);
        // A retained leaf per record, seeded by the committer's admission.
        const leaves = a.alloc(*CachedNode, recs.len) catch return error.OutOfMemory;
        var resolved: usize = 0;
        defer for (leaves[0..resolved]) |l| l.release();
        for (recs, 0..) |r, ri| {
            leaves[ri] = blk: {
                if (ri < m.seeds.len) {
                    const seed = m.seeds[ri];
                    if (!seed.state.isSuperseded() and !key.lessThan(r.key, seed.min_key) and !key.lessThan(seed.max_key, r.key)) {
                        self.stats.resolve_seeded.inc();
                        break :blk seed.retain();
                    }
                }
                self.stats.resolve_descended.inc();
                const t = self.treeFor(r.tree_id, r.key) orelse return error.Corrupt;
                break :blk t.resolveLeaf(r.key) catch |e| return mapTreeError(e);
            };
            resolved += 1;
        }
        // The records still to apply, by leaf address (record order kept
        // within a leaf), re-sorted only after a re-resolve.
        const order = a.alloc(u32, recs.len) catch return error.OutOfMemory;
        for (order, 0..) |*o, i| o.* = @intCast(i);
        const ByLeaf = struct {
            leaves: []*CachedNode,
            fn lessThan(ctx: @This(), x: u32, y: u32) bool {
                const ax = ctx.leaves[x].addr;
                const ay = ctx.leaves[y].addr;
                return ax < ay or (ax == ay and x < y);
            }
        };
        std.mem.sort(u32, order, ByLeaf{ .leaves = leaves }, ByLeaf.lessThan);
        var rest: []u32 = order;
        var undo: std.ArrayList(tx.UndoKey) = .empty;
        var group: std.ArrayList(OwnedRec) = .empty;
        defer group.deinit(gpa);
        var attempt: u32 = 0;
        const err: ?Error = while (rest.len != 0) {
            const leaf = leaves[rest[0]];
            var end: usize = 1;
            while (end < rest.len and leaves[rest[end]].addr == leaf.addr) end += 1;
            const run = rest[0..end];
            const t_take = clock.monotonicNs();
            const dirty = leaf.lockWrite(io);
            const t_held = clock.monotonicNs();
            self.stats.leaf_takes_apply.record(t_held -| t_take);
            var moved = leaf.state.isSuperseded();
            for (run) |i| moved = moved or key.lessThan(recs[i].key, leaf.min_key) or key.lessThan(leaf.max_key, recs[i].key);
            if (moved) {
                leaf.unlockWrite(io);
                self.stats.leafwise_leaf_hold.record(clock.monotonicNs() -| t_held);
                tree.stats.meta_kv_commit_smo_retries.inc();
                attempt += 1;
                if (attempt >= COMMIT_RETRY_BUDGET) {
                    log.err("meta volume {s}: commit retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.deps.volume});
                    break error.RetryBudgetExhausted;
                }
                const fresh_err: ?Error = for (run) |i| {
                    const t = self.treeFor(recs[i].tree_id, recs[i].key) orelse break error.Corrupt;
                    const fresh = t.resolveLeaf(recs[i].key) catch |e| break mapTreeError(e);
                    leaves[i].release();
                    leaves[i] = fresh;
                } else null;
                if (fresh_err) |e| break e;
                std.mem.sort(u32, rest, ByLeaf{ .leaves = leaves }, ByLeaf.lessThan);
                continue;
            }
            const applied: ?Error = blk: {
                self.captureUndoRun(a, &undo, recs, run, leaf) catch |e| break :blk e;
                group.clearRetainingCapacity();
                for (run) |i| {
                    const r = recs[i];
                    const owned = OwnedRec.fromRecordRef(gpa, .{ .key = r.key, .seq = r.seq, .kind = r.kind, .value = r.value }) catch break :blk error.OutOfMemory;
                    group.append(gpa, owned) catch {
                        var o = owned;
                        o.deinit();
                        break :blk error.OutOfMemory;
                    };
                } else {
                    // The elements are the apply's on every outcome.
                    defer group.clearRetainingCapacity();
                    leaf.applyLocked(dirty, group.items, p.start) catch |e| break :blk switch (e) {
                        error.Superseded => error.Corrupt,
                        error.ReaderMayNotMutate, error.LeafLeaseRefused => error.PartitionRefused,
                        error.OutOfMemory => error.OutOfMemory,
                    };
                }
                break :blk null;
            };
            // A run never handed to the apply: its built elements are ours.
            for (group.items) |*o| o.deinit();
            group.clearRetainingCapacity();
            const threshold = applied == null and dirty.overlayBytes() >= self.deps.cache.cfg.writeback_delta_bytes;
            if (threshold) if (self.treeOf(leaf)) |t| t.enqueueMaintenance(leaf.addr);
            leaf.unlockWrite(io);
            self.stats.leafwise_leaf_hold.record(clock.monotonicNs() -| t_held);
            if (applied) |e| break e;
            self.stats.leafwise_leaves.inc();
            if (threshold) self.deps.checkpoint.kickThreshold(self.deps.checkpoint.ctx);
            rest = rest[end..];
        } else null;
        p.undo = undo.items;
        const poison = test_seams.poison_apply_ino.load(.monotonic);
        const outcome: ?Error = if (err) |e| e else if (poison != 0 and namesInode(recs, poison)) error.Corrupt else null;
        if (outcome != null and undo.items.len != 0) self.rollbackFailedTx(span, undo.items);
        return outcome;
    }

    /// A7 for one leaf's run of a leafwise member, under that leaf's lock:
    /// one pre-image per distinct `(tree, key)` across the member, first
    /// touch wins (`captureUndo`'s law).
    fn captureUndoRun(self: *Conveyor, a: Allocator, undo: *std.ArrayList(tx.UndoKey), recs: []const tx.Rec, run: []const u32, leaf: *CachedNode) Error!void {
        const snap = leaf.snapshotLoad();
        defer snap.release();
        recs: for (run) |i| {
            const rec = recs[i];
            for (undo.items) |u| {
                if (u.tree_id == rec.tree_id and std.mem.eql(u8, u.key, rec.key)) continue :recs;
            }
            var l = snap.value.lookup(self.deps.gpa, rec.key) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Corrupt,
            };
            defer l.deinit();
            const pre: tx.PreImage = switch (l) {
                .live => |lv| .{ .live = try a.dupe(u8, lv.value) },
                .tombstone => .tombstone,
                .absent => .absent,
            };
            try undo.append(a, .{ .tree_id = rec.tree_id, .key = try a.dupe(u8, rec.key), .pre = pre });
        }
    }

    /// §BC: every member applied (or rolled out) — the window's image from
    /// their ops, the handoff to the lane. Runs on the last member's thread.
    fn submitSelfApplied(self: *Conveyor, w: *Window) void {
        const gpa = self.deps.gpa;
        const wa = w.arena.allocator();
        var entry_bytes: u64 = 0;
        var header_bytes: u64 = 0;
        var survivors: u64 = 0;
        var undo_n: usize = 0;
        for (w.parts, w.failed) |p, f| {
            header_bytes += p.header_bytes;
            undo_n += p.undo.len;
            if (f == null) {
                survivors += 1;
                entry_bytes += p.len;
            }
        }
        w.entries = survivors;
        // The undo of the whole window, for the lane's rollback of a
        // window nobody was acked from.
        if (wa.alloc(tx.UndoKey, undo_n)) |undo| {
            var i: usize = 0;
            for (w.parts) |p| {
                @memcpy(undo[i .. i + p.undo.len], p.undo);
                i += p.undo.len;
            }
            w.undo = undo;
        } else |_| {}
        w.image = if (wa.alloc([]const journal.WriteOp, w.parts.len)) |member_ops| blk: {
            for (w.parts, member_ops) |p, *mo| mo.* = p.ops;
            break :blk journal.buildWindowFromOps(gpa, self.deps.ring_layout, self.deps.geo, self.deps.appender_id, w.res, member_ops, entry_bytes, header_bytes) catch null;
        } else |_| null;
        w.t_handoff_ns = clock.monotonicNs();
        self.stats.self_window_span.record(w.t_handoff_ns -| w.t_turns_ns);
        if (w.image == null) {
            self.abandonSelfApplied(w, error.OutOfMemory);
            return;
        }
        self.stats.lane_handoffs.inc();
        if (w.held) self.stats.windows_held.inc() else if (w.acct().padMajority()) self.stats.windows_unshadowed.inc() else self.stats.windows_pipelined.inc();
        self.deps.lane.submit(self.deps.lane.ctx, w) catch |e| self.abandonSelfApplied(w, e);
    }

    /// A self-applied window the lane never took: a window with a member
    /// answered ahead of the landing rolls nothing back and latches the
    /// volume FAILED (§BB.5); one nobody was acked from rolls back as any
    /// abandoned window.
    fn abandonSelfApplied(self: *Conveyor, w: *Window, err: Error) void {
        if (!w.anyAnswered()) return self.abandonWindow(w, err);
        self.stats.acked_write_failures.inc();
        if (self.deps.latch.failStop()) {
            log.err("meta volume {s}: an ACKED window could not be handed to the journal lane ({s}) — volume marked FAILED; mutations return EIO until remount; the supervisor's tail replay carries the acked bytes", .{ self.deps.volume, @errorName(err) });
        }
        self.completeAbandoned(w.res);
        self.finishWindow(w, err);
    }

    /// One member's terminal bookkeeping: the tee, its kernel's
    /// invalidation, the lease epochs, the guards, and — unless it is the
    /// committer answering itself — the send.
    fn answerOne(self: *Conveyor, m: *Queued, outcome: Outcome, send: bool) void {
        if (outcome) |_| {
            if (self.tee.load(.acquire)) |tee| tee.note(tee.ctx, m.q.recs);
        } else |_| {}
        if (self.kernel.load(.acquire)) |view| noteKernel(view, &m.q, if (outcome) |_| false else |_| true);
        noteLease(&m.q);
        m.q.guards.release();
        if (outcome) |_| self.stats.acks.inc() else |_| self.stats.refusals.inc();
        self.ack_spin.ack.sample(clock.monotonicNs() -| m.enqueued_ns);
        if (send) m.done.send(self.deps.io, outcome) catch {};
        m.deinit();
    }

    /// A window left its applier (submitted, or abandoned on its way): the
    /// pass's pacing wake.
    fn leftApply(self: *Conveyor) void {
        _ = self.in_apply.fetchSub(1, .acq_rel);
        self.applied.notifyWaiters(self.deps.io);
    }

    /// Stage A's second half on the APPLIER's thread (§AW): A3 resolve →
    /// A4 lock the union → A5 revalidate (re-resolve on a moved leaf; the
    /// seqs are the window's already) → A7 pre-images → A9 apply in queue
    /// order → A10 the threshold → unlock → A11 the kick → A12 encode →
    /// A13 submit. Every exit completes the reservation: a refusal before
    /// any apply abandons the window (nothing to roll back); a member
    /// whose apply fails is rolled out in-window and its range left an
    /// unwritten hole, as before.
    pub fn applyWindow(self: *Conveyor, window: *Window) void {
        const gpa = self.deps.gpa;
        const t_apply = clock.monotonicNs();
        defer self.leftApply();
        self.holdApplyForTest();
        var arena_state = std.heap.ArenaAllocator.init(gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        const members = window.members;
        const wa = window.arena.allocator();
        var attempt: u32 = 0;
        while (true) : (attempt += 1) {
            if (attempt >= COMMIT_RETRY_BUDGET) {
                log.err("meta volume {s}: commit retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.deps.volume});
                self.abandonWindow(window, error.RetryBudgetExhausted);
                return;
            }
            // A3.
            var r = self.resolve(arena, members, .apply) catch |e| {
                self.abandonWindow(window, e);
                return;
            };
            // A4.
            const dirties = arena.alloc(*NodeDirty, r.lock_set.len) catch {
                r.releaseRefs();
                self.abandonWindow(window, error.OutOfMemory);
                return;
            };
            const t_leaf = clock.monotonicNs();
            self.lockAll(r.lock_set, dirties, .apply);
            // A5.
            if (stale(members, &r)) {
                self.unlockAll(r.lock_set);
                self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
                r.releaseRefs();
                tree.stats.meta_kv_commit_smo_retries.inc();
                continue;
            }
            // A7: pre-images, first touch per (tree, key).
            window.undo = self.captureUndo(wa, members, &r) catch |e| {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                self.abandonWindow(window, e);
                return;
            };
            // A9: apply in queue order; a failing member rolls out alone.
            self.applyMembers(members, &r, dirties, window.res.start, window.seq_base, window.failed);
            // A10: the writeback threshold, inside the window.
            var threshold_crossed = false;
            for (r.lock_set, dirties) |n, d| {
                if (d.overlayBytes() >= self.deps.cache.cfg.writeback_delta_bytes) {
                    if (self.treeOf(n)) |t| {
                        t.enqueueMaintenance(n.addr);
                        threshold_crossed = true;
                    }
                }
            }
            self.unlockAll(r.lock_set);
            self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
            r.releaseRefs();
            // A11.
            if (threshold_crossed) self.deps.checkpoint.kickThreshold(self.deps.checkpoint.ctx);
            break;
        }
        self.stats.pass_leaf_locks.record(clock.monotonicNs() -| t_apply);

        // A12: the survivors' entries, ONE submission, outside every lock.
        var parts: std.ArrayList(journal.BatchPart) = .empty;
        var pos_cursor = window.res.start;
        var survivors: u64 = 0;
        var encode_failed = false;
        for (window.members, window.failed) |*m, f| {
            const start = pos_cursor;
            pos_cursor += m.q.len;
            if (f != null) continue;
            const frames = m.q.framesAlloc() catch {
                encode_failed = true;
                break;
            };
            parts.append(arena, .{ .res = Reservation.unpadded(start, m.q.len), .records = frames }) catch {
                encode_failed = true;
                break;
            };
            survivors += 1;
            if (self.deps.door) |d| d.frontier(d.ctx, &m.q.door, pos_cursor);
        }
        window.entries = survivors;
        if (!encode_failed and parts.items.len != 0) {
            window.image = journal.buildBatchWrite(gpa, self.deps.ring_layout, self.deps.geo, self.deps.appender_id, window.res, parts.items) catch null;
            encode_failed = window.image == null;
        }
        window.t_handoff_ns = clock.monotonicNs();
        self.stats.apply_total.record(window.t_handoff_ns -| t_apply);
        if (encode_failed) {
            // A failed write with nothing landed — the lane's verdict for
            // it, run here (nothing was submitted).
            self.abandonWindow(window, error.Corrupt);
            return;
        }
        if (parts.items.len == 0) {
            // Every member failed at apply: an unwritten hole, no survivor
            // to ack — complete the reservation, checkpoint past the hole,
            // answer the apply errors.
            self.completeAbandoned(window.res);
            _ = self.deps.checkpoint.checkpointPast(self.deps.checkpoint.ctx, window.res.paddedEnd());
            self.finishWindow(window, {});
            return;
        }
        // A13: handoff. The window's class for the pacing law (§L).
        self.stats.lane_handoffs.inc();
        if (window.held) self.stats.windows_held.inc() else if (window.acct().padMajority()) self.stats.windows_unshadowed.inc() else self.stats.windows_pipelined.inc();
        // §BB.1: the ack ahead of the landing — for a window of THIS mount's
        // kernel's own ops alone: a wire-served step, a control record (a
        // lease, a grant, the claim), a job's or a tool's commit is acked at
        // the landing, so every word of the symmetric protocol still means
        // DURABLE (charter §5.5: idempotent against durable state — a lease
        // a dead manager had acked and lost poisoned its appender at the
        // reclaim, UnknownLease). A window with an apply hole waits for the
        // hole checkpoint at the landing; the image goes to the tail BEFORE
        // the handoff (a crash after the ack finds it there); a slot refused
        // falls back to the landing (counted).
        var ack_now = self.deps.ack_at_apply and !window.applyHoles() and kernelOriginOnly(window.members);
        if (ack_now) if (self.deps.tail) |t| {
            window.tail_slot = t.claim(t.ctx, window.seq_base, window.res.start, window.ops());
            if (window.tail_slot == null) {
                self.stats.tail_slot_misses.fire("the journal tail's slot table refused a window; acked at the landing");
                ack_now = false;
            }
        };
        // Two parties while the applier answers: the lane's verdict may
        // land before the answer and must not free the window under it.
        if (ack_now) window.parties.store(2, .release);
        self.deps.lane.submit(self.deps.lane.ctx, window) catch |e| {
            self.abandonWindow(window, e);
            if (ack_now) window.release();
            return;
        };
        if (ack_now) {
            // The handoff precedes the ack: an fsync issued after the ack
            // finds the window in the lane's queue (§BB.4).
            if (window.claimAck(Window.ACK_APPLIER)) {
                self.stats.acks_at_apply.inc();
                self.answerMembers(window, {});
            }
            window.release();
        }
    }

    /// `test_seams.hold_next_apply`: the one armed window's applier parks
    /// (never spins: it shares its lane with the window it must let pass)
    /// until the test releases it; register-then-check against the release.
    fn holdApplyForTest(self: *Conveyor) void {
        if (test_seams.hold_next_apply.load(.acquire) != 1) return;
        if (test_seams.hold_next_apply.cmpxchgStrong(1, 2, .acq_rel, .acquire) != null) return;
        test_seams.apply_held.store(1, .release);
        const io = self.deps.io;
        while (test_seams.hold_next_apply.load(.acquire) == 2) {
            var n = test_seams.apply_release.notified();
            _ = n.enable(io);
            if (test_seams.hold_next_apply.load(.acquire) != 2) {
                n.cancel(io);
                break;
            }
            n.waitUncancelable(io);
        }
    }

    fn anyGrouped(members: []const Queued) bool {
        for (members) |m| if (m.grouped) return true;
        return false;
    }

    pub fn reservedHwm(self: *const Conveyor) u64 {
        return self.reserved_hwm.load(.acquire);
    }

    /// Every member is a request this mount's kernel sent (`tx.kernel_origin`).
    fn kernelOriginOnly(members: []const Queued) bool {
        for (members) |m| if (!m.q.kernel_origin) return false;
        return true;
    }

    /// Complete a reservation whose bytes never land (the hole completes
    /// for the watermark) and kick the lane: a stage-B prefix wait may sit
    /// behind it.
    fn completeAbandoned(self: *Conveyor, res: Reservation) void {
        self.deps.reg.complete(self.deps.core, res);
        if (self.deps.smo_journal.device) |d| d.completed(d.ctx);
    }

    /// A7: one pre-image per distinct `(tree, key)` across the batch, first
    /// touch wins (same-key members are Δtime merge records by the co-queue
    /// invariant; their shared pre-image is the pre-batch fold).
    fn captureUndo(self: *Conveyor, wa: Allocator, members: []const Queued, r: *const Resolved) Error![]tx.UndoKey {
        var undo: std.ArrayList(tx.UndoKey) = .empty;
        for (members, 0..) |m, qi| {
            recs: for (m.q.recs, 0..) |rec, ri| {
                for (undo.items) |u| {
                    if (u.tree_id == rec.tree_id and std.mem.eql(u8, u.key, rec.key)) continue :recs;
                }
                const leaf = r.leaves[qi][ri];
                const snap = leaf.snapshotLoad();
                defer snap.release();
                var l = snap.value.lookup(self.deps.gpa, rec.key) catch |e| return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Corrupt,
                };
                defer l.deinit();
                const pre: tx.PreImage = switch (l) {
                    .live => |lv| .{ .live = try wa.dupe(u8, lv.value) },
                    .tombstone => .tombstone,
                    .absent => .absent,
                };
                // The key copied: the member's own goes with its ack, which
                // may precede the rollback's need of it (§BB.5).
                try undo.append(wa, .{ .tree_id = rec.tree_id, .key = try wa.dupe(u8, rec.key), .pre = pre });
            }
        }
        return undo.items;
    }

    /// A9: per member in queue order, grouped per locked node; the floor
    /// contribution is the member's ENTRY START (FIND-SMO-TAIL §1b: a leaf
    /// holding only rec[j>0] of a multi-leaf tx must not pin the tail
    /// mid-entry). A member whose apply fails is rolled OUT in-window.
    fn applyMembers(self: *Conveyor, members: []Queued, r: *const Resolved, dirties: []*NodeDirty, res_start: u64, seq_base: u64, failed: []?Error) void {
        const gpa = self.deps.gpa;
        const poison = test_seams.poison_apply_ino.load(.monotonic);
        var pos_cursor = res_start;
        var stamp_cursor = seq_base;
        var group: std.ArrayList(OwnedRec) = .empty;
        defer group.deinit(gpa);
        for (members, 0..) |*m, qi| {
            const entry_start = pos_cursor;
            pos_cursor += m.q.len;
            const member_span = SeqSpan.stamped(stamp_cursor, m.q.len);
            stamp_cursor += m.q.len;
            var apply_err: ?Error = null;
            for (r.lock_set, dirties, 0..) |n, d, gi| {
                group.clearRetainingCapacity();
                for (m.q.recs, r.leaf_of[qi]) |rec, g| {
                    if (g != gi) continue;
                    const owned = OwnedRec.fromRecordRef(gpa, .{ .key = rec.key, .seq = rec.seq, .kind = rec.kind, .value = rec.value }) catch {
                        apply_err = error.OutOfMemory;
                        break;
                    };
                    group.append(gpa, owned) catch {
                        var o = owned;
                        o.deinit();
                        apply_err = error.OutOfMemory;
                        break;
                    };
                }
                if (apply_err != null) {
                    for (group.items) |*o| o.deinit();
                    break;
                }
                if (group.items.len == 0) continue;
                n.applyLocked(d, group.items, entry_start) catch |e| {
                    apply_err = switch (e) {
                        error.Superseded => error.Corrupt,
                        error.ReaderMayNotMutate, error.LeafLeaseRefused => error.PartitionRefused,
                        error.OutOfMemory => error.OutOfMemory,
                    };
                    break;
                };
            }
            if (apply_err == null and poison != 0 and namesInode(m.q.recs, poison)) apply_err = error.Corrupt;
            if (apply_err) |e| {
                for (m.q.recs, r.leaf_of[qi]) |rec, g| {
                    _ = r.lock_set[g].removeOverlayRecordsLocked(dirties[g], rec.key, member_span) catch {
                        self.divergedFromReplay(&self.stats.divergence_rollout, "a failed member's record could not be removed from its leaf in-window (out of memory); its entry is never written");
                    };
                }
                failed[qi] = e;
            }
        }
    }

    fn namesInode(recs: []const tx.Rec, ino: u64) bool {
        const ik = key.inodeKey(ino);
        for (recs) |r| {
            if (r.tree_id == key.TREE_INODES and std.mem.eql(u8, r.key, &ik)) return true;
        }
        return false;
    }

    // ---- stage B's callbacks ----

    /// Register a window for stage B's hooks (the rollback addresses it by
    /// its reservation). A window the map cannot name must not be
    /// submitted: its failed write could not be rolled back. Nothing is
    /// allocated here: the map was sized for the whole population at init,
    /// and a population past it is refused as the map growth it would need.
    fn track(self: *Conveyor, w: *Window) Allocator.Error!void {
        self.inflight_lock.lock();
        defer self.inflight_lock.unlock();
        if (self.inflight.count() >= self.inflight_cap) return error.OutOfMemory;
        self.inflight.putAssumeCapacity(w.res.start, w);
        const n = self.stats.windows_inflight.load() + 1;
        self.stats.windows_inflight.set(n);
        if (n > self.stats.windows_inflight_hwm.load()) self.stats.windows_inflight_hwm.set(n);
    }

    fn untrack(self: *Conveyor, w: *Window) void {
        const removed = blk: {
            self.inflight_lock.lock();
            defer self.inflight_lock.unlock();
            const r = self.inflight.remove(w.res.start);
            if (r) self.stats.windows_inflight.sub(1);
            break :blk r;
        };
        if (removed) if (self.deps.growth_gate) |g| g.windowLeft(g.ctx);
    }

    fn lookupInflight(self: *Conveyor, start: u64) ?*Window {
        self.inflight_lock.lock();
        defer self.inflight_lock.unlock();
        return self.inflight.get(start);
    }

    /// The lane's failed-write arm for the window at `window_res`: the
    /// seq-conditional rollback over its stamped span (stage B's ONLY
    /// leaf-lock take; runs on the lane thread).
    pub fn rollbackHook(self: *Conveyor, span: SeqSpan, window_res: Reservation) void {
        const w = self.lookupInflight(window_res.start) orelse return;
        assert(w.seq_base == span.lo);
        // §BB.5: the lane claims the answer BEFORE it rolls back; a window
        // the applier already answered keeps its RAM (the ack stands) and
        // the volume latches FAILED — the tail's replay retries the bytes.
        const acked = if (w.self_applied) w.anyAnswered() else !w.claimAck(Window.ACK_LANE);
        if (!acked) {
            self.rollbackFailedTx(span, w.undo);
            return;
        }
        self.stats.acked_write_failures.inc();
        if (self.deps.latch.failStop()) {
            log.err("meta volume {s}: the journal write of an ACKED window failed ({d} entries at [{d}, {d})) — volume marked FAILED; mutations return EIO until remount; the supervisor's tail replay retries the bytes at the daemon's death", .{ self.deps.volume, w.members.len, window_res.start, window_res.end() });
        }
    }

    /// A window the lane never ran (a refused submit, an encode refusal):
    /// the failed-write verdict, synchronously — complete the reservation
    /// as abandoned (the hole completes for the watermark; replay's
    /// checksum walk drops it), roll back, count, checkpoint past the hole,
    /// answer every member. A submit refused `VolumeFailed` is the latch
    /// landing between A1's screen and the handoff: the fail-stop working,
    /// counted, not the tripwire.
    fn abandonWindow(self: *Conveyor, w: *Window, err: Error) void {
        if (err == error.VolumeFailed and self.deps.latch.isFailed()) {
            self.stats.windows_abandoned_failed_volume.inc();
            log.info("meta volume {s}: a conveyor window was refused by the lane on a FAILED volume — rolled back, every member answered", .{self.deps.volume});
        } else {
            var why: [160]u8 = undefined;
            self.stats.pass_panics.fire(std.fmt.bufPrint(&why, "a conveyor window was abandoned before stage B observed its write on a HEALTHY volume ({s}, {d} members)", .{ @errorName(err), w.members.len }) catch "a conveyor window was abandoned before stage B observed its write on a HEALTHY volume");
        }
        self.completeAbandoned(w.res);
        self.rollbackFailedTx(w.span(), w.undo);
        _ = self.deps.latch.noteFailure(.journal_write);
        if (!self.deps.checkpoint.checkpointPast(self.deps.checkpoint.ctx, w.res.paddedEnd())) {
            journal.meta_kv_journal_hole_checkpoint_failures.inc();
            if (self.deps.latch.admitsLine(.hole_checkpoint)) log.err("meta volume {s}: post-failure checkpoint could not drain the journal hole at {d} (volume escalating)", .{ self.deps.volume, w.res.paddedEnd() });
            _ = self.deps.latch.noteFailure(.journal_write);
        }
        self.finishWindow(w, err);
    }

    /// The terminal outcome of a window: per member in journal order the
    /// in-window apply failure else the lane's verdict; guards drop BEFORE
    /// the ack; the window dies.
    pub fn finishWindow(self: *Conveyor, w: *Window, result: Error!void) void {
        self.untrack(w);
        // The landing's wake for a pass holding on this window (§L): after
        // the in-flight count dropped, so the pass's re-read sees 0; the
        // round trip feeds the hold's predictor first.
        const rtt = clock.monotonicNs() -| w.t_handoff_ns;
        self.rtt.sample(rtt);
        self.landed.notifyWaiters(self.deps.io);
        // The token union leaves the flight with this window — both arms
        // (fan-out and rollback): parked grants wake to the post-commit
        // (or rolled-back) records.
        if (w.token_union.len != 0) if (self.tokens.load(.acquire)) |hook| hook.settle(hook.ctx, w.token_union);
        self.stats.window_total.record(rtt);
        // The answer, unless the applier gave it ahead of the landing
        // (§BB.1); the rollback arm claimed it already (§BB.5).
        if (w.self_applied) {
            // The members that waited for the landing (§BC: a served verb,
            // a control record, a tool; a member whose tail slot was
            // refused); the others answered themselves at the apply.
            for (w.members, w.parts, w.failed) |*m, *p, f| {
                if (p.answered) continue;
                self.stats.acks_at_landing.inc();
                self.answerOne(m, if (f) |e| e else result, true);
            }
            // The tail slots of a window that LANDED; a failed write's stay
            // written for the supervisor's replay (§BB.5).
            if (result) |_| {
                for (w.parts) |p| if (p.tail_slot) |slot| if (self.deps.tail) |t| t.release(t.ctx, slot);
            } else |_| {}
            w.release();
            return;
        }
        const owner = w.ack_owner.load(.acquire);
        if (owner == Window.ACK_LANE or w.claimAck(Window.ACK_LANE)) {
            self.stats.acks_at_landing.inc();
            self.answerMembers(w, result);
        }
        if (result) |_| {
            if (w.tail_slot) |slot| if (self.deps.tail) |t| t.release(t.ctx, slot);
        } else |_| {}
        w.release();
    }

    /// Per member in journal order: the in-window apply failure else
    /// `result`; the tee, the kernel's invalidation, the lease epochs,
    /// then the ack — guards drop BEFORE it (`answer`).
    fn answerMembers(self: *Conveyor, w: *Window, result: Error!void) void {
        // The tee sees every member that reached SUCCESS, before the
        // window's arena (the members' key copies) goes.
        if (self.tee.load(.acquire)) |tee| {
            if (result) |_| {
                for (w.members, w.failed) |*m, f| if (f == null) tee.note(tee.ctx, m.q.recs);
            } else |_| {}
        }
        const kernel = self.kernel.load(.acquire);
        for (w.members, w.failed) |*m, f| {
            const outcome: Outcome = if (f) |e| e else result;
            if (kernel) |view| noteKernel(view, &m.q, if (outcome) |_| false else |_| true);
            noteLease(&m.q);
            self.answer(m, outcome);
        }
    }

    /// One member's invalidation in this mount's kernel (`KernelView`): a
    /// change its kernel asked for and got is its reply's to tell, one its
    /// own channel tells (`kernel_told`) is that channel's; the
    /// migration engine's delta copy re-homes records without changing a
    /// name or an attribute; a `token_quiet` control record is no kernel's.
    fn noteKernel(view: *const KernelView, q: *const tx.QueuedTx, failed: bool) void {
        // `token_quiet`: a control record no kernel caches.
        if (q.door_bypass or q.token_quiet) return;
        if ((q.kernel_origin or q.kernel_told) and !failed) return;
        // The objects in chunks through a stack buffer (no allocation on
        // the lane); adjacent duplicates folded (a member's records are
        // grouped by object).
        var buf: [32]u64 = undefined;
        var n: usize = 0;
        var named = false;
        const unnamed = unnamedDentries(q);
        for (q.recs) |r| {
            if (r.tree_id != key.TREE_INODES and r.tree_id != key.TREE_XATTRS) continue;
            const o = token_table.recordObjectIno(r.tree_id, r.key) orelse continue;
            if (n != 0 and buf[n - 1] == o) continue;
            if (n == buf.len) {
                view.invalidate(view.ctx, if (named) &.{} else q.names.slice(), unnamed and !named, buf[0..n]);
                named = true;
                n = 0;
            }
            buf[n] = o;
            n += 1;
        }
        if (n != 0 or (!named and (q.names.len != 0 or unnamed))) view.invalidate(view.ctx, if (named) &.{} else q.names.slice(), unnamed and !named, buf[0..n]);
    }

    /// The layout lease epochs (`data/lease_epoch.zig`): every inode whose
    /// record or xattrs this member changed — its layout, its size — bumps
    /// its stripe here, after the RAM apply and before the ack, whatever
    /// the origin (a direct client's cached layout is no kernel's cache: the
    /// kernel-origin exemption does not apply). A failed member's RAM was
    /// readable until the rollback: bumped too.
    fn noteLease(q: *const tx.QueuedTx) void {
        var last: u64 = 0;
        for (q.recs) |r| {
            if (r.tree_id != key.TREE_INODES and r.tree_id != key.TREE_XATTRS) continue;
            // An access stamp alone (the atime posture's drain, charter §4
            // "Access time") changes no byte a cached layout could name.
            if (r.tree_id == key.TREE_INODES and r.kind == .delta and record.inodeDeltaMask(r.value) == record.DELTA_ATIME) continue;
            const o = token_table.recordObjectIno(r.tree_id, r.key) orelse continue;
            if (o == last) continue;
            last = o;
            lease_epoch.bump(o);
        }
    }

    // ---- the seq-conditional rollback (§5.2 "Failure") ----

    const Comp = struct { tree_id: u8, key: []const u8, kind: record.RecordKind, value: []const u8 };

    /// RAM ≠ replay: a roll-out, rollback or compensation exited before it
    /// could make RAM agree with what the ring will replay. Loud, counted
    /// on the site's tripwire, and TERMINAL — the volume latches FAILED
    /// (mutations EIO until remount, where replay rebuilds RAM) rather than
    /// serving a state no recovery can reproduce (charter §1.8). On a
    /// volume already FAILED the exit is the latch's consequence (a
    /// compensation entry's write on a device that answers EIO): the
    /// counted outcome, one info line, the tripwire untouched.
    fn divergedFromReplay(self: *Conveyor, site: *counters.Tripwire, detail: []const u8) void {
        if (self.deps.latch.isFailed()) {
            self.stats.divergences_failed_volume.inc();
            log.info("meta volume {s}: RAM diverged from replay on a FAILED volume ({s}: {s}) — the remount's replay rebuilds RAM", .{ self.deps.volume, site.site, detail });
            return;
        }
        site.fire(detail);
        if (self.deps.latch.failStop()) {
            log.err("meta volume {s}: RAM diverged from replay ({s}: {s}) — volume marked FAILED; mutations return EIO until remount", .{ self.deps.volume, site.site, detail });
        }
    }

    /// The compensation entry's device write failed: RAM holds the
    /// compensation the ring will not replay, so the volume latches FAILED
    /// at once (mutations EIO until the remount, where replay rebuilds
    /// RAM) — a device event counted as
    /// `meta_conveyor_compensation_write_failures`, never a tripwire.
    fn compensationWriteFailed(self: *Conveyor) void {
        self.stats.compensation_write_failures.inc();
        if (self.deps.latch.failStop()) {
            log.err("meta volume {s}: the compensation entry's write failed — the escaped record stands for replay while RAM holds its compensation; volume marked FAILED, mutations return EIO until remount", .{self.deps.volume});
        } else {
            log.info("meta volume {s}: the compensation entry's write failed on a FAILED volume — the remount's replay rebuilds RAM", .{self.deps.volume});
        }
    }

    /// Phase 1 under re-acquired ascending locks: remove every record of the
    /// window's STAMPED span for each undo key; probe the key's newest seq —
    /// still inside the span ⇒ a record escaped the open overlay (a freeze
    /// raced the failed write) ⇒ a compensating record. A NEWER committed
    /// record on the key (a Δtime merge) stands: LWW-correct. Phase 2
    /// commits the compensations through the checkpoint-class direct writer.
    pub fn rollbackFailedTx(self: *Conveyor, span: SeqSpan, undo: []const tx.UndoKey) void {
        const gpa = self.deps.gpa;
        self.stats.rollbacks.inc();
        var arena_state = std.heap.ArenaAllocator.init(gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        var comps: std.ArrayList(Comp) = .empty;
        const site = &self.stats.divergence_rollback;
        var attempt: u32 = 0;
        while (attempt < COMMIT_RETRY_BUDGET) : (attempt += 1) {
            comps.clearRetainingCapacity();
            const leaves = arena.alloc(*CachedNode, undo.len) catch return self.divergedFromReplay(site, "out of memory before the rollback's leaf resolution; the window's records stay applied");
            var n: usize = 0;
            defer for (leaves[0..n]) |l| l.release();
            for (undo) |u| {
                const t = self.treeFor(u.tree_id, u.key) orelse return self.divergedFromReplay(site, "a dirty key names a tree that is not mounted; the window's records stay applied");
                leaves[n] = t.resolveLeaf(u.key) catch return self.divergedFromReplay(site, "leaf resolution failed for a dirty key; the window's records stay applied");
                n += 1;
            }
            const lock_set = arena.dupe(*CachedNode, leaves) catch return self.divergedFromReplay(site, "out of memory building the rollback's lock set; the window's records stay applied");
            core_sort.stable(*CachedNode, lock_set, {}, addrLess);
            var out: usize = 0;
            for (lock_set) |l| {
                if (out > 0 and lock_set[out - 1].addr == l.addr) continue;
                lock_set[out] = l;
                out += 1;
            }
            const set = lock_set[0..out];
            const dirties = arena.alloc(*NodeDirty, set.len) catch return self.divergedFromReplay(site, "out of memory taking the rollback's locks; the window's records stay applied");
            self.lockAll(set, dirties, .apply);
            var is_stale = false;
            for (undo, leaves) |u, leaf| {
                if (leaf.state.isSuperseded() or key.lessThan(u.key, leaf.min_key) or key.lessThan(leaf.max_key, u.key)) is_stale = true;
            }
            if (is_stale) {
                self.unlockAll(set);
                tree.stats.meta_kv_commit_smo_retries.inc();
                continue;
            }
            var diverged = false;
            for (undo, leaves) |u, leaf| {
                const gi = for (set, 0..) |l, i| {
                    if (l.addr == leaf.addr) break i;
                } else unreachable;
                _ = leaf.removeOverlayRecordsLocked(dirties[gi], u.key, span) catch {
                    diverged = true;
                };
                const snap = leaf.snapshotLoad();
                defer snap.release();
                if (tx.UndoKey.escaped(span, snap.value.newestSeqOf(u.key))) {
                    const c = u.compensation();
                    comps.append(arena, .{ .tree_id = u.tree_id, .key = u.key, .kind = c.kind, .value = c.value }) catch {
                        diverged = true;
                    };
                }
            }
            self.unlockAll(set);
            if (diverged) return self.divergedFromReplay(site, "out of memory removing a window's records or recording an escaped record's compensation");
            break;
        } else {
            return self.divergedFromReplay(site, "the rollback's lock-revalidate retry budget is exhausted (SMO protocol bug); the window's records stay applied");
        }
        if (comps.items.len == 0) return;
        self.commitCompensation(span, comps.items);
    }

    /// Phase 2 (rare): the compensating records as ONE checkpoint-class
    /// entry through the direct writer — admitted from the reserve (never
    /// parks behind user admissions), leaves locked ascending, skip-if-newer
    /// re-checked under the locks, reserved inside, applied, written after
    /// release. An escaped record left uncompensated is RAM ≠ replay: every
    /// exit short of the written entry is a divergence (terminal); only the
    /// entry write itself is the ring's crash-equivalent hole class.
    fn commitCompensation(self: *Conveyor, span: SeqSpan, comps: []const Comp) void {
        const gpa = self.deps.gpa;
        const j = self.deps.smo_journal;
        const site = &self.stats.divergence_compensation;
        var arena_state = std.heap.ArenaAllocator.init(gpa);
        defer arena_state.deinit();
        const arena = arena_state.allocator();
        const frames = arena.alloc(journal.RecordFrame, comps.len) catch return self.divergedFromReplay(site, "out of memory building the compensation frames; escaped records stand uncompensated");
        for (comps, frames) |c, *f| f.* = .{ .tag = journal.tagFor(c.tree_id, 0), .key = c.key, .kind = @fromBackingInt(@intCast(@backingInt(c.kind))), .seq = 0, .value = c.value };
        const len = journal.entryLenFor(frames) catch return self.divergedFromReplay(site, "the compensation entry does not encode; escaped records stand uncompensated");
        const adm = self.deps.core.tryAdmit(len, .checkpoint) orelse return self.divergedFromReplay(site, "the checkpoint reserve could not admit the compensation entry; escaped records stand uncompensated");
        var attempt: u32 = 0;
        while (attempt < COMMIT_RETRY_BUDGET) : (attempt += 1) {
            const leaves = arena.alloc(*CachedNode, comps.len) catch break;
            var n: usize = 0;
            defer for (leaves[0..n]) |l| l.release();
            for (comps) |c| {
                const t = self.treeFor(c.tree_id, c.key) orelse break;
                leaves[n] = t.resolveLeaf(c.key) catch break;
                n += 1;
            }
            if (n != comps.len) break;
            const lock_set = arena.dupe(*CachedNode, leaves) catch break;
            core_sort.stable(*CachedNode, lock_set, {}, addrLess);
            var out: usize = 0;
            for (lock_set) |l| {
                if (out > 0 and lock_set[out - 1].addr == l.addr) continue;
                lock_set[out] = l;
                out += 1;
            }
            const set = lock_set[0..out];
            const dirties = arena.alloc(*NodeDirty, set.len) catch break;
            self.lockAll(set, dirties, .apply);
            var is_stale = false;
            for (comps, leaves) |c, leaf| {
                if (leaf.state.isSuperseded() or key.lessThan(c.key, leaf.min_key) or key.lessThan(leaf.max_key, c.key)) is_stale = true;
            }
            if (is_stale) {
                self.unlockAll(set);
                continue;
            }
            // Skip-if-newer under the locks: a key whose newest record is
            // past the failed span was committed since — it stands.
            const live = arena.alloc(bool, comps.len) catch {
                self.unlockAll(set);
                break;
            };
            var any = false;
            for (comps, leaves, live) |c, leaf, *lv| {
                const snap = leaf.snapshotLoad();
                defer snap.release();
                const newest = snap.value.newestSeqOf(c.key);
                lv.* = newest == null or newest.? < span.hi;
                any = any or lv.*;
            }
            if (!any) {
                self.unlockAll(set);
                self.deps.core.release(adm);
                return;
            }
            const reg = j.reserve(adm) catch {
                // The core transferred the claim before the registry's append
                // failed: the range is the ring's now, un-registered.
                self.unlockAll(set);
                return self.divergedFromReplay(site, "the compensation's reservation could not be registered (out of memory); escaped records stand uncompensated");
            };
            for (frames, 0..) |*f, i| f.seq = reg.seq_base + i;
            // The frame IS written below, so a record that cannot be applied
            // to RAM is a divergence in the other direction.
            var diverged = false;
            for (comps, leaves, live, frames) |c, leaf, lv, f| {
                if (!lv) continue;
                const gi = for (set, 0..) |l, i| {
                    if (l.addr == leaf.addr) break i;
                } else unreachable;
                var one = [_]OwnedRec{OwnedRec.fromRecordRef(gpa, .{ .key = c.key, .seq = f.seq, .kind = c.kind, .value = c.value }) catch {
                    diverged = true;
                    continue;
                }};
                leaf.applyLocked(dirties[gi], &one, reg.res.start) catch {
                    diverged = true;
                };
            }
            self.unlockAll(set);
            if (diverged) self.divergedFromReplay(site, "a compensating record could not be applied to RAM while its frame is written to the ring");
            self.stats.compensations.add(comps.len);
            // The entry's hole (the write never landed) leaves the escaped
            // bset record standing for replay while RAM holds the
            // compensation: unlike an SMO entry's hole, nothing folds back.
            // A device refusing the write (`SmoJournal.commitEntry`: a device
            // write failure is `error.Corrupt`) is the fail-stop lattice's own
            // event, never a "cannot happen": FAILED at once, counted, loud
            // once (generic/475: the rollback's own writes meet EIO).
            j.commitEntry(reg.res, frames) catch |e| switch (e) {
                error.Corrupt => self.compensationWriteFailed(),
                else => self.divergedFromReplay(site, "the compensation entry could not be built; the escaped record stands for replay while RAM holds its compensation"),
            };
            return;
        }
        self.deps.core.release(adm);
        self.divergedFromReplay(site, "the compensation could not resolve or lock its leaves (out of memory or the lock-revalidate budget); escaped records stand uncompensated");
    }
};

// ---------------------------------------------------------------------------
// The in-RAM lane: stage B's protocol over a ring image, under test
// control (synchronous, or held windows landed / failed in any order).
// ---------------------------------------------------------------------------

pub const RamLane = struct {
    gpa: Allocator,
    io: Io,
    core: *jc.JournalCore,
    reg: *jc.InflightRegistry,
    latch: *journal.FailureLatch,
    ring_stats: *journal.RingStats,
    /// The ring image with power-cut semantics.
    image: *journal.VolatileImage,
    strict: bool,
    /// Windows the lane holds (handoff order) when `hold` is set.
    hold: bool = false,
    held: std.ArrayList(Held) = .empty,
    /// The held list's lock for the threaded contracts (a pass thread
    /// submitting against a device thread landing); uncontended elsewhere.
    mutex: std.Io.Mutex = .init,
    /// The head window's handoff instant (maxInt = none held), maintained
    /// under the lock: the device thread polls it lock-free and takes the
    /// lock only to land (a lock per poll starved the pass's submit).
    head_issued_ns: std.atomic.Value(u64) = .init(std.math.maxInt(u64)),
    /// Fail the next N windows' writes synthetically (nothing lands).
    fail_next: u32 = 0,
    /// Arm this allocator to fail from the rollback hook's FIRST allocation
    /// on (the conveyor's `gpa` is its face): the rollback-under-OOM seam.
    oom_on_rollback: ?*std.testing.FailingAllocator = null,
    /// The conveyor's hooks: set once the conveyor exists.
    conveyor: ?*Conveyor = null,
    checkpoint_past: ?*const fn (ctx: *anyopaque, pos: u64) bool = null,
    checkpoint_ctx: ?*anyopaque = null,
    barriers: u64 = 0,
    needs_flush: bool = false,
    durability_passes: u64 = 0,
    acks: u64 = 0,
    ack_order: std.ArrayList(u64) = .empty,
    /// Landed windows whose reservations completed, awaiting the
    /// completed prefix (an earlier window still with its applier, §AW);
    /// its length mirrored for the device thread's lock-free poll.
    completed: std.ArrayList(Held) = .empty,
    completed_pending: std.atomic.Value(usize) = .init(0),
    /// The lane's window cells (`journal_lane.windowCells`' law over the
    /// ring's pages): a window submitted while that many are held or
    /// awaiting their prefix is refused `ConveyorFull`, as the device
    /// lane's empty window pool refuses `LaneQueueFull`.
    window_cells: usize,

    pub const Held = struct { w: *Window, landed: bool = false, fail: bool = false, issued_ns: u64 = 0 };

    pub fn init(gpa: Allocator, io: Io, core: *jc.JournalCore, reg: *jc.InflightRegistry, latch: *journal.FailureLatch, ring_stats: *journal.RingStats, image: *journal.VolatileImage, strict: bool) RamLane {
        return .{ .gpa = gpa, .io = io, .core = core, .reg = reg, .latch = latch, .ring_stats = ring_stats, .image = image, .strict = strict, .window_cells = queueCapacity(core.geo.pages) / 2 };
    }

    pub fn deinit(self: *RamLane) void {
        assert(self.held.items.len == 0);
        assert(self.completed.items.len == 0);
        self.held.deinit(self.gpa);
        self.completed.deinit(self.gpa);
        self.ack_order.deinit(self.gpa);
    }

    pub fn seam(self: *RamLane) LaneSeam {
        return .{ .ctx = self, .submit = submit, .barrier = barrierFn, .window_cells = self.window_cells };
    }

    fn submit(ctx: *anyopaque, w: *Window) Error!void {
        const self: *RamLane = @ptrCast(@alignCast(ctx));
        if (self.latch.isFailed()) return error.VolumeFailed;
        self.mutex.lockUncancelable(self.io);
        defer self.mutex.unlock(self.io);
        if (self.held.items.len + self.completed.items.len >= self.window_cells) return error.ConveyorFull;
        var h: Held = .{ .w = w, .issued_ns = clock.monotonicNs() };
        if (self.fail_next > 0) {
            self.fail_next -= 1;
            h.fail = true;
        }
        try self.held.append(self.gpa, h);
        self.publishHead();
        if (!self.hold) {
            self.landAll();
            self.pump();
        }
    }

    fn publishHead(self: *RamLane) void {
        self.head_issued_ns.store(if (self.held.items.len == 0) std.math.maxInt(u64) else self.held.items[0].issued_ns, .release);
    }

    /// The threaded contracts' device: land (and pump) the head window
    /// once `device_ns` has passed since its handoff. True when a window
    /// landed; the poll is lock-free.
    pub fn landDue(self: *RamLane, device_ns: u64) bool {
        const head = self.head_issued_ns.load(.acquire);
        if (head == std.math.maxInt(u64) or clock.monotonicNs() < head +| device_ns) {
            // A completed group awaiting its prefix (§AW) is pumped by the
            // device thread too: its missing predecessor may have been
            // completed as abandoned, which submits nothing.
            if (self.completed_pending.load(.acquire) == 0) return false;
            self.mutex.lockUncancelable(self.io);
            defer self.mutex.unlock(self.io);
            self.pump();
            return false;
        }
        self.mutex.lockUncancelable(self.io);
        defer self.mutex.unlock(self.io);
        if (self.held.items.len == 0) return false;
        self.held.items[0].landed = true;
        self.pump();
        self.publishHead();
        return true;
    }

    /// Whether nothing is held or awaiting its prefix (lock-free; the
    /// device thread's exit test).
    pub fn drained(self: *const RamLane) bool {
        return self.head_issued_ns.load(.acquire) == std.math.maxInt(u64) and self.completed_pending.load(.acquire) == 0;
    }

    fn barrierFn(ctx: *anyopaque) Error!void {
        const self: *RamLane = @ptrCast(@alignCast(ctx));
        self.image.fdatasync();
        self.barriers += 1;
        self.needs_flush = false;
        return;
    }

    /// Mark the i-th held window landed (its bytes reach the image).
    pub fn land(self: *RamLane, i: usize) void {
        self.held.items[i].landed = true;
    }

    pub fn failWindow(self: *RamLane, i: usize) void {
        self.held.items[i].fail = true;
        self.held.items[i].landed = true;
    }

    pub fn landAll(self: *RamLane) void {
        for (self.held.items) |*h| h.landed = true;
    }

    pub fn heldCount(self: *const RamLane) usize {
        return self.held.items.len;
    }

    /// The coalesced barrier the cadence would run.
    pub fn flushCoalesced(self: *RamLane) void {
        if (!self.needs_flush) return;
        self.image.fdatasync();
        self.barriers += 1;
        self.needs_flush = false;
    }

    /// Stage B for the head group: the head once landed plus every landed
    /// successor; reservations completed in journal order (failed ones too);
    /// ONE prefix wait; verdicts (rollback hook, latch, hole checkpoint);
    /// the apply-hole checkpoint before any survivor's ack; ONE barrier
    /// (strict) else `needs_flush`; acks in journal order. The prefix wait
    /// is a CHECK here, never a park under the lane's lock: a window
    /// reserved earlier may still be in its applier's hands (§AW) and
    /// reach `submit` — which needs this lock — after a later one landed;
    /// the landed group waits in `completed` and the next pump (that
    /// submit's, a landing's, the test's) runs its verdicts.
    pub fn pump(self: *RamLane) void {
        while (self.held.items.len != 0 and self.held.items[0].landed) {
            var n: usize = 0;
            while (n < self.held.items.len and self.held.items[n].landed) : (n += 1) {}
            const landed = self.gpa.dupe(Held, self.held.items[0..n]) catch return;
            defer self.gpa.free(landed);
            self.held.replaceRange(self.gpa, 0, n, &.{}) catch unreachable;
            for (landed) |h| {
                if (!h.fail) {
                    self.image.writeOps(h.w.ops()) catch {};
                }
                self.reg.complete(self.core, h.w.res);
            }
            self.completed.appendSlice(self.gpa, landed) catch @panic("RamLane (a test double): out of memory");
            self.completed_pending.store(self.completed.items.len, .release);
        }
        if (self.completed.items.len == 0) return;
        var prefix: u64 = 0;
        for (self.completed.items) |h| prefix = @max(prefix, h.w.res.paddedEnd());
        if (self.reg.completedUpto() < prefix) return;
        {
            self.durability_passes += 1;
            const group = self.completed.toOwnedSlice(self.gpa) catch @panic("RamLane (a test double): out of memory");
            defer self.gpa.free(group);
            self.completed_pending.store(0, .release);
            // Journal order for the verdicts and the acks (the submits
            // may have arrived out of it).
            core_sort.stable(Held, group, {}, heldStartLess);
            const cv = self.conveyor.?;
            var hole_end: ?u64 = null;
            var any_ok = false;
            const verdicts = self.gpa.alloc(Error!void, group.len) catch return;
            defer self.gpa.free(verdicts);
            for (group, verdicts) |h, *v| {
                if (h.fail) {
                    if (self.oom_on_rollback) |fa| fa.fail_index = fa.alloc_index;
                    cv.rollbackHook(h.w.span(), h.w.res);
                    if (self.oom_on_rollback) |fa| fa.fail_index = std.math.maxInt(usize);
                    _ = self.latch.noteFailure(.journal_write);
                    if (!self.checkpointPast(h.w.res.paddedEnd())) _ = self.latch.noteFailure(.journal_write);
                    v.* = error.JournalWriteFailed;
                } else {
                    self.latch.noteSuccess(.journal_write);
                    self.ring_stats.noteWritten(h.w.acct(), h.w.entries);
                    if (h.w.applyHoles()) hole_end = @max(hole_end orelse 0, h.w.res.paddedEnd());
                    any_ok = true;
                    v.* = {};
                }
            }
            var hole_err: ?Error = null;
            if (hole_end) |end| {
                if (!self.checkpointPast(end)) hole_err = error.HoleCheckpointFailed;
            }
            if (hole_err == null and any_ok) {
                if (self.strict) {
                    self.image.fdatasync();
                    self.barriers += 1;
                } else self.needs_flush = true;
            }
            for (group, verdicts) |h, v| {
                const result: Error!void = if (v) |_| (if (hole_err) |e| e else {}) else |e| e;
                self.acks += 1;
                self.ack_order.append(self.gpa, h.w.res.start) catch @panic("RamLane (a test double): out of memory");
                h.w.finish(result);
            }
        }
    }

    fn checkpointPast(self: *RamLane, pos: u64) bool {
        const f = self.checkpoint_past orelse return true;
        return f(self.checkpoint_ctx.?, pos);
    }

    fn heldStartLess(_: void, a: Held, b: Held) bool {
        return a.w.res.start < b.w.res.start;
    }
};

// ---------------------------------------------------------------------------
// Tests — the row-12 contracts over `smo.TestVolume` + `RamLane`.
// ---------------------------------------------------------------------------

const testing = std.testing;
const economy = @import("economy.zig");

test "batch caps tie: txs max(64, cpus × 2), bytes max(256 KiB, ring/16) clamped to the admissible capacity; the knobs win verbatim" {
    try testing.expectEqual(@as(usize, 64), deriveBatchTxs(1));
    try testing.expectEqual(@as(usize, 64), deriveBatchTxs(32));
    try testing.expectEqual(@as(usize, 128), deriveBatchTxs(64));
    try testing.expectEqual(@as(usize, 384), deriveBatchTxs(192));
    try testing.expectEqual(@as(u64, 256 * derive.KiB), deriveBatchBytes(1));
    try testing.expectEqual(@as(u64, 256 * derive.KiB), deriveBatchBytes(4 * derive.MiB));
    try testing.expectEqual(@as(u64, 2 * derive.MiB), deriveBatchBytes(32 * derive.MiB));
    // The 8 MiB floor ring on a 4 KiB grain: logical 2048 × 4072 = 8,339,456;
    // reserve 256 KiB; max pad 21 + 4096.
    const geo = journal.productionGeometry(2048, 4096);
    const cap = ringUserCapacity(geo);
    try testing.expectEqual(geo.logicalLen() - geo.reserve_bytes - geo.maxPad(), cap);
    try testing.expectEqual(@max(256 * derive.KiB, cap / 16), resolveBatchBytes(null, cap));
    try testing.expectEqual(cap, resolveBatchBytes(cap + 1, cap));
    try testing.expectEqual(@as(u64, 4096), resolveBatchBytes(4096, cap));
    try testing.expectEqual(@as(usize, 1), resolveBatchTxs(1, 192));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const d = resolveCaps(&env, 8, geo);
    try testing.expectEqual(@as(usize, 64), d.max_txs);
    try testing.expectEqual(resolveBatchBytes(null, cap), d.max_bytes);
    try env.put("SQUEEZEFS_META_COMMIT_BATCH_TXS", "3");
    try env.put("SQUEEZEFS_META_COMMIT_BATCH_BYTES", "8192");
    const e = resolveCaps(&env, 8, geo);
    try testing.expectEqual(@as(usize, 3), e.max_txs);
    try testing.expectEqual(@as(u64, 8192), e.max_bytes);
    try testing.expectEqual(@as(u64, 30 * std.time.ns_per_s), PARK_ESCALATION_NS);
    try testing.expectEqual(@as(usize, 64), queueCapacity(10));
    try testing.expectEqual(@as(usize, 2048), queueCapacity(2048));
    try testing.expectEqual(@as(usize, 8192), queueCapacity(1 << 20));
}

test "pad hold tie: the §L pad hold holds from a 4 KiB grain on (a pad-majority window wastes a page), not on a 512-B one; the knob's arms override it" {
    try testing.expect(!padHoldFor(512));
    try testing.expect(!padHoldFor(1));
    try testing.expect(padHoldFor(4096));
    try testing.expect(padHoldFor(16384));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expect(!windowHoldFromEnv(&env, 50, 512).?.pad_hold);
    try testing.expect(windowHoldFromEnv(&env, 50, 4096).?.pad_hold);
    try testing.expectEqual(@as(?WindowHold, null), windowHoldFromEnv(&env, 0, 512));
    try env.put("SQUEEZEFS_META_WINDOW_HOLD", "both");
    try testing.expect(windowHoldFromEnv(&env, 50, 512).?.pad_hold);
    try env.put("SQUEEZEFS_META_WINDOW_HOLD", "pacing");
    try testing.expect(!windowHoldFromEnv(&env, 50, 4096).?.pad_hold);
    try env.put("SQUEEZEFS_META_WINDOW_HOLD", "none");
    try testing.expectEqual(@as(?WindowHold, null), windowHoldFromEnv(&env, 50, 4096));
}

test "applier venue width tie: clamp(cpus / 8, 2, 4) — 2 up to 23 cores, 3 at 24, 4 from 32 on" {
    try testing.expectEqual(@as(u32, 2), applierParallelism(1));
    try testing.expectEqual(@as(u32, 2), applierParallelism(8));
    try testing.expectEqual(@as(u32, 2), applierParallelism(23));
    try testing.expectEqual(@as(u32, 3), applierParallelism(24));
    try testing.expectEqual(@as(u32, 4), applierParallelism(32));
    try testing.expectEqual(@as(u32, 4), applierParallelism(256));
}

test "the error table is the ONE table: every member maps onto a core member with its errno and class" {
    inline for (@typeInfo(Error).error_set.error_names.?) |name| {
        const err: Error = @field(Error, name);
        _ = errnoOf(err);
        _ = classOf(err);
    }
    try testing.expectEqual(std.os.linux.E.NOSPC, errnoOf(error.NoSpace));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.NoSpace));
    // A joiner's exhausted grant is the manager dependency: retryable.
    try testing.expectEqual(std.os.linux.E.AGAIN, errnoOf(error.GrantExhausted));
    try testing.expectEqual(ErrorClass.retryable, classOf(error.GrantExhausted));
    try testing.expectEqual(std.os.linux.E.IO, errnoOf(error.VolumeFailed));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.VolumeFailed));
    try testing.expectEqual(ErrorClass.retryable, classOf(error.ConveyorFull));
    try testing.expectEqual(std.os.linux.E.AGAIN, errnoOf(error.ConveyorFull));
    try testing.expectEqual(std.os.linux.E.@"2BIG", errnoOf(error.ValueTooLarge));
    try testing.expectEqual(std.os.linux.E.INVAL, errnoOf(error.ForestKeyRefused));
    try testing.expectEqual(std.os.linux.E.ROFS, errnoOf(error.PartitionRefused));
    try testing.expectEqual(std.os.linux.E.ROFS, errnoOf(error.VolumeClosed));
    try testing.expectEqual(std.os.linux.E.IO, errnoOf(error.JournalWriteFailed));
}

test "group size histogram buckets: 1–8 exact, ≤16, ≤32, ≤64, >64; Σ buckets ≡ records" {
    var h: GroupSizeHistogram = .{};
    for ([_]usize{ 1, 1, 2, 8, 9, 16, 17, 32, 33, 64, 65, 1000 }) |n| h.record(n);
    try testing.expectEqual(@as(u64, 2), h.load(0));
    try testing.expectEqual(@as(u64, 1), h.load(1));
    try testing.expectEqual(@as(u64, 1), h.load(7));
    try testing.expectEqual(@as(u64, 2), h.load(8));
    try testing.expectEqual(@as(u64, 2), h.load(9));
    try testing.expectEqual(@as(u64, 2), h.load(10));
    try testing.expectEqual(@as(u64, 2), h.load(11));
    try testing.expectEqual(@as(u64, 12), h.total());
}

/// The contracts' volume: `smo.TestVolume` (journaled) + a volatile ring
/// image + `RamLane` + the checkpoint hooks over the harness's cycle.
const Harness = struct {
    gpa: Allocator,
    vol: smo.TestVolume,
    image: journal.VolatileImage,
    latch: journal.FailureLatch = .{},
    lane: RamLane,
    space: sync.Notify = .init,
    posture: heap_law.HeapFullPosture = .{},
    space_retry: heap_law.SpaceRetryHold = .{},
    cv: *Conveyor,
    name_key: key.NameKey = .init(0x5EED),
    inos: inos_mod.InoWatermark,
    park_kicks: u64 = 0,
    threshold_kicks: u64 = 0,
    checkpoints: u64 = 0,
    last_ledger: ?smo.TestVolume.Ledger = null,
    /// A kick that runs the cycle inline (the park resolves without a timer).
    park_runs_cycle: bool = true,
    fail_checkpoint_past: bool = false,
    /// The threaded venue (the §L contract): the election winner's wake
    /// is a `notifyOne` on `pass_wake` and a dedicated thread runs the
    /// pass — the production shape (a wake, a fiber) over OS threads.
    threaded_pass: bool = false,
    pass_wake: sync.Notify = .init,
    pass_stop: std.atomic.Value(bool) = .init(false),

    const inos_mod = @import("inos.zig");

    fn create(cfg: smo.TestVolume.Config, caps: ?Caps, strict: bool) !*Harness {
        return createWithGpa(cfg, caps, strict, testing.allocator);
    }

    /// `create` with the CONVEYOR's allocator chosen (a `FailingAllocator`
    /// for the OOM contracts); the fixture itself stays on `testing.allocator`.
    fn createWithGpa(cfg: smo.TestVolume.Config, caps: ?Caps, strict: bool, cv_gpa: Allocator) !*Harness {
        return createWithAllocators(cfg, caps, strict, testing.allocator, cv_gpa);
    }

    /// Both allocators chosen: the §L contract runs the whole fixture on a
    /// leak-checking allocator WITHOUT stack-trace capture (the traces cost
    /// ~100 µs per record in Debug and dwarfed the device round trip the
    /// test measures against).
    fn createWithAllocators(cfg: smo.TestVolume.Config, caps: ?Caps, strict: bool, gpa: Allocator, cv_gpa: Allocator) !*Harness {
        const h = try gpa.create(Harness);
        errdefer gpa.destroy(h);
        var c = cfg;
        c.journaled = true;
        h.* = .{ .gpa = gpa, .vol = undefined, .image = undefined, .lane = undefined, .cv = undefined, .inos = inos_mod.InoWatermark.init(2) };
        h.vol = try smo.TestVolume.init(gpa, c);
        errdefer h.vol.deinit();
        _ = try h.vol.createTree(.control);
        _ = try h.vol.createTree(.native);
        h.image = journal.VolatileImage.init(gpa, h.vol.ring_image, 0);
        h.lane = RamLane.init(gpa, testing.io, h.vol.core, h.vol.reg, &h.latch, &h.vol.journal.stats, &h.image, strict);
        h.lane.checkpoint_past = checkpointPastFn;
        h.lane.checkpoint_ctx = h;
        h.vol.journal.device = .{ .ctx = h, .write = smoWrite, .completed = smoCompleted };
        const geo = h.vol.journal.geo;
        h.cv = try Conveyor.init(cv_gpa, .{
            .io = testing.io,
            .gpa = cv_gpa,
            .volume = "ram-vol",
            .trees = h.vol.table,
            .cache = h.vol.cache,
            .alloc = h.vol.alloc,
            .core = h.vol.core,
            .reg = h.vol.reg,
            .ring_layout = h.vol.journal.layout,
            .geo = geo,
            .appender_id = h.vol.journal.appender_id,
            .latch = &h.latch,
            .lane = h.lane.seam(),
            .pass = .{ .ctx = h, .wake = passInline },
            .checkpoint = .{ .ctx = h, .kickPark = kickPark, .kickThreshold = kickThreshold, .checkpointNow = checkpointNowFn, .checkpointPast = checkpointPastFn, .space = &h.space, .posture = &h.posture, .space_retry = &h.space_retry },
            .smo_journal = h.vol.journal,
            .caps = caps orelse .{ .max_txs = deriveBatchTxs(8), .max_bytes = resolveBatchBytes(null, ringUserCapacity(geo)) },
            .park_escalation_ns = PARK_ESCALATION_NS,
        });
        h.lane.conveyor = h.cv;
        return h;
    }

    fn destroy(h: *Harness) void {
        h.cv.quiesce();
        h.cv.deinit();
        h.lane.deinit();
        h.image.deinit();
        h.vol.deinit();
        h.gpa.destroy(h);
    }

    // The SMO journal's device path on the RAM volume: the ops land on the
    // same volatile image the lane writes (durable at the next barrier).
    fn smoWrite(ctx: *anyopaque, ops: []const journal.WriteOp) error{Io}!void {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        h.image.writeOps(ops) catch return error.Io;
    }

    fn smoCompleted(_: *anyopaque) void {}

    /// The contracts' pass venue: the election winner's own thread, inline
    /// (deterministic: a commit returns with its pass run) — or, threaded,
    /// one wake of the pass thread (the permit absorbs a wake that lands
    /// before the thread registers).
    fn passInline(ctx: *anyopaque) void {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        if (h.threaded_pass) {
            h.pass_wake.notifyOne(testing.io);
            return;
        }
        h.cv.runScheduledPass();
    }

    /// The pass thread: register → run the scheduled pass → re-read the
    /// leader latch (model 7: a wake absorbed while running) → park.
    fn passThreadMain(h: *Harness) void {
        const io = testing.io;
        while (true) {
            var n = h.pass_wake.notified();
            _ = n.enable(io);
            if (h.pass_stop.load(.acquire)) {
                n.cancel(io);
                return;
            }
            h.cv.runScheduledPass();
            if (h.cv.leaderHeld()) {
                n.cancel(io);
                continue;
            }
            n.waitUncancelable(io);
        }
    }

    /// The threaded contracts' device: lands the head window `device_ns`
    /// after its handoff and completes the committers' simulated data
    /// DMAs at their due instants, until stopped and drained. The ONE
    /// thread that watches the clock: committers park on their DMA's
    /// oneshot, never spin.
    fn deviceThreadMain(h: *Harness, device_ns: u64, dma: *DmaSim, stop: *std.atomic.Value(bool)) void {
        var idle: u32 = 0;
        while (true) {
            const landed = h.lane.landDue(device_ns);
            const completed = dma.completeDue();
            if (landed or completed) {
                idle = 0;
                continue;
            }
            if (stop.load(.acquire) and h.lane.drained()) return;
            // The device keeps its own clock: a spin, with a yield every
            // so often so a loaded box still schedules the others (a yield
            // per poll let the scheduler's quantum batch the completions).
            idle +%= 1;
            if (idle % 4096 == 0) std.Thread.yield() catch {};
            std.atomic.spinLoopHint();
        }
    }

    /// A simulated data device for the §L contract: an op's DMA is a
    /// timed completion the device thread delivers (the W1 patch's own
    /// device round trip before its commit). The earliest due instant is
    /// polled lock-free.
    const DmaSim = struct {
        const Pending = struct { due_ns: u64, done: *sync.Oneshot(void) };
        mutex: std.Io.Mutex = .init,
        pending: std.ArrayList(Pending) = .empty,
        earliest_due_ns: std.atomic.Value(u64) = .init(std.math.maxInt(u64)),

        fn submit(self: *DmaSim, due_ns: u64, done: *sync.Oneshot(void)) !void {
            self.mutex.lockUncancelable(testing.io);
            defer self.mutex.unlock(testing.io);
            try self.pending.append(testing.allocator, .{ .due_ns = due_ns, .done = done });
            _ = self.earliest_due_ns.fetchMin(due_ns, .release);
        }

        fn completeDue(self: *DmaSim) bool {
            if (clock.monotonicNs() < self.earliest_due_ns.load(.acquire)) return false;
            self.mutex.lockUncancelable(testing.io);
            defer self.mutex.unlock(testing.io);
            const now = clock.monotonicNs();
            var any = false;
            var i: usize = 0;
            var earliest: u64 = std.math.maxInt(u64);
            while (i < self.pending.items.len) {
                if (self.pending.items[i].due_ns <= now) {
                    const p = self.pending.swapRemove(i);
                    p.done.send(testing.io, {}) catch {};
                    any = true;
                } else {
                    earliest = @min(earliest, self.pending.items[i].due_ns);
                    i += 1;
                }
            }
            self.earliest_due_ns.store(earliest, .release);
            return any;
        }

        fn deinit(self: *DmaSim) void {
            std.debug.assert(self.pending.items.len == 0);
            self.pending.deinit(testing.allocator);
        }
    };

    fn kickPark(ctx: *anyopaque) void {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        h.park_kicks += 1;
        if (h.park_runs_cycle) {
            if (h.checkpoint()) |_| {} else |_| {}
            h.space.notifyWaiters(testing.io);
        }
    }

    fn kickThreshold(ctx: *anyopaque) void {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        h.threshold_kicks += 1;
    }

    /// The harness's cycle with the hole/park semantics: flush + tail +
    /// cover, the ledger kept.
    fn checkpoint(h: *Harness) !smo.TestVolume.Ledger {
        h.checkpoints += 1;
        h.image.fdatasync();
        const led = try h.vol.checkpoint();
        h.image.fdatasync();
        h.last_ledger = led;
        return led;
    }

    fn checkpointNowFn(ctx: *anyopaque) bool {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        _ = h.checkpoint() catch return false;
        return true;
    }

    /// Cycle until at least half the user slice is reusable again (a dying
    /// floor from a flush-pass SMO clamps one cycle's tail: reclamation
    /// lags a cycle, as it does on the device).
    fn coverRing(h: *Harness, user_cap: u64) !void {
        for (0..8) |_| {
            if (h.vol.core.headPos() - h.vol.core.reusableUpto() <= user_cap / 2) return;
            _ = try h.checkpoint();
        }
        return error.RingNotCovered;
    }

    fn checkpointPastFn(ctx: *anyopaque, pos: u64) bool {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        if (h.fail_checkpoint_past) return false;
        for (0..8) |_| {
            const led = h.checkpoint() catch return false;
            if (led.tail >= pos) return true;
        }
        return false;
    }

    fn treeOf(h: *Harness, identity: tree.TreeIdentity) *KvTree {
        for (h.vol.trees.items) |t| if (t.identity.eql(identity)) return t;
        unreachable;
    }

    /// One staged create under the root: inode Put, dentry Put, parent Δ.
    fn stageCreate(h: *Harness, t: *tx.KvTx, name: []const u8) !u64 {
        const ino = h.inos.allocate();
        const parent_value: record.InodeValue = .{ .mode = economy.S.IFDIR | 0o755, .nlink = 2 };
        _ = try economy.stageCreate(t, .{ .name_key = &h.name_key, .now = 1000, .parent = 1, .parent_value = parent_value, .name = name, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = ino, .global_ino = ino });
        return ino;
    }

    fn commitCreate(h: *Harness, name: []const u8) !u64 {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        const ino = try h.stageCreate(t, name);
        try h.cv.commit(t);
        return ino;
    }

    fn present(h: *Harness, ino: u64) !bool {
        const got = try h.treeOf(.native).lookup(h.gpa, &key.inodeKey(ino));
        defer if (got) |g| h.gpa.free(g);
        return got != null;
    }

    /// A crash-equivalent reopen from the last ledger over the image as the
    /// device holds it (the caller power-cut it first if it wanted the
    /// volatile writes gone).
    fn reopen(h: *Harness) !smo.TestVolume.Reopened {
        return h.vol.crashReplay(h.last_ledger.?);
    }

    fn replayedPresent(r: *smo.TestVolume.Reopened, ino: u64) !bool {
        const got = try r.treeOf(.native).lookup(testing.allocator, &key.inodeKey(ino));
        defer if (got) |g| testing.allocator.free(g);
        return got != null;
    }
};

test "a single commit: the pass drains one, the lane lands it and acks after the entry landed; guards drop before the ack; the closure identities hold at quiesce" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const Guards = struct {
        released: u32 = 0,
        fn release(ctx: *anyopaque) void {
            const g: *@This() = @ptrCast(@alignCast(ctx));
            g.released += 1;
        }
    };
    var g: Guards = .{};
    const t = try h.gpa.create(tx.KvTx);
    defer h.gpa.destroy(t);
    t.* = tx.KvTx.init(h.gpa, @src());
    t.holdGuards(tx.GuardSet.init(&g, Guards.release));
    const ino = try h.stageCreate(t, "one");
    try h.cv.commit(t);
    try testing.expectEqual(@as(u32, 1), g.released);
    try testing.expect(try h.present(ino));
    try testing.expectEqual(@as(u64, 1), h.cv.stats.leader_passes.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.group_size.total());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.lane_handoffs.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.acks.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.windows_inflight.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.queued.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.commit_parked.load());
    try testing.expectEqual(@as(usize, 0), h.cv.pending());
    try testing.expect(!h.cv.core.leader.load(.seq_cst));
    // The contracts' venue: the pass ran inline on the committer's thread.
    try testing.expectEqual(@as(u64, @intCast(std.Thread.getCurrentId())), h.cv.passThread());
    try testing.expectEqual(@as(usize, 0), h.vol.reg.openCount());
    try testing.expectEqual(h.vol.core.headPos(), h.vol.reg.completedUpto());
    // The entry replays whole from the image.
    var rec = try journal.replayScanImage(h.gpa, h.vol.ring_image, h.vol.journal.geo, 0, 0);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 1), rec.entries.len);
    var it = rec.entries[0].records();
    var n: usize = 0;
    while (it.next()) |_| n += 1;
    try testing.expectEqual(@as(usize, 3), n);
}

test "the kernel view (decision 0003 §AB.2): a change this mount's kernel did not ask for — a peer's served step, a job, a tool — is invalidated in it by name and by object after the RAM apply and BEFORE its ack; the kernel's own change is its reply's to tell (nothing invalidated); a dentry change no note names drops the whole dentry cache" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const View = struct {
        calls: u32 = 0,
        names: u32 = 0,
        saw_name: bool = false,
        unnamed: u32 = 0,
        objects: u32 = 0,
        /// The ack count at the call: the invalidation precedes the ack.
        acks_at_call: u64 = 0,
        /// The child's record readable at the call: after the RAM apply.
        applied_at_call: bool = false,
        child: u64 = 0,
        h: *Harness,
        fn invalidate(ctx: *anyopaque, names: []const tx.NameNote, unnamed: bool, objects: []const u64) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.calls += 1;
            self.names += @intCast(names.len);
            for (names) |n| if (n.parent == 1 and std.mem.eql(u8, n.name, "peer-made")) {
                self.saw_name = true;
            };
            if (unnamed) self.unnamed += 1;
            self.objects += @intCast(objects.len);
            self.acks_at_call = self.h.cv.stats.acks.load();
            if (self.child != 0) self.applied_at_call = self.h.present(self.child) catch false;
        }
    };
    var v: View = .{ .h = h };
    const kv: KernelView = .{ .ctx = &v, .invalidate = &View.invalidate };
    h.cv.armKernel(&kv);
    defer h.cv.disarmKernel();
    // The kernel's own create: nothing to invalidate.
    {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        _ = try h.stageCreate(t, "kernel-made");
        t.kernel_origin = true;
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u32, 0), v.calls);
    // A peer's create (no kernel origin): ONE call naming it, its inode and
    // the parent's, before the ack, after the apply.
    const acks0 = h.cv.stats.acks.load();
    {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        v.child = try h.stageCreate(t, "peer-made");
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u32, 1), v.calls);
    try testing.expect(v.saw_name);
    try testing.expectEqual(@as(u32, 1), v.names);
    try testing.expectEqual(@as(u32, 0), v.unnamed);
    try testing.expectEqual(@as(u32, 2), v.objects);
    try testing.expectEqual(acks0, v.acks_at_call);
    try testing.expect(v.applied_at_call);
    // A dentry change no note names: the whole dentry cache.
    v.child = 0;
    {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        try t.stagePut(key.TREE_DENTRIES, &key.dentryKey(1, 0x77, 0), "x");
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u32, 2), v.calls);
    try testing.expectEqual(@as(u32, 1), v.unnamed);
}

test "the kernel view: a change its own channel tells this mount's kernel (`kernel_told` — a ring write's commit, whose size the IPC host announced before the ack) is not invalidated back: the duplicate, landing after the ack inside a kernel GETATTR, made the kernel drop the fresh size (the IPC host's aio row read 0 of 32 KiB) — the same change from a tool is still invalidated" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const View = struct {
        calls: u32 = 0,
        fn invalidate(ctx: *anyopaque, _: []const tx.NameNote, _: bool, _: []const u64) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.calls += 1;
        }
    };
    var v: View = .{};
    const kv: KernelView = .{ .ctx = &v, .invalidate = &View.invalidate };
    h.cv.armKernel(&kv);
    defer h.cv.disarmKernel();
    {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        _ = try h.stageCreate(t, "ring-told");
        t.kernel_told = true;
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u32, 0), v.calls);
    {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        _ = try h.stageCreate(t, "tool-made");
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u32, 1), v.calls);
}

test "the ack spin's bound (decision 0003 §AB.4): no measured wake ⇒ no spin; 2·W when the ack's mean is within it; an ack predicted past 2·W ⇒ no spin (every spin would fall through)" {
    try testing.expectEqual(@as(u64, 0), spinBound(0, 10_000));
    try testing.expectEqual(@as(u64, 20_000), spinBound(10_000, 15_000));
    try testing.expectEqual(@as(u64, 20_000), spinBound(10_000, 20_000));
    try testing.expectEqual(@as(u64, 0), spinBound(10_000, 20_001));
    var a: AckSpin = .{};
    try testing.expectEqual(@as(u64, 0), a.read().bound_ns);
    a.ack.sample(30_000);
    try testing.expectEqual(@as(u64, 0), a.read().bound_ns);
    a.wake.sample(17_500);
    try testing.expectEqual(@as(u64, 35_000), a.read().bound_ns);
}

test "the turn spin's tie (§BC): the cap before a sample; 2× the turn's mean within the cap; 0 past it" {
    try testing.expectEqual(TURN_SPIN_CAP_NS, turnSpinBound(null));
    try testing.expectEqual(@as(u64, 20_000), turnSpinBound(10_000));
    try testing.expectEqual(TURN_SPIN_CAP_NS, turnSpinBound(20_000));
    try testing.expectEqual(TURN_SPIN_CAP_NS, turnSpinBound(TURN_SPIN_CAP_NS));
    try testing.expectEqual(@as(u64, 0), turnSpinBound(TURN_SPIN_CAP_NS + 1));
}

test "the ack spin: an ack landing inside the bound is a hit (the outcome on its way: the recv takes it); none inside is a fall-through after the whole bound" {
    var done: Done = .init;
    const Sender = struct {
        fn run(d: *Done) void {
            const t0 = clock.monotonicNs();
            while (clock.monotonicNs() - t0 < 2 * std.time.ns_per_ms) std.atomic.spinLoopHint();
            d.send(testing.io, {}) catch {};
        }
    };
    const t = try std.Thread.spawn(.{}, Sender.run, .{&done});
    try testing.expect(spinForAck(&done, 5 * std.time.ns_per_s));
    t.join();
    try (try done.recvOutcome(testing.io));
    var never: Done = .init;
    const t0 = clock.monotonicNs();
    try testing.expect(!spinForAck(&never, 200 * std.time.ns_per_us));
    try testing.expect(clock.monotonicNs() - t0 >= 200 * std.time.ns_per_us);
}

test "Done.send writes nothing once the receiver can take the outcome: a committer's Done lives in its frame, gone the instant its recv returns (a storm thread exited, its stack unmapped — the lane's late ack stamp segfaulted at a stack address, 1 in 100 runs of the journal-lane storm under 8-way load)" {
    const gpa = testing.allocator;
    const done = try gpa.create(Done);
    defer gpa.destroy(done);
    done.* = .init;
    test_seams.send_held.store(0, .release);
    test_seams.hold_after_send.store(1, .release);
    defer test_seams.hold_after_send.store(0, .release);
    const Sender = struct {
        fn run(d: *Done) void {
            d.send(testing.io, {}) catch {};
        }
    };
    const t = try std.Thread.spawn(.{}, Sender.run, .{done});
    try (try done.recvOutcome(testing.io));
    while (test_seams.send_held.load(.acquire) == 0) std.Thread.yield() catch {};
    // The receiver's frame moves on: every byte of it is someone else's now.
    @memset(std.mem.asBytes(done), 0xA5);
    test_seams.hold_after_send.store(0, .release);
    t.join();
    for (std.mem.asBytes(done)) |b| try testing.expectEqual(@as(u8, 0xA5), b);
}

test "the ack spin's ledger: every park is a hit, a fall-through, a skip (no bound) or busy (work on the lane, or off a lane: never a spin); every answer feeds the ack estimator once" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    var i: usize = 0;
    while (i < 5) : (i += 1) {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        var nb: [8]u8 = undefined;
        _ = try h.stageCreate(t, try std.fmt.bufPrint(&nb, "s{d}", .{i}));
        try h.cv.commit(t);
    }
    const st = &h.cv.stats;
    try testing.expectEqual(@as(u64, 5), st.ack_spin_hits.load() + st.ack_spin_falls.load() + st.ack_spin_skips.load() + st.ack_spin_busy.load());
    // Off a lane every park is `busy`: never a spin, never a wake sample.
    try testing.expectEqual(@as(u64, 5), st.ack_spin_busy.load());
    try testing.expectEqual(@as(u64, 0), h.cv.ack_spin.wake.samples.load(.monotonic));
    try testing.expectEqual(st.acks.load(), h.cv.ack_spin.ack.samples.load(.monotonic));

    // An idle caller: the first park has no measured wake (a skip) and
    // teaches one; a hit returns without a wake sample; a park after a
    // skip or a fall-through samples the wake.
    const Idle = struct {
        var polls: u64 = 0;
        fn yes() bool {
            return true;
        }
        /// Each read one more: the inline pass's resume ran alone.
        fn tick() u64 {
            polls += 1;
            return polls;
        }
    };
    h.cv.deps.caller = .{ .idle = Idle.yes, .polls = Idle.tick };
    while (i < 10) : (i += 1) {
        const t = try h.gpa.create(tx.KvTx);
        defer h.gpa.destroy(t);
        t.* = tx.KvTx.init(h.gpa, @src());
        var nb: [8]u8 = undefined;
        _ = try h.stageCreate(t, try std.fmt.bufPrint(&nb, "s{d}", .{i}));
        try h.cv.commit(t);
    }
    try testing.expectEqual(@as(u64, 5), st.ack_spin_busy.load());
    try testing.expectEqual(@as(u64, 5), st.ack_spin_hits.load() + st.ack_spin_falls.load() + st.ack_spin_skips.load());
    try testing.expect(st.ack_spin_skips.load() >= 1);
    const wakes = h.cv.ack_spin.wake.samples.load(.monotonic);
    try testing.expect(wakes >= 1 and wakes <= st.ack_spin_skips.load() + st.ack_spin_falls.load());
    try testing.expectEqual(st.acks.load(), h.cv.ack_spin.ack.samples.load(.monotonic));
}

test "a group of 8 is ONE pass with 8 acks in journal order; every record present; the lane wrote one window; identities hold" {
    const h = try Harness.create(.{}, null, true);
    defer h.destroy();
    var txs: [8]*tx.KvTx = undefined;
    var inos: [8]u64 = undefined;
    var names: [8][8]u8 = undefined;
    for (&txs, &inos, &names, 0..) |*t, *ino, *nb, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        const name = try std.fmt.bufPrint(nb, "g{d}", .{i});
        ino.* = try h.stageCreate(t.*, name);
    }
    defer for (txs) |t| h.gpa.destroy(t);
    var results: [8]Outcome = undefined;
    h.cv.commitGroup(&txs, &results);
    for (results) |r| try r;
    for (inos) |ino| try testing.expect(try h.present(ino));
    try testing.expectEqual(@as(u64, 1), h.cv.stats.leader_passes.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.group_size.load(7));
    try testing.expectEqual(@as(u64, 1), h.cv.stats.group_commits.load());
    try testing.expectEqual(@as(u64, 8), h.cv.stats.group_txs.load());
    try testing.expectEqual(@as(u64, 8), h.cv.stats.acks.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.lane_handoffs.load());
    try testing.expectEqual(@as(u64, 1), h.lane.durability_passes);
    // Strict: ONE barrier for the group, before any ack.
    try testing.expectEqual(@as(u64, 1), h.lane.barriers);
    // Eight ordinary entries in the one reservation, replayed in order.
    var rec = try journal.replayScanImage(h.gpa, h.vol.ring_image, h.vol.journal.geo, 0, 0);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 8), rec.entries.len);
    for (rec.entries[1..], 1..) |e, i| try testing.expect(e.seq > rec.entries[i - 1].seq);
}

test "A1½: an armed token hook sees the batch's union (sorted, deduped, token_quiet skipped) ONCE before the lane's submit; the settle runs at the terminal outcome on the fan-out arm AND the rollback arm; unarmed, nothing is called" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const Hook = struct {
        recalls: u32 = 0,
        settles: u32 = 0,
        last_union: [8]u64 = @splat(0),
        last_len: usize = 0,
        /// The lane's handoff count at the recall: the recall precedes it.
        handoffs_at_recall: u64 = 0,
        cv: *Conveyor,
        fn recall(ctx: *anyopaque, objects: []const u64, _: BatchNames) Allocator.Error!void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.recalls += 1;
            self.handoffs_at_recall = self.cv.stats.lane_handoffs.load();
            self.last_len = objects.len;
            for (objects, 0..) |o, i| if (i < 8) {
                self.last_union[i] = o;
            };
        }
        fn settle(ctx: *anyopaque, objects: []const u64) void {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.settles += 1;
            std.debug.assert(objects.len == self.last_len);
        }
    };
    // Unarmed: the baseline commit calls nothing.
    var hook: Hook = .{ .cv = h.cv };
    _ = try h.commitCreate("unarmed");
    try testing.expectEqual(@as(u32, 0), hook.recalls);
    const th: TokenHook = .{ .ctx = &hook, .recall = &Hook.recall, .settle = &Hook.settle };
    h.cv.armTokens(&th);
    defer h.cv.disarmTokens();
    // A group of 3: two creates under the root (the parent 1 and each
    // child) and ONE token_quiet control write that recalls nothing.
    var txs: [3]*tx.KvTx = undefined;
    for (&txs, 0..) |*t, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        if (i < 2) {
            var nb: [8]u8 = undefined;
            _ = try h.stageCreate(t.*, try std.fmt.bufPrint(&nb, "c{d}", .{i}));
        } else {
            try t.*.stagePut(key.TREE_XATTRS, &key.xattrKey(1, 77, 0), "\x05claimX");
            t.*.token_quiet = true;
        }
    }
    defer for (txs) |t| h.gpa.destroy(t);
    const handoffs_before = h.cv.stats.lane_handoffs.load();
    var results: [3]Outcome = undefined;
    h.cv.commitGroup(&txs, &results);
    for (results) |r| try r;
    try testing.expectEqual(@as(u32, 1), hook.recalls);
    try testing.expectEqual(@as(u32, 1), hook.settles);
    try testing.expectEqual(handoffs_before, hook.handoffs_at_recall);
    try testing.expectEqual(handoffs_before + 1, h.cv.stats.lane_handoffs.load());
    // The union: the root (both creates' dentries + Δtimes) and the two
    // children, ascending, each once.
    try testing.expectEqual(@as(usize, 3), hook.last_len);
    try testing.expectEqual(@as(u64, 1), hook.last_union[0]);
    try testing.expect(hook.last_union[1] < hook.last_union[2]);
    // The rollback arm: a failed window settles its union too.
    h.lane.fail_next = 1;
    try testing.expectError(error.JournalWriteFailed, h.commitCreate("victim"));
    try testing.expectEqual(@as(u32, 2), hook.recalls);
    try testing.expectEqual(@as(u32, 2), hook.settles);
    try testing.expectEqual(@as(u64, 2), h.cv.stats.pass_token_recall.snapshot().count);
}

test "the tx cap bounds the group size and the byte cap splits an over-cap group; every member commits exactly once" {
    const h = try Harness.create(.{}, .{ .max_txs = 3, .max_bytes = std.math.maxInt(u64) }, false);
    defer h.destroy();
    var txs: [7]*tx.KvTx = undefined;
    var names: [7][8]u8 = undefined;
    for (&txs, &names, 0..) |*t, *nb, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        _ = try h.stageCreate(t.*, try std.fmt.bufPrint(nb, "c{d}", .{i}));
    }
    defer for (txs) |t| h.gpa.destroy(t);
    var results: [7]Outcome = undefined;
    h.cv.commitGroup(&txs, &results);
    for (results) |r| try r;
    // 7 members at cap 3: passes of 3, 3, 1.
    try testing.expectEqual(@as(u64, 3), h.cv.stats.leader_passes.load());
    try testing.expectEqual(@as(u64, 2), h.cv.stats.group_size.load(2));
    try testing.expectEqual(@as(u64, 1), h.cv.stats.group_size.load(0));
    try testing.expectEqual(h.cv.stats.leader_passes.load(), h.cv.stats.group_size.total());
    try testing.expectEqual(@as(u64, 7), h.cv.stats.acks.load());

    // The byte cap: one entry per batch when the cap is below two entries.
    h.cv.deps.caps = .{ .max_txs = 64, .max_bytes = 1 };
    var more: [4]*tx.KvTx = undefined;
    var mnames: [4][8]u8 = undefined;
    for (&more, &mnames, 0..) |*t, *nb, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        _ = try h.stageCreate(t.*, try std.fmt.bufPrint(nb, "b{d}", .{i}));
    }
    defer for (more) |t| h.gpa.destroy(t);
    var r2: [4]Outcome = undefined;
    h.cv.commitGroup(&more, &r2);
    for (r2) |r| try r;
    try testing.expectEqual(@as(u64, 7), h.cv.stats.leader_passes.load());
    try testing.expectEqual(@as(u64, 11), h.cv.stats.acks.load());
    try testing.expectEqual(h.cv.stats.lane_handoffs.load(), h.lane.durability_passes);
}

test "empty txs commit nothing; an over-cap value fails ITS member alone and its siblings commit; a closed conveyor refuses" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const empty = try h.gpa.create(tx.KvTx);
    defer h.gpa.destroy(empty);
    empty.* = tx.KvTx.init(h.gpa, @src());
    try h.cv.commit(empty);
    try testing.expectEqual(@as(u64, 0), h.cv.stats.leader_passes.load());

    const big = try h.gpa.alloc(u8, record.recordValueCap(h.vol.layout.node_size) + 1);
    defer h.gpa.free(big);
    @memset(big, 7);
    var txs: [3]*tx.KvTx = undefined;
    for (&txs) |*t| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
    }
    defer for (txs) |t| h.gpa.destroy(t);
    const a = try h.stageCreate(txs[0], "sib-a");
    try txs[1].stagePut(key.TREE_XATTRS, &key.xattrKey(9, 1, 0), big);
    const c = try h.stageCreate(txs[2], "sib-c");
    var results: [3]Outcome = undefined;
    h.cv.commitGroup(&txs, &results);
    try results[0];
    try testing.expectError(error.ValueTooLarge, results[1]);
    try results[2];
    try testing.expect(try h.present(a));
    try testing.expect(try h.present(c));
    try testing.expectEqual(@as(u64, 2), h.cv.stats.acks.load());
    try testing.expectEqual(@as(u64, 2), h.cv.stats.group_txs.load());

    h.cv.close();
    try testing.expectError(error.VolumeClosed, h.commitCreate("late"));
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
}

test "a poisoned member fails alone in-window: its records are rolled out, the batch's survivors commit, the hole is checkpointed past before the acks, replay never shows the victim" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    var txs: [3]*tx.KvTx = undefined;
    var inos: [3]u64 = undefined;
    var names: [3][8]u8 = undefined;
    for (&txs, &inos, &names, 0..) |*t, *ino, *nb, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        ino.* = try h.stageCreate(t.*, try std.fmt.bufPrint(nb, "p{d}", .{i}));
    }
    defer for (txs) |t| h.gpa.destroy(t);
    test_seams.poison_apply_ino.store(inos[1], .monotonic);
    defer test_seams.poison_apply_ino.store(0, .monotonic);
    const ckpts0 = h.checkpoints;
    var results: [3]Outcome = undefined;
    h.cv.commitGroup(&txs, &results);
    try results[0];
    try testing.expectError(error.Corrupt, results[1]);
    try results[2];
    try testing.expect(try h.present(inos[0]));
    try testing.expect(!try h.present(inos[1]));
    try testing.expect(try h.present(inos[2]));
    // The hole checkpoint ran before the survivors were acked and its tail
    // passed the window (on this unpadded ring the second survivor shares
    // the hole's page and is chain-unreachable — exactly why the ack waits
    // for the covering checkpoint).
    try testing.expect(h.checkpoints > ckpts0);
    try testing.expect(h.last_ledger.?.tail >= h.vol.core.headPos());
    // A reopen from that ledger: survivors present, the victim never.
    var r = try h.reopen();
    defer r.deinit();
    try testing.expect(try Harness.replayedPresent(&r, inos[0]));
    try testing.expect(!try Harness.replayedPresent(&r, inos[1]));
    try testing.expect(try Harness.replayedPresent(&r, inos[2]));
}

test "rollback: a failed window answers JournalWriteFailed, its records are rolled back seq-conditionally, a NEWER committed record on a shared key stands, the volume stays open; 3 failures latch FAILED and every later commit is refused" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    // A committed baseline on the parent (the shared key every create's Δ
    // touches).
    const base = try h.commitCreate("base");
    try testing.expect(try h.present(base));

    h.lane.fail_next = 1;
    try testing.expectError(error.JournalWriteFailed, h.commitCreate("victim"));
    try testing.expectEqual(@as(u64, 1), h.cv.stats.rollbacks.load());
    try testing.expect(!h.latch.isFailed());
    // The victim's inode is gone; the baseline stands.
    const victim_ino = h.inos.snapshot() - 1;
    try testing.expect(!try h.present(victim_ino));
    try testing.expect(try h.present(base));
    // A success resets the run.
    const after = try h.commitCreate("after");
    try testing.expect(try h.present(after));

    // The shared parent key: hold the failing window, commit a NEWER Δ on
    // the parent in a later window that succeeds, then fail the held one:
    // the rollback removes only the failed window's records; the newer
    // committed Δ stands (LWW-correct).
    h.lane.hold = true;
    const parent_key = key.inodeKey(1);
    // Two sequential commits held in the lane: window A (to fail), window B (to land).
    var ta = tx.KvTx.init(h.gpa, @src());
    const ino_a = try h.stageCreate(&ta, "held-a");
    var tb = tx.KvTx.init(h.gpa, @src());
    const ino_b = try h.stageCreate(&tb, "held-b");
    // Enqueue both under one lock so one pass drains both into ONE window;
    // we want two windows, so commit them through two passes: the first
    // pass runs inline and hands off window A; the second likewise.
    var da: Done = .init;
    var ea = try h.cv.build(&ta, &da);
    try h.cv.enqueueOne(&ea, ea.q.len);
    h.cv.leadIfElected();
    var db: Done = .init;
    var eb = try h.cv.build(&tb, &db);
    try h.cv.enqueueOne(&eb, eb.q.len);
    h.cv.leadIfElected();
    try testing.expectEqual(@as(usize, 2), h.lane.heldCount());
    try testing.expectEqual(@as(u64, 2), h.cv.windowsInflight());
    // Both applied to RAM; the parent's newest record is B's Δ.
    const newest_before = blk: {
        const leaf = try h.treeOf(.native).resolveLeaf(&parent_key);
        defer leaf.release();
        const snap = leaf.snapshotLoad();
        defer snap.release();
        break :blk snap.value.newestSeqOf(&parent_key).?;
    };
    h.lane.failWindow(0);
    h.lane.land(1);
    h.lane.pump();
    try testing.expectError(error.JournalWriteFailed, try da.recvOutcome(testing.io));
    try try db.recvOutcome(testing.io);
    try testing.expect(!try h.present(ino_a));
    try testing.expect(try h.present(ino_b));
    // The parent's newest record is still B's (A's Δ was removed, B's stands).
    {
        const leaf = try h.treeOf(.native).resolveLeaf(&parent_key);
        defer leaf.release();
        const snap = leaf.snapshotLoad();
        defer snap.release();
        try testing.expectEqual(newest_before, snap.value.newestSeqOf(&parent_key).?);
    }
    h.lane.hold = false;

    // Three consecutive failures latch FAILED (the latch logs the one loud
    // line); later commits answer it.
    log.testing.expectErrorLogs(1);
    h.lane.fail_next = 3;
    for (0..3) |i| {
        var nb: [8]u8 = undefined;
        try testing.expectError(error.JournalWriteFailed, h.commitCreate(try std.fmt.bufPrint(&nb, "f{d}", .{i})));
    }
    try testing.expect(h.latch.isFailed());
    try log.testing.expectAllFired();
    try testing.expectError(error.VolumeFailed, h.commitCreate("post-failed"));
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
    // acks ≡ drained − refusals (the refusals: the victim, A, the three).
    try testing.expectEqual(@as(u64, 5), h.cv.stats.refusals.load());
    try testing.expectEqual(h.cv.stats.drained_txs.load(), h.cv.stats.acks.load() + h.cv.stats.refusals.load());
}

test "an escaped record (a freeze raced the failed write) is compensated through the checkpoint class: a reopen folds to the rolled-back state" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const base = try h.commitCreate("base");
    _ = try h.checkpoint();
    h.lane.hold = true;
    var t = tx.KvTx.init(h.gpa, @src());
    const victim = try h.stageCreate(&t, "escapee");
    var d: Done = .init;
    var e = try h.cv.build(&t, &d);
    try h.cv.enqueueOne(&e, e.q.len);
    h.cv.leadIfElected();
    try testing.expectEqual(@as(usize, 1), h.lane.heldCount());
    // The window is applied but unwritten; a flush pass freezes the leaves
    // and appends them: the victim's records ESCAPE the open overlay.
    for (h.vol.trees.items) |tr| try h.vol.flushPass(tr);
    h.lane.failWindow(0);
    h.lane.pump();
    try testing.expectError(error.JournalWriteFailed, try d.recvOutcome(testing.io));
    try testing.expect(h.cv.stats.compensations.load() > 0);
    try testing.expect(!try h.present(victim));
    try testing.expect(try h.present(base));
    h.lane.hold = false;
    // A crash-equivalent reopen from the pre-failure ledger: the escaped
    // bset plus the compensation entry fold to the rolled-back state.
    var r = try h.reopen();
    defer r.deinit();
    try testing.expect(!try Harness.replayedPresent(&r, victim));
    try testing.expect(try Harness.replayedPresent(&r, base));
}

// generic/475 under dm-error: the second failed window's rollback found
// an escaped record and its compensation entry's write met the dead device
// — before this a divergence TRIPWIRE on a volume no rung had latched yet.
test "a compensation entry whose device write fails is the fail-stop lattice's own event: the volume latches FAILED at once (one loud line), meta_conveyor_compensation_write_failures +1, the divergence tripwire untouched, every later commit refused" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const base = try h.commitCreate("base");
    _ = try h.checkpoint();
    h.lane.hold = true;
    var t = tx.KvTx.init(h.gpa, @src());
    const victim = try h.stageCreate(&t, "escapee");
    var d: Done = .init;
    var e = try h.cv.build(&t, &d);
    try h.cv.enqueueOne(&e, e.q.len);
    h.cv.leadIfElected();
    try testing.expectEqual(@as(usize, 1), h.lane.heldCount());
    for (h.vol.trees.items) |tr| try h.vol.flushPass(tr);
    // The SMO journal's device answers EIO from here: the compensation
    // entry's write is the first write to meet it.
    const DeadDevice = struct {
        fn write(_: *anyopaque, _: []const journal.WriteOp) error{Io}!void {
            return error.Io;
        }
        fn completed(_: *anyopaque) void {}
    };
    var dead: u8 = 0;
    h.vol.journal.device = .{ .ctx = &dead, .write = &DeadDevice.write, .completed = &DeadDevice.completed };
    defer h.vol.journal.device = null;
    const site0 = counters.tripwireCount("conveyor_divergence_compensation");
    const failures0 = h.cv.stats.compensation_write_failures.load();
    try testing.expect(!h.latch.isFailed());
    log.testing.expectErrorLogs(1);
    h.lane.failWindow(0);
    h.lane.pump();
    try log.testing.expectAllFired();
    try testing.expectError(error.JournalWriteFailed, try d.recvOutcome(testing.io));
    try testing.expectEqual(failures0 + 1, h.cv.stats.compensation_write_failures.load());
    try testing.expectEqual(site0, counters.tripwireCount("conveyor_divergence_compensation"));
    try testing.expectEqual(@as(u64, 0), h.cv.stats.divergence_compensation.load());
    try testing.expect(h.latch.isFailed());
    try testing.expectError(error.VolumeFailed, h.commitCreate("post"));
    h.lane.hold = false;
    try testing.expect(!try h.present(victim));
    try testing.expect(try h.present(base));
}

test "a rollback that cannot complete (out of memory on its first allocation) is RAM ≠ replay: the volume latches FAILED at once, the divergence tripwire fires, the stuck records are never served as acked" {
    var fa = std.testing.FailingAllocator.init(testing.allocator, .{});
    const h = try Harness.createWithGpa(.{}, null, false, fa.allocator());
    defer h.destroy();
    const base = try h.commitCreate("base");
    // The failed window's rollback runs out of memory before it can remove
    // the window's records from the leaf: RAM holds records whose entry
    // never landed — the one class §1.8 says must never be silent.
    h.lane.fail_next = 1;
    h.lane.oom_on_rollback = &fa;
    log.testing.expectErrorLogs(2);
    try testing.expectError(error.JournalWriteFailed, h.commitCreate("victim"));
    try log.testing.expectAllFired();
    h.lane.oom_on_rollback = null;
    try testing.expect(h.latch.isFailed());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.divergence_rollback.load());
    try testing.expect(try h.present(base));
    try testing.expectError(error.VolumeFailed, h.commitCreate("post"));
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
}

// Owner ruling 2026-10-19: a divergence on a volume ALREADY FAILED (a
// dm-error device: the rollback's own writes meet EIO, generic/475) is the
// fail-stop's consequence — counted, never the tripwire that is for a
// divergence latching a healthy volume.
test "a rollback that cannot complete on a volume ALREADY FAILED is the counted FAILED-volume outcome: meta_conveyor_divergences_failed_volume +1, the divergence tripwire untouched, no error log; the commit is still refused and the volume stays FAILED" {
    var fa = std.testing.FailingAllocator.init(testing.allocator, .{});
    const h = try Harness.createWithGpa(.{}, null, false, fa.allocator());
    defer h.destroy();
    const base = try h.commitCreate("base");
    // Hold the window in the lane, latch the volume FAILED underneath it
    // (the journal's third consecutive failure elsewhere), then fail the
    // held write with the rollback out of memory.
    h.lane.hold = true;
    var t = tx.KvTx.init(h.gpa, @src());
    _ = try h.stageCreate(&t, "victim");
    var d: Done = .init;
    var e = try h.cv.build(&t, &d);
    try h.cv.enqueueOne(&e, e.q.len);
    h.cv.leadIfElected();
    try testing.expectEqual(@as(usize, 1), h.lane.heldCount());
    try testing.expect(h.latch.failStop());
    const div0 = h.cv.stats.divergence_rollback.load();
    const site0 = counters.tripwireCount("conveyor_divergence_rollback");
    const failed0 = h.cv.stats.divergences_failed_volume.load();
    h.lane.oom_on_rollback = &fa;
    h.lane.failWindow(0);
    h.lane.pump();
    h.lane.oom_on_rollback = null;
    try testing.expectError(error.JournalWriteFailed, try d.recvOutcome(testing.io));
    try testing.expectEqual(failed0 + 1, h.cv.stats.divergences_failed_volume.load());
    try testing.expectEqual(div0, h.cv.stats.divergence_rollback.load());
    try testing.expectEqual(site0, counters.tripwireCount("conveyor_divergence_rollback"));
    try testing.expect(h.latch.isFailed());
    try testing.expect(try h.present(base));
    try testing.expectError(error.VolumeFailed, h.commitCreate("post"));
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
}

test "the in-flight window map never fills with tombstones (§BV): 20 000 windows tracked and untracked two at a time leave its tombstones under a quarter of the table and allocate nothing — a new window's `track` never probes the whole of it" {
    var fa = std.testing.FailingAllocator.init(testing.allocator, .{});
    const h = try Harness.createWithGpa(.{}, null, false, fa.allocator());
    defer h.destroy();
    fa.fail_index = fa.alloc_index;
    defer fa.fail_index = std.math.maxInt(usize);
    var pair: [2]Window = undefined;
    for (0..20_000) |i| {
        const w = &pair[i % 2];
        w.* = .{ .conveyor = h.cv, .arena = undefined, .members = &.{}, .failed = &.{}, .res = Reservation.unpadded(@as(u64, i) * 4096, 100), .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .t_handoff_ns = 0 };
        try h.cv.track(w);
        if (i > 0) h.cv.untrack(&pair[(i + 1) % 2]);
    }
    const cap = h.cv.inflight.capacity();
    var tombstones: usize = 0;
    for (h.cv.inflight.metadata.?[0..cap]) |md| tombstones += @intFromBool(md.isTombstone());
    h.cv.untrack(&pair[(20_000 - 1) % 2]);
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
    try testing.expect(tombstones * 4 <= cap);
}

test "the pass gives its venue a turn between two batches (§BV): a group of three under a one-tx batch cap is ONE pass of three batches — the venue's yield runs after the first and the second (more was queued), never after the last; every member acked" {
    const h = try Harness.create(.{}, .{ .max_txs = 1, .max_bytes = 1 << 20 }, true);
    defer h.destroy();
    const Turns = struct {
        var yields: u32 = 0;
        var queued_at_yield: [4]u64 = @splat(0);
        var cv: ?*Conveyor = null;
        fn yield(_: *anyopaque) void {
            if (yields < queued_at_yield.len) queued_at_yield[yields] = cv.?.stats.queued.load();
            yields += 1;
        }
    };
    Turns.yields = 0;
    Turns.cv = h.cv;
    h.cv.deps.pass.yield = Turns.yield;
    var txs: [3]*tx.KvTx = undefined;
    var inos: [3]u64 = undefined;
    var names: [3][8]u8 = undefined;
    for (&txs, &inos, &names, 0..) |*t, *ino, *nb, i| {
        t.* = try h.gpa.create(tx.KvTx);
        t.*.* = tx.KvTx.init(h.gpa, @src());
        ino.* = try h.stageCreate(t.*, try std.fmt.bufPrint(nb, "turn-{d}", .{i}));
    }
    defer for (txs) |t| h.gpa.destroy(t);
    var results: [3]Outcome = undefined;
    const passes0 = h.cv.stats.leader_passes.load();
    const yields0 = h.cv.stats.pass_yields.load();
    h.cv.commitGroup(&txs, &results);
    for (results) |r| try r;
    try testing.expectEqual(passes0 + 3, h.cv.stats.leader_passes.load());
    try testing.expectEqual(@as(u32, 2), Turns.yields);
    try testing.expectEqual(yields0 + 2, h.cv.stats.pass_yields.load());
    try testing.expectEqual(@as(u64, 2), Turns.queued_at_yield[0]);
    try testing.expectEqual(@as(u64, 1), Turns.queued_at_yield[1]);
    for (inos) |ino| try testing.expect(try h.present(ino));
}

test "the in-flight window map is sized at init: tracking a full population of windows allocates nothing (never an allocation under inflight_lock)" {
    var fa = std.testing.FailingAllocator.init(testing.allocator, .{});
    const h = try Harness.createWithGpa(.{}, null, false, fa.allocator());
    defer h.destroy();
    // Every allocation from here on fails: a `track` that allocates is red.
    fa.fail_index = fa.alloc_index;
    defer fa.fail_index = std.math.maxInt(usize);
    const n = h.cv.inflight_cap;
    try testing.expectEqual(windowCap(h.lane.window_cells), n);
    const windows = try testing.allocator.alloc(Window, n);
    defer testing.allocator.free(windows);
    for (windows, 0..) |*w, i| {
        w.* = .{ .conveyor = h.cv, .arena = std.heap.ArenaAllocator.init(testing.allocator), .members = &.{}, .failed = &.{}, .res = Reservation.unpadded(@as(u64, i) * 4096, 100), .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .t_handoff_ns = 0 };
        try h.cv.track(w);
    }
    try testing.expectEqual(n, h.cv.inflight.count());
    try testing.expectEqual(@as(u64, n), h.cv.windowsInflight());
    // One past the population is refused loud, not grown under the lock.
    var extra: Window = .{ .conveyor = h.cv, .arena = std.heap.ArenaAllocator.init(testing.allocator), .members = &.{}, .failed = &.{}, .res = Reservation.unpadded(@as(u64, n) * 4096, 100), .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .t_handoff_ns = 0 };
    try testing.expectError(error.OutOfMemory, h.cv.track(&extra));
    for (windows) |*w| h.cv.untrack(w);
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
}

/// Yield until `cond.ready()` or 10 s (a bound against the hang only: the
/// observables move in microseconds); false names the miss.
fn awaitObservable(cond: anytype) bool {
    const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (!cond.ready()) {
        if (clock.monotonicNs() >= deadline) return false;
        std.Thread.yield() catch std.atomic.spinLoopHint();
    }
    return true;
}

// The joined appender's pass panic (TWO-DAEMON MATRIX, daemon B under the
// 4-wide load): its lane's cells were sized by the 512 KiB floor ring and
// its grown ring admitted more windows than them — `tar -x`'s chmod met a
// full lane (`ConveyorFull`, 1 member). The ring never bounds the lane:
// only the window gate does.
test "THE WINDOW GATE: with the lane's window cells all but one held (nothing landed), the next pass parks holding nothing — no reservation, no ring budget — and hands its window a cell at the first landing; a full lane is never met (no pass panic, no refusal), every member acked" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const io = testing.io;
    const cells = 4;
    h.lane.window_cells = cells;
    h.cv.deps.lane.window_cells = cells;
    h.lane.hold = true;
    h.threaded_pass = true;
    const pass_thread = try std.Thread.spawn(.{}, Harness.passThreadMain, .{h});
    // Every exit lands what the lane holds before the quiesce (a red run
    // leaves windows held) and joins the pass thread.
    defer {
        while (h.cv.windowsInflight() != 0) _ = h.lane.landDue(0);
        h.cv.quiesce();
        h.pass_stop.store(true, .release);
        h.pass_wake.notifyOne(io);
        pass_thread.join();
    }
    const panics0 = h.cv.stats.pass_panics.load();
    const handoffs0 = h.cv.stats.lane_handoffs.load();
    var txs: [cells + 1]tx.KvTx = undefined;
    var dones: [cells + 1]Done = @splat(.init);
    var inos: [cells + 1]u64 = undefined;
    var head_before_park: u64 = 0;
    var issued: usize = 0;
    const Moved = struct {
        cv: *Conveyor,
        handoffs: u64,
        fn ready(s: @This()) bool {
            return s.cv.stats.lane_handoffs.load() >= s.handoffs or s.cv.stats.window_gate_parks.load() != 0 or s.cv.stats.pass_panics.load() != 0;
        }
    };
    // One committer at a time, each its own pass and window, until a pass
    // stops short of its handoff.
    while (issued < txs.len) : (issued += 1) {
        txs[issued] = tx.KvTx.init(h.gpa, @src());
        var nb: [8]u8 = undefined;
        inos[issued] = try h.stageCreate(&txs[issued], try std.fmt.bufPrint(&nb, "w{d}", .{issued}));
        var e = try h.cv.build(&txs[issued], &dones[issued]);
        head_before_park = h.vol.core.headPos();
        try h.cv.enqueueOne(&e, e.q.len);
        h.cv.leadIfElected();
        if (!awaitObservable(Moved{ .cv = h.cv, .handoffs = handoffs0 + issued + 1 })) return error.PassNeverMoved;
        if (h.cv.stats.lane_handoffs.load() < handoffs0 + issued + 1) {
            issued += 1;
            break;
        }
    }
    // The gate, not the lane, stopped the flight: `cells − 1` windows held,
    // the next pass parked BEFORE its reservation (the head unmoved, no
    // ring budget admitted), nothing abandoned.
    try testing.expectEqual(panics0, h.cv.stats.pass_panics.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.window_gate_parks.load());
    try testing.expectEqual(@as(usize, cells), issued);
    try testing.expectEqual(handoffs0 + cells - 1, h.cv.stats.lane_handoffs.load());
    try testing.expectEqual(head_before_park, h.vol.core.headPos());
    try testing.expectEqual(@as(u64, 0), h.vol.core.admitted());
    // The first landing: the parked pass resumes and hands its window a cell.
    try testing.expect(h.lane.landDue(0));
    const Handed = struct {
        cv: *Conveyor,
        want: u64,
        fn ready(s: @This()) bool {
            return s.cv.stats.lane_handoffs.load() >= s.want;
        }
    };
    if (!awaitObservable(Handed{ .cv = h.cv, .want = handoffs0 + cells })) return error.ParkedPassNeverResumed;
    while (h.cv.windowsInflight() != 0) _ = h.lane.landDue(0);
    for (dones[0..issued]) |*d| try try d.recvOutcome(io);
    for (inos[0..issued]) |ino| try testing.expect(try h.present(ino));
    try testing.expectEqual(panics0, h.cv.stats.pass_panics.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expect(h.cv.stats.windows_inflight_hwm.load() <= windowCap(cells));
}

// The window gate's report (0003 §BN): `queueCapacity(pages)` said the
// ring bounds the committer queue, but a committer queues BEFORE ring
// admission — the 129th simultaneous committer of a floor ring got
// `ConveyorFull`, EAGAIN to a create, an mkdir, an unlink.
test "THE COMMITTER BURST: with the pass parked at the window gate (the one window it may fly held un-landed, the next batch drained) and every queue cell taken, 16 committers more are none of them refused — each parks for room (never ConveyorFull, never EAGAIN) and the drain that frees room wakes it; every create acked and present" {
    const h = try Harness.create(.{ .ring_pages = 120 }, null, false);
    defer h.destroy();
    const io = testing.io;
    const cells = 2;
    h.lane.window_cells = cells;
    h.cv.deps.lane.window_cells = cells;
    h.lane.hold = true;
    h.threaded_pass = true;
    const cap = h.cv.slots.len;
    const extra = 16;
    const n = 2 + cap + extra;
    const Committer = struct {
        h: *Harness,
        idx: usize,
        finished: *std.atomic.Value(usize),
        ino: u64 = 0,
        result: anyerror!void = {},

        fn run(self: *@This()) void {
            var nb: [16]u8 = undefined;
            const name = std.fmt.bufPrint(&nb, "burst{d}", .{self.idx}) catch unreachable;
            if (self.h.commitCreate(name)) |ino| self.ino = ino else |e| self.result = e;
            _ = self.finished.fetchAdd(1, .acq_rel);
        }
    };
    var finished: std.atomic.Value(usize) = .init(0);
    const committers = try testing.allocator.alloc(Committer, n);
    defer testing.allocator.free(committers);
    const threads = try testing.allocator.alloc(std.Thread, n);
    defer testing.allocator.free(threads);
    const pass_thread = try std.Thread.spawn(.{}, Harness.passThreadMain, .{h});
    var spawned: usize = 0;
    // Every exit lands what the lane holds until every committer finished
    // (a red run leaves windows held), joins them, then the pass thread.
    defer {
        while (finished.load(.acquire) < spawned) {
            if (!h.lane.landDue(0)) std.Thread.yield() catch {};
        }
        for (threads[0..spawned]) |t| t.join();
        while (h.cv.windowsInflight() != 0) _ = h.lane.landDue(0);
        h.cv.quiesce();
        h.pass_stop.store(true, .release);
        h.pass_wake.notifyOne(io);
        pass_thread.join();
    }
    const S = struct {
        fn spawn(hh: *Harness, cs: []Committer, ts: []std.Thread, at: *usize, upto: usize, fin: *std.atomic.Value(usize)) !void {
            while (at.* < upto) : (at.* += 1) {
                cs[at.*] = .{ .h = hh, .idx = at.*, .finished = fin };
                ts[at.*] = try std.Thread.spawn(.{ .stack_size = 4 << 20 }, Committer.run, .{&cs[at.*]});
            }
        }
    };
    const Reached = struct {
        cv: *Conveyor,
        handoffs: u64,
        gate_parks: u64,
        pending: usize,
        fn ready(s: @This()) bool {
            return s.cv.stats.lane_handoffs.load() >= s.handoffs and s.cv.stats.window_gate_parks.load() >= s.gate_parks and s.cv.pending() >= s.pending;
        }
    };
    const handoffs0 = h.cv.stats.lane_handoffs.load();
    // The flight's one window, held; then the pass parked at the gate with
    // the next batch; then every queue cell.
    try S.spawn(h, committers, threads, &spawned, 1, &finished);
    if (!awaitObservable(Reached{ .cv = h.cv, .handoffs = handoffs0 + 1, .gate_parks = 0, .pending = 0 })) return error.FirstWindowNeverHanded;
    try S.spawn(h, committers, threads, &spawned, 2, &finished);
    if (!awaitObservable(Reached{ .cv = h.cv, .handoffs = handoffs0 + 1, .gate_parks = 1, .pending = 0 })) return error.PassNeverParkedAtTheGate;
    try S.spawn(h, committers, threads, &spawned, 2 + cap, &finished);
    if (!awaitObservable(Reached{ .cv = h.cv, .handoffs = handoffs0 + 1, .gate_parks = 1, .pending = cap })) return error.QueueNeverFilled;
    // The burst past the queue: each extra either answered (a refusal: no
    // window can land, so nothing else answers it) or parked for room.
    const room0 = h.cv.stats.commit_room_parks.load();
    try S.spawn(h, committers, threads, &spawned, n, &finished);
    const Burst = struct {
        cv: *Conveyor,
        finished: *std.atomic.Value(usize),
        room0: u64,
        fn ready(s: @This()) bool {
            return s.finished.load(.acquire) + (s.cv.stats.commit_room_parks.load() - s.room0) >= extra;
        }
    };
    if (!awaitObservable(Burst{ .cv = h.cv, .finished = &finished, .room0 = room0 })) return error.BurstNeverArrived;
    try testing.expectEqual(cap, h.cv.pending());
    // The landings: the gate opens, every drain frees room, every parked
    // committer enqueues and is acked.
    while (finished.load(.acquire) < n) {
        if (!h.lane.landDue(0)) std.Thread.yield() catch {};
    }
    var refused: usize = 0;
    for (committers) |c| c.result catch |e| {
        if (refused == 0) std.debug.print("\ncommitter burst: committer {d} answered {t}\n", .{ c.idx, e });
        refused += 1;
    };
    try testing.expectEqual(@as(usize, 0), refused);
    for (committers) |c| try testing.expect(try h.present(c.ino));
    try testing.expectEqual(room0 + extra, h.cv.stats.commit_room_parks.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
}

test "the admission park: a ring too full for the batch parks the pass holding no node lock, kicks the checkpoint with the park mark, and resolves without a timer" {
    // A tiny ring: 64 pages (260,608 B logical, 4 KiB reserve floor in the
    // test geometry is the production 256 KiB, so the user slice is small).
    const h = try Harness.create(.{ .ring_pages = 80 }, null, false);
    defer h.destroy();
    const user_cap = ringUserCapacity(h.vol.journal.geo);
    // Fill most of the user slice with commits the lane lands but nothing
    // checkpoints (the harness's cycle only runs on a kick).
    var filled: u64 = 0;
    var i: u64 = 0;
    const first_kicks = h.park_kicks;
    while (filled + 4096 < user_cap / 2) : (i += 1) {
        var nb: [12]u8 = undefined;
        _ = try h.commitCreate(try std.fmt.bufPrint(&nb, "fill{d}", .{i}));
        filled = h.vol.core.headPos();
    }
    try testing.expectEqual(first_kicks, h.park_kicks);
    // Now commits past the capacity must park: the kick runs the cycle
    // inline (tail = head after the flush), the retry admits.
    while (h.park_kicks == first_kicks) : (i += 1) {
        var nb: [12]u8 = undefined;
        _ = try h.commitCreate(try std.fmt.bufPrint(&nb, "park{d}", .{i}));
    }
    try testing.expect(h.park_kicks > first_kicks);
    try testing.expect(h.cv.stats.full_stalls.load() > 0);
    try testing.expectEqual(@as(u64, 0), h.cv.stats.park_tick_rescues.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
}

/// A committer thread that commits until the conveyor's stall count moves
/// past `stalls_before` (its own pass parked and resumed).
const ParkingCommitter = struct {
    h: *Harness,
    stalls_before: u64,
    tag: []const u8,
    err: ?anyerror = null,

    fn run(self: *ParkingCommitter) void {
        var n: u64 = 0;
        while (self.h.cv.stats.full_stalls.load() == self.stalls_before) : (n += 1) {
            var nb: [16]u8 = undefined;
            _ = self.h.commitCreate(std.fmt.bufPrint(&nb, "{s}{d}", .{ self.tag, n }) catch unreachable) catch |e| {
                self.err = e;
                return;
            };
        }
    }
};

test "the admission park wakes on the notify, never the tick: a pass parked on a full ring resumes on another thread's space wake with zero tick rescues; the tick alone (no wake) is counted as the rescue it is" {
    const h = try Harness.create(.{ .ring_pages = 80 }, null, false);
    defer h.destroy();
    // The kick runs nothing: the ring is opened by the OTHER thread below.
    h.park_runs_cycle = false;
    defer test_seams.hold_at_park.store(0, .release);
    const user_cap = ringUserCapacity(h.vol.journal.geo);
    var i: u64 = 0;
    while (h.vol.core.headPos() + 4096 < user_cap / 2) : (i += 1) {
        var nb: [12]u8 = undefined;
        _ = try h.commitCreate(try std.fmt.bufPrint(&nb, "fill{d}", .{i}));
    }
    // Round 1: the pass parks and is held in the window between its
    // registration on `space` and its wait; this thread covers the ring and
    // sends THE wake (what `afterDurableBarrier` does on every barrier that
    // moves `reusable_upto`); released, the pass resumes on the wake.
    test_seams.hold_at_park.store(1, .release);
    test_seams.park_held.store(0, .release);
    var p: ParkingCommitter = .{ .h = h, .stalls_before = 0, .tag = "park" };
    const t = try std.Thread.spawn(.{}, ParkingCommitter.run, .{&p});
    while (test_seams.park_held.load(.acquire) == 0) try std.Thread.yield();
    try h.coverRing(user_cap);
    h.space.notifyWaiters(testing.io);
    test_seams.hold_at_park.store(0, .release);
    t.join();
    if (p.err) |e| return e;
    try testing.expectEqual(@as(u64, 1), h.cv.stats.full_stalls.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.park_tick_rescues.load());
    // Round 2: the ring is covered WITHOUT a wake — only the backstop tick
    // can end the park, and it is counted as the rescue it is.
    while (h.vol.core.headPos() + 4096 < h.vol.core.reusableUpto() + user_cap / 2) : (i += 1) {
        var nb: [12]u8 = undefined;
        _ = try h.commitCreate(try std.fmt.bufPrint(&nb, "fill{d}", .{i}));
    }
    test_seams.hold_at_park.store(1, .release);
    test_seams.park_held.store(0, .release);
    var p2: ParkingCommitter = .{ .h = h, .stalls_before = 1, .tag = "late" };
    const t2 = try std.Thread.spawn(.{}, ParkingCommitter.run, .{&p2});
    while (test_seams.park_held.load(.acquire) == 0) try std.Thread.yield();
    try h.coverRing(user_cap);
    test_seams.hold_at_park.store(0, .release);
    t2.join();
    if (p2.err) |e| return e;
    try testing.expectEqual(@as(u64, 2), h.cv.stats.full_stalls.load());
    try testing.expectEqual(@as(u64, 1), h.cv.stats.park_tick_rescues.load());
}

test "coalesced vs strict: a coalesced ack precedes the barrier (acked ≠ durable until the trailing barrier); a strict ack never precedes it; fsync is the trailing barrier" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    _ = try h.commitCreate("coalesced");
    try testing.expectEqual(@as(u64, 0), h.lane.barriers);
    try testing.expect(h.lane.needs_flush);
    try h.cv.barrier();
    try testing.expectEqual(@as(u64, 1), h.lane.barriers);
    try testing.expect(!h.lane.needs_flush);

    const s = try Harness.create(.{}, null, true);
    defer s.destroy();
    _ = try s.commitCreate("strict");
    try testing.expectEqual(@as(u64, 1), s.lane.barriers);
    _ = try s.commitCreate("strict2");
    try testing.expectEqual(@as(u64, 2), s.lane.barriers);
}

test "power cut after an acked coalesced commit loses it (never barriered); after a strict ack or a barrier it survives; replay is whole-transaction atomic and fsck-shaped records agree" {
    const h = try Harness.create(.{}, null, false);
    defer h.destroy();
    const durable = try h.commitCreate("durable");
    _ = try h.checkpoint();
    const acked = try h.commitCreate("acked-not-barriered");
    try testing.expect(try h.present(acked));
    // The cut: every volatile write reverts.
    try testing.expect(h.image.powerCut() > 0);
    var r = try h.reopen();
    defer r.deinit();
    try testing.expect(try Harness.replayedPresent(&r, durable));
    try testing.expect(!try Harness.replayedPresent(&r, acked));
    // No partial tx: the dentry of the lost create is absent too.
    var kvs: std.ArrayList(tree.KeyValue) = .empty;
    defer tree.deinitKeyValues(h.gpa, &kvs);
    const range = tx.dentryChainRange(1, key.dentryNameHash54("acked-not-barriered", &h.name_key));
    try r.treeOf(.native).range(h.gpa, &range.start, &range.end, 8, &kvs);
    try testing.expectEqual(@as(usize, 0), kvs.items.len);

    const s = try Harness.create(.{}, null, true);
    defer s.destroy();
    _ = try s.checkpoint();
    const strict_ino = try s.commitCreate("strict-acked");
    _ = s.image.powerCut();
    var rs = try s.reopen();
    defer rs.deinit();
    try testing.expect(try Harness.replayedPresent(&rs, strict_ino));
    try testing.expectEqual(@as(u64, 0), rs.recovery.dropped_torn);
}

test "4-thread commit storm with concurrent checkpoints: every ack present, replay holds every acked record, identities close at quiesce" {
    const h = try Harness.create(.{ .extents = 1024 }, null, false);
    defer h.destroy();
    const threads = 4;
    const per_thread = 40;
    const Storm = struct {
        fn run(hh: *Harness, lane_id: usize, out: *[per_thread]u64) void {
            for (0..per_thread) |i| {
                var nb: [16]u8 = undefined;
                const name = std.fmt.bufPrint(&nb, "s{d}-{d}", .{ lane_id, i }) catch unreachable;
                // Each thread's tx is built on its own frame; the ino is
                // minted by the shared watermark.
                var t = tx.KvTx.init(hh.gpa, @src());
                out[i] = hh.stageCreate(&t, name) catch unreachable;
                hh.cv.commit(&t) catch unreachable;
            }
        }
    };
    var acked: [threads][per_thread]u64 = undefined;
    var handles: [threads]std.Thread = undefined;
    for (&handles, 0..) |*hd, i| hd.* = try std.Thread.spawn(.{}, Storm.run, .{ h, i, &acked[i] });
    for (handles) |hd| hd.join();
    h.cv.quiesce();
    for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try h.present(ino));
    try testing.expectEqual(@as(u64, threads * per_thread), h.cv.stats.acks.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expectEqual(h.cv.stats.drained_txs.load(), h.cv.stats.acks.load() + h.cv.stats.refusals.load());
    try testing.expectEqual(h.cv.stats.leader_passes.load(), h.cv.stats.group_size.total());
    // A landed window whose predecessor was still with its applier (§AW)
    // waits for it and shares its durability pass: passes ≤ handoffs.
    try testing.expect(h.lane.durability_passes <= h.cv.stats.lane_handoffs.load());
    try testing.expectEqual(h.cv.stats.lane_handoffs.load(), @as(u64, h.lane.ack_order.items.len));
    try testing.expectEqual(@as(u64, 0), h.cv.stats.windows_inflight.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.queued.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.commit_parked.load());
    try testing.expectEqual(@as(u64, 0), h.vol.cache.heapPromised());
    try testing.expectEqual(@as(usize, 0), h.vol.reg.openCount());
    // Acks left in journal order.
    for (h.lane.ack_order.items[1..], 1..) |s, i| try testing.expect(s > h.lane.ack_order.items[i - 1]);
    // A checkpoint, then a reopen: every acked record present.
    _ = try h.checkpoint();
    var r = try h.reopen();
    defer r.deinit();
    for (acked) |lane_acks| for (lane_acks) |ino| try testing.expect(try Harness.replayedPresent(&r, ino));
}

test "the stats JSON names every conveyor face" {
    var s: Stats = .{};
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try s.writeJson(&w);
    const out = w.buffered();
    for ([_][]const u8{ "meta_commit_group_size", "meta_conveyor_leader_passes", "meta_conveyor_windows_inflight_hwm", "meta_conveyor_acks", "meta_kv_journal_full_stalls", "meta_conveyor_park_tick_rescues", "meta_conveyor_pass_panics", "meta_conveyor_holds", "meta_conveyor_hold_landings", "meta_conveyor_hold_deadlines", "meta_conveyor_holds_not_worth", "meta_conveyor_hold_probes", "meta_conveyor_hold_gain_milli", "meta_conveyor_windows_held", "meta_conveyor_windows_unshadowed", "meta_conveyor_windows_pipelined", "meta_conveyor_hold_bound_ns", "meta_conveyor_leafwise_applies", "meta_conveyor_leafwise_leaves", "meta_conveyor_pass_yields" }) |name| {
        try testing.expect(std.mem.indexOf(u8, out, name) != null);
    }
    var pb: [8192]u8 = undefined;
    var pw: std.Io.Writer = .fixed(&pb);
    try s.writePhasesJson(&pw);
    for ([_][]const u8{ "meta_pass_hold", "leafwise_leaf_hold", "meta_leafwise_apply", "meta_self_window_span" }) |name| {
        try testing.expect(std.mem.indexOf(u8, pw.buffered(), name) != null);
    }
}

// ---------------------------------------------------------------------------
// The journal's pad economy (decision 0003 §L).
// ---------------------------------------------------------------------------

test "§L tie: the pad arithmetic — a sub-page window costs one grain whatever it carries; pad fraction 1 − n·s/g; the 512 B shape's 76 B entry needs 53 to fill a page, 27 to stop being pad-majority; the byte bound at the measured shapes" {
    const geo = journal.productionGeometry(2048, 4096);
    // The 512 B-write shape: Δ(size, mtime, ctime) = 24 B header + 1 tag +
    // 16 B record header + 9 B inode key + 26 B delta = 76 B.
    const s: u64 = 24 + 1 + 16 + 9 + (2 + 8 + 8 + 8);
    try testing.expectEqual(@as(u64, 76), s);
    const one = windowCost(geo, 1, s);
    try testing.expectEqual(@as(u64, 4096), one.device_bytes);
    try testing.expectEqual(@as(u64, 4072 - 76), one.pad_bytes);
    try testing.expectEqual(@as(u64, 975_585), one.padFractionPpm()); // 97.6 %
    // n = 27: entries 2052 ≥ pad 2020 — the first entry-majority population.
    try testing.expect(windowCost(geo, 26, s).pad_bytes > 26 * s);
    try testing.expect(windowCost(geo, 27, s).pad_bytes <= 27 * s);
    // n = 53 fills the page (4028 + a 44 B pad ≥ PAD_MIN); 54 spills into
    // a second page whose pad is nearly whole.
    try testing.expectEqual(@as(u64, 4096), windowCost(geo, 53, s).device_bytes);
    try testing.expectEqual(@as(u64, 44), windowCost(geo, 53, s).pad_bytes);
    try testing.expectEqual(@as(u64, 8192), windowCost(geo, 54, s).device_bytes);
    // The measured venue shape (rand-4k-write, nvmet-loop, F14): 12.4 k
    // entries/s, round trip 233 µs, pass 34 µs ⇒ 3 entries per paced
    // window ⇒ ≤ 1,365 B/op (was 2,929 B/op = 0.715 × 4 KiB with
    // 1.46 per window); the 727 k IOPS bar at the same latencies ⇒ 194
    // per window ⇒ 2s per op (entry-majority, the pipelined class).
    try testing.expectEqual(@as(u64, 1365), bytesPerOpBound(geo, 12_400, s, 233_000, 34_000));
    try testing.expectEqual(2 * s, bytesPerOpBound(geo, 727_000, s, 233_000, 34_000));
    // A lone committer (nothing in flight) pays one grain per op: the floor.
    try testing.expectEqual(@as(u64, 4096), bytesPerOpBound(geo, 1_000, s, 233_000, 34_000));
    // Unpadded ring: no pad, no grain — the law never holds there.
    const flat = journal.productionGeometry(2048, 1);
    try testing.expectEqual(@as(u64, 0), windowCost(flat, 1, s).pad_bytes);
    // The law is armed by the cadence: strict (0 ms) ⇒ none; coalesced ⇒
    // the interval is the deadline's ceiling.
    try testing.expect(windowHoldFor(0) == null);
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), windowHoldFor(50).?.ceiling_ns);
}

test "§L tie: the worth gate — OFF declines 159 eligible windows then probes 2 forming + 8 sampled holds whose MEAN opens it at ½ an entry; ON tracks the EWMA per hold and closes below ¼ (the 2:1 band); a burst's mean below ½ leaves it closed; the line scales with the batch (a quarter of the pad: c ≥ n/3) so a seven-entry batch collecting 0.4 declines" {
    // The small-write shapes (n̄ ≤ 1.5): the ½ / ¼ lines as measured.
    try testing.expect(!holdWorthIt(null, false, 1) and !holdWorthIt(null, true, 1));
    try testing.expect(!holdWorthIt(GAIN_OPEN - 1, false, 1));
    try testing.expect(holdWorthIt(GAIN_OPEN, false, 1));
    // The band: an open gate stays open down to a quarter.
    try testing.expect(holdWorthIt(GAIN_CLOSE, true, 1) and !holdWorthIt(GAIN_CLOSE - 1, true, 1));
    try testing.expect(holdWorthIt(GAIN_OPEN - 1, true, 1));
    try testing.expectEqual(GAIN_UNIT / 2, GAIN_OPEN);
    try testing.expectEqual(GAIN_UNIT / 4, GAIN_CLOSE);
    // The batch-scaled line: n/3 to open, n/6 to stay open — the ½ line
    // at n = 1 (n/3 < ½), 2⅓ entries at n = 7; the create shape's 0.4
    // per hold on seven-entry batches is declined and closes an open gate.
    try testing.expectEqual(GAIN_OPEN, gainLine(false, 1));
    try testing.expectEqual(GAIN_CLOSE, gainLine(true, 1));
    try testing.expectEqual(7 * GAIN_UNIT / 3, gainLine(false, 7));
    try testing.expectEqual(7 * GAIN_UNIT / 6, gainLine(true, 7));
    try testing.expect(!holdWorthIt(GAIN_UNIT * 2 / 5, false, 7) and !holdWorthIt(GAIN_UNIT * 2 / 5, true, 7));
    try testing.expect(holdWorthIt(3 * GAIN_UNIT, false, 7));
    try testing.expectEqual(@as(u32, 2), HOLD_PROBE_FORMING);
    try testing.expectEqual(@as(u32, 8), HOLD_PROBE_BURST);
    try testing.expectEqual(@as(u32, 160), HOLD_PROBE_PERIOD);
    var g: WorthGate = .{};
    // OFF: the first 159 eligible windows are declined, the 160th starts a burst.
    for (0..HOLD_PROBE_PERIOD - 1) |_| try testing.expectEqual(WorthGate.Verdict.decline, g.decide(1));
    // rand-4k-write's shape: the forming holds collect little (the unpaced
    // regime), the paced holds 1.375 — the burst's mean opens the gate.
    for (0..HOLD_PROBE_FORMING) |_| {
        try testing.expectEqual(WorthGate.Verdict.probe, g.decide(1));
        g.observe(.probe, GAIN_UNIT / 2, 1);
    }
    for (0..HOLD_PROBE_BURST) |i| {
        try testing.expectEqual(WorthGate.Verdict.probe, g.decide(1));
        g.observe(.probe, if (i % 10 < 3) 2 * GAIN_UNIT else GAIN_UNIT, 1);
    }
    try testing.expectEqual(@as(u64, 1375), g.gainMilli()); // 11/8 entries per hold
    try testing.expectEqual(WorthGate.Verdict.hold, g.decide(1));
    // ON: the EWMA tracks; empty holds close it in thirteen (1.375 ×
    // 0.875¹³ < ¼ — the band's lower line), after which it declines again.
    var holds_until_closed: u32 = 0;
    while (g.decide(1) == .hold) : (holds_until_closed += 1) g.observe(.hold, 0, 1);
    try testing.expectEqual(@as(u32, 13), holds_until_closed);
    // fsync-4k's shape: a burst whose mean is 0.125 leaves the gate closed.
    var f: WorthGate = .{};
    for (0..HOLD_PROBE_PERIOD - 1) |_| _ = f.decide(1);
    for (0..HOLD_PROBE_FORMING + HOLD_PROBE_BURST) |i| {
        try testing.expectEqual(WorthGate.Verdict.probe, f.decide(1));
        f.observe(.probe, if (i == 5) GAIN_UNIT else 0, 1);
    }
    try testing.expectEqual(@as(u64, 125), f.gainMilli());
    try testing.expectEqual(WorthGate.Verdict.decline, f.decide(1));
    // The create shape: a burst collecting 0.4 per hold on seven-entry
    // batches (above the ½ line, a twentieth of the pad) stays closed; an
    // open gate meeting it closes.
    var c: WorthGate = .{};
    for (0..HOLD_PROBE_PERIOD - 1) |_| _ = c.decide(7);
    for (0..HOLD_PROBE_FORMING + HOLD_PROBE_BURST) |_| {
        try testing.expectEqual(WorthGate.Verdict.probe, c.decide(7));
        c.observe(.probe, GAIN_UNIT * 2 / 5, 7);
    }
    try testing.expectEqual(@as(u64, 399), c.gainMilli());
    try testing.expectEqual(WorthGate.Verdict.decline, c.decide(7));
    var o: WorthGate = .{ .open = true };
    o.gain.sample(GAIN_UNIT * 2 / 5);
    try testing.expectEqual(WorthGate.Verdict.hold, o.decide(1));
    try testing.expectEqual(WorthGate.Verdict.decline, o.decide(7));
    try testing.expect(!o.open);
}

// THE §L RED TEST (red at f27c501: 16 windows per committer round, pad
// ≈ 94 %): the 512 B-write shape — N closed-loop committers, each op one
// simulated data DMA (spread over the device round trip, as the W1
// patch's is) then one 76 B Δ(size, mtime, ctime) inode record, a journal
// device that lands a window `D` after its handoff (pipelined: every
// window lands `D` after its own issue), the pass on its own thread.
// Without the hold every pass drains the arrivals of the last pass
// latency and lands them as one page; the law paces pad-majority windows
// one per round trip, so a window carries every committer that arrived
// during the previous one's flight. With `N·s < g` the two alternating
// cohorts sum to N per pair of windows whatever the split, so the pad
// fraction over the run is `1 − N·s/(2·g)` up to the first and last
// windows.
/// The window-fill row's committers: each its own inode's closed loop —
/// a simulated data DMA (a per-committer latency spread over the journal's
/// round trip: 16 distinct instants under D, so arrivals spread over a
/// window's flight instead of bursting with its landing), then ONE Δ
/// commit acked before the next.
const WindowFillCommitters = struct {
    const n = 48;
    const device_ns: u64 = 4 * std.time.ns_per_ms;

    fn run(hh: *Harness, sim: *Harness.DmaSim, ino: u64, index: usize, first: usize, rounds: usize, err_out: *?anyerror) void {
        const dma_ns: u64 = ((index * 7) % 16) * (device_ns / 16);
        for (first..first + rounds) |k| {
            var dma_done: sync.Oneshot(void) = .init;
            sim.submit(clock.monotonicNs() + dma_ns, &dma_done) catch |e| {
                err_out.* = e;
                return;
            };
            dma_done.recvUncancelable(testing.io) catch |e| {
                err_out.* = e;
                return;
            };
            var t = tx.KvTx.init(hh.gpa, @src());
            t.stageDelta(key.TREE_INODES, &key.inodeKey(ino), .{ .mask = record.DELTA_SIZE | record.DELTA_TIMES, .fields = .{ .size = 512 * (k + 1), .mtime = 2000 + k, .ctime = 2000 + k } }) catch |e| {
                err_out.* = e;
                return;
            };
            hh.cv.commit(&t) catch |e| {
                err_out.* = e;
                return;
            };
        }
    }

    /// `rounds` commits per committer from round `first`, every committer
    /// joined, every window at its terminal outcome.
    fn phase(h: *Harness, sim: *Harness.DmaSim, inos: *const [n]u64, first: usize, rounds: usize) !void {
        var errs: [n]?anyerror = @splat(null);
        var handles: [n]std.Thread = undefined;
        for (&handles, 0..) |*hd, i| hd.* = try std.Thread.spawn(.{}, run, .{ h, sim, inos[i], i, first, rounds, &errs[i] });
        for (handles) |hd| hd.join();
        for (errs, 0..) |e, i| if (e) |err| {
            std.debug.print("§L: committer {d} failed: {s}\n", .{ i, @errorName(err) });
            return err;
        };
        h.cv.quiesce();
    }
};

test "§L: the window-fill hold — 48 closed-loop committers of 76 B entries against a 4 ms device, every eligible window held (the gate pinned open): the pacing law under ANY schedule — pad-majority windows ≤ one per device round trip of the run + one per deadline, journal device bytes ≤ one grain per such window + twice the entry bytes — and at the paced schedule two windows per committer round at pad ≤ 1 − N·s/(2·g) + slack; every entry acked once, zero tick rescues" {
    // Leak-checked, no stack traces: `testing.allocator`'s trace capture
    // made a one-record apply cost ~0.5 ms in Debug — the pass latency
    // the shape measures against must stay far under the round trip.
    var dbg: std.heap.DebugAllocator(.{ .stack_trace_frames = 0, .thread_safe = true }) = .init;
    defer testing.expect(dbg.deinit() == .ok) catch @panic("the §L contract leaked");
    const gpa = dbg.allocator();
    const h = try Harness.createWithAllocators(.{ .extents = 1024, .ring_grain = 4096 }, null, false, gpa, gpa);
    defer h.destroy();
    const io = testing.io;
    const C = WindowFillCommitters;
    const n_committers = C.n;
    const per_committer = 96;
    // The simulated journal device's round trip: long against the pass
    // latency AND the device thread's own delivery grain (a futex wake per
    // completion, tens of µs on macOS: the 16 arrival phases, 250 µs
    // apart, must stay distinct), short for the row (≈ 96 × 2 × 4 ms ≈
    // 0.8 s of paced landings).
    const device_ns = C.device_ns;
    // The gate pinned open: the row measures the HOLD; when the gate opens
    // under a schedule is its learning law's (the worth-gate tie rows) — a
    // loaded host's thin holds close it, and its declined windows are the
    // unpaced regime, not this law's.
    h.cv.deps.window_hold = .{ .ceiling_ns = 50 * std.time.ns_per_ms, .pin_gate_open = true };
    h.lane.hold = true;
    h.threaded_pass = true;
    var dma: Harness.DmaSim = .{};
    defer dma.deinit();
    // The venue's threads are stopped and joined on EVERY exit (an
    // assertion that fails must not leave them running into `destroy`).
    var device_stop: std.atomic.Value(bool) = .init(false);
    const pass_thread = try std.Thread.spawn(.{}, Harness.passThreadMain, .{h});
    defer {
        h.cv.quiesce();
        h.pass_stop.store(true, .release);
        h.pass_wake.notifyOne(io);
        pass_thread.join();
    }
    const device_thread = try std.Thread.spawn(.{}, Harness.deviceThreadMain, .{ h, device_ns, &dma, &device_stop });
    defer {
        h.cv.quiesce();
        device_stop.store(true, .release);
        device_thread.join();
    }

    // Each committer's inode exists before the Δ stream (the fold has a
    // base); the creates are their own phase, outside the measurement.
    var inos: [n_committers]u64 = undefined;
    for (&inos, 0..) |*ino, i| {
        var nb: [12]u8 = undefined;
        ino.* = try h.commitCreate(try std.fmt.bufPrint(&nb, "w{d}", .{i}));
    }
    h.cv.quiesce();
    const st = &h.vol.journal.stats;
    const pad0 = st.written_pad_bytes.load();
    const dev0 = st.written_device_bytes.load();
    const win0 = st.written_windows.load();
    const ent0 = st.written_entries.load();
    const acks0 = h.cv.stats.acks.load();
    const holds0 = h.cv.stats.holds.load();
    const landings0 = h.cv.stats.hold_landings.load();
    const deadlines0 = h.cv.stats.hold_deadlines.load();
    const not_worth0 = h.cv.stats.holds_not_worth.load();
    const handoffs0 = h.cv.stats.lane_handoffs.load();
    const held0 = h.cv.stats.windows_held.load();
    const unshadowed0 = h.cv.stats.windows_unshadowed.load();
    const pipelined0 = h.cv.stats.windows_pipelined.load();
    // THE MEASUREMENT: per_committer rounds from the first, the span of
    // every window it issues inside [t0, t1].
    const t0 = clock.monotonicNs();
    try C.phase(h, &dma, &inos, 0, per_committer);
    const elapsed = clock.monotonicNs() - t0;

    const entries = st.written_entries.load() - ent0;
    const windows = st.written_windows.load() - win0;
    const pad = st.written_pad_bytes.load() - pad0;
    const device = st.written_device_bytes.load() - dev0;
    const holds = h.cv.stats.holds.load() - holds0;
    const landings = h.cv.stats.hold_landings.load() - landings0;
    const deadlines = h.cv.stats.hold_deadlines.load() - deadlines0;
    const held = h.cv.stats.windows_held.load() - held0;
    const unshadowed = h.cv.stats.windows_unshadowed.load() - unshadowed0;
    const pipelined = h.cv.stats.windows_pipelined.load() - pipelined0;
    // Every Δ landed as an entry (a structural entry — a leaf's compaction
    // under the growing overlay — may ride beside them).
    try testing.expect(entries >= n_committers * per_committer);
    try testing.expectEqual(acks0 + n_committers * per_committer, h.cv.stats.acks.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.refusals.load());
    try testing.expectEqual(@as(u64, 0), h.cv.stats.park_tick_rescues.load());
    // Every handoff is one class; every hold ended by the landing's wake
    // or the deadline; the pinned gate declined nothing.
    try testing.expectEqual(h.cv.stats.lane_handoffs.load() - handoffs0, held + unshadowed + pipelined);
    try testing.expectEqual(windows, held + unshadowed + pipelined);
    try testing.expectEqual(holds, landings + deadlines);
    try testing.expectEqual(not_worth0, h.cv.stats.holds_not_worth.load());
    try testing.expect(holds >= per_committer);

    // THE PACING LAW, under any schedule. A pad-majority window issues
    // only once nothing is in flight (unshadowed) or after a hold the
    // flight's emptying ended — or its deadline, the one way two overlap —
    // and the device lands a window no sooner than `device_ns` after its
    // issue: pad-majority windows are serialized at least a round trip
    // apart, so a run of `elapsed` holds at most `⌊elapsed / D⌋ + 1` of
    // them plus one per deadline. A load that stretches the committers'
    // rounds stretches `elapsed` with them; an unpaced pass (no hold: one
    // window per pass, the red test's 16 per round) breaks it at once.
    const law_windows = elapsed / device_ns + 1 + deadlines;
    // Its byte reading: a pad-majority window fills at most one grain (a
    // page: under half a page of entries, padded); an entry-majority one
    // costs under twice its entries' bytes (`bytesPerOpBound`).
    const s: u64 = 76;
    const geo = h.vol.journal.geo;
    const grain_bytes = geo.page_data_len + jc.PAGE_HDR_LEN;
    const law_device = law_windows * grain_bytes + 2 * s * entries;
    // The paced schedule's reading (the committers re-arrive inside one
    // flight: two alternating cohorts summing to N per pair of windows):
    // two windows per committer round, a third when the re-drain races a
    // cohort's tail, plus the forming/finishing transient — against
    // entries/2 ≈ 2,300 unpaced; pad `1 − N·s/(2·g)` up to the first and
    // last windows (12 pp: the row's tolerance for the forming and
    // finishing cohorts over its 2K windows).
    const paced_windows: u64 = (5 * per_committer) / 2 + 8;
    const paced = elapsed <= paced_windows * device_ns;
    const pair = windowCost(geo, n_committers / 2, s);
    const bound_ppm = pair.padFractionPpm();
    const pad_ppm = (pad * WindowCost.PPM) / device;
    const slack_ppm = 12 * WindowCost.PPM / 100;
    std.debug.print("\n§L window-fill hold: {d} entries in {d} windows over {d} ms = {d} round trips (paced schedule: {}); held {d}, unshadowed {d}, pipelined {d} — law {d} pad-majority windows, {d} B device of law {d} B; pad {d} B = {d}.{d:0>1} % (paced bound {d}.{d:0>1} % + {d} pp); holds {d} (landings {d}, deadlines {d}; gain {d}/1000); hold bound {d} µs\n", .{ entries, windows, elapsed / std.time.ns_per_ms, elapsed / device_ns, paced, held, unshadowed, pipelined, law_windows, device, law_device, pad, pad_ppm / 10_000, (pad_ppm % 10_000) / 1000, bound_ppm / 10_000, (bound_ppm % 10_000) / 1000, slack_ppm / 10_000, holds, landings, deadlines, h.cv.stats.hold_gain_milli.load(), h.cv.stats.hold_bound_ns.load() / 1000 });
    try testing.expect(held + unshadowed <= law_windows);
    try testing.expect(device <= law_device);
    // At the paced schedule the law IS two windows per round: a run that
    // kept the device's pace must not have spent its round trips on more
    // windows (and their pad).
    if (paced) {
        try testing.expect(windows <= paced_windows);
        try testing.expect(pad_ppm <= bound_ppm + slack_ppm);
    }
    // Every Δ landed: the fold's newest size per inode is the last write's.
    for (inos) |ino| try testing.expect(try h.present(ino));
}
