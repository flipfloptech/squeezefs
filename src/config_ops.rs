use crate::block_allocator::BlockAllocator;
use crate::error::{Result, SqueezefsError};
use crate::nvme_dev::NvmeBlockDev;
use crate::recovery::recover_staging;
use redis::AsyncCommands;
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

/// Helper to connect to redis
async fn connect_redis(redis_url: &str) -> Result<redis::aio::MultiplexedConnection> {
    let client = redis::Client::open(redis_url).map_err(SqueezefsError::Redis)?;
    let con = client
        .get_multiplexed_tokio_connection()
        .await
        .map_err(SqueezefsError::Redis)?;
    Ok(con)
}

pub async fn list_config(redis_url: &str, _fs_name: &str) -> Result<ConfigList> {
    let mut con = connect_redis(redis_url).await?;

    let format_fields: HashMap<String, String> = con
        .hgetall(crate::fs_key!("format"))
        .await
        .unwrap_or_default();

    let active_write_backend = format_fields
        .get("active_write_backend")
        .cloned()
        .unwrap_or_else(|| "backend_0".to_string());

    let disk_cache_paths_str = format_fields
        .get("disk_cache_paths")
        .cloned()
        .unwrap_or_default();
    let paths: Vec<PathBuf> = if disk_cache_paths_str.is_empty() {
        Vec::new()
    } else {
        disk_cache_paths_str.split(',').map(PathBuf::from).collect()
    };

    let status_map: HashMap<String, String> = con
        .hgetall(crate::fs_key!("diskcache:status"))
        .await
        .unwrap_or_default();

    let mut diskcaches = Vec::new();
    for p in paths {
        let p_str = p.to_string_lossy().to_string();
        let status = status_map
            .get(&p_str)
            .cloned()
            .unwrap_or_else(|| "enabled".to_string());
        diskcaches.push(DiskCacheInfo { path: p, status });
    }

    let backends: HashMap<String, String> = con
        .hgetall(crate::fs_key!("backends"))
        .await
        .unwrap_or_default();

    let mut backend_statuses: HashMap<String, String> = con
        .hgetall(crate::fs_key!("backend:status"))
        .await
        .unwrap_or_default();

    for be_id in backends.keys() {
        backend_statuses
            .entry(be_id.clone())
            .or_insert_with(|| "enabled".to_string());
    }

    Ok(ConfigList {
        diskcaches,
        backends,
        backend_statuses,
        active_write_backend,
    })
}

pub async fn add_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con
        .hget(crate::fs_key!("format"), "disk_cache_paths")
        .await?;
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
        .hset(crate::fs_key!("format"), "disk_cache_paths", new_paths_str)
        .hset(crate::fs_key!("diskcache:status"), &path_str, "enabled")
        .query_async(&mut con)
        .await?;

    Ok(())
}

pub async fn disable_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let exists: bool = con
        .hexists(crate::fs_key!("diskcache:status"), &path_str)
        .await
        .unwrap_or(false);
    if !exists {
        // Double check in format paths
        let current_paths_str: Option<String> = con
            .hget(crate::fs_key!("format"), "disk_cache_paths")
            .await?;
        let has_path = current_paths_str
            .as_deref()
            .unwrap_or("")
            .split(',')
            .any(|s| s == path_str);
        if !has_path {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Disk cache path '{}' not registered",
                path_str
            )));
        }
    }

    let _: () = con
        .hset(crate::fs_key!("diskcache:status"), &path_str, "disabled")
        .await?;
    Ok(())
}

pub async fn enable_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con
        .hget(crate::fs_key!("format"), "disk_cache_paths")
        .await?;
    let has_path = current_paths_str
        .as_deref()
        .unwrap_or("")
        .split(',')
        .any(|s| s == path_str);
    if !has_path {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Disk cache path '{}' not registered",
            path_str
        )));
    }

    let _: () = con
        .hset(crate::fs_key!("diskcache:status"), &path_str, "enabled")
        .await?;
    Ok(())
}

pub async fn flush_disk_cache_path(redis_url: &str, _fs_name: &str, path: &Path) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    // Verification: must be disabled first
    let status: Option<String> = con
        .hget(crate::fs_key!("diskcache:status"), &path_str)
        .await?;
    if status.as_deref() != Some("disabled") {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Cannot flush disk cache '{}' because it is not disabled",
            path_str
        )));
    }

    let meta_client = std::sync::Arc::new(crate::dlm::MetaClient::new(redis_url)?);
    let block_alloc =
        std::sync::Arc::new(BlockAllocator::new(meta_client.clone(), "default").await?);
    let nvme_dev = std::sync::Arc::new(NvmeBlockDev::new(path.to_str().unwrap()));
    let dlm = crate::dlm::DlmClient::new(redis_url)?;
    recover_staging(path, &meta_client, &block_alloc, &nvme_dev, Some(&dlm)).await?;
    Ok(())
}

