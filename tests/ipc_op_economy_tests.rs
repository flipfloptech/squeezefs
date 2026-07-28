//! IPC **op economy** — the 2026-07-28 campaign's contract + profiling
//! surface (levers: prelude allocations, completion-reap wake economy).
//!
//! ## Lever 1 contract (warm-serve prelude allocations)
//!
//! The warm §5.5.1 sync fast path serves at a ~1 µs/op budget (measured
//! 1.02 M IOPS warm, G-L4-2); P2 profiling convicted ~5+ heap
//! allocations in the daemon-side per-op prelude (decode → dispatch →
//! probe → response framing). The contract here pins the fix: a warm
//! fast-path serve performs (amortized) **zero** heap allocations
//! process-wide — measured with a counting global allocator around a
//! quiesced, engagement-verified warm ring-read window. The bound is
//! `≤ 1 alloc per 100 ops` (not literal 0) so rare moka housekeeping
//! inside `get` cannot flake the gate; the pre-fix path measures ~10
//! allocs/op, three orders of magnitude past the bound.
//!
//! `SQZ_ALLOC_TRACE=1 cargo test --test ipc_op_economy_tests -- --nocapture`
//! flips the same harness into the **profiler**: every allocation inside
//! a short traced window captures a backtrace, printed as a deduped
//! site table (the campaign's alloc-site evidence instrument).
//!
//! ## Lever 2 contract (completion-side wake economy)
//!
//! `SlotCompletion::complete` must issue completion wakes only toward
//! parked reapers (the cqe-doorbell protocol): an unparked-client
//! completion stream elides every cqe wake (`ipc_cqe_wake_elided`
//! grows, `ipc_cqe_wake_writes` does not), and a parked reaper is woken
//! promptly by the first completion (`ipc_cqe_wake_writes` moves). The
//! sync per-slot WAITER wake path is unchanged (pinned by the existing
//! parity/lifecycle suites).
//!
//! Harness: the preload_parity_tests raw-protocol client (no kernel in
//! the loop; the daemon side is the REAL host → service thread → sink
//! path).

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::CtlMsg;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// counting global allocator (the profiling instrument)
// ---------------------------------------------------------------------------

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
/// When set, every allocation captures a backtrace into [`TRACE_LOG`]
/// (recursion-guarded per thread).
static TRACE: AtomicBool = AtomicBool::new(false);
static TRACE_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    static IN_TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct CountingAlloc;

impl CountingAlloc {
    fn record(&self, layout: Layout) {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        if TRACE.load(Ordering::Relaxed) {
            IN_TRACE.with(|flag| {
                if !flag.get() {
                    flag.set(true);
                    let bt = std::backtrace::Backtrace::force_capture();
                    if let Ok(mut log) = TRACE_LOG.lock() {
                        log.push(format!("[{} B]\n{bt}", layout.size()));
                    }
                    flag.set(false);
                }
            });
        }
    }
}

// SAFETY: delegates verbatim to `System`; the accounting side effects are
// atomic counters plus a recursion-guarded trace hook.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn allocs_now() -> u64 {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// fixture (the preload_parity_tests shape)
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
        BlockAllocator::new(dlm.meta_client().clone(), "ipc_op_economy_tests")
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
    fs: squeezefs::fuse_client::SqueezefsFilesystem,
    host: Arc<IpcHost>,
    cfg: IpcHostConfig,
    sink: Arc<InoMapSink>,
    _backing: tempfile::NamedTempFile,
    _meta: tempfile::NamedTempFile,
    _staging: tempfile::TempDir,
}

impl Fixture {
    async fn new(name: &str) -> Fixture {
        let (fs, backing, meta, staging) = sandbox_fs().await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone()),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-oe-{}-{}", std::process::id(), name),
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
            _backing: backing,
            _meta: meta,
            _staging: staging,
        }
    }

    /// Shift ino allocation so parallel tests never contend on the
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
        let create = self
            .fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create");
        create.attr.ino
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
        assert_eq!(w.written as usize, data.len());
    }

    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        let reply = self
            .fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read");
        reply.data.to_vec()
    }

    fn shutdown(&self) {
        self.host.shutdown();
    }
}

/// Buffered stand-in fd on a real filesystem, with the host's expected
/// st_dev re-pointed and the ino translation registered.
fn buffered_standin(fx: &Fixture, dir: &tempfile::TempDir, name: &str, fs_ino: u64) -> OwnedFd {
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

    /// One allocation-free warm ring pread on slot 0: claim → publish →
    /// push → doorbell → bounded spin. Panics on failure/timeouts (this
    /// is the measured hot loop — no result plumbing).
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
            assert!(Instant::now() < deadline, "warm ring read never completed");
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

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 17) % 251) as u8)
        .collect()
}

// ---------------------------------------------------------------------------
// lever 1: warm-serve prelude allocations
// ---------------------------------------------------------------------------

