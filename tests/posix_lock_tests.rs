//! POSIX advisory byte-range lock semantics (fstests generic/131,
//! `src/locktest.c` — found by the VL10 release gate's full `-g auto`
//! sweep; repro-port mandate).
//!
//! The daemon opts into `FUSE_POSIX_LOCKS`, so the kernel delegates ALL
//! advisory-lock arbitration to `setlk`/`getlk`. The pre-fix table
//! (`DashMap<(ino, owner, start, end), PosixLock>`) had three structural
//! holes locktest section 3 walks straight into:
//!
//! 1. **No lock TYPE**: two different owners' READ locks on overlapping
//!    ranges conflicted (locktest 13: `RDLOCK` over another process's
//!    `RDLOCK`, expected PASS, got EAGAIN).
//! 2. **No replace/split**: a same-owner re-lock stacked a second entry
//!    instead of replacing coverage, and F_UNLCK removed whole entries
//!    on ANY overlap instead of carving the requested range out
//!    (locktest 16/17 boundary shapes).
//! 3. **getlk fabricated the conflict**: always replied `F_WRLCK` with
//!    the stored range, never the real type/pid.
//!
//! Contract pinned here (POSIX 1003.1 fcntl byte-range semantics):
//! - RD/RD across owners: compatible. RD/WR and WR/WR across owners:
//!   EAGAIN on the non-blocking path.
//! - A same-owner lock REPLACES its coverage (upgrade/downgrade splits
//!   the old entry); same-owner ops never conflict with themselves.
//! - F_UNLCK carves exactly [start, end] out of the owner's coverage,
//!   splitting a spanning lock into two remnants.
//! - getlk reports the actual conflicting lock's type, range, and pid;
//!   no conflict ⇒ F_UNLCK.
//! - RELEASE with a lock_owner drops that owner's locks (close-drops-
//!   process-locks).

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

const RD: u32 = libc::F_RDLCK as u32;
const WR: u32 = libc::F_WRLCK as u32;
const UN: u32 = libc::F_UNLCK as u32;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    ino: u64,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "posixlock_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
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
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_9ABC_DEF1,
            uuid: *b"posix-lock-v3!!!",
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
    };
    let ino =
        fs.create(req, 1, OsStr::new("lockfile"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    H {
        fs,
        req,
        ino,
        _b: b,
        _m: m,
        _s: s,
    }
}

/// fcntl length→end conversion (inclusive end, locktest ranges).
fn end(start: u64, len: u64) -> u64 {
    start + len - 1
}

async fn lk(h: &H, owner: u64, start: u64, len: u64, typ: u32) -> Result<(), i32> {
    h.fs.setlk(
        h.req,
        h.ino,
        h.ino,
        owner,
        start,
        end(start, len),
        typ,
        owner as u32,
        false,
    )
    .await
    .map_err(|e| {
        let e: std::io::Error = e.into();
        e.raw_os_error().unwrap_or(-1)
    })
}

/// locktest section 3, step 13 shape: overlapping READ locks from two
/// different owners are COMPATIBLE (`{13,CMD_RDLOCK,50,10,PASS,CLIENT}`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_read_locks_across_owners_are_compatible() {
    let h = make().await;
    lk(&h, 1, 50, 10, RD).await.expect("owner 1 RDLOCK");
    lk(&h, 2, 50, 10, RD)
        .await
        .expect("owner 2's overlapping RDLOCK must succeed (shared locks)");
    // And a third, partially overlapping (locktest 14: RD over RD PASS).
    lk(&h, 3, 45, 20, RD)
        .await
        .expect("owner 3's straddling RDLOCK must succeed");
}

/// WR/WR and RD/WR overlaps across owners conflict, both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_conflicts_across_owners() {
    let h = make().await;
    lk(&h, 1, 30, 10, WR).await.expect("owner 1 WRLOCK");
    assert_eq!(
        lk(&h, 2, 30, 10, WR).await.unwrap_err(),
        libc::EAGAIN,
        "WR over another owner's WR must EAGAIN"
    );
    assert_eq!(
        lk(&h, 2, 30, 10, RD).await.unwrap_err(),
        libc::EAGAIN,
        "RD over another owner's WR must EAGAIN"
    );
    lk(&h, 1, 50, 10, RD).await.expect("owner 1 RDLOCK");
    assert_eq!(
        lk(&h, 2, 50, 15, WR).await.unwrap_err(),
        libc::EAGAIN,
        "WR over another owner's RD must EAGAIN"
    );
}

