//! DLM stage **S4** — the slot-homed lock authority, **solo mode**
//! (`docs/pre-rc-engineering-spec.md` §6.7 decisions 2/3 + §6.9 stage S4;
//! `docs/pre-rc-execution-plan.md` Phase 4, the program's go/no-go gate;
//! contracts in `tests/dlm_slot_lock_tests.rs`).
//!
//! This node owns every slot, so **every acquire is exactly the local
//! `scc` probe** `LocalLockManager` has always performed and the network
//! ledger ([`dlm_rpcs`]) is **0 by construction**. What the stage adds is
//! the two questions the remote stages need answered — asked here, once,
//! at the single acquire entry point:
//!
//! 1. **Where does this lock object live?** [`lock_home_slot`] —
//!    `slot = (ino − 2) % W` over the volume set's durable
//!    `routing_width W`, evaluated by the SAME
//!    [`crate::meta_backend::route_ino_width`] the metadata plane routes
//!    with (`docs/design-dynamic-meta-routing.md`).
//!    Spec §6.7 decision 2: the slot map is already durable, already
//!    online-migratable and already has a per-slot cutover gate that
//!    parks operations before 4a acquisition, so **no hash ring needs to
//!    be invented** — and homing on it makes the lock master and the
//!    metadata authority the same process by construction, which is what
//!    lets a metadata RPC and its lock be ONE round trip at S8.
//! 2. **Do we own that home?** [`is_local_slot`] — one lock-free load.
//!    Solo mode answers `true` for every slot unconditionally; a future
//!    non-solo answer is this same call reading an installed per-slot
//!    owner table, not a re-plumbing of the acquire path.
//!
//! **What this module deliberately does NOT do.**
//!
//! * It does not touch the lock table, the wait protocol, the custody
//!   arbitration (S11 byte ranges included) or the fencing mint: those
//!   stay in [`crate::dlm`], and [`SlotLockManager`] delegates to
//!   [`LocalLockManager`] the instant ownership says "local". Solo mode
//!   is therefore byte-identical to S0–S2 behaviour, not merely similar.
//! * It did not home the **fencing reads**
//!   ([`SlotLockManager::get_fencing_token_ino`] and its path form).
//!   Those are ~24 call sites on hot paths and in solo mode the local
//!   view IS the authority, so paying a parse + a modulo + an ownership
//!   probe per read would buy nothing. **Contract for S6/S8:** when a
//!   foreign home becomes possible, a fencing read on a foreign-home
//!   object must become an owner read (or a leased/cached-token read) —
//!   it must NOT keep serving the local view, which would then be a
//!   stale-generation answer. **Discharged by S8** (2026-08-05): the read
//!   is homed behind one relaxed load and a foreign home serves the
//!   client token cache the owners' own grants feed
//!   ([`crate::meta_ship::foreign_fencing_token`]).
//! * It does not carry a mode knob. Solo is not a configuration; it is
//!   the absence of an installed owner table, which nothing in production
//!   can install yet.
//!
//! **The refusal became a round trip — DLM S9 (2026-08-06).** S4 said the
//! only correct answer to "not mine" was a LOUD REFUSAL *"until the remote
//! arm ships"*, because a local grant on a foreign home is exactly the
//! silent-divergence bug (two nodes each believing they hold exclusive
//! custody). The remote arm has shipped: a foreign home now travels to its
//! owner through [`crate::data_grant::acquire_remote`], and what comes back
//! is custody that authority ISSUED — adopted into this process's custody
//! table with the owner's own token
//! ([`crate::dlm::adopt_remote_grant`]), never minted here.
//!
//! The refusal survives, unweakened, in the two shapes where no owner can
//! answer: **no custody client armed** (a single-writer mount, which is
//! every shipped mount) and a **non-inode object**, which has no routed home
//! and no shipped custody verb. [`dlm_rpcs`] keeps exactly the meaning S4
//! gave it — LOCK round trips — so the counter did not move when the site
//! under it stopped refusing and started travelling.

use crate::dlm::{LocalLockManager, LockLease, LockManager, LockMode};
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::route_ino_width;
use arc_swap::ArcSwapOption;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The volume set's frozen `routing_width W`, published when a routed
/// meta set is opened (`RoutedMetaBackend`'s constructors). `0` = nothing
/// published yet — an offline tool that has not opened a set, or a pure
/// in-RAM test — which routes through [`route_ino_width`]'s `W ≤ 1`
/// identity arm and homes every object on slot 0.
///
/// A process serves ONE volume set (the daemon mounts one; the offline
/// verbs open one), so a single word is the honest representation and
/// last-publish-wins is unambiguous. A test process that opens several
/// sets sees the last one — benign in solo mode, where ownership is
/// width-independent. **Contract for S6/S8:** a process that can serve
/// two sets at once must bind the width to the set handle instead.
static ROUTING_WIDTH: AtomicU64 = AtomicU64::new(0);