/// Drive `ops` warm 4 KiB ring reads on slot 0 and return
/// `(alloc delta, fast-path serve delta)`. The caller must have warmed
/// the shape (attr cache + sync-servable tier) already.
fn measure_warm_window(session: &ClientSession, binding: u64, offset: u64, ops: u64) -> (u64, u64) {
    let serves0 = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed);
    let allocs0 = allocs_now();
    for _ in 0..ops {
        let r = session.ring_pread_spin(binding, offset, 4096);
        assert_eq!(r, 4096, "warm read must serve full length");
    }
    let allocs = allocs_now() - allocs0;
    let serves = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed) - serves0;
    (allocs, serves)
}

/// The lever-1 contract: an engagement-verified warm fast-path window is
/// (amortized) allocation-free — ≤ 1 heap allocation per 100 ops across
/// the whole process. Pre-fix this measures ~10 allocs/op (the convicted
/// prelude: double metadata-cache clone, heap key formatting, staging-key
/// `Bytes` mint, `StagedMetadata` path String, payload `Bytes` bounce).
///
/// With `SQZ_ALLOC_TRACE=1` (+ `--nocapture`) a short traced window
/// prints the deduped alloc-site backtrace table instead of asserting —
/// the campaign's profiling instrument.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_fast_path_serves_are_allocation_free() {
    let fx = Fixture::new("warmalloc").await;
    fx.salt_inos(3).await;

    // A STAGED-layout file: the staging-mmap sync serve leg (leg 1) —
    // the warm shape the G-L4-2 row exercises hardest.
    let ino = fx.create_file("warm.bin").await;
    let payload = deterministic_bytes(1024 * 1024, 7);
    fx.fuse_write(ino, 0, &payload).await;
    let meta = fx
        .fs
        .router
        .metadata_cache
        .get(&ino)
        .expect("metadata cache entry after write");
    assert_eq!(meta.file_type, "staged", "fixture file must be staged");

    let dir = tempfile::tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Warm-up: the first ring read may demote once (cold attr cache);
    // the handoff re-seeds it. Loop until a window of ops is fully
    // fast-path-served (engagement == ops).
    let warm = tokio::task::block_in_place(|| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, serves) = measure_warm_window(&session, binding, 8192, 64);
            if serves == 64 {
                return true;
            }
            assert!(
                Instant::now() < deadline,
                "warm shape never reached 100 % fast-path engagement"
            );
        }
    });
    assert!(warm);

    if std::env::var("SQZ_ALLOC_TRACE").as_deref() == Ok("1") {
        // Profiler mode: trace a short window, print the site table.
        tokio::task::block_in_place(|| {
            TRACE.store(true, Ordering::SeqCst);
            let (allocs, serves) = measure_warm_window(&session, binding, 8192, 32);
            TRACE.store(false, Ordering::SeqCst);
            println!("traced window: {allocs} allocs / {serves} warm serves (32 ops)");
            print_site_table();
        });
        fx.shutdown();
        return;
    }

    const OPS: u64 = 5_000;
    let (allocs, serves) = tokio::task::block_in_place(|| {
        // Settle: give idle housekeeping (moka, tokio timers) a beat so
        // the measured window is the serve path, not startup residue.
        std::thread::sleep(Duration::from_millis(100));
        measure_warm_window(&session, binding, 8192, OPS)
    });
    // Engagement (charter rule 4): every measured op must be a warm
    // fast-path serve, or the window measured the wrong path.
    assert_eq!(
        serves, OPS,
        "measurement invalid: {serves}/{OPS} ops were fast-path serves"
    );
    let per_100 = allocs * 100 / OPS;
    assert!(
        per_100 <= 1,
        "warm fast-path serve prelude allocates: {allocs} allocs / {OPS} ops \
         (~{}.{:02} per op) — the op-economy contract is ≤ 1 alloc per 100 ops",
        allocs / OPS,
        (allocs * 100 / OPS) % 100,
    );
    fx.shutdown();
}

fn print_site_table() {
    let log = TRACE_LOG.lock().expect("trace log mutex");
    // Dedup by the first in-crate frame lines (skip the allocator's own).
    let mut sites: HashMap<String, u64> = HashMap::new();
    for entry in log.iter() {
        let key: String = entry
            .lines()
            .filter(|l| {
                (l.contains("squeezefs") || l.contains("fuse3"))
                    && !l.contains("ipc_op_economy_tests")
                    && !l.contains("CountingAlloc")
            })
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        *sites.entry(key).or_insert(0) += 1;
    }
    let mut rows: Vec<(u64, String)> = sites.into_iter().map(|(k, v)| (v, k)).collect();
    rows.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    println!("== alloc-site table (count × site, top frames) ==");
    for (count, site) in rows {
        println!("--- {count} allocs ---\n{site}\n");
    }
}

// ---------------------------------------------------------------------------
// lever 2: completion-side wake economy (the cqe doorbell)
// ---------------------------------------------------------------------------

