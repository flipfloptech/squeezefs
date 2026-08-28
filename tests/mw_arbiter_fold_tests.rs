//! Finding 26 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`, PR 5
//! acceptance attempt 8 — the FIRST thermally honest venue): the
//! AUTHORITY's fold of shipped-assembly extents consults the range-shared
//! fast-path clauses with its OWN fencing token, so the holder's covering
//! grant — the very custody whose writes the fold is executing by proxy —
//! reads as foreign, and the clause declines its own designed vehicle on
//! EVERY fold pass (`overlay_ineligible_range_shared` ≡ `fold_passes` ≡
//! `extent_parks` per phase on the cloud row: A1 2, B1 15, B2 12; the
//! demoted-region arm answers TRUE for ANY overlap, so the rung-17
//! demotion machinery's own publisher is blocked by the arm that exists
//! to route work TO it).
//!
//! The law under contract: on the ARBITER (the authority executing
//! shipped extents), a span is range-shared only when grants from TWO OR
//! MORE distinct holders overlap it — a single holder's span is
//! proxy-custody (the fold publishes that holder's own bytes, serialized
//! against its shipped publishes by the per-ino serve stripe), and a
//! demoted region is the arbiter's OWN vehicle (every holder's writes
//! there ship to it — KD-MW-8). Holder-side semantics are UNTOUCHED
//! (`span_range_shared_classifies_custody` in
//! `tests/dlm_range_custody_tests.rs` pins them).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1024 * 1024; // 1 MiB logical blocks (whole-block write-through)

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
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
        Some("16MB"),
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
            hash_seed: 0x57A7_F26A_2026_0828,
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
        dlm,
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

