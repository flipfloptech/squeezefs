//! **The data allocation bitmap** — durable free-list truth per DATA
//! volume, co-located with its allocation holder's ring
//! (docs/design-symmetric-metadata.md §5.5.1, KD-SYM-9; PR 8).
//!
//! Today the free list of a data volume is *the complement of the
//! referenced set below the cursor*, derived from ONE volume-wide by-block
//! census. In the forest the references are scattered over 65,536 slot
//! trees, so a successor allocation holder needs a truth it can recover
//! WITHOUT that walk — and the truth must be ORDERED so a stale page can
//! never show a granted range as free:
//!
//! * **one bit per block per data volume**, A/B page pairs exactly as the
//!   meta heap's bitmap (`kv::alloc_ext`'s pattern: newest-valid-page-
//!   wins, generation-stamped, checksummed). 1 TiB / 4 MiB = 262,144 bits
//!   = 32 KiB (×2 for A/B) — [`bitmap_bytes_for`] is the derivation;
//! * the pages live in extents of the holder's OWN extent grant on its
//!   HOME metadata volume — the same device as its ring, so "a page write
//!   covers the deltas ≤ its tail" is the meta-bitmap law verbatim on one
//!   device;
//! * **deltas are journal-resident records in the holder's OWN ring** —
//!   [`crate::meta_backend::kv::record::TREE_ALLOC_RESERVED`]'s pattern, kind 4 with a
//!   `vol_tag` prefix ([`data_alloc_key`]: 16 bytes, where the meta
//!   heap's extent key is 8 — the length IS the discriminator, and every
//!   meta-heap decoder that meets a 16-byte kind-4 key skips it through
//!   [`is_data_alloc_delta_key`]): a `BlockGrant` SETS its range BEFORE
//!   the reply, a terminal `FreeBlocks` CLEARS bits only at `finish_free`,
//!   so a clear bit means "reallocatable now";
//! * **recovery + the ordering law**: the dead holder's ring replay applies
//!   the deltas to the pages ([`DataAllocBitmap::replay`] — the arm PR 10's
//!   recovery driver runs; `KvMetaBackend::replay_data_alloc_deltas_for_
//!   region` is its caller and the contracts' seam) BEFORE `recovered:` is
//!   written, and volume 0's manager re-grants the allocation lease ONLY
//!   after that record exists — so a successor reads pages that show every
//!   journaled grant SET.
//!
//! ### Page image (4 KiB, little-endian)
//!
//! ```text
//! [0..4)    magic: u32       DATA_ALLOC_PAGE_MAGIC ("KVDA")
//! [4..8)    page_index: u32  which bitmap page (misdirected-write guard)
//! [8..16)   generation: u64  newest-valid-wins selector
//! [16..24)  vol_tag: u64     the DATA volume the bits describe (a page of
//!                            another volume's bitmap never reads as ours)
//! [24..32)  xxh3_64: u64     over the whole image, this field zeroed
//! [32..4096) bits            LSB-first; bit b of byte i = block
//!                            page_index·32,512 + i·8 + b
//! ```
//!
//! Physical layout at `base`: page `i`'s slot A at `base + i·8 KiB`, slot
//! B at `base + i·8 KiB + 4 KiB` — the meta bitmap's geometry.
//!
//! The C8 census stays the ORACLE: fsck compares the bitmap against
//! `Σ slot trees ∪ shared index ∪ open grants` ([`DataAllocBitmap::drift`])
//! — a set bit with no reference and no open grant is a LEAK to release, a
//! referenced block with a clear bit is the S1 class (report-only,
//! `data_alloc_bitmap_drift` must-stay-0 in the loss direction).
//!
//! Fuzzed by `fuzz/fuzz_targets/data_alloc_bitmap_page.rs`, mirrored on
//! stable in `tests/decoder_property_tests.rs`; contracts in
//! `tests/sym_block_grant_tests.rs`.

