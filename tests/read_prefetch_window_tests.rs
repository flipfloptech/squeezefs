//! Read-saturation campaign (2026-07-29) — the §5.5 prefetch window
//! economy, extracted into pure decision functions and pinned.
//!
//! The baseline conviction (rsat rig, 16-stream cold sequential rows):
//! `share_blocks = share% × hot_budget / block / active_streams` = **1
//! block per lane** at the default shape (128 MiB hot budget, 4 MiB
//! blocks, 16 streams), and the issue bound charged IN-FLIGHT fetches
//! against that resident share — so the pipeline could never overlap
//! fetch latency with consumption. Measured: prefetch covered 3,701 of
//! 58,073 device fetches on the kernel seq-1M row (6 %; the reader
//! foreground-paid the rest), and the il seq rows ran with no pipeline
//! at all. In-flight fetch bytes are transient DMA buffers (R5-charged
//! via `prefetch_inflight_bytes`), NOT hot-tier residents; only
//! LANDED-unconsumed fills occupy the budget.
//!
//! The economy after this campaign (no fixed constants — the house
//! no-constants law, mirroring the write governor's derived posture):
//!
//! - **Window cap** derives from the budget: `share% × hot_budget /
//!   block_size`, railed [4, 4096]; an explicit
//!   `SQUEEZEFS_READ_PREFETCH_WINDOW` wins verbatim (0 = kill switch).
//! - **Issue admission** splits the two bounds: landed-unconsumed ≤ the
//!   per-lane resident share (budget honesty), in-flight + unconsumed ≤
//!   the AIMD window (plan bound) — in-flight depth is no longer
//!   resident-clamped.
//! - **Growth** fires on foreground-wait (reader caught an in-flight
//!   fetch) OR plan overrun (reader passed the whole issued plan — the
//!   silent-consumption regime's only shallowness signal, since warm
//!   ring serves never wait) — overrun growth only while the lane has
//!   no evicted-unconsumed streak (never grow into demonstrated
//!   starvation), and any growth only in mem-budget Green.
//!
//! The reactive spiral controls are unchanged and remain the safety
//! net: AIMD halving + progress-clocked quiescence on
//! `prefetch_evicted_unconsumed`, spawn-admission shedding, R5
//! Red/Yellow gates (pinned by tests/read_prefetch_pipeline_tests.rs).

use squeezefs::routing::{
    derived_prefetch_window_cap, prefetch_issue_admits, prefetch_window_grows,
};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// derived window cap
// ---------------------------------------------------------------------------

#[test]
fn window_cap_derives_from_the_hot_budget() {
    // The rig default shape: 50 % × 128 MiB / 4 MiB = 16 blocks.
    assert_eq!(derived_prefetch_window_cap(50, 128 * MIB, 4 * MIB), 16);
    // A budget that admits deeper pipelines must yield a deeper cap —
    // the fixed 16 was the constant the no-constants law retires:
    // 50 % × 2 GiB / 4 MiB = 256.
    assert_eq!(derived_prefetch_window_cap(50, 2048 * MIB, 4 * MIB), 256);
    // Small-block volumes scale in blocks: 50 % × 128 MiB / 512 KiB = 128.
    assert_eq!(derived_prefetch_window_cap(50, 128 * MIB, 512 * 1024), 128);
}

#[test]
fn window_cap_is_railed() {
    // Floor 4: below it the AIMD start (2) can never even double once —
    // the pre-derivation posture's minimum useful plan.
    assert_eq!(derived_prefetch_window_cap(50, 0, 4 * MIB), 4);
    assert_eq!(derived_prefetch_window_cap(50, 8 * MIB, 4 * MIB), 4);
    // Rail 4096 (16 GiB of 4 MiB plan per lane is past any BDP this
    // code serves; the rail bounds a pathological budget/block ratio).
    assert_eq!(derived_prefetch_window_cap(100, 1 << 44, 4096), 4096);
    // Zero block size must not divide-by-zero.
    assert_eq!(derived_prefetch_window_cap(50, 128 * MIB, 0), 4);
}

// ---------------------------------------------------------------------------
// issue admission: the two bounds, split
// ---------------------------------------------------------------------------

#[test]
fn inflight_depth_is_not_resident_clamped() {
    // THE starvation pin (red on the baseline formula): a lane whose
    // resident share is 1 block (the 16-stream default shape) with one
    // fetch in flight and nothing landed must be allowed to deepen its
    // pipeline — in-flight bytes are not residents.
    assert!(
        prefetch_issue_admits(4, 16, 1, 1, 0),
        "in-flight depth must be bounded by the AIMD window, not the \
         per-lane resident share (share=1 starved the 16-stream shape \
         to a depth-1 pipeline — the baseline collapse)"
    );
    // Deeper still, while the window allows.
    assert!(prefetch_issue_admits(4, 16, 1, 3, 0));
}

#[test]
fn landed_unconsumed_is_resident_bounded() {
    // Landed fills ARE residents: at the share, issue stops.
    assert!(!prefetch_issue_admits(4, 16, 1, 0, 1));
    assert!(!prefetch_issue_admits(8, 16, 2, 0, 2));
    // Below the share, admit.
    assert!(prefetch_issue_admits(8, 16, 2, 0, 1));
}

#[test]
fn plan_is_window_bounded_and_zero_share_never_speculates() {
    // in_flight + unconsumed ≥ min(window, cap) ⇒ deny.
    assert!(!prefetch_issue_admits(4, 16, 8, 3, 1));
    assert!(!prefetch_issue_admits(16, 8, 8, 7, 1));
    // A share that cannot retain even ONE block: every speculative fill
    // is guaranteed evicted-before-consume — the only non-wasteful
    // window is empty (the §5.5 no-floor rationale, preserved).
    assert!(!prefetch_issue_admits(4, 16, 0, 0, 0));
}

// ---------------------------------------------------------------------------
// growth triggers
// ---------------------------------------------------------------------------

#[test]
fn growth_fires_on_foreground_wait_or_clean_overrun() {
    // Foreground-wait: today's trigger, unchanged.
    assert!(prefetch_window_grows(true, false, 0, true));
    // Plan overrun with a clean lane: the silent-consumption regime's
    // shallowness signal (warm ring serves never wait on the single
    // flight — without this the il pipeline is stuck at the start
    // window forever).
    assert!(prefetch_window_grows(false, true, 0, true));
    // No signal ⇒ no growth.
    assert!(!prefetch_window_grows(false, false, 0, true));
}

#[test]
fn overrun_growth_never_feeds_demonstrated_starvation() {
    // A lane with a live evicted-unconsumed streak is starving, not
    // shallow — growing it would feed the R-5 spiral the AIMD collapse
    // is fighting.
    assert!(!prefetch_window_grows(false, true, 1, true));
    // The foreground-wait trigger keeps its historical semantics even
    // under a streak (the reader demonstrably caught an in-flight fetch
    // — depth, not starvation).
    assert!(prefetch_window_grows(true, false, 3, true));
}

#[test]
fn growth_is_green_gated() {
    // Yellow/Red freeze growth (the §5.7 advisory-at-admission rule).
    assert!(!prefetch_window_grows(true, false, 0, false));
    assert!(!prefetch_window_grows(false, true, 0, false));
}
