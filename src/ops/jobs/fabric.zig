//! The fabric's PORTABLE core (charter §8 "Job fabric"; spec §1.1, §1.3,
//! §1.4, §1.7, §1.9): the live job table over the durable `job:` records,
//! `submit` (the coordinator-class act: durable Put FIRST), adoption (every
//! non-terminal record re-queued; Paused stays paused — operator intent
//! survives a crash), `claimNext` under the MOVER-SCOPE law (one mover per
//! volume; `WholeSet` conflicts with every mover; the later one stays
//! Queued LOUD), the CLAIM word (a CAS covering the whole worker-held
//! window, released on every exit — a leaked claim wedged a volume scope
//! forever), the state machine with the TERMINAL law (persist before the
//! live flip; the first terminal state wins; a regression is a tripwire),
//! the paused-PARK ceremony (durable `Paused`, then `settlePark` under the
//! state lock re-checks the intent — a `resume` that landed inside the
//! checkpoint window wins), the checkpoint cadence (256 tasks or 5 s),
//! Memory Red's shed hook (pause loudly; never self-resume), and the
//! worker loop every population runs (`serve`).
//!
//! Correctness is PLAN REGENERATION (KD-6): a job type's body regenerates
//! its plan from durable state at every pass; the record is advisory. The
//! body is a vtable per type the Linux coordinator registers; `noop` and
//! the error-returning probe are built in so the fabric's laws are pinned
//! here with no device. Every wait takes `io: std.Io`.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const sync = @import("../../sync/sync.zig");
const jc = @import("../../meta/journal_core.zig");
const records = @import("records.zig");
const throttle = @import("throttle.zig");
const stats_mod = @import("stats.zig");

const Atomic = std.atomic.Value;
const stats = stats_mod.stats;

pub const JobType = records.JobType;
pub const JobState = records.JobState;
pub const JobRecord = records.JobRecord;
pub const ErrorClass = records.ErrorClass;
pub const StableIdentity = records.StableIdentity;

/// ≈ 1 GiB of moved data per checkpoint at 4 MiB blocks — bounded re-work
/// on a crash; whichever of the two comes first.
pub const CHECKPOINT_TASKS: u64 = 256;
pub const CHECKPOINT_NS: u64 = 5 * std.time.ns_per_s;
/// A no-progress pass waits for quiescence / in-flight publishes.
pub const REPLAN_BACKOFF_NS: u64 = 200 * std.time.ns_per_ms;
/// Termination belt under churn: a maintenance job completes best-effort,
/// never wedges.
pub const DEFRAG_MAX_PASSES: u32 = 16;
/// A lost pool notify cannot strand a worker.
pub const WORKER_PARK_BOUND_NS: u64 = 500 * std.time.ns_per_ms;
/// The L4 service posture's slope, floored so a small box runs a
/// coordinator pair, ceilinged so a big one does not spawn a worker farm
/// for a duty-cycled plane.
pub fn defaultWorkers(cpus: u32) u32 {
    return @intCast(std.math.clamp(cpus / 4, 2, 8));
}
/// The most jobs one fabric tracks live (adopted + submitted): a bounded
/// table so the claim scan allocates nothing.
pub const MAX_JOBS: usize = 256;

/// A mover's scope: `Volume(v)` conflicts with the same volume, `WholeSet`
/// with every mover.
pub const Scope = union(enum) {
    none,
    volume: u64,
    whole_set,

    pub fn fromRecord(r: JobRecord) Scope {
        if (!r.job_type.isMover()) return .none;
        if (r.job_type == .rebalance or r.arg0 == 0) return .whole_set;
        return .{ .volume = r.arg0 };
    }

    pub fn intersects(a: Scope, b: Scope) bool {
        if (a == .none or b == .none) return false;
        if (a == .whole_set or b == .whole_set) return true;
        return a.volume == b.volume;
    }
};

/// How one PASS of a job body ended.
pub const PassOutcome = enum {
    /// The plan is empty and the terminal rule is met.
    completed,
    /// Progress was made; re-plan at once.
    progressed,
    /// No progress (deferred work, an empty census with blocks still
    /// allocated): re-plan after `REPLAN_BACKOFF`.
    deferred,
    /// The checkpoint re-verification found the slack consumed (§5.2).
    paused_capacity,
    /// The job's premise is gone (the victim is no longer draining): a
    /// contractual cancel.
    superseded,
};

pub const PassError = error{ ScopeIneligible, LeverOff, Io, Body, OutOfMemory };

/// What `taskDone` tells the body after each step.
pub const Step = enum { @"continue", stop };

/// A job TYPE's body: one pass regenerates the plan from durable state and
/// executes it, calling `fabric.taskDone(io, job, elapsed)` per step (the
/// throttle park, the checkpoint cadence, the control flags) and stopping
/// when it says so.
pub const Body = struct {
    ctx: *anyopaque,
    pass: *const fn (ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) PassError!PassOutcome,
};

/// The durable-record seam: tree 0 of volume 0 (the coordinator) or a RAM
/// map (tests). `put` is a conveyor tx; `list` yields every `Tag.job`
/// value in key order.
pub const Store = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const Error = error{ Io, OutOfMemory };
    pub const VTable = struct {
        put: *const fn (ctx: *anyopaque, job_id: u64, bytes: *const [records.JobRecordV1.LEN]u8) Error!void,
        list: *const fn (ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList([]u8)) Error!void,
    };

    pub fn put(self: Store, job_id: u64, bytes: *const [records.JobRecordV1.LEN]u8) Error!void {
        return self.vtable.put(self.ctx, job_id, bytes);
    }
    pub fn list(self: Store, gpa: Allocator, out: *std.ArrayList([]u8)) Error!void {
        return self.vtable.list(self.ctx, gpa, out);
    }
};

