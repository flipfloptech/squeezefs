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
    ips: &[String],
) -> std::io::Result<String> {
    check_root()?;

    // Prevent duplicate sharing of the same backing path
    let canonical_target =
        fs::canonicalize(backing_path).unwrap_or_else(|_| PathBuf::from(backing_path));

    let shares = load_shares();
    for share in &shares {
        let share_canonical = fs::canonicalize(&share.backing_path)
            .unwrap_or_else(|_| PathBuf::from(&share.backing_path));
        if canonical_target == share_canonical {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "Backing path '{}' is already shared under subsystem '{}'",
                    backing_path, share.subnqn
                ),
            ));
        }
    }
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

    // 6. Setup Ports
    for ip in ips {
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
    }

    let _ = register_share(backing_path, &subnqn, port, ips);

    // Return connect instruction string
    Ok(subnqn)
}

pub fn call_spdk_rpc(
    method: &str,
    params: serde_json::Value,
) -> std::io::Result<serde_json::Value> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let socket_path =
        std::env::var("SQUEEZEFS_SPDK_SOCK").unwrap_or_else(|_| "/var/tmp/spdk.sock".to_string());

    if is_mock() {
        return Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": true
        }));
    }

    let mut stream = UnixStream::connect(&socket_path)?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });

    let req_str = request.to_string();
    stream.write_all(req_str.as_bytes())?;
    stream.flush()?;

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        response_bytes.extend_from_slice(&buf[..n]);
        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&response_bytes) {
            if val.get("result").is_some() || val.get("error").is_some() {
                return Ok(val);
            }
        }
    }

    let val: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(val)
}

pub fn share_target_spdk(
    backing_path: &str,
    subnqn_opt: Option<&str>,
    port: u16,
    ips: &[String],
) -> std::io::Result<String> {
    // Prevent duplicate sharing of the same backing path via local configuration
    let canonical_target =
        fs::canonicalize(backing_path).unwrap_or_else(|_| PathBuf::from(backing_path));

    let shares = load_shares();
    for share in &shares {
        let share_canonical = fs::canonicalize(&share.backing_path)
            .unwrap_or_else(|_| PathBuf::from(&share.backing_path));
        if canonical_target == share_canonical {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "Backing path '{}' is already shared under subsystem '{}'",
                    backing_path, share.subnqn
                ),
            ));
        }
    }

    // Double check active SPDK bdevs directly
    if let Ok(res) = call_spdk_rpc("bdev_get_bdevs", serde_json::json!({})) {
        if let Some(bdevs) = res.get("result").and_then(|r| r.as_array()) {
            for bdev in bdevs {
                if let Some(driver_specific) = bdev.get("driver_specific") {
                    if let Some(aio) = driver_specific.get("aio") {
                        if let Some(filename) = aio.get("filename").and_then(|f| f.as_str()) {
                            let canonical_existing = fs::canonicalize(filename)
                                .unwrap_or_else(|_| PathBuf::from(filename));
                            if canonical_target == canonical_existing {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::AlreadyExists,
                                    format!(
                                        "Backing path '{}' is already shared by active SPDK bdev '{}'",
                                        backing_path,
                                        bdev.get("name").and_then(|n| n.as_str()).unwrap_or("unknown")
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    let subnqn = match subnqn_opt {
        Some(s) => s.to_string(),
        None => format!("nqn.2026-06.io.squeezefs:spdk-subsystem-{}", Uuid::new_v4()),
    };

    // 1. Create transport (ignore if already exists)
    let _ = call_spdk_rpc(
        "nvmf_create_transport",
        serde_json::json!({
            "trtype": "TCP"
        }),
    );

    // 2. Create bdev from backing path
    let bdev_name = format!("bdev_{}", Uuid::new_v4().simple());

    let res = call_spdk_rpc(
        "bdev_aio_create",
        serde_json::json!({
            "name": bdev_name,
            "filename": backing_path,
            "block_size": 4096
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create SPDK AIO bdev: {}", err),
        ));
    }

    // 3. Create subsystem
    let res = call_spdk_rpc(
        "nvmf_create_subsystem",
        serde_json::json!({
            "nqn": subnqn,
            "allow_any_host": true,
            "serial_number": format!("SQ{}", &Uuid::new_v4().to_string()[..10])
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create SPDK NVMe-oF subsystem: {}", err),
        ));
    }

    // 4. Add namespace using our bdev
    let res = call_spdk_rpc(
        "nvmf_subsystem_add_ns",
        serde_json::json!({
            "nqn": subnqn,
            "namespace": {
                "bdev_name": bdev_name
            }
        }),
    )?;

    if let Some(err) = res.get("error") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to add bdev to SPDK subsystem namespace: {}", err),
        ));
    }

    // 5. Add listener to expose the port/IP for each address
    for ip in ips {
        let res = call_spdk_rpc(
            "nvmf_subsystem_add_listener",
            serde_json::json!({
                "nqn": subnqn,
                "listen_address": {
                    "trtype": "TCP",
                    "adrfam": "IPv4",
                    "traddr": ip,
                    "trsvcid": port.to_string()
                }
            }),
        )?;

        if let Some(err) = res.get("error") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Failed to expose SPDK subsystem listener on {}:{}: {}",
                    ip, port, err
                ),
            ));
        }
    }

    let _ = register_share_ext(backing_path, &subnqn, port, ips, true);

    Ok(subnqn)
}

