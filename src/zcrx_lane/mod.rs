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

pub mod area;
pub mod area_core;
mod area_queue;
mod ethtool;
mod ethtool_nl;
pub mod fill_table;
pub mod initiator;
pub mod nic_census;
pub mod pdu;
pub mod pdu_stream;
pub mod probe;
pub mod rxq_alloc;
pub mod steering;
mod uring_zcrx;

pub use initiator::{LaneBackend, LaneSession, LaneTarget};
pub use uring_zcrx::park_class_name;

use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Every session this process armed (Weak — the per-device OnceCells own
/// the Arcs). Registered at connect; pruned on read.
static SESSIONS: std::sync::LazyLock<std::sync::Mutex<Vec<std::sync::Weak<LaneSession>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

pub(crate) fn register_session(sess: &Arc<LaneSession>) {
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Arc::downgrade(sess));
}

/// Live (not-torn-down) lane sessions in this process — the finding-F
/// teardown instrument (tests + the shutdown ladder's log line).
pub fn live_lane_sessions() -> usize {
    let mut reg = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    reg.retain(|w| w.strong_count() > 0);
    reg.iter()
        .filter_map(|w| w.upgrade())
        .filter(|s| !s.torn_down())
        .count()
}

/// Orderly teardown of EVERY live lane session (stop → join → restore →
/// release leases) — the daemon-shutdown hook (finding F: sessions live
/// in per-device OnceCell statics, which never drop at process exit, so
/// ArmedSteering's Drop convergence is structurally unreachable on the
/// NORMAL exit path without this; kill-9 stays the documented residue).
pub async fn teardown_all_lanes() {
    let live: Vec<Arc<LaneSession>> = {
        let reg = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        reg.iter().filter_map(|w| w.upgrade()).collect()
    };
    if live.is_empty() {
        return;
    }
    log::info!(
        "zcrx-lane: shutdown teardown — {} session(s) to quiesce/restore",
        live.len()
    );
    for sess in live {
        sess.teardown().await;
    }
}

/// The whole-NIC release (round 3 — the majority-structural sweep): a
/// structurally-torn session whose census vote made the MAJORITY tears
/// the NIC's remaining live sessions down too, so their RSS width
/// restores MID-ROW instead of at umount (round-2 field: the surviving
/// minority held ~16 % of queue width at ~0 engagement for the rest of
/// the row). `except` is the sweeping session's identity (it is already
/// inside its own teardown). Awaited — the caller's teardown completes
/// only when the NIC is released; peers' teardowns are idempotent, and
/// a swept peer is never structural itself, so recursion ends at depth
/// one (Box::pin breaks the async cycle for the compiler).
pub(crate) async fn sweep_nic_sessions(ifindex: u32, except: usize) {
    let peers: Vec<Arc<LaneSession>> = {
        let reg = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .filter_map(|w| w.upgrade())
            .filter(|s| s.nic_ifindex() == Some(ifindex))
            .filter(|s| Arc::as_ptr(s) as usize != except)
            .filter(|s| !s.torn_down())
            .collect()
    };
    if peers.is_empty() {
        return;
    }
    log::error!(
        "zcrx-lane: the MAJORITY of lane sessions on ifindex {ifindex} proved the \
         provider-pool term structural — releasing the whole NIC ({} remaining \
         session(s) torn down; RSS width restores now, kernel path serves)",
        peers.len()
    );
    for p in peers {
        Box::pin(p.teardown()).await;
    }
}

/// Opt-in master switch (design §6).
pub fn lane_env_armed() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_ZCRX_LANE", false)
}

/// Test/measurement seam: allow the classic-recv backend to arm (copy-parity
/// with the kernel path — the PR Z1 contract venue; counted, never a win).
pub fn lane_force_copy_backend() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", false)
}

/// Test seam (PR Z2 contract venue): arm the AREA backend with the socket
/// recv simulating NIC DMA into area chunks — the full chunk-parser /
/// refill / gather machinery over classic delivery. Never a product
/// posture (the real backend's delivery is RECV_ZC).
pub fn lane_area_sim_backend() -> bool {
    crate::env_knobs::bool_knob("SQUEEZEFS_ZCRX_LANE_AREA_SIM", false)
}

