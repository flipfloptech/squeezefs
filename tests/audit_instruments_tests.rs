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
//! C. Always-on `meta_op_phase_ns`: per metadata op {entry_to_backend,
//!    backend, backend_to_reply, total}, riding the existing `OpProf`
//!    begin/mark/Drop hooks with NO `SQUEEZEFS_OP_PROFILE` gate; the gated
//!    `fuse_op_phase_ns` keeps its behavior (silent + absent when off).
//! D. Lock wait AND hold: `lock_phase_ns` = {stripe_lock_wait,
//!    stripe_lock_hold} for the 3.5 `INODE_META_LOCKS` at every site (one
//!    guard type, `routing::meta_lock_acquire`), `leaf_lock_wait` (the 4b
//!    pure wait split out of `pass_leaf_locks`) and `dlm_guard_hold` (the
//!    4a exclusive I-guard's `DlmGuard` Drop). Waits/holds move on
//!    contended fixtures, waits stay zero uncontended.
//! E. CPU attribution: `daemon_cpu_ns` (RUSAGE_SELF utime+stime) and
//!    `daemon_cpu_ns_by_class` (per-thread schedstat folded by comm
//!    prefix; retired threads keep their last sample so classes stay
//!    monotone), sampled at stats-read time only.

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
    make_bs(uuid, alloc_ns, BS).await
}

async fn make_bs(uuid: [u8; 16], alloc_ns: &str, bs: u64) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", bs.to_string());
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

/// The eight audit-B phases plus D-2's two durability-lane phases
/// (`window_lane_wait`: handoff → lane pickup; `window_total`: apply pass
/// start → the window's members answered).
const TXPASS_PHASES: [&str; 10] = [
    "tx_queue_wait",
    "pass_admission",
    "pass_leaf_locks",
    "pass_journal_write",
    "pass_total",
    "journal_ring_write",
    "journal_prefix_wait",
    "journal_barrier",
    "window_lane_wait",
    "window_total",
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
        "exactly the ten phases: {obj:?}"
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
    // The CoW pipeline venue (the write_pipeline_phase_tests posture): the
    // fresh small-file route promotes via staging, so the measured pass is
    // the striped coverage-complete REWRITE, routed CoW (not in place, no
    // rewrite shadow) so every block pays `dma`.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            squeezefs::routing::set_rewrite_shadow(true);
            squeezefs::fuse_client::set_inplace_overwrite(false);
        }
    }
    let _r = Restore;
    squeezefs::routing::set_rewrite_shadow(false);
    squeezefs::fuse_client::set_inplace_overwrite(false);
    // 4 KiB blocks, one coverage-complete 8-block write (the
    // write_pipeline_phase_tests venue): fixture write + fsync promotes to
    // striped; the measured pass is the 8-block rewrite.
    const FBS: u64 = 4096;
    let blocks = 8u64;
    let h = make_bs([0x11; 16], "audit_b_pipeline", FBS).await;
    let ino = create(&h, "split").await;
    let whole = |tag: u8| pattern((blocks * FBS) as usize, tag);
    write_at(&h, ino, 0, &whole(0x21)).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
    let before = write_pipeline_phase_json();
    write_at(&h, ino, 0, &whole(0x33)).await;
    assert!(h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
    let after = write_pipeline_phase_json();

    let d = |p: &str| {
        let (c0, s0) = phase_words(&before, p);
        let (c1, s1) = phase_words(&after, p);
        (c1 - c0, s1 - s0)
    };
    let (dma_n, dma_sum) = d("dma");
    let (q_n, _) = d("dev_queue");
    let (s_n, s_sum) = d("dev_service");
    assert!(
        dma_n >= blocks,
        "{blocks} whole blocks ⇒ ≥ {blocks} dma spans, got {dma_n}"
    );
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

// ---------------------------------------------------------------------------
// C — always-on meta_op_phase_ns
// ---------------------------------------------------------------------------

const META_OPS: [&str; 9] = [
    "lookup",
    "getattr",
    "setattr",
    "mkdir",
    "create",
    "unlink",
    "rename",
    "readdir",
    "readdirplus",
];

const META_OP_PHASES: [&str; 4] = ["entry_to_backend", "backend", "backend_to_reply", "total"];

fn meta_op_words(fam: &serde_json::Value, op: &str, phase: &str) -> (u64, u64) {
    let h = &fam[op][phase];
    assert!(h.is_object(), "meta_op_phase_ns.{op}.{phase} missing");
    (hist_count(h), hist_sum_ns(h))
}

#[test]
fn meta_op_family_is_pinned_to_the_metadata_ops_and_four_phases() {
    let fam = squeezefs::fuse_client::meta_op_phase_json();
    let ops = fam.as_object().expect("family object");
    let mut have: Vec<&str> = ops.keys().map(|k| k.as_str()).collect();
    have.sort_unstable();
    let mut want = META_OPS.to_vec();
    want.sort_unstable();
    assert_eq!(have, want, "exactly the metadata op classes");
    for op in META_OPS {
        let phases = fam[op].as_object().expect("op object");
        assert_eq!(phases.len(), META_OP_PHASES.len(), "{op}: four phases");
        for ph in META_OP_PHASES {
            let _ = meta_op_words(&fam, op, ph);
        }
    }
}

/// The pure hook law with the profile rig OFF: one metadata `OpProf`
/// (begin → mark_backend_start → mark_backend_done → drop) records
/// exactly one span in each of its four phases, the three legs tile
/// `total` exactly, a data op (READ) moves nothing here, and the gated
/// `fuse_op_phase_ns` family stays silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_metadata_op_moves_its_four_phases_and_the_gated_family_stays_silent() {
    use squeezefs::fuse_client::{
        meta_op_phase_json, op_profile_enabled, op_profile_phase_json, FuseOpKind, OpProf,
    };
    let _g = serial().await;
    assert!(
        !op_profile_enabled(),
        "fixture premise: SQUEEZEFS_OP_PROFILE must be OFF"
    );
    let m0 = meta_op_phase_json();
    let g0 = op_profile_phase_json();

    let p = OpProf::begin(FuseOpKind::Create, 7);
    p.mark_backend_start();
    std::thread::sleep(Duration::from_micros(300));
    p.mark_backend_done();
    drop(p);

    let m1 = meta_op_phase_json();
    let g1 = op_profile_phase_json();
    let mut legs = 0u64;
    for ph in META_OP_PHASES {
        let (c0, s0) = meta_op_words(&m0, "create", ph);
        let (c1, s1) = meta_op_words(&m1, "create", ph);
        assert_eq!(c1 - c0, 1, "create.{ph}: exactly one span");
        if ph != "total" {
            legs += s1 - s0;
        }
    }
    let (_, t0) = meta_op_words(&m0, "create", "total");
    let (_, t1) = meta_op_words(&m1, "create", "total");
    assert_eq!(legs, t1 - t0, "the three legs tile total exactly");
    assert!(
        meta_op_words(&m1, "create", "backend").1 - meta_op_words(&m0, "create", "backend").1
            >= 300_000,
        "backend leg brackets the 300 µs backend span"
    );
    for op in META_OPS {
        if op == "create" {
            continue;
        }
        for ph in META_OP_PHASES {
            assert_eq!(
                meta_op_words(&m1, op, ph).0,
                meta_op_words(&m0, op, ph).0,
                "{op}.{ph} must not move on a create"
            );
        }
    }
    assert_eq!(
        g1, g0,
        "the SQUEEZEFS_OP_PROFILE-gated fuse_op_phase_ns must stay silent when off"
    );

    // A data op is not a metadata op: nothing here moves.
    let m2 = meta_op_phase_json();
    let r = OpProf::begin(FuseOpKind::Read, 7);
    r.mark_backend_start();
    r.mark_backend_done();
    drop(r);
    assert_eq!(
        meta_op_phase_json(),
        m2,
        "READ never touches meta_op_phase_ns"
    );
}

