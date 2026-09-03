//! Finding 50 — the inode reclaim's **return law**: when
//! `reclaim_orphaned_batch(inos).await` returns, every ino in `inos` has
//! reached its terminal reclaim outcome, *whichever batch carried it*.
//!
//! The single-drive guard (FIND-RW5-A face 4) lets exactly one batch drive
//! an ino's teardown; a second batch that finds the ino claimed must not
//! drive it. Before this fix that loser also RETURNED at once — with the
//! owner's destroy still uncommitted — so a caller that awaited its own
//! reclaim and then read the live-inode gauge read the destroyed count too
//! early. The production shape: `release` enqueues every closed ino on the
//! background reclaim pool (`queue_reclaim_inode`), whose batches run on
//! the `sqz-meta` lanes on their own schedule and race every FORGET-driven
//! or explicit reclaim of the same inos. Under gate load the pool's batch
//! claimed all 48 of `statfs_iused_returns_to_baseline_after_create_delete_
//! loop`'s inos after their unlinks landed, the test's own reclaim skipped
//! all 48 as in flight, and `IUsed` read the peak (the batch-gate #4
//! flake; a 200 ms later read was back at baseline — the owner's commit).
//!
//! Every test drives the real [`SqueezefsFilesystem`] handlers in-process
//! against a real v3 KV volume (the `posix_semantics_tests` harness).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::dlm::LockMode;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;
const GIB: u64 = 1024 * 1024 * 1024;
const INODE_QUOTA: u64 = 1_000_000;

struct H {
    fs: SqueezefsFilesystem,
    be: Arc<KvMetaBackend>,
    routed: Arc<RoutedMetaBackend>,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("reclaim_inflight_test").await.unwrap());
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
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid: *b"reclaim-f50-v3!!",
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be.clone()]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    let _ = fs.inodes_limit.set(INODE_QUOTA);
    let _ = fs.capacity_limit.set(8 * GIB);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        be,
        routed,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

/// CREATE + RELEASE — the `release` is what enqueues the ino on the
/// background reclaim pool (nlink 1 at that point: skipped by the pool
/// unless its batch runs after the unlink).
async fn create_in(h: &H, name: &str) -> u64 {
    let created =
        h.fs.create(
            h.req,
            1,
            OsStr::new(name),
            libc::S_IFREG | 0o644,
            libc::O_RDWR as u32,
        )
        .await
        .unwrap();
    let ino = created.attr.ino;
    h.fs.release(h.req, ino, created.fh, 0, 0, false)
        .await
        .unwrap();
    ino
}

/// `f_files - f_ffree` — the `IUsed` column `df -i` prints.
async fn iused(h: &H) -> u64 {
    let st = h.fs.statfs(h.req, 1).await.unwrap();
    assert_eq!(st.files, INODE_QUOTA, "f_files must be the format quota");
    st.files - st.ffree
}

