//! **Lane-aware write placement on a laned co-writer** — the fleet finding
//! of 2026-09-07 (`.benchmarks/2026-09-07-cowriter-lane-aware-placement.md`;
//! per-second samples `/tmp/five/d4/keep-sampler-shakedown/samples/m5*.jsonl`).
//!
//! On the s11-mpiio fleet (authority + 8 co-writers, TWO data volumes, W = 16)
//! every co-writer's lane owns 512 blocks PER VOLUME, and recycled blocks
//! return to it PER VOLUME (the authority's grace ring releases onto the free
//! list of the volume the block lives on; the co-writer harvests per
//! allocator). The write path does ONE §5.9 placement pick and allocates on
//! that volume only, and the pick's weight is the DEVICE fill — which on a
//! dense-full co-writer view is ≈ 0 for both volumes (the band is noise, and
//! stale for the 5 s health-worker cadence while the lane supply churns at
//! hundreds of blocks per second). Sampled (5 s deltas):
//!
//! ```text
//! m50 t=80   reachable 328 (summed)   harvests +1,631   harvested +374   ENOSPC +1,625
//! m50 t=100  reachable 0→35  hint 512→448  harvests +2,264  harvested +73  ENOSPC +2,261
//! m53 t=50   reachable 0→63  hint 498→448  harvests +725    harvested +65  ENOSPC +743
//! ```
//!
//! Hundreds of blocks of this lane are LOCALLY reachable on one volume while
//! the pick lands on the other, whose lane is exhausted: each such pick costs
//! a wasted harvest RPC on the empty volume and a refused write
//! (`StorageFull`), and never tries the sibling.
//!
//! # The contracts
//!
//! 1. **lane-aware weights**: on a laned co-writer a volume's placement
//!    weight is its LANE-reachable fraction (`lane_reachable_blocks × 1000 ÷
//!    lane share`), so an exhausted lane leaves the band; with supply on
//!    volume B only every allocation lands on B — no refusal, no park, no
//!    RPC, `backend_placement_lane_exhausted_picks` flat;
//! 2. **the two faster-than-cadence events** move the pick BEFORE the
//!    refresh: an allocation draining a volume's last lane block (the pick
//!    skips it), and a harvest refilling an out-of-band volume (the pick
//!    reaches it) — the table is never rebuilt for either;
//! 3. **failover before the park**: a `StorageFull` on the picked volume
//!    tries the remaining eligible volumes in the SAME attempt; the park is
//!    for "no volume has supply", and then it behaves exactly as today
//!    (parks on a held ring, refuses at the wall; refuses at once on
//!    nothing held);
//! 4. **single-writer byte-identity**: an unpartitioned router keeps the
//!    device-fill `health_effective` weights and no failover counter moves;
//! 5. **the lever** `SQUEEZEFS_COWRITER_LANE_PLACEMENT=0` restores the
//!    shipped device-fill pick + no failover;
//! 6. **the harvest reaches the volume that holds the supply**: with both
//!    lanes locally dry, the allocation harvests the volume the authority
//!    holds blocks on; the pushed refill asks a DRY volume on a hint even
//!    when it is owed nothing, and never a stocked one it is owed nothing on;
//!    the ahead refill treats the hint as evidence beside the owed ledger.
//!
//! RED against `dev` (2a486273): `BackendRouter::allocate_placed_block`,
//! `BlockAllocator::lane_placement_governed` / `lane_share_blocks`, the
//! lever and the two gauges do not exist; the table weighs device fill.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::data_alloc_lane::{self as lane, LaneHarvest, LaneHarvestSink};
use squeezefs::error::SqueezefsError;
use squeezefs::free_grace;
use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{health_effective, lane_placement_weight, BackendRouter, StorageBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Device capacity per volume, in allocator chunks.
const CAP: u64 = 64;
/// The co-writer's partition: lane 1 of 2 (the authority holds lane 0), so
/// the lane share is 32 blocks per volume.
const W: u16 = 2;
const LANE: u16 = 1;
/// The bound every "must terminate" assertion runs under — an order of
/// magnitude above the longest legal park on an unarmed plane (1 s wall).
const BOUND: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Serialization + restoration (METRICS, the lever and the free-grace plane
// are process-global)
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
        squeezefs::block_allocator::test_set_cowriter_lane_placement(None);
        squeezefs::block_allocator::test_set_harvest_ahead(None);
        squeezefs::block_allocator::test_clear_harvest_single_flight();
        free_grace::reset_for_test();
    }
}

fn restore() -> Restore {
    free_grace::reset_for_test();
    Restore
}

// ---------------------------------------------------------------------------
// The rig: a two-volume router whose allocators are one co-writer's laned
// allocators, each wired to a fake authority holding THAT volume's lane list
// ---------------------------------------------------------------------------

