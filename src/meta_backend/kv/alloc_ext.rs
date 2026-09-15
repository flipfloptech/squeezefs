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
//! (`alloc.rs` → `ENOSPC`). **The production user side is the commit
//! pass's heap admission, not a claim** (design §4.7 amended 2026-09-11):
//! a user commit's records land in leaf overlays and the flush pass
//! claims (internal class) for the SMOs they force, so the admission
//! PROMISES those extents against `free − promised` — splits above the
//! whole reserve, compactions down to [`compaction_floor_extents`] — and
//! refuses `NoSpace` where a claim would have.
//!
//! ## Append partitioning (pre-RC engineering spec §6.2 item 3)
//!
//! The A/B pages above are a **whole-volume single-appender** structure:
//! one dirty set, one page-state array, and one `advance_durable` tail.
//! [`ExtentAllocator::format_partitioned`] / [`ExtentAllocator::load_partitioned`]
//! give an appender its own view — the core's per-partition budgets,
//! reserves, pending-free FIFOs, and coverage clocks
//! ([`super::alloc_ext_core::PartitionMap`]) plus:
//!
//! * a *page-granular* ownership rule, because the page is the A/B write
//!   unit: two appenders writing one page would put a peer's newest copy in
//!   the slot this write replaces, which is exactly the clobber §6.2
//!   describes. A fresh partitioned appender starts only its OWN pages
//!   dirty, and [`ExtentAllocator::write_dirty_pages`] counts any foreign
//!   page it persists ([`ExtentAllocator::foreign_page_writes`]) — a
//!   tripwire, not a silent clobber;
//! * ownership enforcement on **replayed** deltas: a delta naming an
//!   extent outside its emitter's partition refuses the mount LOUD (the
//!   `PartitionViolation::Extent` arm's second line of defence);
//! * per-appender mounted tails, since a gate seq is a position in the
//!   freeing appender's own journal ring and a peer's tail says nothing
//!   about it.
//!
//! Everything an un-stamped (solo) volume does is unchanged: one partition
//! owning the whole bitmap, the shipped flat claim scan, the whole-volume
//! reserve, one clock.
//!
//! Like all K1–K5 modules, nothing here is mount-wired yet: the module is
//! kept alive by `tests/kv_alloc_tests.rs`, the PR K4 crash cases, and the
//! loom models (design PR-plan liveness convention). The partitioned
//! entry points are kept alive the same way — by
//! `tests/kv_partitioned_append_tests.rs` — until spec §6.9 S4 wires an
//! appender set into `KvMetaBackend::open`.

use super::alloc_ext_core::{AllocClass, ClaimError, ExtCore, PartitionMap};
use super::journal::{AppendPartition, MergedEntry, ReplayedEntry};
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

/// The §4.7 reserve's split for the commit-time **heap admission**
/// (`KvMetaBackend::run_batch_pipeline`): a user commit whose flush needs
/// a NEW leaf (a split) is admitted only while the WHOLE reserve stays
/// clear; one whose flush is a net-zero COMPACTION (deletes, overwrites,
/// a dead-heavy leaf) is admitted down to this floor — half the reserve
/// — so deletes keep committing on a full volume while the other half
/// stays the flush pass's own (interior SMOs, mount-time SMOs, the
/// projection's under-estimates). Derived from the reserve, never a
/// constant; tie-tested in `tests/meta_volume_full_tests.rs`.
pub fn compaction_floor_extents(reserve: u64) -> u64 {
    reserve / 2
}

