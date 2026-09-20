//! Node incarnation seqs — **one SPACE per appender incarnation**
//! (design-symmetric-metadata §5.3.3 as amended by PR 13, defect 5).
//!
//! A node's `node_seq` is the CoW law's identity word: the §4.2 child and
//! root pointer checks, the §4.5 frame-incarnation check that ends a
//! recycled extent's log at a previous node's frames, PR 11's residue
//! ceiling — every one compares seqs for EQUALITY and rests on "no two
//! nodes ever carry one seq". Before this module every appender of a
//! forest volume seeded its handle from the SAME ledger word at its open
//! (`ledger.seq.max(node_seq_watermark)`), so under N daemons lockstep
//! storms minted equal seqs in every daemon and the guards were void
//! ACROSS appenders: the fleet's `sym-scale` N = 8 row read one node
//! extent carrying appender 3's header + base bset and three frames
//! appender 4 appended into it, all under one `node_seq`, each folding the
//! other's records.
//!
//! **The space law.** The volume's base `B = node_seq_base(uuid)` (the
//! builder's — its top bit clear, 2^63 of headroom) is the start of
//! incarnation 0's space: the manager's and every FLAT / unarmed mount's
//! — the legacy handle, byte-identical seeding (`ledger.seq.max(
//! node_seq_watermark)`), raised by every root/pointer it replays as
//! before. Every JOINED appender takes incarnation `o ≥ 1`, a DISJOINT
//! space `[B + o·2^K, B + (o+1)·2^K)` whose ordinal the manager MINTS at
//! `JoinAppender` from the durable tree-0 counter
//! [`NODE_SEQ_INCARNATIONS_KEY`] — monotone, barriered before the reply
//! (a manager dying after the reply never re-mints the ordinal), a REJOIN
//! a new incarnation like any join (its old space is dead: nothing mints
//! there again, so its residue can collide with nothing). The joiner's
//! handle starts at its base and is never RAISED by a replayed pointer
//! (a fresh space has no residue of its own; a raise from a projected or
//! transferred tree's pointer would move it into a foreign space).
//!
//! **`K` derived** ([`INCARNATION_SPACE_BITS`]): the 63 bits above the
//! base's cleared top bit split into `63 − K` bits of incarnations and
//! `K` bits of mints per incarnation. `K = 38`: 2^38 ≈ 2.7 × 10^11 node
//! writes per incarnation (a joiner at 200 SMOs/s — the storm rows' rate
//! — for 43 years; the manager's own space has the same width) and
//! 2^25 ≈ 33.5 M incarnations per volume (15 k mounts re-joining daily
//! for six years). Exhaustion of either REFUSES loud
//! ([`NodeSeqHandle::next`], [`incarnation_base`]) — never a wrap.
//!
//! Every ORDER comparison of node seqs is confined to one space or made
//! equality: `install_recovered_root`'s "cached image older than the
//! pointer" re-read is a `!=` now; the handle's `raise_to` ignores a
//! target outside its own space (PR 10's recovery raises to a dead
//! joiner's root / residue stamps land in THAT incarnation's space, where
//! the manager never mints — the disjointness IS the guarantee the raise
//! used to buy); the frame screen, the pointer checks and the root choice
//! by generation compare equality or generations and are unaffected.

use super::KvError;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bits of node-seq space per incarnation — `2^K` mints. See the module
/// doc for the arithmetic (63 usable bits = 25 of incarnations + 38 of
/// mints).
pub const INCARNATION_SPACE_BITS: u32 = 38;
/// One incarnation's span.
pub const INCARNATION_SPACE: u64 = 1u64 << INCARNATION_SPACE_BITS;
/// The bits left for incarnations above `B` (the top bit is clear).
pub const INCARNATION_ORDINAL_BITS: u32 = 63 - INCARNATION_SPACE_BITS;
/// The highest ordinal the volume can mint (`B + (o + 1) · 2^K` must
/// fit below `2^64` for every `B < 2^63`).
pub const INCARNATION_ORDINAL_MAX: u64 = (1u64 << INCARNATION_ORDINAL_BITS) - 2;

/// The base of incarnation `ordinal`'s space over the volume base
/// `volume_base` (`node_seq_base(uuid)`), or `None` past the volume's
/// capacity — the caller refuses loud.
pub fn incarnation_base(volume_base: u64, ordinal: u64) -> Option<u64> {
    if ordinal > INCARNATION_ORDINAL_MAX {
        return None;
    }
    let start = volume_base.checked_add(ordinal.checked_mul(INCARNATION_SPACE)?)?;
    // The whole span must fit.
    start.checked_add(INCARNATION_SPACE)?;
    Some(start)
}