/// A RAM store: the tests' and the loopback coordinator's.
pub const RamStore = struct {
    gpa: Allocator,
    lock: jc.SpinLock = .{},
    map: std.AutoArrayHashMapUnmanaged(u64, [records.JobRecordV1.LEN]u8) = .empty,
    puts: u64 = 0,
    /// Test seam: fail the next N puts with `Io`.
    fail_puts: u32 = 0,

    pub fn init(gpa: Allocator) RamStore {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *RamStore) void {
        self.map.deinit(self.gpa);
    }

    pub fn store(self: *RamStore) Store {
        return .{ .ctx = self, .vtable = &vtable };
    }

    const vtable: Store.VTable = .{ .put = &put, .list = &list };

    fn put(ctx: *anyopaque, job_id: u64, bytes: *const [records.JobRecordV1.LEN]u8) Store.Error!void {
        const self: *RamStore = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        if (self.fail_puts > 0) {
            self.fail_puts -= 1;
            return error.Io;
        }
        self.map.put(self.gpa, job_id, bytes.*) catch return error.OutOfMemory;
        self.puts += 1;
    }

    fn list(ctx: *anyopaque, gpa: Allocator, out: *std.ArrayList([]u8)) Store.Error!void {
        const self: *RamStore = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const keys = self.map.keys();
        const sorted = gpa.dupe(u64, keys) catch return error.OutOfMemory;
        defer gpa.free(sorted);
        std.mem.sort(u64, sorted, {}, std.sort.asc(u64));
        for (sorted) |k| {
            const v = self.map.get(k).?;
            out.append(gpa, gpa.dupe(u8, &v) catch return error.OutOfMemory) catch return error.OutOfMemory;
        }
    }

    /// The durable image of a job (tests).
    pub fn record(self: *RamStore, job_id: u64) ?JobRecord {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.map.get(job_id) orelse return null;
        return records.JobRecordV1.decode(&v) catch null;
    }

    /// Test seam: plant raw bytes (a corrupt record).
    pub fn plant(self: *RamStore, job_id: u64, bytes: [records.JobRecordV1.LEN]u8) !void {
        self.lock.lock();
        defer self.lock.unlock();
        try self.map.put(self.gpa, job_id, bytes);
    }
};

/// The live control block of one job.
pub const JobCtl = struct {
    /// The durable image; read/written under `Fabric.lock`.
    rec: JobRecord,
    scope: Scope,
    /// The CAS word covering the worker-held window (release/acquire with
    /// the releasing exit).
    claimed: Atomic(bool) = .init(false),
    /// Operator pause intent (durable through `rec.pause_intent`).
    pause_intent: Atomic(bool) = .init(false),
    cancel_intent: Atomic(bool) = .init(false),
    ctl: throttle.Control = .{},
    /// Woken on every terminal transition (durable-then-visible).
    terminal: sync.Notify = .init,
    last_ckpt_tasks: u64 = 0,
    last_ckpt_ns: u64 = 0,
    /// Logged once per queued-loud episode.
    serialized_logged: bool = false,
    passes: u32 = 0,
    /// `seams.Throttle.forJob`'s storage.
    throttle_binding: ThrottleBinding = undefined,

    pub fn id(self: *const JobCtl) u64 {
        return self.rec.job_id;
    }

    pub fn jobType(self: *const JobCtl) JobType {
        return self.rec.job_type;
    }

    /// The body's control read per step: a pause or cancel intent ends the
    /// pass after the in-flight task.
    pub fn shouldStop(self: *const JobCtl) bool {
        return self.pause_intent.load(.acquire) or self.cancel_intent.load(.acquire);
    }
};

/// The (fabric, job) pair a `seams.Throttle` binds.
pub const ThrottleBinding = struct { f: *Fabric, j: *JobCtl };

pub const SubmitError = error{ NotCoordinator, TableFull, Io, OutOfMemory };
pub const ControlError = error{ UnknownJob, Terminal, Io, OutOfMemory };

/// What to submit.
pub const Submission = struct {
    job_type: JobType,
    /// 0 ⇒ the fabric's default (`job_cpu_limit`).
    throttle_pct: u8 = 0,
    arg0: u64 = 0,
    arg1: u64 = 0,
    /// Submitted already paused (an offline add's queued rebalance is NOT:
    /// it is queued; this is the operator's `--paused`).
    paused: bool = false,
};

