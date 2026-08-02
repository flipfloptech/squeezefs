//! PR L4-5 — the shim's **session client** end-to-end against the real
//! host + data plane (`docs/design-preload-interception.md` §5.2 client
//! side, §5.3 wake protocol, §5.4.1 ring-op error ladder).
//!
//! This is the seam between the two crates: `squeezefs_il::session`
//! (the library the interposers call) speaks the ctl socket + shm
//! protocol to `squeezefs::ipc_host::IpcHost` running the production
//! [`DataPlaneSink`]. The xattr bootstrap itself needs a real mount
//! (kernel GETXATTR) — here the daemon-side blob feeds the client
//! directly through the factored [`Session::establish`] seam; the
//! fgetxattr fetch is pinned by the mount-tier gate script.
//!
//! Ino translation as in tests/preload_parity_tests.rs: harness-bound
//! fds are tempdir files; [`InoMapSink`] maps st_ino → fs-ino at the
//! sink boundary (a documented test seam, never a production path).

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{DataOp, IpcHost, IpcHostConfig, SessionSink, SlotCompletion};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_il::session::{RingOutcome, Session, SessionError};
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::wire::BootstrapBlob;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// fixture (data plane + host; see module docs for the ino seam)
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
        BlockAllocator::new(dlm.meta_client().clone(), "preload_session_tests")
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
    map: std::sync::Mutex<HashMap<u64, u64>>,
}

impl SessionSink for InoMapSink {
    fn serve_data(&self, mut op: DataOp, completion: SlotCompletion) {
        let mapped = self
            .map
            .lock()
            .expect("ino map mutex never poisons")
            .get(&op.binding.ino)
            .copied();
        let Some(fs_ino) = mapped else {
            panic!(
                "harness bound an untranslated st_ino {} — register it in the InoMapSink",
                op.binding.ino
            );
        };
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn flush(&self) {
        // Forward the end-of-sweep hook (SessionSink::flush liveness
        // rule): direct-drive SQEs published during serve_data must
        // reach the kernel before the service thread parks.
        SessionSink::flush(&self.inner);
    }
}

/// A sink that NEVER completes: drops the completion handle. Pins the
/// client's bounded-wait rule (§5.4.1 timeout ⇒ passthrough + poison).
struct StallSink;

impl SessionSink for StallSink {
    fn serve_data(&self, _op: DataOp, completion: SlotCompletion) {
        drop(completion);
    }
}

/// A sink that completes each op from a detached thread after a fixed
/// delay — the ticket-park pins' stand-in for a device-latency-bound
/// daemon (the op is in flight long enough for the client to park, then
/// completes while it sleeps).
struct DelaySink {
    delay: std::time::Duration,
}

impl SessionSink for DelaySink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        let d = self.delay;
        let n = op.desc.len as i64;
        std::thread::spawn(move || {
            std::thread::sleep(d);
            completion.complete(n);
        });
    }
}

/// A sink that completes inline on the daemon's drain pass — zero serve
/// latency, so the only inter-op gap a service thread observes is the
/// client's own submit round trip (a few µs). The service-economy pins'
/// stand-in for a warm serve.
struct InstantSink;

impl SessionSink for InstantSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        completion.complete(op.desc.len as i64);
    }
}

/// Dedicated host over an arbitrary sink + one bound O_RDWR fd (the
/// ticket-park / service-economy pins' fixture shape — no filesystem
/// needed, the sink fabricates results).
fn sink_host(
    name: &str,
    sink: Arc<dyn SessionSink>,
) -> (Arc<IpcHost>, tempfile::TempDir, std::fs::File, Session, u64) {
    sink_host_geo(name, sink, test_geometry())
}

/// [`sink_host`] with an explicit geometry (the large-op economy pins use
/// a multi-slab `max_op_bytes` window; the default fixture keeps its
/// `max_op < slab` shape).
fn sink_host_geo(
    name: &str,
    sink: Arc<dyn SessionSink>,
    geometry: Geometry,
) -> (Arc<IpcHost>, tempfile::TempDir, std::fs::File, Session, u64) {
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-session-{name}-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry,
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    };
    let host = IpcHost::spawn(cfg, sink).expect("host must spawn");
    let dir = tempfile::tempdir().unwrap();
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).unwrap().dev()
    };
    host.set_expected_st_dev(st_dev);
    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![7u8; 8192]).unwrap();
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();
    let session = Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("session establishes");
    let bind = session.bind(f.as_raw_fd()).expect("bind succeeds");
    (host, dir, f, session, bind.binding_id)
}

/// [`sink_host`] over a [`DelaySink`] (the ticket-park pins' shape).
fn delay_host(
    name: &str,
    delay_ms: u64,
) -> (Arc<IpcHost>, tempfile::TempDir, std::fs::File, Session, u64) {
    sink_host(
        name,
        Arc::new(DelaySink {
            delay: std::time::Duration::from_millis(delay_ms),
        }),
    )
}

fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 2 * 1024 * 1024, // 16 slots ⇒ 128 KiB slabs
        max_op_bytes: 64 * 1024,      // < slab: max_op governs chunking
        _pad: 0,
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
    dir: tempfile::TempDir,
    _backing: tempfile::NamedTempFile,
    _meta: tempfile::NamedTempFile,
    _staging: tempfile::TempDir,
    _sockdir: tempfile::TempDir,
}

impl Fixture {
    async fn new(name: &str) -> Fixture {
        Self::new_geo(name, test_geometry()).await
    }

