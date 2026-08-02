//! Crash-contract tests for the v3 CoW KV metadata format
//! (design-cow-kv-metadata §4.5/§4.10) over the deterministic
//! fault-injection shim in `uring_fs` (`nvme_dev.rs` precedent):
//!
//! - **Torn write**: the first write intersecting an armed offset persists
//!   only a prefix, the caller sees `EIO`, and the path is poisoned ("device
//!   died mid-commit") until `clear_faults()`.
//! - **Power cut**: writes since the last `fdatasync` on an armed path are
//!   reverted by `power_cut()` — modeling volatile-cache loss, which neither
//!   kill-9 nor process exit can produce on file-backed volumes.
//!
//! (The v2 D0/D1 sector-contract cases this suite grew out of — torn
//! sector applies, the v2 strict/deferred flush barriers, journal-region
//! hygiene — were deleted with v2 support; the v3 strict/deferred
//! equivalents live in `kv_backend_tests.rs` and the checkpoint suite.)
//!
//! The `PR K2` section covers the v3 CoW KV node
//! format (design-cow-kv-metadata §4.5/§4.10): torn tail bsets, the loud
//! positional valid-bset-after-tear classifier, and torn rewrites.
//!
//! The trailing `PR K3` section extends it to the v3 journal ring and root
//! ledger (design-cow-kv-metadata §4.1/§4.6 pt 3/§4.10): torn entries, torn
//! page headers, garbage lengths, holes, torn multi-page middles, torn
//! ledger slots, and the ring-reuse-never-overwrites-the-fallback-window
//! invariant — everything inside the replay window recovers and resyncs,
//! never failing a mount loud.
//!
//! The trailing `PR K4` section extends it to the v3 extent allocator
//! (design-cow-kv-metadata §4.7/§4.10): torn A/B bitmap slots (newest
//! tears ⇒ the predecessor slot + journal replay reconstruct), and the
//! risk-R3 case — tear the newest root-ledger record after
//! compaction-heavy reuse churn ⇒ the predecessor's extents are
//! byte-intact (pending-free never released them before the retiring
//! checkpoint was durable), nothing double-allocated, and the full
//! allocator state is reconstructible from the predecessor + journal.

use squeezefs::uring_fs;
use tempfile::NamedTempFile;

use squeezefs::meta_backend::kv::node::{
    append_bset, compact_node, encode_bset_frame, load_node, split_node, write_node, AppendDest,
    NodeLayout, NodeWriteParams, SplitDest, DEFAULT_NODE_SIZE,
};
use squeezefs::meta_backend::kv::record::{inode_key, Folded, InodeValue, Record, TREE_INODES};
use squeezefs::meta_backend::kv::{KvError, META_KV_NODE_DROPPED_TAIL_BSETS};

use squeezefs::meta_backend::kv::checkpoint::{
    read_newest_ledger, write_ledger_slot, LedgerRecord, TreeRoot, ROOT_LEDGER_SLOTS,
    ROOT_LEDGER_SLOT_LEN,
};
use squeezefs::meta_backend::kv::journal::{
    entry_len_for, JournalRing, ENTRY_HDR_LEN, JOURNAL_PAGE_DATA_LEN, JOURNAL_PAGE_HDR_LEN,
    JOURNAL_PAGE_LEN,
};
use squeezefs::meta_backend::kv::journal_core::{AdmissionClass, Reservation};

use squeezefs::meta_backend::kv::alloc_ext::{
    alloc_record, bitmap_region_len, decode_bitmap_page, free_record, ExtentAllocator,
    ALLOC_PAGE_LEN,
};

/// RAII: faults never leak across tests (the shim state is process-global
/// and the suite runs `--test-threads=1`).
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        uring_fs::clear_faults();
    }
}

// ---------------------------------------------------------------------------
// Shim self-tests: the harness must be trustworthy before it can pin
// anything.
// ---------------------------------------------------------------------------

/// A torn write persists exactly the requested prefix, the caller sees EIO,
/// and the path is dead (reads AND writes fail) until `clear_faults()`.
#[tokio::test]
async fn test_torn_write_poisons_path_until_cleared() {
    let tmp = NamedTempFile::new().unwrap();
    tmp.as_file().set_len(96 * 1024 * 1024).unwrap();
    let _g = FaultGuard;

    // A fresh (all-zero) offset.
    let target = 1024 * 1024 * 64;
    let image = vec![0xABu8; 4096];
    uring_fs::arm_torn_write(target, 100);

    let err = uring_fs::write_at(tmp.path(), target, image.clone())
        .await
        .expect_err("torn write must report EIO to the caller");
    assert_eq!(err.to_errno(), libc::EIO, "torn write maps to EIO: {err}");

    // Device died: subsequent I/O on the same path fails…
    assert!(
        uring_fs::read_at(tmp.path(), target, 4096).await.is_err(),
        "reads on a poisoned path must fail"
    );
    assert!(
        uring_fs::write_at(tmp.path(), target, image).await.is_err(),
        "writes on a poisoned path must fail"
    );

    // …until the fault is cleared ("device replaced / remount").
    uring_fs::clear_faults();
    let buf = uring_fs::read_at(tmp.path(), target, 4096)
        .await
        .expect("cleared path must serve reads again");
    assert_eq!(&buf[..100], &[0xABu8; 100][..], "torn prefix must persist");
    assert_eq!(
        &buf[100..],
        &[0u8; 4096 - 100][..],
        "bytes past the tear must never land"
    );
}

/// `power_cut` reverts exactly the writes since the last fdatasync: unsynced
/// bytes vanish, fdatasync'd bytes survive.
#[tokio::test]
async fn test_power_cut_reverts_unsynced_writes() {
    let tmp = NamedTempFile::new().unwrap();
    tmp.as_file().set_len(96 * 1024 * 1024).unwrap();
    let _g = FaultGuard;

    let target = 1024 * 1024 * 64;
    uring_fs::arm_power_cut(tmp.path());

    // Unsynced write: lost at the cut.
    uring_fs::write_at(tmp.path(), target, vec![0x11u8; 4096])
        .await
        .unwrap();
    let reverted = uring_fs::power_cut(tmp.path());
    assert!(reverted >= 1, "the unsynced write must be reverted");
    let buf = uring_fs::read_at(tmp.path(), target, 4096).await.unwrap();
    assert_eq!(
        &buf[..],
        &[0u8; 4096][..],
        "unsynced bytes must not survive a power cut"
    );

    // Synced write: survives the cut.
    uring_fs::arm_power_cut(tmp.path());
    uring_fs::write_at(tmp.path(), target, vec![0x22u8; 4096])
        .await
        .unwrap();
    uring_fs::fdatasync(tmp.path().to_path_buf()).await.unwrap();
    let _ = uring_fs::power_cut(tmp.path());
    let buf = uring_fs::read_at(tmp.path(), target, 4096).await.unwrap();
    assert_eq!(
        &buf[..],
        &[0x22u8; 4096][..],
        "fdatasync'd bytes must survive a power cut"
    );
}

// ---------------------------------------------------------------------------
// PR K2: the v3 CoW node crash contract (design-cow-kv-metadata §4.5/§4.10).
//
// Node appends mutate only never-written bytes, so a torn append can only
// damage data that was never live: dropped + counted, prior bsets intact.
// The §4.5 positional classifier turns exactly one shape LOUD — a valid
// same-incarnation bset beyond the tear whose journal_seq_horizon is ≤ the
// durable tail (checkpoint-covered data cannot legitimately follow a torn
// append; the §4.6 barrier covered every earlier one). Rewrites go to fresh
// extents, never in place, so a torn rewrite is unreferenced garbage and
// the old node serves. `TORN_WRITE_FAULT` is reused for tear injection.
// ---------------------------------------------------------------------------

const KV_VOL_SIZE: u64 = 8 * 1024 * 1024;

/// Fresh fixed-size file volume for injected KV node extents (no v2
/// geometry needed — the node layer is pure over caller-provided extents).
fn fresh_kv_volume() -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    tmp.as_file().set_len(KV_VOL_SIZE).unwrap();
    tmp
}

fn kv_layout() -> NodeLayout {
    NodeLayout::new(DEFAULT_NODE_SIZE).expect("default layout")
}

fn kv_put(ino: u64, seq: u64) -> Record {
    let v = InodeValue {
        mode: 0o100644,
        uid: seq as u32,
        nlink: 1,
        mtime: seq,
        ctime: seq,
        ..Default::default()
    };
    Record::put(inode_key(ino).to_vec(), seq, v.encode())
}

fn kv_params<'a>(addr: u64, seq: u64) -> NodeWriteParams<'a> {
    NodeWriteParams {
        node_addr: addr,
        node_seq: seq,
        tree_id: TREE_INODES,
        level: 0,
        min_key: b"",
        max_key: b"\xff",
    }
}

fn dropped_counter() -> u64 {
    META_KV_NODE_DROPPED_TAIL_BSETS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build `base + one appended bset` at extent 0 and return the tail offset
/// where the next append lands.
async fn kv_node_with_one_append(vol: &NamedTempFile, l: &NodeLayout) -> usize {
    let written = write_node(
        vol.path(),
        l,
        &kv_params(0, 1),
        &[kv_put(1, 1), kv_put(2, 2)],
        2,
    )
    .await
    .expect("base");
    let dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: written.bytes_written,
    };
    append_bset(vol.path(), l, &dest, &[kv_put(3, 18)], 20)
        .await
        .expect("append 1")
}

