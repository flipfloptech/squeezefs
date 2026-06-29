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
        .unwrap_or(16);
    let capacity = std::cmp::max(cores * 16, 512);
    Arc::new(BufferPool::new(capacity, 4 * 1024 * 1024))
});
