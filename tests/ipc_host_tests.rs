//! PR L4-3 — IPC session host: the §5.2 daemon-side fd screen (THE security
//! boundary), bind/refusal matrix, session lifecycle, budgets, and the
//! bootstrap-xattr / KD-11 mount surfaces
//! (`docs/design-preload-interception.md` §5.2, §5.7, §6, §8).
//!
//! Every adversarial row here is a RAW-SOCKET client — no shim exists yet
//! and none is assumed: any process can speak the socket protocol directly,
//! which is exactly why the daemon screen is normative and the shim ladder
//! is only an optimization (§5.2). Every row must refuse LOUD (a counted
//! refusal class), never serve.
//!
//! NO data plane in this PR: the one served ring op is ECHO (payload
//! round-trip proof); READ/WRITE validate direction (mode screen) and then
//! complete `-ENOSYS` until PR L4-4 lands the data plane.

use squeezefs::ipc_host::{
    abstract_connect, futex_wake, path_connect, recv_ctl, send_ctl, EchoSessionSink, IpcHost,
    IpcHostConfig,
};
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_ECHO, OP_READ, OP_WRITE,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::{BootstrapBlob, CtlMsg, RefuseClass, BOOTSTRAP_XATTR, NONCE_LEN};

use squeezefs::fuse_client::METRICS;
use squeezefs::meta_backend::Metadata as _;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// harness client (raw protocol speaker — deliberately NOT a shim)
// ---------------------------------------------------------------------------

fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 1024 * 1024,
        max_op_bytes: 64 * 1024,
        _pad: 0,
    }
}

fn test_config(name: &str) -> IpcHostConfig {
    IpcHostConfig {
        socket_name: format!("sqz-il0-test-{}-{}", std::process::id(), name),
        socket_dir: None,
        build_commit: "a".repeat(40),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 16 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    }
}

fn spawn_host(name: &str) -> (Arc<IpcHost>, IpcHostConfig) {
    let cfg = test_config(name);
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host must spawn");
    (host, cfg)
}

/// A regular file on a tempdir filesystem — the "mount" stand-in whose
/// `st_dev` the host is configured to expect.
struct MountFile {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    st_dev: u64,
}

fn mount_file() -> MountFile {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bound_file");
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(&[7u8; 8192]).expect("write");
    drop(f);
    let st_dev = fstat_dev(&path);
    MountFile {
        _dir: dir,
        path,
        st_dev,
    }
}

fn fstat_dev(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).expect("metadata").dev()
}

fn open_flags(path: &std::path::Path, flags: libc::c_int) -> OwnedFd {
    let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: plain open(2); ownership taken immediately.
    let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
    assert!(
        fd >= 0,
        "open({path:?}, {flags:#o}) failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fd is a fresh, owned descriptor.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn set_recv_timeout(sock: &UnixStream, dur: Duration) {
    sock.set_read_timeout(Some(dur)).expect("SO_RCVTIMEO");
}

/// Connect + HELLO with an explicit identity; returns the socket and the
/// daemon's first reply (SessionOk carries the memfd).
fn hello(
    cfg: &IpcHostConfig,
    host: &IpcHost,
    fd: RawFd,
    commit: &str,
    nonce: [u8; NONCE_LEN],
    pid: u32,
    uid: u32,
) -> (UnixStream, CtlMsg, Option<OwnedFd>) {
    let _ = host; // the host is running; discovery is via cfg (tests skip the xattr hop)
    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    set_recv_timeout(&sock, Duration::from_secs(10));
    send_ctl(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid,
            uid,
            build_commit: commit.to_string(),
            nonce,
        },
        Some(fd),
    )
    .expect("send HELLO");
    let (reply, rx_fd) = recv_ctl(&sock).expect("recv HELLO reply");
    (sock, reply, rx_fd)
}

/// The happy-path HELLO for `cfg` (matching commit, fresh nonce, own creds).
fn hello_ok(
    cfg: &IpcHostConfig,
    host: &IpcHost,
    fd: RawFd,
) -> (UnixStream, CtlMsg, Option<OwnedFd>) {
    hello(
        cfg,
        host,
        fd,
        &cfg.build_commit,
        host.current_nonce(),
        std::process::id(),
        // SAFETY: getuid is trivially safe.
        unsafe { libc::getuid() },
    )
}

/// Client-side mapping of a received session memfd: the raw-protocol
/// harness half (what the L4-5 shim will do properly).
struct ClientSession {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
}

impl ClientSession {
    fn map(memfd: &OwnedFd, geometry: Geometry) -> ClientSession {
        let layout = SessionLayout::compute(&geometry).expect("layout");
        // SAFETY: shared mapping of the sealed memfd, full layout length.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                layout.total_bytes as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd.as_raw_fd(),
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "mmap session");
        let s = ClientSession {
            base: base as *mut u8,
            layout,
            geometry,
        };
        s.header().validate().expect("header must validate");
        assert_eq!(s.header().geometry, geometry, "geometry must round-trip");
        s
    }

    fn header(&self) -> &SessionHeader {
        // SAFETY: header page is at offset 0 of a mapping sized by layout.
        unsafe { &*(self.base as *const SessionHeader) }
    }

    fn ring(&self) -> MpscRingView<'_> {
        // SAFETY: layout offsets are inside the mapping; types are the
        // repr(C) shm protocol types the layout sized.
        unsafe {
            let tail = &*(self.base.add(self.layout.ring_off as usize) as *const AtomicU32);
            let cells = std::slice::from_raw_parts(
                self.base.add(self.layout.ring_cells_off as usize) as *const RingCell,
                self.geometry.ring_entries as usize,
            );
            MpscRingView::from_parts(tail, cells).expect("ring view")
        }
    }

    fn slot(&self, i: usize) -> &IpcSlot {
        assert!(i < self.geometry.slots as usize);
        // SAFETY: slot i is inside the slot region by the assert + layout.
        unsafe { &*((self.base.add(self.layout.slots_off as usize) as *const IpcSlot).add(i)) }
    }

    fn arena(&mut self, off: u64, len: usize) -> &mut [u8] {
        assert!(off + len as u64 <= self.geometry.arena_bytes);
        // SAFETY: bounds asserted against the arena region.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.base.add((self.layout.arena_off + off) as usize),
                len,
            )
        }
    }

    /// Submit one op on slot 0 and wait for DONE (bounded); returns result.
    fn submit_wait(&self, d: &SlotDescriptor) -> i64 {
        let slot = self.slot(0);
        let gen = slot.core.try_claim().expect("slot 0 must be FREE");
        slot.publish_descriptor(d);
        slot.core.publish_submitted();
        assert!(self.ring().push(0), "ring must accept");
        // Doorbell: correctness-only client (no elision — that is perf).
        self.header().doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&self.header().doorbell, 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !slot.core.is_done_for(gen) {
            assert!(
                Instant::now() < deadline,
                "op not completed within deadline (op {})",
                d.op
            );
            std::hint::spin_loop();
        }
        let result = slot.result();
        slot.core.release();
        result
    }
}

// SAFETY (test harness): the mapping is plain shared memory reached only
// through atomics and bounded raw copies; one test thread owns a given
// `ClientSession` at a time (the VAL-5e saturator MOVES it, never shares).
unsafe impl Send for ClientSession {}

impl Drop for ClientSession {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `map`.
        unsafe {
            libc::munmap(
                self.base as *mut libc::c_void,
                self.layout.total_bytes as usize,
            );
        }
    }
}

fn expect_refuse(reply: &CtlMsg, class: RefuseClass, what: &str) {
    match reply {
        CtlMsg::Refuse { class: c } => assert_eq!(*c, class, "{what}: refusal class"),
        other => panic!("{what}: expected Refuse({class:?}), got {other:?}"),
    }
}

fn expect_bind_refused(reply: &CtlMsg, class: RefuseClass, what: &str) {
    match reply {
        CtlMsg::BindRefused { class: c } => assert_eq!(*c, class, "{what}: refusal class"),
        other => panic!("{what}: expected BindRefused({class:?}), got {other:?}"),
    }
}

/// Establish a full session (HELLO ok) and return the mapped client half.
fn establish(cfg: &IpcHostConfig, host: &IpcHost, fd: RawFd) -> (UnixStream, ClientSession) {
    let (sock, reply, memfd) = hello_ok(cfg, host, fd);
    let geometry = match reply {
        CtlMsg::SessionOk { geometry } => geometry,
        other => panic!("expected SessionOk, got {other:?}"),
    };
    let memfd = memfd.expect("SessionOk must carry the memfd");
    let session = ClientSession::map(&memfd, geometry);
    (sock, session)
}

/// BIND one fd on an established session; returns the daemon's reply.
fn bind(sock: &UnixStream, fd: RawFd) -> CtlMsg {
    send_ctl(sock, &CtlMsg::Bind, Some(fd)).expect("send BIND");
    let (reply, none) = recv_ctl(sock).expect("recv BIND reply");
    assert!(none.is_none(), "BIND replies carry no fd");
    reply
}