/// K2 crash case (a): a torn tail-bset append is EIO to the writer; on
/// reload the torn bset is dropped **and counted**
/// (`meta_kv_node_dropped_tail_bsets`), every prior bset is intact, and the
/// torn records were never live. A sub-frame-header tear (nothing
/// identifiable landed) is indistinguishable from a clean end: dropped,
/// uncounted, priors intact — the counter is the *clean-unmount* corruption
/// alert, not a tear census (§4.5).
#[tokio::test]
async fn test_kv_node_torn_tail_bset_dropped_counted_priors_intact() {
    let vol = fresh_kv_volume();
    let _g = FaultGuard;
    let l = kv_layout();
    let tail = kv_node_with_one_append(&vol, &l).await;

    // Tear the next append 64 bytes in: the 32 B frame header lands (this
    // incarnation's stamp), the bset image tears.
    uring_fs::arm_torn_write(tail as u64, 64);
    let dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: tail,
    };
    let err = append_bset(vol.path(), &l, &dest, &[kv_put(4, 22)], 30)
        .await
        .expect_err("torn append must report the device error");
    match err {
        KvError::Io(inner) => assert_eq!(inner.to_errno(), libc::EIO),
        other => panic!("expected KvError::Io(EIO), got {other:?}"),
    }
    uring_fs::clear_faults();

    // Reload with the torn bset inside the replay window (horizon 30 > the
    // durable tail 15): expected power-loss artifact — silent drop, counted.
    let before = dropped_counter();
    let node = load_node(vol.path(), &l, 0, 15)
        .await
        .expect("a torn un-checkpointed tail must never fail the load");
    assert_eq!(node.bset_count(), 2, "base + append 1 intact");
    assert_eq!(node.dropped_tail_bsets(), 1, "the torn bset is counted");
    assert_eq!(dropped_counter() - before, 1, "global counter mirrors it");
    assert_eq!(
        node.tail_offset(),
        tail,
        "the truncated view ends where the tear began"
    );
    match node.lookup(&inode_key(3)).expect("fold") {
        Folded::Put { seq, .. } => assert_eq!(seq, 18, "prior append intact"),
        other => panic!("expected prior append's record, got {other:?}"),
    }
    assert_eq!(
        node.lookup(&inode_key(4)).expect("fold"),
        Folded::Absent,
        "the torn record was never live"
    );

    // Sub-header tear on a second node: not even the frame magic lands ⇒
    // indistinguishable from a clean end. Dropped, uncounted, priors intact.
    let addr2 = DEFAULT_NODE_SIZE as u64;
    let w2 = write_node(vol.path(), &l, &kv_params(addr2, 5), &[kv_put(7, 7)], 7)
        .await
        .expect("second node");
    uring_fs::arm_torn_write(addr2 + w2.bytes_written as u64, 2);
    let dest2 = AppendDest {
        node_addr: addr2,
        node_seq: 5,
        tail_offset: w2.bytes_written,
    };
    append_bset(vol.path(), &l, &dest2, &[kv_put(8, 9)], 9)
        .await
        .expect_err("torn append must fail");
    uring_fs::clear_faults();
    let before = dropped_counter();
    let node2 = load_node(vol.path(), &l, addr2, 0).await.expect("load");
    assert_eq!(node2.bset_count(), 1, "prior base intact");
    assert_eq!(node2.dropped_tail_bsets(), 0, "nothing identifiable landed");
    assert_eq!(dropped_counter(), before);
}

/// K2 crash case (b): the §4.5 **positional** classifier. A torn bset's own
/// bytes — including its landed `journal_seq_horizon` field — are garbage
/// and never branched on; classification uses only checksum-verified bsets
/// beyond the tear. A valid same-incarnation bset with horizon ≤ the
/// durable tail ⇒ LOUD typed failure (checkpoint-covered data after a tear
/// is impossible under the §4.6 barrier); horizon > tail ⇒ silent
/// replay-window drop; a stale-incarnation frame (recycled extent) never
/// trips it.
#[tokio::test]
async fn test_kv_node_valid_bset_after_tear_horizon_le_tail_fails_loud() {
    let vol = fresh_kv_volume();
    let _g = FaultGuard;
    let l = kv_layout();
    let tail = kv_node_with_one_append(&vol, &l).await;
    let durable_tail = 15u64;

    // Torn append whose OWN horizon field (1 ≤ 15) physically lands: keep
    // 64 covers the 32 B frame header AND the 32 B bset header. If the
    // classifier branched on those torn bytes it would fail loud here.
    uring_fs::arm_torn_write(tail as u64, 64);
    let dest = AppendDest {
        node_addr: 0,
        node_seq: 1,
        tail_offset: tail,
    };
    append_bset(vol.path(), &l, &dest, &[kv_put(4, 1)], 1)
        .await
        .expect_err("torn append must fail");
    uring_fs::clear_faults();
    let node = load_node(vol.path(), &l, 0, durable_tail)
        .await
        .expect("a torn bset's own horizon must never be branched on");
    assert_eq!(node.dropped_tail_bsets(), 1);

    // A valid same-incarnation bset beyond the tear, horizon 16 > tail 15:
    // still the replay window — silent drop, counted, never loud.
    let beyond = tail + squeezefs::meta_backend::kv::node::NODE_PAGE;
    let replay_window_frame = encode_bset_frame(&l, 1, &[kv_put(9, 16)], 16).expect("forge frame");
    uring_fs::write_at(vol.path(), beyond as u64, replay_window_frame)
        .await
        .expect("plant");
    let node = load_node(vol.path(), &l, 0, durable_tail)
        .await
        .expect("replay-window bsets beyond a tear drop silently");
    assert_eq!(
        node.dropped_tail_bsets(),
        2,
        "the torn unit and the unreachable replay-window bset both count"
    );
    assert_eq!(node.bset_count(), 2, "the view still truncates at the tear");

    // A stale-incarnation frame (node_seq 999 ≠ 1) with horizon ≤ tail:
    // recycled-extent garbage, structurally expected — never loud. The
    // higher-stamp direction is deliberate: dead-generation residue after
    // a quick reformat can out-number the live uuid-derived seq base and
    // must stay silently buried (see FrameProbe::StaleIncarnation and
    // tests/kv_finding_a_tests.rs).
    let stale = encode_bset_frame(&l, 999, &[kv_put(10, 5)], 5).expect("forge stale");
    let stale_off = beyond + squeezefs::meta_backend::kv::node::NODE_PAGE;
    uring_fs::write_at(vol.path(), stale_off as u64, stale)
        .await
        .expect("plant stale");
    load_node(vol.path(), &l, 0, durable_tail)
        .await
        .expect("stale-incarnation frames never trip the classifier");

    // The loud shape: a valid SAME-incarnation bset beyond the tear with
    // horizon 5 ≤ tail 15 ⇒ checkpoint-covered data follows a tear ⇒ the
    // node must fail loud with the typed positional error.
    let violation = encode_bset_frame(&l, 1, &[kv_put(9, 5)], 5).expect("forge violation");
    uring_fs::write_at(vol.path(), beyond as u64, violation)
        .await
        .expect("plant violation");
    let err = load_node(vol.path(), &l, 0, durable_tail)
        .await
        .expect_err("checkpoint-covered bset after a tear must fail LOUD");
    match err {
        KvError::CheckpointCoveredBsetAfterTear {
            node_addr,
            bset_offset,
            horizon,
            durable_tail: t,
        } => {
            assert_eq!(node_addr, 0);
            assert_eq!(bset_offset, beyond);
            assert_eq!(horizon, 5);
            assert_eq!(t, durable_tail);
        }
        other => panic!("expected CheckpointCoveredBsetAfterTear, got {other:?}"),
    }

    // Boundary: horizon == tail is still "≤" — still loud (§4.5).
    let boundary = encode_bset_frame(&l, 1, &[kv_put(9, 15)], durable_tail).expect("forge");
    uring_fs::write_at(vol.path(), beyond as u64, boundary)
        .await
        .expect("plant boundary");
    let err = load_node(vol.path(), &l, 0, durable_tail)
        .await
        .expect_err("horizon == durable tail must stay loud");
    assert!(
        matches!(
            err,
            KvError::CheckpointCoveredBsetAfterTear { horizon: 15, .. }
        ),
        "got {err:?}"
    );
}

/// K2 crash case (c): a torn rewrite (compact/split) targets a FRESH extent
/// — never in place — so the tear leaves unreferenced garbage: the write
/// errors, the destination fails to load, and the source node is
/// byte-identical and fully serving (§4.1/§4.10 "torn rewrite ⇒
/// unreferenced, old node serves").
#[tokio::test]
async fn test_kv_node_torn_rewrite_unreferenced_old_node_untouched() {
    let vol = fresh_kv_volume();
    let _g = FaultGuard;
    let l = kv_layout();
    kv_node_with_one_append(&vol, &l).await;
    let src = load_node(vol.path(), &l, 0, 0).await.expect("load src");
    let image_before = uring_fs::read_at(vol.path(), 0, DEFAULT_NODE_SIZE)
        .await
        .expect("src image");

    // Torn compaction: 20 bytes of the destination header page land — the
    // header checksum field never does.
    let dst = DEFAULT_NODE_SIZE as u64;
    uring_fs::arm_torn_write(dst, 20);
    let err = compact_node(vol.path(), &l, &src, &[], dst, 2, 0)
        .await
        .expect_err("torn rewrite must report the device error");
    match err {
        KvError::Io(inner) => assert_eq!(inner.to_errno(), libc::EIO),
        other => panic!("expected KvError::Io(EIO), got {other:?}"),
    }
    uring_fs::clear_faults();

    load_node(vol.path(), &l, dst, 0)
        .await
        .expect_err("the torn destination must never verify");
    let image_after = uring_fs::read_at(vol.path(), 0, DEFAULT_NODE_SIZE)
        .await
        .expect("src image after");
    assert_eq!(
        image_before, image_after,
        "a rewrite must never write the source extent"
    );
    let src_again = load_node(vol.path(), &l, 0, 0)
        .await
        .expect("old node serves");
    assert_eq!(src_again.bset_count(), 2);
    match src_again.lookup(&inode_key(3)).expect("fold") {
        Folded::Put { seq, .. } => assert_eq!(seq, 18),
        other => panic!("expected the source record, got {other:?}"),
    }

    // Torn split: tear the LEFT destination; the fault poisons the path so
    // the right write fails cleanly too — either way both destinations are
    // unreferenced and the source is untouched.
    uring_fs::arm_torn_write(2 * dst, 20);
    let err = split_node(
        vol.path(),
        &l,
        &src_again,
        &[],
        &SplitDest {
            node_addr: 2 * dst,
            node_seq: 3,
        },
        &SplitDest {
            node_addr: 3 * dst,
            node_seq: 4,
        },
        0,
    )
    .await
    .expect_err("torn split must fail");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    load_node(vol.path(), &l, 2 * dst, 0)
        .await
        .expect_err("torn left destination must never verify");
    let final_image = uring_fs::read_at(vol.path(), 0, DEFAULT_NODE_SIZE)
        .await
        .expect("src image final");
    assert_eq!(image_before, final_image, "source still byte-identical");
    load_node(vol.path(), &l, 0, 0)
        .await
        .expect("old node still serves after the torn split");
}

