//! The journal lane: ONE OS thread per writable metadata volume that owns
//! the volume's journal io_uring and parks IN it (charter §5.2 "Both stages
//! run on ONE OS thread per writable volume (`sqz-jrnl<N>`)", §5.1 "Device
//! posture"; `docs/porting/kv_journal_ledger.md` §3.4–3.5;
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.3, §2.6, §3.5, §7 row 11;
//! `docs/decisions/0001-executor-is-an-io-implementation.md` §2; decision
//! 0002 row 6). Linux only.
//!
//! WHY one thread that owns the ring: on the shared `sqz-meta` pool the
//! completion's cross-thread wake hop measured 746/664 µs of a 1,209/1,130 µs
//! journal-write round trip — 62 % / 59 % (C-2,
//! `.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`); the lane takes it
//! to 53–107 µs. Here a task wake (the eventfd READ the lane keeps armed)
//! and a device CQE are ONE `io_uring_enter` wait, and the thread that reaps
//! the CQE is the thread that runs the durability protocol and acks — no
//! scheduler wake, no run-queue wait behind the serve plane.
//!
//! The thread IS an `exec.Lane` of class `.journal`: its ring is the lane's
//! `ParkedRing` (the eventfd wake through the SeqCst coalescer, the armed
//! READ, the submit-first EXT_ARG park, the dead-ring park), its park is the
//! lane's pass, its timers the lane's heap. Stage B lives on it twice over:
//!
//!   - the lane's `Resident` (the CQE sink): every run's WRITE SQE and the
//!     one FSYNC are tagged in the resident space and routed here at the
//!     lane's reap — a run advances or finishes, a window lands, the barrier
//!     records its outcome — then the stage-B fiber is woken;
//!   - the stage-B FIBER: scans the inbound queue, issues the runs, and runs
//!     the protocol per group: complete every landed window's reservation in
//!     journal order → ONE completed-prefix wait → verdicts (a failed window:
//!     the seq-conditional rollback hook, the failure latch, a checkpoint
//!     past the permanent hole) → hole checkpoints for acked windows with
//!     in-window apply failures → ONE barrier (`FSYNC|DATASYNC` on the same
//!     ring; strict cadence = per group, else `needs_flush` at the flush
//!     interval) → acks in journal order, through the window's `DoneHook`
//!     on this thread (the conveyor's fan-out) or a ticket. Acks only after
//!     the entry landed; strict: after the barrier. Its park is a fiber park
//!     (the lane keeps reaping), bounded by the backstop while a completion
//!     is owed and by the flush due when a coalesced barrier is pending.
//!
//! The conveyor's apply pass is a second fiber on the SAME lane (the backend
//! spawns it): a window it hands off is admitted right here, on this thread,
//! and its SQEs ride the lane's next enter — stage A and stage B never cross
//! a thread. A window from another thread (the tests' submitters) reaches
//! the fiber through the bounded inbound queue and one coalesced wake.
//!
//! Two writer classes share the ONE reservation core: user windows (the
//! conveyor's; stage B's groups, acks and barriers) and STRUCTURAL entries
//! (the SMO task's and the rollback's compensation — `submitStructural`):
//! issued through the same aligned-run path, answered when they LAND (their
//! caller completes the reservation and kicks, as the registry's prefix law
//! requires), never grouped, never barriered here. A lane-thread caller of
//! either class (the pass fiber, the stage-B fiber inside a hook) is served
//! on its own thread: its request is admitted directly and its wait is a
//! fiber park — a queue only the parked fiber scans would be a wait on
//! itself. Every OTHER waiter (a handler fiber's `fsync`, the checkpoint
//! thread's structural entry) parks through the lane's wait route
//! (`routeWaits`, the backend installs it at open): a fiber alone in the
//! executor's futex table, a raw thread on the counted kernel wait —
//! decision 0001's 2026-10-18 amendment, extended here; a bare lane keeps
//! the raw futex on both sides.
//!
//! Zig has no unwinding panics: where the reference panic-guards its lane
//! task (`LaneSentinel`), a panic on this thread aborts the process — fail
//! loud. Every recoverable outcome is an error union answered through the
//! request's cell, and every "cannot happen" is a counted tripwire.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const align_probe = @import("../io/align_probe.zig");
const clock = @import("../core/clock.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const log = @import("../core/log.zig");
const uring_fs = @import("../io/uring_fs.zig");
const completion = @import("../sync/completion.zig");
const futex = @import("../sync/futex.zig");
const mpmc = @import("../sync/mpmc.zig");
const exec_lane = @import("../exec/lane.zig");
const fiber_mod = @import("../exec/fiber.zig");
const group_mod = @import("../exec/group.zig");
const sqz_io = @import("../exec/sqz_io.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");
const checkpoint = @import("checkpoint.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("journal_lane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Reservation = jc.Reservation;
pub const SeqSpan = jc.SeqSpan;
pub const WriteOp = journal.WriteOp;
pub const Lane = exec_lane.Lane;
pub const Fiber = fiber_mod.Fiber;

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// SQ depth of a volume's journal ring: a window's entries (≤ the commit
/// batch cap, each one or two segment SQEs plus its owned page headers on an
/// unpadded ring; one aligned run per contiguous span on a padded one) plus
/// the wake READ and the one barrier; the SQ-full arm (`submit()` then push
/// again) covers a wider chained batch, so the depth bounds a pass, not a
/// window.
pub const ring_entries: u16 = 512;

/// Fibers on a journal lane: the stage-B fiber, the conveyor's pass fiber,
/// and slack for a same-lane caller of `fsync` or a later lane-resident
/// helper. Each carries the journal-class stack (`fiber.stackSizeFor`).
pub const lane_fibers: u32 = 8;

/// The park's backstop while the lane OWES a completion (a run, a prefix, a
/// barrier): a CQE or a kick always arrives first on a healthy lane, so a
/// tick here is a rescue (`journal_lane_park_tick_rescues` ≈ 0) — 100 ms is
/// three orders above the quiet journal round trip (116 µs) and a tenth of
/// the 1 s checkpoint cadence it would otherwise hold back.
pub const park_backstop_ns: u64 = 100 * std.time.ns_per_ms;

/// Resident tags on the lane's ring: the one barrier at the top of the
/// resident space, run slots from its bottom; a slot index past the space
/// is refused at issue (`SubmissionQueueOverflow`), which the SQ depth makes
/// unreachable short of a storm the ring itself could not carry.
const ud_barrier: u64 = exec_lane.resident_ud_max;
const ud_slot_base: u64 = exec_lane.resident_ud_min;
const max_slots: u64 = ud_barrier - ud_slot_base;

/// Inbound cells (= requests the lane can hold): one per ring page — on a
/// padded ring every window occupies at least a page, so the ring's own
/// admission bounds windows in flight by `pages`; floored for tiny test
/// rings, capped where a 32 MiB ring's 8192 pages already exceed any
/// admissible in-flight population. A full queue is a counted retryable
/// refusal, never a wedge.
pub fn inboundCap(pages: u64) usize {
    return @intCast(derive.clamp(derive.nextPow2(pages), 64, 8192));
}

// ---------------------------------------------------------------------------
// The flush cadence knob.
// ---------------------------------------------------------------------------

pub const Cadence = struct {
    /// `interval_ms == 0`: ONE barrier per group before any ack.
    strict: bool,
    interval_ns: u64,
};

pub fn cadenceFrom(interval_ms: u64) Cadence {
    return .{ .strict = interval_ms == 0, .interval_ns = interval_ms * std.time.ns_per_ms };
}

/// The cadence over the checkpointer's ONE flush-interval derivation (the
/// explicit row wins; 0 = strict): the lane and the tick period read the
/// same number from the same place.
pub fn resolveFlushCadence(env: *const std.process.Environ.Map) Cadence {
    return cadenceFrom(checkpoint.flushIntervalMsFromEnv(env));
}

// ---------------------------------------------------------------------------
// Errors: the ONE set (`core/errors.zig`). The lane's own members —
// `VolumeFailed` (the FAILED latch), `LaneStopped`, `LaneQueueFull` (the
// inbound backpressure, `journal_lane_queue_full`), `HoleCheckpointFailed`
// — are rows of that table.
// ---------------------------------------------------------------------------

pub const Error = errors.Error;
pub const ErrorClass = errors.Class;
pub const classOf = errors.classOf;

pub const SpawnError = error{ OutOfMemory, RingSetupFailed, ThreadSpawnFailed, DeviceOpenFailed } || uring_fs.RegisterError;

/// A CQE's `res`: negative is `-errno`.
fn cqeError(res: i32) Error {
    return errors.fromCqeRes(res);
}

// ---------------------------------------------------------------------------
// Observability (charter §10). Process-global counters for the stats face;
// per-lane mirrors for the engagement assertions.
// ---------------------------------------------------------------------------

pub const Stats = struct {
    /// One per writable volume's lane (`journal_lanes_spawned`).
    lanes_spawned: counters.ShardedCounter = .{},
    /// Lanes between spawn and exit (`journal_lanes_live`).
    lanes_live: counters.Gauge = .{},
    /// User windows issued on a lane's own ring (`journal_ring_lane_writes`
    /// — the engagement instrument: ≡ conveyor passes on a lane mount).
    lane_writes: counters.ShardedCounter = .{},
    /// Structural entries (SMO, compensation) issued on the lane
    /// (`journal_lane_structural_writes`).
    structural_writes: counters.ShardedCounter = .{},
    /// Bytes the lane's ring landed on the metadata device
    /// (`journal_lane_write_bytes`, per WRITE CQE): one of the two
    /// device-write arms of the metadata-WRITE ledger (the other is
    /// `meta_io_write_bytes`, the io pool's); their sum is the daemon's
    /// view of the META namespace's diskstats (decision 0003 §L).
    write_bytes: counters.ShardedCounter = .{},
    write_failures: counters.ShardedCounter = .{},
    rollbacks: counters.ShardedCounter = .{},
    barriers_strict: counters.ShardedCounter = .{},
    barriers_coalesced: counters.ShardedCounter = .{},
    barrier_failures: counters.ShardedCounter = .{},
    /// The wake economy of the stage-B fiber: eventfd writes paid by its
    /// wakers vs elided (the delivery word found it scheduled or running,
    /// the coalescer found a write pending, or the waker was on the lane
    /// itself). `writes ÷ (writes + elided)` ≈ 1 under saturation = the
    /// coalescing stopped (the `transport_wake_{writes,elided}` law).
    wake_writes: counters.ShardedCounter = .{},
    wake_elided: counters.ShardedCounter = .{},
    /// Fiber parks COMPLETED (a CQE, a wake or a tick ended them); an idle
    /// lane sits inside one untimed park and adds nothing here.
    parks: counters.ShardedCounter = .{},
    /// Backstop-ended parks whose re-check found PROGRESS with no wake
    /// delivered (a lost kick absorbed by the tick) — must stay 0.
    park_tick_rescues: counters.ShardedCounter = .{},
    /// Backstop-ended parks whose re-check found nothing moved: the owed
    /// completion is still at the device (slow, not lost). Reporting.
    park_backstop_ticks: counters.ShardedCounter = .{},
    prefix_waits: counters.ShardedCounter = .{},
    /// Ticket waits from OFF the lane thread, by what parked: a fiber on
    /// another lane in the executor's futex table (its lane thread ran on —
    /// `journal_lane_wait_fiber_parks`) vs a raw thread on the kernel
    /// futex (`journal_lane_wait_raw_parks`: the checkpoint thread, tests,
    /// a lane without a route). On a serving mount a handler's fsync is a
    /// fiber park; a same-lane fiber's wait is neither (its own park).
    wait_fiber_parks: counters.ShardedCounter = .{},
    wait_raw_parks: counters.ShardedCounter = .{},
    queue_full: counters.ShardedCounter = .{},
    volumes_failed: counters.ShardedCounter = .{},
    /// Rings that died under a lane (`io_uring_enter` / the CQE read
    /// failed): each latches its volume FAILED.
    rings_dead: counters.ShardedCounter = .{},
    cqe_orphan: counters.Tripwire = .init("journal_lane_cqe_orphan"),
    queue_push_with_cell: counters.Tripwire = .init("journal_lane_queue_push_with_cell"),
    /// A user window acked below the previous ack's journal position: the
    /// in-order fan-out is a construction law, so this must stay 0.
    ack_out_of_order: counters.Tripwire = .init("journal_lane_ack_out_of_order"),
};

pub var stats: Stats = .{};

/// `{"journal_lanes_spawned":n,…}`
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"journal_lanes_spawned\":{d},\"journal_lanes_live\":{d},\"journal_ring_lane_writes\":{d},\"journal_lane_structural_writes\":{d},\"journal_lane_write_bytes\":{d},\"journal_lane_write_failures\":{d},\"journal_lane_rollbacks\":{d},\"journal_lane_barriers_strict\":{d},\"journal_lane_barriers_coalesced\":{d},\"journal_lane_barrier_failures\":{d},\"journal_lane_wake_writes\":{d},\"journal_lane_wake_elided\":{d},\"journal_lane_parks\":{d},\"journal_lane_park_tick_rescues\":{d},\"journal_lane_park_backstop_ticks\":{d},\"journal_lane_prefix_waits\":{d},\"journal_lane_wait_fiber_parks\":{d},\"journal_lane_wait_raw_parks\":{d},\"journal_lane_queue_full\":{d},\"journal_lane_volumes_failed\":{d},\"journal_lane_rings_dead\":{d},\"journal_lane_cqe_orphan\":{d},\"journal_lane_ack_out_of_order\":{d}}}", .{
        stats.lanes_spawned.load(),
        stats.lanes_live.load(),
        stats.lane_writes.load(),
        stats.structural_writes.load(),
        stats.write_bytes.load(),
        stats.write_failures.load(),
        stats.rollbacks.load(),
        stats.barriers_strict.load(),
        stats.barriers_coalesced.load(),
        stats.barrier_failures.load(),
        stats.wake_writes.load(),
        stats.wake_elided.load(),
        stats.parks.load(),
        stats.park_tick_rescues.load(),
        stats.park_backstop_ticks.load(),
        stats.prefix_waits.load(),
        stats.wait_fiber_parks.load(),
        stats.wait_raw_parks.load(),
        stats.queue_full.load(),
        stats.volumes_failed.load(),
        stats.rings_dead.load(),
        stats.cqe_orphan.load(),
        stats.ack_out_of_order.load(),
    });
}

/// Per-lane mirrors (the lane thread writes; any thread reads).
pub const LaneSnapshot = struct {
    lane_writes: u64,
    structural_writes: u64,
    /// Write runs pushed to the ring (a short write's re-push included):
    /// one CQE each, so one fiber wake at most each.
    runs: u64,
    parks: u64,
    tick_rescues: u64,
    backstop_ticks: u64,
    wake_writes: u64,
    wake_elided: u64,
    barriers_strict: u64,
    barriers_coalesced: u64,
    barrier_failures: u64,
    prefix_waits: u64,
    acks: u64,
    /// `io_uring_enter` calls the lane's ring paid (`exec.Lane.enters`).
    enters: u64,
};

const LaneStats = struct {
    lane_writes: std.atomic.Value(u64) = .init(0),
    structural_writes: std.atomic.Value(u64) = .init(0),
    runs: std.atomic.Value(u64) = .init(0),
    parks: std.atomic.Value(u64) = .init(0),
    tick_rescues: std.atomic.Value(u64) = .init(0),
    backstop_ticks: std.atomic.Value(u64) = .init(0),
    wake_writes: std.atomic.Value(u64) = .init(0),
    wake_elided: std.atomic.Value(u64) = .init(0),
    barriers_strict: std.atomic.Value(u64) = .init(0),
    barriers_coalesced: std.atomic.Value(u64) = .init(0),
    barrier_failures: std.atomic.Value(u64) = .init(0),
    prefix_waits: std.atomic.Value(u64) = .init(0),
    acks: std.atomic.Value(u64) = .init(0),

    fn bump(v: *std.atomic.Value(u64)) void {
        _ = v.fetchAdd(1, .monotonic);
    }

    fn snapshot(self: *const LaneStats, enters: u64) LaneSnapshot {
        return .{
            .lane_writes = self.lane_writes.load(.monotonic),
            .structural_writes = self.structural_writes.load(.monotonic),
            .runs = self.runs.load(.monotonic),
            .parks = self.parks.load(.monotonic),
            .tick_rescues = self.tick_rescues.load(.monotonic),
            .backstop_ticks = self.backstop_ticks.load(.monotonic),
            .wake_writes = self.wake_writes.load(.monotonic),
            .wake_elided = self.wake_elided.load(.monotonic),
            .barriers_strict = self.barriers_strict.load(.monotonic),
            .barriers_coalesced = self.barriers_coalesced.load(.monotonic),
            .barrier_failures = self.barrier_failures.load(.monotonic),
            .prefix_waits = self.prefix_waits.load(.monotonic),
            .acks = self.acks.load(.monotonic),
            .enters = enters,
        };
    }
};

