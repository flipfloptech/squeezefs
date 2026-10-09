//! The checkpoint CYCLE protocol, the DUR-3 barrier tickets and the task
//! shape (charter §5.3 "Checkpoint", "Space", "fsync"; survey §2.10, §2.10.1
//! the 14-step table, §3.5, §3.9; `docs/porting/kv_journal_ledger.md` §5).
//! The pure half (derivations, the decision, the tape) is `checkpoint.zig`.
//!
//! ## One cycle (the survey's pinned order)
//!
//! `h = head` → collect the dirty set (every node with a floor, not
//! superseded; every leaf dirtied from here is the NEXT cycle's) → order it
//! oldest floor first, the oldest keep-exempt (its SMO may take the class's
//! kept space: it is the node whose flush moves the tail) → flush each once
//! through the `FlushSink` (freeze + take the floor under the node lock,
//! append OUTSIDE it; a frame that does not fit goes to the `SmoSeam`;
//! `JournalReserveExhausted` / `NoSpace` / `GrantExhausted` are
//! skip-and-defer with the floor restored) → the heap-full posture →
//! the dirty bitmap pages at generation `ckpt_seq` → **barrier #1** (appends,
//! pages, completed journal writes and the PREVIOUS record are durable; the
//! reclaim queue drains inside) → `tail = min(h, min in-flight start, dying
//! floors, every live dirty floor)` — every input an ENTRY START → the ledger
//! slot `seq % 32` (on failure the dying floors fold back and the previous
//! record stays authoritative) → `checkpoint_seq = ckpt_seq`, `retire_seq =
//! ckpt_seq + 1`, `pending_reclaim.push((tail, barrier_push_epoch))` →
//! `barrier_now` ⇒ **barrier #2** + the wedged-tail audit → the heap-full
//! merge sweep (bounded to one tick period, after the barrier so its count
//! reads the SAME tail a post-cycle census reads).
//!
//! ## DUR-3: `reusable_upto` moves on a completed barrier that STARTED after
//! the record's write
//!
//! `syncDevice` takes a start ticket BEFORE the device op and publishes it as
//! `barrier_durable` on success. A record is pushed with epoch = tickets
//! already started; barriers in flight cannot have flushed it, so only a
//! barrier whose ticket exceeds the epoch drains it — then ALL THREE
//! watermarks move on one clock: the ring's `reusable_upto` (waking admission
//! parkers), the allocator's coverage clock (pending-free releases ride the
//! TAIL, never a record's durability) and the cache's elision tail.
//!
//! ## The progress theorem (R10)
//!
//! A minimal cycle consumes ZERO ring bytes (appends go to node extents, the
//! ledger and the bitmap to their own); only SMO records draw the checkpoint
//! reserve and they never park — so the cycle always completes and nothing
//! blocks on ring space while holding a node lock.
//!
//! ## The task
//!
//! `CheckpointTask` is `runOnce` behind a Notify wait with a FIXED cadence
//! deadline read off the CLOCK (a wake past the deadline IS the deadline:
//! under a storm whose every pass re-arms the threshold wake the sleep arm
//! was never reached and no cycle ever ran — the ring filled and a parked
//! committer escalated to fail-stop); a wake carrying the park mark is
//! promoted to a cadence tick. The executor (rung 3) will run `runOnce`;
//! until then `runUntilStopped(io)` is the body on a caller's thread with
//! the stop latch, which a test drives under `std.testing.io`.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const thread_name = @import("../core/thread_name.zig");
const sync = @import("../sync/sync.zig");
const jc = @import("journal_core.zig");
const journal = @import("journal.zig");
const ledger = @import("ledger.zig");
const node_cache = @import("node_cache.zig");
const alloc_core = @import("alloc_core.zig");
const alloc_ext = @import("alloc_ext.zig");
const heap_law = @import("heap_law.zig");
const inos = @import("inos.zig");
const node_seq = @import("node_seq.zig");
const tree = @import("tree.zig");
const ckpt = @import("checkpoint.zig");

pub const CachedNode = node_cache.CachedNode;
pub const NodeCache = node_cache.NodeCache;
pub const TreeRoot = ledger.TreeRoot;
pub const Decision = ckpt.Decision;
pub const DrainBudget = ckpt.DrainBudget;
pub const CycleTape = ckpt.CycleTape;

const ns_per_ms: u64 = std.time.ns_per_ms;

pub const IoError = error{Io};

/// The cycle's classes: the device (`Io`, latched per the lattice), a record
/// that cannot encode or a protocol inconsistency (`Corrupt`), allocation,
/// and the wedge terminal.
pub const CycleError = error{ Io, Corrupt, OutOfMemory, VolumeFailed };

// ---------------------------------------------------------------------------
// The seams: device effects and the structural task, as typed function-
// pointer structs the bindings (and the tests) satisfy.
// ---------------------------------------------------------------------------

/// The device barrier (`fdatasync` of the metadata device). In tests a counter.
pub const BarrierSink = struct {
    ctx: *anyopaque,
    barrier: *const fn (ctx: *anyopaque) IoError!void,
};

/// Write one 4 KiB ledger slot image at slot `slot` (the binding adds the
/// ledger base: `ledger.slotOffset`).
pub const LedgerSink = struct {
    ctx: *anyopaque,
    writeSlot: *const fn (ctx: *anyopaque, slot: u64, image: *const [ledger.ROOT_LEDGER_SLOT_LEN]u8) IoError!void,
};

/// Write one allocation-bitmap page image at `offset` relative to the bitmap
/// region base (`alloc_ext.PageWrite.offset`).
pub const PageSink = struct {
    ctx: *anyopaque,
    writePage: *const fn (ctx: *anyopaque, offset: u64, image: *const [alloc_ext.page_len]u8) IoError!void,
};

/// The ledger record's identity inputs.
pub const RecordSource = struct {
    ctx: *anyopaque,
    /// Every ledger-rooted tree's root `(id, addr, node_seq)`, appended to `out`.
    treeRoots: *const fn (ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList(TreeRoot)) Allocator.Error!void,
    next_ino: *const inos.InoWatermark,
    /// Every tree shares ONE mint, so any handle's snapshot is the volume's
    /// watermark (captured AFTER the flush loop, whose SMOs mint).
    node_seq: *const node_seq.NodeSeqHandle,
    /// The stamp image this record carries (every volume is a stamped set
    /// member): the stored membership with the LIVE guest-cursor cells
    /// folded in (charter §5.4; `routing/stamp_cell.zig`). The slices the
    /// stamp borrows stay valid until the source's next call — both record
    /// sites run under the volume's SMO lock, one at a time.
    membershipStamp: *const fn (ctx: *anyopaque) ledger.MembershipStamp,
    /// Publish every guest slot tree's root whose durable copy in tree 0
    /// is stale (one checkpoint-class control entry each), after the
    /// flush pass and before the tail rule (C4b).
    publishGuestRoots: *const fn (ctx: *anyopaque) CycleError!void,
    /// The symmetric plane's appender pages (charter §5.5: the page IS the
    /// appender's ledger record — one write per checkpoint), after the
    /// ledger record landed and its bookkeeping moved; `cache_clean` is
    /// the cycle's fixpoint (no dirty node, no dying floor) — the region
    /// tail law's input. Null on a single-writer volume.
    appenderPages: ?*const fn (ctx: *anyopaque, record: *const ledger.LedgerRecord, cache_clean: bool) IoError!void = null,
};

pub const FlushFailure = enum { none, io, corrupt, out_of_memory };

/// One node's flush outcome.
pub const FlushDisposition = enum {
    /// The floor was taken and there was nothing to write (threshold
    /// maintenance already appended the bytes; barrier #1 covers them).
    discharged,
    /// The frozen delta was appended in place.
    appended,
    /// The frame did not fit: the structural seam replaced the node.
    replaced,
    /// The node was superseded between collection and its lock (its floor
    /// travelled with the dying floors).
    skipped,
    /// SMO reserve exhausted: floor restored, next cycle (never a park).
    deferred_reserve,
    /// A threshold SMO found no pending-free FIFO headroom (the §4.7
    /// pressure valve): the drain forces a barriered cycle and retries. The
    /// checkpoint pass's SMOs run forced and never answer this.
    deferred_pending_free,
    /// The allocator answered `NoSpace`: the SPACE standstill class.
    deferred_space,
    /// The lessee's extent grant is exhausted: the manager dependency.
    deferred_grant,
    /// The structural seam declined (no structural task bound): the floor
    /// stays and clamps the tail — a node that fills with no SMO behind it is
    /// the wedge class the audit judges.
    declined,
    /// `failure` names the class; the cycle aborts.
    failed,
};

pub const FlushResult = struct {
    disposition: FlushDisposition,
    /// Fresh images the structural seam wrote (the per-image unit's count).
    images: u64 = 0,
    failure: FlushFailure = .none,
};

pub const FlushMode = enum {
    /// The cycle's flush pass: freeze AND TAKE the floor under the lock (the
    /// coming barrier covers the bytes); forced retirement.
    checkpoint,
    /// The threshold drain: freeze + append only — the floor STAYS until a
    /// checkpoint's barrier covers the appended bytes; threshold retirement
    /// may be refused at the FIFO cap.
    threshold,
};

pub const FlushOpts = struct {
    mode: FlushMode,
    /// The ring's oldest node: its SMO entry may take the class's kept space.
    keep_exempt: bool = false,
};

/// Flush one node: freeze under its lock, append outside it, or hand a full
/// node to the structural seam.
pub const FlushSink = struct {
    ctx: *anyopaque,
    flush: *const fn (ctx: *anyopaque, node: *CachedNode, opts: FlushOpts) FlushResult,
};

pub const SmoOutcome = union(enum) {
    /// Successors published, the old node retired; `images` fresh extents written.
    replaced: struct { images: u64 },
    journal_reserve_exhausted,
    /// Threshold retirement refused at the FIFO cap (never under forced retirement).
    pending_free_full,
    no_space,
    grant_exhausted,
    /// No structural task is bound (`SmoSeam.none`).
    declined,
    failed: FlushFailure,
};

/// The threshold maintenance queue the task drains — the trees' lock-free
/// `tree.MaintenanceQueue`s, which the commit path fills (A10) and the
/// backend round-robins over; one queue in the portable tests. ONE queue
/// family, one drain: a second queue the commit path never fills left the
/// writeback threshold inert on every mounted volume.
pub const MaintenanceSource = struct {
    ctx: *anyopaque,
    /// The next queued address, or null when every queue is empty.
    pop: *const fn (ctx: *anyopaque) ?u64,
    /// Whether anything is queued (the task's re-arm test).
    pending: *const fn (ctx: *anyopaque) bool,
    /// Queue `addr` again for the next drain (a reserve deferral's retry).
    requeue: *const fn (ctx: *anyopaque, addr: u64) void,

    /// One `tree.MaintenanceQueue` as the source (tests, a one-tree volume).
    pub fn ofQueue(q: *tree.MaintenanceQueue) MaintenanceSource {
        return .{ .ctx = q, .pop = queuePop, .pending = queuePending, .requeue = queueRequeue };
    }

    fn queuePop(ctx: *anyopaque) ?u64 {
        const q: *tree.MaintenanceQueue = @ptrCast(@alignCast(ctx));
        return q.pop();
    }

    fn queuePending(ctx: *anyopaque) bool {
        const q: *tree.MaintenanceQueue = @ptrCast(@alignCast(ctx));
        return !q.isEmpty();
    }

    fn queueRequeue(ctx: *anyopaque, addr: u64) void {
        const q: *tree.MaintenanceQueue = @ptrCast(@alignCast(ctx));
        if (!q.push(addr)) tree.stats.meta_kv_maintenance_queue_full.inc();
    }
};

/// The admission parkers' futex Notify with the `Io` its wake needs (a
/// barrier is never a hot path).
pub const SpaceWake = struct {
    notify: *sync.Notify,
    io: Io,
};

/// The heap-full sweep's report.
pub const SweepReport = struct {
    merges: u64 = 0,
    interior_merges: u64 = 0,
    root_collapses: u64 = 0,
    lap_complete: bool = true,
    /// A candidate the compaction floor refused (the backlog stands).
    space_refused: bool = false,
    candidates: u64 = 0,
};

/// The structural task's hook — satisfied by `smo.zig` / `leaf_merge.zig`.
pub const SmoSeam = struct {
    ctx: *anyopaque,
    /// Compact/split `node` whose frozen delta does not fit its extent (the
    /// structural half of `maintain_node`). `forced_retirement`: the flush
    /// pass's SMOs park their old extent in the unbounded overflow at the
    /// FIFO cap instead of refusing (the §4.7 cycle-break).
    maintain: *const fn (ctx: *anyopaque, node: *CachedNode, opts: MaintainOpts) SmoOutcome,
    /// Merge underfull adjacent leaves within `deadline_ns` (cursor-resumed).
    mergeUnderfull: *const fn (ctx: *anyopaque, deadline_ns: u64) SweepReport,

    pub const MaintainOpts = struct { keep_exempt: bool, forced_retirement: bool };

    /// No structural task bound (bring-up, tests): a full node is declined
    /// and the sweep merges nothing.
    pub const none: SmoSeam = .{ .ctx = &none_ctx, .maintain = declineMaintain, .mergeUnderfull = noSweep };
    /// The inert context the seam-less hooks ignore.
    var none_ctx: u8 = 0;

    fn declineMaintain(_: *anyopaque, _: *CachedNode, _: MaintainOpts) SmoOutcome {
        return .declined;
    }

    fn noSweep(_: *anyopaque, _: u64) SweepReport {
        return .{};
    }
};

