//! **The ranged write lease rides the POSIX-5 retry ladder** (MW rung-18
//! residual (b); rung-17 live findings 5a/5b,
//! `.benchmarks/2026-08-17-s11-authority-assembler.md` §Findings item 5).
//!
//! The whole-file write lease has carried the POSIX-5 ladder since the
//! generic/795 fix (`acquire_lease_with_retry`: a LOST 5 s DLM wait
//! retries with backoff for the op-watchdog budget and fails `EIO` — never
//! `EAGAIN` — only when the holder never lets go). The RANGED write lease
//! (`acquire_write_lease_for_span`, S11 rung 15) shipped WITHOUT it: one
//! `DLM_LEASE_WAIT` (5 s) and the op failed EIO. That was latent while the
//! only contention was the leg's disjoint halves; it is load-bearing now
//! that the §9.3 demotion barrier is armed in production (the
//! zeros-interleave fix) — a barrier resolution is RENEWAL-BOUNDED (the
//! notice rides the incumbent's next renewal reply, up to TTL/3 away),
//! and at the shipped 45 s membership TTL that bound (15 s) exceeds one
//! 5 s wait by construction. Finding 5b's reconciliation IS the ladder:
//! the per-attempt wait stays `DLM_LEASE_WAIT`, the total budget is the
//! watchdog's (`SQUEEZEFS_TIMEOUT`, default 30 s > any shipped renewal
//! cadence), so a barrier-parked ranged acquire converges instead of
//! surfacing fsync/write EIO.
//!
//! Contracts:
//!
//! 1. **A lost wait retries and wins** — foreign custody holding the span
//!    longer than one `DLM_LEASE_WAIT` (the authority's conflict park
//!    times out, `range_custody.conflicts` moves) no longer fails the
//!    write: the ladder backs off, re-ships the acquire, and the write
//!    completes once the holder releases. `lease_retry_waits` accounts.
//! 2. **The honest exhaustion survives** — a holder that NEVER lets go
//!    fails the op `EIO` (never `EAGAIN`) after the watchdog budget,
//!    counted `lease_retry_exhaustions` (the POSIX-5 law, ranged face).
//!
//! Venue: the strand-test fixture (a real `SqueezefsFilesystem` over a
//! file-backed KV volume) armed as an S11 CO-WRITER — all-foreign
//! ownership, a live in-process custody authority on the cluster wire, an
//! installed `WriteCustodyClient`, `TEST_RANGE_CUSTODY_OVERRIDE = 1` —
//! so `fs.write` takes the production ranged path
//! (`range_write_engaged`), never a seam-only shortcut. The authority's
//! lease clock is MANUAL and frozen: nothing expires, so the only
//! resolution is the one under test (the holder's explicit release).
//! RED pre-fix: contract 1's write returns EIO right after the first
//! lost wait.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const SECRET: &[u8] = b"s11-ranged-lease-ladder-storage-trust-secret";

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global custody state)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(0, Ordering::Relaxed);
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        squeezefs::data_custody::test_reset_custody_generation();
        squeezefs::data_custody::test_clear_poison();
        // Finding 16's trim teacher learns ceilings from acquire replies,
        // and the ceiling map is keyed by ino — which RECURS across this
        // binary's fresh volumes.
        squeezefs::meta_ship::tokens::test_clear_stretch_ceilings();
    }
}

// ---------------------------------------------------------------------------
// Fixture: the strand-test fs + a live custody authority + co-writer arming
// ---------------------------------------------------------------------------

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The mw_truncate_lease_strand fixture, verbatim shape: one file-backed
/// volume set, default 4 MiB blocks.
async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    // Both tests share this process: ONE memoized watchdog budget (the
    // ladder's total budget — kept small so the exhaustion pin is
    // affordable, large enough for contract 1's one-lost-wait shape).
    std::env::set_var("SQUEEZEFS_TIMEOUT", "12");
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
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
        Some("32MB"),
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
            hash_seed: 0xC0FF_EE00_5511_0018,
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

struct Authority {
    listener: Arc<squeezefs::cluster_wire::RpcListener>,
    endpoint: String,
    /// Manual, FROZEN owner clock: leases never expire — the only
    /// resolution reachable in these pins is the one under test.
    _clock_ms: Arc<AtomicU64>,
}

fn start_authority(tag: &str) -> Authority {
    start_authority_be(tag, None)
}

/// [`start_authority`] with a PUBLISH service over `be` — the fsync-
/// bearing pins' shape (a co-writer's fsync ships its layout publishes;
/// without the service the row dies on the loud no-client refusal).
fn start_authority_be(
    tag: &str,
    be: Option<Arc<squeezefs::meta_backend::RoutedMetaBackend>>,
) -> Authority {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        tag,
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    // Deliberately NO geometry source: a foreign overlap parks on the
    // PLAIN conflict law ("no geometry, no barrier") — this suite pins
    // the ladder, not the demotion barrier's resolution.
    if be.is_some() {
        // The publish-bearing shape serves custody-scoped Puts, whose
        // law is scoped-or-REFUSED (the zeros-interleave fix): arm the
        // §9.2 geometry source (fixed 4 MiB blocks — the fs fixture's
        // default block size).
        owner.install_range_geometry(data_grant::fixed_range_geometry(
            64 * 4 * 1024 * 1024,
            4 * 1024 * 1024,
        ));
    }
    data_grant::install_custody_owner(Arc::clone(&owner));
    let mut router = data_grant::AsyncVerbRouter::new().with_custody(owner);
    if let Some(be) = be {
        router = router.with_publish(publish::PublishService::new(be));
    }
    let router = router;
    let listener = squeezefs::cluster_wire::RpcListener::start_async(
        squeezefs::cluster_wire::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..squeezefs::cluster_wire::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority {
        listener,
        endpoint,
        _clock_ms: ms,
    }
}

/// Arm THIS process as a co-writer whose every volume is foreign-home:
/// `range_write_engaged` becomes true for every ino, and the fs's ranged
/// acquires ship to the in-process authority.
async fn arm_cowriter(auth: &Authority, h: &H) -> Arc<WriteCustodyClient> {
    let routed = h.fs.meta_backend.as_ref().expect("meta backend").clone();
    let foreign: Vec<(usize, PeerOwner)> = (0..routed.volumes.len())
        .map(|v| (v, PeerOwner::new("ladder-authority", &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(&routed, foreign).expect("owner map"));
    publish::install_client(publish::PublishClient::new(
        "node-ladder-a",
        SECRET.to_vec(),
    ));
    let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, "node-ladder-a")
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&client));
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);
    client
}