pub fn unshare_target_spdk(subnqn: &str) -> std::io::Result<()> {
    // 1. Get subsystems to identify associated bdev name
    let res = call_spdk_rpc("nvmf_get_subsystems", serde_json::json!({}))?;
    let mut bdev_to_delete = None;
    if let Some(result_arr) = res.get("result").and_then(|r| r.as_array()) {
        for sub in result_arr {
            if sub.get("nqn").and_then(|n| n.as_str()) == Some(subnqn) {
                if let Some(namespaces) = sub.get("namespaces").and_then(|ns| ns.as_array()) {
                    if let Some(ns1) = namespaces.first() {
                        if let Some(name) = ns1.get("name").and_then(|n| n.as_str()) {
                            bdev_to_delete = Some(name.to_string());
                        }
                    }
                }
            }
        }
    }

    // 2. Delete the subsystem
    let res = call_spdk_rpc(
        "nvmf_delete_subsystem",
        serde_json::json!({
            "nqn": subnqn
        }),
    )?;
    if let Some(err) = res.get("error") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to delete SPDK subsystem: {}", err),
        ));
    }

    // 3. Delete the associated bdev if we found it
    if let Some(bdev_name) = bdev_to_delete {
        let _ = call_spdk_rpc(
            "bdev_aio_delete",
            serde_json::json!({
                "name": bdev_name
            }),
        );
    }

    let _ = deregister_share(subnqn);

    Ok(())
}

pub fn unshare_target(subnqn: &str) -> std::io::Result<()> {
    check_root()?;

    // Check if shared via SPDK
    let shares = load_shares();
    if let Some(share) = shares.iter().find(|s| s.subnqn == subnqn) {
        if share.is_spdk.unwrap_or(false) {
            return unshare_target_spdk(subnqn);
        }
    }

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
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("nvme connect failed: {}", err_msg),
            ));
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
    connect_target_with_local_ips(ip, port, subnqn, &[])
}

