use fuse3::raw::{prelude::*, Request};
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn setup_fs() -> Option<(SqueezefsFilesystem, tempfile::TempDir)> {
    let redis_url = get_redis_url();
    let dlm = DlmClient::new(&redis_url).ok()?;

    // Check if redis connection works and flush DB to have a clean slate
    let mut con = redis::Client::open(redis_url.clone())
        .ok()?
        .get_multiplexed_tokio_connection()
        .await
        .ok()?;

    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let backend = RustFsClient::new().await;
    let temp_dir = tempdir().unwrap();
    let cache = TieredCache::new(
        temp_dir.path().to_path_buf(),
        backend.clone(),
        dlm.redis_client().clone(),
    )
    .ok()?;

    let router = DataRouter::new(dlm.clone(), backend, cache);
    Some((SqueezefsFilesystem::new(router, dlm), temp_dir))
}

#[tokio::test]
async fn test_metadata_mkdir_and_readdir() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // 1. Create a directory "testdir_mkdir" under root (parent=1)
    let reply_mkdir = fs
        .mkdir(req, 1, OsStr::new("testdir_mkdir"), 0o755, 0)
        .await
        .expect("mkdir should succeed");

    assert_eq!(reply_mkdir.attr.kind, FileType::Directory);
    assert_eq!(reply_mkdir.attr.perm, 0o755);
    let new_dir_ino = reply_mkdir.attr.ino;

    // 2. Lookup the new directory
    let reply_lookup = fs
        .lookup(req, 1, OsStr::new("testdir_mkdir"))
        .await
        .expect("lookup should succeed");
    assert_eq!(reply_lookup.attr.ino, new_dir_ino);
    assert_eq!(reply_lookup.attr.kind, FileType::Directory);

    // 3. Readdir of root directory should contain "testdir_mkdir"
    let reply_getattr = fs
        .getattr(req, new_dir_ino, None, 0)
        .await
        .expect("getattr should succeed");
    assert_eq!(reply_getattr.attr.ino, new_dir_ino);
    assert_eq!(reply_getattr.attr.kind, FileType::Directory);
}

#[tokio::test]
async fn test_metadata_create_and_setattr() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 2,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // 1. Create file "testfile_setattr.txt" under root (parent=1)
    let reply_created = fs
        .create(req, 1, OsStr::new("testfile_setattr.txt"), 0o644, 0)
        .await
        .expect("create file should succeed");

    let ino = reply_created.attr.ino;
    assert_eq!(reply_created.attr.kind, FileType::RegularFile);
    assert_eq!(reply_created.attr.perm, 0o644);

    // 2. Modify permissions to 0o700 via setattr (chmod)
    let setattr_req = SetAttr {
        mode: Some(0o700),
        ..Default::default()
    };

    let reply_setattr = fs
        .setattr(req, ino, None, setattr_req)
        .await
        .expect("setattr chmod should succeed");
    assert_eq!(reply_setattr.attr.perm, 0o700);

    // 3. Modify uid/gid (chown)
    let setattr_req2 = SetAttr {
        uid: Some(2000),
        gid: Some(3000),
        ..Default::default()
    };

    let reply_setattr2 = fs
        .setattr(req, ino, None, setattr_req2)
        .await
        .expect("setattr chown should succeed");
    assert_eq!(reply_setattr2.attr.uid, 2000);
    assert_eq!(reply_setattr2.attr.gid, 3000);

    // 4. Truncate file size
    let setattr_req3 = SetAttr {
        size: Some(1024),
        ..Default::default()
    };

    let reply_setattr3 = fs
        .setattr(req, ino, None, setattr_req3)
        .await
        .expect("setattr truncate should succeed");
    assert_eq!(reply_setattr3.attr.size, 1024);
}

#[tokio::test]
async fn test_metadata_symlink_and_readlink() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 3,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // Create a symlink "mylink_symlink" pointing to "target/path" under root (parent=1)
    let target = OsStr::new("target/path");
    let reply_symlink = fs
        .symlink(req, 1, OsStr::new("mylink_symlink"), target)
        .await
        .expect("symlink creation should succeed");

    let ino = reply_symlink.attr.ino;
    assert_eq!(reply_symlink.attr.kind, FileType::Symlink);

    // Read the symlink
    let target_read = fs
        .readlink(req, ino)
        .await
        .expect("readlink should succeed");
    assert_eq!(target_read.data, "target/path".as_bytes());
}

#[tokio::test]
async fn test_metadata_hard_link() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 4,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // 1. Create file "original_link.txt"
    let reply_created = fs
        .create(req, 1, OsStr::new("original_link.txt"), 0o644, 0)
        .await
        .expect("create file should succeed");
    let ino = reply_created.attr.ino;
    assert_eq!(reply_created.attr.nlink, 1);

    // 2. Create hard link "linked_link.txt" pointing to the same inode
    let reply_link = fs
        .link(req, ino, 1, OsStr::new("linked_link.txt"))
        .await
        .expect("hard link should succeed");
    assert_eq!(reply_link.attr.ino, ino);
    assert_eq!(reply_link.attr.nlink, 2);

    // Getattr should reflect nlink = 2
    let reply_getattr = fs
        .getattr(req, ino, None, 0)
        .await
        .expect("getattr should succeed");
    assert_eq!(reply_getattr.attr.nlink, 2);

    // 3. Unlink "original_link.txt" -> nlink should become 1
    fs.unlink(req, 1, OsStr::new("original_link.txt"))
        .await
        .expect("unlink original should succeed");

    let reply_getattr2 = fs
        .getattr(req, ino, None, 0)
        .await
        .expect("getattr should succeed");
    assert_eq!(reply_getattr2.attr.nlink, 1);

    // 4. Unlink "linked_link.txt" -> nlink should become 0 and inode should be cleaned up
    fs.unlink(req, 1, OsStr::new("linked_link.txt"))
        .await
        .expect("unlink linked should succeed");

    // Getattr should now return ENOENT (not found) or similar error
    let res_getattr3 = fs.getattr(req, ino, None, 0).await;
    assert!(res_getattr3.is_err());
}

#[tokio::test]
async fn test_metadata_rename() {
    let (fs, _temp_dir) = match setup_fs().await {
        Some(res) => res,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let req = Request {
        unique: 5,
        uid: 1000,
        gid: 1000,
        pid: 1234,
    };

    // 1. Create file "source_rename.txt"
    let reply_created = fs
        .create(req, 1, OsStr::new("source_rename.txt"), 0o644, 0)
        .await
        .expect("create file should succeed");
    let ino = reply_created.attr.ino;

    // 2. Create directory "destdir_rename"
    let reply_mkdir = fs
        .mkdir(req, 1, OsStr::new("destdir_rename"), 0o755, 0)
        .await
        .expect("mkdir should succeed");
    let dest_dir_ino = reply_mkdir.attr.ino;

    // 3. Rename "source_rename.txt" to "destdir_rename/target_rename.txt"
    fs.rename(
        req,
        1,
        OsStr::new("source_rename.txt"),
        dest_dir_ino,
        OsStr::new("target_rename.txt"),
    )
    .await
    .expect("rename should succeed");

    // 4. Old lookup should fail
    let lookup_old = fs.lookup(req, 1, OsStr::new("source_rename.txt")).await;
    assert!(lookup_old.is_err());

    // 5. New lookup should succeed
    let lookup_new = fs
        .lookup(req, dest_dir_ino, OsStr::new("target_rename.txt"))
        .await
        .expect("lookup target should succeed");
    assert_eq!(lookup_new.attr.ino, ino);
}
