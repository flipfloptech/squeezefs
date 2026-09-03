//! **Per-writer lane cursors** — the shared core of pre-RC engineering
//! spec §6.2 **items 5 and 6** (design
//! `docs/design-mw-cursors-and-incarnation.md`).
//!
//! One law, two consumers:
//!
//! * **item 5** — ino minting (`kv::backend`'s `next_ino` / per-slot guest
//!   cursors): two writers sharing one monotone counter mint DUPLICATE
//!   inos, which alias files immediately and — because the daemon's IPC
//!   binding table rests on the monotonic never-reused ino law — alias fd
//!   bindings too;
//! * **item 6** — block-key incarnation stamps (`block_allocator`): the
//!   lifetime component of an `offset ‖ incarnation` key must be
//!   unrepeatable across appenders as well as across mounts.
//!
//! ## The law
//!
//! Over a value space based at `base` and partitioned `writers` ways,
//! value `v` belongs to **lane** `(v − base) % writers`, and appender `w`
//! mints only lane-`w` values: `base + w`, `base + w + writers`, … A lane
//! is therefore a *residue class*, which is what makes the partition
//! **arbitration-free** — an appender needs to know only its own id to
//! mint values no peer can ever mint. There is no range hand-out, no
//! durable distribution watermark, and no negotiation (who MAY mint is
//! §6.9 S4/S8's problem, deliberately not this core's).
//!
//! Two properties follow directly and are what the consumers depend on:
//!
//! 1. **disjointness** — two lanes never produce the same value, so
//!    "never reused" survives N concurrent appenders;
//! 2. **attribution** — [`lane_of`] names the appender that minted any
//!    value, so recovery/fsck can classify a foreign writer's values
//!    instead of guessing.
//!
//! Recovery is the third property: a lane cursor may be seeded from ANY
//! floor that is known to dominate this appender's own committed values
//! ([`next_in_lane_at_or_above`] rounds it up into the lane), and values
//! skipped by the rounding are burned — the same law §4.8 already states
//! for inos ("a failed create burns the ino; crash-skipped ranges waste
//! nothing that matters").
//!
//! **Solo (`writers == 1`) is the shipped path and is arithmetically
//! identical to today's dense counters**: lane 0 is every value and the
//! stride is 1, so [`LaneCursor::mint`] is exactly the `fetch_add(1)` the
//! §4.8 watermark and `slot_cursor_core::SlotCursor` perform. That
//! equivalence is a *tie test*, not a comment
//! (`tests/mw_ino_lane_tests.rs::solo_lane_cursor_ties_the_shipped_cursor`).
//!
//! Dependency-free so `loom-models/` can `#[path]`-include it and
//! exhaustively check the mint / publish / install interleavings. The main
//! build never sets `cfg(loom)`.
//!
//! [`lane_of`]: crate::lane_core::lane_of
//! [`next_in_lane_at_or_above`]: crate::lane_core::next_in_lane_at_or_above
//! [`LaneCursor::mint`]: crate::lane_core::LaneCursor::mint

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// The lane `value` belongs to in a `writers`-way partition of the space
/// based at `base` — the ATTRIBUTION function (which appender minted this
/// value). `writers == 0` is not a partition; it reads as solo.
pub fn lane_of(value: u64, base: u64, writers: u64) -> u64 {
    if writers <= 1 {
        return 0;
    }
    value.wrapping_sub(base) % writers
}

/// The first value appender `writer` may ever mint.
pub fn first_in_lane(base: u64, writers: u64, writer: u64) -> u64 {
    if writers <= 1 {
        return base;
    }
    base + (writer % writers)
}

/// The smallest lane-`writer` value `≥ floor` — the recovery/migration
/// rounding. Idempotent: a floor already in the lane is returned
/// unchanged, so re-seeding never advances a cursor.
pub fn next_in_lane_at_or_above(floor: u64, base: u64, writers: u64, writer: u64) -> u64 {
    if writers <= 1 {
        return floor.max(base);
    }
    let first = first_in_lane(base, writers, writer);
    if floor <= first {
        return first;
    }
    let ahead = lane_of(floor, base, writers);
    let want = writer % writers;
    if ahead == want {
        return floor;
    }
    // Distance forward from `floor`'s lane to ours, in the ring of lanes.
    floor + (want + writers - ahead) % writers
}

