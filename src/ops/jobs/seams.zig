//! The fabric's PUBLISHED seams (spec §7: "items 3/4/10 expose
//! `FleetDispatch{readCapacity, dispatchReadShard, retireFleetShards,
//! shardLeaseTtl}` so fsck plugs in without touching the fabric"). The
//! fsck agent (`src/ops/fsck/`) consumes these; nothing here depends on
//! fsck. Every face is a plain vtable over `*anyopaque` so the consumer's
//! tests bind RAM worlds. Portable.
//!
//!   - `ShardLease`: the per-`(job, shard)` lease words a remote worker
//!     renews and a coordinator fences (`lease.zig` implements it; a fleet
//!     read shard takes the fencing/notify arm ONLY — no destinations, no
//!     PR preempt).
//!   - `Throttle`: the live duty words a long walk reads per step
//!     (`fabric.taskDone` behind it: the park, the checkpoint cadence, the
//!     control flags).
//!   - `AdminJob`: submit / status / wait for a job TYPE from an admin verb
//!     (`fsck --online` submits `JobType.fsck`; the report lands as a
//!     shard-class record).
//!   - `FleetDispatch`: the fleet READ-shard dispatcher (§1.9a).

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const records = @import("records.zig");
const fabric = @import("fabric.zig");

pub const JobType = records.JobType;
pub const JobState = records.JobState;
pub const JobRecord = records.JobRecord;
pub const DestTuple = records.DestTuple;
pub const StableIdentity = records.StableIdentity;

/// Charter §8: 30 s TTL, 10 s heartbeat.
pub const LEASE_TTL_MS: u32 = 30_000;
pub const HEARTBEAT_MS: u32 = 10_000;

/// One shard's identity on the wire and on disk.
pub const ShardId = struct {
    job_id: u64,
    shard: u32,

    pub fn eql(a: ShardId, b: ShardId) bool {
        return a.job_id == b.job_id and a.shard == b.shard;
    }
};

/// The lease verdicts a renewal or a proposal meets.
pub const LeaseVerdict = enum {
    /// The holder's fencing matches and the lease is live.
    live,
    /// Unknown `(job, shard)` (retired, or never assigned): refused stale.
    unknown,
    /// The fencing moved (an expiry re-leased it) or the shard is done.
    stale,
};

/// The per-shard lease face. A FLEET READ shard carries no destinations:
/// `assign` with an empty window is the read-shard shape.
pub const ShardLease = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const Error = error{ Full, OutOfMemory };
    pub const Assigned = struct { fencing: u64, lease_ttl_ms: u32 };

    pub const VTable = struct {
        /// Lease `id` to `holder` with `dests` (≤ 64; empty for a read
        /// shard). Reuses the shard's state with a bumped fencing or starts
        /// at 0. The ttl is the fabric's.
        assign: *const fn (ctx: *anyopaque, id: ShardId, holder: StableIdentity, dests: []const DestTuple, now_ns: u64) Error!Assigned,
        /// A heartbeat under `fencing`: `live` extends the deadline by a TTL.
        renew: *const fn (ctx: *anyopaque, id: ShardId, fencing: u64, now_ns: u64) LeaseVerdict,
        /// The proposal gate: `live` iff the fencing is current and the
        /// shard is not done (the caller verifies destinations AFTER this).
        check: *const fn (ctx: *anyopaque, id: ShardId, fencing: u64) LeaseVerdict,
        /// The proposal landed: the shard is done; a late duplicate is stale.
        complete: *const fn (ctx: *anyopaque, id: ShardId, fencing: u64) LeaseVerdict,
        /// Expire NOW (a departed session, a failed verification, a
        /// `shard_abandon`): the fencing bumps, the destinations enter the
        /// quarantine cohort, the shard re-queues. False if unknown/done.
        expire: *const fn (ctx: *anyopaque, id: ShardId, now_ns: u64) bool,
        /// Drop the per-shard state at pass end (without it every pass grew
        /// the map by `n − 1` forever); a zombie's late proposal after this
        /// is the unknown-shard refusal.
        retire: *const fn (ctx: *anyopaque, job_id: u64) void,
    };

    pub fn assign(self: ShardLease, id: ShardId, holder: StableIdentity, dests: []const DestTuple, now_ns: u64) Error!Assigned {
        return self.vtable.assign(self.ctx, id, holder, dests, now_ns);
    }
    pub fn renew(self: ShardLease, id: ShardId, fencing: u64, now_ns: u64) LeaseVerdict {
        return self.vtable.renew(self.ctx, id, fencing, now_ns);
    }
    pub fn check(self: ShardLease, id: ShardId, fencing: u64) LeaseVerdict {
        return self.vtable.check(self.ctx, id, fencing);
    }
    pub fn complete(self: ShardLease, id: ShardId, fencing: u64) LeaseVerdict {
        return self.vtable.complete(self.ctx, id, fencing);
    }
    pub fn expire(self: ShardLease, id: ShardId, now_ns: u64) bool {
        return self.vtable.expire(self.ctx, id, now_ns);
    }
    pub fn retire(self: ShardLease, job_id: u64) void {
        return self.vtable.retire(self.ctx, job_id);
    }
};