// ---------------------------------------------------------------------------
// Test seams (one load each when unarmed; `SQUEEZEFS_TEST_*`-class hooks
// that hold the exact window a pin needs — never a sleep).
// ---------------------------------------------------------------------------

pub const TestSeams = struct {
    /// While > 0 each admitted window is failed synthetically (no SQE is
    /// issued, its bytes never land, outcome `fail_errno`'s class —
    /// `InputOutput` when unset) — the torn-run shape of the reference's
    /// `arm_torn_write(keep = 0)`; with `.BADE` it is the fence on an
    /// ENTRY-WRITE CQE (the O_DIRECT conflict site).
    fail_windows: std.atomic.Value(u32) = .init(0),
    /// The errno the two injections above and below carry (`@backingInt`
    /// of a `linux.E`; 0 = EIO).
    fail_errno: std.atomic.Value(u32) = .init(0),
    /// While > 0 each barrier the lane starts is failed synthetically with
    /// `fail_errno`'s class before any FSYNC SQE is issued (the fence on a
    /// BARRIER CQE, `uring_fs::arm_barrier_error(EBADE)`'s shape).
    fail_barriers: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ the stage-B fiber publishes `held = 1` and holds the lane thread
    /// at the top of its next poll (BEFORE its scan), so a test can publish
    /// N messages between two polls deterministically.
    hold_before_scan: std.atomic.Value(u32) = .init(0),
    held: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ the stage-B fiber's next poll kills the ring after its scan, as
    /// a failed `io_uring_enter` would (every in-flight op fails, the
    /// volume latches FAILED); self-disarming.
    kill_ring: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ an admitted window's runs are NOT issued until the seam clears
    /// (the fiber issues them at a later poll): the device-is-slow shape —
    /// a completion owed past the backstop with nothing lost.
    delay_issue: std.atomic.Value(u32) = .init(0),
};

pub var test_seams: TestSeams = .{};

/// The injected failure's class (`fail_errno` through the ONE table).
fn injectedError() Error {
    const raw = test_seams.fail_errno.load(.acquire);
    if (raw == 0) return error.InputOutput;
    return errors.fromErrno(@fromBackingInt(@intCast(@as(u16, @intCast(raw)))));
}

// ---------------------------------------------------------------------------
// Hooks the conveyor installs: all run ON the lane thread (the stage-B
// fiber), synchronously; a wait inside one is a fiber park.
// ---------------------------------------------------------------------------

pub const Hooks = struct {
    ctx: ?*anyopaque = null,
    /// A failed window's seq-conditional rollback (stage B's ONLY leaf-lock
    /// take): `span` is the window's STAMPED record-seq range (the overlay's
    /// domain), `window` its position-domain reservation (the hole).
    rollback: ?*const fn (ctx: ?*anyopaque, span: SeqSpan, window: Reservation) void = null,
    /// Checkpoint past a permanent hole ending at `padded_end` (zero ring
    /// bytes); false = the cycle could not drain (the volume escalates).
    checkpoint_past: ?*const fn (ctx: ?*anyopaque, padded_end: u64) bool = null,
    /// DUR-3: a barrier is about to be issued on this ring — its start
    /// ticket (the checkpointer's `barrierStarted`), taken BEFORE the FSYNC
    /// SQE so a ledger record pushed after the ticket is never drained by it.
    barrier_started: ?*const fn (ctx: ?*anyopaque) u64 = null,
    /// DUR-3: the barrier with `ticket` completed on the device — every
    /// record it covers becomes `reusable_upto` (the checkpointer's
    /// `barrierLanded`): the lane's barriers and the checkpointer's move the
    /// ring, the allocator's coverage clock and the elision tail on one clock.
    barrier_landed: ?*const fn (ctx: ?*anyopaque, ticket: u64) void = null,
};

// ---------------------------------------------------------------------------
// Requests, outcomes, the per-request cell.
// ---------------------------------------------------------------------------

/// One window: the batch's ONE registered reservation and the aligned runs
/// `journal.buildBatchWrite` produced for it. `ops` and their bytes must
/// stay alive until the ticket's outcome is observed (the kernel DMAs from
/// them); the lane completes `window` in the registry — the caller never
/// does.
pub const WriteRequest = struct {
    window: Reservation,
    /// The window's first record stamp (`reserveRegistered`'s `seq_base`).
    seq_base: u64,
    ops: []const WriteOp,
    /// Accounting: surviving entries and the image's device terms
    /// (`journal.WriteImage.acct`; the ledger's ONE site is the verdict).
    entries: u64,
    acct: journal.WriteAccounting,
    /// The window had in-window apply failures (rolled-back members): the
    /// survivors' ack waits for a checkpoint past the window.
    apply_holes: bool = false,
};

pub const Outcome = struct {
    result: Error!void,
    /// Lane-wide ack sequence: strictly increasing in journal order
    /// (0 for a structural entry: answered at landing, not acked).
    ack_seq: u64,
    landed_ns: u64,
    acked_ns: u64,
};

/// A window's terminal-outcome hook (`submitWriteBatchDetached`): runs ON
/// the lane thread at the ack instant, in journal order, instead of a
/// ticket — the conveyor's fan-out (guards drop, then the committers'
/// oneshots), so the thread that reaped the CQE is the thread that acks
/// (no scheduler hop between landing and answer). A wait inside is a fiber
/// park, never a wait on this lane's own tickets.
pub const DoneHook = struct {
    ctx: *anyopaque,
    call: *const fn (ctx: *anyopaque, outcome: Outcome) void,
};

const Kind = enum(u8) {
    /// A user window: grouped, verdicted, barriered, acked in journal order.
    window,
    /// A structural entry: answered when its runs landed.
    structural,
    barrier,
};

/// One request = one cell: the caller fills it, publishes it, parks on
/// `done`; the lane owns it from the admit to the publish of the outcome.
/// The queue push (release) / pop (acquire) orders a foreign caller's fill
/// before the lane's reads; the `done` release orders the lane's outcome
/// before the caller's acquire.
const Cell = struct {
    kind: Kind = .window,
    window: Reservation = .{ .start = 0, .len = 0, .pad = 0 },
    seq_base: u64 = 0,
    ops: []const WriteOp = &.{},
    entries: u64 = 0,
    acct: journal.WriteAccounting = .{},
    apply_holes: bool = false,
    /// Detached window: the hook answers and the lane frees the cell.
    done_hook: ?DoneHook = null,
    /// A ticket waiter that is a fiber on THIS lane (set by `Ticket.wait`
    /// before it parks, read by the lane's deliver — one thread).
    waiter: ?*Fiber = null,
    // ---- lane-owned ----
    runs_in_flight: u32 = 0,
    first_err: ?Error = null,
    landed: bool = false,
    landed_ns: u64 = 0,
    verdict: Error!void = {},
    /// A barrier request: the ticket of the barrier that satisfies it.
    need_barrier: u64 = 0,
    /// The ONE completion cell (`sync/completion.zig`): pending until the
    /// lane publishes the outcome.
    done: completion.Completion(Outcome) = .{},

    fn resetAs(self: *Cell, kind: Kind) void {
        self.kind = kind;
        self.window = .{ .start = 0, .len = 0, .pad = 0 };
        self.seq_base = 0;
        self.ops = &.{};
        self.entries = 0;
        self.acct = .{};
        self.apply_holes = false;
        self.done_hook = null;
        self.waiter = null;
        self.runs_in_flight = 0;
        self.first_err = null;
        self.landed = false;
        self.landed_ns = 0;
        self.verdict = {};
        self.need_barrier = 0;
    }
};

/// The awaitable handle of a submitted request. `wait` / `poll` CONSUME it
/// (the cell returns to the lane's pool) — observe once.
pub const Ticket = struct {
    lane: *JournalLane,
    cell: u32,

    /// Park until the outcome: a fiber on the lane's own thread parks as a
    /// fiber (the lane keeps reaping — its own CQE is what answers it); any
    /// other caller parks through the lane's wait route
    /// (`JournalLane.wait_io`): a fiber on ANOTHER lane alone, in the
    /// executor's futex table (its lane thread runs the fibers homed there
    /// for the whole wait), a raw thread on the counted kernel wait; a
    /// lane without a route parks the caller thread on the cell's raw
    /// futex.
    pub fn wait(self: Ticket) Outcome {
        const c = &self.lane.cells[self.cell];
        const o = if (self.lane.onLaneThread()) blk: {
            const f = exec_lane.currentFiber().?;
            c.waiter = f;
            while (c.done.poll() == null) f.park();
            c.waiter = null;
            break :blk c.done.poll().?;
        } else if (self.lane.wait_io) |io| blk: {
            // The only route ever installed is `SqzIo`'s, whose futex face
            // parks exactly the fibers `currentFiber` names.
            if (exec_lane.currentFiber() != null) stats.wait_fiber_parks.inc() else stats.wait_raw_parks.inc();
            break :blk c.done.waitIo(io);
        } else blk: {
            stats.wait_raw_parks.inc();
            break :blk c.done.wait();
        };
        self.lane.freeCell(self.cell);
        return o;
    }

    /// Non-blocking: null while the lane still owns the cell.
    pub fn poll(self: Ticket) ?Outcome {
        const o = self.lane.cells[self.cell].done.poll() orelse return null;
        self.lane.freeCell(self.cell);
        return o;
    }
};

// ---------------------------------------------------------------------------
// The lane.
// ---------------------------------------------------------------------------

pub const Options = struct {
    /// The volume's metadata device (registered through `registerMetaDevice`
    /// here: the O_DIRECT posture from the first byte).
    path: []const u8,
    /// `sqz-jrnl<index>`.
    index: u16,
    core: *jc.JournalCore,
    registry: *jc.InflightRegistry,
    ring_stats: *journal.RingStats,
    hooks: Hooks = .{},
    /// The log seam is thread-local: error logs the LANE thread is expected
    /// to emit in a test (the FAILED latch, a hole checkpoint that could not
    /// drain, the lane's own dead-ring line). Applied on the lane thread;
    /// `testErrorLogsFired` reads back.
    test_error_logs: if (builtin.is_test) usize else void = if (builtin.is_test) 0 else {},
};

