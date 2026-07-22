//! PR K2 node-format contract tests (design §4.1/§4.2/§4.3/§4.5).
//!
//! Integration tests over **real temp files through `crate::uring_fs`**
//! (io_uring-only, per AGENTS.md): node header roundtrip + self-address
//! checks, 4 KiB-granular bset appends with LWW folds across the log,
//! the record-value cap `min(65,536, node_size/4)` enforced at this layer
//! with a typed error, `NodeFull` append backpressure, compact/split as
//! pure functions over caller-provided (injected) extents — the allocator
//! arrives in PR K4 — and the recycled-extent incarnation rule
//! (`node_seq_at_write`).
//!
//! Tear-injection cases (torn tail bset, the §4.5 loud positional
//! classifier, torn rewrite) live with the rest of the crash harness in
//! `tests/crash_contract_tests.rs`.

use squeezefs::meta_backend::kv::node::{
    append_bset, compact_node, key_successor, load_node, record_value_cap, split_node, write_node,
    AppendDest, NodeLayout, NodeWriteParams, SplitDest, DEFAULT_NODE_SIZE, MAX_NODE_SIZE,
    MIN_NODE_SIZE, NODE_PAGE,
};
use squeezefs::meta_backend::kv::record::{
    inode_key, Folded, InodeDelta, InodeValue, Record, RecordKind,
};
use squeezefs::meta_backend::kv::KvError;
use squeezefs::uring_fs;
use tempfile::NamedTempFile;

/// A file-backed "volume" large enough for a few injected node extents.
const VOL_SIZE: u64 = 8 * 1024 * 1024;

/// Fresh fixed-size file volume (extents beyond EOF would short-read; real
/// volumes are fixed-size devices — `MetaLvStorage` discipline).
fn fresh_volume() -> NamedTempFile {
    let tmp = NamedTempFile::new().expect("create temp volume");
    tmp.as_file().set_len(VOL_SIZE).expect("size volume");
    tmp
}

fn layout() -> NodeLayout {
    NodeLayout::new(DEFAULT_NODE_SIZE).expect("default layout is valid")
}

fn inode_value(seed: u64) -> InodeValue {
    InodeValue {
        mode: 0o100644,
        uid: seed as u32,
        gid: 0,
        nlink: 1,
        flags: 0,
        flags2: 0,
        size: seed * 3,
        atime: seed,
        mtime: seed,
        ctime: seed,
    }
}

fn put(ino: u64, seq: u64) -> Record {
    Record::put(inode_key(ino).to_vec(), seq, inode_value(seq).encode())
}

fn params<'a>(addr: u64, seq: u64, min_key: &'a [u8], max_key: &'a [u8]) -> NodeWriteParams<'a> {
    NodeWriteParams {
        node_addr: addr,
        node_seq: seq,
        tree_id: squeezefs::meta_backend::kv::record::TREE_INODES,
        level: 0,
        min_key,
        max_key,
    }
}

// ---------------------------------------------------------------------------
// The format knob and the record-value cap (§4.2, §5.1).
// ---------------------------------------------------------------------------

/// `NodeLayout` validates the format knob: 64 KiB–1 MiB, 4 KiB-aligned.
#[test]
fn test_node_layout_validates_the_format_knob() {
    for good in [MIN_NODE_SIZE, 128 * 1024, DEFAULT_NODE_SIZE, MAX_NODE_SIZE] {
        let l = NodeLayout::new(good).unwrap_or_else(|e| panic!("{good} must be valid: {e}"));
        assert_eq!(l.node_size(), good);
    }
    for bad in [
        0,
        NODE_PAGE,
        MIN_NODE_SIZE - NODE_PAGE,
        DEFAULT_NODE_SIZE + 1,
        DEFAULT_NODE_SIZE + 512,
        MAX_NODE_SIZE * 2,
    ] {
        assert!(
            matches!(NodeLayout::new(bad), Err(KvError::Corrupt(_))),
            "node_size {bad} must be rejected as a format bug"
        );
    }
}

