//! PR VL7 — online defragmenter, red-first
//! (docs/design-volume-lifecycle.md §5.7, KD-11, KD-12, gate G-VL-6):
//!
//! - **The four-axis model (KD-11)**: fragmentation IS the four measured
//!   axes — D1 free-space contiguity (per-volume largest-free-run /
//!   total-free over the allocated offset space `[0, highest)`, plus the
//!   reclaimable tail), D2 file locality (fraction of logically-adjacent
//!   block pairs whose physical offsets are same-backend ascending), D3
//!   staged-extent pressure (parked overlay bytes + spilled
//!   `active_block_ext:` record bytes), D4 meta node occupancy (dead
//!   records in the serialized bset log vs distinct live keys). Each
//!   axis has a gauge AND an independently invocable mover.
//! - **Movers REUSE existing machinery**: D1/D2 = the VL4 mover
//!   (`move_one` copy-then-republish CoW under the CURRENT fencing
//!   token) with a contiguity-aware destination pick; D3 = the W2 fold
//!   machinery kicked to completion; D4 = KV node compaction through
//!   the SMO serialization (`smo_replace` — never a new compactor).
//! - **G-VL-6 at cargo scale**: interleaved create/delete drives D1
//!   contiguity ≤ 0.3; `defrag --data` with concurrent churn reaches
//!   D1 ≥ 0.9 and reclaimable tail ≥ 90 % with ZERO corruption
//!   (checksummed manifest); D2 strictly improved on the streaming
//!   fixture, byte-identical; `--report-only` matches an independent
//!   census recomputed by this suite.
//! - **Fabric discipline inherited**: durable job records, duty-cycle
//!   throttle live-retunable, pause/cancel, kill-9 resume by plan
//!   regeneration (KD-6).
//!
//! RED: no `squeezefs::defrag` module, no `JobType::Defrag*`, no
//! `defrag_*`/`frag_*` metrics exist yet — every contract fails.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType, MoverCtx};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_RETIRED};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::TempDir;

const BLOCK: usize = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// Mount-shaped fixture (the VL4 drain-test shape) with the VL7 fold
/// hook wired into the mover context — the mount posture for defrag.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    _staging: TempDir,
}

async fn open_fixture_ext(meta: &Path, records: &[DataVolumeRecord], compressed: bool) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new().unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }

    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    if compressed {
        router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    }
    for rec in records {
        if rec.state == VOL_STATE_RETIRED {
            continue;
        }
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());

    for kv in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }

    let fs = Arc::new(fs);
    // The VL7 mount shape: quiescence probe + the D3 fold hook.
    let mover =
        MoverCtx::new(fs.router.clone(), fs.mover_quiesce_probe()).with_fold(fs.defrag_fold_hook());
    let fabric = JobFabric::start(routed.clone(), 2, 100, Some(mover))
        .await
        .expect("fabric start");
    fs.job_fabric.store(Arc::new(Some(fabric.clone())));

    Fx {
        fs,
        meta: routed,
        fabric,
        _staging: staging,
    }
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    open_fixture_ext(meta, records, false).await
}

impl Fx {
    async fn close(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    /// Kill-9 analog (the VL4 crash shape).
    async fn crash(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            let _ = vol.shutdown().await;
        }
    }
}

