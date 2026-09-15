//! Symmetric metadata program, PR 4 — the **SLOT LEASE core**
//! (`docs/design-symmetric-metadata.md` §5.1; KD-SYM-1/4/11/16/17).
//!
//! Extracted dependency-free so `loom-models/` can `#[path]`-include it
//! and model-check the exact shipped transitions (the `grant_table_core`
//! pattern: short-hold interior mutexes around O(1)/O(slots) table ops,
//! never held across an await; the durable halves — tree 0, the pages —
//! stay in `kv/backend.rs`). The main build never sets `cfg(loom)`.
//!
//! Three protocols live here, and `slot_lease_models` in
//! `loom-models/src/lib.rs` pins each:
//!
//! * **[`LeaseGate`]** — the node cache's THIRD gate state ("reader for
//!   structure, appender for my own leaves", design §1.3 fact 2 / §5.4.1):
//!   a dense atomic bitset of the slots this mount leases and the slots
//!   mid-handover. [`CachedNode::apply_locked`](crate::meta_backend::kv::node_cache::CachedNode::apply_locked)
//!   reads its verdict UNDER the node's write lock; the departing holder
//!   raises `Releasing` BEFORE it takes the node locks its flush needs.
//!   **The load-bearing order**: verdict-inside-the-lock. A committer that
//!   read the verdict outside the lock could land a record after the
//!   flush's snapshot — an acked record no ring holds and no page names.
//!   Weakening-verified (`a_commit_never_lands_on_a_releasing_slot`).
//! * **[`SlotLeaseTable`]** — the manager's RAM lease map (§5.1.4's state
//!   diagram): `Unleased → Leased → Offered → Releasing → Unleased`, one
//!   holder per slot, `g` strictly monotone per slot (incremented at
//!   every grant, never at a replay), every transition idempotent against
//!   the state it finds (KD-SYM-7 — the durable witness is tree 0; this
//!   table mirrors it). Weakening-verified: two racing acquires of one
//!   unleased slot yield exactly one holder and one `g` increment; an
//!   offer expiring under an accept never yields two holders.
//! * **[`DominanceWindow`]** — the holder's ops-over-a-common-window
//!   counters (§5.1.4): its own commits (`ops_h`) and each requester's
//!   ships (`ops_q`) on one slot over the same `T_idle` window, in a
//!   bounded per-requester table — never a per-node allocation per ship.
//!   The offer fires iff `ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor` for ONE
//!   requester; the idle arm is the same rule at `ops_h = 0`; aggregate
//!   shipping (a crowd below `N_floor` each) never triggers.
//!
//! The derivations of §5.1.2 / §5.1.4 are here too, tie-tested in
//! `tests/derivation_sweep_tests.rs`: [`mint_slots_derived`],
//! [`affinity_ceiling_bytes`], [`n_floor`], [`handover_cold_start_ns`].

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
    pub use loom::sync::{Mutex, MutexGuard};
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
    pub use std::sync::{Mutex, MutexGuard};
}

use std::collections::{BTreeMap, BTreeSet};
use sync::{fence, AtomicBool, AtomicU32, AtomicU64, Mutex, MutexGuard, Ordering};

/// A forest slot (`kv::record::ForestSlot`): 0 = the native slot, guest
/// slot `s` = `s + 1`.
pub type Slot = u32;
/// An appender id (`AppenderPage::appender_id`).
pub type AppenderId = u32;
/// A requester's identity on the ship path — the wire client's identity
/// hashed to one word (the S8 `client_id`'s xxh3, an appender's node
/// token). One word so the per-slot table stays fixed-size.
pub type RequesterId = u64;

/// The forest slot namespace: the native slot plus the u16 guest-slot
/// space (`kv::record::FOREST_SLOT_MAX + 1`; tie-tested there — this
/// module cannot name the codec).
pub const SLOT_NAMESPACE: usize = (u16::MAX as usize) + 2;
const GATE_WORDS: usize = SLOT_NAMESPACE.div_ceil(64);

/// The shipped mint-spread granularity: a volume's load divisible into
/// this many movable slices (`meta_backend::MINT_SPREAD`, tie-tested).
pub const MINT_SPREAD: u64 = 64;

/// Requesters one slot's dominance table keeps, LRU-evicted: **8** — a
/// dominating requester needs ≥ 2× the holder's ops and the offer names
/// ONE identity, so at most one requester can dominate at a time; with 8
/// equal requesters each holds ≤ 1/8 of the ships and none can reach the
/// holder's 2× unless the holder is idle, in which case `N_floor` decides
/// and the crowd is aggregate by construction (§5.1.4). An evicted
/// requester re-enters at 0 and pays at most one more window — never a
/// lost handover, since the dominating one is by definition the most
/// active and is never the LRU victim.
pub const REQUESTERS_PER_SLOT_MAX: usize = 8;

// ---------------------------------------------------------------------------
// The lease gate — the node cache's third gate state.
// ---------------------------------------------------------------------------

/// What the gate answers a leaf mutation of slot `s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitVerdict {
    /// The plane is not armed: PR 1–3's forest, every mount leases
    /// everything (the shipped-dark posture).
    Unarmed,
    /// This mount leases the slot and no handover is in flight.
    Allowed,
    /// Another appender leases the slot (or nobody does and this mount
    /// never acquired it) — the writer twin of `node_partition_refusals`.
    NotLeased,
    /// The slot is mid-handover (flush-then-transfer): a new commit would
    /// land after the flush's snapshot.
    Releasing,
}

/// A dense atomic bitset over the forest slot namespace: `leased` = the
/// slots this mount holds, `releasing` = the slots mid-handover. Read at
/// the ONE RAM-mutation choke point with two relaxed loads on an armed
/// forest mount and none on a flat one (the node's forest-slot stamp
/// short-circuits first).
///
/// **The door tokens** (review round 2, Issue 6 — the §5.4.1 door law):
/// `inflight[slot]` counts the commits that passed the door for the slot
/// and have not reached their terminal outcome. The door takes its
/// token BEFORE it reads `releasing` and a release raises `releasing`
/// BEFORE it reads the count — the Dekker pair, each side's two accesses
/// separated by a `SeqCst` fence (the `slot_gate_core` idiom; loom models
/// fences, and the pair is what `a_commit_that_passed_the_door_is_
/// drained_by_the_release` weakens) — so one of the two always sees the
/// other: a commit that passed the door is drained by the release before
/// its flush, and a commit that reads the bit parks at the door. The
/// belt in `apply_locked` is then reached by NO legal schedule
/// (`meta_kv_leaf_lease_refusals` keeps its must-stay-0 meaning); before
/// the tokens, every commit admitted between the door and the flush
/// failed EINVAL on a legal schedule.
///
/// **The structural verdict** (PR 4 review round 4, Issue 25): the SMO
/// task's own moves read [`Self::verdict_structural`], which admits a
/// slot mid-handover (the departing holder's flush) AND — on the
/// MANAGER — a slot nobody leases: an unleased slot tree's structure is
/// the manager's to maintain (KD-SYM-2/3 — its root lives in tree 0, its
/// SMOs journal in the manager's ring), while a slot ANOTHER appender
/// leases (`foreign`, installed from the manager's table) is refused
/// there as it is for content — the belt behind the sweep's own filter.
#[derive(Debug)]
pub struct LeaseGate {
    armed: AtomicBool,
    /// This mount holds the manager lease (every PR-4 arm; PR 12's
    /// non-manager appender arms without it and maintains only what it
    /// leases).
    manager: AtomicBool,
    leased: Box<[AtomicU64]>,
    releasing: Box<[AtomicU64]>,
    /// Slots another appender leases — the manager's table projected
    /// (`install_foreign`, rebuilt whole at every grant/release that
    /// moves a slot to or from a wire appender).
    foreign: Box<[AtomicU64]>,
    inflight: Box<[AtomicU32]>,
}

