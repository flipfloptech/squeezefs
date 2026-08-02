//! VAL-4 (pre-RC engineering spec §3, P0) — **the shim must authenticate
//! the daemon it connects to**, red-first.
//!
//! The bootstrap blob — including the socket name it dictates — is read
//! from the MOUNT (`fgetxattr(fd, "user.squeezefs.il0")`), so any
//! filesystem that can answer that call determines the rendezvous. Before
//! this suite, `grep -n 'SO_PEERCRED\|F_GET_SEALS' crates/squeezefs-preload/src`
//! returned nothing: the shim connected wherever the blob pointed, sent
//! the caller's open fd over `SCM_RIGHTS`, mapped whatever memfd came
//! back, and trusted it for the session's life.
//!
//! The ladder pinned here runs **before the credential fd leaves the
//! process** (steps 1–2) and **before the mapping is trusted** (step 3):
//!
//! 1. the path rung must resolve under a directory owned by an identity
//!    the peer ladder would accept (root or the mount owner) and that is
//!    not group/world-writable — a squatted rendezvous never gets a
//!    `connect(2)`;
//! 2. `getsockopt(SO_PEERCRED)`: the daemon's uid must be `0` or the
//!    mount root's `st_uid` (the value `interpose.rs` already `fstat`ed
//!    on the credential fd — never re-stat'ed here);
//! 3. `fcntl(memfd, F_GET_SEALS)`: `F_SEAL_SEAL | F_SEAL_SHRINK |
//!    F_SEAL_GROW` must ALL be present, or the "session" is a mapping the
//!    other end can still shrink under us.
//!
//! **Fallback-is-correctness (§5.4.2)**: every refusal here degrades to
//! kernel FUSE. The app must never see an error it would not have seen
//! without the shim — the legs assert the refusal AND that the credential
//! fd stayed home.
//!
//! The adversary is modelled by [`RogueDaemon`]: a raw-protocol server
//! that binds the abstract name a (hostile) blob advertises and records
//! everything it receives. It is deliberately NOT an `IpcHost` — the
//! point is that anything can answer this protocol.

use squeezefs::ipc_host::{recv_ctl, send_ctl, EchoSessionSink, IpcHost, IpcHostConfig};
use squeezefs_il::session::{Session, SessionError};
use squeezefs_ipc::layout::{Geometry, SessionHeader, SessionLayout};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::{BootstrapBlob, CtlMsg};

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEST_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 1024 * 1024,
        max_op_bytes: 64 * 1024,
        _pad: 0,
    }
}

fn my_uid() -> u32 {
    // SAFETY: getuid is trivially safe.
    unsafe { libc::getuid() }
}

/// A uid that is neither root nor ours — the "the daemon answering this
/// mount is not who the mount says owns it" shape, reachable without
/// root (the predicate is symmetric: a foreign EXPECTED uid exercises
/// exactly the branch a foreign PEER uid would).
fn foreign_uid() -> u32 {
    my_uid().wrapping_add(4242) | 1
}

// ---------------------------------------------------------------------------
// the adversary: a raw-protocol "daemon" on the name a blob advertises
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RogueLog {
    /// Ctl datagrams received (a shim that authenticates first sends 0).
    datagrams: u32,
    /// Credential fds received over `SCM_RIGHTS` — the thing that must
    /// never leave the process when the peer is untrusted.
    cred_fds: u32,
}

struct RogueDaemon {
    listener: OwnedFd,
    log: Arc<std::sync::Mutex<RogueLog>>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Abstract name (empty when the rogue listens on a path).
    socket: String,
    /// Filesystem path (empty when abstract).
    socket_path: String,
}

impl RogueDaemon {
    /// Spawn a rogue on an abstract name. `seal` selects whether the
    /// session memfd it hands back carries the required seals.
    fn abstract_rogue(name: &str, seal: bool) -> RogueDaemon {
        let listener = abstract_listen(name);
        Self::serve(listener, seal, name.to_string(), String::new())
    }

    /// Spawn a rogue on a filesystem-path socket (the OQ-6 rung).
    fn path_rogue(path: &std::path::Path, seal: bool) -> RogueDaemon {
        let listener = path_listen(path);
        Self::serve(
            listener,
            seal,
            String::new(),
            path.to_string_lossy().into_owned(),
        )
    }

