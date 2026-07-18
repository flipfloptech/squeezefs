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

use std::fs;
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
    mb.div_ceil(HUGEPAGE_2M_MB)
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

fn read_u64_attr(path: &Path) -> io::Result<u64> {
    let raw = fs::read_to_string(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot read hugepage attr {}: {e}", path.display()),
        )
    })?;
    raw.trim().parse::<u64>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "hugepage attr {} holds non-numeric '{}': {e}",
                path.display(),
                raw.trim()
            ),
        )
    })
}

/// Read `nr_hugepages` + `free_hugepages` from a hugepage sysfs dir.
pub fn read_snapshot(sysfs_dir: &Path) -> io::Result<HugepageSnapshot> {
    Ok(HugepageSnapshot {
        total: read_u64_attr(&sysfs_dir.join("nr_hugepages"))?,
        free: read_u64_attr(&sysfs_dir.join("free_hugepages"))?,
    })
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

/// The recorded prior, if any.
pub fn recorded_prior(spdk_state_dir: &Path) -> io::Result<Option<u64>> {
    let path = spdk_state_dir.join(PRIOR_RECORD_FILE);
    match fs::read_to_string(&path) {
        Ok(raw) => raw.trim().parse::<u64>().map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "hugepage prior record {} holds non-numeric '{}': {e} — remove or fix \
                         it by hand",
                    path.display(),
                    raw.trim()
                ),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
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
    let snap = read_snapshot(sysfs_dir)?;
    let requested_pages = pages_for_mb(hugemem_mb);
    let mut warnings = Vec::new();

    if let Some(avail_kb) = mem_available_kb {
        if reservation_exceeds_warn_fraction(hugemem_mb, avail_kb) {
            warnings.push(format!(
                "requested hugepage reservation ({hugemem_mb} MiB) exceeds {:.0} % of \
                 MemAvailable ({} MiB) — on a converged node the daemon's memory budget cannot \
                 see hugepages; pass the mount an explicit --mem-budget reduced by the \
                 reservation (design R2)",
                MEM_AVAILABLE_WARN_FRACTION * 100.0,
                avail_kb / 1024
            ));
        }
    }

    // Never lower below currently-allocated-in-use pages.
    let in_use = snap.in_use();
    let clamped_to_in_use = requested_pages < in_use;
    let target_pages = requested_pages.max(in_use);
    if clamped_to_in_use {
        warnings.push(format!(
            "requested {requested_pages} pages but {in_use} pages are currently in use \
             (running target?) — clamping the reservation to {target_pages}; stop the \
             consumer first if you really want fewer"
        ));
    }

    // Record the prior BEFORE mutating, write-once until restored: a
    // second setup must not overwrite the true pre-SqueezeFS value with
    // an already-mutated one.
    let prior_recorded = if recorded_prior(spdk_state_dir)?.is_none() {
        fs::create_dir_all(spdk_state_dir)?;
        fs::write(
            spdk_state_dir.join(PRIOR_RECORD_FILE),
            format!("{}\n", snap.total),
        )?;
        Some(snap.total)
    } else {
        None
    };

    if snap.total == target_pages {
        return Ok(SetupOutcome {
            prior_recorded,
            requested_pages,
            target_pages,
            achieved_pages: snap.total,
            clamped_to_in_use,
            verified_noop: true,
            warnings,
        });
    }

    let nr_path = sysfs_dir.join("nr_hugepages");
    fs::write(&nr_path, format!("{target_pages}\n")).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot write {} <- {target_pages}: {e}", nr_path.display()),
        )
    })?;
    let achieved_pages = read_u64_attr(&nr_path)?;
    if achieved_pages < requested_pages {
        return Err(io::Error::other(format!(
            "hugepage reservation fell short: wanted {requested_pages} × 2 MiB pages, kernel \
             granted {achieved_pages} — memory is too fragmented or too small; free memory \
             (or reboot) and retry, or request less with --hugemem-mb"
        )));
    }
    Ok(SetupOutcome {
        prior_recorded,
        requested_pages,
        target_pages,
        achieved_pages,
        clamped_to_in_use,
        verified_noop: false,
        warnings,
    })
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
    let record_path = spdk_state_dir.join(PRIOR_RECORD_FILE);
    let prior = recorded_prior(spdk_state_dir)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no recorded prior hugepage value at {} — nothing to restore (the record is \
                 written by 'squeezefs nvmeof target setup' and cleared by this verb)",
                record_path.display()
            ),
        )
    })?;
    let snap = read_snapshot(sysfs_dir)?;
    let in_use = snap.in_use();
    if prior < in_use {
        return Err(io::Error::other(format!(
            "refusing to restore nr_hugepages to {prior}: {in_use} pages are currently in use \
             (a running spdk_tgt?) — run 'squeezefs nvmeof target stop' first, then restore"
        )));
    }
    let nr_path = sysfs_dir.join("nr_hugepages");
    fs::write(&nr_path, format!("{prior}\n"))?;
    let achieved = read_u64_attr(&nr_path)?;
    fs::remove_file(&record_path)?;
    Ok(RestorePriorOutcome { prior, achieved })
}

/// `target start` preflight: free hugepages must cover the configured
/// DPDK memory (`-s`). Failure is the G3 no-hugepages loud fail — the
/// message carries the arithmetic and names `target setup`.
pub fn preflight_free_for_dpdk(sysfs_dir: &Path, dpdk_mem_mb: u64) -> Result<(), PreflightError> {
    let needed = pages_for_mb(dpdk_mem_mb);
    let snap = read_snapshot(sysfs_dir).map_err(|e| PreflightError {
        message: format!(
            "error: SPDK target stack unavailable: cannot read the 2 MiB hugepage pool \
             ({e})\n  this kernel exposes no {} — SPDK needs hugepages; check kernel \
             config, or select the kernel target stack explicitly:\n    sudo squeezefs \
             nvmeof <verb> … --target-stack nvmet",
            sysfs_dir.display()
        ),
    })?;
    if snap.free < needed {
        return Err(PreflightError {
            message: format!(
                "error: SPDK target stack unavailable: not enough free hugepages for the \
                 configured DPDK memory\n  need {needed} free 2 MiB pages ({dpdk_mem_mb} MiB \
                 via -s), have {free} free of {total} reserved\n  reserve:  sudo squeezefs \
                 nvmeof target setup --hugemem-mb {suggest}\n  (the reservation records the \
                 prior value and is restored by 'target setup --restore-prior')\nnote: \
                 SqueezeFS never falls back between target stacks automatically —\n      \
                 select the kernel target explicitly with --target-stack nvmet if intended.",
                free = snap.free,
                total = snap.total,
                suggest = (needed * HUGEPAGE_2M_MB).max(DEFAULT_HUGEMEM_MB),
            ),
        });
    }
    Ok(())
}

/// `MemAvailable` from `/proc/meminfo` (kB), when readable.
pub fn mem_available_kb() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse::<u64>()
                .ok();
        }
    }
    None
}

/// R2: does a reservation exceed the warn fraction of `MemAvailable`?
pub fn reservation_exceeds_warn_fraction(hugemem_mb: u64, mem_available_kb: u64) -> bool {
    let reservation_kb = hugemem_mb.saturating_mul(1024) as f64;
    reservation_kb > mem_available_kb as f64 * MEM_AVAILABLE_WARN_FRACTION
}
