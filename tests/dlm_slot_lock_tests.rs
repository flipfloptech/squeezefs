//! DLM stage **S4** — the slot-homed lock authority in **solo mode**
//! (`docs/pre-rc-engineering-spec.md` §6.7 decisions 2/3 + §6.9 stage S4;
//! `docs/pre-rc-execution-plan.md` Phase 4, where S4 is the program's
//! go/no-go gate: *"mdstorm + rand-4k + scoreboard within noise;
//! `dlm_rpcs == 0` asserted everywhere"*).
//!
//! S4 ships **structure, not traffic**. This node owns every slot, so
//! every acquire is exactly the `scc` probe S0–S2 shipped and the network
//! ledger is **0 by construction**. What the stage adds is the pair of
//! questions S6/S8/S9 need answered without ever touching the acquire
//! path again: *where does this lock object live* (homing) and *do we own
//! that home* (ownership).
//!
//! Contracts:
//!
//! 1. **Homing IS the metadata slot map** — `slot = (ino − 2) % W` over
//!    the durable `routing_width W` (`route_ino_width`,
//!    `docs/design-dynamic-meta-routing.md`), never an invented hash
//!    ring. So the lock master and the metadata authority are the same
//!    process by construction (spec §6.7 decision 2) and a metadata RPC
//!    and its lock are one round trip. Includes the `W ≤ 1` identity arm
//!    and the ino-1 (root) / ino-2 (first routed ino) edges.
//! 2. **Ownership is QUERIED, not assumed** — one cheap lock-free
//!    lookup, whose solo answer is unconditionally "local" for every
//!    slot, including slots past the width and `u64::MAX`.
//! 3. **`dlm_rpcs == 0` by construction in solo mode**, across every
//!    acquire shape the product issues: uncontended, contended (the
//!    bounded loud refusal), byte-range, and lease refresh/re-acquire.
//! 4. **Composition with S11 byte-range custody** — homing is per
//!    *object*: a span never changes an object's home, so disjoint
//!    ranges of one ino stay genuinely concurrent through the new layer
//!    and an overlapping range still arbitrates.
//! 5. **Fencing stays monotone through the layer** — the ~24-site census
//!    reads the same generator: the lease snapshot equals the read while
//!    held, re-acquisition is strictly greater, and no release regresses
//!    it.
//! 6. **Concurrency** — acquire/release hammered across an ino family
//!    that homes to ONE slot (`ino + k·W`) and one that spreads across
//!    distinct slots; tokens stay globally unique and the ledger stays 0.
//! 7. **A foreign home refuses LOUD and counts the RPC site.** No
//!    production configuration can produce a foreign home today (solo
//!    owns every slot); the documented test seam is the only way in —
//!    the `test_arm_cw_mode` precedent — and it is what proves the
//!    refusal is real rather than a comment, and that `dlm_rpcs` is a
//!    live counter rather than a permanently-0 decoration (the §6.1
//!    `lease_acquire_*` lesson).
//! 8. **The `DlmClient` handle alias IS the slot manager** (S4 swapped
//!    it, as `dlm.rs` promised at S0) and it satisfies `LockManager`, so
//!    every one of the ~200 historical call sites now routes through
//!    homing + ownership with no call-site edit.
//!
//! Test-process discipline: the ownership plane, the published routing
//! width and the `dlm_rpcs` ledger are process-global. Tests that MUTATE
//! them take [`GLOBAL_STATE`] exclusively; tests that merely assert the
//! solo posture (or a zero ledger delta) take it shared. No sleeps
//! anywhere — the only clocks are acquire budgets and failure timeouts.

use squeezefs::dlm::{DlmClient, LockManager, LockMode};
use squeezefs::dlm_slot::{
    dlm_mode, dlm_rpcs, is_local_slot, lock_home_slot, publish_routing_width, routing_width,
    slot_of_ino, test_set_local_slots, SlotLockManager,
};
use squeezefs::meta_backend::{route_ino_width, DERIVED_ROUTING_WIDTH};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Writers mutate the process-global ownership plane / routing width;
/// readers only assert the solo posture and zero ledger deltas.
static GLOBAL_STATE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

