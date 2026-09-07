//! **The data-plane allocation partition** — DLM **S9** blocker #3
//! (`src/data_alloc_lane.rs`, design record
//! `docs/design-mw-data-alloc-partition.md`).
//!
//! S9 named the gap: *"No data-plane allocation partition. Two writers'
//! allocators would collide on fresh offsets — the DATA analogue of §6.2
//! item 3. This is the largest remaining gap for a real two-host write
//! row."* These are the contracts that close it.
//!
//! The file is organized as the eight properties the design rests on:
//!
//! 1. **the partition function** — the owning lane is derivable from the
//!    offset alone, which is what makes a free need no protocol;
//! 2. **disjointness under concurrency** — two lanes minting at once never
//!    produce the same offset, with no arbitration between them;
//! 3. **lane-blind frees** — either writer frees the other's block with no
//!    ownership lookup, and re-allocation (not the free) is what is
//!    partitioned;
//! 4. **durable cursor survival** — a crash-and-remount resumes above every
//!    index its predecessor could have minted, including the unpublished
//!    in-flight tail derived state cannot see;
//! 5. **single-writer byte-identity** — one writer means one lane means
//!    today's allocator, structurally (a solo engagement installs nothing)
//!    and observably (same sequence, same contiguity, zero extra work);
//! 6. **exhaustion and fairness** — the ENOSPC rule, the proven-dead lane
//!    adoption, and the published stranding bound;
//! 7. **composition** — the lifetime-stamp funnel, S7's quarantine, the
//!    reclaim window, and fsck's reconciliation all still hold;
//! 8. **hygiene** — the capability bit this gates on (no new bit is taken)
//!    and the reservation grain's derivation;
//! 9. **the refill gate** — a co-writer's two PROACTIVE lane refills (the
//!    ahead tick, the pushed refill) arm on the authority's ADVERTISED
//!    supply (the renewal grant's lane-supply hint), never on the owed
//!    ledger alone; the ENOSPC-path harvest is unchanged
//!    (`.benchmarks/2026-09-07-lane-refill-hint-gate.md`);
//! 10. **the single-flight harvest** — one in-flight harvest RPC per
//!     allocator: concurrent callers join its outcome, a fresh EMPTY reply
//!     declines re-issue until the authority's advertisement moves (a
//!     grant, a wake, an owed `Freed`), two volumes are two flights, the
//!     lever off is one RPC per caller, and no park outlives the wall
//!     (`.benchmarks/2026-09-07-lane-harvest-single-flight.md`).
//!
//! **No numbers here — ruling D11.** The bench coverage
//! (`benches/write_path_bench.rs::alloc_lane`) is written and NOT run; every
//! cost claim in the design record is a prediction with a falsification
//! criterion.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::data_alloc_lane as lane;
use squeezefs::data_custody::declare_dead_epoch;
use squeezefs::error::SqueezefsError;
use squeezefs::free_grace;
use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::superblock as sb;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Tests that touch PROCESS-GLOBAL state (the `alloc_lane_*` gauges, the
/// installed mount partition, the knob environment) serialize on this —
/// libtest runs a file's tests on threads, and the gate's
/// `--test-threads=1` bounds files, not tests within one. (The
/// `dlm_data_fence_tests` latch, verbatim: an atomic rather than a `Mutex`
/// because the guard is deliberately held across `.await`.)
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

/// Env-knob guard (the `async_block_reclaim_tests` shape): set for one
/// test, restore on drop.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

async fn allocator(id: &str, capacity_blocks: u64) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    if capacity_blocks > 0 {
        a.set_capacity_bytes(capacity_blocks * a.chunk_size());
    }
    a
}

fn part(writers: u16, id: u16) -> AppendPartition {
    AppendPartition::new(writers, id).expect("partition")
}

/// A recording reservation sink — the durable half, stubbed so the contract
/// under test is the ALLOCATOR's ordering (reserve before hand-out), not the
/// metadata plane's. The KV sink's own record bytes are pinned separately
/// (`the_reservation_record_round_trips_and_refuses_tampering`).
#[derive(Default)]
struct Recorder {
    raises: Mutex<Vec<(u16, u64)>>,
    fail: AtomicBool,
}

impl Recorder {
    fn sink(self: &Arc<Self>) -> lane::LaneReserveSink {
        let me = Arc::clone(self);
        Arc::new(move |lane_id: u16, upto: u64| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                if me.fail.load(Ordering::Relaxed) {
                    return Err(SqueezefsError::InvalidOperation("sink refused".into()));
                }
                me.raises.lock().unwrap().push((lane_id, upto));
                Ok(())
            })
        })
    }

    fn raises(&self) -> Vec<(u16, u64)> {
        self.raises.lock().unwrap().clone()
    }

    fn records(&self, writers: u16) -> Vec<lane::LaneReservation> {
        self.raises()
            .into_iter()
            .map(|(lane_id, reserved_upto)| lane::LaneReservation {
                writers,
                lane: lane_id,
                reserved_upto,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 1. The partition function: the owning lane is derivable from the offset
// ---------------------------------------------------------------------------

/// Contract (design requirement 1): the lane is a pure function of the
/// block index — `b % W` — so a free needs **no** ownership lookup, no
/// message and no record. Pinned across every admissible width, on indices
/// and on byte offsets, and against the allocator's own mints.
#[tokio::test]
async fn the_owning_lane_is_derivable_from_the_offset_alone() {
    let _serial = serial();
    for writers in [1u16, 2, 4, 8, 16] {
        for idx in 0..64u64 {
            assert_eq!(
                lane::block_lane_of(idx, writers),
                idx % u64::from(writers),
                "the lane of block {idx} in a {writers}-way partition is a residue class"
            );
            assert_eq!(
                lane::offset_lane_of(idx * 4 * 1024 * 1024, 4 * 1024 * 1024, writers),
                lane::block_lane_of(idx, writers),
                "the byte-offset form must agree with the index form"
            );
        }
        for id in 0..writers {
            let p = part(writers, id);
            assert_eq!(
                lane::first_block_in_lane(p),
                u64::from(id),
                "lane {id} of {writers} starts at its own id (base 0 — block 0 is allocatable)"
            );
        }
    }

    // And against real mints: every offset this allocator hands out is in
    // its own lane, so `block_lane_of` ATTRIBUTES any offset to its minter.
    let a = allocator("lane-attribution", 0).await;
    a.engage_alloc_lanes(part(4, 2)).expect("engage");
    for _ in 0..16 {
        let off = a.allocate_block().await.expect("mint");
        assert_eq!(
            lane::offset_lane_of(off, a.chunk_size(), 4),
            2,
            "offset {off} was minted by lane 2 and must attribute to it"
        );
    }
}

/// Contract: solo (`W = 1`) collapses the whole arithmetic to today's dense
/// counter — the tie test `lane_core`'s own solo pin has one plane up.
#[test]
fn solo_ties_the_shipped_arithmetic() {
    let solo = AppendPartition::SOLO;
    for idx in 0..32u64 {
        assert_eq!(lane::block_lane_of(idx, 1), 0, "lane 0 owns every index");
        assert_eq!(
            lane::next_block_in_lane_at_or_above(idx, solo),
            idx,
            "no rounding at stride 1"
        );
        assert_eq!(
            lane::next_owned_index_at_or_above(idx, 1, 1),
            Some(idx),
            "the mint step is the floor itself — the shipped CAS loop"
        );
    }
    assert_eq!(lane::lane_capacity_blocks(1000, 1, 0), 1000);
    assert_eq!(
        lane::stranded_blocks_bound(1000, 1, 1),
        0,
        "one writer strands nothing, by construction"
    );
}

// ---------------------------------------------------------------------------
// 2. Disjointness under concurrency
// ---------------------------------------------------------------------------

/// Contract (design requirement 1, the headline): two writers' allocators
/// minting at the same time never produce the same offset — **with no
/// arbitration, no range hand-out and no message between them**. Two
/// `BlockAllocator`s in one process with different partitions is exactly two
/// hosts' allocators over one device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_lanes_allocating_concurrently_never_collide() {
    let _serial = serial();
    let a = allocator("lane-collide-a", 0).await;
    let b = allocator("lane-collide-b", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage a");
    b.engage_alloc_lanes(part(2, 1)).expect("engage b");

    let mint = |alloc: Arc<BlockAllocator>| async move {
        let mut out = Vec::new();
        for _ in 0..256 {
            out.push(alloc.allocate_block().await.expect("mint"));
            tokio::task::yield_now().await;
        }
        out
    };
    let (ours, theirs) = tokio::join!(mint(Arc::clone(&a)), mint(Arc::clone(&b)));

    let chunk = a.chunk_size();
    for off in &ours {
        assert_eq!(off / chunk % 2, 0, "writer 0 mints only even indices");
    }
    for off in &theirs {
        assert_eq!(off / chunk % 2, 1, "writer 1 mints only odd indices");
    }
    let mine: std::collections::BTreeSet<u64> = ours.iter().copied().collect();
    assert_eq!(mine.len(), ours.len(), "a lane never repeats an offset");
    for off in &theirs {
        assert!(
            !mine.contains(off),
            "offset {off} was minted by BOTH writers — the partition failed"
        );
    }
}

/// Contract: the same disjointness while **frees interleave in both
/// directions** — each writer freeing blocks the other minted, which is the
/// shape the durable-refcount ledger produces (shared ownership). The
/// tripwire is `block_double_frees`: a free that lands on an offset already
/// on the free list is the double-release lineage that mints ONE device
/// offset to TWO owners.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hammering_both_lanes_with_interleaved_frees_never_double_mints() {
    let _serial = serial();
    let a = allocator("lane-hammer-a", 0).await;
    let b = allocator("lane-hammer-b", 0).await;
    a.engage_alloc_lanes(part(4, 0)).expect("engage a");
    b.engage_alloc_lanes(part(4, 1)).expect("engage b");
    let doubles0 = METRICS.block_double_frees.load(Ordering::Relaxed);
    let untracked0 = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);

    let live: Arc<Mutex<std::collections::BTreeMap<u64, &'static str>>> =
        Arc::new(Mutex::new(std::collections::BTreeMap::new()));

    let worker =
        |alloc: Arc<BlockAllocator>,
         who: &'static str,
         live: Arc<Mutex<std::collections::BTreeMap<u64, &'static str>>>| async move {
            for round in 0..128u64 {
                let off = alloc.allocate_block().await.expect("mint");
                {
                    let mut map = live.lock().unwrap();
                    assert!(
                        map.insert(off, who).is_none(),
                        "offset {off} handed to {who} while another owner held it"
                    );
                }
                tokio::task::yield_now().await;
                if round % 2 == 0 {
                    live.lock().unwrap().remove(&off);
                    alloc.free_block(off).await.expect("free own");
                }
            }
        };

    tokio::join!(
        worker(Arc::clone(&a), "writer0", Arc::clone(&live)),
        worker(Arc::clone(&b), "writer1", Arc::clone(&live)),
    );

    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles0,
        "no double free may occur under a partition"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed),
        untracked0,
        "every free in this run was of a tracked offset"
    );
}

