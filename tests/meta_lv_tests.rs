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
