//! Ingest-economy sizing (2026-07-28): the ONE derivation both halves of
//! the L4 data plane size themselves from.
//!
//! The 2026-07-28 field capture (4-node 2×200GbE nvme-tcp cluster,
//! 32-CPU client) convicted a **defaults mismatch** as the large-write
//! ingest wall: the shim's per-mount session count defaulted to a flat 4
//! (`SQUEEZEFS_IL_SESSIONS`, sized under L4 IOPS economics) while the
//! daemon's service threads defaulted to `clamp(cpus/4, 2, 8)` — two
//! independent constants. pidstat showed `sqz-ipc-svc0..3` at 94–99 %
//! CPU and `svc4..7` permanently idle: half the drain capacity had no
//! ring to drain, walling sequential ingest at ~7.5 GB/s against a
//! 16.6 GB/s raw-fio ceiling. Setting sessions=8 by hand recovered
//! +44 % (10.8 GB/s) on the spot.
//!
//! The fix is structural, not another constant: **one function** sizes
//! both sides, so they cannot drift —
//!
//! - the **shim** defaults its per-mount fd-shard session count to
//!   [`il_sessions_default`] (`SQUEEZEFS_IL_SESSIONS` remains an
//!   override lever only);
//! - the **daemon** defaults its service-thread *ceiling* to the same
//!   value (`SQUEEZEFS_IPC_SERVICE_THREADS` remains an override lever
//!   only) and spawns threads **on session admission** (spawn-on-bind)
//!   — a thread exists only when a session is pinned to it, so the
//!   field's parked-spare-thread shape is unrepresentable.
//!
//! Paired contract tests on both sides (`sessions_default_ties_to_*` in
//! `squeezefs-preload`, `service_ceiling_default_ties_to_*` in the root
//! suite) turn any future divergence into a red test.
//!
//! The rails: floor 2 (a single-session process serializes on one
//! dequeue thread — the pre-L4-8 plateau), ceiling
//! [`SESSION_REGISTRY_SLOTS`]`/2` (derivation sweep 2026-08-04: the
//! former literal 16 is now DERIVED from the structural binder — the
//! shim's fixed-size session registry, leaving headroom for a second
//! concurrently-bound mount; the old R5-arena-math leg of the 16
//! rationale dissolved when the fixed 2 GiB session-shm ceiling was
//! deleted, 2026-08-02). Admission refusals past the R5 cap remain
//! honest — refused shards passthrough, counted. `cpus/4` is the
//! measured drain-thread saturation slope from the 2026-07-19 service
//! sweep (unchanged), now applied to BOTH sides.

/// The shim's session-registry capacity across ALL mounts — a
/// STRUCTURAL constant, not tuning: the LD_PRELOAD shim keeps its
/// session table as a fixed static array (constructor-safe, alloc-free
/// — the interposer environment's law), so this is the physical binder
/// every per-mount session ceiling derives from. Growing it is a shim
/// table change, not a knob (`squeezefs-preload` ties its `MAX_SESSIONS`
/// to this constant — drift is a red test on both sides).
pub const SESSION_REGISTRY_SLOTS: usize = 32;

/// `clamp(cpus/4, 2, SESSION_REGISTRY_SLOTS/2)` — see the module docs
/// for the derivation (the ceiling = half the registry: two
/// concurrently-bound mounts' worth of headroom).
pub fn il_sessions_default(cpus: usize) -> usize {
    (cpus / 4).clamp(2, SESSION_REGISTRY_SLOTS / 2)
}

