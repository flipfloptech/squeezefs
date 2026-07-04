//! Volume defragmentation: move high-index blocks into low free holes.

use crate::error::Result;
use std::collections::BTreeMap;

/// Tunables for an incremental defrag pass.
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
    _redis_url: &str,
    _fs_name: &str,
    _nvme_path: &str,
    _opts: DefragOptions,
) -> Result<()> {
    Ok(())
}
