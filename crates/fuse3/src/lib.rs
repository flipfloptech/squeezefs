//! FUSE user-space library async version implementation.
//!
//! This is an improved rewrite of the FUSE user-space library to fully take advantage of Rust's
//! architecture.
//!
//! This library doesn't depend on `libfuse`, unless enable `unprivileged` feature, this feature
//! will support mount the filesystem without root permission by using `fusermount3` binary.
//!
//! # Features:
//!
//! - `file-lock`: enable POSIX file lock feature.
//! - `tokio-runtime`: use [tokio](https://docs.rs/tokio) runtime to drive async io and task.
//! - `unprivileged`: allow mount filesystem without root permission by using `fusermount3`.
//!
//! # Notes:
//!
//! You must enable the `tokio-runtime` feature (the FUSE-over-io_uring transport is tokio-only).

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::io;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use errno::Errno;
pub use helper::{mode_from_kind_and_perm, perm_from_mode_and_kind};
pub use mount_options::MountOptions;
use nix::sys::stat::mode_t;
use raw::abi::{
    fuse_setattr_in, FATTR_ATIME, FATTR_ATIME_NOW, FATTR_CTIME, FATTR_GID, FATTR_KILL_SUIDGID,
    FATTR_LOCKOWNER, FATTR_MODE, FATTR_MTIME, FATTR_MTIME_NOW, FATTR_SIZE, FATTR_UID,
};
pub use raw::set_zc_hold_streaming;
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
pub use raw::{
    drain_group_stats, fast_dispatch_demotes, fast_dispatch_serves, kmbuf_negotiated,
    negotiated_max_readahead, note_zc_write_direct, numa_local_bytes, numa_remote_bytes,
    over_uring_classical_sideband, over_uring_commit_batch_stats, over_uring_geometry,
    over_uring_negotiated_write, over_uring_sessions_active, over_uring_stats,
    retention_negotiated, transport_cq_overflow_stats, transport_lease_stats,
    transport_park_backstop_ticks, transport_reply_integrity_stats, transport_wake_stats,
    zc_bridge_cancels, zc_bridge_lost, zc_bridge_orphans, zc_fallbacks, zc_negotiated,
    zc_release_failures, zc_releases, zc_replies, zc_retain_commits, zc_retain_refused,
    zc_retained_outstanding, zc_slot_payload_skips, zc_write_direct_bytes, zc_write_directs,
    zc_write_extract_bytes, zc_write_extractions, zc_write_fusion_bytes, zc_write_fusion_demotions,
    zc_write_fusions, zc_write_lazy_extractions, zc_write_store_qid_census, COMMIT_BATCH_LABELS,
};
pub use raw::{
    fused_midpass_reaps, fused_passbottom_reaps, fused_timeline_snapshot, pin_scope_from_env,
    read_inplace_replies, read_transport_phase_record, read_transport_phase_snapshot,
    reap_gap_snapshot, reap_phase_record_n, scoped_affinity_cpus, write_inplace_replies,
    write_transport_phase_record, write_transport_phase_snapshot, PhaseSnapshot, PinScope,
    ReapPhase, TransportPhase,
};
#[cfg(feature = "tokio-runtime")]
pub use raw::{tpc_lane_redispatches, tpc_spawn, tpc_spawn_on_node, tpc_thread_count};

