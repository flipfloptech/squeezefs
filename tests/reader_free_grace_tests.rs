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