fn wait_sessions_active(expect: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while METRICS.ipc_sessions_active.load(Ordering::Relaxed) != expect {
        assert!(
            Instant::now() < deadline,
            "{what}: ipc_sessions_active never reached {expect} (now {})",
            METRICS.ipc_sessions_active.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// version / nonce / peercred (HELLO-level screen)
// ---------------------------------------------------------------------------

#[test]
fn hello_on_control_plane_only_host_refuses_class_disabled() {
    // User directive 2026-07-25 (reason-bearing shim refusal lines): a
    // mount without `-o interception` arms the VL2 control-plane-only
    // host, which refuses every data-plane HELLO — previously with the
    // opaque `Flags` class (indistinguishable from a credential-fd
    // screen refusal). The daemon KNOWS the reason; the wire must carry
    // it so the shim can print "interception not armed on this mount".
    let mut cfg = test_config("ctlonly");
    cfg.data_plane = false;
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host must spawn");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    let (_sock, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &cfg.build_commit,
        host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() },
    );
    expect_refuse(
        &reply,
        RefuseClass::Disabled,
        "control-plane-only mount must name its refusal (not the Flags screen)",
    );
}

#[test]
fn hello_version_skew_refuses_including_unknown_and_dirty() {
    let (host, cfg) = spawn_host("skew");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    let before = METRICS.ipc_bind_refused_version.load(Ordering::Relaxed);

    // Wrong ABI.
    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    set_recv_timeout(&sock, Duration::from_secs(10));
    send_ctl(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI + 1,
            pid: std::process::id(),
            uid: unsafe { libc::getuid() },
            build_commit: cfg.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        Some(fd.as_raw_fd()),
    )
    .expect("send");
    let (reply, _) = recv_ctl(&sock).expect("recv");
    expect_refuse(&reply, RefuseClass::Version, "abi skew");

    // Wrong commit.
    let (_s2, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &"b".repeat(40),
        host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() },
    );
    expect_refuse(&reply, RefuseClass::Version, "commit skew");

    // Degenerate identities prove nothing by equality (KD-7): `unknown`
    // on BOTH sides refuses; `-dirty` on both sides refuses.
    let unknown_host_cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-test-{}-unknown", std::process::id()),
        socket_dir: None,
        build_commit: "unknown".to_string(),
        ..cfg.clone()
    };
    let unknown_host =
        IpcHost::spawn(unknown_host_cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    unknown_host.set_expected_st_dev(mf.st_dev);
    let (_s3, reply, _) = hello(
        &unknown_host_cfg,
        &unknown_host,
        fd.as_raw_fd(),
        "unknown",
        unknown_host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() },
    );
    expect_refuse(&reply, RefuseClass::Version, "unknown == unknown");

    let dirty_commit = format!("{}-dirty", "c".repeat(40));
    let dirty_host_cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-test-{}-dirty", std::process::id()),
        socket_dir: None,
        build_commit: dirty_commit.clone(),
        ..cfg.clone()
    };
    let dirty_host =
        IpcHost::spawn(dirty_host_cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    dirty_host.set_expected_st_dev(mf.st_dev);
    let (_s4, reply, _) = hello(
        &dirty_host_cfg,
        &dirty_host,
        fd.as_raw_fd(),
        &dirty_commit,
        dirty_host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() },
    );
    expect_refuse(&reply, RefuseClass::Version, "-dirty == -dirty");

    assert!(
        METRICS.ipc_bind_refused_version.load(Ordering::Relaxed) >= before + 4,
        "every version-skew row must be counted"
    );

    // The counted dev override (`SQUEEZEFS_IPC_ALLOW_DEV` posture, config-
    // injected here): the SAME dirty pair binds, and the override counts.
    let dev_before = METRICS.ipc_binds_dev_override.load(Ordering::Relaxed);
    let allow_cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-test-{}-allowdev", std::process::id()),
        socket_dir: None,
        build_commit: dirty_commit.clone(),
        allow_dev: true,
        ..cfg.clone()
    };
    let allow_host = IpcHost::spawn(allow_cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    allow_host.set_expected_st_dev(mf.st_dev);
    let (_s5, reply, memfd) = hello(
        &allow_cfg,
        &allow_host,
        fd.as_raw_fd(),
        &dirty_commit,
        allow_host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() },
    );
    assert!(
        matches!(reply, CtlMsg::SessionOk { .. }),
        "allow_dev must admit the dirty pair, got {reply:?}"
    );
    assert!(memfd.is_some());
    assert!(
        METRICS.ipc_binds_dev_override.load(Ordering::Relaxed) > dev_before,
        "the dev override is COUNTED (fleet-hygiene alarm surface)"
    );
    allow_host.shutdown();
    unknown_host.shutdown();
    dirty_host.shutdown();
    host.shutdown();
}

#[test]
fn hello_stale_nonce_refuses() {
    let (host, cfg) = spawn_host("nonce");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    // The immediately-previous nonce is still accepted (rotation race).
    let prev = host.current_nonce();
    host.rotate_nonce_for_test();
    let (_s1, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &cfg.build_commit,
        prev,
        std::process::id(),
        unsafe { libc::getuid() },
    );
    assert!(
        matches!(reply, CtlMsg::SessionOk { .. }),
        "previous nonce inside the rotation window must be accepted, got {reply:?}"
    );

    // Two rotations later it is stale.
    let stale = prev;
    host.rotate_nonce_for_test();
    let before = METRICS.ipc_bind_refused_nonce.load(Ordering::Relaxed);
    let (_s2, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &cfg.build_commit,
        stale,
        std::process::id(),
        unsafe { libc::getuid() },
    );
    expect_refuse(&reply, RefuseClass::Nonce, "stale nonce");
    assert!(METRICS.ipc_bind_refused_nonce.load(Ordering::Relaxed) > before);
    host.shutdown();
}

#[test]
fn hello_peercred_mismatch_refuses() {
    let (host, cfg) = spawn_host("peercred");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    let before = METRICS.ipc_bind_refused_peercred.load(Ordering::Relaxed);
    // Claimed pid ≠ SO_PEERCRED pid: defense-in-depth vs replayed HELLO
    // bytes — the kernel-supplied peercred is the truth.
    let (_s, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &cfg.build_commit,
        host.current_nonce(),
        std::process::id() + 1,
        unsafe { libc::getuid() },
    );
    expect_refuse(&reply, RefuseClass::Peercred, "pid mismatch");

    // Claimed uid ≠ SO_PEERCRED uid.
    let (_s2, reply, _) = hello(
        &cfg,
        &host,
        fd.as_raw_fd(),
        &cfg.build_commit,
        host.current_nonce(),
        std::process::id(),
        unsafe { libc::getuid() } + 1,
    );
    expect_refuse(&reply, RefuseClass::Peercred, "uid mismatch");
    assert!(METRICS.ipc_bind_refused_peercred.load(Ordering::Relaxed) >= before + 2);
    host.shutdown();
}

// ---------------------------------------------------------------------------
// the §5.2 fd screen (adversarial rows — every one must refuse)
// ---------------------------------------------------------------------------

#[test]
fn fd_screen_refuses_adversarial_fds_at_hello_and_bind() {
    let (host, cfg) = spawn_host("screen");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);

    let flags_before = METRICS.ipc_bind_refused_flags.load(Ordering::Relaxed);
    let mode_before = METRICS.ipc_bind_refused_mode.load(Ordering::Relaxed);

    // --- HELLO-level rows (session refused outright) ---

    // O_PATH: obtainable with search-only permission; access-mode bits
    // read O_RDONLY — the load-bearing explicit rejection (§5.2 rule 2).
    let opath = open_flags(&mf.path, libc::O_PATH);
    let (_s, reply, _) = hello_ok(&cfg, &host, opath.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Flags, "O_PATH at HELLO");

    // Directory fd (non-S_ISREG → flags class, §5.2 rule 1).
    let dirfd = open_flags(
        mf.path.parent().unwrap(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    );
    let (_s, reply, _) = hello_ok(&cfg, &host, dirfd.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Flags, "directory fd at HELLO");

    // Wrong st_dev: a REGULAR file on a different filesystem (memfd) —
    // passes S_ISREG, fails the mount-device check (class `mode`: the fd
    // is not a rights-bearing capability on this mount).
    let wrong_dev = memfd_regular_file();
    let (_s, reply, _) = hello_ok(&cfg, &host, wrong_dev.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Mode, "wrong-st_dev at HELLO");

    // --- BIND-level rows (session established with a good fd first) ---
    let good = open_flags(&mf.path, libc::O_RDWR);
    let (sock, _session) = establish(&cfg, &host, good.as_raw_fd());

    let reply = bind(&sock, opath.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "O_PATH at BIND");

    let reply = bind(&sock, dirfd.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "directory at BIND");

    let reply = bind(&sock, wrong_dev.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Mode, "wrong-st_dev at BIND");

    // O_APPEND (append needs an atomic size authority round trip — v1
    // refuses, §5.4.1 — enforced DAEMON-side, not merely shim-side).
    let append = open_flags(&mf.path, libc::O_RDWR | libc::O_APPEND);
    let reply = bind(&sock, append.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "O_APPEND at BIND");

    // O_SYNC / O_DSYNC (per-op durable barriers — kernel path provides).
    let osync = open_flags(&mf.path, libc::O_RDWR | libc::O_SYNC);
    let reply = bind(&sock, osync.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "O_SYNC at BIND");

    let odsync = open_flags(&mf.path, libc::O_RDWR | libc::O_DSYNC);
    let reply = bind(&sock, odsync.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "O_DSYNC at BIND");

    // O_TMPFILE-class: an unnamed regular file on the RIGHT device.
    let tmpfile = open_flags(mf.path.parent().unwrap(), libc::O_TMPFILE | libc::O_RDWR);
    let reply = bind(&sock, tmpfile.as_raw_fd());
    expect_bind_refused(&reply, RefuseClass::Flags, "O_TMPFILE at BIND");

    assert!(
        METRICS.ipc_bind_refused_flags.load(Ordering::Relaxed) >= flags_before + 7,
        "every flags-class refusal is counted"
    );
    assert!(
        METRICS.ipc_bind_refused_mode.load(Ordering::Relaxed) >= mode_before + 2,
        "every mode-class refusal is counted"
    );
    host.shutdown();
}

