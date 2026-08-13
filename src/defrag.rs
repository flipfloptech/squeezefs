//! PR VL7 — the online-defrag **measurement engine**
//! (docs/design-volume-lifecycle.md §5.7, KD-11).
//!
//! Fragmentation in SqueezeFS IS the four measured axes (KD-11) — each
//! with a gauge here and an independently invocable mover on the job
//! fabric (`crate::jobs`):
//!
//! | Axis | Metric (this module) | Mover |
//! |------|----------------------|-------|
//! | **D1** free-space contiguity | per volume, over the ALLOCATED offset space `[0, highest)`: `contiguity = largest_free_run / total_free`, `reclaimable_tail = trailing_free_run / total_free` | `JobType::DefragData` — tail blocks into low-offset same-backend gaps (`move_one` + `DestPick::CompactLow`) |
//! | **D2** file locality | fraction of logically-adjacent striped block pairs whose physical mappings are same-backend ascending | `JobType::DefragData` — refcount-1 quiescent rewrites onto one backend (`DestPick::BackendAscending`) |
//! | **D3** staged-extent pressure | parked overlay bytes (`parked_extent_bytes`) + spilled `active_block_ext:` record bytes | `JobType::DefragFold` — the W2 fold machinery kicked to completion |
//! | **D4** meta node occupancy | per meta volume: dead (superseded) records vs distinct keys in the serialized bset logs (`KvMetaBackend::dead_bset_census`) | `JobType::DefragMeta` — leaf compaction through the SMO serialization |
//!
//! The virgin tail past `highest_block` deliberately does NOT count as
//! free space for D1: it is already contiguous and already reclaimable —
//! counting it would mask real fragmentation on any volume that is not
//! nearly full. `capacity_bytes`/`space_blocks` ride the report so the
//! shrink-cost view ("highest used offset vs capacity") stays derivable.
//!
//! **Gauge publication** (§10): the four `frag_*` gauges live on
//! [`METRICS`] — ratios permille+1 encoded ([`encode_ratio`], raw `0` =
//! never measured ⇒ the stats JSON emits `null`), worst-volume semantics
//! for the per-volume axes. D1/D3 are cheap (allocator snapshot + record
//! lens) and refresh on the [`spawn_gauge_worker`] cadence plus after
//! every defrag mover pass; D2/D4 are walk-priced and refresh whenever
//! [`measure`] runs (`--report-only`, the defrag verbs) — stale-until-
//! measured by design, never silently guessed.

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::RoutedMetaBackend;
use crate::routing::DataRouter;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Encode a `[0, 1]` ratio for an atomic gauge: permille + 1, so raw `0`
/// stays the never-measured sentinel. Out-of-range inputs clamp.
pub fn encode_ratio(r: f64) -> u64 {
    (r.clamp(0.0, 1.0) * 1000.0).round() as u64 + 1
}

/// Decode a permille+1 gauge (`None` = never measured).
pub fn decode_ratio(raw: u64) -> Option<f64> {
    (raw != 0).then(|| (raw - 1) as f64 / 1000.0)
}

/// One volume's D1 row (free-space contiguity over `[0, highest)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct D1Volume {
    pub id: String,
    /// Bound device capacity (`0` = unbounded offline/test allocator).
    pub capacity_bytes: u64,
    /// The allocated offset space in blocks (`highest_block`).
    pub space_blocks: u64,
    pub used_blocks: u64,
    pub free_blocks: u64,
    pub largest_free_run: u64,
    /// Free blocks in the trailing run (above the highest used offset).
    pub tail_free_blocks: u64,
    /// `largest_free_run / free_blocks` (1.0 when nothing is free).
    pub contiguity: f64,
    /// `tail_free_blocks / free_blocks` (1.0 when nothing is free).
    pub reclaimable_tail: f64,
}

/// The set-level D2 row (file locality).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct D2Report {
    /// Striped files with at least one adjacency pair.
    pub files: u64,
    /// Logically-adjacent block pairs measured.
    pub pairs: u64,
    /// Pairs whose mappings are same-backend ascending.
    pub local_pairs: u64,
    /// `local_pairs / pairs` (1.0 when no pairs exist).
    pub locality: f64,
}

