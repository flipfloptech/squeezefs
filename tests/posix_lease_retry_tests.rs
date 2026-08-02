//! Pre-RC POSIX-semantics contract, spec §5 **POSIX-5**: `write(2)`,
//! `ftruncate`, `fsync`, and `fallocate` must never return `EAGAIN`.
//!
//! POSIX reserves `EAGAIN` on those calls for `O_NONBLOCK` descriptors;
//! userspace treats it as "retry immediately, nothing is wrong". The
//! daemon returned it for a *lost 5 s lease wait*: `DlmClient::
//! acquire_lock` fails `LockFailed` at its deadline and the error mapped
//! straight through, so a conveyor batch stall long enough to outlast one
//! wait made `cp` abort mid-copy ("Resource temporarily unavailable" —
//! fstests generic/795, whose provenance is recorded in the
//! `copy_file_range` handler). `copy_file_range` got a bounded ad-hoc
//! retry then; the write path retried only `FencingTokenExpired`.
//!
//! The contract: a lost lease wait is retried with backoff for the op
//! watchdog's budget (`SQUEEZEFS_TIMEOUT`, the same deadline that makes
//! the op loud), and exhaustion is **`EIO`** — a durable, honest failure
//! — never `EAGAIN`.
//!
//! The ladder's own timing IS the unit under test here, so the budgets
//! are driven small and explicit (sub-second) rather than synchronized
//! with sleeps: each test states its budget and asserts the ladder spent
//! it. The handler-level tests shrink the watchdog budget through
//! `SQUEEZEFS_TIMEOUT` (a launch-time knob, memoized per process — every
//! test in this binary sets the same value), and the per-attempt DLM
//! wait is clamped to the REMAINING budget, so a wedged lease costs the
//! budget and not a fixed 5 s wait.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::{
    acquire_lease_with_retry, lease_retry_backoff, SqueezefsFilesystem, METRICS,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::cell::Cell;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

fn lock_failed() -> SqueezefsError {
    SqueezefsError::LockFailed {
        reason: "lock I7 still held after 5s wait budget".into(),
    }
}

/// The happy path: a wait that is lost twice and then won returns the
/// token. No error ever reaches userspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_lost_wait_is_retried_and_succeeds() {
    let calls = Cell::new(0u32);
    let token = acquire_lease_with_retry(7, Duration::from_millis(500), |_wait| {
        let n = calls.get() + 1;
        calls.set(n);
        async move {
            if n <= 2 {
                Err(lock_failed())
            } else {
                Ok(4242)
            }
        }
    })
    .await
    .expect("a transient holder must not fail the op");
    assert_eq!(token, 4242);
    assert_eq!(calls.get(), 3, "two retries, then the win");
}

/// The exhaustion path: a holder that never lets go costs the whole
/// watchdog budget and then reports **EIO**. The pre-POSIX-5 behavior
/// was `EAGAIN` on the FIRST lost wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_holder_exhausts_into_eio_never_eagain() {
    const BUDGET: Duration = Duration::from_millis(300);
    let calls = Cell::new(0u32);
    let started = tokio::time::Instant::now();
    let err = acquire_lease_with_retry(9, BUDGET, |_wait| {
        calls.set(calls.get() + 1);
        async { Err(lock_failed()) }
    })
    .await
    .expect_err("a permanent holder must fail the op");

    assert_eq!(err.to_errno(), libc::EIO, "POSIX-5: exhaustion is EIO");
    assert_ne!(
        err.to_errno(),
        libc::EAGAIN,
        "EAGAIN is reserved for O_NONBLOCK"
    );
    assert!(
        calls.get() > 1,
        "the ladder must actually retry (attempts: {})",
        calls.get()
    );
    assert!(
        started.elapsed() >= BUDGET,
        "the ladder must spend the whole watchdog budget before giving up \
         (spent {:?})",
        started.elapsed()
    );
    // The refusal names the op's ino so the log line is actionable.
    assert!(err.to_string().contains('9'), "{err}");
}

/// A real error is NOT a lost wait: it returns on the first attempt.
/// (Retrying a dead backend for 30 s would turn every hard failure into
/// a watchdog-length hang.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_lock_error_is_returned_immediately() {
    let calls = Cell::new(0u32);
    let err = acquire_lease_with_retry(11, Duration::from_secs(30), |_wait| {
        calls.set(calls.get() + 1);
        async {
            Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::ENOENT,
            )))
        }
    })
    .await
    .expect_err("a hard error must surface");
    assert_eq!(err.to_errno(), libc::ENOENT, "verbatim, not re-derived");
    assert_eq!(calls.get(), 1, "no retry ladder for a hard error");
}

/// A zero budget still makes exactly one attempt (never zero): a knob
/// misconfiguration must not turn every mutating op into an instant EIO
/// without trying.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_budget_still_attempts_once() {
    let calls = Cell::new(0u32);
    let _ = acquire_lease_with_retry(13, Duration::ZERO, |_wait| {
        calls.set(calls.get() + 1);
        async { Err(lock_failed()) }
    })
    .await;
    assert_eq!(calls.get(), 1);
}

// ---------------------------------------------------------------------------
// Handler level — the ladder is WIRED, not merely available.
// ---------------------------------------------------------------------------