const MIB: u64 = 1 << 20;
/// This suite's private inode band — `74_200_000 .. 75_000_000` (the lock
/// table and the fencing mint are process-global statics shared across
/// the binary; the same-slot family strides by `W = 65536`, hence the
/// width of the reservation).
const BAND: u64 = 74_200_000;

fn dlm() -> DlmClient {
    DlmClient::new().expect("dlm client")
}

/// Restores the published width on drop so a width-publishing test can
/// never leak its width into the rest of the binary.
struct WidthGuard(u64);

impl Drop for WidthGuard {
    fn drop(&mut self) {
        publish_routing_width(self.0);
    }
}

fn publish_width(width: u64) -> WidthGuard {
    let guard = WidthGuard(routing_width());
    publish_routing_width(width);
    guard
}

// ---------------------------------------------------------------------------
// 1. Homing IS the metadata slot map
// ---------------------------------------------------------------------------

/// Contract 1: the lock home slot is the METADATA slot, bit for bit —
/// `slot_of_ino` must be `route_ino_width(...).0` at every width, or the
/// lock master and the metadata authority can diverge and a metadata RPC
/// plus its lock stop being one round trip (spec §6.7 decision 2).
#[test]
fn lock_homing_is_the_metadata_slot_map() {
    let widths = [
        0,
        1,
        2,
        3,
        64,
        u64::from(DERIVED_ROUTING_WIDTH),
        u64::from(DERIVED_ROUTING_WIDTH) * 2,
    ];
    let inos = [
        1,
        2,
        3,
        4,
        5,
        63,
        64,
        65,
        65_537,
        65_538,
        65_539,
        1_000_003,
        u64::MAX - 1,
        u64::MAX,
    ];
    for w in widths {
        for ino in inos {
            assert_eq!(
                slot_of_ino(ino, w),
                route_ino_width(ino, w).0,
                "lock homing diverged from metadata routing at ino {ino} width {w}"
            );
        }
    }
}

/// Contract 1: the `W ≤ 1` identity arm — every object homes to slot 0.
/// This is the in-RAM test constructor's and every legacy single-meta
/// volume set's shape; it must never produce a slot the set does not
/// have.
#[test]
fn width_one_or_zero_homes_everything_on_slot_zero() {
    for w in [0u64, 1] {
        for ino in [1u64, 2, 3, 4, 999, u64::MAX] {
            assert_eq!(
                slot_of_ino(ino, w),
                0,
                "W = {w} must be the identity arm: ino {ino} homed off slot 0"
            );
        }
    }
}

/// Contract 1: the edges. Ino 1 (the root) pins to slot 0 by definition;
/// ino 2 is the first routed ino and also lands on slot 0; ino 3 is the
/// first ino on slot 1; and `ino + W` shares a home with `ino` — the
/// property the same-slot concurrency family below is built from.
#[test]
fn ino_edges_and_stride_share_homes() {
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    assert_eq!(slot_of_ino(1, w), 0, "the root ino pins to slot 0");
    assert_eq!(slot_of_ino(2, w), 0, "ino 2 is slot 0's first local ino");
    assert_eq!(slot_of_ino(3, w), 1, "ino 3 opens slot 1");
    assert_eq!(slot_of_ino(2 + w, w), 0, "ino 2 + W wraps back onto slot 0");
    assert_eq!(slot_of_ino(w + 1, w), w - 1, "the last slot is W − 1");
    for k in 0..4u64 {
        assert_eq!(
            slot_of_ino(BAND + 5 + k * w, w),
            slot_of_ino(BAND + 5, w),
            "the +k·W stride must preserve the home slot"
        );
    }
}