/// Unparked completions elide every cqe wake: a spin-waiting client's
/// stream of warm serves moves `ipc_cqe_wake_elided`, never
/// `ipc_cqe_wake_writes` (the daemon must not pay a wake syscall toward
/// a reaper that is not parked).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unparked_completions_elide_cqe_wakes() {
    let fx = Fixture::new("cqeelide").await;
    fx.salt_inos(5).await;
    let ino = fx.create_file("cqe.bin").await;
    let payload = deterministic_bytes(256 * 1024, 9);
    fx.fuse_write(ino, 0, &payload).await;

    let dir = tempfile::tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Warm one op (may demote once).
    let _ = fx.fuse_read(ino, 0, 4096).await;
    tokio::task::block_in_place(|| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let serves0 = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed);
            let r = session.ring_pread_spin(binding, 4096, 4096);
            assert_eq!(r, 4096);
            if METRICS.ipc_fast_path_serves.load(Ordering::Relaxed) > serves0 {
                break;
            }
            assert!(Instant::now() < deadline, "warm shape never engaged");
        }
    });

    let writes0 = METRICS.ipc_cqe_wake_writes.load(Ordering::Relaxed);
    let elided0 = METRICS.ipc_cqe_wake_elided.load(Ordering::Relaxed);
    tokio::task::block_in_place(|| {
        for _ in 0..256 {
            let r = session.ring_pread_spin(binding, 4096, 4096);
            assert_eq!(r, 4096);
        }
    });
    let writes = METRICS.ipc_cqe_wake_writes.load(Ordering::Relaxed) - writes0;
    let elided = METRICS.ipc_cqe_wake_elided.load(Ordering::Relaxed) - elided0;
    assert_eq!(
        writes, 0,
        "no reaper was ever parked — every completion's cqe wake must elide \
         (writes {writes}, elided {elided})"
    );
    assert!(
        elided >= 256,
        "the elision counter must account for the unparked completions \
         (elided {elided} < 256)"
    );
    fx.shutdown();
}

/// A parked reaper is woken promptly by the first completion: the
/// client parks on the session's cqe doorbell (reaper_parked + seq
/// snapshot, the disarm→scan law), a completion bumps the seq and pays
/// exactly one wake (`ipc_cqe_wake_writes` moves), and the parked
/// thread returns well inside the 5 s bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_reaper_is_woken_by_completion_cqe_wake() {
    let fx = Fixture::new("cqewake").await;
    fx.salt_inos(7).await;
    let ino = fx.create_file("cqew.bin").await;
    let payload = deterministic_bytes(256 * 1024, 11);
    fx.fuse_write(ino, 0, &payload).await;

    let dir = tempfile::tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    // Warm (first op may demote).
    tokio::task::block_in_place(|| {
        let r = session.ring_pread_spin(binding, 0, 4096);
        assert_eq!(r, 4096);
    });

    let writes0 = METRICS.ipc_cqe_wake_writes.load(Ordering::Relaxed);
    let woke_in = tokio::task::block_in_place(|| {
        // Reaper protocol, client side: submit WITHOUT spinning on the
        // slot, then park on the cqe doorbell.
        let header = session.header();
        let slot = session.slot(0);
        let gen = slot.core.try_claim().expect("slot 0 FREE");
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding,
            offset: 0,
            len: 4096,
            arena_off: 0,
        });
        // Park intent FIRST (the daemon observes it before serving),
        // then snapshot (park_begin = register-then-snapshot), then
        // publish the op.
        let seq0 = header.cqe.park_begin();
        slot.core.publish_submitted();
        assert!(session.ring().push(0), "ring must accept");
        header.doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&header.doorbell, 1);
        // Bounded futex wait on the cqe word (no slot spin): a lost wake
        // strands this for the full 5 s bound and fails the ≤ 2 s assert.
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(5);
        while header.cqe.seq() == seq0 {
            assert!(
                Instant::now() < deadline,
                "parked reaper never woken by the completion"
            );
            squeezefs::ipc_host::futex_wait_for_test(
                header.cqe.seq_word(),
                seq0,
                Duration::from_secs(1),
            );
        }
        header.cqe.park_end();
        // Consume the completion.
        let cdl = Instant::now() + Duration::from_secs(5);
        while !slot.core.is_done_for(gen) {
            assert!(Instant::now() < cdl, "op never completed");
            std::hint::spin_loop();
        }
        let r = slot.result();
        slot.core.release();
        assert_eq!(r, 4096);
        t0.elapsed()
    });
    assert!(
        woke_in < Duration::from_secs(2),
        "parked reaper took {woke_in:?} to observe the completion — the cqe \
         wake path must be prompt, not timeout-bounded"
    );
    assert!(
        METRICS.ipc_cqe_wake_writes.load(Ordering::Relaxed) > writes0,
        "a completion toward a parked reaper must pay (and count) a cqe wake"
    );
    fx.shutdown();

    // Sanity: read back through FUSE so the fixture file is coherent.
    let mut expect = vec![0u8; 4096];
    expect.copy_from_slice(&payload[..4096]);
    let mut got = vec![0u8; 4096];
    session.arena_read_into(0, &mut got);
    assert_eq!(got, expect, "parked-reap read served the right bytes");
}
