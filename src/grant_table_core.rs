//! DLM **S9**'s custody **grant table** — the client-lease and grant
//! bookkeeping behind [`crate::data_grant::WriteCustodyOwner`] (spec
//! §6.9's named loom obligation `grant_table_core`; KD-MW-10,
//! `docs/design-full-multi-writer.md`).
//!
//! Extracted dependency-free so `loom-models/` can `#[path]`-include it
//! and model-check the exact shipped transitions — the `conveyor_core`
//! pattern (short-hold interior mutexes around O(1)/O(entries) table
//! ops, never held across an await; the commit paths are in the
//! sanctioned metadata/lease-transaction lock class). The main build
//! never sets `cfg(loom)`.
//!
//! What lives here is the PROTOCOL the models exercise
//! (`grant_table_models` in `loom-models/src/lib.rs`):
//!
//! * **lease epochs are minted monotonically and never reused**
//!   ([`GrantTableCore::join`] — the `fetch_add` word);
//! * **a re-join replaces the prior lease and its custody dies with it**
//!   (the `on_kill` seam runs BEFORE the new lease becomes visible, so
//!   the dead epoch's quarantine precedes any grant under the new one —
//!   the shipped `join` order, preserved);
//! * **kill is exactly-once by pop ownership** ([`GrantTableCore::revoke`]'s
//!   `clients` removal is the linearization point — racing revoke /
//!   expiry paths retire a client's grants and quarantine its in-flight
//!   set exactly once);
//! * **a grant can never outlive its lease**
//!   ([`GrantTableCore::commit_grant_if_current`] — validate-and-commit as ONE
//!   step under the `clients` lock, never check-then-commit across the
//!   arbitration await: the rung-4 loom finding's fix);
//! * **the grace window admits reclaim only and closes exactly once**
//!   (deadline close or full re-assertion, whichever first).
//!
//! The wire frames, counters, phase records, the arbiter
//! (`LocalLockManager`) and the S7 quarantine stay in
//! `src/data_grant.rs`; `L` is the authority's own lease handle
//! (`LockLease` in production — dropping it is what makes the bytes
//! grantable again, which is why [`GrantTableCore::revoke`] drops retired
//! entries INSIDE the call rather than returning them).

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
    pub use loom::sync::{Mutex, MutexGuard};
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{AtomicU64, Ordering};
    pub use std::sync::{Mutex, MutexGuard};
}

use std::collections::{BTreeSet, HashMap};
use sync::{AtomicU64, Mutex, Ordering};

/// One client's lease as the authority holds it.
#[derive(Debug, Clone)]
pub struct LeaseState {
    /// The lease epoch — monotone per authority, never reused.
    pub epoch: u64,
    /// The client's NVMe registrant key (`0` = none).
    pub pr_key: u64,
    /// Last renewal instant (owner clock, ms).
    pub renewed_ms: u64,
    /// The owner-side deadline after which the bytes may be granted
    /// elsewhere.
    pub deadline_ms: u64,
    /// The client's declared in-flight destination offsets — the cohort a
    /// dead epoch's quarantine is keyed on.
    pub inflight: Vec<u64>,
    /// Grants whose custody died while this client was away — drained by
    /// its next renewal (the pull-based revocation channel).
    pub dead_grants: Vec<u64>,
}

/// One live grant. `L` is the authority's OWN lease on the bytes —
/// dropping it is what makes them grantable again, so a revoke is
/// structurally a drop, not a bookkeeping edit.
#[derive(Debug)]
pub struct GrantEntry<L> {
    pub client: String,
    pub lease_epoch: u64,
    pub ino: u64,
    pub span: Option<(u64, u64)>,
    pub lease: L,
}

/// What a kill removed: the lease (its declared in-flight set is the
/// quarantine cohort) and the retired grant ids (their `L` handles were
/// dropped inside the call — the bytes are already grantable again).
#[derive(Debug)]
pub struct Killed {
    pub lease: LeaseState,
    pub grant_ids: Vec<u64>,
}