/// The product `FlushSink` over the node cache: the freeze under the node's
/// write lock (`flushFreezeLocked` takes the floor on the checkpoint pass;
/// `freezeLocked` leaves it on the threshold drain), `appendFrozen` outside
/// it, the structural seam when the frame does not fit. A deferral or a
/// failure restores a taken floor so the tail keeps respecting the records.
pub const CacheFlusher = struct {
    cache: *NodeCache,
    smo: SmoSeam,

    pub fn sink(self: *CacheFlusher) FlushSink {
        return .{ .ctx = self, .flush = flush };
    }

    fn freezeFailure(e: node_cache.FreezeError) FlushResult {
        return switch (e) {
            error.FreezeRefusedSuperseded => .{ .disposition = .skipped },
            error.OutOfMemory => .{ .disposition = .failed, .failure = .out_of_memory },
            // The remaining protocol refusals and the encoder's classes leave
            // the floor in place (taken only after the freeze) — the pass
            // fails loud and retries next tick.
            error.FreezeRefusedNotDirty, error.FreezeRefusedAlreadyFreezing, error.InjectedFreezeFailure, error.ValueTooLarge, error.Corrupt => .{ .disposition = .failed, .failure = .corrupt },
        };
    }

    fn flush(ctx: *anyopaque, n: *CachedNode, opts: FlushOpts) FlushResult {
        const self: *CacheFlusher = @ptrCast(@alignCast(ctx));
        const io = self.cache.io;
        const layout = self.cache.config().layout;
        // The lock window: freeze (or get the parked delta back); the
        // checkpoint pass takes the floor here, in the same window.
        var taken: ?u64 = null;
        const frozen = blk: {
            const d = n.lockWrite(io);
            defer n.unlockWrite(io);
            switch (opts.mode) {
                .checkpoint => {
                    const ff = n.flushFreezeLocked(d, layout) catch |e| return freezeFailure(e);
                    taken = ff.floor;
                    break :blk ff.frozen;
                },
                .threshold => break :blk (n.freezeLocked(d, layout) catch |e| return freezeFailure(e)) != null,
            }
        };
        if (!frozen) return .{ .disposition = .discharged };
        const appended = self.cache.appendFrozen(n) catch |e| {
            if (taken) |f| n.restoreDirtyFloor(f);
            return .{ .disposition = .failed, .failure = switch (e) {
                error.OutOfMemory => .out_of_memory,
                error.Io => .io,
                else => .corrupt,
            } };
        };
        if (appended) return .{ .disposition = .appended };
        const out = self.smo.maintain(self.smo.ctx, n, .{ .keep_exempt = opts.keep_exempt, .forced_retirement = opts.mode == .checkpoint });
        if (out == .replaced) return .{ .disposition = .replaced, .images = out.replaced.images };
        // Every other answer leaves the node unflushed: a taken floor goes back.
        if (taken) |f| n.restoreDirtyFloor(f);
        return switch (out) {
            .replaced => unreachable,
            .journal_reserve_exhausted => .{ .disposition = .deferred_reserve },
            .pending_free_full => .{ .disposition = .deferred_pending_free },
            .no_space => .{ .disposition = .deferred_space },
            .grant_exhausted => .{ .disposition = .deferred_grant },
            .declined => .{ .disposition = .declined },
            .failed => |f| .{ .disposition = .failed, .failure = f },
        };
    }
};

fn failureError(f: FlushFailure) CycleError {
    return switch (f) {
        .io => error.Io,
        .corrupt => error.Corrupt,
        .out_of_memory => error.OutOfMemory,
        .none => unreachable,
    };
}

// ---------------------------------------------------------------------------
// The per-volume checkpointer.
// ---------------------------------------------------------------------------

pub const Config = struct {
    /// The volume's name in log lines.
    volume: []const u8,
    max_age_ms: u64 = ckpt.CHECKPOINT_MAX_AGE_MS,
    /// 0 = strict (the lane barriers per group; the tick still runs at 100 ms).
    flush_interval_ms: u64 = ckpt.DEFAULT_FLUSH_INTERVAL_MS,
    /// `checkpoint.resolveMaxDirtyNodes` at open.
    max_dirty_nodes: u64,
    /// A `SYMMETRIC_PLANE` member: the anticipating trigger measured from
    /// the last collection. A one-member forest keeps the shipped law
    /// verbatim.
    forest: bool = false,
    /// The mount's first seq: `alloc_ext.resumeCheckpointSeq(ledger.seq, alloc.resumeGeneration())`.
    checkpoint_seq: u64,
    /// The mounted record's `journal_tail_seq`.
    last_ledger_tail: u64,
};

pub const Deps = struct {
    core: *jc.JournalCore,
    inflight: *jc.InflightRegistry,
    cache: *NodeCache,
    alloc: *alloc_ext.ExtentAllocator,
    /// Shared with the journal lane: barrier failures and the wedge latch here.
    latch: *journal.FailureLatch,
    flush: FlushSink,
    barrier: BarrierSink,
    /// The fixed ledger's slot writer; null = the PAGE-AS-LEDGER posture
    /// of a JOINED appender (charter §5.5 "the page IS the appender's
    /// ledger record"): the record's durable home is `record.appenderPages`
    /// alone, which must then be bound.
    ledger: ?LedgerSink,
    /// The allocator bitmap's page writer; null = a joiner, whose
    /// allocator is a grant mirror with no bitmap home (its dirty set is
    /// discharged each cycle without a write: the deltas in its ring and
    /// the manager's grant record are the homes).
    pages: ?PageSink,
    record: RecordSource,
    smo: SmoSeam,
    maintenance: MaintenanceSource,
};

