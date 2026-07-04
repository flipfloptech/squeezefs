//! Volume defragmentation: move high-index blocks into low free holes.
//!
//! # Scaling (P2-11)
//!
//! A full-volume reverse map + `SMEMBERS` of every free block does not scale.
//! This module uses **bounded** passes:
//! - free holes via **SSCAN** (not full set load), capped at [`DefragOptions::max_moves`]
//! - high-block candidates via **SCAN** of layout meta, retaining only the highest
//!   [`DefragOptions::max_high_candidates`] offsets above a low-zone threshold
//!
//! Each run therefore performs a limited amount of work; re-run for further passes.

use crate::block_allocator::BlockAllocator;
use crate::error::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Tunables for an incremental defrag pass (P2-11).
#[derive(Debug, Clone)]
pub struct DefragOptions {
    /// Maximum block migrations to schedule in this pass.
    pub max_moves: usize,
    /// Redis `COUNT` hint for `SCAN` / `SSCAN` batches.
    pub scan_count: usize,
    /// Max high-offset candidates retained while scanning metadata.
    pub max_high_candidates: usize,
    /// Optional target inode to defragment. If Some, defragments only this file.
    pub target_inode: Option<u64>,
}

impl Default for DefragOptions {
    fn default() -> Self {
        Self {
            max_moves: 256,
            scan_count: 200,
            max_high_candidates: 512,
            target_inode: None,
        }
    }
}

/// Keep only free block **indices** in the low zone, sorted ascending, capped.
pub fn select_low_free_holes(
    free_indices: impl IntoIterator<Item = u64>,
    low_max_idx: u64,
    max_holes: usize,
) -> Vec<u64> {
    let mut holes: Vec<u64> = free_indices
        .into_iter()
        .filter(|&idx| idx <= low_max_idx && idx > 0)
        .collect();
    holes.sort_unstable();
    holes.truncate(max_holes);
    holes
}

/// Insert a high-block candidate; drop lowest offsets when over capacity.
pub fn insert_high_candidate(
    map: &mut BTreeMap<u64, (u64, String, String)>,
    offset: u64,
    info: (u64, String, String),
    min_high_offset: u64,
    max_candidates: usize,
) {
    if offset < min_high_offset || max_candidates == 0 {
        return;
    }
    map.insert(offset, info);
    while map.len() > max_candidates {
        if let Some(lowest) = map.keys().next().copied() {
            map.remove(&lowest);
        } else {
            break;
        }
    }
}

pub async fn run_defragmentation(redis_url: &str, fs_name: &str, nvme_path: &str) -> Result<()> {
    run_defragmentation_with_options(redis_url, fs_name, nvme_path, DefragOptions::default()).await
}