/// Through the real FS: a create moves the family; the stats inode
/// carries it UNGATED (and, with the rig off, does NOT carry the gated
/// `fuse_op_phase_ns`); a stats-inode read — a data op — leaves every
/// metadata class flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_create_moves_the_family_and_the_stats_inode_carries_it_ungated() {
    use squeezefs::fuse_client::{meta_op_phase_json, STATS_INODE};
    let _g = serial().await;
    let h = make([0x21; 16], "audit_c_create").await;
    let m0 = meta_op_phase_json();
    let _ino = create(&h, "c1").await;
    let m1 = meta_op_phase_json();
    for ph in META_OP_PHASES {
        assert_eq!(
            meta_op_words(&m1, "create", ph).0 - meta_op_words(&m0, "create", ph).0,
            1,
            "one real create ⇒ one create.{ph} span"
        );
    }

    let m2 = meta_op_phase_json();
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let metrics = stats.get("metrics").expect("metrics object");
    let fam = metrics
        .get("meta_op_phase_ns")
        .expect("stats inode must carry meta_op_phase_ns UNGATED");
    for op in META_OPS {
        for ph in META_OP_PHASES {
            let _ = meta_op_words(fam, op, ph);
        }
    }
    assert!(
        metrics.get("fuse_op_phase_ns").is_none(),
        "the gated family stays absent with the rig off"
    );
    let m3 = meta_op_phase_json();
    assert_eq!(
        m3, m2,
        "a stats-inode READ is a data op: no metadata class moves"
    );
}

// ---------------------------------------------------------------------------
// D — lock wait AND hold histograms
// ---------------------------------------------------------------------------

