//! Reproduction harness for the LTP data-path failures (read/write/readv/mmap
//! content mismatches). Exercises write-then-read-back correctness across all
//! three layouts (inline / staged / striped), at full-file and sub-range offsets,
//! and past EOF — the byte-exactness these LTP tests assert.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    // Block size 64 KiB so we cover all three layouts:
    // inline <=4 KiB, staged 4 KiB..64 KiB, striped >64 KiB.
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "dp_test")
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
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(m.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&ms).await.unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(ms)),
    ]));
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

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size)
        .await
        .unwrap()
        .data
        .to_vec()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn test_write_read_roundtrip_all_layouts_and_offsets() {
    let h = make().await;
    // inline, inline-cap, staged-boundary, staged, striped-boundary, striped.
    for (idx, &size) in [1usize, 100, 4096, 5000, 65536, 65537, 200_000]
        .iter()
        .enumerate()
    {
        let ino = create(&h, &format!("f{idx}")).await;
        let data = pattern(size);
        write_at(&h, ino, 0, &data).await;

        // Full read.
        let full = read_at(&h, ino, 0, size as u32).await;
        assert_eq!(full.len(), size, "size {size}: short full read");
        assert_eq!(full, data, "size {size}: full-file content mismatch");

        // Sub-range read at a non-zero offset.
        if size >= 8 {
            let off = (size / 3) as u64;
            let rlen = std::cmp::min(size - off as usize, 1000) as u32;
            let mid = read_at(&h, ino, off, rlen).await;
            assert_eq!(
                mid,
                &data[off as usize..off as usize + rlen as usize],
                "size {size}: sub-range read at off {off} mismatch"
            );
        }

        // Read past EOF returns nothing.
        let past = read_at(&h, ino, size as u64, 100).await;
        assert!(
            past.is_empty(),
            "size {size}: read past EOF returned {} bytes",
            past.len()
        );

        // Read spanning EOF returns only the valid tail.
        if size >= 50 {
            let off = (size - 50) as u64;
            let span = read_at(&h, ino, off, 200).await;
            assert_eq!(
                span,
                &data[off as usize..],
                "size {size}: EOF-spanning read mismatch"
            );
        }
    }
}

/// Build a file with many small, non-block-aligned sequential writes so it grows
/// through all three layouts (inline -> staged -> striped), then read it back
/// whole — the classic LTP "write a file in a loop, verify content" pattern.
#[tokio::test]
async fn test_incremental_append_grows_through_layouts() {
    let h = make().await;
    let ino = create(&h, "grow").await;
    let chunk = 3000usize; // not block-aligned, and > MAX_INLINE across a few writes
    let n = 100usize; // ~300 KiB total: crosses inline(4K) -> staged(64K) -> striped
    let mut all: Vec<u8> = Vec::new();
    for i in 0..n {
        let data: Vec<u8> = (0..chunk).map(|j| ((i * chunk + j) % 251) as u8).collect();
        write_at(&h, ino, (i * chunk) as u64, &data).await;
        all.extend_from_slice(&data);
    }
    let got = read_at(&h, ino, 0, all.len() as u32).await;
    let first_diff = (0..all.len()).find(|&i| got.get(i) != all.get(i));
    assert_eq!(got.len(), all.len(), "incremental append: short read");
    assert_eq!(
        got, all,
        "incremental append: content mismatch (first diff at {first_diff:?})"
    );
}

#[tokio::test]
async fn test_overwrite_then_read() {
    let h = make().await;
    for (idx, &size) in [200usize, 5000, 200_000].iter().enumerate() {
        let ino = create(&h, &format!("ow{idx}")).await;
        write_at(&h, ino, 0, &pattern(size)).await;
        // Overwrite a middle chunk with a distinct pattern.
        let ostart = size / 4;
        let patch: Vec<u8> = (0..size / 4).map(|i| ((i % 13) as u8) ^ 0xF0).collect();
        write_at(&h, ino, ostart as u64, &patch).await;

        let mut expected = pattern(size);
        expected[ostart..ostart + patch.len()].copy_from_slice(&patch);
        let got = read_at(&h, ino, 0, size as u32).await;
        let first_diff = (0..size).find(|&i| got.get(i) != expected.get(i));
        assert_eq!(
            got, expected,
            "size {size}: overwrite RMW content mismatch (first diff at {first_diff:?})"
        );
    }
}

/// Regression for LTP `mmap02` (SIGBUS) / `fchmod` zeroing the size: a
/// metadata-only `setattr` (chmod/chown/utimes — no `size` in the request) on a
/// file whose write is still cached (durable inode lags) must NOT reset the
/// file size. Previously `setattr` read the stale durable inode (size 0) and
/// overwrote the attr cache with it, truncating just-written data to zero.
#[tokio::test]
async fn test_setattr_mode_preserves_size_of_pending_write() {
    let h = make().await;
    let ino = create(&h, "chmodme").await;
    let data = pattern(4096);
    write_at(&h, ino, 0, &data).await;

    // getattr reflects the write and populates the attr cache.
    let before = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        before.size, 4096,
        "size after 4096-byte write should be 4096"
    );

    // chmod 0444 — the request carries no size, so the size must be preserved.
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            mode: Some(0o444),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let after = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
    assert_eq!(
        after.size, 4096,
        "chmod must not change file size (was reset to {})",
        after.size
    );
    assert_eq!(
        after.perm & 0o777,
        0o444,
        "chmod should have applied the mode"
    );

    // Data must still read back intact (mmap02 would SIGBUS on a zero size).
    let got = read_at(&h, ino, 0, 4096).await;
    assert_eq!(got, data, "chmod corrupted just-written file data");
}