/// Contract 1: the lock object's home comes from the PUBLISHED width
/// (what the mounted volume set froze), and a non-inode object — which
/// has no ino and therefore no routed home — pins to slot 0, the root's
/// slot, whose owner is a member of every set.
#[tokio::test]
async fn home_slot_follows_the_published_width() {
    let _serial = GLOBAL_STATE.write().await;
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    let _width = publish_width(w);

    assert_eq!(routing_width(), w, "the published width must be readable");
    assert_eq!(lock_home_slot("inode_3"), 1);
    assert_eq!(lock_home_slot(&format!("inode_{}", 2 + w)), 0);
    // Non-inode and malformed-inode keys are opaque objects: slot 0.
    for opaque in ["writer_claim", "inode_", "inode_abc", "inode_12x"] {
        assert_eq!(
            lock_home_slot(opaque),
            0,
            "an object with no routed ino must pin to slot 0: {opaque}"
        );
    }

    // A narrower width re-homes the same object — homing is not cached
    // anywhere, so a set's frozen width is always the live answer.
    publish_routing_width(4);
    assert_eq!(lock_home_slot("inode_7"), 1, "(7 − 2) % 4 = 1");
    assert_eq!(lock_home_slot("inode_6"), 0, "(6 − 2) % 4 = 0");
}

// ---------------------------------------------------------------------------
// 2. Ownership is queried; solo answers local
// ---------------------------------------------------------------------------

