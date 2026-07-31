//! Non-temporal (streaming) copy core for **DMA-destined** payload
//! merges — near-zero-copy campaign, 2026-07-31 (red-commit skeleton:
//! contracts first, the NT body lands in the green commit).

/// The env-derived NT policy (pure core, tested via [`policy_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtPolicy {
    pub enabled: bool,
    pub min_bytes: usize,
}

/// Pure policy derivation from `SQUEEZEFS_NT_COPY` /
/// `SQUEEZEFS_NT_COPY_MIN`.
pub fn policy_from(_nt: Option<&str>, _min: Option<&str>) -> NtPolicy {
    NtPolicy {
        enabled: false,
        min_bytes: 0,
    }
}

/// Policy-gated copy: returns `true` when the NT path ran.
pub fn dma_copy(dst: &mut [u8], src: &[u8]) -> bool {
    dst.copy_from_slice(src);
    false
}

/// Policy-bypassing copy (tests): always attempts the NT body.
pub fn dma_copy_forced(dst: &mut [u8], src: &[u8]) -> bool {
    dst.copy_from_slice(src);
    false
}

/// Raw-pointer variant of [`dma_copy_forced`].
///
/// # Safety
/// `dst` and `src` must be valid for `len` bytes and non-overlapping.
pub unsafe fn dma_copy_raw_forced(dst: *mut u8, src: *const u8, len: usize) -> bool {
    std::ptr::copy_nonoverlapping(src, dst, len);
    false
}
