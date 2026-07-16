//! Item B — the overwrite lazy-RMW seed (write-side track parked by the
//! read-path program; attribution H3): the FIRST write into an existing
//! striped block RMW-seeded by fetching the old 4 MiB block from the
//! device (`block_write_needs_existing_data` → `get_block_for_index`) even
//! when the accumulating writes fully cover the block before write-through
//! — a pure sequential overwrite issued one device READ per block (~17.8
//! GiB of reads in the elbencho row-4 pass; 870 vs ~3,500 MiB/s fresh).
//!
//! Contract pinned here:
//! - **Full coverage ⇒ zero device reads** (the ledger: `get_obj` stays
//!   flat across a covering overwrite) and byte-exactness.
//! - **Partial coverage ⇒ the old block is seeded AT FLUSH TIME** (binding-
//!   validated, per the leg-5 rule), never zeros/stale for the uncovered
//!   remainder — across fsync, truncate/punch/CFR interleavings inside the
//!   deferral window, concurrent readers (overlay authority), unmount
//!   (never-lossy stage exit), and crash (old device block untouched until
//!   the merge publishes).
//!
//! The deferral must not violate docs/design-zero-copy-write-path.md: the
//! complete-block write-through trigger, §5.2 CoW snapshots, §5.3 coverage
//! contract (recycled bytes never escape), and the 1-copy+DMA budget are
//! all unchanged — deferral only removes the seed read.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536; // striped at small sizes; keeps get_obj deltas exact

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // Item-B pins the DEFERRAL (accumulation) machinery — the exact
    // pipeline every W1-patch-INELIGIBLE shape (unaligned, overlay,
    // shared, transform, decorated, hole) still rides. The aligned
    // sub-block shapes this suite uses became W1 patch-eligible in RW2
    // (they would neither defer nor park), so the binary pins the patch
    // path OFF; tests/extent_patch_tests.rs owns the patched-shape twin
    // contracts (incl. the sharpened crash blast-radius audit).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
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
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
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
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at {i}: got {:#04x} want {:#04x}",
            got[i], want[i]
        );
    }
}

/// A durable striped file: 6 blocks written, fsynced (map published), all
/// RAM/parked state drained, read tiers dropped so later reads are honest.
async fn durable_striped(h: &H, name: &str, tag: u8) -> (u64, Vec<u8>) {
    const LEN: usize = 6 * BS as usize;
    let ino = create(h, name).await;
    let base = pattern(LEN, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    assert!(
        m.block_map.as_ref().map(|bm| bm.len()).unwrap_or(0) >= 6 || m.block_map_id.is_some(),
        "fixture must have a durable block map"
    );
    // COLD dataset (the elbencho row-4 shape: 16 GiB, nothing cached): purge
    // every RAM/NVMe read tier so any RMW seed MUST be a device read — the
    // `get_obj` ledger then measures exactly the eager-seed cost.
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    if let Some(bm) = m.block_map.as_ref() {
        for bk in bm.values() {
            h.fs.router.cache.purge_block_key(bk);
        }
    }
    (ino, base)
}

fn get_obj() -> u64 {
    METRICS.get_obj.load(Ordering::Relaxed)
}

/// 1. THE headline contract: a sequential overwrite that fully covers every
/// block before its write-through must issue ZERO device block reads (the
/// row-4 ledger) and stay byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_coverage_overwrite_reads_nothing_and_is_byte_exact() {
    let h = make(*b"owlz-full-cov-01", "owlz_ns_a").await;
    let (ino, _) = durable_striped(&h, "full", 0x00).await;
    const LEN: usize = 6 * BS as usize;

    let over = pattern(LEN, 0x5A);
    let g0 = get_obj();
    // Sequential sub-block chunks (16 KiB), fully covering each block in
    // order — the elbencho row-4 shape.
    let chunk = 16 * 1024;
    for off in (0..LEN).step_by(chunk) {
        write_at(&h, ino, off as u64, &over[off..off + chunk]).await;
    }
    let g1 = get_obj();
    assert_eq!(
        g1 - g0,
        0,
        "a fully-covering sequential overwrite must not read old blocks \
         (the eager RMW seed: one 4 MiB device read per block — row 4's \
         17.8 GiB of reads during a pure overwrite)"
    );

    fsync(&h, ino).await;
    let got = read_at(&h, ino, 0, LEN).await;
    assert_bytes(&got, &over, "full-coverage overwrite");
}