    fn serve(listener: OwnedFd, seal: bool, socket: String, socket_path: String) -> RogueDaemon {
        let log = Arc::new(std::sync::Mutex::new(RogueLog::default()));
        let thread_log = Arc::clone(&log);
        // SAFETY: dup of our own listener for the serving thread.
        let lfd = unsafe { libc::dup(listener.as_raw_fd()) };
        assert!(lfd >= 0, "dup listener");
        let join = std::thread::spawn(move || {
            // SAFETY: fresh owned fd from dup.
            let listener = unsafe { OwnedFd::from_raw_fd(lfd) };
            loop {
                // SAFETY: accept4 on our own listener.
                let fd = unsafe {
                    libc::accept4(
                        listener.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return; // listener shut down: the rogue is done
                }
                // SAFETY: fresh owned fd from accept4.
                let sock = unsafe { UnixStream::from_raw_fd(fd) };
                let Ok((msg, rx)) = recv_ctl(&sock) else {
                    continue;
                };
                {
                    let mut l = thread_log.lock().expect("rogue log");
                    l.datagrams += 1;
                    if rx.is_some() {
                        l.cred_fds += 1;
                    }
                }
                if !matches!(msg, CtlMsg::Hello { .. }) {
                    continue;
                }
                let geometry = test_geometry();
                let memfd = rogue_session_memfd(&geometry, seal);
                let _ = send_ctl(
                    &sock,
                    &CtlMsg::SessionOk { geometry },
                    Some(memfd.as_raw_fd()),
                );
                // Keep the connection alive for the client's lifetime
                // check; the listener shutdown ends this thread.
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        RogueDaemon {
            listener,
            log,
            join: Some(join),
            socket,
            socket_path,
        }
    }

    /// The hostile bootstrap blob: what a filesystem answering the
    /// `user.squeezefs.il0` `fgetxattr` would hand the shim.
    fn blob(&self) -> BootstrapBlob {
        BootstrapBlob {
            abi: squeezefs_ipc::layout::IPC_ABI,
            flags: 0,
            build_commit: TEST_COMMIT.to_string(),
            socket: if self.socket.is_empty() {
                format!("sqz-il0-nowhere-{}", std::process::id())
            } else {
                self.socket.clone()
            },
            socket_path: self.socket_path.clone(),
            nonce: [0u8; squeezefs_ipc::wire::NONCE_LEN],
        }
    }

    fn log(&self) -> (u32, u32) {
        let l = self.log.lock().expect("rogue log");
        (l.datagrams, l.cred_fds)
    }
}

impl Drop for RogueDaemon {
    fn drop(&mut self) {
        // SAFETY: shutdown(2) on our own listener — unblocks the accept.
        unsafe { libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if !self.socket_path.is_empty() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

fn abstract_listen(name: &str) -> OwnedFd {
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(fd >= 0, "socket");
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, b) in name.as_bytes().iter().enumerate() {
        addr.sun_path[i + 1] = *b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len();
    // SAFETY: bind with a correctly-sized sockaddr_un.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    };
    assert_eq!(r, 0, "bind abstract {name}");
    // SAFETY: listen(2).
    assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 8) }, 0, "listen");
    fd
}

fn path_listen(path: &std::path::Path) -> OwnedFd {
    use std::os::unix::ffi::OsStrExt;
    let _ = std::fs::remove_file(path);
    // SAFETY: socket(2); ownership taken immediately.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(fd >= 0, "socket");
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: zeroed sockaddr_un is a valid all-default value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len(), "path fits sun_path");
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    // SAFETY: bind with a correctly-sized sockaddr_un.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    };
    assert_eq!(r, 0, "bind path {}", path.display());
    // SAFETY: listen(2).
    assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 8) }, 0, "listen");
    fd
}

