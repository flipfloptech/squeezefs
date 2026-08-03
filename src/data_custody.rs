//! DLM **stage S7** — the data plane's custody-epoch fence, the dead-epoch
//! allocation quarantine, and the WERO hold on data namespaces
//! (`docs/pre-rc-engineering-spec.md` §6.9 S7 row, §6.7 "Recovery",
//! §7 **RES-6**, risk **R2**).
//!
//! # Why an epoch and not a boolean
//!
//! RES-6's local face gates every DMA submission on the D0 writer guard's
//! per-volume `failed` latch (`nvme_dev::DeviceFence`) — one relaxed load
//! per submit, refusing [`crate::error::SqueezefsError::WriterGuardFenced`]
//! and counting `data_dma_fence_refusals`. That answers *"is this device's
//! probe latched right now"*, which stops being the right question the
//! moment custody can move:
//!
//! * a write authorized under custody epoch E, parked in the write
//!   pipeline, and submitted after the mount lost E passes a latch whose
//!   probe has not fired yet;
//! * a submission on a device registered without the probe (or a sibling
//!   volume whose own latch has not been evaluated) is not gated at all;
//! * an S9 remote client's DMA carries custody the LOCAL latch knows
//!   nothing about.
//!
//! S7 makes the fence **epoch-bearing**, and **S9 gives the epoch its
//! per-client half**: the low [`crate::dlm::GRANT_SEQ_BITS`] carry this
//! process's [`custody_generation`], which advances when one of its remote
//! grants dies ([`advance_custody_generation`]) and NEVER poisons — losing
//! custody costs the work authorized under it, not the mount. The
//! generation is 0 on every single-writer mount, so the epoch there is
//! exactly the era's base and every shipped behaviour is byte-identical.
//!
//! [`authorize_dma`] is THE
//! authorization point — the W1 predicate's "the predicate lives in ONE
//! place" discipline applied to DMA submission: every data-plane write
//! passes through it (the device submit gate calls it; every carrier of an
//! earlier-captured epoch validates through it), and a submission whose
//! epoch is no longer current is refused there, before any device work.
//!
//! **The epoch is the durable writer term** (spec §6.7 decision 4): the
//! successor of a failed mount bumps `WriterClaim.term` durably *before*
//! arming, so every token — and now every DMA authorization — minted in an
//! earlier era is stale by construction. On an un-stamped volume (no
//! incompat bit 7) the term is 0 and the epoch is a constant: the fence
//! degrades to exactly the pre-S7 latch behavior, which is the honest
//! posture for a format that cannot express eras.
//!
//! A fence observation additionally **poisons** process custody: the D0
//! fail-stop lattice is mount-wide (`disabled_volumes` mirrors any
//! volume's `failed` latch, and `block_reclaim` already ceases ALL device
//! reclaims on the first observation), so the write path must not wait for
//! each device to evaluate the same probe. Poison is sticky — a fenced
//! holder is dead until remount.
//!
//! # The dead-epoch allocation quarantine
//!
//! §6.7 "Recovery": when a client epoch dies, *"blocks allocated under
//! that epoch enter a do-not-reallocate quarantine until the epoch is
//! proven drained — the job wire's fresh-destination law applied
//! verbatim"*. [`BlockQuarantine`] is that law pushed DOWN into
//! [`crate::block_allocator::BlockAllocator`], where it becomes
//! structural instead of asserted:
//!
//! * **admission** takes the offset out of the free list (the
//!   `claim_free_for_trim` claim protocol) so no allocation path — free
//!   list, contiguity pick, ascending pick — can hand it out;
//! * a quarantined offset's `finish_free` **defers its publish** instead
//!   of returning it to the free list (so recovery, fsck repair or the
//!   reclaimer freeing a dead epoch's block cannot make it reallocatable);
//! * **release requires a drain proof** — the caller states that the dead
//!   epoch can no longer submit (the job wire's PR preempt of the victim
//!   host is exactly such a proof), and only then is the deferred publish
//!   performed.
//!
//! **Pressure ruling (pinned by `tests/dlm_data_fence_tests.rs`): a
//! quarantine never force-drains.** With the whole free list quarantined,
//! allocation refuses `StorageFull` promptly and honestly. Draining the
//! quarantine to satisfy an allocation would hand a possibly-live zombie's
//! offset to a new owner — silent cross-writer corruption, the exact
//! failure the quarantine exists to prevent. ENOSPC is a bounded
//! availability cost; the drain proof is the recovery act. The ENOSPC
//! valve's queued-reclaim drain still runs first, so a quarantine can
//! never mask reclaimable supply.
//!
//! # WERO on data namespaces
//!
//! Write Exclusive – Registrants Only (rtype 2) on the DATA namespaces is
//! the cross-host half: a zombie's DMA is rejected by the DEVICE, not by
//! its own latch. The protocol machinery is
//! [`crate::meta_backend::reservation`]'s (D0's rtype-1 Write Exclusive on
//! meta volumes is disjoint — different namespaces, different rtypes), and
//! the acquire ladder used to live in `job_wire`. It lives HERE now, once,
//! with a **shared hold**: the job-wire coordinator's first-enrollment
//! fence and an S7-armed mount join the SAME reservation key. A second
//! independent acquire would conflict at the device and silently downgrade
//! the job wire's guarantee class to `deferred-reclaim`.
//!
//! **Multi-writer refuses to arm where the substrate cannot enforce**
//! (§6.7 "On external consensus"): a namespace advertising no reservation
//! support is a detection-grade substrate — every developer box and the
//! repo's own loop `dev_substrate.sh` shape — and the refusal names it.
//! It also refuses a format that does not carry
//! [`crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA`],
//! which is every volume today (ruling **D9**: the bit is built, nothing
//! stamps it). The single-writer posture never refuses: the D0 guard
//! governs, the data plane is fenced locally by the custody epoch, and the
//! guarantee-class table in `docs/operations.md` states the difference.

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::reservation::{register_ladder, resolve_for_mount, ReservationClient};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

