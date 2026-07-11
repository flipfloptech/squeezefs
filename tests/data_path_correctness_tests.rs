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
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter, LayoutMetadata};
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

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(path: &std::path::Path, len: u64) -> Arc<KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
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
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
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
async fn test_stats_json_exposes_meta_metrics() {
    let h = make().await;
    // Drive one write so the surface reflects a living filesystem.
    let ino = create(&h, "statsprobe").await;
    write_at(&h, ino, 0, &pattern(64)).await;

    let json = h.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("stats JSON must parse");
    let metrics = &v["metrics"];
    for key in [
        "meta_flush_deferred",
        "meta_reclaim_batch_size",
        "meta_volume_atomicity",
        "meta_volume_atomicity_physical",
        "meta_format_version",
        "meta_kv_journal_entries",
    ] {
        assert!(!metrics[key].is_null(), "stats JSON missing metrics.{key}");
    }

    // Per-volume atomicity fields (resolved OQ 2, design-cow-kv-metadata
    // §4.10): the CONTRACT class is "cow-checksummed" by construction; the
    // PHYSICAL probe is its own field — "unprobed" for harness-constructed
    // backends that never ran the mount probe, the probed class otherwise.
    let atomicity = metrics["meta_volume_atomicity"]
        .as_array()
        .expect("meta_volume_atomicity must be an array (one entry per volume)");
    assert_eq!(atomicity.len(), 1, "harness mounts exactly one meta volume");
    assert_eq!(atomicity[0], "cow-checksummed");
    assert_eq!(metrics["meta_volume_atomicity_physical"][0], "unprobed");
    assert_eq!(
        metrics["meta_format_version"][0], "3",
        "meta_format_version reports the constant \"3\" per volume"
    );
    h.fs.meta_backend.as_ref().unwrap().volumes[0]
        .set_atomicity_physical(squeezefs::meta_backend::atomicity::AtomicityClass::FileBacked);
    let json = h.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("stats JSON must parse");
    assert_eq!(
        v["metrics"]["meta_volume_atomicity_physical"][0], "file-backed",
        "the physical probe must surface alongside the contract class"
    );
    assert_eq!(
        v["metrics"]["meta_volume_atomicity"][0], "cow-checksummed",
        "the contract class holds by construction regardless of the probe"
    );
}

/// Virtual inodes (.stats/.config) must be opened FOPEN_DIRECT_IO: their
/// content is regenerated per open, but the kernel clamps buffered reads to
/// a PREVIOUS generation's i_size (lookup-time attr) — observed as
/// truncated/unparseable stats JSON once the metrics payload grew between
/// generations. DIRECT_IO makes the kernel trust the daemon's read replies
/// (EOF on short read) instead of the stale size.
#[tokio::test]
async fn test_virtual_inodes_open_direct_io() {
    const FOPEN_DIRECT_IO: u32 = 1 << 0;
    let h = make().await;

    let stats =
        h.fs.open(h.req, squeezefs::fuse_client::STATS_INODE, 0)
            .await
            .expect("open .stats");
    assert_ne!(
        stats.flags & FOPEN_DIRECT_IO,
        0,
        ".stats must be FOPEN_DIRECT_IO — buffered reads clamp to a stale i_size"
    );

    let config =
        h.fs.open(h.req, squeezefs::fuse_client::CONFIG_INODE, 0)
            .await
            .expect("open .config");
    assert_ne!(
        config.flags & FOPEN_DIRECT_IO,
        0,
        ".config must be FOPEN_DIRECT_IO — same stale-size clamp"
    );

    // The full fresh generation must be readable through the handler at any
    // offset (the kernel no longer gates it): read past the first 4 KiB.
    let json = h.fs.generate_stats_json().await;
    assert!(json.len() > 4096, "stats JSON is multi-page by now");
    let tail =
        h.fs.read(
            h.req,
            squeezefs::fuse_client::STATS_INODE,
            stats.fh,
            4096,
            1 << 20,
        )
        .await
        .expect("read .stats tail");
    assert!(
        !tail.data.is_empty(),
        "reads beyond the first page must serve the fresh generation"
    );
}

/// PR 2 of docs/design-zero-copy-write-path.md (§5.6 / Observability): the
/// pooled-buffer alignment-contract violation detector
/// (`nvme_unaligned_write_fallbacks`) must be exposed on the stats surface —
/// it is the live regression signal that `write_block`'s zero-copy DMA
/// branch stays guaranteed for pooled sources ("must stay 0" on aligned
/// workloads).
#[tokio::test]
async fn test_stats_surface_exposes_nvme_unaligned_write_fallbacks() {
    let h = make().await;
    let stats: serde_json::Value =
        serde_json::from_str(&h.fs.generate_stats_json().await).expect("stats JSON parses");
    assert!(
        stats["metrics"]["nvme_unaligned_write_fallbacks"].is_u64(),
        "nvme_unaligned_write_fallbacks must be exposed on the stats inode"
    );
}

