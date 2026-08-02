//! Write in-handler economy campaign (2026-08-01) — Phase 1 instrument
//! contracts: the ~5.2 ms IN-HANDLER leg of the EXA write wall
//! (`.benchmarks/2026-08-01-transport-ingress.md` §6: transport_total
//! 6.68 ms − ingress 1.50 − reply 0.006 ⇒ ~5.2 ms inside `fs.write`)
//! must be attributable to named sub-phases with closed residue BEFORE
//! anything is built. The existing `fuse_write_phase_ns` family
//! (SQUEEZEFS_OP_PROFILE-gated, RW1) covers checkout / sibling_remove /
//! merge_copy / park_spill — but three in-handler spans are UNSTAMPED
//! and cannot be separated from the residue:
//!
//! 1. **lease_acquire** — `get_or_acquire_lease` in the WRITE handler
//!    (cached-hit fast path vs DLM acquisition; rides inside
//!    route_classify today, invisible).
//! 2. **extent_probe** — the `try_extent_park` call in the per-block
//!    future (the patch-ineligible small-write park probe; every
//!    striped block write pays its refusal path).
//! 3. **admit_gate** — the write-pipeline admission park awaited IN the
//!    handler before the completing write's ACK (the always-on
//!    `write_pipeline_phase_ns.admit_wait` twin, recorded per-op in the
//!    write family so the in-handler table composes in ONE family).
//!
//! Contracts (rig ARMED — every test resolves the memoized
//! `SQUEEZEFS_OP_PROFILE` gate ON before any op; the disabled-cost twin
//! stays pinned by `tests/rand_write_rig_off_tests.rs`):
//!
//! 1. **Family shape**: `fuse_write_phase_ns` carries exactly the
//!    twelve phase keys — the nine RW1 phases plus `lease_acquire`,
//!    `extent_probe`, `admit_gate`.
//! 2. **Phase-exact recording** (pure): a span recorded against each
//!    new phase lands in that phase's histogram and no other.
//! 3. **The real striped write path drives the new phases**: a
//!    coverage-complete striped rewrite through the REAL `fs.write`
//!    pipeline path records `lease_acquire` ≥ 1 per WRITE op,
//!    `extent_probe` ≥ 1 per block future, and `admit_gate` ≥ 1 per
//!    admitted (pipelined, coverage-complete) block.
//!
//! RED against dev 076fe81: the three phase keys do not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    write_phase_record, write_profile_phase_json, SqueezefsFilesystem, WritePhase, METRICS,
};
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

/// Arm the RW1/M2 rig for this whole binary BEFORE the memoized gate
/// first resolves (the rand_write_amp_tests convention).
fn rig_on() {
    static ARM: OnceLock<()> = OnceLock::new();
    ARM.get_or_init(|| std::env::set_var("SQUEEZEFS_OP_PROFILE", "1"));
}

/// The histogram family is process-global; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// The twelve in-handler write sub-phases — the closed-residue table.
const PHASES: [&str; 12] = [
    "route_classify",
    "checkout",
    "sibling_remove",
    "merge_copy",
    "seed_fetch",
    "upload_dma",
    "upload_map_merge",
    "park_spill",
    "staging_put",
    "lease_acquire",
    "extent_probe",
    "admit_gate",
];

/// Sum of one phase histogram's buckets (= spans recorded).
fn phase_count(family: &serde_json::Value, phase: &str) -> u64 {
    family
        .get(phase)
        .unwrap_or_else(|| panic!("phase key {phase} missing from fuse_write_phase_ns"))
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

/// Full-FS harness (the write_pipeline_phase_tests shape): real v3 meta
/// backend, real striped write path, real pipeline.
async fn make_harness(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
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
        ..Default::default()
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
// Contract 1 — the armed family carries exactly the twelve phase keys.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_phase_family_carries_the_in_handler_keys() {
    rig_on();
    let _g = serial().await;
    let family = write_profile_phase_json();
    let obj = family
        .as_object()
        .expect("fuse_write_phase_ns must be an object");
    assert_eq!(
        obj.len(),
        PHASES.len(),
        "exactly the twelve write sub-phases (nine RW1 + lease_acquire/\
         extent_probe/admit_gate): {:?}",
        obj.keys().collect::<Vec<_>>()
    );
    for p in PHASES {
        let _ = phase_count(&family, p); // key exists, histogram-shaped
    }
}

// ---------------------------------------------------------------------------
// Contract 2 — recording is phase-exact (pure) for the new phases.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_phase_recording_is_phase_exact() {
    rig_on();
    let _g = serial().await;
    for phase in [
        WritePhase::LeaseAcquire,
        WritePhase::ExtentProbe,
        WritePhase::AdmitGate,
    ] {
        let before = write_profile_phase_json();
        write_phase_record(phase, Some(std::time::Instant::now()));
        let after = write_profile_phase_json();
        let name = match phase {
            WritePhase::LeaseAcquire => "lease_acquire",
            WritePhase::ExtentProbe => "extent_probe",
            WritePhase::AdmitGate => "admit_gate",
            _ => unreachable!(),
        };
        for p in PHASES {
            let delta = phase_count(&after, p) - phase_count(&before, p);
            if p == name {
                assert_eq!(delta, 1, "span recorded against {name} must land in it");
            } else {
                assert_eq!(delta, 0, "span recorded against {name} leaked into {p}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — the real striped write path drives the new phases.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn striped_pipeline_rewrite_records_the_in_handler_phases() {
    rig_on();
    let _g = serial().await;
    // CoW-always A/B lever: the in-place-overwrite default elides no
    // in-handler phase, but pin the venue like the pipeline suite does.
    squeezefs::fuse_client::set_inplace_overwrite(false);
    let h = make_harness("wih_phase_drive").await;

    // Striped fixture: fresh write + fsync + drain, then the measured
    // rewrite pass (the coverage-complete pipelined write-through venue).
    let blocks = 8u64;
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new("wih_phased"),
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

    let before = write_profile_phase_json();
    let wt0 = METRICS.write_through_blocks.load(Ordering::Relaxed);

    // The measured pass: one 8-block coverage-complete rewrite — one
    // WRITE op, 8 block futures, 8 pipelined admissions.
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
        METRICS.write_through_blocks.load(Ordering::Relaxed) - wt0,
        blocks,
        "fixture premise: all {blocks} rewrite blocks ride the pipelined \
         write-through (the admit_gate venue)"
    );

    let after = write_profile_phase_json();
    let grew = |p: &str| phase_count(&after, p) - phase_count(&before, p);
    assert!(
        grew("lease_acquire") >= 1,
        "one WRITE op must record >= 1 lease_acquire span (got {})",
        grew("lease_acquire")
    );
    assert!(
        grew("extent_probe") >= blocks,
        "every striped block future must record its extent_probe span \
         (got {}, want >= {blocks})",
        grew("extent_probe")
    );
    assert!(
        grew("admit_gate") >= blocks,
        "every pipelined coverage-complete block must record its \
         in-handler admit_gate span (got {}, want >= {blocks})",
        grew("admit_gate")
    );
    squeezefs::fuse_client::set_inplace_overwrite(false);
}
