//! IL **hold probe** — the il-hold-probe campaign's contracts
//! (2026-08-03; charter `.benchmarks/2026-08-02-il-anomalies.md` §2).
//!
//! The named term: cold rand-4k libaio il rows ran −8 %/+110 µs vs
//! kernel because the kernel path's probe ladder serves ~294 k ops/row
//! from the read-lane HOLD / hot tier at ~µs (hot → hold → NVMe tier,
//! before the ranged dispatch) while the il DIALED-P1.5 prelude never
//! probed the hold — its only warm source was the §5.5.1 sync fast
//! path's hot leg. The fix: the hold becomes the sync fast path's
//! fourth leg (staging → hot → **hold** → NVMe read-cache — the
//! handler ladder's order, hot strictly first so warm hot entries keep
//! funding the governor's payback basis), probed latch-free on the
//! foreign `sqz-ipc-svc*` service thread under the held inode read
//! guard (binding currency structural — leg 3's own argument).
//!
//! The contracts:
//! 1. **Bytes-exact serve + gauges**: a demand-deposited hold entry
//!    serves a ring read byte-exact on the service thread
//!    (`ipc_fast_path_serves`), counted by the engagement pair
//!    (`ipc_hold_probe_serves`, `read_lane_serves`/`_serve_bytes`).
//! 2. **The R1b ledger ceremony lands exactly-once** (the `3cd528b`
//!    law's third serve site): a ledger-visible (demand-provenance)
//!    probe serve dispatches ghost touch → second-touch publish →
//!    protected hot re-landing to the fuse3 handler lanes
//!    (`tpc_spawn` — the 2026-07-26 handoff-economy venue, never
//!    `Handle::spawn`), asynchronously but exactly once per serve.
//! 3. **Lane-fetch deposits stay ledger-invisible end to end** (the
//!    2026-07-26 scan-resistance verdict — read_lane_tests contract 2
//!    extended to the probe site): the serve completes, the ceremony
//!    never runs.
//! 4. **Probe miss preserves current behavior**: `ipc_hold_probe_misses`
//!    counts, the op demotes/direct-drives exactly as before.
//! 5. **A0 (`SQUEEZEFS_READ_LANE=0`) makes the probe structurally
//!    inert**: no probe, both gauges stay 0, a planted entry is never
//!    consumed — exact prior behavior.
//! 6. **Sync hot-leg serves credit the hold copy** (the "every
//!    consumption path credits consumed bytes" law, mirrored from the
//!    kernel handler's hot arm): full ring-read coverage through the
//!    hot leg retires the held entry — memory converges by
//!    consumption, and `hold_evicted_unconsumed` keeps meaning
//!    starvation on il rows.
//!
//! Harness: the preload_parity_tests raw-protocol client (no kernel in
//! the loop; the daemon side is the REAL host → service thread → sink
//! path). Counter-asserting phases take deltas within each test
//! (METRICS is process-global; the suite runs under `--test-threads=1`
//! per the house gate).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::CtlMsg;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 512 KiB blocks: whole-block writes go write-through striped, and
/// every fill sits above the 256 KiB hold-participation boundary.
const BS: u64 = 524_288;

// ---------------------------------------------------------------------------
// fixture: admission-suite fs (512 KiB blocks, ranged dispatch off) +
// the raw-protocol ring client (the ipc_op_economy_tests shape)
// ---------------------------------------------------------------------------

async fn sandbox_fs(uuid: [u8; 16], read_lane_off: bool) -> (SqueezefsFilesystem, Fenv) {
    // Env pins live only across router construction (the admission-suite
    // pattern; the suite runs --test-threads=1 so no cross-test races).
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Whole-block fill machinery must engage (the hold's deposit vector);
    // the ranged path's own contracts live in ranged_read_tests.
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    if read_lane_off {
        std::env::set_var("SQUEEZEFS_READ_LANE", "0");
    }
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "ipc_hold_probe_tests")
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    if read_lane_off {
        std::env::remove_var("SQUEEZEFS_READ_LANE");
    }

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    (
        fs,
        Fenv {
            _b: b,
            _m: m,
            _s: staging,
        },
    )
}

/// Keeps the backing files alive for the fixture's lifetime.
struct Fenv {
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
    }
}

