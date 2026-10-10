//! THE VOLUME LIFECYCLE (charter §8 "Volume lifecycle"; `docs/porting/
//! ops_jobs_lifecycle.md` §2): `vol-{16 hex}` ids never reused; the lattice
//! `Active → Draining → Retired` with the seven-step durability order
//! (`lattice.zig`); the census planner (`census.zig`); THE ONE MOVER
//! primitive in nine steps (`mover.zig`); the drain, rebalance and retire
//! passes the fabric runs (`drain.zig`); the instruments (`stats.zig`).
//! The Linux half (`volume_ops.zig`: the offline verbs over the writer
//! door, the data superblock, volume 0's conveyor; the plane binding of
//! the mover seam) compiles only there.

const builtin = @import("builtin");

pub const lattice = @import("lifecycle/lattice.zig");
pub const census = @import("lifecycle/census.zig");
pub const mover = @import("lifecycle/mover.zig");
pub const drain = @import("lifecycle/drain.zig");
pub const stats = @import("lifecycle/stats.zig");
pub const volume_ops = if (builtin.os.tag == .linux) @import("lifecycle/volume_ops.zig") else struct {};

test {
    _ = lattice;
    _ = census;
    _ = mover;
    _ = drain;
    _ = stats;
    if (builtin.os.tag == .linux) _ = volume_ops;
}
