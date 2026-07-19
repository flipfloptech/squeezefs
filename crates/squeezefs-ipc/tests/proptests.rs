//! Property suites for the squeezefs-ipc protocol crate (PR L4-1).
//!
//! Concurrency is loom-models' job (exhaustive, small); these properties
//! pin the *sequential* semantics over big random spaces: layout arithmetic
//! invariants, ring FIFO-vs-reference equivalence across laps, and slot
//! state-machine exactly-once/generation discipline across arbitrary
//! park/serve orderings.

use proptest::prelude::*;
use squeezefs_ipc::layout::{Geometry, SessionLayout, PAGE_BYTES, SLOT_BYTES};
use squeezefs_ipc::ring_core::{RingConsumer, RingStorage};
use squeezefs_ipc::slot_core::{ParkOutcome, SlotCore};
use std::collections::VecDeque;

fn valid_geometry() -> impl Strategy<Value = Geometry> {
    // ring_entries: power of two 1..=65536; slots 1..=ring_entries;
    // arena: 1..=1024 pages; max_op 1..=arena.
    (0u32..=16, 1u64..=1024)
        .prop_flat_map(|(ring_pow, arena_pages)| {
            let ring_entries = 1u32 << ring_pow;
            let arena_bytes = arena_pages * PAGE_BYTES;
            (
                Just(ring_entries),
                1..=ring_entries,
                Just(arena_bytes),
                1..=u32::try_from(arena_bytes.min(u64::from(u32::MAX))).unwrap(),
            )
        })
        .prop_map(|(ring_entries, slots, arena_bytes, max_op_bytes)| Geometry {
            ring_entries,
            slots,
            arena_bytes,
            max_op_bytes,
            _pad: 0,
        })
}

proptest! {
    /// Every valid geometry computes a layout whose regions are page-
    /// aligned, strictly ordered, non-overlapping, sized to the geometry,
    /// with the arena last and the total exact.
    #[test]
    fn layout_regions_ordered_aligned_exact(g in valid_geometry()) {
        g.validate().expect("strategy generates valid geometries");
        let l = SessionLayout::compute(&g).expect("valid geometry must compute");

        prop_assert_eq!(l.header_off, 0);
        prop_assert_eq!(l.header_bytes, PAGE_BYTES);
        let regions = [
            (l.header_off, l.header_bytes),
            (l.ring_off, l.ring_bytes),
            (l.slots_off, l.slots_bytes),
            (l.stats_off, l.stats_bytes),
            (l.arena_off, l.arena_bytes),
        ];
        for (off, bytes) in regions {
            prop_assert_eq!(off % PAGE_BYTES, 0, "offset {} not page-aligned", off);
            prop_assert!(bytes > 0);
        }
        for w in regions.windows(2) {
            let (a_off, a_bytes) = w[0];
            let (b_off, _) = w[1];
            prop_assert!(a_off + a_bytes <= b_off, "regions overlap or disorder");
        }
        prop_assert!(l.ring_cells_off >= l.ring_off);
        prop_assert!(
            l.ring_cells_off + u64::from(g.ring_entries) * 8 <= l.ring_off + l.ring_bytes
        );
        prop_assert_eq!(l.slots_bytes, u64::from(g.slots) * SLOT_BYTES);
        prop_assert_eq!(l.arena_bytes, g.arena_bytes);
        prop_assert_eq!(l.arena_off + l.arena_bytes, l.total_bytes);
    }

    /// Ring vs reference model: an arbitrary interleaving of pushes and
    /// pops (single-threaded — the sequential semantics) matches a
    /// VecDeque bounded to the same capacity, across many laps.
    #[test]
    fn ring_matches_reference_model(
        cap_pow in 0u32..=4,
        ops in proptest::collection::vec(any::<bool>(), 1..200),
    ) {
        let capacity = 1u32 << cap_pow;
        let storage = RingStorage::with_capacity(capacity).unwrap();
        let ring = storage.view();
        let mut consumer = RingConsumer::new();
        let mut reference: VecDeque<u32> = VecDeque::new();
        let mut next = 0u32;

        for is_push in ops {
            if is_push {
                let accepted = ring.push(next);
                let ref_accepted = reference.len() < capacity as usize;
                prop_assert_eq!(
                    accepted, ref_accepted,
                    "push acceptance diverged at value {}", next
                );
                if ref_accepted {
                    reference.push_back(next);
                }
                next += 1;
            } else {
                prop_assert_eq!(consumer.pop(&ring), reference.pop_front());
            }
        }
        // Drain both — full equivalence.
        while let Some(expect) = reference.pop_front() {
            prop_assert_eq!(consumer.pop(&ring), Some(expect));
        }
        prop_assert_eq!(consumer.pop(&ring), None);
    }

    /// Slot cycles under arbitrary wait shapes (spin-consume vs park-then-
    /// consume, with the park attempt landing before or after completion):
    /// generations are strictly monotonic, every op completes exactly once
    /// for its own generation, and stale generations never read DONE.
    #[test]
    fn slot_cycles_exactly_once_generations_monotonic(
        shapes in proptest::collection::vec((any::<bool>(), any::<bool>()), 1..50),
    ) {
        let s = SlotCore::new();
        let mut last_gen = 0u64;
        for (park_before_serve, park_after_done) in shapes {
            let gen = s.try_claim().expect("slot must be FREE at cycle start");
            prop_assert!(gen > last_gen, "generation must be strictly monotonic");
            prop_assert!(!s.is_done_for(gen), "fresh claim can not be done");
            prop_assert!(
                !s.is_done_for(last_gen),
                "stale generation must never read done"
            );
            s.publish_submitted();

            if park_before_serve {
                // Client commits to parking before the daemon dequeues.
                let parked = matches!(s.park_prepare(), ParkOutcome::Park { .. });
                prop_assert!(parked, "nothing is done yet: park_prepare must park");
            }
            prop_assert!(s.try_begin_serve());
            let need_wake = s.complete();
            prop_assert_eq!(
                need_wake, park_before_serve,
                "wake needed exactly when a waiter parked"
            );
            if park_after_done {
                // Late parker: publish-then-recheck must say Ready.
                prop_assert_eq!(s.park_prepare(), ParkOutcome::Ready);
            }
            prop_assert!(s.is_done_for(gen), "own generation must consume");
            s.release();
            prop_assert!(!s.is_done_for(gen), "released slot is not done");
            last_gen = gen;
        }
    }
}
