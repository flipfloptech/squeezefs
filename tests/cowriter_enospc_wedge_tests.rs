//! **The co-writer ENOSPC wedge** — the fleet finding recorded in
//! `.benchmarks/2026-09-06-cowriter-enospc-wedge.md` (evidence
//! `.benchmarks/rows-d4-s11-20260905/m57.log`, `m50.stats.json`).
//!
//! On the s11-mpiio fleet (1 authority + 8 co-writers, 32 ior ranks on one
//! shared file) every co-writer's allocation lane exhausted 23 s in. The
//! refusal itself is correct (finding 15 — the free-grace recycle loop
//! loses to the churn). What was WRONG is that the ENOSPC'd writes never
//! terminated: 100 writes in flight for 30 minutes, 37k watchdog lines,
//! the write-phase census naming `ov_alloc` and `write_checkout`, the
//! lock-wait census naming a genuinely held stripe, a `cat .stats` in
//! D-state until the fleet teardown's connection abort.
//!
//! # The park, named
//!
//! [`BlockAllocator::allocate_block_grace_bounded`] — the finding-29
//! bounded-allocation park — held under the write's `BLOCK_FLUSH_LOCKS`
//! guard (order 3, the `write_checkout` site) from
//! `try_device_overlay_store` (phase `ov_alloc`) and every other write-path
//! allocation site. Two compounding defects made "bounded" a lie:
//!
//! 1. `free_grace::pressure_park_wall_ms()` read `bound()` — the
//!    **reallocation LABEL** (an owner-clock instant, `u64::MAX` on every
//!    mount that is not a free-grace owner with members) — as if it were a
//!    duration. The wall was `u64::MAX` ms on every co-writer, reader and
//!    unarmed writer.
//! 2. `reclaimable_supply_exists()` on a laned co-writer answered `true`
//!    whenever a harvest SINK was installed — the existence of the wire,
//!    not evidence of supply. The allocator therefore parked every
//!    exhausted co-writer allocation forever, one authority harvest RPC per
//!    50 ms slice (m50: 16,899 harvests for 16,769 refusals).
//!
//! # The contracts
//!
//! * the exact park ENDS: a laned allocator whose lane is exhausted refuses
//!   `StorageFull` within the wall, whatever the authority reports;
//! * an authority reporting nothing held is genuine exhaustion — the
//!   co-writer refuses at once, exactly like the local empty-ring arm;
//! * the wall is a DURATION derived from the plane's routine fence bound,
//!   never the label;
//! * the solo control: a full single-writer store refuses at once (today's
//!   law, untouched);
//! * at the mount: writes against an exhausted lane TERMINATE within a
//!   bound, leave every block stripe free, leave the write pipeline with
//!   nothing in flight, leave the mount answering `getattr` and `.stats`,
//!   and surface the exhaustion honestly at the durability boundary;
//! * error isolation, recovery once space returns, and the
//!   `write_enospc_refusals` gauge.
//!
//! RED against `dev` (8168e26e): every test that reaches the park times
//! out at its 10 s bound instead of returning `StorageFull`.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_alloc_lane::{LaneHarvest, LaneHarvestSink};
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::free_grace;
use squeezefs::fuse_client::{SqueezefsFilesystem, BLOCK_FLUSH_LOCKS, METRICS, STATS_INODE};
use squeezefs::membership::{
    self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use squeezefs::write_pipeline;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

/// The bound every "must terminate" assertion runs under — an order of
/// magnitude above the longest legal park on an unarmed plane (1 s wall).
const BOUND: Duration = Duration::from_secs(10);

/// Block size for the mount-level rig: striped at small sizes so the lane
/// exhausts in a handful of blocks.
const BS: u64 = 65536;

// ---------------------------------------------------------------------------
// Serialization + restoration (the free-grace plane, the depth override and
// the gauges are process-global)
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
        write_pipeline::set_depth_override(None);
        free_grace::reset_for_test();
        membership::uninstall();
    }
}

