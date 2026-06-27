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

pub fn pool_remove(pool_name: &str, disks: &[String]) -> Result<()> {
    info!("Removing disks {:?} from storage pool '{}'", disks, pool_name);
    // 1. Move any data off the physical volumes if possible (pvmove)
    // Note: pvmove might take a long time and requires free space elsewhere in the VG.
    // For simplicity/safety, we just try to vgreduce. If it fails, the user must ensure it's empty.
    let mut vg_args: Vec<&str> = vec![pool_name];
    for d in disks {
        // Attempt pvmove first (ignore errors if it's already empty)
        let _ = run_cmd("pvmove", &[d.as_str()]);
        vg_args.push(d.as_str());
    }

    run_cmd("vgreduce", &vg_args)?;

    // 2. Remove the physical volume signature
    let mut pv_args: Vec<&str> = vec!["-y"];
    for d in disks {
        pv_args.push(d.as_str());
    }
    run_cmd("pvremove", &pv_args)?;

    info!("Successfully removed disks from pool '{}'", pool_name);
    Ok(())
}

pub fn pool_delete(pool_name: &str) -> Result<()> {
    info!("Deleting entire storage pool '{}'", pool_name);
    run_cmd("vgremove", &["-y", pool_name])?;
    info!("Successfully deleted pool.");
    Ok(())
}

pub fn volume_delete(pool_name: &str, vol_name: &str) -> Result<()> {
    let lv_path = format!("/dev/{}/{}", pool_name, vol_name);
    info!("Deleting volume '{}'", lv_path);
    run_cmd("lvremove", &["-y", &lv_path])?;
    info!("Successfully deleted volume.");
    Ok(())
}
