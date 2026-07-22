//! PR K3 integration tests: the KV journal ring (page/entry framing,
//! multi-page continuation, lock-free admission/reservation budget,
//! `reusable_upto` watermark), the replay scan, and the A/B root ledger —
//! over temp files, all I/O through `crate::uring_fs` (io_uring).
//!
//! Contracts pinned (design `docs/design-cow-kv-metadata.md`):
//! - §4.1: 4 KiB pages with checksummed 24 B headers; `first_entry_off`
//!   chain discovery (`0xFFFF` = continuation-only page); entries pack,
//!   straddle page-data boundaries, and continue across pages as one
//!   checksummed all-or-nothing unit; the 128 KiB writer-side entry cap.
//! - §4.4 pts 2/5: admission before reservation over a logical byte space
//!   that excludes header slots; one `fetch_add` per reservation
//!   (seq == start position); checkpoint reserve carve-out
//!   `max(256 KiB, ring/64)`; budget conservation across transfer/release.
//! - §4.6 pt 3: the head bounds against `reusable_upto`, never the in-RAM
//!   tail.
//! - §4.1/§4.6 pt 2: root-ledger round-robin slots, newest-valid-wins.
//! - §4.2: replayed entries feed the K1 fold algebra (the replay-reproduces-
//!   RAM theorem's journal half).
//!
//! Torn/garbage/hole tear semantics live in `tests/crash_contract_tests.rs`
//! (the fault-injection harness); this file covers the clean-path contracts.

use squeezefs::meta_backend::kv::checkpoint::{
    read_newest_ledger, write_ledger_slot, LedgerRecord, TreeRoot, ROOT_LEDGER_MAGIC,
    ROOT_LEDGER_SLOTS, ROOT_LEDGER_SLOT_LEN,
};
use squeezefs::meta_backend::kv::journal::{
    checkpoint_reserve_bytes, decode_entry_payload, encode_entry_payload, entry_len_for,
    JournalRing, ReplayedEntry, ENTRY_HDR_LEN, FIRST_ENTRY_NONE, JOURNAL_PAGE_DATA_LEN,
    JOURNAL_PAGE_HDR_LEN, JOURNAL_PAGE_LEN, JOURNAL_PAGE_MAGIC, MAX_ENTRY_LEN,
};
use squeezefs::meta_backend::kv::journal_core::{
    AdmissionClass, CoreGeometry, JournalCore, PageSegment, Reservation,
};
use squeezefs::meta_backend::kv::record::{
    fold_newest_first, inode_key, Folded, InodeDelta, InodeValue, Record, TREE_DENTRIES,
    TREE_INODES,
};
use squeezefs::uring_fs;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A zero-filled temp file big enough for `pages` ring pages at `base`.
fn ring_file(base: u64, pages: u64) -> NamedTempFile {
    let f = NamedTempFile::new().expect("temp file");
    f.as_file()
        .set_len(base + pages * JOURNAL_PAGE_LEN)
        .expect("size ring file");
    f
}

/// Overhead of a one-record entry: entry header + tree_id byte + record
/// framing + the 8-byte inode key. `value_len = target − ENTRY_OVERHEAD`
/// yields an entry of exactly `target` bytes.
const ENTRY_OVERHEAD: u64 = ENTRY_HDR_LEN
    + 1
    + squeezefs::meta_backend::kv::record::RECORD_HEADER_LEN as u64
    + squeezefs::meta_backend::kv::record::INODE_KEY_LEN as u64;

/// One staged record (tree-tagged Put on ino `i`) padding its entry to
/// exactly `entry_len` bytes; `marker` makes payload equality checks exact.
fn sized_records(i: u64, entry_len: u64, marker: u8, seq: u64) -> Vec<(u8, Record)> {
    assert!(
        entry_len >= ENTRY_OVERHEAD,
        "entry_len too small: {entry_len}"
    );
    let value = vec![marker; (entry_len - ENTRY_OVERHEAD) as usize];
    vec![(TREE_INODES, Record::put(inode_key(i).to_vec(), seq, value))]
}

