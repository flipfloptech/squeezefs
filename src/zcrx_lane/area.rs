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

/// Admission accounting grain (bytes per semaphore permit) — one page
/// class: a sub-page read rounds to one unit, and the unit matching the
/// chunk grain keeps permit arithmetic and chunk arithmetic composable.
pub const ADMISSION_UNIT: usize = 4096;

/// Admission permits over the ADMITTED payload window — since the
/// 2026-08-06 engagement campaign the caller's window admits in FULL
/// (floor one chunk). The retired `/2` halved the window as an implicit
/// delivery-slack budget; that budget now lives in the AREA size where
/// it belongs ([`delivery_slack_bytes`]), so the admitted window and
/// the CID namespace are the SAME arithmetic (`depth × max_xfer` ≡
/// depth commands of max_xfer): the round-8 field verdict measured the
/// halved window declining most of the ~51 MiB/device the cold row
/// offers (0.05 % engagement — `.benchmarks/2026-08-05-zcrx-z3-field-
/// rows.md`), and a declined read is a kernel-RX-copy read on a
/// CPU-walled box.
pub fn admission_permits(admit_window_len: usize, chunk: usize) -> usize {
    (admit_window_len.max(chunk) / ADMISSION_UNIT).max(1)
}

/// One wire delivery burst's worst-case chunk geometry: an MTU-grain
/// payload lands in `⌈mtu/chunk⌉` page-grain niovs (HDS splits the
/// header off; payload starts at a fresh niov), so `burst_bytes` chunk
/// bytes hold `payload_bytes` of payload. `None` = the delivery grain
/// is unknown (MTU probe failed) — callers degrade to the occupancy-½
/// posture. `payload_bytes` uses the interface MTU verbatim: the
/// L4 header bytes it overstates shave < 1–4 % of occupancy, absorbed
/// by the PMD round-up and, at the pathological end (sub-MTU
/// segmentation storms), by the round-5/6 park governor — parks are
/// flow control, never poison.
pub struct BurstGeometry {
    pub payload_bytes: u64,
    pub burst_bytes: u64,
}

pub fn burst_geometry(mtu: Option<u32>, chunk: usize) -> Option<BurstGeometry> {
    let m = u64::from(mtu?);
    if m == 0 || chunk == 0 {
        return None;
    }
    let chunks = m.div_ceil(chunk as u64).max(1);
    Some(BurstGeometry {
        payload_bytes: m,
        burst_bytes: chunks * chunk as u64,
    })
}

/// The MPWQE (striding-RQ) per-frame stride model (round 3, 2026-08-06
/// — the pool-term adjudication against the sqz linux-6.19.14 tree):
/// mlx5 reports its RX ring in FRAMES (`ethtool -g` rx =
/// `1 << log_rq_mtu_frames`, en_ethtool.c:372), and a striding-RQ
/// queue's standing provider-pool demand is `pages_per_wqe <<
/// log_rq_size` (en_main.c:940–941), which algebraically reduces to
/// `frames × linear_stride_sz` — `log_wqe_sz` cancels
/// (params.c:415 `log_rq_size = log_rq_mtu_frames − log_pkts_per_wqe`;
/// :292–301 `log_pkts_per_wqe = log_wqe_sz − order2(linear_stride)`)
/// — so the term is probeable WITHOUT any driver-internal input. The
/// stride is `roundup_pow_of_two(SKB_FRAG_SZ(headroom + hw_mtu))`
/// (params.c:284, :252–262): mtu + NET_SKB_PAD (64, en.h:79) +
/// `hard_mtu` (≤ 22, en.h:75 SW2HW) + `SKB_DATA_ALIGN(skb_shared_info)`
/// (320 on x86_64) + two SKB_DATA_ALIGN roundings (≤ 128) — ceiled to
/// 512 B on the line: over-estimating the overhead only rounds UP at a
/// pow2 boundary, the safe (over-provisioning) direction.
pub fn mpwqe_stride_bytes(mtu: u32) -> u64 {
    /// The kernel skb linear-size arithmetic above, ceiled (≤ 498 real).
    const MPWQE_SKB_LINEAR_OVERHEAD_BYTES: u64 = 512;
    (u64::from(mtu) + MPWQE_SKB_LINEAR_OVERHEAD_BYTES).next_power_of_two()
}

