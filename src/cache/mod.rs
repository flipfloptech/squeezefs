pub mod gds;
pub mod lru;
pub mod nvme;

use crate::backend::RustFsClient;
use crate::error::{Result, SqueezefsError};
use std::path::PathBuf;

#[derive(Clone)]
pub struct TieredCache {
    pub gds: gds::GdsCache,
    pub lru: lru::LruCache,
    pub nvme: nvme::NvmeStaging,
}

impl TieredCache {
    pub fn new(
        staging_dirs: Vec<PathBuf>,
        mem_cache_size: Option<&str>,
        disk_cache_size: Option<&str>,
        backend: RustFsClient,
        redis_client: crate::dlm::MetaClient,
    ) -> Result<Self> {
        let gds = gds::GdsCache::new();

        // 1. Get memory size limit
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory(); // In bytes
        let mem_limit = if let Some(mem_cfg) = mem_cache_size {
            parse_size_string(mem_cfg, total_memory)?
        } else {
            // Default 20% of system RAM
            total_memory / 5
        };
        let lru = lru::LruCache::with_capacity(mem_limit);

        // 2. Get disk size limit
        let aggregate_capacity = get_aggregate_disk_capacity(&staging_dirs);
        let disk_limit = if let Some(disk_cfg) = disk_cache_size {
            parse_size_string(disk_cfg, aggregate_capacity)?
        } else {
            // Default 50% of aggregate capacity
            aggregate_capacity / 2
        };

        let nvme = nvme::NvmeStaging::new(staging_dirs, disk_limit, backend, redis_client)?;
        Ok(Self { gds, lru, nvme })
    }
}

/// Helper function to parse size configs like "128GB", "50%" into raw bytes.
pub fn parse_size_string(val: &str, system_total: u64) -> Result<u64> {
    let s = val.trim();
    if s.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "Empty size configuration".to_string(),
        ));
    }

    if let Some(stripped) = s.strip_suffix('%') {
        let pct_str = stripped.trim();
        let pct = pct_str.parse::<f64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "Invalid percentage format '{}': {:?}",
                pct_str, e
            ))
        })?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Percentage out of bounds: {}",
                pct
            )));
        }
        return Ok((system_total as f64 * pct / 100.0) as u64);
    }

    let mut num_end = s.len();
    for (i, c) in s.char_indices() {
        if !c.is_numeric() && c != '.' {
            num_end = i;
            break;
        }
    }

    let num_str = s[..num_end].trim();
    let unit_str = s[num_end..].trim().to_uppercase();

    let val_f = num_str.parse::<f64>().map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "Invalid size numeric value '{}': {:?}",
            num_str, e
        ))
    })?;

    let multiplier: u64 = match unit_str.as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        "TB" | "T" => 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Unknown size unit '{}'",
                unit_str
            )))
        }
    };

    Ok((val_f * multiplier as f64) as u64)
}

/// Helper function to calculate aggregate capacity of disks mapped to staging paths.
pub fn get_aggregate_disk_capacity(paths: &[PathBuf]) -> u64 {
    use std::collections::HashSet;
    use sysinfo::Disks;

    let disks = Disks::new_with_refreshed_list();
    let mut matched_disk_names = HashSet::new();
    let mut total_capacity = 0;

    for path in paths {
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.clone());
        let path_str = abs_path.to_string_lossy();

        let mut best_match: Option<(&sysinfo::Disk, usize)> = None;
        for disk in &disks {
            let mount_str = disk.mount_point().to_string_lossy();
            if path_str.starts_with(&*mount_str) {
                let len = mount_str.len();
                if best_match.is_none() || len > best_match.unwrap().1 {
                    best_match = Some((disk, len));
                }
            }
        }

        if let Some((disk, _)) = best_match {
            let disk_name = disk.name().to_string_lossy().to_string();
            if matched_disk_names.insert(disk_name) {
                total_capacity += disk.total_space();
            }
        }
    }

    if total_capacity == 0 {
        100 * 1024 * 1024 * 1024 // Fallback 100GB
    } else {
        total_capacity
    }
}
