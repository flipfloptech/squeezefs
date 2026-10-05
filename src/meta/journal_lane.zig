//! The journal lane: ONE OS thread per writable metadata volume that owns
//! the volume's journal io_uring and parks IN it (charter §5.2 "Both stages
//! run on ONE OS thread per writable volume (`sqz-jrnl<N>`)", §5.1 "Device
//! posture"; `docs/porting/kv_journal_ledger.md` §3.4–3.5;
//! `docs/porting/kv_tree_cache_commit_survey.md` §2.3, §2.6, §3.5, §7 row 11;
//! `docs/decisions/0001-executor-is-an-io-implementation.md` §2). Linux only.
//!
//! WHY one thread that owns the ring: on the shared `sqz-meta` pool the
//! completion's cross-thread wake hop measured 746/664 µs of a 1,209/1,130 µs
//! journal-write round trip — 62 % / 59 % (C-2,
//! `.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`); the lane takes it
//! to 53–107 µs. Here a task wake (the eventfd READ armed at
//! `user_data = u64::MAX`) and a device CQE are ONE `io_uring_enter` wait,
//! and the thread that reaps the CQE is the thread that runs the durability
//! protocol and acks — no scheduler wake, no run-queue wait behind the serve
//! plane.
//!
//! Shape: a caller (the conveyor's apply pass; any thread) publishes a
//! window — the aligned runs `journal.buildBatchWrite` produced for ONE
//! registered reservation — into a bounded lock-free inbound queue and wakes
//! the lane through a SeqCst wake coalescer; the lane issues the runs as
//! WRITE SQEs on its ring (O_DIRECT, fixed file 0), tracks every run's CQE,
//! and runs stage B per group: complete every landed window's reservation in
//! journal order → ONE completed-prefix wait → verdicts (a failed window: the
//! seq-conditional rollback hook, the failure latch, a checkpoint past the
//! permanent hole) → hole checkpoints for acked windows with in-window apply
//! failures → ONE barrier (`FSYNC|DATASYNC` on the same ring; strict cadence
//! = per group, else `needs_flush` at the flush interval) → acks in journal
//! order through per-window futex slots. Acks only after the entry landed;
//! strict: after the barrier.
//!
//! Lane-native only: every window reaches the ring through the inbound
//! queue whatever thread built it, so there is no off-lane submitter, hence
//! no pool path (the reference's `journal_ring_pool_writes` has no
//! counterpart) and no ringless degrade — a box that cannot give the lane a
//! ring refuses at spawn (charter §1.2: no classical fallback).
//!
//! The ring is the lane's own `linux.IoUring`, not `uring_fs.OwnedRing`:
//! that reactor's face carries writes, the park and the wake but neither an
//! FSYNC SQE nor a fixed file, both of which the barrier-on-the-lane-ring
//! design needs. The park shape (flush, then an EXT_ARG timed wait with the
//! wake READ armed), the posture law (`prepareDirectWrite`) and the
//! `user_data` space are shared with it.
//!
//! Zig has no unwinding panics: where the reference panic-guards its lane
//! task (`LaneSentinel`), a panic on this thread aborts the process — fail
//! loud. Every recoverable outcome is an error union answered through the
//! window's slot, and every "cannot happen" is a counted tripwire.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const IoUring = linux.IoUring;
const Allocator = std.mem.Allocator;