use crate::meta_backend::kv::record::{Record, TREE_ALLOC_RESERVED};
use crate::meta_backend::kv::KvError;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Physical page length (the meta bitmap's 4 KiB pages).
pub const DATA_ALLOC_PAGE_LEN: u64 = 4096;
/// Page header length (`magic | page_index | generation | vol_tag | xxh3`).
pub const DATA_ALLOC_PAGE_HDR_LEN: usize = 32;
/// Bit-payload bytes per page.
pub const DATA_ALLOC_PAGE_DATA_LEN: usize =
    (DATA_ALLOC_PAGE_LEN as usize) - DATA_ALLOC_PAGE_HDR_LEN;
/// Blocks covered by one page (`4064 × 8`).
pub const DATA_ALLOC_PAGE_BITS: u64 = (DATA_ALLOC_PAGE_DATA_LEN as u64) * 8;
/// Page magic (`"KVDA"`) — distinct from the meta bitmap's `"KVAB"` so a
/// misplaced page of the other structure never verifies.
pub const DATA_ALLOC_PAGE_MAGIC: u32 = 0x4B56_4441;

/// Delta key length: `vol_tag: u64 BE ‖ block_idx: u64 BE`. The meta
/// heap's extent key is 8 bytes — the length discriminates the two kind-4
/// families.
pub const DATA_ALLOC_KEY_LEN: usize = 16;
/// `Set` delta value tag (a grant's range, ahead of the reply).
pub const DATA_ALLOC_REC_SET: u8 = 0x11;
/// `Clear` delta value tag (`finish_free` — reallocatable now).
pub const DATA_ALLOC_REC_CLEAR: u8 = 0x12;

/// Slot marker for a page never yet written.
const SLOT_NONE: u64 = 2;

/// Bitmap pages covering `blocks`.
pub fn pages_for(blocks: u64) -> u64 {
    blocks.div_ceil(DATA_ALLOC_PAGE_BITS)
}

/// On-disk region length for `blocks`: pages × 2 slots × 4 KiB.
pub fn region_len(blocks: u64) -> u64 {
    pages_for(blocks) * 2 * DATA_ALLOC_PAGE_LEN
}

/// Bitmap payload bytes for a data volume of `capacity_bytes` at
/// `block_size` — one bit per block: 32 KiB per TiB at the shipped 4 MiB
/// block (§5.5.1's figure; tie-tested in `derivation_sweep_tests`). A zero
/// block size reads as zero blocks rather than dividing by it.
pub fn bitmap_bytes_for(capacity_bytes: u64, block_size: u64) -> u64 {
    if block_size == 0 {
        return 0;
    }
    (capacity_bytes / block_size).div_ceil(8)
}

/// xxh3 over a page image with the checksum field (bytes 24..32) zeroed —
/// the ledger / bset / meta-bitmap header-then-payload discipline.
fn page_checksum(image: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&image[..24]);
    h.update(&[0u8; 8]);
    h.update(&image[DATA_ALLOC_PAGE_HDR_LEN..]);
    h.digest()
}

#[inline]
fn le64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_le_bytes(b)
}

#[inline]
fn le32(v: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&v[off..off + 4]);
    u32::from_le_bytes(b)
}

#[inline]
fn be64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_be_bytes(b)
}

/// Encode one full 4 KiB page image of `vol_tag`'s bitmap: `bits` (≤ 4064
/// bytes, zero-padded) stamped with `page_index` and `generation`.
pub fn encode_data_alloc_page(
    vol_tag: u64,
    page_index: u32,
    generation: u64,
    bits: &[u8],
) -> Result<Vec<u8>, KvError> {
    if bits.len() > DATA_ALLOC_PAGE_DATA_LEN {
        return Err(KvError::Corrupt(format!(
            "data bitmap page payload {} exceeds the {DATA_ALLOC_PAGE_DATA_LEN}-byte page data \
             area",
            bits.len()
        )));
    }
    let mut image = vec![0u8; DATA_ALLOC_PAGE_LEN as usize];
    image[0..4].copy_from_slice(&DATA_ALLOC_PAGE_MAGIC.to_le_bytes());
    image[4..8].copy_from_slice(&page_index.to_le_bytes());
    image[8..16].copy_from_slice(&generation.to_le_bytes());
    image[16..24].copy_from_slice(&vol_tag.to_le_bytes());
    image[DATA_ALLOC_PAGE_HDR_LEN..DATA_ALLOC_PAGE_HDR_LEN + bits.len()].copy_from_slice(bits);
    let sum = page_checksum(&image);
    image[24..32].copy_from_slice(&sum.to_le_bytes());
    Ok(image)
}