// ---------------------------------------------------------------------------
// 3. Frees are lane-blind; REUSE is what the partition governs
// ---------------------------------------------------------------------------

/// Contract (design requirement 1's second half): a writer frees a block
/// belonging to **any** lane with no ownership lookup — the free path has no
/// lane check at all — and the freed block re-enters the free supply of the
/// lane the arithmetic names, so THIS writer never re-allocates it.
#[tokio::test]
async fn either_writer_frees_the_others_block_with_no_ownership_lookup() {
    let _serial = serial();
    let a = allocator("lane-free-blind", 0).await;
    a.engage_alloc_lanes(part(4, 1)).expect("engage");
    let refusals0 = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);

    // Mint our own lane first so the cursor is past the indices below
    // (`recover_block`'s gap fill is then not involved: this test is about
    // the free path, not about recovery's free-list seeding).
    let chunk = a.chunk_size();
    for _ in 0..4 {
        a.allocate_block().await.expect("own lane");
    }

    // A reference to a block minted by EVERY other lane (what recovery
    // seeds from the set-wide durable ledger — clone-shared ownership).
    for foreign_idx in [0u64, 2, 3] {
        a.recover_block(foreign_idx).await.expect("seed reference");
    }
    for foreign_idx in [0u64, 2, 3] {
        a.free_block(foreign_idx * chunk)
            .await
            .expect("a free never consults a lane");
    }
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed),
        refusals0,
        "freeing a peer's block is a normal free, not a refusal"
    );
    assert_eq!(
        a.foreign_lane_free_blocks(),
        3,
        "the freed lane-0/2/3 blocks are free supply this writer cannot reach"
    );

    // ... and reuse obeys the same residue class: the next allocation is a
    // lane-1 index, never one of the foreign blocks just freed.
    for _ in 0..4 {
        let off = a.allocate_block().await.expect("mint");
        assert_eq!(
            lane::offset_lane_of(off, chunk, 4),
            1,
            "reuse is lane-filtered exactly like a fresh mint"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Durable cursor survival across a crash
// ---------------------------------------------------------------------------

/// Contract (design requirement 2): a lane's durable reservation is written
/// **ahead of use**, so a successor of that lane resumes above every index
/// its predecessor could have minted — including the allocated-but-never-
/// published in-flight tail, which the derived floor (the complement of the
/// durably-referenced set) cannot see.
///
/// The second half of the test is the counterfactual that makes the first
/// half meaningful: with NO reservation records the recovered floor is the
/// derived one, and the successor re-mints its predecessor's offsets — which
/// is safe for a lone writer and is precisely the corruption a live peer
/// cannot survive.
#[tokio::test]
async fn a_crash_and_remount_never_re_mints_a_reserved_lane_block() {
    let _serial = serial();
    let p = part(2, 0);
    let rec = Arc::new(Recorder::default());

    // --- the pre-crash mount: mint, never publish, then die -------------
    let a = allocator("lane-crash", 0).await;
    a.engage_alloc_lanes(p).expect("engage");
    a.set_lane_reserve_sink(rec.sink());
    let mut minted = Vec::new();
    for _ in 0..8 {
        minted.push(a.allocate_block().await.expect("mint"));
    }
    let chunk = a.chunk_size();
    let highest_minted = minted.iter().copied().max().unwrap() / chunk;
    assert!(
        !rec.raises().is_empty(),
        "a fresh-mint run must have raised the durable frontier at least once"
    );
    assert!(
        rec.raises().iter().all(|(l, _)| *l == 0),
        "a writer only ever reserves its OWN lane"
    );
    assert!(
        a.lane_reserved_upto().expect("engaged") > highest_minted,
        "the frontier must dominate every index handed out"
    );
    drop(a); // crash: no publish, no unmount, no checkpoint.

    // --- the successor of lane 0 ----------------------------------------
    // Derived floor 0: nothing was published, so the durable-reference seed
    // finds no blocks at all.
    let records = rec.records(p.writers());
    let floor = lane::recover_lane_floor(&records, 0, p);
    assert!(
        floor > highest_minted,
        "the recovered floor {floor} must dominate the predecessor's last mint {highest_minted}"
    );
    let b = allocator("lane-crash", 0).await;
    b.engage_alloc_lanes(p).expect("engage");
    b.install_lane_floor(floor);
    let first = b.allocate_block().await.expect("mint") / chunk;
    assert!(
        first >= floor,
        "the successor's first mint {first} must be at or above the recovered floor {floor}"
    );
    for off in &minted {
        assert_ne!(
            *off / chunk,
            first,
            "the successor re-minted an offset its predecessor may still be writing"
        );
    }

    // --- the counterfactual: derived state alone loses the tail ---------
    assert_eq!(
        lane::recover_lane_floor(&[], 0, p),
        0,
        "without the durable reservation the floor is the derived one, and the successor \
         re-mints the predecessor's unpublished offsets — the crash window this record closes"
    );
}

/// Contract: the reservation record is versioned and checksummed, and every
/// malformed shape refuses **loud** rather than decoding to a lower floor —
/// a silently-dropped watermark is exactly the lost floor it exists to be.
#[test]
fn the_reservation_record_round_trips_and_refuses_tampering() {
    let rec = lane::LaneReservation {
        writers: 4,
        lane: 3,
        reserved_upto: 1 << 40,
    };
    let raw = rec.encode();
    assert_eq!(raw.len(), lane::LANE_RESERVATION_LEN);
    assert_eq!(
        lane::LaneReservation::decode(&raw).expect("round trip"),
        rec
    );

    let mut short = raw.clone();
    short.pop();
    assert!(lane::LaneReservation::decode(&short).is_err(), "length");

    let mut version = raw.clone();
    version[0] = lane::LANE_RESERVATION_VERSION + 1;
    assert!(
        lane::LaneReservation::decode(&version).is_err(),
        "an unknown version must refuse (forward-only), never be guessed"
    );

    let mut flipped = raw.clone();
    flipped[8] ^= 0x40;
    assert!(
        lane::LaneReservation::decode(&flipped).is_err(),
        "a bit-flipped watermark must refuse, not report a lower frontier"
    );

    let bad_lane = lane::LaneReservation {
        writers: 2,
        lane: 5,
        reserved_upto: 7,
    }
    .encode();
    assert!(
        lane::LaneReservation::decode(&bad_lane).is_err(),
        "a lane outside its own width is not a lane"
    );

    // The record NAME carries the durable `vol-` identity (KD-5), not a
    // path or a set position.
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag("vol-00aa11bb00aa11bb");
    let name = lane::lane_record_name(tag, 3);
    assert!(name.starts_with(lane::LANE_RECORD_PREFIX));
    assert_eq!(lane::parse_lane_record_name(&name), Some((tag, 3)));
    assert_eq!(lane::parse_lane_record_name("writer_claim"), None);
    // ... and it is invisible through FUSE: the VAL-2 xattr screen is an
    // ALLOWLIST, so an internal name needs no denylist edit to be safe.
    assert!(
        !squeezefs::meta_backend::kv::backend::xattr_name_allowed(&name),
        "the reservation record must be unreachable from a shell"
    );
}

/// Contract (the recovery rule's three clauses): the own lane's record is a
/// floor; a FOREIGN lane's record at the same width is not (its indices are
/// not ours to mint); and a record at a DIFFERENT width floors every lane,
/// because a width change makes lane identity itself meaningless.
#[test]
fn the_recovery_floor_uses_own_lane_and_every_foreign_width_record() {
    let p = part(4, 1);
    let own = lane::LaneReservation {
        writers: 4,
        lane: 1,
        reserved_upto: 100,
    };
    let foreign_same_width = lane::LaneReservation {
        writers: 4,
        lane: 2,
        reserved_upto: 400,
    };
    let foreign_width = lane::LaneReservation {
        writers: 2,
        lane: 0,
        reserved_upto: 900,
    };

    let floor = lane::recover_lane_floor(&[own], 0, p);
    assert!(
        floor >= 100 && floor % 4 == 1,
        "own-lane record floors, in-lane"
    );

    assert_eq!(
        lane::recover_lane_floor(&[foreign_same_width], 8, p),
        lane::next_block_in_lane_at_or_above(8, p),
        "a peer's watermark at our width says nothing about our residue class"
    );

    let floor = lane::recover_lane_floor(&[foreign_width], 0, p);
    assert!(
        floor >= 900 && floor % 4 == 1,
        "a record from a different width must dominate EVERY lane"
    );

    // The derived floor still participates (it is what covers published
    // blocks), and rounding stays idempotent.
    let floor = lane::recover_lane_floor(&[own], 4096, p);
    assert!(floor >= 4096 && floor % 4 == 1);
    assert_eq!(
        lane::next_block_in_lane_at_or_above(floor, p),
        floor,
        "rounding a floor already in the lane must not advance it"
    );
}

/// Contract: an offset is handed out **only** once its lane's frontier is
/// durable. A sink that cannot commit gives the offset BACK rather than
/// letting a successor of this lane mint it again.
#[tokio::test]
async fn an_unreservable_offset_is_returned_rather_than_handed_out() {
    let _serial = serial();
    let a = allocator("lane-reserve-fail", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let rec = Arc::new(Recorder::default());
    rec.fail.store(true, Ordering::Relaxed);
    a.set_lane_reserve_sink(rec.sink());

    let refused = a.allocate_block().await;
    assert!(
        refused.is_err(),
        "an offset whose reservation cannot be made durable must not be handed out"
    );
    assert_eq!(
        a.free_blocks_count(),
        1,
        "the claimed offset was returned to the free list (begin+finish, nothing between)"
    );

    // With the sink healthy again the same offset is handed out normally.
    rec.fail.store(false, Ordering::Relaxed);
    let off = a.allocate_block().await.expect("mint");
    assert_eq!(off, 0, "the returned offset is reused, not leaked");
}

// ---------------------------------------------------------------------------
// 5. Single-writer byte-identity (design requirement 3)
// ---------------------------------------------------------------------------

/// Contract (design requirement 3): **one writer means one lane means
/// today's allocator.** Proved twice — structurally (a solo engagement
/// installs NO partition, so no code path can differ) and observably (the
/// same allocation sequence, the same contiguity picks, zero durable work,
/// and not one `alloc_lane_*` gauge moved).
#[tokio::test]
async fn single_writer_is_byte_identical_and_costs_nothing() {
    let _serial = serial();
    let before = (
        METRICS.alloc_lane_writers.load(Ordering::Relaxed),
        METRICS.alloc_lane_reservations.load(Ordering::Relaxed),
        METRICS.alloc_lane_stranded_bytes.load(Ordering::Relaxed),
    );

    let engaged = allocator("solo-engaged", 64).await;
    engaged
        .engage_alloc_lanes(AppendPartition::SOLO)
        .expect("a solo engagement is a no-op, never a refusal");
    assert!(
        engaged.lane_partition().is_none(),
        "a solo partition installs NOTHING — that is the byte-identity proof"
    );
    assert_eq!(engaged.owned_lane_mask(), None);
    assert_eq!(engaged.lane_reserved_upto(), None);
    assert_eq!(engaged.foreign_lane_free_blocks(), 0);

    // A sink cannot even be wired without a partition, so a solo mount can
    // never pay a reservation commit.
    let rec = Arc::new(Recorder::default());
    engaged.set_lane_reserve_sink(rec.sink());

    let plain = allocator("solo-plain", 64).await;
    let mut a_seq = Vec::new();
    let mut b_seq = Vec::new();
    for _ in 0..16 {
        a_seq.push(engaged.allocate_block().await.expect("mint"));
        b_seq.push(plain.allocate_block().await.expect("mint"));
    }
    assert_eq!(
        a_seq, b_seq,
        "an engaged solo allocator's sequence must equal a never-engaged one's"
    );
    let chunk = engaged.chunk_size();
    assert_eq!(
        a_seq,
        (0..16).map(|i| i * chunk).collect::<Vec<_>>(),
        "and it must be the shipped dense sequence, stride 1"
    );

    // Contiguity picks (VL4 `move_one`, VL7 D1/D2) are unchanged.
    engaged.free_block(4 * chunk).await.expect("free");
    plain.free_block(4 * chunk).await.expect("free");
    assert_eq!(
        engaged.allocate_block_below(8),
        plain.allocate_block_below(8),
        "the contiguity pick must be identical"
    );
    engaged.free_block(9 * chunk).await.expect("free");
    plain.free_block(9 * chunk).await.expect("free");
    assert_eq!(
        engaged.allocate_block_at_or_above(6).ok(),
        plain.allocate_block_at_or_above(6).ok(),
        "the ascending pick must be identical"
    );

    assert!(
        rec.raises().is_empty(),
        "zero durable work: a single writer pays no reservation commit, ever"
    );
    assert_eq!(
        (
            METRICS.alloc_lane_writers.load(Ordering::Relaxed),
            METRICS.alloc_lane_reservations.load(Ordering::Relaxed),
            METRICS.alloc_lane_stranded_bytes.load(Ordering::Relaxed),
        ),
        before,
        "no alloc_lane_* gauge may move on a single-writer mount"
    );
}

// ---------------------------------------------------------------------------
// 6. Exhaustion, fairness, and the stranding bound
// ---------------------------------------------------------------------------

/// Contract (design requirement 4): a lane that exhausts its own share
/// refuses `StorageFull` **loudly, naming how many free blocks belong to
/// lanes it does not own** — a writer starving while the set has space is a
/// first-class operator signal (`alloc_lane_enospc_refusals`), never a
/// silent ENOSPC.
#[tokio::test]
async fn lane_exhaustion_refuses_storage_full_naming_the_unreachable_free_space() {
    let _serial = serial();
    let a = allocator("lane-enospc", 8).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let refusals0 = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);
    let chunk = a.chunk_size();

    // Lane 0 of a 2-way partition owns 4 of this device's 8 blocks.
    assert_eq!(lane::lane_capacity_blocks(8, 2, 0), 4);
    for i in 0..4 {
        assert_eq!(
            a.allocate_block().await.expect("own share"),
            i * 2 * chunk,
            "lane 0 mints 0, 2, 4, 6"
        );
    }

    // Make a peer's block free supply, then prove it is unreachable AND
    // that the refusal says so.
    a.recover_block(3).await.expect("seed a lane-1 reference");
    a.free_block(3 * chunk)
        .await
        .expect("free the peer's block");
    assert_eq!(a.foreign_lane_free_blocks(), 1);

    let err = a
        .allocate_block()
        .await
        .expect_err("an exhausted lane must refuse");
    let msg = err.to_string();
    assert!(
        matches!(&err, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull),
        "the refusal keeps the StorageFull class the ENOSPC ladder keys on: {msg}"
    );
    assert!(
        msg.contains("lane 0 of 2") && msg.contains("1 free block"),
        "the refusal must name the lane and the unreachable free supply: {msg}"
    );
    assert!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) > refusals0,
        "and it must be counted (must-stay-0 tripwire)"
    );
}