/// The NIC RX ring's STANDING demand on the provider pool — round 6
/// modeled the LEGACY/cyclic RQ (`descs × ⌈mtu/chunk⌉ chunks`); round 3
/// adds the STRIDING-RQ (MPWQE) model ([`mpwqe_stride_bytes`] —
/// `frames × stride`), because a zcrx-provider-backed mlx5 queue runs
/// striding RQ + SHAMPO HDS (en_main.c:826/:1005–1007 — the header
/// pool is SEPARATE for unreadable-MP queues; payload strides ride the
/// provider pool) and the queue-restart path reuses the channel params
/// verbatim (en_main.c:5561–5601). The RQ mode is not portably
/// probeable from userspace, so the derivation takes **max(legacy,
/// striding)** — the round-6 safe direction (small-MTU rails keep the
/// legacy floor: 2 KiB strides < 1-chunk frames). Field rail: 8192 ×
/// 16 KiB = 128 MiB vs the legacy 96 MiB — the 32 MiB shortfall that
/// held every round-2 failover window starved. Unknown MTU degrades to
/// one chunk per descriptor (the round-6 floor; the failed-probe warn
/// is loud at the arm site).
pub fn ring_standing_bytes(rx_descs: u32, mtu: Option<u32>, chunk: usize) -> u64 {
    let legacy_per_desc = match mtu {
        Some(m) if m > 0 => (m as usize).div_ceil(chunk.max(1)).max(1),
        _ => 1,
    };
    let legacy = rx_descs as u64 * legacy_per_desc as u64 * chunk as u64;
    let striding = match mtu {
        Some(m) if m > 0 => rx_descs as u64 * mpwqe_stride_bytes(m),
        _ => 0,
    };
    legacy.max(striding)
}

/// The delivery-slack AREA allotment for a fully-admitted fill window
/// (2026-08-06 engagement campaign — the retired admission `/2`'s
/// implicit budget, made explicit and DERIVED): admitted payload
/// occupies chunks at the burst occupancy ([`burst_geometry`] —
/// `payload/burst` ≈ 73 % at the 9000-MTU/4 KiB field shape), so the
/// fills' chunk budget needs `window × (burst − payload)/payload` extra
/// bytes, PMD-rounded (hugepage hygiene; the area total stays
/// chunk-granular). Unknown MTU degrades to `window` — occupancy ½,
/// byte-identical to the retired posture's budget.
pub fn delivery_slack_bytes(fill_window: u64, mtu: Option<u32>, chunk: usize) -> u64 {
    let slack = match burst_geometry(mtu, chunk) {
        Some(g) => fill_window
            .saturating_mul(g.burst_bytes - g.payload_bytes)
            .div_ceil(g.payload_bytes),
        None => fill_window,
    };
    if slack == 0 {
        0
    } else {
        slack.div_ceil(PMD_BYTES) * PMD_BYTES
    }
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

    /// Re-materialize a grant whose ledger ref the caller holds RAW
    /// (via [`GrantRef::into_raw_slot`]).
    ///
    /// # Safety
    /// The caller must own exactly one outstanding raw ref for `slot`
    /// (this transfers it back into RAII custody — never a resurrection).
    pub unsafe fn adopt_grant(self: &Arc<Self>, slot: u32) -> GrantRef {
        GrantRef {
            area: Arc::clone(self),
            slot,
        }
    }

    /// Release a RAW-held ledger ref (the driver-exit cleanup arm for
    /// slots that never re-materialized).
    ///
    /// # Safety
    /// As [`Self::adopt_grant`]: the caller must own the ref it drops.
    pub unsafe fn release_raw(&self, slot: u32) {
        self.release_slot(slot);
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
    /// Disassemble into the raw slot, KEEPING the ledger ref (the caller
    /// now owns it — rebuild custody with [`ZcrxArea::adopt_grant`] or
    /// drop it with [`ZcrxArea::release_raw`]). The embedded area `Arc`
    /// is dropped properly (the caller's own handle keeps the area
    /// alive), so this never leaks the mapping the way a bare
    /// `mem::forget` would.
    pub fn into_raw_slot(self) -> u32 {
        let mut this = std::mem::ManuallyDrop::new(self);
        let slot = this.slot;
        // SAFETY: the field is taken exactly once out of ManuallyDrop;
        // GrantRef::drop is skipped, so the ledger ref stays held.
        unsafe { std::ptr::drop_in_place(&mut this.area) };
        slot
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
    ///
    /// MEM-4: `unsafe` because [`Self::as_slice`] dereferences `ptr` for
    /// `len` bytes with no further check — a safe constructor let safe code
    /// publish a slice over an arbitrary address.
    ///
    /// # Safety
    ///
    /// `ptr..ptr + len` must lie inside the chunk `grant` holds (the grant
    /// is what keeps it from recycling and keeps the area mapped), and the
    /// bytes must be initialized receive data.
    pub unsafe fn new(grant: GrantRef, ptr: *const u8, len: usize) -> Self {
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