/// Decode + verify one page image against the `vol_tag` and `page_index`
/// it must carry: magic, volume, index (misdirected-write guards),
/// checksum. Any failure means "this slot holds no valid page" — A/B
/// selection treats it as absent, never loud (the K3 ledger discipline).
/// Total over arbitrary bytes (fuzzed).
pub fn decode_data_alloc_page(
    buf: &[u8],
    vol_tag: u64,
    page_index: u32,
) -> Result<(u64, &[u8]), KvError> {
    if buf.len() < DATA_ALLOC_PAGE_LEN as usize {
        return Err(KvError::Corrupt(format!(
            "truncated data bitmap page: {} of {DATA_ALLOC_PAGE_LEN} bytes",
            buf.len()
        )));
    }
    let buf = &buf[..DATA_ALLOC_PAGE_LEN as usize];
    let magic = le32(buf, 0);
    if magic != DATA_ALLOC_PAGE_MAGIC {
        return Err(KvError::Corrupt(format!(
            "bad data bitmap page magic {magic:#010x} (expected {DATA_ALLOC_PAGE_MAGIC:#010x})"
        )));
    }
    let stored_index = le32(buf, 4);
    if stored_index != page_index {
        return Err(KvError::Corrupt(format!(
            "data bitmap page carries index {stored_index}, expected {page_index} (misdirected \
             write)"
        )));
    }
    let stored_tag = le64(buf, 16);
    if stored_tag != vol_tag {
        return Err(KvError::Corrupt(format!(
            "data bitmap page carries vol_tag {stored_tag:#018x}, expected {vol_tag:#018x} (a \
             page of another data volume's bitmap)"
        )));
    }
    let generation = le64(buf, 8);
    let stored = le64(buf, 24);
    let computed = page_checksum(buf);
    if stored != computed {
        return Err(KvError::ChecksumMismatch { stored, computed });
    }
    Ok((generation, &buf[DATA_ALLOC_PAGE_HDR_LEN..]))
}

// ---------------------------------------------------------------------------
// The kind-4 `vol_tag`-prefixed deltas
// ---------------------------------------------------------------------------

/// The delta record key: `vol_tag BE ‖ block_idx BE` (memcmp-ordered like
/// every §4.2 key; the volume-major order groups one volume's deltas).
pub fn data_alloc_key(vol_tag: u64, block_idx: u64) -> [u8; DATA_ALLOC_KEY_LEN] {
    let mut k = [0u8; DATA_ALLOC_KEY_LEN];
    k[..8].copy_from_slice(&vol_tag.to_be_bytes());
    k[8..].copy_from_slice(&block_idx.to_be_bytes());
    k
}

/// `true` ⇔ a kind-4 record key is a DATA bitmap delta (16 bytes) rather
/// than a meta-heap extent delta (8 bytes) — the one discriminator every
/// meta-heap decoder consults before it decodes.
pub fn is_data_alloc_delta_key(key: &[u8]) -> bool {
    key.len() == DATA_ALLOC_KEY_LEN
}

/// One decoded data bitmap delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataAllocDelta {
    /// The block was granted (or otherwise claimed) — the bit is set.
    Set { vol_tag: u64, block_idx: u64 },
    /// The block reached `finish_free` — the bit is clear.
    Clear { vol_tag: u64, block_idx: u64 },
}

impl DataAllocDelta {
    /// The volume the delta names.
    pub fn vol_tag(&self) -> u64 {
        match self {
            Self::Set { vol_tag, .. } | Self::Clear { vol_tag, .. } => *vol_tag,
        }
    }

    /// The block the delta names.
    pub fn block_idx(&self) -> u64 {
        match self {
            Self::Set { block_idx, .. } | Self::Clear { block_idx, .. } => *block_idx,
        }
    }
}

/// The journal record for a SET delta: `(TREE_ALLOC_RESERVED, Put)` in the
/// §4.4 staging shape. `seq` is the entry seq (re-stamped by the
/// reservation like every control record).
pub fn set_record(vol_tag: u64, block_idx: u64, seq: u64) -> (u8, Record) {
    (
        TREE_ALLOC_RESERVED,
        Record::put(
            data_alloc_key(vol_tag, block_idx).to_vec(),
            seq,
            vec![DATA_ALLOC_REC_SET],
        ),
    )
}

/// The journal record for a CLEAR delta.
pub fn clear_record(vol_tag: u64, block_idx: u64, seq: u64) -> (u8, Record) {
    (
        TREE_ALLOC_RESERVED,
        Record::put(
            data_alloc_key(vol_tag, block_idx).to_vec(),
            seq,
            vec![DATA_ALLOC_REC_CLEAR],
        ),
    )
}