mod errno;
mod helper;
// Bench seam (microbench program 2026-08-04): the exact bincode options
// the session hot path serializes headers with — `benches/
// fuse3_hot_bench.rs` must measure the shipping codec, not a lookalike.
#[doc(hidden)]
pub use helper::get_bincode_config;
// Bench seam (FUSE-3g): the per-request delivery-bounds decision every
// dispatch pass runs before a handler sees its body.
#[doc(hidden)]
pub use raw::delivery_body_bounds;
mod mount_options;
pub mod notify;
// N-topology-general NUMA nearest-resource map — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here and by the root crate (the
// `thp.rs`/`wake_core` production-sharing precedent). Both consumers
// build the map from the same sysfs through the same code, so the DENSE
// NODE INDICES agree across the crate boundary by construction (only
// plain `usize` indices ever cross it).
#[path = "../../squeezefs-ipc/src/numa_core.rs"]
pub mod numa_core;
// µs-bucket latency-histogram core — canonical file in the squeezefs-ipc
// tree, `#[path]`-included here (the read_transport_phase_ns rig) and by
// the root crate (LatencyHistogram), so transport- and daemon-side phase
// histograms bucket identically by construction.
#[path = "../../squeezefs-ipc/src/latency_core.rs"]
pub mod latency_core;
// Per-op trace-ring CORE (e2e audit A2) — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here ONLY (the storage lives in
// `raw::op_trace`, and the root crate reaches the one ring set through
// `fuse3::op_trace` rather than a second include — a second `Stage`
// identity would not be accepted by the stamp API) and by `loom-models`
// (the SPSC ring is a lock-free core).
#[path = "../../squeezefs-ipc/src/op_trace_core.rs"]
pub mod op_trace_core;
pub use raw::op_trace;
// The ONE env-knob parsing convention (ENG-10) — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here (this fork cannot depend on
// that crate: it is its own excluded workspace root) and by the root crate
// and the preload shim. Transport knobs obey the same value law as daemon
// knobs, and the shared `numa_core` above resolves `crate::env_knob_core`
// through this include.
#[path = "../../squeezefs-ipc/src/env_knob_core.rs"]
pub mod env_knob_core;
// Per-mount thread-comm suffixes (design-full-multi-writer §5.3, PR 3) —
// same share pattern. This copy's tag STATIC is the fork's own: the
// daemon seeds it explicitly from `writer_scope::set_mount_identity`
// alongside squeezefs-ipc's (an unseeded copy keeps bare names — the
// standalone-suite posture). `sqz_time`/`sqz_fdwatch` below resolve
// `crate::comm_core` through this include.
#[path = "../../squeezefs-ipc/src/comm_core.rs"]
pub mod comm_core;
// The sqz-exec first-party task executor + its loom-modeled delivery
// state word (design-sqz-sync Stage 1b) — canonical files in the
// squeezefs-ipc tree, `#[path]`-included here for the TPC handler-lane
// venue (`sqz_exec` resolves `crate::exec_core` through this pair, the
// `numa_core`/`env_knob_core` pattern). The lanes must not run on the
// tokio scheduler: the Stage-1 field attribution proved the OQ-5 wedge
// class is a task lost inside tokio's delivery, unhealable by any
// future-layer backstop.
#[path = "../../squeezefs-ipc/src/exec_core.rs"]
pub mod exec_core;
pub mod path;
pub mod raw;
// First-party blocking-work offload (the rip-tokio-total sweep) — same
// share pattern; `run_blocking` resolves `crate::sqz_channel` through
// the include below.
#[path = "../../squeezefs-ipc/src/sqz_blocking.rs"]
pub mod sqz_blocking;
// First-party oneshot/mpsc/watch channels (the rip-tokio-total sweep) —
// same share pattern; unbounded parks resolve `crate::sqz_time`'s
// ticked backstop through the include below.
#[path = "../../squeezefs-ipc/src/sqz_channel.rs"]
pub mod sqz_channel;
#[path = "../../squeezefs-ipc/src/sqz_exec.rs"]
pub mod sqz_exec;
// First-party fd-readability watcher (replaces the tokio `AsyncFd`
// reactor dependence in `connection/tokio.rs`) — same share pattern;
// resolves `crate::sqz_channel::ticked` through the include above.
#[path = "../../squeezefs-ipc/src/sqz_fdwatch.rs"]
pub mod sqz_fdwatch;
// First-party biased select (`race2`) — the `tokio::select!` shape the
// InboundQueue pop uses.
#[path = "../../squeezefs-ipc/src/sqz_future.rs"]
pub mod sqz_future;
// First-party Notify with the `notified_raw()`/`enable()` registration
// handle (the enable-then-check pop protocol).
#[path = "../../squeezefs-ipc/src/sqz_notify.rs"]
pub mod sqz_notify;
// First-party async locks (guards held across `.await` are their whole
// point) — canonical files in the ROOT crate's src/ tree, crate-neutral
// (`sqz_sync` resolves `crate::sqz_sync_core` / `crate::sqz_time`
// through these includes).
#[path = "../../../src/sqz_sync.rs"]
pub mod sqz_sync;
// `pub` mirrors the root crate's posture (a private include makes the
// core's test/diagnostic surface read as dead code here).
#[path = "../../../src/sqz_sync_core.rs"]
pub mod sqz_sync_core;
// First-party timer service (the rip-tokio-out sweep, 2026-08-13) —
// same share pattern; `sleep`/`timeout` here never touch a tokio driver.
#[path = "../../squeezefs-ipc/src/sqz_time.rs"]
pub mod sqz_time;

/// Filesystem Inode.
pub type Inode = u64;

