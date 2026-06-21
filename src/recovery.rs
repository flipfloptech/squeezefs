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
    redis_client: &redis::Client,
) -> Result<usize> {
    if !staging_dir.exists() {
        return Ok(0);
    }

    info!(
        "Crash Recovery: Scanning local NVMe staging directory '{:?}' for pending writes.",
        staging_dir
    );
    let mut recovered_count = 0;
    let mut con = redis_client.get_multiplexed_tokio_connection().await?;

    let entries = fs::read_dir(staging_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Only process metadata files (.meta)
        if path.is_file() && path.extension().is_some_and(|ext| ext == "meta") {
            let file_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            if file_id.is_empty() {
                continue;
            }

            let meta_path = &path;
            let data_path = staging_dir.join(format!("{}.data", file_id));

            debug!(
                "Crash Recovery: Found staged write transaction ID: {}",
                file_id
            );

            // 1. Read metadata
            let meta_bytes = match fs::read(meta_path) {
                Ok(b) => b,
                Err(e) => {
                    error!(
                        "Crash Recovery: Failed to read metadata file {:?}: {:?}",
                        meta_path, e
                    );
                    continue;
                }
            };

            let meta: StagedMetadata = match serde_json::from_slice(&meta_bytes) {
                Ok(m) => m,
                Err(e) => {
                    error!(
                        "Crash Recovery: Failed to parse metadata file {:?}: {:?}",
                        meta_path, e
                    );
                    // Corrupted metadata, delete transaction files
                    let _ = fs::remove_file(meta_path);
                    let _ = fs::remove_file(&data_path);
                    continue;
                }
            };

            // 2. Cross-reference with Garnet
            let meta_key = format!("metadata:{}", meta.file_path);
            let redis_file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
            let redis_file_type: Option<String> = con.hget(&meta_key, "type").await?;

            let should_recover = redis_file_type.as_deref() == Some("staged")
                && redis_file_id.as_deref() == Some(&file_id);

            if should_recover {
                if data_path.exists() {
                    let data = match fs::read(&data_path) {
                        Ok(d) => d,
                        Err(e) => {
                            error!(
                                "Crash Recovery: Failed to read data block {:?}: {:?}",
                                data_path, e
                            );
                            continue;
                        }
                    };

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
                    warn!("Crash Recovery: Metadata exists for {} but data file {:?} is missing. Discarding transaction.", file_id, data_path);
                }
            } else {
                warn!(
                    "Crash Recovery: Stale write detected for '{}' (ID: {}). Redis has type={:?} and file_id={:?}. Discarding stale local files.",
                    meta.file_path, file_id, redis_file_type, redis_file_id
                );
            }

            // 5. Clean up local staging files
            let _ = fs::remove_file(meta_path);
            let _ = fs::remove_file(&data_path);
        }
    }

    info!(
        "Crash Recovery: Staging directory scan complete. Recovered {} files.",
        recovered_count
    );
    Ok(recovered_count)
}
