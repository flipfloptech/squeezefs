//! The ring-ingress residence CLOCK (reap-fanin campaign, 2026-08-08):
//! CLOCK_MONOTONIC truncated to its low 32 bits of nanoseconds — the
//! one clock BOTH sides of the [`crate::layout::IpcSlot::stamp_ingress`]
//! protocol read, so the daemon's dequeue-time subtraction
//! ([`crate::layout::ingress_delta_ns`]) is a measurement, not a
//! cross-clock guess. Same host, same boot (the session protocol's own
//! precondition — design KD-7), so the domain is shared by construction;
//! the u32 truncation wraps every ~4.295 s, which is why the delta law
//! carries its 1 s plausibility ceiling.
//!
//! Canonical file in the `squeezefs-ipc` tree, `#[path]`-included by the
//! ROOT crate (the dequeue-side reader) and the PRELOAD shim (the
//! publish-side stamper) — the `numa_core.rs`/`thp.rs` production-sharing
//! precedent. Deliberately NOT a module of `squeezefs-ipc` itself: the
//! protocol library is dependency-free (loom-included cores), and this
//! file needs `libc::clock_gettime`, which both consumers already carry.

/// CLOCK_MONOTONIC now, low 32 bits of nanoseconds. Rust's `Instant` is
/// the same kernel clock on Linux, so spans built from this compose with
/// the `Instant`-anchored phase histograms.
#[inline]
pub fn monotonic_stamp_ns_u32() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: plain clock_gettime(2) into a live timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock is monotonic modulo wrap: two adjacent readings differ
    /// by a small plausible delta (the wrap window is ~4.295 s — a unit
    /// test never spans it).
    #[test]
    fn adjacent_stamps_are_plausibly_ordered() {
        let a = monotonic_stamp_ns_u32();
        let b = monotonic_stamp_ns_u32();
        let delta = b.wrapping_sub(a);
        assert!(
            u64::from(delta) < 1_000_000_000,
            "adjacent stamps must sit within the plausibility ceiling \
             (a={a}, b={b}, delta={delta})"
        );
    }
}
