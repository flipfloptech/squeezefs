//! The fleet-launch session-admission convoy (durable-write decomposition,
//! 2026-08-05 — `.benchmarks/2026-08-05-fleet-parity-writes.md`'s open
//! residual): a 256-process fio fleet establishes 256 shim sessions at
//! row start, and the measured client-side cost was ~150–190 ms per
//! process at w256 (connect() convoy behind the hardcoded listen(64)
//! backlog + HELLO→SessionOk service) vs ~8 ms for the kernel arm's
//! open(2). Two contained changes, pinned here:
//!
//! 1. **The ctl listen backlog DERIVES from the ctl connection cap**
//!    (`ipc_host::ctl_listen_backlog`) instead of the hardcoded 64 —
//!    the derivation law (AGENTS.md: fixed clamps need a documented
//!    reason on the line; the field shape is exactly 256 simultaneous
//!    connects, four times that constant). Floor 64 = the shipped
//!    posture (never regress below shipped); rail 4096 = the ctl-thread
//!    rail (`ctl_conn_cap_from`'s own rail); listen(2) additionally
//!    truncates to `net.core.somaxconn`, which stays kernel-owned.
//!
//! 2. **Session admission latency is a first-class instrument**
//!    (`ipc_session_admission_ns`, stats inode, ALWAYS-ON): HELLO
//!    receipt → SessionOk sent, one histogram sample per ADMITTED
//!    session — the field adjudication instrument for the convoy (a
//!    fleet row's delta names the daemon-side share of the launch term
//!    without a remount; the client-side connect share is the backlog's).

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{
    ctl_listen_backlog, DataOp, IpcHost, IpcHostConfig, SessionSink, SlotCompletion,
};
use squeezefs_il::session::Session;
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::wire::BootstrapBlob;

use std::os::fd::AsRawFd;
use std::sync::Arc;

const TEST_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn my_uid() -> u32 {
    // SAFETY: getuid is trivially safe.
    unsafe { libc::getuid() }
}

fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 2 * 1024 * 1024,
        max_op_bytes: 64 * 1024,
        _pad: 0,
    }
}

/// A sink that answers every op immediately (the admission tests never
/// exercise the data plane).
struct NoopSink;

impl SessionSink for NoopSink {
    fn serve_data(&self, op: DataOp, completion: SlotCompletion) {
        completion.complete(op.payload.len() as i64);
    }
    fn on_bind(&self, _ino: u64) {}
    fn on_last_unbind(&self, _ino: u64) {}
    fn flush(&self) {}
}

/// The derivation tie test (drift-is-red): backlog tracks the ctl cap
/// between the shipped floor and the thread rail.
#[test]
fn ctl_listen_backlog_derives_from_the_ctl_cap() {
    // Floor: the pre-2026-08-05 shipped constant — a tiny budget still
    // listens at least as deep as it always did.
    assert_eq!(ctl_listen_backlog(1), 64);
    assert_eq!(ctl_listen_backlog(32), 64);
    // Tracking: the field fleet shape. `ctl_conn_cap_from` doubles the
    // session budget, so a 256-session budget yields cap 512 — the
    // backlog must admit the whole fleet's simultaneous connect burst
    // instead of convoying it through 64-slot accept windows.
    assert_eq!(ctl_listen_backlog(512), 512);
    // Rail: bounded by the same 4096 the ctl-thread cap rails at.
    assert_eq!(ctl_listen_backlog(100_000), 4096);
}

/// The instrument: one `ipc_session_admission_ns` sample per admitted
/// session, none for a refused HELLO. (×10 in the verification run —
/// async multi_thread per the house rule.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_admission_records_one_latency_sample_per_admission() {
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-admlat-{}", std::process::id()),
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
    let host = IpcHost::spawn(cfg, Arc::new(NoopSink)).expect("host must spawn");
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

    let samples = || -> u64 {
        METRICS
            .ipc_session_admission_ns
            .buckets
            .iter()
            .map(|b| b.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    };
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).unwrap();

    let before = samples();
    let _session = tokio::task::block_in_place(|| {
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
            .expect("session establishes")
    });
    assert_eq!(
        samples() - before,
        1,
        "an admitted session must record exactly one admission-latency sample"
    );

    // A refused HELLO (KD-7 skew) records nothing — the histogram is the
    // ADMITTED-establishment cost, refusals stay on the refusal counters.
    let before = samples();
    let skewed = tokio::task::block_in_place(|| {
        Session::establish(
            &blob,
            f.as_raw_fd(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            my_uid(),
        )
    });
    assert!(skewed.is_err(), "a skewed build commit must refuse");
    assert_eq!(
        samples() - before,
        0,
        "a refused HELLO must not record an admission-latency sample"
    );

    host.shutdown();
}

/// The convoy shape itself: a burst of simultaneous session
/// establishments all succeed (nothing in the burst is refused or
/// dropped by the listen backlog) and each records its sample.
/// Width 32 keeps the fixture inside the per-test budget; the w256
/// venue evidence lives in the campaign note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_establishment_burst_is_admitted_completely() {
    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-admburst-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: test_geometry(),
        arena_cap_bytes: 512 * 1024 * 1024,
        per_uid_session_cap: 64,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    };
    let host = IpcHost::spawn(cfg, Arc::new(NoopSink)).expect("host must spawn");
    let dir = tempfile::tempdir().unwrap();
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).unwrap().dev()
    };
    host.set_expected_st_dev(st_dev);

    let samples = || -> u64 {
        METRICS
            .ipc_session_admission_ns
            .buckets
            .iter()
            .map(|b| b.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    };
    let before = samples();

    const WIDTH: usize = 32;
    let blob = Arc::new(BootstrapBlob::decode(&host.bootstrap_blob()).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(WIDTH));
    let mut joins = Vec::new();
    for i in 0..WIDTH {
        let blob = Arc::clone(&blob);
        let barrier = Arc::clone(&barrier);
        let path = dir.path().join(format!("f{i}.bin"));
        std::fs::write(&path, vec![7u8; 4096]).unwrap();
        joins.push(std::thread::spawn(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            barrier.wait(); // the simultaneous-connect burst
            Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
                .map(|s| (s, f))
                .expect("every session in the burst must establish")
        }));
    }
    let sessions: Vec<_> = joins
        .into_iter()
        .map(|j| j.join().expect("establish thread"))
        .collect();
    assert_eq!(sessions.len(), WIDTH);
    assert_eq!(
        samples() - before,
        WIDTH as u64,
        "every admitted session in the burst records its admission sample"
    );
    drop(sessions);
    host.shutdown();
}