fn memfd_regular_file() -> OwnedFd {
    // SAFETY: memfd_create + write; ownership taken immediately.
    unsafe {
        let fd = libc::memfd_create(c"sqz-il0-wrongdev".as_ptr(), libc::MFD_CLOEXEC);
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        let fd = OwnedFd::from_raw_fd(fd);
        let buf = [1u8; 4096];
        assert_eq!(
            libc::write(
                fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len()
            ),
            4096
        );
        fd
    }
}

// ---------------------------------------------------------------------------
// per-op rights (both directions), ECHO happy path, no data plane
// ---------------------------------------------------------------------------

#[test]
fn wrong_direction_ring_ops_refuse_ebadf_both_directions() {
    let (host, cfg) = spawn_host("direction");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let good = open_flags(&mf.path, libc::O_RDWR);
    let (sock, session) = establish(&cfg, &host, good.as_raw_fd());

    // O_WRONLY binding: ring READS refused (§5.2 screen rule 3 — the
    // symmetric direction; surfaces as EBADF like the kernel would).
    let wronly = open_flags(&mf.path, libc::O_WRONLY);
    let wr_binding = match bind(&sock, wronly.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("O_WRONLY must bind (write-only rights), got {other:?}"),
    };

    let rejects_before = METRICS.ipc_descriptor_rejects.load(Ordering::Relaxed);
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_READ,
        flags: 0,
        binding: wr_binding,
        offset: 0,
        len: 4096,
        arena_off: 0,
    });
    assert_eq!(r, -libc::EBADF as i64, "O_WRONLY ring read must be EBADF");

    // O_RDONLY binding: ring WRITES refused.
    let rdonly = open_flags(&mf.path, libc::O_RDONLY);
    let rd_binding = match bind(&sock, rdonly.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("O_RDONLY must bind (read-only rights), got {other:?}"),
    };
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_WRITE,
        flags: 0,
        binding: rd_binding,
        offset: 0,
        len: 4096,
        arena_off: 0,
    });
    assert_eq!(r, -libc::EBADF as i64, "O_RDONLY ring write must be EBADF");

    assert!(
        METRICS.ipc_descriptor_rejects.load(Ordering::Relaxed) >= rejects_before + 2,
        "wrong-direction ops are descriptor rejects (attack/bug tripwire)"
    );
    host.shutdown();
}

#[test]
fn happy_path_bind_serves_echo_and_only_echo() {
    let (host, cfg) = spawn_host("echo");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let good = open_flags(&mf.path, libc::O_RDWR);

    let binds_before = METRICS.ipc_binds.load(Ordering::Relaxed);
    let (sock, mut session) = establish(&cfg, &host, good.as_raw_fd());

    let binding = match bind(&sock, good.as_raw_fd()) {
        CtlMsg::BindOk {
            binding_id,
            ino,
            read_ok,
            write_ok,
        } => {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                ino,
                std::fs::metadata(&mf.path).unwrap().ino(),
                "binding ino derives from the received fd's fstat"
            );
            assert!(read_ok && write_ok, "O_RDWR grants both directions");
            binding_id
        }
        other => panic!("expected BindOk, got {other:?}"),
    };
    assert!(
        METRICS.ipc_binds.load(Ordering::Relaxed) > binds_before,
        "successful binds are counted"
    );

    // ECHO: payload round-trip proof — result is the byte sum (proves the
    // daemon READ it), payload comes back bitwise-inverted (proves the
    // daemon WROTE it).
    let len = 4096usize;
    let arena_off = 8192u64;
    let payload = session.arena(arena_off, len);
    let mut sum = 0i64;
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i % 251) as u8;
        sum += i64::from(*b);
    }
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding,
        offset: 0,
        len: len as u32,
        arena_off,
    });
    assert_eq!(r, sum, "ECHO result is the request byte sum");
    let echoed = session.arena(arena_off, len);
    for (i, b) in echoed.iter().enumerate() {
        assert_eq!(*b, !((i % 251) as u8), "ECHO payload is bitwise-inverted");
    }

    // NO data plane in this PR: correctly-directed READ/WRITE complete
    // -ENOSYS (PR L4-4 lands the serve).
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_READ,
        flags: 0,
        binding,
        offset: 0,
        len: 4096,
        arena_off: 0,
    });
    assert_eq!(r, -libc::ENOSYS as i64, "READ has no data plane yet");
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_WRITE,
        flags: 0,
        binding,
        offset: 0,
        len: 4096,
        arena_off: 0,
    });
    assert_eq!(r, -libc::ENOSYS as i64, "WRITE has no data plane yet");

    // Malformed descriptors: arena overflow / oversize len / dead binding
    // — all -EINVAL, all counted (never a panic, never a serve).
    let rejects_before = METRICS.ipc_descriptor_rejects.load(Ordering::Relaxed);
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding,
        offset: 0,
        len: 4096,
        arena_off: session.geometry.arena_bytes - 512, // spills past the arena
    });
    assert_eq!(r, -libc::EINVAL as i64, "arena-bounds violation is EINVAL");
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding,
        offset: 0,
        len: session.geometry.max_op_bytes + 1,
        arena_off: 0,
    });
    assert_eq!(r, -libc::EINVAL as i64, "len > max_op_bytes is EINVAL");
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding: binding + 999,
        offset: 0,
        len: 512,
        arena_off: 0,
    });
    assert_eq!(r, -libc::EINVAL as i64, "dead binding id is EINVAL");
    let r = session.submit_wait(&SlotDescriptor {
        op: 77, // unknown op
        flags: 0,
        binding,
        offset: 0,
        len: 512,
        arena_off: 0,
    });
    assert_eq!(r, -libc::EINVAL as i64, "unknown op is EINVAL");
    assert!(
        METRICS.ipc_descriptor_rejects.load(Ordering::Relaxed) >= rejects_before + 4,
        "malformed descriptors are counted"
    );

    // UNBIND releases the binding: the id goes dead.
    send_ctl(
        &sock,
        &CtlMsg::Unbind {
            binding_id: binding,
        },
        None,
    )
    .expect("send UNBIND");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = session.submit_wait(&SlotDescriptor {
            op: OP_ECHO,
            flags: 0,
            binding,
            offset: 0,
            len: 512,
            arena_off: 0,
        });
        if r == -libc::EINVAL as i64 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unbound binding must go dead (still serving)"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    host.shutdown();
}

// ---------------------------------------------------------------------------
// budgets: arena admission, per-uid caps, shed = refuse-new-sessions
// ---------------------------------------------------------------------------

#[test]
fn arena_budget_and_uid_caps_refuse_admission() {
    let mf = mount_file();

    // Arena cap below one arena: every session refused, class budget.
    let mut cfg = test_config("budget");
    cfg.arena_cap_bytes = cfg.geometry.arena_bytes - 1;
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let budget_before = METRICS.ipc_bind_refused_budget.load(Ordering::Relaxed);
    let adm_before = METRICS.ipc_admission_refusals.load(Ordering::Relaxed);
    let (_s, reply, _) = hello_ok(&cfg, &host, fd.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Budget, "arena over cap");
    assert!(METRICS.ipc_bind_refused_budget.load(Ordering::Relaxed) > budget_before);
    assert!(
        METRICS.ipc_admission_refusals.load(Ordering::Relaxed) > adm_before,
        "budget admission refusals feed the R5 counter too"
    );
    host.shutdown();

    // Per-uid session cap 1: second concurrent session refused.
    let mut cfg = test_config("uidcap");
    cfg.per_uid_session_cap = 1;
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    host.set_expected_st_dev(mf.st_dev);
    let (_live_sock, _live_session) = establish(&cfg, &host, fd.as_raw_fd());
    let (_s2, reply, _) = hello_ok(&cfg, &host, fd.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Budget, "per-uid session cap");
    host.shutdown();
}

#[test]
fn shed_refuses_new_sessions_never_tears_live_ones() {
    let (host, cfg) = spawn_host("shed");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    let (sock, mut session) = establish(&cfg, &host, fd.as_raw_fd());
    let live_arena = host.arena_bytes();
    assert!(
        live_arena >= cfg.geometry.arena_bytes,
        "gauge counts the arena"
    );

    // Shed to 0: new sessions refuse (class budget)…
    host.shed_to(0);
    let (_s2, reply, _) = hello_ok(&cfg, &host, fd.as_raw_fd());
    expect_refuse(&reply, RefuseClass::Budget, "shed target refusal");

    // …but the LIVE session is never torn (R5 never-lossy): its ring op
    // still serves.
    let binding = match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("live session must keep serving, got {other:?}"),
    };
    let payload = session.arena(0, 8);
    payload.copy_from_slice(&[1, 1, 1, 1, 1, 1, 1, 1]);
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding,
        offset: 0,
        len: 8,
        arena_off: 0,
    });
    assert_eq!(r, 8, "live session serves through a shed");

    // Recovery: when the gauge is back under target, admission reopens.
    drop(sock);
    drop(session);
    wait_sessions_active(0, "teardown after socket drop");
    let (_s3, reply, _) = hello_ok(&cfg, &host, fd.as_raw_fd());
    assert!(
        matches!(reply, CtlMsg::SessionOk { .. }),
        "admission reopens once under the shed target, got {reply:?}"
    );
    host.shutdown();
}

