//! Extent allocator I/O layer (design §4.7): A/B durable bitmap pages,
//! journaled alloc/free delta records, the in-RAM lock-free mirror, and the
//! compaction reserve's typed ENOSPC surface.
//!
//! The lock-free protocol core (bitmap claim/release, the pending-free seq
//! gate, reserve accounting) lives in [`super::alloc_ext_core`] — a
//! self-contained module the `loom-models` crate `#[path]`-includes and
//! exhaustively model-checks (`tests/run_loom.sh`). This wrapper adds
//! everything that touches bytes:
//!
//! ## A/B bitmap pages (§4.1, §4.7)
//!
//! 1 bit per heap extent, packed into 4 KiB pages, each stored as an **A/B
//! slot pair**: the writer alternates slots per page (never overwriting the
//! page's newest valid copy), stamps a monotonic generation, and checksums
//! the image; the reader takes the newest valid slot per page — so a torn
//! bitmap write can only damage the copy being replaced, never the one
//! mount needs (torn-bitmap-safe by construction; the §4.10 "torn A ⇒ B"
//! crash case). Selection mirrors the K3 root-ledger pattern
//! ([`super::checkpoint::read_newest_ledger`]): one region-sized
//! `uring_fs::read_at`, per-unit verify, newest-generation-wins, invalid
//! slots read as absent and never fail a mount loud.
//!
//! ### Page image (4 KiB, little-endian)
//!
//! ```text
//! [0..4)    magic: u32       ALLOC_PAGE_MAGIC
//! [4..8)    page_index: u32  which bitmap page (misdirected-write guard)
//! [8..16)   generation: u64  newest-valid-wins selector
//! [16..24)  xxh3_64: u64     over the whole image, this field zeroed
//! [24..4096) bits             LSB-first; bit b of byte i = extent
//!                            page_index·32,576 + i·8 + b
//! ```
//!
//! Physical layout at `base`: page `i`'s slot A at `base + i·8 KiB`, slot B
//! at `base + i·8 KiB + 4 KiB`.
//!
//! ## Journaled deltas (§4.7 "Durability")
//!
//! The bitmap is a **checkpoint accelerator, not the sole truth**: every
//! alloc/free emits a journal record and mount replays records ≥ tail over
//! the loaded pages. Allocator deltas ride the K3 entry framing tagged
//! [`super::record::TREE_ALLOC_RESERVED`] — §4.2
//! reserves that id for the snapshot-era
//! refcounted-extent *node tree*; until then it unambiguously names
//! journal-resident allocator records (allocator state never lives in
//! btree nodes — pages + journal are its whole durable story). Records are
//! `Put`s keyed by the big-endian extent index so per-key LWW-by-seq (the
//! K1 fold shape) gives replay its idempotence:
//!
//! ```text
//! Allocated: value = [1]
//! Freed:     value = [2, retire_seq: u64 LE]
//! ```
//!
//! A `Freed` record's value carries the historical §4.7 pending-free tag
//! — the checkpoint seq the freeing SMO expected to stop referencing the
//! extent (`retire_seq = 0` = never referenced by any checkpoint: a build
//! abandoned before publication, immediately reusable). The value is kept
//! **byte-for-byte** (zero format change; kvparse and old volumes parse
//! identically), but since the Option-A coverage fix
//! (`docs/design-smo-replay-currency.md` §2-A) it is no longer the release
//! gate: generation-durability certified the freeing checkpoint RECORD,
//! not coverage of the freeing swap/flips, and the two decouple (reserve-
//! skipped interiors, dying-floor-clamped root swaps) — the recycled-
//! extent stale-route mechanism behind the child-seq mount-refusal class.
//! Replay rebuilds the pending list from these records (§4.7 "pending-
//! free is journaled"), and **every replayed non-zero-tag free parks**:
//! a replayed free is in-window by construction (its entry sits at-or-
//! past the mounted tail — nothing durable covers it; after a kill the
//! mounted record itself can be page-cache-only), so it stays gated on
//! its own record seq until the first **post-mount** durable checkpoint's
//! tail passes it.
//!
//! ## ENOSPC + the compaction reserve (§4.7)
//!
//! [`ExtentAllocator::claim_user`] fails with the typed
//! [`KvError::NoSpace`] once granting the claim would dip the free budget
//! to-or-below the reserve (`max(8 extents, 2 %)` —
//! [`compaction_reserve_extents`]); [`ExtentAllocator::claim_internal`]
//! (compaction/checkpoint/SMO internals) may consume the reserve, so the
//! tree can always fold appends and free space at user-visible ENOSPC —
//! no write-to-free-space deadlock. K6a/K6b map `NoSpace` onto the crate
//! error path exactly like today's "Inode table full" analog
//! (`alloc.rs` → `ENOSPC`).
//!
//! Like all K1–K5 modules, nothing here is mount-wired yet: the module is
//! kept alive by `tests/kv_alloc_tests.rs`, the PR K4 crash cases, and the
//! loom models (design PR-plan liveness convention).

