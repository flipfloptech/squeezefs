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
//! tail + logical_len)` is by definition the maybe-torn region. Replay is
//! **chain-primary**: the tail is always an entry boundary (§4.6 pt 2's
//! tail rule — an entry seq or the head), so the walk starts at `tail`
//! itself and parses entries back-to-back, validating `seq == position`
//! (the [`super::journal_core`] seq identity), the len bounds (checked
//! before any byte the length governs is read), and the whole-entry
//! checksum. Page headers are consulted only for **resync**: after any
//! chain failure, the scanner advances to the next window page whose
//! header verifies — magic, checksum, and the lap expected for that page's
//! position in the window — and whose `first_entry_off ≠ 0xFFFF`, and
//! resumes there. An entry *continuing* through a dead-header page is
//! therefore still read at chain-known offsets, and its whole-entry
//! checksum decides (§4.1). Chain-primacy also covers the one window
//! shape discovery cannot: with a mid-page tail and the head a full lap
//! ahead, the tail page's *header* is legally re-owned by lap + 1 while
//! the byte range `[tail, page end)` is still protected — headerless but
//! chain-reachable.
//!
//! **Drop accounting** (`meta_kv_replay_dropped_torn`, §10): every chain
//! parse failure and every unverifiable discovery page increments a
//! *pending* count; pending drops are **confirmed** (added to
//! [`JournalRecovery::dropped_torn`]) only when a later entry parses
//! successfully. Trailing failures — the ordinary end-of-log garbage past
//! the head — are never confirmed, so a clean-unmount replay reports zero,
//! which is exactly what makes "nonzero after a clean unmount" the
//! corruption alert (§10) instead of a tear census.

use super::journal_core::{AdmissionClass, CoreGeometry, JournalCore, Reservation};
use super::record::{Record, RecordRef, TREE_BACKPTR_RESERVED, TREE_INODES};
use super::KvError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
pub fn checkpoint_reserve_bytes(ring_len: u64) -> u64 {
    (256 * 1024).max(ring_len / 64)
}

/// Exact whole-entry size (header + payload) for staged records — the
/// admission size of §4.4 pt 5 ("a committer's staged records fix its exact
/// entry size before any lock is taken"). Errors when the entry would
/// exceed [`MAX_ENTRY_LEN`].
pub fn entry_len_for(records: &[(u8, Record)]) -> Result<u64, KvError> {
    let payload: u64 = records
        .iter()
        .map(|(_, r)| 1 + r.record_ref().encoded_len() as u64)
        .sum();
    let len = ENTRY_HDR_LEN + payload;
    if len > MAX_ENTRY_LEN {
        return Err(KvError::EntryTooLarge {
            len,
            cap: MAX_ENTRY_LEN,
        });
    }
    Ok(len)
}

/// Encode the entry payload: each staged record as `tree_id: u8` followed
/// by the K1 record framing.
pub fn encode_entry_payload(records: &[(u8, Record)]) -> Vec<u8> {
    let cap: usize = records
        .iter()
        .map(|(_, r)| 1 + r.record_ref().encoded_len())
        .sum();
    let mut out = Vec::with_capacity(cap);
    for (tree_id, r) in records {
        out.push(*tree_id);
        r.record_ref().encode_into(&mut out);
    }
    out
}

/// Encode a record tag byte: the low nibble is the §4.2 `tree_id`
/// (1..=5), the high nibble the node **level** the record targets — 0 for
/// ordinary leaf records (byte == tree_id, the K3 wire encoding
/// unchanged), > 0 for the SMO task's journaled interior-pointer records
/// (§4.6: "interior mutations are journaled records"). Replay routes a
/// level-`L` record to the level-`L` node covering its key.
pub fn tag_for(tree_id: u8, level: u8) -> u8 {
    debug_assert!((TREE_INODES..=TREE_BACKPTR_RESERVED).contains(&tree_id));
    debug_assert!(
        level <= 0x0F,
        "interior level {level} exceeds the tag nibble"
    );
    tree_id | (level << 4)
}

/// Split a record tag byte into `(tree_id, level)` (see [`tag_for`]).
pub fn untag(tag: u8) -> (u8, u8) {
    (tag & 0x0F, tag >> 4)
}

