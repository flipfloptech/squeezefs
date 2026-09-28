//! **A write-path allocation refusal is TERMINAL for that write** — the
//! fleet finding recorded in `.benchmarks/2026-09-06-cowriter-enospc-wedge.md`
//! (evidence `.benchmarks/rows-d4-s11-20260905/m57.log`, `m50.stats.json`),
//! re-shaped at PR 14 onto the one store every writer has: the co-writer's
//! allocation LANE that found it retired with the posture (a joined
//! writer's supply is PR 8's block grant, whose exhaustion the holder
//! answers `Full` — `tests/sym_block_grant_tests.rs`), and the wedge's
//! LAW is the allocator's, whatever fed it.
//!
//! On the s11-mpiio fleet (1 authority + 8 co-writers, 32 ior ranks on one
//! shared file) every co-writer's supply exhausted 23 s in. The refusal
//! itself was correct. What was WRONG is that the ENOSPC'd writes never
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
//! allocation site. `free_grace::pressure_park_wall_ms()` read `bound()` —
//! the **reallocation LABEL** (an owner-clock instant, `u64::MAX` on every
//! mount that is not a free-grace owner with members) — as if it were a
//! duration, so the wall was `u64::MAX` ms on every mount that parked.
//!
//! # The contracts
//!
//! * the wall is a DURATION derived from the plane's routine fence bound,
//!   never the label;
//! * a full store with nothing reclaimable refuses at once — no park;
//! * at the mount: writes against a full store TERMINATE within a bound,
//!   leave every block stripe free, leave the write pipeline with nothing
//!   in flight, leave the mount answering `getattr` and `.stats`, and
//!   surface the exhaustion honestly at the durability boundary;
//! * error isolation, recovery once space returns, and the
//!   `write_enospc_refusals` gauge.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::free_grace;
use squeezefs::fuse_client::{SqueezefsFilesystem, BLOCK_FLUSH_LOCKS, STATS_INODE};
use squeezefs::membership::{
    self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
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

/// Block size for the mount-level rig: striped at small sizes so the store
/// fills in a handful of blocks.
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
// The allocator rig: a store minted to capacity
// ---------------------------------------------------------------------------

async fn allocator(id: &str, capacity_blocks: u64) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    a.set_capacity_bytes(capacity_blocks * a.chunk_size());
    a
}