/// Lock operations whose home slot was NOT local — the exact count of
/// round trips a non-solo owner map would have cost. **In solo mode this
/// is 0 by construction** (the S4 gate; asserted in every acquire shape's
/// contract and exported as `dlm_rpcs` on the stats inode). The counter
/// increments at the site the remote acquire will occupy, which today
/// refuses loud; when S6/S8 land the wire there, the increment moves onto
/// the RPC itself and the meaning is unchanged.
static DLM_RPCS: AtomicU64 = AtomicU64::new(0);

/// The installed per-slot lock-ownership table, or `None` = **solo**:
/// this node owns every slot. An `ArcSwapOption` because the answer must
/// be readable lock-free on the acquire path and replaceable wholesale by
/// a future remastering event (the `PlacementTable` precedent). Composed
/// from [`OWNER_SOURCES`] by [`rebuild_owner_table`] — never stored by a
/// contributor directly.
static SLOT_OWNERS: Lazy<ArcSwapOption<SlotOwners>> = Lazy::new(ArcSwapOption::empty);

/// The two contributors of the owner table (PR 4 review round 2, Issue
/// 13 — the table was one process-global word each armed volume stored
/// WHOLESALE, so the second volume's install marked the first volume's
/// foreign slots local again and both clobbered S8's `arm_ownership`):
///
/// * `ownership_local` — the S8 metadata-ownership plane's local set
///   (`arm_ownership`; `None` = not armed, every slot local as far as it
///   is concerned);
/// * `lease_foreign` — per armed symmetric VOLUME (keyed by its
///   superblock uuid), the routing slots a FOREIGN appender leases on
///   it (design-symmetric-metadata §5.1.5).
///
/// The table in force is `local = (ownership_local ∨ all) ∖ ⋃ lease_
/// foreign`; solo (`None`) iff no contributor is present.
#[derive(Default)]
struct OwnerSources {
    ownership_local: Option<Vec<u16>>,
    lease_foreign: std::collections::BTreeMap<u128, Vec<u16>>,
}

static OWNER_SOURCES: std::sync::Mutex<OwnerSources> = std::sync::Mutex::new(OwnerSources {
    ownership_local: None,
    lease_foreign: std::collections::BTreeMap::new(),
});

/// `dlm_mode` when no owner table is installed: this node is the lock
/// authority for every slot. The shipped mount's answer.
const MODE_SOLO: &str = "solo";
/// `dlm_mode` when a per-slot owner table is installed: homes are
/// partitioned and a foreign home is a remote operation.
const MODE_SLOT_HOMED: &str = "slot-homed";

/// Which slots this node's lock authority homes — a bitset, so the
/// ownership question is a shift and a mask at any width.
///
/// Slot ids are the **u16 namespace** (`DERIVED_ROUTING_WIDTH = 2^16`,
/// the same typing `kv::slot_set::SlotSet` uses), so the table is at most
/// 1024 words = 8 KiB whatever an owner map says.
struct SlotOwners {
    local: Box<[u64]>,
}

impl SlotOwners {
    fn from_slots(slots: &[u16]) -> Self {
        let words = slots
            .iter()
            .map(|s| usize::from(*s) / 64 + 1)
            .max()
            .unwrap_or(0);
        let mut local = vec![0u64; words];
        for slot in slots {
            local[usize::from(*slot) / 64] |= 1u64 << (slot % 64);
        }
        Self {
            local: local.into_boxed_slice(),
        }
    }

    /// Every slot of the u16 namespace local (1024 full words) — the
    /// starting point a foreign set is subtracted from (Issue 15: never a
    /// 65,536-entry vector per install).
    fn all() -> Self {
        Self {
            local: vec![u64::MAX; (usize::from(u16::MAX) + 1) / 64].into_boxed_slice(),
        }
    }

    fn clear(&mut self, slot: u16) {
        if let Some(word) = self.local.get_mut(usize::from(slot) / 64) {
            *word &= !(1u64 << (slot % 64));
        }
    }

