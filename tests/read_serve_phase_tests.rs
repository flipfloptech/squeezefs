//! Read-serve per-phase residence instrumentation — the 2026-08-01
//! serve-latency decomposition campaign's read-side instrument
//! (`.benchmarks/2026-08-01-serve-decomposition.md`), the read twin of
//! `write_pipeline_phase_ns` (`tests/write_pipeline_phase_tests.rs`).
//!
//! Field conviction (read-lane campaign, `.benchmarks/2026-08-01-read-lane.md`
//! §7): at the EXA cold-read shape the FS serve is Little-closed at
//! **10.4 ms/op vs 6.2 ms raw** at identical in-flight bytes — the ~4 ms/op
//! residual is a PER-OP LATENCY CHAIN, not a concurrency term, and no
//! instrument could name which leg owns it (FUSE arrival→dispatch, routing/
//! meta, fetch issue→DMA, tier legs, slice-out, reply). The instrument:
//! ALWAYS-ON histogram families (the `write_pipeline_phase_ns` pattern —
//! deliberately NOT `SQUEEZEFS_OP_PROFILE`-gated; one `Instant` read + one
//! relaxed `fetch_add` per phase boundary actually crossed, and the field
//! needs the decomposition on production mounts without a remount-to-arm
//! round trip):
//!
//! - **`read_serve_phase_ns`** (per data-read op): prelude → meta_resolve →
//!   key_resolve → classify_probe → {sf_wait | block_fetch} →
//!   binding_check → slice_out → post_validate → total.
//! - **`read_fill_phase_ns`** (per block fill / device read): dev_queue →
//!   dev_service → fetch_dma → decode → admission → deposit → fill_total.
//! - **`read_transport_phase_ns`** (per FUSE_READ through the over-uring
//!   transport, fuse3-side): queue_wait → dispatch_lag → reply_commit →
//!   transport_total. Shape/recording pinned here; engagement needs a real
//!   mount (the field capture is the venue).
//!
//! Containment map (the no-unexplained-residue law):
//!   total ≈ prelude + meta_resolve + key_resolve + classify_probe
//!           + (warm: slice_out + binding_check | cold: block_fetch + slice_out)
//!           + post_validate
//!   block_fetch ⊇ {sf_wait (waiter) | fill_total + binding_check (primary)}
//!   fill_total ≈ fetch_dma + decode + admission + deposit;
//!   fetch_dma ⊇ dev_queue + dev_service (+ oneshot-wake residue)
//!   transport_total ≈ queue_wait + dispatch_lag + [handler total] + reply_commit
//!
//! RED against dev da837ca: none of the three families exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    read_fill_phase_json, read_fill_phase_record, read_serve_phase_json, read_serve_phase_record,
    read_transport_phase_json, ReadFillPhase, ReadServePhase, SqueezefsFilesystem, METRICS,
    STATS_INODE,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 512 KiB: > 256 KiB — the production large-block population (hot-tier
/// landing, no RAM-LRU shortcut, ≥ 64 KiB publish branch), the churn
/// suite's geometry.
const BS: u64 = 524_288;

/// The histogram families are process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const SERVE_PHASES: [&str; 10] = [
    "prelude",
    "meta_resolve",
    "key_resolve",
    "classify_probe",
    "sf_wait",
    "block_fetch",
    "binding_check",
    "slice_out",
    "post_validate",
    "total",
];

const FILL_PHASES: [&str; 7] = [
    "dev_queue",
    "dev_service",
    "fetch_dma",
    "decode",
    "admission",
    "deposit",
    "fill_total",
];

