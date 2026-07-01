#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::redundant_closure
)]
pub mod nvme_dev;
pub mod tiering;

pub mod cache;
pub mod config_ops;
pub mod crypto_compress;
pub mod dlm;
pub mod error;
pub mod fuse_client;
pub mod nvmeof;
pub mod p2p;
pub mod recovery;
pub mod routing;

#[macro_export]
macro_rules! coz_progress {
    ($name:expr) => {
        #[cfg(feature = "coz-on")]
        coz::progress!($name);
    };
    () => {
        #[cfg(feature = "coz-on")]
        coz::progress!();
    };
}
pub mod block_allocator;
pub mod defrag;
pub mod jobs;
pub mod storage;

use std::sync::OnceLock;

pub static FS_PREFIX: OnceLock<&'static str> = OnceLock::new();

pub static WRITE_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn write_verification_enabled() -> bool {
    WRITE_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_write_verification(enabled: bool) {
    WRITE_VERIFICATION.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn fs_prefix() -> &'static str {
    *FS_PREFIX.get().unwrap_or(&"squeezefs")
}

pub fn set_fs_prefix(prefix: &str) {
    if !prefix.is_empty() {
        let leaked = Box::leak(prefix.to_string().into_boxed_str());
        // Ignore error if it's already set (OnceLock set can fail if already initialized)
        let _ = FS_PREFIX.set(leaked);
    }
}

#[macro_export]
macro_rules! fs_key {
    ($suffix:expr) => {
        format!("{}:{}", $crate::fs_prefix(), $suffix)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_prefix_default() {
        let prefix = fs_prefix();
        assert!(!prefix.is_empty());
    }
}