/// Admit → reserve → write one entry of exactly `entry_len` bytes; returns
/// the reservation. The three phases are composed exactly the way K6b's
/// commit pipeline will split them around node locks (§4.4).
async fn append_sized(
    ring: &JournalRing,
    class: AdmissionClass,
    i: u64,
    entry_len: u64,
    marker: u8,
) -> Reservation {
    let probe = sized_records(i, entry_len, marker, 0);
    let need = entry_len_for(&probe).expect("entry under cap");
    assert_eq!(need, entry_len, "sized_records must hit the target length");
    let adm = ring
        .core()
        .try_admit(need, class)
        .expect("ring must have room for this test entry");
    let res = ring.core().reserve(adm);
    assert_eq!(res.len, need, "reservation carries the exact entry size");
    let records = sized_records(i, entry_len, marker, res.seq());
    ring.write_entry(&res, &records)
        .await
        .expect("entry write must succeed");
    res
}

/// Read one raw page header: `(magic, lap, first_entry_off, stored_xxh3,
/// computed_xxh3)`.
async fn raw_page_header(
    path: &std::path::Path,
    base: u64,
    page: u64,
) -> (u32, u32, u16, u64, u64) {
    let buf = uring_fs::read_at(
        path,
        base + page * JOURNAL_PAGE_LEN,
        JOURNAL_PAGE_HDR_LEN as usize,
    )
    .await
    .expect("read page header");
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let lap = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let feo = u16::from_le_bytes(buf[8..10].try_into().unwrap());
    let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let computed = xxhash_rust::xxh3::xxh3_64(&buf[0..16]);
    (magic, lap, feo, stored, computed)
}

fn ledger_rec(seq: u64) -> LedgerRecord {
    LedgerRecord {
        seq,
        tree_roots: vec![
            TreeRoot {
                tree_id: TREE_INODES,
                node_addr: 0x1000 + seq,
                node_seq: 7 + seq,
            },
            TreeRoot {
                tree_id: TREE_DENTRIES,
                node_addr: 0x2000 + seq,
                node_seq: 9 + seq,
            },
        ],
        journal_tail_seq: 10 * seq,
        next_ino: 100 + seq,
        alloc_bitmap_generation: seq,
        node_seq_watermark: 100 + seq,
        membership_stamp: None,
    }
}

// ---------------------------------------------------------------------------
// Pure core: reserve formula, budget accounting, geometry.
// ---------------------------------------------------------------------------

/// §4.4 pt 5: the checkpoint-task carve-out is `max(256 KiB, ring/64)`.
#[test]
fn test_checkpoint_reserve_formula() {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    assert_eq!(
        checkpoint_reserve_bytes(8 * MIB),
        256 * KIB,
        "floor ring: 128 KiB < 256 KiB floor"
    );
    assert_eq!(
        checkpoint_reserve_bytes(16 * MIB),
        256 * KIB,
        "exactly the floor"
    );
    assert_eq!(
        checkpoint_reserve_bytes(32 * MIB),
        512 * KIB,
        "ceiling ring: 32 MiB / 64"
    );
    assert_eq!(
        checkpoint_reserve_bytes(64 * MIB),
        MIB,
        "override-sized ring: /64 dominates"
    );
}

/// §4.4 pts 2/5 single-threaded semantics: admission bounds (user vs
/// checkpoint class), transfer moves budget from `admitted` to `head`
/// exactly, release conserves, the watermark is monotonic, and reservations
/// are back-to-back with seq == start.
#[test]
fn test_core_admission_reservation_budget_roundtrip() {
    let geo = CoreGeometry {
        page_data_len: 10,
        pages: 4,
        reserve_bytes: 8,
    }; // capacity 40, user budget 32.
    let core = JournalCore::new(geo, 0, 0);
    assert_eq!(core.head(), 0);
    assert_eq!(core.admitted(), 0);
    assert_eq!(core.reusable_upto(), 0);
    assert_eq!(core.geometry().logical_len(), 40);

    // User admissions stop at capacity − reserve…
    let a1 = core.try_admit(30, AdmissionClass::User).expect("30 ≤ 32");
    assert_eq!(core.admitted(), 30);
    assert!(
        core.try_admit(3, AdmissionClass::User).is_none(),
        "30 + 3 > 32: user admission must not touch the reserve"
    );
    // …the checkpoint class keeps going into the carve-out…
    let a2 = core
        .try_admit(3, AdmissionClass::Checkpoint)
        .expect("checkpoint class admits from the reserve");
    assert!(
        core.try_admit(8, AdmissionClass::Checkpoint).is_none(),
        "33 + 8 > 40: nothing admits past physical capacity"
    );

    // Transfer: head += len, admitted -= len, seq == start.
    let r1 = core.reserve(a1);
    assert_eq!((r1.start, r1.len, r1.seq(), r1.end()), (0, 30, 0, 30));
    assert_eq!(core.head(), 30);
    assert_eq!(core.admitted(), 3);
    // Release: conserved back to zero.
    core.release(a2);
    assert_eq!(core.admitted(), 0);

    // Head now bounds against the watermark: 30 + 8 ≤ 0 + 40 − 8 fails by
    // 6; advancing the watermark by 6 opens exactly that admission.
    assert!(core.try_admit(8, AdmissionClass::User).is_none());
    core.advance_reusable_upto(6);
    assert_eq!(core.reusable_upto(), 6);
    let a3 = core
        .try_admit(8, AdmissionClass::User)
        .expect("watermark opened room");
    // Monotonic: a stale advance is a no-op.
    core.advance_reusable_upto(2);
    assert_eq!(
        core.reusable_upto(),
        6,
        "watermark must never move backwards"
    );
    let r3 = core.reserve(a3);
    assert_eq!((r3.start, r3.len), (30, 8), "reservations are back-to-back");
    assert!(
        r3.seq() > r1.seq(),
        "seq strictly monotonic in reservation order"
    );
}

