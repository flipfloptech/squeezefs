//! **Symmetric PR 10 — dead-appender recovery: the death ledger's driver**
//! (`docs/design-symmetric-metadata.md` §5.5.2, §5.5.3, §5.8.2–§5.8.5,
//! §5.9; KD-SYM-3/4/6/15).
//!
//! The ledger's WRITER is the home shard's eviction
//! ([`install_death_ledger_writer`]: a [`crate::membership::DeathSink`]
//! that ships `RecordDeath` to volume 0's manager — in-process the direct
//! call, `KvMetaBackend::record_death_with_key`). Every manager READS
//! tree 0 of volume 0 as a projection ([`spawn_ledger_poll`], and once at
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
    AppenderState, SlotEntryState,
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
    /// The measured journal entry per create (design §1.4, ≈ 230 B).
    const ENTRY_BYTES_PER_CREATE: u64 = 230;
    /// Files per 256 KiB leaf at ~330 B per inode + dentry + layout.
    const FILES_PER_256K_LEAF: u64 = 800;
    /// The upper end of the measured cold leaf read (85–120 µs).
    const LEAF_LOAD_US: u64 = 120;
    /// The per-entry fold cost (a per-key LWW insert), ns.
    const ENTRY_FOLD_NS: u64 = 1_000;
    let entries = ring_bytes / ENTRY_BYTES_PER_CREATE;
    let files_per_leaf = (FILES_PER_256K_LEAF * node_size.max(1) / (256 * 1024)).max(1);
    let leaves = entries.div_ceil(files_per_leaf);
    let load_ms = leaves.saturating_mul(LEAF_LOAD_US).div_ceil(1_000);
    let fold_ms = entries.saturating_mul(ENTRY_FOLD_NS).div_ceil(1_000_000);
    load_ms
        + fold_ms
        + crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_ms(flush_interval_ms)
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
}

