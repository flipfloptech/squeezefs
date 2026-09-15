//! The symmetric manager's service-edge ALLOCATION bounds
//! (design-symmetric-metadata §5.3.5 / §6.3; PR 3 review round 2, the
//! Issue-2 residual and Issue 19) — "bounded codec = bounded EXECUTION",
//! measured with a peak-allocation gauge (the `job_wire_bounds_tests`
//! instrument): a decoded `ReturnExtents` frame's integers must never
//! buy an allocation proportional to `frame runs × record extents`. Before
//! the fix the record intersection emitted one extent per (frame run
//! × record extent) BEFORE its dedup — ~250K copies of one run over a
//! 4,096-extent record was an 8 GiB list on the node holding the manager
//! lease. Now the frame's runs are COALESCED first (bounded by the
//! frame's own run count), then intersected as disjoint intervals with
//! the record (bounded by the record), and `already` is counted after the
//! coalesce so a duplicated run is one extent, not two.

use squeezefs::meta_backend::kv::appender::{
    coalesce_runs, intersect_coalesced_with_record, runs_extent_count, GrantRun,
};
use squeezefs::meta_backend::kv::slot_state::ExtentGrantRecord;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

// ---------------------------------------------------------------------------
// The allocation instrument: a per-thread PEAK single-allocation gauge.
// ---------------------------------------------------------------------------

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

struct PeakAlloc;

// SAFETY: delegates verbatim to `System`; the accounting side effect is a
// const-initialized thread-local Cell update that never allocates.
unsafe impl GlobalAlloc for PeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn record(size: usize) {
    ARMED.with(|a| {
        if a.get() {
            PEAK.with(|p| p.set(p.get().max(size)));
        }
    });
}

#[global_allocator]
static GLOBAL: PeakAlloc = PeakAlloc;

/// Run `f` with the peak-allocation gauge armed; returns `(output, peak)`.
fn peak_alloc<T>(f: impl FnOnce() -> T) -> (T, usize) {
    PEAK.with(|p| p.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, PEAK.with(|p| p.get()))
}

