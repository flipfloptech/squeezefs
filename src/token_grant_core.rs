//! Symmetric metadata program, PR 5 — the **GRANT ∥ PASS gate** of the
//! read-token holder (`docs/design-symmetric-metadata.md` §5.7.1; review
//! round 1, Issue 2).
//!
//! Extracted dependency-free so `loom-models/` can `#[path]`-include it
//! and model-check the exact shipped order (the `slot_lease_core`
//! pattern). The main build never sets `cfg(loom)`.
//!
//! **The law**: a first-touch grant and the conveyor pass that mutates
//! the same object race on two tables — the pass's IN-FLIGHT set (the
//! objects of its batch, marked from the union to the apply's settle) and
//! the lane's HOLDER table (the grant's registration). Lustre's intent
//! lock in two steps, a Dekker pair whose ORDER is the load-bearing part:
//!
//! * the pass MARKS every object of its union in flight, THEN reads each
//!   object's holders (and recalls them);
//! * a grant REGISTERS its token in the holder table, THEN reads the
//!   in-flight set: marked ⇒ it PARKS until the pass settles and reads
//!   the records after the apply; clear ⇒ it reads now, and any pass
//!   that begins afterwards finds it in the table.
//!
//! Both tables are mutexed and each side takes BOTH locks, one per step,
//! so the two steps of one side are ordered against the other's by the
//! locks' own acquire/release chains (a lock is an RMW on its word, so
//! whichever side takes the second lock later also observes the other's
//! first step) — no extra fence is needed and none is claimed. One side
//! therefore always sees the other: a grant registered concurrently with
//! a pass is either recalled by it (the pass saw the holder) or served the
//! post-commit records (the grant saw the mark) — never neither, which
//! was the first build's hole (the grant recorded AFTER its read; the
//! pass marking only objects that already had holders). Weakening-
//! verified in `loom-models` (`token_grant_models`): with either side's
//! two steps SWAPPED (read before write) loom finds the schedule where the
//! pass reads no holder AND the grant reads no mark.
//!
//! The in-flight set is a mutexed REFCOUNT map (`BTreeMap<object, users>`
//! — review round 2, Issue 23): the gate has TWO users that overlap by
//! design — the conveyor pass task (stage A) and the durability lane's
//! rollback of a FAILED window (stage B, Issue 14) — and both may hold
//! the same object in flight at once (a rollback's undo key and a later
//! pass's other key of one ino). A set would let the first `settle`
//! clear the other's mark and a grant registered between proceed against
//! a rollback still in progress; with the count an object leaves the
//! flight only when its LAST user settles. Control plane — one lock per
//! pass on an ARMED holder, never the unarmed hot path, which stops at
//! the `token_holder()` probe; the holder table is the S10
//! `RecallLane`'s, reached through [`HolderTable`].

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::Mutex;
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::Mutex;
}

use std::collections::BTreeMap;
use sync::Mutex;

/// The holder table the gate registers grants in and reads holders from
/// (the S10 `RecallLane` in the product; a mutexed map in the models).
pub trait HolderTable {
    /// Register `client`'s token on `object`; `true` when it was NOT
    /// held before (the grant's `already` is the negation).
    fn register(&self, object: u64, client: &str) -> bool;
    /// Outstanding holders of `object`.
    fn holders(&self, object: u64) -> usize;
}

/// The grant's verdict after registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAdmission {
    /// No pass holds the object in flight: read the records now.
    Proceed,
    /// A pass has the object in flight: park until it settles, then read
    /// (the records served are the post-commit ones).
    Park,
}

/// The gate: the in-flight objects, each with the count of its users
/// (passes and rollbacks between their `pass_begin` and their `settle`).
#[derive(Debug, Default)]
pub struct GrantPassGate {
    inflight: Mutex<BTreeMap<u64, usize>>,
}

impl GrantPassGate {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(BTreeMap::new()),
        }
    }

    /// **The pass's half**: mark every object of the union in flight
    /// (one lock), THEN read each object's holders through `table` (the
    /// lane's lock). Returns `(object, holders)` for the union — the
    /// caller recalls where `holders > 0` and settles EVERY object after
    /// its apply.
    pub fn pass_begin<T: HolderTable>(&self, objects: &[u64], table: &T) -> Vec<(u64, usize)> {
        {
            let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            for o in objects {
                *g.entry(*o).or_insert(0) += 1;
            }
        }
        objects.iter().map(|o| (*o, table.holders(*o))).collect()
    }

    /// This user's apply landed (or failed as a unit): its count on each
    /// object drops; an object leaves the flight when its LAST user
    /// settles. Returns whether any object left the flight (the parked
    /// grants' wake).
    pub fn settle(&self, objects: &[u64]) -> bool {
        let mut g = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        for o in objects {
            if let Some(n) = g.get_mut(o) {
                *n -= 1;
                if *n == 0 {
                    g.remove(o);
                    any = true;
                }
            }
        }
        any
    }

    /// **The grant's half**: register the token (the lane's lock), THEN
    /// read the in-flight set (this lock). Returns `(already, admission)`.
    pub fn grant_register<T: HolderTable>(
        &self,
        object: u64,
        client: &str,
        table: &T,
    ) -> (bool, GrantAdmission) {
        let fresh = table.register(object, client);
        let admission = if self.is_inflight(object) {
            GrantAdmission::Park
        } else {
            GrantAdmission::Proceed
        };
        (!fresh, admission)
    }

    /// Is `object` in some user's flight right now?
    pub fn is_inflight(&self, object: u64) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&object)
    }

    /// Objects in flight right now (the stats face).
    pub fn inflight_len(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}