/// Contract (design requirement 4's answer): the way to reach a dead lane's
/// space is to **adopt** it under the same drain proof S7's quarantine
/// demands — a `DeadEpoch`. Adoption needs no durable record (the
/// reservation is keyed on the LANE, so minting there raises that lane's own
/// watermark) and it lowers the published stranding bound exactly.
#[tokio::test]
async fn adopting_a_proven_dead_lane_reclaims_its_space_and_shrinks_the_bound() {
    let _serial = serial();
    let a = allocator("lane-adopt", 8).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let chunk = a.chunk_size();
    let stranded0 = METRICS.alloc_lane_stranded_bytes.load(Ordering::Relaxed);
    let adoptions0 = METRICS.alloc_lane_adoptions.load(Ordering::Relaxed);

    // Exhaust this lane's own share (0, 2, 4, 6 of an 8-block device), then
    // make a peer's block free supply.
    for _ in 0..4 {
        a.allocate_block().await.expect("own share");
    }
    a.recover_block(3).await.expect("seed");
    a.free_block(3 * chunk).await.expect("free");
    assert_eq!(a.foreign_lane_free_blocks(), 1);

    let proof = declare_dead_epoch("test: lane 1's holder is proven dead");
    assert!(!a.adopt_lane(0, proof), "the own lane is not adoptable");
    assert!(
        !a.adopt_lane(7, proof),
        "a lane outside the width is not one"
    );
    assert!(a.adopt_lane(1, proof), "lane 1 is adopted under the proof");
    assert!(!a.adopt_lane(1, proof), "adoption is idempotent");

    assert_eq!(
        a.owned_lane_mask(),
        Some(0b11),
        "both lanes are now mintable here"
    );
    assert_eq!(
        a.foreign_lane_free_blocks(),
        0,
        "the dead lane's free supply is reachable"
    );
    assert_eq!(
        a.allocate_block().await.expect("adopted reuse"),
        3 * chunk,
        "and the first allocation takes it"
    );
    assert!(
        METRICS.alloc_lane_adoptions.load(Ordering::Relaxed) > adoptions0,
        "adoption is counted"
    );
    assert!(
        METRICS.alloc_lane_stranded_bytes.load(Ordering::Relaxed) < stranded0 + 4 * chunk,
        "and the published stranding bound shrank by the adopted lane's share"
    );

    // An unpartitioned allocator has no lanes to adopt, and says so.
    let plain = allocator("lane-adopt-plain", 8).await;
    assert!(!plain.adopt_lane(1, proof));
}