pub async fn remove_disk_cache_path(
    redis_url: &str,
    _fs_name: &str,
    path: &Path,
    force: bool,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;
    let path_str = path.to_string_lossy().to_string();

    let current_paths_str: Option<String> = con
        .hget(crate::fs_key!("format"), "disk_cache_paths")
        .await?;
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
        let status: Option<String> = con
            .hget(crate::fs_key!("diskcache:status"), &path_str)
            .await?;
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
        .hset(crate::fs_key!("format"), "disk_cache_paths", new_paths_str)
        .hdel(crate::fs_key!("diskcache:status"), &path_str)
        .query_async(&mut con)
        .await?;

    Ok(())
}

pub async fn add_storage_backend(
    redis_url: &str,
    _fs_name: &str,
    backend_id: &str,
    backing_dev: Option<&str>,
    ip: Option<&str>,
    port: Option<u16>,
    subnqn: Option<&str>,
    capacity: Option<u64>,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    let name_exists: bool = con.hexists(crate::fs_key!("backends"), backend_id).await?;
    if name_exists {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backend name '{}' already exists in registry",
            backend_id
        )));
    }

    let mut resolved_ip = ip.map(|s| s.to_string());
    let mut resolved_port = port;
    let mut resolved_subnqn = subnqn.map(|s| s.to_string());

    // Resolve backing_dev using connection parameters if not explicitly provided
    let resolved_backing_dev = match backing_dev {
        Some(dev) => {
            let dev_str = dev.to_string();
            // Automatically pull NVMe-oF details if possible
            if let Some((ext_ip, ext_port, ext_subnqn)) =
                crate::nvmeof::extract_nvmeof_connection_details(&dev_str)
            {
                log::info!(
                    "Automatically extracted NVMe-oF connection details for {}: {}:{} / {}",
                    dev_str,
                    ext_ip,
                    ext_port,
                    ext_subnqn
                );
                if resolved_ip.is_none() {
                    resolved_ip = Some(ext_ip);
                }
                if resolved_port.is_none() {
                    resolved_port = Some(ext_port);
                }
                if resolved_subnqn.is_none() {
                    resolved_subnqn = Some(ext_subnqn);
                }
            }
            dev_str
        }
        None => {
            if let (Some(ip_val), Some(port_val), Some(nqn_val)) = (ip, port, subnqn) {
                log::info!(
                    "Connecting to NVMe-oF target at {}:{} / {}...",
                    ip_val,
                    port_val,
                    nqn_val
                );
                let dev_path =
                    crate::nvmeof::connect_target(ip_val, port_val, nqn_val).map_err(|e| {
                        SqueezefsError::InvalidOperation(format!(
                            "Failed to connect to NVMe-oF target: {:?}",
                            e
                        ))
                    })?;
                log::info!("Connected to remote NVMe-oF disk: {}", dev_path);
                dev_path
            } else {
                return Err(SqueezefsError::InvalidOperation(
                    "Either backing device path or NVMe-oF parameters (ip, port, subnqn) must be specified".to_string()
                ));
            }
        }
    };

    crate::storage::validate_backing_device(&resolved_backing_dev)?;

    let backends_map: std::collections::HashMap<String, String> = con
        .hgetall(crate::fs_key!("backends"))
        .await
        .unwrap_or_default();
    for (be_id, be_json) in backends_map {
        if let Ok(config) = serde_json::from_str::<serde_json::Value>(&be_json) {
            let bd = config["backing_dev"].as_str().unwrap_or("");
            if bd == resolved_backing_dev {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Duplicate backend configuration: backing device '{}' is already registered under name '{}'",
                    resolved_backing_dev, be_id
                )));
            }
        }
    }

    // Read filesystem metadata to create a correct superblock
    let block_size: u64 = con
        .hget(crate::fs_key!("format"), "block_size")
        .await
        .unwrap_or(4 * 1024 * 1024);
    let inodes: u64 = con
        .hget(crate::fs_key!("format"), "inodes")
        .await
        .unwrap_or(1_000_000);

    // Resolve capacity. If not specified, default to format key capacity
    let resolved_capacity: u64 = match capacity {
        Some(cap) => cap,
        None => con
            .hget(crate::fs_key!("format"), "capacity")
            .await
            .unwrap_or(1024 * 1024 * 1024 * 1024),
    };

    // Auto-initialize the backing device by writing the 4KB SqueezeFS superblock
    log::info!(
        "Writing SqueezeFS superblock signature to target backend device: {}",
        resolved_backing_dev
    );
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&resolved_backing_dev)
        .await
        .map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "Failed to open backing device '{}': {:?}",
                resolved_backing_dev, e
            ))
        })?;

    // Construct superblock
    let mut sb = vec![0u8; 4096];
    let magic = b"SQUEEZEFS_SUPER\x00";
    sb[0..magic.len()].copy_from_slice(magic);
    let name_bytes = _fs_name.as_bytes();
    let name_len = std::cmp::min(name_bytes.len(), 63);
    sb[16..16 + name_len].copy_from_slice(&name_bytes[..name_len]);
    sb[80..88].copy_from_slice(&resolved_capacity.to_be_bytes());
    sb[88..96].copy_from_slice(&block_size.to_be_bytes());
    sb[96..104].copy_from_slice(&inodes.to_be_bytes());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    sb[104..112].copy_from_slice(&timestamp.to_be_bytes());

    use tokio::io::AsyncSeekExt;
    use tokio::io::AsyncWriteExt;
    file.seek(std::io::SeekFrom::Start(0))
        .await
        .map_err(SqueezefsError::Io)?;
    file.write_all(&sb).await.map_err(SqueezefsError::Io)?;
    file.sync_all().await.map_err(SqueezefsError::Io)?;

    let (lvm_vg, lvm_loops) = crate::storage::extract_lvm_loop_info(&resolved_backing_dev);

    let backend_json = serde_json::json!({
        "backing_dev": resolved_backing_dev,
        "capacity": resolved_capacity,
        "ip": resolved_ip,
        "port": resolved_port,
        "subnqn": resolved_subnqn,
        "lvm_vg": lvm_vg,
        "lvm_loops": lvm_loops,
    })
    .to_string();

    let _: () = redis::pipe()
        .hset(crate::fs_key!("backends"), backend_id, backend_json)
        .hset(crate::fs_key!("backend:status"), backend_id, "enabled")
        .query_async(&mut con)
        .await?;
    Ok(())
}

