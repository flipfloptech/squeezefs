//! Repro-port: fstests generic/003 ctime-across-remount divergence
//! (2026-07-28 release gate, `.benchmarks/2026-07-28-release-gate-v1.1.md`).
//!
//! THE FOUND ISSUE: the write path authored inode times TWICE. The FUSE
//! write handler publishes `mtime = ctime = coarse_realtime_ns()` to the
//! attr cache (the value every kernel-visible stat serves), while
//! `KvMetaBackend::set_layout_and_size` — the layout+size persist the
//! router's inline/staged commits ride — fabricated a SECOND
//! `ctime = now_ns()` sampled later. Whenever the two samples straddled a
//! coarse-clock tick (~1 ms), the DURABLE ctime ran one tick ahead of
//! every ctime the daemon ever served, so a clean remount "changed" ctime
//! (generic/003's `change time has changed for file1 after remount` /
//! `after accessing file3 second time` lines — flaky by tick alignment).
//! The mtime face of the same wound: with no kernel flush-times SETATTR
//! (write-through mounts, or this in-process venue), the write's mtime
//! was NEVER made durable at all — a remount regressed mtime to
//! create-time.
//!
//! THE CONTRACT (single-authority times): the write op's kernel-domain
//! stamp — the one published to the attr cache — is the ONLY author of
//! the write's inode times. It parks as a pending-times refinement
//! (fold-visible reads, batched drain — the M6 machinery) and layout
//! persistence never touches times.

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

async fn make(uuid: [u8; 16]) -> H {
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("wtimes_test").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_9ABC_DEF0,
            uuid,
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
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

fn backend(h: &H) -> &Arc<RoutedMetaBackend> {
    h.fs.meta_backend.as_ref().unwrap()
}

/// Spin until CLOCK_REALTIME_COARSE advances a tick (~1–4 ms) so the
/// next daemon stamp is guaranteed to differ from the previous one —
/// this is what makes the pre-fix divergence deterministic instead of
/// tick-alignment-flaky.
fn spin_next_coarse_tick() {
    let t0 = squeezefs::coarse_realtime_ns();
    loop {
        if squeezefs::coarse_realtime_ns() != t0 {
            return;
        }
        std::hint::spin_loop();
    }
}

fn ts_ns(t: fuse3::Timestamp) -> u64 {
    (t.sec as u64).wrapping_mul(1_000_000_000) + t.nsec as u64
}

/// The mechanism conviction: `set_layout_and_size` — the fsync/release/
/// inline-commit layout persist — must NEVER author inode times. It is
/// size+layout bookkeeping for data whose times were already stamped by
/// the op that wrote it; a fresh fabricated ctime here is a second clock
/// authority that outruns every value the daemon has served (the
/// generic/003 remount divergence).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_layout_and_size_never_authors_inode_times() {
    let h = make(*b"wtimes-mech-v3!!").await;
    let be = backend(&h);
    let ino = be
        .create(1, "victim", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let before = be.getattr(ino).await.unwrap();

    // Force the persist onto a LATER coarse tick — pre-fix this makes
    // the fabricated ctime observably diverge every run.
    spin_next_coarse_tick();
    be.set_layout_and_size(ino, b"layout-bytes", 4096, &[])
        .await
        .unwrap();

    let after = be.getattr(ino).await.unwrap();
    assert_eq!(after.size, 4096, "size persist is the call's job");
    assert_eq!(
        (after.mtime, after.ctime),
        (before.mtime, before.ctime),
        "set_layout_and_size fabricated inode times: a layout persist \
         must never author mtime/ctime (generic/003 remount divergence)"
    );
}

/// The end-to-end port of generic/003's remount legs, in-process: after a
/// write, the times the daemon SERVES must be exactly the times that are
/// DURABLE — across the drain point (`sync_all_devices` = the fsync/
/// unmount durability surface), with no kernel flush-times SETATTR to
/// paper over the gap (this venue has no kernel — the write-through
/// posture). Pre-fix: durable mtime is stuck at create-time (never
/// persisted) and durable ctime is the layout persist's fabricated
/// stamp — both diverge from the served view after a remount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stamp_is_the_single_durable_times_authority() {
    let h = make(*b"wtimes-e2e-v3!!!").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("f"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    // Put the write on a strictly later tick than the create so a
    // never-persisted write mtime is distinguishable from the create
    // stamp every run.
    spin_next_coarse_tick();
    let w =
        h.fs.write(
            h.req,
            ino,
            ino,
            0,
            bytes::Bytes::from(vec![0xABu8; 512]),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written, 512);

    let served = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;

    // The fsync/unmount durability point: parked refinements drain, the
    // barrier covers them. After this, the backend view IS the durable
    // (remount) view.
    backend(&h).sync_all_devices().await.unwrap();
    let durable = backend(&h).getattr(ino).await.unwrap();

    assert_eq!(
        durable.mtime,
        ts_ns(served.mtime),
        "durable mtime diverges from the served mtime: the write's stamp \
         never became durable — a remount regresses mtime to create-time"
    );
    assert_eq!(
        durable.ctime,
        ts_ns(served.ctime),
        "durable ctime diverges from the served ctime: a second clock \
         authority stamped the durable inode (generic/003's 'change time \
         has changed ... after remount')"
    );
}

/// The park must be unconditional — NOT contingent on the attr cache
/// holding the ino (the pre-fix publish was `if let Some(cached)`-gated).
/// An evicted attr entry must not cost the write its durable times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_times_park_survives_attr_cache_eviction() {
    let h = make(*b"wtimes-evict-v3!").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("g"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    spin_next_coarse_tick();
    let pre_write_floor = squeezefs::coarse_realtime_ns();
    h.fs.attr_cache.invalidate(&ino); // evicted: nothing to publish into
    let w =
        h.fs.write(
            h.req,
            ino,
            ino,
            0,
            bytes::Bytes::from(vec![0xCDu8; 64]),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written, 64);

    backend(&h).sync_all_devices().await.unwrap();
    let durable = backend(&h).getattr(ino).await.unwrap();
    assert!(
        (durable.mtime as i64) >= (pre_write_floor as i64),
        "durable mtime {} predates the write ({}): the write's times park \
         must not be contingent on an attr-cache hit",
        durable.mtime,
        pre_write_floor
    );
    assert!(
        (durable.ctime as i64) >= (pre_write_floor as i64),
        "durable ctime {} predates the write ({})",
        durable.ctime,
        pre_write_floor
    );
}