pub const JournalLane = struct {
    gpa: Allocator,
    path: []u8,
    index: u16,
    core: *jc.JournalCore,
    registry: *jc.InflightRegistry,
    ring_stats: *journal.RingStats,
    hooks: Hooks,
    cadence: Cadence,
    mode: uring_fs.MetaIoMode,
    /// The volume's FAILED latch: `JOURNAL_FAILURE_LATCH` consecutive
    /// journal-write (or barrier) failures; readable by every caller.
    latch: journal.FailureLatch = .{},
    /// The device fd (O_RDWR|O_CLOEXEC|O_DIRECT per posture), fixed file 0
    /// on the ring when `fixed_file` (registered on the lane thread).
    fd: linux.fd_t,
    fixed_file: bool = false,
    /// The thread, the ring, the park, the timers.
    lane: *Lane,
    cells: []Cell,
    free_cells: mpmc.BoundedMpmc(u32),
    inbound: mpmc.BoundedMpmc(u32),
    /// Refuses new submitters (SeqCst — Dekker with `inflight_submitters`).
    closing: std.atomic.Value(bool) = .init(false),
    /// Producers inside `publish`. The closer waits for 0 before sealing.
    inflight_submitters: std.atomic.Value(u32) = .init(0),
    /// Set only after every in-flight submitter left: the fiber's
    /// disconnect signal, after which the queue can only drain.
    sealed: std.atomic.Value(bool) = .init(false),
    /// The stage-B fiber and the group that joins it at shutdown.
    fiber: ?*Fiber = null,
    group: Io.Group = .init,
    /// The wait route (`routeWaits`): the executor's futex face every
    /// off-lane ticket waiter parks through and the lane's deliver wakes
    /// through, so a fiber on ANOTHER lane (a handler's fsync, a space
    /// retry's structural write) parks ALONE — its lane thread runs the
    /// other fibers homed there — and a raw thread takes the counted
    /// kernel wait. Null = the raw futex on both sides (a bare lane:
    /// tests, offline tools). Set once, before the first request; both
    /// sides of every cell read the same value (a cell whose waiter and
    /// completer disagreed would lose its wake). A same-lane fiber never
    /// takes it: its wait is its own park, answered by this thread.
    wait_io: ?Io = null,
    /// The lane thread's kernel tid, published at start (0 until then).
    tid: std.atomic.Value(i32) = .init(0),
    stats: LaneStats = .{},
    /// Stage B's state: lane thread only.
    w: Worker,
    test_error_logs: if (builtin.is_test) usize else void,
    test_error_logs_fired: if (builtin.is_test) std.atomic.Value(usize) else void = if (builtin.is_test) .init(0) else {},

    /// Spawn the lane for `opts.path`. The device open, the ring and the
    /// fixed-file registration happen HERE so a box that cannot give the
    /// lane a ring refuses now, not on the first commit.
    pub fn spawn(gpa: Allocator, env: *const std.process.Environ.Map, opts: Options) SpawnError!*JournalLane {
        const mode = try uring_fs.registerMetaDevice(gpa, env, opts.path);
        const self = try gpa.create(JournalLane);
        errdefer gpa.destroy(self);
        const path = try gpa.dupe(u8, opts.path);
        errdefer gpa.free(path);

        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = align_probe.pathZ(&zbuf, opts.path) catch return error.NameTooLong;
        // WHY O_RDWR: the recovery pad's read-modify-write and a future read
        // leg share this fd; a write-only fd makes a ring READ EBADF.
        const fd_rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .DIRECT = mode.isDirect() }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) {
            log.err("journal lane {d}: opening {s} failed ({t}) — the lane cannot start", .{ opts.index, opts.path, linux.errno(fd_rc) });
            return error.DeviceOpenFailed;
        }
        const fd: linux.fd_t = @intCast(fd_rc);
        errdefer _ = linux.close(fd);

        const cap = inboundCap(opts.core.geometry().pages);
        const cells = try gpa.alloc(Cell, cap);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .{};
        var free_cells = try mpmc.BoundedMpmc(u32).init(gpa, cap);
        errdefer free_cells.deinit(gpa);
        var inbound = try mpmc.BoundedMpmc(u32).init(gpa, cap);
        errdefer inbound.deinit(gpa);
        var i: u32 = 0;
        while (i < cap) : (i += 1) std.debug.assert(free_cells.tryPush(i));
        var w = try Worker.init(gpa, cap);
        errdefer w.deinit(gpa);

        const page = std.heap.pageSize();
        const lane = Lane.init(gpa, .{
            .index = opts.index,
            .class = .journal,
            .fiber_capacity = lane_fibers,
            .stack_size = fiber_mod.stackSizeFor(.journal, page),
            .page = page,
            .ring_entries = ring_entries,
            .resident = .{ .ctx = self, .started = residentStarted, .cqe = residentCqe, .ringDead = residentRingDead, .stopping = residentStopping },
        }) catch return error.OutOfMemory;
        errdefer lane.deinit();

        self.* = .{
            .gpa = gpa,
            .path = path,
            .index = opts.index,
            .core = opts.core,
            .registry = opts.registry,
            .ring_stats = opts.ring_stats,
            .hooks = opts.hooks,
            .cadence = resolveFlushCadence(env),
            .mode = mode,
            .fd = fd,
            .lane = lane,
            .cells = cells,
            .free_cells = free_cells,
            .inbound = inbound,
            .w = w,
            .test_error_logs = opts.test_error_logs,
        };
        lane.start() catch |e| {
            log.err("journal lane {d}: the lane could not start ({s}) — the journal lane cannot start (charter §1.2: no classical fallback)", .{ opts.index, @errorName(e) });
            return switch (e) {
                error.RingSetupFailed => error.RingSetupFailed,
                error.ThreadSpawnFailed => error.ThreadSpawnFailed,
            };
        };
        errdefer lane.stopAndJoin();
        // Stage B's fiber: a group member (the lane recycles it at exit; the
        // closer joins the group), a committer (never cancelled).
        group_mod.addMember(&self.group);
        self.fiber = lane.spawn(.group, .{ .group = fiberEntry }, std.mem.asBytes(&self), &self.group, .blocked) catch {
            group_mod.removeMember(&self.group);
            log.err("journal lane {d}: the stage-B fiber could not be spawned", .{opts.index});
            return error.ThreadSpawnFailed;
        };
        stats.lanes_spawned.inc();
        return self;
    }

    /// Stop + join (idempotent): refuse new submitters → wait for in-flight
    /// submitters → seal → wake the fiber → join it (it finishes every
    /// request it holds — acks, the trailing barrier — before it exits) →
    /// stop the lane thread. SYNCHRONOUS: exit cleanup has run on return.
    /// Never aborted from outside: the stop latches and the lane drains.
    pub fn shutdown(self: *JournalLane) void {
        // SeqCst pairs with `publish`'s SeqCst census RMW + closing load:
        // exactly one of {closer, submitter} sees the other.
        self.closing.store(true, .seq_cst);
        while (self.inflight_submitters.load(.seq_cst) != 0) std.Thread.yield() catch {};
        // Release: every push that preceded the seal is visible to the
        // fiber's sealed-then-scan (acquire) poll.
        self.sealed.store(true, .release);
        self.wakeFiber();
        group_mod.awaitGroup(&self.group, exec_lane.currentFiber(), &.{}) catch {};
        self.lane.stopAndJoin();
    }

    pub fn deinit(self: *JournalLane) void {
        self.shutdown();
        self.lane.deinit();
        _ = linux.close(self.fd);
        self.w.deinit(self.gpa);
        self.inbound.deinit(self.gpa);
        self.free_cells.deinit(self.gpa);
        self.gpa.free(self.cells);
        self.gpa.free(self.path);
        self.gpa.destroy(self);
    }

    pub fn isFailed(self: *const JournalLane) bool {
        return self.latch.isFailed();
    }

    pub fn snapshot(self: *const JournalLane) LaneSnapshot {
        return self.stats.snapshot(self.lane.enters());
    }

    /// The lane thread's kernel tid (0 until the thread published it).
    pub fn threadId(self: *const JournalLane) i32 {
        return self.tid.load(.acquire);
    }

    pub fn testErrorLogsFired(self: *const JournalLane) usize {
        return self.test_error_logs_fired.load(.acquire);
    }

    /// The `exec.Lane` beneath: the venue for a fiber that must share this
    /// thread (the conveyor's pass).
    pub fn execLane(self: *JournalLane) *Lane {
        return self.lane;
    }

    /// Whether the caller runs on this lane's thread (a fiber there).
    pub fn onLaneThread(self: *const JournalLane) bool {
        return exec_lane.current_lane == self.lane;
    }

    /// Route every off-lane ticket wait through `io`'s futex face (the
    /// `wait_io` field doc; decision 0001 amendment 2026-10-18, extended to
    /// the journal lane). Legal only before the first request: a request
    /// in flight would have its waiter on one route and its completer on
    /// the other. The `Io` must outlive the lane: deliver wakes through it
    /// until the fiber's exit.
    pub fn routeWaits(self: *JournalLane, io: Io) void {
        std.debug.assert(self.inflight_submitters.load(.seq_cst) == 0);
        std.debug.assert(self.free_cells.len() == self.cells.len);
        self.wait_io = io;
    }

    /// Submit one window on the lane's ring. Refusals answer on THIS thread
    /// before anything is queued: a misaligned run (the posture law; the
    /// tripwire fires here), a FAILED volume, a stopped lane, a full queue.
    /// Everything after that is the ticket's outcome.
    pub fn submitWriteBatch(self: *JournalLane, req: WriteRequest) Error!Ticket {
        return self.submitWindow(req, null);
    }

    /// `submitWriteBatch` whose outcome is DELIVERED through `done` on the
    /// lane thread (journal order, at the ack instant) rather than observed
    /// through a ticket; the cell returns to the pool right after the hook.
    /// A refusal answers here, synchronously, and the hook never runs.
    pub fn submitWriteBatchDetached(self: *JournalLane, req: WriteRequest, done: DoneHook) Error!void {
        _ = try self.submitWindow(req, done);
    }

    fn submitWindow(self: *JournalLane, req: WriteRequest, done: ?DoneHook) Error!Ticket {
        try self.refuseMisaligned(req.ops);
        if (self.latch.isFailed()) return error.VolumeFailed;
        const idx = self.acquireCell() orelse {
            stats.queue_full.inc();
            return error.LaneQueueFull;
        };
        const c = &self.cells[idx];
        c.resetAs(.window);
        c.window = req.window;
        c.seq_base = req.seq_base;
        c.ops = req.ops;
        c.entries = req.entries;
        c.acct = req.acct;
        c.apply_holes = req.apply_holes;
        c.done_hook = done;
        return self.publish(idx);
    }

    /// A structural entry's aligned runs (an SMO entry, the rollback's
    /// compensation): issued like a window, answered when they LANDED. The
    /// caller still completes its reservation and kicks — the registry's
    /// prefix law is its own. Never grouped, never barriered here (an SMO
    /// barriers its images before any pointer record exists; the entry's
    /// own durability rides the next barrier on the device).
    pub fn submitStructural(self: *JournalLane, ops: []const WriteOp) Error!Ticket {
        try self.refuseMisaligned(ops);
        if (self.latch.isFailed()) return error.VolumeFailed;
        const idx = self.acquireCell() orelse {
            stats.queue_full.inc();
            return error.LaneQueueFull;
        };
        const c = &self.cells[idx];
        c.resetAs(.structural);
        c.ops = ops;
        return self.publish(idx);
    }

    /// `fsync`: the trailing coalesced barrier — answered once a barrier
    /// that STARTED after this request completed (one already in flight may
    /// predate a landing it must cover, so the request waits for the next).
    pub fn requestBarrier(self: *JournalLane) Error!Ticket {
        if (self.latch.isFailed()) return error.VolumeFailed;
        const idx = self.acquireCell() orelse {
            stats.queue_full.inc();
            return error.LaneQueueFull;
        };
        self.cells[idx].resetAs(.barrier);
        return self.publish(idx);
    }

    /// Wake the stage-B fiber without a message: the contract of every
    /// out-of-lane completer of this ring (a reservation completed elsewhere
    /// may be the one a lane group's prefix wait sits behind). A no-op once
    /// the lane is closing (its fiber drains and exits on its own).
    pub fn kick(self: *JournalLane) void {
        if (self.closing.load(.acquire)) return;
        self.wakeFiber();
    }

    /// The write law on the caller's thread: a misaligned offset or length
    /// on a direct path is REFUSED before any byte is queued — a caller bug
    /// (the aligned form belongs beside the shape). Pointers are bounced on
    /// the lane (`prepareDirectWrite` at SQE build).
    fn refuseMisaligned(self: *JournalLane, ops: []const WriteOp) Error!void {
        const d = switch (self.mode) {
            .buffered => return,
            .direct => |d| d,
        };
        for (ops) |op| {
            if (op.bytes.len == 0 or align_probe.writeShapeAligned(op.offset, op.bytes.len, d.grain)) continue;
            var buf: [512]u8 = undefined;
            const detail = std.fmt.bufPrint(&buf, "direct journal write at offset {d} of {d} bytes to {s} is not aligned to the device grain ({d} B) — a caller bug: the aligned form belongs beside the shape", .{ op.offset, op.bytes.len, self.path, d.grain }) catch "direct journal write misaligned";
            uring_fs.stats.unaligned_refusals.fire(detail);
            return error.UnalignedDirectWrite;
        }
    }

    fn acquireCell(self: *JournalLane) ?u32 {
        return self.free_cells.tryPop();
    }

    fn freeCell(self: *JournalLane, idx: u32) void {
        self.cells[idx].done.reset();
        // Every cell lives in the ring and every caller takes: a refusal is a
        // taker mid-pop on the wrap cell, waited out (decision 0003 §AI.4).
        if (!self.free_cells.pushSpinUnlessFull(idx)) stats.queue_push_with_cell.fire("a cell met a full free list (a double give-back)");
    }

    /// Publish a filled cell. A caller on the lane thread (the pass fiber,
    /// the stage-B fiber inside a hook) has it admitted at once — the queue
    /// is scanned only by the parked fiber, so a lane-thread caller waiting
    /// on a queued cell would wait on itself; a foreign caller pushes it and
    /// the fiber scans it at its next poll. Either way ONE wake: the
    /// delivery word and the coalescer absorb the rest.
    fn publish(self: *JournalLane, idx: u32) Error!Ticket {
        // SeqCst RMW: pairs with `shutdown`'s SeqCst `closing` store +
        // `inflight_submitters` load so exactly one of us sees the other.
        _ = self.inflight_submitters.fetchAdd(1, .seq_cst);
        defer _ = self.inflight_submitters.fetchSub(1, .release);
        if (self.closing.load(.seq_cst)) {
            self.freeCell(idx);
            return error.LaneStopped;
        }
        if (self.onLaneThread()) {
            self.admit(idx);
        } else if (!self.inbound.tryPush(idx)) {
            // The cell pool and the queue share one bound: a held cell always
            // has a queue slot.
            stats.queue_push_with_cell.fire("inbound queue refused a push while a cell was held");
            self.freeCell(idx);
            return error.LaneQueueFull;
        }
        self.wakeFiber();
        return .{ .lane = self, .cell = idx };
    }

    /// The wake, modelled (`exec/task_state_model.zig` for the delivery
    /// word, `exec/wake_core_model.zig` for the coalescer):
    ///
    ///     producer:  publish (queue push, release)  →  word.wake()
    ///                → IDLE→SCHEDULED: enqueue — same lane: the ready FIFO;
    ///                  foreign: inbox push → coalescer arm → eventfd write
    ///                  iff arm returned true
    ///                → RUNNING→NOTIFIED / already SCHEDULED: nothing
    ///     lane:      wake CQE consumed → disarm → scan the inbox → the
    ///                fiber polls → scans the queue
    ///
    /// A wake that lands while the fiber runs is requeued by the runner
    /// (law 1); the coalescer's SeqCst pair orders a producer that elided
    /// behind a write still pending. N publishes between two polls cost at
    /// most ONE eventfd write: `journal_lane_wake_{writes,elided}`.
    fn wakeFiber(self: *JournalLane) void {
        const f = self.fiber orelse return;
        if (!f.state.wake()) {
            stats.wake_elided.inc();
            LaneStats.bump(&self.stats.wake_elided);
            return;
        }
        switch (self.lane.enqueueReporting(f)) {
            .written => {
                stats.wake_writes.inc();
                LaneStats.bump(&self.stats.wake_writes);
            },
            .elided, .same_lane => {
                stats.wake_elided.inc();
                LaneStats.bump(&self.stats.wake_elided);
            },
        }
    }

    // ---- the resident: the lane thread's edges and the CQE sink ----

    fn residentStarted(ctx: *anyopaque) void {
        const self: *JournalLane = @ptrCast(@alignCast(ctx));
        if (builtin.is_test) log.testing.expectErrorLogs(self.test_error_logs);
        // Fixed FILE: every SQE names index 0 with IOSQE_FIXED_FILE (one fd
        // lookup/refcount per op saved). A refusal degrades to the raw fd.
        self.fixed_file = self.lane.ring.registerFile(self.fd);
        if (!self.fixed_file) log.debug("journal lane {d}: register_files refused; using the raw fd", .{self.index});
        // Release pairs with `threadId`'s acquire: the lane is up when the tid reads nonzero.
        self.tid.store(linux.gettid(), .release);
        stats.lanes_live.add(1);
    }

    fn residentStopping(ctx: *anyopaque) void {
        const self: *JournalLane = @ptrCast(@alignCast(ctx));
        // Post-seal stragglers (none on the shutdown protocol; answered
        // rather than leaked if the ordering is ever broken).
        while (self.inbound.tryPop()) |idx| self.answerUnrun(idx, error.LaneStopped);
        stats.lanes_live.sub(1);
        if (builtin.is_test) self.test_error_logs_fired.store(log.testing.capturedErrorLogs(), .release);
    }

    fn residentRingDead(ctx: *anyopaque) void {
        const self: *JournalLane = @ptrCast(@alignCast(ctx));
        self.failAll(error.RingFailed);
        self.wakeFiber();
    }

    /// The sink: one reaped CQE under a resident tag. Runs on the lane
    /// thread between fiber polls; it records (a run's progress, a window's
    /// landing, the barrier's outcome), re-pushes a short write, and wakes
    /// the fiber — the protocol itself (groups, hooks, acks) is the fiber's.
    fn residentCqe(ctx: *anyopaque, cqe: linux.io_uring_cqe) void {
        const self: *JournalLane = @ptrCast(@alignCast(ctx));
        const w = &self.w;
        const now = clock.monotonicNs();
        if (cqe.user_data == ud_barrier) {
            if (w.barrier_in_flight != null and !w.barrier_landed) {
                w.barrier_landed = true;
                w.barrier_outcome = if (cqe.res < 0) cqeError(cqe.res) else {};
            } else stats.cqe_orphan.fire("a barrier CQE with no barrier in flight");
            self.wakeFiber();
            return;
        }
        const slot_ud = cqe.user_data - ud_slot_base;
        if (slot_ud >= w.slots.items.len or w.slots.items[@intCast(slot_ud)] == null) {
            stats.cqe_orphan.fire("CQE named a slot with no in-flight run");
            return;
        }
        const i: u32 = @intCast(slot_ud);
        if (self.advanceRun(i, cqe.res, now)) {
            if (!self.pushRun(i)) self.finishRun(i, error.SubmissionQueueOverflow, now);
        }
        self.wakeFiber();
    }

    /// Answer a cell the lane will never run: the outcome fires with the
    /// error; nothing was issued.
    fn answerUnrun(self: *JournalLane, idx: u32, err: Error) void {
        const now = clock.monotonicNs();
        self.deliver(&self.cells[idx], .{ .result = err, .ack_seq = 0, .landed_ns = now, .acked_ns = now });
    }

    /// Publish a cell's outcome: through its hook (then the cell is free),
    /// or through the completion cell — the wake takes the route the
    /// waiter took (both read `wait_io`, set before the first request); a
    /// same-lane fiber waiter is woken here (one thread: its registration
    /// preceded its park).
    fn deliver(self: *JournalLane, c: *Cell, outcome: Outcome) void {
        if (c.done_hook) |h| {
            h.call(h.ctx, outcome);
            self.freeCell(@intCast((@intFromPtr(c) - @intFromPtr(self.cells.ptr)) / @sizeOf(Cell)));
            return;
        }
        // A ticket has no abandon face (`wait`/`poll` consume it), so the
        // waiter always owns the cell after this publish.
        _ = if (self.wait_io) |io| c.done.completeIo(io, outcome) else c.done.complete(outcome);
        if (c.waiter) |f| if (f.state.wake()) self.lane.enqueue(f);
    }

    // ---- admission (lane thread) ----

    fn admit(self: *JournalLane, idx: u32) void {
        const w = &self.w;
        const c = &self.cells[idx];
        switch (c.kind) {
            .barrier => {
                c.need_barrier = w.barriers_started + 1;
                w.fsync_waiters.appendAssumeCapacity(idx);
                w.needs_flush = true;
                w.flush_due_ns = 0;
            },
            .window => {
                stats.lane_writes.inc();
                LaneStats.bump(&self.stats.lane_writes);
                self.issueWindow(idx);
                self.insertPending(idx);
            },
            .structural => {
                stats.structural_writes.inc();
                LaneStats.bump(&self.stats.structural_writes);
                self.issueWindow(idx);
            },
        }
    }

    /// Issue the request's runs as WRITE SQEs. Nothing is issued for a
    /// synthetically failed window (the torn-run seam) or on a dead ring;
    /// a run that cannot get an SQE after a flush fails alone and the window
    /// lands when its siblings do.
    fn issueWindow(self: *JournalLane, idx: u32) void {
        const w = &self.w;
        const c = &self.cells[idx];
        if (test_seams.delay_issue.load(.acquire) != 0) {
            // Capacity equals the cell count, which bounds the delayed set.
            w.delayed_issue.appendAssumeCapacity(idx);
            return;
        }
        if (c.kind == .window and test_seams.fail_windows.load(.acquire) != 0) {
            _ = test_seams.fail_windows.fetchSub(1, .acq_rel);
            c.first_err = injectedError();
            self.markLanded(c, clock.monotonicNs());
            return;
        }
        if (w.ring_dead) {
            c.first_err = error.RingFailed;
            self.markLanded(c, clock.monotonicNs());
            return;
        }
        for (c.ops) |op| {
            if (op.bytes.len == 0) continue;
            // The shape was refused on the caller; here only a misaligned
            // POINTER can bounce (counted in `meta_io_bounce_bytes`).
            const prepared = uring_fs.prepareDirectWrite(self.gpa, self.mode, self.path, op.offset, op.bytes) catch |e| {
                noteRunError(c, @errorCast(e));
                continue;
            };
            const slot = w.claimSlot(self.gpa, .{ .cell = idx, .data = prepared.data, .offset = op.offset, .written = 0, .bounce = prepared.bounce }) catch |e| {
                var p = prepared;
                p.deinit();
                noteRunError(c, e);
                continue;
            };
            if (!self.pushRun(slot)) {
                var released = w.releaseSlot(self.gpa, slot);
                if (released.bounce) |*b| b.deinit();
                noteRunError(c, error.SubmissionQueueOverflow);
                continue;
            }
            c.runs_in_flight += 1;
            w.runs_in_flight += 1;
        }
        if (c.runs_in_flight == 0) self.markLanded(c, clock.monotonicNs());
    }

    /// Windows are handed off in reservation order by the single conveyor
    /// pass; a multi-submitter storm may not, so the list is kept sorted
    /// (an in-order arrival is an O(1) append).
    fn insertPending(self: *JournalLane, idx: u32) void {
        const w = &self.w;
        const start = self.cells[idx].window.start;
        var at = w.pending.items.len;
        while (at > 0 and self.cells[w.pending.items[at - 1]].window.start > start) at -= 1;
        // Capacity equals the cell count, which bounds the pending set.
        w.pending.insertAssumeCapacity(at, idx);
    }

    /// A request's runs all finished: a user window waits for its group; a
    /// structural entry is answered now.
    fn markLanded(self: *JournalLane, c: *Cell, now: u64) void {
        c.landed = true;
        c.landed_ns = now;
        if (c.kind == .structural) {
            const result: Error!void = if (c.first_err) |e| e else {};
            self.deliver(c, .{ .result = result, .ack_seq = 0, .landed_ns = now, .acked_ns = now });
        }
    }

    // ---- SQEs (lane thread) ----

    fn sqeFd(self: *const JournalLane) linux.fd_t {
        return if (self.fixed_file) 0 else self.fd;
    }

    /// Fill the WRITE SQE for slot `i` at its progress. `user_data` = the
    /// slot's resident tag.
    fn pushRun(self: *JournalLane, i: u32) bool {
        const sqe = self.lane.ring.getSqe() orelse return false;
        const r = &self.w.slots.items[i].?;
        sqe.prep_write(self.sqeFd(), r.data[r.written..], r.offset + r.written);
        if (self.fixed_file) sqe.flags |= linux.IOSQE_FIXED_FILE;
        sqe.user_data = ud_slot_base + i;
        LaneStats.bump(&self.stats.runs);
        return true;
    }

    /// Apply one run's CQE. True ⇒ a short write continues and must be
    /// re-pushed; false ⇒ the run is finished (its request may have landed).
    fn advanceRun(self: *JournalLane, i: u32, res: i32, now: u64) bool {
        const r = &self.w.slots.items[i].?;
        if (res > 0) {
            stats.write_bytes.add(@intCast(res));
            r.written += @intCast(res);
            if (r.written < r.data.len) return true;
            self.finishRun(i, {}, now);
            return false;
        }
        self.finishRun(i, if (res < 0) cqeError(res) else error.ShortWrite, now);
        return false;
    }

    fn finishRun(self: *JournalLane, i: u32, result: Error!void, now: u64) void {
        const w = &self.w;
        var released = w.releaseSlot(self.gpa, i);
        if (released.bounce) |*b| b.deinit();
        const c = &self.cells[released.cell];
        result catch |e| noteRunError(c, e);
        c.runs_in_flight -= 1;
        w.runs_in_flight -= 1;
        if (c.runs_in_flight == 0) self.markLanded(c, now);
    }

    /// The ring broke: every in-flight run and the barrier fail with `err`;
    /// later requests fail at admission; and the VOLUME latches FAILED at
    /// once — a ring that cannot `io_uring_enter` is a device fence, which
    /// charter §1.8 names terminal (three later window verdicts would leave
    /// a quiet volume "dead lane, not FAILED"). Idempotent: the seam and
    /// the lane's own verdict may both arrive. A run prepared but not yet
    /// submitted never reaches the kernel (a dead ring's flush submits
    /// nothing); a run the kernel already took may still land, so its
    /// RingFailed is an indeterminate EIO and replay may apply the entry.
    /// Its late CQE never reaches `residentCqe`: the lane drops it (0003
    /// §AR.2; a WRITE only reads its buffer, so releasing it now is safe).
    fn failAll(self: *JournalLane, err: Error) void {
        const w = &self.w;
        if (w.ring_dead) return;
        w.ring_dead = true;
        self.lane.ring.markDead();
        stats.rings_dead.inc();
        if (self.latch.failStop()) {
            stats.volumes_failed.inc();
            log.err("journal lane {d}: the ring is dead ({s}) — latching the volume FAILED; mutations answer EIO until remount (charter §1.8: a device fence is terminal)", .{ self.index, @errorName(err) });
        }
        const now = clock.monotonicNs();
        for (w.slots.items, 0..) |slot, i| {
            if (slot == null) continue;
            var released = w.releaseSlot(self.gpa, @intCast(i));
            if (released.bounce) |*b| b.deinit();
            const c = &self.cells[released.cell];
            noteRunError(c, err);
            c.runs_in_flight -= 1;
            w.runs_in_flight -= 1;
            if (c.runs_in_flight == 0) self.markLanded(c, now);
        }
        if (w.barrier_in_flight != null and !w.barrier_landed) {
            w.barrier_landed = true;
            w.barrier_outcome = err;
        }
    }

    // ---- the stage-B fiber ----

    fn fiberEntry(context: *const anyopaque) void {
        const p: *const *JournalLane = @ptrCast(@alignCast(context));
        p.*.fiberMain();
    }

    /// One poll per iteration: the hold seam → scan (sealed read BEFORE
    /// the pump) → the landed barrier → stage B → the coalesced barrier
    /// when due → the exit edge → ONE fiber park (timed by the backstop
    /// while a completion is owed and by the flush due) → the park verdict.
    ///
    /// The verdict of a park the BACKSTOP ended (`Fiber.timerFired`: the
    /// timer's pop was the wake — a CQE or a kick reaped in the same lane
    /// pass before the pop already scheduled the fiber and leaves it false)
    /// is judged by the NEXT poll's re-check, the channel-class contract of
    /// `sync/tick.zig`: progress found with no wake delivered is a RESCUE
    /// (`journal_lane_park_tick_rescues`, must stay 0 — an out-of-lane
    /// completer that forgot to `kick`); no progress is a `backstop_tick`
    /// (the owed completion is still at the device — a 4 KiB journal write
    /// or an FSYNC past 100 ms on a saturated substrate is slow, not lost).
    /// The pre-fix count-at-the-tick read 6–186 "rescues" per saturated
    /// write leg on the venue's one shared virtio disk (rung 9 F6).
    fn fiberMain(self: *JournalLane) void {
        const f = exec_lane.currentFiber().?;
        const w = &self.w;
        var judge_tick = false;
        while (true) {
            holdIfArmed();
            const before = w.progressKey();
            // Acquire pairs with `shutdown`'s release: every push ordered
            // before the seal is visible to this scan.
            const sealed = self.sealed.load(.acquire);
            while (self.inbound.tryPop()) |idx| self.admit(idx);
            if (sealed) w.disconnected = true;
            if (test_seams.kill_ring.swap(0, .acq_rel) != 0) self.failAll(error.RingFailed);
            var now = clock.monotonicNs();
            if (w.barrier_landed) self.barrierDone(w.barrier_outcome, now);
            self.advanceDurability();
            if (judge_tick) {
                judge_tick = false;
                if (!std.meta.eql(before, w.progressKey())) {
                    stats.park_tick_rescues.inc();
                    LaneStats.bump(&self.stats.tick_rescues);
                } else {
                    stats.park_backstop_ticks.inc();
                    LaneStats.bump(&self.stats.backstop_ticks);
                }
            }
            // After the verdict: the seam's deferred issue is the test's
            // device coming back, not progress a wake owed.
            if (test_seams.delay_issue.load(.acquire) == 0) while (w.delayed_issue.pop()) |idx| self.issueWindow(idx);
            now = clock.monotonicNs();
            // The trailing barrier before exit: ONE, after the last ack, so
            // every acked entry leaves durable.
            if (w.disconnected and w.needs_flush and !w.owesCompletion()) w.flush_due_ns = now;
            self.maybeStartCoalescedBarrier(now);
            if (w.disconnected and !w.owesWork()) return;
            now = clock.monotonicNs();
            const tick = w.tickFor(now);
            if (tick) |t| self.lane.timers.arm(f, now + t);
            f.park();
            const fired = tick != null and f.timerFired();
            if (tick != null and !fired) _ = self.lane.timers.cancel(f);
            stats.parks.inc();
            LaneStats.bump(&self.stats.parks);
            // The flush-due deadline is a cadence, never a backstop.
            judge_tick = fired and !(w.needs_flush and clock.monotonicNs() >= w.flush_due_ns);
        }
    }

    // ---- stage B: the durability protocol over the registry ----

    /// Drive the one group in progress as far as it can go without a park.
    fn advanceDurability(self: *JournalLane) void {
        const w = &self.w;
        while (true) {
            switch (w.phase) {
                .idle => if (!self.formGroup()) return,
                .prefix => {
                    self.foldLatePredecessors();
                    if (w.prefix_token) |t| {
                        if (!self.registry.completion_notify.firedSince(t)) return;
                        w.prefix_token = null;
                    }
                    switch (self.registry.pollCompletedUpto(w.prefix_target)) {
                        .ready => self.runVerdicts(),
                        .pending => |token| {
                            w.prefix_token = token;
                            stats.prefix_waits.inc();
                            LaneStats.bump(&self.stats.prefix_waits);
                            return;
                        },
                    }
                },
                .want_barrier => {
                    if (w.barrier_in_flight != null) return;
                    // The phase BEFORE the issue: a barrier that fails at
                    // issue (dead ring, no SQE) lands synchronously and
                    // moves the phase on to `.ack` itself.
                    w.phase = .barrier;
                    self.startBarrier(.strict_group);
                },
                .barrier => return,
                .ack => self.fanOut(),
            }
        }
    }

    /// The group = the head window once landed plus every successor that
    /// already landed; each is completed in the registry IN JOURNAL ORDER
    /// (failed ones too: the hole completes for watermark purposes), so
    /// `completed_upto` walks forward through the lane alone.
    fn formGroup(self: *JournalLane) bool {
        const w = &self.w;
        while (w.pending.items.len != 0) {
            const idx = w.pending.items[0];
            const c = &self.cells[idx];
            if (!c.landed) break;
            _ = w.pending.orderedRemove(0);
            self.registry.complete(self.core, c.window);
            w.group.appendAssumeCapacity(idx);
        }
        if (w.group.items.len == 0) return false;
        w.prefix_target = self.cells[w.group.items[w.group.items.len - 1]].window.paddedEnd();
        w.prefix_token = null;
        w.phase = .prefix;
        return true;
    }

    /// A window reserved before the group's but handed off after it holds
    /// the group's prefix: once landed it is completed and joins the group
    /// at its journal position (acks stay in journal order); un-landed, the
    /// prefix wait simply continues until its CQE.
    fn foldLatePredecessors(self: *JournalLane) void {
        const w = &self.w;
        while (w.pending.items.len != 0) {
            const idx = w.pending.items[0];
            const c = &self.cells[idx];
            if (c.window.start >= w.prefix_target or !c.landed) break;
            _ = w.pending.orderedRemove(0);
            self.registry.complete(self.core, c.window);
            var at: usize = 0;
            while (at < w.group.items.len and self.cells[w.group.items[at]].window.start < c.window.start) at += 1;
            w.group.insertAssumeCapacity(at, idx);
        }
    }

    /// Per window in journal order: Ok ⇒ the write-failure run resets, the
    /// ring accounting, an apply-hole note; Err ⇒ the rollback hook, the
    /// failure latch, a checkpoint past the permanent hole. Then the hole
    /// checkpoint for acked windows (BEFORE any survivor is acked), then the
    /// barrier decision. The hooks may park this fiber.
    fn runVerdicts(self: *JournalLane) void {
        const w = &self.w;
        var any_ok = false;
        var hole_end: ?u64 = null;
        var k: usize = 0;
        while (k < w.group.items.len) : (k += 1) {
            const c = &self.cells[w.group.items[k]];
            if (c.first_err) |e| {
                log.warn("journal lane {d}: window [{d}, {d}) write failed: {s} — rolling back {d} entries", .{ self.index, c.window.start, c.window.end(), @errorName(e), c.entries });
                stats.write_failures.inc();
                if (self.hooks.rollback) |f| {
                    f(self.hooks.ctx, SeqSpan.stamped(c.seq_base, c.window.len), c.window);
                    stats.rollbacks.inc();
                }
                // The fence rung FIRST: a reservation conflict on an
                // entry-write CQE latches FAILED at once; the generic
                // 3-count below is then a no-op on a latched volume.
                if (self.latch.classifyFence(e, "an entry-write CQE") == .latched) stats.volumes_failed.inc();
                self.noteJournalFailure();
                // The reserved range is a PERMANENT hole (same-page successors
                // of a dead chain are undiscoverable): checkpoint past it.
                if (!self.checkpointPast(c.window.paddedEnd())) {
                    log.err("journal lane {d}: post-failure checkpoint could not drain the journal hole at {d} (volume escalating)", .{ self.index, c.window.paddedEnd() });
                    self.noteJournalFailure();
                }
                c.verdict = e;
            } else {
                self.latch.noteSuccess(.journal_write);
                self.ring_stats.noteWritten(c.acct, c.entries);
                if (c.apply_holes) hole_end = @max(hole_end orelse 0, c.window.paddedEnd());
                any_ok = true;
                c.verdict = {};
            }
        }
        w.group_hole_err = null;
        if (hole_end) |end| {
            if (!self.checkpointPast(end)) {
                log.err("journal lane {d}: post-isolation checkpoint could not cover the batch hole at {d} (volume escalating; failing the survivors loud rather than acking unreachable entries)", .{ self.index, end });
                self.noteJournalFailure();
                w.group_hole_err = error.HoleCheckpointFailed;
            }
        }
        w.group_barrier_out = {};
        if (w.group_hole_err == null and any_ok and self.cadence.strict) {
            w.phase = .want_barrier;
            return;
        }
        if (w.group_hole_err == null and any_ok) self.setNeedsFlush(clock.monotonicNs());
        w.phase = .ack;
    }

    fn checkpointPast(self: *JournalLane, padded_end: u64) bool {
        const f = self.hooks.checkpoint_past orelse return true;
        return f(self.hooks.ctx, padded_end);
    }

    fn noteJournalFailure(self: *JournalLane) void {
        if (self.latch.noteFailure(.journal_write)) stats.volumes_failed.inc();
    }

    /// Acks in journal order: a member's own write error, else the hole
    /// checkpoint's, else the group barrier's.
    fn fanOut(self: *JournalLane) void {
        const w = &self.w;
        const now = clock.monotonicNs();
        for (w.group.items) |idx| {
            const c = &self.cells[idx];
            const result: Error!void = if (c.verdict) |_| (if (w.group_hole_err) |h| h else w.group_barrier_out) else |e| e;
            if (c.window.start < w.last_acked_start) stats.ack_out_of_order.fire("a window acked below the previous ack's journal position");
            w.last_acked_start = c.window.start;
            self.publishAck(c, result, now);
        }
        w.group.clearRetainingCapacity();
        w.phase = .idle;
    }

    fn publishAck(self: *JournalLane, c: *Cell, result: Error!void, now: u64) void {
        const w = &self.w;
        w.ack_seq += 1;
        LaneStats.bump(&self.stats.acks);
        self.deliver(c, .{ .result = result, .ack_seq = w.ack_seq, .landed_ns = c.landed_ns, .acked_ns = now });
    }

    // ---- barriers ----

    /// Coalesced cadence: the first ack after a barrier schedules the next
    /// at `last + interval` (never sooner than now); an fsync request pulls
    /// it to now.
    fn setNeedsFlush(self: *JournalLane, now: u64) void {
        const w = &self.w;
        if (w.needs_flush) return;
        w.needs_flush = true;
        w.flush_due_ns = @max(now, w.last_barrier_ns + self.cadence.interval_ns);
    }

    fn maybeStartCoalescedBarrier(self: *JournalLane, now: u64) void {
        const w = &self.w;
        if (!w.needs_flush or w.barrier_in_flight != null or now < w.flush_due_ns) return;
        self.startBarrier(.coalesced);
    }

    /// ONE `FSYNC|DATASYNC` SQE on the lane's ring. Never `fsync(2)`:
    /// O_DIRECT bypasses the page cache, not the device's volatile write
    /// cache, and a blocking syscall here would stall every CQE behind it.
    fn startBarrier(self: *JournalLane, kind: BarrierKind) void {
        const w = &self.w;
        w.barriers_started += 1;
        w.needs_flush = false;
        w.barrier_in_flight = kind;
        w.barrier_landed = false;
        // The DUR-3 ticket BEFORE the device op (a wasted ticket on an issue
        // failure only makes later pushes wait for a later barrier).
        w.barrier_ticket = if (self.hooks.barrier_started) |f| f(self.hooks.ctx) else 0;
        switch (kind) {
            .strict_group => {
                stats.barriers_strict.inc();
                LaneStats.bump(&self.stats.barriers_strict);
            },
            .coalesced => {
                stats.barriers_coalesced.inc();
                LaneStats.bump(&self.stats.barriers_coalesced);
            },
        }
        if (w.ring_dead) return self.barrierDone(error.RingFailed, clock.monotonicNs());
        if (test_seams.fail_barriers.load(.acquire) != 0) {
            _ = test_seams.fail_barriers.fetchSub(1, .acq_rel);
            return self.barrierDone(injectedError(), clock.monotonicNs());
        }
        if (self.lane.ring.prepFdatasync(self.sqeFd(), self.fixed_file, ud_barrier) == null) self.barrierDone(error.SubmissionQueueOverflow, clock.monotonicNs());
    }

    /// The barrier's outcome: the latch's barrier class, DUR-3 coverage (so
    /// an fsync caller returns with `reusable_upto` advanced), the strict
    /// group's ack gate, and every fsync request whose ticket it satisfies.
    fn barrierDone(self: *JournalLane, result: Error!void, now: u64) void {
        const w = &self.w;
        const kind = w.barrier_in_flight.?;
        w.barrier_in_flight = null;
        w.barrier_landed = false;
        w.barriers_done += 1;
        w.last_barrier_ns = now;
        if (result) |_| {
            self.latch.noteSuccess(.barrier);
            if (self.hooks.barrier_landed) |f| f(self.hooks.ctx, w.barrier_ticket);
        } else |e| {
            log.warn("journal lane {d}: barrier failed: {s}", .{ self.index, @errorName(e) });
            stats.barrier_failures.inc();
            LaneStats.bump(&self.stats.barrier_failures);
            // The fence rung first (charter §5.7: a reservation-conflict
            // errno at ANY barrier latches FAILED), then the 3-count.
            if (self.latch.classifyFence(e, "a durability barrier") == .latched) stats.volumes_failed.inc();
            if (self.latch.noteFailure(.barrier)) stats.volumes_failed.inc();
        }
        if (kind == .strict_group) {
            w.group_barrier_out = result;
            w.phase = .ack;
        }
        var i: usize = 0;
        while (i < w.fsync_waiters.items.len) {
            const idx = w.fsync_waiters.items[i];
            const c = &self.cells[idx];
            if (c.need_barrier > w.barriers_done) {
                i += 1;
                continue;
            }
            _ = w.fsync_waiters.swapRemove(i);
            self.publishAck(c, result, now);
        }
    }
};