/// pre-defined Result, the Err type is [`Errno`].
pub type Result<T> = std::result::Result<T, Errno>;

/// File types
#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum FileType {
    /// Named pipe (S_IFIFO)
    NamedPipe,
    /// Character device (S_IFCHR)
    CharDevice,
    /// Block device (S_IFBLK)
    BlockDevice,
    /// Directory (S_IFDIR)
    Directory,
    /// Regular file (S_IFREG)
    RegularFile,
    /// Symbolic link (S_IFLNK)
    Symlink,
    /// Unix domain socket (S_IFSOCK)
    Socket,
}

impl FileType {
    /// convert [`FileType`] into [`mode_t`]
    pub const fn const_into_mode_t(self) -> mode_t {
        match self {
            FileType::NamedPipe => libc::S_IFIFO,
            FileType::CharDevice => libc::S_IFCHR,
            FileType::BlockDevice => libc::S_IFBLK,
            FileType::Directory => libc::S_IFDIR,
            FileType::RegularFile => libc::S_IFREG,
            FileType::Symlink => libc::S_IFLNK,
            FileType::Socket => libc::S_IFSOCK,
        }
    }
}

impl From<FileType> for mode_t {
    fn from(kind: FileType) -> Self {
        kind.const_into_mode_t()
    }
}

/// the setattr argument.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct SetAttr {
    /// set file or directory mode.
    pub mode: Option<mode_t>,
    /// set file or directory uid.
    pub uid: Option<u32>,
    /// set file or directory gid.
    pub gid: Option<u32>,
    /// set file or directory size.
    pub size: Option<u64>,
    /// the lock_owner argument.
    pub lock_owner: Option<u64>,
    /// set file or directory atime.
    pub atime: Option<Timestamp>,
    /// set file or directory mtime.
    pub mtime: Option<Timestamp>,
    /// set file or directory ctime.
    pub ctime: Option<Timestamp>,
    /// `FATTR_KILL_SUIDGID` (FUSE_HANDLE_KILLPRIV_V2): the handler must
    /// clear S_ISUID, clear S_ISGID only when the file is
    /// group-executable (sgid without group-exec — the
    /// mandatory-locking marker — must be preserved), and drop the
    /// `security.capability` xattr, folded into this SETATTR's commit.
    pub kill_suidgid: bool,
    #[cfg(target_os = "macos")]
    pub crtime: Option<Timestamp>,
    #[cfg(target_os = "macos")]
    pub chgtime: Option<Timestamp>,
    #[cfg(target_os = "macos")]
    pub bkuptime: Option<Timestamp>,
    #[cfg(target_os = "macos")]
    pub flags: Option<u32>,
}

/// Helper for constructing Timestamps from fuse_setattr_in, which sign-casts
/// the seconds.
macro_rules! fsai2ts {
    ( $secs: expr, $nsecs: expr) => {
        Some(Timestamp::new($secs as i64, $nsecs))
    };
}

/// Resolve a `FATTR_{A,M}TIME_NOW` request in the KERNEL'S inode-timestamp
/// clock domain: `CLOCK_REALTIME_COARSE` (what `inode_set_ctime_current()`
/// reads). The fine `CLOCK_REALTIME` runs AHEAD of it by up to a tick, so a
/// fine-resolved "now" could out-rank kernel-authored writeback-cache
/// stamps taken later — a cross-inode timestamp inversion (fstests
/// generic/423 class; see the daemon's `coarse_realtime_ns`).
fn coarse_now_timestamp() -> Timestamp {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with a valid clock id and a valid out pointer;
    // CLOCK_REALTIME_COARSE cannot fail on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) };
    Timestamp::new(t.tv_sec, t.tv_nsec as u32)
}