// ---------------------------------------------------------------------------
// The custody epoch and the ONE authorization point
// ---------------------------------------------------------------------------

/// The custody epoch a data-plane DMA is authorized under — this mount's
/// durable writer era (see the module docs). Cheap to carry (one `u64`),
/// cheap to check (one comparison against one atomic load).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustodyEpoch(u64);

impl CustodyEpoch {
    /// The raw era value (diagnostics, S9's wire encoding).
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct an epoch from its raw value — the inverse of
    /// [`Self::raw`]: an epoch that travelled (S9's grant on the cluster
    /// wire) or a deliberately foreign one (the bench's refused arm).
    /// Never a way to MINT one: only [`current_epoch`] does that, and
    /// [`authorize_dma`] compares against it.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for CustodyEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "epoch:{:#x}", self.0)
    }
}

/// Sticky process-custody poison: set on the first D0 fence observation
/// from any device or volume. Never cleared in production (a fenced holder
/// is dead until remount); [`test_clear_poison`] is the suite seam.
static POISONED: AtomicBool = AtomicBool::new(false);

/// DLM **S9**: this process's **custody generation** — the low
/// [`crate::dlm::GRANT_SEQ_BITS`] of the epoch, and the per-client
/// granularity S7 reserved for this stage (*"the low 40 bits are free if
/// you need per-client granularity; extend `current_epoch()`'s
/// composition — never make the check ambient"*).
///
/// **0 on every mount that does not hold remote write custody**, so
/// [`current_epoch`] is then exactly S7's `term_base()` and every shipped
/// behaviour is byte-identical.
static CUSTODY_GEN: AtomicU64 = AtomicU64::new(0);

/// This process's custody generation (`0` = the shipped posture: the epoch
/// IS the durable era's base).
pub fn custody_generation() -> u64 {
    CUSTODY_GEN.load(Ordering::Acquire)
}

/// This mount's live data-plane custody epoch.
pub fn current_epoch() -> CustodyEpoch {
    // `term_base()` = `(durable_term << GRANT_SEQ_BITS)` — the era's floor,
    // which changes iff the durable writer term changes and never within
    // an era. S9 composes the per-client CUSTODY GENERATION into the free
    // low bits, so a client that loses one grant retires its own
    // in-flight authorizations without touching the era (which belongs to
    // the whole set) and without poisoning (which is terminal).
    CustodyEpoch(crate::dlm::term_base() | (CUSTODY_GEN.load(Ordering::Acquire) & GEN_MASK))
}

/// The generation field's width — the grant field of a composed token, so
/// an epoch and a fencing token are the same shape and a carry into the
/// term field is impossible by construction.
const GEN_MASK: u64 = crate::dlm::GRANT_SEQ_MAX;

/// DLM **S9**: adopt a custody generation an owner minted for us
/// (monotone — a reordered or replayed grant can never lower it).
///
/// The value is the grant field of the owner's fencing token, so
/// generations are unique and increasing per authority by construction:
/// the authority's single mint is what orders them.
pub fn adopt_custody_generation(generation: u64) -> CustodyEpoch {
    let generation = generation & GEN_MASK;
    let prev = CUSTODY_GEN.fetch_max(generation, Ordering::AcqRel);
    if generation > prev {
        log::debug!(
            "data-plane custody generation {generation} adopted (was {prev}): authorizations \
             minted under the earlier generation are void (DLM S9)"
        );
    }
    current_epoch()
}

