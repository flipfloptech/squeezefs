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
pub fn area_bytes_per_queue(depth: u16, max_xfer_bytes: u32) -> u64 {
    let window = (depth as u64).saturating_mul(max_xfer_bytes as u64);
    window.div_ceil(PMD_BYTES).max(1) * PMD_BYTES
}

/// Refill-ring entries for an area of `chunks` chunks: 1:1, next pow2
/// (design §8; the kernel requires a power of two).
pub fn rq_entries_for(chunks: u64) -> u32 {
    chunks.max(1).next_power_of_two().min(u32::MAX as u64) as u32
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
    /// NUMA-bound to `numa_node` BEFORE first touch (fault-time
    /// placement — the numa_core arena law), chunked at `chunk_bytes`.
    /// Charges the `zcrx_area_bytes` gauge; Drop credits it.
    pub fn new(len: u64, chunk_bytes: usize, numa_node: Option<usize>) -> Result<Arc<ZcrxArea>> {
        let len = usize::try_from(len).map_err(|_| {
            SqueezefsError::Io(std::io::Error::other("zcrx area length overflows usize"))
        })?;
        if len == 0 || chunk_bytes == 0 || len % chunk_bytes != 0 {
            return Err(SqueezefsError::Io(std::io::Error::other(format!(
                "zcrx area geometry invalid: len={len} chunk={chunk_bytes}"
            ))));
        }
        let base = map_anon_pmd_aligned(len).ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "zcrx area mmap failed ({len} bytes)"
            )))
        })?;
        // SAFETY: advisory on our own fresh mapping.
        unsafe { libc::madvise(base as *mut libc::c_void, len, libc::MADV_HUGEPAGE) };
        // NUMA bind BEFORE first touch (refusal-tolerant: placement is an
        // optimization, never a correctness need — numa_core contract).
        if let Some(node) = numa_node {
            let took = crate::numa_core::topology().bind_region_preferred(base, len, node);
            log::debug!("zcrx-lane: area bind to node {node}: took={took} ({len} bytes)");
        }
        crate::fuse_client::METRICS
            .zcrx_area_bytes
            .fetch_add(len as u64, Ordering::Relaxed);
        Ok(Arc::new(ZcrxArea {
            base,
            len,
            chunk: chunk_bytes,
            ledger: SpanLedger::new(len / chunk_bytes),
            freed: tokio::sync::Notify::new(),
        }))
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
            // Register interest BEFORE the probe (the notify-then-check
            // race: a release between try_grant and notified() must not
            // strand this waiter).
            let notified = self.freed.notified();
            if let Some(slot) = self.ledger.try_grant() {
                return GrantRef {
                    area: Arc::clone(self),
                    slot,
                };
            }
            notified.await;
        }
    }

    /// Non-blocking grant (the real backend's driver thread never awaits;
    /// the kernel rq ring is its backpressure venue).
    pub fn try_grant_chunk(self: &Arc<Self>) -> Option<GrantRef> {
        self.ledger.try_grant().map(|slot| GrantRef {
            area: Arc::clone(self),
            slot,
        })
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

/// Anonymous private RW mapping whose base is PMD-aligned — the
/// `thp.rs map_shared_pmd_aligned` reservation trick (over-reserve
/// PROT_NONE, MAP_FIXED the real mapping at the aligned offset, trim the
/// slack) over MAP_ANONYMOUS instead of an fd.
fn map_anon_pmd_aligned(len: usize) -> Option<*mut u8> {
    let pmd = PMD_BYTES as usize;
    let span = len.checked_add(pmd)?;
    // SAFETY: fresh anonymous PROT_NONE reservation.
    let reserve = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if reserve == libc::MAP_FAILED {
        return None;
    }
    let addr = reserve as usize;
    let aligned = (addr + pmd - 1) & !(pmd - 1);
    let head = aligned - addr;
    let tail = span - head - len;
    // SAFETY: MAP_FIXED inside our own reservation.
    let base = unsafe {
        libc::mmap(
            aligned as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        // SAFETY: unmapping our own reservation.
        unsafe { libc::munmap(reserve, span) };
        return None;
    }
    // SAFETY: trimming the slack of our own reservation.
    unsafe {
        if head > 0 {
            libc::munmap(reserve, head);
        }
        if tail > 0 {
            libc::munmap((aligned + len) as *mut libc::c_void, tail);
        }
    }
    Some(base as *mut u8)
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
    /// A slice over `len` chunk-resident bytes at `ptr` (the receive
    /// extent, not necessarily a whole chunk).
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
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::mem_budget::MEM_BUDGET.register(crate::mem_budget::Component::new(
            "zcrx_area",
            0,
            1,
            std::sync::Arc::new(|| {
                crate::fuse_client::METRICS
                    .zcrx_area_bytes
                    .load(Ordering::Relaxed)
            }),
            // Non-sheddable: the area is fixed registered DMA memory —
            // Red blocks NEW arms (`arm_admission`) instead.
            std::sync::Arc::new(|_| {}),
        ));
    });
}