#[test]
fn session_teardown_on_eof_frees_arena_and_gauges() {
    let (host, cfg) = spawn_host("eof");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);

    let total_before = METRICS.ipc_sessions_total.load(Ordering::Relaxed);
    let (sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    assert!(METRICS.ipc_sessions_total.load(Ordering::Relaxed) > total_before);
    assert!(host.arena_bytes() >= cfg.geometry.arena_bytes);
    assert_eq!(
        host.arena_bytes(),
        METRICS.ipc_arena_bytes.load(Ordering::Relaxed),
        "the component gauge IS the host gauge"
    );

    drop(sock); // socket EOF = client death (SEQPACKET)
    drop(session);
    wait_sessions_active(0, "EOF teardown");
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.arena_bytes() != 0 {
        assert!(Instant::now() < deadline, "arena gauge must return to 0");
        std::thread::sleep(Duration::from_millis(5));
    }
    host.shutdown();
}

// ---------------------------------------------------------------------------
// mem_budget integration (cap arithmetic + the component registration)
// ---------------------------------------------------------------------------

#[test]
fn ipc_arena_cap_is_budget_fraction_no_fixed_ceiling() {
    use squeezefs::mem_budget::{ipc_arena_cap, resolve_budget_from};
    let mib = 1024 * 1024u64;
    let gib = 1024 * mib;
    // Small box: the established fraction (budget/8 = 12.5 %) unchanged.
    assert_eq!(ipc_arena_cap(8 * gib), gib);
    assert_eq!(ipc_arena_cap(0), 0);
    // The 251 GB field client (2026-08-01 rewrite-publish-drain rider):
    // 70 % RAM budget ≈ 176 GiB. The deleted 2 GiB ceiling clamped the
    // pool to ~31 × 64 MiB sessions against a 48-HELLO fleet
    // (ipc_bind_refused_budget 13/48, engagement-INVALID il rows). The
    // derived cap must admit the whole fleet with headroom.
    let field_budget = resolve_budget_from(None, None, None, 251 * 1000 * 1000 * 1000);
    let cap = ipc_arena_cap(field_budget);
    assert_eq!(cap, field_budget / 8, "the fraction, nothing else");
    assert!(
        cap >= 48 * 64 * mib,
        "48 × 64 MiB sessions must fit the derived cap (got {cap} B)"
    );
    // Huge budgets scale with the machine — no fixed byte ceiling
    // anywhere (the budget is already machine-derived; per-uid session
    // caps + idle reap + R5 shedding bound the pool).
    assert_eq!(ipc_arena_cap(u64::MAX / 2), u64::MAX / 2 / 8);
    // Tie test (the il_sessions_default pattern): the documented default
    // percentage IS the /8 derivation — drift between the constant and
    // the arithmetic is a red test.
    let budget = 8 * gib;
    assert_eq!(
        (budget as f64 * (squeezefs::mem_budget::IPC_ARENA_CAP_DEFAULT_PCT / 100.0)) as u64,
        ipc_arena_cap(budget),
        "IPC_ARENA_CAP_DEFAULT_PCT must equal the budget/8 derivation"
    );
}

#[test]
fn ipc_arena_cap_resolution_precedence_absolute_pct_default() {
    use squeezefs::mem_budget::resolve_ipc_arena_cap;
    let mib = 1024 * 1024u64;
    let gib = 1024 * mib;
    // Absolute (SQUEEZEFS_IPC_MEM_MAX, MiB) wins verbatim over both.
    assert_eq!(
        resolve_ipc_arena_cap(8 * gib, Some("8192"), Some("50")),
        8192 * mib
    );
    // Explicit-wins-verbatim includes 0 (compat semantics unchanged).
    assert_eq!(resolve_ipc_arena_cap(8 * gib, Some("0"), Some("50")), 0);
    // Percentage (SQUEEZEFS_IPC_MEM_PCT) beats the derived default.
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("50")), 4 * gib);
    // Fractional percentages are legal (the default fraction IS 12.5).
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("12.5")), gib);
    // Neither set: the derived default (budget/8 = 12.5 %).
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, None), gib);
    // Garbage absolute falls through to the percentage.
    assert_eq!(
        resolve_ipc_arena_cap(8 * gib, Some("lots"), Some("25")),
        2 * gib
    );
}

#[test]
fn ipc_arena_cap_pct_clamps_and_ignores_garbage() {
    use squeezefs::mem_budget::resolve_ipc_arena_cap;
    let gib = 1024 * 1024 * 1024u64;
    // pct clamps into (0, 100]: over-100 clamps to 100 (warn, not refuse).
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("150")), 8 * gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("100")), 8 * gib);
    // Non-positive / non-finite / garbage percentages warn and fall back
    // to the derived default (the IPC knob-family convention: never
    // panic, never fail the mount on a bad env string).
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("0")), gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("-5")), gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("NaN")), gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("inf")), gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("pct")), gib);
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some("")), gib);
    // Whitespace is trimmed like every sibling knob.
    assert_eq!(resolve_ipc_arena_cap(8 * gib, None, Some(" 25 ")), 2 * gib);
    assert_eq!(
        resolve_ipc_arena_cap(8 * gib, Some(" 1024 "), None),
        gib,
        "absolute MiB trims whitespace too"
    );
}

#[test]
fn ipc_session_arena_component_registers_and_sheds() {
    use squeezefs::mem_budget::{register_ipc_session_arena_component, MemBudget};
    let mb = MemBudget::new_for_test();
    let gauge = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let shed_seen = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    let g = gauge.clone();
    let s = shed_seen.clone();
    register_ipc_session_arena_component(
        &mb,
        Arc::new(move || g.load(Ordering::Relaxed)),
        Arc::new(move |target| s.store(target, Ordering::Relaxed)),
    );
    // Fill the gauge past a red budget and tick: the component's shed
    // must be invoked with a target below the current gauge.
    gauge.store(1000, Ordering::Relaxed);
    for _ in 0..3 {
        mb.tick_inner(1000, 1000, 1000);
    }
    let seen = shed_seen.load(Ordering::Relaxed);
    assert!(
        seen < 1000,
        "red pressure must shed the ipc_session_arenas component (target {seen})"
    );
}

// ---------------------------------------------------------------------------
// KD-11: interception forces kernel write-through; explicit writeback+
// interception is refused loud
// ---------------------------------------------------------------------------

#[test]
fn kd11_interception_forces_write_through_and_refuses_explicit_writeback() {
    use squeezefs::fuse_client::resolve_interception_posture;

    // No interception anywhere: writeback posture passes through.
    let p = resolve_interception_posture(None, false, false, true).expect("plain mount");
    assert!(!p.interception);
    assert!(p.write_back);
    let p = resolve_interception_posture(None, false, false, false).expect("no-writeback mount");
    assert!(!p.write_back);

    // `-o interception` (or the CLI flag / SQUEEZEFS_IPC=1) forces
    // write_back = false even though the mount default is writeback-on.
    for (opts, flag, env) in [
        (Some("interception"), false, false),
        (None, true, false),
        (None, false, true),
        (Some("max_read=1048576,interception"), false, false),
    ] {
        let p = resolve_interception_posture(opts, flag, env, true)
            .unwrap_or_else(|e| panic!("interception mount must resolve, got {e}"));
        assert!(p.interception, "opts={opts:?} flag={flag} env={env}");
        assert!(
            !p.write_back,
            "KD-11: interception forces kernel write-through (opts={opts:?})"
        );
    }

    // Explicit writeback request + interception: refused LOUD.
    for opts in [
        "interception,writeback",
        "writeback,interception",
        "interception,writeback_cache",
    ] {
        let err = resolve_interception_posture(Some(opts), false, false, true)
            .expect_err("explicit writeback + interception must refuse");
        assert!(
            err.contains("interception") && (err.contains("writeback") || err.contains("write")),
            "the refusal names the conflict: {err}"
        );
    }

    // Explicit writeback WITHOUT interception is honored.
    let p = resolve_interception_posture(Some("writeback"), false, false, false)
        .expect("explicit writeback alone resolves");
    assert!(p.write_back);
    assert!(!p.interception);
}