/// DLM **S9**: **advance** the custody generation — the non-fatal custody
/// change S7 specified for this stage (*"anything needing a non-fatal
/// custody change must advance the epoch, not poison it"*).
///
/// Every authorization minted under the previous generation is refused at
/// [`authorize_dma`] (counted in `data_dma_epoch_refusals`), while the
/// mount stays alive and may acquire fresh custody immediately: losing one
/// grant costs the work authorized under it, never the process.
///
/// Saturation is a **poison**, not a wrap: `GRANT_SEQ_MAX` advances in one
/// era is 10¹² revocations, and a carry into the term field would forge a
/// newer era — the same refusal the fencing mint makes.
pub fn advance_custody_generation(reason: &str) -> CustodyEpoch {
    let prev = CUSTODY_GEN.fetch_add(1, Ordering::AcqRel);
    if prev >= GEN_MASK {
        CUSTODY_GEN.store(GEN_MASK, Ordering::Release);
        poison(&format!(
            "custody generation space exhausted after {prev} advances in writer term {} \
             (a carry into the term field would forge a newer era) — last change: {reason}",
            crate::dlm::durable_term()
        ));
        return current_epoch();
    }
    crate::fuse_client::METRICS
        .dlm_custody_epoch_advances
        .fetch_add(1, Ordering::Relaxed);
    let now = current_epoch();
    log::warn!(
        "data-plane custody generation advanced {prev} → {} ({reason}): every DMA authorized \
         under the previous generation is now refused at the authorization point, and this \
         mount may acquire fresh custody immediately (DLM S9; dlm_custody_epoch_advances)",
        prev + 1
    );
    now
}

/// **Test seam** (the [`test_clear_poison`] precedent): reset the custody
/// generation to the shipped 0. Production has no reset path — the
/// generation is monotone for the life of the process.
pub fn test_reset_custody_generation() {
    CUSTODY_GEN.store(0, Ordering::Release);
}

/// `true` ⇔ this process's data-plane custody is poisoned (fenced /
/// fail-stopped). Sticky.
pub fn poisoned() -> bool {
    POISONED.load(Ordering::Acquire)
}

/// **THE authorization point for data-plane DMA** (see the module docs).
///
/// * `carried = None` — the caller authorizes at submission (every
///   pre-S7 call site: correct under the D0 single-writer guard, where the
///   only thing that can happen between capture and submit is the fence
///   this call observes).
/// * `carried = Some(e)` — the caller captured `e` when its custody was
///   established (write-pipeline admission, and S9's remote grants) and
///   presents it now. A stale `e` is refused here.
///
/// Refusals are loud, classified [`SqueezefsError::WriterGuardFenced`]
/// (the write pipeline's [`crate::write_pipeline::PipelineDisposition::FenceDrop`]
/// — publish nothing, free nothing, successor accounting owns it) and
/// counted in `data_dma_fence_refusals`, with the epoch class split out
/// into `data_dma_epoch_refusals`.
#[inline]
pub fn authorize_dma(carried: Option<CustodyEpoch>) -> Result<CustodyEpoch> {
    let current = current_epoch();
    if POISONED.load(Ordering::Relaxed) {
        METRICS
            .data_dma_fence_refusals
            .fetch_add(1, Ordering::Relaxed);
        return Err(SqueezefsError::WriterGuardFenced);
    }
    if let Some(carried) = carried {
        if carried != current {
            METRICS
                .data_dma_fence_refusals
                .fetch_add(1, Ordering::Relaxed);
            METRICS
                .data_dma_epoch_refusals
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "data-plane DMA refused: authorized under {carried}, current custody is \
                 {current} — this mount's custody moved (successor term bump / revoked \
                 grant) and the offsets it authorized may already be reallocated \
                 (data_dma_epoch_refusals; DLM S7)"
            );
            return Err(SqueezefsError::WriterGuardFenced);
        }
    }
    Ok(current)
}

/// Poison process custody: the D0 fence fired (called from the device
/// submit gate's first latch observation). Idempotent and loud once.
pub fn poison(reason: &str) {
    if !POISONED.swap(true, Ordering::AcqRel) {
        log::error!(
            "data-plane custody POISONED ({reason}) — every DMA authorization in this \
             process is void and no new one is minted; a fenced holder is dead until \
             remount (DLM S7; counted in data_dma_fence_refusals)"
        );
    }
}

/// **Test seam** (the [`crate::dlm::test_swap_grant_seq`] precedent):
/// clear the sticky poison latch. Production has no clear path — a fenced
/// holder is dead until remount; this exists so one test can fence the
/// process without ending the test binary.
pub fn test_clear_poison() {
    POISONED.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Dead epochs and the block quarantine
// ---------------------------------------------------------------------------

/// A dead custody epoch: the cohort identity a quarantine is keyed on.
/// Minted by [`declare_dead_epoch`] when an epoch is *proven or presumed
/// dead* (TTL fired, provable death, revoked grant) — never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeadEpoch(u64);

impl DeadEpoch {
    /// The cohort id (diagnostics / durable records).
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for DeadEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dead-epoch:{}", self.0)
    }
}