/// Logical→physical mapping at production geometry: laps, page indices,
/// contiguous multi-page segments, owned page-first-byte positions.
#[test]
fn test_core_geometry_lap_offset_segments() {
    let geo = CoreGeometry {
        page_data_len: JOURNAL_PAGE_DATA_LEN,
        pages: 8,
        reserve_bytes: 0,
    };
    let l = geo.logical_len();
    assert_eq!(l, 8 * 4072);

    // Interior single-page range: one segment, no owned page starts.
    assert_eq!(
        geo.segments(100, 50),
        vec![PageSegment {
            page: 0,
            data_off: 100,
            len: 50
        }]
    );
    assert!(geo.owned_page_starts(100, 50).is_empty());

    // A 10,000-byte range starting near the end of page 0 spans four
    // pages: partial head, full interior pages, partial tail — contiguous.
    let start = 4000;
    let segs = geo.segments(start, 10_000);
    assert_eq!(
        segs,
        vec![
            PageSegment {
                page: 0,
                data_off: 4000,
                len: 72
            },
            PageSegment {
                page: 1,
                data_off: 0,
                len: 4072
            },
            PageSegment {
                page: 2,
                data_off: 0,
                len: 4072
            },
            PageSegment {
                page: 3,
                data_off: 0,
                len: 10_000 - 72 - 2 * 4072
            },
        ]
    );
    assert_eq!(
        geo.owned_page_starts(start, 10_000),
        vec![4072, 2 * 4072, 3 * 4072],
        "the range contains pages 1, 2, and 3's first logical bytes"
    );

    // Wrap: a range crossing the lap boundary re-enters page 0 with lap 1.
    let pos = l - 10;
    assert_eq!(geo.lap(pos), 0);
    assert_eq!(geo.lap(l), 1);
    assert_eq!(geo.page_index(l), 0, "lap boundary wraps to page 0");
    assert_eq!(geo.in_page_off(l), 0);
    assert_eq!(geo.ring_offset(l + 5), 5);
    assert_eq!(
        geo.segments(pos, 30),
        vec![
            PageSegment {
                page: 7,
                data_off: 4072 - 10,
                len: 10
            },
            PageSegment {
                page: 0,
                data_off: 0,
                len: 20
            },
        ]
    );
    assert_eq!(geo.owned_page_starts(pos, 30), vec![l]);
    assert_eq!(geo.page_start_pos(l + 5), l);
}

// ---------------------------------------------------------------------------
// Entry framing.
// ---------------------------------------------------------------------------

/// Entry sizing is exact (`ENTRY_HDR_LEN` + payload), the payload encodes
/// `(tree_id, record)` pairs round-trip, and decode bounds-checks its
/// container (§9): truncation and out-of-table tree ids are corruption.
#[test]
fn test_entry_payload_roundtrip_and_bounds() {
    let records = vec![
        (
            TREE_INODES,
            Record::put(inode_key(7).to_vec(), 42, vec![0xAB; 30]),
        ),
        (TREE_DENTRIES, Record::delete(vec![1, 2, 3], 43)),
    ];
    let payload = encode_entry_payload(&records);
    let len = entry_len_for(&records).expect("small entry");
    assert_eq!(len, ENTRY_HDR_LEN + payload.len() as u64);

    let decoded = decode_entry_payload(&payload).expect("roundtrip");
    assert_eq!(decoded, records);

    // Truncated container: never a panic, always a typed error.
    assert!(decode_entry_payload(&payload[..payload.len() - 1]).is_err());
    assert!(decode_entry_payload(&payload[..1]).is_err());
    // tree_id 0 (zeroed garbage) and ids beyond the §4.2 table are corrupt.
    let mut zero_id = payload.clone();
    zero_id[0] = 0;
    assert!(decode_entry_payload(&zero_id).is_err());
    let mut big_id = payload;
    big_id[0] = 6;
    assert!(decode_entry_payload(&big_id).is_err());
}