/// The D3 row (staged-extent pressure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct D3Report {
    pub parked_extent_bytes: u64,
    pub spilled_records: u64,
    pub spilled_record_bytes: u64,
    /// `parked + spilled` — the `frag_d3_pressure_bytes` gauge.
    pub pressure_bytes: u64,
}

/// One meta volume's D4 row (dead-bset occupancy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct D4Volume {
    /// The volume's device path (meta volumes have no `vol-` ids).
    pub volume: String,
    pub leaves: u64,
    pub records_indexed: u64,
    pub records_live: u64,
    /// `1 − live/indexed` (0.0 when empty).
    pub dead_bset_ratio: f64,
}

/// The full four-axis report — the `--report-only` JSON body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefragReport {
    pub d1: Vec<D1Volume>,
    pub d2: D2Report,
    pub d3: D3Report,
    pub d4: Vec<D4Volume>,
}

// ---------------------------------------------------------------------------
// D1 — free-space contiguity (allocator census, cheap/sync)
// ---------------------------------------------------------------------------

/// Measure D1 for every registered data volume. Sync and latch-free
/// (DashSet/scc snapshots); a racing foreground write can skew a row by
/// its in-flight blocks — the movers re-plan from fresh censuses, and
/// the gauges refresh on cadence.
pub fn measure_d1(router: &DataRouter) -> Vec<D1Volume> {
    let br = &router.backend_router;
    let mut rows: Vec<D1Volume> = Vec::new();
    for entry in br.backends.iter() {
        let alloc = &entry.value().block_allocator;
        let free = alloc.free_block_indices();
        let highest = alloc.highest_block_index();
        let free_total = free.len() as u64;
        let mut largest = 0u64;
        let mut run = 0u64;
        let mut prev: Option<u64> = None;
        for &idx in &free {
            run = match prev {
                Some(p) if idx == p + 1 => run + 1,
                _ => 1,
            };
            largest = largest.max(run);
            prev = Some(idx);
        }
        let mut tail = 0u64;
        for &idx in free.iter().rev() {
            if highest > tail && idx == highest - tail - 1 {
                tail += 1;
            } else {
                break;
            }
        }
        let (contiguity, reclaimable_tail) = if free_total == 0 {
            (1.0, 1.0)
        } else {
            (
                largest as f64 / free_total as f64,
                tail as f64 / free_total as f64,
            )
        };
        rows.push(D1Volume {
            id: entry.key().clone(),
            capacity_bytes: alloc.capacity_bytes(),
            space_blocks: highest,
            used_blocks: highest.saturating_sub(free_total),
            free_blocks: free_total,
            largest_free_run: largest,
            tail_free_blocks: tail,
            contiguity,
            reclaimable_tail,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

// ---------------------------------------------------------------------------
// D2 — file locality (census-walk priced)
// ---------------------------------------------------------------------------

/// One striped file's parsed block-map entries, logical order:
/// `(block_idx, mapping verbatim, parsed backend id, parsed offset)`.
pub(crate) struct FileMap {
    pub ino: u64,
    pub entries: Vec<(u32, String, String, u64)>,
}

/// Walk every meta volume's live inode tree and collect each striped
/// file's parsed block map (the `census_for` walk shape — inline maps
/// and indirect blobs both). Undecodable mappings are skipped (fsck owns
/// that surface).
pub(crate) async fn walk_striped_files(
    meta: &Arc<RoutedMetaBackend>,
    router: &DataRouter,
) -> Result<Vec<FileMap>> {
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let br = &router.backend_router;
    let block_size = router.block_size.load(Ordering::Relaxed) as usize;
    let mut out = Vec::new();
    for (vol_idx, kv) in meta.volumes.iter().enumerate() {
        let inodes = kv.trees()[0];
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = inodes.range(&cursor, &end, 512).await.map_err(|e| {
                SqueezefsError::InvalidOperation(format!("defrag D2 inode walk failed: {e}"))
            })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(local_ino) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                if val.nlink == 0 {
                    continue;
                }
                let Ok(Some(bytes)) = kv.getxattr(local_ino, "layout").await else {
                    continue;
                };
                let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
                    serde_json::from_slice(&bytes).ok()
                } else {
                    bincode::deserialize(&bytes).ok()
                };
                let Some(layout) = layout else { continue };
                if layout.file_type != "striped" {
                    continue;
                }
                // Guest-only members carry raw CONTROL records with no
                // global encoding — skip them (VL9 soak-found panic).
                let Some(global_ino) = meta.try_make_global_ino(local_ino, vol_idx) else {
                    continue;
                };

                let mut raw: Vec<(u32, String)> = Vec::new();
                if let Some(ref map_id) = layout.block_map_id {
                    if let Some(blob_key) = map_id.strip_prefix("indirect:") {
                        if let Ok(raw_bytes) = br.read_block(blob_key, block_size).await {
                            if let Ok(decoded) =
                                crate::routing::decode_indirect_block_map(&raw_bytes)
                            {
                                raw = decoded;
                            }
                        }
                    }
                }
                if raw.is_empty() {
                    if let Some(ref bm) = layout.block_map {
                        raw = bm.iter().map(|(&b, key)| (b, key.clone())).collect();
                    }
                }
                if raw.is_empty() {
                    continue;
                }
                let mut entries: Vec<(u32, String, String, u64)> = Vec::new();
                for (b, mapping) in raw {
                    let clean = crate::routing::clean_block_key(&mapping);
                    let Ok((be_id, off)) = br.parse_block_key(&clean) else {
                        continue;
                    };
                    entries.push((b, mapping, be_id, off));
                }
                entries.sort_by_key(|e| e.0);
                out.push(FileMap {
                    ino: global_ino,
                    entries,
                });
            }
        }
    }
    Ok(out)
}

/// The ONE locality definition (measurement and the D2 planner share
/// it): over logical-order entries, a pair counts when `idx₂ = idx₁+1`;
/// it is LOCAL when same backend and physically ascending.
pub(crate) fn locality_pairs(entries: &[(u32, String, String, u64)]) -> (u64, u64) {
    let mut pairs = 0u64;
    let mut local = 0u64;
    for w in entries.windows(2) {
        if w[1].0 == w[0].0 + 1 {
            pairs += 1;
            if w[1].2 == w[0].2 && w[1].3 > w[0].3 {
                local += 1;
            }
        }
    }
    (pairs, local)
}

/// Measure D2 over the durable inode trees.
pub async fn measure_d2(meta: &Arc<RoutedMetaBackend>, router: &DataRouter) -> Result<D2Report> {
    let files = walk_striped_files(meta, router).await?;
    let mut pairs = 0u64;
    let mut local = 0u64;
    let mut counted_files = 0u64;
    for f in &files {
        let (p, l) = locality_pairs(&f.entries);
        if p > 0 {
            counted_files += 1;
            pairs += p;
            local += l;
        }
    }
    Ok(D2Report {
        files: counted_files,
        pairs,
        local_pairs: local,
        locality: if pairs == 0 {
            1.0
        } else {
            local as f64 / pairs as f64
        },
    })
}

// ---------------------------------------------------------------------------
// D3 — staged-extent pressure (cheap/sync; read_staged takes the staging
// shard read lock — call on the blocking pool from async contexts)
// ---------------------------------------------------------------------------

/// Measure D3: RAM-parked overlay bytes plus the spilled
/// `active_block_ext:` records' stored bytes.
pub fn measure_d3(router: &DataRouter) -> D3Report {
    let parked = METRICS.parked_extent_bytes.load(Ordering::Relaxed);
    let mut records = 0u64;
    let mut record_bytes = 0u64;
    for key in router.cache.nvme.extent_record_keys("") {
        records += 1;
        if let Some(blob) = router.cache.nvme.read_staged(&key) {
            record_bytes += blob.len() as u64;
        }
    }
    D3Report {
        parked_extent_bytes: parked,
        spilled_records: records,
        spilled_record_bytes: record_bytes,
        pressure_bytes: parked.saturating_add(record_bytes),
    }
}

// ---------------------------------------------------------------------------
// D4 — meta node occupancy (tree-walk priced: leaves are paged resident
// first so the census covers the WHOLE volume, not the read working set)
// ---------------------------------------------------------------------------

/// Page every leaf of every tree resident (full range scans — the
/// demand-paged node cache is the census surface; eviction under budget
/// pressure leaves the census honest over what stayed resident).
pub(crate) async fn page_in_leaves(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
) -> Result<()> {
    for tree in kv.trees() {
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = tree
                .range(&cursor, &crate::meta_backend::kv::tree::KEY_SPACE_MAX, 512)
                .await
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("defrag D4 tree walk failed: {e}"))
                })?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
        }
    }
    Ok(())
}