static DEAD_EPOCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Declare a custody epoch dead and mint its quarantine cohort id. Loud:
/// a dead epoch is always worth a line in the log, and `reason` is what
/// tells an operator which client died and how it was proven.
pub fn declare_dead_epoch(reason: &str) -> DeadEpoch {
    let epoch = DeadEpoch(DEAD_EPOCH_SEQ.fetch_add(1, Ordering::AcqRel) + 1);
    log::warn!(
        "custody {epoch} declared DEAD ({reason}) — its blocks enter the do-not-reallocate \
         quarantine until the epoch is proven drained (DLM S7; dlm_quarantined_offsets)"
    );
    epoch
}

/// One quarantined offset.
struct Quarantined {
    epoch: DeadEpoch,
    /// The offset's terminal free completed while it was quarantined, so
    /// its free-list publish is OWED to the release (never performed
    /// early — that is exactly the reallocation the quarantine forbids).
    free_pending: bool,
}

/// The dead-epoch **do-not-reallocate** set for one volume's allocator
/// (see the module docs). Empty on every single-writer mount: the probe
/// the allocation and free paths pay is one lock-free lookup on an empty
/// `scc::HashMap`.
///
/// Bounded by the offsets of dead epochs awaiting a drain proof — a
/// recovery-window population, not a per-op one — and every entry leaves
/// on [`Self::release`].
#[derive(Default)]
pub struct BlockQuarantine {
    entries: scc::HashMap<u64, Quarantined>,
}

impl BlockQuarantine {
    /// An empty quarantine (no allocation until the first admission).
    pub fn new() -> Self {
        Self {
            entries: scc::HashMap::new(),
        }
    }

    /// Admit `offset` to `epoch`'s cohort. `true` ⇔ newly admitted
    /// (re-admission is idempotent — a re-declared cohort must not
    /// double-count the gauge).
    pub fn admit(&self, offset: u64, epoch: DeadEpoch) -> bool {
        match self.entries.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(_) => false,
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(Quarantined {
                    epoch,
                    free_pending: false,
                });
                METRICS
                    .dlm_quarantined_offsets
                    .fetch_add(1, Ordering::Relaxed);
                true
            }
        }
    }

    /// `true` ⇔ `offset` is quarantined (the allocation/free-path probe).
    #[inline]
    pub fn contains(&self, offset: u64) -> bool {
        self.entries.read_sync(&offset, |_, _| ()).is_some()
    }

    /// Record that `offset`'s terminal free completed. `true` ⇔ the
    /// offset is quarantined and the caller must **not** publish it to
    /// the free list; the publish is owed to [`Self::release`].
    pub fn defer_free(&self, offset: u64) -> bool {
        self.entries
            .update_sync(&offset, |_, q| {
                q.free_pending = true;
            })
            .is_some()
    }

    /// Release `epoch`'s whole cohort — **the drain proof**. Returns the
    /// offsets released, each paired with whether its free-list publish
    /// is owed (its free completed while quarantined).
    pub fn release(&self, epoch: DeadEpoch) -> Vec<(u64, bool)> {
        let mut victims: Vec<u64> = Vec::new();
        self.entries.iter_sync(|offset, q| {
            if q.epoch == epoch {
                victims.push(*offset);
            }
            true
        });
        let mut out = Vec::with_capacity(victims.len());
        for offset in victims {
            if let Some((offset, q)) = self.entries.remove_sync(&offset) {
                METRICS
                    .dlm_quarantined_offsets
                    .fetch_sub(1, Ordering::Relaxed);
                METRICS
                    .dlm_quarantine_releases
                    .fetch_add(1, Ordering::Relaxed);
                out.push((offset, q.free_pending));
            }
        }
        out
    }

    /// Live quarantined offsets on this volume.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` ⇔ nothing is quarantined (the common case).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// WERO on data namespaces — one shared hold
// ---------------------------------------------------------------------------

/// Which custody posture a mount arms the data plane in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyPosture {
    /// The shipped posture: the D0 guard is the arbiter, the data plane is
    /// fenced locally by the custody epoch, and no data-namespace
    /// reservation is taken. Never refuses.
    SingleWriter,
    /// The S7 opt-in (`SQUEEZEFS_MULTI_WRITER=1`): the data plane must be
    /// **device-enforced**. Refuses a substrate without reservation
    /// support and a format without the S7 incompat bit.
    MultiWriter,
}

