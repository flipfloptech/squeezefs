//! Runtime capability probes for the zcrx read lane (portable-by-default:
//! every gate is probed on THIS kernel/NIC/device at arm time — no model
//! tables; an absent capability leaves today's path byte-identical).

use super::initiator::LaneTarget;
use std::path::Path;

/// `IORING_OP_RECV_ZC` opcode value (uapi ≥ 6.15; absent from the io-uring
/// crate's generated opcode list at 0.6, so probed by raw value).
pub const IORING_OP_RECV_ZC: u8 = 58;

/// Does this kernel's io_uring support `RECV_ZC`? (The zcrx recv backend —
/// PR Z2 — additionally requires `IORING_REGISTER_ZCRX_IFQ` + NIC HDS; this
/// opcode probe is the cheap first gate and is sufficient to *refuse* early.)
pub fn recv_zc_supported() -> bool {
    let ring = match io_uring::IoUring::new(2) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut probe = io_uring::Probe::new();
    if ring.submitter().register_probe(&mut probe).is_err() {
        return false;
    }
    probe.is_supported(IORING_OP_RECV_ZC)
}

/// Resolve an nvme-tcp lane target for `device_path` from the kernel
/// initiator's own sysfs attachment (design §4.1 — zero new config; the
/// kernel already resolved MDTS into `max_hw_sectors_kb`, which the lane
/// reuses as its per-command transfer cap).
pub fn nvme_tcp_target_for(device_path: &str) -> Option<LaneTarget> {
    nvme_tcp_target_for_with_root(device_path, Path::new("/sys"))
}