/// Create a DURABLE STRIPED file BEFORE the arm (local, solo-posture ops
/// — the fleet's rank-0 create shape), retire the create/write custody at
/// last close, then reopen: the writes under test ride an open fh against
/// an existing striped layout — the exact branch the live rows exercise —
/// and the pre-arm whole-file lease is provably gone (the peer's
/// uncontended grant below is the strand tripwire).
async fn create_striped_open(h: &H, name: &str) -> (u64, u64) {
    let created =
        h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    let ino = created.attr.ino;
    let written =
        h.fs.write(
            h.req,
            ino,
            created.fh,
            0,
            bytes::Bytes::from(vec![0x11u8; 8 * 1024 * 1024]),
            0,
            0,
        )
        .await
        .expect("solo striped seed write")
        .written;
    assert_eq!(written, 8 * 1024 * 1024, "whole seed write");
    h.fs.fsync(h.req, ino, created.fh, false).await.unwrap();
    h.fs.release(h.req, ino, created.fh, 0, 0, false)
        .await
        .unwrap();
    let fh =
        h.fs.open(h.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("reopen for the contended writes")
            .fh;
    (ino, fh)
}

/// Wait until the authority has counted at least one MORE lost range wait
/// (`range_custody.conflicts`) than `before` — the proof that one full
/// `DLM_LEASE_WAIT` was lost, i.e. the exact instant the PRE-FIX path
/// surfaced EIO.
async fn wait_one_lost_wait(before: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(9);
    loop {
        if squeezefs::dlm::range_custody_stats().conflicts > before {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the authority never counted a lost range wait — the write \
             task's ranged acquire did not engage the conflict park"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Contract 1: one lost `DLM_LEASE_WAIT` no longer fails the write — the
/// ladder retries and wins when the foreign holder releases.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranged_write_lease_survives_one_lost_wait_and_retries() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let auth = start_authority("ladder-authority-1");
    let h = Arc::new(make(*b"ranged-ladder-01", "ranged-ladder-1").await);
    // Private inode band (the suite convention): the custody table and the
    // token mint are process-global — pad one ino so this test's object
    // never collides with the sibling test's, even across a panic-skipped
    // teardown.
    let pad =
        h.fs.create(h.req, 1, OsStr::new("pad.dat"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap();
    h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
        .await
        .unwrap();
    let (ino, fh) = create_striped_open(&h, "shared.dat").await;

    // The foreign holder: whole-file EX custody under a FOREIGN nonce,
    // taken directly in the arbiter's table BEFORE the co-writer arm (an
    // in-process wire peer would ADOPT its grant into this process's
    // custody table and misclassify the write range-shared — a fixture
    // artifact the real fleet cannot produce; whole-file custody keeps
    // the plain conflict law, which is exactly finding 5a's class).
    let peer = DlmClient::new().unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let lease = peer
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("the peer's uncontended whole-file hold");
    let client = arm_cowriter(&auth, &h).await;

    let conflicts_before = squeezefs::dlm::range_custody_stats().conflicts;
    let retries_before = METRICS.lease_retry_waits.load(Ordering::Relaxed);
    let grants_before = squeezefs::dlm::range_custody_stats().grants;

    let writer = {
        let h = h.clone();
        tokio::spawn(async move {
            h.fs.write(
                h.req,
                ino,
                fh,
                0,
                bytes::Bytes::from(vec![0x5Au8; 4096]),
                0,
                0,
            )
            .await
        })
    };

    // One full wait provably lost (the pre-fix EIO instant), THEN the
    // holder lets go. No magic sleeps: the authority's own conflict
    // counter is the schedule. A writer finishing BEFORE any lost wait
    // means the ranged path never engaged — a fixture lie, failed loud.
    let mut writer = writer;
    tokio::select! {
        early = &mut writer => {
            panic!(
                "the write finished ({early:?}) before the authority counted \
                 a lost range wait — the ranged conflict never engaged"
            );
        }
        () = wait_one_lost_wait(conflicts_before) => {}
    }
    lease.release().await.expect("peer releases");

    let reply = writer.await.expect("writer task").unwrap_or_else(|e| {
        panic!(
            "the ranged write lease must ride the POSIX-5 ladder — one \
                 lost DLM_LEASE_WAIT is a retry, never EIO (rung-17 finding \
                 5a, the s11-subblock live venue): {e:?}"
        )
    });
    assert_eq!(reply.written, 4096, "the retried write completes whole");
    assert!(
        METRICS.lease_retry_waits.load(Ordering::Relaxed) > retries_before,
        "lease_retry_waits must account the lost-then-won wait (the \
         POSIX-5 instrument, ranged face)"
    );
    assert!(
        squeezefs::dlm::range_custody_stats().grants > grants_before,
        "the winning acquire must be a RANGE grant (the ranged path \
         engaged — never a silent whole-file fallback)"
    );

    drop(client);
    auth.listener.shutdown();
}

/// Contract 2: the honest exhaustion survives — a holder that never lets
/// go fails the op EIO (never EAGAIN) after the watchdog budget, counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranged_write_lease_exhaustion_stays_loud_eio_after_the_budget() {
    let _serial = serial();
    let _restore = Restore;
    let auth = start_authority("ladder-authority-2");
    let h = Arc::new(make(*b"ranged-ladder-02", "ranged-ladder-2").await);
    let (ino, fh) = create_striped_open(&h, "wedged.dat").await;

    // The never-letting-go holder (the same foreign-nonce whole-file
    // shape as the sibling test — the plain conflict law).
    let peer = DlmClient::new().unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let held = peer
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("the peer's uncontended whole-file hold");
    let _client = arm_cowriter(&auth, &h).await;

    let exhaustions_before = METRICS.lease_retry_exhaustions.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    let err =
        h.fs.write(
            h.req,
            ino,
            fh,
            0,
            bytes::Bytes::from(vec![0x5Au8; 4096]),
            0,
            0,
        )
        .await
        .expect_err("a never-released foreign hold must fail the op");
    let spent = t0.elapsed();
    assert_eq!(
        err,
        fuse3::Errno::from(libc::EIO),
        "POSIX-5: EIO, never EAGAIN (reserved for O_NONBLOCK), got {err:?}"
    );
    assert!(
        spent >= Duration::from_secs(10),
        "the failure must consume the watchdog budget (12 s here), not one \
         5 s wait — took {spent:?}"
    );
    assert!(
        METRICS.lease_retry_exhaustions.load(Ordering::Relaxed) > exhaustions_before,
        "lease_retry_exhaustions must account the give-up"
    );

    // Converge the process-global custody table for the next test: the
    // wedge-holder lets go.
    held.release().await.expect("peer releases at teardown");

    auth.listener.shutdown();
}

/// **The desired-window stretch, v2** (rung-15 residual #3 — "the
/// desired-window stream stretch ... the R2-classifier window and the
/// measured ≥99.5 %-local verdict are the ior row's business"): the v1
/// stretch unioned the mount's whole span HULL and always doubled
/// forward. On an INTERLEAVED decomposition (the §9.5 MPI-IO/block-cyclic
/// rows: a mount's stripes are strided, never adjacent) the hull spans
/// every gap between the mount's own stripes — so the second ask's
/// desired GRABBED the unclaimed gaps, a peer's later REQUIRED for its
/// own block then conflicted with custody the grabber never writes, and
/// the §9.3 barrier demoted an aligned block to authority assembly (the
/// anti-shape) — fabricated sharing on rows where "nothing should share
/// a block" is the falsifier. The v2 law: union only an
/// OVERLAPPING-or-ABUTTING own span (a genuine stream extension), and
/// forward-double only on a sequentially-advancing per-ino write
/// frontier (the classifier-shaped gate — a strided writer pays one
/// extend RTT per stripe, a streaming writer keeps O(log n)).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strided_asks_never_bridge_the_gap_between_own_stripes() {
    let _serial = serial();
    let _restore = Restore;
    let auth = start_authority("ladder-authority-3");
    let h = Arc::new(make(*b"ranged-ladder-03", "ranged-ladder-3").await);
    // Private inode band: two pads (the suite convention).
    for pad_name in ["pad-a.dat", "pad-b.dat"] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "strided.dat").await;
    let _client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    // The strided shape: this mount's stripes are blocks 0 and 8 — never
    // adjacent (the block-cyclic per-holder law).
    for off in [0u64, 8 * BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x22u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("strided ranged write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }

    // The falsifier: block 4 — inside the GAP between this mount's
    // stripes — must be FREE custody. Pre-v2 the second ask's hull-union
    // desired grabbed [block 1, block 8), so this foreign acquire
    // conflicted (and, with geometry armed, would have fabricated a
    // demotion of an unshared block).
    let peer = DlmClient::new().unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let probe = peer
        .acquire_lock(&path, Some((4 * BLK, 5 * BLK)), Duration::from_millis(500))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "a strided holder's desired must never bridge its own gap: \
                 the peer's acquire of untouched block 4 conflicted — the \
                 hull-union grab fabricated custody over blocks the holder \
                 never writes: {e:?}"
            )
        });
    probe.release().await.expect("probe releases");

    auth.listener.shutdown();
}

/// The stretch's OTHER half survives v2: a genuinely SEQUENTIAL stream
/// still converges by extension + forward doubling — O(log n) round
/// trips, never one per block (the ≥99.5 %-local law's streaming face,
/// preserved verbatim from the rung-15 live leg's "1 acquire + 3
/// extensions per 64 MiB half").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_stream_still_converges_by_extension_and_doubling() {
    let _serial = serial();
    let _restore = Restore;
    let auth = start_authority("ladder-authority-4");
    let h = Arc::new(make(*b"ranged-ladder-04", "ranged-ladder-4").await);
    for pad_name in ["pad-c.dat", "pad-d.dat", "pad-e.dat"] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "stream.dat").await;
    let _client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let s0 = squeezefs::dlm::range_custody_stats();
    for b in 0..8u64 {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                b * BLK,
                bytes::Bytes::from(vec![0x33u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("sequential ranged write block {b} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    let s1 = squeezefs::dlm::range_custody_stats();
    let grants = s1.grants - s0.grants;
    let extensions = s1.extensions - s0.extensions;
    assert_eq!(
        grants, 1,
        "one stream = ONE grant record (extensions widen it in place)"
    );
    assert!(
        extensions <= 3,
        "8 sequential blocks must converge in O(log n) extensions \
         (doubling engaged) — got {extensions} (v1 measured 3; one per \
         block = 7 means the stretch died)"
    );

    auth.listener.shutdown();
}

/// **The geometry source's size read** (rung 18 — the Issue-19 class
/// firing at the SOURCE, convicted live on the MPI-IO row): the
/// production `router_range_geometry` read the ino's size from the
/// LAYOUT head — absent on a truncate-created sparse file (the rank-0
/// create-truncate shape stamps size in the INODE record), so a 10 GiB
/// shared file answered size 0, the §9.2 span cap floored at
/// `max(16, ceil(0/block)) = 16`, and the 32-rank row's 17th live span
/// refused "at capacity" — a CONSTANT refusing the workload S11 exists
/// for, wearing the floor's clothes. The law: the geometry size is the
/// MAX of the inode record's size and the layout head's (growth
/// published in either plane counts; absent-both is honestly 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_geometry_source_reads_a_truncate_created_files_size() {
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-05", "ranged-ladder-5").await);
    let created =
        h.fs.create(
            h.req,
            1,
            OsStr::new("rank0-sparse.dat"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap();
    let ino = created.attr.ino;
    h.fs.release(h.req, ino, created.fh, 0, 0, false)
        .await
        .unwrap();
    // The rank-0 shape: truncate(2) to the full decomposition size — no
    // layout exists, the size lives in the inode record.
    let size = 40 * 4 * 1024 * 1024u64; // 40 blocks
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let geometry = squeezefs::multi_writer::router_range_geometry(
        h.fs.meta_backend.as_ref().unwrap().clone(),
        h.fs.router.backend_router.clone(),
    );
    let (g_size, g_block) = geometry
        .geometry(ino)
        .await
        .expect("the source answers on a live data plane");
    assert!(g_block > 0, "block size");
    assert!(
        g_size >= size,
        "the geometry size must see the truncate-created inode size \
         ({size}), got {g_size} — a 0 answer floors the §9.2 span cap at \
         16 and refuses the 17th stripe of the very decomposition S11 \
         exists for (the MPI-IO row's live wedge)"
    );
}

/// The §6.2-item-9 divergence refusal is the SUPERSESSION class (rung 18
/// — the MPI-IO row's width-32 conviction: 32 concurrent ranged
/// publishers of one ino keep the owner's chain compacting, and every
/// in-flight delta naming the prior base refused "divergent
/// layout-delta chain ... folds onto 0x0" — which the writeback
/// classifier latched TERMINAL, EINVAL'd the app's fsync (POSIX-16) and
/// aborted the row. The save's refusal arm resets the ino's RAM
/// provenance, so a retry refetches and converges: the class is
/// RETRIED, on both its faces (the raw refusal and the coalesced-pass
/// Io wrap). The width-N behavior itself is the live leg's falsifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_divergence_refusal_is_the_retried_supersession_class() {
    let raw = squeezefs::error::SqueezefsError::InvalidOperation(
        "kv metadata: corrupt KV encoding: divergent layout-delta chain (spec §6.2 item 9): \
         link seq 767296 names base version 0x30000000727 but folds onto 0x0"
            .to_string(),
    );
    assert!(
        !squeezefs::fuse_client::writeback_error_is_terminal(&raw),
        "the divergence refusal must be retried (the owner re-based; the \
         recompute converges), never latched EINVAL"
    );
    let wrapped = squeezefs::error::SqueezefsError::Io(std::io::Error::other(format!(
        "coalesced publish failed: {raw}"
    )));
    assert!(
        !squeezefs::fuse_client::writeback_error_is_terminal(&wrapped),
        "the coalesced-pass wrap of the same refusal is the same class"
    );
    let shipped = squeezefs::error::SqueezefsError::refused(libc::EINVAL, format!("{raw}"));
    assert!(
        !squeezefs::fuse_client::writeback_error_is_terminal(&shipped),
        "the SHIPPED face (the owner's refusal decoded Refused{{errno}}          through the publish wire) is the same class"
    );
    let indirect = squeezefs::error::SqueezefsError::refused(
        libc::EINVAL,
        "Invalid operation: kv metadata: corrupt KV encoding: layout delta base unusable:          indirect base — its map lives in a data-plane blob; a delta cannot fold onto it",
    );
    assert!(
        !squeezefs::fuse_client::writeback_error_is_terminal(&indirect),
        "the owner's mid-flight INDIRECT flip is the same refetch-and-recompute class"
    );
    let corrupt = squeezefs::error::SqueezefsError::InvalidOperation(
        "kv metadata: corrupt KV encoding: bad checksum".to_string(),
    );
    assert!(
        squeezefs::fuse_client::writeback_error_is_terminal(&corrupt),
        "every OTHER corrupt-encoding refusal stays terminal"
    );
}

/// **A range writer fences on its OWN lease token** (§9.2's law verbatim,
/// made real at width — rung-15 residual #2's composed pin, convicted
/// live on the MPI-IO row: every new stripe grant advances the ino's max
/// generation, so every in-flight writeback unit presenting its own
/// older-but-LIVE grant token read as superseded — 1,841 stale-token
/// retries against 20 MiB of progress, the convergence ladder livelocked
/// by the acquire storm it was converging toward). The pin: two
/// non-adjacent stripes (two grants, two tokens), one fsync — ZERO
/// stale-token retries (each unit's own live token IS current custody);
/// the remount/supersession refusal (a token that is neither current nor
/// live) stays intact elsewhere. (The width-32 RACE itself — writes
/// minting new stripes concurrently with in-flight writeback units — is
/// the live leg's falsifier per the repro-port exception; this pin is
/// the law's structural guard on the deterministic two-stripe shape.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_writers_own_live_token_never_reads_stale() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-06", "ranged-ladder-6").await);
    let auth = start_authority_be(
        "ladder-authority-6",
        Some(h.fs.meta_backend.as_ref().unwrap().clone()),
    );
    for pad_name in ["pad-f.dat", "pad-g.dat", "pad-h.dat", "pad-i.dat"] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "stripes.dat").await;
    let _client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let retries_before = METRICS
        .writeback_stale_token_retries
        .load(Ordering::Relaxed);
    // Two NON-ADJACENT stripes: two grants, two tokens (the second mint
    // advances the ino's max generation past the first unit's token).
    for off in [0u64, 8 * BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x44u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("stripe write at {off}: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    h.fs.fsync(h.req, ino, fh, false)
        .await
        .unwrap_or_else(|e| panic!("fsync across two live grants: {e:?}"));
    let retries = METRICS
        .writeback_stale_token_retries
        .load(Ordering::Relaxed)
        - retries_before;
    assert_eq!(
        retries, 0,
        "a unit presenting its OWN live grant token is CURRENT custody — \
         {retries} stale-token retries mean the write path fences on the \
         ino's max generation instead of §9.2's own-lease law (the MPI-IO \
         row's livelock)"
    );

    auth.listener.shutdown();
}

// ---------------------------------------------------------------------------
// §9.3a — the required-watermark TAIL SHRINK, end to end (residual board
// item 7's fix; measured conviction
// `.benchmarks/2026-08-19-blob-aware-merge-and-fabric-venue.md` §3). The
// production round trip under test: the mount's forward doubling stretches
// its grant past what it writes; a peer's REQUIRED in the stretch tail
// parks on the SHRINK barrier (never the demotion barrier); the notice
// rides the mount's next renewal reply; the client shrinks its covering
// cache BEFORE the ack travels, answers its written high-water, and the
// peer is granted EXCLUSIVE custody — plus the client-side LEARNED
// STRETCH CEILING that stops the same interleave from colliding again.
// ---------------------------------------------------------------------------

/// Poll a stats predicate on a bounded deadline (the suite's
/// wait_one_lost_wait pattern, generalized — no magic sleeps: the
/// counters are the schedule).
async fn wait_until(secs: u64, what: &str, probe: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if probe() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// **The learned stretch ceiling** (§9.3a's client half): a shrink notice
/// teaches the client the stretch LENGTH that survived beyond its written
/// frontier; future sequential doublings on that ino clamp to it
/// (`range_custody_stretch_ceiling_clamps`), so a steady block-cyclic
/// interleave pays at most ONE shrink round per custody episode — never
/// one per stride (the fabric row's 822-conflict retry amplification).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_learned_ceiling_stops_repeat_stretch_collisions() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-07", "ranged-ladder-7").await);
    let auth = start_authority_be(
        "ladder-authority-7",
        Some(h.fs.meta_backend.as_ref().unwrap().clone()),
    );
    // Private inode band: five pads (the suite convention).
    for pad_name in [
        "pad-j.dat",
        "pad-k.dat",
        "pad-l.dat",
        "pad-m.dat",
        "pad-n.dat",
    ] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "cyclic.dat").await;
    let client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let geometry = Some((64 * BLK, BLK));
    let path = squeezefs::keys::inode_path(ino);
    let s0 = squeezefs::dlm::range_custody_stats();

    // Run 1 (the mount's first stride): blocks 0-1, sequential — the
    // doubling stretches custody to [0, 16M) while the mount only ever
    // writes [0, 8M).
    for off in [0u64, BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x22u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("run-1 write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }

    // The peer's block-2 required lands in the stretch tail: it must park
    // on the SHRINK barrier — never the demotion barrier.
    let peer = squeezefs::dlm::LocalLockManager::new().expect("peer manager");
    let peer_task = {
        let peer = peer.clone();
        let path = path.clone();
        tokio::spawn(async move {
            peer.acquire_lock_range_scoped(
                &path,
                (2 * BLK, 3 * BLK),
                (2 * BLK, 3 * BLK),
                Duration::from_secs(20),
                geometry,
                Some(0xB),
            )
            .await
        })
    };
    wait_until(
        9,
        "the peer's tail required never marked a SHRINK pending",
        || squeezefs::dlm::range_custody_stats().tail_shrinks > s0.tail_shrinks,
    )
    .await;
    assert_eq!(
        squeezefs::dlm::range_custody_stats().demotions,
        s0.demotions,
        "a tail-only overlap must never fabricate a demotion"
    );

    // The notice rides the incumbent's renewal reply; the client answers
    // its written high-water (8M ≤ the 8M floor) and the peer's grant
    // issues EXCLUSIVE.
    client
        .renew_all()
        .await
        .expect("the incumbent's renewal carries the shrink notice");
    let peer_grant = tokio::time::timeout(Duration::from_secs(10), peer_task)
        .await
        .expect("the peer resolves after the shrink ack")
        .expect("join")
        .expect("the peer's grant issues");
    let peer_lease = match peer_grant {
        squeezefs::dlm::RangeAcquired::New { lease, span } => {
            assert_eq!(span, (2 * BLK, 3 * BLK), "the peer gets exactly its ask");
            lease
        }
        other => panic!("the peer's grant is NEW exclusive custody: {other:?}"),
    };
    let s1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(s1.tail_shrinks - s0.tail_shrinks, 1);
    assert_eq!(s1.tail_shrink_acks - s0.tail_shrink_acks, 1);
    assert_eq!(s1.demotions, s0.demotions, "zero demotions throughout");

    // Run 2 (the mount's next stride): blocks 4-5. The learned ceiling
    // clamps the doubling, so the stretch never crosses into unclaimed
    // foreign territory again.
    for off in [4 * BLK, 5 * BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x33u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("run-2 write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    let s2 = squeezefs::dlm::range_custody_stats();
    assert!(
        s2.stretch_ceiling_clamps > s1.stretch_ceiling_clamps,
        "the learned ceiling must clamp the run-2 doubling \
         (range_custody_stretch_ceiling_clamps)"
    );
    assert_eq!(
        s2.tail_shrinks, s1.tail_shrinks,
        "no second shrink round on the steady interleave"
    );

    // The falsifier: block 6 — exactly where the UN-clamped doubling
    // would have stretched — is free custody NOW, without a shrink round
    // (pre-fix this parks on a fabricated pending and dies on the ttl).
    let probe = peer
        .acquire_lock_range_scoped(
            &path,
            (6 * BLK, 7 * BLK),
            (6 * BLK, 7 * BLK),
            Duration::from_secs(2),
            geometry,
            Some(0xB),
        )
        .await
        .expect(
            "the mount's clamped stretch must leave the peer's next run \
             free — a park here is the repeat-collision shape",
        );
    let s3 = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        s3.tail_shrinks, s2.tail_shrinks,
        "no shrink round for the probe"
    );
    assert_eq!(s3.demotions, s2.demotions);
    match probe {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => {
            lease.release().await.expect("probe releases")
        }
        other => panic!("the probe's grant is NEW: {other:?}"),
    }
    peer_lease.release().await.expect("peer releases");

    drop(client);
    auth.listener.shutdown();
}

/// **Zero true sharing keeps the shared clauses silent**: the full
/// block-cyclic interleave — the mount owns runs {0-1, 4-5, 8-9}, the
/// peer owns runs {2-3, 6-7} — with ZERO true block sharing must run
/// with `patch_ineligible_range_shared` and
/// `overlay_ineligible_range_shared` deltas 0, zero demotions, nothing
/// demoted, every write of the mount acked whole and every acquire of
/// the peer granted — and the shrink ledger closed at quiesce.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_true_sharing_keeps_the_shared_clauses_silent() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-08", "ranged-ladder-8").await);
    let auth = start_authority_be(
        "ladder-authority-8",
        Some(h.fs.meta_backend.as_ref().unwrap().clone()),
    );
    for pad_name in [
        "pad-o.dat",
        "pad-p.dat",
        "pad-q.dat",
        "pad-r.dat",
        "pad-s.dat",
        "pad-t.dat",
    ] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "interleave.dat").await;
    let client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let geometry = Some((64 * BLK, BLK));
    let path = squeezefs::keys::inode_path(ino);
    let s0 = squeezefs::dlm::range_custody_stats();
    let p0 = METRICS
        .patch_ineligible_range_shared
        .load(Ordering::Relaxed);
    let o0 = METRICS
        .overlay_ineligible_range_shared
        .load(Ordering::Relaxed);

    let write = |off: u64, fill: u8| {
        let h = h.clone();
        async move {
            let written =
                h.fs.write(
                    h.req,
                    ino,
                    fh,
                    off,
                    bytes::Bytes::from(vec![fill; BLK as usize]),
                    0,
                    0,
                )
                .await
                .unwrap_or_else(|e| panic!("interleave write at {off} failed: {e:?}"))
                .written;
            assert_eq!(written as u64, BLK, "every write acks whole");
        }
    };

    // Mount run 1: blocks 0-1 (the doubling stretches past 8M).
    write(0, 0x41).await;
    write(BLK, 0x42).await;

    // Peer run 1: blocks 2-3 — the transient shrink round (the ONE
    // allowed round per episode), resolved over the renewal channel.
    let peer = squeezefs::dlm::LocalLockManager::new().expect("peer manager");
    let peer_task = {
        let peer = peer.clone();
        let path = path.clone();
        tokio::spawn(async move {
            peer.acquire_lock_range_scoped(
                &path,
                (2 * BLK, 4 * BLK),
                (2 * BLK, 4 * BLK),
                Duration::from_secs(20),
                geometry,
                Some(0xB),
            )
            .await
        })
    };
    wait_until(
        9,
        "the peer's run-1 required never marked a SHRINK pending",
        || squeezefs::dlm::range_custody_stats().tail_shrinks > s0.tail_shrinks,
    )
    .await;
    client
        .renew_all()
        .await
        .expect("the incumbent's renewal carries the shrink notice");
    let peer_lease_1 = match tokio::time::timeout(Duration::from_secs(10), peer_task)
        .await
        .expect("the peer resolves after the shrink ack")
        .expect("join")
        .expect("the peer's run-1 grant issues")
    {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease,
        other => panic!("the peer's run-1 grant is NEW: {other:?}"),
    };

    // Mount run 2: blocks 4-5 (ceiling-clamped stretch).
    write(4 * BLK, 0x43).await;
    write(5 * BLK, 0x44).await;

    // Peer run 2: blocks 6-7 — must grant immediately, no round of any
    // kind (the clamp left the run unclaimed).
    let peer_lease_2 = match peer
        .acquire_lock_range_scoped(
            &path,
            (6 * BLK, 8 * BLK),
            (6 * BLK, 8 * BLK),
            Duration::from_secs(2),
            geometry,
            Some(0xB),
        )
        .await
        .expect("the peer's run-2 acquire grants immediately")
    {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease,
        other => panic!("the peer's run-2 grant is NEW: {other:?}"),
    };

    // Mount run 3: blocks 8-9, plus a covered RE-write of block 1 (an
    // overwrite beside a foreign boundary — the W1/B4 probes fire on own
    // custody and must stay silent).
    write(8 * BLK, 0x45).await;
    write(9 * BLK, 0x46).await;
    write(BLK, 0x47).await;

    let s1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        METRICS
            .patch_ineligible_range_shared
            .load(Ordering::Relaxed)
            - p0,
        0,
        "zero true sharing: the W1 clause-7 ledger must not move"
    );
    assert_eq!(
        METRICS
            .overlay_ineligible_range_shared
            .load(Ordering::Relaxed)
            - o0,
        0,
        "zero true sharing: the B4 range-clause ledger must not move"
    );
    assert_eq!(
        s1.demotions, s0.demotions,
        "zero demotions on the aligned interleave"
    );
    assert_eq!(s1.shrink_demotions, s0.shrink_demotions);
    assert!(
        squeezefs::dlm::demoted_regions(ino).is_empty(),
        "nothing authority-assembled on a zero-sharing row"
    );
    assert_eq!(
        s1.tail_shrinks - s0.tail_shrinks,
        (s1.tail_shrink_acks - s0.tail_shrink_acks)
            + (s1.tail_shrink_fence_resolves - s0.tail_shrink_fence_resolves),
        "the shrink ledger closes at quiesce"
    );

    peer_lease_1.release().await.expect("peer run-1 releases");
    peer_lease_2.release().await.expect("peer run-2 releases");
    drop(client);
    auth.listener.shutdown();
}

/// **Finding 16's trim teacher** (residual 7, the steady-state half —
/// `.benchmarks/2026-08-25-s11-freeloop-stall.md` §finding 16): the
/// acquire reply's OWN TRIM is a ceiling lesson. The shrink-notice
/// teacher rides renewals, but block-cyclic grants churn faster than a
/// renewal cadence (row 2: 40 of 51 notices died through the fence, the
/// ceiling never learned, 8,610 trims were ignored). A stretched ask
/// whose granted span comes back CLIPPED teaches the surviving stretch
/// length immediately — zero wire change, no shrink round, no park.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_acquire_replys_trim_teaches_the_ceiling_without_a_shrink_round() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-08", "ranged-ladder-8").await);
    let auth = start_authority_be(
        "ladder-authority-8",
        Some(h.fs.meta_backend.as_ref().unwrap().clone()),
    );
    // SEVEN pads (the suite's private-inode-band convention — a distinct
    // count per test, so the process-global per-ino range/ceiling caches
    // never collide across the binary's fresh volumes; 2/3/4/5/6 are
    // taken).
    for pad_name in [
        "pad-o.dat",
        "pad-p.dat",
        "pad-q.dat",
        "pad-r.dat",
        "pad-s.dat",
        "pad-t.dat",
        "pad-u.dat",
    ] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino, fh) = create_striped_open(&h, "trimtaught.dat").await;
    let _client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let geometry = Some((64 * BLK, BLK));
    let path = squeezefs::keys::inode_path(ino);
    let s0 = squeezefs::dlm::range_custody_stats();

    // Two peers hold the stripes AHEAD of each of the mount's runs — the
    // block-cyclic neighbor shape (their REQUIRED never overlaps anything
    // the mount writes; only its stretched DESIRE ever reaches them).
    let peer = squeezefs::dlm::LocalLockManager::new().expect("peer manager");
    let _peer_a = match peer
        .acquire_lock_range_scoped(
            &path,
            (2 * BLK, 4 * BLK),
            (2 * BLK, 4 * BLK),
            Duration::from_secs(10),
            geometry,
            Some(0xC),
        )
        .await
        .expect("peer A's custody issues")
    {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease,
        other => panic!("peer A expected NEW custody: {other:?}"),
    };
    let _peer_b = match peer
        .acquire_lock_range_scoped(
            &path,
            (6 * BLK, 8 * BLK),
            (6 * BLK, 8 * BLK),
            Duration::from_secs(10),
            geometry,
            Some(0xD),
        )
        .await
        .expect("peer B's custody issues")
    {
        squeezefs::dlm::RangeAcquired::New { lease, .. } => lease,
        other => panic!("peer B expected NEW custody: {other:?}"),
    };

    // Run 1: blocks 0-1, sequential — the doubling stretches the desire
    // to [0, 16M), which the authority TRIMS against peer A's [8M, 16M).
    // The reply's clipped span IS the lesson: surviving stretch = 0.
    for off in [0u64, BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x44u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("run-1 write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    let s1 = squeezefs::dlm::range_custody_stats();
    assert!(
        s1.desired_trims > s0.desired_trims,
        "run 1's stretched desire was trimmed against peer A (the fixture's premise)"
    );

    // Run 2: blocks 4-5 (the mount's next stride). The TAUGHT ceiling
    // clamps the doubling before it reaches peer B — the ask goes out
    // exactly required-sized, so run 2 pays ZERO trims and ZERO shrink
    // rounds (pre-fix: the un-taught stretch reaches [16M,32M), overlaps
    // peer B, pays another trim — the 8,610-trims-per-row treadmill).
    for off in [4 * BLK, 5 * BLK] {
        let written =
            h.fs.write(
                h.req,
                ino,
                fh,
                off,
                bytes::Bytes::from(vec![0x55u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("run-2 write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    let s2 = squeezefs::dlm::range_custody_stats();
    assert!(
        s2.stretch_ceiling_clamps > s1.stretch_ceiling_clamps,
        "the trim TAUGHT the ceiling — run 2's doubling clamps at the source \
         (pre-fix the lesson only ever arrived on a renewal-carried shrink \
         notice, which a churning grant never lives to hear)"
    );
    assert_eq!(
        s2.desired_trims, s1.desired_trims,
        "the clamped ask never reaches peer B: zero trims in run 2 — the \
         treadmill is off"
    );
    assert_eq!(
        s2.tail_shrinks, s0.tail_shrinks,
        "no shrink round anywhere: required spans never overlapped, and the \
         lesson came from the reply itself"
    );
    assert_eq!(
        s2.demotions, s0.demotions,
        "and no demotion was ever fabricated"
    );
}

/// **Finding 16 half (a) — the notice carrier is every custody-channel
/// reply, not just the renewal** (`.benchmarks/2026-08-25-s11-freeloop-stall.md`
/// §Finding 16): row 2's ledger proved 40 of 51 shrink notices DIED with
/// their grant (`tail_shrink_fence_resolves`) because the only carrier was
/// the incumbent's renewal reply and, under the block-cyclic interleave,
/// grants live shorter than a renewal cadence. The §9.3a learning loop was
/// structurally dark on exactly the workload it was built for.
///
/// Two carriers pinned, one fixture (no `renew_all` anywhere in this
/// test):
///
/// * **Phase A — the ACQUIRE reply**: a peer's required parks in the
///   mount's stretch tail; the mount's next custody interaction is an
///   acquire for a DISTANT span of the same file, and that reply must
///   carry the shrink notice — the client shrinks, acks, and the peer's
///   grant issues within one interaction instead of one cadence.
/// * **Phase B — the RELEASE reply**: same setup on a second file, and
///   the mount's next interaction is a queued-release drain; the release
///   reply is the carrier.
///
/// Both phases close through the ACK column with the fence column FLAT —
/// the ledger law `tail_shrinks ≡ acks + fence_resolves` holding on the
/// healthy side, which is the entire point of the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shrink_notices_ride_acquire_and_release_replies_not_just_renewals() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    let h = Arc::new(make(*b"ranged-ladder-09", "ranged-ladder-9").await);
    let auth = start_authority_be(
        "ladder-authority-9",
        Some(h.fs.meta_backend.as_ref().unwrap().clone()),
    );
    // Private inode band: eight pads (the suite convention) — subjects at
    // the band's 9th and 10th inos, colliding with no sibling test.
    for pad_name in [
        "pad-v.dat",
        "pad-w.dat",
        "pad-x.dat",
        "pad-y.dat",
        "pad-z.dat",
        "pad-aa.dat",
        "pad-ab.dat",
        "pad-ac.dat",
    ] {
        let pad =
            h.fs.create(h.req, 1, OsStr::new(pad_name), libc::S_IFREG | 0o644, 0)
                .await
                .unwrap();
        h.fs.release(h.req, pad.attr.ino, pad.fh, 0, 0, false)
            .await
            .unwrap();
    }
    let (ino_a, fh_a) = create_striped_open(&h, "carrier-a.dat").await;
    let (ino_b, fh_b) = create_striped_open(&h, "carrier-b.dat").await;
    let client = arm_cowriter(&auth, &h).await;

    const BLK: u64 = 4 * 1024 * 1024;
    let geometry = Some((64 * BLK, BLK));
    let peer = squeezefs::dlm::LocalLockManager::new().expect("peer manager");
    let s0 = squeezefs::dlm::range_custody_stats();

    // ---------------- Phase A: the ACQUIRE reply carries the notice ----
    // The mount's sequential blocks 0-1 stretch its grant to [0, 16M)
    // while it only ever writes [0, 8M).
    for off in [0u64, BLK] {
        let written =
            h.fs.write(
                h.req,
                ino_a,
                fh_a,
                off,
                bytes::Bytes::from(vec![0x66u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("phase-A write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    // The peer's block-2 required lands in the stretch tail and parks on
    // the SHRINK barrier.
    let peer_a = {
        let peer = peer.clone();
        let path = squeezefs::keys::inode_path(ino_a);
        tokio::spawn(async move {
            peer.acquire_lock_range_scoped(
                &path,
                (2 * BLK, 3 * BLK),
                (2 * BLK, 3 * BLK),
                Duration::from_secs(30),
                geometry,
                Some(0xC),
            )
            .await
        })
    };
    wait_until(
        9,
        "phase A: the peer's tail required never marked a SHRINK pending",
        || squeezefs::dlm::range_custody_stats().tail_shrinks > s0.tail_shrinks,
    )
    .await;

    // The mount's NEXT custody interaction is an acquire for a distant
    // span of the same file (nothing queued a release, so the acquire is
    // the first reply composed after the pending-mark). Its reply must
    // carry the notice; the client's absorb ships the ack inline.
    let written =
        h.fs.write(
            h.req,
            ino_a,
            fh_a,
            8 * BLK,
            bytes::Bytes::from(vec![0x77u8; BLK as usize]),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("phase-A far write failed: {e:?}"))
        .written;
    assert_eq!(written as u64, BLK);
    wait_until(
        9,
        "phase A: the shrink notice never rode the far acquire's reply — \
         the carrier is still renewal-only (finding 16 half (a))",
        || squeezefs::dlm::range_custody_stats().tail_shrink_acks > s0.tail_shrink_acks,
    )
    .await;
    let grant_a = tokio::time::timeout(Duration::from_secs(10), peer_a)
        .await
        .expect("phase A: the peer resolves after the acquire-carried ack")
        .expect("join")
        .expect("phase A: the peer's grant issues");
    match grant_a {
        squeezefs::dlm::RangeAcquired::New { lease, span } => {
            assert_eq!(span, (2 * BLK, 3 * BLK), "the peer gets exactly its ask");
            lease.release().await.expect("peer A releases");
        }
        other => panic!("phase A: the peer's grant is NEW exclusive custody: {other:?}"),
    }
    let s1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        s1.tail_shrink_fence_resolves, s0.tail_shrink_fence_resolves,
        "phase A closed through the ACK column — a fence resolve means the \
         notice died with the grant again"
    );
    assert_eq!(s1.demotions, s0.demotions, "no fabricated demotion");

    // ---------------- Phase B: the RELEASE reply carries the notice ----
    // A far-span grant held BEFORE the contention exists — the handle
    // whose queued release will be the mount's next custody interaction.
    let far = client
        .acquire_range(
            ino_b,
            (8 * BLK, 9 * BLK),
            (8 * BLK, 9 * BLK),
            Duration::from_secs(5),
        )
        .await
        .expect("phase B: the far-span grant issues uncontended");
    let far_lease = match far {
        squeezefs::data_grant::RangeAcquireOutcome::New { lease, .. } => lease,
        other => panic!("phase B: the far span is NEW custody: {other:?}"),
    };
    for off in [0u64, BLK] {
        let written =
            h.fs.write(
                h.req,
                ino_b,
                fh_b,
                off,
                bytes::Bytes::from(vec![0x88u8; BLK as usize]),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("phase-B write at {off} failed: {e:?}"))
            .written;
        assert_eq!(written as u64, BLK);
    }
    let peer_b = {
        let peer = peer.clone();
        let path = squeezefs::keys::inode_path(ino_b);
        tokio::spawn(async move {
            peer.acquire_lock_range_scoped(
                &path,
                (2 * BLK, 3 * BLK),
                (2 * BLK, 3 * BLK),
                Duration::from_secs(30),
                geometry,
                Some(0xC),
            )
            .await
        })
    };
    wait_until(
        9,
        "phase B: the peer's tail required never marked a SHRINK pending",
        || squeezefs::dlm::range_custody_stats().tail_shrinks > s1.tail_shrinks,
    )
    .await;

    // The mount's next custody interaction is the queued-release drain —
    // no acquire, no renewal. The release reply is the carrier.
    far_lease
        .release()
        .await
        .expect("phase B: queue the far release");
    client.drain_releases().await;
    wait_until(
        9,
        "phase B: the shrink notice never rode the release reply — the \
         carrier is still renewal-only (finding 16 half (a))",
        || squeezefs::dlm::range_custody_stats().tail_shrink_acks > s1.tail_shrink_acks,
    )
    .await;
    let grant_b = tokio::time::timeout(Duration::from_secs(10), peer_b)
        .await
        .expect("phase B: the peer resolves after the release-carried ack")
        .expect("join")
        .expect("phase B: the peer's grant issues");
    match grant_b {
        squeezefs::dlm::RangeAcquired::New { lease, span } => {
            assert_eq!(span, (2 * BLK, 3 * BLK), "the peer gets exactly its ask");
            lease.release().await.expect("peer B releases");
        }
        other => panic!("phase B: the peer's grant is NEW exclusive custody: {other:?}"),
    }

    // The ledger law on the healthy side: both rounds closed through the
    // ACK column, the fence column never moved.
    let s2 = squeezefs::dlm::range_custody_stats();
    assert_eq!(s2.tail_shrinks - s0.tail_shrinks, 2, "two shrink rounds");
    assert_eq!(s2.tail_shrink_acks - s0.tail_shrink_acks, 2, "two acks");
    assert_eq!(
        s2.tail_shrink_fence_resolves, s0.tail_shrink_fence_resolves,
        "zero fence resolves — the healthy-fleet law, restored on a \
         churn-shaped interaction pattern"
    );
    assert_eq!(s2.demotions, s0.demotions, "zero demotions throughout");

    drop(client);
    auth.listener.shutdown();
}

// ===========================================================================
// Finding 27 — the quiet incumbent hears at POLL latency, not its renewal
// ===========================================================================

/// Finding 27 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`, PR 5
/// acceptance attempt 9): the §9.3 demotion barrier resolves at the
/// incumbent's ACK, and every notice carrier (f16a: acquire, release,
/// renewal replies) is a reply on a verb the incumbent must SEND — a
/// rank idling at an ior barrier sends nothing until its renewal cadence
/// (min(10 s, T_self/3) — 10 s on the fleet), so the asker parks up to
/// its full wait budget and the lockstep iteration's aggregate halves
/// (the every-4th-iteration ~half-bandwidth dip in EVERY shared phase;
/// attempt 9's A2 died on dip placement: 2,106 → 976 MiB/s, one
/// `dlm_custody_phase_ns.arbitrate.<=8s`).
///
/// The law under contract: a custody client keeps ONE standing NOTICE
/// POLL parked on its authority (a client-initiated RPC whose REPLY
/// carries the notice — the §9.3 barrier's own vocabulary, the
/// delegation recall channel's exact shape; never a push backchannel),
/// so a QUIET incumbent hears a pending demotion at poll latency and
/// the barrier resolves in milliseconds. The ledger is unchanged:
/// `demotions ≡ acks + fence_resolves`, closed through the ACK column.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_incumbents_demotion_resolves_at_poll_latency_not_renewal() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial();
    let _restore = Restore;
    const INO: u64 = 77_000_141;
    const MIB: u64 = 1024 * 1024;

    // The quiet-clock authority: renewal CANNOT rescue the row inside the
    // test budget (t_owner 60 s ⇒ renew cadence min(10 s, T_self/3) =
    // 10 s), geometry armed — the barrier is "no geometry, no barrier".
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_secs(60),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        "f27-authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    owner.install_range_geometry(data_grant::fixed_range_geometry(
        64 * 4 * 1024 * 1024,
        4 * 1024 * 1024,
    ));
    data_grant::install_custody_owner(Arc::clone(&owner));
    let router = data_grant::AsyncVerbRouter::new().with_custody(owner);
    let listener = squeezefs::cluster_wire::RpcListener::start_async(
        squeezefs::cluster_wire::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..squeezefs::cluster_wire::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();

    // The incumbent: a sub-block grant inside block 0, then QUIET — no
    // writes, no acquires, nothing until a 10 s-away renewal (the
    // rank-at-the-ior-barrier shape).
    let a = WriteCustodyClient::connect(&endpoint, SECRET, "f27-node-a")
        .await
        .expect("incumbent joins");
    data_grant::install_custody_client(Arc::clone(&a));
    let _ga = a
        .acquire_range(INO, (0, MIB), (0, MIB), Duration::from_secs(3))
        .await
        .expect("the incumbent's grant");

    // The asker: byte-DISJOINT, SAME block — the §9.3 demotion barrier
    // parks this ask until the incumbent acks.
    let b = WriteCustodyClient::connect(&endpoint, SECRET, "f27-node-b")
        .await
        .expect("asker joins");
    let s0 = squeezefs::dlm::range_custody_stats();
    let t0 = std::time::Instant::now();
    let rb = b
        .acquire_range(
            INO,
            (2 * MIB, 3 * MIB),
            (2 * MIB, 3 * MIB),
            Duration::from_secs(3),
        )
        .await;
    let waited = t0.elapsed();
    assert!(
        rb.is_ok(),
        "the asker's grant must issue at NOTICE-POLL latency — a QUIET incumbent \
         hears the pending demotion on its standing poll, never at its renewal \
         cadence (finding 27's dip; after {waited:?} of a 3 s budget: {rb:?})"
    );
    assert!(
        waited < Duration::from_secs(2),
        "resolution must be poll-latency, not a renewal-bounded park (waited {waited:?})"
    );
    let s1 = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        s1.demotions - s0.demotions,
        1,
        "the barrier engaged (one region demoted)"
    );
    assert_eq!(
        s1.demotion_acks - s0.demotion_acks,
        1,
        "the ledger closes through the ACK column — the quiet incumbent ACKED \
         (never the fence column)"
    );

    drop(b);
    drop(a);
    listener.shutdown();
}
