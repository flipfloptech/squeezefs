//! Device-overlay **pure state core** (Approach B, PR B1 —
//! `docs/design-device-overlay.md` Rev 2 §2).
//!
//! One [`OverlayRecordCore`] per live `(ino, block)` overlay: the record
//! state machine (`Open → Frozen → Published | Superseded |
//! FenceDropped`), the law-6 **generation** ordering word, the law-3
//! coverage words, the in-flight set, and the §2.3 **range-claim
//! overlap exclusion** — expressed as pure transitions with no I/O, so
//! proptest schedules and the loom model check the exact shipped code.
//!
//! The laws this module owns (the B2+ wiring owns their I/O halves):
//!
//! * **Law 3 (publish-coverage-only-after-full-CQE)** —
//!   [`OverlayRecordCore::complete_store`] publishes coverage only on a
//!   full-success completion; [`OverlayRecordCore::covered_probe`]
//!   additionally screens the in-flight claim bitmap, so *a range in
//!   `inflight` is never served* even when an older pass already
//!   covered it (the re-write-in-flight torn-read screen).
//! * **Law 6 (generations-for-overlap, newest-wins)** — every accepted
//!   segment bumps `generation` (including re-writes of covered
//!   ranges); the §5.2 read protocol revalidates on EQUALITY
//!   ([`OverlayRecordCore::read_begin`] /
//!   [`OverlayRecordCore::read_valid`]). **Mandatory clause (Rev 2
//!   correction 1):** two overlapping stores may never be in flight
//!   concurrently — enforced by the reused
//!   `placed_core::PlacedClaims` page-claim protocol
//!   (grant at submission, release only at the store CQE; the §2.3
//!   stale-DMA counterexample is unrepresentable). Disjoint ranges run
//!   concurrently (the 4×1 MiB cohort stays parallel).
//! * **Law 9 (never-return-unpublished-to-allocator-while-DMA-in-flight)**
//!   — [`OverlayRecordCore::rollback_admissible`] answers `true` only
//!   for a TERMINAL record with an EMPTY in-flight set. The disposition
//!   at teardown belongs to the prod-side mint owner (KD-OV-11):
//!   `Superseded` ⇒ the `MintedBlockGuard` frees; `Published` ⇒ the
//!   guard DISARMS (durable map/ref publication transferred ownership);
//!   `FenceDropped` ⇒ disarm **without** freeing (W5 — nothing freed
//!   post-fence, successor recovery owns the accounting); `Fed` ⇒
//!   disarm **without** freeing (B4a, design-overlay-overwrite §5.4a —
//!   ownership transferred to the rewrite epoch at the feed, KD-B4-3:
//!   the dest is the epoch's pending B key, so freeing it here is the
//!   KD-1.11 corruption).
//! * **The §5.1 old-binding capture (B4a)** — an OVERWRITE record
//!   carries the displaced mapping string captured at install
//!   ([`OverlayRecordCore::old_binding`]), IMMUTABLE for the record's
//!   life (no mutator exists — the §5.6 lock-free compose stays
//!   two-word because the capture needs no revalidation word of its
//!   own). `None` ⇔ the fresh/hole shape (law 5's gaps are zeros);
//!   `Some` ⇔ gaps compose/seed from the old binding (§5.6(1)/§5.8).
//!
//! The coverage LAW is the shared [`crate::coverage_core::CoverageUnion`]
//! (KD-OV-2 — one union for RAM and device accumulation); this module
//! adds only the lock-free **published face** (a page-granular atomic
//! bitmap written under the caller's block lock, readable by the §5.2
//! snapshot/revalidate protocol without any lock).
//!
//! Atomics are `#[cfg(loom)]`-switched so `loom-models/` includes the
//! exact shipped code (`overlay_core` model — the §5.2 word protocol).

