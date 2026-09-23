//! **Symmetric PR 10 — dead-appender recovery: the death ledger's driver**
//! (`docs/design-symmetric-metadata.md` §5.5.2, §5.5.3, §5.8.2–§5.8.5,
//! §5.9; KD-SYM-3/4/6/15).
//!
//! The ledger's WRITER is the home shard's eviction
//! ([`install_death_ledger_writer`](crate::meta_backend::kv::backend::recovery::install_death_ledger_writer): a [`crate::membership::DeathSink`]
//! that ships `RecordDeath` to volume 0's manager — in-process the direct
//! call, `KvMetaBackend::record_death_with_key`). Every manager READS
//! tree 0 of volume 0 as a projection ([`spawn_ledger_poll`](crate::meta_backend::kv::backend::recovery::spawn_ledger_poll), and once at
//! the mount path BEFORE the set serves — the C15 arm) and, per `Live` /
//! `Recovering` page on its own volume whose identity the ledger names
//! dead, runs [`KvMetaBackend::recover_dead_appenders`] in §5.9's order:
//!
//! 1. **preempt** — the dead member's registrant key on this volume's
//!    namespace (the manager's WERO, PR 3) and on the data namespaces this
//!    process holds WERO on (S7's drain proof); nothing on a non-PR
//!    substrate, where the full tail scan (step 6) is the fence;
//! 2. the page `Live` ⇒ `Recovering { recovered_by_term }` (both directory
//!    slots, barriered);
//! 3. **read** the ring from the page's tail; the three violation classes
//!    over the window refuse loud (no merge order is correct);
//! 4. the RAM lease table: every slot tree 0 leases to the dead appender
//!    goes `Unleased { g }` NOW — the manager maintains it from here
//!    (KD-SYM-2/3), its frames carry `(0, g)` under no lessee (rule 4
//!    inert), ring 0's seq offset is raised above the dead ring's frontier
//!    (the round-5 law) — and a page entry tree 0 no longer leases to the
//!    appender is stale residue, dropped (`slot_lease_stale_entries`);
//! 5. **replay** the window two-phase into those trees under the
//!    STRUCTURAL lease class (the records' floor is ring 0's head at the
//!    replay — the window itself is the dead ring's, kept by its
//!    `Recovering` page until step 9), the kind-4 data-bitmap deltas of an
//!    allocation lease the dead member held with THIS volume as its home
//!    folded onto its pages (§5.5.1 — the arm PR 8 built);
//! 6. **flush** every dirty leaf (barriered checkpoint cycles until none
//!    of the recovered slots holds a dirty node and every moved root is
//!    published) and record the **tails** — EVERY reachable leaf on a
//!    non-PR substrate (`recovery_full_tail_scan_bytes`), the resident
//!    ones under a device fence;
//! 7. **tree 0**: `Unleased { root (post-flush), cursor per §5.1.8, g,
//!    extents, seq_floor }` + `slot_tails:{s}` per slot, the dead
//!    appender's grant record minus the trees' images — the leave's own
//!    control entries, chunked by the entry cap; its UNCLAIMED grant
//!    returned; the orphan images its grant still claims that no root
//!    reaches (C13 for a dead appender) returned — never an extent an
//!    `alloc_lease:` record names;
//! 8. the page `Recovered { ledger_tail_seq = head }` (§5.8.3: never
//!    replayed again);
//! 9. `recovered:{X, v}` to volume 0 (idempotent; PR 8's allocation-lease
//!    re-grant gate) — on volume 0 a `dir_rename` lock the dead appender
//!    held is released, and the parked doors are woken.
//!
//! The region stays `Recovered` until no `alloc_lease:` record names it
//! as a holder homed here ([`KvMetaBackend::release_recovered_regions`],
//! the poll's second act): then its ring returns to the heap and the page
//! goes `Free`, its id and term kept.
//!
//! **A reclaiming `Live` page is never recovered**: the trigger is the
//! ledger record, which the S6 owner writes only for a member evicted
//! past `T_owner` under a LIVE owner or a predecessor's member that never
//! re-asserted inside the successor's grace window (§5.5.3 item 2) — a
//! member that reclaimed left `expected` at its reclaim and is named by
//! no record. A `Live` page of an identity the ledger does not name is a
//! joined appender and the driver never touches it.
//!
//! Every phase is timed on `appender_recovery_phase_ns` (exact-sum), the
//! bound `appender_recovery_bound_ms` is derived from the ring geometry
//! ([`appender_recovery_bound_ms`]), and `dead_members_acted` counts one
//! act per region recovered (closure `acted ≡ recorded × regions held`).

use super::*;
use crate::meta_backend::kv::alloc_lease::{note_dead_member_acted, DeadMemberRecord};
use crate::meta_backend::kv::appender::{
    first_segment_ring_part, read_directory, AppenderEntry, AppenderIdentity, AppenderPage,
    AppenderRegion, AppenderState, SlotEntryState,
};
use crate::meta_backend::kv::journal::RingSegment;
use crate::meta_backend::kv::superblock::ExtentRef;
use crate::meta_backend::kv::{
    appender, checkpoint, forest, journal, node, record, slot_lease, slot_state,
};
use crate::meta_backend::RoutedMetaBackend;

// ---------------------------------------------------------------------------
// Gauges (§11)
// ---------------------------------------------------------------------------

/// `appender_recoveries` — regions this node recovered.
pub static APPENDER_RECOVERIES: AtomicU64 = AtomicU64::new(0);
/// `appender_recovery_phase_ns` — exact-sum `preempt / read / replay /
/// flush / tails / tree0 / total`.
pub static APPENDER_RECOVERY_PHASE_NS: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// `recovery_full_tail_scan_bytes` — bytes of NON-RESIDENT leaves read to
/// record their tails on a non-PR substrate (§5.8.2's priced scan).
pub static RECOVERY_FULL_TAIL_SCAN_BYTES: AtomicU64 = AtomicU64::new(0);
/// `appender_recovery_preempts` — namespaces where the dead member's
/// registrant key was preempted (the PR leg's engagement).
pub static APPENDER_RECOVERY_PREEMPTS: AtomicU64 = AtomicU64::new(0);
/// `recovered_regions_released` — `Recovered` regions whose ring returned
/// to the heap once no allocation lease named them.
pub static RECOVERED_REGIONS_RELEASED: AtomicU64 = AtomicU64::new(0);
/// `appender_clear_runs` — operator attestations through `appender clear`.
pub static APPENDER_CLEAR_RUNS: AtomicU64 = AtomicU64::new(0);
/// `recovery_ledger_polls` — ledger projections this node read.
pub static RECOVERY_LEDGER_POLLS: AtomicU64 = AtomicU64::new(0);
/// `recovery_intents_rolled_forward` — a dead initiator's open intents
/// the recoverer completed after its ring replay (design §5.6).
pub static RECOVERY_INTENTS_ROLLED_FORWARD: AtomicU64 = AtomicU64::new(0);

/// Test seam (review round 1, Issue 3's pins): the recovery FAILS with a
/// retryable error once it reaches step `N` (3 = the ring read done, 4 =
/// the RAM table `Releasing` + roots installed, 5 = the replay done, 6 =
/// the flush done, 7 = the tails recorded (before tree 0), 8 = tree 0
/// written + the RAM table released (before the page), 9 = the page
/// `Recovered` (before `recovered:`)). `0` = off; consumed by the failure.
pub static TEST_RECOVERY_FAIL_AT_STEP: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Test seam: the recovery PARKS right before its tree-0 write (after the
/// tails) while set — the window a concurrent first-touch acquire of a
/// recovering slot must be refused in. `TEST_RECOVERY_HELD` reads `true`
/// while it is parked; clearing the hold and `TEST_RECOVERY_HOLD_RELEASE.
/// notify_waiters()` resumes it.
pub static TEST_RECOVERY_HOLD_BEFORE_TREE0: AtomicBool = AtomicBool::new(false);
/// Test seam: the poll PARKS between its directory snapshot and the
/// decision under the handover mutex (review round 1, Issue 8's TOCTOU
/// window) while set; `TEST_RECOVERY_HELD` / `TEST_RECOVERY_HOLD_RELEASE`
/// are shared with the tree-0 hold.
pub static TEST_RECOVERY_HOLD_BEFORE_REREAD: AtomicBool = AtomicBool::new(false);
pub static TEST_RECOVERY_HELD: AtomicBool = AtomicBool::new(false);
pub static TEST_RECOVERY_HOLD_RELEASE: squeezefs_ipc::sqz_notify::Notify =
    squeezefs_ipc::sqz_notify::Notify::new();
/// Test seam (PR 13g review round 2, Issue 20): the death path's settle
/// of the dead identity's pending `GrowRing` segment FAILS once with the
/// retryable class — the shape of ring 0 refusing the settle's admission
/// under the recovery's own hold of the SMO mutex. Consumed by the
/// failure.
pub static TEST_RECOVERY_SETTLE_FAIL_ONCE: AtomicBool = AtomicBool::new(false);

fn test_fail_at_step(step: u32, id: u32) -> std::result::Result<(), KvError> {
    if TEST_RECOVERY_FAIL_AT_STEP
        .compare_exchange(step, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Err(KvError::Busy(format!(
            "TEST_RECOVERY_FAIL_AT_STEP — appender {id}'s recovery failed at step {step}"
        )));
    }
    Ok(())
}

async fn test_hold_at(flag: &AtomicBool) {
    loop {
        let n = TEST_RECOVERY_HOLD_RELEASE.notified();
        if !flag.load(Ordering::SeqCst) {
            TEST_RECOVERY_HELD.store(false, Ordering::SeqCst);
            return;
        }
        TEST_RECOVERY_HELD.store(true, Ordering::SeqCst);
        n.await;
    }
}

/// Test seam: seconds ADDED to the clock `appender clear` judges a
/// `writer_claim`'s age against — the operator's wait for the TTL after a
/// kill, made deterministic (a killed writer's claim is heartbeat-fresh
/// for `CLIENT_STALE_TTL_SECS`, and the verb refuses it on every volume it
/// touches). `0` = the wall clock.
pub static TEST_CLAIM_CLOCK_SKEW_SECS: AtomicU64 = AtomicU64::new(0);

/// The RAM words of a recovery that did not reach its tree-0 step, rolled
/// back as ONE RAII (review round 1, Issue 3; round 2, Issue 23). Step 4
/// changes exactly these words per slot, and every one goes back:
///
/// 1. the lease table: `Leased { dead }` → `Releasing { dead }` goes back
///    to `Leased { dead }` — the door's state (a first-touch acquire is
///    refused in either state; the re-run begins the release again);
/// 2. the gate's `foreign` bit, CLEARED at step 4 so the recovery's own
///    flush is the manager's structure — restored to what it read before
///    (marked for another appender's slot; an in-process region's slot
///    never carried it), so the merge sweep, the heap-full recovery and
///    the D4 arm SKIP the tree again (`merge_sweep_foreign_skips`) instead
///    of running an SMO on a tree tree 0 still leases to the dead
///    appender — ring-0 interior records the next open's `Lease` detector
///    refuses (PR 4 round 5's Issue-28 class);
/// 3. the door's waiters, woken to re-read the table;
/// 4. the slot's RAM TREE (review round 3, Issue 29): the dead lessee's
///    page root was installed writer-legal with a `root_floor` at this
///    run's ring-0 head, and tree 0 still names the grant-time root — so
///    the installed root is `published` nowhere and, with the table back
///    at `Leased { dead }`, `publish_forest_roots` never publishes it:
///    left in place its floor clamps ring 0's checkpoint tail until a
///    re-run succeeds, and a recovery that NEVER re-runs successfully (the
///    record retired by the member's rejoin; a permanent failure) pinned
///    the tail for ever — the ring fills, every commit parks, the wedge
///    class. So every cached node of the slot is DISCARDED (dirty ones
///    included — the window's records the failed replay folded into RAM,
///    which the ring still holds and the re-run folds again;
///    `NodeCache::discard_slot_nodes`) and the tree goes back to the
///    `(root, floor)` it held before the install (`KvTree::restore_root`),
///    or leaves the forest when the run adopted it fresh from the page
///    (`SlotTrees::remove_guest`). The re-run installs from the durable
///    state exactly as a first run does.
///
/// What stays: the per-slot extent ledger (a max-only word). The page
/// stays `Recovering`; the re-run resumes from the durable state.
/// Declared AFTER the SMO mutex is taken, so it drops BEFORE the guard
/// (the discard runs under the mutex — no flush pass mid-walk). Disarmed
/// by clearing `begun` once tree 0's records are durable and the table
/// released.
struct RecoveryRollback<'a> {
    plane: &'a slot_lease::SlotLeasePlane,
    cache: &'a Arc<NodeCache>,
    forest: &'a forest::SlotTrees,
    set: &'a appender::AppenderSet,
    id: u32,
    begun: Vec<SlotRollback>,
}

/// One slot's words as step 4 found them.
struct SlotRollback {
    slot: record::ForestSlot,
    /// The gate's `foreign` bit before step 4 cleared it.
    was_foreign: bool,
    tree: TreeRollback,
    /// A dead recoverer's stashed interior records this run TOOK from
    /// [`AppenderSet::recovering_structure`] and applied (Issue 31) — put
    /// back for the re-run; the nodes they dirtied are discarded with the
    /// tree's.
    structure: Vec<appender::RecoveringInterior>,
}

/// What step 4 did to the slot's RAM tree.
enum TreeRollback {
    /// Nothing (the page named no newer root, or the install failed).
    Untouched,
    /// Re-installed at the page's root over a tree this mount held at
    /// `(root, floor)`.
    Installed {
        tree: Arc<KvTree>,
        root: RootPtr,
        floor: u64,
    },
    /// Adopted fresh from the page (this mount held no tree of the slot).
    Adopted,
}

impl Drop for RecoveryRollback<'_> {
    fn drop(&mut self) {
        if self.begun.is_empty() {
            return;
        }
        for SlotRollback {
            slot,
            was_foreign,
            tree,
            structure,
        } in self.begun.drain(..)
        {
            self.plane.table.abort_release(slot, self.id);
            if was_foreign {
                self.plane.gate.mark_foreign(slot);
            }
            if !structure.is_empty() {
                // The stash goes back for the re-run; what it dirtied is
                // discarded with the tree's nodes (an `Untouched` tree
                // discards for this alone — the flips are the ring's, the
                // re-run folds them again).
                if matches!(tree, TreeRollback::Untouched) {
                    self.cache.discard_slot_nodes(slot);
                }
                self.set
                    .recovering_structure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(slot)
                    .or_default()
                    .splice(0..0, structure);
            }
            match tree {
                TreeRollback::Untouched => {}
                TreeRollback::Installed { tree, root, floor } => {
                    let dropped = self.cache.discard_slot_nodes(slot);
                    tree.restore_root(root, floor);
                    log::info!(
                        "recovery rollback: slot {slot}'s tree back at its pre-install root \
                         {root:?} (floor {floor}; {dropped} cached node(s) discarded)"
                    );
                }
                TreeRollback::Adopted => {
                    let dropped = self.cache.discard_slot_nodes(slot);
                    self.forest.remove_guest(slot);
                    log::info!(
                        "recovery rollback: slot {slot}'s freshly adopted tree left the forest \
                         ({dropped} cached node(s) discarded)"
                    );
                }
            }
        }
        self.plane.handover_done.notify_waiters();
    }
}

/// A tree's node addresses by class ([`KvMetaBackend::node_addrs_unloaded`]).
struct TreeAddrs {
    interior: Vec<u64>,
    leaves: Vec<u64>,
}

impl TreeAddrs {
    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.interior.iter().chain(self.leaves.iter()).copied()
    }
}

/// **The `RecordDeath` key word's screen** (review round 1, Issue 7 — the
/// wire-word law over `pr_key`): the key a peer's word carries drives a
/// PREEMPT on every namespace this process holds WERO on, so it is judged
/// against durable / derived state BEFORE it is stored. `registered` is
/// the key the census REGISTERED for the member (its join's `pr_key`,
/// kept through its departure); `own` this process's registrant keys;
/// `live` every live member's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeathKeyVerdict {
    /// Stored as carried.
    Accept,
    /// The word contradicts the registered key, or names a key this
    /// process or a live member holds: `Rejected`, nothing written.
    Reject,
    /// No census knows the member (a peer owner's — PR 12's carriage):
    /// the death is recorded with key 0, the tail scan is the fence.
    Unvalidated,
}

pub fn screen_death_key(
    word: u64,
    registered: Option<u64>,
    own: &[u64],
    live: &[u64],
) -> DeathKeyVerdict {
    if word == 0 {
        return DeathKeyVerdict::Accept;
    }
    if own.contains(&word) || live.contains(&word) {
        return DeathKeyVerdict::Reject;
    }
    match registered {
        Some(k) if k == word => DeathKeyVerdict::Accept,
        Some(_) => DeathKeyVerdict::Reject,
        None => DeathKeyVerdict::Unvalidated,
    }
}

/// Test seam: the recoverer DIES right after the page went `Recovering`
/// and before anything else moved — §5.5.1's "home manager dies
/// mid-recovery" row (the successor re-runs it idempotently).
pub static TEST_RECOVERY_HALT_AFTER_RECOVERING_PAGE: AtomicBool = AtomicBool::new(false);

