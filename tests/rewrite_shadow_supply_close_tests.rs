//! The supply-coupled rewrite-epoch close on co-writer lanes (finding 15's
//! parked-supply term — `.benchmarks/2026-09-06-free-grace-term1-fleet.md`
//! §3; the lever `.benchmarks/2026-09-07-rewrite-epoch-supply-close.md`;
//! design-rewrite-program §5.3, the KD-1.7 amendment).
//!
//! A rewrite of an existing block writes the new data to a fresh block B
//! and PARKS the old key A in the open epoch's shadow; A is freed only
//! when the epoch closes (KD-1.6). On a single writer a close returns A
//! to the local free list at once, so KD-1.7's "close on StorageFull,
//! retry once" works. On a CO-WRITER the freed A key does not come back
//! for seconds — the free ships to the authority, enters the grace ring,
//! is released to the authority's per-lane list and returns on a harvest
//! RPC — so the KD-1.7 retry finds nothing and the write falls into the
//! never-lossy ladder. On the s11 shape no routine trigger fires DURING
//! an iteration (full coverage needs the whole file; fsync/RELEASE come
//! at the boundary), so each co-writer parks its whole per-iteration
//! displacement against a lane share that must also hold live + new +
//! the previous burst still in flight.
//!
//! The lever closes a co-writer's open epochs AHEAD of the `StorageFull`,
//! on the ahead-refill tick, when the lane's reachable supply sits below
//! the watermark the refill already derives (`rate × horizon`, capped at
//! share/4 — the blocks one loop transit consumes): the parked A keys
//! enter the recycle loop while there is still headroom for them to come
//! back. Largest epoch first, until the yield covers the deficit
//! `watermark − reachable`, which is also the per-tick bound: a co-writer
//! never publishes more epochs in a tick than blocks it is short.
//!
//! Contracts:
//! 1. **The close fires ahead of the storm**: a laned co-writer with
//!    reachable supply below the watermark and parked A keys closes its
//!    epoch on the tick BEFORE any `StorageFull` — `rewrite_shadow_
//!    supply_closes` +1, the parked keys reach the free path (the frees
//!    ledger), the durable map names B, no `rewrite_shadow_fallbacks`.
//! 2. **The declines**: a stocked lane never closes
//!    (`…_declined_covered`); a starving lane with nothing parked has
//!    nothing to inject (`…_declined_no_parked`); the lever off is the
//!    shipped KD-1.6/1.7 shape byte-identically (no gauge moves, the epoch
//!    stays open until its routine trigger).
//! 3. **Scope**: a single-writer mount (unpartitioned) and an authority
//!    (lane 0, no harvest sink) never see the trigger — the arm installs
//!    nothing, the tick counts nothing, the epoch closes only on KD-1.6.
//! 4. **Largest first + the deficit bound**: many small epochs close in
//!    parked-count order and the tick stops once the yield covers the
//!    deficit; the rest are counted `…_bounded`, never published.
//! 5. **The plan is pure** (`supply_close_plan`): order, stop rule, ties.
//! 6. **The closed-loop model**: one co-writer lane on the s11 per-volume
//!    shape (share 512, 160 displaced per iteration, the product's
//!    watermark arithmetic, the product's planner) at the fleet's measured
//!    loop latencies — the lever bounds the parked term by the lane's
//!    headroom instead of the iteration's length.
//! 7. **The close plans MOUNT-wide — a starving volume's tick publishes the
//!    epoch whose parked keys live on its covered SIBLING** (finding 15's
//!    fpp re-attribution, `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`
//!    §8): under the lane-aware placement the two volumes' STOCKS are
//!    equalized (the 90 % band + the failover), so a parked key returning
//!    to either volume restocks the mount, and the per-volume plan the
//!    residue landing tried (candidates = keys parked on the asking
//!    volume; a "covered" sibling's tick declines) stranded the keys
//!    parked on the covered volume to the iteration boundary — the D row's
//!    routine-close share went 0.3–1 % → 14 % of the displaced keys. The
//!    two-volume rig pins the mount-wide law: B four short, F1 (4 keys on
//!    A) and F2 (2 on B) open ⇒ B's tick publishes F1, the largest, and
//!    the released keys re-enter the mount's stock on A, where the next
//!    placed allocation reaches them without a refusal.
//!
//! RED against `2a486273`: no supply-coupled trigger exists — a
//! co-writer's parked keys wait for the iteration boundary or the
//! `StorageFull`. Contract 7 RED against `0bd03455`: the plan counted the
//! asking volume's keys only.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_alloc_lane::{LaneHarvest, LaneHarvestSink};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::Metadata;
use squeezefs::routing::DataRouter;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the shipped posture on scope exit (knob hygiene). The W1 patch
/// path is disabled (`set_patch_max_bytes(0)`) and the overlay is off so
/// every whole-block overwrite rides the accumulation shadow feed.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::set_rewrite_shadow(true);
        squeezefs::routing::set_rewrite_supply_close(true);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
        squeezefs::block_reclaim::set_elision_class_all(false);
    }
}

/// The authority as the co-writer's harvest sees it: an EMPTY grant (the
/// lane's supply has not come back yet) carrying no bound-age hint. Its
/// presence is what makes the allocator a CO-WRITER's (the authority's
/// own lane wires no harvest sink).
struct EmptyAuthority {
    calls: AtomicU64,
}

impl EmptyAuthority {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU64::new(0),
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
                    bound_age_hint_ms: 0,
                    rtt_ms: 1,
                    release_ages_ms: Vec::new(),
                    grant_seq: 0,
                })
            })
        })
    }
}