/// The authority as one volume's harvest sink sees it: the lane-1 block
/// indices sitting on ITS free list for this volume, served up to `max` per
/// call, plus the live bound-age hint (nonzero = a held ring).
struct FakeAuthority {
    supply: Mutex<Vec<u64>>,
    calls: AtomicU64,
    hint_ms: AtomicU64,
}

impl FakeAuthority {
    fn new(hint_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            supply: Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
            hint_ms: AtomicU64::new(hint_ms),
        })
    }

    fn sink(self: &Arc<Self>) -> LaneHarvestSink {
        let me = Arc::clone(self);
        Arc::new(move |max: u64| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.calls.fetch_add(1, Ordering::Relaxed);
                let blocks: Vec<u64> = {
                    let mut s = me.supply.lock().unwrap();
                    let n = (max as usize).min(s.len());
                    s.drain(..n).collect()
                };
                Ok(LaneHarvest {
                    release_ages_ms: vec![0; blocks.len()],
                    blocks,
                    bound_age_hint_ms: me.hint_ms.load(Ordering::Relaxed),
                    rtt_ms: 1,
                })
            })
        })
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn holds(&self) -> usize {
        self.supply.lock().unwrap().len()
    }
}

struct Vol {
    id: &'static str,
    alloc: Arc<BlockAllocator>,
    authority: Arc<FakeAuthority>,
}

impl Vol {
    /// The lane-1 block indices this co-writer has minted so far, in
    /// minting order.
    async fn mint(&self, n: u64) -> Vec<u64> {
        let mut out = Vec::new();
        for i in 0..n {
            let off = self
                .alloc
                .allocate_block()
                .await
                .unwrap_or_else(|e| panic!("{}: mint {i}: {e}", self.id));
            let idx = off / self.alloc.chunk_size();
            assert_eq!(idx % u64::from(W), u64::from(LANE), "own residue class");
            out.push(idx);
        }
        out
    }

    /// Mint the whole lane share: the lane is exhausted afterwards.
    async fn exhaust(&self) -> Vec<u64> {
        let share = lane::lane_capacity_blocks(CAP, W, LANE);
        let minted = self.mint(share).await;
        assert_eq!(
            self.alloc.lane_reachable_blocks(),
            0,
            "{}: exhausted",
            self.id
        );
        minted
    }

    /// The S9 rewrite's local half for `idxs`: the displaced blocks were
    /// shipped and answered `Freed`, so this mount retires its tracking; the
    /// blocks now belong to the authority's list for this lane.
    fn ship_frees(&self, idxs: &[u64]) {
        for idx in idxs {
            self.alloc
                .retire_shipped_free_tracking(idx * self.alloc.chunk_size());
        }
    }

    /// Put `idxs` on the AUTHORITY's list for this volume (shipped there).
    fn park_at_authority(&self, idxs: &[u64]) {
        self.ship_frees(idxs);
        self.authority
            .supply
            .lock()
            .unwrap()
            .extend_from_slice(idxs);
    }

    /// A harvest that already landed: `idxs` are on THIS allocator's free
    /// list (lane-reachable here, no RPC needed).
    fn adopt_locally(&self, idxs: &[u64]) {
        self.ship_frees(idxs);
        assert_eq!(
            self.alloc.adopt_lane_free_grant(idxs),
            idxs.len() as u64,
            "{}: every index adopts",
            self.id
        );
    }
}

struct Rig {
    router: Arc<BackendRouter>,
    a: Vol,
    b: Vol,
    _dir: TempDir,
}

fn dev_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    p
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    a.set_capacity_bytes(CAP * a.chunk_size());
    a
}

/// A two-volume router. `laned` engages lane 1 of 2 on both allocators and
/// wires each to its own fake authority (the co-writer engagement shape);
/// `false` leaves them unpartitioned (the single-writer control).
async fn rig(laned: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let default_path = dev_file(dir.path(), "default.img");
    let default_dev = Arc::new(NvmeBlockDev::new(default_path.to_str().unwrap()));
    let default_alloc = Arc::new(BlockAllocator::new("lane-placement-default").await.unwrap());
    let router = Arc::new(BackendRouter::new(
        default_alloc,
        default_dev,
        Arc::new(AtomicU64::new(squeezefs::block_allocator::CHUNK_SIZE)),
    ));
    let mut vols = Vec::new();
    for (id, vol_id) in [
        ("volA", "vol-00000000000000a1"),
        ("volB", "vol-00000000000000b2"),
    ] {
        let dev_path = dev_file(dir.path(), &format!("{id}.img"));
        let dev = Arc::new(NvmeBlockDev::new(dev_path.to_str().unwrap()));
        let alloc = allocator(vol_id).await;
        let authority = FakeAuthority::new(0);
        if laned {
            alloc
                .engage_alloc_lanes(AppendPartition::new(W, LANE).unwrap())
                .expect("engage lane 1 of 2");
            alloc.set_lane_harvest_sink(authority.sink());
        }
        router
            .publish_backend(
                id,
                Arc::new(StorageBackend {
                    device: dev,
                    block_allocator: Arc::clone(&alloc),
                }),
            )
            .unwrap();
        vols.push(Vol {
            id,
            alloc,
            authority,
        });
    }
    let b = vols.pop().unwrap();
    let a = vols.pop().unwrap();
    Rig {
        router,
        a,
        b,
        _dir: dir,
    }
}

