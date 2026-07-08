//! Exclusive-owner, copy-on-write active-block accumulation buffers
//! (zero-copy write-path design §5.2 PR 1 + §5.3 PR 4).
//!
//! [`ActiveBlockBuf`] replaces the shared `bytes::Bytes` value of
//! `active_block_buffers` and makes aliasing-and-mutation impossible by
//! construction: the write merge mutates only provably-unique memory
//! ([`ActiveBlockBuf::make_mut`], gated by `Arc::get_mut` via the
//! `cow_core::CowCell` protocol core), while readers and staging/upload paths
//! take immutable [`ActiveBlockBuf::snapshot`]s that stay byte-identical
//! for their whole lifetime — a writer that finds a live snapshot copies
//! first (CoW), and the copy is observable as the
//! `active_block_cow_copies` stat. Sequential streams never collide with a
//! snapshot of a still-accumulating block, so they never pay the copy.
//!
//! PR 4 makes the `covered` interval load-bearing (memset elision, §5.3):
//! [`ActiveBlockBuf::fresh`] buffers are born **uncovered** — no seed-time
//! zero-fill — and `covered` tracks the contiguous initialized range
//! `[covered.0, covered.1)`. It is pure elision bookkeeping, **never**
//! write-through trigger input. The §5.3 contract: recycled pool bytes
//! never leave the covered range — not to the kernel (the read path serves
//! sparse holes via [`ActiveBlockBuf::covered_snapshot`]), not to staging
//! or the device ([`ActiveBlockBuf::zero_complete`] under the block lock at
//! the trigger and at every stage/upload exit). All `covered` mutations run
//! under `BLOCK_FLUSH_LOCKS` (the write merge, spill, fsync/teardown exits
//! all hold the entry's block lock); readers stay lock-free.
//!
//! Backing memory is block-sized and 4096-aligned from `ALIGNED_BUF_POOL`
//! (recycled on drop of the last handle), which keeps snapshots eligible
//! for `nvme_dev::write_block`'s aligned zero-copy DMA branch. Pool memory
//! is zeroed at OS-allocation birth (`alloc_zeroed`) and only ever written
//! through safe slices afterwards, so an uncovered buffer holds *stale but
//! initialized* bytes — forming `&[u8]` views over it is sound; the
//! coverage contract is what keeps the stale content private.

use crate::cache::pool::ALIGNED_BUF_POOL;
use crate::cow_core::sync::Arc;
use crate::cow_core::CowCell;
use std::sync::atomic::Ordering;

/// A block-sized, 4096-aligned allocation. Buffers no larger than the
/// pool's buffer size borrow pool memory (using a `len`-byte prefix, as the
/// previous `AlignedBufOwner` usage did) and recycle it on drop; larger
/// block-size configurations fall back to a dedicated aligned allocation
/// so the buffer can never overrun pooled memory.
struct AlignedBlock {
    ptr: *mut u8,
    len: usize,
    pooled: bool,
}

// SAFETY: `ptr` designates a uniquely-owned allocation of `len` bytes.
// Shared (`&`) access only reads it; mutation happens exclusively through
// `as_mut_slice(&mut self)`, which `CowCell::owned_mut` hands out only when
// the owning `Arc` is provably unique — so cross-thread access is either
// all-shared reads of initialized memory or exclusive mutation.
unsafe impl Send for AlignedBlock {}
unsafe impl Sync for AlignedBlock {}