/// The seven phase indices of [`APPENDER_RECOVERY_PHASE_NS`].
const PH_PREEMPT: usize = 0;
const PH_READ: usize = 1;
const PH_REPLAY: usize = 2;
const PH_FLUSH: usize = 3;
const PH_TAILS: usize = 4;
const PH_TREE0: usize = 5;
const PH_TOTAL: usize = 6;

/// The Recovery family's snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    pub recoveries: u64,
    pub phase_ns: [u64; 7],
    pub full_tail_scan_bytes: u64,
    pub preempts: u64,
    pub regions_released: u64,
    pub clear_runs: u64,
    pub ledger_polls: u64,
    pub intents_rolled_forward: u64,
}

/// The family's live values.
pub fn recovery_stats() -> RecoveryStats {
    let mut phase_ns = [0u64; 7];
    for (i, p) in APPENDER_RECOVERY_PHASE_NS.iter().enumerate() {
        phase_ns[i] = p.load(Ordering::Relaxed);
    }
    RecoveryStats {
        recoveries: APPENDER_RECOVERIES.load(Ordering::Relaxed),
        phase_ns,
        full_tail_scan_bytes: RECOVERY_FULL_TAIL_SCAN_BYTES.load(Ordering::Relaxed),
        preempts: APPENDER_RECOVERY_PREEMPTS.load(Ordering::Relaxed),
        regions_released: RECOVERED_REGIONS_RELEASED.load(Ordering::Relaxed),
        clear_runs: APPENDER_CLEAR_RUNS.load(Ordering::Relaxed),
        ledger_polls: RECOVERY_LEDGER_POLLS.load(Ordering::Relaxed),
        intents_rolled_forward: RECOVERY_INTENTS_ROLLED_FORWARD.load(Ordering::Relaxed),
    }
}

/// The derived **recovery time bound** per dead region, ms (§5.9): a
/// ring of `ring_bytes` holds at most `ring_bytes / ENTRY_BYTES_PER_CREATE`
/// entries (the measured ≈ 230 B journal entry per create, design §1.4),
/// clustered into at most `entries / FILES_PER_LEAF` leaves (≈ 800 files
/// per 256 KiB leaf at ~330 B each — scaled by the node size), each a
/// cold leaf load of `LEAF_LOAD_US` (the measured 85–120 µs cold read —
/// the upper end), plus the fold (`ENTRY_FOLD_NS` per entry) and ONE
/// barriered checkpoint cycle (the landing ceiling of the cadence in
/// force). The bound is what the operator's time-to-reclaim adds to
/// `T_owner` + propagation (published `appender_recovery_bound_ms`).
pub fn appender_recovery_bound_ms(ring_bytes: u64, node_size: u64, flush_interval_ms: u64) -> u64 {
    appender_recovery_bound_with_ceiling_ms(
        ring_bytes,
        node_size,
        crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_ms(flush_interval_ms),
    )
}

/// [`appender_recovery_bound_ms`] with the checkpoint LANDING ceiling
/// already resolved (the mounted set's, resolved at open). The scan terms:
/// the flush's leaves are also the tail scan's (one extent read per leaf
/// on non-PR — `recovery_full_tail_scan_bytes`) and the orphan census's
/// interior reads are the leaves ÷ the fan-out, so both ride the
/// `leaves × LEAF_LOAD_US` term twice over (review round 1, Issue 10).
pub fn appender_recovery_bound_with_ceiling_ms(
    ring_bytes: u64,
    node_size: u64,
    landing_ceiling_ms: u64,
) -> u64 {
    /// The measured journal entry per create (design §1.4, ≈ 230 B).
    const ENTRY_BYTES_PER_CREATE: u64 = 230;
    /// Files per 256 KiB leaf at ~330 B per inode + dentry + layout.
    const FILES_PER_256K_LEAF: u64 = 800;
    /// The upper end of the measured cold leaf read (85–120 µs).
    const LEAF_LOAD_US: u64 = 120;
    /// The per-entry fold cost (a per-key LWW insert), ns.
    const ENTRY_FOLD_NS: u64 = 1_000;
    /// The leaf passes the bound prices: the flush's cold loads, the tail
    /// scan's extent reads, the orphan census's interior reads (≤ leaves).
    const LEAF_PASSES: u64 = 3;
    let entries = ring_bytes / ENTRY_BYTES_PER_CREATE;
    let files_per_leaf = (FILES_PER_256K_LEAF * node_size.max(1) / (256 * 1024)).max(1);
    let leaves = entries.div_ceil(files_per_leaf);
    let load_ms = leaves
        .saturating_mul(LEAF_LOAD_US)
        .saturating_mul(LEAF_PASSES)
        .div_ceil(1_000);
    let fold_ms = entries.saturating_mul(ENTRY_FOLD_NS).div_ceil(1_000_000);
    load_ms + fold_ms + landing_ceiling_ms
}

/// One recovered region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRegion {
    pub appender_id: u32,
    pub identity: AppenderIdentity,
    /// Slots released to tree 0 (`Unleased`).
    pub slots: Vec<record::ForestSlot>,
    /// Window entries replayed.
    pub entries: u64,
    /// Page entries tree 0 no longer leased to the appender (dropped).
    pub stale_entries: u64,
    /// Data-bitmap bits the replay changed on a lease homed here.
    pub data_bits_changed: u64,
}

/// What one pass of the driver did on one volume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub recovered: Vec<RecoveredRegion>,
    /// `Live` pages the ledger names dead but this volume could not act
    /// on (a failed volume, a non-writer).
    pub deferred: u64,
    /// Pages the poll snapshotted that had MOVED by the time the decision
    /// was taken under the handover mutex (another actor's act — Issue 8),
    /// or whose member is live again (Issue 9): not recovered, not an error.
    pub skipped: u64,
    /// `Recovered` pages the ledger names whose `recovered:` record was
    /// missing (the recoverer died between the two writes) — the record
    /// completed by this pass.
    pub completed: u64,
}

/// What the routed pass did across the set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySetReport {
    pub per_volume: Vec<(u16, RecoveryReport)>,
    pub regions_released: u64,
    pub intents_rolled_forward: u64,
    /// Parked death records this projection made durable (Issue 5).
    pub deaths_landed: u64,
    /// Death records the retirement sweep retired (§5.5.2).
    pub records_retired: u64,
}

impl RecoverySetReport {
    /// Regions recovered across the set.
    pub fn recovered(&self) -> u64 {
        self.per_volume
            .iter()
            .map(|(_, r)| r.recovered.len() as u64)
            .sum()
    }
}

/// The C14 / C15 census of one volume (§5.8.5), as fsck reports it and
/// the mount path acts on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustodyCensus {
    /// C14 — slot custody conflicts: `(slot, appender a, appender b)` for
    /// one slot attested `Live` on two pages at tree 0's `g`, or `(slot,
    /// page's appender, tree 0's lessee)` for a `Live` entry contradicting
    /// tree 0 at a newer `g` (a `Releasing` entry never counts).
    pub conflicts: Vec<(record::ForestSlot, u32, u32)>,
    /// C15 — un-recovered appenders: `(appender id, identity, window
    /// entries)` for every `Live` / `Recovering` page the death ledger
    /// names whose ring window is non-empty (or whose page is
    /// `Recovering` — a recovery that never finished).
    pub unrecovered: Vec<(u32, AppenderIdentity, u64)>,
    /// `Recovering` pages of an identity the ledger does NOT name — a
    /// shape no legal schedule writes (every `Recovering` write follows
    /// a ledger read); the mount refuses on it naming `appender clear`.
    pub recovering_unledgered: Vec<(u32, AppenderIdentity)>,
}

/// `squeezefs appender clear`'s outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppenderClearOutcome {
    /// The page is `Free` / `Recovered` / absent — nothing to attest.
    NothingToClear,
    /// The attestation landed: the death record for `identity`, the page
    /// marked `Recovering` (its `Live` entries are no longer custody at
    /// the next mount's C14 settle); the next mount recovers it (C15).
    Cleared {
        identity: AppenderIdentity,
        was: AppenderState,
        window_entries: u64,
    },
}

/// The dead ring's two-phase replay input, held across the phases.
struct DeadRing {
    ring: JournalRing,
    recovery: journal::JournalRecovery,
}

/// `(node_token, mount_slot)` equality — the death key's identity (a
/// page's `writer_id` changes per mount and never binds).
fn same_mount(a: &AppenderIdentity, b: &AppenderIdentity) -> bool {
    a.node_token == b.node_token && a.mount_slot == b.mount_slot
}

impl KvMetaBackend {
    /// The wire-word screen of `RecordDeath` (the level-5 law): a peer's
    /// word never declares dead (a) this mount's own identity, nor (b) a
    /// member the installed S6 owner lists LIVE — both `Rejected`
    /// (`STATUS_REJECTED`, `manager_verb_rejected`), nothing written; and
    /// (c) the KEY word is judged before it can drive a preempt (review
    /// round 1, Issue 7 — [`screen_death_key`]): against the key the
    /// owner's census REGISTERED for the member (kept through its
    /// departure), this process's own registrant keys and every live
    /// member's — a contradiction is `Rejected`; a member no census knows
    /// has its key DROPPED to 0 (the tail scan is the fence). Answers the
    /// key to store.
    pub fn screen_record_death(
        &self,
        member: &crate::meta_ship::manager::WireIdentity,
        pr_key: u64,
    ) -> std::result::Result<u64, KvError> {
        let reject = |why: String| {
            if let Some(set) = self.appenders.as_ref() {
                set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            }
            KvError::Rejected(format!(
                "{}: RecordDeath from the wire — {why} (manager_verb_rejected)",
                self.path.display()
            ))
        };
        if let Some(set) = self.appenders.as_ref() {
            if set.identity.node_token == member.node_token
                && set.identity.mount_slot == member.mount_slot
            {
                return Err(reject(
                    "the member named is THIS mount's own identity".to_string(),
                ));
            }
        }
        let id = crate::cowriter::node_member_id_of(member.node_token, member.mount_slot);
        let owner = crate::membership::installed_owner();
        if let Some(owner) = owner.as_ref() {
            if owner.member_is_live(&id) {
                return Err(reject(format!(
                    "member '{id}' holds a LIVE lease with this owner (inside T_owner)"
                )));
            }
        }
        let registered = owner.as_ref().and_then(|o| o.registered_key(&id));
        let live: Vec<u64> = owner.as_ref().map(|o| o.live_keys()).unwrap_or_default();
        let mut own = vec![self.pr_key];
        own.extend(crate::data_custody::own_registrant_keys());
        match screen_death_key(pr_key, registered, &own, &live) {
            DeathKeyVerdict::Accept => Ok(pr_key),
            DeathKeyVerdict::Reject => Err(reject(format!(
                "the key word {pr_key:#x} contradicts the census (registered {registered:?}) or \
                 names a key this process or a live member holds — a preempt it would drive is \
                 refused"
            ))),
            DeathKeyVerdict::Unvalidated => {
                log::warn!(
                    "meta volume {}: RecordDeath for '{id}' carries key {pr_key:#x} no census \
                     here registered — recorded with key 0 (the tail scan is the fence; PR 12's \
                     carriage of a peer owner's census validates it)",
                    self.path.display()
                );
                Ok(0)
            }
        }
    }

    /// Test seam: this mount's metadata-namespace registrant key (`0` =
    /// none) — what the key-word screen refuses as "our own" (the
    /// contracts' witness).
    pub fn test_pr_key(&self) -> u64 {
        self.pr_key
    }

    /// Test seam: the volume's node-seq handle as it stands — the word
    /// every root install raises (review round 2, Issue 24's witness).
    pub fn test_node_seq_now(&self) -> u64 {
        self.seq_handle().load()
    }

