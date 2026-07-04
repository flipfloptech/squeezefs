use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigList {
    pub diskcaches: Vec<DiskCacheInfo>,
    pub backends: HashMap<String, String>,
    pub backend_statuses: HashMap<String, String>,
    pub active_write_backend: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskCacheInfo {
    pub path: PathBuf,
    pub status: String,
}

pub async fn list_config(_redis_url: &str, _fs_name: &str) -> Result<ConfigList> {
    Ok(ConfigList {
        diskcaches: Vec::new(),
        backends: HashMap::new(),
        backend_statuses: HashMap::new(),
        active_write_backend: "backend_0".to_string(),
    })
}

pub async fn set_config_quota(
    _redis_url: &str,
    _fs_name: &str,
    _key: &str,
    _value: &str,
) -> Result<()> {
    Ok(())
}

pub async fn add_disk_cache_path(
    _redis_url: &str,
    _fs_name: &str,
    _path: &Path,
    _force: bool,
) -> Result<()> {
    Ok(())
}

pub async fn remove_disk_cache_path(
    _redis_url: &str,
    _fs_name: &str,
    _path: &Path,
    _force: bool,
) -> Result<()> {
    Ok(())
}

pub async fn enable_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn disable_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn flush_disk_cache_path(_redis_url: &str, _fs_name: &str, _path: &Path) -> Result<()> {
    Ok(())
}

pub async fn add_storage_backend(
    _redis_url: &str,
    _fs_name: &str,
    _backend_id: &str,
    _backing_dev: Option<&str>,
    _ip: Option<&str>,
    _port: Option<u16>,
    _subnqn: Option<&str>,
    _capacity: Option<u64>,
) -> Result<()> {
    Ok(())
}

pub async fn remove_storage_backend(
    _redis_url: &str,
    _fs_name: &str,
    _backend_id: &str,
    _force: bool,
) -> Result<()> {
    Ok(())
}

pub async fn set_active_backend(_redis_url: &str, _fs_name: &str, _backend_id: &str) -> Result<()> {
    Ok(())
}

pub async fn run_metadata_fsck(_redis_url: &str, _fs_name: &str) -> Result<Vec<String>> {
    Ok(Vec::new())
}
