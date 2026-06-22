use crate::backend::{parse_backend_and_key, RustFsClient};
use crate::error::{Result, SqueezefsError};
use crate::recovery::recover_staging;
use redis::AsyncCommands;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigList {
    pub diskcaches: Vec<DiskCacheInfo>,
    pub backends: HashMap<String, String>,
    pub active_write_backend: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskCacheInfo {
    pub path: PathBuf,
    pub status: String,
}

/// Helper to connect to redis
async fn connect_redis(redis_url: &str) -> Result<redis::aio::MultiplexedConnection> {
    let client = redis::Client::open(redis_url)
        .map_err(SqueezefsError::Redis)?;
    let con = client.get_multiplexed_tokio_connection().await
        .map_err(SqueezefsError::Redis)?;
    Ok(con)
}

pub async fn list_config(redis_url: &str, _fs_name: &str) -> Result<ConfigList> {
    let mut con = connect_redis(redis_url).await?;

    let format_fields: HashMap<String, String> = con.hgetall("squeezefs:format").await.unwrap_or_default();
    
    let active_write_backend = format_fields
        .get("active_write_backend")
        .cloned()
        .unwrap_or_else(|| "backend_0".to_string());

    let disk_cache_paths_str = format_fields.get("disk_cache_paths").cloned().unwrap_or_default();
    let paths: Vec<PathBuf> = if disk_cache_paths_str.is_empty() {
        Vec::new()
    } else {
        disk_cache_paths_str.split(',').map(PathBuf::from).collect()
    };

    let status_map: HashMap<String, String> = con.hgetall("squeezefs:diskcache:status").await.unwrap_or_default();

    let mut diskcaches = Vec::new();
    for p in paths {
        let p_str = p.to_string_lossy().to_string();
        let status = status_map.get(&p_str).cloned().unwrap_or_else(|| "enabled".to_string());
        diskcaches.push(DiskCacheInfo { path: p, status });
    }

    let backends: HashMap<String, String> = con.hgetall("squeezefs:backends").await.unwrap_or_default();

    Ok(ConfigList {
        diskcaches,
        backends,
        active_write_backend,
    })
}

pub async fn add_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con.hget("squeezefs:format", "disk_cache_paths").await?;
    let mut paths: Vec<String> = current_paths_str
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    if paths.contains(&path_str) {
        return Ok(()); // Already exists
    }

    paths.push(path_str.clone());
    let new_paths_str = paths.join(",");

    let _: () = redis::pipe()
        .hset("squeezefs:format", "disk_cache_paths", new_paths_str)
        .hset("squeezefs:diskcache:status", &path_str, "enabled")
        .query_async(&mut con)
        .await?;

    Ok(())
}

pub async fn disable_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let exists: bool = con.hexists("squeezefs:diskcache:status", &path_str).await.unwrap_or(false);
    if !exists {
        // Double check in format paths
        let current_paths_str: Option<String> = con.hget("squeezefs:format", "disk_cache_paths").await?;
        let has_path = current_paths_str.as_deref().unwrap_or("").split(',').any(|s| s == path_str);
        if !has_path {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Disk cache path '{}' not registered",
                path_str
            )));
        }
    }

    let _: () = con.hset("squeezefs:diskcache:status", &path_str, "disabled").await?;
    Ok(())
}

pub async fn enable_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con.hget("squeezefs:format", "disk_cache_paths").await?;
    let has_path = current_paths_str.as_deref().unwrap_or("").split(',').any(|s| s == path_str);
    if !has_path {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Disk cache path '{}' not registered",
            path_str
        )));
    }

    let _: () = con.hset("squeezefs:diskcache:status", &path_str, "enabled").await?;
    Ok(())
}