// ---------------------------------------------------------------------------
// PR K3: the v3 journal ring + root ledger crash contract
// (design-cow-kv-metadata §4.1, §4.6 pt 3, §4.10).
//
// The ring's replay window [tail, tail + capacity) is by definition the
// maybe-torn region: a tear there is a legitimate power-loss artifact, so
// NOTHING in the ring ever fails a mount loud — every case below must
// recover-and-resync (`recover` returns Ok), with whole-entry atomicity
// (a damaged multi-page entry drops entirely; no partial records) and
// drop-and-resync accounting (`dropped_torn` counts confirmed mid-log
// damage, and stays zero for trailing/end-of-log artifacts so a clean
// unmount reports zero — the §10 corruption alert).
//
// Tear injection: the in-flight shim (`arm_torn_write`, the honest
// died-mid-commit model) for last-write tears, and post-hoc byte damage
// via `uring_fs::write_at` for mid-log tears — the §4.10 "torn + reordered
// pages" power-loss reality, where a LATER entry's pages persisted while
// an EARLIER write's did not (unordered page-cache writeback).
// ---------------------------------------------------------------------------

/// Zero-filled ring file: `pages` × 4 KiB at offset 0.
fn kv_ring_file(pages: u64) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    tmp.as_file().set_len(pages * JOURNAL_PAGE_LEN).unwrap();
    tmp
}

/// Entry overhead of a one-record (inode Put) journal entry.
const KV_ENTRY_OVERHEAD: u64 = ENTRY_HDR_LEN + 1 + 15 + 8;

/// Staged records for an entry of exactly `entry_len` bytes.
fn kv_sized_records(ino: u64, entry_len: u64, marker: u8, seq: u64) -> Vec<(u8, Record)> {
    let value = vec![marker; (entry_len - KV_ENTRY_OVERHEAD) as usize];
    vec![(
        TREE_INODES,
        Record::put(inode_key(ino).to_vec(), seq, value),
    )]
}

/// Admit → reserve → write an entry of exactly `entry_len` bytes.
async fn kv_append(ring: &JournalRing, ino: u64, entry_len: u64, marker: u8) -> Reservation {
    let probe = kv_sized_records(ino, entry_len, marker, 0);
    let need = entry_len_for(&probe).expect("entry under cap");
    assert_eq!(need, entry_len);
    let adm = ring
        .core()
        .try_admit(need, AdmissionClass::User)
        .expect("test ring must have room");
    let res = ring.core().reserve(adm);
    ring.write_entry(&res, &kv_sized_records(ino, entry_len, marker, res.seq()))
        .await
        .expect("clean entry write");
    res
}

/// Physical file offset of logical ring position `pos` (ring base 0).
fn kv_phys(ring: &JournalRing, pos: u64) -> u64 {
    let geo = ring.core().geometry();
    geo.page_index(pos) * JOURNAL_PAGE_LEN + JOURNAL_PAGE_HDR_LEN + geo.in_page_off(pos)
}

/// Post-hoc damage: overwrite `len` bytes at physical offset `off` with
/// `0x5A` garbage via io_uring (the unordered-writeback tear model).
async fn kv_smash(path: &std::path::Path, off: u64, len: usize) {
    uring_fs::write_at(path, off, vec![0x5Au8; len])
        .await
        .expect("fault injection write");
}

/// The recovered inos, in replay order (each test entry carries one Put).
fn kv_recovered_inos(rec: &squeezefs::meta_backend::kv::journal::JournalRecovery) -> Vec<u64> {
    rec.entries
        .iter()
        .flat_map(|e| e.records.iter())
        .map(|(_, r)| {
            squeezefs::meta_backend::kv::record::decode_inode_key(&r.key).expect("inode key")
        })
        .collect()
}

/// K3 crash case (a): an in-flight tear on the LAST entry — the classic
/// died-mid-commit. The writer sees EIO (D-level: the tx never returned);
/// after "remount", every prior entry replays, the torn trailing entry is
/// gone, and — because the tear is trailing, indistinguishable from the
/// ordinary end-of-log — `dropped_torn` stays 0 (§4.1 accounting: the
/// counter is the clean-unmount corruption alert, not a tear census).
#[tokio::test]
async fn test_kv_journal_torn_last_entry_recovers_prefix_not_loud() {
    let f = kv_ring_file(4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);
    let _g = FaultGuard;

    let _a = kv_append(&ring, 1, 1000, 0xA1).await;
    let b = kv_append(&ring, 2, 1000, 0xA2).await;

    // The third entry tears 10 bytes in: its seq bytes land, nothing else.
    let probe = kv_sized_records(3, 1000, 0xA3, 0);
    let need = entry_len_for(&probe).unwrap();
    let adm = ring.core().try_admit(need, AdmissionClass::User).unwrap();
    let res = ring.core().reserve(adm);
    uring_fs::arm_torn_write(kv_phys(&ring, res.start), 10);
    let err = ring
        .write_entry(&res, &kv_sized_records(3, 1000, 0xA3, res.seq()))
        .await
        .expect_err("torn entry write must fail loud to the writer");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0)
        .await
        .expect("ring contents never fail a mount loud");
    assert_eq!(kv_recovered_inos(&recovery), vec![1, 2]);
    assert_eq!(
        recovery.head_pos,
        b.end(),
        "the recovered head resumes before the torn trailing entry"
    );
    assert_eq!(
        recovery.dropped_torn, 0,
        "a trailing tear is end-of-log, not a counted drop"
    );
}

/// K3 crash case (b): a torn entry mid-log (its pages lost while later
/// entries' pages persisted — unordered writeback). The damaged entry
/// drops, replay resynchronizes at the next verifiable page header, later
/// entries are recovered, and the drop is counted (confirmed by the
/// recovery downstream).
#[tokio::test]
async fn test_kv_journal_torn_entry_mid_log_drops_and_resyncs() {
    let f = kv_ring_file(4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    // A [0, 2000), B [2000, 4072) — page 0; C [4072, 6072) — page 1;
    // D [6072, 8072) — page 1.
    let _a = kv_append(&ring, 1, 2000, 0xB1).await;
    let b = kv_append(&ring, 2, JOURNAL_PAGE_DATA_LEN - 2000, 0xB2).await;
    let _c = kv_append(&ring, 3, 2000, 0xB3).await;
    let _d = kv_append(&ring, 4, 2000, 0xB4).await;

    // B's payload bytes are damaged in place.
    kv_smash(f.path(), kv_phys(&ring, b.start + 100), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![1, 3, 4],
        "B drops; the chain resyncs at page 1 and recovers C and D"
    );
    assert_eq!(
        recovery.dropped_torn, 1,
        "one confirmed drop-and-resync event (B)"
    );
}

/// K3 crash case (c): a torn page HEADER is never loud (§4.1). With the
/// entry chain intact, an entry *continuing* through the dead-header page
/// is still read at chain-known offsets and everything replays (variant
/// a). With the chain also broken, entries *starting* in the dead page are
/// lost, the scanner resyncs at the next verified header, and later
/// entries are recovered (variant b).
#[tokio::test]
async fn test_kv_journal_torn_page_header_recovers_never_loud() {
    // Variant (a): chain-continuation THROUGH a dead-header page.
    // A [0, 9000) spans pages 0..2; B [9000, 11000) in page 2; C [11000,
    // 13000) crosses into page 3.
    {
        let f = kv_ring_file(8);
        let ring = JournalRing::new(f.path(), 0, 8, 0);
        let _a = kv_append(&ring, 1, 9000, 0xC1).await;
        let _b = kv_append(&ring, 2, 2000, 0xC2).await;
        let _c = kv_append(&ring, 3, 2000, 0xC3).await;

        // Kill page 1's header: A's continuation bytes there are untouched.
        kv_smash(f.path(), JOURNAL_PAGE_LEN, JOURNAL_PAGE_HDR_LEN as usize).await;

        let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
            .await
            .expect("a torn page header must never fail the mount loud");
        assert_eq!(
            kv_recovered_inos(&recovery),
            vec![1, 2, 3],
            "the chain reads straight through the dead header (§4.1)"
        );
        assert_eq!(recovery.dropped_torn, 0, "nothing was lost");
    }

    // Variant (b): the same layout with A's page-0 bytes ALSO damaged: the
    // chain breaks at A, page 1 cannot host discovery (dead header), and
    // replay resyncs at page 2 — B and C recovered, A lost, both damage
    // sites counted (confirmed by B's recovery).
    {
        let f = kv_ring_file(8);
        let ring = JournalRing::new(f.path(), 0, 8, 0);
        let _a = kv_append(&ring, 1, 9000, 0xC4).await;
        let _b = kv_append(&ring, 2, 2000, 0xC5).await;
        let _c = kv_append(&ring, 3, 2000, 0xC6).await;

        kv_smash(f.path(), JOURNAL_PAGE_LEN, JOURNAL_PAGE_HDR_LEN as usize).await;
        kv_smash(f.path(), kv_phys(&ring, 200), 16).await; // A's payload

        let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
            .await
            .expect("never loud");
        assert_eq!(
            kv_recovered_inos(&recovery),
            vec![2, 3],
            "entries starting in/behind the dead region die; resync recovers B, C"
        );
        assert_eq!(
            recovery.dropped_torn, 2,
            "two confirmed damage events: A's torn entry + page 1's dead header"
        );
    }
}

/// K3 crash case (d): a garbage `len` probe — both over the 128 KiB cap
/// and under-cap-but-overrunning-the-window — is dropped and resynced
/// WITHOUT the length ever being dereferenced (§4.1/§9: the bound is
/// enforced before any byte it governs is read). Later entries recover.
#[tokio::test]
async fn test_kv_journal_garbage_len_never_dereferenced() {
    // len > cap.
    {
        let f = kv_ring_file(4);
        let ring = JournalRing::new(f.path(), 0, 4, 0);
        // A [0, 1000), B [1000, 4072) — page 0; C [4072, ...) — page 1.
        let _a = kv_append(&ring, 1, 1000, 0xD1).await;
        let b = kv_append(&ring, 2, JOURNAL_PAGE_DATA_LEN - 1000, 0xD2).await;
        let _c = kv_append(&ring, 3, 1000, 0xD3).await;

        // B's len field (logical bytes 8..12 of the entry) → 0xFFFFFFFF.
        uring_fs::write_at(
            f.path(),
            kv_phys(&ring, b.start + 8),
            vec![0xFF, 0xFF, 0xFF, 0xFF],
        )
        .await
        .unwrap();

        let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0)
            .await
            .expect("a garbage len must never be dereferenced, let alone be loud");
        assert_eq!(kv_recovered_inos(&recovery), vec![1, 3]);
        assert_eq!(recovery.dropped_torn, 1);
    }

    // len ≤ cap but overrunning the replay window.
    {
        let f = kv_ring_file(4);
        let ring = JournalRing::new(f.path(), 0, 4, 0);
        let _a = kv_append(&ring, 1, 1000, 0xD4).await;
        let b = kv_append(&ring, 2, JOURNAL_PAGE_DATA_LEN - 1000, 0xD5).await;
        let _c = kv_append(&ring, 3, 1000, 0xD6).await;

        // 100,000 < the cap, but far past the 4-page window.
        uring_fs::write_at(
            f.path(),
            kv_phys(&ring, b.start + 8),
            100_000u32.to_le_bytes().to_vec(),
        )
        .await
        .unwrap();

        let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0)
            .await
            .expect("never loud");
        assert_eq!(kv_recovered_inos(&recovery), vec![1, 3]);
        assert_eq!(recovery.dropped_torn, 1);
    }
}