fn is_storage_full(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

type Placed = (String, Arc<BlockAllocator>, Arc<NvmeBlockDev>, u64);

/// `expect_err` for the placed-allocation tuple (whose Arcs carry no
/// `Debug`).
fn refused(r: Result<Placed, SqueezefsError>, why: &str) -> SqueezefsError {
    match r {
        Err(e) => e,
        Ok((be_id, _, _, off)) => panic!("{why}: allocated {off} on {be_id}"),
    }
}

struct Gauges {
    refusals: u64,
    parks: u64,
    failovers: u64,
    exhausted_picks: u64,
    refreshes: u64,
    harvested: u64,
}

fn gauges() -> Gauges {
    Gauges {
        refusals: METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed),
        parks: free_grace::pressure_parks(),
        failovers: METRICS
            .backend_placement_lane_failovers
            .load(Ordering::Relaxed),
        exhausted_picks: METRICS
            .backend_placement_lane_exhausted_picks
            .load(Ordering::Relaxed),
        refreshes: METRICS.placement_table_refreshes.load(Ordering::Relaxed),
        harvested: METRICS.alloc_lane_harvested_blocks.load(Ordering::Relaxed),
    }
}

fn weight_of(router: &BackendRouter, id: &str) -> u32 {
    router
        .placement_snapshot()
        .rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("row {id} missing"))
        .weight
}

fn band(router: &BackendRouter) -> Vec<String> {
    let mut b = router.placement_snapshot().band_ids();
    b.sort();
    b
}

// ===========================================================================
// 0. The weight, pure
// ===========================================================================

/// The lane-governed weight is the lane-reachable fraction of the lane
/// share on `health_effective`'s 0..1000 scale: exhausted = 0, full share
/// (or more, after an adoption) = 1000, an unbounded allocator (reachable
/// `u64::MAX` — space is not a constraint) = 1000 whatever its share.
#[test]
fn the_lane_placement_weight_is_the_reachable_fraction_of_the_share() {
    assert_eq!(lane_placement_weight(0, 32), 0);
    assert_eq!(lane_placement_weight(8, 32), 250);
    assert_eq!(lane_placement_weight(32, 32), 1000);
    assert_eq!(
        lane_placement_weight(40, 32),
        1000,
        "adopted lanes never exceed the scale"
    );
    assert_eq!(
        lane_placement_weight(u64::MAX, 0),
        1000,
        "unbounded: space is no constraint"
    );
    assert_eq!(lane_placement_weight(5, 0), 0, "no share, no weight");
}

// ===========================================================================
// 1. Lane-aware weights: supply on B only ⇒ every allocation lands on B
// ===========================================================================

/// Contract 1: with volume A's lane exhausted and 8 recycled lane blocks
/// locally reachable on volume B, the table weighs A at 0 (out of the band)
/// and B at `8 × 1000 ÷ 32 = 250`, and eight placed allocations all land on
/// B — no `StorageFull`, no park, no harvest RPC on either volume, no
/// rebuild, and the exhausted-pick gauge flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supply_on_one_volume_places_every_allocation_there() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    rig.b.adopt_locally(&minted_b[..8]);
    assert_eq!(rig.b.alloc.lane_reachable_blocks(), 8);

    rig.router.refresh_placement_table();
    assert_eq!(
        weight_of(&rig.router, "volA"),
        0,
        "an exhausted lane weighs nothing on a laned co-writer"
    );
    assert_eq!(
        weight_of(&rig.router, "volB"),
        250,
        "the lane-reachable fraction of the lane share: 8 × 1000 ÷ 32"
    );
    assert_eq!(band(&rig.router), vec!["volB".to_string()]);

    let g0 = gauges();
    for i in 0..8 {
        let (be_id, alloc, _dev, off) = rig
            .router
            .allocate_placed_block()
            .await
            .unwrap_or_else(|e| panic!("placed allocation {i} must succeed: {e}"));
        assert_eq!(
            be_id, "volB",
            "allocation {i} lands where the lane has supply"
        );
        assert!(Arc::ptr_eq(&alloc, &rig.b.alloc));
        assert_eq!((off / alloc.chunk_size()) % u64::from(W), u64::from(LANE));
    }
    let g1 = gauges();
    assert_eq!(g1.refusals, g0.refusals, "no StorageFull anywhere");
    assert_eq!(g1.parks, g0.parks, "no park");
    assert_eq!(
        rig.a.authority.calls(),
        0,
        "no wasted harvest RPC on the dry volume"
    );
    assert_eq!(
        rig.b.authority.calls(),
        0,
        "no RPC where the supply is local"
    );
    assert_eq!(
        g1.exhausted_picks, g0.exhausted_picks,
        "the pick never landed dry"
    );
    assert_eq!(g1.failovers, g0.failovers, "nothing needed failing over");
    assert_eq!(g1.refreshes, g0.refreshes, "picks never rebuild the table");
    assert_eq!(
        rig.b.alloc.lane_reachable_blocks(),
        0,
        "B's supply is spent"
    );
}

