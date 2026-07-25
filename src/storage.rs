use crate::error::{Result, SqueezefsError};
use std::process::Command;

pub(crate) fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(cmd).args(args).output().map_err(|e| {
        SqueezefsError::InvalidOperation(format!("Failed to execute {}: {}", cmd, e))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "Command {} failed: {}",
            cmd, stderr
        )));
    }
    Ok(())
}

pub fn pool_create(pool_name: &str, disks: &[String]) -> Result<()> {
    // 1. Run pvcreate on all disks
    let mut pv_args: Vec<&str> = vec!["-y", "-ff"]; // Yes to prompts and force re-init
    for d in disks {
        pv_args.push(d.as_str());
    }
    println!("Initializing physical volumes on disks: {:?}", disks);
    run_cmd("pvcreate", &pv_args)?;

    // 2. Run vgcreate
    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        vg_args.push(d.as_str());
    }
    println!("Creating storage pool '{}'...", pool_name);
    run_cmd("vgcreate", &vg_args)?;

    println!("Successfully created storage pool '{}'.", pool_name);
    Ok(())
}

pub fn pool_add(pool_name: &str, disks: &[String]) -> Result<()> {
    // 1. Run pvcreate on new disks
    let mut pv_args: Vec<&str> = vec!["-y", "-ff"];
    for d in disks {
        pv_args.push(d.as_str());
    }
    println!("Initializing physical volumes on new disks: {:?}", disks);
    run_cmd("pvcreate", &pv_args)?;

    // 2. Run vgextend
    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        vg_args.push(d.as_str());
    }
    println!("Adding disks to storage pool '{}'...", pool_name);
    run_cmd("vgextend", &vg_args)?;

    println!("Successfully extended storage pool '{}'.", pool_name);
    Ok(())
}

pub fn volume_create(
    pool_name: &str,
    vol_name: &str,
    size: &str,
    stripes: Option<usize>,
    stripe_size: Option<&str>,
) -> Result<()> {
    // 1. Resolve number of stripes
    let resolved_stripes = match stripes {
        Some(s) => s,
        None => {
            let count = get_pool_disk_count(pool_name).unwrap_or(1);
            println!("Auto-detected {} disk(s) in pool '{}'", count, pool_name);
            count
        }
    };

    // 2. Run lvcreate
    if resolved_stripes > 1 {
        let size_str = stripe_size.unwrap_or("512K");
        let stripes_str = resolved_stripes.to_string();
        println!(
            "Creating striped volume '{}' in pool '{}' with size {}, striped across {} disks (stripe size {})",
            vol_name, pool_name, size, stripes_str, size_str
        );
        run_cmd(
            "lvcreate",
            &[
                "-y",
                "-i",
                &stripes_str,
                "-I",
                size_str,
                "-n",
                vol_name,
                "-L",
                size,
                pool_name,
            ],
        )?;
    } else {
        println!(
            "Creating linear volume '{}' in pool '{}' with size {}",
            vol_name, pool_name, size
        );
        run_cmd("lvcreate", &["-y", "-n", vol_name, "-L", size, pool_name])?;
    }

    println!(
        "Successfully created volume. It is accessible at /dev/{}/{}",
        pool_name, vol_name
    );
    Ok(())
}

fn get_pool_disk_count(pool_name: &str) -> Option<usize> {
    let output = Command::new("vgs")
        .args(["-o", "pv_count", "--noheadings", pool_name])
        .output()
        .ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim().parse::<usize>().ok()
    } else {
        None
    }
}

pub fn volume_extend(pool_name: &str, vol_name: &str, add_size: &str) -> Result<()> {
    let lv_path = format!("/dev/{}/{}", pool_name, vol_name);
    let size_arg = format!("+{}", add_size);
    println!("Extending volume '{}' by {}...", lv_path, add_size);

    run_cmd("lvextend", &["-L", &size_arg, &lv_path])?;

    println!("Successfully extended volume.");
    Ok(())
}