pub async fn remove_storage_backend(
    redis_url: &str,
    _fs_name: &str,
    backend_id: &str,
    force: bool,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    if !force {
        // Verification: cannot delete active write backend
        let format_fields: HashMap<String, String> = con
            .hgetall(crate::fs_key!("format"))
            .await
            .unwrap_or_default();
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
        let mut metadata_keys = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (next_cursor, chunk): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("metadata:*")
                .arg("COUNT")
                .arg(100)
                .query_async(&mut con)
                .await?;
            metadata_keys.extend(chunk);
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }

        for meta_key in metadata_keys {
            let meta_type: Option<String> = con.hget(&meta_key, "type").await?;
            if meta_type.as_deref() == Some("striped") {
                let block_map_id: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                if let Some(bmid) = block_map_id {
                    let map_key = crate::keys::block_map(&bmid).to_string();
                    let block_keys: Vec<String> = con.hvals(&map_key).await.unwrap_or_default();
                    for bk in block_keys {
                        let parts: Vec<&str> = bk.split("://").collect();
                        let be_id = if parts.len() > 1 {
                            parts[0].to_string()
                        } else {
                            "backend_0".to_string()
                        };
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
                    let mapping_key = crate::keys::mapping(&fid).to_string();
                    let block_key: Option<String> = con.hget(&mapping_key, "block").await?;
                    if let Some(bk) = block_key {
                        let parts: Vec<&str> = bk.split("://").collect();
                        let be_id = if parts.len() > 1 {
                            parts[0].to_string()
                        } else {
                            "backend_0".to_string()
                        };
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

    let _: () = con.hdel(crate::fs_key!("backends"), backend_id).await?;
    Ok(())
}

pub async fn set_active_backend(redis_url: &str, _fs_name: &str, backend_id: &str) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    // Check if backend exists
    let exists: bool = con
        .hexists(crate::fs_key!("backends"), backend_id)
        .await
        .unwrap_or(false);
    if !exists && backend_id != "backend_0" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backend '{}' does not exist in registry",
            backend_id
        )));
    }

    let _: () = con
        .hset(crate::fs_key!("format"), "active_write_backend", backend_id)
        .await?;
    Ok(())
}

