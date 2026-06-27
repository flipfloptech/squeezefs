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
                // 2. If no freed blocks, increment the global max block counter
                let new_max: u64 = redis::cmd("INCR")
                    .arg(&max_block_key)
                    .query_async(&mut conn)
                    .await?;
                
                // INCR returns the value after incrementing (1-based). We want 0-based index.
                new_max - 1
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
}