pub const Fabric = struct {
    gpa: Allocator,
    store: Store,
    identity: StableIdentity,
    pid: u32,
    /// `SQUEEZEFS_JOB_CPU_LIMIT`: the default throttle of a submit without one.
    default_throttle_pct: u8,
    /// `SQUEEZEFS_TEST_JOB_PARK_DELAY_MS`: the park window's hold (0 = none).
    park_delay_ns: u64 = 0,
    /// The coordinator predicate (volume 0's manager): a non-coordinator
    /// SERVES shards but refuses `submit`.
    is_coordinator: Atomic(bool) = .init(true),
    /// The mount-time admission hold: adopt and re-queue, claim nothing
    /// until the allocation lease armed over a quiescent allocator.
    held: Atomic(bool) = .init(false),
    /// Workers exit on the next park.
    stopping: Atomic(bool) = .init(false),
    lock: sync.Mutex = .init,
    jobs: [MAX_JOBS]?*JobCtl = @splat(null),
    n_jobs: usize = 0,
    pool_wake: sync.Notify = .init,
    bodies: [JobType.count]?Body = @splat(null),
    random: *const fn () u64,
    /// Red pressure's instrument: the component's current level (the
    /// coordinator binds the copy-buffer gauge).
    copy_buffer_bytes: *counters.Gauge = &stats.job_copy_buffer_bytes,

    pub const Options = struct {
        identity: StableIdentity,
        pid: u32,
        default_throttle_pct: u8 = 100,
        park_delay_ns: u64 = 0,
        held: bool = false,
        random: *const fn () u64,
    };

    pub fn init(gpa: Allocator, store: Store, opts: Options) Fabric {
        std.debug.assert(opts.default_throttle_pct >= 1 and opts.default_throttle_pct <= 100);
        return .{ .gpa = gpa, .store = store, .identity = opts.identity, .pid = opts.pid, .default_throttle_pct = opts.default_throttle_pct, .park_delay_ns = opts.park_delay_ns, .held = .init(opts.held), .random = opts.random };
    }

    pub fn deinit(self: *Fabric) void {
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| self.gpa.destroy(jp);
        self.n_jobs = 0;
    }

    pub fn registerBody(self: *Fabric, t: JobType, body: Body) void {
        self.bodies[t.slot()] = body;
    }

    fn bodyOf(self: *Fabric, t: JobType) ?Body {
        return self.bodies[t.slot()];
    }

    pub fn openAdmission(self: *Fabric, io: Io) void {
        self.held.store(false, .release);
        self.pool_wake.notifyWaiters(io);
    }

    pub fn setCoordinator(self: *Fabric, on: bool) void {
        self.is_coordinator.store(on, .release);
    }

    // ---- the table ----

    pub fn find(self: *Fabric, job_id: u64) ?*JobCtl {
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| if (jp.rec.job_id == job_id) return jp;
        return null;
    }

    fn insertLocked(self: *Fabric, rec: JobRecord) SubmitError!*JobCtl {
        if (self.n_jobs >= MAX_JOBS) return error.TableFull;
        const j = self.gpa.create(JobCtl) catch return error.OutOfMemory;
        j.* = .{ .rec = rec, .scope = Scope.fromRecord(rec) };
        j.pause_intent.store(rec.pause_intent, .release);
        j.ctl.pct.store(rec.throttle_pct, .release);
        self.jobs[self.n_jobs] = j;
        self.n_jobs += 1;
        return j;
    }

    /// Live counts (the gauges are refreshed after every transition).
    fn refreshGaugesLocked(self: *Fabric) void {
        var active: u64 = 0;
        var queued: u64 = 0;
        var paused: u64 = 0;
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| switch (jp.rec.state) {
            .running => active += 1,
            .queued => queued += 1,
            .paused, .paused_capacity => paused += 1,
            else => {},
        };
        stats.job_active.set(active);
        stats.job_queued.set(queued);
        stats.job_paused.set(paused);
    }

    pub fn liveNonTerminal(self: *Fabric, io: Io) u64 {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        var n: u64 = 0;
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| if (!jp.rec.isTerminal()) {
            n += 1;
        };
        return n;
    }

    /// The durable write of a job's current image (the checkpoint).
    fn persistLocked(self: *Fabric, j: *JobCtl) Store.Error!void {
        j.rec.updated_realtime_s = clock.realtimeCoarseS();
        j.rec.pause_intent = j.pause_intent.load(.acquire);
        j.rec.capacity_pause = j.rec.state == .paused_capacity;
        const bytes = j.rec.encode();
        try self.store.put(j.rec.job_id, &bytes);
        j.last_ckpt_tasks = j.rec.tasks_done;
        j.last_ckpt_ns = clock.monotonicNs();
        stats.job_checkpoint_writes.inc();
    }

    // ---- submit / adopt ----

    /// Minting a job record IS the coordinator-class act: the durable Put
    /// first, the live entry after.
    pub fn submit(self: *Fabric, io: Io, s: Submission) SubmitError!u64 {
        if (!self.is_coordinator.load(.acquire)) return error.NotCoordinator;
        std.debug.assert(s.throttle_pct <= 100);
        var job_id = self.random();
        while (job_id == 0) job_id = self.random();
        const now = clock.realtimeCoarseS();
        const rec: JobRecord = .{
            .job_id = job_id,
            .job_type = s.job_type,
            .state = if (s.paused) .paused else .queued,
            .throttle_pct = if (s.throttle_pct == 0) self.default_throttle_pct else s.throttle_pct,
            .pause_intent = s.paused,
            .created_realtime_s = now,
            .created_by = self.identity,
            .created_pid = self.pid,
            .arg0 = s.arg0,
            .arg1 = s.arg1,
            .updated_realtime_s = now,
        };
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const bytes = rec.encode();
        self.store.put(job_id, &bytes) catch |e| return switch (e) {
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
        stats.job_checkpoint_writes.inc();
        _ = try self.insertLocked(rec);
        stats.job_submitted.inc();
        self.refreshGaugesLocked();
        self.pool_wake.notifyWaiters(io);
        return job_id;
    }

    /// Adoption at start: every non-terminal durable record is adopted;
    /// Running/Queued re-queue, Paused/PausedCapacity stay paused. An
    /// undecodable record is counted, logged by key and SKIPPED.
    pub fn adopt(self: *Fabric, io: Io) Store.Error!usize {
        var list: std.ArrayList([]u8) = .empty;
        defer {
            for (list.items) |v| self.gpa.free(v);
            list.deinit(self.gpa);
        }
        try self.store.list(self.gpa, &list);
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        var adopted: usize = 0;
        for (list.items, 0..) |v, i| {
            var rec = records.JobRecordV1.decode(v) catch {
                stats.job_records_corrupt.inc();
                log.err("job fabric: durable job record #{d} ({d} B) does not decode — skipped (the adopting scan never refuses the mount over one record)", .{ i, v.len });
                continue;
            };
            if (rec.isTerminal() or self.find(rec.job_id) != null) continue;
            const was = rec.state;
            if (rec.state == .running or rec.state == .queued) rec.state = .queued;
            if (rec.state == .queued) rec.pause_intent = false;
            const j = self.insertLocked(rec) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.TableFull => break,
                else => unreachable,
            };
            try self.persistLocked(j);
            adopted += 1;
            stats.job_adopted.inc();
            log.info("job fabric: adopted {f} ({s}, was {s}, now {s})", .{ rec, rec.job_type.name(), @tagName(was), @tagName(rec.state) });
        }
        self.refreshGaugesLocked();
        if (adopted > 0) self.pool_wake.notifyWaiters(io);
        return adopted;
    }

    // ---- the claim ----

    /// A candidate must be Queued, not paused, and its scope must not
    /// intersect a CLAIMED mover's. The CAS on `claimed` is the claim; the
    /// Running flip is checkpointed under the lock.
    pub fn claimNext(self: *Fabric, io: Io, for_wire: bool) ?*JobCtl {
        if (self.held.load(.acquire)) return null;
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        for (self.jobs[0..self.n_jobs]) |j| {
            const jp = j orelse continue;
            if (jp.rec.state != .queued or jp.pause_intent.load(.acquire)) continue;
            if (for_wire and !jp.rec.job_type.wireExecutable()) continue;
            if (self.scopeBusyLocked(jp)) {
                if (!jp.serialized_logged) {
                    jp.serialized_logged = true;
                    stats.job_serialized_waits.inc();
                    log.info("job fabric: {f} ({s}) queued behind a claimed mover on an intersecting scope (one mover per volume at a time)", .{ jp.rec, jp.rec.job_type.name() });
                }
                continue;
            }
            // acq_rel pairs with the release in `releaseClaim`: the claimer
            // sees every write the previous holder left.
            if (jp.claimed.cmpxchgStrong(false, true, .acq_rel, .acquire) != null) continue;
            jp.serialized_logged = false;
            jp.rec.state = .running;
            jp.ctl.clearStop();
            self.persistLocked(jp) catch {};
            self.refreshGaugesLocked();
            return jp;
        }
        return null;
    }

    fn scopeBusyLocked(self: *Fabric, cand: *JobCtl) bool {
        if (cand.scope == .none) return false;
        for (self.jobs[0..self.n_jobs]) |j| {
            const jp = j orelse continue;
            if (jp == cand or !jp.claimed.load(.acquire)) continue;
            if (jp.scope.intersects(cand.scope)) return true;
        }
        return false;
    }

    /// The RAII release every exit runs; a release by a non-holder is a
    /// tripwire.
    pub fn releaseClaim(self: *Fabric, io: Io, j: *JobCtl) void {
        // release pairs with `claimNext`'s acq_rel CAS.
        if (j.claimed.cmpxchgStrong(true, false, .release, .acquire) != null) {
            stats.job_claim_released_unheld.fire("a worker released a claim it did not hold");
            return;
        }
        self.pool_wake.notifyWaiters(io);
    }

    // ---- the state machine ----

    /// A terminal transition: persist FIRST, then flip, then wake. The first
    /// terminal state wins; a later different terminal is a tripwire.
    fn finishLocked(self: *Fabric, io: Io, j: *JobCtl, to: JobState, class: ErrorClass) void {
        std.debug.assert(to.isTerminal());
        if (j.rec.isTerminal()) {
            if (j.rec.state != to) stats.job_terminal_regressed.fire("a terminal job asked to change its terminal state");
            return;
        }
        var next = j.rec;
        next.state = to;
        next.error_class = class;
        next.pause_intent = false;
        next.capacity_pause = false;
        next.updated_realtime_s = clock.realtimeCoarseS();
        const bytes = next.encode();
        self.store.put(next.job_id, &bytes) catch |e| {
            // The record stays non-terminal on disk: the next adoption
            // re-queues it, which is the honest outcome of an unrecorded
            // completion (plan regeneration finds nothing to do).
            log.err("job fabric: {f}: the terminal checkpoint ({s}) failed ({s}); the live state flips anyway and the next adoption re-plans", .{ j.rec, @tagName(to), @errorName(e) });
        };
        stats.job_checkpoint_writes.inc();
        j.rec = next;
        switch (to) {
            .completed => stats.job_completed.inc(),
            .cancelled => stats.job_cancelled.inc(),
            .failed => stats.job_failed.inc(),
            else => unreachable,
        }
        self.refreshGaugesLocked();
        j.terminal.notifyWaiters(io);
    }

    pub fn pause(self: *Fabric, io: Io, job_id: u64) ControlError!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const j = self.find(job_id) orelse return error.UnknownJob;
        if (j.rec.isTerminal()) return error.Terminal;
        j.pause_intent.store(true, .release);
        if (j.rec.state == .queued) j.rec.state = .paused;
        j.ctl.interrupt(io);
        self.persistLocked(j) catch |e| return storeErr(e);
        self.refreshGaugesLocked();
    }

    pub fn resumeJob(self: *Fabric, io: Io, job_id: u64) ControlError!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const j = self.find(job_id) orelse return error.UnknownJob;
        if (j.rec.isTerminal()) return error.Terminal;
        try self.resumeLocked(io, j);
    }

    fn resumeLocked(self: *Fabric, io: Io, j: *JobCtl) ControlError!void {
        j.pause_intent.store(false, .release);
        if (j.rec.state.isPaused()) j.rec.state = .queued;
        self.persistLocked(j) catch |e| return storeErr(e);
        self.refreshGaugesLocked();
        self.pool_wake.notifyWaiters(io);
    }

    /// Terminal, durable; also clears the intent so a paused job terminates.
    pub fn cancel(self: *Fabric, io: Io, job_id: u64) ControlError!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const j = self.find(job_id) orelse return error.UnknownJob;
        if (j.rec.isTerminal()) return error.Terminal;
        j.cancel_intent.store(true, .release);
        j.pause_intent.store(false, .release);
        j.ctl.interrupt(io);
        // A claimed job finishes at its worker's next control read (the
        // in-flight task completes); an unclaimed one is terminal now.
        if (!j.claimed.load(.acquire)) self.finishLocked(io, j, .cancelled, .none);
    }

    /// The live retune: the atomic, then the checkpoint. A remote shard in
    /// flight keeps its granted duty.
    pub fn setThrottle(self: *Fabric, io: Io, job_id: u64, pct: u8) ControlError!void {
        std.debug.assert(pct <= 100);
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const j = self.find(job_id) orelse return error.UnknownJob;
        if (j.rec.isTerminal()) return error.Terminal;
        j.rec.throttle_pct = pct;
        j.ctl.setPct(io, pct);
        self.persistLocked(j) catch |e| return storeErr(e);
    }

    fn storeErr(e: Store.Error) ControlError {
        return switch (e) {
            error.Io => error.Io,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    /// Memory Red (charter §6.2 "jobs pause loudly"): the `job_copy_buffers`
    /// component's shed hook — pauses every RUNNING job when real copy
    /// buffers exist (gauge > target), a `warn` line naming gauge > target;
    /// deliberately NOT self-resuming (`job resume` is the operator's).
    pub fn shedTo(self: *Fabric, io: Io, target: u64) void {
        const gauge = self.copy_buffer_bytes.load();
        if (gauge <= target) return;
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        var paused: usize = 0;
        for (self.jobs[0..self.n_jobs]) |j| {
            const jp = j orelse continue;
            if (jp.rec.state != .running and jp.rec.state != .queued) continue;
            jp.pause_intent.store(true, .release);
            if (jp.rec.state == .queued) jp.rec.state = .paused;
            jp.ctl.interrupt(io);
            self.persistLocked(jp) catch {};
            stats.job_paused_mem_pressure.inc();
            paused += 1;
        }
        self.refreshGaugesLocked();
        if (paused > 0) log.warn("job fabric: memory Red — job_copy_buffer_bytes {d} > target {d}: {d} job(s) paused; they do NOT self-resume (run `squeezefs job resume`)", .{ gauge, target, paused });
    }

    /// Wait for a job's terminal record (durable before the wake).
    pub fn waitTerminal(self: *Fabric, io: Io, job_id: u64, deadline_ns: u64) ?JobState {
        while (true) {
            self.lock.lockUncancelable(io);
            const j = self.find(job_id) orelse {
                self.lock.unlock(io);
                return null;
            };
            if (j.rec.isTerminal()) {
                const s = j.rec.state;
                self.lock.unlock(io);
                return s;
            }
            var n = j.terminal.notified();
            _ = n.enable(io);
            self.lock.unlock(io);
            if (n.waitUntilUncancelable(io, deadline_ns) == .deadline) return null;
        }
    }

    /// A snapshot of a job's record (`job status`).
    pub fn snapshot(self: *Fabric, io: Io, job_id: u64) ?JobRecord {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        const j = self.find(job_id) orelse return null;
        return j.rec;
    }

    /// Every live record in table order (`job list`); `out` owned by the caller.
    pub fn listJobs(self: *Fabric, io: Io, gpa: Allocator, out: *std.ArrayList(JobRecord)) Allocator.Error!void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| try out.append(gpa, jp.rec);
    }

    // ---- the body's per-step face ----

    /// Called by a body after each task: the task count, the checkpoint
    /// cadence (256 tasks or 5 s), the throttle park, the control flags.
    pub fn taskDone(self: *Fabric, io: Io, j: *JobCtl, elapsed_ns: u64) Step {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            j.rec.tasks_done += 1;
            stats.job_tasks_done.inc();
            const now = clock.monotonicNs();
            if (j.rec.tasks_done - j.last_ckpt_tasks >= CHECKPOINT_TASKS or now - j.last_ckpt_ns >= CHECKPOINT_NS) self.persistLocked(j) catch {};
        }
        if (j.shouldStop()) return .stop;
        return switch (throttle.dutyPark(io, &j.ctl, elapsed_ns)) {
            .served => if (j.shouldStop()) .stop else .@"continue",
            .interrupted => .stop,
        };
    }

    /// Publish `tasks_total` after a re-plan (`done + remaining`).
    pub fn setRemaining(self: *Fabric, io: Io, j: *JobCtl, remaining: u64) void {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        j.rec.tasks_total = j.rec.tasks_done + remaining;
    }

    // ---- the worker ----

    /// Run ONE claimed job to its exit: passes until terminal/parked; the
    /// claim released on every exit. Returns the job's state at exit.
    pub fn runClaimed(self: *Fabric, io: Io, j: *JobCtl) JobState {
        defer self.releaseClaim(io, j);
        const body = self.bodyOf(j.rec.job_type) orelse {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            log.err("job fabric: {f}: no body registered for {s} on this node — failed", .{ j.rec, j.rec.job_type.name() });
            self.finishLocked(io, j, .failed, .body);
            return j.rec.state;
        };
        while (true) {
            if (j.cancel_intent.load(.acquire)) {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                self.finishLocked(io, j, .cancelled, .none);
                return j.rec.state;
            }
            if (j.pause_intent.load(.acquire)) {
                if (self.park(io, j) == .resumed) continue;
                return j.rec.state;
            }
            j.passes += 1;
            const outcome = body.pass(body.ctx, io, self, j) catch |e| {
                self.lock.lockUncancelable(io);
                defer self.lock.unlock(io);
                const class: ErrorClass = switch (e) {
                    error.ScopeIneligible => .scope_ineligible,
                    error.LeverOff => .lever_off,
                    error.Io => .io,
                    error.Body, error.OutOfMemory => .body,
                };
                log.err("job fabric: {f} ({s}) failed: {s}", .{ j.rec, j.rec.job_type.name(), @errorName(e) });
                self.finishLocked(io, j, .failed, class);
                return j.rec.state;
            };
            switch (outcome) {
                .completed => {
                    self.lock.lockUncancelable(io);
                    defer self.lock.unlock(io);
                    self.finishLocked(io, j, .completed, .none);
                    return j.rec.state;
                },
                .superseded => {
                    self.lock.lockUncancelable(io);
                    defer self.lock.unlock(io);
                    self.finishLocked(io, j, .cancelled, .none);
                    return j.rec.state;
                },
                .progressed => {},
                .deferred => {
                    if (j.shouldStop()) continue;
                    const deadline = clock.monotonicNs() + REPLAN_BACKOFF_NS;
                    var n = j.ctl.wake.notified();
                    _ = n.waitUntilUncancelable(io, deadline);
                },
                .paused_capacity => {
                    self.lock.lockUncancelable(io);
                    j.rec.state = .paused_capacity;
                    j.pause_intent.store(true, .release);
                    self.persistLocked(j) catch {};
                    stats.job_paused_capacity.inc();
                    self.refreshGaugesLocked();
                    self.lock.unlock(io);
                    log.warn("job fabric: {f}: the capacity re-verification refused — PausedCapacity; free space and `squeezefs job resume`", .{j.rec});
                    return j.rec.state;
                },
            }
        }
    }

    /// A remote whole-job shard landed (the coordinator's `result_submit`):
    /// the job completes without a local body; the claim releases.
    pub fn finishRemote(self: *Fabric, io: Io, j: *JobCtl, to: JobState) void {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            self.finishLocked(io, j, to, .none);
        }
        self.releaseClaim(io, j);
    }

    /// A remote shard's lease expired or was abandoned: the job goes back
    /// to Queued for any population, durably, and the claim releases.
    pub fn requeue(self: *Fabric, io: Io, j: *JobCtl) void {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            if (!j.rec.isTerminal()) {
                j.rec.state = .queued;
                self.persistLocked(j) catch {};
                self.refreshGaugesLocked();
            }
        }
        self.releaseClaim(io, j);
    }

    const ParkVerdict = enum { parked, resumed };

    /// The paused-park ceremony: the durable `Paused` checkpoint, the
    /// (test-seam) window, then `settlePark` under the state lock
    /// re-checks the intent — a `resume` that landed inside the window
    /// wins (`Queued`, re-asserted durably, pool woken) instead of being
    /// overwritten by the stale flip, which left the job unclaimable
    /// forever (the lost-resume strand).
    fn park(self: *Fabric, io: Io, j: *JobCtl) ParkVerdict {
        {
            self.lock.lockUncancelable(io);
            defer self.lock.unlock(io);
            if (j.rec.state == .running) j.rec.state = .paused;
            self.persistLocked(j) catch {};
            self.refreshGaugesLocked();
        }
        // The window is open: the durable Paused is on disk, the settle
        // has not run.
        stats.job_park_entries.inc();
        if (self.park_delay_ns != 0) {
            var n = j.ctl.wake.notified();
            _ = n.waitUntilUncancelable(io, clock.monotonicNs() + self.park_delay_ns);
        }
        return self.settlePark(io, j);
    }

    fn settlePark(self: *Fabric, io: Io, j: *JobCtl) ParkVerdict {
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        if (j.cancel_intent.load(.acquire)) {
            self.finishLocked(io, j, .cancelled, .none);
            return .parked;
        }
        if (!j.pause_intent.load(.acquire)) {
            // The resume landed inside the window: this worker keeps the
            // claim and continues (Running again, durably).
            j.rec.state = .running;
            j.ctl.clearStop();
            self.persistLocked(j) catch {};
            self.refreshGaugesLocked();
            return .resumed;
        }
        return .parked;
    }

    /// The worker loop every population runs: claim → run → park on the
    /// pool with a 500 ms bound (a lost notify cannot strand it) until
    /// `stop`. `for_wire` restricts to wire-executable types.
    pub fn serve(self: *Fabric, io: Io, for_wire: bool) void {
        stats.job_workers.add(1);
        defer stats.job_workers.sub(1);
        while (!self.stopping.load(.acquire)) {
            if (self.claimNext(io, for_wire)) |j| {
                _ = self.runClaimed(io, j);
                continue;
            }
            var n = self.pool_wake.notified();
            _ = n.waitUntilUncancelable(io, clock.monotonicNs() + WORKER_PARK_BOUND_NS);
        }
    }

    pub fn stop(self: *Fabric, io: Io) void {
        self.stopping.store(true, .release);
        self.pool_wake.notifyWaiters(io);
        self.lock.lockUncancelable(io);
        defer self.lock.unlock(io);
        for (self.jobs[0..self.n_jobs]) |j| if (j) |jp| jp.ctl.interrupt(io);
    }
};