#[derive(Clone, Copy)]
enum Posture {
    /// Lane 1 of 2 with a harvest sink — a co-writer's allocator.
    CoWriter,
    /// Lane 0 of 2, no harvest sink — the authority's own lane.
    Authority,
    /// Unpartitioned — every single-writer mount today.
    Solo,
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    alloc: Arc<BlockAllocator>,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

fn part(writers: u16, id: u16) -> AppendPartition {
    AppendPartition::new(writers, id).expect("partition")
}

async fn make_harness(test_id: &str, capacity_blocks: u64, posture: Posture) -> H {
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
    // Elision keeps a freed offset instantly reallocatable on this
    // file-backed harness (the bdev-classification seam) — the closed
    // loop is one process here, so the freed A keys land straight back on
    // the lane's own free list.
    squeezefs::block_reclaim::set_elision_class_all(true);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    ba.set_capacity_bytes(capacity_blocks * ba.chunk_size());
    match posture {
        Posture::CoWriter => {
            ba.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2");
            ba.set_lane_harvest_sink(EmptyAuthority::new().sink());
        }
        Posture::Authority => {
            ba.engage_alloc_lanes(part(2, 0)).expect("lane 0 of 2");
        }
        Posture::Solo => {}
    }
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
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    // The product's wiring point (`cowriter::install_client_halves`, after
    // the lane engagement): the router installs the supply-coupled close
    // on every laned allocator that harvests — a co-writer's, and only a
    // co-writer's.
    fs.router.arm_rewrite_supply_close();
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
        alloc: ba,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

/// The TWO-VOLUME co-writer (the s11 fleet's shape: two data volumes, one
/// lane share PER VOLUME — `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`):
/// the default slot aliases volume A's allocator exactly as a real mount's
/// registration does (`BackendRouter::build_backend` on the default device
/// — bare keys), volume B is a second registered backend with its own
/// device and laned allocator, and BOTH harvest from an empty authority.
struct H2 {
    /// Volume A's view — `h.alloc` IS volume A's allocator, so every
    /// single-volume helper works verbatim over the two-volume mount.
    h: H,
    b: Arc<BlockAllocator>,
    _backing_b: NamedTempFile,
}

async fn make_two_volume_harness(test_id: &str, capacity_blocks: u64) -> H2 {
    let h = make_harness(test_id, capacity_blocks, Posture::CoWriter).await;
    let backing_b = NamedTempFile::new().unwrap();
    std::fs::File::create(backing_b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let dev_b = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_b.path().to_str().unwrap(),
    ));
    let b = Arc::new(
        BlockAllocator::new(&format!("{test_id}_volb"))
            .await
            .unwrap(),
    );
    b.set_capacity_bytes(capacity_blocks * b.chunk_size());
    b.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2 on B");
    b.set_lane_harvest_sink(EmptyAuthority::new().sink());
    let router = &h.fs.router.backend_router;
    // Volume A = the default slot's alias (the first-volume registration).
    let (_, dev_a) = router.get_backend("backend_0").expect("default slot");
    router
        .publish_backend(
            "volA",
            Arc::new(squeezefs::routing::StorageBackend {
                device: dev_a,
                block_allocator: Arc::clone(&h.alloc),
            }),
        )
        .expect("register A");
    router
        .publish_backend(
            "volB",
            Arc::new(squeezefs::routing::StorageBackend {
                device: dev_b,
                block_allocator: Arc::clone(&b),
            }),
        )
        .expect("register B");
    // The product's wiring point, re-run over the now two-volume router
    // (the single-volume harness armed A already; the setter is a
    // OnceLock, so A keeps its sink and B gains one).
    h.fs.router.arm_rewrite_supply_close();
    assert!(h.alloc.lane_supply_close_installed() && b.lane_supply_close_installed());
    H2 {
        h,
        b,
        _backing_b: backing_b,
    }
}

/// Take `n` lane blocks off `alloc`'s reachable supply and HOLD them (the
/// live-data shape: minted, never freed) — what parks a volume's lane at a
/// chosen stock so the §5.9 lane-aware pick lands on its sibling.
async fn hold_lane_blocks(alloc: &BlockAllocator, n: u64) -> Vec<u64> {
    let mut held = Vec::new();
    for _ in 0..n {
        held.push(alloc.allocate_block().await.expect("lane block"));
    }
    held
}

/// Give held blocks back to the lane's LOCAL free list (the harvest that
/// already landed — `adopt_lane_free_grant` after the shipped-free retire,
/// the placement suite's `adopt_locally` shape).
fn release_held(alloc: &BlockAllocator, held: &[u64]) {
    let idxs: Vec<u64> = held.iter().map(|o| o / alloc.chunk_size()).collect();
    for off in held {
        alloc.retire_shipped_free_tracking(*off);
    }
    assert_eq!(alloc.adopt_lane_free_grant(&idxs), idxs.len() as u64);
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
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
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

/// Fresh striped fixture of `blocks` full blocks, drained + fsync'd.
async fn striped_fixture(h: &H, name: &str, blocks: u64, seed: u8) -> u64 {
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    write_at(h, ino, 0, &pattern((blocks * FBS) as usize, seed)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(h).await;
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "fixture premise: every block mapped"
    );
    ino
}

/// The DURABLE layout's block map, read straight from the backend.
async fn durable_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let bytes =
        h.fs.meta_backend
            .as_ref()
            .expect("backend")
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
    let layout: squeezefs::layout_wire::LayoutMetadata = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).expect("json layout")
    } else {
        bincode::deserialize(&bytes).expect("bincode layout")
    };
    layout.block_map.unwrap_or_default()
}

/// The RAM-authoritative map (what reads resolve through).
async fn ram_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    (*h.fs
        .router
        .fetch_metadata(&path)
        .await
        .expect("meta")
        .block_map
        .expect("mapped"))
    .clone()
}

/// Every gauge the lever moves, snapshotted together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ledger {
    supply_closes: u64,
    supply_close_blocks: u64,
    declined_covered: u64,
    declined_no_parked: u64,
    bounded: u64,
    swaps: u64,
    fallbacks: u64,
    open_epochs: u64,
    parked_bytes: u64,
    terminal_frees: u64,
}

fn ledger() -> Ledger {
    Ledger {
        supply_closes: METRICS.rewrite_shadow_supply_closes.load(Ordering::Relaxed),
        supply_close_blocks: METRICS
            .rewrite_shadow_supply_close_blocks
            .load(Ordering::Relaxed),
        declined_covered: METRICS
            .rewrite_shadow_supply_close_declined_covered
            .load(Ordering::Relaxed),
        declined_no_parked: METRICS
            .rewrite_shadow_supply_close_declined_no_parked
            .load(Ordering::Relaxed),
        bounded: METRICS
            .rewrite_shadow_supply_close_bounded
            .load(Ordering::Relaxed),
        swaps: METRICS.rewrite_shadow_swaps.load(Ordering::Relaxed),
        fallbacks: METRICS.rewrite_shadow_fallbacks.load(Ordering::Relaxed),
        open_epochs: METRICS.rewrite_shadow_open_epochs.load(Ordering::Relaxed),
        parked_bytes: METRICS.rewrite_shadow_parked_bytes.load(Ordering::Relaxed),
        terminal_frees: METRICS.block_free_reclaim_queued.load(Ordering::Relaxed)
            + METRICS.block_free_reclaim_elided.load(Ordering::Relaxed),
    }
}

