//! Per-op trace ring — the DAEMON policy layer (e2e audit A2,
//! `docs/design-e2e-perf-audit.md` §1 honesty precondition 2 / Appendix D
//! item 2). The ring, the stage vocabulary and the current-op scope live
//! in the transport crate (`fuse3::op_trace`, storage; the `#[path]`-
//! shared `op_trace_core`, the pure laws); this module owns what only the
//! daemon can decide:
//!
//! - **the geometry derivation** ([`derive_geometry`]) — rings from the
//!   thread population, the pool from the R5 budget, the sampling divisor
//!   from the machine's op ceiling so ONE drain interval fits;
//! - **arming** — the `SQUEEZEFS_OP_TRACE` knob at mount
//!   ([`arm_from_env`]), the `op-trace` admin verb, the test seams;
//! - **the il op id law** ([`il_op_id`]) — the slot ticket under the IL
//!   namespace bit, disjoint from every kernel `unique`;
//! - **the `.trace` export** ([`trace_json`]) — DRAIN every ring into
//!   `{armed, divisor, dropped, samples_total, clock, stages, samples}`;
//! - the `op_trace_{armed,samples,dropped,divisor}` stats gauges.
//!
//! Hook sites in this crate reach the ring through the re-exports below
//! (`stamp_current` inside every `*_phase_record`, `scope` around
//! detached work, `TracedBatch` in the conveyor passes) — never through
//! `fuse3::` paths, so the policy layer stays the one seam.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub use fuse3::op_trace::{
    current_op, divisor, drain, dropped, is_armed, samples_total, scope, stamp, stamp_current,
    stamp_mono, stamp_now, traced, ArmConfig, Sample, Stage, TracedBatch,
};

/// The IL namespace bit: il ring op ids carry it, kernel `unique`s never
/// do (they are small counters — the kernel's `fuse_get_unique` steps by
/// `FUSE_REQ_ID_STEP`).
pub const OP_ID_IL_BIT: u64 = 1 << 63;

/// The il op id: the slot TICKET `(session, slot, generation)` — the
/// tuple the client's own `try_claim` returned it (the generation is the
/// slot core's ABA word), so a future client-side completion stamp joins
/// on the same key. Layout: bit 63 = namespace, bits 48..63 = session id
/// (low 15 bits), bits 32..48 = slot index (low 16), bits 0..32 =
/// generation (low 32). Every field is client-visible or daemon-minted;
/// a hostile generation only pollutes its own trace (VAL: display-only,
/// like the ingress stamp).
pub const fn il_op_id(session_id: u64, slot_index: u32, generation: u64) -> u64 {
    OP_ID_IL_BIT
        | ((session_id & 0x7FFF) << 48)
        | ((slot_index as u64 & 0xFFFF) << 32)
        | (generation & 0xFFFF_FFFF)
}

/// Op ceiling per core, ops/s — the sampling-divisor derivation's rate
/// term. Measured: 1.03–1.04 M IOPS flat on the 32-core field client
/// (`.benchmarks/2026-08-07-shim-iops.md`) ≈ 32 k/core, rounded up.
pub const OPS_PER_CORE_PER_S: u64 = 40_000;

/// Stamps one traced op can leave across its longest chain (transport 5
/// + read serve 10 + fill 7 = 22, rounded to the next power of two) —
/// the derivation's per-op multiplier.
pub const STAGES_PER_OP: u64 = 32;

/// The drain cadence the pool is sized for: the rigs' 1 Hz `.stats` /
/// `.trace` poll.
pub const DRAIN_INTERVAL_S: u64 = 1;

/// Bytes per ring slot (`op_trace_core::Sample`'s three words).
pub const SAMPLE_BYTES: u64 = 24;

/// A ring shallower than this records drops, not chains: 1024 slots =
/// 32 traced ops' longest chains — the physical minimum, never a tuning
/// floor.
pub const RING_MIN_CAPACITY: usize = 1024;

/// The derived arm geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub rings: usize,
    pub ring_capacity: usize,
    pub divisor: u32,
}

impl Geometry {
    /// Pool bytes (what the R5 component reports while armed).
    pub fn pool_bytes(&self) -> u64 {
        (self.rings as u64) * (self.ring_capacity as u64) * SAMPLE_BYTES
    }
}

