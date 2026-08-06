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

/// FUSE_URING_ZERO_COPY serve integration (K1 kill, 2026-08-06): the
/// sparse-slot bridge machinery (bounce arena, opcode mirror, pending
/// table) — rides the kmbuf module's carried-series ABI boundary.
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub mod zc;

pub(crate) type CompleteIoResult<T, U> = (T, io::Result<U>);
