//! W-5 — the fsync economy (e2e perf audit §3.3 item 15 / §5.3 row 15;
//! write ledger #9; `.benchmarks/2026-09-08-w5-fsync-economy.md`).
//!
//! The shipped `fsync` barriered EVERY data namespace on every call and
//! ran its meta legs one after another, with no instrument naming where
//! an fsync's time went. This module carries the three pieces that
//! change that, none of which weakens the DUR-1 ordering law (the data
//! barrier completes STRICTLY BEFORE the metadata barrier that names its
//! blocks — `tests/fsync_durability_contract_tests.rs`):
//!
//! * **`fsync_phase_ns`** — the always-on exact-sum decomposition
//!   ([`FsyncPhase`], [`FsyncProf`]): consecutive wall spans of the
//!   ladder's steps recorded off SHARED boundary instants, so
//!   `Σ phases ≡ total` to the nanosecond whatever the levers do, and
//!   every phase records once per fsync (0 ns for an absent leg).
//! * **The touched-namespace table** ([`TouchedTable`]) — per-ino
//!   "data namespaces written since the last covering barrier", kept as
//!   ONE atomic word per stripe (the `stripe_locks` splitmix64 mix and
//!   the D-3 width law — the DLM table's width, 8 B per stripe): the low
//!   32 bits are per-device ordinal bits (bit 31 = ALL, the unresolvable
//!   fallback), the high 32 a stamp generation. A stamp is a CAS that
//!   ORs its bits AND bumps the generation, so a clear that raced a stamp
//!   fails and leaves the bits set (the next fsync barriers again —
//!   conservative, never a missed barrier). Stripes are shared by
//!   construction and that is CORRECT here, not a compromise: a device
//!   barrier covers every write that completed before it started, so
//!   any fsync's barrier on device D after ino X's stamp covers X's
//!   bytes on D whoever issued it. A false share costs one extra
//!   coalesced barrier; a missed stamp would be the durability hole, so
//!   the stamps ride the ONE translation every layout publish runs
//!   (`DataRouter::block_ref_ops` — the same closure the C8 durable-ref
//!   oracle proves complete) plus the two DMA shapes that change no map
//!   key (the W1 sole-owner patch, the in-place full-block overwrite).
//! * **The barrier plan** ([`plan_barriers`]) — bits → the devices to
//!   barrier: write-through namespaces are skipped (acknowledged writes
//!   are power-safe on completion — `data_volume_write_cache`), an ALL
//!   or unmatched bit barriers every volatile device.
//!
//! The two levers (`SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES`,
//! `SQUEEZEFS_FSYNC_PARALLEL_LEGS`, default on; `0` = the shipped
//! serialized all-namespace shape) latch on first read; the test seams
//! preset the latch.

use crate::fuse_client::LatencyHistogram;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

// ---------------------------------------------------------------------------
// The levers
// ---------------------------------------------------------------------------

/// Lever latches (the free-grace levers' shape): 0 = unread, 1 = on, 2 = off.
static TOUCHED_NAMESPACES: AtomicU8 = AtomicU8::new(0);
static PARALLEL_LEGS: AtomicU8 = AtomicU8::new(0);