#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicU64, AtomicU8, AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(not(loom))]
use std::sync::atomic::{fence, AtomicU64, AtomicU8, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::Mutex;

use crate::coverage_core::CoverageUnion;
use crate::placed_core::PlacedClaims;

/// Store granularity: one claim/coverage bit per 4 KiB page (the v1
/// gate: segments are LBA-aligned, offset and length 4 KiB multiples).
pub const OVERLAY_PAGE: usize = crate::placed_core::CLAIM_PAGE;

/// The record lifecycle (§2.1 `state`). `Published`, `Superseded`,
/// `FenceDropped` and `Fed` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OverlayState {
    /// Accepting segments.
    Open = 0,
    /// The fsync/publication freeze: no new segment may install;
    /// in-flight stores complete (§6.2 steps 1–2).
    Frozen = 1,
    /// The durable map/ref publish transferred ownership of the dest.
    Published = 2,
    /// A shape-change op superseded the record (its own primitive owns
    /// durable state); the dest rolls back once idle.
    Superseded = 3,
    /// The D0 writer guard fenced this mount (W5): publish refused,
    /// nothing freed, successor recovery owns all accounting.
    FenceDropped = 4,
    /// The settle's publish split FED the completed overwrite overlay
    /// to the rewrite epoch (design-overlay-overwrite §5.4a): the dest
    /// key is now the epoch's B key and the EPOCH owns publication, the
    /// displaced park, the deferred free, the fencing law and every
    /// crash window (KD-B4-1). DISTINCT from `Published` — never a
    /// reuse — because (a) the `overlay_publishes` (durable) vs
    /// `overlay_epoch_feeds` (RAM-only) accounting split must be
    /// readable off the record, and (b) a fed record observed after a
    /// fenced epoch is a debugging fact `Published` would erase.
    Fed = 5,
}

impl OverlayState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Open,
            1 => Self::Frozen,
            2 => Self::Published,
            3 => Self::Superseded,
            5 => Self::Fed,
            // Unknown words decode to the free-NOTHING arm (the
            // conservative disposition), as before B4a.
            _ => Self::FenceDropped,
        }
    }

    /// Terminal states (law 9's precondition half).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Published | Self::Superseded | Self::FenceDropped | Self::Fed
        )
    }
}

/// Why a [`OverlayRecordCore::begin_store`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreRefusal {
    /// An overlapping store is in flight (§2.3 — wait for its CQE or
    /// take the accumulation fallback).
    Overlap,
    /// The record is no longer `Open` (frozen or terminal).
    NotOpen,
    /// The range does not lie within the record's block.
    OutOfRange,
}

/// The verdict of one [`OverlayRecordCore::complete_store`].
#[derive(Debug, Clone, Copy)]
pub enum CompleteVerdict {
    /// Full-success CQE on a live record: the range joined `completed`.
    Covered {
        /// The union reached the whole block — the publication trigger
        /// (fires exactly once, the `coverage_core` transition law).
        coverage_complete: bool,
    },
    /// Failed/short CQE: nothing published for this range (law 3); the
    /// claim released, so the range re-admits (the §4.3 fallback owns
    /// the bytes).
    NotCovered,
    /// The record went terminal while the store was in flight: publish
    /// nothing (the CQE-supersession law, one level down). The claim
    /// released and the in-flight set shrank — law 9's teardown wait is
    /// what this verdict unblocks.
    Superseded,
}

/// A granted store: the claim + the law-6 generation stamp, held from
/// submission to CQE. Deliberately not `Clone` — one ticket, one CQE.
#[derive(Debug)]
pub struct StoreTicket {
    first_page: usize,
    pages: usize,
    generation: u64,
}

impl StoreTicket {
    /// The law-6 ordering stamp captured at acceptance.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The claimed page range (page units).
    pub fn page_range(&self) -> (usize, usize) {
        (self.first_page, self.pages)
    }
}

/// The pure per-record state core (§2.1's word fields; the prod
/// registry record wraps this with dest/guards/fence identity).
pub struct OverlayRecordCore {
    /// Block size in bytes.
    len: u32,
    /// Claimable/coverable page count.
    pages: usize,
    /// Record identity for the §5.2 destination-identity check —
    /// `(backend, offset, birth)` never repeats across a re-opened
    /// record (offsets are reused; births are not).
    birth: u64,
    state: AtomicU8,
    /// Law 6's ordering word: bumped by EVERY accepted segment
    /// (including re-writes of covered ranges). §5.2 revalidates on
    /// equality.
    generation: AtomicU64,
    /// Stores submitted, CQE not yet seen (law 9's wait set).
    inflight: AtomicUsize,
    /// §2.3 range claims — the reused placed-sever protocol: granted at
    /// submission, released only at the store CQE.
    claims: PlacedClaims,
    /// The published coverage face: one bit per page, set only by a
    /// full-success CQE (law 3), lock-free readable (§5.2 step 1).
    covered: Box<[AtomicU64]>,
    /// The coverage LAW (shared with RAM accumulation — KD-OV-2):
    /// mutated under the caller's block lock; the atomic face above is
    /// its reader-visible projection.
    union: Mutex<CoverageUnion>,
    /// The §5.1 old-binding capture (B4a): the displaced durable mapping
    /// string of an OVERWRITE record, captured at install and IMMUTABLE
    /// for the record's life (private field, getter only — no mutator
    /// exists by construction). `None` ⇔ the fresh/hole shape. A
    /// read-composition/gap-seed SOURCE, never custody: only the epoch
    /// ever parks or frees the old key (§5.4's hazard-1/2 discharge).
    old_binding: Option<String>,
}