pub async fn run_metadata_fsck(redis_url: &str, _fs_name: &str) -> Result<Vec<String>> {
    let mut con = connect_redis(redis_url).await?;
    let mut issues = Vec::new();

    // Helper to parse block index from block key (e.g. "backend_0://4194304")
    let parse_block_index = |bk: &str| -> Option<(String, u64)> {
        let parts: Vec<&str> = bk.split("://").collect();
        if parts.len() == 2 {
            let backend = parts[0].to_string();
            if let Ok(offset) = parts[1].parse::<u64>() {
                let chunk_size = 4 * 1024 * 1024;
                return Some((backend, offset / chunk_size));
            }
        }
        None
    };

    // 1. Fetch block allocator status (highest_block, free_blocks, refcounts)
    let highest_block: u64 = con.get(crate::fs_key!("highest_block")).await.unwrap_or(0);
    let free_blocks_vec: Vec<u64> = con
        .smembers(crate::fs_key!("free_blocks"))
        .await
        .unwrap_or_default();
    let free_blocks_set: std::collections::HashSet<u64> = free_blocks_vec.into_iter().collect();
    let db_refcounts: HashMap<String, String> = con
        .hgetall(crate::fs_key!("block_refcounts"))
        .await
        .unwrap_or_default();

    // 2. Fetch all registered backends
    let backends: HashMap<String, String> = con
        .hgetall(crate::fs_key!("backends"))
        .await
        .unwrap_or_default();

    // 3. Scan all metadata keys
    let mut metadata_keys = Vec::new();
    let mut cursor = 0u64;
    loop {
        let (next_cursor, chunk): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("metadata:*")
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut con)
            .await?;
        metadata_keys.extend(chunk);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    // 4. Batch pipeline fetching of metadata attributes
    let mut metadata_attrs = Vec::new();
    let batch_size = 500;
    for chunk in metadata_keys.chunks(batch_size) {
        let mut pipe = redis::pipe();
        for key in chunk {
            pipe.hgetall(key);
        }
        let res: Vec<HashMap<String, String>> = pipe.query_async(&mut con).await?;
        for (key, fields) in chunk.iter().zip(res.into_iter()) {
            metadata_attrs.push((key.clone(), fields));
        }
    }

    let mut striped_files = Vec::new(); // Vec<(file_path, block_map_key)>
    let mut staged_files = Vec::new(); // Vec<(file_path, mapping_key)>

    for (meta_key, fields) in &metadata_attrs {
        let file_path = meta_key
            .strip_prefix("metadata:")
            .unwrap_or(meta_key)
            .to_string();
        let meta_type = fields.get("type").map(|s| s.as_str());
        match meta_type {
            Some("striped") => {
                if let Some(bmid) = fields.get("block_map_id") {
                    striped_files.push((file_path, crate::keys::block_map(&bmid).to_string()));
                } else {
                    issues.push(format!(
                        "File '{}' has type 'striped' but missing 'block_map' reference",
                        file_path
                    ));
                }
            }
            Some("staged") => {
                if let Some(fid) = fields.get("file_id") {
                    staged_files.push((file_path, crate::keys::mapping(&fid).to_string()));
                } else {
                    issues.push(format!(
                        "File '{}' has type 'staged' but missing 'file_id' field",
                        file_path
                    ));
                }
            }
            _ => {}
        }
    }

    // 5. Batch pipeline fetching of block_map tables
    let mut block_maps = HashMap::new(); // block_map_key -> HashMap<String, String>
    for chunk in striped_files.chunks(batch_size) {
        let mut pipe = redis::pipe();
        for (_, map_key) in chunk {
            pipe.hgetall(map_key);
        }
        let res: Vec<HashMap<String, String>> = pipe.query_async(&mut con).await?;
        for ((_, map_key), map_data) in chunk.iter().zip(res.into_iter()) {
            block_maps.insert(map_key.clone(), map_data);
        }
    }

    // 6. Batch pipeline fetching of staged mappings
    let mut staged_mappings = HashMap::new(); // mapping_key -> Option<String>
    for chunk in staged_files.chunks(batch_size) {
        let mut pipe = redis::pipe();
        for (_, mapping_key) in chunk {
            pipe.hget(mapping_key, "block");
        }
        let res: Vec<Option<String>> = pipe.query_async(&mut con).await?;
        for ((_, mapping_key), block_opt) in chunk.iter().zip(res.into_iter()) {
            staged_mappings.insert(mapping_key.clone(), block_opt);
        }
    }

    let mut actual_refcounts: HashMap<String, usize> = HashMap::new();

    // 7. Audit block references
    for (file_path, map_key) in &striped_files {
        if let Some(block_keys) = block_maps.get(map_key) {
            if block_keys.is_empty() {
                issues.push(format!(
                    "File '{}' block map '{}' is empty or missing",
                    file_path, map_key
                ));
            }
            for (b_idx, bk) in block_keys {
                if let Some((be_id, _)) = parse_block_index(bk) {
                    if be_id != "backend_0" && !backends.contains_key(&be_id) {
                        issues.push(format!(
                            "File '{}' block '{}' references unregistered backend '{}' (key: {})",
                            file_path, b_idx, be_id, bk
                        ));
                    }
                    if be_id == "backend_0" {
                        *actual_refcounts.entry(bk.clone()).or_insert(0) += 1;
                    }
                } else {
                    issues.push(format!(
                        "File '{}' block '{}' has invalid block key format: {}",
                        file_path, b_idx, bk
                    ));
                }
            }
        }
    }

    for (file_path, mapping_key) in &staged_files {
        if let Some(block_opt) = staged_mappings.get(mapping_key) {
            if let Some(bk) = block_opt {
                if let Some((be_id, _)) = parse_block_index(bk) {
                    if be_id != "backend_0" && !backends.contains_key(&be_id) {
                        issues.push(format!(
                            "File '{}' staged block references unregistered backend '{}' (key: {})",
                            file_path, be_id, bk
                        ));
                    }
                    if be_id == "backend_0" {
                        *actual_refcounts.entry(bk.clone()).or_insert(0) += 1;
                    }
                } else {
                    issues.push(format!(
                        "File '{}' staged block has invalid block key format: {}",
                        file_path, bk
                    ));
                }
            } else {
                issues.push(format!(
                    "File '{}' staged mapping '{}' is missing or has no 'block' field",
                    file_path, mapping_key
                ));
            }
        }
    }

    // 8. Cross-reference block counts against free_blocks, highest_block, and refcounts
    for (bk, &actual_ref) in &actual_refcounts {
        if let Some((be_id, block_idx)) = parse_block_index(bk) {
            if be_id == "backend_0" {
                if block_idx > highest_block {
                    issues.push(format!(
                        "Block index {} (key: {}) is referenced by files but exceeds highest_block ({})",
                        block_idx, bk, highest_block
                    ));
                }
                if free_blocks_set.contains(&block_idx) {
                    issues.push(format!(
                        "CRITICAL: Block index {} (key: {}) is referenced by files but is marked as FREE in database",
                        block_idx, bk
                    ));
                }
                let db_ref = db_refcounts
                    .get(bk)
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or(0);
                if db_ref != actual_ref as i32 {
                    issues.push(format!(
                        "Reference count mismatch for block key '{}': DB has {}, actual is {}",
                        bk, db_ref, actual_ref
                    ));
                }
            }
        }
    }

    // Detect leaked blocks
    for idx in 1..=highest_block {
        if !free_blocks_set.contains(&idx) {
            let offset = idx * 4 * 1024 * 1024;
            let bk = format!("backend_0://{}", offset);
            if !actual_refcounts.contains_key(&bk) {
                issues.push(format!(
                    "Leaked block detected: Block index {} (key: {}) is not in free set and is not referenced by any file",
                    idx, bk
                ));
            }
        }
    }

    Ok(issues)
}

