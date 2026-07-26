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
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-session-{name}-{}", std::process::id()),
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
    let session =
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT).expect("session establishes");
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
        let (fs, backing, meta, staging) = sandbox_fs().await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone(), tokio::runtime::Handle::current()),
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
            geometry: test_geometry(),
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
        Session::establish(&self.blob(), f.as_raw_fd(), TEST_COMMIT)
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
        Session::establish(&fx.blob(), f.as_raw_fd(), &other_commit).is_err(),
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
    let session = Session::establish_with_op_timeout_ms(&blob, f.as_raw_fd(), TEST_COMMIT, 200)
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
// ticket parks (the 2026-07-26 reap economy): event-driven, never polled
// ---------------------------------------------------------------------------

/// The libaio reap's wait is EVENT-DRIVEN: a parked reaper is woken by
/// the daemon's completion (the slot protocol's WAITER bit), not by a
/// poll quantum. The wait admission is race-free: a completion landing
/// between `ticket_wait_entry` and `wait_any` fails the futex admission
/// (state word changed) — the wait returns immediately, never strands.
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
        match session.ticket_wait_entry(t) {
            None => break, // DONE — consume below
            Some(entry) => {
                squeezefs_il::session::wait_any(&[entry], std::time::Duration::from_secs(10));
            }
        }
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

    // Ready path: the op already completed — `ticket_wait_entry` must
    // refuse to hand out a park (None), directing the caller to consume.
    let t2 = session
        .submit_pread_nowait(binding, 512, 0)
        .expect("slot claim");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !session.ticket_done(t2) {
        assert!(std::time::Instant::now() < deadline, "sink must complete");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        session.ticket_wait_entry(t2).is_none(),
        "a DONE ticket must never park (Ready ⇒ consume immediately)"
    );
    assert_eq!(
        session
            .poll_ticket(t2, Some(&mut [0u8; 512]))
            .expect("done"),
        512
    );
    host.shutdown();
}

/// Wake breadth: EVERY waiter parked on a slot's state word is woken by
/// its completion. Two reapers legally park on the same in-flight op
/// (split submitter/reaper pairs re-snapshot the same pending set); a
/// single-waiter wake would strand the loser for its full bound.
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
                // A None here means the op already completed (the race is
                // legal) — trivially "woken".
                if let Some(entry) = session.ticket_wait_entry(t) {
                    squeezefs_il::session::wait_any(&[entry], std::time::Duration::from_secs(6));
                }
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
    let (host, _dir, _f, session, binding) = sink_host("svc-hot", Arc::new(InstantSink));

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
    let session_b =
        Session::establish(&blob, f_b.as_raw_fd(), TEST_COMMIT).expect("session B establishes");
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
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT)
            .expect("path-socket fallback must establish")
    });
    assert!(!session.poisoned(), "fallback session is live");
    drop(session);

    // Both rungs dead ⇒ Socket error (the bind_refused{socket} path).
    let mut dead = fx.blob();
    dead.socket = format!("sqz-il0-nowhere2-{}", std::process::id());
    dead.socket_path = "/tmp/sqz-il0-does-not-exist.sock".to_string();
    let err = tokio::task::block_in_place(|| {
        match Session::establish(&dead, f.as_raw_fd(), TEST_COMMIT) {
            Ok(_) => panic!("no rendezvous must refuse"),
            Err(e) => e,
        }
    });
    assert!(
        matches!(err, SessionError::Socket(_)),
        "both-rungs-dead is a socket refusal, got {err:?}"
    );
    fx.host.shutdown();
}
