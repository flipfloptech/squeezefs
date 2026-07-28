//! Ingest-economy campaign (2026-07-28) — the session/service-thread
//! **defaults derivation** and **spawn-on-bind** contracts
//! (`.benchmarks/2026-07-28-ingest-economy.md`).
//!
//! Field conviction (4-node 2×200GbE nvme-tcp cluster, 32-CPU client):
//! large sequential ingest over the shim walled at 7.5 GB/s against a
//! 16.6 GB/s raw-fio ceiling because the shim's session default (flat 4)
//! and the daemon's service-thread default (`clamp(cpus/4, 2, 8)`) were
//! two unrelated constants — pidstat showed `sqz-ipc-svc0..3` at 94–99 %
//! CPU with `svc4..7` permanently idle (spawned, parked, no ring to
//! drain). `SQUEEZEFS_IL_SESSIONS=8` recovered +44 % on the spot.
//!
//! The contracts pinned here:
//!
//! 1. **One derivation** — the daemon's service-thread ceiling default
//!    IS `squeezefs_ipc::sizing::il_sessions_default` (the same function
//!    the shim's per-mount session default rides; its own tie test lives
//!    in `squeezefs-preload`). The pair cannot drift without turning one
//!    of the paired tests red.
//! 2. **Override levers stay levers** — `SQUEEZEFS_IPC_SERVICE_THREADS`
//!    is honored verbatim within its clamp; the default is never a
//!    constant.
//! 3. **Spawn-on-bind** — a service thread exists only once a session is
//!    pinned to it: zero `sqz-ipc-svc*` threads on a host with no
//!    sessions (the every-mount control-plane-only posture now spawns
//!    NO service threads), one per distinct owner as sessions admit, and
//!    the `ipc_service_threads` gauge reports the SPAWNED count.

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{
    service_thread_ceiling_from, DataOp, IpcHost, IpcHostConfig, SessionSink, SlotCompletion,
};
use squeezefs_il::session::Session;
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::sizing::il_sessions_default;
use squeezefs_ipc::wire::BootstrapBlob;

use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 1. + 2. — the derivation tie (pure forms, machine-independent)
// ---------------------------------------------------------------------------

/// The daemon's service-thread ceiling default derives from the SAME
/// function as the shim's per-mount session default — for every machine
/// size, not just the box running the test (the field mismatch was
/// invisible at cpus ≤ 32 where the old clamps agreed up to the shim's
/// flat 4).
#[test]
fn service_ceiling_default_ties_to_shim_session_default() {
    for cpus in [1, 2, 4, 8, 16, 22, 32, 48, 64, 128, 256] {
        assert_eq!(
            service_thread_ceiling_from(None, cpus),
            il_sessions_default(cpus),
            "daemon service-thread ceiling default must ride the shared \
             derivation (cpus={cpus}) — a drift here recreates the field's \
             idle-thread/starved-session mismatch"
        );
    }
}

/// `SQUEEZEFS_IPC_SERVICE_THREADS` remains an override lever only:
/// honored verbatim within the 1..=64 clamp, unparseable ⇒ the derived
/// default (never a silent constant).
#[test]
fn service_ceiling_env_override_is_a_lever() {
    assert_eq!(service_thread_ceiling_from(Some("12"), 8), 12);
    assert_eq!(service_thread_ceiling_from(Some("1"), 64), 1);
    assert_eq!(service_thread_ceiling_from(Some("0"), 64), 1, "clamp floor");
    assert_eq!(
        service_thread_ceiling_from(Some("999"), 8),
        64,
        "clamp ceiling"
    );
    assert_eq!(
        service_thread_ceiling_from(Some("garbage"), 32),
        il_sessions_default(32),
        "unparseable falls back to the derivation"
    );
}

// ---------------------------------------------------------------------------
// 3. — spawn-on-bind (real host, real shim session client)
// ---------------------------------------------------------------------------

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