    /// [`Fixture::new`] with an explicit geometry (multi-slab windows for
    /// the large-op economy pins).
    async fn new_geo(name: &str, geometry: Geometry) -> Fixture {
        let (fs, backing, meta, staging) = sandbox_fs().await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone()),
            map: std::sync::Mutex::new(HashMap::new()),
        });
        // OQ-6: fixtures advertise a path socket too (abstract stays the
        // first rung — these suites prove both rungs of the ladder).
        let sockdir = tempfile::tempdir().expect("socket tempdir");
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-session-{}-{}", std::process::id(), name),
            socket_dir: Some(sockdir.path().to_path_buf()),
            build_commit: TEST_COMMIT.to_string(),
            allow_dev: false,
            geometry,
            arena_cap_bytes: 64 * 1024 * 1024,
            per_uid_session_cap: 8,
            idle_secs: 0,
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
            dir,
            _backing: backing,
            _meta: meta,
            _staging: staging,
            _sockdir: sockdir,
        }
    }

    fn blob(&self) -> BootstrapBlob {
        BootstrapBlob::decode(&self.host.bootstrap_blob()).expect("host blob decodes")
    }

    fn establish(&self) -> Session {
        // Any real fd on the expected device works as the HELLO
        // credential; use the tempdir root's directory? No — the screen
        // demands S_ISREG. Use a scratch regular file.
        let path = self.dir.path().join(".hello-cred");
        std::fs::write(&path, b"x").expect("cred file");
        let f = std::fs::File::open(&path).expect("open cred");
        Session::establish(&self.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid())
            .expect("session must establish")
    }

    async fn create_file(&self, name: &str, open_flags: libc::c_int) -> (u64, OwnedFd) {
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
        let fd = unsafe { libc::open(cpath.as_ptr(), open_flags) };
        assert!(
            fd >= 0,
            "open stand-in: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: fresh owned fd.
        (fs_ino, unsafe { OwnedFd::from_raw_fd(fd) })
    }

    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read")
            .data
            .to_vec()
    }

    async fn fuse_write(&self, ino: u64, offset: u64, data: &[u8]) -> u32 {
        self.fs
            .write(
                req(),
                ino,
                0,
                offset,
                bytes::Bytes::copy_from_slice(data),
                0,
                0,
            )
            .await
            .expect("fuse write")
            .written
    }
}

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 17) % 251) as u8)
        .collect()
}

// ---------------------------------------------------------------------------
// establish + bind + data parity through the REAL client library
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_establish_bind_and_rw_parity() {
    let fx = Fixture::new("parity").await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("parity.bin", libc::O_RDWR).await;

    let bind = session.bind(fd.as_raw_fd()).expect("bind must succeed");
    assert!(
        bind.read_ok && bind.write_ok,
        "O_RDWR grants both directions"
    );

    // Write via the client library (spans multiple max_op_bytes chunks:
    // 200 KiB over a 64 KiB per-op ceiling), read back via BOTH
    // transports.
    let w = deterministic_bytes(200 * 1024, 1);
    match tokio::task::block_in_place(|| session.ring_pwrite(bind.binding_id, &w, 4096)) {
        RingOutcome::Served(n) => assert_eq!(n, w.len(), "chunked pwrite serves fully"),
        other => panic!("ring_pwrite must serve, got {other:?}"),
    }
    let ring = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; w.len()];
        match session.ring_pread(bind.binding_id, &mut buf, 4096) {
            RingOutcome::Served(n) => {
                buf.truncate(n);
                buf
            }
            other => panic!("ring_pread must serve, got {other:?}"),
        }
    });
    assert_eq!(ring, w, "client-library read parity");
    let fuse = fx.fuse_read(ino, 4096, w.len() as u32).await;
    assert_eq!(fuse, w, "FUSE read parity after client-library write");

    // FUSE write → client-library read (the KD-11 direction).
    let w2 = deterministic_bytes(32 * 1024, 2);
    fx.fuse_write(ino, 300 * 1024, &w2).await;
    let ring = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; w2.len()];
        match session.ring_pread(bind.binding_id, &mut buf, 300 * 1024) {
            RingOutcome::Served(n) => {
                buf.truncate(n);
                buf
            }
            other => panic!("ring_pread must serve, got {other:?}"),
        }
    });
    assert_eq!(ring, w2, "kernel-side write visible to client-library read");

    // Read past EOF: short (0) — POSIX-identical.
    let n = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; 4096];
        session.ring_pread(bind.binding_id, &mut buf, 1 << 40)
    });
    assert!(
        matches!(n, RingOutcome::Served(0)),
        "past-EOF pread serves 0"
    );

    session.unbind(bind.binding_id);
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// bind screens (client-visible refusals) + wrong-direction EBADF
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bind_refuses_directory_and_wrong_direction_is_ebadf() {
    let fx = Fixture::new("screens").await;
    let session = fx.establish();

    // Directory fd: refused (daemon screen S_ISREG; the client mirror
    // may refuse it even earlier — either way bind() errors).
    let dpath = std::ffi::CString::new(fx.dir.path().to_str().unwrap()).unwrap();
    // SAFETY: plain open(2) of a directory.
    let dfd = unsafe { libc::open(dpath.as_ptr(), libc::O_RDONLY) };
    assert!(dfd >= 0);
    // SAFETY: fresh owned fd.
    let dfd = unsafe { OwnedFd::from_raw_fd(dfd) };
    assert!(
        session.bind(dfd.as_raw_fd()).is_err(),
        "directory fd must never bind"
    );

    // O_RDONLY binding: ring write must surface EBADF (kernel-identical
    // for the wrong-direction op on that fd).
    let (_ino, fd) = fx.create_file("ro.bin", libc::O_RDONLY).await;
    let bind = session.bind(fd.as_raw_fd()).expect("O_RDONLY binds");
    assert!(bind.read_ok && !bind.write_ok);
    let out = tokio::task::block_in_place(|| session.ring_pwrite(bind.binding_id, b"nope", 0));
    assert!(
        matches!(out, RingOutcome::Errno(e) if e == libc::EBADF),
        "wrong-direction ring write must be EBADF, got {out:?}"
    );
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// version skew refusal (KD-7 client side)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn establish_refuses_on_commit_skew() {
    let fx = Fixture::new("skew").await;
    let path = fx.dir.path().join("cred");
    std::fs::write(&path, b"x").unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let other_commit = "b".repeat(40);
    assert!(
        Session::establish(&fx.blob(), f.as_raw_fd(), &other_commit, my_uid()).is_err(),
        "a skewed shim identity must refuse to establish (KD-7)"
    );
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// bounded wait: a stalled daemon poisons the session, ops fall through
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_serve_times_out_poisons_and_falls_through() {
    // Dedicated host wired to a sink that never completes.
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-session-stall-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    };
    let host = IpcHost::spawn(cfg, Arc::new(StallSink)).expect("host must spawn");
    let dir = tempfile::tempdir().unwrap();
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).unwrap().dev()
    };
    host.set_expected_st_dev(st_dev);

    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![7u8; 8192]).unwrap();
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();
    let session =
        Session::establish_with_op_timeout_ms(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid(), 200)
            .expect("session establishes");
    let bind = session.bind(f.as_raw_fd()).expect("bind succeeds");

    let start = std::time::Instant::now();
    let out = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; 4096];
        session.ring_pread(bind.binding_id, &mut buf, 0)
    });
    assert!(
        matches!(out, RingOutcome::Fallthrough),
        "a timed-out ring op must fall through to the real call, got {out:?}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "the wait must be bounded (§5.4.1), took {:?}",
        start.elapsed()
    );
    assert!(session.poisoned(), "a timeout poisons the session");
    let out = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; 64];
        session.ring_pread(bind.binding_id, &mut buf, 0)
    });
    assert!(
        matches!(out, RingOutcome::Fallthrough),
        "every op on a poisoned session falls through immediately"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// reap parks (2026-07-26 reap economy, re-based on the 2026-07-28 cqe
// doorbell): event-driven, never polled
// ---------------------------------------------------------------------------

/// The libaio reap's wait is EVENT-DRIVEN: a parked reaper is woken by
/// the daemon's completion (the session cqe doorbell — op-economy
/// 2026-07-28; formerly the per-ticket WAITER bit), not by a poll
/// quantum. The park is race-free by the register→snapshot→re-scan
/// protocol: a completion landing before `cqe_park_begin` is found by
/// the post-registration scan; one landing after it either fails the
/// futex admission (seq bumped) or pays the wake (parked gate) — the
/// `ipc_cqe_parked_reaper_never_stranded` loom model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_park_wait_is_event_driven_not_quantum_polled() {
    let (host, _dir, _f, session, binding) = delay_host("ticket-park", 100);

    // Park path: op in flight (sink completes in ~100 ms), the reaper
    // parks with a 10 s bound and must return on the completion WAKE —
    // an expired bound here means the wake never arrived.
    let t = session
        .submit_pread_nowait(binding, 4096, 0)
        .expect("slot claim");
    assert!(!session.ticket_done(t), "op is in flight");
    let start = std::time::Instant::now();
    tokio::task::block_in_place(|| loop {
        // The reap-loop protocol: register + snapshot, then the
        // mandatory pending re-scan, then the bounded wait.
        let entry = session.cqe_park_begin();
        if session.ticket_done(t) {
            session.cqe_park_end();
            break;
        }
        squeezefs_il::session::wait_any(&[entry], std::time::Duration::from_secs(10));
        session.cqe_park_end();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(9),
            "parked reaper was not woken by the completion (slept toward \
             the full bound — the wake is the contract)"
        );
    });
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "completion must WAKE the parked reaper (took {:?} against a \
         ~100 ms serve — a poll quantum or a stranded park)",
        start.elapsed()
    );
    assert!(session.ticket_done(t));
    let res = session
        .poll_ticket(t, Some(&mut [0u8; 4096]))
        .expect("done");
    assert_eq!(res, 4096);

    // Ready path: the op already completed — the post-registration
    // re-scan must consume it without sleeping toward any bound.
    let t2 = session
        .submit_pread_nowait(binding, 512, 0)
        .expect("slot claim");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !session.ticket_done(t2) {
        assert!(std::time::Instant::now() < deadline, "sink must complete");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let ready_probe = std::time::Instant::now();
    let _entry = session.cqe_park_begin();
    assert!(
        session.ticket_done(t2),
        "a DONE ticket must be found by the post-registration scan \
         (register→snapshot→re-scan) — never parked toward a bound"
    );
    session.cqe_park_end();
    assert!(
        ready_probe.elapsed() < std::time::Duration::from_secs(1),
        "the Ready path must not sleep"
    );
    assert_eq!(
        session
            .poll_ticket(t2, Some(&mut [0u8; 512]))
            .expect("done"),
        512
    );
    host.shutdown();
}

