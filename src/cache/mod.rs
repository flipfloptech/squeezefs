pub mod gds;
pub mod lru;
pub mod nvme;

use crate::backend::RustFsClient;
use crate::error::Result;
use std::path::PathBuf;

#[derive(Clone)]
pub struct TieredCache {
    pub gds: gds::GdsCache,
    pub lru: lru::LruCache,
    pub nvme: nvme::NvmeStaging,
}

impl TieredCache {
    pub fn new(
        staging_dir: PathBuf,
        backend: RustFsClient,
        redis_client: redis::Client,
    ) -> Result<Self> {
        let gds = gds::GdsCache::new();
        let lru = lru::LruCache::new()?;
        let nvme = nvme::NvmeStaging::new(staging_dir, backend, redis_client)?;
        Ok(Self { gds, lru, nvme })
    }
}