/// Decode an entry payload back into `(tag, Record)` pairs (`tag` =
/// [`tag_for`]'s tree/level byte). Every length is bounds-checked against
/// the container (§9); unknown tree ids are structural corruption (the
/// checksum already verified, so this is defense in depth against writer
/// bugs).
pub fn decode_entry_payload(buf: &[u8]) -> Result<Vec<(u8, Record)>, KvError> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = buf[pos];
        pos += 1;
        let (tree_id, _level) = untag(tag);
        if !(TREE_INODES..=TREE_BACKPTR_RESERVED).contains(&tree_id) {
            return Err(KvError::Corrupt(format!(
                "journal record carries tree id {tree_id} outside the §4.2 table"
            )));
        }
        let (r, used) = RecordRef::decode(&buf[pos..])?;
        out.push((tag, r.to_record()));
        pos += used;
    }
    Ok(out)
}

/// xxh3 whole-entry digest: `seq ‖ len ‖ payload` (module docs), with the
/// payload supplied as segment slices so hashing never copies.
fn entry_checksum<'a>(seq: u64, len: u32, payload_parts: impl Iterator<Item = &'a [u8]>) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&seq.to_le_bytes());
    h.update(&len.to_le_bytes());
    for part in payload_parts {
        h.update(part);
    }
    h.digest()
}

/// Build one 24 B page-header image.
fn page_header_bytes(lap: u32, first_entry_off: u16) -> [u8; JOURNAL_PAGE_HDR_LEN as usize] {
    let mut hdr = [0u8; JOURNAL_PAGE_HDR_LEN as usize];
    hdr[0..4].copy_from_slice(&JOURNAL_PAGE_MAGIC.to_le_bytes());
    hdr[4..8].copy_from_slice(&lap.to_le_bytes());
    hdr[8..10].copy_from_slice(&first_entry_off.to_le_bytes());
    // [10..16) explicit zero padding, covered by the checksum.
    let sum = xxhash_rust::xxh3::xxh3_64(&hdr[0..16]);
    hdr[16..24].copy_from_slice(&sum.to_le_bytes());
    hdr
}

/// Parse + verify one page header from the ring image: `(lap, feo)` when
/// the magic and checksum hold, `None` otherwise (an unverifiable page —
/// unusable for entry discovery, §4.1).
fn parse_page_header(page: &[u8]) -> Option<(u32, u16)> {
    let magic = u32::from_le_bytes(page[0..4].try_into().unwrap());
    if magic != JOURNAL_PAGE_MAGIC {
        return None;
    }
    let stored = u64::from_le_bytes(page[16..24].try_into().unwrap());
    if stored != xxhash_rust::xxh3::xxh3_64(&page[0..16]) {
        return None;
    }
    Some((
        u32::from_le_bytes(page[4..8].try_into().unwrap()),
        u16::from_le_bytes(page[8..10].try_into().unwrap()),
    ))
}

