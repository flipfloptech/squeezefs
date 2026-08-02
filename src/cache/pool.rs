//! Pooled I/O buffers with a **contractual** 4 KiB alignment guarantee
//! (zero-copy write-path design §5.6, PR 2).
//!
//! Both process-wide pools ([`BUFFER_POOL`] and [`ALIGNED_BUF_POOL`]) back
//! every buffer with a `Layout::from_size_align(_, POOLED_BUF_ALIGN)`
//! allocation, so a pooled full-block payload satisfies
//! `nvme_dev::write_block`'s zero-copy `WriteData::Aligned` DMA branch by
//! construction instead of by allocator luck (audit #11 — the previous
//! `Vec<u8>` backing was page-aligned only under jemalloc's incidental
//! large-allocation behavior, and never in test/bench builds). Writes that
//! still miss the branch are counted in the `nvme_unaligned_write_fallbacks`
//! stat; it must stay 0 for pooled sources.
//!
//! [`PooledBuf`]s are handed out **logically empty** (`len() == 0`) and
//! [`PooledBuf::resize`] fills every newly exposed byte, so recycled pool
//! memory can never leak a previous user's content through hole reads or
//! short-read tails (the old `Vec` backing preserved stale content across
//! recycling).

use crossbeam::queue::ArrayQueue;
use once_cell::sync::Lazy;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// Contractual alignment (bytes) of every pooled buffer handed out by
/// [`BufferPool`] and [`AlignedBufPool`] — the pointer-alignment requirement
/// of `nvme_dev::write_block`'s zero-copy `WriteData::Aligned` DMA branch.
pub const POOLED_BUF_ALIGN: usize = 4096;

fn pooled_layout(size: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(size, POOLED_BUF_ALIGN)
        .expect("pooled buffer layout (size, 4 KiB align)")
}

/// Allocate `size` **zeroed** bytes at [`POOLED_BUF_ALIGN`].
///
/// Zeroed at birth (one-time cost per OS allocation, amortized to nothing
/// across recycling) so every pooled byte is always initialized memory:
/// the memset-elided `ActiveBlockBuf::fresh` path (zero-copy write-path
/// design §5.3) forms `&[u8]` views over not-yet-written pool bytes, which
/// is only sound when the backing is initialized. Recycled buffers keep
/// whatever prior content safe writes left — the §5.3 covered-interval
/// contract is what keeps those bytes from ever *escaping*.
fn alloc_pooled(size: usize) -> *mut u8 {
    // SAFETY: `pooled_layout` never has zero size for pool buffers
    // (`BufferPool::new` / `AlignedBufPool::new` take non-zero sizes and
    // grow targets are > 0).
    let ptr = unsafe { std::alloc::alloc_zeroed(pooled_layout(size)) };
    assert!(!ptr.is_null(), "pooled buffer allocation failed");
    debug_assert_eq!(
        ptr as usize % POOLED_BUF_ALIGN,
        0,
        "allocator violated the pooled-buffer alignment contract"
    );
    ptr
}

/// Free a pointer previously produced by [`alloc_pooled`] with the same
/// `size`.
///
/// SAFETY (caller): `ptr` must come from `alloc_pooled(size)` and must not
/// be used afterwards.
unsafe fn dealloc_pooled(ptr: *mut u8, size: usize) {
    std::alloc::dealloc(ptr, pooled_layout(size));
}

/// Recycling pool of [`POOLED_BUF_ALIGN`]-aligned, `buf_size`-byte backings
/// for [`PooledBuf`] (striped RMW / read-assembly buffers and the striped
/// router write source via [`PooledBuf::into_bytes`]).
pub struct BufferPool {
    queue: ArrayQueue<*mut u8>,
    buf_size: usize,
}

// SAFETY: the queue holds uniquely-owned allocations (no aliases exist while
// a pointer sits in the pool); handing one out transfers ownership to a
// single `PooledBuf`. All queue operations are lock-free and thread-safe.
unsafe impl Send for BufferPool {}
unsafe impl Sync for BufferPool {}