/// K3 crash case (e): a hole — one entry's whole page (header and all)
/// never landed while its neighbors' pages did (§4.10 caveat (a) made
/// concrete). Replay applies the entries around the hole and counts one
/// confirmed drop.
#[tokio::test]
async fn test_kv_journal_hole_then_resync_recovers_later_entries() {
    let f = kv_ring_file(4);
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    // A fills page 0 exactly; B fills page 1 exactly; C [8144, 10144) in
    // page 2.
    let _a = kv_append(&ring, 1, JOURNAL_PAGE_DATA_LEN, 0xE1).await;
    let _b = kv_append(&ring, 2, JOURNAL_PAGE_DATA_LEN, 0xE2).await;
    let _c = kv_append(&ring, 3, 2000, 0xE3).await;

    // B's page (page 1) never made it to the device: zero the whole page.
    uring_fs::write_at(
        f.path(),
        JOURNAL_PAGE_LEN,
        vec![0u8; JOURNAL_PAGE_LEN as usize],
    )
    .await
    .unwrap();

    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, 0)
        .await
        .expect("a hole must never fail the mount loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![1, 3],
        "entry N torn, N+1 intact in later pages ⇒ replay applies N+1 without N (§4.10)"
    );
    assert_eq!(recovery.dropped_torn, 1);
}

/// K3 crash case (f): a torn MIDDLE page of a multi-page entry ⇒ the whole
/// entry drops (one entry = one atomicity unit — no partial records can
/// ever be replayed), and entries in later intact pages are recovered
/// through the continuation pages' `first_entry_off` chain (§4.1).
#[tokio::test]
async fn test_kv_journal_torn_middle_page_of_multipage_entry() {
    let f = kv_ring_file(8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);

    // E [0, 9000) spans pages 0,1,2; F [9000, 11000) in page 2;
    // G [11000, 13000) crosses pages 2→3.
    let e = kv_append(&ring, 1, 9000, 0xF1).await;
    let _f2 = kv_append(&ring, 2, 2000, 0xF2).await;
    let _g = kv_append(&ring, 3, 2000, 0xF3).await;

    // Damage E's continuation bytes in its MIDDLE page (page 1).
    kv_smash(f.path(), kv_phys(&ring, e.start + 5000), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![2, 3],
        "E drops whole (no partial records); F and G are recovered"
    );
    assert!(
        recovery.entries.iter().all(|en| en.seq != e.seq()),
        "no fragment of the torn multi-page entry may replay"
    );
    assert_eq!(recovery.dropped_torn, 1, "one confirmed drop (E)");
}

fn kv_ledger_rec(seq: u64) -> LedgerRecord {
    LedgerRecord {
        seq,
        tree_roots: vec![TreeRoot {
            tree_id: TREE_INODES,
            node_addr: 0x40000 * seq,
            node_seq: seq,
        }],
        journal_tail_seq: 100 * seq,
        next_ino: 2 + seq,
        alloc_bitmap_generation: seq,
        node_seq_watermark: seq,
        membership_stamp: None,
    }
}

/// K3 crash case (g): a torn newest ledger slot ⇒ mount selects the
/// predecessor (§4.1 newest-valid-wins); a second corrupted slot falls
/// back one more. Ledger contents never fail the read loud.
#[tokio::test]
async fn test_kv_ledger_torn_slot_falls_back_to_predecessor() {
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();
    let _g = FaultGuard;

    for seq in 1..=4 {
        write_ledger_slot(f.path(), 0, &kv_ledger_rec(seq))
            .await
            .unwrap();
    }

    // Checkpoint 5 races power loss: its slot write tears mid-record —
    // inside the header's checksum field (byte 20 of 24), so the stored
    // digest is half-written. (A tear whose lost suffix happens to equal
    // the slot's prior bytes is undetectable by construction and
    // indistinguishable from a complete write — torn-write immunity is
    // about never *trusting* damage, not about sensing it.)
    uring_fs::arm_torn_write(5 * ROOT_LEDGER_SLOT_LEN + 100, 20);
    let err = write_ledger_slot(f.path(), 0, &kv_ledger_rec(5))
        .await
        .expect_err("the torn slot write fails loud to the checkpointer");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    let newest = read_newest_ledger(f.path(), 0)
        .await
        .expect("ledger contents never fail the read loud")
        .expect("valid predecessors exist");
    assert_eq!(
        newest,
        kv_ledger_rec(4),
        "the torn newest slot loses to seq 4"
    );

    // Slot 4's record decays too (a second, older tear): fall back again.
    kv_smash(f.path(), 4 * ROOT_LEDGER_SLOT_LEN + 30, 8).await;
    let newest = read_newest_ledger(f.path(), 0).await.unwrap().unwrap();
    assert_eq!(
        newest,
        kv_ledger_rec(3),
        "double fallback: newest VALID wins"
    );
}

/// K3 crash case (h) — the §4.6 pt 3 invariant with no other test: ring
/// reuse never overwrites the root-fallback window. The head bounds
/// against `reusable_upto` (advanced only after the retiring ledger record
/// is durable), NOT the in-RAM tail — so when the newest ledger slot tears,
/// the predecessor's whole replay window is still byte-intact; and once the
/// watermark does advance, the ring wraps and the retired entries can never
/// be resurrected.
#[tokio::test]
async fn test_kv_ring_reuse_never_overwrites_fallback_window() {
    let ring_f = kv_ring_file(8); // capacity 8 × 4072 = 32,576 logical bytes
    let ledger_f = NamedTempFile::new().unwrap();
    ledger_f
        .as_file()
        .set_len(ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN)
        .unwrap();
    let ring = JournalRing::new(ring_f.path(), 0, 8, 0);

    // Five 6,000-byte entries: head 30,000 of 32,576.
    let mut rs = Vec::new();
    for i in 1..=5u64 {
        rs.push(kv_append(&ring, i, 6000, 0x60 + i as u8).await);
    }

    // Checkpoint 1 (durable): tail = e1 (everything still live).
    let mut l1 = kv_ledger_rec(1);
    l1.journal_tail_seq = rs[0].seq();
    write_ledger_slot(ledger_f.path(), 0, &l1).await.unwrap();

    // Checkpoint 2 advances the in-RAM tail to e4 and writes its ledger
    // record — but the record is NOT yet known durable, so reusable_upto
    // must NOT move, and the head must refuse to grow into [e1, e4):
    let mut l2 = kv_ledger_rec(2);
    l2.journal_tail_seq = rs[3].seq();
    write_ledger_slot(ledger_f.path(), 0, &l2).await.unwrap();
    assert!(
        ring.core().try_admit(6000, AdmissionClass::User).is_none(),
        "the head must bound against reusable_upto, never the in-RAM tail (§4.6 pt 3)"
    );

    // Power loss tears checkpoint 2's slot. Mount falls back to seq 1 —
    // and BECAUSE the admission above was refused, its whole window
    // [e1, head) replays intact.
    kv_smash(ledger_f.path(), 2 * ROOT_LEDGER_SLOT_LEN + 40, 8).await;
    let mounted = read_newest_ledger(ledger_f.path(), 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mounted.seq, 1, "torn newest slot ⇒ predecessor selected");
    let (_, recovery) = JournalRing::recover(ring_f.path(), 0, 8, 0, mounted.journal_tail_seq)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![1, 2, 3, 4, 5],
        "the fallback window is byte-intact: ring reuse never overwrote it"
    );
    assert_eq!(recovery.dropped_torn, 0);

    // Checkpoint 2 retries and THIS time is known durable (post-barrier):
    // the watermark advances, admission opens, the ring wraps over the
    // retired entries…
    write_ledger_slot(ledger_f.path(), 0, &l2).await.unwrap();
    uring_fs::fdatasync(ledger_f.path().to_path_buf())
        .await
        .unwrap();
    ring.core().advance_reusable_upto(l2.journal_tail_seq);
    let r6 = kv_append(&ring, 6, 6000, 0x66).await;
    assert!(
        r6.end() > ring.core().geometry().logical_len(),
        "the new entry wrapped into lap 1 over retired pages"
    );

    // …and a mount from checkpoint 2 replays exactly its window: e4, e5,
    // e6 — the overwritten e1..e3 can never be resurrected.
    let mounted = read_newest_ledger(ledger_f.path(), 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mounted.seq, 2);
    let (_, recovery) = JournalRing::recover(ring_f.path(), 0, 8, 0, mounted.journal_tail_seq)
        .await
        .expect("never loud");
    assert_eq!(kv_recovered_inos(&recovery), vec![4, 5, 6]);
    assert_eq!(recovery.dropped_torn, 0, "a clean wrap is not a tear");
}

// ---------------------------------------------------------------------------
// PR K4: the v3 extent-allocator crash contract
// (design-cow-kv-metadata §4.7, §4.10, risk R3).
// ---------------------------------------------------------------------------

/// Write one journal entry of allocator delta records (admit → reserve →
/// write, seqs restamped with the reservation seq — the K3 identity).
async fn kv_alloc_journal(ring: &JournalRing, records: Vec<(u8, Record)>) {
    let need = entry_len_for(&records).expect("entry under cap");
    let adm = ring
        .core()
        .try_admit(need, AdmissionClass::User)
        .expect("test ring must have room");
    let res = ring.core().reserve(adm);
    let records: Vec<(u8, Record)> = records
        .into_iter()
        .map(|(t, mut r)| {
            r.seq = res.seq();
            (t, r)
        })
        .collect();
    ring.write_entry(&res, &records)
        .await
        .expect("clean entry write");
}

