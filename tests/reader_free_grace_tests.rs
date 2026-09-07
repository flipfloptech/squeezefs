//! **Spec §6.8 item 3 — the freed-offset grace period**
//! (`docs/pre-rc-engineering-spec.md` §6.3, §6.8 item 3, §6.9 S5/S9;
//! `docs/pre-rc-execution-plan.md` Phase 4; implementation
//! `src/free_grace.rs`).
//!
//! ## The hole this closes
//!
//! §6.3, verbatim: the read path's serve proof is "bytes for key K serve
//! for block *b* iff the fetch was incarnation-valid and the current map
//! still binds *b → K*", and **both premises are process-local**. So when
//! the writer overwrites a block (CoW), frees the offset, and the
//! allocator reissues it to a **different file**, a reader whose cached
//! map still binds *b → K* serves the other file's bytes — and block keys
//! are bare reusable device offsets, so even a reader with FRESH metadata
//! can serve stale CACHED BYTES for a reused key. On a transformed
//! (compressed/encrypted) volume that is loud (the AEAD tag fails); on a
//! **passthrough volume, which is the default, it is silent**. S5 bounded
//! the window to one revalidation interval; item 3 is what ELIMINATES it,
//! which is why the spec calls it "the highest-value single item in the
//! coherence analysis" and why §6.3 makes it a multi-writer prerequisite.
//!
//! ## The mechanism
//!
//! The writer already maintains the freed-offset log (the reclaim queue).
//! Item 3 adds ONE gate: **a terminally-freed offset does not re-enter the
//! free list until every live registered reader has acknowledged passing
//! the free's epoch**, and *"a reader that fails to acknowledge is fenced,
//! not waited on"*. The acknowledgement channel is DLM S6 membership
//! (`MemberSession::ack_free_epoch` → `MembershipOwner::{
//! min_acked_free_epoch, members_behind_free_epoch, evict}`) — a reader
//! performs no metadata write, so the `client:` heartbeat the spec named
//! could never have carried it.
//!
//! **Epoch identity.** The label is the OWNER's own monotonic instant
//! (`Grant::granted_at_owner_ms`, already on the wire), and a reader only
//! ever echoes a label the owner HANDED it — a causal token, never a
//! foreign clock read as a deadline. The reader's *qualification* for
//! echoing one is expressed entirely in revalidation-epoch terms: a
//! completed epoch-step purge whose pass began after the label was learned
//! plus the published staleness bound, then the drain wait. The per-volume
//! ledger sequence cannot BE the scalar (the channel is one `u64`, and
//! min/max composition across N independent per-volume counters starves in
//! an unbalanced set — the 2026-07-30 field shape of one volume at 25k
//! writes/s beside a sibling at 0.00).
//!
//! ## Contracts pinned here
//!
//! 1. **Zero cost, zero behaviour change when unarmed** (the shipped
//!    default `SQUEEZEFS_MEMBERSHIP_BIND=off`): a terminal free publishes
//!    to the free list in the same call, the offset is immediately
//!    reallocatable, and every item-3 gauge stays 0.
//! 2. **A freed offset refuses reallocation until acknowledged**: no
//!    allocation path (free-list claim, contiguity pick, ascending pick,
//!    fresh mint) can hand it out, and it is not on the free list.
//! 3. **The bound is the minimum across live readers**, and an
//!    acknowledgement that advances it RELEASES the offset.
//! 4. **A laggard is fenced, not waited on**: past the derived grace bound
//!    the writer names the laggards, evicts them through S6, and
//!    allocation resumes — bounded, loud, counted.
//! 5. **Space pressure is ENOSPC, never an unacknowledged early release**:
//!    a full store whose free list is entirely in grace refuses
//!    `StorageFull` promptly (counted), and the PRESSURE deadline (one ack
//!    cycle) — not the routine bound — is what eventually fences.
//! 6. **Composition with the async reclaim queue**: grace sits strictly
//!    DOWNSTREAM of the device reclaim (`finish_free` is its only entry),
//!    so a fence-halted queue still drops entries without `finish_free`
//!    and without entering grace, and a released offset publishes exactly
//!    once (no double free).
//! 7. **Grace and S7's dead-epoch quarantine are independent gates**: an
//!    offset must clear BOTH, in that order (custody proof first, reader
//!    coherence second).
//! 8. **The ring is bounded**: at cap it forces progress through the same
//!    fence act (never a silent early release, never unbounded RAM).
//! 9. **The reader's ack is emitted after the purge AND the drain**: an
//!    inert poll (no epoch step ⇒ no purge) never qualifies, a pass that
//!    began too early never qualifies, and the ack waits the drain window
//!    before it rides a renewal.
//! 10. **Both derivations are drift-is-red**: the grace bound derives from
//!     the membership clocks plus the reader staleness bound, the ring cap
//!     from the R5 budget, and an explicit bound below one honest ack
//!     cycle REFUSES (it would fence a healthy reader).
//! 11. **Graced offsets are honestly accounted**: they read as USED, and
//!     the fsck reconciliation never steals one back onto the free list.
//! 12. **Concurrency**: readers acking while a writer frees and
//!     reallocates in a loop never yields a live-offset collision, never a
//!     double free, and the ledger closes
//!     (`deferrals == releases + held`).
//!
//! RED against `feat/reader-freed-offset-grace`'s parent: `squeezefs::
//! free_grace` does not exist, the allocator has no grace ring,
//! `MembershipOwner::refresh_free_grace_bound` / `membership::
//! installed_owner` / `MemberSession::learned_label` do not exist.
//!
//! ## The pressure-coupled release valve (rung-20 residual 6)
//!
//! The field convicted the cadence, not the mechanism: under a rewrite
//! storm a writer's deferrals outrun the readers' releases on their
//! NATURAL beat, `free_grace_offsets` climbs monotonically and the lane's
//! share runs out (`.benchmarks/2026-08-19-blob-aware-merge-and-fabric-venue.md`
//! §3 — 0 → 825 across one 8-rank row, never draining; the 2026-08-18
//! prep row's 21,001 deferrals vs 17,751 releases is the same shape with
//! the cadence merely slower than the rewrite rate). Contracts 13–18 pin
//! the graded ladder that answers it:
//!
//! 13. **The convicted storm never reaches rung (c)**: the pressure signal
//!     (the ring's own measured deferral rate against BOTH supplies — ring
//!     headroom and the volume's free blocks) engages rungs (a) prod and
//!     (b) tighten, allocation never stalls, and nothing is fenced.
//! 14. **Rung (c) is intact**: a member that beats but never acknowledges
//!     is prodded first and FENCED after — with its eviction, exactly as
//!     the law says, and the writer progresses.
//! 15. **The published bound is the bound in force**: the tightened
//!     deadline is what `free_grace_fence_bound_ms` reads (the routine
//!     derivation stays visible as `..._base_ms`), and the tightening
//!     never crosses the floor of one honest acknowledgement cycle — the
//!     number the READER's published staleness bound derives, which the
//!     valve never moves.
//! 16. **Unarmed engages no rung**: every valve gauge is 0 and no prod is
//!     ever in force on the shipped default.
//! 17. **Every threshold is derived**: the runway reading, the graded
//!     deadline interpolation and the prodded cadence are pure functions
//!     of numbers the plane already publishes (drift-is-red).
//! 18. **A tightened cadence can never starve the ladder**: the
//!     acknowledgement qualifies against the label the ladder SNAPSHOTTED,
//!     never against whatever the newest renewal has since learned — a
//!     beat faster than `staleness + skew_max` would otherwise refresh the
//!     label out from under every pass and the reader would ack NOTHING,
//!     which is the failure the valve exists to prevent (and which an
//!     ordinary `SQUEEZEFS_META_FLUSH_INTERVAL_MS` already makes reachable
//!     without any valve at all).
//!
//! ## The sustain campaign's closed loop (D-4, ladder row 13)
//!
//! `docs/design-free-grace-sustain.md`'s §3 rate equation, driven end to
//! end in-process on the manual owner clock (the `LoopShape` harness —
//! every decision is product code: labels, runway, rungs (a)/(a′), site 0,
//! the ladder's gates, the min-composition, the sweep, the L3 refresh).
//! Four shapes × the four lever configurations of PR 5's A/B (A0 =
//! pre-campaign, A3 = shipped), printed as `ROW` lines for
//! `.benchmarks/2026-09-05-d4-free-grace-sustain.md`:
//!
//! 19. **The recycle-bound stream releases faster with the levers and
//!     never fences**: closure on every configuration, `forced = fences
//!     = 0`, A3 sustains the stream above A0 with a lower `bound_age` and
//!     no more stalls, and site 0 observes the coupling.
//! 20. **Little's law holds on a still-bound stream**: `rate ≈ spare ÷
//!     bound_age` on every configuration (the §3.3 reconciliation on live
//!     gauges), and the shipped ceiling sits above the pre-campaign one by
//!     the latency it removes.
//! 21. **Finding D4-1 (economy, pinned as current behavior)**: the
//!     re-based runway divides ONE lane's supply by the FLEET's ring rate,
//!     so a mid-supply lane is asked the floor cadence for a storm's whole
//!     duration — buying parked inventory, never throughput.
//! 22. **An uncoupled fleet runs the routine beat and the demand arm stays
//!     dark** (the 2026-08-30 GREEN s11-mpiio row's shape): `demand_waits
//!     = prods = stalls = 0`, `bound_age` at the routine composite under
//!     every configuration — PR 5's gate (c) is a statement about a
//!     COUPLED storm.
//!
//! ## The hold-time campaign (contracts 23–31) and the writer→member
//! checkpoint composite (contracts 32–36, user decision 2026-09-06)
//!
//! Contracts 23–31 (`.benchmarks/2026-09-06-free-grace-hold-time.md`)
//! decompose the hold per stage and land levers (b)/(d); contract 31
//! measured a faster writer checkpoint INERT alone. Contracts 32–36
//! (`.benchmarks/2026-09-06-free-grace-checkpoint-composite.md`) land it
//! as the composite: the writer's ceiling follows the valve's ask (P/2),
//! the grant carries it, the member's prod floor and L2b pass floor follow
//! it — the cadences halve, the hold drops, the cost is the accepted 2×,
//! the published staleness bound is honoured, and the lever off is the
//! shipped H3 row to the tick.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::block_reclaim::{ReclaimEntry, ReclaimQueue};
use squeezefs::data_custody::declare_dead_epoch;
use squeezefs::error::SqueezefsError;
use squeezefs::free_grace::{self, AckInputs, GraceRing, ReaderAckLadder};
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{
    self, Grant, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MemberSession,
    MembershipOwner, RenewOutcome,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Every test here drives PROCESS-GLOBAL state (the item-3 plane, its
/// gauges, the membership registry) — libtest runs a file's tests on
/// threads and the gate's `--test-threads=1` bounds files, not tests
/// within one. Same shape and reasoning as
/// `tests/dlm_membership_tests.rs`.
static SERIAL: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    free_grace::reset_for_test();
    membership::uninstall();
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        free_grace::reset_for_test();
        membership::uninstall();
        SERIAL.store(false, Ordering::Release);
    }
}

/// A manual lease clock plus its tick word: the grace bound and the
/// reader's ack ladder must both be provable without a sleep (the
/// `cluster_wire::WireClock` / `membership::LeaseClock::Manual`
/// precedent).
fn manual_clock() -> (LeaseClock, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(10_000));
    (LeaseClock::manual(Arc::clone(&ticks)), ticks)
}

fn shipped_clocks() -> LeaseClocks {
    LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation must be safe")
}

/// An armed OWNER on `clock`, installed for the process (which is what
/// makes the writer-side gate reachable from the allocator).
fn armed_owner(clock: &LeaseClock) -> Arc<MembershipOwner> {
    let owner = MembershipOwner::arm("grace-owner", 3, 2, shipped_clocks(), clock.clone())
        .expect("arming a successor term must be admitted");
    membership::install_owner(Arc::clone(&owner));
    owner
}

fn join(owner: &MembershipOwner, id: &str, role: MemberRole) -> Grant {
    match owner.join(JoinRequest {
        id: id.to_string(),
        role,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-grace-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    }
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    Arc::new(BlockAllocator::new(id).await.expect("allocator"))
}

fn free_listed(ba: &BlockAllocator, offset: u64) -> bool {
    ba.free_block_indices()
        .contains(&(offset / ba.chunk_size()))
}

/// Acknowledge `label` for `id` on its renewal — the production channel
/// (the value rides the beat) — and republish the writer's bound.
fn ack(owner: &Arc<MembershipOwner>, id: &str, epoch: u64, label: u64) {
    assert!(
        matches!(owner.renew(id, epoch, label), RenewOutcome::Renewed(_)),
        "the renewal that carries an acknowledgement must be admitted"
    );
    owner.refresh_free_grace_bound();
}

// ---------------------------------------------------------------------------
// 1 — zero cost, zero behaviour change when the reader plane is unarmed
// ---------------------------------------------------------------------------

/// The shipped default is `SQUEEZEFS_MEMBERSHIP_BIND=off`, so the common
/// mount must be BYTE-IDENTICAL: the free publishes in the same call, the
/// offset is immediately reallocatable, and every item-3 gauge stays 0
/// (the gate is one relaxed load feeding a never-taken branch).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unarmed_mount_publishes_frees_immediately_and_moves_no_gauge() {
    let _serial = serial();
    let ba = allocator("grace-unarmed").await;
    assert!(!free_grace::armed(), "no plane is armed by default");

    let first = ba.allocate_block().await.expect("allocate");
    ba.free_block(first).await.expect("free");
    assert!(
        free_listed(&ba, first),
        "an unarmed mount publishes the terminal free in the same call"
    );
    assert_eq!(ba.grace_len(), 0, "nothing is held");
    let again = ba.allocate_block().await.expect("reallocate");
    assert_eq!(
        again, first,
        "the free list serves the offset straight back"
    );

    assert_eq!(free_grace::deferrals(), 0);
    assert_eq!(free_grace::held_offsets(), 0);
    assert_eq!(free_grace::held_bytes(), 0);
    assert_eq!(free_grace::releases(), 0);
    assert_eq!(free_grace::forced_releases(), 0);
    assert_eq!(free_grace::laggard_fences(), 0);
    assert_eq!(free_grace::bound(), u64::MAX, "no plane ⇒ no bound");
    // The sustain campaign's instruments obey the same solo re-gate
    // (design-free-grace-sustain §8): zero movement on an unarmed mount.
    assert_eq!(free_grace::demand_waits(), 0);
    assert_eq!(free_grace::bound_age_ms(), 0);
    assert_eq!(free_grace::residence_samples(), 0);
}

// ---------------------------------------------------------------------------
// 2 + 3 — the gate, and the acknowledgement that opens it
// ---------------------------------------------------------------------------

/// A freed offset is not reallocatable until every live reader has
/// acknowledged passing its label — enforced AT the allocator, so no
/// allocation path can hand it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_freed_offset_refuses_reallocation_until_the_reader_acknowledges() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("the derived bound is safe");
    let reader = join(&owner, "r-1", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    assert!(free_grace::armed(), "a live member arms the gate");
    assert_eq!(
        free_grace::bound(),
        0,
        "a reader that has acknowledged nothing holds the bound at 0"
    );

    let ba = allocator("grace-gate").await;
    let victim = ba.allocate_block().await.expect("allocate");
    ba.free_block(victim).await.expect("free");

    assert_eq!(ba.grace_len(), 1, "the free is held in grace");
    assert!(
        !free_listed(&ba, victim),
        "a graced offset must never be on the free list"
    );
    assert_eq!(free_grace::deferrals(), 1);
    assert_eq!(free_grace::held_offsets(), 1);
    assert_eq!(free_grace::held_bytes(), ba.chunk_size());

    // No allocation path may hand it out.
    for _ in 0..4 {
        let fresh = ba.allocate_block().await.expect("allocate");
        assert_ne!(fresh, victim, "a graced offset was reallocated");
    }
    assert!(ba.allocate_block_below(1024).is_none());
    let above = ba.allocate_block_at_or_above(0).expect("ascending pick");
    assert_ne!(
        above, victim,
        "the ascending pick handed out a graced offset"
    );

    // The acknowledgement — carried by the reader's renewal — releases it.
    let label = ba
        .grace_oldest_label()
        .expect("the held entry carries a label");
    ack(&owner, "r-1", reader.epoch, label);
    assert!(free_grace::bound() >= label);
    let reused = ba.allocate_block().await.expect("allocate");
    assert_eq!(
        reused, victim,
        "an acknowledged offset returns to the free list and is served again"
    );
    assert_eq!(ba.grace_len(), 0);
    assert_eq!(free_grace::releases(), 1);
    assert_eq!(free_grace::forced_releases(), 0, "nobody was fenced");
    assert_eq!(free_grace::held_offsets(), 0);
}

/// The bound is the MINIMUM across live readers: one reader that has not
/// caught up holds every freed offset, and its acknowledgement is what
/// releases them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bound_is_the_minimum_across_live_readers() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let a = join(&owner, "r-a", MemberRole::Reader);
    let b = join(&owner, "r-b", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-min").await;
    let victim = ba.allocate_block().await.expect("allocate");
    ba.free_block(victim).await.expect("free");
    let label = ba.grace_oldest_label().expect("label");

    // The fast reader acknowledges far past the label; the slow one has
    // not moved. The minimum governs.
    ack(&owner, "r-a", a.epoch, label + 1_000);
    assert_eq!(free_grace::bound(), 0, "the laggard holds the minimum");
    assert_eq!(ba.grace_len(), 1);
    let other = ba.allocate_block().await.expect("allocate");
    assert_ne!(other, victim);

    ack(&owner, "r-b", b.epoch, label);
    assert!(free_grace::bound() >= label);
    assert_eq!(
        ba.allocate_block().await.expect("allocate"),
        victim,
        "the last acknowledgement releases the offset"
    );
    assert_eq!(free_grace::laggard_fences(), 0, "nobody had to be fenced");
}

// ---------------------------------------------------------------------------
// 4 — "a reader that fails to acknowledge is fenced, not waited on"
// ---------------------------------------------------------------------------

/// Past the derived grace bound the writer names the laggards, EVICTS them
/// through S6 and continues. An unbounded wait would convert a slow reader
/// into a writer-side ENOSPC, which is the worse failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_laggard_is_fenced_not_waited_on() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    // An explicit bound (still ≥ one honest ack cycle) keeps the tick
    // arithmetic readable; the derived default is pinned separately.
    let cycle = free_grace::ack_cycle(owner.clocks());
    free_grace::arm_owner_plane_with(clock.clone(), cycle * 2, cycle);
    let reader = join(&owner, "r-mute", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-fence").await;
    let victim = ba.allocate_block().await.expect("allocate");
    ba.free_block(victim).await.expect("free");
    assert_eq!(ba.grace_len(), 1);

    // Inside the bound the writer WAITS (the reader may be mid-cycle).
    ticks.fetch_add(cycle.as_millis() as u64, Ordering::SeqCst);
    let _ = ba.allocate_block().await.expect("allocate");
    assert_eq!(ba.grace_len(), 1, "inside the bound the offset is held");
    assert_eq!(free_grace::laggard_fences(), 0);
    assert!(
        owner.epoch_of("r-mute").is_some(),
        "a reader inside the bound is never evicted"
    );

    // Past it the laggard is fenced and allocation resumes.
    let evictions0 = METRICS.membership_evictions.load(Ordering::Relaxed);
    ticks.fetch_add(cycle.as_millis() as u64 * 2, Ordering::SeqCst);
    let _ = ba.allocate_block().await.expect("allocate");
    assert_eq!(free_grace::laggard_fences(), 1, "the laggard was fenced");
    assert!(
        METRICS.membership_evictions.load(Ordering::Relaxed) > evictions0,
        "the fence is an S6 eviction, not a private mechanism"
    );
    assert!(
        owner.epoch_of("r-mute").is_none(),
        "the fenced member left the census"
    );
    assert_eq!(ba.grace_len(), 0, "the offset was released after the fence");
    assert!(
        free_grace::forced_releases() >= 1,
        "a release past the bound is counted as FORCED, never as an acknowledgement"
    );
    let _ = reader;

    // With no members left the gate disarms and frees publish directly.
    assert!(!free_grace::armed());
    let next = ba.allocate_block().await.expect("allocate");
    ba.free_block(next).await.expect("free");
    assert!(free_listed(&ba, next));
}

// ---------------------------------------------------------------------------
// 5 — the space-pressure ruling
// ---------------------------------------------------------------------------

/// **The ruling (S7's quarantine precedent, argued for the grace period):
/// a full store refuses ENOSPC; it NEVER releases an unacknowledged
/// offset.** Serving another file's bytes to a reader — silently, on a
/// passthrough volume — is worse than a bounded availability loss, and
/// unlike the quarantine's proof this wait always resolves itself: the
/// PRESSURE deadline (one honest ack cycle, half the routine bound) is
/// what fences.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_pressure_refuses_enospc_and_never_releases_unacknowledged() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    let cycle = free_grace::ack_cycle(owner.clocks());
    free_grace::arm_owner_plane_with(clock.clone(), cycle * 4, cycle);
    let _reader = join(&owner, "r-slow", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-pressure").await;
    ba.set_capacity_bytes(4 * ba.chunk_size());
    let mut offs = Vec::new();
    for _ in 0..4 {
        offs.push(ba.allocate_block().await.expect("allocate"));
    }
    for o in &offs {
        ba.free_block(*o).await.expect("free");
    }
    assert_eq!(ba.grace_len(), 4, "the whole store is in grace");

    let stalls0 = free_grace::alloc_stalls();
    let t0 = std::time::Instant::now();
    let err = ba
        .allocate_block()
        .await
        .expect_err("a fully graced store must refuse");
    assert!(
        matches!(&err, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull),
        "the verdict is StorageFull, got {err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "the refusal must be PROMPT — a grace period that can wedge allocation is a bug"
    );
    assert!(
        free_grace::alloc_stalls() > stalls0,
        "the stall is counted, not silent"
    );
    assert_eq!(
        ba.grace_len(),
        4,
        "pressure released nothing unacknowledged"
    );
    assert_eq!(free_grace::forced_releases(), 0);

    // Past the PRESSURE deadline — one ack cycle, well inside the routine
    // bound — the laggard is fenced and the store recovers.
    ticks.fetch_add(cycle.as_millis() as u64 + 1, Ordering::SeqCst);
    let recovered = ba
        .allocate_block()
        .await
        .expect("post-fence allocation succeeds");
    assert!(offs.contains(&recovered));
    assert_eq!(free_grace::laggard_fences(), 1);
}

// ---------------------------------------------------------------------------
// 6 — composition with the async reclaim queue
// ---------------------------------------------------------------------------

/// Grace sits strictly DOWNSTREAM of the device reclaim: `finish_free` is
/// its only entry, so the reclaim invariants are untouched — a fenced
/// queue still drops entries WITHOUT `finish_free` (and therefore without
/// entering grace), and a completed reclaim's offset is graced instead of
/// published, then published exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_composes_with_the_reclaim_queue() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let reader = join(&owner, "r-reclaim", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-reclaim").await;
    let doubles0 = METRICS.block_double_frees.load(Ordering::Relaxed);

    // (a) A FENCED queue drops the entry without finish_free — nothing
    // enters grace, and the offset stays unallocatable either way.
    let halted = ReclaimQueue::from_env();
    halted.set_fence_signal(Arc::new(|| true));
    let fenced_off = ba.allocate_block().await.expect("allocate");
    assert!(ba.begin_free(fenced_off), "terminal free");
    let inflight = ba.inflight_register(fenced_off);
    halted
        .enqueue(ReclaimEntry {
            allocator: ba.clone(),
            inflight,
            device_path: "/dev/null".to_string(),
            offset: fenced_off,
            size: ba.chunk_size(),
        })
        .await;
    assert_eq!(halted.drain_off_thread().await, 1, "the entry was consumed");
    assert_eq!(ba.grace_len(), 0, "no finish_free ⇒ no grace entry");
    assert!(
        !free_listed(&ba, fenced_off),
        "no finish_free without reclaim"
    );

    // (b) A COMPLETED reclaim's finish_free defers into grace.
    let q = ReclaimQueue::from_env();
    let victim = ba.allocate_block().await.expect("allocate");
    assert!(ba.begin_free(victim), "terminal free");
    let inflight = ba.inflight_register(victim);
    q.enqueue(ReclaimEntry {
        allocator: ba.clone(),
        inflight,
        device_path: "/dev/null".to_string(),
        offset: victim,
        size: ba.chunk_size(),
    })
    .await;
    let _ = q.drain_off_thread().await;
    assert_eq!(ba.grace_len(), 1, "the reclaimed offset is graced");
    assert!(!free_listed(&ba, victim));

    // (c) The acknowledgement publishes it EXACTLY once.
    let label = ba.grace_oldest_label().expect("label");
    ack(&owner, "r-reclaim", reader.epoch, label);
    let _ = ba.allocate_block().await.expect("allocate");
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles0,
        "the deferred publish is not a second free"
    );
}

// ---------------------------------------------------------------------------
// 7 — grace and S7's dead-epoch quarantine are independent gates
// ---------------------------------------------------------------------------

/// The two mechanisms answer different questions — S7: *"can a
/// possibly-live zombie still DMA into this offset?"* (release needs a
/// drain PROOF); item 3: *"can a reader still be holding a binding to
/// it?"* (release needs an ACKNOWLEDGEMENT, and always resolves). An
/// offset must clear both, custody first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grace_and_the_dead_epoch_quarantine_are_independent_gates() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let reader = join(&owner, "r-both", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-quarantine").await;
    let offset = ba.allocate_block().await.expect("allocate");
    let dead = declare_dead_epoch("item-3 test: both gates on one offset");
    assert!(ba.quarantine_offset(offset, dead));

    // The quarantine is the FIRST gate: the free defers to it, not to
    // grace (custody proof before reader coherence).
    ba.free_block(offset).await.expect("free");
    assert!(ba.is_quarantined(offset));
    assert_eq!(ba.grace_len(), 0, "the quarantine gate came first");
    assert!(!free_listed(&ba, offset));

    // The drain proof hands it to the SECOND gate rather than to the free
    // list: a reader can still be holding a binding to it.
    assert_eq!(ba.release_quarantine(dead), 1);
    assert!(!ba.is_quarantined(offset));
    assert_eq!(ba.grace_len(), 1, "the proof releases INTO grace");
    assert!(!free_listed(&ba, offset));

    let label = ba.grace_oldest_label().expect("label");
    ack(&owner, "r-both", reader.epoch, label);
    assert_eq!(
        ba.allocate_block().await.expect("allocate"),
        offset,
        "clearing both gates makes the offset reallocatable"
    );
}

// ---------------------------------------------------------------------------
// 8 — the ring is bounded, and at cap it FENCES rather than leaking
// ---------------------------------------------------------------------------