/// Which half of a WERO (rtype 2) hold this process owns.
///
/// The distinction is the whole of DLM S9's co-writer admission rung 5:
/// under Write Exclusive – Registrants Only **every registrant writes**,
/// so the AUTHORITY acquires the reservation exactly once and every
/// co-writer merely REGISTERS under it. A second acquire would conflict at
/// the device and silently downgrade the guarantee class (the
/// join-never-fork law), and a co-writer that released the reservation on
/// its way out would drop the fence for the whole set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum HoldRole {
    /// This process ACQUIRED the reservation (the authority / the job-wire
    /// coordinator). Teardown releases it.
    Holder,
    /// This process is a REGISTRANT under somebody else's standing
    /// reservation (a co-writer). Teardown unregisters only — never
    /// releases, because the reservation is not ours to end.
    Registrant,
}

/// The process's live WERO hold on one data-namespace set. Dropping the
/// last [`WeroHold`] releases the reservation and its registration (zero
/// residue). Release issues one-shot ioctls per namespace: drop it from a
/// blocking context (`spawn_blocking`) on the async paths.
struct WeroInner {
    key: u64,
    paths: Vec<PathBuf>,
    clients: Vec<Arc<dyn ReservationClient>>,
    role: HoldRole,
}

impl Drop for WeroInner {
    fn drop(&mut self) {
        for client in &self.clients {
            let outcome = match self.role {
                HoldRole::Holder => client.release_registrants_only(self.key),
                // A registrant ends its OWN registration and nothing else:
                // the reservation belongs to the authority, and
                // `unregister` is same-host-scoped by the device.
                HoldRole::Registrant => client.unregister(self.key),
            };
            if let Err(e) = outcome {
                log::warn!("data-plane WERO {:?} teardown failed: {e}", self.role);
            }
        }
        registry()
            .lock()
            .unwrap()
            .remove(&(self.paths.clone(), self.role));
        METRICS.data_plane_fence_mode.store(0, Ordering::Relaxed);
        log::info!(
            "data-plane WERO {:?} released on {} namespace(s) (last reference departed)",
            self.role,
            self.paths.len()
        );
    }
}

/// A reference to the process's WERO hold on a data-namespace set. Clone
/// it (or acquire again) to join the SAME reservation — never to take a
/// second one.
#[derive(Clone)]
pub struct WeroHold {
    inner: Arc<WeroInner>,
}

impl std::fmt::Debug for WeroHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeroHold")
            .field("key", &format_args!("{:#x}", self.inner.key))
            .field("namespaces", &self.inner.paths.len())
            .finish()
    }
}

impl WeroHold {
    /// The reservation key this hold stands on.
    pub fn key(&self) -> u64 {
        self.inner.key
    }

    /// PREEMPT `victim_key`'s registration under the standing WERO — the
    /// dead-epoch fence at the device: the victim host's resumed DMA is
    /// rejected while every other registrant keeps writing (§5.1.6
    /// rung 2). Returns the namespaces where the preempt landed; that
    /// count is the **drain proof** a quarantine release needs. Blocking
    /// (one-shot ioctls).
    pub fn preempt(&self, victim_key: u64) -> u64 {
        let mut n = 0;
        for client in &self.inner.clients {
            match client.preempt_registrants_only(self.inner.key, victim_key) {
                Ok(()) => n += 1,
                Err(e) => log::warn!("data-plane WERO preempt of key {victim_key:#x} failed: {e}"),
            }
        }
        n
    }
}

/// The per-process WERO registry, keyed by the namespace set AND the role
/// held over it: one process is either the reservation's holder or a
/// registrant under somebody else's, and conflating the two would hand a
/// co-writer's join the authority's release-on-drop.
type WeroRegistry = Mutex<HashMap<(Vec<PathBuf>, HoldRole), Weak<WeroInner>>>;

fn registry() -> &'static WeroRegistry {
    static REG: OnceLock<WeroRegistry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The reservation key of the process's live WERO hold, when it has one
/// (`None` = detection grade). DLM **S6** records it in the §6.2 item-7
/// claim set, which is how a set's membership names its DEVICE-side
/// registrants; it is deliberately a READ of the standing hold, never a
/// second acquire (a second reservation would conflict at the device and
/// silently downgrade the guarantee class — the join-never-fork law).
pub fn live_wero_key() -> Option<u64> {
    registry()
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .map(|inner| inner.key)
        .next()
}

/// The process's guarantee class for the data plane: `pr` while a WERO
/// hold stands on a data-namespace set, else `detection` (the local
/// custody-epoch fence only). The `data_plane_fence_mode` gauge's word
/// form.
pub fn wero_mode() -> &'static str {
    let live = registry()
        .lock()
        .unwrap()
        .values()
        .any(|w| w.strong_count() > 0);
    if live {
        "pr"
    } else {
        "detection"
    }
}

fn canonical(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = paths.to_vec();
    v.sort();
    v.dedup();
    v
}