/// A session memfd shaped exactly like the daemon's — optionally WITHOUT
/// the seals (the whole point of step 3: an unsealed mapping can be
/// shrunk under the client, turning every later slot access into SIGBUS).
fn rogue_session_memfd(geometry: &Geometry, seal: bool) -> OwnedFd {
    let layout = SessionLayout::compute(geometry).expect("layout");
    // SAFETY: memfd_create with a static name; ownership taken immediately.
    let fd = unsafe {
        libc::memfd_create(
            c"sqz-rogue-session".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(fd >= 0, "memfd_create");
    // SAFETY: fresh owned fd.
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate on our own memfd.
    assert_eq!(
        unsafe { libc::ftruncate(memfd.as_raw_fd(), layout.total_bytes as libc::off_t) },
        0,
        "ftruncate"
    );
    // SAFETY: shared RW mapping of the full memfd.
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
    assert!(base != libc::MAP_FAILED, "mmap");
    // SAFETY: header page at offset 0 of a mapping sized by the layout.
    unsafe { std::ptr::write(base as *mut SessionHeader, SessionHeader::new(*geometry)) };
    // SAFETY: ring region offsets come from the layout that sized the map.
    unsafe {
        let tail = &*((base as *mut u8).add(layout.ring_off as usize) as *const AtomicU32);
        let cells = std::slice::from_raw_parts(
            (base as *mut u8).add(layout.ring_cells_off as usize) as *const RingCell,
            geometry.ring_entries as usize,
        );
        MpscRingView::from_parts(tail, cells)
            .expect("ring view")
            .seed_for_sharing();
    }
    // SAFETY: unmapping our own mapping (the memfd keeps the pages).
    unsafe { libc::munmap(base, layout.total_bytes as usize) };
    if seal {
        // SAFETY: F_ADD_SEALS on our own memfd.
        let r = unsafe {
            libc::fcntl(
                memfd.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        };
        assert_eq!(r, 0, "seal");
    }
    memfd
}

/// A regular file the shim can present as its credential (any fd on the
/// mount); returns `(dir, fd)`.
fn cred_file(tag: &str) -> (tempfile::TempDir, std::fs::File) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}.cred"));
    std::fs::write(&path, b"x").expect("cred file");
    let f = std::fs::File::open(&path).expect("open cred");
    (dir, f)
}

// ---------------------------------------------------------------------------
// step 2 — SO_PEERCRED before the credential fd is sent
// ---------------------------------------------------------------------------

#[test]
fn shim_refuses_a_daemon_whose_uid_is_neither_root_nor_the_mount_owner() {
    let name = format!("sqz-il0-rogue-peer-{}", std::process::id());
    let rogue = RogueDaemon::abstract_rogue(&name, true);
    let (_dir, f) = cred_file("peer");

    // The mount says its root is owned by uid X; the process answering
    // the socket the mount named is neither root nor X.
    let err = match Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, foreign_uid()) {
        Ok(_) => panic!(
            "VAL-4: the shim established a session with a daemon whose \
             SO_PEERCRED uid is neither 0 nor the mount owner"
        ),
        Err(e) => e,
    };
    assert!(
        matches!(err, SessionError::PeerUntrusted { .. }),
        "the refusal must name the peer-credential cause, got {err:?}"
    );

    // Fallback-is-correctness: nothing left the process. No HELLO, and
    // above all no `SCM_RIGHTS` credential fd.
    let (datagrams, cred_fds) = rogue.log();
    assert_eq!(
        datagrams, 0,
        "the ladder must refuse BEFORE the HELLO datagram"
    );
    assert_eq!(
        cred_fds, 0,
        "VAL-4: the caller's open fd was handed to an untrusted peer"
    );

    // And the reason line names the cause (the operator-facing half).
    let line = err.describe(squeezefs_ipc::layout::IPC_ABI, TEST_COMMIT, TEST_COMMIT);
    assert!(
        line.contains("peer") || line.contains("uid"),
        "refusal line names the peer identity: {line}"
    );
}

#[test]
fn shim_accepts_the_real_daemon_when_the_mount_owner_matches() {
    // The control leg: the SAME ladder must not refuse the honest
    // rendezvous (a security check that breaks interception is a
    // regression, not a fix).
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-authn-ok-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 16 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        owner_uid: my_uid(),
    };
    let host = IpcHost::spawn(cfg, Arc::new(EchoSessionSink)).expect("host must spawn");
    let (dir, f) = cred_file("ok");
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).expect("metadata").dev()
    };
    host.set_expected_st_dev(st_dev);
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    let session = Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("the honest daemon (peer uid == mount owner, sealed memfd) must establish");
    assert!(!session.poisoned());
    drop(session);
    host.shutdown();
}

// ---------------------------------------------------------------------------
// step 3 — F_GET_SEALS on the received memfd
// ---------------------------------------------------------------------------

#[test]
fn shim_refuses_an_unsealed_session_memfd() {
    let name = format!("sqz-il0-rogue-seal-{}", std::process::id());
    let rogue = RogueDaemon::abstract_rogue(&name, false);
    let (_dir, f) = cred_file("seal");

    // Peer uid == the mount owner (same process), so the ladder gets
    // past step 2 — the seals are the only thing standing between the
    // shim and a mapping the other end can shrink under it.
    let err = match Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid()) {
        Ok(_) => panic!("VAL-4: the shim mapped an UNSEALED session memfd"),
        Err(e) => e,
    };
    assert!(
        matches!(err, SessionError::Unsealed { .. }),
        "the refusal must name the seal cause, got {err:?}"
    );
    let line = err.describe(squeezefs_ipc::layout::IPC_ABI, TEST_COMMIT, TEST_COMMIT);
    assert!(line.contains("seal"), "refusal line names seals: {line}");
}

#[test]
fn shim_accepts_a_fully_sealed_session_memfd() {
    // The seal predicate is exact: SEAL|SHRINK|GROW present ⇒ accepted
    // (the rogue seals exactly what `create_session_shm` does).
    let name = format!("sqz-il0-rogue-sealed-{}", std::process::id());
    let rogue = RogueDaemon::abstract_rogue(&name, true);
    let (_dir, f) = cred_file("sealed");
    let session = Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("a properly sealed session must establish");
    drop(session);
    let (datagrams, cred_fds) = rogue.log();
    assert_eq!(datagrams, 1, "one HELLO");
    assert_eq!(cred_fds, 1, "the credential fd rides the accepted HELLO");
}

