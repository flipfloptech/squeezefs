use crate::error::Result;
use crate::meta_backend::storage::MetaLvStorage;

pub struct Journal {
    pub start_offset: u64,
    pub size: u64,
}

impl Journal {
    pub fn new(start_offset: u64, size: u64) -> Self {
        Self { start_offset, size }
    }

    /// Write a redo log record to the circular journal (Phase 0 skeleton)
    pub async fn write_record(&self, _storage: &MetaLvStorage, _record: &[u8]) -> Result<()> {
        // No-op skeleton for Phase 0
        Ok(())
    }

    /// Replays outstanding log entries on mount/recovery
    pub async fn replay(&self, _storage: &MetaLvStorage) -> Result<()> {
        // No-op skeleton for Phase 0
        Ok(())
    }
}