// ---------------------------------------------------------------------------
// bootstrap xattr surface + stats fields (full FS fixture)
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

    let dlm = DlmClient::new().unwrap();
    let backing_temp = tempfile::NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new("ipc_host_tests")
            .await
            .expect("block allocator"),
    );
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_xattr_synthesis_filter_and_stats_fields() {
    use fuse3::raw::prelude::Filesystem;
    use fuse3::raw::Request;
    use std::ffi::OsStr;

    let (fs, _b, _m, _s) = sandbox_fs().await;
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
        ..Default::default()
    };

    let create = fs
        .create(req, 1, OsStr::new("il0.txt"), libc::S_IFREG | 0o644, 0)
        .await
        .expect("create");
    let ino = create.attr.ino;

    // Interception DISABLED: the reserved name is not synthesized (ENODATA
    // — the cheap negative probe non-enabled mounts answer).
    let err = fs
        .getxattr(req, ino, OsStr::new(BOOTSTRAP_XATTR), 4096)
        .await
        .expect_err("disabled mount must not synthesize the blob");
    assert_eq!(libc::c_int::from(err), -libc::ENODATA);

    // setxattr on the reserved name: EPERM even while disabled (the name
    // is reserved unconditionally).
    let err = fs
        .setxattr(req, ino, OsStr::new(BOOTSTRAP_XATTR), b"forged", 0, 0)
        .await
        .expect_err("reserved name is never writable");
    assert_eq!(libc::c_int::from(err), -libc::EPERM);

    // Arm the host (what `start_mount -o interception` does).
    let cfg = test_config("xattr");
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host must spawn");
    host.set_expected_st_dev(4242);
    fs.ipc_host.store(Arc::new(Some(host.clone())));

    // Synthesis: the blob decodes and carries the handshake identity.
    let reply = fs
        .getxattr(req, ino, OsStr::new(BOOTSTRAP_XATTR), 4096)
        .await
        .expect("armed mount synthesizes the blob");
    let data = match reply {
        fuse3::raw::reply::ReplyXAttr::Data(d) => d,
        other => panic!("expected data, got {other:?}"),
    };
    let blob = BootstrapBlob::decode(&data).expect("blob decodes");
    assert_eq!(blob.abi, squeezefs_ipc::layout::IPC_ABI);
    assert_eq!(blob.build_commit, cfg.build_commit);
    assert_eq!(blob.socket, cfg.socket_name);
    assert_eq!(blob.nonce, host.current_nonce());
    assert!(blob.socket_path.is_empty(), "OQ-6 path field is reserved");

    // Size-probe contract (size == 0 returns the length).
    let reply = fs
        .getxattr(req, ino, OsStr::new(BOOTSTRAP_XATTR), 0)
        .await
        .expect("size probe");
    match reply {
        fuse3::raw::reply::ReplyXAttr::Size(n) => assert_eq!(n as usize, data.len()),
        other => panic!("expected size, got {other:?}"),
    }

    // listxattr filter: a historical/foreign on-disk key under the
    // reserved name is never listed.
    fs.meta_backend
        .as_ref()
        .unwrap()
        .setxattr(ino, BOOTSTRAP_XATTR, b"stale-on-disk")
        .await
        .expect("plant on-disk key");
    fs.meta_backend
        .as_ref()
        .unwrap()
        .setxattr(ino, "user.other", b"visible")
        .await
        .expect("plant sibling key");
    let reply = fs.listxattr(req, ino, 4096).await.expect("listxattr");
    let names = match reply {
        fuse3::raw::reply::ReplyXAttr::Data(d) => d,
        other => panic!("expected data, got {other:?}"),
    };
    let names: Vec<&[u8]> = names.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    assert!(
        names.iter().any(|n| *n == b"user.other"),
        "sibling keys stay listed"
    );
    assert!(
        !names.iter().any(|n| *n == BOOTSTRAP_XATTR.as_bytes()),
        "the reserved name is filtered from listxattr"
    );

    // setxattr still EPERM while armed.
    let err = fs
        .setxattr(req, ino, OsStr::new(BOOTSTRAP_XATTR), b"forged", 0, 0)
        .await
        .expect_err("reserved name is never writable");
    assert_eq!(libc::c_int::from(err), -libc::EPERM);

    // §8 stats surface: the ipc_* family exports on the stats inode —
    // under the "metrics" object like every counter family
    // (tests/metrics_tests.rs house style).
    let stats = fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json parses");
    let v = v
        .get("metrics")
        .expect("stats JSON carries a metrics object");
    for key in [
        "ipc_sessions_active",
        "ipc_sessions_total",
        "ipc_binds",
        "ipc_bind_refused_version",
        "ipc_bind_refused_nonce",
        "ipc_bind_refused_flags",
        "ipc_bind_refused_mode",
        "ipc_bind_refused_budget",
        "ipc_bind_refused_peercred",
        "ipc_binds_dev_override",
        "ipc_admission_refusals",
        "ipc_arena_bytes",
        "ipc_descriptor_rejects",
        "ipc_sessions_poisoned",
    ] {
        assert!(
            v.get(key).is_some(),
            "stats inode must export {key} (got keys: {:?})",
            v.as_object().map(|o| o.len())
        );
    }
    host.shutdown();
}

// ---------------------------------------------------------------------------
// OQ-6 (v1.1): path-based ctl socket for container netns
// ---------------------------------------------------------------------------
//
// Abstract AF_UNIX names are per network namespace (§5.2 known
// limitation): a containerized app with the mount bind-mounted in but
// its own netns can read the bootstrap xattr yet never connect. The
// host therefore ALSO binds a filesystem-path SOCK_SEQPACKET socket
// under a runtime dir and advertises it in the blob's reserved
// `socket_path` field (carried since v1 — no ABI bump). The path
// socket grants nothing the abstract one does not: SO_PEERCRED + the
// §5.2 daemon fd screen remain the security boundary; the socket file
// is 0666 BECAUSE connecting is not a credential.

fn test_config_with_dir(name: &str, dir: &std::path::Path) -> IpcHostConfig {
    let mut cfg = test_config(name);
    cfg.socket_dir = Some(dir.to_path_buf());
    cfg
}

#[test]
fn path_socket_binds_advertises_and_serves_a_full_establish() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = test_config_with_dir("path-rt", dir.path());
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host must spawn");

    // Advertised in the blob's reserved field, and live on disk 0666
    // (any uid that can read the mount may connect; connecting is not
    // a credential — the fd screen is).
    let blob = squeezefs_ipc::wire::BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    let expected = dir
        .path()
        .join(format!("{}.sock", cfg.socket_name))
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        blob.socket_path, expected,
        "blob advertises the path socket"
    );
    assert_eq!(blob.socket, cfg.socket_name, "abstract stays primary");
    let meta = std::fs::metadata(&blob.socket_path).expect("socket file exists");
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    assert!(meta.file_type().is_socket(), "must be a unix socket");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o666,
        "socket file mode is 0666"
    );

    // A full HELLO → SessionOk → BIND round trip over the PATH socket
    // (identical protocol; only the rendezvous differs).
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let sock = path_connect(&blob.socket_path).expect("connect via path");
    set_recv_timeout(&sock, Duration::from_secs(10));
    send_ctl(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid: std::process::id(),
            // SAFETY: getuid is trivially safe.
            uid: unsafe { libc::getuid() },
            build_commit: cfg.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        Some(fd.as_raw_fd()),
    )
    .expect("send HELLO over path socket");
    let (reply, memfd) = recv_ctl(&sock).expect("recv HELLO reply");
    match reply {
        CtlMsg::SessionOk { .. } => {}
        other => panic!("expected SessionOk over the path socket, got {other:?}"),
    }
    assert!(memfd.is_some(), "SessionOk must carry the memfd");
    match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { .. } => {}
        other => panic!("expected BindOk over the path socket, got {other:?}"),
    }

    drop(sock);
    host.shutdown();
    assert!(
        !std::path::Path::new(&blob.socket_path).exists(),
        "shutdown must unlink the path socket (zero residue restored)"
    );
}

#[test]
fn unbindable_socket_dir_degrades_to_abstract_only_never_fails_spawn() {
    // The path socket is optional plumbing: a dir that cannot be
    // created/bound degrades LOUDLY to abstract-only — the mount (and
    // same-netns interception) must never be held hostage by it.
    let cfg = test_config_with_dir(
        "path-degrade",
        std::path::Path::new("/proc/does-not-exist/never"),
    );
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink))
        .expect("spawn must survive a failed path bind");
    let blob = squeezefs_ipc::wire::BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    assert!(
        blob.socket_path.is_empty(),
        "no path advertised when the bind failed"
    );
    assert_eq!(blob.socket, cfg.socket_name, "abstract still serves");
    // Abstract establish still works end to end.
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let (_sock, _sess) = establish(&cfg, &host, fd.as_raw_fd());
    host.shutdown();
}

#[test]
fn no_socket_dir_means_no_path_socket_and_empty_blob_field() {
    let (host, cfg) = spawn_host("path-none");
    let blob = squeezefs_ipc::wire::BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    assert!(blob.socket_path.is_empty(), "v1-shaped blob when disabled");
    assert_eq!(blob.socket, cfg.socket_name);
    host.shutdown();
}

