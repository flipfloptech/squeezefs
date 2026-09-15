//! **The mounted slot-lease plane** of one forest volume
//! (docs/design-symmetric-metadata.md §5.1, §5.4.1, §11 "Slot-lease
//! family"; PR 4) — the product half of [`crate::slot_lease_core`]: the
//! four knobs' resolvers, the per-slot extent ledger the affinity cap
//! reads, and the plane's gauges. Every lease TRANSITION is the core's;
//! every durable act (tree 0's `slot_state`, the page entries) is
//! `KvMetaBackend`'s.
//!
//! Armed by `SQUEEZEFS_SYMMETRIC_META=1` at a writer's open of a bit-17
//! volume; absent, the PR 1–3 forest runs verbatim (every slot the
//! mount's, the shared mint rotor, `dlm_mode` = `solo`).

use super::record::ForestSlot;
use crate::slot_lease_core::{
    affinity_ceiling_bytes, mint_slots_derived, DominanceWindow, LeaseGate, SlotLeaseTable,
    MINT_SPREAD,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Arms the plane (bool, default 0).
pub const SYMMETRIC_META_ENV: &str = "SQUEEZEFS_SYMMETRIC_META";
/// The explicit rotor size (int 1..=`MINT_SPREAD`, derived).
pub const SYM_MINT_SLOTS_ENV: &str = "SQUEEZEFS_SYM_MINT_SLOTS";
/// The explicit STATIC affinity ceiling, MiB (derived dynamic).
pub const SYM_AFFINITY_MAX_MB_ENV: &str = "SQUEEZEFS_SYM_AFFINITY_MAX_MB";
/// The handover window `T_idle`, ms (derived `T_owner`).
pub const SYM_T_IDLE_MS_ENV: &str = "SQUEEZEFS_SYM_T_IDLE_MS";

/// Whether the operator asked for the plane (the knob alone — the volume
/// gate is the open's).
pub fn symmetric_meta_requested() -> bool {
    crate::env_knobs::bool_knob(SYMMETRIC_META_ENV, false)
}

/// The rotor size in force: the knob verbatim, else
/// [`mint_slots_derived`] (`clamp(W / (2 × writers), 1, MINT_SPREAD)`).
pub fn resolve_mint_slots(width: u64, writers_known: u64) -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(SYM_MINT_SLOTS_ENV) {
        Some(n) => n.clamp(1, MINT_SPREAD),
        None => mint_slots_derived(width, writers_known),
    }
}

/// The affinity ceiling in force, bytes: the knob's static value (MiB,
/// clamped to `[node_size, heap_bytes]` — the design's "one-extent
/// floor" is `node_size`, the heap is the degenerate hard ceiling), else
/// the DYNAMIC [`affinity_ceiling_bytes`] over the volume's current used
/// bytes. `static_mb` is the knob as read ONCE at open
/// ([`SlotLeasePlane::affinity_static_mb`]).
pub fn affinity_ceiling_in_force(
    static_mb: Option<u64>,
    used_bytes: u64,
    node_size: u64,
    heap_bytes: u64,
) -> u64 {
    match static_mb {
        Some(mb) => (mb << 20).clamp(node_size, heap_bytes.max(node_size)),
        None => affinity_ceiling_bytes(used_bytes, node_size).min(heap_bytes.max(node_size)),
    }
}

/// [`affinity_ceiling_in_force`] with the knob read now (the open's read).
pub fn resolve_affinity_ceiling(used_bytes: u64, node_size: u64, heap_bytes: u64) -> u64 {
    affinity_ceiling_in_force(
        crate::env_knobs::opt_int_knob::<u64>(SYM_AFFINITY_MAX_MB_ENV),
        used_bytes,
        node_size,
        heap_bytes,
    )
}

/// The rotor size in force from an explicit knob value read at open
/// (`Some` wins verbatim) or the derivation over the census.
pub fn mint_slots_in_force(knob: Option<u64>, width: u64, writers_known: u64) -> u64 {
    match knob {
        Some(n) => n.clamp(1, MINT_SPREAD),
        None => mint_slots_derived(width, writers_known),
    }
}

/// `T_idle` in force, ms: the knob verbatim, else the membership plane's
/// `T_owner` (`SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS`, the 45 s staleness law).
pub fn resolve_t_idle_ms() -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(SYM_T_IDLE_MS_ENV) {
        Some(ms) => ms,
        None => crate::env_knobs::int_knob(
            "SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS",
            crate::fuse_client::CLIENT_STALE_TTL_SECS * 1000,
        ),
    }
}

