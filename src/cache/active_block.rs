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
//! PR 4 introduced the covered-range bookkeeping (memset elision, §5.3):
//! [`ActiveBlockBuf::fresh`] buffers are born **uncovered** — no seed-time
//! zero-fill. RW3b (the FIND-L1-A fix,
//! `.benchmarks/2026-07-17-rw3-find-l1a-forensics.md`) upgraded it to an
//! **overlap-safe multi-run written-coverage union** and made it the
//! **write-through trigger input**: [`ActiveBlockBuf::record_write`]
//! returns `true` exactly when the union of written ranges reaches the
//! whole block — order-blind, so kernel-split FUSE WRITE segments
//! dispatched concurrently (`FOPEN_PARALLEL_DIRECT_WRITES`) trigger at
//! true completion regardless of arrival order. The representation is a
//! primary run `written` (the only thing an in-order stream ever touches —
//! no allocation) plus a rare `written_extra` overflow of disjoint runs
//! created only by out-of-order arrival (`active_block_ooo_runs`).
//!
//! The §5.3 contract is unchanged: recycled pool bytes never leave the
//! written runs — not to the kernel (the read path serves gap bytes as
//! zeros via [`ActiveBlockBuf::covered_runs_in`]), not to staging or the
//! device ([`ActiveBlockBuf::zero_complete`] zeroes **every** gap under the
//! block lock at every stage/upload exit). All coverage mutations run
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

/// A SHARED handle to a block-sized aligned allocation — the placed-sever
/// assembly backing (shim-parity 2026-07-28): ring WRITE severs copy into
/// it at dequeue, and the first merging handler ADOPTS it as an
/// [`ActiveBlockBuf`] backing ([`ActiveBlockBuf::adopted`]) so the merge
/// copy is elided. Holding a `SharedBlock` (or any clone) makes the
/// adopted cell non-unique — every in-place mutation path CoWs away
/// instead of touching this memory (the destruction-safety that lets
/// in-flight placed payloads keep reading their regions).
#[derive(Clone)]
pub(crate) struct SharedBlock(Arc<AlignedBlock>);

impl SharedBlock {
    /// Allocate a `len`-byte shared block (pool-recycled, 4096-aligned,
    /// initialized memory — same class as every overlay backing).
    pub(crate) fn alloc(len: usize) -> Self {
        Self(Arc::new(AlignedBlock::alloc_raw(len)))
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len
    }