/// A striped file with `blocks` durable whole blocks (write-through).
async fn striped_file(h: &H, name: &str, blocks: u64) -> u64 {
    let ino = create(h, name).await;
    for i in 0..blocks {
        let data = bytes::Bytes::from(vec![(i % 251) as u8; BS as usize]);
        let w = h.fs.write(h.req, ino, 0, i * BS, data, 0, 0).await.unwrap();
        assert_eq!(w.written as u64, BS, "short write at block {i}");
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    ino
}

fn clause_counters() -> (u64, u64) {
    (
        METRICS
            .patch_ineligible_range_shared
            .load(Ordering::Relaxed),
        METRICS
            .overlay_ineligible_range_shared
            .load(Ordering::Relaxed),
    )
}

/// Acquire a NEW range grant, retrying while the fs write path's just-
/// dropped whole-file lease (see `invalidate_local_lease`) finishes its
/// queued async release.
async fn new_range_grant(
    d: &DlmClient,
    path: &str,
    span: (u64, u64),
    geometry: Option<(u64, u64)>,
) -> squeezefs::dlm::LockLease {
    for _ in 0..200 {
        match d
            .acquire_lock_range(path, span, span, Duration::from_millis(200), geometry)
            .await
        {
            Ok(squeezefs::dlm::RangeAcquired::New { lease, .. }) => return lease,
            Ok(other) => panic!("expected a NEW grant, got {other:?}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!("the whole-file lease never released — the range grant cannot be planted");
}

/// The cloud row's exact shape: a single holder's covering grant on the
/// ino, and the authority folds that holder's shipped sub-block extents.
/// The fold is the holder's PROXY — the fast-path clauses must not
/// decline it (attempt 8's engagement failure: one decline per fold
/// pass), and the extent's bytes must land durably.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_arbiters_fold_of_a_single_holders_extents_keeps_the_fast_paths() {
    let _g = serial();
    let h = make(*b"arbiter-fold-001", "arb_ns_1").await;
    let ino = striped_file(&h, "fpp.dat", 2).await;
    let path = format!("inode_{ino}");

    // The fs write path cached its own whole-file lease — drop it (the
    // live authority's table holds only the CO-WRITERS' grants).
    h.fs.invalidate_local_lease(ino);
    // The holder's grant (the co-writer's custody, seen from the
    // authority's LOCK_MAP — the fold's own token differs from it). A
    // DISTINCT manager = a distinct holder identity.
    let geometry = Some((16 * BS, BS));
    let dlm_holder = DlmClient::new().unwrap();
    let grant = new_range_grant(&dlm_holder, &path, (0, 2 * BS), geometry).await;
    // The live shape: the authority's own token generation runs AHEAD of
    // any single grant (grants mint continuously across the fleet), so
    // the fold's `get_fencing_token_ino` never accidentally equals the
    // holder's token.
    squeezefs::dlm::test_bump_fencing_generation(&path);

    let (p0, o0) = clause_counters();
    // The holder's sub-block write ships as an extent; the authority
    // assembles and the verb forces the fold (rung 17's executors,
    // driven directly — the same methods `install_extent_assembler`
    // wires to the wire).
    h.fs.assemble_shipped_extent(ino, 0, 4096, bytes::Bytes::from(vec![0xABu8; 8192]))
        .await
        .expect("the assembly write lands");
    h.fs.flush_shipped_extents(ino)
        .await
        .expect("the verb-forced fold publishes");
    let (p1, o1) = clause_counters();
    assert_eq!(
        (p1 - p0, o1 - o0),
        (0, 0),
        "the arbiter folding a SINGLE holder's extents is that holder's proxy — \
         the range-shared clauses must not decline their own designed vehicle \
         (attempt 8: one decline per fold pass, patch d={} overlay d={})",
        p1 - p0,
        o1 - o0
    );

    // The proceed face: the extent's bytes are the durable truth.
    let out =
        h.fs.read(h.req, ino, 0, 4096, 8192, 0)
            .await
            .expect("read back")
            .data;
    assert!(
        out.iter().all(|&x| x == 0xAB),
        "the folded extent's bytes are the published truth"
    );

    grant.release().await.expect("release");
}

/// The A1-phase face: the span is DEMOTED (authority-assembled for every
/// holder). The demotion machinery's own publisher — the arbiter's fold —
/// must keep its fast paths: the demoted arm exists to route holders'
/// writes TO this fold, not to block it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_arbiters_fold_under_a_demoted_region_keeps_the_fast_paths() {
    let _g = serial();
    let h = make(*b"arbiter-fold-002", "arb_ns_2").await;
    let ino = striped_file(&h, "shared.dat", 2).await;

    assert!(
        squeezefs::dlm::adopt_demoted_region(ino, (0, BS)),
        "fixture: block 0's span is demoted"
    );

    let (p0, o0) = clause_counters();
    h.fs.assemble_shipped_extent(ino, 0, 0, bytes::Bytes::from(vec![0xCDu8; 4096]))
        .await
        .expect("the assembly write lands");
    h.fs.flush_shipped_extents(ino)
        .await
        .expect("the verb-forced fold publishes");
    let (p1, o1) = clause_counters();
    assert_eq!(
        (p1 - p0, o1 - o0),
        (0, 0),
        "a demoted region is the ARBITER's own vehicle — its fold keeps the \
         fast paths (patch d={} overlay d={})",
        p1 - p0,
        o1 - o0
    );

    let out =
        h.fs.read(h.req, ino, 0, 0, 4096, 0)
            .await
            .expect("read back")
            .data;
    assert!(out.iter().all(|&x| x == 0xCD));
}

/// The decline that MUST stand: two DISTINCT holders' grants overlap the
/// folded span — the arbiter cannot be both writers' proxy at once, and
/// the fold rides the demotion/CoW vehicle (the clauses decline and
/// count). Green before and after the fix: the pin against
/// over-relaxation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_holder_span_still_declines_the_arbiters_fast_paths() {
    let _g = serial();
    let h = make(*b"arbiter-fold-003", "arb_ns_3").await;
    let ino = striped_file(&h, "contended.dat", 2).await;
    let path = format!("inode_{ino}");
    // A finer alignment quantum (BS/4) so two holders' sub-block grants
    // stay disjoint inside ONE folded block span — the admit stretches
    // desired to the geometry's block grain.
    let geometry = Some((16 * BS, BS / 4));
    h.fs.invalidate_local_lease(ino);
    // TWO distinct holder identities (two managers, two merge scopes).
    let dlm_a = DlmClient::new().unwrap();
    let dlm_b = DlmClient::new().unwrap();
    let a = new_range_grant(&dlm_a, &path, (0, BS / 2), geometry).await;
    let b = new_range_grant(&dlm_b, &path, (BS / 2, BS), geometry).await;

    let (p0, o0) = clause_counters();
    h.fs.assemble_shipped_extent(ino, 0, 0, bytes::Bytes::from(vec![0xEFu8; 4096]))
        .await
        .expect("the assembly write lands");
    h.fs.flush_shipped_extents(ino)
        .await
        .expect("the fold still publishes — via the sharing-safe vehicle");
    let (p1, o1) = clause_counters();
    assert!(
        (p1 - p0) + (o1 - o0) >= 1,
        "TWO distinct holders overlap the span: the fast-path clauses decline \
         and count (the pin against over-relaxing the arbiter's proxy law)"
    );

    a.release().await.expect("release A");
    b.release().await.expect("release B");
}