/// PR 1 of docs/design-zero-copy-write-path.md (§5.2, P0): a zero-copy read
/// reply served from a dirty active-block buffer must stay byte-identical for
/// its whole lifetime, even when the same block is overwritten afterwards.
///
/// Today the merge in `write_file_staged` mutates the *shared* `bytes::Bytes`
/// through a raw pointer (`fuse_client.rs:1383-1390`) while the read path
/// hands out zero-copy slices of the very same buffer (`:2617-2628`) — UB,
/// and observable read-your-own-writes instability: the held reply changes
/// underneath the reader. This test encodes the bug as a failure and pins the
/// CoW contract: writers that find a live snapshot copy first, and the CoW
/// event is observable as the `active_block_cow_copies` stat.
#[tokio::test]
async fn test_active_block_read_snapshot_stable_across_overwrite() {
    let h = make().await;
    let ino = create(&h, "ryw_snap").await;

    // 200_000 bytes @ 64 KiB blocks => striped (first write goes through the
    // router's striped path; no active buffer yet).
    let mut content = pattern(200_000);
    write_at(&h, ino, 0, &content).await;

    // Partial overwrite inside block 3 [196608, 262144): routes through
    // write_file_staged, RMW-seeds an in-RAM active-block buffer, and — being
    // partial — leaves it dirty in active_block_buffers.
    let patch1 = vec![0xABu8; 1024];
    write_at(&h, ino, 196_608 + 512, &patch1).await;
    content[196_608 + 512..196_608 + 512 + 1024].copy_from_slice(&patch1);

    // Hold the zero-copy reply for a range of the dirty block.
    let held =
        h.fs.read(h.req, ino, 0, 196_608, 2048)
            .await
            .expect("read of dirty active block")
            .data;
    let expected = &content[196_608..196_608 + 2048];
    assert_eq!(
        &held[..],
        expected,
        "read returned wrong bytes at hold time"
    );

    // Overwrite an overlapping range of the same (still-dirty) block.
    let patch2 = vec![0xCDu8; 512];
    write_at(&h, ino, 196_608 + 256, &patch2).await;

    // The held reply must not have been mutated underneath us.
    assert_eq!(
        &held[..],
        expected,
        "zero-copy read reply mutated by a later write to the same active \
         block (shared-Bytes in-place mutation)"
    );

    // Read-your-own-writes: a fresh read observes the merged content.
    let mut merged = expected.to_vec();
    merged[256..256 + 512].copy_from_slice(&patch2);
    let after = read_at(&h, ino, 196_608, 2048).await;
    assert_eq!(after, merged, "post-overwrite read must see the merge");

    // The write above collided with our live snapshot, so exactly this path
    // must have paid the copy-on-write — observable on the stats surface.
    let stats: serde_json::Value =
        serde_json::from_str(&h.fs.generate_stats_json().await).expect("stats JSON parses");
    let cow = stats["metrics"]["active_block_cow_copies"]
        .as_u64()
        .expect("active_block_cow_copies stat present");
    assert!(
        cow >= 1,
        "a write colliding with a live snapshot must be counted as a CoW copy"
    );
}

/// Same contract under real concurrency: N readers hold zero-copy replies of
/// the dirty tail block while a writer keeps overwriting it. Every held reply
/// must remain byte-identical to its at-hold copy, and the final read must
/// observe the last write (read-your-own-writes). Coordination is via
/// `Barrier`/`watch` — no sleeps.
#[rstest::rstest]
#[case::two_readers(2, 3)]
#[case::many_readers(8, 5)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_active_block_snapshots_stable_under_concurrent_writers(
    #[case] readers: usize,
    #[case] write_rounds: usize,
) {
    let h = std::sync::Arc::new(make().await);
    let ino = create(&h, "cow_race").await;

    // Striped file, then a partial overwrite inside the tail block so
    // [196608, 200000) becomes a dirty in-RAM active-block buffer.
    write_at(&h, ino, 0, &pattern(200_000)).await;
    let tail_off = 196_608u64;
    let tail_len = 200_000usize - 196_608;
    write_at(&h, ino, tail_off, &vec![0u8; tail_len]).await;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(readers + 1));
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..readers {
        let h = h.clone();
        let barrier = barrier.clone();
        let mut done_rx = done_rx.clone();
        tasks.spawn(async move {
            let held =
                h.fs.read(h.req, ino, 0, tail_off, tail_len as u32)
                    .await
                    .expect("read of dirty active block")
                    .data;
            let at_hold = held.to_vec();
            barrier.wait().await; // writer starts only after every hold
            while !*done_rx.borrow_and_update() {
                done_rx.changed().await.expect("writer done signal");
            }
            assert_eq!(
                &held[..],
                &at_hold[..],
                "held zero-copy read reply mutated by concurrent writes \
                 (shared-Bytes in-place mutation)"
            );
        });
    }

    barrier.wait().await;
    for round in 1..=write_rounds {
        write_at(&h, ino, tail_off, &vec![round as u8; tail_len]).await;
    }
    done_tx.send(true).expect("readers alive");
    while let Some(res) = tasks.join_next().await {
        res.expect("reader task must not panic");
    }

    // Read-your-own-writes: the final content is the last round's pattern.
    let after = read_at(&h, ino, tail_off, tail_len as u32).await;
    assert_eq!(
        after,
        vec![write_rounds as u8; tail_len],
        "final read must observe the last write"
    );
}

// ---------------------------------------------------------------------------
// PR 6 of docs/design-zero-copy-write-path.md (§5.6): full-coverage slice
// reuse in the router's striped per-block task (kills audit #12), plus the
// no-dead-code removal of the subsumed `is_aligned` fast path and the
// provably-unreachable in-handler promotion block. This harness runs
// `block_size = 64 KiB` — the small-block config where the `is_aligned`
// branch IS reachable today — so these tests pin behavior equivalence
// before/after the deletion, and the promotion round-trips pin that
// inline→striped and staged→striped growth stays byte-exact.
// ---------------------------------------------------------------------------