/// Contract: a reservation raise declares the dense frontier for **every**
/// owned lane — so a future holder of an ADOPTED lane also recovers above
/// the indices we minted in it, and adoption needs no new durable structure.
/// On the shipped shape (one owned lane) this is one commit, unchanged.
#[tokio::test]
async fn a_raise_declares_the_frontier_for_every_owned_lane() {
    let _serial = serial();
    let a = allocator("lane-adopt-reserve", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let rec = Arc::new(Recorder::default());
    a.set_lane_reserve_sink(rec.sink());

    a.allocate_block().await.expect("mint");
    assert_eq!(
        rec.raises().iter().map(|(l, _)| *l).collect::<Vec<_>>(),
        vec![0],
        "one owned lane, one commit"
    );

    let proof = declare_dead_epoch("test: adopt then reserve");
    assert!(a.adopt_lane(1, proof));
    // Force the next raise (the grain is large, so drain the frontier by
    // asking for a far-away index through the ascending pick's refusal path
    // instead: simplest is to allocate until the frontier is passed).
    let frontier = a.lane_reserved_upto().expect("engaged");
    while a.highest_block_index() <= frontier {
        a.allocate_block().await.expect("mint");
    }
    let lanes: std::collections::BTreeSet<u16> =
        rec.raises().iter().skip(1).map(|(l, _)| *l).collect();
    assert_eq!(
        lanes,
        [0u16, 1].into_iter().collect(),
        "after adoption a raise declares BOTH owned lanes: {:?}",
        rec.raises()
    );
    let ups: Vec<u64> = rec.raises().iter().rev().take(2).map(|(_, u)| *u).collect();
    assert_eq!(ups[0], ups[1], "and it is the same dense frontier for both");
}

/// Contract (design requirement 5): the stranded capacity is a published
/// formula, not a vibe — `cap − Σ owned lane shares`, with the two readings
/// `docs/operations.md` states (the reachability bound and the `W − 1`
/// granularity bound), and lane shares that sum to the whole device.
#[test]
fn the_stranded_capacity_bound_is_the_published_formula() {
    for cap in [0u64, 1, 7, 8, 1_000, 25_000_000] {
        for writers in [1u16, 2, 4, 8, 16] {
            let w = u64::from(writers);
            // Lane shares are exact, differ by at most one, and tile the
            // device: partitioning costs at most W − 1 blocks of usable
            // capacity when every writer stays inside its share.
            let shares: Vec<u64> = (0..writers)
                .map(|l| lane::lane_capacity_blocks(cap, writers, l))
                .collect();
            assert_eq!(
                shares.iter().sum::<u64>(),
                cap,
                "lane shares tile the device"
            );
            let (lo, hi) = (
                shares.iter().copied().min().unwrap(),
                shares.iter().copied().max().unwrap(),
            );
            assert!(hi - lo <= 1, "lane shares differ by at most one block");

            // The reachability bound for a single-lane writer.
            let one = lane::stranded_blocks_bound(cap, writers, 1);
            assert_eq!(one, cap - shares[0], "the bound is cap − own share");
            assert!(
                one <= cap.saturating_mul(w - 1) / w + (w - 1),
                "and it is bounded by cap × (W−1)/W + (W−1)"
            );

            // Owning every lane strands nothing; owning none strands all.
            let all = (0..w).fold(0u64, |m, l| m | (1 << l));
            assert_eq!(lane::stranded_blocks_bound(cap, writers, all), 0);
            assert_eq!(lane::stranded_blocks_bound(cap, writers, 0), cap);
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Composition with every landed invariant (design requirement 6)
// ---------------------------------------------------------------------------

/// Contract: **every** allocated offset still comes out of the ONE funnel
/// that mints its §6.2 item-6 lifetime stamp. A partition that added an
/// admission path around `claim_block_idx` would leave offsets reading
/// `unknown` forever and silently re-open §6.3.
#[tokio::test]
async fn every_laned_allocation_carries_a_lifetime_stamp() {
    let _serial = serial();
    let p = part(4, 2);
    let a = allocator("lane-stamp", 0).await;
    a.engage_alloc_lanes(p).expect("engage");
    a.engage_incarnations(7, p);
    assert!(a.incarnations_engaged());

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..8 {
        let off = a.allocate_block().await.expect("mint");
        let stamp = a.live_incarnation(off);
        assert_ne!(
            stamp,
            squeezefs::routing::INCARNATION_NONE,
            "offset {off} started a lifetime with no stamp — it would read `unknown` forever"
        );
        assert!(seen.insert(stamp), "a lifetime stamp is never reused");
    }
    // The contiguity picks route through the same funnel.
    let chunk = a.chunk_size();
    a.free_block(2 * chunk).await.expect("free");
    let off = a.allocate_block_below(16).expect("contiguity pick");
    assert_ne!(
        a.live_incarnation(off),
        squeezefs::routing::INCARNATION_NONE
    );
}

/// Contract: S7's dead-epoch quarantine still gates every allocation path
/// under a partition — a quarantined offset in THIS writer's lane is
/// unreachable until the drain proof, and the free-list publish it owes is
/// paid by `release_quarantine` alone.
#[tokio::test]
async fn the_quarantine_still_gates_a_laned_allocation() {
    let _serial = serial();
    let a = allocator("lane-quarantine", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let chunk = a.chunk_size();

    let off = a.allocate_block().await.expect("mint");
    let dead = declare_dead_epoch("test: laned quarantine");
    assert!(a.quarantine_offset(off, dead));
    a.free_block(off).await.expect("terminal free");
    assert!(
        a.is_quarantined(off),
        "the free completed but the publish is OWED to the drain proof"
    );
    let next = a.allocate_block().await.expect("mint");
    assert_ne!(
        next, off,
        "a quarantined offset must be unreachable from every allocation path"
    );
    assert_eq!(next, 2 * chunk, "and the next lane index is minted instead");

    assert_eq!(a.release_quarantine(dead), 1, "the drain proof releases it");
    let again = a.allocate_block().await.expect("mint");
    assert_eq!(again, off, "released, it is the lane's lowest free block");
}

/// Contract: the `begin_free → reclaim → finish_free` window is unchanged.
/// An offset whose terminal free began is not reallocatable until
/// `finish_free` — which is what makes the queued discard safe — and a
/// partition neither shortens nor lengthens that window.
#[tokio::test]
async fn the_reclaim_window_invariant_survives_the_partition() {
    let _serial = serial();
    let a = allocator("lane-reclaim-window", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let chunk = a.chunk_size();

    let off = a.allocate_block().await.expect("mint");
    assert!(a.begin_free(off), "terminal release");
    let next = a.allocate_block().await.expect("mint");
    assert_ne!(
        next, off,
        "no finish_free, no reuse — the reclaim queue owns the offset"
    );
    assert_eq!(next, 2 * chunk);
    a.finish_free(off);
    assert_eq!(
        a.allocate_block().await.expect("mint"),
        off,
        "finish_free is what publishes it, exactly as before"
    );
}

/// Contract: fsck's C6 reconciliation must **never** complete a foreign
/// lane's free. Under a partition the dense cursor spans peers' indices, and
/// "untracked and not free-listed" is the NORMAL state of a peer's live
/// block here — publishing it would hand another writer's block to this one.
#[tokio::test]
async fn the_fsck_reconcile_never_completes_a_foreign_lanes_free() {
    let _serial = serial();
    let a = allocator("lane-fsck", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    for _ in 0..4 {
        a.allocate_block().await.expect("mint");
    }
    // The cursor now spans lane-1 indices this writer skipped.
    assert!(a.highest_block_index() >= 7);
    let (completed, evictions) = a.fsck_reconcile_accounting();
    assert_eq!(
        (completed, evictions),
        (0, 0),
        "a foreign lane's index is not this writer's to reconcile"
    );
    assert_eq!(
        a.foreign_lane_free_blocks(),
        0,
        "and none of them landed on the free list"
    );
}

/// Contract: the SYNCHRONOUS contiguity pick cannot `await` a reservation
/// raise, so a fresh mint past the frontier is refused loud (the mover
/// defers) rather than handed out uncovered. Free-list picks are unaffected —
/// a freed index needs no new watermark.
#[tokio::test]
async fn the_ascending_pick_refuses_to_mint_past_the_reservation_frontier() {
    let _serial = serial();
    let a = allocator("lane-sync-pick", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let chunk = a.chunk_size();

    let err = a
        .allocate_block_at_or_above(0)
        .expect_err("a fresh sync mint past the frontier must refuse");
    assert!(
        err.to_string().contains("reservation frontier"),
        "the refusal must name why: {err}"
    );

    // With a frontier in place (the async path raised it) the same pick
    // works, and a free-listed index never needed one.
    let rec = Arc::new(Recorder::default());
    a.set_lane_reserve_sink(rec.sink());
    let off = a.allocate_block().await.expect("mint raises the frontier");
    a.free_block(off).await.expect("free");
    assert_eq!(
        a.allocate_block_at_or_above(0).expect("free-list pick"),
        off,
        "reuse needs no reservation"
    );
    assert!(
        a.allocate_block_at_or_above(1).is_ok(),
        "fresh, but covered"
    );
    let _ = chunk;
}

/// Contract: the reservation is **amortized** — one durable commit per
/// grain of fresh blocks, and none at all for free-list reuse (the rewrite
/// hot path). This is the cost claim the design record makes structural
/// rather than measured (ruling D11).
#[tokio::test]
async fn reservations_are_amortized_and_reuse_never_reserves() {
    let _serial = serial();
    let _grain = EnvGuard::set("SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS", "16");
    let a = allocator("lane-amortize", 0).await;
    a.engage_alloc_lanes(part(2, 0)).expect("engage");
    let rec = Arc::new(Recorder::default());
    a.set_lane_reserve_sink(rec.sink());

    let mut offs = Vec::new();
    for _ in 0..8 {
        offs.push(a.allocate_block().await.expect("mint"));
    }
    assert_eq!(
        rec.raises().len(),
        1,
        "a grain of 16 covers 8 fresh lane-0 mints in one commit: {:?}",
        rec.raises()
    );

    for off in &offs {
        a.free_block(*off).await.expect("free");
    }
    for _ in 0..8 {
        a.allocate_block().await.expect("reuse");
    }
    assert_eq!(
        rec.raises().len(),
        1,
        "reuse pays NOTHING: a freed index is dominated by the derived floor"
    );
}

// ---------------------------------------------------------------------------
// 8. Hygiene: the capability bit, and the grain's derivation
// ---------------------------------------------------------------------------

/// Contract (ruling D9 + the five parallel bit claims this program has
/// already survived): the allocation partition takes **no new incompat
/// bit**. It gates on bit 11 (`KV_MULTI_WRITER_DATA`), whose documented
/// meaning already is *"the format's recovery paths are expressed for more
/// than one data-plane writer"* — which the `alloc_lane:` reservation
/// records are — and which S9's arm already requires. Nothing stamps it.
#[test]
fn the_partition_gates_on_bit_11_and_takes_no_new_bit() {
    assert_eq!(
        sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        1 << 11,
        "the gate is bit 11"
    );
    assert_ne!(
        sb::FEATURES_INCOMPAT_KNOWN & sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "this binary must understand the bit it gates on (old binaries refuse it loud, which is \
         exactly right: they would mint dense offsets across every peer's lane and ignore the \
         reservation frontier a live peer published)"
    );
    assert_ne!(
        squeezefs::multi_writer::REQUIRED_INCOMPAT & sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "S9's arm already requires it, so the partition adds no capability demand"
    );
    // Ruling D9: production `format` never stamps it.
    let plan = sb::SuperblockV3::plan(1 << 30, 262_144, None, [7u8; 16], 0x1234).unwrap();
    assert_eq!(
        plan.features_incompat & sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "a fresh format must mount UNPARTITIONED"
    );
    // And this mount is unpartitioned until an admission says otherwise.
    assert!(
        lane::mount_partition().is_solo(),
        "no shipped path installs a non-solo data-plane partition"
    );
}

/// Contract (the derivation law + its drift-is-red tie test): the
/// reservation grain DERIVES from the write pipeline's cold window, is
/// floored at the eight-lane cold aggregate, is capped at 1/64 of a lane
/// share, and an explicit knob wins verbatim.
#[test]
fn the_reservation_grain_derives_and_the_knob_wins_verbatim() {
    let _serial = serial();
    let floor = squeezefs::write_pipeline::FLOOR_BLOCKS_PER_LANE * 8;
    let cpus = squeezefs::cpu::process_parallelism() as u64;
    let derived = squeezefs::write_pipeline::FLOOR_BLOCKS_PER_LANE
        * squeezefs::write_pipeline::HEADROOM
        * cpus;

    // A tiny volume clamps to the floor (the ceiling can never fall below
    // it: a grain of zero would mean a commit per block).
    assert_eq!(lane::reserve_grain_blocks(64, 2), floor);

    // A large volume takes the derived window.
    let big = 1_000_000u64;
    let want = derived.clamp(
        floor,
        (lane::lane_capacity_blocks(big, 2, 0) / 64).max(floor),
    );
    assert_eq!(
        lane::reserve_grain_blocks(big, 2),
        want,
        "the grain is the derived window, clamped — never a free-floating constant"
    );

    let _g = EnvGuard::set("SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS", "12345");
    assert_eq!(
        lane::reserve_grain_blocks(big, 2),
        12_345,
        "an explicit knob wins verbatim (explicit > derived)"
    );
}

/// Contract: the installed mount partition is monotone and refuses a SWAP —
/// moving a live mount's lane would move offsets it has already minted into a
/// peer's residue class.
#[test]
fn the_mount_partition_installs_once_and_refuses_a_swap() {
    let _serial = serial();
    lane::test_reset_mount_partition();
    assert!(lane::mount_partition().is_solo());
    lane::install_mount_partition(part(4, 1)).expect("install");
    assert_eq!(lane::mount_partition().writer_id(), 1);
    assert_eq!(lane::mount_partition().writers(), 4);
    lane::install_mount_partition(part(4, 1))
        .expect("idempotent: the same partition installs again");
    let err = lane::install_mount_partition(part(4, 2)).expect_err("a swap must refuse");
    assert!(err.to_string().contains("already writer 1 of 4"), "{err}");
    lane::test_reset_mount_partition();
}

// ---------------------------------------------------------------------------
// 9. The refill gate: the proactive arms fire on the ADVERTISED supply
// ---------------------------------------------------------------------------

/// Restores the process-global refill posture this section moves (the
/// free-grace words — the hint, the lane-push and refill-hint levers — and
/// the ahead lever), so a panicking assertion never leaves the binary armed.
struct RefillRestore;

impl Drop for RefillRestore {
    fn drop(&mut self) {
        free_grace::reset_for_test();
        squeezefs::block_allocator::test_clear_harvest_ahead();
    }
}

fn refill_restore() -> RefillRestore {
    free_grace::reset_for_test();
    squeezefs::block_allocator::test_clear_harvest_ahead();
    RefillRestore
}

/// The authority's lane list as a co-writer's harvest sink sees it: a
/// queue of lane block indices served up to the ask, every ask recorded.
#[derive(Default)]
struct Supply {
    blocks: Mutex<VecDeque<u64>>,
    asks: Mutex<Vec<u64>>,
}

impl Supply {
    fn sink(self: &Arc<Self>) -> lane::LaneHarvestSink {
        let me = Arc::clone(self);
        Arc::new(move |max: u64| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.asks.lock().unwrap().push(max);
                let mut q = me.blocks.lock().unwrap();
                let n = (max as usize).min(q.len());
                let blocks: Vec<u64> = q.drain(..n).collect();
                Ok(lane::LaneHarvest {
                    blocks,
                    bound_age_hint_ms: 0,
                    rtt_ms: 1,
                    release_ages_ms: Vec::new(),
                })
            })
        })
    }

    fn held(&self) -> u64 {
        self.blocks.lock().unwrap().len() as u64
    }

    fn asks(&self) -> Vec<u64> {
        self.asks.lock().unwrap().clone()
    }
}

/// A co-writer's allocator (lane 1 of 2 on a 64-block device — a 32-block
/// lane share, watermark cap 8) wired to `supply`, with `minted` lane
/// blocks claimed and a claim rate sampled so the derived watermark reads
/// its cap: `reachable` = the lane's virgin remainder.
async fn laned_co_writer(id: &str, supply: &Arc<Supply>, minted: u64) -> Arc<BlockAllocator> {
    let a = allocator(id, 64).await;
    a.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2");
    a.set_lane_harvest_sink(supply.sink());
    // 20 claims seed the rate snapshots; the rest land inside one second
    // (10 blk/s ⇒ EWMA 2.5 blk/s ⇒ `ceil(rate × horizon)` past the
    // lane-share/4 cap on any derived horizon).
    let seed = minted.min(20);
    for _ in 0..seed {
        a.allocate_block().await.expect("mint");
    }
    a.sample_alloc_rate(10_000);
    for _ in seed..minted {
        a.allocate_block().await.expect("mint");
    }
    a.sample_alloc_rate(11_000);
    a
}

/// Contract (the refill-hint gate, finding 15 — the s11 fleet's second
/// starvation case): **the ahead tick fires on the hint alone.** The
/// authority's renewal grant says this lane has supply on its lists
/// (`free_grace_lane_supply_hint`); this allocator is OWED nothing (its
/// displaced blocks returned through the authority's publish recompute,
/// which notes nothing owed — ≈ 90 % of them on the s11 fleet); its
/// lane-reachable stock sits below the derived watermark ⇒ the ahead tick
/// harvests, asking for the full grain (never fewer than the grain when
/// the hint is larger), counted as an ahead harvest AND a hint refill. The
/// watermark law is untouched: stocked above it, a nonzero hint fires
/// nothing. Hint 0 ∧ owed 0 fires nothing (the quiet-lane posture — no RPC
/// storms on an idle lane). `SQUEEZEFS_ALLOC_LANE_REFILL_HINT=0` is the
/// retired owed-only gate verbatim; `HARVEST_AHEAD=0` the ENOSPC-only
/// shape.
#[tokio::test]
async fn the_ahead_refill_fires_on_the_hint_alone_below_the_watermark() {
    let _serial = serial();
    let _restore = refill_restore();
    squeezefs::block_allocator::test_set_harvest_ahead(Some(true));
    free_grace::test_set_lane_push(Some(true));
    let supply = Arc::new(Supply::default());
    let a = laned_co_writer("lane-hint-ahead", &supply, 30).await;
    // The ask the allocator derives at engagement (`reserve_grain_blocks`
    // over the device's capacity and the width).
    let grain = lane::reserve_grain_blocks(64, 2).max(1);
    assert_eq!(
        a.watermark_blocks(),
        8,
        "a 10 blk/s claimer derives the lane-share/4 cap on any horizon"
    );
    assert_eq!(
        a.lane_reachable_blocks(),
        2,
        "30 of the 32-block share minted"
    );
    assert_eq!(a.lane_owed_blocks(), 0, "nothing shipped ⇒ nothing owed");

    let m = &METRICS;
    let harvests0 = m.alloc_lane_harvests.load(Ordering::Relaxed);
    let ahead0 = m.alloc_lane_ahead_harvests.load(Ordering::Relaxed);
    let hint_refills0 = m.alloc_lane_hint_refills.load(Ordering::Relaxed);
    let harvested0 = m.alloc_lane_harvested_blocks.load(Ordering::Relaxed);

    // (3) hint 0 ∧ owed 0 ⇒ no proactive harvest, no RPC.
    assert_eq!(free_grace::lane_supply_hint(), 0);
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "no advertised supply and nothing owed ⇒ the tick stays dark"
    );
    assert_eq!(a.ahead_refill_tick(12_000).await, 0);
    assert!(supply.asks().is_empty(), "a dark tick sends no RPC");
    assert_eq!(m.alloc_lane_harvests.load(Ordering::Relaxed), harvests0);

    // (1) the authority's list holds 10 of this lane's blocks — the
    // co-writer's own displaced mints, recomputed there (their local
    // retire is finding 36's, noting nothing owed) — and the renewal grant
    // says so.
    let chunk = a.chunk_size();
    let displaced: Vec<u64> = (0..10u64).map(|i| 2 * i + 1).collect();
    for idx in &displaced {
        assert_eq!(lane::block_lane_of(*idx, 2), 1, "a lane-1 block");
        a.retire_shipped_free_tracking(idx * chunk);
    }
    supply
        .blocks
        .lock()
        .unwrap()
        .extend(displaced.iter().copied());
    free_grace::note_lane_supply_hint(supply.held());
    assert_eq!(free_grace::lane_supply_hint(), 10);
    assert_eq!(
        a.should_harvest_ahead(),
        Some(grain),
        "hint > 0 ∧ owed 0 ∧ reachable < watermark ⇒ the tick fires, asking a full grain"
    );
    let adopted = a.ahead_refill_tick(13_000).await;
    assert_eq!(adopted, 10, "the advertised supply is adopted");
    assert_eq!(
        supply.asks(),
        vec![grain],
        "one RPC, asking the grain — never fewer than the grain when the hint is larger"
    );
    assert_eq!(m.alloc_lane_harvests.load(Ordering::Relaxed), harvests0 + 1);
    assert_eq!(
        m.alloc_lane_ahead_harvests.load(Ordering::Relaxed),
        ahead0 + 1
    );
    assert_eq!(
        m.alloc_lane_hint_refills.load(Ordering::Relaxed),
        hint_refills0 + 1,
        "a proactive harvest the owed gate would have declined is a HINT refill"
    );
    assert_eq!(
        m.alloc_lane_harvested_blocks.load(Ordering::Relaxed),
        harvested0 + 10
    );
    assert_eq!(
        a.lane_owed_blocks(),
        0,
        "the owed ledger is the explicit arm's face — untouched"
    );
    assert_eq!(a.lane_reachable_blocks(), 12, "2 virgin + 10 adopted");

    // The watermark law is untouched: stocked above it, the hint fires
    // nothing (the hint is a supply witness, not a demand).
    assert!(free_grace::lane_supply_hint() > 0);
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "reachable ≥ watermark ⇒ no harvest, hint or not"
    );
    assert_eq!(a.ahead_refill_tick(14_000).await, 0);
    assert_eq!(supply.asks().len(), 1, "no second RPC");

    // Below the watermark again with the A/B control off: the retired
    // owed-only gate declines the hint; one owed block re-arms it.
    for _ in 0..10 {
        a.allocate_block()
            .await
            .expect("re-mint the adopted supply");
    }
    assert_eq!(a.lane_reachable_blocks(), 2);
    supply.blocks.lock().unwrap().push_back(21);
    free_grace::note_lane_supply_hint(supply.held());
    free_grace::test_set_refill_hint(Some(false));
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "REFILL_HINT=0: owed nothing ⇒ never harvested (the retired gate verbatim)"
    );
    a.note_owed_freed(1);
    assert_eq!(
        a.should_harvest_ahead(),
        Some(grain),
        "REFILL_HINT=0: owed > 0 ∧ reachable < watermark still fires"
    );
    assert!(free_grace::test_clear_refill_hint());
    assert_eq!(a.should_harvest_ahead(), Some(grain));

    // The ahead lever off restores the ENOSPC-only shape, hint included.
    squeezefs::block_allocator::test_set_harvest_ahead(Some(false));
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "HARVEST_AHEAD=0: shipped shape"
    );
    assert_eq!(a.ahead_refill_tick(15_000).await, 0);
    assert_eq!(supply.asks().len(), 1);
}

/// Contract (4): **the ENOSPC-path harvest is unchanged** — an exhausted
/// lane's allocation still runs the harvest inline before its verdict
/// (one RPC, adopt, retry through the funnel) with hint 0 and owed 0, and
/// that harvest is neither an ahead nor a pushed nor a HINT refill: the new
/// gauge counts only the proactive arms the hint armed.
#[tokio::test]
async fn the_enospc_path_harvest_is_unchanged_and_never_a_hint_refill() {
    let _serial = serial();
    let _restore = refill_restore();
    squeezefs::block_allocator::test_set_harvest_ahead(Some(true));
    free_grace::test_set_lane_push(Some(true));
    let supply = Arc::new(Supply::default());
    let a = laned_co_writer("lane-hint-enospc", &supply, 32).await;
    assert_eq!(a.lane_reachable_blocks(), 0, "the whole share minted");
    let chunk = a.chunk_size();
    // Three displaced blocks sit on the authority's list; no grant has
    // advertised them yet and nothing was shipped.
    for idx in [1u64, 3, 5] {
        a.retire_shipped_free_tracking(idx * chunk);
        supply.blocks.lock().unwrap().push_back(idx);
    }
    assert_eq!(free_grace::lane_supply_hint(), 0);
    assert_eq!(a.lane_owed_blocks(), 0);
    assert_eq!(a.should_harvest_ahead(), None, "the proactive arm is dark");

    let m = &METRICS;
    let harvests0 = m.alloc_lane_harvests.load(Ordering::Relaxed);
    let ahead0 = m.alloc_lane_ahead_harvests.load(Ordering::Relaxed);
    let pushed0 = m.alloc_lane_pushed_harvests.load(Ordering::Relaxed);
    let hint_refills0 = m.alloc_lane_hint_refills.load(Ordering::Relaxed);
    let refusals0 = m.alloc_lane_enospc_refusals.load(Ordering::Relaxed);

    let off = a
        .allocate_block()
        .await
        .expect("the ENOSPC-path harvest feeds the funnel before the verdict");
    assert!(
        [1u64, 3, 5].contains(&(off / chunk)),
        "a harvested block mints (got index {})",
        off / chunk
    );
    assert_eq!(supply.asks().len(), 1, "one inline harvest RPC");
    assert_eq!(m.alloc_lane_harvests.load(Ordering::Relaxed), harvests0 + 1);
    assert_eq!(m.alloc_lane_ahead_harvests.load(Ordering::Relaxed), ahead0);
    assert_eq!(
        m.alloc_lane_pushed_harvests.load(Ordering::Relaxed),
        pushed0
    );
    assert_eq!(
        m.alloc_lane_hint_refills.load(Ordering::Relaxed),
        hint_refills0,
        "the ENOSPC arm is never a hint refill"
    );
    assert_eq!(
        m.alloc_lane_enospc_refusals.load(Ordering::Relaxed),
        refusals0,
        "a fed funnel refuses nothing"
    );
    assert_eq!(a.lane_reachable_blocks(), 2, "the other two adopted blocks");
}

// ---------------------------------------------------------------------------
// 10. The single-flight harvest: one in-flight RPC per allocator
// ---------------------------------------------------------------------------
//
// Finding 15 phase B1 (`.benchmarks/2026-09-07-f15-day2-fleet-pair.md` §2):
// on the 8-co-writer fleet the file-per-proc phase drove 124,240 lane
// harvest RPCs in 9.5 minutes for 60,419 blocks — half of them empty —
// because every parked allocation on a co-writer issued its OWN harvest
// per park slice, and the authority's renewal serves (the liveness plane)
// queued behind the storm. These contracts pin the lever that cuts the
// storm by the park population: `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT`.

/// Restores the single-flight lever beside the refill posture.
struct FlightRestore {
    _refill: RefillRestore,
}

impl Drop for FlightRestore {
    fn drop(&mut self) {
        squeezefs::block_allocator::test_clear_harvest_single_flight();
    }
}

fn flight_restore() -> FlightRestore {
    squeezefs::block_allocator::test_clear_harvest_single_flight();
    FlightRestore {
        _refill: refill_restore(),
    }
}

/// The authority's lane list behind a GATE: every RPC parks inside the
/// sink until `release()` — the shape that holds one harvest in flight
/// while more callers arrive. `hint_ms` is the reply's bound-age hint
/// (nonzero = the authority reports a held ring, so an empty reply parks
/// the bounded allocation; 0 = nothing held, refuse at once).
struct GatedSupply {
    blocks: Mutex<VecDeque<u64>>,
    asks: Mutex<Vec<u64>>,
    gate: tokio::sync::watch::Sender<bool>,
    hint_ms: u64,
}

impl GatedSupply {
    fn new(blocks: impl IntoIterator<Item = u64>, hint_ms: u64) -> Arc<Self> {
        let (gate, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            blocks: Mutex::new(blocks.into_iter().collect()),
            asks: Mutex::new(Vec::new()),
            gate,
            hint_ms,
        })
    }

    fn sink(self: &Arc<Self>) -> lane::LaneHarvestSink {
        let me = Arc::clone(self);
        Arc::new(move |max: u64| {
            let me = Arc::clone(&me);
            Box::pin(async move {
                me.asks.lock().unwrap().push(max);
                let mut rx = me.gate.subscribe();
                rx.wait_for(|open| *open).await.expect("gate sender lives");
                let mut q = me.blocks.lock().unwrap();
                let n = (max as usize).min(q.len());
                let blocks: Vec<u64> = q.drain(..n).collect();
                Ok(lane::LaneHarvest {
                    release_ages_ms: vec![0; blocks.len()],
                    blocks,
                    bound_age_hint_ms: me.hint_ms,
                    rtt_ms: 1,
                })
            })
        })
    }

    fn release(&self) {
        self.gate.send_replace(true);
    }

    fn asks(&self) -> usize {
        self.asks.lock().unwrap().len()
    }
}

