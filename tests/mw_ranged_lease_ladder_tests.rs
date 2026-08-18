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
use squeezefs::meta_ship::{self as ship, OwnerMap, PeerOwner};
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
        ship::disarm_ownership();
        squeezefs::data_custody::test_reset_custody_generation();
        squeezefs::data_custody::test_clear_poison();
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
    let fh = h
        .fs
        .open(h.req, ino, libc::O_WRONLY as u32, 0)
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

    let reply = writer
        .await
        .expect("writer task")
        .unwrap_or_else(|e| {
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
    let err = h
        .fs
        .write(
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
        .acquire_lock(
            &path,
            Some((4 * BLK, 5 * BLK)),
            Duration::from_millis(500),
        )
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
        h.fs.create(h.req, 1, OsStr::new("rank0-sparse.dat"), libc::S_IFREG | 0o644, 0)
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