/// The per-volume checkpoint state and protocol. SERIALIZED: every cycle,
/// tick, drain and cover loop runs under the volume's structural (SMO)
/// context — the task loop owns it; a direct caller (`checkpointNow`, the
/// lane's hole checkpoint, the mount's bring-up cover) holds that context
/// for the call. The barrier and the admission-parker wake are the only
/// faces another thread touches concurrently (`syncDevice` is an fsync
/// caller's too) and they are atomics + the reclaim queue's own lock.
pub const Checkpointer = struct {
    gpa: Allocator,
    cfg: Config,
    deps: Deps,

    // ---- the record's state ----
    checkpoint_seq: std.atomic.Value(u64),
    last_ledger_tail: std.atomic.Value(u64),
    /// `ckpt_seq + 1`: the historical retire tag free-record values carry.
    retire_seq: std.atomic.Value(u64),
    /// DUR-3 tickets: barriers started / the highest completed start.
    barrier_starts: std.atomic.Value(u64) = .init(0),
    barrier_durable: std.atomic.Value(u64) = .init(0),
    reclaim: ledger.ReclaimQueue,
    /// THE admission-parker wake: the conveyor's Σ-admission parks on this
    /// Notify and `afterDurableBarrier` is its one notifier, whichever
    /// thread's barrier moved `reusable_upto` (the task's, the lane's, a
    /// committer's space retry) — so the park's tick is a lost-wake
    /// backstop, never the transport. Null until the backend binds it.
    space_wake: ?SpaceWake = null,

    // ---- the cadence's references ----
    /// The last cycle's END (the flat age law's reference).
    last_cycle_end_ns: std.atomic.Value(u64),
    /// The last cycle's COLLECTION (the forest age law's reference).
    collected_ns: std.atomic.Value(u64),

    // ---- the term machinery ----
    terms: ckpt.CycleTermWindow = .{},
    lates: ckpt.CycleTermWindow = .{},
    barriers: ckpt.CycleTermWindow = .{},
    node_units: ckpt.CycleTermWindow = .{},
    image_units: ckpt.CycleTermWindow = .{},
    /// The decision that fired the running cycle (consumed by its term fold).
    decision_late_ns: u64 = 0,
    decision_tick_ns: u64 = 0,
    decision_at_ns: u64 = 0,
    decision_words: CycleTape = .{},
    tape_lock: jc.SpinLock = .{},
    last_cycle: CycleTape = .{},
    /// `.stats` faces: `meta_kv_checkpoint_{term,late_max,barrier,trigger,projected}_ms`, `_{node,image}_unit_ns`.
    term_ms: counters.Gauge = .{},
    late_max_ms: counters.Gauge = .{},
    barrier_ms: counters.Gauge = .{},
    node_unit_ns: counters.Gauge = .{},
    image_unit_ns: counters.Gauge = .{},
    trigger_ms: counters.Gauge = .{},
    projected_ms: counters.Gauge = .{},
    /// `reusable_upto`: the ring-reclamation watermark as the last barrier left it.
    reusable_upto: counters.Gauge = .{},
    /// `meta_kv_dirty_nodes`: the dirty count as the cadence tick last read it
    /// (the stats faces and the tape read this word, never a cache walk).
    dirty_nodes: counters.Gauge = .{},

    // ---- space ----
    audit: alloc_core.SpaceAudit = .{},
    posture: heap_law.HeapFullPosture = .{},
    space_retry: heap_law.SpaceRetryHold = .{},
    merge_backlog: bool = false,

    // ---- the threshold drain ----
    /// The addr whose last maintenance item deferred for the reserve: a second
    /// deferral in a row leaves it to the cadence's flush pass (no spin).
    reserve_deferred_addr: ?u64 = null,

    pub fn init(gpa: Allocator, cfg: Config, deps: Deps) Checkpointer {
        // The record needs ONE durable home: the ledger slot or the page.
        std.debug.assert(deps.ledger != null or deps.record.appenderPages != null);
        const now = clock.monotonicNs();
        return .{
            .gpa = gpa,
            .cfg = cfg,
            .deps = deps,
            .checkpoint_seq = .init(cfg.checkpoint_seq),
            .last_ledger_tail = .init(cfg.last_ledger_tail),
            .retire_seq = .init(cfg.checkpoint_seq + 1),
            .reclaim = ledger.ReclaimQueue.init(gpa),
            .last_cycle_end_ns = .init(now),
            .collected_ns = .init(now),
        };
    }

    pub fn deinit(self: *Checkpointer) void {
        self.reclaim.deinit();
        self.* = undefined;
    }

    // ---- faces ----

    pub fn checkpointSeq(self: *const Checkpointer) u64 {
        return self.checkpoint_seq.load(.acquire);
    }

    pub fn lastLedgerTail(self: *const Checkpointer) u64 {
        return self.last_ledger_tail.load(.acquire);
    }

    pub fn retireSeq(self: *const Checkpointer) u64 {
        return self.retire_seq.load(.acquire);
    }

    pub fn isFailed(self: *const Checkpointer) bool {
        return self.deps.latch.isFailed();
    }

    pub fn lastCycle(self: *Checkpointer) CycleTape {
        self.tape_lock.lock();
        defer self.tape_lock.unlock();
        return self.last_cycle;
    }

    /// The DUR-3 push epoch: barriers already started, none of which can
    /// cover a record written from now on. Acquire pairs with the ticket's
    /// acq_rel fetchAdd in `syncDevice`.
    pub fn barrierPushEpoch(self: *const Checkpointer) u64 {
        return self.barrier_starts.load(.acquire);
    }

    /// The committers' space-retry hold (the sweep yields while one is held).
    pub fn spaceRetryHold(self: *Checkpointer) *heap_law.SpaceRetryHold {
        return &self.space_retry;
    }

    // ---- the maintenance queue ----

    /// Whether the trees' queues hold a node for the drain.
    pub fn maintenancePending(self: *const Checkpointer) bool {
        return self.deps.maintenance.pending(self.deps.maintenance.ctx);
    }

    // ---- the barrier (DUR-3) ----

    /// The checkpointer's own device barrier through the sink: a DUR-3
    /// ticket around it; a failure counts toward the FAILED lattice (three
    /// in a row latch) and covers nothing. The FENCE rung (a
    /// reservation-conflict errno at ANY barrier latches FAILED at once,
    /// charter §5.7) is classified by the device sink's typed outcome
    /// BEFORE it narrows to `IoError` (`device_loader.DeviceLoader.latch`,
    /// the one funnel of every checkpoint-side write and barrier); a
    /// fenced latch reads FAILED here already, so the generic count below
    /// never undoes it.
    pub fn syncDevice(self: *Checkpointer) IoError!void {
        ckpt.stats.meta_device_syncs.inc();
        const started_at = self.barrierStarted();
        const t0 = clock.monotonicNs();
        self.deps.barrier.barrier(self.deps.barrier.ctx) catch |e| {
            _ = self.deps.latch.noteFailure(.barrier);
            return e;
        };
        self.deps.latch.noteSuccess(.barrier);
        ckpt.stats.meta_barrier.record(clock.monotonicNs() -| t0);
        self.barrierLanded(started_at);
    }

    /// DUR-3 start ticket for a barrier about to be issued on the device —
    /// by this checkpointer or by the journal lane's own FSYNC SQE: every
    /// barrier on the volume moves the same three watermarks on one clock.
    /// Take it BEFORE the device op. acq_rel pairs with `barrierPushEpoch`'s
    /// acquire: a push that read this ticket as "started" is never drained
    /// by this barrier.
    pub fn barrierStarted(self: *Checkpointer) u64 {
        return self.barrier_starts.fetchAdd(1, .acq_rel) + 1;
    }

    /// The barrier with ticket `started_at` completed on the device: coverage
    /// is published BEFORE the drain so everything it releases drains
    /// against it. Any thread (the task's, the lane's, an fsync caller's).
    pub fn barrierLanded(self: *Checkpointer, started_at: u64) void {
        _ = self.barrier_durable.fetchMax(started_at, .acq_rel);
        self.afterDurableBarrier();
    }

    /// Every record pushed with an epoch below the completed barrier's start
    /// is durable: its tail opens the ring, releases the pending frees it
    /// covers and advances the cache's elision tail — all three on one clock.
    /// Concurrent callers are safe: the queue pops under its own lock and
    /// every advance is a monotonic fetchMax, so the gauge reads the ring's
    /// own word after the drain rather than a stale local.
    fn afterDurableBarrier(self: *Checkpointer) void {
        const covered = self.barrier_durable.load(.acquire);
        var moved = false;
        while (self.reclaim.popCovered(covered)) |e| {
            _ = self.deps.alloc.advanceDurable(e.tail);
            self.deps.core.advanceReusableUpto(e.tail);
            self.deps.cache.setDurableTail(e.tail);
            moved = true;
        }
        if (moved) {
            self.reusable_upto.set(self.deps.core.reusableUpto());
            if (self.space_wake) |w| w.notify.notifyWaiters(w.io);
        }
    }

    // ---- the decision ----

    /// One walk of the node cache: the dirty count (the stats face keeps it).
    pub fn dirtyNodeCount(self: *Checkpointer) u64 {
        var dirty: u64 = 0;
        self.deps.cache.forEachNode(&dirty, struct {
            fn count(acc: *u64, n: *CachedNode) void {
                if (n.dirtyFloor() != node_cache.no_floor) acc.* += 1;
            }
        }.count);
        self.dirty_nodes.set(dirty);
        return dirty;
    }

    /// The live projection of the next cycle's decision-to-landing wall, ms.
    pub fn projectedMsFor(self: *const Checkpointer, dirty_nodes: u64) u64 {
        const flush = ckpt.projectedFlushWallNs(dirty_nodes, self.node_units.anticipatedNs(), self.deps.cache.heapPromised(), self.image_units.anticipatedNs());
        return ckpt.projectedCycleWallNs(flush, ckpt.CYCLE_COVERING_BARRIERS, self.barriers.anticipatedNs()) / ns_per_ms;
    }

    /// Read the decision's inputs once and take it.
    pub fn decide(self: *Checkpointer, now_ns: u64, park_kick: bool, final_cycle: bool) Decision {
        const core = self.deps.core;
        const dirty = self.dirtyNodeCount();
        const projected = self.projectedMsFor(dirty);
        const term_projection: u64 = if (self.cfg.forest) ckpt.anticipatedTermMs(self.terms.anticipatedNs() / ns_per_ms, projected) else 0;
        self.projected_ms.set(projected);
        const d = ckpt.decideCheckpoint(.{
            .now_ns = now_ns,
            .last_cycle_at_ns = if (self.cfg.forest) self.collected_ns.load(.acquire) else self.last_cycle_end_ns.load(.acquire),
            .journal_distance = core.headPos() -| core.reusableUpto(),
            .ring_len = core.geometry().logicalLen(),
            .dirty_nodes = dirty,
            .max_dirty = self.cfg.max_dirty_nodes,
            .park_mark = park_kick,
            .heap_full = self.posture.isFull(),
            .term_projection_ms = term_projection,
            .max_age_ms = self.cfg.max_age_ms,
            .final_cycle = final_cycle,
        });
        self.trigger_ms.set(d.trigger_ms);
        return d;
    }

    /// The age decision about to RUN a cycle hands it its lateness, the tick
    /// in force and the words the tape reports — read HERE, before the cycle
    /// consumes the promises and moves the units.
    fn noteDecision(self: *Checkpointer, d: Decision, late_ns: u64, tick_ms: u64, decided_at_ns: u64, dirty_nodes: u64) void {
        self.decision_late_ns = late_ns;
        self.decision_tick_ns = tick_ms *| ns_per_ms;
        self.decision_at_ns = decided_at_ns;
        self.decision_words = .{
            .dirty_at_decision = dirty_nodes,
            .promised_at_decision = self.deps.cache.heapPromised(),
            .node_unit_us = self.node_units.anticipatedNs() / 1000,
            .image_unit_us = self.image_units.anticipatedNs() / 1000,
            .projected_ms = self.projectedMsFor(dirty_nodes),
            .anticipated_ms = self.terms.anticipatedNs() / ns_per_ms,
            .trigger_ms = d.trigger_ms,
        };
    }

    /// A FAILED cycle never folded its decision: the lateness was that
    /// cycle's, and the next cycle's term is its own.
    fn clearDecision(self: *Checkpointer) void {
        self.decision_late_ns = 0;
        self.decision_tick_ns = 0;
        self.decision_at_ns = 0;
        self.decision_words = .{};
    }

    fn noteFlushPass(self: *Checkpointer, sample: ckpt.FlushPassSample) void {
        if (ckpt.flushUnitNs(sample.node_ns, sample.nodes)) |u| {
            self.node_units.push(u);
            self.node_unit_ns.set(self.node_units.anticipatedNs());
        }
        if (ckpt.flushUnitNs(sample.image_ns, sample.images)) |u| {
            self.image_units.push(u);
            self.image_unit_ns.set(self.image_units.anticipatedNs());
        }
    }

    fn noteCheckpointBarrier(self: *Checkpointer, wall_ns: u64) void {
        self.barriers.push(wall_ns);
        self.barrier_ms.set(self.barriers.anticipatedNs() / ns_per_ms);
    }

    fn spaceSnapshot(self: *const Checkpointer) heap_law.SpaceSnapshot {
        return .{ .free = self.deps.alloc.freeExtents(), .promised = self.deps.cache.heapPromised(), .reserve = self.deps.alloc.reserveExtents() };
    }

    fn growthFloorClear(self: *const Checkpointer) bool {
        return heap_law.heapGrowthFloorClear(self.deps.cache.config().layout, self.deps.alloc.freeExtents(), self.deps.cache.heapPromised(), self.deps.alloc.reserveExtents());
    }

    // ---- the cycle ----

    /// Force one full cycle with an immediate barrier (`checkpoint_now`).
    pub fn checkpointNow(self: *Checkpointer) CycleError!void {
        return self.checkpointCycle(true);
    }

    /// The cadence face: the record's durability rides a later barrier and
    /// reclamation lags one cycle.
    pub fn checkpointCadence(self: *Checkpointer) CycleError!void {
        return self.checkpointCycle(false);
    }

    /// One §5.3 cycle over EVERY dirty node (the cadence's shape).
    pub fn checkpointCycle(self: *Checkpointer, barrier_now: bool) CycleError!void {
        return self.checkpointCycleBounded(barrier_now, DrainBudget.unbounded);
    }

    /// A cycle whose flush pass admits nodes through `budget` (oldest floor
    /// first, the first always): the nodes it leaves keep their floors and
    /// clamp the tail. A failure before the term fold clears the decision
    /// words so the next cycle folds its own.
    pub fn checkpointCycleBounded(self: *Checkpointer, barrier_now: bool, budget: DrainBudget) CycleError!void {
        self.cycleManaged(barrier_now, budget) catch |e| {
            self.clearDecision();
            return e;
        };
    }

    const Collector = struct {
        gpa: Allocator,
        list: *std.ArrayList(*CachedNode),
        oom: bool = false,

        fn visit(c: *Collector, n: *CachedNode) void {
            if (n.dirtyFloor() == node_cache.no_floor or n.state.isSuperseded()) return;
            c.list.append(c.gpa, n.retain()) catch {
                n.release();
                c.oom = true;
            };
        }
    };

    fn floorLess(_: void, a: *CachedNode, b: *CachedNode) bool {
        return a.dirtyFloor() < b.dirtyFloor();
    }

    fn minDirtyFloor(self: *Checkpointer) u64 {
        var min: u64 = node_cache.no_floor;
        self.deps.cache.forEachNode(&min, struct {
            fn fold(acc: *u64, n: *CachedNode) void {
                acc.* = @min(acc.*, n.dirtyFloor());
            }
        }.fold);
        return min;
    }

    fn cycleManaged(self: *Checkpointer, barrier_now: bool, budget_in: DrainBudget) CycleError!void {
        const gpa = self.gpa;
        const t_start = clock.monotonicNs();
        const h = self.deps.core.headPos();
        // The wedged-tail audit's inputs: captured before anything mutates.
        const pending_before = self.deps.alloc.pendingCount();
        const tail_before = self.last_ledger_tail.load(.acquire);

        // ---- C2: the dirty set. Every leaf dirtied from here is the NEXT
        // cycle's, so the cadence measures its interval from this instant.
        var dirty: std.ArrayList(*CachedNode) = .empty;
        defer {
            for (dirty.items) |n| n.release();
            dirty.deinit(gpa);
        }
        var collector: Collector = .{ .gpa = gpa, .list = &dirty };
        self.deps.cache.forEachNode(&collector, Collector.visit);
        if (collector.oom) return error.OutOfMemory;
        self.collected_ns.store(clock.monotonicNs(), .release);
        // ---- C3: oldest floor first; the ring's oldest node keep-exempt.
        std.mem.sort(*CachedNode, dirty.items, {}, floorLess);
        const t_collected = clock.monotonicNs();

        // ---- C4: the flush pass.
        var sample: ckpt.FlushPassSample = .{};
        var deferred_for_space: u64 = 0;
        var deferred_for_grant: u64 = 0;
        var budget = budget_in;
        for (dirty.items, 0..) |n, i| {
            if (!budget.admits(clock.monotonicNs())) break;
            const node_started = clock.monotonicNs();
            const r = self.deps.flush.flush(self.deps.flush.ctx, n, .{ .mode = .checkpoint, .keep_exempt = i == 0 });
            sample.note(clock.monotonicNs() -| node_started, r.images);
            switch (r.disposition) {
                .discharged, .appended, .replaced, .skipped => {},
                .deferred_reserve => log.debug("checkpoint: SMO reserve exhausted at node 0x{x}; deferred to the next cycle", .{n.addr}),
                .deferred_space => deferred_for_space += 1,
                .deferred_grant => deferred_for_grant += 1,
                .declined => log.debug("checkpoint: node 0x{x} does not fit its extent and no structural task is bound; its floor clamps the tail", .{n.addr}),
                // Structurally unreachable under forced retirement: a
                // protocol bug, never a deferral.
                .deferred_pending_free => {
                    log.err("checkpoint: the flush pass's forced SMO at node 0x{x} answered PendingFreeFull — the §4.7 cycle-break is broken; the cycle fails", .{n.addr});
                    return error.Corrupt;
                },
                .failed => return failureError(r.failure),
            }
        }
        // ---- C4b: the roots the ledger does NOT name (guest slot trees,
        // charter §5.5 "a guest slot tree's root lives in tree 0") are
        // published into the control tree as checkpoint-class journal
        // entries AFTER the flush pass (whose SMOs are the only root
        // movers; the task serializes them) and BEFORE the tail rule, so
        // the entries sit inside the window the record names and tree 0's
        // dirty leaf floor clamps the tail under them.
        try self.deps.record.publishGuestRoots(self.deps.record.ctx);
        const t_flushed = clock.monotonicNs();

        // ---- C5: the heap-full posture; the pass's units.
        const growth_clear = self.growthFloorClear();
        self.posture.cycleFlushPass(self.cfg.volume, deferred_for_space, growth_clear, self.spaceSnapshot());
        self.noteFlushPass(sample);

        // ---- C6: the dirty bitmap pages at this checkpoint's generation —
        // on the device before the record that names it.
        const ckpt_seq = self.checkpoint_seq.load(.acquire) + 1;
        try self.writeDirtyPages(ckpt_seq);
        const t_pages = clock.monotonicNs();

        // ---- C7: barrier #1. A failure counts toward the lattice and aborts
        // the cycle before its record; the appended bytes stay on the device
        // and the next cycle's barrier covers them before ITS record.
        try self.syncDevice();
        const landed_ns = clock.monotonicNs();
        self.noteCheckpointBarrier(landed_ns -| t_pages);
        var tape = self.foldCycleTerm(.{ .started_ns = t_start, .collected_ns = t_collected, .flushed_ns = t_flushed, .pages_ns = t_pages, .landed_ns = landed_ns }, ckpt_seq, dirty.items.len, sample);

        // ---- C8: the tail rule. Floors of nodes whose mappings LEFT the
        // cache since the last drain are invisible to the live walk, so they
        // clamp this record (folded back on any failure past this point).
        const dying = self.deps.cache.takeDyingFloors();
        const inflight_start = self.deps.inflight.minInflightStart();
        const dirty_floor_min = self.minDirtyFloor();
        const tail = ledger.checkpointTail(h, &.{ inflight_start, dying, dirty_floor_min });
        if (tail < h) log.debug("checkpoint tail {d} < head {d}: inflight {d}, dying floors {d}, dirty node floor {d}", .{ tail, h, inflight_start, dying, dirty_floor_min });
        const t_tail = clock.monotonicNs();

        // ---- C9: the record. Node-seq watermark captured AFTER the flush
        // loop (whose SMOs mint).
        var roots: std.ArrayList(TreeRoot) = .empty;
        defer roots.deinit(gpa);
        self.deps.record.treeRoots(self.deps.record.ctx, gpa, &roots) catch |e| {
            self.deps.cache.restoreDyingFloors(dying);
            return e;
        };
        const record: ledger.LedgerRecord = .{
            .seq = ckpt_seq,
            .tree_roots = roots.items,
            .journal_tail_seq = tail,
            .next_ino = self.deps.record.next_ino.snapshot(),
            .alloc_bitmap_generation = ckpt_seq,
            .node_seq_watermark = self.deps.record.node_seq.load(),
            .membership_stamp = self.deps.record.membershipStamp(self.deps.record.ctx),
        };
        const cache_clean = dirty_floor_min == node_cache.no_floor and dying == node_cache.no_floor;
        if (self.deps.ledger != null) {
            self.writeLedgerRecord(record) catch |e| {
                // The covering record never landed: the previous record stays
                // authoritative and the dying floors are still uncovered.
                self.deps.cache.restoreDyingFloors(dying);
                return e;
            };
        } else {
            // Page-as-ledger: the page write IS the record's landing, so
            // its failure is the record's (the previous page stays
            // authoritative; the next cycle rewrites it).
            const f = self.deps.record.appenderPages.?;
            f(self.deps.record.ctx, &record, cache_clean) catch |e| {
                self.deps.cache.restoreDyingFloors(dying);
                return e;
            };
        }
        // ---- C10.
        self.checkpoint_seq.store(ckpt_seq, .release);
        self.last_ledger_tail.store(tail, .release);
        self.retire_seq.store(ckpt_seq + 1, .release);
        // DUR-3: stamped with the barriers started AS OF the write's
        // completion — none of them can have flushed it.
        try self.reclaim.push(tail, self.barrierPushEpoch());
        ckpt.stats.meta_kv_checkpoints.inc();
        // C11: the appender pages (the record is authoritative; a page that
        // fails to land is one cycle stale and the next cycle rewrites it).
        if (self.deps.ledger != null) if (self.deps.record.appenderPages) |f| try f(self.deps.record.ctx, &record, cache_clean);
        const t_ledger = clock.monotonicNs();

        // ---- C12: make THIS record durable now and judge progress.
        if (barrier_now) {
            try self.syncDevice();
            try self.wedgedTailAudit(.{
                .pending_before = pending_before,
                .pending_after = self.deps.alloc.pendingCount(),
                .tail_before = tail_before,
                .tail = tail,
                .deferred_for_grant = deferred_for_grant,
                .deferred_for_space = deferred_for_space,
            });
        }
        // ---- C13: the heap-full sweep, after the barrier.
        if (self.posture.isFull() or self.merge_backlog) self.mergeSweepArm();
        const t_end = clock.monotonicNs();
        tape.tail_ns = t_tail - landed_ns;
        tape.ledger_ns = t_ledger - t_tail;
        tape.reclaim_ns = t_end - t_ledger;
        tape.total_ns = t_end - t_start;
        std.debug.assert(tape.phasesSum() == tape.total_ns);
        self.publishTape(tape);
        self.last_cycle_end_ns.store(t_end, .release);
    }

    fn writeDirtyPages(self: *Checkpointer, generation: u64) CycleError!void {
        var set = try self.deps.alloc.beginDirtyPageWrite(generation);
        defer set.deinit();
        if (set.writes.len == 0) return;
        const pages = self.deps.pages orelse {
            // A grant mirror: the dirty set is discharged, nothing lands.
            set.commit(self.deps.alloc);
            return;
        };
        for (set.writes) |*w| {
            pages.writePage(pages.ctx, w.offset, &w.image) catch |e| {
                // Law 1: the snapshot is this write's only copy — re-dirty.
                set.abort(self.deps.alloc);
                return e;
            };
        }
        set.commit(self.deps.alloc);
    }

    fn writeLedgerRecord(self: *Checkpointer, record: ledger.LedgerRecord) CycleError!void {
        const image = record.encodeSlot() catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt, error.ChecksumMismatch => error.Corrupt,
        };
        const l = self.deps.ledger.?;
        try l.writeSlot(l.ctx, record.slotIndex(), &image);
        ckpt.stats.meta_kv_ledger_writes.inc();
    }

    const Walls = struct { started_ns: u64, collected_ns: u64, flushed_ns: u64, pages_ns: u64, landed_ns: u64 };

    /// Fold the landing wall — from the age decision that fired this cycle
    /// (else its start) to barrier #1 — with the decision's lateness into the
    /// term the cadence anticipates, and write the tape's words (the audit's
    /// WARN line reads them right after).
    fn foldCycleTerm(self: *Checkpointer, walls: Walls, seq: u64, dirty_collected: usize, sample: ckpt.FlushPassSample) CycleTape {
        const late_ns = self.decision_late_ns;
        const tick_ns = self.decision_tick_ns;
        const decided_at = self.decision_at_ns;
        const start = if (decided_at != 0 and decided_at < walls.started_ns) decided_at else walls.started_ns;
        const ceiling_ns = ckpt.checkpointLandingCeilingMs(self.cfg.max_age_ms, self.cfg.flush_interval_ms) *| ns_per_ms;
        const term = ckpt.checkpointCycleTermNs(walls.landed_ns -| start, late_ns, tick_ns, ceiling_ns);
        self.terms.push(term);
        self.term_ms.set(self.terms.anticipatedNs() / ns_per_ms);
        self.lates.push(late_ns);
        self.late_max_ms.set(self.lates.anticipatedNs() / ns_per_ms);
        var tape = self.decision_words;
        self.clearDecision();
        tape.seq = seq;
        tape.late_ms = late_ns / ns_per_ms;
        tape.pre_start_ns = walls.started_ns -| start;
        tape.collect_ns = walls.collected_ns - walls.started_ns;
        tape.flush_ns = walls.flushed_ns - walls.collected_ns;
        tape.pages_ns = walls.pages_ns - walls.flushed_ns;
        tape.barrier_ns = walls.landed_ns - walls.pages_ns;
        tape.landing_ns = walls.landed_ns -| start;
        tape.term_ns = term;
        tape.dirty_collected = dirty_collected;
        tape.nodes_appended = sample.nodes;
        tape.smo_nodes = sample.smo_nodes;
        tape.images = sample.images;
        tape.append_ns = sample.node_ns;
        tape.image_ns = sample.image_ns;
        return tape;
    }

    fn publishTape(self: *Checkpointer, tape: CycleTape) void {
        ckpt.stats.meta_kv_checkpoint_phase_ns.record(tape);
        self.tape_lock.lock();
        defer self.tape_lock.unlock();
        self.last_cycle = tape;
    }

    /// The §4.7 progress audit, on EVERY barriered cycle: a parked retirement
    /// released or the tail advancing resets the rung; a tail pinned for an
    /// exhausted GRANT is the manager dependency; one pinned for `NoSpace` is
    /// the SPACE standstill (counted, latched, never terminal); otherwise the
    /// rung advances and at `PENDING_FREE_FORCE_CYCLES` the volume fails loud.
    fn wedgedTailAudit(self: *Checkpointer, obs: alloc_core.CycleObservations) error{VolumeFailed}!void {
        const out = self.audit.tick(obs);
        ckpt.stats.pending_free_stalled_cycles.set(out.stalled_cycles);
        switch (out.verdict) {
            .progress, .wedge_rung => {},
            .manager_dependency => log.debug("checkpoint: {d} node(s) deferred for an exhausted extent grant (tail {d}); the manager's refill owns the retry", .{ obs.deferred_for_grant, obs.tail }),
            .space_standstill => log.debug("checkpoint: space standstill — {d} node(s) deferred for NoSpace, {d} retirements parked behind their floors (tail {d}); not a wedge, the volume stays writable for deletes", .{ obs.deferred_for_space, obs.pending_after, obs.tail }),
            .wedge_failed => {
                if (self.deps.latch.failStop()) {
                    log.err("meta volume {s}: pending-free retirements wedged ({d} parked) and {d} consecutive barriered checkpoint cycles neither released one nor advanced the ledger tail (stuck at {d}) — the durable tail is pinned by something no flush pass can discharge (§4.7 wedged-tail bound); volume marked FAILED (mutations return EIO until remount)", .{ self.cfg.volume, obs.pending_after, out.stalled_cycles, obs.tail });
                }
                return error.VolumeFailed;
            },
        }
    }

    /// The §4.6a heap-full sweep: bounded to one tick period, cursor-resumed,
    /// while the volume is full or a lap left a backlog; YIELDS to a user
    /// commit retrying for space (the sweep claims at the same floor the
    /// retry needs) with the backlog word standing.
    fn mergeSweepArm(self: *Checkpointer) void {
        if (self.space_retry.inflight()) {
            self.merge_backlog = true;
            heap_law.SpaceRetryHold.noteMergeYield();
            log.debug("checkpoint: merge sweep on {s} yielded to a user commit retrying for space", .{self.cfg.volume});
            return;
        }
        const deadline = clock.monotonicNs() +| ckpt.mergeSweepBudgetMs(self.cfg.flush_interval_ms) *| ns_per_ms;
        const report = self.deps.smo.mergeUnderfull(self.deps.smo.ctx, deadline);
        ckpt.stats.meta_kv_merge_sweeps.inc();
        // The backlog stands while the recovery is not at its fixed point: a
        // lap the floor or the budget cut, or a lap that MERGED (its
        // successors and shrunk parents can pair again).
        self.merge_backlog = report.space_refused or !report.lap_complete or report.merges + report.root_collapses > 0;
    }

    // ---- the DENSE rule outside a cycle ----

    /// A checkpoint-class durable step CONSUMES a seq and writes the record
    /// that seq names: the dirty pages at `ckpt_seq`, a barrier, the last
    /// cycle's record RESTATED at `ckpt_seq` (the roots as they stand, the
    /// last tail — nothing became covered — the live ino and watermark),
    /// a barrier. Content-equivalent to the record it follows, so the
    /// predicted-slot poll never stops on a gap. Returns the seq consumed.
    pub fn consumeCheckpointSeqForBitmap(self: *Checkpointer) CycleError!u64 {
        const ckpt_seq = self.checkpoint_seq.fetchAdd(1, .acq_rel) + 1;
        try self.writeDirtyPages(ckpt_seq);
        try self.syncDevice();
        var roots: std.ArrayList(TreeRoot) = .empty;
        defer roots.deinit(self.gpa);
        try self.deps.record.treeRoots(self.deps.record.ctx, self.gpa, &roots);
        const last: ledger.LedgerRecord = .{
            .seq = ckpt_seq - 1,
            .tree_roots = roots.items,
            .journal_tail_seq = self.last_ledger_tail.load(.acquire),
            .next_ino = self.deps.record.next_ino.snapshot(),
            .alloc_bitmap_generation = ckpt_seq,
            .node_seq_watermark = self.deps.record.node_seq.load(),
            .membership_stamp = self.deps.record.membershipStamp(self.deps.record.ctx),
        };
        try self.writeLedgerRecord(last.restatement(ckpt_seq));
        self.retire_seq.store(ckpt_seq + 1, .release);
        try self.syncDevice();
        return ckpt_seq;
    }

    // ---- the cover loops ----

    /// The hole discipline: cycle until the written tail reaches `pos` (the
    /// unwritten hole's END) so replay's chain walk never enters it.
    pub fn checkpointPast(self: *Checkpointer, pos: u64) CycleError!void {
        for (0..ckpt.HOLE_COVER_CYCLES_MAX) |_| {
            try self.checkpointCycle(true);
            if (self.last_ledger_tail.load(.acquire) >= pos) return;
        }
        log.err("meta volume {s}: checkpoint tail failed to clear the journal hole ending at {d} after {d} cycles (tail stuck at {d}) — replay would walk into the hole", .{ self.cfg.volume, pos, ckpt.HOLE_COVER_CYCLES_MAX, self.last_ledger_tail.load(.acquire) });
        return error.Corrupt;
    }

    /// Cover the bring-up residue: barriered cycles until `reusable_upto ==
    /// head`, so the volume SERVES with an empty replay window and ZERO
    /// reclaimable tail (the D1.b wedge-crumb class: a first tick covering
    /// bring-up commits released exactly their size as admission budget and
    /// let a small parked committer slip the escalation lattice).
    pub fn coverBringUpResidue(self: *Checkpointer) CycleError!void {
        const core = self.deps.core;
        if (core.reusableUpto() >= core.headPos()) return;
        for (0..ckpt.COVER_CYCLES_MAX) |_| {
            try self.checkpointCycle(true);
            if (core.reusableUpto() >= core.headPos()) return;
        }
        log.err("meta volume {s}: bring-up journal residue did not cover within {d} barriered cycles (head={d}, reusable_upto={d}) — refusing to serve with a reclaimable tail", .{ self.cfg.volume, ckpt.COVER_CYCLES_MAX, core.headPos(), core.reusableUpto() });
        return error.Corrupt;
    }

    // ---- the tick ----

    /// Drain the threshold queue within `budget`: appends (and the SMOs they
    /// force) only — no barrier, no ledger, both stay on the cadence; the
    /// floors stay too (only a checkpoint's barrier discharges them).
    /// Reserve exhaustion or a FIFO-cap refusal runs one barriered drain
    /// cycle and retries the item once; a `NoSpace` deferral latches the
    /// posture and leaves the node to the cadence's flush pass.
    pub fn drainMaintenance(self: *Checkpointer, budget_in: DrainBudget) CycleError!void {
        const src = self.deps.maintenance;
        var budget = budget_in;
        var sample: ckpt.FlushPassSample = .{};
        while (budget.admits(clock.monotonicNs())) {
            const addr = src.pop(src.ctx) orelse break;
            const n = self.deps.cache.tryGet(addr) orelse continue;
            defer n.release();
            const t0 = clock.monotonicNs();
            const r = self.deps.flush.flush(self.deps.flush.ctx, n, .{ .mode = .threshold });
            sample.note(clock.monotonicNs() -| t0, r.images);
            ckpt.stats.meta_kv_maintenance_items.inc();
            switch (r.disposition) {
                .deferred_reserve, .deferred_pending_free => {
                    if (self.reserve_deferred_addr == addr) {
                        self.reserve_deferred_addr = null;
                        log.debug("threshold maintenance: node 0x{x} deferred for the SMO reserve or the pending-free cap twice; left to the cadence's flush pass", .{addr});
                        continue;
                    }
                    self.reserve_deferred_addr = addr;
                    try self.checkpointCycle(true);
                    src.requeue(src.ctx, addr);
                },
                .deferred_space => _ = self.posture.enter(self.cfg.volume, "threshold maintenance: the SMO could not claim an extent", self.spaceSnapshot()),
                .failed => return failureError(r.failure),
                .discharged, .appended, .replaced, .skipped, .deferred_grant, .declined => self.reserve_deferred_addr = null,
            }
        }
        self.noteFlushPass(sample);
    }

    /// Shutdown: the registry must be QUIET — `close` quiesces the conveyor
    /// (nothing queued, no pass, no window in flight: every user
    /// reservation completed) BEFORE the task stops, and the task's own SMO
    /// entries complete synchronously, so no reservation can be open here.
    /// The lane outlives this tick (its SMO entries are structural writes
    /// on it) and shuts down after. The ordering is made executable: an
    /// open reservation fires the tripwire and the tick proceeds — its tail
    /// clamps below the open start and the next mount replays the residue —
    /// instead of waiting on a completer under `smo_lock` (a wedge).
    fn checkHeadStable(self: *Checkpointer) void {
        const open = self.deps.inflight.openCount();
        if (open != 0) ckpt.stats.final_tick_open_reservations.fire("a journal reservation is still open at the shutdown tick: close() must quiesce the conveyor before the task stops");
    }

    fn uncovered(self: *const Checkpointer) bool {
        return self.deps.core.headPos() != self.deps.core.reusableUpto() or self.deps.alloc.hasDirtyPages();
    }

    /// One task tick: decide BEFORE the drain (a drain ahead of a due cycle
    /// is work the cycle repeats) → the threshold drain within the budget
    /// when no cycle is due → decide again → the cycle. The coalesced
    /// trailing barrier is the LANE's (its DUR-3 hooks move the watermarks);
    /// the tick issues none of its own. The shutdown tick drains FIRST and
    /// unbounded, forces a full cycle with an immediate barrier and
    /// iterates to the fixpoint (`tail == head ∧ no dirty pages`: an empty
    /// replay window).
    pub fn tick(self: *Checkpointer, final_cycle: bool, drain_deadline_ns: ?u64, park_kick: bool, tick_ms: u64) CycleError!void {
        if (final_cycle) self.checkHeadStable();
        var decided_at = clock.monotonicNs();
        var decision: ?Decision = if (final_cycle) null else self.decide(decided_at, park_kick, false);
        const run_drain = if (decision) |d| !d.runsACycle() else true;
        if (run_drain) try self.drainMaintenance(if (drain_deadline_ns) |d| DrainBudget.bounded(d) else DrainBudget.unbounded);
        if (run_drain) {
            decided_at = clock.monotonicNs();
            decision = self.decide(decided_at, park_kick, final_cycle);
        }
        const d = decision.?;
        if (d.runsACycle()) {
            if (d.reason == .park_kick) ckpt.stats.meta_kv_checkpoint_park_cycles.inc();
            // The lateness rides the cycle it RUNS, and only that one.
            if (d.age_late_ns) |late| self.noteDecision(d, late, tick_ms, decided_at, self.dirty_nodes.load());
            try self.checkpointCycle(d.immediate_barrier);
        } else if (d.age_late_ns != null) {
            // An idle due tick is an EMPTY COLLECTION: the age law's reference
            // advances and an idle volume never accrues lateness.
            self.collected_ns.store(decided_at, .release);
            self.last_cycle_end_ns.store(decided_at, .release);
        }
        if (final_cycle) {
            for (0..ckpt.FIXPOINT_COVER_CYCLES_MAX) |_| {
                if (!self.uncovered()) return;
                try self.checkpointCycle(true);
            }
            if (self.uncovered()) {
                log.warn("meta volume {s}: shutdown checkpoint did not converge to an empty replay window (head={d}, reusable_upto={d}, dirty bitmap pages: {}): the next mount will replay the residue", .{ self.cfg.volume, self.deps.core.headPos(), self.deps.core.reusableUpto(), self.deps.alloc.hasDirtyPages() });
            }
        }
    }
};

