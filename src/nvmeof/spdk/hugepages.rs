//! Hugepage sizing rule, reservation, and preflight math
//! (`docs/design-nvmeof-target-management.md` §6.5, from scoping §5).
//!
//! * DPDK memory default `-s 1024` (1 GiB); reservation default
//!   **1024 × 2 MiB pages = 2 GiB** for headroom. The sizing rule scales
//!   with **transport buffers**
//!   (`in_capsule_data_size × queue_depth × connections`), not namespace
//!   count.
//! * `target setup --hugemem-mb N` records the **prior** `nr_hugepages`
//!   in the state dir before writing (the rig pattern; write-once until
//!   restored), is idempotent, and **never lowers below
//!   currently-allocated-in-use pages**.
//! * `target setup --restore-prior` is the restore path: back to the
//!   recorded prior, record cleared.
//! * Preflight warnings: requested reservation > 25 % of `MemAvailable`
//!   (converged-node hazard, Risks R2); free hugepages at `target start`
//!   < configured `-s` (the G3 no-hugepages loud fail).
//!
//! Every function takes its sysfs/state roots as parameters — the §6.8
//! injection seam (unit tier runs the production code against a tempdir).

use std::io;
use std::path::Path;

use super::super::stack::PreflightError;

/// Production 2 MiB-hugepage sysfs directory.
pub const HUGEPAGES_2M_SYSFS_DIR: &str = "/sys/kernel/mm/hugepages/hugepages-2048kB";
/// One 2 MiB page, in MiB.
pub const HUGEPAGE_2M_MB: u64 = 2;
/// `target setup` reservation default (2 GiB — scoping §5 headroom rule).
pub const DEFAULT_HUGEMEM_MB: u64 = 2048;
/// Recorded-prior file name under the SPDK state dir.
pub const PRIOR_RECORD_FILE: &str = "hugepages-prior";
/// R2 warning threshold: reservation > this fraction of `MemAvailable`.
pub const MEM_AVAILABLE_WARN_FRACTION: f64 = 0.25;

/// MiB → 2 MiB pages, rounding up.
pub fn pages_for_mb(mb: u64) -> u64 {
    let _ = mb;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// One `nr_hugepages`/`free_hugepages` reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HugepageSnapshot {
    pub total: u64,
    pub free: u64,
}

impl HugepageSnapshot {
    /// Pages currently allocated-in-use (the never-lower floor).
    pub fn in_use(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// Read `nr_hugepages` + `free_hugepages` from a hugepage sysfs dir.
pub fn read_snapshot(sysfs_dir: &Path) -> io::Result<HugepageSnapshot> {
    let _ = sysfs_dir;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// What `setup` did (rendered by the verb layer).
#[derive(Debug, Clone)]
pub struct SetupOutcome {
    /// The prior value recorded by THIS run (`None` when a record already
    /// existed — write-once until restored).
    pub prior_recorded: Option<u64>,
    pub requested_pages: u64,
    /// `max(requested, in_use)` — the never-lower-below-in-use clamp.
    pub target_pages: u64,
    pub achieved_pages: u64,
    pub clamped_to_in_use: bool,
    /// `true` when the pool already sat at the target (no write needed).
    pub verified_noop: bool,
    /// Loud advisory lines (R2 MemAvailable fraction, clamp notes).
    pub warnings: Vec<String>,
}

/// The reservation verb body (§6.5): record prior (write-once) → clamp →
/// write → verify readback ≥ requested (shortfall fails loud with the
/// wanted/got numbers — the rig pattern).
pub fn setup(
    sysfs_dir: &Path,
    spdk_state_dir: &Path,
    hugemem_mb: u64,
    mem_available_kb: Option<u64>,
) -> io::Result<SetupOutcome> {
    let _ = (sysfs_dir, spdk_state_dir, hugemem_mb, mem_available_kb);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// What `--restore-prior` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestorePriorOutcome {
    pub prior: u64,
    pub achieved: u64,
}

/// The restore path: write the recorded prior back (refusing to go below
/// in-use pages), then clear the record. A missing record refuses loud.
pub fn restore_prior(sysfs_dir: &Path, spdk_state_dir: &Path) -> io::Result<RestorePriorOutcome> {
    let _ = (sysfs_dir, spdk_state_dir);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// The recorded prior, if any.
pub fn recorded_prior(spdk_state_dir: &Path) -> io::Result<Option<u64>> {
    let _ = spdk_state_dir;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `target start` preflight: free hugepages must cover the configured
/// DPDK memory (`-s`). Failure is the G3 no-hugepages loud fail — the
/// message carries the arithmetic and names `target setup`.
pub fn preflight_free_for_dpdk(sysfs_dir: &Path, dpdk_mem_mb: u64) -> Result<(), PreflightError> {
    let _ = (sysfs_dir, dpdk_mem_mb);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// `MemAvailable` from `/proc/meminfo` (kB), when readable.
pub fn mem_available_kb() -> Option<u64> {
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// R2: does a reservation exceed the warn fraction of `MemAvailable`?
pub fn reservation_exceeds_warn_fraction(hugemem_mb: u64, mem_available_kb: u64) -> bool {
    let _ = (hugemem_mb, mem_available_kb);
    unimplemented!("N3 skeleton — implemented by the feat commit")
}
