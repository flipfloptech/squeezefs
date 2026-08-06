//! Per-NIC structural census — the whole-NIC face of the no-harm
//! economics arm (engagement round 3, 2026-08-06).
//!
//! The pool term (the RX ring's standing provider demand — see
//! `super::area::ring_standing_bytes`) is per-NIC PHYSICS: every lane
//! session on a rail shares the same driver, ring geometry and MTU, so
//! when HALF the sessions ever armed on a NIC prove the term
//! structural, the remainder share it — holding their RSS exclusion is
//! pure rent. Round-2 field: 5 of 10 sessions tore down structurally
//! and the surviving 5 held ~16 % of the queue width at ~0 engagement
//! for the rest of the row (the residual 5–7 % A-vs-B gap). The ½
//! majority boundary is the same derivation as the per-session
//! economics arm (never a fresh constant); the census is EVER-ARMED
//! (monotone — a mount arms each device once, and a majority verdict
//! about shared physics does not un-prove itself when a session dies).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

#[derive(Default)]
struct NicCensus {
    /// Sessions ever armed on this NIC (monotone within the process).
    armed: u32,
    /// Sessions whose starvation proved structural (either arm).
    structural: u32,
    /// Sessions RELEASED (torn down — structural, poison, or shutdown;
    /// round 4): a torn session holds no RSS exclusion, so it carries
    /// no rent into the majority denominator.
    released: u32,
}

static CENSUS: LazyLock<Mutex<HashMap<u32, NicCensus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn census() -> MutexGuard<'static, HashMap<u32, NicCensus>> {
    // Plain counters — recover from a panicked holder.
    CENSUS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A lane session armed on `ifindex` (the real-backend arm site).
pub fn note_armed(ifindex: u32) {
    census().entry(ifindex).or_default().armed += 1;
}

/// A session on `ifindex` tore down (ANY reason — the voter's own
/// structural teardown, a poison, or shutdown): it leaves the rent
/// denominator (round 4 — the live-armed adjudication).
pub fn note_released(ifindex: u32) {
    let e = &mut *census();
    let e = e.entry(ifindex).or_default();
    e.released = e.released.saturating_add(1).min(e.armed);
}

/// A session on `ifindex` proved structurally starved (either no-harm
/// arm). `true` ⇔ the voters now dominate the LIVE-armed set — the
/// caller releases the WHOLE NIC (sweeps the remaining live sessions
/// so their RSS width restores mid-row).
///
/// Round-4 adjudication (the field's structural=4-of-10 miss): the
/// denominator is LIVE-armed (`armed − released`), not ever-armed —
/// the vote asks "do the sessions that proved the shared pool term
/// outnumber half the sessions still PAYING RENT?", and a torn session
/// (voter or poisoned peer) holds no RSS exclusion. The voter votes
/// while still counted live (its own `note_released` follows in its
/// teardown), so the round-4 tape's 4th vote fires
/// (2×4 ≥ 10 − 3 released) where the ever-armed rule idled one vote
/// short for the whole row.
pub fn note_structural(ifindex: u32) -> bool {
    let mut reg = census();
    let e = reg.entry(ifindex).or_default();
    e.structural += 1;
    let live = e.armed.saturating_sub(e.released).max(1);
    e.structural * 2 >= live
}