impl BufferPool {
    pub fn new(capacity: usize, buf_size: usize) -> Self {
        let queue = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let _ = queue.push(alloc_pooled(buf_size));
        }
        Self { queue, buf_size }
    }

    /// R5 gauge: bytes of IDLE (queued) pool backings. Handed-out
    /// backings are deliberately excluded — their bytes are owned and
    /// gauged by their consumers (parked buffers, in-flight reads), and
    /// counting them here double-billed the pressure signal (measured on
    /// the row-5 trace: `aligned_buf_pool` tracked `parked_write_buffers`
    /// 1:1, inflating pressure ~2x).
    pub fn allocated_bytes(&self) -> u64 {
        self.queue.len() as u64 * self.buf_size as u64
    }

    /// R5 Red trim (§5.7): free QUEUED (idle) backings until the pool's
    /// idle bytes are ≤ `target` — handed-out buffers are untouched (they
    /// recycle or free later through the same accounting).
    pub fn trim_to(&self, target: u64) {
        while self.allocated_bytes() > target {
            let Some(ptr) = self.queue.pop() else { break };
            // SAFETY: every queued pointer came from `alloc_pooled(self.buf_size)`.
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
    }

    /// Take a buffer from the pool (or allocate a fresh aligned one when the
    /// pool is empty). The handout is logically empty — call
    /// [`PooledBuf::resize`] to expose initialized bytes.
    pub fn alloc(self: &Arc<Self>) -> PooledBuf {
        let ptr = self
            .queue
            .pop()
            .unwrap_or_else(|| alloc_pooled(self.buf_size));
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "pooled handout violates the alignment contract"
        );
        PooledBuf {
            ptr,
            cap: self.buf_size,
            len: 0,
            pool: self.clone(),
        }
    }

    /// Return a pool-sized backing; frees it when the pool is full.
    fn recycle_ptr(&self, ptr: *mut u8) {
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "recycled pointer violates the alignment contract"
        );
        if self.queue.push(ptr).is_err() {
            // Pool full — free the over-capacity buffer.
            // SAFETY: `ptr` was produced by `alloc_pooled(self.buf_size)`
            // (only pool-sized backings reach `recycle_ptr`).
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Capacity (bytes) of every pooled backing this pool hands out —
    /// callers' fit check before drawing scratch (crypto scratch §5.7
    /// bounces to the heap when a worst case exceeds it).
    pub fn buf_size(&self) -> usize {
        self.buf_size
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        // Outstanding `PooledBuf`s hold an `Arc<BufferPool>`, so by the time
        // this runs the queue owns every remaining backing.
        while let Some(ptr) = self.queue.pop() {
            // SAFETY: every queued pointer came from
            // `alloc_pooled(self.buf_size)`.
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
    }
}

/// An exclusively-owned, [`POOLED_BUF_ALIGN`]-aligned pooled buffer with
/// `Vec`-like resize semantics over an aligned backing.
///
/// Invariants: `ptr` is a live `cap`-byte allocation from `alloc_pooled`;
/// `len <= cap`; bytes `[0, len)` are initialized. Handouts start with
/// `len == 0`, and [`PooledBuf::resize`] fills every newly exposed byte, so
/// recycled content is unobservable.
pub struct PooledBuf {
    ptr: *mut u8,
    cap: usize,
    len: usize,
    pool: Arc<BufferPool>,
}

// SAFETY: `ptr` designates a uniquely-owned allocation. Shared (`&`) access
// only reads the initialized `[0, len)` prefix; mutation requires `&mut
// self`. Ownership moves between threads with the value.
unsafe impl Send for PooledBuf {}
unsafe impl Sync for PooledBuf {}

impl PooledBuf {
    /// Backing capacity in bytes. `resize` up to this length never
    /// reallocates (pointer-stable).
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Set the logical length. Newly exposed bytes `[old_len, new_len)` are
    /// filled with `value`; shrinking just truncates the logical view.
    /// Growing past [`Self::capacity`] moves to a fresh aligned allocation
    /// (the displaced pool-sized backing returns to the pool).
    pub fn resize(&mut self, new_len: usize, value: u8) {
        if new_len > self.cap {
            self.grow(new_len);
        }
        if new_len > self.len {
            // SAFETY: `[self.len, new_len)` is within the `cap`-byte
            // allocation; filling it upholds the initialized-prefix
            // invariant.
            unsafe { std::ptr::write_bytes(self.ptr.add(self.len), value, new_len - self.len) };
        }
        self.len = new_len;
    }

    /// Full-capacity mutable view over the backing for **write-only
    /// scratch** use (crypto scratch, zero-copy write-path design §5.7).
    ///
    /// Every byte is initialized memory (zeroed at birth, recycled bytes
    /// hold prior safe writes), so this is sound — but bytes beyond the
    /// logical length may be another user's recycled content. Callers must
    /// only *write* through this view and must pair it with
    /// [`Self::set_written_len`] covering exactly the bytes they wrote, so
    /// stale content can never escape (the same hygiene contract
    /// [`Self::resize`] enforces by filling).
    pub(crate) fn backing_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is a live, uniquely-owned `cap`-byte allocation
        // (struct invariant) whose every byte is initialized (zeroed at
        // birth; only safe writes since); `&mut self` guarantees
        // exclusivity.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.cap) }
    }

    /// Set the logical length after writing `[0, new_len)` through
    /// [`Self::backing_mut`]. Never fills: the caller asserts it wrote every
    /// byte it exposes (see the `backing_mut` contract).
    pub(crate) fn set_written_len(&mut self, new_len: usize) {
        assert!(
            new_len <= self.cap,
            "set_written_len past the backing capacity"
        );
        self.len = new_len;
    }

    fn grow(&mut self, min_cap: usize) {
        let new_cap = min_cap.next_multiple_of(POOLED_BUF_ALIGN);
        let new_ptr = alloc_pooled(new_cap);
        // SAFETY: `[0, len)` of the old backing is initialized; the fresh
        // allocation is at least `len` bytes and cannot overlap it.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len) };
        let old_ptr = std::mem::replace(&mut self.ptr, new_ptr);
        let old_cap = std::mem::replace(&mut self.cap, new_cap);
        if old_cap == self.pool.buf_size {
            self.pool.recycle_ptr(old_ptr);
        } else {
            // SAFETY: `old_ptr` came from `alloc_pooled(old_cap)` (a prior
            // grow) and is no longer reachable.
            unsafe { dealloc_pooled(old_ptr, old_cap) };
        }
    }

    /// Zero-copy conversion into `Bytes` over the aligned backing (the
    /// pooled write-source shape — eligible for `write_block`'s aligned DMA
    /// branch when the logical length is a 4 KiB multiple). The backing is
    /// recycled when the last `Bytes` handle drops.
    pub fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(PooledBufOwner(self))
    }
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        // SAFETY: `[0, len)` is initialized (struct invariant) and `ptr` is
        // valid for `cap >= len` bytes for the lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: as `deref`, plus `&mut self` guarantees exclusivity.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if self.cap == self.pool.buf_size {
            self.pool.recycle_ptr(self.ptr);
        } else {
            // Grown past the pool size: free it rather than poisoning the
            // pool with an over-sized (layout-mismatched) backing.
            // SAFETY: `ptr` came from `alloc_pooled(self.cap)`.
            unsafe { dealloc_pooled(self.ptr, self.cap) };
        }
    }
}

