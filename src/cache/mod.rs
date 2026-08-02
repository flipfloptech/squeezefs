pub mod active_block;
pub mod gds;
pub mod lru;
pub mod nvme;
pub mod pool;
pub use nvme::{
    read_staging_generation_marker, scan_live_staged_custody, seed_staged_custody_for_test,
    write_staging_generation_marker, write_staging_generation_prepare_marker, NvmeStaging,
};
pub use pool::{AlignedBufPool, PooledBuf, ALIGNED_BUF_POOL, BUFFER_POOL};

use crate::error::{Result, SqueezefsError};
use std::path::PathBuf;

#[derive(Clone)]
pub struct TieredCache {
    pub gds: gds::GdsCache,
    pub read_lru: lru::LruCache,
    pub write_lru: lru::LruCache,
    /// R4 (docs/design-read-path.md §5.4): budgeted RAM tier for the
    /// > 256 KiB striped-block population the `read_lru` gate excludes.
    /// Values are `Bytes` refcount clones — a put/hit never memcpys.
    /// Filled ONLY by device-validated fills (never re-promoted from the
    /// NVMe tier, never owner-put from the write path); budget carved
    /// BESIDE `--read-mem-cache-size`, not from it
    /// (`SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`; 0 short-circuits the tier).
    pub hot_block: lru::LruCache,
    /// Scan-resistant admission governor (docs/design-read-path.md §5.3
    /// addendum): owns the escalation clamp windows; fed waste by the
    /// hot-block tier's evictions and foreground bytes by the ranged
    /// device-read site.
    pub admission_governor: std::sync::Arc<crate::routing::AdmissionGovernor>,
    /// The read-lane completed-fill hold (2026-08-01 campaign,
    /// `src/read_lane.rs`): the LEDGER-INVISIBLE landing zone for
    /// governed cold-stream lane fetches and the deep-qd cohort
    /// stability store. A block-key-addressed byte store, so it joins
    /// the unified purge below (the R-6 law) — but it is deliberately
    /// NOT a cache tier: no admission, no ghost, no governor ledger, no
    /// dehydration; entries retire on consumption coverage.
    pub read_lane_hold: std::sync::Arc<crate::read_lane::ReadLaneHold>,
    pub nvme: nvme::NvmeStaging,
}

impl TieredCache {
    pub fn set_backend_router(&self, router: std::sync::Arc<crate::routing::BackendRouter>) {
        self.nvme.set_backend_router(router);
    }

