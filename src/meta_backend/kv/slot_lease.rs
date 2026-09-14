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
/// bytes.
pub fn resolve_affinity_ceiling(used_bytes: u64, node_size: u64, heap_bytes: u64) -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(SYM_AFFINITY_MAX_MB_ENV) {
        Some(mb) => (mb << 20).clamp(node_size, heap_bytes.max(node_size)),
        None => affinity_ceiling_bytes(used_bytes, node_size).min(heap_bytes.max(node_size)),
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
    /// `T_idle` in force, ms.
    pub t_idle_ms: u64,
    /// Region 0's rotor slots in acquisition order (the mint policy's
    /// candidates; the overflow arm appends).
    pub rotor: arc_swap::ArcSwap<Vec<ForestSlot>>,
    /// Round-robin cursor for equal-headroom ties.
    pub rr: AtomicUsize,
    pub extents: Arc<SlotExtentLedger>,
    /// The affinity ceiling last computed (`affinity_a_max_bytes`).
    pub a_max_bytes: AtomicU64,
    /// EWMA inputs of `N_floor` (ns): the handover's own measured cost
    /// and one ship's.
    pub ewma_handover_ns: AtomicU64,
    pub ewma_ship_ns: AtomicU64,
    /// The manager's seq the table stamps releases with (`last_written`).
    pub release_seq: AtomicU64,
    // ---- gauges (§11) ----
    pub acquires: AtomicU64,
    pub grants: AtomicU64,
    pub offers_idle: AtomicU64,
    pub offers_dominated: AtomicU64,
    pub handovers: AtomicU64,
    pub ships: AtomicU64,
    pub lru_releases: AtomicU64,
    pub forced_shrinks: AtomicU64,
    pub conflicts: AtomicU64,
    pub resolve_rpcs: AtomicU64,
    pub resolve_redirects: AtomicU64,
    pub affinity_mints: AtomicU64,
    pub rotor_mints: AtomicU64,
    pub ceiling_spills: AtomicU64,
    pub ceiling_overflows: AtomicU64,
    pub region_releases: AtomicU64,
    pub phases: HandoverPhases,
}

impl SlotLeasePlane {
    pub fn new(gate: Arc<LeaseGate>, extents: Arc<SlotExtentLedger>, mint_slots: u64) -> Self {
        Self {
            table: SlotLeaseTable::new(),
            gate,
            dominance: DominanceWindow::new(),
            mint_slots: AtomicU64::new(mint_slots),
            t_idle_ms: resolve_t_idle_ms(),
            rotor: arc_swap::ArcSwap::from_pointee(Vec::new()),
            rr: AtomicUsize::new(0),
            extents,
            a_max_bytes: AtomicU64::new(0),
            ewma_handover_ns: AtomicU64::new(0),
            ewma_ship_ns: AtomicU64::new(0),
            release_seq: AtomicU64::new(0),
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
            resolve_redirects: AtomicU64::new(0),
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

    /// The rotor size in force.
    pub fn mint_slots(&self) -> u64 {
        self.mint_slots.load(Ordering::Relaxed)
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
            resolve_redirects: self.resolve_redirects.load(Relaxed),
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
        }
    }
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
    pub resolve_rpcs: u64,
    pub resolve_redirects: u64,
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
}