/// Wake breadth: EVERY reaper parked on a session's completion doorbell
/// is woken by a completion. Two reapers legally park on one session
/// (split submitter/reaper pairs re-snapshot the same pending set); a
/// single-waiter wake would strand the loser for its full bound —
/// `SlotCompletion::complete` wakes the cqe word at breadth `i32::MAX`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_completion_wakes_every_parked_waiter() {
    let (host, _dir, _f, session, binding) = delay_host("wake-breadth", 300);
    let session = Arc::new(session);

    let t = session
        .submit_pread_nowait(binding, 4096, 0)
        .expect("slot claim");

    let start = std::time::Instant::now();
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                // A DONE hit in the post-registration scan means the op
                // already completed (the race is legal) — trivially
                // "woken".
                let entry = session.cqe_park_begin();
                if !session.ticket_done(t) {
                    squeezefs_il::session::wait_any(&[entry], std::time::Duration::from_secs(6));
                }
                session.cqe_park_end();
                start.elapsed()
            })
        })
        .collect();
    for w in waiters {
        let took = w.join().expect("waiter thread");
        assert!(
            took < std::time::Duration::from_secs(3),
            "a parked waiter slept toward its full bound ({took:?}) — the \
             completion must wake EVERY waiter on the word, not one"
        );
    }
    assert_eq!(
        session
            .poll_ticket(t, Some(&mut [0u8; 4096]))
            .expect("done"),
        4096
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// service-thread economy (the 2026-07-26 reap-economy residual 2): a hot
// stream never pays a park/wake cycle per op, and the owned-session view
// a service thread drains from always converges on the registry
// ---------------------------------------------------------------------------

/// A back-to-back op stream must NOT park the service thread per op:
/// the empty-pass spin window covers the client's submit round trip
/// (a few µs), so `ipc_service_parks` stays ~flat across the stream.
/// Pre-fix, the spin window was 64 bare `spin_loop` hints (sub-µs —
/// smaller than ANY client round trip), so every op paid a full
/// park/wake cycle: the measured sessions inversion (svc voluntary
/// context switches 32k→394k /s from sessions=1→8 at one offered load,
/// > 1 park cycle per op at 8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_thread_stays_hot_between_back_to_back_ops() {
    // Pin the window explicitly (the knob is read at service-thread
    // start, inside this spawn): the mechanism under test is the
    // time-based window, not the deployment default.
    std::env::set_var("SQUEEZEFS_IPC_SPIN_US", "200");
    let (host, _dir, _f, session, binding) = sink_host("svc-hot", Arc::new(InstantSink));
    std::env::remove_var("SQUEEZEFS_IPC_SPIN_US");

    // One op round trip, then a ~50 µs busy-wait gap — the fleet-spread
    // per-session inter-arrival shape (a 235 µs device staggers bursts;
    // the rig measured ~28 µs at sessions=8 and coarser on 16-process
    // fleets). The gap sits far outside the old 64-pass spin (sub-µs to
    // low-µs) and far inside the fixed time-based window.
    let roundtrip = |n: usize| {
        for _ in 0..n {
            let t = session
                .submit_pread_nowait(binding, 512, 0)
                .expect("slot claim");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(res) = session.poll_ticket(t, Some(&mut [0u8; 512])) {
                    assert_eq!(res, 512);
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "instant sink must complete"
                );
                std::hint::spin_loop();
            }
            let gap = std::time::Instant::now() + std::time::Duration::from_micros(50);
            while std::time::Instant::now() < gap {
                std::hint::spin_loop();
            }
        }
    };

    tokio::task::block_in_place(|| {
        roundtrip(100); // warmup: session admitted, thread spun up
        let parks0 = METRICS.ipc_service_parks.load(Ordering::Relaxed);
        roundtrip(2000);
        let parks = METRICS.ipc_service_parks.load(Ordering::Relaxed) - parks0;
        assert!(
            parks < 200,
            "a hot back-to-back stream paid {parks} service parks over 2000 \
             ops — the empty-pass spin window must cover the client's \
             round-trip gap (a park/wake cycle per op is the sessions-\
             inversion engine)"
        );
    });
    host.shutdown();
}