    /// The backing base pointer — the placed-merge pointer-identity proof
    /// input (never dereferenced by callers).
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.0.ptr
    }

    /// Copy `len` bytes from `src` into `[off, off + len)`.
    ///
    /// # Safety
    /// The caller must hold an exclusive [`crate::placed_core`] claim over
    /// the region (no other writer, no reader forms a reference over it:
    /// pre-adoption the block is reachable only through claim-disjoint
    /// [`SharedBlock::region`] views, and adoption is fenced against
    /// in-progress writers by the claims Dekker), `off + len` must be in
    /// bounds, and `src` must be valid for `len` reads (torn CONTENT from
    /// racing client memory is fine; the destination region is exclusive).
    pub(crate) unsafe fn write_at(&self, off: usize, src: *const u8, len: usize) {
        debug_assert!(off + len <= self.0.len);
        std::ptr::copy_nonoverlapping(src, self.0.ptr.add(off), len);
    }

    /// A shared view of `[off, off + len)` — the placed payload's
    /// `AsRef<[u8]>` source. Sound per the claims protocol: the region's
    /// bytes are written exactly once (by its own claim's sever, which
    /// happens-before every reader via the handoff edge) and every
    /// post-adoption mutation path CoWs away while this handle lives.
    pub(crate) fn region(&self, off: usize, len: usize) -> &[u8] {
        assert!(off + len <= self.0.len, "region out of block bounds");
        // SAFETY: bounds asserted; memory initialized (pool birth);
        // exclusivity per the method docs.
        unsafe { std::slice::from_raw_parts(self.0.ptr.add(off), len) }
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

/// W2 compact representation (design-random-small-writes §5.2): payload
/// slabs for the SMALL-write shapes, mirroring the buffer's
/// written-coverage union EXACTLY — `slabs[i]` carries the bytes of the
/// i-th run of [`ActiveBlockBuf::runs_sorted`] (ascending, pairwise
/// disjoint, non-abutting). The coverage union stays the single source of
/// truth (RW3b); the overlay is payload storage only, kept in lockstep by
/// [`ActiveBlockBuf::merge_extent`] and asserted in debug builds. A 4 KiB
/// write parks ~4 KiB (`parked_extent_bytes`), not a block-size backing.
struct ExtentOverlay {
    /// `(start, payload)` slabs — invariant: ranges == the coverage runs.
    slabs: Vec<(u32, Vec<u8>)>,
}

impl ExtentOverlay {
    fn payload_bytes(&self) -> u64 {
        self.slabs.iter().map(|(_, d)| d.len() as u64).sum()
    }

    /// Merge `[start, start+data)` newest-wins, coalescing with
    /// overlapping/abutting slabs — the payload twin of the coverage
    /// union's interval merge.
    fn merge(&mut self, start: u32, data: &[u8]) {
        let end = start + data.len() as u32;
        let lo = self
            .slabs
            .partition_point(|&(s, ref d)| (s + d.len() as u32) < start);
        let mut hi = lo;
        while hi < self.slabs.len() && self.slabs[hi].0 <= end {
            hi += 1;
        }
        if lo == hi {
            self.slabs.insert(lo, (start, data.to_vec()));
            return;
        }
        let new_start = self.slabs[lo].0.min(start);
        let new_end = (self.slabs[hi - 1].0 + self.slabs[hi - 1].1.len() as u32).max(end);
        let mut merged = vec![0u8; (new_end - new_start) as usize];
        for (s, d) in self.slabs.drain(lo..hi) {
            let off = (s - new_start) as usize;
            merged[off..off + d.len()].copy_from_slice(&d);
        }
        // The incoming write is NEWEST: it wins every overlap.
        let off = (start - new_start) as usize;
        merged[off..off + data.len()].copy_from_slice(data);
        self.slabs.insert(lo, (new_start, merged));
    }
}

/// The buffer's physical representation: a full block-sized aligned
/// backing (the historical form — every staging/upload/RMW path) or the
/// W2 compact extent overlay (small-write parking; escalates to `Full`
/// before any whole-image consumer needs it).
enum Repr {
    Full(CowCell<AlignedBlock>),
    Extent(ExtentOverlay),
}

/// A block-sized active-block accumulation buffer.
///
/// Mutation requires provable uniqueness (`Arc::get_mut`); shared
/// snapshots force copy-on-write. Deliberately not `Clone`: the map entry
/// is the exclusive owner, and [`ActiveBlockBuf::snapshot`] is the only
/// sharing primitive.
pub struct ActiveBlockBuf {
    repr: Repr,
    /// Logical block size — representation-independent (the extent form
    /// has no backing to measure).
    block_size: u32,
    /// Primary written run `[written.0, written.1)` — the union of ranges
    /// merged via [`ActiveBlockBuf::record_write`], as long as they arrive
    /// overlapping/abutting (the in-order common case: this pair is the
    /// ONLY coverage state ever touched, no allocation). RW3b: written
    /// coverage IS the write-through trigger input — `record_write`
    /// returns `true` when the union reaches the whole block.
    written: (u32, u32),
    /// Out-of-order overflow: additional written runs, sorted by start,
    /// pairwise disjoint and non-abutting, each disjoint and non-abutting
    /// from `written`. Empty — never allocated — on in-order streams;
    /// populated only when a write lands disjoint from every existing run
    /// (kernel-split segment reorder; counted `active_block_ooo_runs`).
    written_extra: Vec<(u32, u32)>,
    /// Every byte of the buffer is the block's correct current content:
    /// born-`seeded`, written union spans the block, complement zeroed
    /// ([`ActiveBlockBuf::zero_complete`]) or seed-filled
    /// ([`ActiveBlockBuf::fill_complement_from`]). Distinct from written
    /// coverage: a Seeded buffer is content-valid with an empty written
    /// union (a lone partial overwrite of it parks — it must never
    /// re-trigger per merged range).
    content_valid: bool,
    /// Item B (the overwrite lazy-RMW seed): the unwritten complement's
    /// correct content is the block's OLD DEVICE BYTES, not zeros — the
    /// seed read was deferred at checkout. Cleared when written coverage
    /// reaches full (the old bytes are wholly overwritten — the row-4 win:
    /// no read at all) or when
    /// [`ActiveBlockBuf::fill_complement_from`] materializes the seed
    /// (stage/upload exits / sparse read). While set,
    /// [`ActiveBlockBuf::zero_complete`] is FORBIDDEN — zeroing would
    /// codify zeros over acked old bytes. Invariant: `deferred_seed ⇒
    /// !content_valid` (every full-coverage / fill transition clears it),
    /// which is exactly the RW3b covered flush-seed elision — flush/spill
    /// exits gate their seed fetch on `seed_deferred()`, so a fully
    /// covered buffer can never pay one.
    deferred_seed: bool,
}

/// RAII gauge charge: `parked_full_buffer_bytes` for `Full`,
/// `parked_extent_bytes` for `Extent` slab bytes — adjusted at
/// construction, drop, escalation, and slab growth. Together these are the
/// W2 parked-write BYTE budget (the retired 256-count cap's byte form).
fn gauge_full() -> &'static std::sync::atomic::AtomicU64 {
    &crate::fuse_client::METRICS.parked_full_buffer_bytes
}

fn gauge_extent() -> &'static std::sync::atomic::AtomicU64 {
    &crate::fuse_client::METRICS.parked_extent_bytes
}