// ===========================================================================
// 2. The two faster-than-cadence events move the pick before the refresh
// ===========================================================================

/// Contract 2a: both volumes hold 8 lane blocks at refresh time (equal
/// weights, both in the band). Volume A is then drained OUTSIDE placement
/// (its last lane block goes) with the table untouched. Every placed
/// allocation lands on B: the pick reads the O(1) lane-reachable counter,
/// so a drained volume leaves the pick at once — no RPC, no refusal, no
/// rebuild.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_allocation_draining_the_last_lane_block_moves_the_pick_before_the_refresh() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    let minted_a = rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    rig.a.adopt_locally(&minted_a[..8]);
    rig.b.adopt_locally(&minted_b[..8]);
    rig.router.refresh_placement_table();
    assert_eq!(
        band(&rig.router),
        vec!["volA".to_string(), "volB".to_string()],
        "equal lane supply ⇒ both in the band"
    );

    // The drain, outside placement: A's lane supply goes to 0 while the
    // snapshot still names it.
    rig.a.mint(8).await;
    assert_eq!(rig.a.alloc.lane_reachable_blocks(), 0);
    assert!(
        band(&rig.router).contains(&"volA".to_string()),
        "the table is stale"
    );

    let g0 = gauges();
    for i in 0..8 {
        let (be_id, _, _, _) = rig
            .router
            .allocate_placed_block()
            .await
            .unwrap_or_else(|e| panic!("placed allocation {i}: {e}"));
        assert_eq!(be_id, "volB", "allocation {i} skips the drained volume");
    }
    let g1 = gauges();
    assert_eq!(g1.refusals, g0.refusals);
    assert_eq!(
        g1.refreshes, g0.refreshes,
        "no rebuild — the pick is table-only"
    );
    assert_eq!(
        rig.a.authority.calls(),
        0,
        "the drained volume is never asked"
    );
    assert_eq!(
        g1.failovers, g0.failovers,
        "the pick was right the first time"
    );
    assert_eq!(g1.exhausted_picks, g0.exhausted_picks);
}

/// Contract 2b: volume A's lane is exhausted at refresh time (out of the
/// band; B alone carries the band). B is then drained outside placement and
/// a harvest refills A (adopted into A's lane list) — the table untouched.
/// The next placed allocations land on A: an out-of-band volume that
/// regained supply is reachable by the pick before any rebuild.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harvest_refilling_an_out_of_band_volume_is_reachable_before_the_refresh() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    let minted_a = rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    rig.b.adopt_locally(&minted_b[..4]);
    rig.router.refresh_placement_table();
    assert_eq!(band(&rig.router), vec!["volB".to_string()]);

    rig.b.mint(4).await;
    assert_eq!(rig.b.alloc.lane_reachable_blocks(), 0);
    // The refill lands on A (the harvest's adoption), out of the band.
    rig.a.adopt_locally(&minted_a[..4]);

    let g0 = gauges();
    for i in 0..4 {
        let (be_id, _, _, _) = rig
            .router
            .allocate_placed_block()
            .await
            .unwrap_or_else(|e| panic!("placed allocation {i}: {e}"));
        assert_eq!(be_id, "volA", "allocation {i} reaches the refilled volume");
    }
    let g1 = gauges();
    assert_eq!(g1.refusals, g0.refusals, "no refusal");
    assert_eq!(g1.refreshes, g0.refreshes, "no rebuild");
    assert_eq!(
        rig.a.authority.calls() + rig.b.authority.calls(),
        0,
        "no RPC"
    );
}

// ===========================================================================
// 3. Failover before the park; the park itself is unchanged
// ===========================================================================