/// Decode one data bitmap delta record. A key of the wrong length is the
/// caller's misroute (it should have asked [`is_data_alloc_delta_key`]
/// first); a malformed value is structural corruption — the containing
/// entry's checksum already verified, so this is defence in depth.
pub fn decode_data_alloc_record(rec: &Record) -> Result<DataAllocDelta, KvError> {
    if !is_data_alloc_delta_key(&rec.key) {
        return Err(KvError::Corrupt(format!(
            "data bitmap delta key must be {DATA_ALLOC_KEY_LEN} bytes, got {}",
            rec.key.len()
        )));
    }
    let vol_tag = be64(&rec.key, 0);
    let block_idx = be64(&rec.key, 8);
    match rec.value.as_slice() {
        [DATA_ALLOC_REC_SET] => Ok(DataAllocDelta::Set { vol_tag, block_idx }),
        [DATA_ALLOC_REC_CLEAR] => Ok(DataAllocDelta::Clear { vol_tag, block_idx }),
        other => Err(KvError::Corrupt(format!(
            "data bitmap delta record for vol_tag {vol_tag:#018x} block {block_idx} carries an \
             unknown value {other:02x?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// The in-RAM mirror + A/B persistence
// ---------------------------------------------------------------------------

/// Per-page write state: the newest valid generation on disk and which
/// physical slot holds it (the meta bitmap's `PageState`).
struct PageState {
    generation: AtomicU64,
    /// 0 = slot A, 1 = slot B, [`SLOT_NONE`] = fresh (next write → A).
    slot: AtomicU64,
}

/// The bitmap-vs-census verdict ([`DataAllocBitmap::drift`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftReport {
    /// Referenced blocks whose bit is CLEAR — the S1 class, report-only.
    pub loss: Vec<u64>,
    /// Set bits with no reference and no open grant — a leak to release.
    pub leak: Vec<u64>,
}

/// One data volume's allocation bitmap: the lock-free in-RAM mirror plus
/// the A/B page persistence and dirty-page tracking.
///
/// Concurrency contract: `set` / `clear` / `set_run` are lock-free and
/// callable from any task; [`Self::write_dirty_pages`] belongs to the
/// holder's serialized checkpoint task — it is the only page-state writer.
pub struct DataAllocBitmap {
    vol_tag: u64,
    blocks: u64,
    words: Box<[AtomicU64]>,
    pages: u64,
    page_states: Box<[PageState]>,
    dirty: Box<[AtomicU64]>,
    set_bits: AtomicU64,
    clear_bits: AtomicU64,
}

impl std::fmt::Debug for DataAllocBitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataAllocBitmap")
            .field("vol_tag", &format_args!("{:#018x}", self.vol_tag))
            .field("blocks", &self.blocks)
            .field("population", &self.population())
            .finish()
    }
}

impl DataAllocBitmap {
    /// A fresh (all-clear) bitmap over `blocks` of volume `vol_tag`.
    pub fn new(vol_tag: u64, blocks: u64) -> Self {
        let pages = pages_for(blocks);
        let words = (0..blocks.div_ceil(64))
            .map(|_| AtomicU64::new(0))
            .collect();
        let page_states = (0..pages)
            .map(|_| PageState {
                generation: AtomicU64::new(0),
                slot: AtomicU64::new(SLOT_NONE),
            })
            .collect();
        let dirty = (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        Self {
            vol_tag,
            blocks,
            words,
            pages,
            page_states,
            dirty,
            set_bits: AtomicU64::new(0),
            clear_bits: AtomicU64::new(0),
        }
    }

    /// Load from a region image (`region_len(blocks)` bytes, or shorter —
    /// a short image zero-extends and reads as fresh pages): per page the
    /// newest VALID slot wins; a page with no valid slot reads as clear.
    pub fn from_region_image(vol_tag: u64, blocks: u64, image: &[u8]) -> Self {
        let me = Self::new(vol_tag, blocks);
        let page_len = DATA_ALLOC_PAGE_LEN as usize;
        for page in 0..me.pages {
            let page_base = (page * 2 * DATA_ALLOC_PAGE_LEN) as usize;
            let mut newest: Option<(u64, u64, &[u8])> = None;
            for slot in 0..2u64 {
                let start = page_base + (slot as usize) * page_len;
                let Some(buf) = image.get(start..start + page_len) else {
                    continue;
                };
                if let Ok((generation, bits)) = decode_data_alloc_page(buf, vol_tag, page as u32) {
                    if newest.is_none_or(|(g, _, _)| generation > g) {
                        newest = Some((generation, slot, bits));
                    }
                }
            }
            if let Some((generation, slot, bits)) = newest {
                let first = page * DATA_ALLOC_PAGE_BITS;
                let last = (first + DATA_ALLOC_PAGE_BITS).min(blocks);
                for block in first..last {
                    let off = (block - first) as usize;
                    if bits[off / 8] & (1 << (off % 8)) != 0 {
                        me.words[(block / 64) as usize]
                            .fetch_or(1 << (block % 64), Ordering::Relaxed);
                    }
                }
                me.page_states[page as usize]
                    .generation
                    .store(generation, Ordering::Relaxed);
                me.page_states[page as usize]
                    .slot
                    .store(slot, Ordering::Relaxed);
            }
        }
        me
    }

    /// Read the region at `base` of `path` and load it.
    pub async fn load(
        path: &std::path::Path,
        base: u64,
        vol_tag: u64,
        blocks: u64,
    ) -> Result<Self, KvError> {
        let got = crate::uring_fs::read_at(path, base, region_len(blocks) as usize).await?;
        Ok(Self::from_region_image(vol_tag, blocks, &got))
    }

    /// The volume the bitmap describes.
    pub fn vol_tag(&self) -> u64 {
        self.vol_tag
    }

    /// Blocks covered.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Pages covering the blocks.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// `true` ⇔ `block`'s bit is set (allocated / granted).
    pub fn is_set(&self, block: u64) -> bool {
        if block >= self.blocks {
            return false;
        }
        self.words[(block / 64) as usize].load(Ordering::Acquire) & (1 << (block % 64)) != 0
    }

    /// Set `block`'s bit; `true` ⇔ it was clear. Out of range ⇒ `false`.
    pub fn set(&self, block: u64) -> bool {
        if block >= self.blocks {
            return false;
        }
        let prev = self.words[(block / 64) as usize].fetch_or(1 << (block % 64), Ordering::AcqRel);
        let newly = prev & (1 << (block % 64)) == 0;
        if newly {
            self.set_bits.fetch_add(1, Ordering::Relaxed);
            self.mark_dirty(block);
        }
        newly
    }

    /// Clear `block`'s bit; `true` ⇔ it was set. Out of range ⇒ `false`.
    pub fn clear(&self, block: u64) -> bool {
        if block >= self.blocks {
            return false;
        }
        let prev =
            self.words[(block / 64) as usize].fetch_and(!(1 << (block % 64)), Ordering::AcqRel);
        let was = prev & (1 << (block % 64)) != 0;
        if was {
            self.clear_bits.fetch_add(1, Ordering::Relaxed);
            self.mark_dirty(block);
        }
        was
    }

    /// Set every bit of `[start, start + len)`; returns how many were
    /// newly set (a grant of a range that is partly set is the replay /
    /// idempotency shape, never an error here — the ledger judges it).
    pub fn set_run(&self, start: u64, len: u64) -> u64 {
        let end = start.saturating_add(len).min(self.blocks);
        (start..end).filter(|b| self.set(*b)).count() as u64
    }

    /// Clear every bit of `[start, start + len)`; returns how many were set.
    pub fn clear_run(&self, start: u64, len: u64) -> u64 {
        let end = start.saturating_add(len).min(self.blocks);
        (start..end).filter(|b| self.clear(*b)).count() as u64
    }

    /// Bits set (allocated + granted).
    pub fn population(&self) -> u64 {
        self.words
            .iter()
            .map(|w| u64::from(w.load(Ordering::Relaxed).count_ones()))
            .sum()
    }

    /// Cumulative bits set / cleared since construction (the
    /// `data_alloc_bitmap_{set,clear}_bits` gauges).
    pub fn set_count(&self) -> u64 {
        self.set_bits.load(Ordering::Relaxed)
    }

    /// See [`Self::set_count`].
    pub fn clear_count(&self) -> u64 {
        self.clear_bits.load(Ordering::Relaxed)
    }

    /// The highest set bit, or `None` on an all-clear bitmap — the
    /// recovery floor's bitmap term (a grant's END is a set bit).
    pub fn highest_set(&self) -> Option<u64> {
        for (i, w) in self.words.iter().enumerate().rev() {
            let v = w.load(Ordering::Acquire);
            if v != 0 {
                return Some(i as u64 * 64 + 63 - u64::from(v.leading_zeros()));
            }
        }
        None
    }

    /// The lowest run of CLEAR bits at or above `floor`, capped at `want`
    /// bits — the carve a ranged block grant takes. `None` ⇔ no clear bit
    /// at or above `floor` (the volume is full for this holder).
    pub fn first_clear_run(&self, floor: u64, want: u64) -> Option<(u64, u64)> {
        if want == 0 {
            return None;
        }
        let mut b = floor;
        while b < self.blocks {
            let w = self.words[(b / 64) as usize].load(Ordering::Acquire);
            if w == u64::MAX {
                b = (b / 64 + 1) * 64;
                continue;
            }
            if w & (1 << (b % 64)) != 0 {
                b += 1;
                continue;
            }
            let start = b;
            let mut len = 0u64;
            while b < self.blocks && len < want && !self.is_set(b) {
                len += 1;
                b += 1;
            }
            return Some((start, len));
        }
        None
    }

    /// Every set bit, ascending (the census's face; sized by population).
    pub fn set_blocks(&self) -> Vec<u64> {
        let mut out = Vec::with_capacity(self.population() as usize);
        for (i, w) in self.words.iter().enumerate() {
            let mut v = w.load(Ordering::Acquire);
            while v != 0 {
                let bit = v.trailing_zeros();
                let block = i as u64 * 64 + u64::from(bit);
                if block < self.blocks {
                    out.push(block);
                }
                v &= v - 1;
            }
        }
        out
    }

    fn mark_dirty(&self, block: u64) {
        let page = block / DATA_ALLOC_PAGE_BITS;
        self.dirty[(page / 64) as usize].fetch_or(1 << (page % 64), Ordering::AcqRel);
    }

    /// `true` ⇔ some page diverged from its newest on-disk copy.
    pub fn has_dirty_pages(&self) -> bool {
        self.dirty.iter().any(|w| w.load(Ordering::Acquire) != 0)
    }

    fn page_bits(&self, page: u64) -> Vec<u8> {
        let first = page * DATA_ALLOC_PAGE_BITS;
        let last = (first + DATA_ALLOC_PAGE_BITS).min(self.blocks);
        let mut bits = vec![0u8; ((last - first) as usize).div_ceil(8)];
        for block in first..last {
            if self.is_set(block) {
                let off = (block - first) as usize;
                bits[off / 8] |= 1 << (off % 8);
            }
        }
        bits
    }

    /// The A/B write plan for every dirty page at `generation`: `(page,
    /// byte offset within the region, image)`. Each page's slot is
    /// ALTERNATED (never overwriting the newest valid copy) and its state
    /// advanced — the caller MUST write what it is handed (a plan taken
    /// and dropped leaves the page state ahead of the device, which only
    /// costs the next plan a re-write since the dirty bit is set again by
    /// any later mutation; `restore_dirty` puts the bits back on a failed
    /// write).
    pub fn take_dirty_plan(&self, generation: u64) -> Vec<(u32, u64, Vec<u8>)> {
        let mut pages: Vec<u32> = Vec::new();
        for (w, word) in self.dirty.iter().enumerate() {
            let mut rest = word.swap(0, Ordering::AcqRel);
            while rest != 0 {
                let b = rest.trailing_zeros();
                let page = (w as u64) * 64 + u64::from(b);
                if page < self.pages {
                    pages.push(page as u32);
                }
                rest &= rest - 1;
            }
        }
        pages
            .into_iter()
            .filter_map(|page| {
                let state = &self.page_states[page as usize];
                let cur = state.slot.load(Ordering::Acquire);
                let next = if cur == SLOT_NONE { 0 } else { 1 - cur };
                let gen = state.generation.load(Ordering::Acquire).max(generation - 1) + 1;
                let image = encode_data_alloc_page(
                    self.vol_tag,
                    page,
                    gen,
                    &self.page_bits(u64::from(page)),
                )
                .ok()?;
                state.slot.store(next, Ordering::Release);
                state.generation.store(gen, Ordering::Release);
                let off = u64::from(page) * 2 * DATA_ALLOC_PAGE_LEN + next * DATA_ALLOC_PAGE_LEN;
                Some((page, off, image))
            })
            .collect()
    }

    /// Put `pages` back on the dirty set (a failed write).
    pub fn restore_dirty(&self, pages: &[u32]) {
        for p in pages {
            let page = u64::from(*p);
            self.dirty[(page / 64) as usize].fetch_or(1 << (page % 64), Ordering::AcqRel);
        }
    }

    /// Write every dirty page's newest image into its ALTERNATE slot at
    /// `base` of `path` (the holder's checkpoint step); returns the pages
    /// written. The caller barriers.
    pub async fn write_dirty_pages(
        &self,
        path: &std::path::Path,
        base: u64,
        generation: u64,
    ) -> Result<Vec<u32>, KvError> {
        let plan = self.take_dirty_plan(generation);
        if plan.is_empty() {
            return Ok(Vec::new());
        }
        let pages: Vec<u32> = plan.iter().map(|(p, _, _)| *p).collect();
        let ops: Vec<(u64, bytes::Bytes)> = plan
            .into_iter()
            .map(|(_, off, image)| (base + off, bytes::Bytes::from(image)))
            .collect();
        if let Err(e) = crate::uring_fs::write_at_batch(path, ops).await {
            self.restore_dirty(&pages);
            return Err(KvError::Io(e));
        }
        Ok(pages)
    }

    /// A COMPLETE region image of the current bits at `generation` — every
    /// page written into slot A, slot B zeroed: the successor's COPY of a
    /// recovered bitmap into extents of its own grant (§5.5.1), where no
    /// predecessor copy exists to alternate against.
    pub fn region_image(&self, generation: u64) -> Result<Vec<u8>, KvError> {
        let mut out = vec![0u8; region_len(self.blocks) as usize];
        for page in 0..self.pages {
            let image = encode_data_alloc_page(
                self.vol_tag,
                page as u32,
                generation,
                &self.page_bits(page),
            )?;
            let off = (page * 2 * DATA_ALLOC_PAGE_LEN) as usize;
            out[off..off + DATA_ALLOC_PAGE_LEN as usize].copy_from_slice(&image);
        }
        Ok(out)
    }

    /// **The recovery arm** (§5.5.1): apply the window's kind-4 deltas of
    /// THIS volume to the loaded pages, per-key LWW by record seq FIRST
    /// (the K1 fold shape — a grant's SET superseded by a later CLEAR of
    /// the same block folds to the clear), then the survivors. Records of
    /// other volumes and non-delta keys are ignored. Returns the bits the
    /// replay changed.
    pub fn replay<'a>(&self, records: impl IntoIterator<Item = (u8, &'a Record)>) -> u64 {
        let mut folded: std::collections::BTreeMap<u64, (u64, DataAllocDelta)> =
            std::collections::BTreeMap::new();
        for (tree_id, rec) in records {
            if tree_id != TREE_ALLOC_RESERVED || !is_data_alloc_delta_key(&rec.key) {
                continue;
            }
            let Ok(delta) = decode_data_alloc_record(rec) else {
                continue;
            };
            if delta.vol_tag() != self.vol_tag {
                continue;
            }
            match folded.entry(delta.block_idx()) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert((rec.seq, delta));
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if rec.seq >= o.get().0 {
                        o.insert((rec.seq, delta));
                    }
                }
            }
        }
        let mut changed = 0u64;
        for (block, (_, delta)) in folded {
            let moved = match delta {
                DataAllocDelta::Set { .. } => self.set(block),
                DataAllocDelta::Clear { .. } => self.clear(block),
            };
            if moved {
                changed += 1;
            }
        }
        changed
    }

    /// **The oracle** (§5.5.1 / §5.8.5): the bitmap against the census —
    /// `referenced` (Σ slot trees ∪ the shared index, by block index) and
    /// the OPEN grants (`granted` ranges). LOSS = referenced ∧ clear (the
    /// S1 class, report-only, must-stay-0); LEAK = set ∧ ¬referenced ∧
    /// ¬granted (release).
    pub fn drift(&self, referenced: &BTreeSet<u64>, granted: &[(u64, u64)]) -> DriftReport {
        let in_grant = |b: u64| {
            granted
                .iter()
                .any(|(s, l)| b >= *s && b < s.saturating_add(*l))
        };
        let loss: Vec<u64> = referenced
            .iter()
            .copied()
            .filter(|b| *b < self.blocks && !self.is_set(*b))
            .collect();
        let leak: Vec<u64> = self
            .set_blocks()
            .into_iter()
            .filter(|b| !referenced.contains(b) && !in_grant(*b))
            .collect();
        DriftReport { loss, leak }
    }
}