fn restore() -> Restore {
    free_grace::reset_for_test();
    Restore
}

// ---------------------------------------------------------------------------
// The allocator rig: a laned co-writer whose authority answers the harvest
// ---------------------------------------------------------------------------

async fn allocator(id: &str, capacity_blocks: u64) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    a.set_capacity_bytes(capacity_blocks * a.chunk_size());
    a
}

fn part(writers: u16, id: u16) -> AppendPartition {
    AppendPartition::new(writers, id).expect("partition")
}

/// The authority as the co-writer's harvest sees it: an EMPTY grant (the
/// lane's supply is gone) carrying the authority's live bound age. A
/// nonzero `hint` is the field shape (m50 read 15,096 ms — the authority's
/// ring held offsets under the storm); `0` is "nothing held".
struct EmptyAuthority {
    calls: AtomicU64,
    hint_ms: AtomicU64,
}

impl EmptyAuthority {
    fn new(hint_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU64::new(0),
            hint_ms: AtomicU64::new(hint_ms),
        })
    }

    fn sink(self: &Arc<Self>) -> LaneHarvestSink {
        let me = Arc::clone(self);
        Arc::new(move |_max: u64| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.calls.fetch_add(1, Ordering::Relaxed);
                Ok(LaneHarvest {
                    blocks: Vec::new(),
                    bound_age_hint_ms: me.hint_ms.load(Ordering::Relaxed),
                    rtt_ms: 1,
                })
            })
        })
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

/// A laned allocator (lane 1 of 2) with `capacity_blocks` of device, wired
/// to `authority`, with its whole lane share already minted.
async fn exhausted_co_writer(
    id: &str,
    capacity_blocks: u64,
    authority: &Arc<EmptyAuthority>,
) -> Arc<BlockAllocator> {
    let a = allocator(id, capacity_blocks).await;
    a.engage_alloc_lanes(part(2, 1))
        .expect("engage lane 1 of 2");
    a.set_lane_harvest_sink(authority.sink());
    let share = squeezefs::data_alloc_lane::lane_capacity_blocks(capacity_blocks, 2, 1);
    for i in 0..share {
        let off = a
            .allocate_block()
            .await
            .unwrap_or_else(|e| panic!("mint {i} of the lane share: {e}"));
        assert_eq!(
            (off / a.chunk_size()) % 2,
            1,
            "every mint is in this mount's residue class"
        );
    }
    a
}

fn is_storage_full(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

// ---------------------------------------------------------------------------
// 1. The exact park ends
// ---------------------------------------------------------------------------

/// The field shape verbatim: the lane is exhausted, every harvest comes back
/// empty, and the authority reports a held ring (nonzero bound age). The
/// bounded allocation must PARK (finding 29's promise — the retries drive
/// the authority's pressure fence) and then REFUSE at the wall. Against
/// `dev` it never returns: the wall is `u64::MAX`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bounded_allocation_ends_on_an_exhausted_co_writer_lane() {
    let _s = serial();
    let _r = restore();
    let authority = EmptyAuthority::new(15_096);
    let a = exhausted_co_writer("f15-wedge-park", 8, &authority).await;

    // The honest refusal the field logged, once per attempt.
    let refusals0 = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);
    let e = a.allocate_block().await.expect_err("the lane is exhausted");
    assert!(is_storage_full(&e), "lane exhaustion is StorageFull: {e}");
    assert!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) > refusals0,
        "the refusal is counted on alloc_lane_enospc_refusals"
    );

    let parks0 = free_grace::pressure_parks();
    let harvests0 = authority.calls();
    let wall = free_grace::pressure_park_wall_ms();
    let t0 = Instant::now();
    let verdict = tokio::time::timeout(BOUND, a.allocate_block_grace_bounded())
        .await
        .expect("THE WEDGE: the bounded allocation must end within the bound");
    let elapsed = t0.elapsed();
    let e = verdict.expect_err("the lane is still exhausted");
    assert!(is_storage_full(&e), "the verdict stays StorageFull: {e}");
    assert!(
        free_grace::pressure_parks() > parks0,
        "the park engaged (the retries are what drive the authority's fence)"
    );
    assert!(
        authority.calls() > harvests0 + 1,
        "each park slice re-ran the harvest"
    );
    assert!(
        elapsed.as_millis() as u64 <= wall + 2_000,
        "the park ends at the wall ({wall} ms) — took {elapsed:?}"
    );
}