pub fn detect_squeezefs_volume(device_path: &str) -> Option<String> {
    use std::fs::File;
    use std::io::Read;

    let mut file = File::open(device_path).ok()?;
    let mut buf = vec![0u8; 512];
    file.read_exact(&mut buf).ok()?;

    let magic = b"SQUEEZEFS_SUPER\x00";
    if buf[0..magic.len()] == *magic {
        let name_bytes = &buf[16..80];
        let end_idx = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        let name = String::from_utf8_lossy(&name_bytes[..end_idx])
            .trim()
            .to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn get_pv_allocated_extents(disk_path: &str) -> Option<usize> {
    let output = Command::new("pvs")
        .args(["-o", "pv_pe_alloc", "--noheadings", disk_path])
        .output()
        .ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim().parse::<usize>().ok()
    } else {
        None
    }
}

fn confirm_action(prompt: &str, default_yes: bool) -> bool {
    use std::io::{self, Write};
    if default_yes {
        print!("{} [Y/n]: ", prompt);
    } else {
        print!("{} [y/N]: ", prompt);
    }
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim().to_lowercase();
        if trimmed.is_empty() {
            default_yes
        } else {
            trimmed == "y" || trimmed == "yes"
        }
    } else {
        false
    }
}

pub fn pool_remove(pool_name: &str, disks: &[String], force_yes: bool) -> Result<()> {
    println!(
        "Removing disks {:?} from storage pool '{}'...",
        disks, pool_name
    );

    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        // Check if disk has allocated extents
        let extents = get_pv_allocated_extents(d).unwrap_or(0);
        if extents > 0 {
            use colored::Colorize;
            println!(
                "{}",
                format!(
                    "WARNING: Disk '{}' contains {} allocated LVM physical extents (data).",
                    d, extents
                )
                .yellow()
                .bold()
            );

            let migrate = force_yes || confirm_action(
                "Do you want to migrate this data to other disks in the pool using pvmove first?",
                true,
            );

            if migrate {
                println!(
                    "Migrating data off '{}' via pvmove (this may take some time)...",
                    d
                );
                if let Err(e) = run_cmd("pvmove", &[d.as_str()]) {
                    println!("{}", format!("ERROR: pvmove failed: {:?}", e).red().bold());
                    let proceed = force_yes || confirm_action(
                        "Do you still want to force remove the disk? (This WILL cause data loss!)",
                        false,
                    );
                    if !proceed {
                        return Err(SqueezefsError::InvalidOperation(
                            "Disk removal cancelled by user due to migration failure".to_string(),
                        ));
                    }
                } else {
                    println!("Data migrated successfully from '{}'.", d);
                }
            } else {
                let proceed = force_yes || confirm_action(
                    "Do you want to proceed with disk removal? (This WILL destroy data on this disk!)",
                    false,
                );
                if !proceed {
                    return Err(SqueezefsError::InvalidOperation(
                        "Disk removal cancelled by user".to_string(),
                    ));
                }
            }
        }
        vg_args.push(d.as_str());
    }

    run_cmd("vgreduce", &vg_args)?;

    let mut pv_args: Vec<&str> = vec!["-y"];
    for d in disks {
        pv_args.push(d.as_str());
    }
    run_cmd("pvremove", &pv_args)?;

    println!("Successfully removed disks from pool '{}'.", pool_name);
    Ok(())
}

pub fn pool_delete(pool_name: &str, force_yes: bool) -> Result<()> {
    // 1. Scan logical volumes in VG for SqueezeFS superblocks
    let mut detected_vols = Vec::new();
    let lvs_output = Command::new("lvs")
        .args(["-o", "lv_path", "--noheadings", pool_name])
        .output();
    if let Ok(output) = lvs_output {
        if output.status.success() {
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            for line in stdout_str.lines() {
                let lv_path = line.trim();
                if !lv_path.is_empty() {
                    if let Some(fs_name) = detect_squeezefs_volume(lv_path) {
                        detected_vols.push(format!("{} (SqueezeFS: {})", lv_path, fs_name));
                    }
                }
            }
        }
    }

    if !detected_vols.is_empty() {
        use colored::Colorize;
        println!(
            "{}",
            format!(
                "WARNING: Storage pool '{}' contains SqueezeFS filesystem volume(s):",
                pool_name
            )
            .red()
            .bold()
        );
        for vol in &detected_vols {
            println!("  - {}", vol);
        }
        println!(
            "{}",
            "Deleting this storage pool will destroy these filesystems!"
                .red()
                .bold()
        );
    }

    let proceed = force_yes
        || confirm_action(
            &format!(
                "Are you sure you want to delete storage pool '{}' and all its volumes?",
                pool_name
            ),
            false,
        );

    if !proceed {
        return Err(SqueezefsError::InvalidOperation(
            "Storage pool deletion cancelled by user".to_string(),
        ));
    }

    // Get the physical volumes composing this VG to clean them up afterward
    let mut pv_paths = Vec::new();
    let pvs_output = Command::new("pvs")
        .args([
            "-o",
            "pv_name",
            "-S",
            &format!("vg_name={}", pool_name),
            "--noheadings",
        ])
        .output();
    if let Ok(out) = pvs_output {
        if out.status.success() {
            let stdout_str = String::from_utf8_lossy(&out.stdout);
            for line in stdout_str.lines() {
                let pv_path = line.trim().to_string();
                if !pv_path.is_empty() {
                    pv_paths.push(pv_path);
                }
            }
        }
    }

    println!("Deleting entire storage pool '{}'...", pool_name);
    run_cmd("vgremove", &["-y", pool_name])?;

    // Post-deletion: pvremove LVM labels and detach loops
    for pv_path in pv_paths {
        println!(
            "Auto-cleaning: Removing LVM metadata label on PV '{}'...",
            pv_path
        );
        let _ = run_cmd("pvremove", &["-y", "-ff", &pv_path]);

        if pv_path.starts_with("/dev/loop") {
            println!("Auto-cleaning: Detaching loop device '{}'...", pv_path);
            let _ = Command::new("losetup").args(["-d", &pv_path]).output();
        }
    }

    println!("Successfully deleted pool.");
    Ok(())
}

