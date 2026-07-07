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

use squeezefs::meta_backend::inode::{read_inode, write_inode, DiskInode};
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata};
use squeezefs::uring_fs;
use tempfile::NamedTempFile;

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
/// commit implies the APPLY bytes are durable. RED today (§2.5): the
/// interval-0 barrier fires on the WAL record BEFORE the apply is issued,
/// so the apply bytes are still volatile when the commit returns.
#[tokio::test]
#[ignore = "RED until PR 4: today's interval-0 barrier covers the WAL record, not the apply (design §2.5). PR 4 moves the strict-mode barrier post-apply and flips this test to enforced."]
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

// ---------------------------------------------------------------------------
// Journal-region hygiene.
// ---------------------------------------------------------------------------

/// A full mutation session (create / setxattr / unlink / destroy) leaves the
/// journal region [104 MiB, 108 MiB) byte-identical. RED today by
/// definition: the WAL worker writes a record for every mutation.
/// Strengthens `test_reconciliation_does_not_touch_journal`
/// (meta_lv_tests.rs) from "reconciliation reads don't write" to "NO code
/// path writes" (§4.2).
#[tokio::test]
#[ignore = "RED until PR 4: the WAL worker journals every mutation today. PR 4 deletes the write path and flips this test to enforced."]
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
