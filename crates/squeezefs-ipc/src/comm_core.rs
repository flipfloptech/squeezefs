//! Per-mount thread-comm disambiguation (design-full-multi-writer §5.3,
//! PR 3 `fix/mw-colocated-collisions`).
//!
//! N co-located daemons all name their hot threads identically
//! (`sqz-ipc-svc0`, `fuse3-tpc3`, `sqz-meta0`, `sqz-timer`, …), so
//! pidstat/perf attribution on a fleet box cannot tell one mount's
//! service population from another's — the `fuse3-tpcN` lesson one
//! level up. Every per-mount thread name routes through [`comm_name`],
//! which appends the derived suffix `m{:x}` — the mount slot's LOW HEX
//! nibble (KD-MW-2's `mount_slot`, so it is mount-point-stable, cheap,
//! and never a knob; collisions between two co-located mounts are
//! 1-in-16 and COSMETIC — the suffix is an attribution aid, never an
//! identity: OQ-5's full-slot collision refusal owns identity).
//!
//! Budget law: the kernel comm is `TASK_COMM_LEN` (16) minus the NUL —
//! 15 visible chars, silently truncated past that. The suffix survives
//! VERBATIM at the end and the BASE truncates to fit (a truncated base
//! still greps by prefix; a truncated suffix is a silent attribution
//! lie). `tests/mount_comm_tests.rs` sweeps the tree's actual base
//! literals through the budget.
//!
//! Sharing shape (the `numa_core`/`thp.rs` production-sharing
//! precedent): canonical file in the squeezefs-ipc tree — a REAL module
//! there (shared static with the root crate and the preload shim) and
//! `#[path]`-included by the fuse3 fork (which cannot depend on this
//! crate: it is its own excluded workspace root). A `#[path]` include
//! gets its OWN copy of the static, so the daemon seeds EVERY crate
//! copy from `writer_scope::set_mount_identity` (the KD-MW-2 identity
//! point) — an unseeded copy keeps today's bare names, which is also
//! the permanent posture of offline verbs, tests, and the shim.

use std::sync::atomic::{AtomicU32, Ordering};

/// The kernel's visible comm budget: `TASK_COMM_LEN` (16) − NUL.
pub const COMM_MAX: usize = 15;

/// This process's mount slot for comm purposes; `0` = no mount (offline
/// verbs, tests, the shim) ⇒ bare names. Relaxed: a plain tag word,
/// written once at mount time before the named populations spawn.
static COMM_TAG: AtomicU32 = AtomicU32::new(0);

/// Seed this crate copy's tag (called from the mount path via
/// `writer_scope::set_mount_identity`, once per process, before the
/// named thread populations spawn — later spawns pick it up, threads
/// already alive keep their bare name: the suffix is cosmetic).
pub fn set_comm_tag(mount_slot: u32) {
    COMM_TAG.store(mount_slot, Ordering::Relaxed);
}

/// Pure form (unit-pinned): `base` + `m{:x}` of the slot's low nibble,
/// base-truncated into [`COMM_MAX`]; `slot == 0` ⇒ `base` verbatim.
pub fn comm_name_with(base: &str, mount_slot: u32) -> String {
    if mount_slot == 0 {
        return base.to_string();
    }
    let suffix_nibble = mount_slot & 0xf;
    // suffix = "m" + one hex char: 2 bytes, always ASCII.
    let keep = COMM_MAX - 2;
    let base = if base.len() > keep { &base[..keep] } else { base };
    format!("{base}m{suffix_nibble:x}")
}

/// The process form every spawn site uses.
pub fn comm_name(base: &str) -> String {
    comm_name_with(base, COMM_TAG.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget + suffix-survival law on adversarial widths.
    #[test]
    fn budget_and_suffix_survival() {
        for base in ["x", "sqz-ipc-svc63", "fuse-over-uring-0-511"] {
            for slot in [1u32, 0xf, 0xab, u32::MAX] {
                let name = comm_name_with(base, slot);
                assert!(name.len() <= COMM_MAX, "{base} slot {slot:#x}: {name}");
                let want = format!("m{:x}", slot & 0xf);
                assert!(
                    name.ends_with(&want),
                    "{base} slot {slot:#x}: {name} must end with {want}"
                );
            }
        }
    }

    /// Slot 0 is byte-identical bare (offline verbs / tests / shim).
    #[test]
    fn slot_zero_is_bare() {
        assert_eq!(comm_name_with("sqz-ipc-svc0", 0), "sqz-ipc-svc0");
    }
}
