//! Hybrid lane gate — red-first contracts (ruling D14's fleet-posture
//! corollary formalized INSIDE the shim: large ops route to the kernel
//! FUSE lane — zero-copy, line-rate bandwidth — small ops ride the IPC
//! ring, the IOPS lane).
//!
//! The pieces under contract here are the pure cores the interposer
//! funnels compose (`crates/squeezefs-preload/src/lane_gate.rs` + the
//! fd-table's sticky offsetful latch):
//!
//! * the **decision table** — strictly-greater-than-threshold routes
//!   kernel; `0` = gate off entirely (the A/B lever, all eligible ops
//!   ring); at-or-below stays ring;
//! * the **resolve law** — `SQUEEZEFS_IL_KERNEL_LANE_MIN` explicit wins
//!   VERBATIM (even below the slab — that is what an override lever
//!   means), absent takes the derived value, malformed announces and
//!   keeps the derived default (the shim's documented ENG-10 asymmetry:
//!   it never kills its host application over an env typo);
//! * the **memBW micro-probe** — the runtime input the derivation rides
//!   (portable-by-default: runtime behavior, never a CPU-ID table);
//! * the **sticky offsetful latch** — OFFSETFUL forms (`read`/`write`/
//!   `readv`/`writev`) must never lane-flap per op (each ring→Real
//!   transition flushes and permanently demotes the PERF-7 offset
//!   mirror), so the gate latches the whole file DESCRIPTION to the
//!   kernel lane exactly once: monotone, shared by dup siblings (one
//!   description = one f_pos = one lane), cleared only by a fresh bind.
//!
//! The end-to-end interposer legs (LD_PRELOAD, real mount) are
//! `tests/run_preload_gate.sh` territory; the daemon-side stats-page
//! export plumbing is pinned in `tests/preload_parity_tests.rs`.

use squeezefs_il::fd_table::{Binding, FdTable};
use squeezefs_il::lane_gate;

fn binding(id: u64) -> Binding {
    Binding {
        binding_id: id,
        ino: 100 + id,
        read_ok: true,
        write_ok: true,
        session: 0,
    }
}

// ---------------------------------------------------------------------------
// decision table (the ONE routing predicate)
// ---------------------------------------------------------------------------

#[test]
fn gate_routes_strictly_larger_ops_kernel_and_zero_disables() {
    const THR: u64 = 64 * 1024;
    // Below: ring (the IOPS lane keeps its ops).
    assert!(!lane_gate::kernel_lane_route(4096, THR));
    assert!(!lane_gate::kernel_lane_route(THR - 1, THR));
    // AT the threshold: ring — the threshold is the LARGEST op the ring
    // keeps (an exactly-slab op is the single-flight serial path's top
    // shape; routing it kernel would tax the ring's own best regime).
    assert!(!lane_gate::kernel_lane_route(THR, THR));
    // Strictly above: kernel.
    assert!(lane_gate::kernel_lane_route(THR + 1, THR));
    assert!(lane_gate::kernel_lane_route(1024 * 1024, THR));
    // 0 = gate OFF (the A/B lever): everything rides the ring, even
    // huge ops — never "everything > 0 routes kernel".
    assert!(!lane_gate::kernel_lane_route(1, 0));
    assert!(!lane_gate::kernel_lane_route(16 * 1024 * 1024, 0));
    // Degenerate op sizes are never gate business.
    assert!(!lane_gate::kernel_lane_route(0, THR));
    assert!(!lane_gate::kernel_lane_route(0, 0));
}

// ---------------------------------------------------------------------------
// resolve law (explicit-wins-verbatim; the shim's announce-and-default
// asymmetry)
// ---------------------------------------------------------------------------

#[test]
fn resolve_env_wins_verbatim_absent_derives_malformed_keeps_derived() {
    const DERIVED: u64 = 64 * 1024;
    // Absent (unset / empty / whitespace) takes the derived value.
    assert_eq!(lane_gate::resolve_kernel_lane_min(None, DERIVED), DERIVED);
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some(""), DERIVED),
        DERIVED
    );
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some("   "), DERIVED),
        DERIVED
    );
    // Explicit wins VERBATIM — including 0 (gate off, the A/B lever) and
    // values below the slab (an override lever is not clamped).
    assert_eq!(lane_gate::resolve_kernel_lane_min(Some("0"), DERIVED), 0);
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some("16384"), DERIVED),
        16384
    );
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some(" 262144 "), DERIVED),
        262144,
        "values are trimmed (the ONE convention)"
    );
    // Malformed keeps the derived default (announced by the caller —
    // ENG-10's documented shim asymmetry, never a host-app kill).
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some("lots"), DERIVED),
        DERIVED
    );
    assert_eq!(
        lane_gate::resolve_kernel_lane_min(Some("-1"), DERIVED),
        DERIVED
    );
}

