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
pub mod storage;

use once_cell::sync::Lazy;
use std::sync::RwLock;

pub static FS_PREFIX: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new("squeezefs".to_string()));

pub fn fs_prefix() -> String {
    FS_PREFIX.read().unwrap().clone()
}

pub fn set_fs_prefix(prefix: &str) {
    if !prefix.is_empty() {
        let mut guard = FS_PREFIX.write().unwrap();
        *guard = prefix.to_string();
    }
}

#[macro_export]
macro_rules! fs_key {
    ($suffix:expr) => {
        format!("{}:{}", $crate::fs_prefix(), $suffix)
    };
}