use super::alloc_ext_core::{AllocClass, ClaimError, ExtCore};
use super::journal::ReplayedEntry;
use super::record::{Record, TREE_ALLOC_RESERVED};
use super::KvError;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Physical bitmap page length (§4.1: 4 KiB pages).
pub const ALLOC_PAGE_LEN: u64 = 4096;
/// Page header length (`magic | page_index | generation | xxh3`).
pub const ALLOC_PAGE_HDR_LEN: usize = 24;
/// Bit-payload bytes per page.
pub const ALLOC_PAGE_DATA_LEN: usize = (ALLOC_PAGE_LEN as usize) - ALLOC_PAGE_HDR_LEN;
/// Extents covered by one page (`4072 × 8`).
pub const ALLOC_PAGE_BITS: u64 = (ALLOC_PAGE_DATA_LEN as u64) * 8;
/// Bitmap page magic (`"KVAB"`).
pub const ALLOC_PAGE_MAGIC: u32 = 0x4B56_4142;

/// `Allocated` delta-record value tag.
pub const ALLOC_REC_ALLOCATED: u8 = 1;
/// `Freed` delta-record value tag (followed by `retire_seq: u64` LE).
pub const ALLOC_REC_FREED: u8 = 2;
/// Extent-key length (big-endian u64 — memcmp-ordered like every §4.2 key).
pub const EXTENT_KEY_LEN: usize = 8;

/// The §4.7 compaction reserve for a heap of `total` extents:
/// `max(8 extents, 2 % of heap)`. Claimable only by
/// [`ExtentAllocator::claim_internal`]; production callers (K6a format /
/// mount) pass this, tests pass explicit values sized to their heaps.
pub fn compaction_reserve_extents(total: u64) -> u64 {
    (total / 50).max(8)
}

/// Number of bitmap pages covering `total_extents`.
pub fn bitmap_pages_for(total_extents: u64) -> u64 {
    total_extents.div_ceil(ALLOC_PAGE_BITS)
}

/// On-disk bitmap region length for `total_extents`: pages × 2 slots ×
/// 4 KiB.
pub fn bitmap_region_len(total_extents: u64) -> u64 {
    bitmap_pages_for(total_extents) * 2 * ALLOC_PAGE_LEN
}

/// Big-endian extent key (memcmp-ordered, §4.2 key discipline).
pub fn extent_key(extent: u64) -> [u8; EXTENT_KEY_LEN] {
    extent.to_be_bytes()
}

/// Decode an extent key; length-checked (§9 bounds rule).
pub fn decode_extent_key(key: &[u8]) -> Result<u64, KvError> {
    let arr: [u8; EXTENT_KEY_LEN] = key.try_into().map_err(|_| {
        KvError::Corrupt(format!(
            "extent key must be {EXTENT_KEY_LEN} bytes, got {}",
            key.len()
        ))
    })?;
    Ok(u64::from_be_bytes(arr))
}

