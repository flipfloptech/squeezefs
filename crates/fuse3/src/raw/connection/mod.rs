use std::io;

#[cfg(feature = "tokio-runtime")]
pub use tokio::FuseConnection;

#[cfg(feature = "tokio-runtime")]
mod tokio;

#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub mod fuse_over_uring;

/// kmbuf / FUSE-zc adoption surface (2026-08-04 campaign) — THE
/// severable module boundary for the carried-series ABI (see its module
/// doc for the upstream-drop/re-port expectation).
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub mod kmbuf;

pub(crate) type CompleteIoResult<T, U> = (T, io::Result<U>);