/// RED until PR 6 lands — the slice-reuse contract itself: when a striped
/// write through `DataRouter::write_file` fully covers a block
/// (`rel_start == 0 && rel_end == block_size`), the per-block task must use
/// the payload slice directly as the block bytes instead of copying it into
/// a `PooledBuf` (routing copy, audit #12). Observable without new API: the
/// plaintext block the task caches in the read LRU under the new block key
/// is then a zero-copy slice of the caller's payload allocation — pointer
/// containment proves the copy is gone. Lease-safe by construction: the
/// §5.4 severance boundary guarantees no transport lease ever reaches
/// `DataRouter::write_file`, so retaining the slice retains a private copy.
#[tokio::test]
async fn test_striped_full_coverage_write_reuses_payload_slice() {
    let h = make().await;
    let block = 65536usize;
    let ino = create(&h, "slice_reuse").await;
    let path = format!("inode_{ino}");

    // Stripe the file (4 blocks: 3 full + tail) through the FUSE handler.
    write_at(&h, ino, 0, &pattern(200_000)).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "seed file must be striped");

    // One payload covering blocks 0 and 1 completely, written through the
    // router route the design keeps live (promotions / copy_file_range /
    // stale-cache striped dispatch).
    let payload_vec: Vec<u8> = (0..2 * block).map(|i| ((i % 239) as u8) ^ 0x5A).collect();
    let payload = bytes::Bytes::from(payload_vec);
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    h.fs.router
        .write_file(&path, 0, payload.clone(), token)
        .await
        .expect("router striped write");

    let payload_base = payload.as_ptr() as usize;
    let payload_range = payload_base..payload_base + payload.len();

    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    let block_map = meta.block_map.as_ref().expect("striped block map");
    for b in [0u32, 1u32] {
        let key = block_map
            .get(&b)
            .unwrap_or_else(|| panic!("block {b} missing from block map"));
        let cached =
            h.fs.router
                .cache
                .read_lru
                .get(key)
                .unwrap_or_else(|| panic!("block {b} (key {key}) not in read LRU after write"));
        assert_eq!(
            &cached[..],
            &payload[b as usize * block..(b as usize + 1) * block],
            "block {b}: cached plaintext differs from the payload slice"
        );
        let ptr = cached.as_ptr() as usize;
        assert!(
            payload_range.contains(&ptr),
            "block {b}: full-coverage striped write still copies the payload \
             into a pooled buffer (cached block at {ptr:#x} is outside the \
             payload allocation {payload_range:?}) — §5.6 slice reuse must \
             hand data_slice through as block_bytes"
        );
    }

    // The reuse must not change what readers observe.
    let mut expected = pattern(200_000);
    expected[..2 * block].copy_from_slice(&payload);
    let got = read_at(&h, ino, 0, 200_000).await;
    assert_eq!(got, expected, "content mismatch after slice-reuse write");
}

/// Equivalence pin for the `is_aligned` direct-leg deletion: block-aligned
/// overwrites of a striped file (single-block, multi-block, and a
/// block-boundary append) must produce byte-identical results before and
/// after the branch is removed — post-PR 4, `write_file_staged` handles
/// complete blocks equivalently (write-through) for every config, which is
/// the design's justification for deleting the duplicate striped route.
#[tokio::test]
async fn test_aligned_striped_overwrite_equivalence_64k_blocks() {
    let h = make().await;
    let block = 65536usize;
    let size = 4 * block; // exactly 4 blocks — aligned append lands on EOF
    let ino = create(&h, "aligned_eq").await;

    let mut expected = pattern(size);
    write_at(&h, ino, 0, &expected).await;

    // Single-block aligned overwrite (block 1).
    let one: Vec<u8> = (0..block).map(|i| ((i % 241) as u8) ^ 0xA5).collect();
    write_at(&h, ino, block as u64, &one).await;
    expected[block..2 * block].copy_from_slice(&one);
    assert_eq!(
        read_at(&h, ino, 0, size as u32).await,
        expected,
        "single-block aligned overwrite mismatch"
    );

    // Multi-block aligned overwrite (blocks 2..4).
    let two: Vec<u8> = (0..2 * block).map(|i| ((i % 243) as u8) ^ 0x3C).collect();
    write_at(&h, ino, 2 * block as u64, &two).await;
    expected[2 * block..4 * block].copy_from_slice(&two);
    assert_eq!(
        read_at(&h, ino, 0, size as u32).await,
        expected,
        "multi-block aligned overwrite mismatch"
    );

    // Aligned append at EOF (grows the file by one whole block).
    let grow: Vec<u8> = (0..block).map(|i| ((i % 245) as u8) ^ 0x69).collect();
    write_at(&h, ino, size as u64, &grow).await;
    expected.extend_from_slice(&grow);
    let got = read_at(&h, ino, 0, (size + block) as u32).await;
    assert_eq!(got.len(), size + block, "aligned append: short read");
    assert_eq!(got, expected, "aligned append mismatch");

    // Durable view: drop the hot meta/attr caches and read again.
    h.fs.router
        .metadata_cache
        .invalidate(&format!("inode_{ino}"));
    h.fs.attr_cache.invalidate(&ino);
    assert_eq!(
        read_at(&h, ino, 0, (size + block) as u32).await,
        expected,
        "aligned overwrite/append mismatch after cache invalidation"
    );
}

