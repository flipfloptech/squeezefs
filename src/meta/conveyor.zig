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
//! §5.2 "both stages on ONE OS thread per writable volume", decision 0002
//! row 6): stage A hands its window to stage B on the SAME thread, so a
//! task wake and a device CQE are one `io_uring_enter` wait. The portable
//! contracts below bind the seam to an inline call.
//!
//! Stage A per drained batch, the survey's 14 steps:
//!   A0 drain (caps: `max(64, cpus × 2)` txs / `max(256 KiB, ring/16)`
//!   bytes clamped to the ring's admissible capacity; NO timers) → A1 the
//!   liveness flags → A2 ONE ring admission for Σ sizes BEFORE any node
//!   lock (a parked committer holds no node lock; the park kicks the
//!   checkpoint task with the park mark and escalates through the
//!   journal-failure lattice past `PARK_ESCALATION_NS`) → A3 resolve every
//!   record's leaf latch-free → A4 lock the UNION ascending by address,
//!   deduped → A5 revalidate every member (any member moved ⇒ drop ALL,
//!   re-resolve ALL) → A6 heap admission (`heap_law.admitHeapLocked`: a
//!   refused member answers `NoSpace`; one retry behind ≤ 2 return-room
//!   cycles when budget can return) → A7 pre-images, first touch per
//!   `(tree, key)` → A8 ONE contiguous reservation inside the window, per-tx
//!   seqs stamped back-to-back → A9 apply in queue order (a member whose
//!   apply fails is rolled OUT in-window and fails alone) → A10 the
//!   writeback threshold → unlock → A11 the threshold kick → A12 SUBMIT the
//!   survivors as ordinary checksummed entries in the one reservation (no
//!   mega-entry) → A13 hand the window to stage B.
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
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../core/counters.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const sync = @import("../sync/sync.zig");
const conveyor_core = @import("conveyor_core.zig");
const tx = @import("tx.zig");
const token_table = @import("sym/tokens/token_table.zig");
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

