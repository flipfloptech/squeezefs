//! PR B2 — the **device-backed visible overlay** registry
//! (`docs/design-device-overlay.md` Rev 2; Approach B of the
//! write-bandwidth program, rc-manifest §3f).
//!
//! One volatile registry per daemon: at most one live
//! [`DeviceOverlayRecord`] per `(ino, block)`. A record's pure state
//! (the laws) lives in [`crate::overlay_core::OverlayRecordCore`]; this
//! module adds the prod-side custody the core deliberately does not
//! know about:
//!
//! * the **destination** — an unpublished offset reserved from
//!   [`crate::block_allocator::BlockAllocator::allocate_block`] (law 2:
//!   reserve-before-DMA; the offset's incarnation stays UNSTABLE until
//!   publication, so racing validated tier fills of a reused key fail
//!   their seqlock re-check);
//! * the **fsck guard** — [`InflightAllocGuard`], the C2/C3 in-flight
//!   exemption. **Visibility ONLY — dropping it frees nothing**
//!   (Rev 2 correction A);
//! * the **mint rollback owner** — a [`MintedBlockGuard`]-class owner
//!   (law 9 / KD-OV-11): every exit between mint and durable
//!   publication frees the offset through it. Its disposition is
//!   per-terminal-state: `Published` ⇒ DISARM (the durable map/ref
//!   publish transferred ownership); `Superseded` ⇒ the guard's drop
//!   FREES; `FenceDropped` ⇒ disarm WITHOUT freeing (W5 — nothing
//!   freed post-fence, successor recovery owns the accounting);
//! * the ino's **fencing token** captured at install (the FIND-M11-A
//!   face — publication re-presents the current generation through the
//!   same retry law the write-through uses).
//!
//! **Durability class (§6.1):** RAM only — no on-disk representation,
//! no incompat bit, no journal record. A crash recovers by the existing
//! census arithmetic (`recover_active_blocks_v3` walks durable maps
//! only; unpublished destinations are simply never referenced ⇒
//! free-listed). Un-fsynced ACKed overlay writes are lost — the
//! writeback-class contract, unchanged and stated.
//!
//! **B2 scope fence:** fresh/hole blocks only (`old_binding_or_hole =
//! Hole` — law 5's gaps are zeros, no displaced free exists, and the
//! rewrite-shadow dual-authority hazards structurally cannot arise);
//! reads of open overlays DRAIN (freeze → complete → seed zeros →
//! publish) instead of composing — the §5.2 lock-free read protocol is
//! PR B3.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use crate::overlay_core::OverlayRecordCore;

/// Enablement tri-state: 0 = read the env knob on first probe,
/// 1 = off, 2 = on.
static ENABLED: AtomicU8 = AtomicU8::new(0);
/// Test seam (`SQUEEZEFS_TEST_OVERLAY_BYTES`): in-process suites carry
/// `WritePayload::Bytes` (no zc transport), so the pooled §4.3 vehicle
/// serves as the store engine. Same tri-state encoding.
static BYTES_VEHICLE: AtomicU8 = AtomicU8::new(0);

fn tri(state: &AtomicU8, knob: &str) -> bool {
    match state.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = crate::env_knobs::bool_knob(knob, false);
            state.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// The `SQUEEZEFS_DEVICE_OVERLAY` knob (registry entry in
/// `src/env_knobs.rs`; default OFF until the B5 gate row — D17's
/// measured-posture rule applies from the bracket on).
pub fn device_overlay_enabled() -> bool {
    tri(&ENABLED, "SQUEEZEFS_DEVICE_OVERLAY")
}

/// Whether `WritePayload::Bytes` may ride the overlay store.
///
/// Production: follows [`device_overlay_enabled`] — O_DIRECT overlay
/// extracts at delivery (batched worker copy) and the handler's Bytes
/// ARE the snapshot; refusing them forced a HOLD + late extract.
/// Test seam: [`set_device_overlay_for_tests`] still pins on/off
/// independently. Unset + `SQUEEZEFS_TEST_OVERLAY_BYTES=1` forces ON
/// even when overlay is off (in-process suites that do not call the
/// setter). Never cache a 1 on the unset arm — that would freeze the
/// vehicle off across a later overlay enable.
pub fn bytes_vehicle_armed() -> bool {
    match BYTES_VEHICLE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            crate::env_knobs::bool_knob("SQUEEZEFS_TEST_OVERLAY_BYTES", false)
                || device_overlay_enabled()
        }
    }
}