/// RAM is bounded by the derived cap. At cap the ring forces progress
/// through the SAME fence act the deadline uses — never a silent early
/// release (that would break the coherence promise), never unbounded
/// growth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ring_cap_forces_progress_through_the_fence() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    let cycle = free_grace::ack_cycle(owner.clocks());
    free_grace::arm_owner_plane_with(clock.clone(), cycle * 10, cycle * 5);
    let _reader = join(&owner, "r-cap", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    // A three-entry ring, two entries in: below the cap and far inside
    // both deadlines, so a routine harvest must release nothing.
    let ring = GraceRing::new(3);
    assert!(ring.defer(0, 4096));
    assert!(ring.defer(4096, 4096));
    assert_eq!(ring.len(), 2);
    assert!(
        ring.harvest(64).is_empty(),
        "nothing is acknowledged, so a routine harvest releases nothing"
    );

    assert!(ring.defer(8192, 4096));
    // AT cap, still far inside both deadlines: the only honest way forward
    // is the fence, so the fence happens and the entries release.
    let released = ring.harvest(64);
    assert!(
        !released.is_empty(),
        "an at-cap ring must force progress, not grow"
    );
    assert!(ring.len() < 3, "the cap holds: {} entries", ring.len());
    assert_eq!(
        free_grace::laggard_fences(),
        1,
        "progress came from a fence"
    );
    assert!(free_grace::forced_releases() >= released.len() as u64);
}

// ---------------------------------------------------------------------------
// 9 — where in the reader's cycle the acknowledgement is emitted
// ---------------------------------------------------------------------------

/// The ack means *"I have FINISHED using anything freed at or before this
/// label"*, not *"I saw it"*. Three conditions, each a published number:
///
/// * an **epoch step** (a revalidation pass that advanced, i.e. one that
///   ran the R-6 purge) — an inert poll purges nothing, so it proves
///   nothing about cached bytes keyed by a reused bare offset;
/// * whose pass **began** at least the qualify window after the label was
///   learned, so the writer's dereference is durably checkpointed and
///   therefore in the record the pass adopts;
/// * plus the **drain**: the daemon layout cache may serve no pre-step
///   binding and every pre-step serve must have finished.
///
/// This contract drives the ladder with the L1-era numbers (qualify 2,000,
/// drain 4,000, timer only) — the gates' MECHANICS; their derivations are
/// the 2026-09-06 re-derivation's (contracts 32–38): qualify = the writer's
/// checkpoint landing ceiling + skew, drain = an epoch-step invalidation
/// plus an observed in-flight drain.
#[test]
fn a_reader_acknowledges_only_after_the_purge_and_the_drain() {
    let _serial = serial();
    let ladder = ReaderAckLadder::new();
    let inputs = |pass_start: u64, now: u64, advanced: bool| AckInputs {
        label: 5_000,
        learned_at_ms: 1_000,
        pass_start_ms: pass_start,
        now_ms: now,
        advanced,
        qualify_lag_ms: 2_000,
        drain_lag_ms: 4_000,
        refresh_floor_ms: 1_000,
        drain_gen: 0,
        drain_budget_ms: 2_000,
    };

    // Too early: the dereference need not be checkpointed yet.
    assert_eq!(ladder.note_pass(inputs(2_500, 2_600, true)), None);
    // Late enough, but INERT — no epoch step, so no purge ran.
    assert_eq!(ladder.note_pass(inputs(3_500, 3_600, false)), None);
    // Qualifying pass: the purge ran, but the drain has not elapsed.
    assert_eq!(ladder.note_pass(inputs(3_500, 3_600, true)), None);
    assert_eq!(ladder.note_pass(inputs(4_600, 5_000, false)), None);
    // Drain elapsed (3_600 + 4_000): the ack is emitted, once.
    assert_eq!(ladder.note_pass(inputs(7_000, 7_600, false)), Some(5_000));
    assert_eq!(ladder.note_pass(inputs(8_000, 8_600, false)), None);
    assert_eq!(ladder.acked(), 5_000);
}

/// The reader half wired end to end: the label is learned from the grant
/// (a causal token the OWNER minted — never a foreign clock read as a
/// deadline), the ladder promotes it, `ack_free_epoch` carries it on the
/// next renewal, and the writer's bound advances.
#[test]
fn the_reader_ack_rides_the_renewal_and_advances_the_writers_bound() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-wire", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let anchor = clock.now_ms();
    let session = Arc::new(MemberSession::adopt(
        "r-wire",
        MemberRole::Reader,
        &grant,
        anchor,
        clock.clone(),
    ));
    membership::install_member(Arc::clone(&session));
    let (label, learned_at) = session.learned_label();
    assert_eq!(
        label, grant.granted_at_owner_ms,
        "the label is the owner's own instant of the grant"
    );
    assert_eq!(learned_at, anchor);

    // A pass that is too early proves nothing.
    assert_eq!(free_grace::reader_pass_completed(anchor, true), None);

    // Qualify, then drain: both windows are the published derived numbers.
    let staleness = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
    let qualify = staleness + session.skew_max_ms();
    let drain = staleness + session.d_purge_ms();
    ticks.fetch_add(qualify + 1, Ordering::SeqCst);
    let pass_start = clock.now_ms();
    assert_eq!(
        free_grace::reader_pass_completed(pass_start, true),
        None,
        "the drain window has not elapsed"
    );
    ticks.fetch_add(drain + 1, Ordering::SeqCst);
    assert_eq!(
        free_grace::reader_pass_completed(clock.now_ms(), false),
        Some(label),
        "the ack is emitted after the purge and the drain"
    );
    assert_eq!(session.acked_free_epoch(), label);
    assert_eq!(free_grace::reader_acks(), 1);

    // The value rides the beat, and the writer's bound follows.
    assert!(matches!(
        owner.renew("r-wire", grant.epoch, session.acked_free_epoch()),
        RenewOutcome::Renewed(_)
    ));
    owner.refresh_free_grace_bound();
    assert_eq!(free_grace::bound(), label);

    // A READER's own stats face. This one process is standing in for two
    // mounts, so the writer's plane has to go before the reader's posture
    // can be read — in production a mount is an owner or a member, never
    // both, and the snapshot deliberately answers the writer's question
    // first when (only in a test) both exist.
    free_grace::disarm_owner_plane();
    let reader_stats = free_grace::stats_snapshot();
    assert_eq!(reader_stats["free_grace_mode"], "reader");
    assert_eq!(reader_stats["free_grace_acked_label"], label);
    assert_eq!(reader_stats["free_grace_learned_label"], label);
    assert_eq!(
        reader_stats["free_grace_reader_pending_label"], 0,
        "nothing is pending once the acknowledgement has been emitted"
    );
}

// ---------------------------------------------------------------------------
// 10 — both derivations, drift-is-red
// ---------------------------------------------------------------------------

/// The grace bound is DERIVED from the plane's own numbers, not tuned:
/// one honest acknowledgement cycle is
/// `3 × renew_interval + 3 × staleness_bound + skew_max + D_purge`
/// (learn the label → qualify → wait for a pass → drain → carry the ack
/// home → the owner republishes the bound), and the routine bound is two
/// of them (one missed cycle tolerated — S6's three-attempts renewal
/// discipline). A configured bound below ONE cycle refuses: it would
/// fence a reader that is behaving exactly as designed.
#[test]
fn the_grace_bound_is_derived_and_an_unsafe_override_refuses() {
    let _serial = serial();
    let clocks = shipped_clocks();
    let staleness = squeezefs::ro_coherence::reader_staleness_bound();
    let expected = clocks.renew_interval * 3 + staleness * 3 + clocks.skew_max + clocks.d_purge;
    assert_eq!(free_grace::ack_cycle(&clocks), expected);

    let cycle = free_grace::ack_cycle(&clocks);
    assert_eq!(
        free_grace::resolve_fence_bound_from(None, &clocks).expect("the derived default is safe"),
        cycle * 2
    );
    // Explicit wins verbatim at or above one cycle (in whole ms — the
    // knob's own unit).
    let ok_ms = cycle.as_millis() as u64 + 1;
    assert_eq!(
        free_grace::resolve_fence_bound_from(Some(ok_ms), &clocks).expect("safe"),
        Duration::from_millis(ok_ms)
    );
    // ...and REFUSES below it, naming the numbers.
    let err = free_grace::resolve_fence_bound_from(Some(cycle.as_millis() as u64 / 2), &clocks)
        .expect_err("a bound below one ack cycle must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("SQUEEZEFS_FREE_GRACE_MAX_MS") && msg.contains("acknowledgement cycle"),
        "the refusal must name the knob and the reason: {msg}"
    );
}

/// The ring cap rides the R5 budget (never a free-floating constant), with
/// a floor derived from the field's own saturated ingest rate: 12.7 GB/s
/// (`.benchmarks/2026-07-28-ingest-economy.md`) over one default ack cycle
/// displaces ≈ 120 k 4 MiB blocks, so the shipped floor never fences a
/// healthy reader merely because the writer is fast.
#[test]
fn the_ring_cap_is_derived_from_the_memory_budget_with_a_field_floor() {
    const GIB: u64 = 1024 * 1024 * 1024;
    const FIELD_BUDGET: u64 = 176 * GIB;
    const FLOOR_BUDGET: u64 = 2 * GIB + 820 * 1024 * 1024;

    // The field shape: 176 GiB / 1024 / 24 B per entry.
    assert_eq!(
        free_grace::resolve_ring_cap(FIELD_BUDGET),
        (FIELD_BUDGET / 1024 / free_grace::GRACE_ENTRY_BYTES) as usize
    );
    // The floor box: the derivation lands below the field floor, so the
    // floor holds (a physical/measured minimum, never tuning).
    assert_eq!(free_grace::resolve_ring_cap(FLOOR_BUDGET), 131_072);
    assert_eq!(free_grace::resolve_ring_cap(0), 131_072);
}

// ---------------------------------------------------------------------------
// 11 — honest accounting while an offset is held
// ---------------------------------------------------------------------------

/// A graced offset is neither free nor owned: it reads as **used** (which
/// is honest — it is genuinely unavailable), and no reconciliation path
/// steals it back onto the free list behind the ring's back (that would
/// double-publish it at the next harvest).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graced_offset_reads_as_used_and_survives_reconciliation() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let _reader = join(&owner, "r-account", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-accounting").await;
    let a = ba.allocate_block().await.expect("allocate");
    let b = ba.allocate_block().await.expect("allocate");
    ba.free_block(a).await.expect("free");
    assert_eq!(
        ba.get_used_blocks(),
        2,
        "a held offset is unavailable, so it counts as used"
    );

    let (completed, evictions) = ba.fsck_reconcile_accounting();
    assert_eq!(
        completed, 0,
        "reconciliation must not complete a graced free (it would double-publish)"
    );
    assert_eq!(evictions, 0);
    assert!(!free_listed(&ba, a));
    assert_eq!(ba.grace_len(), 1);
    let _ = b;
}

// ---------------------------------------------------------------------------
// 12 — concurrency: readers acknowledging while the writer churns
// ---------------------------------------------------------------------------

/// The shape the mechanism actually lives in: a writer freeing and
/// reallocating in a loop while readers acknowledge on their beats. No
/// offset may be handed out while it is still in grace, no double free may
/// occur, and the ledger must close (`deferrals == releases + held`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_acknowledge_while_the_writer_frees_and_reallocates() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let a = join(&owner, "r-1", MemberRole::Reader);
    let b = join(&owner, "r-2", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-concurrent").await;
    let doubles0 = METRICS.block_double_frees.load(Ordering::Relaxed);
    let stop = Arc::new(AtomicBool::new(false));

    // The readers: acknowledge the owner's current instant on every beat
    // (a reader that keeps up, which is the healthy shape).
    let acker = {
        let owner = Arc::clone(&owner);
        let clock = clock.clone();
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let label = clock.now_ms();
                let _ = owner.renew("r-1", a.epoch, label);
                let _ = owner.renew("r-2", b.epoch, label);
                owner.refresh_free_grace_bound();
                tokio::task::yield_now().await;
            }
        })
    };

    // The writer: allocate → free → allocate, checking the live invariant
    // on every hand-out.
    let mut live: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for i in 0..400u64 {
        let offset = ba.allocate_block().await.expect("allocate");
        assert!(
            !ba.grace_holds(offset),
            "offset {offset} was handed out while still in grace"
        );
        assert!(
            live.insert(offset),
            "offset {offset} was handed to two live owners"
        );
        // Free a previously held offset (not the fresh one) so the ring
        // genuinely overlaps with allocation.
        if i % 2 == 1 {
            let victim = *live.iter().next().expect("a live offset");
            live.remove(&victim);
            ba.free_block(victim).await.expect("free");
        }
        // Advance the owner clock so labels and acknowledgements interleave.
        ticks.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
    }
    stop.store(true, Ordering::Release);
    acker.await.expect("the acking task");

    // Drain: acknowledge the current instant, then let the allocator
    // harvest.
    let label = clock.now_ms() + 1;
    ack(&owner, "r-1", a.epoch, label);
    ack(&owner, "r-2", b.epoch, label);
    for _ in 0..8 {
        let _ = ba.allocate_block().await.expect("allocate");
    }
    assert_eq!(
        free_grace::deferrals(),
        free_grace::releases() + free_grace::held_offsets(),
        "the ledger must close: every deferral is released or still held"
    );
    assert_eq!(
        free_grace::forced_releases(),
        0,
        "readers that keep up are never fenced"
    );
    assert_eq!(free_grace::laggard_fences(), 0);
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles0,
        "no offset was published twice"
    );
}

// ---------------------------------------------------------------------------
// 13–18 — the pressure-coupled release valve (rung-20 residual 6)
// ---------------------------------------------------------------------------

/// One rewrite storm's OTHER half, driven deterministically on the manual
/// clock: the reader and the owner's own cadences, modelled exactly as
/// production runs them.
///
/// * The beat is `Grant::renew_ms` — literally what `MemberSession::
///   renew_at_ms` (and hence `spawn_member_renewal`'s sleep) is computed
///   from, which is what makes rung (a) deliverable at all.
/// * A member can only acknowledge a label it LEARNED (labels arrive on
///   beats, nowhere else), and only once its ladder's qualification and
///   drain windows have elapsed since it learned it — `2 × staleness +
///   skew_max + D_purge`, the two `AckInputs` lags added.
/// * The owner republishes the reallocation bound on its SWEEP cadence,
///   never per renewal (`refresh_free_grace_bound`'s own doc comment).
///   Rung (a)'s second half is what shortens that term under pressure,
///   and it is the daemon's own code doing it — not this harness.
struct Storm {
    owner: Arc<MembershipOwner>,
    clock: LeaseClock,
    ticks: Arc<AtomicU64>,
    id: &'static str,
    epoch: u64,
    /// `false` ⇒ the member beats but never advances its acknowledgement
    /// (the operator page's "renewing but not acknowledging" laggard).
    acknowledges: bool,
    /// `(instant learned, label)` per beat, oldest first.
    learned: std::collections::VecDeque<(u64, u64)>,
    ladder_lag_ms: u64,
    beat_at: u64,
    cadence: u64,
    /// The shortest cadence the owner ever granted — rung (a)'s engagement
    /// as the MEMBER experienced it.
    min_cadence: u64,
    beats: u64,
    /// `true` once the owner has evicted this member (rung (c)): a fenced
    /// member's next beat is refused, and in production it self-fences and
    /// re-joins rather than continuing.
    fenced: bool,
    sweep_at: u64,
    sweep_ms: u64,
}

impl Storm {
    fn new(
        owner: &Arc<MembershipOwner>,
        clock: &LeaseClock,
        ticks: &Arc<AtomicU64>,
        id: &'static str,
        grant: &Grant,
        acknowledges: bool,
    ) -> Self {
        let clocks = owner.clocks();
        let staleness = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
        Self {
            owner: Arc::clone(owner),
            clock: clock.clone(),
            ticks: Arc::clone(ticks),
            id,
            epoch: grant.epoch,
            acknowledges,
            learned: std::collections::VecDeque::new(),
            // qualify (`staleness + skew_max`) + drain (`staleness +
            // D_purge`) — the ladder's fixed cost, which no cadence can
            // shrink and which rung (a)'s floor exists to respect.
            ladder_lag_ms: staleness * 2
                + clocks.skew_max.as_millis() as u64
                + clocks.d_purge.as_millis() as u64,
            beat_at: clock.now_ms(),
            cadence: grant.renew_ms,
            min_cadence: grant.renew_ms,
            beats: 0,
            fenced: false,
            sweep_at: clock.now_ms() + clocks.renew_interval.as_millis() as u64,
            sweep_ms: clocks.renew_interval.as_millis() as u64,
        }
    }

    /// The label this member can honestly carry now: the newest one it
    /// learned at least a whole ladder cycle ago.
    fn carriable(&mut self, now: u64) -> u64 {
        if !self.acknowledges {
            return 0;
        }
        let mut best = 0;
        while let Some(&(at, label)) = self.learned.front() {
            if at + self.ladder_lag_ms > now {
                break;
            }
            best = label;
            self.learned.pop_front();
        }
        if best != 0 {
            // Keep it available: a later beat with nothing newer ready
            // re-presents the same value, exactly as a monotone ack does.
            self.learned.push_front((0, best));
        }
        best
    }

    /// Advance the storm's clock by `step_ms` and run every cadence that
    /// falls due.
    fn advance(&mut self, step_ms: u64) {
        self.ticks.fetch_add(step_ms, Ordering::SeqCst);
        let now = self.clock.now_ms();
        while !self.fenced && self.clock.now_ms() >= self.beat_at {
            let carried = self.carriable(self.clock.now_ms());
            let grant = match self.owner.renew(self.id, self.epoch, carried) {
                RenewOutcome::Renewed(g) => g,
                // Rung (c) landed on this member: in production it
                // self-fences and re-joins; here the storm simply stops
                // hearing from it, which is what the writer sees.
                RenewOutcome::UnknownLease { .. } => {
                    self.fenced = true;
                    break;
                }
            };
            let at = self.clock.now_ms();
            self.learned.push_back((at, grant.granted_at_owner_ms));
            self.cadence = grant.renew_ms.max(1);
            self.min_cadence = self.min_cadence.min(self.cadence);
            self.beat_at = at + self.cadence;
            self.beats += 1;
        }
        if now >= self.sweep_at {
            self.owner.refresh_free_grace_bound();
            self.sweep_at = now + self.sweep_ms;
        }
    }
}

/// **Contract 13 — the convicted shape** (rung-20 residual 6, first live
/// capture 2026-08-19): a rewrite storm defers faster than a reader
/// acknowledges on its natural 10 s beat, so `free_grace_offsets` climbs
/// monotonically and the store's own supply runs out underneath it.
///
/// The fix is a graded ladder, not a bigger ring: the pressure signal (the
/// ring's measured deferral rate against the SMALLER of its headroom and
/// the volume's free supply) engages rung (a) — the writer asks the
/// members it is waiting on to come back sooner, on a cadence derived by
/// inverting the plane's own acknowledgement cycle against the measured
/// runway — and rung (b) — the fence deadline slides from its routine
/// value toward the pressure floor. Rungs (a)+(b) must make rung (c) (a
/// forced release + the reader's eviction) and `free_grace_alloc_stalls`
/// UNREACHABLE on this shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rewrite_storm_prods_readers_instead_of_stalling_allocation() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-storm", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    // A 32-block store rewritten through a 4-block working set: every pass
    // mints one block and terminally frees one, which is the CoW rewrite
    // shape whose displacement stream fills the ring. Scaled down from the
    // field's 32 GiB lane; what the signal reads is the RATIO — supply
    // against the measured displacement rate — and this one is sized so
    // the store outlives the ladder's own fixed cost (≈ 6 s of
    // qualification + drain) but NOT the routine cadence's (≈ 26 s of
    // beat + sweep + ladder), which is exactly the field's shape.
    const STORE_BLOCKS: u64 = 32;
    const WORKING_SET: usize = 4;
    const PASSES: usize = 300;
    const STEP_MS: u64 = 1_000;

    let ba = allocator("grace-storm").await;
    ba.set_capacity_bytes(STORE_BLOCKS * ba.chunk_size());
    let mut storm = Storm::new(&owner, &clock, &ticks, "r-storm", &grant, true);

    let mut live: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    for pass in 0..PASSES {
        let fresh = ba.allocate_block().await.unwrap_or_else(|e| {
            panic!(
                "pass {pass}: the storm stalled ({e}) with {} offset(s) held in grace and \
                 {} stall(s) counted — the pressure ladder must reach the reader before the \
                 supply does (rung-20 residual 6)",
                ba.grace_len(),
                free_grace::alloc_stalls(),
            )
        });
        live.push_back(fresh);
        if live.len() > WORKING_SET {
            let victim = live.pop_front().expect("a live block");
            ba.free_block(victim).await.expect("free");
        }
        storm.advance(STEP_MS);
    }

    // Rung (a) and rung (b) engaged...
    assert!(
        free_grace::prods() > 0,
        "the writer never asked the reader to come back sooner (rung a)"
    );
    assert!(
        free_grace::bound_tightenings() > 0,
        "the fence deadline never tightened under pressure (rung b)"
    );
    assert!(
        storm.min_cadence < grant.renew_ms,
        "the prodded beat ({} ms) must be shorter than the routine one ({} ms)",
        storm.min_cadence,
        grant.renew_ms
    );
    // ...so that rung (c) and the ENOSPC ruling are never reached.
    assert_eq!(
        free_grace::alloc_stalls(),
        0,
        "allocation stalled on space the readers owed back"
    );
    assert_eq!(
        free_grace::forced_releases(),
        0,
        "a release without an acknowledgement is rung (c): the ladder must not have needed it"
    );
    assert_eq!(free_grace::laggard_fences(), 0, "nobody was fenced");
    assert!(
        owner.epoch_of("r-storm").is_some(),
        "a reader that keeps up must survive the storm"
    );

    // The ring stayed bounded far below its cap (the climb is what the
    // field convicted), and the ledger closes.
    assert!(
        ba.grace_len() < STORE_BLOCKS as usize,
        "the grace ring held {} offset(s) on a {STORE_BLOCKS}-block store — it is climbing, \
         not oscillating",
        ba.grace_len()
    );
    assert_eq!(
        free_grace::deferrals(),
        free_grace::releases() + free_grace::held_offsets(),
        "the closure law must hold across the storm"
    );
    assert!(
        free_grace::deferrals() >= PASSES as u64 - WORKING_SET as u64,
        "the storm must actually have exercised the ring"
    );
}

/// **Finding 18 — an expired prod DECAYS toward routine; it never snaps**
/// (`.benchmarks/2026-08-25-s11-freeloop-stall.md` §Finding 18): under a
/// real storm the runway reading sawtooths — every release crest lets the
/// prod's reading lapse, and pre-fix the very next renewal beat drew the
/// ROUTINE cadence. One routine grant = one routine-beat acknowledgement
/// hole, and the owner's min-composition inherits the widest member's
/// hole: the f16a row measured `bound_age` 15.5–22.6 s against PR 5's
/// ≤ 12 s gate with the member ladder itself ON budget (6.3 s). The law:
/// while the plane still HOLDS offsets, an expired ask relaxes one
/// doubling step per reading-TTL window (the write-pipeline probe
/// governor's bleed-to-routine pattern — derived, never a knob) instead
/// of lapsing, re-tightens fully on the next pressure reading, and
/// retires only at routine or when the ring drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_prod_decays_toward_routine_while_offsets_are_held() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-decay", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let routine = grant.renew_ms;

    let ba = allocator("grace-decay").await;
    ba.set_capacity_bytes(32 * ba.chunk_size());
    let mut storm = Storm::new(&owner, &clock, &ticks, "r-decay", &grant, true);

    // Phase 1 — storm until rung (a) is in force (the ask at a tightened
    // cadence). Bounded loud.
    let mut live: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    let mut tightened = 0u64;
    for pass in 0..120 {
        let fresh = ba
            .allocate_block()
            .await
            .unwrap_or_else(|e| panic!("pass {pass}: the storm stalled ({e})"));
        live.push_back(fresh);
        if live.len() > 4 {
            let victim = live.pop_front().expect("a live block");
            ba.free_block(victim).await.expect("free");
        }
        storm.advance(1_000);
        tightened = free_grace::prod_renew_ms();
        if tightened > 0 {
            break;
        }
    }
    assert!(
        tightened > 0 && tightened < routine,
        "the storm must arm rung (a) with a tightened cadence \
         (got {tightened} vs routine {routine})"
    );

    assert!(
        tightened < routine / 2,
        "premise: the tightened ask ({tightened} ms) must sit below \
         routine/2 ({} ms) or the ladder cannot be discriminated",
        routine / 2
    );
    assert!(
        free_grace::held_offsets() > 0,
        "the storm must leave offsets held — otherwise the decay law has \
         nothing to protect"
    );

    // Phase 2 — the release CREST: no ring traffic, no readings, no
    // member beats. Probe the DELIVERY POINT once per second exactly as
    // the owner's renewal path would for a member that is behind, and
    // record the asked cadences until the ask retires (None).
    let mut seq: Vec<u64> = Vec::new();
    for _ in 0..240 {
        ticks.fetch_add(1_000, Ordering::SeqCst);
        match free_grace::take_prod_cadence(0) {
            Some(c) => seq.push(c),
            None => break,
        }
    }
    // THE CONTRACT (pre-fix RED): while offsets are held, the ask RELAXES
    // window by window — it never lapses straight from the tightened
    // cadence to the routine hole. Pre-fix the reading expires once and
    // every later beat draws routine (the f16a row's 15.5–22.6 s
    // bound_age against the ≤ 12 s gate, with the member ladder itself on
    // budget); post-fix the recorded sequence walks its doublings to at
    // least routine/2 before retiring at routine.
    assert!(
        free_grace::held_offsets() > 0,
        "the crest phase must not have drained the ring"
    );
    let last = *seq.last().unwrap_or(&0);
    assert!(
        last >= routine / 2,
        "finding 18: the ask LAPSED at {last} ms (tightened {tightened}, \
         routine {routine}) while {} offset(s) were still held — the very \
         next renewal beat draws the ROUTINE cadence and the \
         min-composition inherits the hole; the decay ladder must walk to \
         at least routine/2 before retiring",
        free_grace::held_offsets()
    );
    let mut prev = 0u64;
    for &c in &seq {
        assert!(
            c >= prev && c < routine,
            "the decay ladder must relax monotonically below routine \
             ({prev} → {c} vs {routine})"
        );
        prev = c;
    }
    assert!(
        free_grace::prod_decays() > 0,
        "the decay's engagement gauge must count the relax steps"
    );

    // Integrity: the decay bought promptness without touching the fence,
    // and the ladder RETIRED (the loop broke on None) — a permanent
    // elevated ask on a quiet plane is the inverted economy.
    assert!(
        seq.len() < 240,
        "the ladder must retire at routine, never ask for ever"
    );
    assert_eq!(free_grace::forced_releases(), 0);
    assert_eq!(free_grace::laggard_fences(), 0);
    assert_eq!(
        free_grace::deferrals(),
        free_grace::releases() + free_grace::held_offsets(),
        "the closure law holds across the decay"
    );
}

