use crate::dlm::MetaClient;
use crate::error::Result;
use crossbeam::queue::ArrayQueue;
use crossbeam::utils::CachePadded;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const LOCAL_BATCH: u64 = 256;

struct Reservoir {
    local: ArrayQueue<u64>,
    next_inline: AtomicU64,
    inline_end: AtomicU64,
}

pub struct BlockAllocator {
    client: Arc<MetaClient>,
    _volume_id: Box<str>,
    free_set_key: Box<str>,
    max_block_key: Box<str>,
    reservoirs: Vec<CachePadded<Reservoir>>,
    chunk_size: u64,
}

impl BlockAllocator {
    pub async fn new(client: Arc<MetaClient>, volume_id: &str) -> Result<Self> {
        let free_set_key = format!("{}:free_blocks", volume_id).into_boxed_str();
        let max_block_key = format!("{}:highest_block", volume_id).into_boxed_str();

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let num_reservoirs = std::cmp::max(cores, 4);

        let mut reservoirs = Vec::with_capacity(num_reservoirs);
        for _ in 0..num_reservoirs {
            reservoirs.push(CachePadded::new(Reservoir {
                local: ArrayQueue::new(LOCAL_BATCH as usize),
                next_inline: AtomicU64::new(0),
                inline_end: AtomicU64::new(0),
            }));
        }

        Ok(Self {
            client,
            _volume_id: volume_id.to_string().into_boxed_str(),
            free_set_key,
            max_block_key,
            reservoirs,
            chunk_size: 4 * 1024 * 1024, // 4MB
        })
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        let rs = self.current_reservoir();
        loop {
            // 1. Fast path: lock-free local queue pop (no Redis, no syscalls).
            if let Some(idx) = rs.local.pop() {
                return Ok(idx * self.chunk_size);
            }

            // 2. Medium path: serve from the contiguous inline run with fetch_add.
            let cur = rs.next_inline.load(Ordering::Relaxed);
            let end = rs.inline_end.load(Ordering::Acquire);
            if cur < end {
                if rs
                    .next_inline
                    .compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return Ok(cur * self.chunk_size);
                }
                continue;
            }

            // 3. Slow path: refill reservoirs.
            let mut conn = self.client.get_connection().await?;
            let (spopped, incrbed): (Vec<u64>, u64) = redis::pipe()
                .atomic()
                .cmd("SPOP")
                .arg(&*self.free_set_key)
                .arg(LOCAL_BATCH as usize)
                .cmd("INCRBY")
                .arg(&*self.max_block_key)
                .arg(LOCAL_BATCH)
                .query_async(&mut conn)
                .await?;

            let new_end = incrbed + 1; // INCRBY returns post-increment
            rs.inline_end.store(new_end, Ordering::Release);
            rs.next_inline
                .store(incrbed - LOCAL_BATCH + 1, Ordering::Release);

            for idx in spopped {
                let _ = rs.local.push(idx);
            }
        }
    }

    pub async fn free_block(&self, offset: u64) -> Result<()> {
        let block_idx = offset / self.chunk_size;

        let mut conn = self.client.get_connection().await?;
        let _: () = redis::cmd("SADD")
            .arg(&*self.free_set_key)
            .arg(block_idx)
            .query_async(&mut conn)
            .await?;

        crate::fuse_client::METRICS
            .del_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    pub async fn free_blocks(&self, offsets: &[u64]) -> Result<()> {
        if offsets.is_empty() {
            return Ok(());
        }

        let mut conn = self.client.get_connection().await?;
        let mut pipe = redis::pipe();
        for &offset in offsets {
            let block_idx = offset / self.chunk_size;
            pipe.cmd("SADD").arg(&*self.free_set_key).arg(block_idx);
        }
        let _: () = pipe.query_async(&mut conn).await?;

        crate::fuse_client::METRICS
            .del_obj
            .fetch_add(offsets.len() as u64, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let mut conn = self.client.get_connection().await?;

        let highest_block: Option<u64> = redis::cmd("GET")
            .arg(&*self.max_block_key)
            .query_async(&mut conn)
            .await?;
        let highest_block = highest_block.unwrap_or(0);

        let free_blocks: u64 = redis::cmd("SCARD")
            .arg(&*self.free_set_key)
            .query_async(&mut conn)
            .await?;

        let used_blocks = highest_block.saturating_sub(free_blocks);
        let frag_percent = if highest_block > 0 {
            (free_blocks as f64 / highest_block as f64) * 100.0
        } else {
            0.0
        };

        Ok((highest_block, used_blocks, free_blocks, frag_percent))
    }

    pub async fn allocate_specific_block(&self, block_idx: u64) -> Result<()> {
        let mut conn = self.client.get_connection().await?;
        let removed: u64 = redis::cmd("SREM")
            .arg(&*self.free_set_key)
            .arg(block_idx)
            .query_async(&mut conn)
            .await?;

        if removed == 0 {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Block {} is not free or does not exist",
                block_idx
            )));
        }
        Ok(())
    }

    pub async fn get_free_blocks(&self) -> Result<Vec<u64>> {
        let mut conn = self.client.get_connection().await?;
        let mut free_blocks: Vec<u64> = redis::cmd("SMEMBERS")
            .arg(&*self.free_set_key)
            .query_async(&mut conn)
            .await?;
        free_blocks.sort_unstable();
        Ok(free_blocks)
    }

    fn current_reservoir(&self) -> &Reservoir {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as usize };
        &self.reservoirs[tid % self.reservoirs.len()]
    }
}