/// The per-volume record-value budget is the user VALUE cap
/// `min(65,536, node_size/4)` (§4.2 — exactly Linux `XATTR_SIZE_MAX` at
/// the 256 KiB default, scaling down with the knob, ceiling-clamped
/// above it) PLUS the xattr record envelope allowance (1-byte name_len +
/// name ≤ 255), so a full cap-sized user value with a maximal name still
/// encodes (fstests generic/020, VL10 release gate).
#[test]
fn test_record_value_cap_is_min_of_ceiling_and_quarter_node() {
    use squeezefs::meta_backend::kv::node::{xattr_value_cap, XATTR_RECORD_ENVELOPE_MAX};
    assert_eq!(XATTR_RECORD_ENVELOPE_MAX, 256, "1-byte name_len + 255 name");
    assert_eq!(xattr_value_cap(DEFAULT_NODE_SIZE), 65_536);
    assert_eq!(xattr_value_cap(MIN_NODE_SIZE), 16_384);
    assert_eq!(xattr_value_cap(128 * 1024), 32_768);
    assert_eq!(xattr_value_cap(512 * 1024), 65_536, "ceiling-clamped");
    assert_eq!(xattr_value_cap(MAX_NODE_SIZE), 65_536, "ceiling-clamped");
    for ns in [MIN_NODE_SIZE, 128 * 1024, DEFAULT_NODE_SIZE, MAX_NODE_SIZE] {
        assert_eq!(
            record_value_cap(ns),
            xattr_value_cap(ns) + XATTR_RECORD_ENVELOPE_MAX,
            "record budget = value cap + envelope at node_size {ns}"
        );
    }
    assert_eq!(
        layout().record_value_cap(),
        record_value_cap(DEFAULT_NODE_SIZE)
    );
}

/// The cap is enforced HERE (node layer) with a typed error, on both write
/// paths; a value of exactly the cap passes.
#[tokio::test]
async fn test_record_value_cap_enforced_with_typed_error() {
    let vol = fresh_volume();
    let small = NodeLayout::new(MIN_NODE_SIZE).expect("64 KiB layout");
    let cap = small.record_value_cap();
    assert_eq!(
        cap,
        16_384 + 256,
        "64 KiB knob ⇒ node_size/4 value cap + the xattr envelope allowance"
    );

    let over = Record::put(inode_key(7).to_vec(), 1, vec![0xAB; cap + 1]);
    let at_cap = Record::put(inode_key(7).to_vec(), 1, vec![0xAB; cap]);

    // write_node rejects an over-cap base record, typed.
    let err = write_node(
        vol.path(),
        &small,
        &params(0, 1, b"", b"\xff"),
        std::slice::from_ref(&over),
        1,
    )
    .await
    .expect_err("over-cap value must be rejected");
    match err {
        KvError::ValueTooLarge { len, cap: c } => {
            assert_eq!(len, cap + 1);
            assert_eq!(c, cap);
        }
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }

    // append_bset rejects it too.
    let written = write_node(vol.path(), &small, &params(0, 1, b"", b"\xff"), &[], 0)
        .await
        .expect("header-only node");
    let dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: written.bytes_written,
    };
    let err = append_bset(vol.path(), &small, &dest, &[over], 2)
        .await
        .expect_err("over-cap append must be rejected");
    assert!(
        matches!(err, KvError::ValueTooLarge { .. }),
        "append cap failure must stay typed, got {err:?}"
    );

    // Exactly at the cap: legal on both paths.
    let tail = append_bset(vol.path(), &small, &dest, std::slice::from_ref(&at_cap), 2)
        .await
        .expect("at-cap append is legal");
    assert!(tail > dest.tail_offset);
    write_node(
        vol.path(),
        &small,
        &params(MIN_NODE_SIZE as u64, 2, b"", b"\xff"),
        &[at_cap],
        1,
    )
    .await
    .expect("at-cap base record is legal");
}

// ---------------------------------------------------------------------------
// Write / load roundtrip (§4.1 header, §4.3 verify-once-at-load).
// ---------------------------------------------------------------------------

