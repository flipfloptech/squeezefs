use crate::error::{Result, SqueezefsError};
use log::{error, info};
use std::process::Command;

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
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
    let mut pv_args: Vec<&str> = vec!["-y"]; // Yes to prompts (like wiping signatures)
    for d in disks {
        pv_args.push(d.as_str());
    }
    info!("Initializing physical volumes on disks: {:?}", disks);
    run_cmd("pvcreate", &pv_args)?;

    // 2. Run vgcreate
    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        vg_args.push(d.as_str());
    }
    info!("Creating storage pool '{}'", pool_name);
    run_cmd("vgcreate", &vg_args)?;

    info!("Successfully created storage pool '{}'", pool_name);
    Ok(())
}

pub fn pool_add(pool_name: &str, disks: &[String]) -> Result<()> {
    // 1. Run pvcreate on new disks
    let mut pv_args: Vec<&str> = vec!["-y"];
    for d in disks {
        pv_args.push(d.as_str());
    }
    info!("Initializing physical volumes on new disks: {:?}", disks);
    run_cmd("pvcreate", &pv_args)?;

    // 2. Run vgextend
    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        vg_args.push(d.as_str());
    }
    info!("Adding disks to storage pool '{}'", pool_name);
    run_cmd("vgextend", &vg_args)?;

    info!("Successfully extended storage pool '{}'", pool_name);
    Ok(())
}

pub fn volume_create(pool_name: &str, vol_name: &str, size: &str) -> Result<()> {
    info!(
        "Creating volume '{}' in pool '{}' with size {}",
        vol_name, pool_name, size
    );

    // Run lvcreate
    run_cmd("lvcreate", &["-y", "-n", vol_name, "-L", size, pool_name])?;

    info!(
        "Successfully created volume. It is accessible at /dev/{}/{}",
        pool_name, vol_name
    );
    Ok(())
}

pub fn volume_extend(pool_name: &str, vol_name: &str, add_size: &str) -> Result<()> {
    let lv_path = format!("/dev/{}/{}", pool_name, vol_name);
    let size_arg = format!("+{}", add_size);
    info!("Extending volume '{}' by {}", lv_path, add_size);

    run_cmd("lvextend", &["-L", &size_arg, &lv_path])?;

    info!("Successfully extended volume.");
    Ok(())
}
