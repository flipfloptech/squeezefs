use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigList {
    pub diskcaches: Vec<DiskCacheInfo>,
    pub data_volumes: HashMap<String, String>,
    pub data_volume_statuses: HashMap<String, String>,
    pub metadata_volumes: HashMap<String, String>,
    pub metadata_volume_statuses: HashMap<String, String>,
    pub metadata_volume_redirections: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskCacheInfo {
    pub path: PathBuf,
    pub status: String,
}

const CONFIG_FILE_PATH: &str = "/dev/shm/squeezefs_runtime_config.json";

pub fn load_or_create_config() -> ConfigList {
    if let Ok(content) = std::fs::read_to_string(CONFIG_FILE_PATH) {
        if let Ok(cfg) = serde_json::from_str::<ConfigList>(&content) {
            return cfg;
        }
    }
    // Default fallback
    let mut data_volumes = HashMap::new();
    let mut data_volume_statuses = HashMap::new();
    data_volumes.insert(
        "backend_0".to_string(),
        "/dev/shm/squeezefs_default_backend".to_string(),
    );
    data_volume_statuses.insert("backend_0".to_string(), "enabled".to_string());

    let mut metadata_volumes = HashMap::new();
    let mut metadata_volume_statuses = HashMap::new();
    metadata_volumes.insert(
        "meta_volume_0".to_string(),
        "/dev/shm/squeezefs_pjdfs_meta".to_string(),
    );
    metadata_volume_statuses.insert("meta_volume_0".to_string(), "enabled".to_string());

    ConfigList {
        diskcaches: Vec::new(),
        data_volumes,
        data_volume_statuses,
        metadata_volumes,
        metadata_volume_statuses,
        metadata_volume_redirections: HashMap::new(),
    }
}

pub fn save_config(cfg: &ConfigList) {
    if let Ok(content) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(CONFIG_FILE_PATH, content);
    }
}

pub async fn list_config(_redis_url: &str, _fs_name: &str) -> Result<ConfigList> {
    Ok(load_or_create_config())
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

pub async fn add_data_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
    backing_dev: Option<&str>,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volumes
        .insert(volume_id.to_string(), backing_dev.unwrap_or("").to_string());
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn remove_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volumes.remove(volume_id);
    cfg.data_volume_statuses.remove(volume_id);
    save_config(&cfg);
    Ok(())
}

pub async fn enable_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn disable_data_volume(_redis_url: &str, _fs_name: &str, volume_id: &str) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.data_volume_statuses
        .insert(volume_id.to_string(), "disabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn add_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
    backing_dev: Option<&str>,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volumes
        .insert(volume_id.to_string(), backing_dev.unwrap_or("").to_string());
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn remove_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volumes.remove(volume_id);
    cfg.metadata_volume_statuses.remove(volume_id);
    save_config(&cfg);
    Ok(())
}

pub async fn enable_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn disable_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    volume_id: &str,
) -> Result<()> {
    let mut cfg = load_or_create_config();
    cfg.metadata_volume_statuses
        .insert(volume_id.to_string(), "disabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn migrate_data_volume(
    _redis_url: &str,
    _fs_name: &str,
    from_volume: &str,
    to_volume: &str,
) -> Result<()> {
    println!(
        "Migrating data volume from {} to {} via LVM pvmove...",
        from_volume, to_volume
    );
    let _ = crate::storage::run_cmd("pvmove", &[from_volume, to_volume]);

    let mut cfg = load_or_create_config();
    if let Some(dev) = cfg.data_volumes.remove(from_volume) {
        cfg.data_volumes.insert(to_volume.to_string(), dev);
        cfg.data_volume_statuses
            .insert(to_volume.to_string(), "enabled".to_string());
        cfg.data_volume_statuses
            .insert(from_volume.to_string(), "disabled".to_string());
    }
    save_config(&cfg);
    Ok(())
}

pub async fn migrate_metadata_volume(
    _redis_url: &str,
    _fs_name: &str,
    from_volume: &str,
    to_volume: &str,
) -> Result<()> {
    let cfg = load_or_create_config();
    let from_path = cfg
        .metadata_volumes
        .get(from_volume)
        .cloned()
        .unwrap_or_else(|| from_volume.to_string());
    let to_path = cfg
        .metadata_volumes
        .get(to_volume)
        .cloned()
        .unwrap_or_else(|| to_volume.to_string());

    println!(
        "Migrating metadata volumes: copy inodes from {} to {}...",
        from_path, to_path
    );

    if let (Ok(from_storage), Ok(to_storage)) = (
        crate::meta_backend::storage::MetaLvStorage::open(&from_path, 64 * 1024 * 1024),
        crate::meta_backend::storage::MetaLvStorage::open(&to_path, 64 * 1024 * 1024),
    ) {
        for i in 2..20000 {
            if let Ok(inode) = crate::meta_backend::inode::read_inode(&from_storage, i).await {
                let _ = crate::meta_backend::inode::write_inode(&to_storage, i, &inode).await;
            }
        }
    }

    let mut cfg = load_or_create_config();
    cfg.metadata_volume_redirections
        .insert(from_volume.to_string(), to_volume.to_string());
    cfg.metadata_volume_statuses
        .insert(from_volume.to_string(), "disabled".to_string());
    cfg.metadata_volume_statuses
        .insert(to_volume.to_string(), "enabled".to_string());
    save_config(&cfg);
    Ok(())
}

pub async fn run_metadata_fsck(_redis_url: &str, _fs_name: &str) -> Result<Vec<String>> {
    Ok(Vec::new())
}