pub async fn enable_storage_backend(
    redis_url: &str,
    _fs_name: &str,
    backend_id: &str,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    let exists: bool = con.hexists(crate::fs_key!("backends"), backend_id).await?;
    if !exists && backend_id != "backend_0" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backend '{}' does not exist in registry",
            backend_id
        )));
    }

    let _: () = con
        .hset(crate::fs_key!("backend:status"), backend_id, "enabled")
        .await?;
    Ok(())
}

pub async fn disable_storage_backend(
    redis_url: &str,
    _fs_name: &str,
    backend_id: &str,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    let exists: bool = con.hexists(crate::fs_key!("backends"), backend_id).await?;
    if !exists && backend_id != "backend_0" {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Backend '{}' does not exist in registry",
            backend_id
        )));
    }

    let backends_map: std::collections::HashMap<String, String> = con
        .hgetall(crate::fs_key!("backends"))
        .await
        .unwrap_or_default();
    let statuses: std::collections::HashMap<String, String> = con
        .hgetall(crate::fs_key!("backend:status"))
        .await
        .unwrap_or_default();

    let mut enabled_count = 0;

    for be_id in backends_map.keys() {
        let status = statuses.get(be_id).map(|s| s.as_str()).unwrap_or("enabled");
        if status == "enabled" {
            enabled_count += 1;
        }
    }

    let status_b0 = statuses
        .get("backend_0")
        .map(|s| s.as_str())
        .unwrap_or("enabled");
    if status_b0 == "enabled" && !backends_map.contains_key("backend_0") {
        enabled_count += 1;
    }

    let target_status = statuses
        .get(backend_id)
        .map(|s| s.as_str())
        .unwrap_or("enabled");
    if target_status == "enabled" && enabled_count <= 1 {
        return Err(SqueezefsError::InvalidOperation(
            "Cannot disable backend: at least one storage backend must remain enabled for writes"
                .to_string(),
        ));
    }

    let _: () = con
        .hset(crate::fs_key!("backend:status"), backend_id, "disabled")
        .await?;
    Ok(())
}