/// K4 crash case (a) — the §4.10 "bitmap slots (torn A ⇒ B)" unit: a
/// checkpoint's bitmap page write tears mid-slot. The tear can only
/// damage the slot being replaced (the writer alternates away from the
/// newest valid copy), so mount selects the intact predecessor slot and
/// the journal's alloc records ≥ tail re-supply exactly the deltas the
/// torn write carried — the §4.7 "bitmap is a checkpoint accelerator,
/// not the sole truth" rule under fire. Never loud.
#[tokio::test]
async fn test_kv_bitmap_torn_slot_predecessor_plus_journal_reconstruct() {
    let ring_pages = 4u64;
    let bitmap_base = ring_pages * JOURNAL_PAGE_LEN;
    let total = 12u64; // one bitmap page
    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(bitmap_base + bitmap_region_len(total))
        .unwrap();
    let _g = FaultGuard;

    let ring = JournalRing::new(f.path(), 0, ring_pages, 0);
    let alloc = ExtentAllocator::format(total, 0, 4);

    // Checkpoint 1 (durable): e0, e1 allocated, journaled, persisted to
    // slot A at generation 1.
    let e0 = alloc.claim_internal().unwrap();
    let e1 = alloc.claim_internal().unwrap();
    kv_alloc_journal(&ring, vec![alloc_record(e0, 0), alloc_record(e1, 0)]).await;
    let wrote = alloc
        .write_dirty_pages(f.path(), bitmap_base, 1)
        .await
        .unwrap();
    assert_eq!(wrote, vec![0]);
    uring_fs::fdatasync(f.path().to_path_buf()).await.unwrap();

    // e2's claim lands in the journal, then checkpoint 2's page write —
    // into slot B, away from the newest valid copy — tears mid-image.
    let e2 = alloc.claim_internal().unwrap();
    kv_alloc_journal(&ring, vec![alloc_record(e2, 0)]).await;
    uring_fs::arm_torn_write(bitmap_base + ALLOC_PAGE_LEN + 64, 10);
    let err = alloc
        .write_dirty_pages(f.path(), bitmap_base, 2)
        .await
        .expect_err("the torn page write fails loud to the checkpointer");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    // "Remount": slot A (generation 1) still verifies, torn slot B reads
    // as absent; replaying the window re-marks e0, e1 (idempotent) and
    // re-supplies e2 — the delta the torn write lost.
    let region = uring_fs::read_at(f.path(), bitmap_base, bitmap_region_len(total) as usize)
        .await
        .unwrap();
    let (gen_a, _) = decode_bitmap_page(&region[..ALLOC_PAGE_LEN as usize], 0)
        .expect("the predecessor slot must survive the tear");
    assert_eq!(gen_a, 1);
    assert!(
        decode_bitmap_page(&region[ALLOC_PAGE_LEN as usize..], 0).is_err(),
        "the torn slot must read as absent, never loud"
    );

    let (_ring2, recovery) = JournalRing::recover(f.path(), 0, ring_pages, 0, 0)
        .await
        .expect("never loud");
    let loaded = ExtentAllocator::load(f.path(), bitmap_base, total, 0, 4, 1, &recovery.entries)
        .await
        .expect("bitmap contents never fail a mount loud");
    for e in [e0, e1, e2] {
        assert!(
            loaded.is_allocated(e),
            "extent {e} lost to the torn bitmap slot"
        );
    }
    assert_eq!(loaded.free_extents(), total - 3);
    assert_eq!(
        loaded.resume_generation(),
        1,
        "generation numbering resumes above the intact predecessor"
    );
}

/// K4 crash case (b) — **risk R3**, the §4.7 pending-free rule's dedicated
/// crash test: compaction-heavy churn (extents freed and *reused* across
/// checkpoints), then power loss tears the NEWEST root-ledger record.
/// Mount falls back to the predecessor — and because a freed extent is
/// never allocatable before the freeing checkpoint's durable seq, every
/// extent the predecessor references is byte-intact: the crashed round's
/// claims could not have reused them. Nothing is double-allocated,
/// nothing is lost that the predecessor + journal cannot reconstruct.
#[tokio::test]
async fn test_kv_alloc_torn_newest_root_after_churn_predecessor_extents_intact() {
    // One file, four regions: ring 8 pages [0, 32 KiB); ledger 32 slots;
    // bitmap (one page pair); a 6-extent "heap" of 4 KiB test extents.
    const RING_PAGES: u64 = 8;
    const LEDGER_BASE: u64 = RING_PAGES * JOURNAL_PAGE_LEN;
    const BITMAP_BASE: u64 = LEDGER_BASE + ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN;
    const TOTAL_EXTENTS: u64 = 6;
    const EXT_LEN: u64 = 4096;
    let heap_base: u64 = BITMAP_BASE + bitmap_region_len(TOTAL_EXTENTS);

    let f = NamedTempFile::new().unwrap();
    f.as_file()
        .set_len(heap_base + TOTAL_EXTENTS * EXT_LEN)
        .unwrap();
    let _g = FaultGuard;

    let pattern = |round: u8, extent: u64| vec![round * 16 + extent as u8; EXT_LEN as usize];
    let ledger_rec = |seq: u64, live: &[u64], generation: u64| LedgerRecord {
        seq,
        tree_roots: live
            .iter()
            .map(|&e| TreeRoot {
                tree_id: TREE_INODES,
                node_addr: heap_base + e * EXT_LEN,
                node_seq: seq,
            })
            .collect(),
        journal_tail_seq: 0,
        next_ino: 2,
        alloc_bitmap_generation: generation,
        node_seq_watermark: seq,
        membership_stamp: None,
    };

    let ring = JournalRing::new(f.path(), 0, RING_PAGES, 0);
    let alloc = ExtentAllocator::format(TOTAL_EXTENTS, 0, 8);

    // Rounds 1..=3: full compaction cycles. Round r claims fresh extents,
    // writes its patterns, journals the deltas, pending-frees round
    // r−1's extents under checkpoint r, checkpoints (pages + ledger +
    // barrier), and only then — §4.7 — releases them for reuse.
    let mut live: Vec<u64> = Vec::new();
    let mut freed_last_round: Vec<u64> = Vec::new();
    let mut reuse_seen = false;
    for round in 1u64..=3 {
        let mut claims = Vec::new();
        for _ in 0..2 {
            let e = alloc.claim_internal().expect("churn heap has room");
            claims.push(e);
            uring_fs::write_at(f.path(), heap_base + e * EXT_LEN, pattern(round as u8, e))
                .await
                .unwrap();
            kv_alloc_journal(&ring, vec![alloc_record(e, 0)]).await;
        }
        reuse_seen |= claims.iter().any(|e| freed_last_round.contains(e));

        for &old in &live {
            alloc.free_pending(old, round).expect("pending cap holds");
            kv_alloc_journal(&ring, vec![free_record(old, round, 0)]).await;
        }
        alloc
            .write_dirty_pages(f.path(), BITMAP_BASE, round)
            .await
            .unwrap();
        write_ledger_slot(f.path(), LEDGER_BASE, &ledger_rec(round, &claims, round))
            .await
            .unwrap();
        uring_fs::fdatasync(f.path().to_path_buf()).await.unwrap();
        // The barrier makes checkpoint `round` durable: NOW its retired
        // extents may re-enter the pool (§4.7).
        let released = alloc.advance_durable(round);
        assert_eq!(released, live.len() as u64);
        freed_last_round = live.clone();
        live = claims;
    }
    assert!(
        reuse_seen,
        "the churn must actually reuse freed extents, or R3 is untested"
    );

    // Round 4 — the crashing checkpoint: claim EVERY free extent (the
    // heap is sized so this is exactly 4), overwrite them, journal, and
    // pending-free round 3's extents under checkpoint 4.
    let mut r4_claims = Vec::new();
    for _ in 0..4 {
        let e = alloc.claim_internal().expect("4 free extents");
        r4_claims.push(e);
        uring_fs::write_at(f.path(), heap_base + e * EXT_LEN, pattern(4, e))
            .await
            .unwrap();
        kv_alloc_journal(&ring, vec![alloc_record(e, 0)]).await;
    }
    for &old in &live {
        alloc.free_pending(old, 4).expect("pending cap holds");
        kv_alloc_journal(&ring, vec![free_record(old, 4, 0)]).await;
    }
    // The §4.7 gate under pressure: round 3's extents are pending, their
    // retiring checkpoint is NOT yet durable — a claim must refuse rather
    // than reuse them (a broken gate would hand them out here and destroy
    // the predecessor's state).
    assert!(
        matches!(alloc.claim_internal(), Err(KvError::NoSpace { .. })),
        "pending extents must never satisfy claims before their durable seq"
    );

    // Checkpoint 4's pages land, but power loss tears its ledger slot
    // mid-record; the barrier never fires, advance_durable(4) never runs.
    alloc
        .write_dirty_pages(f.path(), BITMAP_BASE, 4)
        .await
        .unwrap();
    uring_fs::arm_torn_write(LEDGER_BASE + 4 * ROOT_LEDGER_SLOT_LEN + 100, 20);
    let err = write_ledger_slot(f.path(), LEDGER_BASE, &ledger_rec(4, &r4_claims, 4))
        .await
        .expect_err("the torn ledger write fails loud to the checkpointer");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    // ---- "Remount" ----
    // Newest valid ledger record: the predecessor, checkpoint 3.
    let mounted = read_newest_ledger(f.path(), LEDGER_BASE)
        .await
        .expect("ledger contents never fail the read loud")
        .expect("predecessors exist");
    assert_eq!(mounted.seq, 3, "torn newest slot ⇒ predecessor selected");

    // THE R3 ASSERTION: every extent the predecessor references is
    // byte-intact — round 4's claims could not have reused them because
    // their pending-free tags (4) were never durable.
    for root in &mounted.tree_roots {
        let extent = (root.node_addr - heap_base) / EXT_LEN;
        let bytes = uring_fs::read_at(f.path(), root.node_addr, EXT_LEN as usize)
            .await
            .unwrap();
        assert_eq!(
            &bytes[..],
            &pattern(3, extent)[..],
            "extent {extent} referenced by the mounted predecessor was \
             overwritten — the §4.7 pending-free gate is broken (R3)"
        );
    }

    // Reconstruction: predecessor pages (generation ≤ 4 on disk — the
    // torn checkpoint's page writes may all have landed; newest-valid is
    // still safe, §4.7) + journal replay rebuild the full allocator
    // state: predecessor extents allocated (nothing lost), round-4
    // claims allocated (their records replay — the per-key LWW finals),
    // round-3 frees pending again (in-window ⇒ parked, design-smo-
    // replay-currency §2-A), and NOTHING double-allocatable.
    let (_ring2, recovery) =
        JournalRing::recover(f.path(), 0, RING_PAGES, 0, mounted.journal_tail_seq)
            .await
            .expect("never loud");
    assert_eq!(recovery.dropped_torn, 0);
    let loaded = ExtentAllocator::load(
        f.path(),
        BITMAP_BASE,
        TOTAL_EXTENTS,
        0,
        8,
        mounted.journal_tail_seq,
        &recovery.entries,
    )
    .await
    .expect("never loud");

    for root in &mounted.tree_roots {
        let extent = (root.node_addr - heap_base) / EXT_LEN;
        assert!(
            loaded.is_allocated(extent),
            "predecessor extent {extent} lost by reconstruction"
        );
    }
    for &e in &r4_claims {
        assert!(loaded.is_allocated(e), "replayed round-4 claim {e} lost");
    }
    assert_eq!(
        loaded.pending_count(),
        live.len() as u64,
        "round-3 frees rebuild as pending (§4.7: pending-free is journaled)"
    );
    assert_eq!(loaded.free_extents(), 0, "occupancy reconstructs exactly");
    assert!(
        matches!(loaded.claim_internal(), Err(KvError::NoSpace { .. })),
        "nothing is double-allocatable after the fallback"
    );

    // The reconstruct closes: the first post-mount checkpoint (seq 4
    // again, bitmap generation above everything on disk — including the
    // crashed round's landed pages) becomes durable with a tail past the
    // replayed window — coverage of the freeing records, not mere record
    // durability (§2-A) — and only then do the predecessor's retired
    // extents re-enter the pool.
    let generation = loaded
        .resume_generation()
        .max(mounted.alloc_bitmap_generation)
        + 1;
    assert_eq!(
        generation, 5,
        "the crashed checkpoint's landed pages (generation 4) must push \
         the resume generation past them"
    );
    loaded
        .write_dirty_pages(f.path(), BITMAP_BASE, generation)
        .await
        .unwrap();
    write_ledger_slot(
        f.path(),
        LEDGER_BASE,
        &ledger_rec(4, &r4_claims, generation),
    )
    .await
    .unwrap();
    uring_fs::fdatasync(f.path().to_path_buf()).await.unwrap();
    // The post-mount record's tail: past every replayed entry (the
    // window fully re-covered by the fresh checkpoint's flush).
    let covering_tail = recovery
        .entries
        .last()
        .map(|e| e.seq + JOURNAL_PAGE_LEN)
        .expect("the churn window is non-empty");
    assert_eq!(loaded.advance_durable(covering_tail), 2);
    let mut reopened = Vec::new();
    while let Ok(e) = loaded.claim_internal() {
        reopened.push(e);
    }
    reopened.sort_unstable();
    let mut expected = live.clone();
    expected.sort_unstable();
    assert_eq!(
        reopened, expected,
        "exactly the predecessor's retired extents re-enter the pool once \
         their retiring checkpoint is durable"
    );
}

