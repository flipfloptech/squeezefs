use crate::error::Result;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// FIND-RW5-A double-release forensics tape (env-gated by
/// `SQUEEZEFS_FREE_FORENSICS`, diagnostic-only): the last recorded free
/// backtrace per offset, shared by `finish_free` (records + pairs a
/// DOUBLE FREE) and `begin_free`'s refusal arm (pairs a REFUSED release
/// with the first free that emptied the refcount).
fn free_forensics_tape() -> &'static std::sync::Mutex<std::collections::HashMap<u64, String>> {
    static TAPE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, String>>> =
        std::sync::OnceLock::new();
    TAPE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Physical allocation stride of every data-volume allocator: block
/// offsets are minted as `block_idx * CHUNK_SIZE`, so a stored block
/// image longer than this tramples the NEXT chunk's bytes on the device
/// (the FIND-RW4-A neighbor-corruption mechanism). Every producer of a
/// stored block image must satisfy `image_len <= CHUNK_SIZE` — see
/// [`ensure_stored_block_image_fits`].
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// FIND-RW4-A defense in depth: refuse — loudly, never by silent
/// truncation or a silent overflow write — any stored block image that
/// would exceed its allocator chunk. Post-fix geometry (the store-raw
/// escape + the format-time block-size headroom clamp + the mount
/// geometry gate) makes this unreachable on in-contract volumes; if it
/// fires, a write path produced an image the volume geometry cannot hold
/// and the write MUST fail rather than corrupt the neighboring chunk.
/// Deliberately an error, not an assert: a mis-geometried volume must
/// degrade to a loud EIO, never abort the daemon.
pub fn ensure_stored_block_image_fits(
    stored_len: usize,
    chunk_size: u64,
    context: &str,
) -> crate::error::Result<()> {
    if stored_len as u64 > chunk_size {
        let msg = format!(
            "stored block image ({stored_len} B) exceeds the {chunk_size} B allocator \
             chunk at {context}: refusing the write — landing it would corrupt the \
             neighboring chunk (FIND-RW4-A guard)"
        );
        log::error!("{msg}");
        return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            msg,
        )));
    }
    Ok(())
}

/// `true` ⇔ `e` is the allocator's StorageFull refusal (the ENOSPC
/// pressure-valve trigger).
/// The ENOSPC pressure valve's async body (see the `space_valve` field):
/// boxed so the wiring closure can be stored latch-free in a `OnceLock`.
pub type SpaceValve =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Reclaimable-supply predicate (see `space_pending`): `true` while the
/// background reclaimer still owes finish_frees (queued or in-flight).
pub type SpacePending = Arc<dyn Fn() -> bool + Send + Sync>;