    /// Test seam: the interior records ring 0's window carried for `slot`
    /// at this open while its lessee's page was `Recovering` — a dead
    /// recoverer's flips, stashed for the re-run (Issue 31's witness).
    pub fn test_recovering_structure_len(&self, slot: record::ForestSlot) -> usize {
        self.appenders
            .as_ref()
            .map(|set| {
                set.recovering_structure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&slot)
                    .map_or(0, Vec::len)
            })
            .unwrap_or(0)
    }

    /// Test seam: the UNPUBLISHED guest-root floors per slot AS THE
    /// CHECKPOINT READS THEM — the tail's clamp after the lease filter
    /// (empty on a flat volume; review round 3, Issue 29's witness; round
    /// 7, Issue 32's).
    pub fn test_unpublished_root_floors(
        &self,
    ) -> std::collections::BTreeMap<record::ForestSlot, u64> {
        self.unpublished_root_floors()
    }

    /// **The custody quarantines' re-derivation** (review round 8, Issue
    /// 36): every `custody_quarantine:{slot}` record in this volume's tree
    /// 0 — written by a recovery from an EARLY death record beside the
    /// slot's `Unleased` — installs its deadline into the process's RAM
    /// quarantine when still in the future, so a manager restart inside
    /// the window keeps refusing fresh custody grants on the slot's files;
    /// expired records are retired in ONE control entry (best effort — a
    /// refused write leaves them for the next open). Returns the count
    /// installed. A no-op (no read) on a flat volume or an unarmed forest.
    pub async fn load_custody_quarantines(&self) -> std::result::Result<usize, KvError> {
        let Some(forest) = self.forest() else {
            return Ok(0);
        };
        if !self.slot_lease_armed() {
            return Ok(0);
        }
        let control = Arc::clone(forest.control());
        let (start, end) = slot_state::custody_quarantine_key_range();
        let page = control.range(&start, &end, 4096).await?;
        if page.is_empty() {
            return Ok(0);
        }
        let now = crate::meta_backend::kv::alloc_lease::unix_now_ms();
        let mut installed = 0usize;
        let mut expired: Vec<(u8, Record)> = Vec::new();
        for (k, v) in &page {
            let slot = slot_state::decode_custody_quarantine_key(k)?;
            let until = slot_state::decode_custody_quarantine(v)?;
            if until > now {
                crate::data_grant::quarantine_slot_custody(self.volume_uuid(), slot, until);
                installed += 1;
            } else {
                expired.push((
                    journal::tag_for(record::TREE_CONTROL, 0),
                    Record::delete(k.to_vec(), 0),
                ));
            }
        }
        if installed > 0 {
            log::warn!(
                "meta volume {}: {installed} slot(s) under a custody quarantine a previous \
                 incarnation wrote — fresh custody grants on their files stay refused until \
                 the bound",
                self.path.display()
            );
        }
        if !expired.is_empty() {
            if let Err(e) = self.write_control_entry(expired, EntryAdmission::Try).await {
                log::warn!(
                    "meta volume {}: retiring expired custody-quarantine records failed ({e}); \
                     the next open retries",
                    self.path.display()
                );
            }
        }
        Ok(installed)
    }

    /// **The recovery HOLD** (Issue 31, made real in review round 7): the
    /// lowest unpublished-root floor of a slot whose lessee is a foreign
    /// appender mid-recovery — its page `Recovering` at this open, or the
    /// driver between its `Recovering` write and its tree-0 step. Those
    /// floors are the dead recoverer's records in ring 0's window (its
    /// flips and root swaps, stashed at the open; its parked frees), which
    /// only the re-run's tree-0 publication may release: the bring-up
    /// covers down to the hold, never past it. `None` with no hold (every
    /// flat mount, every open without a recovery in flight).
    pub(super) fn recovery_hold_floor(&self) -> Option<u64> {
        let set = self.appenders.as_ref()?;
        let plane = set.slot_leases()?;
        let forest = self.forest()?;
        forest
            .unpublished_root_floors()
            .into_iter()
            .filter(|(slot, _)| match plane.table.resolve(*slot) {
                crate::slot_lease_core::Resolved::Holder { holder, .. } => {
                    set.region(holder).is_none() && plane.recovering_lessees.contains_sync(&holder)
                }
                crate::slot_lease_core::Resolved::Unleased { .. } => false,
            })
            .map(|(_, floor)| floor)
            .min()
    }

    /// Test seam: the device byte ranges appender `id`'s REGION owns on
    /// this volume — its ring segments, its two directory page slots, and
    /// every extent its `extent_grant` record names (the images its trees
    /// reach and the unclaimed remainder) — as `(offset, len)`. The two-backend
    /// fixture's capture set: a foreign lessee's later activity touches
    /// exactly these bytes and nothing the manager holds cached, so a
    /// snapshot of them re-applied to the device while the manager is
    /// open IS another daemon's checkpoint landing under it.
    pub async fn test_region_device_ranges(
        &self,
        id: u32,
    ) -> std::result::Result<Vec<(u64, u64)>, KvError> {
        let mut out = Vec::new();
        let entries = read_directory(&self.path, &self.sb).await?;
        let Some(e) = entries.iter().find(|e| e.appender_id == id) else {
            return Ok(out);
        };
        for off in e.dir_offsets {
            out.push((off, appender::APPENDER_PAGE_LEN as u64));
        }
        if let Some(p) = e.page.as_ref() {
            for s in &p.segments {
                out.push((s.start, s.len));
            }
        }
        let node_size = self.cache.config().layout.node_size() as u64;
        for ext in self.extent_grant_record(id).await?.extents() {
            out.push((self.cache.extent_addr(ext), node_size));
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Test seam: PLANT `claim` as `path`'s `writer_claim` — a foreign
    /// host's live manager as this host sees it (the `appender clear`
    /// contracts' fixture; an UNARMED forest, whose frames stamp `(0, 0)`
    /// like this raw writer's). Takes the flock like the verb.
    pub async fn test_plant_writer_claim(
        path: &Path,
        claim: &WriterClaim,
    ) -> std::result::Result<(), KvError> {
        let guard_fd = match Self::acquire_writer_flock(path) {
            Ok(fd) => fd,
            Err(FlockOutcome::Held) => {
                return Err(KvError::Busy(format!(
                    "{}: live-mounted on this host",
                    path.display()
                )));
            }
            Err(FlockOutcome::Io(e)) => {
                return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
            }
        };
        let mut inner = Self::open_inner(path, OpenPosture::Writer, None).await?;
        *inner.guard_fd.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(guard_fd);
        let be = Arc::new(inner);
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
            .await?;
        be.sync_device().await.map_err(KvError::Io)?;
        be.checkpoint_now().await?;
        be.shutdown().await
    }

    /// Whether this volume's manager role has been RELEASED by the vol-0
    /// rule (§5.5.2; `manager_lease` reads `vacant` on a joined writer):
    /// the heartbeat then stops refreshing the D0 claim, so the ladder
    /// re-elects a successor once it ages past the TTL.
    pub fn manager_role_released(&self) -> bool {
        let Some(set) = self.appenders.as_ref() else {
            return false;
        };
        set.joined.load(Ordering::Acquire)
            && *set.manager_lease.lock().unwrap_or_else(|e| e.into_inner())
                == appender::ManagerLease::Vacant
    }

    /// This volume's published recovery bound ([`appender_recovery_bound_ms`])
    /// over the LARGEST ring a dead region of it can hold — PR 2 caps every
    /// declared ring at the fixed ring's length, so the manager's own ring
    /// is the ceiling — at the cadence in force. 0 on a bit-17-absent
    /// volume (no region can die there).
    pub fn appender_recovery_bound_ms(&self) -> u64 {
        let Some(set) = self.appenders.as_ref() else {
            return 0;
        };
        // The cadence term is the landing ceiling RESOLVED AT OPEN
        // (`AppenderSet::flush_ceiling_ms` ≡ `checkpoint_landing_ceiling_
        // ms(interval)`), never an env read per stats serve (review round
        // 1, Issue 21).
        appender_recovery_bound_with_ceiling_ms(
            Self::fixed_ring_extent(&self.sb).len,
            u64::from(self.sb.node_size),
            set.flush_ceiling_ms,
        )
    }

    /// The GLOBAL inos named by every FOREIGN appender ring window this
    /// open did not replay — every `Live` / `Recovering` page that is not
    /// one of this mount's regions (a same-node page is own residue,
    /// replayed at open), its ring read ONCE from its `ledger_tail_seq`:
    /// the inode keys the window puts or deletes and the child inos its
    /// dentry records name. fsck's inode plane SCOPES these out of C9/C10
    /// (review round 1, Issue 12 — never the whole plane): a cross-owner
    /// create's dentry rides the parent holder's ring while the child's
    /// record is the creator's, so the referenced set is incomplete over
    /// exactly these inos and complete over every other. Empty on a flat
    /// volume, a solo forest, and a set whose peers are checkpointed.
    pub async fn foreign_window_inos(
        &self,
        width: u64,
    ) -> std::result::Result<std::collections::BTreeSet<u64>, KvError> {
        let mut out = std::collections::BTreeSet::new();
        let Some(set) = self.appenders.as_ref() else {
            return Ok(out);
        };
        let entries = read_directory(&self.path, &self.sb).await?;
        for e in &entries {
            let Some(page) = e.page.as_ref() else {
                continue;
            };
            if !matches!(page.state, AppenderState::Live | AppenderState::Recovering) {
                continue;
            }
            if set.region(e.appender_id).is_some() || same_mount(&page.identity, &set.identity) {
                continue;
            }
            if page.segments.is_empty() {
                continue;
            }
            let dead = self.read_dead_ring(page).await?;
            for entry in &dead.recovery.entries {
                for (tag, r) in &entry.records {
                    let (kind, level) = untag(*tag);
                    if level > 0 || !record::is_slot_tree_kind(kind) {
                        continue;
                    }
                    let Ok((k, legacy)) = record::split_forest_key(&r.key) else {
                        continue;
                    };
                    let mentioned: Option<u64> = match k {
                        record::TREE_INODES => record::decode_inode_key(&legacy).ok(),
                        record::TREE_DENTRIES => match r.kind {
                            // A dentry DELETE carries no value, and its
                            // child is exactly the ino the plane must not
                            // judge (review round 2, Issue 26): a cross-
                            // owner unlink's name removal rides the parent
                            // holder's ring while the child's `nlink`
                            // decrement landed in the child's slot — read
                            // as `nlink` BELOW the name count, C10's LOSS
                            // finding, until the holder's checkpoint. The
                            // window is unreplayed, so the tree still holds
                            // the pre-delete dentry: the key resolves it.
                            record::RecordKind::Delete => {
                                match self.lookup_kind(k, &legacy).await? {
                                    Some(v) => {
                                        record::DentryValue::decode(&v).ok().map(|d| d.child_ino)
                                    }
                                    None => None,
                                }
                            }
                            _ => record::DentryValue::decode(&r.value)
                                .ok()
                                .map(|d| d.child_ino),
                        },
                        _ => None,
                    };
                    let Some(ino) = mentioned else {
                        continue;
                    };
                    // A guest local-key ino becomes its global form; a
                    // native or already-global ino (a dentry's child) is
                    // itself.
                    let global = match crate::meta_backend::split_guest_local(ino) {
                        Some((slot, raw)) => {
                            crate::meta_backend::make_global_ino_width(raw, u64::from(slot), width)
                        }
                        None => ino,
                    };
                    out.insert(global);
                }
            }
        }
        Ok(out)
    }

    /// **The C14 / C15 census** (§5.8.5) of this volume against the ledger
    /// on `vol0` (`None` = no ledger reachable: C15 is empty, C14 stands).
    /// fsck's face — every dead ring read once for C15's window count.
    pub async fn slot_custody_census(
        &self,
        vol0: Option<&Arc<KvMetaBackend>>,
    ) -> std::result::Result<CustodyCensus, KvError> {
        self.slot_custody_census_opts(vol0, true).await
    }

    /// [`Self::slot_custody_census`] with the ring reads optional: the
    /// mount-path gate needs the `Recovering`-unledgered verdict alone and
    /// the driver reads every dead ring right after it (review round 1,
    /// Issue 21 — the ring was read twice per recovery on the mount path).
    /// `read_windows = false` reports every ledgered page as unrecovered
    /// with a window count of 0.
    pub async fn slot_custody_census_opts(
        &self,
        vol0: Option<&Arc<KvMetaBackend>>,
        read_windows: bool,
    ) -> std::result::Result<CustodyCensus, KvError> {
        let mut out = CustodyCensus::default();
        let Some(set) = self.appenders.as_ref() else {
            return Ok(out);
        };
        let Some(forest) = self.forest() else {
            return Ok(out);
        };
        let entries = read_directory(&self.path, &self.sb).await?;
        let dead: Vec<(AppenderIdentity, DeadMemberRecord)> = match vol0 {
            Some(v) => v.dead_member_records().await?,
            None => Vec::new(),
        };
        let leases = Self::read_tree0_lease_map(forest.control()).await?;
        let gens = self.tree0_generations().await?;
        // C14: every Live page's Live entries, one attestation per slot at
        // tree 0's g; a Live entry under another lessee at tree 0's g.
        let mut live_by_slot: std::collections::BTreeMap<record::ForestSlot, u32> =
            Default::default();
        for e in &entries {
            let Some(page) = e.page.as_ref() else {
                continue;
            };
            if page.state != AppenderState::Live {
                continue;
            }
            for se in page
                .slots
                .iter()
                .filter(|s| s.state == SlotEntryState::Live)
            {
                let slot = appender::forest_slot_of_page_slot(se.slot, set.native_slot);
                if gens.get(&slot).is_some_and(|(g, _)| *g > se.g) {
                    continue; // stale residue, never a conflict
                }
                // One finding per UNORDERED pair per slot: a second Live
                // page and tree 0's lease usually name the same two
                // appenders, and the census reports the conflict once.
                let mut note = |a: u32, b: u32| {
                    let pair = (slot, a.min(b), a.max(b));
                    if !out.conflicts.contains(&pair) {
                        out.conflicts.push(pair);
                    }
                };
                if let Some(other) = live_by_slot.insert(slot, e.appender_id) {
                    if other != e.appender_id {
                        note(other, e.appender_id);
                    }
                }
                if let Some((_, Some(lessee))) = gens.get(&slot) {
                    if *lessee != e.appender_id {
                        note(e.appender_id, *lessee);
                    }
                }
            }
        }
        // C15: a Live / Recovering page the ledger names, with a window.
        for e in &entries {
            let Some(page) = e.page.as_ref() else {
                continue;
            };
            let named = dead.iter().any(|(id, _)| same_mount(id, &page.identity));
            match page.state {
                AppenderState::Live if named => {
                    let window = if read_windows {
                        self.window_entries_of(page).await.unwrap_or(0)
                    } else {
                        0
                    };
                    let holds = leases.get(&e.appender_id).is_some_and(|s| !s.is_empty());
                    if window > 0 || holds || page.is_manager || !read_windows {
                        out.unrecovered.push((e.appender_id, page.identity, window));
                    }
                }
                AppenderState::Recovering if named => {
                    let window = if read_windows {
                        self.window_entries_of(page).await.unwrap_or(0)
                    } else {
                        0
                    };
                    out.unrecovered.push((e.appender_id, page.identity, window));
                }
                AppenderState::Recovering => {
                    out.recovering_unledgered
                        .push((e.appender_id, page.identity));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Tree 0's `(g, lessee)` per slot — `None` lessee when `Unleased`.
    async fn tree0_generations(
        &self,
    ) -> std::result::Result<
        std::collections::BTreeMap<record::ForestSlot, (u32, Option<u32>)>,
        KvError,
    > {
        let mut out = std::collections::BTreeMap::new();
        let Some(control) = self.forest_control_tree() else {
            return Ok(out);
        };
        let (mut cursor, end) = slot_state::slot_state_key_range();
        loop {
            let page = control.range(&cursor, &end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = node::key_successor(last);
            for (k, v) in &page {
                let slot = slot_state::decode_slot_state_key(k)?;
                match slot_state::SlotState::decode(v)? {
                    slot_state::SlotState::Leased { appender_id, g, .. } => {
                        out.insert(slot, (g, Some(appender_id)));
                    }
                    slot_state::SlotState::Unleased { g, .. } => {
                        out.insert(slot, (g, None));
                    }
                }
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
    }

    /// The window entries a page's ring holds past its tail — one read of
    /// the ring (the census's C15 evidence; the driver reads the ring
    /// again for the replay, under its own locks).
    async fn window_entries_of(&self, page: &AppenderPage) -> std::result::Result<u64, KvError> {
        if page.segments.is_empty() {
            return Ok(0);
        }
        let dead = self.read_dead_ring(page).await?;
        Ok(dead.recovery.entries.len() as u64)
    }

    /// Read a dead appender's ring from its page's tail.
    async fn read_dead_ring(&self, page: &AppenderPage) -> std::result::Result<DeadRing, KvError> {
        let segs: Vec<RingSegment> = page
            .segments
            .iter()
            .enumerate()
            .map(|(i, ext)| {
                RingSegment::from_extent(&if i == 0 {
                    first_segment_ring_part(ext)
                } else {
                    *ext
                })
            })
            .collect();
        let (ring, recovery) = JournalRing::recover_segments(
            &self.path,
            segs,
            journal::checkpoint_reserve_bytes(page.ring_bytes()),
            page.ledger_tail_seq,
        )
        .await?;
        ring.recover_seq_offset(
            page.seq_offset
                .max(journal::seq_offset_of_window(&recovery.entries)),
        );
        Ok(DeadRing { ring, recovery })
    }

    /// Write a FOREIGN appender's page image into BOTH directory slots
    /// (the manager's page-writing primitive — `write_wire_joiner_page_
    /// grant`'s form; a state change is a table-neutral write, and the
    /// pair alone finds the region).
    async fn write_foreign_page(
        &self,
        entry: &AppenderEntry,
        page: &mut AppenderPage,
    ) -> std::result::Result<(), KvError> {
        for off in entry.dir_offsets {
            page.generation += 1;
            appender::write_page(&self.path, off, page.encode()?).await?;
        }
        self.sync_device().await.map_err(KvError::Io)?;
        Ok(())
    }

    /// **The driver** (§5.9): recover every `Live` / `Recovering` page of
    /// this volume whose identity the ledger on `vol0` names dead —
    /// serial per manager, each region in §5.9's order. `vol_ordinal` is
    /// this volume's ordinal in the set (the `recovered:` record's key).
    /// A no-op on a flat volume, a non-writer, an unarmed forest, and a
    /// volume whose manager role is not held.
    pub async fn recover_dead_appenders(
        self: &Arc<Self>,
        vol0: &Arc<KvMetaBackend>,
        vol_ordinal: u16,
    ) -> std::result::Result<RecoveryReport, KvError> {
        let mut report = RecoveryReport::default();
        let Some(set) = self.appenders.as_ref() else {
            return Ok(report);
        };
        if self.read_only || self.non_writer || self.is_failed() {
            return Ok(report);
        }
        if !set.joined.load(Ordering::Acquire)
            || *set.manager_lease.lock().unwrap_or_else(|e| e.into_inner())
                != appender::ManagerLease::Held
        {
            return Ok(report);
        }
        let dead = vol0.dead_member_records().await?;
        if dead.is_empty() {
            return Ok(report);
        }
        let entries = read_directory(&self.path, &self.sb).await?;
        for e in &entries {
            let Some(page) = e.page.clone() else {
                continue;
            };
            // Our own regions are never the ledger's to recover: a same-
            // node page is own residue at open (PR 2), never a peer.
            if set.region(e.appender_id).is_some() || same_mount(&page.identity, &set.identity) {
                continue;
            }
            let Some((_, rec)) = dead.iter().find(|(id, _)| same_mount(id, &page.identity)) else {
                continue;
            };
            // A `Recovered` page the ledger names without its `recovered:`
            // record: the recoverer died between its page write and the
            // record (Issue 3's last window) — the record is what PR 8's
            // re-grant gate waits on, so it is completed here, idempotent.
            if page.state == AppenderState::Recovered {
                if vol0
                    .recovered_record(&page.identity, vol_ordinal)
                    .await?
                    .is_none()
                {
                    let _handover = self.handover.lock().await;
                    vol0.manager_record_recovered(page.identity, vol_ordinal)
                        .await?;
                    if vol_ordinal == 0 {
                        if let Err(err) = self.manager_dir_rename_release_dead(e.appender_id).await
                        {
                            log::warn!(
                                "meta volume {}: releasing dead appender {}'s directory-rename \
                                 lock failed ({err})",
                                self.path.display(),
                                e.appender_id
                            );
                        }
                    }
                    if let Some(plane) = set.slot_leases() {
                        plane.handover_done.notify_waiters();
                    }
                    report.completed += 1;
                    log::warn!(
                        "meta volume {}: appender {}'s recovery COMPLETED — its page was \
                         Recovered without its `recovered:` record (the recoverer died in \
                         between); the record is written now",
                        self.path.display(),
                        e.appender_id
                    );
                }
                continue;
            }
            if !matches!(page.state, AppenderState::Live | AppenderState::Recovering) {
                continue;
            }
            let rec = *rec;
            test_hold_at(&TEST_RECOVERY_HOLD_BEFORE_REREAD).await;
            match self.recover_region(vol0, vol_ordinal, e, page, &rec).await {
                Ok(Some(r)) => report.recovered.push(r),
                Ok(None) => report.skipped += 1,
                Err(err) => {
                    log::error!(
                        "meta volume {}: recovery of appender {} (node {:#018x}, mount slot \
                         {:#x}) FAILED: {err} — its page stays as written; the next ledger poll \
                         (or the next mount) re-runs it",
                        self.path.display(),
                        e.appender_id,
                        e.page.as_ref().map_or(0, |p| p.identity.node_token),
                        e.page.as_ref().map_or(0, |p| p.identity.mount_slot),
                    );
                    report.deferred += 1;
                }
            }
        }
        Ok(report)
    }

    /// One region's recovery (§5.9), under the handover mutex (serialized
    /// with the cadence's releases and the leave) and — for the replay,
    /// the flush and the tree-0 writes — the SMO mutex (the structural
    /// door: no manager SMO on the trees is in flight while their custody
    /// moves, and no pass spans the move). `Ok(None)` = skipped: the page
    /// the poll snapshotted moved under it (review round 1, Issue 8) or
    /// the member is live again (Issue 9).
    ///
    /// **The order is the handover protocol's** (review round 1, Issue 3
    /// — §5.8.4's recover-before-grant clause): the RAM lease table marks
    /// the dead appender's slots `Releasing` FIRST (a first-touch acquire
    /// at the commit door is refused naming the holder, exactly like a
    /// slot mid-handover), every durable step follows, and the table goes
    /// `Unleased` LAST — after tree 0's `Unleased` records are barriered.
    /// Any `Err` in between aborts the release (`Leased { dead }` again,
    /// the page left `Recovering`) so the re-run resumes from the durable
    /// state with nothing acked lost; the first build released the table
    /// at step 4 and a `Try` refusal at step 7 left tree 0 `Leased {
    /// dead }` under a `Recovered` page for ever.
    async fn recover_region(
        self: &Arc<Self>,
        vol0: &Arc<KvMetaBackend>,
        vol_ordinal: u16,
        entry: &AppenderEntry,
        snapshot: AppenderPage,
        dead: &DeadMemberRecord,
    ) -> std::result::Result<Option<RecoveredRegion>, KvError> {
        use std::time::Instant;
        let t_total = Instant::now();
        let set = self.manager_gate(false)?;
        let plane = set.slot_leases().cloned().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: dead-appender recovery needs the armed symmetric plane \
                 (SQUEEZEFS_SYMMETRIC_META=1)",
                self.path.display()
            ))
        })?;
        let forest = self.forest().ok_or_else(|| {
            KvError::Corrupt(format!("{}: not a forest volume", self.path.display()))
        })?;
        let _handover = self.handover.lock().await;
        let id = entry.appender_id;
        // ---- 0. the decision is taken UNDER the handover mutex on a
        // FRESH read (Issue 8): the poll's snapshot may have been overtaken
        // by the mount-path gate, an online fsck repair or a rejoin — a
        // page no longer `Live` / `Recovering`, or of another identity or
        // term, is somebody else's act; and the ledger record must still
        // stand (a rejoined member retired it — Issue 9).
        let entries_now = read_directory(&self.path, &self.sb).await?;
        let Some(entry) = entries_now.iter().find(|e| e.appender_id == id) else {
            return Ok(None);
        };
        let Some(mut page) = entry.page.clone() else {
            return Ok(None);
        };
        if !matches!(page.state, AppenderState::Live | AppenderState::Recovering)
            || !same_mount(&page.identity, &snapshot.identity)
            || page.term != snapshot.term
        {
            log::info!(
                "meta volume {}: appender {id}'s page moved under the ledger poll (now {} / \
                 node {:#018x} slot {:#x} term {}) — this projection skips it",
                self.path.display(),
                page.state.as_str(),
                page.identity.node_token,
                page.identity.mount_slot,
                page.term
            );
            return Ok(None);
        }
        let identity = page.identity;
        if vol0.dead_member_record(&identity).await?.is_none() {
            log::info!(
                "meta volume {}: appender {id}'s death record was retired under the poll (the \
                 member rejoined) — not recovered",
                self.path.display()
            );
            return Ok(None);
        }
        if let Some(owner) = crate::membership::installed_owner() {
            let member =
                crate::cowriter::node_member_id_of(identity.node_token, identity.mount_slot);
            if owner.member_is_live(&member) {
                // The incarnation check (Issue 9): a member the owner lists
                // LIVE again holds a newer lease epoch than the record's —
                // it rejoined; its record is retired here (idempotent) and
                // its region is its own.
                vol0.retire_death_record(&identity).await?;
                log::warn!(
                    "meta volume {}: appender {id}'s member '{member}' is LIVE with the \
                     installed owner (rejoined past its death record, epoch {}) — record \
                     retired, region left to its holder",
                    self.path.display(),
                    dead.epoch
                );
                return Ok(None);
            }
        }
        log::warn!(
            "meta volume {}: RECOVERING appender {id} (node {:#018x}, mount slot {:#x}, term \
             {}) — named dead by the ledger (epoch {}, {} ms ago); page was {}",
            self.path.display(),
            identity.node_token,
            identity.mount_slot,
            page.term,
            dead.epoch,
            crate::meta_backend::kv::alloc_lease::unix_now_ms().saturating_sub(dead.ts_ms),
            page.state.as_str()
        );

        // ---- 1. preempt (PR substrates; the fidelity leg's engagement).
        // `pr_fenced` — the tail scan's fence verdict (§5.8.2) — is TRUE
        // only when the preempt of a NON-ZERO key LANDED on this volume's
        // namespace (review round 1, Issue 4): a key-less record (`appender
        // clear`'s), a failed preempt or a detection-grade substrate all
        // take the full tail scan, the fence that needs no device.
        let t = Instant::now();
        let mut pr_fenced = false;
        if dead.pr_key != 0 && self.is_own_registrant_key(dead.pr_key) {
            // The S9 sweep's own-key law (`multi_writer.rs`) on the death
            // ledger: a CO-LOCATED appender shares this host's registrant
            // (KD-SYM-22 — its adopted word IS this manager's key, or a
            // peer's `RecordDeath` carried it), and a preempt-and-abort of
            // one's own key takes down one's own fence. A same-host death
            // is the flock's / the S6 eviction's proof; the full tail scan
            // is its fence.
            log::warn!(
                "meta volume {}: dead appender {id}'s record carries key {:#x}, which THIS \
                 process holds (the co-located shared-registrant shape) — no preempt is driven; \
                 the full tail scan is the fence for this recovery",
                self.path.display(),
                dead.pr_key
            );
        } else if dead.pr_key != 0 {
            let victim = dead.pr_key;
            let meta = self.preempt_meta_registrant(victim).await;
            let data = squeezefs_ipc::sqz_blocking::run_blocking(move || {
                crate::data_custody::preempt_dead_registrant(victim)
            })
            .await;
            pr_fenced = set.stats().meta_pr_wero && meta == 1;
            APPENDER_RECOVERY_PREEMPTS.fetch_add(meta + data, Ordering::Relaxed);
        }
        APPENDER_RECOVERY_PHASE_NS[PH_PREEMPT]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);

        // ---- 2. the page: Live ⇒ Recovering (a re-run keeps its term).
        if page.state == AppenderState::Live {
            page.state = AppenderState::Recovering;
            page.recovered_by_term = self.writer_term();
            self.write_foreign_page(entry, &mut page).await?;
        }
        // The lessee is mid-recovery from here (Issue 31): the frame
        // screen admits the manager's `(0, g)` frames on its slots beside
        // its own — the flush below writes them, and a reload (an eviction
        // now, the next open after a death) must read them.
        let _ = plane.recovering_lessees.insert_sync(id);
        if TEST_RECOVERY_HALT_AFTER_RECOVERING_PAGE.load(Ordering::SeqCst) {
            return Err(KvError::Busy(format!(
                "{}: TEST_RECOVERY_HALT_AFTER_RECOVERING_PAGE — the recoverer died after \
                 appender {id}'s page went Recovering",
                self.path.display()
            )));
        }

        // ---- 3. read the ring; the violation classes over its window.
        let t = Instant::now();
        let dead_ring = if page.segments.is_empty() {
            None
        } else {
            Some(self.read_dead_ring(&page).await?)
        };
        let entries_n = dead_ring
            .as_ref()
            .map_or(0, |d| d.recovery.entries.len() as u64);
        let mut leases = Self::read_tree0_lease_map(forest.control()).await?;
        let grant_record = self.extent_grant_record(id).await?;
        // A RE-RUN past its tree-0 step (the page `Recovering`, tree 0
        // already `Unleased` for slots the window still carries): those
        // slots were replayed, flushed and released by the run that died
        // after its tree-0 write (Issue 3's step 8/9 windows) — their
        // records are ABSORBED, judged legal here and never re-applied
        // (`absorbed`); only the page's `Recovered` and the `recovered:`
        // record are owed. A `Live` page's window names only slots tree 0
        // leases to the appender (PR 2's law), so the set is empty there.
        let mut absorbed: std::collections::BTreeSet<record::ForestSlot> = Default::default();
        if let Some(d) = dead_ring.as_ref() {
            if page.state == AppenderState::Recovering {
                let gens = self.tree0_generations().await?;
                for e in &d.recovery.entries {
                    for (tag, r) in &e.records {
                        let (kind, level) = untag(*tag);
                        let slot = if kind == record::KIND_INTERIOR && level > 0 {
                            forest::split_interior_journal_key(&r.key)
                                .ok()
                                .map(|(s, _)| s)
                        } else if level == 0 && record::is_slot_tree_kind(kind) {
                            record::forest_key_slot(&r.key).ok()
                        } else {
                            None
                        };
                        if let Some(s) = slot {
                            if matches!(gens.get(&s), Some((_, None))) {
                                absorbed.insert(s);
                            }
                        }
                    }
                }
                if !absorbed.is_empty() {
                    leases
                        .entry(id)
                        .or_default()
                        .extend(absorbed.iter().copied());
                }
            }
            let owned = vec![(
                id,
                journal::JournalRecovery {
                    entries: d.recovery.entries.clone(),
                    head_pos: d.recovery.head_pos,
                    dropped_torn: d.recovery.dropped_torn,
                    foreign_pages: d.recovery.foreign_pages,
                },
            )];
            let granted = |appender: u32, extent: u64| -> bool {
                appender == id && grant_record.contains(extent)
            };
            // A re-run past its tree-0 step judges nothing: the run that
            // released the slots judged this same window (a dead ring
            // never moves) and then MOVED the trees' images out of the
            // grant record, so the window's alloc deltas would now read as
            // outside it.
            let violations = if absorbed.is_empty() {
                // The dead lessee's OWN ring is judged by the lease map
                // alone: the recovering exemption is ring 0's (Issue 31).
                journal::detect_appender_violations(&owned, &leases, &granted, &Default::default())
            } else {
                log::info!(
                    "meta volume {}: appender {id}'s recovery re-runs past its tree-0 step \
                     ({} slot(s) already released) — the window was judged by the run that \
                     released them",
                    self.path.display(),
                    absorbed.len()
                );
                Vec::new()
            };
            if !violations.is_empty() {
                let shown: Vec<String> = violations.iter().take(4).map(|v| v.to_string()).collect();
                return Err(KvError::Corrupt(format!(
                    "{}: appender {id}'s ring window violates the partition ({} record(s): \
                     {}) — no merge order is correct; refusing to recover it",
                    self.path.display(),
                    violations.len(),
                    shown.join("; ")
                )));
            }
            if d.recovery.dropped_torn > 0 {
                // The un-checkpointed-tail artifact (a torn last entry
                // past the head, never acked) — logged like the mount's.
                log::info!(
                    "meta volume {}: appender {id}'s ring dropped {} torn tail record(s) at \
                     its head (never acked)",
                    self.path.display(),
                    d.recovery.dropped_torn
                );
            }
        }
        APPENDER_RECOVERY_PHASE_NS[PH_READ]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        test_fail_at_step(3, id)?;

        // ---- 4. the RAM lease table: the dead appender's slots go
        // RELEASING now — held by the dead id, refusing every first-touch
        // acquire at the door (`SlotBusy`, the mid-handover verdict) — and
        // `Unleased` only at step 7b, after tree 0's records are durable;
        // the `foreign` bit is cleared so the STRUCTURAL class (the
        // replay, the flush pass's SMOs) is the manager's from here. Stale
        // page entries dropped.
        let t = Instant::now();
        let held: Vec<record::ForestSlot> = leases
            .get(&id)
            .map(|s| {
                s.iter()
                    .copied()
                    .filter(|s| !absorbed.contains(s))
                    .collect()
            })
            .unwrap_or_default();
        let mut stale = 0u64;
        for se in &page.slots {
            let slot = appender::forest_slot_of_page_slot(se.slot, set.native_slot);
            if !held.contains(&slot) {
                stale += 1;
                plane.stale_entries.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The window's highest local ino per slot (§5.1.8's third term).
        let mut window_max: std::collections::BTreeMap<record::ForestSlot, u64> =
            Default::default();
        if let Some(d) = dead_ring.as_ref() {
            for e in &d.recovery.entries {
                for (tag, r) in &e.records {
                    let (kind, level) = untag(*tag);
                    if level > 0 || !record::is_slot_tree_kind(kind) {
                        continue;
                    }
                    let Ok(slot) = record::forest_key_slot(&r.key) else {
                        continue;
                    };
                    if kind == record::TREE_BLOCK_REFS || r.key.len() < 8 {
                        continue;
                    }
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&r.key[..8]);
                    let local =
                        u64::from_be_bytes(b) & ((1u64 << crate::meta_backend::GUEST_NS_SHIFT) - 1);
                    let m = window_max.entry(slot).or_insert(0);
                    *m = (*m).max(local);
                }
            }
        }
        let dead_frontier = dead_ring.as_ref().map_or(0, |d| d.ring.seq_frontier());
        // The structural door for everything from the root install on: no
        // manager SMO on the trees is in flight while their custody moves.
        let mut smo = self.smo.lock().await;
        // Defect 33: the flush-ceiling audit judges the leaves that age
        // under this hold against the extended bound (declared after the
        // mutex guard so it drops FIRST — the hold's end is recorded while
        // the mutex is still ours).
        let _recovery_hold = self.recovery_hold();
        // Every RAM word step 4 changes goes back on any `Err` before step
        // 7b; declared under the mutex so its drop runs under it.
        let mut rollback = RecoveryRollback {
            plane: &plane,
            cache: &self.cache,
            forest,
            set,
            id,
            begun: Vec::new(),
        };
        // The recovered records' floor on THIS ring (the replay journals
        // nothing here; the flush makes them durable) and the installed
        // roots' un-published floor.
        let floor = self.ring.core().head();
        let mut released: Vec<(record::ForestSlot, u32, crate::slot_lease_core::SlotWords)> =
            Vec::new();
        for slot in &held {
            let Some(lease) = plane.table.get(*slot) else {
                continue;
            };
            if lease.state == crate::slot_lease_core::LeaseState::Unleased || lease.holder != id {
                continue;
            }
            let was_foreign = plane.gate.is_foreign(*slot);
            match plane.table.begin_release(*slot, id) {
                Ok(_) => rollback.begun.push(SlotRollback {
                    slot: *slot,
                    was_foreign,
                    tree: TreeRollback::Untouched,
                    structure: Vec::new(),
                }),
                Err(refusal) => {
                    return Err(KvError::Corrupt(format!(
                        "{}: the lease table refused to begin the recovery release of slot \
                         {slot} from dead appender {id} ({refusal:?})",
                        self.path.display()
                    )));
                }
            }
            plane.gate.clear_foreign(*slot);
            let page_entry = page
                .slots
                .iter()
                .find(|se| appender::forest_slot_of_page_slot(se.slot, set.native_slot) == *slot);
            // The tree's root: the lessee's PAGE word at the lease's
            // generation — KD-SYM-3, a leased slot's root rides its
            // lessee's page, written by its checkpoints strictly AFTER the
            // grant; tree 0's record is the grant-time root. Never a
            // node-seq comparison (PR 12b — the `sym-storm` fleet leg: a
            // joiner's node seqs are ITS handle's, the manager's its own;
            // "the newer by seq" picked the manager's grant-time root over
            // the dead joiner's page root on four of 64 slots and every
            // record the joiner had FLUSHED there was lost — 84 acked
            // files absent at the manager). A page entry below the
            // lease's `g` is a previous lease's stale attestation (PR 4's
            // `slot_lease_stale_entries` class) and yields to the record.
            let recorded = lease.words;
            let mut root = RootPtr {
                addr: recorded.root.0,
                seq: recorded.root.1,
            };
            match page_entry {
                Some(se) if se.root.addr != 0 && se.g >= lease.g => root = se.root,
                Some(se) => log::warn!(
                    "{}: recovery of appender {id}: slot {slot}'s page entry (g {}, root {:#x}) \
                     is not the lease's (g {}) — the grant-time root {:#x} (seq {}) stands",
                    self.path.display(),
                    se.g,
                    se.root.addr,
                    lease.g,
                    root.addr,
                    root.seq
                ),
                None => log::warn!(
                    "{}: recovery of appender {id}: slot {slot} has NO page entry — the \
                     grant-time root {:#x} (seq {}) stands (g {})",
                    self.path.display(),
                    root.addr,
                    root.seq,
                    lease.g
                ),
            }
            if root.addr != 0 {
                // The ONE install the own-residue open runs too (Issue
                // 24): the root node's seq verified against the pointer,
                // the node-seq handle raised to it — an EMPTY window
                // raises it nowhere else, and a recoverer minting below
                // the dead lessee's stamps would adopt a residue frame in
                // a returned extent as its own tail (PR 11's class).
                // Installed whenever the RAM tree stands elsewhere: the
                // recoverer never writes a leased slot's tree, so its RAM
                // root is at most the page's — equal or stale, never ahead
                // (a re-run past a rollback is back at its pre-install
                // root; equal roots are left untouched).
                // A lessee of ANOTHER daemon appends into node IMAGES under
                // an unchanged root pointer (the log-structured node's
                // bset append — the root `(addr, seq)` names the same
                // node): equality of roots says nothing about the cached
                // images here. For a wire lessee the barrier runs whatever
                // the roots read (PR 12b round 3, the F1 pin's `keep`
                // slot: 40 records appended into the grant-time root leaf
                // were invisible at the recoverer, whose cached image of
                // that leaf predated them); an in-process region shares
                // this cache and equal roots ARE the same images.
                let foreign_daemon = set.region(id).is_none();
                let tree_rollback = match forest.tree(*slot) {
                    Some(tr) => {
                        if root != tr.root() || foreign_daemon {
                            // The recoverer's RAM tree is STALE (Issue 2):
                            // it holds the slot at the root it last saw
                            // while the lessee's checkpoints moved it. Every
                            // cached node of the slot is dropped (the cache
                            // barrier — a stale image would fold the window
                            // onto a base missing the lessee's flushed bsets)
                            // and the page's root installed writer-legal.
                            let prior = (tr.root(), tr.root_floor());
                            self.cache.drop_slot_nodes(*slot)?;
                            tr.install_recovered_root(root, floor).await?;
                            TreeRollback::Installed {
                                tree: Arc::clone(&tr),
                                root: prior.0,
                                floor: prior.1,
                            }
                        } else {
                            TreeRollback::Untouched
                        }
                    }
                    None => {
                        let tree = KvTree::open_unpublished_slot_tree(
                            Arc::clone(&self.cache),
                            *slot,
                            root,
                            self.seq_handle(),
                            floor,
                        )
                        .await?;
                        forest.adopt_guest_unpublished(*slot, Arc::new(tree));
                        TreeRollback::Adopted
                    }
                };
                if let Some(last) = rollback.begun.last_mut() {
                    last.tree = tree_rollback;
                }
                // A dead recoverer's flips of this slot (Issue 31): ring
                // 0's window carried the manager's interior records for a
                // tree whose lessee's page was `Recovering` at this
                // mount's open — a previous recoverer's step-6 compactions
                // that died before their tree-0 step; the open stashed them
                // instead of folding them into a foreign tree. They apply
                // HERE, onto the installed page root (the base they were
                // journaled against), in the structural class the recovery
                // holds, before the dead window; the rollback puts them
                // back. Their floors are their own ring-0 positions.
                let stash = set
                    .recovering_structure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(slot)
                    .unwrap_or_default();
                if !stash.is_empty() {
                    if let Some(last) = rollback.begun.last_mut() {
                        last.structure = stash.clone();
                    }
                    let tree = forest.tree(*slot).ok_or_else(|| {
                        KvError::Corrupt(format!(
                            "{}: slot {slot} has stashed recovery structure but no tree",
                            self.path.display()
                        ))
                    })?;
                    let mut ordered = stash;
                    ordered.sort_by(|a, b| {
                        b.level.cmp(&a.level).then(a.record.seq.cmp(&b.record.seq))
                    });
                    let applied = ordered.len();
                    for s in ordered {
                        let (_slot, separator) = forest::split_interior_journal_key(&s.record.key)?;
                        tree.apply_replayed_interior_recovery(
                            separator,
                            s.level,
                            s.record.seq,
                            s.record.kind,
                            Bytes::copy_from_slice(&s.record.value),
                            s.entry_seq,
                        )
                        .await?;
                    }
                    log::warn!(
                        "meta volume {}: appender {id}'s recovery re-applied {applied} interior \
                         record(s) a previous recoverer left in ring 0 for slot {slot} (it died \
                         between its flush and its tree-0 step)",
                        self.path.display()
                    );
                }
            }
            // §5.1.8: the cursor is never lowered.
            let cursor = recorded
                .cursor
                .max(page_entry.map_or(0, |se| se.cursor))
                .max(window_max.get(slot).map_or(0, |m| m + 1));
            let extents = u64::from(recorded.extents)
                .max(page_entry.map_or(0, |se| u64::from(se.slot_tree_extents)))
                .max(plane.extents.get(*slot));
            if extents != 0 {
                plane.extents.set(*slot, extents);
            }
            let words = crate::slot_lease_core::SlotWords {
                root: (root.addr, root.seq),
                cursor,
                extents: u32::try_from(extents).unwrap_or(u32::MAX),
                seq_floor: recorded.seq_floor.max(dead_frontier),
            };
            released.push((*slot, lease.g, words));
        }
        test_fail_at_step(4, id)?;

        // ---- 5. replay under the structural door.
        if let Some(d) = dead_ring.as_ref() {
            self.replay_dead_window(&d.recovery, floor, &absorbed)
                .await?;
        }
        APPENDER_RECOVERY_PHASE_NS[PH_REPLAY]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        test_fail_at_step(5, id)?;

        // The data-bitmap arm (§5.5.1): a lease the dead member held with
        // THIS volume as its home — its pages take the window's deltas.
        let mut data_bits = 0u64;
        for (vol_tag, lease) in vol0.alloc_lease_records().await? {
            if !same_mount(&lease.holder, &identity) || lease.home_vol != vol_ordinal {
                continue;
            }
            let refs: Vec<ExtentRef> = lease.bitmap.iter().map(|(_, e)| *e).collect();
            if refs.is_empty() {
                continue;
            }
            let image = self.read_alloc_bitmap_image(&refs, lease.blocks).await?;
            let pages = crate::data_alloc_bitmap::DataAllocBitmap::from_region_image(
                vol_tag,
                lease.blocks,
                &image,
            )?;
            let kept =
                crate::data_alloc_bitmap::take_replayed_deltas(&self.path, vol_tag, lease.term);
            let changed = pages.replay(
                kept.iter().map(|r| (record::TREE_ALLOC_RESERVED, r)).chain(
                    dead_ring.iter().flat_map(|d| {
                        d.recovery
                            .entries
                            .iter()
                            .flat_map(|e| e.records.iter().map(|(t, r)| (journal::untag(*t).0, r)))
                    }),
                ),
                lease.term,
            );
            if changed > 0 {
                let base = refs.first().map_or(0, |e| e.start);
                pages
                    .write_dirty_pages(
                        &self.path,
                        base,
                        self.checkpoint_seq.load(Ordering::Acquire) + 1,
                    )
                    .await?;
                self.sync_device().await.map_err(KvError::Io)?;
            }
            data_bits += changed;
        }

        // ---- 6. flush: barriered cycles until no recovered slot holds a
        // dirty node (the handover's post-condition law, `COVER_CYCLES_MAX`).
        // The moved roots stay UNPUBLISHED through these cycles — a slot
        // mid-recovery is skipped by `publish_forest_roots` exactly like a
        // slot mid-handover, its floor clamps the tail — and step 7's
        // `Unleased` records ARE their publication.
        let t = Instant::now();
        let slots: Vec<record::ForestSlot> = released.iter().map(|(s, _, _)| *s).collect();
        for cycle in 0..=checkpoint::COVER_CYCLES_MAX {
            self.checkpoint_cycle(&mut smo, true).await?;
            let dirty = self.dirty_nodes_of_slots(&slots);
            if dirty == 0 {
                break;
            }
            if cycle == checkpoint::COVER_CYCLES_MAX {
                return Err(KvError::Corrupt(format!(
                    "{}: recovery of appender {id} could not flush its slot trees in {} \
                     barriered cycles ({dirty} dirty node(s)) — a stuck tail is a defect, \
                     never a longer wait",
                    self.path.display(),
                    checkpoint::COVER_CYCLES_MAX
                )));
            }
        }
        APPENDER_RECOVERY_PHASE_NS[PH_FLUSH]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        test_fail_at_step(6, id)?;

        // ---- 6b. tails: every reachable leaf on non-PR, the resident
        // ones under a device fence (§5.8.2's tail-coverage law).
        let t = Instant::now();
        let mut tails_by_slot: Vec<(record::ForestSlot, Vec<(u64, u32)>)> = Vec::new();
        for slot in &slots {
            let tails = match forest.tree(*slot) {
                Some(tr) => self.recovery_leaf_tails(&tr, pr_fenced).await?,
                None => Vec::new(),
            };
            tails_by_slot.push((*slot, tails));
        }
        APPENDER_RECOVERY_PHASE_NS[PH_TAILS]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        test_fail_at_step(7, id)?;
        test_hold_at(&TEST_RECOVERY_HOLD_BEFORE_TREE0).await;

        // ---- 7. tree 0: Unleased + tails per slot, the grant record
        // minus the trees' images (the leave's chunked entries), admitted
        // drain-and-retry under the SMO mutex the recovery holds (a park
        // here would wait on the checkpoint task, which waits on this
        // mutex); the unclaimed grant returned; the dead appender's orphan
        // images returned (C13 for a dead appender).
        let t = Instant::now();
        // The death-path custody QUARANTINE's deadline (review round 7,
        // Issue 34; round 8, Issue 36): an EARLY record — `appender clear`,
        // the same-node takeover — can be written inside a surviving
        // custody writer's `T_self` (the plane's own recorders cannot:
        // `T_self < T_owner`), so this arbiter grants nothing fresh on the
        // recovered slots' files until the OWNER-side bound — `T_owner`
        // past the record, plus `2 × skew_max` for the recorder's wall
        // clock against ours — has elapsed; the S7 dead-epoch quarantine's
        // shape, per slot. Written beside each slot's `Unleased` (durable
        // across a restart inside the window), installed in RAM at 7b.
        let custody_quarantine_until = dead.early.then(|| {
            dead.ts_ms
                .saturating_add(crate::data_grant::custody_quarantine_bound_ms())
        });
        self.release_recovered_slots(
            &plane,
            id,
            &released,
            tails_by_slot,
            custody_quarantine_until,
            &mut smo,
        )
        .await?;

        // ---- 7b. the RAM lease table LAST (Issue 3): tree 0's `Unleased`
        // records are barriered — the slots go `Unleased { g }` here, the
        // manager maintains them from now on (the stamp reads (0, g)
        // under no lessee), ring 0's seq offset is raised above the dead
        // ring's frontier (the round-5 law). From here nothing is rolled
        // back: every later step is idempotent against durable state.
        for (slot, g, words) in &released {
            let outcome = plane.table.release(*slot, id, *g, *words, self.lease_seq());
            if !matches!(
                outcome,
                crate::slot_lease_core::ReleaseOutcome::Released
                    | crate::slot_lease_core::ReleaseOutcome::Already
            ) {
                return Err(KvError::Corrupt(format!(
                    "{}: the lease table refused the recovery release of slot {slot} from dead \
                     appender {id} at g {g} ({outcome:?}) AFTER its tree-0 record landed — the \
                     RAM table and tree 0 disagree",
                    self.path.display()
                )));
            }
            plane.gate.revoke(*slot);
            if words.seq_floor != 0 {
                self.ring.raise_seq_floor(words.seq_floor);
            }
        }
        rollback.begun.clear();
        plane.refresh_holders();
        // The S4 lock plane and the gate's structural belt follow the
        // table (the grant's and the release's own step): the recovered
        // slots leave the process-wide FOREIGN set, so a custody acquire on
        // their files is the local arbiter's from here — the seam PR 9's
        // rebase found: without it every `acquire_lock` on a recovered
        // slot's file was refused as a foreign home until the volume's next
        // grant or release republished the owners.
        self.publish_slot_owners(set, &plane);
        if let Some(until) = custody_quarantine_until {
            for (slot, _, _) in &released {
                crate::data_grant::quarantine_slot_custody(self.volume_uuid(), *slot, until);
            }
            if !released.is_empty() {
                log::warn!(
                    "meta volume {}: appender {id}'s death record is an EARLY attestation — \
                     fresh custody grants on its {} recovered slot(s) are quarantined until \
                     Unix ms {until} (T_owner + 2 × skew_max past the record)",
                    self.path.display(),
                    released.len()
                );
            }
        }
        // The UNCLAIMED remainder: the page's runs MINUS every extent the
        // window's `alloc` records claimed since that page write — those
        // hold live images the trees now reach (returning one would free
        // a live root); the window's retired images fall to the orphan
        // sweep below as claimed-and-unreachable.
        let window_allocs: std::collections::BTreeSet<u64> = dead_ring
            .iter()
            .flat_map(|d| d.recovery.entries.iter())
            .flat_map(|e| e.records.iter())
            .filter(|(tag, r)| {
                untag(*tag).0 == TREE_ALLOC_RESERVED
                    && !crate::data_alloc_bitmap::is_data_alloc_delta_key(&r.key)
            })
            .filter_map(
                |(_, r)| match crate::meta_backend::kv::alloc_ext::decode_alloc_record(r) {
                    Ok(crate::meta_backend::kv::alloc_ext::AllocDelta::Allocated { extent }) => {
                        Some(extent)
                    }
                    _ => None,
                },
            )
            .collect();
        let unclaimed: Vec<u64> = page
            .grant
            .iter()
            .flat_map(|run| run.start..run.start + u64::from(run.len))
            .filter(|e| !window_allocs.contains(e))
            .collect();
        if !unclaimed.is_empty() {
            match self.return_extents_inner(id, &unclaimed, false).await {
                Ok((returned, _)) => log::info!(
                    "meta volume {}: appender {id}'s unclaimed grant returned ({returned} \
                     extent(s))",
                    self.path.display()
                ),
                Err(e) => log::warn!(
                    "meta volume {}: appender {id}'s unclaimed grant return failed ({e}) — the \
                     extents stay granted; the region release retries and fsck C13 reclaims",
                    self.path.display()
                ),
            }
        }
        let orphans = self
            .dead_appender_orphans(vol0, id, &identity, &slots)
            .await?;
        if !orphans.is_empty() {
            // The orphan images carry the dead lessee's node-seq stamps
            // (its retired predecessors and successors — minted from ITS
            // handle, which this mount's watermark law never covered):
            // the handle is floored above every stamp they carry BEFORE
            // the extents return to the heap (PR 11's residue law,
            // `node::residue_seq_ceiling`; Issue 24) — one extent read per
            // orphan, the price the offline census pays. Under the
            // per-incarnation seq spaces (PR 13, `kv::node_seq`) a wire
            // joiner's stamps sit in ITS space, where this handle never
            // mints: `raise_to` ignores them and the disjointness is the
            // guarantee; a same-space stamp (an in-process region's) still
            // floors the handle exactly as before.
            let mut ceiling = 0u64;
            let node_size = self.cache.config().layout.node_size() as u64;
            for e in &orphans {
                let image = crate::uring_fs::read_at(
                    &self.path,
                    self.sb.heap.start + e * node_size,
                    node_size as usize,
                )
                .await
                .map_err(KvError::Io)?;
                ceiling = ceiling.max(crate::meta_backend::kv::node::residue_seq_ceiling(&image));
            }
            if ceiling != 0 {
                self.seq_handle().raise_to(ceiling);
            }
            match self.return_extents_inner(id, &orphans, false).await {
                Ok((returned, _)) => log::info!(
                    "meta volume {}: appender {id}'s {returned} orphan image extent(s) returned \
                     (C13 for a dead appender)",
                    self.path.display()
                ),
                Err(e) => log::warn!(
                    "meta volume {}: appender {id}'s orphan return failed ({e}) — fsck C13 \
                     reclaims",
                    self.path.display()
                ),
            }
        }
        // A GrowRing segment the dead page names is released with its
        // ring (`release_recovered_regions`); one the manager carved that
        // the page never named — the incarnation died between the reply
        // and its page write — is returned HERE (the witness's settle
        // point on the death path; PR 13g review round 1, Issue 1).
        {
            let _g = self.manager_verbs.lock().await;
            let settled = if TEST_RECOVERY_SETTLE_FAIL_ONCE
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                Err(KvError::JournalReserveExhausted { needed: 0 })
            } else {
                self.settle_pending_ring_segment(
                    identity,
                    Some(&page),
                    "the death ledger's recovery",
                )
                .await
            };
            if let Err(e) = settled {
                log::warn!(
                    "meta volume {}: appender {id}'s pending GrowRing segment could not be \
                     settled ({e}) — the next verb for its identity retries",
                    self.path.display()
                );
            }
        }
        drop(smo);
        test_fail_at_step(8, id)?;

        // ---- 8. the page: Recovered, its tail the head it was read to
        // (§5.8.3 — never replayed again). The page is re-read: its
        // generation moved under the flush cycles' own writes of nothing
        // (a foreign page is never written by a checkpoint) but the
        // directory-first law wants the newest image edited.
        let entries_now = read_directory(&self.path, &self.sb).await?;
        let entry_now = entries_now
            .iter()
            .find(|e| e.appender_id == id)
            .ok_or_else(|| {
                KvError::Corrupt(format!(
                    "{}: appender {id}'s page vanished from the directory mid-recovery",
                    self.path.display()
                ))
            })?;
        let mut page = entry_now.page.clone().unwrap_or(page);
        page.state = AppenderState::Recovered;
        page.recovered_by_term = self.writer_term();
        page.slots.clear();
        // Its slots are the manager's now (tree 0 `Unleased`, step 7 —
        // rule 4 is inert there by the lease alone).
        plane.recovering_lessees.remove_sync(&id);
        if let Some(d) = dead_ring.as_ref() {
            let head = d.ring.core().head();
            page.head_hint = head;
            page.ledger_tail_seq = head;
            page.seq_offset = d.ring.seq_offset();
        }
        self.write_foreign_page(entry_now, &mut page).await?;
        test_fail_at_step(9, id)?;

        // ---- 9. recovered:{X, v}; volume 0's dead-holder lock; the doors.
        vol0.manager_record_recovered(identity, vol_ordinal).await?;
        if vol_ordinal == 0 {
            if let Err(e) = self.manager_dir_rename_release_dead(id).await {
                log::warn!(
                    "meta volume {}: releasing dead appender {id}'s directory-rename lock \
                     failed ({e})",
                    self.path.display()
                );
            }
        }
        plane.handover_done.notify_waiters();
        APPENDER_RECOVERY_PHASE_NS[PH_TREE0]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        APPENDER_RECOVERY_PHASE_NS[PH_TOTAL]
            .fetch_add(t_total.elapsed().as_nanos() as u64, Ordering::Relaxed);
        APPENDER_RECOVERIES.fetch_add(1, Ordering::Relaxed);
        note_dead_member_acted(dead.ts_ms);
        log::warn!(
            "meta volume {}: appender {id} RECOVERED — {entries_n} window entr{} replayed, {} \
             slot(s) released to tree 0, {stale} stale page entr{} dropped, {data_bits} \
             data-bitmap bit(s) folded, in {} ms",
            self.path.display(),
            if entries_n == 1 { "y" } else { "ies" },
            slots.len(),
            if stale == 1 { "y" } else { "ies" },
            t_total.elapsed().as_millis()
        );
        Ok(Some(RecoveredRegion {
            appender_id: id,
            identity,
            slots,
            entries: entries_n,
            stale_entries: stale,
            data_bits_changed: data_bits,
        }))
    }

    /// The two-phase replay of a dead ring's window into the slot trees
    /// its keys name (§5.3.4 per ring) under the structural class; the
    /// heap alloc/free deltas are the dead appender's grant-internal
    /// claims and retirements — the images stay claimed in its record
    /// until the tree-0 step moves the live ones and returns the rest.
    async fn replay_dead_window(
        &self,
        rec: &journal::JournalRecovery,
        floor: u64,
        absorbed: &std::collections::BTreeSet<record::ForestSlot>,
    ) -> std::result::Result<(), KvError> {
        let forest = self.forest().ok_or_else(|| {
            KvError::Corrupt(format!("{}: not a forest volume", self.path.display()))
        })?;
        let mint = forest::MintContext {
            cache: &self.cache,
            seq: &self.seq_handle(),
            alloc: &self.alloc,
            floor,
            policy: forest::MintPolicy::Recovery,
            region: None,
        };
        let mut interior: Vec<(u8, u64, &Record)> = Vec::new();
        for entry in &rec.entries {
            for (tag, r) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if tree_id == record::KIND_INTERIOR && level > 0 {
                    interior.push((level, entry.seq, r));
                }
            }
        }
        interior.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.seq.cmp(&b.2.seq)));
        for (level, _entry_start, r) in interior {
            let (slot, separator) = forest::split_interior_journal_key(&r.key)?;
            if absorbed.contains(&slot) {
                continue;
            }
            if r.kind == RecordKind::Put {
                if let Ok((_addr, child_seq)) = decode_interior_value(&r.value) {
                    self.seq_handle().raise_to(child_seq);
                }
            }
            let tree = forest.slot_or_mint(slot, &mint).await?;
            tree.apply_replayed_interior_recovery(
                separator,
                level,
                r.seq,
                r.kind,
                Bytes::copy_from_slice(&r.value),
                floor,
            )
            .await?;
        }
        for entry in &rec.entries {
            for (tag, r) in &entry.records {
                let (tree_id, level) = untag(*tag);
                if level > 0 || !record::is_slot_tree_kind(tree_id) {
                    continue;
                }
                if record::forest_key_slot(&r.key).is_ok_and(|s| absorbed.contains(&s)) {
                    continue;
                }
                let (_slot, tree) = forest.route_forest_key_or_mint(&r.key, &mint).await?;
                tree.apply_replayed_recovery(
                    &r.key,
                    r.seq,
                    r.kind,
                    Bytes::copy_from_slice(&r.value),
                    floor,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Dirty, non-superseded nodes stamped with one of `slots`.
    fn dirty_nodes_of_slots(&self, slots: &[record::ForestSlot]) -> usize {
        let mut n = 0usize;
        self.cache.for_each_node(|node| {
            if node.dirty_floor() != u64::MAX
                && !node.state().is_superseded()
                && node.forest_slot().is_some_and(|s| slots.contains(&s))
            {
                n += 1;
            }
        });
        n
    }

    /// The tails a recovery records for `tree`: every reachable leaf on
    /// a non-PR substrate (the dead lessee may append to ANY leaf it ever
    /// loaded — a non-resident leaf costs one extent read, counted on
    /// `recovery_full_tail_scan_bytes`); the RESIDENT leaves alone under a
    /// device fence (the preempt is the fence; the design skips the scan).
    async fn recovery_leaf_tails(
        &self,
        tree: &KvTree,
        pr_fenced: bool,
    ) -> std::result::Result<Vec<(u64, u32)>, KvError> {
        let node_size = self.cache.config().layout.node_size() as u64;
        let mut out = Vec::new();
        for addr in self.leaf_addrs_unloaded(tree).await? {
            let resident = self.cache.try_get(addr).is_some();
            if pr_fenced && !resident {
                continue;
            }
            if !resident {
                RECOVERY_FULL_TAIL_SCAN_BYTES.fetch_add(node_size, Ordering::Relaxed);
            }
            let Some(tail) = self.cache.peek_tail_offset(addr).await? else {
                continue;
            };
            out.push((addr, u32::try_from(tail).unwrap_or(u32::MAX)));
        }
        Ok(out)
    }

    /// Every leaf address of `tree` WITHOUT loading a leaf: the interior
    /// population is walked (a level-1 node's children are the leaves),
    /// so a non-resident leaf costs the tail peek's one extent read and
    /// never a materialized node (`reachable_node_addrs` is C13's — it
    /// loads everything, which would make every leaf resident and the
    /// scan's ledger read 0).
    async fn leaf_addrs_unloaded(&self, tree: &KvTree) -> std::result::Result<Vec<u64>, KvError> {
        Ok(self.node_addrs_unloaded(tree).await?.leaves)
    }

    /// Every node address `tree` reaches — interior AND leaf — without
    /// loading a leaf ([`Self::leaf_addrs_unloaded`]'s walk, both
    /// populations kept): the dead-appender orphan census's reachable set.
    async fn node_addrs_unloaded(&self, tree: &KvTree) -> std::result::Result<TreeAddrs, KvError> {
        let root = tree.root();
        let root_node = match self.cache.try_get(root.addr) {
            Some(n) => n,
            None => self
                .cache
                .load_for_slot(root.addr, tree.forest_slot())
                .await?
                .ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "{}: slot tree root {:#x} is on a retired extent",
                        self.path.display(),
                        root.addr
                    ))
                })?,
        };
        if root_node.level() == 0 {
            return Ok(TreeAddrs {
                interior: Vec::new(),
                leaves: vec![root.addr],
            });
        }
        let mut leaves = Vec::new();
        let mut interior = Vec::new();
        let mut frontier: Vec<(u64, u8)> = vec![(root.addr, root_node.level())];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((addr, level)) = frontier.pop() {
            if !seen.insert(addr) {
                continue;
            }
            interior.push(addr);
            let node = match self.cache.try_get(addr) {
                Some(n) => n,
                None => self
                    .cache
                    .load_for_slot(addr, tree.forest_slot())
                    .await?
                    .ok_or_else(|| {
                        KvError::Corrupt(format!(
                            "{}: interior node {addr:#x} is on a retired extent",
                            self.path.display()
                        ))
                    })?,
            };
            let snap = node.snapshot();
            let mut cursor: Vec<u8> = Vec::new();
            while let Some((key, ptr)) = snap.next_live(&cursor, None)? {
                let (child, _seq) = crate::meta_backend::kv::tree::decode_interior_value(&ptr)?;
                if level == 1 {
                    leaves.push(child);
                } else {
                    frontier.push((child, level - 1));
                }
                cursor.clear();
                cursor.extend_from_slice(&key);
                cursor.push(0);
            }
        }
        leaves.sort_unstable();
        leaves.dedup();
        Ok(TreeAddrs { interior, leaves })
    }

    /// A PARKING-free admission for a control entry while the caller holds
    /// the SMO mutex: `try_admit`, and on a full ring one barriered cycle
    /// (which advances `reusable_upto`) then again — `COVER_CYCLES_MAX`
    /// cycles, then the wedge class (a ring no cycle drains is pinned by
    /// something no flush discharges).
    async fn admit_control_drain_and_retry(
        &self,
        recs: &[(u8, Record)],
        smo: &mut SmoContext,
    ) -> std::result::Result<EntryAdmission, KvError> {
        let len = entry_len_for(recs)?;
        let tail_start = self.ring.core().reusable_upto();
        for cycle in 0..=checkpoint::COVER_CYCLES_MAX {
            if let Some(adm) = self.ring.try_admit(len, AdmissionClass::User) {
                return Ok(EntryAdmission::Held(adm));
            }
            if cycle == checkpoint::COVER_CYCLES_MAX {
                break;
            }
            self.checkpoint_cycle(smo, true).await?;
        }
        // The bound's class is the tail's (PR 4 round 6's law for the
        // grant's clearing loop; review round 2, Issue 27): a tail that
        // MOVED is a busy ring the poll's next projection retries —
        // `Busy`, the `deferred` arm's word; a tail that did not move in
        // `COVER_CYCLES_MAX` barriered cycles is pinned by something no
        // flush discharges — a defect, never a longer wait.
        let tail = self.ring.core().reusable_upto();
        if tail != tail_start {
            return Err(KvError::Busy(format!(
                "{}: a {len} B control entry found no ring-0 admission in {} barriered cycles \
                 (tail {tail_start} → {tail}) — the ring is busy; the next ledger poll retries",
                self.path.display(),
                checkpoint::COVER_CYCLES_MAX
            )));
        }
        Err(KvError::Corrupt(format!(
            "{}: a {len} B control entry found no ring-0 admission in {} barriered cycles with \
             the tail stuck at {tail} — the ring is pinned by something no flush discharges",
            self.path.display(),
            checkpoint::COVER_CYCLES_MAX
        )))
    }

    /// The tree-0 step of one recovery (the leave's per-region body for a
    /// DEAD appender): per slot its `Unleased` put with the post-flush
    /// root, its tails record (the prior spill retired), and the grant
    /// record minus the trees' images — packed by the entry cap.
    async fn release_recovered_slots(
        &self,
        plane: &slot_lease::SlotLeasePlane,
        appender_id: u32,
        released: &[(record::ForestSlot, u32, crate::slot_lease_core::SlotWords)],
        tails_by_slot: Vec<(record::ForestSlot, Vec<(u64, u32)>)>,
        custody_quarantine_until: Option<u64>,
        smo: &mut SmoContext,
    ) -> std::result::Result<(), KvError> {
        if released.is_empty() {
            return Ok(());
        }
        let forest = self.forest().ok_or_else(|| {
            KvError::Corrupt(format!("{}: not a forest volume", self.path.display()))
        })?;
        let last_written = self.lease_seq();
        let tag = journal::tag_for(record::TREE_CONTROL, 0);
        let mut staged: Vec<LeaveSlot> = Vec::with_capacity(released.len());
        for (slot, g, words) in released {
            // The root as flushed (the moved root's publication may have
            // rewritten the RAM record; the table holds the release's).
            let root = forest.tree(*slot).map_or(
                RootPtr {
                    addr: words.root.0,
                    seq: words.root.1,
                },
                |t| t.root(),
            );
            let words = crate::slot_lease_core::SlotWords {
                root: (root.addr, root.seq),
                ..*words
            };
            let mut recs: Vec<(u8, Record)> = vec![(
                tag,
                Record::put(
                    slot_state::slot_state_key(*slot),
                    0,
                    slot_state::SlotState::Unleased {
                        root,
                        cursor: words.cursor,
                        g: *g,
                        slot_tree_extents: words.extents,
                        last_written,
                        seq_floor: words.seq_floor,
                    }
                    .encode(),
                ),
            )];
            // The custody quarantine's DURABLE word (Issue 36): beside the
            // slot's `Unleased`, in the same entry — a manager restart
            // inside the window re-derives the RAM quarantine from it at
            // its arm.
            if let Some(until) = custody_quarantine_until {
                recs.push((
                    tag,
                    Record::put(
                        slot_state::custody_quarantine_key(*slot),
                        0,
                        slot_state::encode_custody_quarantine(until),
                    ),
                ));
            }
            let tails = tails_by_slot
                .iter()
                .find(|(s, _)| s == slot)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let step = async {
                let spilled = self.stage_slot_tails(*slot, *g, tails, &mut recs).await?;
                let prior = match self.retire_slot_tails_spill(*slot, &mut recs).await {
                    Ok(v) => v,
                    Err(e) => {
                        self.release_spill_claims(&spilled);
                        return Err(e);
                    }
                };
                let images = match self.slot_tree_image_extents(*slot).await {
                    Ok(v) => v,
                    Err(e) => {
                        self.release_spill_claims(&spilled);
                        return Err(e);
                    }
                };
                Ok::<_, KvError>((spilled, prior, images))
            }
            .await;
            let (spilled, prior, images) = match step {
                Ok(v) => v,
                Err(e) => {
                    for s in &staged {
                        self.release_spill_claims(&s.spilled);
                    }
                    return Err(e);
                }
            };
            let payload_len = recs
                .iter()
                .map(|(_, r)| record_frame_len(r.key.len(), r.value.len()))
                .sum();
            staged.push(LeaveSlot {
                slot: *slot,
                g: *g,
                words,
                recs,
                payload_len,
                spilled,
                prior,
                images,
            });
        }
        let release_from = |from: usize| {
            for s in &staged[from..] {
                self.release_spill_claims(&s.spilled);
            }
        };
        let mut record = match self.extent_grant_record(appender_id).await {
            Ok(r) => r,
            Err(e) => {
                release_from(0);
                return Err(e);
            }
        };
        let payloads: Vec<u64> = staged.iter().map(|s| s.payload_len).collect();
        let mut overhead_err: Option<KvError> = None;
        let chunks = journal::pack_entries(&payloads, |range| {
            match Self::leave_chunk_rewrite(&record, appender_id, &staged[range]) {
                Ok(p) => p
                    .put
                    .as_ref()
                    .map_or(0, |(_, r)| record_frame_len(r.key.len(), r.value.len())),
                Err(e) => {
                    overhead_err = Some(e);
                    u64::MAX
                }
            }
        });
        if let Some(e) = overhead_err {
            release_from(0);
            return Err(e);
        }
        for range in chunks {
            let chunk = &staged[range.clone()];
            let rewrite = match Self::leave_chunk_rewrite(&record, appender_id, chunk) {
                Ok(r) => r,
                Err(e) => {
                    release_from(range.start);
                    return Err(e);
                }
            };
            let mut recs: Vec<(u8, Record)> = chunk.iter().flat_map(|s| s.recs.clone()).collect();
            recs.extend(rewrite.put);
            // The recovery holds the SMO mutex, so the entry's admission is
            // drain-and-retry (the checkpoint task's own law — §4.4 pt 5):
            // a ring 0 full of user windows is cycled by THIS task, never
            // parked on (the checkpoint task would wait on the mutex we
            // hold). Bounded; the bound is the wedge class.
            let admission = match self.admit_control_drain_and_retry(&recs, smo).await {
                Ok(a) => a,
                Err(e) => {
                    release_from(range.start);
                    return Err(e);
                }
            };
            if let Err(e) = self.write_control_entry(recs, admission).await {
                release_from(range.start);
                return Err(e);
            }
            for s in chunk {
                self.release_spill_claims(&s.prior);
                if s.words.root.0 != 0 {
                    forest.note_published(
                        s.slot,
                        RootPtr {
                            addr: s.words.root.0,
                            seq: s.words.root.1,
                        },
                    );
                }
                // The frame screen's per-slot tails cache is stale for the
                // generation just recorded.
                let _ = plane.frame_tails.remove_sync(&s.slot);
            }
            record = rewrite.record;
        }
        Ok(())
    }

    /// **The own-residue POOL census** (PR 13g review round 1, Issue 2 —
    /// the death path's orphan census run for THIS mount's OWN regions at
    /// its open): `RegionGrant::recover` lands every record extent the
    /// page does not name as CLAIMED, and since PR 13g the page names the
    /// pool's largest `GRANT_RUNS_MAX` runs alone — a crash-rejoin's
    /// unnamed pool (the recycled singles, every run past the four
    /// largest: tens to a hundred-plus extents per crash per region on a
    /// fragmented pool) read claimed, routed to by nothing, freed by
    /// nothing, a C13 candidate only at an fsck run at the joiner, its
    /// repair gated. PR 3 review round 1 Issue 9 had made the page name
    /// EVERY unclaimed extent so no crash-class open recovered a pool as
    /// claimed; the pool reverses it, and this census is what closes the
    /// class again: a claimed extent no tree of this mount reaches
    /// (`node_addrs_unloaded` — never a leaf load) and no in-window claim
    /// named (the replay's `claim_exact` — a live image whose root install
    /// the window carries) is the pool — back to UNCLAIMED, counted on
    /// `appender_pool_restored_extents`. Runs for own regions that
    /// RECOVERED own residue; a clean leave leaves nothing pooled. Under
    /// the SMO mutex and the forest's mint guard (C13's own posture: an
    /// SMO's successor before its route flip and a lazy mint's root before
    /// the forest names it are never candidates) — at the open both are
    /// free.
    pub(in crate::meta_backend::kv) async fn restore_own_pools(
        &self,
    ) -> std::result::Result<(), KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(());
        };
        let Some(forest) = self.forest() else {
            return Ok(());
        };
        let own: Vec<Arc<AppenderRegion>> = set
            .own_regions()
            .filter(|r| r.id != 0 && r.self_recovered)
            .cloned()
            .collect();
        if own.is_empty() {
            return Ok(());
        }
        let _smo = self.smo.lock().await;
        let _mint = forest.mint_guard().await;
        let mut reachable: Option<std::collections::BTreeSet<u64>> = None;
        for region in own {
            let (claimed, window) = {
                let mut g = region.grant();
                (g.claimed_extents(), g.take_window_claims())
            };
            if claimed.is_empty() {
                continue;
            }
            // Every tree this mount holds, walked once for all regions: a
            // projection's tree can only KEEP an extent claimed (the leak
            // direction, C13's), never restore a live one to the pool.
            if reachable.is_none() {
                let mut set = std::collections::BTreeSet::new();
                for t in self.all_trees() {
                    for addr in self.node_addrs_unloaded(&t).await?.iter() {
                        set.insert(self.cache.addr_extent(addr));
                    }
                }
                reachable = Some(set);
            }
            let reachable = reachable.as_ref().expect("computed above");
            let mut restored = 0u64;
            {
                let mut g = region.grant();
                for e in claimed {
                    if !reachable.contains(&e) && !window.contains(&e) && g.unclaim(e) {
                        restored += 1;
                    }
                }
            }
            if restored > 0 {
                set.pool_restored_extents
                    .fetch_add(restored, Ordering::Relaxed);
                log::info!(
                    "meta volume {}: appender {}'s own-residue open restored {restored} pool \
                     extent(s) the page could not name — claimed by no tree, back to unclaimed \
                     (appender_pool_restored_extents)",
                    self.path.display(),
                    region.id
                );
            }
        }
        Ok(())
    }

    /// The image extents dead appender `id`'s grant record still claims
    /// that no tree root reaches and no `alloc_lease:` record names —
    /// C13's class for a dead appender, returned by the recoverer (its
    /// ring is `Recovered` from here, so no in-window `alloc` is ever
    /// judged against the rewritten record).
    ///
    /// **Scoped and non-materializing** (review round 1, Issue 10): a
    /// grant's images can be reached only by the trees the appender wrote
    /// — the slots it leased (`slots`, just released) — so the census walks
    /// THOSE trees' interior population (one read per interior node; a
    /// leaf's address is read off its parent, never the leaf) and never the
    /// whole forest under the SMO mutex. The bound
    /// `appender_recovery_bound_ms` prices it as the flush's leaves ÷ fan-out.
    async fn dead_appender_orphans(
        &self,
        vol0: &Arc<KvMetaBackend>,
        id: u32,
        identity: &AppenderIdentity,
        slots: &[record::ForestSlot],
    ) -> std::result::Result<Vec<u64>, KvError> {
        let record = self.extent_grant_record(id).await?;
        if record.is_empty() {
            return Ok(Vec::new());
        }
        let Some(forest) = self.forest() else {
            return Ok(Vec::new());
        };
        let _mint = forest.mint_guard().await;
        // The law (PR 12b round 3, F1): an extent ANY slot-tree root
        // reaches is never returned — the released slots' trees AND every
        // other slot tree this volume holds (the dead appender's record
        // can claim images of a slot it released BEFORE dying — a wire
        // release whose image walk ran over a stale tree left them
        // claimed — and of a slot the manager maintains unleased). Walking
        // a tree that is a projection here can only KEEP an extent claimed
        // (the leak direction, C13's), never return a live one.
        let mut reachable: std::collections::BTreeSet<u64> = Default::default();
        let mut walked: std::collections::BTreeSet<record::ForestSlot> = Default::default();
        for slot in slots {
            let Some(t) = forest.tree(*slot) else {
                continue;
            };
            walked.insert(*slot);
            for addr in self.node_addrs_unloaded(&t).await?.iter() {
                reachable.insert(self.cache.addr_extent(addr));
            }
        }
        for t in self.all_trees() {
            let Some(slot) = t.forest_slot() else {
                continue;
            };
            if !walked.insert(slot) {
                continue;
            }
            for addr in self.node_addrs_unloaded(&t).await?.iter() {
                reachable.insert(self.cache.addr_extent(addr));
            }
        }
        let node_size = self.cache.config().layout.node_size() as u64;
        let mut named: std::collections::BTreeSet<u64> = Default::default();
        for (_, lease) in vol0.alloc_lease_records().await? {
            if !same_mount(&lease.holder, identity) {
                continue;
            }
            for (_, e) in &lease.bitmap {
                let mut off = e.start;
                while off < e.end() {
                    named.insert(self.cache.addr_extent(off));
                    off += node_size;
                }
            }
        }
        Ok(record
            .extents()
            .filter(|e| !reachable.contains(e) && !named.contains(e))
            .collect())
    }

    /// Whether `key` is one THIS process stands on — the metadata guard's
    /// own key or any hold in the data-plane registry (a holder's, a
    /// registrant's, an ADOPTED co-located hold's — that last word is the
    /// manager's own key seen from a joiner). The death ledger's preempt
    /// never drives such a key (§5.9 step 1 under KD-SYM-22).
    fn is_own_registrant_key(&self, key: u64) -> bool {
        key != 0
            && (key == self.pr_key || crate::data_custody::own_registrant_keys().contains(&key))
    }

    /// Preempt `victim` on this volume's metadata namespace under the
    /// manager's standing WERO (rtype 3 — PR 3): the device rejects the
    /// dead appender's writes from here. Namespaces preempted (0 or 1;
    /// 0 without a reservation client, without WERO, or on a failed
    /// preempt — logged).
    async fn preempt_meta_registrant(&self, victim: u64) -> u64 {
        if !self.meta_wero || !self.pr_active.load(Ordering::Acquire) || self.pr_key == 0 {
            return 0;
        }
        let Some(rsv) = self.reservations.as_ref() else {
            return 0;
        };
        let key = self.pr_key;
        // RACQA 2 — PREEMPT AND ABORT (design §5.9 step 1; review round 1,
        // Issue 16): the victim's in-flight commands go with its
        // registration.
        match rsv_call(rsv, move |c| {
            c.preempt_and_abort_registrants_only(key, victim)
        })
        .await
        {
            Ok(()) => {
                log::warn!(
                    "meta volume {}: PREEMPTED AND ABORTED dead registrant key {victim:#x} under \
                     the manager's WERO — the device rejects its writes from here",
                    self.path.display()
                );
                1
            }
            Err(e) => {
                log::warn!(
                    "meta volume {}: preempt of dead registrant key {victim:#x} failed ({e}); \
                     the frame screen's full tail scan is the fence for this recovery",
                    self.path.display()
                );
                0
            }
        }
    }

    /// **The dead MANAGER's own lock** (§5.6.4's expiry law at a manager
    /// failover — PR 3's successor arm composed): volume 0's manager IS
    /// appender 0, so a `dir_rename` record naming appender 0 at a writer
    /// era BELOW this manager's was taken by a predecessor incarnation
    /// the D0 ladder replaced — dead by D0's proof — and is released
    /// here, at the successor's arm. `Ok(true)` = released.
    pub async fn release_stale_manager_dir_rename(&self) -> std::result::Result<bool, KvError> {
        let Some(rec) = self.dir_rename_record().await? else {
            return Ok(false);
        };
        if rec.holder != self.own_appender_id() || rec.term >= self.writer_term() {
            return Ok(false);
        }
        self.manager_dir_rename_release_dead(rec.holder).await
    }

    /// Test seam: PLANT a `dir_rename` record as a dead manager incarnation
    /// leaves it — `holder` at writer era `term` — without the in-process
    /// RAII lease (whose `Drop` would release it; a held lease keeps the
    /// backend alive through the kill). The failover contract's fixture.
    pub async fn test_plant_dir_rename_record(
        &self,
        holder: u32,
        term: u64,
    ) -> std::result::Result<(), KvError> {
        let rec = slot_state::DirRenameRecord {
            holder,
            term,
            since_ns: crate::meta_backend::kv::alloc_lease::unix_now_ms() * 1_000_000,
        };
        self.write_control_entry(
            vec![(
                journal::tag_for(record::TREE_CONTROL, 0),
                Record::put(slot_state::DIR_RENAME_KEY.to_vec(), 0, rec.encode()),
            )],
            EntryAdmission::Try,
        )
        .await?;
        self.sync_device().await.map_err(KvError::Io)
    }

    /// **The `Recovered`-until-released law** (§5.5.1): a `Recovered`
    /// page whose identity no `alloc_lease:` record names as a holder
    /// homed on this volume is RELEASED — its ring extents return to the
    /// heap (no journal record ever references a ring extent; the page is
    /// its only reference) and the page goes `Free`, id and term kept,
    /// the ring's final head kept as the region's seq-space watermark.
    /// Returns the regions released.
    pub async fn release_recovered_regions(
        self: &Arc<Self>,
        vol0: &Arc<KvMetaBackend>,
        vol_ordinal: u16,
    ) -> std::result::Result<u64, KvError> {
        let Some(set) = self.appenders.as_ref() else {
            return Ok(0);
        };
        if self.read_only
            || self.non_writer
            || self.is_failed()
            || !set.joined.load(Ordering::Acquire)
            || *set.manager_lease.lock().unwrap_or_else(|e| e.into_inner())
                != appender::ManagerLease::Held
        {
            return Ok(0);
        }
        let leases = vol0.alloc_lease_records().await?;
        let dead = vol0.dead_member_records().await?;
        let entries = read_directory(&self.path, &self.sb).await?;
        let node_size = u64::from(self.sb.node_size);
        let mut released = 0u64;
        for e in &entries {
            let Some(mut page) = e.page.clone() else {
                continue;
            };
            if page.state != AppenderState::Recovered || set.region(e.appender_id).is_some() {
                continue;
            }
            if leases
                .iter()
                .any(|(_, l)| same_mount(&l.holder, &page.identity) && l.home_vol == vol_ordinal)
            {
                continue; // its bitmap pages are still the lease's
            }
            // The `recovered:` record precedes the release (§5.5.1's
            // ordering): a `Recovered` page the ledger names whose record
            // is missing is the recoverer's last window — the driver's
            // completion arm writes the record first, the release follows.
            if dead.iter().any(|(id, _)| same_mount(id, &page.identity))
                && vol0
                    .recovered_record(&page.identity, vol_ordinal)
                    .await?
                    .is_none()
            {
                continue;
            }
            let _handover = self.handover.lock().await;
            let segments = std::mem::take(&mut page.segments);
            page.state = AppenderState::Free;
            page.grant.clear();
            page.slots.clear();
            self.write_foreign_page(e, &mut page).await?;
            for ext in &segments {
                let mut off = ext.start;
                while off < ext.end() {
                    self.alloc
                        .release_unpublished((off - self.sb.heap.start) / node_size);
                    off += node_size;
                }
            }
            // The bits + the ledger record the consumed seq names (PR 13,
            // defect 25 — a reader's poll stops on a ledger gap), under the
            // SMO mutex (handover → SMO, the recovery's own order) so no
            // cycle is mid-flight while the roots are restated.
            {
                let _smo = self.smo.lock().await;
                let _hold = self.recovery_hold();
                self.consume_checkpoint_seq_for_bitmap().await?;
            }
            released += 1;
            RECOVERED_REGIONS_RELEASED.fetch_add(1, Ordering::Relaxed);
            log::info!(
                "meta volume {}: recovered appender {}'s region RELEASED — no allocation lease \
                 names it; {} ring extent(s) returned, page Free",
                self.path.display(),
                e.appender_id,
                segments.len()
            );
        }
        Ok(released)
    }

    /// **`squeezefs appender clear <sqmeta-uri> <id>`** — the attested
    /// offline remedy (design §6.2; the `claim clear` law): the operator
    /// attests that appender `appender_id` on `path` is DEAD. Refuses a
    /// live-mounted volume (the flock), a fresh writer claim or client
    /// registration of the appender's node (it heartbeated inside the
    /// TTL and may be alive), and this node's own page (own residue — the
    /// next mount recovers it itself). Otherwise the death record lands in
    /// tree 0 of `vol0_path` (the ledger — idempotent) and the page is
    /// marked `Recovering` (its `Live` slot entries stop contending at the
    /// next mount's C14 settle); the next mount recovers it before serving
    /// (C15). Audited (`appender_clear_runs`).
    pub async fn appender_clear(
        path: &Path,
        vol0_path: &Path,
        appender_id: u32,
    ) -> std::result::Result<AppenderClearOutcome, KvError> {
        let guard_fd = match Self::acquire_writer_flock(path) {
            Ok(fd) => fd,
            Err(FlockOutcome::Held) => {
                return Err(KvError::Busy(format!(
                    "{}: refusing to clear an appender — the volume is live-mounted on this \
                     host (the writer lock is held). Unmount it first",
                    path.display()
                )));
            }
            Err(FlockOutcome::Io(e)) => {
                return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
            }
        };
        let vol0_guard = if vol0_path != path {
            match Self::acquire_writer_flock(vol0_path) {
                Ok(fd) => Some(fd),
                Err(FlockOutcome::Held) => {
                    return Err(KvError::Busy(format!(
                        "{}: refusing to clear an appender — volume 0 ({}) is live-mounted on \
                         this host. Unmount it first",
                        path.display(),
                        vol0_path.display()
                    )));
                }
                Err(FlockOutcome::Io(e)) => {
                    return Err(KvError::Io(crate::error::SqueezefsError::Io(e)));
                }
            }
        } else {
            None
        };
        // The `claim clear` law over EVERY volume the verb touches (review
        // round 1, Issue 6): a fresh `writer_claim` on `path` OR on volume
        // 0 — ANY appender's manager, this host's or a foreign one — is a
        // live manager the verb would write under; refused from a probe
        // read BEFORE either writer open (nothing of the verb's is opened
        // when it refuses). The first build gated this on the cleared
        // page being the manager's, and never read volume 0's claim.
        let now = unix_now_secs() + TEST_CLAIM_CLOCK_SKEW_SECS.load(Ordering::Relaxed);
        let mut claim_volumes: Vec<&Path> = vec![path];
        if vol0_path != path {
            claim_volumes.push(vol0_path);
        }
        // The manager's claim age (volume 0's) — the park bound below reads
        // it: `None` = no claim ever written.
        let mut manager_claim_age_secs: Option<u64> = None;
        for p in claim_volumes {
            let probe = Self::open_inner(p, OpenPosture::NonWriter, None).await?;
            if let Ok(Some(raw)) = probe.getxattr(1, WRITER_CLAIM_XATTR).await {
                if let Some(c) = WriterClaim::decode(&raw) {
                    if p == vol0_path {
                        manager_claim_age_secs = Some(c.age_secs(now));
                    }
                    if c.age_secs(now) <= crate::fuse_client::CLIENT_STALE_TTL_SECS {
                        return Err(KvError::Busy(format!(
                            "{}: refusing to clear appender {appender_id} — {}'s writer claim \
                             (holder '{}') heartbeated {}s ago, inside the {}s TTL: a live \
                             manager holds the set. Stop it (or wait for the TTL), then retry",
                            path.display(),
                            p.display(),
                            c.id,
                            c.age_secs(now),
                            crate::fuse_client::CLIENT_STALE_TTL_SECS
                        )));
                    }
                }
            }
        }
        let mut inner = Self::open_inner(path, OpenPosture::Writer, None).await?;
        *inner.guard_fd.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(guard_fd);
        let be = Arc::new(inner);
        let _ = be.conveyor_self.set(Arc::downgrade(&be));
        let entries = read_directory(path, &be.sb).await?;
        let Some(entry) = entries.iter().find(|e| e.appender_id == appender_id) else {
            return Ok(AppenderClearOutcome::NothingToClear);
        };
        let Some(page) = entry.page.clone() else {
            return Ok(AppenderClearOutcome::NothingToClear);
        };
        if !matches!(page.state, AppenderState::Live | AppenderState::Recovering) {
            return Ok(AppenderClearOutcome::NothingToClear);
        }
        let set = be.appenders.as_ref().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: not a symmetric-forest volume (bit 17 absent) — no appender directory \
                 exists",
                path.display()
            ))
        })?;
        // The manager's page under this node, or any page under this exact
        // mount identity, is own residue the next mount recovers itself; a
        // same-node JOINED appender's page (PR 12b) is another daemon's —
        // attestable once the flock-free liveness probes below pass.
        if super::super::appender::page_is_own(
            appender_id,
            &page.identity,
            set.identity.node_token,
            set.identity.mount_slot,
            true,
        ) {
            return Err(KvError::Busy(format!(
                "{}: appender {appender_id}'s page is this node's OWN residue (node {:#018x}, \
                 mount slot {:#x}) — the next mount of this node recovers it itself; nothing to \
                 attest",
                path.display(),
                page.identity.node_token,
                page.identity.mount_slot
            )));
        }
        // **The parked-joiner bound** (PR 12b review round 1, Issue 4(ii)):
        // a JOINED appender writes no `client:` heartbeat (PR 13's), so on
        // a MANAGER-LESS set the probes below cannot tell a live joiner
        // PARKED at `T_self` (PR 8's law — it reclaims against the
        // successor the ledger names for up to `T_park_max`, then poisons
        // itself: `appender_park_expiries`) from a dead one. Inside that
        // window after the manager's claim went stale the verb REFUSES —
        // clearing a live parked writer's page would hand its ring to a
        // successor's recovery while it still holds acked custody. Past it
        // the joiner is dead or self-fenced by derivation, and the verb's
        // other probes govern. The bound is DERIVED (`park_gate::
        // t_park_max_for` over the volume's failover bound), never a knob.
        if !super::super::appender::page_is_own(
            appender_id,
            &page.identity,
            set.identity.node_token,
            set.identity.mount_slot,
            true,
        ) && appender_id != 0
        {
            if let Some(age) = manager_claim_age_secs {
                let failover_ms = be.appender_stats().map_or(0, |s| s.failover_bound_ms);
                let park_max_ms =
                    match crate::membership::LeaseClocks::derive(std::time::Duration::ZERO) {
                        Ok(clocks) => crate::park_gate::t_park_max_for(failover_ms, &clocks),
                        Err(_) => failover_ms,
                    };
                let stale_for_ms = age
                    .saturating_sub(crate::fuse_client::CLIENT_STALE_TTL_SECS)
                    .saturating_mul(1000);
                if stale_for_ms < park_max_ms {
                    return Err(KvError::Busy(format!(
                        "{}: refusing to clear appender {appender_id} — the set's manager claim                          went stale only {stale_for_ms} ms ago and a LIVE joined appender parked                          at T_self reclaims against the successor for up to T_park_max =                          {park_max_ms} ms before it fences itself (appender_park_expiries); a                          clear inside that window would recover a live writer's ring. Mount the                          successor (its death ledger recovers what is dead) or retry after the                          bound",
                        path.display()
                    )));
                }
            }
        }
        // The liveness probe an offline verb has (the writer claims above,
        // and) a fresh `client:` registration naming the appender's node
        // inside the TTL — it may be alive.
        let member =
            crate::cowriter::node_member_id_of(page.identity.node_token, page.identity.mount_slot);
        if let Ok(attrs) = be.listxattr(1).await {
            for k in attrs.iter().filter(|k| k.starts_with("client:")) {
                if !k[7..].starts_with(&member) {
                    continue;
                }
                if let Ok(Some(val)) = be.getxattr(1, k).await {
                    let fresh = serde_json::from_slice::<serde_json::Value>(&val)
                        .ok()
                        .and_then(|v| v.get("ts")?.as_u64())
                        .is_some_and(|ts| {
                            now.saturating_sub(ts) <= crate::fuse_client::CLIENT_STALE_TTL_SECS
                        });
                    if fresh {
                        return Err(KvError::Busy(format!(
                            "{}: refusing to clear appender {appender_id} — its node ({member}) \
                             registered a client heartbeat inside the {}s TTL and may be alive. \
                             Stop that mount (or wait for the TTL), then retry",
                            path.display(),
                            crate::fuse_client::CLIENT_STALE_TTL_SECS
                        )));
                    }
                }
            }
        }
        let window = be.window_entries_of(&page).await.unwrap_or(0);
        // The death record — on volume 0's tree 0 (this open when `path`
        // IS volume 0, a guarded open of volume 0 otherwise).
        // The operator's attestation is an EARLY recorder (Issue 34): it
        // may run inside a surviving custody writer's `T_self`, so the
        // recovery quarantines fresh custody grants on the recovered slots
        // until `ts_ms + T_self`.
        let dead = DeadMemberRecord {
            epoch: 0,
            ts_ms: crate::meta_backend::kv::alloc_lease::unix_now_ms(),
            pr_key: 0,
            early: true,
        };
        let death_put = (
            journal::tag_for(record::TREE_CONTROL, 0),
            Record::put(
                crate::meta_backend::kv::alloc_lease::dead_member_key(&page.identity),
                0,
                dead.encode(),
            ),
        );
        if vol0_path == path {
            be.write_control_entry(vec![death_put], EntryAdmission::Try)
                .await?;
            be.sync_device().await.map_err(KvError::Io)?;
            be.checkpoint_now().await?;
        } else {
            let mut v0 = Self::open_inner(vol0_path, OpenPosture::Writer, None).await?;
            *v0.guard_fd.get_mut().unwrap_or_else(|e| e.into_inner()) = vol0_guard;
            let v0 = Arc::new(v0);
            let _ = v0.conveyor_self.set(Arc::downgrade(&v0));
            v0.write_control_entry(vec![death_put], EntryAdmission::Try)
                .await?;
            v0.sync_device().await.map_err(KvError::Io)?;
            v0.checkpoint_now().await?;
            v0.shutdown().await?;
        }
        let was = page.state;
        let mut page = page;
        page.state = AppenderState::Recovering;
        page.recovered_by_term = 0;
        be.write_foreign_page(entry, &mut page).await?;
        be.shutdown().await?;
        APPENDER_CLEAR_RUNS.fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "meta volume {}: appender {appender_id} (node {:#018x}, mount slot {:#x}) CLEARED by \
             operator attestation — death recorded on {}, page {} ⇒ recovering; the next mount \
             recovers its {window}-entry window before serving (appender_clear_runs)",
            path.display(),
            page.identity.node_token,
            page.identity.mount_slot,
            vol0_path.display(),
            was.as_str()
        );
        Ok(AppenderClearOutcome::Cleared {
            identity: page.identity,
            was,
            window_entries: window,
        })
    }
}