struct FlightGauges {
    harvests: u64,
    coalesced: u64,
    declined: u64,
    harvested: u64,
}

fn flight_gauges() -> FlightGauges {
    FlightGauges {
        harvests: METRICS.alloc_lane_harvests.load(Ordering::Relaxed),
        coalesced: METRICS.alloc_lane_harvest_coalesced.load(Ordering::Relaxed),
        declined: METRICS
            .alloc_lane_harvest_declined_stale
            .load(Ordering::Relaxed),
        harvested: METRICS.alloc_lane_harvested_blocks.load(Ordering::Relaxed),
    }
}

/// A co-writer's allocator (lane 1 of 2 on a 64-block device) with its
/// whole 32-block lane share minted: every allocation from here on is an
/// ENOSPC-path harvest.
async fn exhausted_co_writer(id: &str, sink: lane::LaneHarvestSink) -> Arc<BlockAllocator> {
    let a = allocator(id, 64).await;
    a.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2");
    a.set_lane_harvest_sink(sink);
    let share = lane::lane_capacity_blocks(64, 2, 1);
    for _ in 0..share {
        a.allocate_block().await.expect("mint the lane share");
    }
    assert_eq!(a.lane_reachable_blocks(), 0, "exhausted");
    a
}

fn storage_full(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull)
}