pub async fn set_config_quota(
    redis_url: &str,
    _fs_name: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    let mut con = connect_redis(redis_url).await?;

    match key.to_lowercase().as_str() {
        "capacity" => {
            let bytes = crate::cache::parse_size_string(value, 0)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "capacity", bytes)
                .await?;
            println!("Configuration quota 'capacity' set to {} bytes.", bytes);
        }
        "inodes" => {
            let limit: u64 = value.parse().map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "Invalid inodes value '{}': {:?}",
                    value, e
                ))
            })?;
            let _: () = con.hset(crate::fs_key!("format"), "inodes", limit).await?;
            println!("Configuration quota 'inodes' set to {}.", limit);
        }
        "mem_cache_size" | "mem-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "mem_cache_size", value)
                .await?;
            println!("Configuration quota 'mem_cache_size' set to '{}'.", value);
        }
        "read_mem_cache_size" | "read-mem-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "read_mem_cache_size", value)
                .await?;
            println!(
                "Configuration quota 'read_mem_cache_size' set to '{}'.",
                value
            );
        }
        "write_mem_cache_size" | "write-mem-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "write_mem_cache_size", value)
                .await?;
            println!(
                "Configuration quota 'write_mem_cache_size' set to '{}'.",
                value
            );
        }
        "disk_cache_size" | "disk-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "disk_cache_size", value)
                .await?;
            println!("Configuration quota 'disk_cache_size' set to '{}'.", value);
        }
        "read_cache_size" | "read-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "read_cache_size", value)
                .await?;
            println!("Configuration quota 'read_cache_size' set to '{}'.", value);
        }
        "write_cache_size" | "write-cache-size" => {
            let _ = crate::cache::parse_size_string(value, 1024 * 1024 * 1024)?;
            let _: () = con
                .hset(crate::fs_key!("format"), "write_cache_size", value)
                .await?;
            println!("Configuration quota 'write_cache_size' set to '{}'.", value);
        }
        "fuse_io_uring_sqpoll_idle_ms" | "fuse-io-uring-sqpoll-idle-ms" => {
            let parsed: u32 = value.parse().map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "Invalid fuse_io_uring_sqpoll_idle_ms value '{}': {:?}",
                    value, e
                ))
            })?;
            let stored_value = if parsed == 0 { "" } else { value };
            let _: () = con
                .hset(
                    crate::fs_key!("format"),
                    "fuse_io_uring_sqpoll_idle_ms",
                    stored_value,
                )
                .await?;
            if parsed == 0 {
                println!(
                    "Configuration 'fuse_io_uring_sqpoll_idle_ms' cleared; mounts will fall back to local CLI/env overrides or disabled SQPOLL."
                );
            } else {
                println!(
                    "Configuration quota 'fuse_io_uring_sqpoll_idle_ms' set to '{}'.",
                    value
                );
            }
        }
        _ => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Invalid config quota key '{}'. Supported keys: 'capacity', 'inodes', 'mem_cache_size', 'read_mem_cache_size', 'write_mem_cache_size', 'disk_cache_size', 'read_cache_size', 'write_cache_size', 'fuse_io_uring_sqpoll_idle_ms'",
                key
            )));
        }
    }
    Ok(())
}