pub async fn run_defragmentation_with_options(
    redis_url: &str,
    fs_name: &str,
    _nvme_path: &str,
    opts: DefragOptions,
) -> Result<()> {
    let dlm = crate::dlm::DlmClient::new(redis_url)?;
    let client = Arc::new(dlm.meta_client().clone());

    let block_alloc = Arc::new(BlockAllocator::new(client.clone(), fs_name).await?);

    // 1. Calculate fragmentation (cheap meta counters — not a full free-set load)
    let (highest_block, used_blocks, free_blocks, frag_percent) =
        block_alloc.calculate_fragmentation().await?;
    println!(
        "Volume Fragmentation: {:.2}% (Used: {}, Free: {}, Highest Block: {})",
        frag_percent, used_blocks, free_blocks, highest_block
    );

    if free_blocks == 0 {
        println!("No fragmentation detected. Exiting.");
        return Ok(());
    }

    let chunk_size = 4 * 1024 * 1024u64;
    let _ = (used_blocks, highest_block); // logged above; pairing uses free-hole order

    let mut conn = client.get_connection().await?;
    let free_set_key = format!("{}:free_blocks", fs_name);

    // 2. Sample free holes with SSCAN (bounded) — never SMEMBERS the whole set.
    // Prefer the *lowest* free indices (true holes for packing), independent of
    // possibly-stale global used-block counters on shared test DBs.
    println!(
        "Scanning free-block set for up to {} low-index holes (SSCAN)...",
        opts.max_moves
    );
    let free_holes = collect_low_free_holes_sscan(
        &mut conn,
        &free_set_key,
        u64::MAX, // collect lowest free indices; cap applied in select_low_free_holes
        &opts,
    )
    .await?;

    if free_holes.is_empty() {
        println!("No low-index holes available for defragmentation.");
        return Ok(());
    }

    // High zone: only blocks whose byte offset is above the highest low hole we
    // plan to fill (so a move always packs downward).
    let max_hole_idx = *free_holes.last().unwrap_or(&0);
    let min_high_offset = max_hole_idx.saturating_add(1).saturating_mul(chunk_size);

    // 3. Collect high-offset candidates (either target inode only, or global scan)
    let mut block_to_file: BTreeMap<u64, (u64, String, String)> = BTreeMap::new();
    if let Some(ino) = opts.target_inode {
        println!(
            "Collecting block candidates for target inode {} (cap {} candidates, min offset {})...",
            ino, opts.max_high_candidates, min_high_offset
        );
        collect_inode_block_candidates(
            &mut conn,
            ino,
            min_high_offset,
            opts.max_high_candidates,
            &mut block_to_file,
        )
        .await?;
    } else {
        println!(
            "Scanning metadata for high blocks (cap {} candidates, min offset {})...",
            opts.max_high_candidates, min_high_offset
        );
        collect_high_block_candidates(
            &mut conn,
            min_high_offset,
            opts.max_high_candidates,
            opts.scan_count,
            &mut block_to_file,
        )
        .await?;
    }

    if block_to_file.is_empty() {
        println!("No high blocks found above the selected free holes.");
        return Ok(());
    }

    // 4. Pair low holes with highest candidates (same policy as before, bounded).
    let mut tasks = Vec::new();

    for target_hole_idx in free_holes {
        let (&highest_offset, highest_file_info) = match block_to_file.iter().next_back() {
            Some(entry) => entry,
            None => break,
        };
        let highest_file_info = highest_file_info.clone();

        if highest_offset < min_high_offset {
            break;
        }

        let target_hole_offset = target_hole_idx * chunk_size;

        if block_alloc
            .allocate_specific_block(target_hole_idx)
            .await
            .is_ok()
        {
            let (ino, map_id, idx_str) = highest_file_info;
            tasks.push(crate::jobs::TaskType::BlockMove {
                ino,
                map_id: map_id.clone(),
                idx_str: idx_str.clone(),
                src_offset: highest_offset,
                dest_offset: target_hole_offset,
                len: chunk_size as usize,
            });

            block_to_file.remove(&highest_offset);
            block_to_file.insert(target_hole_offset, (ino, map_id, idx_str));
        }

        if tasks.len() >= opts.max_moves {
            break;
        }
    }

    if tasks.is_empty() {
        println!("No low-index holes to defragment.");
        return Ok(());
    }

    println!(
        "Submitting defragmentation job with {} block migrations to the cluster...",
        tasks.len()
    );
    crate::jobs::submit_and_wait_for_job(redis_url, fs_name, tasks).await?;
    Ok(())
}

async fn collect_low_free_holes_sscan(
    conn: &mut crate::dlm::MetaConnection,
    free_set_key: &str,
    low_max_idx: u64,
    opts: &DefragOptions,
) -> Result<Vec<u64>> {
    let mut cursor: u64 = 0;
    let mut sampled: Vec<u64> = Vec::new();
    loop {
        let (next_cursor, members): (u64, Vec<String>) = redis::cmd("SSCAN")
            .arg(free_set_key)
            .arg(cursor)
            .arg("COUNT")
            .arg(opts.scan_count)
            .query_async(conn)
            .await?;

        for m in members {
            if let Ok(idx) = m.parse::<u64>() {
                if idx > 0 && idx <= low_max_idx {
                    sampled.push(idx);
                }
            }
        }

        cursor = next_cursor;
        // Early stop once we have more than enough; still sort/truncate below.
        if cursor == 0 || sampled.len() >= opts.max_moves.saturating_mul(4) {
            break;
        }
    }

    Ok(select_low_free_holes(sampled, low_max_idx, opts.max_moves))
}