/// A written node loads back byte-faithfully: header identity, base
/// records, tail placement — at a nonzero injected extent offset.
#[tokio::test]
async fn test_write_load_roundtrip_preserves_header_and_base_records() {
    let vol = fresh_volume();
    let l = layout();
    let addr = 2 * DEFAULT_NODE_SIZE as u64; // injected, deliberately nonzero
    let records = vec![put(10, 1), put(20, 2), put(30, 3)];

    let written = write_node(
        vol.path(),
        &l,
        &params(addr, 42, &inode_key(10), &inode_key(30)),
        &records,
        3,
    )
    .await
    .expect("write");
    assert_eq!(written.node_addr, addr);
    assert_eq!(written.node_seq, 42);
    assert_eq!(written.record_count, 3);
    assert_eq!(written.journal_seq_horizon, 3);
    assert_eq!(
        written.bytes_written % NODE_PAGE,
        0,
        "images are page-granular"
    );

    let node = load_node(vol.path(), &l, addr, 0).await.expect("load");
    let h = node.header();
    assert_eq!(h.node_addr, addr, "self-address");
    assert_eq!(h.node_seq, 42);
    assert_eq!(h.tree_id, squeezefs::meta_backend::kv::record::TREE_INODES);
    assert_eq!(h.level, 0);
    assert_eq!(h.node_size, DEFAULT_NODE_SIZE as u32);
    assert_eq!(h.min_key, inode_key(10).to_vec());
    assert_eq!(h.max_key, inode_key(30).to_vec());

    assert_eq!(node.bset_count(), 1, "one base bset");
    assert_eq!(node.dropped_tail_bsets(), 0, "clean load drops nothing");
    assert_eq!(
        node.tail_offset(),
        written.bytes_written,
        "tail = header page + padded base frame"
    );
    let base = node.bset(0).expect("validated at load");
    assert_eq!(base.len(), 3);
    assert_eq!(base.journal_seq_horizon(), 3);
    let got: Vec<Record> = base.iter().map(|r| r.to_record()).collect();
    assert_eq!(got, records);
}

/// A header-only node (empty base — e.g. everything folded away) is legal:
/// zero bsets, tail right after the header page.
#[tokio::test]
async fn test_header_only_node_roundtrip() {
    let vol = fresh_volume();
    let l = layout();
    write_node(vol.path(), &l, &params(0, 7, b"", b"\xff"), &[], 0)
        .await
        .expect("header-only write");
    let node = load_node(vol.path(), &l, 0, 0).await.expect("load");
    assert_eq!(node.bset_count(), 0);
    assert_eq!(node.tail_offset(), NODE_PAGE);
    assert_eq!(node.dropped_tail_bsets(), 0);
    assert_eq!(node.header().min_key, b"".to_vec());
    assert_eq!(node.header().max_key, b"\xff".to_vec());
    assert_eq!(
        node.lookup(&inode_key(1)).expect("fold"),
        Folded::Absent,
        "empty node resolves everything absent"
    );
}

// ---------------------------------------------------------------------------
// Appends: 4 KiB granularity, never-touch-live-bytes, LWW folds (§4.1/§4.2).
// ---------------------------------------------------------------------------