impl Drop for ActiveBlockBuf {
    fn drop(&mut self) {
        match &self.repr {
            Repr::Full(_) => {
                crate::gauge_core::sub_saturating(gauge_full(), self.block_size as u64);
            }
            Repr::Extent(ov) => {
                crate::gauge_core::sub_saturating(gauge_extent(), ov.payload_bytes());
            }
        }
    }
}

impl ActiveBlockBuf {
    fn new_full(block_size: usize, deferred_seed: bool) -> Self {
        gauge_full().fetch_add(block_size as u64, Ordering::Relaxed);
        Self {
            repr: Repr::Full(CowCell::new(AlignedBlock::alloc_raw(block_size))),
            block_size: block_size as u32,
            written: (0, 0),
            written_extra: Vec::new(),
            content_valid: false,
            deferred_seed,
        }
    }

    /// A fresh accumulation buffer for a block with **no existing data**:
    /// born unwritten, with the seed-time zero-fill elided (§5.3). The
    /// complement's correct content is zeros by definition — established
    /// lazily by [`ActiveBlockBuf::zero_complete`] (stage/upload exits) and
    /// served as zeros by the coverage-aware read
    /// ([`ActiveBlockBuf::covered_runs_in`]) meanwhile.
    pub fn fresh(block_size: usize) -> Self {
        Self::new_full(block_size, false)
    }

    /// A deferred-RMW accumulation buffer for a block WITH existing device
    /// data whose seed read is postponed (item B): born unwritten, the
    /// complement's correct content is the old block. If accumulation
    /// fully covers the block before any exit — in ANY segment order
    /// (RW3b) — the seed read never happens (the sequential-overwrite fast
    /// path); otherwise the owner materializes via
    /// [`ActiveBlockBuf::fill_complement_from`] before the bytes can
    /// escape.
    pub fn deferred(block_size: usize) -> Self {
        Self::new_full(block_size, true)
    }

    /// ADOPT a placed-sever assembly as this block's backing (shim-parity
    /// 2026-07-28): identical semantics to [`ActiveBlockBuf::fresh`]
    /// (`deferred == false`) / [`ActiveBlockBuf::deferred`] (`true`) —
    /// born unwritten, coverage machinery untouched — except the backing
    /// is the SHARED assembly the ring severs already landed in, so the
    /// adopting merge (and every sibling-chunk merge that passes the
    /// pointer proof) records coverage without copying. While any foreign
    /// `SharedBlock`/placed-payload handle lives, the cell is non-unique:
    /// [`ActiveBlockBuf::make_mut`] and every content-establishing exit
    /// ([`ActiveBlockBuf::zero_complete`] /
    /// [`ActiveBlockBuf::fill_complement_from`]) copy first — in-flight
    /// placed payloads can never have their severed bytes destroyed.
    pub(crate) fn adopted(shared: &SharedBlock, deferred: bool) -> Self {
        gauge_full().fetch_add(shared.len() as u64, Ordering::Relaxed);
        Self {
            repr: Repr::Full(CowCell::adopt(Arc::clone(&shared.0))),
            block_size: shared.len() as u32,
            written: (0, 0),
            written_extra: Vec::new(),
            content_valid: false,
            deferred_seed: deferred,
        }
    }

    /// The current Full-repr backing pointer at `rel` when `[rel,
    /// rel + len)` is in bounds — the placed-merge pointer-identity probe
    /// (`None` on the extent repr). Equality with a payload slice pointer
    /// PROVES the payload region IS this backing region at this offset:
    /// only a placed sever can produce a payload inside an overlay
    /// backing, and any mutation since adoption either happened in place
    /// (impossible while the placed payload's shared handle keeps the
    /// cell non-unique) or CoW'd the backing (pointer changed — the probe
    /// fails and the caller copies).
    pub(crate) fn full_ptr_at(&self, rel: usize, len: usize) -> Option<*const u8> {
        match &self.repr {
            Repr::Full(cell) if rel + len <= self.block_size as usize => {
                // SAFETY: in-bounds pointer arithmetic on the backing
                // allocation; the pointer is compared, never dereferenced.
                Some(unsafe { cell.peek().ptr.add(rel) })
            }
            _ => None,
        }
    }

    /// W2 (§5.2): a COMPACT extent-overlay buffer for the small-write
    /// shapes — no block-size backing is allocated; payload slabs charge
    /// `parked_extent_bytes`. `deferred` carries the item-B law verbatim:
    /// `true` = the block has existing device data and the unwritten
    /// complement owes its OLD bytes (a fold seeds once; reads compose the
    /// complement from the base tiers); `false` = hole/no-existing-data —
    /// the complement is zeros and a fold performs NO seed read.
    pub fn extent(block_size: usize, deferred: bool) -> Self {
        Self {
            repr: Repr::Extent(ExtentOverlay { slabs: Vec::new() }),
            block_size: block_size as u32,
            written: (0, 0),
            written_extra: Vec::new(),
            content_valid: false,
            deferred_seed: deferred,
        }
    }