/// Spin (yielding) until `cond` holds or `bound` passes; `true` ⇔ it held.
async fn wait_until(bound: std::time::Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = std::time::Instant::now();
    while !cond() {
        if t0.elapsed() > bound {
            return false;
        }
        tokio::task::yield_now().await;
    }
    true
}

/// Contract (1): **N concurrent parked allocations on one allocator with
/// supply on the authority issue exactly ONE harvest RPC**, and all N
/// allocate from its result. The first caller leads; the other N−1 arrive
/// while the RPC is in flight and JOIN its outcome
/// (`alloc_lane_harvest_coalesced`) instead of each issuing their own;
/// `alloc_lane_harvests` counts the one RPC, so `harvests + coalesced`
/// accounts for every would-be call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_parked_allocations_on_one_allocator_issue_one_harvest_rpc() {
    let _serial = serial();
    let _restore = flight_restore();
    const N: usize = 8;
    // The authority holds 16 lane-1 blocks of this volume.
    let held: Vec<u64> = (0..16u64).map(|i| 2 * i + 1).collect();
    let supply = GatedSupply::new(held.iter().copied(), 0);
    let a = exhausted_co_writer("sf-one-rpc", supply.sink()).await;
    let chunk = a.chunk_size();
    for idx in &held {
        a.retire_shipped_free_tracking(idx * chunk);
    }
    let grain = lane::reserve_grain_blocks(64, 2).max(1) as usize;
    let expect_adopted = grain.min(held.len());
    assert!(
        expect_adopted >= N,
        "the fixture needs one grain to feed every caller (grain {grain})"
    );

    let g0 = flight_gauges();
    let mut tasks = Vec::new();
    for _ in 0..N {
        let a = Arc::clone(&a);
        tasks.push(tokio::spawn(async move {
            a.allocate_block_grace_bounded().await
        }));
    }
    // Every caller but the leader has joined the in-flight RPC.
    assert!(
        wait_until(std::time::Duration::from_secs(10), || {
            flight_gauges().coalesced == g0.coalesced + (N as u64 - 1)
        })
        .await,
        "N−1 callers join the one in-flight harvest (coalesced {} → {})",
        g0.coalesced,
        flight_gauges().coalesced
    );
    assert_eq!(supply.asks(), 1, "one RPC in flight, no second issued");
    supply.release();

    let mut got = Vec::new();
    for t in tasks {
        let off = t
            .await
            .expect("task")
            .expect("every caller allocates from the one harvest");
        let idx = off / chunk;
        assert!(held.contains(&idx), "a harvested lane-1 block (idx {idx})");
        got.push(idx);
    }
    got.sort_unstable();
    got.dedup();
    assert_eq!(got.len(), N, "N distinct offsets — no double handout");

    let g1 = flight_gauges();
    assert_eq!(supply.asks(), 1, "exactly ONE harvest RPC for N callers");
    assert_eq!(
        g1.harvests,
        g0.harvests + 1,
        "alloc_lane_harvests = the RPC count"
    );
    assert_eq!(g1.coalesced, g0.coalesced + (N as u64 - 1), "N−1 joiners");
    assert_eq!(g1.declined, g0.declined, "a fed harvest declines nobody");
    assert_eq!(
        g1.harvested,
        g0.harvested + expect_adopted as u64,
        "one grain adopted"
    );
    assert_eq!(
        a.lane_reachable_blocks(),
        (expect_adopted - N) as u64,
        "the rest of the grain is local now"
    );
}