/// Keep-alive owner behind [`PooledBuf::into_bytes`]: recycles the backing
/// on drop of the last `Bytes` handle.
struct PooledBufOwner(PooledBuf);

impl AsRef<[u8]> for PooledBufOwner {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub static BUFFER_POOL: Lazy<Arc<BufferPool>> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let capacity = std::cmp::max(cores * 16, 64);
    Arc::new(BufferPool::new(capacity, 4 * 1024 * 1024))
});

pub enum ReadBlockValue {
    Pooled(PooledBuf),
    Bytes(bytes::Bytes),
}

impl Deref for ReadBlockValue {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            ReadBlockValue::Pooled(p) => &**p,
            ReadBlockValue::Bytes(b) => b.as_ref(),
        }
    }
}

impl AsRef<[u8]> for ReadBlockValue {
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}

/// PERF-4 (b): one contiguous 4 KiB-aligned allocation carrying a pool's
/// entire initial capacity — registerable with io_uring as ONE fixed
/// buffer (`register_buffers` takes a single iovec covering every slot),
/// so slab-resident read bounces skip the per-op page pin (gup) every
/// unregistered DMA pays. The region lives for the pool's life; slots are
/// never freed piecewise (see [`AlignedBufPool::trim_to`]).
struct SlabRegion {
    base: *mut u8,
    len: usize,
}

