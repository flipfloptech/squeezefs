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

use parking_lot::RwLock;

pub static FS_PREFIX: RwLock<&'static str> = RwLock::new("squeezefs");

pub static WRITE_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn write_verification_enabled() -> bool {
    WRITE_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_write_verification(enabled: bool) {
    WRITE_VERIFICATION.store(enabled, std::sync::atomic::Ordering::Relaxed);
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

impl redis::ToRedisArgs for FsKey {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + redis::RedisWrite,
    {
        self.0.as_str().write_redis_args(out)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_prefix_default() {
        let prefix = fs_prefix();
        assert!(!prefix.is_empty());
    }
}