    /// Slots past the table's extent are foreign — an unlisted slot is
    /// never silently adopted.
    #[inline]
    fn is_local(&self, slot: u64) -> bool {
        match self.local.get((slot / 64) as usize) {
            Some(word) => word & (1u64 << (slot % 64)) != 0,
            None => false,
        }
    }
}

/// Publish the volume set's frozen routing width so lock homing routes
/// exactly as the metadata plane does. Called when a routed meta set is
/// constructed — mounts and offline verbs alike.
pub fn publish_routing_width(width: u64) {
    ROUTING_WIDTH.store(width, Ordering::Release);
}

/// The width lock homing currently routes over (`0` = none published; see
/// `ROUTING_WIDTH`).
pub fn routing_width() -> u64 {
    ROUTING_WIDTH.load(Ordering::Acquire)
}

/// The home slot of `ino` at width `width` — the metadata plane's own
/// routing, so the lock authority and the metadata authority can never
/// disagree.
#[inline]
pub fn slot_of_ino(ino: u64, width: u64) -> u64 {
    route_ino_width(ino, width).0
}

/// The home slot of the lock object named by `file_path`.
///
/// `inode_{N}` objects — every product lock — home by their **global**
/// ino over the published width. A non-inode object has no ino and
/// therefore no routed home; it pins to **slot 0**, the slot the root ino
/// pins to, whose owner is a member of every set by construction.
#[inline]
pub fn lock_home_slot(file_path: &str) -> u64 {
    match crate::dlm::ino_of_path(file_path) {
        Some(ino) => slot_of_ino(ino, routing_width()),
        None => 0,
    }
}

/// Does this node's lock authority home `slot`? One lock-free load; in
/// solo mode (no owner table installed) unconditionally `true`.
#[inline]
pub fn is_local_slot(slot: u64) -> bool {
    match &*SLOT_OWNERS.load() {
        None => true,
        Some(owners) => owners.is_local(slot),
    }
}

/// The `dlm_mode` stats field: `solo` while this node owns every slot.
pub fn dlm_mode() -> &'static str {
    if SLOT_OWNERS.load().is_none() {
        MODE_SOLO
    } else {
        MODE_SLOT_HOMED
    }
}

/// The `dlm_rpcs` stats field — see `DLM_RPCS`. **0 in solo mode, by
/// construction.**
pub fn dlm_rpcs() -> u64 {
    DLM_RPCS.load(Ordering::Relaxed)
}

/// Install the per-slot lock-ownership table where exactly `slots` are
/// local, or `None` to restore solo mode.
///
/// **Since S8 this has a real issuer**: `meta_ship::arm_ownership`
/// publishes the local slot set derived from the metadata ownership plane
/// here, in the same call, because spec §6.7 decision 2's whole point is
/// that the lock master and the metadata authority are the same process.
/// A foreign volume's slots therefore leave the lock plane's local set at
/// the same instant they leave the metadata plane's — no window exists in
/// which one plane would grant what the other ships away.
pub(crate) fn install_local_slots(slots: Option<&[u16]>) {
    let mut src = OWNER_SOURCES.lock().unwrap_or_else(|e| e.into_inner());
    src.ownership_local = slots.map(<[u16]>::to_vec);
    rebuild_owner_table(&src);
}

/// The symmetric plane's contribution of ONE armed volume (PR 4, §5.1.5):
/// the routing slots a foreign appender leases on `volume` (its
/// superblock uuid), or `None` at its leave. A per-volume MERGE — every
/// other volume's foreign set and S8's local set stay in force (Issue 13).
pub(crate) fn install_lease_foreign_slots(volume: u128, foreign: Option<&[u16]>) {
    let mut src = OWNER_SOURCES.lock().unwrap_or_else(|e| e.into_inner());
    match foreign {
        None => {
            src.lease_foreign.remove(&volume);
        }
        Some(f) => {
            src.lease_foreign.insert(volume, f.to_vec());
        }
    }
    rebuild_owner_table(&src);
}

/// Compose the table in force from its contributors and publish it.
fn rebuild_owner_table(src: &OwnerSources) {
    if src.ownership_local.is_none() && src.lease_foreign.is_empty() {
        SLOT_OWNERS.store(None);
        return;
    }
    let mut table = match &src.ownership_local {
        Some(local) => SlotOwners::from_slots(local),
        None => SlotOwners::all(),
    };
    for foreign in src.lease_foreign.values() {
        for slot in foreign {
            table.clear(*slot);
        }
    }
    SLOT_OWNERS.store(Some(Arc::new(table)));
}

