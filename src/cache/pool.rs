use crossbeam::queue::ArrayQueue;
use once_cell::sync::Lazy;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

pub struct BufferPool {
    queue: ArrayQueue<Vec<u8>>,
    buf_size: usize,
}

impl BufferPool {
    pub fn new(capacity: usize, buf_size: usize) -> Self {
        let queue = ArrayQueue::new(capacity);
        for _ in 0..capacity {
            let _ = queue.push(vec![0u8; buf_size]);
        }
        Self { queue, buf_size }
    }

    pub fn alloc(self: &Arc<Self>) -> PooledBuf {
        let buf = self.queue.pop().unwrap_or_else(|| vec![0u8; self.buf_size]);
        PooledBuf {
            buf: Some(buf),
            pool: Some(self.clone()),
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

pub struct PooledBuf {
    buf: Option<Vec<u8>>,
    pool: Option<Arc<BufferPool>>,
}

impl Deref for PooledBuf {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        self.buf.as_ref().unwrap()
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buf.as_mut().unwrap()
    }
}

impl PooledBuf {
    pub fn into_inner(mut self) -> Vec<u8> {
        self.buf.take().unwrap()
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let (Some(buf), Some(pool)) = (self.buf.take(), &self.pool) {
            let _ = pool.queue.push(buf);
        }
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

impl AsRef<[u8]> for AlignedBufOwner {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBufOwner {
    fn drop(&mut self) {
        let _ = ALIGNED_BUF_POOL.queue.push(self.ptr);
    }
}

impl AlignedBufPool {
    pub fn new(capacity: usize, buf_size: usize) -> Self {
        let queue = ArrayQueue::new(capacity);
        let layout = std::alloc::Layout::from_size_align(buf_size, 4096).unwrap();
        for _ in 0..capacity {
            let ptr = unsafe { std::alloc::alloc(layout) };
            assert!(!ptr.is_null());
            let _ = queue.push(ptr);
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
        self.queue.pop().unwrap_or_else(|| {
            let layout = std::alloc::Layout::from_size_align(self.buf_size, 4096).unwrap();
            let ptr = unsafe { std::alloc::alloc(layout) };
            assert!(!ptr.is_null());
            ptr
        })
    }

    /// Return a buffer previously obtained from [`Self::alloc_raw`].
    pub fn recycle(self: &Arc<Self>, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        if self.queue.push(ptr).is_err() {
            // Pool full — free the over-capacity buffer.
            let layout = std::alloc::Layout::from_size_align(self.buf_size, 4096).unwrap();
            unsafe {
                std::alloc::dealloc(ptr, layout);
            }
        }
    }

    pub fn buf_size(&self) -> usize {
        self.buf_size
    }
}

pub static ALIGNED_BUF_POOL: Lazy<Arc<AlignedBufPool>> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let capacity = std::cmp::max(cores * 16, 64);
    Arc::new(AlignedBufPool::new(capacity, 4 * 1024 * 1024))
});