pub async fn flush_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    // Verification: must be disabled first
    let status: Option<String> = con.hget("squeezefs:diskcache:status", &path_str).await?;
    if status.as_deref() != Some("disabled") {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Cannot flush disk cache '{}' because it is not disabled",
            path_str
        )));
    }

    // Resolve backend S3 configuration
    let format_fields: HashMap<String, String> = con.hgetall("squeezefs:format").await.unwrap_or_default();
    let active_be_id = format_fields
        .get("active_write_backend")
        .cloned()
        .unwrap_or_else(|| "backend_0".to_string());

    let backend = if let Ok(json_str) = con.hget::<_, _, Option<String>>("squeezefs:backends", &active_be_id).await {
        if let Some(json_str) = json_str {
            if let Ok(config) = serde_json::from_str::<serde_json::Value>(&json_str) {
                let ep = config["endpoint"].as_str().map(|s| s.to_string());
                let ak = config["access_key"].as_str().map(|s| s.to_string());
                let sk = config["secret_key"].as_str().map(|s| s.to_string());
                let bu = config["bucket"].as_str().map(|s| s.to_string());
                RustFsClient::new_with_local_ips(Vec::new(), ep, ak, sk, bu).await
            } else {
                RustFsClient::new().await
            }
        } else {
            RustFsClient::new().await
        }
    } else {
        RustFsClient::new().await
    };

    let meta_client = crate::dlm::MetaClient::new(redis_url)?;
    recover_staging(path, &backend, &meta_client).await?;
    Ok(())
}

pub async fn remove_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path, force: bool) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con.hget("squeezefs:format", "disk_cache_paths").await?;
    let mut paths: Vec<String> = current_paths_str
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    if !paths.contains(&path_str) {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Disk cache path '{}' not registered",
            path_str
        )));
    }

    if !force {
        // Verification: must be disabled
        let status: Option<String> = con.hget("squeezefs:diskcache:status", &path_str).await?;
        if status.as_deref() != Some("disabled") {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Cannot remove disk cache '{}' because it is not disabled",
                path_str
            )));
        }

        // Verification: must be flushed (no staged files left)
        if path.exists() {
            let entries = std::fs::read_dir(path)?;
            for entry in entries {
                let entry = entry?;
                let file_path = entry.path();
                if file_path.is_file() {
                    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if ext == "data" || ext == "meta" {
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "Cannot remove disk cache '{}' because it contains unflushed staged files",
                            path_str
                        )));
                    }
                }
            }
        }
    }

    paths.retain(|p| p != &path_str);
    let new_paths_str = paths.join(",");

    let _: () = redis::pipe()
        .hset("squeezefs:format", "disk_cache_paths", new_paths_str)
        .hdel("squeezefs:diskcache:status", &path_str)
        .query_async(&mut con)
        .await?;

    Ok(())
}

pub async fn add_storage_backend(
    redis_url: &str,
    _fs_name: &str,
    backend_id: &str,
    endpoint: &str,
    access_key: &str,
    secret_key: &str,
    bucket: &str,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    let backend_json = serde_json::json!({
        "endpoint": endpoint,
        "access_key": access_key,
        "secret_key": secret_key,
        "bucket": bucket,
    }).to_string();

    let _: () = con.hset("squeezefs:backends", backend_id, backend_json).await?;
    Ok(())
}