// ---------------------------------------------------------------------------
// The routed set: the writer's install, the mount-path arm, the poll
// ---------------------------------------------------------------------------

/// **THE volume-0 resolver** (review round 1, Issue 19 — one law for the
/// driver, fsck's C14/C15 census and every other reader of the set-wide
/// ledger): volume 0 is the volume the ROOT inode routes to — the slot
/// map's slot-0 member, `route_ino(1).0` — with its ordinal in the routed
/// set. The offline verbs (`appender clear`) take the same volume by the
/// set's canonical URI order, which `plan_meta_slot_set` makes the slot-0
/// member (`docs/operations.md`).
pub fn vol0_of(routed: &RoutedMetaBackend) -> Option<(usize, &Arc<KvMetaBackend>)> {
    let slot0 = routed.route_ino(1).0;
    routed.volumes.get(slot0).map(|v| (slot0, v))
}

/// **The death ledger's production WRITER**: a membership death sink
/// that ships every dead member the installed S6 owner declares to
/// volume 0's manager — this process's volume 0 (the wire form,
/// `ManagerClient::record_death`, is a peer manager's under PR 12's join
/// ladder). One record per death, idempotent; the poll acts on it.
pub fn install_death_ledger_writer(routed: &Arc<RoutedMetaBackend>) {
    let Some((_, vol0)) = vol0_of(routed) else {
        return;
    };
    let vol0 = Arc::downgrade(vol0);
    crate::membership::install_death_sink(Arc::new(move |dead: crate::membership::DeadMember| {
        let Some(vol0) = vol0.upgrade() else {
            return;
        };
        let Some((node_token, mount_slot)) = crate::cowriter::parse_node_member_id(&dead.id) else {
            return; // a reader's uuid: holds no region
        };
        let identity = AppenderIdentity {
            node_token,
            mount_slot,
            writer_id: 0,
        };
        crate::meta_exec::spawn_meta_contained("record_death", async move {
            match vol0
                .record_death_with_key(identity, dead.epoch, dead.pr_key)
                .await
            {
                Ok(already) => log::warn!(
                    "death ledger: member '{}' (node {node_token:#018x}, mount slot \
                     {mount_slot:#x}) recorded dead{} — every manager's next ledger poll \
                     recovers its regions",
                    dead.id,
                    if already { " (already)" } else { "" }
                ),
                Err(e) => {
                    // §5.5.2 "retried until durable" (review round 1, Issue
                    // 5): the admission itself PARKS on a full ring, so
                    // this arm is the device-error class — the death is
                    // parked in RAM and the next ledger poll retries it.
                    crate::meta_backend::kv::alloc_lease::defer_death_record(
                        crate::meta_backend::kv::alloc_lease::PendingDeath {
                            member: identity,
                            epoch: dead.epoch,
                            pr_key: dead.pr_key,
                        },
                    );
                    log::error!(
                        "death ledger: recording member '{}' dead FAILED ({e}); parked for the \
                         next ledger poll's retry (dead_member_write_deferrals)",
                        dead.id
                    );
                }
            }
        });
    }));
}