struct GraceState {
    until_ms: u64,
    expected: BTreeSet<String>,
}

/// One [`GrantTableCore::probe_grace`] outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum GraceProbe {
    /// No window is open.
    Closed,
    /// The window just closed ON ITS DEADLINE, with this many prior
    /// co-writers never re-asserting (the caller's log line).
    ClosedNow { never_reasserted: usize },
    /// The window is open: reclaim only.
    Open,
}

/// One [`GrantTableCore::note_reclaim`] outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum ReclaimNote {
    /// No window was open — nothing to note.
    NoWindow,
    /// Noted; other prior co-writers are still awaited.
    Waiting,
    /// Every prior co-writer has re-asserted — the window closed early
    /// (the caller's log line).
    ClosedEarly,
}

/// The custody grant table: client leases, live grants, the failover
/// grace window, and the two mint words.
pub struct GrantTableCore<L> {
    clients: Mutex<HashMap<String, LeaseState>>,
    grants: Mutex<HashMap<u64, GrantEntry<L>>>,
    next_epoch: AtomicU64,
    next_grant: AtomicU64,
    grace: Mutex<Option<GraceState>>,
}

impl<L> Default for GrantTableCore<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L> GrantTableCore<L> {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            grants: Mutex::new(HashMap::new()),
            next_epoch: AtomicU64::new(1),
            next_grant: AtomicU64::new(1),
            grace: Mutex::new(None),
        }
    }

    fn lock_clients(&self) -> sync::MutexGuard<'_, HashMap<String, LeaseState>> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_grants(&self) -> sync::MutexGuard<'_, HashMap<u64, GrantEntry<L>>> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admit `client` (fresh, or a re-join that lost its lease view),
    /// minting its lease epoch. A re-join REPLACES the prior lease (same
    /// identity, new epoch): the client is telling us it lost its view,
    /// and keeping the old epoch alive would leave custody nobody
    /// presents. Its GRANTS are revoked with it — `on_kill` runs the
    /// caller's death path (quarantine, dead-epoch mint, counters)
    /// BEFORE the new lease becomes visible, preserving the shipped
    /// order: a dead epoch's offsets are quarantined before anything can
    /// be granted under the successor lease.
    pub fn join<F: FnOnce(Killed)>(
        &self,
        client: &str,
        pr_key: u64,
        prior_epoch_claim: Option<u64>,
        now_ms: u64,
        ttl_ms: u64,
        on_kill: F,
    ) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::AcqRel);
        let prior = self.lock_clients().get(client).map(|l| l.epoch);
        if let Some(prior) = prior {
            if Some(prior) != prior_epoch_claim || prior_epoch_claim.is_none() {
                if let Some(killed) = self.revoke(client) {
                    on_kill(killed);
                }
            }
        }
        self.lock_clients().insert(
            client.to_string(),
            LeaseState {
                epoch,
                pr_key,
                renewed_ms: now_ms,
                deadline_ms: now_ms + ttl_ms,
                inflight: Vec::new(),
                dead_grants: Vec::new(),
            },
        );
        epoch
    }

    /// Is `client`'s presented lease epoch its current custody?
    pub fn lease_current(&self, client: &str, epoch: u64) -> bool {
        self.lock_clients()
            .get(client)
            .map(|l| l.epoch == epoch)
            .unwrap_or(false)
    }

    /// A client's OWNER-side deadline, ms.
    pub fn lease_deadline_ms(&self, client: &str) -> Option<u64> {
        self.lock_clients().get(client).map(|l| l.deadline_ms)
    }

    /// `true` ⇔ some live lease carries `epoch` — the shipped-free gate's
    /// era check (keyed on the epoch alone by design; see
    /// `WriteCustodyOwner::check_free`).
    pub fn epoch_live(&self, epoch: u64) -> bool {
        self.lock_clients().values().any(|l| l.epoch == epoch)
    }

    /// Record a granted custody — **validate-and-commit as ONE
    /// linearization step** (the rung-4 loom finding's fix, adjudicated
    /// 2026-08-15): the client's lease epoch is re-validated under the
    /// `clients` lock and the grant is inserted while that lock is still
    /// held, so a racing kill can only linearize BEFORE this commit (the
    /// commit refuses — `Err` hands the caller's own lease back for
    /// release) or AFTER it (its grant scan then retires this grant).
    /// There is no third interleaving — the orphan-grant wedge the
    /// pre-fix check-then-commit across the arbitration await admitted
    /// (loom `a_granted_custody_never_outlives_its_lease`; RED at commit
    /// `ea34796c` against the unconditional-commit form, which IS this
    /// method's weakening evidence).
    ///
    /// Lock order: `clients` then `grants`, nested HERE only; no path in
    /// this module takes `grants` before `clients`, so the nesting is
    /// acyclic.
    pub fn commit_grant_if_current(
        &self,
        client: &str,
        lease_epoch: u64,
        ino: u64,
        span: Option<(u64, u64)>,
        lease: L,
    ) -> std::result::Result<u64, L> {
        let clients = self.lock_clients();
        let current = clients
            .get(client)
            .map(|l| l.epoch == lease_epoch)
            .unwrap_or(false);
        if !current {
            return Err(lease);
        }
        let grant_id = self.next_grant.fetch_add(1, Ordering::AcqRel);
        let _ = self.lock_grants().insert(
            grant_id,
            GrantEntry {
                client: client.to_string(),
                lease_epoch,
                ino,
                span,
                lease,
            },
        );
        drop(clients);
        Ok(grant_id)
    }

    /// Renew a client lease if `epoch` is its current custody: re-anchor
    /// the deadline, absorb the declared in-flight set, and drain the
    /// dead-grant list (the pull-based revocation channel). `None` = not
    /// custody.
    pub fn renew(
        &self,
        client: &str,
        epoch: u64,
        now_ms: u64,
        ttl_ms: u64,
        inflight: &[u64],
    ) -> Option<Vec<u64>> {
        let mut clients = self.lock_clients();
        match clients.get_mut(client) {
            Some(l) if l.epoch == epoch => {
                l.renewed_ms = now_ms;
                l.deadline_ms = now_ms + ttl_ms;
                l.inflight = inflight.to_vec();
                Some(std::mem::take(&mut l.dead_grants))
            }
            _ => None,
        }
    }

    /// S11 rung 15: record the WIDENED span of a grant the arbiter's
    /// admit-time merge extended (the audit/reclaim surface must agree
    /// with the custody actually held). `false` ⇔ the grant is gone (a
    /// racing kill retired it — the widened record died whole with it).
    pub fn widen_grant_span(&self, grant_id: u64, span: Option<(u64, u64)>) -> bool {
        match self.lock_grants().get_mut(&grant_id) {
            Some(g) => {
                g.span = span;
                true
            }
            None => false,
        }
    }

    /// Retire named grants at the client's request (ownership-checked).
    /// Dropping the `L` is what makes the bytes grantable again.
    pub fn release(&self, client: &str, grant_ids: &[u64]) -> usize {
        let mut n = 0;
        for id in grant_ids {
            let mut grants = self.lock_grants();
            let mine = grants.get(id).map(|g| g.client == client).unwrap_or(false);
            if !mine {
                continue;
            }
            if grants.remove(id).is_some() {
                n += 1;
            }
        }
        n
    }

    /// The kill's table half: remove `client`'s lease (the exactly-once
    /// linearization point — a racing killer gets `None`), then retire
    /// every grant it held, DROPPING each `L` (the bytes become grantable
    /// during the call, exactly as the shipped scan did). Returns the
    /// removed lease + the retired grant ids, sorted.
    ///
    /// Pop ownership is load-bearing and weakening-verified (loom
    /// `racing_kill_paths_retire_a_client_exactly_once`): substituting a
    /// check-then-remove across two lock takes lets an operator revoke
    /// and the TTL sweep BOTH claim the death — two quarantine cohorts
    /// and two dead epochs for one client.
    pub fn revoke(&self, client: &str) -> Option<Killed> {
        let lease = self.lock_clients().remove(client)?;
        let mut grants = self.lock_grants();
        let mut ids: Vec<u64> = grants
            .iter()
            .filter(|(_, g)| g.client == client)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        for id in &ids {
            let _ = grants.remove(id);
        }
        Some(Killed {
            lease,
            grant_ids: ids,
        })
    }

    /// Every client past its deadline at `now_ms` (`>=`: the lease
    /// expires AT the deadline, which is the instant the client's own
    /// strictly earlier deadline was measured against), sorted. The
    /// caller kills each via [`Self::revoke`] — pop ownership makes a
    /// racing revoke harmless.
    pub fn expire_scan(&self, now_ms: u64) -> Vec<String> {
        let mut due: Vec<String> = self
            .lock_clients()
            .iter()
            .filter(|(_, l)| now_ms >= l.deadline_ms)
            .map(|(id, _)| id.clone())
            .collect();
        due.sort();
        due
    }

    /// Open the failover grace window until `until_ms`, awaiting
    /// re-assertion from `expected`. Returns the deduplicated count (the
    /// caller's log line).
    pub fn open_grace(&self, until_ms: u64, expected: Vec<String>) -> usize {
        let expected: BTreeSet<String> = expected.into_iter().collect();
        let n = expected.len();
        *self.grace.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(GraceState { until_ms, expected });
        n
    }

    /// Probe the window at `now_ms`, closing it exactly once when the
    /// deadline has passed.
    pub fn probe_grace(&self, now_ms: u64) -> GraceProbe {
        let mut guard = self.grace.lock().unwrap_or_else(|e| e.into_inner());
        let Some(g) = guard.as_ref() else {
            return GraceProbe::Closed;
        };
        if now_ms >= g.until_ms {
            let never_reasserted = g.expected.len();
            *guard = None;
            return GraceProbe::ClosedNow { never_reasserted };
        }
        GraceProbe::Open
    }

    /// Milliseconds left in the window at `now_ms` (`0` = closed). The
    /// caller composes this with [`Self::probe_grace`] (the shipped
    /// `grace_remaining_ms` shape).
    pub fn grace_remaining_ms(&self, now_ms: u64) -> u64 {
        self.grace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|g| g.until_ms.saturating_sub(now_ms))
            .unwrap_or(0)
    }

    /// Note a grace-window reclaim from `client`, closing the window
    /// early when every expected co-writer has re-asserted.
    pub fn note_reclaim(&self, client: &str) -> ReclaimNote {
        let mut guard = self.grace.lock().unwrap_or_else(|e| e.into_inner());
        let Some(g) = guard.as_mut() else {
            return ReclaimNote::NoWindow;
        };
        g.expected.remove(client);
        if g.expected.is_empty() {
            *guard = None;
            return ReclaimNote::ClosedEarly;
        }
        ReclaimNote::Waiting
    }

    /// Live client leases.
    pub fn clients_len(&self) -> usize {
        self.lock_clients().len()
    }

    /// Live grants.
    pub fn grants_len(&self) -> usize {
        self.lock_grants().len()
    }

    /// Map every live grant, ordered by grant id (the audit surface —
    /// bounded by live custody, never by grants ever issued).
    pub fn grants_snapshot_with<T>(&self, f: impl Fn(u64, &GrantEntry<L>) -> T) -> Vec<T> {
        let grants = self.lock_grants();
        let mut ids: Vec<u64> = grants.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| f(id, grants.get(&id).expect("id enumerated under this lock")))
            .collect()
    }
}