async fn create_file(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Striped file of `nblocks` distinct blocks, fsynced. Returns expected
/// content.
async fn striped_burst(fx: &Fx, ino: u64, nblocks: usize, tag: u8) -> Vec<u8> {
    let dummy = vec![0u8; BLOCK + 1];
    fx.fs
        .write(
            req(),
            ino,
            0,
            0,
            bytes::Bytes::copy_from_slice(&dummy),
            0,
            0,
        )
        .await
        .unwrap();
    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = vec![(b as u8) ^ tag; BLOCK];
        expected.extend_from_slice(&data);
        fx.fs
            .write(
                req(),
                ino,
                0,
                (b * BLOCK) as u64,
                bytes::Bytes::copy_from_slice(&data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write of block {b} failed: {e:?}"));
    }
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    expected
}

async fn read_back(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let reply = fx
            .fs
            .read(req(), ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read of block {b} failed: {e:?}"));
        out.extend_from_slice(&reply.data);
    }
    out
}

/// Unlink + reclaim: block frees are deferred to FORGET + the reclaim
/// worker pool (armed at FUSE INIT, which this harness never runs) —
/// drive the same batch entry point directly, exactly what the kernel's
/// forget would reach.
async fn unlink(fx: &Fx, name: &str, ino: u64) {
    // The create's open handle must release first (an open ino is
    // reclaim-exempt by design), like the kernel's close() before rm.
    let _ = fx.fs.release(req(), ino, 0, 0, 0, true).await;
    fx.fs
        .unlink(req(), 1, OsStr::new(name))
        .await
        .unwrap_or_else(|e| panic!("unlink {name} failed: {e:?}"));
    fx.fs.reclaim_orphaned_batch(vec![ino]).await;
}

/// Independent D1 recomputation (the G-VL-6 census-match instrument):
/// the same numbers from the allocator's raw surfaces with the suite's
/// OWN run-scan — never the engine's code path.
async fn independent_d1(
    alloc: &Arc<BlockAllocator>,
) -> (
    u64, /* free */
    u64, /* largest run */
    u64, /* tail free */
) {
    let mut free = alloc.get_free_blocks().await.unwrap();
    free.sort_unstable();
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
    // Tail free = the trailing free run ending at highest-1.
    let mut tail = 0u64;
    for &idx in free.iter().rev() {
        if idx == highest - tail - 1 {
            tail += 1;
        } else {
            break;
        }
    }
    (free_total, largest, tail)
}

// ---------------------------------------------------------------------------
// Ratio codec: gauges are permille+1 encoded (0 = never measured)
// ---------------------------------------------------------------------------

#[test]
fn test_ratio_codec_roundtrip() {
    use squeezefs::defrag::{decode_ratio, encode_ratio};
    assert_eq!(decode_ratio(0), None, "0 is the never-measured sentinel");
    for v in [0.0_f64, 0.001, 0.25, 0.3, 0.9, 0.999, 1.0] {
        let got = decode_ratio(encode_ratio(v)).expect("measured value decodes");
        assert!(
            (got - v).abs() < 0.001,
            "ratio {v} must survive the codec within 1 permille (got {got})"
        );
    }
    // Out-of-range inputs clamp, never wrap.
    assert_eq!(decode_ratio(encode_ratio(7.5)), Some(1.0));
    assert_eq!(decode_ratio(encode_ratio(-1.0)), Some(0.0));
}

// ---------------------------------------------------------------------------
// D1 measurement matches an independent census (G-VL-6 clause 3, unit)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_d1_measure_matches_independent_census() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let alloc = fx
        .fs
        .router
        .backend_router
        .backends
        .get("oss1")
        .unwrap()
        .value()
        .block_allocator
        .clone();

    // Interleaved alloc/free directly on the allocator: 64 blocks, free
    // every other one — the canonical D1 fragmentation shape.
    let mut offsets = Vec::new();
    for _ in 0..64 {
        offsets.push(alloc.allocate_block().await.unwrap());
    }
    for off in offsets.iter().skip(1).step_by(2) {
        alloc.free_block(*off).await.unwrap();
    }

    let report = squeezefs::defrag::measure_d1(&fx.fs.router);
    let row = report
        .iter()
        .find(|r| r.id == "oss1")
        .expect("a D1 row per registered volume");

    let (free, largest, tail) = independent_d1(&alloc).await;
    assert_eq!(row.free_blocks, free, "free census must match");
    assert_eq!(
        row.largest_free_run, largest,
        "largest-run census must match"
    );
    assert_eq!(row.tail_free_blocks, tail, "tail census must match");
    let want_contiguity = largest as f64 / free as f64;
    assert!(
        (row.contiguity - want_contiguity).abs() < 1e-9,
        "contiguity = largest_free_run / total_free (got {}, want {want_contiguity})",
        row.contiguity
    );
    // 32 isolated 1-block holes: contiguity 1/32, tail 1/32 (only the
    // last freed block touches the frontier).
    assert!(
        row.contiguity <= 0.05,
        "interleaved frees must fragment (got {})",
        row.contiguity
    );

    // Compact it by hand (free the lot) — contiguity/tail snap to 1.0.
    for off in offsets.iter().step_by(2) {
        alloc.free_block(*off).await.unwrap();
    }
    let report = squeezefs::defrag::measure_d1(&fx.fs.router);
    let row = report.iter().find(|r| r.id == "oss1").unwrap();
    assert!(
        (row.contiguity - 1.0).abs() < 1e-9 && (row.reclaimable_tail - 1.0).abs() < 1e-9,
        "an all-free space is fully contiguous and fully reclaimable \
         (contiguity {}, tail {})",
        row.contiguity,
        row.reclaimable_tail
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Report-only: full measure() matches independent recomputation and
// publishes the §10 gauges (G-VL-6 clause 3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_report_only_gauges_match_independent_census() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Dataset + interleaved deletes → measurable D1/D2 state.
    let mut inos = Vec::new();
    for i in 0..8 {
        let ino = create_file(&fx, &format!("f{i}.bin")).await;
        striped_burst(&fx, ino, 4, i as u8).await;
        inos.push(ino);
    }
    for i in (1..8).step_by(2) {
        unlink(&fx, &format!("f{i}.bin"), inos[i]).await;
    }
    // Settle the async block reclaimer (2026-07-27 machinery): unlink
    // frees ENQUEUE their device reclaim and the blocks rejoin the
    // allocator census only at finish_free — measuring mid-drain raced
    // the report against the independent census below (pre-existing
    // flake, 4/5 red at f16d5a9 on the loaded gate box: report saw
    // (3,2,0) free-run state, the census taken µs later saw (12,3,2)).
    fx.fs.router.backend_router.reclaim_drain().await;

    let report = squeezefs::defrag::measure(&fx.meta, &fx.fs.router)
        .await
        .expect("measure");

    // D1 rows match the independent census per volume.
    for row in &report.d1 {
        let alloc = fx
            .fs
            .router
            .backend_router
            .backends
            .get(&row.id)
            .unwrap_or_else(|| panic!("row for unregistered volume {}", row.id))
            .value()
            .block_allocator
            .clone();
        let (free, largest, tail) = independent_d1(&alloc).await;
        assert_eq!(
            (row.free_blocks, row.largest_free_run, row.tail_free_blocks),
            (free, largest, tail),
            "D1 census mismatch on {}",
            row.id
        );
    }

    // D2 matches an independent per-file recomputation from the durable
    // block maps.
    let mut want_pairs = 0u64;
    let mut want_local = 0u64;
    for &ino in inos.iter().step_by(2) {
        let m = fx
            .fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap();
        let Some(bm) = m.block_map.as_deref() else {
            continue;
        };
        let mut entries: Vec<(u32, String, u64)> = Vec::new();
        for (&b, mapping) in bm {
            let clean = squeezefs::routing::clean_block_key(mapping);
            let (be, off) = fx.fs.router.backend_router.parse_block_key(&clean).unwrap();
            entries.push((b, be, off));
        }
        entries.sort_by_key(|e| e.0);
        for w in entries.windows(2) {
            if w[1].0 == w[0].0 + 1 {
                want_pairs += 1;
                if w[1].1 == w[0].1 && w[1].2 > w[0].2 {
                    want_local += 1;
                }
            }
        }
    }
    assert_eq!(report.d2.pairs, want_pairs, "D2 pair census must match");
    assert_eq!(
        report.d2.local_pairs, want_local,
        "D2 local-pair census must match"
    );

    // The §10 gauges were published: worst-volume D1, global D2.
    use squeezefs::defrag::decode_ratio;
    let min_contig = report
        .d1
        .iter()
        .map(|r| r.contiguity)
        .fold(f64::INFINITY, f64::min);
    let g = decode_ratio(METRICS.frag_d1_contiguity.load(Ordering::Relaxed))
        .expect("frag_d1_contiguity published");
    assert!(
        (g - min_contig).abs() < 0.002,
        "frag_d1_contiguity gauge = worst volume (gauge {g}, report {min_contig})"
    );
    let g = decode_ratio(METRICS.frag_d2_locality.load(Ordering::Relaxed))
        .expect("frag_d2_locality published");
    assert!(
        (g - report.d2.locality).abs() < 0.002,
        "frag_d2_locality gauge mismatch (gauge {g}, report {})",
        report.d2.locality
    );
    assert_eq!(
        METRICS.frag_d3_pressure_bytes.load(Ordering::Relaxed),
        report.d3.pressure_bytes,
        "frag_d3_pressure_bytes gauge mismatch"
    );
    assert!(
        decode_ratio(METRICS.frag_d4_dead_bset_ratio.load(Ordering::Relaxed)).is_some(),
        "frag_d4_dead_bset_ratio published by measure()"
    );

    // The report serializes (the --report-only JSON body).
    let json = serde_json::to_value(&report).expect("report serializes");
    assert!(json["d1"].is_array() && json["d2"]["locality"].is_number());
    fx.close().await;
}

// ---------------------------------------------------------------------------
// G-VL-6: defrag --data on the synthetic-fragmentation volume with
// concurrent churn — D1 ≤ 0.3 → ≥ 0.9, tail ≥ 90 %, ZERO corruption
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_defrag_data_g_vl6_contiguity_tail_churn_zero_corruption() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 8 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Synthetic fragmentation: 24 × 4-block files, delete every other
    // one → ~12 isolated 4-block holes.
    const FILES: usize = 24;
    const FBLOCKS: usize = 4;
    let mut manifest: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut inos = Vec::new();
    for i in 0..FILES {
        let ino = create_file(&fx, &format!("frag{i}.bin")).await;
        let bytes = striped_burst(&fx, ino, FBLOCKS, i as u8).await;
        inos.push((i, ino, bytes));
    }
    for (i, ino, _) in &inos {
        if i % 2 == 1 {
            unlink(&fx, &format!("frag{i}.bin"), *ino).await;
        }
    }
    for (i, ino, bytes) in &inos {
        if i % 2 == 0 {
            manifest.insert(*ino, bytes.clone());
        }
    }

    // Wait for the delete reclaim to land in the allocator.
    let alloc = fx
        .fs
        .router
        .backend_router
        .backends
        .get("oss1")
        .unwrap()
        .value()
        .block_allocator
        .clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while alloc.free_blocks_count() < (FILES / 2 * FBLOCKS) as u64 {
        assert!(
            std::time::Instant::now() < deadline,
            "unlink reclaim did not free the deleted files' blocks \
             (free {})",
            alloc.free_blocks_count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Fixture validity: D1 contiguity ≤ 0.3 (the G-VL-6 precondition).
    let before = squeezefs::defrag::measure_d1(&fx.fs.router);
    let row = before.iter().find(|r| r.id == "oss1").unwrap();
    assert!(
        row.contiguity <= 0.3,
        "the synthetic fixture must fragment to ≤ 0.3 (got {})",
        row.contiguity
    );

    // Concurrent churn (the rig's dd/rm shape at cargo scale): create,
    // write, delete a churn file in a loop while the mover runs.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let fs = fx.fs.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let name = format!("churn{n}.bin");
                let ino = fs
                    .create(req(), 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                    .await
                    .unwrap()
                    .attr
                    .ino;
                let data = vec![n as u8; BLOCK * 2];
                let _ = fs
                    .write(req(), ino, 0, 0, bytes::Bytes::copy_from_slice(&data), 0, 0)
                    .await;
                let _ = fs.fsync(req(), ino, 0, false).await;
                let _ = fs.release(req(), ino, 0, 0, 0, true).await;
                let _ = fs.unlink(req(), 1, OsStr::new(&name)).await;
                fs.reclaim_orphaned_batch(vec![ino]).await;
                n += 1;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    };

    let moved_before = METRICS.defrag_blocks_moved.load(Ordering::Relaxed);
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss1".to_string()),
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag-data");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed, "defrag under churn completes");

    stop.store(true, Ordering::Relaxed);
    churn.await.unwrap();

    // Quiescent convergence pass (the churn's own frees are new work).
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss1".to_string()),
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit convergence pass");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);

    // G-VL-6: D1 ≥ 0.9 and reclaimable tail ≥ 90 %.
    let after = squeezefs::defrag::measure_d1(&fx.fs.router);
    let row = after.iter().find(|r| r.id == "oss1").unwrap();
    assert!(
        row.contiguity >= 0.9,
        "G-VL-6: D1 contiguity must reach ≥ 0.9 (got {})",
        row.contiguity
    );
    assert!(
        row.reclaimable_tail >= 0.9,
        "G-VL-6: reclaimable tail must reach ≥ 90 % (got {})",
        row.reclaimable_tail
    );
    assert!(
        METRICS.defrag_blocks_moved.load(Ordering::Relaxed) > moved_before,
        "engagement: defrag_blocks_moved must account for the compaction"
    );

    // ZERO corruption: every survivor byte-identical.
    for (ino, want) in &manifest {
        let got = read_back(&fx, *ino, FBLOCKS).await;
        assert_eq!(
            &got, want,
            "G-VL-6 zero-corruption: ino {ino} bytes must survive the defrag"
        );
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// G-VL-6: D2 locality strictly improved on the streaming fixture,
// byte-identical
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_d2_streaming_fixture_strictly_improves_bytes_identical() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // A streaming file whose blocks the 2-volume placement interleaves
    // across backends — the classic cross-backend locality hole.
    const NBLOCKS: usize = 16;
    let ino = create_file(&fx, "stream.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS, 0x3C).await;

    let report = squeezefs::defrag::measure(&fx.meta, &fx.fs.router)
        .await
        .unwrap();
    let before = report.d2.locality;
    assert!(
        before < 1.0,
        "the 2-volume placement must interleave the streaming fixture \
         (locality {before})"
    );

    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag-data");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);

    let report = squeezefs::defrag::measure(&fx.meta, &fx.fs.router)
        .await
        .unwrap();
    let after = report.d2.locality;
    assert!(
        after > before,
        "G-VL-6: D2 locality must STRICTLY improve ({before} → {after})"
    );
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "the streaming file must be byte-identical after the rewrite"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// D3: the fold kick drives parked/spilled extent custody down through
// the EXISTING fold machinery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_d3_fold_kick_drains_parked_bytes() {
    let _serial = serial().await;
    // High fold thresholds so the park stays parked until OUR kick.
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    // lz4 volume: every small overwrite is patch-ineligible ⇒ parks.
    let fx = open_fixture_ext(&meta, &recs, true).await;

    const NBLOCKS: usize = 6;
    let ino = create_file(&fx, "park.bin").await;
    let mut expected = striped_burst(&fx, ino, NBLOCKS, 0x00).await;

    // Small non-adjacent overwrite of a durable striped block: parks an
    // extent overlay (W2), never a whole-block buffer.
    let parked_before = METRICS.parked_extent_bytes.load(Ordering::Relaxed);
    let patch = vec![0x5Au8; 512];
    let off = BLOCK as u64 + 1024;
    expected[off as usize..off as usize + patch.len()].copy_from_slice(&patch);
    fx.fs
        .write(
            req(),
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(&patch),
            0,
            0,
        )
        .await
        .unwrap();
    let parked = METRICS.parked_extent_bytes.load(Ordering::Relaxed);
    assert!(
        parked > parked_before,
        "the small overwrite must park an extent overlay \
         ({parked_before} → {parked})"
    );

    // The D3 gauge sees the pressure.
    let d3 = squeezefs::defrag::measure_d3(&fx.fs.router);
    assert!(
        d3.pressure_bytes >= parked - parked_before,
        "frag_d3 pressure must cover the parked bytes (pressure {}, parked {})",
        d3.pressure_bytes,
        parked - parked_before
    );

    // Kick the fold through the fabric (JobType::DefragFold).
    let folds_before = METRICS.defrag_folds_kicked.load(Ordering::Relaxed);
    let passes_before = METRICS.fold_passes.load(Ordering::Relaxed);
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragFold,
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag-fold");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(60))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);

    assert!(
        METRICS.defrag_folds_kicked.load(Ordering::Relaxed) > folds_before,
        "defrag_folds_kicked must count the kick"
    );
    assert!(
        METRICS.fold_passes.load(Ordering::Relaxed) > passes_before,
        "the kick must DRIVE the existing fold machinery (fold_passes), \
         never reimplement it"
    );
    assert!(
        METRICS.parked_extent_bytes.load(Ordering::Relaxed) <= parked_before,
        "the fold must drain the parked custody (parked {} after kick)",
        METRICS.parked_extent_bytes.load(Ordering::Relaxed)
    );

    // Never-lossy: the composed bytes survive the fold.
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "folded content must compose the patch over the base"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// D4: dead-bset census + the compaction nudge through the SMO
// serialization (never a new compactor)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_d4_meta_nudge_increments_compactions() {
    use squeezefs::meta_backend::Metadata as _;
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 1 << 30);
    format_meta(&meta, &[&oss1]).await;

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
        .await
        .expect("open v3");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));

    // Dead-bset-heavy fixture: rewrite the same keys across checkpoint
    // boundaries — every checkpoint freezes+appends one bset whose
    // records supersede the previous bset's.
    for round in 0u8..6 {
        for k in 0..8 {
            routed
                .setxattr(1, &format!("user.dead{k}"), &vec![round; 512])
                .await
                .expect("setxattr");
        }
        routed.volumes[0]
            .checkpoint_now()
            .await
            .expect("checkpoint");
    }

    let census = routed.volumes[0].dead_bset_census();
    assert!(
        census.records_indexed > census.records_live,
        "the fixture must accumulate dead records \
         (indexed {}, live {})",
        census.records_indexed,
        census.records_live
    );
    assert!(
        !census.candidates.is_empty(),
        "dead-heavy leaves must be nudge candidates"
    );
    let ratio_before = 1.0 - census.records_live as f64 / census.records_indexed.max(1) as f64;

    // The nudge: compaction THROUGH the SMO serialization.
    let compactions_before =
        squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed);
    let kicked = routed.volumes[0]
        .defrag_compact_nodes(&census.candidates)
        .await
        .expect("nudge");
    assert!(kicked >= 1, "at least one node must compact");
    assert!(
        squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
            > compactions_before,
        "the nudge must ride the EXISTING compactor (meta_kv_node_compactions)"
    );

    // The census improves and the data still reads.
    let census = routed.volumes[0].dead_bset_census();
    let ratio_after = 1.0 - census.records_live as f64 / census.records_indexed.max(1) as f64;
    assert!(
        ratio_after < ratio_before,
        "dead-bset ratio must improve ({ratio_before} → {ratio_after})"
    );
    for k in 0..8 {
        let v = routed
            .getxattr(1, &format!("user.dead{k}"))
            .await
            .unwrap()
            .expect("xattr survives compaction");
        assert_eq!(v, vec![5u8; 512], "the NEWEST value survives");
    }

    // Idempotence: a clean re-nudge is a no-op (verified state, not
    // blind rewrites).
    let census2 = routed.volumes[0].dead_bset_census();
    if census2.candidates.is_empty() {
        let kicked2 = routed.volumes[0]
            .defrag_compact_nodes(&census2.candidates)
            .await
            .expect("re-nudge");
        assert_eq!(kicked2, 0, "nothing dead ⇒ nothing compacted");
    }

    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

/// The fabric spelling of the same nudge: `JobType::DefragMeta` completes
/// and counts `defrag_meta_compactions_kicked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_defrag_meta_job_counts_kicks() {
    use squeezefs::meta_backend::Metadata as _;
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 1 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    for round in 0u8..6 {
        for k in 0..8 {
            fx.meta
                .setxattr(1, &format!("user.dead{k}"), &vec![round; 512])
                .await
                .expect("setxattr");
        }
        fx.meta.volumes[0]
            .checkpoint_now()
            .await
            .expect("checkpoint");
    }
    let kicked_before = METRICS
        .defrag_meta_compactions_kicked
        .load(Ordering::Relaxed);

    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragMeta,
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag-meta");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(60))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed);
    assert!(
        METRICS
            .defrag_meta_compactions_kicked
            .load(Ordering::Relaxed)
            > kicked_before,
        "defrag_meta_compactions_kicked must count the job's nudges"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Fabric discipline: throttle live-retunes; pause/cancel honored
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_defrag_throttle_live_retune_and_pause() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 8 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let mut t_inos = Vec::new();
    for i in 0..12 {
        let ino = create_file(&fx, &format!("t{i}.bin")).await;
        striped_burst(&fx, ino, 4, i as u8).await;
        t_inos.push(ino);
    }
    for i in (1..12).step_by(2) {
        unlink(&fx, &format!("t{i}.bin"), t_inos[i]).await;
    }
    let alloc = fx
        .fs
        .router
        .backend_router
        .backends
        .get("oss1")
        .unwrap()
        .value()
        .block_allocator
        .clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while alloc.free_blocks_count() < 6 {
        assert!(std::time::Instant::now() < deadline, "reclaim stalled");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Throttle 1 %: the duty cycle stretches the job (KD-3) — it must
    // still be live shortly after submit.
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss1".to_string()),
            },
            throttle_pct: 1,
        })
        .await
        .expect("submit");
    // TEST-3: the observable is "the fabric has a status record for the
    // submitted job"; poll for it rather than sleeping 300 ms.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let st = loop {
        if let Some(st) = fx.fabric.status(&job_id).await.unwrap() {
            break st;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the submitted defrag never produced a status record"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    };
    assert_eq!(st.throttle_pct, 1, "the submitted throttle is recorded");

    // Pause parks it durably; resume + live rethrottle to 100 converges.
    fx.fabric.pause(&job_id).await.expect("pause");
    let st = fx.fabric.status(&job_id).await.unwrap().unwrap();
    assert!(
        matches!(st.state, JobState::Paused | JobState::Completed),
        "pause must park a live defrag (got {:?})",
        st.state
    );
    fx.fabric.resume(&job_id).await.expect("resume");
    fx.fabric.throttle(&job_id, 100).await.expect("rethrottle");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("terminal");
    assert_eq!(end, JobState::Completed, "rethrottled defrag completes");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// KD-6: kill-9 mid-defrag — re-run converges, manifest intact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_kill9_mid_defrag_resume_converges_manifest_intact() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 8 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const FILES: usize = 12;
    const FBLOCKS: usize = 4;
    let mut manifest: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut k_inos = Vec::new();
    for i in 0..FILES {
        let ino = create_file(&fx, &format!("k{i}.bin")).await;
        let bytes = striped_burst(&fx, ino, FBLOCKS, i as u8).await;
        if i % 2 == 0 {
            manifest.insert(ino, bytes);
        }
        k_inos.push(ino);
    }
    for i in (1..FILES).step_by(2) {
        unlink(&fx, &format!("k{i}.bin"), k_inos[i]).await;
    }
    let alloc = fx
        .fs
        .router
        .backend_router
        .backends
        .get("oss1")
        .unwrap()
        .value()
        .block_allocator
        .clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while alloc.free_blocks_count() < (FILES / 2 * FBLOCKS) as u64 {
        assert!(std::time::Instant::now() < deadline, "reclaim stalled");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Park the mover at its first pre-publish window, then crash.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let fired = fired.clone();
        squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
            if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = hit_tx.send(());
                // KEPT sleep — TEST-3 class "hang simulator": the mover is
                // deliberately parked forever-in-practice so the crash lands
                // mid-publish. It costs no test wall clock (the test proceeds
                // on `hit_rx`), and a poll cannot express "never proceed".
                std::thread::sleep(std::time::Duration::from_secs(120)); // parked "mid-publish"
            }
        }));
    }
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss1".to_string()),
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    hit_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the mover must reach the parked publish");
    fx.crash().await; // kill-9 analog: durable record stays non-terminal
    squeezefs::jobs::clear_evacuate_pre_publish_hook();

    // Reopen: the fabric ADOPTS the durable defrag job (KD-6 plan
    // regeneration) and converges without operator input.
    let fx = open_fixture(&meta, &recs).await;
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("the adopted job must converge");
    assert_eq!(end, JobState::Completed, "re-run converges (KD-6)");

    let after = squeezefs::defrag::measure_d1(&fx.fs.router);
    let row = after.iter().find(|r| r.id == "oss1").unwrap();
    assert!(
        row.contiguity >= 0.9 && row.reclaimable_tail >= 0.9,
        "the resumed defrag must still converge (contiguity {}, tail {})",
        row.contiguity,
        row.reclaimable_tail
    );
    for (ino, want) in &manifest {
        assert_eq!(
            &read_back(&fx, *ino, FBLOCKS).await,
            want,
            "manifest intact across the crash (ino {ino})"
        );
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.8 offline duality: --report-only against an unmounted set
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_offline_report_only_probe() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 1 << 30);
    format_meta(&meta, &[&oss1]).await;

    // Put some durable content on the set (the drain-fixture shape),
    // then close it.
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let ino = create_file(&fx, "data.bin").await;
    striped_burst(&fx, ino, 8, 0x11).await;
    fx.close().await;

    let report = squeezefs::defrag::run_offline_report(&[meta.display().to_string()])
        .await
        .expect("offline report-only");
    let row = report
        .d1
        .iter()
        .find(|r| r.id == "oss1")
        .expect("offline D1 row for the data volume");
    assert!(
        row.space_blocks > 0,
        "the offline census must rebuild the allocator ground truth"
    );
    assert!(
        report.d2.pairs > 0,
        "the offline walk must see the striped file's adjacency pairs"
    );
    // D4 rows exist per meta volume.
    assert_eq!(report.d4.len(), 1, "one D4 row per meta volume");
}
