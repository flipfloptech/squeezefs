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
/// * whose pass **began** at least `staleness_bound + skew_max` after the
///   label was learned, so the writer's dereference is durably
///   checkpointed and therefore in the record the pass adopts;
/// * plus a **drain wait** of `staleness_bound + D_purge` — the daemon
///   layout/attr caches (whose reader TTL *is* the staleness bound) must
///   expire and in-flight serves must finish. §6.7's `D_purge` is exactly
///   the "observe, then finish" term.
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
        prod.cadence_for(cycle),
        None,
        "a runway of one whole cycle needs no prod: the routine beat already fits"
    );
    assert_eq!(
        prod.cadence_for(u64::MAX),
        None,
        "no runway pressure, no prod"
    );
    assert_eq!(
        prod.cadence_for(0),
        Some(floor),
        "at the cliff the beat is the fastest one that can carry a new answer"
    );
    let mid = prod
        .cadence_for(cycle - renew)
        .expect("a mid-pressure prod");
    assert!(
        (floor..renew).contains(&mid),
        "the prodded cadence {mid} must sit between the floor {floor} and the routine {renew}"
    );
    assert!(
        prod.cadence_for(cycle - renew * 2).expect("more pressure") <= mid,
        "the cadence must tighten monotonically as the runway shortens"
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
/// the checkpoint ceiling as its physics floor** (OQ 3, user decision):
/// `pass_interval = clamp(prodded renew_ms, CHECKPOINT_MAX_AGE_MS,
/// routine)`, TTL'd like the prod; expiry restores the routine cadence;
/// `PASS_ELASTIC=0` is the routine cadence always; and on a venue whose
/// routine interval already sits AT the floor the lever is structurally
/// inert (`free_grace_pass_prods` stays 0 — the s11 venue's own shape).
#[test]
fn a_prodded_grant_tightens_the_pass_cadence_with_the_checkpoint_floor() {
    let _serial = serial();
    free_grace::test_set_pass_elastic(Some(true));

    // A slow-flush venue: routine pass interval 5 s, prodded renew 1 s.
    let routine = Duration::from_millis(5_000);
    let prods_before = free_grace::pass_prods();
    free_grace::note_prodded_renewal(1_000, 10_000);
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

    // The physics floor: an ask below the checkpoint ceiling clamps UP —
    // polling faster than the writer's 1 s checkpoint observes nothing.
    free_grace::note_prodded_renewal(200, 11_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 11_100),
        Duration::from_millis(1_000),
        "the floor is CHECKPOINT_MAX_AGE_MS — physics, not tuning"
    );

    // Expiry: the routine cadence recovers within one reading TTL.
    assert_eq!(
        free_grace::reader_pass_interval(routine, 60_000),
        routine,
        "a quiet window restores the routine cadence"
    );
    assert_eq!(free_grace::pass_interval_ms(), 5_000);

    // The floor venue (routine == ceiling): structurally inert.
    let floor_routine = Duration::from_millis(1_000);
    let prods_mid = free_grace::pass_prods();
    free_grace::note_prodded_renewal(1_000, 70_000);
    assert_eq!(
        free_grace::reader_pass_interval(floor_routine, 70_100),
        floor_routine,
        "routine already AT the floor: nothing to tighten (the s11 venue)"
    );
    assert_eq!(
        free_grace::pass_prods(),
        prods_mid,
        "structural inertness: no engagement counted where routine = floor"
    );

    // The lever: routine verbatim, engagement 0.
    free_grace::test_set_pass_elastic(Some(false));
    free_grace::note_prodded_renewal(1_000, 80_000);
    assert_eq!(
        free_grace::reader_pass_interval(routine, 80_100),
        routine,
        "PASS_ELASTIC=0: the pre-campaign S5 cadence verbatim"
    );
    assert!(free_grace::test_clear_pass_elastic());

    // The three numbers that deliberately DO NOT move under a prodded
    // window (§5.2b's never-weakens argument): the published staleness
    // bound is the ROUTINE derivation — structurally independent of the
    // pass word (it reads no prodded state).
    let bound_before = squeezefs::ro_coherence::reader_staleness_bound();
    free_grace::test_set_pass_elastic(Some(true));
    free_grace::note_prodded_renewal(1_000, 90_000);
    assert_eq!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        bound_before,
        "the PUBLISHED staleness bound never flickers with load — it is \
         the guarantee in force, not the cadence in force"
    );
    assert!(free_grace::test_clear_pass_elastic());
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
