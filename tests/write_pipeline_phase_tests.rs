//! Write-pipeline per-phase residence instrumentation — the 2026-07-31
//! write-wall campaign's conviction-2 instrument
//! (`.benchmarks/2026-07-31-write-wall.md`).
//!
//! Field conviction (4-node cluster, dev c9921f1): fresh writes wall at
//! 10.6 GB/s with ~45 blocks in-pipe ⇒ ~17 ms residence per block, while
//! the devices hold each block only ~3–4 ms — ~13 ms/block of pipeline
//! residence was UNATTRIBUTED (the meta hypothesis died twice this week;
//! the numbers must name the term, not another guess). The instrument:
//! an ALWAYS-ON `write_pipeline_phase_ns` histogram family (the
//! `fuse_op_phase_ns` pattern, deliberately NOT `SQUEEZEFS_OP_PROFILE`-
//! gated — the cost is one `Instant` read + one relaxed `fetch_add` per
//! phase per 4 MiB-class block, invisible at any credible block rate,
//! and the field needs the decomposition on production mounts without a
//! remount-to-arm round trip) decomposing every admitted block's
//! residence: admission wait → detach lag (tpc-lane scheduling) → block
//! lock → crypto → allocate → DMA → publish (conveyor wait + commit) →
//! displaced-free → invalidation tail → total (admit → release).
//!
//! Contracts:
//!
//! 1. **The family exists always-on** with exactly the ten phase keys,
//!    each a standard µs-bucket histogram, surfaced on the stats inode
//!    unconditionally (no profile gate).
//! 2. **A coverage-complete striped write drives every phase** through
//!    the REAL fs.create/fs.write pipeline path: all ten phase counts
//!    grow by ≥ the block count.
//! 3. **Recording is phase-exact** (pure): a span recorded against a
//!    phase lands in that phase's histogram and no other.
//!
//! RED against dev c9921f1: the family does not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    pipeline_phase_record, write_pipeline_phase_json, PipelinePhase, SqueezefsFilesystem, METRICS,
    STATS_INODE,
};
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

/// The histogram family is process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const PHASES: [&str; 10] = [
    "admit_wait",
    "detach_lag",
    "lock_wait",
    "crypto",
    "allocate",
    "dma",
    "publish",
    "displaced_free",
    "inval_tail",
    "total",
];

/// Sum of one phase histogram's buckets (= spans recorded).
fn phase_count(family: &serde_json::Value, phase: &str) -> u64 {
    family
        .get(phase)
        .unwrap_or_else(|| panic!("phase key {phase} missing from write_pipeline_phase_ns"))
        .as_object()
        .expect("phase histogram must be a bucket object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

/// Full-FS harness (the async_block_reclaim_tests FieldH shape): real v3
/// meta backend, real striped write path, real pipeline.
async fn make_harness(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
            .await
            .expect("open v3 meta volume"),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs: Arc::new(fs),
        req,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

// ---------------------------------------------------------------------------
// Contract 1 — the family exists always-on with exactly the ten keys and
// rides the stats inode UNGATED (no SQUEEZEFS_OP_PROFILE arm required).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_family_is_always_on_with_exact_keys() {
    let _g = serial().await;
    assert!(
        std::env::var("SQUEEZEFS_OP_PROFILE").is_err(),
        "fixture premise: the profile rig must be OFF — this family is \
         deliberately always-on"
    );
    let family = write_pipeline_phase_json();
    let obj = family
        .as_object()
        .expect("write_pipeline_phase_ns must be an object");
    assert_eq!(
        obj.len(),
        PHASES.len(),
        "exactly the ten residence phases: {obj:?}"
    );
    for p in PHASES {
        let _ = phase_count(&family, p); // key exists, histogram-shaped
    }

    // Stats-inode surface, ungated: a real fixture's stats JSON carries
    // the family with the profile rig off.
    let h = make_harness("wp_phase_stats_surface").await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let fam = stats
        .get("metrics")
        .and_then(|m| m.get("write_pipeline_phase_ns"))
        .expect("stats inode metrics must carry write_pipeline_phase_ns UNGATED");
    for p in PHASES {
        let _ = phase_count(fam, p);
    }
}

// ---------------------------------------------------------------------------
// Contract 2 — the real pipeline write path drives every phase.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_write_through_records_every_residence_phase() {
    let _g = serial().await;
    let h = make_harness("wp_phase_write_through").await;

    // Striped fixture: fresh write + fsync + drain (the fresh small-file
    // route promotes via staging in this venue; the pipelined
    // write-through — the phases-under-test venue — is the striped
    // coverage-complete path, so the measured pass below is a rewrite).
    let blocks = 8u64;
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new("phased"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    let p0 = pattern((blocks * FBS) as usize, 42);
    let w =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&p0), 0, 0)
            .await
            .expect("fixture write");
    assert_eq!(w.written as u64, blocks * FBS, "short fixture write");
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "fixture pipeline must drain"
    );
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");

    let before = write_pipeline_phase_json();
    let wt0 = METRICS
        .write_through_blocks
        .load(std::sync::atomic::Ordering::Relaxed);

    // The measured pass: one 8-block coverage-complete rewrite — 8
    // admissions, 8 detached uploads, 8 publishes, 8 displaced frees.
    let p1 = pattern((blocks * FBS) as usize, 77);
    let w =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&p1), 0, 0)
            .await
            .expect("rewrite");
    assert_eq!(w.written as u64, blocks * FBS, "short rewrite");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
    assert_eq!(
        METRICS
            .write_through_blocks
            .load(std::sync::atomic::Ordering::Relaxed)
            - wt0,
        blocks,
        "fixture premise: all {blocks} rewrite blocks must ride the \
         pipelined write-through (the phases-under-test venue)"
    );

    let after = write_pipeline_phase_json();
    for p in PHASES {
        let delta = phase_count(&after, p) - phase_count(&before, p);
        assert!(
            delta >= blocks,
            "phase {p} must record one span per pipelined block \
             (residence decomposition has no blind segments): got {delta}, \
             want ≥ {blocks}"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — recording is phase-exact (pure).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_lands_in_exactly_the_named_phase() {
    let _g = serial().await;
    let before = write_pipeline_phase_json();
    pipeline_phase_record(
        PipelinePhase::Publish,
        std::time::Instant::now() - std::time::Duration::from_micros(100),
    );
    let after = write_pipeline_phase_json();
    for p in PHASES {
        let delta = phase_count(&after, p) - phase_count(&before, p);
        let want = u64::from(p == "publish");
        assert_eq!(
            delta, want,
            "one publish span recorded ⇒ exactly the publish key moves \
             (phase {p} moved by {delta})"
        );
    }
}