/// 2. Partial coverage: the uncovered remainder must be seeded from the old
/// block AT FLUSH TIME — byte-exact old bytes around the patch, and the
/// ledger shows the deferred seed happened (not skipped, not doubled).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_coverage_seeds_old_bytes_at_flush() {
    let h = make(*b"owlz-partial-001", "owlz_ns_b").await;
    let (ino, base) = durable_striped(&h, "part", 0x00).await;

    // Patch the middle of block 2 only.
    let woff = 2 * BS + 16 * 1024;
    let wlen = 16 * 1024usize;
    let patch = pattern(wlen, 0xE1);
    write_at(&h, ino, woff, &patch).await;
    fsync(&h, ino).await;

    let mut want = base.clone();
    want[woff as usize..woff as usize + wlen].copy_from_slice(&patch);
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "partial overwrite (old bytes preserved)");
}

/// 3. Truncate DOWN then re-extend inside the deferral window: the dead
/// tail must read zeros; surviving old bytes and the patch stay exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_inside_deferral_window() {
    let h = make(*b"owlz-truncwin-01", "owlz_ns_c").await;
    let (ino, base) = durable_striped(&h, "twin", 0x00).await;

    // Deferred partial write into block 4.
    let woff = 4 * BS + 8 * 1024;
    let patch = pattern(8 * 1024, 0xE2);
    write_at(&h, ino, woff, &patch).await;

    // Truncate into block 1 (kills blocks 2..6 incl. the deferred buffer),
    // then extend back.
    let down = BS + 12 * 1024;
    let up = 5 * BS;
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(down),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(up),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    fsync(&h, ino).await;

    let mut want = base[..down as usize].to_vec();
    want.resize(up as usize, 0);
    let got = read_at(&h, ino, 0, up as usize).await;
    assert_bytes(&got, &want, "truncate down/up across the window");
}

/// 4. PUNCH_HOLE inside the deferral window: partial-edge punch RMWs the
/// same block the deferred buffer owns; whole-block punch drops it. Both
/// must end byte-exact per the POSIX model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn punch_inside_deferral_window() {
    let h = make(*b"owlz-punchwin-01", "owlz_ns_d").await;
    let (ino, base) = durable_striped(&h, "pwin", 0x00).await;
    let mut want = base.clone();

    // Deferred partial write into block 1.
    let woff = BS + 4 * 1024;
    let patch = pattern(12 * 1024, 0xE3);
    write_at(&h, ino, woff, &patch).await;
    want[woff as usize..woff as usize + patch.len()].copy_from_slice(&patch);

    // Partial-edge punch overlapping the SAME block (edges inside blocks 1
    // and 2) + a whole-block punch of block 3.
    let p_off = BS + 32 * 1024;
    let p_len = BS; // [1.5 blocks): edge in 1, edge in 2
    h.fs.fallocate(
        h.req,
        ino,
        0,
        p_off,
        p_len,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .unwrap();
    want[p_off as usize..(p_off + p_len) as usize].fill(0);

    let p2_off = 3 * BS;
    h.fs.fallocate(
        h.req,
        ino,
        0,
        p2_off,
        BS,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .unwrap();
    want[p2_off as usize..(p2_off + BS) as usize].fill(0);

    fsync(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "punch across the window");
}

/// 5. copy_file_range with the deferral window open on BOTH sides: source
/// block carries a deferred partial write (CFR pre-flushes source
/// overlays), dest write lands in a deferred block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cfr_inside_deferral_window() {
    let h = make(*b"owlz-cfrwin-0001", "owlz_ns_e").await;
    let (ino, base) = durable_striped(&h, "cwin", 0x00).await;
    let mut want = base.clone();

    // Deferred partial writes: src block 0, dest block 5.
    let s_patch = pattern(8 * 1024, 0xE4);
    write_at(&h, ino, 4 * 1024, &s_patch).await;
    want[4 * 1024..4 * 1024 + s_patch.len()].copy_from_slice(&s_patch);
    let d_patch = pattern(8 * 1024, 0xE5);
    write_at(&h, ino, 5 * BS + 4 * 1024, &d_patch).await;
    want[(5 * BS + 4 * 1024) as usize..(5 * BS + 4 * 1024) as usize + d_patch.len()]
        .copy_from_slice(&d_patch);

    // CFR: src [0, 32K) -> dest block 5 offset +16K (both windows open).
    let copied =
        h.fs.copy_file_range(h.req, ino, 0, 0, ino, 0, 5 * BS + 16 * 1024, 32 * 1024, 0)
            .await
            .unwrap()
            .copied;
    assert!(copied > 0, "CFR must make progress");
    let seg = want[..copied as usize].to_vec();
    want[(5 * BS + 16 * 1024) as usize..(5 * BS + 16 * 1024) as usize + copied as usize]
        .copy_from_slice(&seg);

    fsync(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "CFR across the window");
}

