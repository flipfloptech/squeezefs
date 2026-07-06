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
