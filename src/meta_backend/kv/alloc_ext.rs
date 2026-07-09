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
//! [`TREE_ALLOC_RESERVED`](super::record::TREE_ALLOC_RESERVED) — §4.2
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
//! A `Freed` record carries the §4.7 pending-free tag — the checkpoint seq
//! that stops referencing the extent (`retire_seq = 0` = never referenced
//! by any checkpoint: a build abandoned before publication, immediately
//! reusable). Replay rebuilds the pending list from these (§4.7 "pending-
//! free is journaled"): `retire_seq ≤` the mounted ledger seq ⇒ the gate
//! already passed (that record is durable — mount read a successor) ⇒
//! released; `retire_seq >` mounted ⇒ still gated, parked pending until a
//! **post-mount** checkpoint is durable.
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

use super::alloc_ext_core::ExtCore;
use super::journal::ReplayedEntry;
use super::record::Record;
use super::KvError;
use std::path::Path;
use std::sync::atomic::AtomicU64;

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
pub fn compaction_reserve_extents(_total: u64) -> u64 {
    todo!()
}

/// Number of bitmap pages covering `total_extents`.
pub fn bitmap_pages_for(_total_extents: u64) -> u64 {
    todo!()
}

/// On-disk bitmap region length for `total_extents`: pages × 2 slots ×
/// 4 KiB.
pub fn bitmap_region_len(_total_extents: u64) -> u64 {
    todo!()
}

/// Big-endian extent key (memcmp-ordered, §4.2 key discipline).
pub fn extent_key(_extent: u64) -> [u8; EXTENT_KEY_LEN] {
    todo!()
}

/// Decode an extent key; length-checked (§9 bounds rule).
pub fn decode_extent_key(_key: &[u8]) -> Result<u64, KvError> {
    todo!()
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
pub fn alloc_record(_extent: u64, _seq: u64) -> (u8, Record) {
    todo!()
}

/// Build the journal record for a free tagged `retire_seq` (§4.7 —
/// the checkpoint seq that stops referencing the extent; 0 = never
/// referenced).
pub fn free_record(_extent: u64, _retire_seq: u64, _seq: u64) -> (u8, Record) {
    todo!()
}

/// Decode one allocator delta record (a `TREE_ALLOC_RESERVED`-tagged
/// journal record). Value tags/lengths are bounds-checked (§9); anything
/// malformed is structural corruption — the containing entry's checksum
/// already verified, so this is defense in depth against writer bugs.
pub fn decode_alloc_record(_rec: &Record) -> Result<AllocDelta, KvError> {
    todo!()
}

/// Encode one full 4 KiB page image: `bits` (≤ 4072 bytes, zero-padded)
/// stamped with `page_index` and `generation`.
pub fn encode_bitmap_page(
    _page_index: u32,
    _generation: u64,
    _bits: &[u8],
) -> Result<Vec<u8>, KvError> {
    todo!()
}

/// Decode + verify one page image against its expected `page_index`:
/// magic, index (misdirected-write guard), checksum. Any failure means
/// "this slot holds no valid page" — A/B selection treats it as absent,
/// never loud (the K3 ledger discipline).
pub fn decode_bitmap_page(_buf: &[u8], _page_index: u32) -> Result<(u64, &[u8]), KvError> {
    todo!()
}

/// Per-page write state: the newest valid generation on disk and which
/// physical slot holds it. Written by the serialized checkpoint task
/// (§4.6); atomics for `Sync`, not for contention.
struct PageState {
    _generation: AtomicU64,
    /// 0 = slot A, 1 = slot B, 2 = fresh (next write → A).
    _slot: AtomicU64,
}

/// The per-volume extent allocator: the lock-free in-RAM mirror
/// ([`ExtCore`]) plus A/B page persistence and dirty-page tracking.
///
/// Concurrency contract: `claim_*` / `free_pending` / `advance_durable` /
/// `release_unpublished` are lock-free and callable from any task;
/// [`Self::write_dirty_pages`] belongs to the serialized per-volume
/// checkpoint/writeback task (§4.6) — it is the only page-state writer.
pub struct ExtentAllocator {
    _core: ExtCore,
    _pages: u64,
    _page_states: Box<[PageState]>,
    /// Dirty-page bits (one per page): pages whose in-RAM bits diverged
    /// from the newest on-disk copy since the last [`Self::write_dirty_pages`].
    _dirty: Box<[AtomicU64]>,
}

impl std::fmt::Debug for ExtentAllocator {
    /// Summarizes instead of deriving: dumping the whole mirror bitmap
    /// into panic messages helps no one (the node-layer precedent).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtentAllocator").finish_non_exhaustive()
    }
}