impl Default for LeaseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseGate {
    /// Unarmed, nothing leased, over the whole slot namespace.
    pub fn new() -> Self {
        Self::with_namespace(SLOT_NAMESPACE)
    }

    /// The gate over the first `slots` slots (the loom models size it to
    /// their handful; the product uses [`Self::new`]).
    pub fn with_namespace(slots: usize) -> Self {
        Self {
            armed: AtomicBool::new(false),
            manager: AtomicBool::new(false),
            leased: (0..GATE_WORDS).map(|_| AtomicU64::new(0)).collect(),
            releasing: (0..GATE_WORDS).map(|_| AtomicU64::new(0)).collect(),
            foreign: (0..GATE_WORDS).map(|_| AtomicU64::new(0)).collect(),
            inflight: (0..slots).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    #[inline]
    fn index(slot: Slot) -> (usize, u64) {
        ((slot as usize) / 64, 1u64 << (slot % 64))
    }

    /// The door of `slot` (§5.4.1): take a token, then read `releasing`
    /// — `true` = the commit may proceed and holds the token until
    /// [`Self::leave`]; `false` = the slot is mid-handover, the token is
    /// returned and the caller parks until the handover completes.
    #[inline]
    pub fn enter(&self, slot: Slot) -> bool {
        let (w, bit) = Self::index(slot);
        self.inflight[slot as usize].fetch_add(1, Ordering::AcqRel);
        // Dekker fence #1: publish the token before probing the bit
        // (pairs with `begin_release`'s).
        fence(Ordering::SeqCst);
        if self.releasing[w].load(Ordering::Acquire) & bit != 0 {
            self.inflight[slot as usize].fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Return the door token of `slot` (the commit's terminal outcome).
    /// Answers the tokens still out — `0` wakes a release parked on the
    /// drain. A leave with no token out (a `DoorPass` dropped twice would
    /// be the bug) answers 0 and leaves the count at 0 rather than
    /// wrapping it — a wrapped count is a permanently un-drainable slot
    /// (review round 3, Issue 23).
    #[inline]
    pub fn leave(&self, slot: Slot) -> u32 {
        let word = &self.inflight[slot as usize];
        let mut cur = word.load(Ordering::Acquire);
        loop {
            if cur == 0 {
                return 0;
            }
            match word.compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return cur - 1,
                Err(seen) => cur = seen,
            }
        }
    }

    /// The door tokens out on `slot` — the release's half of the Dekker
    /// pair, read after [`Self::begin_release`] behind the pairing fence.
    #[inline]
    pub fn inflight(&self, slot: Slot) -> u32 {
        fence(Ordering::SeqCst);
        self.inflight[slot as usize].load(Ordering::Acquire)
    }

    /// Arm the gate: from here on a leaf mutation of a slot this mount
    /// does not lease is refused. Grants published before the arm stay.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Whether the gate is armed.
    #[inline]
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// This mount is the volume's MANAGER (KD-SYM-3): the structure of a
    /// slot nobody leases is its to maintain. Set at the manager's arm;
    /// PR 12's non-manager appender never sets it.
    pub fn arm_as_manager(&self) {
        self.manager.store(true, Ordering::Release);
    }

    /// Whether this mount maintains the unleased slot trees.
    #[inline]
    pub fn is_manager(&self) -> bool {
        self.manager.load(Ordering::Relaxed)
    }

    /// Install the slots ANOTHER appender leases, whole — each word is
    /// stored atomically, so a reader between two words sees every slot
    /// in either its old or its new state, never a torn word.
    pub fn install_foreign(&self, slots: &[Slot]) {
        let mut words = vec![0u64; GATE_WORDS];
        for s in slots {
            let (w, bit) = Self::index(*s);
            if let Some(word) = words.get_mut(w) {
                *word |= bit;
            }
        }
        for (w, word) in words.into_iter().enumerate() {
            self.foreign[w].store(word, Ordering::Release);
        }
    }

    /// Whether another appender leases `slot`.
    #[inline]
    pub fn is_foreign(&self, slot: Slot) -> bool {
        let (w, bit) = Self::index(slot);
        self.foreign[w].load(Ordering::Acquire) & bit != 0
    }

    /// Mark `slot` as another appender's AHEAD of the durable grant (the
    /// RAM table already names the holder — review round 5, Issue 28):
    /// a structural pass that starts from here skips it. Rolled back with
    /// the table by [`Self::clear_foreign`] when the grant's write fails;
    /// the next whole-set [`Self::install_foreign`] is idempotent over it.
    pub fn mark_foreign(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.foreign[w].fetch_or(bit, Ordering::Release);
    }

    /// Withdraw a [`Self::mark_foreign`] (a failed grant, a release).
    pub fn clear_foreign(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.foreign[w].fetch_and(!bit, Ordering::Release);
    }

    /// Publish a lease of `slot` to the commit path.
    pub fn grant(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.leased[w].fetch_or(bit, Ordering::Release);
    }

    /// Withdraw the lease of `slot` (after a release or a handover).
    pub fn revoke(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.leased[w].fetch_and(!bit, Ordering::Release);
        self.releasing[w].fetch_and(!bit, Ordering::Release);
    }

    /// The departing holder's FIRST act of a handover: from this store
    /// on no new commit passes the door of the slot. `SeqCst` — the
    /// store is the release's half of the door's Dekker pair (it must be
    /// ordered before the release reads [`Self::inflight`]), and it must
    /// be ordered before the node locks the flush takes, so a committer
    /// inside its own node lock observes it (the belt).
    pub fn begin_release(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.releasing[w].fetch_or(bit, Ordering::SeqCst);
        // Dekker fence #2: the bit is published before any read of the
        // door's count (pairs with `enter`'s).
        fence(Ordering::SeqCst);
    }

    /// A handover that did not complete (the manager refused the release,
    /// the offer expired): the slot accepts commits again.
    pub fn end_release(&self, slot: Slot) {
        let (w, bit) = Self::index(slot);
        self.releasing[w].fetch_and(!bit, Ordering::Release);
    }

    /// Whether this mount leases `slot` (a lease mid-release counts).
    #[inline]
    pub fn is_leased(&self, slot: Slot) -> bool {
        let (w, bit) = Self::index(slot);
        self.leased[w].load(Ordering::Acquire) & bit != 0
    }

    /// Whether `slot` is mid-handover.
    #[inline]
    pub fn is_releasing(&self, slot: Slot) -> bool {
        let (w, bit) = Self::index(slot);
        self.releasing[w].load(Ordering::Acquire) & bit != 0
    }

    /// The verdict on a leaf mutation of `slot` — read UNDER the node's
    /// write lock (see the module docs for why).
    #[inline]
    pub fn verdict(&self, slot: Slot) -> CommitVerdict {
        if !self.is_armed() {
            return CommitVerdict::Unarmed;
        }
        let (w, bit) = Self::index(slot);
        if self.leased[w].load(Ordering::Acquire) & bit == 0 {
            return CommitVerdict::NotLeased;
        }
        if self.releasing[w].load(Ordering::Acquire) & bit != 0 {
            return CommitVerdict::Releasing;
        }
        CommitVerdict::Allowed
    }

    /// The verdict on the SMO task's OWN moves (an SMO's leftover overlay
    /// into its successor, a parent's pointer flips) — see the type docs:
    /// a slot this mount leases is admitted whether or not a handover is
    /// in flight (flush-then-transfer IS the departing holder flushing);
    /// a slot nobody leases is admitted on the MANAGER (its structure to
    /// maintain — KD-SYM-2/3) and refused elsewhere; a slot another
    /// appender leases is refused — the sweep filters it first, so a
    /// refusal here is the belt (`meta_kv_leaf_lease_refusals`).
    #[inline]
    pub fn verdict_structural(&self, slot: Slot) -> CommitVerdict {
        if !self.is_armed() {
            return CommitVerdict::Unarmed;
        }
        let (w, bit) = Self::index(slot);
        if self.leased[w].load(Ordering::Acquire) & bit != 0 {
            return CommitVerdict::Allowed;
        }
        if self.foreign[w].load(Ordering::Acquire) & bit != 0 || !self.is_manager() {
            return CommitVerdict::NotLeased;
        }
        CommitVerdict::Allowed
    }

    /// Every leased slot, ascending.
    pub fn leased_slots(&self) -> Vec<Slot> {
        let mut out = Vec::new();
        for (w, word) in self.leased.iter().enumerate() {
            let mut v = word.load(Ordering::Acquire);
            while v != 0 {
                let bit = v.trailing_zeros();
                out.push((w * 64 + bit as usize) as Slot);
                v &= v - 1;
            }
        }
        out
    }

    /// Leased slots, counted.
    pub fn leased_count(&self) -> usize {
        self.leased
            .iter()
            .map(|w| w.load(Ordering::Acquire).count_ones() as usize)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// The manager's lease table — §5.1.4's state diagram, one holder per slot.
// ---------------------------------------------------------------------------

/// One slot's lease state at the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Unleased,
    Leased,
    /// Offered to ONE requester until `offer_expires_ns`.
    Offered,
    /// Flush-then-transfer in flight at the holder.
    Releasing,
}

/// The slot tree's durable words a lease carries (§5.1.4 "four words
/// move: root, cursor, `g`, tails" — `tails` ride the durable record only)
/// plus the seq-space floor (§5.8.2, review round 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotWords {
    /// The tree root `(addr, seq)`; `(0, 0)` = never minted.
    pub root: (u64, u64),
    /// The slot's ino cursor (§5.1.8).
    pub cursor: u64,
    /// The tree's extent count (the affinity cap's durable input).
    pub extents: u32,
    /// **The record-seq floor**: every record of the slot's keys carries
    /// a seq strictly below it (the departing ring's stamp frontier at the
    /// release, `max`ed over every release); the next lessee's ring is
    /// raised above it at the grant, so the leaf fold's per-key LWW is
    /// monotone across rings.
    pub seq_floor: u64,
}

/// One slot's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotLease {
    pub state: LeaseState,
    /// Valid unless `Unleased`.
    pub holder: AppenderId,
    /// The lease generation: incremented by the manager at every grant.
    pub g: u32,
    /// The requester an `Offered` slot names.
    pub offered_to: AppenderId,
    /// When the offer lapses (CLOCK_MONOTONIC ns).
    pub offer_expires_ns: u64,
    /// The manager's seq at the last release — `prefer: unleased-then-
    /// idle`'s ordering key (0 = never written).
    pub last_written: u64,
    /// The words the last release recorded (what a grant hands over).
    pub words: SlotWords,
    /// RAM-only: the lease was granted by a ROTOR ask (`AcquireSlots {
    /// want }`), so it counts against the holder's `2 × M` rotor cap
    /// (§5.1.2; review round 2, Issue 14 — the cap counts rotor slots,
    /// never every non-native lease; the page budget governs holdings).
    /// Cleared at the release; a re-adoption at open reads `false` from
    /// tree 0 and the arm re-marks the manager's rebuilt rotor
    /// ([`SlotLeaseTable::mark_rotor`]) so the cap counts it.
    pub rotor: bool,
}

impl SlotLease {
    /// An unleased slot with history.
    pub fn unleased(g: u32, last_written: u64, words: SlotWords) -> Self {
        Self {
            state: LeaseState::Unleased,
            holder: 0,
            g,
            offered_to: 0,
            offer_expires_ns: 0,
            last_written,
            words,
            rotor: false,
        }
    }

    /// A leased slot (tree 0's `Leased{appender_id, g}`).
    pub fn leased(holder: AppenderId, g: u32) -> Self {
        Self::leased_with(holder, g, SlotWords::default())
    }

    /// A leased slot carrying the grant-time words the record holds (the
    /// seq floor among them — the one word a re-adoption must re-apply).
    pub fn leased_with(holder: AppenderId, g: u32, words: SlotWords) -> Self {
        Self {
            state: LeaseState::Leased,
            holder,
            g,
            offered_to: 0,
            offer_expires_ns: 0,
            last_written: 0,
            words,
            rotor: false,
        }
    }
}

/// [`SlotLeaseTable::acquire`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// Granted now: `g` incremented, the slot's last recorded words.
    Granted { g: u32, words: SlotWords },
    /// The requester already holds it (KD-SYM-7's replay).
    Already { g: u32 },
    /// Another appender holds it — ship to the holder (the metanode arm).
    Refused { holder: AppenderId, g: u32 },
    /// The holder offered it to this requester: recall the holder
    /// (flush-then-transfer), then acquire again.
    Recall { holder: AppenderId, g: u32 },
}

/// [`SlotLeaseTable::release`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Released,
    /// Already unleased at that generation (a replay).
    Already,
    /// The caller is not the holder, or names a stale `g`.
    Refused {
        holder: AppenderId,
        g: u32,
    },
}

/// [`SlotLeaseTable::resolve`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Unleased { g: u32 },
    Holder { holder: AppenderId, g: u32 },
}

