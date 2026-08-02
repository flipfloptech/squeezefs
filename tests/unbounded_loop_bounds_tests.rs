//! RES-14 / RES-15 (pre-RC engineering spec §7): two loops on the
//! allocation path with no cap, no deadline and no diagnostic.
//!
//! * **RES-14 — `ExtCore::claim`.** The bit scan rescans forever when the
//!   free-budget CAS won an entitlement the bitmap cannot honour. The
//!   rescan is CORRECT as a race handler (a release landing behind the
//!   cursor), but the exit condition is "a clear bit appears", which
//!   invariant drift makes unreachable. It is a **sync** function called
//!   from async: a spinning claim consumes a tokio worker permanently and
//!   says nothing. Every other invariant in this core fails LOUD (typed
//!   errors, refusals); this one hangs.
//! * **RES-15 — `BlockAllocator::allocate_block`'s ENOSPC valve loop.**
//!   The loop exits only when a pass observes `pending == false` before
//!   its drain. Under concurrent reclaim traffic `pending` is true on
//!   every pass, so the honest-refusal exit is never reached and the
//!   allocating task spins on a genuinely full store instead of returning
//!   `StorageFull`.
//!
//! Both fixes are the same shape: bound the loop, then escalate to the
//! honest typed refusal.
//!
//! RED against dev 7d1ec2e1: both tests hang until their timeout.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::meta_backend::kv::alloc_ext_core::{AllocClass, ClaimError, ExtCore};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// RES-14 — the extent-claim bit scan.
// ---------------------------------------------------------------------------

#[test]
fn ext_claim_bounds_its_rescan_and_fails_loud_on_invariant_drift() {
    let core = ExtCore::new(256, 8, 4);
    // Exhaust the heap honestly first: every bit set, budget at 0.
    for _ in 0..256 {
        core.claim(AllocClass::Internal).expect("heap not yet full");
    }
    assert_eq!(
        core.claim(AllocClass::Internal),
        Err(ClaimError::NoSpace),
        "premise: a genuinely full heap refuses through the budget gate"
    );

    // Now DRIFT: the budget claims a clear bit exists; the bitmap says
    // otherwise. Pre-fix the scan below never returns.
    core.inflate_free_budget_for_test(1);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    let handle = std::thread::spawn(move || {
        let r = core.claim(AllocClass::Internal);
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        r
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "RES-14: ExtCore::claim spins forever on invariant drift — no \
             cap, no yield, no diagnostic, inside a sync fn called from \
             async (it permanently consumes a worker)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        handle.join().expect("claim thread"),
        Err(ClaimError::InvariantDrift),
        "RES-14: drift must be a LOUD typed refusal, never NoSpace (which \
         an operator would read as an honest full heap) and never a hang"
    );
}

// ---------------------------------------------------------------------------
// RES-15 — the ENOSPC pressure-valve loop.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allocate_block_enospc_valve_loop_is_bounded() {
    let alloc = Arc::new(
        BlockAllocator::new("res15_valve_bound")
            .await
            .expect("allocator"),
    );
    // A store with exactly one block, already spent: the next allocation
    // is a genuine, permanent StorageFull.
    alloc.set_capacity_bytes(alloc.chunk_size());
    alloc
        .allocate_block()
        .await
        .expect("the one block is available");

    // The field shape: a drain that reclaims nothing while `pending`
    // stays true because OTHER writers keep queueing frees. Pre-fix the
    // loop never reaches its honest-refusal exit.
    alloc.set_space_pressure_valve(
        Arc::new(|| Box::pin(async {})),
        Arc::new(|| true), // always pending
    );

    let res = tokio::time::timeout(Duration::from_secs(10), alloc.allocate_block())
        .await
        .expect(
            "RES-15: the ENOSPC valve loop has no attempt or deadline bound — \
             concurrent reclaim traffic keeps `pending` true so the honest \
             refusal is never reached and the allocating task spins",
        );
    let err = res.expect_err("an exhausted store must refuse");
    assert!(
        matches!(&err, squeezefs::error::SqueezefsError::Io(io)
                 if io.kind() == std::io::ErrorKind::StorageFull),
        "RES-15: the bounded loop must escalate to StorageFull, not some \
         other error class: {err:?}"
    );
}