fn is_storage_full(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

// ---------------------------------------------------------------------------
// 1. The wall and the solo control
// ---------------------------------------------------------------------------

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

/// A full store with nothing reclaimable (no grace ring) refuses through
/// the bounded form at once — the park is for supply the grace ring holds,
/// never for genuine exhaustion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_store_with_nothing_reclaimable_refuses_at_once() {
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
// The mount-level rig: a cache-less striped mount over a small store
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
/// holds `capacity_blocks` of device.
async fn make(uuid: [u8; 16], alloc_ns: &str, capacity_blocks: u64) -> H {
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

/// Fill the store exactly: one striped file of `blocks` full blocks, durable.
async fn fill_store(h: &H, name: &str, blocks: u64) -> u64 {
    let ino = create(h, name).await;
    let data = pattern((blocks * BS) as usize, 0x11);
    let written = bounded_write(&h.fs, h.req, ino, 0, data)
        .await
        .expect("the store's capacity fits");
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
        .expect_err("the store is full exactly");
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
/// held block guard): N concurrent whole-block writes past the full
/// store. Every one TERMINATES within the bound; every block stripe is free
/// afterwards; `getattr` and `.stats` answer during and after; the write
/// pipeline holds nothing; and the durability boundary surfaces the
/// exhaustion honestly (`fsync` → `ENOSPC`) instead of a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_store_writes_terminate_and_the_mount_keeps_answering() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF1; 16], "f15_wedge_mount_sync", 4).await;
    let ino = fill_store(&h, "shared.bin", 4).await;

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
    // (Under item 24 the fill's terminal refusal latched the store, so
    // every storm write is refused pre-ack and `acked` reads 0 — the fsync
    // arm below is then the pre-item-24 law kept for the shape where a
    // write was acked before the latch; the acked-custody contract in §4
    // exercises it directly.)
    let acked = outcomes.iter().filter(|o| o.is_ok()).count();
    let fsync = bounded_fsync(&h, ino).await;
    if acked > 0 {
        assert_eq!(
            fsync,
            Err(libc::ENOSPC),
            "acked bytes with no block to land in surface ENOSPC at fsync"
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
    let h = make([0xF2; 16], "f15_wedge_mount_pipe", 4).await;
    let ino = fill_store(&h, "shared.bin", 4).await;

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
/// another block of the same file that DOES have a block to land in —
/// the sibling's bytes are durable and read back exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enospc_write_does_not_poison_its_sibling_block() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    // Capacity 4; fill 3 so exactly ONE block remains.
    let h = make([0xF3; 16], "f15_wedge_isolation", 4).await;
    let ino = create(&h, "shared.bin").await;
    let base = pattern((3 * BS) as usize, 0x11);
    bounded_write(&h.fs, h.req, ino, 0, base)
        .await
        .expect("3 blocks fit");
    bounded_fsync(&h, ino).await.expect("durable");

    // Two concurrent whole-block writes: one lands in the last block, the
    // other has nowhere to go.
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
        "exactly one block remained: one sibling landed, one could not (map {bm:?}, fsync {fsync:?})"
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

/// Recovery: once space returns (the fixture file is unlinked and its
/// blocks freed), new writes land and fsync succeeds — no latched dead
/// state survives the ENOSPC episode.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn freeing_space_after_enospc_lets_new_writes_land() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF4; 16], "f15_wedge_recovery", 4).await;
    let full = fill_store(&h, "filler.bin", 4).await;

    let victim = create(&h, "victim.bin").await;
    let data = pattern(BS as usize, 0x55);
    match bounded_write(&h.fs, h.req, victim, 0, data.clone()).await {
        Ok(n) => {
            assert_eq!(n as u64, BS);
            assert_eq!(bounded_fsync(&h, victim).await, Err(libc::ENOSPC));
        }
        Err(errno) => assert_eq!(errno, libc::ENOSPC),
    }

    // Space returns: the filler's four blocks come back — unlink, the
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
/// refused `ENOSPC` — a fresh file's first striped write against the full
/// store (the promotion allocates synchronously, so the write itself is
/// refused) counts one; writes the never-lossy ladder ACKs and
/// the fsync that later reports them count none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_write_enospc_refusals_gauge_counts_exactly_the_refused_writes() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF5; 16], "f15_wedge_gauge", 4).await;
    let _full = fill_store(&h, "filler.bin", 4).await;

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

// ---------------------------------------------------------------------------
// 4. The never-lossy law's boundary is ACKED custody (record §7 item 24)
// ---------------------------------------------------------------------------
//
// generic/751 on the 1.3.0 release chain: once the allocator refused for
// space, every later write into a FRESH block was still ACKed into a
// parked buffer, then degraded into the never-lossy staging fallback —
// staging filled, the parked set grew past the memory-budget cap (a 74 GiB
// daemon on a 24 GiB volume), the writeback ladder retried for ever, and
// the unmount lost every block that never landed. The law now: a write
// whose bytes would CREATE custody of a block nothing can land in — no
// mapping, no parked buffer, no staged copy, while the set's allocators
// have proven themselves out of supply — is refused `ENOSPC` BEFORE it is
// acked; custody the daemon already acked keeps the ladder exactly as
// before. Owner decision 2026-09-28 (option 1).

fn fresh_refusals(stats: &serde_json::Value) -> u64 {
    stats["metrics"]["write_fresh_block_enospc_refusals"]
        .as_u64()
        .expect("write_fresh_block_enospc_refusals rides the stats inode")
}

fn exhausted_volumes(stats: &serde_json::Value) -> u64 {
    stats["metrics"]["alloc_fresh_supply_exhausted"]
        .as_u64()
        .expect("alloc_fresh_supply_exhausted rides the stats inode")
}

/// The ACK-early pipeline shape (the default the fstests venue runs): a
/// full store, then eight whole-block writes into FRESH blocks of the
/// striped filler. Every one is refused `ENOSPC` at the WRITE — none is
/// acked — so no parked custody is created, the pipeline holds nothing,
/// the refusals ride their own gauge, the set publishes its exhaustion,
/// and `fsync` of the filler SUCCEEDS: nothing was acked that cannot
/// land. RED on the pre-fix tree: every write acked, eight parked buffers,
/// fsync `ENOSPC`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_block_writes_against_an_exhausted_set_are_refused_before_the_ack() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(None);
    let h = make([0xF6; 16], "f24_fresh_refused", 4).await;
    let ino = fill_store(&h, "filler.bin", 4).await;
    let stats0 = bounded_stats(&h).await;
    let g0 = fresh_refusals(&stats0);
    let parked0 = squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes();
    // The latch here is set by `fill_store`'s own probe (`allocate_block`
    // → `StorageFull` — the harness's terminal refusal, exactly what a
    // write's allocation would have produced); the live pin drives it
    // through a real write.
    assert_eq!(
        exhausted_volumes(&stats0),
        1,
        "the fill's terminal refusal latched the volume exhausted"
    );

    for b in 4u64..12 {
        let data = pattern(BS as usize, 0x40 + b as u8);
        let r = bounded_write(&h.fs, h.req, ino, b * BS, data).await;
        assert_eq!(
            r,
            Err(libc::ENOSPC),
            "block {b}: a fresh block on an exhausted set is refused at the write, never acked"
        );
    }
    let stats1 = bounded_stats(&h).await;
    assert_eq!(
        fresh_refusals(&stats1) - g0,
        8,
        "one count per refused write"
    );
    assert_eq!(
        squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes(),
        parked0,
        "a refused write creates no parked custody"
    );
    await_pipeline_drained(&h).await;
    assert_eq!(
        stats1["metrics"]["write_pipeline_inflight_bytes"].as_u64(),
        Some(0)
    );
    assert_eq!(
        bounded_fsync(&h, ino).await,
        Ok(()),
        "nothing was acked that cannot land, so the durability boundary is clean"
    );
    assert_eq!(
        bounded_getattr(&h, ino).await,
        4 * BS,
        "the size never led the data"
    );
}

/// The synchronous shape (`SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0`) — the
/// write itself owns the allocation: the same law, the same gauge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_block_writes_are_refused_on_the_synchronous_shape_too() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF7; 16], "f24_fresh_refused_sync", 4).await;
    let ino = fill_store(&h, "filler.bin", 4).await;
    let g0 = fresh_refusals(&bounded_stats(&h).await);
    for b in 4u64..8 {
        let data = pattern(BS as usize, 0x50 + b as u8);
        assert_eq!(
            bounded_write(&h.fs, h.req, ino, b * BS, data).await,
            Err(libc::ENOSPC),
            "block {b}"
        );
    }
    assert_eq!(fresh_refusals(&bounded_stats(&h).await) - g0, 4);
    assert_eq!(bounded_fsync(&h, ino).await, Ok(()));
}

/// ACKED custody keeps the never-lossy ladder. Capacity 5, fill 4; the
/// first half of block 4 is written (parked — custody, the block open),
/// then a sibling file takes the last block durably and the set is
/// exhausted. The SECOND half of block 4 is admitted (its block already
/// has custody), the completed block cannot land (`fsync` → `ENOSPC`,
/// honest), the bytes stay readable through the mount, and once the
/// sibling's block returns the block lands, `fsync` succeeds and every
/// byte reads back exact. Nothing acked is ever dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_custody_of_an_open_block_keeps_the_never_lossy_ladder() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF8; 16], "f24_acked_custody", 5).await;
    let ino = create(&h, "grower.bin").await;
    let base = pattern((4 * BS) as usize, 0x11);
    bounded_write(&h.fs, h.req, ino, 0, base)
        .await
        .expect("4 blocks fit");
    bounded_fsync(&h, ino).await.expect("durable");
    let half = (BS / 2) as usize;
    let first = pattern(half, 0x61);
    let second = pattern(half, 0x62);
    // The open block's first half: acked custody (parked, block 4 open).
    assert_eq!(
        bounded_write(&h.fs, h.req, ino, 4 * BS, first.clone()).await,
        Ok(half as u32)
    );
    // The sibling takes the last block durably: the set is now exhausted.
    let sib = create(&h, "sibling.bin").await;
    let sib_data = pattern(BS as usize, 0x77);
    bounded_write(&h.fs, h.req, sib, 0, sib_data.clone())
        .await
        .expect("the last block");
    bounded_fsync(&h, sib)
        .await
        .expect("the sibling is durable");
    let e = h
        .alloc
        .allocate_block()
        .await
        .expect_err("the store is full exactly");
    assert!(is_storage_full(&e), "{e}");
    let g0 = fresh_refusals(&bounded_stats(&h).await);

    // The second half: the block HAS custody — admitted, never refused.
    assert_eq!(
        bounded_write(&h.fs, h.req, ino, 4 * BS + half as u64, second.clone()).await,
        Ok(half as u32),
        "a segment into a block this mount already holds custody of is acked"
    );
    assert_eq!(
        fresh_refusals(&bounded_stats(&h).await),
        g0,
        "the fresh-block gauge never counts acked custody"
    );
    // Honest at the durability boundary, readable through the mount.
    assert_eq!(bounded_fsync(&h, ino).await, Err(libc::ENOSPC));
    let got = tokio::time::timeout(BOUND, h.fs.read(h.req, ino, 0, 4 * BS, BS as u32, 0))
        .await
        .expect("read answers")
        .expect("read")
        .data;
    assert_eq!(&got[..half], &first[..], "the acked first half serves");
    assert_eq!(&got[half..], &second[..], "the acked second half serves");

    // Space returns: the sibling's block comes back; the open block lands.
    tokio::time::timeout(BOUND, async {
        h.fs.unlink(h.req, 1, OsStr::new("sibling.bin"))
            .await
            .expect("unlink");
        let _ = h.fs.release(h.req, sib, sib, 0, 0, false).await;
        h.fs.reclaim_orphaned_batch(vec![sib]).await;
        h.fs.router.backend_router.reclaim_drain().await;
    })
    .await
    .expect("the delete + reclaim drain terminate");
    let t0 = Instant::now();
    while h.alloc.free_block_indices().is_empty() {
        assert!(
            t0.elapsed() < BOUND,
            "the freed block never reached the free list"
        );
        h.fs.router.backend_router.reclaim_drain().await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        bounded_fsync(&h, ino).await,
        Ok(()),
        "the acked block lands once space returns"
    );
    let got = tokio::time::timeout(BOUND, h.fs.read(h.req, ino, 0, 4 * BS, BS as u32, 0))
        .await
        .expect("read answers")
        .expect("read")
        .data;
    assert_eq!(&got[..half], &first[..]);
    assert_eq!(&got[half..], &second[..]);
}

/// The brim rewrite stays admitted: a whole-block rewrite of a MAPPED
/// block at fill 1.0 is space-neutral (contract 9's in-place arm) and the
/// exhaustion latch never touches it — the write lands, is durable, reads
/// back exact, and the fresh-block gauge is unmoved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_brim_rewrite_of_a_mapped_block_is_admitted_while_the_set_is_exhausted() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xF9; 16], "f24_brim_rewrite", 4).await;
    let ino = fill_store(&h, "filler.bin", 4).await;
    assert_eq!(exhausted_volumes(&bounded_stats(&h).await), 1);
    let g0 = fresh_refusals(&bounded_stats(&h).await);
    let want = pattern(BS as usize, 0x99);
    assert_eq!(
        bounded_write(&h.fs, h.req, ino, 0, want.clone()).await,
        Ok(BS as u32),
        "a rewrite of block 0 needs no fresh block"
    );
    assert_eq!(bounded_fsync(&h, ino).await, Ok(()), "and lands in place");
    let got = tokio::time::timeout(BOUND, h.fs.read(h.req, ino, 0, 0, BS as u32, 0))
        .await
        .expect("read answers")
        .expect("read")
        .data;
    assert_eq!(&got[..], &want[..]);
    assert_eq!(fresh_refusals(&bounded_stats(&h).await), g0);
}

/// The latch clears when supply returns: after the refusals, freeing the
/// filler lets a fresh file's write land and the volume reads un-exhausted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_exhaustion_latch_clears_when_supply_returns() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(None);
    let h = make([0xFA; 16], "f24_latch_clears", 4).await;
    let full = fill_store(&h, "filler.bin", 4).await;
    assert_eq!(
        bounded_write(&h.fs, h.req, full, 4 * BS, pattern(BS as usize, 0x21)).await,
        Err(libc::ENOSPC)
    );
    assert_eq!(exhausted_volumes(&bounded_stats(&h).await), 1);
    tokio::time::timeout(BOUND, async {
        h.fs.unlink(h.req, 1, OsStr::new("filler.bin"))
            .await
            .expect("unlink");
        let _ = h.fs.release(h.req, full, full, 0, 0, false).await;
        h.fs.reclaim_orphaned_batch(vec![full]).await;
        h.fs.router.backend_router.reclaim_drain().await;
    })
    .await
    .expect("the delete + reclaim drain terminate");
    let t0 = Instant::now();
    while h.alloc.free_block_indices().is_empty() {
        assert!(t0.elapsed() < BOUND);
        h.fs.router.backend_router.reclaim_drain().await;
        tokio::task::yield_now().await;
    }
    let fresh = create(&h, "fresh.bin").await;
    let data = pattern((2 * BS) as usize, 0x66);
    assert_eq!(
        bounded_write(&h.fs, h.req, fresh, 0, data.clone()).await,
        Ok((2 * BS) as u32),
        "supply returned: the write is admitted"
    );
    bounded_fsync(&h, fresh).await.expect("and durable");
    assert_eq!(
        exhausted_volumes(&bounded_stats(&h).await),
        0,
        "a landed allocation clears the latch"
    );
}

