use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata};
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_metalv_format_and_mount() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();

    // Size limit of 32MB
    let storage = MetaLvStorage::open(&path, 32 * 1024 * 1024).unwrap();

    // Format
    MetaLvBackend::format(&storage).unwrap();

    // Verify Superblock
    let sb = storage.read_superblock().unwrap();
    assert_eq!(&sb.magic, squeezefs::meta_backend::storage::MAGIC_VALUE);
    assert_eq!(sb.version, 1);

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
    let storage = MetaLvStorage::open(&path, 32 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&storage).unwrap();
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