/// One journal ring over a file extent: `pages` × 4 KiB starting at byte
/// `base` of `path`. Owns the lock-free [`JournalCore`]; all device I/O
/// goes through `crate::uring_fs` (io_uring — non-negotiable).
///
/// PR K6b adds the **in-flight reservation registry** around the core:
///
/// - `min_inflight_start()` feeds the §4.6 pt 2 tail rule (a tx between
///   its in-lock reservation and its RAM apply/entry write must hold the
///   tail back — the registry is the only witness of that window);
/// - `completed_upto()` is the **contiguous completed-prefix watermark**
///   behind the K3 fsync-barrier observation: an acked entry starting
///   mid-page is chain-reachable only through its predecessors, so a tx
///   returns (and a barrier acks) only once every reservation below it
///   has completed its write — or explicitly abandoned it (a rollback's
///   reserved-but-unwritten hole still *completes* for watermark
///   purposes, it just never writes bytes).
///
/// The registry is a `std::sync::Mutex` over a `BTreeMap` — held for O(1)
/// map ops, never across `.await`, entered at most twice per transaction
/// (reserve + complete). It is *around* the loom-modeled lock-free core,
/// not inside it; the commit hot path's reads stay latch-free.
pub struct JournalRing {
    path: PathBuf,
    base: u64,
    core: JournalCore,
    inflight: Mutex<Inflight>,
    /// Wakes admission parkers (§4.4 pt 5) when `reusable_upto` advances
    /// — and on shutdown, so parked committers can observe the flag.
    space_notify: tokio::sync::Notify,
    /// Wakes `wait_completed_upto` waiters when the watermark advances.
    completion_notify: tokio::sync::Notify,
    /// **Per-volume** mirror of the global `META_KV_JOURNAL_ENTRIES`
    /// accounting (perf/meta-plane-writes, 2026-07-30): entries written
    /// into THIS ring. The process-global counter cannot attribute
    /// journal traffic to a volume — the exact blindness that let the
    /// field's one-volume meta-plane ceiling (its sibling at 0.00
    /// device-writes/s) hide from the stats inode. Surfaced as
    /// `meta_kv_journal_entries_per_volume`.
    written_entries: std::sync::atomic::AtomicU64,
    /// Per-volume mirror of `META_KV_JOURNAL_BYTES` (`res.len` per
    /// committed entry, headers included). Surfaced as
    /// `meta_kv_journal_bytes_per_volume`.
    written_bytes: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Inflight {
    /// Reservations issued but not yet completed: start → end.
    open: BTreeMap<u64, u64>,
    /// Every position below this has a completed (or abandoned)
    /// reservation covering it. Reservations partition `[0, head)`
    /// contiguously, so this is `min(open) `or the head when none open.
    completed_upto: u64,
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
    pub fn new(path: &Path, base: u64, pages: u64, reserve_bytes: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            base,
            core: JournalCore::new(
                CoreGeometry {
                    page_data_len: JOURNAL_PAGE_DATA_LEN,
                    pages,
                    reserve_bytes,
                },
                0,
                0,
            ),
            inflight: Mutex::new(Inflight::default()),
            space_notify: tokio::sync::Notify::new(),
            completion_notify: tokio::sync::Notify::new(),
            written_entries: std::sync::atomic::AtomicU64::new(0),
            written_bytes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Remount: replay-scan the ring (§4.1 semantics, never loud for ring
    /// contents — `Err` is real device I/O failure only, [`KvError::Io`])
    /// from the mounted ledger record's `journal_tail_seq`, and resume the
    /// core at the recovered head with `reusable_upto = tail_seq` (§4.6
    /// pt 3: the chosen ledger record is durable by virtue of having been
    /// read).
    pub async fn recover(
        path: &Path,
        base: u64,
        pages: u64,
        reserve_bytes: u64,
        tail_seq: u64,
    ) -> Result<(Self, JournalRecovery), KvError> {
        let geo = CoreGeometry {
            page_data_len: JOURNAL_PAGE_DATA_LEN,
            pages,
            reserve_bytes,
        };
        // One sequential ring read (§3's mount budget). A short read (file
        // smaller than the extent) zero-extends: zeros verify nothing and
        // replay to nothing.
        let ring_len = (pages * JOURNAL_PAGE_LEN) as usize;
        let got = crate::uring_fs::read_at(path, base, ring_len).await?;
        let image: std::borrow::Cow<'_, [u8]> = if got.len() == ring_len {
            std::borrow::Cow::Borrowed(&got)
        } else {
            let mut full = vec![0u8; ring_len];
            full[..got.len()].copy_from_slice(&got);
            std::borrow::Cow::Owned(full)
        };
        let recovery = replay_scan_image(&image, &geo, tail_seq);
        let ring = Self {
            path: path.to_path_buf(),
            base,
            core: JournalCore::new(geo, recovery.head_pos, tail_seq),
            inflight: Mutex::new(Inflight {
                open: BTreeMap::new(),
                completed_upto: recovery.head_pos,
            }),
            space_notify: tokio::sync::Notify::new(),
            completion_notify: tokio::sync::Notify::new(),
            written_entries: std::sync::atomic::AtomicU64::new(0),
            written_bytes: std::sync::atomic::AtomicU64::new(0),
        };
        Ok((ring, recovery))
    }

    /// The lock-free admission/reservation core (§4.4 pts 2/5): K6b's
    /// commit pipeline admits before node locks, reserves inside them, and
    /// calls [`Self::write_entry`] after unlock; tests compose the same
    /// three phases.
    pub fn core(&self) -> &JournalCore {
        &self.core
    }

    /// Journal entries written into THIS ring since open — the per-volume
    /// attribution instrument (`meta_kv_journal_entries_per_volume`; see
    /// the field-conviction note on the struct field).
    pub fn written_entries(&self) -> u64 {
        self.written_entries
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Entry bytes written into THIS ring since open (headers included) —
    /// `meta_kv_journal_bytes_per_volume`.
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Non-parking admission (the checkpoint task's own records, §4.4
    /// pt 5: the task must never wait on ring space it is itself
    /// responsible for freeing — on `None` it runs a minimal drain and
    /// retries).
    pub fn try_admit(
        &self,
        len: u64,
        class: AdmissionClass,
    ) -> Option<super::journal_core::Admission> {
        self.core.try_admit(len, class)
    }

    /// Transfer admitted budget to the head **and register the
    /// reservation in-flight** — one mutex section so the completed-prefix
    /// watermark can never observe a head past an unregistered
    /// reservation. Called inside the node-lock window (§4.4 pt 2); the
    /// mutex is held for two map operations, never across `.await`.
    pub fn reserve_registered(&self, adm: super::journal_core::Admission) -> Reservation {
        let mut g = self.inflight.lock().unwrap();
        let res = self.core.reserve(adm);
        g.open.insert(res.start, res.end());
        res
    }

    /// Mark a reservation's write complete — or abandoned (a failed
    /// writer's reserved range stays unwritten but still completes for
    /// watermark purposes, §4.4 pt 4: "the reserved ring range stays
    /// unwritten — replay's checksum walk drops it").
    pub fn complete(&self, res: &Reservation) {
        let mut g = self.inflight.lock().unwrap();
        let removed = g.open.remove(&res.start);
        debug_assert!(removed.is_some(), "double-complete of a reservation");
        g.completed_upto = match g.open.first_key_value() {
            Some((start, _)) => *start,
            None => self.core.head(),
        };
        drop(g);
        self.completion_notify.notify_waiters();
    }

    /// The contiguous completed-prefix watermark (module docs).
    pub fn completed_upto(&self) -> u64 {
        self.inflight.lock().unwrap().completed_upto
    }

    /// The oldest in-flight reservation's start, or `u64::MAX` when none
    /// — the §4.6 pt 2 tail rule's third input (a tx between reservation
    /// and RAM apply must hold the tail back).
    pub fn min_inflight_start(&self) -> u64 {
        self.inflight
            .lock()
            .unwrap()
            .open
            .first_key_value()
            .map(|(s, _)| *s)
            .unwrap_or(u64::MAX)
    }

    /// Wait until every reservation below `pos` has completed — the D0
    /// chain-reachability rule (module docs): a tx returns only once its
    /// predecessors' bytes are in page cache; a barrier acks only writes
    /// that completed before it started.
    pub async fn wait_completed_upto(&self, pos: u64) {
        loop {
            if self.completed_upto() >= pos {
                return;
            }
            let notified = self.completion_notify.notified();
            if self.completed_upto() >= pos {
                return;
            }
            notified.await;
        }
    }

    /// Advance the §4.6 pt 3 reclamation watermark **and wake admission
    /// parkers** — the checkpoint task calls this only after the ledger
    /// record that retired the range is known durable.
    pub fn advance_reusable_upto(&self, pos: u64) {
        self.core.advance_reusable_upto(pos);
        self.space_notify.notify_waiters();
    }

    /// Wake every admission parker without advancing anything (shutdown:
    /// parked committers re-check the volume state and bail out).
    pub fn wake_parked(&self) {
        self.space_notify.notify_waiters();
    }

    /// A registered waiter on the space notify (the caller's
    /// register-recheck-await admission loop; §4.4 pt 5).
    pub fn space_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.space_notify.notified()
    }

    /// Physical file offset of logical ring position `pos` (the crash
    /// harness arms write faults at exact entry offsets).
    pub fn physical_offset_of(&self, pos: u64) -> u64 {
        let geo = self.core.geometry();
        self.base
            + geo.page_index(pos) * JOURNAL_PAGE_LEN
            + JOURNAL_PAGE_HDR_LEN
            + geo.in_page_off(pos)
    }

    /// Build one entry's write ops into `ops`: payload segments + the 24 B
    /// header of every page whose first logical byte the reservation owns
    /// (§4.4 pt 2). Shared by [`Self::write_entry`] and
    /// [`Self::write_entries_batch`], so a conveyor batch member's bytes
    /// are **by construction** identical to a solo commit's (the §5.5
    /// batch-of-1 equivalence). `res.len` must equal [`entry_len_for`] of
    /// `records`.
    fn entry_ops(
        &self,
        res: &Reservation,
        records: &[(u8, Record)],
        ops: &mut Vec<(u64, bytes::Bytes)>,
    ) -> Result<(), KvError> {
        let geo = self.core.geometry();
        let payload = encode_entry_payload(records);
        let need = entry_len_for(records)?;
        if need != res.len {
            return Err(KvError::Corrupt(format!(
                "entry write does not match its reservation: staged {need} bytes, reserved {}",
                res.len
            )));
        }

        // Whole-entry bytes: 20 B header + payload, one buffer; the batch
        // ops below are refcounted slices of it (no copies).
        let mut entry = Vec::with_capacity(need as usize);
        let len = payload.len() as u32;
        let sum = entry_checksum(res.seq(), len, std::iter::once(payload.as_slice()));
        entry.extend_from_slice(&res.seq().to_le_bytes());
        entry.extend_from_slice(&len.to_le_bytes());
        entry.extend_from_slice(&sum.to_le_bytes());
        entry.extend_from_slice(&payload);
        let entry = bytes::Bytes::from(entry);

        // Payload segments first, then the owned page headers. The order
        // within one batch carries no durability meaning (unordered
        // writeback is the crash model either way); this order lets the
        // torn-batch shim exercise payload-landed/header-lost shapes.
        let mut consumed = 0usize;
        for seg in geo.segments(res.start, res.len) {
            let file_off =
                self.base + seg.page * JOURNAL_PAGE_LEN + JOURNAL_PAGE_HDR_LEN + seg.data_off;
            ops.push((file_off, entry.slice(consumed..consumed + seg.len as usize)));
            consumed += seg.len as usize;
        }
        debug_assert_eq!(consumed, entry.len(), "segments must cover the entry");

        for page_start in geo.owned_page_starts(res.start, res.len) {
            // §4.4 pt 2: the owner always has the local information
            // first_entry_off needs — its entry starts here, ends inside,
            // or spans the page entirely.
            let feo = if res.start == page_start {
                JOURNAL_PAGE_HDR_LEN as u16
            } else if res.end() < page_start + geo.page_data_len {
                (JOURNAL_PAGE_HDR_LEN + (res.end() - page_start)) as u16
            } else {
                FIRST_ENTRY_NONE
            };
            let lap = geo.lap(page_start) as u32;
            let hdr = page_header_bytes(lap, feo);
            ops.push((
                self.base + geo.page_index(page_start) * JOURNAL_PAGE_LEN,
                bytes::Bytes::copy_from_slice(&hdr),
            ));
        }
        Ok(())
    }

    /// Write one entry's bytes into its reserved range: the committer's own
    /// bytes, one `uring_fs` submission (`write_at` for a page-local entry,
    /// `write_at_batch` for multi-page — §4.4 pt 3).
    pub async fn write_entry(
        &self,
        res: &Reservation,
        records: &[(u8, Record)],
    ) -> Result<(), KvError> {
        let mut ops: Vec<(u64, bytes::Bytes)> = Vec::new();
        self.entry_ops(res, records, &mut ops)?;
        if ops.len() == 1 {
            let (off, data) = ops.pop().expect("one op");
            crate::uring_fs::write_at(&self.path, off, data).await?;
        } else {
            crate::uring_fs::write_at_batch(&self.path, ops).await?;
        }
        // §10 / §8 row 7 accounting: entry bytes actually written to ring
        // pages (headers included) — one half of the v3 device-byte story.
        super::META_KV_JOURNAL_BYTES.fetch_add(res.len, std::sync::atomic::Ordering::Relaxed);
        super::META_KV_JOURNAL_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.written_bytes
            .fetch_add(res.len, std::sync::atomic::Ordering::Relaxed);
        self.written_entries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// PR M7 (design-metadata-throughput §5.5 D5): write a conveyor
    /// batch's entries — **N ordinary checksummed entries** at
    /// back-to-back reservations inside one registered range — as ONE
    /// `uring_fs` submission (zero on-disk format change: each part is
    /// byte-identical to a [`Self::write_entry`] of the same
    /// reservation, and replay is byte-for-byte today's walk; a torn
    /// member drops that tx only).
    ///
    /// Parts must be non-overlapping and each sized exactly
    /// ([`entry_len_for`] == `res.len`), but need not be contiguous —
    /// a rolled-back member's sub-range is simply absent (the §4.4 pt 4
    /// unwritten hole; its page headers are written iff some surviving
    /// part owns the page's first byte).
    ///
    /// The caller owns registration/completion of the covering
    /// reservation (the pass completes it on BOTH outcomes — the
    /// commit_entry discipline, batch edition).
    pub async fn write_entries_batch(
        &self,
        parts: &[(Reservation, &[(u8, Record)])],
    ) -> Result<(), KvError> {
        let mut ops: Vec<(u64, bytes::Bytes)> = Vec::new();
        for (res, records) in parts {
            self.entry_ops(res, records, &mut ops)?;
        }
        if ops.len() == 1 {
            let (off, data) = ops.pop().expect("one op");
            crate::uring_fs::write_at(&self.path, off, data).await?;
        } else {
            crate::uring_fs::write_at_batch(&self.path, ops).await?;
        }
        let bytes: u64 = parts.iter().map(|(r, _)| r.len).sum();
        super::META_KV_JOURNAL_BYTES.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        super::META_KV_JOURNAL_ENTRIES
            .fetch_add(parts.len() as u64, std::sync::atomic::Ordering::Relaxed);
        self.written_bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        self.written_entries
            .fetch_add(parts.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// [`Self::write_entry`] + guaranteed [`Self::complete`] on **both**
    /// outcomes — the commit pipeline's write step. A reservation that
    /// never completes would wedge the completed-prefix watermark (and
    /// with it every later committer), so completion is not optional.
    pub async fn commit_entry(
        &self,
        res: &Reservation,
        records: &[(u8, Record)],
    ) -> Result<(), KvError> {
        let out = self.write_entry(res, records).await;
        self.complete(res);
        out
    }
}

/// Read the logical byte range `[pos, pos + len)` out of the ring image
/// into `out` (cleared first). Bounded by the caller (positions map into
/// the image by construction).
fn read_logical(image: &[u8], geo: &CoreGeometry, pos: u64, len: u64, out: &mut Vec<u8>) {
    out.clear();
    for seg in geo.segments(pos, len) {
        let start = (seg.page * JOURNAL_PAGE_LEN + JOURNAL_PAGE_HDR_LEN + seg.data_off) as usize;
        out.extend_from_slice(&image[start..start + seg.len as usize]);
    }
}

/// One chain-parse outcome.
enum Parsed {
    /// A verified entry: `(records, end_pos)`.
    Entry(Vec<(u8, Record)>, u64),
    /// No valid entry at this position (torn/garbage/stale/end-of-log).
    Fail,
}

/// Parse + verify the entry at logical `pos`. Order of checks (§4.1/§9):
/// header fits the window; `seq == pos` (the stale-bytes/end-of-log
/// rejector); `len` bounded by the cap and the window **before any byte it
/// governs is read**; whole-entry checksum; payload decode.
fn parse_entry_at(image: &[u8], geo: &CoreGeometry, pos: u64, chain_end: u64) -> Parsed {
    if pos + ENTRY_HDR_LEN > chain_end {
        return Parsed::Fail;
    }
    let mut hdr = Vec::with_capacity(ENTRY_HDR_LEN as usize);
    read_logical(image, geo, pos, ENTRY_HDR_LEN, &mut hdr);
    let seq = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
    let len = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
    let stored = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
    if seq != pos {
        return Parsed::Fail;
    }
    let whole = ENTRY_HDR_LEN + u64::from(len);
    if whole > MAX_ENTRY_LEN || pos + whole > chain_end {
        // The garbage-len guard: the length is rejected on bounds alone —
        // no byte it governs is ever read (§4.1, §9).
        return Parsed::Fail;
    }
    // Hash the payload straight out of the image segments (no copy), then
    // materialize it only if the entry verifies.
    let payload_pos = pos + ENTRY_HDR_LEN;
    let computed = entry_checksum(
        seq,
        len,
        geo.segments(payload_pos, u64::from(len))
            .into_iter()
            .map(|seg| {
                let start =
                    (seg.page * JOURNAL_PAGE_LEN + JOURNAL_PAGE_HDR_LEN + seg.data_off) as usize;
                &image[start..start + seg.len as usize]
            }),
    );
    if computed != stored {
        return Parsed::Fail;
    }
    let mut payload = Vec::with_capacity(len as usize);
    read_logical(image, geo, payload_pos, u64::from(len), &mut payload);
    match decode_entry_payload(&payload) {
        Ok(records) => Parsed::Entry(records, pos + whole),
        // The checksum verified but the records do not decode: a writer
        // bug class — dropped like any other invalid chain structure
        // rather than loud (§4.1's replay-window rule).
        Err(_) => Parsed::Fail,
    }
}

/// The §4.1 replay scan over a ring image. Pure over bytes: `recover` owns
/// the I/O. Never fails — any ring contents produce a recovery.
fn replay_scan_image(image: &[u8], geo: &CoreGeometry, tail: u64) -> JournalRecovery {
    let l = geo.logical_len();
    let window_base = geo.page_start_pos(tail);
    // Entries live in [tail, tail + L) (head ≤ reusable_upto + L ≤ tail
    // + L). Discovery slots cover every physical page once from the tail
    // page onward, plus — for a mid-page tail — the tail page's second
    // occurrence, whose sliver [window_base + L, tail + L) is legally
    // writable (module docs).
    let chain_end = tail + l;
    let slots = if window_base < tail {
        geo.pages + 1
    } else {
        geo.pages
    };

    // Verify every physical page header once.
    let headers: Vec<Option<(u32, u16)>> = (0..geo.pages as usize)
        .map(|k| {
            parse_page_header(
                &image[k * JOURNAL_PAGE_LEN as usize..][..JOURNAL_PAGE_HDR_LEN as usize],
            )
        })
        .collect();

    let mut entries: Vec<ReplayedEntry> = Vec::new();
    let mut dropped = 0u64;
    let mut pending = 0u64;
    let mut head_pos = tail;

    // The slot to resume discovery from after a failure at position `p`:
    // the window page after `p`'s.
    let slot_after = |p: u64| (geo.page_start_pos(p) - window_base) / geo.page_data_len + 1;

    // Chain-primary walk: the tail is an entry boundary by the §4.6 pt 2
    // tail rule, so the chain starts there; discovery is resync-only.
    let mut cursor: Option<u64> = Some(tail);
    let mut next_slot = 0u64;
    loop {
        match cursor {
            Some(pos) if pos < chain_end => match parse_entry_at(image, geo, pos, chain_end) {
                Parsed::Entry(records, end) => {
                    // A recovered entry confirms every pending drop before
                    // it (module docs: trailing failures stay unconfirmed).
                    dropped += pending;
                    pending = 0;
                    head_pos = end;
                    if pos >= tail {
                        entries.push(ReplayedEntry { seq: pos, records });
                    }
                    cursor = Some(end);
                }
                Parsed::Fail => {
                    pending += 1;
                    next_slot = slot_after(pos);
                    cursor = None;
                }
            },
            Some(_) => {
                // The chain ran exactly to the window end: done.
                break;
            }
            None => {
                // Resync: the next window page whose header verifies for
                // its expected lap and names an entry start (§4.1).
                let mut found = None;
                while next_slot < slots {
                    let pos_i = window_base + next_slot * geo.page_data_len;
                    next_slot += 1;
                    let page = geo.page_index(pos_i) as usize;
                    match headers[page] {
                        Some((lap, feo)) if u64::from(lap) == geo.lap(pos_i) => {
                            if feo == FIRST_ENTRY_NONE {
                                // Attested continuation-only page: nothing
                                // starts here, nothing to lose.
                                continue;
                            }
                            if u64::from(feo) < JOURNAL_PAGE_HDR_LEN
                                || u64::from(feo) >= JOURNAL_PAGE_LEN
                            {
                                // Checksummed-but-impossible offset:
                                // defensive — treat as a discovery hole.
                                pending += 1;
                                continue;
                            }
                            found = Some(pos_i + (u64::from(feo) - JOURNAL_PAGE_HDR_LEN));
                            break;
                        }
                        _ => {
                            // Unverifiable or wrong-lap header: entries
                            // starting here are lost (a discovery hole,
                            // §4.1) — counted iff a later entry confirms.
                            pending += 1;
                        }
                    }
                }
                match found {
                    Some(p) => cursor = Some(p),
                    None => break,
                }
            }
        }
    }

    // Positions are strictly increasing along the walk, so the output is
    // seq-sorted by construction; keep the defensive sort the contract
    // promises ("seq-sorted") — it is O(n) on the already-sorted vec.
    entries.sort_by_key(|e| e.seq);
    JournalRecovery {
        entries,
        head_pos,
        dropped_torn: dropped,
    }
}