impl SlabRegion {
    fn contains(&self, ptr: *mut u8) -> bool {
        let p = ptr as usize;
        let b = self.base as usize;
        p >= b && p < b + self.len
    }
}

pub struct AlignedBufPool {
    /// Fresh (individually-allocated) backings — the over-capacity /
    /// pool-exhausted class; freed on over-capacity recycle and by
    /// `trim_to`.
    queue: ArrayQueue<*mut u8>,
    /// Idle slab slots (slab-backed pools only). Sized exactly to the
    /// slot count, so a slab-slot recycle can never overflow into a free.
    slab_queue: Option<ArrayQueue<*mut u8>>,
    slab: Option<SlabRegion>,
    buf_size: usize,
}

unsafe impl Send for AlignedBufPool {}
unsafe impl Sync for AlignedBufPool {}

pub struct AlignedBufOwner {
    pub ptr: *mut u8,
    pub len: usize,
    /// The HOME pool this backing recycles into (2026-07-25 ipc-miss-path
    /// fix: two size-classed pools exist — [`ALIGNED_BUF_POOL`] whole-block
    /// and [`RANGED_BUF_POOL`] sub-block; a cross-pool recycle would hand a
    /// small backing out as a whole-block one, a heap overflow).
    pool: Arc<AlignedBufPool>,
}

unsafe impl Send for AlignedBufOwner {}
unsafe impl Sync for AlignedBufOwner {}

pub struct UringBufOwner {
    pub ptr: *mut u8,
    pub len: usize,
}

unsafe impl Send for UringBufOwner {}
unsafe impl Sync for UringBufOwner {}

impl AsRef<[u8]> for UringBufOwner {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for UringBufOwner {
    fn drop(&mut self) {}
}

impl AsRef<[u8]> for AlignedBufOwner {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBufOwner {
    fn drop(&mut self) {
        // Route through `recycle` so a full pool frees the buffer instead of
        // leaking it (the previous direct `queue.push` dropped over-capacity
        // buffers on the floor). Always the HOME pool (struct doc).
        // SAFETY: `self.ptr` came from `self.pool.alloc_raw()` (owner
        // invariant) and this drop is the last use of the buffer.
        unsafe { self.pool.recycle(self.ptr) };
    }
}

impl AlignedBufPool {
    pub fn new(capacity: usize, buf_size: usize) -> Self {
        let queue = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let _ = queue.push(alloc_pooled(buf_size));
        }
        Self {
            queue,
            slab_queue: None,
            slab: None,
            buf_size,
        }
    }

    /// PERF-4 (b): slab-backed constructor — the initial `capacity`
    /// backings are `capacity` stride-`buf_size` slots of ONE contiguous
    /// aligned allocation ([`SlabRegion`]), exposed via
    /// [`Self::slab_range`] for io_uring fixed-buffer registration. Same
    /// bytes the per-buffer constructor eagerly allocated (zeroed at
    /// birth, lazily committed until touched — registration is what
    /// commits/pins them); over-capacity traffic still rides fresh
    /// individually-freed backings.
    pub fn new_slabbed(capacity: usize, buf_size: usize) -> Self {
        let total = capacity
            .checked_mul(buf_size)
            .expect("slab pool geometry overflow");
        let base = alloc_pooled(total);
        let slab_queue = ArrayQueue::new(capacity);
        for i in 0..capacity {
            // SAFETY: `i * buf_size < total` — inside the slab allocation.
            let _ = slab_queue.push(unsafe { base.add(i * buf_size) });
        }
        Self {
            queue: ArrayQueue::new(capacity),
            slab_queue: Some(slab_queue),
            slab: Some(SlabRegion { base, len: total }),
            buf_size,
        }
    }

