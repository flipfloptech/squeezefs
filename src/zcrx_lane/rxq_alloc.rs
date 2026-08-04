//! Process-wide per-NIC RX-queue arbiter — the 2026-08-04 field row-3
//! `REGISTER_ZCRX_IFQ (if_idx=6, rxq=31): File exists (EEXIST)` fix.
//!
//! One zcrx ifq binds one `(ifindex, rxq)` pair kernel-wide; the
//! pre-arbiter arm derived the SAME highest-indexed §8 picks for every
//! session on a shared NIC (10 fabric devices ride one mlx5), so the
//! second session's REGISTER collided. The arbiter is the process-wide
//! registry those sessions were missing: DISTINCT queues from the
//! NIC-derived lane-eligible pool, freed on lease drop (the session —
//! and thus its ifqs — going away), loud refusal with the derived
//! numbers when the pool exhausts. Nothing here is a constant (the
//! derivation law): the pool is `steering::lane_eligible_queues`
//! (`channels / 4`, design §8) probed from THIS NIC at arm time.
//!
//! Cross-PROCESS arbitration stays the kernel's: a second daemon's
//! REGISTER on a leased queue still refuses EEXIST loud and that mount
//! stays on the kernel path (the OnceCell-cached per-device refusal).

/// One session's granted RX queues on one NIC. RAII: dropping the lease
/// returns the indices to the NIC's pool (the session's ifqs close with
/// it — the ring driver's fd close is the kernel-side unregister; a
/// same-instant re-arm racing that async close can still see the
/// kernel's EEXIST, which refuses THAT arm loud and leaves the mount on
/// the kernel path — the kernel stays the cross-process/final arbiter).
#[derive(Debug, PartialEq, Eq)]
pub struct RxqLease {
    ifindex: u32,
    queues: Vec<u32>,
}

impl RxqLease {
    /// The granted RX queue indices, ascending (qid `1..=N` maps to
    /// `queues()[qid - 1]` — the plan's `rx_queues` law).
    pub fn queues(&self) -> &[u32] {
        &self.queues
    }
}

impl Drop for RxqLease {
    fn drop(&mut self) {
        release(self.ifindex, &self.queues);
    }
}

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// One NIC's arbitration state. Keyed by ifindex (kernel-unique per
/// NIC on this host — the same key `REGISTER_ZCRX_IFQ` collides on).
#[derive(Default)]
struct NicPool {
    /// Currently leased indices.
    in_use: BTreeSet<u32>,
    /// Freed indices in RELEASE order (front = least recently freed) —
    /// the reuse-hygiene law (2026-08 field finding 2): the kernel's
    /// ifq teardown is ASYNC (ring-fd close defers the unregister
    /// in-kernel), so a just-freed rxq re-granted instantly
    /// re-registers into EEXIST. Grants take FRESH queues first, then
    /// reuse least-recently-freed — serial re-arms walk the whole pool
    /// before any index repeats. Invariant: `retired ∩ in_use = ∅`.
    retired: VecDeque<u32>,
}