/// The four audit-D phases plus D-2's `leaf_lock_hold` (the conveyor
/// pass's 4b union hold — the "never across device I/O" instrument) and
/// D-3's `dlm_guard_wait` (the CONTENDED 4a acquire's parked span; its
/// count ≡ Σ the two 4a census classes — `tests/stripe_census_tests.rs`).
const LOCK_PHASES: [&str; 6] = [
    "stripe_lock_wait",
    "stripe_lock_hold",
    "leaf_lock_wait",
    "dlm_guard_hold",
    "leaf_lock_hold",
    "dlm_guard_wait",
];

#[test]
fn lock_family_is_pinned_to_six_phases() {
    let fam = squeezefs::fuse_client::lock_phase_json();
    let obj = fam.as_object().expect("family object");
    assert_eq!(
        obj.len(),
        LOCK_PHASES.len(),
        "exactly the six lock phases: {obj:?}"
    );
    for p in LOCK_PHASES {
        let _ = phase_words(&fam, p);
    }
}

/// The 3.5 stripe guard: an uncontended acquire records a ZERO wait (the
/// fast path pays no clock read for it) and one hold; a contended acquire
/// records the wait it actually parked for, and the holder's hold
/// brackets its critical section. The HOLD half is the
/// `SQUEEZEFS_OP_PROFILE`-gated half (measured +66.6 ns/acquire always-on,
/// over the ~50 ns rule): OFF by default, armed here through the seam.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stripe_lock_wait_and_hold_move_on_contention_and_wait_is_zero_uncontended() {
    use squeezefs::fuse_client::{lock_phase_json, set_stripe_hold_timing};
    use squeezefs::routing::meta_lock_acquire;
    let _g = serial().await;
    const INO: u64 = 0x5EED_0001;

    // Default posture (rig off): the wait half records, the hold half is
    // silent.
    let f0 = lock_phase_json();
    drop(meta_lock_acquire(INO).await);
    let f1 = lock_phase_json();
    assert_eq!(
        phase_words(&f1, "stripe_lock_wait").0 - phase_words(&f0, "stripe_lock_wait").0,
        1,
        "the wait half is always-on"
    );
    assert_eq!(
        phase_words(&f1, "stripe_lock_hold"),
        phase_words(&f0, "stripe_lock_hold"),
        "the hold half is gated (off with the rig off)"
    );

    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            set_stripe_hold_timing(false);
        }
    }
    let _d = Disarm;
    set_stripe_hold_timing(true);

    // Uncontended, hold armed.
    let f0 = lock_phase_json();
    {
        let g = meta_lock_acquire(INO).await;
        std::thread::sleep(Duration::from_micros(500));
        drop(g);
    }
    let f1 = lock_phase_json();
    let (w0, ws0) = phase_words(&f0, "stripe_lock_wait");
    let (w1, ws1) = phase_words(&f1, "stripe_lock_wait");
    let (h0, hs0) = phase_words(&f0, "stripe_lock_hold");
    let (h1, hs1) = phase_words(&f1, "stripe_lock_hold");
    assert_eq!(w1 - w0, 1, "one wait sample per acquire");
    assert_eq!(
        ws1 - ws0,
        0,
        "uncontended ⇒ the wait sample is exactly zero"
    );
    assert_eq!(h1 - h0, 1, "one hold sample per release");
    assert!(
        hs1 - hs0 >= 500_000,
        "hold brackets the 500 µs critical section"
    );

    // Contended: A holds for 2 ms while B parks.
    let f0 = lock_phase_json();
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel::<()>();
    let a = tokio::spawn(async move {
        let g = meta_lock_acquire(INO).await;
        armed_tx.send(()).unwrap();
        squeezefs_ipc::sqz_time::sleep(Duration::from_millis(2)).await;
        drop(g);
    });
    armed_rx.await.unwrap();
    let t0 = std::time::Instant::now();
    let g = meta_lock_acquire(INO).await;
    let b_wall = t0.elapsed().as_nanos() as u64;
    drop(g);
    a.await.unwrap();
    let f1 = lock_phase_json();
    let (_, ws0) = phase_words(&f0, "stripe_lock_wait");
    let (w1, ws1) = phase_words(&f1, "stripe_lock_wait");
    let (h0, hs0) = phase_words(&f0, "stripe_lock_hold");
    let (h1, hs1) = phase_words(&f1, "stripe_lock_hold");
    assert_eq!(
        w1 - phase_words(&f0, "stripe_lock_wait").0,
        2,
        "two acquires"
    );
    assert!(
        ws1 - ws0 >= 1_000_000 && ws1 - ws0 <= b_wall,
        "B's parked wait ({}) is ≥ ~A's hold and ⊆ B's wall ({b_wall})",
        ws1 - ws0
    );
    assert_eq!(h1 - h0, 2, "two releases");
    assert!(hs1 - hs0 >= 2_000_000, "A's hold brackets its 2 ms section");
}

