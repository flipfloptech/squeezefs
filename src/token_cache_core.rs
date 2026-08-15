//! The S8 **client token cache**'s word protocol — the monotone
//! per-object grant slot and the owner-era floor behind
//! [`crate::meta_ship::tokens`] (spec §6.7 decision 3; §6.9's named loom
//! obligation `token_cache_core`).
//!
//! Extracted dependency-free (KD-MW-10, `docs/design-full-multi-writer.md`)
//! so `loom-models/` can `#[path]`-include it and model-check the exact
//! shipped transitions — the `gauge_core` pattern; the main build never
//! sets `cfg(loom)`. What the models pin (`token_cache_models` in
//! `loom-models/src/lib.rs`):
//!
//! * **Per-slot monotonicity** ([`TokenSlot::merge`]): a reordered or
//!   replayed owner reply can never move an object's generation
//!   backwards — the property every consumer comparison (`<`, `==`,
//!   `.max()`) needs. Weakening-verified: replacing the `fetch_max` with
//!   a load-compare-store fails the model (the lost update regresses a
//!   racing higher grant).
//! * **Era-floor monotonicity** ([`EraFloor::record`]): same law for the
//!   owner term a miss floors on.
//! * **The record ORDER** ([`record_grant_ordered`]): the floor is
//!   recorded BEFORE the grant becomes findable, so a sweep that retires
//!   the entry can never expose a miss whose floor predates the grant's
//!   own era — too low "adopts a superseded record" (§6.11's inversion).
//!   Weakening-verified: flipping the two steps fails the model. (The
//!   findability edge itself — publish/retire — is the `scc` bucket's
//!   own synchronization, a stated model precondition per the
//!   `ipc_ring_core` lesson.)
//!
//! The container (the `scc::HashMap` keyed by ino, its capacity law and
//! the second-chance sweep's retain walk) stays in
//! `src/meta_ship/tokens.rs`; the words each entry carries live here.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
}

use atomic::{AtomicBool, AtomicU64, Ordering};

/// One cached grant: the object's fencing generation as the owner last
/// answered it, the era it was granted in, and the second-chance
/// reference bit the sweep consumes.
#[derive(Debug)]
pub struct TokenSlot {
    token: AtomicU64,
    /// The grant's era — merged monotonically beside the token. Written
    /// on every grant and deliberately not (yet) consumed: it is the
    /// audit face a future per-object era comparison reads, and dropping
    /// it would change the entry's accounting size (`ENTRY_BYTES`).
    term: AtomicU64,
    /// Second-chance reference bit: set on every serve/merge, cleared by
    /// a sweep, and its clearing is what earns the entry one more pass.
    used: AtomicBool,
}

impl TokenSlot {
    /// A freshly granted entry (reference bit set — it was just used).
    pub fn granted(token: u64, term: u64) -> Self {
        Self {
            token: AtomicU64::new(token),
            term: AtomicU64::new(term),
            used: AtomicBool::new(true),
        }
    }

    /// Fold a grant in. Monotone per object: a reordered or replayed
    /// reply can never lower the generation. Weakening-verified (loom
    /// `racing_grants_converge_to_the_max_and_never_regress`): a
    /// load-compare-store here loses the race and regresses.
    pub fn merge(&self, token: u64, term: u64) {
        self.token.fetch_max(token, Ordering::AcqRel);
        self.term.fetch_max(term, Ordering::AcqRel);
        self.used.store(true, Ordering::Relaxed);
    }

    /// Serve the cached generation, marking the reference bit (what earns
    /// the entry its next second-chance pass).
    pub fn serve(&self) -> u64 {
        self.used.store(true, Ordering::Relaxed);
        self.token.load(Ordering::Acquire)
    }

    /// One second-chance probe: `true` = the entry was used since the
    /// last sweep and is KEPT (its bit now cleared — that clearing is
    /// what earns it one more pass); `false` = retire it. Retiring costs
    /// at most one loud miss and one refresh, never a wrong answer.
    pub fn keep_for_another_pass(&self) -> bool {
        self.used.swap(false, Ordering::AcqRel)
    }
}

/// The owner era the cache last learned — a miss's floor, and the reason
/// a miss degrades exactly as a fresh mount does rather than answering 0
/// on a volume whose records name eras.
#[derive(Debug)]
pub struct EraFloor {
    term: AtomicU64,
}

impl EraFloor {
    /// A floor that has learned nothing (era 0 — the fresh-mount answer).
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            term: AtomicU64::new(0),
        }
    }

    /// Loom's atomics are not const-constructible; the models build at
    /// runtime.
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            term: AtomicU64::new(0),
        }
    }

    /// Record an owner era. Monotone — a reordered reply never lowers it.
    pub fn record(&self, term: u64) {
        self.term.fetch_max(term, Ordering::AcqRel);
    }

    /// The era a miss floors on.
    pub fn get(&self) -> u64 {
        self.term.load(Ordering::Acquire)
    }
}

#[cfg(not(loom))]
impl Default for EraFloor {
    fn default() -> Self {
        Self::new()
    }
}

/// The grant-recording ORDER (the shipped `record_grant` shape): the era
/// floor is recorded BEFORE the grant is published into the cache, so an
/// entry that later retires can never expose a miss whose floor predates
/// the grant's own era. `publish` is the container's insert/merge (the
/// `scc` op in production; the model's stand-in cell under loom).
/// Weakening-verified (loom
/// `a_retired_grant_never_lowers_the_miss_floor`): flipping these two
/// steps fails the model.
pub fn record_grant_ordered<P: FnOnce()>(floor: &EraFloor, term: u64, publish: P) {
    floor.record(term);
    publish();
}