/// Contract 3a: both lanes are exhausted, both authorities hold nothing and
/// report nothing held (bound age 0). The placed allocation refuses
/// `StorageFull` AT ONCE — after asking BOTH volumes' authorities exactly
/// once each (the pick's ENOSPC harvest, then the failover's): the park is
/// for "no volume has supply", and here no reclaimable supply exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_lanes_exhausted_and_nothing_held_refuses_at_once_after_asking_both() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    rig.a.exhaust().await;
    rig.b.exhaust().await;
    rig.router.refresh_placement_table();

    let g0 = gauges();
    let t0 = Instant::now();
    let e = refused(
        tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
            .await
            .expect("must terminate"),
        "no volume has supply",
    );
    assert!(is_storage_full(&e), "the verdict is StorageFull: {e}");
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "nothing held anywhere ⇒ no park slice"
    );
    let g1 = gauges();
    assert_eq!(g1.parks, g0.parks, "no park on nothing held");
    assert_eq!(
        rig.a.authority.calls(),
        1,
        "the dry pick asked its authority once"
    );
    assert_eq!(
        rig.b.authority.calls(),
        1,
        "the failover asked the sibling once"
    );
    assert_eq!(
        g1.failovers, g0.failovers,
        "a failed failover is not a failover"
    );
}

/// Contract 3b: both lanes exhausted, both authorities empty but reporting a
/// HELD ring (the field's m50 shape, bound age 15,096 ms). The placed
/// allocation PARKS (finding 29's promise — the retries drive the
/// authority's fence), retries the harvest on both volumes each slice, and
/// refuses `StorageFull` at the wall: exactly today's bounded allocation,
/// over the set instead of one volume. Each volume's retries are that
/// volume's single-flight (finding 15 phase B1): with no grant arriving,
/// each authority is asked ONCE and every later slice declines on the
/// fresh empty reply — two independent declines, one per allocator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_lanes_exhausted_with_a_held_ring_parks_then_refuses_at_the_wall() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    rig.a.authority.hint_ms.store(15_096, Ordering::Relaxed);
    rig.b.authority.hint_ms.store(15_096, Ordering::Relaxed);
    rig.a.exhaust().await;
    rig.b.exhaust().await;
    rig.router.refresh_placement_table();

    let g0 = gauges();
    let declined0 = METRICS
        .alloc_lane_harvest_declined_stale
        .load(Ordering::Relaxed);
    let wall = free_grace::pressure_park_wall_ms();
    let t0 = Instant::now();
    let e = refused(
        tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
            .await
            .expect("THE WEDGE: the bounded allocation must end within the bound"),
        "still no supply",
    );
    let elapsed = t0.elapsed();
    assert!(is_storage_full(&e), "the verdict stays StorageFull: {e}");
    let g1 = gauges();
    assert!(g1.parks > g0.parks, "the park engaged");
    assert_eq!(
        (rig.a.authority.calls(), rig.b.authority.calls()),
        (1, 1),
        "each volume's authority asked once; every later slice declined on its empty reply"
    );
    assert!(
        METRICS
            .alloc_lane_harvest_declined_stale
            .load(Ordering::Relaxed)
            >= declined0 + 2,
        "both allocators declined their retries"
    );
    assert!(
        elapsed.as_millis() as u64 <= wall + 2_000,
        "the park ends at the wall ({wall} ms) — took {elapsed:?}"
    );
    assert_eq!(g1.failovers, g0.failovers);
}

/// Contract 3b's A/B control: `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT=0`
/// re-runs the harvest on BOTH volumes each park slice — the shipped shape
/// verbatim, over the set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_single_flight_lever_off_re_runs_both_harvests_every_slice() {
    let _s = serial();
    let _r = restore();
    squeezefs::block_allocator::test_set_harvest_single_flight(Some(false));
    let rig = rig(true).await;
    rig.a.authority.hint_ms.store(15_096, Ordering::Relaxed);
    rig.b.authority.hint_ms.store(15_096, Ordering::Relaxed);
    rig.a.exhaust().await;
    rig.b.exhaust().await;
    rig.router.refresh_placement_table();

    let g0 = gauges();
    let wall = free_grace::pressure_park_wall_ms();
    let t0 = Instant::now();
    let e = refused(
        tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
            .await
            .expect("the bounded allocation must end within the bound"),
        "still no supply",
    );
    let elapsed = t0.elapsed();
    assert!(is_storage_full(&e), "the verdict stays StorageFull: {e}");
    let g1 = gauges();
    assert!(g1.parks > g0.parks, "the park engaged");
    assert!(
        rig.a.authority.calls() > 1 && rig.b.authority.calls() > 1,
        "lever off: each park slice re-ran the harvest on BOTH volumes ({} / {})",
        rig.a.authority.calls(),
        rig.b.authority.calls()
    );
    assert!(
        elapsed.as_millis() as u64 <= wall + 2_000,
        "the park ends at the wall ({wall} ms) — took {elapsed:?}"
    );
    assert!(squeezefs::block_allocator::test_clear_harvest_single_flight());
}