/// **The joiner's screen of the `Joined.node_seq_base` wire word** (PR 13
/// review round 1, Issue 6 — PR 3's bounded-execution law: every
/// wire-carried integer is validated against DURABLE state before any
/// effect; the `screen_release_words` / `screen_publish_root_words` shape).
/// The word decides the identity space of every node this daemon will
/// ever write, and the joiner holds the truth to judge it: the volume's
/// own `node_seq_base(uuid)`. A legitimate base is `B + o · 2^K` for an
/// ordinal `o ≥ 1` (incarnation 0 is the MANAGER's space — a word equal
/// to `B` would put the joiner back into it, defect 5(a)'s P0 class) at
/// or below the volume's capacity, and nothing else: a word off the
/// stride straddles two spaces. Refused = `KvError::Rejected` naming the
/// word and the base, nothing installed. Pure and total — the fuzz target
/// `manager_call_frame`'s service-edge arm and the proptest mirror drive
/// it.
pub fn screen_incarnation_base(volume_base: u64, word: u64) -> Result<u64, KvError> {
    let refuse = |why: &str| {
        KvError::Rejected(format!(
            "Joined.node_seq_base {word:#x} rejected: {why} (the volume's base is \
             {volume_base:#x}, a joiner's base is B + o · 2^{INCARNATION_SPACE_BITS} for \
             1 ≤ o ≤ {INCARNATION_ORDINAL_MAX} — a buggy or hostile manager frame; nothing \
             installed)"
        ))
    };
    let Some(offset) = word.checked_sub(volume_base) else {
        return Err(refuse("below the volume's base"));
    };
    if offset == 0 {
        return Err(refuse("incarnation 0 is the manager's own space"));
    }
    if offset % INCARNATION_SPACE != 0 {
        return Err(refuse(
            "not on the incarnation stride — it would straddle two spaces",
        ));
    }
    let ordinal = offset / INCARNATION_SPACE;
    match incarnation_base(volume_base, ordinal) {
        Some(base) if base == word => Ok(word),
        _ => Err(refuse("past the volume's incarnation capacity")),
    }
}

/// The ONE tree-0 key of the incarnation counter: how many joiner
/// incarnations this volume has minted (the manager's, volume-local —
/// every volume of a set has its own base and its own counter).
pub const NODE_SEQ_INCARNATIONS_KEY: &[u8] = b"node_seq_incarnations";
/// Record value version (byte 0).
pub const NODE_SEQ_INCARNATIONS_VERSION: u8 = 1;
/// `version ‖ minted: u64`.
pub const NODE_SEQ_INCARNATIONS_LEN: usize = 1 + 8;

/// Encode the counter record's value.
pub fn encode_incarnations(minted: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(NODE_SEQ_INCARNATIONS_LEN);
    out.push(NODE_SEQ_INCARNATIONS_VERSION);
    out.extend_from_slice(&minted.to_le_bytes());
    out
}

/// Decode + validate (total).
pub fn decode_incarnations(value: &[u8]) -> Result<u64, KvError> {
    let version = *value
        .first()
        .ok_or_else(|| KvError::Corrupt("node_seq_incarnations record is empty".to_string()))?;
    if version != NODE_SEQ_INCARNATIONS_VERSION {
        return Err(KvError::Corrupt(format!(
            "node_seq_incarnations record version {version} — this binary writes \
             {NODE_SEQ_INCARNATIONS_VERSION} and the format is forward-only (upgrade squeezefs)"
        )));
    }
    if value.len() != NODE_SEQ_INCARNATIONS_LEN {
        return Err(KvError::Corrupt(format!(
            "node_seq_incarnations record must be {NODE_SEQ_INCARNATIONS_LEN} bytes, got {}",
            value.len()
        )));
    }
    let bytes = <[u8; 8]>::try_from(&value[1..9])
        .map_err(|_| KvError::Corrupt("node_seq_incarnations record: short counter".into()))?;
    Ok(u64::from_le_bytes(bytes))
}

/// The per-volume node-seq mint handle: the counter, its space's
/// exclusive ceiling, and whether replayed pointers may RAISE it (the
/// shared incarnation-0 space's watermark law) or not (a joined
/// incarnation's fresh space).
#[derive(Debug)]
pub struct NodeSeqHandle {
    next: AtomicU64,
    ceiling: u64,
    raises: bool,
}

impl NodeSeqHandle {
    /// The legacy shared handle — a flat volume's, every unarmed mount's:
    /// seeded from the ledger, no ceiling, raised by every replayed root.
    /// Byte-identical to the pre-PR-13 `AtomicU64`.
    pub fn shared(seed: u64) -> Self {
        Self {
            next: AtomicU64::new(seed),
            ceiling: u64::MAX,
            raises: true,
        }
    }