// ---------------------------------------------------------------------------
// The built-in bodies: `noop` (the probe) and the error-returning probe.
// ---------------------------------------------------------------------------

/// `noop{tasks, task_ms}`: `tasks_done` is the advisory cursor; each task
/// is a bounded wait of `task_ms` (a `Notify` deadline, never a sleep).
pub const NoopBody = struct {
    /// Test seam: fail the pass with `Body` once `tasks_done ≥ fail_at`.
    fail_at: ?u64 = null,
    /// The task-active ns this body accumulated (the duty gate's numerator).
    active_ns: Atomic(u64) = .init(0),
    wake: sync.Notify = .init,

    pub fn body(self: *NoopBody) Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, j: *JobCtl) PassError!PassOutcome {
        const self: *NoopBody = @ptrCast(@alignCast(ctx));
        const total = j.rec.arg0;
        const task_ns = j.rec.arg1 * std.time.ns_per_ms;
        f.setRemaining(io, j, total -| j.rec.tasks_done);
        while (j.rec.tasks_done < total) {
            if (self.fail_at) |at| if (j.rec.tasks_done >= at) return error.Body;
            const t0 = clock.monotonicNs();
            if (task_ns > 0) {
                var n = self.wake.notified();
                _ = n.waitUntilUncancelable(io, t0 + task_ns);
            }
            const elapsed = clock.monotonicNs() - t0;
            _ = self.active_ns.fetchAdd(elapsed, .monotonic);
            if (f.taskDone(io, j, elapsed) == .stop) return .progressed;
        }
        return .completed;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

var test_seed: Atomic(u64) = .init(0x1234_5678_9abc_def1);
fn testRandom() u64 {
    return test_seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

const Rig = struct {
    store: RamStore,
    fabric: Fabric,
    noop: NoopBody = .{},

    fn init(gpa: Allocator, opts: struct { held: bool = false, park_delay_ns: u64 = 0 }) !*Rig {
        const r = try gpa.create(Rig);
        r.* = .{ .store = RamStore.init(gpa), .fabric = undefined };
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 7, .mount_slot = 1 }, .pid = 99, .held = opts.held, .park_delay_ns = opts.park_delay_ns, .random = &testRandom });
        r.fabric.registerBody(.noop, r.noop.body());
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.fabric.deinit();
        self.store.deinit();
        gpa.destroy(self);
    }

    fn worker(self: *Rig, io: Io) void {
        self.fabric.serve(io, false);
    }
};

