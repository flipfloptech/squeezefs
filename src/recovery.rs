//! Crash recovery for local NVMe staging after a client kill / reboot.
//!
//! # Consistency model (P0-4)
//!
//! - **DLM leases** (`lock:inode_*` in Garnet) use TTL + heartbeat. After a hard kill,
//!   the lease expires; another client may acquire a **new fencing token**.
//! - **File metadata** stores the last committed `fencing_token`. Writers must present a
//!   token ≥ that value (see `DataRouter::write_file`).
//! - **Local staging** (active blocks / staged files) may outlive the process. On remount,
//!   `recover_staging` only commits staging entries whose fencing token is not stale
//!   relative to Garnet, and whose layout still points at that staged id.
//! - **Live clients** must not trust a locally cached lease after Redis no longer holds
//!   their client id on the lock key (`get_or_acquire_lease` re-validates).
//!
//! Staged payload headers use the binary format in `cache::nvme::StagedMetadata`
//! (not JSON).
//!
//! # Recovery completeness matrix (P2-13)
//!
//! | Scenario | Expected |
//! |----------|----------|
//! | Staged id matches meta `file_id` + type staged + fence OK | commit to backend + mapping |
//! | Staged fence &lt; Garnet fence | discard |
//! | Staged present, meta `file_id` mismatch / not staged | discard |
//! | Corrupt staged blob | discard (no panic) |
//! | `active_block:` with missing inode meta | discard |
//! | `active_block:` fence stale | discard |
//! | Partial flush (write_block fails) | leave entry / continue (best-effort log) |
//!
//! Covered by `tests/crash_consistency_tests.rs` and `tests/recovery_tests.rs`.

use crate::cache::nvme::parse_staged_blob;
use crate::error::Result;
use log::{debug, error, info, warn};
use redis::AsyncCommands;
use std::fs;
use std::path::Path;

