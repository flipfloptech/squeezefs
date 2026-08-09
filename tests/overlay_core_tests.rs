//! PR B1 — the device-overlay **pure state core** contracts
//! (`docs/design-device-overlay.md` Rev 2, §2 + §8 B1). Red-first: this
//! suite compiles against `squeezefs::overlay_core` /
//! `squeezefs::coverage_core` and fails until the cores exist and
//! express laws 1–3/6/9 as pure transitions.
//!
//! Two halves:
//!
//! 1. **Deterministic law pins** — each law's transition shape, plus the
//!    §2.3 range-claim overlap exclusion (grant/refuse/release-at-CQE:
//!    the `placed_core::PlacedClaims` protocol reused, never forked).
//! 2. **Property tests** (proptest) — arbitrary segment schedules never
//!    violate the laws: no two overlapping in-flight claims are ever
//!    granted, coverage only grows and only via successful completion
//!    (law 3), the completion transition fires exactly once, generations
//!    are strictly monotone per accepted segment (law 6's ordering
//!    word), and destination rollback is admissible only with the
//!    in-flight set empty on a terminal record (law 9).
//!
//! The shared `coverage_core` union (KD-OV-2 — ONE coverage law for RAM
//! and device accumulation) is exercised against `record_write`'s
//! documented semantics; `active_block.rs`'s own suites re-running
//! unmodified is the byte-identical gate.

use proptest::prelude::*;
use squeezefs::coverage_core::CoverageUnion;
use squeezefs::overlay_core::{
    CompleteVerdict, OverlayRecordCore, OverlayState, StoreRefusal, OVERLAY_PAGE,
};

const LEN: u32 = 16 * OVERLAY_PAGE as u32; // a 16-page model block

fn rec() -> OverlayRecordCore {
    OverlayRecordCore::new(LEN, 7)
}

// ---------------------------------------------------------------------
// coverage_core — the shared union law (KD-OV-2)
// ---------------------------------------------------------------------

#[test]
fn coverage_union_is_overlap_safe_and_order_blind() {
    // The RW3b shape: kernel-split out-of-order segments complete the
    // union exactly once, never because a segment's end touches len.
    let mut c = CoverageUnion::new();
    assert!(!c.record(1024, 2048, 4096).completed);
    assert!(
        !c.record(3072, 4096, 4096).completed,
        "end-at-len alone must not fire"
    );
    assert!(!c.record(0, 512, 4096).completed);
    let v = c.record(512, 3072, 4096);
    assert!(v.completed, "the bridging segment completes the union");
    assert!(
        !c.record(0, 4096, 4096).completed,
        "no double-fire after complete"
    );
}

#[test]
fn coverage_union_tracks_out_of_order_runs_and_gaps() {
    let mut c = CoverageUnion::new();
    let v0 = c.record(0, 1024, 8192);
    assert!(!v0.out_of_order, "first touch is the primary run");
    let v1 = c.record(4096, 5120, 8192);
    assert!(v1.out_of_order, "a disjoint run is out-of-order");
    assert_eq!(c.run_count(), 2);
    assert_eq!(c.gaps(8192), vec![(1024, 4096), (5120, 8192)]);
    assert!(c.contains(0, 1024));
    assert!(c.contains(4096, 5120));
    assert!(!c.contains(1024, 4096), "a gap is never covered");
    assert!(
        !c.contains(0, 5120),
        "a range spanning a gap is never covered"
    );
}

#[test]
fn coverage_union_set_full_claims_everything() {
    let mut c = CoverageUnion::new();
    c.record(512, 1024, 4096);
    c.set_full(4096);
    assert!(c.is_full(4096));
    assert!(c.gaps(4096).is_empty());
    assert!(
        !c.record(0, 4096, 4096).completed,
        "set_full consumed the transition"
    );
}

// ---------------------------------------------------------------------
// Laws 2/3 — reserve-before-DMA, publish-coverage-only-after-full-CQE
// ---------------------------------------------------------------------

