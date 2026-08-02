//! PR L4-6 — lifecycle hardening (`docs/design-preload-interception.md`
//! §5.7, §5.4.1 fork row, §5.6.2 W1 mitigation):
//!
//! - **The fork-child poison law**: the child flag-poisons AND
//!   `close(2)`s its inherited ctl-socket COPY — never `shutdown(2)`,
//!   which acts on the SHARED file description and would sever the
//!   PARENT's session (the bug this suite exists to catch: L4-5 shipped
//!   one `poison()` that shutdowns — right for the same-process timeout
//!   path, fatal from the atfork child).
//! - **Idle reap** (§5.7 row 2): sessions idle past the configured bound
//!   are torn down with a generation bump (client sees poison, degrades
//!   lazily); ACTIVE sessions are never reaped (activity refreshes).
//! - **W1 invalidation handoff** (§5.6.2): the daemon fires an inode
//!   invalidation on BIND and on the FIRST ring write per (ino, window)
//!   — rate-limited, reads never fire. Tested against an injected
//!   invalidator hook (production wraps the fuse3 `Notify` handle; the
//!   kernel-delivery leg is the gate script's no-settle parity row).
//! - **In-flight teardown**: a session torn down (EOF) while a handoff
//!   is parked completes harmlessly (the completion's mapping Arc is
//!   the §5.3.1 rule-4 ordering) — no panic, no poison, zero residue.

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{DataOp, IpcHost, IpcHostConfig, SessionSink, SlotCompletion};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_il::session::{RingOutcome, Session};
use squeezefs_ipc::wire::BootstrapBlob;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// fixture (the preload_session_tests shape + injected invalidator)
// ---------------------------------------------------------------------------

async fn sandbox_fs() -> (
    squeezefs::fuse_client::SqueezefsFilesystem,
    tempfile::NamedTempFile,
    tempfile::NamedTempFile,
    tempfile::TempDir,
) {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;

    let dlm = DlmClient::new("local").unwrap();
    let backing_temp = tempfile::NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "preload_lifecycle_tests")
            .await
            .expect("block allocator"),
    );
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta_temp = tempfile::NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        meta_temp.path(),
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let meta = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta_temp.path())
        .await
        .expect("open v3");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![meta]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    (fs, backing_temp, meta_temp, staging)
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
    }
}

struct InoMapSink {
    inner: DataPlaneSink,
    map: Mutex<HashMap<u64, u64>>,
}

impl InoMapSink {
    fn translate(&self, st_ino: u64) -> u64 {
        self.map
            .lock()
            .expect("ino map mutex never poisons")
            .get(&st_ino)
            .copied()
            .unwrap_or_else(|| panic!("untranslated st_ino {st_ino}"))
    }
}

