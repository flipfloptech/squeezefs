//! inode based
//!
//! it is not recommend to use this inode based [`Filesystem`] first, you need to handle inode
//! allocate, recycle and sometimes map to the path, [`PathFilesystem`][crate::path::PathFilesystem]
//! helps you do those jobs so you can pay more attention to your filesystem design. However if you
//! want to control the inode or do the path<->inode map on yourself, [`Filesystem`] is the only one
//! choose.

use bytes::Bytes;
pub use filesystem::Filesystem;
use futures_util::future::Either;
pub use request::Request;
#[cfg(any(feature = "async-io-runtime", feature = "tokio-runtime"))]
pub use session::{MountHandle, Session, tpc_spawn, tpc_thread_count};
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use connection::fuse_over_uring::{over_uring_sessions_active, over_uring_stats};

pub(crate) type FuseData = Either<Vec<u8>, (Vec<u8>, Bytes, Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>)>;

pub(crate) mod abi;
mod connection;
mod filesystem;
pub mod flags;
pub mod reply;
mod request;
pub(crate) mod session;

pub mod prelude {
    pub use super::reply::FileAttr;
    pub use super::reply::*;
    pub use super::Filesystem;
    pub use super::Request;
    pub use super::Session;
    pub use crate::notify::Notify;
    pub use crate::FileType;
    pub use crate::SetAttr;
}