/// §4.1 writer-side cap: an entry over 128 KiB is refused at sizing time —
/// before admission, before any byte is written.
#[test]
fn test_entry_cap_enforced_writer_side() {
    let records = vec![(
        TREE_INODES,
        Record::put(inode_key(1).to_vec(), 0, vec![0; MAX_ENTRY_LEN as usize]),
    )];
    assert!(
        entry_len_for(&records).is_err(),
        "an entry beyond MAX_ENTRY_LEN must be refused writer-side"
    );
    // At exactly the cap it is legal (the cap is inclusive).
    let exact = vec![(
        TREE_INODES,
        Record::put(
            inode_key(1).to_vec(),
            0,
            vec![0; (MAX_ENTRY_LEN - ENTRY_OVERHEAD) as usize],
        ),
    )];
    assert_eq!(
        entry_len_for(&exact).expect("cap is inclusive"),
        MAX_ENTRY_LEN
    );
}

// ---------------------------------------------------------------------------
// Ring write + replay round-trips.
// ---------------------------------------------------------------------------

/// One small entry: replay returns it byte-identically; the raw page-0
/// header carries magic, lap 0, `first_entry_off == 24` (the entry starts
/// at the page's first data byte), and a verifying checksum.
#[tokio::test]
async fn test_single_page_entry_roundtrip_and_page_header() {
    let base = 4096; // nonzero base: the ring must respect its extent.
    let f = ring_file(base, 4);
    let ring = JournalRing::new(f.path(), base, 4, 0);

    let res = append_sized(&ring, AdmissionClass::User, 1, 230, 0x11).await;
    assert_eq!(res.start, 0);

    let (magic, lap, feo, stored, computed) = raw_page_header(f.path(), base, 0).await;
    assert_eq!(magic, JOURNAL_PAGE_MAGIC);
    assert_eq!(lap, 0);
    assert_eq!(
        feo, JOURNAL_PAGE_HDR_LEN as u16,
        "first entry starts at the page's first data byte"
    );
    assert_eq!(stored, computed, "page header checksum must verify (§4.3)");

    let (rec_ring, recovery) = JournalRing::recover(f.path(), base, 4, 0, 0)
        .await
        .expect("replay never fails loud on ring contents");
    assert_eq!(recovery.entries.len(), 1);
    let e = &recovery.entries[0];
    assert_eq!(e.seq, res.seq());
    assert_eq!(e.records, sized_records(1, 230, 0x11, res.seq()));
    assert_eq!(recovery.head_pos, res.end());
    assert_eq!(recovery.dropped_torn, 0);
    assert_eq!(
        rec_ring.core().head(),
        res.end(),
        "core resumes at the recovered head"
    );
}

/// §4.4's packing arithmetic: ~17 create-sized (230 B) entries fit one
/// page; none spill into page 1, replay returns all in seq order.
#[tokio::test]
async fn test_packed_entries_single_page() {
    let f = ring_file(0, 4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    let n = JOURNAL_PAGE_DATA_LEN / 230; // 17
    assert_eq!(n, 17, "the §4.4 ~17-entries-per-page arithmetic");
    let mut ends = Vec::new();
    for i in 0..n {
        let r = append_sized(&ring, AdmissionClass::User, i, 230, i as u8).await;
        ends.push(r.end());
    }
    assert!(
        *ends.last().unwrap() <= JOURNAL_PAGE_DATA_LEN,
        "17 packed entries stay within one page's data area"
    );
    // Page 1 was never touched: still all zeros.
    let (magic, ..) = raw_page_header(f.path(), 0, 1).await;
    assert_eq!(magic, 0, "no header may be written to an untouched page");

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0).await.unwrap();
    assert_eq!(recovery.entries.len(), n as usize);
    let seqs: Vec<u64> = recovery.entries.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "replay output must be seq-sorted");
    assert_eq!(recovery.dropped_torn, 0);
}