/// Contract (2) + (3): **a fresh EMPTY reply declines re-issue until the
/// authority's advertisement moves.** After a harvest that returned 0
/// blocks the next callers decline (`alloc_lane_harvest_declined_stale`,
/// no RPC) until (a) a renewal grant refreshes the hint — the SAME value
/// included, since the grant cadence is what bounds the decline (and the
/// arrival counts whatever `SQUEEZEFS_FREE_GRACE_LANE_PUSH` says about the
/// value), (b) a nonzero hint wakes the refill, or (c) an owed `Freed`
/// verdict lands; each ends the window for exactly one RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_harvest_declines_re_issue_until_the_advertisement_moves() {
    let _serial = serial();
    let _restore = flight_restore();
    free_grace::test_set_lane_push(Some(true));
    let supply = GatedSupply::new([], 0);
    supply.release();
    let a = exhausted_co_writer("sf-decline", supply.sink()).await;
    let chunk = a.chunk_size();
    let g0 = flight_gauges();
    let gen0 = free_grace::lane_supply_hint_gen();

    // The first exhausted allocation asks (one RPC) and gets nothing.
    let e = a.allocate_block().await.expect_err("exhausted");
    assert!(storage_full(&e), "StorageFull: {e}");
    assert_eq!(supply.asks(), 1);
    assert_eq!(flight_gauges().harvests, g0.harvests + 1);

    // The next two decline: the advertisement has not moved.
    for i in 0..2 {
        let e = a.allocate_block().await.expect_err("still exhausted");
        assert!(storage_full(&e));
        assert_eq!(supply.asks(), 1, "declined: no RPC (attempt {i})");
    }
    let g1 = flight_gauges();
    assert_eq!(g1.harvests, g0.harvests + 1, "one RPC so far");
    assert_eq!(g1.declined, g0.declined + 2, "two declined callers");
    assert_eq!(g1.coalesced, g0.coalesced, "nothing was in flight to join");

    // (a) A grant refresh with the SAME value (0) ends the window: the
    // generation is the grant's arrival, the renewal cadence is the bound.
    free_grace::note_lane_supply_hint(0);
    assert_eq!(free_grace::lane_supply_hint_gen(), gen0 + 1, "one grant");
    let _ = a.allocate_block().await.expect_err("still exhausted");
    assert_eq!(supply.asks(), 2, "the grant re-armed exactly one RPC");
    let _ = a.allocate_block().await.expect_err("still exhausted");
    assert_eq!(supply.asks(), 2, "…and the empty reply declines again");

    // The arrival counts with the lane-push lever OFF too (the value is
    // dropped there; the grant still arrived).
    free_grace::test_set_lane_push(Some(false));
    free_grace::note_lane_supply_hint(0);
    assert_eq!(free_grace::lane_supply_hint_gen(), gen0 + 2);
    let _ = a.allocate_block().await.expect_err("still exhausted");
    assert_eq!(
        supply.asks(),
        3,
        "LANE_PUSH=0: a grant still ends the decline"
    );
    free_grace::test_set_lane_push(Some(true));

    // (c) An owed Freed verdict ends the window.
    let _ = a.allocate_block().await.expect_err("declined");
    assert_eq!(supply.asks(), 3);
    a.note_owed_freed(1);
    let _ = a.allocate_block().await.expect_err("still empty");
    assert_eq!(supply.asks(), 4, "owed moved ⇒ one RPC");

    // (b) Supply lands and the grant says so: the wake ends the window and
    // the harvest adopts it.
    let idx = 7u64;
    a.retire_shipped_free_tracking(idx * chunk);
    supply.blocks.lock().unwrap().push_back(idx);
    let _ = a
        .allocate_block()
        .await
        .expect_err("declined until the hint");
    assert_eq!(supply.asks(), 4);
    free_grace::note_lane_supply_hint(1);
    let off = a.allocate_block().await.expect("the advertised block");
    assert_eq!(off / chunk, idx);
    assert_eq!(supply.asks(), 5);
    let g2 = flight_gauges();
    assert_eq!(g2.harvests, g0.harvests + 5, "five RPCs in all");
    assert_eq!(g2.harvested, g0.harvested + 1);
    assert_eq!(
        g2.harvests - g0.harvests + (g2.declined - g0.declined) + (g2.coalesced - g0.coalesced),
        10,
        "harvests + declined + coalesced accounts for every would-be call"
    );
}