    /// `fs_generation` binds local staging to the mounted filesystem
    /// generation (see [`nvme::NvmeStaging::new`]); `None` only for
    /// offline tooling with no metadata volume set.
    pub async fn new(
        staging_dirs: Vec<PathBuf>,
        read_mem_cache_size: Option<&str>,
        write_mem_cache_size: Option<&str>,
        read_disk_cache_size: Option<&str>,
        write_disk_cache_size: Option<&str>,

        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        fs_generation: Option<&str>,
    ) -> Result<Self> {
        let gds = gds::GdsCache::new(staging_dirs.clone());

        // 1. Get memory size limit
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory(); // In bytes

        let read_mem_limit =
            if let Some(cfg) = read_mem_cache_size.filter(|c| !c.is_empty() && *c != "none") {
                parse_size_string(cfg, total_memory)?
            } else {
                // Default 10% of system RAM
                total_memory / 10
            };

        let write_mem_limit =
            if let Some(cfg) = write_mem_cache_size.filter(|c| !c.is_empty() && *c != "none") {
                parse_size_string(cfg, total_memory)?
            } else {
                // Default 10% of system RAM
                total_memory / 10
            };

        let read_lru = lru::LruCache::with_capacity(read_mem_limit);
        let write_lru = lru::LruCache::with_capacity(write_mem_limit);

        // R4 hot-block budget: env override wins (0 disables); default
        // max(2 × block_size, 25 % of the read-mem limit) — the 2-block
        // floor is the minimum for one stream's consume-behind window.
        // Block size is a mount-time router knob; the 4 MiB default shape
        // is used for the floor (a smaller configured block only lowers
        // the need, never the floor's safety).
        let hot_budget = match std::env::var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB") {
            Ok(v) => {
                v.trim().parse::<u64>().map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB must be an integer MiB count: {e}"
                    ))
                })? * 1024
                    * 1024
            }
            Err(_) => std::cmp::max(2 * 4 * 1024 * 1024, read_mem_limit / 4),
        };
        // Scan-resistance clamp budget: escalation fill bytes as a percent
        // of foreground ranged device bytes while the waste signal holds
        // (default 5 — admission waste bounded to a few percent of the
        // device bandwidth the workload already pays; 0 = no escalations
        // while clamped; fitting working sets never clamp).
        let fill_pct = match std::env::var("SQUEEZEFS_READ_ADMISSION_FILL_PCT") {
            Ok(v) => {
                let p = v.trim().parse::<u64>().map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "SQUEEZEFS_READ_ADMISSION_FILL_PCT must be an integer percent 0..=100: {e}"
                    ))
                })?;
                if p > 100 {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "SQUEEZEFS_READ_ADMISSION_FILL_PCT must be 0..=100 (got {p})"
                    )));
                }
                p
            }
            Err(_) => 5,
        };
        let admission_governor =
            std::sync::Arc::new(crate::routing::AdmissionGovernor::new(fill_pct));
        // Shard geometry: 4 MiB hot values need shards that can hold a few
        // NEIGHBORS — the generic core-count sharding gave the default
        // 128 MiB budget 32 shards = ONE block per shard, so any two keys
        // sharing a shard could never coexist (measured on the rig as
        // perpetual protected churn at 50 % budget fill: 6.1k whole-block
        // refetches / 20 s on a FITTING 64 MiB set). Keep per-shard
        // capacity ≥ 4 default blocks (16 MiB).
        let hot_block = lru::LruCache::with_capacity_min_shard(hot_budget, 16 * 1024 * 1024)
            .with_drop_probation_evictions()
            .with_admission_governor(admission_governor.clone());

        // 2. Get disk size limit
        let aggregate_capacity = get_aggregate_disk_capacity(&staging_dirs);

        let read_disk_limit =
            if let Some(cfg) = read_disk_cache_size.filter(|c| !c.is_empty() && *c != "none") {
                parse_size_string(cfg, aggregate_capacity)?
            } else {
                #[cfg(test)]
                {
                    10 * 1024 * 1024 // 10MB default in tests
                }
                #[cfg(not(test))]
                {
                    // Default 25% of aggregate capacity
                    aggregate_capacity / 4
                }
            };

        let write_disk_limit =
            if let Some(cfg) = write_disk_cache_size.filter(|c| !c.is_empty() && *c != "none") {
                parse_size_string(cfg, aggregate_capacity)?
            } else {
                #[cfg(test)]
                {
                    10 * 1024 * 1024 // 10MB default in tests
                }
                #[cfg(not(test))]
                {
                    // Default 25% of aggregate capacity
                    aggregate_capacity / 4
                }
            };

        let nvme = nvme::NvmeStaging::new(
            staging_dirs,
            write_disk_limit,
            read_disk_limit,
            block_allocator.clone(),
            nvme_writer.clone(),
            fs_generation,
        )
        .await?;

        // Spawn background dehydration tasks to move evicted RAM blocks to
        // NVMe. PR 3 plumbs the victim CLASS through the channel
        // BEHAVIOR-NEUTRAL: every class still dehydrates (`read_lru`'s
        // plain-put inserts are all protected-class by definition, so the
        // ≤ 256 KiB population's dehydration is bit-identical to today,
        // key-filter included); the protected-only gate flip is PR 4
        // policy. The hot tier gets its own worker so evictions are
        // observable (`hot_block_evictions`); its victims carry offset-
        // string keys that the historical `blocks/` filter never matches,
        // so in PR 3 hot victims are dropped after counting — harmless,
        // because every hot fill also published to the NVMe tier in this
        // PR (admission is PR 4, which owns the dehydration policy).
        if let Some(mut evict_rx) = read_lru.take_evict_rx() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let nvme_clone = nvme.clone();
                let channel_gauge = read_lru.clone();
                // RES-8: a panic in this LOOP ends dehydration for the
                // life of the mount (warmth silently stops) — contained.
                handle.spawn(crate::detached::contain("read_lru_dehydrate", async move {
                    while let Some((key, data, _class)) = evict_rx.recv().await {
                        // R5: credit the channel byte gauge per message.
                        channel_gauge.evict_channel_sub(data.len() as u64);
                        // R5 Yellow+ (§5.7): dehydration paused ENTIRELY —
                        // drop the victim (its Bytes ref dies here instead
                        // of parking through a blocking write). Disk-tier
                        // warmth is the cheapest sacrifice under pressure.
                        if crate::mem_budget::level() >= crate::mem_budget::Level::Yellow {
                            crate::fuse_client::METRICS
                                .mem_budget_dehydrate_paused
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                        if key.contains("blocks/") {
                            let nvme_clone_inner = nvme_clone.clone();
                            let key_clone = key.clone();
                            let data_clone = data.clone();
                            // Dehydration is a non-owner publish of a payload
                            // that parked in this channel for arbitrarily
                            // long: incarnation-validated or not at all
                            // (generic/074 stale-fill family).
                            if data_clone.len() < 64 * 1024 {
                                let _ = nvme_clone_inner
                                    .cache_read_block_validated_self(&key_clone, data_clone);
                            } else {
                                let _ = tokio::task::spawn_blocking(move || {
                                    nvme_clone_inner
                                        .cache_read_block_validated_self(&key_clone, data_clone)
                                })
                                .await;
                            }
                        }
                    }
                }));
            }
        }
        if let Some(mut evict_rx) = hot_block.take_evict_rx() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let nvme_clone = nvme.clone();
                let channel_gauge = hot_block.clone();
                // RES-8: same — contained + counted.
                handle.spawn(crate::detached::contain(
                    "hot_block_dehydrate",
                    async move {
                        while let Some((key, data, class)) = evict_rx.recv().await {
                            // R5: credit the channel byte gauge per message.
                            channel_gauge.evict_channel_sub(data.len() as u64);
                            crate::fuse_client::METRICS
                                .hot_block_evictions
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            match class {
                                // R1b gate flip (§5.3, PR 4): a probation
                                // victim nothing ever read is a one-pass
                                // stream's residue — dehydrating it is the
                                // publish tax in RAM-eviction form. Drop it.
                                crate::tiering::memory::EvictClass::Probation => {
                                    crate::fuse_client::METRICS
                                        .hot_block_probation_drops
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                // A protected victim was worth keeping (read-
                                // promoted or ghost-admitted): preserve its
                                // warmth on the NVMe tier via the validated
                                // non-owner publish (074 discipline). No
                                // legacy `blocks/` key filter here — hot-tier
                                // keys are offset strings; the filter belongs
                                // to the read_lru worker's historical
                                // population only.
                                //
                                // DEDUPE at the channel mouth: under
                                // second-touch, every ghost-admitted fill
                                // published at fetch time AND landed protected
                                // in hot — its warmth is already tier-durable,
                                // and re-writing it here per hot eviction is
                                // duplicate-write churn (measured on the
                                // rand-4k bench row: ~100% protected
                                // evictions, each a redundant multi-MiB
                                // spawn_blocking write competing with the
                                // reads). Index-membership probe only; a
                                // racing tier eviction after the probe just
                                // re-cools one block — the always-true device
                                // fallback, never wrongness.
                                crate::tiering::memory::EvictClass::Protected { .. } => {
                                    // R5 Yellow+ (§5.7): dehydration paused
                                    // entirely — protected victims INCLUDED
                                    // (an escalation over the steady-state
                                    // probation-drop policy; frees the queued
                                    // Bytes and stops tier-mmap dirty growth).
                                    if crate::mem_budget::level()
                                        >= crate::mem_budget::Level::Yellow
                                    {
                                        crate::fuse_client::METRICS
                                            .mem_budget_dehydrate_paused
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    } else if nvme_clone.has_cached_read_block(&key) {
                                        crate::fuse_client::METRICS
                                            .hot_block_dehydrate_skips
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    } else {
                                        let nvme_inner = nvme_clone.clone();
                                        let _ = tokio::task::spawn_blocking(move || {
                                            nvme_inner.cache_read_block_validated_self(&key, data)
                                        })
                                        .await;
                                    }
                                }
                            }
                        }
                    },
                ));
            }
        }

        Ok(Self {
            gds,
            read_lru,
            write_lru,
            hot_block,
            admission_governor,
            read_lane_hold: std::sync::Arc::new(crate::read_lane::ReadLaneHold::new()),
            nvme,
        })
    }

    /// Purge every block-key-addressed cache tier for a block key — ALL
    /// FOUR: RAM LRU, hot-block tier, NVMe disk tier, and the GDS file
    /// cache. The ONLY legal way to drop a block key from the caches —
    /// displaced-key frees, fill undos, incarnation purges and the
    /// read_tier_purge callback all route here, so a tier (present or
    /// future) cannot be forgotten by one call site (the 074 family's
    /// lesson; pinned by the census grep-guard in
    /// tests/hot_block_tier_tests.rs). Latch-free on the RAM/index arms;
    /// the GDS arm is an unlink syscall (ENOENT ignored) — cheap because
    /// `.gds_cache` files exist only on GDS-warmed mounts.
    pub fn purge_block_key(&self, block_key: &str) {
        self.read_lru.remove(block_key);
        self.hot_block.remove(block_key);
        // The read-lane hold (2026-08-01): a fifth block-key byte store
        // joins the unified purge or it does not exist (the R-6 law) —
        // a displaced/freed key must never serve stale from the hold.
        self.read_lane_hold.purge(block_key);
        self.nvme.remove_cached_read_block(block_key);
        // The 4th block-key tier: `.gds_cache` files are keyed by block
        // key, written by the prefetch GDS arm and read_direct, and served
        // by the GDS ioctl behind a `!path.exists()` check that never
        // refreshes — within a mount, a freed-and-reallocated key would
        // serve the dead incarnation's file forever (cross-mount is closed
        // by wipe_gds_cache_files). Name construction is unified through
        // `get_gds_path` (PR 3), so this unlink covers every producer by
        // construction, not by enumeration.
        self.gds.remove_cached(block_key);
    }
}

