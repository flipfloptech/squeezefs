//! W-5 `perf/fsync-economy` — e2e perf audit row 15 (write ledger #9):
//! `fsync` flushed EVERY data namespace and ran its meta legs serialized,
//! with no `fsync_phase_ns` to say where an fsync's time went.
//!
//! Contracts (red-first):
//!
//! 1. **The instrument** — `fsync_phase_ns` is always-on and EXACT-SUM:
//!    `intent_barrier + data_flush + staged_promote + data_barrier +
//!    meta_publish + meta_barrier + extent_barrier ≡ total` to the ns,
//!    every phase's count ≡ total's count, rendered through the one
//!    histogram shape on the stats inode beside the `fsync_*` counters.
//! 2. **Touched-namespace flush** — an ino whose blocks all live on volume
//!    A never barriers volume B (per-device barrier epochs are the
//!    witness); a clean fsync is a counted DATA no-op (zero device
//!    barriers, the meta barrier still runs); an in-place W1 patch and an
//!    in-place full-block overwrite stamp their namespace (the two DMA
//!    shapes that change no block-map key); write-through namespaces
//!    skip the barrier entirely.
//! 3. **Parallel legs** — with a seamed barrier latency on both devices
//!    the `data_barrier` phase reads ≈ max(legs), not Σ; lever off reads
//!    Σ.
//! 4. **Lever off = the shipped shape** — every namespace barriers on
//!    every fsync.
//! 5. **The meta barrier under a storm coalesces** — N concurrent fsyncs
//!    request N meta barriers and the coalescer issues far fewer.
//! 6. **Durability ordering is untouched** — a faulted data barrier fails
//!    the fsync and the meta barrier never runs (the DUR-1 leg, restated
//!    on the new ladder).
//! 7. **The staged-promotion lever** (`SQUEEZEFS_FSYNC_PROMOTE_STAGED`,
//!    default off) — on, the fsync of a staged-layout file promotes it
//!    exactly once in its own `staged_promote` leg; an entry a racing
//!    promoter already released is a counted no-op, never an error; off,
//!    the entry stays resident and no gauge moves.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsync_economy::{self, FsyncPhase};
use squeezefs::fuse_client::{set_patch_max_bytes, SqueezefsFilesystem, METRICS, STATS_INODE};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::write_cache::WriteCacheClass;
use squeezefs::{DataVolumeRecord, FormatConfig};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

/// Downscaled block: the striped write path with cheap images.
const BS: u64 = 64 * 1024;
const LBA: usize = 4096;

/// The phase family and the counters are process-global: serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Lever seams return to the knob on drop, so one test's posture never
/// leaks into the next.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        fsync_economy::test_set_touched_namespaces(None);
        fsync_economy::test_set_parallel_legs(None);
        fsync_economy::test_set_promote_staged(None);
        squeezefs::routing::set_inline_max_bytes_override(None);
        squeezefs::fuse_client::set_inplace_overwrite(false);
        squeezefs::dev_power_cut::clear_faults();
    }
}

fn make_dev_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BS,
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

struct Fx {
    fs: SqueezefsFilesystem,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    meta_path: PathBuf,
    /// `(record id, backing path)` per data volume, in record order.
    vols: Vec<(String, PathBuf)>,
    _dir: TempDir,
    _staging: TempDir,
}

/// Sparse backing size per data volume. The allocator chunk is 4 MiB
/// whatever the block size, so every 64 KiB block here costs a chunk of
/// capacity: 4 GiB = 1,024 blocks per volume (the rows write 384).
const DEV_BYTES: u64 = 4 << 30;