/// An authority reporting NOTHING held (bound age 0) is genuine exhaustion
/// for the co-writer exactly as an empty local ring is for the authority:
/// refuse at once, no park slices at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_holding_nothing_refuses_the_co_writer_without_a_park() {
    let _s = serial();
    let _r = restore();
    let authority = EmptyAuthority::new(0);
    let a = exhausted_co_writer("f15-wedge-honest", 8, &authority).await;

    let parks0 = free_grace::pressure_parks();
    let t0 = Instant::now();
    let verdict = tokio::time::timeout(BOUND, a.allocate_block_grace_bounded())
        .await
        .expect("THE WEDGE: the bounded allocation must end within the bound");
    let elapsed = t0.elapsed();
    let e = verdict.expect_err("genuine exhaustion refuses");
    assert!(is_storage_full(&e), "the verdict stays StorageFull: {e}");
    assert_eq!(
        free_grace::pressure_parks(),
        parks0,
        "nothing reclaimable was reported — no park slice is taken"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "genuine exhaustion refuses promptly — took {elapsed:?}"
    );
}

/// The wall is a DURATION derived from the plane's routine fence bound —
/// twice it, floored at one second — never the reallocation label. Against
/// `dev` an unarmed plane answers `u64::MAX` (the label's "releases
/// everything" sentinel read as a wait), and an armed plane answers twice
/// the min-acked owner-clock instant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_park_wall_is_a_duration_never_the_reallocation_label() {
    let _s = serial();
    let _r = restore();
    assert_eq!(
        free_grace::pressure_park_wall_ms(),
        1_000,
        "no plane: the wall is the one-second floor"
    );

    let ticks = Arc::new(AtomicU64::new(10_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let clocks = LeaseClocks::derive(Duration::from_micros(250)).expect("shipped clocks");
    let owner = MembershipOwner::arm("f15-wall-owner", 3, 2, clocks, clock.clone())
        .expect("a successor term arms");
    membership::install_owner(Arc::clone(&owner));
    match owner.join(JoinRequest {
        id: "f15-wall-reader".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-f15".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(_) => {}
        other => panic!("a fresh reader joins: {other:?}"),
    }
    free_grace::arm_owner_plane_with(
        clock.clone(),
        Duration::from_millis(4_000),
        Duration::from_millis(2_000),
    );
    owner.refresh_free_grace_bound();
    assert_eq!(
        free_grace::fence_bound_base_ms(),
        4_000,
        "the routine fence bound in force"
    );
    assert_eq!(
        free_grace::pressure_park_wall_ms(),
        8_000,
        "armed: twice the routine fence bound"
    );

    free_grace::reset_for_test();
    free_grace::arm_owner_plane_with(
        clock,
        Duration::from_millis(400),
        Duration::from_millis(200),
    );
    owner.refresh_free_grace_bound();
    assert_eq!(
        free_grace::pressure_park_wall_ms(),
        1_000,
        "a tiny routine bound floors at one second"
    );
}

/// The single-writer control: a full solo store (no lanes, no grace ring)
/// refuses through the bounded form at once — today's law, untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_solo_control_a_full_single_writer_store_refuses_at_once() {
    let _s = serial();
    let _r = restore();
    let a = allocator("f15-wedge-solo", 2).await;
    a.allocate_block().await.expect("mint 0");
    a.allocate_block().await.expect("mint 1");
    let parks0 = free_grace::pressure_parks();
    let t0 = Instant::now();
    let verdict = tokio::time::timeout(BOUND, a.allocate_block_grace_bounded())
        .await
        .expect("a solo full store must answer within the bound");
    let e = verdict.expect_err("the store is full");
    assert!(is_storage_full(&e), "StorageFull: {e}");
    assert_eq!(
        free_grace::pressure_parks(),
        parks0,
        "no park on a solo store"
    );
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "the solo refusal is prompt"
    );
}

