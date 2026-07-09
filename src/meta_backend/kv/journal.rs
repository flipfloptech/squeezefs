//! Journal ring I/O: page/entry framing (including multi-page
//! continuation), entry writes via `crate::uring_fs`, and the mount-time
//! replay scan with drop-and-resync tear semantics (design §4.1, §4.4
//! pts 2/3/5, §4.6 pt 3, §4.10).
//!
//! The ring is a fixed extent of 4 KiB pages. Each page starts with a
//! checksummed 24 B header (§4.1 — the header is a unit replay trusts to
//! find entry boundaries, so the every-unit-checksummed rule applies to it
//! literally); the remaining 4072 bytes are entry bytes addressed through
//! the [`super::journal_core`] logical byte space, which skips the header
//! slots by construction.
//!
//! ## Page header (24 B, little-endian)
//!
//! ```text
//! [0..4)   magic: u32           JOURNAL_PAGE_MAGIC
//! [4..8)   lap: u32             low 32 bits of the page's write lap
//! [8..10)  first_entry_off: u16 physical in-page offset (24..4096) of the
//!                               first entry that STARTS in this page;
//!                               0xFFFF = whole page is continuation bytes
//! [10..12) _pad: u16            zero
//! [12..16) _pad2: u32           zero (aligns the checksum; covered by it)
//! [16..24) xxh3_64: u64         over bytes [0..16)
//! ```
//!
//! The §4.1 field list (`{magic, lap, first_entry_off, _pad, xxh3_64}`)
//! sums to 20 bytes inside a declared-24 B header; the remaining 4 bytes
//! are explicit zero padding placed before the checksum so the u64 lands
//! aligned — covered by the checksum like every other header field.
//!
//! ## Entry framing (20 B header + payload, little-endian)
//!
//! ```text
//! [0..8)   seq: u64             == the entry's logical start position
//! [8..12)  len: u32             payload byte length
//! [12..20) xxh3_64: u64         whole-entry checksum
//! payload: [tree_id: u8 ‖ K1 record framing]  (one per staged record)
//! ```
//!
//! §4.1 writes the checksum as `xxh3_64(whole payload)` but also calls it
//! the "whole-entry checksum"; K3 resolves to the strictly stronger
//! reading: the digest covers `seq ‖ len ‖ payload` (checksum field
//! excluded), so a corrupted `seq` or `len` can never masquerade as a
//! valid entry. Entries pack back-to-back in logical space — headers may
//! straddle page-data boundaries; an entry larger than the page remainder
//! continues across consecutive pages with one header and one checksum, so
//! a transaction of any size up to [`MAX_ENTRY_LEN`] stays a single
//! all-or-nothing unit. Records carry a leading `tree_id` byte because
//! §4.2 puts `tree_id` "in node headers and journal records" — the entry
//! payload is exactly the §4.4 staging shape `Vec<(tree_id, Record)>`.
//!
//! ## Replay semantics (§4.1)
//!
//! Nothing inside the replay window ever fails a mount loud: `[tail,
//! tail + logical_len)` is by definition the maybe-torn region. Replay
//! walks pages in ring order; a page whose header fails its checksum (or
//! carries the wrong lap) is unusable for entry *discovery* — entries
//! starting there are lost — but an entry *continuing* through it at
//! chain-known offsets is still read and its whole-entry checksum decides.
//! Any invalid structure on the chain (bad entry checksum, `len` over the
//! cap, `len` overrunning the window — the length is bounds-checked before
//! any byte it governs is read) drops the entry and resynchronizes at the
//! next page whose header verifies and whose `first_entry_off ≠ 0xFFFF`.
//! Drops are counted only when a later entry is successfully recovered
//! (`[JournalRecovery::dropped_torn]`): trailing parse failures are the
//! ordinary end-of-log (stale bytes past the head), so a clean-unmount
//! replay reports zero — which is what makes "nonzero after a clean
//! unmount" the corruption alert (§10).

use super::journal_core::{JournalCore, Reservation};
use super::record::Record;
use super::KvError;
use std::path::{Path, PathBuf};

/// Physical page length (§4.1: 4 KiB pages).
pub const JOURNAL_PAGE_LEN: u64 = 4096;
/// Page header length (§4.1: checksummed 24 B header).
pub const JOURNAL_PAGE_HDR_LEN: u64 = 24;
/// Entry-byte capacity of one page.
pub const JOURNAL_PAGE_DATA_LEN: u64 = JOURNAL_PAGE_LEN - JOURNAL_PAGE_HDR_LEN;
/// Page header magic (`"KVJP"`).
pub const JOURNAL_PAGE_MAGIC: u32 = 0x4B56_4A50;
/// `first_entry_off` sentinel: the whole page is continuation bytes of an
/// entry begun in an earlier page (§4.1).
pub const FIRST_ENTRY_NONE: u16 = 0xFFFF;
/// Entry header length (`seq: u64 | len: u32 | xxh3_64: u64`).
pub const ENTRY_HDR_LEN: u64 = 20;
/// Max whole-entry size (header + payload): 128 KiB (§4.1 — the writer-side
/// guard; replay drops-and-resyncs any `len` implying more without ever
/// dereferencing it).
pub const MAX_ENTRY_LEN: u64 = 128 * 1024;