/// Count this process's live `sqz-ipc-svc*` OS threads (spawn-on-bind's
/// observable: the thread either exists or it does not).
fn svc_os_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("/proc/self/task")
        .filter(|e| {
            e.as_ref()
                .ok()
                .and_then(|e| std::fs::read_to_string(e.path().join("comm")).ok())
                .is_some_and(|name| name.trim_end().starts_with("sqz-ipc-svc"))
        })
        .count()
}

fn wait_for_svc_threads(n: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while svc_os_threads() != n {
        assert!(
            Instant::now() < deadline,
            "{what}: expected {n} sqz-ipc-svc threads, still {} after 10 s",
            svc_os_threads()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Serve READs instantly with their full length (no fs stack — this file
/// pins host thread lifecycle, not the data plane).
struct InstantSink;

impl SessionSink for InstantSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        completion.complete(i64::from(op.desc.len));
    }
}

/// The spawn-on-bind lifecycle: 0 threads with 0 sessions (host spawn is
/// thread-free), +1 per distinct owner as sessions admit, a late-spawned
/// thread actually serves, the gauge tracks the spawned count, and
/// shutdown joins everything back to 0.
#[test]
fn service_threads_spawn_on_bind_no_parked_spares() {
    assert_eq!(
        svc_os_threads(),
        0,
        "test invariant: no service threads before any host exists"
    );
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-ingest-{}", std::process::id()),
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
    let host = IpcHost::spawn(cfg, Arc::new(InstantSink)).expect("host must spawn");

    // No sessions ⇒ no service threads (the field's parked spares are
    // unrepresentable; a control-plane-only mount pays zero).
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        svc_os_threads(),
        0,
        "a host with no sessions must own no sqz-ipc-svc threads \
         (spawn-on-bind: parked spare threads are the field bug)"
    );
    assert_eq!(
        METRICS.ipc_service_threads.load(Ordering::Relaxed),
        0,
        "the gauge reports SPAWNED threads — 0 before any session binds"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).expect("metadata").dev()
    };
    host.set_expected_st_dev(st_dev);
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");

    // Session 1 ⇒ exactly one service thread, and it serves.
    let path_a = dir.path().join("a.bin");
    std::fs::write(&path_a, vec![7u8; 8192]).expect("write a");
    let f_a = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path_a)
        .expect("open a");
    let session_a =
        Session::establish(&blob, f_a.as_raw_fd(), TEST_COMMIT).expect("session A establishes");
    let bind_a = session_a.bind(f_a.as_raw_fd()).expect("bind A");
    wait_for_svc_threads(1, "after session A admitted");
    assert_eq!(
        METRICS.ipc_service_threads.load(Ordering::Relaxed),
        1,
        "gauge tracks the first spawn"
    );
    let mut buf = [0u8; 512];
    match session_a.ring_pread(bind_a.binding_id, &mut buf, 0) {
        squeezefs_il::session::RingOutcome::Served(n) => {
            assert_eq!(n, 512, "the late-spawned thread must actually serve");
        }
        other => panic!("ring pread on the spawn-on-bind thread: {other:?}"),
    }

    // Session 2 ⇒ a second owner, a second thread.
    let path_b = dir.path().join("b.bin");
    std::fs::write(&path_b, vec![9u8; 8192]).expect("write b");
    let f_b = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path_b)
        .expect("open b");
    let session_b =
        Session::establish(&blob, f_b.as_raw_fd(), TEST_COMMIT).expect("session B establishes");
    let _bind_b = session_b.bind(f_b.as_raw_fd()).expect("bind B");
    wait_for_svc_threads(2, "after session B admitted");
    assert_eq!(
        METRICS.ipc_service_threads.load(Ordering::Relaxed),
        2,
        "gauge tracks the second spawn"
    );

    host.shutdown();
    wait_for_svc_threads(0, "after host shutdown (threads must join)");
}
