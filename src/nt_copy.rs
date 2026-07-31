//! Non-temporal (streaming) copy core for **DMA-destined** payload
//! merges — near-zero-copy campaign, 2026-07-31
//! (`.benchmarks/2026-07-31-near-zero-copy.md`).
//!
//! ## Why this exists
//!
//! The campaign's copy census proved the two remaining hot-path payload
//! copies are **load-bearing** and cannot be eliminated without breaking
//! a named law:
//!
//! * the **kernel-path merge** (transport payload lease →
//!   `ActiveBlockBuf`) is what lets the FUSE-over-io_uring ent re-arm
//!   (the §5.4 lease-severance boundary: a lease outliving its handler
//!   parks that ent's COMMIT_AND_FETCH — queue starvation) and what lets
//!   the WRITE ACK detach from the DMA (the write-pipeline-depth
//!   campaign's ACK-before-DMA law);
//! * the **ring sever** (client arena → assembly/severed buffer) IS the
//!   L4 §5.2/§5.3.1 isolation boundary — the daemon must never serve
//!   from client-writable memory.
//!
//! So this module optimizes their COST instead: for copies whose
//! destination's next consumer is **device DMA** (never a CPU read),
//! non-temporal stores delete the destination's read-for-ownership
//! traffic (a cached `memcpy` moves ~3 bytes across the memory fabric
//! per payload byte: source read + RFO fill + writeback; an NT copy
//! moves ~2) and keep multi-GiB streams from sweeping the LLC that warm
//! reads and the sync fast path live in.
//!
//! ## Where it is legal
//!
//! Only the two DMA-destined sites use it (each feeds the
//! `nt_copy_bytes` engagement counter):
//!
//! 1. `write_file_staged`'s lease→`ActiveBlockBuf` merge
//!    (`src/fuse_client.rs`) — the buffer's next reader is
//!    `nvme_dev::write_block`'s DMA (or the staging fallback's device
//!    write). RYW snapshot reads of a still-dirty block exist but are
//!    the rare mixed-workload case; they read from DRAM instead of LLC,
//!    which is priced (bounded, correct) — the stream case is the hot
//!    one.
//! 2. the placed-sever arena→assembly copy
//!    (`SharedBlock::write_at`, `src/cache/active_block.rs`) — the
//!    assembly is adopted as the `ActiveBlockBuf` backing and DMA'd.
//!
//! Deliberately **not** used: the pooled sever (destination is
//! CPU-read by the merge), read serves into the arena / ent payload
//! (destination is CPU-read by the app / kernel immediately), CoW
//! copies and zero-fills (rare, correctness paths), and the shim's
//! app→arena copy (destination is CPU-read by the service-thread
//! sever; severs want those lines in LLC).
//!
//! ## Memory-ordering contract (the one subtle bit)
//!
//! Non-temporal stores are **weakly ordered** — they bypass the usual
//! TSO store ordering and drain through write-combining buffers. Every
//! NT body here therefore ends with `sfence` before returning, which
//! restores the usual rule: any subsequently-executed release/unlock
//! (the merge publishes under `BLOCK_FLUSH_LOCKS`; the placed sever
//! publishes via `PlacedClaims::end_write`'s SeqCst) makes the bytes
//! visible to whoever acquires afterwards. Callers need no new
//! discipline; the fence is unconditional and internal. (No loom model:
//! loom cannot express NT stores — the fence is pinned by this comment,
//! the publication-edge smoke test, and review; weakening it is a
//! correctness bug, not a perf tune.)
//!
//! ## Policy
//!
//! Pure core [`policy_from`]: default **enabled** with a **256 KiB**
//! floor (the write path's DMA-destined copies are 1 MiB-class chunks;
//! sub-floor shapes keep cached copies — small writes are latency-bound
//! and their lines are re-read soon). `SQUEEZEFS_NT_COPY=0` is the kill
//! switch, `SQUEEZEFS_NT_COPY_MIN` retunes the floor — both are
//! measurement levers (census rig A/B), not operational escapes.
//! Non-x86_64 targets always take the plain copy (reported honestly).

use std::sync::OnceLock;

/// The env-derived NT policy (pure core — tested via [`policy_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtPolicy {
    pub enabled: bool,
    pub min_bytes: usize,
}

/// Default engagement floor: DMA-destined merge chunks are 1 MiB-class;
/// anything below this is latency-bound small-write territory where
/// cached stores win (lines re-read by flush folds / RYW soon).
pub const DEFAULT_MIN_BYTES: usize = 256 * 1024;

/// Pure policy derivation from `SQUEEZEFS_NT_COPY` /
/// `SQUEEZEFS_NT_COPY_MIN` values. `"0"` on the former kills the path;
/// an unparseable floor falls back to [`DEFAULT_MIN_BYTES`] (never
/// panics — this runs inside the daemon and, transitively, tests).
pub fn policy_from(nt: Option<&str>, min: Option<&str>) -> NtPolicy {
    let enabled = !matches!(nt.map(str::trim), Some("0"));
    let min_bytes = min
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MIN_BYTES);
    NtPolicy { enabled, min_bytes }
}

