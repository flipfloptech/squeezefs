//! Process-level CPU parallelism, immune to calling-thread pinning —
//! and, since PR 3b of the full-multi-writer program, the fleet-share
//! DIVIDED sizing root (KD-MW-14, `docs/design-full-multi-writer.md`
//! §5.6).
//!
//! `main.rs` pins every tokio runtime worker to one core
//! (`on_thread_start` → `core_affinity::set_for_current`), and both
//! `std::thread::available_parallelism()` and
//! `core_affinity::get_core_ids()` consult the **calling thread's**
//! affinity mask. Any lazily-sized pool first touched from a pinned worker
//! therefore saw ONE CPU and collapsed — the fuse3 TPC handler pool became
//! a single LocalSet thread and `striped_block_concurrency` fell to its
//! floor, the enabling half of the Hang-1 CFR wedge. Every sizing decision
//! must instead use the PROCESS mask: the main thread (tid == pid) is never
//! pinned, so `sched_getaffinity(getpid())` returns the mount's original
//! taskset mask no matter which thread asks.
//!
//! Two CPU roots live here, deliberately distinct (§5.6's exemption law):
//!
//! * [`process_parallelism`] — the **derived-sizing root**: the process
//!   affinity mask divided by [`fleet_share`] (`ceil`, never 0). Every
//!   CPU-derived cap (drain lanes, dd shards, conveyor batches, NVMe
//!   fan-out, zcrx geometry, …) reads THIS number, so N co-located
//!   daemons scale their derivations through one divisor at the root.
//! * [`possible_cpus`] — **kernel-mandated geometry, EXEMPT**: the
//!   FUSE-over-io_uring queue population (one queue per possible CPU or
//!   the session never becomes ready) and anything shadowing that
//!   delivered geometry. Never divided — pretending to divide it would be
//!   a lie the exemption-list tie test forbids
//!   (`tests/derivation_sweep_tests.rs`).

use std::sync::OnceLock;

/// KD-MW-14 (design-full-multi-writer §5.6): the fleet-share divisor —
/// how many co-located SqueezeFS daemons divide this machine's derived
/// sizing tree. Default 1 = today's whole-machine posture (solo-dark,
/// byte-identical). Never auto-detected: the operator (or the
/// `mw_fleet.sh` rig) sets `SQUEEZEFS_FLEET_SHARE`; the ENG-10 registry
/// refuses 0/out-of-range at startup, and the `.max(1)` here is the
/// library-embedding guard, never a clamp of an admitted value.
///
/// Cached: the share is a process-lifetime root input — every derivation
/// must read the SAME divisor, pinned or not, early or late.
pub fn fleet_share() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| crate::env_knobs::int_knob::<usize>("SQUEEZEFS_FLEET_SHARE", 1).max(1))
}

/// The CPU-root division, pure (the tie-test form): `ceil(raw / share)`,
/// never 0. Rounds UP per the rounding doctrine (user ruling 2026-08-14:
/// slightly more than not enough — a fractional share derives the next
/// whole unit; mild oversubscription of divisible resources is
/// acceptable, silent starvation is not).
pub fn effective_parallelism_from(raw: usize, share: usize) -> usize {
    raw.max(1).div_ceil(share.max(1)).max(1)
}

/// Number of CPUs this daemon SIZES for: the process's (main thread's)
/// affinity mask divided by the fleet share (§5.6 — `ceil`, floor 1).
/// This is the **derived-sizing CPU root**; kernel-mandated geometry
/// reads [`possible_cpus`] instead.
///
/// Cached: the value is calling-thread independent by construction, so the
/// first caller — pinned or not — computes the same result. Falls back to
/// `available_parallelism()` if the syscall fails; never returns 0.
pub fn process_parallelism() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| effective_parallelism_from(raw_process_parallelism(), fleet_share()))
}

/// Kernel **possible** CPU count (`_SC_NPROCESSORS_CONF`) — the same
/// population the FUSE-over-io_uring transport sizes its queue set from
/// (kernel `fuse_uring_create()` uses `num_possible_cpus()`; fewer queues
/// than possible CPUs never becomes ready). Distinct from
/// [`process_parallelism`] on purpose: taskset/offline masks shrink the
/// affinity mask but never the delivered ring geometry, and anything
/// shadowing that geometry (the D1.b op registry) must size from THIS
/// number. **Fleet-share EXEMPT** (KD-MW-14's kernel-mandated-geometry
/// exemption class): N co-located daemons always hold N × possible-CPUs
/// queues regardless of share — the memory behind them scales through the
/// divided budget root via the depth-degradation law instead. The
/// fallback is the RAW (undivided) affinity mask for the same reason.
/// Never returns 0.
pub fn possible_cpus() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        if n > 0 {
            n as usize
        } else {
            raw_process_parallelism()
        }
    })
}

/// The RAW process affinity mask width — the undivided machine fact both
/// roots derive from ([`process_parallelism`] divides it; the
/// [`possible_cpus`] fallback must NOT). **The FLEET-WIDTH exemption
/// class** (symmetric PR 13c, F-B3): a cluster-wire LISTENER's load is the
/// whole fleet's width × each member's session demand, which N co-located
/// daemons each serve WHOLE — the share divisor would shrink the cap
/// exactly as the fleet it serves grows (64 on a 32-member fleet on 32
/// cores, the 14th member refused at accept). Its consumer census is
/// pinned beside `possible_cpus`'s in `tests/derivation_sweep_tests.rs`.
pub fn raw_parallelism() -> usize {
    raw_process_parallelism()
}

fn raw_process_parallelism() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(compute_process_parallelism)
}

fn compute_process_parallelism() -> usize {
    // SAFETY: zeroed cpu_set_t is a valid empty set; sched_getaffinity
    // writes at most size_of::<cpu_set_t>() bytes into it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(
            std::process::id() as libc::pid_t,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut set,
        ) == 0
        {
            let count = (0..libc::CPU_SETSIZE as usize)
                .filter(|&i| libc::CPU_ISSET(i, &set))
                .count();
            if count > 0 {
                return count;
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_parallelism_nonzero_and_stable() {
        let a = process_parallelism();
        let b = process_parallelism();
        assert!(a >= 1);
        assert_eq!(a, b, "cached value must be stable");
    }

    /// The division law's pure form (the derivation-sweep suite carries
    /// the canonical rows): ceil, never 0, share-1 identity.
    #[test]
    fn test_effective_parallelism_ceil_never_zero() {
        assert_eq!(effective_parallelism_from(32, 4), 8);
        assert_eq!(effective_parallelism_from(32, 5), 7);
        assert_eq!(effective_parallelism_from(2, 64), 1);
        assert_eq!(effective_parallelism_from(0, 1), 1);
        for raw in 1..=64 {
            assert_eq!(effective_parallelism_from(raw, 1), raw);
        }
    }
}