/// Scan NVMe staging segment index, cross-reference pending staged files with Garnet metadata,
/// and recover/finalize writes to backing block device.
/// Returns the number of successfully recovered files.
pub async fn recover_staging(
    staging_dir: &Path,
    redis_client: &crate::dlm::MetaClient,
    block_allocator: &std::sync::Arc<crate::block_allocator::BlockAllocator>,
    nvme_writer: &std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
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

    let staging_segment_dir = staging_dir.join("staging_segment");
    if !staging_segment_dir.exists() {
        return Ok(0);
    }

    info!(
        "Crash Recovery: Scanning NVMe staging segment directory '{:?}' for pending writes.",
        staging_segment_dir
    );

    let mut con = redis_client.get_connection().await?;

    // Determine staging capacity limit
    let size_str: Option<String> = con
        .hget(crate::fs_key!("format"), "write_disk_limit")
        .await
        .unwrap_or(None);
    let max_write_bytes = if let Some(ref s) = size_str {
        crate::cache::parse_size_string(s, 100 * 1024 * 1024 * 1024).unwrap_or(100 * 1024 * 1024)
    } else {
        100 * 1024 * 1024
    };

    // Load encryption and compression settings
    let compression: String = con
        .hget(crate::fs_key!("format"), "compression")
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "none".to_string());
    let encrypt_algo: String = con
        .hget(crate::fs_key!("format"), "encrypt_algo")
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "none".to_string());
    let encrypt_key: Option<String> = con
        .hget(crate::fs_key!("format"), "encrypt_key")
        .await
        .unwrap_or(None);

    let crypto_state = crate::crypto_compress::CryptoCompressState::new(
        compression,
        encrypt_algo,
        encrypt_key.as_deref(),
    );

    let write_shards = if max_write_bytes < 10 * 1024 * 1024 {
        1
    } else {
        16
    };
    // Instantiate NvmeCache temporarily to recover from segment files
    let cache = crate::tiering::nvme::NvmeCache::new(
        &[staging_segment_dir.as_path()],
        &[max_write_bytes as usize],
        write_shards,
    )?;

    if crate::cache::nvme::dir_has_segment_data(&staging_segment_dir) {
        cache.recover_index();
    }

    let mut recovered_count = 0;
    let keys = cache.list_keys();
    for key_bytes in keys {
        let file_id = String::from_utf8(key_bytes.to_vec()).unwrap_or_default();
        if file_id.is_empty() {
            continue;
        }

        if file_id.starts_with("active_block:") {
            let active_block_data = {
                if let Some(guard) = cache.get(&key_bytes) {
                    let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
                    parse_staged_blob(bytes, true)
                } else {
                    None
                }
            }; // guard dropped here

            if let Some((meta, data)) = active_block_data {
                // Parse inode and block index
                let parts: Vec<&str> = file_id.split(':').collect();
                if parts.len() == 3 {
                    let ino_part = parts[1].trim_start_matches("inode_");
                    let b_part = parts[2].trim_start_matches("block_");
                    if let (Ok(ino), Ok(b)) = (ino_part.parse::<u64>(), b_part.parse::<u32>()) {
                        // Cross-reference metadata
                        let meta_key = crate::keys::metadata_for_inode(ino);
                        let exists: bool = con.exists(&meta_key).await.unwrap_or(false);
                        if exists {
                            let db_fencing: Option<u64> =
                                con.hget(&meta_key, "fencing_token").await.unwrap_or(None);
                            if let Some(df) = db_fencing {
                                if meta.fencing_token < df {
                                    warn!(
                                        "Crash Recovery: Stale fencing token {} detected for inode {} (database has {}). Discarding entry.",
                                        meta.fencing_token, ino, df
                                    );
                                    cache.remove(&key_bytes);
                                    continue;
                                }
                            }
                            let block_map_id_opt: Option<String> =
                                con.hget(&meta_key, "block_map_id").await?;
                            let mut block_map_id = block_map_id_opt.unwrap_or_default();
                            if block_map_id.is_empty() {
                                block_map_id = uuid::Uuid::new_v4().to_string();
                                let _: () =
                                    con.hset(&meta_key, "block_map_id", &block_map_id).await?;
                            }

                            let block_map_key = format!("block_map:{}", block_map_id);
                            let old_block_key: Option<String> =
                                con.hget(&block_map_key, b.to_string()).await?;

                            let data_bytes = bytes::Bytes::from(data);
                            let processed_block = match crypto_state
                                .process_write(data_bytes.clone())
                            {
                                Ok(b) => b,
                                Err(e) => {
                                    error!(
                                         "Crash Recovery: Failed to process active block {} of inode {} with crypto/compression: {:?}",
                                         b, ino, e
                                     );
                                    cache.remove(&key_bytes);
                                    continue;
                                }
                            };

                            info!(
                                 "Crash Recovery: Recovering active block {} for inode {} (size: {} bytes, fencing token: {})",
                                 b, ino, data_bytes.len(), meta.fencing_token
                             );

                            let data_len = data_bytes.len();
                            let processed_len = processed_block.len();
                            let offset = block_allocator.allocate_block().await?;
                            let new_block_key = offset.to_string();
                            if let Err(e) = nvme_writer.write_block(offset, &processed_block).await
                            {
                                error!(
                                    "Crash Recovery: Failed to write active block {} of inode {} to backing device: {:?}",
                                    b, ino, e
                                );
                                continue;
                            }

                            let active_be = "backend_0";
                            let stored_block_key = format!("{}://{}", active_be, new_block_key);

                            let refcounts_key_str = crate::fs_key!("block_refcounts");
                            let refcounts_key = &refcounts_key_str;
                            let mut pipe = redis::pipe();
                            pipe.hset(refcounts_key, &stored_block_key, 1)
                                .hset(&block_map_key, b.to_string(), &stored_block_key)
                                .hset(
                                    crate::fs_key!("block_sizes"),
                                    &stored_block_key,
                                    format!("{}:{}", data_len, processed_len),
                                );
                            let _: () = pipe.query_async(&mut con).await?;

                            if let Some(bk) = old_block_key {
                                let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                                if let Some(mut r) = old_ref {
                                    r -= 1;
                                    if r <= 0 {
                                        let _: () = redis::pipe()
                                            .hdel(refcounts_key, &bk)
                                            .hdel(crate::fs_key!("block_sizes"), &bk)
                                            .query_async(&mut con)
                                            .await?;
                                    } else {
                                        let _: () = con.hset(refcounts_key, &bk, r).await?;
                                    }
                                } else {
                                    let _: () = con
                                        .hdel(crate::fs_key!("block_sizes"), &bk)
                                        .await
                                        .unwrap_or(());
                                }
                            }

                            recovered_count += 1;
                        } else {
                            warn!(
                                "Crash Recovery: Stale active block detected for inode {} (block {}). Inode metadata does not exist. Discarding entry.",
                                ino, b
                            );
                        }
                    }
                }
            }
            cache.remove(&key_bytes);
            continue;
        }

        debug!(
            "Crash Recovery: Found staged write transaction ID: {}",
            file_id
        );

        let staged_data = {
            if let Some(guard) = cache.get(&key_bytes) {
                let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
                match parse_staged_blob(bytes, false) {
                    Some(v) => Some(v),
                    None => {
                        error!(
                            "Crash Recovery: Failed to parse binary staged metadata for key {}",
                            file_id
                        );
                        None
                    }
                }
            } else {
                None
            }
        }; // guard dropped here

        if let Some((meta, data)) = staged_data {
            // Cross-reference with Garnet
            let meta_key = crate::keys::metadata_for_path(&meta.file_path);
            let redis_file_id: Option<String> = con.hget(&meta_key, "file_id").await?;
            let redis_file_type: Option<String> = con.hget(&meta_key, "type").await?;
            let db_fencing: Option<u64> =
                con.hget(&meta_key, "fencing_token").await.unwrap_or(None);

            let mut should_recover = redis_file_type.as_deref() == Some("staged")
                && redis_file_id.as_deref() == Some(&file_id);

            if should_recover {
                if let Some(df) = db_fencing {
                    if meta.fencing_token < df {
                        warn!(
                            "Crash Recovery: Stale fencing token {} detected for staged file '{}' (database has {}). Discarding entry.",
                            meta.fencing_token, meta.file_path, df
                        );
                        should_recover = false;
                    }
                }
            }

            if should_recover {
                info!(
                    "Crash Recovery: Recovering write for '{}' (ID: {}, size: {} bytes, fencing token: {})",
                    meta.file_path, file_id, data.len(), meta.fencing_token
                );

                // Write to backing block device
                let offset = block_allocator.allocate_block().await?;
                let recovered_key = offset.to_string();
                if let Err(e) = nvme_writer
                    .write_block(offset, &bytes::Bytes::from(data.clone()))
                    .await
                {
                    error!(
                        "Crash Recovery: Failed to write recovered block to NVMe-oF backend: {:?}",
                        e
                    );
                    continue;
                }

                // Update Garnet mapping
                let mapping_key = format!("mapping:{}", file_id);
                let _: () = redis::pipe()
                    .hset(&mapping_key, "block", &recovered_key)
                    .hset(&mapping_key, "offset", 0u64)
                    .hset(&mapping_key, "size", data.len() as u64)
                    .hset(
                        crate::fs_key!("block_sizes"),
                        &recovered_key,
                        format!("{}:{}", data.len(), data.len()),
                    )
                    .query_async(&mut con)
                    .await?;

                recovered_count += 1;
            } else {
                warn!(
                    "Crash Recovery: Stale write detected for '{}' (ID: {}). Redis has type={:?} and file_id={:?}. Discarding stale local entry.",
                    meta.file_path, file_id, redis_file_type, redis_file_id
                );
            }
        }

        // Clean up entry from local staging cache
        cache.remove(&key_bytes);
    }

    info!(
        "Crash Recovery: Staging directory scan complete. Recovered {} files.",
        recovered_count
    );
    Ok(recovered_count)
}