/// **Finding 18's economy half — a drained ring retires the ask at once.**
/// The decay law exists for a plane that HOLDS offsets; a ring that has
/// drained to zero asks nothing (prodding a member that is not holding
/// the free list buys nothing and costs it beats — the shipped law,
/// unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drained_ring_retires_the_expired_ask_immediately() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-drained", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-drained").await;
    ba.set_capacity_bytes(32 * ba.chunk_size());
    let mut storm = Storm::new(&owner, &clock, &ticks, "r-drained", &grant, true);

    // Storm until the ask is in force, then DRAIN: the member
    // acknowledges everything (one renewal carrying a max label) and the
    // harvest releases the ring to zero.
    let mut live: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    for pass in 0..120 {
        let fresh = ba
            .allocate_block()
            .await
            .unwrap_or_else(|e| panic!("pass {pass}: the storm stalled ({e})"));
        live.push_back(fresh);
        if live.len() > 4 {
            let victim = live.pop_front().expect("a live block");
            ba.free_block(victim).await.expect("free");
        }
        storm.advance(1_000);
        if free_grace::prod_renew_ms() > 0 {
            break;
        }
    }
    assert!(
        free_grace::prod_renew_ms() > 0,
        "the storm must arm the ask"
    );
    // A label past everything issued (labels are owner-clock ms — never
    // `u64::MAX`, which is `min_acked_free_epoch`'s no-members sentinel).
    ack(&owner, "r-drained", storm.epoch, clock.now_ms() + 1_000_000);
    // Allocation-head harvests release the fully-acknowledged ring (the
    // production drain path — no test-only hook). Bounded poll: the tail
    // frees ride the background reclaim worker into the ring, so the
    // drain needs a real-time beat or two to converge.
    for _ in 0..400 {
        if free_grace::held_offsets() == 0 {
            break;
        }
        // A capacity refusal here is fine — the head harvest still ran,
        // and the next beat retries once the background reclaim worker
        // lands the tail frees into the ring (its batch cadence is real
        // milliseconds, which is what this poll waits out).
        if let Ok(fresh) = ba.allocate_block().await {
            live.push_back(fresh);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        free_grace::held_offsets(),
        0,
        "the full acknowledgement must drain the ring"
    );

    // Quiet crest on the DRAINED ring: the delivery point retires the ask
    // (bounded — one reading window at most) and the decay gauge never
    // moves. No ask on a plane holding nothing.
    let decays0 = free_grace::prod_decays();
    let mut retired = false;
    for _ in 0..240 {
        ticks.fetch_add(1_000, Ordering::SeqCst);
        if free_grace::take_prod_cadence(0).is_none() {
            retired = true;
            break;
        }
    }
    assert!(
        retired,
        "a drained ring must retire the ask, never decay it"
    );
    assert_eq!(
        free_grace::prod_decays(),
        decays0,
        "no decay step on a drained ring"
    );
}

/// **Contract 14 — rung (c) is intact.** The valve buys promptness, never
/// a broken promise: a member that keeps beating but never acknowledges is
/// prodded first (rung a), refused ENOSPC rather than served an offset it
/// may still resolve (the ruling), and only then FENCED — together with
/// its eviction, past a deadline that is at minimum one honest
/// acknowledgement cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_that_never_acknowledges_is_prodded_then_fenced() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-mute-storm", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-storm-mute").await;
    ba.set_capacity_bytes(32 * ba.chunk_size());
    let mut storm = Storm::new(&owner, &clock, &ticks, "r-mute-storm", &grant, false);

    // Run the storm past the store's supply. Every allocation that refuses
    // is the RULING working (counted, never a silent early release), and
    // the member keeps beating throughout — it is alive, it just never
    // answers.
    let evictions0 = METRICS.membership_evictions.load(Ordering::Relaxed);
    let cycle_ms = free_grace::ack_cycle(owner.clocks()).as_millis() as u64;
    let armed_at = clock.now_ms();
    let mut live: Vec<u64> = Vec::new();
    let mut refusals = 0u64;
    let mut prods_before_fence = 0u64;
    for _ in 0..200 {
        match ba.allocate_block().await {
            Ok(fresh) => live.push(fresh),
            Err(e) => {
                assert!(
                    matches!(&e, SqueezefsError::Io(io)
                        if io.kind() == std::io::ErrorKind::StorageFull),
                    "the verdict on a mute reader is StorageFull, got {e:?}"
                );
                refusals += 1;
            }
        }
        if live.len() > 4 {
            let victim = live.remove(0);
            ba.free_block(victim).await.expect("free");
        }
        // Everything below rung (c) must hold right up to the fence.
        if free_grace::laggard_fences() == 0 {
            prods_before_fence = free_grace::prods();
            assert_eq!(
                free_grace::forced_releases(),
                0,
                "nothing may be released unacknowledged before the deadline"
            );
        }
        storm.advance(1_000);
        if storm.fenced {
            break;
        }
    }

    assert!(
        refusals > 0,
        "a member that never acknowledges must eventually cost the writer ENOSPC — that is \
         the ruling, and it is what makes rung (c) necessary"
    );
    assert!(free_grace::alloc_stalls() > 0, "the stalls are counted");
    assert!(
        prods_before_fence > 0,
        "the ladder must ASK before it fences (rung a precedes rung c)"
    );
    assert!(
        storm.beats > 1,
        "the member kept beating; it just never acknowledged"
    );

    // Rung (c): the fence, WITH the eviction, and never inside one honest
    // acknowledgement cycle of the writer starting to wait.
    assert!(storm.fenced, "the mute member was never fenced");
    assert_eq!(
        free_grace::laggard_fences(),
        1,
        "one member was holding the free list, so exactly one is fenced"
    );
    assert!(free_grace::forced_releases() >= 1);
    assert!(
        METRICS.membership_evictions.load(Ordering::Relaxed) > evictions0,
        "a forced release must happen WITH that member's eviction, never without it"
    );
    assert!(owner.epoch_of("r-mute-storm").is_none());
    assert!(
        clock.now_ms().saturating_sub(armed_at) >= cycle_ms,
        "the fence landed inside one honest acknowledgement cycle"
    );

    // ...and the writer progresses.
    let _recovered = ba
        .allocate_block()
        .await
        .expect("the writer must progress once the laggard is fenced");
}

/// **Contract 15 — the published bound is the bound in force.** A
/// tightening that only the machinery knows about is a promise an operator
/// cannot read, so `free_grace_fence_bound_ms` publishes the EFFECTIVE
/// deadline (the `write_pipeline_depth_target` / `..._base` precedent) and
/// the routine derivation stays visible beside it. The floor is one honest
/// acknowledgement cycle — computed from the READER's published staleness
/// bound, which the valve never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tightened_bound_is_published_and_never_crosses_the_ack_cycle() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let _reader = join(&owner, "r-honest", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let staleness_before = squeezefs::ro_coherence::reader_staleness_bound();
    let cycle = free_grace::ack_cycle(owner.clocks());
    let base = free_grace::fence_bound_base_ms();

    // Quiet: the effective bound IS the routine one.
    assert_eq!(free_grace::effective_bound_ms(), base);
    assert_eq!(
        free_grace::pressure_pct(),
        0,
        "a quiet plane reads no pressure"
    );

    // Under pressure: a tiny store, freed into grace.
    let ba = allocator("grace-honest").await;
    ba.set_capacity_bytes(6 * ba.chunk_size());
    let mut offs = Vec::new();
    for _ in 0..4 {
        offs.push(ba.allocate_block().await.expect("allocate"));
    }
    for (i, o) in offs.iter().enumerate() {
        ticks.fetch_add(10 * (i as u64 + 1), Ordering::SeqCst);
        ba.free_block(*o).await.expect("free");
    }

    let effective = free_grace::effective_bound_ms();
    assert!(
        effective < base,
        "the deadline did not tighten under pressure ({effective} vs base {base})"
    );
    assert!(
        effective >= cycle.as_millis() as u64,
        "the tightening crossed the floor of one honest acknowledgement cycle"
    );
    assert!(
        free_grace::pressure_pct() > 0,
        "the graded signal must read"
    );

    let stats = free_grace::stats_snapshot();
    assert_eq!(
        stats["free_grace_fence_bound_ms"], effective,
        "the published bound must be the one in force"
    );
    assert_eq!(stats["free_grace_fence_bound_base_ms"], base);
    assert_eq!(stats["free_grace_pressure_pct"], free_grace::pressure_pct());
    assert_eq!(stats["free_grace_prods"], free_grace::prods());
    assert_eq!(
        stats["free_grace_bound_tightenings"],
        free_grace::bound_tightenings()
    );
    assert_eq!(
        stats["free_grace_prod_renew_ms"],
        free_grace::prod_renew_ms()
    );
    assert!(
        stats["free_grace_fence_bound_ms"].as_u64()
            >= stats["free_grace_pressure_bound_ms"].as_u64(),
        "the pressure bound is the FLOOR of the tightening, never below it"
    );

    // The reader's own published guarantee is untouched by the valve: the
    // number the acknowledgement cycle derives from cannot drift from the
    // number the reader publishes.
    assert_eq!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        staleness_before,
        "the valve must never move the reader's published staleness bound"
    );
    assert_eq!(free_grace::ack_cycle(owner.clocks()), cycle);
}

/// **Contract 16 — unarmed engages no rung.** The shipped default is
/// `SQUEEZEFS_MEMBERSHIP_BIND=off`; the valve must add nothing to the free
/// path there — no reading, no prod, no counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unarmed_mount_engages_no_rung_of_the_pressure_ladder() {
    let _serial = serial();
    let ba = allocator("grace-valve-off").await;
    ba.set_capacity_bytes(8 * ba.chunk_size());
    for _ in 0..16 {
        let off = ba.allocate_block().await.expect("allocate");
        ba.free_block(off).await.expect("free");
    }

    assert_eq!(free_grace::prods(), 0);
    assert_eq!(free_grace::bound_tightenings(), 0);
    assert_eq!(free_grace::pressure_pct(), 0);
    assert_eq!(free_grace::prod_renew_ms(), 0, "no prod is ever in force");
    assert!(
        free_grace::take_prod_cadence(0).is_none(),
        "an unarmed plane must hand out no tightened cadence"
    );
    assert_eq!(free_grace::effective_bound_ms(), 0, "no plane, no deadline");
    let stats = free_grace::stats_snapshot();
    assert_eq!(stats["free_grace_mode"], "off");
    assert!(
        stats.get("free_grace_pressure_pct").is_none(),
        "an unarmed mount exports the posture word alone"
    );
}

/// **Contract 17 — every threshold derives** (drift-is-red). The signal is
/// the ring's OWN arithmetic: the deferral rate it can measure from its
/// two end labels, applied to the smaller of its headroom and the volume's
/// free supply. The response interpolates between two numbers the plane
/// already publishes, and the prodded cadence is the plane's own
/// acknowledgement cycle INVERTED against the runway.
#[test]
fn the_pressure_signal_and_its_ladder_are_derived() {
    let _serial = serial();

    // A rate needs two samples: one entry carries none, so it reads as no
    // pressure rather than as a cliff.
    assert_eq!(free_grace::runway_ms(0, 0, 1024, 100), None);
    assert_eq!(free_grace::runway_ms(1, 0, 1024, 100), None);
    // Four offsets over 1 s = 1 per 250 ms. The ring's headroom (1020) is
    // the larger supply, so the volume's 10 free blocks bind: 2.5 s.
    assert_eq!(free_grace::runway_ms(4, 1_000, 1024, 10), Some(2_500));
    // ...and with unbounded space the ring's own headroom binds.
    assert_eq!(
        free_grace::runway_ms(4, 1_000, 1024, u64::MAX),
        Some(1_020 * 250)
    );
    // A burst inside one millisecond is maximum pressure, not a division
    // by zero.
    assert_eq!(free_grace::runway_ms(8, 0, 8, 0), Some(0));

    // The graded deadline: no reading ⇒ the routine bound; at the cliff ⇒
    // the pressure floor; linear between, so there is no threshold cliff.
    let (fence, pressure) = (80_000u64, 40_000u64);
    assert_eq!(
        free_grace::effective_bound_ms_from(None, fence, pressure),
        fence
    );
    assert_eq!(
        free_grace::effective_bound_ms_from(Some(0), fence, pressure),
        pressure
    );
    assert_eq!(
        free_grace::effective_bound_ms_from(Some(fence), fence, pressure),
        fence
    );
    assert_eq!(
        free_grace::effective_bound_ms_from(Some(fence * 4), fence, pressure),
        fence,
        "a runway longer than the bound is not pressure"
    );
    assert_eq!(
        free_grace::effective_bound_ms_from(Some(fence / 2), fence, pressure),
        pressure + (fence - pressure) / 2
    );

    // The prodded cadence: `runway = 3 × beats + (the terms a faster beat
    // cannot shrink)`, solved for the beat, clamped into
    // [the shortest interval that can carry a NEW answer, the routine
    // cadence].
    let clocks = shipped_clocks();
    let prod = free_grace::ProdParams::derive(&clocks);
    let cycle = free_grace::ack_cycle(&clocks).as_millis() as u64;
    let renew = clocks.renew_interval.as_millis() as u64;
    let floor = free_grace::ack_refresh_floor(&clocks).as_millis() as u64;
    assert_eq!(
        floor,
        squeezefs::ro_coherence::reader_revalidate_interval()
            .max(clocks.skew_max)
            .as_millis() as u64,
        "the floor is the shortest interval at which the reader's answer can change"
    );
    assert_eq!(
        prod.cadence_for(cycle, floor),
        None,
        "a runway of one whole cycle needs no prod: the routine beat already fits"
    );
    assert_eq!(
        prod.cadence_for(u64::MAX, floor),
        None,
        "no runway pressure, no prod"
    );
    assert_eq!(
        prod.cadence_for(0, floor),
        Some(floor),
        "at the cliff the beat is the fastest one that can carry a new answer"
    );
    let mid = prod
        .cadence_for(cycle - renew, floor)
        .expect("a mid-pressure prod");
    assert!(
        (floor..renew).contains(&mid),
        "the prodded cadence {mid} must sit between the floor {floor} and the routine {renew}"
    );
    assert!(
        prod.cadence_for(cycle - renew * 2, floor)
            .expect("more pressure")
            <= mid,
        "the cadence must tighten monotonically as the runway shortens"
    );
    // The composite (adjudication item 4): the floor FOLLOWS the writer's
    // advertised checkpoint ceiling — `max(min(P, ceiling), skew)` — so
    // an owner asking under a halved ceiling asks at the halved floor.
    assert_eq!(
        prod.cadence_for(0, prod.floor_for(500)),
        Some(500),
        "at the cliff under a 500 ms writer ceiling the ask is 500 ms"
    );
}

/// **Contract 18 — a tightened cadence can never starve the ladder.**
///
/// The ladder's qualification is "a purging pass that BEGAN at least
/// `staleness + skew_max` after the label was learned", and every renewal
/// re-learns a fresher label. Qualify against whatever the newest renewal
/// carries and a beat faster than that lag refreshes the target out from
/// under every pass — the reader then acknowledges NOTHING, for ever,
/// which is precisely the "climbs and never drains" signature. So the
/// ladder must snapshot a CANDIDATE and qualify that.
///
/// (This is not only the valve's problem: with the shipped clocks the
/// routine beat is 10 s against a 2.02 s lag, but a writer running
/// `SQUEEZEFS_META_FLUSH_INTERVAL_MS=5000` derives a 6.02 s lag plus a 5 s
/// pass cadence against the same 10 s beat and closes the window on its
/// own.)
#[test]
fn a_tightened_renewal_cadence_never_starves_the_ack_ladder() {
    let _serial = serial();
    let ladder = ReaderAckLadder::new();

    // A prodded 500 ms beat against a 2 s qualification lag, with the
    // reader polling every 250 ms.
    const BEAT_MS: u64 = 500;
    const POLL_MS: u64 = 250;
    const QUALIFY_LAG_MS: u64 = 2_000;
    const DRAIN_LAG_MS: u64 = 4_000;

    let mut acked = Vec::new();
    let mut learned_at = 0u64;
    let mut label = 1_000u64;
    for step in 1..=200u64 {
        let now = step * POLL_MS;
        if now.is_multiple_of(BEAT_MS) {
            // The beat re-learns a fresher label, exactly as
            // `MemberSession::renewed` does.
            learned_at = now;
            label = 1_000 + now;
        }
        if let Some(l) = ladder.note_pass(AckInputs {
            label,
            learned_at_ms: learned_at,
            pass_start_ms: now,
            now_ms: now,
            advanced: true,
            qualify_lag_ms: QUALIFY_LAG_MS,
            drain_lag_ms: DRAIN_LAG_MS,
            refresh_floor_ms: POLL_MS,
            drain_gen: 0,
            drain_budget_ms: 2_000,
        }) {
            acked.push(l);
        }
    }

    assert!(
        !acked.is_empty(),
        "the ladder acknowledged NOTHING across 50 s of beats: a cadence shorter than the \
         qualification lag starved it, so the writer's grace ring can only climb"
    );
    assert!(
        acked.windows(2).all(|w| w[1] > w[0]),
        "acknowledgements must be monotone: {acked:?}"
    );
    // Each promotion must still be an HONEST one: its label was learned at
    // least the qualification lag before the pass that adopted it, and the
    // drain window elapsed after that pass.
    assert!(
        acked.len() >= 4,
        "with a 500 ms beat the ladder should promote repeatedly, got {acked:?}"
    );
}

// ===========================================================================
// The free-grace sustain campaign, PR 1 (docs/design-free-grace-sustain.md
// §5.4/§8; finding 15 part 2): the attribution instruments + the GREEN
// pinned-current-behavior contracts the later PRs invert
// ===========================================================================

/// A shared inputs builder for the ladder contracts (PR 2's shape: the
/// refresh floor rides the inputs so the depth can derive).
fn ack_inputs(label: u64, learned: u64, pass: u64, now: u64, adv: bool) -> AckInputs {
    AckInputs {
        label,
        learned_at_ms: learned,
        pass_start_ms: pass,
        now_ms: now,
        advanced: adv,
        qualify_lag_ms: 2_000,
        drain_lag_ms: 4_000,
        refresh_floor_ms: 1_000,
        drain_gen: 0,
        drain_budget_ms: 2_000,
    }
}

/// **PR 2 (L1), the inversion of PR 1's pinned contract (i): the ladder
/// PIPELINES candidates, each on its own unchanged gates.** A fresher
/// label arriving mid-qualification is adopted as a SECOND candidate
/// (its own learned-at snapshot — the anti-starvation law per candidate,
/// verbatim), qualifies on its own condition-(2) pass and promotes after
/// its own drain window — CONCURRENTLY with the first, never serialized
/// behind its ack (the T6 quantization §3.2 derived, removed).
#[test]
fn ack_pipeline_advances_concurrent_candidates_each_on_its_own_gates() {
    let _serial = serial();
    free_grace::test_set_ack_pipeline(Some(true));
    let ladder = ReaderAckLadder::new();

    // Candidate A (label 5_000, learned 1_000) starts qualifying.
    assert_eq!(
        ladder.note_pass(ack_inputs(5_000, 1_000, 2_500, 2_600, true)),
        None
    );
    // A fresher label B (9_000, learned 3_200) arrives mid-cycle: ADOPTED
    // as a second candidate (depth 2), while A keeps its snapshot.
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 3_500, 3_600, true)),
        None
    );
    assert_eq!(
        ladder.depth(),
        2,
        "two candidates in flight — the pipeline is what PR 1's pin denied"
    );
    // B qualifies on ITS own gate (pass ≥ 3_200 + 2_000 = 5_200), while
    // A's drain is still running — B's ready_at becomes 5_400 + 4_000.
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 5_300, 5_400, true)),
        None
    );
    // A's drain elapses (3_600 + 4_000): A promotes first — monotone.
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 7_700, 7_800, false)),
        Some(5_000),
        "the oldest qualified candidate promotes first (label order)"
    );
    // B promotes when ITS drain elapses (9_400) — one qualify+drain after
    // ITS OWN learn instant, NOT a full serial cycle after A's ack (the
    // shipped ladder answers None here: B was never even adopted).
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 9_450, 9_500, false)),
        Some(9_000),
        "the pipelined candidate rides its OWN windows — the inverted pin"
    );
    assert_eq!(ladder.acked(), 9_000);
    assert_eq!(
        ladder.acked_lag_ms(),
        9_500 - 3_200,
        "the reader-lag gauge: member-clock promote instant − learned_at(acked)"
    );
    assert!(
        free_grace::test_clear_ack_pipeline(),
        "the seam was in force for this contract"
    );
}

/// **The depth-1 lever restores the shipped ladder VERBATIM**
/// (`SQUEEZEFS_FREE_GRACE_ACK_PIPELINE=0` — the A/B lever; the design's
/// bit-compatibility contract): PR 1's pinned schedule replays with the
/// pre-campaign outcomes exactly, and the depth gauge pins at ≤ 1.
#[test]
fn the_ack_pipeline_lever_off_is_the_shipped_ladder_verbatim() {
    let _serial = serial();
    free_grace::test_set_ack_pipeline(Some(false));
    let ladder = ReaderAckLadder::new();

    assert_eq!(
        ladder.note_pass(ack_inputs(5_000, 1_000, 2_500, 2_600, true)),
        None
    );
    // The snapshot law at depth 1: the fresher label is NOT adopted while
    // A is in flight.
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 3_500, 3_600, true)),
        None
    );
    assert!(ladder.depth() <= 1, "depth pinned at 1 under the lever");
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 3_200, 7_700, 7_800, false)),
        Some(5_000),
        "one candidate in flight: the cycle emits its snapshot label only"
    );
    // B pays a FULL second cycle from its own adoption — the shipped
    // quantization, restored exactly.
    assert_eq!(
        ladder.note_pass(ack_inputs(9_000, 7_800, 7_900, 8_000, true)),
        None,
        "the depth-1 lever: label B pays a full second cycle"
    );
    assert_eq!(ladder.acked(), 5_000);
    assert!(free_grace::test_clear_ack_pipeline());
}

/// **The never-early-ack law under arbitrary schedules** (the design's
/// proptest gate, 1,000 cases): whatever the learn/pass schedule, an ack
/// for label `L` is emitted only after (i) an ADVANCING pass whose start
/// was ≥ `L`'s learn instant + the qualify lag, and (ii) the drain window
/// elapsed since that pass — no label's gates ever move, pipelined or not.
#[test]
fn prop_an_ack_is_never_emitted_before_its_labels_own_gates() {
    use proptest::prelude::*;
    let _serial = serial();
    free_grace::test_set_ack_pipeline(Some(true));
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 1_000,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &proptest::collection::vec(
                // (learn a fresher label?, time step ms 1..4000, advanced?)
                (any::<bool>(), 1u64..4_000, any::<bool>()),
                1..40,
            ),
            |steps| {
                let ladder = ReaderAckLadder::new();
                let mut now = 1_000u64;
                let mut label = 0u64;
                let mut learned_at = 0u64;
                // Per-label learn instants + per-label earliest qualifying
                // ADVANCING pass (the oracle's evidence).
                let mut learn: std::collections::HashMap<u64, u64> = Default::default();
                let mut qualified_at: std::collections::HashMap<u64, u64> = Default::default();
                for (fresh, step, advanced) in steps {
                    now += step;
                    if fresh || label == 0 {
                        label = now; // labels are owner instants: monotone
                        learned_at = now;
                        learn.insert(label, learned_at);
                    }
                    let pass_start = now;
                    now += 50; // the pass takes 50 ms
                    if advanced {
                        // The oracle: this pass qualifies every learned
                        // label whose learn instant + qualify lag ≤ start.
                        for (l, at) in &learn {
                            if pass_start >= at + 2_000 {
                                qualified_at.entry(*l).or_insert(now + 4_000);
                            }
                        }
                    }
                    let out = ladder.note_pass(AckInputs {
                        label,
                        learned_at_ms: learned_at,
                        pass_start_ms: pass_start,
                        now_ms: now,
                        advanced,
                        qualify_lag_ms: 2_000,
                        drain_lag_ms: 4_000,
                        refresh_floor_ms: 1_000,
                        drain_gen: 0,
                        drain_budget_ms: 2_000,
                    });
                    if let Some(acked) = out {
                        let ready = qualified_at.get(&acked).ok_or_else(|| {
                            proptest::test_runner::TestCaseError::fail(format!(
                                "ack {acked} emitted with NO qualifying advancing pass"
                            ))
                        })?;
                        prop_assert!(
                            now >= *ready,
                            "ack {acked} emitted at {now} before its drain window {ready}"
                        );
                    }
                }
                Ok(())
            },
        )
        .unwrap();
    assert!(free_grace::test_clear_ack_pipeline());
}

/// **The bounded queue never wedges** (the design's second proptest law,
/// 1,000 cases): under an arbitrary label storm the depth never exceeds
/// the derived cap, a QUALIFIED candidate is never dropped, and once the
/// storm stops the ladder always makes progress (quiet advancing passes
/// promote an ack — the high-water drop rule cannot strand it).
#[test]
fn prop_the_candidate_queue_is_bounded_and_never_wedges() {
    use proptest::prelude::*;
    let _serial = serial();
    free_grace::test_set_ack_pipeline(Some(true));
    // cap = clamp(ceil((2000+4000)/1000)+2, 2, 16) = 8 for these lags.
    let cap = 8u64;
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 1_000,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &proptest::collection::vec((1u64..1_500, any::<bool>()), 1..60),
            |storm| {
                let ladder = ReaderAckLadder::new();
                let mut now = 1_000u64;
                for (step, advanced) in storm {
                    now += step;
                    // Every step learns a fresher label: the storm shape.
                    let out = ladder.note_pass(AckInputs {
                        label: now,
                        learned_at_ms: now,
                        pass_start_ms: now,
                        now_ms: now + 10,
                        advanced,
                        qualify_lag_ms: 2_000,
                        drain_lag_ms: 4_000,
                        refresh_floor_ms: 1_000,
                        drain_gen: 0,
                        drain_budget_ms: 2_000,
                    });
                    let _ = out;
                    now += 10;
                    prop_assert!(
                        ladder.depth() <= cap,
                        "depth {} exceeded the derived cap {cap}",
                        ladder.depth()
                    );
                }
                // The storm stops: quiet advancing passes must drain the
                // ladder to an ack (progress — the no-wedge half).
                let target = now;
                let mut promoted = false;
                for _ in 0..12 {
                    now += 3_000;
                    if ladder
                        .note_pass(AckInputs {
                            label: target,
                            learned_at_ms: target,
                            pass_start_ms: now,
                            now_ms: now + 10,
                            advanced: true,
                            qualify_lag_ms: 2_000,
                            drain_lag_ms: 4_000,
                            refresh_floor_ms: 1_000,
                            drain_gen: 0,
                            drain_budget_ms: 2_000,
                        })
                        .is_some()
                    {
                        promoted = true;
                    }
                    now += 10;
                }
                prop_assert!(promoted, "the ladder wedged: no ack after the storm");
                prop_assert!(ladder.acked() > 0);
                Ok(())
            },
        )
        .unwrap();
    assert!(free_grace::test_clear_ack_pipeline());
}