/// The owned-session view converges: a session admitted to an ALREADY
/// BUSY service thread (single-thread host, existing session mid-
/// stream) is served promptly. Guards the snapshot-caching machinery —
/// a stale owned-set that never refreshes would strand the new
/// session's ops forever, not just for one park bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_session_on_a_busy_thread_is_served_promptly() {
    // Force ONE service thread so both sessions share an owner. The env
    // var is read once at host spawn (inside this test); the full suite
    // runs single-threaded in the gate, so the window is benign.
    std::env::set_var("SQUEEZEFS_IPC_SERVICE_THREADS", "1");
    let (host, dir, _f, session_a, binding_a) = sink_host("svc-refresh", Arc::new(InstantSink));
    std::env::remove_var("SQUEEZEFS_IPC_SERVICE_THREADS");

    // Session A: continuous stream on a background thread — keeps the
    // single service thread hot (and its owned snapshot warm).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let streamer = {
        let session_a = Arc::new(session_a);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Some(t) = session_a.submit_pread_nowait(binding_a, 512, 0) else {
                    continue;
                };
                while session_a.poll_ticket(t, Some(&mut [0u8; 512])).is_none() {
                    std::hint::spin_loop();
                }
            }
        })
    };

    // Session B admits mid-stream onto the same (only) service thread.
    let path_b = dir.path().join("g.bin");
    std::fs::write(&path_b, vec![9u8; 8192]).unwrap();
    let f_b = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path_b)
        .unwrap();
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();
    let session_b = Session::establish(&blob, f_b.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("session B establishes");
    let bind_b = session_b.bind(f_b.as_raw_fd()).expect("bind B succeeds");

    let start = std::time::Instant::now();
    let t = session_b
        .submit_pread_nowait(bind_b.binding_id, 512, 0)
        .expect("slot claim on B");
    tokio::task::block_in_place(|| loop {
        if let Some(res) = session_b.poll_ticket(t, Some(&mut [0u8; 512])) {
            assert_eq!(res, 512);
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "op on the newly admitted session was never served — the \
             service thread's owned-session view must refresh on admission"
        );
        std::hint::spin_loop();
    });

    stop.store(true, Ordering::Relaxed);
    streamer.join().expect("streamer thread");
    host.shutdown();
}

/// Host shutdown is prompt even with the §5.7 idle reaper armed: the
/// reap thread parks on a `clamp(idle/4, 100ms, 5s)` tick and shutdown
/// must cut that park short, not sleep it out under the join (the
/// measured 2026-07-26 umount term: every interception mount paid a
/// full 5 s reap-tick join on teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_is_prompt_with_the_idle_reaper_armed() {
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-session-reapjoin-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 3600, // reap tick clamps to its 5 s max
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    };
    let host = IpcHost::spawn(cfg, Arc::new(InstantSink)).expect("host must spawn");
    // Let the reap thread reach its park.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let start = std::time::Instant::now();
    tokio::task::block_in_place(|| host.shutdown());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "shutdown took {:?} — the reap thread's park must be cut short \
         at join, not slept out",
        start.elapsed()
    );
}