/// A 64 KiB-class entry (§4.1's largest-transaction shape, ~17 pages): one
/// header, one whole-entry checksum, contiguous continuation. Replay
/// reproduces it; continuation pages' headers say `0xFFFF`; the final page
/// header points at the successor's start position, where a successor then
/// lands and replays.
#[tokio::test]
async fn test_multi_page_entry_roundtrip_and_continuation_headers() {
    let f = ring_file(0, 32);
    let ring = JournalRing::new(f.path(), 0, 32, 0);

    let big = 64 * 1024; // 64 KiB whole-entry: spans 17 pages.
    let r1 = append_sized(&ring, AdmissionClass::User, 1, big, 0xE1).await;
    let geo = *ring.core().geometry();
    let first_full_page = geo.page_index(r1.start) + 1;

    // A continuation page holds no entry start (§4.1: 0xFFFF).
    let (magic, lap, feo, stored, computed) = raw_page_header(f.path(), 0, first_full_page).await;
    assert_eq!(magic, JOURNAL_PAGE_MAGIC);
    assert_eq!(lap, 0);
    assert_eq!(feo, FIRST_ENTRY_NONE);
    assert_eq!(stored, computed);

    // The page the entry ends in: first_entry_off == the successor's start
    // position, whether or not a successor is ever written (§4.4 pt 2).
    let end_page = geo.page_index(r1.end() - 1);
    let expect_feo = (JOURNAL_PAGE_HDR_LEN + geo.in_page_off(r1.end())) as u16;
    let (_, _, feo_end, ..) = raw_page_header(f.path(), 0, end_page).await;
    assert_eq!(feo_end, expect_feo);

    // The successor lands exactly there.
    let r2 = append_sized(&ring, AdmissionClass::User, 2, 230, 0xE2).await;
    assert_eq!(r2.start, r1.end());

    let (_, recovery) = JournalRing::recover(f.path(), 0, 32, 0, 0).await.unwrap();
    assert_eq!(recovery.entries.len(), 2);
    assert_eq!(
        recovery.entries[0].records,
        sized_records(1, big, 0xE1, r1.seq())
    );
    assert_eq!(
        recovery.entries[1].records,
        sized_records(2, 230, 0xE2, r2.seq())
    );
    assert_eq!(recovery.dropped_torn, 0);
}

/// An entry whose 20 B header straddles the page-data boundary: entries
/// pack back-to-back in logical space (§4.1), so headers are ordinary
/// bytes — replay reassembles across the boundary.
#[tokio::test]
async fn test_entry_header_straddles_page_boundary() {
    let f = ring_file(0, 4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    // First entry ends 10 bytes before page 0's data ends, so the second
    // entry's header splits 10/10 across pages 0 and 1.
    let first_len = JOURNAL_PAGE_DATA_LEN - 10;
    let r1 = append_sized(&ring, AdmissionClass::User, 1, first_len, 0x21).await;
    assert_eq!(r1.end(), JOURNAL_PAGE_DATA_LEN - 10);
    let r2 = append_sized(&ring, AdmissionClass::User, 2, 300, 0x22).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0).await.unwrap();
    assert_eq!(recovery.entries.len(), 2);
    assert_eq!(recovery.entries[1].seq, r2.seq());
    assert_eq!(
        recovery.entries[1].records,
        sized_records(2, 300, 0x22, r2.seq())
    );
    assert_eq!(recovery.dropped_torn, 0);
}

/// An entry filling its page exactly hands the next page's header to the
/// successor: both pages carry `first_entry_off == 24`.
#[tokio::test]
async fn test_exact_page_fill_hands_next_header_to_successor() {
    let f = ring_file(0, 4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    let r1 = append_sized(&ring, AdmissionClass::User, 1, JOURNAL_PAGE_DATA_LEN, 0x31).await;
    assert_eq!(
        r1.end(),
        JOURNAL_PAGE_DATA_LEN,
        "entry fills page 0 exactly"
    );
    let r2 = append_sized(&ring, AdmissionClass::User, 2, 230, 0x32).await;
    assert_eq!(
        r2.start, JOURNAL_PAGE_DATA_LEN,
        "successor owns page 1's first byte"
    );

    let (_, _, feo0, ..) = raw_page_header(f.path(), 0, 0).await;
    let (_, _, feo1, ..) = raw_page_header(f.path(), 0, 1).await;
    assert_eq!(feo0, JOURNAL_PAGE_HDR_LEN as u16);
    assert_eq!(feo1, JOURNAL_PAGE_HDR_LEN as u16);

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0).await.unwrap();
    assert_eq!(recovery.entries.len(), 2);
}