/// **PR 3 (L3), the inversion of PR 1's pinned contract (ii): a live
/// demand mark republishes the bound within the floor beat.** Without
/// demand the T7 quantization stands (a recorded ack waits for the sweep);
/// with the demand mark live, the very harvest that observes the coupling
/// runs the valve's existing rate-limited refresh (the gate widened from
/// "a prod cadence was computed" to "…or the demand mark is live"), so
/// recorded acks publish within one floor beat of arrival.
#[test]
fn a_live_demand_mark_republishes_the_bound_within_the_floor() {
    let _serial = serial();
    free_grace::test_set_demand(Some(true));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-sweep", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let before = free_grace::bound();

    // The coupled shape (the ENOSPC capture's): the ring holds offsets
    // aged past the physics floor while the lane-reachable supply is 0.
    let ring = GraceRing::new(1024);
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(200, Ordering::SeqCst);
    assert!(ring.defer(8 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(20_000, Ordering::SeqCst);

    // The bare renewal RECORDS the ack (no manual sweep anywhere below).
    assert!(
        matches!(
            owner.renew("r-sweep", grant.epoch, 14_000),
            RenewOutcome::Renewed(_)
        ),
        "the renewal that carries the acknowledgement is admitted"
    );
    assert_eq!(free_grace::bound(), before, "recorded, not yet published");

    // The harvest observes the coupling (site 0) and — the L3 inversion —
    // republishes the bound itself, within the floor's rate limit.
    let refreshes_before = free_grace::bound_refreshes();
    let released = ring.harvest_with_supply(64, u64::MAX, 0);
    assert!(
        free_grace::bound_refreshes() > refreshes_before,
        "the demand-coupled refresh ran on the harvest path (L3's gate)"
    );
    assert_eq!(
        free_grace::bound(),
        14_000,
        "the recorded ack published WITHOUT the owner's sweep — T7 \
         collapsed to the floor beat (PR 1's pin (ii), inverted)"
    );
    // …and the now-covered offsets released in the same pass or the next.
    let released2 = ring.harvest_with_supply(64, u64::MAX, 0);
    assert_eq!(
        released.len() + released2.len(),
        2,
        "the acknowledged offsets reach the free list without a sweep"
    );
    assert!(free_grace::test_clear_demand());
}

/// **PR 3 (L2 + L4), the inversion of PR 1's pinned contract (iii): the
/// coupled storm PRODS at the floor while the fence stays runway-only.**
/// The motivating row reached no refusal edge — long PASSED-global runway,
/// prods 0, pressure 0 — while the ring aged past the physics floor and
/// the lane-reachable supply sat at the trough (the ENOSPC capture's
/// shape). Site 0 observes the coupling (PR 1's counter), the demand mark
/// arms rung a′ (the floor cadence to members holding the free list) and
/// the coupling face reads the cliff — while rung (b)'s LAW is untouched:
/// the fence deadline stays the space runway's, verbatim.
#[test]
fn a_coupled_storm_prods_at_the_floor_and_the_fence_stays_runway_only() {
    let _serial = serial();
    free_grace::test_set_demand(Some(true));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-hold", MemberRole::Reader);
    let _ = grant;
    owner.refresh_free_grace_bound();

    // Two held offsets (the runway needs ≥ 2 samples), deferred now…
    let ring = GraceRing::new(1024);
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(200, Ordering::SeqCst);
    assert!(ring.defer(8 * 1024 * 1024, 4 * 1024 * 1024));

    // …aged past the physics floor (qualify + drain + 2×refresh ≈ 8.02 s
    // on the shipped derivation) but far inside the 76 s routine fence.
    ticks.fetch_add(20_000, Ordering::SeqCst);

    let waits_before = free_grace::demand_waits();
    // The addendum shape: passed-global supply HIGH (foreign-lane
    // accumulation, §2.2's corrected note), lane-reachable at the trough.
    let released = ring.harvest_with_supply(64, 1_000, 4);
    assert!(
        released.is_empty(),
        "nothing is acknowledged and no deadline expired: the promise holds"
    );
    assert!(
        free_grace::demand_waits() > waits_before,
        "site 0 OBSERVES the coupling the row decayed under: ring aging \
         past the physics floor while the lane-reachable supply sits at \
         the trough"
    );
    // PR 3 (L2/L4), the inversion of PR 1's pin (iii): the demand mark is
    // now LIVE — rung a′ answers the FLOOR cadence for a member behind the
    // held labels, the coupling face reads the cliff, and — the law pin —
    // the FENCE deadline stays runway-only (a demand-prodded healthy
    // reader is asked to answer sooner, never fenced sooner).
    assert_eq!(
        free_grace::demand_pct(),
        100,
        "the coupling face reads the cliff (the s11 shape: decay at \
         scarcity 0 must read ≈ 100 HERE)"
    );
    let prods_before = free_grace::prods();
    let demand_prods_before = free_grace::demand_prods();
    let cadence = free_grace::take_prod_cadence(0)
        .expect("a member holding the free list is prodded under demand (rung a′)");
    assert_eq!(
        cadence, 1_000,
        "the demand prod is the FLOOR cadence (ack_refresh_floor ≈ 1 s on \
         the shipped derivation)"
    );
    assert!(free_grace::prods() > prods_before);
    assert!(
        free_grace::demand_prods() > demand_prods_before,
        "the demand arm's engagement is counted apart (⊆ prods)"
    );
    assert_eq!(
        free_grace::effective_bound_ms(),
        free_grace::fence_bound_base_ms(),
        "the demand mark NEVER feeds rung (b): with a long space runway \
         the fence deadline stays the routine bound (constraint 2 — asked \
         sooner, never fenced sooner)"
    );

    // The same harvest with a HEALTHY lane-reachable supply is not demand.
    let waits_mid = free_grace::demand_waits();
    let _ = ring.harvest_with_supply(64, 1_000, 1_000);
    assert_eq!(
        free_grace::demand_waits(),
        waits_mid,
        "a lane with supply is not coupled — site 0 stays quiet"
    );
    assert!(free_grace::test_clear_demand());
}

/// **The `DEMAND=0` restore-exactly contract** (KD-FG-10's lever law):
/// with the lever off, the coupled shape is the PRE-CAMPAIGN valve
/// verbatim — no demand mark, no rung-a′ prod, the coupling face 0 —
/// while site 0's OBSERVATION (PR 1's counter) keeps counting, because
/// the instrument is not the mechanism.
#[test]
fn the_demand_lever_off_restores_the_shipped_valve_verbatim() {
    let _serial = serial();
    free_grace::test_set_demand(Some(false));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let _grant = join(&owner, "r-off", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ring = GraceRing::new(1024);
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(200, Ordering::SeqCst);
    assert!(ring.defer(8 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(20_000, Ordering::SeqCst);

    let waits_before = free_grace::demand_waits();
    let _ = ring.harvest_with_supply(64, 1_000, 0);
    assert!(
        free_grace::demand_waits() > waits_before,
        "the OBSERVATION still counts under the lever (the instrument is \
         not the mechanism)"
    );
    assert_eq!(free_grace::demand_pct(), 0, "no coupling face");
    assert_eq!(
        free_grace::take_prod_cadence(0),
        None,
        "no rung-a′ prod: the pre-campaign valve verbatim (the long space \
         runway never prods, exactly as PR 1 pinned)"
    );
    assert!(free_grace::test_clear_demand());
}

/// **L2b — a prodded grant tightens the revalidation pass cadence, with
/// the WRITER'S ADVERTISED checkpoint ceiling as its physics floor** (OQ 3,
/// user decision 2026-08-25; the floor's INPUT made live by the
/// writer→member composite, user decision 2026-09-06):
/// `pass_interval = clamp(prodded renew_ms, advertised ceiling, routine)`,
/// TTL'd like the prod; the ceiling rides the same grant as the ask (a
/// grant that advertised none — `0` — floors at `CHECKPOINT_MAX_AGE_MS`,
/// the shipped law verbatim); expiry restores the routine cadence;
/// `PASS_ELASTIC=0` is the routine cadence always; and on a venue whose
/// routine interval already sits AT the routine floor the lever is
/// structurally inert UNTIL the writer's ceiling drops below it (the s11
/// venue's own shape — which is exactly what the composite changes).
#[test]
fn a_prodded_grant_tightens_the_pass_cadence_with_the_checkpoint_floor() {
    let _serial = serial();
    free_grace::test_set_pass_elastic(Some(true));
    free_grace::test_set_checkpoint_composite(Some(true));
    let ceiling = squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64;

    // A slow-flush venue: routine pass interval 5 s, prodded renew 1 s,
    // no ceiling advertised (the pre-composite grant).
    let routine = Duration::from_millis(5_000);
    let prods_before = free_grace::pass_prods();
    free_grace::note_prodded_renewal(1_000, 0, 10_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 10_500),
        Duration::from_millis(1_000),
        "the prodded ask tightens the pass cadence (5 s → 1 s per stage)"
    );
    assert!(
        free_grace::pass_prods() > prods_before,
        "the tightened pass is counted (the engagement gauge)"
    );
    assert_eq!(
        free_grace::pass_interval_ms(),
        1_000,
        "the cadence in force is published (the prod_renew_ms precedent)"
    );

    // The physics floor: an ask below the writer's ceiling clamps UP —
    // a pass faster than the writer's checkpoint cadence finds nothing.
    // Nothing advertised ⇒ the routine ceiling constant, as shipped.
    free_grace::note_prodded_renewal(200, 0, 11_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 11_100),
        Duration::from_millis(ceiling),
        "no ceiling advertised: the floor is CHECKPOINT_MAX_AGE_MS — the shipped law verbatim"
    );

    // Expiry: the routine cadence recovers within one reading TTL.
    assert_eq!(
        free_grace::reader_pass_interval(routine, 60_000),
        routine,
        "a quiet window restores the routine cadence"
    );
    assert_eq!(free_grace::pass_interval_ms(), 5_000);

    // The floor venue (routine == the routine ceiling): structurally inert
    // while the writer advertises its routine ceiling…
    let floor_routine = Duration::from_millis(ceiling);
    let prods_mid = free_grace::pass_prods();
    free_grace::note_prodded_renewal(ceiling, ceiling, 70_000);
    assert_eq!(
        free_grace::reader_pass_interval(floor_routine, 70_100),
        floor_routine,
        "routine already AT the routine floor: nothing to tighten (the s11 venue, no composite)"
    );
    assert_eq!(
        free_grace::pass_prods(),
        prods_mid,
        "structural inertness: no engagement counted where routine = floor"
    );
    // …and the composite is what makes it pay THERE: a grant advertising
    // a halved ceiling with a halved ask runs the pass at the halved
    // cadence — the floor's input is the writer's LIVE ceiling.
    free_grace::note_prodded_renewal(ceiling / 2, ceiling / 2, 71_000);
    assert_eq!(
        free_grace::reader_pass_interval(floor_routine, 71_100),
        Duration::from_millis(ceiling / 2),
        "an advertised P/2 ceiling floors the pass at P/2 (the composite)"
    );
    assert!(
        free_grace::pass_prods() > prods_mid,
        "the composite engages L2b on the floor venue (counted)"
    );
    // The re-stated law: an ask below the ADVERTISED ceiling clamps up to
    // it — the physics is unchanged, the input is live.
    free_grace::note_prodded_renewal(100, ceiling / 2, 72_000);
    assert_eq!(
        free_grace::reader_pass_interval(floor_routine, 72_100),
        Duration::from_millis(ceiling / 2),
        "an ask below the writer's advertised ceiling clamps up to it"
    );
    // A ceiling advertised ABOVE the routine constant (a slow-flush
    // writer's honest routine — its tick) floors the pass there: a pass
    // faster than that writer's checkpoints observes nothing.
    free_grace::note_prodded_renewal(1_000, 5_000, 73_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 73_100),
        routine,
        "a writer checkpointing every 5 s floors the pass at 5 s, whatever the ask"
    );

    // The composite lever off: the advertised ceiling is ignored — the
    // shipped floor constant, byte for byte.
    free_grace::test_set_checkpoint_composite(Some(false));
    free_grace::note_prodded_renewal(ceiling / 2, ceiling / 2, 74_000);
    assert_eq!(
        free_grace::reader_pass_interval(floor_routine, 74_100),
        floor_routine,
        "CHECKPOINT_COMPOSITE=0: the shipped floor (the constant) verbatim"
    );
    assert!(free_grace::test_clear_checkpoint_composite());
    free_grace::test_set_checkpoint_composite(Some(true));

    // The lever: routine verbatim, engagement 0.
    free_grace::test_set_pass_elastic(Some(false));
    free_grace::note_prodded_renewal(1_000, 0, 80_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 80_100),
        routine,
        "PASS_ELASTIC=0: the pre-campaign S5 cadence verbatim"
    );
    assert!(free_grace::test_clear_pass_elastic());

    // The numbers that deliberately DO NOT move under a prodded window
    // (§5.2b's never-weakens argument, KD-FG-11 as amended): the published
    // staleness bound is the ROUTINE derivation — structurally independent
    // of the pass word AND of the advertised ceiling (it reads neither).
    let bound_before = squeezefs::ro_coherence::reader_staleness_bound();
    free_grace::test_set_pass_elastic(Some(true));
    free_grace::note_prodded_renewal(ceiling / 2, ceiling / 2, 90_000);
    assert_eq!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        bound_before,
        "the PUBLISHED staleness bound never flickers with load — it is \
         the guarantee in force, not the cadence in force"
    );
    // …and the elastic cadence sits INSIDE it: a member passing at the
    // advertised ceiling against a writer checkpointing at that ceiling
    // is at most `pass + ceiling` stale — half the published bound.
    let pass = free_grace::reader_pass_interval(floor_routine, 90_100);
    assert!(
        pass + Duration::from_millis(ceiling / 2) <= bound_before,
        "the elastic cadence's worst-case staleness {:?} honours the published bound {:?}",
        pass + Duration::from_millis(ceiling / 2),
        bound_before
    );
    assert!(free_grace::test_clear_pass_elastic());
    assert!(free_grace::test_clear_checkpoint_composite());
}

/// **The loop-latency instruments (PR 1, §8 rows 1–2).** `bound_age_ms`
/// is owner-clock `now − BOUND` while armed and holding (0 unarmed, 0
/// when drained); `residence` records `now − (label−1)` at each release.
#[test]
fn bound_age_and_residence_follow_the_ring() {
    let _serial = serial();
    assert_eq!(
        free_grace::bound_age_ms(),
        0,
        "no plane: the loop-latency instrument reads 0"
    );
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-age", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ring = GraceRing::new(1024);
    let label_at = clock.now_ms() + 1;
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(3_000, Ordering::SeqCst);

    // Holding, reader acked nothing: the bound has never advanced, so its
    // age is the clock's own reading (maximally old — honest).
    assert!(
        free_grace::bound_age_ms() >= 3_000,
        "armed + holding: the age is now − BOUND (got {})",
        free_grace::bound_age_ms()
    );

    // The reader acknowledges; the release records residence.
    let samples_before = free_grace::residence_samples();
    ack(&owner, "r-age", grant.epoch, label_at);
    owner.refresh_free_grace_bound();
    let released = ring.harvest_with_supply(64, u64::MAX, u64::MAX);
    assert_eq!(released.len(), 1, "the acknowledged offset releases");
    assert_eq!(
        free_grace::residence_samples() - samples_before,
        1,
        "each release stamps one residence sample (now − (label−1))"
    );
    assert_eq!(
        free_grace::bound_age_ms(),
        0,
        "the ring drained: the instrument falls to 0"
    );
}

// ===========================================================================
// D-4 (e2e perf audit ladder row 13, DLM #6 — `.benchmarks/2026-09-05-d4-
// free-grace-sustain.md`): the design's §3 RATE EQUATION, closed-loop,
// in-process. Every decision below is PRODUCT code on the manual owner
// clock — labels, the runway, rung (a)/(a′), site 0, the ladder's three
// gates, the min-composition, the sweep, the L3 refresh — and the only
// test-side arithmetic is the allocator's free-list bookkeeping (counts
// of blocks by residue class, `BlockAllocator`'s own law: a lane-0 stream
// reaches lane-0 offsets only; foreign-lane releases accumulate on the
// passed-global number until a lane harvest consumes them, and none runs
// here — the row's `alloc_lane_harvests 0`). L5 (the co-writer's
// ahead-of-stall refill) rides the publish wire and is NOT in this loop;
// L2b is structurally inert on this venue (routine pass = the 1 s floor).
// ===========================================================================

/// The design's §3.4 loop, parametrized (`docs/design-free-grace-sustain.md`).
#[derive(Debug, Clone, Copy)]
struct LoopShape {
    label: &'static str,
    /// Readers acknowledging (the s11 venue: 8 co-writers).
    members: usize,
    /// The STARVING stream's circulating spare, blocks — lane 0's
    /// `lane_reachable` at t0 (all virgin).
    lane_spare: u64,
    /// The starving stream's offered rewrite demand, blocks/s.
    lane_demand_per_s: u64,
    /// The rest of the fleet's displaced frees entering the SAME ring
    /// (co-writer-lane blocks the authority `finish_free`s), blocks/s.
    storm_per_s: u64,
    /// Owner-clock duration.
    duration_ms: u64,
    /// Members' first renewals spread across one beat (the §3.2 T8 phase
    /// assumption) or all in phase (the mw rig's near-simultaneous start).
    staggered: bool,
    /// The writer's ROUTINE checkpoint cadence, ms (`CHECKPOINT_MAX_AGE_MS`
    /// on the shipped tree): a member's pass ADVANCES iff a checkpoint
    /// landed since its previous pass — the field's `epochs ÷ polls`.
    checkpoint_ms: u64,
}

/// The lever configuration one loop runs under: the D-4 pair
/// (`ACK_PIPELINE`, `DEMAND`) plus the hold-time campaign's levers. Every
/// arm is a product knob read through its test seam; the sim performs
/// only the ACT the product would (a wake, a renewal), never the decision.
#[derive(Debug, Clone, Copy)]
struct Levers {
    pipeline: bool,
    demand: bool,
    /// (b) a promotion triggers the member's renewal at once.
    ack_renewal: bool,
    /// (d) a binding member's advancing ack marks the bound dirty.
    refresh_on_ack: bool,
    /// Re-derivation item 1: qualify = the writer's checkpoint ceiling +
    /// skew (off = the pre-change `staleness + skew`).
    qualify_ceiling: bool,
    /// Re-derivation item 2: the layout cache is epoch-step stamped, so
    /// the drain drops its `S` term (off = `S + D_purge`).
    drain_epoch_stamp: bool,
    /// Re-derivation item 3: the drain is OBSERVED (the pre-step in-flight
    /// serves reaching zero) and `D_purge` only a tripwire (off = the
    /// `D_purge` timer).
    drain_observed: bool,
    /// The writer→member checkpoint composite (adjudication item 4): the
    /// writer's checkpoint ceiling follows the valve's ask (P/2 while an
    /// ask is in force), the grant carries it, and the members' pass and
    /// beat floors follow it.
    composite: bool,
}

impl Levers {
    /// The D-4 matrix's configuration: the hold-time levers OFF (the
    /// 2026-09-05 binary — the pre-campaign baseline for this campaign).
    fn d4(pipeline: bool, demand: bool) -> Self {
        Self {
            pipeline,
            demand,
            ack_renewal: false,
            refresh_on_ack: false,
            qualify_ceiling: false,
            drain_epoch_stamp: false,
            drain_observed: false,
            composite: false,
        }
    }

    /// The hold-time campaign's shipped configuration (H3) with the
    /// ladder re-derivation levers OFF — the 2026-09-06 hold-time binary.
    fn h3() -> Self {
        Self {
            ack_renewal: true,
            refresh_on_ack: true,
            ..Self::d4(true, true)
        }
    }

    fn config(&self) -> &'static str {
        match (
            self.pipeline,
            self.demand,
            self.ack_renewal,
            self.refresh_on_ack,
            self.qualify_ceiling,
            self.drain_epoch_stamp,
            self.drain_observed,
            self.composite,
        ) {
            (false, false, false, false, false, false, false, false) => "A0 pipeline=0 demand=0",
            (true, false, false, false, false, false, false, false) => "A1 pipeline=1 demand=0",
            (false, true, false, false, false, false, false, false) => "A2 pipeline=0 demand=1",
            (true, true, false, false, false, false, false, false) => {
                "A3 pipeline=1 demand=1 (hold-time levers off)"
            }
            (true, true, true, false, false, false, false, false) => "H1 +ack_renewal",
            (true, true, false, true, false, false, false, false) => "H2 +refresh_on_ack",
            (true, true, true, true, false, false, false, false) => {
                "H3 +ack_renewal +refresh_on_ack (hold-time shipped)"
            }
            (true, true, true, true, true, false, false, false) => "R1 H3 +qualify_ceiling",
            (true, true, true, true, true, true, false, false) => "R2 R1 +drain_epoch_stamp",
            (true, true, true, true, true, true, true, false) => {
                "R3 R2 +drain_observed (re-derivation shipped)"
            }
            (true, true, true, true, false, false, false, true) => "H4 H3 +checkpoint_composite",
            (true, true, true, true, true, true, true, true) => {
                "R4 R3 +checkpoint_composite (all four adjudication items)"
            }
            _ => "custom",
        }
    }
}

/// One measured row of the loop (the note's columns). Every `*_steady`
/// figure is a delta over the steady window (the last two thirds — the
/// first third is the loop's fill: readers acking their first labels, the
/// lane spending its virgin margin).
#[derive(Debug, Clone)]
struct LoopRow {
    config: &'static str,
    /// Lane-0 allocations that landed, per steady second — the stream's
    /// sustained throughput (× 4 MiB = MiB/s).
    steady_allocs_per_s: f64,
    /// The thirds law: middle-third vs last-third throughput.
    mid_third_per_s: f64,
    last_third_per_s: f64,
    /// `StorageFull` refusals in the steady window (each one park slice).
    stalls_steady: u64,
    deferrals: u64,
    releases: u64,
    held_end: u64,
    forced: u64,
    fences: u64,
    bound_age_mean_ms: f64,
    bound_age_max_ms: u64,
    residence_mean_ms: f64,
    /// Prods handed out in the steady window (the bootstrap's own prods —
    /// a ring whose first two labels land in one ms reads a zero runway —
    /// belong to the fill).
    prods_steady: u64,
    demand_prods: u64,
    demand_waits: u64,
    bound_refreshes: u64,
    tightenings: u64,
    prod_decays: u64,
    acks_steady: u64,
    renewals_steady: u64,
    from_freelist: u64,
    fresh: u64,
    /// The hold-time decomposition (`free_grace_hold_phase_ns` means, ms)
    /// over the WHOLE run: defer→checkpointed, checkpointed→min_acked,
    /// min_acked→released, and the samples the checkpoint stage could not
    /// place.
    hold_defer_ck_ms: f64,
    hold_ck_acked_ms: f64,
    hold_acked_rel_ms: f64,
    hold_unplaced: u64,
    /// `free_grace_hold_ms` at the end (the live EWMA).
    hold_ms: u64,
    /// The per-member ack lag at the end: max / mean.
    ack_lag_max_ms: u64,
    ack_lag_mean_ms: u64,
    /// Renewals a promotion triggered (lever b's engagement).
    ack_renewals: u64,
    /// Bound refreshes a binding ack triggered (lever d's engagement).
    refreshes_on_ack: u64,
    /// The composite's cost ledger over the WHOLE run: the writer's
    /// checkpoint cycles (the hold ledger's marks — 1:1 with
    /// `meta_kv_checkpoints` on an armed writer) and the lease lane's
    /// renewals (the product's `membership_renewals`), as rates.
    checkpoints_per_s: f64,
    renewals_per_s: f64,
    /// Checkpoint cycles run with the elastic ceiling in force (the
    /// composite's writer-side engagement).
    elastic_cycles: u64,
    /// L2b passes run on a tightened cadence (the composite's member-side
    /// engagement — structurally 0 on this venue without it).
    pass_prods: u64,
    /// The smallest checkpoint ceiling the writer enforced during the run.
    ceiling_min_ms: u64,
    /// The worst `pass interval + ceiling in force` any pass ran under —
    /// the elastic cadence's actual staleness, checked against the
    /// PUBLISHED bound (which never moves).
    staleness_worst_ms: u64,
}

impl LoopRow {
    fn render(&self, shape: &LoopShape) -> String {
        format!(
            "ROW {label} {config}: lane {mibs:.1} MiB/s ({allocs:.2} blk/s; thirds mid {mid:.2} / \
             last {last:.2}) stalls {stalls} | deferrals {d} releases {r} held {h} closure {clo} | \
             forced {f} fences {fe} | bound_age mean {ba:.0} ms max {bam} ms residence mean {res:.0} ms | \
             prods {p} demand_prods {dp} demand_waits {dw} refreshes {rf} tightenings {t} decays {dec} | \
             acks {acks} renewals {ren} | alloc freelist {fl} fresh {fr} | \
             hold defer→ckpt {hck:.0} ckpt→min_acked {hak:.0} min_acked→rel {hrel:.0} unplaced {hun} \
             hold_ms {hold} ack_lag max {lmax} mean {lmean} | ack_renewals {ar} refreshes_on_ack {roa} | \
             cost checkpoints/s {cps:.2} renewals/s {rps:.2} elastic_cycles {ec} pass_prods {pp} \
             ceiling_min {cm} ms staleness_worst {sw} ms",
            label = shape.label,
            config = self.config,
            mibs = self.steady_allocs_per_s * 4.0,
            allocs = self.steady_allocs_per_s,
            mid = self.mid_third_per_s,
            last = self.last_third_per_s,
            stalls = self.stalls_steady,
            d = self.deferrals,
            r = self.releases,
            h = self.held_end,
            clo = if self.deferrals == self.releases + self.held_end { "OK" } else { "BROKEN" },
            f = self.forced,
            fe = self.fences,
            ba = self.bound_age_mean_ms,
            bam = self.bound_age_max_ms,
            res = self.residence_mean_ms,
            p = self.prods_steady,
            dp = self.demand_prods,
            dw = self.demand_waits,
            rf = self.bound_refreshes,
            t = self.tightenings,
            dec = self.prod_decays,
            acks = self.acks_steady,
            ren = self.renewals_steady,
            fl = self.from_freelist,
            fr = self.fresh,
            hck = self.hold_defer_ck_ms,
            hak = self.hold_ck_acked_ms,
            hrel = self.hold_acked_rel_ms,
            hun = self.hold_unplaced,
            hold = self.hold_ms,
            lmax = self.ack_lag_max_ms,
            lmean = self.ack_lag_mean_ms,
            ar = self.ack_renewals,
            roa = self.refreshes_on_ack,
            cps = self.checkpoints_per_s,
            rps = self.renewals_per_s,
            ec = self.elastic_cycles,
            pp = self.pass_prods,
            cm = self.ceiling_min_ms,
            sw = self.staleness_worst_ms,
        )
    }
}

/// One simulated reader: its ladder (product code), the label it learned
/// on its last grant, the ack it is carrying, and its two cadences.
struct SimReader {
    id: String,
    epoch: u64,
    ladder: ReaderAckLadder,
    learned: (u64, u64),
    /// The writer's checkpoint ceiling advertised on the grant that carried
    /// `learned` (`MemberSession::checkpoint_ceiling_ms`'s word — the
    /// constant when nothing was advertised).
    ceiling_ms: u64,
    acked: u64,
    next_renew_ms: u64,
    next_pass_ms: u64,
    /// The previous pass's instant: a pass advances iff a checkpoint
    /// landed after it.
    last_pass_ms: u64,
}

const LOOP_BLOCK: u64 = 4 * 1024 * 1024;
/// The partition width the s11 venue derives (9 writers → 16).
const LOOP_LANES: u64 = 16;
/// The starving stream's residue class.
const LOOP_LANE: u64 = 0;
/// A parked writer's queue depth: demand beyond it is a STALLED writer,
/// not a backlog — the storm is paced by the loop, never queued forever.
const LOOP_MAX_BACKLOG: u64 = 16;
/// The simulation step: one owner-clock millisecond, so a storm's frees
/// spread across labels the way a real one's do (two frees stamped in one
/// ms read as a zero-span burst — the documented cliff reading — which a
/// coarser step would manufacture at every tick).
const LOOP_STEP_MS: u64 = 1;