fn latched(cell: &AtomicU8, knob: &str) -> bool {
    match cell.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::env_knobs::bool_knob(knob, true);
            cell.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

fn preset(cell: &AtomicU8, on: Option<bool>) {
    cell.store(
        match on {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

/// `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES`: barrier only the data namespaces
/// this ino's blocks landed on since its last covering barrier (default
/// on); off = every namespace, every fsync (the shipped shape).
pub fn touched_namespaces_enabled() -> bool {
    latched(&TOUCHED_NAMESPACES, "SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES")
}

/// `SQUEEZEFS_FSYNC_PARALLEL_LEGS`: issue the independent barrier legs
/// concurrently and await them all (default on); off = one after another.
pub fn parallel_legs_enabled() -> bool {
    latched(&PARALLEL_LEGS, "SQUEEZEFS_FSYNC_PARALLEL_LEGS")
}

/// Test seam: `Some(on)` presets the touched-namespace latch; `None`
/// returns it to the knob.
pub fn test_set_touched_namespaces(on: Option<bool>) {
    preset(&TOUCHED_NAMESPACES, on);
}

/// Test seam: `Some(on)` presets the parallel-legs latch; `None` returns
/// it to the knob.
pub fn test_set_parallel_legs(on: Option<bool>) {
    preset(&PARALLEL_LEGS, on);
}

// ---------------------------------------------------------------------------
// The touched-namespace table
// ---------------------------------------------------------------------------

/// The "some namespace this table cannot name" bit: set by a stamp whose
/// key resolved to no listed device (or a device past
/// [`TouchedTable::MAX_ORDINAL`]); the plan barriers every volatile
/// device for it.
pub const ALL_BIT: u64 = 1 << 31;
const BITS_MASK: u64 = u32::MAX as u64;
const GEN_STEP: u64 = 1 << 32;

/// Per-stripe `gen << 32 | bits` words over the `stripe_locks` mix.
pub struct TouchedTable {
    words: crate::stripe_locks::StripeLocks<AtomicU64>,
}

impl TouchedTable {
    /// The highest device ordinal a stripe word can name (bit 31 is ALL).
    pub const MAX_ORDINAL: u32 = 30;

    /// A table of `width` words (a power of two — the index is a mask).
    pub fn new(width: usize) -> Self {
        Self {
            words: crate::stripe_locks::StripeLocks::new(width),
        }
    }

    /// The stripe population.
    pub fn width(&self) -> usize {
        self.words.width()
    }

    /// The bit a device ordinal stamps: its own for `0..=MAX_ORDINAL`,
    /// [`ALL_BIT`] for `None` (an unassigned device) or anything past it.
    pub fn bit_for_ordinal(ordinal: Option<u32>) -> u64 {
        match ordinal {
            Some(n) if n <= Self::MAX_ORDINAL => 1u64 << n,
            _ => ALL_BIT,
        }
    }

    /// The device bits of an observed word.
    pub fn bits(word: u64) -> u64 {
        word & BITS_MASK
    }

    /// Record that `ino`'s bytes landed on the devices `bits` names: OR
    /// the bits and bump the generation in ONE CAS, so a clear that
    /// observed the word before this stamp cannot succeed.
    pub fn stamp(&self, ino: u64, bits: u64) {
        if bits == 0 {
            return;
        }
        let w = self.words.get_inode_lock(ino);
        let mut cur = w.load(Ordering::Relaxed);
        loop {
            let next = (cur | (bits & BITS_MASK)).wrapping_add(GEN_STEP);
            match w.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    /// The stripe word as it stands (read BEFORE the barriers start).
    pub fn observe(&self, ino: u64) -> u64 {
        self.words.get_inode_lock(ino).load(Ordering::Acquire)
    }

    /// Clear the bits of a word observed by [`Self::observe`] once the
    /// barriers for them completed: succeeds only if NO stamp landed since
    /// the observation (the generation is unchanged); a failure leaves the
    /// bits for the next fsync. The generation itself is kept.
    pub fn clear_observed(&self, ino: u64, observed: u64) -> bool {
        self.words
            .get_inode_lock(ino)
            .compare_exchange(
                observed,
                observed & !BITS_MASK,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

/// The process-wide table (one router per daemon; the width is the DLM
/// tables' D-3 law — a false share costs one extra coalesced barrier).
pub fn touched_table() -> &'static TouchedTable {
    static T: Lazy<TouchedTable> =
        Lazy::new(|| TouchedTable::new(crate::stripe_locks::dlm_stripe_width()));
    &T
}

// ---------------------------------------------------------------------------
// The barrier plan
// ---------------------------------------------------------------------------

/// Which listed devices an fsync barriers for an observed bit set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarrierPlan {
    /// Indices (into the caller's device list) to barrier, ascending.
    pub targets: Vec<usize>,
    /// Namespaces the ino touched (write-through ones included).
    pub touched: usize,
    /// Touched namespaces skipped because their cache is write-through.
    pub write_through_skips: usize,
    /// The plan fell back to every device (ALL bit, or a bit no listed
    /// ordinal names).
    pub all: bool,
}

/// `bits` (a [`TouchedTable`] word's bits) against the listed devices'
/// ordinals and volatility: a device barriers iff it is touched — or the
/// plan is ALL — and its acknowledged writes are volatile.
pub fn plan_barriers(bits: u64, ordinals: &[Option<u32>], volatile: &[bool]) -> BarrierPlan {
    let bits = bits & BITS_MASK;
    let matched: u64 = ordinals
        .iter()
        .filter_map(|o| o.map(|n| TouchedTable::bit_for_ordinal(Some(n))))
        .filter(|b| *b != ALL_BIT)
        .fold(0, |acc, b| acc | b);
    let all = bits & ALL_BIT != 0 || bits & !matched != 0;
    let mut plan = BarrierPlan {
        targets: Vec::new(),
        touched: 0,
        write_through_skips: 0,
        all,
    };
    for (i, (ordinal, is_volatile)) in ordinals.iter().zip(volatile.iter()).enumerate() {
        let touched = all
            || ordinal
                .map(|n| bits & TouchedTable::bit_for_ordinal(Some(n)) != 0)
                .unwrap_or(false);
        if !touched {
            continue;
        }
        plan.touched += 1;
        if *is_volatile {
            plan.targets.push(i);
        } else {
            plan.write_through_skips += 1;
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// The instrument
// ---------------------------------------------------------------------------

/// The fsync ladder's consecutive steps (`fsync_phase_ns`). `repr(usize)`
/// indexes the histogram table directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum FsyncPhase {
    /// The S10 intent barrier (`meta_ship::intents::fsync_ino_barrier`):
    /// a pending mint's flush, or one relaxed load.
    IntentBarrier = 0,
    /// The local data flush: overlay drain, memory-buffer flush, staged
    /// active-block uploads (the DMAs this fsync awaits), lease acquire.
    DataFlush = 1,
    /// The barrier step: the staged payload sync plus one device Fsync
    /// per touched volatile namespace (joined when the lever is on — the
    /// span is then ≈ max(legs), not Σ).
    DataBarrier = 2,
    /// The meta legs that NAME the data: the rewrite-epoch close and the
    /// dirty-layout persist (journal commits, no barrier).
    MetaPublish = 3,
    /// `sync_device_for_ino` — the coalesced meta-volume barrier.
    MetaBarrier = 4,
    /// The S11 `FlushExtents` force through the authority — the RESIDUAL
    /// wait past the local ladder when the lever overlaps it.
    ExtentBarrier = 5,
    /// Handler entry → the last boundary (≡ Σ of the six legs).
    Total = 6,
}

const FSYNC_PHASES: usize = 7;

/// Export order of the family (a pinned contract).
pub const FSYNC_PHASE_NAMES: [&str; FSYNC_PHASES] = [
    "intent_barrier",
    "data_flush",
    "data_barrier",
    "meta_publish",
    "meta_barrier",
    "extent_barrier",
    "total",
];

static FSYNC_PROF: Lazy<[LatencyHistogram; FSYNC_PHASES]> =
    Lazy::new(|| std::array::from_fn(|_| LatencyHistogram::default()));

/// One fsync's phase clock: every `mark` records the span since the
/// previous boundary and moves the boundary, so the six legs partition
/// entry → last boundary exactly. Dropping it records `total` and a
/// 0 ns sample for every leg that never ran (equal counts per phase; a
/// leg an error skipped contributes nothing and neither does `total`
/// for the time past the last boundary — exact-sum holds on every exit).
pub struct FsyncProf {
    t0: std::time::Instant,
    last: std::time::Instant,
    marked: u8,
}

impl FsyncProf {
    /// Start the clock at handler entry.
    pub fn begin() -> Self {
        let now = std::time::Instant::now();
        Self {
            t0: now,
            last: now,
            marked: 0,
        }
    }

    /// Close `phase` at this instant.
    pub fn mark(&mut self, phase: FsyncPhase) {
        let now = std::time::Instant::now();
        FSYNC_PROF[phase as usize].record(now.saturating_duration_since(self.last));
        self.marked |= 1 << (phase as u8);
        self.last = now;
    }
}

impl Drop for FsyncProf {
    fn drop(&mut self) {
        for i in 0..FsyncPhase::Total as usize {
            if self.marked & (1 << i) == 0 {
                FSYNC_PROF[i].record(std::time::Duration::ZERO);
            }
        }
        FSYNC_PROF[FsyncPhase::Total as usize].record(self.last.saturating_duration_since(self.t0));
    }
}

/// `fsync_phase_ns` — `{phase: histogram}`, surfaced ungated on the
/// stats inode.
pub fn fsync_phase_json() -> serde_json::Value {
    let mut phases = serde_json::Map::new();
    for (i, name) in FSYNC_PHASE_NAMES.iter().enumerate() {
        phases.insert((*name).to_string(), FSYNC_PROF[i].to_json());
    }
    serde_json::Value::Object(phases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_moves_the_generation_even_on_a_set_bit() {
        let t = TouchedTable::new(4);
        t.stamp(7, 1);
        let w = t.observe(7);
        t.stamp(7, 1);
        assert_ne!(w, t.observe(7));
        assert_eq!(TouchedTable::bits(t.observe(7)), 1);
    }

    #[test]
    fn zero_stamp_is_a_no_op() {
        let t = TouchedTable::new(4);
        t.stamp(9, 0);
        assert_eq!(t.observe(9), 0);
    }

    /// The exact-sum law itself is pinned by the integration suite under
    /// its serial lock (`tests/fsync_economy_tests.rs`) — the family is
    /// process-global and the lib harness runs its tests in parallel.
    #[test]
    fn plan_skips_write_through_and_falls_back_on_all() {
        let p = plan_barriers(ALL_BIT, &[Some(0), None], &[false, true]);
        assert!(p.all);
        assert_eq!(p.targets, vec![1]);
        assert_eq!(p.touched, 2);
        assert_eq!(p.write_through_skips, 1);
    }
}