/// The 4a I-guard: an EXCLUSIVE inode guard records its hold at drop;
/// shared inode guards and dentry guards record nothing here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dlm_exclusive_inode_guard_records_its_hold() {
    use squeezefs::fuse_client::lock_phase_json;
    use squeezefs::meta_backend::dlm::DlmLockManager;
    let _g = serial().await;
    let dlm = DlmLockManager::new();

    let f0 = lock_phase_json();
    let g = dlm.lock_inode_exclusive(99).await;
    std::thread::sleep(Duration::from_millis(1));
    drop(g);
    let f1 = lock_phase_json();
    let (c0, s0) = phase_words(&f0, "dlm_guard_hold");
    let (c1, s1) = phase_words(&f1, "dlm_guard_hold");
    assert_eq!(c1 - c0, 1, "one exclusive I-guard ⇒ one hold sample");
    assert!(s1 - s0 >= 1_000_000, "hold brackets the 1 ms section");

    let f2 = lock_phase_json();
    drop(dlm.lock_inode_shared(99).await);
    drop(dlm.lock_dentry_exclusive(1, "x").await);
    drop(dlm.lock_dentry_shared(1, "y").await);
    let f3 = lock_phase_json();
    assert_eq!(
        phase_words(&f3, "dlm_guard_hold"),
        phase_words(&f2, "dlm_guard_hold"),
        "shared / dentry guards are not the 4a exclusive I-guard"
    );
}

/// Through the real FS: mkdirs take the parent's 4a EXCLUSIVE I-guard
/// (design §3.8 — regular creates take it SHARED, so they are not this
/// histogram's population) and commit through the conveyor, whose pass
/// records exactly one `leaf_lock_wait` per `pass_leaf_locks` with
/// `Σ leaf_lock_wait ≤ Σ pass_leaf_locks` (the pure wait is a subset of
/// the locked window).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conveyor_pass_splits_the_leaf_lock_pure_wait() {
    use squeezefs::fuse_client::{lock_phase_json, meta_txpass_phase_json};
    let _g = serial().await;
    let h = make([0x31; 16], "audit_d_leaf").await;
    let l0 = lock_phase_json();
    let t0 = meta_txpass_phase_json();
    for i in 0..16 {
        h.fs.mkdir(h.req, 1, OsStr::new(&format!("d{i}")), 0o755, 0)
            .await
            .expect("mkdir");
    }
    let l1 = lock_phase_json();
    let t1 = meta_txpass_phase_json();
    let (pl0, pls0) = phase_words(&t0, "pass_leaf_locks");
    let (pl1, pls1) = phase_words(&t1, "pass_leaf_locks");
    let (lw0, lws0) = phase_words(&l0, "leaf_lock_wait");
    let (lw1, lws1) = phase_words(&l1, "leaf_lock_wait");
    assert!(pl1 - pl0 >= 1, "mkdirs run conveyor passes");
    assert_eq!(lw1 - lw0, pl1 - pl0, "one leaf_lock_wait per pass");
    assert!(
        lws1 - lws0 <= pls1 - pls0,
        "Σ leaf_lock_wait ({}) ⊆ Σ pass_leaf_locks ({})",
        lws1 - lws0,
        pls1 - pls0
    );
    // Every mkdir holds the parent's exclusive I-guard; its drop rides
    // the conveyor's terminal fan-out (the queue entry co-owns the guard
    // set until then), so the LAST op's sample may land after this read
    // — the population, not the exact count, is the contract.
    let holds = phase_words(&l1, "dlm_guard_hold").0 - phase_words(&l0, "dlm_guard_hold").0;
    assert!(holds >= 8, "mkdirs hold exclusive I-guards (saw {holds})");

    // Stats-inode surface, ungated.
    let reply =
        h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let fam = stats["metrics"]
        .get("lock_phase_ns")
        .expect("stats inode must carry lock_phase_ns UNGATED");
    for p in LOCK_PHASES {
        let _ = phase_words(fam, p);
    }
}

// ---------------------------------------------------------------------------
// E — CPU attribution
// ---------------------------------------------------------------------------