// ---------------------------------------------------------------------------
// The task.
// ---------------------------------------------------------------------------

pub const KickMark = enum {
    /// A node crossed the writeback threshold: a maintenance pass.
    threshold,
    /// A committer parked at ring admission: a cycle with an immediate barrier.
    ring_park,
    /// The memory budget's shed lever: an early run of the cadence's drain.
    shed,
};

pub const CheckpointTask = struct {
    cp: *Checkpointer,
    io: Io,
    wake: sync.Notify = .init,
    ring_park_kick: std.atomic.Value(bool) = .init(false),
    stop_requested: std.atomic.Value(bool) = .init(false),
    stopped: std.atomic.Value(bool) = .init(false),
    period_ns: u64,
    next_tick_ns: u64,
    /// The volume's structural (SMO) context lock when other callers share
    /// it — `checkpointNow` from a committer's space retry, the lane's hole
    /// checkpoint — held around every `runOnce`; the tests' solo task runs
    /// unlocked.
    serial: ?*Io.Mutex = null,

    pub fn init(cp: *Checkpointer, io: Io) CheckpointTask {
        const period = ckpt.checkpointTickPeriodMs(cp.cfg.flush_interval_ms) * ns_per_ms;
        return .{ .cp = cp, .io = io, .period_ns = period, .next_tick_ns = clock.monotonicNs() + period };
    }

    /// Wake the task. The park mark rides the wake and makes the next tick's
    /// cycle due with an immediate barrier; a threshold wake never carries it.
    pub fn kick(self: *CheckpointTask, mark: KickMark) void {
        switch (mark) {
            // release pairs with the task's acq_rel swap in `runOnce`.
            .ring_park => self.ring_park_kick.store(true, .release),
            .threshold, .shed => {},
        }
        self.wake.notifyOne(self.io);
    }

    /// The stop latch: the next wake runs the final tick and the loop exits.
    pub fn stop(self: *CheckpointTask) void {
        self.stop_requested.store(true, .release);
        self.wake.notifyOne(self.io);
    }

    pub fn isStopped(self: *const CheckpointTask) bool {
        return self.stopped.load(.acquire);
    }

    /// The node cache's budget-shed hook (`installCheckpointKick`).
    pub fn hook(self: *CheckpointTask) node_cache.Hook {
        return .{ .ctx = self, .call = shedHook };
    }

    fn shedHook(ctx: *anyopaque) void {
        const self: *CheckpointTask = @ptrCast(@alignCast(ctx));
        self.kick(.shed);
    }

    /// The task body for ONE wake (or the deadline): the cadence is read
    /// off the CLOCK; the park mark promotes a wake to a tick; a FAILED
    /// volume idles until shutdown; a tick's drain is bounded by one period
    /// and the shutdown tick's is not. Returns false after the final tick.
    pub fn runOnce(self: *CheckpointTask, woke: bool, now_ns: u64) bool {
        var cadence = !woke or now_ns >= self.next_tick_ns;
        if (cadence) self.next_tick_ns = now_ns + self.period_ns;
        const park_kick = self.ring_park_kick.swap(false, .acq_rel);
        if (woke and !cadence and park_kick) {
            cadence = true;
            self.next_tick_ns = now_ns + self.period_ns;
        }
        const shutting_down = self.stop_requested.load(.acquire);
        if (self.cp.isFailed() and !shutting_down) return true;
        const drain_deadline: ?u64 = if (shutting_down) null else now_ns + self.period_ns;
        if (cadence or shutting_down) {
            self.cp.tick(shutting_down, drain_deadline, park_kick, self.period_ns / ns_per_ms) catch |e| {
                log.err("kv checkpoint tick failed on {s}: {s} (state stays RAM-consistent; retrying next tick — barrier failures additionally escalate through the fail-stop lattice)", .{ self.cp.cfg.volume, @errorName(e) });
            };
            if (shutting_down) {
                self.stopped.store(true, .release);
                return false;
            }
        } else {
            self.cp.drainMaintenance(DrainBudget.bounded(drain_deadline.?)) catch |e| {
                log.warn("kv maintenance pass failed on {s}: {s} (state stays RAM-consistent; the cadence tick retries)", .{ self.cp.cfg.volume, @errorName(e) });
            };
        }
        // Work arrived while draining, or a shutdown landed mid-pass: re-arm
        // so the wait returns at once (the deadline is read off the clock, so
        // the re-arm can never starve the cadence).
        if (self.stop_requested.load(.acquire) or self.cp.maintenancePending()) self.wake.notifyOne(self.io);
        return true;
    }

    /// The loop a thread runs until `stop`: `runOnce` behind the Notify wait
    /// bounded by the cadence deadline.
    pub fn runUntilStopped(self: *CheckpointTask) void {
        var name: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name, "sqz-ckpt");
        self.next_tick_ns = clock.monotonicNs() + self.period_ns;
        while (true) {
            var w = self.wake.notified();
            const woke = w.waitUntilUncancelable(self.io, self.next_tick_ns) == .notified;
            const more = blk: {
                if (self.serial) |m| m.lockUncancelable(self.io);
                defer if (self.serial) |m| m.unlock(self.io);
                break :blk self.runOnce(woke, clock.monotonicNs());
            };
            if (!more) return;
        }
    }
};