/// The daemon's il drain-LANE width — `clamp(3×cpus/8, 2, 64)` — sizing
/// BOTH halves of one lane: the service-thread ceiling and the
/// direct-drive shard width (one lane = one `sqz-ipc-svcN` submitter +
/// one `sqz-ipc-ddN` reaper = **two OS threads**). The two halves are
/// ONE number by the lane routing (`lane = owner_idx % width`; owners
/// are bounded by the ceiling), so a shard set wider than the ceiling is
/// production-dark and a ceiling wider than the shard set shares reapers
/// — the ingest-economy paired-derivation law applies to the pair.
///
/// The slope is the counted 2026-08-06 field width sweep
/// (`.benchmarks/2026-08-06-dd-width-slope.md` — squeeze-test, 32 CPUs /
/// 2 nodes, il rand-4k 32×qd32, 3×30 s rows per width, W8 brackets at
/// both ends, engagement + shards/svc gauges exact per width): W8 (the
/// old cpus/4) 622–636k → **W12 695–700k (+10.4 %, clat down — best)** →
/// W16 675–690k → W24 616–626k (regression). In lane-thread/core terms
/// the grid is 2W/cpus ∈ {0.5, 0.75, 1.0, 1.5}: the optimum sits where
/// the lane-pair population takes ¾ of the core budget — leaving the
/// complement for the co-located client fleet — and the one sampled
/// point past 1.0× is the one regression (oversubscription), verifying
/// the budget story's failure mode inside the same sweep. `3×cpus/8` is
/// `2W = ¾·cpus` solved for W. Honest scope: one box shape; the slope
/// (vs any other form through 12-at-32) is the budget story's claim, and
/// the second-box-shape confirmation row stays filed in the evidence
/// note.
///
/// Rails: floor 2 = the pre-L4-8 single-consumer plateau, and the
/// never-regress posture holds pointwise (⌊3c/8⌋ ≥ ⌊c/4⌋, so no box
/// derives below the previously shipped width); ceiling 64 = the
/// explicit-lever clamp parity (`SQUEEZEFS_IPC_SERVICE_THREADS` /
/// `SQUEEZEFS_IPC_DD_SHARDS` admit 1..=64, and a default must be
/// expressible as an explicit setting — engages only at cpus ≥ 174,
/// harmless where absent). DOMINANCE over [`il_sessions_default`] at
/// every machine size is what subsumes (never retunes) the ingest
/// surfaces: every default shim session still owns its own drain thread,
/// and spawn-on-bind keeps idle spares unrepresentable — the
/// DEFAULTS-MISMATCH topology cannot recur in either direction. The
/// shim's per-mount session count and the fuse3 drain-group width keep
/// their own measured cpus/4 slopes: this class is the daemon drain lane
/// only.
pub fn il_drain_lanes_default(cpus: usize) -> usize {
    (cpus.saturating_mul(3) / 8).clamp(2, 64)
}

/// The hybrid lane gate's **lane-RTT delta class** (ruling D14's
/// corollary, 2026-08-07): the per-op overhead the kernel FUSE lane pays
/// over the IPC ring — a MEASURED class constant, like the W2 fold
/// amortization constant, never tuning. Basis: the D14 field bracket's
/// rand-4k rows (squeeze-test, the shape where per-byte cost vanishes and
/// the per-op delta is the whole difference): ring lane 594 k IOPS vs
/// kernel lane ≈ 253 k at the same shape ⇒ per-op service-cost gap
/// 1/253 k − 1/594 k ≈ 2.27 µs. Retuning this is a deliberate act with a
/// new counted citation (the derivation-sweep tie test pins it).
pub const KERNEL_LANE_RTT_DELTA_NS: u64 = 2_300;

/// The ring pays this many CPU passes over a READ payload's bytes (the
/// daemon's serve-into-arena copy + the client's arena→app consume copy —
/// the read-copy-count ledger's E-IL closure). Writes pay one client
/// copy; their crossover is 2× and collapses to the same clamp floor at
/// every measured memBW on target hardware, so ONE threshold (derived on
/// the read side, the conservative half and the direction with the larger
/// field gap) governs both directions.
const KERNEL_LANE_RING_COPIES: u64 = 2;