impl SessionSink for InoMapSink {
    fn serve_data(&self, mut op: DataOp, completion: SlotCompletion) {
        op.binding.ino = self.translate(op.binding.ino);
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn on_bind(&self, ino: u64) {
        self.inner.on_bind(self.translate(ino));
    }

    fn flush(&self) {
        // Forward the end-of-sweep hook (SessionSink::flush liveness
        // rule): direct-drive SQEs published during serve_data must
        // reach the kernel before the service thread parks.
        SessionSink::flush(&self.inner);
    }
}

/// Recorded invalidations (the injected W1 hook).
#[derive(Default)]
struct InvalLog {
    events: Mutex<Vec<u64>>,
}

impl InvalLog {
    fn count_for(&self, ino: u64) -> usize {
        self.events
            .lock()
            .expect("inval log mutex never poisons")
            .iter()
            .filter(|i| **i == ino)
            .count()
    }
}

const TEST_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// VAL-4 (daemon-authentication ladder): the mount-root owner the shim
/// checks the socket peer against. Harness "mounts" are tempdirs owned
/// by the test user, so the honest anchor is our own uid.
fn my_uid() -> u32 {
    // SAFETY: getuid is trivially safe.
    unsafe { libc::getuid() }
}

struct Fixture {
    fs: squeezefs::fuse_client::SqueezefsFilesystem,
    host: Arc<IpcHost>,
    sink: Arc<InoMapSink>,
    inval: Arc<InvalLog>,
    dir: tempfile::TempDir,
    _backing: tempfile::NamedTempFile,
    _meta: tempfile::NamedTempFile,
    _staging: tempfile::TempDir,
}

impl Fixture {
    /// `idle_secs` = the host idle-reap bound (0 = default/disabled for
    /// the test's purposes); `inval_window_ms` = the W1 rate window.
    async fn new(name: &str, idle_secs: u64, inval_window_ms: u64) -> Fixture {
        let (fs, backing, meta, staging) = sandbox_fs().await;
        let inval = Arc::new(InvalLog::default());
        let hook = {
            let inval = Arc::clone(&inval);
            Arc::new(move |ino: u64| {
                inval
                    .events
                    .lock()
                    .expect("inval log mutex never poisons")
                    .push(ino);
            })
        };
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::with_invalidator(fs.clone(), hook, inval_window_ms),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-life-{}-{}", std::process::id(), name),
            socket_dir: None,
            build_commit: TEST_COMMIT.to_string(),
            allow_dev: false,
            geometry: squeezefs_ipc::layout::Geometry {
                ring_entries: 16,
                slots: 16,
                arena_bytes: 2 * 1024 * 1024,
                max_op_bytes: 64 * 1024,
                _pad: 0,
            },
            arena_cap_bytes: 64 * 1024 * 1024,
            per_uid_session_cap: 8,
            idle_secs,
            data_plane: true,
            // SAFETY: getuid is trivially safe.
            owner_uid: unsafe { libc::getuid() },
        };
        let host = IpcHost::spawn(cfg, sink.clone()).expect("host must spawn");
        let dir = tempfile::tempdir().expect("tempdir");
        let st_dev = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(dir.path()).expect("metadata").dev()
        };
        host.set_expected_st_dev(st_dev);
        Fixture {
            fs,
            host,
            sink,
            inval,
            dir,
            _backing: backing,
            _meta: meta,
            _staging: staging,
        }
    }

    fn establish(&self) -> Session {
        let path = self.dir.path().join(".hello-cred");
        std::fs::write(&path, b"x").expect("cred file");
        let f = std::fs::File::open(&path).expect("open cred");
        let blob = BootstrapBlob::decode(&self.host.bootstrap_blob()).expect("blob decodes");
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
            .expect("session must establish")
    }

    async fn create_file(&self, name: &str) -> (u64, OwnedFd) {
        let create = self
            .fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create");
        let fs_ino = create.attr.ino;
        let path = self.dir.path().join(name);
        std::fs::write(&path, [0u8; 16]).expect("stand-in bytes");
        let st_ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).expect("metadata").ino()
        };
        self.sink
            .map
            .lock()
            .expect("ino map mutex never poisons")
            .insert(st_ino, fs_ino);
        let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: plain open(2); ownership taken immediately.
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        assert!(
            fd >= 0,
            "open stand-in: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: fresh owned fd.
        (fs_ino, unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn ring_write_ok(session: &Session, binding: u64, data: &[u8], offset: u64) {
    match session.ring_pwrite(binding, data, offset) {
        RingOutcome::Served(n) => assert_eq!(n, data.len(), "full serve"),
        other => panic!("ring write must serve, got {other:?}"),
    }
}

fn wait_until(what: &str, deadline: Duration, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + deadline;
    while !cond() {
        assert!(Instant::now() < end, "{what}: condition never met");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// the fork-child poison law (§5.4.1 fork row)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_child_poison_never_severs_the_parent_session() {
    let fx = Fixture::new("fork", 0, 1000).await;
    let session = fx.establish();
    let (_ino, fd) = fx.create_file("fork.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    let w = vec![7u8; 8192];
    tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 0));

    // Fork. The child runs ONLY the AS-safe child-poison path and
    // _exits — exactly the atfork child handler's job.
    // SAFETY: fork(2); the child performs only AS-safe calls then _exit.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // Child: poison_child = flag + close(2) of OUR fd-table copy.
        session.poison_child();
        // SAFETY: immediate exit, no unwinding into the test harness.
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    // SAFETY: waitpid on our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child exited cleanly"
    );

    // THE LAW: the parent's session must still serve — the child closed
    // its COPY of the fd (per-process); a shutdown(2) there would have
    // severed the shared description and killed this session.
    assert!(
        !session.poisoned(),
        "parent session must not be poisoned by the child's poison"
    );
    let mut buf = vec![0u8; 8192];
    let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
    assert!(
        matches!(out, RingOutcome::Served(8192)),
        "parent ring ops must keep serving after the child poisoned, got {out:?}"
    );
    assert_eq!(buf, w, "parity through the surviving parent session");

    // And the parent can still do ctl work (bind another fd).
    let (_ino2, fd2) = fx.create_file("fork2.bin").await;
    session
        .bind(fd2.as_raw_fd())
        .expect("parent ctl socket must survive the child's close");
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// idle reap (§5.7): idle sessions torn down; active sessions never
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_sessions_reap_and_active_sessions_survive() {
    let fx = Fixture::new("idle", 1, 1000).await;
    let session = fx.establish();
    let (_ino, fd) = fx.create_file("idle.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    // Phase 1: ACTIVE sessions are never reaped — keep issuing ops for
    // ~2.5× the idle bound.
    let reaped_before = METRICS.ipc_sessions_reaped.load(Ordering::Relaxed);
    for i in 0..5 {
        let w = vec![i as u8; 512];
        tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 0));
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        !session.poisoned(),
        "an ACTIVE session must never be reaped (activity refreshes the idle clock)"
    );
    assert_eq!(
        METRICS.ipc_sessions_reaped.load(Ordering::Relaxed),
        reaped_before,
        "no reap while active"
    );

    // Phase 2: go idle past the bound — the host reaps: generation bump
    // (client observes poison), teardown, gauges to zero.
    wait_until("idle reap", Duration::from_secs(10), || {
        METRICS.ipc_sessions_reaped.load(Ordering::Relaxed) > reaped_before
    });
    wait_until("arena gauge drains", Duration::from_secs(5), || {
        fx.host.arena_bytes() == 0
    });
    assert!(
        session.poisoned(),
        "the reaped client must observe the generation bump and degrade"
    );
    let mut buf = vec![0u8; 64];
    let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
    assert!(
        matches!(out, RingOutcome::Fallthrough),
        "ops on a reaped session must fall through, got {out:?}"
    );
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// W1 invalidation handoff (§5.6.2): bind + rate-limited first write
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalidation_fires_on_bind_and_rate_limited_writes_never_reads() {
    let fx = Fixture::new("inval", 0, 300).await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("inval.bin").await;

    let grant = session.bind(fd.as_raw_fd()).expect("bind");
    wait_until("bind invalidation", Duration::from_secs(5), || {
        fx.inval.count_for(ino) >= 1
    });
    let after_bind = fx.inval.count_for(ino);
    assert_eq!(after_bind, 1, "BIND fires exactly one invalidation");

    // First ring write in the window fires; an immediate second write is
    // suppressed by the rate limiter.
    let w = vec![1u8; 4096];
    tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 0));
    wait_until("first-write invalidation", Duration::from_secs(5), || {
        fx.inval.count_for(ino) >= 2
    });
    tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 4096));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fx.inval.count_for(ino),
        2,
        "a second write inside the window must be suppressed"
    );
    let suppressed = METRICS.ipc_inval_suppressed.load(Ordering::Relaxed);
    assert!(suppressed >= 1, "suppression is counted");

    // Past the window, the next write fires again.
    tokio::time::sleep(Duration::from_millis(350)).await;
    tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 8192));
    wait_until("post-window invalidation", Duration::from_secs(5), || {
        fx.inval.count_for(ino) >= 3
    });

    // Reads NEVER invalidate.
    let count_before_reads = fx.inval.count_for(ino);
    let mut buf = vec![0u8; 4096];
    for _ in 0..5 {
        let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
        assert!(matches!(out, RingOutcome::Served(_)));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fx.inval.count_for(ino),
        count_before_reads,
        "ring reads must never fire invalidations"
    );
    assert!(
        METRICS.ipc_inval_notifies.load(Ordering::Relaxed) >= 3,
        "invalidations are counted on the stats surface"
    );
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// in-flight teardown: EOF mid-handoff completes harmlessly (§5.3.1 r4)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_with_parked_handoff_completes_without_poison() {
    let fx = Fixture::new("inflight", 0, 1000).await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("inflight.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    let w = vec![9u8; 4096];
    tokio::task::block_in_place(|| ring_write_ok(&session, grant.binding_id, &w, 0));

    // Park a write handoff behind a held inode write lock, then tear the
    // session down (client death) while it is in flight.
    let lock = fx.fs.get_inode_lock_ref(ino);
    let guard = lock.write().await;
    let handoffs_before = METRICS.ipc_async_handoffs.load(Ordering::Relaxed);
    let submitted = tokio::task::block_in_place(|| {
        session
            .submit_pwrite_nowait(grant.binding_id, &w, 4096)
            .is_some()
    });
    assert!(submitted, "op must submit");
    wait_until("handoff parked", Duration::from_secs(5), || {
        METRICS.ipc_async_handoffs.load(Ordering::Relaxed) > handoffs_before
    });

    let poisons_before = METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed);
    drop(session); // ctl EOF: the daemon tears the session down
    wait_until("teardown", Duration::from_secs(5), || {
        METRICS.ipc_sessions_active.load(Ordering::Relaxed) == 0
    });
    drop(guard); // the parked handoff now completes — into the mapping
                 // the completion's Arc still holds (rule 4), harmlessly

    // Give the completion a moment, then prove: no panic, no poison, and
    // the write LANDED (acked-to-nobody is still daemon-owned state).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed),
        poisons_before,
        "an EOF teardown with an in-flight op is clean, never a poison"
    );
    let reply = fx
        .fs
        .read(req(), ino, 0, 4096, 4096, 0)
        .await
        .expect("read");
    assert_eq!(reply.data.to_vec(), w, "the in-flight write landed");
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// §8 stats pins for the new lifecycle families
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_stats_fields_export() {
    let fx = Fixture::new("stats", 0, 1000).await;
    let stats = fx.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json parses");
    let m = v.get("metrics").expect("metrics object");
    for key in [
        "ipc_sessions_reaped",
        "ipc_inval_notifies",
        "ipc_inval_suppressed",
    ] {
        assert!(m.get(key).is_some(), "stats inode must export {key}");
    }
    fx.host.shutdown();
}