#[test]
fn thread_classes_fold_by_comm_prefix_with_the_mount_suffix_tolerated() {
    use squeezefs::daemon_cpu::{classify, CLASSES, OTHER};
    for (comm, want) in [
        ("fuse3-tpc12", "fuse3-tpc"),
        ("fuse3-tpc511m7", "fuse3-tpc"),
        ("f3-ur0", "fuse3-ur"),
        ("f3-ur4-7m2", "fuse3-ur"),
        ("f3-ur-watch", "fuse3-ur"),
        ("sqz-ipc-svc0", "sqz-ipc-svc"),
        ("sqz-ipc-svc63ma", "sqz-ipc-svc"),
        ("sqz-ipc-dd7", "sqz-ipc-dd"),
        ("sqz-meta1m3", "sqz-meta"),
        ("sqz-jrnl0", "sqz-jrnl"),
        ("sqz-jrnl3m2", "sqz-jrnl"),
        ("sqz-blk12", "sqz-blk"),
        ("sqz-nvme3m1", "sqz-nvme"),
        ("sqz-zcrx-rd2", "sqz-zcrx"),
        ("sqz-ipc-reap", OTHER),
        ("sqz-timer", "sqz-timer"),
        ("sqz-timerm3", "sqz-timer"),
        ("squeezefs", OTHER),
        ("", OTHER),
    ] {
        assert_eq!(classify(comm), want, "comm {comm:?}");
    }
    // Every class name is exported even when no thread of it is alive.
    let sample = squeezefs::daemon_cpu::sample();
    for c in CLASSES
        .iter()
        .map(|c| c.class)
        .chain(std::iter::once(OTHER))
    {
        assert!(
            sample.by_class.iter().any(|(k, _)| *k == c),
            "class {c} exported"
        );
    }
}

/// Monotone non-decreasing across samples (total AND every class), class
/// sum ≤ total, and a known-CPU-burning thread moves exactly its class;
/// a retired thread keeps its last sample (the class never falls).
#[test]
fn cpu_sample_is_monotone_class_sum_bounded_and_attributes_a_burning_thread() {
    use squeezefs::daemon_cpu::sample;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let s0 = sample();
    let get = |s: &squeezefs::daemon_cpu::CpuSample, c: &str| {
        s.by_class
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, v)| *v)
            .expect("class present")
    };
    let class_sum =
        |s: &squeezefs::daemon_cpu::CpuSample| -> u64 { s.by_class.iter().map(|(_, v)| *v).sum() };
    assert!(class_sum(&s0) <= s0.total_ns, "Σ classes ≤ total");

    let stop = Arc::new(AtomicBool::new(false));
    let burner = {
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("fuse3-tpc99".into())
            .spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
            .unwrap()
    };
    std::thread::sleep(Duration::from_millis(60));
    let s1 = sample(); // burner alive
    stop.store(true, Ordering::Relaxed);
    burner.join().unwrap();
    let s2 = sample(); // burner retired

    assert!(
        s1.total_ns >= s0.total_ns && s2.total_ns >= s1.total_ns,
        "total monotone"
    );
    for (k, v0) in &s0.by_class {
        assert!(get(&s1, k) >= *v0, "class {k} monotone (s0→s1)");
        assert!(
            get(&s2, k) >= get(&s1, k),
            "class {k} monotone (s1→s2, retired kept)"
        );
    }
    assert!(class_sum(&s1) <= s1.total_ns, "Σ classes ≤ total (s1)");
    assert!(class_sum(&s2) <= s2.total_ns, "Σ classes ≤ total (s2)");
    let burned = get(&s1, "fuse3-tpc") - get(&s0, "fuse3-tpc");
    assert!(
        burned >= 20_000_000,
        "a 60 ms burner named fuse3-tpc99 moves its class by ≥ 20 ms (saw {burned} ns)"
    );
    assert!(
        get(&s2, "fuse3-tpc") >= get(&s1, "fuse3-tpc"),
        "the retired burner's sample is kept"
    );

    let json = s2.by_class_json();
    assert!(
        json["fuse3-tpc"].as_u64().is_some(),
        "JSON is {{class: ns}}"
    );
}

/// The stats inode carries both words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_daemon_cpu_words() {
    let _g = serial().await;
    let h = make([0x41; 16], "audit_e_stats").await;
    let reply =
        h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let m = &stats["metrics"];
    let total = m["daemon_cpu_ns"].as_u64().expect("daemon_cpu_ns is u64");
    let by = m["daemon_cpu_ns_by_class"]
        .as_object()
        .expect("daemon_cpu_ns_by_class is an object");
    let sum: u64 = by.values().map(|v| v.as_u64().expect("ns")).sum();
    assert!(sum <= total, "Σ classes ({sum}) ≤ total ({total})");
    assert!(by.contains_key("other"), "the residual class is exported");
}

// ---------------------------------------------------------------------------
// R-1 — the sqz_time registry gauges (candidate finding 48's instrument)
// ---------------------------------------------------------------------------