impl OverlayRecordCore {
    /// A fresh/hole `Open` record over a `len`-byte block (no old
    /// binding — law 5's gaps are zeros). `birth` is the registry's
    /// monotone record identity.
    pub fn new(len: u32, birth: u64) -> Self {
        Self::with_binding(len, birth, None)
    }

    /// An OVERWRITE `Open` record (B4a, §5.1): `old_binding` is the
    /// displaced mapping captured in the SAME `INODE_META_LOCKS`
    /// section as the registry install (the one-3.5-section rule —
    /// B4c-ii's caller obligation), immutable from birth.
    pub fn new_overwrite(len: u32, birth: u64, old_binding: String) -> Self {
        Self::with_binding(len, birth, Some(old_binding))
    }

    fn with_binding(len: u32, birth: u64, old_binding: Option<String>) -> Self {
        let pages = (len as usize).div_ceil(OVERLAY_PAGE).max(1);
        let covered = (0..pages.div_ceil(64))
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            len,
            pages,
            birth,
            state: AtomicU8::new(OverlayState::Open as u8),
            generation: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            claims: PlacedClaims::new(len as usize),
            covered,
            union: Mutex::new(CoverageUnion::new()),
            old_binding,
        }
    }

    /// Record identity (the §5.2 dest-identity component).
    pub fn birth(&self) -> u64 {
        self.birth
    }

    /// The §5.1 immutable old-binding capture: `Some` names the gap
    /// composition/seed source of an overwrite record (§5.6(1)/§5.8);
    /// `None` is the fresh/hole shape (gaps are zeros — law 5). This
    /// getter is the ONLY old-binding surface.
    pub fn old_binding(&self) -> Option<&str> {
        self.old_binding.as_deref()
    }

    /// Block size in bytes.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// `len() == 0` is unrepresentable (`new` floors pages at 1), but
    /// clippy's `len`-without-`is_empty` convention wants the pair.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current lifecycle state.
    pub fn state(&self) -> OverlayState {
        OverlayState::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// Accept a segment over pages `[first_page, first_page + pages)`:
    /// grant its exclusive in-flight claim (§2.3), join the in-flight
    /// set, and stamp the law-6 generation. The claim is held until
    /// [`Self::complete_store`] — success or failure — which is what
    /// makes the §2.3 stale-DMA interleaving unrepresentable.
    pub fn begin_store(
        &self,
        first_page: usize,
        pages: usize,
    ) -> Result<StoreTicket, StoreRefusal> {
        if pages == 0 || first_page + pages > self.pages {
            return Err(StoreRefusal::OutOfRange);
        }
        if self.state() != OverlayState::Open {
            return Err(StoreRefusal::NotOpen);
        }
        if !self.claims.begin_claim(first_page, pages) {
            // Range pre-checked ⇒ the refusal is a live overlapping
            // claim (or a defensive seal, which we never set).
            return Err(StoreRefusal::Overlap);
        }
        // The placed-sever writer window guards a CPU memcpy; the
        // overlay's "window" is submit→CQE and is carried by the claim
        // BITS themselves, so the Dekker window closes immediately.
        self.claims.end_write();
        // Join the in-flight set BEFORE the state re-check: a racing
        // freeze that swaps the state after our check must either be
        // observed here (we back out) or observe our in-flight entry
        // (its completion wait covers us). This is a Dekker
        // (store-buffering) pair with [`Self::freeze`] +
        // [`Self::inflight_empty`] — each side publishes its word THEN
        // checks the other's — and the `fence(SeqCst)` between our
        // publish and our check is LOAD-BEARING (the W1 §5.1 /
        // `placed_core` fence protocol shape, paired with the fence in
        // `inflight_empty`): remove it and the SB interleaving admits a
        // store that neither backed out on the freeze nor was visible
        // to the freezer's drain — the loom model
        // `overlay_freeze_drain_observes_final_coverage` fails on
        // weakening.
        self.inflight.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.state() != OverlayState::Open {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            self.claims.release(first_page, pages);
            return Err(StoreRefusal::NotOpen);
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(StoreTicket {
            first_page,
            pages,
            generation,
        })
    }

    /// The store's CQE: publish coverage on full success (law 3),
    /// release the claim (re-admitting the range — §2.3), and leave the
    /// in-flight set. Runs under the caller's block lock in prod (the
    /// coverage-mutation law); the atomic face keeps lock-free readers
    /// sound regardless.
    pub fn complete_store(&self, ticket: StoreTicket, success: bool) -> CompleteVerdict {
        let StoreTicket {
            first_page, pages, ..
        } = ticket;
        let st = self.state();
        let verdict = if st.is_terminal() {
            // CQE-supersession, one level down: a terminal record
            // publishes nothing for a straggler store.
            CompleteVerdict::Superseded
        } else if success {
            let completed = {
                let mut u = self.union.lock().expect("coverage union mutex poisoned");
                let start = (first_page * OVERLAY_PAGE) as u32;
                let end = (((first_page + pages) * OVERLAY_PAGE) as u32).min(self.len);
                u.record(start, end, self.len).completed
            };
            // Publish the reader-visible face AFTER the union (both
            // under the block lock in prod; SeqCst keeps the lock-free
            // §5.2 probe sound against the claim release below).
            self.set_covered_bits(first_page, pages);
            CompleteVerdict::Covered {
                coverage_complete: completed,
            }
        } else {
            CompleteVerdict::NotCovered
        };
        // The claim releases ONLY here — success or failure — which is
        // what re-admits the range (§2.3).
        self.claims.release(first_page, pages);
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        verdict
    }

    fn set_covered_bits(&self, first_page: usize, pages: usize) {
        let mut page = first_page;
        let end = first_page + pages;
        while page < end {
            let word = page / 64;
            let lo = page % 64;
            let hi = (end - word * 64).min(64);
            let bits = hi - lo;
            let mask = if bits == 64 {
                u64::MAX
            } else {
                ((1u64 << bits) - 1) << lo
            };
            self.covered[word].fetch_or(mask, Ordering::SeqCst);
            page = word * 64 + hi;
        }
    }

    /// Law 3's read screen + law 4's serve authority, lock-free: `true`
    /// iff every page of `[first_page, first_page + pages)` is covered
    /// AND no in-flight store overlaps the range (an in-flight range is
    /// never served — even a re-write of an already-covered range).
    pub fn covered_probe(&self, first_page: usize, pages: usize) -> bool {
        if pages == 0 || first_page + pages > self.pages {
            return false;
        }
        // Claims first: with every word SeqCst, a store accepted before
        // the reader's generation snapshot is visible here, and one
        // accepted after it fails the reader's equality revalidation.
        if self.claims.overlaps(first_page, pages) {
            return false;
        }
        let mut page = first_page;
        let end = first_page + pages;
        while page < end {
            let word = page / 64;
            let lo = page % 64;
            let hi = (end - word * 64).min(64);
            let bits = hi - lo;
            let mask = if bits == 64 {
                u64::MAX
            } else {
                ((1u64 << bits) - 1) << lo
            };
            if self.covered[word].load(Ordering::SeqCst) & mask != mask {
                return false;
            }
            page = word * 64 + hi;
        }
        true
    }

    /// True iff a live claim overlaps `[first_page, first_page + pages)`.
    pub fn range_inflight(&self, first_page: usize, pages: usize) -> bool {
        pages > 0 && self.claims.overlaps(first_page, pages)
    }

    /// Coalesced covered page runs inside `[first_page, first_page + pages)`.
    /// Caller waits out [`Self::range_inflight`] first; uncovered pages
    /// are omitted (the serve fills them with zeros — law 5).
    pub fn covered_runs(&self, first_page: usize, pages: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let end = first_page.saturating_add(pages).min(self.pages);
        let mut p = first_page.min(self.pages);
        while p < end {
            if !self.page_covered(p) {
                p += 1;
                continue;
            }
            let start = p;
            p += 1;
            while p < end && self.page_covered(p) {
                p += 1;
            }
            out.push((start, p - start));
        }
        out
    }

    fn page_covered(&self, page: usize) -> bool {
        let word = page / 64;
        let bit = page % 64;
        self.covered[word].load(Ordering::SeqCst) & (1u64 << bit) != 0
    }

    /// §5.2 step 1: the generation snapshot (one word).
    pub fn read_begin(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// §5.2 step 3: generation EQUALITY (not ≥ — `g' > g` means a
    /// same-block segment was accepted mid-fetch: the fetched bytes may
    /// straddle old and new).
    pub fn read_valid(&self, snapshot: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == snapshot
    }

    /// `Open → Frozen` (fsync step 1). `false` = already left `Open`.
    pub fn freeze(&self) -> bool {
        self.state
            .compare_exchange(
                OverlayState::Open as u8,
                OverlayState::Frozen as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// `Open | Frozen → Superseded` (shape-change ops — §7 row 6).
    pub fn supersede(&self) -> bool {
        self.terminalize(OverlayState::Superseded)
    }

    /// `Open | Frozen → FenceDropped` (W5). Nothing is freed on this
    /// arm — the prod mint owner disarms WITHOUT freeing.
    pub fn fence_drop(&self) -> bool {
        self.terminalize(OverlayState::FenceDropped)
    }

    fn terminalize(&self, to: OverlayState) -> bool {
        for from in [OverlayState::Open, OverlayState::Frozen] {
            if self
                .state
                .compare_exchange(from as u8, to as u8, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    /// `Frozen → Published` — only ever from the freeze (the §6.2
    /// sequence), and never from a fenced/superseded/fed record.
    pub fn mark_published(&self) -> bool {
        self.state
            .compare_exchange(
                OverlayState::Frozen as u8,
                OverlayState::Published as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// `Frozen → Fed` (B4a, §5.4a) — only ever from the freeze (the
    /// settle's publish split runs freeze → drain → seed → feed), and
    /// never from a fenced/superseded/published record: the epoch
    /// becomes the ONE pending-binding authority at this instant
    /// (KD-B4-1), so no durable publish may ever follow on the record
    /// itself.
    pub fn mark_fed(&self) -> bool {
        self.state
            .compare_exchange(
                OverlayState::Frozen as u8,
                OverlayState::Fed as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// The in-flight set is empty (fsync step 2's await condition; law
    /// 9's second precondition). The freezer/teardown half of the
    /// begin_store Dekker pair: the caller published its state
    /// transition (freeze/supersede/fence-drop) first, and this fence —
    /// paired with the one in [`Self::begin_store`] — is what forbids
    /// the store-buffering outcome (both sides reading old); see the
    /// begin_store comment. Equally load-bearing.
    pub fn inflight_empty(&self) -> bool {
        fence(Ordering::SeqCst);
        self.inflight.load(Ordering::SeqCst) == 0
    }

    /// The live in-flight store count (the leaked-ticket bark's
    /// fingerprint — diagnostic only).
    pub fn inflight_count(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// The claim words' page ranges currently held (diagnostic only:
    /// which pages the leaked ticket claimed identifies the STORE that
    /// never completed).
    pub fn claim_words(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut run_start: Option<usize> = None;
        for page in 0..self.pages {
            let claimed = self.claims.overlaps(page, 1);
            match (claimed, run_start) {
                (true, None) => run_start = Some(page),
                (false, Some(s)) => {
                    out.push((s, page));
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run_start {
            out.push((s, self.pages));
        }
        out
    }

    /// Law 9 as a pure transition: the destination may leave through
    /// its mint rollback owner only when the record is TERMINAL and no
    /// `WRITE_FIXED` naming it can still land. The per-terminal-state
    /// disposition (free / disarm / leak-to-recovery) is the prod mint
    /// owner's — see the module docs.
    pub fn rollback_admissible(&self) -> bool {
        self.state().is_terminal() && self.inflight_empty()
    }

    /// The union's gaps (fsync step 3's seed input). Takes the union
    /// lock — write-side callers only (never the lock-free read path).
    pub fn gaps(&self) -> Vec<(u32, u32)> {
        let u = self.union.lock().expect("coverage union mutex poisoned");
        u.gaps(self.len)
    }

    /// The union reached the whole block.
    pub fn coverage_complete(&self) -> bool {
        let u = self.union.lock().expect("coverage union mutex poisoned");
        u.is_full(self.len)
    }

    /// Covered bytes (stats/engagement accounting).
    pub fn covered_bytes(&self) -> u64 {
        let u = self.union.lock().expect("coverage union mutex poisoned");
        u.runs_sorted().map(|(s, e)| u64::from(e - s)).sum()
    }
}
