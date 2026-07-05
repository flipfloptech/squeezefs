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
        .rename(1, "hello.txt", 1, "world.txt")
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