/// Drive the closed loop once under one lever configuration.
///
/// The starving stream (lane 0) allocates at its offered rate through the
/// allocator's funnel order — harvest, free list, virgin mint, then the
/// `StorageFull` arm's pressure harvest — and every landed rewrite
/// displaces one lane-0 block into the ring. The storm (the co-writers'
/// shipped frees) enters the same ring at its own rate and its releases
/// pile up on the passed-global number, reachable to nobody here. Members
/// pass on the cadence the product's L2b resolver answers (a pass
/// advances iff a checkpoint landed since the previous one) and renew on
/// the cadence their last grant carried; the owner sweeps on
/// `renew_interval`. The writer checkpoints on the product's own decision
/// — `elapsed ≥ the ceiling in force`, the checkpoint task's tick — with
/// the composite lever deciding whether that ceiling follows the ask.
fn run_closed_loop(shape: &LoopShape, levers: Levers) -> LoopRow {
    free_grace::reset_for_test();
    membership::uninstall();
    let Levers {
        pipeline,
        demand,
        ack_renewal,
        refresh_on_ack,
        qualify_ceiling,
        drain_epoch_stamp,
        drain_observed,
        composite,
    } = levers;
    free_grace::test_set_ack_pipeline(Some(pipeline));
    free_grace::test_set_demand(Some(demand));
    free_grace::test_set_ack_renewal(Some(ack_renewal));
    free_grace::test_set_refresh_on_ack(Some(refresh_on_ack));
    free_grace::test_set_qualify_ceiling(Some(qualify_ceiling));
    squeezefs::ro_coherence::test_set_drain_epoch_stamp(Some(drain_epoch_stamp));
    squeezefs::ro_coherence::test_set_drain_observed(Some(drain_observed));
    if drain_observed {
        // The cadence task's spawn arms the ledger on a real mount.
        squeezefs::ro_coherence::test_arm_serve_ledger();
    }
    free_grace::test_set_checkpoint_composite(Some(composite));
    let renewals_metric_before = METRICS.membership_renewals.load(Ordering::Relaxed);
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let clocks = owner.clocks().clone();
    let renew_ms = clocks.renew_interval.as_millis() as u64;
    let skew_ms = clocks.skew_max.as_millis() as u64;
    let staleness_ms = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
    let pass_ms = squeezefs::ro_coherence::reader_revalidate_interval().as_millis() as u64;
    // The ladder's windows are the PRODUCT's derivations (the reader's
    // `reader_pass_completed` computes exactly these). The writer's ceiling
    // is what `MemberSession::checkpoint_ceiling_ms` answers: the value the
    // grant ADVERTISED (the composite — P/2 while an ask is in force), the
    // landing ceiling when none was or the composite lever is off.
    let landing_ceiling_ms =
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived();
    let learned_ceiling = |advertised: u64| {
        if advertised != 0 && composite {
            advertised
        } else {
            landing_ceiling_ms
        }
    };
    let d_purge_ms = clocks.d_purge.as_millis() as u64;
    let drain_lag_ms =
        free_grace::drain_lag_ms(drain_epoch_stamp, drain_observed, staleness_ms, d_purge_ms);

    let t0 = clock.now_ms();
    let mut readers: Vec<SimReader> = (0..shape.members)
        .map(|i| {
            let id = format!("sim-reader-{i}");
            let grant = join(&owner, &id, MemberRole::Reader);
            // Staggered: renewal AND pass phases spread across one beat /
            // one pass — every member's loops run on their own phase in
            // the field (the §3.2 T8 assumption for both grids).
            let (phase, pass_phase) = if shape.staggered {
                (
                    renew_ms * (i as u64 + 1) / shape.members as u64,
                    pass_ms * i as u64 / shape.members as u64,
                )
            } else {
                (renew_ms, 0)
            };
            SimReader {
                id,
                epoch: grant.epoch,
                ladder: ReaderAckLadder::new(),
                learned: (grant.granted_at_owner_ms, t0),
                ceiling_ms: learned_ceiling(grant.checkpoint_ceiling_ms),
                acked: 0,
                next_renew_ms: t0 + phase,
                next_pass_ms: t0 + pass_ms + pass_phase,
                last_pass_ms: t0,
            }
        })
        .collect();
    owner.refresh_free_grace_bound();
    assert!(free_grace::armed(), "owner + members ⇒ the gate is live");

    // The ring is never the constraint (the field: 572 of 3.6 M) — cap it
    // far above anything this loop holds.
    let ring = GraceRing::new(1 << 22);
    // The allocator's arithmetic — lane 0's reachable set and the
    // passed-global accumulation (`BlockAllocator::{lane_reachable_blocks,
    // free_supply_blocks}`; `grace_supply_blocks` resolves the lever).
    let mut lane_free: u64 = 0;
    let mut lane_virgin: u64 = shape.lane_spare;
    let mut foreign_released: u64 = 0;
    let mut lane_mint: u64 = 0;
    let mut storm_mint: u64 = 0;
    let route = |released: &[u64], lane_free: &mut u64, foreign: &mut u64| {
        for off in released {
            if (off / LOOP_BLOCK) % LOOP_LANES == LOOP_LANE {
                *lane_free += 1;
            } else {
                *foreign += 1;
            }
        }
    };
    let harvest = |ring: &GraceRing, lane_free: &mut u64, lane_virgin: u64, foreign: &mut u64| {
        let lane_reachable = *lane_free + lane_virgin;
        let supply = if demand {
            lane_reachable
        } else {
            lane_reachable + *foreign
        };
        let released = ring.harvest_with_supply(free_grace::HARVEST_BATCH, supply, lane_reachable);
        route(&released, lane_free, foreign);
    };

    let mut lane_acc: u64 = 0;
    let mut storm_acc: u64 = 0;
    let mut allocs_by_third = [0u64; 3];
    let mut from_freelist = 0u64;
    let mut fresh = 0u64;
    let mut renewals = 0u64;
    let mut bound_age_samples: Vec<u64> = Vec::new();
    let mut next_sample_ms = t0 + pass_ms;
    let mut next_sweep_ms = t0 + renew_ms;
    // A refused allocation parks for the product's own slice before it
    // re-runs the pressure harvest (finding 29's bounded wait).
    let mut parked_until_ms = 0u64;
    // The writer's checkpoint cadence (a storming writer always has
    // dirty nodes, so the cycle runs at the ceiling): each cycle marks the
    // hold ledger, and a member's pass advances iff one landed since its
    // previous pass. Under the composite the ceiling is the product's
    // decision (`checkpoint_ceiling_in_force_ms`, read per step the way
    // the checkpoint task reads it per tick); otherwise the shape's.
    let mut last_checkpoint_ms = t0;
    let mut ceiling_min_ms = u64::MAX;
    let mut staleness_worst_ms = 0u64;
    let pass_routine = Duration::from_millis(pass_ms);
    let mut ack_renewals = 0u64;
    let third_ms = shape.duration_ms / 3;
    let steady_from_ms = t0 + third_ms;
    let end = t0 + shape.duration_ms;
    // Counters snapshotted at the steady window's start (deltas below).
    let mut at_steady: Option<(u64, u64, u64, u64)> = None;

    while clock.now_ms() < end {
        ticks.fetch_add(LOOP_STEP_MS, Ordering::SeqCst);
        let now = clock.now_ms();
        let third = ((now - t0) / third_ms.max(1)).min(2) as usize;
        if at_steady.is_none() && now >= steady_from_ms {
            at_steady = Some((
                free_grace::alloc_stalls(),
                free_grace::prods(),
                free_grace::reader_acks(),
                renewals,
            ));
        }

        // The writer's checkpoint (before the readers' passes: a pass at
        // the same instant adopts it).
        let elastic = free_grace::checkpoint_ceiling_in_force_ms();
        let ceiling_ms = if composite {
            elastic.unwrap_or(shape.checkpoint_ms)
        } else {
            shape.checkpoint_ms
        };
        ceiling_min_ms = ceiling_min_ms.min(ceiling_ms);
        if now - last_checkpoint_ms >= ceiling_ms {
            free_grace::note_checkpoint_completed(Duration::from_millis(20));
            if composite && elastic.is_some() {
                free_grace::note_elastic_checkpoint_cycle();
            }
            last_checkpoint_ms = now;
        }

        // The storm: the co-writers' displaced frees, `finish_free`d at
        // the authority (each runs the routine harvest, per terminal free).
        storm_acc += shape.storm_per_s * LOOP_STEP_MS;
        while storm_acc >= 1_000 {
            storm_acc -= 1_000;
            let lane = 1 + (storm_mint % (LOOP_LANES / 2));
            let idx = storm_mint * LOOP_LANES + lane;
            storm_mint += 1;
            assert!(ring.defer(idx * LOOP_BLOCK, LOOP_BLOCK), "armed ⇒ deferred");
            harvest(&ring, &mut lane_free, lane_virgin, &mut foreign_released);
        }

        // The starving stream: the allocator's funnel, in its order.
        lane_acc =
            (lane_acc + shape.lane_demand_per_s * LOOP_STEP_MS).min(LOOP_MAX_BACKLOG * 1_000);
        while lane_acc >= 1_000 && now >= parked_until_ms {
            harvest(&ring, &mut lane_free, lane_virgin, &mut foreign_released);
            let landed = if lane_free > 0 {
                lane_free -= 1;
                from_freelist += 1;
                true
            } else if lane_virgin > 0 {
                lane_virgin -= 1;
                fresh += 1;
                true
            } else {
                // The `StorageFull` arm: the pressure harvest, then the
                // counted refusal and the park (finding 29).
                let released = ring.harvest_pressure(free_grace::HARVEST_BATCH);
                route(&released, &mut lane_free, &mut foreign_released);
                if lane_free > 0 {
                    lane_free -= 1;
                    from_freelist += 1;
                    true
                } else {
                    free_grace::note_alloc_stall(ring.len(), ring.bytes());
                    parked_until_ms = now + free_grace::pressure_park_slice_ms();
                    false
                }
            };
            if !landed {
                break;
            }
            lane_acc -= 1_000;
            allocs_by_third[third] += 1;
            // The rewrite displaces one lane-0 block: it enters the ring.
            let idx = lane_mint * LOOP_LANES + LOOP_LANE;
            lane_mint += 1;
            assert!(ring.defer(idx * LOOP_BLOCK, LOOP_BLOCK), "armed ⇒ deferred");
        }

        // The readers: passes (the ladder) and renewals (the carriage).
        for r in readers.iter_mut() {
            if now >= r.next_pass_ms {
                let advanced = last_checkpoint_ms > r.last_pass_ms;
                if advanced {
                    // The product's purge sink steps the reader's layout
                    // generation as the last act of every epoch step.
                    squeezefs::ro_coherence::test_note_epoch_step();
                }
                let promoted = r.ladder.note_pass(AckInputs {
                    label: r.learned.0,
                    learned_at_ms: r.learned.1,
                    pass_start_ms: now,
                    now_ms: now,
                    advanced,
                    qualify_lag_ms: free_grace::qualify_lag_ms(
                        qualify_ceiling,
                        r.ceiling_ms,
                        staleness_ms,
                        skew_ms,
                    ),
                    drain_lag_ms,
                    // `reader_pass_completed`'s derivation: the pass
                    // cadence floored at the skew, with the ceiling this
                    // member learned (the composite's depth input).
                    refresh_floor_ms: free_grace::reader_refresh_floor_ms(
                        pass_ms,
                        r.ceiling_ms,
                        skew_ms,
                    ),
                    // Item 3: with the observed drain in force the pass
                    // records the generation its steps left (no serves run
                    // in this model, so the drain is the step itself).
                    drain_gen: if drain_observed {
                        squeezefs::ro_coherence::reader_step_generation()
                    } else {
                        0
                    },
                    drain_budget_ms: d_purge_ms,
                });
                r.last_pass_ms = now;
                if let Some(label) = promoted {
                    r.acked = label;
                    // Lever (b): the promotion carries itself home NOW as a
                    // CARRIAGE renewal (the product wakes the renewal loop;
                    // the sim performs the wake's effect): the ack travels,
                    // the routine beat keeps its schedule and the label
                    // stays the routine renewal's (`renewed_carriage` —
                    // which deposits the grant's ask + ceiling for the
                    // pass resolver exactly like the routine renewal).
                    if free_grace::ack_renewal_enabled() {
                        match owner.renew(&r.id, r.epoch, r.acked) {
                            RenewOutcome::Renewed(grant) => {
                                free_grace::note_prodded_renewal(
                                    grant.renew_ms,
                                    grant.checkpoint_ceiling_ms,
                                    now,
                                );
                                renewals += 1;
                                ack_renewals += 1;
                            }
                            other => panic!("a carriage renewal is admitted: {other:?}"),
                        }
                    }
                }
                // The next pass on the product's L2b cadence (the
                // revalidation loop's own sleep): the routine interval,
                // or the prodded ask floored at the advertised ceiling.
                let pass = free_grace::reader_pass_interval(pass_routine, now);
                let pass_ms_now = pass.as_millis() as u64;
                staleness_worst_ms = staleness_worst_ms.max(pass_ms_now + ceiling_ms);
                r.next_pass_ms = now + pass_ms_now.max(LOOP_STEP_MS);
            }
            if now >= r.next_renew_ms {
                match owner.renew(&r.id, r.epoch, r.acked) {
                    RenewOutcome::Renewed(grant) => {
                        r.learned = (grant.granted_at_owner_ms, now);
                        r.ceiling_ms = learned_ceiling(grant.checkpoint_ceiling_ms);
                        r.next_renew_ms = now + grant.renew_ms.max(LOOP_STEP_MS);
                        // `MemberSession::renewed`'s deposit: the ask and
                        // the ceiling it rode in with.
                        free_grace::note_prodded_renewal(
                            grant.renew_ms,
                            grant.checkpoint_ceiling_ms,
                            now,
                        );
                        renewals += 1;
                    }
                    other => panic!("a healthy reader's renewal is admitted: {other:?}"),
                }
            }
        }

        // The owner's sweep (the idle-fleet backstop).
        if now >= next_sweep_ms {
            owner.refresh_free_grace_bound();
            next_sweep_ms += renew_ms;
        }
        if now >= next_sample_ms {
            if third >= 1 {
                bound_age_samples.push(free_grace::bound_age_ms());
            }
            next_sample_ms += pass_ms;
        }
    }

    let steady_secs = (shape.duration_ms - third_ms) as f64 / 1_000.0;
    let third_secs = third_ms as f64 / 1_000.0;
    let residence_mean_ms = free_grace::stats_snapshot()["free_grace_residence_ms"]["mean_ns"]
        .as_u64()
        .unwrap_or(0) as f64
        / 1e6;
    let (stalls0, prods0, acks0, renewals0) = at_steady.expect("the loop ran past its fill");
    let snap = free_grace::stats_snapshot();
    let phase_mean_ms = |name: &str| -> f64 {
        snap["free_grace_hold_phase_ns"][name]["mean_ns"]
            .as_u64()
            .unwrap_or(0) as f64
            / 1e6
    };
    let lag = &snap["free_grace_member_ack_lag_ms"];
    let row = LoopRow {
        config: levers.config(),
        steady_allocs_per_s: (allocs_by_third[1] + allocs_by_third[2]) as f64 / steady_secs,
        mid_third_per_s: allocs_by_third[1] as f64 / third_secs,
        last_third_per_s: allocs_by_third[2] as f64 / third_secs,
        stalls_steady: free_grace::alloc_stalls() - stalls0,
        deferrals: free_grace::deferrals(),
        releases: free_grace::releases(),
        held_end: free_grace::held_offsets(),
        forced: free_grace::forced_releases(),
        fences: free_grace::laggard_fences(),
        bound_age_mean_ms: bound_age_samples.iter().sum::<u64>() as f64
            / bound_age_samples.len().max(1) as f64,
        bound_age_max_ms: bound_age_samples.iter().copied().max().unwrap_or(0),
        residence_mean_ms,
        prods_steady: free_grace::prods() - prods0,
        demand_prods: free_grace::demand_prods(),
        demand_waits: free_grace::demand_waits(),
        bound_refreshes: free_grace::bound_refreshes(),
        tightenings: free_grace::bound_tightenings(),
        prod_decays: free_grace::prod_decays(),
        acks_steady: free_grace::reader_acks() - acks0,
        renewals_steady: renewals - renewals0,
        from_freelist,
        fresh,
        hold_defer_ck_ms: phase_mean_ms("defer_checkpointed"),
        hold_ck_acked_ms: phase_mean_ms("checkpointed_min_acked"),
        hold_acked_rel_ms: phase_mean_ms("min_acked_released"),
        hold_unplaced: free_grace::hold_unplaced(),
        hold_ms: free_grace::hold_ms(),
        ack_lag_max_ms: lag["max"].as_u64().unwrap_or(0),
        ack_lag_mean_ms: lag["mean"].as_u64().unwrap_or(0),
        ack_renewals,
        refreshes_on_ack: free_grace::bound_refreshes_on_ack(),
        checkpoints_per_s: free_grace::checkpoint_marks() as f64
            / (shape.duration_ms as f64 / 1_000.0),
        renewals_per_s: (METRICS.membership_renewals.load(Ordering::Relaxed)
            - renewals_metric_before) as f64
            / (shape.duration_ms as f64 / 1_000.0),
        elastic_cycles: free_grace::checkpoint_elastic_cycles(),
        pass_prods: free_grace::pass_prods(),
        ceiling_min_ms,
        staleness_worst_ms,
    };
    drop(readers);
    assert!(free_grace::test_clear_ack_pipeline());
    assert!(free_grace::test_clear_demand());
    assert!(free_grace::test_clear_ack_renewal());
    assert!(free_grace::test_clear_refresh_on_ack());
    assert!(free_grace::test_clear_qualify_ceiling());
    assert!(squeezefs::ro_coherence::test_clear_drain_epoch_stamp());
    assert!(squeezefs::ro_coherence::test_clear_drain_observed());
    assert!(free_grace::test_clear_checkpoint_composite());
    row
}

/// The four lever configurations the acceptance A/B names (PR 5 (e)):
/// A0 = the pre-campaign shape (`ACK_PIPELINE=0 DEMAND=0`), A3 = shipped.
fn run_loop_matrix(shape: &LoopShape) -> [LoopRow; 4] {
    let rows = [
        run_closed_loop(shape, Levers::d4(false, false)),
        run_closed_loop(shape, Levers::d4(true, false)),
        run_closed_loop(shape, Levers::d4(false, true)),
        run_closed_loop(shape, Levers::d4(true, true)),
    ];
    for r in &rows {
        println!("{}", r.render(shape));
    }
    rows
}

/// **The recycle-bound stream (finding 15's shape): the levers move the
/// loop's ceiling, and the promise never bends.** Lane 0's spare is
/// smaller than its demand × the routine loop latency, so the stream is
/// paced by the release rate (Little's law: `spare ÷ L_lag`). The storm
/// keeps the passed-global number high (foreign-lane releases nobody
/// harvests — the KD-FG-10 skew), so the pre-campaign valve reads no
/// scarcity while the lane starves.
///
/// Laws pinned (numbers printed as `ROW` lines for the note):
/// * closure `deferrals ≡ releases + held` on every configuration;
/// * `forced_releases = 0` and `laggard_fences = 0` on every configuration
///   (faster HONEST acks, never faster fences);
/// * the shipped configuration (A3) sustains the stream at a higher rate
///   than the pre-campaign one (A0), with a lower steady `bound_age`, and
///   never stalls it more;
/// * the demand arm engages on this shape — site 0 observes the coupling
///   (`demand_waits > 0`) and the members are asked at the floor
///   (`prods > 0`). `demand_prods` is NOT asserted: it counts prods the
///   space arm would not have issued, and under the KD-FG-10 re-base the
///   space arm reads the same lane-reachable trough and asks the floor on
///   its own — the mark's share is 0 by the ledger's own definition.
#[test]
fn the_recycle_bound_loop_releases_faster_with_the_levers_and_never_fences() {
    let _serial = serial();
    let shape = LoopShape {
        label: "coupled(spare=256,demand=20/s,storm=500/s,8m,staggered)",
        members: 8,
        lane_spare: 256,
        lane_demand_per_s: 20,
        storm_per_s: 500,
        duration_ms: 300_000,
        staggered: true,
        checkpoint_ms: 1_000,
    };
    let rows = run_loop_matrix(&shape);
    for r in &rows {
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure — no leak, no double release",
            r.config
        );
        assert_eq!(
            r.forced, 0,
            "{}: no forced release on a healthy fleet",
            r.config
        );
        assert_eq!(
            r.fences, 0,
            "{}: no laggard fence on a healthy fleet",
            r.config
        );
        assert!(
            r.deferrals > 0 && r.releases > 0,
            "{}: the loop ran (deferrals {} releases {})",
            r.config,
            r.deferrals,
            r.releases
        );
    }
    let a0 = &rows[0];
    let a3 = &rows[3];
    assert!(
        a3.demand_waits > 0 && a3.prods_steady > 0,
        "the shipped configuration observes the coupling and asks the floor \
         (demand_waits {} prods {})",
        a3.demand_waits,
        a3.prods_steady
    );
    assert!(
        a3.steady_allocs_per_s > a0.steady_allocs_per_s,
        "the levers raise the recycle-bound stream's sustained rate: A3 {:.2} vs A0 {:.2} blk/s",
        a3.steady_allocs_per_s,
        a0.steady_allocs_per_s
    );
    assert!(
        a3.bound_age_mean_ms < a0.bound_age_mean_ms,
        "the levers cut the loop latency: A3 bound_age {:.0} vs A0 {:.0} ms",
        a3.bound_age_mean_ms,
        a0.bound_age_mean_ms
    );
    assert!(
        a3.stalls_steady <= a0.stalls_steady,
        "the levers never stall the stream more: A3 {} vs A0 {}",
        a3.stalls_steady,
        a0.stalls_steady
    );
}

/// **Little's law on the closed loop: when the stream stays recycle-bound
/// even post-fix, its ceiling is `spare ÷ L_lag`, and the levers move it
/// by exactly the latency they remove.** Lane 0's spare (128 blocks) is
/// below demand × the POST-fix latency, so every configuration runs
/// below the offered rate; the sustained rate must then agree with the
/// circulating inventory over the measured bound age (the §3.3
/// reconciliation — `held ÷ rate = L_lag` — on live gauges), and the
/// shipped configuration's ceiling must sit above the pre-campaign one
/// in the same ratio its latency sits below.
#[test]
fn a_still_bound_stream_runs_at_spare_over_latency_on_every_configuration() {
    let _serial = serial();
    let shape = LoopShape {
        label: "bound(spare=128,demand=20/s,storm=500/s,8m,staggered)",
        members: 8,
        lane_spare: 128,
        lane_demand_per_s: 20,
        storm_per_s: 500,
        duration_ms: 300_000,
        staggered: true,
        checkpoint_ms: 1_000,
    };
    let rows = run_loop_matrix(&shape);
    for r in &rows {
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!(r.forced, 0, "{}: no forced release", r.config);
        assert_eq!(r.fences, 0, "{}: no laggard fence", r.config);
        assert!(
            r.steady_allocs_per_s < shape.lane_demand_per_s as f64,
            "{}: the stream is recycle-bound ({:.2} < {} blk/s)",
            r.config,
            r.steady_allocs_per_s,
            shape.lane_demand_per_s
        );
        // Little's law: rate ≈ spare ÷ L_lag, with L_lag read as the
        // steady bound age (the inventory's own residence). Bucketed
        // gauges and the park slice's quantization leave ±30 %.
        let littles = shape.lane_spare as f64 / (r.bound_age_mean_ms / 1_000.0);
        assert!(
            (r.steady_allocs_per_s - littles).abs() <= 0.3 * littles,
            "{}: sustained {:.2} blk/s vs spare ÷ bound_age {:.2} (Little's law)",
            r.config,
            r.steady_allocs_per_s,
            littles
        );
    }
    let a0 = &rows[0];
    let a3 = &rows[3];
    assert!(
        a3.steady_allocs_per_s > a0.steady_allocs_per_s
            && a3.bound_age_mean_ms < a0.bound_age_mean_ms,
        "the shipped configuration's ceiling is above the pre-campaign one \
         because its latency is below (A3 {:.2} blk/s @ {:.0} ms vs A0 {:.2} @ {:.0})",
        a3.steady_allocs_per_s,
        a3.bound_age_mean_ms,
        a0.steady_allocs_per_s,
        a0.bound_age_mean_ms
    );
}

/// **Finding D4-1 (economy class, pinned as CURRENT behavior — a
/// follow-on that changes the divisor must flip this assertion red
/// first).** The re-based runway (KD-FG-10) reads ONE lane's reachable
/// supply but divides it by the RING's deferral rate, which on an
/// authority that `finish_free`s the whole fleet's shipped frees is the
/// FLEET's rate: `runway = lane_reachable ÷ fleet_rate`. A lane whose own
/// demand would take 200 s to spend its spare therefore reads a ≈ 8 s
/// runway under a 500 blk/s storm, and rung (a) asks every member the
/// floor cadence for the storm's whole duration. What that buys is
/// INVENTORY, not throughput: the stream was never recycle-bound (it runs
/// at its offered rate with or without the prods), but the parked
/// inventory falls by λ × (L_routine − L_floor) — the row below holds
/// ≈ 4.5 k blocks instead of ≈ 12 k (≈ 30 GiB of the fleet's spare not
/// parked). The cost is the floor beat on the lease lane for the storm's
/// duration (§5.8's "pressure-scoped" term becomes storm-scoped). Never a
/// correctness effect — rung (b)'s floor is one honest ack cycle and no
/// fence results — and structurally absent on a solo writer, whose ring
/// rate IS its allocation rate. The lane's own claim rate (the EWMA L5
/// keeps for the co-writer watermark) is the divisor that would make the
/// runway a per-lane forecast; whether the inventory is worth the beats
/// is the trade this row prices for the adjudication.
#[test]
fn a_mid_supply_lane_is_asked_at_the_floor_by_the_fleet_rate_divisor() {
    let _serial = serial();
    let shape = LoopShape {
        label: "midsupply(spare=4096,demand=20/s,storm=500/s,8m,staggered)",
        members: 8,
        lane_spare: 4_096,
        lane_demand_per_s: 20,
        storm_per_s: 500,
        duration_ms: 300_000,
        staggered: true,
        checkpoint_ms: 1_000,
    };
    let rows = run_loop_matrix(&shape);
    for r in &rows {
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!(r.forced, 0, "{}: no forced release", r.config);
        assert_eq!(r.fences, 0, "{}: no laggard fence", r.config);
        assert_eq!(r.stalls_steady, 0, "{}: never recycle-bound", r.config);
        assert!(
            (r.steady_allocs_per_s - shape.lane_demand_per_s as f64).abs() < 0.5,
            "{}: the stream runs at its offered rate regardless ({:.2})",
            r.config,
            r.steady_allocs_per_s
        );
    }
    // The passed-global runway (DEMAND=0) reads the foreign accumulation
    // and never prods; the lane-reachable one (DEMAND=1) prods at the
    // floor on every renewal of the steady window — the finding.
    assert_eq!(
        rows[0].prods_steady, 0,
        "A0: the passed-global runway is long"
    );
    assert!(
        rows[3].prods_steady >= rows[3].renewals_steady * 9 / 10,
        "A3: the fleet-rate divisor asks the floor on ≈ every renewal \
         (prods {} of {} renewals) — finding D4-1",
        rows[3].prods_steady,
        rows[3].renewals_steady
    );
}