    /// The pool's registerable slab window `(base, len)`, when slab-backed
    /// (PERF-4 (b)). Pool statics live for the process, so a fixed-buffer
    /// registration over this range can never dangle.
    pub fn slab_range(&self) -> Option<(usize, usize)> {
        self.slab.as_ref().map(|s| (s.base as usize, s.len))
    }

    /// R5 gauge: bytes of IDLE (queued) pool backings (see
    /// [`BufferPool::allocated_bytes`] for the double-count rationale).
    pub fn allocated_bytes(&self) -> u64 {
        let slab_idle = self
            .slab_queue
            .as_ref()
            .map(|q| q.len() as u64 * self.buf_size as u64)
            .unwrap_or(0);
        self.queue.len() as u64 * self.buf_size as u64 + slab_idle
    }

    /// R5 Red trim (§5.7): free queued (idle) FRESH backings toward
    /// `target`. Slab slots are one allocation and cannot be freed
    /// piecewise — and once registered as an io_uring fixed buffer their
    /// pages are kernel-pinned, so trimming them would return nothing.
    pub fn trim_to(&self, target: u64) {
        while self.allocated_bytes() > target {
            let Some(ptr) = self.queue.pop() else { break };
            // SAFETY: every `queue` pointer came from
            // `alloc_pooled(self.buf_size)` (slab slots live only in
            // `slab_queue`).
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
    }

    pub fn alloc(self: &Arc<Self>) -> (*mut u8, bytes::Bytes) {
        let ptr = self.alloc_raw();
        let owner = AlignedBufOwner {
            ptr,
            len: self.buf_size,
            pool: Arc::clone(self),
        };
        let bytes = bytes::Bytes::from_owner(owner);

        (ptr, bytes)
    }

    /// Take a 4096-aligned buffer of [`Self::buf_size`] without wrapping in
    /// `Bytes` (P2-4: nvme unaligned write path recycles via [`Self::recycle`]).
    /// Slab slots are preferred (PERF-4 (b): they ride the registered
    /// fixed buffer, skipping per-op page pins), then fresh recycles,
    /// then allocation.
    pub fn alloc_raw(self: &Arc<Self>) -> *mut u8 {
        let pooled = self
            .slab_queue
            .as_ref()
            .and_then(|q| q.pop())
            .or_else(|| self.queue.pop());
        let ptr = match pooled {
            Some(p) => {
                crate::fuse_client::METRICS
                    .aligned_pool_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                p
            }
            None => {
                // RW1 H3 evidence (design-random-small-writes §5.3): a handout
                // that missed the recycle queue pays the mmap/page-fault
                // allocation path — the pool-exhaustion counter the ≥13-writer
                // convoy forensics read. (2026-07-25: on the ipc miss path a
                // 4 MiB-class miss is ALSO a THP-zeroing fault + munmap TLB
                // storm per op — the read-bounce traffic rides
                // [`RANGED_BUF_POOL`] for exactly that reason.)
                crate::fuse_client::METRICS
                    .aligned_pool_misses
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                alloc_pooled(self.buf_size)
            }
        };
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "pooled handout violates the alignment contract"
        );
        ptr
    }

    /// Return a buffer previously obtained from [`Self::alloc_raw`].
    ///
    /// # Safety
    ///
    /// `ptr` must have been produced by [`Self::alloc_raw`] (or [`Self::alloc`])
    /// of **this** pool (i.e. `alloc_pooled(self.buf_size)` backing), must not
    /// have been recycled already, and no live reference into the buffer may
    /// outlive this call — the pointer is either re-handed out or deallocated.
    pub unsafe fn recycle(self: &Arc<Self>, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "recycled pointer violates the alignment contract"
        );
        // Slab slots return to their own queue (sized exactly to the slot
        // count — the push can never fail) and are NEVER freed piecewise:
        // they are windows of one allocation, and possibly a registered
        // (kernel-pinned) fixed buffer.
        if let (Some(slab), Some(sq)) = (&self.slab, &self.slab_queue) {
            if slab.contains(ptr) {
                let _ = sq.push(ptr);
                return;
            }
        }
        if self.queue.push(ptr).is_err() {
            // Pool full — free the over-capacity buffer.
            // SAFETY: every non-slab buffer recycled here was produced by
            // `alloc_pooled(self.buf_size)`.
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
    }

