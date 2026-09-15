//! The small-file packer's **open pack block**
//! (`docs/design-small-file-packing.md` §5.2–§5.4, PR PK2).
//!
//! A staged-layout file whose stored image's slot fits
//! [`crate::routing::pack_max_slot_bytes`] no longer promotes into a whole
//! 4 MiB block of its own (the 64× space law of
//! `.benchmarks/2026-09-09-fsync-promote-staged-ab.md`): it reserves an
//! `LBA_GRAIN`-aligned **slot** in the mount's current
//! open pack block — ONE `fetch_add` on the block's cursor — DMAs its image
//! at `base + off`, and commits the size-carrying mapping `bk:off:len` the
//! read path already decodes, with its C8 reference in the same
//! transaction. N tenants of one block are N durable references; the block
//! frees terminally when the population reaches 0 — the arithmetic the
//! allocator already runs (KD-2).
//!
//! **RAM-only, pinned by a refcount** (KD-3): `allocate_placed_block` hands
//! the packer the block at refcount 1 — that reference is the packer's PIN
//! for the open lifetime (`move_one`'s discipline). Every tenant takes its
//! own reference at reserve, so while a pack is open
//! `refcount = committed + mid-flight tenants + 1` and, on the authority,
//! no sequence of tenant deletes can free the block under a DMA the packer
//! has not finished. The seal releases the pin through
//! [`crate::block_allocator::BlockAllocator::release_pack_reference`] —
//! nonterminal while tenants live, the ordinary terminal free otherwise.
//! No pack-level in-flight guard: every TENANT holds its own
//! `inflight_register(base)` from reserve to commit (the registry is a
//! per-offset counter, so N holders compose), and the pin alone is
//! declared to fsck's shared C2/C3 arm through the pack-open ledger
//! ([`crate::jobs::pack_open_ledger`]), entered at OPEN.
//!
//! **Crash contract** (§5.4): nothing durable records the open block. A
//! crash leaves committed tenants (each with its layout + C8 record —
//! the block recovers ALLOCATED at `refcount = N`) and an uncommitted
//! tail that is dead bytes inside a live block, or — with no tenant
//! committed — a block that recovers FREE. Torn slot DMAs are invisible
//! (no layout names them).
//!
//! **Sealing** (§5.3): on FULL (the reservation that overflows the chunk
//! — the first sealer runs the seal) and at DISMOUNT (after the promotion
//! pass, before "Dismount clean"). Not on idle in v1 — one half-empty
//! block per data volume for the life of the mount is the stated cost,
//! made visible by `pack_open_block_{age_ms,occupancy}`.
//!
//! **OQ-1 (owner, 2026-09-09):** a refill that hits `StorageFull` STOPS
//! the pack arm for the rest of the promotion batch — every remaining
//! file stays resident-and-counted (the never-wrong degrade), ONE
//! aggregated WARN, and the own-block arm is NOT retried (it needs the
//! same space). The next batch (`begin_promotion_batch`) re-arms it.
//!
//! Hot-path law: the packer runs on the merge worker, the dismount pass
//! and the fsync leg — never the WRITE/READ hot path. Its shared words are
//! the `fetch_add` cursor, the allocator's refcount and the incarnation
//! word (all latch-free); the refill is single-flighted on the R1a shape
//! (`sqz_flight`); no lock is held across the allocation's park.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::block_allocator::{BlockAllocator, InflightAllocGuard, CHUNK_SIZE};
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::nvme_dev::NvmeBlockDev;
use crate::routing::BackendRouter;

