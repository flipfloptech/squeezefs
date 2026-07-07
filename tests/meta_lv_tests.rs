use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata};
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_metalv_format_and_mount() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();

    // Size limit of 256MB
    let storage = MetaLvStorage::open(&path, 256 * 1024 * 1024).unwrap();

    // Format
    MetaLvBackend::format(&storage).await.unwrap();

    // Verify Superblock
    let sb = storage.read_superblock().await.unwrap();
    assert_eq!(&sb.magic, squeezefs::meta_backend::storage::MAGIC_VALUE);
    assert_eq!(sb.version, 2);

    // Mount
    let backend = MetaLvBackend::new(storage);

    // Get root attribute (ino 1)
    let root = backend.getattr(1).await.unwrap();
    assert_eq!(root.ino, 1);
    assert!(root.mode & libc::S_IFDIR != 0);
}

#[tokio::test]
async fn test_seed_inode_alloc_from_table_reconstructs_bits() {
    use squeezefs::meta_backend::inode::{write_inode, DiskInode};

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    // Fresh volume: only root (ino 1, skipped) exists — [2, limit) is empty.
    storage.seed_inode_alloc_from_table().await.unwrap();
    assert_eq!(
        storage.inode_alloc.allocated_count(),
        0,
        "a freshly-formatted volume has no allocatable inodes in use"
    );

    // Persist three inodes at spread-out indices (magic set by DiskInode::new).
    for &ino in &[5u64, 100, 250] {
        write_inode(
            &storage,
            ino,
            &DiskInode::new(ino, libc::S_IFREG | 0o644, 0, 0),
        )
        .await
        .unwrap();
    }

    // Simulate a fresh mount: re-open (new empty allocator) and seed from disk.
    let storage2 = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    storage2.seed_inode_alloc_from_table().await.unwrap();

    assert!(storage2.inode_alloc.is_set(5));
    assert!(storage2.inode_alloc.is_set(100));
    assert!(storage2.inode_alloc.is_set(250));
    assert!(
        !storage2.inode_alloc.is_set(6),
        "an unwritten slot must remain free after seeding"
    );
    assert_eq!(
        storage2.inode_alloc.allocated_count(),
        3,
        "seed must set exactly the magic-valid slots"
    );

    // And a subsequent alloc must skip the seeded-in-use numbers.
    for _ in 0..10 {
        let ino = storage2.inode_alloc.alloc().unwrap();
        assert!(
            ![5u64, 100, 250].contains(&ino),
            "alloc handed out an already-in-use inode {ino}"
        );
    }
}

#[tokio::test]
async fn test_refresh_bitmap_from_table_round_trip() {
    use squeezefs::meta_backend::inode::{write_inode, DiskInode};

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    for &ino in &[5u64, 100, 250] {
        write_inode(
            &storage,
            ino,
            &DiskInode::new(ino, libc::S_IFREG | 0o644, 0, 0),
        )
        .await
        .unwrap();
    }

    // Reconcile the on-disk bitmap from the (now-populated) inode table.
    storage.refresh_bitmap_from_table().await.unwrap();

    // Read the raw bitmap sector (offset 4096) and verify it mirrors the table.
    let mut bm = [0u8; 4096];
    storage.read_blocks_direct(4096, &mut bm).await.unwrap();
    let is_set = |i: usize| bm[i / 8] & (1 << (i % 8)) != 0;
    assert!(
        is_set(0) && is_set(1),
        "reserved(0) + root(1) must be marked"
    );
    for ino in [5usize, 100, 250] {
        assert!(is_set(ino), "in-use inode {ino} must be set in the bitmap");
    }
    assert!(!is_set(6) && !is_set(7), "unused inodes must be clear");

    // `get_allocated_inode_count` is bitmap-derived on the legacy path and
    // allocator-derived when the sector-lock path is on (the default). Seed the
    // in-RAM allocator from the table first, exactly as the mount sequence does,
    // so the count is meaningful under either flag.
    storage.seed_inode_alloc_from_table().await.unwrap();
    let backend = MetaLvBackend::new(storage);
    assert_eq!(
        backend.get_allocated_inode_count().await,
        3,
        "allocated inode count must equal the number of in-use inodes"
    );
}