/// **The rejoin's retirement** (§5.5.2; review round 1, Issue 9): this
/// writer's own identity is the NEWER incarnation of anything the ledger
/// names dead under it — the record (and its `recovered:` records) are
/// retired on volume 0 before the set serves, so no manager's poll ever
/// recovers a region this mount is live on. Idempotent; `Ok(true)` = a
/// record was retired. (This mount's own `Live` / `Recovering` pages are
/// own residue, replayed at its open — PR 2.)
pub async fn retire_own_death_records(
    routed: &Arc<RoutedMetaBackend>,
) -> std::result::Result<bool, KvError> {
    let Some((_, vol0)) = vol0_of(routed) else {
        return Ok(false);
    };
    if vol0.read_only || vol0.non_writer || vol0.appender_stats().is_none() {
        return Ok(false);
    }
    let retired = vol0.retire_own_death_records().await?;
    if retired {
        log::warn!(
            "recovery: this mount's own death record on {} RETIRED at its arm — the newer \
             incarnation supersedes it (design-symmetric-metadata §5.5.2)",
            vol0.device_path().display()
        );
    }
    Ok(retired)
}

/// **One ledger projection across the set** (the mount-path C15 arm and
/// the poll's body): every manager this process holds reads tree 0 of
/// volume 0, recovers the regions the ledger names on its volume,
/// releases the `Recovered` regions no lease names, and rolls forward
/// every open intent whose home the recovered slots made local (the
/// dead initiator's — design §5.6 "roll-forward by whoever recovers the
/// initiator's ring"). The vol-0 rule's probe rides the same read.
pub async fn recover_dead_appenders_set(
    routed: &Arc<RoutedMetaBackend>,
) -> std::result::Result<RecoverySetReport, KvError> {
    let mut out = RecoverySetReport::default();
    let Some((slot0, vol0)) = vol0_of(routed) else {
        return Ok(out);
    };
    RECOVERY_LEDGER_POLLS.fetch_add(1, Ordering::Relaxed);
    let now_ms = crate::meta_backend::kv::alloc_lease::unix_now_ms();
    let t_owner_ms = crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000;
    let reachable = !vol0.is_failed() && vol0.dead_member_records().await.is_ok();
    for (i, vol) in routed.volumes.iter().enumerate() {
        if i != slot0 {
            vol.note_vol0_ledger_probe(reachable, now_ms, t_owner_ms);
        }
    }
    if !reachable {
        return Ok(out);
    }
    // The deaths the sink could not write (Issue 5) land first — they are
    // this projection's input.
    if vol0.appender_stats().is_some() {
        match vol0.drain_pending_deaths().await {
            Ok(n) => out.deaths_landed = n,
            Err(e) => log::warn!("recovery: the pending death records did not land ({e})"),
        }
    }
    let mut recovered_any = false;
    for (i, vol) in routed.volumes.iter().enumerate() {
        let ordinal = u16::try_from(i).unwrap_or(u16::MAX);
        // The release law runs BEFORE this projection's recoveries: a
        // region recovered now stays `Recovered` until the next
        // projection finds no lease naming it (one observable step per
        // state, and the successor's copy has a whole poll to land).
        out.regions_released += vol.release_recovered_regions(vol0, ordinal).await?;
        let report = vol.recover_dead_appenders(vol0, ordinal).await?;
        recovered_any |= !report.recovered.is_empty();
        out.per_volume.push((ordinal, report));
    }
    // The retirement sweep (§5.5.2): a record past `2 × T_owner` whose
    // regions on EVERY volume of this set are recovered (no `Live` /
    // `Recovering` page of the identity left) and that no lease names.
    if vol0.appender_stats().is_some() {
        let mut pending_pages: Vec<AppenderIdentity> = Vec::new();
        for vol in &routed.volumes {
            for e in read_directory(vol.device_path(), vol.superblock()).await? {
                if let Some(p) = e.page {
                    if matches!(p.state, AppenderState::Live | AppenderState::Recovering) {
                        pending_pages.push(p.identity);
                    }
                }
            }
        }
        let pending = |m: &AppenderIdentity| pending_pages.iter().any(|p| same_mount(p, m));
        match vol0
            .sweep_retirable_death_records(now_ms, t_owner_ms, &pending)
            .await
        {
            Ok(n) => out.records_retired = n,
            Err(e) => log::warn!("recovery: the death-record retirement sweep failed ({e})"),
        }
    }
    if recovered_any {
        match crate::meta_backend::crossvol_tx::roll_forward_open_intents(routed).await {
            Ok(n) => {
                out.intents_rolled_forward = n as u64;
                RECOVERY_INTENTS_ROLLED_FORWARD.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) => log::warn!(
                "recovery: rolling the recovered slots' open intents forward failed ({e}); the \
                 cadence retries"
            ),
        }
    }
    Ok(out)
}