pub fn volume_delete(pool_name: &str, vol_name: &str, force_yes: bool) -> Result<()> {
    let lv_path = format!("/dev/{}/{}", pool_name, vol_name);

    // Check if the volume is a SqueezeFS filesystem
    if let Some(fs_name) = detect_squeezefs_volume(&lv_path) {
        use colored::Colorize;
        println!(
            "{}",
            format!(
                "WARNING: Logical Volume '{}' belongs to SqueezeFS volume '{}'.",
                lv_path, fs_name
            )
            .red()
            .bold()
        );
        println!(
            "{}",
            "Deleting this volume will destroy all files and metadata stored on it!"
                .red()
                .bold()
        );

        let proceed = force_yes
            || confirm_action(
                &format!(
                    "Are you sure you want to delete SqueezeFS volume '{}'?",
                    fs_name
                ),
                false,
            );
        if !proceed {
            return Err(SqueezefsError::InvalidOperation(
                "Volume deletion cancelled by user".to_string(),
            ));
        }
    } else {
        let proceed = force_yes
            || confirm_action(
                &format!(
                    "Are you sure you want to delete Logical Volume '{}'?",
                    lv_path
                ),
                false,
            );
        if !proceed {
            return Err(SqueezefsError::InvalidOperation(
                "Volume deletion cancelled by user".to_string(),
            ));
        }
    }

    println!("Deleting volume '{}'...", lv_path);
    run_cmd("lvremove", &["-y", &lv_path])?;
    println!("Successfully deleted volume.");
    Ok(())
}

/// Validate a data backing device path.
///
/// Policy: a backing device is any existing, **nonzero-sized** **regular
/// file** or **block device** (raw NVMe namespace, partition, dm/LVM
/// volume, loop device). Nothing else — no LVM/VG/NQN interrogation, no
/// path allow-lists. Non-I/O-capable node types (directories, character
/// devices, FIFOs, sockets), zero-sized backing, and missing paths are
/// rejected with specific errors.
pub fn validate_backing_device(path: &str) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::FileTypeExt;
    use std::path::Path;

    let path_buf = Path::new(path);
    if !path_buf.exists() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backing device path '{}' does not exist.",
            path
        )));
    }

    let real_path = fs::canonicalize(path_buf).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("Failed to resolve path '{}': {}", path, e))
    })?;

    let meta = fs::metadata(&real_path).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("Failed to stat '{}': {}", path, e))
    })?;
    let file_type = meta.file_type();

    if file_type.is_file() {
        if meta.len() == 0 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Backing file '{}' has zero size — create/extend it first (e.g. truncate -s <size>).",
                path
            )));
        }
        return Ok(());
    }

    if file_type.is_block_device() {
        // stat's size is 0 for device nodes; capacity lives in sysfs
        // (512-byte sectors). An absent sysfs entry is tolerated — the
        // device open will fail loudly later if the node is truly unusable.
        let sectors: Option<u64> = real_path.file_name().and_then(|name| {
            fs::read_to_string(format!("/sys/class/block/{}/size", name.to_string_lossy()))
                .ok()?
                .trim()
                .parse()
                .ok()
        });
        if sectors == Some(0) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Backing block device '{}' has zero capacity (e.g. an unattached loop device).",
                path
            )));
        }
        return Ok(());
    }

    if file_type.is_char_device() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backing device '{}' is a character device, not a block device. For NVMe, pass \
             the namespace block node (e.g. /dev/nvme0n1), not the controller (e.g. /dev/nvme0).",
            path
        )));
    }

    Err(SqueezefsError::InvalidOperation(format!(
        "Backing device '{}' is not a block device or regular file.",
        path
    )))
}

