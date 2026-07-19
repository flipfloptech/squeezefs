//! PR L4-7 — direct-to-arena DMA for ring reads (§5.5.3 / §5.5.2 copy
//! ledger): the read handoff registers its op's arena window as the
//! router's `dest_addr`, so device-path reads land **directly in the
//! completion payload region** — deleting the pooled-buffer→arena copy
//! (the ledger's "DMA→arena → 1 copy + 1 DMA, strictly one better than
//! kernel FUSE can ever do").
//!
//! Discipline pinned here:
//! - **Parity is unconditional**: the DMA is an optimization the router
//!   may decline (transform volumes, unaligned shapes, tier serves) —
//!   the sink detects where the bytes actually landed (pointer
//!   equality, the fuse3 reply-path precedent) and copies only when
//!   they landed elsewhere. Wrong bytes can never appear either way.
//! - **§5.3.1 rule 3**: the DMA *writes* client-visible memory, never
//!   reads it back — reads have no severance/interpretation surface.
//! - `ipc_arena_dma_reads` counts landed DMAs (the A/B instrument);
//!   `SQUEEZEFS_IL_ARENA_DMA=0` is the acceptance A/B lever (the
//!   PATCH_MAX_BYTES=0 precedent — not an operational escape).

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
        BlockAllocator::new(dlm.meta_client().clone(), "preload_dma_tests")
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

impl SessionSink for InoMapSink {
    fn serve_data(&self, mut op: DataOp, completion: SlotCompletion) {
        let fs_ino = self
            .map
            .lock()
            .expect("ino map mutex never poisons")
            .get(&op.binding.ino)
            .copied()
            .unwrap_or_else(|| panic!("untranslated st_ino {}", op.binding.ino));
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
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
}

impl Fixture {
    async fn new(name: &str) -> Fixture {
        let (fs, backing, meta, staging) = sandbox_fs().await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone(), tokio::runtime::Handle::current()),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-dma-{}-{}", std::process::id(), name),
            build_commit: TEST_COMMIT.to_string(),
            allow_dev: false,
            geometry: squeezefs_ipc::layout::Geometry {
                ring_entries: 16,
                slots: 16,
                // 16 slots over 2 MiB ⇒ 128 KiB slabs; max_op 64 KiB.
                // Slab starts are 4 KiB-aligned by construction — the
                // DMA eligibility shape.
                arena_bytes: 2 * 1024 * 1024,
                max_op_bytes: 64 * 1024,
                _pad: 0,
            },
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
        }
    }

    fn establish(&self) -> Session {
        let path = self.dir.path().join(".hello-cred");
        std::fs::write(&path, b"x").expect("cred file");
        let f = std::fs::File::open(&path).expect("open cred");
        let blob = BootstrapBlob::decode(&self.host.bootstrap_blob()).expect("blob decodes");
        Session::establish(&blob, f.as_raw_fd(), TEST_COMMIT).expect("session must establish")
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

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 17) % 251) as u8)
        .collect()
}

/// Cold-read parity through the DMA-eligible shape: write via the ring,
/// fsync + drop the RAM state so reads must reach the router, then
/// ring-read back — parity AND the DMA counter must both hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dma_read_parity_and_counter_on_cold_router_reads() {
    let fx = Fixture::new("parity").await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("dma.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    // 8 MiB spans multiple 4 MiB blocks — striped layout, real backend
    // blocks after flush.
    let w = deterministic_bytes(8 * 1024 * 1024, 7);
    match tokio::task::block_in_place(|| session.ring_pwrite(grant.binding_id, &w, 0)) {
        RingOutcome::Served(n) => assert_eq!(n, w.len()),
        other => panic!("write must serve, got {other:?}"),
    }
    fx.fs
        .fsync(req(), ino, 0, false)
        .await
        .expect("fsync flushes active state to the backend");
    // Drop RAM copies so ring reads MUST take the router/device path
    // (the DMA-eligible shape).
    fx.fs.attr_cache.invalidate(&ino);
    fx.fs.router.metadata_cache.invalidate(&ino);

    let dma_before = METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed);
    let mut buf = vec![0u8; w.len()];
    let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
    assert!(
        matches!(out, RingOutcome::Served(n) if n == w.len()),
        "cold ring read must serve fully, got {out:?}"
    );
    assert_eq!(buf, w, "byte parity through the DMA read path");

    let dma = METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed) - dma_before;
    assert!(
        dma > 0,
        "cold aligned ring reads must land at least some direct-to-arena DMAs (got 0 — \
         the dest plumb is dead)"
    );

    // FUSE-side parity too (the daemon state is transport-agnostic).
    let reply = fx
        .fs
        .read(req(), ino, 0, 4096, 64 * 1024, 0)
        .await
        .expect("fuse read");
    assert_eq!(
        reply.data.to_vec(),
        w[4096..4096 + 64 * 1024],
        "FUSE parity unaffected by the ring DMA plumb"
    );
    fx.host.shutdown();
}