impl From<&fuse_setattr_in> for SetAttr {
    fn from(setattr_in: &fuse_setattr_in) -> Self {
        let mut set_attr = Self::default();

        if setattr_in.valid & FATTR_MODE > 0 {
            set_attr.mode = Some(setattr_in.mode as mode_t);
        }

        if setattr_in.valid & FATTR_UID > 0 {
            set_attr.uid = Some(setattr_in.uid);
        }

        if setattr_in.valid & FATTR_GID > 0 {
            set_attr.gid = Some(setattr_in.gid);
        }

        if setattr_in.valid & FATTR_SIZE > 0 {
            set_attr.size = Some(setattr_in.size);
        }

        if setattr_in.valid & FATTR_ATIME > 0 {
            set_attr.atime = fsai2ts!(setattr_in.atime, setattr_in.atimensec);
        }

        if setattr_in.valid & FATTR_ATIME_NOW > 0 {
            set_attr.atime = Some(coarse_now_timestamp());
        }

        if setattr_in.valid & FATTR_MTIME > 0 {
            set_attr.mtime = fsai2ts!(setattr_in.mtime, setattr_in.mtimensec);
        }

        if setattr_in.valid & FATTR_MTIME_NOW > 0 {
            set_attr.mtime = Some(coarse_now_timestamp());
        }

        if setattr_in.valid & FATTR_LOCKOWNER > 0 {
            set_attr.lock_owner = Some(setattr_in.lock_owner);
        }

        if setattr_in.valid & FATTR_CTIME > 0 {
            set_attr.ctime = fsai2ts!(setattr_in.ctime, setattr_in.ctimensec);
        }

        if setattr_in.valid & FATTR_KILL_SUIDGID > 0 {
            set_attr.kill_suidgid = true;
        }

        set_attr
    }
}

/// A file's timestamp, according to FUSE.
///
/// Nearly the same as a `libc::timespec`, except for the width of the nsec
/// field.
// Could implement From for Duration, and/or libc::timespec, if desired
#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

impl Timestamp {
    /// Create a new timestamp from its component parts.
    ///
    /// `nsec` should be less than 1_000_000_000.
    pub fn new(sec: i64, nsec: u32) -> Self {
        Timestamp { sec, nsec }
    }
}

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        let d = t
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        Timestamp {
            sec: d.as_secs().try_into().unwrap_or(i64::MAX),
            nsec: d.subsec_nanos(),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "unprivileged"))]
fn find_fusermount3() -> io::Result<PathBuf> {
    which::which("fusermount3")
        .map_err(|err| io::Error::other(format!("find fusermount3 binary failed {err:?}")))
}

#[cfg(test)]
mod setattr_killpriv_tests {
    use bincode::Options;

    use super::*;
    use crate::helper::get_bincode_config;
    use crate::raw::abi::{fuse_setattr_in, FATTR_KILL_SUIDGID, FATTR_SIZE};

    /// Wire-shaped `fuse_setattr_in` (Linux layout, 88 bytes): only
    /// `valid` and `size` populated — everything else zero.
    fn setattr_in_bytes(valid: u32, size: u64) -> Vec<u8> {
        let mut b = Vec::with_capacity(88);
        b.extend_from_slice(&valid.to_le_bytes()); // valid
        b.extend_from_slice(&0u32.to_le_bytes()); // _padding
        b.extend_from_slice(&0u64.to_le_bytes()); // fh
        b.extend_from_slice(&size.to_le_bytes()); // size
        for _ in 0..4 {
            b.extend_from_slice(&0u64.to_le_bytes()); // lock_owner, atime, mtime, ctime
        }
        for _ in 0..8 {
            b.extend_from_slice(&0u32.to_le_bytes()); // *nsec, mode, unused4, uid, gid, unused5
        }
        b
    }

    /// FUSE_HANDLE_KILLPRIV_V2 contract (killpriv campaign): a
    /// size-changing SETATTR from a non-CAP_FSETID caller carries
    /// `FATTR_KILL_SUIDGID` — the daemon (not the kernel) must clear
    /// suid / group-exec sgid / security.capability. The bit must
    /// surface on [`SetAttr`] or the handler can never honor it.
    #[test]
    fn setattr_in_fattr_kill_suidgid_surfaces_on_set_attr() {
        let bytes = setattr_in_bytes(FATTR_SIZE | FATTR_KILL_SUIDGID, 0);
        let setattr_in: fuse_setattr_in = get_bincode_config()
            .deserialize(&bytes)
            .expect("wire-shaped fuse_setattr_in decodes");
        let set_attr = SetAttr::from(&setattr_in);
        assert_eq!(set_attr.size, Some(0), "FATTR_SIZE still maps");
        assert!(
            set_attr.kill_suidgid,
            "FATTR_KILL_SUIDGID must surface as SetAttr::kill_suidgid"
        );

        // And absent ⇒ false (an unflagged truncate must clear nothing).
        let bytes = setattr_in_bytes(FATTR_SIZE, 4096);
        let setattr_in: fuse_setattr_in = get_bincode_config()
            .deserialize(&bytes)
            .expect("wire-shaped fuse_setattr_in decodes");
        let set_attr = SetAttr::from(&setattr_in);
        assert!(
            !set_attr.kill_suidgid,
            "kill_suidgid must be false when the kernel did not flag it"
        );
    }
}