/// The live duty face a long walk reads per step: `step(elapsed)` prices
/// the throttle park, runs the checkpoint cadence and returns whether the
/// walk must stop (a pause/cancel landed).
pub const Throttle = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        step: *const fn (ctx: *anyopaque, io: Io, elapsed_ns: u64) fabric.Step,
        pct: *const fn (ctx: *anyopaque) u8,
    };

    pub fn step(self: Throttle, io: Io, elapsed_ns: u64) fabric.Step {
        return self.vtable.step(self.ctx, io, elapsed_ns);
    }
    pub fn pct(self: Throttle) u8 {
        return self.vtable.pct(self.ctx);
    }

    /// The fabric's own binding for a claimed job: the pair lives in the
    /// job's control block so no allocation rides a step.
    pub fn forJob(f: *fabric.Fabric, j: *fabric.JobCtl) Throttle {
        j.throttle_binding = .{ .f = f, .j = j };
        return .{ .ctx = &j.throttle_binding, .vtable = &job_vt };
    }

    const job_vt: VTable = .{ .step = &jobStep, .pct = &jobPct };

    fn jobStep(ctx: *anyopaque, io: Io, elapsed_ns: u64) fabric.Step {
        const b: *fabric.ThrottleBinding = @ptrCast(@alignCast(ctx));
        return b.f.taskDone(io, b.j, elapsed_ns);
    }
    fn jobPct(ctx: *anyopaque) u8 {
        const b: *fabric.ThrottleBinding = @ptrCast(@alignCast(ctx));
        return b.j.ctl.loadPct();
    }
};

/// Submit / status / wait for an admin verb's job.
pub const AdminJob = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        submit: *const fn (ctx: *anyopaque, io: Io, s: fabric.Submission) fabric.SubmitError!u64,
        status: *const fn (ctx: *anyopaque, io: Io, job_id: u64) ?JobRecord,
        wait: *const fn (ctx: *anyopaque, io: Io, job_id: u64, deadline_ns: u64) ?JobState,
    };

    pub fn submit(self: AdminJob, io: Io, s: fabric.Submission) fabric.SubmitError!u64 {
        return self.vtable.submit(self.ctx, io, s);
    }
    pub fn status(self: AdminJob, io: Io, job_id: u64) ?JobRecord {
        return self.vtable.status(self.ctx, io, job_id);
    }
    pub fn wait(self: AdminJob, io: Io, job_id: u64, deadline_ns: u64) ?JobState {
        return self.vtable.wait(self.ctx, io, job_id, deadline_ns);
    }

    /// The fabric's own binding.
    pub fn forFabric(f: *fabric.Fabric) AdminJob {
        return .{ .ctx = f, .vtable = &fabric_vt };
    }

    const fabric_vt: VTable = .{ .submit = &fabricSubmit, .status = &fabricStatus, .wait = &fabricWait };

    fn fabricSubmit(ctx: *anyopaque, io: Io, s: fabric.Submission) fabric.SubmitError!u64 {
        const f: *fabric.Fabric = @ptrCast(@alignCast(ctx));
        return f.submit(io, s);
    }
    fn fabricStatus(ctx: *anyopaque, io: Io, job_id: u64) ?JobRecord {
        const f: *fabric.Fabric = @ptrCast(@alignCast(ctx));
        return f.snapshot(io, job_id);
    }
    fn fabricWait(ctx: *anyopaque, io: Io, job_id: u64, deadline_ns: u64) ?JobState {
        const f: *fabric.Fabric = @ptrCast(@alignCast(ctx));
        return f.waitTerminal(io, job_id, deadline_ns);
    }
};

/// Inode-plane shards number from here (census residues are `1..n`): one
/// number space, so the shard map, the lease law, the fencing identity and
/// the durable records are the same ones.
pub const INODE_PLANE_SHARD_BASE: u32 = 1 << 20;

/// A dispatched read shard's outcome.
pub const ReadShardOutcome = union(enum) {
    /// The payload the worker proposed (owned by the dispatcher's gpa until
    /// `release`).
    payload: []const u8,
    /// Lost (TTL, abandon, malformed): the caller runs it locally
    /// (`job_fleet_shards_relocal`).
    lost,
};