pub fn connect_target_with_local_ips(
    ip: &str,
    port: u16,
    subnqn: &str,
    local_ips: &[std::net::IpAddr],
) -> std::io::Result<String> {
    if local_ips.is_empty() {
        connect_target_single(ip, port, subnqn, None)?;
    } else {
        let mut handles = Vec::new();
        for &local_ip in local_ips {
            let ip = ip.to_string();
            let subnqn = subnqn.to_string();
            let handle = std::thread::spawn(move || {
                connect_target_single(&ip, port, &subnqn, Some(local_ip))
            });
            handles.push(handle);
        }

        let mut last_err = None;
        let mut success = false;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {
                    success = true;
                }
                Ok(Err(e)) => {
                    last_err = Some(e);
                }
                Err(_) => {
                    last_err = Some(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Thread join failed",
                    ));
                }
            }
        }

        if !success {
            return Err(last_err.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to connect via any local IP address",
                )
            }));
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

    // SPDK Target Subsystems
    if let Ok(res) = call_spdk_rpc("nvmf_get_subsystems", serde_json::json!({})) {
        if let Some(result_arr) = res.get("result").and_then(|r| r.as_array()) {
            for sub in result_arr {
                let nqn = sub.get("nqn").and_then(|n| n.as_str()).unwrap_or_default();
                if nqn == "nqn.2014-08.org.nvmexpress.discovery" {
                    continue; // Skip discovery subsystem
                }

                // Backing bdev
                let mut backing = "unknown".to_string();
                if let Some(namespaces) = sub.get("namespaces").and_then(|n| n.as_array()) {
                    if let Some(ns) = namespaces.first() {
                        if let Some(bdev_name) = ns.get("bdev_name").and_then(|b| b.as_str()) {
                            backing = bdev_name.to_string();
                        }
                    }
                }

                // Listen addresses
                let mut listen_str = Vec::new();
                if let Some(listeners) = sub.get("listen_addresses").and_then(|l| l.as_array()) {
                    for listener in listeners {
                        let ip = listener
                            .get("traddr")
                            .and_then(|i| i.as_str())
                            .unwrap_or("");
                        let port = listener
                            .get("trsvcid")
                            .and_then(|p| p.as_str())
                            .unwrap_or("");
                        if !ip.is_empty() && !port.is_empty() {
                            listen_str.push(format!("{}:{}", ip, port));
                        }
                    }
                }
                let bound_addr = if listen_str.is_empty() {
                    "none".to_string()
                } else {
                    listen_str.join(", ")
                };

                println!("  NQN:    {}", nqn);
                println!("  Backing: {} (SPDK)", backing);
                println!("  Listen:  {}", bound_addr);
                println!();
                targets_found = true;
            }
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

                    if let (Ok(nqn_raw), Ok(addr_raw)) = (
                        fs::read_to_string(subsysnqn_path),
                        fs::read_to_string(address_path),
                    ) {
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
    pub is_spdk: Option<bool>,
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

pub fn register_share(
    backing_path: &str,
    subnqn: &str,
    port: u16,
    ips: &[String],
) -> std::io::Result<()> {
    register_share_ext(backing_path, subnqn, port, ips, false)
}

pub fn register_share_ext(
    backing_path: &str,
    subnqn: &str,
    port: u16,
    ips: &[String],
    is_spdk: bool,
) -> std::io::Result<()> {
    let mut shares = load_shares();
    shares.retain(|s| s.subnqn != subnqn);
    shares.push(NvmeofShareConfig {
        backing_path: backing_path.to_string(),
        subnqn: subnqn.to_string(),
        port,
        ip: ips.join(","),
        is_spdk: Some(is_spdk),
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
        log::info!(
            "Restoring shared target {} on port {} (IPs: {})...",
            share.backing_path,
            share.port,
            share.ip
        );
        let ips: Vec<String> = share.ip.split(',').map(|s| s.trim().to_string()).collect();
        let res = if share.is_spdk.unwrap_or(false) {
            share_target_spdk(&share.backing_path, Some(&share.subnqn), share.port, &ips)
        } else {
            share_target(&share.backing_path, Some(&share.subnqn), share.port, &ips)
                .map(|_| share.subnqn.clone())
        };
        if let Err(e) = res {
            log::error!(
                "Failed to restore target share for {}: {:?}",
                share.subnqn,
                e
            );
        }
    }
    Ok(())
}

pub fn spdk_install() -> std::io::Result<()> {
    check_root()?;
    println!("Installing SPDK dependencies and compiling from source...");
    if is_mock() {
        println!("MOCK: Cloning spdk, running pkgdep.sh, configuring, and building via make.");
        return Ok(());
    }

    // 1. Clone
    println!("Cloning SPDK repo to /opt/spdk...");
    let status = std::process::Command::new("git")
        .args(&["clone", "https://github.com/spdk/spdk.git", "/opt/spdk"])
        .status()?;
    if !status.success() {
        println!(
            "SPDK repo already exists at /opt/spdk or git clone failed. Proceeding with update..."
        );
    }

    let status = std::process::Command::new("git")
        .current_dir("/opt/spdk")
        .args(&["submodule", "update", "--init"])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to update SPDK submodules",
        ));
    }

    println!("Running pkgdep.sh to install system dependencies...");
    let status = std::process::Command::new("./scripts/pkgdep.sh")
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to install SPDK dependencies",
        ));
    }

    // Install Python dependencies (tabulate)
    println!("Installing required Python modules (tabulate)...");
    let pip_status = std::process::Command::new("pip3")
        .args(&["install", "tabulate", "--break-system-packages"])
        .status();
    if pip_status.is_err() || !pip_status.unwrap().success() {
        let pip_status2 = std::process::Command::new("pip")
            .args(&["install", "tabulate"])
            .status();
        if pip_status2.is_err() || !pip_status2.unwrap().success() {
            let apt_status = std::process::Command::new("apt-get")
                .args(&["install", "-y", "python3-tabulate"])
                .status();
            if apt_status.is_err() || !apt_status.unwrap().success() {
                println!(
                    "Warning: Could not install python 'tabulate' library. Compilation might fail."
                );
            }
        }
    }

    println!("Configuring SPDK...");
    let status = std::process::Command::new("./configure")
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to configure SPDK",
        ));
    }

    println!("Building SPDK (this may take a few minutes)...");
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let status = std::process::Command::new("make")
        .arg(format!("-j{}", cores))
        .current_dir("/opt/spdk")
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Failed to compile SPDK",
        ));
    }

    println!("Successfully installed and compiled SPDK at /opt/spdk.");
    Ok(())
}