/// Appends land at 4 KiB-aligned tails, the log loads in append order, and
/// point lookups fold newest-first across the whole log: Δtime onto its
/// base, tombstone shadowing, untouched keys borrowed zero-copy.
#[tokio::test]
async fn test_append_bsets_then_load_folds_lww() {
    let vol = fresh_volume();
    let l = layout();

    let written = write_node(
        vol.path(),
        &l,
        &params(0, 1, &inode_key(1), &inode_key(99)),
        &[put(1, 1), put(2, 2)],
        2,
    )
    .await
    .expect("base");

    // Append 1: Δtime on ino 1 + a new ino 3.
    let mut dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: written.bytes_written,
    };
    let d = InodeDelta::times(500, 501);
    let tail1 = append_bset(
        vol.path(),
        &l,
        &dest,
        &[Record::delta(inode_key(1).to_vec(), 5, &d), put(3, 4)],
        5,
    )
    .await
    .expect("append 1");
    assert_eq!(tail1 % NODE_PAGE, 0, "4 KiB append granularity");
    assert_eq!(
        tail1 - dest.tail_offset,
        NODE_PAGE,
        "a small bset consumes exactly one page"
    );

    // Append 2: tombstone ino 2.
    dest.tail_offset = tail1;
    let tail2 = append_bset(
        vol.path(),
        &l,
        &dest,
        &[Record::delete(inode_key(2).to_vec(), 7)],
        7,
    )
    .await
    .expect("append 2");
    assert!(tail2 > tail1);

    let node = load_node(vol.path(), &l, 0, 0).await.expect("load");
    assert_eq!(node.bset_count(), 3, "base + two appends, in order");
    assert_eq!(node.tail_offset(), tail2);
    assert_eq!(node.append_dest().tail_offset, tail2);
    assert_eq!(node.bset(1).expect("bset 1").journal_seq_horizon(), 5);
    assert_eq!(node.bset(2).expect("bset 2").journal_seq_horizon(), 7);

    // Fold: Δ applied onto the base Put.
    match node.lookup(&inode_key(1)).expect("fold ino 1") {
        Folded::Put { value, seq } => {
            assert_eq!(seq, 5);
            let v = InodeValue::decode(&value).expect("decode");
            assert_eq!((v.mtime, v.ctime), (500, 501));
            assert_eq!(v.uid, 1, "non-time fields keep the base value");
        }
        other => panic!("expected folded Put, got {other:?}"),
    }
    // Fold: tombstone shadows the base Put.
    assert_eq!(
        node.lookup(&inode_key(2)).expect("fold ino 2"),
        Folded::Tombstone { seq: 7 }
    );
    // Untouched key from the newest append's sibling bset.
    match node.lookup(&inode_key(3)).expect("fold ino 3") {
        Folded::Put { value, seq } => {
            assert_eq!(seq, 4);
            assert!(
                matches!(value, std::borrow::Cow::Borrowed(_)),
                "plain Put lookups borrow the node buffer (zero-copy)"
            );
        }
        other => panic!("expected borrowed Put, got {other:?}"),
    }
    assert_eq!(node.lookup(&inode_key(9)).expect("fold"), Folded::Absent);
}

/// A full log returns the typed `NodeFull` signal (the caller's cue to
/// compact — §4.6 pt 1) and the failed append leaves the node loadable with
/// every prior bset intact.
#[tokio::test]
async fn test_append_full_node_returns_node_full_and_keeps_node_loadable() {
    let vol = fresh_volume();
    let small = NodeLayout::new(MIN_NODE_SIZE).expect("64 KiB layout");
    write_node(vol.path(), &small, &params(0, 1, b"", b"\xff"), &[], 0)
        .await
        .expect("header-only node");

    // Each append: one ~2 KiB-value record ⇒ one 4 KiB page. 15 pages of
    // log space exist (64 KiB − header page).
    let mut dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: NODE_PAGE,
    };
    let mut appended = 0u64;
    let err = loop {
        let rec = Record::put(inode_key(appended).to_vec(), appended + 1, vec![0x5A; 2048]);
        match append_bset(vol.path(), &small, &dest, &[rec], appended + 1).await {
            Ok(tail) => {
                dest.tail_offset = tail;
                appended += 1;
                assert!(appended < 64, "64 KiB node cannot hold 64 pages");
            }
            Err(e) => break e,
        }
    };
    assert_eq!(appended, 15, "64 KiB = header page + 15 log pages");
    match err {
        KvError::NodeFull { needed, available } => {
            assert!(
                needed > available,
                "typed capacity math: {needed} > {available}"
            );
        }
        other => panic!("expected NodeFull, got {other:?}"),
    }

    let node = load_node(vol.path(), &small, 0, 0).await.expect("load");
    assert_eq!(
        node.bset_count(),
        appended as usize,
        "every prior bset intact"
    );
    assert_eq!(
        node.dropped_tail_bsets(),
        0,
        "a refused append writes nothing"
    );
    assert_eq!(node.tail_offset(), MIN_NODE_SIZE, "log area exactly full");
}

// ---------------------------------------------------------------------------
// Load-time verification (§4.3) and misdirection.
// ---------------------------------------------------------------------------

