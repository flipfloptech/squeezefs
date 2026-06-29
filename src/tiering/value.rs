use crate::tiering::nvme::NvmeReadGuard;
use bytes::Bytes;
use std::ops::Deref;

/// Unified zero-copy reference to cached bytes.
/// Dereferences to `&[u8]` to enable zero-copy reads across Memory, NVMe, and NVMe-oF Backend tiers.
pub enum CacheValue<'a> {
    Memory(Bytes),
    Nvme(NvmeReadGuard<'a>),
    Owned(Bytes),
}

impl<'a> Deref for CacheValue<'a> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            CacheValue::Memory(b) => b.as_ref(),
            CacheValue::Nvme(g) => &g.guard.mmap[g.offset..g.offset + g.len],
            CacheValue::Owned(b) => b.as_ref(),
        }
    }
}

impl<'a> AsRef<[u8]> for CacheValue<'a> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.deref()
    }
}
