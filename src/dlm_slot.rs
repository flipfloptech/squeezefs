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
//!    [`route_ino_width`](crate::meta_backend::route_ino_width) the
//!    metadata plane routes with (`docs/design-dynamic-meta-routing.md`).
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
//! * It does not home the **fencing reads**
//!   ([`SlotLockManager::get_fencing_token_ino`] and its path form).
//!   Those are ~24 call sites on hot paths and in solo mode the local
//!   view IS the authority, so paying a parse + a modulo + an ownership
//!   probe per read would buy nothing. **Contract for S6/S8:** when a
//!   foreign home becomes possible, a fencing read on a foreign-home
//!   object must become an owner read (or a leased/cached-token read) —
//!   it must NOT keep serving the local view, which would then be a
//!   stale-generation answer.
//! * It does not carry a mode knob. Solo is not a configuration; it is
//!   the absence of an installed owner table, which nothing in production
//!   can install yet.
//!
//! **The refusal, and why it is not dead code.** If ownership ever says
//! "not mine", the acquire is REFUSED LOUD — never granted locally. A
//! local grant on a foreign home is precisely the silent-divergence bug
//! this stage exists to make impossible (two nodes each believing they
//! hold exclusive custody), so the refusal is the only correct answer
//! until the remote arm ships. Production cannot reach it today (solo
//! owns every slot); [`test_set_local_slots`] is the documented seam that
//! makes it reachable, tested, and keeps [`dlm_rpcs`] a live counter
//! rather than the permanently-0 decoration spec §6.1 called out. Same
//! shape as [`crate::dlm::test_arm_cw_mode`]'s ships-disabled mode.

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
/// a future remastering event (the `PlacementTable` precedent).
static SLOT_OWNERS: Lazy<ArcSwapOption<SlotOwners>> = Lazy::new(ArcSwapOption::empty);

/// `dlm_mode` when no owner table is installed: this node is the lock
/// authority for every slot. The shipped mount's answer.
const MODE_SOLO: &str = "solo";
/// `dlm_mode` when a per-slot owner table is installed: homes are
/// partitioned and a foreign home is a remote operation.
const MODE_SLOT_HOMED: &str = "slot-homed";

/// Which slots this node's lock authority homes — a bitset, so the
/// ownership question is a shift and a mask at any width.
struct SlotOwners {
    local: Box<[u64]>,
}

impl SlotOwners {
    fn from_slots(slots: &[u64]) -> Self {
        let words = slots
            .iter()
            .map(|s| (s / 64) as usize + 1)
            .max()
            .unwrap_or(0);
        let mut local = vec![0u64; words];
        for slot in slots {
            local[(slot / 64) as usize] |= 1u64 << (slot % 64);
        }
        Self {
            local: local.into_boxed_slice(),
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
/// [`ROUTING_WIDTH`]).
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

/// The `dlm_rpcs` stats field — see [`DLM_RPCS`]. **0 in solo mode, by
/// construction.**
pub fn dlm_rpcs() -> u64 {
    DLM_RPCS.load(Ordering::Relaxed)
}

/// **Test seam** (the [`crate::dlm::test_arm_cw_mode`] precedent):
/// install a per-slot owner table where exactly `slots` are local, or
/// `None` to restore solo mode.
///
/// The foreign-home refusal is a real product behaviour with no
/// production issuer until S6/S8 ship the remote arm; this seam is what
/// makes it reachable and tested rather than a comment. Production never
/// calls it.
pub fn test_set_local_slots(slots: Option<&[u64]>) {
    match slots {
        None => SLOT_OWNERS.store(None),
        Some(slots) => SLOT_OWNERS.store(Some(Arc::new(SlotOwners::from_slots(slots)))),
    }
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
        let slot = lock_home_slot(file_path);
        if !is_local_slot(slot) {
            // The RPC site. Count it — this is exactly the round trip a
            // non-solo owner map costs — then refuse loud: granting a
            // foreign home locally would be two nodes each believing
            // they hold exclusive custody.
            DLM_RPCS.fetch_add(1, Ordering::Relaxed);
            let reason = format!(
                "lock object {file_path} homes on slot {slot} (routing width {}), which this \
                 node's lock authority does not own: remote lock acquisition ships with DLM \
                 stages S6/S8 (spec §6.9) — refusing {range:?} {mode:?} rather than granting \
                 custody the owner never issued",
                routing_width()
            );
            log::error!("{reason}");
            return Err(SqueezefsError::LockFailed { reason });
        }
        self.local
            .acquire_lock_mode(file_path, range, mode, ttl)
            .await
    }

    /// Current fencing generation for a path-form object key. Deliberately
    /// NOT homed — see the module docs' fencing-read contract for S6/S8.
    pub fn get_fencing_token(&self, file_path: &str) -> u64 {
        self.local.get_fencing_token(file_path)
    }

    /// Current fencing generation for an inode object (binary fast path).
    /// Deliberately NOT homed — see the module docs.
    pub fn get_fencing_token_ino(&self, ino: u64) -> u64 {
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