/// Process-wide cached policy (env read once — the knobs are A/B levers
/// set before mount, not runtime tunables).
fn policy() -> NtPolicy {
    static POLICY: OnceLock<NtPolicy> = OnceLock::new();
    *POLICY.get_or_init(|| {
        policy_from(
            std::env::var("SQUEEZEFS_NT_COPY").ok().as_deref(),
            std::env::var("SQUEEZEFS_NT_COPY_MIN").ok().as_deref(),
        )
    })
}

/// Policy-gated DMA-destined copy. Semantics of `copy_from_slice`
/// (panics on length mismatch); returns `true` iff the NT body ran —
/// callers feed `nt_copy_bytes` (the engagement instrument) from it.
#[inline]
pub fn dma_copy(dst: &mut [u8], src: &[u8]) -> bool {
    let p = policy();
    if p.enabled && dst.len() >= p.min_bytes {
        dma_copy_forced(dst, src)
    } else {
        dst.copy_from_slice(src);
        false
    }
}

/// Policy-bypassing variant (tests + benches): always attempts the NT
/// body on x86_64. Same `copy_from_slice` semantics.
#[inline]
pub fn dma_copy_forced(dst: &mut [u8], src: &[u8]) -> bool {
    assert_eq!(
        dst.len(),
        src.len(),
        "dma_copy: source and destination lengths must match"
    );
    // SAFETY: equal lengths just asserted; `&mut` guarantees the slices
    // cannot overlap.
    unsafe { copy_body(dst.as_mut_ptr(), src.as_ptr(), dst.len()) }
}

/// Policy-gated raw-pointer variant (the placed-sever site, whose
/// destination region is claim-exclusive raw memory).
///
/// # Safety
/// `dst` and `src` must be valid for `len` bytes and non-overlapping.
#[inline]
pub unsafe fn dma_copy_raw(dst: *mut u8, src: *const u8, len: usize) -> bool {
    let p = policy();
    if p.enabled && len >= p.min_bytes {
        copy_body(dst, src, len)
    } else {
        std::ptr::copy_nonoverlapping(src, dst, len);
        false
    }
}

/// Policy-bypassing raw variant (tests).
///
/// # Safety
/// As [`dma_copy_raw`].
#[inline]
pub unsafe fn dma_copy_raw_forced(dst: *mut u8, src: *const u8, len: usize) -> bool {
    copy_body(dst, src, len)
}

/// The copy body: NT-store implementation on x86_64, plain
/// `copy_nonoverlapping` elsewhere. Returns `true` iff NT stores ran.
///
/// # Safety
/// `dst`/`src` valid for `len` bytes, non-overlapping.
#[inline]
unsafe fn copy_body(dst: *mut u8, src: *const u8, len: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        copy_nt_x86(dst, src, len);
        true
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::ptr::copy_nonoverlapping(src, dst, len);
        false
    }
}

/// SSE2 non-temporal copy (SSE2 is baseline on x86_64 — no runtime
/// dispatch, no `target_feature` gate needed): scalar head to 16-byte
/// destination alignment, 64-byte unrolled `movntdq` body (unaligned
/// loads — the source is a transport lease / arena window at arbitrary
/// alignment), scalar tail, trailing `sfence` (see the module-level
/// memory-ordering contract).
///
/// # Safety
/// `dst`/`src` valid for `len` bytes, non-overlapping.
#[cfg(target_arch = "x86_64")]
unsafe fn copy_nt_x86(mut dst: *mut u8, mut src: *const u8, len: usize) {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm_sfence, _mm_stream_si128, __m128i,
    };
    let mut remaining = len;

    // Head: bring dst to 16-byte alignment.
    let head = dst.align_offset(16).min(remaining);
    if head > 0 {
        std::ptr::copy_nonoverlapping(src, dst, head);
        dst = dst.add(head);
        src = src.add(head);
        remaining -= head;
    }

    // Body: 64 B per iteration (4 × movntdq).
    while remaining >= 64 {
        let a = _mm_loadu_si128(src as *const __m128i);
        let b = _mm_loadu_si128(src.add(16) as *const __m128i);
        let c = _mm_loadu_si128(src.add(32) as *const __m128i);
        let d = _mm_loadu_si128(src.add(48) as *const __m128i);
        _mm_stream_si128(dst as *mut __m128i, a);
        _mm_stream_si128(dst.add(16) as *mut __m128i, b);
        _mm_stream_si128(dst.add(32) as *mut __m128i, c);
        _mm_stream_si128(dst.add(48) as *mut __m128i, d);
        dst = dst.add(64);
        src = src.add(64);
        remaining -= 64;
    }
    while remaining >= 16 {
        let v = _mm_loadu_si128(src as *const __m128i);
        _mm_stream_si128(dst as *mut __m128i, v);
        dst = dst.add(16);
        src = src.add(16);
        remaining -= 16;
    }

    // Tail.
    if remaining > 0 {
        std::ptr::copy_nonoverlapping(src, dst, remaining);
    }

    // LOAD-BEARING: NT stores are weakly ordered; the fence is what
    // makes the caller's existing release/unlock publication edges
    // sufficient (module docs). Never remove or make conditional.
    _mm_sfence();
}