/// The adversarial shape: 10,000 copies of one run covering a
/// 4,096-extent record (a CONTROL-class body holds ~250K of them). The
/// peak single allocation stays within a small constant of the FRAME's
/// own bytes plus the RECORD's — never `runs × record`.
#[test]
fn a_duplicated_return_frame_allocates_frame_plus_record_never_their_product() {
    const RUNS: usize = 10_000;
    const RECORD_EXTENTS: u64 = 4_096;
    let record = ExtentGrantRecord::from_extents(100..100 + RECORD_EXTENTS);
    let runs: Vec<GrantRun> = (0..RUNS)
        .map(|_| GrantRun {
            start: 0,
            len: 1 << 20,
        })
        .collect();
    let (inside, peak) =
        peak_alloc(|| intersect_coalesced_with_record(&coalesce_runs(&runs), &record));
    assert_eq!(
        inside.len() as u64,
        RECORD_EXTENTS,
        "every record extent, once"
    );
    assert!(inside.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
    let frame_bytes = RUNS * std::mem::size_of::<GrantRun>();
    let record_bytes = RECORD_EXTENTS as usize * std::mem::size_of::<u64>();
    // The product would be 10,000 × 4,096 × 8 B = 320 MiB; the bound is a
    // small constant over the two inputs (the coalesce's sorted copy of
    // the frame's runs and the output's growth doubling).
    let bound = 2 * frame_bytes + 2 * record_bytes;
    assert!(
        peak <= bound,
        "peak single allocation {peak} B exceeds the frame+record bound {bound} B \
         (frame {frame_bytes} B, record {record_bytes} B) — the intersection allocated \
         proportional to runs × record"
    );
}

/// Issue 19: `already` is counted after the coalesce — a run named twice
/// (or overlapping) is one extent, and the coalesced count is exactly the
/// distinct extents the frame names.
#[test]
fn duplicated_and_overlapping_runs_count_once_after_the_coalesce() {
    let runs = [
        GrantRun { start: 10, len: 5 },
        GrantRun { start: 10, len: 5 },
        GrantRun { start: 12, len: 6 },
        GrantRun { start: 30, len: 1 },
        GrantRun { start: 30, len: 1 },
        GrantRun { start: 40, len: 0 },
        GrantRun {
            start: u64::MAX,
            len: 2,
        },
    ];
    let coalesced = coalesce_runs(&runs);
    assert_eq!(
        coalesced,
        vec![
            GrantRun { start: 10, len: 8 },
            GrantRun { start: 30, len: 1 }
        ],
        "merged, deduplicated, the empty and the overflowing run dropped"
    );
    assert_eq!(runs_extent_count(&coalesced), 9);
    assert_eq!(runs_extent_count(&runs), 20, "the raw sum double-counts");
    let record = ExtentGrantRecord::from_extents([11u64, 12, 13, 30, 31]);
    assert_eq!(
        intersect_coalesced_with_record(&coalesce_runs(&runs), &record),
        vec![11, 12, 13, 30]
    );
}

/// Review round 6, Issue 29 — the slot words a wire `ReleaseSlot` carries
/// are screened against DURABLE / derived bounds before any RAM or durable
/// effect (`screen_release_words`, the pure edge `KvMetaBackend::
/// manager_release_slot_wire` runs): every poisoned word is refused with
/// its own class, the legitimate words — the grant's with the frontier the
/// lessee's raise left — pass, and the derived frontier bound
/// (`release_seq_floor_bound`) never exceeds the sane seq-space maximum
/// whatever the page says.
#[test]
fn a_release_frames_slot_words_are_screened_against_durable_bounds() {
    use squeezefs::meta_backend::kv::appender::{
        release_seq_floor_bound, root_extent_of, screen_release_words, ReleaseWordBounds,
        ReleaseWordRefusal, SEQ_FRONTIER_SANE_MAX,
    };
    use squeezefs::slot_lease_core::SlotWords;
    const NODE: u64 = 64 * 1024;
    const HEAP: u64 = 1 << 20;
    let grant = ExtentGrantRecord::from_extents([5u64, 6, 7, 40, 41]);
    let bounds = ReleaseWordBounds {
        seq_floor_recorded: 1_000,
        seq_floor_max: release_seq_floor_bound(0, Some(1_000), 4096, 512 * 1024),
        cursor_recorded: 18,
        cursor_max: 1 << 40,
        root_recorded: (HEAP + 6 * NODE, 9),
        heap_base: HEAP,
        node_size: NODE,
        total_extents: 5_120,
    };
    assert_eq!(bounds.seq_floor_max, 1_001 + 4096 + 2 * 512 * 1024);
    let legit = SlotWords {
        root: (HEAP + 6 * NODE, 9),
        cursor: 18,
        extents: 3,
        seq_floor: 1_001,
    };
    assert_eq!(screen_release_words(&legit, &bounds, &grant), Ok(()));
    // A moved root inside the grant passes the pure screen (the header
    // read is the caller's second witness).
    let moved = SlotWords {
        root: (HEAP + 41 * NODE, 12),
        ..legit
    };
    assert_eq!(screen_release_words(&moved, &bounds, &grant), Ok(()));
    let cases = [
        (
            SlotWords {
                seq_floor: 1_000,
                ..legit
            },
            ReleaseWordRefusal::SeqFloorBelowGrant {
                floor: 1_000,
                recorded: 1_000,
            },
        ),
        (
            SlotWords {
                seq_floor: u64::MAX,
                ..legit
            },
            ReleaseWordRefusal::SeqFloorAboveBound {
                floor: u64::MAX,
                bound: bounds.seq_floor_max,
            },
        ),
        (
            SlotWords {
                cursor: 17,
                ..legit
            },
            ReleaseWordRefusal::CursorBelowGrant {
                cursor: 17,
                recorded: 18,
            },
        ),
        (
            SlotWords {
                cursor: (1 << 40) + 1,
                ..legit
            },
            ReleaseWordRefusal::CursorAboveNamespace {
                cursor: (1 << 40) + 1,
                max: 1 << 40,
            },
        ),
        (
            SlotWords {
                extents: 5_121,
                ..legit
            },
            ReleaseWordRefusal::ExtentsAboveVolume {
                extents: 5_121,
                total: 5_120,
            },
        ),
        (
            SlotWords {
                root: (0, 0),
                ..legit
            },
            ReleaseWordRefusal::RootOutsideGrant { addr: 0 },
        ),
        (
            SlotWords {
                root: (HEAP + 8 * NODE, 1),
                ..legit
            },
            ReleaseWordRefusal::RootOutsideGrant {
                addr: HEAP + 8 * NODE,
            },
        ),
        (
            SlotWords {
                root: (HEAP + 6 * NODE + 4096, 9),
                ..legit
            },
            ReleaseWordRefusal::RootOutsideGrant {
                addr: HEAP + 6 * NODE + 4096,
            },
        ),
        (
            SlotWords {
                root: (HEAP + 6_000 * NODE, 9),
                ..legit
            },
            ReleaseWordRefusal::RootOutsideGrant {
                addr: HEAP + 6_000 * NODE,
            },
        ),
    ];
    for (words, want) in cases {
        assert_eq!(
            screen_release_words(&words, &bounds, &grant),
            Err(want),
            "{words:?}"
        );
        assert!(!want.to_string().is_empty());
    }
    // The address → extent step refuses everything off the node grid.
    assert_eq!(root_extent_of(HEAP + 7 * NODE, &bounds), Some(7));
    assert_eq!(root_extent_of(HEAP - 1, &bounds), None);
    assert_eq!(root_extent_of(HEAP + 1, &bounds), None);
    assert_eq!(root_extent_of(HEAP + 5_120 * NODE, &bounds), None);
    // The derived bound: the page's offset or the granted floor + 1,
    // whichever is higher, plus the head hint and two ring lengths — and
    // never past the sane maximum however the page lies.
    assert_eq!(release_seq_floor_bound(500, Some(1_000), 0, 0), 1_001);
    assert_eq!(release_seq_floor_bound(5_000, Some(1_000), 0, 0), 5_000);
    assert_eq!(release_seq_floor_bound(0, None, 10, 100), 210);
    assert_eq!(
        release_seq_floor_bound(u64::MAX, Some(u64::MAX), u64::MAX, u64::MAX),
        SEQ_FRONTIER_SANE_MAX
    );
    assert_eq!(SEQ_FRONTIER_SANE_MAX, u64::MAX / 2);
}
