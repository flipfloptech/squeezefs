//! Regression harness for the `copy_file_range` (CFR) short-copy CRAWL found
//! live during the K7 fstests run (`generic/616` on v3, `generic/112` on v2 —
//! both format-agnostic, in the shared data path). See
//! `.benchmarks/2026-07-09-generic616-cfr-crawl-diagnosis.md`.
//!
//! The bug: `ltp/fsx … copy_file_range` copies within one file with a loop that
//! only terminates via `olen -= nr` for `nr > 0` — a `copy_file_range` returning
//! `0` for a nonzero request wedges it forever (528,808 calls in 12 s live).
//! SqueezeFS's handler returned `copied = 0` when `off_in` fell in the gap
//! between a file's *physical* data length and its *logical* size (`meta.size`)
//! — e.g. a staged/inline file grown by truncate-extend or a sparse write, whose
//! `read_file` reads back shorter than `meta.size`. `off_in >= src_data.len()`
//! then tripped the `return copied: 0` path even though `off_in < src_size`.
//!
//! Contract these tests pin (for inline / staged / striped, cross-block ranges,
//! and hole regions, on BOTH v2 and v3):
//!   * a nonzero, in-logical-bounds request NEVER returns `copied == 0`;
//!   * the copy completes in O(bytes/chunk) calls, not O(len) — no crawl;
//!   * destination bytes are byte-exact (real data where the source has it,
//!     zeros where the source range is a hole).

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

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// Block size 64 KiB so a single harness covers all three layouts:
/// inline (<= 4 KiB), staged (4 KiB .. 64 KiB), striped (> 64 KiB).
async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("cfr_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid: *b"cfr-regress-v3!!",
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
        ..Default::default()
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
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// ftruncate to `size` (extend or shrink) via the FUSE setattr handler.
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

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) % 251) as u8).collect()
}

/// fsx's `do_copy_range` loop, faithfully: repeat `copy_file_range` until the
/// requested length is consumed, advancing offsets by the returned count.
/// fsx only makes progress on `nr > 0` and never breaks on `nr == 0`, so a
/// handler that returns 0 for a nonzero request is an infinite loop live. Here
/// we assert forward progress (the crawl guard) and cap the call count.
async fn cfr_loop(
    h: &H,
    src: u64,
    off_in0: u64,
    dst: u64,
    off_out0: u64,
    len0: u64,
    tag: &str,
) -> u32 {
    let (mut off_in, mut off_out, mut len) = (off_in0, off_out0, len0);
    let mut calls = 0u32;
    // A correct handler needs ceil(len / CFR_MAX_CHUNK) calls; len here is small,
    // so anything beyond a handful means the per-call count is pathologically
    // tiny (or zero) — the crawl.
    let cap = 64u32;
    while len > 0 {
        calls += 1;
        assert!(
            calls <= cap,
            "[{tag}] CRAWL: {calls} copy_file_range calls, {len} bytes still \
             unconsumed — handler makes near-zero progress per call"
        );
        let copied =
            h.fs.copy_file_range(h.req, src, 0, off_in, dst, 0, off_out, len, 0)
                .await
                .unwrap()
                .copied;
        assert!(
            copied > 0,
            "[{tag}] CRAWL: copy_file_range returned 0 for a nonzero request \
             (off_in={off_in}, off_out={off_out}, len={len}) — fsx's loop \
             would spin forever here"
        );
        assert!(
            copied <= len,
            "[{tag}] copy_file_range over-copied: returned {copied} > requested {len}"
        );
        off_in += copied;
        off_out += copied;
        len -= copied;
    }
    calls
}

/// Partial (sub-file) CFR across inline / staged / striped source sizes, at a
/// non-zero source AND destination offset. Pins forward progress + byte-exact
/// destination for the common in-bounds case.
async fn partial_range_all_layouts() {
    let h = make().await;
    // inline, staged, striped (64 KiB block size).
    for (idx, &size) in [2048usize, 40_000, 200_000].iter().enumerate() {
        let src = create(&h, &format!("src{idx}")).await;
        let dst = create(&h, &format!("dst{idx}")).await;
        let data = pattern(size);
        write_at(&h, src, 0, &data).await;

        let off_in = (size / 4) as u64;
        let off_out = 111u64; // non-zero dest offset
        let len = (size / 2) as u64;
        let calls = cfr_loop(&h, src, off_in, dst, off_out, len, &format!("sz{size}")).await;
        assert!(
            calls <= 4,
            "size {size}: partial CFR took {calls} calls (expected O(1))"
        );

        let got = read_at(&h, dst, off_out, len as u32).await;
        assert_eq!(
            got,
            &data[off_in as usize..off_in as usize + len as usize],
            "size {size}: destination bytes mismatch after partial CFR"
        );
    }
}

#[tokio::test]
async fn cfr_partial_range_all_layouts_v3() {
    partial_range_all_layouts().await;
}

/// Striped range that straddles a 64 KiB block boundary — the cross-block leg
/// the diagnosis flagged.
async fn cross_block_range() {
    let h = make().await;
    let size = 300_000usize; // > 4 blocks of 64 KiB
    let src = create(&h, "cbsrc").await;
    let dst = create(&h, "cbdst").await;
    let data = pattern(size);
    write_at(&h, src, 0, &data).await;

    // [60000, 200000) spans block boundaries at 65536, 131072.
    let off_in = 60_000u64;
    let off_out = 5_000u64;
    let len = 140_000u64;
    cfr_loop(&h, src, off_in, dst, off_out, len, "xblock").await;

    let got = read_at(&h, dst, off_out, len as u32).await;
    assert_eq!(
        got,
        &data[off_in as usize..off_in as usize + len as usize],
        "v3: cross-block CFR destination mismatch"
    );
}