// ---------------------------------------------------------------------------
// Tests: a full volume in RAM — node cache over `RamLoader`, the journal
// core + in-flight registry, the allocator with a bitmap region image, a
// ledger image — driven through the seams.
// ---------------------------------------------------------------------------

const testing = std.testing;
const node = @import("node.zig");
const key = @import("key.zig");

const NODE_SIZE: usize = node.MIN_NODE_SIZE;
const test_stamp = ledger.MembershipStamp.oneMember("set-uuid-fixture".*);
const EXTENTS: u64 = 16;
const RING_PAGES: u64 = 64;

const TestVol = struct {
    gpa: Allocator,
    ram: node_cache.RamLoader,
    cache: *NodeCache,
    layout: node.NodeLayout,
    core: jc.JournalCore,
    inflight: jc.InflightRegistry,
    alloc: alloc_ext.ExtentAllocator,
    bitmap: []u8,
    ledger_image: []u8,
    latch: journal.FailureLatch = .{},
    ino: inos.InoWatermark,
    seq_handle: node_seq.NodeSeqHandle,
    roots: [1]TreeRoot,
    flusher: CacheFlusher,
    /// The one tree queue the threshold drain pops from.
    maintenance: tree.MaintenanceQueue,
    cp: Checkpointer,
    barriers: u64 = 0,
    fail_barrier: bool = false,
    fail_ledger: bool = false,
    slots_written: std.ArrayList(u64) = .empty,

    fn create(flush_interval_ms: u64, max_age_ms: u64) !*TestVol {
        const gpa = testing.allocator;
        const v = try gpa.create(TestVol);
        errdefer gpa.destroy(v);
        v.gpa = gpa;
        v.ram = try node_cache.RamLoader.init(gpa, @intCast(EXTENTS * NODE_SIZE));
        errdefer v.ram.deinit(gpa);
        v.layout = try node.NodeLayout.new(NODE_SIZE, null);
        v.cache = try NodeCache.init(gpa, testing.io, .{ .layout = v.layout, .heap_base = 0, .budget_bytes = EXTENTS * NODE_SIZE, .writeback_delta_bytes = std.math.maxInt(usize) }, v.ram.loader());
        errdefer v.cache.deinit();
        v.core = jc.JournalCore.init(.{ .page_data_len = 4072, .pages = RING_PAGES, .reserve_bytes = 4072 * 4, .grain = 1 }, 0, 0);
        v.inflight = try jc.InflightRegistry.init(gpa, 0, 16);
        errdefer v.inflight.deinit();
        v.alloc = try alloc_ext.ExtentAllocator.format(gpa, EXTENTS, 2, 8);
        errdefer v.alloc.deinit();
        v.bitmap = try gpa.alloc(u8, @intCast(alloc_ext.bitmapRegionLen(EXTENTS)));
        errdefer gpa.free(v.bitmap);
        @memset(v.bitmap, 0);
        v.ledger_image = try gpa.alloc(u8, @intCast(ledger.ROOT_LEDGER_LEN));
        errdefer gpa.free(v.ledger_image);
        @memset(v.ledger_image, 0);
        v.latch = .{};
        v.ino = inos.InoWatermark.init(2);
        v.seq_handle = node_seq.NodeSeqHandle.shared(100);
        v.roots = .{.{ .id = .native_slot, .node_addr = 0, .node_seq = 1 }};
        v.barriers = 0;
        v.fail_barrier = false;
        v.fail_ledger = false;
        v.slots_written = .empty;
        // The bootstrap record at seq 1 (slot 1), as format writes it.
        const boot = try ledger.bootstrapRecord(&v.roots, 2, 100, test_stamp).encodeSlot();
        @memcpy(v.ledger_image[4096 .. 2 * 4096], &boot);
        v.flusher = .{ .cache = v.cache, .smo = SmoSeam.none };
        v.maintenance = try tree.MaintenanceQueue.init(gpa, tree.MAINTENANCE_QUEUE_FLOOR);
        errdefer v.maintenance.deinit(gpa);
        v.cp = Checkpointer.init(gpa, .{
            .volume = "test-vol",
            .max_age_ms = max_age_ms,
            .flush_interval_ms = flush_interval_ms,
            .max_dirty_nodes = 4,
            .checkpoint_seq = alloc_ext.resumeCheckpointSeq(1, v.alloc.resumeGeneration()),
            .last_ledger_tail = 0,
        }, .{
            .core = &v.core,
            .inflight = &v.inflight,
            .cache = v.cache,
            .alloc = &v.alloc,
            .latch = &v.latch,
            .flush = v.flusher.sink(),
            .barrier = .{ .ctx = v, .barrier = barrierFn },
            .ledger = .{ .ctx = v, .writeSlot = writeSlotFn },
            .pages = .{ .ctx = v, .writePage = writePageFn },
            .record = .{ .ctx = v, .treeRoots = treeRootsFn, .next_ino = &v.ino, .node_seq = &v.seq_handle, .membershipStamp = stampFn, .publishGuestRoots = noGuestRootsFn },
            .smo = SmoSeam.none,
            .maintenance = MaintenanceSource.ofQueue(&v.maintenance),
        });
        return v;
    }

    fn destroy(v: *TestVol) void {
        const gpa = v.gpa;
        v.cp.deinit();
        v.maintenance.deinit(gpa);
        v.slots_written.deinit(gpa);
        gpa.free(v.ledger_image);
        gpa.free(v.bitmap);
        v.alloc.deinit();
        v.inflight.deinit();
        v.cache.deinit();
        v.ram.deinit(gpa);
        gpa.destroy(v);
    }

    fn barrierFn(ctx: *anyopaque) IoError!void {
        const v: *TestVol = @ptrCast(@alignCast(ctx));
        if (v.fail_barrier) return error.Io;
        v.barriers += 1;
    }

    fn writeSlotFn(ctx: *anyopaque, slot: u64, image: *const [ledger.ROOT_LEDGER_SLOT_LEN]u8) IoError!void {
        const v: *TestVol = @ptrCast(@alignCast(ctx));
        if (v.fail_ledger) return error.Io;
        const at: usize = @intCast(ledger.slotOffset(0, slot));
        @memcpy(v.ledger_image[at .. at + 4096], image);
        v.slots_written.append(v.gpa, slot) catch @panic("oom");
    }

    fn writePageFn(ctx: *anyopaque, offset: u64, image: *const [alloc_ext.page_len]u8) IoError!void {
        const v: *TestVol = @ptrCast(@alignCast(ctx));
        @memcpy(v.bitmap[@intCast(offset)..][0..alloc_ext.page_len], image);
    }

    fn treeRootsFn(ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList(TreeRoot)) Allocator.Error!void {
        const v: *TestVol = @ptrCast(@alignCast(ctx));
        try out.appendSlice(gpa, &v.roots);
    }

    fn stampFn(_: *anyopaque) ledger.MembershipStamp {
        return test_stamp;
    }

    fn noGuestRootsFn(_: *anyopaque) CycleError!void {}

    fn addrOf(ext: u64) u64 {
        return ext * NODE_SIZE;
    }

    /// A header-only leaf spanning the whole key space at extent `ext`.
    fn writeLeaf(v: *TestVol, ext: u64, seq: u64) !void {
        const gpa = v.gpa;
        var w = try node.writeNode(gpa, v.layout, .{ .node_addr = addrOf(ext), .node_seq = seq, .tree_id = key.KIND_INTERIOR, .level = 0, .min_key = "", .max_key = &key.KEY_SPACE_MAX }, &.{}, 0, null);
        defer w.deinit(gpa);
        v.ram.write(w.node_addr, w.image);
    }

    /// One commit: admit + reserve (registered) a `len`-byte entry, apply
    /// `keys` to `n` with seqs `start + i` and the ENTRY START as the floor,
    /// complete the write. Returns the reservation.
    fn commit(v: *TestVol, n: *CachedNode, keys: []const []const u8, len: u64) !jc.Reservation {
        const adm = v.core.tryAdmit(len, .user) orelse return error.RingFull;
        const reg = try v.inflight.reserveRegistered(&v.core, adm);
        try v.applyAt(n, keys, reg.res.start, reg.res.start);
        v.inflight.complete(&v.core, reg.res);
        return reg.res;
    }

    /// Apply `keys` with seqs `seq_start + i` and `floor` as the entry start.
    fn applyAt(v: *TestVol, n: *CachedNode, keys: []const []const u8, seq_start: u64, floor: u64) !void {
        const gpa = v.gpa;
        const recs = try gpa.alloc(node_cache.OwnedRec, keys.len);
        defer gpa.free(recs);
        for (keys, recs, 0..) |k, *r, i| r.* = try node_cache.OwnedRec.fromRecordRef(gpa, .{ .key = k, .seq = seq_start + i, .kind = .put, .value = "v" });
        const d = n.lockWrite(testing.io);
        defer n.unlockWrite(testing.io);
        try n.applyLocked(d, recs, floor);
    }

    fn newest(v: *TestVol) !ledger.LedgerRecord {
        return (try ledger.newestValid(v.gpa, v.ledger_image)) orelse error.NoRecord;
    }
};

