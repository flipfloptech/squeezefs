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
pub use connection::fuse_over_uring::fused::{
    zc_write_fusion_bytes, zc_write_fusion_demotions, zc_write_fusions, zc_write_lazy_extractions,
    zc_write_place_fallbacks, zc_write_placement_bytes, zc_write_placements,
};
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use connection::fuse_over_uring::{
    drain_group_stats, negotiated_max_readahead, numa_local_bytes, numa_remote_bytes,
    over_uring_classical_sideband, over_uring_commit_batch_stats, over_uring_geometry,
    over_uring_negotiated_write, over_uring_sessions_active, over_uring_stats,
    transport_cq_overflow_stats, transport_lease_stats, transport_reply_integrity_stats,
    transport_wake_stats, COMMIT_BATCH_LABELS,
};
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use connection::kmbuf::{
    kmbuf_negotiated, note_zc_write_direct, zc_bridge_cancels, zc_bridge_lost, zc_fallbacks,
    zc_negotiated, zc_replies, zc_slot_payload_skips, zc_write_direct_bytes, zc_write_directs,
    zc_write_extract_bytes, zc_write_extractions, zc_write_store_qid_census,
};
pub use connection::zc::set_zc_hold_streaming;
pub use filesystem::Filesystem;
use futures_util::future::Either;
pub use read_phase::{
    read_inplace_replies, read_transport_phase_record, read_transport_phase_snapshot,
    write_inplace_replies, write_transport_phase_record, write_transport_phase_snapshot,
    TransportPhase,
};
pub use request::{ReplySlot, Request};
#[cfg(feature = "tokio-runtime")]
// Bench seam (FUSE-3g): the per-request delivery-bounds decision.
#[doc(hidden)]
pub use session::delivery_body_bounds;
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

/// One reply travelling to the reply task: the serialized bytes plus the
/// **slot they must be committed against** (FUSE-2 ⊕ PERF-16).
///
/// Carrying the address with the reply is what replaced the transport's
/// sharded `unique → (qid, ent_idx, commit_id)` map: the reply task no
/// longer asks "where does this unique live?" (three sharded-mutex
/// acquisitions per request), it commits against the slot the request
/// was delivered on.
pub(crate) struct FuseReply {
    pub(crate) data: FuseData,
    pub(crate) slot: request::ReplySlot,
}

// `pub` + doc(hidden) for the microbench program (2026-08-04): the
// transport bench (`benches/fuse3_hot_bench.rs`) measures the per-op
// hot-struct codec cost (fuse_in_header decode, out-header + attr/entry/
// write-out encode). Not a stable API surface.
#[doc(hidden)]
pub mod abi;
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
