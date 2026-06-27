use crate::dlm::MetaClient;
use crate::error::Result;
use std::sync::Arc;

pub struct BlockAllocator {
    client: Arc<MetaClient>,
    volume_id: String,
}

impl BlockAllocator {
    pub async fn new(client: Arc<MetaClient>, volume_id: &str) -> Result<Self> {
        Ok(Self {
            client,
            volume_id: volume_id.to_string(),
        })
    }

    pub async fn allocate_block(&self) -> Result<u64> {
        let free_set_key = format!("{}:free_blocks", self.volume_id);
        let max_block_key = format!("{}:highest_block", self.volume_id);

        let mut conn = self.client.get_connection().await?;

        // 1. Try to pop a freed block
        let popped: Option<u64> = redis::cmd("SPOP")
            .arg(&free_set_key)
            .query_async(&mut conn)
            .await?;
        let block_idx = match popped {
            Some(idx) => idx,
            None => {
                let new_max: u64 = redis::cmd("INCR")
                    .arg(&max_block_key)
                    .query_async(&mut conn)
                    .await?;
                // INCR returns the value after incrementing (1-based). We start index from 1 to reserve block 0 for superblock.
                new_max
            }
        };

        let chunk_size = 4 * 1024 * 1024; // 4MB
        Ok(block_idx * chunk_size)
    }

    pub async fn free_block(&self, offset: u64) -> Result<()> {
        let chunk_size = 4 * 1024 * 1024;
        let block_idx = offset / chunk_size;

        let free_set_key = format!("{}:free_blocks", self.volume_id);

        let mut conn = self.client.get_connection().await?;
        let _: () = redis::cmd("SADD")
            .arg(&free_set_key)
            .arg(block_idx)
            .query_async(&mut conn)
            .await?;

        Ok(())
    }

    pub async fn calculate_fragmentation(&self) -> Result<(u64, u64, u64, f64)> {
        let free_set_key = format!("{}:free_blocks", self.volume_id);
        let max_block_key = format!("{}:highest_block", self.volume_id);
        let mut conn = self.client.get_connection().await?;

        let highest_block: Option<u64> = redis::cmd("GET")
            .arg(&max_block_key)
            .query_async(&mut conn)
            .await?;
        let highest_block = highest_block.unwrap_or(0);

        let free_blocks: u64 = redis::cmd("SCARD")
            .arg(&free_set_key)
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
        let free_set_key = format!("{}:free_blocks", self.volume_id);
        let mut conn = self.client.get_connection().await?;
        let removed: u64 = redis::cmd("SREM")
            .arg(&free_set_key)
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
        let free_set_key = format!("{}:free_blocks", self.volume_id);
        let mut conn = self.client.get_connection().await?;
        let mut free_blocks: Vec<u64> = redis::cmd("SMEMBERS")
            .arg(&free_set_key)
            .query_async(&mut conn)
            .await?;
        free_blocks.sort_unstable();
        Ok(free_blocks)
    }
}