/// Acquire (or JOIN) the process's WERO hold on `paths` — the ONE
/// data-namespace reservation implementation (the job-wire coordinator's
/// first-enrollment fence and an S7-armed mount are both callers).
///
/// `Some` only when EVERY namespace is PR-capable and every acquire
/// succeeded (the `pr` class); anything partial releases what it took and
/// returns `None` (the caller's documented degrade — for the job wire,
/// `deferred-reclaim`). Blocking: call via `spawn_blocking` from async
/// paths.
pub fn acquire_wero(paths: &[PathBuf]) -> Option<WeroHold> {
    if paths.is_empty() {
        return None;
    }
    let key_set = canonical(paths);
    let mut reg = registry().lock().unwrap();
    if let Some(existing) = reg
        .get(&(key_set.clone(), HoldRole::Holder))
        .and_then(Weak::upgrade)
    {
        log::info!(
            "data-plane WERO: joining the standing hold on {} namespace(s) (key {:#x}) — \
             a second reservation would conflict at the device",
            existing.paths.len(),
            existing.key
        );
        return Some(WeroHold { inner: existing });
    }
    let key = loop {
        let k = rand::Rng::gen::<u64>(&mut rand::thread_rng());
        if k != 0 {
            break k;
        }
    };
    let mut clients: Vec<Arc<dyn ReservationClient>> = Vec::new();
    for path in &key_set {
        let Some(client) = resolve_for_mount(path) else {
            log::warn!(
                "data-plane WERO: namespace {} advertises no reservation support — fence \
                 unavailable (detection grade)",
                path.display()
            );
            release_partial(&clients, key);
            return None;
        };
        let step = register_ladder(client.as_ref(), key)
            .and_then(|_| client.acquire_write_exclusive_registrants_only(key));
        match step {
            Ok(()) => clients.push(client),
            Err(e) => {
                log::warn!(
                    "data-plane WERO: acquire on {} failed: {e} — fence unavailable",
                    path.display()
                );
                release_partial(&clients, key);
                return None;
            }
        }
    }
    let inner = Arc::new(WeroInner {
        key,
        paths: key_set.clone(),
        clients,
        role: HoldRole::Holder,
    });
    reg.insert((key_set, HoldRole::Holder), Arc::downgrade(&inner));
    METRICS.data_plane_fence_mode.store(1, Ordering::Relaxed);
    log::info!(
        "data-plane WERO (rtype 2) acquired on {} namespace(s), key {key:#x} — guarantee \
         class pr: an unregistered (fenced) host's writes are rejected by the device",
        inner.paths.len()
    );
    Some(WeroHold { inner })
}

fn release_partial(clients: &[Arc<dyn ReservationClient>], key: u64) {
    for client in clients {
        if let Err(e) = client.release_registrants_only(key) {
            log::warn!("data-plane WERO: partial release failed: {e}");
        }
    }
}

fn unregister_partial(clients: &[Arc<dyn ReservationClient>], key: u64) {
    for client in clients {
        if let Err(e) = client.unregister(key) {
            log::warn!("data-plane WERO: partial unregister failed: {e}");
        }
    }
}

/// A CO-WRITER's registration under the authority's standing WERO hold,
/// plus the device-side evidence its admission rung 5 consumes. Dropping
/// it unregisters this process's key on every namespace and leaves the
/// authority's reservation standing (zero residue, no fence loss).
pub struct WeroRegistrantJoin {
    hold: WeroHold,
    evidence: crate::cowriter::RegistrantEvidence,
}

impl std::fmt::Debug for WeroRegistrantJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeroRegistrantJoin")
            .field("hold", &self.hold)
            .field("evidence", &self.evidence)
            .finish()
    }
}

impl WeroRegistrantJoin {
    /// The device-side evidence: PR capability, the standing hold's type,
    /// whether a reservation is held at all, and whether OUR key is a
    /// registrant.
    pub fn evidence(&self) -> crate::cowriter::RegistrantEvidence {
        self.evidence
    }

    /// The underlying hold reference (its `preempt` is the drain proof a
    /// dead peer's quarantine release needs).
    pub fn hold(&self) -> &WeroHold {
        &self.hold
    }
}