/// Contract 2: solo mode reports itself and owns every slot — including
/// slots beyond the width and the extremes, because the acquire path must
/// never be able to synthesize a "not mine" answer on a single-node
/// mount.
#[tokio::test]
async fn solo_mode_owns_every_slot_and_reports_solo() {
    let _serial = GLOBAL_STATE.read().await;
    assert_eq!(
        dlm_mode(),
        "solo",
        "a single-node mount must report dlm_mode=solo (spec §6.9)"
    );
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    for slot in [0, 1, 2, w / 2, w - 1, w, w + 1, u64::MAX] {
        assert!(
            is_local_slot(slot),
            "solo mode must own slot {slot} unconditionally"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. `dlm_rpcs == 0` by construction, every acquire shape
// ---------------------------------------------------------------------------

/// Contract 3 — the gate itself. Every acquire shape the product issues,
/// each asserted to cost ZERO network operations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dlm_rpcs_is_zero_across_every_acquire_shape() {
    let _serial = GLOBAL_STATE.read().await;
    let before = dlm_rpcs();
    let a = dlm();
    let b = dlm();
    let path = format!("inode_{}", BAND + 101);

    // (a) uncontended whole-file — the `get_or_acquire_lease` shape.
    let held = a
        .acquire_lock(&path, None, Duration::from_secs(5))
        .await
        .expect("uncontended acquire");

    // (b) lease refresh: the liveness probe a cached lease answers from,
    // plus the fencing read every save_metadata does.
    assert!(held.is_held().await, "the fresh lease must be held");
    assert_eq!(
        a.get_fencing_token(&path),
        held.fencing_token(),
        "the fencing read must serve the held lease's own token"
    );

    // (c) contended — bounded, loud, and network-free.
    match timeout(
        Duration::from_secs(3),
        b.acquire_lock(&path, None, Duration::from_millis(200)),
    )
    .await
    {
        Err(_) => panic!("contended acquire hung unbounded (200 ms budget)"),
        Ok(Ok(_)) => panic!("an exclusive lock must conflict while held"),
        Ok(Err(_)) => {}
    }
    held.release().await.expect("release");

    // (d) re-acquire after release (the refresh half of a lease cycle).
    let again = a
        .acquire_lock(&path, None, Duration::from_secs(5))
        .await
        .expect("re-acquire after release");
    again.release().await.expect("release again");

    // (e) byte-range acquire (S11 custody), granted and refused.
    let range = a
        .acquire_lock(&path, Some((0, 4 * MIB)), Duration::from_secs(5))
        .await
        .expect("range acquire");
    assert!(
        b.acquire_lock(&path, Some((MIB, 2 * MIB)), Duration::from_millis(200))
            .await
            .is_err(),
        "an overlapping range must arbitrate"
    );
    range.release().await.expect("release range");

    // (f) the moded entry point (EX; CW still ships disabled).
    let moded = a
        .acquire_lock_mode(&path, None, LockMode::Exclusive, Duration::from_secs(5))
        .await
        .expect("EX through the moded entry point");
    moded.release().await.expect("release moded");

    assert_eq!(
        dlm_rpcs(),
        before,
        "solo mode must issue ZERO lock RPCs — the S4 gate (spec §6.9)"
    );
    assert_eq!(dlm_mode(), "solo", "the mode must still read solo");
}

// ---------------------------------------------------------------------------
// 4. Composition with S11 byte-range custody
// ---------------------------------------------------------------------------

/// Contract 4: the S11 arbitration must come THROUGH the slot layer
/// unchanged — a span is not part of an object's identity, so both
/// disjoint ranges home to the same slot, both are owned locally, and
/// both are held at once (proven by a barrier crossed while both leases
/// are live).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disjoint_ranges_stay_concurrent_through_the_slot_layer() {
    let _serial = GLOBAL_STATE.read().await;
    let before = dlm_rpcs();
    let path = format!("inode_{}", BAND + 102);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut set = tokio::task::JoinSet::new();

    for (i, (start, end)) in [(0, 4 * MIB), (4 * MIB, 8 * MIB)].into_iter().enumerate() {
        let d = dlm();
        let path = path.clone();
        let barrier = barrier.clone();
        set.spawn(async move {
            let lease = d
                .acquire_lock(&path, Some((start, end)), Duration::from_secs(5))
                .await
                .unwrap_or_else(|e| panic!("disjoint range {i} refused: {e:?}"));
            barrier.wait().await;
            assert!(lease.is_held().await, "range {i} must still be held");
            lease.release().await.expect("release");
        });
    }
    timeout(Duration::from_secs(20), async {
        while let Some(joined) = set.join_next().await {
            joined.expect("range task panicked");
        }
    })
    .await
    .expect("disjoint ranges serialized through the slot layer");

    // The conflicting face, same object, same home slot.
    let a = dlm();
    let b = dlm();
    let held = a
        .acquire_lock(&path, Some((0, 8192)), Duration::from_secs(5))
        .await
        .expect("holder acquire");
    assert!(
        b.acquire_lock(&path, Some((4096, 12288)), Duration::from_millis(200))
            .await
            .is_err(),
        "an overlapping range must still arbitrate through the slot layer"
    );
    held.release().await.expect("release");

    assert_eq!(
        dlm_rpcs(),
        before,
        "range custody must stay network-free in solo mode"
    );
}

// ---------------------------------------------------------------------------
// 5. Fencing monotonicity through the new layer
// ---------------------------------------------------------------------------

/// Contract 5: the S1/S2 fencing surface is byte-for-byte what it was —
/// the layer adds homing, never a second generator. Every property the
/// ~24-site census depends on is re-asserted through the slot manager:
/// exact read while held, strictly greater on re-acquisition, no
/// regression on release, and the path/ino reads agreeing.
#[tokio::test]
async fn fencing_stays_monotone_through_the_slot_layer() {
    let _serial = GLOBAL_STATE.read().await;
    let ino = BAND + 103;
    let path = format!("inode_{ino}");
    let d = dlm();

    let l1 = d
        .acquire_lock(&path, None, Duration::from_secs(5))
        .await
        .expect("acquire 1");
    let t1 = l1.fencing_token();
    assert!(t1 > 0, "a real grant carries a token > 0");
    assert_eq!(
        d.get_fencing_token_ino(ino),
        t1,
        "the held read must be the holder's own token exactly"
    );
    assert_eq!(
        d.get_fencing_token(&path),
        t1,
        "the path and ino reads are one generator"
    );
    l1.release().await.expect("release 1");
    assert!(
        d.get_fencing_token_ino(ino) >= t1,
        "the read regressed below a granted token after release"
    );

    let l2 = d
        .acquire_lock(&path, None, Duration::from_secs(5))
        .await
        .expect("acquire 2");
    assert!(
        l2.fencing_token() > t1,
        "tokens must stay strictly monotone across re-acquisition: {t1} -> {}",
        l2.fencing_token()
    );
    // A range grant on the same object shares the FILE's generator (S11).
    let r = d
        .acquire_lock(&path, Some((0, MIB)), Duration::from_millis(200))
        .await;
    assert!(
        r.is_err(),
        "a range under a live whole-file lease must still conflict"
    );
    l2.release().await.expect("release 2");
}

// ---------------------------------------------------------------------------
// 6. Concurrency: same-slot and distinct-slot ino families
// ---------------------------------------------------------------------------

/// Contract 6: hammer acquire/release across an ino family that all homes
/// to ONE slot (`base + k·W` — the shape a future single remote owner
/// would serialize) and one that spreads over distinct slots, two workers
/// per ino so every ino is genuinely contended. Everything must be
/// granted, tokens must stay globally unique, and the ledger must stay 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_acquires_across_same_and_distinct_slots() {
    let _serial = GLOBAL_STATE.write().await;
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    let _width = publish_width(w);
    let before = dlm_rpcs();

    const FAMILY: u64 = 8;
    const ROUNDS: u64 = 16;
    let base_same = BAND + 200;
    let base_spread = BAND + 400;

    // Premise: the two families really do home the way the names claim.
    for k in 0..FAMILY {
        assert_eq!(
            slot_of_ino(base_same + k * w, w),
            slot_of_ino(base_same, w),
            "the same-slot family must share one home"
        );
    }
    let spread: std::collections::HashSet<u64> = (0..FAMILY)
        .map(|k| slot_of_ino(base_spread + k, w))
        .collect();
    assert_eq!(
        spread.len(),
        FAMILY as usize,
        "the distinct-slot family must cover {FAMILY} homes"
    );

    let tokens = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let grants = Arc::new(AtomicU64::new(0));
    let mut set = tokio::task::JoinSet::new();

    for base in [base_same, base_spread] {
        let stride = if base == base_same { w } else { 1 };
        for k in 0..FAMILY {
            // Two workers per ino: every ino is contended.
            for _ in 0..2 {
                let d = dlm();
                let tokens = tokens.clone();
                let grants = grants.clone();
                let path = format!("inode_{}", base + k * stride);
                set.spawn(async move {
                    for _ in 0..ROUNDS {
                        let lease = d
                            .acquire_lock(&path, None, Duration::from_secs(20))
                            .await
                            .unwrap_or_else(|e| panic!("{path} acquire refused: {e:?}"));
                        tokens.lock().unwrap().push(lease.fencing_token());
                        grants.fetch_add(1, Ordering::Relaxed);
                        lease.release().await.expect("release");
                    }
                });
            }
        }
    }

    timeout(Duration::from_secs(60), async {
        while let Some(joined) = set.join_next().await {
            joined.expect("hammer task panicked");
        }
    })
    .await
    .expect("the acquire hammer did not drain");

    let all = tokens.lock().unwrap().clone();
    let expected = (FAMILY * 2 * ROUNDS * 2) as usize;
    assert_eq!(all.len(), expected, "every round must have been granted");
    assert_eq!(grants.load(Ordering::Relaxed) as usize, expected);
    let unique: std::collections::HashSet<u64> = all.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "the single mint must stay globally unique under the hammer"
    );
    assert_eq!(
        dlm_rpcs(),
        before,
        "the hammer must have issued ZERO lock RPCs"
    );
}