/// One OPEN pack block — the slot cursor N concurrent promotions append
/// into. RAM-only: a crash leaves the committed tenants (each with its
/// durable layout + C8 record) and an unreferenced tail (§5.4).
pub struct OpenPack {
    /// The data volume the block was placed on (the routed backend id).
    pub(crate) be_id: String,
    /// The pack's SCOPE (design-symmetric-metadata §5.4.3 law 1, KD-SYM-8):
    /// the tenants' forest slot on an armed set — every reference of the
    /// block then lives in ONE slot tree — and `0` (the one scope per data
    /// volume, PK2's) everywhere else. `DataRouter::pack_scope_of` decides.
    pub(crate) scope: u32,
    pub(crate) allocator: Arc<BlockAllocator>,
    pub(crate) device: Arc<NvmeBlockDev>,
    /// Device offset of the block.
    pub(crate) base: u64,
    /// The persisted base key (`be://offset[@inc]`) — every tenant mapping's
    /// prefix, the pack-open ledger's entry and the pin release's key.
    pub(crate) base_key: String,
    /// Next free slot offset within the block; reservation is ONE
    /// `fetch_add`, so the cursor is monotone and every reservation after
    /// the overflowing one overflows too.
    next_slot: AtomicU64,
    /// GAUGE ONLY (`pack_blocks_abandoned`, `pack_open_block_occupancy`):
    /// tenants whose layout commit succeeded. The seal's terminal-vs-
    /// nonterminal outcome is what the REFCOUNT says (a tenant mid-DMA
    /// holds a reference with `committed == 0`), never this word.
    committed: AtomicU32,
    opened_at: Instant,
    /// Set once by the first sealer (a CAS); a sealed pack takes no
    /// reservation and its slot in the table is being vacated.
    sealed: AtomicBool,
    /// This pack's OPEN sequence (process-monotone): the identity of one
    /// pack LIFETIME — a recycled offset re-opened as a fresh pack is a new
    /// one. The pack trace's key (KD-4's "at most one frame per block" is a
    /// law about lifetimes, and un-stamped keys cannot tell two apart).
    seq: u64,
}

static PACK_OPEN_SEQ: AtomicU64 = AtomicU64::new(0);

impl OpenPack {
    /// ONE `fetch_add`: `Some(off)` ⇔ the caller owns `[off, off + slot)`.
    fn try_reserve(&self, slot: u64) -> Option<u64> {
        let prev = self.next_slot.fetch_add(slot, Ordering::AcqRel);
        (prev.saturating_add(slot) <= CHUNK_SIZE).then_some(prev)
    }

    /// CAS `false → true`; `true` ⇔ this caller is THE sealer.
    fn seal(&self) -> bool {
        self.sealed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// A tenant's layout commit succeeded (gauge only).
    pub(crate) fn note_committed(&self) {
        self.committed.fetch_add(1, Ordering::Relaxed);
    }

    /// Reserved bytes (the cursor, clamped to the chunk once overflowed).
    pub fn reserved_bytes(&self) -> u64 {
        self.next_slot.load(Ordering::Relaxed).min(CHUNK_SIZE)
    }

    /// Reserved permille of the chunk — the occupancy gauge's unit.
    pub fn occupancy_permille(&self) -> u64 {
        self.reserved_bytes() * 1000 / CHUNK_SIZE
    }

    pub fn age_ms(&self) -> u64 {
        self.opened_at.elapsed().as_millis() as u64
    }

    pub fn committed(&self) -> u32 {
        self.committed.load(Ordering::Relaxed)
    }

    /// The pack lifetime's open sequence (see the field).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// A reserved slot — the tenant's handle from reserve to commit. It holds
/// the tenant's OWN in-flight registration on the base (dropped with the
/// handle: after the commit is durable and visible, or when the failure
/// path releases) and stands for the tenant's RAM reference, which the
/// layout commit's C8 record justifies on success and
/// `DataRouter::release_pack_tenant` gives back on
/// failure. No `Drop` release on purpose: a committed tenant's reference
/// STAYS (the layout owns it now).
pub struct PackTenant {
    pub(crate) pack: Arc<OpenPack>,
    /// The slot's offset within the block (`LBA_GRAIN`-aligned).
    pub(crate) off: u64,
    _inflight: InflightAllocGuard,
}

/// What a refill answered its cohort: a pack is open (retry the
/// reservation), a `StorageFull` refusal (the arm stops for the batch), or
/// another allocation error.
#[derive(Clone)]
enum RefillOutcome {
    Opened,
    Full,
    Failed(String),
}

/// Why a pack sealed (the `pack_blocks_sealed_*` split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealKind {
    /// The reservation that overflowed the chunk.
    Full,
    /// The dismount teardown, after the promotion pass.
    Dismount,
    /// The drain mover found the pack open on its victim volume (PK3,
    /// §5.11): sealed so the next re-plan moves it as a unit.
    Drain,
    /// A co-writer's batch-scoped pack at its frame reply (§5.3 (c), PK4).
    Batch,
}

/// A pack table key: the data volume the block was placed on and the
/// tenants' scope (§5.4.3 law 1). Unarmed every scope is `0`, so the key
/// is PK2's per-data-volume one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackKey {
    be_id: String,
    scope: u32,
}