/// Header corruption fails the load loud; a node image copied to the wrong
/// extent (misdirected write / misdirected read) fails the self-address
/// check loud.
#[tokio::test]
async fn test_load_detects_header_corruption_and_misdirection() {
    let vol = fresh_volume();
    let l = layout();
    let addr = DEFAULT_NODE_SIZE as u64;
    write_node(
        vol.path(),
        &l,
        &params(addr, 3, &inode_key(1), &inode_key(2)),
        &[put(1, 1)],
        1,
    )
    .await
    .expect("write");

    // Flip one byte inside the header page (past the magic): loud.
    let page = uring_fs::read_at(vol.path(), addr, NODE_PAGE)
        .await
        .expect("read header page");
    let mut doctored = page.to_vec();
    doctored[17] ^= 0x01; // node_seq byte
    uring_fs::write_at(vol.path(), addr, doctored)
        .await
        .expect("scribble");
    let err = load_node(vol.path(), &l, addr, 0)
        .await
        .expect_err("corrupt header must fail loud");
    assert!(
        matches!(err, KvError::ChecksumMismatch { .. } | KvError::Corrupt(_)),
        "got {err:?}"
    );

    // Restore, then copy the whole (valid) extent elsewhere: self-address
    // mismatch must fail loud.
    uring_fs::write_at(vol.path(), addr, page.to_vec())
        .await
        .expect("restore");
    load_node(vol.path(), &l, addr, 0)
        .await
        .expect("restored node loads again");
    let image = uring_fs::read_at(vol.path(), addr, DEFAULT_NODE_SIZE)
        .await
        .expect("read extent");
    let elsewhere = 3 * DEFAULT_NODE_SIZE as u64;
    uring_fs::write_at(vol.path(), elsewhere, image)
        .await
        .expect("misdirect");
    let err = load_node(vol.path(), &l, elsewhere, 0)
        .await
        .expect_err("misdirected node must fail the self-address check");
    assert!(matches!(err, KvError::Corrupt(_)), "got {err:?}");

    // Never-written extents (zeros) fail loud too, not as a tear.
    let err = load_node(vol.path(), &l, 5 * DEFAULT_NODE_SIZE as u64, 0)
        .await
        .expect_err("a zeroed extent is not a node");
    assert!(matches!(err, KvError::Corrupt(_)), "got {err:?}");
}