/// One in-flight run (one WRITE SQE): owns the bounce it may carry.
const Run = struct {
    cell: u32,
    data: []const u8,
    offset: u64,
    written: usize,
    bounce: ?uring_fs.AlignedBuf,
};

const Phase = enum { idle, prefix, want_barrier, barrier, ack };
const BarrierKind = enum { strict_group, coalesced };

/// Stage B's state: slot table, the pending windows in journal order, the
/// one group in progress, the barrier slot. Lane thread only (the sink
/// between polls, the fiber within them).
const Worker = struct {
    slots: std.ArrayList(?Run),
    free_slots: std.ArrayList(u32),
    runs_in_flight: usize = 0,
    /// Admitted windows not yet in a group, ascending by `window.start`.
    pending: std.ArrayList(u32),
    /// The stage-B group: completed in the registry, not yet acked; ascending.
    group: std.ArrayList(u32),
    phase: Phase = .idle,
    prefix_target: u64 = 0,
    prefix_token: ?jc.Notify.Token = null,
    group_hole_err: ?Error = null,
    group_barrier_out: Error!void = {},
    barrier_in_flight: ?BarrierKind = null,
    /// The sink recorded the in-flight barrier's CQE; the fiber folds it.
    barrier_landed: bool = false,
    barrier_outcome: Error!void = {},
    /// The in-flight barrier's DUR-3 ticket (`Hooks.barrier_started`).
    barrier_ticket: u64 = 0,
    /// Barrier tickets: a request is satisfied by a barrier STARTED after it.
    barriers_started: u64 = 0,
    barriers_done: u64 = 0,
    needs_flush: bool = false,
    flush_due_ns: u64 = 0,
    /// The coalesced cadence's origin: the last barrier's completion, or the
    /// lane's start (so the first period is measured from spawn, never from
    /// the monotonic clock's zero).
    last_barrier_ns: u64,
    /// Barrier-request cells awaiting their ticket.
    fsync_waiters: std.ArrayList(u32),
    /// Cells admitted under the `delay_issue` seam, issued when it clears.
    delayed_issue: std.ArrayList(u32),
    disconnected: bool = false,
    ring_dead: bool = false,
    ack_seq: u64 = 0,
    /// The last acked user window's start: the in-order tripwire's witness.
    last_acked_start: u64 = 0,

    fn init(gpa: Allocator, cells: usize) Allocator.Error!Worker {
        // One ring's worth of runs plus a resubmit burst never reallocates
        // the slot table; a wider window grows it (amortised).
        var slots = try std.ArrayList(?Run).initCapacity(gpa, 2 * ring_entries);
        errdefer slots.deinit(gpa);
        var free_slots = try std.ArrayList(u32).initCapacity(gpa, 2 * ring_entries);
        errdefer free_slots.deinit(gpa);
        var pending = try std.ArrayList(u32).initCapacity(gpa, cells);
        errdefer pending.deinit(gpa);
        var group = try std.ArrayList(u32).initCapacity(gpa, cells);
        errdefer group.deinit(gpa);
        var fsync_waiters = try std.ArrayList(u32).initCapacity(gpa, cells);
        errdefer fsync_waiters.deinit(gpa);
        const delayed_issue = try std.ArrayList(u32).initCapacity(gpa, cells);
        return .{ .slots = slots, .free_slots = free_slots, .pending = pending, .group = group, .fsync_waiters = fsync_waiters, .delayed_issue = delayed_issue, .last_barrier_ns = clock.monotonicNs() };
    }

    fn deinit(w: *Worker, gpa: Allocator) void {
        w.slots.deinit(gpa);
        w.free_slots.deinit(gpa);
        w.pending.deinit(gpa);
        w.group.deinit(gpa);
        w.fsync_waiters.deinit(gpa);
        w.delayed_issue.deinit(gpa);
    }

    fn owesWork(w: *const Worker) bool {
        return w.owesCompletion() or w.needs_flush or w.fsync_waiters.items.len != 0;
    }

    /// Everything a poll's scan + stage B can move: unequal keys across a
    /// backstop-ended park mean the tick found work a wake should have
    /// announced (`fiberMain`'s rescue verdict).
    const ProgressKey = struct {
        phase: Phase,
        runs_in_flight: usize,
        pending: usize,
        group: usize,
        barrier_in_flight: bool,
        barrier_landed: bool,
        fsync_waiters: usize,
        ack_seq: u64,
        barriers_done: u64,
        disconnected: bool,
    };

    fn progressKey(w: *const Worker) ProgressKey {
        return .{
            .phase = w.phase,
            .runs_in_flight = w.runs_in_flight,
            .pending = w.pending.items.len,
            .group = w.group.items.len,
            .barrier_in_flight = w.barrier_in_flight != null,
            .barrier_landed = w.barrier_landed,
            .fsync_waiters = w.fsync_waiters.items.len,
            .ack_seq = w.ack_seq,
            .barriers_done = w.barriers_done,
            .disconnected = w.disconnected,
        };
    }

    /// A run, a window behind a group, a group, or a barrier is outstanding:
    /// the park carries the backstop.
    fn owesCompletion(w: *const Worker) bool {
        return w.runs_in_flight != 0 or w.pending.items.len != 0 or w.group.items.len != 0 or w.barrier_in_flight != null;
    }

    /// The fiber park's tick: the backstop while a completion is owed, the
    /// flush due when a coalesced barrier is pending, else none (the lane's
    /// own park is bounded by its 2 s liveness tick alone).
    fn tickFor(w: *const Worker, now: u64) ?u64 {
        var tick: ?u64 = if (w.owesCompletion()) park_backstop_ns else null;
        if (w.needs_flush and w.barrier_in_flight == null) {
            const until = w.flush_due_ns -| now;
            tick = if (tick) |t| @min(t, until) else until;
        }
        return tick;
    }

    fn claimSlot(w: *Worker, gpa: Allocator, r: Run) Error!u32 {
        if (w.free_slots.pop()) |i| {
            w.slots.items[i] = r;
            return i;
        }
        if (w.slots.items.len >= max_slots) return error.SubmissionQueueOverflow;
        w.slots.append(gpa, r) catch return error.OutOfMemory;
        return @intCast(w.slots.items.len - 1);
    }

    fn releaseSlot(w: *Worker, gpa: Allocator, i: u32) Run {
        const r = w.slots.items[i].?;
        w.slots.items[i] = null;
        w.free_slots.append(gpa, i) catch {
            // Losing a free-list entry costs a slot of table growth later,
            // never correctness.
        };
        return r;
    }
};