/// Review round 1, Issue 1: there is no belt that admits "one write per
/// window" — EIGHT concurrent fresh-block writers against a latched store
/// are ALL refused, nothing is acked, nothing parks. (The first build's
/// re-probe window cleared the latch for every concurrent writer at once:
/// generic/751's 32 jobs would have parked 32 blocks per window.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fresh_block_writers_against_a_latched_store_are_all_refused() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(None);
    let h = make([0xFB; 16], "f24_concurrent_refused", 4).await;
    let ino = fill_store(&h, "filler.bin", 4).await;
    let parked0 = squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes();
    let g0 = fresh_refusals(&bounded_stats(&h).await);
    let mut writes = Vec::new();
    for b in 4u64..12 {
        let fs = h.fs.clone();
        let req = h.req;
        let data = pattern(BS as usize, 0x70 + b as u8);
        writes.push(tokio::spawn(async move {
            bounded_write(&fs, req, ino, b * BS, data).await
        }));
    }
    for (i, w) in writes.into_iter().enumerate() {
        assert_eq!(
            w.await.expect("write task"),
            Err(libc::ENOSPC),
            "concurrent writer {i}: refused, never the belt's admitted one"
        );
    }
    assert_eq!(fresh_refusals(&bounded_stats(&h).await) - g0, 8);
    assert_eq!(
        squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes(),
        parked0,
        "no concurrent writer created parked custody"
    );
    assert_eq!(bounded_fsync(&h, ino).await, Ok(()));
}