async fn collect_high_block_candidates(
    conn: &mut crate::dlm::MetaConnection,
    min_high_offset: u64,
    max_candidates: usize,
    scan_count: usize,
    out: &mut BTreeMap<u64, (u64, String, String)>,
) -> Result<()> {
    let mut cursor: u64 = 0;
    let match_pattern = "metadata:inode_*";
    loop {
        let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(match_pattern)
            .arg("COUNT")
            .arg(scan_count)
            .query_async(conn)
            .await?;

        for key in keys {
            let inode_str = key.strip_prefix("metadata:inode_").unwrap_or(&key);
            let ino = match inode_str.parse::<u64>() {
                Ok(i) => i,
                Err(_) => continue,
            };

            let (file_type, block_map_id_opt): (Option<String>, Option<String>) =
                redis::cmd("HMGET")
                    .arg(&key)
                    .arg("type")
                    .arg("block_map_id")
                    .query_async(conn)
                    .await?;

            if file_type.as_deref() != Some("striped") {
                continue;
            }
            let Some(block_map_id) = block_map_id_opt else {
                continue;
            };

            let block_map_key = crate::keys::block_map(&block_map_id);
            // HSCAN in batches would be nicer for huge maps; HGETALL is fine for
            // typical per-file block counts and keeps this path simple.
            let mappings: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
                .arg(&block_map_key)
                .query_async(conn)
                .await?;

            for (block_idx_str, offset_str) in mappings {
                if let Ok(offset) = offset_str.parse::<u64>() {
                    insert_high_candidate(
                        out,
                        offset,
                        (ino, block_map_id.clone(), block_idx_str),
                        min_high_offset,
                        max_candidates,
                    );
                }
            }
        }

        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }
    Ok(())
}

async fn collect_inode_block_candidates(
    conn: &mut crate::dlm::MetaConnection,
    ino: u64,
    min_high_offset: u64,
    max_candidates: usize,
    out: &mut BTreeMap<u64, (u64, String, String)>,
) -> Result<()> {
    let meta_key = crate::keys::metadata_for_inode(ino);
    let (file_type, block_map_id_opt): (Option<String>, Option<String>) = redis::cmd("HMGET")
        .arg(&meta_key)
        .arg("type")
        .arg("block_map_id")
        .query_async(conn)
        .await?;

    if file_type.as_deref() != Some("striped") {
        return Ok(());
    }
    let Some(block_map_id) = block_map_id_opt else {
        return Ok(());
    };

    let block_map_key = crate::keys::block_map(&block_map_id);
    let mappings: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
        .arg(&block_map_key)
        .query_async(conn)
        .await?;

    for (block_idx_str, offset_str) in mappings {
        if let Ok(offset) = offset_str.parse::<u64>() {
            insert_high_candidate(
                out,
                offset,
                (ino, block_map_id.clone(), block_idx_str),
                min_high_offset,
                max_candidates,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_low_free_holes_filters_sorts_caps() {
        let holes = select_low_free_holes([9u64, 0, 2, 5, 1, 100], 5, 3);
        assert_eq!(holes, vec![1, 2, 5]);
    }

    #[test]
    fn test_insert_high_candidate_keeps_highest() {
        let mut map = BTreeMap::new();
        for (off, ino) in [(10u64, 1u64), (50, 2), (30, 3), (80, 4), (20, 5)] {
            insert_high_candidate(
                &mut map,
                off,
                (ino, "m".into(), "0".into()),
                15, // min high
                3,
            );
        }
        // Offsets < 15 dropped; among rest keep highest 3: 30,50,80
        let keys: Vec<u64> = map.keys().copied().collect();
        assert_eq!(keys, vec![30, 50, 80]);
    }
}
