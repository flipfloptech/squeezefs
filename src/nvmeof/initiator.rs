/*
 * JuiceFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! The KEPT client/initiator half of the NVMe-oF module
//! (`docs/design-nvmeof-target-management.md` binding decision 3: the
//! daemon stays on the kernel initiator — nvme-cli with the
//! `/dev/nvme-fabrics` fallback — and kernel PR ioctls; retained/derived
//! code keeps the existing Apache-2.0 header):
//! `connect`/`disconnect`, the connected-fabric-disk listing, and the
//! `/etc/nvme/hostnqn|hostid` host-identity convention (`get_host_nqn` /
//! `get_host_id` create those files; the writer guard's
//! `reservation.rs::host_identity()` reads the same files — the files
//! are the connect-time identity inputs, while `wire_host_id()` stays
//! the registrant match authority, §6.7).
//!
//! N2 note: the `SQUEEZEFS_MOCK_NVMEOF*` env forks died with the §6.8
//! zero-mock policy — these are the real paths only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use super::fabric::{split_nvme_namespace, SYSFS_NVME, SYSFS_NVME_SUBSYSTEM};
use super::{check_root, execute_cmd};

fn get_host_id() -> String {
    if let Ok(content) = fs::read_to_string("/etc/nvme/hostid") {
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let new_id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("/etc/nvme");
    let _ = fs::write("/etc/nvme/hostid", &new_id);
    new_id
}

fn get_host_nqn() -> String {
    if let Ok(content) = fs::read_to_string("/etc/nvme/hostnqn") {
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let new_nqn = format!("nqn.2014-08.org.nvmexpress:uuid:{}", get_host_id());
    let _ = fs::create_dir_all("/etc/nvme");
    let _ = fs::write("/etc/nvme/hostnqn", &new_nqn);
    new_nqn
}

/// Sorted child directories of a sysfs class root. An absent root is an
/// empty walk (sysfs classes appear with their first member — no
/// `nvme-subsystem` class on non-multipath kernels, no `nvme` class
/// before the first controller attaches).
fn sorted_child_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Does `dir` carry attribute file `attr` with (trimmed) content
/// `want`? A missing file is a mismatch, never an error (sysfs entries
/// can vanish mid-walk while a controller tears down).
fn attr_matches(dir: &Path, attr: &str, want: &str) -> std::io::Result<bool> {
    let file = dir.join(attr);
    if !file.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(file)?.trim() == want)
}

/// First child of `dir` whose name is a namespace BLOCK device —
/// strictly `^nvme\d+n\d+$` (`split_nvme_namespace`), sorted. The
/// strictness is load-bearing: on CONFIG_NVME_MULTIPATH kernels the
/// controller dir carries hidden per-controller path nodes
/// `nvme<X>c<C>n<Y>` which are NOT user-visible block devices — the
/// pre-fix `starts_with(dev) && contains('n')` predicate matched them
/// (the 2026-07-25 `/dev/nvme0c0n1` bring-up bug).
fn first_namespace_block_child(dir: &Path) -> std::io::Result<Option<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() && split_nvme_namespace(&name).is_some() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names.into_iter().next())
}

/// Resolve the user-visible namespace block device serving `subnqn`
/// from explicit sysfs roots (§6.8 injection seam — production wraps
/// this with the real `SYSFS_NVME_SUBSYSTEM` / `SYSFS_NVME` roots).
///
/// Discovery order: prefer the SUBSYSTEM class
/// (`/sys/class/nvme-subsystem/*/subsysnqn` → its strict-shape
/// `nvme<X>n<Y>` child — on multipath kernels the head node lives
/// there and its instance number is the subsystem's, not necessarily
/// any controller's), then fall back to the controller class
/// (non-multipath kernels: the plain namespace is a controller child)
/// under the same strict shape rule. Unresolvable is an honest `None`
/// — device names are never fabricated by string concatenation (the
/// pre-fix `format!("/dev/{}n1", ctrl)` fallback could mint a
/// nonexistent node).
pub fn find_device_for_nqn_at(
    subsystem_root: &Path,
    nvme_root: &Path,
    subnqn: &str,
) -> std::io::Result<Option<String>> {
    for dir in sorted_child_dirs(subsystem_root)? {
        if attr_matches(&dir, "subsysnqn", subnqn)? {
            if let Some(name) = first_namespace_block_child(&dir)? {
                return Ok(Some(format!("/dev/{}", name)));
            }
        }
    }
    for dir in sorted_child_dirs(nvme_root)? {
        if attr_matches(&dir, "subsysnqn", subnqn)? {
            if let Some(name) = first_namespace_block_child(&dir)? {
                return Ok(Some(format!("/dev/{}", name)));
            }
        }
    }
    Ok(None)
}

fn find_device_for_nqn(subnqn: &str) -> std::io::Result<Option<String>> {
    find_device_for_nqn_at(
        Path::new(SYSFS_NVME_SUBSYSTEM),
        Path::new(SYSFS_NVME),
        subnqn,
    )
}

/// Optional connect-time path/queue controls (the 2026-07-25
/// live-cluster findings): pin the fabric connection's source
/// address/interface on multi-homed hosts (same-subnet dual-NIC setups
/// cannot select the path by routing alone), and bound the I/O-queue
/// request to what the target will grant (a target granting fewer
/// queues than requested fails the connect with errno -18). Every
/// field passes through ONLY when supplied — an all-`None` value is
/// byte-identical to the pre-flag behavior.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Local source address for the fabric connection
    /// (nvme-cli `--host-traddr` / fabrics `host_traddr=`).
    pub host_traddr: Option<String>,
    /// Local source interface
    /// (nvme-cli `--host-iface` / fabrics `host_iface=`).
    pub host_iface: Option<String>,
    /// Upper bound on requested I/O queues, ≥ 1
    /// (nvme-cli `--nr-io-queues` / fabrics `nr_io_queues=`).
    pub nr_io_queues: Option<u32>,
}

/// The nvme-cli argv for one connect. Pure and injectable (tested
/// directly): the option-less shape is a stable prefix; supplied
/// options only append their flag/value pairs.
pub fn nvme_cli_connect_args(
    ip: &str,
    port: u16,
    subnqn: &str,
    opts: &ConnectOptions,
) -> Vec<String> {
    let mut args = vec![
        "connect".to_string(),
        "-t".to_string(),
        "tcp".to_string(),
        "-a".to_string(),
        ip.to_string(),
        "-s".to_string(),
        port.to_string(),
        "-n".to_string(),
        subnqn.to_string(),
    ];
    if let Some(traddr) = &opts.host_traddr {
        args.push("--host-traddr".to_string());
        args.push(traddr.clone());
    }
    if let Some(iface) = &opts.host_iface {
        args.push("--host-iface".to_string());
        args.push(iface.clone());
    }
    if let Some(n) = opts.nr_io_queues {
        args.push("--nr-io-queues".to_string());
        args.push(n.to_string());
    }
    args
}

/// The `/dev/nvme-fabrics` option string for one connect (the
/// no-nvme-cli fallback). Host identity is injectable (tested
/// directly); supplied options map to the kernel's fabrics option
/// names and append only when present.
pub fn fabrics_connect_string(
    ip: &str,
    port: u16,
    subnqn: &str,
    hostnqn: &str,
    hostid: &str,
    opts: &ConnectOptions,
) -> String {
    let mut s = format!(
        "transport=tcp,traddr={},trsvcid={},nqn={},hostnqn={},hostid={}",
        ip, port, subnqn, hostnqn, hostid
    );
    if let Some(traddr) = &opts.host_traddr {
        s.push_str(&format!(",host_traddr={}", traddr));
    }
    if let Some(iface) = &opts.host_iface {
        s.push_str(&format!(",host_iface={}", iface));
    }
    if let Some(n) = opts.nr_io_queues {
        s.push_str(&format!(",nr_io_queues={}", n));
    }
    s
}

fn connect_target_single(
    ip: &str,
    port: u16,
    subnqn: &str,
    opts: &ConnectOptions,
) -> std::io::Result<()> {
    check_root()?;
    let _ = execute_cmd("modprobe", &["nvme-tcp"]);

    // Use nvme-cli connect if available
    let has_nvme_cli = Command::new("nvme").arg("--version").status().is_ok();

    if has_nvme_cli {
        let args = nvme_cli_connect_args(ip, port, subnqn, opts);
        let output = Command::new("nvme").args(&args).output()?;
        if !output.status.success() {
            let err_msg = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(std::io::Error::other(format!(
                "nvme connect failed: {}",
                err_msg
            )));
        }
    } else {
        // Fallback to direct fabrics write
        let dev_fabrics = PathBuf::from("/dev/nvme-fabrics");
        if !dev_fabrics.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "NVMe Fabrics interface (/dev/nvme-fabrics) not found. Please ensure 'nvme-tcp' \
                 module is loaded.",
            ));
        }

        let ctrl_conn_str =
            fabrics_connect_string(ip, port, subnqn, &get_host_nqn(), &get_host_id(), opts);
        fs::write(&dev_fabrics, &ctrl_conn_str)?;
    }
    Ok(())
}

pub fn connect_target(
    ip: &str,
    port: u16,
    subnqn: &str,
    opts: &ConnectOptions,
) -> std::io::Result<String> {
    // Refuse the degenerate bound BEFORE any side effect (root check,
    // modprobe, connect): zero I/O queues is not a connection — the
    // flag exists to bound the request, never to zero it.
    if opts.nr_io_queues == Some(0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--nr-io-queues must be at least 1: the flag bounds the I/O-queue request to what \
             the target grants; zero requests no I/O queues at all",
        ));
    }
    connect_target_single(ip, port, subnqn, opts)?;

    // Wait up to 2 seconds for the block device node to appear in sysfs
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < std::time::Duration::from_secs(2) {
        if let Ok(Some(dev)) = find_device_for_nqn(subnqn) {
            return Ok(dev);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok("Connection requested. Check 'squeezefs nvmeof list' for device mapping.".to_string())
}

/// Write the delete request to EVERY controller under `nvme_root`
/// serving `subnqn` and return the controller names written, sorted
/// (§6.8 injection seam). Multipath kernels attach several path
/// controllers per subsystem — all of them are the connection, so all
/// of them get the delete.
pub fn disconnect_controllers_at(nvme_root: &Path, subnqn: &str) -> std::io::Result<Vec<String>> {
    let mut deleted = Vec::new();
    for dir in sorted_child_dirs(nvme_root)? {
        if !attr_matches(&dir, "subsysnqn", subnqn)? {
            continue;
        }
        // Trigger disconnect by writing to delete_controller.
        let del_file = dir.join("delete_controller");
        if del_file.exists() {
            fs::write(del_file, "1")?;
            if let Some(name) = dir.file_name() {
                deleted.push(name.to_string_lossy().to_string());
            }
        }
    }
    Ok(deleted)
}

pub fn disconnect_target(subnqn: &str) -> std::io::Result<()> {
    check_root()?;
    let nvme_path = Path::new(SYSFS_NVME);
    if !nvme_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No active NVMe devices found.",
        ));
    }

    let deleted = disconnect_controllers_at(nvme_path, subnqn)?;
    if deleted.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("No connected controller found for NQN '{}'.", subnqn),
        ));
    }
    for name in &deleted {
        println!("Sent delete request to controller '{}'.", name);
    }

    Ok(())
}

/// One connected remote fabric controller, as `list` reports it.
#[derive(Debug, Clone)]
pub struct FabricDisk {
    /// The resolved user-visible namespace block device
    /// (`/dev/nvme<X>n<Y>` — the multipath head node where one
    /// exists); `None` when no strict-shape namespace has materialized
    /// yet (the pre-fix code fabricated `/dev/{ctrl}n1` here — a name
    /// that need not exist).
    pub device: Option<String>,
    pub subnqn: String,
    pub address: String,
}

/// The connected-fabric-disks walk behind `list`, on explicit sysfs
/// roots (§6.8 injection seam): one row per controller carrying a
/// `subsysnqn`, sorted, with the device resolved through the same
/// subsystem-first strict-shape discovery as connect.
pub fn connected_fabric_disks_at(
    subsystem_root: &Path,
    nvme_root: &Path,
) -> std::io::Result<Vec<FabricDisk>> {
    let mut out = Vec::new();
    for dir in sorted_child_dirs(nvme_root)? {
        let Some(dev_name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if !dev_name.starts_with("nvme") {
            continue;
        }
        let subs_nqn_file = dir.join("subsysnqn");
        if !subs_nqn_file.exists() {
            continue;
        }
        let nqn = fs::read_to_string(subs_nqn_file)?.trim().to_string();
        let address_file = dir.join("address");
        let addr = if address_file.exists() {
            fs::read_to_string(address_file)?.trim().to_string()
        } else {
            "unknown".to_string()
        };
        out.push(FabricDisk {
            device: find_device_for_nqn_at(subsystem_root, nvme_root, &nqn)?,
            subnqn: nqn,
            address: addr,
        });
    }
    Ok(out)
}

/// The connected-fabric-disks half of `list` (kept initiator listing).
pub(crate) fn connected_fabric_disks() -> std::io::Result<Vec<FabricDisk>> {
    connected_fabric_disks_at(Path::new(SYSFS_NVME_SUBSYSTEM), Path::new(SYSFS_NVME))
}