    pub fn buf_size(&self) -> usize {
        self.buf_size
    }
}

impl Drop for AlignedBufPool {
    fn drop(&mut self) {
        while let Some(ptr) = self.queue.pop() {
            // SAFETY: every `queue` pointer came from
            // `alloc_pooled(self.buf_size)`.
            unsafe { dealloc_pooled(ptr, self.buf_size) };
        }
        // Slab slots are windows of one allocation — drain the queue (the
        // pointers are not individually owned) and free the region once.
        if let Some(sq) = &self.slab_queue {
            while sq.pop().is_some() {}
        }
        if let Some(slab) = self.slab.take() {
            // SAFETY: `base` came from `alloc_pooled(len)` in
            // `new_slabbed`; outstanding handouts hold an `Arc` to the
            // pool, so by Drop time none exist.
            unsafe { dealloc_pooled(slab.base, slab.len) };
        }
    }
}

pub static ALIGNED_BUF_POOL: Lazy<Arc<AlignedBufPool>> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let capacity = std::cmp::max(cores * 16, 64);
    Arc::new(AlignedBufPool::new(capacity, 4 * 1024 * 1024))
});

/// Sub-block read-bounce pool (2026-07-25 ipc-miss-path fix): R3 ranged
/// reads bounce their LBA window through an aligned pooled buffer, and the
/// only pool was the whole-block 4 MiB one — every concurrent 4 KiB ranged
/// read checked out (or, past ~370 in flight, FRESH-ALLOCATED) a 4 MiB
/// backing, whose first-touch THP zeroing under the io_uring gup
/// (`kernel_init_pages`, measured 40 % of daemon CPU) and per-free TLB
/// shootdowns were the ring-read miss path's 6 ms/op convoy. Windows up to
/// [`RANGED_BUF_SIZE`] ride this pool; larger stay on the whole-block pool.
/// (The kernel transport never sees this: its reads land in registered
/// payload buffers via `dest_addr`.)
pub const RANGED_BUF_SIZE: usize = 64 * 1024;

pub static RANGED_BUF_POOL: Lazy<Arc<AlignedBufPool>> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Sized for miss-path concurrency (hundreds of in-flight sub-block
    // reads), not block count: 64 KiB backings are cheap (cores × 64 ≈
    // 92 MiB on a 23-CPU box), and exhaustion degrades to ordinary
    // allocation — counted by `aligned_pool_misses`.
    // Slab-backed (PERF-4 (b)): the NvmeBlockDev workers register the
    // slab as ONE io_uring fixed buffer, so ranged cold fills DMA without
    // per-op page pins. Same derived byte budget as before — one mapping
    // instead of `capacity` of them.
    let capacity = std::cmp::max(cores * 64, 512);
    Arc::new(AlignedBufPool::new_slabbed(capacity, RANGED_BUF_SIZE))
});

