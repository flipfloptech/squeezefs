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

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

fn is_mock() -> bool {
    std::env::var("SQUEEZEFS_MOCK_NVMEOF").is_ok()
}

fn check_root() -> std::io::Result<()> {
    if is_mock() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if unsafe { libc::getuid() } != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "This command requires root privileges. Please run with sudo.",
            ));
        }
    }
    Ok(())
}

fn configfs_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvmet")
    } else {
        PathBuf::from("/sys/kernel/config/nvmet")
    }
}

fn sysfs_fabrics_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_FABRICS_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvme_fabrics")
    } else {
        PathBuf::from("/sys/class/nvme-fabrics")
    }
}

fn sysfs_nvme_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SQUEEZEFS_MOCK_NVMEOF_NVME_DIR") {
        PathBuf::from(dir)
    } else if is_mock() {
        PathBuf::from("/tmp/squeezefs_nvme")
    } else {
        PathBuf::from("/sys/class/nvme")
    }
}

fn execute_cmd(cmd_name: &str, args: &[&str]) -> std::io::Result<String> {
    if is_mock() {
        return Ok(String::new());
    }
    let output = Command::new(cmd_name).args(args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Command {} failed: {}",
            cmd_name,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn share_target(
    backing_path: &str,
    subnqn_opt: Option<&str>,
    port: u16,
    ip: &str,
) -> std::io::Result<String> {
    check_root()?;
    // 1. Check/create backing path if it's a regular file path
    let backing_path_buf = PathBuf::from(backing_path);
    if !backing_path_buf.exists() {
        println!(
            "Backing file '{}' does not exist. Auto-creating a 1GB sparse file...",
            backing_path
        );
        let f = fs::File::create(&backing_path_buf)?;
        f.set_len(1024 * 1024 * 1024)?; // 1GB default
    }

    let is_dir = backing_path_buf.is_dir();
    if is_dir {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Backing path cannot be a directory.",
        ));
    }

    // 2. Load modules
    if !is_mock() {
        let _ = execute_cmd("modprobe", &["nvmet"]);
        let _ = execute_cmd("modprobe", &["nvmet-tcp"]);
    }

    // 3. Mount configfs if not mounted
    let config_dir = configfs_path();
    if !is_mock() && !config_dir.exists() {
        let _ = execute_cmd("mount", &["-t", "configfs", "none", "/sys/kernel/config"]);
    }

    // 4. Handle regular files via loop devices
    let mut resolved_device = backing_path.to_string();
    let is_reg_file = !is_mock() && {
        let metadata = fs::metadata(&backing_path_buf)?;
        metadata.is_file()
    };

    let mut associated_loop = None;
    if is_reg_file || (is_mock() && backing_path.ends_with(".img")) {
        // Try to find if already associated
        let loop_list = if is_mock() {
            String::new()
        } else {
            execute_cmd("losetup", &["-j", backing_path]).unwrap_or_default()
        };

        if !loop_list.is_empty() {
            // Already associated, parse loop device
            if let Some(first_line) = loop_list.lines().next() {
                if let Some(pos) = first_line.find(':') {
                    resolved_device = first_line[..pos].to_string();
                    associated_loop = Some(resolved_device.clone());
                }
            }
        } else {
            // Find free loop device and associate
            let free_loop = if is_mock() {
                "/dev/loop99".to_string()
            } else {
                execute_cmd("losetup", &["-f"])?
            };
            if !is_mock() {
                execute_cmd("losetup", &[&free_loop, backing_path])?;
            }
            resolved_device = free_loop.clone();
            associated_loop = Some(free_loop);
        }
    }

    // 5. Setup subsystem
    let subnqn = match subnqn_opt {
        Some(s) => s.to_string(),
        None => format!("nqn.2026-06.io.squeezefs:subsystem-{}", Uuid::new_v4()),
    };

    let sub_dir = config_dir.join("subsystems").join(&subnqn);
    fs::create_dir_all(&sub_dir)?;

    // Enable any host access
    fs::write(sub_dir.join("attr_allow_any_host"), "1")?;

    // Create namespace
    let ns_dir = sub_dir.join("namespaces").join("1");
    fs::create_dir_all(&ns_dir)?;
    fs::write(ns_dir.join("device_path"), &resolved_device)?;
    fs::write(ns_dir.join("enable"), "1")?;

    // Save loop association info in subsystem dir if present
    if let Some(loop_dev) = associated_loop {
        let _ = fs::write(sub_dir.join("associated_loop_device"), loop_dev);
    }

    // 6. Setup Port
    // Try port index 1 to 100 to find a free or existing match
    let mut port_exists = false;
    let mut port_id = 1;
    let mut port_dir = config_dir.join("ports").join(port_id.to_string());
    while port_dir.exists() {
        // Read active address and port svc ID to see if it is our IP and port
        if let Ok(addr) = fs::read_to_string(port_dir.join("addr_traddr")) {
            if let Ok(svc) = fs::read_to_string(port_dir.join("addr_trsvcid")) {
                if addr.trim() == ip && svc.trim() == port.to_string() {
                    port_exists = true;
                    break; // Port already exists and matches!
                }
            }
        }
        port_id += 1;
        port_dir = config_dir.join("ports").join(port_id.to_string());
    }

    if !port_exists {
        fs::create_dir_all(&port_dir)?;
        fs::write(port_dir.join("addr_traddr"), ip)?;
        fs::write(port_dir.join("addr_trtype"), "tcp")?;
        fs::write(port_dir.join("addr_trsvcid"), port.to_string())?;
        fs::write(port_dir.join("addr_adrfam"), "ipv4")?;
    }

    // Link subsystem to port
    let link_dest = port_dir.join("subsystems").join(&subnqn);
    fs::create_dir_all(port_dir.join("subsystems"))?;

    #[cfg(unix)]
    if !link_dest.exists() {
        std::os::unix::fs::symlink(&sub_dir, &link_dest)?;
    }

    let _ = register_share(backing_path, &subnqn, port, ip);

    // Return connect instruction string
    Ok(subnqn)
}