const align_probe = @import("../io/align_probe.zig");
const clock = @import("../core/clock.zig");
const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const nd = @import("../io/nvme_derive.zig");
const thread_name = @import("../core/thread_name.zig");
const uring_fs = @import("../io/uring_fs.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("journal_lane is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Reservation = jc.Reservation;
pub const SeqSpan = jc.SeqSpan;
pub const WriteOp = journal.WriteOp;

// ---------------------------------------------------------------------------
// Constants — value and WHY.
// ---------------------------------------------------------------------------

/// SQ depth of a volume's journal ring: a window's entries (≤ the commit
/// batch cap, each one or two segment SQEs plus its owned page headers on an
/// unpadded ring; one aligned run per contiguous span on a padded one) plus
/// the wake READ and the one barrier; `uring_fs`'s owned-ring depth, whose
/// SQ-full arm (`submit()` then push again) covers a wider chained batch.
pub const ring_entries: u16 = uring_fs.journal_ring_entries;

/// The park's backstop while the lane OWES a completion (a run, a prefix, a
/// barrier): a CQE or a kick always arrives first on a healthy lane, so a
/// tick here is a rescue (`journal_lane_park_tick_rescues` ≈ 0) — 100 ms is
/// three orders above the quiet journal round trip (116 µs) and a tenth of
/// the 1 s checkpoint cadence it would otherwise hold back.
pub const park_backstop_ns: u64 = 100 * std.time.ns_per_ms;

/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` default: the legacy v2 flusher cadence
/// reused as the coalesced barrier's period; 0 = strict (one barrier per
/// group before any ack).
pub const default_flush_interval_ms: u64 = 50;

/// `user_data` of the wake READ (`uring_fs.wake_ud`) and of the one barrier
/// in flight; run slots are small indices. `u64::MAX − 1` stays reserved in
/// parity with `uring_fs` (its fault shim's linked timeout).
const ud_wake: u64 = uring_fs.wake_ud;
const ud_barrier: u64 = std.math.maxInt(u64) - 2;

/// Inbound cells (= windows the lane can hold): one per ring page — on a
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

/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (0..=600000, default 50; 0 = strict).
pub fn resolveFlushCadence(env: *const std.process.Environ.Map) Cadence {
    return cadenceFrom(knobs.intKnob(u64, env, "SQUEEZEFS_META_FLUSH_INTERVAL_MS", default_flush_interval_ms));
}

// ---------------------------------------------------------------------------
// Errors: the io layer's classes plus the lane's own.
// ---------------------------------------------------------------------------

pub const Error = uring_fs.Error || error{
    /// The volume latched FAILED (`JOURNAL_FAILURE_LATCH` consecutive
    /// failures): every later op answers this until remount.
    VolumeFailed,
    /// The lane is stopping or stopped.
    LaneStopped,
    /// The inbound queue is full (backpressure; `journal_lane_queue_full`).
    LaneQueueFull,
    /// The checkpoint past a hole this ack depended on could not drain.
    HoleCheckpointFailed,
};

pub const ErrorClass = uring_fs.ErrorClass;

pub fn classOf(err: Error) ErrorClass {
    return switch (err) {
        error.VolumeFailed, error.LaneStopped => .refusal,
        error.LaneQueueFull => .retryable,
        error.HoleCheckpointFailed => .terminal,
        else => |e| uring_fs.classOf(@errorCast(e)),
    };
}

pub const SpawnError = error{ OutOfMemory, RingSetupFailed, ThreadSpawnFailed, DeviceOpenFailed } || uring_fs.RegisterError;

/// A CQE's `res`: negative is `-errno`.
fn cqeError(res: i32) Error {
    return uring_fs.fromErrno(@fromBackingInt(@as(u16, @intCast(-res))));
}

// ---------------------------------------------------------------------------
// Futex primitives (Linux raw; no `Io` on the lane).
// ---------------------------------------------------------------------------

fn futexWait(word: *const std.atomic.Value(u32), expect: u32) void {
    // EAGAIN (word != expect) and EINTR both mean "re-check"; the callers loop.
    _ = linux.futex_4arg(&word.raw, .{ .cmd = .WAIT, .private = true }, expect, null);
}

fn futexWakeAll(word: *const std.atomic.Value(u32)) void {
    // FUTEX_WAKE's count is a C int: `maxInt(u32)` would wake exactly one.
    _ = linux.futex_3arg(&word.raw, .{ .cmd = .WAKE, .private = true }, std.math.maxInt(i32));
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
    /// Windows submitted on a lane's own ring (`journal_ring_lane_writes` —
    /// the engagement instrument: ≡ conveyor passes on a lane mount).
    lane_writes: counters.ShardedCounter = .{},
    write_failures: counters.ShardedCounter = .{},
    rollbacks: counters.ShardedCounter = .{},
    barriers_strict: counters.ShardedCounter = .{},
    barriers_coalesced: counters.ShardedCounter = .{},
    barrier_failures: counters.ShardedCounter = .{},
    /// The wake economy: eventfd writes performed vs elided by the
    /// coalescer. `writes ÷ (writes + elided)` ≈ 1 under saturation = the
    /// coalescer stopped eliding (the `transport_wake_{writes,elided}` law).
    wake_writes: counters.ShardedCounter = .{},
    wake_elided: counters.ShardedCounter = .{},
    /// Parks COMPLETED (a CQE, a wake or a tick ended them); an idle lane
    /// sits inside one untimed park and adds nothing here.
    parks: counters.ShardedCounter = .{},
    /// Backstop ticks that fired while a completion was owed — ≈ 0 healthy.
    park_tick_rescues: counters.ShardedCounter = .{},
    prefix_waits: counters.ShardedCounter = .{},
    queue_full: counters.ShardedCounter = .{},
    volumes_failed: counters.ShardedCounter = .{},
    wake_arm_failed: counters.Tripwire = .init("journal_lane_wake_arm_failed"),
    cqe_orphan: counters.Tripwire = .init("journal_lane_cqe_orphan"),
    queue_push_with_cell: counters.Tripwire = .init("journal_lane_queue_push_with_cell"),
};

pub var stats: Stats = .{};

/// `{"journal_lanes_spawned":n,…}`
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"journal_lanes_spawned\":{d},\"journal_lanes_live\":{d},\"journal_ring_lane_writes\":{d},\"journal_lane_write_failures\":{d},\"journal_lane_rollbacks\":{d},\"journal_lane_barriers_strict\":{d},\"journal_lane_barriers_coalesced\":{d},\"journal_lane_barrier_failures\":{d},\"journal_lane_wake_writes\":{d},\"journal_lane_wake_elided\":{d},\"journal_lane_parks\":{d},\"journal_lane_park_tick_rescues\":{d},\"journal_lane_prefix_waits\":{d},\"journal_lane_queue_full\":{d},\"journal_lane_volumes_failed\":{d},\"journal_lane_wake_arm_failed\":{d},\"journal_lane_cqe_orphan\":{d}}}", .{
        stats.lanes_spawned.load(),
        stats.lanes_live.load(),
        stats.lane_writes.load(),
        stats.write_failures.load(),
        stats.rollbacks.load(),
        stats.barriers_strict.load(),
        stats.barriers_coalesced.load(),
        stats.barrier_failures.load(),
        stats.wake_writes.load(),
        stats.wake_elided.load(),
        stats.parks.load(),
        stats.park_tick_rescues.load(),
        stats.prefix_waits.load(),
        stats.queue_full.load(),
        stats.volumes_failed.load(),
        stats.wake_arm_failed.load(),
        stats.cqe_orphan.load(),
    });
}

/// Per-lane mirrors (the lane thread writes; any thread reads).
pub const LaneSnapshot = struct {
    lane_writes: u64,
    parks: u64,
    tick_rescues: u64,
    wake_writes: u64,
    wake_elided: u64,
    barriers_strict: u64,
    barriers_coalesced: u64,
    barrier_failures: u64,
    prefix_waits: u64,
    acks: u64,
};

const LaneStats = struct {
    lane_writes: std.atomic.Value(u64) = .init(0),
    parks: std.atomic.Value(u64) = .init(0),
    tick_rescues: std.atomic.Value(u64) = .init(0),
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

    fn snapshot(self: *const LaneStats) LaneSnapshot {
        return .{
            .lane_writes = self.lane_writes.load(.monotonic),
            .parks = self.parks.load(.monotonic),
            .tick_rescues = self.tick_rescues.load(.monotonic),
            .wake_writes = self.wake_writes.load(.monotonic),
            .wake_elided = self.wake_elided.load(.monotonic),
            .barriers_strict = self.barriers_strict.load(.monotonic),
            .barriers_coalesced = self.barriers_coalesced.load(.monotonic),
            .barrier_failures = self.barrier_failures.load(.monotonic),
            .prefix_waits = self.prefix_waits.load(.monotonic),
            .acks = self.acks.load(.monotonic),
        };
    }
};

// ---------------------------------------------------------------------------
// Test seams (one load each when unarmed; `SQUEEZEFS_TEST_*`-class hooks
// that hold the exact window a pin needs — never a sleep).
// ---------------------------------------------------------------------------

pub const TestSeams = struct {
    /// While > 0 each admitted window is failed synthetically (no SQE is
    /// issued, its bytes never land, outcome `InputOutput`) — the torn-run
    /// shape of the reference's `arm_torn_write(keep = 0)`.
    fail_windows: std.atomic.Value(u32) = .init(0),
    /// 1 ⇒ the lane publishes `held = 1` and waits here BEFORE its next park
    /// (after the pass's disarm + scan), so a test can publish N messages
    /// between two lane passes deterministically.
    hold_before_park: std.atomic.Value(u32) = .init(0),
    held: std.atomic.Value(u32) = .init(0),
};

pub var test_seams: TestSeams = .{};

// ---------------------------------------------------------------------------
// Hooks the conveyor installs: both run ON the lane thread, synchronously,
// and must never wait on this lane.
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
};

// ---------------------------------------------------------------------------
// Requests, outcomes, the per-window slot.
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
    /// Accounting: surviving entries and Σ their `res.len`.
    entries: u64,
    bytes: u64,
    /// The window had in-window apply failures (rolled-back members): the
    /// survivors' ack waits for a checkpoint past the window.
    apply_holes: bool = false,
};

pub const Outcome = struct {
    result: Error!void,
    /// Lane-wide ack sequence: strictly increasing in journal order.
    ack_seq: u64,
    landed_ns: u64,
    acked_ns: u64,
};

const Kind = enum(u8) { window, barrier };

/// One inbound message = one cell: the caller fills it, publishes its index,
/// parks on `state`; the lane owns it from the pop to the publish of
/// `outcome`. The queue push (release) / pop (acquire) orders the caller's
/// fill before the lane's reads; the `state` release store orders the lane's
/// outcome before the caller's acquire load.
const Cell = struct {
    kind: Kind = .window,
    window: Reservation = .{ .start = 0, .len = 0, .pad = 0 },
    seq_base: u64 = 0,
    ops: []const WriteOp = &.{},
    entries: u64 = 0,
    bytes: u64 = 0,
    apply_holes: bool = false,
    // ---- lane-owned ----
    runs_in_flight: u32 = 0,
    first_err: ?Error = null,
    landed: bool = false,
    landed_ns: u64 = 0,
    verdict: Error!void = {},
    /// A barrier request: the ticket of the barrier that satisfies it.
    need_barrier: u64 = 0,
    outcome: Outcome = undefined,
    /// The oneshot word: `pending` until the lane publishes `outcome`.
    state: std.atomic.Value(u32) = .init(pending),

    const pending: u32 = 0;
    const done: u32 = 1;

    fn resetAs(self: *Cell, kind: Kind) void {
        self.kind = kind;
        self.window = .{ .start = 0, .len = 0, .pad = 0 };
        self.seq_base = 0;
        self.ops = &.{};
        self.entries = 0;
        self.bytes = 0;
        self.apply_holes = false;
        self.runs_in_flight = 0;
        self.first_err = null;
        self.landed = false;
        self.landed_ns = 0;
        self.verdict = {};
        self.need_barrier = 0;
    }
};

/// The awaitable handle of a submitted window or barrier request. `wait` /
/// `poll` CONSUME it (the cell returns to the lane's pool) — observe once.
pub const Ticket = struct {
    lane: *JournalLane,
    cell: u32,

    pub fn wait(self: Ticket) Outcome {
        const c = &self.lane.cells[self.cell];
        // Acquire pairs with the lane's release store in `publish`.
        while (c.state.load(.acquire) == Cell.pending) futexWait(&c.state, Cell.pending);
        const o = c.outcome;
        self.lane.freeCell(self.cell);
        return o;
    }

    /// Non-blocking: null while the lane still owns the cell.
    pub fn poll(self: Ticket) ?Outcome {
        const c = &self.lane.cells[self.cell];
        if (c.state.load(.acquire) != Cell.done) return null;
        const o = c.outcome;
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
    index: u32,
    core: *jc.JournalCore,
    registry: *jc.InflightRegistry,
    ring_stats: *journal.RingStats,
    hooks: Hooks = .{},
    /// The log seam is thread-local: error logs the LANE thread is expected
    /// to emit in a test (the FAILED latch, a hole checkpoint that could not
    /// drain). Applied on the lane thread; `testErrorLogsFired` reads back.
    test_error_logs: if (builtin.is_test) usize else void = if (builtin.is_test) 0 else {},
};

pub const JournalLane = struct {
    gpa: Allocator,
    path: []u8,
    index: u32,
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
    /// on the ring when `fixed_file`.
    fd: linux.fd_t,
    fixed_file: bool,
    /// Owned by the lane thread after spawn; torn down there at exit.
    ring: IoUring,
    wake_fd: linux.fd_t,
    /// The wake READ's target: the lane struct is heap-allocated, so the
    /// address is stable across parks.
    wake_buf: u64 = 0,
    coalescer: nd.WakeCoalescer = .{},
    cells: []Cell,
    free_cells: nd.BoundedMpmcQueue(u32),
    inbound: nd.BoundedMpmcQueue(u32),
    /// Refuses new submitters (SeqCst — Dekker with `inflight_submitters`).
    closing: std.atomic.Value(bool) = .init(false),
    /// Producers inside `publish`. The closer waits for 0 before sealing.
    inflight_submitters: std.atomic.Value(u32) = .init(0),
    /// Set only after every in-flight submitter left: the lane's disconnect
    /// signal, after which the queue can only drain.
    sealed: std.atomic.Value(bool) = .init(false),
    thread: ?std.Thread = null,
    /// The lane thread's kernel tid, published at start (0 until then).
    tid: std.atomic.Value(i32) = .init(0),
    stats: LaneStats = .{},
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

        // Level-triggered counter; non-blocking so a READ racing a late
        // write never blocks a plain read.
        const efd_rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
        if (linux.errno(efd_rc) != .SUCCESS) {
            log.err("journal lane {d}: eventfd failed ({t})", .{ opts.index, linux.errno(efd_rc) });
            return error.RingSetupFailed;
        }
        const wake_fd: linux.fd_t = @intCast(efd_rc);
        errdefer _ = linux.close(wake_fd);

        var ring = IoUring.init(ring_entries, 0) catch |e| {
            log.err("journal lane {d}: io_uring_setup({d}) failed: {s} — the journal lane cannot start (charter §1.2: no classical fallback)", .{ opts.index, ring_entries, @errorName(e) });
            return error.RingSetupFailed;
        };
        errdefer ring.deinit();
        // Fixed FILE: every SQE names index 0 with IOSQE_FIXED_FILE (one fd
        // lookup/refcount per op saved). A refusal degrades to the raw fd.
        const fixed_file = if (ring.register_files(&.{fd})) |_| true else |e| blk: {
            log.debug("journal lane {d}: register_files failed ({s}); using the raw fd", .{ opts.index, @errorName(e) });
            break :blk false;
        };

        const cap = inboundCap(opts.core.geometry().pages);
        const cells = try gpa.alloc(Cell, cap);
        errdefer gpa.free(cells);
        for (cells) |*c| c.* = .{};
        var free_cells = try nd.BoundedMpmcQueue(u32).init(gpa, cap);
        errdefer free_cells.deinit(gpa);
        var inbound = try nd.BoundedMpmcQueue(u32).init(gpa, cap);
        errdefer inbound.deinit(gpa);
        var i: u32 = 0;
        while (i < cap) : (i += 1) std.debug.assert(free_cells.tryPush(i));

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
            .fixed_file = fixed_file,
            .ring = ring,
            .wake_fd = wake_fd,
            .cells = cells,
            .free_cells = free_cells,
            .inbound = inbound,
            .test_error_logs = opts.test_error_logs,
        };
        self.thread = std.Thread.spawn(.{}, laneMain, .{self}) catch |e| {
            log.err("journal lane {d}: spawning the thread failed: {s}", .{ opts.index, @errorName(e) });
            return error.ThreadSpawnFailed;
        };
        stats.lanes_spawned.inc();
        return self;
    }

    /// Stop + join (idempotent): refuse new submitters → wait for in-flight
    /// submitters → seal → unconditional eventfd write → join. The lane
    /// finishes every window it holds (acks, the trailing barrier) before it
    /// exits; the join is SYNCHRONOUS so exit cleanup has run on return.
    /// Never aborted from outside: the stop latches and the lane drains.
    pub fn shutdown(self: *JournalLane) void {
        // SeqCst pairs with `publish`'s SeqCst census RMW + closing load:
        // exactly one of {closer, submitter} sees the other.
        self.closing.store(true, .seq_cst);
        while (self.inflight_submitters.load(.seq_cst) != 0) std.Thread.yield() catch {};
        // Release: every push that preceded the seal is visible to the lane's
        // sealed-then-scan (acquire) pass.
        self.sealed.store(true, .release);
        self.writeEventfd();
        if (self.thread) |t| {
            t.join();
            self.thread = null;
        }
    }

    pub fn deinit(self: *JournalLane) void {
        self.shutdown();
        _ = linux.close(self.fd);
        _ = linux.close(self.wake_fd);
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
        return self.stats.snapshot();
    }

    /// The lane thread's kernel tid (0 until the thread published it).
    pub fn threadId(self: *const JournalLane) i32 {
        return self.tid.load(.acquire);
    }

    pub fn testErrorLogsFired(self: *const JournalLane) usize {
        return self.test_error_logs_fired.load(.acquire);
    }

    /// Submit one window on the lane's ring. Refusals answer on THIS thread
    /// before anything is queued: a misaligned run (the posture law; the
    /// tripwire fires here), a FAILED volume, a stopped lane, a full queue.
    /// Everything after that is the ticket's outcome.
    pub fn submitWriteBatch(self: *JournalLane, req: WriteRequest) Error!Ticket {
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
        c.bytes = req.bytes;
        c.apply_holes = req.apply_holes;
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

    /// Wake a parked lane without a message: the contract of every
    /// out-of-lane completer of this ring (a reservation completed elsewhere
    /// may be the one a lane group's prefix wait sits behind).
    pub fn kick(self: *JournalLane) void {
        self.wake();
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
        self.cells[idx].state.store(Cell.pending, .monotonic);
        if (!self.free_cells.tryPush(idx)) stats.queue_push_with_cell.fire("a cell could not return to the free list");
    }

    /// Publish a filled cell: queue push THEN wake — the coalescer orders
    /// the two (see `wake`).
    fn publish(self: *JournalLane, idx: u32) Error!Ticket {
        // SeqCst RMW: pairs with `shutdown`'s SeqCst `closing` store +
        // `inflight_submitters` load so exactly one of us sees the other.
        _ = self.inflight_submitters.fetchAdd(1, .seq_cst);
        defer _ = self.inflight_submitters.fetchSub(1, .release);
        if (self.closing.load(.seq_cst)) {
            self.freeCell(idx);
            return error.LaneStopped;
        }
        if (!self.inbound.tryPush(idx)) {
            // The cell pool and the queue share one bound: a held cell always
            // has a queue slot.
            stats.queue_push_with_cell.fire("inbound queue refused a push while a cell was held");
            self.freeCell(idx);
            return error.LaneQueueFull;
        }
        self.wake();
        return .{ .lane = self, .cell = idx };
    }

    /// The wake pair, modelled:
    ///
    ///     producer:  publish (queue push, release)  →  arm := swap(armed, true)
    ///                → eventfd write iff arm returned false-before (true)
    ///     lane:      wake CQE consumed  →  disarm := swap(armed, false)
    ///                → scan the queue
    ///
    /// Both RMWs are SeqCst on one word, so they form a single modification
    /// order: a producer that elides (saw `armed`) follows an unbroken chain
    /// of arms back to one that wrote the eventfd; either that write is
    /// still unconsumed (the level-triggered READ completes at the next
    /// park) or the pass that consumed it disarms AFTER this arm and scans
    /// after its disarm — the push is seen. N publishes between two lane
    /// passes cost ONE eventfd write: `journal_lane_wake_{writes,elided}`,
    /// the same law as `transport_wake_{writes,elided}`.
    fn wake(self: *JournalLane) void {
        if (self.coalescer.arm()) {
            self.writeEventfd();
            stats.wake_writes.inc();
            LaneStats.bump(&self.stats.wake_writes);
        } else {
            stats.wake_elided.inc();
            LaneStats.bump(&self.stats.wake_elided);
        }
    }

    /// Unconditional 8-byte write (teardown and the kick): the lane parks on
    /// the ring, so a sealed queue is only observable through the wake.
    fn writeEventfd(self: *JournalLane) void {
        const one: u64 = 1;
        // EAGAIN (counter at u64::MAX−1) is impossible at one increment per
        // wake and harmless: the park is already pending.
        _ = linux.write(self.wake_fd, @ptrCast(&one), @sizeOf(u64));
    }
};

// ---------------------------------------------------------------------------
// The lane thread.
// ---------------------------------------------------------------------------

fn laneMain(lane: *JournalLane) void {
    var name_buf: thread_name.CommBuf = undefined;
    var base: [24]u8 = undefined;
    _ = thread_name.nameCurrentThread(&name_buf, std.fmt.bufPrint(&base, "sqz-jrnl{d}", .{lane.index}) catch unreachable);
    if (builtin.is_test) log.testing.expectErrorLogs(lane.test_error_logs);
    // Release pairs with `threadId`'s acquire: the name is set when the tid reads nonzero.
    lane.tid.store(linux.gettid(), .release);
    stats.lanes_live.add(1);
    defer stats.lanes_live.sub(1);

    var w = Worker.init(lane) catch {
        // The lane never runs: close the door (the closer's own protocol, so
        // no submitter parks on a cell nobody will answer), then answer
        // every cell that got in.
        log.err("journal lane {d}: out of memory at start — failing every window", .{lane.index});
        lane.closing.store(true, .seq_cst);
        while (lane.inflight_submitters.load(.seq_cst) != 0) std.Thread.yield() catch {};
        lane.ring.deinit();
        while (lane.inbound.tryPop()) |idx| answerUnrun(lane, idx, error.OutOfMemory);
        return;
    };
    defer w.deinit();
    w.run();
    // The ring's close cancels the armed wake READ; its buffer outlives it.
    lane.ring.deinit();
    while (lane.inbound.tryPop()) |idx| answerUnrun(lane, idx, error.LaneStopped);
    if (builtin.is_test) lane.test_error_logs_fired.store(log.testing.capturedErrorLogs(), .release);
}

/// Answer a cell the lane will never run (start failure, post-seal
/// stragglers): the oneshot fires with the error; nothing was issued.
fn answerUnrun(lane: *JournalLane, idx: u32, err: Error) void {
    const c = &lane.cells[idx];
    const now = clock.monotonicNs();
    c.outcome = .{ .result = err, .ack_seq = 0, .landed_ns = now, .acked_ns = now };
    c.state.store(Cell.done, .release);
    futexWakeAll(&c.state);
}

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
const ParkVerdict = enum { event, tick };

/// The lane thread's private state: slot table, the pending windows in
/// journal order, the one stage-B group in progress, the barrier slot.
const Worker = struct {
    lane: *JournalLane,
    ring: *IoUring,
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
    wake_armed: bool = false,
    wake_consumed: bool = false,
    disconnected: bool = false,
    ring_dead: bool = false,
    ack_seq: u64 = 0,
    cqes: [128]linux.io_uring_cqe = undefined,

    fn init(lane: *JournalLane) Allocator.Error!Worker {
        const gpa = lane.gpa;
        // One ring's worth of runs plus a resubmit burst never reallocates
        // the slot table; a wider window grows it (amortised).
        var slots = try std.ArrayList(?Run).initCapacity(gpa, 2 * ring_entries);
        errdefer slots.deinit(gpa);
        var free_slots = try std.ArrayList(u32).initCapacity(gpa, 2 * ring_entries);
        errdefer free_slots.deinit(gpa);
        var pending = try std.ArrayList(u32).initCapacity(gpa, lane.cells.len);
        errdefer pending.deinit(gpa);
        var group = try std.ArrayList(u32).initCapacity(gpa, lane.cells.len);
        errdefer group.deinit(gpa);
        const fsync_waiters = try std.ArrayList(u32).initCapacity(gpa, lane.cells.len);
        return .{ .lane = lane, .ring = &lane.ring, .slots = slots, .free_slots = free_slots, .pending = pending, .group = group, .fsync_waiters = fsync_waiters, .last_barrier_ns = clock.monotonicNs() };
    }

    fn deinit(w: *Worker) void {
        const gpa = w.lane.gpa;
        w.slots.deinit(gpa);
        w.free_slots.deinit(gpa);
        w.pending.deinit(gpa);
        w.group.deinit(gpa);
        w.fsync_waiters.deinit(gpa);
    }

    /// One pass per iteration: disarm-if-consumed → scan (sealed read
    /// BEFORE the pump) → stage B → the coalesced barrier when due → the
    /// exit edge → ONE park → reap.
    fn run(w: *Worker) void {
        const lane = w.lane;
        while (true) {
            // Drain → disarm → scan: the worker half of the coalescer.
            if (w.wake_consumed) {
                lane.coalescer.disarm();
                w.wake_consumed = false;
            }
            // Acquire pairs with `shutdown`'s release: every push ordered
            // before the seal is visible to this scan.
            const sealed = lane.sealed.load(.acquire);
            while (lane.inbound.tryPop()) |idx| w.admit(idx);
            if (sealed) w.disconnected = true;
            w.advanceDurability();
            var now = clock.monotonicNs();
            // The trailing barrier before exit: ONE, after the last ack, so
            // every acked entry leaves durable.
            if (w.disconnected and w.needs_flush and !w.owesCompletion()) w.flush_due_ns = now;
            w.maybeStartCoalescedBarrier(now);
            if (w.disconnected and !w.owesWork()) break;
            holdIfArmed();
            // Re-arm BEFORE sizing the tick: an unarmed wake is the one case
            // nothing but the backstop could end an idle park.
            if (!w.ring_dead and !w.wake_armed) w.armWake();
            now = clock.monotonicNs();
            const tick = w.tickFor(now);
            const verdict = w.park(tick);
            stats.parks.inc();
            LaneStats.bump(&lane.stats.parks);
            if (verdict == .tick and !(w.needs_flush and clock.monotonicNs() >= w.flush_due_ns)) {
                stats.park_tick_rescues.inc();
                LaneStats.bump(&lane.stats.tick_rescues);
            }
            w.reap();
        }
    }

    fn owesWork(w: *const Worker) bool {
        return w.owesCompletion() or w.needs_flush or w.fsync_waiters.items.len != 0;
    }

    /// A run, a window behind a group, a group, or a barrier is outstanding:
    /// the park carries the backstop.
    fn owesCompletion(w: *const Worker) bool {
        return w.runs_in_flight != 0 or w.pending.items.len != 0 or w.group.items.len != 0 or w.barrier_in_flight != null;
    }

    /// The park's tick: the backstop while a completion is owed (or while
    /// the wake could not be armed — the one case nothing else could end
    /// the park), the flush due when a coalesced barrier is pending, else
    /// none (an untimed wait on the wake and the device).
    fn tickFor(w: *const Worker, now: u64) ?u64 {
        var tick: ?u64 = if (w.owesCompletion() or !w.wake_armed) park_backstop_ns else null;
        if (w.needs_flush and w.barrier_in_flight == null) {
            const until = w.flush_due_ns -| now;
            tick = if (tick) |t| @min(t, until) else until;
        }
        return tick;
    }

    // ---- admission ----

    fn admit(w: *Worker, idx: u32) void {
        const lane = w.lane;
        const c = &lane.cells[idx];
        switch (c.kind) {
            .barrier => {
                c.need_barrier = w.barriers_started + 1;
                w.fsync_waiters.appendAssumeCapacity(idx);
                w.needs_flush = true;
                w.flush_due_ns = 0;
            },
            .window => {
                stats.lane_writes.inc();
                LaneStats.bump(&lane.stats.lane_writes);
                w.issueWindow(idx);
                w.insertPending(idx);
            },
        }
    }

    /// Issue the window's runs as WRITE SQEs. Nothing is issued for a
    /// synthetically failed window (the torn-run seam) or on a dead ring;
    /// a run that cannot get an SQE after a flush fails alone and the window
    /// lands when its siblings do.
    fn issueWindow(w: *Worker, idx: u32) void {
        const lane = w.lane;
        const c = &lane.cells[idx];
        if (test_seams.fail_windows.load(.acquire) != 0) {
            _ = test_seams.fail_windows.fetchSub(1, .acq_rel);
            c.first_err = error.InputOutput;
            markLanded(c, clock.monotonicNs());
            return;
        }
        if (w.ring_dead) {
            c.first_err = error.RingFailed;
            markLanded(c, clock.monotonicNs());
            return;
        }
        for (c.ops) |op| {
            if (op.bytes.len == 0) continue;
            // The shape was refused on the caller; here only a misaligned
            // POINTER can bounce (counted in `meta_io_bounce_bytes`).
            const prepared = uring_fs.prepareDirectWrite(lane.gpa, lane.mode, lane.path, op.offset, op.bytes) catch |e| {
                noteRunError(c, @errorCast(e));
                continue;
            };
            const slot = w.claimSlot(.{ .cell = idx, .data = prepared.data, .offset = op.offset, .written = 0, .bounce = prepared.bounce }) catch {
                var p = prepared;
                p.deinit();
                noteRunError(c, error.OutOfMemory);
                continue;
            };
            if (!w.pushRun(slot)) {
                var released = w.releaseSlot(slot);
                if (released.bounce) |*b| b.deinit();
                noteRunError(c, error.SubmissionQueueOverflow);
                continue;
            }
            c.runs_in_flight += 1;
            w.runs_in_flight += 1;
        }
        if (c.runs_in_flight == 0) markLanded(c, clock.monotonicNs());
    }

    /// Windows are handed off in reservation order by the single conveyor
    /// pass; a multi-submitter storm may not, so the list is kept sorted
    /// (an in-order arrival is an O(1) append).
    fn insertPending(w: *Worker, idx: u32) void {
        const start = w.lane.cells[idx].window.start;
        var at = w.pending.items.len;
        while (at > 0 and w.lane.cells[w.pending.items[at - 1]].window.start > start) at -= 1;
        // Capacity equals the cell count, which bounds the pending set.
        w.pending.insertAssumeCapacity(at, idx);
    }

    // ---- slots and SQEs ----

    fn claimSlot(w: *Worker, r: Run) Allocator.Error!u32 {
        if (w.free_slots.pop()) |i| {
            w.slots.items[i] = r;
            return i;
        }
        try w.slots.append(w.lane.gpa, r);
        return @intCast(w.slots.items.len - 1);
    }

    fn releaseSlot(w: *Worker, i: u32) Run {
        const r = w.slots.items[i].?;
        w.slots.items[i] = null;
        w.free_slots.append(w.lane.gpa, i) catch {
            // Losing a free-list entry costs a slot of table growth later,
            // never correctness.
        };
        return r;
    }

    /// `get_sqe` with one SQ-full flush: a plain `submit()` frees the SQ
    /// independently of completions.
    fn getSqe(w: *Worker) ?*linux.io_uring_sqe {
        if (w.ring.get_sqe()) |sqe| return sqe else |_| {}
        _ = w.ring.submit() catch {};
        return w.ring.get_sqe() catch null;
    }

    fn sqeFd(w: *const Worker) linux.fd_t {
        return if (w.lane.fixed_file) 0 else w.lane.fd;
    }

    fn markFixed(w: *const Worker, sqe: *linux.io_uring_sqe) void {
        if (w.lane.fixed_file) sqe.flags |= linux.IOSQE_FIXED_FILE;
    }

    /// Fill the WRITE SQE for slot `i` at its progress. `user_data` = slot.
    fn pushRun(w: *Worker, i: u32) bool {
        const sqe = w.getSqe() orelse return false;
        const r = &w.slots.items[i].?;
        sqe.prep_write(w.sqeFd(), r.data[r.written..], r.offset + r.written);
        w.markFixed(sqe);
        sqe.user_data = i;
        return true;
    }

    /// Push the eventfd READ under `ud_wake`. Level-triggered: a write that
    /// races the re-arm completes the next READ immediately.
    fn armWake(w: *Worker) void {
        const sqe = w.getSqe() orelse {
            stats.wake_arm_failed.fire("eventfd READ could not be queued after an SQ flush");
            return;
        };
        sqe.prep_read(w.lane.wake_fd, std.mem.asBytes(&w.lane.wake_buf), 0);
        sqe.user_data = ud_wake;
        w.wake_armed = true;
    }

    // ---- the park ----

    /// THE wait. Untimed: ONE `io_uring_enter` carrying every SQE pushed
    /// this pass (runs, the barrier, the re-armed wake READ) and the wait
    /// for the first CQE of either class. Timed: the SQEs go in their own
    /// enter FIRST — when `io_uring_enter` submits anything it returns the
    /// submitted count and DISCARDS the wait's verdict, so a combined
    /// submit-and-timed-wait never reports ETIME (liburing waits in a
    /// separate `to_submit = 0` call for the same reason).
    fn park(w: *Worker, tick_ns: ?u64) ParkVerdict {
        if (w.ring_dead) return w.parkDead(tick_ns);
        const ring = w.ring;
        const tick = tick_ns orelse {
            _ = ring.submit_and_wait(1) catch |e| switch (e) {
                error.SignalInterrupt => {},
                else => w.ringDied(e),
            };
            return .event;
        };
        _ = ring.submit() catch |e| switch (e) {
            error.SignalInterrupt => {},
            else => {
                w.ringDied(e);
                return .event;
            },
        };
        if (ring.features & linux.IORING_FEAT_EXT_ARG == 0) return w.untimedWait();
        var ts: linux.kernel_timespec = .{ .sec = @intCast(tick / std.time.ns_per_s), .nsec = @intCast(tick % std.time.ns_per_s) };
        var arg: linux.io_uring_getevents_arg = .{ .sigmask = 0, .sigmask_sz = 0, .pad = 0, .ts = @intFromPtr(&ts) };
        // The kernel reads `arg`/`ts` during this call only.
        const rc = linux.syscall6(.io_uring_enter, @as(u32, @bitCast(ring.fd)), 0, 1, linux.IORING_ENTER_GETEVENTS | linux.IORING_ENTER_EXT_ARG, @intFromPtr(&arg), @sizeOf(linux.io_uring_getevents_arg));
        return switch (linux.errno(rc)) {
            .SUCCESS, .INTR => .event,
            .TIME => .tick,
            // Pre-5.11 kernels without EXT_ARG (below the charter's floor): an untimed wait.
            .INVAL => w.untimedWait(),
            else => |e| {
                if (!w.ring_dead) {
                    log.err("journal lane {d}: io_uring_enter failed: {t} — the ring is dead; failing every in-flight op", .{ w.lane.index, e });
                    w.failAll(error.RingFailed);
                }
                return .event;
            },
        };
    }

    fn untimedWait(w: *Worker) ParkVerdict {
        _ = w.ring.submit_and_wait(1) catch |e| switch (e) {
            error.SignalInterrupt => {},
            else => w.ringDied(e),
        };
        return .event;
    }

    /// The ring is dead: nothing on it can complete, so the lane parks on
    /// the wake eventfd alone (`poll(2)` — the failure drain, never a
    /// product I/O path) until a message, a kick or the tick; every window
    /// it still receives fails at admission until the stop.
    fn parkDead(w: *Worker, tick_ns: ?u64) ParkVerdict {
        var pfd = [_]linux.pollfd{.{ .fd = w.lane.wake_fd, .events = linux.POLL.IN, .revents = 0 }};
        const timeout_ms: i32 = if (tick_ns) |t| @intCast(@min(t / std.time.ns_per_ms, std.math.maxInt(i32))) else -1;
        const rc = linux.poll(&pfd, 1, timeout_ms);
        if (linux.errno(rc) == .SUCCESS and rc == 0) return .tick;
        var count: u64 = 0;
        _ = linux.read(w.lane.wake_fd, std.mem.asBytes(&count), @sizeOf(u64));
        w.wake_consumed = true;
        return .event;
    }

    fn ringDied(w: *Worker, e: anyerror) void {
        if (w.ring_dead) return;
        log.err("journal lane {d}: io_uring_enter failed: {s} — the ring is dead; failing every in-flight op and every later window on this lane (charter §1.2: no classical fallback)", .{ w.lane.index, @errorName(e) });
        w.failAll(error.RingFailed);
    }

    /// The ring broke: every in-flight run and the barrier fail with `err`;
    /// later windows fail at admission.
    fn failAll(w: *Worker, err: Error) void {
        w.ring_dead = true;
        w.wake_armed = false;
        const now = clock.monotonicNs();
        for (w.slots.items, 0..) |slot, i| {
            if (slot == null) continue;
            var released = w.releaseSlot(@intCast(i));
            if (released.bounce) |*b| b.deinit();
            const c = &w.lane.cells[released.cell];
            noteRunError(c, err);
            c.runs_in_flight -= 1;
            w.runs_in_flight -= 1;
            if (c.runs_in_flight == 0) markLanded(c, now);
        }
        if (w.barrier_in_flight != null) w.barrierDone(err, now);
    }

    // ---- reap ----

    /// Reap every ready CQE; one clock read per pass; continuations (short
    /// writes) are re-pushed after the CQEs are consumed.
    fn reap(w: *Worker) void {
        while (true) {
            const n = w.ring.copy_cqes(&w.cqes, 0) catch |e| {
                log.err("journal lane {d}: completion queue read failed: {s} — failing every in-flight op", .{ w.lane.index, @errorName(e) });
                w.failAll(error.RingFailed);
                return;
            };
            if (n == 0) break;
            const now = clock.monotonicNs();
            var cont: [128]u32 = undefined;
            var nc: usize = 0;
            for (w.cqes[0..n]) |cqe| {
                if (cqe.user_data == ud_wake) {
                    w.wake_consumed = true;
                    w.wake_armed = false;
                    continue;
                }
                if (cqe.user_data == ud_barrier) {
                    w.barrierDone(if (cqe.res < 0) cqeError(cqe.res) else {}, now);
                    continue;
                }
                if (cqe.user_data >= w.slots.items.len or w.slots.items[@intCast(cqe.user_data)] == null) {
                    stats.cqe_orphan.fire("CQE named a slot with no in-flight run");
                    continue;
                }
                const i: u32 = @intCast(cqe.user_data);
                if (w.advanceRun(i, cqe.res, now)) {
                    cont[nc] = i;
                    nc += 1;
                }
            }
            for (cont[0..nc]) |i| {
                if (!w.pushRun(i)) w.finishRun(i, error.SubmissionQueueOverflow, now);
            }
        }
    }

    /// Apply one run's CQE. True ⇒ a short write continues and must be
    /// re-pushed; false ⇒ the run is finished (its window may have landed).
    fn advanceRun(w: *Worker, i: u32, res: i32, now: u64) bool {
        const r = &w.slots.items[i].?;
        if (res > 0) {
            r.written += @intCast(res);
            if (r.written < r.data.len) return true;
            w.finishRun(i, {}, now);
            return false;
        }
        w.finishRun(i, if (res < 0) cqeError(res) else error.ShortWrite, now);
        return false;
    }

    fn finishRun(w: *Worker, i: u32, result: Error!void, now: u64) void {
        var released = w.releaseSlot(i);
        if (released.bounce) |*b| b.deinit();
        const c = &w.lane.cells[released.cell];
        result catch |e| noteRunError(c, e);
        c.runs_in_flight -= 1;
        w.runs_in_flight -= 1;
        if (c.runs_in_flight == 0) markLanded(c, now);
    }

    // ---- stage B: the durability protocol over the registry ----

    /// Drive the one group in progress as far as it can go without a park.
    fn advanceDurability(w: *Worker) void {
        while (true) {
            switch (w.phase) {
                .idle => if (!w.formGroup()) return,
                .prefix => {
                    w.foldLatePredecessors();
                    if (w.prefix_token) |t| {
                        if (!w.lane.registry.completion_notify.firedSince(t)) return;
                        w.prefix_token = null;
                    }
                    switch (w.lane.registry.pollCompletedUpto(w.prefix_target)) {
                        .ready => w.runVerdicts(),
                        .pending => |token| {
                            w.prefix_token = token;
                            stats.prefix_waits.inc();
                            LaneStats.bump(&w.lane.stats.prefix_waits);
                            return;
                        },
                    }
                },
                .want_barrier => {
                    if (w.barrier_in_flight != null) return;
                    w.startBarrier(.strict_group);
                    w.phase = .barrier;
                    return;
                },
                .barrier => return,
                .ack => w.fanOut(),
            }
        }
    }

    /// The group = the head window once landed plus every successor that
    /// already landed; each is completed in the registry IN JOURNAL ORDER
    /// (failed ones too: the hole completes for watermark purposes), so
    /// `completed_upto` walks forward through the lane alone.
    fn formGroup(w: *Worker) bool {
        while (w.pending.items.len != 0) {
            const idx = w.pending.items[0];
            const c = &w.lane.cells[idx];
            if (!c.landed) break;
            _ = w.pending.orderedRemove(0);
            w.lane.registry.complete(w.lane.core, c.window);
            w.group.appendAssumeCapacity(idx);
        }
        if (w.group.items.len == 0) return false;
        w.prefix_target = w.lane.cells[w.group.items[w.group.items.len - 1]].window.paddedEnd();
        w.prefix_token = null;
        w.phase = .prefix;
        return true;
    }

    /// A window reserved before the group's but handed off after it holds
    /// the group's prefix: once landed it is completed and joins the group
    /// at its journal position (acks stay in journal order); un-landed, the
    /// prefix wait simply continues until its CQE.
    fn foldLatePredecessors(w: *Worker) void {
        while (w.pending.items.len != 0) {
            const idx = w.pending.items[0];
            const c = &w.lane.cells[idx];
            if (c.window.start >= w.prefix_target or !c.landed) break;
            _ = w.pending.orderedRemove(0);
            w.lane.registry.complete(w.lane.core, c.window);
            var at: usize = 0;
            while (at < w.group.items.len and w.lane.cells[w.group.items[at]].window.start < c.window.start) at += 1;
            w.group.insertAssumeCapacity(at, idx);
        }
    }

    /// Per window in journal order: Ok ⇒ the write-failure run resets, the
    /// ring accounting, an apply-hole note; Err ⇒ the rollback hook, the
    /// failure latch, a checkpoint past the permanent hole. Then the hole
    /// checkpoint for acked windows (BEFORE any survivor is acked), then the
    /// barrier decision.
    fn runVerdicts(w: *Worker) void {
        const lane = w.lane;
        var any_ok = false;
        var hole_end: ?u64 = null;
        for (w.group.items) |idx| {
            const c = &lane.cells[idx];
            if (c.first_err) |e| {
                log.warn("journal lane {d}: window [{d}, {d}) write failed: {s} — rolling back {d} entries", .{ lane.index, c.window.start, c.window.end(), @errorName(e), c.entries });
                stats.write_failures.inc();
                if (lane.hooks.rollback) |f| {
                    f(lane.hooks.ctx, SeqSpan.stamped(c.seq_base, c.window.len), c.window);
                    stats.rollbacks.inc();
                }
                w.noteJournalFailure();
                // The reserved range is a PERMANENT hole (same-page successors
                // of a dead chain are undiscoverable): checkpoint past it.
                if (!w.checkpointPast(c.window.paddedEnd())) {
                    log.err("journal lane {d}: post-failure checkpoint could not drain the journal hole at {d} (volume escalating)", .{ lane.index, c.window.paddedEnd() });
                    w.noteJournalFailure();
                }
                c.verdict = e;
            } else {
                lane.latch.noteSuccess(.journal_write);
                lane.ring_stats.noteEntriesWritten(c.entries, c.bytes);
                lane.ring_stats.notePadWritten(c.window.pad);
                if (c.apply_holes) hole_end = @max(hole_end orelse 0, c.window.paddedEnd());
                any_ok = true;
                c.verdict = {};
            }
        }
        w.group_hole_err = null;
        if (hole_end) |end| {
            if (!w.checkpointPast(end)) {
                log.err("journal lane {d}: post-isolation checkpoint could not cover the batch hole at {d} (volume escalating; failing the survivors loud rather than acking unreachable entries)", .{ lane.index, end });
                w.noteJournalFailure();
                w.group_hole_err = error.HoleCheckpointFailed;
            }
        }
        w.group_barrier_out = {};
        if (w.group_hole_err == null and any_ok and lane.cadence.strict) {
            w.phase = .want_barrier;
            return;
        }
        if (w.group_hole_err == null and any_ok) w.setNeedsFlush(clock.monotonicNs());
        w.phase = .ack;
    }

    fn checkpointPast(w: *Worker, padded_end: u64) bool {
        const f = w.lane.hooks.checkpoint_past orelse return true;
        return f(w.lane.hooks.ctx, padded_end);
    }

    fn noteJournalFailure(w: *Worker) void {
        if (w.lane.latch.noteFailure(.journal_write)) stats.volumes_failed.inc();
    }

    /// Acks in journal order: a member's own write error, else the hole
    /// checkpoint's, else the group barrier's.
    fn fanOut(w: *Worker) void {
        const now = clock.monotonicNs();
        for (w.group.items) |idx| {
            const c = &w.lane.cells[idx];
            const result: Error!void = if (c.verdict) |_| (if (w.group_hole_err) |h| h else w.group_barrier_out) else |e| e;
            w.publish(c, result, now);
        }
        w.group.clearRetainingCapacity();
        w.phase = .idle;
    }

    fn publish(w: *Worker, c: *Cell, result: Error!void, now: u64) void {
        w.ack_seq += 1;
        LaneStats.bump(&w.lane.stats.acks);
        c.outcome = .{ .result = result, .ack_seq = w.ack_seq, .landed_ns = c.landed_ns, .acked_ns = now };
        // Release publishes `outcome`; the ticket's acquire load pairs with it.
        c.state.store(Cell.done, .release);
        futexWakeAll(&c.state);
    }

    // ---- barriers ----

    /// Coalesced cadence: the first ack after a barrier schedules the next
    /// at `last + interval` (never sooner than now); an fsync request pulls
    /// it to now.
    fn setNeedsFlush(w: *Worker, now: u64) void {
        if (w.needs_flush) return;
        w.needs_flush = true;
        w.flush_due_ns = @max(now, w.last_barrier_ns + w.lane.cadence.interval_ns);
    }

    fn maybeStartCoalescedBarrier(w: *Worker, now: u64) void {
        if (!w.needs_flush or w.barrier_in_flight != null or now < w.flush_due_ns) return;
        w.startBarrier(.coalesced);
    }

    /// ONE `FSYNC|DATASYNC` SQE on the lane's ring. Never `fsync(2)`:
    /// O_DIRECT bypasses the page cache, not the device's volatile write
    /// cache, and a blocking syscall here would stall every CQE behind it.
    fn startBarrier(w: *Worker, kind: BarrierKind) void {
        w.barriers_started += 1;
        w.needs_flush = false;
        w.barrier_in_flight = kind;
        switch (kind) {
            .strict_group => {
                stats.barriers_strict.inc();
                LaneStats.bump(&w.lane.stats.barriers_strict);
            },
            .coalesced => {
                stats.barriers_coalesced.inc();
                LaneStats.bump(&w.lane.stats.barriers_coalesced);
            },
        }
        if (w.ring_dead) return w.barrierDone(error.RingFailed, clock.monotonicNs());
        const sqe = w.getSqe() orelse return w.barrierDone(error.SubmissionQueueOverflow, clock.monotonicNs());
        sqe.prep_fsync(w.sqeFd(), linux.IORING_FSYNC_DATASYNC);
        w.markFixed(sqe);
        sqe.user_data = ud_barrier;
    }

    /// The barrier's outcome: the latch's barrier class, the strict group's
    /// ack gate, and every fsync request whose ticket it satisfies.
    fn barrierDone(w: *Worker, result: Error!void, now: u64) void {
        const kind = w.barrier_in_flight.?;
        w.barrier_in_flight = null;
        w.barriers_done += 1;
        w.last_barrier_ns = now;
        if (result) |_| {
            w.lane.latch.noteSuccess(.barrier);
        } else |e| {
            log.warn("journal lane {d}: barrier failed: {s}", .{ w.lane.index, @errorName(e) });
            stats.barrier_failures.inc();
            LaneStats.bump(&w.lane.stats.barrier_failures);
            if (w.lane.latch.noteFailure(.barrier)) stats.volumes_failed.inc();
        }
        if (kind == .strict_group) {
            w.group_barrier_out = result;
            w.phase = .ack;
        }
        var i: usize = 0;
        while (i < w.fsync_waiters.items.len) {
            const idx = w.fsync_waiters.items[i];
            const c = &w.lane.cells[idx];
            if (c.need_barrier > w.barriers_done) {
                i += 1;
                continue;
            }
            _ = w.fsync_waiters.swapRemove(i);
            w.publish(c, result, now);
        }
    }
};

fn noteRunError(c: *Cell, err: Error) void {
    if (c.first_err == null) c.first_err = err;
}

fn markLanded(c: *Cell, now: u64) void {
    c.landed = true;
    c.landed_ns = now;
}

/// The hold seam: park-adjacent so a test can place N publishes between
/// two lane passes.
fn holdIfArmed() void {
    if (test_seams.hold_before_park.load(.acquire) == 0) return;
    test_seams.held.store(1, .release);
    futexWakeAll(&test_seams.held);
    while (test_seams.hold_before_park.load(.acquire) != 0) futexWait(&test_seams.hold_before_park, 1);
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

    fn submit(self: *Harness, r: jc.InflightRegistry.Registered, ops: []const WriteOp) Error!Ticket {
        return self.lane.?.submitWriteBatch(.{ .window = r.res, .seq_base = r.seq_base, .ops = ops, .entries = 1, .bytes = r.res.len });
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

test "the flush cadence knob: default 50 ms coalesced; 0 is strict; junk keeps the default loudly" {
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const d = resolveFlushCadence(&env);
    try testing.expect(!d.strict);
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
}

test "the lane thread is sqz-jrnl<N> in /proc, counted live, and joins owing nothing" {
    const h = try Harness.init(testing.allocator, "name");
    defer h.deinit();
    const live_before = stats.lanes_live.load();
    const spawned_before = stats.lanes_spawned.load();
    try h.spawnLane(.{}, null, 0);
    const lane = h.lane.?;
    spinUntil(struct {
        l: *JournalLane,
        fn ready(s: @This()) bool {
            return s.l.threadId() != 0;
        }
    }{ .l = lane });
    var cbuf: [32]u8 = undefined;
    try testing.expectEqualStrings("sqz-jrnl7", try commOf(lane.threadId(), &cbuf));
    try testing.expectEqual(live_before + 1, stats.lanes_live.load());
    try testing.expectEqual(spawned_before + 1, stats.lanes_spawned.load());
    try testing.expect(lane.mode.isDirect());
    // An idle lane owes nothing, so its ONE park is untimed: three backstop
    // periods later it is still inside it (no park completed, no tick).
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
    std.debug.print("\nmeasurement: coalesced, {d} sequential windows: parks {d} ({d} per window), wake writes {d}, elided {d}\n", .{ n, snap.parks, snap.parks / n, snap.wake_writes, snap.wake_elided });
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
    std.debug.print("\nmeasurement: strict, 5 sequential windows: parks {d} ({d} per window incl. the barrier), wake writes {d}, elided {d}\n", .{ snap.parks, snap.parks / 5, snap.wake_writes, snap.wake_elided });
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

    // A 1 ms interval: the lane's own tick barriers after an ack with no request.
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
    const t = try lane.submitWriteBatch(.{ .window = r.res, .seq_base = r.seq_base, .ops = ops, .entries = 1, .bytes = r.res.len, .apply_holes = true });
    try testing.expectError(error.HoleCheckpointFailed, t.wait().result);
    try testing.expectEqualSlices(u64, &.{r.res.paddedEnd()}, rl.checkpoints.items);
    try testing.expectEqual(@as(u64, 0), lane.snapshot().barriers_strict);
    try testing.expectEqual(@as(u32, 1), lane.latch.journal_failures.load(.acquire));
    try testing.expectEqual(@as(usize, 0), rl.calls.items.len);

    rl.checkpoint_ok = true;
    const r2 = try h.reserve(500);
    const ops2 = try h.build(r2, 6, 500, 0x66);
    const t2 = try lane.submitWriteBatch(.{ .window = r2.res, .seq_base = r2.seq_base, .ops = ops2, .entries = 1, .bytes = r2.res.len, .apply_holes = true });
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

test "wake coalescing: N publishes between two lane passes cost ONE eventfd write" {
    const h = try Harness.init(testing.allocator, "coalesce");
    defer h.deinit();
    try h.spawnLane(.{}, "600000", 0);
    const lane = h.lane.?;
    // Warm up: after this ack the lane has consumed its wake and disarmed.
    const warm = try h.append(1, 300, 0x01);
    try warm.ticket.wait().result;

    test_seams.held.store(0, .release);
    test_seams.hold_before_park.store(1, .release);
    var tickets: [9]Ticket = undefined;
    tickets[0] = (try h.append(2, 300, 0x02)).ticket;
    while (test_seams.held.load(.acquire) == 0) futexWait(&test_seams.held, 0);
    // The lane is held between its scan and its park: every publish from
    // here rides one write.
    const w0 = lane.snapshot().wake_writes;
    const e0 = lane.snapshot().wake_elided;
    var k: usize = 1;
    while (k < tickets.len) : (k += 1) tickets[k] = (try h.append(@intCast(2 + k), 300, @intCast(k))).ticket;
    const held_publishes: u64 = tickets.len - 1;
    try testing.expectEqual(w0 + 1, lane.snapshot().wake_writes);
    try testing.expectEqual(e0 + held_publishes - 1, lane.snapshot().wake_elided);
    test_seams.hold_before_park.store(0, .release);
    futexWakeAll(&test_seams.hold_before_park);
    for (tickets) |t| try t.wait().result;
    test_seams.held.store(0, .release);
    std.debug.print("\nmeasurement: {d} publishes between two lane passes: eventfd writes {d}, elided {d}\n", .{ held_publishes, lane.snapshot().wake_writes - w0, lane.snapshot().wake_elided - e0 });
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
    std.debug.print("\nmeasurement: strict storm, {d} threads × {d} windows: parks {d}, barriers {d} (groups), wake writes {d}, elided {d}, tick rescues {d}\n", .{ threads_n, per_thread, snap.parks, snap.barriers_strict, snap.wake_writes, snap.wake_elided, snap.tick_rescues });
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

test "the stats JSON names every lane counter" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    for ([_][]const u8{ "\"journal_lanes_spawned\":", "\"journal_lanes_live\":", "\"journal_ring_lane_writes\":", "\"journal_lane_write_failures\":", "\"journal_lane_rollbacks\":", "\"journal_lane_barriers_strict\":", "\"journal_lane_barriers_coalesced\":", "\"journal_lane_barrier_failures\":", "\"journal_lane_wake_writes\":", "\"journal_lane_wake_elided\":", "\"journal_lane_parks\":", "\"journal_lane_park_tick_rescues\":", "\"journal_lane_prefix_waits\":", "\"journal_lane_queue_full\":", "\"journal_lane_volumes_failed\":", "\"journal_lane_wake_arm_failed\":", "\"journal_lane_cqe_orphan\":" }) |needle| {
        try testing.expect(std.mem.indexOf(u8, s, needle) != null);
    }
}