const TRANSPORT_PHASES: [&str; 5] = [
    "queue_wait",
    "dispatch_lag",
    "reply_commit",
    "transport_total",
    // 2026-08-04 kmbuf campaign: COMMIT-carrying ring-flush syscall
    // duration — per-FLUSH sampled (saturated/wait-free flushes only),
    // NOT per-op; the venue of the kernel's commit-side copy machinery,
    // so the killed FR_LOCKED/GUP term shows as this phase's
    // before/after delta on kmbuf A/Bs.
    "commit_flush",
];

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

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The read-tier churn suite's harness shape: real v3 meta backend, real
/// striped write/read paths, 512 KiB blocks, request-driven pinned (no R2
/// pipeline fetches, no R3 ranged windows — their fills would shift the
/// per-op phase counts without violating any contract; their own counting
/// disciplines live in their own suites).
async fn make(test_id: &str, uuid: [u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Make every mapped block of `ino` COLD (the churn suite's helper): fsync,
/// then purge every read-tier retention of every current block key.
async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

// ---------------------------------------------------------------------------
// Contract 1 — the three families exist always-on with exactly the named
// keys and ride the stats inode UNGATED (no SQUEEZEFS_OP_PROFILE arm).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_families_are_always_on_with_exact_keys() {
    let _g = serial().await;
    assert!(
        std::env::var("SQUEEZEFS_OP_PROFILE").is_err(),
        "fixture premise: the profile rig must be OFF — these families are \
         deliberately always-on"
    );
    for (family, phases) in [
        (read_serve_phase_json(), &SERVE_PHASES[..]),
        (read_fill_phase_json(), &FILL_PHASES[..]),
        (read_transport_phase_json(), &TRANSPORT_PHASES[..]),
    ] {
        let obj = family.as_object().expect("family must be an object");
        assert_eq!(obj.len(), phases.len(), "exactly the named phases: {obj:?}");
        for p in phases {
            let _ = phase_count(&family, p); // key exists, histogram-shaped
        }
    }

    // Stats-inode surface, ungated.
    let h = make("rsp_stats_surface", *b"rsp-stats-vol-v3").await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let metrics = stats.get("metrics").expect("metrics object");
    for (key, phases) in [
        ("read_serve_phase_ns", &SERVE_PHASES[..]),
        ("read_fill_phase_ns", &FILL_PHASES[..]),
        ("read_transport_phase_ns", &TRANSPORT_PHASES[..]),
    ] {
        let fam = metrics
            .get(key)
            .unwrap_or_else(|| panic!("stats inode metrics must carry {key} UNGATED"));
        for p in phases {
            let _ = phase_count(fam, p);
        }
    }
    // The in-place-reply engagement gauge (the P2 wiring finding) rides
    // along: any armed-session field capture verifies engagement from it.
    let v = metrics
        .get("fuse3_read_inplace_replies")
        .expect("stats inode metrics must carry fuse3_read_inplace_replies");
    assert!(v.is_u64(), "fuse3_read_inplace_replies is a counter");
}

// ---------------------------------------------------------------------------
// Contract 2 — a real cold striped read drives the full serve + fill chain;
// a warm re-read of the same block leaves the cold-only phases flat.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_striped_read_records_the_full_serve_chain() {
    let _g = serial().await;
    let h = make("rsp_cold_chain", *b"rsp-cold-vol-v3!").await;

    let ino = create(&h, "cold_probe").await;
    write_at(&h, ino, 0, &vec![0xA7u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xB8u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    assert!(map.contains_key(&0), "fixture: block 0 mapped (striped)");

    let serve0 = read_serve_phase_json();
    let fill0 = read_fill_phase_json();
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);

    // ONE cold sub-block read of block 0.
    let d = read_at(&h, ino, 0, 128 * 1024).await;
    assert_eq!(d.len(), 128 * 1024);
    assert!(d.iter().all(|&x| x == 0xA7), "cold read content");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        1,
        "fixture premise: exactly one device fetch (the cold chain)"
    );

    let serve1 = read_serve_phase_json();
    let fill1 = read_fill_phase_json();
    for p in [
        "prelude",
        "meta_resolve",
        "key_resolve",
        "classify_probe",
        "block_fetch",
        "binding_check",
        "slice_out",
        "post_validate",
        "total",
    ] {
        assert!(
            phase_count(&serve1, p) > phase_count(&serve0, p),
            "cold read must record serve phase {p} (no blind segment in the \
             serve chain)"
        );
    }
    assert_eq!(
        phase_count(&serve1, "sf_wait"),
        phase_count(&serve0, "sf_wait"),
        "a lone cold reader is the primary — sf_wait must stay flat"
    );
    for p in FILL_PHASES {
        assert!(
            phase_count(&fill1, p) > phase_count(&fill0, p),
            "cold fill must record fill phase {p} (no blind segment in the \
             fill chain)"
        );
    }

    // Warm re-read (different sub-range of the same block): tier serve —
    // the cold-only phases stay flat, the warm chain still records.
    let d = read_at(&h, ino, 128 * 1024, 128 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA7), "warm read content");
    let serve2 = read_serve_phase_json();
    let fill2 = read_fill_phase_json();
    assert_eq!(
        phase_count(&serve2, "block_fetch"),
        phase_count(&serve1, "block_fetch"),
        "warm tier serve must not enter the cold fetch leg"
    );
    for p in FILL_PHASES {
        assert_eq!(
            phase_count(&fill2, p),
            phase_count(&fill1, p),
            "warm tier serve must not record fill phase {p}"
        );
    }
    for p in ["prelude", "classify_probe", "slice_out", "total"] {
        assert!(
            phase_count(&serve2, p) > phase_count(&serve1, p),
            "warm serve still records phase {p}"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — single-flight cohort waiters record sf_wait.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cohort_waiters_record_sf_wait() {
    let _g = serial().await;
    let h = make("rsp_cohort", *b"rsp-cohortvol-v3").await;

    let ino = create(&h, "cohort_probe").await;
    // Two blocks: multi-block files take the striped layout on flush
    // (a lone 512 KiB blob stays staged); all four readers target block 0.
    write_at(&h, ino, 0, &vec![0xC5u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xD6u8; BS as usize]).await;
    let map = make_cold(&h, ino).await;
    assert!(map.contains_key(&0), "fixture: block 0 mapped");

    let serve0 = read_serve_phase_json();
    let w0 = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);

    // Four concurrent cold resolvers of one block: one primary + three
    // waiters (the churn suite's phase-E shape, deterministic on the
    // fetch window).
    let reads = (0..4u64).map(|i| read_at(&h, ino, i * 128 * 1024, 128 * 1024));
    let results = futures::future::join_all(reads).await;
    for (i, d) in results.iter().enumerate() {
        assert_eq!(d.len(), 128 * 1024, "cohort slice {i} length");
        assert!(d.iter().all(|&x| x == 0xC5), "cohort slice {i} content");
    }

    let w1 = METRICS
        .singleflight_waiter_result_serves
        .load(Ordering::Relaxed);
    assert_eq!(w1 - w0, 3, "fixture premise: three cohort waiters");
    let serve1 = read_serve_phase_json();
    assert!(
        phase_count(&serve1, "sf_wait") - phase_count(&serve0, "sf_wait") >= 3,
        "every cohort waiter must record its sf_wait span — the deep-qd \
         cohort-wait term must be nameable from the histogram"
    );
}

// ---------------------------------------------------------------------------
// Contract 4 — recording is phase-exact (pure), both root families.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recording_lands_in_exactly_the_named_phase() {
    let _g = serial().await;
    let before = read_serve_phase_json();
    read_serve_phase_record(
        ReadServePhase::SliceOut,
        std::time::Instant::now() - std::time::Duration::from_micros(100),
    );
    let after = read_serve_phase_json();
    for p in SERVE_PHASES {
        let delta = phase_count(&after, p) - phase_count(&before, p);
        let want = u64::from(p == "slice_out");
        assert_eq!(
            delta, want,
            "one slice_out span recorded ⇒ exactly the slice_out key moves \
             (phase {p} moved by {delta})"
        );
    }

    let before = read_fill_phase_json();
    read_fill_phase_record(
        ReadFillPhase::Decode,
        std::time::Instant::now() - std::time::Duration::from_micros(100),
    );
    let after = read_fill_phase_json();
    for p in FILL_PHASES {
        let delta = phase_count(&after, p) - phase_count(&before, p);
        let want = u64::from(p == "decode");
        assert_eq!(
            delta, want,
            "one decode span recorded ⇒ exactly the decode key moves \
             (phase {p} moved by {delta})"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 5 — the fuse3 transport family: shape, phase-exact recording,
// and the shared bucket core (label drift = red test).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_family_shape_and_bucket_tie() {
    let _g = serial().await;

    // Phase-exact recording through the fuse3-side rig.
    let snap0 = fuse3::read_transport_phase_snapshot();
    fuse3::read_transport_phase_record(
        fuse3::TransportPhase::QueueWait,
        std::time::Duration::from_micros(100),
    );
    let snap1 = fuse3::read_transport_phase_snapshot();
    assert_eq!(snap1.len(), TRANSPORT_PHASES.len());
    for (i, name) in TRANSPORT_PHASES.iter().enumerate() {
        assert_eq!(snap1[i].0, *name, "transport phase order/name");
        let d: u64 = snap1[i].1.iter().sum::<u64>() - snap0[i].1.iter().sum::<u64>();
        let want = u64::from(*name == "queue_wait");
        assert_eq!(
            d, want,
            "one queue_wait span recorded ⇒ exactly the queue_wait phase moves"
        );
    }

    // Bucket tie: fuse3's histograms and the root's LatencyHistogram must
    // bucket identically — both delegate to the shared latency core, so
    // the JSON conversion below is label-for-bucket exact.
    let fam = read_transport_phase_json();
    for name in TRANSPORT_PHASES {
        let hist = fam.get(name).expect("phase present in JSON");
        let obj = hist.as_object().expect("bucketed object");
        assert_eq!(
            obj.len(),
            squeezefs::latency_core::LATENCY_BUCKET_LABELS.len(),
            "transport histograms carry the full standard bucket set"
        );
        for label in squeezefs::latency_core::LATENCY_BUCKET_LABELS {
            assert!(
                obj.contains_key(label),
                "standard bucket label {label} present"
            );
        }
    }
    // The 100 µs span above lands in the same bucket the root histogram
    // would put it in (the shared-core index).
    let idx = squeezefs::latency_core::latency_bucket_index(100);
    let qsnap = fuse3::read_transport_phase_snapshot();
    assert!(
        qsnap[0].1[idx] >= 1,
        "the recorded 100 µs span must land in shared-core bucket {idx}"
    );
}
