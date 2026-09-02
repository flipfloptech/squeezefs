//! E2E performance-audit instrument ladder (PR A1) — the contracts the
//! `.benchmarks/2026-09-02-e2e-audit` note's numbers rest on. Every
//! `*_phase_ns` family was a 26-bucket power-of-two µs histogram with no
//! sum and no count word, so every mean the evidence notes quote carried
//! up to 2× per-bucket error and no containment law (`total ≈ Σ phases`)
//! was checkable to the ns. These are histogram/export laws, never perf
//! numbers.
//!
//! A. Exact sums: `LatencyHistogram` (root) and the fuse3 sharded tables
//!    carry `count` + `sum_ns`; JSON = `{"buckets": {label: n}, "count",
//!    "sum_ns", "mean_ns"}` with the bucket labels byte-identical.
//! B. Write-side device split: `write_pipeline_phase_ns` gains
//!    `dev_queue` + `dev_service` at the NvmeBlockDev funnel (the read
//!    funnel's twin) so `dma ⊇ dev_queue + dev_service` is checkable; the
//!    journal pass's lumped `pass_journal_write` splits into
//!    `journal_ring_write` / `journal_prefix_wait` / `journal_barrier`
//!    (the old label stays as their sum).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{LatencyHistogram, SqueezefsFilesystem};
use squeezefs::latency_core::{latency_bucket_index, LATENCY_BUCKETS, LATENCY_BUCKET_LABELS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Downscaled block: the striped write path with a small fixture.
const BS: u64 = 65536;

/// The phase families are process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The publish_phase_tests harness: real v3 meta backend, real striped
/// write path, real publish + journal conveyors.
async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// Write `blocks` whole blocks, fsync, drain the pipeline.
async fn stream_blocks(h: &H, ino: u64, blocks: u32, tag: u8) {
    for b in 0..blocks {
        write_at(
            h,
            ino,
            b as u64 * BS,
            &pattern(BS as usize, tag ^ (b as u8)),
        )
        .await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline must drain"
    );
}

/// `(count, sum_ns)` of one phase in a family export.
fn phase_words(family: &serde_json::Value, phase: &str) -> (u64, u64) {
    let h = family
        .get(phase)
        .unwrap_or_else(|| panic!("phase {phase} missing from {family}"));
    (hist_count(h), hist_sum_ns(h))
}

// ---------------------------------------------------------------------------
// A — exact sums
// ---------------------------------------------------------------------------

/// The four top-level keys every histogram export carries.
const HIST_KEYS: [&str; 4] = ["buckets", "count", "sum_ns", "mean_ns"];

fn hist_count(h: &serde_json::Value) -> u64 {
    h["count"].as_u64().expect("count is u64")
}

fn hist_sum_ns(h: &serde_json::Value) -> u64 {
    h["sum_ns"].as_u64().expect("sum_ns is u64")
}

fn bucket_sum(h: &serde_json::Value) -> u64 {
    h["buckets"]
        .as_object()
        .expect("buckets is an object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

#[test]
fn histogram_reports_exact_sum_count_and_mean() {
    let h = LatencyHistogram::default();
    assert_eq!(h.count(), 0);
    assert_eq!(h.sum_ns(), 0);
    assert_eq!(h.mean_ns(), 0, "an empty histogram's mean is 0, never NaN");

    let spans_ns: [u64; 5] = [1, 999, 1_000, 123_456, 7_000_000_000];
    for ns in spans_ns {
        h.record(Duration::from_nanos(ns));
    }
    let want_sum: u64 = spans_ns.iter().sum();
    assert_eq!(h.count(), spans_ns.len() as u64, "one count per record");
    assert_eq!(
        h.sum_ns(),
        want_sum,
        "sum is exact to the ns, not a bucket estimate"
    );
    assert_eq!(h.mean_ns(), want_sum / spans_ns.len() as u64);

    // The bucket law is unchanged: the same µs → index formula as before.
    let json = h.to_json();
    for ns in spans_ns {
        let idx = latency_bucket_index(ns / 1_000);
        let n = json["buckets"][LATENCY_BUCKET_LABELS[idx]]
            .as_u64()
            .expect("bucket present");
        assert!(n >= 1, "{ns} ns must land in bucket {idx}");
    }
    assert_eq!(bucket_sum(&json), h.count(), "Σ buckets ≡ count");
}

#[test]
fn histogram_json_shape_is_pinned() {
    let h = LatencyHistogram::default();
    h.record(Duration::from_micros(100));
    h.record(Duration::from_micros(300));
    let json = h.to_json();
    let obj = json.as_object().expect("histogram JSON is an object");
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    let mut want = HIST_KEYS.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want, "exactly the four top-level keys");

    let buckets = json["buckets"].as_object().expect("buckets object");
    assert_eq!(buckets.len(), LATENCY_BUCKETS);
    for label in LATENCY_BUCKET_LABELS {
        assert!(buckets.contains_key(label), "label {label} byte-identical");
    }
    assert_eq!(hist_count(&json), 2);
    assert_eq!(hist_sum_ns(&json), 400_000);
    assert_eq!(json["mean_ns"].as_u64(), Some(200_000));
}

#[test]
fn empty_histogram_json_is_all_zero() {
    let json = LatencyHistogram::default().to_json();
    assert_eq!(hist_count(&json), 0);
    assert_eq!(hist_sum_ns(&json), 0);
    assert_eq!(json["mean_ns"].as_u64(), Some(0));
    assert_eq!(bucket_sum(&json), 0);
}

/// Every root phase family exports through the one histogram shape.
#[test]
fn phase_families_export_the_exact_shape() {
    use squeezefs::fuse_client::{
        meta_txpass_phase_json, publish_phase_json, read_fill_phase_json, read_serve_phase_json,
        write_pipeline_phase_json,
    };
    for fam in [
        write_pipeline_phase_json(),
        publish_phase_json(),
        meta_txpass_phase_json(),
        read_serve_phase_json(),
        read_fill_phase_json(),
    ] {
        for (phase, h) in fam.as_object().expect("family object") {
            for k in HIST_KEYS {
                assert!(h.get(k).is_some(), "phase {phase} lacks {k}");
            }
            assert_eq!(
                bucket_sum(h),
                hist_count(h),
                "phase {phase}: Σ buckets ≡ count"
            );
        }
    }
}

/// The fuse3 sharded tables fold EXACTLY across recording threads: N
/// threads each own a shard; the snapshot's count/sum deltas equal the
/// known totals to the ns.
#[test]
fn fuse3_sharded_fold_is_exact() {
    use fuse3::{write_transport_phase_record, write_transport_phase_snapshot, TransportPhase};
    const THREADS: u64 = 6;
    const PER_THREAD: u64 = 500;
    let phase_idx = write_transport_phase_snapshot()
        .iter()
        .position(|p| p.name == "commit_flush")
        .expect("commit_flush phase present");
    let before = write_transport_phase_snapshot();
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    // Distinct ns spans so a bucket-midpoint estimate could
                    // never reproduce the sum by accident.
                    let ns = 1_000 * (t + 1) + 7 * i;
                    write_transport_phase_record(
                        TransportPhase::CommitFlush,
                        Duration::from_nanos(ns),
                    );
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let after = write_transport_phase_snapshot();
    let want_count = THREADS * PER_THREAD;
    let want_sum: u64 = (0..THREADS)
        .flat_map(|t| (0..PER_THREAD).map(move |i| 1_000 * (t + 1) + 7 * i))
        .sum();
    assert_eq!(
        after[phase_idx].count - before[phase_idx].count,
        want_count,
        "count folds exactly across shards"
    );
    assert_eq!(
        after[phase_idx].sum_ns - before[phase_idx].sum_ns,
        want_sum,
        "sum_ns folds exactly across shards"
    );
    let bucket_delta: u64 = after[phase_idx]
        .buckets
        .iter()
        .zip(before[phase_idx].buckets.iter())
        .map(|(a, b)| a - b)
        .sum();
    assert_eq!(
        bucket_delta, want_count,
        "Σ buckets ≡ count on the fold too"
    );

    // The daemon renders the fold through the same JSON shape.
    let json = squeezefs::fuse_client::write_transport_phase_json();
    let h = &json["commit_flush"];
    for k in HIST_KEYS {
        assert!(h.get(k).is_some(), "transport JSON lacks {k}");
    }
    assert_eq!(hist_count(h), after[phase_idx].count);
    assert_eq!(hist_sum_ns(h), after[phase_idx].sum_ns);
}

// ---------------------------------------------------------------------------
// B — write-side device split + journal split
// ---------------------------------------------------------------------------

const PIPELINE_PHASES: [&str; 12] = [
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
    "dev_queue",
    "dev_service",
];

const TXPASS_PHASES: [&str; 8] = [
    "tx_queue_wait",
    "pass_admission",
    "pass_leaf_locks",
    "pass_journal_write",
    "pass_total",
    "journal_ring_write",
    "journal_prefix_wait",
    "journal_barrier",
];

#[test]
fn write_pipeline_family_carries_the_device_split() {
    let fam = squeezefs::fuse_client::write_pipeline_phase_json();
    let obj = fam.as_object().expect("family object");
    assert_eq!(
        obj.len(),
        PIPELINE_PHASES.len(),
        "exactly the twelve phases: {obj:?}"
    );
    for p in PIPELINE_PHASES {
        let _ = phase_words(&fam, p);
    }
}

#[test]
fn meta_txpass_family_carries_the_journal_split() {
    let fam = squeezefs::fuse_client::meta_txpass_phase_json();
    let obj = fam.as_object().expect("family object");
    assert_eq!(
        obj.len(),
        TXPASS_PHASES.len(),
        "exactly the eight phases: {obj:?}"
    );
    for p in TXPASS_PHASES {
        let _ = phase_words(&fam, p);
    }
}

/// One `write_block` at the NvmeBlockDev funnel moves `dev_queue` and
/// `dev_service` by exactly one span each, both contained in the caller's
/// wall; a barrier (`flush`) rides the same response variant and moves
/// NEITHER.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nvme_write_block_is_split_at_the_funnel_and_barriers_are_not() {
    use squeezefs::fuse_client::write_pipeline_phase_json;
    let _g = serial().await;
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(8 << 20).unwrap();
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(f.path().to_str().unwrap());

    let before = write_pipeline_phase_json();
    let t0 = std::time::Instant::now();
    dev.write_block(1 << 20, bytes::Bytes::from(vec![0xA5u8; 1 << 20]))
        .await
        .expect("write_block");
    let wall = t0.elapsed().as_nanos() as u64;
    let after = write_pipeline_phase_json();

    let (q0, qs0) = phase_words(&before, "dev_queue");
    let (s0, ss0) = phase_words(&before, "dev_service");
    let (q1, qs1) = phase_words(&after, "dev_queue");
    let (s1, ss1) = phase_words(&after, "dev_service");
    assert_eq!(q1 - q0, 1, "one dev_queue span per write_block");
    assert_eq!(s1 - s0, 1, "one dev_service span per write_block");
    assert!(
        (qs1 - qs0) + (ss1 - ss0) <= wall,
        "dev_queue + dev_service ({} ns) ⊆ the write_block wall ({wall} ns)",
        (qs1 - qs0) + (ss1 - ss0)
    );
    // `dma` is the pipeline's span, not the funnel's: a bare device write
    // records no pipeline `dma`.
    assert_eq!(
        phase_words(&after, "dma").0,
        phase_words(&before, "dma").0,
        "a bare write_block is not a pipeline dma span"
    );

    let before = write_pipeline_phase_json();
    dev.flush().await.expect("barrier");
    let after = write_pipeline_phase_json();
    assert_eq!(
        phase_words(&after, "dev_queue"),
        phase_words(&before, "dev_queue"),
        "a barrier is not a device write"
    );
    assert_eq!(
        phase_words(&after, "dev_service"),
        phase_words(&before, "dev_service"),
        "a barrier is not a device write"
    );
}

/// The pipeline's `dma` span contains the funnel split: every pipeline
/// block's DMA is one funnel write, so `count(dev_queue) ≥ count(dma)`
/// (other device writers add to the funnel, never to `dma`),
/// `count(dev_queue) ≡ count(dev_service)` (every submitted write
/// completes), and `Σ dma ≥ Σ dev_service` (the DMA span brackets the
/// device service leg).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_dma_contains_the_funnel_split() {
    use squeezefs::fuse_client::write_pipeline_phase_json;
    let _g = serial().await;
    let h = make([0x11; 16], "audit_b_pipeline").await;
    let ino = create(&h, "split").await;
    let before = write_pipeline_phase_json();
    stream_blocks(&h, ino, 6, 0x21).await;
    let after = write_pipeline_phase_json();

    let d = |p: &str| {
        let (c0, s0) = phase_words(&before, p);
        let (c1, s1) = phase_words(&after, p);
        (c1 - c0, s1 - s0)
    };
    let (dma_n, dma_sum) = d("dma");
    let (q_n, _) = d("dev_queue");
    let (s_n, s_sum) = d("dev_service");
    assert!(dma_n >= 6, "six whole blocks ⇒ ≥ 6 dma spans, got {dma_n}");
    assert!(
        q_n >= dma_n,
        "every dma is a funnel write: dev_queue {q_n} ≥ dma {dma_n}"
    );
    assert_eq!(q_n, s_n, "every submitted write completes");
    assert!(
        dma_sum >= s_sum,
        "Σ dma ({dma_sum}) brackets Σ dev_service ({s_sum})"
    );
}