/// Ino-translating sink (harness stand-in fds carry the stand-in file's
/// `st_ino`; the sink rewrites to the sandbox fs ino before serving).
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
            panic!(
                "harness bound an untranslated st_ino {} — register it in the InoMapSink",
                op.binding.ino
            );
        };
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn flush(&self) {
        SessionSink::flush(&self.inner);
    }
}

fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 2 * 1024 * 1024,
        max_op_bytes: 128 * 1024,
        _pad: 0,
    }
}

struct Fixture {
    fs: SqueezefsFilesystem,
    host: Arc<IpcHost>,
    cfg: IpcHostConfig,
    sink: Arc<InoMapSink>,
    _env: Fenv,
}

impl Fixture {
    async fn new(name: &str, uuid: [u8; 16], read_lane_off: bool) -> Fixture {
        let (fs, env) = sandbox_fs(uuid, read_lane_off).await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone()),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-hp-{}-{}", std::process::id(), name),
            socket_dir: None,
            build_commit: "a".repeat(40),
            allow_dev: false,
            geometry: test_geometry(),
            arena_cap_bytes: 64 * 1024 * 1024,
            per_uid_session_cap: 8,
            idle_secs: 0,
            data_plane: true,
            // SAFETY: getuid is trivially safe.
            owner_uid: unsafe { libc::getuid() },
        };
        let host = IpcHost::spawn(cfg.clone(), sink.clone()).expect("host must spawn");
        Fixture {
            fs,
            host,
            cfg,
            sink,
            _env: env,
        }
    }

    /// Shift ino allocation so parallel suites never contend on the
    /// process-global DLM lock map for the same `inode_N` key.
    async fn salt_inos(&self, n: usize) {
        for i in 0..n {
            let name = format!("salt{i}");
            self.fs
                .create(req(), 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                .await
                .expect("salt create");
        }
    }

    async fn create_file(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino
    }

    async fn fuse_write(&self, ino: u64, offset: u64, data: &[u8]) {
        let w = self
            .fs
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
            .expect("fuse write");
        assert_eq!(w.written as usize, data.len(), "short write at {offset}");
    }

    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read")
            .data
            .to_vec()
    }

    async fn block_map_of(&self, ino: u64) -> Arc<HashMap<u32, String>> {
        let path = squeezefs::keys::inode_path(ino);
        self.fs
            .router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default()
    }

    /// fsync + purge every block key across all tiers AND the hold —
    /// the admission-suite `make_cold`.
    async fn make_cold(&self, ino: u64) -> Arc<HashMap<u32, String>> {
        self.fs.fsync(req(), ino, 0, false).await.unwrap();
        let map = self.block_map_of(ino).await;
        for key in map.values() {
            self.fs.router.cache.purge_block_key(key);
        }
        map
    }

    /// Deterministic §5.5.1 precondition: the sync fast path demotes on
    /// a cold attr cache before it can probe anything — warm it through
    /// the FUSE getattr handler (its miss arm seeds the cache).
    async fn warm_attr(&self, ino: u64) {
        let _ = self.fs.getattr(req(), ino, None, 0).await.expect("getattr");
        assert!(
            self.fs.attr_cache.get(&ino).is_some(),
            "fixture: getattr must seed the attr cache"
        );
    }

    fn tier_has(&self, key: &str) -> bool {
        self.fs
            .router
            .cache
            .nvme
            .get_cached_read_block(key)
            .is_some()
    }

    fn hot_has(&self, key: &str) -> bool {
        self.fs.router.cache.hot_block.get(key).is_some()
    }

    fn shutdown(&self) {
        self.host.shutdown();
    }
}

