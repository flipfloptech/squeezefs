#![allow(clippy::all)]
// The stats-inode `serde_json::json!` literal exceeds the default macro
// recursion limit (128) — compile-time only, no runtime effect.
#![recursion_limit = "512"]
pub mod mem_budget;
pub mod nvme_dev;
pub mod tiering;

pub mod bench;
pub mod bg_admit;
pub mod cache;
pub mod config_ops;
pub(crate) mod cow_core;
pub mod cpu;
pub mod crypto_compress;
pub mod dlm;
pub mod error;
pub mod fuse_client;
pub(crate) mod gauge_core;
pub mod health;
pub(crate) mod incarnation_core;
pub mod meta_backend;
pub mod nvmeof;
pub mod p2p;
pub(crate) mod patch_clone_core;
pub mod recovery;
pub(crate) mod refcount_core;
pub mod routing;

#[macro_export]
macro_rules! coz_progress {
    ($name:expr) => {
        #[cfg(all(feature = "coz-on", not(test)))]
        coz::progress!($name);
    };
    () => {
        #[cfg(all(feature = "coz-on", not(test)))]
        coz::progress!();
    };
}
pub mod block_allocator;
pub mod defrag;
pub mod jobs;
pub mod storage;
pub mod stripe_locks;
pub mod supervisor;
pub mod uring_fs;

use parking_lot::RwLock;

pub static FS_PREFIX: RwLock<&'static str> = RwLock::new("squeezefs");

pub static WRITE_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// When write verification is enabled, check every N-th write (P2-9).
/// `1` = verify every write (historical default).
static WRITE_VERIFICATION_SAMPLE_N: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

static WRITE_VERIFICATION_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn write_verification_enabled() -> bool {
    WRITE_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_write_verification(enabled: bool) {
    WRITE_VERIFICATION.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// How often enabled write-verification issues a read-after-write.
/// `every_n == 1` verifies all writes; larger N samples roughly 1/N of writes.
pub fn set_write_verification_sample_rate(every_n: u64) {
    WRITE_VERIFICATION_SAMPLE_N.store(every_n.max(1), std::sync::atomic::Ordering::Relaxed);
}

pub fn write_verification_sample_rate() -> u64 {
    WRITE_VERIFICATION_SAMPLE_N
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1)
}

/// Whether this write should run read-after-write verification (enabled + sample).
#[inline]
pub fn write_verification_should_check() -> bool {
    if !write_verification_enabled() {
        return false;
    }
    let n = write_verification_sample_rate();
    if n <= 1 {
        return true;
    }
    WRITE_VERIFICATION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n == 0
}

pub fn fs_prefix() -> &'static str {
    *FS_PREFIX.read()
}