/// Drive the allocator's watermark to `share/4` (the cap): a measured
/// horizon of 20 s (the fleet's serve-gap class) + the refresh floor and
/// a claim burst sampled over one second — `ceil(rate × horizon)`
/// overshoots the cap for any burst of ≥ 2 blocks, so the watermark reads
/// exactly `share/4`.
fn arm_watermark_at_cap(alloc: &BlockAllocator, share: u64, sample_ms: u64) -> u64 {
    alloc.note_harvest_hint(20_000, 0);
    alloc.sample_alloc_rate(sample_ms);
    let wm = alloc.watermark_blocks();
    assert_eq!(
        wm,
        share / 4,
        "premise: the watermark sits at its lane-share/4 cap"
    );
    wm
}

// ---------------------------------------------------------------------------
// Contract 1 — the close fires ahead of the storm.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_starving_laned_co_writer_closes_its_epoch_ahead_of_the_storage_full() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    // 64-block device, lane 1 of 2 ⇒ a 32-block share, watermark cap 8.
    let h = make_harness("supply_close_fires", 64, Posture::CoWriter).await;
    let share = 32u64;
    let blocks = 14u64;
    let ino = striped_fixture(&h, "f1", blocks, 3).await;
    let map_a = ram_block_map(&h, ino).await;
    h.alloc.sample_alloc_rate(1_000); // seeds the rate snapshots

    // Rewrite 13 of 14 blocks: coverage stays partial (no KD-1.6 close),
    // 13 A keys park, 13 B blocks mint — the lane is down to 5 reachable.
    let l0 = ledger();
    let rewritten = 13u64;
    let v1 = pattern((rewritten * FBS) as usize, 11);
    write_at(&h, ino, 0, &v1).await;
    quiesce(&h).await;
    let l1 = ledger();
    assert_eq!(
        l1.open_epochs - l0.open_epochs,
        1,
        "premise: one open epoch"
    );
    assert_eq!(
        l1.parked_bytes - l0.parked_bytes,
        rewritten * FBS,
        "premise: every displaced A key is parked"
    );
    assert_eq!(l1.swaps, l0.swaps, "premise: nothing closed yet");
    let reachable = h.alloc.lane_reachable_blocks();
    assert_eq!(
        reachable,
        share - blocks - rewritten,
        "premise: 5 reachable"
    );

    // The refill's own signal: watermark 8 > reachable 5 — the lane is
    // within one loop transit of exhaustion at its measured rate.
    let wm = arm_watermark_at_cap(&h.alloc, share, 2_000);
    assert!(
        reachable < wm,
        "premise: reachable {reachable} < watermark {wm}"
    );
    assert_eq!(
        h.alloc.lane_owed_blocks(),
        0,
        "premise: nothing is owed — the parked keys are the ONLY supply"
    );

    // The tick (the ahead-refill task's body, at the same sample instant
    // so the watermark holds): no harvest fires (owed 0), the supply
    // close does.
    let adopted = h.alloc.ahead_refill_tick(2_000).await;
    assert_eq!(adopted, 0, "the empty authority granted nothing");
    let l2 = ledger();
    assert_eq!(
        l2.supply_closes - l1.supply_closes,
        1,
        "the supply-coupled close fired once (rewrite_shadow_supply_closes)"
    );
    assert_eq!(
        l2.supply_close_blocks - l1.supply_close_blocks,
        rewritten,
        "…releasing every parked A key into the loop (rewrite_shadow_supply_close_blocks)"
    );
    assert_eq!(l2.swaps - l1.swaps, 1, "the close IS the one whole-tx swap");
    assert_eq!(
        l2.fallbacks, l1.fallbacks,
        "and it fired BEFORE any StorageFull — no KD-1.7 fallback was paid"
    );
    assert_eq!(l2.open_epochs, l0.open_epochs, "the epoch is closed");
    assert_eq!(l2.parked_bytes, l0.parked_bytes, "nothing stays parked");
    assert_eq!(
        l2.terminal_frees - l1.terminal_frees,
        rewritten,
        "the parked A keys reached the free path (§5.2: strictly after the save)"
    );
    assert_eq!(l2.bounded, l1.bounded, "one candidate, nothing left open");
    assert_eq!(
        l2.declined_covered, l1.declined_covered,
        "a starving lane is not `covered`"
    );
    assert_eq!(l2.declined_no_parked, l1.declined_no_parked);

    // The swap is durable: the DURABLE map names B for every rewritten
    // block and keeps A for the one untouched.
    let ram = ram_block_map(&h, ino).await;
    let durable = durable_block_map(&h, ino).await;
    for b in 0..rewritten as u32 {
        assert_eq!(durable.get(&b), ram.get(&b), "block {b}: swapped durably");
        assert_ne!(durable.get(&b), map_a.get(&b), "block {b}: no longer A");
    }
    assert_eq!(
        durable.get(&(rewritten as u32)),
        map_a.get(&(rewritten as u32)),
        "the un-rewritten block keeps A"
    );
    // And in this one-process loop the released supply is already back on
    // the lane (elided frees finish synchronously): reachable 5 → 18.
    assert_eq!(
        h.alloc.lane_reachable_blocks(),
        reachable + rewritten,
        "the released A keys re-entered the lane's reachable supply"
    );
    // A later tick with the lane restocked is `covered` — no second close.
    let l3 = ledger();
    h.alloc.ahead_refill_tick(2_000).await;
    let l4 = ledger();
    assert_eq!(l4.supply_closes, l3.supply_closes);
    assert_eq!(l4.declined_covered - l3.declined_covered, 1);
}

