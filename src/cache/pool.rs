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

pub struct AlignedBufPool {
    queue: ArrayQueue<*mut u8>,
    buf_size: usize,
}

unsafe impl Send for AlignedBufPool {}
unsafe impl Sync for AlignedBufPool {}

pub struct AlignedBufOwner {
    pub ptr: *mut u8,
    pub len: usize,
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
        // buffers on the floor).
        ALIGNED_BUF_POOL.recycle(self.ptr);
    }
}

impl AlignedBufPool {
    pub fn new(capacity: usize, buf_size: usize) -> Self {
        let queue = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let _ = queue.push(alloc_pooled(buf_size));
        }
        Self { queue, buf_size }
    }

    pub fn alloc(self: &Arc<Self>) -> (*mut u8, bytes::Bytes) {
        let ptr = self.alloc_raw();
        let owner = AlignedBufOwner {
            ptr,
            len: self.buf_size,
        };
        let bytes = bytes::Bytes::from_owner(owner);

        (ptr, bytes)
    }

    /// Take a 4096-aligned buffer of [`Self::buf_size`] without wrapping in
    /// `Bytes` (P2-4: nvme unaligned write path recycles via [`Self::recycle`]).
    pub fn alloc_raw(self: &Arc<Self>) -> *mut u8 {
        let ptr = self
            .queue
            .pop()
            .unwrap_or_else(|| alloc_pooled(self.buf_size));
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "pooled handout violates the alignment contract"
        );
        ptr
    }

    /// Return a buffer previously obtained from [`Self::alloc_raw`].
    pub fn recycle(self: &Arc<Self>, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        debug_assert_eq!(
            ptr as usize % POOLED_BUF_ALIGN,
            0,
            "recycled pointer violates the alignment contract"
        );
        if self.queue.push(ptr).is_err() {
            // Pool full — free the over-capacity buffer.
            // SAFETY: every buffer recycled here was produced by
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
            // SAFETY: every queued pointer came from
            // `alloc_pooled(self.buf_size)`.
            unsafe { dealloc_pooled(ptr, self.buf_size) };
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
