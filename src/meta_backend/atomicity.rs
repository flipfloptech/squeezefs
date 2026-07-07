//! Mount-time sector-atomicity probe (design-wal-crash-consistency §4.6,
//! PR 6, Key Decision 2).
//!
//! The crash contract's D1 row ("per-sector consistency under power loss")
//! holds only if the storage stack writes 4 KiB sectors atomically. This
//! module classifies each metadata volume at mount from sysfs block
//! attributes so the guarantee is *probed*, not assumed: the result is
//! logged, surfaced on the stats inode (`meta_volume_atomicity`), and —
//! with `--strict-meta-atomicity` — enforced as a hard mount gate.
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

use crate::error::{Result, SqueezefsError};
use std::path::Path;

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
    let sector = crate::meta_backend::storage::SECTOR_SIZE as u64;
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
/// anything unreadable ⇒ `unknown` — the probe never fails a mount by
/// itself (that is [`enforce_strict`]'s job, operator opt-in).
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

/// The `--strict-meta-atomicity` gate: classification below `atomic4k` ⇒
/// mount fails loud with the classification in the error. Default off —
/// dev file-backed volumes are the test substrate (§4.6).
pub fn enforce_strict(class: AtomicityClass, path: &Path) -> Result<()> {
    if class == AtomicityClass::Atomic4k {
        return Ok(());
    }
    Err(SqueezefsError::InvalidOperation(format!(
        "Metadata volume {} classifies as '{class}' — below the 'atomic4k' bar required by \
         --strict-meta-atomicity. Use a 4 KiB-LBA / atomic-write-capable device, or drop the \
         strict flag to accept the documented D1/D2 torn-sector exposure.",
        path.display()
    )))
}