// ---------------------------------------------------------------------------
// Contract 2 — the declines and the lever.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stocked_lane_never_closes_and_the_ledger_says_covered() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    // 128-block device ⇒ a 64-block share, watermark cap 16.
    let h = make_harness("supply_close_covered", 128, Posture::CoWriter).await;
    let ino = striped_fixture(&h, "f1", 6, 4).await;
    h.alloc.sample_alloc_rate(1_000);
    let l0 = ledger();
    write_at(&h, ino, 0, &pattern((4 * FBS) as usize, 12)).await;
    quiesce(&h).await;
    let l1 = ledger();
    assert_eq!(
        l1.parked_bytes - l0.parked_bytes,
        4 * FBS,
        "premise: 4 parked"
    );
    let wm = arm_watermark_at_cap(&h.alloc, 64, 2_000);
    let reachable = h.alloc.lane_reachable_blocks();
    assert!(reachable >= wm, "premise: stocked ({reachable} ≥ {wm})");

    h.alloc.ahead_refill_tick(2_000).await;
    let l2 = ledger();
    assert_eq!(
        l2.supply_closes, l1.supply_closes,
        "a stocked lane never closes"
    );
    assert_eq!(l2.supply_close_blocks, l1.supply_close_blocks);
    assert_eq!(l2.swaps, l1.swaps, "no swap");
    assert_eq!(l2.open_epochs, l1.open_epochs, "the epoch stays open");
    assert_eq!(l2.parked_bytes, l1.parked_bytes, "the keys stay parked");
    assert_eq!(
        l2.declined_covered - l1.declined_covered,
        1,
        "the decision ledger names the decline: covered"
    );
    assert_eq!(l2.declined_no_parked, l1.declined_no_parked);
    assert_eq!(l2.bounded, l1.bounded);
    // The routine trigger still owns the close (KD-1.6: fsync).
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    let l3 = ledger();
    assert_eq!(l3.swaps - l2.swaps, 1);
    assert_eq!(
        l3.supply_closes, l2.supply_closes,
        "…and it is not a supply close"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_starving_lane_with_nothing_parked_declines_no_parked() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    let h = make_harness("supply_close_no_parked", 64, Posture::CoWriter).await;
    // Fresh ingest only: 28 of the 32-block share, nothing displaced, no
    // epoch — the KD-1.7-only shape (nothing to inject).
    let _ino = striped_fixture(&h, "f1", 28, 5).await;
    h.alloc.sample_alloc_rate(1_000);
    for _ in 0..2 {
        h.alloc.allocate_block().await.expect("mint");
    }
    let wm = arm_watermark_at_cap(&h.alloc, 32, 2_000);
    let reachable = h.alloc.lane_reachable_blocks();
    assert!(reachable < wm, "premise: starving ({reachable} < {wm})");
    let l1 = ledger();
    assert_eq!(l1.open_epochs, 0, "premise: no epoch is open");

    h.alloc.ahead_refill_tick(2_000).await;
    let l2 = ledger();
    assert_eq!(
        l2.declined_no_parked - l1.declined_no_parked,
        1,
        "nothing parked ⇒ the ledger says so"
    );
    assert_eq!(l2.supply_closes, l1.supply_closes);
    assert_eq!(l2.declined_covered, l1.declined_covered);
    assert_eq!(l2.swaps, l1.swaps);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lever_off_is_the_shipped_kd16_kd17_shape_verbatim() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(false);
    let h = make_harness("supply_close_lever_off", 64, Posture::CoWriter).await;
    let blocks = 14u64;
    let ino = striped_fixture(&h, "f1", blocks, 6).await;
    h.alloc.sample_alloc_rate(1_000);
    let l0 = ledger();
    write_at(&h, ino, 0, &pattern((13 * FBS) as usize, 13)).await;
    quiesce(&h).await;
    let l1 = ledger();
    assert_eq!(
        l1.parked_bytes - l0.parked_bytes,
        13 * FBS,
        "premise: 13 parked"
    );
    let wm = arm_watermark_at_cap(&h.alloc, 32, 2_000);
    assert!(h.alloc.lane_reachable_blocks() < wm, "premise: starving");

    // The same tick that fires with the lever on moves NOTHING with it off
    // — not a close, not a decline: the shipped shape byte-identically.
    h.alloc.ahead_refill_tick(2_000).await;
    let l2 = ledger();
    assert_eq!(l2, l1, "SQUEEZEFS_REWRITE_SUPPLY_CLOSE=0: no gauge moves");
    // The epoch closes on its routine trigger exactly as shipped.
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    let l3 = ledger();
    assert_eq!(l3.swaps - l2.swaps, 1, "KD-1.6 fsync close");
    assert_eq!(l3.supply_closes, l2.supply_closes);
    assert_eq!(l3.terminal_frees - l2.terminal_frees, 13);
}

// ---------------------------------------------------------------------------
// Contract 3 — scope: single writers and authorities never see it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_writer_and_an_authority_never_see_the_trigger() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    for (posture, id) in [
        (Posture::Solo, "supply_close_solo"),
        (Posture::Authority, "supply_close_authority"),
    ] {
        let h = make_harness(id, 64, posture).await;
        assert!(
            !h.alloc.lane_supply_close_installed(),
            "{id}: the arm installs nothing on a non-harvesting allocator"
        );
        let blocks = 14u64;
        let ino = striped_fixture(&h, "f1", blocks, 7).await;
        h.alloc.sample_alloc_rate(1_000);
        let l0 = ledger();
        write_at(&h, ino, 0, &pattern((13 * FBS) as usize, 14)).await;
        quiesce(&h).await;
        let l1 = ledger();
        assert_eq!(l1.parked_bytes - l0.parked_bytes, 13 * FBS, "{id}: premise");
        if matches!(posture, Posture::Authority) {
            // The authority's lane derives a watermark too; its reachable
            // supply is below it — and still nothing fires.
            let wm = arm_watermark_at_cap(&h.alloc, 32, 2_000);
            assert!(h.alloc.lane_reachable_blocks() < wm);
        } else {
            h.alloc.sample_alloc_rate(2_000);
            assert_eq!(h.alloc.watermark_blocks(), 0, "solo: no watermark exists");
        }
        assert_eq!(
            h.alloc.supply_close_deficit(),
            None,
            "{id}: the decision itself declines uncounted where no close is wired"
        );
        h.alloc.ahead_refill_tick(2_000).await;
        let l2 = ledger();
        assert_eq!(
            l2, l1,
            "{id}: KD-1.6/1.7 exactly as shipped — no gauge moves"
        );
        h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
        assert_eq!(ledger().swaps - l2.swaps, 1, "{id}: the routine close");
    }
}