/// Both registries (squeezefs-ipc's and the fuse3 fork's `#[path]` copy)
/// export arms / tombstones / occupancy on the stats inode, each face
/// reading ITS OWN registry's words — the two must stay distinguishable,
/// because the transport's ticked parks land on the fuse3 registry while
/// the daemon's land on squeezefs-ipc's. Background cadence loops may arm
/// timers between reads, so every check is an interval, never an equality
/// against a quiescent counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_both_timer_registries_reading_their_own_words() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let _g = serial().await;
    let h = make([0x51; 16], "audit_r1_timer").await;
    let read = || async {
        let reply =
            h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
                .await
                .expect("read stats inode");
        let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
        stats["metrics"].clone()
    };
    let word = |m: &serde_json::Value, k: &str| -> u64 {
        m[k].as_u64()
            .unwrap_or_else(|| panic!("{k} is a u64 gauge on the stats inode"))
    };
    const KEYS: [&str; 8] = [
        "timer_arms",
        "timer_tombstones_skipped",
        "timer_heap_entries",
        "timer_live_sleeps",
        "transport_timer_arms",
        "transport_timer_tombstones_skipped",
        "transport_timer_heap_entries",
        "transport_timer_live_sleeps",
    ];

    // Each exported word is bracketed by its OWN static read before and
    // after the inode read (monotone counters ⇒ the export lies inside).
    let ipc_a = squeezefs_ipc::sqz_time::TIMER_ARMS.load(Ordering::Relaxed);
    let f3_a = fuse3::sqz_time::TIMER_ARMS.load(Ordering::Relaxed);
    let m0 = read().await;
    let ipc_b = squeezefs_ipc::sqz_time::TIMER_ARMS.load(Ordering::Relaxed);
    let f3_b = fuse3::sqz_time::TIMER_ARMS.load(Ordering::Relaxed);
    for k in KEYS {
        let _ = word(&m0, k);
    }
    assert!(
        (ipc_a..=ipc_b).contains(&word(&m0, "timer_arms")),
        "timer_arms reads the squeezefs-ipc registry"
    );
    assert!(
        (f3_a..=f3_b).contains(&word(&m0, "transport_timer_arms")),
        "transport_timer_arms reads the fuse3 registry"
    );

    // A WON timeout on each face: arms +≥1 on that face now, and its heap
    // entry pops as a tombstone once the deadline passes.
    let r = squeezefs_ipc::sqz_time::timeout(Duration::from_millis(20), async { 7u32 }).await;
    assert_eq!(r, Ok(7));
    let r = fuse3::sqz_time::timeout(Duration::from_millis(20), async { 9u32 }).await;
    assert_eq!(r, Ok(9));
    let m1 = read().await;
    assert!(
        word(&m1, "timer_arms") > word(&m0, "timer_arms"),
        "a squeezefs-ipc arm moves timer_arms"
    );
    assert!(
        word(&m1, "transport_timer_arms") > word(&m0, "transport_timer_arms"),
        "a fuse3 arm moves transport_timer_arms"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    let m2 = read().await;
    assert!(
        word(&m2, "timer_tombstones_skipped") > word(&m0, "timer_tombstones_skipped"),
        "the won squeezefs-ipc timeout's heap entry pops as a tombstone"
    );
    assert!(
        word(&m2, "transport_timer_tombstones_skipped")
            > word(&m0, "transport_timer_tombstones_skipped"),
        "the won fuse3 timeout's heap entry pops as a tombstone"
    );
    // Occupancy words are gauges: a held sleep on either face is visible
    // in BOTH its heap and live counts while it lives.
    let held = fuse3::sqz_time::sleep(Duration::from_secs(3600));
    let m3 = read().await;
    assert!(
        word(&m3, "transport_timer_live_sleeps") >= 1
            && word(&m3, "transport_timer_heap_entries") >= 1,
        "a live fuse3 sleep shows in the transport occupancy gauges"
    );
    drop(held);
}

// ---------------------------------------------------------------------------
// R-2 step 1 — the reap-gap family + the fast-dispatch engagement pair
// (`.benchmarks/2026-09-03-4k-random-attribution.md` §5/§8: the K1
// `send → transport_recv` residue owned the kern tail with no instrument
// that could say whether the queue worker was busy, parked or
// descheduled; the fast-dispatch pair is the lever's engagement).
// ---------------------------------------------------------------------------