/// The deterministic form of the race. The OWNER's batch is the N shared
/// orphans plus one SENTINEL orphan last; the test holds the sentinel's
/// inode DLM stripe exclusive, so the owner's admission loop claims all N
/// shared inos and then parks on the sentinel's `getattr` (a shared take
/// of the held stripe) — claims in place, nothing destroyed. The LOSER's
/// batch is the N shared inos only (their stripes are checked disjoint
/// from the sentinel's, so its admission reads nothing held) and finds
/// every one claimed. While the stripe is held NEITHER call may return —
/// the owner is parked on the lock, the loser must be parked on the
/// owner. Releasing the stripe lets both finish, and `IUsed` is at
/// baseline the moment they have. (Before the fix the loser returned
/// within microseconds of skipping its last ino.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reclaim_that_finds_its_inos_claimed_waits_for_the_owning_batch() {
    let h = make().await;
    let base = iused(&h).await;
    // Orphans staged through the routed backend (no `release`, so the
    // background pool never sees them): the two batches below are the
    // only reclaims in play and their schedule is the test's.
    const N: usize = 8;
    let mut inos = Vec::with_capacity(N);
    for i in 0..N {
        let f = h
            .routed
            .create(1, &format!("f50_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        inos.push(f.ino);
    }
    for i in 0..N {
        h.routed.unlink(1, &format!("f50_{i}")).await.unwrap();
    }
    // The sentinel: an orphan on a stripe none of the shared inos hash to.
    let dlm = h.be.dlm();
    let shared_stripes: Vec<usize> = inos.iter().map(|&i| dlm.inode_stripe(i)).collect();
    let sentinel = loop {
        let f = h
            .routed
            .create(1, "f50_sentinel", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        h.routed.unlink(1, "f50_sentinel").await.unwrap();
        if !shared_stripes.contains(&dlm.inode_stripe(f.ino)) {
            break f.ino;
        }
        // Stripe collision with a shared ino: reap this one and mint again.
        h.fs.reclaim_orphaned_batch(vec![f.ino]).await;
    };
    let peak = iused(&h).await;
    assert_eq!(peak, base + N as u64 + 1);
    let waits_before = METRICS.meta_reclaim_inflight_waits.load(Ordering::Relaxed);

    let held = dlm.lock_many(&[(sentinel, LockMode::Exclusive)], &[]).await;

    let fs_a = h.fs.clone();
    let mut inos_a = inos.clone();
    inos_a.push(sentinel);
    let a = tokio::spawn(async move { fs_a.reclaim_orphaned_batch(inos_a).await });
    // The loser starts once the owner is parked with its claims in place:
    // the owner's `getattr(sentinel)` blocks on the held stripe, and the
    // only thing the loser can observe about that is the negative window
    // below — so give the owner its head start first.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !a.is_finished(),
        "the owner must be parked on the held stripe"
    );
    let fs_b = h.fs.clone();
    let inos_b = inos.clone();
    let b = tokio::spawn(async move { fs_b.reclaim_orphaned_batch(inos_b).await });

    // The negative window: with the stripe held the owner cannot reach
    // its destroy, so a loser that returns here returned BEFORE its inos'
    // outcome.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        iused(&h).await,
        peak,
        "nothing can be destroyed while the owner is parked"
    );
    assert!(
        !a.is_finished(),
        "the owner must still be parked on the held stripe"
    );
    assert!(
        !b.is_finished(),
        "a reclaim returned while its inos were still claimed-and-undestroyed by a \
         concurrent batch — the return law is broken"
    );

    drop(held);
    a.await.unwrap();
    b.await.unwrap();
    assert_eq!(
        iused(&h).await,
        base,
        "both batches returned ⇒ every ino reached its terminal outcome"
    );
    assert!(
        METRICS.meta_reclaim_inflight_waits.load(Ordering::Relaxed) > waits_before,
        "the loser must have parked on the owner's claim (engagement gauge flat)"
    );
}

/// The gate's shape, tightened: create N, unlink N, reclaim, read — on ONE
/// volume for many rounds, with the release-enqueued background pool
/// racing every round's explicit reclaim. `IUsed` must read baseline
/// immediately after EVERY awaited reclaim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iused_reads_baseline_right_after_every_awaited_reclaim_under_a_racing_pool() {
    let h = make().await;
    let base = iused(&h).await;
    const N: usize = 48;
    for round in 0..40 {
        let mut inos = Vec::with_capacity(N);
        for i in 0..N {
            inos.push(create_in(&h, &format!("f50_{round}_{i}")).await);
        }
        for i in 0..N {
            h.fs.unlink(h.req, 1, OsStr::new(&format!("f50_{round}_{i}")))
                .await
                .unwrap();
        }
        h.fs.reclaim_orphaned_batch(inos).await;
        let after = iused(&h).await;
        assert_eq!(
            after, base,
            "round {round}: IUsed {after} != baseline {base} right after an awaited reclaim"
        );
    }
}