// ---------------------------------------------------------------------------
// The mount-level rig: a cache-less striped mount over a laned allocator
// ---------------------------------------------------------------------------

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    alloc: Arc<BlockAllocator>,
    _b: NamedTempFile,
    _m: NamedTempFile,
}

/// A cache-less mount (RAM tiers + direct block I/O — no staging detour, so
/// the never-lossy ladder's terminal arm is "keep parked") whose allocator
/// is lane 1 of 2 over `capacity_blocks`, harvesting from `authority`.
async fn make(
    uuid: [u8; 16],
    alloc_ns: &str,
    capacity_blocks: u64,
    authority: &Arc<EmptyAuthority>,
) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // The accumulation machinery is under test (the field's holder sat in
    // the overlay's allocation under the same guard — the allocator park
    // is one function); the sub-block fast paths are pinned OFF so every
    // write rides it.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    ba.set_capacity_bytes(capacity_blocks * ba.chunk_size());
    ba.engage_alloc_lanes(part(2, 1))
        .expect("engage lane 1 of 2");
    ba.set_lane_harvest_sink(authority.sink());
    let cache = TieredCache::new(
        vec![],
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
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xF15E_D6E0_0000_0001,
            uuid,
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
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
        alloc: ba,
        _b: b,
        _m: m,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// fuse3's `Errno` converts to the kernel's NEGATIVE form; the contracts
/// speak libc's positive one.
fn errno_of(e: fuse3::Errno) -> i32 {
    i32::from(e).abs()
}

/// One bounded write: `Ok(written)` or the errno — never a hang.
async fn bounded_write(
    fs: &SqueezefsFilesystem,
    req: Request,
    ino: u64,
    off: u64,
    data: Vec<u8>,
) -> Result<u32, i32> {
    let len = data.len();
    let r = tokio::time::timeout(
        BOUND,
        fs.write(req, ino, 0, off, bytes::Bytes::from(data), 0, 0),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("THE WEDGE: write at {off} (+{len}) did not terminate within {BOUND:?}")
    });
    r.map(|w| w.written).map_err(errno_of)
}

async fn bounded_fsync(h: &H, ino: u64) -> Result<(), i32> {
    tokio::time::timeout(BOUND, h.fs.fsync(h.req, ino, 0, false))
        .await
        .expect("fsync must terminate within the bound")
        .map_err(errno_of)
}

async fn bounded_getattr(h: &H, ino: u64) -> u64 {
    tokio::time::timeout(BOUND, h.fs.getattr(h.req, ino, None, 0))
        .await
        .expect("getattr must answer within the bound")
        .expect("getattr")
        .attr
        .size
}

async fn bounded_stats(h: &H) -> serde_json::Value {
    let reply = tokio::time::timeout(BOUND, h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0))
        .await
        .expect(".stats must answer within the bound")
        .expect("read stats inode");
    serde_json::from_slice(&reply.data).expect("stats JSON")
}

/// Fill the lane exactly: one striped file of `blocks` full blocks, durable.
async fn fill_lane(h: &H, name: &str, blocks: u64) -> u64 {
    let ino = create(h, name).await;
    let data = pattern((blocks * BS) as usize, 0x11);
    let written = bounded_write(&h.fs, h.req, ino, 0, data)
        .await
        .expect("the lane share fits");
    assert_eq!(written as u64, blocks * BS);
    bounded_fsync(h, ino).await.expect("the fill is durable");
    let m =
        h.fs.router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap();
    assert_eq!(m.file_type, "striped", "the fixture is STRIPED");
    let e = h
        .alloc
        .allocate_block()
        .await
        .expect_err("the lane is exhausted exactly");
    assert!(is_storage_full(&e), "{e}");
    ino
}