pub fn set_fs_prefix(prefix: &str) {
    if !prefix.is_empty() {
        let leaked = Box::leak(prefix.to_string().into_boxed_str());
        *FS_PREFIX.write() = leaked;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FsKey(pub compact_str::CompactString);

impl std::ops::Deref for FsKey {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl AsRef<str> for FsKey {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<[u8]> for FsKey {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Display for FsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn build_fs_key(suffix: &str) -> FsKey {
    let prefix = fs_prefix();
    let mut s = compact_str::CompactString::with_capacity(prefix.len() + 1 + suffix.len());
    s.push_str(prefix);
    s.push(':');
    s.push_str(suffix);
    FsKey(s)
}

#[macro_export]
macro_rules! fs_key {
    ($suffix:expr) => {
        $crate::build_fs_key(&$suffix)
    };
}

/// Garnet/Redis key construction for the hot path (P2-1 / P2-2).
///
/// # Namespace convention
///
/// - **Volume keys** (format, free_blocks, used_bytes, attr, dir, job sets, …):
///   always under [`fs_prefix`] via [`fs_key!`] / [`build_fs_key`] / [`keys::attr`].
/// - **Layout keys** (per-file metadata, inline payload, block maps, staging maps,
///   active-block buffers): **unprefixed** historical names
///   (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`).
///   Do **not** put these under `FS_PREFIX` without an on-disk format migration —
///   existing volumes already store the unprefixed forms.
///
/// Prefer these helpers over ad-hoc `format!(…)` so key shape stays consistent and
/// we avoid redundant intermediate `String`s on the FUSE write/read path.
pub mod keys {
    use super::FsKey;
    use compact_str::CompactString;
    use std::fmt::Write;

    /// Logical file path used as the in-process cache / layout identity: `inode_{ino}`.
    ///
    /// Returns [`String`] so it plugs into existing `String`-keyed caches (moka/scc)
    /// without extra conversion noise at call sites.
    #[inline]
    pub fn inode_path(ino: u64) -> String {
        format!("inode_{ino}")
    }

    /// Layout meta hash key: `metadata:inode_{ino}`.
    #[inline]
    pub fn metadata_for_inode(ino: u64) -> FsKey {
        let mut s = CompactString::with_capacity(20);
        let _ = write!(s, "metadata:inode_{ino}");
        FsKey(s)
    }

    /// Layout meta hash key for a path that is already `inode_N` (or similar):
    /// `metadata:{file_path}`.
    #[inline]
    pub fn metadata_for_path(file_path: &str) -> FsKey {
        let mut s = CompactString::with_capacity(10 + file_path.len());
        s.push_str("metadata:");
        s.push_str(file_path);
        FsKey(s)
    }

    /// Inline payload key: `inline_data:{file_path}`.
    #[inline]
    pub fn inline_data(file_path: &str) -> FsKey {
        let mut s = CompactString::with_capacity(12 + file_path.len());
        s.push_str("inline_data:");
        s.push_str(file_path);
        FsKey(s)
    }

    /// Block-map hash key: `block_map:{block_map_id}`.
    #[inline]
    pub fn block_map(block_map_id: &str) -> FsKey {
        let mut s = CompactString::with_capacity(10 + block_map_id.len());
        s.push_str("block_map:");
        s.push_str(block_map_id);
        FsKey(s)
    }

    /// Staged-file mapping hash key: `mapping:{file_id}`.
    #[inline]
    pub fn mapping(file_id: &str) -> FsKey {
        let mut s = CompactString::with_capacity(8 + file_id.len());
        s.push_str("mapping:");
        s.push_str(file_id);
        FsKey(s)
    }

    /// Active-block buffer / staging key: `active_block:inode_{ino}:block_{block}`.
    #[inline]
    pub fn active_block(ino: u64, block: u64) -> FsKey {
        let mut s = CompactString::with_capacity(40);
        let _ = write!(s, "active_block:inode_{ino}:block_{block}");
        FsKey(s)
    }

    /// Active-block key when `file_path` is already `inode_N`:
    /// `active_block:{file_path}:block_{block}`.
    #[inline]
    pub fn active_block_for_path(file_path: &str, block: u32) -> FsKey {
        let mut s = CompactString::with_capacity(24 + file_path.len());
        let _ = write!(s, "active_block:{file_path}:block_{block}");
        FsKey(s)
    }

    /// Scan prefix for an inode's active blocks: `active_block:inode_{ino}:`.
    #[inline]
    pub fn active_block_ino_prefix(ino: u64) -> CompactString {
        let mut s = CompactString::with_capacity(28);
        let _ = write!(s, "active_block:inode_{ino}:");
        s
    }

    /// Scan prefix when `file_path` is already `inode_N`: `active_block:{file_path}:`.
    #[inline]
    pub fn active_block_path_prefix(file_path: &str) -> CompactString {
        let mut s = CompactString::with_capacity(14 + file_path.len());
        s.push_str("active_block:");
        s.push_str(file_path);
        s.push(':');
        s
    }

    /// POSIX attr hash under the volume prefix: `{fs_prefix}:attr:{ino}`.
    #[inline]
    pub fn attr(ino: u64) -> FsKey {
        let prefix = super::fs_prefix();
        let mut s = CompactString::with_capacity(prefix.len() + 20);
        let _ = write!(s, "{prefix}:attr:{ino}");
        FsKey(s)
    }

    /// Directory listing hash under the volume prefix: `{fs_prefix}:dir:{ino}`.
    #[inline]
    pub fn dir(ino: u64) -> FsKey {
        let prefix = super::fs_prefix();
        let mut s = CompactString::with_capacity(prefix.len() + 20);
        let _ = write!(s, "{prefix}:dir:{ino}");
        FsKey(s)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct FormatConfig {
    pub name: String,
    pub block_size: u64,
    pub capacity: u64,
    pub inodes: u64,
    pub compression: String,
    pub encrypt_algo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_cache_paths: Option<Vec<std::path::PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_lv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismount_wait: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_delay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuse_io_uring_sqpoll_idle_ms: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_prefix_default() {
        let prefix = fs_prefix();
        assert!(!prefix.is_empty());
    }

    #[test]
    fn test_write_verification_sample_gate() {
        set_write_verification(false);
        set_write_verification_sample_rate(1);
        assert!(!write_verification_should_check());

        set_write_verification(true);
        set_write_verification_sample_rate(1);
        assert!(write_verification_should_check());
        assert!(write_verification_should_check());

        set_write_verification_sample_rate(5);
        let mut hits = 0usize;
        for _ in 0..50 {
            if write_verification_should_check() {
                hits += 1;
            }
        }
        // Roughly 1/5 of 50 = 10; allow wide band for counter phase.
        assert!(
            (5..=20).contains(&hits),
            "expected ~10 sample hits in 50, got {hits}"
        );

        // Restore defaults so other tests are not affected.
        set_write_verification(false);
        set_write_verification_sample_rate(1);
    }

    #[test]
    fn test_layout_key_shapes_match_historical_format() {
        assert_eq!(keys::inode_path(42), "inode_42");
        assert_eq!(&*keys::metadata_for_inode(42), "metadata:inode_42");
        assert_eq!(&*keys::metadata_for_path("inode_7"), "metadata:inode_7");
        assert_eq!(
            &*keys::metadata_for_path(&keys::inode_path(7)),
            &*keys::metadata_for_inode(7)
        );
        assert_eq!(&*keys::inline_data("inode_1"), "inline_data:inode_1");
        assert_eq!(&*keys::block_map("abc"), "block_map:abc");
        assert_eq!(&*keys::mapping("fid"), "mapping:fid");
        assert_eq!(&*keys::active_block(9, 3), "active_block:inode_9:block_3");
        assert_eq!(
            &*keys::active_block_for_path("inode_9", 3),
            &*keys::active_block(9, 3)
        );
        assert_eq!(
            keys::active_block_ino_prefix(9).as_str(),
            "active_block:inode_9:"
        );
        assert_eq!(
            keys::active_block_path_prefix("inode_9").as_str(),
            "active_block:inode_9:"
        );
    }

    #[test]
    fn test_volume_attr_dir_use_fs_prefix() {
        let prefix = fs_prefix();
        assert_eq!(&*keys::attr(1), format!("{prefix}:attr:1"));
        assert_eq!(&*keys::dir(1), format!("{prefix}:dir:1"));
        // Same shape as fs_key! for attr suffix
        assert_eq!(&*keys::attr(1), &*build_fs_key("attr:1"));
    }
}
