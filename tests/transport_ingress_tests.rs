//! Transport ingress economy campaign — Phase 1: the WRITE transport
//! residence family (`write_transport_phase_ns`), the write twin of
//! `read_transport_phase_ns` (`.benchmarks/2026-08-01-serve-decomposition.md`
//! §6 item 3: "extend the transport family to WRITE — the stamps exist,
//! the family is READ-gated by one opcode check").
//!
//! Why this exists: the write wall's ~5.3 ms pre-handler leg is a STATED
//! INFERENCE in the decomposition note (§4.1 — "the same dispatch machinery
//! reads measured at 3.25 ms daemon-side", inferred from shared machinery,
//! never measured). This family converts that inference into measurement
//! and is the campaign's write-side acceptance instrument.
//!
//! Phases (identical semantics to the READ family — one shared
//! `TransportPhase` enum, two op-class tables):
//!
//! - `queue_wait` — ring CQE reaped (inbound push) → session dispatch
//!   pop (the WRITE stamps ride the same dispatch loop).
//! - `dispatch_lag` — dispatch → the write-handler future's first poll.
//! - `reply_commit` — `fs.write` returned → the reply handed to the
//!   transport.
//! - `transport_total` — inbound push → reply committed; `fio clat −
//!   transport_total` = the kernel-side residue for WRITE, statable by
//!   subtraction (the §4.1 split).
//!
//! Always-on (the `write_pipeline_phase_ns` cost contract: ≤ 4 `Instant`
//! reads per WRITE actually delivered over the armed uring transport);
//! buckets through the SHARED `latency_core`, so the write table composes
//! bucket-for-bucket with `fuse_op_phase_ns[write]` and
//! `write_pipeline_phase_ns`. Error replies deliberately record nothing.
//! Engagement (every armed-session WRITE records) is field-verified — a
//! pure unit test cannot exercise an armed session (the read family's
//! standing repro-port exception).
//!
//! RED against dev 3cd528b: `fuse3::TransportPhase`,
//! `fuse3::write_transport_phase_{record,snapshot}` and the root
//! `write_transport_phase_json` / stats-inode key do not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{write_transport_phase_json, SqueezefsFilesystem, STATS_INODE};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const TRANSPORT_PHASES: [&str; 4] = [
    "queue_wait",
    "dispatch_lag",
    "reply_commit",
    "transport_total",
];

/// The histogram families are process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Sum of one phase histogram's buckets (= spans recorded).
fn phase_count(family: &serde_json::Value, phase: &str) -> u64 {
    family
        .get(phase)
        .unwrap_or_else(|| panic!("phase key {phase} missing from family: {family}"))
        .as_object()
        .expect("phase histogram must be a bucket object")
        .values()
        .map(|v| v.as_u64().expect("bucket counts are u64"))
        .sum()
}

fn read_family_snapshot_sums() -> Vec<(String, u64)> {
    fuse3::read_transport_phase_snapshot()
        .iter()
        .map(|(name, buckets)| ((*name).to_string(), buckets.iter().sum::<u64>()))
        .collect()
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The read_serve_phase suite's harness shape: real v3 meta backend, real
/// router stack — enough to read the stats inode.
async fn make(test_id: &str, uuid: [u8; 16]) -> H {
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
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
    let routed: Arc<RoutedMetaBackend> = {
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
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — the WRITE transport family exists with exactly the READ
// family's phase set (names AND order: the two tables must compose in one
// analyzer), recording is phase-exact, and the write family is INDEPENDENT
// of the read family (a WRITE span must never pollute the read table — the
// read acceptance numbers ride on it).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_family_shape_phase_exact_and_independent() {
    let _g = serial().await;

    let read_before = read_family_snapshot_sums();
    let snap0 = fuse3::write_transport_phase_snapshot();
    fuse3::write_transport_phase_record(
        fuse3::TransportPhase::QueueWait,
        std::time::Duration::from_micros(100),
    );
    let snap1 = fuse3::write_transport_phase_snapshot();

    assert_eq!(snap1.len(), TRANSPORT_PHASES.len());
    for (i, name) in TRANSPORT_PHASES.iter().enumerate() {
        assert_eq!(
            snap1[i].0, *name,
            "write transport phase order/name must mirror the read family"
        );
        let d: u64 = snap1[i].1.iter().sum::<u64>() - snap0[i].1.iter().sum::<u64>();
        let want = u64::from(*name == "queue_wait");
        assert_eq!(
            d, want,
            "one write queue_wait span recorded ⇒ exactly the write \
             queue_wait phase moves (phase {name} moved by {d})"
        );
    }

    // Independence: the read table did not move.
    assert_eq!(
        read_family_snapshot_sums(),
        read_before,
        "recording a WRITE span must never move the READ transport family"
    );

    // And the reverse: a read span leaves the write table flat.
    let wsnap0 = fuse3::write_transport_phase_snapshot();
    fuse3::read_transport_phase_record(
        fuse3::TransportPhase::DispatchLag,
        std::time::Duration::from_micros(100),
    );
    let wsnap1 = fuse3::write_transport_phase_snapshot();
    for i in 0..TRANSPORT_PHASES.len() {
        assert_eq!(
            wsnap1[i].1.iter().sum::<u64>(),
            wsnap0[i].1.iter().sum::<u64>(),
            "recording a READ span must never move the WRITE transport family"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 2 — the root JSON payload carries the full shared-core bucket
// set (the fuse3 rig buckets through the same latency_core, label-for-
// bucket exact), and a recorded span lands in the shared-core bucket.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_family_json_buckets_through_the_shared_core() {
    let _g = serial().await;

    fuse3::write_transport_phase_record(
        fuse3::TransportPhase::TransportTotal,
        std::time::Duration::from_micros(100),
    );

    let fam = write_transport_phase_json();
    for name in TRANSPORT_PHASES {
        let hist = fam.get(name).expect("phase present in JSON");
        let obj = hist.as_object().expect("bucketed object");
        assert_eq!(
            obj.len(),
            squeezefs::latency_core::LATENCY_BUCKET_LABELS.len(),
            "write transport histograms carry the full standard bucket set"
        );
        for label in squeezefs::latency_core::LATENCY_BUCKET_LABELS {
            assert!(
                obj.contains_key(label),
                "standard bucket label {label} present"
            );
        }
    }

    let idx = squeezefs::latency_core::latency_bucket_index(100);
    let snap = fuse3::write_transport_phase_snapshot();
    let total_row = snap
        .iter()
        .find(|(n, _)| *n == "transport_total")
        .expect("transport_total phase");
    assert!(
        total_row.1[idx] >= 1,
        "the recorded 100 µs span must land in shared-core bucket {idx}"
    );
}

// ---------------------------------------------------------------------------
// Contract 3 — the stats inode carries `write_transport_phase_ns` UNGATED
// (always-on: the field reads the write decomposition off production
// mounts without a remount-to-arm round trip), alongside the read family.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_inode_carries_write_transport_family_ungated() {
    let _g = serial().await;
    assert!(
        std::env::var("SQUEEZEFS_OP_PROFILE").is_err(),
        "fixture premise: the profile rig must be OFF — the family is \
         deliberately always-on"
    );

    let h = make("wtp_stats_surface", *b"wtp-stats-vol-v3").await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let metrics = stats.get("metrics").expect("metrics object");
    for key in ["read_transport_phase_ns", "write_transport_phase_ns"] {
        let fam = metrics
            .get(key)
            .unwrap_or_else(|| panic!("stats inode metrics must carry {key} UNGATED"));
        for p in TRANSPORT_PHASES {
            let _ = phase_count(fam, p);
        }
    }
}