/// The closure identity judged as a DELTA (the counters are process-wide
/// and earlier tests' fabrics are gone): `Δsubmitted ≡ Δcompleted +
/// Δcancelled + Δfailed + live`.
const Delta = struct {
    submitted: u64,
    completed: u64,
    cancelled: u64,
    failed: u64,

    fn now() Delta {
        return .{ .submitted = stats.job_submitted.load(), .completed = stats.job_completed.load(), .cancelled = stats.job_cancelled.load(), .failed = stats.job_failed.load() };
    }

    fn closes(self: Delta, base: Delta, live: u64) bool {
        return self.submitted - base.submitted == (self.completed - base.completed) + (self.cancelled - base.cancelled) + (self.failed - base.failed) + live;
    }
};

test "noop job runs to completion and the record is durable: the terminal record is on disk BEFORE the waiter wakes; submitted ≡ completed + live" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const d0 = Delta.now();
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 5, .arg1 = 0 });
    try testing.expectEqual(JobState.queued, r.store.record(id).?.state);
    try testing.expect(Delta.now().closes(d0, r.fabric.liveNonTerminal(io)));
    const th = try std.Thread.spawn(.{}, Rig.worker, .{ r, io });
    const st = r.fabric.waitTerminal(io, id, clock.monotonicNs() + 10 * std.time.ns_per_s);
    try testing.expectEqual(@as(?JobState, .completed), st);
    try testing.expectEqual(JobState.completed, r.store.record(id).?.state);
    try testing.expectEqual(@as(u64, 5), r.store.record(id).?.tasks_done);
    r.fabric.stop(io);
    th.join();
    try testing.expect(Delta.now().closes(d0, r.fabric.liveNonTerminal(io)));
    try testing.expectEqual(@as(u64, 0), stats.job_active.load());
}