/// Buffered stand-in fd on a real filesystem, with the host's expected
/// st_dev re-pointed and the ino translation registered.
fn buffered_standin(fx: &Fixture, dir: &TempDir, name: &str, fs_ino: u64) -> OwnedFd {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("create buffered stand-in");
    f.write_all(&[0u8; 16]).expect("stand-in bytes");
    drop(f);
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(&path).expect("stand-in metadata");
    fx.host.set_expected_st_dev(md.dev());
    fx.sink
        .map
        .lock()
        .expect("ino map mutex never poisons")
        .insert(md.ino(), fs_ino);
    let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: plain open(2); ownership taken immediately.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    assert!(fd >= 0, "buffered open failed");
    // SAFETY: fresh owned fd.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

struct ClientSession {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
    _sock: UnixStream,
}

// SAFETY: harness drives ops from one thread; shared memory + atomics.
unsafe impl Send for ClientSession {}

impl ClientSession {
    fn establish(fx: &Fixture, fd: &OwnedFd) -> (ClientSession, u64) {
        let sock = abstract_connect(&fx.cfg.socket_name).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("SO_RCVTIMEO");
        send_ctl(
            &sock,
            &CtlMsg::Hello {
                abi: squeezefs_ipc::layout::IPC_ABI,
                pid: std::process::id(),
                uid: unsafe { libc::getuid() },
                build_commit: fx.cfg.build_commit.clone(),
                nonce: fx.host.current_nonce(),
            },
            Some(fd.as_raw_fd()),
        )
        .expect("send HELLO");
        let (reply, memfd) = recv_ctl(&sock).expect("recv HELLO reply");
        let geometry = match reply {
            CtlMsg::SessionOk { geometry } => geometry,
            other => panic!("expected SessionOk, got {other:?}"),
        };
        let memfd = memfd.expect("SessionOk must carry the memfd");
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
        let session = ClientSession {
            base: base as *mut u8,
            layout,
            geometry,
            _sock: sock,
        };
        session.header().validate().expect("header validates");
        send_ctl(&session._sock, &CtlMsg::Bind, Some(fd.as_raw_fd())).expect("send BIND");
        let (reply, none) = recv_ctl(&session._sock).expect("recv BIND reply");
        assert!(none.is_none());
        let binding = match reply {
            CtlMsg::BindOk { binding_id, .. } => binding_id,
            other => panic!("expected BindOk, got {other:?}"),
        };
        (session, binding)
    }

    fn header(&self) -> &SessionHeader {
        // SAFETY: header page at offset 0 of a mapping sized by layout.
        unsafe { &*(self.base as *const SessionHeader) }
    }

    fn ring(&self) -> MpscRingView<'_> {
        // SAFETY: offsets inside the mapping; repr(C) protocol types.
        unsafe {
            let tail = &*(self.base.add(self.layout.ring_off as usize) as *const AtomicU32);
            let cells = std::slice::from_raw_parts(
                self.base.add(self.layout.ring_cells_off as usize) as *const RingCell,
                self.geometry.ring_entries as usize,
            );
            MpscRingView::from_parts(tail, cells).expect("ring view")
        }
    }

    fn slot(&self, i: u32) -> &IpcSlot {
        assert!(i < self.geometry.slots);
        // SAFETY: bounds asserted; slots at slots_off.
        unsafe {
            &*((self.base.add(self.layout.slots_off as usize) as *const IpcSlot).add(i as usize))
        }
    }

    fn arena_read_into(&self, off: u64, out: &mut [u8]) {
        assert!(off + out.len() as u64 <= self.geometry.arena_bytes);
        // SAFETY: bounds asserted against the arena region.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.add((self.layout.arena_off + off) as usize),
                out.as_mut_ptr(),
                out.len(),
            )
        };
    }

    /// One ring pread on slot 0: claim → publish → push → doorbell →
    /// bounded spin. Panics on failure/timeouts.
    fn ring_pread_spin(&self, binding: u64, offset: u64, len: u32) -> i64 {
        let slot = self.slot(0);
        let gen = slot.core.try_claim().expect("slot 0 must be FREE");
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding,
            offset,
            len,
            arena_off: 0,
        });
        slot.core.publish_submitted();
        assert!(self.ring().push(0), "ring must accept");
        self.header().doorbell.fetch_add(1, Ordering::Release);
        if self.header().daemon_parked.load(Ordering::SeqCst) != 0 {
            futex_wake(&self.header().doorbell, 1);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while !slot.core.is_done_for(gen) {
            assert!(Instant::now() < deadline, "ring read never completed");
            std::hint::spin_loop();
        }
        let r = slot.result();
        slot.core.release();
        r
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `establish`.
        unsafe {
            libc::munmap(
                self.base as *mut libc::c_void,
                self.layout.total_bytes as usize,
            );
        }
    }
}

/// Poll `cond` at 25 ms cadence up to `secs`; false on deadline.
fn poll_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cond()
}