/// Contract (4): **two allocators (two volumes) are two independent
/// flights.** A harvest in flight on volume A coalesces nobody on B, and
/// A's empty-reply decline never declines B (nor B's decline A).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_volumes_are_independent_single_flights() {
    let _serial = serial();
    let _restore = flight_restore();
    // A: gated, holding supply. B: open, empty.
    let supply_a = GatedSupply::new([1u64, 3, 5, 7, 9, 11, 13, 15], 0);
    let supply_b = GatedSupply::new([], 0);
    supply_b.release();
    let a = exhausted_co_writer("sf-vol-a", supply_a.sink()).await;
    let b = exhausted_co_writer("sf-vol-b", supply_b.sink()).await;
    for idx in [1u64, 3, 5, 7, 9, 11, 13, 15] {
        a.retire_shipped_free_tracking(idx * a.chunk_size());
    }
    let g0 = flight_gauges();

    // A's leader parks in its RPC.
    let leader = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { a.allocate_block_grace_bounded().await })
    };
    assert!(
        wait_until(std::time::Duration::from_secs(10), || supply_a.asks() == 1).await,
        "A's harvest is in flight"
    );

    // B's callers: their own RPC (empty), then B's decline — A's flight
    // coalesces none of them.
    let _ = b.allocate_block().await.expect_err("B exhausted");
    assert_eq!(supply_b.asks(), 1, "B issued its own RPC");
    let _ = b.allocate_block().await.expect_err("B exhausted");
    assert_eq!(supply_b.asks(), 1, "B declined on B's empty reply");
    let g1 = flight_gauges();
    assert_eq!(g1.coalesced, g0.coalesced, "B never joined A's flight");
    assert_eq!(g1.declined, g0.declined + 1, "B's decline is B's");

    // A's second caller joins A's flight; B's decline does not touch it.
    let joiner = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { a.allocate_block_grace_bounded().await })
    };
    assert!(
        wait_until(std::time::Duration::from_secs(10), || {
            flight_gauges().coalesced == g0.coalesced + 1
        })
        .await,
        "A's second caller joined A's flight"
    );
    supply_a.release();
    leader.await.unwrap().expect("A's leader allocates");
    joiner.await.unwrap().expect("A's joiner allocates");
    let g2 = flight_gauges();
    assert_eq!(supply_a.asks(), 1, "one RPC on A");
    assert_eq!(g2.harvests, g0.harvests + 2, "one RPC per volume");
    assert_eq!(g2.declined, g0.declined + 1, "only B's caller declined");
    assert_eq!(g2.coalesced, g0.coalesced + 1, "only A's joiner coalesced");
}

/// Contract (5): **the lever off is one RPC per caller** — the shipped
/// shape verbatim: N concurrent callers issue N RPCs, nobody joins, an
/// empty reply declines nobody. The same fixture as contract (1) makes the
/// shipped cost legible: the first RPC to reach the authority drains the
/// grain, the other N−1 come back EMPTY and their callers refuse
/// `StorageFull` — with the harvested blocks sitting on the local free
/// list — because a caller retries its funnel only after ITS OWN reply
/// carried blocks. (Under the lever, contract (1): one RPC, all N fed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lever_off_issues_one_rpc_per_caller() {
    let _serial = serial();
    let _restore = flight_restore();
    squeezefs::block_allocator::test_set_harvest_single_flight(Some(false));
    const N: usize = 4;
    let held: Vec<u64> = (0..16u64).map(|i| 2 * i + 1).collect();
    let supply = GatedSupply::new(held.iter().copied(), 0);
    let a = exhausted_co_writer("sf-lever-off", supply.sink()).await;
    let chunk = a.chunk_size();
    for idx in &held {
        a.retire_shipped_free_tracking(idx * chunk);
    }
    assert!(
        lane::reserve_grain_blocks(64, 2) >= held.len() as u64,
        "one grain covers the whole held supply"
    );
    let g0 = flight_gauges();
    let mut tasks = Vec::new();
    for _ in 0..N {
        let a = Arc::clone(&a);
        tasks.push(tokio::spawn(async move {
            a.allocate_block_grace_bounded().await
        }));
    }
    // With the lever off every caller reaches the sink itself.
    assert!(
        wait_until(std::time::Duration::from_secs(10), || supply.asks() == N).await,
        "N callers ⇒ N RPCs in flight (got {})",
        supply.asks()
    );
    supply.release();
    let (mut fed, mut refused) = (0usize, 0usize);
    for t in tasks {
        match t.await.unwrap() {
            Ok(off) => {
                assert!(held.contains(&(off / chunk)));
                fed += 1;
            }
            Err(e) => {
                assert!(storage_full(&e), "{e}");
                refused += 1;
            }
        }
    }
    // The lever-off law is about the RPC COUNT (one per caller — below),
    // not about refusing: the RPC that drained the grain feeds its caller
    // and the N−1 empty replies used to refuse theirs while the rest of the
    // grain sat on the local list — until the ENOSPC arm re-checked the
    // list before its verdict (the trim-claim-window park,
    // `.benchmarks/2026-09-07-overlay-enospc-convergence-flake.md`): a
    // refusal with supply on the local list is never correct, so every
    // caller is fed from the one grain.
    assert_eq!(
        (fed, refused),
        (N, 0),
        "every caller is fed from the grain the first RPC drained; none refuses with \
         supply on the local list"
    );
    assert_eq!(
        a.lane_reachable_blocks(),
        held.len() as u64 - N as u64,
        "…while the rest of the grain sits on the local list"
    );
    let g1 = flight_gauges();
    assert_eq!(g1.harvests, g0.harvests + N as u64, "one RPC per caller");
    assert_eq!(
        g1.coalesced, g0.coalesced,
        "nobody joins with the lever off"
    );

    // An empty reply declines nobody: drain the supply, then two callers
    // are two RPCs.
    supply.blocks.lock().unwrap().clear();
    while a.allocate_block().await.is_ok() {}
    let asks = supply.asks();
    let _ = a.allocate_block().await.expect_err("exhausted");
    assert_eq!(supply.asks(), asks + 1);
    let g2 = flight_gauges();
    assert_eq!(g2.declined, g0.declined, "the lever off never declines");
    assert!(squeezefs::block_allocator::test_clear_harvest_single_flight());
}

/// Contract (6): **the ENOSPC verdict's timing is unchanged — no caller
/// waits past the wall.** A joiner whose leader's RPC outlives the wall
/// gives up the join at the wall and takes its verdict (its bounded
/// allocation refuses `StorageFull` within the wall) while the leader is
/// still in flight; the leader's own outcome is unaffected. And a parked
/// allocation whose declined retries never re-issue still ends at the
/// wall: the decline shortens nothing and lengthens nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_caller_waits_past_the_wall() {
    let _serial = serial();
    let _restore = flight_restore();
    let wall = free_grace::pressure_park_wall_ms();
    let bound = std::time::Duration::from_millis(wall + 2_000);

    // The joiner half: a leader parked in a slow RPC.
    let supply = GatedSupply::new([1u64, 3, 5], 0);
    let a = exhausted_co_writer("sf-wall-join", supply.sink()).await;
    for idx in [1u64, 3, 5] {
        a.retire_shipped_free_tracking(idx * a.chunk_size());
    }
    let g0 = flight_gauges();
    let leader = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { a.allocate_block_grace_bounded().await })
    };
    assert!(
        wait_until(std::time::Duration::from_secs(10), || supply.asks() == 1).await,
        "the leader's RPC is in flight"
    );
    let t0 = std::time::Instant::now();
    let joined = tokio::time::timeout(bound, a.allocate_block_grace_bounded())
        .await
        .expect("the joiner must end within the wall bound");
    let elapsed = t0.elapsed();
    let e = joined.expect_err("the joiner takes its verdict without the leader's reply");
    assert!(storage_full(&e), "{e}");
    assert!(
        elapsed.as_millis() as u64 <= wall + 1_000,
        "the join gave up at the wall ({wall} ms) — took {elapsed:?}"
    );
    assert_eq!(supply.asks(), 1, "the joiner issued no RPC of its own");
    assert_eq!(flight_gauges().coalesced, g0.coalesced + 1, "it joined");
    supply.release();
    leader
        .await
        .unwrap()
        .expect("the leader's outcome is untouched by the joiner's wall");

    // The park half: an exhausted lane whose authority reports a held ring
    // (bound age nonzero) but hands out nothing — the field's m50 shape.
    // The bounded allocation parks, its retries DECLINE (one RPC in all),
    // and it refuses at the wall exactly as before.
    let empty = GatedSupply::new([], 15_096);
    empty.release();
    let b = exhausted_co_writer("sf-wall-park", empty.sink()).await;
    let parks0 = free_grace::pressure_parks();
    let g0 = flight_gauges();
    let t0 = std::time::Instant::now();
    let verdict = tokio::time::timeout(bound, b.allocate_block_grace_bounded())
        .await
        .expect("the park must end within the wall bound");
    let elapsed = t0.elapsed();
    let e = verdict.expect_err("still exhausted");
    assert!(storage_full(&e), "{e}");
    assert!(free_grace::pressure_parks() > parks0, "the park engaged");
    assert!(
        elapsed.as_millis() as u64 <= wall + 1_000,
        "the park ends at the wall ({wall} ms) — took {elapsed:?}"
    );
    let g1 = flight_gauges();
    assert_eq!(empty.asks(), 1, "ONE RPC for the whole park");
    assert_eq!(g1.harvests, g0.harvests + 1);
    assert!(
        g1.declined > g0.declined,
        "every retry after the empty reply declined"
    );
}