/// The set's C14 / C15 verdict at the mount path (BEFORE serving): a
/// `Recovering` page the ledger does not name refuses the mount loud
/// (nothing legal writes that shape — `appender clear` is the remedy);
/// the C15 regions are recovered by [`recover_dead_appenders_set`].
pub async fn mount_path_custody_gate(
    routed: &Arc<RoutedMetaBackend>,
) -> std::result::Result<RecoverySetReport, KvError> {
    let Some((_, vol0)) = vol0_of(routed) else {
        return Ok(RecoverySetReport::default());
    };
    // A manager failover's composition (§5.9's last note): the successor
    // replayed the dead manager's ring at its open (PR 3); the set-wide
    // lock the dead incarnation held is released here.
    if let Err(e) = vol0.release_stale_manager_dir_rename().await {
        log::warn!(
            "recovery: releasing the predecessor manager's directory-rename lock failed ({e})"
        );
    }
    for vol in &routed.volumes {
        // The custody quarantines a previous incarnation of this manager
        // wrote (Issue 36): re-derived from tree 0 before the set serves,
        // expired ones retired — one range read per armed volume.
        vol.load_custody_quarantines().await?;
        let census = vol.slot_custody_census_opts(Some(vol0), false).await?;
        if let Some((id, identity)) = census.recovering_unledgered.first() {
            return Err(KvError::Corrupt(format!(
                "{}: appender {id}'s page is RECOVERING under node {:#018x} (mount slot {:#x}) \
                 while the death ledger does not name it — no recovery wrote this shape \
                 (design-symmetric-metadata §5.8.5 C14/C15); refusing the mount. Remedy: \
                 `squeezefs appender clear <sqmeta-uri> {id}` attests it dead and the next \
                 mount recovers its window",
                vol.device_path().display(),
                identity.node_token,
                identity.mount_slot
            )));
        }
    }
    recover_dead_appenders_set(routed).await
}