test "a held fabric claims no job until admission opens; a non-coordinator refuses submit" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{ .held = true });
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1 });
    try testing.expectEqual(@as(?*JobCtl, null), r.fabric.claimNext(io, false));
    r.fabric.openAdmission(io);
    const j = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(id, j.id());
    try testing.expectEqual(JobState.running, r.store.record(id).?.state);
    try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, j));
    r.fabric.setCoordinator(false);
    try testing.expectError(error.NotCoordinator, r.fabric.submit(io, .{ .job_type = .noop }));
}

test "pause / resume / cancel / live rethrottle: the intent flips the state durably; resume re-queues; cancel of a paused job is terminal; a terminal job refuses control; the rethrottle reaches the live word" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 3, .throttle_pct = 50 });
    try r.fabric.pause(io, id);
    try testing.expectEqual(JobState.paused, r.store.record(id).?.state);
    try testing.expect(r.store.record(id).?.pause_intent);
    try testing.expectEqual(@as(?*JobCtl, null), r.fabric.claimNext(io, false));
    try r.fabric.resumeJob(io, id);
    try testing.expectEqual(JobState.queued, r.store.record(id).?.state);
    try testing.expect(!r.store.record(id).?.pause_intent);
    try r.fabric.setThrottle(io, id, 10);
    try testing.expectEqual(@as(u8, 10), r.fabric.find(id).?.ctl.loadPct());
    try testing.expectEqual(@as(u8, 10), r.store.record(id).?.throttle_pct);
    try r.fabric.pause(io, id);
    try r.fabric.cancel(io, id);
    try testing.expectEqual(JobState.cancelled, r.store.record(id).?.state);
    try testing.expectError(error.Terminal, r.fabric.resumeJob(io, id));
    try testing.expectError(error.Terminal, r.fabric.pause(io, id));
    try testing.expectError(error.Terminal, r.fabric.setThrottle(io, id, 5));
    try testing.expectError(error.UnknownJob, r.fabric.pause(io, 0xdead));
    // A second terminal with the SAME state is idempotent; a different one
    // would trip (exercised through cancel-after-complete below).
    const id2 = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1 });
    const j2 = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, j2));
    try testing.expectError(error.Terminal, r.fabric.cancel(io, id2));
}