// ---------------------------------------------------------------------------
// 7. A foreign home refuses loud and counts the RPC site
// ---------------------------------------------------------------------------

/// Contract 7: the ownership question is real. Install a per-slot owner
/// table where THIS node owns only slot 0 (the documented test seam —
/// production has no non-solo table until S6/S8 ship the remote arm):
///
/// * an object homed on slot 0 acquires exactly as before, ledger flat;
/// * an object homed elsewhere is **refused loud** — never granted
///   locally, which would be the silent-divergence bug the whole stage
///   exists to make impossible — and the refusal increments `dlm_rpcs`
///   at the site the remote acquire will occupy;
/// * a byte-range request on a foreign-home object refuses the same way
///   (homing is per object, not per span);
/// * `dlm_mode` stops reporting `solo`;
/// * restoring solo restores the grant, and the ledger never decreases.
#[tokio::test]
async fn foreign_home_slot_refuses_loud_and_counts_the_rpc_site() {
    let _serial = GLOBAL_STATE.write().await;
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    let _width = publish_width(w);
    let d = dlm();

    // The next ino at or above `BAND + 600` that homes to slot 0.
    let local_ino = BAND + 600 + (w - (BAND + 598) % w) % w;
    assert_eq!(slot_of_ino(local_ino, w), 0, "premise: slot 0");
    let foreign_ino = local_ino + 1; // the next slot
    assert_eq!(slot_of_ino(foreign_ino, w), 1, "premise: slot 1");
    let local_path = format!("inode_{local_ino}");
    let foreign_path = format!("inode_{foreign_ino}");

    test_set_local_slots(Some(&[0]));
    let armed = dlm_rpcs();
    assert_ne!(
        dlm_mode(),
        "solo",
        "with an owner table installed the mode is no longer solo"
    );
    assert!(is_local_slot(0), "slot 0 is ours");
    assert!(!is_local_slot(1), "slot 1 is foreign");

    // The owned home still grants, network-free.
    let ours = d
        .acquire_lock(&local_path, None, Duration::from_secs(5))
        .await
        .expect("an owned home must still grant locally");
    ours.release().await.expect("release");
    assert_eq!(
        dlm_rpcs(),
        armed,
        "an owned home must not touch the RPC ledger"
    );

    // The foreign home refuses loud, and the refusal is counted.
    let err = d
        .acquire_lock(&foreign_path, None, Duration::from_secs(5))
        .await
        .err()
        .expect("a foreign home must never be granted locally");
    let msg = format!("{err}");
    assert!(
        msg.contains("slot"),
        "the refusal must name the home slot: {msg}"
    );
    assert_eq!(
        dlm_rpcs(),
        armed + 1,
        "the refusal must count at the RPC site (dlm_rpcs is a live counter)"
    );

    // Ranges home identically — a span is not part of the identity.
    assert!(
        d.acquire_lock(&foreign_path, Some((0, MIB)), Duration::from_secs(5))
            .await
            .is_err(),
        "a byte-range request on a foreign home must refuse too"
    );
    assert_eq!(dlm_rpcs(), armed + 2, "the range refusal counts as well");

    // Restore solo: the same object is grantable again and the ledger,
    // being a lifetime counter, never decreases.
    test_set_local_slots(None);
    assert_eq!(dlm_mode(), "solo", "solo must be restorable");
    assert!(is_local_slot(1), "solo owns every slot again");
    let back = d
        .acquire_lock(&foreign_path, None, Duration::from_secs(5))
        .await
        .expect("solo mode must grant every home");
    back.release().await.expect("release");
    assert_eq!(
        dlm_rpcs(),
        armed + 2,
        "the ledger is monotone and solo adds nothing to it"
    );
}

// ---------------------------------------------------------------------------
// 8. The handle alias
// ---------------------------------------------------------------------------

/// Contract 8: `DlmClient` — the name every historical call site uses —
/// resolves to the S4 slot manager, and the slot manager satisfies
/// `LockManager` (the surface later stages' remote implementation must
/// also satisfy). This is what makes homing + ownership universal
/// without editing ~200 call sites.
#[tokio::test]
async fn dlm_client_alias_is_the_slot_lock_manager() {
    let _serial = GLOBAL_STATE.read().await;
    fn is_lock_manager<M: LockManager>(_: &M) {}
    let via_alias: DlmClient = DlmClient::new().expect("alias");
    let direct: SlotLockManager = SlotLockManager::new().expect("slot manager");
    is_lock_manager(&via_alias);
    is_lock_manager(&direct);

    let path = format!("inode_{}", BAND + 900);
    // The trait surface and the inherent surface are one object.
    let lease = LockManager::acquire_lock(&via_alias, &path, None, Duration::from_secs(5))
        .await
        .expect("trait acquire");
    assert_eq!(
        LockManager::get_fencing_token(&direct, &path),
        lease.fencing_token(),
        "both handles read one generator"
    );
    lease.release().await.expect("release");
}