/// ACK-early enablement (`SQUEEZEFS_ZC_ACK_EARLY`, default ON —
/// engagement additionally requires an armed overlay, a
/// retention-negotiated transport, and the §3.4 class gate below, so
/// the ON default is inert on every shipped posture).
static ACK_EARLY: AtomicU8 = AtomicU8::new(0);
/// The O_DIRECT ACK-early opt-in (`SQUEEZEFS_ZC_ACK_EARLY_ODIRECT`,
/// default OFF): O_DIRECT/GUP writes may ACK early only under it — a
/// post-ACK buffer reuse persists scribbled bytes, the NFS
/// UNSTABLE-class contract the operator must explicitly accept.
static ACK_EARLY_ODIRECT: AtomicU8 = AtomicU8::new(0);

fn tri_default_on(state: &AtomicU8, knob: &str) -> bool {
    match state.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = crate::env_knobs::bool_knob(knob, true);
            state.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// The `SQUEEZEFS_ZC_ACK_EARLY` lever (registry entry in
/// `src/env_knobs.rs`).
pub fn ack_early_enabled() -> bool {
    tri_default_on(&ACK_EARLY, "SQUEEZEFS_ZC_ACK_EARLY")
}

/// The `SQUEEZEFS_ZC_ACK_EARLY_ODIRECT` snapshot-then-ACK opt-in.
pub fn ack_early_odirect() -> bool {
    tri(&ACK_EARLY_ODIRECT, "SQUEEZEFS_ZC_ACK_EARLY_ODIRECT")
}

/// Test override (the `set_patch_max_bytes` precedent): pins both the
/// enablement and the bytes-vehicle seam without env-order coupling.
pub fn set_device_overlay_for_tests(enabled: bool, bytes_vehicle: bool) {
    ENABLED.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
    BYTES_VEHICLE.store(if bytes_vehicle { 2 } else { 1 }, Ordering::Relaxed);
}

/// Test override for the ACK-early pair.
pub fn set_ack_early_for_tests(enabled: bool, odirect_opt_in: bool) {
    ACK_EARLY.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
    ACK_EARLY_ODIRECT.store(if odirect_opt_in { 2 } else { 1 }, Ordering::Relaxed);
}

/// The registry-wide fast path (§5.2 Resolved Questions #5): one
/// relaxed gauge load — every mount with no live overlay pays nothing
/// on the read/write probe hooks.
pub fn any_open_fast() -> bool {
    crate::fuse_client::METRICS
        .overlay_open
        .load(Ordering::Relaxed)
        != 0
}

/// One live overlay record: the pure core + the prod custody.
pub struct DeviceOverlayRecord {
    /// The laws (state machine, generation, coverage, claims).
    pub core: OverlayRecordCore,
    /// Destination backend (durable `vol-` id per KD-5 — never a path).
    pub be_id: String,
    /// Destination device offset (unpublished until the map flip).
    pub dest_offset: u64,
    /// The destination device (stores + the §6.2 step-4 barrier).
    pub device: Arc<crate::nvme_dev::NvmeBlockDev>,
    /// The destination allocator (law 9's rollback target).
    pub allocator: Arc<crate::block_allocator::BlockAllocator>,
    /// The ino's DLM fencing token at install (publication re-presents
    /// the CURRENT generation via the fencing retry law).
    pub fence_token: u64,
    /// fsck C2/C3 visibility ONLY (correction A).
    pub(crate) fsck_guard: std::sync::Mutex<Option<crate::block_allocator::InflightAllocGuard>>,
    /// Law 9's mint rollback owner (KD-OV-11).
    pub(crate) mint_owner: std::sync::Mutex<Option<crate::assembly_tasks::MintedBlockGuard>>,
    /// Waiters on the record leaving the registry (frozen-record
    /// writers, drain rendezvous).
    pub retired: tokio::sync::Notify,
}

impl DeviceOverlayRecord {
    /// Terminal teardown disposition (law 9 / KD-OV-11 — see module
    /// docs). Callers prove `core.rollback_admissible()` first.
    pub(crate) fn run_teardown_disposition(&self) {
        debug_assert!(self.core.rollback_admissible());
        let mut mint = self
            .mint_owner
            .lock()
            .expect("overlay mint owner mutex poisoned");
        match self.core.state() {
            crate::overlay_core::OverlayState::Published => {
                // Ownership transferred to the durable map: disarm.
                if let Some(m) = mint.as_mut() {
                    m.disarm();
                }
            }
            crate::overlay_core::OverlayState::FenceDropped => {
                // W5: NOTHING freed post-fence — disarm without freeing
                // (successor recovery owns the accounting; the offset is
                // deliberately left to the census).
                if let Some(m) = mint.as_mut() {
                    m.disarm();
                }
            }
            crate::overlay_core::OverlayState::Superseded => {
                // The guard's drop frees the unpublished destination.
            }
            s => {
                crate::note_invariant_tripwire(
                    "overlay_teardown_nonterminal",
                    "overlay teardown disposition ran on a non-terminal record",
                );
                let _ = s;
                if let Some(m) = mint.as_mut() {
                    m.disarm();
                }
            }
        }
        *mint = None; // Superseded: the armed guard drops HERE (frees).
        *self
            .fsck_guard
            .lock()
            .expect("overlay fsck guard mutex poisoned") = None;
    }
}

/// The per-daemon registry: `(ino, block)` → live record.
#[derive(Default)]
pub struct DeviceOverlayRegistry {
    records: scc::HashMap<(u64, u32), Arc<DeviceOverlayRecord>>,
    birth: AtomicU64,
}

impl DeviceOverlayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Exact lookup (callers run [`any_open_fast`] first).
    pub fn get(&self, ino: u64, block: u32) -> Option<Arc<DeviceOverlayRecord>> {
        self.records.read_sync(&(ino, block), |_, r| r.clone())
    }

    /// The live records of one ino (drain enumeration). scc scan —
    /// drain-frequency work, never the hot path.
    pub fn blocks_of(&self, ino: u64) -> Vec<u32> {
        let mut out = Vec::new();
        self.records.iter_sync(|&(i, b), _| {
            if i == ino {
                out.push(b);
            }
            true
        });
        out.sort_unstable();
        out
    }

    /// Every live `(ino, block)` (unmount drain).
    pub fn all(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        self.records.iter_sync(|&k, _| {
            out.push(k);
            true
        });
        out.sort_unstable();
        out
    }

    /// Install a fresh record (callers hold the block's
    /// `BLOCK_FLUSH_LOCKS` guard — the §10 install order). Returns the
    /// record, or `None` if one already exists (the caller joins it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install(
        &self,
        ino: u64,
        block: u32,
        block_size: u32,
        be_id: String,
        dest_offset: u64,
        device: Arc<crate::nvme_dev::NvmeBlockDev>,
        allocator: Arc<crate::block_allocator::BlockAllocator>,
        fence_token: u64,
        fsck_guard: crate::block_allocator::InflightAllocGuard,
        mint_owner: crate::assembly_tasks::MintedBlockGuard,
    ) -> Option<Arc<DeviceOverlayRecord>> {
        let birth = self.birth.fetch_add(1, Ordering::Relaxed) + 1;
        let rec = Arc::new(DeviceOverlayRecord {
            core: OverlayRecordCore::new(block_size, birth),
            be_id,
            dest_offset,
            device,
            allocator,
            fence_token,
            fsck_guard: std::sync::Mutex::new(Some(fsck_guard)),
            mint_owner: std::sync::Mutex::new(Some(mint_owner)),
            retired: tokio::sync::Notify::new(),
        });
        match self.records.entry_sync((ino, block)) {
            scc::hash_map::Entry::Occupied(_) => None,
            scc::hash_map::Entry::Vacant(vac) => {
                vac.insert_entry(rec.clone());
                crate::fuse_client::METRICS
                    .overlay_open
                    .fetch_add(1, Ordering::Relaxed);
                crate::fuse_client::METRICS
                    .overlay_installs
                    .fetch_add(1, Ordering::Relaxed);
                Some(rec)
            }
        }
    }

    /// Remove a TERMINAL record (its teardown disposition already ran)
    /// and wake every waiter.
    pub(crate) fn retire(&self, ino: u64, block: u32, rec: &Arc<DeviceOverlayRecord>) {
        if let Some((_, r)) = self.records.remove_sync(&(ino, block)) {
            debug_assert!(Arc::ptr_eq(&r, rec));
            crate::gauge_core::sub_saturating(&crate::fuse_client::METRICS.overlay_open, 1);
        }
        rec.retired.notify_waiters();
    }
}