// ---------------------------------------------------------------------------
// Contract 4 — largest first, the deficit bounds the tick.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_small_epochs_close_largest_first_and_the_deficit_bounds_the_tick() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    // 64-block device ⇒ share 32, cap 8. Eight 2-block files (16 blocks)
    // + one 4-block file (4) = 20 live. Rewrite one block of each small
    // file (8 epochs × 1 parked) and 3 of the big one (1 epoch × 3): 31
    // used, 1 reachable, watermark 8 ⇒ deficit 7.
    let h = make_harness("supply_close_bound", 64, Posture::CoWriter).await;
    let mut small = Vec::new();
    for i in 0..8u8 {
        small.push(striped_fixture(&h, &format!("s{i}"), 2, 20 + i).await);
    }
    let big = striped_fixture(&h, "big", 4, 40).await;
    h.alloc.sample_alloc_rate(1_000);
    let l0 = ledger();
    for (i, ino) in small.iter().enumerate() {
        write_at(&h, *ino, 0, &pattern(FBS as usize, 50 + i as u8)).await;
    }
    write_at(&h, big, 0, &pattern((3 * FBS) as usize, 60)).await;
    quiesce(&h).await;
    let l1 = ledger();
    assert_eq!(
        l1.open_epochs - l0.open_epochs,
        9,
        "premise: nine open epochs"
    );
    assert_eq!(
        l1.parked_bytes - l0.parked_bytes,
        11 * FBS,
        "premise: 11 parked"
    );
    assert_eq!(h.alloc.lane_reachable_blocks(), 1, "premise: 1 reachable");
    let wm = arm_watermark_at_cap(&h.alloc, 32, 2_000);
    let deficit = wm - 1;
    assert_eq!(deficit, 7);

    h.alloc.ahead_refill_tick(2_000).await;
    let l2 = ledger();
    // Largest first: the 3-parked epoch (yield 3), then four 1-parked
    // epochs (yield 7 ≥ deficit 7) — five closes, four candidates left
    // open and counted, never published.
    assert_eq!(
        l2.supply_closes - l1.supply_closes,
        5,
        "five closes cover a deficit of 7"
    );
    assert_eq!(l2.supply_close_blocks - l1.supply_close_blocks, 7);
    assert_eq!(l2.swaps - l1.swaps, 5);
    assert_eq!(
        l2.bounded - l1.bounded,
        4,
        "the deficit bound left four epochs open"
    );
    assert_eq!(l2.open_epochs - l0.open_epochs, 4);
    assert_eq!(l2.parked_bytes - l0.parked_bytes, 4 * FBS);
    assert_eq!(l2.terminal_frees - l1.terminal_frees, 7);
    // The big epoch went first.
    let big_durable = durable_block_map(&h, big).await;
    let big_ram = ram_block_map(&h, big).await;
    for b in 0..3u32 {
        assert_eq!(
            big_durable.get(&b),
            big_ram.get(&b),
            "big block {b} swapped"
        );
    }
    // The four survivors close on their routine trigger, untouched.
    for ino in &small {
        h.fs.fsync(h.req, *ino, 0, false).await.expect("fsync");
    }
    let l3 = ledger();
    assert_eq!(l3.swaps - l2.swaps, 4);
    assert_eq!(l3.open_epochs, l0.open_epochs);
    assert_eq!(l3.supply_closes, l2.supply_closes);
}

// ---------------------------------------------------------------------------
// Contract 7 — the close plans MOUNT-wide on a two-volume co-writer (finding
// 15's fpp re-attribution, `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md` §8).
// ---------------------------------------------------------------------------

/// Two volumes, F1 (5 blocks) placed on A and F2 (3 blocks) on B; rewrite 4
/// of F1 and 2 of F2 so two epochs park 4 A-volume keys and 2 B-volume
/// keys. Returns the fixture with both epochs open and the lane stocks
/// the fixture arithmetic predicts: `a_stock = 32 − 5 − new_a`, `b_stock =
/// 32 − 3 − new_b`, where `new_a`/`new_b` count the rewrites' fresh B keys
/// the round-robin pick placed on each volume.
struct ParkedFixture {
    h2: H2,
    f1: u64,
    f2: u64,
    a_stock: u64,
    b_stock: u64,
}

async fn two_volume_parked_fixture(test_id: &str) -> ParkedFixture {
    let h2 = make_two_volume_harness(test_id, 64).await;
    let (a, b) = (&h2.h.alloc, &h2.b);
    a.sample_alloc_rate(1_000);
    b.sample_alloc_rate(1_000);
    // F1 on A: B's whole lane held, so the lane-aware pick admits only A.
    let held_b = hold_lane_blocks(b, b.lane_reachable_blocks()).await;
    assert_eq!(b.lane_reachable_blocks(), 0, "premise: B's lane is dry");
    let f1 = striped_fixture(&h2.h, "f1", 5, 3).await;
    for key in ram_block_map(&h2.h, f1).await.values() {
        assert!(
            !key.contains("://"),
            "premise: F1's block {key} is on A (bare key)"
        );
    }
    release_held(b, &held_b);
    // F2 on B: A's remaining lane held.
    let held_a = hold_lane_blocks(a, a.lane_reachable_blocks()).await;
    assert_eq!(a.lane_reachable_blocks(), 0, "premise: A's lane is dry");
    let f2 = striped_fixture(&h2.h, "f2", 3, 5).await;
    for key in ram_block_map(&h2.h, f2).await.values() {
        assert!(
            key.starts_with("volB://"),
            "premise: F2's block {key} is on B"
        );
    }
    release_held(a, &held_a);
    // Partial rewrites (coverage stays partial — no KD-1.6 close): F1's
    // four displaced keys park on A, F2's two on B.
    let l0 = ledger();
    write_at(&h2.h, f1, 0, &pattern((4 * FBS) as usize, 11)).await;
    write_at(&h2.h, f2, 0, &pattern((2 * FBS) as usize, 13)).await;
    quiesce(&h2.h).await;
    let l1 = ledger();
    assert_eq!(
        l1.open_epochs - l0.open_epochs,
        2,
        "premise: two open epochs"
    );
    assert_eq!(
        l1.parked_bytes - l0.parked_bytes,
        6 * FBS,
        "premise: 4 + 2 parked"
    );
    assert_eq!(l1.swaps, l0.swaps, "premise: nothing closed yet");
    // The fresh B keys' placement (the round-robin pick, both volumes in
    // band) — what the stock arithmetic below needs.
    let f1_ram = ram_block_map(&h2.h, f1).await;
    let f2_ram = ram_block_map(&h2.h, f2).await;
    let fresh = (0..4u32)
        .map(|blk| f1_ram[&blk].as_str())
        .chain((0..2u32).map(|blk| f2_ram[&blk].as_str()));
    let new_b = fresh.clone().filter(|k| k.starts_with("volB://")).count() as u64;
    let new_a = fresh.count() as u64 - new_b;
    let a_stock = 32 - 5 - new_a;
    let b_stock = 32 - 3 - new_b;
    assert_eq!(
        settled_reachable(a, a_stock).await,
        a_stock,
        "premise: A's stock"
    );
    assert_eq!(
        settled_reachable(b, b_stock).await,
        b_stock,
        "premise: B's stock"
    );
    ParkedFixture {
        h2,
        f1,
        f2,
        a_stock,
        b_stock,
    }
}

