#![allow(clippy::all)]
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

use std::sync::Mutex;

pub static FS_PREFIX: Mutex<Option<&'static str>> = Mutex::new(None);

pub static WRITE_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn write_verification_enabled() -> bool {
    WRITE_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_write_verification(enabled: bool) {
    WRITE_VERIFICATION.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn fs_prefix() -> &'static str {
    FS_PREFIX.lock().unwrap().unwrap_or("squeezefs")
}

pub fn set_fs_prefix(prefix: &str) {
    if !prefix.is_empty() {
        let leaked = Box::leak(prefix.to_string().into_boxed_str());
        *FS_PREFIX.lock().unwrap() = Some(leaked);
    }
}

pub fn build_fs_key(suffix: &str) -> String {
    let prefix = fs_prefix();
    let mut s = String::with_capacity(prefix.len() + 1 + suffix.len());
    s.push_str(prefix);
    s.push(':');
    s.push_str(suffix);
    s
}

#[macro_export]
macro_rules! fs_key {
    ($suffix:expr) => {
        $crate::build_fs_key(&$suffix)
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
