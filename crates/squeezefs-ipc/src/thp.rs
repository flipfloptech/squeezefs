//! Session-arena transparent-huge-page economy — near-zero-copy
//! campaign, 2026-07-31 (red-commit skeleton: contracts first, the
//! madvise/populate/collapse body lands in the green commit).

/// How aggressively to pursue huge pages for a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThpMode {
    /// `MADV_HUGEPAGE` only (fault-time PMD allocation where policy
    /// allows) — the shim-side posture.
    Advise,
    /// `MADV_HUGEPAGE` + `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` — the
    /// daemon-side session-admission posture (one-time, off the data
    /// path; collapse bypasses the shmem sysfs policy).
    PopulateCollapse,
}

/// Best-effort outcome — refusals are reported, never raised.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThpOutcome {
    pub madvise_ok: bool,
    pub collapse_ok: bool,
}

/// Advise/populate/collapse huge pages over `[base, base + len)`.
/// Never fails: every refusal degrades to a `false` in the outcome.
pub fn advise_hugepages(_base: *mut u8, _len: usize, _mode: ThpMode) -> ThpOutcome {
    ThpOutcome::default()
}