/// **Test seam** (the [`crate::dlm::test_arm_cw_mode`] precedent):
/// `install_local_slots` without the metadata plane, so the S4 suite can
/// reach the foreign-home refusal on its own.
pub fn test_set_local_slots(slots: Option<&[u16]>) {
    install_local_slots(slots);
}

/// The slot-homed lock authority (spec §6.9 stage **S4**) — and the
/// object [`crate::dlm::DlmClient`] resolves to, so every historical call
/// site routes through homing + ownership without an edit.
///
/// Solo mode: home the object, confirm we own the home (always, today),
/// then run the unchanged local acquire.
#[derive(Clone)]
pub struct SlotLockManager {
    local: LocalLockManager,
}

impl SlotLockManager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            local: LocalLockManager::new()?,
        })
    }

    /// Acquire an **exclusive** lease on `file_path` — the whole file
    /// (`range: None`) or one `[start, end)` byte span — homed on the
    /// object's slot. See [`crate::dlm::LocalLockManager::acquire_lock`]
    /// for the custody and wait semantics, which this does not alter.
    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        self.acquire_lock_mode(file_path, range, LockMode::Exclusive, ttl)
            .await
    }

    /// [`Self::acquire_lock`] in an explicit [`LockMode`] (spec §6.7).
    ///
    /// The homing + ownership gate lives HERE, at the one entry point
    /// every acquire funnels through, so no future mode or span shape can
    /// slip past it.
    pub async fn acquire_lock_mode(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        mode: LockMode,
        ttl: Duration,
    ) -> Result<LockLease> {
        // Symmetric PR 9 (design §5.5 "S9 custody endpoint"): under the
        // armed plane an inode object's custody server is its slot's
        // HOLDER — resolved through tree 0 + the SlotHolderCache, the
        // LOCK round trip `dlm_rpcs` has always counted. One relaxed load
        // on every unarmed mount, and `None` for every own-slot object.
        if let Some((ino, home)) = crate::data_grant::slot_holder_home_of_path(file_path) {
            DLM_RPCS.fetch_add(1, Ordering::Relaxed);
            match crate::data_grant::acquire_at_slot_holder(home, ino, range, mode, ttl).await? {
                crate::data_grant::HolderAcquire::Granted(lease) => return Ok(lease),
                // The slot was handed to THIS mount under the acquire
                // (review round 2, Issue 15): the local arbiter serves.
                crate::data_grant::HolderAcquire::NowLocal => {}
            }
        }
        let slot = lock_home_slot(file_path);
        if !is_local_slot(slot) {
            // The RPC site — and since **S9** it is a real round trip.
            // `dlm_rpcs` keeps exactly the meaning S4 gave it (LOCK round
            // trips), which is why the increment did not move: it counted
            // the trip a non-solo owner map costs, and now it counts the
            // trip itself.
            DLM_RPCS.fetch_add(1, Ordering::Relaxed);
            let Some(ino) = crate::dlm::ino_of_path(file_path) else {
                // A non-inode object has no routed home (it pins to slot
                // 0) and no verb names one, so reaching here means the
                // owner table excludes slot 0 — refuse rather than invent
                // a custody protocol for an object nobody ships.
                let reason = format!(
                    "S9: lock object {file_path} is not an inode object and homes on slot \
                     {slot}, which this node does not own — there is no remote custody verb \
                     for a non-inode object, so this refuses rather than granting custody the \
                     owner never issued"
                );
                log::error!("{reason}");
                return Err(SqueezefsError::LockFailed { reason });
            };
            // S9: ship the acquire to the home's owner. Without an armed
            // custody client this is S4's refusal, verbatim in meaning: a
            // local grant on a foreign home would be two nodes each
            // believing they hold exclusive custody.
            return crate::data_grant::acquire_remote(ino, range, mode, ttl, slot).await;
        }
        self.local
            .acquire_lock_mode(file_path, range, mode, ttl)
            .await
    }

    /// **S11 rung 15**: acquire an EX byte-range lease by the §9.2
    /// required/desired law, homed exactly like every other acquire —
    /// local homes run [`LocalLockManager::acquire_lock_range`] (the
    /// solo/authority arm); a foreign home SHIPS the pair to its owner
    /// over the S9 custody lease ([`crate::data_grant::acquire_remote_range`]),
    /// whose reply is adopted (New) or widened in place (Extended — the
    /// admit-time merge's client face).
    ///
    /// `geometry` feeds the per-file span cap on the LOCAL arm only: on
    /// the shipped arm the OWNER's installed geometry source is
    /// authoritative (a client-declared size would let a misbehaving
    /// client widen its own cap).
    pub async fn acquire_lock_range(
        &self,
        file_path: &str,
        required: (u64, u64),
        desired: (u64, u64),
        ttl: Duration,
        geometry: Option<(u64, u64)>,
    ) -> Result<crate::dlm::RangeAcquired> {
        // Symmetric PR 9: the slot holder serves the ranged acquire too
        // (see `acquire_lock_mode`).
        if let Some((ino, home)) = crate::data_grant::slot_holder_home_of_path(file_path) {
            DLM_RPCS.fetch_add(1, Ordering::Relaxed);
            return crate::data_grant::acquire_range_at_slot_holder(
                home, ino, required, desired, ttl,
            )
            .await;
        }
        let slot = lock_home_slot(file_path);
        if !is_local_slot(slot) {
            DLM_RPCS.fetch_add(1, Ordering::Relaxed);
            let Some(ino) = crate::dlm::ino_of_path(file_path) else {
                let reason = format!(
                    "S11: lock object {file_path} is not an inode object and homes on slot \
                     {slot}, which this node does not own — there is no remote custody verb \
                     for a non-inode object, so this refuses rather than granting custody \
                     the owner never issued"
                );
                log::error!("{reason}");
                return Err(SqueezefsError::LockFailed { reason });
            };
            return crate::data_grant::acquire_remote_range(ino, required, desired, ttl, slot)
                .await;
        }
        self.local
            .acquire_lock_range(file_path, required, desired, ttl, geometry)
            .await
    }

    /// Current fencing generation for a path-form object key — homed since
    /// **S8** (see [`Self::get_fencing_token_ino`]).
    pub fn get_fencing_token(&self, file_path: &str) -> u64 {
        match crate::dlm::ino_of_path(file_path) {
            Some(ino) => self.get_fencing_token_ino(ino),
            // A non-inode object has no routed home (it pins to slot 0,
            // whose owner is a member of every set) and no shipped verb
            // names one, so the local view is the authority for it.
            None => self.local.get_fencing_token(file_path),
        }
    }

    /// Current fencing generation for an inode object (binary fast path).
    ///
    /// **The S4 fencing-read contract, resolved by S8.** S4 deliberately
    /// left these ~24 hot sites unhomed, correct only while solo owns
    /// every slot, and stated the obligation: *"when a foreign home
    /// becomes possible, a fencing read on a foreign-home object must
    /// become an owner read (or a leased/cached-token read) — it must NOT
    /// keep serving the local view, which would then be a
    /// stale-generation answer."*
    ///
    /// The resolution is the **cached-token read**, because §6.5 item 1
    /// forbids the alternative outright (*"≥ 99.5 % of lock operations
    /// must be served from a locally cached or delegated token"* — a
    /// round trip at 24 sites per write is not a candidate), and because
    /// every reference client converged on it. The cache is fed ONLY by
    /// owners' answers riding the metadata RPCs the operations issue
    /// anyway (§6.7 decision 3's intent locks), so it repeats an
    /// authority rather than becoming a second generator.
    ///
    /// Cost on the shipped (unarmed) path: **one relaxed load**. Nothing
    /// else about this read changed.
    pub fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        if crate::meta_ship::ownership_armed() && !is_local_slot(slot_of_ino(ino, routing_width()))
        {
            // **S9**: an ADOPTED remote grant is the owner's own answer,
            // recorded in this process's custody table at adoption — exact,
            // and strictly better than a cached grant (it is the same
            // number, from the same authority, without an aging window).
            // The S8 cache remains the answer for a foreign object this
            // node holds no custody on, which is what keeps its miss
            // tripwire meaningful.
            if let Some(generation) = crate::dlm::live_custody_generation(ino) {
                return generation;
            }
            return crate::meta_ship::foreign_fencing_token(ino);
        }
        self.local.get_fencing_token_ino(ino)
    }
}

impl LockManager for SlotLockManager {
    fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> impl std::future::Future<Output = Result<LockLease>> + Send {
        // Inherent method (resolution prefers it over the trait).
        SlotLockManager::acquire_lock(self, file_path, range, ttl)
    }
    fn get_fencing_token(&self, file_path: &str) -> u64 {
        SlotLockManager::get_fencing_token(self, file_path)
    }
    fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        SlotLockManager::get_fencing_token_ino(self, ino)
    }
}
