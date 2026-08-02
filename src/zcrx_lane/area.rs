//! The zcrx receive area (design §5/§8): a PMD-aligned, NUMA-bound,
//! chunked anonymous mapping the NIC DMA-writes into (real backend) or the
//! sim recv lands in (contract venue), plus the derived-sizing laws and
//! the R5 `zcrx_area` component.
//!
//! Sizing is DERIVED — no fixed constants (design §8): per queue the area
//! is the queue's in-flight payload window (`depth × max_xfer`, both
//! themselves BDP/MDTS-derived) rounded up to PMD granularity with a one-
//! PMD floor; the refill ring is 1:1 with the area's chunk count (next
//! pow2). R5: the area registers as the non-sheddable `zcrx_area`
//! component at arm — Red blocks NEW lane arms and sheds nothing (the
//! area is fixed); in-flight converges by completion (design §7).

use super::area_core::SpanLedger;
use crate::error::{Result, SqueezefsError};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// x86_64 PMD (2 MiB) — the huge-page granule the area aligns and rounds
/// to (the thp.rs law: an unaligned vma maps huge folios 4 KiB-wise).
pub const PMD_BYTES: u64 = 2 * 1024 * 1024;

/// Derived per-queue area bytes (design §8): `depth × max_xfer` rounded
/// up to PMD, floor one PMD.
pub fn area_bytes_per_queue(_depth: u16, _max_xfer_bytes: u32) -> u64 {
    0 // Z2 phase A stub — contracts red
}

/// Refill-ring entries for an area of `chunks` chunks: 1:1, next pow2
/// (design §8; the kernel requires a power of two).
pub fn rq_entries_for(_chunks: u64) -> u32 {
    0 // Z2 phase A stub — contracts red
}

/// The receive-chunk grain: one page (the kernel zcrx net_iov granule).
pub fn chunk_bytes_default() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) is always callable.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 {
        ps as usize
    } else {
        4096
    }
}

/// One mapped area + its grant ledger. Created per lane queue at arm;
/// gauge-charged to `zcrx_area_bytes` (the R5 component reads the gauge).
pub struct ZcrxArea {
    base: *mut u8,
    len: usize,
    chunk: usize,
    ledger: SpanLedger,
    /// Wakes grant waiters when a chunk recycles (sim backpressure edge;
    /// the real backend's kernel rq ring needs no waiter).
    freed: tokio::sync::Notify,
}

// SAFETY: the area is a process-private anonymous mapping; `base` is
// exclusively owned by this struct (unmapped only on Drop, after every
// slice's Arc clone is gone) and all slice access is read-only or
// exclusive-per-grant by the ledger protocol.
unsafe impl Send for ZcrxArea {}
unsafe impl Sync for ZcrxArea {}

impl ZcrxArea {
    /// Map `len` bytes (PMD-aligned base, `MADV_HUGEPAGE`), optionally
    /// NUMA-bound to `numa_node` BEFORE first touch, chunked at
    /// `chunk_bytes`. Charges the `zcrx_area_bytes` gauge.
    pub fn new(len: u64, chunk_bytes: usize, numa_node: Option<usize>) -> Result<Arc<ZcrxArea>> {
        let _ = (len, chunk_bytes, numa_node);
        Err(SqueezefsError::Io(std::io::Error::other(
            "zcrx area not implemented (PR Z2 phase A)",
        )))
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn chunk_bytes(&self) -> usize {
        self.chunk
    }
    pub fn chunk_count(&self) -> usize {
        self.ledger.capacity()
    }
    /// Free (recycled, grantable) chunks — the refill-discipline gauge.
    pub fn free_chunks(&self) -> usize {
        self.ledger.free_count()
    }

    /// Grant a chunk for the next receive (single consumer — the queue
    /// driver), awaiting a recycle when exhausted. The await is the
    /// GRANT edge only; command admission is bounded by the queue's
    /// admission semaphore so this can only starve transiently.
    pub async fn grant_chunk(self: &Arc<Self>) -> GrantRef {
        loop {
            if let Some(slot) = self.ledger.try_grant() {
                return GrantRef {
                    area: Arc::clone(self),
                    slot,
                };
            }
            self.freed.notified().await;
        }
    }

    fn release_slot(&self, slot: u32) {
        if self.ledger.release(slot) {
            self.freed.notify_waiters();
        }
    }
}

impl Drop for ZcrxArea {
    fn drop(&mut self) {
        if !self.base.is_null() && self.len > 0 {
            // SAFETY: unmapping our own mapping; every GrantRef holds an
            // Arc so Drop runs strictly after the last slice is gone.
            unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
            crate::fuse_client::METRICS
                .zcrx_area_bytes
                .fetch_sub(self.len as u64, Ordering::Relaxed);
        }
    }
}

/// A held reference on one granted chunk. Clone = add_ref; Drop =
/// release (the 0-crossing recycles the chunk to the refill path).
pub struct GrantRef {
    area: Arc<ZcrxArea>,
    slot: u32,
}

impl GrantRef {
    pub fn slot(&self) -> u32 {
        self.slot
    }
    pub fn area(&self) -> &Arc<ZcrxArea> {
        &self.area
    }
    /// The chunk's base pointer.
    pub fn chunk_ptr(&self) -> *mut u8 {
        // SAFETY: slot < chunk_count by ledger construction; the chunk
        // lies wholly within the mapping.
        unsafe { self.area.base.add(self.slot as usize * self.area.chunk) }
    }
}

impl Clone for GrantRef {
    fn clone(&self) -> Self {
        self.area.ledger.add_ref(self.slot);
        GrantRef {
            area: Arc::clone(&self.area),
            slot: self.slot,
        }
    }
}

impl Drop for GrantRef {
    fn drop(&mut self) {
        self.area.release_slot(self.slot);
    }
}

/// A byte span inside a granted chunk — the parser's payload unit (holds
/// its chunk alive via the grant ref; read-only).
pub struct AreaSlice {
    grant: GrantRef,
    ptr: *const u8,
    len: usize,
}

// SAFETY: read-only view into the area; the grant ref keeps the chunk
// from recycling (and the area mapped) for the slice's lifetime.
unsafe impl Send for AreaSlice {}
unsafe impl Sync for AreaSlice {}

impl AreaSlice {
    /// A slice over `[off, off+len)` of `grant`'s chunk-resident bytes
    /// at `ptr` (the receive extent, not necessarily a whole chunk).
    pub fn new(grant: GrantRef, ptr: *const u8, len: usize) -> Self {
        AreaSlice { grant, ptr, len }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn grant(&self) -> &GrantRef {
        &self.grant
    }
    /// Sub-span (clones the chunk ref).
    pub fn sub(&self, off: usize, len: usize) -> AreaSlice {
        assert!(off + len <= self.len, "sub-span out of bounds");
        AreaSlice {
            grant: self.grant.clone(),
            // SAFETY: in-bounds by the assert above.
            ptr: unsafe { self.ptr.add(off) },
            len,
        }
    }
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len constructed in-bounds; chunk pinned by grant.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// Register the non-sheddable R5 `zcrx_area` component (idempotent; the
/// gauge is the process-wide `zcrx_area_bytes` sum over live areas —
/// design §7: Red blocks NEW arms and sheds nothing).
pub fn register_r5_component() {
    // Z2 phase A stub — contracts red.
}
