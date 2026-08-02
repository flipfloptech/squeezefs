//! Behavior guards for the small-file (inline) write path.
//!
//! These tests pin the observable contract of the inline layout — byte-exact
//! round trips, read-modify-write patching, and truncate-on-shrink — so that the
//! zero-copy refactor of `CachedMetadata::data_key` (`Vec<u8>` -> `bytes::Bytes`)
//! cannot silently change behavior. They exercise the same `write_file` inline
//! branch, the RMW assemble path, and the `truncate`-on-shrink path that the
//! refactor touches.

use fuse3::raw::prelude::{Filesystem, SetAttr};
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

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

/// Keep temp-file/dir guards alive for the lifetime of the filesystem under test.
struct Harness {
    fs: SqueezefsFilesystem,
    req: Request,
    _backing: NamedTempFile,
    _meta: NamedTempFile,
    _staging: TempDir,
}

async fn make_fs(test_id: &str) -> Harness {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    let dlm = DlmClient::new().unwrap();

    let backing_temp = NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));

    let block_alloc = Arc::new(BlockAllocator::new(test_id).await.unwrap());

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_backend = open_v3_meta(meta_temp.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1234,
    };

    Harness {
        fs,
        req,
        _backing: backing_temp,
        _meta: meta_temp,
        _staging: temp_staging,
    }
}

async fn create_file(h: &Harness, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn read_all(h: &Harness, ino: u64, len: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, 0, len, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Inline writes at a range of sub-block sizes must round-trip byte-exact.
#[tokio::test]
async fn test_inline_small_write_roundtrip() {
    let h = make_fs("zc_roundtrip").await;

    // 1 byte, 100 bytes, 512 bytes (old inline cap), and exactly MAX_INLINE_SIZE.
    for (i, size) in [1usize, 100, 512, 4096].into_iter().enumerate() {
        let ino = create_file(&h, &format!("inline_{i}.bin")).await;
        let payload: Vec<u8> = (0..size).map(|b| (b % 251) as u8).collect();
        let written =
            h.fs.write(
                h.req,
                ino,
                0,
                0,
                bytes::Bytes::copy_from_slice(&payload),
                0,
                0,
            )
            .await
            .unwrap()
            .written;
        assert_eq!(written as usize, size, "short write for size {size}");

        let got = read_all(&h, ino, size as u32).await;
        assert_eq!(got, payload, "inline round-trip mismatch for size {size}");
    }
}

/// Partial overwrite (offset > 0) must patch in place without corrupting the
/// surrounding bytes — this drives the RMW assemble branch in `write_file`.
#[tokio::test]
async fn test_inline_partial_overwrite_rmw() {
    let h = make_fs("zc_rmw").await;
    let ino = create_file(&h, "rmw.bin").await;

    let base = vec![0xAAu8; 200];
    h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&base), 0, 0)
        .await
        .unwrap();

    // Overwrite 20 bytes starting at offset 40.
    let patch = vec![0xBBu8; 20];
    h.fs.write(
        h.req,
        ino,
        0,
        40,
        bytes::Bytes::copy_from_slice(&patch),
        0,
        0,
    )
    .await
    .unwrap();

    let mut expected = base.clone();
    expected[40..60].copy_from_slice(&patch);

    let got = read_all(&h, ino, 200).await;
    assert_eq!(got, expected, "RMW patch corrupted surrounding bytes");
}

/// Truncate-on-shrink (`setattr size`) must drop the tail; this drives the
/// in-place `data.truncate(..)` path on the inline payload.
#[tokio::test]
async fn test_inline_truncate_shrink() {
    let h = make_fs("zc_truncate").await;
    let ino = create_file(&h, "trunc.bin").await;

    let payload: Vec<u8> = (0..200u16).map(|b| (b % 251) as u8).collect();
    h.fs.write(
        h.req,
        ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&payload),
        0,
        0,
    )
    .await
    .unwrap();

    let set_attr = SetAttr {
        size: Some(50),
        ..Default::default()
    };
    let res = h.fs.setattr(h.req, ino, None, set_attr).await.unwrap();
    assert_eq!(res.attr.size, 50, "size not updated by truncate");

    // Reading past the new EOF must return only the surviving 50-byte prefix.
    let got = read_all(&h, ino, 200).await;
    assert_eq!(
        got,
        payload[..50],
        "truncate did not drop the tail correctly"
    );
}