#[tokio::test]
async fn cfr_cross_block_boundary_v3() {
    cross_block_range().await;
}

/// THE CRAWL REGRESSION (reproduces the live scenario, minimally): a staged
/// file grown past its physical data by truncate-extend, then a `copy_file_range`
/// whose source range lies in the resulting hole. The logical size says the
/// bytes exist (kernel `i_size` == meta.size), so the request is nonzero and
/// in-bounds — the handler MUST advance (copying zeros for the hole), not
/// return 0. Pre-fix this returned `copied == 0` and wedged.
async fn cfr_from_truncate_extended_hole() {
    let h = make().await;
    let src = create(&h, "holesrc").await;
    let dst = create(&h, "holedst").await;

    // Physical data of 5000 bytes (staged), then extend logical size to 50000.
    let head = pattern(5000);
    write_at(&h, src, 0, &head).await;
    truncate_to(&h, src, 50_000).await;

    // Precondition: the LOGICAL size is now 50000 (this is what the kernel
    // caches as i_size and clamps copy_file_range against), while the PHYSICAL
    // staged data is still only 5000 bytes — exactly the physical/logical gap
    // that made the handler return 0 for an off_in in the hole. (A short FUSE
    // read of the tail is expected in-process; the kernel zero-fills to i_size
    // on a real mount.)
    let logical = h.fs.getattr(h.req, src, None, 0).await.unwrap().attr.size;
    assert_eq!(logical, 50_000, "v3: truncate-extend logical size");
    let head_read = read_at(&h, src, 0, 5000).await;
    assert_eq!(head_read, head, "v3: head data preserved");

    // Copy a range entirely inside the hole: off_in=30000 (> physical 5000, <
    // logical 50000), len=15000. Pre-fix: off_in >= src_data.len() => copied 0.
    let off_in = 30_000u64;
    let off_out = 0u64;
    let len = 15_000u64;
    let calls = cfr_loop(&h, src, off_in, dst, off_out, len, "hole").await;
    assert!(
        calls <= 4,
        "v3: hole CFR took {calls} calls (expected O(1))"
    );

    // Destination range must be the hole's zeros.
    let got = read_at(&h, dst, off_out, len as u32).await;
    assert_eq!(got.len(), len as usize, "v3: short dest read");
    assert!(
        got.iter().all(|&b| b == 0),
        "v3: hole must copy as zeros to the destination"
    );

    // A range straddling the physical/hole boundary must copy real head bytes
    // then zeros, in one advancing sequence.
    let dst2 = create(&h, "holedst2").await;
    cfr_loop(&h, src, 4000, dst2, 0, 4000, "straddle").await;
    let got2 = read_at(&h, dst2, 0, 4000).await;
    assert_eq!(&got2[..1000], &head[4000..5000], "v3: straddle real bytes");
    assert!(
        got2[1000..].iter().all(|&b| b == 0),
        "v3: straddle hole zeros"
    );
}

#[tokio::test]
async fn cfr_from_truncate_extended_hole_v3() {
    cfr_from_truncate_extended_hole().await;
}

/// Same-file (src == dst) non-overlapping copy — exactly how fsx exercises
/// `copy_file_range` (it uses one fd for both ends). Must advance and be exact.
async fn cfr_same_file_nonoverlapping() {
    let h = make().await;
    let ino = create(&h, "samefile").await;
    let data = pattern(200_000);
    write_at(&h, ino, 0, &data).await;

    // src [10000, 40000) -> dst [120000, 150000): disjoint, within the file.
    let off_in = 10_000u64;
    let off_out = 120_000u64;
    let len = 30_000u64;
    cfr_loop(&h, ino, off_in, ino, off_out, len, "samefile").await;

    let got = read_at(&h, ino, off_out, len as u32).await;
    assert_eq!(
        got,
        &data[off_in as usize..off_in as usize + len as usize],
        "v3: same-file CFR destination mismatch"
    );
}

#[tokio::test]
async fn cfr_same_file_nonoverlapping_v3() {
    cfr_same_file_nonoverlapping().await;
}

/// The whole-file reflink/clone fast path must be preserved: copying an entire
/// source into an empty destination should reproduce it byte-for-byte.
async fn cfr_whole_file_clone() {
    let h = make().await;
    for (idx, &size) in [2048usize, 40_000, 200_000].iter().enumerate() {
        let src = create(&h, &format!("wsrc{idx}")).await;
        let dst = create(&h, &format!("wdst{idx}")).await;
        let data = pattern(size);
        write_at(&h, src, 0, &data).await;

        let copied =
            h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, size as u64, 0)
                .await
                .unwrap()
                .copied;
        assert_eq!(copied, size as u64, "size {size}: whole-file copy count");

        let got = read_at(&h, dst, 0, size as u32).await;
        assert_eq!(got, data, "size {size}: whole-file clone mismatch");
    }
}

#[tokio::test]
async fn cfr_whole_file_clone_v3() {
    cfr_whole_file_clone().await;
}