/// The bitmap partition map an [`AppendPartition`] implies (spec §6.2
/// item 3): pages are the partition unit, so the map is
/// `writers`-way over [`ALLOC_PAGE_BITS`]-extent pages. Solo collapses to
/// one partition owning every page — the shipped structure.
fn partition_map_for(part: AppendPartition) -> PartitionMap {
    if part.is_solo() {
        // `extents_per_page = u64::MAX` would overflow the page division;
        // ALLOC_PAGE_BITS keeps the geometry honest and the single
        // partition owns every page either way (`page % 1 == 0`).
        PartitionMap::solo(ALLOC_PAGE_BITS)
    } else {
        PartitionMap::new(u64::from(part.writers()), ALLOC_PAGE_BITS)
    }
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
    /// Which appender this allocator instance IS (spec §6.2 item 3):
    /// `claim_user`/`claim_internal`/`advance_durable` act on this
    /// partition, and [`Self::write_dirty_pages`] expects to write only
    /// pages this appender owns. [`AppendPartition::SOLO`] is the shipped
    /// posture — one appender owning the whole bitmap.
    partition: AppendPartition,
    /// Pages this appender persisted that it does NOT own — the ownership
    /// tripwire. Zero in steady partitioned operation (a writer only
    /// dirties its own pages, because it only claims and frees its own
    /// extents); legitimately nonzero exactly once, on the recovery mount
    /// that replays a peer's window while holding the volume alone.
    foreign_page_writes: AtomicU64,
    /// The replayed in-window frees `load` deferred — `(writer, gate seq,
    /// extent)`, seq-sorted — until [`Self::park_replayed_frees`] knows
    /// the mounted roots. Empty on a formatted allocator and after the
    /// park.
    replayed_parks: std::sync::Mutex<Vec<(u16, u64, u64)>>,
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
        Self::format_partitioned(total_extents, reserve, pending_cap, AppendPartition::SOLO)
    }

    /// [`Self::format`] for appender `part` of a partitioned bitmap (spec
    /// §6.2 item 3): a fresh all-free bitmap of which this appender owns
    /// its own pages. Solo starts every page dirty (the shipped
    /// whole-bitmap first checkpoint); a partitioned appender starts only
    /// its OWN pages dirty, because writing a page it does not own is
    /// exactly the A/B clobber the partitioning exists to prevent.
    pub fn format_partitioned(
        total_extents: u64,
        reserve: u64,
        pending_cap: usize,
        part: AppendPartition,
    ) -> Self {
        let pages = bitmap_pages_for(total_extents);
        let page_states: Vec<PageState> = (0..pages)
            .map(|_| PageState {
                generation: AtomicU64::new(0),
                slot: AtomicU64::new(SLOT_NONE),
            })
            .collect();
        let map = partition_map_for(part);
        let dirty: Vec<AtomicU64> = (0..pages.div_ceil(64))
            .map(|w| {
                if part.is_solo() {
                    return AtomicU64::new(u64::MAX);
                }
                let mut bits = 0u64;
                for b in 0..64u64 {
                    let page = w as u64 * 64 + b;
                    if page < pages && map.owner_of_page(page) == u64::from(part.writer_id()) {
                        bits |= 1 << b;
                    }
                }
                AtomicU64::new(bits)
            })
            .collect();
        Self {
            core: ExtCore::new_partitioned(
                total_extents,
                reserve,
                pending_cap,
                partition_map_for(part),
            ),
            pages,
            page_states: page_states.into_boxed_slice(),
            dirty: dirty.into_boxed_slice(),
            partition: part,
            foreign_page_writes: AtomicU64::new(0),
            replayed_parks: std::sync::Mutex::new(Vec::new()),
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
        Self::load_inner(
            path,
            base,
            total_extents,
            reserve,
            pending_cap,
            AppendPartition::SOLO,
            &[mounted_tail],
            replay
                .iter()
                .map(|e| (0u16, e.seq, e.records.as_slice()))
                .collect(),
        )
        .await
    }

    /// [`Self::load`] for appender `part` of a **partitioned** bitmap
    /// (spec §6.2 item 3), taking the merged multi-appender replay window
    /// ([`super::journal::replay_merge`]) and one mounted tail per
    /// appender — coverage gates are per-appender clocks, so each
    /// partition's watermark seeds from ITS OWN ledger record's tail
    /// (index = writer id; a missing/absent record contributes 0, i.e. the
    /// most conservative seed: nothing is covered).
    ///
    /// Refuses LOUD on an allocator delta whose extent lies outside the
    /// emitting appender's partition — defense in depth behind the merge's
    /// own [`super::journal::PartitionViolation::Extent`] detector, so a
    /// foreign bit can never be applied silently even if a caller skipped
    /// the merge's policy step.
    pub async fn load_partitioned(
        path: &Path,
        base: u64,
        total_extents: u64,
        reserve: u64,
        pending_cap: usize,
        part: AppendPartition,
        mounted_tails: &[u64],
        replay: &[MergedEntry],
    ) -> Result<Self, KvError> {
        Self::load_inner(
            path,
            base,
            total_extents,
            reserve,
            pending_cap,
            part,
            mounted_tails,
            replay
                .iter()
                .map(|e| (e.writer_id, e.seq, e.records.as_slice()))
                .collect(),
        )
        .await
    }

    /// The shared mount body. `replay` is `(writer_id, entry seq, records)`
    /// in canonical order — for a solo mount every writer id is 0, which
    /// makes every partition/ownership branch below a no-op and the path
    /// byte-for-byte the shipped one.
    #[allow(clippy::too_many_arguments)]
    async fn load_inner(
        path: &Path,
        base: u64,
        total_extents: u64,
        reserve: u64,
        pending_cap: usize,
        part: AppendPartition,
        mounted_tails: &[u64],
        replay: Vec<(u16, u64, &[(u8, Record)])>,
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

        let map = partition_map_for(part);
        let mut core = ExtCore::new_partitioned(total_extents, reserve, pending_cap, map);
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
        // replay-currency §2-A mount gate). One seed per appender: gate
        // seqs are positions in the freeing appender's own ring, so a
        // peer's tail can neither cover nor release them.
        for writer in 0..u64::from(part.writers()) {
            let tail = mounted_tails.get(writer as usize).copied().unwrap_or(0);
            core.advance_durable_in(writer, tail);
        }

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
        let mut folded: std::collections::BTreeMap<u64, (u16, u64, AllocDelta)> =
            std::collections::BTreeMap::new();
        for (writer, entry_seq, records) in &replay {
            for (i, (tree_id, rec)) in records.iter().enumerate() {
                if *tree_id != TREE_ALLOC_RESERVED {
                    continue;
                }
                // PR 8: a DATA allocation bitmap delta (`vol_tag`-prefixed,
                // 16-byte key — `crate::data_alloc_bitmap`) shares the
                // kind; it is the allocation holder's and is applied by
                // its own replay arm, never to the heap bitmap.
                if crate::data_alloc_bitmap::is_data_alloc_delta_key(&rec.key) {
                    crate::data_alloc_bitmap::note_replayed_delta(rec);
                    continue;
                }
                let delta = decode_alloc_record(rec)?;
                let extent = decode_extent_key(&rec.key)?;
                // Ownership (spec §6.2 item 3): an appender may only name
                // extents in its own bitmap partition. Loud — a foreign
                // bit applied silently is precisely the cross-appender
                // corruption the partition exists to prevent. Vacuous on a
                // solo mount (one partition owns everything).
                let owner = map.owner_of_extent(extent) as u16;
                if owner != *writer {
                    return Err(KvError::Corrupt(format!(
                        "replayed allocator delta for extent {extent} came from writer \
                         {writer} at seq {entry_seq}, but that extent belongs to writer \
                         {owner}'s bitmap partition (spec §6.2 item 3 — the appenders were \
                         not disjoint)"
                    )));
                }
                // Entries arrive in canonical merge order; per-key
                // newest-wins is a plain overwrite (an extent's records
                // all come from its owner, whose seqs are monotonic). The
                // park gate is the record's POSITION (`entry_seq + i`,
                // `Reservation::record_gate`'s law) — the tail that
                // covers it is a position, and the record's STAMP sits
                // `seq_offset` above it on a ring that received a
                // handed-over slot (PR 4 review round 3, Issue 20).
                folded.insert(extent, (*writer, entry_seq + i as u64, delta));
            }
        }
        // Apply survivors; parked finals push per partition in seq order
        // (the FIFO's non-decreasing-gate contract), not extent order.
        let mut replay_dirty: Vec<u64> = Vec::with_capacity(folded.len());
        let mut parked: Vec<(u16, u64, u64)> = Vec::new(); // (writer, rec seq, extent)
        for (extent, (writer, seq, delta)) in &folded {
            match delta {
                AllocDelta::Allocated { .. } => core.mark_allocated(*extent),
                AllocDelta::Freed { retire_seq: 0, .. } => {
                    // Never referenced by any checkpoint (a build
                    // abandoned before publication): immediately
                    // reusable, as always.
                    core.release(*extent);
                }
                AllocDelta::Freed { .. } => {
                    // In-window by construction ⇒ the freeing swap/flips
                    // are not durably covered (the mounted record itself
                    // may be page-cache-only after a kill). The bit may
                    // live only in this same window (its claiming alloc
                    // folded away under this key's LWW; the pages may
                    // predate both) — set it now, idempotently, so the
                    // FIFO's claimed-while-pending contract holds when
                    // the park lands. The park itself is DEFERRED to
                    // [`Self::park_replayed_frees`]: whether a replayed
                    // free may park at all depends on the roots the
                    // mount ends up with, which the trees decide AFTER
                    // this load (the root-swap carve-out, doc there).
                    core.mark_allocated(*extent);
                    parked.push((*writer, *seq, *extent));
                }
            }
            replay_dirty.push(*extent);
        }
        parked.sort_unstable();

        let dirty: Vec<AtomicU64> = (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let alloc = Self {
            core,
            pages,
            page_states: page_states.into_boxed_slice(),
            dirty: dirty.into_boxed_slice(),
            partition: part,
            foreign_page_writes: AtomicU64::new(0),
            replayed_parks: std::sync::Mutex::new(parked),
        };
        for extent in replay_dirty {
            alloc.mark_dirty(extent);
        }
        Ok(alloc)
    }

    /// Park the replayed in-window frees [`Self::load`] deferred — every
    /// one EXCEPT a free of an extent one of `live_roots` names, which is
    /// DROPPED and counted on `meta_kv_replay_root_frees_dropped`.
    ///
    /// The root-swap carve-out (design-smo-replay-currency §2 C′): a root
    /// swap journals no pointer record, so its durable form is
    /// exclusively the next ledger / page / tree-0 record naming the new
    /// root. A kill between the swap and that record leaves the mount
    /// replaying through the PREDECESSOR — which is therefore LIVE again
    /// — while the swap's `free(old)` is in the window. Parking that free
    /// released the live root's extent at the first post-mount
    /// checkpoint, and the next claim (lowest-free-first) overwrote the
    /// tree's root with a fresh node image. The predecessor's free is
    /// the unpublished swap's and must not fold; the swap's successor
    /// image stays claimed-and-unrouted (fsck C13's class on a forest
    /// grant; the bounded leak the design states on a flat volume).
    ///
    /// Must run once, after every tree is open and adopted (the roots are
    /// final) and BEFORE any post-mount SMO (the FIFO's non-decreasing
    /// gate order per partition: every replayed seq precedes every
    /// post-mount one). Parking is FORCED (never refused): a recovered
    /// window can legitimately carry more frees than the FIFO cap — a
    /// tail pinned pre-crash accumulates parked SMO retirements without
    /// bound on the window's budget, and a loud refusal here made
    /// exactly that image UNMOUNTABLE (the §4.7 pinned-floor wedge's
    /// remount face, P2 2026-07-26 §9). Beyond-cap entries park in the
    /// overflow and drain at the first post-mount durable checkpoint like
    /// every other; the park routes to the extent's OWNER partition,
    /// whose clock is the only one that can cover its gate seq.
    /// Returns `(parked, dropped)`.
    pub fn park_replayed_frees(&self, live_roots: &std::collections::BTreeSet<u64>) -> (u64, u64) {
        let parked = std::mem::take(
            &mut *self
                .replayed_parks
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let (mut n_parked, mut dropped, mut overflowed) = (0u64, 0u64, 0u64);
        for (_writer, seq, extent) in parked {
            if live_roots.contains(&extent) {
                dropped += 1;
                super::META_KV_REPLAY_ROOT_FREES_DROPPED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::warn!(
                    "mount: replayed free of extent {extent} (seq {seq}) names a LIVE tree \
                     root — an unpublished root swap's retirement; dropped, the root stays \
                     allocated (meta_kv_replay_root_frees_dropped). Its successor image is \
                     unrouted: fsck C13 reclaims it on a forest grant"
                );
                continue;
            }
            if self.core.free_pending_forced(extent, seq) {
                overflowed += 1;
                super::META_KV_PENDING_FREE_OVERFLOW
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            n_parked += 1;
            super::META_KV_PENDING_FREE_PARKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if overflowed > 0 {
            log::warn!(
                "mount: {overflowed} replayed pending-free record(s) exceeded the FIFO \
                 cap and parked in the overflow (a pre-crash pinned tail accumulated \
                 them); they drain at the first post-mount durable checkpoint"
            );
        }
        (n_parked, dropped)
    }

    /// Total heap extents.
    pub fn total_extents(&self) -> u64 {
        self.core.total()
    }

    /// Which appender this allocator instance is (spec §6.2 item 3);
    /// [`AppendPartition::SOLO`] on every un-stamped volume.
    pub fn partition(&self) -> AppendPartition {
        self.partition
    }

    /// Whether `extent` lies in this appender's bitmap partition.
    pub fn owns_extent(&self, extent: u64) -> bool {
        self.core.map().owner_of_extent(extent) == u64::from(self.partition.writer_id())
    }

    /// Bitmap pages this appender persisted that it does not own — the
    /// ownership tripwire (see the field docs). 0 in steady partitioned
    /// operation and structurally 0 on a solo volume.
    pub fn foreign_page_writes(&self) -> u64 {
        self.foreign_page_writes.load(Ordering::Relaxed)
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

    /// Whether any bitmap page holds a delta the next
    /// [`Self::write_dirty_pages`] must persist — a claim, or a release the
    /// last checkpoint's coverage barrier made AFTER that cycle wrote its
    /// pages. The shutdown fixpoint's second convergence term: a final
    /// cycle whose barrier releases pending frees dirties their pages for a
    /// cycle that would otherwise never run, and the released images then
    /// read CLAIMED at every later mount (PR 11's finding).
    pub fn has_dirty_pages(&self) -> bool {
        self.dirty.iter().any(|w| w.load(Ordering::Acquire) != 0)
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

    /// [`Self::claim_user`] on **another** appender's partition — the S4
    /// hook (a mount that owns the volume alone can allocate on behalf of
    /// a partition whose appender is absent) and what lets one test
    /// exercise two partitions of one bitmap. Steady-state appenders use
    /// [`Self::claim_user`], which is their own partition.
    pub fn claim_user_in(&self, writer: u16) -> Result<u64, KvError> {
        self.claim_in(u64::from(writer), AllocClass::User)
    }

    /// Claim an extent for compaction/checkpoint/SMO internals: may
    /// consume the reserve (§4.7 — the tree can always fold appends and
    /// free space even at user-visible ENOSPC). [`KvError::NoSpace`] here
    /// means the heap is genuinely exhausted.
    pub fn claim_internal(&self) -> Result<u64, KvError> {
        self.claim(AllocClass::Internal)
    }

    fn claim(&self, class: AllocClass) -> Result<u64, KvError> {
        self.claim_in(u64::from(self.partition.writer_id()), class)
    }

    fn claim_in(&self, writer: u64, class: AllocClass) -> Result<u64, KvError> {
        match self.core.claim_in(writer, class) {
            Ok(extent) => {
                self.mark_dirty(extent);
                Ok(extent)
            }
            // The refusal reports THIS partition's budget (identical to
            // the whole-volume budget on a solo volume): an appender at
            // ENOSPC in its own partition is the §4.7 condition, and
            // naming the aggregate would make the message a lie under
            // partitioning.
            Err(ClaimError::NoSpace) => Err(KvError::NoSpace {
                free: self.core.free_extents_in(writer),
                reserve: self.core.reserve(),
            }),
            // RES-14: the core's budget and bitmap disagree. Pre-fix this
            // SPUN forever in a sync fn called from async (a permanently
            // consumed worker, no log line, no counter). It is now a
            // bounded refusal — loud here, where crate logging exists.
            Err(ClaimError::InvariantDrift) => {
                log::error!(
                    "KV extent allocator INVARIANT DRIFT: the free budget won an \
                     entitlement the bitmap could not honour after {} full rescans \
                     (free={}, reserve={}, total={}). Refusing the claim instead of \
                     spinning; the volume needs an fsck (class C2/C3)",
                    64,
                    self.core.free_extents_in(writer),
                    self.core.reserve(),
                    self.core.total(),
                );
                Err(KvError::NoSpace {
                    free: self.core.free_extents_in(writer),
                    reserve: self.core.reserve(),
                })
            }
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

    /// [`Self::free_pending`] that can never be refused (the §4.7
    /// cycle-break, P2 2026-07-26 §9 fix direction a): at cap the entry
    /// parks in the core's unbounded overflow, gated on the same durable
    /// tail. For retirements whose refusal would close the pinned-floor
    /// dependency cycle ONLY — the checkpoint flush pass's own
    /// compactions and mount-side re-parking of replayed in-window frees;
    /// threshold SMOs keep the [`Self::pending_has_room`] admission
    /// valve.
    pub fn free_pending_forced(&self, extent: u64, gate_seq: u64) {
        if self.core.free_pending_forced(extent, gate_seq) {
            super::META_KV_PENDING_FREE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        }
        super::META_KV_PENDING_FREE_PARKED.fetch_add(1, Ordering::Relaxed);
    }

    /// Producer-side FIFO headroom (§4.7 at-cap protocol, design-smo-
    /// replay-currency PR 4 clause a): the serialized SMO task checks
    /// this at admission — BEFORE the swap — so `PendingFreeFull` can
    /// only ever surface pre-swap (clean abort, claims released) and the
    /// post-swap push is guaranteed to fit (this task is the FIFO's only
    /// producer; drains only vacate). The valve applies to threshold
    /// SMOs only — see [`Self::free_pending_forced`].
    pub fn pending_has_room(&self) -> bool {
        self.core
            .pending_has_room_in(u64::from(self.partition.writer_id()))
    }

    /// [`Self::pending_has_room`] for an SMO that parks `n` retirements
    /// (the §4.6a merge parks two: both old siblings ride one entry).
    pub fn pending_has_room_for(&self, n: u64) -> bool {
        self.core
            .pending_room_in(u64::from(self.partition.writer_id()))
            >= n
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
        self.advance_durable_in(self.partition.writer_id(), tail)
    }

    /// [`Self::advance_durable`] for **another** appender's coverage clock
    /// (spec §6.2 item 3): gate seqs are positions in the freeing
    /// appender's own journal ring, so each partition drains on the tail of
    /// ITS OWN durable ledger record. A mount that recovers a peer's
    /// window while holding the volume alone advances that peer's clock
    /// this way; steady-state appenders use [`Self::advance_durable`].
    pub fn advance_durable_in(&self, writer: u16, tail: u64) -> u64 {
        let released = self.core.advance_durable_in(u64::from(writer), tail);
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
        self.mark_page_dirty(page);
    }

    /// Re-arm one page's dirty bit — the DUR-4 restore half of
    /// [`Self::write_dirty_pages`]'s snapshot-and-clear (the
    /// `restore_dying_floors` pattern from the sibling checkpoint path).
    fn mark_page_dirty(&self, page: u64) {
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
    ///
    /// **DUR-4 (two laws, both load-bearing).**
    ///
    /// 1. *Snapshot-and-clear is transactional.* Every fallible step past
    ///    the clear restores the snapshot's bits (`fetch_or`) before
    ///    returning, so the caller's retry still knows what to write.
    ///    Dropping them is unrecoverable: allocator deltas create no
    ///    dirty-node floor, so the next successful cycle advances the
    ///    tail past the alloc/free records that were the only remaining
    ///    copy, and remount then reads those extents FREE while live
    ///    nodes occupy them. (The pattern is `restore_dying_floors` in
    ///    the sibling checkpoint path.)
    /// 2. *A written image never ties an on-disk generation.* A/B
    ///    resolution is newest-valid-wins, so a tie makes the choice
    ///    between the slots arbitrary and the older bits can win. The
    ///    guard was `debug_assert!`-only — absent in release — and the
    ///    tie is a REACHABLE steady-state shape: a cycle that wrote its
    ///    pages and then failed (barrier or ledger write) leaves
    ///    `checkpoint_seq` unadvanced, so the retry arrives with the same
    ///    seq. Refusing the write would wedge that retry forever, so the
    ///    image's generation is RAISED above the page's newest valid copy
    ///    (`max(generation, newest + 1)`) — what the protocol actually
    ///    requires — and the raise is logged loud. Mount resumes
    ///    numbering above `max(page generations,
    ///    ledger.alloc_bitmap_generation)`, so a raised page generation
    ///    stays sound across remount.
    ///
    /// **Append partitioning (spec §6.2 item 3).** One page must have one
    /// appender, or the A/B alternation stops being single-appender and a
    /// peer's newest copy can be the slot this write replaces. That is a
    /// property of *ownership*, not of this function: an appender only
    /// dirties pages it claims and frees extents in. Pages this appender
    /// does not own are still written (mount recovery legitimately replays
    /// a peer's window while holding the volume alone) but counted in
    /// [`Self::foreign_page_writes`] and logged — the tripwire that says
    /// the partition leaked, rather than a silent clobber.
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

        if !self.partition.is_solo() {
            let mine = u64::from(self.partition.writer_id());
            let map = self.core.map();
            let foreign: Vec<u32> = to_write
                .iter()
                .copied()
                .filter(|p| map.owner_of_page(u64::from(*p)) != mine)
                .collect();
            if !foreign.is_empty() {
                self.foreign_page_writes
                    .fetch_add(foreign.len() as u64, Ordering::Relaxed);
                log::warn!(
                    "bitmap: appender {mine} is persisting {} page(s) it does not own \
                     ({:?}…) — legitimate only for a recovery mount holding the volume \
                     alone; in steady multi-appender operation this is a partition leak \
                     (spec §6.2 item 3)",
                    foreign.len(),
                    &foreign[..foreign.len().min(4)]
                );
            }
        }

        let words = self.core.snapshot_words();
        let mut ops: Vec<(u64, bytes::Bytes)> = Vec::with_capacity(to_write.len());
        let mut new_slots: Vec<(u32, u64, u64)> = Vec::with_capacity(to_write.len());
        for &page in &to_write {
            let state = &self.page_states[page as usize];
            let cur_gen = state.generation.load(Ordering::Acquire);
            let has_copy = state.slot.load(Ordering::Acquire) != SLOT_NONE;
            // Law 2: never emit an image whose generation ties or trails
            // the page's newest valid copy.
            let page_gen = if has_copy && generation <= cur_gen {
                let raised = cur_gen + 1;
                super::META_KV_BITMAP_GENERATION_RAISES.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "bitmap page {page}: requested generation {generation} does not exceed \
                     the newest on-disk copy {cur_gen} (a checkpoint retry after a failed \
                     cycle) — writing at {raised} instead; a tie would make \
                     newest-valid-wins pick between the A/B slots arbitrarily"
                );
                raised
            } else {
                generation
            };
            let target = match state.slot.load(Ordering::Acquire) {
                SLOT_NONE => 0,
                s => 1 - s,
            };
            let bits = self.page_bits(&words, u64::from(page));
            let image = match encode_bitmap_page(page, page_gen, &bits) {
                Ok(img) => img,
                // Law 1: the dirty set is this write's only copy.
                Err(e) => {
                    self.restore_dirty_pages(&to_write);
                    return Err(e);
                }
            };
            ops.push((
                base + u64::from(page) * 2 * ALLOC_PAGE_LEN + target * ALLOC_PAGE_LEN,
                bytes::Bytes::from(image),
            ));
            new_slots.push((page, target, page_gen));
        }

        let wrote = if ops.len() == 1 {
            let (off, data) = ops.pop().expect("one op");
            crate::uring_fs::write_at(path, off, data).await
        } else {
            crate::uring_fs::write_at_batch(path, ops).await
        };
        if let Err(e) = wrote {
            // Law 1 again — and note a partially-landed BATCH is safe to
            // re-dirty wholesale: rewriting a page that did land costs one
            // extra image at a higher generation, never a lost bit.
            self.restore_dirty_pages(&to_write);
            return Err(KvError::Io(e));
        }

        // The writes landed: record the new newest-valid slot per page.
        for (page, slot, page_gen) in new_slots {
            let state = &self.page_states[page as usize];
            state.generation.store(page_gen, Ordering::Release);
            state.slot.store(slot, Ordering::Release);
        }
        Ok(to_write)
    }

    /// DUR-4 law 1: fold a failed write's snapshot back into the dirty
    /// set so the retry still carries those pages.
    fn restore_dirty_pages(&self, pages: &[u32]) {
        for &page in pages {
            self.mark_page_dirty(u64::from(page));
        }
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