// ---------------------------------------------------------------------------
// explicit poison (the atfork child path) is immediate fallthrough
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_poison_falls_through_and_stops_ctl_traffic() {
    let fx = Fixture::new("poison").await;
    let session = fx.establish();
    let (_ino, fd) = fx.create_file("p.bin", libc::O_RDWR).await;
    let bind = session.bind(fd.as_raw_fd()).expect("bind");

    let unbinds_before = METRICS.ipc_binds.load(Ordering::Relaxed);
    session.poison(); // AS-safe: flag + socket close (the atfork shape)
    assert!(session.poisoned());

    let out = tokio::task::block_in_place(|| session.ring_pwrite(bind.binding_id, b"x", 0));
    assert!(matches!(out, RingOutcome::Fallthrough));
    // A poisoned session never emits ctl traffic (§5.4.1 normative):
    // unbind after poison must be a silent no-op, and further binds fail.
    session.unbind(bind.binding_id);
    assert!(
        session.bind(fd.as_raw_fd()).is_err(),
        "bind after poison fails"
    );
    let _ = unbinds_before; // (bind-count is host-side; the real assert
                            // is that nothing panicked and nothing hung)
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// OQ-6 (v1.1): connect ladder — abstract first, path-socket fallback
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn establish_falls_back_to_the_path_socket_when_abstract_is_unreachable() {
    let fx = Fixture::new("path-ladder").await;
    let mut blob = fx.blob();
    assert!(
        !blob.socket_path.is_empty(),
        "fixture hosts advertise a path socket"
    );
    // A foreign-netns stand-in: the abstract name resolves nowhere in
    // OUR namespace (ECONNREFUSED) while the path socket — reachable
    // through any shared mount surface — still rendezvouses.
    blob.socket = format!("sqz-il0-nowhere-{}", std::process::id());

    let path = fx.dir.path().join(".hello-cred-ladder");
    std::fs::write(&path, b"x").expect("cred file");
    let f = std::fs::File::open(&path).expect("open cred");
    let session = tokio::task::block_in_place(|| {
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
            .expect("path-socket fallback must establish")
    });
    assert!(!session.poisoned(), "fallback session is live");
    drop(session);

    // Both rungs dead ⇒ Socket error (the bind_refused{socket} path).
    // The dead path must live in a TRUSTED directory (VAL-4 step 1
    // screens the rendezvous before connecting — an untrusted dir
    // refuses earlier, with its own class, pinned below).
    let mut dead = fx.blob();
    dead.socket = format!("sqz-il0-nowhere2-{}", std::process::id());
    dead.socket_path = fx
        ._sockdir
        .path()
        .join("does-not-exist.sock")
        .to_string_lossy()
        .into_owned();
    let err = tokio::task::block_in_place(|| {
        match Session::establish(&dead, f.as_raw_fd(), TEST_COMMIT, my_uid()) {
            Ok(_) => panic!("no rendezvous must refuse"),
            Err(e) => e,
        }
    });
    assert!(
        matches!(err, SessionError::Socket(_)),
        "both-rungs-dead is a socket refusal, got {err:?}"
    );

    // VAL-4 step 1: the same shape in a world-writable directory (the
    // pre-RC `/tmp` rendezvous) is refused WITHOUT a connect attempt —
    // the squattable-name class, distinct from "nobody is listening".
    let mut squat = dead.clone();
    squat.socket_path = "/tmp/sqz-il0-does-not-exist.sock".to_string();
    let err = tokio::task::block_in_place(|| {
        match Session::establish(&squat, f.as_raw_fd(), TEST_COMMIT, my_uid()) {
            Ok(_) => panic!("a squattable rendezvous must refuse"),
            Err(e) => e,
        }
    });
    assert!(
        matches!(err, SessionError::UntrustedRendezvous),
        "a world-writable rendezvous dir refuses at step 1, got {err:?}"
    );
    fx.host.shutdown();
}

// ---------------------------------------------------------------------------
// DIALED P3 — large-op ring economy (the write matrix's >slab collapse,
// .benchmarks/2026-07-27-write-side-economy.md): a ring write larger than
// one arena slab must NOT degrade into serial slab-sized round trips. The
// contract: chunks are sized to the full `max_op_bytes` window (multi-slab
// contiguous arena runs), and every chunk of one op is SUBMITTED before the
// client waits on any of them (pipelining). POSIX prefix semantics and slot
// accounting are pinned alongside.
// ---------------------------------------------------------------------------

/// Multi-slab geometry: 16 slots × 128 KiB slabs, max_op 512 KiB (4 slabs
/// per op window).
fn wide_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 2 * 1024 * 1024,
        max_op_bytes: 512 * 1024,
        _pad: 0,
    }
}

/// Records every served WRITE `(offset, len)` and completes it fully.
struct RecordingSink {
    writes: std::sync::Mutex<Vec<(u64, u32)>>,
}

impl SessionSink for RecordingSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if op.desc.op == squeezefs_ipc::layout::OP_WRITE {
            self.writes
                .lock()
                .expect("recording mutex never poisons")
                .push((op.desc.offset, op.desc.len));
        }
        completion.complete(op.desc.len as i64);
    }
}

/// Parks every WRITE completion until `need` distinct WRITE ops have
/// arrived, then completes all of them fully — the pipelining proof: a
/// serial-round-trip client strands on its first chunk (the op deadline
/// fires); a pipelined client lands every chunk in one drain window.
/// Never blocks the service thread (completions are stashed, not awaited).
struct ParkUntilNSink {
    need: usize,
    parked: std::sync::Mutex<Vec<(u32, SlotCompletion)>>,
}

impl SessionSink for ParkUntilNSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        let mut parked = self.parked.lock().expect("park mutex never poisons");
        parked.push((op.desc.len, completion));
        if parked.len() >= self.need {
            for (len, c) in parked.drain(..) {
                c.complete(len as i64);
            }
        }
    }
}

/// Completes the FIRST write short (`short_n` bytes), everything after it
/// fully — the POSIX prefix pin.
struct ShortFirstSink {
    short_n: i64,
    fired: std::sync::atomic::AtomicBool,
}

impl SessionSink for ShortFirstSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if !self.fired.swap(true, Ordering::SeqCst) {
            completion.complete(self.short_n);
        } else {
            completion.complete(op.desc.len as i64);
        }
    }
}

/// Errors the op whose offset matches `err_at` with -EIO, completes every
/// other op fully.
struct ErrAtSink {
    err_at: u64,
}

impl SessionSink for ErrAtSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if op.desc.offset == self.err_at {
            completion.complete(i64::from(-libc::EIO));
        } else {
            completion.complete(op.desc.len as i64);
        }
    }
}

/// Parks READ completions until released (slot-occupancy fixture for the
/// fragmentation pin); WRITE ops complete inline.
struct ParkReadsSink {
    parked: std::sync::Mutex<Vec<SlotCompletion>>,
}

impl SessionSink for ParkReadsSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if op.desc.op == squeezefs_ipc::layout::OP_READ {
            self.parked
                .lock()
                .expect("park mutex never poisons")
                .push(completion);
        } else {
            completion.complete(op.desc.len as i64);
        }
    }
}