/// The lane's reachable count once the closed epoch's freed keys have
/// landed on the lane's free list (the close's frees are terminal frees
/// whose `finish_free` runs behind the tick). The debt drainer's trim
/// claim windows (KD-4.4: with the virgin tail minted, the pressure venue
/// claims an elided-debt offset OUT of the free list for one device command
/// and puts it back) do not move this count — a windowed offset is
/// reachable supply the funnel parks for, and reading the batch as a
/// deficit was the 2026-09-08 placement-refresh flake
/// (`.benchmarks/2026-09-08-placement-refresh-race.md`). Bounded spin;
/// returns the last read.
async fn settled_reachable(alloc: &BlockAllocator, expect: u64) -> u64 {
    let t0 = std::time::Instant::now();
    loop {
        let n = alloc.lane_reachable_blocks();
        if n == expect || t0.elapsed() > std::time::Duration::from_secs(10) {
            return n;
        }
        tokio::task::yield_now().await;
    }
}

/// Park volume `alloc`'s lane at `reachable` blocks (from `stock`) and arm
/// its watermark at the share/4 cap (8): the tick's deficit is `8 −
/// reachable`.
async fn starve_to(alloc: &BlockAllocator, stock: u64, reachable: u64) -> u64 {
    assert!(
        stock >= reachable,
        "premise: {stock} stocked, want {reachable}"
    );
    let _held = hold_lane_blocks(alloc, stock - reachable).await;
    assert_eq!(settled_reachable(alloc, reachable).await, reachable);
    let wm = arm_watermark_at_cap(alloc, 32, 2_000);
    wm - reachable
}