/// Equivalence pin for the deleted branch's buffer-invalidation duty: an
/// aligned full-block overwrite of a block that holds a DIRTY in-RAM
/// active-block buffer (left by a prior partial write) must supersede that
/// buffer — a later read must see the overwrite, not the stale merge state.
#[tokio::test]
async fn test_aligned_overwrite_supersedes_dirty_active_block() {
    let h = make().await;
    let block = 65536usize;
    let size = 4 * block;
    let ino = create(&h, "aligned_dirty").await;

    let mut expected = pattern(size);
    write_at(&h, ino, 0, &expected).await;

    // Partial write inside block 2 → dirty active-block buffer (no trigger).
    let patch = vec![0xEEu8; 1024];
    write_at(&h, ino, (2 * block + 100) as u64, &patch).await;
    expected[2 * block + 100..2 * block + 100 + patch.len()].copy_from_slice(&patch);
    assert_eq!(
        read_at(&h, ino, 0, size as u32).await,
        expected,
        "partial write not observed"
    );

    // Aligned overwrite of the same (dirty) block.
    let full: Vec<u8> = (0..block).map(|i| ((i % 233) as u8) ^ 0x11).collect();
    write_at(&h, ino, 2 * block as u64, &full).await;
    expected[2 * block..3 * block].copy_from_slice(&full);
    assert_eq!(
        read_at(&h, ino, 0, size as u32).await,
        expected,
        "aligned overwrite of a dirty active block must supersede the buffer"
    );
}

/// Promotion round-trip pin (design PR 6 gate): inline→striped growth must
/// stay byte-exact across the deletion of the in-handler promotion block —
/// which is provably unreachable (`use_router_write` is unconditionally true
/// for `file_type == "inline"`), so promotions resolve inside
/// `DataRouter::write_file` via §5.4 route (i), before and after.
#[tokio::test]
async fn test_inline_to_striped_promotion_roundtrip() {
    let h = make().await;
    let ino = create(&h, "promo_inline").await;
    let path = format!("inode_{ino}");

    // Inline seed (≤ 4096).
    let seed = pattern(3000);
    write_at(&h, ino, 0, &seed).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "inline", "3000-byte file must be inline");

    // Growth write past block_size (64 KiB) → promotes inline → striped.
    let grow: Vec<u8> = (0..150_000).map(|i| ((i % 251) as u8) ^ 0x77).collect();
    write_at(&h, ino, 3000, &grow).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(
        meta.file_type, "striped",
        "growth past block_size must promote inline → striped"
    );

    let mut expected = seed.clone();
    expected.extend_from_slice(&grow);
    assert_eq!(
        read_at(&h, ino, 0, expected.len() as u32).await,
        expected,
        "inline→striped promotion round-trip mismatch"
    );

    // Durable view (cold meta/attr caches).
    h.fs.router.metadata_cache.invalidate(&path);
    h.fs.attr_cache.invalidate(&ino);
    assert_eq!(
        read_at(&h, ino, 0, expected.len() as u32).await,
        expected,
        "inline→striped promotion mismatch after cache invalidation"
    );
}

/// Promotion round-trip pin (design PR 6 gate): staged→striped growth must
/// stay byte-exact across the deletion, same unreachability argument
/// (`use_router_write` is unconditionally true for `file_type == "staged"`).
#[tokio::test]
async fn test_staged_to_striped_promotion_roundtrip() {
    let h = make().await;
    let ino = create(&h, "promo_staged").await;
    let path = format!("inode_{ino}");

    // Staged seed (> 4096 inline cap, ≤ 64 KiB block size).
    let seed = pattern(30_000);
    write_at(&h, ino, 0, &seed).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "staged", "30 KB file must be staged");

    // Growth write past block_size → promotes staged → striped.
    let grow: Vec<u8> = (0..200_000).map(|i| ((i % 249) as u8) ^ 0x88).collect();
    write_at(&h, ino, 30_000, &grow).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(
        meta.file_type, "striped",
        "growth past block_size must promote staged → striped"
    );

    let mut expected = seed.clone();
    expected.extend_from_slice(&grow);
    assert_eq!(
        read_at(&h, ino, 0, expected.len() as u32).await,
        expected,
        "staged→striped promotion round-trip mismatch"
    );

    // Durable view (cold meta/attr caches).
    h.fs.router.metadata_cache.invalidate(&path);
    h.fs.attr_cache.invalidate(&ino);
    assert_eq!(
        read_at(&h, ino, 0, expected.len() as u32).await,
        expected,
        "staged→striped promotion mismatch after cache invalidation"
    );
}

// ---------------------------------------------------------------------------
// Stale-fill hardening under block-key reuse (surfaced by PR 6, pre-existing
// class): block keys are device-offset strings, so a displaced-key free +
// reallocation reuses the SAME key string for a NEW incarnation. Cache
// publishes that cannot prove which incarnation their bytes belong to must
// not stick — otherwise a reader that raced the free serves the dead
// incarnation's bytes (observed as all-zero blocks) until remount.
// ---------------------------------------------------------------------------

/// An NVMe read-tier hit must serve the caller WITHOUT re-promoting the
/// entry into the RAM block LRU: tier-entry provenance under key reuse is
/// unprovable from the incarnation word (the entry may predate a
/// free+realloc whose undo/purge is still in flight), and a stale
/// re-promote is exactly the sticky poison the seqlock exists to prevent.
/// The RAM LRU is filled only by device-validated fills and owners.
#[tokio::test]
async fn test_nvme_tier_hit_does_not_repromote_into_ram_lru() {
    let h = make().await;
    // A key with NO live incarnation bookkeeping (never allocated in this
    // process) — the shape where provenance is least provable.
    let key = "31391744".to_string();
    let payload: Vec<u8> = (0..4096u32).map(|b| (b % 197) as u8).collect();
    h.fs.router
        .cache
        .nvme
        .cache_read_block(&key, bytes::Bytes::copy_from_slice(&payload))
        .expect("seed NVMe read tier");
    assert!(
        h.fs.router.cache.read_lru.get(&key).is_none(),
        "test precondition: RAM LRU cold"
    );

    let served =
        h.fs.router
            .get_cached_or_fetch_block(&key)
            .await
            .expect("NVMe tier hit");
    assert_eq!(&served[..], &payload[..], "tier hit must serve the bytes");

    assert!(
        h.fs.router.cache.read_lru.get(&key).is_none(),
        "NVMe tier hit re-promoted into the RAM LRU — an unprovable-provenance \
         publish that can stick a dead incarnation's bytes under a reused key"
    );
}