/// The hybrid lane gate's derived crossover threshold (bytes): ops
/// STRICTLY larger ride the kernel FUSE lane, at-or-below rides the IPC
/// ring. `SQUEEZEFS_IL_KERNEL_LANE_MIN` overrides verbatim (0 = gate off);
/// this is the ABSENT default.
///
/// The model: `ring_time(size) ≈ ring_rtt + copies × size / memBW`,
/// `kernel_time(size) ≈ fuse_rtt` (+ ≈ 0 per-byte userspace cost — the
/// kernel lane's zc serve moves payload by DMA/kernel copy at line rate).
/// Setting them equal: `size* = memBW × (fuse_rtt − ring_rtt) / copies`
/// with the delta being [`KERNEL_LANE_RTT_DELTA_NS`] and `memBW` the
/// shim's startup micro-probe (runtime-derived — portable-by-default,
/// never a CPU-ID table; a failed probe passes 0 and lands on the floor).
///
/// Rails (each physical, per the derivation law):
/// * rounded DOWN to the 4 KiB LBA/page grain — the same grain
///   [`crate::layout::Geometry::slot_slab`] floors to, so the boundary
///   composes with O_DIRECT alignment;
/// * floor = `slab_bytes` — at or below the slab a ring op is the
///   single-flight allocation-free serial path (one claim, one wait: the
///   IOPS lane's own regime, which the gate exists to protect, never to
///   tax);
/// * ceiling = `max_op_bytes` — past the per-op payload ceiling an op
///   must chunk into multiple ring flights (RTT multiplication), so the
///   gate must have engaged at latest there.
pub fn kernel_lane_min_default(
    mem_bw_bytes_per_sec: u64,
    slab_bytes: u64,
    max_op_bytes: u64,
) -> u64 {
    const GRAIN: u64 = 4096;
    let raw = (u128::from(mem_bw_bytes_per_sec) * u128::from(KERNEL_LANE_RTT_DELTA_NS)
        / (1_000_000_000u128 * u128::from(KERNEL_LANE_RING_COPIES))) as u64;
    let grained = raw / GRAIN * GRAIN;
    // lo ≤ hi even for degenerate/toy geometries (slot_slab already
    // guarantees slab ≤ max_op for real sessions).
    let lo = slab_bytes.min(max_op_bytes);
    grained.clamp(lo, max_op_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The derivation itself: machine-scaled, railed 2..=16.
    #[test]
    fn derivation_values() {
        assert_eq!(il_sessions_default(1), 2, "floor: never below 2");
        assert_eq!(il_sessions_default(4), 2);
        assert_eq!(il_sessions_default(8), 2);
        assert_eq!(il_sessions_default(16), 4);
        assert_eq!(
            il_sessions_default(22),
            5,
            "non-multiple-of-4 boxes truncate"
        );
        assert_eq!(
            il_sessions_default(32),
            8,
            "the field box: 8 sessions == the +44 % experiment"
        );
        assert_eq!(il_sessions_default(64), 16);
        assert_eq!(
            il_sessions_default(256),
            16,
            "ceiling: the structural registry bound (32 slots ÷ 2 mounts)"
        );
    }

    /// Derivation sweep 2026-08-04: the ceiling is DERIVED from the
    /// structural registry capacity (half of it — two concurrently-
    /// bound mounts' headroom), never a free-floating literal. Growing
    /// the registry grows the ceiling with it; drift is red here.
    #[test]
    fn ceiling_is_registry_derived() {
        assert_eq!(
            il_sessions_default(usize::MAX),
            SESSION_REGISTRY_SLOTS / 2,
            "ceiling must equal half the shim session registry"
        );
    }

    /// The drain-lane width (direct-drive width re-grade, 2026-08-06):
    /// machine-scaled 3×cpus/8, railed 2..=64, overflow-safe. The root
    /// suite carries the tie tests (lane-pair equality + dominance);
    /// this pins the pure form's canonical values.
    #[test]
    fn drain_lane_derivation_values() {
        assert_eq!(il_drain_lanes_default(1), 2, "floor: never below 2");
        assert_eq!(il_drain_lanes_default(4), 2, "3×4/8 = 1 ⇒ floor 2");
        assert_eq!(il_drain_lanes_default(8), 3);
        assert_eq!(il_drain_lanes_default(16), 6);
        assert_eq!(
            il_drain_lanes_default(32),
            12,
            "the field box: the counted sweep's interior optimum"
        );
        assert_eq!(il_drain_lanes_default(64), 24);
        assert_eq!(il_drain_lanes_default(96), 36);
        assert_eq!(
            il_drain_lanes_default(256),
            64,
            "rail: the explicit-lever clamp parity"
        );
        assert_eq!(
            il_drain_lanes_default(usize::MAX),
            64,
            "saturating — never overflows"
        );
    }

    /// Dominance / never-regress (the subsumption proof's arithmetic
    /// half): the lane width covers the shim session default at every
    /// machine size, so every default session keeps its own drain
    /// thread and no box derives below the previously shipped cpus/4.
    #[test]
    fn drain_lanes_dominate_sessions_pointwise() {
        for cpus in 1..=4096usize {
            assert!(
                il_drain_lanes_default(cpus) >= il_sessions_default(cpus),
                "dominance broken at cpus={cpus}"
            );
        }
    }

    /// The hybrid lane gate's crossover (2026-08-07): pure-form values
    /// on the canonical shapes; the root suite carries the tie test.
    #[test]
    fn kernel_lane_min_derivation_values() {
        const SLAB: u64 = 64 * 1024;
        const MAX_OP: u64 = 1024 * 1024;
        // Field-class memcpy (~15 GB/s): raw ≈ 17 KiB ⇒ slab floor.
        assert_eq!(kernel_lane_min_default(15_000_000_000, SLAB, MAX_OP), SLAB);
        // Failed probe (0) ⇒ the floor, conservatively.
        assert_eq!(kernel_lane_min_default(0, SLAB, MAX_OP), SLAB);
        // 100 GB/s-class engine: interior, 4 KiB-grained (115_000 → 28 ×
        // 4096 = 112 KiB).
        assert_eq!(
            kernel_lane_min_default(100_000_000_000, SLAB, MAX_OP),
            112 * 1024
        );
        // Overflow-safe rail at the payload ceiling.
        assert_eq!(kernel_lane_min_default(u64::MAX, SLAB, MAX_OP), MAX_OP);
        // Toy geometries never invert the rails.
        assert_eq!(kernel_lane_min_default(0, 4096, 4096), 4096);
        assert_eq!(kernel_lane_min_default(0, 8192, 4096), 4096);
    }
}