    /// Whether this buffer is the W2 compact extent-overlay form.
    pub fn is_extent_repr(&self) -> bool {
        matches!(self.repr, Repr::Extent(_))
    }

    /// Whether the unwritten complement still owes the old-block seed.
    pub fn seed_deferred(&self) -> bool {
        self.deferred_seed
    }

    /// Extent-repr accessors (0/empty on the full repr).
    pub fn extent_count(&self) -> usize {
        self.written_extra.len() + usize::from(self.written.0 != self.written.1)
    }

    /// Parked extent payload bytes (0 for the full repr).
    pub fn extent_payload_bytes(&self) -> u64 {
        match &self.repr {
            Repr::Extent(ov) => ov.payload_bytes(),
            Repr::Full(_) => 0,
        }
    }

    /// Merge a small write into the extent overlay: coverage first (the
    /// RW3b union — the single trigger source), then the payload slab
    /// merge (newest wins). Returns the coverage-completion transition
    /// exactly as [`ActiveBlockBuf::record_write`] does. Callers hold this
    /// block's `BLOCK_FLUSH_LOCKS`.
    pub fn merge_extent(&mut self, start: usize, data: &[u8]) -> bool {
        debug_assert!(self.is_extent_repr(), "merge_extent on a full buffer");
        if data.is_empty() {
            return false;
        }
        let completed = self.record_write(start, start + data.len());
        let Repr::Extent(ov) = &mut self.repr else {
            unreachable!("checked extent repr above");
        };
        let before = ov.payload_bytes();
        ov.merge(start as u32, data);
        let after = ov.payload_bytes();
        if after >= before {
            gauge_extent().fetch_add(after - before, Ordering::Relaxed);
        } else {
            crate::gauge_core::sub_saturating(gauge_extent(), before - after);
        }
        #[cfg(debug_assertions)]
        self.assert_slabs_mirror_runs();
        completed
    }

    /// Apply an OLDER extent (a staged record being absorbed) — only into
    /// the coverage GAPS, so newer parked bytes are never overwritten.
    /// Works on both representations; callers hold the block lock.
    pub fn absorb_older_extent(&mut self, start: usize, data: &[u8]) {
        let end = start + data.len();
        let subs: Vec<(u32, u32)> = self
            .gaps(self.block_size)
            .into_iter()
            .filter_map(|(gs, ge)| {
                let s = gs.max(start as u32);
                let e = ge.min(end as u32);
                (s < e).then_some((s, e))
            })
            .collect();
        for (s, e) in subs {
            let slice = &data[(s as usize - start)..(e as usize - start)];
            match &mut self.repr {
                Repr::Extent(_) => {
                    let _ = self.merge_extent(s as usize, slice);
                }
                Repr::Full(_) => {
                    let _ = self.record_write(s as usize, e as usize);
                    self.make_mut()[s as usize..e as usize].copy_from_slice(slice);
                }
            }
        }
    }

    /// The extent runs intersected with `[start, end)` as `(abs_start,
    /// payload_copy)` — the read-compose input (bounded copies; extents
    /// are small by construction). Content beyond the runs is the
    /// caller's complement (zeros for a fresh overlay, the base
    /// tiers/device for a deferred one).
    pub fn extent_runs_in(&self, start: usize, end: usize) -> Vec<(usize, Vec<u8>)> {
        match &self.repr {
            Repr::Extent(ov) => ov
                .slabs
                .iter()
                .filter(|&&(s, ref d)| (s as usize) < end && s as usize + d.len() > start)
                .map(|&(s, ref d)| {
                    let lo = start.max(s as usize);
                    let hi = end.min(s as usize + d.len());
                    (lo, d[lo - s as usize..hi - s as usize].to_vec())
                })
                .collect(),
            Repr::Full(_) => Vec::new(),
        }
    }

    /// Every extent run as `(start, payload_copy)` — the spill serializer
    /// input (mirrors the coverage runs exactly).
    pub fn extent_table(&self) -> Vec<(u32, Vec<u8>)> {
        match &self.repr {
            Repr::Extent(ov) => ov.slabs.clone(),
            Repr::Full(_) => Vec::new(),
        }
    }