static REGISTRY: LazyLock<Mutex<HashMap<u32, NicPool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn registry() -> MutexGuard<'static, HashMap<u32, NicPool>> {
    // A panicked holder leaves plain collections in a valid state —
    // recover the inner map rather than poisoning every future arm.
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Grant `want` DISTINCT lane RX queues on the NIC `ifindex` names, from
/// the §8-derived eligible pool of `channels` (probed from THIS NIC —
/// `steering::lane_eligible_queues`, never a constant). Grants prefer
/// the pure `lane_queue_picks` slice (uncontended parity with the
/// pre-arbiter behavior), fill highest-first from the remaining free
/// pool under contention (a partial grant degrades the session's queue
/// count — the same posture as the §8 ceiling clamp), and refuse loudly
/// — naming the NIC, the probed queue count, the derived pool and the
/// demand — when NO free queue exists (or the NIC is too narrow to
/// dedicate ZC queues at all).
pub fn acquire(ifindex: u32, ifname: &str, channels: u32, want: u16) -> Result<RxqLease, String> {
    if want == 0 {
        return Err(format!(
            "zcrx rxq arbiter: zero lane queues requested for {ifname} (ifindex \
             {ifindex}) — a lane with no queues cannot exist (refusing)"
        ));
    }
    let pool = super::steering::lane_eligible_queues(channels);
    let pool_len = pool.end.saturating_sub(pool.start);
    if pool_len == 0 {
        return Err(format!(
            "zcrx rxq arbiter: {ifname} (ifindex {ifindex}) has {channels} RX queues \
             — too narrow to dedicate ZC queues (needs ≥ 4: the lane pool is \
             nic_queues/4, design §8)"
        ));
    }
    let mut reg = registry();
    let entry = reg.entry(ifindex).or_default();
    let goal = u32::from(want).min(pool_len) as usize;
    // FRESH = free AND not recently freed (the finding-2 reuse-hygiene
    // law: a retired index is the LAST candidate — its kernel ifq
    // teardown may still be in flight).
    let fresh = |e: &NicPool, q: &u32| -> bool { !e.in_use.contains(q) && !e.retired.contains(q) };
    // Preferred grant: the pure §8 single-session picks…
    let mut grant: Vec<u32> = super::steering::lane_queue_picks(channels, want)
        .into_iter()
        .filter(|q| fresh(entry, q))
        .collect();
    // …contended: fill from the remaining FRESH pool, highest-first…
    if grant.len() < goal {
        for q in pool.clone().rev() {
            if grant.len() >= goal {
                break;
            }
            if fresh(entry, &q) && !grant.contains(&q) {
                grant.push(q);
            }
        }
    }
    // …exhausted-fresh: reuse retired indices, LEAST-recently-freed
    // first (their teardown has had the longest to complete).
    while grant.len() < goal {
        let pos = entry
            .retired
            .iter()
            .position(|q| pool.contains(q) && !grant.contains(q));
        match pos.and_then(|p| entry.retired.remove(p)) {
            Some(q) => grant.push(q),
            None => break,
        }
    }
    if grant.is_empty() {
        let leased = entry.in_use.len();
        return Err(format!(
            "zcrx rxq arbiter: {ifname} (ifindex {ifindex}) has {channels} RX queues \
             → {pool_len} lane-eligible (nic_queues/4, design §8: RSS keeps ≥ ¾ of \
             the NIC), all {leased} already leased by armed lane sessions — demand \
             for {want} more refused; kernel path serves (free a lane session, or \
             widen the NIC: ethtool -L {ifname} combined <N>)"
        ));
    }
    grant.sort_unstable();
    if grant.len() < goal {
        log::warn!(
            "zcrx rxq arbiter: {ifname} granted {} of {want} wanted lane queues \
             ({} of the {pool_len}-queue pool leased by other sessions) — this \
             session degrades its queue count",
            grant.len(),
            entry.in_use.len()
        );
    }
    for q in &grant {
        entry.retired.retain(|r| r != q);
        entry.in_use.insert(*q);
    }
    Ok(RxqLease {
        ifindex,
        queues: grant,
    })
}