/// The A/B lever: `SQUEEZEFS_IL_ARENA_DMA=0` disables the dest plumb —
/// parity identical, counter frozen. (Env is process-global: this test
/// sets/clears it around the op; the suite runs --test-threads=1.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dma_lever_off_keeps_parity_with_zero_dmas() {
    let fx = Fixture::new("lever").await;
    let session = fx.establish();
    let (ino, fd) = fx.create_file("lever.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    let w = deterministic_bytes(256 * 1024, 9);
    match tokio::task::block_in_place(|| session.ring_pwrite(grant.binding_id, &w, 0)) {
        RingOutcome::Served(n) => assert_eq!(n, w.len()),
        other => panic!("write must serve, got {other:?}"),
    }
    fx.fs.fsync(req(), ino, 0, false).await.expect("fsync");
    fx.fs.attr_cache.invalidate(&ino);
    fx.fs.router.metadata_cache.invalidate(&ino);

    std::env::set_var("SQUEEZEFS_IL_ARENA_DMA", "0");
    let dma_before = METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed);
    let mut buf = vec![0u8; w.len()];
    let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
    std::env::remove_var("SQUEEZEFS_IL_ARENA_DMA");

    assert!(matches!(out, RingOutcome::Served(n) if n == w.len()));
    assert_eq!(buf, w, "parity with the lever off");
    assert_eq!(
        METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed),
        dma_before,
        "lever off ⇒ zero DMAs (the copy path serves)"
    );
    fx.host.shutdown();
}

/// Two premises this test DISPROVED while red, now pinned as facts:
/// a 64 KiB file takes the STAGED layout (no active-block buffer, so
/// warm reads handoff — no fast-path serve), and the staging serve
/// HONORS the dest plumb (bytes land directly in the arena: one
/// staging→arena copy, the pool bounce deleted — counted, parity
/// unconditional). What must never move the counter is the sync fast
/// path, which completes structurally before the plumb exists — the
/// EOF row pins that half.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_serves_land_in_arena_and_fast_path_never_dmas() {
    let fx = Fixture::new("warm").await;
    let session = fx.establish();
    let (_ino, fd) = fx.create_file("warm.bin").await;
    let grant = session.bind(fd.as_raw_fd()).expect("bind");

    let w = deterministic_bytes(64 * 1024, 11);
    match tokio::task::block_in_place(|| session.ring_pwrite(grant.binding_id, &w, 0)) {
        RingOutcome::Served(n) => assert_eq!(n, w.len()),
        other => panic!("write must serve, got {other:?}"),
    }
    // Warm the attr cache (one read may demote-miss), then measure.
    let mut buf = vec![0u8; w.len()];
    let _ = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));

    let out = tokio::task::block_in_place(|| session.ring_pread(grant.binding_id, &mut buf, 0));
    assert!(matches!(out, RingOutcome::Served(n) if n == w.len()));
    assert_eq!(
        buf, w,
        "staged-layout warm read parity (in-arena landing or copied — either way exact)"
    );
    let dma_before = METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed);

    // EOF short-circuit = a sync fast-path serve, structurally pre-DMA.
    let fast_before = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed);
    let out = tokio::task::block_in_place(|| {
        let mut b = vec![0u8; 4096];
        session.ring_pread(grant.binding_id, &mut b, 1 << 40)
    });
    assert!(matches!(out, RingOutcome::Served(0)));
    assert!(
        METRICS.ipc_fast_path_serves.load(Ordering::Relaxed) > fast_before,
        "EOF row still fast-paths"
    );
    assert_eq!(
        METRICS.ipc_arena_dma_reads.load(Ordering::Relaxed),
        dma_before,
        "fast-path serves never touch the DMA plumb"
    );
    fx.host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dma_stats_field_exports() {
    let fx = Fixture::new("stats").await;
    let stats = fx.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json parses");
    let m = v.get("metrics").expect("metrics object");
    assert!(
        m.get("ipc_arena_dma_reads").is_some(),
        "stats inode must export ipc_arena_dma_reads"
    );
    fx.host.shutdown();
}