// ---------------------------------------------------------------------------
// The replayed window's deltas — kept until a holding adopts them
// ---------------------------------------------------------------------------

/// The data deltas the mount's journal replay met in the window, by
/// volume tag, kept until the recovery arm applies them to that volume's
/// pages. Load-bearing: the mount's own bring-up checkpoint advances the
/// ledger tail past the window BEFORE any allocation lease is re-held on
/// it, so a replay that read the ring alone would find nothing — while
/// the pages on the device still show a journaled grant as FREE (§5.5.1's
/// headline row). Bounded by the window's size; drained per volume by
/// [`take_replayed_deltas`]. Empty on every flat volume (nothing writes a
/// 16-byte kind-4 key there).
static REPLAYED_DELTAS: once_cell::sync::Lazy<parking_lot::Mutex<Vec<(u64, Record)>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(Vec::new()));

/// The journal replay met a data delta record in the window.
pub fn note_replayed_delta(rec: &Record) {
    if !is_data_alloc_delta_key(&rec.key) {
        return;
    }
    let vol_tag = be64(&rec.key, 0);
    REPLAYED_DELTAS.lock().push((vol_tag, rec.clone()));
}

/// Take the window's deltas kept for `vol_tag` (the recovery arm's input
/// beside the live ring scan).
pub fn take_replayed_deltas(vol_tag: u64) -> Vec<Record> {
    let mut kept = REPLAYED_DELTAS.lock();
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *kept)
        .into_iter()
        .partition(|(t, _)| *t == vol_tag);
    *kept = rest;
    mine.into_iter().map(|(_, r)| r).collect()
}