/// How many values a lane cursor that was SEEDED at `start` has minted when
/// it stands at `cursor` — the exact count, which is what keeps `statfs`'s
/// live-inode derivation (POSIX-1) honest under a strided cursor.
///
/// The `start` argument is load-bearing and was found by the POSIX-1
/// contract: a lane cursor is seeded from the space's recovered DENSE
/// watermark (which already counts every value below it, in every lane),
/// so counting from the lane's absolute first value instead would count the
/// seeding gap as mints and over-report `IUsed` — the same class of error,
/// one level down, that made `df -i` misreport before the MINT_SPREAD fix.
pub fn minted_between(cursor: u64, start: u64, writers: u64) -> u64 {
    cursor.saturating_sub(start) / writers.max(1)
}

/// One appender's monotone cursor over its own lane.
///
/// The publication edge mirrors `slot_cursor_core::SlotCursor` (whose
/// soundness argument this reuses verbatim): a snapshot taken after a
/// mint's record reached the tree is strictly greater than that minted
/// value, so a durable watermark never under-declares a value it covers;
/// a mint that commits after the snapshot rides the journal and is
/// recovered by replay's fold; a mint that never commits is burned.
#[derive(Debug)]
pub struct LaneCursor {
    next: AtomicU64,
    /// The value this cursor was seeded at (raised in lockstep by
    /// [`LaneCursor::install_floor`], which is recovery/migration and not
    /// minting) — the base [`LaneCursor::minted`] counts from.
    start: AtomicU64,
    base: u64,
    writers: u64,
    writer: u64,
}

impl LaneCursor {
    /// A cursor for appender `writer` of `writers`, over the space based
    /// at `base`, seeded at the smallest lane value `≥ floor`.
    pub fn new(base: u64, writers: u64, writer: u64, floor: u64) -> Self {
        let writers = writers.max(1);
        let writer = writer % writers;
        let seed = next_in_lane_at_or_above(floor, base, writers, writer);
        Self {
            next: AtomicU64::new(seed),
            start: AtomicU64::new(seed),
            base,
            writers,
            writer,
        }
    }

    /// Mint one value: `fetch_add(writers)`, no reuse, no free-on-failure.
    /// Solo is `fetch_add(1)` — today's counter, instruction for
    /// instruction.
    pub fn mint(&self) -> u64 {
        self.next.fetch_add(self.writers, Ordering::AcqRel)
    }

    /// The publication snapshot (the value a durable watermark carries):
    /// every mint whose record-apply happened-before this load is strictly
    /// below the returned value.
    pub fn snapshot(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// Raise the cursor to at least the smallest lane value `≥ floor`.
    /// Monotone — a stale floor can never regress a fresher mint — and
    /// idempotent under re-seeding.
    pub fn install_floor(&self, floor: u64) {
        let want = next_in_lane_at_or_above(floor, self.base, self.writers, self.writer);
        self.next.fetch_max(want, Ordering::AcqRel);
        // Recovery/migration is not minting: re-base the mint count too,
        // or the installed gap would read as this cursor's work.
        self.start.fetch_max(want, Ordering::AcqRel);
    }

    /// Values this cursor has minted since it was seeded
    /// ([`minted_between`] — never from the lane's absolute first value; see
    /// that function's note).
    pub fn minted(&self) -> u64 {
        minted_between(
            self.snapshot(),
            self.start.load(Ordering::Acquire),
            self.writers,
        )
    }

    /// The value this cursor was seeded (or last re-based) at.
    pub fn start(&self) -> u64 {
        self.start.load(Ordering::Acquire)
    }

    /// Appenders this cursor's space is partitioned for.
    pub fn writers(&self) -> u64 {
        self.writers
    }

    /// This cursor's appender id.
    pub fn writer(&self) -> u64 {
        self.writer
    }
}