test "a full cycle over an in-RAM volume: dirty nodes are flushed and their floors discharged, the slot is seq % 32, tail = head, phases are exact-sum, reusable_upto waits for the SECOND barrier and pending frees release only on coverage" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    try v.writeLeaf(1, 2);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    const b = (try v.cache.load(TestVol.addrOf(1))).?;
    defer b.release();
    const r1 = try v.commit(a, &.{"a1"}, 200);
    _ = try v.commit(b, &.{ "b1", "b2" }, 300);
    try testing.expectEqual(@as(u64, 0), r1.start);
    try testing.expectEqual(@as(u64, 2), v.cp.dirtyNodeCount());
    // A parked retirement inside the window, one past it.
    const e_in = try v.alloc.claimInternal();
    const e_out = try v.alloc.claimInternal();
    try v.alloc.freePendingForced(e_in, 250);
    const head = v.core.headPos();
    try v.alloc.freePendingForced(e_out, head + 5);
    try testing.expectEqual(@as(u64, 2), v.alloc.pendingCount());
    const appends0 = v.ram.appends.load(.monotonic);
    const checkpoints0 = ckpt.stats.meta_kv_checkpoints.load();

    try v.cp.checkpointCadence();

    // Both nodes appended, floors gone, states clean.
    try testing.expectEqual(appends0 + 2, v.ram.appends.load(.monotonic));
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
    try testing.expectEqual(node_cache.no_floor, b.dirtyFloor());
    try testing.expectEqual(@as(u64, 0), v.cp.dirtyNodeCount());
    try testing.expectEqual(checkpoints0 + 1, ckpt.stats.meta_kv_checkpoints.load());
    // The record: seq 2 at slot 2, tail = the head at cycle start (nothing
    // still dirty, nothing in flight), generation 2.
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u64, 3), v.cp.retireSeq());
    try testing.expectEqualSlices(u64, &.{2}, v.slots_written.items);
    var got = try v.newest();
    defer got.deinit(v.gpa);
    try testing.expectEqual(@as(u64, 2), got.seq);
    try testing.expectEqual(head, got.journal_tail_seq);
    try testing.expectEqual(@as(u64, 2), got.alloc_bitmap_generation);
    try testing.expectEqual(@as(u64, 100), got.node_seq_watermark);
    try testing.expectEqualSlices(TreeRoot, &v.roots, got.tree_roots);
    try testing.expectEqual(head, v.cp.lastLedgerTail());
    // The bitmap pages landed in the region (every page dirty at format).
    try testing.expect(!v.alloc.hasDirtyPages());
    try testing.expectEqual(alloc_ext.page_magic, std.mem.readInt(u32, v.bitmap[0..4], .little));
    // ONE barrier (#1); the record's durability rides the next one: the
    // watermark, the coverage clock and the elision tail have NOT moved.
    try testing.expectEqual(@as(u64, 1), v.barriers);
    try testing.expectEqual(@as(u64, 0), v.core.reusableUpto());
    try testing.expectEqual(@as(u64, 2), v.alloc.pendingCount());
    try testing.expectEqual(@as(u64, 0), v.cache.durableTail());
    try testing.expectEqual(@as(usize, 1), v.cp.reclaim.len());
    // The tape.
    const tape = v.cp.lastCycle();
    try testing.expectEqual(@as(u64, 2), tape.seq);
    try testing.expectEqual(@as(u64, 2), tape.dirty_collected);
    try testing.expectEqual(@as(u64, 2), tape.nodes_appended);
    try testing.expectEqual(tape.total_ns, tape.phasesSum());
    try testing.expect(tape.total_ns > 0);
    // The second barrier (the lane's next trailing one, here direct):
    // all three watermarks move on one clock; the free inside the window
    // releases, the one past the tail stays parked; the parkers wake.
    var space: sync.Notify = .init;
    v.cp.space_wake = .{ .notify = &space, .io = testing.io };
    var parker = space.notified();
    try testing.expect(!parker.enable(testing.io));
    try v.cp.syncDevice();
    try testing.expectEqual(@as(u64, 2), v.barriers);
    try testing.expectEqual(head, v.core.reusableUpto());
    try testing.expectEqual(head, v.cp.reusable_upto.load());
    try testing.expectEqual(head, v.cache.durableTail());
    try testing.expectEqual(@as(u64, 1), v.alloc.pendingCount());
    try testing.expect(!v.alloc.isAllocated(e_in));
    try testing.expect(v.alloc.isAllocated(e_out));
    try testing.expectEqual(@as(usize, 0), v.cp.reclaim.len());
    try testing.expect(parker.enable(testing.io));
    parker.cancel(testing.io);
    // A barrier with nothing pending moves nothing.
    try v.cp.syncDevice();
    try testing.expectEqual(head, v.core.reusableUpto());
    // The ring's admission bounds against the watermark now.
    try testing.expect(v.core.tryAdmit(4072 * 50, .user) != null);
}