/// Recycled-extent incarnation rule (§4.1 `node_seq_at_write`): stale bset
/// frames from a previous node life beyond the new node's tail neither leak
/// into the log nor trip the tear classifier — a clean end, zero drops.
#[tokio::test]
async fn test_stale_bsets_from_recycled_extent_do_not_leak() {
    let vol = fresh_volume();
    let l = layout();

    // First life at extent 0: base + two appends (seqs stamped node_seq=5).
    let first = write_node(vol.path(), &l, &params(0, 5, b"", b"\xff"), &[put(1, 1)], 1)
        .await
        .expect("first life");
    let mut dest = AppendDest {
        node_addr: 0,
        node_seq: 5,
        tail_offset: first.bytes_written,
    };
    dest.tail_offset = append_bset(vol.path(), &l, &dest, &[put(2, 2)], 2)
        .await
        .expect("first-life append 1");
    append_bset(vol.path(), &l, &dest, &[put(3, 3)], 3)
        .await
        .expect("first-life append 2");

    // The extent is "freed and reallocated" (K4's job): a NEW node (higher
    // node_seq) is written over it. Its image covers the header + base
    // only; the first life's append frames survive physically beyond the
    // new tail.
    write_node(
        vol.path(),
        &l,
        &params(0, 9, b"", b"\xff"),
        &[put(50, 10)],
        10,
    )
    .await
    .expect("second life");

    let node = load_node(vol.path(), &l, 0, 0).await.expect("load");
    assert_eq!(node.header().node_seq, 9);
    assert_eq!(node.bset_count(), 1, "only the second life's base bset");
    assert_eq!(
        node.dropped_tail_bsets(),
        0,
        "stale incarnation frames are a clean end, not a counted tear"
    );
    assert_eq!(
        node.lookup(&inode_key(2)).expect("fold"),
        Folded::Absent,
        "first-life records must not resurrect"
    );
    match node.lookup(&inode_key(50)).expect("fold") {
        Folded::Put { seq, .. } => assert_eq!(seq, 10),
        other => panic!("expected the second life's record, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Compact (§4.6 pt 1) — pure function over caller-provided extents.
// ---------------------------------------------------------------------------

/// Compaction folds the whole log into one base bset at a FRESH extent —
/// tombstone elision honors the §4.2 durable-tail rule — and never touches
/// the source extent (CoW).
#[tokio::test]
async fn test_compact_folds_into_fresh_extent_old_untouched() {
    let vol = fresh_volume();
    let l = layout();
    let src_addr = 0u64;
    let dst_addr = DEFAULT_NODE_SIZE as u64;

    let written = write_node(
        vol.path(),
        &l,
        &params(src_addr, 1, b"", b"\xff"),
        &[put(1, 1), put(2, 2), put(3, 3)],
        3,
    )
    .await
    .expect("base");
    let mut dest = AppendDest {
        node_addr: src_addr,
        node_seq: 1,
        tail_offset: written.bytes_written,
    };
    // Δ on ino 1; tombstone ino 2 (seq 5, below the durable tail ⇒ elides);
    // tombstone ino 3 (seq 8, inside the replay window ⇒ survives).
    dest.tail_offset = append_bset(
        vol.path(),
        &l,
        &dest,
        &[
            Record::delta(inode_key(1).to_vec(), 9, &InodeDelta::times(90, 91)),
            Record::delete(inode_key(2).to_vec(), 5),
        ],
        9,
    )
    .await
    .expect("append 1");
    append_bset(
        vol.path(),
        &l,
        &dest,
        &[Record::delete(inode_key(3).to_vec(), 8)],
        9,
    )
    .await
    .expect("append 2");

    let src = load_node(vol.path(), &l, src_addr, 0)
        .await
        .expect("load src");
    let src_image_before = uring_fs::read_at(vol.path(), src_addr, DEFAULT_NODE_SIZE)
        .await
        .expect("src image");

    // Compacting IN PLACE is a protocol violation, refused before any I/O.
    let err = compact_node(vol.path(), &l, &src, &[], src_addr, 2, 6)
        .await
        .expect_err("in-place compaction must be refused");
    assert!(matches!(err, KvError::Corrupt(_)), "got {err:?}");

    let compacted = compact_node(vol.path(), &l, &src, &[], dst_addr, 2, 6)
        .await
        .expect("compact");
    assert_eq!(compacted.node_addr, dst_addr);
    assert_eq!(compacted.node_seq, 2);
    assert_eq!(
        compacted.record_count, 2,
        "folded ino1 Put + surviving ino3 tombstone; ino2 elided"
    );
    assert_eq!(
        compacted.journal_seq_horizon, 9,
        "output horizon = max over source bset horizons"
    );

    // The destination folds identically to the source.
    let dst = load_node(vol.path(), &l, dst_addr, 0)
        .await
        .expect("load dst");
    assert_eq!(dst.bset_count(), 1, "single base bset");
    assert_eq!(dst.header().min_key, src.header().min_key);
    assert_eq!(dst.header().max_key, src.header().max_key);
    let base = dst.bset(0).expect("base");
    assert_eq!(base.record(0).kind, RecordKind::Put);
    assert_eq!(base.record(1).kind, RecordKind::Delete);
    assert_eq!(base.record(1).seq, 8, "replay-window tombstone survives");
    match dst.lookup(&inode_key(1)).expect("fold") {
        Folded::Put { value, seq } => {
            assert_eq!(seq, 9);
            let v = InodeValue::decode(&value).expect("decode");
            assert_eq!((v.mtime, v.ctime), (90, 91));
        }
        other => panic!("expected folded Put, got {other:?}"),
    }
    assert_eq!(dst.lookup(&inode_key(2)).expect("fold"), Folded::Absent);

    // CoW: the source extent is byte-identical and still loads.
    let src_image_after = uring_fs::read_at(vol.path(), src_addr, DEFAULT_NODE_SIZE)
        .await
        .expect("src image after");
    assert_eq!(
        src_image_before, src_image_after,
        "compaction must never write the source extent"
    );
    let src_again = load_node(vol.path(), &l, src_addr, 0)
        .await
        .expect("src loads");
    assert_eq!(src_again.bset_count(), 3);
}

// ---------------------------------------------------------------------------
// Split (§4.6) — two fresh nodes partitioning the folded key space.
// ---------------------------------------------------------------------------

/// A split writes two fresh nodes whose folded record sequences concatenate
/// to exactly the source fold, with correct inclusive key-space bounds and
/// disjoint, ordered key ranges.
#[tokio::test]
async fn test_split_partitions_key_space_into_two_fresh_nodes() {
    let vol = fresh_volume();
    let l = layout();
    let n = DEFAULT_NODE_SIZE as u64;

    let records: Vec<Record> = (0..100u64).map(|i| put(i * 2, i + 1)).collect();
    let written = write_node(vol.path(), &l, &params(0, 1, b"", b"\xff"), &records, 100)
        .await
        .expect("base");
    // One overwrite + one tombstone through the log, so the split input is
    // a real fold, not a copy.
    let dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: written.bytes_written,
    };
    append_bset(
        vol.path(),
        &l,
        &dest,
        &[
            Record::put(inode_key(0).to_vec(), 200, inode_value(200).encode()),
            Record::delete(inode_key(198).to_vec(), 201),
        ],
        201,
    )
    .await
    .expect("append");

    let src = load_node(vol.path(), &l, 0, 0).await.expect("load src");
    let left_dest = SplitDest {
        node_addr: n,
        node_seq: 2,
    };
    let right_dest = SplitDest {
        node_addr: 2 * n,
        node_seq: 3,
    };

    // Destination hygiene: fresh + distinct.
    assert!(
        split_node(vol.path(), &l, &src, &[], &left_dest, &left_dest, 300)
            .await
            .is_err(),
        "left == right must be refused"
    );
    let bad = SplitDest {
        node_addr: 0,
        node_seq: 2,
    };
    assert!(
        split_node(vol.path(), &l, &src, &[], &bad, &right_dest, 300)
            .await
            .is_err(),
        "splitting onto the source extent must be refused"
    );

    let (lw, rw) = split_node(vol.path(), &l, &src, &[], &left_dest, &right_dest, 300)
        .await
        .expect("split");
    assert!(
        lw.record_count >= 1 && rw.record_count >= 1,
        "both sides live"
    );
    assert_eq!(
        lw.record_count + rw.record_count,
        99,
        "100 keys − 1 elided tombstone (durable_tail 300 > seq 201)"
    );

    let left = load_node(vol.path(), &l, n, 0).await.expect("load left");
    let right = load_node(vol.path(), &l, 2 * n, 0)
        .await
        .expect("load right");
    assert_eq!(left.header().node_seq, 2);
    assert_eq!(right.header().node_seq, 3);

    // Reassemble and compare against the source fold.
    let mut keys = Vec::new();
    for (node, side) in [(&left, "left"), (&right, "right")] {
        assert_eq!(node.bset_count(), 1, "{side}: fresh single-base node");
        let b = node.bset(0).expect("base");
        for i in 0..b.len() {
            keys.push(b.record(i).key.to_vec());
        }
    }
    let expected: Vec<Vec<u8>> = (0..99u64).map(|i| inode_key(i * 2).to_vec()).collect();
    assert_eq!(
        keys, expected,
        "concatenation reproduces the fold, in order"
    );

    // Inclusive key-space bounds (§4.6 revalidation contract): the sides
    // PARTITION the source range — right.min = successor(left.max), so no
    // key the parent separators can route is rejected by `min ≤ key ≤ max`
    // revalidation (an unroutable gap would loop the K5 writer retry).
    assert_eq!(left.header().min_key, src.header().min_key);
    assert_eq!(right.header().max_key, src.header().max_key);
    let lb = left.bset(0).expect("base");
    let rb = right.bset(0).expect("base");
    assert_eq!(
        left.header().max_key,
        lb.record(lb.len() - 1).key.to_vec(),
        "left.max = last left key"
    );
    assert_eq!(
        right.header().min_key,
        key_successor(&left.header().max_key),
        "right.min = successor(left.max): gap-free partition"
    );
    assert!(
        right.header().min_key <= rb.record(0).key.to_vec(),
        "right's first key is within its bounds"
    );
    assert!(
        left.header().max_key < right.header().min_key,
        "sides are disjoint and ordered"
    );

    // The overwritten key folded to its newest value on the correct side.
    match left.lookup(&inode_key(0)).expect("fold") {
        Folded::Put { seq, .. } => assert_eq!(seq, 200),
        other => panic!("expected the overwrite to win, got {other:?}"),
    }

    // A node too small to hold two records cannot split (writer-bug guard).
    let tiny_addr = 3 * n;
    write_node(
        vol.path(),
        &l,
        &params(tiny_addr, 8, b"", b"\xff"),
        &[put(1, 1)],
        1,
    )
    .await
    .expect("one-record node");
    let one = load_node(vol.path(), &l, tiny_addr, 0).await.expect("load");
    let err = split_node(
        vol.path(),
        &l,
        &one,
        &[],
        &SplitDest {
            node_addr: 4 * n,
            node_seq: 9,
        },
        &SplitDest {
            node_addr: 5 * n,
            node_seq: 10,
        },
        0,
    )
    .await
    .expect_err("fewer than two folded records cannot split");
    assert!(matches!(err, KvError::Corrupt(_)), "got {err:?}");
}

/// The §4.6 pt 1 escalation, end to end: a node whose log is `NodeFull`
/// compacts together with the frozen dirty delta that no longer fits
/// (`extra_records` — the K5 SMO's successor-build input); when even the
/// fold cannot fit one node, `compact_node` refuses with the typed
/// `NodeFull` split signal and `split_node` absorbs exactly that input.
/// (A node's own log alone can never overflow — folding only reclaims
/// append padding — so the signal is reachable exactly this way.)
#[tokio::test]
async fn test_compact_overflow_signals_split() {
    let vol = fresh_volume();
    let small = NodeLayout::new(MIN_NODE_SIZE).expect("64 KiB layout");

    // Fill the 64 KiB node's log completely: 15 appends × one ~4 KiB-value
    // record (each padded frame = 2 pages) would overshoot — use 15 one-page
    // frames with ~2 KiB values, distinct keys: the log is full at ~34 KiB
    // of live payload.
    write_node(vol.path(), &small, &params(0, 1, b"", b"\xff"), &[], 0)
        .await
        .expect("header-only");
    let mut dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: NODE_PAGE,
    };
    for i in 0..15u64 {
        let rec = Record::put(inode_key(i).to_vec(), i + 1, vec![0x11; 2048]);
        dest.tail_offset = append_bset(vol.path(), &small, &dest, &[rec], i + 1)
            .await
            .expect("append");
    }
    let src = load_node(vol.path(), &small, 0, 0).await.expect("load");
    assert_eq!(src.bset_count(), 15);
    assert_eq!(src.tail_offset(), MIN_NODE_SIZE, "log full");

    // The frozen delta that no longer fits the log: 8 more distinct-keyed
    // ~4 KiB-value records. Fold = 15 × ~2 KiB + 8 × ~4 KiB ≈ 64 KiB of
    // records > 64 KiB − 4 KiB header − bset overhead ⇒ cannot fit.
    let extra: Vec<Record> = (100..108u64)
        .map(|i| Record::put(inode_key(i).to_vec(), i, vec![0x22; 4096]))
        .collect();
    let err = compact_node(vol.path(), &small, &src, &extra, MIN_NODE_SIZE as u64, 2, 0)
        .await
        .expect_err("oversized fold must not silently write");
    assert!(
        matches!(err, KvError::NodeFull { .. }),
        "compact overflow is the typed split signal, got {err:?}"
    );

    // Without the frozen delta the same log compacts fine (padding reclaim).
    let alone = compact_node(vol.path(), &small, &src, &[], MIN_NODE_SIZE as u64, 2, 0)
        .await
        .expect("a node's own log always compacts");
    assert_eq!(alone.record_count, 15);

    // And the split of the oversized fold succeeds, preserving every record.
    let (lw, rw) = split_node(
        vol.path(),
        &small,
        &src,
        &extra,
        &SplitDest {
            node_addr: 2 * MIN_NODE_SIZE as u64,
            node_seq: 3,
        },
        &SplitDest {
            node_addr: 3 * MIN_NODE_SIZE as u64,
            node_seq: 4,
        },
        0,
    )
    .await
    .expect("split absorbs the oversized fold");
    assert_eq!(lw.record_count + rw.record_count, 15 + 8);
}