/// Why an offer / a release-begin was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRefusal {
    NotHolder {
        holder: AppenderId,
    },
    Unleased,
    /// A transition already in flight (`Offered` / `Releasing`).
    Busy {
        state: LeaseState,
    },
}

/// The table's interior: the per-slot entries plus the two indexes the
/// renewal carriage and the manager read per HOLDER — `held[holder]` =
/// every slot in a leased state under it, `offered[to]` = every slot
/// offered to it (review round 2, Issue 16: `held_by` / `carriage_for`
/// are O(held) off these, never a scan of the table — KD-FG-4's
/// no-scan-in-renew law; `snapshot()` stays the stats face's).
#[derive(Debug, Default)]
struct TableInner {
    slots: BTreeMap<Slot, SlotLease>,
    held: BTreeMap<AppenderId, BTreeSet<Slot>>,
    offered: BTreeMap<AppenderId, BTreeSet<Slot>>,
}

impl TableInner {
    fn index_hold(&mut self, slot: Slot, holder: AppenderId) {
        self.held.entry(holder).or_default().insert(slot);
    }

    fn unindex_hold(&mut self, slot: Slot, holder: AppenderId) {
        if let Some(set) = self.held.get_mut(&holder) {
            set.remove(&slot);
            if set.is_empty() {
                self.held.remove(&holder);
            }
        }
    }