/// A >slab write must ride `max_op_bytes`-sized ring ops (multi-slab arena
/// runs), not slab-sized serial chunks: 1 MiB over a 512 KiB window = 2
/// ring ops, at offsets 0 and 512 KiB.
#[test]
fn large_write_is_one_ring_op_per_max_op_window() {
    let sink = Arc::new(RecordingSink {
        writes: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, _f, session, bid) =
        sink_host_geo("wide-opcount", sink.clone(), wide_geometry());

    let buf = deterministic_bytes(1024 * 1024, 3);
    match session.ring_pwrite(bid, &buf, 0) {
        RingOutcome::Served(n) => assert_eq!(n, buf.len(), "full serve"),
        other => panic!("ring_pwrite must serve, got {other:?}"),
    }
    let mut writes = sink.writes.lock().unwrap().clone();
    writes.sort_unstable();
    assert_eq!(
        writes,
        vec![(0, 512 * 1024), (512 * 1024, 512 * 1024)],
        "1 MiB over a 512 KiB max_op window must be exactly two full-window \
         ring ops — slab-sized serial chunking is the write-matrix collapse"
    );
    host.shutdown();
}

/// Every chunk of one large op is submitted BEFORE the client waits on any
/// of them: a sink that completes nothing until both chunks arrived can
/// only be satisfied by a pipelined client (a serial client strands its
/// first chunk until the op deadline poisons the session).
#[test]
fn large_write_pipelines_all_chunks_before_waiting() {
    let sink = Arc::new(ParkUntilNSink {
        need: 2,
        parked: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, f, _default_session, _bid) =
        sink_host_geo("wide-pipeline", sink.clone(), wide_geometry());
    // Short deadline so the RED shape (serial chunk stranding) fails fast
    // instead of the 30 s default.
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();
    let session =
        Session::establish_with_op_timeout_ms(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid(), 2_000)
            .expect("session establishes");
    let bind = session.bind(f.as_raw_fd()).expect("bind succeeds");

    let buf = deterministic_bytes(1024 * 1024, 4);
    match session.ring_pwrite(bind.binding_id, &buf, 0) {
        RingOutcome::Served(n) => assert_eq!(n, buf.len(), "both chunks in flight together"),
        other => panic!(
            "a pipelined large write must serve (serial chunking strands \
             on the parked first chunk), got {other:?}"
        ),
    }
    assert!(!session.poisoned(), "no deadline fired");
    host.shutdown();
}

/// POSIX prefix semantics: the first (lowest-offset) chunk completing
/// short ends the reported prefix, regardless of later chunks.
#[test]
fn large_write_short_first_completion_reports_posix_prefix() {
    let sink = Arc::new(ShortFirstSink {
        short_n: 100_000,
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let (host, _dir, _f, session, bid) = sink_host_geo("wide-short", sink, wide_geometry());

    let buf = deterministic_bytes(1024 * 1024, 5);
    match session.ring_pwrite(bid, &buf, 0) {
        RingOutcome::Served(n) => assert_eq!(
            n, 100_000,
            "prefix ends at the first short completion (POSIX short write)"
        ),
        other => panic!("short-completion write must serve a prefix, got {other:?}"),
    }
    host.shutdown();
}

/// Error semantics: an error on the FIRST chunk is the op's errno; an
/// error after a completed prefix reports the prefix (POSIX short write —
/// the caller's retry surfaces the errno).
#[test]
fn large_write_error_semantics_prefix_or_errno() {
    // Error at offset 0 (first chunk) ⇒ Errno.
    let (host, _dir, _f, session, bid) = sink_host_geo(
        "wide-err0",
        Arc::new(ErrAtSink { err_at: 0 }),
        wide_geometry(),
    );
    let buf = deterministic_bytes(1024 * 1024, 6);
    match session.ring_pwrite(bid, &buf, 0) {
        RingOutcome::Errno(e) => assert_eq!(e, libc::EIO, "first-chunk error is the errno"),
        other => panic!("first-chunk error must surface, got {other:?}"),
    }
    host.shutdown();

    // Error at the second window ⇒ Served(first window).
    let (host, _dir, _f, session, bid) = sink_host_geo(
        "wide-err1",
        Arc::new(ErrAtSink { err_at: 512 * 1024 }),
        wide_geometry(),
    );
    match session.ring_pwrite(bid, &buf, 0) {
        RingOutcome::Served(n) => assert_eq!(
            n,
            512 * 1024,
            "mid-stream error reports the completed prefix"
        ),
        other => panic!("mid-stream error must report the prefix, got {other:?}"),
    }
    host.shutdown();
}

/// Slot accounting: a multi-slab op returns EVERY slot — the submitted
/// base and the arena-extension holds — to FREE. All 16 slots must be
/// claimable afterwards.
#[test]
fn large_write_releases_every_slot_after_serve() {
    let sink = Arc::new(RecordingSink {
        writes: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, _f, session, bid) = sink_host_geo("wide-slots", sink, wide_geometry());

    let buf = deterministic_bytes(1024 * 1024, 7);
    for _ in 0..3 {
        match session.ring_pwrite(bid, &buf, 0) {
            RingOutcome::Served(n) => assert_eq!(n, buf.len()),
            other => panic!("serve expected, got {other:?}"),
        }
    }
    // All 16 slots claimable ⇒ nothing leaked CLAIMED.
    let slab = session.slab_bytes() as usize;
    let mut tickets = Vec::new();
    for i in 0..16u64 {
        let t = session
            .submit_pwrite_nowait(bid, &buf[..slab.min(4096)], i * 4096)
            .unwrap_or_else(|| panic!("slot {i} must be claimable after large writes"));
        tickets.push(t);
    }
    for t in tickets {
        loop {
            if let Some(r) = session.poll_ticket(t, None) {
                assert!(r > 0, "ticket completes clean");
                break;
            }
            std::hint::spin_loop();
        }
    }
    host.shutdown();
}

/// Fragmented slot space still serves: with most slots held by parked
/// tickets (no 4-slab run available), a 512 KiB write must degrade to
/// smaller runs — never fail, never strand.
#[test]
fn fragmented_slots_still_serve_large_write_via_smaller_runs() {
    let sink = Arc::new(ParkReadsSink {
        parked: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, _f, session, bid) = sink_host_geo("wide-frag", sink.clone(), wide_geometry());

    // Park 13 of 16 slots behind never-completing reads.
    let mut held = Vec::new();
    for _ in 0..13 {
        held.push(
            session
                .submit_pread_nowait(bid, 4096, 0)
                .expect("hold ticket claims"),
        );
    }
    let buf = deterministic_bytes(512 * 1024, 8);
    match session.ring_pwrite(bid, &buf, 0) {
        RingOutcome::Served(n) => assert_eq!(n, buf.len(), "fragmented slots still serve fully"),
        other => panic!("fragmented large write must serve, got {other:?}"),
    }
    // Release the held slots (complete the parked reads) and consume.
    {
        let mut parked = sink.parked.lock().unwrap();
        for c in parked.drain(..) {
            c.complete(0);
        }
    }
    for t in held {
        loop {
            if let Some(_r) = session.poll_ticket(t, None) {
                break;
            }
            std::hint::spin_loop();
        }
    }
    host.shutdown();
}

// ---------------------------------------------------------------------------
// Read-saturation campaign (2026-07-29) — the READ twins of the P3 pins.
// A ring read larger than one arena slab must NOT degrade into serial
// slab-sized round trips either: on the fabric-latency field shape a 1 MiB
// il streaming read paid 16 × 64 KiB device round trips per MiB (measured:
// `ipc_ops_read`/user-op = 16.02 on the baseline rig row) while the write
// side rode ONE multi-slab ring op. The contract mirrors ring_pwrite's:
// chunks size to the full `max_op_bytes` window via contiguous multi-slab
// runs, every chunk of one op is submitted before any is waited on, POSIX
// short-read prefix semantics, slot accounting, fragmentation degradation.
// ---------------------------------------------------------------------------

/// The deterministic read pattern: byte at ABSOLUTE file offset `o` is a
/// pure function of `o`, so any chunking of a read window can be verified
/// byte-exactly against the reassembled buffer.
fn read_pattern(offset: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((offset + i as u64).wrapping_mul(31).wrapping_add(7) % 251) as u8)
        .collect()
}

/// Records every served READ `(offset, len)`, serves the deterministic
/// pattern into the arena window, completes fully.
struct PatternReadSink {
    reads: std::sync::Mutex<Vec<(u64, u32)>>,
}

impl SessionSink for PatternReadSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if op.desc.op == squeezefs_ipc::layout::OP_READ {
            self.reads
                .lock()
                .expect("recording mutex never poisons")
                .push((op.desc.offset, op.desc.len));
            op.payload
                .write(&read_pattern(op.desc.offset, op.desc.len as usize));
        }
        completion.complete(op.desc.len as i64);
    }
}

/// Serves the pattern but completes the FIRST read short (`short_n`),
/// everything after it fully — the POSIX read-prefix pin (EOF shape).
struct ShortFirstReadSink {
    short_n: i64,
    fired: std::sync::atomic::AtomicBool,
}

impl SessionSink for ShortFirstReadSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        op.payload
            .write(&read_pattern(op.desc.offset, op.desc.len as usize));
        if !self.fired.swap(true, Ordering::SeqCst) {
            completion.complete(self.short_n);
        } else {
            completion.complete(op.desc.len as i64);
        }
    }
}

/// A >slab read must ride `max_op_bytes`-sized ring ops (multi-slab arena
/// runs), not slab-sized serial chunks: 1 MiB over a 512 KiB window = 2
/// ring ops, at offsets 0 and 512 KiB — and byte-exact reassembly.
#[test]
fn large_read_is_one_ring_op_per_max_op_window() {
    let sink = Arc::new(PatternReadSink {
        reads: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, _f, session, bid) =
        sink_host_geo("wide-read-opcount", sink.clone(), wide_geometry());

    let mut buf = vec![0u8; 1024 * 1024];
    match session.ring_pread(bid, &mut buf, 0) {
        RingOutcome::Served(n) => assert_eq!(n, buf.len(), "full serve"),
        other => panic!("ring_pread must serve, got {other:?}"),
    }
    assert_eq!(buf, read_pattern(0, 1024 * 1024), "byte-exact reassembly");
    let mut reads = sink.reads.lock().unwrap().clone();
    reads.sort_unstable();
    assert_eq!(
        reads,
        vec![(0, 512 * 1024), (512 * 1024, 512 * 1024)],
        "1 MiB over a 512 KiB max_op window must be exactly two full-window \
         ring ops — slab-sized serial chunking is the il streaming-read \
         round-trip collapse (16 RTTs per MiB at default geometry)"
    );
    host.shutdown();
}

/// Every chunk of one large read is submitted BEFORE the client waits on
/// any of them: a sink that completes nothing until both chunks arrived
/// can only be satisfied by a pipelined client.
#[test]
fn large_read_pipelines_all_chunks_before_waiting() {
    let sink = Arc::new(ParkUntilNSink {
        need: 2,
        parked: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, f, _default_session, _bid) =
        sink_host_geo("wide-read-pipeline", sink.clone(), wide_geometry());
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();
    let session =
        Session::establish_with_op_timeout_ms(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid(), 2_000)
            .expect("session establishes");
    let bind = session.bind(f.as_raw_fd()).expect("bind succeeds");

    let mut buf = vec![0u8; 1024 * 1024];
    match session.ring_pread(bind.binding_id, &mut buf, 0) {
        RingOutcome::Served(n) => assert_eq!(n, buf.len(), "both chunks in flight together"),
        other => panic!(
            "a pipelined large read must serve (serial chunking strands \
             on the parked first chunk), got {other:?}"
        ),
    }
    assert!(!session.poisoned(), "no deadline fired");
    host.shutdown();
}

/// POSIX read-prefix semantics: the first (lowest-offset) chunk completing
/// short (the EOF shape) ends the reported prefix; later in-flight chunks
/// are drained and ignored.
#[test]
fn large_read_short_first_completion_reports_posix_prefix() {
    let sink = Arc::new(ShortFirstReadSink {
        short_n: 100_000,
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let (host, _dir, _f, session, bid) = sink_host_geo("wide-read-short", sink, wide_geometry());

    let mut buf = vec![0u8; 1024 * 1024];
    match session.ring_pread(bid, &mut buf, 0) {
        RingOutcome::Served(n) => {
            assert_eq!(
                n, 100_000,
                "prefix ends at the first short completion (POSIX short read)"
            );
            assert_eq!(
                &buf[..n],
                &read_pattern(0, n)[..],
                "served prefix is byte-exact"
            );
        }
        other => panic!("short-completion read must serve a prefix, got {other:?}"),
    }
    host.shutdown();
}

/// Error semantics: an error on the FIRST chunk is the op's errno; an
/// error after a completed prefix reports the prefix.
#[test]
fn large_read_error_semantics_prefix_or_errno() {
    // Error at offset 0 (first chunk) ⇒ Errno.
    let (host, _dir, _f, session, bid) = sink_host_geo(
        "wide-read-err0",
        Arc::new(ErrAtSink { err_at: 0 }),
        wide_geometry(),
    );
    let mut buf = vec![0u8; 1024 * 1024];
    match session.ring_pread(bid, &mut buf, 0) {
        RingOutcome::Errno(e) => assert_eq!(e, libc::EIO, "first-chunk error is the errno"),
        other => panic!("first-chunk error must surface, got {other:?}"),
    }
    host.shutdown();

    // Error at the second window ⇒ Served(first window).
    let (host, _dir, _f, session, bid) = sink_host_geo(
        "wide-read-err1",
        Arc::new(ErrAtSink { err_at: 512 * 1024 }),
        wide_geometry(),
    );
    let mut buf = vec![0u8; 1024 * 1024];
    match session.ring_pread(bid, &mut buf, 0) {
        RingOutcome::Served(n) => assert_eq!(
            n,
            512 * 1024,
            "mid-stream error reports the completed prefix"
        ),
        other => panic!("mid-stream error must report the prefix, got {other:?}"),
    }
    host.shutdown();
}

/// Slot accounting: a multi-slab read returns EVERY slot (submitted base +
/// arena-extension holds) to FREE — all 16 slots claimable afterwards.
#[test]
fn large_read_releases_every_slot_after_serve() {
    let sink = Arc::new(PatternReadSink {
        reads: std::sync::Mutex::new(Vec::new()),
    });
    let (host, _dir, _f, session, bid) = sink_host_geo("wide-read-slots", sink, wide_geometry());

    let mut buf = vec![0u8; 1024 * 1024];
    for _ in 0..3 {
        match session.ring_pread(bid, &mut buf, 0) {
            RingOutcome::Served(n) => assert_eq!(n, buf.len()),
            other => panic!("serve expected, got {other:?}"),
        }
    }
    let mut tickets = Vec::new();
    for i in 0..16u64 {
        let t = session
            .submit_pread_nowait(bid, 4096, i * 4096)
            .unwrap_or_else(|| panic!("slot {i} must be claimable after large reads"));
        tickets.push(t);
    }
    for t in tickets {
        loop {
            if let Some(r) = session.poll_ticket(t, None) {
                assert!(r > 0, "ticket completes clean");
                break;
            }
            std::hint::spin_loop();
        }
    }
    host.shutdown();
}

/// Fragmented slot space still serves reads via smaller runs — never
/// fails, never strands (the write pin's read twin).
#[test]
fn fragmented_slots_still_serve_large_read_via_smaller_runs() {
    let sink = Arc::new(ParkReadsUntilReleasedSink {
        parked: std::sync::Mutex::new(Vec::new()),
        park_offset: 1 << 40,
    });
    let (host, _dir, _f, session, bid) =
        sink_host_geo("wide-read-frag", sink.clone(), wide_geometry());

    // Park 13 of 16 slots behind never-completing reads at the park
    // offset; ordinary reads serve the pattern inline.
    let mut held = Vec::new();
    for _ in 0..13 {
        held.push(
            session
                .submit_pread_nowait(bid, 4096, 1 << 40)
                .expect("hold ticket claims"),
        );
    }
    let mut buf = vec![0u8; 512 * 1024];
    match session.ring_pread(bid, &mut buf, 0) {
        RingOutcome::Served(n) => {
            assert_eq!(n, buf.len(), "fragmented slots still serve fully");
            assert_eq!(
                buf,
                read_pattern(0, buf.len()),
                "byte-exact under fragmentation"
            );
        }
        other => panic!("fragmented large read must serve, got {other:?}"),
    }
    {
        let mut parked = sink.parked.lock().unwrap();
        for c in parked.drain(..) {
            c.complete(0);
        }
    }
    for t in held {
        loop {
            if let Some(_r) = session.poll_ticket(t, None) {
                break;
            }
            std::hint::spin_loop();
        }
    }
    host.shutdown();
}

/// Parks READ completions whose offset matches `park_offset` (the slot
/// occupancy fixture); every other read serves the pattern inline.
struct ParkReadsUntilReleasedSink {
    parked: std::sync::Mutex<Vec<SlotCompletion>>,
    park_offset: u64,
}

impl SessionSink for ParkReadsUntilReleasedSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        if op.desc.op == squeezefs_ipc::layout::OP_READ && op.desc.offset == self.park_offset {
            self.parked
                .lock()
                .expect("park mutex never poisons")
                .push(completion);
        } else {
            if op.desc.op == squeezefs_ipc::layout::OP_READ {
                op.payload
                    .write(&read_pattern(op.desc.offset, op.desc.len as usize));
            }
            completion.complete(op.desc.len as i64);
        }
    }
}

/// Multi-slab arena windows through the REAL data-plane sink: byte parity
/// end-to-end (the severed copy spans slabs), both transports agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_slab_ring_write_full_parity_through_real_sink() {
    let fx = Fixture::new_geo("wide-parity", wide_geometry()).await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("wide-parity.bin", libc::O_RDWR).await;
    let bind = session.bind(fd.as_raw_fd()).expect("bind succeeds");

    let w = deterministic_bytes(1024 * 1024 + 12_345, 9); // odd tail chunk
    match tokio::task::block_in_place(|| session.ring_pwrite(bind.binding_id, &w, 4096)) {
        RingOutcome::Served(n) => assert_eq!(n, w.len(), "full serve"),
        other => panic!("ring_pwrite must serve, got {other:?}"),
    }
    let fuse = fx.fuse_read(ino, 4096, w.len() as u32).await;
    assert_eq!(fuse, w, "FUSE read parity after multi-slab ring write");
    let ring = tokio::task::block_in_place(|| {
        let mut buf = vec![0u8; w.len()];
        match session.ring_pread(bind.binding_id, &mut buf, 4096) {
            RingOutcome::Served(n) => {
                buf.truncate(n);
                buf
            }
            other => panic!("ring_pread must serve, got {other:?}"),
        }
    });
    assert_eq!(ring, w, "ring read parity after multi-slab ring write");
    session.unbind(bind.binding_id);
    fx.host.shutdown();
}