/// Measure D4 per meta volume (leaves paged in first).
pub async fn measure_d4(meta: &Arc<RoutedMetaBackend>) -> Result<Vec<D4Volume>> {
    let mut rows = Vec::new();
    for kv in &meta.volumes {
        page_in_leaves(kv).await?;
        let census = kv.dead_bset_census();
        rows.push(D4Volume {
            volume: kv.device_path().display().to_string(),
            leaves: census.leaves,
            records_indexed: census.records_indexed,
            records_live: census.records_live,
            dead_bset_ratio: if census.records_indexed == 0 {
                0.0
            } else {
                1.0 - census.records_live as f64 / census.records_indexed as f64
            },
        });
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// The full measure + gauge publication
// ---------------------------------------------------------------------------

/// Compute all four axes and publish the §10 gauges. The
/// `--report-only` engine and the defrag verbs run this; G-VL-6's
/// census-match clause pins its numbers against independent
/// recomputation (`tests/defrag_tests.rs`).
pub async fn measure(meta: &Arc<RoutedMetaBackend>, router: &DataRouter) -> Result<DefragReport> {
    let d1 = measure_d1(router);
    let d2 = measure_d2(meta, router).await?;
    let d3 = {
        let r = router.clone();
        squeezefs_ipc::sqz_blocking::run_blocking(move || measure_d3(&r)).await
    };
    let d4 = measure_d4(meta).await?;

    publish_d1_gauges(&d1);
    METRICS
        .frag_d2_locality
        .store(encode_ratio(d2.locality), Ordering::Relaxed);
    METRICS
        .frag_d3_pressure_bytes
        .store(d3.pressure_bytes, Ordering::Relaxed);
    let worst_d4 = d4.iter().map(|r| r.dead_bset_ratio).fold(0.0, f64::max);
    METRICS
        .frag_d4_dead_bset_ratio
        .store(encode_ratio(worst_d4), Ordering::Relaxed);

    Ok(DefragReport { d1, d2, d3, d4 })
}

/// Publish the worst-volume D1 gauges from measured rows (an empty set
/// reads fully-contiguous — nothing exists to fragment).
fn publish_d1_gauges(rows: &[D1Volume]) {
    let worst_contig = rows.iter().map(|r| r.contiguity).fold(1.0, f64::min);
    let worst_tail = rows.iter().map(|r| r.reclaimable_tail).fold(1.0, f64::min);
    METRICS
        .frag_d1_contiguity
        .store(encode_ratio(worst_contig), Ordering::Relaxed);
    METRICS
        .frag_d1_reclaimable_tail
        .store(encode_ratio(worst_tail), Ordering::Relaxed);
}

/// The cheap-gauge refresh (D1 + D3): the defrag movers run it after
/// every pass; [`spawn_gauge_worker`] runs it on cadence. Sync — call on
/// the blocking pool from async contexts (D3 reads staged records).
pub fn refresh_d1_d3_gauges(router: &DataRouter) {
    publish_d1_gauges(&measure_d1(router));
    let d3 = measure_d3(router);
    METRICS
        .frag_d3_pressure_bytes
        .store(d3.pressure_bytes, Ordering::Relaxed);
}

/// The mount-lifetime gauge worker (§5.7 "also published continuously as
/// stats gauges"): D1 + D3 every 5 s on the blocking pool — the
/// health-worker cadence precedent (detached loop, dies with the
/// daemon). D2/D4 stay measure-priced and refresh via [`measure`].
pub fn spawn_gauge_worker(router: DataRouter) {
    crate::meta_exec::spawn_meta("defrag_gauge_worker", async move {
        let mut interval = squeezefs_ipc::sqz_time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let r = router.clone();
            squeezefs_ipc::sqz_blocking::run_blocking(move || refresh_d1_d3_gauges(&r)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// §5.8 offline duality — the report-only probe
// ---------------------------------------------------------------------------

/// Offline `--report-only`: read-only probes (refused loud under a live
/// writer — the fsck offline posture), a probe-shaped router with the
/// allocator census rebuilt (the D1 ground truth an unmounted set can
/// offer), then [`measure`]. D3 reads 0 by construction here: staged
/// custody is mount-owned (a clean unmount drains records to fold), and
/// the offline coordinator never adopts a mount's isolated staging dirs.
pub async fn run_offline_report(meta_lvs: &[String]) -> Result<DefragReport> {
    use std::path::Path;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline defrag --report-only refused: {e} — run `squeezefs defrag \
                     <mountpoint> --report-only` against the live mount instead"
                ))
            })?;
    }
    let routed = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    let result = offline_report_body(&routed, meta_lvs).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing probe after offline defrag report: {e}");
        }
    }
    result
}