pub async fn remove_storage_backend(redis_url: &str, _fs_name: &str, backend_id: &str, force: bool) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    if !force {
        // Verification: cannot delete active write backend
        let format_fields: HashMap<String, String> = con.hgetall("squeezefs:format").await.unwrap_or_default();
        let active_be_id = format_fields
            .get("active_write_backend")
            .cloned()
            .unwrap_or_else(|| "backend_0".to_string());

        if backend_id == active_be_id {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Cannot remove backend '{}' because it is currently the active write backend",
                backend_id
            )));
        }

        // Verification: scan Garnet metadata to verify backend is not referenced by block metadata
        let metadata_keys: Vec<String> = con.keys("metadata:*").await?;
        for meta_key in metadata_keys {
            let meta_type: Option<String> = con.hget(&meta_key, "type").await?;
            if meta_type.as_deref() == Some("striped") {
                let block_map_id: Option<String> = con.hget(&meta_key, "block_map").await?;
                if let Some(bmid) = block_map_id {
                    let map_key = format!("block_map:{}", bmid);
                    let block_keys: Vec<String> = con.hvals(&map_key).await.unwrap_or_default();
                    for bk in block_keys {
                        let (be_id, _) = parse_backend_and_key(&bk);
                        if be_id == backend_id {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "Cannot remove backend '{}' because it is referenced by block metadata for key '{}'",
                                backend_id, meta_key
                            )));
                        }
                    }
                }
            } else if meta_type.as_deref() == Some("staged") {
                let file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id {
                    let mapping_key = format!("mapping:{}", fid);
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    if let Some(bk) = block_key {
                        let (be_id, _) = parse_backend_and_key(&bk);
                        if be_id == backend_id {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "Cannot remove backend '{}' because it is referenced by staged mapping for file ID '{}'",
                                backend_id, fid
                            )));
                        }
                    }
                }
            }
        }
    }

    let _: () = con.hdel("squeezefs:backends", backend_id).await?;
    Ok(())
}

pub async fn set_active_backend(redis_url: &str, _fs_name: &str, backend_id: &str) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    // Check if backend exists
    let exists: bool = con.hexists("squeezefs:backends", backend_id).await.unwrap_or(false);
    if !exists && backend_id != "backend_0" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backend '{}' does not exist in registry",
            backend_id
        )));
    }

    let _: () = con.hset("squeezefs:format", "active_write_backend", backend_id).await?;
    Ok(())
}

pub async fn run_metadata_fsck(redis_url: &str, _fs_name: &str) -> Result<Vec<String>> {
    let mut con = connect_redis(redis_url).await?;
    let mut issues = Vec::new();

    // 1. Fetch all registered backends
    let backends: HashMap<String, String> = con.hgetall("squeezefs:backends").await.unwrap_or_default();
    
    // 2. Scan all metadata
    let metadata_keys: Vec<String> = con.keys("metadata:*").await?;
    for meta_key in metadata_keys {
        let file_path = meta_key.strip_prefix("metadata:").unwrap_or(&meta_key).to_string();
        let meta_type: Option<String> = con.hget(&meta_key, "type").await?;
        
        match meta_type.as_deref() {
            Some("striped") => {
                let block_map_id: Option<String> = con.hget(&meta_key, "block_map").await?;
                if let Some(bmid) = block_map_id {
                    let map_key = format!("block_map:{}", bmid);
                    let block_keys: HashMap<String, String> = con.hgetall(&map_key).await.unwrap_or_default();
                    if block_keys.is_empty() {
                        issues.push(format!("File '{}' block map '{}' is empty or missing", file_path, map_key));
                    }
                    for (b_idx, bk) in block_keys {
                        let (be_id, _) = parse_backend_and_key(&bk);
                        if be_id != "backend_0" && !backends.contains_key(&be_id) {
                            issues.push(format!(
                                "File '{}' block '{}' references unregistered backend '{}' (key: {})",
                                file_path, b_idx, be_id, bk
                            ));
                        }
                    }
                } else {
                    issues.push(format!("File '{}' has type 'striped' but missing 'block_map' reference", file_path));
                }
            }
            Some("staged") => {
                let file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
                if let Some(fid) = file_id {
                    let mapping_key = format!("mapping:{}", fid);
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    if let Some(bk) = block_key {
                        let (be_id, _) = parse_backend_and_key(&bk);
                        if be_id != "backend_0" && !backends.contains_key(&be_id) {
                            issues.push(format!(
                                "File '{}' staged block references unregistered backend '{}' (key: {})",
                                file_path, be_id, bk
                            ));
                        }
                    } else {
                        issues.push(format!("File '{}' staged mapping '{}' is missing or has no 'block' field", file_path, mapping_key));
                    }
                } else {
                    issues.push(format!("File '{}' has type 'staged' but missing 'file_id' field", file_path));
                }
            }
            _ => {}
        }
    }

    Ok(issues)
}