pub fn spdk_setup(hugepages_mb: usize) -> std::io::Result<()> {
    check_root()?;
    println!("Configuring hugepages ({}MB)...", hugepages_mb);
    if is_mock() {
        println!("MOCK: Configuring hugepages.");
        return Ok(());
    }

    // 1. Allocate hugepages via sysfs
    let pages = hugepages_mb / 2; // 2MB pages
    let nr_hugepages_path = "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages";
    if std::path::Path::new(nr_hugepages_path).exists() {
        fs::write(nr_hugepages_path, pages.to_string())?;
        println!("Successfully allocated {} x 2MB hugepages.", pages);
    } else {
        // Fallback to setup.sh config_huge
        let setup_script = "/opt/spdk/scripts/setup.sh";
        if std::path::Path::new(setup_script).exists() {
            let status = std::process::Command::new(setup_script)
                .arg("config_huge")
                .env("HUGEMEM", hugepages_mb.to_string())
                .status()?;
            if !status.success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "SPDK setup.sh config_huge failed.",
                ));
            }
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "SPDK setup.sh not found at /opt/spdk/scripts/setup.sh.",
            ));
        }
    }

    println!("Successfully configured hugepages.");
    Ok(())
}

pub fn spdk_bind(pci_addr: &str) -> std::io::Result<()> {
    check_root()?;
    println!(
        "Binding device at PCI address {} to SPDK user-space driver...",
        pci_addr
    );
    if is_mock() {
        println!("MOCK: Binding device {} to SPDK.", pci_addr);
        return Ok(());
    }

    let setup_script = "/opt/spdk/scripts/setup.sh";
    if std::path::Path::new(setup_script).exists() {
        let status = std::process::Command::new(setup_script)
            .arg("bind")
            .arg(pci_addr)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to bind device {} using SPDK setup.sh", pci_addr),
            ));
        }
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "SPDK setup.sh not found. Run 'squeezefs nvmeof spdk-install' first.",
        ));
    }

    println!("Successfully bound device {} to SPDK.", pci_addr);
    Ok(())
}

pub fn spdk_unbind(pci_addr: &str) -> std::io::Result<()> {
    check_root()?;
    println!("Unbinding device at PCI address {} from SPDK...", pci_addr);
    if is_mock() {
        println!("MOCK: Unbinding device {} from SPDK.", pci_addr);
        return Ok(());
    }

    // Unbind from SPDK driver (vfio-pci or uio_pci_generic) via sysfs
    let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", pci_addr);
    if std::path::Path::new(&unbind_path).exists() {
        let _ = fs::write(&unbind_path, pci_addr);
    }

    // Trigger driver probe to return it to the kernel NVMe driver
    let probe_path = "/sys/bus/pci/drivers_probe";
    if std::path::Path::new(probe_path).exists() {
        let _ = fs::write(probe_path, pci_addr);
    }

    println!("Successfully unbound device {} from SPDK.", pci_addr);
    Ok(())
}

pub fn spdk_start() -> std::io::Result<()> {
    check_root()?;
    println!("Starting SPDK NVMe-oF target daemon (nvmf_tgt)...");
    if is_mock() {
        println!("MOCK: Spawning /opt/spdk/build/bin/nvmf_tgt in background.");
        return Ok(());
    }

    let bin_path = "/opt/spdk/build/bin/nvmf_tgt";
    if !std::path::Path::new(bin_path).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "SPDK target binary not found. Run 'squeezefs nvmeof spdk-install' first.",
        ));
    }

    // Check if nvmf_tgt is already running
    let check = std::process::Command::new("pgrep").arg("nvmf_tgt").status();
    if let Ok(status) = check {
        if status.success() {
            println!("SPDK target daemon (nvmf_tgt) is already running.");
            return Ok(());
        }
    }

    // Spawn daemon in background
    let child = std::process::Command::new(bin_path)
        .arg("-i")
        .arg("0")
        .arg("-m")
        .arg("0x1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    println!("Spawned SPDK nvmf_tgt in background (PID: {}).", child.id());
    println!("JSON-RPC socket listening at /var/tmp/spdk.sock");
    Ok(())
}
