//! inode based
//!
//! it is not recommend to use this inode based [`Filesystem`] first, you need to handle inode
//! allocate, recycle and sometimes map to the path, [`PathFilesystem`][crate::path::PathFilesystem]
//! helps you do those jobs so you can pay more attention to your filesystem design. However if you
//! want to control the inode or do the path<->inode map on yourself, [`Filesystem`] is the only one
//! choose.

pub use affinity::{pin_scope_from_env, scoped_affinity_cpus, PinScope};
use bytes::Bytes;
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use connection::fuse_over_uring::{
    numa_local_bytes, numa_remote_bytes, over_uring_classical_sideband,
    over_uring_commit_batch_stats, over_uring_geometry, over_uring_negotiated_write,
    over_uring_sessions_active, over_uring_stats, transport_lease_stats, transport_wake_stats,
    COMMIT_BATCH_LABELS,
};
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use connection::kmbuf::{kmbuf_negotiated, zc_replies};
pub use filesystem::Filesystem;
use futures_util::future::Either;
pub use read_phase::{
    read_inplace_replies, read_transport_phase_record, read_transport_phase_snapshot,
    write_inplace_replies, write_transport_phase_record, write_transport_phase_snapshot,
    TransportPhase,
};
pub use request::Request;
#[cfg(feature = "tokio-runtime")]
pub use session::{
    kernel_init_info, negotiated_reply_flags, tpc_lane_redispatches, tpc_spawn, tpc_spawn_on_node,
    tpc_thread_count, KernelInit, MountHandle, Session,
};

pub(crate) type FuseData = Either<
    Vec<u8>,
    (
        Vec<u8>,
        Bytes,
        Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    ),
>;

pub(crate) mod abi;
pub(crate) mod affinity;
pub mod connection;
mod filesystem;
pub mod flags;
pub(crate) mod read_phase;
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