    /// The manager's handle on a forest volume: the legacy seeding and
    /// the raise law, bounded by incarnation 0's span over `volume_base`
    /// (a raise past it — a dead joiner's root, residue stamps — is a
    /// foreign space and is ignored).
    pub fn shared_bounded(seed: u64, volume_base: u64) -> Self {
        let ceiling = incarnation_base(volume_base, 1).unwrap_or(u64::MAX);
        Self {
            next: AtomicU64::new(seed),
            ceiling,
            raises: true,
        }
    }

    /// A JOINED appender's handle: incarnation `base`'s fresh space,
    /// never raised.
    pub fn incarnation(base: u64) -> Self {
        Self {
            next: AtomicU64::new(base),
            ceiling: base.saturating_add(INCARNATION_SPACE),
            raises: false,
        }
    }

    /// Mint the next node seq — REFUSED loud at the space's ceiling,
    /// never wrapped into the next incarnation's (or, on the legacy
    /// handle, past `u64::MAX`).
    pub fn next(&self) -> Result<u64, KvError> {
        let seq = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        if seq >= self.ceiling {
            // Park the counter at the ceiling so a retry refuses too.
            self.next.fetch_min(self.ceiling, Ordering::AcqRel);
            return Err(KvError::Corrupt(format!(
                "node-seq space exhausted: this appender incarnation minted 2^{} node seqs \
                 (ceiling {:#x}) — remount to take a fresh incarnation (design-symmetric-\
                 metadata §5.3.3, PR 13)",
                INCARNATION_SPACE_BITS, self.ceiling
            )));
        }
        Ok(seq)
    }

    /// The §4.5 watermark raise: a replayed root/pointer seq floors the
    /// counter — INSIDE this handle's own space only. A joined
    /// incarnation's handle never raises (its space is fresh); the shared
    /// handle ignores a target in another incarnation's space (the
    /// disjointness makes a collision there impossible by construction).
    pub fn raise_to(&self, seq: u64) {
        if self.raises && seq < self.ceiling {
            self.next.fetch_max(seq, Ordering::AcqRel);
        }
    }

    /// The counter's current value (the ledger's `node_seq_watermark`).
    pub fn load(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// The space's exclusive ceiling (`u64::MAX` on the legacy handle).
    pub fn ceiling(&self) -> u64 {
        self.ceiling
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_arithmetic_partitions_the_63_bits_and_refuses_past_them() {
        assert_eq!(INCARNATION_SPACE_BITS + INCARNATION_ORDINAL_BITS, 63);
        let b = 0x1234_5678_9abc_def0 & (u64::MAX >> 1);
        assert_eq!(incarnation_base(b, 0), Some(b));
        assert_eq!(incarnation_base(b, 1), Some(b + INCARNATION_SPACE));
        assert_eq!(incarnation_base(b, 2), Some(b + 2 * INCARNATION_SPACE));
        // Disjoint, adjacent.
        let (s1, s2) = (
            incarnation_base(b, 1).unwrap(),
            incarnation_base(b, 2).unwrap(),
        );
        assert_eq!(s1 + INCARNATION_SPACE, s2);
        // The widest base still fits every admitted ordinal; one past refuses.
        let top = u64::MAX >> 1;
        assert!(incarnation_base(top, INCARNATION_ORDINAL_MAX).is_some());
        assert!(incarnation_base(top, INCARNATION_ORDINAL_MAX + 1).is_none());
        assert!(incarnation_base(top, u64::MAX).is_none());
    }

    #[test]
    fn the_handle_mints_inside_its_space_and_refuses_at_the_ceiling() {
        let h = NodeSeqHandle::incarnation(1000);
        assert_eq!(h.next().unwrap(), 1001);
        h.raise_to(5_000_000); // a joined handle never raises
        assert_eq!(h.next().unwrap(), 1002);
        assert_eq!(h.ceiling(), 1000 + INCARNATION_SPACE);
        let tiny = NodeSeqHandle {
            next: AtomicU64::new(10),
            ceiling: 12,
            raises: true,
        };
        assert_eq!(tiny.next().unwrap(), 11);
        assert!(tiny.next().is_err(), "the ceiling refuses");
        assert!(tiny.next().is_err(), "and stays refused");
        tiny.raise_to(100); // outside the space: ignored
        assert_eq!(tiny.load(), 12);
        let shared = NodeSeqHandle::shared(7);
        shared.raise_to(40);
        assert_eq!(shared.next().unwrap(), 41);
        assert_eq!(shared.ceiling(), u64::MAX);
    }

    #[test]
    fn the_incarnation_record_round_trips_and_refuses_other_shapes() {
        assert_eq!(decode_incarnations(&encode_incarnations(77)).unwrap(), 77);
        assert!(decode_incarnations(&[]).is_err());
        assert!(decode_incarnations(&[2, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(decode_incarnations(&[1, 0, 0]).is_err());
    }
}