test "the slot wraps at seq % 32 and newest-valid-wins follows the seq; a direct barriered cycle advances reusable_upto inside the call" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    // Four leaves: a 64 KiB node holds 15 appended pages, so one leaf alone
    // would fill at the 16th commit (its floor then pins the tail — the
    // structural seam's business, pinned by the declined case elsewhere).
    for (0..4) |e| try v.writeLeaf(e, e + 1);
    var leaves: [4]*CachedNode = undefined;
    for (0..4) |e| leaves[e] = (try v.cache.load(TestVol.addrOf(e))).?;
    defer for (leaves) |n| n.release();
    for (0..32) |i| {
        var k: [8]u8 = undefined;
        const s = try std.fmt.bufPrint(&k, "k{d}", .{i});
        _ = try v.commit(leaves[i % 4], &.{s}, 100);
        try v.cp.checkpointNow();
        try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    }
    try testing.expectEqual(@as(u64, 33), v.cp.checkpointSeq());
    try testing.expectEqual(@as(usize, 32), v.slots_written.items.len);
    for (v.slots_written.items, 0..) |slot, i| try testing.expectEqual((@as(u64, i) + 2) % 32, slot);
    var got = try v.newest();
    defer got.deinit(v.gpa);
    try testing.expectEqual(@as(u64, 33), got.seq);
    try testing.expectEqual(@as(u64, 1), got.slotIndex());
    // Two barriers per direct cycle.
    try testing.expectEqual(@as(u64, 64), v.barriers);
}

test "the tail is an ENTRY START rounded down below the still-dirty node's records and the in-flight reservation; a bounded pass flushes the oldest first" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    try v.writeLeaf(1, 2);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    const b = (try v.cache.load(TestVol.addrOf(1))).?;
    defer b.release();
    // Entry E1 → a; entry E2 holds three records, b holds only its LAST two
    // (its sibling in a flushed elsewhere), with E2's start as the floor.
    _ = try v.commit(a, &.{"a1"}, 100);
    const adm = v.core.tryAdmit(300, .user) orelse return error.RingFull;
    const e2 = try v.inflight.reserveRegistered(&v.core, adm);
    // b's records carry seqs E2.start + 1, +2 — its floor is E2.start.
    try v.applyAt(b, &.{ "b1", "b2" }, e2.res.start + 1, e2.res.start);
    v.inflight.complete(&v.core, e2.res);
    // A third entry still IN FLIGHT (reserved, not completed).
    const adm3 = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const e3 = try v.inflight.reserveRegistered(&v.core, adm3);
    try testing.expect(e3.res.start > e2.res.start);
    // A passed deadline admits exactly ONE node (progress): the oldest, a.
    try v.cp.checkpointCycleBounded(true, DrainBudget.bounded(1));
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
    try testing.expectEqual(e2.res.start, b.dirtyFloor());
    // tail = min(h, inflight e3, b's floor) = E2.start — strictly below b's
    // record seqs (E2.start + 1) and below the open reservation.
    try testing.expectEqual(e2.res.start, v.cp.lastLedgerTail());
    try testing.expect(v.cp.lastLedgerTail() < e2.res.start + 1);
    try testing.expectEqual(e2.res.start, v.core.reusableUpto());
    // Complete e3 and flush everything: the tail reaches the head.
    v.inflight.complete(&v.core, e3.res);
    try v.cp.checkpointNow();
    try testing.expectEqual(v.core.headPos(), v.cp.lastLedgerTail());
    try testing.expectEqual(node_cache.no_floor, b.dirtyFloor());
}

test "an in-flight reservation alone pins the tail (the dying-floor and in-flight clamps); the tape's decision words are 0 for a cycle no age decision fired" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    const adm = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const open = try v.inflight.reserveRegistered(&v.core, adm);
    const adm2 = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const done = try v.inflight.reserveRegistered(&v.core, adm2);
    v.inflight.complete(&v.core, done.res);
    try v.cp.checkpointNow();
    try testing.expectEqual(open.res.start, v.cp.lastLedgerTail());
    const tape = v.cp.lastCycle();
    try testing.expectEqual(@as(u64, 0), tape.late_ms);
    try testing.expectEqual(@as(u64, 0), tape.trigger_ms);
    try testing.expectEqual(@as(u64, 0), tape.dirty_collected);
    // A dying floor (an evicted node's) clamps too, and is drained by the
    // cycle that covered it.
    v.inflight.complete(&v.core, open.res);
    v.cache.noteDyingFloor(3);
    try v.cp.checkpointNow();
    try testing.expectEqual(@as(u64, 3), v.cp.lastLedgerTail());
    try testing.expectEqual(node_cache.no_floor, v.cache.takeDyingFloors());
    try v.cp.checkpointNow();
    try testing.expectEqual(v.core.headPos(), v.cp.lastLedgerTail());
}

test "a node whose frame no longer fits and no structural task bound is DECLINED: its floor stays and pins the tail (the wedge class's honest input); a bound seam that replaces it releases the tail" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    // 15 appended pages fill a 64 KiB node; the 16th freeze does not fit.
    var first: u64 = 0;
    for (0..16) |i| {
        var k: [8]u8 = undefined;
        const r = try v.commit(a, &.{try std.fmt.bufPrint(&k, "k{d}", .{i})}, 100);
        if (i == 15) first = r.start;
        if (i < 15) try v.cp.checkpointNow();
    }
    try v.cp.checkpointNow();
    try testing.expectEqual(first, a.dirtyFloor());
    try testing.expectEqual(first, v.cp.lastLedgerTail());
    try testing.expect(a.state.isFreezing());
    // A structural seam that answers `replaced` consumes the parked delta
    // (here: a stand-in that retires the node's floor the way a successor
    // publication would) and the next cycle's tail reaches the head.
    const Replacer = struct {
        fn maintain(_: *anyopaque, n: *CachedNode, opts: SmoSeam.MaintainOpts) SmoOutcome {
            std.debug.assert(opts.forced_retirement and opts.keep_exempt);
            _ = n.state.endFreeze();
            return .{ .replaced = .{ .images = 1 } };
        }
    };
    v.flusher.smo = .{ .ctx = &SmoSeam.none_ctx, .maintain = Replacer.maintain, .mergeUnderfull = SmoSeam.noSweep };
    try v.cp.checkpointNow();
    try testing.expectEqual(v.core.headPos(), v.cp.lastLedgerTail());
    const tape = v.cp.lastCycle();
    try testing.expectEqual(@as(u64, 1), tape.smo_nodes);
    try testing.expectEqual(@as(u64, 1), tape.images);
    // The per-image unit window received exactly one sample (its wall may
    // read 0 on a coarse clock; the count is the contract).
    try testing.expectEqual(@as(usize, 1), v.cp.image_units.next);
}

test "failure dispositions: a failed ledger write keeps the previous record authoritative and restores the dying floors; a failed barrier counts toward the latch and aborts before the record; three in a row latch FAILED" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    _ = try v.commit(a, &.{"a1"}, 100);
    try v.cp.checkpointNow();
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    const tail1 = v.cp.lastLedgerTail();
    // A failed ledger write.
    _ = try v.commit(a, &.{"a2"}, 100);
    v.cache.noteDyingFloor(7);
    v.fail_ledger = true;
    try testing.expectError(error.Io, v.cp.checkpointNow());
    v.fail_ledger = false;
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    try testing.expectEqual(tail1, v.cp.lastLedgerTail());
    try testing.expectEqual(@as(usize, 1), v.slots_written.items.len);
    // The dying floor folded back: the next record respects it.
    try v.cp.checkpointNow();
    try testing.expectEqual(@as(u64, 3), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u64, 7), v.cp.lastLedgerTail());
    var got = try v.newest();
    defer got.deinit(v.gpa);
    try testing.expectEqual(@as(u64, 3), got.seq);
    // A failed barrier #1: nothing written past it, the seq stays.
    _ = try v.commit(a, &.{"a3"}, 100);
    v.fail_barrier = true;
    try testing.expectError(error.Io, v.cp.checkpointNow());
    try testing.expectEqual(@as(u64, 3), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u32, 1), v.latch.barrier_failures.load(.acquire));
    try testing.expect(!v.cp.isFailed());
    try testing.expectError(error.Io, v.cp.syncDevice());
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Io, v.cp.syncDevice());
    try log.testing.expectAllFired();
    try testing.expect(v.cp.isFailed());
    // The appended bytes are on the device; the next good barrier covers them.
    v.fail_barrier = false;
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
}

test "the wedge rung: a tail pinned by a stuck in-flight reservation with retirements parked reaches FAILED on the 8th non-advancing barriered cycle and NOT before; a release in between resets the rung" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    // A completed entry so the head is past 0, then a reservation that never completes.
    const adm0 = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const done = try v.inflight.reserveRegistered(&v.core, adm0);
    v.inflight.complete(&v.core, done.res);
    const adm = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const stuck = try v.inflight.reserveRegistered(&v.core, adm);
    const s = stuck.res.start;
    // A retirement parked past the pinned tail; one that a release later resets the rung with.
    const e1 = try v.alloc.claimInternal();
    try v.alloc.freePendingForced(e1, s + 1);
    // Cycle 1: the tail advances 0 → s (progress).
    try v.cp.checkpointNow();
    try testing.expectEqual(s, v.cp.lastLedgerTail());
    try testing.expectEqual(@as(u64, 0), ckpt.stats.pending_free_stalled_cycles.load());
    // Cycles 2..8: seven non-advancing cycles, rung 1..7, not failed.
    for (1..8) |rung| {
        try v.cp.checkpointNow();
        try testing.expectEqual(@as(u64, rung), ckpt.stats.pending_free_stalled_cycles.load());
        try testing.expect(!v.cp.isFailed());
        try testing.expectEqual(@as(u64, 1), v.alloc.pendingCount());
    }
    // The pin moves: the stuck reservation completes and a new one opens
    // further on — the tail advances (progress, the rung resets) and the
    // parked free's gate is covered (released).
    v.inflight.complete(&v.core, stuck.res);
    const adm2 = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const stuck2 = try v.inflight.reserveRegistered(&v.core, adm2);
    const s2 = stuck2.res.start;
    try testing.expect(s2 > s + 1);
    try v.cp.checkpointNow();
    try testing.expectEqual(s2, v.cp.lastLedgerTail());
    try testing.expectEqual(@as(u64, 0), ckpt.stats.pending_free_stalled_cycles.load());
    try testing.expectEqual(@as(u64, 0), v.alloc.pendingCount());
    try testing.expect(!v.alloc.isAllocated(e1));
    try testing.expect(!v.cp.isFailed());
    // Re-park past the new pin and run the rung out: the 8th non-advancing
    // barriered cycle fails loud, once.
    const e2 = try v.alloc.claimInternal();
    try v.alloc.freePendingForced(e2, s2 + 1);
    for (1..8) |rung| {
        try v.cp.checkpointNow();
        try testing.expectEqual(@as(u64, rung), ckpt.stats.pending_free_stalled_cycles.load());
        try testing.expect(!v.cp.isFailed());
    }
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.VolumeFailed, v.cp.checkpointNow());
    try log.testing.expectAllFired();
    try testing.expect(v.cp.isFailed());
    try testing.expectEqual(@as(u64, 8), ckpt.stats.pending_free_stalled_cycles.load());
    try testing.expectEqual(@as(u64, 8), ckpt.PENDING_FREE_FORCE_CYCLES);
    try testing.expectError(error.VolumeFailed, v.latch.admitMutation());
    // The record itself landed (the audit runs after barrier #2).
    try testing.expectEqual(s2, v.cp.lastLedgerTail());
    try testing.expect(v.alloc.isAllocated(e2));
    v.inflight.complete(&v.core, stuck2.res);
}

test "the DENSE rule outside a cycle: a consumed seq restates the last record at the consumed seq (pages + barrier + record + barrier), counted; the cycle's next seq is dense behind it" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    _ = try v.commit(a, &.{"a1"}, 100);
    try v.cp.checkpointNow();
    const tail = v.cp.lastLedgerTail();
    const restated0 = ledger.meta_kv_ledger_restatements.load();
    const writes0 = ckpt.stats.meta_kv_ledger_writes.load();
    const barriers0 = v.barriers;
    _ = try v.alloc.claimInternal();
    try testing.expectEqual(@as(u64, 3), try v.cp.consumeCheckpointSeqForBitmap());
    try testing.expectEqual(restated0 + 1, ledger.meta_kv_ledger_restatements.load());
    try testing.expectEqual(writes0 + 1, ckpt.stats.meta_kv_ledger_writes.load());
    try testing.expectEqual(barriers0 + 2, v.barriers);
    try testing.expect(!v.alloc.hasDirtyPages());
    var got = try v.newest();
    defer got.deinit(v.gpa);
    try testing.expectEqual(@as(u64, 3), got.seq);
    try testing.expectEqual(tail, got.journal_tail_seq);
    try testing.expectEqual(@as(u64, 3), got.alloc_bitmap_generation);
    try testing.expectEqual(@as(u64, 4), v.cp.retireSeq());
    try v.cp.checkpointNow();
    try testing.expectEqual(@as(u64, 4), v.cp.checkpointSeq());
}