test "resume inside the park window never strands the job: with the park delay seam, a resume landing between the Paused checkpoint and settlePark leaves the job Running under the same claim" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{ .park_delay_ns = 300 * std.time.ns_per_ms });
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1000, .arg1 = 1 });
    const th = try std.Thread.spawn(.{}, Rig.worker, .{ r, io });
    // Let it run a few tasks, then pause.
    var spins: usize = 0;
    while (r.fabric.snapshot(io, id).?.tasks_done < 3 and spins < 5000) : (spins += 1) {
        var n = r.noop.wake.notified();
        _ = n.waitUntilUncancelable(io, clock.monotonicNs() + std.time.ns_per_ms);
    }
    const parks_before = stats.job_park_entries.load();
    try r.fabric.pause(io, id);
    // Wait for the park ENTRY (the durable Paused checkpoint landed).
    spins = 0;
    while (stats.job_park_entries.load() == parks_before and spins < 5000) : (spins += 1) {
        var n = r.noop.wake.notified();
        _ = n.waitUntilUncancelable(io, clock.monotonicNs() + std.time.ns_per_ms);
    }
    try testing.expect(stats.job_park_entries.load() > parks_before);
    try testing.expectEqual(JobState.paused, r.store.record(id).?.state);
    // The resume lands INSIDE the window.
    try r.fabric.resumeJob(io, id);
    // The worker settles: Running again, still claimed, and keeps moving.
    const done_at_resume = r.fabric.snapshot(io, id).?.tasks_done;
    spins = 0;
    while (r.fabric.snapshot(io, id).?.tasks_done < done_at_resume + 3 and spins < 5000) : (spins += 1) {
        var n = r.noop.wake.notified();
        _ = n.waitUntilUncancelable(io, clock.monotonicNs() + std.time.ns_per_ms);
    }
    try testing.expectEqual(JobState.running, r.fabric.snapshot(io, id).?.state);
    try testing.expect(r.fabric.find(id).?.claimed.load(.acquire));
    try testing.expect(!r.store.record(id).?.pause_intent);
    try r.fabric.cancel(io, id);
    try testing.expectEqual(@as(?JobState, .cancelled), r.fabric.waitTerminal(io, id, clock.monotonicNs() + 10 * std.time.ns_per_s));
    r.fabric.stop(io);
    th.join();
}

test "cancel inside the park window stays terminal; a parked job is re-claimable by another worker after resume (the claim was released)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1000, .arg1 = 1 });
    const j = r.fabric.claimNext(io, false).?;
    try r.fabric.pause(io, id);
    try testing.expectEqual(JobState.paused, r.fabric.runClaimed(io, j));
    try testing.expect(!j.claimed.load(.acquire));
    try testing.expectEqual(JobState.paused, r.store.record(id).?.state);
    try r.fabric.resumeJob(io, id);
    const again = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(j, again);
    try r.fabric.cancel(io, id);
    try testing.expectEqual(JobState.cancelled, r.fabric.runClaimed(io, again));
    try testing.expectEqual(JobState.cancelled, r.store.record(id).?.state);
}

test "crash resume reconstructs from durable records: a fresh fabric over the same store adopts Running/Queued as Queued, Paused stays paused, terminal and corrupt records are skipped (counted, logged)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const done = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 0 });
    try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    const running = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1000, .arg1 = 1 });
    const queued = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1 });
    const paused = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1, .paused = true });
    try testing.expectEqual(running, r.fabric.claimNext(io, false).?.id());
    try testing.expectEqual(JobState.running, r.store.record(running).?.state);
    var corrupt: [records.JobRecordV1.LEN]u8 = @splat(0xEE);
    try r.store.plant(0x77, corrupt);
    corrupt = undefined;
    // "Crash": a second fabric over the same store.
    var f2 = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 8, .mount_slot = 2 }, .pid = 100, .random = &testRandom });
    defer f2.deinit();
    log.testing.expectErrorLogs(1);
    const n = try f2.adopt(io);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(usize, 3), n);
    try testing.expectEqual(JobState.queued, f2.snapshot(io, running).?.state);
    try testing.expectEqual(JobState.queued, f2.snapshot(io, queued).?.state);
    try testing.expectEqual(JobState.paused, f2.snapshot(io, paused).?.state);
    try testing.expect(f2.snapshot(io, paused).?.pause_intent);
    try testing.expectEqual(@as(?JobRecord, null), f2.snapshot(io, done));
    try testing.expectEqual(@as(?JobRecord, null), f2.snapshot(io, 0x77));
    try testing.expectEqual(JobState.queued, r.store.record(running).?.state);
    // Adoption is idempotent (the corrupt record is skipped loud again).
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(@as(usize, 0), try f2.adopt(io));
    try log.testing.expectAllFired();
}