/// The write pipeline's in-flight gauge converges to 0 within the bound
/// (permits are released on every exit — a parked write never strands
/// bytes in flight).
async fn await_pipeline_drained(h: &H) {
    let t0 = Instant::now();
    loop {
        if h.fs.write_pipeline.inflight_bytes() == 0 {
            return;
        }
        assert!(
            t0.elapsed() < BOUND,
            "write_pipeline_inflight_bytes stayed at {} past {BOUND:?}",
            h.fs.write_pipeline.inflight_bytes()
        );
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// 2. At the mount: the storm terminates and the mount keeps answering
// ---------------------------------------------------------------------------

/// The field's holder shape (`SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0`:
/// the write itself owns the write-through and its allocation under the
/// held block guard): N concurrent whole-block writes past the exhausted
/// lane. Every one TERMINATES within the bound; every block stripe is free
/// afterwards; `getattr` and `.stats` answer during and after; the write
/// pipeline holds nothing; and the durability boundary surfaces the
/// exhaustion honestly (`fsync` → `ENOSPC`) instead of a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_lane_writes_terminate_and_the_mount_keeps_answering() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let authority = EmptyAuthority::new(0);
    let h = make([0xF1; 16], "f15_wedge_mount_sync", 8, &authority).await;
    let ino = fill_lane(&h, "shared.bin", 4).await;

    // The storm: 4 concurrent whole-block writes to blocks 4..8, with a
    // getattr and a `.stats` read racing them.
    let mut writes = Vec::new();
    for b in 4u64..8 {
        let fs = h.fs.clone();
        let req = h.req;
        let data = pattern(BS as usize, 0x20 + b as u8);
        writes.push(tokio::spawn(async move {
            bounded_write(&fs, req, ino, b * BS, data).await
        }));
    }
    let size_during = bounded_getattr(&h, ino).await;
    assert!(size_during >= 4 * BS, "getattr answers during the storm");
    let _ = bounded_stats(&h).await;
    let mut outcomes = Vec::new();
    for w in writes {
        outcomes.push(w.await.expect("write task"));
    }
    for (i, o) in outcomes.iter().enumerate() {
        match o {
            Ok(n) => assert_eq!(*n as u64, BS, "write {i}: a completed write is whole"),
            Err(errno) => assert_eq!(
                *errno,
                libc::ENOSPC,
                "write {i}: the refusal class is ENOSPC"
            ),
        }
    }

    // Every block stripe the storm touched is FREE (try-lock succeeds).
    for b in 4u32..8 {
        let g = BLOCK_FLUSH_LOCKS.get_lock(ino, b).try_lock();
        assert!(
            g.is_ok(),
            "block {b}'s stripe is still held after the storm"
        );
    }
    await_pipeline_drained(&h).await;

    // The durability boundary is honest: whatever was acked under the
    // never-lossy ladder cannot land, and fsync says so — within the bound.
    let acked = outcomes.iter().filter(|o| o.is_ok()).count();
    let fsync = bounded_fsync(&h, ino).await;
    if acked > 0 {
        assert_eq!(
            fsync,
            Err(libc::ENOSPC),
            "acked bytes with no lane to land in surface ENOSPC at fsync"
        );
    }
    // And the mount still answers afterwards.
    let _ = bounded_getattr(&h, ino).await;
    let stats = bounded_stats(&h).await;
    assert_eq!(
        stats["metrics"]["write_pipeline_inflight_bytes"].as_u64(),
        Some(0),
        "nothing is left in flight"
    );
}

/// The ACK-early pipeline shape (the default depth governor): a complete
/// block's write ACKs and a DETACHED upload owns the allocation while
/// holding a write-pipeline permit. Against `dev` that task parks forever
/// and its permit never returns — `write_pipeline_inflight_bytes` stays
/// pinned and every later write parks at admission (the field's `entry`
/// phase). The permit must come back within the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_pipeline_upload_returns_its_permit_within_the_bound() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(None);
    let authority = EmptyAuthority::new(0);
    let h = make([0xF2; 16], "f15_wedge_mount_pipe", 8, &authority).await;
    let ino = fill_lane(&h, "shared.bin", 4).await;

    let inflight0 = h.fs.write_pipeline.inflight_bytes();
    assert_eq!(inflight0, 0, "quiet before the storm");
    for b in 4u64..6 {
        let data = pattern(BS as usize, 0x30 + b as u8);
        match bounded_write(&h.fs, h.req, ino, b * BS, data).await {
            Ok(n) => assert_eq!(n as u64, BS),
            Err(errno) => assert_eq!(errno, libc::ENOSPC),
        }
    }
    await_pipeline_drained(&h).await;
    for b in 4u32..6 {
        assert!(
            BLOCK_FLUSH_LOCKS.get_lock(ino, b).try_lock().is_ok(),
            "block {b}'s stripe is free"
        );
    }
    let stats = bounded_stats(&h).await;
    assert_eq!(
        stats["metrics"]["write_pipeline_inflight_bytes"].as_u64(),
        Some(0)
    );
}