/// **The residence histogram resets with the plane** (RED against the
/// tree at `27a396e1`): `160650e3` (audit A) gave every latency histogram
/// exact `count`/`sum_ns`/`mean_ns` words AFTER `reset_for_test` was
/// written, and the seam kept zeroing the buckets alone — so
/// `free_grace_residence_ms.mean_ns` bled from one contract into the
/// next (the same deterministic loop read a 13.9 s and a 23.9 s mean in
/// two process orders). A seam that resets HALF an instrument is a
/// measurement hazard, not a flake: every row's residence column rode
/// on it.
#[test]
fn the_residence_histogram_resets_with_the_plane() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-reset", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let ring = GraceRing::new(64);
    let label = clock.now_ms() + 1;
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(5_000, Ordering::SeqCst);
    ack(&owner, "r-reset", grant.epoch, label);
    assert_eq!(ring.harvest_with_supply(64, u64::MAX, u64::MAX).len(), 1);
    let hist = free_grace::stats_snapshot()["free_grace_residence_ms"].clone();
    assert_eq!(hist["count"].as_u64(), Some(1), "one release, one sample");
    assert!(hist["mean_ns"].as_u64().unwrap_or(0) >= 5_000_000_000);

    // The seam, then a fresh plane: EVERY word of the instrument is 0.
    free_grace::reset_for_test();
    membership::uninstall();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let hist = free_grace::stats_snapshot()["free_grace_residence_ms"].clone();
    assert_eq!(free_grace::residence_samples(), 0);
    assert_eq!(hist["count"].as_u64(), Some(0), "the exact count resets");
    assert_eq!(hist["sum_ns"].as_u64(), Some(0), "the exact sum resets");
    assert_eq!(hist["mean_ns"].as_u64(), Some(0), "the mean resets");
    let buckets = hist["buckets"].as_object().expect("bucket map");
    assert!(
        buckets.values().all(|v| v.as_u64() == Some(0)),
        "every bucket resets"
    );
}

/// **The uncoupled fleet (the 2026-08-30 GREEN s11-mpiio row's shape —
/// `.benchmarks/cloud/2026-08-30-094130`): the demand arm stays DARK and
/// the loop runs the ROUTINE beat, by design.** Lane 0's spare dwarfs its
/// demand × latency, so no stream is recycle-bound; site 0 never fires
/// (`demand_waits 0`), no prod is issued, and `bound_age` sits at the
/// routine composite (the §3.2 T1–T8 band, ≈ 24–29 s) under EVERY lever
/// configuration — which is what the field row read (23–29 s) and why
/// PR 5's gate (c) (`bound_age ≤ 12 s`) is a statement about a COUPLED
/// storm, not a healthy well-supplied fleet (the design's non-goal 4: the
/// routine beat is not retuned). Inventory = λ × L_lag, and the fleet
/// pays it in parked space, never in throughput.
#[test]
fn an_uncoupled_fleet_runs_the_routine_beat_and_the_demand_arm_stays_dark() {
    let _serial = serial();
    let shape = LoopShape {
        label: "uncoupled(spare=100000,demand=20/s,storm=500/s,8m,staggered)",
        members: 8,
        lane_spare: 100_000,
        lane_demand_per_s: 20,
        storm_per_s: 500,
        duration_ms: 300_000,
        staggered: true,
        checkpoint_ms: 1_000,
    };
    let rows = run_loop_matrix(&shape);
    let renew_ms = shipped_clocks().renew_interval.as_millis() as f64;
    for r in &rows {
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!(r.forced, 0, "{}: no forced release", r.config);
        assert_eq!(r.fences, 0, "{}: no laggard fence", r.config);
        assert_eq!(
            r.stalls_steady, 0,
            "{}: an uncoupled stream never stalls",
            r.config
        );
        assert_eq!(
            r.demand_waits, 0,
            "{}: site 0 is silent when no lane is at the trough",
            r.config
        );
        assert_eq!(
            r.prods_steady, 0,
            "{}: no prod on a long runway once the loop has filled",
            r.config
        );
        assert!(
            (r.steady_allocs_per_s - shape.lane_demand_per_s as f64).abs() < 0.5,
            "{}: the stream runs at its offered rate ({:.2} vs {} blk/s)",
            r.config,
            r.steady_allocs_per_s,
            shape.lane_demand_per_s
        );
        // The routine composite (§3.2 T1–T8): more than one renewal beat
        // (the min over members ages 10–20 s before the sweep republishes
        // it) and at most the routine fence bound.
        assert!(
            r.bound_age_mean_ms > renew_ms
                && r.bound_age_mean_ms <= free_grace::fence_bound_base_ms() as f64,
            "{}: the uncoupled loop runs the routine beat's composite \
             (bound_age mean {:.0} ms against a {renew_ms} ms beat)",
            r.config,
            r.bound_age_mean_ms
        );
    }
}

// ---------------------------------------------------------------------------
// Finding 29 (residual): the write path's allocation OUTLIVES a grace storm
// ---------------------------------------------------------------------------

/// Finding 29's residual (RED — compile-blocked: the bounded form is the
/// fix's own seam, the f30 precedent): the pressure ruling's own words
/// promise "this wait always ends by itself, because past the deadline
/// the laggard is fenced" — but NO CALLER performed the wait. The write
/// ladder took the first prompt `StorageFull` as a terminal verdict and
/// surfaced fsync EIO seconds into the storm (the f29 probe:
/// forced_releases 0, laggard_fences 0, EIO delivered), while the fence
/// machinery sat proven-but-unasked. The bounded form IS the promised
/// wait: park on the plane's own numbers, re-harvest as acks land, and
/// let the tightened deadline fence the laggard — release-or-evict,
/// never a starved writer over reclaimable space.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grace_storm_parks_the_bounded_allocation_until_the_fence() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    let cycle = free_grace::ack_cycle(owner.clocks());
    free_grace::arm_owner_plane_with(clock.clone(), cycle * 4, cycle);
    let _reader = join(&owner, "r-f29", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-f29-park").await;
    ba.set_capacity_bytes(4 * ba.chunk_size());
    let mut offs = Vec::new();
    for _ in 0..4 {
        offs.push(ba.allocate_block().await.expect("allocate"));
    }
    for o in &offs {
        ba.free_block(*o).await.expect("free");
    }
    assert_eq!(ba.grace_len(), 4, "the whole store is in grace");

    // The storm's clock: the owner instant advances past the pressure
    // deadline while the bounded allocation parks (the field shape — the
    // reader never answers, the deadline does).
    let advancer = {
        let ticks = ticks.clone();
        let step = (cycle.as_millis() as u64 / 4).max(1);
        tokio::spawn(async move {
            for _ in 0..64 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                ticks.fetch_add(step, Ordering::SeqCst);
            }
        })
    };

    let parks0 = free_grace::pressure_parks();
    let got = ba
        .allocate_block_grace_bounded()
        .await
        .expect("the bounded allocation outlives the storm (finding 29)");
    assert!(
        offs.contains(&got),
        "progress came from the grace supply, not thin air"
    );
    assert!(
        free_grace::laggard_fences() >= 1,
        "progress past the deadline is release-OR-EVICT: the laggard is \
         fenced, never silently bypassed"
    );
    assert!(
        free_grace::pressure_parks() > parks0,
        "the park engagement is counted (free_grace_pressure_parks)"
    );
    advancer.abort();
}

/// The honest-exhaustion arm is UNTOUCHED: a genuinely full store (empty
/// grace ring) refuses `StorageFull` promptly through the bounded form —
/// the park is only ever a wait for RECLAIMABLE space.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_genuinely_full_store_refuses_promptly_through_the_bounded_form() {
    let _serial = serial();
    let ba = allocator("grace-f29-honest").await;
    ba.set_capacity_bytes(2 * ba.chunk_size());
    let _a = ba.allocate_block().await.expect("allocate");
    let _b = ba.allocate_block().await.expect("allocate");
    assert_eq!(
        ba.grace_len(),
        0,
        "nothing is in grace — genuine exhaustion"
    );

    let t0 = std::time::Instant::now();
    let err = ba
        .allocate_block_grace_bounded()
        .await
        .expect_err("a genuinely full store refuses");
    assert!(
        matches!(&err, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull),
        "the verdict is StorageFull, got {err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "genuine exhaustion refuses PROMPTLY — the park is only for grace-held space"
    );
}

/// The wall backstop: a storm whose owner clock never advances (the
/// pathological frozen-plane shape) still ends — bounded by the plane's
/// own routine bound in wall time, loud, and the verdict stays the
/// honest `StorageFull`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frozen_plane_bounds_the_park_by_wall_time() {
    let _serial = serial();
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    // Tiny clocks: the wall backstop derives from the plane's bounds, so
    // small bounds keep this test fast.
    free_grace::arm_owner_plane_with(
        clock.clone(),
        Duration::from_millis(400),
        Duration::from_millis(200),
    );
    let _reader = join(&owner, "r-f29-frozen", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ba = allocator("grace-f29-frozen").await;
    ba.set_capacity_bytes(2 * ba.chunk_size());
    let a = ba.allocate_block().await.expect("allocate");
    let b = ba.allocate_block().await.expect("allocate");
    ba.free_block(a).await.expect("free");
    ba.free_block(b).await.expect("free");
    assert_eq!(ba.grace_len(), 2);

    let t0 = std::time::Instant::now();
    let err = ba
        .allocate_block_grace_bounded()
        .await
        .expect_err("a frozen plane cannot fence — the backstop refuses");
    assert!(
        matches!(&err, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull),
        "the verdict stays StorageFull, got {err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "the park is wall-bounded even when the owner clock is frozen"
    );
    assert_eq!(
        ba.grace_len(),
        2,
        "the backstop released nothing unacknowledged"
    );
}

// ===========================================================================
// The hold-time campaign (finding 15's remaining half — `.benchmarks/
// 2026-09-06-free-grace-hold-time.md`, design-free-grace-sustain §"Hold-time
// campaign"): with the supply leak closed (`380ea732`) the s11 fleet row
// HOLDS the supply in the ring — `free_grace_bound_age_ms` 8,994 with every
// cadence at its 1 Hz floor and nothing starving. The contracts below pin
// the decomposition instrument (WHERE the nine seconds go, per stage) and
// then each lever that removes a cadence term.
// ===========================================================================

/// Read one phase of the `free_grace_hold_phase_ns` export: `(count,
/// mean_ms)` — the histogram's exact words, never a bucket estimate.
fn hold_phase(name: &str) -> (u64, f64) {
    let snap = free_grace::stats_snapshot();
    let phase = &snap["free_grace_hold_phase_ns"][name];
    assert!(
        phase.is_object(),
        "free_grace_hold_phase_ns.{name} is exported on the owner side (got {phase})"
    );
    (
        phase["count"].as_u64().unwrap_or(0),
        phase["mean_ns"].as_u64().unwrap_or(0) as f64 / 1e6,
    )
}

/// **Contract 23 — the hold decomposes into three stages that sum to the
/// total, per offset.** An offset deferred at owner instant `t` (label
/// `t+1`) is stamped at release with `defer→checkpointed` (the first
/// checkpoint the authority completed AFTER the defer), `checkpointed→
/// min_acked` (the first bound publish that covered the label — every
/// member acknowledged past it) and `min_acked→released` (the harvest);
/// `total` is `release − t`. The three stages are exact-sum with the
/// total, and the instrument is READ, never inferred: the checkpoint
/// instant comes from the KV checkpoint hook, the covering instant from
/// `publish_bound`'s own advance.
#[test]
fn the_hold_decomposes_into_three_stages_that_sum_to_the_total() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let a = join(&owner, "r-hold-a", MemberRole::Reader);
    let b = join(&owner, "r-hold-b", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    let ring = GraceRing::new(1024);
    let t0 = clock.now_ms();
    let label = t0 + 1;
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));

    // The writer's next checkpoint lands 700 ms after the defer.
    ticks.fetch_add(700, Ordering::SeqCst);
    free_grace::note_checkpoint_completed(Duration::from_millis(20));
    assert_eq!(
        free_grace::checkpoint_marks(),
        1,
        "the hook recorded one mark"
    );

    // Reader A acknowledges at +3,000 (the bound stays 0: B has not).
    ticks.fetch_add(2_300, Ordering::SeqCst);
    ack(&owner, "r-hold-a", a.epoch, label);
    assert!(
        ring.harvest_with_supply(64, u64::MAX, u64::MAX).is_empty(),
        "one of two readers acknowledging releases nothing"
    );
    // Reader B acknowledges a FRESHER label at +5,000: the bound advances
    // past the offset's label — the covering instant.
    ticks.fetch_add(2_000, Ordering::SeqCst);
    ack(&owner, "r-hold-b", b.epoch, t0 + 2_001);
    // The harvest runs 400 ms later (the allocation funnel's next visit).
    ticks.fetch_add(400, Ordering::SeqCst);
    let released = ring.harvest_with_supply(64, u64::MAX, u64::MAX);
    assert_eq!(released.len(), 1, "the covered offset releases");

    let (n_ck, ck) = hold_phase("defer_checkpointed");
    let (n_ack, ackd) = hold_phase("checkpointed_min_acked");
    let (n_rel, rel) = hold_phase("min_acked_released");
    let (n_tot, tot) = hold_phase("total");
    assert_eq!(
        (n_ck, n_ack, n_rel, n_tot),
        (1, 1, 1, 1),
        "one sample per stage per release"
    );
    assert_eq!(
        ck as u64, 700,
        "defer→checkpointed = the first checkpoint after the defer"
    );
    assert_eq!(
        ackd as u64, 4_300,
        "checkpointed→min_acked = the first bound publish covering the label (at +5,000)"
    );
    assert_eq!(rel as u64, 400, "min_acked→released = the harvest's visit");
    assert_eq!(tot as u64, 5_400, "total = release − defer");
    assert_eq!(
        (ck + ackd + rel) as u64,
        tot as u64,
        "the three stages are exact-sum with the total"
    );
}

/// **Contract 24 — the per-member acknowledgement lag names the binding
/// member.** The bound is a MIN over members, so the loop's latency is
/// the slowest member's; `free_grace_member_ack_lag_ms` publishes
/// `now − acked` per live member as max/mean/min (the census itself rides
/// the `SQUEEZEFS_STATS_KEY_CENSUS` gate — it names peers). A member that
/// has acknowledged nothing reads its whole membership as lag.
#[test]
fn member_ack_lag_names_the_binding_member() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let t0 = clock.now_ms();
    let a = join(&owner, "r-lag-a", MemberRole::Reader);
    let b = join(&owner, "r-lag-b", MemberRole::Reader);
    owner.refresh_free_grace_bound();

    ticks.fetch_add(3_000, Ordering::SeqCst);
    ack(&owner, "r-lag-a", a.epoch, t0 + 1);
    ticks.fetch_add(2_000, Ordering::SeqCst);
    ack(&owner, "r-lag-b", b.epoch, t0 + 2_001);
    ticks.fetch_add(400, Ordering::SeqCst);
    // now = t0 + 5,400: A is 5,399 behind, B 3,399.
    let lags = owner.member_ack_lags();
    let mut by_id: std::collections::BTreeMap<String, u64> = lags.into_iter().collect();
    assert_eq!(
        by_id.remove("r-lag-a"),
        Some(5_399),
        "A's lag = now − its acked label"
    );
    assert_eq!(
        by_id.remove("r-lag-b"),
        Some(3_399),
        "B's lag = now − its acked label"
    );
    assert!(by_id.is_empty(), "exactly the live members are named");

    let snap = free_grace::stats_snapshot();
    let agg = &snap["free_grace_member_ack_lag_ms"];
    assert_eq!(
        agg["max"].as_u64(),
        Some(5_399),
        "max = the binding member (A)"
    );
    assert_eq!(agg["min"].as_u64(), Some(3_399));
    assert_eq!(agg["mean"].as_u64(), Some(4_399));
    assert_eq!(agg["members"].as_u64(), Some(2));

    // A member that acknowledged nothing: its lag is its whole membership.
    let _c = join(&owner, "r-lag-c", MemberRole::Reader);
    ticks.fetch_add(600, Ordering::SeqCst);
    let lags: std::collections::BTreeMap<String, u64> =
        owner.member_ack_lags().into_iter().collect();
    assert_eq!(
        lags.get("r-lag-c"),
        Some(&600),
        "acked nothing ⇒ lag = now − joined"
    );
}