const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    dlm: DlmClient,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // The op-watchdog budget IS the POSIX-5 retry budget. Shrink it so a
    // permanently wedged lease costs the test a second, not thirty
    // (launch-time knob, memoized: every test in this binary agrees).
    std::env::set_var("SQUEEZEFS_TIMEOUT", "1");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("posix5_lease_test").await.unwrap());
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
    m.as_file().set_len(96 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x5EED_0000_0000_0005,
            uuid: *b"posix-5-lease-rc",
        })
        .unwrap()
        .build(m.path(), 96 * 1024 * 1024)
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
        dlm,
        _b: b,
        _m: m,
        _s: s,
    }
}

/// A file whose lease is held by ANOTHER client for the whole op — the
/// conveyor-stall shape generic/795 hit, made permanent.
async fn wedged_file(h: &H, name: &str) -> (u64, squeezefs::dlm::LockLease) {
    let created =
        h.fs.create(
            h.req,
            1,
            OsStr::new(name),
            libc::S_IFREG | 0o644,
            libc::O_RDWR as u32,
        )
        .await
        .expect("create");
    let ino = created.attr.ino;
    // The daemon may have cached a lease during CREATE; drop it so the
    // op under test really has to acquire.
    h.fs.invalidate_local_lease(ino);
    let foreign = DlmClient::new().unwrap();
    let hold = foreign
        .acquire_lock(
            &format!("inode_{ino}"),
            None,
            std::time::Duration::from_millis(1),
        )
        .await
        .expect("foreign holder takes the lease");
    // The daemon's own client must now lose every wait on this ino.
    assert!(
        h.dlm
            .acquire_lock(
                &format!("inode_{ino}"),
                None,
                std::time::Duration::from_millis(1)
            )
            .await
            .is_err(),
        "the harness must actually wedge the lease"
    );
    (ino, hold)
}

/// The ladder's engagement instrument: an op that gives up under a
/// permanently held lease must have RUN the ladder (spent the budget,
/// retried), not merely inherited `LockFailed`'s errno mapping. Deltas
/// only ever grow, so `>= 1` is parallel-safe.
fn exhaustions() -> u64 {
    METRICS.lease_retry_exhaustions.load(Ordering::Relaxed)
}

/// `write(2)` — the headline. Under a permanently held lease the reply
/// is EIO; it was EAGAIN, which `cp`/`dd`/glibc treat as "nothing is
/// wrong, try again" on a blocking fd.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_under_a_held_lease_is_eio_never_eagain() {
    let h = make().await;
    let (ino, _hold) = wedged_file(&h, "p5-write").await;
    let before = exhaustions();
    let err =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::from_static(b"hello"), 0, 0)
            .await
            .expect_err("a permanently held lease must fail the write");
    assert_eq!(libc::c_int::from(err), -libc::EIO, "write(2): {err}");
    assert!(
        exhaustions() > before,
        "the write handler must run the POSIX-5 ladder, not just inherit \
         LockFailed's errno"
    );
}

/// `ftruncate` (SETATTR size) — same law.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_under_a_held_lease_is_eio_never_eagain() {
    use fuse3::SetAttr;
    let h = make().await;
    let (ino, _hold) = wedged_file(&h, "p5-truncate").await;
    let before = exhaustions();
    let err =
        h.fs.setattr(
            h.req,
            ino,
            None,
            SetAttr {
                size: Some(4096),
                ..Default::default()
            },
        )
        .await
        .expect_err("a permanently held lease must fail the truncate");
    assert_eq!(libc::c_int::from(err), -libc::EIO, "ftruncate: {err}");
    assert!(exhaustions() > before, "SETATTR must run the ladder");
}

/// `fsync` — same law (a `fsync` that says EAGAIN is a data-loss trap:
/// applications treat it as retryable and move on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fsync_under_a_held_lease_is_eio_never_eagain() {
    let h = make().await;
    let (ino, _hold) = wedged_file(&h, "p5-fsync").await;
    let before = exhaustions();
    let err =
        h.fs.fsync(h.req, ino, 0, false)
            .await
            .expect_err("a permanently held lease must fail the fsync");
    assert_eq!(libc::c_int::from(err), -libc::EIO, "fsync: {err}");
    assert!(exhaustions() > before, "fsync must run the ladder");
}

/// `fallocate(PUNCH_HOLE)` — same law.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallocate_under_a_held_lease_is_eio_never_eagain() {
    let h = make().await;
    let (ino, _hold) = wedged_file(&h, "p5-fallocate").await;
    let before = exhaustions();
    let err =
        h.fs.fallocate(
            h.req,
            ino,
            0,
            0,
            4096,
            (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
        )
        .await
        .expect_err("a permanently held lease must fail the fallocate");
    assert_eq!(libc::c_int::from(err), -libc::EIO, "fallocate: {err}");
    assert!(exhaustions() > before, "fallocate must run the ladder");
}

/// The backoff schedule: strictly non-decreasing, capped, and never
/// zero — a spin loop against a held lease is what starves the holder's
/// own conveyor pass.
#[test]
fn the_backoff_schedule_is_bounded_and_non_decreasing() {
    let mut prev = Duration::ZERO;
    for attempt in 0..16u32 {
        let d = lease_retry_backoff(attempt);
        assert!(d > Duration::ZERO, "attempt {attempt} must wait");
        assert!(d >= prev, "attempt {attempt} must not shrink");
        assert!(
            d <= Duration::from_secs(1),
            "attempt {attempt} backoff {d:?} is unbounded"
        );
        prev = d;
    }
}
