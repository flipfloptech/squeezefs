//! **The park gate's protocol core** — KD-SYM-15 (extended): a symmetric
//! appender's `T_self` action is PARK-AND-RECLAIM, not poison
//! (docs/design-symmetric-metadata.md §5.5.3; PR 8).
//!
//! Self-contained (std atomics under the main build, `loom`'s under
//! `--cfg loom`) so `loom-models/` `#[path]`-includes it and model-checks
//! the two racing edges: a committer entering the pre-admission gate
//! against the park being raised, and the release against the expiry.
//! The process plane around it — the notify the parked committers wait
//! on, the durability lane's held acks, the gauges, the `T_park_max`
//! derivation, `data_custody::poison` at expiry — is `crate::park_gate`
//! (never included by loom).
//!
//! # The word
//!
//! `Running → Parked → Running` (a reclaim under the successor's grace) or
//! `Parked → Expired` (the park outlived `T_park_max`: the member
//! self-fences terminally, every held ack fails `EIO`). `Expired` is
//! sticky — the shipped `data_custody::poison` posture, reached by the
//! park's one terminal signal only.
//!
//! # The two invariants the models pin
//!
//! 1. **A commit is never both admitted-in-flight and parked** (the
//!    Dekker pair of [`ParkCore::try_enter`] and [`ParkCore::park`]): the
//!    committer registers itself in flight BEFORE reading the word, the
//!    parker writes the word BEFORE reading the in-flight count, both with
//!    a `SeqCst` fence between — so either the committer sees `Parked` and
//!    withdraws (it parks), or the parker sees it in flight (its entry
//!    lands, its ack is held). A committer that slipped through unseen by
//!    both is the acked-then-lost shape the weakened model reports.
//! 2. **Release and expiry are exclusive**: exactly one of two racing
//!    `release` / `expire` calls moves the word off `Parked`, so a held ack
//!    is released OR failed, never both and never neither.

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{fence, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{fence, AtomicU64, Ordering};
}

use sync::{fence, AtomicU64, Ordering};

/// The gate is open: commits admit, acks flow.
pub const RUNNING: u64 = 0;
/// The gate is parked: admission waits, landed entries' acks are held.
pub const PARKED: u64 = 1;
/// The park outlived its bound: terminal, every parked op fails.
pub const EXPIRED: u64 = 2;

/// The verdict a committer reads at the pre-admission door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterVerdict {
    /// Admitted — the commit is in flight; its ack will be held if a
    /// park is raised before the lane answers it.
    Admitted,
    /// Parked — the committer waits for a release and re-enters.
    Parked,
    /// The gate expired — the commit fails `EIO`.
    Expired,
}

/// The state word plus the in-flight count and the ledger counters.
#[derive(Debug)]
pub struct ParkCore {
    state: AtomicU64,
    /// Commits admitted and not yet answered (the entries whose acks a
    /// park holds).
    inflight: AtomicU64,
    /// The park's start, in the caller's clock ms (0 = not parked).
    parked_since_ms: AtomicU64,
    parks: AtomicU64,
    expiries: AtomicU64,
    reclaims: AtomicU64,
    /// Acks the lane held across a park and released on the grant.
    acks_held: AtomicU64,
}

impl Default for ParkCore {
    fn default() -> Self {
        Self::new()
    }
}

impl ParkCore {
    pub fn new() -> Self {
        Self {
            state: AtomicU64::new(RUNNING),
            inflight: AtomicU64::new(0),
            parked_since_ms: AtomicU64::new(0),
            parks: AtomicU64::new(0),
            expiries: AtomicU64::new(0),
            reclaims: AtomicU64::new(0),
            acks_held: AtomicU64::new(0),
        }
    }

    /// The word.
    pub fn state(&self) -> u64 {
        self.state.load(Ordering::Acquire)
    }

    /// `true` ⇔ parked (the posture word `appender_parked`).
    pub fn is_parked(&self) -> bool {
        self.state() == PARKED
    }

    /// `true` ⇔ the park expired (terminal).
    pub fn is_expired(&self) -> bool {
        self.state() == EXPIRED
    }