impl AlignedBlock {
    /// Allocate `len` bytes of initialized memory: recycled pool content
    /// (stale bytes from prior safe writes; zeroed at OS-allocation birth)
    /// or a fresh zeroed dedicated allocation for oversized blocks. The
    /// §5.3 `covered` contract keeps stale content from ever escaping.
    fn alloc_raw(len: usize) -> Self {
        if len <= ALIGNED_BUF_POOL.buf_size() {
            Self {
                ptr: ALIGNED_BUF_POOL.alloc_raw(),
                len,
                pooled: true,
            }
        } else {
            let layout = Self::oversized_layout(len);
            // SAFETY: `layout` has non-zero size (len > pool buf_size > 0).
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "aligned active-block allocation failed");
            Self {
                ptr,
                len,
                pooled: false,
            }
        }
    }

    fn oversized_layout(len: usize) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(len, 4096)
            .expect("active-block layout (len rounded, 4096 align)")
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` initialized bytes for the
        // lifetime of `self` (pool memory is zeroed at birth and only
        // written through safe slices; oversized allocations are zeroed).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `&mut self` is only reachable through
        // `CowCell::owned_mut`, which proves the owning Arc unique — no
        // snapshot aliases this memory.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        if self.pooled {
            ALIGNED_BUF_POOL.recycle(self.ptr);
        } else {
            // SAFETY: allocated in `alloc_raw` with this exact layout.
            unsafe { std::alloc::dealloc(self.ptr, Self::oversized_layout(self.len)) };
        }
    }
}

/// Keep-alive owner behind [`ActiveBlockBuf::snapshot`] `Bytes`: pins the
/// block allocation (and thereby forces CoW on any later writer) until the
/// last snapshot handle drops.
struct SnapshotOwner(Arc<AlignedBlock>);

impl AsRef<[u8]> for SnapshotOwner {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// A block-sized, 4096-aligned active-block accumulation buffer.
///
/// Mutation requires provable uniqueness (`Arc::get_mut`); shared
/// snapshots force copy-on-write. Deliberately not `Clone`: the map entry
/// is the exclusive owner, and [`ActiveBlockBuf::snapshot`] is the only
/// sharing primitive.
pub struct ActiveBlockBuf {
    cell: CowCell<AlignedBlock>,
    /// Contiguous initialized range `[covered.0, covered.1)` — §5.3
    /// memset-elision bookkeeping. Seeded buffers are born fully covered;
    /// [`ActiveBlockBuf::fresh`] buffers grow it via
    /// [`ActiveBlockBuf::record_write`]. Never write-through trigger input.
    covered: (u32, u32),
}

impl ActiveBlockBuf {
    /// A fresh accumulation buffer for a block with **no existing data**:
    /// born uncovered, with the seed-time zero-fill elided (§5.3). The
    /// complement's correct content is zeros by definition — established
    /// lazily by [`ActiveBlockBuf::record_write`] (gap degrade) or
    /// [`ActiveBlockBuf::zero_complete`] (trigger / stage / upload exits).
    pub fn fresh(block_size: usize) -> Self {
        Self {
            cell: CowCell::new(AlignedBlock::alloc_raw(block_size)),
            covered: (0, 0),
        }
    }

    /// A block seeded from existing content (RMW / staged / promotion
    /// seeds): copies `min(existing.len(), block_size)` bytes and
    /// zero-fills the remainder, so the buffer is born content-valid at
    /// full block size regardless of the seed's length.
    pub fn seeded(existing: &[u8], block_size: usize) -> Self {
        let block = AlignedBlock::alloc_raw(block_size);
        let copy_len = existing.len().min(block_size);
        // SAFETY: `block.ptr` is a fresh `block_size`-byte allocation;
        // `existing` provides `copy_len` initialized source bytes and the
        // two regions cannot overlap.
        unsafe {
            if copy_len > 0 {
                std::ptr::copy_nonoverlapping(existing.as_ptr(), block.ptr, copy_len);
            }
            if block_size > copy_len {
                std::ptr::write_bytes(block.ptr.add(copy_len), 0, block_size - copy_len);
            }
        }
        Self {
            cell: CowCell::new(block),
            covered: (0, block_size as u32),
        }
    }

    /// The covered (initialized) interval `[start, end)`.
    pub fn covered(&self) -> (u32, u32) {
        self.covered
    }

    /// Content-valid: every byte is the block's correct current content
    /// (Seeded buffers always; Fresh buffers once fully covered).
    pub fn is_content_valid(&self) -> bool {
        self.covered.0 == 0 && self.covered.1 as usize == self.cell.peek().len
    }

    /// Record a write of `[start, end)` **before** merging it via
    /// [`ActiveBlockBuf::make_mut`]. Overlapping/abutting writes extend the
    /// covered interval; a gap write zeroes the uncovered complement and
    /// degrades the buffer to fully-initialized (§5.3 state machine:
    /// `Accumulating_Fresh → Accumulating_Seeded`). Callers hold this
    /// block's `BLOCK_FLUSH_LOCKS` (as all mutations do).
    pub fn record_write(&mut self, start: usize, end: usize) {
        let len = self.cell.peek().len;
        debug_assert!(
            start <= end && end <= len,
            "write range out of block bounds"
        );
        if self.is_content_valid() {
            return;
        }
        let (c0, c1) = (self.covered.0 as usize, self.covered.1 as usize);
        if c0 == c1 {
            // First touch.
            if start == 0 && end == len {
                self.complete_coverage(0);
            } else {
                self.covered = (start as u32, end as u32);
            }
        } else if start <= c1 && end >= c0 {
            // Overlaps or abuts: extend the interval.
            let new = (c0.min(start), c1.max(end));
            if new == (0, len) {
                self.complete_coverage(0);
            } else {
                self.covered = (new.0 as u32, new.1 as u32);
            }
        } else {
            // Gap write: zero the whole uncovered complement (the correct
            // content of a Fresh block's complement *is* zeros) and degrade
            // to fully-initialized — recycled bytes can now never escape.
            self.zero_complement();
        }
    }

    /// Establish content-validity at a trigger / stage / upload exit: zero
    /// the uncovered complement (elided when the buffer is fully covered or
    /// was born Seeded). Idempotent. Callers hold this block's
    /// `BLOCK_FLUSH_LOCKS`.
    pub fn zero_complete(&mut self) {
        if self.is_content_valid() {
            return;
        }
        self.zero_complement();
    }

    fn zero_complement(&mut self) {
        let len = self.cell.peek().len;
        let (c0, c1) = (self.covered.0 as usize, self.covered.1 as usize);
        let zeroed = c0 + (len - c1);
        let slice = self.make_mut();
        slice[..c0].fill(0);
        slice[c1..].fill(0);
        self.complete_coverage(zeroed);
    }

    /// Single Fresh → content-valid transition: record how many memset
    /// bytes the elision actually saved vs today's unconditional
    /// block-sized seed zero-fill.
    fn complete_coverage(&mut self, zeroed_bytes: usize) {
        let len = self.cell.peek().len;
        self.covered = (0, len as u32);
        crate::fuse_client::METRICS
            .active_block_memset_elided_bytes
            .fetch_add((len - zeroed_bytes) as u64, Ordering::Relaxed);
    }

    /// Zero-copy immutable snapshot for readers (read-your-own-writes) and
    /// for staging/upload. The snapshot is immutable forever: any later
    /// writer that finds it alive copies first (CoW). Staging/upload
    /// callers must [`ActiveBlockBuf::zero_complete`] first (§5.3);
    /// coverage-aware readers use [`ActiveBlockBuf::covered_snapshot`].
    pub fn snapshot(&self) -> bytes::Bytes {
        bytes::Bytes::from_owner(SnapshotOwner(self.cell.share()))
    }

    /// Snapshot **paired with** the covered interval, read together from
    /// the same entry so the pair is mutually consistent (§5.3 read hit):
    /// a reader must serve zeros — never buffer bytes — outside `covered`.
    pub fn covered_snapshot(&self) -> (bytes::Bytes, (u32, u32)) {
        (self.snapshot(), self.covered)
    }

    /// Exclusive mutable view for the write merge. O(1) when unique;
    /// O(block_size) copy into a fresh block when a snapshot is still alive
    /// (copy-on-write, counted in `active_block_cow_copies`).
    pub fn make_mut(&mut self) -> &mut [u8] {
        let (copied, block) = self.cell.owned_mut(|shared| {
            let fresh = AlignedBlock::alloc_raw(shared.len);
            // SAFETY: `fresh.ptr` is a new `shared.len`-byte allocation;
            // `shared` is fully initialized and cannot overlap it.
            unsafe { std::ptr::copy_nonoverlapping(shared.ptr, fresh.ptr, shared.len) };
            fresh
        });
        if copied {
            crate::fuse_client::METRICS
                .active_block_cow_copies
                .fetch_add(1, Ordering::Relaxed);
        }
        block.as_mut_slice()
    }

    /// Borrowed read of the current content (synchronous staging puts).
    /// Sound without refcount traffic: mutation needs `&mut self`, which
    /// cannot coexist with this borrow. Staging callers must
    /// [`ActiveBlockBuf::zero_complete`] first (§5.3).
    pub fn as_slice(&self) -> &[u8] {
        self.cell.peek().as_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed(block_size: usize) -> ActiveBlockBuf {
        let mut buf = ActiveBlockBuf::fresh(block_size);
        buf.zero_complete();
        buf
    }

    #[test]
    fn zero_completed_fresh_is_full_size_and_zero() {
        let buf = zeroed(8192);
        assert_eq!(buf.as_slice().len(), 8192);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
        assert!(buf.is_content_valid());
    }

    #[test]
    fn seeded_copies_prefix_and_zero_extends() {
        let seed = vec![0xAAu8; 100];
        let buf = ActiveBlockBuf::seeded(&seed, 4096);
        assert_eq!(buf.as_slice().len(), 4096);
        assert!(buf.as_slice()[..100].iter().all(|&b| b == 0xAA));
        assert!(buf.as_slice()[100..].iter().all(|&b| b == 0));
        assert!(buf.is_content_valid(), "seeded buffers are born covered");
    }

    #[test]
    fn seeded_truncates_oversized_seed() {
        let seed = vec![0xBBu8; 8192];
        let buf = ActiveBlockBuf::seeded(&seed, 4096);
        assert_eq!(buf.as_slice().len(), 4096);
        assert!(buf.as_slice().iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn make_mut_unique_mutates_in_place() {
        let mut buf = zeroed(4096);
        let before = buf.as_slice().as_ptr();
        buf.make_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            buf.as_slice().as_ptr(),
            before,
            "unique buffer must mutate in place (no copy)"
        );
        assert_eq!(&buf.as_slice()[0..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn live_snapshot_forces_cow_and_stays_immutable() {
        let mut buf = ActiveBlockBuf::seeded(&[7u8; 64], 4096);
        let snap = buf.snapshot();
        let snap_ptr = snap.as_ptr();
        let cow_before = crate::fuse_client::METRICS
            .active_block_cow_copies
            .load(Ordering::Relaxed);

        buf.make_mut()[0..64].copy_from_slice(&[9u8; 64]);

        assert!(
            snap[..64].iter().all(|&b| b == 7),
            "live snapshot mutated by a later write"
        );
        assert!(
            snap[64..].iter().all(|&b| b == 0),
            "seeded snapshot tail must stay zero-extended"
        );
        assert_eq!(snap.as_ptr(), snap_ptr, "snapshot memory must not move");
        assert!(buf.as_slice()[..64].iter().all(|&b| b == 9));
        assert_ne!(
            buf.as_slice().as_ptr(),
            snap.as_ptr(),
            "writer must have copied to fresh memory"
        );
        assert!(
            crate::fuse_client::METRICS
                .active_block_cow_copies
                .load(Ordering::Relaxed)
                > cow_before,
            "CoW must be observable in active_block_cow_copies"
        );
    }

    #[test]
    fn cow_preserves_covered_interval() {
        let mut buf = ActiveBlockBuf::fresh(4096);
        buf.record_write(100, 200);
        buf.make_mut()[100..200].fill(5);
        let snap = buf.snapshot(); // force CoW on the next mutation
        buf.record_write(200, 300);
        buf.make_mut()[200..300].fill(6);
        drop(snap);
        assert_eq!(
            buf.covered(),
            (100, 300),
            "covered interval must survive a CoW payload swap"
        );
    }

    #[test]
    fn dropped_snapshot_restores_in_place_mutation() {
        let mut buf = zeroed(4096);
        drop(buf.snapshot());
        let before = buf.as_slice().as_ptr();
        buf.make_mut()[0] = 1;
        assert_eq!(
            buf.as_slice().as_ptr(),
            before,
            "uniqueness must be restored after the snapshot drops"
        );
    }

    #[test]
    fn snapshot_slice_matches_content() {
        let seed: Vec<u8> = (0..255u8).collect();
        let buf = ActiveBlockBuf::seeded(&seed, 4096);
        let slice = buf.snapshot().slice(10..20);
        assert_eq!(&slice[..], &seed[10..20]);
    }

    #[test]
    fn oversized_block_uses_dedicated_allocation() {
        let len = ALIGNED_BUF_POOL.buf_size() + 4096;
        let mut buf = zeroed(len);
        assert_eq!(buf.as_slice().len(), len);
        assert_eq!(buf.as_slice().as_ptr() as usize % 4096, 0, "4096-aligned");
        buf.make_mut()[len - 1] = 0xCC;
        let snap = buf.snapshot();
        drop(buf);
        assert_eq!(snap[len - 1], 0xCC, "snapshot outlives the owner");
    }

    #[test]
    fn one_shot_full_write_covers_without_zeroing() {
        let mut buf = ActiveBlockBuf::fresh(4096);
        buf.record_write(0, 4096);
        assert!(buf.is_content_valid());
    }
}