/// Mount-shaped fixture with `n` data volumes (the placement_tests shape):
/// records registered, the first record's device is the default slot.
/// The checkpoint timer is parked so no background tick lands in the
/// `meta_device_syncs` funnel the contracts read.
async fn open_fx(n: usize, tag: &str) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "3600000");
    set_patch_max_bytes(512 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let meta = make_dev_file(dir.path(), &format!("meta-{tag}"), 256 * 1024 * 1024);
    let paths: Vec<PathBuf> = (0..n)
        .map(|i| make_dev_file(dir.path(), &format!("oss{}-{tag}", i + 1), DEV_BYTES))
        .collect();
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    format_meta(&meta, &refs).await;
    let records: Vec<DataVolumeRecord> = base_format_config(&refs).resolved_data_volumes();

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
        Some("16MB"),
        Some("64MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in &records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.clone());

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    Fx {
        fs,
        meta: routed,
        meta_path: meta,
        vols: records
            .iter()
            .map(|r| (r.id.clone(), PathBuf::from(&r.backing_dev)))
            .collect(),
        _dir: dir,
        _staging: staging,
    }
}

impl Fx {
    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    /// Force every NEW placement onto volume `idx` by disabling the others.
    fn place_only_on(&self, idx: usize) {
        for (i, (id, _)) in self.vols.iter().enumerate() {
            self.fs
                .router
                .backend_router
                .set_health_override(id, i != idx)
                .unwrap();
        }
    }

    fn dev_path(&self, idx: usize) -> String {
        self.vols[idx].1.display().to_string()
    }
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

async fn create(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(fx: &Fx, ino: u64, off: u64, data: &[u8]) {
    let written = fx
        .fs
        .write(
            req(),
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
        .written;
    assert_eq!(written as usize, data.len(), "short write at {off}");
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

async fn fsync(fx: &Fx, ino: u64) {
    fx.fs
        .fsync(req(), ino, 0, false)
        .await
        .unwrap_or_else(|e| panic!("fsync ino {ino} failed: {e:?}"));
}

/// A durable striped file of `blocks` blocks, every block on volume `vol`.
async fn durable_striped_on(fx: &Fx, name: &str, blocks: u64, vol: usize, tag: u8) -> u64 {
    fx.place_only_on(vol);
    let ino = create(fx, name).await;
    write_at(fx, ino, 0, &pattern((blocks * BS) as usize, tag)).await;
    fsync(fx, ino).await;
    assert!(
        fx.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fixture pipeline must drain"
    );
    let path = squeezefs::keys::inode_path(ino);
    let m = fx.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture premise: striped");
    ino
}

/// Append one FRESH full block at index `block` of a striped file, placed
/// on volume `vol` (a fresh block's destination is the placement pick;
/// a staged-layout file would place at promotion instead, which is why
/// the fixtures are striped first). Drains the detached upload.
async fn append_block_on(fx: &Fx, ino: u64, block: u64, vol: usize, tag: u8) {
    fx.place_only_on(vol);
    write_at(fx, ino, block * BS, &pattern(BS as usize, tag)).await;
    assert!(
        fx.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "append pipeline must drain"
    );
}

fn hist_count(h: &serde_json::Value) -> u64 {
    h["count"].as_u64().expect("count")
}

fn hist_sum_ns(h: &serde_json::Value) -> u64 {
    h["sum_ns"].as_u64().expect("sum_ns")
}

#[derive(Clone, Debug)]
struct PhaseWords {
    count: Vec<u64>,
    sum_ns: Vec<u64>,
}

fn phase_words() -> PhaseWords {
    let fam = fsync_economy::fsync_phase_json();
    let names = fsync_economy::FSYNC_PHASE_NAMES;
    PhaseWords {
        count: names.iter().map(|n| hist_count(&fam[*n])).collect(),
        sum_ns: names.iter().map(|n| hist_sum_ns(&fam[*n])).collect(),
    }
}

fn barrier_epoch(path: &str) -> u64 {
    squeezefs::dev_power_cut::barrier_epoch(path)
}

fn metric(c: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    c.load(Ordering::Relaxed)
}

/// The W-6 per-thread-striped counters (`patch_writes` since the WRITE
/// handler's per-op words went core-local) fold on read.
fn striped(c: &squeezefs::fuse_client::ShardedAtomic) -> u64 {
    c.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// 1 — the instrument
// ---------------------------------------------------------------------------

/// The family is exact-sum per fsync, every phase's count moves with
/// `total`'s, and the stats inode carries the family and the counters
/// ungated. `FSYNC_PHASE_NAMES` is the export's order contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_phase_family_is_exact_sum_and_rides_the_stats_inode() {
    let _g = serial().await;
    let _l = LeverGuard;
    assert_eq!(
        fsync_economy::FSYNC_PHASE_NAMES,
        [
            "intent_barrier",
            "data_flush",
            "staged_promote",
            "data_barrier",
            "meta_publish",
            "meta_barrier",
            "extent_barrier",
            "total",
        ]
    );
    assert_eq!(FsyncPhase::Total as usize, 7);
    let fx = open_fx(1, "phase").await;
    let ino = create(&fx, "phase.bin").await;
    write_at(&fx, ino, 0, &pattern((2 * BS) as usize, 0x11)).await;

    let before = phase_words();
    let calls0 = metric(&METRICS.fsync_calls);
    fsync(&fx, ino).await;
    let after = phase_words();

    let total = FsyncPhase::Total as usize;
    assert_eq!(
        after.count[total] - before.count[total],
        1,
        "one fsync = one total sample"
    );
    assert_eq!(metric(&METRICS.fsync_calls) - calls0, 1);
    let mut leg_sum = 0u64;
    for (i, name) in fsync_economy::FSYNC_PHASE_NAMES.iter().enumerate() {
        assert_eq!(
            after.count[i] - before.count[i],
            1,
            "{name}: every phase records once per fsync (0 ns for an absent leg)"
        );
        if i != total {
            leg_sum += after.sum_ns[i] - before.sum_ns[i];
        }
    }
    assert_eq!(
        leg_sum,
        after.sum_ns[total] - before.sum_ns[total],
        "Σ legs ≡ total to the ns (shared boundary instants)"
    );
    // A written file's fsync moves the data legs and the meta barrier.
    let df = FsyncPhase::DataFlush as usize;
    let mb = FsyncPhase::MetaBarrier as usize;
    assert!(after.sum_ns[df] > before.sum_ns[df], "data_flush moved");
    assert!(after.sum_ns[mb] > before.sum_ns[mb], "meta_barrier moved");

    // The stats inode carries the family and the counters.
    let reply = fx
        .fs
        .read(req(), STATS_INODE, 0, 0, 1 << 22, 0)
        .await
        .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let fam = &stats["metrics"]["fsync_phase_ns"];
    assert!(fam.is_object(), "fsync_phase_ns rides the stats inode");
    for name in fsync_economy::FSYNC_PHASE_NAMES {
        let h = &fam[name];
        for k in ["buckets", "count", "sum_ns", "mean_ns"] {
            assert!(h.get(k).is_some(), "fsync_phase_ns.{name} lacks {k}");
        }
    }
    for key in [
        "fsync_calls",
        "fsync_noop_clean",
        "fsync_data_namespaces_touched",
        "fsync_data_namespaces_flushed",
        "fsync_write_through_skips",
        "fsync_touched_unresolved",
        "fsync_parallel_joins",
    ] {
        assert!(
            stats["metrics"][key].is_u64(),
            "stats inode must carry {key}"
        );
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 2 — touched-namespace flush
// ---------------------------------------------------------------------------

/// An ino whose blocks all landed on volume A barriers A and never B; an
/// ino on B barriers B and never A. The per-device barrier epoch is the
/// witness, and the ratio counters read 1 flushed of 2 present per fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_barriers_only_the_namespaces_the_ino_wrote() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    let fx = open_fx(2, "touched").await;
    let (a, b) = (fx.dev_path(0), fx.dev_path(1));
    squeezefs::dev_power_cut::arm_power_cut(&a);
    squeezefs::dev_power_cut::arm_power_cut(&b);

    // Fresh files, one per volume: the fixture's own fsync barriers land
    // before the deltas below are taken.
    let ino_a = durable_striped_on(&fx, "on-a.bin", 2, 0, 0xA1).await;
    let ino_b = durable_striped_on(&fx, "on-b.bin", 2, 1, 0xB2).await;

    // New blocks on A only, then fsync(A): B's barrier epoch must not move.
    fx.place_only_on(0);
    write_at(&fx, ino_a, 2 * BS, &pattern(BS as usize, 0xA3)).await;
    let (ea0, eb0) = (barrier_epoch(&a), barrier_epoch(&b));
    let (t0, f0) = (
        metric(&METRICS.fsync_data_namespaces_touched),
        metric(&METRICS.fsync_data_namespaces_flushed),
    );
    fsync(&fx, ino_a).await;
    assert_eq!(barrier_epoch(&a) - ea0, 1, "A barriered once for ino_a");
    assert_eq!(barrier_epoch(&b) - eb0, 0, "B never barriered for ino_a");
    assert_eq!(metric(&METRICS.fsync_data_namespaces_touched) - t0, 1);
    assert_eq!(metric(&METRICS.fsync_data_namespaces_flushed) - f0, 1);
    assert_eq!(
        fx.fs.router.backend_router.data_volume_write_caches().len(),
        2,
        "two namespaces present — one flushed"
    );

    // New blocks on B only, then fsync(B): A's epoch must not move.
    fx.place_only_on(1);
    write_at(&fx, ino_b, 2 * BS, &pattern(BS as usize, 0xB4)).await;
    let (ea1, eb1) = (barrier_epoch(&a), barrier_epoch(&b));
    fsync(&fx, ino_b).await;
    assert_eq!(barrier_epoch(&a) - ea1, 0, "A never barriered for ino_b");
    assert_eq!(barrier_epoch(&b) - eb1, 1, "B barriered once for ino_b");

    // A file whose NEW blocks span both volumes barriers both.
    let ino_ab = durable_striped_on(&fx, "on-ab.bin", 2, 0, 0xC0).await;
    append_block_on(&fx, ino_ab, 2, 0, 0xC1).await;
    append_block_on(&fx, ino_ab, 3, 1, 0xC2).await;
    let (ea2, eb2) = (barrier_epoch(&a), barrier_epoch(&b));
    fsync(&fx, ino_ab).await;
    assert_eq!(
        barrier_epoch(&a) - ea2,
        1,
        "A barriered for the spanning ino"
    );
    assert_eq!(
        barrier_epoch(&b) - eb2,
        1,
        "B barriered for the spanning ino"
    );
    assert_eq!(
        metric(&METRICS.fsync_touched_unresolved),
        0,
        "every stamp resolved to its device — no all-namespace fallback"
    );
    fx.close().await;
}

/// A clean fsync (nothing written since the last barrier) issues ZERO
/// data-device barriers and is counted; the meta barrier still runs (a
/// create/setattr may be un-barriered — POSIX's fsync covers the inode).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_fsync_is_a_counted_data_noop_and_keeps_the_meta_barrier() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    let fx = open_fx(2, "clean").await;
    let ino = durable_striped_on(&fx, "clean.bin", 2, 0, 0x33).await;

    let d0 = metric(&METRICS.data_device_sync_requests);
    let n0 = metric(&METRICS.fsync_noop_clean);
    let m0 = metric(&METRICS.meta_sync_requests);
    fsync(&fx, ino).await;
    assert_eq!(
        metric(&METRICS.data_device_sync_requests) - d0,
        0,
        "a clean fsync requests no data-device barrier"
    );
    assert_eq!(metric(&METRICS.fsync_noop_clean) - n0, 1);
    assert_eq!(
        metric(&METRICS.meta_sync_requests) - m0,
        1,
        "the meta barrier is still requested (coalesced) — fsync covers the inode"
    );
    fx.close().await;
}

/// The two DMA shapes that change NO block-map key — the W1 sole-owner
/// patch and the in-place full-block overwrite — stamp their namespace,
/// so the fsync after them barriers it (never a counted no-op).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_place_dma_shapes_stamp_their_namespace() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    let fx = open_fx(2, "inplace").await;
    let a = fx.dev_path(0);
    squeezefs::dev_power_cut::arm_power_cut(&a);
    let ino = durable_striped_on(&fx, "patch.bin", 2, 0, 0x44).await;
    fsync(&fx, ino).await; // settle: the next fsync starts clean

    // W1 patch: one aligned LBA overwrite inside block 1.
    let p0 = striped(&METRICS.patch_writes);
    write_at(&fx, ino, BS + 8192, &pattern(LBA, 0x45)).await;
    assert_eq!(
        striped(&METRICS.patch_writes) - p0,
        1,
        "premise: the aligned overwrite rode the W1 patch"
    );
    let (e0, n0) = (barrier_epoch(&a), metric(&METRICS.fsync_noop_clean));
    fsync(&fx, ino).await;
    assert_eq!(barrier_epoch(&a) - e0, 1, "the patched namespace barriers");
    assert_eq!(metric(&METRICS.fsync_noop_clean) - n0, 0);

    // In-place full-block overwrite (the rewrite-wall lever): a TWO-block
    // rewrite, so neither the single-block patch nor the single-block
    // overlay arm owns the request — it accumulates and writes through.
    squeezefs::fuse_client::set_inplace_overwrite(true);
    let ip0 = metric(&METRICS.write_through_inplace_overwrites);
    write_at(&fx, ino, 0, &pattern(2 * BS as usize, 0x46)).await;
    assert!(fx.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
    assert_eq!(
        metric(&METRICS.write_through_inplace_overwrites) - ip0,
        2,
        "premise: both full-block overwrites landed in place"
    );
    let (e1, n1) = (barrier_epoch(&a), metric(&METRICS.fsync_noop_clean));
    fsync(&fx, ino).await;
    assert_eq!(barrier_epoch(&a) - e1, 1, "the in-place namespace barriers");
    assert_eq!(metric(&METRICS.fsync_noop_clean) - n1, 0);
    fx.close().await;
}

/// A write-through namespace needs no barrier: acknowledged writes are
/// power-safe on completion, so the fsync skips its device Fsync (counted)
/// while the data is published exactly as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_through_namespaces_skip_the_barrier() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    let fx = open_fx(1, "wt").await;
    let ino = durable_striped_on(&fx, "wt.bin", 2, 0, 0x55).await;
    let dev = fx.fs.router.backend_router.default_device.clone();
    assert_eq!(dev.write_cache(), WriteCacheClass::FileBacked);
    dev.override_write_cache_for_test(Some(WriteCacheClass::WriteThrough));

    write_at(&fx, ino, 2 * BS, &pattern(BS as usize, 0x56)).await;
    let (d0, s0, n0) = (
        metric(&METRICS.data_device_sync_requests),
        metric(&METRICS.fsync_write_through_skips),
        metric(&METRICS.fsync_noop_clean),
    );
    fsync(&fx, ino).await;
    assert_eq!(
        metric(&METRICS.data_device_sync_requests) - d0,
        0,
        "write-through: no device barrier requested"
    );
    assert_eq!(metric(&METRICS.fsync_write_through_skips) - s0, 1);
    assert_eq!(
        metric(&METRICS.fsync_noop_clean) - n0,
        0,
        "a skipped write-through namespace is not a clean no-op"
    );
    let path = squeezefs::keys::inode_path(ino);
    let m = fx.fs.router.fetch_metadata(&path).await.unwrap();
    assert!(
        m.block_map.as_ref().is_some_and(|bm| bm.contains_key(&2)),
        "the block is published regardless"
    );
    dev.override_write_cache_for_test(None);
    assert_eq!(dev.write_cache(), WriteCacheClass::FileBacked);
    fx.close().await;
}

/// Lever off = the shipped shape: every namespace barriers on every fsync,
/// whichever volume the ino's blocks live on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_barriers_every_namespace() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(false));
    let fx = open_fx(2, "leveroff").await;
    let (a, b) = (fx.dev_path(0), fx.dev_path(1));
    squeezefs::dev_power_cut::arm_power_cut(&a);
    squeezefs::dev_power_cut::arm_power_cut(&b);
    let ino = durable_striped_on(&fx, "off.bin", 2, 0, 0x66).await;
    write_at(&fx, ino, 2 * BS, &pattern(BS as usize, 0x67)).await;
    let (ea, eb) = (barrier_epoch(&a), barrier_epoch(&b));
    let n0 = metric(&METRICS.fsync_noop_clean);
    fsync(&fx, ino).await;
    assert_eq!(barrier_epoch(&a) - ea, 1);
    assert_eq!(barrier_epoch(&b) - eb, 1, "shipped shape: B barriers too");
    // And a clean fsync still barriers everything — never a no-op.
    let (ea, eb) = (barrier_epoch(&a), barrier_epoch(&b));
    fsync(&fx, ino).await;
    assert_eq!(barrier_epoch(&a) - ea, 1);
    assert_eq!(barrier_epoch(&b) - eb, 1);
    assert_eq!(metric(&METRICS.fsync_noop_clean) - n0, 0);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 3 — parallel legs
// ---------------------------------------------------------------------------

/// Two namespaces, each with a seamed 200 ms barrier: the parallel posture
/// pays ≈ one latency in `data_barrier`, the serialized posture pays both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_legs_pay_max_not_sum() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    let fx = open_fx(2, "parallel").await;
    let (a, b) = (fx.dev_path(0), fx.dev_path(1));
    let lat = Duration::from_millis(200);

    // Two fresh blocks, one per volume: both namespaces touched.
    async fn spanning_write(fx: &Fx, ino: u64, first_block: u64, tag: u8) {
        append_block_on(fx, ino, first_block, 0, tag).await;
        append_block_on(fx, ino, first_block + 1, 1, tag ^ 0xFF).await;
    }
    let ino = durable_striped_on(&fx, "span.bin", 2, 0, 0x70).await;
    spanning_write(&fx, ino, 2, 0x71).await;
    squeezefs::dev_power_cut::arm_barrier_latency(&a, lat);
    squeezefs::dev_power_cut::arm_barrier_latency(&b, lat);
    let db = FsyncPhase::DataBarrier as usize;

    fsync_economy::test_set_parallel_legs(Some(true));
    let before = phase_words();
    let j0 = metric(&METRICS.fsync_parallel_joins);
    fsync(&fx, ino).await;
    let after = phase_words();
    let parallel_ns = after.sum_ns[db] - before.sum_ns[db];
    assert_eq!(metric(&METRICS.fsync_parallel_joins) - j0, 1);
    assert!(
        parallel_ns >= lat.as_nanos() as u64,
        "the barrier step waits for BOTH legs (≥ one latency): {parallel_ns} ns"
    );
    assert!(
        parallel_ns < (lat.as_nanos() as u64) * 3 / 2,
        "parallel legs: data_barrier ≈ max(legs), got {parallel_ns} ns vs 200 ms legs"
    );

    // Re-dirty both namespaces; serialized posture pays Σ.
    spanning_write(&fx, ino, 4, 0x72).await;
    fsync_economy::test_set_parallel_legs(Some(false));
    let before = phase_words();
    let j1 = metric(&METRICS.fsync_parallel_joins);
    fsync(&fx, ino).await;
    let after = phase_words();
    let serial_ns = after.sum_ns[db] - before.sum_ns[db];
    assert_eq!(metric(&METRICS.fsync_parallel_joins) - j1, 0);
    assert!(
        serial_ns >= 2 * lat.as_nanos() as u64,
        "serialized legs: data_barrier ≥ Σ legs, got {serial_ns} ns"
    );
    squeezefs::dev_power_cut::clear_faults();
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 5 — the meta barrier under a storm
// ---------------------------------------------------------------------------

/// N concurrent small-file fsyncs each REQUEST a meta barrier; the
/// coalescer issues far fewer (group commit of barriers), with the data
/// legs touching only each file's namespace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_fsync_storm_coalesces_its_meta_barriers() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    fsync_economy::test_set_parallel_legs(Some(true));
    let fx = open_fx(2, "storm").await;
    const N: usize = 16;
    let mut inos = Vec::with_capacity(N);
    for i in 0..N {
        fx.place_only_on(i % 2);
        let ino = create(&fx, &format!("storm-{i}.bin")).await;
        write_at(&fx, ino, 0, &pattern(BS as usize, i as u8)).await;
        inos.push(ino);
    }
    assert!(fx.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
    // A slow meta barrier so the storm actually overlaps at the coalescer.
    squeezefs::uring_fs::arm_device_latency(
        &fx.meta_path,
        Duration::ZERO,
        Duration::from_millis(20),
    );
    let r0 = metric(&METRICS.meta_sync_requests);
    let s0 = metric(&METRICS.meta_device_syncs);
    let c0 = metric(&METRICS.fsync_calls);
    let futs: Vec<_> = inos.iter().map(|&ino| fsync(&fx, ino)).collect();
    futures::future::join_all(futs).await;
    squeezefs::uring_fs::disarm_device_latency(&fx.meta_path);
    assert_eq!(metric(&METRICS.fsync_calls) - c0, N as u64);
    assert_eq!(
        metric(&METRICS.meta_sync_requests) - r0,
        N as u64,
        "every fsync requests its meta barrier"
    );
    let issued = metric(&METRICS.meta_device_syncs) - s0;
    assert!(
        issued < N as u64 / 2,
        "the coalescer must collapse the storm: {issued} barriers issued for {N} fsyncs"
    );
    assert_eq!(
        metric(&METRICS.fsync_touched_unresolved),
        0,
        "every stamp resolved"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 6 — the durability ordering is untouched on the new ladder
// ---------------------------------------------------------------------------

/// DUR-1's ordering leg restated on the parallel/touched ladder: a faulted
/// data barrier fails the fsync and the metadata barrier never runs — a
/// failed leg fails the whole step, and the meta legs strictly follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_data_barrier_still_fails_the_fsync_before_the_meta_barrier() {
    let _g = serial().await;
    let _l = LeverGuard;
    fsync_economy::test_set_touched_namespaces(Some(true));
    fsync_economy::test_set_parallel_legs(Some(true));
    let fx = open_fx(2, "order").await;
    let b = fx.dev_path(1);
    let ino = durable_striped_on(&fx, "order.bin", 2, 0, 0x80).await;
    append_block_on(&fx, ino, 2, 0, 0x81).await;
    append_block_on(&fx, ino, 3, 1, 0x82).await;

    // Two legs in flight; B's fails.
    squeezefs::dev_power_cut::arm_barrier_error(&b, libc::EIO);
    let m0 = metric(&METRICS.meta_device_syncs);
    let res = fx.fs.fsync(req(), ino, 0, false).await;
    assert!(res.is_err(), "a failed leg fails the fsync");
    assert_eq!(
        metric(&METRICS.meta_device_syncs) - m0,
        0,
        "the meta barrier never ran behind a failed data barrier"
    );
    // The failed namespace stays dirty: once the fault clears, the next
    // fsync barriers it (never a clean no-op on a failed barrier).
    squeezefs::dev_power_cut::disarm_barrier_error(&b);
    squeezefs::dev_power_cut::arm_power_cut(&b);
    let eb = barrier_epoch(&b);
    let n0 = metric(&METRICS.fsync_noop_clean);
    fsync(&fx, ino).await;
    assert_eq!(barrier_epoch(&b) - eb, 1, "B barriered on the retry");
    assert_eq!(metric(&METRICS.fsync_noop_clean) - n0, 0);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// In-process rows for the W-5 note (release, `--ignored --nocapture`)
// ---------------------------------------------------------------------------

/// One A/B leg of the small-file fsync storm: `files` two-block files
/// created, written and fsynced by `streams` concurrent streams on a
/// two-volume mount whose data barriers take `dev_lat` each and whose
/// meta barrier takes `meta_lat` (SLOW devices, never parked). Placement
/// alternates the volumes by fill, so every fsync touches ONE of two
/// namespaces. Prints fsyncs/s, the fsync wall p50/p99 and the phase
/// family's per-fsync means plus the barrier ledger deltas.
async fn storm_leg(
    label: &str,
    touched: bool,
    parallel: bool,
    files: usize,
    streams: usize,
    dev_lat: Duration,
    meta_lat: Duration,
) {
    fsync_economy::test_set_touched_namespaces(Some(touched));
    fsync_economy::test_set_parallel_legs(Some(parallel));
    let tag: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let fx = open_fx(2, &format!("rows-{tag}")).await;
    for i in 0..2 {
        squeezefs::dev_power_cut::arm_barrier_latency(fx.dev_path(i), dev_lat);
    }
    squeezefs::uring_fs::arm_device_latency(&fx.meta_path, Duration::ZERO, meta_lat);
    let fx = Arc::new(fx);
    let before = phase_words();
    let (c0, f0, t0, d0, m0, n0) = (
        metric(&METRICS.fsync_calls),
        metric(&METRICS.fsync_data_namespaces_flushed),
        metric(&METRICS.fsync_data_namespaces_touched),
        metric(&METRICS.data_device_syncs),
        metric(&METRICS.meta_device_syncs),
        metric(&METRICS.fsync_noop_clean),
    );
    let t_wall = std::time::Instant::now();
    let mut tasks = Vec::new();
    for s in 0..streams {
        let fx = fx.clone();
        let tag = tag.clone();
        tasks.push(tokio::spawn(async move {
            let mut walls = Vec::new();
            let mut i = s;
            while i < files {
                let ino = create(&fx, &format!("storm-{tag}-{i}.bin")).await;
                write_at(&fx, ino, 0, &pattern(2 * BS as usize, i as u8)).await;
                let t = std::time::Instant::now();
                fsync(&fx, ino).await;
                walls.push(t.elapsed());
                i += streams;
            }
            walls
        }));
    }
    let mut walls: Vec<Duration> = Vec::new();
    for t in tasks {
        walls.extend(t.await.expect("stream"));
    }
    let wall = t_wall.elapsed();
    walls.sort();
    let pct = |p: f64| walls[((walls.len() as f64 - 1.0) * p) as usize];
    let after = phase_words();
    let calls = metric(&METRICS.fsync_calls) - c0;
    println!(
        "ROW {label:<28} touched={touched} parallel={parallel} files={files} streams={streams} \
         dev_lat={dev_lat:?} meta_lat={meta_lat:?}"
    );
    println!(
        "    fsyncs/s {:.0}  wall {:?}  fsync p50 {:?} p99 {:?} max {:?}",
        files as f64 / wall.as_secs_f64(),
        wall,
        pct(0.5),
        pct(0.99),
        walls[walls.len() - 1],
    );
    let mut phases = String::new();
    for (i, name) in fsync_economy::FSYNC_PHASE_NAMES.iter().enumerate() {
        let n = (after.count[i] - before.count[i]).max(1);
        phases.push_str(&format!(
            " {name}={:.0}µs",
            (after.sum_ns[i] - before.sum_ns[i]) as f64 / n as f64 / 1000.0
        ));
    }
    println!("    fsync_phase_ns mean/fsync:{phases}");
    println!(
        "    fsync_calls {calls}  namespaces touched {} flushed {} (of {} × 2)  \
         data_device_syncs {}  meta_device_syncs {}  noop_clean {}",
        metric(&METRICS.fsync_data_namespaces_touched) - t0,
        metric(&METRICS.fsync_data_namespaces_flushed) - f0,
        calls,
        metric(&METRICS.data_device_syncs) - d0,
        metric(&METRICS.meta_device_syncs) - m0,
        metric(&METRICS.fsync_noop_clean) - n0,
    );
    squeezefs::uring_fs::disarm_device_latency(&fx.meta_path);
    squeezefs::dev_power_cut::clear_faults();
    Arc::try_unwrap(fx)
        .ok()
        .expect("streams joined")
        .close()
        .await;
}

/// The W-5 note's in-process rows: A = both levers off (the shipped
/// serialized all-namespace shape), B = both on; A-B-B-A in one process.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "in-process rows for the W-5 note — run explicitly in release with --ignored --nocapture"]
async fn fsync_storm_rows() {
    let _g = serial().await;
    let _l = LeverGuard;
    let files = 192;
    let streams = 8;
    let dev_lat = Duration::from_millis(3);
    let meta_lat = Duration::from_millis(1);
    for (label, touched, parallel) in [
        // The first leg pays the process's cold start (uring workers,
        // allocator, blocking pool): discarded.
        ("warm-up (discarded)", false, false),
        ("A shipped (off/off)", false, false),
        ("B levers (on/on)", true, true),
        ("B levers (on/on)", true, true),
        ("A shipped (off/off)", false, false),
        ("touched only (on/off)", true, false),
        ("parallel only (off/on)", false, true),
    ] {
        storm_leg(label, touched, parallel, files, streams, dev_lat, meta_lat).await;
    }
}

// ---------------------------------------------------------------------------
// 7 — the staged-promotion lever (`SQUEEZEFS_FSYNC_PROMOTE_STAGED`)
// ---------------------------------------------------------------------------

/// A staged-layout file (4 KiB < size < BS on this fixture's staging dir):
/// its RAM layout says `staged`/`file_id`, no `block_map[0]`, and the
/// ring holds its payload.
async fn staged_file(fx: &Fx, name: &str, tag: u8) -> (u64, String) {
    let ino = create(fx, name).await;
    write_at(fx, ino, 0, &pattern(16 * 1024, tag)).await;
    let m = fx
        .fs
        .router
        .metadata_cache
        .get(&ino)
        .expect("RAM layout published by the write");
    assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
    let file_id = m
        .file_id
        .as_deref()
        .expect("staged layout carries file_id")
        .to_string();
    assert!(
        fx.fs.router.cache.nvme.read_staged(&file_id).is_some(),
        "fixture premise: payload resident in the ring"
    );
    (ino, file_id)
}

fn promote_gauges() -> (u64, u64, u64, u64) {
    (
        metric(&METRICS.fsync_promoted_files),
        metric(&METRICS.fsync_promoted_bytes),
        metric(&METRICS.fsync_promote_failures),
        metric(&METRICS.fsync_promote_noops),
    )
}

/// Lever on: the fsync promotes the file (block_map[0] named, ring entry
/// released, `fsync_promoted_*` moved, the `staged_promote` leg carries
/// the time and the family stays exact-sum); a second fsync has nothing
/// to move (no second promotion, no no-op — the map says promoted); an
/// fsync of a staged file whose ring entry is GONE before the promotion
/// commits (the racing-promoter shape: `promote_staged_file`'s
/// generation-gated commit refuses) is a counted no-op that does not fail
/// the fsync. Lever off: the fsync leaves the entry resident and moves no
/// gauge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_promotion_lever_promotes_exactly_once_on_fsync() {
    let _g = serial().await;
    let _l = LeverGuard;
    // Touched-namespace barriers ON: the promoted block's device is
    // barriered only if the promotion STAMPED it.
    fsync_economy::test_set_touched_namespaces(Some(true));
    // The staged-layout legs run at the one-page inline ceiling (the
    // default since the phase-B sweep, pinned explicitly so an override in
    // the environment cannot move the 16 KiB fixture file inline); the
    // last leg RAISES the ceiling over the file to pin the promotion's
    // inline dispatch.
    squeezefs::routing::set_inline_max_bytes_override(Some(squeezefs::routing::INLINE_MAX_FLOOR));
    let fx = open_fx(1, "promote").await;
    squeezefs::dev_power_cut::arm_power_cut(fx.dev_path(0));
    let sp = FsyncPhase::StagedPromote as usize;
    let total = FsyncPhase::Total as usize;

    // Off (the shipped default): resident, nothing promoted.
    fsync_economy::test_set_promote_staged(Some(false));
    let (ino_off, fid_off) = staged_file(&fx, "off.bin", 0x21).await;
    let g0 = promote_gauges();
    fsync(&fx, ino_off).await;
    assert!(
        fx.fs.router.cache.nvme.read_staged(&fid_off).is_some(),
        "lever off: the fsync'd staged-layout file stays resident"
    );
    assert_eq!(promote_gauges(), g0, "lever off: no promotion gauge moves");

    // On: promoted exactly once, in its own leg — and the promoted
    // block's device is barriered by THIS fsync (the DMA stamped the
    // touched table; DUR-1: the barrier precedes the meta barrier that
    // names the block).
    fsync_economy::test_set_promote_staged(Some(true));
    let (ino, fid) = staged_file(&fx, "on.bin", 0x22).await;
    let before = phase_words();
    let (files0, bytes0, fail0, noop0) = promote_gauges();
    let epoch0 = barrier_epoch(&fx.dev_path(0));
    let flushed0 = metric(&METRICS.fsync_data_namespaces_flushed);
    fsync(&fx, ino).await;
    let after = phase_words();
    let (files1, bytes1, fail1, noop1) = promote_gauges();
    assert_eq!(files1 - files0, 1, "one promotion");
    assert_eq!(bytes1 - bytes0, 16 * 1024, "the file's size at promotion");
    assert_eq!((fail1, noop1), (fail0, noop0), "no failure, no no-op");
    assert_eq!(
        barrier_epoch(&fx.dev_path(0)) - epoch0,
        1,
        "the promoting fsync barriers the device the promoted block landed on (stamped)"
    );
    assert_eq!(
        metric(&METRICS.fsync_data_namespaces_flushed) - flushed0,
        1,
        "one touched namespace flushed by the promoting fsync"
    );
    assert!(
        fx.fs.router.cache.nvme.read_staged(&fid).is_none(),
        "the ring entry is released by the promotion"
    );
    let m = fx.fs.router.metadata_cache.get(&ino).expect("layout");
    assert!(
        m.block_map.as_ref().is_some_and(|bm| bm.contains_key(&0)),
        "block_map[0] names the promoted block"
    );
    assert_eq!(after.count[sp] - before.count[sp], 1);
    assert!(
        after.sum_ns[sp] > before.sum_ns[sp],
        "the staged_promote leg carries the promotion's time"
    );
    let leg_sum: u64 = (0..total).map(|i| after.sum_ns[i] - before.sum_ns[i]).sum();
    assert_eq!(
        leg_sum,
        after.sum_ns[total] - before.sum_ns[total],
        "Σ legs ≡ total with the new leg"
    );
    // The promoted bytes read back through the durable mapping.
    let reply = fx
        .fs
        .read(req(), ino, 0, 0, 16 * 1024, 0)
        .await
        .expect("read promoted file");
    assert_eq!(reply.data.as_ref(), &pattern(16 * 1024, 0x22)[..]);

    // A second fsync: already promoted, nothing to move, nothing counted.
    fsync(&fx, ino).await;
    assert_eq!(
        promote_gauges(),
        (files1, bytes1, fail1, noop1),
        "a second fsync of a promoted file is not a promotion and not a no-op"
    );

    // The racing-promoter shape: the entry is gone, the RAM layout still
    // says staged/no map (a peer's promotion has not published yet, or
    // the crash-discard shape) — a counted no-op, the fsync succeeds.
    let (ino_gone, fid_gone) = staged_file(&fx, "gone.bin", 0x23).await;
    fx.fs.router.cache.nvme.remove_staged(&fid_gone);
    let (files2, _, fail2, noop2) = promote_gauges();
    fsync(&fx, ino_gone).await;
    let (files3, _, fail3, noop3) = promote_gauges();
    assert_eq!(files3, files2, "an entry that is gone promotes nothing");
    assert_eq!(fail3, fail2, "…and is not a failure");
    assert_eq!(noop3 - noop2, 1, "…it is the counted no-op");

    // The size dispatch: a file STAGED under the default ceiling, fsync'd
    // under a RAISED one (the operator's override — never the default,
    // the sweep's verdict), promotes INTO INLINE — the payload rides the
    // layout record, no block is allocated, no device is barriered for
    // it, and the bytes read back through `data_key`.
    let (ino_in, fid_in) = staged_file(&fx, "inline.bin", 0x24).await;
    squeezefs::routing::set_inline_max_bytes_override(Some(16 * 1024));
    assert_eq!(
        fx.fs.router.inline_max_bytes(ino_in),
        16 * 1024,
        "the raised ceiling admits the 16 KiB file"
    );
    let allocated0 = fx.fs.router.backend_router.allocated_bytes();
    let (files4, bytes4, fail4, noop4) = promote_gauges();
    let (pi0, pb0) = (
        metric(&METRICS.layout_promoted_inline),
        metric(&METRICS.layout_promoted_block),
    );
    let (fi0, epoch1) = (
        metric(&METRICS.fsync_promoted_inline_files),
        barrier_epoch(&fx.dev_path(0)),
    );
    fsync(&fx, ino_in).await;
    let (files5, bytes5, fail5, noop5) = promote_gauges();
    assert_eq!(files5 - files4, 1, "one promotion");
    assert_eq!(bytes5 - bytes4, 16 * 1024);
    assert_eq!((fail5, noop5), (fail4, noop4));
    assert_eq!(
        metric(&METRICS.fsync_promoted_inline_files) - fi0,
        1,
        "the fsync promotion was the INLINE dispatch"
    );
    assert_eq!(metric(&METRICS.layout_promoted_inline) - pi0, 1);
    assert_eq!(
        metric(&METRICS.layout_promoted_block),
        pb0,
        "no block promotion"
    );
    assert_eq!(
        fx.fs.router.backend_router.allocated_bytes(),
        allocated0,
        "the inline dispatch allocates no block"
    );
    assert_eq!(
        barrier_epoch(&fx.dev_path(0)),
        epoch1,
        "nothing landed on the data device — no data barrier for it"
    );
    assert!(
        fx.fs.router.cache.nvme.read_staged(&fid_in).is_none(),
        "the ring entry is released"
    );
    let m = fx.fs.router.metadata_cache.get(&ino_in).expect("layout");
    assert_eq!(m.file_type, "inline");
    assert!(m.block_map.is_none() && m.file_id.is_none());
    assert_eq!(
        m.data_key.as_deref().map(<[u8]>::len),
        Some(16 * 1024),
        "the payload rides the layout record"
    );
    let reply = fx
        .fs
        .read(req(), ino_in, 0, 0, 16 * 1024, 0)
        .await
        .expect("read inline-promoted file");
    assert_eq!(reply.data.as_ref(), &pattern(16 * 1024, 0x24)[..]);
    // And the block arm's gauge for the block-path promotion above.
    assert!(
        pb0 >= 1,
        "the block promotions above counted on layout_promoted_block"
    );

    fx.close().await;
}

// ---------------------------------------------------------------------------
// The pure cores
// ---------------------------------------------------------------------------

/// The stripe word is `gen << 32 | bits`: a stamp always changes the word
/// (so a clear that raced a stamp fails), a clear keeps the generation,
/// and an unresolvable stamp sets the ALL bit.
#[test]
fn touched_table_word_law() {
    use fsync_economy::{TouchedTable, ALL_BIT};
    // The process table's width is the D-3 DLM law — a derivation reused,
    // never a constant of its own.
    assert_eq!(
        fsync_economy::touched_table().width(),
        squeezefs::stripe_locks::dlm_stripe_width()
    );
    let t = TouchedTable::new(16);
    assert_eq!(t.width(), 16);
    let ino = 42u64;
    assert_eq!(TouchedTable::bits(t.observe(ino)), 0);
    t.stamp(ino, TouchedTable::bit_for_ordinal(Some(0)));
    let w1 = t.observe(ino);
    assert_eq!(TouchedTable::bits(w1), 1);
    // Re-stamping an already-set bit still changes the word (generation).
    t.stamp(ino, TouchedTable::bit_for_ordinal(Some(0)));
    let w2 = t.observe(ino);
    assert_ne!(w1, w2, "a stamp always moves the word");
    assert_eq!(TouchedTable::bits(w2), 1);
    // A clear against a stale observation fails and leaves the bits.
    assert!(!t.clear_observed(ino, w1));
    assert_eq!(TouchedTable::bits(t.observe(ino)), 1);
    // A clear against the current observation succeeds and keeps gen.
    assert!(t.clear_observed(ino, w2));
    let w3 = t.observe(ino);
    assert_eq!(TouchedTable::bits(w3), 0);
    assert_eq!(w3 >> 32, w2 >> 32);
    // Ordinal past the capacity, or None, is the ALL bit.
    assert_eq!(TouchedTable::bit_for_ordinal(None), ALL_BIT);
    assert_eq!(
        TouchedTable::bit_for_ordinal(Some(TouchedTable::MAX_ORDINAL + 1)),
        ALL_BIT
    );
    assert_eq!(
        TouchedTable::bit_for_ordinal(Some(TouchedTable::MAX_ORDINAL)),
        1 << TouchedTable::MAX_ORDINAL
    );
}

/// The barrier plan: bits → the devices to barrier, write-through skipped,
/// ALL / unmatched bits → every volatile device.
#[test]
fn barrier_plan_law() {
    use fsync_economy::{plan_barriers, BarrierPlan, ALL_BIT};
    let ordinals = [Some(0u32), Some(1), Some(2)];
    let volatile = [true, false, true];
    // Nothing touched: no legs, a clean no-op.
    let p = plan_barriers(0, &ordinals, &volatile);
    assert_eq!(
        p,
        BarrierPlan {
            targets: vec![],
            touched: 0,
            write_through_skips: 0,
            all: false,
        }
    );
    // Device 1 touched but write-through: skipped, counted.
    let p = plan_barriers(1 << 1, &ordinals, &volatile);
    assert_eq!(p.targets, Vec::<usize>::new());
    assert_eq!(p.touched, 1);
    assert_eq!(p.write_through_skips, 1);
    // Devices 0 + 2 touched: both barrier.
    let p = plan_barriers((1 << 0) | (1 << 2), &ordinals, &volatile);
    assert_eq!(p.targets, vec![0, 2]);
    assert_eq!(p.touched, 2);
    // ALL: every volatile device, touched = all devices.
    let p = plan_barriers(ALL_BIT, &ordinals, &volatile);
    assert_eq!(p.targets, vec![0, 2]);
    assert_eq!(p.touched, 3);
    assert_eq!(p.write_through_skips, 1);
    assert!(p.all);
    // An unmatched bit (a device no listed ordinal names) is ALL.
    let p = plan_barriers(1 << 7, &ordinals, &volatile);
    assert!(p.all);
    assert_eq!(p.targets, vec![0, 2]);
}