/// `transport_reap_gap_ns` rides the stats inode UNGATED with exactly the
/// three phases, in the one histogram shape, and a recorded span moves
/// exactly its phase's exact words (the family folds across the fuse3
/// shards like the transport tables — a weighted `blind_cqe` record adds
/// `n` to the count and `n × span` to the sum in ONE record).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_the_reap_gap_family_exact() {
    use fuse3::{reap_gap_snapshot, reap_phase_record_n, ReapPhase};
    let _g = serial().await;
    let h = make([0x52; 16], "audit_r2_reap").await;
    let read = || async {
        let reply =
            h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
                .await
                .expect("read stats inode");
        let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
        stats["metrics"].clone()
    };
    let m0 = read().await;
    let fam0 = m0
        .get("transport_reap_gap_ns")
        .expect("stats inode metrics must carry transport_reap_gap_ns UNGATED");
    let obj = fam0.as_object().expect("family object");
    assert_eq!(
        obj.keys().collect::<Vec<_>>(),
        ["blind", "blind_cqe", "park"],
        "exactly the three reap phases, in order"
    );
    for (phase, hh) in obj {
        for k in HIST_KEYS {
            assert!(hh.get(k).is_some(), "phase {phase} lacks {k}");
        }
        assert_eq!(
            bucket_sum(hh),
            hist_count(hh),
            "phase {phase}: Σ buckets ≡ count"
        );
    }
    // The export reads the fuse3 fold: a direct record on each phase
    // moves that phase's words by exactly the recorded weight/span.
    let snap0 = reap_gap_snapshot();
    reap_phase_record_n(ReapPhase::Blind, 12_345, 1);
    reap_phase_record_n(ReapPhase::BlindCqe, 12_345, 9);
    reap_phase_record_n(ReapPhase::Park, 777_000, 1);
    let m1 = read().await;
    let fam1 = &m1["transport_reap_gap_ns"];
    let snap1 = reap_gap_snapshot();
    let (bc, bs) = phase_words(fam1, "blind");
    let (cc, cs) = phase_words(fam1, "blind_cqe");
    let (pc, ps) = phase_words(fam1, "park");
    // Live workers may record concurrently (none in this harness — no
    // mount), so the pins are exact deltas against the fold read around
    // the inode read.
    assert_eq!(bc - snap0[0].count, snap1[0].count - snap0[0].count);
    assert_eq!(snap1[0].count - snap0[0].count, 1, "one blind sample");
    assert_eq!(bs - snap0[0].sum_ns, 12_345, "blind sum is the exact span");
    assert_eq!(cc - snap0[1].count, 9, "blind_cqe count = the CQE weight");
    assert_eq!(
        cs - snap0[1].sum_ns,
        12_345 * 9,
        "blind_cqe sum = span × weight"
    );
    assert_eq!(pc - snap0[2].count, 1, "one park sample");
    assert_eq!(ps - snap0[2].sum_ns, 777_000, "park sum is the exact span");
}

/// The fast-dispatch engagement pair rides the stats inode as u64
/// counters reading the fuse3 statics (0 in this harness — no armed
/// session; the mechanism's own suites move them).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_the_fast_dispatch_engagement_pair() {
    let _g = serial().await;
    let h = make([0x53; 16], "audit_r2_fastd").await;
    let reply =
        h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let m = &stats["metrics"];
    let serves = m["transport_fast_dispatch_serves"]
        .as_u64()
        .expect("transport_fast_dispatch_serves is a u64 counter");
    let demotes = m["transport_fast_dispatch_demotes"]
        .as_u64()
        .expect("transport_fast_dispatch_demotes is a u64 counter");
    assert_eq!(
        serves,
        fuse3::fast_dispatch_serves(),
        "serves reads the fuse3 static"
    );
    assert_eq!(
        demotes,
        fuse3::fast_dispatch_demotes(),
        "demotes reads the fuse3 static"
    );
}

/// R-4: the spin-before-park ledger rides the stats inode as five u64
/// words reading the fuse3 fold — `absorbed + expired` ≡ the spins run
/// (the closure law), `ns` their summed cost, `refused_busy` the box
/// gauge's refusals, `window_us` the last derived window. 0 across the
/// board in this harness (no armed session).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_the_spin_ledger_reading_the_fuse3_fold() {
    let _g = serial().await;
    let h = make([0x54; 16], "audit_r4_spin").await;
    let reply =
        h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let m = &stats["metrics"];
    let (absorbed, expired, ns, refused, window_us) = fuse3::transport_spin_stats();
    for (key, want) in [
        ("transport_spin_absorbed", absorbed),
        ("transport_spin_expired", expired),
        ("transport_spin_ns", ns),
        ("transport_spin_refused_busy", refused),
        ("transport_spin_window_us", window_us),
    ] {
        assert_eq!(
            m[key]
                .as_u64()
                .unwrap_or_else(|| panic!("{key} is a u64 word")),
            want,
            "{key} reads the fuse3 fold"
        );
    }
}

// ---------------------------------------------------------------------------
// R-3 — the zc direct-leg bridge decomposition (`zc_bridge_phase_ns`)
// ---------------------------------------------------------------------------