/// The derivation (the standing law: every sizing is a function of
/// system resources — no free-floating constants):
///
/// - `rings = clamp(cpus × 8, 16, 4096)`: the stamping-thread population
///   is ≈ 4–6 × cpus (fuse3 queue workers + TPC lanes + svc/dd/nvme/meta
///   lanes); ×8 leaves claim headroom, and a thread past the pool drops
///   + counts rather than allocating. 16 = the smallest box's population
///   (2 cpus); 4096 = a 512-cpu host, the `SQUEEZEFS_FUSE_OVER_IO_URING_
///   QUEUES` clamp.
/// - `pool_bytes = budget >> 10`: 0.1 % of the R5 budget — a diagnostic
///   ring must never compete with the tiers it measures; floored at
///   `rings × RING_MIN_CAPACITY × SAMPLE_BYTES` (below that depth a ring
///   records drops, not chains).
/// - `ring_capacity = prev_pow2(pool_bytes / rings / SAMPLE_BYTES)` — rounded
///   DOWN so the pool never exceeds its slice (floored at the minimum
///   depth).
/// - `divisor = ceil(cpus × OPS_PER_CORE_PER_S × STAGES_PER_OP ×
///   DRAIN_INTERVAL_S / (rings × ring_capacity))`: the smallest N such
///   that one drain interval of the machine's op ceiling, at the longest
///   chain, fits the pool — the stitch tool then sees complete chains
///   for every sampled op instead of a truncated tail.
pub fn derive_geometry(budget_bytes: u64, cpus: usize) -> Geometry {
    let cpus = cpus.max(1);
    let rings = (cpus * 8).clamp(16, 4096);
    let floor = rings as u64 * RING_MIN_CAPACITY as u64 * SAMPLE_BYTES;
    let pool_bytes = (budget_bytes >> 10).max(floor);
    let per_ring = (pool_bytes / rings as u64 / SAMPLE_BYTES).max(RING_MIN_CAPACITY as u64);
    let ring_capacity = 1usize << per_ring.ilog2();
    let capacity = rings as u64 * ring_capacity as u64;
    let demand = cpus as u64 * OPS_PER_CORE_PER_S * STAGES_PER_OP * DRAIN_INTERVAL_S;
    let divisor = demand.div_ceil(capacity).clamp(1, u32::MAX as u64) as u32;
    Geometry {
        rings,
        ring_capacity,
        divisor,
    }
}

/// Pool bytes of the CURRENT arm (0 disarmed) — the R5 component's gauge.
static ARMED_POOL_BYTES: AtomicU64 = AtomicU64::new(0);

/// One `Instant` + CLOCK_MONOTONIC pair, read back-to-back: the
/// conversion the ring uses to stamp absolute CLOCK_MONOTONIC ns from
/// the `Instant`s the phase records already hold.
fn epoch_pair() -> (std::time::Instant, u64) {
    let instant = std::time::Instant::now();
    let mono = crate::mono_core::monotonic_ns_u64();
    (instant, mono)
}

fn register_budget_component() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Non-sheddable by construction (a ring is fixed at arm; disarm
        // is the operator's shed) — attribution only, weight 1, floor 0.
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "op_trace_rings",
            0,
            1,
            std::sync::Arc::new(|| ARMED_POOL_BYTES.load(Ordering::Relaxed)),
            std::sync::Arc::new(|_| {}),
        ));
    });
}

fn arm_geometry(g: Geometry) {
    register_budget_component();
    let (epoch_instant, epoch_mono_ns) = epoch_pair();
    fuse3::op_trace::arm(ArmConfig {
        rings: g.rings,
        ring_capacity: g.ring_capacity,
        divisor: g.divisor,
        epoch_instant,
        epoch_mono_ns,
    });
    ARMED_POOL_BYTES.store(g.pool_bytes(), Ordering::Relaxed);
    log::info!(
        "op-trace armed: {} rings × {} slots ({} MiB), sampling 1 in {} ops",
        g.rings,
        g.ring_capacity,
        g.pool_bytes() >> 20,
        g.divisor
    );
}

/// Arm with the DERIVED geometry (the knob's and the admin verb's arm).
pub fn arm_default() -> Geometry {
    let g = derive_geometry(
        crate::mem_budget::MEM_BUDGET.resolve_budget_now(),
        crate::cpu::possible_cpus(),
    );
    arm_geometry(g);
    g
}

/// Mount-time: arm iff `SQUEEZEFS_OP_TRACE` is on (ENG-10 Bool; the
/// startup gate already refused a malformed value).
pub fn arm_from_env() -> Option<Geometry> {
    crate::env_knobs::bool_knob("SQUEEZEFS_OP_TRACE", false).then(arm_default)
}