/// The journal split is contained in the old lumped label:
/// `count(journal_ring_write) ≡ count(journal_prefix_wait) ≡
/// count(pass_journal_write)` (one each per successful pass) and
/// `Σ pass_journal_write ≥ Σ ring_write + Σ prefix_wait + Σ barrier`.
/// On the default (non-strict) cadence no barrier rides the pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_split_is_contained_in_pass_journal_write() {
    use squeezefs::fuse_client::meta_txpass_phase_json;
    let _g = serial().await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let h = make([0x12; 16], "audit_b_journal").await;
    let before = meta_txpass_phase_json();
    for i in 0..24 {
        create(&h, &format!("j{i}")).await;
    }
    let after = meta_txpass_phase_json();
    let d = |p: &str| {
        let (c0, s0) = phase_words(&before, p);
        let (c1, s1) = phase_words(&after, p);
        (c1 - c0, s1 - s0)
    };
    let (lump_n, lump_sum) = d("pass_journal_write");
    let (ring_n, ring_sum) = d("journal_ring_write");
    let (pfx_n, pfx_sum) = d("journal_prefix_wait");
    let (bar_n, bar_sum) = d("journal_barrier");
    assert!(lump_n >= 1, "creates commit through the conveyor");
    assert_eq!(ring_n, lump_n, "one ring write per successful pass");
    assert_eq!(
        pfx_n, lump_n,
        "one completed-prefix wait per successful pass"
    );
    assert_eq!(bar_n, 0, "non-strict cadence: the barrier leaves the pass");
    assert!(
        lump_sum >= ring_sum + pfx_sum + bar_sum,
        "Σ pass_journal_write {lump_sum} ≥ ring {ring_sum} + prefix {pfx_sum} + barrier {bar_sum}"
    );
}

/// Strict cadence (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`): every pass
/// carries the coalesced barrier, so `count(journal_barrier) ≡
/// count(pass_journal_write)` and the containment law still holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_cadence_puts_the_barrier_in_the_pass() {
    use squeezefs::fuse_client::meta_txpass_phase_json;
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    let h = make([0x13; 16], "audit_b_journal_strict").await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let before = meta_txpass_phase_json();
    for i in 0..12 {
        create(&h, &format!("s{i}")).await;
    }
    let after = meta_txpass_phase_json();
    let d = |p: &str| {
        let (c0, s0) = phase_words(&before, p);
        let (c1, s1) = phase_words(&after, p);
        (c1 - c0, s1 - s0)
    };
    let (lump_n, lump_sum) = d("pass_journal_write");
    let (ring_n, ring_sum) = d("journal_ring_write");
    let (pfx_n, pfx_sum) = d("journal_prefix_wait");
    let (bar_n, bar_sum) = d("journal_barrier");
    assert!(lump_n >= 1);
    assert_eq!(bar_n, lump_n, "strict: one barrier per pass");
    assert_eq!(ring_n, lump_n);
    assert_eq!(pfx_n, lump_n);
    assert!(lump_sum >= ring_sum + pfx_sum + bar_sum);
}
