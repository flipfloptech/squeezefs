pub mod gds;
pub mod lru;
pub mod nvme;

use crate::backend::RustFsClient;
use crate::error::{Result, SqueezefsError};
use std::path::PathBuf;

#[derive(Clone)]
pub struct TieredCache {
    pub gds: gds::GdsCache,
    pub read_lru: lru::LruCache,
    pub write_lru: lru::LruCache,
    pub nvme: nvme::NvmeStaging,
}

impl TieredCache {
    pub fn new(
        staging_dirs: Vec<PathBuf>,
        read_mem_cache_size: Option<&str>,
        write_mem_cache_size: Option<&str>,
        read_disk_cache_size: Option<&str>,
        write_disk_cache_size: Option<&str>,
        backend: RustFsClient,
        redis_client: crate::dlm::MetaClient,
    ) -> Result<Self> {
        let gds = gds::GdsCache::new(staging_dirs.clone());

        // 1. Get memory size limit
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory(); // In bytes

        let read_mem_limit = if let Some(cfg) = read_mem_cache_size.filter(|c| !c.is_empty() && *c != "none") {
            parse_size_string(cfg, total_memory)?
        } else {
            // Default 10% of system RAM
            total_memory / 10
        };

        let write_mem_limit = if let Some(cfg) = write_mem_cache_size.filter(|c| !c.is_empty() && *c != "none") {
            parse_size_string(cfg, total_memory)?
        } else {
            // Default 10% of system RAM
            total_memory / 10
        };

        let read_lru = lru::LruCache::with_capacity(read_mem_limit);
        let write_lru = lru::LruCache::with_capacity(write_mem_limit);

        // 2. Get disk size limit
        let aggregate_capacity = get_aggregate_disk_capacity(&staging_dirs);

        let read_disk_limit = if let Some(cfg) = read_disk_cache_size.filter(|c| !c.is_empty() && *c != "none") {
            parse_size_string(cfg, aggregate_capacity)?
        } else {
            // Default 25% of aggregate capacity
            aggregate_capacity / 4
        };

        let write_disk_limit = if let Some(cfg) = write_disk_cache_size.filter(|c| !c.is_empty() && *c != "none") {
            parse_size_string(cfg, aggregate_capacity)?
        } else {
            // Default 25% of aggregate capacity
            aggregate_capacity / 4
        };

        let nvme = nvme::NvmeStaging::new(
            staging_dirs,
            write_disk_limit,
            read_disk_limit,
            backend,
            redis_client,
        )?;
        Ok(Self {
            gds,
            read_lru,
            write_lru,
            nvme,
        })
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
    let mut total_capacity = 0;

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        for path in paths {
            let abs_path = path.canonicalize().unwrap_or_else(|_| path.clone());
            let c_path = match CString::new(abs_path.as_os_str().as_bytes()) {
                Ok(s) => s,
                Err(_) => continue,
            };

            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                    total_capacity += stat.f_blocks as u64 * stat.f_frsize as u64;
                }
            }
        }
    }

    if total_capacity == 0 {
        100 * 1024 * 1024 * 1024 // Fallback 100GB
    } else {
        total_capacity
    }
}

/// Helper function to parse duration strings like "500ms", "5s", "1000" into a standard Duration.
pub fn parse_duration(val: &str) -> Result<std::time::Duration> {
    let s = val.trim().to_lowercase();
    if s.ends_with("ms") {
        let num_str = &s[..s.len() - 2];
        let ms = num_str.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_millis(ms))
    } else if s.ends_with('s') {
        let num_str = &s[..s.len() - 1];
        let secs = num_str.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_secs(secs))
    } else {
        let ms = s.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_millis(ms))
    }
}