test "the task: a park-mark kick makes an immediate barriered cycle with a cadence far away; a plain threshold wake drains without a cycle; the stop latch runs the final fixpoint" {
    const v = try TestVol.create(10_000, 1_000_000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    var task = CheckpointTask.init(&v.cp, testing.io);
    const now = clock.monotonicNs();
    task.next_tick_ns = now + 10 * std.time.ns_per_s;
    // A threshold wake: the maintenance pass appends the queued node; no
    // record, no barrier.
    _ = try v.commit(a, &.{"a1"}, 100);
    try testing.expect(v.maintenance.push(a.addr));
    try testing.expect(v.cp.maintenancePending());
    task.kick(.threshold);
    try testing.expect(task.runOnce(true, now));
    try testing.expectEqual(@as(u64, 1), v.ram.appends.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u64, 0), v.barriers);
    try testing.expect(!v.cp.maintenancePending());
    // The floor survives a threshold append until a barrier covers it.
    try testing.expect(a.dirtyFloor() != node_cache.no_floor);
    // The park mark: promoted to a cadence tick whose cycle barriers NOW.
    const park0 = ckpt.stats.meta_kv_checkpoint_park_cycles.load();
    task.kick(.ring_park);
    try testing.expect(task.runOnce(true, now + 1));
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u64, 2), v.barriers);
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    try testing.expectEqual(park0 + 1, ckpt.stats.meta_kv_checkpoint_park_cycles.load());
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
    // The mark was consumed: the next wake is a plain one.
    try testing.expect(task.runOnce(true, now + 2));
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    // A cadence tick with nothing dirty and the ring covered runs no cycle
    // and issues no barrier of its own (the trailing coalesced barrier is
    // the lane's).
    try testing.expect(task.runOnce(false, task.next_tick_ns));
    try testing.expectEqual(@as(u64, 2), v.barriers);
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    // Stop: the final tick cycles to the fixpoint and the loop exits.
    _ = try v.commit(a, &.{"a2"}, 100);
    task.stop();
    try testing.expect(!task.runOnce(true, clock.monotonicNs()));
    try testing.expect(task.isStopped());
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    try testing.expect(!v.alloc.hasDirtyPages());
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
}

test "the task on its own thread under std.testing.io: the cadence fires off the clock, a kick wakes it, stop joins it with an empty replay window" {
    const v = try TestVol.create(5, 1);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    _ = try v.commit(a, &.{"a1"}, 100);
    var task = CheckpointTask.init(&v.cp, testing.io);
    const t = try std.Thread.spawn(.{}, CheckpointTask.runUntilStopped, .{&task});
    // The 5 ms cadence with a 1 ms max age: a cycle lands without any kick.
    while (v.cp.checkpointSeq() < 2) try std.Thread.yield();
    // A parked committer's kick lands another cycle promptly.
    _ = try v.commit(a, &.{"a2"}, 100);
    const seq = v.cp.checkpointSeq();
    task.kick(.ring_park);
    while (v.cp.checkpointSeq() <= seq) try std.Thread.yield();
    task.stop();
    t.join();
    try testing.expect(task.isStopped());
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    try testing.expectEqual(v.core.headPos(), v.cp.lastLedgerTail());
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
    // The shed hook is the kick: a permit for the next wait.
    const h = task.hook();
    h.call(h.ctx);
    try testing.expect(task.wake.permit);
}

test "ONE space wake: every barrier that moves reusable_upto — a direct checkpointNow (a committer's space retry, the lane's hole checkpoint), another issuer's landed ticket — wakes the admission parkers' Notify itself, with no task loop and no tick in between" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    var space: sync.Notify = .init;
    v.cp.space_wake = .{ .notify = &space, .io = testing.io };
    // A parker registers, then re-checks (the enable-then-recheck law);
    // nothing has moved yet.
    var parker = space.notified();
    try testing.expect(!parker.enable(testing.io));
    _ = try v.commit(a, &.{"a1"}, 100);
    // The committer's own space-retry cycle, on ITS thread: no `runOnce`.
    try v.cp.checkpointNow();
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    try testing.expect(parker.enable(testing.io));
    parker.cancel(testing.io);
    // Another issuer's barrier (the lane's) landing a ticket wakes too.
    var parker2 = space.notified();
    try testing.expect(!parker2.enable(testing.io));
    _ = try v.commit(a, &.{"a2"}, 100);
    try v.cp.checkpointCadence();
    try testing.expect(!parker2.enable(testing.io));
    v.cp.barrierLanded(v.cp.barrierStarted());
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    try testing.expect(parker2.enable(testing.io));
    parker2.cancel(testing.io);
    // A barrier that moves nothing wakes nobody.
    var parker3 = space.notified();
    try testing.expect(!parker3.enable(testing.io));
    try v.cp.syncDevice();
    try testing.expect(!parker3.enable(testing.io));
    parker3.cancel(testing.io);
}

test "the close ordering is executable: a reservation still open at the final tick fires the tripwire and the tick proceeds (its tail clamps below the open start) instead of waiting on a completer that no longer exists" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    _ = try v.commit(a, &.{"a1"}, 100);
    // A reservation nobody will complete: the shape `close` orders away
    // (the conveyor quiesced and the lane shut down before the task stops).
    const adm = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const open = try v.inflight.reserveRegistered(&v.core, adm);
    const fired0 = ckpt.stats.final_tick_open_reservations.load();
    log.testing.expectErrorLogs(1);
    try v.cp.tick(true, null, false, 50);
    try log.testing.expectAllFired();
    try testing.expectEqual(fired0 + 1, ckpt.stats.final_tick_open_reservations.load());
    try testing.expectEqual(open.res.start, v.cp.lastLedgerTail());
    try testing.expectEqual(node_cache.no_floor, a.dirtyFloor());
    v.inflight.complete(&v.core, open.res);
    // A quiet registry at the final tick fires nothing.
    try v.cp.tick(true, null, false, 50);
    try testing.expectEqual(fired0 + 1, ckpt.stats.final_tick_open_reservations.load());
    try testing.expectEqual(v.core.headPos(), v.cp.lastLedgerTail());
}

test "cover loops: coverBringUpResidue cycles until reusable_upto == head and is a no-op when covered; checkpointPast clears a hole end within the bound and refuses loud past it" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    try v.writeLeaf(0, 1);
    const a = (try v.cache.load(TestVol.addrOf(0))).?;
    defer a.release();
    _ = try v.commit(a, &.{"a1"}, 100);
    try testing.expect(v.core.reusableUpto() < v.core.headPos());
    try v.cp.coverBringUpResidue();
    try testing.expectEqual(v.core.headPos(), v.core.reusableUpto());
    const seq = v.cp.checkpointSeq();
    try v.cp.coverBringUpResidue();
    try testing.expectEqual(seq, v.cp.checkpointSeq());
    // A hole ending at the head clears in one cycle; one past a stuck
    // reservation never does and refuses after 8.
    _ = try v.commit(a, &.{"a2"}, 100);
    try v.cp.checkpointPast(v.core.headPos());
    const adm = v.core.tryAdmit(100, .user) orelse return error.RingFull;
    const stuck = try v.inflight.reserveRegistered(&v.core, adm);
    const before = v.cp.checkpointSeq();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.Corrupt, v.cp.checkpointPast(stuck.res.start + 1));
    try log.testing.expectAllFired();
    try testing.expectEqual(before + ckpt.HOLE_COVER_CYCLES_MAX, v.cp.checkpointSeq());
    v.inflight.complete(&v.core, stuck.res);
}

test "the tick: a due tick with the park mark records the decision words on the tape; another issuer's barrier (the lane's) drains the pending record through the DUR-3 faces; a dirty-cap tick cycles without an immediate barrier" {
    const v = try TestVol.create(50, 1000);
    defer v.destroy();
    for (0..5) |e| try v.writeLeaf(e, e + 1);
    var nodes: [5]*CachedNode = undefined;
    for (0..5) |e| nodes[e] = (try v.cache.load(TestVol.addrOf(e))).?;
    defer for (nodes) |n| n.release();
    // Dirty cap 4: five dirty nodes make a cadence tick's cycle due without
    // the age; the barrier is NOT immediate.
    for (nodes, 0..) |n, i| {
        var k: [4]u8 = undefined;
        _ = try v.commit(n, &.{try std.fmt.bufPrint(&k, "k{d}", .{i})}, 100);
    }
    const d = v.cp.decide(clock.monotonicNs(), false, false);
    try testing.expectEqual(ckpt.Reason.dirty_cap, d.reason);
    try testing.expect(!d.immediate_barrier);
    try v.cp.tick(false, clock.monotonicNs() + 50 * ns_per_ms, false, 50);
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    try testing.expectEqual(@as(u64, 1), v.barriers);
    try testing.expectEqual(@as(u64, 0), v.core.reusableUpto());
    // A tick with nothing due issues no barrier of its own; the record's
    // durability rides ANOTHER issuer's barrier (the lane's coalesced one):
    // a ticket taken before its device op, landed after — the pending
    // record drains and all three watermarks move, with no sink barrier.
    const syncs0 = ckpt.stats.meta_device_syncs.load();
    try v.cp.tick(false, clock.monotonicNs() + 50 * ns_per_ms, false, 50);
    try testing.expectEqual(@as(u64, 1), v.barriers);
    try testing.expectEqual(@as(u64, 0), v.core.reusableUpto());
    const ticket = v.cp.barrierStarted();
    try testing.expectEqual(@as(usize, 1), v.cp.reclaim.len());
    v.cp.barrierLanded(ticket);
    try testing.expectEqual(syncs0, ckpt.stats.meta_device_syncs.load());
    try testing.expectEqual(@as(u64, 1), v.barriers);
    try testing.expectEqual(v.cp.lastLedgerTail(), v.core.reusableUpto());
    try testing.expectEqual(v.cp.lastLedgerTail(), v.cache.durableTail());
    try testing.expectEqual(@as(usize, 0), v.cp.reclaim.len());
    try testing.expectEqual(@as(u64, 2), v.cp.checkpointSeq());
    // A ticket taken BEFORE a record's push never drains that record: the
    // push's epoch counted it as started.
    const early = v.cp.barrierStarted();
    _ = try v.commit(nodes[1], &.{"y"}, 100);
    try v.cp.checkpointCadence();
    try testing.expectEqual(@as(usize, 1), v.cp.reclaim.len());
    v.cp.barrierLanded(early);
    try testing.expectEqual(@as(usize, 1), v.cp.reclaim.len());
    v.cp.barrierLanded(v.cp.barrierStarted());
    try testing.expectEqual(@as(usize, 0), v.cp.reclaim.len());
    try testing.expectEqual(v.cp.lastLedgerTail(), v.core.reusableUpto());
    // Age-due on a forest volume records the decision words and folds a
    // term: the trigger is the max age less the anticipated term the same
    // decision measured, and the lateness is the span past it.
    v.cp.cfg.forest = true;
    v.cp.cfg.max_age_ms = 100;
    _ = try v.commit(nodes[0], &.{"z"}, 100);
    v.cp.collected_ns.store(clock.monotonicNs() -| 250 * ns_per_ms, .release);
    try v.cp.tick(false, clock.monotonicNs() + 50 * ns_per_ms, false, 50);
    try testing.expectEqual(@as(u64, 4), v.cp.checkpointSeq());
    const tape = v.cp.lastCycle();
    try testing.expectEqual(ckpt.checkpointTriggerMs(100, ckpt.anticipatedTermMs(tape.anticipated_ms, tape.projected_ms)), tape.trigger_ms);
    try testing.expect(tape.trigger_ms <= 100);
    try testing.expectEqual(@as(u64, 1), tape.dirty_at_decision);
    // ≈ 150 ms late (below the 200 ms landing-ceiling cap): the lateness
    // beyond one 50 ms tick rides the term on top of the landing wall.
    try testing.expect(tape.late_ms >= 250 - 100 and tape.late_ms < 200);
    try testing.expect(tape.landing_ns >= tape.collect_ns + tape.flush_ns + tape.pages_ns + tape.barrier_ns);
    try testing.expectEqual(tape.late_ms - 50, (tape.term_ns - tape.landing_ns) / ns_per_ms);
    try testing.expect(v.cp.lates.anticipatedNs() >= 150 * ns_per_ms);
    try testing.expect(v.cp.late_max_ms.load() >= 150);
    // The decision words were consumed: a direct cycle's tape reads 0.
    try v.cp.checkpointNow();
    try testing.expectEqual(@as(u64, 0), v.cp.lastCycle().late_ms);
    try testing.expectEqual(@as(u64, 0), v.cp.lastCycle().trigger_ms);
}
