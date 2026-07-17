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

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

use super::{check_root, execute_cmd, is_mock};

pub(crate) fn sysfs_fabrics_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvme_fabrics")
    } else {
        PathBuf::from("/sys/class/nvme-fabrics")
    }
}

pub(crate) fn sysfs_nvme_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_NVME_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvme")
    } else {
        PathBuf::from("/sys/class/nvme")
    }
}

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

fn find_device_for_nqn(subnqn: &str) -> std::io::Result<Option<String>> {
    let nvme_path = sysfs_nvme_path();
    if !nvme_path.exists() {
        return Ok(None);
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(nvme_path)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let dev_name = entry.file_name().to_string_lossy().to_string();
        if dev_name.starts_with("nvme") {
            let nqn_file = path.join("subsysnqn");
            if nqn_file.exists() {
                let current_nqn = fs::read_to_string(nqn_file)?.trim().to_string();
                if current_nqn == subnqn {
                    // Check if namespace block device dir exists (e.g. nvme0n1)
                    let mut sub_entries = Vec::new();
                    for sub_entry in fs::read_dir(&path)? {
                        sub_entries.push(sub_entry?);
                    }
                    sub_entries.sort_by_key(|e| e.file_name());

                    for sub_entry in sub_entries {
                        let sub_name = sub_entry.file_name().to_string_lossy().to_string();
                        if sub_name.starts_with(&dev_name) && sub_name.contains('n') {
                            return Ok(Some(format!("/dev/{}", sub_name)));
                        }
                    }
                    return Ok(Some(format!("/dev/{}n1", dev_name)));
                }
            }
        }
    }
    Ok(None)
}

static MOCK_WRITE_MUTEX: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

fn connect_target_single(
    ip: &str,
    port: u16,
    subnqn: &str,
    local_ip: Option<std::net::IpAddr>,
) -> std::io::Result<()> {
    check_root()?;
    if !is_mock() {
        let _ = execute_cmd("modprobe", &["nvme-tcp"]);
    }

    // Use nvme-cli connect if available
    let has_nvme_cli = !is_mock() && Command::new("nvme").arg("--version").status().is_ok();

    if has_nvme_cli {
        let port_str = port.to_string();
        let mut args = vec![
            "connect".to_string(),
            "-t".to_string(),
            "tcp".to_string(),
            "-a".to_string(),
            ip.to_string(),
            "-s".to_string(),
            port_str,
            "-n".to_string(),
            subnqn.to_string(),
        ];
        if let Some(host_ip) = local_ip {
            args.push("-p".to_string());
            args.push(host_ip.to_string());
        }
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
        let target_file = if is_mock() {
            let fabrics_path = sysfs_fabrics_path();
            fs::create_dir_all(&fabrics_path)?;
            fabrics_path.join("ctl")
        } else {
            let dev_fabrics = PathBuf::from("/dev/nvme-fabrics");
            if !dev_fabrics.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "NVMe Fabrics interface (/dev/nvme-fabrics) not found. Please ensure 'nvme-tcp' module is loaded.",
                ));
            }
            dev_fabrics
        };

        let hostnqn = get_host_nqn();
        let hostid = get_host_id();
        let mut ctrl_conn_str = format!(
            "transport=tcp,traddr={},trsvcid={},nqn={},hostnqn={},hostid={}",
            ip, port, subnqn, hostnqn, hostid
        );
        if let Some(host_ip) = local_ip {
            ctrl_conn_str = format!("{},host_traddr={}", ctrl_conn_str, host_ip);
        }

        if is_mock() {
            let _lock = MOCK_WRITE_MUTEX.lock().unwrap();
            let mut ctl_content = String::new();
            if target_file.exists() {
                ctl_content = fs::read_to_string(&target_file)?;
            }
            if !ctl_content.is_empty() {
                ctl_content.push('\n');
            }
            ctl_content.push_str(&ctrl_conn_str);
            fs::write(&target_file, &ctl_content)?;

            // Find next available controller name (e.g. nvme0, nvme1, ...)
            let mut ctrl_index = 0;
            let nvme_path = sysfs_nvme_path();
            while nvme_path.join(format!("nvme{}", ctrl_index)).exists() {
                ctrl_index += 1;
            }
            let ctrl_dir = nvme_path.join(format!("nvme{}", ctrl_index));
            fs::create_dir_all(&ctrl_dir)?;
            fs::write(ctrl_dir.join("subsysnqn"), subnqn)?;
            fs::write(
                ctrl_dir.join("address"),
                format!("traddr={},trsvcid={}", ip, port),
            )?;
            fs::write(ctrl_dir.join("delete_controller"), "")?;

            // Create mock namespace
            let ns_dev_dir = ctrl_dir.join(format!("nvme{}n1", ctrl_index));
            fs::create_dir_all(&ns_dev_dir)?;
        } else {
            fs::write(&target_file, &ctrl_conn_str)?;
        }
    }
    Ok(())
}

pub fn connect_target(ip: &str, port: u16, subnqn: &str) -> std::io::Result<String> {
    connect_target_single(ip, port, subnqn, None)?;

    // Wait up to 2 seconds for the block device node to appear in sysfs
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < std::time::Duration::from_secs(2) {
        if let Ok(Some(dev)) = find_device_for_nqn(subnqn) {
            return Ok(dev);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(
        "Connection requested. Check 'squeezefs storage nvmeof list' for device mapping."
            .to_string(),
    )
}

pub fn disconnect_target(subnqn: &str) -> std::io::Result<()> {
    check_root()?;
    let nvme_path = sysfs_nvme_path();
    if !nvme_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No active NVMe devices found.",
        ));
    }

    let mut found = false;
    for entry in fs::read_dir(nvme_path)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("nvme")
        {
            let nqn_file = path.join("subsysnqn");
            if nqn_file.exists() {
                let current_nqn = fs::read_to_string(nqn_file)?.trim().to_string();
                if current_nqn == subnqn {
                    // Trigger disconnect by writing to delete_controller
                    let del_file = path.join("delete_controller");
                    if del_file.exists() {
                        fs::write(del_file, "1")?;
                        found = true;
                        println!(
                            "Sent delete request to controller '{}'.",
                            path.file_name().unwrap().to_string_lossy()
                        );
                    }
                }
            }
        }
    }

    if !found {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("No connected controller found for NQN '{}'.", subnqn),
        ));
    }

    Ok(())
}

/// The connected-fabric-disks half of `list` (kept initiator listing):
/// prints each connected remote controller; returns whether any exist.
pub(crate) fn print_connected_fabric_disks() -> std::io::Result<bool> {
    let nvme_path = sysfs_nvme_path();
    let mut initiator_found = false;
    if nvme_path.exists() {
        for entry in fs::read_dir(nvme_path)? {
            let entry = entry?;
            let path = entry.path();
            let dev_name = entry.file_name().to_string_lossy().to_string();
            if dev_name.starts_with("nvme") {
                let subs_nqn_file = path.join("subsysnqn");
                let address_file = path.join("address");
                if subs_nqn_file.exists() {
                    let nqn = fs::read_to_string(subs_nqn_file)?.trim().to_string();
                    let addr = if address_file.exists() {
                        fs::read_to_string(address_file)?.trim().to_string()
                    } else {
                        "unknown".to_string()
                    };

                    println!("  Device:  /dev/{}n1", dev_name);
                    println!("  NQN:     {}", nqn);
                    println!("  Target:  {}", addr);
                    println!();
                    initiator_found = true;
                }
            }
        }
    }
    Ok(initiator_found)
}