/// **DLM S9 — the co-writer's device half**: REGISTER this process's key
/// under the authority's *standing* WERO (rtype 2) reservation on every
/// data namespace, and report what the device says.
///
/// This is deliberately not [`acquire_wero`]: a co-writer must never
/// acquire. Under Write Exclusive – Registrants Only every registrant
/// writes, so the authority acquires once and each co-writer registers —
/// a second acquire would conflict at the device (and, on a target that
/// answered it, would silently take the fence away from the authority).
/// Registration is also what makes a co-writer *fenceable*: the
/// authority's `preempt_registrants_only` of this key is what turns a dead
/// co-writer's death into a drain proof.
///
/// Refuses (never degrades — the admission ladder's rung 5 is a
/// guarantee-class demand):
///   1. a namespace advertising no reservation support, naming it;
///   2. a register that did not land, naming the namespace;
///   3. a namespace where NO reservation is held (nobody is fencing this
///      data plane, so a co-writer would be writing beside an
///      unauthenticated peer);
///   4. a namespace whose held reservation is not registrants-only —
///      under rtype 1 our registration grants no write access at all, so
///      admitting would produce a mount whose every DMA is rejected.
///
/// Blocking (reservation ioctls) — call via `spawn_blocking` from async
/// paths.
pub fn join_wero_as_registrant(data_paths: &[PathBuf]) -> Result<WeroRegistrantJoin> {
    if data_paths.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "co-writer WERO join refuses: the mount names no data namespace, so there is \
             nothing to be a registrant of"
                .to_string(),
        ));
    }
    let key_set = canonical(data_paths);
    let mut reg = registry().lock().unwrap();
    if let Some(existing) = reg
        .get(&(key_set.clone(), HoldRole::Registrant))
        .and_then(Weak::upgrade)
    {
        let key = existing.key;
        let namespaces = existing.paths.len();
        log::info!(
            "data-plane WERO: joining this process's standing REGISTRATION on {namespaces} \
             namespace(s) (key {key:#x})"
        );
        return Ok(WeroRegistrantJoin {
            hold: WeroHold { inner: existing },
            evidence: crate::cowriter::RegistrantEvidence {
                pr_capable: true,
                wero: true,
                reservation_held: true,
                registered: true,
                key,
                namespaces,
            },
        });
    }
    let key = loop {
        let k = rand::Rng::gen::<u64>(&mut rand::thread_rng());
        if k != 0 {
            break k;
        }
    };
    let mut clients: Vec<Arc<dyn ReservationClient>> = Vec::new();
    for path in &key_set {
        let Some(client) = resolve_for_mount(path) else {
            unregister_partial(&clients, key);
            return Err(SqueezefsError::InvalidOperation(format!(
                "co-writer WERO join refuses: data namespace {} advertises no NVMe reservation \
                 support (RESCAP=0), so a fenced co-writer's DMA could only be DETECTED, never \
                 rejected — spec §6.7 requires enforcement for multi-writer, and that applies \
                 to the ADMISSION decision, not only to the data plane",
                path.display()
            )));
        };
        if let Err(e) = register_ladder(client.as_ref(), key) {
            unregister_partial(&clients, key);
            return Err(SqueezefsError::InvalidOperation(format!(
                "co-writer WERO join refuses: reservation REGISTER on {} failed: {e} — without \
                 a registrant key this node can neither write under the standing hold nor be \
                 preempted out of it (no preempt, no drain proof)",
                path.display()
            )));
        }
        // Ours from here on: every refusal below unregisters the whole
        // prefix, this namespace included.
        clients.push(Arc::clone(&client));
        let report = match client.report() {
            Ok(r) => r,
            Err(e) => {
                unregister_partial(&clients, key);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "co-writer WERO join refuses: Reservation Report on {} failed: {e} — the \
                     admission cannot verify the standing hold blind",
                    path.display()
                )));
            }
        };
        if !report.is_wero() {
            let held = report.holder_key;
            unregister_partial(&clients, key);
            return Err(SqueezefsError::InvalidOperation(format!(
                "co-writer WERO join refuses: data namespace {} holds no Write Exclusive – \
                 Registrants Only (rtype 2) reservation (holder {held:?}, rtype {}). Either no \
                 authority is fencing this data plane, or it holds an rtype under which OUR \
                 registration grants no write access — admitting would produce a mount whose \
                 every DMA the device rejects. Arm the authority's multi-writer plane first",
                path.display(),
                report.rtype
            )));
        }
        if !report.registered(key) {
            unregister_partial(&clients, key);
            return Err(SqueezefsError::InvalidOperation(format!(
                "co-writer WERO join refuses: our key {key:#x} does not appear among {}'s \
                 registrants after a successful register — the device does not attribute this \
                 host's registration to us, so neither its write admission nor its preempt \
                 would name us",
                path.display()
            )));
        }
    }
    let namespaces = key_set.len();
    let inner = Arc::new(WeroInner {
        key,
        paths: key_set.clone(),
        clients,
        role: HoldRole::Registrant,
    });
    reg.insert((key_set, HoldRole::Registrant), Arc::downgrade(&inner));
    METRICS.data_plane_fence_mode.store(1, Ordering::Relaxed);
    log::warn!(
        "data-plane WERO: REGISTERED as a co-writer under the standing hold on {namespaces} \
         namespace(s), key {key:#x} — this node may write, and the authority's preempt of this \
         key is what fences it (DLM S9 admission rung 5)"
    );
    Ok(WeroRegistrantJoin {
        hold: WeroHold { inner },
        evidence: crate::cowriter::RegistrantEvidence {
            pr_capable: true,
            wero: true,
            reservation_held: true,
            registered: true,
            key,
            namespaces,
        },
    })
}

