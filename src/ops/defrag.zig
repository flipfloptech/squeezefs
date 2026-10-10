//! DEFRAG — the four axes (charter §8 "Defrag axes D1 free-space
//! contiguity / D2 file locality / D3 staged-extent pressure / D4 meta
//! node occupancy, each a gauge + an independently invocable mover
//! reusing existing machinery"; `docs/porting/ops_jobs_lifecycle.md` §3):
//! the fragmentation model (`metrics.zig`), the planners (`plan.zig`: D2
//! then D1; the `--pack` victims), the pack compactor (`pack_compact.zig`,
//! charter §6.1), the report and its bounded JSON (`report.zig`), and the
//! fabric bodies (`bodies.zig`: `defrag_data` over the ONE mover,
//! `defrag_pack` over the compactor, `defrag_fold` and `defrag_meta` as
//! kicks of the existing machinery through seams). Portable.

pub const metrics = @import("defrag/metrics.zig");
pub const plan = @import("defrag/plan.zig");
pub const pack_compact = @import("defrag/pack_compact.zig");
pub const report = @import("defrag/report.zig");
pub const bodies = @import("defrag/bodies.zig");

test {
    _ = metrics;
    _ = plan;
    _ = pack_compact;
    _ = report;
    _ = bodies;
}
