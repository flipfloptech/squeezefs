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