fn noteRunError(c: *Cell, err: Error) void {
    if (c.first_err == null) c.first_err = err;
}

/// The hold seam: at the top of the fiber's poll so a test can place N
/// publishes between two polls.
fn holdIfArmed() void {
    if (test_seams.hold_before_scan.load(.acquire) == 0) return;
    test_seams.held.store(1, .release);
    futex.wakeAll(&test_seams.held);
    while (test_seams.hold_before_scan.load(.acquire) != 0) futex.wait(&test_seams.hold_before_scan, 1);
}

// ---------------------------------------------------------------------------
// Tests (Linux; the metadata device is a regular file on the venue's ext4
// at /build/tmp, O_DIRECT through `registerMetaDevice`).
// ---------------------------------------------------------------------------

const testing = std.testing;

const Harness = struct {
    gpa: Allocator,
    path_buf: [256]u8 = undefined,
    path_len: usize = 0,
    layout: journal.RingLayout,
    geo: journal.CoreGeometry,
    core: jc.JournalCore,
    reg: jc.InflightRegistry,
    ring_stats: journal.RingStats = .{},
    env: std.process.Environ.Map,
    lane: ?*JournalLane = null,
    images: std.ArrayList(journal.WriteImage) = .empty,

    const pages: u64 = 256;
    const base: u64 = journal.JOURNAL_START;

    fn path(self: *const Harness) []const u8 {
        return self.path_buf[0..self.path_len];
    }

    /// A zero-filled `base + pages × 4 KiB` file, registered as a metadata
    /// device (the grain comes from the registration), plus a fresh core
    /// and registry over it. Heap-allocated: the lane keeps pointers in.
    fn init(gpa: Allocator, name: []const u8) !*Harness {
        const self = try gpa.create(Harness);
        errdefer gpa.destroy(self);
        self.* = .{ .gpa = gpa, .layout = undefined, .geo = undefined, .core = undefined, .reg = undefined, .env = std.process.Environ.Map.init(gpa) };
        errdefer self.env.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        _ = linux.mkdirat(linux.AT.FDCWD, try align_probe.pathZ(&zbuf, "/build/tmp"), 0o755);
        self.path_len = (try std.fmt.bufPrint(&self.path_buf, "/build/tmp/sqz-jrnl-{d}-{s}.img", .{ linux.getpid(), name })).len;
        const z = try align_probe.pathZ(&zbuf, self.path());
        const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true, .TRUNC = true }, 0o644);
        if (linux.errno(rc) != .SUCCESS) return error.CannotCreateDevice;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var zeros: [65536]u8 = @splat(0);
        var left: usize = @intCast(base + pages * journal.PAGE_LEN);
        while (left > 0) {
            const n = @min(left, zeros.len);
            const wrote = linux.write(fd, &zeros, n);
            if (linux.errno(wrote) != .SUCCESS) return error.CannotFillDevice;
            left -= wrote;
        }
        const mode = try uring_fs.registerMetaDevice(gpa, &self.env, self.path());
        self.layout = .{ .base = base, .pages = pages };
        self.geo = journal.productionGeometry(pages, mode.grain());
        self.core = jc.JournalCore.init(self.geo, 0, 0);
        self.reg = try jc.InflightRegistry.init(gpa, 0, 64);
        return self;
    }

    fn spawnLane(self: *Harness, hooks: Hooks, flush_ms: ?[]const u8, test_error_logs: usize) !void {
        if (flush_ms) |v| try self.env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", v);
        self.lane = try JournalLane.spawn(self.gpa, &self.env, .{ .path = self.path(), .index = 7, .core = &self.core, .registry = &self.reg, .ring_stats = &self.ring_stats, .hooks = hooks, .test_error_logs = test_error_logs });
    }

    fn deinit(self: *Harness) void {
        if (self.lane) |l| l.deinit();
        for (self.images.items) |*img| img.deinit();
        self.images.deinit(self.gpa);
        self.reg.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        if (align_probe.pathZ(&zbuf, self.path())) |z| {
            _ = linux.unlink(z);
        } else |_| {}
        self.env.deinit();
        self.gpa.destroy(self);
    }

    /// Admit + reserve one entry of `entry_len` whole bytes.
    fn reserve(self: *Harness, entry_len: u64) !jc.InflightRegistry.Registered {
        const adm = self.core.tryAdmit(entry_len, .user) orelse return error.RingFull;
        return self.reg.reserveRegistered(&self.core, adm);
    }

    /// Build the window's aligned runs for ONE Put on inode key `ino` whose
    /// value pads the entry to `entry_len`; the image lives until `deinit`
    /// (the DMA source must outlive the ack).
    fn build(self: *Harness, r: jc.InflightRegistry.Registered, ino: u64, entry_len: u64, marker: u8) ![]const WriteOp {
        var key: [9]u8 = undefined;
        std.mem.writeInt(u64, key[0..8], ino, .big);
        key[8] = journal.TREE_INODES;
        const value_len: usize = @intCast(entry_len - journal.ENTRY_HDR_LEN - 1 - journal.RECORD_HEADER_LEN - key.len);
        const value = try self.gpa.alloc(u8, value_len);
        defer self.gpa.free(value);
        @memset(value, marker);
        const recs = [_]journal.RecordFrame{.{ .tag = journal.tagFor(journal.TREE_INODES, 0), .key = &key, .kind = .put, .seq = r.seq_base, .value = value }};
        const img = try journal.buildEntryWrite(self.gpa, self.layout, self.geo, 0, r.res, &recs);
        try self.images.append(self.gpa, img);
        return img.ops;
    }

    /// The harness's one-entry window accounting over its aligned runs
    /// (`build` is a solo `buildEntryWrite`: no holes; the headers are the
    /// runs' remainder).
    fn acctOf(res: Reservation, ops: []const WriteOp) journal.WriteAccounting {
        var run: u64 = 0;
        for (ops) |op| run += op.bytes.len;
        return .{ .entry_bytes = res.len, .pad_bytes = res.pad, .header_bytes = run -| (res.len + res.pad), .hole_bytes = 0, .run_bytes = run };
    }

    fn submit(self: *Harness, r: jc.InflightRegistry.Registered, ops: []const WriteOp) Error!Ticket {
        return self.lane.?.submitWriteBatch(.{ .window = r.res, .seq_base = r.seq_base, .ops = ops, .entries = 1, .acct = acctOf(r.res, ops) });
    }

    fn append(self: *Harness, ino: u64, entry_len: u64, marker: u8) !struct { res: Reservation, ticket: Ticket } {
        const r = try self.reserve(entry_len);
        const ops = try self.build(r, ino, entry_len, marker);
        return .{ .res = r.res, .ticket = try self.submit(r, ops) };
    }

    /// The ring's image, read O_DIRECT (the device's truth, never the page
    /// cache).
    fn readImage(self: *Harness) !uring_fs.AlignedBuf {
        var buf = try uring_fs.AlignedBuf.zeroed(self.gpa, @intCast(pages * journal.PAGE_LEN), 4096);
        errdefer buf.deinit();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = try align_probe.pathZ(&zbuf, self.path());
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .CLOEXEC = true, .DIRECT = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.CannotOpenDevice;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var filled: usize = 0;
        while (filled < buf.bytes.len) {
            const n = linux.pread(fd, buf.bytes[filled..].ptr, buf.bytes.len - filled, @intCast(base + filled));
            if (linux.errno(n) != .SUCCESS) return error.CannotReadDevice;
            if (n == 0) break;
            filled += n;
        }
        return buf;
    }

    fn replay(self: *Harness, tail: u64) !journal.JournalRecovery {
        var img = try self.readImage();
        defer img.deinit();
        return journal.replayScanImage(self.gpa, img.bytes, self.geo, tail, 0);
    }
};