// ===========================================================================
// 4. Single-writer byte-identity
// ===========================================================================

/// Contract 4: an UNPARTITIONED two-volume router keeps the shipped §5.9
/// weights exactly — `health_effective(free-fraction × 1000, fill, set
/// mean)` from the device census — the placed allocation is the pick +
/// that volume's bounded allocation, and neither failover gauge moves: a
/// `StorageFull` on a full single-writer set propagates, because "full" is
/// full when every volume's whole free list is this mount's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_writer_router_keeps_the_device_fill_weights_and_never_fails_over() {
    let _s = serial();
    let _r = restore();
    let rig = rig(false).await;
    // A 48 of 64 used, B empty: the VL4b imbalance shape.
    for _ in 0..48 {
        rig.a.alloc.allocate_block().await.unwrap();
    }
    rig.router.refresh_placement_table();
    let fill_a = 48.0 / 64.0;
    let mean = 48.0 / 128.0;
    assert_eq!(
        weight_of(&rig.router, "volA"),
        health_effective(((1.0 - fill_a) * 1000.0) as u32, fill_a, mean),
        "device-fill weight, byte-identical"
    );
    assert_eq!(
        weight_of(&rig.router, "volB"),
        health_effective(1000, 0.0, mean),
        "device-fill weight, byte-identical"
    );
    assert_eq!(band(&rig.router), vec!["volB".to_string()]);

    let g0 = gauges();
    let (be_id, _, _, _) = rig.router.allocate_placed_block().await.unwrap();
    assert_eq!(be_id, "volB");

    // Both volumes full ⇒ both weigh 0 ⇒ both in the band (a full-but-
    // healthy set still places, the shipped law) ⇒ the pick's StorageFull
    // propagates with no failover: the sibling is full too, and a single
    // writer never pays a second attempt.
    for _ in 0..16 {
        rig.a.alloc.allocate_block().await.unwrap();
    }
    // B: 63 more — the placed allocation above took one.
    for _ in 0..63 {
        rig.b.alloc.allocate_block().await.unwrap();
    }
    rig.router.refresh_placement_table();
    let e = refused(
        tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
            .await
            .expect("terminates"),
        "a full single-writer set refuses",
    );
    assert!(is_storage_full(&e), "{e}");
    let g1 = gauges();
    assert_eq!(g1.failovers, g0.failovers, "no failover on a single writer");
    assert_eq!(
        g1.exhausted_picks, g0.exhausted_picks,
        "no lane gauge on a single writer"
    );
    assert_eq!(
        g1.refusals, g0.refusals,
        "no lane refusal — the mount has no lane"
    );
}

// ===========================================================================
// 5. The lever off = the shipped pick, no failover
// ===========================================================================

/// Contract 5: `SQUEEZEFS_COWRITER_LANE_PLACEMENT=0` on the contract-2a
/// shape (both in the band at refresh, A drained afterwards) restores the
/// shipped behaviour verbatim: the stale band keeps sending picks to the
/// drained volume, each such pick asks its authority and refuses
/// `StorageFull`, and no failover is attempted — the fleet A/B control.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lever_off_restores_the_shipped_pick_and_no_failover() {
    let _s = serial();
    let _r = restore();
    squeezefs::block_allocator::test_set_cowriter_lane_placement(Some(false));
    let rig = rig(true).await;
    let minted_a = rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    rig.a.adopt_locally(&minted_a[..8]);
    rig.b.adopt_locally(&minted_b[..8]);
    rig.router.refresh_placement_table();
    assert_eq!(
        band(&rig.router),
        vec!["volA".to_string(), "volB".to_string()],
        "the shipped device-fill weights (equal fill) keep both in the band"
    );
    rig.a.mint(8).await;

    let g0 = gauges();
    let mut refused = 0u32;
    let mut on_b = 0u32;
    for _ in 0..4 {
        match tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
            .await
            .expect("terminates")
        {
            Ok((be_id, _, _, _)) => {
                assert_eq!(be_id, "volB");
                on_b += 1;
            }
            Err(e) => {
                assert!(is_storage_full(&e), "{e}");
                refused += 1;
            }
        }
    }
    let g1 = gauges();
    assert!(
        refused >= 1,
        "the stale round-robin lands on the drained volume"
    );
    assert!(on_b >= 1, "and on the stocked one");
    assert!(
        rig.a.authority.calls() >= 1,
        "each dry pick pays its harvest RPC"
    );
    assert!(
        g1.refusals > g0.refusals,
        "counted on alloc_lane_enospc_refusals"
    );
    assert_eq!(g1.failovers, g0.failovers, "the lever off never fails over");
    assert_eq!(
        g1.exhausted_picks, g0.exhausted_picks,
        "the lever off never counts picks"
    );
    assert!(squeezefs::block_allocator::test_clear_cowriter_lane_placement());
}