/// What the routed pass did across the set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySetReport {
    pub per_volume: Vec<(u16, RecoveryReport)>,
    pub regions_released: u64,
    pub intents_rolled_forward: u64,
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
    /// (`STATUS_REJECTED`, `manager_verb_rejected`), nothing written.
    pub fn screen_record_death(
        &self,
        member: &crate::meta_ship::manager::WireIdentity,
    ) -> std::result::Result<(), KvError> {
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
        if let Some(owner) = crate::membership::installed_owner() {
            let id = crate::cowriter::node_member_id_of(member.node_token, member.mount_slot);
            if owner.member_is_live(&id) {
                return Err(reject(format!(
                    "member '{id}' holds a LIVE lease with this owner (inside T_owner)"
                )));
            }
        }
        Ok(())
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

    /// **The C14 / C15 census** (§5.8.5) of this volume against the ledger
    /// on `vol0` (`None` = no ledger reachable: C15 is empty, C14 stands).
    pub async fn slot_custody_census(
        &self,
        vol0: Option<&Arc<KvMetaBackend>>,
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
                if let Some(other) = live_by_slot.insert(slot, e.appender_id) {
                    if other != e.appender_id {
                        out.conflicts.push((slot, other, e.appender_id));
                    }
                }
                if let Some((_, Some(lessee))) = gens.get(&slot) {
                    if *lessee != e.appender_id {
                        out.conflicts.push((slot, e.appender_id, *lessee));
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
                    let window = self.window_entries_of(page).await.unwrap_or(0);
                    let holds = leases.get(&e.appender_id).is_some_and(|s| !s.is_empty());
                    if window > 0 || holds || page.is_manager {
                        out.unrecovered.push((e.appender_id, page.identity, window));
                    }
                }
                AppenderState::Recovering if named => {
                    let window = self.window_entries_of(page).await.unwrap_or(0);
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
            if !matches!(page.state, AppenderState::Live | AppenderState::Recovering) {
                continue;
            }
            // Our own regions are never the ledger's to recover: a same-
            // node page is own residue at open (PR 2), never a peer.
            if set.region(e.appender_id).is_some() || same_mount(&page.identity, &set.identity) {
                continue;
            }
            let Some((_, rec)) = dead.iter().find(|(id, _)| same_mount(id, &page.identity)) else {
                continue;
            };
            let rec = *rec;
            match self.recover_region(vol0, vol_ordinal, e, page, &rec).await {
                Ok(r) => report.recovered.push(r),
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
    /// moves, and no pass spans the move).
    async fn recover_region(
        self: &Arc<Self>,
        vol0: &Arc<KvMetaBackend>,
        vol_ordinal: u16,
        entry: &AppenderEntry,
        mut page: AppenderPage,
        dead: &DeadMemberRecord,
    ) -> std::result::Result<RecoveredRegion, KvError> {
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
        let identity = page.identity;
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
        let t = Instant::now();
        let pr_fenced = set.stats().meta_pr_wero;
        if dead.pr_key != 0 {
            let victim = dead.pr_key;
            let preempted = self.preempt_meta_registrant(victim).await
                + squeezefs_ipc::sqz_blocking::run_blocking(move || {
                    crate::data_custody::preempt_dead_registrant(victim)
                })
                .await;
            APPENDER_RECOVERY_PREEMPTS.fetch_add(preempted, Ordering::Relaxed);
        }
        APPENDER_RECOVERY_PHASE_NS[PH_PREEMPT]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);

        // ---- 2. the page: Live ⇒ Recovering (a re-run keeps its term).
        if page.state == AppenderState::Live {
            page.state = AppenderState::Recovering;
            page.recovered_by_term = self.writer_term();
            self.write_foreign_page(entry, &mut page).await?;
        }
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
        let leases = Self::read_tree0_lease_map(forest.control()).await?;
        let grant_record = self.extent_grant_record(id).await?;
        if let Some(d) = dead_ring.as_ref() {
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
            let violations = journal::detect_appender_violations(&owned, &leases, &granted);
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

        // ---- 4. the RAM lease table: the dead appender's slots go
        // Unleased NOW (the manager maintains them; the stamp reads (0, g)
        // under no lessee); stale page entries dropped.
        let t = Instant::now();
        let held: Vec<record::ForestSlot> = leases
            .get(&id)
            .map(|s| s.iter().copied().collect())
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
        let mut released: Vec<(record::ForestSlot, u32, crate::slot_lease_core::SlotWords)> =
            Vec::new();
        for slot in &held {
            let Some(lease) = plane.table.get(*slot) else {
                continue;
            };
            if lease.state == crate::slot_lease_core::LeaseState::Unleased || lease.holder != id {
                continue;
            }
            let page_entry = page
                .slots
                .iter()
                .find(|se| appender::forest_slot_of_page_slot(se.slot, set.native_slot) == *slot);
            // The tree's root: the page's when newer (the lessee's own
            // checkpoints), else the grant-time record's.
            let recorded = lease.words;
            let mut root = RootPtr {
                addr: recorded.root.0,
                seq: recorded.root.1,
            };
            if let Some(se) = page_entry {
                if se.root.addr != 0 && se.root.seq >= root.seq {
                    root = se.root;
                }
            }
            if root.addr != 0 {
                match forest.tree(*slot) {
                    Some(tr) => {
                        if root.seq > tr.root().seq {
                            tr.adopt_root(root)?;
                        }
                    }
                    None => {
                        let tree = KvTree::open_slot_tree(
                            Arc::clone(&self.cache),
                            *slot,
                            root,
                            self.seq_handle(),
                        )
                        .await?;
                        forest.adopt_guest(*slot, Arc::new(tree));
                    }
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
            let outcome = plane
                .table
                .release(*slot, id, lease.g, words, self.lease_seq());
            if !matches!(
                outcome,
                crate::slot_lease_core::ReleaseOutcome::Released
                    | crate::slot_lease_core::ReleaseOutcome::Already
            ) {
                return Err(KvError::Corrupt(format!(
                    "{}: the lease table refused the recovery release of slot {slot} from dead \
                     appender {id} at g {} ({outcome:?})",
                    self.path.display(),
                    lease.g
                )));
            }
            plane.gate.clear_foreign(*slot);
            plane.gate.revoke(*slot);
            // The round-5 law: ring 0 stamps above every record the slot
            // carries before the manager's first structural write to it.
            if words.seq_floor != 0 {
                self.ring.raise_seq_floor(words.seq_floor);
            }
            released.push((*slot, lease.g, words));
        }
        plane.refresh_holders();

        // ---- 5. replay under the structural door.
        let mut smo = self.smo.lock().await;
        let floor = self.ring.core().head();
        if let Some(d) = dead_ring.as_ref() {
            self.replay_dead_window(&d.recovery, floor).await?;
        }
        APPENDER_RECOVERY_PHASE_NS[PH_REPLAY]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);

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
        // dirty node and every moved root is published (the handover's
        // post-condition law, `COVER_CYCLES_MAX`).
        let t = Instant::now();
        let slots: Vec<record::ForestSlot> = released.iter().map(|(s, _, _)| *s).collect();
        for cycle in 0..=checkpoint::COVER_CYCLES_MAX {
            self.checkpoint_cycle(&mut smo, true).await?;
            let dirty = self.dirty_nodes_of_slots(&slots);
            let unpublished = forest
                .roots_to_publish()
                .iter()
                .any(|(s, _)| slots.contains(s));
            if dirty == 0 && !unpublished {
                break;
            }
            if cycle == checkpoint::COVER_CYCLES_MAX {
                return Err(KvError::Corrupt(format!(
                    "{}: recovery of appender {id} could not flush its slot trees in {} \
                     barriered cycles ({dirty} dirty node(s), unpublished roots: \
                     {unpublished}) — a stuck tail is a defect, never a longer wait",
                    self.path.display(),
                    checkpoint::COVER_CYCLES_MAX
                )));
            }
        }
        APPENDER_RECOVERY_PHASE_NS[PH_FLUSH]
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);

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

        // ---- 7. tree 0: Unleased + tails per slot, the grant record
        // minus the trees' images (the leave's chunked entries); the
        // unclaimed grant returned; the dead appender's orphan images
        // returned (C13 for a dead appender).
        let t = Instant::now();
        self.release_recovered_slots(&plane, id, &released, tails_by_slot)
            .await?;
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
        let orphans = self.dead_appender_orphans(vol0, id, &identity).await?;
        if !orphans.is_empty() {
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
        drop(smo);

        // ---- 8. the page: Recovered, its tail the head it was read to
        // (§5.8.3 — never replayed again).
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
        if let Some(d) = dead_ring.as_ref() {
            let head = d.ring.core().head();
            page.head_hint = head;
            page.ledger_tail_seq = head;
            page.seq_offset = d.ring.seq_offset();
        }
        self.write_foreign_page(entry_now, &mut page).await?;

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
        Ok(RecoveredRegion {
            appender_id: id,
            identity,
            slots,
            entries: entries_n,
            stale_entries: stale,
            data_bits_changed: data_bits,
        })
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
            if r.kind == RecordKind::Put {
                if let Ok((_addr, child_seq)) = decode_interior_value(&r.value) {
                    self.seq_handle().fetch_max(child_seq, Ordering::AcqRel);
                }
            }
            let (slot, separator) = forest::split_interior_journal_key(&r.key)?;
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
        for addr in tree.reachable_node_addrs().await? {
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
            if let Err(e) = self.write_control_entry(recs, EntryAdmission::Try).await {
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

    /// The image extents dead appender `id`'s grant record still claims
    /// that no tree root reaches and no `alloc_lease:` record names —
    /// C13's class for a dead appender, returned by the recoverer (its
    /// ring is `Recovered` from here, so no in-window `alloc` is ever
    /// judged against the rewritten record).
    async fn dead_appender_orphans(
        &self,
        vol0: &Arc<KvMetaBackend>,
        id: u32,
        identity: &AppenderIdentity,
    ) -> std::result::Result<Vec<u64>, KvError> {
        let record = self.extent_grant_record(id).await?;
        if record.is_empty() {
            return Ok(Vec::new());
        }
        let Some(forest) = self.forest() else {
            return Ok(Vec::new());
        };
        let _mint = forest.mint_guard().await;
        let mut reachable: std::collections::BTreeSet<u64> = Default::default();
        let mut trees: Vec<Arc<KvTree>> =
            vec![Arc::clone(forest.control()), Arc::clone(forest.native())];
        trees.extend(forest.slot_trees().into_iter().map(|(_, t)| t));
        for t in &trees {
            for addr in t.reachable_node_addrs().await? {
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
        match rsv_call(rsv, move |c| c.preempt_registrants_only(key, victim)).await {
            Ok(()) => {
                log::warn!(
                    "meta volume {}: PREEMPTED dead registrant key {victim:#x} under the \
                     manager's WERO — the device rejects its writes from here",
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
            let ckpt_seq = self.checkpoint_seq.fetch_add(1, Ordering::AcqRel) + 1;
            self.alloc
                .write_dirty_pages(&self.path, self.sb.alloc_bitmap.start, ckpt_seq)
                .await?;
            self.sync_device().await.map_err(KvError::Io)?;
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
        let mut inner = Self::open_inner(path, OpenPosture::Writer).await?;
        *inner.guard_fd.get_mut().unwrap() = Some(guard_fd);
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
        if page.identity.owned_by_node(set.identity.node_token) {
            return Err(KvError::Busy(format!(
                "{}: appender {appender_id}'s page is this node's OWN residue (node {:#018x}) — \
                 the next mount of this node recovers it itself; nothing to attest",
                path.display(),
                page.identity.node_token
            )));
        }
        // The liveness probe an offline verb has: a fresh writer claim
        // (the manager's) or a fresh `client:` registration naming the
        // appender's node inside the TTL — it may be alive.
        let now = unix_now_secs();
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
        if let Ok(Some(raw)) = be.getxattr(1, WRITER_CLAIM_XATTR).await {
            if let Some(c) = WriterClaim::decode(&raw) {
                if page.is_manager && c.age_secs(now) <= crate::fuse_client::CLIENT_STALE_TTL_SECS {
                    return Err(KvError::Busy(format!(
                        "{}: refusing to clear appender {appender_id} (the manager's page) — the \
                         writer claim heartbeated {}s ago, inside the TTL",
                        path.display(),
                        c.age_secs(now)
                    )));
                }
            }
        }
        let window = be.window_entries_of(&page).await.unwrap_or(0);
        // The death record — on volume 0's tree 0 (this open when `path`
        // IS volume 0, a guarded open of volume 0 otherwise).
        let dead = DeadMemberRecord {
            epoch: 0,
            ts_ms: crate::meta_backend::kv::alloc_lease::unix_now_ms(),
            pr_key: 0,
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
            let mut v0 = Self::open_inner(vol0_path, OpenPosture::Writer).await?;
            *v0.guard_fd.get_mut().unwrap() = vol0_guard;
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

/// Volume 0 of `routed` (the set-wide ledger's home) and its ordinal.
fn vol0_of(routed: &RoutedMetaBackend) -> Option<(usize, &Arc<KvMetaBackend>)> {
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
                Err(e) => log::error!(
                    "death ledger: recording member '{}' dead FAILED ({e}); the S6 owner's next \
                     eviction sweep does not retry — `squeezefs appender clear` is the remedy",
                    dead.id
                ),
            }
        });
    }));
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
    for vol in &routed.volumes {
        let census = vol.slot_custody_census(Some(vol0)).await?;
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
    let report = mount_path_custody_gate(routed).await?;
    spawn_ledger_poll(
        Arc::downgrade(routed),
        crate::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived(),
    );
    Ok(report)
}