#[test]
fn coverage_publishes_only_on_successful_complete() {
    let r = rec();
    let t = r.begin_store(0, 4).expect("fresh range claims");
    assert!(
        !r.covered_probe(0, 4),
        "law 3: an in-flight range is never served"
    );
    let v = r.complete_store(t, false);
    assert!(
        matches!(v, CompleteVerdict::NotCovered),
        "a failed CQE publishes nothing"
    );
    assert!(
        !r.covered_probe(0, 4),
        "failed store leaves the range uncovered"
    );

    let t2 = r
        .begin_store(0, 4)
        .expect("failed store's claim released at CQE");
    let v2 = r.complete_store(t2, true);
    assert!(matches!(
        v2,
        CompleteVerdict::Covered {
            coverage_complete: false
        }
    ));
    assert!(
        r.covered_probe(0, 4),
        "law 4's serve authority: covered after full CQE"
    );
}

#[test]
fn completion_transition_fires_exactly_once_at_full_union() {
    let r = rec();
    let mut fired = 0;
    for p in 0..16u32 {
        let t = r.begin_store(p as usize, 1).unwrap();
        if matches!(
            r.complete_store(t, true),
            CompleteVerdict::Covered {
                coverage_complete: true
            }
        ) {
            fired += 1;
            assert_eq!(p, 15, "the transition is the union reaching len");
        }
    }
    assert_eq!(fired, 1, "the write-through-class trigger fires once");
}

// ---------------------------------------------------------------------
// Law 6 + §2.3 — generations for overlap; in-flight overlap exclusion
// ---------------------------------------------------------------------

#[test]
fn overlapping_inflight_stores_are_unrepresentable() {
    // §2.3's stale-DMA counterexample, made unrepresentable: while a
    // store over [0,4) is in flight, an overlapping begin refuses;
    // disjoint ranges proceed concurrently (the 4×1 MiB cohort law).
    let r = rec();
    let t = r.begin_store(0, 4).expect("first claim");
    assert!(
        matches!(r.begin_store(2, 4), Err(StoreRefusal::Overlap)),
        "an overlapping in-flight store must refuse"
    );
    let t2 = r
        .begin_store(4, 4)
        .expect("disjoint ranges run concurrently");
    r.complete_store(t, true);
    r.complete_store(t2, true);
    // The claim releases only at the store CQE — re-admitting the range.
    let t3 = r.begin_store(0, 8).expect("released ranges re-admit");
    r.complete_store(t3, true);
}

#[test]
fn generations_are_strictly_monotone_per_accepted_segment() {
    let r = rec();
    let t1 = r.begin_store(0, 2).unwrap();
    let g1 = t1.generation();
    r.complete_store(t1, true);
    // A re-write of an already-covered range still bumps (law 6: every
    // accepted segment, including re-writes — the read protocol's
    // revalidation word).
    let t2 = r.begin_store(0, 2).unwrap();
    let g2 = t2.generation();
    assert!(g2 > g1, "every accepted segment advances the generation");
    r.complete_store(t2, true);
    assert!(r.read_begin() >= g2);
}

#[test]
fn read_snapshot_revalidation_catches_mid_fetch_accepts() {
    // §5.2's read face: generation EQUALITY, not ≥ — a segment accepted
    // mid-fetch invalidates the snapshot even before its CQE.
    let r = rec();
    let t = r.begin_store(0, 4).unwrap();
    r.complete_store(t, true);
    let snap = r.read_begin();
    assert!(r.read_valid(snap), "quiet record revalidates");
    let t2 = r.begin_store(8, 4).unwrap(); // accept bumps the word
    assert!(
        !r.read_valid(snap),
        "an accept between snapshot and revalidate fails equality"
    );
    r.complete_store(t2, true);
}

// ---------------------------------------------------------------------
// State machine + law 9 — freeze/terminal/rollback
// ---------------------------------------------------------------------