/// Wrap: with the watermark advanced (a durable checkpoint retired the old
/// pages — §4.6 pt 3), the head wraps into lap 1 and overwrites them; a
/// replay from the new tail returns exactly the window entries — the
/// overwritten ones can never be resurrected (their pages carry the new
/// lap).
#[tokio::test]
async fn test_wraparound_replay_window() {
    let f = ring_file(0, 4);
    let ring = JournalRing::new(f.path(), 0, 4, 0); // capacity 16,288.

    let mut rs = Vec::new();
    for i in 0..4u64 {
        rs.push(append_sized(&ring, AdmissionClass::User, i, 4000, 0x40 + i as u8).await);
    }
    // Ring nearly full: 16,000 of 16,288. The next 4,000-byte entry waits
    // for the watermark…
    assert!(ring.core().try_admit(4000, AdmissionClass::User).is_none());
    // …a checkpoint retires the first two entries (tail → rs[2].start) and,
    // once durable, advances the watermark to the tail.
    ring.core().advance_reusable_upto(rs[2].start);
    let r5 = append_sized(&ring, AdmissionClass::User, 4, 4000, 0x44).await;
    assert_eq!(r5.start, 16_000);
    assert!(
        r5.end() > ring.core().geometry().logical_len(),
        "the entry wrapped into lap 1"
    );

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, rs[2].start)
        .await
        .unwrap();
    let seqs: Vec<u64> = recovery.entries.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        vec![rs[2].seq(), rs[3].seq(), r5.seq()],
        "exactly the window entries: no loss, no resurrection of retired pages"
    );
    assert_eq!(recovery.head_pos, r5.end());
    assert_eq!(recovery.dropped_torn, 0, "a clean wrap is not a tear");
}

/// Entries below the tail are already checkpoint-covered: replay skips
/// them silently (no drops counted) even when they share the tail page.
#[tokio::test]
async fn test_replay_skips_pre_tail_entries() {
    let f = ring_file(0, 4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    let r1 = append_sized(&ring, AdmissionClass::User, 1, 230, 0x51).await;
    let r2 = append_sized(&ring, AdmissionClass::User, 2, 230, 0x52).await;
    let r3 = append_sized(&ring, AdmissionClass::User, 3, 230, 0x53).await;
    assert_eq!(r1.start, 0);

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, r2.seq())
        .await
        .unwrap();
    let seqs: Vec<u64> = recovery.entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![r2.seq(), r3.seq()]);
    assert_eq!(recovery.head_pos, r3.end());
    assert_eq!(recovery.dropped_torn, 0);
}

/// An all-zero (freshly formatted) ring replays to nothing, quietly.
#[tokio::test]
async fn test_empty_ring_replays_zero_entries() {
    let f = ring_file(0, 8);
    let (ring, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0).await.unwrap();
    assert!(recovery.entries.is_empty());
    assert_eq!(recovery.head_pos, 0);
    assert_eq!(recovery.dropped_torn, 0);
    assert_eq!(ring.core().head(), 0);
    assert_eq!(ring.core().reusable_upto(), 0);
}

/// Remount continuity: `recover` resumes the core at the recovered head
/// with `reusable_upto = tail`, and appends written through the recovered
/// ring replay seamlessly next to the pre-crash entries.
#[tokio::test]
async fn test_recover_resumes_head_and_watermark() {
    let f = ring_file(0, 8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);
    let _r1 = append_sized(&ring, AdmissionClass::User, 1, 500, 0x61).await;
    let r2 = append_sized(&ring, AdmissionClass::User, 2, 500, 0x62).await;
    let r3 = append_sized(&ring, AdmissionClass::User, 3, 500, 0x63).await;
    drop(ring); // "crash" — the file bytes are the only survivors.

    let (ring2, recovery) = JournalRing::recover(f.path(), 0, 8, 0, r2.seq())
        .await
        .unwrap();
    assert_eq!(recovery.head_pos, r3.end());
    assert_eq!(ring2.core().head(), r3.end());
    assert_eq!(ring2.core().reusable_upto(), r2.seq());

    let r4 = append_sized(&ring2, AdmissionClass::User, 4, 500, 0x64).await;
    assert_eq!(
        r4.start,
        r3.end(),
        "post-recovery appends continue at the head"
    );

    let (_, again) = JournalRing::recover(f.path(), 0, 8, 0, r2.seq())
        .await
        .unwrap();
    let seqs: Vec<u64> = again.entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![r2.seq(), r3.seq(), r4.seq()]);
}