/// The fleet READ-shard dispatcher (KD-MW-16): `n = 1 + readCapacity()`
/// residue shards; shard 0 stays on the coordinator; shards `1..n` ship.
pub const FleetDispatch = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const Descriptor = struct {
        id: ShardId,
        /// `(k, n)` — the residue this shard walks.
        fleet_k: u32,
        fleet_n: u32,
        /// KD-PV-16: the owner's volumes only.
        inode_plane: u64,
        throttle_pct: u8,
    };

    pub const VTable = struct {
        /// Enrolled read-capable sessions NOW (0 ⇒ `run()` verbatim).
        readCapacity: *const fn (ctx: *anyopaque) u32,
        /// Ship one read shard; blocks until the proposal lands or the lease
        /// law gives it up. The progress deadline derives from the TTL.
        dispatchReadShard: *const fn (ctx: *anyopaque, io: Io, gpa: Allocator, d: Descriptor) ReadShardOutcome,
        release: *const fn (ctx: *anyopaque, gpa: Allocator, payload: []const u8) void,
        retireFleetShards: *const fn (ctx: *anyopaque, job_id: u64) void,
        shardLeaseTtlMs: *const fn (ctx: *anyopaque) u32,
    };

    pub fn readCapacity(self: FleetDispatch) u32 {
        return self.vtable.readCapacity(self.ctx);
    }
    pub fn dispatchReadShard(self: FleetDispatch, io: Io, gpa: Allocator, d: Descriptor) ReadShardOutcome {
        return self.vtable.dispatchReadShard(self.ctx, io, gpa, d);
    }
    pub fn release(self: FleetDispatch, gpa: Allocator, payload: []const u8) void {
        return self.vtable.release(self.ctx, gpa, payload);
    }
    pub fn retireFleetShards(self: FleetDispatch, job_id: u64) void {
        return self.vtable.retireFleetShards(self.ctx, job_id);
    }
    pub fn shardLeaseTtlMs(self: FleetDispatch) u32 {
        return self.vtable.shardLeaseTtlMs(self.ctx);
    }

    /// The zero-capacity dispatcher: `run()` verbatim (a single-writer
    /// mount's fsck is literally the shipped path).
    pub const none: FleetDispatch = .{ .ctx = @ptrCast(@constCast(&none_ctx)), .vtable = &none_vt };
    const none_ctx: u8 = 0;
    const none_vt: VTable = .{ .readCapacity = &noneCapacity, .dispatchReadShard = &noneDispatch, .release = &noneRelease, .retireFleetShards = &noneRetire, .shardLeaseTtlMs = &noneTtl };
    fn noneCapacity(_: *anyopaque) u32 {
        return 0;
    }
    fn noneDispatch(_: *anyopaque, _: Io, _: Allocator, _: Descriptor) ReadShardOutcome {
        return .lost;
    }
    fn noneRelease(_: *anyopaque, _: Allocator, _: []const u8) void {}
    fn noneRetire(_: *anyopaque, _: u64) void {}
    fn noneTtl(_: *anyopaque) u32 {
        return LEASE_TTL_MS;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

var seed: std.atomic.Value(u64) = .init(0x5eed);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

test "the fabric's own AdminJob and Throttle bindings: submit/status/wait route to the fabric; a step prices the park and reads the live pct; the none dispatcher has zero capacity and the charter's TTL" {
    const gpa = testing.allocator;
    const io = testing.io;
    var store = fabric.RamStore.init(gpa);
    defer store.deinit();
    var f = fabric.Fabric.init(gpa, store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
    defer f.deinit();
    var noop: fabric.NoopBody = .{};
    f.registerBody(.noop, noop.body());
    const admin = AdminJob.forFabric(&f);
    const id = try admin.submit(io, .{ .job_type = .noop, .arg0 = 2, .throttle_pct = 100 });
    try testing.expectEqual(JobState.queued, admin.status(io, id).?.state);
    const j = f.claimNext(io, false).?;
    const th = Throttle.forJob(&f, j);
    try testing.expectEqual(@as(u8, 100), th.pct());
    try testing.expectEqual(fabric.Step.@"continue", th.step(io, 1000));
    try testing.expectEqual(@as(u64, 1), admin.status(io, id).?.tasks_done);
    try f.pause(io, id);
    try testing.expectEqual(fabric.Step.stop, th.step(io, 1000));
    try f.resumeJob(io, id);
    try testing.expectEqual(JobState.completed, f.runClaimed(io, j));
    try testing.expectEqual(@as(?JobState, .completed), admin.wait(io, id, std.math.maxInt(u64) / 2));
    try testing.expectEqual(@as(u32, 0), FleetDispatch.none.readCapacity());
    try testing.expectEqual(LEASE_TTL_MS, FleetDispatch.none.shardLeaseTtlMs());
    try testing.expectEqual(@as(u32, 30_000), LEASE_TTL_MS);
    try testing.expectEqual(@as(u32, 10_000), HEARTBEAT_MS);
    try testing.expectEqual(@as(u32, 1 << 20), INODE_PLANE_SHARD_BASE);
}