/// The §4.4 pt 5 checkpoint-task ring reserve for a ring of `ring_len`
/// physical bytes: `max(256 KiB, ring/64)`.
pub fn checkpoint_reserve_bytes(_ring_len: u64) -> u64 {
    todo!()
}

/// Exact whole-entry size (header + payload) for staged records — the
/// admission size of §4.4 pt 5 ("a committer's staged records fix its exact
/// entry size before any lock is taken"). Errors when the entry would
/// exceed [`MAX_ENTRY_LEN`].
pub fn entry_len_for(_records: &[(u8, Record)]) -> Result<u64, KvError> {
    todo!()
}

/// Encode the entry payload: each staged record as `tree_id: u8` followed
/// by the K1 record framing.
pub fn encode_entry_payload(_records: &[(u8, Record)]) -> Vec<u8> {
    todo!()
}

/// Decode an entry payload back into `(tree_id, Record)` pairs. Every
/// length is bounds-checked against the container (§9); unknown tree ids
/// are structural corruption (the checksum already verified, so this is
/// defense in depth against writer bugs).
pub fn decode_entry_payload(_buf: &[u8]) -> Result<Vec<(u8, Record)>, KvError> {
    todo!()
}

/// One journal ring over a file extent: `pages` × 4 KiB starting at byte
/// `base` of `path`. Owns the lock-free [`JournalCore`]; all device I/O
/// goes through `crate::uring_fs` (io_uring — non-negotiable).
pub struct JournalRing {
    _path: PathBuf,
    _base: u64,
    _core: JournalCore,
}

/// One replayed transaction: the entry seq and its staged records in
/// entry order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedEntry {
    /// Entry seq (== its logical start position).
    pub seq: u64,
    /// The entry's records, each tagged with its tree id (§4.2).
    pub records: Vec<(u8, Record)>,
}

/// Outcome of a replay scan (§4.1).
#[derive(Debug)]
pub struct JournalRecovery {
    /// Recovered entries with `seq ≥ tail_seq`, seq-sorted.
    pub entries: Vec<ReplayedEntry>,
    /// The recovered head position: one past the last valid entry (the
    /// resume point for [`JournalRing::recover`]'s core), at least the
    /// tail.
    pub head_pos: u64,
    /// Drop-and-resync events confirmed by a later recovered entry —
    /// mount-scoped `meta_kv_replay_dropped_torn` (§10): nonzero after a
    /// crash is working-as-designed; nonzero after a clean unmount is the
    /// corruption alert.
    pub dropped_torn: u64,
}

impl JournalRing {
    /// A fresh ring (head = 0, watermark = 0) over `pages` × 4 KiB at
    /// `base` in `path`, with the §4.4 pt 5 checkpoint carve-out
    /// `reserve_bytes` (production callers pass
    /// [`checkpoint_reserve_bytes`]; tests pass explicit values sized to
    /// their rings). Ring sizing itself is the caller's (format-time)
    /// decision — K6a owns the `clamp(volume/64, 8-32 MiB)` policy.
    pub fn new(_path: &Path, _base: u64, _pages: u64, _reserve_bytes: u64) -> Self {
        todo!()
    }

    /// Remount: replay-scan the ring (§4.1 semantics, never loud for ring
    /// contents — `Err` is real device I/O failure only, [`KvError::Io`])
    /// from the mounted ledger record's `journal_tail_seq`, and resume the
    /// core at the recovered head with `reusable_upto = tail_seq` (§4.6
    /// pt 3: the chosen ledger record is durable by virtue of having been
    /// read).
    pub async fn recover(
        _path: &Path,
        _base: u64,
        _pages: u64,
        _reserve_bytes: u64,
        _tail_seq: u64,
    ) -> Result<(Self, JournalRecovery), KvError> {
        todo!()
    }

    /// The lock-free admission/reservation core (§4.4 pts 2/5): K6b's
    /// commit pipeline admits before node locks, reserves inside them, and
    /// calls [`Self::write_entry`] after unlock; tests compose the same
    /// three phases.
    pub fn core(&self) -> &JournalCore {
        todo!()
    }

    /// Number of ring pages.
    pub fn pages(&self) -> u64 {
        todo!()
    }

    /// Write one entry's bytes into its reserved range: the committer's own
    /// bytes, one `uring_fs` submission (`write_at` for a page-local entry,
    /// `write_at_batch` for multi-page — §4.4 pt 3), including the 24 B
    /// header of every page whose first logical byte the reservation owns
    /// (§4.4 pt 2). `res.len` must equal [`entry_len_for`] of `records`.
    pub async fn write_entry(
        &self,
        _res: &Reservation,
        _records: &[(u8, Record)],
    ) -> Result<(), KvError> {
        todo!()
    }
}