#[test]
fn freeze_refuses_new_stores_and_inflight_drains_to_publishable() {
    let r = rec();
    let t = r.begin_store(0, 4).unwrap();
    assert!(r.freeze(), "Open → Frozen");
    assert!(!r.freeze(), "freeze is once");
    assert_eq!(r.state(), OverlayState::Frozen);
    assert!(
        matches!(r.begin_store(8, 4), Err(StoreRefusal::NotOpen)),
        "a frozen record admits no new segment"
    );
    assert!(
        !r.inflight_empty(),
        "the pre-freeze store is still in flight"
    );
    // The frozen record still completes its in-flight set (fsync step 2).
    assert!(matches!(
        r.complete_store(t, true),
        CompleteVerdict::Covered { .. }
    ));
    assert!(r.inflight_empty());
    assert!(r.mark_published(), "Frozen → Published");
    assert_eq!(r.state(), OverlayState::Published);
}

#[test]
fn law9_rollback_requires_terminal_state_and_empty_inflight() {
    // Never-return-unpublished-to-allocator-while-DMA-in-flight: the
    // MEM-1 class applied to device offsets.
    let r = rec();
    let t = r.begin_store(0, 4).unwrap();
    assert!(
        !r.rollback_admissible(),
        "an Open record never rolls back its dest"
    );
    assert!(r.supersede(), "Open → Superseded");
    assert!(
        !r.rollback_admissible(),
        "law 9: a terminal record with a straggler DMA in flight must wait it out"
    );
    let v = r.complete_store(t, true);
    assert!(
        matches!(v, CompleteVerdict::Superseded),
        "a store completing into a superseded record publishes nothing"
    );
    assert!(
        r.rollback_admissible(),
        "terminal + inflight empty ⇒ the mint owner may free"
    );
}

#[test]
fn fence_drop_is_terminal_and_never_publishable() {
    let r = rec();
    let t = r.begin_store(0, 4).unwrap();
    r.complete_store(t, true);
    assert!(r.fence_drop(), "Open → FenceDropped (W5)");
    assert!(!r.mark_published(), "a fenced record can never publish");
    assert_eq!(r.state(), OverlayState::FenceDropped);
    assert!(matches!(r.begin_store(8, 4), Err(StoreRefusal::NotOpen)));
}