/// `true` ⇔ this mount was asked to arm the multi-writer data plane
/// (`SQUEEZEFS_MULTI_WRITER=1`).
pub fn multi_writer_requested() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_MULTI_WRITER", false)
}

/// Arm the data plane in `posture` over `data_paths`.
///
/// * [`CustodyPosture::SingleWriter`] — never refuses and takes no
///   reservation: the D0 guard is the arbiter and the custody-epoch fence
///   is the data plane's local face. `Ok(None)`.
/// * [`CustodyPosture::MultiWriter`] — refuses, in this order:
///   1. any namespace that advertises no reservation support, **naming
///      it** (detection-grade substrates cannot enforce a decision — the
///      repo's own loop substrate included, §6.7 "On external
///      consensus");
///   2. a format that does not carry the S7 incompat bit
///      (`multi_writer_stamped == false`, which is every volume today —
///      ruling D9);
///   3. a WERO acquire that did not land on every namespace.
///   Otherwise `Ok(Some(hold))`, held for the mount lifetime.
///
/// Blocking (reservation ioctls) — call via `spawn_blocking` from async
/// paths.
pub fn arm_data_plane(
    posture: CustodyPosture,
    data_paths: &[PathBuf],
    multi_writer_stamped: bool,
) -> Result<Option<WeroHold>> {
    if posture == CustodyPosture::SingleWriter {
        log::info!(
            "data plane armed single-writer: custody {} fenced locally (no data-namespace \
             reservation — the D0 writer guard is the arbiter; set SQUEEZEFS_MULTI_WRITER=1 \
             to demand a device-enforced WERO hold)",
            current_epoch()
        );
        return Ok(None);
    }
    for path in data_paths {
        if resolve_for_mount(path).is_none() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "multi-writer data plane refuses to arm: data namespace {} advertises no \
                 NVMe reservation support (RESCAP=0), so a fenced writer's DMA can only be \
                 DETECTED, never rejected — spec §6.7 requires enforcement for multi-writer \
                 (this is the shape of every loop-device substrate, including the repo's own \
                 tests/dev_substrate.sh default). Use a PR-capable namespace or unset \
                 SQUEEZEFS_MULTI_WRITER.",
                path.display()
            )));
        }
    }
    if !multi_writer_stamped {
        return Err(SqueezefsError::InvalidOperation(
            "multi-writer data plane refuses to arm: the metadata format does not carry the \
             multi-writer data capability (incompat bit 11). Nothing stamps it today (ruling \
             D9: the bit is built, not stamped) — the capability lands with DLM S8/S9. Unset \
             SQUEEZEFS_MULTI_WRITER."
                .to_string(),
        ));
    }
    let hold = acquire_wero(data_paths).ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "multi-writer data plane refuses to arm: the WERO (rtype 2) acquire did not land \
             on every data namespace — a partial fence is not a fence (the acquire log names \
             the namespace that refused)"
                .to_string(),
        )
    })?;
    Ok(Some(hold))
}

/// Mount-path arming (`main`'s mount verb, after the meta backend is
/// wired): resolve the posture from `SQUEEZEFS_MULTI_WRITER`, read the S7
/// capability off every meta volume's superblock, and arm off-runtime.
pub async fn arm_mount_data_plane(
    meta: &Arc<crate::meta_backend::RoutedMetaBackend>,
    data_paths: Vec<PathBuf>,
) -> Result<Option<WeroHold>> {
    let posture = if multi_writer_requested() {
        CustodyPosture::MultiWriter
    } else {
        CustodyPosture::SingleWriter
    };
    let stamped = !meta.volumes.is_empty()
        && meta.volumes.iter().all(|v| {
            v.superblock().features_incompat
                & crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
                != 0
        });
    tokio::task::spawn_blocking(move || arm_data_plane(posture, &data_paths, stamped))
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!("data-plane arming task failed: {e}"))
        })?
}

/// Release a hold off the async runtime (the reservation ioctls are
/// blocking). Unmount teardown and the job wire's last-departure release
/// both go through here.
pub async fn release_hold(hold: WeroHold) {
    let _ = tokio::task::spawn_blocking(move || drop(hold)).await;
}

/// Quarantine every offset of `dests` on `allocator` under `epoch` — the
/// job wire's expired-lease destination quarantine, enforced by the
/// allocator instead of asserted after the fact (§6.7 "Recovery"). Returns
/// the newly admitted count.
pub fn quarantine_offsets(
    allocator: &crate::block_allocator::BlockAllocator,
    offsets: impl IntoIterator<Item = u64>,
    epoch: DeadEpoch,
) -> usize {
    offsets
        .into_iter()
        .filter(|o| allocator.quarantine_offset(*o, epoch))
        .count()
}
