use crate::error::{Result, SqueezefsError};
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
    // Default fallback. Data volumes start EMPTY: real volumes register at
    // mount from the resolved data paths, and `config data-volume add` fills
    // this map explicitly. The old hardcoded `backend_0` seed (a legacy
    // key-resolution alias, not a volume — its `backing_dev` was not even a
    // real path) leaked a phantom entry into every runtime config.
    let data_volumes = HashMap::new();
    let data_volume_statuses = HashMap::new();

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

/// Read the format-recorded volume-set config off the FIRST metadata
/// volume via a read-only probe mount (nothing written, safe against a
/// volume another process has live-mounted). Fails loud on blank /
/// legacy-v2 / unformatted volumes.
async fn read_format_config(first_meta: &str) -> Result<crate::FormatConfig> {
    let vol = crate::meta_backend::open_volume_probe(first_meta).await?;
    let val = vol
        .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "format configuration not found on metadata volume {first_meta}; \
                 is this volume formatted?"
            ))
        })?;
    serde_json::from_slice(&val).map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "failed to parse the format configuration on {first_meta}: {e}"
        ))
    })
}

/// `squeezefs config get-cache-paths`: the staging/cache directories the
/// filesystem was formatted with (`None`/empty ⇒ permanently cache-less).
pub async fn get_cache_paths(meta_lvs: &[String]) -> Result<Option<Vec<PathBuf>>> {
    let first = meta_lvs.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("at least one metadata volume is required".to_string())
    })?;
    Ok(read_format_config(first).await?.disk_cache_paths)
}

/// `squeezefs config set-cache-paths`: the ONLY way to change a
/// filesystem's staging/cache directories after format (mount rejects the
/// flag — cache-path policy).
///
/// Guarded like `format` itself:
/// - every metadata volume runs the [`format_preflight`] live-client gate
///   (`force` semantics: an already-formatted volume is fine, a volume any
///   client has LIVE-mounted refuses — changing cache paths under an
///   active mount is never safe);
/// - the volume set must be formatted (the config read fails loud
///   otherwise) — checked BEFORE any directory is touched;
/// - the NEW directories are wiped + recreated (the same cleanliness
///   `format --disk-cache-paths` applies), so the next mount stamps a
///   fresh staging generation into empty dirs (no discard noise). Content
///   safety does not depend on the wipe: staging generation-binding
///   discards foreign content at mount anyway.
///
/// The rewrite itself is one setxattr transaction on the FIRST volume's
/// root inode (where format recorded it), made durable by the v3 journal
/// and closed with a clean checkpoint shutdown.
///
/// [`format_preflight`]: crate::meta_backend::kv::builder::format_preflight
pub async fn set_cache_paths(meta_lvs: &[String], paths: &[PathBuf]) -> Result<()> {
    let first = meta_lvs.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("at least one metadata volume is required".to_string())
    })?;
    if paths.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "at least one cache path is required".to_string(),
        ));
    }

    // 1. Live-client gate on EVERY volume before anything is touched.
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true).await?;
    }

    // 2. The volume set must be formatted; read the config to rewrite.
    let mut cfg = read_format_config(first).await?;

    // 3. Wipe + recreate the NEW dirs (format-grade cleanliness).
    for dir in paths {
        if dir.exists() {
            tokio::fs::remove_dir_all(dir).await?;
        }
        tokio::fs::create_dir_all(dir).await?;
    }

    // 4. Rewrite the format config on the first volume (journal-durable
    //    commit + clean checkpoint shutdown).
    cfg.disk_cache_paths = Some(paths.to_vec());
    let bytes = serde_json::to_vec(&cfg).map_err(|e| {
        SqueezefsError::InvalidOperation(format!("failed to serialize the format config: {e}"))
    })?;
    let vol = crate::meta_backend::open_volume_for_mount(first).await?;
    crate::meta_backend::Metadata::setxattr(
        vol.as_ref(),
        1,
        crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR,
        &bytes,
    )
    .await?;
    vol.shutdown().await.map_err(SqueezefsError::from)?;
    Ok(())
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
    // Route-config bookkeeping only: point the ino-routing redirection at
    // the new volume and flip the statuses. (The retired v2 backend used
    // to also best-effort copy fixed-geometry inode slots here — a
    // half-measure that never carried dentries/xattrs; no data movement
    // is performed.)
    println!(
        "Redirecting metadata volume {} to {} in the runtime config (no data is moved).",
        from_volume, to_volume
    );
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