/// Per-slot extent counts (`slot_tree_extents`, §5.1.2): maintained from
/// the lessee's own claims and frees at the SMO context, seeded from the
/// page entry / tree-0 record at open, written back to both. Latch-free
/// reads on the mint path.
#[derive(Debug, Default)]
pub struct SlotExtentLedger {
    map: scc::HashMap<ForestSlot, Arc<AtomicU64>>,
}

impl SlotExtentLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn cell(&self, slot: ForestSlot) -> Arc<AtomicU64> {
        if let Some(c) = self.map.read_sync(&slot, |_, c| Arc::clone(c)) {
            return c;
        }
        let fresh = Arc::new(AtomicU64::new(0));
        match self.map.insert_sync(slot, Arc::clone(&fresh)) {
            Ok(()) => fresh,
            Err(_) => self
                .map
                .read_sync(&slot, |_, c| Arc::clone(c))
                .unwrap_or(fresh),
        }
    }

    /// One image claimed for `slot`'s tree.
    pub fn claim(&self, slot: ForestSlot) {
        self.cell(slot).fetch_add(1, Ordering::Relaxed);
    }

    /// One image of `slot`'s tree retired (or a claim released).
    pub fn free(&self, slot: ForestSlot) {
        let c = self.cell(slot);
        let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
    }

    /// The count in force (0 = unknown or empty).
    pub fn get(&self, slot: ForestSlot) -> u64 {
        self.map
            .read_sync(&slot, |_, c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Install a durable count (open, acquire).
    pub fn set(&self, slot: ForestSlot, n: u64) {
        self.cell(slot).store(n, Ordering::Relaxed);
    }

    /// Forget `slot` (released).
    pub fn forget(&self, slot: ForestSlot) {
        let _ = self.map.remove_sync(&slot);
    }

    /// Σ over every slot tree this mount holds a count for — the
    /// affinity cap's `used_leaf_bytes` input in extents (review round 2,
    /// Issue 10: the ledger counts every slot tree's IMAGES, leaves and
    /// interior; at the shipped fan-out the interior share is under 1 %,
    /// and none of the non-leaf heap — rings, the directory, pages, tree 0
    /// — is in it).
    pub fn total(&self) -> u64 {
        let mut sum = 0u64;
        self.map.iter_sync(|_, c| {
            sum = sum.saturating_add(c.load(Ordering::Relaxed));
            true
        });
        sum
    }
}

/// `slot_handover_phase_ns` — `flush / page / tree0 / grant / total`,
/// exact-sum.
#[derive(Debug, Default)]
pub struct HandoverPhases {
    pub flush_ns: AtomicU64,
    pub page_ns: AtomicU64,
    pub tree0_ns: AtomicU64,
    pub grant_ns: AtomicU64,
    pub total_ns: AtomicU64,
    pub samples: AtomicU64,
}

impl HandoverPhases {
    /// Record one handover's phases.
    pub fn record(&self, flush: u64, page: u64, tree0: u64, grant: u64) {
        self.flush_ns.fetch_add(flush, Ordering::Relaxed);
        self.page_ns.fetch_add(page, Ordering::Relaxed);
        self.tree0_ns.fetch_add(tree0, Ordering::Relaxed);
        self.grant_ns.fetch_add(grant, Ordering::Relaxed);
        self.total_ns
            .fetch_add(flush + page + tree0 + grant, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
    }

    /// `[flush, page, tree0, grant, total]`.
    pub fn snapshot(&self) -> [u64; 5] {
        [
            self.flush_ns.load(Ordering::Relaxed),
            self.page_ns.load(Ordering::Relaxed),
            self.tree0_ns.load(Ordering::Relaxed),
            self.grant_ns.load(Ordering::Relaxed),
            self.total_ns.load(Ordering::Relaxed),
        ]
    }
}

/// The plane of one mounted volume: the manager's lease table (this
/// mount IS the manager of every volume it writes in PR 4), the gate the
/// node cache reads, the holder's dominance counters, the rotor, the
/// derivations in force and the gauges.
pub struct SlotLeasePlane {
    pub table: SlotLeaseTable,
    pub gate: Arc<LeaseGate>,
    pub dominance: DominanceWindow,
    /// The rotor size in force (`slot_rotor`).
    pub mint_slots: AtomicU64,
    /// `SQUEEZEFS_SYM_MINT_SLOTS` as read at open (`None` = derived from
    /// the census at every cadence).
    pub mint_slots_knob: Option<u64>,
    /// `SQUEEZEFS_SYM_AFFINITY_MAX_MB` as read at open (`None` = the
    /// dynamic derivation).
    pub affinity_static_mb: Option<u64>,
    /// The PR-2 seam's declared partition (`SQUEEZEFS_TEST_SYM_APPENDER_
    /// SLOTS`) as the arm's WISH-LIST for each declared region: acquired
    /// where unleased or already the region's, skipped where another
    /// appender holds the slot (the real acquire path decides).
    pub declared: std::collections::BTreeMap<u32, std::collections::BTreeSet<ForestSlot>>,
    /// Test seam: the census the forced shrink derives `M` from, when set
    /// (the in-process contracts cannot stand up 512 joiners; `0` = read
    /// `appenders_known`).
    pub test_writers_known: AtomicU64,
    /// `T_idle` in force, ms.
    pub t_idle_ms: u64,
    /// Region 0's rotor slots in acquisition order (the mint policy's
    /// candidates; the overflow arm appends).
    pub rotor: arc_swap::ArcSwap<Vec<ForestSlot>>,
    /// Round-robin cursor for equal-headroom ties.
    pub rr: AtomicUsize,
    pub extents: Arc<SlotExtentLedger>,
    /// The holder's own ops per leased slot over the common window
    /// (lock-free — one bump per commit on the hot path).
    holder_ops: scc::HashMap<ForestSlot, Arc<crate::slot_lease_core::HolderOps>>,
    /// The affinity ceiling last computed (`affinity_a_max_bytes`).
    pub a_max_bytes: AtomicU64,
    /// EWMA inputs of `N_floor` (ns): the handover's own measured cost
    /// and one ship's.
    pub ewma_handover_ns: AtomicU64,
    pub ewma_ship_ns: AtomicU64,
    /// The manager's seq the table stamps releases with (`last_written`).
    pub release_seq: AtomicU64,
    /// Slot → holder as tree 0 records it (§5.1.6) — the ship target's
    /// and token server's resolution, wire-free.
    pub holders: crate::slot_holder_cache::SlotHolderCache,
    /// Per-slot handover cooldown (the S10 valve reused as §5.1.4's
    /// requester-side cooldown): no new offer of a slot before the
    /// instant recorded at its last handover.
    cooldowns: std::sync::Mutex<std::collections::BTreeMap<ForestSlot, u64>>,
    /// The slot entries of every in-process region's page AS LOADED at
    /// open (`region id → entries`): the join's checkpoint rewrites the
    /// pages from the RAM lease set before the arm settles them against
    /// tree 0, so the §5.3.4 row 5/6 `Releasing` entries and C14's live
    /// attestations are read from this snapshot, taken once, drained by
    /// the arm.
    pub loaded_page_entries:
        std::sync::Mutex<std::collections::BTreeMap<u32, Vec<super::appender::SlotEntry>>>,
    /// The volume's native routing slot (page entries name ROUTING slots;
    /// the carriage does too).
    pub native_slot: u16,
    /// The volume's superblock uuid — the process-global S4 owner
    /// table's per-volume key (`dlm_slot::install_lease_foreign_slots`);
    /// the plane's drop withdraws the contribution, so a backend dropped
    /// without its leave never leaves a stale foreign set behind.
    pub volume_uuid: u128,
    /// Appender id → `(node_token, mount_slot)` of every `Live` page in
    /// the directory (the arm reads it; a wire join adds one) — the
    /// membership carriage's member-id → appender resolution.
    pub identities: std::sync::Mutex<std::collections::BTreeMap<u32, (u64, u32)>>,
    /// Slots the manager RECALLS from a wire holder (an accepted offer of
    /// a slot it holds) — carried on its renewal grant as
    /// `slot_release_notices`; cleared by its `ReleaseSlot`.
    pub recalls:
        std::sync::Mutex<std::collections::BTreeMap<u32, std::collections::BTreeSet<ForestSlot>>>,
    /// Fired when a handover COMPLETES or aborts (the gate's `Releasing`
    /// bit cleared, the requester granted): the door's park wakes here
    /// and re-reads the slot (review round 2, Issue 6).
    pub handover_done: squeezefs_ipc::sqz_notify::Notify,
    /// Fired when a door token returns and the slot's count reaches 0:
    /// a release parked on the door's drain wakes here.
    pub door_drained: squeezefs_ipc::sqz_notify::Notify,
    /// **The one-writer mutex of the rotor** (review round 2, Issue 5):
    /// every clone-and-store of `rotor` runs under it — the arm, the
    /// overflow arm's push, the handover's and the leave's removal — so
    /// two RMWs from two critical sections can never lose an update.
    /// Held for the RMW only, never across an await; readers stay
    /// lock-free on the `ArcSwap`.
    rotor_writer: std::sync::Mutex<()>,
    // ---- gauges (§11) ----
    pub acquires: AtomicU64,
    pub grants: AtomicU64,
    pub offers_idle: AtomicU64,
    pub offers_dominated: AtomicU64,
    /// `OfferSlot` answered `Busy` — a second dominating ship before the
    /// accept (legal; never `manager_verb_refusals`).
    pub offers_busy: AtomicU64,
    pub handovers: AtomicU64,
    pub ships: AtomicU64,
    pub lru_releases: AtomicU64,
    pub forced_shrinks: AtomicU64,
    pub conflicts: AtomicU64,
    pub resolve_rpcs: AtomicU64,
    pub affinity_mints: AtomicU64,
    pub rotor_mints: AtomicU64,
    pub ceiling_spills: AtomicU64,
    pub ceiling_overflows: AtomicU64,
    pub region_releases: AtomicU64,
    pub recall_notices: AtomicU64,
    pub stale_entries: AtomicU64,
    /// Commits parked at the door on a slot mid-handover (Issue 6).
    pub door_parks: AtomicU64,
    /// Commits refused at the door because another appender leases the
    /// slot — `SlotBusy`, the "ship to the holder" class (legal).
    pub door_refusals: AtomicU64,
    /// Explicit `AcquireSlot(s)` asks for a slot another appender holds
    /// (legal; the singular verb's `Refused` reply and the plural's error).
    pub acquire_refusals: AtomicU64,
    /// Rotor asks refused past `2 × M` (legal — the overflow arm's
    /// designed past-cap answer).
    pub rotor_cap_refusals: AtomicU64,
    /// Slot trees another appender leases that a structural pass (the
    /// merge sweep, the heap-full recovery, the D4 arm) SKIPPED — never
    /// this mount's to maintain (review round 4, Issue 25; the gate's
    /// refusal is the belt behind this filter). 0 on a solo mount.
    pub merge_sweep_foreign_skips: AtomicU64,
    pub phases: HandoverPhases,
}

impl Drop for SlotLeasePlane {
    fn drop(&mut self) {
        crate::dlm_slot::install_lease_foreign_slots(self.volume_uuid, None);
    }
}

impl SlotLeasePlane {
    pub fn new(
        gate: Arc<LeaseGate>,
        extents: Arc<SlotExtentLedger>,
        mint_slots: u64,
        declared: std::collections::BTreeMap<u32, std::collections::BTreeSet<ForestSlot>>,
        native_slot: u16,
        volume_uuid: u128,
    ) -> Self {
        Self {
            declared,
            native_slot,
            volume_uuid,
            identities: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            recalls: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            handover_done: squeezefs_ipc::sqz_notify::Notify::new(),
            door_drained: squeezefs_ipc::sqz_notify::Notify::new(),
            rotor_writer: std::sync::Mutex::new(()),
            recall_notices: AtomicU64::new(0),
            stale_entries: AtomicU64::new(0),
            door_parks: AtomicU64::new(0),
            door_refusals: AtomicU64::new(0),
            acquire_refusals: AtomicU64::new(0),
            rotor_cap_refusals: AtomicU64::new(0),
            merge_sweep_foreign_skips: AtomicU64::new(0),
            offers_busy: AtomicU64::new(0),
            table: SlotLeaseTable::new(),
            gate,
            dominance: DominanceWindow::new(),
            mint_slots: AtomicU64::new(mint_slots),
            mint_slots_knob: crate::env_knobs::opt_int_knob::<u64>(SYM_MINT_SLOTS_ENV),
            affinity_static_mb: crate::env_knobs::opt_int_knob::<u64>(SYM_AFFINITY_MAX_MB_ENV),
            test_writers_known: AtomicU64::new(0),
            t_idle_ms: resolve_t_idle_ms(),
            rotor: arc_swap::ArcSwap::from_pointee(Vec::new()),
            rr: AtomicUsize::new(0),
            extents,
            holder_ops: scc::HashMap::new(),
            a_max_bytes: AtomicU64::new(0),
            ewma_handover_ns: AtomicU64::new(0),
            ewma_ship_ns: AtomicU64::new(0),
            release_seq: AtomicU64::new(0),
            holders: crate::slot_holder_cache::SlotHolderCache::new(),
            cooldowns: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            loaded_page_entries: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            acquires: AtomicU64::new(0),
            grants: AtomicU64::new(0),
            offers_idle: AtomicU64::new(0),
            offers_dominated: AtomicU64::new(0),
            handovers: AtomicU64::new(0),
            ships: AtomicU64::new(0),
            lru_releases: AtomicU64::new(0),
            forced_shrinks: AtomicU64::new(0),
            conflicts: AtomicU64::new(0),
            resolve_rpcs: AtomicU64::new(0),
            affinity_mints: AtomicU64::new(0),
            rotor_mints: AtomicU64::new(0),
            ceiling_spills: AtomicU64::new(0),
            ceiling_overflows: AtomicU64::new(0),
            region_releases: AtomicU64::new(0),
            phases: HandoverPhases::default(),
        }
    }

    /// `T_idle` in ns.
    pub fn t_idle_ns(&self) -> u64 {
        self.t_idle_ms.saturating_mul(1_000_000)
    }

    fn holder_cell(&self, slot: ForestSlot) -> Arc<crate::slot_lease_core::HolderOps> {
        if let Some(c) = self.holder_ops.read_sync(&slot, |_, c| Arc::clone(c)) {
            return c;
        }
        let fresh = Arc::new(crate::slot_lease_core::HolderOps::new());
        match self.holder_ops.insert_sync(slot, Arc::clone(&fresh)) {
            Ok(()) => fresh,
            Err(_) => self
                .holder_ops
                .read_sync(&slot, |_, c| Arc::clone(c))
                .unwrap_or(fresh),
        }
    }

    /// One own commit on `slot` at `now_ns`.
    pub fn note_holder_op(&self, slot: ForestSlot, now_ns: u64, t_idle_ns: u64) {
        self.holder_cell(slot).note(now_ns, t_idle_ns);
    }

    /// The holder's ops on `slot` over the common window at `now_ns`.
    pub fn holder_ops(&self, slot: ForestSlot, now_ns: u64) -> u64 {
        self.holder_ops
            .read_sync(&slot, |_, c| c.total(now_ns, self.t_idle_ns()))
            .unwrap_or(0)
    }

    /// The rotor size in force.
    pub fn mint_slots(&self) -> u64 {
        self.mint_slots.load(Ordering::Relaxed)
    }

    /// The handover cooldown of one slot (§5.1.4 — the S10 never-thrash
    /// valve, `recall_cooldown_from`, over the `T_idle` window): a slot
    /// handed over at `now_ns` is offered again no sooner than this.
    pub fn cooldown_ns(&self) -> u64 {
        crate::meta_ship::tokens::recall_cooldown_from(
            crate::env_knobs::opt_int_knob::<u64>(crate::meta_ship::tokens::RECALL_COOLDOWN_ENV),
            std::time::Duration::from_millis(self.t_idle_ms),
        )
        .as_nanos() as u64
    }

    /// Record a handover of `slot` at `now_ns`: its cooldown starts.
    pub fn note_handover(&self, slot: ForestSlot, now_ns: u64) {
        self.cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slot, now_ns.saturating_add(self.cooldown_ns()));
    }

    /// Whether `slot` is inside its handover cooldown at `now_ns`.
    pub fn in_cooldown(&self, slot: ForestSlot, now_ns: u64) -> bool {
        self.cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&slot)
            .is_some_and(|until| now_ns < *until)
    }

    /// Mirror the lease table into the holder cache (the manager's own
    /// tree-0 view — every grant and release it writes lands here).
    pub fn refresh_holders(&self) {
        self.holders.refresh(self.table.leased_entries());
    }

    /// Mutate the rotor under its one-writer mutex (Issue 5): `f` sees
    /// the current vector and returns the next; readers keep their
    /// lock-free `ArcSwap` load.
    pub fn rotor_update(&self, f: impl FnOnce(&mut Vec<ForestSlot>)) {
        let _w = self.rotor_writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = (**self.rotor.load()).clone();
        f(&mut next);
        self.rotor.store(Arc::new(next));
    }

    /// Seed the `N_floor` inputs at the arm from the mount's always-on
    /// EWMAs (§5.1.4's cold start, review round 2 Issue 7): one ship =
    /// the S8 `meta_ship_phase_ns.rtt` mean when a verb has shipped, else
    /// one journal write's round trip (`uring_fs_write_phase_ns.total` —
    /// the in-process proxy for a fabric RTT, the same order on the
    /// fleet's fabric; the first measured ship replaces it); one handover
    /// = [`crate::slot_lease_core::handover_cold_start_ns`] over the
    /// barrier and RTT means. Idempotent: a seeded EWMA is left alone.
    pub fn seed_n_floor_inputs(&self) {
        let mean = |(sum, n): (u64, u64)| if n == 0 { 0 } else { sum / n };
        let write_ns = mean(crate::uring_fs::uring_fs_write_phase_totals(
            crate::uring_fs::UringFsWritePhase::Total,
        ));
        let rtt_ns = mean(crate::meta_ship::ship_phase_totals(
            crate::meta_ship::ShipPhase::Rtt,
        ));
        let ship_ns = if rtt_ns != 0 { rtt_ns } else { write_ns };
        if ship_ns != 0 {
            let _ = self.ewma_ship_ns.compare_exchange(
                0,
                ship_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        let handover_ns = crate::slot_lease_core::handover_cold_start_ns(write_ns, ship_ns);
        if handover_ns != 0 {
            let _ = self.ewma_handover_ns.compare_exchange(
                0,
                handover_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// `N_floor` in force (`slot_offer_n_floor`): the measured handover
    /// cost over the measured ship cost, or the cold-start derivation
    /// the caller folded into the EWMAs.
    pub fn n_floor(&self) -> u64 {
        crate::slot_lease_core::n_floor(
            self.ewma_handover_ns.load(Ordering::Relaxed),
            self.ewma_ship_ns.load(Ordering::Relaxed),
        )
    }

    /// Fold one measured handover wall into the EWMA (`ewma = ewma × 7/8
    /// + sample / 8`; the first sample seeds).
    pub fn fold_handover_ns(&self, sample: u64) {
        let cur = self.ewma_handover_ns.load(Ordering::Relaxed);
        let next = if cur == 0 {
            sample
        } else {
            cur - cur / 8 + sample / 8
        };
        self.ewma_handover_ns.store(next, Ordering::Relaxed);
    }

    /// Fold one measured ship wall into the EWMA.
    pub fn fold_ship_ns(&self, sample: u64) {
        let cur = self.ewma_ship_ns.load(Ordering::Relaxed);
        let next = if cur == 0 {
            sample
        } else {
            cur - cur / 8 + sample / 8
        };
        self.ewma_ship_ns.store(next, Ordering::Relaxed);
    }

    /// The Slot-lease family snapshot. `minted_of(slot)` = the inos the
    /// slot's cursor has minted (`slot_tree_inos` is cursor-derived: the
    /// minted population, an upper bound on the live one — a census walk
    /// per stats read is not a price a gauge pays).
    pub fn stats(&self, node_size: u64, minted_of: &dyn Fn(ForestSlot) -> u64) -> SlotLeaseStats {
        use Ordering::Relaxed;
        let (offers, offers_expired) = self.table.offer_counts();
        let held = self.gate.leased_slots();
        let mut inos: Vec<u64> = Vec::with_capacity(held.len());
        let mut bytes: Vec<u64> = Vec::with_capacity(held.len());
        for s in &held {
            bytes.push(self.extents.get(*s).saturating_mul(node_size));
            inos.push(minted_of(*s));
        }
        SlotLeaseStats {
            leases_held: held.len() as u64,
            rotor: self.mint_slots(),
            acquires: self.acquires.load(Relaxed),
            grants: self.grants.load(Relaxed),
            offers,
            offers_idle: self.offers_idle.load(Relaxed),
            offers_dominated: self.offers_dominated.load(Relaxed),
            offers_expired,
            handovers: self.handovers.load(Relaxed),
            handover_phase_ns: self.phases.snapshot(),
            ships: self.ships.load(Relaxed),
            lru_releases: self.lru_releases.load(Relaxed),
            forced_shrinks: self.forced_shrinks.load(Relaxed),
            conflicts: self.conflicts.load(Relaxed),
            resolve_rpcs: self.resolve_rpcs.load(Relaxed),
            tree_inos_p50: percentile(&inos, 50),
            tree_inos_p99: percentile(&inos, 99),
            tree_inos_max: inos.iter().copied().max().unwrap_or(0),
            tree_bytes_p99: percentile(&bytes, 99),
            tree_bytes_max: bytes.iter().copied().max().unwrap_or(0),
            a_max_bytes: self.a_max_bytes.load(Relaxed),
            affinity_mints: self.affinity_mints.load(Relaxed),
            rotor_mints: self.rotor_mints.load(Relaxed),
            ceiling_spills: self.ceiling_spills.load(Relaxed),
            ceiling_overflows: self.ceiling_overflows.load(Relaxed),
            offer_n_floor: self.n_floor(),
            region_releases: self.region_releases.load(Relaxed),
            recall_notices: self.recall_notices.load(Relaxed),
            stale_entries: self.stale_entries.load(Relaxed),
            door_parks: self.door_parks.load(Relaxed),
            door_refusals: self.door_refusals.load(Relaxed),
            acquire_refusals: self.acquire_refusals.load(Relaxed),
            rotor_cap_refusals: self.rotor_cap_refusals.load(Relaxed),
            merge_sweep_foreign_skips: self.merge_sweep_foreign_skips.load(Relaxed),
            offers_busy: self.offers_busy.load(Relaxed),
        }
    }

    /// Learn (or refresh) appender `id`'s identity for the carriage.
    pub fn note_identity(&self, id: u32, node_token: u64, mount_slot: u32) {
        self.identities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (node_token, mount_slot));
    }

    /// Record a recall of `slot` from wire appender `holder` (its next
    /// renewal grant carries it).
    pub fn note_recall(&self, holder: u32, slot: ForestSlot) {
        let fresh = self
            .recalls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(holder)
            .or_default()
            .insert(slot);
        if fresh {
            self.recall_notices.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The holder released `slot` (or lost it): the recall is spent.
    pub fn clear_recall(&self, holder: u32, slot: ForestSlot) {
        let mut m = self.recalls.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = m.get_mut(&holder) {
            set.remove(&slot);
            if set.is_empty() {
                m.remove(&holder);
            }
        }
    }

    /// The pending recalls of `holder` (forest slots).
    pub fn recalls_of(&self, holder: u32) -> Vec<ForestSlot> {
        self.recalls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&holder)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// **The membership carriage of one member** (§5.1.4 / §5.9 — the
    /// `Grant`'s `slot_leases_ack` / `slot_release_notices` /
    /// `offered_slots`): every appender of this plane whose page identity
    /// is `(node_token, mount_slot)` — its leases `(routing slot, g)`,
    /// the manager's recalls from it, the offers standing for it. O(held)
    /// off the RAM table; empty for an identity no page names.
    pub fn carriage_for(
        &self,
        node_token: u64,
        mount_slot: u32,
    ) -> crate::membership::SlotLeaseCarriage {
        let ids: Vec<u32> = self
            .identities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, ident)| **ident == (node_token, mount_slot))
            .map(|(id, _)| *id)
            .collect();
        let mut out = crate::membership::SlotLeaseCarriage::default();
        if ids.is_empty() {
            return out;
        }
        let routing = |slot: ForestSlot| -> Option<u16> {
            super::appender::page_slot_of_forest_slot(slot, self.native_slot).ok()
        };
        // O(held + offered) off the table's holder and offer indexes
        // (Issue 16) — bounded by the page budget per appender.
        for id in &ids {
            for slot in self.table.held_by(*id) {
                let Some(r) = routing(slot) else {
                    continue;
                };
                if let Some(l) = self.table.get(slot) {
                    out.leases.push((r, l.g));
                }
            }
            for (slot, g) in self.table.offered_to(*id) {
                if let Some(r) = routing(slot) {
                    out.offered.push((r, g));
                }
            }
        }
        for id in &ids {
            for slot in self.recalls_of(*id) {
                if let Some(r) = routing(slot) {
                    out.release_notices.push(r);
                }
            }
        }
        out.leases.sort_unstable();
        out.offered.sort_unstable();
        out.release_notices.sort_unstable();
        out.release_notices.dedup();
        out
    }
}

/// **A commit's door tokens** (review round 2, Issue 6 — the §5.4.1 door
/// law): one token per distinct slot the tx's records name, taken at the
/// door ([`LeaseGate::enter`]) and returned at the tx's TERMINAL outcome
/// — the queue entry owns it, the way it owns the tx's DLM guards, so a
/// dropped committer future cannot return a token early. A release
/// raises `Releasing` and drains the slot's tokens before its flush;
/// the last token out wakes it (`door_drained`).
pub struct DoorPass {
    plane: Arc<SlotLeasePlane>,
    slots: Vec<ForestSlot>,
}

impl std::fmt::Debug for DoorPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoorPass")
            .field("slots", &self.slots)
            .finish()
    }
}

impl DoorPass {
    /// The tokens of `slots`, already taken on `plane`'s gate.
    pub fn new(plane: Arc<SlotLeasePlane>, slots: Vec<ForestSlot>) -> Self {
        Self { plane, slots }
    }
}

impl Drop for DoorPass {
    fn drop(&mut self) {
        let mut drained = false;
        for slot in &self.slots {
            if self.plane.gate.leave(*slot) == 0 {
                drained = true;
            }
        }
        if drained {
            self.plane.door_drained.notify_waiters();
        }
    }
}

/// Every armed plane of this process, for the membership carriage
/// (one owner serves a SET; a member's leases span its volumes).
static CARRIAGE_PLANES: std::sync::Mutex<Vec<std::sync::Weak<SlotLeasePlane>>> =
    std::sync::Mutex::new(Vec::new());

/// Register an armed plane with the membership carriage source (the arm).
pub fn register_carriage_plane(plane: &Arc<SlotLeasePlane>) {
    let mut planes = CARRIAGE_PLANES.lock().unwrap_or_else(|e| e.into_inner());
    planes.retain(|w| w.strong_count() > 0);
    if !planes.iter().any(|w| w.as_ptr() == Arc::as_ptr(plane)) {
        planes.push(Arc::downgrade(plane));
    }
    if planes.len() == 1 {
        crate::membership::install_slot_lease_carriage_source(Arc::new(carriage_for_member));
    }
}

/// Unregister it (the leave); the source is uninstalled with the last
/// plane.
pub fn unregister_carriage_plane(plane: &Arc<SlotLeasePlane>) {
    let mut planes = CARRIAGE_PLANES.lock().unwrap_or_else(|e| e.into_inner());
    planes.retain(|w| w.strong_count() > 0 && w.as_ptr() != Arc::as_ptr(plane));
    if planes.is_empty() {
        crate::membership::uninstall_slot_lease_carriage_source();
    }
}

/// The installed source: the member id → `(node_token, mount_slot)`
/// (`cowriter::parse_node_member_id`), merged over every armed plane.
fn carriage_for_member(member_id: &str) -> crate::membership::SlotLeaseCarriage {
    let Some((node_token, mount_slot)) = crate::cowriter::parse_node_member_id(member_id) else {
        return Default::default();
    };
    let planes: Vec<Arc<SlotLeasePlane>> = CARRIAGE_PLANES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect();
    let mut out = crate::membership::SlotLeaseCarriage::default();
    for p in planes {
        let c = p.carriage_for(node_token, mount_slot);
        out.leases.extend(c.leases);
        out.release_notices.extend(c.release_notices);
        out.offered.extend(c.offered);
    }
    out.leases.sort_unstable();
    out.offered.sort_unstable();
    out.release_notices.sort_unstable();
    out.release_notices.dedup();
    out
}

fn percentile(sorted_in: &[u64], pct: usize) -> u64 {
    if sorted_in.is_empty() {
        return 0;
    }
    let mut v = sorted_in.to_vec();
    v.sort_unstable();
    let idx = (v.len() * pct)
        .div_ceil(100)
        .saturating_sub(1)
        .min(v.len() - 1);
    v[idx]
}

/// The Slot-lease family (§11) as one snapshot; `None` on a bit-17-absent
/// or unarmed mount (`KvMetaBackend::slot_lease_stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLeaseStats {
    pub leases_held: u64,
    pub rotor: u64,
    pub acquires: u64,
    pub grants: u64,
    pub offers: u64,
    pub offers_idle: u64,
    pub offers_dominated: u64,
    pub offers_expired: u64,
    pub handovers: u64,
    /// `flush / page / tree0 / grant / total`.
    pub handover_phase_ns: [u64; 5],
    pub ships: u64,
    pub lru_releases: u64,
    pub forced_shrinks: u64,
    /// **Must-stay-0**: two live attestations of one slot (C14's live face).
    pub conflicts: u64,
    /// `ResolveSlot` verbs served (the wire's stale-view fallback; 0 on a
    /// solo mount — the warm path is the holder cache).
    pub resolve_rpcs: u64,
    /// Cursor-derived per held slot: minted inos p50 / p99 / max.
    pub tree_inos_p50: u64,
    pub tree_inos_p99: u64,
    pub tree_inos_max: u64,
    pub tree_bytes_p99: u64,
    pub tree_bytes_max: u64,
    /// The affinity ceiling in force.
    pub a_max_bytes: u64,
    pub affinity_mints: u64,
    pub rotor_mints: u64,
    pub ceiling_spills: u64,
    pub ceiling_overflows: u64,
    pub offer_n_floor: u64,
    /// Regions released because their last slot on the volume was.
    pub region_releases: u64,
    /// Recalls of a slot from a WIRE holder recorded for its renewal
    /// grant's `slot_release_notices` (an accepted offer of a slot it
    /// holds).
    pub recall_notices: u64,
    /// `Live` page attestations below tree 0's `g` for the slot, dropped
    /// at the arm as stale residue (a leave or handover the page never
    /// recorded — review round 2 Issue 8).
    pub stale_entries: u64,
    /// Commits parked at the door on a slot mid-handover (Issue 6 — a
    /// park, never an error; the handover is bounded).
    pub door_parks: u64,
    /// Commits refused `SlotBusy` at the door: another appender leases
    /// the slot (the "ship to the holder" class — PR 6/12's ship).
    pub door_refusals: u64,
    /// Explicit acquires of a slot another appender holds (legal).
    pub acquire_refusals: u64,
    /// Rotor asks refused past `2 × M` (legal — the overflow arm's
    /// designed answer).
    pub rotor_cap_refusals: u64,
    /// Slot trees another appender leases that a structural pass skipped
    /// (never this mount's to maintain — Issue 25). 0 on a solo mount.
    pub merge_sweep_foreign_skips: u64,
    /// `OfferSlot` answered `Busy` — a second dominating ship before the
    /// accept (legal).
    pub offers_busy: u64,
}