fn is_storage_full(e: &crate::error::SqueezefsError) -> bool {
    matches!(e, crate::error::SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

/// Outcome of [`BlockAllocator::pin_block_validated`] (§5.1 clone
/// validate-after-pin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// Reference taken and the incarnation observed stable — the pin holds.
    Pinned,
    /// Reference taken but the incarnation is UNSTABLE (a patch may be
    /// mid-flight): the caller must unpin this block too, refetch the
    /// authoritative map, and retry.
    PinnedUnstable,
    /// Reference NOT taken (freed/untracked offset) — the caller must
    /// re-resolve, never proceed unpinned.
    Refused,
}

pub struct BlockAllocator {
    _volume_id: Box<str>,
    chunk_size: u64,
    free_blocks: dashmap::DashSet<u64>,
    highest_block: AtomicU64,
    /// Device capacity in whole chunks (0 = unbounded: offline tools /
    /// tests without a real device). Set at mount registration from the
    /// backing device/file size. `allocate_block` refuses to mint offsets
    /// past it: on a real block device the write would fail EIO/ENOSPC at
    /// DMA time; on a FILE-backed volume it silently GREW the file past
    /// its provisioned size — both discovered by the multi-volume bench
    /// EIO investigation.
    capacity_blocks: AtomicU64,
    refcounts: scc::HashMap<u64, AtomicU32>,
    /// PR VL6a (design-volume-lifecycle §5.6): the fsck **allocation
    /// epoch** — one monotonic atomic, bumped by the fsck coordinator at
    /// scan start and again at re-check. Explicitly a SEPARATE side map
    /// (below), never packed into the loom-verified incarnation seqlock.
    fsck_scan_epoch: AtomicU64,
    /// The scan latch: while set, `allocate_block` records each minted
    /// offset in the epoch side map (one relaxed load on the hot path;
    /// when no scan runs — zero stores, zero cost).
    fsck_scan_active: std::sync::atomic::AtomicBool,
    /// The scan-latched allocation-epoch side map (`offset → epoch at
    /// allocation`); exists only for the duration of a scan
    /// (`fsck_end_scan` drains it). C2/C3 checkers exempt any offset
    /// present here (allocation younger than the scan epoch).
    fsck_epoch_map: scc::HashMap<u64, u64>,
    /// PR VL6a (§5.6): the **in-flight allocation registry** — the
    /// enumerable live owners of allocated-but-unpublished offsets
    /// (writeback/flush units, active-block uploads, R5-parked writes,
    /// mover destinations, wire shard destinations). Fed by
    /// [`Self::inflight_register`] RAII guards at the owner paths; an
    /// owner's guard drops only AFTER its publish is durable and visible
    /// to the tree/refcount reads fsck performs (the natural scope: the
    /// guard is a local of the allocate→DMA→merge function). A crashed /
    /// aborted owner drops its guard on unwind, so a genuinely leaked
    /// offset is never shielded — only LIVE owners are.
    inflight: scc::HashMap<u64, u32>,
    /// The ENOSPC **pressure valve** (async block-reclaim,
    /// `tests/async_block_reclaim_tests.rs` contract 2): terminal frees
    /// queue their device reclaim to `crate::block_reclaim`, and the
    /// freed offsets are not reallocatable until the (background)
    /// reclaim completes — so an allocation that would refuse for space
    /// must first force a drain of the queued reclaims. A full volume
    /// can never be wedged by lazily-queued space. The valve is ASYNC
    /// (contract 2b, probe-up campaign 2026-07-29): the allocating task
    /// awaits the drain, the device reclaim work runs on the blocking
    /// pool — an engagement must never freeze an executor thread (the
    /// old inline drain froze whole fuse3 tpc lanes: the write-funnel
    /// conviction). Wired by `BackendRouter` at
    /// construction/registration; allocators used standalone (offline
    /// tools, unit fixtures) simply have no valve.
    space_valve: std::sync::OnceLock<SpaceValve>,
    /// Reclaimable-supply predicate (paired with the valve at wiring).
    space_pending: std::sync::OnceLock<SpacePending>,
    /// Idea 4 — discard elision (design-rewrite-program §3): unreturned
    /// discard DEBT per elided terminal free (`offset → bytes`). Debt is
    /// RAM-only and strictly ⊆ the free list; allocation CANCELS a
    /// reused offset's entry ([`Self::cancel_elided_debt`] in
    /// `claim_block_idx` — claim-cancels-debt, KD-4.3) and the trim
    /// venues drain it (`crate::block_reclaim::drain_debt_sync`).
    elided_debt: scc::HashMap<u64, u64>,
    /// Local mirror of the outstanding debt bytes (the per-target
    /// watermark input; the process-wide gauge is
    /// `METRICS.block_free_elided_debt_bytes`).
    elided_debt_bytes: AtomicU64,
    /// Per-offset incarnation seqlock: `gen << 1 | stable`.
    ///
    /// Block keys are plain offset strings, so when an offset is freed and
    /// reallocated the *same key string* names a new incarnation. A reader that
    /// resolved the key through a slightly stale block map can device-read the
    /// offset mid-transition and then publish those bytes into the shared block
    /// caches — poisoning the key for its new owner (the block then reads as
    /// zeros or another block's data until remount, while durable data is
    /// correct). The seqlock makes cache fills validated: `allocate_block`
    /// marks the offset in-flight (gen+1, stable=0), the writer calls
    /// [`Self::publish_block`] after its device write (stable=1), and
    /// `free_block` retires it (gen+1, stable=0). A fill may publish into the
    /// caches only if the word was stable before its device read and unchanged
    /// after — anything else returns bytes to the caller uncached.
    incarnations: scc::HashMap<u64, AtomicU64>,
}

impl BlockAllocator {
    pub async fn new(volume_id: &str) -> Result<Self> {
        Ok(Self {
            _volume_id: volume_id.to_string().into_boxed_str(),
            chunk_size: CHUNK_SIZE,
            free_blocks: dashmap::DashSet::new(),
            highest_block: AtomicU64::new(0),
            capacity_blocks: AtomicU64::new(0),
            refcounts: scc::HashMap::new(),
            fsck_scan_epoch: AtomicU64::new(0),
            fsck_scan_active: std::sync::atomic::AtomicBool::new(false),
            fsck_epoch_map: scc::HashMap::new(),
            inflight: scc::HashMap::new(),
            space_valve: std::sync::OnceLock::new(),
            space_pending: std::sync::OnceLock::new(),
            elided_debt: scc::HashMap::new(),
            elided_debt_bytes: AtomicU64::new(0),
            incarnations: scc::HashMap::new(),
        })
    }

    // -----------------------------------------------------------------
    // Idea 4 — discard-elision debt (design-rewrite-program §3; contracts
    // in tests/discard_elision_tests.rs)
    // -----------------------------------------------------------------

    /// Record one elided terminal free's unreturned discard debt.
    /// Called BEFORE `finish_free` (so a racing claim always observes the
    /// entry it must cancel).
    pub fn record_elided_debt(&self, offset: u64, bytes: u64) {
        let prev = match self.elided_debt.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(mut occ) => std::mem::replace(occ.get_mut(), bytes),
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(bytes);
                0
            }
        };
        if prev != 0 {
            // Replaced a lingering entry (re-free without an intervening
            // claim — cannot happen at steady state, reconciled anyway).
            self.elided_debt_bytes.fetch_sub(prev, Ordering::Relaxed);
            crate::fuse_client::METRICS
                .block_free_elided_debt_bytes
                .fetch_sub(prev, Ordering::Relaxed);
        }
        self.elided_debt_bytes.fetch_add(bytes, Ordering::Relaxed);
        crate::fuse_client::METRICS
            .block_free_elided_debt_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Claim-cancels-debt (KD-4.3): a reused offset owes no discard.
    /// One lock-free probe on a mostly-empty map when elision is idle.
    pub(crate) fn cancel_elided_debt(&self, offset: u64) {
        if let Some((_, bytes)) = self.elided_debt.remove_sync(&offset) {
            self.elided_debt_bytes.fetch_sub(bytes, Ordering::Relaxed);
            crate::fuse_client::METRICS
                .block_free_elided_debt_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// Outstanding elided-debt bytes on THIS allocator (the per-target
    /// watermark input).
    pub fn elided_debt_bytes_local(&self) -> u64 {
        self.elided_debt_bytes.load(Ordering::Relaxed)
    }

    /// Take up to `max` debt entries for a trim pass (removed from the
    /// tracker — the pop is the ownership transfer; the trim's
    /// claim-from-free-list decides per offset whether anything issues).
    pub fn take_debt_batch(&self, max: usize) -> Vec<(u64, u64)> {
        let mut keys = Vec::new();
        self.elided_debt.iter_sync(|k, _| {
            keys.push(*k);
            keys.len() < max
        });
        let mut out = Vec::new();
        for k in keys {
            if let Some((offset, bytes)) = self.elided_debt.remove_sync(&k) {
                self.elided_debt_bytes.fetch_sub(bytes, Ordering::Relaxed);
                crate::fuse_client::METRICS
                    .block_free_elided_debt_bytes
                    .fetch_sub(bytes, Ordering::Relaxed);
                out.push((offset, bytes));
            }
        }
        out
    }

    /// The never-minted (virgin) tail in bytes — the KD-4.6 watermark's
    /// derivation input. Unbounded allocators (capacity 0: offline
    /// tools / tests) report an infinite tail: pressure never fires.
    pub fn virgin_bytes(&self) -> u64 {
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        if cap == 0 {
            return u64::MAX;
        }
        let cursor = self.highest_block.load(Ordering::Relaxed).min(cap);
        (cap - cursor).saturating_mul(self.chunk_size)
    }

    /// Trim claim (KD-4.4): take `offset` OUT of the free list — the
    /// allocation claim protocol — so no discard can ever race a new
    /// owner's DMA. The returned in-flight registration shields the
    /// claim window from fsck C6's limbo reconciliation (a mid-trim
    /// offset must never be "completed" back onto the free list). `None`
    /// = lost the claim (reused / racing trim): the offset owes nothing.
    pub fn claim_free_for_trim(self: &Arc<Self>, offset: u64) -> Option<InflightAllocGuard> {
        let idx = offset / self.chunk_size;
        self.free_blocks
            .remove(&idx)
            .map(|_| self.inflight_register(offset))
    }

    /// Return a trim-claimed offset to the free list (the claim window
    /// ends; the caller drops the in-flight guard after this).
    pub fn return_from_trim(&self, offset: u64) {
        self.free_blocks.insert(offset / self.chunk_size);
    }

    /// Wire the ENOSPC pressure valve (see the field doc). Set once by
    /// the owning `BackendRouter`; later calls are no-ops.
    pub fn set_space_pressure_valve(&self, valve: SpaceValve, pending: SpacePending) {
        let _ = self.space_valve.set(valve);
        let _ = self.space_pending.set(pending);
    }

    // -----------------------------------------------------------------
    // PR VL6a — fsck allocation-epoch side map + in-flight registry
    // (design-volume-lifecycle §5.6; the incarnation seqlock above is
    // deliberately untouched)
    // -----------------------------------------------------------------

    /// Arm the scan latch and bump the allocation epoch (fsck scan
    /// start). Returns the scan epoch.
    pub fn fsck_begin_scan(&self) -> u64 {
        let epoch = self.fsck_scan_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.fsck_scan_active.store(true, Ordering::SeqCst);
        epoch
    }

    /// Bump the epoch again at re-check (the two-epoch survival gate).
    pub fn fsck_bump_epoch(&self) -> u64 {
        self.fsck_scan_epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Disarm the latch and drain the side map (scan end).
    pub fn fsck_end_scan(&self) {
        self.fsck_scan_active.store(false, Ordering::SeqCst);
        self.fsck_epoch_map.clear_sync();
    }

    /// The epoch recorded for `offset` in the side map (`Some` ⇔ the
    /// offset was allocated while a scan was active — younger than the
    /// scan epoch, exempt from C2/C3 promotion).
    pub fn allocation_epoch_of(&self, offset: u64) -> Option<u64> {
        self.fsck_epoch_map.read_sync(&offset, |_, v| *v)
    }

    /// Register a live owner of an allocated-but-unpublished offset.
    /// The returned guard MUST be held until the owner's publish is
    /// durable and visible to the tree/refcount reads fsck performs
    /// (deregister-after-publish-visible — §5.6 registry contract), or
    /// until the owner's failure path frees the offset; dropping it on
    /// unwind is exactly right (a dead owner shields nothing).
    pub fn inflight_register(self: &Arc<Self>, offset: u64) -> InflightAllocGuard {
        match self.inflight.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(mut occ) => *occ.get_mut() += 1,
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(1);
            }
        }
        InflightAllocGuard {
            alloc: Arc::clone(self),
            offset,
        }
    }

    /// `true` ⇔ a live owner currently holds `offset` in flight.
    pub fn inflight_contains(&self, offset: u64) -> bool {
        self.inflight.read_sync(&offset, |_, _| ()).is_some()
    }

    /// Snapshot of the in-flight-registered offsets — fsck's C6
    /// aggregate-census exemption source (the same §5.6 live-owner
    /// shield `inflight_contains` serves per offset; since the
    /// write-wall manners law the begin_free→reclaim limbo legitimately
    /// spans whole foreground-busy periods as the deferred reclaim
    /// backlog).
    pub fn inflight_offsets(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.inflight.iter_sync(|k, _| {
            out.push(*k);
            true
        });
        out
    }

    /// Snapshot of the tracked (refcounted) population — fsck's C2/C3
    /// allocator-side ground truth.
    pub fn tracked_offsets(&self) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        self.refcounts.iter_sync(|k, v| {
            out.push((*k, crate::refcount_core::peek(v)));
            true
        });
        out
    }

    /// The free-list population (fsck C6 accounting).
    pub fn free_blocks_count(&self) -> u64 {
        self.free_blocks.len() as u64
    }

    /// PR VL6b (design-volume-lifecycle §5.6a, C3 **recount-and-set** /
    /// the C2-lost allocator-repair tail): set `offset`'s refcount to the
    /// verified counted-reference value. Repair-only seam — callers hold
    /// every referencing ino's DLM lease (ascending order) across the
    /// recount and this store; `count == 0` is refused (an unreferenced
    /// tracked offset is C2-leaked's business, freed through the full
    /// begin→purge→punch→finish law, never zeroed in place).
    pub fn fsck_set_refcount(&self, offset: u64, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                occ.get().store(count, Ordering::SeqCst);
                true
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU32::new(count));
                true
            }
        }
    }

    /// PR VL6b (§5.6a, C6 **recompute-and-republish**): reconcile the
    /// derived used/free accounting with the tracked refcount population
    /// (both are mount-session RAM, rebuilt at mount — pure derived
    /// state). Two drift shapes are healed: a begin_free-limbo offset
    /// (untracked, not free-listed, no live in-flight owner — the wedged
    /// freer whose `finish_free` never came, provably dead after the
    /// finding survived two scan epochs + repair-time re-verification)
    /// gets its free completed; a tracked offset sitting on the free list
    /// (double-owned accounting) is pulled off it. Returns
    /// `(frees_completed, free_list_evictions)`.
    pub fn fsck_reconcile_accounting(&self) -> (u64, u64) {
        let highest = self.highest_block.load(Ordering::Relaxed);
        let mut frees_completed = 0u64;
        let mut free_list_evictions = 0u64;
        for idx in 0..highest {
            let offset = idx * self.chunk_size;
            let tracked = self.refcounts.read_sync(&offset, |_, _| ()).is_some();
            let free_listed = self.free_blocks.contains(&idx);
            if !tracked && !free_listed && !self.inflight_contains(offset) {
                self.free_blocks.insert(idx);
                frees_completed += 1;
            } else if tracked && free_listed {
                self.free_blocks.remove(&idx);
                free_list_evictions += 1;
            }
        }
        (frees_completed, free_list_evictions)
    }

    /// The fresh-block cursor (fsck C6 accounting: `used = highest −
    /// free`, the same arithmetic [`Self::get_used_blocks`] runs).
    pub fn highest_block_index(&self) -> u64 {
        self.highest_block.load(Ordering::Relaxed)
    }

    /// gen+1, stable=0 — offset owned by a writer whose data is not yet on the
    /// device (or retired by a free). Cache fills must not publish. Protocol
    /// core: [`crate::incarnation_core`] (loom-model-checked).
    fn mark_incarnation_unstable(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::retire(occ.get());
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(crate::incarnation_core::UNSTABLE_FIRST));
            }
        }
    }

    /// Writer's durable device write for this incarnation completed — cache
    /// fills that observe an unchanged stable word may publish.
    pub fn publish_block(&self, offset: u64) {
        match self.incarnations.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                crate::incarnation_core::publish(occ.get());
            }
            scc::hash_map::Entry::Vacant(vac) => {
                let _ = vac.insert_entry(AtomicU64::new(crate::incarnation_core::STABLE_FIRST));
            }
        }
    }

    /// Snapshot the incarnation word for a fill. `None` while unstable
    /// (in-flight write or retired/free) — the fill must not publish. Offsets
    /// with no recorded incarnation (written before this process / by another
    /// node) are treated as stable.
    pub fn fill_incarnation(&self, offset: u64) -> Option<u64> {
        match self
            .incarnations
            .read_sync(&offset, |_, v| crate::incarnation_core::snapshot(v))
        {
            Some(snap) => snap,
            None => Some(crate::incarnation_core::UNKNOWN_STABLE),
        }
    }

    /// True if the incarnation word is unchanged since [`Self::fill_incarnation`]
    /// (no allocate/publish/free transitioned the offset during the fill's
    /// device read).
    pub fn fill_incarnation_still(&self, offset: u64, before: u64) -> bool {
        match self
            .incarnations
            .read_sync(&offset, |_, v| crate::incarnation_core::still(v, before))
        {
            Some(unchanged) => unchanged,
            None => before == crate::incarnation_core::UNKNOWN_STABLE,
        }
    }

    /// Take one reference on `offset`'s block. Returns `false` (loudly) when
    /// the reference was NOT taken: the count already hit its terminal zero
    /// (racing free) or the offset is untracked — callers must treat the
    /// block as gone and re-resolve, never proceed unpinned.
    #[must_use]
    pub fn increment_refcount(&self, offset: u64) -> bool {
        match self.refcounts.entry_sync(offset) {
            scc::hash_map::Entry::Occupied(occ) => {
                // Acquire-from-nonzero (loom-modeled, crate::refcount_core):
                // a plain fetch_add could resurrect a count a concurrent
                // free_block just took to zero, leaving this clone holding a
                // freed (reallocatable) offset.
                let taken = crate::refcount_core::try_acquire(occ.get());
                if !taken {
                    log::warn!(
                        "increment_refcount raced a free for offset {offset}: reference not taken"
                    );
                }
                taken
            }
            scc::hash_map::Entry::Vacant(_) => {
                // Unknown offset: it was never allocated by this process or
                // its count already hit zero and was removed. Fabricating a
                // fresh count for a possibly free-listed offset would alias
                // a future allocation — refuse loudly instead.
                log::warn!("increment_refcount on untracked offset {offset}: reference not taken");
                false
            }
        }
    }

    /// Read-only refcount accessor (design-random-small-writes §5.1
    /// predicate 4 / review Issue 10 — RW2 adds it; only
    /// [`Self::increment_refcount`] existed before). `None` = untracked
    /// offset (never allocated by this process / already freed) — callers
    /// must treat it as NOT provably sole-owned.
    pub fn refcount(&self, offset: u64) -> Option<u32> {
        self.refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::peek(v))
    }

    /// W1 patch fence, steps 1a+1b of the §5.1 mechanism (the normative
    /// clone/patch fence — docs/design-random-small-writes.md): retire the
    /// offset's incarnation word (racing validated read-tier fills of this
    /// key now fail their seqlock re-check instead of publishing mid-patch
    /// bytes), interpose the cross-word `SeqCst` fence (the store-buffering
    /// closure against `clone_file`'s pin-CAS → fence → snapshot side),
    /// then re-read the refcount. `true` ⇔ this writer is provably the
    /// sole owner and the in-place DMA may proceed.
    ///
    /// On `false` — the count grew (a clone pinned the block) or the
    /// offset is untracked — the caller MUST [`Self::publish_block`] to
    /// re-stabilize (content never changed) and fall back to the CoW path.
    ///
    /// Unstable-for-existing-offsets audit: `mark_incarnation_unstable`
    /// was built for *ownership transitions* (allocate / free) of an
    /// offset; the patch reuses it for a *content transition* of a LIVE
    /// mapped offset. That reuse is sound because the word's contract is
    /// "content is changing under this key — fills must not publish", not
    /// "the key is dead": `fill_incarnation` returns `None` while
    /// unstable, `fill_incarnation_still` fails across the retire→publish
    /// generation bump, and `publish_block` (after the DMA, or on the
    /// back-off/error paths) restores stability under a NEW generation so
    /// no fill that snapshotted the old generation can validate.
    pub fn begin_patch_sole_owner(&self, offset: u64) -> bool {
        self.mark_incarnation_unstable(offset);
        crate::patch_clone_core::cross_word_fence();
        self.refcount(offset) == Some(1)
    }

    /// §5.1 clone amendment (a): pin + **validate-after-pin**. Takes one
    /// reference exactly like [`Self::increment_refcount`]; on success it
    /// interposes the cross-word `SeqCst` fence and snapshots the offset's
    /// incarnation word. An UNSTABLE word means a patch may be mid-flight
    /// on the pinned block: the caller must unpin (free_block = one
    /// decrement), refetch the authoritative map, and retry — the exact
    /// shape of the existing refused-pin retry loop, extended from "pin
    /// refused" to "pin unvalidated" (bounded by the same `attempt >= 3`
    /// loud refusal).
    ///
    /// The fence sits before the word *lookup* (not inside a
    /// found-the-word arm) so the absent-word case — an offset whose first
    /// in-process patch races this pin after a remount — is ordered too;
    /// absent words are otherwise stable-by-definition (written before
    /// this process, `UNKNOWN_STABLE`).
    pub fn pin_block_validated(&self, offset: u64) -> PinOutcome {
        if !self.increment_refcount(offset) {
            return PinOutcome::Refused;
        }
        crate::patch_clone_core::cross_word_fence();
        let stable = self
            .incarnations
            .read_sync(&offset, |_, v| {
                crate::incarnation_core::snapshot(v).is_some()
            })
            .unwrap_or(true);
        if stable {
            PinOutcome::Pinned
        } else {
            PinOutcome::PinnedUnstable
        }
    }

    pub fn volume_id(&self) -> &str {
        &self._volume_id
    }

    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Bound this allocator to a device of `bytes` capacity (whole chunks).
    /// Zero leaves it unbounded (offline tools / tests).
    pub fn set_capacity_bytes(&self, bytes: u64) {
        self.capacity_blocks
            .store(bytes / self.chunk_size, Ordering::Relaxed);
    }

    /// The bound capacity in bytes (`0` = unbounded) — the
    /// `volume_states` capacity gauge source.
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_blocks
            .load(Ordering::Relaxed)
            .saturating_mul(self.chunk_size)
    }

    /// Advance the fresh-block cursor by one, refusing to mint an offset at
    /// or past the device capacity (when bounded). CAS loop: a refused
    /// racer must not bump the cursor.
    fn next_fresh_block(&self) -> Result<u64> {
        let cap = self.capacity_blocks.load(Ordering::Relaxed);
        loop {
            let cur = self.highest_block.load(Ordering::Relaxed);
            if cap != 0 && cur >= cap {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    format!(
                        "data volume '{}' full: {} of {} blocks allocated",
                        self._volume_id, cur, cap
                    ),
                )));
            }
            if self
                .highest_block
                .compare_exchange(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(cur);
            }
        }
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        let first = self.try_allocate_block();
        let Err(e) = first else { return first };
        if !is_storage_full(&e) || self.space_valve.get().is_none() {
            return Err(e);
        }
        // ENOSPC pressure: freed-but-queued reclaims own free space this
        // allocation is entitled to — drain them OFF-THREAD and retry
        // (contract 2b, probe-up campaign 2026-07-29): the allocating
        // task awaits the drain (honest backpressure on exactly the
        // starved task), the ioctl work runs on the blocking pool, and
        // racing engagements drain CONCURRENTLY (width derives from
        // allocation demand — no single-flight mutex, no park ticks:
        // both alternatives were counted on the 4-wide storm bracket
        // and measured −9 % / −12 % — where reclaim work is cheap,
        // added per-allocation latency is pipeline lifetime in a
        // depth-limited regime). The loop terminates: each pass either
        // allocates, or drains supply someone allocated (system-wide
        // progress, the try_allocate_block retry argument), or observes
        // nothing pending and refuses StorageFull honestly.
        loop {
            if let ok @ Ok(_) = self.try_allocate_block() {
                return ok;
            }
            let pending = self.space_pending.get().map(|p| p()).unwrap_or(false);
            if let Some(valve) = self.space_valve.get() {
                valve().await;
            }
            if !pending {
                // Nothing was owed before the final drain: the verdict
                // stands (genuine fullness refuses StorageFull).
                return self.try_allocate_block();
            }
        }
    }

    /// One allocation attempt (free list, then fresh mint) — the body
    /// [`Self::allocate_block`] wraps with the ENOSPC pressure valve.
    ///
    /// The free-list claim RETRIES until a `remove` wins or the list is
    /// observed empty (field ledger inversion, 2026-07-27 — contract 7 of
    /// `tests/async_block_reclaim_tests.rs`): concurrent allocators all
    /// read the same list head, and the old lost-race arm fell straight
    /// to `next_fresh_block()` — which refuses `StorageFull` on any
    /// cursor-at-cap store (any store that has EVER been full;
    /// `highest_block` never shrinks). Every lost race then fired the
    /// ENOSPC valve: a spurious `sync_drains` count plus a synchronous
    /// whole-queue reclaim drain ON THE WRITE PATH, at ANY fill (the
    /// field's per-allocation valve storm and −33 % rewrite tax). Each
    /// retry means another thread claimed that candidate — system-wide
    /// progress — so the loop is livelock-free and terminates when the
    /// list empties.
    fn try_allocate_block(&self) -> Result<u64> {
        loop {
            let Some(idx) = self.free_blocks.iter().next().map(|item| *item) else {
                break;
            };
            if self.free_blocks.remove(&idx).is_some() {
                log::debug!(
                    "allocate_block: offset {} (freelist)",
                    idx * self.chunk_size
                );
                return Ok(self.claim_block_idx(idx));
            }
            // Lost the claim race: rescan for the next candidate.
        }
        let block_idx = self.next_fresh_block()?;
        log::debug!(
            "allocate_block: offset {} (fresh)",
            block_idx * self.chunk_size
        );
        Ok(self.claim_block_idx(block_idx))
    }

    /// The shared post-claim tail: refcount 1, incarnation unstable (cache
    /// fills must not publish until the owner's `publish_block`), and the
    /// PR VL6a fsck epoch-latch record. Returns the byte offset.
    fn claim_block_idx(&self, block_idx: u64) -> u64 {
        let offset = block_idx * self.chunk_size;
        // Claim-cancels-debt (Idea 4, KD-4.3): the new owner's
        // write-before-publish rewrites the range — it owes no discard.
        self.cancel_elided_debt(offset);
        if self
            .refcounts
            .insert_sync(offset, AtomicU32::new(1))
            .is_err()
        {
            // A lingering refcount entry at claim time means the offset was
            // free-listed while a tracked owner existed — the double-owner
            // mint observed from the OTHER side. Loud: this is never legal.
            log::error!(
                "CLAIM ANOMALY: offset {offset} claimed from the free list while a \
                 refcount entry lingers (count={:?})",
                self.refcount(offset)
            );
        }
        // New incarnation, not yet durable: cache fills must not publish until
        // the owner calls `publish_block` after its device write.
        self.mark_incarnation_unstable(offset);
        // PR VL6a (§5.6): while an fsck scan is latched, record the
        // minting epoch in the side map — one relaxed load when idle.
        if self.fsck_scan_active.load(Ordering::Relaxed) {
            let epoch = self.fsck_scan_epoch.load(Ordering::Relaxed);
            match self.fsck_epoch_map.entry_sync(offset) {
                scc::hash_map::Entry::Occupied(mut occ) => *occ.get_mut() = epoch,
                scc::hash_map::Entry::Vacant(vac) => {
                    let _ = vac.insert_entry(epoch);
                }
            }
        }
        offset
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D1/D2): the **contiguity-aware
    /// pick** — claim the LOWEST free block whose index is `< below_idx`,
    /// with the full `allocate_block` discipline (refcount, incarnation,
    /// fsck epoch latch). `None` = no free block below the bound (the
    /// mover defers; it never mints fresh tail blocks for a compaction —
    /// that would grow the very tail it is reclaiming). Lock-free: the
    /// `DashSet::remove` is the atomic claim; a lost race falls through to
    /// the next candidate.
    pub fn allocate_block_below(&self, below_idx: u64) -> Option<u64> {
        let mut cands: Vec<u64> = self
            .free_blocks
            .iter()
            .map(|i| *i)
            .filter(|i| *i < below_idx)
            .collect();
        cands.sort_unstable();
        for idx in cands {
            if self.free_blocks.remove(&idx).is_some() {
                return Some(self.claim_block_idx(idx));
            }
        }
        None
    }

    /// PR VL7 (§5.7 D2): the **ascending pick** — claim the lowest free
    /// block whose index is `≥ min_idx`; none free ⇒ a fresh tail mint
    /// (strictly above every existing index, so successive calls with a
    /// caller-maintained floor are guaranteed ascending — the locality
    /// rewrite's convergence invariant). Full `allocate_block`
    /// discipline; `StorageFull` propagates from the fresh-mint path.
    pub fn allocate_block_at_or_above(&self, min_idx: u64) -> Result<u64> {
        let mut cands: Vec<u64> = self
            .free_blocks
            .iter()
            .map(|i| *i)
            .filter(|i| *i >= min_idx)
            .collect();
        cands.sort_unstable();
        for idx in cands {
            if self.free_blocks.remove(&idx).is_some() {
                return Ok(self.claim_block_idx(idx));
            }
        }
        let idx = self.next_fresh_block()?;
        Ok(self.claim_block_idx(idx))
    }

    /// Release one reference. On the TERMINAL release (count hit zero) the
    /// offset's incarnation is retired and `true` is returned — but the
    /// offset is **not yet reallocatable**: the freer owns it until
    /// [`Self::finish_free`], which is what makes destructive post-free
    /// device work (the router's hole punch) safe to run in between — it
    /// strictly happens-before any new owner's DMA. Non-terminal releases
    /// return `false` and release nothing else.
    ///
    /// FIND-RW5-A face 6: a free of an UNTRACKED offset is **refused**
    /// (`false`), loudly and counted. At steady state every legitimately
    /// freeable offset carries a refcount entry — allocation seeds it
    /// (`claim_block_idx`, [`Self::allocate_specific_block`]) and
    /// the mount recovery walk seeds every live reference
    /// ([`Self::recover_block`]) — so an untracked free is the second half
    /// of a double-release: the lineage that, interleaved with two
    /// allocations, mints ONE device offset to TWO live owners (the
    /// generic/464 rebind-exhaustion EIO engine). Refusal is the leak-safe
    /// direction: the offset never re-enters the free list on a stale
    /// release (fsck C6 reconciles a genuine limbo). The historical
    /// untracked-frees-anyway arm predates recovery seeding and had no
    /// remaining legitimate caller (fsck's C2Leaked apply refuses untracked
    /// offsets itself before freeing).
    pub fn begin_free(&self, offset: u64) -> bool {
        let should_free = if let Some(terminal) = self
            .refcounts
            .read_sync(&offset, |_, v| crate::refcount_core::release(v))
        {
            if terminal {
                self.refcounts.remove_sync(&offset);
                log::debug!("begin_free terminal: offset {offset}");
                true
            } else {
                log::debug!("begin_free nonterminal: offset {offset}");
                false
            }
        } else {
            log::error!(
                "begin_free REFUSED untracked offset {offset}: no refcount entry — \
                 double-release lineage (see block_untracked_free_refusals)"
            );
            // Forensics (env-gated): pair the refused release's backtrace with
            // the recorded FIRST free of this offset — names both halves of
            // the double-release lineage even though the refusal never
            // reaches finish_free's tape.
            if std::env::var("SQUEEZEFS_FREE_FORENSICS").is_ok() {
                let bt = std::backtrace::Backtrace::force_capture().to_string();
                let first = free_forensics_tape()
                    .lock()
                    .unwrap()
                    .get(&offset)
                    .cloned()
                    .unwrap_or_else(|| "<no recorded first free>".to_string());
                log::error!(
                    "REFUSED FREE FORENSICS offset {offset}:\n--- first free ---\n{first}\n--- refused release ---\n{bt}"
                );
            }
            crate::fuse_client::METRICS
                .block_untracked_free_refusals
                .fetch_add(1, Ordering::Relaxed);
            false
        };

        if should_free {
            // Retire this incarnation before the offset becomes reallocatable so
            // any in-flight cache fill that resolved a stale map to this key
            // fails its seqlock validation instead of poisoning the next owner.
            self.mark_incarnation_unstable(offset);
        }
        should_free
    }

    /// Publish a [`Self::begin_free`]-retired offset for reuse. Only after
    /// this can `allocate_block` hand the offset to a new owner.
    pub fn finish_free(&self, offset: u64) {
        let block_idx = offset / self.chunk_size;
        // FIND-RW5-A forensics (env-gated, diagnostic-only): record every
        // free's capture so a DOUBLE FREE names BOTH call sites.
        if std::env::var("SQUEEZEFS_FREE_FORENSICS").is_ok() {
            let bt = std::backtrace::Backtrace::force_capture().to_string();
            let mut tape = free_forensics_tape().lock().unwrap();
            if let Some(first) = tape.get(&offset) {
                if self.free_blocks.contains(&block_idx) {
                    log::error!(
                        "DOUBLE FREE FORENSICS offset {offset}:\n--- free #1 ---\n{first}\n--- free #2 ---\n{bt}"
                    );
                }
            }
            tape.insert(offset, bt);
        }
        if !self.free_blocks.insert(block_idx) {
            // FIND-RW5-A forensics tripwire: a second release of an offset
            // already on the free list is the double-free family the
            // release_superseded_staged doc warns about — the lineage that
            // mints TWO live owners for one device offset once the next
            // two allocations both receive it.
            log::error!(
                "DOUBLE FREE: offset {offset} (block {block_idx}) was already free —                  double-release lineage; see block_double_frees"
            );
            crate::fuse_client::METRICS
                .block_double_frees
                .fetch_add(1, Ordering::Relaxed);
        }
        log::debug!("finish_free: offset {offset}");
        crate::fuse_client::METRICS
            .del_obj
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Release one reference and, when terminal, immediately publish the
    /// offset for reuse (begin + finish with no destructive work between).
    pub async fn free_block(&self, offset: u64) -> Result<()> {
        if self.begin_free(offset) {
            self.finish_free(offset);
        }
        Ok(())
    }

    pub fn get_used_blocks(&self) -> u64 {
        let highest = self.highest_block.load(Ordering::Relaxed);
        let free = self.free_blocks.len() as u64;
        highest.saturating_sub(free)
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let highest_block = self.highest_block.load(Ordering::Relaxed);
        let free_blocks = self.free_blocks.len() as u64;
        let used_blocks = highest_block.saturating_sub(free_blocks);
        let frag_percent = if highest_block > 0 {
            (free_blocks as f64 / highest_block as f64) * 100.0
        } else {
            0.0
        };

        Ok((highest_block, used_blocks, free_blocks, frag_percent))
    }

    pub async fn allocate_specific_block(&self, block_idx: u64) -> Result<()> {
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
        } else {
            if !self.free_blocks.remove(&block_idx).is_some() {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Block {} is not free or does not exist",
                    block_idx
                )));
            }
        }
        let offset = block_idx * self.chunk_size;
        // Claim-cancels-debt (Idea 4): a specifically-claimed offset is
        // owned again — its lingering elided debt dies with the claim.
        self.cancel_elided_debt(offset);
        let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        Ok(())
    }

    pub async fn get_free_blocks(&self) -> Result<Vec<u64>> {
        Ok(self.free_block_indices())
    }

    /// Sorted snapshot of the free-list indices — the sync twin of
    /// [`Self::get_free_blocks`] (PR VL7: the D1 contiguity census reads
    /// it latch-free from sync contexts).
    pub fn free_block_indices(&self) -> Vec<u64> {
        let mut list: Vec<u64> = self.free_blocks.iter().map(|item| *item).collect();
        list.sort_unstable();
        list
    }

    pub async fn recover_block(&self, block_idx: u64) -> Result<()> {
        let cur_highest = self.highest_block.load(Ordering::Relaxed);
        let offset = block_idx * self.chunk_size;

        if block_idx >= cur_highest {
            for idx in cur_highest..block_idx {
                self.free_blocks.insert(idx);
            }
            self.highest_block.store(block_idx + 1, Ordering::Relaxed);
            let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
        } else {
            if self.free_blocks.remove(&block_idx).is_some() {
                let _ = self.refcounts.insert_sync(offset, AtomicU32::new(1));
            } else {
                match self.refcounts.entry_sync(offset) {
                    scc::hash_map::Entry::Occupied(mut occ) => {
                        occ.get_mut().fetch_add(1, Ordering::SeqCst);
                    }
                    scc::hash_map::Entry::Vacant(vac) => {
                        let _ = vac.insert_entry(AtomicU32::new(1));
                    }
                }
            }
        }
        Ok(())
    }

    /// Block refcount recovery over a metadata volume — walk the live
    /// inode tree (paged range scans) and feed each live ino's `"layout"`
    /// xattr through the shared per-layout recovery body.
    pub async fn recover_active_blocks_v3(
        &self,
        kv: &crate::meta_backend::kv::backend::KvMetaBackend,
        backend_router: &crate::routing::BackendRouter,
    ) -> Result<()> {
        use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
        let inodes = kv.trees()[0];
        let mut checked = 0u64;
        let mut valid_inodes = 0u64;
        let mut layouts_found = 0u64;
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = inodes.range(&cursor, &end, 512).await.map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "v3 recovery inode walk failed: {e}"
                ))
            })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(ino) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                checked += 1;
                if val.nlink == 0 {
                    continue;
                }
                valid_inodes += 1;
                if let Ok(Some(bytes)) = kv.getxattr(ino, "layout").await {
                    layouts_found += 1;
                    let layout_opt: Option<crate::routing::LayoutMetadata> =
                        if bytes.starts_with(b"{") {
                            serde_json::from_slice(&bytes).ok()
                        } else {
                            bincode::deserialize(&bytes).ok()
                        };
                    self.recover_from_layout(backend_router, layout_opt).await;
                }
            }
        }
        // log, not stdout: this walk also runs under `squeezefs df`, whose
        // `--json` output must stay machine-parseable.
        log::info!(
            "Recovery scan summary (v3): checked={}, valid_inodes={}, layouts_found={}",
            checked,
            valid_inodes,
            layouts_found
        );
        Ok(())
    }

    /// The shared per-layout refcount recovery body (extracted verbatim
    /// from the v2 scan; both format walks feed it).
    async fn recover_from_layout(
        &self,
        backend_router: &crate::routing::BackendRouter,
        layout_opt: Option<crate::routing::LayoutMetadata>,
    ) {
        {
            {
                {
                    {
                        if let Some(layout) = layout_opt {
                            // FIND-RW5-A remount face: STAGED files carry
                            // durable block_map entries too — the staged
                            // truncate-clip (`bk:0:len`) and the StorageFull
                            // durable spills. Gating this walk on "striped"
                            // left those live offsets untracked AND
                            // free-listed on a fresh allocator (the gap-fill
                            // claims nothing for them), so `allocate_block`
                            // minted the SAME offset to a second owner while
                            // the staged file's map still bound it — the
                            // generic/464 post-remount never-settles EIO +
                            // refused-untracked-free storm. Seed refcounts
                            // for EVERY layout that carries durable block
                            // references, regardless of file_type.
                            if layout.file_type == "striped" || layout.file_type == "staged" {
                                // 1. Check for indirect block map
                                if let Some(ref map_id) = layout.block_map_id {
                                    if map_id.starts_with("indirect:") {
                                        let indirect_key =
                                            map_id.strip_prefix("indirect:").unwrap();
                                        let mut matches = false;
                                        let mut block_offset = 0;
                                        if let Ok((be_id, offset)) =
                                            backend_router.parse_block_key(indirect_key)
                                        {
                                            if be_id == self._volume_id.as_ref()
                                                || ((be_id == "backend_0" || be_id == "squeezefs")
                                                    && (self._volume_id.as_ref() == "squeezefs"
                                                        || self._volume_id.as_ref()
                                                            == backend_router
                                                                .default_allocator
                                                                .volume_id()))
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        } else if let Ok(offset) = indirect_key.parse::<u64>() {
                                            if self._volume_id.as_ref() == "squeezefs"
                                                || self._volume_id.as_ref()
                                                    == backend_router.default_allocator.volume_id()
                                            {
                                                matches = true;
                                                block_offset = offset;
                                            }
                                        }
                                        if matches {
                                            let indirect_idx = block_offset / self.chunk_size;
                                            let _ = self.recover_block(indirect_idx).await;
                                        }
                                        // Read the indirect block to recover its entries.
                                        // Entries carry backend-true key strings (versioned
                                        // v1 blob): recover ONLY the offsets THIS volume
                                        // owns — the same alias-aware matching the inline
                                        // branch below applies.
                                        let block_size =
                                            backend_router.block_size.load(Ordering::Relaxed)
                                                as usize;
                                        if let Ok(raw_bytes) = backend_router
                                            .read_block(indirect_key, block_size)
                                            .await
                                        {
                                            match crate::routing::decode_indirect_block_map(
                                                &raw_bytes,
                                            ) {
                                                Ok(entries) => {
                                                    for (_b, key) in entries {
                                                        let key =
                                                            crate::routing::clean_block_key(&key);
                                                        let Ok((be_id, offset)) =
                                                            backend_router.parse_block_key(&key)
                                                        else {
                                                            continue;
                                                        };
                                                        let owned = be_id
                                                            == self._volume_id.as_ref()
                                                            || ((be_id == "backend_0"
                                                                || be_id == "squeezefs")
                                                                && (self._volume_id.as_ref()
                                                                    == "squeezefs"
                                                                    || self._volume_id.as_ref()
                                                                        == backend_router
                                                                            .default_allocator
                                                                            .volume_id()));
                                                        if owned {
                                                            let block_idx =
                                                                offset / self.chunk_size;
                                                            let _ =
                                                                self.recover_block(block_idx).await;
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "refcount recovery: undecodable indirect \
                                                         block map at '{}': {}",
                                                        indirect_key,
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                // 2. Check for inline block map. Stored
                                // values are backend-true key strings with an
                                // optional `:extra` trailer after the offset —
                                // strip the trailer with `clean_block_key`
                                // (NEVER a bare `split(':')`, which mangles
                                // prefixed keys: `oss2://123` → `oss2`) and
                                // recover ONLY the offsets THIS volume owns —
                                // the same alias-aware matching as the
                                // indirect branch above.
                                if let Some(ref bm) = layout.block_map {
                                    for offset_str in bm.values() {
                                        let block_key = crate::routing::clean_block_key(offset_str);
                                        let Ok((be_id, offset)) =
                                            backend_router.parse_block_key(&block_key)
                                        else {
                                            continue;
                                        };
                                        let owned = be_id == self._volume_id.as_ref()
                                            || ((be_id == "backend_0" || be_id == "squeezefs")
                                                && (self._volume_id.as_ref() == "squeezefs"
                                                    || self._volume_id.as_ref()
                                                        == backend_router
                                                            .default_allocator
                                                            .volume_id()));
                                        if owned {
                                            let block_idx = offset / self.chunk_size;
                                            let _ = self.recover_block(block_idx).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// RAII registration in the [`BlockAllocator::inflight_register`]
/// in-flight allocation registry (PR VL6a, design-volume-lifecycle
/// §5.6). Held by the live owner of an allocated-but-unpublished offset
/// across its allocate → DMA → meta-publish window; dropping it is the
/// deregistration (contract: only after the publish is durable and
/// visible, or on the owner's failure path where the offset is freed).
pub struct InflightAllocGuard {
    alloc: Arc<BlockAllocator>,
    offset: u64,
}

impl Drop for InflightAllocGuard {
    fn drop(&mut self) {
        let remove = self
            .alloc
            .inflight
            .update_sync(&self.offset, |_, c| {
                *c = c.saturating_sub(1);
                *c == 0
            })
            .unwrap_or(false);
        if remove {
            self.alloc.inflight.remove_sync(&self.offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capacity contract (the multi-volume bench EIO investigation): a
    /// bounded allocator must refuse to mint offsets past its device end —
    /// on real block devices the write would EIO/ENOSPC at DMA time, and on
    /// FILE-backed volumes it silently grew the file past its provisioned
    /// size. Freed blocks make the offset pool reusable again; capacity 0
    /// stays unbounded for offline tools.
    #[tokio::test]
    async fn allocate_block_respects_device_capacity() {
        let a = BlockAllocator::new("cap_test").await.unwrap();
        a.set_capacity_bytes(3 * a.chunk_size());

        let o0 = a.allocate_block().await.expect("block 0");
        let o1 = a.allocate_block().await.expect("block 1");
        let o2 = a.allocate_block().await.expect("block 2");
        assert_eq!((o0, o1, o2), (0, a.chunk_size(), 2 * a.chunk_size()));

        let refused = a.allocate_block().await;
        match refused {
            Err(crate::error::SqueezefsError::Io(ref e))
                if e.kind() == std::io::ErrorKind::StorageFull => {}
            other => panic!("allocation past device capacity must fail StorageFull, got {other:?}"),
        }

        // A freed block re-opens exactly one slot.
        a.free_block(o1).await.unwrap();
        let again = a.allocate_block().await.expect("reuse freed block");
        assert_eq!(again, o1);
        assert!(
            a.allocate_block().await.is_err(),
            "pool exhausted again after the freed slot was reused"
        );
    }
}
