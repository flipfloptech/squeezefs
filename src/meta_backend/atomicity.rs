//! Mount-time sector-atomicity probe (design-wal-crash-consistency §4.6,
//! PR 6, Key Decision 2).
//!
//! Sector atomicity is *probed*, not assumed: this module classifies each
//! metadata volume at mount from sysfs block attributes; the result is
//! logged and surfaced on the stats inode as
//! `meta_volume_atomicity_physical` (operator hardware visibility). The
//! metadata **contract** does not depend on it — v3 volumes are
//! copy-on-write and checksummed by construction
//! ([`META_VOLUME_ATOMICITY_COW`]), which is why the retired
//! `--strict-meta-atomicity` gate (it only ever gated v2 volumes) was
//! deleted with v2 support.
//!
//! **Probe I/O mechanism**: one-shot `std::fs::read_to_string` of sysfs
//! attributes, deliberately *not* routed through `uring_fs` — these are
//! tiny synchronous kernel-generated strings on a once-per-mount control
//! path, not data-plane I/O the kernel can accelerate; pinning sysfs fds
//! in the uring workers' fd cache would buy nothing while costing cache
//! slots (`AGENTS.md` scopes the io_uring rule to paths where practical).
//!
//! **Probe scope**: sysfs only (resolved Open Question 5). The NVMe
//! identify (AWUPF) ioctl probe is explicitly deferred — it requires
//! CAP_SYS_ADMIN paths this daemon otherwise avoids. On kernels without
//! `queue/atomic_write_unit_max_bytes` (< 6.11), classification honestly
//! tops out at [`AtomicityClass::Likely`]; the surface can be extended
//! later without format or contract change.

use std::path::Path;

/// The `meta_volume_atomicity` **contract class** (resolved OQ 2,
/// design-cow-kv-metadata §4.10): torn-write immunity holds by
/// construction (every unit checksummed, never-overwrite-live), so the
/// contract field reports `cow-checksummed` regardless of hardware; the
/// physical probe classification is surfaced alongside as
/// `meta_volume_atomicity_physical` for operator hardware visibility.
pub const META_VOLUME_ATOMICITY_COW: &str = "cow-checksummed";

/// Classification of a metadata volume's 4 KiB write-atomicity evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicityClass {
    /// The device advertises whole-sector atomicity: logical block size
    /// ≥ 4096, or the kernel's atomic-write unit (≥ 6.11) covers 4096.
    /// The D1 contract row applies as written.
    Atomic4k,
    /// 512-byte logical over a ≥ 4096 physical block ("512e"): the
    /// firmware almost certainly writes 4 KiB internally, but nothing on
    /// the software path *promises* it.
    Likely,
    /// No 4 KiB-atomicity evidence (512n) or unreadable attributes. Never
    /// a guess.
    Unknown,
    /// Regular file (dev/test substrate): page-cache writeback tears are
    /// possible on power loss — the D2 row.
    FileBacked,
}

impl AtomicityClass {
    /// Stats-surface string (pinned by tests — dashboard contract).
    pub fn as_str(&self) -> &'static str {
        match self {
            AtomicityClass::Atomic4k => "atomic4k",
            AtomicityClass::Likely => "likely",
            AtomicityClass::Unknown => "unknown",
            AtomicityClass::FileBacked => "file-backed",
        }
    }
}

impl std::fmt::Display for AtomicityClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pure classification from the three sysfs block attributes (§4.6).
pub fn classify_block_attrs(
    logical_block_size: Option<u64>,
    physical_block_size: Option<u64>,
    atomic_write_unit_max: Option<u64>,
) -> AtomicityClass {
    let sector = crate::meta_backend::kv::superblock::SECTOR_SIZE as u64;
    if logical_block_size.is_some_and(|l| l >= sector)
        || atomic_write_unit_max.is_some_and(|a| a >= sector)
    {
        return AtomicityClass::Atomic4k;
    }
    if physical_block_size.is_some_and(|p| p >= sector) {
        return AtomicityClass::Likely;
    }
    AtomicityClass::Unknown
}

/// Read one numeric sysfs attribute; `None` on any failure.
fn read_sysfs_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Locate the block device's `queue/` sysfs directory. Partitions have no
/// `queue/` of their own — fall back to the parent disk's.
fn queue_dir_for(rdev: u64) -> Option<std::path::PathBuf> {
    let major = libc::major(rdev);
    let minor = libc::minor(rdev);
    let dev_dir = std::path::PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    let direct = dev_dir.join("queue");
    if direct.is_dir() {
        return Some(direct);
    }
    let parent = dev_dir.join("..").join("queue");
    if parent.is_dir() {
        return Some(parent);
    }
    None
}

/// Probe a metadata volume path's atomicity classification (§4.6):
/// regular file ⇒ `file-backed`; block device ⇒ classify from sysfs;
/// anything unreadable ⇒ `unknown`. Purely informational — the probe
/// never fails a mount (the v3 contract holds by construction,
/// [`META_VOLUME_ATOMICITY_COW`]).
pub fn probe_meta_volume(path: &Path) -> AtomicityClass {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return AtomicityClass::Unknown;
    };
    if meta.file_type().is_file() {
        return AtomicityClass::FileBacked;
    }
    if !meta.file_type().is_block_device() {
        return AtomicityClass::Unknown;
    }
    let Some(queue) = queue_dir_for(meta.rdev()) else {
        return AtomicityClass::Unknown;
    };
    classify_block_attrs(
        read_sysfs_u64(&queue.join("logical_block_size")),
        read_sysfs_u64(&queue.join("physical_block_size")),
        // Kernels >= 6.11; absent elsewhere (classification tops out at
        // `likely` there — resolved Open Question 5).
        read_sysfs_u64(&queue.join("atomic_write_unit_max_bytes")),
    )
}