/// The recovered inode numbers in replay order (every test entry is one Put).
fn recoveredInos(rec: *const journal.JournalRecovery, out: []u64) []u64 {
    var n: usize = 0;
    for (rec.entries) |e| {
        var it = e.records();
        while (it.next()) |r| : (n += 1) out[n] = std.mem.readInt(u64, r.key[0..8], .big);
    }
    return out[0..n];
}

/// `/proc/self/task/<tid>/comm`, trimmed.
fn commOf(tid: i32, buf: *[32]u8) ![]const u8 {
    var pbuf: [64]u8 = undefined;
    const p = try std.fmt.bufPrintSentinel(&pbuf, "/proc/self/task/{d}/comm", .{tid}, 0);
    const rc = linux.open(p, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.NoSuchTask;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const n = linux.read(fd, buf, buf.len);
    if (linux.errno(n) != .SUCCESS) return error.CannotReadComm;
    return std.mem.trimEnd(u8, buf[0..n], "\n");
}

/// A whole-page entry: `res.len + pad == 4072` at every grain, so every
/// window owns its page header and replay's resync can find its successors.
const page_entry: u64 = journal.PAGE_DATA_LEN;

fn spinUntil(cond: anytype) void {
    while (!cond.ready()) std.Thread.yield() catch std.atomic.spinLoopHint();
}

/// `spinUntil` bounded by a monotonic deadline: false when it passed with
/// the condition still unmet (the caller fails loud with its counters).
fn spinUntilBy(cond: anytype, deadline_ns: u64) bool {
    while (!cond.ready()) {
        if (clock.monotonicNs() >= deadline_ns) return false;
        std.Thread.yield() catch std.atomic.spinLoopHint();
    }
    return true;
}

test "the flush cadence: the checkpointer's derived default (50 ms) coalesced; 0 is strict; junk keeps the default loudly" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const d = resolveFlushCadence(&env);
    try testing.expect(!d.strict);
    try testing.expectEqual(checkpoint.DEFAULT_FLUSH_INTERVAL_MS * std.time.ns_per_ms, d.interval_ns);
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), d.interval_ns);
    try env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    try testing.expect(resolveFlushCadence(&env).strict);
    try env.put("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "junk");
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), resolveFlushCadence(&env).interval_ns);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(usize, 64), inboundCap(4));
    try testing.expectEqual(@as(usize, 256), inboundCap(256));
    try testing.expectEqual(@as(usize, 8192), inboundCap(8192));
    try testing.expectEqual(@as(usize, 8192), inboundCap(100_000));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.VolumeFailed));
    try testing.expectEqual(ErrorClass.retryable, classOf(error.LaneQueueFull));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.HoleCheckpointFailed));
    try testing.expectEqual(ErrorClass.terminal, classOf(error.InputOutput));
    try testing.expectEqual(ErrorClass.refusal, classOf(error.UnalignedDirectWrite));
    // The resident tag space: the barrier at its top, 4093 run slots below.
    try testing.expect(ud_barrier < 4096 and ud_slot_base >= 2 and max_slots > ring_entries);
}

test "the lane thread is sqz-jrnl<N> in /proc, an exec.Lane of the journal class, counted live, and joins owing nothing" {
    const h = try Harness.init(testing.allocator, "name");
    defer h.deinit();
    const live_before = stats.lanes_live.load();
    const spawned_before = stats.lanes_spawned.load();
    try h.spawnLane(.{}, null, 0);
    const lane = h.lane.?;
    try testing.expect(lane.threadId() != 0);
    var cbuf: [32]u8 = undefined;
    try testing.expectEqualStrings("sqz-jrnl7", try commOf(lane.threadId(), &cbuf));
    try testing.expectEqualStrings("sqz-jrnl7", lane.execLane().name);
    try testing.expectEqual(exec_lane.LaneClass.journal, lane.execLane().class);
    try testing.expectEqual(live_before + 1, stats.lanes_live.load());
    try testing.expectEqual(spawned_before + 1, stats.lanes_spawned.load());
    try testing.expect(lane.mode.isDirect());
    try testing.expect(!lane.onLaneThread());
    // An idle lane owes nothing, so its fiber's ONE park is untimed: three
    // backstop periods later it is still inside it (no park completed, no
    // tick).
    const idle: linux.timespec = .{ .sec = 0, .nsec = @intCast(3 * park_backstop_ns) };
    _ = linux.nanosleep(&idle, null);
    try testing.expectEqual(@as(u64, 0), lane.snapshot().parks);
    try testing.expectEqual(@as(u64, 0), lane.snapshot().tick_rescues);
    lane.shutdown();
    try testing.expectEqual(live_before, stats.lanes_live.load());
    // Every op after the stop answers the stopped class.
    try testing.expectError(error.LaneStopped, lane.requestBarrier());
    try testing.expectEqual(@as(u64, 0), lane.snapshot().tick_rescues);
}

test "N entries through the lane replay back present and in order; acks never precede landing; no tick rescues" {
    const h = try Harness.init(testing.allocator, "replay");
    defer h.deinit();
    // The coalesced barrier parked at the knob's ceiling: nothing but the
    // test's own requests barriers.
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    const writes_before = stats.lane_writes.load();
    const n: u64 = 16;
    var i: u64 = 0;
    var last_ack: u64 = 0;
    while (i < n) : (i += 1) {
        const a = try h.append(100 + i, 300 + i * 7, @truncate(i));
        const o = a.ticket.wait();
        try o.result;
        // The ack came after the entry landed: the registry's prefix covers it.
        try testing.expect(h.reg.completedUpto() >= a.res.paddedEnd());
        try testing.expect(o.acked_ns >= o.landed_ns);
        try testing.expect(o.ack_seq > last_ack);
        last_ack = o.ack_seq;
    }
    try testing.expectEqual(writes_before + n, stats.lane_writes.load());
    try testing.expectEqual(n, lane.snapshot().lane_writes);
    try testing.expectEqual(@as(usize, 0), h.reg.openCount());
    try testing.expectEqual(h.core.headPos(), h.reg.completedUpto());
    try testing.expectEqual(n, h.ring_stats.written_entries.load());

    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [32]u64 = undefined;
    const got = recoveredInos(&rec, &inos);
    try testing.expectEqual(@as(usize, n), got.len);
    for (got, 0..) |ino, k| try testing.expectEqual(100 + @as(u64, k), ino);
    for (rec.entries[1..], 1..) |e, k| try testing.expect(e.seq > rec.entries[k - 1].seq);
    try testing.expectEqual(h.core.headPos(), rec.head_pos);
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    const snap = lane.snapshot();
    try testing.expectEqual(@as(u64, 0), snap.tick_rescues);
    try testing.expectEqual(@as(u64, 0), snap.barriers_strict);
    try testing.expectEqual(@as(u64, 0), stats.ack_out_of_order.load());
    std.debug.print("\nmeasurement: coalesced, {d} sequential windows: fiber parks {d} ({d} per window), io_uring_enter {d} ({d} per window), wake writes {d}, elided {d}\n", .{ n, snap.parks, snap.parks / n, snap.enters, snap.enters / n, snap.wake_writes, snap.wake_elided });
}

test "strict cadence: ONE barrier per group before any ack; the k-th ack sees k barriers done" {
    const h = try Harness.init(testing.allocator, "strict");
    defer h.deinit();
    try h.spawnLane(.{}, "0", 0);
    const lane = h.lane.?;
    const strict_before = stats.barriers_strict.load();
    var k: u64 = 1;
    while (k <= 5) : (k += 1) {
        const a = try h.append(k, 500, 0x5A);
        try a.ticket.wait().result;
        // Sequential windows form one group each: a barrier per window,
        // complete BEFORE the ack.
        try testing.expectEqual(k, lane.snapshot().barriers_strict);
        try testing.expectEqual(@as(u64, 0), lane.snapshot().barriers_coalesced);
    }
    try testing.expectEqual(strict_before + 5, stats.barriers_strict.load());
    // An fsync on the strict cadence is the trailing barrier: one more.
    const t = try lane.requestBarrier();
    try t.wait().result;
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_coalesced);
    try testing.expectEqual(@as(u64, 0), lane.snapshot().barrier_failures);
    const snap = lane.snapshot();
    std.debug.print("\nmeasurement: strict, 5 sequential windows: fiber parks {d} ({d} per window incl. the barrier), wake writes {d}, elided {d}\n", .{ snap.parks, snap.parks / 5, snap.wake_writes, snap.wake_elided });
}

test "coalesced cadence: acks precede the barrier; an fsync request is answered by the trailing barrier; a short interval barriers on its own" {
    const h = try Harness.init(testing.allocator, "coalesced");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    var k: u64 = 0;
    while (k < 4) : (k += 1) {
        const a = try h.append(10 + k, 400, 0x33);
        try a.ticket.wait().result;
        try testing.expectEqual(@as(u64, 0), lane.snapshot().barriers_strict);
        try testing.expectEqual(@as(u64, 0), lane.snapshot().barriers_coalesced);
    }
    const t = try lane.requestBarrier();
    try t.wait().result;
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_coalesced);
    // Nothing acked since: a second request still pays its own trailing barrier.
    const t2 = try lane.requestBarrier();
    try t2.wait().result;
    try testing.expectEqual(@as(u64, 2), lane.snapshot().barriers_coalesced);
    lane.shutdown();
    // The lane's exit owed no barrier (needs_flush was consumed by the request).
    try testing.expectEqual(@as(u64, 2), lane.snapshot().barriers_coalesced);

    // A 1 ms interval: the fiber's own flush timer barriers after an ack
    // with no request.
    const h2 = try Harness.init(testing.allocator, "coalesced-tick");
    defer h2.deinit();
    try h2.spawnLane(.{}, "1", 0);
    const a = try h2.append(1, 400, 0x44);
    try a.ticket.wait().result;
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.snapshot().barriers_coalesced >= 1;
        }
    }{ .l = h2.lane.? });
    try testing.expectEqual(@as(u64, 0), h2.lane.?.snapshot().tick_rescues);
}

/// The DUR-3 faces as a ticket counter: `started` hands out 1, 2, …;
/// `landed` records the tickets handed back.
const Dur3Log = struct {
    started: std.atomic.Value(u64) = .init(0),
    landed: std.atomic.Value(u64) = .init(0),
    last_landed: std.atomic.Value(u64) = .init(0),
    /// A ticket landed that was never started, or out of order.
    bad: std.atomic.Value(u64) = .init(0),

    fn barrierStarted(ctx: ?*anyopaque) u64 {
        const self: *Dur3Log = @ptrCast(@alignCast(ctx.?));
        return self.started.fetchAdd(1, .acq_rel) + 1;
    }

    fn barrierLanded(ctx: ?*anyopaque, ticket: u64) void {
        const self: *Dur3Log = @ptrCast(@alignCast(ctx.?));
        if (ticket == 0 or ticket > self.started.load(.acquire) or ticket <= self.last_landed.load(.acquire)) _ = self.bad.fetchAdd(1, .acq_rel);
        self.last_landed.store(ticket, .release);
        _ = self.landed.fetchAdd(1, .acq_rel);
    }

    fn hooks(self: *Dur3Log) Hooks {
        return .{ .ctx = self, .barrier_started = barrierStarted, .barrier_landed = barrierLanded };
    }
};

test "DUR-3 hooks: every lane barrier (strict group, coalesced, fsync) takes a start ticket before the device op and lands the SAME ticket, in order, before its requester is answered" {
    const h = try Harness.init(testing.allocator, "dur3");
    defer h.deinit();
    var d3: Dur3Log = .{};
    try h.spawnLane(d3.hooks(), "0", 0);
    const lane = h.lane.?;
    var k: u64 = 1;
    while (k <= 3) : (k += 1) {
        const a = try h.append(k, 500, 0x5A);
        try a.ticket.wait().result;
        // The strict group's barrier landed its ticket before the ack.
        try testing.expectEqual(k, d3.started.load(.acquire));
        try testing.expectEqual(k, d3.landed.load(.acquire));
        try testing.expectEqual(k, d3.last_landed.load(.acquire));
    }
    const t = try lane.requestBarrier();
    try t.wait().result;
    try testing.expectEqual(@as(u64, 4), d3.started.load(.acquire));
    try testing.expectEqual(@as(u64, 4), d3.landed.load(.acquire));
    try testing.expectEqual(@as(u64, 0), d3.bad.load(.acquire));
    lane.shutdown();
    try testing.expectEqual(@as(u64, 4), d3.landed.load(.acquire));
}