/// The packer: the router's per-`(data volume, scope)` open packs, the
/// single-flighted refill, the batch-scoped `StorageFull` stop and the
/// dismount close.
pub struct Packer {
    /// One slot per `(data volume, scope)` the packer has opened a block
    /// on — the volume keyed by the routed backend id
    /// `allocate_placed_block` answered — so at most one open pack per
    /// data volume per scope (the G-PK1 tail bound; PK2's one per volume
    /// when every scope is `0`). On an armed forest set the scope is the
    /// tenant's SLOT: open packs per data volume ≤ leased slots being
    /// written, up to 64 rotor packs + affinity packs on a solo mount
    /// (design §5.4.3's population; the seal residue rides
    /// `pack_blocks_sealed_dismount`).
    open: scc::HashMap<PackKey, Arc<arc_swap::ArcSwapOption<OpenPack>>>,
    /// The single-flighted refill (the R1a `inflight_block_reads` shape):
    /// the leader allocates and installs, the cohort awaits its outcome and
    /// retries the reservation. One flight per router — a refill is one
    /// allocation, and two volumes' refills serialize harmlessly.
    refill: arc_swap::ArcSwapOption<squeezefs_ipc::sqz_flight::Sender<RefillOutcome>>,
    /// OQ-1: a refill hit `StorageFull` — the pack arm is STOPPED for the
    /// rest of the promotion batch; `begin_promotion_batch` re-arms.
    stopped: AtomicBool,
}

impl Default for Packer {
    fn default() -> Self {
        Self::new()
    }
}

impl Packer {
    pub fn new() -> Self {
        Self {
            open: scc::HashMap::new(),
            refill: arc_swap::ArcSwapOption::from(None),
            stopped: AtomicBool::new(false),
        }
    }

    /// A promotion BATCH begins (the merge worker's `promote_batch`, the
    /// dismount pass): re-arm the pack arm after an OQ-1 stop. The fsync
    /// lever is a promotion of one and never re-arms — a stopped arm
    /// answers its promotions resident-and-counted until the next batch.
    pub fn begin_promotion_batch(&self) {
        self.stopped.store(false, Ordering::Relaxed);
    }