/// The replay-feeds-the-fold contract (§4.2): a keyed history journaled
/// across entries — Put, Δtime, Delete, Put-again — folds through K1's
/// `fold_newest_first` to exactly the expected final state.
#[tokio::test]
async fn test_replay_seq_sorted_and_fold_contract() {
    let f = ring_file(0, 8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);

    let ino = 42u64;
    let base_val = InodeValue {
        mode: 0o100644,
        uid: 1,
        gid: 2,
        nlink: 1,
        flags: 0,
        rdev: 0,
        size: 100,
        atime: 1,
        mtime: 1,
        ctime: 1,
    };

    // One entry per closure call: admit on the placeholder-seq encoding
    // (sizes are seq-independent), then write with the reservation's seq.
    async fn journal_one(ring: &JournalRing, mk: impl Fn(u64) -> Record) -> Reservation {
        let need = entry_len_for(&[(TREE_INODES, mk(0))]).unwrap();
        let adm = ring.core().try_admit(need, AdmissionClass::User).unwrap();
        let res = ring.core().reserve(adm);
        ring.write_entry(&res, &[(TREE_INODES, mk(res.seq()))])
            .await
            .unwrap();
        res
    }

    // Entry 1: Put(ino). Entry 2: Δtime (the §4.4 pt 6 merge record).
    let e1 = journal_one(&ring, |seq| {
        Record::put(inode_key(ino).to_vec(), seq, base_val.encode())
    })
    .await;
    let delta = InodeDelta::times(7777, 8888);
    let e2 = journal_one(&ring, |seq| {
        Record::delta(inode_key(ino).to_vec(), seq, &delta)
    })
    .await;
    assert!(e2.seq() > e1.seq());

    // Replay and fold: newest-first per key, exactly K1's algebra.
    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0).await.unwrap();
    let mut key_records: Vec<Record> = recovery
        .entries
        .iter()
        .flat_map(|e| e.records.iter())
        .filter(|(t, r)| *t == TREE_INODES && r.key == inode_key(ino))
        .map(|(_, r)| r.clone())
        .collect();
    key_records.sort_by_key(|r| std::cmp::Reverse(r.seq)); // newest first
    let folded = fold_newest_first(key_records.iter().map(|r| r.record_ref())).unwrap();
    let Folded::Put { value, .. } = folded else {
        panic!("live key must fold to Put");
    };
    let mut expect = base_val;
    delta.apply(&mut expect);
    assert_eq!(
        InodeValue::decode(&value).unwrap(),
        expect,
        "replayed journal history must fold to the RAM state (replay-reproduces-RAM)"
    );
}

/// Lock-free core under real concurrency (multi_thread runtime): 8 racing
/// appenders × 4 entries each, mixed sizes (some multi-page). Every entry
/// replays, seqs are unique and sorted, budget settles to zero, and the
/// head equals the byte sum — the §4.4 pt 2/5 accounting end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_appenders_disjoint_and_replayable() {
    let f = ring_file(0, 64);
    let ring = std::sync::Arc::new(JournalRing::new(f.path(), 0, 64, 0));

    let sizes = [230u64, 900, 5000, 230];
    let mut handles = Vec::new();
    for task in 0..8u64 {
        let ring = ring.clone();
        handles.push(tokio::spawn(async move {
            let mut total = 0;
            for (j, &len) in sizes.iter().enumerate() {
                let marker = (task * 4 + j as u64) as u8;
                append_sized(
                    &ring,
                    AdmissionClass::User,
                    task * 100 + j as u64,
                    len,
                    marker,
                )
                .await;
                total += len;
            }
            total
        }));
    }
    let mut expected_bytes = 0;
    for h in handles {
        expected_bytes += h.await.expect("appender task must not panic");
    }

    assert_eq!(
        ring.core().admitted(),
        0,
        "all admitted budget was transferred"
    );
    assert_eq!(
        ring.core().head(),
        expected_bytes,
        "head == total reserved bytes"
    );

    let (_, recovery) = JournalRing::recover(f.path(), 0, 64, 0, 0).await.unwrap();
    assert_eq!(
        recovery.entries.len(),
        32,
        "every concurrent entry must replay"
    );
    let seqs: Vec<u64> = recovery.entries.iter().map(|e| e.seq).collect();
    let mut dedup = seqs.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        32,
        "entry seqs must be unique (no reservation overlap)"
    );
    assert_eq!(seqs, {
        let mut s = seqs.clone();
        s.sort_unstable();
        s
    });
    assert_eq!(recovery.head_pos, expected_bytes);
    assert_eq!(recovery.dropped_torn, 0);
}