const RollbackLog = struct {
    calls: std.ArrayList(struct { span: SeqSpan, window: Reservation }) = .empty,
    checkpoints: std.ArrayList(u64) = .empty,
    checkpoint_ok: bool = true,
    gpa: Allocator,

    fn rollback(ctx: ?*anyopaque, span: SeqSpan, window: Reservation) void {
        const self: *RollbackLog = @ptrCast(@alignCast(ctx.?));
        self.calls.append(self.gpa, .{ .span = span, .window = window }) catch unreachable;
    }

    fn checkpointPast(ctx: ?*anyopaque, padded_end: u64) bool {
        const self: *RollbackLog = @ptrCast(@alignCast(ctx.?));
        self.checkpoints.append(self.gpa, padded_end) catch unreachable;
        return self.checkpoint_ok;
    }

    fn hooks(self: *RollbackLog) Hooks {
        return .{ .ctx = self, .rollback = rollback, .checkpoint_past = checkpointPast };
    }

    fn deinit(self: *RollbackLog) void {
        self.calls.deinit(self.gpa);
        self.checkpoints.deinit(self.gpa);
    }
};

test "a torn window: the rollback hook runs with the stamped span, the hole is checkpointed, the latch counts one, a success resets it; replay drops exactly the hole" {
    const h = try Harness.init(testing.allocator, "torn");
    defer h.deinit();
    var rl: RollbackLog = .{ .gpa = testing.allocator };
    defer rl.deinit();
    try h.spawnLane(rl.hooks(), "600000", 0);
    const lane = h.lane.?;

    const a = try h.append(1, page_entry, 0xA1);
    try a.ticket.wait().result;
    test_seams.fail_windows.store(1, .release);
    const b = try h.append(2, page_entry, 0xB2);
    const ob = b.ticket.wait();
    try testing.expectError(error.InputOutput, ob.result);
    try testing.expectEqual(@as(u32, 0), test_seams.fail_windows.load(.acquire));
    try testing.expectEqual(@as(usize, 1), rl.calls.items.len);
    try testing.expectEqual(SeqSpan.stamped(b.res.start, b.res.len), rl.calls.items[0].span);
    try testing.expectEqual(b.res, rl.calls.items[0].window);
    try testing.expectEqualSlices(u64, &.{b.res.paddedEnd()}, rl.checkpoints.items);
    try testing.expectEqual(@as(u32, 1), lane.latch.journal_failures.load(.acquire));
    try testing.expect(!lane.isFailed());
    // The hole still COMPLETED: the prefix moved past it.
    try testing.expect(h.reg.completedUpto() >= b.res.paddedEnd());
    const c = try h.append(3, page_entry, 0xC3);
    try c.ticket.wait().result;
    try testing.expectEqual(@as(u32, 0), lane.latch.journal_failures.load(.acquire));

    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [8]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 1, 3 }, recoveredInos(&rec, &inos));
    try testing.expectEqual(@as(u64, 1), rec.dropped_torn);
    try testing.expectEqual(c.res.paddedEnd(), rec.head_pos);
}

test "three consecutive journal-write failures latch the volume FAILED; every later op answers VolumeFailed" {
    const h = try Harness.init(testing.allocator, "failed");
    defer h.deinit();
    var rl: RollbackLog = .{ .gpa = testing.allocator };
    defer rl.deinit();
    // The latch logs once, on the lane thread.
    try h.spawnLane(rl.hooks(), "600000", 1);
    const lane = h.lane.?;
    const failed_before = stats.volumes_failed.load();
    test_seams.fail_windows.store(journal.JOURNAL_FAILURE_LATCH, .release);
    var k: u64 = 0;
    while (k < journal.JOURNAL_FAILURE_LATCH) : (k += 1) {
        const a = try h.append(k + 1, 300, 0xEE);
        try testing.expectError(error.InputOutput, a.ticket.wait().result);
        try testing.expectEqual(k + 1 == journal.JOURNAL_FAILURE_LATCH, lane.isFailed());
    }
    try testing.expectEqual(@as(usize, journal.JOURNAL_FAILURE_LATCH), rl.calls.items.len);
    try testing.expectEqual(failed_before + 1, stats.volumes_failed.load());
    try testing.expectError(error.VolumeFailed, lane.latch.admitMutation());
    const r = try h.reserve(300);
    const ops = try h.build(r, 99, 300, 0x99);
    try testing.expectError(error.VolumeFailed, h.submit(r, ops));
    try testing.expectError(error.VolumeFailed, lane.requestBarrier());
    try testing.expectError(error.VolumeFailed, lane.submitStructural(ops));
    // The refused window's reservation is still the caller's to complete.
    h.reg.complete(&h.core, r.res);
    lane.shutdown();
    try testing.expectEqual(@as(usize, 1), lane.testErrorLogsFired());
}

test "a hole checkpoint that cannot drain fails the survivors loud (no barrier, no ack of unreachable entries); one that drains acks after it" {
    const h = try Harness.init(testing.allocator, "hole");
    defer h.deinit();
    var rl: RollbackLog = .{ .gpa = testing.allocator, .checkpoint_ok = false };
    defer rl.deinit();
    try h.spawnLane(rl.hooks(), "0", 1);
    const lane = h.lane.?;
    const r = try h.reserve(500);
    const ops = try h.build(r, 5, 500, 0x55);
    const t = try lane.submitWriteBatch(.{ .window = r.res, .seq_base = r.seq_base, .ops = ops, .entries = 1, .acct = Harness.acctOf(r.res, ops), .apply_holes = true });
    try testing.expectError(error.HoleCheckpointFailed, t.wait().result);
    try testing.expectEqualSlices(u64, &.{r.res.paddedEnd()}, rl.checkpoints.items);
    try testing.expectEqual(@as(u64, 0), lane.snapshot().barriers_strict);
    try testing.expectEqual(@as(u32, 1), lane.latch.journal_failures.load(.acquire));
    try testing.expectEqual(@as(usize, 0), rl.calls.items.len);

    rl.checkpoint_ok = true;
    const r2 = try h.reserve(500);
    const ops2 = try h.build(r2, 6, 500, 0x66);
    const t2 = try lane.submitWriteBatch(.{ .window = r2.res, .seq_base = r2.seq_base, .ops = ops2, .entries = 1, .acct = Harness.acctOf(r2.res, ops2), .apply_holes = true });
    try t2.wait().result;
    try testing.expectEqualSlices(u64, &.{ r.res.paddedEnd(), r2.res.paddedEnd() }, rl.checkpoints.items);
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_strict);
    lane.shutdown();
    try testing.expectEqual(@as(usize, 1), lane.testErrorLogsFired());
}

test "a misaligned run is REFUSED on the caller's thread before anything is queued (the posture tripwire counts it)" {
    const h = try Harness.init(testing.allocator, "misaligned");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    const r = try h.reserve(300);
    var page = try uring_fs.AlignedBuf.zeroed(testing.allocator, 8192, 4096);
    defer page.deinit();
    const bad = [_]WriteOp{.{ .offset = Harness.base + 1, .bytes = page.bytes[0..4096] }};
    const refusals_before = uring_fs.stats.unaligned_refusals.load();
    const writes_before = stats.lane_writes.load();
    // The tripwire logs at most once per second per site: allowed, not required.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.UnalignedDirectWrite, h.submit(r, &bad));
    log.testing.expectErrorLogs(0);
    try testing.expectEqual(refusals_before + 1, uring_fs.stats.unaligned_refusals.load());
    try testing.expectEqual(writes_before, stats.lane_writes.load());
    h.reg.complete(&h.core, r.res);
    // An aligned run on the same grain lands.
    const ok = [_]WriteOp{.{ .offset = Harness.base + 4096, .bytes = page.bytes[0..4096] }};
    const r2 = try h.reserve(300);
    const t = try h.submit(r2, &ok);
    try t.wait().result;
    _ = lane;
}

test "wake coalescing: N publishes between two polls of the stage-B fiber cost no eventfd write beyond the one that woke it" {
    const h = try Harness.init(testing.allocator, "coalesce");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    // Warm up; the ack lands BEFORE the fiber parks, so wait for the park
    // itself (the delivery word reads IDLE) — a publish that meets the
    // fiber still RUNNING elides its wake (train 2026-10-18: `expected 2,
    // found 1` on a loaded host).
    const warm = try h.append(1, 300, 0x01);
    try warm.ticket.wait().result;
    spinUntil(struct {
        f: *const Fiber,
        fn ready(s: @This()) bool {
            return s.f.state.load() == .idle;
        }
    }{ .f = lane.fiber.? });

    test_seams.held.store(0, .release);
    test_seams.hold_before_scan.store(1, .release);
    // Released on EVERY exit: a failed expectation with the fiber held at
    // the seam left `h.deinit()` waiting for ever (the shard's hang limit).
    defer {
        test_seams.hold_before_scan.store(0, .release);
        futex.wakeAll(&test_seams.hold_before_scan);
    }
    var tickets: [9]Ticket = undefined;
    // Publish #1 wakes the parked fiber: the one eventfd write.
    const w_idle = lane.snapshot().wake_writes;
    tickets[0] = (try h.append(2, 300, 0x02)).ticket;
    try testing.expectEqual(w_idle + 1, lane.snapshot().wake_writes);
    while (test_seams.held.load(.acquire) == 0) futex.wait(&test_seams.held, 0);
    // The fiber is held at the top of its poll: every publish from here
    // lands on a RUNNING fiber (the delivery word parks it as NOTIFIED) and
    // writes nothing; the poll that follows the release scans them all.
    const w0 = lane.snapshot().wake_writes;
    const e0 = lane.snapshot().wake_elided;
    var k: usize = 1;
    while (k < tickets.len) : (k += 1) tickets[k] = (try h.append(@intCast(2 + k), 300, @intCast(k))).ticket;
    const held_publishes: u64 = tickets.len - 1;
    try testing.expectEqual(w0, lane.snapshot().wake_writes);
    try testing.expectEqual(e0 + held_publishes, lane.snapshot().wake_elided);
    test_seams.hold_before_scan.store(0, .release);
    futex.wakeAll(&test_seams.hold_before_scan);
    for (tickets) |t| try t.wait().result;
    test_seams.held.store(0, .release);
    std.debug.print("\nmeasurement: {d} publishes between two fiber polls: eventfd writes {d}, elided {d}\n", .{ held_publishes, lane.snapshot().wake_writes - w0, lane.snapshot().wake_elided - e0 });
}

test "a parked predecessor holds its successors' answers: the prefix wait sits behind a reservation the lane never saw until it completes and kicks" {
    const h = try Harness.init(testing.allocator, "prefix");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    // A is reserved first and handed to nobody (a foreign completer's).
    const a = try h.reserve(400);
    const b = try h.append(2, 400, 0xB0);
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.snapshot().prefix_waits >= 1;
        }
    }{ .l = lane });
    // B landed and completed, yet its ack waits: completed_upto is held at A.
    try testing.expectEqual(a.res.start, h.reg.completedUpto());
    try testing.expectEqual(@as(?Outcome, null), b.ticket.poll());
    h.reg.complete(&h.core, a.res);
    lane.kick();
    try b.ticket.wait().result;
    try testing.expect(h.reg.completedUpto() >= b.res.paddedEnd());
}

// Rung 9 F6: 6–186 "rescues" per saturated write leg were 4 KiB journal
// writes and FSYNCs outlasting the 100 ms backstop on a shared saturated
// disk, counted at the tick. A tick that finds NOTHING moved is the device
// being slow (`journal_lane_park_backstop_ticks`), never a rescue.
test "a slow device: a window whose runs are owed past the backstop costs backstop ticks and ZERO tick rescues; it acks when the device answers" {
    const h = try Harness.init(testing.allocator, "slow-device");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    test_seams.delay_issue.store(1, .release);
    defer test_seams.delay_issue.store(0, .release);
    const parks0 = lane.snapshot().parks;
    const b = try h.append(2, 400, 0xB0);
    // Three parks the backstop ended with the window still owed (every
    // park after the admission's is a tick's: no CQE is coming, nothing
    // publishes); a park counts when it ends, its verdict at the next
    // poll, so three ended parks mean two judged ticks.
    spinUntil(struct {
        l: *JournalLane,
        p0: u64,
        fn ready(s: @This()) bool {
            return s.l.snapshot().parks >= s.p0 + 4;
        }
    }{ .l = lane, .p0 = parks0 });
    try testing.expectEqual(@as(?Outcome, null), b.ticket.poll());
    try testing.expectEqual(@as(u64, 0), lane.snapshot().tick_rescues);
    try testing.expect(lane.snapshot().backstop_ticks >= 2);
    // The device answers: the next tick issues the runs, the CQE acks.
    test_seams.delay_issue.store(0, .release);
    try b.ticket.wait().result;
    try testing.expectEqual(@as(u64, 0), lane.snapshot().tick_rescues);
}

// The rescue stays REACHABLE: a foreign completer that forgets to `kick`
// leaves the prefix wait with its condition satisfied and no wake — the tick
// finds the progress and counts exactly one rescue.
//
// The verdict is the acking poll's: its re-check finds the completed prefix,
// runs the verdicts and ACKS the successor inside `advanceDurability`, and
// judges the tick only after that returns — so the ack reaches its waiter
// BEFORE the rescue is counted. Reading the counter on the ack's heels is a
// race the waker/wakee preemption of a loaded box loses (`expected 1, found
// 0`: 4 of 200 under an 8-way `yes` load, once in a gate at load ~9); the
// pin waits for the VERDICT, the lane's own counter, never the ack.
test "a lost kick: a reservation completed elsewhere without a kick is found by the backstop's re-check — exactly one tick rescue, the successor acks" {
    const h = try Harness.init(testing.allocator, "lost-kick");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    const a = try h.reserve(400);
    const b = try h.append(2, 400, 0xB0);
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.snapshot().prefix_waits >= 1;
        }
    }{ .l = lane });
    try testing.expectEqual(@as(?Outcome, null), b.ticket.poll());
    const rescues0 = lane.snapshot().tick_rescues;
    h.reg.complete(&h.core, a.res);
    // No kick: only the backstop can find the completed prefix.
    try b.ticket.wait().result;
    try testing.expect(h.reg.completedUpto() >= b.res.paddedEnd());
    // The acking poll's verdict: one rescue, owed within the lane's next
    // few instructions — two liveness ticks later the verdict path is gone
    // and the row is red with the lane's counters.
    const judged = spinUntilBy(struct {
        l: *JournalLane,
        want: u64,
        fn ready(s: @This()) bool {
            return s.l.snapshot().tick_rescues >= s.want;
        }
    }{ .l = lane, .want = rescues0 + 1 }, clock.monotonicNs() + 2 * exec_lane.tick_ns);
    const snap = lane.snapshot();
    if (!judged) std.debug.print("\nlost kick: the acking poll never counted its rescue: tick rescues {d} (expected {d}), backstop ticks {d}, parks {d}, prefix waits {d}\n", .{ snap.tick_rescues, rescues0 + 1, snap.backstop_ticks, snap.parks, snap.prefix_waits });
    try testing.expect(judged);
    try testing.expectEqual(rescues0 + 1, snap.tick_rescues);
}

test "a predecessor handed off late joins the group at its journal position: acks leave in journal order" {
    const h = try Harness.init(testing.allocator, "late");
    defer h.deinit();
    try h.spawnLane(.{}, "0", 0);
    const lane = h.lane.?;
    const ra = try h.reserve(400);
    const ops_a = try h.build(ra, 1, 400, 0xA0);
    const b = try h.append(2, 400, 0xB0);
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.snapshot().prefix_waits >= 1;
        }
    }{ .l = lane });
    try testing.expectEqual(@as(?Outcome, null), b.ticket.poll());
    const ta = try h.submit(ra, ops_a);
    const oa = ta.wait();
    const ob = b.ticket.wait();
    try oa.result;
    try ob.result;
    try testing.expect(oa.ack_seq < ob.ack_seq);
    // One group, one strict barrier for both.
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_strict);
    try testing.expectEqual(@as(u64, 0), stats.ack_out_of_order.load());
}