impl ExtentAllocator {
    /// A fresh, all-free allocator (format time): `total_extents` with
    /// `reserve` held back for internal claims and a pending-free FIFO of
    /// `pending_cap` entries. Every page starts dirty (a fresh volume's
    /// first checkpoint persists the whole bitmap).
    pub fn format(_total_extents: u64, _reserve: u64, _pending_cap: usize) -> Self {
        todo!()
    }

    /// Mount: load the newest valid A/B slot per page (one region-sized
    /// `uring_fs::read_at`; invalid/missing slots read as absent — never
    /// loud, the K3 ledger discipline), seed the mirror, then replay the
    /// journal's allocator deltas over it (§4.7 "Mount": pages, then
    /// records ≥ tail).
    ///
    /// `mounted_seq` is the selected ledger record's checkpoint seq — the
    /// §4.7 durability floor: replayed `Freed` tags ≤ it are released
    /// (their gate already passed); tags > it are parked pending until a
    /// post-mount checkpoint is durable. `replay` is the recovered window
    /// (`JournalRecovery::entries`), seq-sorted; the caller's tail rule
    /// (K6b) keeps alloc/free records in the window until their effects
    /// are durable in bitmap pages.
    ///
    /// `Err` is real device I/O failure only ([`KvError::Io`]) — bitmap
    /// *contents* never fail a mount loud.
    pub async fn load(
        _path: &Path,
        _base: u64,
        _total_extents: u64,
        _reserve: u64,
        _pending_cap: usize,
        _mounted_seq: u64,
        _replay: &[ReplayedEntry],
    ) -> Result<Self, KvError> {
        todo!()
    }

    /// Total heap extents.
    pub fn total_extents(&self) -> u64 {
        todo!()
    }

    /// The compaction reserve in extents.
    pub fn reserve_extents(&self) -> u64 {
        todo!()
    }

    /// Claimable extents right now.
    pub fn free_extents(&self) -> u64 {
        todo!()
    }

    /// Pending-free entries awaiting their durable checkpoint.
    pub fn pending_count(&self) -> u64 {
        todo!()
    }

    /// Newest checkpoint seq known durable.
    pub fn durable_seq(&self) -> u64 {
        todo!()
    }

    /// Whether `extent` is allocated or pending-free.
    pub fn is_allocated(&self, _extent: u64) -> bool {
        todo!()
    }

    /// Newest bitmap generation on disk across pages — mount resumes
    /// generation numbering above `max(this, ledger.alloc_bitmap_generation)`
    /// so a new write can never tie an existing valid slot.
    pub fn resume_generation(&self) -> u64 {
        todo!()
    }

    /// Claim an extent for a **user op** (§4.7 ENOSPC semantics): fails
    /// with the typed [`KvError::NoSpace`] once the claim would dip the
    /// free budget to-or-below the compaction reserve — the reserve stays
    /// intact for [`Self::claim_internal`].
    pub fn claim_user(&self) -> Result<u64, KvError> {
        todo!()
    }

    /// Claim an extent for compaction/checkpoint/SMO internals: may
    /// consume the reserve (§4.7 — the tree can always fold appends and
    /// free space even at user-visible ENOSPC). [`KvError::NoSpace`] here
    /// means the heap is genuinely exhausted.
    pub fn claim_internal(&self) -> Result<u64, KvError> {
        todo!()
    }

    /// Enter `extent` into the pending-free list tagged `retire_seq` — the
    /// checkpoint seq that stops referencing it (§4.7). Not claimable
    /// until [`Self::advance_durable`] covers the tag. Errors with the
    /// typed [`KvError::PendingFreeFull`] at the §4.7 cap — the caller
    /// forces a checkpoint rather than reusing unsafely.
    pub fn free_pending(&self, _extent: u64, _retire_seq: u64) -> Result<(), KvError> {
        todo!()
    }

    /// The §4.7 gate: the root-ledger record with checkpoint seq `seq` is
    /// now *known durable* (post-barrier) — release every pending extent
    /// whose tag it covers back to the claimable pool. Returns how many
    /// extents were released (their pages are marked dirty for the next
    /// checkpoint).
    pub fn advance_durable(&self, _seq: u64) -> u64 {
        todo!()
    }

    /// Release an extent **no checkpoint ever referenced** (a node build
    /// abandoned before publication) straight back to the claimable pool.
    /// Checkpoint-referenced extents must go through
    /// [`Self::free_pending`] (§4.7).
    pub fn release_unpublished(&self, _extent: u64) {
        todo!()
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
        _path: &Path,
        _base: u64,
        _generation: u64,
    ) -> Result<Vec<u32>, KvError> {
        todo!()
    }
}