/// **Contract 25 — the KV checkpoint task marks the hold ledger.** The
/// `defer→checkpointed` stage is READ off the checkpoint that actually
/// ran: `KvMetaBackend::checkpoint_now` (the same cycle the cadence task
/// runs) records one mark on the armed owner plane — and none when no
/// plane is armed (the solo mount's cost stays one relaxed load).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kv_checkpoint_marks_the_hold_ledger() {
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
    use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
    use squeezefs::meta_backend::Metadata;
    let _serial = serial();
    let file = tempfile::NamedTempFile::new().expect("temp volume");
    file.as_file()
        .set_len(64 * 1024 * 1024)
        .expect("size volume");
    format_v3(
        file.path(),
        64 * 1024 * 1024,
        &FormatV3Options {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    be.create(1, "f", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    assert_eq!(
        free_grace::checkpoint_marks(),
        0,
        "no plane ⇒ no mark (one relaxed load)"
    );

    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let _r = join(&owner, "r-ckpt", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    be.create(1, "g", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    assert_eq!(
        free_grace::checkpoint_marks(),
        1,
        "an armed plane records the checkpoint"
    );
    be.shutdown().await.expect("shutdown");
}

/// The finding-15 fleet shape on the closed loop: the s11 venue's 8
/// members, the shipped 1 s checkpoint / pass / floor cadences, a lane-0
/// stream at 80 MiB/s against a 500 blk/s storm.
fn fleet_cadence_shape(label: &'static str, lane_spare: u64) -> LoopShape {
    LoopShape {
        label,
        members: 8,
        lane_spare,
        lane_demand_per_s: 20,
        storm_per_s: 500,
        duration_ms: 300_000,
        staggered: true,
        checkpoint_ms: 1_000,
    }
}

/// **Contract 26 — the hold at fleet cadences reproduces in-process and
/// decomposes exactly.** Every cadence at its 1 Hz floor (checkpoint,
/// pass, prodded beat), the D-4 levers on, the hold-time levers off — the
/// 2026-09-05 binary's shape: `bound_age` reads 6–9 s (the fleet's 8,994
/// ms), `defer→checkpointed` is bounded by one checkpoint period, the
/// harvest stage is below one pass, and the bulk sits in
/// `checkpointed→min_acked` — the readers' qualify + drain windows plus
/// the beat/pass/composition quantization. The three stage means sum to
/// the residence mean (exact-sum over a fully placed population).
#[test]
fn the_hold_at_fleet_cadences_is_the_sum_of_its_stages() {
    let _serial = serial();
    let shape = fleet_cadence_shape("hold-fleet(spare=256,ckpt=1s,pass=1s,floor=1s,8m)", 256);
    let row = run_closed_loop(&shape, Levers::d4(true, true));
    println!("{}", row.render(&shape));
    assert_eq!(row.deferrals, row.releases + row.held_end, "closure");
    assert_eq!(
        (row.forced, row.fences),
        (0, 0),
        "no fence on a healthy fleet"
    );
    assert_eq!(
        row.hold_unplaced, 0,
        "every release places all three stages"
    );
    assert!(
        (6_000.0..=9_500.0).contains(&row.bound_age_mean_ms),
        "the red shape: bound_age {:.0} ms at 1 Hz cadences (fleet 8,994)",
        row.bound_age_mean_ms
    );
    assert!(
        row.hold_defer_ck_ms <= shape.checkpoint_ms as f64,
        "defer→checkpointed ≤ one checkpoint period ({:.0} ms)",
        row.hold_defer_ck_ms
    );
    assert!(
        row.hold_acked_rel_ms < 1_000.0,
        "min_acked→released is below one pass: the harvest runs per free ({:.0} ms)",
        row.hold_acked_rel_ms
    );
    assert!(
        row.hold_ck_acked_ms > 5_000.0,
        "checkpointed→min_acked carries the qualify + drain windows and the quantization ({:.0} ms)",
        row.hold_ck_acked_ms
    );
    let sum = row.hold_defer_ck_ms + row.hold_ck_acked_ms + row.hold_acked_rel_ms;
    assert!(
        (sum - row.residence_mean_ms).abs() <= 1.0,
        "exact-sum: {sum:.1} ms vs residence mean {:.1} ms",
        row.residence_mean_ms
    );
    assert!(
        (row.hold_ms as f64 - row.residence_mean_ms).abs() <= 0.25 * row.residence_mean_ms,
        "the live hold gauge tracks the residence ({} vs {:.0} ms)",
        row.hold_ms,
        row.residence_mean_ms
    );
    assert!(
        row.ack_lag_max_ms as f64 >= row.ack_lag_mean_ms as f64 && row.ack_lag_mean_ms > 6_000,
        "the per-member lag names the composition: max {} mean {}",
        row.ack_lag_max_ms,
        row.ack_lag_mean_ms
    );
}

/// **Contract 27 — the capacity law: a lane exhausts exactly when
/// `hold × churn + live > cap/W`.** The stream's spare is `cap/W − live`;
/// with the hold `H` MEASURED on the loop (`free_grace_hold_ms`), a spare
/// below `demand × H` stalls (`free_grace_alloc_stalls` grows — the
/// fleet's lane ENOSPC), a spare above it never does. Both halves on the
/// same binary, the same cadences, the same measured `H`.
#[test]
fn a_lane_exhausts_exactly_when_hold_times_churn_exceeds_its_spare() {
    let _serial = serial();
    let probe = run_closed_loop(
        &fleet_cadence_shape("capacity-probe(spare=256)", 256),
        Levers::d4(true, true),
    );
    let hold_s = probe.hold_ms as f64 / 1_000.0;
    assert!(hold_s > 1.0, "the probe measured a hold ({hold_s:.2} s)");
    let inflight = 20.0 * hold_s; // demand × hold, blocks
    let below = (0.6 * inflight) as u64;
    let above = (1.5 * inflight) as u64;
    let starved = run_closed_loop(
        &fleet_cadence_shape("capacity-below(spare=0.6×demand×hold)", below),
        Levers::d4(true, true),
    );
    let fed = run_closed_loop(
        &fleet_cadence_shape("capacity-above(spare=1.5×demand×hold)", above),
        Levers::d4(true, true),
    );
    println!(
        "ROW capacity: hold {hold_s:.2} s × demand 20 blk/s = {inflight:.0} blocks in flight; \
         spare {below} → stalls {} ({:.1} blk/s); spare {above} → stalls {} ({:.1} blk/s)",
        starved.stalls_steady,
        starved.steady_allocs_per_s,
        fed.stalls_steady,
        fed.steady_allocs_per_s
    );
    assert!(
        starved.stalls_steady > 0 && starved.steady_allocs_per_s < 20.0,
        "spare below demand × hold EXHAUSTS the lane (stalls {}, {:.2} blk/s)",
        starved.stalls_steady,
        starved.steady_allocs_per_s
    );
    assert_eq!(
        fed.stalls_steady, 0,
        "spare above demand × hold never exhausts"
    );
    assert!(
        (fed.steady_allocs_per_s - 20.0).abs() < 0.5,
        "…and the stream runs at its offered rate ({:.2} blk/s)",
        fed.steady_allocs_per_s
    );
}

/// **Contract 28 — lever (b): a promotion carries itself home at once.**
/// The ack used to wait for the member's NEXT routine beat (≤ one prodded
/// cadence — the T5 carry term); now `reader_pass_completed`'s promotion
/// wakes the renewal loop (`membership::renewal_wake`), so the carry is
/// one round trip. `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL=0` restores the
/// beat-only carriage verbatim (no wake, no count).
#[test]
fn a_promotion_carries_itself_home_at_once() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-carry", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let anchor = clock.now_ms();
    let session = Arc::new(MemberSession::adopt(
        "r-carry",
        MemberRole::Reader,
        &grant,
        anchor,
        clock.clone(),
    ));
    membership::install_member(Arc::clone(&session));
    // Drain any stale permit: the wake must come from THIS promotion.
    assert!(
        !membership::renewal_wake().notified_raw().enable(),
        "no renewal request is pending before the promotion"
    );

    let drive = |ticks: &Arc<AtomicU64>| -> Option<u64> {
        // Qualify: an advancing pass ≥ learn + staleness + skew; then the
        // drain window; then the promoting pass.
        ticks.fetch_add(2_100, Ordering::SeqCst);
        assert!(free_grace::reader_pass_completed(clock.now_ms(), true).is_none());
        ticks.fetch_add(4_100, Ordering::SeqCst);
        free_grace::reader_pass_completed(clock.now_ms(), true)
    };

    free_grace::test_set_ack_renewal(Some(true));
    let promoted = drive(&ticks);
    assert_eq!(
        promoted,
        Some(grant.granted_at_owner_ms),
        "the label promotes"
    );
    assert!(
        membership::renewal_wake().notified_raw().enable(),
        "the promotion woke the renewal loop (a permit is stored)"
    );
    assert_eq!(
        free_grace::ack_renewals(),
        1,
        "counted: free_grace_ack_renewals"
    );
    assert!(free_grace::test_clear_ack_renewal());

    // The carriage renewal itself: the lease renews and the ack travels,
    // but the routine beat keeps its schedule and the label stays the
    // routine renewal's — a label learned just after a pass would qualify
    // a whole pass later than one learned at the beat's own phase.
    let learned_before = session.learned_label();
    let beat_before = session.renew_at_ms();
    ticks.fetch_add(100, Ordering::SeqCst);
    let carriage_at = clock.now_ms();
    let renewed = match owner.renew("r-carry", session.epoch(), session.acked_free_epoch()) {
        RenewOutcome::Renewed(g) => g,
        other => panic!("admitted: {other:?}"),
    };
    session.renewed_carriage(&renewed, carriage_at);
    assert_eq!(
        session.learned_label(),
        learned_before,
        "carriage learns no label"
    );
    assert_eq!(
        session.renew_at_ms(),
        beat_before,
        "carriage keeps the routine beat"
    );
    assert_eq!(
        session.t_self_deadline_ms(),
        carriage_at + renewed.t_self_ms(),
        "…and renews the lease"
    );
    // A prod riding the carriage grant IS honoured (the beat comes forward).
    let mut prodded = renewed;
    prodded.renew_ms = 1_000;
    session.renewed_carriage(&prodded, carriage_at);
    assert_eq!(
        session.renew_at_ms(),
        carriage_at + 1_000,
        "a prod brings the beat forward"
    );

    // The lever off: the ack waits for the beat (no wake, no count).
    free_grace::reset_for_test();
    membership::uninstall();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-carry-off", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let session = Arc::new(MemberSession::adopt(
        "r-carry-off",
        MemberRole::Reader,
        &grant,
        clock.now_ms(),
        clock.clone(),
    ));
    membership::install_member(session);
    // Consume the permit the first half stored.
    let _ = membership::renewal_wake().notified_raw().enable();
    free_grace::test_set_ack_renewal(Some(false));
    assert!(drive(&ticks).is_some(), "the ladder still promotes");
    assert_eq!(
        free_grace::ack_renewals(),
        0,
        "ACK_RENEWAL=0: nothing counted"
    );
    assert!(free_grace::test_clear_ack_renewal());
}

/// **Contract 29 — lever (d): a binding member's advancing ack refreshes
/// the bound on the next harvest, not on the next floor beat.** The bound
/// is a min; only a member whose recorded ack sat at or below the
/// published bound can move it, so `renew` marks the bound dirty for
/// exactly those (one compare — no O(members) work in the plane's hot
/// op) and the harvest recomputes when the mark is set, rate-limited by
/// `floor ÷ members` (the rate the min can change at) and by twice the
/// measured scan cost. `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK=0` leaves the
/// refresh on the floor cadence / the sweep verbatim.
#[test]
fn a_binding_members_ack_refreshes_the_bound_on_the_next_harvest() {
    let _serial = serial();
    for lever in [true, false] {
        free_grace::reset_for_test();
        membership::uninstall();
        free_grace::test_set_refresh_on_ack(Some(lever));
        let (clock, ticks) = manual_clock();
        let owner = armed_owner(&clock);
        free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
        let a = join(&owner, "r-d-a", MemberRole::Reader);
        let b = join(&owner, "r-d-b", MemberRole::Reader);
        owner.refresh_free_grace_bound();
        let ring = GraceRing::new(1024);
        let label = clock.now_ms() + 1;
        assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
        ticks.fetch_add(3_000, Ordering::SeqCst);
        // A acknowledges on its renewal (no sweep, no refresh): the bound
        // stays 0 — B is the binding member.
        assert!(matches!(
            owner.renew("r-d-a", a.epoch, label),
            RenewOutcome::Renewed(_)
        ));
        assert!(ring.harvest_with_supply(64, u64::MAX, u64::MAX).is_empty());
        assert_eq!(free_grace::bound(), 0, "B has acknowledged nothing");
        // B's ack arrives on ITS renewal: the binding member moved.
        ticks.fetch_add(1_000, Ordering::SeqCst);
        assert!(matches!(
            owner.renew("r-d-b", b.epoch, label),
            RenewOutcome::Renewed(_)
        ));
        let refreshes0 = free_grace::bound_refreshes_on_ack();
        let released = ring.harvest_with_supply(64, u64::MAX, u64::MAX);
        if lever {
            assert_eq!(
                released.len(),
                1,
                "REFRESH_ON_ACK=1: the harvest recomputed the min and released the offset"
            );
            assert_eq!(free_grace::bound(), label);
            assert_eq!(
                free_grace::bound_refreshes_on_ack() - refreshes0,
                1,
                "counted"
            );
        } else {
            assert!(
                released.is_empty(),
                "REFRESH_ON_ACK=0: the bound waits for the sweep (verbatim)"
            );
            assert_eq!(free_grace::bound(), 0);
            assert_eq!(free_grace::bound_refreshes_on_ack(), 0);
            owner.refresh_free_grace_bound();
            assert_eq!(ring.harvest_with_supply(64, u64::MAX, u64::MAX).len(), 1);
        }
        assert!(free_grace::test_clear_refresh_on_ack());
    }
}

/// **Contract 30 — the hold-time levers on the fleet-cadence loop.** With
/// the D-4 levers on, (b) and (d) each cut the hold and compose: H3 (the
/// shipped configuration) reads a lower `bound_age` than A3 with every
/// safety law intact — closure, `forced = fences = 0`, the stream never
/// stalled more — and the engagement gauges account for the mechanism
/// (`ack_renewals`, `bound_refreshes_on_ack`). The rows are the note's.
#[test]
fn the_hold_time_levers_cut_the_hold_and_never_fence() {
    let _serial = serial();
    let shape = fleet_cadence_shape("hold-levers(spare=256,ckpt=1s,8m)", 256);
    let rows = [
        run_closed_loop(&shape, Levers::d4(true, true)),
        run_closed_loop(
            &shape,
            Levers {
                ack_renewal: true,
                ..Levers::d4(true, true)
            },
        ),
        run_closed_loop(
            &shape,
            Levers {
                refresh_on_ack: true,
                ..Levers::d4(true, true)
            },
        ),
        run_closed_loop(&shape, Levers::h3()),
    ];
    for r in &rows {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
    }
    let (a3, h1, h2, h3) = (&rows[0], &rows[1], &rows[2], &rows[3]);
    assert!(
        h1.ack_renewals > 0,
        "H1: promotions carried themselves home"
    );
    assert!(
        h2.refreshes_on_ack > 0,
        "H2: binding acks refreshed the bound"
    );
    assert!(
        h1.bound_age_mean_ms < a3.bound_age_mean_ms,
        "lever (b) cuts the hold: {:.0} vs {:.0} ms",
        h1.bound_age_mean_ms,
        a3.bound_age_mean_ms
    );
    assert!(
        h2.bound_age_mean_ms < a3.bound_age_mean_ms,
        "lever (d) cuts the hold: {:.0} vs {:.0} ms",
        h2.bound_age_mean_ms,
        a3.bound_age_mean_ms
    );
    assert!(
        h3.bound_age_mean_ms <= h1.bound_age_mean_ms.min(h2.bound_age_mean_ms),
        "the levers compose (never undo each other): H3 {:.0} vs H1 {:.0} / H2 {:.0} ms",
        h3.bound_age_mean_ms,
        h1.bound_age_mean_ms,
        h2.bound_age_mean_ms
    );
    assert!(
        h3.stalls_steady <= a3.stalls_steady,
        "the levers never stall the stream more"
    );
}

/// **Contract 31 — the writer's checkpoint cadence alone does not move
/// the hold (candidate lever (a), measured inert).** The reader qualifies
/// a label on a TIME bound — a pass beginning ≥ `learned + staleness +
/// skew`, where the staleness bound derives from the checkpoint CEILING
/// constant — not on observing the checkpoint; the checkpoint only has to
/// have landed by then, and at the shipped cadences every pass already
/// advances (the field: epochs ≈ polls on the storm volume). So a writer
/// checkpointing twice as often reads the same `bound_age` to the tick,
/// while a writer whose checkpoints land LESS often than the passes (the
/// non-advancing-pass shape) pays the qualification rounding for it. A
/// checkpoint-cadence lever pays only as the writer→member composite
/// that also shortens the pass and beat floors — the adjudication item the
/// note names, not a lever this campaign lands.
#[test]
fn a_faster_checkpoint_cadence_alone_leaves_the_hold_where_it_was() {
    let _serial = serial();
    let shipped = fleet_cadence_shape("ckpt-1000", 256);
    let halved = LoopShape {
        label: "ckpt-500",
        checkpoint_ms: 500,
        ..shipped
    };
    let sparse = LoopShape {
        label: "ckpt-1650(epochs/polls≈0.6)",
        checkpoint_ms: 1_650,
        ..shipped
    };
    let levers = Levers::h3();
    let a = run_closed_loop(&shipped, levers);
    let b = run_closed_loop(&halved, levers);
    let c = run_closed_loop(&sparse, levers);
    for (shape, r) in [(&shipped, &a), (&halved, &b), (&sparse, &c)] {
        println!("{}", r.render(shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            shape.label
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", shape.label);
    }
    assert!(
        (a.bound_age_mean_ms - b.bound_age_mean_ms).abs() <= 1.0,
        "halving the checkpoint period moves nothing: {:.0} vs {:.0} ms",
        a.bound_age_mean_ms,
        b.bound_age_mean_ms
    );
    assert!(
        b.hold_defer_ck_ms < a.hold_defer_ck_ms,
        "…only the defer→checkpointed stage shortens ({:.0} vs {:.0} ms), in the shadow of the qualify window",
        b.hold_defer_ck_ms,
        a.hold_defer_ck_ms
    );
    assert!(
        c.bound_age_mean_ms > a.bound_age_mean_ms + 250.0,
        "checkpoints sparser than the passes DO cost the hold: {:.0} vs {:.0} ms",
        c.bound_age_mean_ms,
        a.bound_age_mean_ms
    );
}

// ===========================================================================
// The ladder re-derivation (2026-09-06, user decision — finding 15 term 1,
// `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`): the two
// DERIVED windows the hold-time campaign named as adjudication items 1–3.
// The windows stay pinned against DEMAND (KD-FG-11: elastic passes never
// shorten them); what changes is their DERIVATION — each term traced to
// the writer's machinery instead of to a poll interval that stood in for it.
// ===========================================================================

/// The reader's pending (qualified, undrained) label off its own stats
/// face — 0 when nothing is qualified.
fn reader_pending_label() -> u64 {
    free_grace::stats_snapshot()["free_grace_reader_pending_label"]
        .as_u64()
        .unwrap_or(0)
}

/// **Contract 32 — item 1: the qualify window is the writer's checkpoint
/// ceiling plus the skew bound; the poll interval is not in it.** Gate 2
/// argues the pass's ledger read must post-date a checkpoint containing
/// the dereference. The dereference commit precedes the free (the reclaim
/// queue sits between), so it lands in the ledger within the writer's
/// checkpoint LANDING ceiling of the label — the cadence trigger
/// (`CHECKPOINT_MAX_AGE_MS`) plus the two tick-granularity terms the
/// trigger is evaluated behind (the tick wait and the bounded maintenance
/// drain, each ≤ one checkpoint-task period). The `P` in the staleness
/// bound is the READER's poll interval — "how stale can a reader be" — but
/// for qualification the pass itself IS the poll, so it was counted twice.
/// `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING=0` restores `staleness + skew`
/// verbatim.
#[test]
fn the_qualify_window_is_the_writers_ceiling_plus_skew_and_carries_no_poll_interval() {
    use squeezefs::meta_backend::kv::checkpoint::{
        checkpoint_landing_ceiling_ms, checkpoint_tick_period_ms, CHECKPOINT_MAX_AGE_MS,
    };
    let _serial = serial();
    // The derivation, drift-is-red: the shipped 50 ms flush tick ⇒
    // 1,000 + 2 × 50 = 1,100 ms; strict mode (0) reads the task's own
    // 100 ms tick; a 5 s flush venue's tick IS the landing term.
    assert_eq!(checkpoint_tick_period_ms(50), 50);
    assert_eq!(checkpoint_tick_period_ms(0), 100);
    assert_eq!(checkpoint_tick_period_ms(5_000), 5_000);
    assert_eq!(
        checkpoint_landing_ceiling_ms(50),
        CHECKPOINT_MAX_AGE_MS as u64 + 100
    );
    assert_eq!(checkpoint_landing_ceiling_ms(0), 1_200);
    assert_eq!(checkpoint_landing_ceiling_ms(5_000), 11_000);

    // The pure rule both the product and the loop model call.
    assert_eq!(
        free_grace::qualify_lag_ms(true, 1_100, 2_000, 22),
        1_122,
        "lever on: ceiling + skew"
    );
    assert_eq!(
        free_grace::qualify_lag_ms(false, 1_100, 2_000, 22),
        2_022,
        "lever off: the pre-change staleness + skew verbatim"
    );

    // End to end on a real member session: the accessor is the shipped
    // derivation, it sits strictly below the staleness bound (the poll
    // interval is NOT in it), and a pass beginning exactly at
    // `learned + ceiling + skew` qualifies while one 1 ms earlier does not.
    let (clock, ticks) = manual_clock();
    // No free-grace OWNER plane in this process: the ladder is the
    // member's alone, and the reader stats face is what is read below.
    let owner = armed_owner(&clock);
    let grant = join(&owner, "r-qualify", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let anchor = clock.now_ms();
    let session = Arc::new(MemberSession::adopt(
        "r-qualify",
        MemberRole::Reader,
        &grant,
        anchor,
        clock.clone(),
    ));
    membership::install_member(Arc::clone(&session));
    let staleness = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
    let ceiling = session.checkpoint_ceiling_ms();
    assert_eq!(
        ceiling,
        checkpoint_landing_ceiling_ms(50),
        "the member's accessor IS the writer's landing derivation on the shipped tree"
    );
    assert!(
        ceiling < staleness,
        "the ceiling ({ceiling}) carries no poll interval; the staleness bound ({staleness}) does"
    );
    let (label, learned_at) = session.learned_label();
    let qualify = ceiling + session.skew_max_ms();

    free_grace::test_set_qualify_ceiling(Some(true));
    // One ms too early: a checkpoint containing the dereference need not
    // have landed.
    ticks.fetch_add(qualify - 1, Ordering::SeqCst);
    assert_eq!(
        free_grace::reader_pass_completed(clock.now_ms(), true),
        None
    );
    assert_eq!(
        reader_pending_label(),
        0,
        "nothing qualified a millisecond before the ceiling"
    );
    // Exactly at it: the pass qualifies (the candidate now awaits its
    // drain — this contract leaves the drain where item 1 found it).
    ticks.fetch_add(1, Ordering::SeqCst);
    let pass_start = clock.now_ms();
    assert_eq!(pass_start, learned_at + qualify);
    assert_eq!(free_grace::reader_pass_completed(pass_start, true), None);
    assert_eq!(
        reader_pending_label(),
        label,
        "the pass at learned + ceiling + skew QUALIFIED the label"
    );
    assert_eq!(
        free_grace::stats_snapshot()["free_grace_qualify_lag_ms"],
        qualify,
        "the derivation in force is published"
    );
    assert!(free_grace::test_clear_qualify_ceiling());

    // The lever off: the same pass instant does NOT qualify (the
    // pre-change `staleness + skew` window), pinned against the same
    // session so the A/B is exact.
    free_grace::reset_for_test();
    membership::uninstall();
    free_grace::test_set_qualify_ceiling(Some(false));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    let grant = join(&owner, "r-qualify-off", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let anchor = clock.now_ms();
    let session = Arc::new(MemberSession::adopt(
        "r-qualify-off",
        MemberRole::Reader,
        &grant,
        anchor,
        clock.clone(),
    ));
    membership::install_member(Arc::clone(&session));
    ticks.fetch_add(qualify, Ordering::SeqCst);
    assert_eq!(
        free_grace::reader_pass_completed(clock.now_ms(), true),
        None
    );
    assert_eq!(
        reader_pending_label(),
        0,
        "lever off: learned + ceiling + skew is inside the old window — nothing qualifies"
    );
    assert_eq!(
        free_grace::stats_snapshot()["free_grace_qualify_lag_ms"],
        staleness + session.skew_max_ms(),
    );
    assert!(free_grace::test_clear_qualify_ceiling());
}

/// **Contract 33 — item 1 on the fleet-cadence loop: the hold drops by
/// the double-counted term and nothing else moves.** The 2026-09-06
/// hold-time binary (H3) against H3 + the re-derived qualify window: the
/// `bound_age` falls by ≈ the removed 900 ms (the poll interval's 1,000 ms
/// less the 100 ms of tick granularity the honest landing bound keeps),
/// closure exact, `forced = fences = 0`, the stream never stalled more.
/// The rows are the note's.
#[test]
fn the_rederived_qualify_window_cuts_the_hold_by_the_double_counted_term() {
    let _serial = serial();
    let shape = fleet_cadence_shape("rederive-qualify(spare=256,ckpt=1s,8m)", 256);
    let before = run_closed_loop(&shape, Levers::h3());
    let after = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            ..Levers::h3()
        },
    );
    for r in [&before, &after] {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
    }
    let cut = before.bound_age_mean_ms - after.bound_age_mean_ms;
    assert!(
        (500.0..=1_300.0).contains(&cut),
        "the qualify re-derivation removes ≈ the double-counted second: {:.0} → {:.0} ms (−{cut:.0})",
        before.bound_age_mean_ms,
        after.bound_age_mean_ms
    );
    assert!(
        after.stalls_steady <= before.stalls_steady,
        "never stalls the stream more"
    );
}

/// **Contract 34 — item 2: the drain window carries no cache TTL once the
/// layout cache is epoch-step stamped.** Gate 3's `S` term existed because
/// the daemon's layout/attr caches (reader TTL = `S`) could serve a
/// pre-step binding for up to `S` after the purge. With every layout-cache
/// entry stamped with the purge generation it was resolved under and an
/// older stamp a MISS (`tests/reader_layout_step_gate_tests.rs` pins the
/// gate itself), no daemon cache can serve a pre-step binding after the
/// step, so the drain is `D_purge` alone; `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0`
/// restores `S + D_purge` verbatim. End to end on a real member session:
/// the promotion lands at `qualify_pass + D_purge`, where the retired
/// window would still be waiting out `S`.
#[test]
fn the_drain_window_carries_no_cache_ttl_once_entries_are_step_stamped() {
    let _serial = serial();
    // The pure rule both the product and the loop model call.
    assert_eq!(free_grace::drain_lag_ms(true, false, 2_000, 2_000), 2_000);
    assert_eq!(free_grace::drain_lag_ms(false, false, 2_000, 2_000), 4_000);

    for lever in [true, false] {
        free_grace::reset_for_test();
        membership::uninstall();
        squeezefs::ro_coherence::test_set_drain_epoch_stamp(Some(lever));
        let (clock, ticks) = manual_clock();
        let owner = armed_owner(&clock);
        let grant = join(&owner, "r-drain", MemberRole::Reader);
        owner.refresh_free_grace_bound();
        let anchor = clock.now_ms();
        let session = Arc::new(MemberSession::adopt(
            "r-drain",
            MemberRole::Reader,
            &grant,
            anchor,
            clock.clone(),
        ));
        membership::install_member(Arc::clone(&session));
        let (label, _) = session.learned_label();
        let staleness = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
        let d_purge = session.d_purge_ms();
        let qualify = session.checkpoint_ceiling_ms() + session.skew_max_ms();

        // Qualify.
        ticks.fetch_add(qualify, Ordering::SeqCst);
        let pass_start = clock.now_ms();
        assert_eq!(free_grace::reader_pass_completed(pass_start, true), None);
        assert_eq!(
            reader_pending_label(),
            label,
            "qualified, awaiting the drain"
        );
        assert_eq!(
            free_grace::stats_snapshot()["free_grace_drain_lag_ms"],
            if lever { d_purge } else { staleness + d_purge },
            "the drain window in force is published"
        );
        // `D_purge` elapses: the re-derived drain promotes; the retired
        // one is still waiting out the caches' TTL.
        ticks.fetch_add(d_purge, Ordering::SeqCst);
        let at_d_purge = free_grace::reader_pass_completed(clock.now_ms(), false);
        if lever {
            assert_eq!(
                at_d_purge,
                Some(label),
                "lever on: the drain is D_purge — promoted"
            );
        } else {
            assert_eq!(at_d_purge, None, "lever off: S still owed");
            ticks.fetch_add(staleness, Ordering::SeqCst);
            assert_eq!(
                free_grace::reader_pass_completed(clock.now_ms(), false),
                Some(label),
                "lever off: promoted at S + D_purge verbatim"
            );
        }
        assert!(squeezefs::ro_coherence::test_clear_drain_epoch_stamp());
    }
}

/// **Contract 35 — item 2 on the fleet-cadence loop: the drain's `S`
/// leaves the hold.** R1 (item 1) against R1 + the step-stamped drain:
/// `bound_age` falls by ≈ the 2,000 ms staleness bound, closure exact, no
/// fence, the stream never stalled more. The loop's readers step their
/// epoch on every advancing pass exactly as the product's sink does.
#[test]
fn the_step_stamped_drain_cuts_the_hold_by_the_cache_ttl() {
    let _serial = serial();
    let shape = fleet_cadence_shape("rederive-drain-stamp(spare=256,ckpt=1s,8m)", 256);
    let before = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            ..Levers::h3()
        },
    );
    let after = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            drain_epoch_stamp: true,
            ..Levers::h3()
        },
    );
    for r in [&before, &after] {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
    }
    let cut = before.bound_age_mean_ms - after.bound_age_mean_ms;
    assert!(
        (1_500.0..=2_500.0).contains(&cut),
        "the step-stamped drain removes ≈ the staleness bound: {:.0} → {:.0} ms (−{cut:.0})",
        before.bound_age_mean_ms,
        after.bound_age_mean_ms
    );
    assert!(
        after.stalls_steady <= before.stalls_steady,
        "never stalls the stream more"
    );
}

/// **Contract 36 — item 3: the drain is OBSERVED — a candidate promotes
/// when every serve that started before its qualifying step has completed,
/// and NOT before; `D_purge` is a tripwire, never a shorter wait.** The
/// ledger counts every read serve in the purge generation it started
/// under (`ServeStamp`); the ladder's drained condition for a candidate
/// qualified after the step that left generation `G` is "every slot below
/// `G` reads zero". A serve that started AFTER the step never blocks it.
/// A pre-step serve outliving the fail-stop budget counts
/// `free_grace_drain_overdue` (and `invariant_tripwires`) exactly once and
/// the candidate KEEPS waiting. `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED=0`
/// restores the `D_purge` timer verbatim.
#[test]
fn the_drain_is_observed_and_never_promotes_over_a_pre_step_serve_in_flight() {
    use squeezefs::ro_coherence::{self, ServeStamp};
    let _serial = serial();
    ro_coherence::test_arm_serve_ledger();

    // The ledger alone: nothing in flight drains at once; a serve stamped
    // before a step blocks the drain below that step until it completes;
    // a serve stamped after the step never does.
    assert!(ro_coherence::serve_drained_below(0));
    let pre = ServeStamp::begin(); // generation 0
    ro_coherence::test_note_epoch_step(); // → 1
    let post = ServeStamp::begin(); // generation 1
    assert_eq!(ro_coherence::serves_inflight(), 2);
    assert!(
        !ro_coherence::serve_drained_below(1),
        "a serve that started before the step is in flight"
    );
    drop(pre);
    assert!(
        ro_coherence::serve_drained_below(1),
        "the post-step serve does not block the drain below its own generation"
    );
    assert_eq!(ro_coherence::serves_inflight(), 1);
    drop(post);
    assert_eq!(ro_coherence::serves_inflight(), 0);

    // The ladder on it: a bare ladder with the timer at 0 (both drain
    // terms re-derived), a candidate qualifying at the pass that stepped
    // to generation 2 while one pre-step serve is in flight.
    let ladder = ReaderAckLadder::new();
    let trips0 = METRICS.invariant_tripwires.load(Ordering::Relaxed);
    let straggler = ServeStamp::begin(); // generation 1 — pre-step for the pass below
    ro_coherence::test_note_epoch_step(); // → 2 (the qualifying pass's step)
    let inputs = |pass_start: u64, now: u64, advanced: bool| AckInputs {
        label: 5_000,
        learned_at_ms: 1_000,
        pass_start_ms: pass_start,
        now_ms: now,
        advanced,
        qualify_lag_ms: 1_122,
        drain_lag_ms: 0,
        refresh_floor_ms: 1_000,
        drain_gen: ro_coherence::reader_step_generation(),
        drain_budget_ms: 2_000,
    };
    assert_eq!(
        ladder.note_pass(inputs(2_200, 2_210, true)),
        None,
        "qualified, but the pre-step serve is in flight"
    );
    assert_eq!(ladder.pending(), 5_000);
    // Time alone never promotes it — not at D_purge, not far past it —
    // and the tripwire fires exactly once when the budget is exceeded.
    assert_eq!(ladder.note_pass(inputs(4_000, 4_100, false)), None);
    assert_eq!(
        free_grace::drain_overdue(),
        0,
        "inside the budget: no tripwire"
    );
    assert_eq!(ladder.note_pass(inputs(4_300, 4_300, false)), None);
    assert_eq!(
        free_grace::drain_overdue(),
        1,
        "2,090 ms after the qualifying pass the D_purge budget is exceeded: the tripwire fires"
    );
    assert_eq!(
        METRICS.invariant_tripwires.load(Ordering::Relaxed),
        trips0 + 1,
        "…and it is an invariant tripwire"
    );
    assert_eq!(ladder.note_pass(inputs(9_000, 9_000, false)), None);
    assert_eq!(free_grace::drain_overdue(), 1, "counted once per candidate");
    assert_eq!(free_grace::drain_observed(), 0);
    // A serve that started AFTER the qualifying step changes nothing.
    let later = ServeStamp::begin(); // generation 2
    assert_eq!(ladder.note_pass(inputs(9_100, 9_100, false)), None);
    // The straggler completes: the very next pass promotes — by
    // observation, with the later serve still in flight.
    drop(straggler);
    assert_eq!(ladder.note_pass(inputs(9_200, 9_200, false)), Some(5_000));
    assert_eq!(free_grace::drain_observed(), 1);
    drop(later);

    // The lever off (timer only): `drain_gen` 0 — the same in-flight serve
    // does not hold the promotion, the D_purge timer does.
    let ladder = ReaderAckLadder::new();
    let held = ServeStamp::begin();
    ro_coherence::test_note_epoch_step();
    let timer = |pass_start: u64, now: u64, advanced: bool| AckInputs {
        drain_gen: 0,
        drain_lag_ms: 2_000,
        ..inputs(pass_start, now, advanced)
    };
    assert_eq!(ladder.note_pass(timer(2_200, 2_210, true)), None);
    assert_eq!(
        ladder.note_pass(timer(4_000, 4_100, false)),
        None,
        "2,000 not yet elapsed"
    );
    assert_eq!(
        ladder.note_pass(timer(4_300, 4_300, false)),
        Some(5_000),
        "lever off: the timer promotes over the in-flight serve (the pre-change shape)"
    );
    drop(held);
    assert_eq!(
        free_grace::drain_observed(),
        1,
        "a timer promotion is not an observed one"
    );
}

/// **Contract 37 — item 3 end to end on a member session, and the
/// completion wake.** With the ledger armed and nothing in flight the
/// promotion lands AT the qualifying pass (the drain is the serves
/// themselves — here none); with one pre-step serve in flight the
/// qualifying pass does not promote, its completion wakes the
/// revalidation task (`free_grace_drain_wakes`, the parked
/// `drain_wake()`), and the woken pass promotes. The windows in force are
/// published.
#[test]
fn a_qualified_candidate_promotes_at_the_pass_and_a_stragglers_completion_wakes_the_ladder() {
    use squeezefs::ro_coherence::{self, ServeStamp};
    let _serial = serial();
    for straggle in [false, true] {
        free_grace::reset_for_test();
        membership::uninstall();
        ro_coherence::test_set_drain_observed(Some(true));
        ro_coherence::test_arm_serve_ledger();
        let (clock, ticks) = manual_clock();
        let owner = armed_owner(&clock);
        let grant = join(&owner, "r-observed", MemberRole::Reader);
        owner.refresh_free_grace_bound();
        let anchor = clock.now_ms();
        let session = Arc::new(MemberSession::adopt(
            "r-observed",
            MemberRole::Reader,
            &grant,
            anchor,
            clock.clone(),
        ));
        membership::install_member(Arc::clone(&session));
        let (label, _) = session.learned_label();
        let qualify = session.checkpoint_ceiling_ms() + session.skew_max_ms();

        let straggler = straggle.then(ServeStamp::begin);
        ticks.fetch_add(qualify, Ordering::SeqCst);
        // The pass's step (the product's sink bumps the generation as its
        // last act), then the ladder at the pass's end.
        ro_coherence::test_note_epoch_step();
        let promoted = free_grace::reader_pass_completed(clock.now_ms(), true);
        let snap = free_grace::stats_snapshot();
        assert_eq!(snap["free_grace_drain_lag_ms"], 0, "no timer term remains");
        assert_eq!(snap["free_grace_qualify_lag_ms"], qualify);
        if !straggle {
            assert_eq!(
                promoted,
                Some(label),
                "nothing in flight: promoted AT the qualifying pass"
            );
            assert_eq!(free_grace::drain_observed(), 1);
            assert_eq!(session.acked_free_epoch(), label);
        } else {
            assert_eq!(promoted, None, "a pre-step serve is in flight");
            assert_eq!(reader_pending_label(), label);
            let mut parked = ro_coherence::drain_wake().notified_raw();
            assert!(!parked.enable(), "nothing has woken the task yet");
            // A post-step serve completing wakes nobody (it is not waited on).
            drop(ServeStamp::begin());
            assert_eq!(ro_coherence::drain_wakes(), 0);
            // The straggler completes: the last pre-step serve wakes the
            // revalidation task, whose early pass promotes.
            drop(straggler);
            assert_eq!(ro_coherence::drain_wakes(), 1);
            assert!(parked.enable(), "the parked revalidation task is woken");
            assert_eq!(
                free_grace::reader_pass_completed(clock.now_ms(), false),
                Some(label),
                "the woken pass promotes by observation"
            );
            assert_eq!(free_grace::drain_observed(), 1);
            assert_eq!(free_grace::drain_overdue(), 0);
        }
        assert!(ro_coherence::test_clear_drain_observed());
    }
}