/// A no-LRU-put owner (complete-block write-through) taking ownership of a
/// REUSED block key must purge the key's read tiers: a reader fill of the
/// key's dying incarnation can legally publish between the displaced-key
/// purge and the free (the incarnation word is still stable there), and
/// with no owner put to overwrite it, that entry would shadow the new
/// owner's device bytes forever.
#[tokio::test]
async fn test_write_through_reused_key_purges_stale_read_tiers() {
    let h = make().await;
    let block = 65536usize;
    let ino = create(&h, "reuse_purge").await;
    let path = format!("inode_{ino}");

    // Striped file, 4 blocks.
    let base = pattern(4 * block);
    write_at(&h, ino, 0, &base).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    let k1_old = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&1).cloned())
        .expect("block 1 mapped");

    // Full overwrite of block 1 only: displaces and frees block 1's old key
    // — the ONLY free-listed offset now (its replacement key is allocated
    // before the displaced free, so it never comes from the free list).
    let over1: Vec<u8> = (0..block).map(|i| ((i % 239) as u8) ^ 0x21).collect();
    write_at(&h, ino, block as u64, &over1).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    let k1_new = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&1).cloned())
        .expect("block 1 mapped");
    assert_ne!(k1_old, k1_new, "overwrite must publish a fresh key");

    // Simulate the raced reader: the dying incarnation's bytes land in both
    // read tiers under the now-freed key.
    let poison = vec![0u8; block];
    h.fs.router
        .cache
        .read_lru
        .put(&k1_old, bytes::Bytes::copy_from_slice(&poison));
    let _ =
        h.fs.router
            .cache
            .nvme
            .cache_read_block(&k1_old, bytes::Bytes::copy_from_slice(&poison));

    // A block-end-reaching partial write RMW-seeds block 2 and fires the
    // complete-block write-through, whose allocation takes the freed offset
    // — the poisoned key string — as block 2's new key.
    let tail2: Vec<u8> = (0..block / 2).map(|i| ((i % 233) as u8) ^ 0x42).collect();
    write_at(&h, ino, (2 * block + block / 2) as u64, &tail2).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    let k2_new = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&2).cloned())
        .expect("block 2 mapped");
    assert_eq!(
        k2_new, k1_old,
        "test precondition: the allocator must reuse the freed offset \
         (single-entry free list)"
    );

    // The new owner's block must read back as written — not as the dead
    // incarnation's poison shadowing the reused key.
    let mut expected = base.clone();
    expected[block..2 * block].copy_from_slice(&over1);
    let s2 = 2 * block + block / 2;
    expected[s2..s2 + tail2.len()].copy_from_slice(&tail2);
    let got = read_at(&h, ino, (2 * block) as u64, block as u32).await;
    assert_eq!(
        got,
        &expected[2 * block..3 * block],
        "reused block key served the dead incarnation's cached bytes — the \
         no-put write-through owner must purge the key's read tiers"
    );
}

// ---------------------------------------------------------------------------
// `write_file_staged` RMW-seed fill discipline (the follow-up filed in
// 8e3995e): the partial-overwrite seed of an existing striped block is a
// cache FILL like any other — it resolves a block key that a concurrent (or
// even the SAME call's block-completing write-through) displace+free can
// retire and reallocate under the identical key string. Its publishes must
// therefore obey the same validated-fill discipline as
// `get_cached_or_fetch_block`: NVMe-tier hits never re-promote into the RAM
// LRU (entry provenance is unprovable from the incarnation word), and
// device-read fills publish only under a stable, unchanged incarnation
// (publish → revalidate → undo; detached tier publishes re-check).
// ---------------------------------------------------------------------------

/// The RMW seed's NVMe read-tier hit must serve the merge WITHOUT
/// re-promoting the entry into the RAM block LRU — the same unprovable-
/// provenance publish `get_cached_or_fetch_block` deleted (8e3995e): a tier
/// entry may hold a dying incarnation's bytes while an undo/purge is still
/// in flight, and a re-promote launders that transient poison into a sticky
/// RAM entry under a reusable key string.
#[tokio::test]
async fn test_rmw_seed_nvme_tier_hit_does_not_repromote_into_ram_lru() {
    let h = make().await;
    let block = 65536usize;
    let ino = create(&h, "seed_norepromote").await;
    let path = format!("inode_{ino}");

    // Striped file, 2 blocks.
    let base = pattern(2 * block);
    write_at(&h, ino, 0, &base).await;
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    let k1 = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&1).cloned())
        .expect("block 1 mapped");

    // Shape the tiers: block 1 lives ONLY in the NVMe read tier.
    h.fs.router.cache.read_lru.remove(&k1);
    h.fs.router
        .cache
        .nvme
        .cache_read_block(&k1, bytes::Bytes::copy_from_slice(&base[block..2 * block]))
        .expect("seed NVMe read tier");
    assert!(
        h.fs.router.cache.read_lru.get(&k1).is_none(),
        "test precondition: RAM LRU cold for block 1"
    );

    // Partial overwrite INSIDE block 1 (never reaches the block end, so no
    // write-through fires): the seed takes the NVMe-tier hit leg.
    let patch: Vec<u8> = (0..100).map(|i| ((i % 89) as u8) ^ 0x5a).collect();
    write_at(&h, ino, (block + 100) as u64, &patch).await;

    assert!(
        h.fs.router.cache.read_lru.get(&k1).is_none(),
        "RMW seed re-promoted an NVMe-tier entry into the RAM LRU — an \
         unprovable-provenance publish that can stick a dead incarnation's \
         bytes under a reused key"
    );

    // The seed still served the merge: read-your-write across the block.
    let mut expected = base[block..2 * block].to_vec();
    expected[100..200].copy_from_slice(&patch);
    let got = read_at(&h, ino, block as u64, block as u32).await;
    assert_eq!(got, expected, "seeded merge content mismatch");
}