test "a structural entry is answered when it LANDS (no group, no ack, no barrier) and the ONE registry still orders both classes: a user window behind it waits for its completion" {
    const h = try Harness.init(testing.allocator, "structural");
    defer h.deinit();
    try h.spawnLane(.{}, "0", 0);
    const lane = h.lane.?;
    // S is reserved first (an SMO entry's reservation), then a user window
    // U behind it; U lands and completes but its prefix wait sits behind S.
    const rs = try h.reserve(400);
    const ops_s = try h.build(rs, 7, 400, 0x5E);
    const u = try h.append(8, 400, 0xA8);
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.snapshot().prefix_waits >= 1;
        }
    }{ .l = lane });
    try testing.expectEqual(@as(?Outcome, null), u.ticket.poll());
    const acks0 = lane.snapshot().acks;
    const barriers0 = lane.snapshot().barriers_strict;
    const ts = try lane.submitStructural(ops_s);
    const os = ts.wait();
    try os.result;
    // Landed, not acked: no ack sequence, no barrier, the reservation still
    // the caller's.
    try testing.expectEqual(@as(u64, 0), os.ack_seq);
    try testing.expectEqual(acks0, lane.snapshot().acks);
    try testing.expectEqual(barriers0, lane.snapshot().barriers_strict);
    try testing.expectEqual(@as(u64, 1), lane.snapshot().structural_writes);
    try testing.expectEqual(rs.res.start, h.reg.completedUpto());
    try testing.expectEqual(@as(?Outcome, null), u.ticket.poll());
    // The caller's half of the contract: complete, then kick.
    h.reg.complete(&h.core, rs.res);
    lane.kick();
    try u.ticket.wait().result;
    try testing.expect(h.reg.completedUpto() >= u.res.paddedEnd());
    try testing.expectEqual(@as(u64, 1), lane.snapshot().lane_writes);
    // Both entries replay, in journal order.
    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [4]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{ 7, 8 }, recoveredInos(&rec, &inos));
}

test "a 4-thread submit storm: every window acked Ok, the registry settles, replay holds every entry seq-sorted" {
    const h = try Harness.init(testing.allocator, "storm");
    defer h.deinit();
    try h.spawnLane(.{}, "0", 0);
    const lane = h.lane.?;
    const threads_n = 4;
    const per_thread = 32;
    const Task = struct {
        fn run(hh: *Harness, t: u64, failed: *std.atomic.Value(u32)) void {
            runInner(hh, t) catch {
                _ = failed.fetchAdd(1, .monotonic);
            };
        }
        fn runInner(hh: *Harness, t: u64) !void {
            var i: u64 = 0;
            while (i < per_thread) : (i += 1) {
                const ino = 1000 * (t + 1) + i;
                // Each thread owns its images (the shared list is not thread-safe).
                var key: [9]u8 = undefined;
                std.mem.writeInt(u64, key[0..8], ino, .big);
                key[8] = journal.TREE_INODES;
                const entry_len: u64 = 300 + (t * 13 + i) % 200;
                const value_len: usize = @intCast(entry_len - journal.ENTRY_HDR_LEN - 1 - journal.RECORD_HEADER_LEN - key.len);
                const value = try testing.allocator.alloc(u8, value_len);
                defer testing.allocator.free(value);
                @memset(value, @truncate(ino));
                const r = try hh.reserve(entry_len);
                const recs = [_]journal.RecordFrame{.{ .tag = journal.tagFor(journal.TREE_INODES, 0), .key = &key, .kind = .put, .seq = r.seq_base, .value = value }};
                var img = try journal.buildEntryWrite(testing.allocator, hh.layout, hh.geo, 0, r.res, &recs);
                defer img.deinit();
                const ticket = while (true) {
                    if (hh.submit(r, img.ops)) |tk| break tk else |e| {
                        if (e != error.LaneQueueFull) return e;
                        std.Thread.yield() catch {};
                    }
                };
                const o = ticket.wait();
                try o.result;
                try testing.expect(hh.reg.completedUpto() >= r.res.paddedEnd());
            }
        }
    };
    var failed: std.atomic.Value(u32) = .init(0);
    var threads: [threads_n]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Task.run, .{ h, @as(u64, i), &failed });
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.monotonic));
    const total: u64 = threads_n * per_thread;
    try testing.expectEqual(total, lane.snapshot().lane_writes);
    try testing.expectEqual(total, lane.snapshot().acks);
    try testing.expectEqual(@as(usize, 0), h.reg.openCount());
    try testing.expectEqual(h.core.headPos(), h.reg.completedUpto());
    try testing.expectEqual(@as(u64, 0), h.core.admitted());
    try testing.expectEqual(@as(u64, 0), stats.ack_out_of_order.load());

    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [total]u64 = undefined;
    const got = recoveredInos(&rec, &inos);
    try testing.expectEqual(@as(usize, total), got.len);
    for (rec.entries[1..], 1..) |e, k| try testing.expect(e.seq > rec.entries[k - 1].seq);
    std.mem.sort(u64, got, {}, std.sort.asc(u64));
    var t: u64 = 0;
    while (t < threads_n) : (t += 1) {
        var i: u64 = 0;
        while (i < per_thread) : (i += 1) try testing.expectEqual(1000 * (t + 1) + i, got[@intCast(t * per_thread + i)]);
    }
    try testing.expectEqual(@as(u64, 0), rec.dropped_torn);
    const snap = lane.snapshot();
    // Strict: at most one barrier per window, and groups amortize them.
    try testing.expect(snap.barriers_strict >= 1 and snap.barriers_strict <= total);
    std.debug.print("\nmeasurement: strict storm, {d} threads × {d} windows: fiber parks {d}, io_uring_enter {d}, barriers {d} (groups), wake writes {d}, elided {d}, tick rescues {d}\n", .{ threads_n, per_thread, snap.parks, snap.enters, snap.barriers_strict, snap.wake_writes, snap.wake_elided, snap.tick_rescues });
}

test "shutdown drains: windows submitted and never awaited are still landed, acked and left durable; the join owes nothing" {
    const h = try Harness.init(testing.allocator, "drain");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    var tickets: [12]Ticket = undefined;
    var ends: [12]u64 = undefined;
    for (&tickets, 0..) |*t, i| {
        const a = try h.append(@intCast(50 + i), 300, @intCast(i));
        t.* = a.ticket;
        ends[i] = a.res.paddedEnd();
    }
    lane.shutdown();
    for (tickets) |t| try t.wait().result;
    // The trailing coalesced barrier ran at exit (acked entries left durable).
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_coalesced);
    try testing.expectEqual(@as(usize, 0), h.reg.openCount());
    try testing.expectEqual(h.core.headPos(), h.reg.completedUpto());
    var rec = try h.replay(0);
    defer rec.deinit();
    try testing.expectEqual(@as(usize, 12), rec.entries.len);
    try testing.expectEqual(ends[11], rec.head_pos);
    const r = try h.reserve(300);
    const ops = try h.build(r, 1, 300, 0);
    try testing.expectError(error.LaneStopped, h.submit(r, ops));
    h.reg.complete(&h.core, r.res);
}

test "a dead ring latches the volume FAILED at once (a device fence is terminal), counted on journal_lane_rings_dead; the in-flight window fails RingFailed; later requests refuse VolumeFailed" {
    const h = try Harness.init(testing.allocator, "deadring");
    defer h.deinit();
    // TWO error logs on the lane thread: the latch line and the exec lane's
    // own dead-ring line.
    try h.spawnLane(.{}, "600000", 2);
    const lane = h.lane.?;
    const dead0 = stats.rings_dead.load();
    const failed0 = stats.volumes_failed.load();
    const a = try h.append(1, 500, 0x11);
    try a.ticket.wait().result;
    try testing.expect(!lane.isFailed());
    // The poll that admits b prepares its WRITE SQE, then the ring dies before
    // any enter, as a failed `io_uring_enter` leaves it. A foreign message
    // posted during that poll (the shutdown kick's shape) sends the lane loop
    // through its pre-park flush: a dead ring must submit nothing there.
    test_seams.held.store(0, .release);
    test_seams.hold_before_scan.store(1, .release);
    const b = try h.append(2, 500, 0x22);
    while (test_seams.held.load(.acquire) == 0) futex.wait(&test_seams.held, 0);
    test_seams.kill_ring.store(1, .release);
    lane.execLane().postCancelOp(lane.fiber.?);
    test_seams.hold_before_scan.store(0, .release);
    futex.wakeAll(&test_seams.hold_before_scan);
    try testing.expectError(error.RingFailed, b.ticket.wait().result);
    test_seams.held.store(0, .release);
    try testing.expect(lane.isFailed());
    try testing.expectEqual(dead0 + 1, stats.rings_dead.load());
    try testing.expectEqual(failed0 + 1, stats.volumes_failed.load());
    try testing.expectError(error.VolumeFailed, lane.requestBarrier());
    lane.shutdown();
    try testing.expectEqual(@as(usize, 2), lane.testErrorLogsFired());
    try testing.expect(!lane.execLane().alive.load(.acquire));
    // The healthy entry stands on the device; the dead one never landed.
    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [4]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{1}, recoveredInos(&rec, &inos));
}

test "a same-lane fiber's ticket wait is a fiber park: a fiber on the journal lane submits a structural entry and a barrier and is answered by the lane's own CQEs" {
    const h = try Harness.init(testing.allocator, "samelane");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    const r = try h.reserve(400);
    const ops = try h.build(r, 3, 400, 0x3C);
    const Job = struct {
        lane: *JournalLane,
        ops: []const WriteOp,
        structural_ok: bool = false,
        barrier_ok: bool = false,
        on_lane: bool = false,
        done: futex.Word = .init(0),

        fn run(context: *const anyopaque) void {
            const p: *const *@This() = @ptrCast(@alignCast(context));
            const j = p.*;
            j.on_lane = j.lane.onLaneThread();
            if (j.lane.submitStructural(j.ops)) |t| {
                j.structural_ok = if (t.wait().result) |_| true else |_| false;
            } else |_| {}
            if (j.lane.requestBarrier()) |t| {
                j.barrier_ok = if (t.wait().result) |_| true else |_| false;
            } else |_| {}
            j.done.store(1, .release);
            futex.wakeAll(&j.done);
        }
    };
    var job: Job = .{ .lane = lane, .ops = ops };
    var g: Io.Group = .init;
    group_mod.addMember(&g);
    const jp: *Job = &job;
    _ = try lane.execLane().spawn(.group, .{ .group = Job.run }, std.mem.asBytes(&jp), &g, .blocked);
    while (job.done.load(.acquire) == 0) futex.wait(&job.done, 0);
    group_mod.awaitGroup(&g, null, &.{}) catch {};
    try testing.expect(job.on_lane);
    try testing.expect(job.structural_ok);
    try testing.expect(job.barrier_ok);
    try testing.expectEqual(@as(u64, 1), lane.snapshot().structural_writes);
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_coalesced);
    h.reg.complete(&h.core, r.res);
    var rec = try h.replay(0);
    defer rec.deinit();
    var inos: [4]u64 = undefined;
    try testing.expectEqualSlices(u64, &.{3}, recoveredInos(&rec, &inos));
}

test "a fiber on ANOTHER lane waiting on a journal ticket parks only itself: a bystander fiber homed on the same lane runs while stage B is held before the ack; the wait counts as a fiber park, a raw thread's as a raw park (decision 0001 amendment 2026-10-18, the journal lane)" {
    // The route outlives the lane (deliver wakes through it until the
    // stage-B fiber's exit), so the executor is built first and torn down
    // last.
    const sqz = try sqz_io.SqzIo.testInstance();
    defer sqz.deinit();
    const io = sqz.io();
    const h = try Harness.init(testing.allocator, "foreign-fiber");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    lane.routeWaits(io);
    const fiber_parks0 = stats.wait_fiber_parks.load();
    const raw_parks0 = stats.wait_raw_parks.load();
    // The test thread is a raw waiter through the route: one raw park.
    const warm = try h.append(1, 300, 0x01);
    try warm.ticket.wait().result;
    try testing.expectEqual(raw_parks0 + 1, stats.wait_raw_parks.load());
    try testing.expectEqual(fiber_parks0, stats.wait_fiber_parks.load());
    spinUntil(struct {
        f: *const Fiber,
        fn ready(s: @This()) bool {
            return s.f.state.load() == .idle;
        }
    }{ .f = lane.fiber.? });

    // Stage B held at the top of its next poll: the barrier request below
    // is published, wakes the fiber, and sits in the inbound queue — no
    // barrier can start, so the fsync's ack is held without a sleep.
    test_seams.held.store(0, .release);
    test_seams.hold_before_scan.store(1, .release);
    defer {
        test_seams.hold_before_scan.store(0, .release);
        futex.wakeAll(&test_seams.hold_before_scan);
        test_seams.held.store(0, .release);
    }
    const S = struct {
        /// A handler's fsync from a fiber on a meta lane.
        fn fsync(l: *JournalLane) Error!void {
            const t = try l.requestBarrier();
            return t.wait().result;
        }
        /// The bystander on the same lane: runs iff the lane thread is free.
        fn bystander(ran: *futex.Word) void {
            ran.store(1, .release);
            futex.wakeAll(ran);
        }
    };
    const mlane = sqz.metaLanes()[0];
    var waiter = try sqz.spawnOn(mlane, S.fsync, .{lane});
    while (test_seams.held.load(.acquire) == 0) futex.wait(&test_seams.held, 0);
    // The ack is held: the waiter is parked, counted before its park.
    while (stats.wait_fiber_parks.load() == fiber_parks0 and stats.wait_raw_parks.load() == raw_parks0 + 1) std.atomic.spinLoopHint();
    var ran: futex.Word = .init(0);
    var by = try sqz.spawnOn(mlane, S.bystander, .{&ran});
    // THE LAW: the bystander runs during the fsync wait. A waiter that held
    // the lane thread would keep it off the CPU until the ack; the bound is
    // a diagnostic, never a synchronization (the held ack cannot arrive, so
    // a timeout here means the lane thread is held — a free lane runs the
    // bystander in microseconds, loaded venue or not).
    while (ran.load(.acquire) == 0) {
        if (futex.waitTimeout(&ran, 0, 30 * std.time.ns_per_s) == .timed_out and ran.load(.acquire) == 0) {
            std.debug.print("the fiber waiting on the journal ticket holds its lane thread: the bystander never ran\n", .{});
            test_seams.hold_before_scan.store(0, .release);
            futex.wakeAll(&test_seams.hold_before_scan);
            try waiter.await(io);
            by.await(io);
            return error.TestExpectedEqual;
        }
    }
    by.await(io);
    try testing.expectEqual(fiber_parks0 + 1, stats.wait_fiber_parks.load());
    try testing.expectEqual(raw_parks0 + 1, stats.wait_raw_parks.load());
    // Release stage B: the barrier runs, the fsync is acked through the route.
    test_seams.hold_before_scan.store(0, .release);
    futex.wakeAll(&test_seams.hold_before_scan);
    try waiter.await(io);
    try testing.expectEqual(@as(u64, 1), lane.snapshot().barriers_coalesced);
    try testing.expectEqual(fiber_parks0 + 1, stats.wait_fiber_parks.load());
}

test "the stats JSON names every lane counter" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"journal_lanes_spawned\":", "\"journal_lanes_live\":", "\"journal_ring_lane_writes\":", "\"journal_lane_structural_writes\":", "\"journal_lane_write_failures\":", "\"journal_lane_rollbacks\":", "\"journal_lane_barriers_strict\":", "\"journal_lane_barriers_coalesced\":", "\"journal_lane_barrier_failures\":", "\"journal_lane_wake_writes\":", "\"journal_lane_wake_elided\":", "\"journal_lane_parks\":", "\"journal_lane_park_tick_rescues\":", "\"journal_lane_park_backstop_ticks\":", "\"journal_lane_prefix_waits\":", "\"journal_lane_wait_fiber_parks\":", "\"journal_lane_wait_raw_parks\":", "\"journal_lane_queue_full\":", "\"journal_lane_volumes_failed\":", "\"journal_lane_rings_dead\":", "\"journal_lane_cqe_orphan\":", "\"journal_lane_ack_out_of_order\":" }) |needle| {
        try testing.expect(std.mem.indexOf(u8, s, needle) != null);
    }
}