// ---------------------------------------------------------------------------
// step 1 — the path rung must resolve under a trusted directory
// ---------------------------------------------------------------------------

#[test]
fn shim_refuses_a_path_rendezvous_in_a_world_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
        .expect("world-writable rendezvous dir");
    let sock_path = dir.path().join("squatted.sock");
    let rogue = RogueDaemon::path_rogue(&sock_path, true);
    let (_cdir, f) = cred_file("path");

    // The blob's abstract rung resolves nowhere (the container-netns
    // shape), so the ladder falls to the path rung — which lives in a
    // directory any uid can replace entries in.
    let err = match Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid()) {
        Ok(_) => panic!("VAL-4: the shim connected to a squattable path rendezvous"),
        Err(e) => e,
    };
    assert!(
        matches!(err, SessionError::UntrustedRendezvous),
        "the refusal must name the rendezvous cause, got {err:?}"
    );
    let (datagrams, cred_fds) = rogue.log();
    assert_eq!(datagrams, 0, "no datagram may reach a squattable socket");
    assert_eq!(cred_fds, 0, "and certainly no credential fd");
}

#[test]
fn shim_accepts_a_path_rendezvous_under_an_owner_private_directory() {
    // The control leg: the OQ-6 rung stays usable where the daemon's
    // runtime dir is owned by the mount owner and not group/world
    // writable (tempfile makes 0700 dirs — the shipped `/run/squeezefs`
    // and `$XDG_RUNTIME_DIR/squeezefs` shapes).
    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("trusted.sock");
    let rogue = RogueDaemon::path_rogue(&sock_path, true);
    let (_cdir, f) = cred_file("path-ok");
    let session = Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("a trusted path rendezvous must still establish");
    drop(session);
    let deadline = Instant::now() + Duration::from_secs(5);
    while rogue.log().0 == 0 {
        assert!(Instant::now() < deadline, "rogue never saw the HELLO");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// daemon side — the socket directory is checked, never conjured
// ---------------------------------------------------------------------------

#[test]
fn host_refuses_to_bind_a_path_socket_in_a_group_or_world_writable_dir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
        .expect("world-writable socket dir");
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-dircheck-{}", std::process::id()),
        socket_dir: Some(dir.path().to_path_buf()),
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 16 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        owner_uid: my_uid(),
    };
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn must succeed");
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    assert!(
        blob.socket_path.is_empty(),
        "VAL-4: the host bound (and advertised) a path socket in a \
         world-writable directory: {}",
        blob.socket_path
    );
    assert!(
        !dir.path()
            .join(format!("{}.sock", cfg.socket_name))
            .exists(),
        "no socket file may be created in an untrusted directory"
    );
    // Refusal is loud but never fatal: the abstract rung still serves.
    assert!(!blob.socket.is_empty(), "abstract rendezvous survives");
    host.shutdown();
}

#[test]
fn host_binds_a_path_socket_in_an_owner_private_dir_and_serves_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-dirok-{}", std::process::id()),
        socket_dir: Some(dir.path().to_path_buf()),
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 16 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        owner_uid: my_uid(),
    };
    let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("spawn");
    let (cdir, f) = cred_file("dirok");
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(cdir.path()).expect("metadata").dev()
    };
    host.set_expected_st_dev(st_dev);
    let mut blob = BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    assert!(
        !blob.socket_path.is_empty(),
        "an owner-private 0700 dir must still yield the OQ-6 path rung"
    );
    // Force the path rung (the abstract name resolves nowhere).
    blob.socket = format!("sqz-il0-nowhere-{}", std::process::id());
    let session = Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("path rendezvous must serve a real session");
    drop(session);
    host.shutdown();
}

// ---------------------------------------------------------------------------
// the fd-leak check the ladder exists to prevent, stated as an invariant
// ---------------------------------------------------------------------------

#[test]
fn refused_rendezvous_never_leaks_descriptors() {
    // Every refusal rung must close what it opened: the ladder runs on
    // EVERY open of an eligible fd (the mount is never negative-cached),
    // so a leaked socket per refusal is a process-lifetime fd exhaustion
    // bug in the app we are supposed to be invisible to.
    let name = format!("sqz-il0-rogue-leak-{}", std::process::id());
    let rogue = RogueDaemon::abstract_rogue(&name, false);
    let (_dir, f) = cred_file("leak");
    let before = open_fd_count();
    for _ in 0..64 {
        let _ = Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, foreign_uid());
        let _ = Session::establish(&rogue.blob(), f.as_raw_fd(), TEST_COMMIT, my_uid());
    }
    let after = open_fd_count();
    assert!(
        after <= before + 4,
        "refusals leaked descriptors: {before} → {after}"
    );
}

fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}