// ---------------------------------------------------------------------------
// the memBW micro-probe (runtime-derived input, never a CPU table)
// ---------------------------------------------------------------------------

#[test]
fn mem_bw_probe_measures_something_real() {
    let bw = lane_gate::probe_mem_bw_bytes_per_sec();
    // Any live box copies memory faster than 100 MB/s and slower than
    // 10 TB/s; outside that the probe is broken, not the machine.
    assert!(
        bw > 100_000_000,
        "probe must measure a real copy bandwidth, got {bw} B/s"
    );
    assert!(
        bw < 10_000_000_000_000,
        "probe result is not a plausible memcpy bandwidth: {bw} B/s"
    );
}

#[test]
fn derived_threshold_composes_probe_and_geometry_rails() {
    // The composed shim-side derivation: probed memBW through the shared
    // sizing form, railed by THIS session's geometry. Whatever this box
    // measures, the result must sit inside the physical rails.
    let (slab, max_op) = (64 * 1024u64, 1024 * 1024u64);
    let thr = lane_gate::kernel_lane_min_for_geometry(slab, max_op);
    assert!(
        (slab..=max_op).contains(&thr),
        "derived threshold {thr} must sit in [slab {slab}, max_op {max_op}]"
    );
    // And it is 4 KiB-grained (the LBA/page grain the slab law uses).
    assert_eq!(thr % 4096, 0, "threshold must sit on the 4 KiB grain");
}

// ---------------------------------------------------------------------------
// sticky offsetful latch (per DESCRIPTION, monotone, dup-shared)
// ---------------------------------------------------------------------------

#[test]
fn kernel_lane_latch_is_off_at_bind_and_sticky_once_set() {
    let t = FdTable::new();
    t.bind(3, binding(1));
    let (_b, m) = t.lookup_with_mirror(3).expect("bound fd resolves");
    assert!(
        !m.kernel_lane_latched(),
        "a fresh binding starts on the ring lane"
    );
    m.latch_kernel_lane();
    assert!(m.kernel_lane_latched(), "the latch engages");
    // Sticky: re-latching is idempotent, and a fresh handle to the same
    // cell observes it (the latch lives in the shared BindingCell).
    m.latch_kernel_lane();
    let (_b2, m2) = t.lookup_with_mirror(3).expect("still bound");
    assert!(
        m2.kernel_lane_latched(),
        "the latch is a property of the cell, not the handle"
    );
}

#[test]
fn dup_siblings_share_the_lane_latch_like_they_share_f_pos() {
    // One description = one f_pos = one lane: dup'd fds share the
    // BindingCell, so a large write(2) through either fd latches BOTH —
    // the exact sharing the offset mirror already rides.
    let t = FdTable::new();
    t.bind(3, binding(1));
    t.on_dup(3, 7);
    let (_b, m3) = t.lookup_with_mirror(3).expect("orig bound");
    let (_b, m7) = t.lookup_with_mirror(7).expect("dup bound");
    assert!(!m3.kernel_lane_latched() && !m7.kernel_lane_latched());
    m7.latch_kernel_lane();
    assert!(
        m3.kernel_lane_latched(),
        "dup sibling must observe the latch (shared cell)"
    );
}

#[test]
fn a_fresh_bind_clears_the_latch_with_the_cell() {
    // Rebinding an fd number installs a FRESH cell (new description, new
    // custody): the lane decision restarts from ring — the latch is
    // per-description state, never per-fd-number residue.
    let t = FdTable::new();
    t.bind(3, binding(1));
    let (_b, m) = t.lookup_with_mirror(3).expect("bound");
    m.latch_kernel_lane();
    assert!(m.kernel_lane_latched());
    t.bind(3, binding(2));
    let (b, m) = t.lookup_with_mirror(3).expect("rebound");
    assert_eq!(b.binding_id, 2);
    assert!(
        !m.kernel_lane_latched(),
        "a fresh binding must start unlatched"
    );
    // And an unrelated fd never observes a foreign latch.
    t.bind(9, binding(3));
    let (_b, m9) = t.lookup_with_mirror(9).expect("bound");
    assert!(!m9.kernel_lane_latched());
}