    /// `true` while an OQ-1 `StorageFull` stop is in force.
    pub fn arm_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Reserve a `slot`-byte slot in an open pack of `scope` — steps 3 of
    /// §5.2's `prepare`: the reservation (one `fetch_add`), the tenant's
    /// own in-flight registration and its reference. `Ok(None)` = the arm
    /// is stopped (OQ-1): the caller answers the promotion resident-and-
    /// counted. A `StorageFull` refill stops the arm and answers `None`;
    /// any other allocation error propagates. `scope` is the tenant's
    /// pack scope (`DataRouter::pack_scope_of` — `0` unarmed).
    pub async fn reserve(
        &self,
        router: &BackendRouter,
        slot: u64,
        scope: u32,
    ) -> Result<Option<PackTenant>> {
        loop {
            if self.stopped.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if let Some(pack) = self.any_open(scope) {
                match pack.try_reserve(slot) {
                    Some(off) => {
                        let inflight = pack.allocator.inflight_register(pack.base);
                        if pack.allocator.increment_refcount(pack.base) {
                            return Ok(Some(PackTenant {
                                pack,
                                off,
                                _inflight: inflight,
                            }));
                        }
                        // Refused ⇒ the count already hit 0: a concurrent
                        // overflow sealed the pack between this thread's
                        // reservation and its reference, and the sealer's
                        // pin release was the block's LAST reference (no
                        // committed tenant held it). Nothing was DMA'd into
                        // the slot; the block is free-listed by that
                        // release. Vacate WITHOUT a second release and retry
                        // on a fresh pack.
                        log::debug!(
                            "packing: reservation on {} raced its seal (the pin release was \
                             terminal) — re-reserving",
                            pack.base_key
                        );
                        pack.seal();
                        self.vacate(&pack);
                        continue;
                    }
                    None => {
                        // The overflowing reservation seals; the first
                        // sealer runs the seal (§5.3 (a)), everyone else
                        // proceeds to the refill.
                        if pack.seal() {
                            self.run_seal(router, &pack, SealKind::Full).await;
                        }
                        continue;
                    }
                }
            }
            match self.refill(router, scope).await {
                // A leader that died without answering (its sender gone):
                // this waiter re-runs the loop and claims the next flight.
                None | Some(RefillOutcome::Opened) => continue,
                Some(RefillOutcome::Full) => return Ok(None),
                Some(RefillOutcome::Failed(why)) => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "packing: the open pack block's refill failed: {why}"
                    )));
                }
            }
        }
    }

    /// Any UNSEALED open pack of `scope` (≤ one per data volume per scope
    /// — a tiny scan).
    fn any_open(&self, scope: u32) -> Option<Arc<OpenPack>> {
        let mut found = None;
        self.open.iter_sync(|key, slot| {
            if key.scope != scope {
                return true;
            }
            if let Some(p) = slot.load_full() {
                if !p.is_sealed() {
                    found = Some(p);
                    return false;
                }
            }
            true
        });
        found
    }

    fn key_of(pack: &OpenPack) -> PackKey {
        PackKey {
            be_id: pack.be_id.clone(),
            scope: pack.scope,
        }
    }

    /// Vacate `pack`'s table slot if it still holds it (idempotent —
    /// the sealer and a refill leader may both reach for it).
    fn vacate(&self, pack: &Arc<OpenPack>) {
        if let Some(slot) = self.open.read_sync(&Self::key_of(pack), |_, s| s.clone()) {
            let cur = slot.load();
            if cur.as_ref().is_some_and(|p| Arc::ptr_eq(p, pack)) {
                let _ = slot.compare_and_swap(&*cur, None);
            }
        }
    }

    /// The refill: single-flighted — the leader allocates ONE placed block,
    /// enters it in the pack-open ledger (under a transient in-flight
    /// registration, so the block is never uncovered) and installs it;
    /// the cohort awaits the outcome. No lock is held across the
    /// allocation's park (an ENOSPC park is bounded by
    /// `free_grace::pressure_park_wall_ms`).
    async fn refill(&self, router: &BackendRouter, scope: u32) -> Option<RefillOutcome> {
        let (tx, _rx) = squeezefs_ipc::sqz_flight::channel::<RefillOutcome>();
        let flight = Arc::new(tx);
        let prev = self
            .refill
            .compare_and_swap(&None::<Arc<_>>, Some(flight.clone()));
        if let Some(existing) = prev.as_ref() {
            // Waiter: the leader's outcome, or — its sender gone without a
            // send (the leader died) — `None`: re-run the reservation loop.
            let rx = existing.subscribe();
            return rx.wait().await.ok();
        }
        // Leader. A pack another leader installed between this thread's
        // scan and its claim serves the cohort without an allocation.
        let outcome = if self.any_open(scope).is_some() {
            RefillOutcome::Opened
        } else {
            match self.open_block(router, scope).await {
                Ok(pack) => {
                    METRICS
                        .pack_reservation_refills
                        .fetch_add(1, Ordering::Relaxed);
                    self.install(pack).await
                }
                Err(e) if crate::block_allocator::is_storage_full(&e) => {
                    self.stop_for_batch();
                    RefillOutcome::Full
                }
                Err(e) => RefillOutcome::Failed(format!("{e:?}")),
            }
        };
        // Clear the flight BEFORE the send: a waiter arriving after the
        // send finds no flight and runs its own reservation loop.
        self.refill.store(None);
        flight.send(outcome.clone());
        Some(outcome)
    }

    /// Allocate ONE placed block as a fresh pack: the pack-open ledger entry
    /// under a transient in-flight registration (the block is never
    /// uncovered), the cursor at 0, the pin = the allocation's own
    /// reference. Shared by the table refill and the co-writer's private
    /// packs; the caller owns the `StorageFull` disposition.
    async fn open_block(&self, router: &BackendRouter, scope: u32) -> Result<Arc<OpenPack>> {
        let (be_id, allocator, device, offset) = router.allocate_placed_block().await?;
        let base_key = router.persist_block_key(&be_id, offset);
        // The pack-open ledger entry precedes any window in which the
        // block's +1 pin is declared to nobody: the allocation's own
        // registration covers the insert.
        let cover = allocator.inflight_register(offset);
        crate::jobs::pack_ledger_insert(&base_key);
        drop(cover);
        Ok(Arc::new(OpenPack {
            be_id,
            scope,
            allocator,
            device,
            base: offset,
            base_key,
            next_slot: AtomicU64::new(0),
            committed: AtomicU32::new(0),
            opened_at: Instant::now(),
            sealed: AtomicBool::new(false),
            seq: PACK_OPEN_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
        }))
    }

    /// OQ-1: a refill hit `StorageFull` — stop the pack arm for the batch,
    /// ONE WARN per stop, never one per file (the 132 k-line anti-pattern).
    fn stop_for_batch(&self) {
        if !self.stopped.swap(true, Ordering::Relaxed) {
            log::warn!(
                "packing: the open pack block's refill hit StorageFull — the pack arm stops for \
                 the rest of this promotion batch; the remaining staged-layout files stay \
                 ring-resident (readable here, promoted by a later batch or the next clean \
                 unmount); the own-block arm is not retried (it needs the same space)"
            );
        }
        METRICS
            .pack_refill_storage_full
            .fetch_add(1, Ordering::Relaxed);
    }

    /// **A co-writer's PRIVATE pack** (design-small-file-packing §5.6, PK4):
    /// a fresh block that never enters the per-volume table — batch-scoped
    /// to ONE `(owner endpoint, home meta volume)` partition, filled by
    /// [`Self::reserve_in`], sealed by [`Self::seal_private`] at the frame
    /// reply. `Ok(None)` = `StorageFull` (the arm stops for the batch,
    /// OQ-1); any other allocation error propagates.
    pub async fn open_private(&self, router: &BackendRouter) -> Result<Option<Arc<OpenPack>>> {
        if self.stopped.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // A private pack is batch-scoped, never in the table: the scope
        // word is not its key.
        match self.open_block(router, 0).await {
            Ok(pack) => {
                METRICS.pack_blocks_opened.fetch_add(1, Ordering::Relaxed);
                Ok(Some(pack))
            }
            Err(e) if crate::block_allocator::is_storage_full(&e) => {
                self.stop_for_batch();
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Reserve a `slot`-byte slot in a GIVEN (private) pack — the tenant's
    /// registration and reference ride the handle exactly as
    /// [`Self::reserve`]'s. `None` = the pack is full (the caller opens the
    /// next one; the overflowing reservation is not a seal here — the
    /// batch driver seals at the frame reply).
    pub fn reserve_in(pack: &Arc<OpenPack>, slot: u64) -> Option<PackTenant> {
        let off = pack.try_reserve(slot)?;
        let inflight = pack.allocator.inflight_register(pack.base);
        // The pin is this driver's own private reference, so the count can
        // never have reached 0 under it; a refusal is a structural bug.
        if !pack.allocator.increment_refcount(pack.base) {
            crate::note_invariant_tripwire(
                "pack_private_reserve_refused",
                &format!(
                    "a private pack block {} lost its pin before its batch sealed",
                    pack.base_key
                ),
            );
            return None;
        }
        Some(PackTenant {
            pack: Arc::clone(pack),
            off,
            _inflight: inflight,
        })
    }

    /// Seal a private pack at its frame reply (§5.3 (c)) with the frame's
    /// typed outcome class: the pin releases through the allocator's one
    /// release primitive — nonterminal while committed tenants live,
    /// terminal + `Known` = the lane recycle (every tenant refused or
    /// abandoned), terminal + `Unknown` = the leak-safe abandon without
    /// recycle (FIND-PK-4), untracked = the counted no-op (a tenant deleted
    /// between the landing and this seal retired the entry). OUTSIDE every
    /// 3.5 guard (RES-1).
    pub async fn seal_private(
        &self,
        router: &BackendRouter,
        pack: &Arc<OpenPack>,
        outcome: crate::block_allocator::PackPublishOutcome,
    ) {
        if pack.seal() {
            self.run_seal_release(router, pack, SealKind::Batch, outcome)
                .await;
        }
    }

    /// Install a freshly opened pack under its volume's slot. A slot that
    /// already holds an UNSEALED pack keeps it — the fresh block is
    /// abandoned (never published, nothing durable named it) and the
    /// cohort is served the existing one.
    async fn install(&self, pack: Arc<OpenPack>) -> RefillOutcome {
        let slot = match self.open.entry_sync(Self::key_of(&pack)) {
            scc::hash_map::Entry::Occupied(occ) => occ.get().clone(),
            scc::hash_map::Entry::Vacant(vac) => {
                let s = Arc::new(arc_swap::ArcSwapOption::from(None));
                let _ = vac.insert_entry(s.clone());
                s
            }
        };
        loop {
            let cur = slot.load();
            match cur.as_ref() {
                Some(existing) if !existing.is_sealed() => {
                    crate::jobs::pack_ledger_remove(&pack.base_key);
                    let _ = pack.allocator.abandon_unpublished_offset(pack.base).await;
                    return RefillOutcome::Opened;
                }
                _ => {
                    let prev = slot.compare_and_swap(&*cur, Some(pack.clone()));
                    let unchanged = match (prev.as_ref(), cur.as_ref()) {
                        (None, None) => true,
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        _ => false,
                    };
                    if unchanged {
                        METRICS.pack_blocks_opened.fetch_add(1, Ordering::Relaxed);
                        return RefillOutcome::Opened;
                    }
                }
            }
        }
    }

    /// The seal (§5.3): vacate the table slot, release the packer's PIN
    /// through the allocator's one release primitive — OUTSIDE every 3.5
    /// guard (the reservation runs in `prepare`, the dismount seal after
    /// the pass) — then leave the pack-open ledger (the block is never
    /// uncovered while the pin is live: `refcount = tenants` once the
    /// release lands, which is what the census sees). A terminal release
    /// with no committed tenant is an ABANDONED pack (every tenant failed
    /// its commit; ≈ 0).
    async fn run_seal(&self, router: &BackendRouter, pack: &Arc<OpenPack>, kind: SealKind) {
        self.vacate(pack);
        self.run_seal_release(
            router,
            pack,
            kind,
            crate::block_allocator::PackPublishOutcome::Known,
        )
        .await;
    }

    /// The seal's release half — the pin through the allocator's one release
    /// primitive with the pack's publish OUTCOME class (an authority's is
    /// always `Known`; a co-writer's batch pack carries the typed frame
    /// fate), then the pack-open ledger exit and the `pack_blocks_sealed_*`
    /// row.
    async fn run_seal_release(
        &self,
        router: &BackendRouter,
        pack: &Arc<OpenPack>,
        kind: SealKind,
        outcome: crate::block_allocator::PackPublishOutcome,
    ) {
        let verdict = pack
            .allocator
            .release_pack_reference(router, &pack.base_key, outcome)
            .await;
        crate::jobs::pack_ledger_remove(&pack.base_key);
        match kind {
            SealKind::Full => METRICS
                .pack_blocks_sealed_full
                .fetch_add(1, Ordering::Relaxed),
            SealKind::Dismount => METRICS
                .pack_blocks_sealed_dismount
                .fetch_add(1, Ordering::Relaxed),
            SealKind::Drain => METRICS
                .pack_blocks_sealed_drain
                .fetch_add(1, Ordering::Relaxed),
            SealKind::Batch => METRICS
                .pack_blocks_sealed_batch
                .fetch_add(1, Ordering::Relaxed),
        };
        match verdict {
            Ok(crate::block_allocator::PackRelease::Terminal) if pack.committed() == 0 => {
                METRICS
                    .pack_blocks_abandoned
                    .fetch_add(1, Ordering::Relaxed);
                log::debug!(
                    "packing: sealed pack block {} ({kind:?}) with no committed tenant — \
                     abandoned (terminal release)",
                    pack.base_key
                );
            }
            Ok(_) => {}
            Err(e) => log::error!(
                "packing: the pin release of sealed pack block {} ({kind:?}) failed: {e:?} — \
                 the block's RAM refcount stays one above its tenants until remount (space, \
                 never data; fsck C3 reads the +1 once the ledger entry is gone)",
                pack.base_key
            ),
        }
    }

    /// The dismount seal (§5.3 (b)): every open pack seals — after the
    /// promotion pass's last promotion, before "Dismount clean". A
    /// promotion that runs later opens a fresh pack whose pin is RAM and
    /// dies with the process (the block recovers at `refcount = tenants`
    /// — nothing durable records a pin, §5.4), so no close latch is
    /// needed. Returns the number sealed.
    pub async fn seal_all(&self, router: &BackendRouter) -> u64 {
        let mut packs = Vec::new();
        self.open.iter_sync(|_, slot| {
            if let Some(p) = slot.load_full() {
                packs.push(p);
            }
            true
        });
        let mut sealed = 0u64;
        for pack in packs {
            if pack.seal() {
                self.run_seal(router, &pack, SealKind::Dismount).await;
                sealed += 1;
            }
        }
        sealed
    }

    /// The drain seal (§5.11): seal the open pack whose block is `base_key`
    /// (the clean base key the mover census resolved — the pack ledger's
    /// own key), if it is open — the drain mover's arm for a pack block on
    /// its victim (`DataRouter::seal_open_pack_block`). `true` ⇔ this call
    /// was the sealer; no such open pack, or a racing FULL/dismount seal,
    /// answers `false`.
    pub async fn seal_block(&self, router: &BackendRouter, base_key: &str) -> bool {
        let mut found = None;
        self.open.iter_sync(|_, slot| {
            if let Some(p) = slot.load_full() {
                if p.base_key == base_key {
                    found = Some(p);
                    return false;
                }
            }
            true
        });
        let Some(pack) = found else {
            return false;
        };
        if !pack.seal() {
            return false;
        }
        self.run_seal(router, &pack, SealKind::Drain).await;
        true
    }

    /// The open-block gauges: `(open packs, worst age ms, least-filled
    /// occupancy permille)` — the idle-tail instrument (§5.3).
    pub fn gauges(&self) -> (u64, u64, u64) {
        let mut open = 0u64;
        let mut age = 0u64;
        let mut occupancy = 1000u64;
        self.open.iter_sync(|_, slot| {
            if let Some(p) = slot.load_full() {
                if !p.is_sealed() {
                    open += 1;
                    age = age.max(p.age_ms());
                    occupancy = occupancy.min(p.occupancy_permille());
                }
            }
            true
        });
        if open == 0 {
            occupancy = 0;
        }
        (open, age, occupancy)
    }

    /// Unreserved bytes of the UNSEALED open pack (0 = none open) — the
    /// compaction plan's lone-victim rule (design §5.8: a plan executes
    /// only if it frees ≥ 1 block, and a SINGLE victim frees one iff the
    /// open pack already has room for its live windows; two or more
    /// victims below half always net a block).
    pub fn open_room_bytes(&self) -> u64 {
        // The largest room among the open packs: a victim's windows land
        // in the packs of THEIR tenants' scopes (`repack_window` scopes by
        // the tenant), so on an armed set the plan reads the roomiest
        // scope — the one scope's pack, verbatim, everywhere else.
        let mut room = 0u64;
        self.open.iter_sync(|_, slot| {
            if let Some(p) = slot.load_full() {
                if !p.is_sealed() {
                    room = room.max(CHUNK_SIZE.saturating_sub(p.reserved_bytes()));
                }
            }
            true
        });
        room
    }

    /// Open (unsealed) packs per scope — the armed set's per-slot pack
    /// population (`pack_open_scopes`).
    pub fn open_scopes(&self) -> u64 {
        let mut scopes = std::collections::BTreeSet::new();
        self.open.iter_sync(|key, slot| {
            if slot.load_full().is_some_and(|p| !p.is_sealed()) {
                scopes.insert(key.scope);
            }
            true
        });
        scopes.len() as u64
    }

    /// The open packs' base keys (the opt-in census row).
    pub fn open_block_keys(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.open.iter_sync(|_, slot| {
            if let Some(p) = slot.load_full() {
                if !p.is_sealed() {
                    out.push(p.base_key.clone());
                }
            }
            true
        });
        out
    }
}