/// Sysfs-root-injectable form (contract tests build a fixture tree).
pub fn nvme_tcp_target_for_with_root(device_path: &str, sysfs_root: &Path) -> Option<LaneTarget> {
    let name = device_path.strip_prefix("/dev/")?;
    // nvme<C>n<N> only (multipath c-paths and partitions are ineligible v1).
    let rest = name.strip_prefix("nvme")?;
    let (ctrl_num, ns_part) = rest.split_once('n')?;
    if ctrl_num.is_empty()
        || ns_part.is_empty()
        || !ctrl_num.bytes().all(|b| b.is_ascii_digit())
        || !ns_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let ctrl_dir = sysfs_root
        .join("class/nvme")
        .join(format!("nvme{ctrl_num}"));
    let read = |p: &Path| -> Option<String> {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    if read(&ctrl_dir.join("transport"))?.as_str() != "tcp" {
        return None;
    }
    let address = read(&ctrl_dir.join("address"))?;
    let mut traddr = None;
    let mut trsvcid = None;
    for part in address.split(',') {
        if let Some(v) = part.trim().strip_prefix("traddr=") {
            traddr = Some(v.to_string());
        } else if let Some(v) = part.trim().strip_prefix("trsvcid=") {
            trsvcid = Some(v.to_string());
        }
    }
    let subnqn = read(&ctrl_dir.join("subsysnqn"))?;

    let blk_dir = sysfs_root.join("block").join(name);
    let nsid: u32 = read(&blk_dir.join("nsid"))?.parse().ok()?;
    let lbs: u32 = read(&blk_dir.join("queue/logical_block_size"))?
        .parse()
        .ok()?;
    if !lbs.is_power_of_two() {
        return None;
    }
    let max_kb: u64 = read(&blk_dir.join("queue/max_hw_sectors_kb"))?
        .parse()
        .ok()?;
    // Derived geometry (design §8, via the pure form below) from the
    // PROCESS core mask — never the calling thread's
    // `available_parallelism()` (the Hang-1 pinned-first-toucher sizing
    // poison; the arm path runs on core-pinned tokio workers — the A10
    // uring_fs precedent, derivation sweep 2026-08-04).
    let (io_queues, queue_depth) = lane_geometry(crate::cpu::process_parallelism());
    // The 2026-08-04 field-refusal fix: fabrics controllers with UNLIMITED
    // MDTS advertise `max_hw_sectors_kb = 2147483644` (the i32::MAX-class
    // sentinel — measured on every nvme-tcp data controller of the
    // squeeze-test fleet). The former u32 `checked_mul(1024)?` overflowed
    // and SILENTLY refused the arm — `zcrx_lane_armed` could never leave 0
    // on a real fabrics box, while the bounded dev-substrate backings never
    // produced the sentinel. A huge device limit means the LANE's own
    // per-command window is the binding constraint: saturate to
    // [`LANE_MAX_XFER_CAP_BYTES`], floor one LBA (a zero/garbage sysfs
    // value must not zero the area window downstream).
    let max_xfer_bytes = max_kb
        .saturating_mul(1024)
        .clamp(1u64 << lbs.trailing_zeros(), LANE_MAX_XFER_CAP_BYTES as u64)
        as u32;
    Some(LaneTarget {
        traddr: traddr?,
        trsvcid: trsvcid?,
        subnqn,
        nsid,
        lba_shift: lbs.trailing_zeros(),
        max_xfer_bytes,
        io_queues,
        queue_depth,
    })
}

/// Pre-steering ceiling on the DERIVED per-device lane-queue want
/// (derivation-debt audit 2026-08-04 — named so the number has a
/// definition site): at probe time the NIC is unknown, so the §8
/// NIC-derived ceiling (`nic_queues / 4` — keep ≥ ¾ of the RSS set)
/// cannot apply yet; it does at steering (`steering::lane_queue_picks`),
/// where the device is known. Until then each wanted queue pins
/// `queue_depth × LANE_MAX_XFER_CAP_BYTES`-class area RAM at arm, so
/// this rail bounds the pre-steering pinned footprint at
/// `8 × 64 × 1 MiB = 512 MiB`/device worst case — the largest geometry
/// the Z2/Z3 acceptance venues exercised. Interim measured posture,
/// filed with the §8 BDP follow-on; tie-tested in
/// `tests/derivation_sweep_tests.rs`.
pub const LANE_IO_QUEUES_MAX: u16 = 8;

/// Derived lane geometry `(io_queues, queue_depth)` from the PROCESS
/// core count — pure (pinned on the canonical box shapes by
/// `tests/derivation_sweep_tests.rs::zcrx_lane_geometry_is_pinned_on_canonical_shapes`).
///
/// * `io_queues = clamp(cpus / 8, 1, LANE_IO_QUEUES_MAX)` — the §8
///   possible-CPUs slope; floor 1 is the physical minimum (a lane with
///   zero queues cannot exist); the ceiling is the pre-steering want
///   rail (see [`LANE_IO_QUEUES_MAX`] — the real §8 ceiling is
///   NIC-derived and applies at steering).
/// * `queue_depth = clamp(cpus × 2, 4, 64)` — an INTERIM in-flight
///   slope standing in for §8's BDP derivation (`bdp_bytes /
///   block_size` from link speed × measured connect RTT — that probe is
///   not built; filed follow-on, derivation-debt audit 2026-08-04).
///   Floor 4 and cap 64 are §8's own `derived_inflight` clamp — the
///   floor is the transport's never-regress `Q_DEPTH_FLOOR` class — and
///   CAP.MQES still clamps the granted depth at connect
///   (`initiator.rs`), so the cap never exceeds what the controller
///   grants.
pub fn lane_geometry(cpus: usize) -> (u16, u16) {
    let io_queues = (cpus / 8).clamp(1, LANE_IO_QUEUES_MAX as usize) as u16;
    let queue_depth = (cpus * 2).clamp(4, 64) as u16;
    (io_queues, queue_depth)
}

/// The lane's own per-command transfer cap — the FUSE transport's payload
/// face, ONE definition (derivation-debt audit 2026-08-04: defined FROM
/// fuse3's [`PAYLOAD_BASE`](fuse3::raw::connection::fuse_over_uring::PAYLOAD_BASE)
/// — yesterday's shipped 1 MiB ent, the largest single destination a
/// lane dest-serve fills in one command today — instead of restating
/// 1 MiB; the Z2 bracket benched exactly this MDTS-face class). A device
/// advertising more (or the unlimited sentinel above) splits into
/// sub-commands exactly as the kernel initiator would, and the cap
/// bounds per-queue area sizing at `depth × 1 MiB`
/// (`area_bytes_per_queue`).
pub const LANE_MAX_XFER_CAP_BYTES: u32 =
    fuse3::raw::connection::fuse_over_uring::PAYLOAD_BASE as u32;

/// Field finding C: count the nvme-tcp NAMESPACES whose controller's
/// `traddr` routes through `ifname` — the devices-behind-this-NIC input
/// to the fair queue-want derivation. Sysfs-root + route-resolver
/// injectable (the contract venue); the product wrapper rides the real
/// `/sys` + UDP-connect route probe.
pub fn tcp_devices_via_nic_with(
    sysfs_root: &Path,
    ifname: &str,
    resolve: &dyn Fn(&std::net::IpAddr) -> Option<String>,
) -> usize {
    // RED PHASE skeleton: contract pinned by tests/zcrx_lane_tests.rs.
    let _ = (sysfs_root, ifname, resolve);
    1
}

/// Host identity (design §4.1): field parity with the kernel initiator when
/// `/etc/nvme/{hostnqn,hostid}` exist, else a stable per-process identity.
pub fn host_identity() -> (String, [u8; 16]) {
    let hostnqn = std::fs::read_to_string("/etc/nvme/hostnqn")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let hostid_str = std::fs::read_to_string("/etc/nvme/hostid")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let hostid: [u8; 16] = hostid_str
        .as_deref()
        .and_then(parse_uuid_bytes)
        .unwrap_or_else(process_hostid);
    let hostnqn = hostnqn
        .unwrap_or_else(|| format!("nqn.2014-08.org.nvmexpress:uuid:{}", format_uuid(&hostid)));
    (hostnqn, hostid)
}

fn parse_uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

fn format_uuid(b: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Stable per-process host id (no /etc/nvme): pid + boot-time salt hashed.
fn process_hostid() -> [u8; 16] {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut h);
    if let Ok(bt) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        bt.trim().hash(&mut h);
    }
    let a = h.finish();
    "squeezefs-zcrx-lane".hash(&mut h);
    let b = h.finish();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    out
}