/// Deltas kept across every volume (the tests' witness).
pub fn replayed_deltas_kept() -> usize {
    REPLAYED_DELTAS.lock().len()
}

/// Test seam: forget every kept delta.
pub fn test_clear_replayed_deltas() {
    REPLAYED_DELTAS.lock().clear();
}

// ---------------------------------------------------------------------------
// The process gauges (the Allocation-lease family, §11)
// ---------------------------------------------------------------------------

/// `data_alloc_bitmap_drift` — loss-direction findings (must-stay-0).
pub static DATA_ALLOC_BITMAP_DRIFT: AtomicU64 = AtomicU64::new(0);

/// Count a drift verdict's loss half on the process gauge; returns it.
pub fn note_drift(report: &DriftReport) -> u64 {
    let loss = report.loss.len() as u64;
    if loss > 0 {
        DATA_ALLOC_BITMAP_DRIFT.fetch_add(loss, Ordering::Relaxed);
        log::error!(
            "data allocation bitmap DRIFT (loss direction): {loss} referenced block(s) read CLEAR \
             in the bitmap — the S1 class, report-only (data_alloc_bitmap_drift; first: {:?})",
            report.loss.first()
        );
    }
    loss
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_round_trips_and_refuses_the_wrong_volume_and_index() {
        let bits = vec![0xA5u8; 100];
        let img = encode_data_alloc_page(7, 3, 9, &bits).unwrap();
        let (g, got) = decode_data_alloc_page(&img, 7, 3).unwrap();
        assert_eq!(g, 9);
        assert_eq!(&got[..100], &bits[..]);
        assert!(decode_data_alloc_page(&img, 8, 3).is_err());
        assert!(decode_data_alloc_page(&img, 7, 4).is_err());
        let mut torn = img.clone();
        torn[100] ^= 1;
        assert!(decode_data_alloc_page(&torn, 7, 3).is_err());
    }

    #[test]
    fn deltas_fold_lww_and_the_run_carve_skips_set_bits() {
        let bm = DataAllocBitmap::new(1, 200);
        assert_eq!(bm.set_run(10, 5), 5);
        let (s, l) = bm.first_clear_run(0, 64).unwrap();
        assert_eq!((s, l), (0, 10));
        let (s, l) = bm.first_clear_run(10, 64).unwrap();
        assert_eq!((s, l), (15, 64));
        let recs = [
            set_record(1, 3, 1),
            clear_record(1, 3, 2),
            set_record(1, 4, 3),
            set_record(2, 5, 4),
        ];
        let changed = bm.replay(recs.iter().map(|(t, r)| (*t, r)));
        assert_eq!(changed, 1);
        assert!(!bm.is_set(3) && bm.is_set(4) && !bm.is_set(5));
        assert_eq!(bm.highest_set(), Some(14));
    }
}
