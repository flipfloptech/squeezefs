//! zcrx read lane — userspace NVMe/TCP initiator for cold read fills
//! (`docs/design-zcrx-read-lane.md`; Phase-1 bracket
//! `.benchmarks/2026-08-03-zcrx-lane.md`).
//!
//! Arm law (design §6/§7): **opt-in** (`SQUEEZEFS_ZCRX_LANE=1`) and
//! capability-probed per device; any gate failing leaves today's kernel
//! block path byte-identical. PR Z1 ships the initiator + probes + gauges
//! with the classic-recv contract backend (`SQUEEZEFS_ZCRX_LANE_FORCE_COPY=1`
//! — a test/measurement seam, never a product posture); the zcrx recv
//! backend (RECV_ZC + REGISTER_ZCRX_IFQ area serve) is PR Z2 and until it
//! lands a plain `SQUEEZEFS_ZCRX_LANE=1` mount logs the refusal loud and
//! stays on the kernel path.

pub mod initiator;
pub mod pdu;
pub mod probe;

pub use initiator::{LaneSession, LaneTarget};

use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Opt-in master switch (design §6).
pub fn lane_env_armed() -> bool {
    std::env::var("SQUEEZEFS_ZCRX_LANE").is_ok_and(|v| v == "1")
}

/// Test/measurement seam: allow the classic-recv backend to arm (copy-parity
/// with the kernel path — the PR Z1 contract venue; counted, never a win).
pub fn lane_force_copy_backend() -> bool {
    std::env::var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY").is_ok_and(|v| v == "1")
}

/// Dev/test target override: `traddr,trsvcid,subnqn,nsid,lba_shift,max_xfer`
/// (+ optional `,io_queues,queue_depth`) — lets contract suites and the
/// devsub-tcp rig aim the lane at an arbitrary NVMe/TCP endpoint without a
/// sysfs attachment. Product mounts never set it.
pub fn lane_target_override() -> Option<LaneTarget> {
    let raw = std::env::var("SQUEEZEFS_ZCRX_LANE_TARGET").ok()?;
    let f: Vec<&str> = raw.split(',').collect();
    if f.len() < 6 {
        log::error!("SQUEEZEFS_ZCRX_LANE_TARGET malformed ({raw:?}) — lane not armed");
        return None;
    }
    Some(LaneTarget {
        traddr: f[0].to_string(),
        trsvcid: f[1].to_string(),
        subnqn: f[2].to_string(),
        nsid: f[3].parse().ok()?,
        lba_shift: f[4].parse().ok()?,
        max_xfer_bytes: f[5].parse().ok()?,
        io_queues: f.get(6).and_then(|v| v.parse().ok()).unwrap_or(1),
        queue_depth: f.get(7).and_then(|v| v.parse().ok()).unwrap_or(8),
    })
}

/// The per-device arm decision + association bring-up. `None` = today's
/// path (every refusal below is deliberate and, where surprising, loud).
pub async fn arm_for_device(device_path: &str) -> Option<Arc<LaneSession>> {
    if !lane_env_armed() {
        return None;
    }
    let target = match lane_target_override() {
        Some(t) => t,
        None => probe::nvme_tcp_target_for(device_path)?,
    };
    if !lane_force_copy_backend() {
        // PR Z1: the zero-copy recv backend is not shipped yet. Refuse to
        // arm rather than silently adding a copy lane (design §6).
        if !probe::recv_zc_supported() {
            log::warn!(
                "zcrx-lane: SQUEEZEFS_ZCRX_LANE=1 but this kernel lacks IORING_OP_RECV_ZC \
                 — lane not armed for {device_path}; kernel path serves (byte-identical)"
            );
        } else {
            log::warn!(
                "zcrx-lane: kernel supports RECV_ZC but the zcrx recv backend is PR Z2 \
                 — lane not armed for {device_path}; kernel path serves \
                 (SQUEEZEFS_ZCRX_LANE_FORCE_COPY=1 arms the classic contract backend)"
            );
        }
        return None;
    }
    match LaneSession::connect(target).await {
        Ok(sess) => {
            crate::fuse_client::METRICS
                .zcrx_lane_armed
                .store(1, Ordering::Relaxed);
            Some(sess)
        }
        Err(e) => {
            crate::fuse_client::METRICS
                .zcrx_conn_errors
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "zcrx-lane: arm failed for {device_path}: {e} — kernel path serves \
                 (byte-identical)"
            );
            None
        }
    }
}
