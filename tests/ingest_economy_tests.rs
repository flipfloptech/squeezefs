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
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_il::session::Session;
use squeezefs_ipc::layout::Geometry;
use squeezefs_ipc::sizing::il_sessions_default;
use squeezefs_ipc::wire::BootstrapBlob;

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
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

/// VAL-4 (daemon-authentication ladder): the mount-root owner the shim
/// checks the socket peer against. Harness "mounts" are tempdirs owned
/// by the test user, so the honest anchor is our own uid.
fn my_uid() -> u32 {
    // SAFETY: getuid is trivially safe.
    unsafe { libc::getuid() }
}

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
///
/// The OS-thread census assumes the gate's `--test-threads=1` (the
/// other host-spawning test in this binary would race it in parallel
/// mode — the preload_session suite documents the same posture).
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
    let session_a = Session::establish(&blob, f_a.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("session A establishes");
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
    let session_b = Session::establish(&blob, f_b.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("session B establishes");
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

// ---------------------------------------------------------------------------
// 4. — the severed-write buffer pool (the per-byte serve-cost conviction)
// ---------------------------------------------------------------------------
//
// Profile (TCP devsub rig, fio psync t16 b4m zero_buffers via the shim,
// 10.2-12.7 GB/s): the sqz-ipc-svc threads saturate ~70 % SYSTEM time —
// ~1.76 M minor faults/s across 5 threads plus a ~5.2 k/s jemalloc
// madvise purge stream. The engine is `ArenaWindow::read_severed`'s
// per-op `vec![0u8; len]`: a 1 MiB slab-sized alloc per ring write is
// past jemalloc's tcache ceiling, so every op walks the arena mutex,
// gets purged (MADV_FREE) pages back, RE-FAULTS ~256 of them (kernel
// re-zeroing included — pure waste, every byte is overwritten by the
// sever memcpy), then frees the extent again. The fix: severed copies
// come from a per-host recycle pool (`Bytes::from_owner` over a pooled
// buffer that returns on drop) — the §5.5.2 ONE-arena-read severance
// law is untouched (same single copy, warm destination), so the
// zero-copy write-path budget is unchanged.

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
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new("ingest_economy_tests")
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

/// Test seam from tests/preload_session_tests.rs: harness-bound fds are
/// tempdir files; map st_ino → fs-ino at the sink boundary.
struct InoMapSink {
    inner: DataPlaneSink,
    map: Mutex<HashMap<u64, u64>>,
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
            panic!("harness bound an untranslated st_ino {}", op.binding.ino);
        };
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn flush(&self) {
        SessionSink::flush(&self.inner);
    }
}

/// Production write severs recycle pooled buffers: after a short warmup,
/// a steady ring-write stream stops allocating fresh severed buffers
/// (`ipc_severed_pool_hits` accounts the window; misses stay bounded by
/// the warmup), and the retained-bytes gauge is visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_write_sever_recycles_pooled_buffers() {
    use fuse3::raw::prelude::Filesystem as _;
    let (fs, _bt, _mt, _st) = sandbox_fs().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.bin");
    std::fs::write(&path, vec![0u8; 4096]).expect("seed file");
    // Create the fs-side inode the writes land on.
    let req = fuse3::raw::Request {
        unique: 1,
        // SAFETY: getuid/getgid are trivially safe.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
        ..Default::default()
    };
    let created = fs
        .create(
            req,
            1,
            std::ffi::OsStr::new("w.bin"),
            0o644,
            libc::O_RDWR as u32,
        )
        .await
        .expect("fs create");
    let fs_ino = created.attr.ino;

    let st_ino = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&path).expect("metadata").ino()
    };
    let sink = InoMapSink {
        inner: DataPlaneSink::new(fs),
        map: Mutex::new(HashMap::from([(st_ino, fs_ino)])),
    };

    let cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-sever-{}", std::process::id()),
        socket_dir: None,
        build_commit: TEST_COMMIT.to_string(),
        allow_dev: false,
        geometry: Geometry {
            ring_entries: 16,
            slots: 16,
            arena_bytes: 4 * 1024 * 1024,
            max_op_bytes: 256 * 1024,
            _pad: 0,
        },
        arena_cap_bytes: 64 * 1024 * 1024,
        per_uid_session_cap: 8,
        idle_secs: 0,
        data_plane: true,
        // SAFETY: getuid is trivially safe.
        owner_uid: unsafe { libc::getuid() },
    };
    let host = IpcHost::spawn(cfg, Arc::new(sink)).expect("host must spawn");
    let st_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir.path()).expect("metadata").dev()
    };
    host.set_expected_st_dev(st_dev);
    let blob = BootstrapBlob::decode(&host.bootstrap_blob()).expect("blob");
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open");
    let session = Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT, my_uid())
        .expect("session establishes");
    let bind = session.bind(f.as_raw_fd()).expect("bind");

    let payload = vec![0xa5u8; 64 * 1024];
    let pwrite = |off: u64| match session.ring_pwrite(bind.binding_id, &payload, off) {
        squeezefs_il::session::RingOutcome::Served(n) => assert_eq!(n, payload.len()),
        other => panic!("ring pwrite must serve: {other:?}"),
    };

    tokio::task::block_in_place(|| {
        // Warmup: let the pool mint its steady-state buffers.
        for i in 0..16u64 {
            pwrite(i * 64 * 1024);
        }
        let h0 = METRICS.ipc_severed_pool_hits.load(Ordering::Relaxed);
        let m0 = METRICS.ipc_severed_pool_misses.load(Ordering::Relaxed);
        for i in 16..256u64 {
            pwrite((i % 32) * 64 * 1024);
        }
        let dh = METRICS.ipc_severed_pool_hits.load(Ordering::Relaxed) - h0;
        let dm = METRICS.ipc_severed_pool_misses.load(Ordering::Relaxed) - m0;
        assert_eq!(
            dh + dm,
            240,
            "every ring write severs exactly once through the pool \
             (the §5.5.2 one-arena-read law, now pool-accounted)"
        );
        assert!(
            dm <= 16,
            "steady-state severs must reuse pooled buffers (got {dm} fresh \
             allocs in a 240-op warm window) — per-op slab-sized allocs are \
             the profiled 1.76M-faults/s svc-thread engine"
        );
        assert!(
            METRICS.ipc_severed_pool_bytes.load(Ordering::Relaxed) > 0,
            "the retained-bytes gauge must be visible while buffers are pooled"
        );
    });
    host.shutdown();
}