fn release(ifindex: u32, queues: &[u32]) {
    let mut reg = registry();
    if let Some(entry) = reg.get_mut(&ifindex) {
        for q in queues {
            if entry.in_use.remove(q) {
                // Re-freed indices move to the BACK (most recent) —
                // the reuse-order history persists for the NIC's
                // lifetime (one tiny entry per NIC, never dropped:
                // dropping it would forget which ifq teardowns are
                // still settling).
                entry.retired.retain(|r| r != q);
                entry.retired.push_back(*q);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The TEST-6 posture (uring_zcrx.rs precedent): pure in-process
    // registry contracts — no NIC, no root; ifindexes are per-test
    // uniques so the process-wide registry never couples tests.

    #[test]
    fn arbiter_hands_out_distinct_queues_per_nic() {
        // Field row 3: two sessions on one NIC must never compute the
        // same rxq (REGISTER_ZCRX_IFQ EEXIST). 32 channels → pool 24..32.
        let ifx = 0xBEE0;
        let a = acquire(ifx, "unit-nic-a", 32, 4).expect("first lease");
        assert_eq!(
            a.queues(),
            &[28, 29, 30, 31],
            "single-session grant keeps parity with the §8 highest-indexed picks"
        );
        let b = acquire(ifx, "unit-nic-a", 32, 4).expect("second lease");
        for q in b.queues() {
            assert!(
                !a.queues().contains(q),
                "second session must get DISTINCT queues (got {:?} vs {:?})",
                b.queues(),
                a.queues()
            );
        }
        assert!(!b.queues().is_empty(), "pool had free queues to grant");
        for q in a.queues().iter().chain(b.queues()) {
            assert!(
                (24..32).contains(q),
                "grants stay inside the derived pool (channels − channels/4 .. channels)"
            );
        }
    }

    #[test]
    fn arbiter_exhaustion_refuses_loudly_with_the_derived_numbers() {
        // 8 channels → pool {6, 7}; a third queue cannot exist.
        let ifx = 0xBEE1;
        let hold = acquire(ifx, "unit-nic-b", 8, 2).expect("pool-filling lease");
        assert_eq!(hold.queues(), &[6, 7]);
        let err = acquire(ifx, "unit-nic-b", 8, 1).expect_err("exhausted pool must refuse");
        for needle in [
            "unit-nic-b",
            "8 RX queues",
            "2 lane-eligible",
            "demand for 1",
        ] {
            assert!(
                err.contains(needle),
                "refusal must name the NIC, the queue count, the derived pool and \
                 the demand — missing {needle:?} in: {err}"
            );
        }
    }

    #[test]
    fn arbiter_release_then_reacquire_reuses_freed_indices() {
        let ifx = 0xBEE2;
        let a = acquire(ifx, "unit-nic-c", 8, 2).expect("lease");
        assert_eq!(a.queues(), &[6, 7]);
        drop(a); // ifq closed → indices return to the pool
        let b = acquire(ifx, "unit-nic-c", 8, 2).expect("reacquire after release");
        assert_eq!(b.queues(), &[6, 7], "freed indices are reused");
    }

    #[test]
    fn arbiter_pool_derives_from_the_nic_queue_count_no_literal() {
        // The derivation law: the usable range is a FUNCTION of the
        // NIC's probed queue count (channels/4 top slice, §8) — never a
        // constant. Sweep several NIC widths.
        for channels in [8u32, 12, 32, 64, 96] {
            let ifx = 0xBEE8 + channels;
            let lease = acquire(ifx, "unit-nic-d", channels, u16::MAX)
                .unwrap_or_else(|e| panic!("{channels}-queue NIC must grant: {e}"));
            assert_eq!(
                lease.queues().len() as u32,
                channels / 4,
                "pool size = channels/4 at {channels} channels"
            );
            for q in lease.queues() {
                assert!(
                    *q >= channels - channels / 4 && *q < channels,
                    "grant {q} outside the derived range at {channels} channels"
                );
            }
        }
        // Too-narrow NIC (pool empty) refuses naming the numbers.
        let err = acquire(0xBFFF, "unit-nic-narrow", 3, 1).expect_err("3-queue NIC must refuse");
        assert!(
            err.contains("unit-nic-narrow") && err.contains('3'),
            "narrow refusal names the NIC and its queue count: {err}"
        );
    }

    #[test]
    fn arbiter_contention_grants_partial_and_nics_are_independent() {
        // Partial grant under contention: A takes 3 of pool {6,7} — clamped
        // to the pool — then B wants 2 and gets the remainder... with a
        // 12-channel NIC (pool 9..12): A wants 2 → {10, 11}; B wants 2 →
        // only {9} left → partial grant of 1 (arm degrades io_queues, the
        // §8 clamp posture — refusal is reserved for EMPTY).
        let ifx = 0xBEC0;
        let a = acquire(ifx, "unit-nic-e", 12, 2).expect("A");
        assert_eq!(a.queues(), &[10, 11]);
        let b = acquire(ifx, "unit-nic-e", 12, 2).expect("B partial");
        assert_eq!(b.queues(), &[9], "B gets the remaining free queue");
        // A DIFFERENT ifindex is a different pool entirely.
        let other = acquire(0xBEC1, "unit-nic-f", 12, 2).expect("other NIC");
        assert_eq!(other.queues(), &[10, 11]);
    }

    #[test]
    fn arbiter_serial_rearm_rotates_the_pool_before_reuse() {
        // FINDING 2's engine (2026-08 field): the kernel's ifq teardown
        // is ASYNC (ring-fd close defers the unregister in-kernel), so a
        // freed rxq re-granted instantly re-registers into EEXIST. Serial
        // re-arms must walk the WHOLE free pool (least-recently-freed
        // reuse) before an index repeats.
        let ifx = 0xBEE3;
        let mut seen: Vec<u32> = Vec::new();
        for i in 0..8 {
            let l = acquire(ifx, "unit-nic-rot", 32, 1).expect("grant");
            assert_eq!(l.queues().len(), 1);
            let q = l.queues()[0];
            assert!(
                !seen.contains(&q),
                "arm {i}: freed queue {q} reused while fresh queues remained \
                 (the EEXIST-recycle class): {seen:?}"
            );
            seen.push(q);
        }
        assert_eq!(seen[0], 31, "first grant keeps the §8 pick parity");
        // History exhausted: the 9th arm reuses the LEAST-recently-freed.
        let l = acquire(ifx, "unit-nic-rot", 32, 1).expect("grant");
        assert_eq!(
            l.queues()[0],
            seen[0],
            "reuse order is least-recently-freed first"
        );
    }

    #[test]
    fn arbiter_zero_want_refuses() {
        let err = acquire(0xBEC2, "unit-nic-g", 32, 0).expect_err("a lane with zero queues");
        assert!(err.contains("unit-nic-g"), "refusal names the NIC: {err}");
    }
}