// ---------------------------------------------------------------------
// Property half — arbitrary schedules never violate the laws
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Begin { first: usize, pages: usize },
    Complete { idx: usize, success: bool },
    Freeze,
    Supersede,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0usize..16, 1usize..=16).prop_map(|(first, pages)| Op::Begin { first, pages }),
        6 => (any::<usize>(), any::<bool>()).prop_map(|(idx, success)| Op::Complete { idx, success }),
        1 => Just(Op::Freeze),
        1 => Just(Op::Supersede),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The B1 falsifier's own oracle: any schedule of begins/completes/
    /// freezes/supersedes holds every law expressible as a pure
    /// transition. If a law cannot be expressed here, the state machine
    /// is under-specified — STOP (the B1 ladder rule).
    #[test]
    fn arbitrary_schedules_hold_the_laws(ops in proptest::collection::vec(op_strategy(), 1..64)) {
        let r = rec();
        let mut live: Vec<(usize, usize, squeezefs::overlay_core::StoreTicket)> = Vec::new();
        let mut covered: Vec<(usize, usize)> = Vec::new(); // page ranges proven covered
        let mut last_gen = 0u64;
        let mut completion_fires = 0u32;

        for op in ops {
            match op {
                Op::Begin { first, pages } => {
                    let pages = pages.min(16 - first.min(16));
                    if pages == 0 { continue; }
                    // A refusal is always legal; a grant carries the laws.
                    if let Ok(t) = r.begin_store(first, pages) {
                        // No two overlapping in-flight claims, ever.
                        for &(s, p, _) in &live {
                            prop_assert!(
                                first + pages <= s || s + p <= first,
                                "overlapping in-flight claims granted: [{first},{}) vs [{s},{})",
                                first + pages, s + p
                            );
                        }
                        // Law 6's ordering word: strictly monotone.
                        prop_assert!(t.generation() > last_gen, "generation must advance");
                        last_gen = t.generation();
                        live.push((first, pages, t));
                    }
                }
                Op::Complete { idx, success } => {
                    if live.is_empty() { continue; }
                    let (first, pages, t) = live.remove(idx % live.len());
                    let st = r.state();
                    match r.complete_store(t, success) {
                        CompleteVerdict::Covered { coverage_complete } => {
                            prop_assert!(success, "law 3: only a full-success CQE covers");
                            prop_assert!(
                                st == OverlayState::Open || st == OverlayState::Frozen,
                                "covered publication only on live records"
                            );
                            covered.push((first, pages));
                            if coverage_complete { completion_fires += 1; }
                        }
                        CompleteVerdict::NotCovered => prop_assert!(!success),
                        CompleteVerdict::Superseded => prop_assert!(
                            st == OverlayState::Superseded || st == OverlayState::FenceDropped,
                            "superseded verdict only from a terminal record"
                        ),
                    }
                }
                Op::Freeze => { let _ = r.freeze(); }
                Op::Supersede => { let _ = r.supersede(); }
            }

            // Coverage only grows: everything proven covered stays
            // servable — UNLESS a newer overlapping store is in flight,
            // in which case law 3's read screen (an in-flight range is
            // never served) correctly withholds the serve until its CQE.
            for &(s, p) in &covered {
                let inflight_overlap = live
                    .iter()
                    .any(|&(ls, lp, _)| s < ls + lp && ls < s + p);
                prop_assert_eq!(
                    r.covered_probe(s, p),
                    !inflight_overlap,
                    "published coverage serves exactly when no in-flight store overlaps"
                );
            }
            // Law 3's read screen: in-flight ranges are never served.
            for &(s, p, _) in &live {
                prop_assert!(!r.covered_probe(s, p), "an in-flight range is never servable");
            }
            // Law 9: rollback admissible ⇔ terminal ∧ inflight empty.
            let terminal = matches!(r.state(), OverlayState::Superseded | OverlayState::FenceDropped | OverlayState::Published);
            prop_assert_eq!(r.rollback_admissible(), terminal && live.is_empty());
            prop_assert_eq!(r.inflight_empty(), live.is_empty());
        }
        prop_assert!(completion_fires <= 1, "the completion transition fires at most once");
    }

    /// KD-OV-2: the shared union is overlap-safe and order-blind under
    /// arbitrary segment schedules — completion fires exactly once iff
    /// the union reaches the whole range, and gaps ∪ runs is a partition.
    #[test]
    fn coverage_union_partitions_the_block(segs in proptest::collection::vec((0u32..64, 1u32..=64), 1..32)) {
        let len = 64u32;
        let mut c = CoverageUnion::new();
        let mut model = vec![false; len as usize];
        let mut fired = 0;
        for (s, l) in segs {
            let e = (s + l).min(len);
            if s >= e { continue; }
            let v = c.record(s, e, len);
            for m in &mut model[s as usize..e as usize] { *m = true; }
            if v.completed { fired += 1; }
            prop_assert_eq!(c.is_full(len), model.iter().all(|&m| m));
        }
        // runs ∪ gaps partition [0, len) exactly against the model.
        let mut derived = vec![false; len as usize];
        for (s, e) in c.runs_sorted() {
            for d in &mut derived[s as usize..e as usize] { *d = true; }
        }
        prop_assert_eq!(&derived, &model, "runs must mirror the written model exactly");
        for (s, e) in c.gaps(len) {
            for i in s..e { prop_assert!(!model[i as usize], "a gap must be unwritten"); }
        }
        prop_assert_eq!(fired, u32::from(model.iter().all(|&m| m)), "completion fires exactly once iff full");
    }
}