/// One decoded allocator delta (the journal-record payloads above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocDelta {
    /// The extent was claimed.
    Allocated { extent: u64 },
    /// The extent was freed with the §4.7 pending-free tag: the checkpoint
    /// seq that stops referencing it (0 = never referenced — immediately
    /// reusable).
    Freed { extent: u64, retire_seq: u64 },
}

/// Build the journal record for a claim: `(TREE_ALLOC_RESERVED, Put)` in
/// the §4.4 staging shape, ready for `entry_len_for`/`write_entry`. `seq`
/// is the entry seq (the K3 reservation-start identity).
pub fn alloc_record(extent: u64, seq: u64) -> (u8, Record) {
    (
        TREE_ALLOC_RESERVED,
        Record::put(extent_key(extent).to_vec(), seq, vec![ALLOC_REC_ALLOCATED]),
    )
}

/// Build the journal record for a free tagged `retire_seq` (§4.7 —
/// the checkpoint seq that stops referencing the extent; 0 = never
/// referenced).
pub fn free_record(extent: u64, retire_seq: u64, seq: u64) -> (u8, Record) {
    let mut value = Vec::with_capacity(9);
    value.push(ALLOC_REC_FREED);
    value.extend_from_slice(&retire_seq.to_le_bytes());
    (
        TREE_ALLOC_RESERVED,
        Record::put(extent_key(extent).to_vec(), seq, value),
    )
}

/// Decode one allocator delta record (a `TREE_ALLOC_RESERVED`-tagged
/// journal record). Value tags/lengths are bounds-checked (§9); anything
/// malformed is structural corruption — the containing entry's checksum
/// already verified, so this is defense in depth against writer bugs.
pub fn decode_alloc_record(rec: &Record) -> Result<AllocDelta, KvError> {
    let extent = decode_extent_key(&rec.key)?;
    match rec.value.as_slice() {
        [ALLOC_REC_ALLOCATED] => Ok(AllocDelta::Allocated { extent }),
        [ALLOC_REC_FREED, rest @ ..] if rest.len() == 8 => Ok(AllocDelta::Freed {
            extent,
            retire_seq: u64::from_le_bytes(rest.try_into().unwrap()),
        }),
        v => Err(KvError::Corrupt(format!(
            "malformed allocator delta value ({} bytes, tag {:?}) for extent {extent}",
            v.len(),
            v.first()
        ))),
    }
}

/// xxh3 over a page image with the checksum field (bytes 16..24) zeroed —
/// the same header-then-payload discipline as the ledger and bsets (§4.3).
fn page_checksum(image: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&image[..16]);
    h.update(&[0u8; 8]);
    h.update(&image[ALLOC_PAGE_HDR_LEN..]);
    h.digest()
}

/// Encode one full 4 KiB page image: `bits` (≤ 4072 bytes, zero-padded)
/// stamped with `page_index` and `generation`.
pub fn encode_bitmap_page(
    page_index: u32,
    generation: u64,
    bits: &[u8],
) -> Result<Vec<u8>, KvError> {
    if bits.len() > ALLOC_PAGE_DATA_LEN {
        return Err(KvError::Corrupt(format!(
            "bitmap page payload {} exceeds the {ALLOC_PAGE_DATA_LEN}-byte page data area",
            bits.len()
        )));
    }
    let mut image = vec![0u8; ALLOC_PAGE_LEN as usize];
    image[0..4].copy_from_slice(&ALLOC_PAGE_MAGIC.to_le_bytes());
    image[4..8].copy_from_slice(&page_index.to_le_bytes());
    image[8..16].copy_from_slice(&generation.to_le_bytes());
    image[ALLOC_PAGE_HDR_LEN..ALLOC_PAGE_HDR_LEN + bits.len()].copy_from_slice(bits);
    let sum = page_checksum(&image);
    image[16..24].copy_from_slice(&sum.to_le_bytes());
    Ok(image)
}