/// Reconciliation must NOT consume/replay the WAL (design Key Decision 10 /
/// review Issue 15): a marker written into the journal region must survive
/// seed + bitmap reconciliation untouched.
#[tokio::test]
async fn test_reconciliation_does_not_touch_journal() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    // Journal starts at 104 MiB (Superblock::journal_start). Stamp a marker.
    let journal_start: u64 = 1024 * 1024 * 104;
    let mut marker = [0u8; 4096];
    marker[..8].copy_from_slice(b"MARKER!!");
    storage
        .write_blocks_direct(journal_start, &marker)
        .await
        .unwrap();

    storage.seed_inode_alloc_from_table().await.unwrap();
    storage.refresh_bitmap_from_table().await.unwrap();

    let mut after = [0u8; 4096];
    storage
        .read_blocks_direct(journal_start, &mut after)
        .await
        .unwrap();
    assert_eq!(
        &after[..],
        &marker[..],
        "reconciliation must not replay/rewrite the journal"
    );
}

/// Read-your-own-writes across two overlapping sub-sector patches in one
/// transaction: the overlay must merge all intersecting staged patches in stage
/// order (last writer wins per byte). This is the correctness the sub-sector
/// staging in the sector-sharded commit (PR 4) depends on (design §3.4 / Issue 6).
#[tokio::test]
async fn test_tx_read_your_own_writes_subsector() {
    use squeezefs::meta_backend::storage::ACTIVE_TX;

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    // A sector-aligned, zeroed scratch sector inside the inode-table region.
    let sec = 8192u64 + 4096 * 40;
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let buf = ACTIVE_TX
        .scope(tx, async {
            // Patch A: 8 bytes of 0xAA at sec+16.
            storage.write_blocks(sec + 16, &[0xAAu8; 8]).await.unwrap();
            // Patch B: 8 bytes of 0xBB at sec+20 — overlaps A's last 4 bytes.
            storage.write_blocks(sec + 20, &[0xBBu8; 8]).await.unwrap();
            let mut buf = [0u8; 4096];
            storage.read_blocks(sec, &mut buf).await.unwrap();
            buf
        })
        .await;

    assert!(
        buf[0..16].iter().all(|&b| b == 0),
        "pre-patch bytes stay zero"
    );
    assert!(
        buf[16..20].iter().all(|&b| b == 0xAA),
        "A's non-overlapped bytes"
    );
    assert!(
        buf[20..28].iter().all(|&b| b == 0xBB),
        "B wins the [20,24) overlap and extends to 28 (stage order)"
    );
    assert!(
        buf[28..].iter().all(|&b| b == 0),
        "post-patch bytes stay zero"
    );
}

/// Regression: full-sector staging (today's transaction_lock path) still reads
/// back correctly through the rewritten overlay.
#[tokio::test]
async fn test_tx_full_sector_staging_overlay_unchanged() {
    use squeezefs::meta_backend::storage::ACTIVE_TX;

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let sec = 8192u64 + 4096 * 41;
    let tx = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let buf = ACTIVE_TX
        .scope(tx, async {
            storage.write_blocks(sec, &[0xCDu8; 4096]).await.unwrap();
            let mut buf = [0u8; 4096];
            storage.read_blocks(sec, &mut buf).await.unwrap();
            buf
        })
        .await;
    assert!(
        buf.iter().all(|&b| b == 0xCD),
        "a staged full-sector image must be read back verbatim"
    );
}

#[tokio::test]
async fn test_metalv_crud_operations() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    let storage = MetaLvStorage::open(&path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    let backend = MetaLvBackend::new(storage);

    // Create a file in root (parent ino 1)
    let file = backend
        .create(1, "hello.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert!(file.ino > 1);

    // Lookup the file
    let found = backend.lookup(1, "hello.txt").await.unwrap();
    assert_eq!(found.ino, file.ino);
    assert_eq!(found.mode, libc::S_IFREG | 0o644);

    // Readdir on root
    let list = backend.readdir(1, 0, 100).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "hello.txt");
    assert_eq!(list[0].ino, file.ino);

    // Setattr size
    let updated = backend
        .setattr(file.ino, None, None, None, Some(1024), None, None, None)
        .await
        .unwrap();
    assert_eq!(updated.size, 1024);

    // Rename the file
    backend
        .rename(1, "hello.txt", 1, "world.txt", 0)
        .await
        .unwrap();

    // Old name lookup should fail
    assert!(backend.lookup(1, "hello.txt").await.is_err());

    // New name lookup should succeed
    let renamed = backend.lookup(1, "world.txt").await.unwrap();
    assert_eq!(renamed.ino, file.ino);

    // Unlink the file
    backend.unlink(1, "world.txt").await.unwrap();

    // Lookup should fail now
    assert!(backend.lookup(1, "world.txt").await.is_err());
}