async fn offline_report_body(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
) -> Result<DefragReport> {
    let router = build_offline_router(routed, meta_lvs).await?;
    measure(routed, &router).await
}

/// Offline defrag mover (§5.8 duality): the short-lived **D0-guarded
/// coordinator** — `format_preflight` live-client refusal per volume,
/// guarded open (the writer claims), the in-process fabric with the
/// router-only mover context, one job run to terminal. `DefragFold` is
/// refused at the CLI (fold custody is mount-owned; no fold hook exists
/// here by construction).
pub async fn run_offline_mover(
    meta_lvs: &[String],
    job_type: crate::jobs::JobType,
    throttle_pct: u32,
) -> Result<()> {
    use std::path::Path;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline defrag refused: {e} — run `squeezefs defrag <mountpoint>` \
                     against the live mount instead"
                ))
            })?;
    }
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result = offline_mover_body(&routed, meta_lvs, job_type, throttle_pct).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after offline defrag: {e}");
        }
    }
    result
}

async fn offline_mover_body(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
    job_type: crate::jobs::JobType,
    throttle_pct: u32,
) -> Result<()> {
    let router = build_offline_router(routed, meta_lvs).await?;
    let fabric = crate::jobs::JobFabric::start(
        routed.clone(),
        2,
        100,
        Some(crate::jobs::MoverCtx::router_only(router)),
    )
    .await?;
    let job_id = fabric
        .submit(crate::jobs::JobSpec {
            job_type,
            throttle_pct,
        })
        .await?;
    // Offline coordinators run to completion in-process (§5.8); the
    // bound is a belt — a kill-9'd run resumes by re-invocation (KD-6).
    let end = fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(7 * 24 * 3600))
        .await?;
    fabric.shutdown_abrupt().await;
    if end != crate::jobs::JobState::Completed {
        return Err(SqueezefsError::InvalidOperation(format!(
            "offline defrag job {job_id} ended {end:?} (see the log above)"
        )));
    }
    Ok(())
}