/// A device-read RMW seed whose block key's incarnation is NOT stable at
/// fill time must never publish the bytes into the shared read tiers —
/// publishing would poison the key for its (new) owner. Since the
/// binding-validated serve (the reused-key stale-fill fix), the seed also
/// refuses to USE such unproven bytes at all: a still-mapped key that never
/// settles is indistinguishable from a key mid-reallocation, so the write
/// fails LOUD (EIO after bounded rebind retries) instead of merging user
/// data over bytes that may belong to a dead incarnation. In production the
/// state is transient by protocol — owners publish before merging — so a
/// retry against a settled map succeeds (the heal leg below).
#[tokio::test]
async fn test_rmw_seed_fill_must_not_publish_unstable_incarnation() {
    let h = make().await;
    let block = 65536usize;
    let ino = create(&h, "seed_unstable").await;
    let path = format!("inode_{ino}");

    // Striped file, 2 blocks.
    let base = pattern(2 * block);
    write_at(&h, ino, 0, &base).await;

    // Re-map block 1 to a fresh allocation whose device bytes exist but
    // whose incarnation was never published (an in-flight owner, exactly
    // what a seed racing an owner's allocate→DMA window observes).
    let (_be, allocator, writer) =
        h.fs.router
            .backend_router
            .get_active_backend()
            .expect("backend");
    let dest = allocator.allocate_block().await.expect("allocate");
    let seeded: Vec<u8> = (0..block).map(|i| ((i % 227) as u8) ^ 0x17).collect();
    writer
        .write_block(dest, bytes::Bytes::copy_from_slice(&seeded))
        .await
        .expect("raw device write");
    // Deliberately NO publish_block(dest): the word stays unstable.
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    let entries = [(1u32, dest.to_string())];
    h.fs.router
        .merge_block_mappings(
            ino,
            squeezefs::routing::BlockMapOp::Merge(&entries),
            0,
            squeezefs::routing::LayoutFlip::KeepLayout,
            token,
        )
        .await
        .expect("merge block 1 → in-flight key");

    let dk = dest.to_string();
    h.fs.router.cache.read_lru.remove(&dk);
    h.fs.router.cache.nvme.remove_cached_read_block(&dk);

    // Partial overwrite INSIDE block 1: the seed misses every cache and
    // device-reads the in-flight key. The unproven bytes must be refused
    // loud — not merged, not published.
    let patch: Vec<u8> = (0..100).map(|i| ((i % 97) as u8) ^ 0x33).collect();
    let refused =
        h.fs.write(
            h.req,
            ino,
            0,
            (block + 10) as u64,
            bytes::Bytes::copy_from_slice(&patch),
            0,
            0,
        )
        .await;
    assert!(
        refused.is_err(),
        "RMW seed merged over a never-settling UNSTABLE incarnation instead \
         of failing loud — user data over possibly-dead bytes"
    );

    assert!(
        h.fs.router.cache.read_lru.get(&dk).is_none(),
        "RMW seed published a device read of an UNSTABLE incarnation into \
         the RAM LRU — an unvalidated fill that poisons the key's owner"
    );
    assert!(
        h.fs.router.cache.nvme.get_cached_read_block(&dk).is_none(),
        "RMW seed published a device read of an UNSTABLE incarnation into \
         the NVMe read tier — an unvalidated fill that poisons the key's owner"
    );

    // Sanity: the refused write left OUR in-flight mapping in place (the
    // seed really resolved `dk` and retried against it).
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(
        meta.block_map.as_ref().and_then(|bm| bm.get(&1)),
        Some(&dk),
        "test premise: block 1 must still resolve to the in-flight key"
    );

    // Heal leg: the owner publishes (the transient window closes, as every
    // production owner does before merging) — the same write now seeds from
    // the settled incarnation and succeeds.
    allocator.publish_block(dest);
    write_at(&h, ino, (block + 10) as u64, &patch).await;
    let mut expected = seeded.clone();
    expected[10..110].copy_from_slice(&patch);
    let got = read_at(&h, ino, block as u64, block as u32).await;
    assert_eq!(got, expected, "post-publish seed content mismatch");
}

// ===========================================================================
// PR K8 (design-cow-kv-metadata §5.3): the layout-xattr inline-spill ceiling
// moves from a fixed > 32-entry count (≈128 MiB files) to the target volume's
// per-ino record-value cap (`min(65_536, node_size/4)` on v3, 8_192 on v2)
// minus a 4 KiB framing headroom. The block-map structure, the indirect-block
// mechanism, and every persistence path are UNCHANGED — only the spill
// predicate at the `save_metadata_to_backend` choke point moves, per-volume.
//
// These exercise the change through the backend (the router's public
// writeback flow + the routed backend's `getxattr("layout")`), the mode the
// §5.3 change lives in — mirroring the sibling refcount-clone suite's style.
// ===========================================================================

