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
        unimplemented!("TDD: not yet implemented")
    }

    pub async fn free_block(&self, _offset: u64) -> Result<()> {
        unimplemented!("TDD: not yet implemented")
    }
}
