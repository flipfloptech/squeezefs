use fuse3::raw::prelude::{Filesystem, SetAttr};
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, CONFIG_INODE};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

#[tokio::test]
async fn test_metalv_fuse_integration() {
    let test_id = "metalv_fuse_test";

    let dlm = DlmClient::new().unwrap();

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(test_id)
            .await
            .expect("BlockAllocator initialization failed"),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(
        dlm.clone(),
        cache.clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    );

    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    // Initialize the metadata volume and backend.
    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();

    // Verify it is not formatted initially (blank classification).
    assert!(matches!(
        squeezefs::meta_backend::kv::superblock::classify_volume(&meta_path)
            .await
            .unwrap(),
        squeezefs::meta_backend::kv::superblock::VolumeFormat::Blank
    ));

    let meta_backend = open_v3_meta(&meta_path, 256 * 1024 * 1024).await;

    // Verify it is now formatted (v3 classification).
    assert!(matches!(
        squeezefs::meta_backend::kv::superblock::classify_volume(&meta_path)
            .await
            .unwrap(),
        squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(_)
    ));

    // Register MetaLV backend
    let routed_meta_backend = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed_meta_backend.clone());
    fs.meta_backend = Some(routed_meta_backend);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    // 1. FUSE lookup non-existent: a MISS is a cacheable negative entry
    // (nodeid 0 + negative TTL) since PR M5 (D2.b), not a bare errno —
    // the kernel caches it as a negative dentry.
    let lookup_res = fs
        .lookup(req, 1, OsStr::new("hello.txt"))
        .await
        .expect("miss must be a negative-entry reply");
    assert_eq!(lookup_res.attr.ino, 0, "negative entry encodes nodeid 0");

    // 2. FUSE create file
    let create_res = fs
        .create(req, 1, OsStr::new("hello.txt"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();
    let child_ino = create_res.attr.ino;
    assert!(child_ino > 1);

    // 3. FUSE lookup should now succeed
    let lookup_res2 = fs.lookup(req, 1, OsStr::new("hello.txt")).await.unwrap();
    assert_eq!(lookup_res2.attr.ino, child_ino);
    assert_eq!(lookup_res2.attr.perm, 0o644);

    // 4. FUSE getattr
    let attr_res = fs.getattr(req, child_ino, None, 0).await.unwrap();
    assert_eq!(attr_res.attr.ino, child_ino);

    // 5. FUSE setattr (size truncate)
    let set_attr = SetAttr {
        size: Some(2048),
        ..Default::default()
    };
    let setattr_res = fs.setattr(req, child_ino, None, set_attr).await.unwrap();
    assert_eq!(setattr_res.attr.size, 2048);

    // 6. FUSE mkdir
    let mkdir_res = fs
        .mkdir(req, 1, OsStr::new("my_dir"), 0o755, 0)
        .await
        .unwrap();
    let dir_ino = mkdir_res.attr.ino;
    assert!(dir_ino > child_ino);

    // 7. FUSE readdir
    use futures::StreamExt;
    let reply_dir = fs.readdir(req, 1, 0, 0).await.unwrap();
    let entries: Vec<_> = reply_dir.entries.collect::<Vec<_>>().await;
    let names: Vec<String> = entries
        .into_iter()
        .map(|e| e.unwrap().name.to_string_lossy().to_string())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()));
    assert!(names.contains(&"my_dir".to_string()));

    // 8. FUSE rename
    fs.rename(
        req,
        1,
        OsStr::new("hello.txt"),
        1,
        OsStr::new("hello_renamed.txt"),
    )
    .await
    .unwrap();

    // Verify lookup of old name misses (negative entry since PR M5),
    // and new succeeds
    assert_eq!(
        fs.lookup(req, 1, OsStr::new("hello.txt"))
            .await
            .unwrap()
            .attr
            .ino,
        0
    );
    let lookup_renamed = fs
        .lookup(req, 1, OsStr::new("hello_renamed.txt"))
        .await
        .unwrap();
    assert_eq!(lookup_renamed.attr.ino, child_ino);

    // 9. FUSE xattr checks
    let test_val = b"my_xattr_value";
    fs.setxattr(req, child_ino, OsStr::new("user.test"), test_val, 0, 0)
        .await
        .unwrap();

    let get_sz = fs
        .getxattr(req, child_ino, OsStr::new("user.test"), 0)
        .await
        .unwrap();
    assert_eq!(
        get_sz,
        fuse3::raw::reply::ReplyXAttr::Size(test_val.len() as u32)
    );

    let get_data = fs
        .getxattr(req, child_ino, OsStr::new("user.test"), 64)
        .await
        .unwrap();
    if let fuse3::raw::reply::ReplyXAttr::Data(d) = get_data {
        assert_eq!(&*d, test_val);
    } else {
        panic!("Expected ReplyXAttr::Data");
    }

    let list_res = fs.listxattr(req, child_ino, 64).await.unwrap();
    if let fuse3::raw::reply::ReplyXAttr::Data(d) = list_res {
        let list_str = std::str::from_utf8(&d).unwrap();
        assert!(list_str.contains("user.test"));
    } else {
        panic!("Expected ReplyXAttr::Data in listxattr");
    }

    fs.removexattr(req, child_ino, OsStr::new("user.test"))
        .await
        .unwrap();
    assert!(fs
        .getxattr(req, child_ino, OsStr::new("user.test"), 64)
        .await
        .is_err());

    // 10. FUSE symlink and readlink checks
    let sym_res = fs
        .symlink(
            req,
            1,
            OsStr::new("my_symlink"),
            OsStr::new("hello_renamed.txt"),
        )
        .await
        .unwrap();
    let sym_ino = sym_res.attr.ino;
    assert!(sym_ino > 1);

    let read_res = fs.readlink(req, sym_ino).await.unwrap();
    assert_eq!(&*read_res.data, b"hello_renamed.txt");

    fs.unlink(req, 1, OsStr::new("my_symlink")).await.unwrap();
    assert_eq!(
        fs.lookup(req, 1, OsStr::new("my_symlink"))
            .await
            .unwrap()
            .attr
            .ino,
        0,
        "post-unlink miss is a negative entry (PR M5)"
    );

    // 11. FUSE rmdir
    fs.rmdir(req, 1, OsStr::new("my_dir")).await.unwrap();
    assert_eq!(
        fs.lookup(req, 1, OsStr::new("my_dir"))
            .await
            .unwrap()
            .attr
            .ino,
        0,
        "post-rmdir miss is a negative entry (PR M5)"
    );

    // 10. FUSE unlink the renamed file
    fs.unlink(req, 1, OsStr::new("hello_renamed.txt"))
        .await
        .unwrap();
    assert_eq!(
        fs.lookup(req, 1, OsStr::new("hello_renamed.txt"))
            .await
            .unwrap()
            .attr
            .ino,
        0,
        "post-unlink miss is a negative entry (PR M5)"
    );

    // 12. Read CONFIG_INODE virtual file to verify health values
    let config_sz_attr = fs.getattr(req, CONFIG_INODE, None, 0).await.unwrap();
    let config_len = config_sz_attr.attr.size;
    let config_read = fs
        .read(req, CONFIG_INODE, 0, 0, config_len as u32, 0)
        .await
        .unwrap();
    let config_str = std::str::from_utf8(&config_read.data).unwrap();
    assert!(config_str.contains("\"health\":"));
    assert!(config_str.contains("backend_0"));
    assert!(config_str.contains("meta_volume_0"));
}