/// The 4 KiB-random attribution pass measured the kern READ's
/// `keys_resolved → block_fetched` at 166 µs with the device at 40 —
/// ≈ 126 µs of bridge software with no per-op split (the note's
/// instrument gap #2). The family that splits it: five phases in the
/// documented order, exact-sum, folding across the worker (three phases)
/// and the lane (two) shards, rendered through the one histogram shape
/// on the stats inode; and the two stages that anchor its op-trace
/// spans (`bridge_sent`, `bridge_taken`) sit in the funnel block of the
/// vocabulary, between `fill_done` and the write pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zc_bridge_family_exports_five_exact_phases_and_its_stages_are_in_the_table() {
    use fuse3::{zc_bridge_phase_record_ns, zc_bridge_phase_snapshot, ZcBridgePhase};
    use squeezefs::op_trace::Stage;
    let _g = serial().await;

    let names: Vec<&str> = zc_bridge_phase_snapshot().iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        ["msg_hop", "sq_wait", "device_cq", "wake_hop", "total"],
        "phase order is the export's contract"
    );

    // Worker-side phases from N "worker" threads, lane-side phases from
    // M "lane" threads: the fold is exact to the ns per phase.
    const WORKERS: u64 = 3;
    const LANES: u64 = 2;
    const PER: u64 = 100;
    let before = zc_bridge_phase_snapshot();
    let mut hs = Vec::new();
    for t in 0..WORKERS {
        hs.push(std::thread::spawn(move || {
            for i in 0..PER {
                zc_bridge_phase_record_ns(ZcBridgePhase::MsgHop, 5_000 + t * 100 + i);
                zc_bridge_phase_record_ns(ZcBridgePhase::SqWait, 2_000 + t * 100 + i);
                zc_bridge_phase_record_ns(ZcBridgePhase::DeviceCq, 40_000 + t * 100 + i);
            }
        }));
    }
    for t in 0..LANES {
        hs.push(std::thread::spawn(move || {
            for i in 0..PER {
                zc_bridge_phase_record_ns(ZcBridgePhase::WakeHop, 9_000 + t * 100 + i);
                zc_bridge_phase_record_ns(ZcBridgePhase::Total, 60_000 + t * 100 + i);
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let after = zc_bridge_phase_snapshot();
    let want = |threads: u64, base: u64| -> (u64, u64) {
        let sum: u64 = (0..threads)
            .flat_map(|t| (0..PER).map(move |i| base + t * 100 + i))
            .sum();
        (threads * PER, sum)
    };
    for (phase, threads, base) in [
        (ZcBridgePhase::MsgHop, WORKERS, 5_000),
        (ZcBridgePhase::SqWait, WORKERS, 2_000),
        (ZcBridgePhase::DeviceCq, WORKERS, 40_000),
        (ZcBridgePhase::WakeHop, LANES, 9_000),
        (ZcBridgePhase::Total, LANES, 60_000),
    ] {
        let pi = phase as usize;
        let (n, sum) = want(threads, base);
        assert_eq!(after[pi].count - before[pi].count, n, "{:?} count", phase);
        assert_eq!(
            after[pi].sum_ns - before[pi].sum_ns,
            sum,
            "{:?} sum_ns",
            phase
        );
        let bucket_delta: u64 = after[pi]
            .buckets
            .iter()
            .zip(before[pi].buckets.iter())
            .map(|(a, b)| a - b)
            .sum();
        assert_eq!(bucket_delta, n, "{:?}: Σ buckets ≡ count", phase);
    }

    // The daemon renders the family through the one histogram shape,
    // and the stats inode carries it ungated.
    let json = squeezefs::fuse_client::zc_bridge_phase_json();
    for (pi, name) in names.iter().enumerate() {
        let h = &json[*name];
        for k in HIST_KEYS {
            assert!(h.get(k).is_some(), "zc_bridge_phase_ns.{name} lacks {k}");
        }
        assert_eq!(hist_count(h), after[pi].count);
        assert_eq!(hist_sum_ns(h), after[pi].sum_ns);
    }
    let h = make([0x53; 16], "audit_r3_bridge").await;
    let reply =
        h.fs.read(h.req, squeezefs::fuse_client::STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value = serde_json::from_slice(&reply.data).expect("stats JSON");
    let fam = &stats["metrics"]["zc_bridge_phase_ns"];
    assert!(fam.is_object(), "zc_bridge_phase_ns rides the stats inode");
    for name in &names {
        assert!(
            hist_count(&fam[*name]) >= after[0].count.min(after[3].count),
            "stats inode carries {name} with its exact words"
        );
    }

    // The op-trace anchors: both stages exist, name-stable, and sit in
    // the device-funnel block (after `fill_done`, before the write
    // pipeline's first stage) so a chain sorts them beside dev_submit /
    // dev_complete.
    assert_eq!(Stage::BridgeSent.name(), "bridge_sent");
    assert_eq!(Stage::BridgeTaken.name(), "bridge_taken");
    assert!((Stage::FillDone as u16) < (Stage::BridgeSent as u16));
    assert!((Stage::BridgeSent as u16) < (Stage::BridgeTaken as u16));
    assert!((Stage::BridgeTaken as u16) < (Stage::WriteAdmitted as u16));
    assert_eq!(
        Stage::from_u16(Stage::BridgeSent as u16),
        Some(Stage::BridgeSent)
    );
    assert_eq!(
        Stage::from_u16(Stage::BridgeTaken as u16),
        Some(Stage::BridgeTaken)
    );
}
