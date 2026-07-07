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

/// Regression for the FUSE writeback-cache read/write coherency bug (LTP
/// read04/write03/readv01/mmap02/mmap03/linkat01/symlinkat01, all returned
/// zeros): a read or getattr that misses the opportunistic attr cache must
/// still observe the size of a just-written-but-not-yet-flushed file via the
/// router metadata cache. Otherwise it reads the stale durable inode size (0 on
/// a fresh file), returns a short read, and (under the kernel writeback cache)
/// the kernel caches zero pages — silent read-after-write corruption.
///
/// The `attr_cache.invalidate` calls simulate the real trigger: the kernel
/// issues readahead/getattr that races the write's attr-cache update, or the
/// entry is simply evicted, before the deferred flush persists the size.
#[tokio::test]
async fn test_read_and_getattr_see_size_of_unflushed_write() {
    let h = make().await;
    // inline, staged (block size in this harness is 65536).
    for (idx, &size) in [26usize, 4096, 60000].iter().enumerate() {
        let ino = create(&h, &format!("wb{idx}")).await;
        let data = pattern(size);
        write_at(&h, ino, 0, &data).await;

        // Cold attr cache: only the router metadata cache holds the fresh size.
        h.fs.attr_cache.invalidate(&ino);
        let attr = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
        assert_eq!(
            attr.size, size as u64,
            "size {size}: getattr observed stale size {} after attr-cache eviction",
            attr.size
        );

        h.fs.attr_cache.invalidate(&ino);
        let got = read_at(&h, ino, 0, size as u32).await;
        assert_eq!(
            got.len(),
            size,
            "size {size}: short read ({} bytes) after attr-cache eviction",
            got.len()
        );
        assert_eq!(
            got, data,
            "size {size}: content mismatch after attr-cache eviction"
        );
    }
}

/// Regression for the striped large-file concurrent read/write coherency race:
/// under the kernel writeback cache, readahead reads run concurrently with the
/// still-in-flight write-flushes of a growing (striped) file. A read that
/// observed a not-yet-written block could poison a cache with zeros that then
/// shadowed the correct data even after the writes settled (cp of a >4 MiB file
/// then read-back returned zero blocks; durable data was correct).
///
/// Drives the FUSE handlers directly from concurrent tasks (shared, Clone-backed
/// state) so it reproduces the race without the kernel's flaky timing. Many
/// trials so a regression is caught with high probability; after the fix it must
/// be 100% clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_write_read_striped_no_stale_zeros() {
    let h = make().await;
    let req = h.req;
    let block = 65536usize; // == harness block size (striped above this)
    let size = 2 * 1024 * 1024usize; // 2 MiB => 32 striped blocks

    for trial in 0..40 {
        let ino = create(&h, &format!("cw{trial}")).await;
        let data = std::sync::Arc::new(pattern(size));

        // Seed the file as striped up front so the concurrent writes below all
        // take the striped block path (mirrors the kernel flushing a large file).
        write_at(&h, ino, 0, &vec![0u8; size]).await;

        // Writers: CONCURRENT block-range writes (mirrors the kernel flushing
        // dirty pages of a large file in parallel — the trigger for the
        // non-atomic block_map read-modify-write lost-update race).
        let nblocks = size / block;
        let nwriters = 8usize;
        let mut writers = Vec::new();
        for w in 0..nwriters {
            let fs = h.fs.clone();
            let d = data.clone();
            writers.push(tokio::spawn(async move {
                // Interleave blocks across writers so they hit metadata concurrently.
                let mut b = w;
                while b < nblocks {
                    let off = b * block;
                    let end = std::cmp::min(off + block, size);
                    fs.write(
                        req,
                        ino,
                        0,
                        off as u64,
                        bytes::Bytes::copy_from_slice(&d[off..end]),
                        0,
                        0,
                    )
                    .await
                    .expect("write");
                    b += nwriters;
                    tokio::task::yield_now().await;
                }
            }));
        }

        // Concurrent readers hammering the file while it is being written.
        let mut readers = Vec::new();
        for _ in 0..4 {
            let fs = h.fs.clone();
            readers.push(tokio::spawn(async move {
                for _ in 0..30 {
                    let _ = fs.read(req, ino, 0, 0, size as u32).await;
                    tokio::task::yield_now().await;
                }
            }));
        }

        for wtask in writers {
            wtask.await.unwrap();
        }
        for r in readers {
            r.await.unwrap();
        }

        // After everything settles, the whole file must read back exactly.
        let got = read_at(&h, ino, 0, size as u32).await;
        assert_eq!(got.len(), size, "trial {trial}: short final read");
        if got != *data {
            // Concise per-block diagnosis instead of dumping 2 MiB.
            for b in 0..(size / block) {
                let s = b * block;
                let e = s + block;
                if got[s..e] != data[s..e] {
                    let allzero = got[s..e].iter().all(|&x| x == 0);
                    let matches_other = (0..(size / block))
                        .find(|&ob| ob != b && got[s..e] == data[ob * block..ob * block + block]);
                    // Bisect RAM-shadow vs durable lost-update: dump the cached
                    // map entry, the durable backend layout entry, and whether a
                    // whole-file RAM snapshot exists.
                    let fp = format!("inode_{ino}");
                    let cached_key = h.fs.router.metadata_cache.get(&fp).and_then(|m| {
                        m.block_map
                            .as_ref()
                            .and_then(|bm| bm.get(&(b as u32)).cloned())
                    });
                    // Durable view: drop the cache entry and force a backend refill.
                    h.fs.router.metadata_cache.invalidate(&fp);
                    let durable_key = h.fs.router.fetch_metadata(&fp).await.ok().and_then(|m| {
                        m.block_map
                            .as_ref()
                            .and_then(|bm| bm.get(&(b as u32)).cloned())
                    });
                    let wf_read = h.fs.router.cache.read_lru.get(&fp).map(|d| d.len());
                    let wf_write = h.fs.router.cache.write_lru.get(&fp).map(|d| d.len());
                    panic!(
                        "trial {trial}: block {b} (off {s}) corrupt: all_zero={allzero} \
                         matches_other_block={matches_other:?} cached_key={cached_key:?} \
                         durable_key={durable_key:?} wholefile_read_lru={wf_read:?} \
                         wholefile_write_lru={wf_write:?}"
                    );
                }
            }
        }
    }
}

/// PR 7 (§Observability): the stats inode JSON must expose the sector-lock /
/// allocator / WAL metrics used for rollout gating and live regression alerts.
#[tokio::test]
async fn test_stats_json_exposes_sector_commit_metrics() {
    let h = make().await;
    // Drive one write so the surface reflects a living filesystem.
    let ino = create(&h, "statsprobe").await;
    write_at(&h, ino, 0, &pattern(64)).await;

    let json = h.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("stats JSON must parse");
    let metrics = &v["metrics"];
    for key in [
        "meta_sector_lock_wait_ns",
        "meta_sector_lock_contended",
        "meta_tx_concurrency",
        "meta_tx_concurrency_peak",
        "meta_inode_alloc_cas_retries",
        "meta_inode_alloc_reconciled",
        "meta_quarantined_inodes",
        "meta_commit_sectors",
        "meta_flush_deferred",
    ] {
        assert!(!metrics[key].is_null(), "stats JSON missing metrics.{key}");
    }
}