/// Decode + verify one page image against its expected `page_index`:
/// magic, index (misdirected-write guard), checksum. Any failure means
/// "this slot holds no valid page" — A/B selection treats it as absent,
/// never loud (the K3 ledger discipline).
pub fn decode_bitmap_page(buf: &[u8], page_index: u32) -> Result<(u64, &[u8]), KvError> {
    if buf.len() < ALLOC_PAGE_LEN as usize {
        return Err(KvError::Corrupt(format!(
            "truncated bitmap page: {} of {ALLOC_PAGE_LEN} bytes",
            buf.len()
        )));
    }
    let buf = &buf[..ALLOC_PAGE_LEN as usize];
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != ALLOC_PAGE_MAGIC {
        return Err(KvError::Corrupt(format!(
            "bad bitmap page magic {magic:#010x} (expected {ALLOC_PAGE_MAGIC:#010x})"
        )));
    }
    let stored_index = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if stored_index != page_index {
        return Err(KvError::Corrupt(format!(
            "bitmap page carries index {stored_index}, expected {page_index} (misdirected write)"
        )));
    }
    let generation = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let computed = page_checksum(buf);
    if stored != computed {
        return Err(KvError::ChecksumMismatch { stored, computed });
    }
    Ok((generation, &buf[ALLOC_PAGE_HDR_LEN..]))
}

/// Slot marker for a page never yet written (fresh volume / short region).
const SLOT_NONE: u64 = 2;

/// Per-page write state: the newest valid generation on disk and which
/// physical slot holds it. Written by the serialized checkpoint task
/// (§4.6); atomics for `Sync`, not for contention.
struct PageState {
    generation: AtomicU64,
    /// 0 = slot A, 1 = slot B, [`SLOT_NONE`] = fresh (next write → A).
    slot: AtomicU64,
}

/// The per-volume extent allocator: the lock-free in-RAM mirror
/// ([`ExtCore`]) plus A/B page persistence and dirty-page tracking.
///
/// Concurrency contract: `claim_*` / `free_pending` / `advance_durable` /
/// `release_unpublished` are lock-free and callable from any task;
/// [`Self::write_dirty_pages`] belongs to the serialized per-volume
/// checkpoint/writeback task (§4.6) — it is the only page-state writer.
pub struct ExtentAllocator {
    core: ExtCore,
    pages: u64,
    page_states: Box<[PageState]>,
    /// Dirty-page bits (one per page): pages whose in-RAM bits diverged
    /// from the newest on-disk copy since the last [`Self::write_dirty_pages`].
    dirty: Box<[AtomicU64]>,
}

impl std::fmt::Debug for ExtentAllocator {
    /// Summarizes instead of deriving: dumping the whole mirror bitmap
    /// into panic messages helps no one (the node-layer precedent).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtentAllocator")
            .field("total_extents", &self.core.total())
            .field("free_extents", &self.core.free_extents())
            .field("reserve_extents", &self.core.reserve())
            .field("pending", &self.core.pending_count())
            .field("durable_seq", &self.core.durable_seq())
            .field("pages", &self.pages)
            .finish()
    }
}

impl ExtentAllocator {
    /// A fresh, all-free allocator (format time): `total_extents` with
    /// `reserve` held for internal claims and a pending-free FIFO of
    /// `pending_cap` entries. Every page starts dirty (a fresh volume's
    /// first checkpoint persists the whole bitmap).
    pub fn format(total_extents: u64, reserve: u64, pending_cap: usize) -> Self {
        let pages = bitmap_pages_for(total_extents);
        let page_states: Vec<PageState> = (0..pages)
            .map(|_| PageState {
                generation: AtomicU64::new(0),
                slot: AtomicU64::new(SLOT_NONE),
            })
            .collect();
        let dirty: Vec<AtomicU64> = (0..pages.div_ceil(64))
            .map(|_| AtomicU64::new(u64::MAX))
            .collect();
        Self {
            core: ExtCore::new(total_extents, reserve, pending_cap),
            pages,
            page_states: page_states.into_boxed_slice(),
            dirty: dirty.into_boxed_slice(),
        }
    }