// ===========================================================================
// 6. The harvest reaches the volume that holds the supply
// ===========================================================================

/// Contract 6a: both lanes locally dry; the authority holds 8 blocks of this
/// lane on volume B's list only. The placed allocation succeeds ON B, the
/// blocks having been harvested from B's authority — whichever volume the
/// (now weightless) round-robin picked first: a dry pick on A costs one
/// wasted RPC and ONE counted failover, a pick on B none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_allocation_harvests_the_volume_whose_authority_holds_the_supply() {
    let _s = serial();
    let _r = restore();
    let rig = rig(true).await;
    rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    rig.b.park_at_authority(&minted_b[..8]);
    rig.router.refresh_placement_table();

    let g0 = gauges();
    let (be_id, _, _, _) = tokio::time::timeout(BOUND, rig.router.allocate_placed_block())
        .await
        .expect("terminates")
        .expect("the supply is reachable through the harvest");
    assert_eq!(
        be_id, "volB",
        "the allocation lands where the authority held supply"
    );
    let g1 = gauges();
    assert_eq!(
        rig.b.authority.calls(),
        1,
        "B's authority was harvested once"
    );
    assert_eq!(g1.harvested, g0.harvested + 8, "the grain adopted into B");
    assert_eq!(rig.b.authority.holds(), 0);
    assert!(
        rig.a.authority.calls() <= 1,
        "at most one wasted RPC on the dry sibling"
    );
    assert_eq!(
        g1.failovers - g0.failovers,
        rig.a.authority.calls(),
        "a dry pick on A is exactly one failover; a pick on B none"
    );
    assert_eq!(g1.parks, g0.parks, "no park — the supply existed");
    assert_eq!(
        rig.b.alloc.lane_reachable_blocks(),
        7,
        "the rest is local now"
    );
}

/// Contract 6b: the PUSHED refill's per-volume decision. The renewal grant's
/// hint is SUMMED over volumes, so it cannot name the one that holds the
/// supply. On a hint: a DRY volume asks even when owed nothing (one RPC that
/// either refills it or proves the supply is its sibling's); a STOCKED
/// volume owed nothing does not (no wasted RTT); an owed volume asks as
/// before. The ahead refill treats the hint as evidence beside the owed
/// ledger: `reachable < watermark` with a nonzero hint fires it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pushed_refill_asks_a_dry_volume_on_a_hint_and_never_a_stocked_unowed_one() {
    let _s = serial();
    let _r = restore();
    free_grace::test_set_lane_push(Some(true));
    squeezefs::block_allocator::test_set_harvest_ahead(Some(true));
    let rig = rig(true).await;
    let minted_a = rig.a.exhaust().await;
    let minted_b = rig.b.exhaust().await;
    // A: dry, owed nothing. B: 4 local, owed nothing. The authority holds
    // 8 of this lane on A's list (a peer's rewrites displaced them — this
    // mount never shipped them, so no owed ledger knows).
    rig.a.park_at_authority(&minted_a[..8]);
    rig.b.adopt_locally(&minted_b[..4]);
    assert_eq!(rig.a.alloc.lane_owed_blocks(), 0);
    assert_eq!(rig.b.alloc.lane_owed_blocks(), 0);

    // The pure decision, per volume (`(hint, owed, reachable, watermark)`).
    assert!(
        free_grace::lane_push_wants_harvest_on_volume(8, 0, 0, 0),
        "hint > 0 ∧ dry ⇒ ask, owed or not"
    );
    assert!(
        !free_grace::lane_push_wants_harvest_on_volume(8, 0, 4, 0),
        "hint > 0 ∧ stocked (quiet lane: no watermark) ∧ owed nothing ⇒ no wasted RTT"
    );
    assert!(
        free_grace::lane_push_wants_harvest_on_volume(8, 0, 4, 50),
        "hint > 0 ∧ below the ahead watermark ⇒ ask (the transit is not covered)"
    );
    assert!(
        !free_grace::lane_push_wants_harvest_on_volume(8, 0, 64, 50),
        "hint > 0 ∧ above the watermark ∧ owed nothing ⇒ no wasted RTT"
    );
    assert!(
        free_grace::lane_push_wants_harvest_on_volume(8, 2, 4, 0),
        "owed ⇒ ask (the shipped decision)"
    );
    assert!(
        !free_grace::lane_push_wants_harvest_on_volume(0, 0, 0, 0),
        "no hint ⇒ nothing to ask for"
    );

    free_grace::note_lane_supply_hint(8);
    let pushed0 = METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed);
    let adopted_b = rig.b.alloc.pushed_refill_tick(1_000).await;
    assert_eq!(adopted_b, 0, "the stocked, unowed volume asked nothing");
    assert_eq!(rig.b.authority.calls(), 0, "no RPC to B's authority");
    let adopted_a = rig.a.alloc.pushed_refill_tick(1_000).await;
    assert_eq!(
        adopted_a, 8,
        "the dry volume asked and the supply was there"
    );
    assert_eq!(rig.a.authority.calls(), 1);
    assert_eq!(
        METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed),
        pushed0 + 1,
        "one pushed harvest counted"
    );
    assert_eq!(rig.a.alloc.lane_reachable_blocks(), 8, "A is stocked now");

    // The ahead decision: a claiming writer below its watermark with the
    // hint in force fires even though the owed ledger reads 0.
    rig.b.alloc.sample_alloc_rate(10_000);
    rig.b.mint(3).await;
    rig.b.alloc.sample_alloc_rate(11_000);
    assert!(
        rig.b.alloc.watermark_blocks() > 0,
        "a claiming writer derives a watermark"
    );
    assert_eq!(rig.b.alloc.lane_reachable_blocks(), 1);
    assert!(
        rig.b.alloc.should_harvest_ahead().is_some(),
        "hint > 0 ∧ reachable < watermark ⇒ the refill fires before the cliff"
    );
    free_grace::note_lane_supply_hint(0);
    assert_eq!(
        rig.b.alloc.should_harvest_ahead(),
        None,
        "no hint, owed nothing ⇒ never asked (the shipped no-wasted-RTT half)"
    );
    rig.b.alloc.note_owed_freed(1);
    assert!(
        rig.b.alloc.should_harvest_ahead().is_some(),
        "owed ⇒ fires as before"
    );
}