/// The ledger poll: one projection per `tick_ms` (the checkpoint landing
/// ceiling — §5.5.2's `poll + CHECKPOINT_MAX_AGE` propagation bound),
/// until the set is dropped.
pub fn spawn_ledger_poll(routed: std::sync::Weak<RoutedMetaBackend>, tick_ms: u64) {
    crate::meta_exec::spawn_meta_contained("sym_ledger_poll", async move {
        let tick = std::time::Duration::from_millis(tick_ms.max(1));
        loop {
            squeezefs_ipc::sqz_time::sleep(tick).await;
            let Some(routed) = routed.upgrade() else {
                return;
            };
            if routed.volumes.iter().all(|v| !v.slot_lease_armed()) {
                return;
            }
            if let Err(e) = recover_dead_appenders_set(&routed).await {
                log::warn!("recovery: the ledger poll failed ({e}); next tick");
            }
            // The deferred leak release's verdict rides the same cadence
            // (PR 12b round 5, Issue 25): a dead peer's page left `Live`
            // by the poll's recovery above, a live peer's declaration
            // landed on its renewal since the last tick — either moves
            // the pending set toward 0.
            crate::meta_backend::kv::alloc_lease::converge_deferred_leaks().await;
        }
    });
}

/// **The production arm** (the mount path, after the routed open and
/// before the set serves): the ledger writer installed, the C14/C15 gate
/// run (recovering every region the ledger names dead), the poll spawned.
/// Inert — one `slot_lease_armed` read per volume — on every unarmed
/// mount.
pub async fn arm(
    routed: &Arc<RoutedMetaBackend>,
) -> std::result::Result<RecoverySetReport, KvError> {
    if routed.volumes.iter().all(|v| !v.slot_lease_armed()) {
        return Ok(RecoverySetReport::default());
    }
    install_death_ledger_writer(routed);
    retire_own_death_records(routed).await?;
    let report = mount_path_custody_gate(routed).await?;
    spawn_ledger_poll(
        Arc::downgrade(routed),
        crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived(),
    );
    Ok(report)
}
