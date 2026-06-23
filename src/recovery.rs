use crate::backend::RustFsClient;
use crate::error::Result;
use log::{debug, error, info, warn};
use redis::AsyncCommands;
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Deserialize, Debug)]
struct StagedMetadata {
    file_path: String,
    fencing_token: u64,
}

/// Scan NVMe staging directory, cross-reference pending staged files with Garnet metadata,
/// and recover/finalize uploads to RustFS S3.
/// Returns the number of successfully recovered files.
pub async fn recover_staging(
    staging_dir: &Path,
    backend: &RustFsClient,
    redis_client: &crate::dlm::MetaClient,
) -> Result<usize> {
    if !staging_dir.exists() {
        return Ok(0);
    }

    // Clean up any remaining active_writes directory from previous crashed mounts
    let active_writes_dir = staging_dir.join("active_writes");
    if active_writes_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&active_writes_dir) {
            warn!(
                "Crash Recovery: Failed to remove stale active_writes directory: {:?}",
                e
            );
        } else {
            info!("Crash Recovery: Cleaned up stale active_writes directory.");
        }
    }

    info!(
        "Crash Recovery: Scanning local NVMe staging directory '{:?}' for pending writes.",
        staging_dir
    );
    let mut recovered_count = 0;
    let mut con = redis_client.get_connection().await?;

    let entries = fs::read_dir(staging_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Only process staged files (.staged)
        if path.is_file() && path.extension().is_some_and(|ext| ext == "staged") {
            let file_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            if file_id.is_empty() {
                continue;
            }

            let staged_path = &path;

            debug!(
                "Crash Recovery: Found staged write transaction ID: {}",
                file_id
            );

            // 1. Read staged file bytes
            let bytes = match fs::read(staged_path) {
                Ok(b) => b,
                Err(e) => {
                    error!(
                        "Crash Recovery: Failed to read staged file {:?}: {:?}",
                        staged_path, e
                    );
                    continue;
                }
            };

            if bytes.len() < 8 {
                error!(
                    "Crash Recovery: Staged file {:?} is truncated (size < 8 bytes)",
                    staged_path
                );
                let _ = fs::remove_file(staged_path);
                continue;
            }

            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() < 8 + meta_len {
                error!(
                    "Crash Recovery: Staged file {:?} is truncated (size < 8 + meta_len)",
                    staged_path
                );
                let _ = fs::remove_file(staged_path);
                continue;
            }

            let meta: StagedMetadata = match serde_json::from_slice(&bytes[8..8 + meta_len]) {
                Ok(m) => m,
                Err(e) => {
                    error!(
                        "Crash Recovery: Failed to parse metadata from staged file {:?}: {:?}",
                        staged_path, e
                    );
                    let _ = fs::remove_file(staged_path);
                    continue;
                }
            };

            // Read the JSON to get original_size
            let original_size =
                match serde_json::from_slice::<serde_json::Value>(&bytes[8..8 + meta_len]) {
                    Ok(json) => json
                        .get("original_size")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize,
                    Err(_) => 0,
                };

            let data_start = 8 + meta_len;
            let data_end = data_start + original_size;
            if bytes.len() < data_end {
                error!(
                    "Crash Recovery: Staged file {:?} data payload is truncated",
                    staged_path
                );
                let _ = fs::remove_file(staged_path);
                continue;
            }
            let data = bytes[data_start..data_end].to_vec();

            // 2. Cross-reference with Garnet
            let meta_key = format!("metadata:{}", meta.file_path);
            let redis_file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
            let redis_file_type: Option<String> = con.hget(&meta_key, "type").await?;

            let should_recover = redis_file_type.as_deref() == Some("staged")
                && redis_file_id.as_deref() == Some(&file_id);

            if should_recover {
                info!(
                    "Crash Recovery: Recovering write for '{}' (ID: {}, size: {} bytes, fencing token: {})",
                    meta.file_path, file_id, data.len(), meta.fencing_token
                );

                // 3. Upload to RustFS S3
                let recovered_key = format!("recovered/blocks/{}", file_id);
                if let Err(e) = backend
                    .put_object(&recovered_key, data.clone(), meta.fencing_token)
                    .await
                {
                    error!(
                        "Crash Recovery: Failed to upload recovered block to RustFS: {:?}",
                        e
                    );
                    continue;
                }

                // 4. Update Garnet mapping
                let mapping_key = format!("mapping:{}", file_id);
                let _: () = redis::pipe()
                    .hset(&mapping_key, "block", &recovered_key)
                    .hset(&mapping_key, "offset", 0u64)
                    .hset(&mapping_key, "size", data.len() as u64)
                    .query_async(&mut con)
                    .await?;

                recovered_count += 1;
            } else {
                warn!(
                    "Crash Recovery: Stale write detected for '{}' (ID: {}). Redis has type={:?} and file_id={:?}. Discarding stale local files.",
                    meta.file_path, file_id, redis_file_type, redis_file_id
                );
            }

            // 5. Clean up local staging file
            let _ = fs::remove_file(staged_path);
        }
    }

    info!(
        "Crash Recovery: Staging directory scan complete. Recovered {} files.",
        recovered_count
    );
    Ok(recovered_count)
}