/// **Contract 38 — item 3 on the fleet-cadence loop: the `D_purge` timer
/// leaves the hold.** R2 (items 1 + 2) against R3 (+ the observed drain):
/// `bound_age` falls by ≈ the 2,000 ms reserve, closure exact, no fence,
/// the stream never stalled more. The model runs no serves, so its drain
/// is the step itself; on a mount the term is the serve residence
/// (milliseconds). The three rows together are the re-derivation's
/// before/after: H3 → R3.
#[test]
fn the_observed_drain_cuts_the_hold_by_the_lease_clock_reserve() {
    let _serial = serial();
    let shape = fleet_cadence_shape("rederive-drain-observed(spare=256,ckpt=1s,8m)", 256);
    let h3 = run_closed_loop(&shape, Levers::h3());
    let before = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            drain_epoch_stamp: true,
            ..Levers::h3()
        },
    );
    let after = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            drain_epoch_stamp: true,
            drain_observed: true,
            ..Levers::h3()
        },
    );
    for r in [&h3, &before, &after] {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
    }
    let cut = before.bound_age_mean_ms - after.bound_age_mean_ms;
    assert!(
        (1_500.0..=2_500.0).contains(&cut),
        "the observed drain removes ≈ D_purge: {:.0} → {:.0} ms (−{cut:.0})",
        before.bound_age_mean_ms,
        after.bound_age_mean_ms
    );
    assert!(
        after.stalls_steady <= before.stalls_steady,
        "never stalls the stream more"
    );
    assert_eq!(
        free_grace::drain_overdue(),
        0,
        "no drain outlived the fail-stop budget"
    );
    assert!(
        (2_500.0..=4_000.0).contains(&after.bound_age_mean_ms),
        "the re-derived hold at fleet cadences: {:.0} ms (H3 {:.0})",
        after.bound_age_mean_ms,
        h3.bound_age_mean_ms
    );
}

/// **Contract 39 — all four adjudication items composed (R4): the shape
/// the fleet row is read against.** R3 (items 1–3) plus the checkpoint
/// composite (item 4): the qualify term now reads the ADVERTISED landing
/// ceiling (`P/2 + 2 × min(tick, P/2)` while the valve asks), and the
/// members' pass and beat floors follow it. R4 holds no longer than R3 —
/// the composite may only shorten the learn, qualify-rounding and carry
/// terms — with closure exact, no fence, no overdue drain, and every
/// coherence gate unchanged. The row is printed so the note can carry it.
#[test]
fn all_four_items_compose_to_the_shortest_hold() {
    let _serial = serial();
    let shape = fleet_cadence_shape("rederive-all-four(spare=256,ckpt=1s,8m)", 256);
    let r3 = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            drain_epoch_stamp: true,
            drain_observed: true,
            ..Levers::h3()
        },
    );
    let r4 = run_closed_loop(
        &shape,
        Levers {
            qualify_ceiling: true,
            drain_epoch_stamp: true,
            drain_observed: true,
            composite: true,
            ..Levers::h3()
        },
    );
    for r in [&r3, &r4] {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
    }
    assert_eq!(
        free_grace::drain_overdue(),
        0,
        "no drain outlived the budget"
    );
    assert!(
        r4.bound_age_mean_ms <= r3.bound_age_mean_ms,
        "the composite never lengthens the re-derived hold: R3 {:.0} → R4 {:.0} ms",
        r3.bound_age_mean_ms,
        r4.bound_age_mean_ms
    );
    assert!(
        r4.stalls_steady <= r3.stalls_steady,
        "never stalls the stream more"
    );
}

// ===========================================================================
// The writer→member checkpoint composite (2026-09-06, adjudication item 4
// — USER DECISION 2026-09-06; `.benchmarks/2026-09-06-free-grace-checkpoint-composite.md`)
//
// Contract 31 measured a faster writer checkpoint INERT alone: the reader
// qualifies on a TIME bound, not on observing the checkpoint. It pays only
// as the composite — the writer's live ceiling under the valve's ask
// (P/2, the Nyquist bound against the reader's routine poll P) CARRIED to
// every member on the grant, where it is the prod floor AND L2b's pass
// floor, so passes and beats run at P/2 too. The user accepted the cost:
// 2× lease-lane beats and 2× checkpoint cycles while an ask is in force.
// ===========================================================================

/// **Contract 32 — the composite's numbers derive; nothing is a constant
/// of its own.** The elastic ceiling is `max(P/2, 2 × measured cycle)`
/// capped at the writer's routine ceiling (a cycle may not run more than
/// half the time — lever (d)'s law for the scan, applied to the
/// checkpoint); the live prod floor is `max(min(P, ceiling), skew)` and
/// reduces to the shipped `max(P, skew)` at the routine ceiling — the
/// identity that makes "no ask ⇒ the shipped shape" structural.
#[test]
fn the_elastic_checkpoint_ceiling_and_the_live_prod_floor_derive() {
    let _serial = serial();
    use free_grace::elastic_checkpoint_ceiling_ms as elastic;
    // The shipped venue: P = 1,000 ms, a ≈ 20 ms cycle ⇒ 500 ms.
    assert_eq!(elastic(1_000, 20, 1_000), 500);
    // The cycle-cost floor: a 300 ms cycle may not run more than half the
    // time ⇒ 600 ms; a 600 ms cycle pins the ceiling at the routine.
    assert_eq!(elastic(1_000, 300, 1_000), 600);
    assert_eq!(elastic(1_000, 600, 1_000), 1_000);
    // A slow-flush venue (5 s tick): P = 5,000, routine 5,000 ⇒ 2,500.
    assert_eq!(elastic(5_000, 20, 5_000), 2_500);
    // A reader polling faster than the writer's routine (the env
    // override): half ITS poll, never below one ms.
    assert_eq!(elastic(300, 0, 1_000), 150);
    assert_eq!(elastic(1, 0, 1_000), 1);
    // Never slower than the routine, whatever the inputs.
    assert_eq!(elastic(20_000, 0, 1_000), 1_000);

    let clocks = shipped_clocks();
    let prod = free_grace::ProdParams::derive(&clocks);
    let floor = free_grace::ack_refresh_floor(&clocks).as_millis() as u64;
    let skew = clocks.skew_max.as_millis() as u64;
    let routine = free_grace::writer_routine_checkpoint_ceiling_ms();
    assert_eq!(
        prod.floor_for(routine),
        floor,
        "at the writer's routine ceiling the live floor IS the shipped floor (the identity)"
    );
    assert_eq!(
        prod.floor_for(u64::MAX),
        floor,
        "a ceiling above P changes nothing"
    );
    assert_eq!(
        prod.floor_for(500),
        500,
        "a halved ceiling halves the floor: a member's answer can now change every P/2"
    );
    assert_eq!(
        prod.floor_for(1),
        skew,
        "…but never below the clock-skew bound — the wire's answer changes no faster"
    );
    assert_eq!(
        prod.cadence_for(0, prod.floor_for(500)),
        Some(500),
        "the cliff ask under the halved ceiling is the halved floor"
    );
}

/// **Contract 33 — the writer's ceiling follows the valve's ask, the grant
/// carries it, and the prod floor follows.** No ask ⇒ no elastic ceiling
/// (`None`: the checkpoint task runs its shipped constant/tick) and every
/// grant advertises the writer's ROUTINE ceiling; an ask in force (a
/// pressure harvest — rung (a) at the cliff) ⇒ the ceiling in force is
/// P/2, the gauge reads it, the next grant carries it, and the next ask
/// is the halved floor. The PROMISE: a grant that advertised an elastic
/// ceiling is honoured for one routine ceiling past it even after the ask
/// lapses (the writer relaxes no sooner than every advertised window has
/// closed). `CHECKPOINT_COMPOSITE=0` ⇒ `None` and the routine ceiling on
/// every grant, ask or no ask.
#[test]
fn an_ask_in_force_tightens_the_writers_ceiling_and_the_grant_carries_it() {
    let _serial = serial();
    free_grace::test_set_checkpoint_composite(Some(true));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant0 = join(&owner, "r-ceiling", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let routine = free_grace::writer_routine_checkpoint_ceiling_ms();
    let poll = squeezefs::ro_coherence::reader_revalidate_interval().as_millis() as u64;
    assert_eq!(
        routine,
        squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64,
        "the shipped venue (50 ms flush): the routine writer ceiling IS the constant"
    );

    // No ask: the shipped posture, and the grant says so.
    assert_eq!(
        free_grace::checkpoint_ceiling_in_force_ms(),
        None,
        "no ask in force ⇒ no elastic ceiling (the checkpoint task's shipped decision)"
    );
    assert_eq!(
        free_grace::checkpoint_ceiling_ms(),
        routine,
        "the gauge reads the routine"
    );
    // What travels is the LANDING ceiling (re-derivation item 1 composed
    // with the composite): the routine decision + two flush ticks.
    let routine_landing =
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived();
    assert_eq!(
        grant0.checkpoint_ceiling_ms, routine_landing,
        "a join grant advertises the routine landing ceiling"
    );
    let g = match owner.renew("r-ceiling", grant0.epoch, 0) {
        RenewOutcome::Renewed(g) => g,
        other => panic!("{other:?}"),
    };
    assert_eq!(g.checkpoint_ceiling_ms, routine_landing);
    assert_eq!(free_grace::checkpoint_elastic_cycles(), 0);

    // The ask: a held offset at the allocation cliff (the pressure
    // harvest — rung (a) asks the floor cadence). Two harvests: the
    // first computes its ask under the routine floor and puts the ask in
    // force; the second reads the halved floor the ask enabled.
    let ring = GraceRing::new(1024);
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    ticks.fetch_add(1, Ordering::SeqCst);
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert_eq!(
        free_grace::prod_renew_ms(),
        routine.max(clock_skew_ms(&owner)),
        "the first ask is the routine floor (the ceiling was routine when it was computed)"
    );
    assert_eq!(
        free_grace::checkpoint_ceiling_in_force_ms(),
        Some(poll / 2),
        "an ask in force ⇒ the writer's ceiling is P/2 (the Nyquist bound)"
    );
    assert_eq!(
        free_grace::checkpoint_ceiling_ms(),
        poll / 2,
        "the gauge reads it"
    );
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert_eq!(
        free_grace::prod_renew_ms(),
        poll / 2,
        "the next ask is the halved floor — the prod floor follows the live ceiling"
    );
    let g = match owner.renew("r-ceiling", grant0.epoch, 0) {
        RenewOutcome::Renewed(g) => g,
        other => panic!("{other:?}"),
    };
    // The grant advertises the LANDING ceiling of the halved decision
    // (re-derivation item 1 composed with the composite): the decision plus
    // the two tick-granularity terms the task evaluates behind, with the
    // tick tightened to the decision.
    // The flush cadence in force, read back off the routine landing
    // derivation (`trigger + 2 × tick`): the same tick the elastic landing
    // rides.
    let tick_ms = (squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived()
        - squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64)
        / 2;
    let landing = poll / 2 + 2 * tick_ms.min(poll / 2);
    assert_eq!(
        landing,
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_for_elastic(
            poll / 2,
            tick_ms,
        ),
        "the elastic landing derivation at the tick in force"
    );
    assert_eq!(
        (g.renew_ms, g.checkpoint_ceiling_ms),
        (poll / 2, landing),
        "the grant carries the halved beat AND the landing ceiling of the halved decision"
    );
    assert!(
        landing > poll / 2 && landing <= poll / 2 * 3,
        "landing = decision + 2 × min(tick, decision): {landing}"
    );
    let snap = free_grace::stats_snapshot();
    assert_eq!(
        snap["free_grace_checkpoint_ceiling_ms"].as_u64(),
        Some(poll / 2),
        "the stats inode publishes the ceiling in force"
    );
    assert_eq!(
        snap["free_grace_checkpoint_elastic_cycles"].as_u64(),
        Some(0)
    );
    free_grace::note_elastic_checkpoint_cycle();
    assert_eq!(
        free_grace::stats_snapshot()["free_grace_checkpoint_elastic_cycles"].as_u64(),
        Some(1),
        "elastic cycles are counted"
    );

    // The promise outlives the ask by one routine ceiling: the last
    // elastic grant was at `t_g`; the ask lapses (TTL = the routine beat,
    // no harvest refreshes it) but the writer holds P/2 until `t_g +
    // routine`, then relaxes.
    let t_g = clock.now_ms();
    let ttl = owner.clocks().renew_interval.as_millis() as u64;
    ticks.fetch_add(ttl + 1, Ordering::SeqCst);
    assert_eq!(free_grace::prod_renew_ms(), 0, "the ask lapsed");
    // The lever is latched off to isolate the promise from the derivation
    // (the decay arm would otherwise re-arm on the next renewal).
    free_grace::test_set_checkpoint_composite(Some(false));
    assert_eq!(
        free_grace::checkpoint_ceiling_in_force_ms(),
        None,
        "past t_g + routine ({} ms) the promise has closed: the routine posture",
        t_g + routine
    );
    // Re-run the promise with the clock inside the window.
    free_grace::test_set_checkpoint_composite(Some(true));
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    let g = match owner.renew("r-ceiling", grant0.epoch, 0) {
        RenewOutcome::Renewed(g) => g,
        other => panic!("{other:?}"),
    };
    // The wire carries the LANDING ceiling of the halved decision; the
    // promise pair (below) stays in DECISION terms — the task's own.
    assert_eq!(g.checkpoint_ceiling_ms, landing);
    free_grace::test_set_checkpoint_composite(Some(false));
    assert_eq!(
        free_grace::checkpoint_ceiling_in_force_ms(),
        Some(poll / 2),
        "the advertised window is honoured after the derivation stopped asking"
    );
    ticks.fetch_add(routine - 1, Ordering::SeqCst);
    assert_eq!(free_grace::checkpoint_ceiling_in_force_ms(), Some(poll / 2));
    ticks.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        free_grace::checkpoint_ceiling_in_force_ms(),
        None,
        "…and closes exactly one routine ceiling after the grant"
    );

    // The lever off, an ask in force: the routine ceiling on every grant,
    // no elastic ceiling, the routine floor — the shipped shape.
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert_eq!(free_grace::checkpoint_ceiling_in_force_ms(), None);
    assert_eq!(
        free_grace::prod_renew_ms(),
        routine.max(clock_skew_ms(&owner)),
        "CHECKPOINT_COMPOSITE=0: the ask is the shipped floor"
    );
    let g = match owner.renew("r-ceiling", grant0.epoch, 0) {
        RenewOutcome::Renewed(g) => g,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        g.checkpoint_ceiling_ms,
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived(),
        "CHECKPOINT_COMPOSITE=0: the grant advertises the routine landing ceiling"
    );
    assert!(free_grace::test_clear_checkpoint_composite());
}

/// The owner's clock-skew bound in ms (the floor's lower clamp).
fn clock_skew_ms(owner: &MembershipOwner) -> u64 {
    owner.clocks().skew_max.as_millis() as u64
}

/// **Contract 34 — the member adopts the advertised ceiling with its
/// label.** `MemberSession::checkpoint_ceiling_ms` is the writer's ceiling
/// advertised on the grant that carried the label `learned_label` reports
/// (a join or a routine renewal); a CARRIAGE renewal learns no label and
/// so learns no ceiling for the ladder — but deposits the grant's ask +
/// ceiling for the pass resolver like the routine renewal does. A grant
/// advertising nothing (`0` — an owner with no grace plane) falls back to
/// the member's own derivation of the routine LANDING ceiling
/// (`checkpoint_landing_ceiling_derived`, re-derivation item 1);
/// `CHECKPOINT_COMPOSITE=0` reads that derivation whatever was advertised.
#[test]
fn the_member_adopts_the_advertised_ceiling_with_its_label() {
    let _serial = serial();
    free_grace::test_set_checkpoint_composite(Some(true));
    free_grace::test_set_pass_elastic(Some(true));
    let constant = squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived();
    let cl = shipped_clocks();
    let grant = |epoch: u64, renew_ms: u64, ceiling: u64, at: u64| Grant {
        epoch,
        term: 1,
        t_owner_ms: cl.t_owner.as_millis() as u64,
        skew_max_ms: cl.skew_max.as_millis() as u64,
        d_purge_ms: cl.d_purge.as_millis() as u64,
        renew_ms,
        granted_at_owner_ms: at,
        lane_supply_blocks: 0,
        checkpoint_ceiling_ms: ceiling,
    };
    let (clock, ticks) = manual_clock();
    let session = MemberSession::adopt(
        "m-ceiling",
        MemberRole::Reader,
        &grant(1, 10_000, 0, 100),
        10_000,
        clock,
    );
    assert_eq!(
        session.checkpoint_ceiling_ms(),
        constant,
        "nothing advertised ⇒ the member's own landing derivation (an owner with no grace plane)"
    );
    ticks.fetch_add(500, Ordering::SeqCst);
    session.renewed(&grant(1, 500, 500, 600), 10_500);
    assert_eq!(
        session.checkpoint_ceiling_ms(),
        500,
        "a routine renewal learns the ceiling WITH the label"
    );
    assert_eq!(session.learned_label().0, 600);
    ticks.fetch_add(200, Ordering::SeqCst);
    session.renewed_carriage(&grant(1, 700, 700, 800), 10_700);
    assert_eq!(
        session.checkpoint_ceiling_ms(),
        500,
        "a carriage renewal learns no label and no ceiling for the ladder"
    );
    assert_eq!(
        session.learned_label().0,
        600,
        "…the label stays the routine renewal's"
    );
    assert_eq!(
        free_grace::reader_pass_interval(Duration::from_millis(5_000), 10_750),
        Duration::from_millis(700),
        "…but the pass resolver reads the carriage's ask + ceiling (the pass floor is about NOW)"
    );
    ticks.fetch_add(300, Ordering::SeqCst);
    session.renewed(&grant(1, 10_000, 0, 1_100), 11_000);
    assert_eq!(
        session.checkpoint_ceiling_ms(),
        constant,
        "a later grant advertising nothing returns the member to its own landing derivation"
    );
    session.renewed(&grant(1, 500, 500, 1_200), 11_100);
    free_grace::test_set_checkpoint_composite(Some(false));
    assert_eq!(
        session.checkpoint_ceiling_ms(),
        constant,
        "CHECKPOINT_COMPOSITE=0: the member's own landing derivation, whatever was advertised"
    );
    assert!(free_grace::test_clear_checkpoint_composite());
    assert!(free_grace::test_clear_pass_elastic());
}

/// **Contract 35 — the composite on the fleet-cadence loop: the cadences
/// halve, the hold drops, the cost is the accepted 2×, and no promise
/// bends.** H4 = the shipped H3 plus the composite. Pinned: (i) the
/// writer ran at P/2 (`ceiling_min` = 500, `elastic_cycles` > 0) and the
/// members' passes tightened (`pass_prods` > 0 — L2b's first engagement on
/// this venue, structurally impossible before); (ii) `bound_age` reads
/// below H3's by the composite's predicted class (≥ 300 of the ≈ 600 ms
/// forecast); (iii) checkpoints/s and renewals/s read ≈ 2× H3's — the
/// cost the user accepted, measured; (iv) closure, `forced = fences = 0`,
/// the stream never stalls more; (v) the published staleness bound (which
/// never moves) is honoured by every pass the elastic cadence ran
/// (`staleness_worst ≤ bound`); (vi) H3 itself (the lever off) is the
/// hold-time note's row to the tick — the shipped shape, byte for byte.
#[test]
fn the_checkpoint_composite_halves_the_cadences_and_cuts_the_hold() {
    let _serial = serial();
    let shape = fleet_cadence_shape("composite(spare=256,8m)", 256);
    let h3 = run_closed_loop(&shape, Levers::h3());
    let h4 = run_closed_loop(
        &shape,
        Levers {
            composite: true,
            ..Levers::h3()
        },
    );
    let staleness_ms = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
    for r in [&h3, &h4] {
        println!("{}", r.render(&shape));
        assert_eq!(
            r.deferrals,
            r.releases + r.held_end,
            "{}: closure",
            r.config
        );
        assert_eq!((r.forced, r.fences), (0, 0), "{}: no fence", r.config);
        assert_eq!(r.hold_unplaced, 0, "{}: every stage placed", r.config);
        assert!(
            r.staleness_worst_ms <= staleness_ms,
            "{}: every pass honoured the published staleness bound ({} ≤ {staleness_ms} ms)",
            r.config,
            r.staleness_worst_ms
        );
    }
    // (vi) The lever off is the shipped H3 row (the hold-time note's
    // 7,724 ms — deterministic, bit-identical across runs).
    assert!(
        (h3.bound_age_mean_ms - 7_724.0).abs() <= 1.0,
        "the composite off is the shipped H3 row verbatim ({:.0} ms)",
        h3.bound_age_mean_ms
    );
    assert_eq!(
        h3.elastic_cycles, 0,
        "no elastic cycle without the composite"
    );
    assert_eq!(
        h3.pass_prods, 0,
        "L2b is structurally inert on this venue without it"
    );
    assert_eq!(h3.ceiling_min_ms, shape.checkpoint_ms);
    // (i) Engagement.
    assert_eq!(
        h4.ceiling_min_ms, 500,
        "the writer ran at P/2 while the ask was in force"
    );
    assert!(h4.elastic_cycles > 0, "elastic checkpoint cycles ran");
    assert!(
        h4.pass_prods > 0,
        "the members' passes tightened (L2b engaged by the composite)"
    );
    // (ii) The hold.
    assert!(
        h4.bound_age_mean_ms <= h3.bound_age_mean_ms - 300.0,
        "the composite cuts the hold: H4 {:.0} vs H3 {:.0} ms",
        h4.bound_age_mean_ms,
        h3.bound_age_mean_ms
    );
    assert!(
        h4.hold_ms < h3.hold_ms,
        "the live hold gauge follows ({} vs {} ms)",
        h4.hold_ms,
        h3.hold_ms
    );
    // (iii) The cost, measured: ≈ 2× checkpoints and ≈ 2× renewals.
    let ck_ratio = h4.checkpoints_per_s / h3.checkpoints_per_s;
    let rn_ratio = h4.renewals_per_s / h3.renewals_per_s;
    println!(
        "ROW composite-cost: checkpoints/s {:.2} → {:.2} ({ck_ratio:.2}×), renewals/s {:.2} → {:.2} ({rn_ratio:.2}×)",
        h3.checkpoints_per_s, h4.checkpoints_per_s, h3.renewals_per_s, h4.renewals_per_s
    );
    assert!(
        (1.6..=2.2).contains(&ck_ratio),
        "the checkpoint cost is the accepted 2× ({ck_ratio:.2}×)"
    );
    assert!(
        (1.6..=2.2).contains(&rn_ratio),
        "the lease-lane cost is the accepted 2× ({rn_ratio:.2}×)"
    );
    // (iv) Never more stalls.
    assert!(
        h4.stalls_steady <= h3.stalls_steady,
        "the composite never stalls the stream more"
    );
}

/// **Contract 36 — the KV checkpoint TASK runs at the elastic ceiling
/// under an ask, and at its shipped cadence without one.** A real
/// `KvMetaBackend` (its checkpoint task ticking on the shipped 50 ms
/// flush cadence) commits under a plane with no ask: no elastic cycle is
/// ever counted and the gauge reads the routine. An ask in force (a
/// pressure harvest) ⇒ the task's cycles are counted elastic and the
/// ledger's marks grow at the halved period. `CHECKPOINT_COMPOSITE=0`
/// under the same ask ⇒ no elastic cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_checkpoint_task_runs_at_the_elastic_ceiling_under_an_ask() {
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
    use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
    use squeezefs::meta_backend::Metadata;
    let _serial = serial();
    free_grace::test_set_checkpoint_composite(Some(true));
    let file = tempfile::NamedTempFile::new().expect("temp volume");
    file.as_file()
        .set_len(64 * 1024 * 1024)
        .expect("size volume");
    format_v3(
        file.path(),
        64 * 1024 * 1024,
        &FormatV3Options {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    let (clock, _ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let _r = join(&owner, "r-task", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let poll = squeezefs::ro_coherence::reader_revalidate_interval().as_millis() as u64;
    let routine = free_grace::writer_routine_checkpoint_ceiling_ms();

    // A commit stream for a little over one routine ceiling, no ask.
    let commit_for = |be: Arc<KvMetaBackend>, tag: &'static str, ms: u64| async move {
        let until = std::time::Instant::now() + Duration::from_millis(ms);
        let mut i = 0u64;
        while std::time::Instant::now() < until {
            be.create(1, &format!("{tag}-{i}"), 0o644, 0, 0)
                .await
                .expect("create");
            i += 1;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    commit_for(Arc::clone(&be), "quiet", routine + routine / 4).await;
    assert_eq!(
        free_grace::checkpoint_elastic_cycles(),
        0,
        "no ask ⇒ the task ran its shipped cadence: no elastic cycle"
    );
    assert_eq!(free_grace::checkpoint_ceiling_ms(), routine);
    assert!(
        free_grace::checkpoint_marks() >= 1,
        "the routine cycles marked the ledger"
    );

    // The ask (the allocation cliff), then the same stream.
    let ring = GraceRing::new(1024);
    assert!(ring.defer(4 * 1024 * 1024, 4 * 1024 * 1024));
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert!(ring.harvest_pressure(free_grace::HARVEST_BATCH).is_empty());
    assert_eq!(free_grace::checkpoint_ceiling_in_force_ms(), Some(poll / 2));
    let marks_before = free_grace::checkpoint_marks();
    commit_for(Arc::clone(&be), "asked", routine + routine / 4).await;
    let elastic = free_grace::checkpoint_elastic_cycles();
    let marks = free_grace::checkpoint_marks() - marks_before;
    println!(
        "ROW checkpoint-task: {marks} cycle(s) in {} ms under an ask, {elastic} elastic",
        routine + routine / 4
    );
    assert!(
        elastic >= 1,
        "an ask in force ⇒ the task's cycles ran under the elastic ceiling ({elastic})"
    );
    assert!(
        marks >= 2,
        "at P/2 the task cycled at least twice in 1.25 routine ceilings ({marks})"
    );

    // The lever off under the same ask: the shipped cadence.
    free_grace::test_set_checkpoint_composite(Some(false));
    let elastic_before = free_grace::checkpoint_elastic_cycles();
    commit_for(Arc::clone(&be), "off", routine + routine / 4).await;
    assert_eq!(
        free_grace::checkpoint_elastic_cycles(),
        elastic_before,
        "CHECKPOINT_COMPOSITE=0: no elastic cycle under an ask"
    );
    assert!(free_grace::test_clear_checkpoint_composite());
    be.shutdown().await.expect("shutdown");
}