#[test]
fn stale_socket_file_is_replaced_on_spawn() {
    // Daemon names embed pid+random so a same-name file is OUR stale
    // corpse (crash residue), never a live foreign daemon: bind must
    // unlink-then-bind rather than fail EADDRINUSE.
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = test_config_with_dir("path-stale", dir.path());
    let stale = dir.path().join(format!("{}.sock", cfg.socket_name));
    // A dead socket file with the same name (simulated crash residue).
    let l = std::os::unix::net::UnixListener::bind(&stale).expect("stale bind");
    drop(l);
    assert!(stale.exists(), "stale socket file present before spawn");
    let host = IpcHost::spawn(cfg, Arc::new(EchoSessionSink))
        .expect("spawn must replace the stale socket file");
    let blob = squeezefs_ipc::wire::BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    assert!(
        !blob.socket_path.is_empty(),
        "path socket live after replace"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// killpriv-v2 il parity (the 2026-07-28 campaign): the session's peer
// privilege class rides every write binding
// ---------------------------------------------------------------------------

/// The CapEff word parser the peer class rides on: CAP_FSETID is bit 4
/// (linux/capability.h). A malformed word must classify as
/// NOT-privileged (the conservative direction: clearing where the
/// kernel might not is safe; preserving where the kernel would clear is
/// the security hole).
#[test]
fn capeff_fsetid_bit_parsing_is_exact() {
    use squeezefs::ipc_host::capeff_hex_has_fsetid;
    assert!(
        capeff_hex_has_fsetid("000001ffffffffff"),
        "full root cap set carries CAP_FSETID"
    );
    assert!(capeff_hex_has_fsetid(" 0000000000000010 "), "bit 4 exactly");
    assert!(
        !capeff_hex_has_fsetid("0000000000000008"),
        "bit 3 (CAP_FOWNER) is not CAP_FSETID"
    );
    assert!(!capeff_hex_has_fsetid("0000000000000000"), "empty cap set");
    assert!(
        !capeff_hex_has_fsetid("not-hex"),
        "malformed CapEff classifies conservative (kill applies)"
    );
}

/// The class computation the HELLO path uses: uid 0 is exempt; a
/// non-root peer is exempt only when its /proc CapEff carries
/// CAP_FSETID; an unreadable /proc (peer died) classifies kill.
#[test]
fn peer_kill_priv_classifies_self_consistently() {
    use squeezefs::ipc_host::peer_kill_priv;
    // SAFETY: plain getuid.
    let uid = unsafe { libc::getuid() };
    let me = peer_kill_priv(uid, std::process::id());
    if uid == 0 {
        assert!(!me, "root sessions are CAP_FSETID-exempt");
    } else {
        // An unprivileged test runner has no CAP_FSETID: kill applies.
        // (A capability-endowed runner legitimately flips this — the
        // assertion recomputes from the same /proc truth.)
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", std::process::id())).unwrap();
        let capeff = status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))
            .expect("CapEff line");
        assert_eq!(
            me,
            !squeezefs::ipc_host::capeff_hex_has_fsetid(capeff),
            "peer_kill_priv must be exactly the inverse of the peer's CAP_FSETID"
        );
    }
    // uid 0 is exempt whatever /proc says.
    assert!(!peer_kill_priv(0, std::process::id()));
    // A dead pid (unreadable /proc) classifies kill for non-root.
    assert!(peer_kill_priv(12345, u32::MAX - 1));
}

/// End-to-end: every WRITE binding carries the session peer's kill
/// class (`BindingRights::kill_priv`), computed at HELLO from the
/// SO_PEERCRED-verified identity — the ipc write path's stand-in for
/// the kernel's per-write `!capable(CAP_FSETID)` check (intercepted
/// write(2) bypasses the VFS, so `file_remove_privs` never runs there).
#[test]
fn write_binding_carries_the_session_peers_kill_priv_class() {
    use squeezefs::ipc_host::{DataOp, SessionSink, SlotCompletion};

    struct KillPrivRecordingSink {
        seen: std::sync::Mutex<Option<bool>>,
    }
    impl SessionSink for KillPrivRecordingSink {
        fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
            *self.seen.lock().expect("recording mutex") = Some(op.binding.kill_priv);
            completion.complete(i64::from(op.desc.len));
        }
    }

    let sink = Arc::new(KillPrivRecordingSink {
        seen: std::sync::Mutex::new(None),
    });
    let cfg = test_config("killpriv-class");
    let host = IpcHost::spawn(cfg.clone(), sink.clone()).expect("host must spawn");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let (sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    let binding = match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind must succeed, got {other:?}"),
    };

    let r = session.submit_wait(&SlotDescriptor {
        op: OP_WRITE,
        flags: 0,
        binding,
        offset: 0,
        len: 16,
        arena_off: 0,
    });
    assert_eq!(r, 16, "recording sink completes the write");

    // SAFETY: plain getuid.
    let expected =
        squeezefs::ipc_host::peer_kill_priv(unsafe { libc::getuid() }, std::process::id());
    assert_eq!(
        sink.seen.lock().expect("recording mutex").take(),
        Some(expected),
        "BindingRights::kill_priv must be the session peer's class \
         (uid + CAP_FSETID at HELLO)"
    );
    host.shutdown();
}

// ---------------------------------------------------------------------------
// VAL-5 (pre-RC engineering spec §3, P0) — control-plane resource bounds
//
// Every row below speaks the raw protocol from an untrusted client, which
// is the whole point: this socket is reachable by any process in the
// namespace, and none of these bounds existed. The five items:
//
//   a. `recv_ctl` derived one fd per SCM_RIGHTS cmsg, never consulting
//      `cmsg_len`, and never tested MSG_CTRUNC — extras were installed in
//      the daemon and never closed, BEFORE any validation ran.
//   b. `connection_loop` blocked on its first datagram with no
//      SO_RCVTIMEO, was in no registry, and its thread was joined
//      unconditionally by `shutdown()`.
//   c. `try_begin_serve` trusted the client-writable state word: no
//      in-flight counter anywhere, no admission gate in the serve path.
//   d. `accept4` spawned an unbounded OS thread per connection before any
//      validation, and `self.threads` was push-only.
//   e. `IpcSession::drain` popped without a per-pass budget: one busy
//      session starved every sibling pinned to its service thread.
// ---------------------------------------------------------------------------

/// How many descriptors in THIS process currently point at `path` — the
/// leak instrument for the fd-passing rows (the host runs in the test
/// process, so an fd it installs and forgets is visible right here).
fn fds_pointing_at(path: &std::path::Path) -> usize {
    let target = std::fs::canonicalize(path).expect("canonicalize marker");
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .filter(|e| {
            e.as_ref()
                .ok()
                .and_then(|e| std::fs::read_link(e.path()).ok())
                .map(|l| l == target)
                .unwrap_or(false)
        })
        .count()
}

/// Send one ctl datagram with `fds` attached in a SINGLE `SCM_RIGHTS`
/// control message (the hostile shape: `cmsg_len` says N, the pre-fix
/// daemon read exactly one and installed the rest silently).
fn send_ctl_many_fds(sock: &UnixStream, msg: &CtlMsg, fds: &[RawFd]) {
    let bytes = msg.encode();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let payload = std::mem::size_of_val(fds);
    // SAFETY: CMSG_SPACE with a runtime length; the buffer is sized from it.
    let space = unsafe { libc::CMSG_SPACE(payload as u32) } as usize;
    let mut cmsg_buf = vec![0u8; space];
    // SAFETY: zeroed msghdr is a valid all-default value.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = space as libc::size_t;
    // SAFETY: standard CMSG_FIRSTHDR/CMSG_DATA over the buffer sized above.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&hdr);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(payload as u32) as libc::size_t;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), payload);
    }
    // SAFETY: sendmsg with the msghdr assembled above.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
    assert!(n > 0, "sendmsg: {}", std::io::Error::last_os_error());
}

/// Wait for the daemon's ctl thread to finish with a connection (it drops
/// the socket, which EOFs our end) — the settle point for the fd-leak
/// assertions.
fn wait_for_peer_close(sock: &UnixStream) {
    set_recv_timeout(sock, Duration::from_secs(5));
    // Either an explicit refusal or EOF; both mean the daemon is done.
    let _ = recv_ctl(sock);
}

/// VAL-5a: a HELLO carrying MANY fds in ONE `SCM_RIGHTS` cmsg must be
/// refused, and the daemon must install exactly zero of them. Pre-fix the
/// loop copied `sizeof(int)` bytes per cmsg and ignored `cmsg_len`, so the
/// other N−1 descriptors landed in the daemon's fd table with no owner
/// and no close — an unauthenticated remote fd-table exhaustion.
#[test]
fn hello_with_many_fds_in_one_cmsg_refuses_and_installs_none() {
    let (host, cfg) = spawn_host("cmsg-many");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);

    // 8 dups of one marker file: `fds_pointing_at` then counts exactly
    // the descriptors this row is responsible for.
    let fds: Vec<OwnedFd> = (0..8).map(|_| open_flags(&mf.path, libc::O_RDWR)).collect();
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let baseline = fds_pointing_at(&mf.path);

    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    send_ctl_many_fds(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid: std::process::id(),
            // SAFETY: getuid is trivially safe.
            uid: unsafe { libc::getuid() },
            build_commit: cfg.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        &raw,
    );
    wait_for_peer_close(&sock);

    // The datagram is refused: no session may exist off a descriptor set
    // the daemon could not account for.
    wait_sessions_active(0, "multi-fd HELLO must not establish a session");
    let after = fds_pointing_at(&mf.path);
    assert_eq!(
        after,
        baseline,
        "VAL-5a: the daemon installed {} descriptors it will never close \
         (one SCM_RIGHTS cmsg carried 8 fds; cmsg_len was ignored)",
        after - baseline
    );
    host.shutdown();
}

/// VAL-5a: the truncation arm. The 64-byte control buffer holds ~12 fds;
/// a client that attaches 24 gets `MSG_CTRUNC` and a partial install —
/// which the pre-fix code never tested for. Truncated control data means
/// the daemon cannot know what it received: refuse and close everything.
#[test]
fn hello_with_truncated_control_data_refuses_and_installs_none() {
    let (host, cfg) = spawn_host("cmsg-trunc");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);

    let fds: Vec<OwnedFd> = (0..24)
        .map(|_| open_flags(&mf.path, libc::O_RDWR))
        .collect();
    let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let baseline = fds_pointing_at(&mf.path);

    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    send_ctl_many_fds(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid: std::process::id(),
            // SAFETY: getuid is trivially safe.
            uid: unsafe { libc::getuid() },
            build_commit: cfg.build_commit.clone(),
            nonce: host.current_nonce(),
        },
        &raw,
    );
    wait_for_peer_close(&sock);

    wait_sessions_active(0, "MSG_CTRUNC HELLO must not establish a session");
    let after = fds_pointing_at(&mf.path);
    assert_eq!(
        after,
        baseline,
        "VAL-5a: {} truncated-cmsg descriptors stayed installed in the daemon",
        after.saturating_sub(baseline)
    );
    host.shutdown();
}