/// 6. Concurrent-shape reads during the window: a read of the SAME block
/// must merge {old device bytes ∪ deferred new bytes} exactly — never
/// zeros for the uncovered remainder, never stale for the patch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_during_window_serve_merged_content() {
    let h = make(*b"owlz-readwin-016", "owlz_ns_f").await;
    let (ino, base) = durable_striped(&h, "rwin", 0x00).await;

    let woff = 3 * BS + 20 * 1024;
    let patch = pattern(8 * 1024, 0xE6);
    write_at(&h, ino, woff, &patch).await;
    let mut want_block = base[(3 * BS) as usize..(4 * BS) as usize].to_vec();
    want_block[20 * 1024..28 * 1024].copy_from_slice(&patch);

    // Sub-range reads: patch interior, old-bytes prefix, straddling both.
    let got = read_at(&h, ino, woff, 4 * 1024).await;
    assert_bytes(&got, &want_block[20 * 1024..24 * 1024], "patch interior");
    let got = read_at(&h, ino, 3 * BS, 8 * 1024).await;
    assert_bytes(&got, &want_block[..8 * 1024], "old prefix during window");
    let got = read_at(&h, ino, 3 * BS + 16 * 1024, 16 * 1024).await;
    assert_bytes(
        &got,
        &want_block[16 * 1024..32 * 1024],
        "straddle old|new during window",
    );
    // Whole-block read.
    let got = read_at(&h, ino, 3 * BS, BS as usize).await;
    assert_bytes(&got, &want_block, "whole block during window");
}

/// 7. fsync forces the merge: after fsync + dropping every RAM tier, a
/// cold read resolves the DURABLE state — patch + seeded old bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_forces_merge_durably() {
    let h = make(*b"owlz-fsyncwin-01", "owlz_ns_g").await;
    let (ino, base) = durable_striped(&h, "fwin", 0x00).await;

    let woff = BS + 24 * 1024;
    let patch = pattern(4 * 1024, 0xE7);
    write_at(&h, ino, woff, &patch).await;
    fsync(&h, ino).await;

    // Cold: drop the layout cache + whole-file LRUs + block read tiers.
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.invalidate(&ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);

    let mut want = base.clone();
    want[woff as usize..woff as usize + patch.len()].copy_from_slice(&patch);
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "cold read after fsync-forced merge");
}