    /// Mount: load the newest valid A/B slot per page (one region-sized
    /// `uring_fs::read_at`; invalid/missing slots read as absent — never
    /// loud, the K3 ledger discipline), seed the mirror, then replay the
    /// journal's allocator deltas over it (§4.7 "Mount": pages, then
    /// records ≥ tail).
    ///
    /// `mounted_tail` is the selected ledger record's `journal_tail_seq`
    /// — the coverage-gate watermark seed (design-smo-replay-currency
    /// §2-A). Every replayed non-zero-tag `Freed` record **parks** gated
    /// on its own record seq: replayed frees are in-window by
    /// construction (`rec.seq ≥ mounted_tail`), so nothing durable covers
    /// their freeing swap/flips — after a kill the mounted record itself
    /// may exist only in page cache — and they release only once the
    /// first post-mount durable checkpoint's tail passes them. A zero
    /// historical tag (never referenced by any checkpoint) releases
    /// immediately, as always. `replay` is the recovered window
    /// (`JournalRecovery::entries`), seq-sorted; the caller's tail rule
    /// (K6b) keeps alloc/free records in the window until their effects
    /// are durable in bitmap pages.
    ///
    /// `Err` is real device I/O failure only ([`KvError::Io`]) — bitmap
    /// *contents* never fail a mount loud.
    pub async fn load(
        path: &Path,
        base: u64,
        total_extents: u64,
        reserve: u64,
        pending_cap: usize,
        mounted_tail: u64,
        replay: &[ReplayedEntry],
    ) -> Result<Self, KvError> {
        let pages = bitmap_pages_for(total_extents);
        let region_len = bitmap_region_len(total_extents) as usize;
        let got = crate::uring_fs::read_at(path, base, region_len).await?;
        // Short region (file smaller than the extent): zero-extend — zeros
        // verify nothing and read as fresh pages.
        let image: std::borrow::Cow<'_, [u8]> = if got.len() == region_len {
            std::borrow::Cow::Borrowed(&got)
        } else {
            let mut full = vec![0u8; region_len];
            full[..got.len()].copy_from_slice(&got);
            std::borrow::Cow::Owned(full)
        };

        let mut core = ExtCore::new(total_extents, reserve, pending_cap);
        let mut page_states = Vec::with_capacity(pages as usize);
        for page in 0..pages {
            let page_base = (page * 2 * ALLOC_PAGE_LEN) as usize;
            let mut newest: Option<(u64, u64, &[u8])> = None; // (gen, slot, bits)
            for slot in 0..2u64 {
                let start = page_base + (slot * ALLOC_PAGE_LEN) as usize;
                if let Ok((generation, bits)) =
                    decode_bitmap_page(&image[start..start + ALLOC_PAGE_LEN as usize], page as u32)
                {
                    if newest.is_none_or(|(g, _, _)| generation > g) {
                        newest = Some((generation, slot, bits));
                    }
                }
            }
            match newest {
                Some((generation, slot, bits)) => {
                    let first = page * ALLOC_PAGE_BITS;
                    let last = (first + ALLOC_PAGE_BITS).min(total_extents);
                    for extent in first..last {
                        let off = (extent - first) as usize;
                        if bits[off / 8] & (1 << (off % 8)) != 0 {
                            core.mark_allocated(extent);
                        }
                    }
                    page_states.push(PageState {
                        generation: AtomicU64::new(generation),
                        slot: AtomicU64::new(slot),
                    });
                }
                None => page_states.push(PageState {
                    generation: AtomicU64::new(0),
                    slot: AtomicU64::new(SLOT_NONE),
                }),
            }
        }

        // The coverage-gate watermark seeds at the mounted tail: nothing
        // at-or-past it is durably covered, so no replayed free can drain
        // before a POST-mount checkpoint's tail passes it (design-smo-
        // replay-currency §2-A mount gate).
        core.advance_durable(mounted_tail);