    /// **The committer's side of the Dekker pair**: register in flight,
    /// fence, read the word. `Admitted` leaves the in-flight count raised
    /// (the lane's answer lowers it through [`Self::leave`]); the other two
    /// verdicts withdraw it.
    pub fn try_enter(&self) -> EnterVerdict {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        match self.state.load(Ordering::SeqCst) {
            RUNNING => EnterVerdict::Admitted,
            PARKED => {
                self.inflight.fetch_sub(1, Ordering::SeqCst);
                EnterVerdict::Parked
            }
            _ => {
                self.inflight.fetch_sub(1, Ordering::SeqCst);
                EnterVerdict::Expired
            }
        }
    }

    /// An admitted commit reached its terminal outcome.
    pub fn leave(&self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Commits in flight.
    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::SeqCst)
    }

    /// **The parker's side of the Dekker pair**: raise `Parked` (from
    /// `Running` only — idempotent on a standing park, refused on
    /// `Expired`), fence, and answer the in-flight count the park will
    /// hold acks for. `None` ⇔ the gate was already parked or expired.
    pub fn park(&self, now_ms: u64) -> Option<u64> {
        if self
            .state
            .compare_exchange(RUNNING, PARKED, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        self.parked_since_ms.store(now_ms.max(1), Ordering::SeqCst);
        self.parks.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        Some(self.inflight.load(Ordering::SeqCst))
    }

    /// **Release** (the successor's grant confirmed the leases): `Parked →
    /// Running`. `true` ⇔ this call moved it — the caller then wakes the
    /// parked committers and releases the held acks IN ORDER.
    pub fn release(&self) -> bool {
        let moved = self
            .state
            .compare_exchange(PARKED, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if moved {
            self.parked_since_ms.store(0, Ordering::SeqCst);
            self.reclaims.fetch_add(1, Ordering::SeqCst);
        }
        moved
    }

    /// **Expire** (the park outlived `t_park_max_ms`): `Parked → Expired`
    /// iff the park's age at `now_ms` reached the bound. `true` ⇔ this call
    /// moved it — the caller poisons custody and fails the held acks.
    pub fn expire(&self, now_ms: u64, t_park_max_ms: u64) -> bool {
        if self.state.load(Ordering::SeqCst) != PARKED {
            return false;
        }
        let since = self.parked_since_ms.load(Ordering::SeqCst);
        if since == 0 || now_ms.saturating_sub(since) < t_park_max_ms {
            return false;
        }
        let moved = self
            .state
            .compare_exchange(PARKED, EXPIRED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if moved {
            self.expiries.fetch_add(1, Ordering::SeqCst);
        }
        moved
    }

    /// The park's age at `now_ms` (0 when not parked).
    pub fn parked_for_ms(&self, now_ms: u64) -> u64 {
        let since = self.parked_since_ms.load(Ordering::Acquire);
        if since == 0 {
            0
        } else {
            now_ms.saturating_sub(since)
        }
    }

    /// Count `n` acks held across a park.
    pub fn note_acks_held(&self, n: u64) {
        self.acks_held.fetch_add(n, Ordering::Relaxed);
    }

    /// `appender_parks`.
    pub fn parks(&self) -> u64 {
        self.parks.load(Ordering::Relaxed)
    }

    /// `appender_park_expiries` (must-stay-0 on a healthy failover).
    pub fn expiries(&self) -> u64 {
        self.expiries.load(Ordering::Relaxed)
    }

    /// `slot_lease_reclaims` — parks released by a reclaim.
    pub fn reclaims(&self) -> u64 {
        self.reclaims.load(Ordering::Relaxed)
    }

    /// Acks held across parks.
    pub fn acks_held(&self) -> u64 {
        self.acks_held.load(Ordering::Relaxed)
    }

    /// **Test seam**: back to `Running`, every counter cleared (a process
    /// runs several parks in one test binary).
    pub fn test_reset(&self) {
        self.state.store(RUNNING, Ordering::SeqCst);
        self.inflight.store(0, Ordering::SeqCst);
        self.parked_since_ms.store(0, Ordering::SeqCst);
        self.parks.store(0, Ordering::SeqCst);
        self.expiries.store(0, Ordering::SeqCst);
        self.reclaims.store(0, Ordering::SeqCst);
        self.acks_held.store(0, Ordering::SeqCst);
    }
}
