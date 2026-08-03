//! **Per-writer ino lanes** — pre-RC engineering spec §6.2 **item 5**
//! ("`next_ino` is a per-mount atomic over a shared namespace … duplicate
//! inos alias files immediately; also silently underpins IPC binding
//! identity"), behind incompat bit 12, ruling **D9** (built, NOT stamped).
//! Design: `docs/design-mw-cursors-and-incarnation.md`.
//!
//! This is the [`AppendPartition`]-aware face of [`crate::lane_core`]: the
//! arithmetic lives there (one law, shared with item 6's incarnation
//! stamps), and the appender identity comes from the SAME descriptor
//! incompat bit 8's partitioned journal/bitmap/ledger already carry — so a
//! volume never has two disagreeing notions of "who is writer 2".
//!
//! ## The two ino spaces, and why both are laned
//!
//! A create mints a **local** ino in one of two spaces, then encodes it
//! globally through the frozen routing width
//! ([`crate::meta_backend::route_ino_width`]):
//!
//! * the volume's **native** watermark space (`kv::backend`'s `next_ino`),
//!   used when the picked mint slot is the volume's legacy keyspace;
//! * a hosted slot's **guest** space (`slot_cursors`, PR VL5b), used for
//!   the other ~63 slots of the `MINT_SPREAD` rotor.
//!
//! Both are laned. Slot ownership alone is NOT sufficient even though S4's
//! homing function will give each slot one owner: slots MIGRATE online
//! (`migrate-meta-slot`), and a membership disagreement — two writers each
//! believing they host slot `s` — is precisely the state where duplicate
//! inos would appear. Lanes make that structurally impossible instead of
//! trusting the arbitration to be correct, and they make every ino
//! ATTRIBUTABLE to its minter ([`ino_lane_of`]).
//!
//! ## What is NOT here
//!
//! Who may mint, and the partitioning that keeps writers disjoint, is
//! §6.9 **S4/S8**. This module makes the format expressible: mints are
//! lane-disjoint, recovery is lane-correct, and a lane on a volume whose
//! format does not express lanes is refused **loud**.

use super::journal::AppendPartition;
use crate::lane_core::{self, LaneCursor};

/// The first ino a local space hands out: ino 1 is the root pin and locals
/// below 2 are reserved, exactly as the §4.8 watermark and
/// `slot_cursor_core::SlotCursor` already define it.
pub const LOCAL_INO_BASE: u64 = 2;

/// Which local-ino space a lane cursor belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InoSpace {
    /// The volume's native watermark space (its legacy keyspace).
    Native,
    /// A hosted guest slot's space (PR VL5b per-slot cursors).
    Guest(u16),
}

/// The lane (appender id) that minted local ino `local` in a `writers`-way
/// partition — the attribution function. Solo reads as lane 0.
pub fn ino_lane_of(local: u64, writers: u16) -> u64 {
    lane_core::lane_of(local, LOCAL_INO_BASE, u64::from(writers))
}

/// The first local ino appender `part` may ever mint.
pub fn first_ino_in_lane(part: AppendPartition) -> u64 {
    lane_core::first_in_lane(
        LOCAL_INO_BASE,
        u64::from(part.writers()),
        u64::from(part.writer_id()),
    )
}

/// This appender's ino-cursor floor after a mount (§4.8's
/// `max(ledger watermark, replayed + 1)` rule, rounded up into the lane).
///
/// `ledger_next_ino` is the watermark carried by **this appender's own**
/// newest root-ledger record (incompat bit 8's per-writer slot ranges are
/// what make that record exist); `replayed_max` is the highest local ino
/// the merged replay window mentions for this space, 0 if none. Rounding
/// up is what keeps the recovered cursor in-lane; the values it skips are
/// burned, the same way §4.8 already burns a failed create's ino.
pub fn recover_ino_floor(ledger_next_ino: u64, replayed_max: u64, part: AppendPartition) -> u64 {
    let dense = ledger_next_ino.max(replayed_max.saturating_add(1));
    lane_core::next_in_lane_at_or_above(
        dense,
        LOCAL_INO_BASE,
        u64::from(part.writers()),
        u64::from(part.writer_id()),
    )
}

/// A fresh lane cursor for `part`, seeded from a dense floor.
pub fn lane_cursor(part: AppendPartition, floor: u64) -> LaneCursor {
    LaneCursor::new(
        LOCAL_INO_BASE,
        u64::from(part.writers()),
        u64::from(part.writer_id()),
        floor.max(LOCAL_INO_BASE),
    )
}

/// Inos appender `part` has minted with a cursor seeded at `start` and now
/// standing at `cursor` — the exact count `statfs`'s live-inode derivation
/// (POSIX-1) needs under a strided cursor. Solo collapses to
/// `cursor − start`, today's arithmetic.
pub fn minted_in_ino_lane(cursor: u64, start: u64, part: AppendPartition) -> u64 {
    lane_core::minted_between(cursor, start, u64::from(part.writers()))
}
