use std::io;

#[cfg(feature = "tokio-runtime")]
pub use tokio::FuseConnection;

#[cfg(feature = "tokio-runtime")]
mod tokio;

#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub mod fuse_over_uring;

pub(crate) type CompleteIoResult<T, U> = (T, io::Result<U>);
