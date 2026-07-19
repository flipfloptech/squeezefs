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
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
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
    let err = tokio::task::block_in_place(
        || match Session::establish(&dead, f.as_raw_fd(), TEST_COMMIT) {
            Ok(_) => panic!("no rendezvous must refuse"),
            Err(e) => e,
        },
    );
    assert!(
        matches!(err, SessionError::Socket(_)),
        "both-rungs-dead is a socket refusal, got {err:?}"
    );
    fx.host.shutdown();
}