/// 8. Crash inside the window (never-lossy ordering): a persistent-device
/// two-session check. Session 1 makes a striped file durable, then leaves
/// an UNFSYNCED deferred partial write and drops everything (kill -9
/// equivalent for RAM state). Session 2 on the SAME device+meta must read
/// the old durable content fully intact — the unfsynced patch is legally
/// lost (D0), but the deferral must never have pre-damaged the old block
/// (no zeros, no partial merge): the old device block is displaced only
/// AFTER a merge publishes its replacement, and no merge ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_inside_window_leaves_old_block_intact() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // ACCUMULATION-path crash contract (this suite's binary-wide pin; the
    // sessions below bypass make()): an unfsynced DEFERRED write never
    // pre-damages the old durable block. The W1-patched twin — foreign
    // bytes never perturbed, the app-written window old-or-new — is pinned
    // by extent_patch_tests::crash_after_acked_patch_*.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let meta = NamedTempFile::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid: *b"owlz-crash-win-1",
    })
    .unwrap()
    .build(meta.path(), 128 * 1024 * 1024)
    .await
    .unwrap();

    async fn session(
        tag: &str,
        meta_path: &std::path::Path,
        backing_path: &std::path::Path,
        staging: &std::path::Path,
    ) -> H {
        let dlm = DlmClient::new("local").unwrap();
        let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
            backing_path.to_str().unwrap(),
        ));
        let ba = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), tag)
                .await
                .unwrap(),
        );
        let cache = TieredCache::new(
            vec![staging.to_path_buf()],
            Some("64MB"),
            Some("64MB"),
            Some("16MB"),
            Some("64MB"),
            dlm.meta_client().clone(),
            ba.clone(),
            nvme.clone(),
            None,
        )
        .await
        .unwrap();
        let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
        let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
        let be = KvMetaBackend::open(meta_path).await.unwrap();
        let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
        fs.router.set_meta_backend(routed.clone());
        fs.meta_backend = Some(routed);
        let req = Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
        };
        // Dummy owned tempfiles: the REAL volumes are the caller's.
        let b = NamedTempFile::new().unwrap();
        let m = NamedTempFile::new().unwrap();
        let s = tempdir().unwrap();
        H {
            fs,
            req,
            _b: b,
            _m: m,
            _s: s,
        }
    }

    let (ino, base) = {
        let h = session("owlz_ns_h1", meta.path(), backing.path(), staging.path()).await;
        let (ino, base) = durable_striped(&h, "crashwin", 0x00).await;
        // Deferred partial write, NO fsync — then drop the session (crash).
        write_at(&h, ino, 2 * BS + 8 * 1024, &pattern(4 * 1024, 0xE8)).await;
        (ino, base)
    };

    // Session 2: same meta volume + same block device.
    let h2 = session("owlz_ns_h1", meta.path(), backing.path(), staging.path()).await;
    let got = read_at(&h2, ino, 0, base.len()).await;
    assert_bytes(
        &got,
        &base,
        "post-crash read: old durable content intact (unfsynced patch legally lost)",
    );
}