// ===========================================================================
// PR K6a — superblock v3 + mount-path crash cases
// (design-cow-kv-metadata §4.1/§4.10, PR K6a). The superblock is a
// **durable-coverage unit**: unlike journal-window contents, a torn or
// corrupt sector 0 fails the mount LOUD. The root ledger keeps its K3
// semantics at the backend level: a torn newest slot falls back to the
// predecessor and the mount serves the predecessor's state byte-intact.
// ===========================================================================

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::checkpoint::write_ledger_slot as kv_write_ledger_slot;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, write_superblock_v3, SUPERBLOCK_V3_LEN,
};

const K6A_VOL_LEN: u64 = 64 * 1024 * 1024;

/// A small populated v3 image (deterministic identity, 64 KiB nodes,
/// 1 MiB ring).
async fn k6a_built_volume() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(K6A_VOL_LEN).unwrap();
    let mut b = ImageBuilder::new(BuilderConfig {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        hash_seed: 0xC0FFEE,
        uuid: *b"crash-k6a-volume",
    })
    .unwrap();
    let d = b.add_dir(ROOT_INO, "dir", 0o755, 0, 0).unwrap();
    for i in 0..32 {
        b.add_file(d, &format!("f{i:02}"), 0o644, 0, 0, i).unwrap();
    }
    b.build(f.path(), K6A_VOL_LEN).await.unwrap();
    f
}

/// K6a crash case (a), **as amended by DUR-5**: a torn sector-0 write no
/// longer condemns the volume — the redundant superblock copy carries it
/// and the write mount repairs sector 0 — but a tear that takes BOTH
/// copies still fails loud (§4.10 "loud mount failures are reserved for
/// units with durable-coverage arguments (superblock, …)"). The tear is
/// injected with the honest in-flight shim: the rewrite persists only a
/// prefix, exactly a format racing power loss.
#[tokio::test]
async fn test_kv_v3_torn_superblock_recovers_then_fails_loud_when_both_slots_die() {
    let f = k6a_built_volume().await;
    let _g = FaultGuard;

    // Grab the valid superblock, then re-write a CHANGED one torn (a
    // re-format with a fresh identity racing power loss): 100 bytes of
    // the new image survive — magic/version plus the new uuid's first
    // bytes land, while the new checksum (offset 120) is lost — so
    // sector 0 holds a hybrid no checksum can bless. (A tear whose
    // persisted prefix is byte-identical to what it replaced is
    // indistinguishable from a complete write by construction — the K3
    // ledger case states the same.)
    let mut sb = match classify_volume(f.path()).await.unwrap() {
        squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 volume, got {other:?}"),
    };
    sb.uuid = *b"reformat-newuuid";
    uring_fs::arm_torn_write(0, 100);
    let err = write_superblock_v3(f.path(), &sb)
        .await
        .expect_err("the torn superblock write fails loud to the formatter");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    // DUR-5: the redundant copy (written and barriered BEFORE sector 0)
    // carries the image, so the volume classifies and MOUNTS instead of
    // being condemned — and the write mount repairs sector 0.
    match classify_volume(f.path()).await {
        Ok(squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(rec)) => assert_eq!(
            rec.uuid, sb.uuid,
            "the recovered superblock must be the newest valid image"
        ),
        other => panic!("a torn sector 0 with an intact copy must classify: {other:?}"),
    }
    let be = KvMetaBackend::open(f.path())
        .await
        .expect("the mount recovers from the redundant superblock copy");
    be.shutdown().await.expect("shutdown");
    drop(be);
    // Sector 0 itself is healed: a raw read of it decodes on its own.
    let raw = uring_fs::read_at(f.path(), 0, SUPERBLOCK_V3_LEN)
        .await
        .expect("read sector 0");
    squeezefs::meta_backend::kv::superblock::classify_sector0(&raw)
        .expect("the write mount must repair sector 0 from the copy");

    // Both slots dead ⇒ loud, as corruption, never as "run format" and
    // never by silently limping into a mount.
    let len = std::fs::metadata(f.path()).unwrap().len();
    let backup = squeezefs::meta_backend::kv::superblock::backup_offset(len)
        .expect("the volume reserves a backup slot");
    for off in [0u64, backup] {
        uring_fs::write_at(
            f.path(),
            off,
            bytes::Bytes::from(vec![0xA5u8; SUPERBLOCK_V3_LEN]),
        )
        .await
        .expect("scribble a slot");
    }
    let err = classify_volume(f.path())
        .await
        .expect_err("both slots dead must classify loud")
        .to_string();
    assert!(
        err.contains("magic") || err.contains("checksum") || err.contains("corrupt"),
        "the refusal must name the corruption, got: {err}"
    );
    assert!(
        !err.to_lowercase().contains("not formatted"),
        "a scribbled SB is corruption, not a blank volume: {err}"
    );
    let err = KvMetaBackend::open(f.path())
        .await
        .expect_err("the mount must refuse when both superblock slots are gone")
        .to_string();
    assert!(
        err.contains("magic") || err.contains("checksum") || err.contains("corrupt"),
        "got: {err}"
    );
}

/// K6a crash case (b): a torn NEWEST ledger record at mount ⇒ the backend
/// serves the predecessor checkpoint byte-intact (§4.1 newest-valid-wins;
/// sound by pending-free §4.7 + the ring twin §4.6 pt 3). This is the K3
/// slot-level case promoted to the full mount path: superblock → ledger →
/// bitmap → replay → reads.
#[tokio::test]
async fn test_kv_v3_torn_newest_ledger_mount_serves_predecessor() {
    let f = k6a_built_volume().await;
    let _g = FaultGuard;

    // Mount once clean: this is the predecessor state a fallback must
    // reproduce exactly.
    let (want_digest, want_seq, sb) = {
        let be = KvMetaBackend::open(f.path()).await.unwrap();
        (
            digest_backend(&be).await.unwrap(),
            be.mounted_ledger().seq,
            be.superblock().clone(),
        )
    };

    // A later checkpoint (seq + 1) races power loss: its slot write tears
    // mid-record. Roots point at garbage on purpose — if the fallback ever
    // TRUSTED this record, the mount would fail loud on a bad root.
    let mut torn = squeezefs::meta_backend::kv::checkpoint::LedgerRecord {
        seq: want_seq + 1,
        tree_roots: vec![],
        journal_tail_seq: 0,
        next_ino: 999_999,
        alloc_bitmap_generation: 999,
        node_seq_watermark: 999,
        membership_stamp: None,
    };
    torn.tree_roots
        .push(squeezefs::meta_backend::kv::checkpoint::TreeRoot {
            tree_id: TREE_INODES,
            node_addr: 0xDEAD_0000,
            node_seq: 0xDEAD,
        });
    let slot_off = sb.root_ledger.start + (torn.seq % 32) * 4096;
    uring_fs::arm_torn_write(slot_off + 40, 12);
    let err = kv_write_ledger_slot(f.path(), sb.root_ledger.start, &torn)
        .await
        .expect_err("the torn slot write fails loud to the checkpointer");
    assert!(matches!(err, KvError::Io(_)), "got {err:?}");
    uring_fs::clear_faults();

    // Remount: the torn newest slot loses to its intact predecessor and
    // the served state is identical to the pre-crash mount.
    let be = KvMetaBackend::open(f.path())
        .await
        .expect("a torn newest ledger slot must never fail the mount");
    assert_eq!(
        be.mounted_ledger().seq,
        want_seq,
        "mount must select the intact predecessor record"
    );
    assert_eq!(
        digest_backend(&be).await.unwrap(),
        want_digest,
        "the predecessor's tree state must be byte-intact (post-fold digest)"
    );
    assert_eq!(
        be.lookup(ROOT_INO, "dir").await.unwrap().mode & libc::S_IFMT,
        libc::S_IFDIR,
        "reads serve normally from the fallback state"
    );
}