/// Is this LVM PV path one of ours — a loop device or a
/// SqueezeFS-served NVMe namespace (its subsystem NQN under the
/// product domain)?
///
/// The NQN lookup rides the strict-shape subsystem-first resolver
/// (`nvmeof::fabric::subsysnqn_of_namespace`): the pre-fix code
/// derived the controller name by string surgery on the namespace
/// basename (`rfind('n')` + `/sys/class/nvme/{ctrl}/subsysnqn`), which
/// is wrong on CONFIG_NVME_MULTIPATH kernels where the head node's
/// instance number is the SUBSYSTEM's — `/dev/nvme1n1` may be served
/// while no controller `nvme1` exists (the 2026-07-25 `nvme0c0n1` bug
/// class).
fn pv_is_squeezefs_backed(pv_trim: &str) -> bool {
    if pv_trim.starts_with("/dev/loop") {
        return true;
    }
    if !pv_trim.starts_with("/dev/nvme") {
        return false;
    }
    let Some(dev_name) = pv_trim.rsplit('/').next() else {
        return false;
    };
    crate::nvmeof::fabric::subsysnqn_of_namespace(
        std::path::Path::new(crate::nvmeof::fabric::SYSFS_NVME_SUBSYSTEM),
        std::path::Path::new(crate::nvmeof::fabric::SYSFS_NVME),
        dev_name,
    )
    .is_some_and(|nqn| nqn.starts_with("nqn.2026-06.io.squeezefs:"))
}