    fn index_offer(&mut self, slot: Slot, to: AppenderId) {
        self.offered.entry(to).or_default().insert(slot);
    }

    fn unindex_offer(&mut self, slot: Slot, to: AppenderId) {
        if let Some(set) = self.offered.get_mut(&to) {
            set.remove(&slot);
            if set.is_empty() {
                self.offered.remove(&to);
            }
        }
    }

    /// Withdraw a standing offer of `slot` (the entry's `offered_to`
    /// cleared with the index).
    fn clear_offer(&mut self, slot: Slot) {
        if let Some(e) = self.slots.get(&slot) {
            let to = e.offered_to;
            if to != 0 {
                self.unindex_offer(slot, to);
            }
        }
        if let Some(e) = self.slots.get_mut(&slot) {
            e.offered_to = 0;
            e.offer_expires_ns = 0;
        }
    }
}

/// The manager's RAM lease map — mirrors tree 0 (loaded at open, written
/// through by every grant/release).
#[derive(Debug, Default)]
pub struct SlotLeaseTable {
    inner: Mutex<TableInner>,
    /// Offers made (`slot_offers`) / expired (`slot_offers_expired`).
    offers: AtomicU64,
    offers_expired: AtomicU64,
}

impl SlotLeaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, TableInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Install a slot's state as tree 0 records it (open / epoch reload).
    pub fn load(&self, slot: Slot, lease: SlotLease) {
        let mut m = self.lock();
        if let Some(prev) = m.slots.get(&slot).copied() {
            if prev.state != LeaseState::Unleased {
                m.unindex_hold(slot, prev.holder);
            }
            if prev.offered_to != 0 {
                m.unindex_offer(slot, prev.offered_to);
            }
        }
        if lease.state != LeaseState::Unleased {
            m.index_hold(slot, lease.holder);
        }
        if lease.state == LeaseState::Offered && lease.offered_to != 0 {
            m.index_offer(slot, lease.offered_to);
        }
        m.slots.insert(slot, lease);
    }

    /// The entry of `slot` (`None` = never leased, no history).
    pub fn get(&self, slot: Slot) -> Option<SlotLease> {
        self.lock().slots.get(&slot).copied()
    }

    /// Every entry, slot-ascending — the stats face and the open's
    /// reconciliation; never the renewal path (Issue 16).
    pub fn snapshot(&self) -> Vec<(Slot, SlotLease)> {
        self.lock().slots.iter().map(|(s, l)| (*s, *l)).collect()
    }

    /// `AcquireSlot` (§5.1.2 / §5.3.5): grant an unleased slot (`g += 1`),
    /// answer a holder's replay `Already`, refuse a foreign holder with
    /// its identity, and turn an offer to this requester into a recall.
    /// A slot mid-release is refused with its holder (retry after the
    /// release lands). The first grant of a never-leased slot mints `g = 1`.
    /// `rotor` marks a grant made by a rotor ask (the `2 × M` cap's unit).
    pub fn acquire(
        &self,
        slot: Slot,
        requester: AppenderId,
        now_ns: u64,
        rotor: bool,
    ) -> AcquireOutcome {
        let mut m = self.lock();
        let entry = m
            .slots
            .entry(slot)
            .or_insert_with(|| SlotLease::unleased(0, 0, SlotWords::default()));
        match entry.state {
            LeaseState::Unleased => {
                entry.g = entry.g.wrapping_add(1);
                entry.state = LeaseState::Leased;
                entry.holder = requester;
                entry.offered_to = 0;
                entry.offer_expires_ns = 0;
                entry.rotor = rotor;
                let out = AcquireOutcome::Granted {
                    g: entry.g,
                    words: entry.words,
                };
                m.index_hold(slot, requester);
                out
            }
            LeaseState::Leased | LeaseState::Offered | LeaseState::Releasing
                if entry.holder == requester =>
            {
                AcquireOutcome::Already { g: entry.g }
            }
            LeaseState::Offered
                if entry.offered_to == requester && now_ns < entry.offer_expires_ns =>
            {
                AcquireOutcome::Recall {
                    holder: entry.holder,
                    g: entry.g,
                }
            }
            LeaseState::Offered if now_ns >= entry.offer_expires_ns => {
                // Lapsed under the acquire: the holder is unchanged.
                entry.state = LeaseState::Leased;
                let out = AcquireOutcome::Refused {
                    holder: entry.holder,
                    g: entry.g,
                };
                m.clear_offer(slot);
                self.offers_expired.fetch_add(1, Ordering::Relaxed);
                out
            }
            LeaseState::Leased | LeaseState::Offered | LeaseState::Releasing => {
                AcquireOutcome::Refused {
                    holder: entry.holder,
                    g: entry.g,
                }
            }
        }
    }

    /// `OfferSlot`: the HOLDER offers `slot` to `to` until `expires_ns`
    /// (one renewal beat). Refused unless the caller holds the slot in
    /// the plain `Leased` state.
    pub fn offer(
        &self,
        slot: Slot,
        holder: AppenderId,
        to: AppenderId,
        expires_ns: u64,
    ) -> Result<(), LeaseRefusal> {
        let mut m = self.lock();
        let Some(entry) = m.slots.get_mut(&slot) else {
            return Err(LeaseRefusal::Unleased);
        };
        match entry.state {
            LeaseState::Unleased => Err(LeaseRefusal::Unleased),
            _ if entry.holder != holder => Err(LeaseRefusal::NotHolder {
                holder: entry.holder,
            }),
            LeaseState::Leased => {
                entry.state = LeaseState::Offered;
                entry.offered_to = to;
                entry.offer_expires_ns = expires_ns;
                m.index_offer(slot, to);
                self.offers.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            state => Err(LeaseRefusal::Busy { state }),
        }
    }

    /// Lapse every offer past `now_ns`: the holder is unchanged. Returns
    /// the slots whose offers expired. O(offers) off the offer index.
    pub fn expire_offers(&self, now_ns: u64) -> Vec<Slot> {
        let mut m = self.lock();
        let offered: Vec<Slot> = m.offered.values().flatten().copied().collect();
        let mut out = Vec::new();
        for slot in offered {
            let lapsed = m.slots.get(&slot).is_some_and(|entry| {
                entry.state == LeaseState::Offered && now_ns >= entry.offer_expires_ns
            });
            if !lapsed {
                continue;
            }
            if let Some(entry) = m.slots.get_mut(&slot) {
                entry.state = LeaseState::Leased;
            }
            m.clear_offer(slot);
            out.push(slot);
        }
        out.sort_unstable();
        self.offers_expired
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    /// The manager's `RecallForOffer` / the holder's own release: mark
    /// `slot` `Releasing` (flush-then-transfer begins). Answers the `g`
    /// the release must present. Idempotent on a slot already releasing
    /// under this holder.
    pub fn begin_release(&self, slot: Slot, holder: AppenderId) -> Result<u32, LeaseRefusal> {
        let mut m = self.lock();
        let Some(entry) = m.slots.get_mut(&slot) else {
            return Err(LeaseRefusal::Unleased);
        };
        match entry.state {
            LeaseState::Unleased => Err(LeaseRefusal::Unleased),
            _ if entry.holder != holder => Err(LeaseRefusal::NotHolder {
                holder: entry.holder,
            }),
            LeaseState::Leased | LeaseState::Offered | LeaseState::Releasing => {
                entry.state = LeaseState::Releasing;
                Ok(entry.g)
            }
        }
    }

    /// A release that will not complete (the manager refused it, the
    /// flush failed): back to `Leased`, holder unchanged, the offer (if
    /// the release was an accepted offer's recall) withdrawn.
    pub fn abort_release(&self, slot: Slot, holder: AppenderId) {
        let mut m = self.lock();
        let aborting = m
            .slots
            .get(&slot)
            .is_some_and(|entry| entry.holder == holder && entry.state == LeaseState::Releasing);
        if !aborting {
            return;
        }
        if let Some(entry) = m.slots.get_mut(&slot) {
            entry.state = LeaseState::Leased;
        }
        m.clear_offer(slot);
    }

    /// `ReleaseSlot` (§5.1.4 / §5.3.5): the holder presents its `g` and
    /// the slot's final words; the slot goes `Unleased` with them
    /// (`last_written = now_seq`). A slot already unleased at that `g`
    /// WITH those words answers `Already` (the replay — "tree 0 already
    /// `Unleased` with that cursor"); a caller that is not the holder, or
    /// presents a stale `g` or other words, is refused with the truth.
    pub fn release(
        &self,
        slot: Slot,
        holder: AppenderId,
        g: u32,
        words: SlotWords,
        now_seq: u64,
    ) -> ReleaseOutcome {
        let mut m = self.lock();
        let Some(entry) = m.slots.get(&slot).copied() else {
            return ReleaseOutcome::Refused { holder: 0, g: 0 };
        };
        match Self::release_verdict(&entry, holder, g, words) {
            ReleaseOutcome::Released => {}
            other => return other,
        }
        m.unindex_hold(slot, entry.holder);
        m.clear_offer(slot);
        if let Some(entry) = m.slots.get_mut(&slot) {
            entry.state = LeaseState::Unleased;
            entry.holder = 0;
            entry.last_written = now_seq;
            entry.words = words;
            entry.rotor = false;
        }
        ReleaseOutcome::Released
    }

    /// [`Self::release`]'s verdict WITHOUT the transition — what the
    /// manager consults BEFORE its durable write (review round 2, Issue
    /// 3: the durable act must never precede the verdict; a stale release
    /// that proceeded to tree 0 rolled the record back to older words).
    pub fn check_release(
        &self,
        slot: Slot,
        holder: AppenderId,
        g: u32,
        words: SlotWords,
    ) -> ReleaseOutcome {
        let m = self.lock();
        match m.slots.get(&slot) {
            None => ReleaseOutcome::Refused { holder: 0, g: 0 },
            Some(entry) => Self::release_verdict(entry, holder, g, words),
        }
    }

    /// The release law over one entry: `Unleased` at the same `g` with
    /// the same words = the replay (`Already`); `Unleased` otherwise, a
    /// foreign holder or a stale `g` = refused with the truth; else the
    /// release lands.
    fn release_verdict(
        entry: &SlotLease,
        holder: AppenderId,
        g: u32,
        words: SlotWords,
    ) -> ReleaseOutcome {
        match entry.state {
            LeaseState::Unleased if entry.g == g && entry.words == words => ReleaseOutcome::Already,
            LeaseState::Unleased => ReleaseOutcome::Refused {
                holder: 0,
                g: entry.g,
            },
            _ if entry.holder != holder || entry.g != g => ReleaseOutcome::Refused {
                holder: entry.holder,
                g: entry.g,
            },
            _ => ReleaseOutcome::Released,
        }
    }

    /// `ResolveSlot` (§5.1.6): who holds `slot`.
    pub fn resolve(&self, slot: Slot) -> Resolved {
        match self.lock().slots.get(&slot) {
            None => Resolved::Unleased { g: 0 },
            Some(e) if e.state == LeaseState::Unleased => Resolved::Unleased { g: e.g },
            Some(e) => Resolved::Holder {
                holder: e.holder,
                g: e.g,
            },
        }
    }

    /// Every slot `holder` leases (any leased state), ascending — O(held)
    /// off the holder index.
    pub fn held_by(&self, holder: AppenderId) -> Vec<Slot> {
        self.lock()
            .held
            .get(&holder)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Mark `slot` — held by `holder` — as a ROTOR grant: the arm's re-
    /// adoption rebuilds the manager's rotor from tree 0's leases, whose
    /// records do not distinguish a rotor grant from a handover's, so the
    /// `2 × M` cap would otherwise count 0 re-adopted rotor slots and
    /// admit up to `M` more than the law (review round 3, Issue 23).
    /// A slot another holder leases is left alone.
    pub fn mark_rotor(&self, slot: Slot, holder: AppenderId) {
        let mut m = self.lock();
        if let Some(e) = m
            .slots
            .get_mut(&slot)
            .filter(|e| e.state != LeaseState::Unleased && e.holder == holder)
        {
            e.rotor = true;
        }
    }

    /// The slots `holder` leases by a ROTOR grant (the `2 × M` cap's
    /// count, §5.1.2) — O(held).
    pub fn rotor_held_by(&self, holder: AppenderId) -> u64 {
        let m = self.lock();
        m.held.get(&holder).map_or(0, |set| {
            set.iter()
                .filter(|s| m.slots.get(s).is_some_and(|e| e.rotor))
                .count() as u64
        })
    }

    /// Every leased slot with its holder and `g`, ascending — the holder
    /// cache's refresh input, O(leased) off the holder index.
    pub fn leased_entries(&self) -> Vec<(Slot, AppenderId, u32)> {
        let m = self.lock();
        let mut out: Vec<(Slot, AppenderId, u32)> = m
            .held
            .iter()
            .flat_map(|(holder, set)| {
                set.iter()
                    .filter_map(|s| m.slots.get(s).map(|e| (*s, *holder, e.g)))
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// Every leased slot whose holder is NOT one of `holders` (the S4
    /// plane's foreign set), ascending — O(leased).
    pub fn held_outside(&self, holders: &BTreeSet<AppenderId>) -> Vec<Slot> {
        let m = self.lock();
        let mut out: Vec<Slot> = m
            .held
            .iter()
            .filter(|(h, _)| !holders.contains(h))
            .flat_map(|(_, set)| set.iter().copied())
            .collect();
        out.sort_unstable();
        out
    }

    /// Every slot offered to `to` with its `g`, ascending — O(offered)
    /// off the offer index.
    pub fn offered_to(&self, to: AppenderId) -> Vec<(Slot, u32)> {
        let m = self.lock();
        m.offered.get(&to).map_or_else(Vec::new, |set| {
            set.iter()
                .filter_map(|s| {
                    m.slots
                        .get(s)
                        .filter(|e| e.state == LeaseState::Offered)
                        .map(|e| (*s, e.g))
                })
                .collect()
        })
    }

    /// `prefer: unleased-then-idle` (§5.1.2): up to `want` unleased
    /// slots out of `candidates`, never-leased slots first (`last_written
    /// == 0`), then the least-recently-written, ties by slot index.
    pub fn pick_unleased(&self, want: usize, candidates: impl Iterator<Item = Slot>) -> Vec<Slot> {
        let m = self.lock();
        let mut ranked: Vec<(u64, Slot)> = candidates
            .filter_map(|s| match m.slots.get(&s) {
                None => Some((0, s)),
                Some(e) if e.state == LeaseState::Unleased => Some((e.last_written, s)),
                Some(_) => None,
            })
            .collect();
        ranked.sort_unstable();
        ranked.into_iter().take(want).map(|(_, s)| s).collect()
    }

    /// `slot_offers` / `slot_offers_expired`.
    pub fn offer_counts(&self) -> (u64, u64) {
        (
            self.offers.load(Ordering::Relaxed),
            self.offers_expired.load(Ordering::Relaxed),
        )
    }
}

// ---------------------------------------------------------------------------
// The dominance window — ops over one common `T_idle` window per slot.
// ---------------------------------------------------------------------------

/// Two half-window buckets: `current + previous` counts the ops of the
/// last `T_idle/2 .. T_idle` — the SAME window for the holder and every
/// requester of the slot, which is what §5.1.4 needs (a comparison across
/// unequal windows is the sustain-clause bug round 3 retired).
#[derive(Debug, Clone, Copy, Default)]
struct Buckets {
    cur: u64,
    prev: u64,
}

impl Buckets {
    fn total(&self) -> u64 {
        self.cur + self.prev
    }
    fn rotate(&mut self, steps: u64) {
        match steps {
            0 => {}
            1 => {
                self.prev = self.cur;
                self.cur = 0;
            }
            _ => *self = Self::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequesterOps {
    id: RequesterId,
    ops: Buckets,
    /// The half-window epoch of the last ship — the LRU key.
    touched: u64,
}

#[derive(Debug, Clone, Default)]
struct SlotOps {
    /// The half-window epoch the buckets are aligned to.
    epoch: u64,
    requesters: Vec<RequesterOps>,
}

impl SlotOps {
    fn align(&mut self, epoch: u64) {
        if epoch > self.epoch {
            let steps = epoch - self.epoch;
            for r in &mut self.requesters {
                r.ops.rotate(steps);
            }
            self.epoch = epoch;
        }
    }
}

/// The HOLDER's own ops on one slot over the common window — lock-free,
/// because it is bumped once per commit on the lessee's hot path (a
/// mutex there is the solo re-gate's regression). Two half-window
/// buckets rotated by a CAS on the epoch word; a lost rotation race
/// under-counts by one commit, never over-counts (the comparison the
/// count feeds — `ops_q ≥ 2 × ops_h` — errs toward serving, never
/// toward a handover).
#[derive(Debug, Default)]
pub struct HolderOps {
    epoch: AtomicU64,
    cur: AtomicU64,
    prev: AtomicU64,
}

impl HolderOps {
    pub fn new() -> Self {
        Self::default()
    }

    /// One commit on the slot at `now_ns`.
    pub fn note(&self, now_ns: u64, t_idle_ns: u64) {
        self.align(DominanceWindow::epoch(now_ns, t_idle_ns));
        self.cur.fetch_add(1, Ordering::Relaxed);
    }

    fn align(&self, epoch: u64) {
        let seen = self.epoch.load(Ordering::Acquire);
        if epoch <= seen {
            return;
        }
        if self
            .epoch
            .compare_exchange(seen, epoch, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let cur = self.cur.swap(0, Ordering::AcqRel);
            self.prev
                .store(if epoch == seen + 1 { cur } else { 0 }, Ordering::Release);
        }
    }

    /// The ops over the common window at `now_ns`.
    pub fn total(&self, now_ns: u64, t_idle_ns: u64) -> u64 {
        self.align(DominanceWindow::epoch(now_ns, t_idle_ns));
        self.cur.load(Ordering::Acquire) + self.prev.load(Ordering::Acquire)
    }
}

/// The verdict of one served ship (§5.1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShipVerdict {
    /// Serve it; the holder keeps the slot.
    Serve,
    /// Offer the slot to this requester — the IDLE arm (`ops_h == 0`).
    OfferIdle { to: RequesterId },
    /// Offer the slot — the DOMINATED arm (`ops_q ≥ 2 × ops_h`).
    OfferDominated { to: RequesterId },
}

/// The holder-side dominance counters of every slot it leases.
#[derive(Debug, Default)]
pub struct DominanceWindow {
    slots: Mutex<BTreeMap<Slot, SlotOps>>,
}

impl DominanceWindow {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<Slot, SlotOps>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The half-window epoch of `now_ns` under a `t_idle_ns` window.
    #[inline]
    pub fn epoch(now_ns: u64, t_idle_ns: u64) -> u64 {
        now_ns / (t_idle_ns / 2).max(1)
    }

    /// The holder served one ship of `requester` on `slot`: count it and
    /// evaluate §5.1.4 — `ops_q ≥ 2 × ops_h ∧ ops_q ≥ n_floor` over the
    /// common window (`ops_h` = the holder's [`HolderOps::total`] at the
    /// same instant). The per-requester table is bounded by
    /// [`REQUESTERS_PER_SLOT_MAX`], LRU-evicted.
    pub fn note_ship(
        &self,
        slot: Slot,
        requester: RequesterId,
        ops_h: u64,
        now_ns: u64,
        t_idle_ns: u64,
        n_floor: u64,
    ) -> ShipVerdict {
        let mut m = self.lock();
        let e = m.entry(slot).or_default();
        let epoch = Self::epoch(now_ns, t_idle_ns);
        e.align(epoch);
        let idx = match e.requesters.iter().position(|r| r.id == requester) {
            Some(i) => i,
            None => {
                if e.requesters.len() >= REQUESTERS_PER_SLOT_MAX {
                    // Evict the least recently touched (ties: the fewest ops).
                    let victim = e
                        .requesters
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, r)| (r.touched, r.ops.total()))
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    e.requesters.swap_remove(victim);
                }
                e.requesters.push(RequesterOps {
                    id: requester,
                    ops: Buckets::default(),
                    touched: epoch,
                });
                e.requesters.len() - 1
            }
        };
        let r = &mut e.requesters[idx];
        r.ops.cur += 1;
        r.touched = epoch;
        let ops_q = r.ops.total();
        if ops_q >= n_floor && ops_q >= 2 * ops_h {
            if ops_h == 0 {
                ShipVerdict::OfferIdle { to: requester }
            } else {
                ShipVerdict::OfferDominated { to: requester }
            }
        } else {
            ShipVerdict::Serve
        }
    }

    /// Forget `slot` (released / handed over).
    pub fn forget(&self, slot: Slot) {
        self.lock().remove(&slot);
    }
}

// ---------------------------------------------------------------------------
// Derivations (§5.1.2, §5.1.4) — tie-tested in `derivation_sweep_tests`.
// ---------------------------------------------------------------------------

/// `M = clamp(W / (2 × writers_known), 1, MINT_SPREAD)` — the rotor size
/// at join (§5.1.2): `2 ×` keeps half the width unleased for inheritance
/// and churn, `MINT_SPREAD` is the shipped granularity ceiling.
pub fn mint_slots_derived(width: u64, writers_known: u64) -> u64 {
    (width / (2 * writers_known.max(1))).clamp(1, MINT_SPREAD)
}

/// `A_max(t) = max(used_leaf_bytes / MINT_SPREAD, node_size)` — the
/// load-relative affinity ceiling (KD-SYM-16): no tree holds more than
/// 1/64 of the volume's CURRENT leaf bytes, never less than one extent.
pub fn affinity_ceiling_bytes(used_leaf_bytes: u64, node_size: u64) -> u64 {
    (used_leaf_bytes / MINT_SPREAD).max(node_size)
}

/// `N_floor = max(2, ceil(ewma_handover_ns / ewma_ship_ns))` — a handover
/// must pay for itself in ships saved; the floor of 2 means a single
/// touch never moves anything (§5.1.4).
pub fn n_floor(ewma_handover_ns: u64, ewma_ship_ns: u64) -> u64 {
    ewma_handover_ns.div_ceil(ewma_ship_ns.max(1)).max(2)
}

/// The cold-start handover cost before this mount has a
/// `slot_handover_phase_ns` sample: what a handover IS — four barriers
/// (`uring_fs_write_phase_ns`'s barrier EWMA) and three ship round
/// trips (`meta_ship_phase_ns.rtt`'s EWMA) (§5.1.4).
pub fn handover_cold_start_ns(ewma_barrier_ns: u64, ewma_ship_rtt_ns: u64) -> u64 {
    ewma_barrier_ns
        .saturating_mul(4)
        .saturating_add(ewma_ship_rtt_ns.saturating_mul(3))
}

/// The mint decision (KD-SYM-11 / KD-SYM-16, §5.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintChoice {
    /// The parent's slot: leased here, not native, below the cap.
    Affinity(Slot),
    /// The rotor slot with the most headroom under the cap (ties by the
    /// round-robin cursor).
    Rotor(Slot),
    /// Every rotor tree is STRICTLY over the cap by ≥ 1 extent: ask the
    /// manager for one more rotor slot (up to `2 × M`).
    Overflow,
    /// Past `2 × M` with every rotor over the cap: the smallest rotor tree.
    Smallest(Slot),
}

/// The parent's standing at the mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentStanding {
    pub slot: Slot,
    /// This mount leases the parent's slot.
    pub leased: bool,
    /// The parent's slot is a native slot (control records + `/`'s
    /// dentries only).
    pub native: bool,
    /// The parent's slot tree's bytes.
    pub tree_bytes: u64,
}

/// Decide where a child mints. `rotors` = `(slot, tree_bytes)` of this
/// mount's rotor slots; `rr` = the round-robin cursor for ties;
/// `may_overflow` = the rotor is below `2 × M`.
pub fn mint_choice(
    parent: Option<ParentStanding>,
    rotors: &[(Slot, u64)],
    a_max: u64,
    node_size: u64,
    rr: usize,
    may_overflow: bool,
) -> Option<MintChoice> {
    if let Some(p) = parent {
        if p.leased && !p.native && p.tree_bytes < a_max {
            return Some(MintChoice::Affinity(p.slot));
        }
    }
    if rotors.is_empty() {
        return None;
    }
    let strictly_over = rotors
        .iter()
        .all(|(_, b)| *b >= a_max.saturating_add(node_size));
    if strictly_over {
        if may_overflow {
            return Some(MintChoice::Overflow);
        }
        let smallest = rotors
            .iter()
            .enumerate()
            .min_by_key(|(i, (_, b))| (*b, (*i + rotors.len() - rr % rotors.len()) % rotors.len()))
            .map(|(_, (s, _))| *s)?;
        return Some(MintChoice::Smallest(smallest));
    }
    // Most headroom — SIGNED, so once every rotor tree is at or over the
    // cap the least-over (smallest) tree takes the mint and the trees
    // equalize instead of one running away; ties broken by the
    // round-robin cursor so equal trees spread (the mint-spread law
    // scoped to the node).
    let n = rotors.len();
    let best = rotors
        .iter()
        .enumerate()
        .max_by_key(|(i, (_, b))| {
            let headroom = i128::from(a_max) - i128::from(*b);
            (headroom, std::cmp::Reverse((*i + n - rr % n) % n))
        })
        .map(|(_, (s, _))| *s)?;
    Some(MintChoice::Rotor(best))
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn gate_verdicts() {
        let g = LeaseGate::new();
        assert_eq!(g.verdict(5), CommitVerdict::Unarmed);
        g.arm();
        assert_eq!(g.verdict(5), CommitVerdict::NotLeased);
        g.grant(5);
        assert_eq!(g.verdict(5), CommitVerdict::Allowed);
        g.begin_release(5);
        assert_eq!(g.verdict(5), CommitVerdict::Releasing);
        g.end_release(5);
        assert_eq!(g.verdict(5), CommitVerdict::Allowed);
        g.revoke(5);
        assert_eq!(g.verdict(5), CommitVerdict::NotLeased);
        g.grant(65_536);
        assert_eq!(g.leased_slots(), vec![65_536]);
        assert_eq!(g.leased_count(), 1);
    }

    /// The door's Dekker pair: a token taken before `Releasing` is seen
    /// by the release; a door that reads `Releasing` takes no token.
    #[test]
    fn door_tokens_pair_with_the_release() {
        let g = LeaseGate::with_namespace(8);
        g.arm();
        g.grant(3);
        assert!(g.enter(3));
        assert!(g.enter(3));
        assert_eq!(g.inflight(3), 2);
        g.begin_release(3);
        assert!(!g.enter(3), "a releasing slot refuses at the door");
        assert_eq!(g.inflight(3), 2, "a refused door leaves no token");
        assert_eq!(g.leave(3), 1);
        assert_eq!(g.leave(3), 0);
        g.end_release(3);
        assert!(g.enter(3));
        assert_eq!(g.leave(3), 0);
    }

    #[test]
    fn table_state_machine() {
        let t = SlotLeaseTable::new();
        assert!(matches!(
            t.acquire(7, 1, 0, false),
            AcquireOutcome::Granted { g: 1, .. }
        ));
        assert_eq!(t.acquire(7, 1, 0, false), AcquireOutcome::Already { g: 1 });
        assert_eq!(
            t.acquire(7, 2, 0, false),
            AcquireOutcome::Refused { holder: 1, g: 1 }
        );
        assert_eq!(
            t.offer(7, 2, 3, 100),
            Err(LeaseRefusal::NotHolder { holder: 1 })
        );
        t.offer(7, 1, 2, 100).unwrap();
        assert_eq!(
            t.acquire(7, 3, 50, false),
            AcquireOutcome::Refused { holder: 1, g: 1 }
        );
        assert_eq!(
            t.acquire(7, 2, 50, false),
            AcquireOutcome::Recall { holder: 1, g: 1 }
        );
        assert_eq!(t.begin_release(7, 1), Ok(1));
        assert_eq!(
            t.acquire(7, 2, 50, false),
            AcquireOutcome::Refused { holder: 1, g: 1 }
        );
        let words = SlotWords {
            root: (10, 20),
            cursor: 30,
            extents: 2,
            seq_floor: 40,
        };
        assert_eq!(
            t.release(7, 1, 0, words, 9),
            ReleaseOutcome::Refused { holder: 1, g: 1 }
        );
        assert_eq!(t.release(7, 1, 1, words, 9), ReleaseOutcome::Released);
        assert_eq!(t.release(7, 1, 1, words, 9), ReleaseOutcome::Already);
        assert_eq!(
            t.acquire(7, 2, 60, true),
            AcquireOutcome::Granted { g: 2, words }
        );
        assert_eq!(t.resolve(7), Resolved::Holder { holder: 2, g: 2 });
        assert_eq!(t.held_by(2), vec![7]);
        assert_eq!(t.held_by(1), Vec::<Slot>::new());
        assert_eq!(t.rotor_held_by(2), 1);
        assert_eq!(t.leased_entries(), vec![(7, 2, 2)]);
        // An offer that lapses under the requester's acquire.
        t.offer(7, 2, 5, 70).unwrap();
        assert_eq!(
            t.acquire(7, 5, 70, false),
            AcquireOutcome::Refused { holder: 2, g: 2 }
        );
        assert_eq!(t.offer_counts(), (2, 1));
        assert!(
            t.offered_to(5).is_empty(),
            "a lapsed offer leaves the index"
        );
        // The offer index answers the carriage; a release clears both.
        t.offer(7, 2, 6, 200).unwrap();
        assert_eq!(t.offered_to(6), vec![(7, 2)]);
        assert_eq!(t.begin_release(7, 2), Ok(2));
        assert_eq!(t.release(7, 2, 2, words, 11), ReleaseOutcome::Released);
        assert!(t.offered_to(6).is_empty());
        assert!(t.leased_entries().is_empty());
        assert_eq!(t.rotor_held_by(2), 0);
    }

    #[test]
    fn pick_prefers_never_written_then_lru() {
        let t = SlotLeaseTable::new();
        t.load(1, SlotLease::unleased(3, 500, SlotWords::default()));
        t.load(2, SlotLease::unleased(1, 100, SlotWords::default()));
        t.load(3, SlotLease::leased(9, 1));
        assert_eq!(t.pick_unleased(3, [1, 2, 3, 4].into_iter()), vec![4, 2, 1]);
    }

    #[test]
    fn dominance_over_a_common_window() {
        let w = DominanceWindow::new();
        let t = 1_000;
        // A crowd of 12 single-shot creators never dominates.
        for q in 0..12u64 {
            assert_eq!(w.note_ship(1, q, 0, 10, t, 2), ShipVerdict::Serve);
        }
        // One requester at N_floor with an idle holder: the idle arm.
        assert_eq!(w.note_ship(1, 100, 0, 11, t, 2), ShipVerdict::Serve);
        assert_eq!(
            w.note_ship(1, 100, 0, 12, t, 2),
            ShipVerdict::OfferIdle { to: 100 }
        );
        // A live holder: 10 ops; the requester needs 20.
        let h = HolderOps::new();
        for _ in 0..10 {
            h.note(20, t);
        }
        for i in 0..19 {
            let ops_h = h.total(20 + i, t);
            assert_eq!(w.note_ship(2, 7, ops_h, 20 + i, t, 2), ShipVerdict::Serve);
        }
        assert_eq!(
            w.note_ship(2, 7, h.total(40, t), 40, t, 2),
            ShipVerdict::OfferDominated { to: 7 }
        );
        // The window rotates: two half-windows later the counts are gone
        // — the requester starts over (one ship is far below the floor).
        assert_eq!(h.total(40 + 2 * t, t), 0);
        assert_eq!(w.note_ship(2, 7, 0, 40 + 2 * t, t, 2), ShipVerdict::Serve);
        // One half-window later the previous bucket still counts.
        let h2 = HolderOps::new();
        h2.note(0, t);
        assert_eq!(h2.total(t / 2, t), 1);
        assert_eq!(h2.total(t, t), 0);
    }

    #[test]
    fn derivations() {
        assert_eq!(mint_slots_derived(65_536, 1), 64);
        assert_eq!(mint_slots_derived(65_536, 12_500), 2);
        assert_eq!(mint_slots_derived(65_536, 1_000_000), 1);
        assert_eq!(affinity_ceiling_bytes(0, 65_536), 65_536);
        assert_eq!(affinity_ceiling_bytes(64 << 20, 65_536), 1 << 20);
        assert_eq!(n_floor(0, 100), 2);
        assert_eq!(n_floor(5_000_000, 100_000), 50);
        assert_eq!(handover_cold_start_ns(10, 3), 49);
        let rotors = [(1, 0), (2, 100), (3, 100)];
        assert_eq!(
            mint_choice(None, &rotors, 200, 64, 0, true),
            Some(MintChoice::Rotor(1))
        );
        assert_eq!(
            mint_choice(
                Some(ParentStanding {
                    slot: 9,
                    leased: true,
                    native: false,
                    tree_bytes: 10
                }),
                &rotors,
                200,
                64,
                0,
                true
            ),
            Some(MintChoice::Affinity(9))
        );
        let over = [(1, 300), (2, 300)];
        assert_eq!(
            mint_choice(None, &over, 200, 64, 0, true),
            Some(MintChoice::Overflow)
        );
        let over = [(1, 300), (2, 264)];
        assert_eq!(
            mint_choice(None, &over, 200, 64, 0, false),
            Some(MintChoice::Smallest(2))
        );
        // Exact equality with the cap is NOT strictly over: no overflow.
        let at = [(1, 200), (2, 200)];
        assert!(matches!(
            mint_choice(None, &at, 200, 64, 0, true),
            Some(MintChoice::Rotor(_))
        ));
    }
}