/// Fixed identity for the deterministic v3 images these tests build.
const K8_SEED: u64 = 0x5CA1_AB1E_0DD5_9111;
const K8_UUID: [u8; 16] = *b"k8-spill-lift!!!";
/// Router block size == the allocator chunk size (4 MiB), so an indirect
/// block is read back whole.
const K8_BLOCK_SIZE: u64 = 4 * 1024 * 1024;

/// A `DataRouter` wired to a single-v3-volume `RoutedMetaBackend` with the
/// given node size. Returns the temp handles that must outlive the router.
async fn v3_spill_router(
    node_size: usize,
    tag: &str,
) -> (
    DataRouter,
    Arc<RoutedMetaBackend>,
    DlmClient,
    NamedTempFile,
    NamedTempFile,
    TempDir,
) {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", K8_BLOCK_SIZE.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), tag)
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
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

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let cfg = BuilderConfig {
        node_size,
        journal_len_override: None,
        hash_seed: K8_SEED,
        uuid: K8_UUID,
    };
    ImageBuilder::new(cfg)
        .unwrap()
        .build(meta.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    router.set_meta_backend(routed.clone());
    (router, routed, dlm, backing, meta, staging)
}

/// Persist a seeded dirty layout while HOLDING a freshly-acquired lease, so
/// the fencing token is current. The process-global "local" DLM fencing map
/// is shared across tests (the FUSE tests in this suite bump it), so a
/// hardcoded token goes stale; acquiring mirrors the production writer path.
async fn persist_under_lease(router: &DataRouter, dlm: &DlmClient, path: &str) {
    let lease = dlm
        .acquire_lock(path, None, std::time::Duration::from_secs(5))
        .await
        .expect("acquire lease");
    router
        .persist_dirty_layout_if_needed(path, lease.fencing_token())
        .await
        .expect("persist layout");
}

/// A dirty striped `CachedMetadata` with `n` distinct block-map entries
/// (valid numeric offsets), ready for `persist_dirty_layout_if_needed`.
fn striped_map_meta(n: usize, block_size: u64) -> CachedMetadata {
    let mut bm = std::collections::HashMap::with_capacity(n);
    for i in 0..n as u32 {
        bm.insert(i, (i as u64 * block_size).to_string());
    }
    CachedMetadata {
        file_type: "striped".to_string(),
        size: n as u64 * block_size,
        block_map: Some(bm),
        layout_dirty: true,
        ..Default::default()
    }
}

async fn mk_striped_file(routed: &RoutedMetaBackend, name: &str) -> u64 {
    routed
        .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino
}

async fn persisted_layout(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr layout")
        .expect("layout xattr present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("deserialize layout")
}

/// Inline: the block map rides in the layout value under the inline sentinel.
fn is_inline(l: &LayoutMetadata) -> bool {
    l.block_map.is_some()
        && l.block_map_id
            .as_deref()
            .is_some_and(|s| s.starts_with("block_map_"))
}
/// Indirect: the map spilled to a block, id points at it, no inline map.
fn is_indirect(l: &LayoutMetadata) -> bool {
    l.block_map.is_none()
        && l.block_map_id
            .as_deref()
            .is_some_and(|s| s.starts_with("indirect:"))
}

/// §5.3 headline: a ≈6 GiB-shaped file (≈1,500 blocks @ 4 MiB) keeps an
/// INLINE block map on a default-node (256 KiB ⇒ 64 KiB cap) v3 volume —
/// the pre-K8 > 32-entry rule would have forced it to an indirect block.
#[tokio::test]
async fn test_v3_six_gib_shaped_file_keeps_inline_map() {
    let (router, routed, dlm, _b, _m, _s) = v3_spill_router(DEFAULT_NODE_SIZE, "k8_six_gib").await;
    let ino = mk_striped_file(&routed, "six_gib_shaped").await;
    let path = format!("inode_{ino}");

    let n = 1500usize; // ≈6 GiB at 4 MiB blocks; ≈30 KiB serialized ≪ 60 KiB cap.
    let meta = striped_map_meta(n, K8_BLOCK_SIZE);
    let expect = meta.block_map.clone().unwrap();
    router.metadata_cache.insert(path.clone(), meta);
    persist_under_lease(&router, &dlm, &path).await;

    let layout = persisted_layout(&routed, ino).await;
    assert!(
        is_inline(&layout),
        "a 6-GiB-shaped ({n}-entry) block map must stay INLINE on v3 \
         (id={:?}, has_map={})",
        layout.block_map_id,
        layout.block_map.is_some(),
    );
    // End-to-end round-trip: every entry preserved inline, byte-exact.
    let got = layout.block_map.unwrap();
    assert_eq!(got.len(), n, "all {n} inline entries preserved");
    assert_eq!(got, expect, "inline block map round-trips exactly");
}

/// §5.3 spill boundary, BOTH directions: inline while under the cap, spill to
/// the (unchanged) indirect block when it grows past it, exact read-back of
/// the already-indirect map, and re-inline on shrink.
#[tokio::test]
async fn test_v3_spill_boundary_roundtrips_both_directions() {
    let (router, routed, dlm, _b, _m, _s) = v3_spill_router(DEFAULT_NODE_SIZE, "k8_boundary").await;
    let ino = mk_striped_file(&routed, "boundary").await;
    let path = format!("inode_{ino}");

    // (a) 200 entries — past the OLD 32-entry rule, ≈4 KiB ≪ 60 KiB — inline.
    router
        .metadata_cache
        .insert(path.clone(), striped_map_meta(200, K8_BLOCK_SIZE));
    persist_under_lease(&router, &dlm, &path).await;
    assert!(
        is_inline(&persisted_layout(&routed, ino).await),
        "200-entry map must stay inline (lifted past the old 32-entry rule)"
    );

    // (b) grow past the cap (5000 entries ≈ 100 KiB) → spills to indirect.
    let big = striped_map_meta(5000, K8_BLOCK_SIZE);
    let big_map = big.block_map.clone().unwrap();
    router.metadata_cache.insert(path.clone(), big);
    persist_under_lease(&router, &dlm, &path).await;
    let grown = persisted_layout(&routed, ino).await;
    assert!(
        is_indirect(&grown),
        "5000-entry map must spill to an indirect block (id={:?})",
        grown.block_map_id
    );

    // (c) read the already-indirect map back — every entry exact. The blob
    // is the versioned backend-true encoding (magic `SQFSIMAP` + LE u32
    // version 1 + bincode `Vec<(u32, String)>` of verbatim key strings —
    // pinned in full by the indirect_map_backend_keys suite).
    let key = grown
        .block_map_id
        .as_deref()
        .unwrap()
        .strip_prefix("indirect:")
        .unwrap();
    let raw = router
        .backend_router
        .read_block(key, K8_BLOCK_SIZE as usize)
        .await
        .expect("read indirect block");
    assert_eq!(&raw[..8], b"SQFSIMAP", "versioned indirect blob magic");
    assert_eq!(
        u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        1,
        "indirect blob header version"
    );
    let entries: Vec<(u32, String)> =
        bincode::deserialize(&raw[12..]).expect("deserialize indirect map");
    assert_eq!(
        entries.len(),
        big_map.len(),
        "indirect map holds every entry"
    );
    let round: std::collections::HashMap<u32, String> = entries.into_iter().collect();
    for (b, s) in &big_map {
        assert_eq!(
            round.get(b),
            Some(s),
            "indirect entry {b} must round-trip verbatim"
        );
    }

    // (d) shrink back under the cap → re-inlines (indirect mechanism unchanged).
    let mut shrunk = striped_map_meta(80, K8_BLOCK_SIZE);
    shrunk.block_map_id = grown.block_map_id.clone(); // let save free the old indirect block
    router.metadata_cache.insert(path.clone(), shrunk);
    persist_under_lease(&router, &dlm, &path).await;
    assert!(
        is_inline(&persisted_layout(&routed, ino).await),
        "shrunk map must re-inline"
    );
}

/// §5.3 per-volume behavior: two v3 volumes with DIFFERENT node sizes in
/// the same routed mount; the SAME block-map shape spills on the small-node
/// volume (64 KiB node ⇒ 16 KiB cap) but stays inline on the default-node
/// volume (256 KiB node ⇒ 64 KiB cap). Routing is pinned via
/// `route_ino`/`make_global_ino`.
#[tokio::test]
async fn test_mixed_node_size_volumes_spill_per_volume() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", K8_BLOCK_SIZE.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "k8_mixed")
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
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

    // Volume 0 = 64 KiB nodes (16 KiB record-value cap); volume 1 =
    // default 256 KiB nodes (64 KiB cap).
    let smallf = NamedTempFile::new().unwrap();
    smallf.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: 64 * 1024,
        journal_len_override: None,
        hash_seed: K8_SEED,
        uuid: *b"k8-small-node!!!",
    })
    .unwrap()
    .build(smallf.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let small_be = KvMetaBackend::open(smallf.path()).await.unwrap();
    let bigf = NamedTempFile::new().unwrap();
    bigf.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: K8_SEED,
        uuid: K8_UUID,
    })
    .unwrap()
    .build(bigf.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let big_be = KvMetaBackend::open(bigf.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![small_be, big_be]));
    router.set_meta_backend(routed.clone());

    // One inode on each arm; compute the routed global ino and pin routing.
    let small_local = routed.volumes[0]
        .create(1, "on_small", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let big_local = routed.volumes[1]
        .create(1, "on_big", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let small_ino = routed.make_global_ino(small_local, 0);
    let big_ino = routed.make_global_ino(big_local, 1);
    assert_eq!(
        routed.route_ino(small_ino),
        (0, small_local),
        "small-node ino routes to volume 0"
    );
    assert_eq!(
        routed.route_ino(big_ino),
        (1, big_local),
        "default-node ino routes to volume 1"
    );

    // §5.3 per-ino cap contract, pinned directly: min(65_536, node_size/4)
    // per volume.
    assert_eq!(
        routed.xattr_value_cap(small_ino),
        16384,
        "64 KiB-node per-ino xattr value cap is node_size/4"
    );
    assert_eq!(
        routed.xattr_value_cap(big_ino),
        65536,
        "default-node per-ino xattr value cap is min(65536, node_size/4)"
    );

    // The SAME 1000-entry shape (≈20 KiB serialized): spills on the
    // 16 KiB-cap volume, stays inline under the 64 KiB cap — §5.3 per
    // volume.
    let n = 1000usize;
    for ino in [small_ino, big_ino] {
        let path = format!("inode_{ino}");
        router
            .metadata_cache
            .insert(path.clone(), striped_map_meta(n, K8_BLOCK_SIZE));
        persist_under_lease(&router, &dlm, &path).await;
    }

    let small_layout = persisted_layout(&routed, small_ino).await;
    let big_layout = persisted_layout(&routed, big_ino).await;
    assert!(
        is_indirect(&small_layout),
        "small-node file must spill past its 16 KiB record cap (id={:?})",
        small_layout.block_map_id
    );
    assert!(
        is_inline(&big_layout),
        "default-node file must stay INLINE to its larger (64 KiB) record cap (id={:?})",
        big_layout.block_map_id
    );

    // Keep temp backing/volumes alive to end of scope.
    let _keep = (backing, smallf, bigf, staging);
}
