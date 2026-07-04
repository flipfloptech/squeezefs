use crate::error::Result;

pub async fn recover_staging(
    _staging_dir: &std::path::Path,
    _redis_client: &crate::dlm::MetaClient,
    _block_alloc: &crate::block_allocator::BlockAllocator,
    _nvme_dev: &crate::nvme_dev::NvmeBlockDev,
    _dlm: Option<&crate::dlm::DlmClient>,
) -> Result<usize> {
    // In-memory block and metadata store does not require external crash recovery sync.
    Ok(0)
}
