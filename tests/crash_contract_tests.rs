//! Crash-contract tests (design-wal-crash-consistency §4.7, PR 3).
//!
//! Pins TODAY's crash contract (D0/D1, §3) using the deterministic
//! fault-injection shim in `uring_fs` (`nvme_dev.rs` precedent):
//!
//! - **Torn write**: the first write intersecting an armed offset persists
//!   only a prefix, the caller sees `EIO`, and the path is poisoned ("device
//!   died mid-commit") until `clear_faults()`.
//! - **Power cut**: writes since the last `fdatasync` on an armed path are
//!   reverted by `power_cut()` — modeling volatile-cache loss, which neither
//!   kill-9 nor process exit can produce on file-backed volumes.
//!
//! Two tests are deliberately RED today and `#[ignore]`d with a PR 4
//! annotation (review Issue 3): `test_strict_mode_flushes_apply` (the §2.5
//! interval-0 fix) and `test_journal_region_never_written` (red by
//! definition while the WAL worker journals every mutation). PR 4 flips
//! them to enforced. Everything else is green against today's code and must
//! stay green through PR 4–6.
//!
//! The fsync single-barrier accounting suites
//! (`fsync_single_barrier_tests.rs`, `fsync_coalescing_tests.rs`) are the
//! retained-unchanged regression guard for the PR 4 `FORCE_SYNC_TX`
//! deletion — deliberately not duplicated here (§4.3).
//!
//! The trailing `PR K2` section extends the harness to the v3 CoW KV node
//! format (design-cow-kv-metadata §4.5/§4.10): torn tail bsets, the loud
//! positional valid-bset-after-tear classifier, and torn rewrites.
//!
//! The trailing `PR K3` section extends it to the v3 journal ring and root
//! ledger (design-cow-kv-metadata §4.1/§4.6 pt 3/§4.10): torn entries, torn
//! page headers, garbage lengths, holes, torn multi-page middles, torn
//! ledger slots, and the ring-reuse-never-overwrites-the-fallback-window
//! invariant — everything inside the replay window recovers and resyncs,
//! never failing a mount loud.

use squeezefs::meta_backend::inode::{read_inode, write_inode, DiskInode};
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata};
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

const VOL_SIZE: u64 = 256 * 1024 * 1024;
const JOURNAL_START: u64 = 1024 * 1024 * 104;
const JOURNAL_LEN: usize = 1024 * 1024 * 4;

async fn fresh_volume() -> (NamedTempFile, MetaLvStorage) {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), VOL_SIZE).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    (tmp, storage)
}

/// RAII: faults never leak across tests (the shim state is process-global
/// and the suite runs `--test-threads=1`).
struct FaultGuard;
impl Drop for FaultGuard {
    fn drop(&mut self) {
        uring_fs::clear_faults();
    }
}