    /// Escalate the extent overlay to a full buffer (coverage ≥ 25 % of
    /// the block or a large merge, §5.2): allocate the block-size backing,
    /// lay the slabs at their offsets, keep the coverage union and the
    /// item-B deferral verbatim. Pure RAM conversion — no I/O, no
    /// coverage change. Idempotent on a full buffer.
    pub fn escalate_to_full(&mut self) {
        let Repr::Extent(ov) = &mut self.repr else {
            return;
        };
        let slabs = std::mem::take(&mut ov.slabs);
        let freed: u64 = slabs.iter().map(|(_, d)| d.len() as u64).sum();
        gauge_full().fetch_add(self.block_size as u64, Ordering::Relaxed);
        crate::gauge_core::sub_saturating(gauge_extent(), freed);
        let block = AlignedBlock::alloc_raw(self.block_size as usize);
        self.repr = Repr::Full(CowCell::new(block));
        let slice = self.make_mut();
        for (s, d) in slabs {
            slice[s as usize..s as usize + d.len()].copy_from_slice(&d);
        }
        crate::fuse_client::METRICS
            .extent_escalations
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Defensive escalation for legacy full-image call sites reaching an
    /// extent buffer through a route the W2 wiring did not special-case:
    /// stays CORRECT (the full form serves everything) and observable
    /// (`extent_implicit_escalations` — designed routes keep it at 0).
    fn implicit_escalate(&mut self) {
        if self.is_extent_repr() {
            self.escalate_to_full();
            crate::fuse_client::METRICS
                .extent_implicit_escalations
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(debug_assertions)]
    fn assert_slabs_mirror_runs(&self) {
        let Repr::Extent(ov) = &self.repr else {
            return;
        };
        let runs: Vec<(u32, u32)> = self.runs_sorted().collect();
        let slabs: Vec<(u32, u32)> = ov
            .slabs
            .iter()
            .map(|&(s, ref d)| (s, s + d.len() as u32))
            .collect();
        assert_eq!(
            runs, slabs,
            "extent slabs must mirror the coverage union exactly"
        );
    }

    fn full_cell(&self) -> &CowCell<AlignedBlock> {
        match &self.repr {
            Repr::Full(cell) => cell,
            Repr::Extent(_) => panic!(
                "whole-image access to an extent-overlay buffer: the caller \
                 must fold or escalate first (W2 route bug)"
            ),
        }
    }

    /// Materialize the deferred seed: fill every unwritten gap from `old`
    /// (the block's current device/durable content; shorter-than-block
    /// seeds zero-fill their own tail, matching
    /// [`ActiveBlockBuf::seeded`] semantics) and become content-valid.
    /// Callers hold this block's `BLOCK_FLUSH_LOCKS`. Idempotent-safe: a
    /// no-op when the seed is no longer deferred.
    ///
    /// Deliberately does NOT claim written coverage: a fill can happen
    /// mid-accumulation (a sparse read materializing a parked buffer), and
    /// the stream must still be able to complete the written union and
    /// fire the write-through trigger afterwards.
    pub fn fill_complement_from(&mut self, old: &[u8]) {
        if !self.deferred_seed {
            return;
        }
        // A seed application is a whole-image operation: an extent overlay
        // reaching here (a route the W2 wiring should fold instead)
        // escalates first — correct, observable.
        self.implicit_escalate();
        self.deferred_seed = false;
        if self.content_valid {
            return;
        }
        let len = self.block_size as usize;
        let gaps = self.gaps(len as u32);
        let slice = self.make_mut();
        for &(gs, ge) in &gaps {
            let (gs, ge) = (gs as usize, ge as usize);
            // Old bytes where the seed reaches, zero-fill past its length.
            let src_end = old.len().clamp(gs, ge);
            if src_end > gs {
                slice[gs..src_end].copy_from_slice(&old[gs..src_end]);
            }
            slice[src_end..ge].fill(0);
        }
        self.content_valid = true;
        crate::fuse_client::METRICS
            .overwrite_seed_materialized
            .fetch_add(1, Ordering::Relaxed);
    }

    /// A block seeded from existing content (RMW / staged / promotion
    /// seeds): copies `min(existing.len(), block_size)` bytes and
    /// zero-fills the remainder, so the buffer is born content-valid at
    /// full block size regardless of the seed's length. Its WRITTEN
    /// coverage starts empty — the write-through trigger still requires a
    /// covering stream (a lone partial overwrite parks).
    pub fn seeded(existing: &[u8], block_size: usize) -> Self {
        // (deferred_seed: false — a seeded buffer is content-valid at birth.)
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
        gauge_full().fetch_add(block_size as u64, Ordering::Relaxed);
        Self {
            repr: Repr::Full(CowCell::new(block)),
            block_size: block_size as u32,
            written: (0, 0),
            written_extra: Vec::new(),
            content_valid: true,
            deferred_seed: false,
        }
    }

    /// The primary covered run `[start, end)` — `(0, block_size)` once
    /// content-valid. Multi-run queries go through
    /// [`ActiveBlockBuf::covered_contains`] /
    /// [`ActiveBlockBuf::covered_runs_in`].
    pub fn covered(&self) -> (u32, u32) {
        if self.content_valid {
            (0, self.block_size)
        } else {
            self.written
        }
    }

    /// Content-valid: every byte is the block's correct current content
    /// (Seeded buffers always; Fresh/deferred buffers once the written
    /// union spans the block or the complement was zeroed / seed-filled).
    pub fn is_content_valid(&self) -> bool {
        self.content_valid
    }

    /// Whether `[start, end)` lies entirely within correct-content bytes:
    /// the whole buffer once content-valid, else a single written run.
    /// Reads inside it may serve buffer bytes verbatim.
    pub fn covered_contains(&self, start: usize, end: usize) -> bool {
        if self.content_valid {
            return true;
        }
        let (s, e) = (start as u32, end as u32);
        let inside = |run: (u32, u32)| run.0 <= s && e <= run.1;
        inside(self.written) || self.written_extra.iter().any(|&run| inside(run))
    }

    /// The written runs intersected with `[start, end)`, ascending — the
    /// sparse-read compose input (buffer bytes inside the runs, zeros in
    /// the gaps of a Fresh buffer). Callers on the deferred path
    /// materialize instead (gaps owe OLD bytes, item B). Content-valid
    /// buffers report the whole range.
    pub fn covered_runs_in(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        if self.content_valid {
            return vec![(start, end)];
        }
        let (s, e) = (start as u32, end as u32);
        self.runs_sorted()
            .filter(|&(rs, re)| rs < e && re > s)
            .map(|(rs, re)| (rs.max(s) as usize, re.min(e) as usize))
            .collect()
    }

    /// All written runs in ascending offset order (primary merged into the
    /// sorted extras view). Runs are pairwise disjoint and non-abutting.
    fn runs_sorted(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let p = self.written;
        let idx = self.written_extra.partition_point(|&(s, _)| s < p.0);
        let (before, after) = self.written_extra.split_at(idx);
        before
            .iter()
            .copied()
            .chain((p.0 != p.1).then_some(p))
            .chain(after.iter().copied())
    }

    /// The unwritten gaps of `[0, len)` in ascending order.
    fn gaps(&self, len: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut cursor = 0u32;
        for (s, e) in self.runs_sorted() {
            if s > cursor {
                out.push((cursor, s));
            }
            cursor = e;
        }
        if cursor < len {
            out.push((cursor, len));
        }
        out
    }

    /// Record a write of `[start, end)` **before** merging it via
    /// [`ActiveBlockBuf::make_mut`], and return `true` exactly when this
    /// write completed the written union to the whole block — **the RW3b
    /// write-through trigger** (overlap-safe, order-blind; fires once per
    /// covering stream, never because a segment's end coincides with the
    /// block end). Overlapping/abutting writes extend the primary run (the
    /// in-order fast path); a disjoint write records an out-of-order run
    /// (`active_block_ooo_runs`) that later writes coalesce with. Callers
    /// hold this block's `BLOCK_FLUSH_LOCKS` (as all mutations do).
    pub fn record_write(&mut self, start: usize, end: usize) -> bool {
        let len = self.block_size as usize;
        debug_assert!(
            start <= end && end <= len,
            "write range out of block bounds"
        );
        if start == end || self.union_is_full() {
            // Degenerate / already-complete: no transition to report (a
            // re-write of a completed union must not double-fire).
            return false;
        }
        let (s, e) = (start as u32, end as u32);
        let (p0, p1) = self.written;
        if p0 == p1 {
            // First touch.
            self.written = (s, e);
        } else if s <= p1 && e >= p0 {
            // Overlaps or abuts the primary run: extend it, then absorb any
            // extras the grown primary now reaches (a bridging write can
            // connect runs on both sides).
            self.written = (p0.min(s), p1.max(e));
            self.coalesce_extras_into_primary();
        } else {
            // Disjoint from the primary: an out-of-order run. Insert into
            // the sorted extras, coalescing with overlapping/abutting
            // neighbours (extras stay disjoint from the primary by
            // construction: a run reaching the primary is caught above).
            self.insert_extra_run(s, e);
            crate::fuse_client::METRICS
                .active_block_ooo_runs
                .fetch_add(1, Ordering::Relaxed);
        }
        if self.union_is_full() {
            self.complete_written_union();
            return true;
        }
        false
    }

    fn union_is_full(&self) -> bool {
        // Extras are disjoint from the primary, so a full primary implies
        // no extras.
        self.written == (0, self.block_size)
    }

    /// Every byte is APP-WRITTEN (the accumulated coverage union spans the
    /// block) — the write-through class. Strictly stronger than
    /// [`ActiveBlockBuf::is_content_valid`]: seeded / zero-completed /
    /// seed-filled custody is content-valid without being app-complete,
    /// and belongs to the staged-writeback ladder, never the write-through
    /// legs (2026-07-27 write-pipeline campaign — the flush write-through
    /// leg's admission predicate).
    pub fn is_union_complete(&self) -> bool {
        !self.is_extent_repr() && self.union_is_full()
    }

    fn coalesce_extras_into_primary(&mut self) {
        let (mut p0, mut p1) = self.written;
        self.written_extra.retain(|&(s, e)| {
            if s <= p1 && e >= p0 {
                p0 = p0.min(s);
                p1 = p1.max(e);
                false
            } else {
                true
            }
        });
        // One retain pass suffices: extras are pairwise non-abutting, so a
        // grown primary can absorb each at most once, and absorbing one
        // cannot make a previously-disjoint one reachable (any run between
        // them would have been coalesced with it already).
        self.written = (p0, p1);
    }

    fn insert_extra_run(&mut self, s: u32, e: u32) {
        let idx = self.written_extra.partition_point(|&(_, re)| re < s);
        let mut end_idx = idx;
        let (mut ns, mut ne) = (s, e);
        while end_idx < self.written_extra.len() && self.written_extra[end_idx].0 <= ne {
            ns = ns.min(self.written_extra[end_idx].0);
            ne = ne.max(self.written_extra[end_idx].1);
            end_idx += 1;
        }
        self.written_extra.splice(idx..end_idx, [(ns, ne)]);
    }

    /// Establish content-validity at a stage/upload exit: zero **every**
    /// unwritten gap (elided when the buffer is already content-valid).
    /// Idempotent. Callers hold this block's `BLOCK_FLUSH_LOCKS`.
    pub fn zero_complete(&mut self) {
        debug_assert!(
            !self.deferred_seed,
            "zero_complete on a deferred-seed buffer: the owner must \
             materialize the old-block seed first (item B exit contract)"
        );
        // Whole-image exit reached with an extent overlay: escalate first
        // (correct + counted; designed routes fold instead).
        self.implicit_escalate();
        if self.content_valid {
            return;
        }
        let len = self.block_size as usize;
        let gaps = self.gaps(len as u32);
        let zeroed: usize = gaps.iter().map(|&(s, e)| (e - s) as usize).sum();
        if zeroed > 0 {
            let slice = self.make_mut();
            for &(gs, ge) in &gaps {
                slice[gs as usize..ge as usize].fill(0);
            }
        }
        // The gaps are zeros now — the exit owns the buffer's remaining
        // life, so claiming the full union keeps `covered()` reporting
        // `(0, len)` exactly as the pre-RW3b degrade did.
        self.written = (0, len as u32);
        self.written_extra.clear();
        self.content_valid = true;
        crate::fuse_client::METRICS
            .active_block_memset_elided_bytes
            .fetch_add((len - zeroed) as u64, Ordering::Relaxed);
    }

    /// The written union reached the whole block (the trigger transition):
    /// every byte is app-written, so the buffer is content-valid with zero
    /// memset; a deferred seed is skipped forever (the row-4 win).
    fn complete_written_union(&mut self) {
        let len = self.block_size as usize;
        debug_assert!(self.written_extra.is_empty());
        if self.deferred_seed {
            self.deferred_seed = false;
            crate::fuse_client::METRICS
                .overwrite_seed_skipped
                .fetch_add(1, Ordering::Relaxed);
        }
        if !self.content_valid {
            self.content_valid = true;
            crate::fuse_client::METRICS
                .active_block_memset_elided_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
        }
    }

    /// Zero-copy immutable snapshot for readers (read-your-own-writes) and
    /// for staging/upload. The snapshot is immutable forever: any later
    /// writer that finds it alive copies first (CoW). Staging/upload
    /// callers must [`ActiveBlockBuf::zero_complete`] first (§5.3);
    /// coverage-aware readers pair it with
    /// [`ActiveBlockBuf::covered_contains`] /
    /// [`ActiveBlockBuf::covered_runs_in`] read under the same entry guard
    /// so snapshot and coverage are mutually consistent — a reader must
    /// serve zeros, never buffer bytes, in the gaps of a Fresh buffer.
    pub fn snapshot(&self) -> bytes::Bytes {
        bytes::Bytes::from_owner(SnapshotOwner(self.full_cell().share()))
    }

    /// Exclusive mutable view for the write merge. O(1) when unique;
    /// O(block_size) copy into a fresh block when a snapshot is still alive
    /// (copy-on-write, counted in `active_block_cow_copies`).
    pub fn make_mut(&mut self) -> &mut [u8] {
        self.implicit_escalate();
        let Repr::Full(cell) = &mut self.repr else {
            unreachable!("implicit_escalate leaves a full repr");
        };
        let (copied, block) = cell.owned_mut(|shared| {
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
        self.full_cell().peek().as_slice()
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
        assert!(
            buf.record_write(0, 4096),
            "a one-shot full write completes the union (the trigger)"
        );
        assert!(buf.is_content_valid());
    }

    /// RW3b: out-of-order disjoint runs coalesce exactly; the trigger fires
    /// once, at the write that completes the union — including a bridge
    /// that connects extras on BOTH sides of the primary run.
    #[test]
    fn ooo_runs_coalesce_and_trigger_fires_once_at_completion() {
        let ooo_before = crate::fuse_client::METRICS
            .active_block_ooo_runs
            .load(Ordering::Relaxed);
        let mut buf = ActiveBlockBuf::fresh(4096);
        assert!(!buf.record_write(1024, 2048)); // primary [1024,2048)
        assert!(!buf.record_write(3072, 4096)); // extra after
        assert!(!buf.record_write(0, 512)); // extra before
        assert!(
            crate::fuse_client::METRICS
                .active_block_ooo_runs
                .load(Ordering::Relaxed)
                >= ooo_before + 2,
            "disjoint out-of-order runs must be observable"
        );
        assert!(buf.covered_contains(3072, 4096));
        assert!(buf.covered_contains(0, 512));
        assert!(!buf.covered_contains(0, 1024), "gap [512,1024) uncovered");
        assert!(!buf.is_content_valid());
        // Bridge [512,1024) + primary + [2048,3072): connects all runs but
        // the union still spans [0,4096) only after this one write.
        assert!(
            buf.record_write(512, 3072),
            "the bridging write completes the union exactly once"
        );
        assert!(buf.is_content_valid());
        assert!(!buf.record_write(0, 4096), "no double-fire after complete");
    }

    /// Seeded buffers are content-valid but their WRITTEN union starts
    /// empty: partial writes never report completion; a covering stream
    /// completes exactly once.
    #[test]
    fn seeded_written_union_tracks_independently_of_content_validity() {
        let mut buf = ActiveBlockBuf::seeded(&[7u8; 4096], 4096);
        assert!(buf.is_content_valid());
        assert!(
            !buf.record_write(2048, 4096),
            "a partial overwrite of a seeded buffer must not report complete"
        );
        assert!(buf.is_content_valid(), "still content-valid meanwhile");
        assert!(
            buf.record_write(0, 2048),
            "the covering stream completes the union once"
        );
    }

    /// Multi-gap zero_complete: every gap — head, interior, tail — zeroes;
    /// written runs stay byte-exact.
    #[test]
    fn zero_complete_zeroes_every_gap() {
        let mut buf = ActiveBlockBuf::fresh(4096);
        buf.make_mut().fill(0xEE); // recycled garbage
        buf.record_write(512, 1024);
        buf.make_mut()[512..1024].fill(1);
        buf.record_write(2048, 2560);
        buf.make_mut()[2048..2560].fill(2);
        buf.zero_complete();
        assert!(buf.is_content_valid());
        let s = buf.as_slice();
        assert!(s[..512].iter().all(|&x| x == 0), "head gap");
        assert!(s[512..1024].iter().all(|&x| x == 1));
        assert!(s[1024..2048].iter().all(|&x| x == 0), "interior gap");
        assert!(s[2048..2560].iter().all(|&x| x == 2));
        assert!(s[2560..].iter().all(|&x| x == 0), "tail gap");
        assert_eq!(buf.covered(), (0, 4096));
    }

    /// Multi-gap fill_complement_from: gaps take OLD bytes (zero-filled
    /// past the seed's length); written runs stay byte-exact; written
    /// coverage is NOT claimed (a later covering stream must still be able
    /// to fire the trigger).
    #[test]
    fn fill_complement_fills_every_gap_with_old_bytes() {
        let mut buf = ActiveBlockBuf::deferred(4096);
        buf.record_write(512, 1024);
        buf.make_mut()[512..1024].fill(1);
        buf.record_write(2048, 2560);
        buf.make_mut()[2048..2560].fill(2);
        assert!(buf.seed_deferred());
        let old = vec![9u8; 2304]; // shorter than the block
        buf.fill_complement_from(&old);
        assert!(!buf.seed_deferred());
        assert!(buf.is_content_valid());
        let s = buf.as_slice();
        assert!(s[..512].iter().all(|&x| x == 9), "head gap = old bytes");
        assert!(s[512..1024].iter().all(|&x| x == 1));
        assert!(s[1024..2048].iter().all(|&x| x == 9), "interior gap = old");
        assert!(s[2048..2560].iter().all(|&x| x == 2));
        assert!(
            s[2560..].iter().all(|&x| x == 0),
            "past the old block's length = zeros"
        );
        // Written union untouched by the fill: completing it still fires.
        assert!(
            !buf.record_write(0, 512),
            "union [0,1024)∪[2048,2560) partial"
        );
        assert!(
            buf.record_write(1024, 4096),
            "the covering stream still fires the trigger after a mid-window fill"
        );
    }

    /// Runs intersected with a read range (the sparse-read compose input).
    #[test]
    fn covered_runs_in_intersects_exactly() {
        let mut buf = ActiveBlockBuf::fresh(4096);
        buf.record_write(512, 1024);
        buf.record_write(2048, 2560);
        assert_eq!(
            buf.covered_runs_in(0, 4096),
            vec![(512, 1024), (2048, 2560)]
        );
        assert_eq!(
            buf.covered_runs_in(600, 2100),
            vec![(600, 1024), (2048, 2100)]
        );
        assert_eq!(buf.covered_runs_in(1024, 2048), vec![]);
        buf.zero_complete();
        assert_eq!(buf.covered_runs_in(100, 200), vec![(100, 200)]);
    }
}
