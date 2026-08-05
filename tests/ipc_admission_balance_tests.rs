//! Session-admission balance contracts (2026-08-05, `perf/il-530k-ceiling`
//! — the 1M-IOPS program's ~530k il rand-4k service ceiling).
//!
//! The field capture (squeeze-test pair 85b79c40, fio libaio rand-4k
//! direct, 32 PROCESSES): `ipc_direct_shards` read 4-5 of the derived 8
//! on every row — governed submits were landing on 4-5 service-thread
//! lanes while IOPS queued flat (451k→507k→531k, clat doubling per qd
//! step). Two composable mechanisms, both closed here:
//!
//! 1. **Locality confinement** (the dominant term, pinned in
//!    `tests/numa_affinity_tests.rs`): the owner pick was lexicographic
//!    `(distance, load, index)`, so a fork-clustered fleet inference
//!    (all 32 pids' HELLO-instant CPUs on one node) confined every
//!    session to that node's owner subset — 4 of 8 on the 2×16 field
//!    box. The pick is now balance-first `(load, distance, index)`.
//! 2. **The admission count-to-insert race**: the pick counted owners by
//!    scanning the session registry, but the INSERT happened later on
//!    the same ctl thread — 32 concurrent HELLOs at fleet launch could
//!    read stale counts and convoy onto low indices. The pick and its
//!    accounting are now ONE atomic act on a dedicated owner-load
//!    ledger (reserve at pick, release at teardown/refusal), so balance
//!    is structural, not schedule-dependent.
//!
//! The observable is `IpcHost::session_owner_spread()` (live sessions
//! per owner index) plus the `ipc_session_owners` gauge (owners with
//! ≥1 live session — the admission-time face of `ipc_direct_shards`).
//!
//! House posture: these run under the gate's `--test-threads=1` (the
//! env-lever + global-METRICS pattern `ipc_host_tests.rs` already uses).

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{abstract_connect, recv_ctl, send_ctl, EchoSessionSink, IpcHost, IpcHostConfig};
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::wire::{CtlMsg, NONCE_LEN};

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------
// harness (the ipc_host_tests raw-protocol speaker, HELLO-only slice)
// ---------------------------------------------------------------------

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
        socket_name: format!("sqz-il0-bal-{}-{}", std::process::id(), name),
        socket_dir: None,
        build_commit: "a".repeat(40),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 64,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    }
}

fn open_cred_fd(dir: &std::path::Path) -> OwnedFd {
    let path = dir.join("cred_file");
    if !path.exists() {
        let mut f = std::fs::File::create(&path).expect("create cred file");
        f.write_all(&[7u8; 4096]).expect("seed cred file");
    }
    let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: plain open(2); ownership taken immediately.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    assert!(fd >= 0, "open cred file: {}", std::io::Error::last_os_error());
    // SAFETY: fresh owned fd.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Full HELLO → SessionOk; returns the live socket (the session lives
/// until it drops).
fn establish(cfg: &IpcHostConfig, host: &IpcHost, fd: RawFd) -> UnixStream {
    let sock = abstract_connect(&cfg.socket_name).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("SO_RCVTIMEO");
    let nonce: [u8; NONCE_LEN] = host.current_nonce();
    send_ctl(
        &sock,
        &CtlMsg::Hello {
            abi: squeezefs_ipc::layout::IPC_ABI,
            pid: std::process::id(),
            // SAFETY: getuid is trivially safe.
            uid: unsafe { libc::getuid() },
            build_commit: cfg.build_commit.clone(),
            nonce,
        },
        Some(fd),
    )
    .expect("send HELLO");
    let (reply, memfd) = recv_ctl(&sock).expect("recv HELLO reply");
    match reply {
        CtlMsg::SessionOk { .. } => {}
        other => panic!("expected SessionOk, got {other:?}"),
    }
    drop(memfd);
    sock
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

fn with_ceiling<T>(n: usize, f: impl FnOnce() -> T) -> T {
    std::env::set_var("SQUEEZEFS_IPC_SERVICE_THREADS", n.to_string());
    let out = f();
    std::env::remove_var("SQUEEZEFS_IPC_SERVICE_THREADS");
    out
}

// ---------------------------------------------------------------------
// contracts
// ---------------------------------------------------------------------

/// Sequential fleet: 8 sessions on a ceiling of 4 must land EXACTLY
/// [2,2,2,2] — every owner engaged, spread max−min == 0. The observable
/// itself (`session_owner_spread`) is this campaign's product change.
#[test]
fn sequential_fleet_admission_balances_exactly() {
    with_ceiling(4, || {
        let cfg = test_config("seq");
        let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host");
        let dir = tempfile::tempdir().expect("tempdir");
        let fd = open_cred_fd(dir.path());
        let mut socks = Vec::new();
        for _ in 0..8 {
            socks.push(establish(&cfg, &host, fd.as_raw_fd()));
        }
        assert_eq!(
            host.session_owner_spread(),
            vec![2, 2, 2, 2],
            "8 sessions on 4 owners must spread exactly"
        );
        assert_eq!(
            METRICS.ipc_session_owners.load(Ordering::Relaxed),
            4,
            "the ipc_session_owners gauge must report every engaged owner"
        );
        drop(socks);
        host.shutdown();
    });
}

/// The fleet-launch race: 16 CONCURRENT establishes on a ceiling of 4
/// must still land [4,4,4,4]. Before the owner-load ledger, the pick
/// read registry counts that concurrent admissions had not yet
/// inserted — balance depended on the schedule. Reserve-at-pick makes
/// it structural (multi-thread flavor; runs ×10 in the campaign gate).
#[test]
fn concurrent_fleet_admission_balances_exactly() {
    with_ceiling(4, || {
        let cfg = test_config("conc");
        let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host");
        let dir = tempfile::tempdir().expect("tempdir");
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let socks: Vec<UnixStream> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let cfg = &cfg;
                    let host = &host;
                    let barrier = Arc::clone(&barrier);
                    let dir = dir.path().to_path_buf();
                    s.spawn(move || {
                        let fd = open_cred_fd(&dir);
                        barrier.wait();
                        establish(cfg, host, fd.as_raw_fd())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("establish thread"))
                .collect()
        });
        let spread = host.session_owner_spread();
        assert_eq!(spread.iter().sum::<usize>(), 16, "all 16 sessions live");
        assert_eq!(
            spread,
            vec![4, 4, 4, 4],
            "concurrent fleet launch must balance structurally, not by schedule"
        );
        drop(socks);
        host.shutdown();
    });
}

/// Teardown releases the ledger: the spread returns to all-zero and the
/// gauge to 0 — a leaked reservation would bias every later pick.
#[test]
fn teardown_releases_the_owner_ledger() {
    with_ceiling(4, || {
        let cfg = test_config("release");
        let host = IpcHost::spawn(cfg.clone(), Arc::new(EchoSessionSink)).expect("host");
        let dir = tempfile::tempdir().expect("tempdir");
        let fd = open_cred_fd(dir.path());
        let socks: Vec<UnixStream> = (0..3).map(|_| establish(&cfg, &host, fd.as_raw_fd())).collect();
        assert_eq!(host.session_owner_spread().iter().sum::<usize>(), 3);
        drop(socks); // ctl EOF ⇒ teardown_session per session
        wait_sessions_active(0, "socket-drop teardown");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let spread = host.session_owner_spread();
            if spread.iter().all(|&n| n == 0) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owner ledger never drained: {spread:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            METRICS.ipc_session_owners.load(Ordering::Relaxed),
            0,
            "gauge must return to 0 with the ledger"
        );
        host.shutdown();
    });
}
