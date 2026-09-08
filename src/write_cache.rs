//! Mount-time **volatile write cache (VWC)** probe for data volumes
//! (pre-RC engineering spec §1 DUR-2).
//!
//! `O_DIRECT` bypasses the page cache, not the device's volatile write
//! cache. Whether an acknowledged block write survives power loss without
//! a flush therefore depends on hardware the daemon must *probe*, never
//! assume — the same discipline (and the same one-shot sysfs mechanism)
//! as [`crate::meta_backend::atomicity`], whose result is surfaced as
//! `meta_volume_atomicity_physical`. This module's result is surfaced
//! beside it as **`data_volume_write_cache`** and logged at mount.
//!
//! The classification is informational: it never gates a mount. What
//! makes the write path safe regardless is the DUR-2 barrier itself
//! ([`crate::nvme_dev::NvmeBlockDev::flush`]) — a device whose cache is
//! write-through simply pays nothing for it.
//!
//! **Probe I/O mechanism**: one-shot `std::fs::read_to_string` of sysfs
//! attributes, deliberately not routed through `uring_fs` — tiny
//! kernel-generated strings on a once-per-open control path, not
//! data-plane I/O (AGENTS.md scopes the io_uring rule to paths where
//! practical).
//!
//! **Probe scope**: sysfs `queue/write_cache` only. The NVMe identify
//! (`id-ctrl` VWC bit) probe is deferred for exactly the reason the
//! atomicity module defers AWUPF — it needs the `CAP_SYS_ADMIN` ioctl
//! paths this daemon otherwise avoids — and it would add nothing: the
//! kernel derives `queue/write_cache` from that same VWC bit and honors
//! the operator's `write through` override, which the raw identify would
//! miss.

use std::path::Path;

/// A data volume's volatile-write-cache classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCacheClass {
    /// The device advertises a volatile write cache (sysfs
    /// `queue/write_cache == "write back"`): an acknowledged write is NOT
    /// power-safe until a flush completes. The default for essentially
    /// every enterprise and consumer NVMe device.
    WriteBack,
    /// The cache is disabled or non-volatile (`"write through"`): writes
    /// are power-safe on completion; the barrier is a cheap no-op.
    WriteThrough,
    /// Regular file (dev/test substrate): durability rides the host
    /// filesystem's page cache — volatile until an `fsync`.
    FileBacked,
    /// Unreadable attributes or a device node with no `queue/` — never a
    /// guess.
    Unknown,
}

impl WriteCacheClass {
    /// Stats-surface string (dashboard contract — pinned by tests).
    pub fn as_str(&self) -> &'static str {
        match self {
            WriteCacheClass::WriteBack => "write-back",
            WriteCacheClass::WriteThrough => "write-through",
            WriteCacheClass::FileBacked => "file-backed",
            WriteCacheClass::Unknown => "unknown",
        }
    }

    /// Whether acknowledged writes on this class need a device barrier to
    /// be power-safe (`unknown` is treated as volatile — the honest,
    /// conservative reading).
    pub fn is_volatile(&self) -> bool {
        !matches!(self, WriteCacheClass::WriteThrough)
    }

    /// Dense code for an atomic cell (`from_u8` inverts it; anything
    /// out of range reads `Unknown` — never a guess toward power-safe).
    pub fn as_u8(&self) -> u8 {
        match self {
            WriteCacheClass::WriteBack => 0,
            WriteCacheClass::WriteThrough => 1,
            WriteCacheClass::FileBacked => 2,
            WriteCacheClass::Unknown => 3,
        }
    }

    /// Inverse of [`Self::as_u8`].
    pub fn from_u8(code: u8) -> Self {
        match code {
            0 => WriteCacheClass::WriteBack,
            1 => WriteCacheClass::WriteThrough,
            2 => WriteCacheClass::FileBacked,
            _ => WriteCacheClass::Unknown,
        }
    }
}

impl std::fmt::Display for WriteCacheClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pure classification of the sysfs `queue/write_cache` string.
pub fn classify_write_cache(attr: Option<&str>) -> WriteCacheClass {
    match attr.map(|s| s.trim()) {
        Some("write back") => WriteCacheClass::WriteBack,
        Some("write through") => WriteCacheClass::WriteThrough,
        _ => WriteCacheClass::Unknown,
    }
}

/// Locate the block device's `queue/` sysfs directory. Partitions have no
/// `queue/` of their own — fall back to the parent disk's (the
/// `atomicity::queue_dir_for` shape).
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

/// Probe a data volume path's write-cache classification: regular file ⇒
/// `file-backed`; block device ⇒ read sysfs; anything unreadable ⇒
/// `unknown`. Purely informational — the probe never fails an open.
pub fn probe_data_volume(path: &Path) -> WriteCacheClass {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return WriteCacheClass::Unknown;
    };
    if meta.file_type().is_file() {
        return WriteCacheClass::FileBacked;
    }
    if !meta.file_type().is_block_device() {
        return WriteCacheClass::Unknown;
    }
    let Some(queue) = queue_dir_for(meta.rdev()) else {
        return WriteCacheClass::Unknown;
    };
    classify_write_cache(
        std::fs::read_to_string(queue.join("write_cache"))
            .ok()
            .as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_exact_and_never_guesses() {
        assert_eq!(
            classify_write_cache(Some("write back\n")),
            WriteCacheClass::WriteBack
        );
        assert_eq!(
            classify_write_cache(Some("write through\n")),
            WriteCacheClass::WriteThrough
        );
        assert_eq!(classify_write_cache(Some("")), WriteCacheClass::Unknown);
        assert_eq!(classify_write_cache(None), WriteCacheClass::Unknown);
    }

    #[test]
    fn volatility_reading_is_conservative() {
        assert!(WriteCacheClass::WriteBack.is_volatile());
        assert!(WriteCacheClass::FileBacked.is_volatile());
        assert!(WriteCacheClass::Unknown.is_volatile());
        assert!(!WriteCacheClass::WriteThrough.is_volatile());
    }

    #[test]
    fn u8_codec_round_trips_and_out_of_range_is_unknown() {
        for c in [
            WriteCacheClass::WriteBack,
            WriteCacheClass::WriteThrough,
            WriteCacheClass::FileBacked,
            WriteCacheClass::Unknown,
        ] {
            assert_eq!(WriteCacheClass::from_u8(c.as_u8()), c);
        }
        assert_eq!(WriteCacheClass::from_u8(200), WriteCacheClass::Unknown);
        assert!(WriteCacheClass::from_u8(200).is_volatile());
    }
}