// ---------------------------------------------------------------------------
// 3. Isolation, recovery, the gauge
// ---------------------------------------------------------------------------

/// Error isolation: the refused write does not poison a sibling write to
/// another block of the same file that DOES have a lane block to land in —
/// the sibling's bytes are durable and read back exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enospc_write_does_not_poison_its_sibling_block() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let authority = EmptyAuthority::new(0);
    // Lane share 4; fill 3 so exactly ONE lane block remains.
    let h = make([0xF3; 16], "f15_wedge_isolation", 8, &authority).await;
    let ino = create(&h, "shared.bin").await;
    let base = pattern((3 * BS) as usize, 0x11);
    bounded_write(&h.fs, h.req, ino, 0, base)
        .await
        .expect("3 blocks fit");
    bounded_fsync(&h, ino).await.expect("durable");

    // Two concurrent whole-block writes: one lands in the last lane block,
    // the other has nowhere to go.
    let want3 = pattern(BS as usize, 0x33);
    let want4 = pattern(BS as usize, 0x44);
    let (w3, w4) = tokio::join!(
        bounded_write(&h.fs, h.req, ino, 3 * BS, want3.clone()),
        bounded_write(&h.fs, h.req, ino, 4 * BS, want4.clone()),
    );
    for (b, w) in [(3, &w3), (4, &w4)] {
        match w {
            Ok(n) => assert_eq!(*n as u64, BS, "block {b}"),
            Err(errno) => assert_eq!(*errno, libc::ENOSPC, "block {b}"),
        }
    }
    let fsync = bounded_fsync(&h, ino).await;
    // Exactly one of the two could land; the other surfaces ENOSPC at the
    // durability boundary. Whichever landed reads back exact.
    let m =
        h.fs.router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .unwrap();
    let bm = m.block_map.clone().expect("striped map");
    let landed: Vec<u32> = [3u32, 4]
        .into_iter()
        .filter(|b| bm.contains_key(b))
        .collect();
    assert_eq!(
        landed.len(),
        1,
        "exactly one lane block remained: one sibling landed, one could not (map {bm:?}, fsync {fsync:?})"
    );
    assert_eq!(
        fsync,
        Err(libc::ENOSPC),
        "the one that could not land is reported"
    );
    let b = landed[0];
    let want = if b == 3 { &want3 } else { &want4 };
    let got = tokio::time::timeout(
        BOUND,
        h.fs.read(h.req, ino, 0, u64::from(b) * BS, BS as u32, 0),
    )
    .await
    .expect("read answers")
    .expect("read")
    .data;
    assert_eq!(&got[..], &want[..], "the landed sibling reads back exact");
}