/// VAL-5a control: the honest single-fd HELLO is untouched by the bound
/// (a fix that refuses the legitimate shape is not a fix).
#[test]
fn hello_with_exactly_one_fd_still_establishes() {
    let (host, cfg) = spawn_host("cmsg-one");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let baseline = fds_pointing_at(&mf.path);
    let (_sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    drop(session);
    // The daemon's dup of the credential fd closes right after the screen
    // (§5.2: it needs the metadata, never a live handle).
    let deadline = Instant::now() + Duration::from_secs(5);
    while fds_pointing_at(&mf.path) != baseline {
        assert!(
            Instant::now() < deadline,
            "the accepted credential fd was never closed by the daemon"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    host.shutdown();
}

/// Block until `host` has accepted `n` ctl connections (the accept is
/// asynchronous; every row that reasons about ctl threads needs this
/// settle point rather than a sleep).
fn wait_ctl_conns(host: &IpcHost, n: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.ctl_conns_live() < n {
        assert!(
            Instant::now() < deadline,
            "{what}: ctl_conns_live never reached {n} (now {})",
            host.ctl_conns_live()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// VAL-5b: a connection that sends NOTHING must not wedge daemon
/// teardown. `connection_loop` blocked in its first `recv` with no
/// SO_RCVTIMEO, was registered nowhere, and its `JoinHandle` was joined
/// unconditionally by `shutdown()` — so any process in the namespace
/// could hold a mount's umount hostage with one `connect(2)` and silence.
#[test]
fn a_silent_connection_cannot_wedge_host_shutdown() {
    let (host, cfg) = spawn_host("silent-conn");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);

    // The hostile client: connect, then say nothing, ever.
    let silent = abstract_connect(&cfg.socket_name).expect("connect");
    wait_ctl_conns(&host, 1, "the silent connection must be accepted");

    let shutdown_host = Arc::clone(&host);
    let (tx, rx) = std::sync::mpsc::channel();
    let joiner = std::thread::spawn(move || {
        shutdown_host.shutdown();
        let _ = tx.send(());
    });
    let done = rx.recv_timeout(Duration::from_secs(15)).is_ok();
    // Keep the connection open until the verdict: dropping it early
    // would EOF the ctl thread and hide the wedge.
    drop(silent);
    assert!(
        done,
        "VAL-5b: host.shutdown() wedged on a silent connection's ctl thread"
    );
    let _ = joiner.join();
}

/// VAL-5b control: an ESTABLISHED session's ctl thread parks in `recv`
/// between binds for as long as the client lives — the timeout must not
/// tear that connection down (a bound that kills idle-but-live sessions
/// would break every long-running app).
#[test]
fn an_established_but_idle_connection_survives_the_recv_timeout() {
    let (host, cfg) = spawn_host("idle-established");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let (sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    let binding = match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind must succeed, got {other:?}"),
    };

    // KEPT sleep — TEST-3 class "product time behavior under test": the
    // contract IS "an established session survives the handshake deadline
    // elapsing", so the elapsing is the stimulus, not synchronization.
    // 1.5 s is a small multiple of the handshake window.
    std::thread::sleep(Duration::from_millis(1500));
    wait_sessions_active(1, "an idle established session stays live");
    let r = session.submit_wait(&SlotDescriptor {
        op: OP_ECHO,
        flags: 0,
        binding,
        offset: 0,
        len: 0,
        arena_off: 0,
    });
    assert_eq!(r, 0, "the idle session still serves");
    match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { .. } => {}
        other => panic!("the ctl lane still serves BINDs after idling, got {other:?}"),
    }
    host.shutdown();
}

/// A sink that parks every completion handle instead of completing it —
/// the VAL-5c rows need ops that STAY in flight (a device-bound serve,
/// modelled without a device).
struct ParkingSink {
    held: std::sync::Mutex<Vec<squeezefs::ipc_host::SlotCompletion>>,
}

impl squeezefs::ipc_host::SessionSink for ParkingSink {
    fn serve_data(
        &self,
        _op: squeezefs::ipc_host::DataOp,
        completion: squeezefs::ipc_host::SlotCompletion,
    ) {
        self.held
            .lock()
            .expect("parked completion mutex")
            .push(completion);
    }
}

/// Hostile submit of `slot_index`: force the client-writable state word
/// back to SUBMITTED and publish the ring entry again. The honest
/// protocol claims a FREE slot and submits it once; this is exactly the
/// forgery VAL-5c says the daemon may not trust. Returns `false` when the
/// daemon stopped consuming (session poisoned/torn down).
fn hostile_resubmit(session: &ClientSession, slot_index: u32, d: &SlotDescriptor) -> bool {
    use squeezefs_ipc::slot_core::STATE_SUBMITTED;
    let slot = session.slot(slot_index as usize);
    slot.publish_descriptor(d);
    slot.core
        .state_futex_word()
        .store(STATE_SUBMITTED, Ordering::Release);
    if !session.ring().push(slot_index) {
        return false; // ring full: the daemon stopped draining
    }
    session.header().doorbell.fetch_add(1, Ordering::Release);
    futex_wake(&session.header().doorbell, 1);
    // Wait for the daemon to take it (SUBMITTED → SERVING).
    let deadline = Instant::now() + Duration::from_secs(5);
    while slot.core.state_futex_word().load(Ordering::Acquire) == STATE_SUBMITTED {
        if Instant::now() >= deadline {
            return false;
        }
        std::hint::spin_loop();
    }
    true
}

/// VAL-5c: `try_begin_serve` succeeds for any slot whose (client-writable)
/// state word reads SUBMITTED, and nothing anywhere counted ops in
/// flight. Honest in-flight is bounded by `slots`; a re-submitted slot is
/// not — each accepted op severs up to `max_op_bytes` and hands the sink
/// a live payload, so an unbounded re-submit loop is an unauthenticated
/// memory-growth engine (`SeveredPool` caps RETENTION, not allocation).
/// Exceeding `slots` is a protocol violation: poison the session.
#[test]
fn resubmitting_slots_past_the_in_flight_bound_poisons_the_session() {
    let sink = Arc::new(ParkingSink {
        held: std::sync::Mutex::new(Vec::new()),
    });
    let cfg = test_config("inflight-forge");
    let host = IpcHost::spawn(cfg.clone(), sink.clone()).expect("host must spawn");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let (sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    let binding = match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind must succeed, got {other:?}"),
    };
    let poisoned_before = METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed);

    let desc = SlotDescriptor {
        op: OP_READ,
        flags: 0,
        binding,
        offset: 0,
        len: 4096,
        arena_off: 0,
    };
    // `slots` re-submits of ONE slot are already `slots` ops in flight
    // (the sink never completes them); the next one exceeds what the
    // geometry can honestly hold.
    let slots = test_geometry().slots;
    for _ in 0..slots + 4 {
        if !hostile_resubmit(&session, 0, &desc) {
            break; // the daemon stopped serving us — the expected end
        }
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed) == poisoned_before {
        assert!(
            Instant::now() < deadline,
            "VAL-5c: {} forged in-flight ops accepted without a poison — \
             the daemon has no in-flight accounting",
            slots + 4
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    wait_sessions_active(0, "a poisoned session is torn down");
    host.shutdown();
}

/// VAL-5c control: `slots` HONEST concurrent ops (one per claimed slot,
/// all parked daemon-side) are exactly the protocol's bound and must
/// serve without a poison — the counter is a protocol check, not a
/// throughput cap.
#[test]
fn honest_in_flight_up_to_the_slot_count_never_poisons() {
    let sink = Arc::new(ParkingSink {
        held: std::sync::Mutex::new(Vec::new()),
    });
    let cfg = test_config("inflight-honest");
    let host = IpcHost::spawn(cfg.clone(), sink.clone()).expect("host must spawn");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd = open_flags(&mf.path, libc::O_RDWR);
    let (sock, session) = establish(&cfg, &host, fd.as_raw_fd());
    let binding = match bind(&sock, fd.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind must succeed, got {other:?}"),
    };
    let poisoned_before = METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed);

    let slots = test_geometry().slots;
    for i in 0..slots {
        let slot = session.slot(i as usize);
        slot.core.try_claim().expect("every slot starts FREE");
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding,
            offset: 0,
            len: 4096,
            arena_off: u64::from(i) * 4096,
        });
        slot.core.publish_submitted();
        assert!(session.ring().push(i), "ring holds one entry per slot");
        session.header().doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&session.header().doorbell, 1);
    }

    // All `slots` ops must reach the sink, and the session must live.
    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.held.lock().expect("parked completions").len() < slots as usize {
        assert!(
            Instant::now() < deadline,
            "only {} of {slots} honest ops were served",
            sink.held.lock().expect("parked completions").len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        METRICS.ipc_sessions_poisoned.load(Ordering::Relaxed),
        poisoned_before,
        "VAL-5c: the in-flight bound poisoned an HONEST client at exactly `slots`"
    );
    wait_sessions_active(1, "the honest session stays live");

    // Completing them releases the ledger: the same session can serve
    // another full window afterwards.
    sink.held.lock().expect("parked completions").clear();
    host.shutdown();
}

/// VAL-5d: `accept4` spawned an unbounded OS thread per connection —
/// before ANY validation — and `self.threads` was push-only: one
/// `JoinHandle` retained per connection ever accepted. Any process in the
/// namespace could therefore turn `connect(2)` into daemon thread (and
/// memory) exhaustion, with the removal pattern sitting 160 lines away
/// (`admin_conns` already used `retain`).
#[test]
fn a_connect_storm_is_capped_at_the_derived_ctl_conn_bound() {
    let (host, cfg) = spawn_host("conn-storm");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let cap = host.ctl_conn_cap();
    assert!(cap >= 32, "the floor keeps the ADMIN lane usable: {cap}");

    // Silent connections, well past the bound: none of them validate,
    // none of them will ever be a session.
    let storm: Vec<UnixStream> = (0..cap + 16)
        .filter_map(|_| abstract_connect(&cfg.socket_name).ok())
        .collect();
    assert!(storm.len() > cap, "the test must actually exceed the cap");

    // The bound holds while the storm is connected.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let live = host.ctl_conns_live();
        assert!(
            live <= cap,
            "VAL-5d: {live} concurrent ctl threads past the derived cap of {cap}"
        );
        if live >= cap || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // And the over-cap connections are refused promptly (loud, not
    // silently parked): the last one must hear something back.
    let last = storm.last().expect("storm is non-empty");
    set_recv_timeout(last, Duration::from_secs(5));
    match recv_ctl(last) {
        Ok((CtlMsg::Refuse { class }, _)) => {
            assert_eq!(class, RefuseClass::Budget, "over-cap refusal class")
        }
        Ok((other, _)) => panic!("expected a Budget refusal, got {other:?}"),
        Err(e) => assert!(
            e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.kind() == std::io::ErrorKind::ConnectionReset,
            "over-cap connections are refused or closed, got {e:?}"
        ),
    }
    drop(storm);
    host.shutdown();
}

/// VAL-5d: finished ctl threads must not accumulate `JoinHandle`s. Each
/// accept prunes the finished ones (`retain(|h| !h.is_finished())`), so a
/// long-lived mount serving thousands of short client sessions keeps a
/// handle vector sized by LIVE threads, not by history.
#[test]
fn finished_ctl_threads_do_not_accumulate_join_handles() {
    let (host, cfg) = spawn_host("handle-reap");
    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let baseline = host.retained_thread_handles();

    for _ in 0..150 {
        let sock = abstract_connect(&cfg.socket_name).expect("connect");
        drop(sock); // immediate EOF: the ctl thread exits at once
    }
    // Give the last accepts a moment to run their prune pass.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut retained = host.retained_thread_handles();
    while retained > baseline + 32 && Instant::now() < deadline {
        // One more accept drives the prune (it happens per accept).
        drop(abstract_connect(&cfg.socket_name).expect("connect"));
        std::thread::sleep(Duration::from_millis(10));
        retained = host.retained_thread_handles();
    }
    assert!(
        retained <= baseline + 32,
        "VAL-5d: {retained} JoinHandles retained after 150 short connections \
         (baseline {baseline}) — the registry is push-only"
    );
    host.shutdown();
}

/// A sink whose serve BLOCKS the service thread for a fixed span before
/// completing — a device-bound serve, modelled without a device. The
/// VAL-5e rows need the drain loop to be the scarce resource.
struct SlowSink {
    per_op: Duration,
}

impl squeezefs::ipc_host::SessionSink for SlowSink {
    fn serve_data(
        &self,
        op: squeezefs::ipc_host::DataOp,
        completion: squeezefs::ipc_host::SlotCompletion,
    ) {
        std::thread::sleep(self.per_op);
        completion.complete(i64::from(op.desc.len));
    }
}

/// Keep `session`'s ring topped up: release every finished slot and
/// re-submit it. One call tops up every free slot; called in a loop it is
/// a client that never lets its ring go empty.
fn top_up(session: &ClientSession, gens: &mut [Option<u64>], desc: &SlotDescriptor) -> usize {
    let mut recycled = 0usize;
    for (i, gen) in gens.iter_mut().enumerate() {
        let slot = session.slot(i);
        if let Some(g) = *gen {
            if slot.core.is_done_for(g) {
                slot.core.release();
                *gen = None;
                recycled += 1;
            } else {
                continue;
            }
        }
        let Some(g) = slot.core.try_claim() else {
            continue;
        };
        slot.publish_descriptor(desc);
        slot.core.publish_submitted();
        if session.ring().push(i as u32) {
            *gen = Some(g);
            session.header().doorbell.fetch_add(1, Ordering::Release);
            futex_wake(&session.header().doorbell, 1);
        } else {
            slot.core.release_claimed();
        }
    }
    recycled
}

/// VAL-5e: `IpcSession::drain` was `while let Some(i) = consumer.pop(...)`
/// with no per-pass budget. Sessions are PINNED to one service thread
/// (§5.5.1) and `service_loop` walks them serially, so a client that
/// keeps its ring non-empty owns that thread forever: every sibling
/// session pinned to it is starved — one unprivileged process starving
/// another process's I/O — and, because the loop never returns to the
/// pass top, a NEWLY admitted session is never even collected.
#[test]
fn a_saturating_session_cannot_starve_its_service_thread_siblings() {
    // One service thread ⇒ both sessions are pinned to it (the shape the
    // §5.5.1 ownership invariant makes ordinary at scale: sessions
    // outnumber threads).
    std::env::set_var("SQUEEZEFS_IPC_SERVICE_THREADS", "1");
    let cfg = test_config("drain-fairness");
    let host = IpcHost::spawn(
        cfg.clone(),
        Arc::new(SlowSink {
            per_op: Duration::from_micros(200),
        }),
    )
    .expect("host must spawn");
    std::env::remove_var("SQUEEZEFS_IPC_SERVICE_THREADS");

    let mf = mount_file();
    host.set_expected_st_dev(mf.st_dev);
    let fd_a = open_flags(&mf.path, libc::O_RDWR);
    let fd_b = open_flags(&mf.path, libc::O_RDWR);
    let (sock_a, sess_a) = establish(&cfg, &host, fd_a.as_raw_fd());
    let bind_a = match bind(&sock_a, fd_a.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind A: {other:?}"),
    };

    // A saturates from its OWN thread: the ring must stay non-empty
    // across everything the main thread does below (B's whole ctl
    // handshake included) or the starvation shape evaporates.
    let slots = test_geometry().slots as usize;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let served_a = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saturator = {
        let stop = Arc::clone(&stop);
        let served_a = Arc::clone(&served_a);
        std::thread::spawn(move || {
            let desc = SlotDescriptor {
                op: OP_READ,
                flags: 0,
                binding: bind_a,
                offset: 0,
                len: 4096,
                arena_off: 0,
            };
            let mut gens: Vec<Option<u64>> = vec![None; slots];
            while !stop.load(Ordering::Relaxed) {
                let n = top_up(&sess_a, &mut gens, &desc);
                served_a.fetch_add(n, Ordering::Relaxed);
            }
            sess_a // hand it back so the mapping outlives the daemon's use
        })
    };

    // Wait until A is genuinely saturating (its ring has cycled).
    let warm = Instant::now() + Duration::from_secs(10);
    while served_a.load(Ordering::Relaxed) < slots * 2 {
        assert!(
            Instant::now() < warm,
            "A never reached saturation ({} ops)",
            served_a.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // NOW B joins — mid-drain — and submits ONE op.
    let (sock_b, sess_b) = establish(&cfg, &host, fd_b.as_raw_fd());
    let bind_b = match bind(&sock_b, fd_b.as_raw_fd()) {
        CtlMsg::BindOk { binding_id, .. } => binding_id,
        other => panic!("bind B: {other:?}"),
    };
    let slot_b = sess_b.slot(0);
    let gen_b = slot_b.core.try_claim().expect("B slot 0 is FREE");
    slot_b.publish_descriptor(&SlotDescriptor {
        op: OP_READ,
        flags: 0,
        binding: bind_b,
        offset: 0,
        len: 4096,
        arena_off: 0,
    });
    slot_b.core.publish_submitted();
    assert!(sess_b.ring().push(0), "B's ring accepts");
    sess_b.header().doorbell.fetch_add(1, Ordering::Release);
    futex_wake(&sess_b.header().doorbell, 1);

    // A fair drain serves B within one budget window (`slots` A-ops);
    // an unbudgeted one never does.
    let deadline = Instant::now() + Duration::from_secs(10);
    let starved = loop {
        if slot_b.core.is_done_for(gen_b) {
            break false;
        }
        if Instant::now() >= deadline {
            break true;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    stop.store(true, Ordering::Relaxed);
    let _sess_a = saturator.join().expect("saturator thread");
    assert!(
        !starved,
        "VAL-5e: a saturating session starved its sibling for 10 s ({} A-ops \
         served meanwhile) — the drain has no per-pass budget",
        served_a.load(Ordering::Relaxed)
    );
    assert_eq!(slot_b.result(), 4096, "B's op is served, not just noticed");
    host.shutdown();
}