/// Contract 7: **the router names each allocator ONCE**. On a real mount the
/// default slot ALIASES the first registered data volume's allocator (the
/// same `Arc` — `BackendRouter::build_backend`'s first-volume bare-key
/// invariant), and every lane path iterates `lane_allocators()`: the
/// co-writer engagement (`engage_co_writer_lanes`), the supply-close arm,
/// the authority's lane-supply source, the release hook. Un-deduplicated,
/// the first volume was ENGAGED TWICE (the fleet's co-writer logs carry two
/// "lane ENGAGED on volume 'nvme32n1'" lines per mount lifetime beside one
/// for 'nvme33n1' — `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md`):
/// two ahead-refill tasks on one allocator, so every renewal wake ran THREE
/// pushed decisions on a two-volume mount (one of them a coalesced or
/// declined duplicate) and the claim-rate EWMA was sampled twice per tick.
/// RED against `d603e7ae`: the list carried the alias twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_router_names_the_aliased_default_allocator_once() {
    let _s = serial();
    let dir = tempfile::tempdir().unwrap();
    let default_path = dev_file(dir.path(), "default.img");
    let default_dev = Arc::new(NvmeBlockDev::new(default_path.to_str().unwrap()));
    let default_alloc = allocator("vol-00000000000000d0").await;
    let router = Arc::new(BackendRouter::new(
        Arc::clone(&default_alloc),
        Arc::clone(&default_dev),
        Arc::new(AtomicU64::new(squeezefs::block_allocator::CHUNK_SIZE)),
    ));
    // The first volume: the mount-time registration REUSES the default
    // slot's Arcs (`build_backend` on the default device).
    router
        .publish_backend(
            "volA",
            Arc::new(StorageBackend {
                device: Arc::clone(&default_dev),
                block_allocator: Arc::clone(&default_alloc),
            }),
        )
        .unwrap();
    let b_path = dev_file(dir.path(), "volB.img");
    let b_alloc = allocator("vol-00000000000000b2").await;
    router
        .publish_backend(
            "volB",
            Arc::new(StorageBackend {
                device: Arc::new(NvmeBlockDev::new(b_path.to_str().unwrap())),
                block_allocator: Arc::clone(&b_alloc),
            }),
        )
        .unwrap();
    let allocs = router.lane_allocators();
    assert_eq!(
        allocs.len(),
        2,
        "two data volumes ⇒ two allocators, the alias named once"
    );
    assert!(
        Arc::ptr_eq(&allocs[0], &default_alloc) && Arc::ptr_eq(&allocs[1], &b_alloc),
        "registration order, the default slot first"
    );
    // A bare router (nothing registered) still names its default allocator.
    let bare = BackendRouter::new(
        allocator("vol-00000000000000d1").await,
        Arc::new(NvmeBlockDev::new(default_path.to_str().unwrap())),
        Arc::new(AtomicU64::new(squeezefs::block_allocator::CHUNK_SIZE)),
    );
    assert_eq!(bare.lane_allocators().len(), 1);
}
