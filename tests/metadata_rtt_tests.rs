//! Integration tests for metadata operation caching and RTT reduction.
//!
//! Run with: `cargo test --all-features --test metadata_rtt_tests -- --test-threads=1`

use fuse3::raw::prelude::{Filesystem, SetAttr};
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

fn redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn garnet_available() -> bool {
    let Ok(client) = redis::Client::open(redis_url()) else {
        return false;
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

#[tokio::test]
async fn test_metadata_caching_and_rtt_reduction() {
    if !garnet_available().await {
        println!("Skipping metadata_rtt_tests: Redis/Garnet not available");
        return;
    }

    let test_id = "metadata_rtt_test";
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prefix = format!("{}_{}", test_id, uniq);
    set_fs_prefix(&prefix);
    set_write_verification(false);

    let dlm = DlmClient::new(&redis_url()).unwrap();

    // Format metadata settings in Redis
    {
        let mut con = dlm.meta_client().get_connection().await.unwrap();
        let format_key = format!("{prefix}:format");
        let _: () = redis::cmd("HSET")
            .arg(&format_key)
            .arg("name")
            .arg(&prefix)
            .arg("block_size")
            .arg("4194304") // 4MB block size
            .arg("inodes")
            .arg("1000") // 1000 max inodes limit
            .query_async(&mut con)
            .await
            .unwrap();
    }

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(Arc::new(dlm.meta_client().clone()), &prefix)
            .await
            .unwrap(),
    );
    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();
    let router = DataRouter::new(
        dlm.clone(),
        cache.clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    );
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    // 1. Create directory at parent = 1
    // We expect self.inodes_limit OnceLock to be populated
    assert!(fs.inodes_limit.get().is_none());
    let parent = 1u64;
    let name1 = OsStr::new("dir1");
    let res_mkdir = fs.mkdir(req, parent, name1, 0o755, 0).await.unwrap();
    let dir_ino = res_mkdir.attr.ino;

    // inodes_limit should now be populated
    assert_eq!(*fs.inodes_limit.get().unwrap(), 1000);

    // Verify dir_ino is cached in attr_cache
    let cached_dir = fs.attr_cache.get(&dir_ino).unwrap().0;
    assert_eq!(cached_dir.ino, dir_ino);
    assert_eq!(cached_dir.kind, fuse3::FileType::Directory);

    // 2. Create file in parent = dir_ino
    let name2 = OsStr::new("file1");
    let res_create = fs.create(req, dir_ino, name2, 0o644, 0).await.unwrap();
    let file_ino = res_create.attr.ino;

    // Verify file_ino is cached in attr_cache
    let cached_file = fs.attr_cache.get(&file_ino).unwrap().0;
    assert_eq!(cached_file.ino, file_ino);
    assert_eq!(cached_file.kind, fuse3::FileType::RegularFile);

    // 3. Setattr on file_ino
    let set_attr = SetAttr {
        size: Some(1024),
        ..Default::default()
    };
    let res_setattr = fs.setattr(req, file_ino, None, set_attr).await.unwrap();
    assert_eq!(res_setattr.attr.size, 1024);

    // Verify attr_cache has updated size
    let cached_setattr = fs.attr_cache.get(&file_ino).unwrap().0;
    assert_eq!(cached_setattr.size, 1024);

    // 4. Link file_ino to new link file2
    let name3 = OsStr::new("file2");
    let res_link = fs.link(req, file_ino, dir_ino, name3).await.unwrap();
    assert_eq!(res_link.attr.ino, file_ino);
    assert_eq!(res_link.attr.nlink, 2);

    // Verify attr_cache for file_ino has updated nlink count
    let cached_link = fs.attr_cache.get(&file_ino).unwrap().0;
    assert_eq!(cached_link.nlink, 2);

    // 5. Symlink creation
    let name4 = OsStr::new("sym1");
    let target = OsStr::new("target1");
    let res_symlink = fs.symlink(req, dir_ino, name4, target).await.unwrap();
    let sym_ino = res_symlink.attr.ino;

    // Verify attr_cache for sym_ino
    let cached_sym = fs.attr_cache.get(&sym_ino).unwrap().0;
    assert_eq!(cached_sym.ino, sym_ino);
    assert_eq!(cached_sym.kind, fuse3::FileType::Symlink);
    assert_eq!(cached_sym.size, target.len() as u64);

    // 6. Mknod creation
    let name5 = OsStr::new("mknod1");
    let res_mknod = fs
        .mknod(req, dir_ino, name5, libc::S_IFIFO | 0o644, 0)
        .await
        .unwrap();
    let mknod_ino = res_mknod.attr.ino;

    // Verify attr_cache for mknod_ino
    let cached_mknod = fs.attr_cache.get(&mknod_ino).unwrap().0;
    assert_eq!(cached_mknod.ino, mknod_ino);
    assert_eq!(cached_mknod.kind, fuse3::FileType::NamedPipe);
}