/// The plan is MOUNT-wide: B is 4 short with F1 (4 keys, all on A) and
/// F2 (2 keys, all on B) open, and B's tick publishes F1 — the largest
/// epoch — not the epoch whose keys happen to live on B. The yield lands
/// on A, and it IS the mount's supply: the next placed allocation reaches
/// it there (the lane-aware pick leaves the exhausted B out of the band)
/// with no refusal and no park. F2 is left open and counted `bounded`.
/// RED against `0bd03455`: the per-volume plan published F2, left F1's
/// four keys parked, and a second B tick declined `offvolume` — the keys
/// stayed parked until F1's routine trigger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_starving_volume_publishes_the_mounts_largest_epoch_whose_keys_restock_the_sibling() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::routing::set_rewrite_supply_close(true);
    let fx = two_volume_parked_fixture("supply_close_mount_wide").await;
    let (h2, f1, f2) = (&fx.h2, fx.f1, fx.f2);
    let (a, b) = (&h2.h.alloc, &h2.b);
    let f1_ram = ram_block_map(&h2.h, f1).await;
    let f2_ram = ram_block_map(&h2.h, f2).await;
    let deficit = starve_to(b, fx.b_stock, 4).await;
    assert_eq!(deficit, 4, "premise: B is 4 blocks short of its watermark");
    // Park A's stock BELOW B's so the round-robin band holds B alone
    // before the close: the fungibility half below needs the pick to move
    // to A on the strength of F1's released keys.
    let a_low = 2u64;
    let _held_a = hold_lane_blocks(a, fx.a_stock - a_low).await;
    assert_eq!(settled_reachable(a, a_low).await, a_low);

    let l1 = ledger();
    let adopted = b.ahead_refill_tick(2_000).await;
    assert_eq!(adopted, 0, "the empty authority granted nothing");
    let l2 = ledger();
    assert_eq!(
        l2.supply_closes - l1.supply_closes,
        1,
        "ONE close: the largest epoch, F1"
    );
    assert_eq!(
        l2.supply_close_blocks - l1.supply_close_blocks,
        4,
        "F1's four parked keys released — every one of them on A"
    );
    assert_eq!(
        l2.bounded - l1.bounded,
        1,
        "F2 left open: 4 ≥ 4 covered the deficit"
    );
    assert_eq!(l1.open_epochs - l2.open_epochs, 1, "F1 closed, F2 open");
    assert_eq!(l1.parked_bytes - l2.parked_bytes, 4 * FBS);
    let f1_durable = durable_block_map(&h2.h, f1).await;
    for blk in 0..4u32 {
        assert_eq!(
            f1_durable.get(&blk),
            f1_ram.get(&blk),
            "F1 block {blk} swapped durably"
        );
    }
    // The yield is the MOUNT's: A went 2 → 6 and B stayed at 4, and the
    // placed allocation — the write path's one pick+allocate act — lands
    // on A (its stock fraction now leads the band) with no refusal.
    assert_eq!(
        settled_reachable(a, a_low + 4).await,
        a_low + 4,
        "A restocked"
    );
    assert_eq!(settled_reachable(b, 4).await, 4, "B unchanged");
    let refusals0 = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);
    // The §5.9 band at the health worker's next refresh: A's 6/32 leads,
    // B's 4/32 sits under 90 % of it and leaves the band.
    h2.h.fs.router.backend_router.refresh_placement_table();
    let (be_id, _, _, _) =
        h2.h.fs
            .router
            .backend_router
            .allocate_placed_block()
            .await
            .expect("the released keys are reachable supply");
    assert_eq!(be_id, "volA", "the pick reached the restocked sibling");
    assert_eq!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed),
        refusals0,
        "no refusal: a key returning to EITHER volume restocks the mount"
    );
    // F2's two keys are the next largest: a second B tick, still short,
    // publishes them (nothing is stranded on a covered sibling).
    let l3 = ledger();
    b.ahead_refill_tick(2_000).await;
    let l4 = ledger();
    assert_eq!(
        l4.supply_closes - l3.supply_closes,
        1,
        "F2 closes on the next tick"
    );
    assert_eq!(l4.supply_close_blocks - l3.supply_close_blocks, 2);
    assert_eq!(l4.open_epochs, l1.open_epochs - 2, "both epochs closed");
    assert_eq!(settled_reachable(b, 6).await, 6, "B restocked by F2's keys");
    let f2_durable = durable_block_map(&h2.h, f2).await;
    for blk in 0..2u32 {
        assert_eq!(
            f2_durable.get(&blk),
            f2_ram.get(&blk),
            "F2 block {blk} swapped durably"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract 5 — the plan is pure.
// ---------------------------------------------------------------------------

#[test]
fn the_plan_closes_largest_first_until_the_deficit_is_covered() {
    use squeezefs::routing::supply_close_plan;
    // Largest first; stop at cover; the rest are left open.
    let (close, left) = supply_close_plan(10, vec![(1, 2), (2, 320), (3, 5)]);
    assert_eq!(close, vec![2], "one 320-block epoch covers a deficit of 10");
    assert_eq!(left, 2);
    // Accumulate across epochs until the deficit is covered.
    let (close, left) = supply_close_plan(7, vec![(1, 1), (2, 3), (3, 1), (4, 1), (5, 1), (6, 1)]);
    assert_eq!(close, vec![2, 1, 3, 4, 5], "3 + 1 + 1 + 1 + 1 = 7 ≥ 7");
    assert_eq!(left, 1);
    // Ties break by ino (deterministic), and a deficit larger than the
    // whole parked population closes everything — bounded by the
    // candidates, and every candidate yields ≥ 1 toward the deficit.
    let (close, left) = supply_close_plan(100, vec![(9, 1), (4, 1), (7, 1)]);
    assert_eq!(close, vec![4, 7, 9]);
    assert_eq!(left, 0);
    // An epoch parking nothing is never a candidate; a zero deficit
    // closes nothing.
    let (close, left) = supply_close_plan(5, vec![(1, 0), (2, 0)]);
    assert!(close.is_empty());
    assert_eq!(left, 0);
    let (close, left) = supply_close_plan(0, vec![(1, 4)]);
    assert!(close.is_empty());
    assert_eq!(left, 1);
}

// ---------------------------------------------------------------------------
// Contract 6 — the closed-loop model on the s11 per-volume shape.
// ---------------------------------------------------------------------------

/// One co-writer lane on one data volume (the fleet's per-volume numbers,
/// `.benchmarks/2026-09-06-free-grace-term1-fleet.md` §3): a 512-block
/// share, 160 displaced blocks per iteration (a 1.25 GiB slice of the
/// 10 GiB file over two volumes), the steady rate the row sustained. The
/// model performs only the ACTS; the product decides every step — the
/// watermark is `BlockAllocator::sample_alloc_rate`'s arithmetic and the
/// closing set is `routing::supply_close_plan`.
#[derive(Clone, Copy)]
struct Shape {
    label: &'static str,
    share: u64,
    iteration_blocks: u64,
    rate_per_s: u64,
    /// The recycle loop's transit (free → ring → release → lane list →
    /// harvest): the horizon the co-writer learns from the harvest hint.
    loop_ms: u64,
    /// The iteration boundary (fsync + barrier).
    pause_ms: u64,
    iterations: u64,
}

#[derive(Debug, Clone, Copy)]
struct Row {
    lever: bool,
    stalls: u64,
    closes: u64,
    peak_parked: u64,
    /// Mean residence of a displaced key in the parked state, ms.
    parked_residence_ms: f64,
    /// Mean lane-reachable supply over the run, blocks.
    mean_reachable: f64,
}

fn run_model(shape: &Shape, lever: bool) -> Row {
    const TICK_MS: u64 = 1_000;
    let mut now = 0u64;
    let live = shape.iteration_blocks;
    let mut reachable = shape.share - live;
    let mut parked: VecDeque<u64> = VecDeque::new(); // park instants
    let mut in_loop: VecDeque<(u64, u64)> = VecDeque::new(); // (return_at, count)
    let mut acc = 0u64; // milli-blocks of demand
    let mut displaced_this_iteration = 0u64;
    let mut iteration = 0u64;
    let mut pause_until = 0u64;
    let mut next_tick = TICK_MS;
    // The product's rate EWMA (milli-blocks/s) and its inputs.
    let (mut claims, mut last_claims, mut ewma) = (0u64, 0u64, 0u64);
    let (mut stalls, mut closes, mut peak_parked) = (0u64, 0u64, 0u64);
    let (mut residence_sum, mut residence_n) = (0u64, 0u64);
    let (mut reachable_sum, mut samples) = (0u64, 0u64);

    let mut close = |parked: &mut VecDeque<u64>,
                     in_loop: &mut VecDeque<(u64, u64)>,
                     now: u64,
                     closes: &mut u64| {
        if parked.is_empty() {
            return;
        }
        let n = parked.len() as u64;
        for t in parked.drain(..) {
            residence_sum += now - t;
            residence_n += 1;
        }
        in_loop.push_back((now + shape.loop_ms, n));
        *closes += 1;
    };

    while iteration < shape.iterations {
        now += 1;
        while let Some(&(at, n)) = in_loop.front() {
            if at > now {
                break;
            }
            in_loop.pop_front();
            reachable += n;
        }
        if now >= pause_until {
            acc += shape.rate_per_s;
            while acc >= 1_000 {
                if reachable == 0 {
                    // KD-1.7 as shipped: the StorageFull closes the epoch
                    // (the parked keys enter the loop NOW) and the write
                    // retries once — on a co-writer the retry finds
                    // nothing back yet, so the write waits.
                    close(&mut parked, &mut in_loop, now, &mut closes);
                    stalls += 1;
                    acc = 1_000; // the demand waits; a stall per ms of waiting
                    break;
                }
                acc -= 1_000;
                reachable -= 1;
                claims += 1;
                parked.push_back(now);
                peak_parked = peak_parked.max(parked.len() as u64);
                displaced_this_iteration += 1;
                if displaced_this_iteration == shape.iteration_blocks {
                    // The boundary: fsync/RELEASE close (KD-1.6).
                    close(&mut parked, &mut in_loop, now, &mut closes);
                    displaced_this_iteration = 0;
                    iteration += 1;
                    pause_until = now + shape.pause_ms;
                    acc = 0;
                    break;
                }
            }
        }
        if now >= next_tick {
            next_tick += TICK_MS;
            reachable_sum += reachable;
            samples += 1;
            // `sample_alloc_rate`: EWMA α = 1/4 over one second of claims,
            // watermark = ceil(rate × horizon) capped at share/4.
            let inst_mblk = (claims - last_claims) * 1_000;
            last_claims = claims;
            ewma = ewma.saturating_sub(ewma.div_ceil(4)) + inst_mblk / 4;
            let inflight = if ewma == 0 {
                0
            } else {
                ewma.saturating_mul(shape.loop_ms).div_ceil(1_000_000)
            };
            let watermark = inflight.min(shape.share / 4);
            if lever && watermark > 0 && reachable < watermark {
                let shortfall = watermark - reachable;
                let (to_close, _left) = squeezefs::routing::supply_close_plan(
                    shortfall,
                    vec![(1, parked.len() as u64)],
                );
                if !to_close.is_empty() {
                    close(&mut parked, &mut in_loop, now, &mut closes);
                }
            }
        }
    }
    Row {
        lever,
        stalls,
        closes,
        peak_parked,
        parked_residence_ms: if residence_n == 0 {
            0.0
        } else {
            residence_sum as f64 / residence_n as f64
        },
        mean_reachable: if samples == 0 {
            0.0
        } else {
            reachable_sum as f64 / samples as f64
        },
    }
}

fn render(shape: &Shape, r: &Row) -> String {
    format!(
        "MODEL {label} loop {loop_ms} ms supply_close={lever}: stalls {stalls} ms | closes {closes} | \
         peak parked {peak} blk | parked residence mean {res:.0} ms | mean reachable {reach:.0} blk",
        label = shape.label,
        loop_ms = shape.loop_ms,
        lever = r.lever,
        stalls = r.stalls,
        closes = r.closes,
        peak = r.peak_parked,
        res = r.parked_residence_ms,
        reach = r.mean_reachable,
    )
}

#[test]
fn the_iteration_model_bounds_the_parked_term_by_the_lanes_headroom() {
    // The s11 per-volume shape at the fleet's measured loop transits: the
    // term-1 row's hold + RTT + one floor (≈ 3 s) and the serve
    // timeline's gap floor (9 s).
    let base = Shape {
        label: "s11-per-volume",
        share: 512,
        iteration_blocks: 160,
        rate_per_s: 25,
        loop_ms: 3_000,
        pause_ms: 1_000,
        iterations: 12,
    };
    // The control: at the 3 s transit the boundary-only shape never
    // starves (192 spare blocks against a 160-block iteration), the lane
    // never dips below the watermark, and the lever fires NOTHING — the
    // two rows are identical.
    let off = run_model(&base, false);
    let on = run_model(&base, true);
    eprintln!("{}", render(&base, &off));
    eprintln!("{}", render(&base, &on));
    assert_eq!(
        off.stalls, 0,
        "control premise: no starvation at a 3 s transit"
    );
    assert_eq!(on.closes, off.closes, "control: the lever fired nothing");
    assert_eq!(on.peak_parked, off.peak_parked);
    assert_eq!(on.stalls, 0);

    // The fleet's transits (the serve timeline's 9–31 s gaps). The shipped
    // shape (KD-1.6 at the boundary + KD-1.7 at the StorageFull) parks the
    // whole iteration and starves once the previous burst stops returning
    // in time; the lever fires BEFORE the StorageFull, so a parked key's
    // residence is bounded by the tick and the starvation shrinks where
    // the PARKING — not the loop's own transit — is the binding term.
    // At the 24 s transit the loop itself binds (Little: 352 circulating
    // blocks ÷ 24 s < the 25 blk/s demand) and the lever can only stop
    // adding to it.
    for loop_ms in [9_000u64, 12_000, 15_000, 24_000] {
        let shape = Shape { loop_ms, ..base };
        let off = run_model(&shape, false);
        let on = run_model(&shape, true);
        eprintln!("{}", render(&shape, &off));
        eprintln!("{}", render(&shape, &on));
        assert_eq!(
            off.peak_parked, base.iteration_blocks,
            "loop {loop_ms}: the shipped shape parks the whole iteration"
        );
        assert!(
            on.parked_residence_ms * 2.0 < off.parked_residence_ms,
            "loop {loop_ms}: a parked key waits less than half as long ({:.0} vs {:.0} ms)",
            on.parked_residence_ms,
            off.parked_residence_ms
        );
        assert!(
            on.stalls <= off.stalls,
            "loop {loop_ms}: the lever never adds starvation ({} vs {} ms)",
            on.stalls,
            off.stalls
        );
        if loop_ms <= 15_000 && off.stalls > 0 {
            assert!(
                on.stalls * 2 < off.stalls,
                "loop {loop_ms}: where the parking binds, the lever removes more than half \
                 the starvation ({} vs {} ms)",
                on.stalls,
                off.stalls
            );
        }
    }
    let starving = Shape {
        loop_ms: 12_000,
        ..base
    };
    assert!(
        run_model(&starving, false).stalls > 0,
        "premise: the shipped shape starves at a 12 s transit"
    );
}