/// Queue cells: one per ring page (on a padded ring every window occupies
/// at least a page, so the ring's own admission bounds the population);
/// floored for tiny rings, capped where a 32 MiB ring's 8192 pages exceed
/// any admissible committer population. The same law as the lane's inbound.
pub fn queueCapacity(pages: u64) usize {
    return @intCast(derive.clamp(derive.nextPow2(pages), 64, 8192));
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
    /// The queue is at capacity (backpressure; retry).
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
        error.JournalWriteFailed, error.Io, error.Corrupt, error.RetryBudgetExhausted => error.InputOutput,
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
    /// `meta_kv_journal_full_stalls`: ring-admission parks (one per park
    /// entry); steady growth ⇒ the checkpoint lags.
    full_stalls: counters.ShardedCounter = .{},
    /// `meta_conveyor_park_tick_rescues`: parks the 10 ms backstop tick
    /// ended with room already there — a wake nobody sent (≈ 0 healthy).
    park_tick_rescues: counters.ShardedCounter = .{},
    /// RAM ≠ replay: a roll-out, rollback or compensation that could not
    /// complete (allocation, resolution, a lock budget) left RAM holding
    /// what the ring never will (or the reverse). Each fires once per
    /// exit and latches the volume FAILED (`divergedFromReplay`).
    divergence_rollout: counters.Tripwire = .init("conveyor_divergence_rollout"),
    divergence_rollback: counters.Tripwire = .init("conveyor_divergence_rollback"),
    divergence_compensation: counters.Tripwire = .init("conveyor_divergence_compensation"),
    /// `meta_conveyor_rollbacks`: windows rolled back after a failed write;
    /// `meta_conveyor_compensations`: escaped records compensated.
    rollbacks: counters.ShardedCounter = .{},
    compensations: counters.ShardedCounter = .{},
    /// `meta_conveyor_pass_panics` ★: a window abandoned without its
    /// outcome observed (every member failed loud, the reservation
    /// completed as abandoned).
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
    pass_total: counters.ShardedLatencyHistogram = .{},
    window_total: counters.ShardedLatencyHistogram = .{},
    /// `lock_phase_ns.leaf_lock_hold`: first acquire → every guard dropped
    /// (its tail must sit ≪ device latency — the "never across I/O" law).
    leaf_lock_hold: counters.ShardedLatencyHistogram = .{},

    pub fn writeJson(self: *const Stats, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("{{\"meta_commit_group_size\":{{", .{});
        for (GroupSizeHistogram.labels, 0..) |l, i| {
            try w.print("{s}\"{s}\":{d}", .{ if (i == 0) "" else ",", l, self.group_size.load(i) });
        }
        try w.print("}},\"meta_commit_group_bytes\":{d},\"meta_conveyor_leader_passes\":{d},\"meta_conveyor_group_commits\":{d},\"meta_conveyor_group_txs\":{d},\"meta_conveyor_lane_handoffs\":{d},\"meta_conveyor_windows_inflight\":{d},\"meta_conveyor_windows_inflight_hwm\":{d},\"meta_conveyor_queued\":{d},\"meta_commit_parked\":{d},\"meta_conveyor_drained_txs\":{d},\"meta_conveyor_acks\":{d},\"meta_conveyor_refusals\":{d},\"meta_kv_journal_full_stalls\":{d},\"meta_conveyor_park_tick_rescues\":{d},\"meta_conveyor_rollbacks\":{d},\"meta_conveyor_compensations\":{d},\"meta_conveyor_pass_panics\":{d}}}", .{
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
            self.rollbacks.load(),
            self.compensations.load(),
            self.pass_panics.load(),
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
        try w.writeAll(",\"meta_pass_total\":");
        try self.pass_total.snapshot().writeJson(w);
        try w.writeAll(",\"meta_window_total\":");
        try self.window_total.snapshot().writeJson(w);
        try w.writeAll(",\"leaf_lock_hold\":");
        try self.leaf_lock_hold.snapshot().writeJson(w);
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
};

pub var test_seams: TestSeams = .{};

// ---------------------------------------------------------------------------
// Queue entries, windows, the seams.
// ---------------------------------------------------------------------------

pub const Outcome = Error!void;
/// The completion channel per tx: the committer parks on it; the lane
/// sends the terminal outcome after dropping the guards.
pub const Done = sync.Oneshot(Outcome);

/// One queue entry: the tx, its channel, its enqueue instant.
pub const Queued = struct {
    q: QueuedTx,
    done: *Done,
    enqueued_ns: u64,
};

const Core = conveyor_core.ConveyorCore(Queued);

/// A window past its RAM apply, SUBMITTED, in the lane's hands until
/// `finish` (the reference's `ConveyorWindow`). Heap-allocated: the lane
/// and the hooks address it by pointer; its arena owns the members' copies
/// and the undo pre-images.
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
    /// The survivors' aligned runs (the DMA source; alive until `finish`);
    /// null when nothing survived the apply.
    image: ?journal.WriteImage,
    /// Surviving entries and Σ their lengths (the lane's accounting).
    entries: u64,
    bytes: u64,
    t_handoff_ns: u64,
    /// The token union the pass took in flight at A1½ (arena-owned);
    /// settled at the terminal outcome.
    token_union: []const u64 = &.{},

    pub fn span(self: *const Window) SeqSpan {
        return SeqSpan.stamped(self.seq_base, self.res.len);
    }

    /// Whether a member was rolled out at apply (the window has a hole the
    /// survivors' ack must wait a checkpoint past).
    pub fn applyHoles(self: *const Window) bool {
        for (self.failed) |f| if (f != null) return true;
        return false;
    }

    /// The lane's write ops (empty when nothing survived).
    pub fn ops(self: *const Window) []const journal.WriteOp {
        return if (self.image) |*img| img.ops else &.{};
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
    recall: *const fn (ctx: *anyopaque, objects: []const u64) Allocator.Error!void,
    settle: *const fn (ctx: *anyopaque, objects: []const u64) void,
};

/// The pass's venue: `wake` is called by the committer that won the
/// election and must make `runScheduledPass` run — on the journal lane's
/// pass fiber (the backend), inline on the caller (the portable contracts).
/// Never a suspension point for the caller.
pub const PassSeam = struct {
    ctx: *anyopaque,
    wake: *const fn (ctx: *anyopaque) void,
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
};

// ---------------------------------------------------------------------------
// The conveyor.
// ---------------------------------------------------------------------------

pub const Conveyor = struct {
    deps: Deps,
    slots: []Core.Entry,
    core: Core,
    /// The drain buffer: one leader at a time, so one buffer.
    batch_buf: []Queued,
    closing: std.atomic.Value(bool) = .init(false),
    /// Windows between handoff and terminal outcome, by reservation start
    /// (the rollback hook addresses a window by its reservation). Sized
    /// ONCE at init for `inflight_cap` entries: the lane holds at most
    /// `queueCapacity(pages)` windows (its inbound cells follow the same
    /// law) plus the one a pass tracks before the lane refuses it — so
    /// `track` never allocates under the spin lock (charter §1.3).
    inflight_lock: jc.SpinLock = .{},
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
    stats: Stats = .{},

    pub fn init(gpa: Allocator, deps: Deps) Allocator.Error!*Conveyor {
        const self = try gpa.create(Conveyor);
        errdefer gpa.destroy(self);
        const slots = try gpa.alloc(Core.Entry, queueCapacity(deps.geo.pages));
        errdefer gpa.free(slots);
        const batch_buf = try gpa.alloc(Queued, @max(deps.caps.max_txs, 1));
        errdefer gpa.free(batch_buf);
        const inflight_cap = queueCapacity(deps.geo.pages) + 1;
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

    /// Yield until nothing is queued, nobody leads and no window is in
    /// flight (the close path, the quiesce assertions).
    pub fn quiesce(self: *Conveyor) void {
        while (self.core.pending() != 0 or self.core.leader.load(.seq_cst) or self.windowsInflight() != 0) std.Thread.yield() catch std.atomic.spinLoopHint();
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
            entry.q.deinit();
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
            const entry = self.build(t, d) catch |e| {
                r.* = e;
                continue;
            };
            scratch.entries[n] = .{ .item = entry, .len = entry.q.len };
            scratch.tx_of[n] = i;
            n += 1;
        }
        if (n == 0) return;
        _ = self.core.enqueueMany(scratch.entries[0..n]) catch {
            for (scratch.entries[0..n], scratch.tx_of[0..n]) |*e, i| {
                e.item.q.deinit();
                results[i] = error.ConveyorFull;
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
        self.deps.latch.admitMutation() catch {
            t.deinit();
            return error.VolumeFailed;
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
        return .{ .q = q, .done = done, .enqueued_ns = clock.monotonicNs() };
    }

    fn enqueueOne(self: *Conveyor, entry: *Queued, len: u64) Error!void {
        self.core.enqueue(entry.*, len) catch return error.ConveyorFull;
    }

    /// Enqueue + elect — no suspension point between: a committer that
    /// disappears can never strand an entry leaderless. The winner wakes
    /// the pass (ONE wake per election; the pass holds leadership until an
    /// empty drain releases it).
    fn leadIfElected(self: *Conveyor) void {
        if (self.core.tryLead()) self.deps.pass.wake(self.deps.pass.ctx);
    }

    /// The pass venue's body: run the pass iff an election scheduled one
    /// (the leader latch is set). A wake that finds no leader is spurious
    /// (the previous pass drained the entry before the wake landed).
    pub fn runScheduledPass(self: *Conveyor) void {
        if (self.core.leader.load(.seq_cst)) self.pass();
    }

    /// The thread the last pass ran on (0 before any).
    pub fn passThread(self: *const Conveyor) u64 {
        return self.pass_thread.load(.acquire);
    }

    fn park(self: *Conveyor, done: *Done) Error!void {
        self.stats.commit_parked.add(1);
        defer self.stats.commit_parked.sub(1);
        return done.recvUncancelable(self.deps.io) catch error.Corrupt;
    }

    // ---- the pass (stage A) ----

    /// The leader's loop: drain → `runBatch` until an empty drain releases
    /// leadership (release THEN re-check: the no-lost-wakeup theorem). The
    /// caller holds leadership.
    pub fn pass(self: *Conveyor) void {
        self.pass_thread.store(@intCast(std.Thread.getCurrentId()), .release);
        while (true) {
            const batch = self.core.drainBatch(self.deps.caps, self.batch_buf);
            if (batch.len == 0) {
                if (!self.core.unleadAndRecheck()) return;
                continue;
            }
            self.runBatch(batch);
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
        m.done.send(self.deps.io, outcome) catch {};
        m.q.deinit();
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
    fn resolve(self: *Conveyor, arena: Allocator, members: []const Queued) Error!Resolved {
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
                const t = self.treeFor(r.tree_id, r.key) orelse return error.Corrupt;
                const leaf = t.resolveLeaf(r.key) catch |e| return mapTreeError(e);
                leaves[qi][ri] = leaf;
                all[n] = leaf;
                n += 1;
            }
        }
        const lock_set = try arena.dupe(*CachedNode, all);
        std.mem.sort(*CachedNode, lock_set, {}, addrLess);
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

    fn lockAll(self: *Conveyor, lock_set: []*CachedNode, dirties: []*NodeDirty) void {
        for (lock_set, dirties) |n, *d| d.* = n.lockWrite(self.deps.io);
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
            if (self.isClosing() or self.deps.latch.isFailed()) {
                n.cancel(io);
                err.* = if (self.deps.latch.isFailed()) error.VolumeFailed else error.VolumeClosed;
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

    /// Run the batch pipeline (stage A) over one drained batch.
    fn runBatch(self: *Conveyor, batch: []Queued) void {
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
        if (self.isClosing() or self.deps.latch.isFailed()) {
            self.failBatch(members.items, if (self.deps.latch.isFailed()) error.VolumeFailed else error.VolumeClosed);
            return;
        }
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
            for (members.items) |*m| u.addMember(arena, &m.q) catch {
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            token_union = u.finish();
            const t_recall = clock.monotonicNs();
            hook.recall(hook.ctx, token_union) catch {
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

        var cycled_for_space = false;
        var space_retry_started: u64 = 0;
        var hold_active = false;
        defer if (hold_active) self.deps.checkpoint.space_retry.end();
        const t_locks = clock.monotonicNs();
        var attempt: u32 = 0;
        const window: *Window = while (true) : (attempt += 1) {
            if (attempt >= COMMIT_RETRY_BUDGET) {
                log.err("meta volume {s}: commit retry budget exhausted (revalidation never passed — SMO protocol bug)", .{self.deps.volume});
                self.failBatch(members.items, error.RetryBudgetExhausted);
                return;
            }
            // A3.
            var r = self.resolve(arena, members.items) catch |e| {
                self.failBatch(members.items, e);
                return;
            };
            // A4.
            const dirties = arena.alloc(*NodeDirty, r.lock_set.len) catch {
                r.releaseRefs();
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            const t_leaf = clock.monotonicNs();
            self.lockAll(r.lock_set, dirties);
            // A5.
            if (stale(members.items, &r)) {
                self.unlockAll(r.lock_set);
                self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
                r.releaseRefs();
                tree.stats.meta_kv_commit_smo_retries.inc();
                continue;
            }
            // A6.
            var refused: std.ArrayList(heap_law.Refused) = .empty;
            self.admitHeap(arena, members.items, &r, dirties, &refused) catch |e| {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                self.failBatch(members.items, e);
                return;
            };
            if (hold_active) {
                self.deps.checkpoint.space_retry.end();
                hold_active = false;
            }
            if (refused.items.len != 0) {
                self.unlockAll(r.lock_set);
                self.stats.leaf_lock_hold.record(clock.monotonicNs() -| t_leaf);
                r.releaseRefs();
                self.deps.core.release(admission.?);
                admission = null;
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
                                log.warn("meta volume {s}: checkpoint cycle for heap headroom failed (the refused members will be answered on the retry)", .{self.deps.volume});
                                break;
                            }
                            runs += 1;
                            if (!heap_law.continueSpaceRetryCycles(runs, self.deps.alloc.pendingCount())) break;
                        }
                    },
                    .final_refusal => {
                        var i = refused.items.len;
                        while (i > 0) {
                            i -= 1;
                            const rf = refused.items[i];
                            const e = heap_law.finalizeRefusal(self.deps.checkpoint.posture, self.deps.volume, rf.why, self.deps.cache.heapPromised());
                            var m = members.orderedRemove(rf.member);
                            self.answer(&m, e);
                        }
                        if (members.items.len == 0) return;
                    },
                }
                var survivors: u64 = 0;
                for (members.items) |m| survivors += m.q.len;
                admission = self.admitUserBudget(survivors, &err) orelse {
                    self.failBatch(members.items, err);
                    return;
                };
                continue;
            }
            // A7: pre-images, first touch per (tree, key).
            const w = gpa.create(Window) catch {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            w.* = .{ .conveyor = self, .arena = std.heap.ArenaAllocator.init(gpa), .members = &.{}, .failed = &.{}, .res = undefined, .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .bytes = 0, .t_handoff_ns = 0 };
            const wa = w.arena.allocator();
            const undo = self.captureUndo(wa, members.items, &r) catch |e| {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                w.arena.deinit();
                gpa.destroy(w);
                self.failBatch(members.items, e);
                return;
            };
            // The window's own copies, allocated BEFORE anything is reserved
            // or applied: past this point no allocation can fail a batch
            // whose records are already in RAM.
            const window_members = wa.dupe(Queued, members.items) catch {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                w.arena.deinit();
                gpa.destroy(w);
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            const failed = wa.alloc(?Error, members.items.len) catch {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                w.arena.deinit();
                gpa.destroy(w);
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            @memset(failed, null);
            w.token_union = wa.dupe(u64, token_union) catch {
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                w.arena.deinit();
                gpa.destroy(w);
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            // A8: ONE contiguous reservation inside the window.
            const reg = self.deps.reg.reserveRegistered(self.deps.core, admission.?) catch {
                // The core transferred the claim before the registry's
                // append failed (the one way a reservation fails): the range
                // is the ring's, unregistered — complete nothing, fail loud.
                self.unlockAll(r.lock_set);
                r.releaseRefs();
                w.arena.deinit();
                gpa.destroy(w);
                admission = null;
                log.err("meta volume {s}: the batch reservation could not be registered (out of memory) — its range stays an unwritten hole", .{self.deps.volume});
                _ = self.deps.latch.noteFailure(.journal_write);
                self.failBatch(members.items, error.OutOfMemory);
                return;
            };
            admission = null;
            var stamp_cursor = reg.seq_base;
            for (window_members) |*m| {
                _ = m.q.stampSeqs(stamp_cursor);
                stamp_cursor += m.q.len;
            }
            // A9: apply in queue order; a failing member rolls out alone.
            self.applyMembers(window_members, &r, dirties, reg, failed);
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
            w.members = window_members;
            w.failed = failed;
            w.res = reg.res;
            w.seq_base = reg.seq_base;
            w.undo = undo;
            break w;
        };
        // From here every exit finishes the window: the union is its.
        union_settled = true;
        self.stats.pass_leaf_locks.record(clock.monotonicNs() -| t_locks);

        // A12: the survivors' entries, ONE submission, outside every lock.
        var parts: std.ArrayList(journal.BatchPart) = .empty;
        var pos_cursor = window.res.start;
        var survivors: u64 = 0;
        var bytes: u64 = 0;
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
            bytes += m.q.len;
            if (self.deps.door) |d| d.frontier(d.ctx, &m.q.door, pos_cursor);
        }
        window.entries = survivors;
        window.bytes = bytes;
        if (!encode_failed and parts.items.len != 0) {
            window.image = journal.buildBatchWrite(gpa, self.deps.ring_layout, self.deps.geo, self.deps.appender_id, window.res, parts.items) catch null;
            encode_failed = window.image == null;
        }
        window.t_handoff_ns = clock.monotonicNs();
        self.stats.pass_total.record(window.t_handoff_ns -| t_pass);
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
        // A13: handoff.
        self.track(window) catch {
            self.abandonWindow(window, error.OutOfMemory);
            return;
        };
        self.stats.lane_handoffs.inc();
        self.deps.lane.submit(self.deps.lane.ctx, window) catch |e| {
            self.untrack(window);
            self.abandonWindow(window, e);
        };
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
                try undo.append(wa, .{ .tree_id = rec.tree_id, .key = rec.key, .pre = pre });
            }
        }
        return undo.items;
    }

    /// A9: per member in queue order, grouped per locked node; the floor
    /// contribution is the member's ENTRY START (FIND-SMO-TAIL §1b: a leaf
    /// holding only rec[j>0] of a multi-leaf tx must not pin the tail
    /// mid-entry). A member whose apply fails is rolled OUT in-window.
    fn applyMembers(self: *Conveyor, members: []Queued, r: *const Resolved, dirties: []*NodeDirty, reg: jc.InflightRegistry.Registered, failed: []?Error) void {
        const gpa = self.deps.gpa;
        const poison = test_seams.poison_apply_ino.load(.monotonic);
        var pos_cursor = reg.res.start;
        var stamp_cursor = reg.seq_base;
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
        self.inflight_lock.lock();
        defer self.inflight_lock.unlock();
        if (self.inflight.remove(w.res.start)) self.stats.windows_inflight.sub(1);
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
        self.rollbackFailedTx(span, w.undo);
    }

    /// A window the lane never ran (a refused submit, an encode refusal):
    /// the failed-write verdict, synchronously — complete the reservation
    /// as abandoned (the hole completes for the watermark; replay's
    /// checksum walk drops it), roll back, count, checkpoint past the hole,
    /// answer every member.
    fn abandonWindow(self: *Conveyor, w: *Window, err: Error) void {
        self.stats.pass_panics.fire("a conveyor window was abandoned before stage B observed its write");
        self.completeAbandoned(w.res);
        self.rollbackFailedTx(w.span(), w.undo);
        _ = self.deps.latch.noteFailure(.journal_write);
        if (!self.deps.checkpoint.checkpointPast(self.deps.checkpoint.ctx, w.res.paddedEnd())) {
            log.err("meta volume {s}: post-failure checkpoint could not drain the journal hole at {d} (volume escalating)", .{ self.deps.volume, w.res.paddedEnd() });
            _ = self.deps.latch.noteFailure(.journal_write);
        }
        self.finishWindow(w, err);
    }

    /// The terminal outcome of a window: per member in journal order the
    /// in-window apply failure else the lane's verdict; guards drop BEFORE
    /// the ack; the window dies.
    pub fn finishWindow(self: *Conveyor, w: *Window, result: Error!void) void {
        self.untrack(w);
        // The token union leaves the flight with this window — both arms
        // (fan-out and rollback): parked grants wake to the post-commit
        // (or rolled-back) records.
        if (w.token_union.len != 0) if (self.tokens.load(.acquire)) |hook| hook.settle(hook.ctx, w.token_union);
        self.stats.window_total.record(clock.monotonicNs() -| w.t_handoff_ns);
        // The tee sees every member that reached SUCCESS, before the
        // window's arena (the members' key copies) goes.
        if (self.tee.load(.acquire)) |tee| {
            if (result) |_| {
                for (w.members, w.failed) |*m, f| if (f == null) tee.note(tee.ctx, m.q.recs);
            } else |_| {}
        }
        for (w.members, w.failed) |*m, f| {
            const outcome: Outcome = if (f) |e| e else result;
            self.answer(m, outcome);
        }
        if (w.image) |*img| img.deinit();
        w.arena.deinit();
        self.deps.gpa.destroy(w);
    }

    // ---- the seq-conditional rollback (§5.2 "Failure") ----

    const Comp = struct { tree_id: u8, key: []const u8, kind: record.RecordKind, value: []const u8 };

    /// RAM ≠ replay: a roll-out, rollback or compensation exited before it
    /// could make RAM agree with what the ring will replay. Loud, counted
    /// on the site's tripwire, and TERMINAL — the volume latches FAILED
    /// (mutations EIO until remount, where replay rebuilds RAM) rather than
    /// serving a state no recovery can reproduce (charter §1.8).
    fn divergedFromReplay(self: *Conveyor, site: *counters.Tripwire, detail: []const u8) void {
        site.fire(detail);
        if (self.deps.latch.failStop()) {
            log.err("meta volume {s}: RAM diverged from replay ({s}: {s}) — volume marked FAILED; mutations return EIO until remount", .{ self.deps.volume, site.site, detail });
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
            std.mem.sort(*CachedNode, lock_set, {}, addrLess);
            var out: usize = 0;
            for (lock_set) |l| {
                if (out > 0 and lock_set[out - 1].addr == l.addr) continue;
                lock_set[out] = l;
                out += 1;
            }
            const set = lock_set[0..out];
            const dirties = arena.alloc(*NodeDirty, set.len) catch return self.divergedFromReplay(site, "out of memory taking the rollback's locks; the window's records stay applied");
            self.lockAll(set, dirties);
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
            std.mem.sort(*CachedNode, lock_set, {}, addrLess);
            var out: usize = 0;
            for (lock_set) |l| {
                if (out > 0 and lock_set[out - 1].addr == l.addr) continue;
                lock_set[out] = l;
                out += 1;
            }
            const set = lock_set[0..out];
            const dirties = arena.alloc(*NodeDirty, set.len) catch break;
            self.lockAll(set, dirties);
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
            j.commitEntry(reg.res, frames) catch {
                self.divergedFromReplay(site, "the compensation entry's write failed; the escaped record stands for replay while RAM holds its compensation");
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

    pub const Held = struct { w: *Window, landed: bool = false, fail: bool = false };

    pub fn init(gpa: Allocator, io: Io, core: *jc.JournalCore, reg: *jc.InflightRegistry, latch: *journal.FailureLatch, ring_stats: *journal.RingStats, image: *journal.VolatileImage, strict: bool) RamLane {
        return .{ .gpa = gpa, .io = io, .core = core, .reg = reg, .latch = latch, .ring_stats = ring_stats, .image = image, .strict = strict };
    }

    pub fn deinit(self: *RamLane) void {
        assert(self.held.items.len == 0);
        self.held.deinit(self.gpa);
        self.ack_order.deinit(self.gpa);
    }

    pub fn seam(self: *RamLane) LaneSeam {
        return .{ .ctx = self, .submit = submit, .barrier = barrierFn };
    }

    fn submit(ctx: *anyopaque, w: *Window) Error!void {
        const self: *RamLane = @ptrCast(@alignCast(ctx));
        if (self.latch.isFailed()) return error.VolumeFailed;
        var h: Held = .{ .w = w };
        if (self.fail_next > 0) {
            self.fail_next -= 1;
            h.fail = true;
        }
        try self.held.append(self.gpa, h);
        if (!self.hold) {
            self.landAll();
            self.pump();
        }
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
    /// (strict) else `needs_flush`; acks in journal order.
    pub fn pump(self: *RamLane) void {
        while (self.held.items.len != 0 and self.held.items[0].landed) {
            var n: usize = 0;
            while (n < self.held.items.len and self.held.items[n].landed) : (n += 1) {}
            const group = self.gpa.dupe(Held, self.held.items[0..n]) catch return;
            defer self.gpa.free(group);
            self.held.replaceRange(self.gpa, 0, n, &.{}) catch unreachable;
            self.durability_passes += 1;
            var prefix: u64 = 0;
            for (group) |h| {
                if (!h.fail) {
                    self.image.writeOps(h.w.ops()) catch {};
                }
                self.reg.complete(self.core, h.w.res);
                prefix = @max(prefix, h.w.res.paddedEnd());
            }
            self.reg.waitCompletedUpto(prefix);
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
                    self.ring_stats.noteEntriesWritten(h.w.entries, h.w.bytes);
                    self.ring_stats.notePadWritten(h.w.res.pad);
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
                self.ack_order.append(self.gpa, h.w.res.start) catch {};
                h.w.finish(result);
            }
        }
    }

    fn checkpointPast(self: *RamLane, pos: u64) bool {
        const f = self.checkpoint_past orelse return true;
        return f(self.checkpoint_ctx.?, pos);
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

test "the error table is the ONE table: every member maps onto a core member with its errno and class" {
    inline for (@typeInfo(Error).error_set.error_names.?) |name| {
        const err: Error = @field(Error, name);
        _ = errnoOf(err);
        _ = classOf(err);
    }
    try testing.expectEqual(std.os.linux.E.NOSPC, errnoOf(error.NoSpace));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.NoSpace));
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
    hash_seed: u64 = 0x5EED,
    inos: inos_mod.InoWatermark,
    park_kicks: u64 = 0,
    threshold_kicks: u64 = 0,
    checkpoints: u64 = 0,
    last_ledger: ?smo.TestVolume.Ledger = null,
    /// A kick that runs the cycle inline (the park resolves without a timer).
    park_runs_cycle: bool = true,
    fail_checkpoint_past: bool = false,

    const inos_mod = @import("inos.zig");

    fn create(cfg: smo.TestVolume.Config, caps: ?Caps, strict: bool) !*Harness {
        return createWithGpa(cfg, caps, strict, testing.allocator);
    }

    /// `create` with the CONVEYOR's allocator chosen (a `FailingAllocator`
    /// for the OOM contracts); the fixture itself stays on `testing.allocator`.
    fn createWithGpa(cfg: smo.TestVolume.Config, caps: ?Caps, strict: bool, cv_gpa: Allocator) !*Harness {
        const gpa = testing.allocator;
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
    /// (deterministic: a commit returns with its pass run).
    fn passInline(ctx: *anyopaque) void {
        const h: *Harness = @ptrCast(@alignCast(ctx));
        h.cv.runScheduledPass();
    }

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
        _ = try economy.stageCreate(t, .{ .hash_seed = h.hash_seed, .now = 1000, .parent = 1, .parent_value = parent_value, .name = name, .mode = economy.S.IFREG | 0o644, .uid = 0, .gid = 0, .local_ino = ino, .global_ino = ino });
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
        fn recall(ctx: *anyopaque, objects: []const u64) Allocator.Error!void {
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
    try testing.expectError(error.JournalWriteFailed, try da.recvUncancelable(testing.io));
    try try db.recvUncancelable(testing.io);
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
    try testing.expectError(error.JournalWriteFailed, try d.recvUncancelable(testing.io));
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

test "the in-flight window map is sized at init: tracking a full population of windows allocates nothing (never an allocation under inflight_lock)" {
    var fa = std.testing.FailingAllocator.init(testing.allocator, .{});
    const h = try Harness.createWithGpa(.{}, null, false, fa.allocator());
    defer h.destroy();
    // Every allocation from here on fails: a `track` that allocates is red.
    fa.fail_index = fa.alloc_index;
    defer fa.fail_index = std.math.maxInt(usize);
    const n = h.cv.inflight_cap;
    try testing.expectEqual(queueCapacity(h.vol.journal.geo.pages) + 1, n);
    const windows = try testing.allocator.alloc(Window, n);
    defer testing.allocator.free(windows);
    for (windows, 0..) |*w, i| {
        w.* = .{ .conveyor = h.cv, .arena = std.heap.ArenaAllocator.init(testing.allocator), .members = &.{}, .failed = &.{}, .res = Reservation.unpadded(@as(u64, i) * 4096, 100), .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .bytes = 0, .t_handoff_ns = 0 };
        try h.cv.track(w);
    }
    try testing.expectEqual(n, h.cv.inflight.count());
    try testing.expectEqual(@as(u64, n), h.cv.windowsInflight());
    // One past the population is refused loud, not grown under the lock.
    var extra: Window = .{ .conveyor = h.cv, .arena = std.heap.ArenaAllocator.init(testing.allocator), .members = &.{}, .failed = &.{}, .res = Reservation.unpadded(@as(u64, n) * 4096, 100), .seq_base = 0, .undo = &.{}, .image = null, .entries = 0, .bytes = 0, .t_handoff_ns = 0 };
    try testing.expectError(error.OutOfMemory, h.cv.track(&extra));
    for (windows) |*w| h.cv.untrack(w);
    try testing.expectEqual(@as(u64, 0), h.cv.windowsInflight());
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
    const range = tx.dentryChainRange(1, key.dentryNameHash54("acked-not-barriered", h.hash_seed));
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
    try testing.expectEqual(h.cv.stats.lane_handoffs.load(), h.lane.durability_passes);
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
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try s.writeJson(&w);
    const out = w.buffered();
    for ([_][]const u8{ "meta_commit_group_size", "meta_conveyor_leader_passes", "meta_conveyor_windows_inflight_hwm", "meta_conveyor_acks", "meta_kv_journal_full_stalls", "meta_conveyor_park_tick_rescues", "meta_conveyor_pass_panics" }) |name| {
        try testing.expect(std.mem.indexOf(u8, out, name) != null);
    }
}