        // Fold the window's allocator deltas per-key LWW FIRST (the K1
        // fold shape: newest record per extent wins — the records are
        // `Put`s keyed by extent index precisely so replay is
        // idempotent), then apply only the survivors. Folding first is
        // load-bearing for the §2-A mount gate: a free superseded by a
        // later in-window re-alloc of the same extent (the reuse chain a
        // live gate-pass produced pre-crash) must fold AWAY — parking it
        // would leave a FIFO entry whose post-mount drain clears a LIVE
        // extent's bit. Deltas dirty their pages so the next checkpoint
        // persists what only the journal held.
        let mut folded: std::collections::BTreeMap<u64, (u64, AllocDelta)> =
            std::collections::BTreeMap::new();
        for entry in replay {
            for (tree_id, rec) in &entry.records {
                if *tree_id != TREE_ALLOC_RESERVED {
                    continue;
                }
                let delta = decode_alloc_record(rec)?;
                let extent = decode_extent_key(&rec.key)?;
                // Entries arrive seq-sorted; per-key newest-wins is a
                // plain overwrite (record seqs are strictly monotonic).
                folded.insert(extent, (rec.seq, delta));
            }
        }
        // Apply survivors; parked finals push in seq order (the FIFO's
        // non-decreasing-gate contract), not extent order.
        let mut replay_dirty: Vec<u64> = Vec::with_capacity(folded.len());
        let mut parked: Vec<(u64, u64)> = Vec::new(); // (rec seq, extent)
        for (extent, (seq, delta)) in &folded {
            match delta {
                AllocDelta::Allocated { .. } => core.mark_allocated(*extent),
                AllocDelta::Freed { retire_seq: 0, .. } => {
                    // Never referenced by any checkpoint (a build
                    // abandoned before publication): immediately
                    // reusable, as always.
                    core.release(*extent);
                }
                AllocDelta::Freed { .. } => parked.push((*seq, *extent)),
            }
            replay_dirty.push(*extent);
        }
        parked.sort_unstable();
        for (seq, extent) in parked {
            // In-window by construction ⇒ the freeing swap/flips are not
            // durably covered (the mounted record itself may be page-
            // cache-only after a kill): park gated on the record's own
            // seq until the first post-mount durable checkpoint's tail
            // passes it. The historical generation tag in the value
            // stays byte-for-byte on disk but no longer gates (§2-A: it
            // certified record durability, not flip coverage). The bit
            // may live only in this same window (its claiming alloc
            // folded away under this key's LWW; the pages may predate
            // both) — set it before parking, idempotently, so the FIFO's
            // claimed-while-pending contract holds. A full FIFO here is a
            // broken K6b tail discipline (the load contract above),
            // failed loud.
            core.mark_allocated(extent);
            core.free_pending(extent, seq).map_err(|_| {
                KvError::Corrupt(format!(
                    "replayed pending-free overflow at extent {extent} (record seq \
                     {seq}): journal window exceeds the pending-free cap"
                ))
            })?;
            super::META_KV_PENDING_FREE_PARKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let dirty: Vec<AtomicU64> = (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let alloc = Self {
            core,
            pages,
            page_states: page_states.into_boxed_slice(),
            dirty: dirty.into_boxed_slice(),
        };
        for extent in replay_dirty {
            alloc.mark_dirty(extent);
        }
        Ok(alloc)
    }

    /// Total heap extents.
    pub fn total_extents(&self) -> u64 {
        self.core.total()
    }

    /// The compaction reserve in extents.
    pub fn reserve_extents(&self) -> u64 {
        self.core.reserve()
    }

    /// Claimable extents right now.
    pub fn free_extents(&self) -> u64 {
        self.core.free_extents()
    }

    /// Pending-free entries awaiting their durable checkpoint.
    pub fn pending_count(&self) -> u64 {
        self.core.pending_count()
    }

    /// Newest checkpoint seq known durable.
    pub fn durable_seq(&self) -> u64 {
        self.core.durable_seq()
    }

    /// Whether `extent` is allocated or pending-free.
    pub fn is_allocated(&self, extent: u64) -> bool {
        self.core.is_allocated(extent)
    }

    /// Newest bitmap generation on disk across pages — mount resumes
    /// generation numbering above `max(this, ledger.alloc_bitmap_generation)`
    /// so a new write can never tie an existing valid slot.
    pub fn resume_generation(&self) -> u64 {
        self.page_states
            .iter()
            .map(|p| p.generation.load(Ordering::Acquire))
            .max()
            .unwrap_or(0)
    }

    /// Claim an extent for a **user op** (§4.7 ENOSPC semantics): fails
    /// with the typed [`KvError::NoSpace`] once the claim would dip the
    /// free budget to-or-below the compaction reserve — the reserve stays
    /// intact for [`Self::claim_internal`].
    pub fn claim_user(&self) -> Result<u64, KvError> {
        self.claim(AllocClass::User)
    }

    /// Claim an extent for compaction/checkpoint/SMO internals: may
    /// consume the reserve (§4.7 — the tree can always fold appends and
    /// free space even at user-visible ENOSPC). [`KvError::NoSpace`] here
    /// means the heap is genuinely exhausted.
    pub fn claim_internal(&self) -> Result<u64, KvError> {
        self.claim(AllocClass::Internal)
    }

    fn claim(&self, class: AllocClass) -> Result<u64, KvError> {
        match self.core.claim(class) {
            Ok(extent) => {
                self.mark_dirty(extent);
                Ok(extent)
            }
            Err(ClaimError::NoSpace) => Err(KvError::NoSpace {
                free: self.core.free_extents(),
                reserve: self.core.reserve(),
            }),
        }
    }

    /// Enter `extent` into the pending-free list gated on `gate_seq` —
    /// the freeing SMO entry's **free-record journal seq** (§4.7 +
    /// design-smo-replay-currency §2-A: the free record is the entry's
    /// highest seq, so a durable tail past it proves every flip of that
    /// entry is materialized). Not claimable until
    /// [`Self::advance_durable`] covers the tag. Errors with the typed
    /// [`KvError::PendingFreeFull`] at the §4.7 cap — the caller forces a
    /// checkpoint rather than reusing unsafely (the at-cap protocol's
    /// admission side is [`Self::pending_has_room`]).
    pub fn free_pending(&self, extent: u64, gate_seq: u64) -> Result<(), KvError> {
        self.core
            .free_pending(extent, gate_seq)
            .map_err(|_| KvError::PendingFreeFull {
                pending: self.core.pending_count(),
            })?;
        super::META_KV_PENDING_FREE_PARKED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Producer-side FIFO headroom (§4.7 at-cap protocol, design-smo-
    /// replay-currency PR 4 clause a): the serialized SMO task checks
    /// this at admission — BEFORE the swap — so `PendingFreeFull` can
    /// only ever surface pre-swap (clean abort, claims released) and the
    /// post-swap push is guaranteed to fit (this task is the FIFO's only
    /// producer; drains only vacate).
    pub fn pending_has_room(&self) -> bool {
        self.core.pending_has_room()
    }

    /// The §4.7 coverage gate: a root-ledger record whose
    /// `journal_tail_seq` is `tail` is now *known durable* (post-barrier)
    /// — release every pending extent whose gate seq that tail covers
    /// back to the claimable pool. Tails are journal-entry boundaries and
    /// gate seqs sit strictly inside their entries (the free record is
    /// never record 0 of an SMO entry), so `tail ≥ gate` here is
    /// equivalent to the design's strict `tail > free.seq`. Returns how
    /// many extents were released (their pages are marked dirty for the
    /// next checkpoint).
    pub fn advance_durable(&self, tail: u64) -> u64 {
        let released = self.core.advance_durable(tail);
        for extent in &released {
            self.mark_dirty(*extent);
        }
        let n = released.len() as u64;
        if n > 0 {
            super::META_KV_PENDING_FREE_RELEASED.fetch_add(n, Ordering::Relaxed);
        }
        n
    }

    /// Release an extent **no checkpoint ever referenced** (a node build
    /// abandoned before publication) straight back to the claimable pool.
    /// Checkpoint-referenced extents must go through
    /// [`Self::free_pending`] (§4.7).
    pub fn release_unpublished(&self, extent: u64) {
        self.core.release(extent);
        self.mark_dirty(extent);
    }

    fn mark_dirty(&self, extent: u64) {
        let page = extent / ALLOC_PAGE_BITS;
        self.dirty[(page / 64) as usize].fetch_or(1u64 << (page % 64), Ordering::AcqRel);
    }

    /// Persist every dirty page at `generation` (strictly greater than any
    /// generation already on disk — the caller derives it from
    /// [`Self::resume_generation`] / the checkpoint seq): each page's
    /// image goes to the slot **not** holding its newest valid copy
    /// (§4.1 "writer alternates slots" — a torn write can only damage the
    /// copy being replaced), all pages in one `uring_fs` batch. Returns
    /// the page indices written, ascending. Serialized-checkpoint-task
    /// only (§4.6); durability rides the caller's barrier, and the caller
    /// records `generation` in its ledger record
    /// (`alloc_bitmap_generation`).
    pub async fn write_dirty_pages(
        &self,
        path: &Path,
        base: u64,
        generation: u64,
    ) -> Result<Vec<u32>, KvError> {
        // Snapshot-and-clear the dirty set first: claims racing this write
        // re-dirty their page and are picked up by the next checkpoint —
        // never lost. (A racing claim may also ride along in this
        // snapshot: bit-set-early is safe, its journal record is ≥ any
        // tail this checkpoint can name.)
        let mut to_write: Vec<u32> = Vec::new();
        for (w, word) in self.dirty.iter().enumerate() {
            let bits = word.swap(0, Ordering::AcqRel);
            let mut rest = bits;
            while rest != 0 {
                let b = rest.trailing_zeros();
                let page = (w as u64) * 64 + u64::from(b);
                if page < self.pages {
                    to_write.push(page as u32);
                }
                rest &= rest - 1;
            }
        }
        if to_write.is_empty() {
            return Ok(to_write);
        }

        let words = self.core.snapshot_words();
        let mut ops: Vec<(u64, bytes::Bytes)> = Vec::with_capacity(to_write.len());
        let mut new_slots: Vec<(u32, u64)> = Vec::with_capacity(to_write.len());
        for &page in &to_write {
            let state = &self.page_states[page as usize];
            let cur_gen = state.generation.load(Ordering::Acquire);
            debug_assert!(
                generation > cur_gen,
                "bitmap generation {generation} must exceed page {page}'s newest {cur_gen} \
                 (newest-valid-wins would tie)"
            );
            let target = match state.slot.load(Ordering::Acquire) {
                SLOT_NONE => 0,
                s => 1 - s,
            };
            let bits = self.page_bits(&words, u64::from(page));
            let image = encode_bitmap_page(page, generation, &bits)?;
            ops.push((
                base + u64::from(page) * 2 * ALLOC_PAGE_LEN + target * ALLOC_PAGE_LEN,
                bytes::Bytes::from(image),
            ));
            new_slots.push((page, target));
        }

        if ops.len() == 1 {
            let (off, data) = ops.pop().expect("one op");
            crate::uring_fs::write_at(path, off, data).await?;
        } else {
            crate::uring_fs::write_at_batch(path, ops).await?;
        }

        // The writes landed: record the new newest-valid slot per page.
        for (page, slot) in new_slots {
            let state = &self.page_states[page as usize];
            state.generation.store(generation, Ordering::Release);
            state.slot.store(slot, Ordering::Release);
        }
        Ok(to_write)
    }

    /// Serialize page `page`'s bit payload out of a mirror snapshot.
    fn page_bits(&self, words: &[u64], page: u64) -> Vec<u8> {
        const WORDS_PER_PAGE: usize = ALLOC_PAGE_DATA_LEN / 8; // 509
        let start = (page as usize) * WORDS_PER_PAGE;
        let end = (start + WORDS_PER_PAGE).min(words.len());
        let mut bits = Vec::with_capacity(ALLOC_PAGE_DATA_LEN);
        for w in &words[start..end] {
            bits.extend_from_slice(&w.to_le_bytes());
        }
        bits
    }
}