/// Test seam: arm with a fixed small geometry and the given divisor
/// (`1` = every op — "armed by a test").
pub fn arm_for_tests(divisor: u32) {
    arm_for_tests_with_geometry(divisor, 64, 1 << 12);
}

/// Test seam: arm with an explicit geometry (the overflow contract needs
/// a ring it can fill).
pub fn arm_for_tests_with_geometry(divisor: u32, rings: usize, ring_capacity: usize) {
    arm_geometry(Geometry {
        rings,
        ring_capacity,
        divisor,
    });
}

/// Disarm and zero the R5 gauge (the ring's memory stays allocated —
/// pools are reused on re-arm; undrained samples stay readable).
pub fn disarm() {
    fuse3::op_trace::disarm();
    ARMED_POOL_BYTES.store(0, Ordering::Relaxed);
}

/// The admin verb: `op-trace on|off|status`.
pub fn admin_verb(arg: &str) -> Result<String, String> {
    match arg.trim() {
        "on" => {
            let g = arm_default();
            Ok(status_json(Some(g)).to_string())
        }
        "off" => {
            disarm();
            Ok(status_json(None).to_string())
        }
        "status" | "" => Ok(status_json(None).to_string()),
        other => Err(format!("usage: op-trace on|off|status (got '{other}')")),
    }
}

fn status_json(g: Option<Geometry>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "armed": is_armed(),
        "divisor": fuse3::op_trace::divisor(),
        "samples_total": samples_total(),
        "dropped": dropped(),
        "pool_bytes": ARMED_POOL_BYTES.load(Ordering::Relaxed),
    });
    if let Some(g) = g {
        v["rings"] = serde_json::Value::from(g.rings);
        v["ring_capacity"] = serde_json::Value::from(g.ring_capacity);
    }
    v
}

/// The `.trace` payload: DRAIN every ring. Shape:
/// `{"armed": bool, "divisor": N, "dropped": N, "samples_total": N,
/// "clock": "CLOCK_MONOTONIC_ns", "stages": {"<id>": "<name>", …},
/// "samples": [[op_id, stage, mono_ns], …]}`, samples sorted by
/// `(op_id, mono_ns)`. `samples_total`/`dropped` are cumulative over the
/// ring's life (a row's delta is the row's count).
pub fn trace_json() -> serde_json::Value {
    let samples = drain();
    let mut stages = serde_json::Map::new();
    for s in Stage::ALL {
        stages.insert((*s as u16).to_string(), serde_json::Value::from(s.name()));
    }
    let rows: Vec<serde_json::Value> = samples
        .iter()
        .map(|s| {
            serde_json::Value::Array(vec![
                serde_json::Value::from(s.op_id),
                serde_json::Value::from(s.stage),
                serde_json::Value::from(s.mono_ns),
            ])
        })
        .collect();
    serde_json::json!({
        "armed": is_armed(),
        "divisor": fuse3::op_trace::divisor(),
        "dropped": dropped(),
        "samples_total": samples_total(),
        "clock": "CLOCK_MONOTONIC_ns",
        "stages": serde_json::Value::Object(stages),
        "samples": rows,
    })
}

/// The stats-inode gauges: `(armed, samples_total, dropped, divisor)`.
pub fn stats_gauges() -> (bool, u64, u64, u32) {
    (
        is_armed(),
        samples_total(),
        dropped(),
        fuse3::op_trace::divisor(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn il_op_id_fields_land_in_their_lanes() {
        let id = il_op_id(0x1234, 0xABCD, 0x1_2345_6789);
        assert_eq!(id >> 63, 1);
        assert_eq!((id >> 48) & 0x7FFF, 0x1234);
        assert_eq!((id >> 32) & 0xFFFF, 0xABCD);
        assert_eq!(id & 0xFFFF_FFFF, 0x2345_6789);
    }

    #[test]
    fn geometry_is_monotone_in_budget_and_fits_the_interval() {
        let a = derive_geometry(1 << 30, 4);
        let b = derive_geometry(1 << 36, 4);
        assert!(b.divisor <= a.divisor);
        for g in [a, b] {
            let demand = 4 * OPS_PER_CORE_PER_S * STAGES_PER_OP * DRAIN_INTERVAL_S;
            assert!(demand / u64::from(g.divisor) <= (g.rings * g.ring_capacity) as u64);
        }
    }
}