// ---------------------------------------------------------------------------
// Contract 1 + 2 — bytes-exact probe serve; the R1b ceremony lands
// exactly-once on the handler lanes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_probe_serves_bytes_exact_and_ceremony_lands_exactly_once() {
    let fx = Fixture::new("srv", *b"il-hold-probe-01", false).await;
    fx.salt_inos(2).await;
    let ino = fx.create_file("probe_serve.bin").await;
    fx.fuse_write(ino, 0, &vec![0xA1u8; BS as usize]).await;
    fx.fuse_write(ino, BS, &vec![0xA2u8; BS as usize]).await;
    let map = fx.make_cold(ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();

    // Demand fill (touch 1): deposits the block in the hold with DEMAND
    // provenance, lands hot probation, records the ghost first touch.
    let d = fx.fuse_read(ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xA1));
    assert!(
        fx.fs.router.cache.read_lane_hold.contains(&k0),
        "fixture: the demand fill must deposit in the hold"
    );
    assert!(
        !fx.tier_has(&k0),
        "fixture: first touch skipped the publish"
    );
    // Kill the hot copy: the ring read's ONLY warm source is the hold.
    fx.fs.router.cache.hot_block.remove(&k0);
    fx.warm_attr(ino).await;

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let serves0 = METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed);
    let fast0 = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed);
    let lane_serves0 = METRICS.read_lane_serves.load(Ordering::Relaxed);
    let lane_bytes0 = METRICS.read_lane_serve_bytes.load(Ordering::Relaxed);
    let ghost0 = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed);
    let adm0 = METRICS.read_tier_admissions.load(Ordering::Relaxed);
    let dev0 = METRICS.get_obj.load(Ordering::Relaxed);

    let r = tokio::task::block_in_place(|| session.ring_pread_spin(binding, 8192, 4096));
    assert_eq!(r, 4096, "hold-probe serve must return the full request");
    let mut got = vec![0u8; 4096];
    session.arena_read_into(0, &mut got);
    assert!(
        got.iter().all(|&x| x == 0xA1),
        "hold-probe serve must be BYTES-EXACT into the arena window"
    );

    // Contract 1 gauges: one probe serve, one sync fast-path serve, one
    // read-lane serve of exactly the request length; zero device bytes.
    assert_eq!(
        METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed) - serves0,
        1,
        "the engagement gauge must account for the serve"
    );
    assert_eq!(
        METRICS.ipc_fast_path_serves.load(Ordering::Relaxed) - fast0,
        1,
        "a hold-probe serve completes on the service thread (§5.5.1)"
    );
    assert_eq!(
        METRICS.read_lane_serves.load(Ordering::Relaxed) - lane_serves0,
        1
    );
    assert_eq!(
        METRICS.read_lane_serve_bytes.load(Ordering::Relaxed) - lane_bytes0,
        4096,
        "serve bytes are exact (the field bracket's validity instrument)"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - dev0,
        0,
        "the serve must pay zero device fetches"
    );

    // Contract 2: the R1b ceremony (ghost touch → second-touch publish →
    // protected hot re-landing) lands ASYNCHRONOUSLY on the handler
    // lanes — poll for it, then assert exactly-once after a settle beat.
    assert!(
        poll_until(5, || {
            METRICS
                .read_tier_admission_ghost_hits
                .load(Ordering::Relaxed)
                > ghost0
                && fx.tier_has(&k0)
                && fx.hot_has(&k0)
        }),
        "the ledger-visible probe serve must run the R1b ceremony: ghost \
         touch, second-touch tier publish, protected hot re-landing"
    );
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        METRICS
            .read_tier_admission_ghost_hits
            .load(Ordering::Relaxed)
            - ghost0,
        1,
        "the ceremony's ghost touch lands EXACTLY once per serve"
    );
    assert_eq!(
        METRICS.read_tier_admissions.load(Ordering::Relaxed) - adm0,
        1,
        "the ceremony's admission lands EXACTLY once per serve"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// Contract 3 — lane-fetch (ledger-invisible) deposits serve without any
// ceremony (scan resistance stands at the probe site)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_probe_lane_deposits_stay_ledger_invisible() {
    let fx = Fixture::new("inv", *b"il-hold-probe-02", false).await;
    fx.salt_inos(4).await;
    let ino = fx.create_file("probe_invisible.bin").await;
    fx.fuse_write(ino, 0, &vec![0xB7u8; BS as usize]).await;
    fx.fuse_write(ino, BS, &vec![0xB8u8; BS as usize]).await;
    let map = fx.make_cold(ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();

    // Plant the entry with LANE provenance (ledger-invisible serves) —
    // the ahead-fetch deposit class, planted directly (the lane's own
    // deposit machinery is pinned by read_lane_tests).
    fx.fs.router.cache.read_lane_hold.insert(
        &k0,
        bytes::Bytes::from(vec![0xB7u8; BS as usize]),
        64 * 1024 * 1024,
    );
    assert!(fx.fs.router.cache.read_lane_hold.contains(&k0));
    fx.warm_attr(ino).await;

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let serves0 = METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed);
    let ghost0 = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed);
    let adm0 = METRICS.read_tier_admissions.load(Ordering::Relaxed);

    let r = tokio::task::block_in_place(|| session.ring_pread_spin(binding, 4096, 4096));
    assert_eq!(r, 4096);
    let mut got = vec![0u8; 4096];
    session.arena_read_into(0, &mut got);
    assert!(got.iter().all(|&x| x == 0xB7), "planted bytes serve exact");
    assert_eq!(
        METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed) - serves0,
        1,
        "the serve engages the probe gauge regardless of provenance"
    );

    // Settle beat: the ceremony must NEVER run for lane deposits.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        METRICS
            .read_tier_admission_ghost_hits
            .load(Ordering::Relaxed)
            - ghost0,
        0,
        "lane-fetch deposits stay ledger-invisible at the probe site \
         (no ghost touch — the scan-resistance verdict)"
    );
    assert_eq!(
        METRICS.read_tier_admissions.load(Ordering::Relaxed) - adm0,
        0,
        "no admission for invisible serves"
    );
    assert!(
        !fx.tier_has(&k0),
        "no tier publish for an invisible-provenance serve"
    );
    assert!(
        !fx.hot_has(&k0),
        "no hot re-landing for an invisible-provenance serve"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// Contract 4 — a probe miss counts and the op demotes exactly as before
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_probe_miss_counts_and_demote_path_is_preserved() {
    let fx = Fixture::new("mis", *b"il-hold-probe-03", false).await;
    fx.salt_inos(6).await;
    let ino = fx.create_file("probe_miss.bin").await;
    fx.fuse_write(ino, 0, &vec![0xC3u8; BS as usize]).await;
    fx.fuse_write(ino, BS, &vec![0xC4u8; BS as usize]).await;
    fx.make_cold(ino).await;
    // Hold, hot and tier all empty: the probe RUNS and misses.
    fx.warm_attr(ino).await;

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let misses0 = METRICS.ipc_hold_probe_misses.load(Ordering::Relaxed);
    let serves0 = METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed);
    let demote0 = METRICS.ipc_fast_path_miss_demotions.load(Ordering::Relaxed);

    let r = tokio::task::block_in_place(|| session.ring_pread_spin(binding, 4096, 4096));
    assert_eq!(r, 4096, "the miss must still serve through the handoff");
    let mut got = vec![0u8; 4096];
    session.arena_read_into(0, &mut got);
    assert!(got.iter().all(|&x| x == 0xC3), "handoff serve bytes exact");

    assert_eq!(
        METRICS.ipc_hold_probe_misses.load(Ordering::Relaxed) - misses0,
        1,
        "an executed-and-missed probe must count the miss half of the pair"
    );
    assert_eq!(
        METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed) - serves0,
        0
    );
    assert_eq!(
        METRICS.ipc_fast_path_miss_demotions.load(Ordering::Relaxed) - demote0,
        1,
        "the miss demotes to the async handoff exactly as before the probe"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// Contract 5 — SQUEEZEFS_READ_LANE=0 (A0): the probe is structurally inert
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_lane_disabled_makes_the_probe_structurally_inert() {
    let fx = Fixture::new("a0", *b"il-hold-probe-04", true).await;
    fx.salt_inos(8).await;
    let ino = fx.create_file("probe_a0.bin").await;
    fx.fuse_write(ino, 0, &vec![0xD5u8; BS as usize]).await;
    fx.fuse_write(ino, BS, &vec![0xD6u8; BS as usize]).await;
    let map = fx.make_cold(ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();

    // Plant an entry DIRECTLY (the deposit sites are lever-gated, so
    // only a planted entry can prove the PROBE is gated too).
    fx.fs.router.cache.read_lane_hold.insert_demand(
        &k0,
        bytes::Bytes::from(vec![0xD5u8; BS as usize]),
        64 * 1024 * 1024,
    );
    fx.warm_attr(ino).await;

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let serves0 = METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed);
    let misses0 = METRICS.ipc_hold_probe_misses.load(Ordering::Relaxed);
    let ghost0 = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed);

    let r = tokio::task::block_in_place(|| session.ring_pread_spin(binding, 4096, 4096));
    assert_eq!(r, 4096, "A0 rides the pre-probe path end to end");
    let mut got = vec![0u8; 4096];
    session.arena_read_into(0, &mut got);
    assert!(got.iter().all(|&x| x == 0xD5), "device serve bytes exact");

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed) - serves0,
        0,
        "A0: the probe never runs — the serves gauge stays 0"
    );
    assert_eq!(
        METRICS.ipc_hold_probe_misses.load(Ordering::Relaxed) - misses0,
        0,
        "A0: the probe never runs — the misses gauge stays 0"
    );
    assert_eq!(
        METRICS
            .read_tier_admission_ghost_hits
            .load(Ordering::Relaxed)
            - ghost0,
        0,
        "A0: no probe ⇒ no probe-side ceremony (the handler's own ledger \
         behavior under A0 is pinned by read_lane_tests contract 6)"
    );
    assert!(
        fx.fs.router.cache.read_lane_hold.contains(&k0),
        "A0: the planted entry is never consumed by the ring path"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// Contract 6 — sync hot-leg serves credit the hold copy toward
// coverage retirement (memory converges by consumption on il rows)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_hot_leg_serves_credit_the_hold_toward_retirement() {
    let fx = Fixture::new("crd", *b"il-hold-probe-05", false).await;
    fx.salt_inos(10).await;
    let ino = fx.create_file("probe_credit.bin").await;
    fx.fuse_write(ino, 0, &vec![0xE9u8; BS as usize]).await;
    fx.fuse_write(ino, BS, &vec![0xEAu8; BS as usize]).await;
    let map = fx.make_cold(ino).await;
    let k0 = map.get(&0).expect("block 0 mapped").clone();

    // Hot copy present + an INVISIBLE hold copy of the same block (the
    // provenance that never ceremonies — isolates the credit law).
    let d = fx.fuse_read(ino, 0, 64 * 1024).await;
    assert!(d.iter().all(|&x| x == 0xE9));
    assert!(fx.hot_has(&k0), "fixture: fill lands hot probation");
    fx.fs.router.cache.read_lane_hold.purge(&k0);
    fx.fs.router.cache.read_lane_hold.insert(
        &k0,
        bytes::Bytes::from(vec![0xE9u8; BS as usize]),
        64 * 1024 * 1024,
    );
    fx.warm_attr(ino).await;

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let probe_serves0 = METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed);
    let retired0 = METRICS.read_lane_hold_retired.load(Ordering::Relaxed);
    let evicted0 = METRICS
        .read_lane_hold_evicted_unconsumed
        .load(Ordering::Relaxed);

    // Full-coverage ring read of block 0 through the HOT leg (probe
    // order: hot strictly before hold — these must be hot serves).
    let steps = BS / 4096;
    tokio::task::block_in_place(|| {
        for i in 0..steps {
            let r = session.ring_pread_spin(binding, i * 4096, 4096);
            assert_eq!(r, 4096, "hot-leg serve step {i}");
        }
    });

    assert_eq!(
        METRICS.ipc_hold_probe_serves.load(Ordering::Relaxed) - probe_serves0,
        0,
        "probe order: warm hot entries serve BEFORE the hold leg (the \
         governor's payback basis keeps funding from hot serves)"
    );
    assert_eq!(
        METRICS.read_lane_hold_retired.load(Ordering::Relaxed) - retired0,
        1,
        "full hot-leg coverage must credit-and-RETIRE the hold copy — \
         every consumption path credits consumed bytes"
    );
    assert!(
        !fx.fs.router.cache.read_lane_hold.contains(&k0),
        "the retired entry is gone (memory converges by consumption)"
    );
    assert_eq!(
        METRICS
            .read_lane_hold_evicted_unconsumed
            .load(Ordering::Relaxed)
            - evicted0,
        0,
        "retirement-by-credit is not an unconsumed eviction"
    );
    fx.shutdown();
}