/// Recovery: once space returns to the lane (the fixture file is unlinked
/// and its blocks freed), new writes land and fsync succeeds — no latched
/// dead state survives the ENOSPC episode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freeing_space_after_enospc_lets_new_writes_land() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let authority = EmptyAuthority::new(0);
    let h = make([0xF4; 16], "f15_wedge_recovery", 8, &authority).await;
    let full = fill_lane(&h, "filler.bin", 4).await;

    let victim = create(&h, "victim.bin").await;
    let data = pattern(BS as usize, 0x55);
    match bounded_write(&h.fs, h.req, victim, 0, data.clone()).await {
        Ok(n) => {
            assert_eq!(n as u64, BS);
            assert_eq!(bounded_fsync(&h, victim).await, Err(libc::ENOSPC));
        }
        Err(errno) => assert_eq!(errno, libc::ENOSPC),
    }

    // Space returns: the filler's four lane blocks come back — unlink, the
    // kernel's RELEASE + FORGET-driven reclaim, the reclaimer's drain (the
    // `statfs_live_accounting_tests` delete-and-drain shape), each bounded.
    tokio::time::timeout(BOUND, async {
        h.fs.unlink(h.req, 1, OsStr::new("filler.bin"))
            .await
            .expect("unlink");
        let _ = h.fs.release(h.req, full, full, 0, 0, false).await;
        h.fs.reclaim_orphaned_batch(vec![full]).await;
        h.fs.router.backend_router.reclaim_drain().await;
    })
    .await
    .expect("the delete + reclaim drain terminate within the bound");
    let t0 = Instant::now();
    while h.alloc.free_block_indices().is_empty() {
        assert!(
            t0.elapsed() < BOUND,
            "the freed blocks never reached the free list"
        );
        h.fs.router.backend_router.reclaim_drain().await;
        tokio::task::yield_now().await;
    }

    let fresh = create(&h, "fresh.bin").await;
    let n = bounded_write(&h.fs, h.req, fresh, 0, data.clone())
        .await
        .expect("a fresh write lands once space returned");
    assert_eq!(n as u64, BS);
    bounded_fsync(&h, fresh).await.expect("and is durable");
    let got =
        h.fs.read(h.req, fresh, 0, 0, BS as u32, 0)
            .await
            .expect("read")
            .data;
    assert_eq!(&got[..], &data[..]);
}

/// The gauge: `write_enospc_refusals` counts exactly the WRITE replies
/// refused `ENOSPC` — a fresh file's first striped write against the
/// exhausted lane (the promotion allocates synchronously, so the write
/// itself is refused) counts one; writes the never-lossy ladder ACKs and
/// the fsync that later reports them count none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_write_enospc_refusals_gauge_counts_exactly_the_refused_writes() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let authority = EmptyAuthority::new(0);
    let h = make([0xF5; 16], "f15_wedge_gauge", 8, &authority).await;
    let _full = fill_lane(&h, "filler.bin", 4).await;

    // Read through the stats inode (the operator's surface): a missing
    // key is the gauge not existing, which is its own failure.
    let gauge = |stats: serde_json::Value| -> u64 {
        stats["metrics"]["write_enospc_refusals"]
            .as_u64()
            .expect("write_enospc_refusals rides the stats inode")
    };
    let g0 = gauge(bounded_stats(&h).await);
    let fresh = create(&h, "fresh.bin").await;
    let data = pattern((2 * BS) as usize, 0x66);
    let r = bounded_write(&h.fs, h.req, fresh, 0, data).await;
    let g1 = gauge(bounded_stats(&h).await);
    match r {
        Err(errno) => {
            assert_eq!(errno, libc::ENOSPC);
            assert_eq!(g1 - g0, 1, "one refused write, one count");
        }
        Ok(_) => {
            assert_eq!(g1 - g0, 0, "an acked write is not a refusal");
            assert_eq!(bounded_fsync(&h, fresh).await, Err(libc::ENOSPC));
            assert_eq!(
                gauge(bounded_stats(&h).await),
                g1,
                "fsync's ENOSPC is not a WRITE refusal"
            );
        }
    }
}