/// The offline data plane (the `offline_drain_body` / fsck
/// `run_offline_body` shape): mount-shaped router over the record set,
/// allocator refcount recovery as the census ground truth.
pub(crate) async fn build_offline_router(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
) -> Result<DataRouter> {
    let cfg = crate::config_ops::read_volume_format_config(meta_lvs).await?;
    let records = cfg.resolved_data_volumes();
    let dlm = crate::dlm::DlmClient::new()?;
    let live: Vec<&crate::DataVolumeRecord> = records
        .iter()
        .filter(|r| r.state != crate::VOL_STATE_RETIRED)
        .collect();
    let first = live.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("no live data volumes to defrag".to_string())
    })?;
    let first_alloc = Arc::new(crate::block_allocator::BlockAllocator::new(&first.id).await?);
    if let Ok(cap) = crate::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let first_dev = Arc::new(crate::nvme_dev::NvmeBlockDev::new(&first.backing_dev));
    let cache = crate::cache::TieredCache::new(
        Vec::new(), // never adopt/mutate a mount's isolated staging dirs
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await?;
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    // §3d.2 (rc-manifest): block size rides the router seams, never a
    // process-env write (see `config_ops::offline_drain_body` — the same
    // retired runtime channel). The passthrough `set_crypto` pins the
    // DUR-8e plaintext bound to this set's block size.
    router.set_block_size(cfg.block_size);
    router.set_crypto(crate::crypto_compress::CryptoCompressState::new(
        "none".to_string(),
        "none".to_string(),
        None,
    ));
    for rec in &live {
        router.backend_router.register_backend(rec).await?;
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(routed.clone());

    // Allocator refcount recovery — the D1/census ground truth.
    for kv in &routed.volumes {
        for entry in router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &router.backend_router)
                .await?;
        }
    }
    Ok(router)
}