#[tokio::test]
async fn test_metalv_bitmap_and_hash_chains() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    let storage = MetaLvStorage::open(&path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    let backend = MetaLvBackend::new(storage);

    // 1. Stress the Free Inode Bitmap
    // Allocate 10 files
    let mut inodes = Vec::new();
    for i in 0..10 {
        let name = format!("file_{}.txt", i);
        let f = backend
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inodes.push((name, f.ino));
    }

    // Verify inode numbers are unique
    for i in 0..10 {
        for j in (i + 1)..10 {
            assert_ne!(inodes[i].1, inodes[j].1);
        }
    }

    // Unlink and destroy the even index files
    for i in (0..10).step_by(2) {
        let ino = inodes[i].1;
        backend.unlink(1, &inodes[i].0).await.unwrap();
        backend.destroy_inode(ino).await.unwrap();
    }

    // Re-allocate 5 files and assert they reuse the unlinked/freed inode numbers
    let mut reused = Vec::new();
    for i in 0..5 {
        let name = format!("reused_{}.txt", i);
        let f = backend
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        reused.push(f.ino);
    }

    // Check that at least some reused inodes match previously unlinked ones
    let old_even_inos: Vec<u64> = (0..10).step_by(2).map(|i| inodes[i].1).collect();
    for r_ino in reused {
        assert!(
            old_even_inos.contains(&r_ino),
            "Inodes should be reused from the free list bitmap"
        );
    }

    // 2. Stress dentry hash chains collision logic
    let subdir = backend
        .create(1, "subdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();

    let mut sub_files = Vec::new();
    for i in 0..50 {
        let name = format!("sub_file_{}", i);
        let f = backend
            .create(subdir.ino, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        sub_files.push((name, f.ino));
    }

    // Verify all 50 lookup successfully
    for (name, ino) in &sub_files {
        let found = backend.lookup(subdir.ino, name).await.unwrap();
        assert_eq!(found.ino, *ino);
    }

    // List dentries and verify length is 50
    let list = backend.readdir(subdir.ino, 0, 100).await.unwrap();
    assert_eq!(list.len(), 50);

    // Remove half of them and verify they are missing while the rest are present
    for i in (0..50).step_by(2) {
        backend.unlink(subdir.ino, &sub_files[i].0).await.unwrap();
    }

    for (i, sub_file) in sub_files.iter().enumerate().take(50) {
        let res = backend.lookup(subdir.ino, &sub_file.0).await;
        if i % 2 == 0 {
            assert!(res.is_err());
        } else {
            assert_eq!(res.unwrap().ino, sub_file.1);
        }
    }
}

#[tokio::test]
async fn test_bench_rmdir_simulation() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    let storage = MetaLvStorage::open(&path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    let backend = MetaLvBackend::new(storage);

    let parent = backend
        .create(1, "bench_dir_0", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();

    // Create 100 directories
    for i in 0..100 {
        let name = format!("dir_{}", i);
        backend
            .create(parent.ino, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
    }

    // Verify all 100 can be looked up
    for i in 0..100 {
        let name = format!("dir_{}", i);
        let found = backend.lookup(parent.ino, &name).await.unwrap();
        assert!(found.ino > parent.ino);
    }

    // Delete them one by one
    for i in 0..100 {
        let name = format!("dir_{}", i);
        backend.unlink(parent.ino, &name).await.unwrap();

        // Verify it is gone
        assert!(backend.lookup(parent.ino, &name).await.is_err());

        // Verify the remaining ones are still there
        for j in (i + 1)..100 {
            let rem_name = format!("dir_{}", j);
            let found = backend.lookup(parent.ino, &rem_name).await.unwrap();
            assert!(found.ino > parent.ino);
        }
    }
}

#[tokio::test]
async fn test_bench_rmdir_simulation_concurrent() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    let storage = MetaLvStorage::open(&path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();
    use std::sync::Arc;
    let backend = Arc::new(MetaLvBackend::new(storage));

    let threads = 10;
    let count = 100;

    let mut parents = Vec::new();
    for i in 0..threads {
        let name = format!("bench_dir_{}", i);
        let parent = backend
            .create(1, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
        parents.push(parent);
    }

    let mut handles = Vec::new();
    for parent in &parents {
        let backend = backend.clone();
        let parent_ino = parent.ino;
        handles.push(tokio::spawn(async move {
            // Create 100 directories
            for i in 0..count {
                let name = format!("dir_{}", i);
                backend
                    .create(parent_ino, &name, libc::S_IFDIR | 0o755, 0, 0)
                    .await
                    .unwrap();
            }

            // Verify they exist
            for i in 0..count {
                let name = format!("dir_{}", i);
                let found = backend.lookup(parent_ino, &name).await.unwrap();
                assert!(found.ino > parent_ino);
            }

            // Delete them one by one
            for i in 0..count {
                let name = format!("dir_{}", i);
                if let Err(e) = backend.unlink(parent_ino, &name).await {
                    println!(
                        "[PANIC] Unlink failed: parent_ino = {}, name = {}, error = {:?}",
                        parent_ino, name, e
                    );
                    panic!("unlink failed");
                }

                // Verify it is gone
                assert!(backend.lookup(parent_ino, &name).await.is_err());

                // Verify the remaining ones are still there
                for j in (i + 1)..count {
                    let rem_name = format!("dir_{}", j);
                    let found = backend.lookup(parent_ino, &rem_name).await.unwrap();
                    assert!(found.ino > parent_ino);
                }
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

/// Phase 0 / ESTALE: dual-volume RoutedMetaBackend under concurrent mkdir.
/// Reproduces the production mount shape (2 meta volumes) and the sector-RMW
/// race that previously zeroed sibling inode slots in the same 4 KiB sector.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_routed_concurrent_mkdir_no_lost_inodes() {
    use squeezefs::meta_backend::RoutedMetaBackend;
    use std::sync::Arc;

    let tmp0 = NamedTempFile::new().unwrap();
    let tmp1 = NamedTempFile::new().unwrap();
    let s0 = MetaLvStorage::open(tmp0.path(), 256 * 1024 * 1024).unwrap();
    let s1 = MetaLvStorage::open(tmp1.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&s0).await.unwrap();
    MetaLvBackend::format(&s1).await.unwrap();

    let backend = Arc::new(RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(s0)),
        Arc::new(MetaLvBackend::new(s1)),
    ]));

    let threads = 10usize;
    let count = 100usize;

    let mut parents = Vec::new();
    for i in 0..threads {
        let name = format!("bench_dir_{}", i);
        let parent = backend
            .create(1, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("create parent");
        // Parent must remain getattr-able (not magic-0 / ESTALE class failure).
        let g = backend.getattr(parent.ino).await.expect("getattr parent");
        assert_eq!(g.ino, parent.ino);
        assert!(g.mode & libc::S_IFDIR != 0);
        assert!(g.nlink >= 2);
        parents.push(parent);
    }

    let mut handles = Vec::new();
    for parent in &parents {
        let backend = backend.clone();
        let parent_ino = parent.ino;
        handles.push(tokio::spawn(async move {
            for i in 0..count {
                let name = format!("dir_{}", i);
                let child = backend
                    .create(parent_ino, &name, libc::S_IFDIR | 0o755, 0, 0)
                    .await
                    .unwrap_or_else(|e| panic!("create {} under {}: {:?}", name, parent_ino, e));
                let g = backend.getattr(child.ino).await.unwrap_or_else(|e| {
                    panic!(
                        "getattr child {} (ino {}) after create: {:?}",
                        name, child.ino, e
                    )
                });
                assert_eq!(g.ino, child.ino);
                assert!(g.mode & libc::S_IFDIR != 0, "child {} not a dir", name);
                assert!(g.nlink >= 2, "child {} nlink={}", name, g.nlink);
            }

            // Parent still valid after concurrent child creates.
            let parent_g = backend.getattr(parent_ino).await.expect("parent after");
            assert!(parent_g.mode & libc::S_IFDIR != 0);
            assert!(
                parent_g.nlink >= 2 + count as u32,
                "parent nlink {} expected >= {}",
                parent_g.nlink,
                2 + count
            );

            for i in 0..count {
                let name = format!("dir_{}", i);
                let found = backend
                    .lookup(parent_ino, &name)
                    .await
                    .unwrap_or_else(|e| panic!("lookup {}: {:?}", name, e));
                let g = backend
                    .getattr(found.ino)
                    .await
                    .unwrap_or_else(|e| panic!("getattr after lookup {}: {:?}", name, e));
                assert_eq!(g.ino, found.ino);
            }
        }));
    }

    for h in handles {
        h.await.expect("join task");
    }
}

// ---------------------------------------------------------------------------
// PR 1 (design-wal-crash-consistency §PR 1, Key Decision 7, resolved OQ 4):
// mount-time superblock validation — fail loud on unknown format — plus a
// real xxh3_64 checksum in the (previously always-zero) `checksum` field.
//
// Superblock byte layout (repr(C), 56 bytes, no padding):
//   magic 0..8 | version 8..12 | inode_count 12..16 |
//   free_inode_bitmap_root 16..24 | dentry_root 24..32 |
//   journal_start 32..40 | journal_size 40..48 | checksum 48..56
// ---------------------------------------------------------------------------

const SB_VERSION_OFF: usize = 8;
const SB_DENTRY_ROOT_OFF: usize = 24;
const SB_CHECKSUM_OFF: usize = 48;

async fn read_sector0(storage: &MetaLvStorage) -> [u8; 4096] {
    let mut buf = [0u8; 4096];
    storage.read_blocks_direct(0, &mut buf).await.unwrap();
    buf
}

/// Fresh format ⇒ `validate_superblock` passes, returns the parsed superblock
/// (magic + version 2), and the on-disk `checksum` field is a real (nonzero)
/// value — the formatted-then-mounted round-trip, including across a re-open.
#[tokio::test]
async fn test_validate_superblock_fresh_format_round_trip() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let sb = storage
        .validate_superblock()
        .await
        .expect("freshly formatted volume must validate");
    assert_eq!(&sb.magic, squeezefs::meta_backend::storage::MAGIC_VALUE);
    assert_eq!(sb.version, 2, "format writes version 2");
    assert_ne!(
        sb.checksum, 0,
        "PR 1: format must persist a real checksum, not the legacy 0"
    );

    // Simulated remount: a fresh MetaLvStorage over the same bytes validates.
    let storage2 = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    let sb2 = storage2
        .validate_superblock()
        .await
        .expect("remount of a valid volume must validate");
    assert_eq!(
        sb2.checksum, sb.checksum,
        "checksum is stable across mounts"
    );
}

/// A blank (auto-created or zero-filled) meta path is NOT silently mounted:
/// validation fails loud with an actionable "not formatted — run `squeezefs
/// format`" error, distinguished from garbage-magic corruption (Key Decision 7,
/// review Issue 7). Covers both the missing-path case (open auto-creates the
/// backing file) and a pre-existing all-zero file.
#[tokio::test]
async fn test_validate_superblock_blank_volume_fails_not_formatted() {
    // Missing path: open() creates the file (create(true) is retained by
    // design — the format flow needs it); validation must still refuse.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("never_formatted.meta");
    let storage = MetaLvStorage::open(&missing, 64 * 1024 * 1024).unwrap();
    let err = storage
        .validate_superblock()
        .await
        .expect_err("a blank auto-created volume must not validate");
    let msg = err.to_string();
    assert!(
        msg.contains("not formatted"),
        "blank volume error must say it is not formatted, got: {msg}"
    );
    assert!(
        msg.contains("squeezefs format"),
        "blank volume error must point at `squeezefs format`, got: {msg}"
    );

    // Pre-existing zero-filled file behaves identically.
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 64 * 1024 * 1024).unwrap();
    let err = storage
        .validate_superblock()
        .await
        .expect_err("an all-zero volume must not validate");
    assert!(
        err.to_string().contains("not formatted"),
        "zero-filled volume must produce the not-formatted error, got: {err}"
    );
}

/// Garbage magic ⇒ validation fails naming the magic (corruption / foreign
/// format is not the same operator error as "you forgot to format").
#[tokio::test]
async fn test_validate_superblock_garbage_magic_names_magic() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let mut sector = read_sector0(&storage).await;
    sector[..8].copy_from_slice(b"GARBAGE!");
    storage.write_blocks_direct(0, &sector).await.unwrap();

    let err = storage
        .validate_superblock()
        .await
        .expect_err("garbage magic must not validate");
    let msg = err.to_string();
    assert!(
        msg.contains("magic"),
        "garbage-magic error must name the magic, got: {msg}"
    );
    assert!(
        !msg.contains("not formatted"),
        "garbage magic is corruption, not the blank-volume case: {msg}"
    );
}

/// A version this binary does not know (99) ⇒ fail loud, naming the version.
/// The version check runs BEFORE checksum verification: a future format bump
/// may change checksum semantics, so the raw-patched (stale-checksum) sector
/// must still report the version as the reason.
#[tokio::test]
async fn test_validate_superblock_future_version_fails() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let mut sector = read_sector0(&storage).await;
    sector[SB_VERSION_OFF..SB_VERSION_OFF + 4].copy_from_slice(&99u32.to_le_bytes());
    storage.write_blocks_direct(0, &sector).await.unwrap();

    let err = storage
        .validate_superblock()
        .await
        .expect_err("version 99 must not validate");
    let msg = err.to_string();
    assert!(
        msg.contains("version") && msg.contains("99"),
        "future-version error must name the version, got: {msg}"
    );
    assert!(
        !msg.contains("checksum"),
        "version must be rejected before checksum verification, got: {msg}"
    );
}

/// A corrupted byte under a nonzero checksum ⇒ validation fails naming the
/// checksum (resolved Open Question 4: verify-if-nonzero).
#[tokio::test]
async fn test_validate_superblock_corrupted_byte_names_checksum() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let mut sector = read_sector0(&storage).await;
    sector[SB_DENTRY_ROOT_OFF] ^= 0xFF; // flip a byte inside dentry_root
    storage.write_blocks_direct(0, &sector).await.unwrap();

    let err = storage
        .validate_superblock()
        .await
        .expect_err("a corrupted superblock byte under a nonzero checksum must not validate");
    assert!(
        err.to_string().contains("checksum"),
        "corruption error must name the checksum, got: {err}"
    );
}

/// A legacy volume (checksum field == 0, as every pre-PR-1 binary wrote) must
/// keep mounting: verification is skipped iff the stored checksum is zero —
/// backward compatible in both directions (Data Model §6).
#[tokio::test]
async fn test_validate_superblock_legacy_zero_checksum_skips_verification() {
    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    // Rewrite the superblock exactly as a pre-PR-1 binary left it: same
    // fields, checksum zeroed (which no longer matches the struct bytes).
    let mut sector = read_sector0(&storage).await;
    sector[SB_CHECKSUM_OFF..SB_CHECKSUM_OFF + 8].copy_from_slice(&0u64.to_le_bytes());
    storage.write_blocks_direct(0, &sector).await.unwrap();

    let sb = storage
        .validate_superblock()
        .await
        .expect("legacy zero-checksum volume must mount with verification skipped");
    assert_eq!(
        sb.checksum, 0,
        "the legacy zero checksum is preserved as-read"
    );
    assert_eq!(sb.version, 2);
}

/// `write_superblock` is the single choke point that stamps the real checksum:
/// writing a struct whose checksum field is 0 (or stale) must land a nonzero,
/// self-consistent value on disk — so every writer (format included) persists
/// a verifiable superblock without each call site computing it.
#[tokio::test]
async fn test_write_superblock_stamps_real_checksum() {
    use squeezefs::meta_backend::storage::Superblock;

    let tmp = NamedTempFile::new().unwrap();
    let storage = MetaLvStorage::open(tmp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).await.unwrap();

    let mut sb = storage.read_superblock().await.unwrap();
    sb.inode_count = 424242; // mutate a field, leave the (now stale) checksum
    storage.write_superblock(&sb).await.unwrap();

    let on_disk = storage
        .validate_superblock()
        .await
        .expect("write_superblock must leave a self-consistent checksum");
    assert_eq!(on_disk.inode_count, 424242, "field mutation persisted");
    assert_ne!(on_disk.checksum, 0, "stamped checksum must be nonzero");
    assert_eq!(
        on_disk.checksum,
        on_disk.compute_checksum(),
        "stored checksum must equal xxh3_64 over the struct bytes with the checksum field zeroed"
    );
    // Type-level pin: compute_checksum is a pure function of the struct.
    let mut copy: Superblock = on_disk;
    copy.dentry_root ^= 1;
    assert_ne!(
        copy.compute_checksum(),
        on_disk.compute_checksum(),
        "any byte change must change the checksum"
    );
}