/// Helper function to parse size configs like "128GB", "50%" into raw bytes.
pub fn parse_size_string(val: &str, system_total: u64) -> Result<u64> {
    let s = val.trim();
    if s.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "Empty size configuration".to_string(),
        ));
    }

    if let Some(stripped) = s.strip_suffix('%') {
        let pct_str = stripped.trim();
        let pct = pct_str.parse::<f64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "Invalid percentage format '{}': {:?}",
                pct_str, e
            ))
        })?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Percentage out of bounds: {}",
                pct
            )));
        }
        return Ok((system_total as f64 * pct / 100.0) as u64);
    }

    let mut num_end = s.len();
    for (i, c) in s.char_indices() {
        if !c.is_numeric() && c != '.' {
            num_end = i;
            break;
        }
    }

    let num_str = s[..num_end].trim();
    let unit_str = s[num_end..].trim().to_uppercase();

    let val_f = num_str.parse::<f64>().map_err(|e| {
        SqueezefsError::InvalidOperation(format!(
            "Invalid size numeric value '{}': {:?}",
            num_str, e
        ))
    })?;

    let multiplier: u64 = match unit_str.as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        "TB" | "T" => 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "Unknown size unit '{}'",
                unit_str
            )))
        }
    };

    Ok((val_f * multiplier as f64) as u64)
}