pub fn unshare_target(subnqn: &str) -> std::io::Result<()> {
    check_root()?;
    let config_dir = configfs_path();
    let sub_dir = config_dir.join("subsystems").join(subnqn);
    if !sub_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Subsystem NQN '{}' not found.", subnqn),
        ));
    }

    // Read loop device association if any
    let associated_loop = fs::read_to_string(sub_dir.join("associated_loop_device"))
        .ok()
        .map(|s| s.trim().to_string());

    // 1. Remove all symlinks from ports
    let ports_dir = config_dir.join("ports");
    if ports_dir.exists() {
        for entry in fs::read_dir(ports_dir)? {
            let entry = entry?;
            let port_subsystems = entry.path().join("subsystems");
            if port_subsystems.exists() {
                let link_path = port_subsystems.join(subnqn);
                if link_path.exists() {
                    let _ = fs::remove_file(link_path);
                }
            }
        }
    }

    // 2. Disable namespace and delete directories
    if is_mock() {
        let _ = fs::remove_dir_all(&sub_dir);
    } else {
        let ns_dir = sub_dir.join("namespaces").join("1");
        if ns_dir.exists() {
            let _ = fs::write(ns_dir.join("enable"), "0");
            let _ = fs::remove_file(ns_dir.join("device_path"));
            let _ = fs::remove_file(ns_dir.join("enable"));
            let _ = fs::remove_dir(ns_dir);
        }
        let _ = fs::remove_dir(sub_dir.join("namespaces"));
        let _ = fs::remove_file(sub_dir.join("associated_loop_device"));
        let _ = fs::remove_file(sub_dir.join("attr_allow_any_host"));
        let _ = fs::remove_dir(&sub_dir);
    }

    // 3. Detach loop device if associated
    if let Some(loop_dev) = associated_loop {
        if !is_mock() {
            let _ = execute_cmd("losetup", &["-d", &loop_dev]);
        }
        println!("Detached associated loop device '{}'.", loop_dev);
    }

    let _ = deregister_share(subnqn);

    Ok(())
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
    for entry in fs::read_dir(nvme_path)? {
        let entry = entry?;
        let path = entry.path();
        let dev_name = entry.file_name().to_string_lossy().to_string();
        if dev_name.starts_with("nvme") {
            let nqn_file = path.join("subsysnqn");
            if nqn_file.exists() {
                let current_nqn = fs::read_to_string(nqn_file)?.trim().to_string();
                if current_nqn == subnqn {
                    // Check if namespace block device dir exists (e.g. nvme0n1)
                    for sub_entry in fs::read_dir(&path)? {
                        let sub_entry = sub_entry?;
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

pub fn connect_target(ip: &str, port: u16, subnqn: &str) -> std::io::Result<String> {
    check_root()?;
    if !is_mock() {
        let _ = execute_cmd("modprobe", &["nvme-tcp"]);
    }

    // Use nvme-cli connect if available
    let has_nvme_cli = !is_mock() && Command::new("nvme").arg("--version").status().is_ok();

    if has_nvme_cli {
        let port_str = port.to_string();
        let _ = Command::new("nvme")
            .args([
                "connect", "-t", "tcp", "-a", ip, "-s", &port_str, "-n", subnqn,
            ])
            .output()?;
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
        let ctrl_conn_str = format!(
            "transport=tcp,traddr={},trsvcid={},nqn={},hostnqn={},hostid={}",
            ip, port, subnqn, hostnqn, hostid
        );

        if is_mock() {
            fs::write(&target_file, &ctrl_conn_str)?;
            // Create a mock controller dir
            let ctrl_dir = sysfs_nvme_path().join("nvme0");
            fs::create_dir_all(&ctrl_dir)?;
            fs::write(ctrl_dir.join("subsysnqn"), subnqn)?;
            fs::write(
                ctrl_dir.join("address"),
                format!("traddr={},trsvcid={}", ip, port),
            )?;
            fs::write(ctrl_dir.join("delete_controller"), "")?;

            // Create mock namespace
            let ns_dev_dir = ctrl_dir.join("nvme0n1");
            fs::create_dir_all(&ns_dev_dir)?;
        } else {
            fs::write(&target_file, &ctrl_conn_str)?;
        }
    }

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

pub fn list_nvmeof() -> std::io::Result<()> {
    check_root()?;
    let config_dir = configfs_path();
    let subs_dir = config_dir.join("subsystems");

    // Colored is imported in main.rs but not necessarily here. We can just print standard strings or implement simple coloring.
    println!("=== Shared NVMe-oF Targets ===");
    let mut targets_found = false;
    if subs_dir.exists() {
        for entry in fs::read_dir(subs_dir)? {
            let entry = entry?;
            let sub_name = entry.file_name().to_string_lossy().to_string();
            let sub_path = entry.path();

            let backing = if let Ok(dev) =
                fs::read_to_string(sub_path.join("namespaces").join("1").join("device_path"))
            {
                dev.trim().to_string()
            } else {
                "unknown".to_string()
            };

            let assoc_loop = fs::read_to_string(sub_path.join("associated_loop_device"))
                .ok()
                .map(|s| format!(" (loop: {})", s.trim()))
                .unwrap_or_default();

            // Find bound IP and Port
            let mut bound_addr = "0.0.0.0:4420".to_string();
            let ports_dir = config_dir.join("ports");
            if ports_dir.exists() {
                for p_entry in fs::read_dir(ports_dir)? {
                    let p_entry = p_entry?;
                    if p_entry.path().join("subsystems").join(&sub_name).exists() {
                        let ip = fs::read_to_string(p_entry.path().join("addr_traddr"))
                            .unwrap_or_default();
                        let port = fs::read_to_string(p_entry.path().join("addr_trsvcid"))
                            .unwrap_or_default();
                        bound_addr = format!("{}:{}", ip.trim(), port.trim());
                        break;
                    }
                }
            }

            println!("  NQN:    {}", sub_name);
            println!("  Backing: {}{}", backing, assoc_loop);
            println!("  Listen:  {}", bound_addr);
            println!();
            targets_found = true;
        }
    }
    if !targets_found {
        println!("  No shared NVMe-oF targets configured.");
    }

    println!("=== Connected Fabric Disks ===");
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

    if !initiator_found {
        println!("  No connected remote NVMe-oF fabric disks.");
    }

    Ok(())
}

pub fn extract_nvmeof_connection_details(backing_dev: &str) -> Option<(String, u16, String)> {
    use std::fs;
    use std::path::Path;

    let path = Path::new(backing_dev);
    let real_path = fs::canonicalize(path).ok()?;
    let real_path_str = real_path.to_string_lossy();

    // Check if it starts with /dev/nvme
    if real_path_str.starts_with("/dev/nvme") {
        let parts: Vec<&str> = real_path_str.split('/').collect();
        if let Some(dev_name) = parts.last() {
            if dev_name.starts_with("nvme") {
                // Find index before 'n' (e.g. nvme0n1 -> nvme0)
                if let Some(end_idx) = dev_name.rfind('n') {
                    let ctrl = &dev_name[..end_idx];
                    let subsysnqn_path = format!("/sys/class/nvme/{}/subsysnqn", ctrl);
                    let address_path = format!("/sys/class/nvme/{}/address", ctrl);

                    if let (Ok(nqn_raw), Ok(addr_raw)) = (fs::read_to_string(subsysnqn_path), fs::read_to_string(address_path)) {
                        let subnqn = nqn_raw.trim().to_string();
                        
                        // Parse address properties: e.g. "traddr=192.168.1.100,trsvcid=4420"
                        let mut ip = None;
                        let mut port = None;
                        for part in addr_raw.split(',') {
                            let kv: Vec<&str> = part.split('=').collect();
                            if kv.len() == 2 {
                                match kv[0].trim() {
                                    "traddr" => ip = Some(kv[1].trim().to_string()),
                                    "trsvcid" => port = kv[1].trim().parse::<u16>().ok(),
                                    _ => {}
                                }
                            }
                        }
                        if let (Some(ip_val), Some(port_val)) = (ip, port) {
                            return Some((ip_val, port_val, subnqn));
                        }
                    }
                }
            }
        }
    }
    None
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct NvmeofShareConfig {
    pub backing_path: String,
    pub subnqn: String,
    pub port: u16,
    pub ip: String,
}

fn get_shares_config_path() -> PathBuf {
    if std::env::var("SQUEEZEFS_TEST_ENV").is_ok() {
        return PathBuf::from("/tmp/squeezefs_nvmeof_shares_test.json");
    }
    // If running as root, save under /etc/squeezefs/
    let path = PathBuf::from("/etc/squeezefs/nvmeof_shares.json");
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_ok() && fs::write(&path, "[]").is_ok() {
            return path;
        }
    }
    // Fallback for non-root / user local path
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(format!("{}/.squeeze/nvmeof_shares.json", home))
}

fn load_shares() -> Vec<NvmeofShareConfig> {
    let path = get_shares_config_path();
    if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn save_shares(shares: &[NvmeofShareConfig]) -> std::io::Result<()> {
    let path = get_shares_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(shares)?;
    fs::write(path, content)?;
    Ok(())
}

pub fn register_share(backing_path: &str, subnqn: &str, port: u16, ip: &str) -> std::io::Result<()> {
    let mut shares = load_shares();
    shares.retain(|s| s.subnqn != subnqn);
    shares.push(NvmeofShareConfig {
        backing_path: backing_path.to_string(),
        subnqn: subnqn.to_string(),
        port,
        ip: ip.to_string(),
    });
    save_shares(&shares)
}

pub fn deregister_share(subnqn: &str) -> std::io::Result<()> {
    let mut shares = load_shares();
    shares.retain(|s| s.subnqn != subnqn);
    save_shares(&shares)
}

pub fn restore_shares() -> std::io::Result<()> {
    let shares = load_shares();
    if shares.is_empty() {
        log::info!("No NVMe-oF target shares to restore.");
        return Ok(());
    }
    log::info!("Restoring {} NVMe-oF target shares...", shares.len());
    for share in shares {
        log::info!("Restoring shared target {} on port {} (IP: {})...", share.backing_path, share.port, share.ip);
        if let Err(e) = share_target(&share.backing_path, Some(&share.subnqn), share.port, &share.ip) {
            log::error!("Failed to restore target share for {}: {:?}", share.subnqn, e);
        }
    }
    Ok(())
}