/// Review round 1, Issue 2: a HOLE below EOF holds nothing. A file
/// pre-sized past the fill (`truncate` — `fallocate` mode 0 grows the size
/// the same way, no blocks) is every pre-sized shape: fio's default
/// `fallocate=native`, sparse images, databases. A write into its hole on
/// an exhausted set is refused like growth (the first build's size-floor
/// arm read every block below EOF as held and kept the sink for exactly
/// this shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_into_a_hole_below_eof_of_a_pre_sized_file_is_refused_when_exhausted() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(None);
    // Capacity 5: the sparse file's block 0 (striped by construction) +
    // the 4-block fill.
    let h = make([0xFC; 16], "f24_hole_below_eof", 5).await;
    let sparse = create(&h, "sparse.bin").await;
    bounded_write(&h.fs, h.req, sparse, 0, pattern(BS as usize, 0x08))
        .await
        .expect("block 0 lands");
    bounded_fsync(&h, sparse).await.expect("durable");
    tokio::time::timeout(
        BOUND,
        h.fs.setattr(
            h.req,
            sparse,
            None,
            fuse3::SetAttr {
                size: Some(8 * BS),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("setattr answers")
    .expect("truncate grows the size with no blocks");
    assert_eq!(bounded_getattr(&h, sparse).await, 8 * BS);
    let _full = fill_store(&h, "filler.bin", 4).await;
    let parked0 = squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes();
    let g0 = fresh_refusals(&bounded_stats(&h).await);
    for b in [1u64, 3, 7] {
        assert_eq!(
            bounded_write(
                &h.fs,
                h.req,
                sparse,
                b * BS,
                pattern(BS as usize, 0x80 + b as u8)
            )
            .await,
            Err(libc::ENOSPC),
            "block {b} of the pre-sized file is a hole: refused, never parked"
        );
    }
    assert_eq!(fresh_refusals(&bounded_stats(&h).await) - g0, 3);
    assert_eq!(
        squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes(),
        parked0
    );
    assert_eq!(
        bounded_fsync(&h, sparse).await,
        Ok(()),
        "nothing acked into the holes"
    );
}

/// Review round 1, Issue 6: "mapped ⇒ held" holds only where an in-place
/// arm can land the write. On a COMPRESSED volume the brim whole-block
/// rewrite declines (a transformed image's stored length varies with
/// content), so a rewrite of a mapped block needs the block nothing can
/// allocate: it is refused before the ack like growth. (The passthrough
/// twin — `a_brim_rewrite_of_a_mapped_block_is_admitted_while_the_set_is_
/// exhausted` — lands in place.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewrite_of_a_mapped_block_on_a_transformed_volume_is_refused_when_exhausted() {
    let _s = serial();
    let _r = restore();
    write_pipeline::set_depth_override(Some(0));
    let h = make([0xFD; 16], "f24_transformed_rewrite", 4).await;
    h.fs.router
        .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    // Incompressible content so every block stores as one whole chunk
    // and the store fills exactly like the passthrough fixture.
    let ino = create(&h, "filler.bin").await;
    let mut data = vec![0u8; (4 * BS) as usize];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in data.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
    bounded_write(&h.fs, h.req, ino, 0, data.clone())
        .await
        .expect("the store's capacity fits");
    bounded_fsync(&h, ino).await.expect("durable");
    let e = h
        .alloc
        .allocate_block()
        .await
        .expect_err("the store is full exactly");
    assert!(is_storage_full(&e), "{e}");
    let g0 = fresh_refusals(&bounded_stats(&h).await);
    let parked0 = squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes();
    let rewrite: Vec<u8> = data[..BS as usize].iter().map(|b| b ^ 0xFF).collect();
    assert_eq!(
        bounded_write(&h.fs, h.req, ino, 0, rewrite).await,
        Err(libc::ENOSPC),
        "a transformed volume's mapped block cannot take a rewrite in place: refused"
    );
    assert_eq!(fresh_refusals(&bounded_stats(&h).await) - g0, 1);
    assert_eq!(
        squeezefs::fuse_client::SqueezefsFilesystem::parked_gauge_bytes(),
        parked0
    );
    // The original bytes are intact and the file is clean.
    let got = tokio::time::timeout(BOUND, h.fs.read(h.req, ino, 0, 0, BS as u32, 0))
        .await
        .expect("read answers")
        .expect("read")
        .data;
    assert_eq!(&got[..], &data[..BS as usize]);
    assert_eq!(bounded_fsync(&h, ino).await, Ok(()));
}
