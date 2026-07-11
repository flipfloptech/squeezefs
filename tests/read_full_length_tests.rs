//! Regression harness for SHORT-READ-BELOW-EOF — the generic/617 O_DIRECT
//! failure ("uring read bad io length: 32768 instead of 53248").
//!
//! POSIX read(2) on a regular file returns the full requested count unless it
//! crosses EOF; holes read as zeros. A FUSE server must therefore reply with
//! EXACTLY `min(size, file_size - offset)` bytes for every below-EOF read —
//! the daemon, not the kernel, owns hole fill. Buffered I/O masked SqueezeFS's
//! short replies (the page cache zero-fills partially-filled pages), but
//! O_DIRECT (`fsx -Z`, generic/617) propagates them straight to userspace as
//! short reads / bad IO lengths.
//!
//! SqueezeFS's physical layouts legitimately under-cover the logical size:
//!   - a staged ring blob stays short after a truncate-up / extending write
//!     at a far offset (its tail is an implicit-zero hole),
//!   - a striped map leaves interior holes unmapped and its tail block short,
//!   - an inline `data_key` can be shorter than a truncate-up size.
//!
//! The read handler must zero-pad every such gap up to the below-EOF request
//! length — a bounded O(read_len) fill, never a whole-file materialization.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB block size: inline (<= 4 KiB), staged (4 KiB .. 64 KiB),
/// striped (> 64 KiB).
const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "read_full_len_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_0000_0617,
            uuid: *b"read-fulllen-v3!",
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn truncate_to(h: &H, ino: u64, size: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// The pinned contract: a below-EOF read returns EXACTLY
/// `min(size, file_size - offset)` bytes; `expect_data` (at its position
/// relative to `off`) must match; everything else must be zeros.
async fn assert_full_read(
    h: &H,
    ino: u64,
    off: u64,
    size: u32,
    file_size: u64,
    expect: &[(u64, &[u8])], // (file offset, bytes) overlapping the range
    tag: &str,
) {
    let want_len = std::cmp::min(size as u64, file_size - off) as usize;
    let got =
        h.fs.read(h.req, ino, 0, off, size)
            .await
            .unwrap_or_else(|e| panic!("[{tag}] read failed: {e:?}"))
            .data
            .to_vec();
    assert_eq!(
        got.len(),
        want_len,
        "[{tag}] SHORT READ BELOW EOF: got {} of {} requested bytes \
         (offset {off}, file size {file_size}) — the generic/617 O_DIRECT bug",
        got.len(),
        want_len
    );
    // Build the expectation: zeros + the known data spans.
    let mut want = vec![0u8; want_len];
    for (data_off, bytes) in expect {
        let s = (*data_off).max(off);
        let e = (data_off + bytes.len() as u64).min(off + want_len as u64);
        if s < e {
            want[(s - off) as usize..(e - off) as usize]
                .copy_from_slice(&bytes[(s - data_off) as usize..(e - data_off) as usize]);
        }
    }
    if got != want {
        let pos = got
            .iter()
            .zip(want.iter())
            .position(|(a, b)| a != b)
            .unwrap();
        panic!(
            "[{tag}] content mismatch at file offset {}: got {:#x} want {:#x}",
            off + pos as u64,
            got[pos],
            want[pos]
        );
    }
}

/// Staged blob shorter than the logical size (write 16 KiB, truncate up to
/// 48 KiB): a read spanning data + hole tail must return full length.
/// This is the exact generic/617 shape (short read at the blob boundary).
#[tokio::test(flavor = "multi_thread")]
async fn staged_hole_tail_read_full_length() {
    let h = make().await;
    let ino = create(&h, "staged_tail").await;
    let data = vec![b'a'; 16384];
    write_at(&h, ino, 0, &data).await;
    truncate_to(&h, ino, 49152).await; // staged blob stays 16 KiB

    // Read [8192, 8192+32768) — 8 KiB of data then 24 KiB of hole, all
    // below EOF: must be exactly 32768 bytes.
    assert_full_read(&h, ino, 8192, 32768, 49152, &[(0, &data)], "staged-tail").await;
    // Read straddling EOF clamps to EOF, not to the blob end.
    assert_full_read(&h, ino, 16384, 65536, 49152, &[], "staged-tail-eof").await;
}

/// Inline data shorter than a truncate-up size.
#[tokio::test(flavor = "multi_thread")]
async fn inline_hole_tail_read_full_length() {
    let h = make().await;
    let ino = create(&h, "inline_tail").await;
    let data = vec![b'i'; 1024];
    write_at(&h, ino, 0, &data).await;
    truncate_to(&h, ino, 4096).await; // inline data_key stays 1 KiB

    assert_full_read(&h, ino, 0, 4096, 4096, &[(0, &data)], "inline-tail").await;
    assert_full_read(&h, ino, 512, 2048, 4096, &[(0, &data)], "inline-mid").await;
}

/// Striped sparse file: interior unmapped holes and a short tail block.
/// Reads over hole/data boundaries must return full length with zeros.
#[tokio::test(flavor = "multi_thread")]
async fn striped_sparse_read_full_length() {
    let h = make().await;
    let ino = create(&h, "striped_sparse").await;
    let head = vec![b'h'; BS as usize];
    write_at(&h, ino, 0, &head).await; // staged full block
    let far_off = 10 * BS + 4096; // block 10, unaligned
    let far = vec![b'f'; 8192];
    write_at(&h, ino, far_off, &far).await; // promotes sparse striped
    let file_size = far_off + 8192;

    // Hole span crossing mapped->unmapped boundary.
    assert_full_read(
        &h,
        ino,
        BS - 4096,
        16384,
        file_size,
        &[(0, &head)],
        "striped-hole-boundary",
    )
    .await;
    // Pure interior hole read (unmapped block 5).
    assert_full_read(&h, ino, 5 * BS, 32768, file_size, &[], "striped-interior").await;
    // Data at the far end, unaligned within its block.
    assert_full_read(
        &h,
        ino,
        far_off - 4096,
        16384,
        file_size,
        &[(far_off, &far)],
        "striped-far",
    )
    .await;
}

/// A read that begins exactly at EOF returns empty; one past EOF too — the
/// zero-pad must never extend reads BEYOND the logical size.
#[tokio::test(flavor = "multi_thread")]
async fn read_at_and_past_eof_stays_empty() {
    let h = make().await;
    let ino = create(&h, "eof_file").await;
    let data = vec![b'e'; 12345];
    write_at(&h, ino, 0, &data).await;

    for off in [12345u64, 20000u64] {
        let got = h.fs.read(h.req, ino, 0, off, 4096).await.unwrap().data;
        assert!(
            got.is_empty(),
            "read at/past EOF (off {off}) must be empty, got {} bytes",
            got.len()
        );
    }
}
