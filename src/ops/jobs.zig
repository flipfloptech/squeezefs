//! THE JOB FABRIC (charter §8 "Job fabric"; `docs/porting/ops_jobs_lifecycle.md`
//! §1): every maintenance op is a job TYPE on one fabric; the coordinator is
//! VOLUME 0's manager; workers execute data-plane shards; results are
//! PROPOSALS the coordinator verifies and commits; crash-resume is
//! idempotent plan regeneration. The portable core (records, throttle law,
//! preflight, rebalance planner, the state machine, the shard-lease table,
//! the wire block's frames + screens, the models) runs on every host; the
//! Linux half (`coordinator.zig`: the manager predicate, the plane, the
//! budget component, the lanes) compiles only there.

const builtin = @import("builtin");

pub const records = @import("jobs/records.zig");
pub const throttle = @import("jobs/throttle.zig");
pub const preflight = @import("jobs/preflight.zig");
pub const rebalance = @import("jobs/rebalance.zig");
pub const fabric = @import("jobs/fabric.zig");
pub const lease = @import("jobs/lease.zig");
pub const seams = @import("jobs/seams.zig");
pub const frames = @import("jobs/frames.zig");
pub const wire = @import("jobs/wire.zig");
pub const admin_frames = @import("jobs/admin_frames.zig");
pub const admin = @import("jobs/admin.zig");
pub const stats = @import("jobs/stats.zig");
pub const models = @import("jobs/models.zig");
pub const coordinator = if (builtin.os.tag == .linux) @import("jobs/coordinator.zig") else struct {};

pub const JobType = records.JobType;
pub const JobState = records.JobState;
pub const JobRecord = records.JobRecord;
pub const Fabric = fabric.Fabric;

test {
    _ = records;
    _ = throttle;
    _ = preflight;
    _ = rebalance;
    _ = fabric;
    _ = lease;
    _ = seams;
    _ = frames;
    _ = wire;
    _ = admin_frames;
    _ = admin;
    _ = stats;
    _ = models;
    if (builtin.os.tag == .linux) _ = coordinator;
}