pub fn pool_list() -> Result<()> {
    let output = Command::new("vgs")
        .args(["-o", "vg_name", "--noheadings"])
        .output()
        .map_err(|e| SqueezefsError::InvalidOperation(format!("Failed to execute vgs: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "Command vgs failed: {}",
            stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut our_vgs = Vec::new();
    for line in stdout.lines() {
        let vg = line.trim();
        if !vg.is_empty() {
            let pvs_output = Command::new("pvs")
                .args([
                    "-o",
                    "pv_name",
                    "-S",
                    &format!("vg_name={}", vg),
                    "--noheadings",
                ])
                .output();
            if let Ok(pvs_out) = pvs_output {
                if pvs_out.status.success() {
                    let pvs_str = String::from_utf8_lossy(&pvs_out.stdout);
                    let mut is_ours = true;
                    let mut count = 0;
                    for pv in pvs_str.lines() {
                        let pv_trim = pv.trim();
                        if !pv_trim.is_empty() {
                            count += 1;
                            if !pv_is_squeezefs_backed(pv_trim) {
                                is_ours = false;
                                break;
                            }
                        }
                    }
                    if is_ours && count > 0 {
                        our_vgs.push(vg.to_string());
                    }
                }
            }
        }
    }

    if our_vgs.is_empty() {
        println!("No squeezefs storage pools found.");
        return Ok(());
    }

    let mut args = vec![];
    for vg in &our_vgs {
        args.push(vg.as_str());
    }
    let output = Command::new("vgs")
        .args(&args)
        .output()
        .map_err(|e| SqueezefsError::InvalidOperation(format!("Failed to execute vgs: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "Command vgs failed: {}",
            stderr
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{}", stdout);
    Ok(())
}

pub fn volume_list() -> Result<()> {
    let output = Command::new("lvs")
        .args(["-o", "vg_name,lv_name,lv_path", "--noheadings"])
        .output()
        .map_err(|e| SqueezefsError::InvalidOperation(format!("Failed to execute lvs: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "Command lvs failed: {}",
            stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut our_lvs = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let vg = parts[0];
            let lv_path = parts[2];
            let pvs_output = Command::new("pvs")
                .args([
                    "-o",
                    "pv_name",
                    "-S",
                    &format!("vg_name={}", vg),
                    "--noheadings",
                ])
                .output();
            if let Ok(pvs_out) = pvs_output {
                if pvs_out.status.success() {
                    let pvs_str = String::from_utf8_lossy(&pvs_out.stdout);
                    let mut is_ours = true;
                    let mut count = 0;
                    for pv in pvs_str.lines() {
                        let pv_trim = pv.trim();
                        if !pv_trim.is_empty() {
                            count += 1;
                            if !pv_is_squeezefs_backed(pv_trim) {
                                is_ours = false;
                                break;
                            }
                        }
                    }
                    if is_ours && count > 0 {
                        our_lvs.push(lv_path.to_string());
                    }
                }
            }
        }
    }

    if our_lvs.is_empty() {
        println!("No squeezefs storage volumes found.");
        return Ok(());
    }

    let mut args = vec![];
    for lv in &our_lvs {
        args.push(lv.as_str());
    }
    let output = Command::new("lvs")
        .args(&args)
        .output()
        .map_err(|e| SqueezefsError::InvalidOperation(format!("Failed to execute lvs: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "Command lvs failed: {}",
            stderr
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{}", stdout);
    Ok(())
}

pub fn get_loop_backing_file(loop_device: &str) -> Option<String> {
    use std::fs;
    let path = std::path::Path::new(loop_device);
    if let Some(dev_name) = path.file_name() {
        let backing_path = format!(
            "/sys/class/block/{}/loop/backing_file",
            dev_name.to_string_lossy()
        );
        if let Ok(content) = fs::read_to_string(backing_path) {
            let trim = content.trim().to_string();
            if !trim.is_empty() {
                return Some(trim);
            }
        }
    }
    None
}

pub fn bind_loop_device(loop_device: &str, backing_file: &str) -> Result<()> {
    log::info!(
        "Automatically binding loop device {} to backing file {}...",
        loop_device,
        backing_file
    );
    let output = std::process::Command::new("losetup")
        .args([loop_device, backing_file])
        .output()
        .map_err(|e| SqueezefsError::InvalidOperation(format!("losetup execute failed: {}", e)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezefsError::InvalidOperation(format!(
            "losetup failed: {}",
            stderr
        )));
    }
    Ok(())
}

pub fn extract_lvm_loop_info(
    backing_dev: &str,
) -> (Option<String>, std::collections::HashMap<String, String>) {
    use std::fs;
    use std::path::Path;
    let mut vg_name = None;
    let mut loop_pvs = std::collections::HashMap::new();

    let path_buf = Path::new(backing_dev);
    let real_path = match fs::canonicalize(path_buf) {
        Ok(rp) => rp,
        Err(_) => return (None, loop_pvs),
    };
    let real_path_str = real_path.to_string_lossy();

    let mut lvs_target = real_path_str.to_string();
    if real_path_str.starts_with("/dev/dm-") {
        if let Some(dev_name) = real_path.file_name() {
            let dm_name_path = format!("/sys/block/{}/dm/name", dev_name.to_string_lossy());
            if let Ok(name) = fs::read_to_string(dm_name_path) {
                lvs_target = format!("/dev/mapper/{}", name.trim());
            }
        }
    }

    // Get VG name
    let output = std::process::Command::new("lvs")
        .args(["-o", "vg_name", "--noheadings", &lvs_target])
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            let vg = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !vg.is_empty() {
                vg_name = Some(vg.clone());

                // Get PVs in VG
                let pvs_output = std::process::Command::new("pvs")
                    .args([
                        "-o",
                        "pv_name",
                        "-S",
                        &format!("vg_name={}", vg),
                        "--noheadings",
                    ])
                    .output();
                if let Ok(pvs_out) = pvs_output {
                    if pvs_out.status.success() {
                        let pvs_str = String::from_utf8_lossy(&pvs_out.stdout);
                        for pv in pvs_str.lines() {
                            let pv_trim = pv.trim();
                            if pv_trim.starts_with("/dev/loop") {
                                if let Some(backing_file) = get_loop_backing_file(pv_trim) {
                                    loop_pvs.insert(pv_trim.to_string(), backing_file);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback: if backing_dev is a loop device directly
    if real_path_str.starts_with("/dev/loop") {
        if let Some(backing_file) = get_loop_backing_file(&real_path_str) {
            loop_pvs.insert(real_path_str.into_owned(), backing_file);
        }
    }

    (vg_name, loop_pvs)
}

pub fn restore_lvm_loop_devices(
    vg_name: Option<&str>,
    loop_pvs: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut bound_any = false;
    for (loop_dev, backing_file) in loop_pvs {
        let is_bound = get_loop_backing_file(loop_dev).is_some();
        if !is_bound {
            log::info!(
                "Re-binding loop device {} to flat file {}...",
                loop_dev,
                backing_file
            );
            bind_loop_device(loop_dev, backing_file)?;
            bound_any = true;
        }
    }
    if let Some(vg) = vg_name {
        if bound_any {
            log::info!("Activating LVM Volume Group {}...", vg);
            let output = std::process::Command::new("vgchange")
                .args(["-ay", vg])
                .output()
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("vgchange execute failed: {}", e))
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "vgchange -ay {} failed: {}",
                    vg, stderr
                )));
            }
        }
    }
    Ok(())
}