// ===========================================================================
// PR M6 (design-metadata-throughput §5.4 D4.b): rename one-entry atomicity.
// The routed rename stages dentry surgery + BOTH parents' Δtimes + the
// moved inode's Δctime (+ dest accounting) in ONE KvTx — one checksummed
// journal entry — so a crash straddling a rename leaves either the fully-
// old or the fully-new naming AND time surface, never a half (the pre-M6
// fragments could persist the naming without the time updates; worse, a
// multi-tx shape could tear between fragments). Injected with the
// power-cut shim: the same volatile-cache-loss model kill-9 cannot produce
// on file-backed volumes.
// ===========================================================================

/// Format a v3 volume, drive a routed rename, cut power before the barrier
/// ⇒ remount serves the FULLY-OLD state (naming + parent times + source
/// ctime, byte-exact); repeat with a barrier before the cut ⇒ FULLY-NEW.
/// Never a mixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rename_one_entry_atomicity_power_cut() {
    use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
    use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
    use std::sync::Arc;

    // Deterministic cadence: park the checkpoint task an hour out so no
    // background barrier can make the rename durable inside the
    // rename→power_cut window (the knob is read at backend open;
    // --test-threads=1 makes the env mutation safe).
    const KNOB: &str = "SQUEEZEFS_META_FLUSH_INTERVAL_MS";
    struct EnvRestore(Option<String>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var(KNOB, v),
                None => std::env::remove_var(KNOB),
            }
        }
    }
    let restore = EnvRestore(std::env::var(KNOB).ok());
    std::env::set_var(KNOB, "3600000");

    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(96 * 1024 * 1024).unwrap();
    format_v3(
        f.path(),
        96 * 1024 * 1024,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(1024 * 1024),
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let _g = FaultGuard;

    // ---- Durable base: two dirs + the file, barriered. -------------------
    let kv = KvMetaBackend::open(f.path()).await.expect("mount 1");
    let b = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let d_from = b
        .create(1, "d_from", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir d_from")
        .ino;
    let d_to = b
        .create(1, "d_to", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir d_to")
        .ino;
    let ino = b
        .create(d_from, "victim", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create victim")
        .ino;
    b.volumes[0].sync_device().await.expect("base barrier");
    let from_base = b.getattr(d_from).await.expect("d_from base");
    let to_base = b.getattr(d_to).await.expect("d_to base");
    let src_base = b.getattr(ino).await.expect("victim base");

    // ---- Crash leg: rename lands in the ring UNSYNCED, power cut. --------
    uring_fs::arm_power_cut(f.path());
    b.rename(d_from, "victim", d_to, "renamed", 0)
        .await
        .expect("rename (unsynced)");
    // RAM sanity: the tx applied before the cut.
    assert!(b.lookup(d_to, "renamed").await.is_ok(), "RAM sees new name");
    assert!(
        b.lookup(d_from, "victim").await.is_err(),
        "RAM lost old name"
    );
    let reverted = uring_fs::power_cut(f.path());
    assert!(reverted >= 1, "the unsynced rename bytes must be reverted");
    drop(b); // releases the writer-guard flock; checkpoint task reaps via Weak

    let kv = KvMetaBackend::open(f.path()).await.expect("remount 1");
    let b = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let old_name = b.lookup(d_from, "victim").await;
    let new_name = b.lookup(d_to, "renamed").await;
    assert!(
        old_name.is_ok() && new_name.is_err(),
        "power cut before the barrier ⇒ FULLY-OLD naming (old: {:?}, new: {:?})",
        old_name.map(|i| i.ino),
        new_name.map(|i| i.ino)
    );
    assert_eq!(
        old_name.unwrap().ino,
        ino,
        "the surviving old name resolves the original inode"
    );
    let from_after = b.getattr(d_from).await.expect("d_from after cut");
    let to_after = b.getattr(d_to).await.expect("d_to after cut");
    let src_after = b.getattr(ino).await.expect("victim after cut");
    assert_eq!(
        (from_after.mtime, from_after.ctime),
        (from_base.mtime, from_base.ctime),
        "FULLY-OLD means the old parent's times reverted byte-exact"
    );
    assert_eq!(
        (to_after.mtime, to_after.ctime),
        (to_base.mtime, to_base.ctime),
        "FULLY-OLD means the new parent's times reverted byte-exact"
    );
    assert_eq!(
        src_after.ctime, src_base.ctime,
        "FULLY-OLD means the moved inode's ctime reverted byte-exact"
    );

    // ---- Committed leg: rename + barrier, then cut ⇒ FULLY-NEW. ----------
    uring_fs::arm_power_cut(f.path());
    b.rename(d_from, "victim", d_to, "renamed", 0)
        .await
        .expect("rename (to be synced)");
    b.volumes[0].sync_device().await.expect("rename barrier");
    let _ = uring_fs::power_cut(f.path());
    drop(b);

    let kv = KvMetaBackend::open(f.path()).await.expect("remount 2");
    let b = Arc::new(RoutedMetaBackend::new(vec![kv]));
    assert!(
        b.lookup(d_from, "victim").await.is_err() && b.lookup(d_to, "renamed").await.is_ok(),
        "barrier before the cut ⇒ FULLY-NEW naming"
    );
    let from_new = b.getattr(d_from).await.expect("d_from committed");
    let to_new = b.getattr(d_to).await.expect("d_to committed");
    let src_new = b.getattr(ino).await.expect("victim committed");
    assert!(
        from_new.mtime > from_base.mtime && from_new.ctime > from_base.ctime,
        "FULLY-NEW carries the old parent's time updates in the SAME entry"
    );
    assert!(
        to_new.mtime > to_base.mtime && to_new.ctime > to_base.ctime,
        "FULLY-NEW carries the new parent's time updates in the SAME entry"
    );
    assert!(
        src_new.ctime > src_base.ctime,
        "FULLY-NEW carries the moved inode's ctime in the SAME entry"
    );
    drop(restore);
}

// ---------------------------------------------------------------------------
// PR M7 — the conveyor batch crash contract (design-metadata-throughput
// §5.5 D5, riding the §4.10 harness). A conveyor batch is N ORDINARY
// checksummed entries in one contiguous reservation, written as ONE
// `write_at_batch`: replay is byte-for-byte today's walk, so a torn batch
// member drops THAT tx only — identical to the pre-conveyor independent-
// committers exposure (§4.10 caveat (a) neither grows nor shrinks). The
// cases below pin exactly that: torn FIRST / MIDDLE / LAST member, a
// batch spanning the ring wrap, the deterministic co-batched rollback
// race (whole-batch §4.4 pt 4 rollback under a live same-key Δtime), and
// replay-twice digest equality over batched commits.
// ---------------------------------------------------------------------------

/// Write a 3-member conveyor batch (one contiguous reservation, one
/// `write_entries_batch` submission) of `lens`-sized entries with inos
/// 101/102/103, returning the per-member reservations.
async fn kv_write_batch3(
    ring: &JournalRing,
    lens: [u64; 3],
) -> [squeezefs::meta_backend::kv::journal_core::Reservation; 3] {
    let total: u64 = lens.iter().sum();
    let adm = ring
        .core()
        .try_admit(total, AdmissionClass::User)
        .expect("test ring must admit the batch");
    let batch = ring.core().reserve(adm);
    let mut parts_res = [Reservation { start: 0, len: 0 }; 3];
    let mut cursor = batch.start;
    for (i, len) in lens.into_iter().enumerate() {
        parts_res[i] = Reservation { start: cursor, len };
        cursor += len;
    }
    let recs: Vec<Vec<(u8, Record)>> = (0..3)
        .map(|i| kv_sized_records(101 + i as u64, lens[i], 0xC1 + i as u8, parts_res[i].seq()))
        .collect();
    let parts: Vec<(Reservation, &[(u8, Record)])> = parts_res
        .iter()
        .zip(&recs)
        .map(|(r, recs)| (*r, recs.as_slice()))
        .collect();
    ring.write_entries_batch(&parts)
        .await
        .expect("clean batch write");
    parts_res
}

/// M7 crash case: torn FIRST batch member — its bytes lost to unordered
/// writeback while its batch-mates' pages persisted. Replay drops exactly
/// that tx, resyncs at the next page, and recovers the middle and last
/// members (plus a later solo entry). One confirmed drop is counted.
/// (Members are page-sized so each starts a page: batch entries follow
/// the SAME §4.1 chain/resync rules as independent committers' — a torn
/// entry's same-page successors are unreachable until the next verified
/// page header, exactly the pre-M7 exposure; the same-page variant below
/// pins that unchanged rule inside a batch.)
#[tokio::test]
async fn test_kv_batch_torn_first_member_drops_alone() {
    let f = kv_ring_file(8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);
    let page = JOURNAL_PAGE_DATA_LEN;

    let parts = kv_write_batch3(&ring, [page, page, page]).await;
    let _later = kv_append(&ring, 200, 1000, 0xD0).await;

    kv_smash(f.path(), kv_phys(&ring, parts[0].start + 40), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![102, 103, 200],
        "the torn FIRST member drops alone; its batch-mates replay whole"
    );
    assert_eq!(recovery.dropped_torn, 1, "one confirmed drop-and-resync");
}

/// M7 crash case: torn MIDDLE batch member — batch-mates on both sides
/// replay whole.
#[tokio::test]
async fn test_kv_batch_torn_middle_member_drops_alone() {
    let f = kv_ring_file(8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);
    let page = JOURNAL_PAGE_DATA_LEN;

    let parts = kv_write_batch3(&ring, [page, page, page]).await;
    let _later = kv_append(&ring, 200, 1000, 0xD0).await;

    kv_smash(f.path(), kv_phys(&ring, parts[1].start + 40), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![101, 103, 200],
        "the torn MIDDLE member drops alone; first and last replay whole"
    );
    assert_eq!(recovery.dropped_torn, 1, "one confirmed drop-and-resync");
}

/// M7 crash case, the SAME-PAGE variant: a torn member's same-page batch
/// successor is unreachable (chain-only discovery inside a page — §4.1),
/// while the next page-start member replays. This is byte-for-byte the
/// pre-M7 independent-committers exposure (§4.10 caveat (a) "un-acked
/// holes were always possible across txs" — the members here were never
/// barrier-acked); batching neither grows nor shrinks it.
#[tokio::test]
async fn test_kv_batch_torn_member_same_page_successor_follows_chain_rule() {
    let f = kv_ring_file(8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);

    // 101 [0,1500) + 102 [1500,3000) share page 0; 103 [3000,3000+page)
    // continues; the later solo entry starts a fresh page.
    let parts = kv_write_batch3(&ring, [1500, 1500, JOURNAL_PAGE_DATA_LEN]).await;
    let _later = kv_append(&ring, 200, 1000, 0xD0).await;

    kv_smash(f.path(), kv_phys(&ring, parts[0].start + 40), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![200],
        "the torn member's same-page successors die with the chain (the standing \
         §4.1 rule); the next page-start entry replays"
    );
    assert!(
        recovery.dropped_torn >= 1,
        "the mid-log damage is a counted drop"
    );
}

/// M7 crash case: torn LAST batch member with nothing after it — the
/// trailing-tear shape: earlier members replay, the tear reads as
/// end-of-log (dropped_torn stays 0 — the §4.1 clean-unmount alert
/// accounting), and the recovered head resumes before the torn member.
#[tokio::test]
async fn test_kv_batch_torn_last_member_is_end_of_log() {
    let f = kv_ring_file(8);
    let ring = JournalRing::new(f.path(), 0, 8, 0);

    let parts = kv_write_batch3(&ring, [1500, 1500, 1500]).await;

    kv_smash(f.path(), kv_phys(&ring, parts[2].start + 40), 16).await;

    let (_, recovery) = JournalRing::recover(f.path(), 0, 8, 0, 0)
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![101, 102],
        "the torn LAST member drops alone; earlier members replay whole"
    );
    assert_eq!(
        recovery.head_pos,
        parts[1].end(),
        "the recovered head resumes before the torn trailing member"
    );
    assert_eq!(
        recovery.dropped_torn, 0,
        "a trailing tear is end-of-log, never a counted drop"
    );
}

/// M7 crash case: a batch spanning the ring WRAP (lap 0 → lap 1) replays
/// every member — the contiguous reservation's logical positions map
/// through the wrap exactly like a solo multi-page entry's.
#[tokio::test]
async fn test_kv_batch_spanning_ring_wrap_replays_whole() {
    let f = kv_ring_file(4); // capacity 4 × 4072 = 16288
    let ring = JournalRing::new(f.path(), 0, 4, 0);

    // Fill most of lap 0, retire it (checkpoint-durable), so the batch
    // below wraps into lap 1.
    let a = kv_append(&ring, 1, 6000, 0xE1).await;
    let b = kv_append(&ring, 2, 6000, 0xE2).await;
    ring.advance_reusable_upto(b.end());
    let _ = a;

    let parts = kv_write_batch3(&ring, [2000, 2000, 2000]).await;
    assert!(
        ring.core().geometry().lap(parts[2].end() - 1) > 0,
        "the batch must actually cross into lap 1 (test geometry)"
    );

    // Mount from the durable tail (the pre-batch ledger state).
    let (_, recovery) = JournalRing::recover(f.path(), 0, 4, 0, b.end())
        .await
        .expect("never loud");
    assert_eq!(
        kv_recovered_inos(&recovery),
        vec![101, 102, 103],
        "every member of the wrap-spanning batch replays"
    );
    assert_eq!(recovery.dropped_torn, 0);
}

/// M7 crash case: the DETERMINISTIC co-batched rollback race (the §4.4
/// pt 4 mid-batch shape). Two same-parent creates are forced into ONE
/// batch (held pass), both staging Δtime merge records on the same
/// parent-inode key; the batch's single write takes an armed fault, the
/// whole-batch seq-conditional rollback runs, and RAM == replay — the
/// parent's committed pre-batch state stands, neither create is visible,
/// and the volume neither leaks budget nor wedges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_kv_batch_mid_rollback_race_ram_equals_replay() {
    use squeezefs::meta_backend::kv::backend::{
        test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_DRAIN,
        TEST_CONVEYOR_HOLD_STAGE,
    };
    use squeezefs::meta_backend::kv::builder::{digest_walk, format_v3, FormatV3Options};
    use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};

    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            TEST_CONVEYOR_HOLD_STAGE.store(0, std::sync::atomic::Ordering::SeqCst);
            test_conveyor_hold_release();
            uring_fs::clear_faults();
        }
    }
    // Park the checkpoint cadence (the probe-digest discipline of the
    // crash_kill rollback-race test).
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let dir = tempfile::tempdir().unwrap();
    let vol = dir.path().join("batch-rollback.v3.meta");
    std::fs::File::create(&vol)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    format_v3(
        &vol,
        64 * 1024 * 1024,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let be = KvMetaBackend::open(&vol).await.unwrap();
    let routed = std::sync::Arc::new(RoutedMetaBackend::new(vec![be.clone()]));
    let parent = routed
        .create(1, "racedir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;

    // Hold the pass; co-queue two same-parent creates (both stage the
    // §4.4 pt 6 Δtime merge record on the parent key — the sanctioned
    // same-key co-queue); arm a persistent write error at the batch's
    // head byte; release.
    TEST_CONVEYOR_HOLD_STAGE.store(
        TEST_CONVEYOR_HOLD_PRE_DRAIN,
        std::sync::atomic::Ordering::SeqCst,
    );
    let a = {
        let r = routed.clone();
        tokio::spawn(async move {
            r.create(parent, "race-a", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    let b = {
        let r = routed.clone();
        tokio::spawn(async move {
            r.create(parent, "race-b", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while be.conveyor_pending_len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "both creates must co-queue behind the held pass"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let head = be.journal_ring().core().head();
    let geo = *be.journal_ring().core().geometry();
    let phys = be.superblock().journal.start
        + geo.page_index(head) * JOURNAL_PAGE_LEN
        + JOURNAL_PAGE_HDR_LEN
        + geo.in_page_off(head);
    uring_fs::arm_sector_write_error(phys);
    TEST_CONVEYOR_HOLD_STAGE.store(0, std::sync::atomic::Ordering::SeqCst);
    test_conveyor_hold_release();

    let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
    uring_fs::clear_faults();
    assert!(
        ra.is_err() && rb.is_err(),
        "the co-batched write failure must fail BOTH members (whole-batch rollback); \
         got a={ra:?} b={rb:?}"
    );
    assert!(
        routed.lookup(parent, "race-a").await.is_err()
            && routed.lookup(parent, "race-b").await.is_err(),
        "rolled-back members must not be visible"
    );

    // RAM == replay (§4.4 pt 4, batch edition): the failed batch's hole
    // was checkpointed past; a probe of the same bytes folds to the live
    // state exactly.
    let d_live = digest_walk(&be.trees()).await.unwrap();
    let probe = KvMetaBackend::open_probe(&vol).await.unwrap();
    let d_replay = digest_backend(&probe).await.unwrap();
    assert_eq!(
        d_live, d_replay,
        "live RAM and replay diverged after the whole-batch rollback"
    );

    // Conservation: no admission leak, no watermark wedge, volume alive.
    assert_eq!(be.journal_ring().core().admitted(), 0);
    assert!(be.journal_ring().completed_upto() >= be.journal_ring().core().head());
    routed
        .create(parent, "post-race", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("volume must serve after the rolled-back batch");
}

/// M7 crash case: replay-twice digest equality under BATCHED commits — a
/// concurrent storm (real group formation), then two independent probes
/// of the same bytes must fold to identical digests, both equal to the
/// live RAM state (replay idempotence, §4.10, batch edition).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_kv_batched_commits_replay_twice_digest_equal() {
    use squeezefs::meta_backend::kv::backend::KvMetaBackend;
    use squeezefs::meta_backend::kv::builder::{digest_walk, format_v3, FormatV3Options};
    use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};

    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let dir = tempfile::tempdir().unwrap();
    let vol = dir.path().join("batch-replay.v3.meta");
    std::fs::File::create(&vol)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    format_v3(
        &vol,
        64 * 1024 * 1024,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: Some(1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let be = KvMetaBackend::open(&vol).await.unwrap();
    let routed = std::sync::Arc::new(RoutedMetaBackend::new(vec![be.clone()]));

    // 8-writer storm: creates, renames, unlinks — real arrival
    // concurrency, real batches.
    let mut tasks = Vec::new();
    for w in 0..8u32 {
        let r = routed.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..32u32 {
                let name = format!("w{w}-f{i}");
                r.create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .expect("storm create");
                if i % 3 == 0 {
                    let renamed = format!("w{w}-r{i}");
                    r.rename(1, &name, 1, &renamed, 0).await.expect("rename");
                    r.unlink(1, &renamed).await.expect("unlink");
                }
            }
        }));
    }
    for t in tasks {
        t.await.expect("storm worker");
    }
    // Group formation actually happened (the storm is the point).
    assert!(
        squeezefs::meta_backend::kv::META_COMMIT_GROUP_SIZE
            .snapshot()
            .iter()
            .skip(1)
            .sum::<u64>()
            > 0,
        "the storm must have produced at least one multi-tx batch"
    );

    let d_live = digest_walk(&be.trees()).await.unwrap();
    let p1 = KvMetaBackend::open_probe(&vol).await.unwrap();
    let d1 = digest_backend(&p1).await.unwrap();
    drop(p1);
    let p2 = KvMetaBackend::open_probe(&vol).await.unwrap();
    let d2 = digest_backend(&p2).await.unwrap();
    assert_eq!(d1, d2, "replay must be idempotent (two probes, one digest)");
    assert_eq!(
        d_live, d1,
        "live RAM and replay diverged under batched commits"
    );
}