/// RAII env-var restore for the flush-interval knob(s). Sets BOTH today's
/// name and the PR 4 canonical name so the test stays valid across the
/// rename (legacy alias retained, new name wins — §4.2).
struct StrictModeEnv;
impl StrictModeEnv {
    fn set() -> Self {
        std::env::set_var("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS", "0");
        std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
        StrictModeEnv
    }
}
impl Drop for StrictModeEnv {
    fn drop(&mut self) {
        std::env::remove_var("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS");
        std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
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
    let (_t, storage) = fresh_volume().await;
    let _g = FaultGuard;

    // A fresh (all-zero) sector far from live metadata.
    let target = 1024 * 1024 * 64;
    let image = vec![0xABu8; 4096];
    uring_fs::arm_torn_write(target, 100);

    let err = storage
        .write_blocks_direct(target, &image)
        .await
        .expect_err("torn write must report EIO to the caller");
    assert_eq!(err.to_errno(), libc::EIO, "torn write maps to EIO: {err}");

    // Device died: subsequent I/O on the same path fails…
    let mut buf = [0u8; 4096];
    assert!(
        storage.read_blocks_direct(target, &mut buf).await.is_err(),
        "reads on a poisoned path must fail"
    );
    assert!(
        storage.write_blocks_direct(target, &image).await.is_err(),
        "writes on a poisoned path must fail"
    );

    // …until the fault is cleared ("device replaced / remount").
    uring_fs::clear_faults();
    storage
        .read_blocks_direct(target, &mut buf)
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
    let (_t, storage) = fresh_volume().await;
    let _g = FaultGuard;

    let target = 1024 * 1024 * 64;
    uring_fs::arm_power_cut(storage.device_path());

    // Unsynced write: lost at the cut.
    storage
        .write_blocks_direct(target, &[0x11u8; 4096])
        .await
        .unwrap();
    let reverted = uring_fs::power_cut(storage.device_path());
    assert!(reverted >= 1, "the unsynced write must be reverted");
    let mut buf = [0u8; 4096];
    storage.read_blocks_direct(target, &mut buf).await.unwrap();
    assert_eq!(
        buf, [0u8; 4096],
        "unsynced bytes must not survive a power cut"
    );

    // Synced write: survives the cut.
    uring_fs::arm_power_cut(storage.device_path());
    storage
        .write_blocks_direct(target, &[0x22u8; 4096])
        .await
        .unwrap();
    uring_fs::fdatasync(storage.device_path().to_path_buf())
        .await
        .unwrap();
    let _ = uring_fs::power_cut(storage.device_path());
    storage.read_blocks_direct(target, &mut buf).await.unwrap();
    assert_eq!(
        buf, [0x22u8; 4096],
        "fdatasync'd bytes must survive a power cut"
    );
}

// ---------------------------------------------------------------------------
// D1: torn-apply detection without amplification.
// ---------------------------------------------------------------------------

/// With a torn inode-table sector: victim slots read as invalid (magic
/// zeroed ⇒ absent), fully-landed sibling slots in the SAME sector remain
/// valid, other sectors are untouched, and mount reconciliation (allocator
/// seed + bitmap refresh) completes — corruption does not spread (the
/// `352d776` per-sector consistency class, across a crash).
#[tokio::test]
async fn test_torn_apply_detected_not_amplified() {
    let (tmp, storage) = fresh_volume().await;
    let _g = FaultGuard;

    // Live metadata in sector 8192 (inos 0–15): root + one keeper file.
    let backend = MetaLvBackend::new(storage);
    let keeper = backend
        .create(1, "keeper", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // Land ino 16 normally in the fresh sector 12288 (inos 16–31)…
    write_inode(
        &backend.storage,
        16,
        &DiskInode::new(16, libc::S_IFREG | 0o644, 0, 0),
    )
    .await
    .unwrap();

    // …then tear ino 17's slot write after 300 bytes of the whole-sector
    // RMW image: ino 16's slot (bytes 0..256) fully lands again, ino 17's
    // slot gets 44 bytes (ino/size/time fields) and its magic (in-slot
    // offset 48, absolute 304) never lands.
    uring_fs::arm_torn_write(12288, 300);
    let err = write_inode(
        &backend.storage,
        17,
        &DiskInode::new(17, libc::S_IFREG | 0o644, 0, 0),
    )
    .await
    .expect_err("the torn apply must fail loud");
    assert_eq!(err.to_errno(), libc::EIO);
    drop(backend);

    // "Remount": fresh storage over the same bytes, faults cleared.
    uring_fs::clear_faults();
    let storage = MetaLvStorage::open(tmp.path(), VOL_SIZE).unwrap();
    storage
        .validate_superblock()
        .await
        .expect("superblock is outside the torn sector — must validate");

    // Victim slot: detectable-absent, not garbage.
    assert!(
        read_inode(&storage, 17).await.is_err(),
        "the torn slot (magic never landed) must read as invalid/absent"
    );
    // Sibling slot in the SAME torn sector, fully landed: valid.
    let survivor = read_inode(&storage, 16)
        .await
        .expect("fully-landed sibling");
    assert_eq!(survivor.ino, 16);
    // Other sectors: unaffected.
    let root = read_inode(&storage, 1).await.expect("root untouched");
    assert_eq!(root.ino, 1);
    let kept = read_inode(&storage, keeper.ino)
        .await
        .expect("keeper untouched");
    assert_eq!(kept.ino, keeper.ino);

    // Mount reconciliation completes and seeds exactly the valid slots.
    storage.seed_inode_alloc_from_table().await.unwrap();
    storage.refresh_bitmap_from_table().await.unwrap();
    assert!(storage.inode_alloc.is_set(16), "landed slot seeds");
    assert!(
        !storage.inode_alloc.is_set(17),
        "torn slot must NOT seed (invalid magic is skipped, never propagated)"
    );
    assert!(storage.inode_alloc.is_set(keeper.ino));
}

// ---------------------------------------------------------------------------
// D0: acked durability (fsync barrier covers everything applied before it).
// ---------------------------------------------------------------------------

/// An op acked by a device barrier (the fsync path's trailing
/// `sync_device`) survives a power cut. Green today: applies are written
/// in-place before the coalesced barrier fires.
#[tokio::test]
async fn test_acked_fsync_survives_power_cut() {
    let (tmp, storage) = fresh_volume().await;
    let _g = FaultGuard;

    uring_fs::arm_power_cut(storage.device_path());
    let backend = MetaLvBackend::new(storage);
    let f = backend
        .create(1, "acked", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    // The fsync guarantee: one trailing coalesced barrier (§2.3).
    backend.sync_device().await.expect("fsync barrier");

    let reverted = uring_fs::power_cut(backend.storage.device_path());
    assert_eq!(
        reverted, 0,
        "everything written before the ack barrier must already be durable"
    );
    drop(backend);
    uring_fs::clear_faults();

    let storage = MetaLvStorage::open(tmp.path(), VOL_SIZE).unwrap();
    let backend = MetaLvBackend::new(storage);
    let found = backend
        .lookup(1, "acked")
        .await
        .expect("fsync-acked create must survive a power cut");
    assert_eq!(found.ino, f.ino);
}

/// Strict mode (`…_FLUSH_INTERVAL_MS=0`, "sync-on-commit"): a returned
/// commit implies the APPLY bytes are durable. Was RED before PR 4 (§2.5):
/// the interval-0 barrier fired on the WAL record BEFORE the apply was
/// issued; PR 4 moved the strict-mode barrier post-apply — enforced since.
#[tokio::test]
async fn test_strict_mode_flushes_apply() {
    let _env = StrictModeEnv::set();
    let (tmp, storage) = fresh_volume().await;
    let _g = FaultGuard;

    uring_fs::arm_power_cut(storage.device_path());
    let backend = MetaLvBackend::new(storage);
    backend
        .create(1, "strict", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("strict-mode commit");
    // Commit returned ⇒ under sync-on-commit the apply must be durable NOW.
    let _ = uring_fs::power_cut(backend.storage.device_path());
    drop(backend);
    uring_fs::clear_faults();

    let storage = MetaLvStorage::open(tmp.path(), VOL_SIZE).unwrap();
    let backend = MetaLvBackend::new(storage);
    backend
        .lookup(1, "strict")
        .await
        .expect("interval-0 commit returned ⇒ the op must survive a power cut");
}

/// Deferred mode (the default): commits set the flush flag and the
/// per-volume flusher barriers the device within the interval — an op is
/// power-cut durable once `meta_flush_deferred` shows the timer fired
/// (§4.2). Bounded poll on the metric, no sleep-for-sync.
#[tokio::test]
async fn test_deferred_flush_window_barriers_applies() {
    use std::sync::atomic::Ordering;

    struct DeferredEnv;
    impl DeferredEnv {
        fn set() -> Self {
            std::env::set_var("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS", "20");
            std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "20");
            DeferredEnv
        }
    }
    impl Drop for DeferredEnv {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS");
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }

    let _env = DeferredEnv::set();
    let (tmp, storage) = fresh_volume().await;
    let _g = FaultGuard;

    uring_fs::arm_power_cut(storage.device_path());
    let backend = MetaLvBackend::new(storage);
    let before = squeezefs::fuse_client::METRICS
        .meta_flush_deferred
        .load(Ordering::Relaxed);
    backend
        .create(1, "deferred", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();

    // Wait (bounded) for the flusher's barrier, observable via the metric.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while squeezefs::fuse_client::METRICS
        .meta_flush_deferred
        .load(Ordering::Relaxed)
        == before
    {
        assert!(
            std::time::Instant::now() < deadline,
            "deferred flusher never barriered within the window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let reverted = uring_fs::power_cut(backend.storage.device_path());
    assert_eq!(
        reverted, 0,
        "after the deferred barrier fires, the commit's writes must be durable"
    );
    drop(backend);
    uring_fs::clear_faults();

    let storage = MetaLvStorage::open(tmp.path(), VOL_SIZE).unwrap();
    let backend = MetaLvBackend::new(storage);
    backend
        .lookup(1, "deferred")
        .await
        .expect("deferred-mode op must survive a power cut after the flush window");
}

// ---------------------------------------------------------------------------
// Journal-region hygiene.
// ---------------------------------------------------------------------------

/// A full mutation session (create / setxattr / unlink / destroy) leaves the
/// journal region [104 MiB, 108 MiB) byte-identical. Was RED before PR 4
/// (the WAL worker wrote a record for every mutation); the write path is
/// deleted — the region stays declared/reserved on disk (§4.2, R1).
/// Strengthens `test_reconciliation_does_not_touch_journal`
/// (meta_lv_tests.rs) from "reconciliation reads don't write" to "NO code
/// path writes" (§4.2).
#[tokio::test]
async fn test_journal_region_never_written() {
    let (_t, storage) = fresh_volume().await;

    let mut before = vec![0u8; JOURNAL_LEN];
    storage
        .read_blocks_direct(JOURNAL_START, &mut before)
        .await
        .unwrap();

    let backend = MetaLvBackend::new(storage);
    for i in 0..16 {
        let name = format!("churn{i}");
        let f = backend
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        squeezefs::meta_backend::xattr::set_xattr(&backend.storage, f.ino, "user.k", b"v")
            .await
            .unwrap();
        if i % 2 == 0 {
            backend.unlink(1, &name).await.unwrap();
            backend.destroy_inode(f.ino).await.unwrap();
        }
    }
    backend.sync_device().await.unwrap();

    let mut after = vec![0u8; JOURNAL_LEN];
    backend
        .storage
        .read_blocks_direct(JOURNAL_START, &mut after)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "a mutation session must leave the journal region byte-identical"
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
    // recycled-extent garbage, structurally expected — never loud.
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