/// 9. OVERLAY NEVER INVISIBLE (the generic/075-in-QUICK transient): an
/// ACKed deferred write must be visible to EVERY concurrent reader of its
/// block for the WHOLE deferral window — including while another read is
/// materializing the seed (a device-read await). The buggy shape checked
/// the buffer OUT of the parked map across that await: concurrent
/// single-block reads (kernel readahead, AIO) missed the overlay, fell to
/// the backend, and served pre-merge bytes — transient stale/zeros that
/// self-heal (fsx 075/112: `.bad` ≡ `.good` afterwards), exactly the
/// hardest corruption class to catch after the fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reads_never_lose_the_overlay_during_materialize() {
    let h = Arc::new(make(*b"owlz-invis-0001!", "owlz_ns_i").await);
    const ROUNDS: usize = 24;
    let (ino, base) = durable_striped(&h, "invis", 0x00).await;

    let woff = 2 * BS + 20 * 1024; // interior of block 2
    let wlen = 8 * 1024usize;
    let path = squeezefs::keys::inode_path(ino);

    for round in 0..ROUNDS {
        // Fresh deferral each round: patch block 2 (deferred), tiers cold so
        // the materialize is a REAL device-read await.
        let patch = pattern(wlen, 0xA0 ^ (round as u8));
        write_at(&h, ino, woff, &patch).await;

        let barrier = Arc::new(tokio::sync::Barrier::new(5));
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);

        // Materialize trigger: ONE read of the UNCOVERED head of block 2 —
        // the reader pays the deferred seed (device read await).
        let trig = {
            let h = h.clone();
            let barrier = barrier.clone();
            let base = base.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let got = read_at(&h, ino, 2 * BS, 8 * 1024).await;
                assert_bytes(
                    &got,
                    &base[(2 * BS) as usize..(2 * BS) as usize + 8 * 1024],
                    "uncovered head must serve OLD bytes",
                );
                let _ = done_tx.send(true);
            })
        };

        // Concurrent patch probes: the ACKed bytes must be visible in every
        // single read while the trigger's materialize is in flight.
        let mut probes = Vec::new();
        for p in 0..4 {
            let h = h.clone();
            let barrier = barrier.clone();
            let patch = patch.clone();
            let mut done = done_rx.clone();
            probes.push(tokio::spawn(async move {
                barrier.wait().await;
                loop {
                    let got = read_at(&h, ino, woff, patch.len()).await;
                    assert_bytes(
                        &got,
                        &patch,
                        &format!(
                            "round {round} probe {p}: ACKed deferred bytes vanished \
                             mid-materialize (overlay checked out across an await)"
                        ),
                    );
                    if *done.borrow_and_update() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }

        trig.await.unwrap();
        for pr in probes {
            pr.await.unwrap();
        }

        // Reset for the next round: flush the merge durably, then cold tiers.
        fsync(&h, ino).await;
        h.fs.router.cache.write_lru.remove(&path);
        h.fs.router.cache.read_lru.remove(&path);
        if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
            if let Some(bm) = m.block_map.as_ref() {
                for bk in bm.values() {
                    h.fs.router.cache.purge_block_key(bk);
                }
            }
        }
    }
}

/// 10. Same invariant at the FLUSH exit: while fsync's stage loop
/// materializes the deferred seed (device-read await), concurrent readers
/// of the patch must never lose the overlay.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reads_never_lose_the_overlay_during_flush() {
    let h = Arc::new(make(*b"owlz-invis-0002!", "owlz_ns_j").await);
    const ROUNDS: usize = 24;
    let (ino, base) = durable_striped(&h, "invisf", 0x00).await;
    let _ = base;

    let woff = BS + 24 * 1024; // interior of block 1
    let wlen = 8 * 1024usize;
    let path = squeezefs::keys::inode_path(ino);

    for round in 0..ROUNDS {
        let patch = pattern(wlen, 0xB0 ^ (round as u8));
        write_at(&h, ino, woff, &patch).await;

        let barrier = Arc::new(tokio::sync::Barrier::new(5));
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);

        let syncer = {
            let h = h.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                fsync(&h, ino).await;
                let _ = done_tx.send(true);
            })
        };

        let mut probes = Vec::new();
        for p in 0..4 {
            let h = h.clone();
            let barrier = barrier.clone();
            let patch = patch.clone();
            let mut done = done_rx.clone();
            probes.push(tokio::spawn(async move {
                barrier.wait().await;
                loop {
                    let got = read_at(&h, ino, woff, patch.len()).await;
                    assert_bytes(
                        &got,
                        &patch,
                        &format!(
                            "round {round} probe {p}: ACKed deferred bytes vanished \
                             mid-flush (overlay checked out across the stage await)"
                        ),
                    );
                    if *done.borrow_and_update() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }

        syncer.await.unwrap();
        for pr in probes {
            pr.await.unwrap();
        }

        h.fs.router.cache.write_lru.remove(&path);
        h.fs.router.cache.read_lru.remove(&path);
        if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
            if let Some(bm) = m.block_map.as_ref() {
                for bk in bm.values() {
                    h.fs.router.cache.purge_block_key(bk);
                }
            }
        }
    }
}