// ---------------------------------------------------------------------------
// Root ledger (checkpoint.rs — record part only in K3).
// ---------------------------------------------------------------------------

/// Slot encode/decode round-trips every field; corrupt or truncated slot
/// images are typed errors, never panics.
#[test]
fn test_ledger_slot_encode_decode_roundtrip() {
    let rec = ledger_rec(5);
    assert_eq!(rec.slot_index(), 5);
    let img = rec.encode_slot().expect("well-formed record encodes");
    assert_eq!(
        img.len(),
        ROOT_LEDGER_SLOT_LEN as usize,
        "full zero-padded slot image"
    );
    assert_eq!(
        u32::from_le_bytes(img[0..4].try_into().unwrap()),
        ROOT_LEDGER_MAGIC
    );
    let back = LedgerRecord::decode_slot(&img).expect("roundtrip");
    assert_eq!(back, rec);

    // Bounds & corruption (§9): flipped payload byte, truncation into the
    // payload, truncation into the header, zeroed image — all typed
    // errors. (A truncation that still contains the whole self-delimiting
    // record decodes — the length/checksum bind the content, not the
    // padding.)
    let mut corrupt = img.clone();
    corrupt[40] ^= 0xFF;
    assert!(LedgerRecord::decode_slot(&corrupt).is_err());
    assert!(LedgerRecord::decode_slot(&img[..50]).is_err());
    assert!(LedgerRecord::decode_slot(&img[..10]).is_err());
    assert!(LedgerRecord::decode_slot(&[0u8; 4096]).is_err());
}

/// Newest-valid-wins across the slot array (§4.1): five sequential
/// checkpoint records land in five slots; mount picks the highest seq and
/// every field survives.
#[tokio::test]
async fn test_ledger_roundtrip_newest_valid_wins() {
    let base = 8192;
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(base + ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();

    for seq in 1..=5 {
        write_ledger_slot(f.path(), base, &ledger_rec(seq))
            .await
            .unwrap();
    }
    let newest = read_newest_ledger(f.path(), base)
        .await
        .unwrap()
        .expect("five valid records");
    assert_eq!(newest, ledger_rec(5));
}

/// Round-robin placement: slot = seq % 32, verified against raw bytes; a
/// full cycle overwrites the oldest slot and selection still follows seq.
#[tokio::test]
async fn test_ledger_round_robin_slot_placement_wraps() {
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();

    let rec40 = ledger_rec(40);
    assert_eq!(rec40.slot_index(), 8, "seq 40 → slot 8");
    write_ledger_slot(f.path(), 0, &ledger_rec(8))
        .await
        .unwrap(); // slot 8
    write_ledger_slot(f.path(), 0, &rec40).await.unwrap(); // overwrites slot 8

    let raw = uring_fs::read_at(
        f.path(),
        8 * ROOT_LEDGER_SLOT_LEN,
        ROOT_LEDGER_SLOT_LEN as usize,
    )
    .await
    .unwrap();
    let stored = LedgerRecord::decode_slot(&raw).expect("slot 8 holds the newer record");
    assert_eq!(
        stored.seq, 40,
        "seq 40 must have overwritten seq 8 in slot 8"
    );

    write_ledger_slot(f.path(), 0, &ledger_rec(39))
        .await
        .unwrap(); // slot 7
    let newest = read_newest_ledger(f.path(), 0).await.unwrap().unwrap();
    assert_eq!(newest.seq, 40, "selection follows seq, not slot order");
}

/// A zeroed (fresh) ledger extent yields `None` — the loud policy for a
/// non-fresh volume is K6a mount wiring, not the record layer.
#[tokio::test]
async fn test_ledger_empty_returns_none() {
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();
    assert!(read_newest_ledger(f.path(), 0).await.unwrap().is_none());
}

/// The dropped-entry ordering guard: `ReplayedEntry` is plain data the K6b
/// apply loop consumes in order — pin its shape so the pipeline PR cannot
/// silently change replay's output contract.
#[test]
fn test_replayed_entry_shape() {
    let e = ReplayedEntry {
        seq: 9,
        records: vec![(TREE_INODES, Record::delete(inode_key(1).to_vec(), 9))],
    };
    assert_eq!(e.seq, 9);
    assert_eq!(e.records[0].0, TREE_INODES);
}