/// R5 arm gate (design §7): Red blocks NEW lane arms (the area is a fixed
/// non-sheddable component); Green/Yellow admit. Pure — the ladder wires
/// it to the live `mem_budget` level.
pub fn arm_admission(level: crate::mem_budget::Level) -> bool {
    !matches!(level, crate::mem_budget::Level::Red)
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
    // R5 gate (design §7): Red blocks NEW lane arms — loud, kernel path
    // serves byte-identical; a later remount (or re-arm attempt on the
    // next eligible read after pressure clears… the OnceCell caches the
    // refusal for this mount, matching the session-death posture).
    if !arm_admission(crate::mem_budget::MEM_BUDGET.level()) {
        log::warn!(
            "zcrx-lane: R5 Red blocks a NEW lane arm for {device_path} — \
             kernel path serves (byte-identical)"
        );
        return None;
    }
    let target = match lane_target_override() {
        Some(t) => t,
        None => match probe::nvme_tcp_target_for(device_path) {
            Some(t) => t,
            None => {
                // The operator ARMED the master switch; a quiet probe
                // refusal here cost a field session to diagnose (the
                // 2026-08-04 unlimited-MDTS sentinel sat behind an
                // all-zeros gauge family with no log line). Loud,
                // once per device per mount (the OnceCell caches this
                // None), kernel path serves byte-identical.
                log::warn!(
                    "zcrx-lane: SQUEEZEFS_ZCRX_LANE=1 but the sysfs probe refused \
                     {device_path} (not a plain /dev/nvme<C>n<N> tcp attachment, or \
                     a missing/garbage sysfs field) — kernel path serves \
                     (byte-identical); see docs/design-zcrx-read-lane.md §4.1"
                );
                return None;
            }
        },
    };
    if lane_area_sim_backend() {
        // PR Z2 contract venue: the area/chunk/refill machinery over a
        // classic socket recv standing in for NIC DMA.
        area::register_r5_component();
        return match LaneSession::connect_with(target, LaneBackend::AreaSim).await {
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
                    "zcrx-lane: area-sim arm failed for {device_path}: {e} — kernel \
                     path serves (byte-identical)"
                );
                None
            }
        };
    }
    if lane_force_copy_backend() {
        // The Z1 classic contract venue (copy-parity, never a win).
        return match LaneSession::connect(target).await {
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
        };
    }

    // The REAL backend's probe ladder (design §5/§7): every gate refuses
    // loud and leaves today's kernel path byte-identical; the ONLY
    // NIC-mutating step (steering) is the last one inside connect and
    // rolls itself back on refusal.
    if !probe::recv_zc_supported() {
        log::warn!(
            "zcrx-lane: SQUEEZEFS_ZCRX_LANE=1 but this kernel lacks IORING_OP_RECV_ZC \
             — lane not armed for {device_path}; kernel path serves (byte-identical)"
        );
        return None;
    }
    let Ok(ip) = target.traddr.parse::<std::net::IpAddr>() else {
        log::warn!(
            "zcrx-lane: traddr {:?} is not an IP literal — lane not armed for \
             {device_path}; kernel path serves",
            target.traddr
        );
        return None;
    };
    let Some(ifname) = ethtool::route_ifname_for(&ip) else {
        log::warn!(
            "zcrx-lane: no local interface routes to {ip} — lane not armed for \
             {device_path}; kernel path serves"
        );
        return None;
    };
    // HDS gate up front (fail before ANY bring-up work; arm_steering
    // re-checks under the same law — the corrected ATTR-compare probe).
    match ethtool_nl::tcp_data_split_on(&ifname) {
        Ok(true) => {}
        Ok(false) => {
            log::warn!(
                "zcrx-lane: {ifname} has tcp-data-split OFF — zcrx needs HDS; lane \
                 not armed for {device_path}; kernel path serves (byte-identical)"
            );
            return None;
        }
        Err(e) => {
            log::warn!(
                "zcrx-lane: HDS probe on {ifname} failed ({e}) — lane not armed for \
                 {device_path}; kernel path serves (byte-identical)"
            );
            return None;
        }
    }
    let Some(ifindex) = ethtool::nic_ifindex(&ifname) else {
        log::warn!("zcrx-lane: no ifindex for {ifname} — lane not armed for {device_path}");
        return None;
    };
    // ONE control-socket open for the remaining NIC probes: channel
    // count + the steering-capacity gate (field row 3, 2026-08-04:
    // ntuple state and rule-slot capacity probe BEFORE any bring-up
    // work — a 0-capacity NIC used to surface as a per-session
    // post-connect arm failure with no remedy named).
    let probed = ethtool::EthtoolNic::open(&ifname).and_then(|mut nic| {
        let channels = steering::NicControl::combined_channels(&mut nic)?;
        let reserved = steering::steering_capacity_gate(&mut nic)?;
        let rx_descs = nic.rx_ring_descriptors().unwrap_or_else(|e| {
            // Degrade LOUD (once per NIC): an unprobed ring means the
            // area cannot cover its standing demand — the park governor
            // bounds the damage, but sizing flies blind.
            if steering::nic_note_once("ring-probe", steering::NicControl::ifname(&nic)) {
                log::warn!(
                    "zcrx-lane: RX ring probe failed ({e}) — area sized without \
                     ring headroom; expect refill parks"
                );
            }
            0
        });
        Ok((channels, reserved, rx_descs))
    });
    let (channels, reserved, rx_ring_descs) = match probed {
        Ok(v) => v,
        Err(e) => {
            // Loud ONCE per NIC (ten fabric devices ride one NIC — the
            // remedy prints once, not 10×); the caller's OnceCell caches
            // the per-device refusal, so this is never per-read.
            if steering::note_arm_refusal_once(&ifname) {
                log::warn!(
                    "zcrx-lane: {e} — lane not armed for {device_path}; kernel path \
                     serves (byte-identical)"
                );
            } else {
                log::debug!(
                    "zcrx-lane: {e} — lane not armed for {device_path} (refusal \
                     already reported for {ifname})"
                );
            }
            return None;
        }
    };
    // flows ≡ queues 1:1, so the FREE rule-slot count (total reserved
    // minus live sessions' holdings) clamps the queue want the same way
    // the census pool does — a queue we cannot steer is a queue we must
    // not claim. The kernel-assigned class (field finding 1:
    // 0-advertised tables that accept inserts) has NO static bound —
    // the kernel's insert verdict rules at steering, so the clamp never
    // zeroes the want there.
    // FINDING C (round 3) + the 2026-08-06 engagement campaign: the
    // eligible pool is shared by every fabric device routing through
    // this NIC, and cold sequential fills spread across ALL namespaces
    // (breadth beats depth) — so the CENSUS both widens the pool
    // (`lane_eligible_queues`: clamp(devices, channels/4, channels/2) —
    // the round-8 verdict's 2-of-10 laneless devices were the flat /4
    // pool refusing sessions 9 and 10) and divides it (`fair_queue_want`:
    // clamp(eligible / devices, 1, geometry want)). The device count
    // comes from the mount's OWN sysfs + route probe (never a
    // constant); an empty or failed enumeration degrades to 1 — the
    // sole-device posture, whose pool is byte-identical to the
    // pre-campaign /4 slice.
    let devices = probe::tcp_devices_via_nic(&ifname).max(1);
    let eligible = {
        let pool = steering::lane_eligible_queues(channels, devices);
        pool.end.saturating_sub(pool.start)
    };
    let fair_want = steering::fair_queue_want(eligible, devices, target.io_queues);
    let slot_want = match steering::free_reserved_slots(&ifname, &reserved) {
        Some(0) => {
            log::warn!(
                "zcrx-lane: live lane sessions hold every reserved steering rule \
                 slot on {ifname} — lane not armed for {device_path}; kernel path \
                 serves"
            );
            return None;
        }
        Some(free) => fair_want.min(free.min(u32::from(u16::MAX)) as u16),
        None => fair_want,
    };
    // DISTINCT per-session RX queues from the NIC-derived pool — the
    // REGISTER_ZCRX_IFQ EEXIST fix (field row 3): the arbiter grants
    // queues no live session holds and frees them when the session (and
    // thus its ifqs) tears down.
    let lease = match rxq_alloc::acquire(ifindex, &ifname, channels, devices, slot_want) {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "zcrx-lane: {e} — lane not armed for {device_path}; kernel path \
                 serves (byte-identical)"
            );
            return None;
        }
    };
    let mut target = target;
    target.io_queues = lease.queues().len() as u16;
    // Round 6: the RX ring's standing pool demand rides the plan (the
    // kernel adjudication: the provider pool is the queue's ONLY buffer
    // source once restarted onto it, and the driver fills its ring from
    // that pool for the queue's lifetime — the area must cover it).
    // The MTU also rides the plan since the engagement campaign: the
    // fills' delivery-slack allotment and the CQ's chunk-touch demand
    // both derive from the burst occupancy (`area::burst_geometry`).
    let mtu = ethtool::nic_mtu(&ifname);
    let ring_fill_bytes =
        area::ring_standing_bytes(rx_ring_descs, mtu, area::chunk_bytes_default());
    let plan = initiator::ZcrxPlan {
        numa_node: ethtool::nic_numa_node(&ifname),
        ifname,
        ifindex,
        rx_queues: lease.queues().to_vec(),
        rxq_lease: Some(Arc::new(lease)),
        ring_fill_bytes,
        mtu,
    };
    area::register_r5_component();
    match LaneSession::connect_with(target, LaneBackend::Zcrx(plan)).await {
        Ok(sess) => {
            crate::fuse_client::METRICS
                .zcrx_lane_armed
                .store(1, Ordering::Relaxed);
            // Round 3: the per-NIC structural census (the whole-NIC
            // no-harm face) counts EVER-ARMED sessions here — the same
            // ifindex the session carries for its structural vote.
            nic_census::note_armed(ifindex);
            Some(sess)
        }
        Err(e) => {
            crate::fuse_client::METRICS
                .zcrx_conn_errors
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "zcrx-lane: zcrx arm failed for {device_path}: {e} — kernel path \
                 serves (byte-identical)"
            );
            None
        }
    }
}