/// Pick the read-bounce pool for a device read of `size` bytes (routing
/// contract pinned by `read_bounce_pool_routing_and_home_recycle`).
pub fn read_bounce_pool(size: usize) -> &'static Arc<AlignedBufPool> {
    if size <= RANGED_BUF_SIZE {
        &RANGED_BUF_POOL
    } else {
        &ALIGNED_BUF_POOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PERF-4 (b): slab-backed pools hand out slab-resident, 4 KiB-aligned
    /// slots first, and every slot lies inside the advertised
    /// `slab_range` (the fixed-buffer registration window).
    #[test]
    fn slabbed_pool_hands_out_slab_resident_slots() {
        let pool = Arc::new(AlignedBufPool::new_slabbed(4, 8192));
        let (base, len) = pool.slab_range().expect("slab-backed pool");
        assert_eq!(len, 4 * 8192);
        assert_eq!(base % POOLED_BUF_ALIGN, 0);

        let ptrs: Vec<*mut u8> = (0..4).map(|_| pool.alloc_raw()).collect();
        for &p in &ptrs {
            let a = p as usize;
            assert!(
                a >= base && a + 8192 <= base + len,
                "initial handout escaped the slab window"
            );
            assert_eq!(a % POOLED_BUF_ALIGN, 0);
        }
        for p in ptrs {
            // SAFETY: every `p` came from this pool's `alloc_raw` above and
            // is returned exactly once, still unaliased (the handout vec is
            // consumed here).
            unsafe { pool.recycle(p) };
        }
    }

    /// Slab slots recycle into the slab queue and are handed out again —
    /// never freed piecewise, even when over-capacity fresh recycles have
    /// filled the fresh queue (the failure mode that would free a window
    /// of the one slab allocation / a registered fixed buffer).
    #[test]
    fn slab_slots_survive_fresh_queue_saturation() {
        let pool = Arc::new(AlignedBufPool::new_slabbed(2, 4096));
        let (base, len) = pool.slab_range().unwrap();

        // Drain the slab, then force fresh allocations…
        let s1 = pool.alloc_raw();
        let s2 = pool.alloc_raw();
        let f1 = pool.alloc_raw();
        let f2 = pool.alloc_raw();
        let f3 = pool.alloc_raw();
        assert!(
            !(f1 as usize >= base && (f1 as usize) < base + len),
            "exhausted slab must serve fresh backings"
        );
        // …and saturate the fresh queue (capacity 2) before the slab
        // slots come home.
        // SAFETY: f1..f3 and s1..s2 each came from this pool's `alloc_raw`
        // above and are each returned exactly once, still unaliased.
        unsafe {
            pool.recycle(f1);
            pool.recycle(f2);
            pool.recycle(f3); // over-capacity fresh: freed
            pool.recycle(s1);
            pool.recycle(s2);
        }

        // Both slab slots must be reusable (they went to the slab queue,
        // not the saturated fresh queue).
        let r1 = pool.alloc_raw() as usize;
        let r2 = pool.alloc_raw() as usize;
        assert!(
            r1 >= base && r1 < base + len && r2 >= base && r2 < base + len,
            "recycled slab slots must be handed out again (slab queue)"
        );
    }

    /// R5 trim frees fresh idle backings but never slab slots (one
    /// allocation; kernel-pinned once registered) — and the idle gauge
    /// counts both classes.
    #[test]
    fn trim_keeps_slab_and_gauge_counts_idle_slots() {
        let pool = Arc::new(AlignedBufPool::new_slabbed(2, 4096));
        assert_eq!(pool.allocated_bytes(), 2 * 4096);

        // Park one fresh backing in the fresh queue.
        let s1 = pool.alloc_raw();
        let s2 = pool.alloc_raw();
        let f1 = pool.alloc_raw();
        // SAFETY: f1/s1/s2 each came from this pool's `alloc_raw` above and
        // are each returned exactly once, still unaliased.
        unsafe {
            pool.recycle(f1);
            pool.recycle(s1);
            pool.recycle(s2);
        }
        assert_eq!(pool.allocated_bytes(), 3 * 4096);

        // Trim to zero: only the fresh backing can go.
        pool.trim_to(0);
        assert_eq!(
            pool.allocated_bytes(),
            2 * 4096,
            "trim must free fresh idle backings and keep every slab slot"
        );
        // Slab slots still serve.
        let r = pool.alloc_raw() as usize;
        let (base, len) = pool.slab_range().unwrap();
        assert!(r >= base && r < base + len);
    }

    /// Non-slab pools advertise no slab window (the register arm in
    /// nvme_dev must fall back to plain reads).
    #[test]
    fn per_buffer_pool_has_no_slab_range() {
        let pool = Arc::new(AlignedBufPool::new(2, 4096));
        assert!(pool.slab_range().is_none());
        let p = pool.alloc_raw();
        // SAFETY: `p` came from this pool's `alloc_raw` above and is
        // returned exactly once, still unaliased.
        unsafe { pool.recycle(p) };
    }
}