/// Helper function to calculate aggregate capacity of disks mapped to staging paths.
pub fn get_aggregate_disk_capacity(paths: &[PathBuf]) -> u64 {
    let mut total_capacity = 0;

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        for path in paths {
            let abs_path = path.canonicalize().unwrap_or_else(|_| path.clone());
            let c_path = match CString::new(abs_path.as_os_str().as_bytes()) {
                Ok(s) => s,
                Err(_) => continue,
            };

            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                    total_capacity += stat.f_blocks as u64 * stat.f_frsize as u64;
                }
            }
        }
    }

    if total_capacity == 0 {
        100 * 1024 * 1024 * 1024 // Fallback 100GB
    } else {
        total_capacity
    }
}

/// Helper function to parse duration strings like "500ms", "5s", "1000" into a standard Duration.
pub fn parse_duration(val: &str) -> Result<std::time::Duration> {
    let s = val.trim().to_lowercase();
    if s.ends_with("ms") {
        let num_str = &s[..s.len() - 2];
        let ms = num_str.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_millis(ms))
    } else if s.ends_with('s') {
        let num_str = &s[..s.len() - 1];
        let secs = num_str.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_secs(secs))
    } else {
        let ms = s.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("invalid duration number: {}", e))
        })?;
        Ok(std::time::Duration::from_millis(ms))
    }
}
