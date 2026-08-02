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
}