test "an error-returning job releases its claim, lands Failed with a typed class, and keeps the worker" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    r.noop.fail_at = 2;
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 10 });
    const j = r.fabric.claimNext(io, false).?;
    log.testing.expectErrorLogs(1);
    try testing.expectEqual(JobState.failed, r.fabric.runClaimed(io, j));
    try log.testing.expectAllFired();
    try testing.expectEqual(ErrorClass.body, r.store.record(id).?.error_class);
    try testing.expectEqual(@as(u64, 2), r.store.record(id).?.tasks_done);
    try testing.expect(!j.claimed.load(.acquire));
    try testing.expectEqual(@as(?JobState, .failed), r.fabric.waitTerminal(io, id, clock.monotonicNs() + std.time.ns_per_s));
    // The same worker (this thread) claims and completes a second job.
    r.noop.fail_at = null;
    const id2 = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1 });
    try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try testing.expectEqual(JobState.completed, r.store.record(id2).?.state);
}

test "intersecting-scope movers serialize (queued loud, once), disjoint scopes run concurrently, WholeSet conflicts with every mover, and the second runs once the first releases" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const a = try r.fabric.submit(io, .{ .job_type = .evacuate_volume, .arg0 = 11 });
    const b = try r.fabric.submit(io, .{ .job_type = .defrag_data, .arg0 = 11 });
    const c = try r.fabric.submit(io, .{ .job_type = .defrag_pack, .arg0 = 22 });
    const d = try r.fabric.submit(io, .{ .job_type = .rebalance });
    const e = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 0 });
    const ja = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(a, ja.id());
    const waits = stats.job_serialized_waits.load();
    const jcc = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(c, jcc.id()); // b skipped loud; c disjoint
    try testing.expectEqual(waits + 1, stats.job_serialized_waits.load());
    const je = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(e, je.id()); // d (WholeSet) skipped; e has no scope
    try testing.expectEqual(waits + 2, stats.job_serialized_waits.load());
    try testing.expectEqual(@as(?*JobCtl, null), r.fabric.claimNext(io, false));
    // No double count on a repeated scan.
    try testing.expectEqual(waits + 2, stats.job_serialized_waits.load());
    r.fabric.releaseClaim(io, ja);
    const jb = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(b, jb.id());
    r.fabric.releaseClaim(io, jb);
    r.fabric.releaseClaim(io, jcc);
    const jd = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(d, jd.id());
    // for_wire restricts to wire-executable types.
    r.fabric.releaseClaim(io, jd);
    r.fabric.releaseClaim(io, je);
    try testing.expectEqual(@as(?*JobCtl, null), r.fabric.claimNext(io, true));
    try testing.expect(Scope.intersects(.whole_set, .{ .volume = 1 }) and !Scope.intersects(.none, .whole_set) and !Scope.intersects(.{ .volume = 1 }, .{ .volume = 2 }));
}

test "the claim word: a release by a non-holder is a tripwire, never a wedge" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1 });
    const j = r.fabric.find(id).?;
    const before = stats.job_claim_released_unheld.load();
    log.testing.expectErrorLogs(1);
    stats.job_claim_released_unheld.resetLogBudgetForTest();
    r.fabric.releaseClaim(io, j);
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 1, stats.job_claim_released_unheld.load());
    try testing.expect(r.fabric.claimNext(io, false) != null);
}

test "Red pressure pauses running and queued jobs loudly and never self-resumes; a shed at/under target is a no-op; operator resume recovers" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 1000, .arg1 = 1 });
    const j = r.fabric.claimNext(io, false).?;
    const paused_before = stats.job_paused_mem_pressure.load();
    stats.job_copy_buffer_bytes.set(0);
    r.fabric.shedTo(io, 0);
    try testing.expectEqual(paused_before, stats.job_paused_mem_pressure.load());
    stats.job_copy_buffer_bytes.set(64 << 20);
    r.fabric.shedTo(io, 1 << 20);
    stats.job_copy_buffer_bytes.set(0);
    try testing.expectEqual(paused_before + 1, stats.job_paused_mem_pressure.load());
    try testing.expectEqual(JobState.paused, r.fabric.runClaimed(io, j));
    try testing.expectEqual(JobState.paused, r.store.record(id).?.state);
    try testing.expectEqual(@as(?*JobCtl, null), r.fabric.claimNext(io, false));
    try r.fabric.resumeJob(io, id);
    try testing.expect(r.fabric.claimNext(io, false) != null);
}

test "checkpoint cadence: tasks_done persists every 256 tasks; the terminal record carries the final count; the default worker count is clamp(cpus/4, 2, 8)" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa, .{});
    defer r.deinit(gpa);
    const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 600 });
    const j = r.fabric.claimNext(io, false).?;
    const puts0 = r.store.puts;
    try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, j));
    // 600 tasks ⇒ 2 cadence checkpoints (256, 512) + the terminal one.
    try testing.expectEqual(puts0 + 3, r.store.puts);
    try testing.expectEqual(@as(u64, 600), r.store.record(id).?.tasks_done);
    try testing.expectEqual(@as(u64, 600), r.store.record(id).?.tasks_total);
    try testing.expectEqual(@as(u32, 2), defaultWorkers(1));
    try testing.expectEqual(@as(u32, 2), defaultWorkers(8));
    try testing.expectEqual(@as(u32, 4), defaultWorkers(16));
    try testing.expectEqual(@as(u32, 8), defaultWorkers(64));
    try testing.expectEqual(@as(u32, 8), defaultWorkers(256));
}

test "duty cycle adheres within ten percent at 25/50/75 over a noop run" {
    const gpa = testing.allocator;
    const io = testing.io;
    for ([_]u8{ 25, 50, 75 }) |pct| {
        const r = try Rig.init(gpa, .{});
        defer r.deinit(gpa);
        const id = try r.fabric.submit(io, .{ .job_type = .noop, .arg0 = 12, .arg1 = 10, .throttle_pct = pct });
        const j = r.fabric.claimNext(io, false).?;
        const t0 = clock.monotonicNs();
        try testing.expectEqual(JobState.completed, r.fabric.runClaimed(io, j));
        const wall = clock.monotonicNs() - t0;
        const active = r.noop.active_ns.load(.monotonic);
        const duty = active * 100 / wall;
        _ = id;
        // The slice clock and the thread scheduler add latency on the
        // active side only, so adherence is judged from below at 25 % and
        // within ±10 pp otherwise.
        try testing.expect(duty + 10 >= pct);
        try testing.expect(duty <= pct + 10 or pct == 75);
    }
}