/// Unlock releases exactly the requested range so another owner can
/// take it (locktest 13 tail: unlock then the other process's WRLOCK
/// passes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlock_then_other_owner_takes_the_range() {
    let h = make().await;
    lk(&h, 1, 30, 10, WR).await.expect("owner 1 WRLOCK");
    assert_eq!(lk(&h, 2, 30, 10, WR).await.unwrap_err(), libc::EAGAIN);
    lk(&h, 1, 30, 10, UN).await.expect("owner 1 unlock");
    lk(&h, 2, 30, 10, WR)
        .await
        .expect("owner 2 WRLOCK after owner 1 unlocked");
}

/// F_UNLCK carves the middle out of a spanning lock: the remnants stay
/// owned, only the carved window frees (POSIX split).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlock_splits_a_spanning_lock() {
    let h = make().await;
    lk(&h, 1, 10, 10, WR).await.expect("owner 1 WRLOCK 10..=19");
    lk(&h, 1, 13, 3, UN).await.expect("owner 1 carve 13..=15");
    lk(&h, 2, 13, 3, WR)
        .await
        .expect("owner 2 takes the carved window");
    assert_eq!(
        lk(&h, 2, 10, 3, WR).await.unwrap_err(),
        libc::EAGAIN,
        "the left remnant 10..=12 must still be owner 1's"
    );
    assert_eq!(
        lk(&h, 2, 16, 4, WR).await.unwrap_err(),
        libc::EAGAIN,
        "the right remnant 16..=19 must still be owner 1's"
    );
}

/// A same-owner re-lock REPLACES its coverage: downgrading W→R makes the
/// range shareable (locktest 11/12 same-process type changes compose
/// with section-3 cross-process checks).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_owner_downgrade_replaces_coverage() {
    let h = make().await;
    lk(&h, 1, 30, 10, WR).await.expect("owner 1 WRLOCK");
    lk(&h, 1, 30, 10, RD)
        .await
        .expect("same-owner downgrade never conflicts with itself");
    lk(&h, 2, 30, 10, RD)
        .await
        .expect("post-downgrade the range is a READ lock — shareable");
}

/// getlk reports the ACTUAL conflicting lock: type, range, and pid —
/// never a fabricated F_WRLCK (locktest CMD_RDTEST/CMD_WRTEST rows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getlk_reports_the_real_conflict() {
    let h = make().await;
    lk(&h, 1, 50, 10, RD).await.expect("owner 1 RDLOCK");

    // WR probe by owner 2: conflicts with the RD lock — reply must carry
    // F_RDLCK + the real range + owner 1's pid.
    let reply = h
        .fs
        .getlk(h.req, h.ino, h.ino, 2, 52, end(52, 4), WR, 2)
        .await
        .expect("getlk itself succeeds");
    assert_eq!(reply.r#type, RD, "the conflict is a READ lock, not WRLCK");
    assert_eq!(reply.start, 50);
    assert_eq!(reply.end, end(50, 10));
    assert_eq!(reply.pid, 1, "the real holder's pid");

    // RD probe by owner 2: RD/RD compatible — must reply F_UNLCK.
    let reply = h
        .fs
        .getlk(h.req, h.ino, h.ino, 2, 52, end(52, 4), RD, 2)
        .await
        .expect("getlk succeeds");
    assert_eq!(reply.r#type, UN, "RD probe over RD lock: no conflict");
}

/// RELEASE with a lock_owner drops that owner's locks (close drops the
/// process's locks) — the other owner's survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_drops_only_that_owners_locks() {
    let h = make().await;
    lk(&h, 1, 10, 10, WR).await.expect("owner 1 WRLOCK");
    lk(&h, 2, 30, 10, WR).await.expect("owner 2 WRLOCK");

    // Owner 1 closes its handle: its locks drop.
    h.fs.open(h.req, h.ino, libc::O_RDWR as u32).await.unwrap();
    h.fs.release(h.req, h.ino, h.ino, 0, 1, false)
        .await
        .expect("release");

    lk(&h, 3, 10, 10, WR)
        .await
        .expect("owner 1's locks dropped at release");
    assert_eq!(
        lk(&h, 3, 30, 10, WR).await.unwrap_err(),
        libc::EAGAIN,
        "owner 2's locks survive owner 1's release"
    );
}
