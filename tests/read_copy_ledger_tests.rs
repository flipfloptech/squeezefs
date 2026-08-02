//! The READ copy ledger — per-site byte accounting for every daemon CPU
//! pass over read payload bytes (read-copy-count campaign, 2026-08-02;
//! the read twin of the write path's copy census,
//! `.benchmarks/2026-07-31-near-zero-copy.md`).
//!
//! Contracts pinned (red-first):
//! 1. **Closed accounting on the dest-armed transport shape** (the
//!    kernel path): every byte served with a payload dest is attributed
//!    to exactly one of `read_copy_dest_bytes` (the lawful serve copy),
//!    `read_dest_dma_bytes` (device DMA straight into the dest — the
//!    zero-daemon-copy legs), never both; `read_fill_dma_bytes` prices
//!    the pooled fill DMA the nvme-tcp RX copy rides.
//! 2. **Zero-copy cold slice (E-IL1)**: a cold whole-block-path serve
//!    WITHOUT a dest (the il handoff shape) returns a refcount slice of
//!    the pooled fill — `read_copy_bounce_bytes` stays 0 and the reply
//!    bytes point INTO the returned backing (pre-fix: a 1 MiB-class
//!    `Bytes::copy_from_slice` alloc+copy per cold il read).
//! 3. **Arena-dest il cold serves (E-IL2)**: a miss-demoted ring read
//!    serves IN PLACE into the validated arena window
//!    (`ipc_read_dest_serves` engagement; `ipc_arena_copy_bytes` 0 for
//!    the op — the reply's intermediate `payload.write` copy is elided;
//!    `SQUEEZEFS_IL_READ_DEST=0` is the A/B lever).
//!
//! Counter-asserting phases share one test fn per fixture (the churn
//! suite's counter-isolation discipline — the ledger counters are
//! process-global).

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
use squeezefs::routing::{DataRouter, ReadClassHint};
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

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(block_size: &str, uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

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

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

/// Deterministic ground-truth byte for global file offset `off`.
fn pat(off: u64) -> u8 {
    ((off % 251) as u8) ^ (((off / 4096) % 7) as u8)
}

async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        write_at(h, ino, off, &data).await;
        off += chunk as u64;
    }
}

async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

/// 4 KiB-aligned scratch destination (the registered-payload stand-in).
struct AlignedDest {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedDest {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        // SAFETY: non-zero size, valid layout.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }
    fn addr(&self) -> u64 {
        self.ptr as u64
    }
    fn slice(&self, len: usize) -> &[u8] {
        assert!(len <= self.layout.size());
        // SAFETY: within the allocation; initialized (zeroed at birth,
        // then written by the serve under test).
        unsafe { std::slice::from_raw_parts(self.ptr, len) }
    }
}

impl Drop for AlignedDest {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout above.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

struct LedgerSnap {
    dest: u64,
    bounce: u64,
    dest_dma: u64,
    fill_dma: u64,
}

fn snap() -> LedgerSnap {
    LedgerSnap {
        dest: METRICS.read_copy_dest_bytes.load(Ordering::Relaxed),
        bounce: METRICS.read_copy_bounce_bytes.load(Ordering::Relaxed),
        dest_dma: METRICS.read_dest_dma_bytes.load(Ordering::Relaxed),
        fill_dma: METRICS.read_fill_dma_bytes.load(Ordering::Relaxed),
    }
}

fn delta(s0: &LedgerSnap) -> LedgerSnap {
    let s1 = snap();
    LedgerSnap {
        dest: s1.dest - s0.dest,
        bounce: s1.bounce - s0.bounce,
        dest_dma: s1.dest_dma - s0.dest_dma,
        fill_dma: s1.fill_dma - s0.fill_dma,
    }
}

/// Contracts 1 + 2: the per-site ledger closes on every serve shape of
/// the single-block striped arm, and the cold None-dest slice is
/// zero-copy (E-IL1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ledger_closes_and_cold_none_dest_slice_is_zero_copy() {
    let h = make_with("524288", *b"rcledger-v1-2026", "rcl_ns_a").await;
    // Shift ino allocation so this test never contends on the
    // process-global DLM lock map with the il fixture's `inode_N` keys
    // (the op-economy salt_inos discipline).
    for i in 0..8 {
        let _ = create(&h, &format!("salt{i}")).await;
    }
    let ino = create(&h, "rcl_a").await;
    write_pattern(&h, ino, 12 * BS).await;
    let _map = make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let part = 384 * 1024usize; // > ranged threshold (256 KiB), < BS

    // ---- Phase A (E-IL1, the il handoff shape): cold whole-block-path
    // partial serve WITHOUT a dest — one pooled fill DMA, ZERO serve
    // copies (the reply is a refcount slice of the fill backing).
    let s0 = snap();
    let reply =
        h.fs.read(h.req, ino, 0, BS, part as u32, 0)
            .await
            .expect("phase A read");
    assert_eq!(reply.data.len(), part);
    assert!(
        reply
            .data
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(BS + j as u64)),
        "phase A content"
    );
    let d = delta(&s0);
    assert_eq!(d.fill_dma, BS, "phase A: exactly one whole-block fill DMA");
    assert_eq!(
        d.bounce, 0,
        "phase A: cold None-dest serve must be a zero-copy slice of the \
         fill backing (E-IL1) — a bounce here is the il cold alloc+copy"
    );
    assert_eq!(d.dest, 0, "phase A: no dest was offered");
    assert_eq!(d.dest_dma, 0, "phase A: no dest was offered");
    // The zero-copy proof itself: the reply bytes point INTO the returned
    // backing (pre-fix they were a fresh heap copy).
    let backing = reply
        .backing
        .as_ref()
        .expect("cold serve carries its backing")
        .clone();
    let block = backing
        .downcast_ref::<squeezefs::cache::pool::ReadBlockValue>()
        .expect("backing is the block value");
    let backing_range =
        block.as_ref().as_ptr() as usize..block.as_ref().as_ptr() as usize + block.len();
    let data_ptr = reply.data.as_ptr() as usize;
    assert!(
        backing_range.contains(&data_ptr),
        "phase A: reply bytes must alias the fill backing (zero-copy slice), \
         got data ptr {data_ptr:#x} outside backing {backing_range:x?}"
    );
    drop(reply);

    // ---- Phase B (the kernel-path warm shape): hot-tier serve INTO a
    // dest — exactly one lawful serve copy, counted.
    let dest = AlignedDest::new(BS as usize);
    let s0 = snap();
    let hot0 = METRICS.hot_block_hits.load(Ordering::Relaxed);
    let nt0 = METRICS.nt_read_serve_bytes.load(Ordering::Relaxed);
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                BS,
                part as u32,
                Some(dest.addr()),
                ReadClassHint::default(),
                None,
            )
            .await
            .expect("phase B read");
    assert!(
        METRICS.hot_block_hits.load(Ordering::Relaxed) > hot0,
        "phase B must serve from the hot tier (phase A's fill deposited it)"
    );
    assert_eq!(data.len(), part);
    assert_eq!(
        data.as_ptr(),
        dest.ptr as *const u8,
        "phase B serves in place"
    );
    assert!(
        dest.slice(part)
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(BS + j as u64)),
        "phase B content (in the dest)"
    );
    let d = delta(&s0);
    assert_eq!(
        d.dest, part as u64,
        "phase B: the hot serve copy into the dest is counted"
    );
    assert_eq!(d.bounce, 0, "phase B: no intermediate copy");
    assert_eq!(d.fill_dma, 0, "phase B: warm — no device fetch");
    assert_eq!(d.dest_dma, 0, "phase B: warm — no dest DMA");
    // NT default-ON engagement (2026-08-02 brackets): a ≥-floor serve
    // into a NON-arena dest runs the NT body and feeds the gauge.
    #[cfg(target_arch = "x86_64")]
    assert_eq!(
        METRICS.nt_read_serve_bytes.load(Ordering::Relaxed) - nt0,
        part as u64,
        "phase B: default-ON NT engagement on the registered-dest serve"
    );
    drop(data);

    // ---- Phase C (the zero-daemon-copy cold leg): full-block read with
    // an aligned dest — raw device DMA straight into the dest.
    let s0 = snap();
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                2 * BS,
                BS as u32,
                Some(dest.addr()),
                ReadClassHint::default(),
                None,
            )
            .await
            .expect("phase C read");
    assert_eq!(data.len(), BS as usize);
    assert!(
        dest.slice(BS as usize)
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(2 * BS + j as u64)),
        "phase C content"
    );
    let d = delta(&s0);
    assert_eq!(
        d.dest_dma, BS,
        "phase C: the raw full-block leg DMAs into the dest (zero daemon copies)"
    );
    assert_eq!(d.dest, 0, "phase C: no serve copy");
    assert_eq!(d.bounce, 0, "phase C: no bounce");
    assert_eq!(d.fill_dma, 0, "phase C: no pooled fill");
    drop(data);

    // ---- Phase D (the cold kernel-path partial shape — the EXA row's
    // dominant serve): pooled fill + one serve copy into the dest.
    let s0 = snap();
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                3 * BS,
                part as u32,
                Some(dest.addr()),
                ReadClassHint::default(),
                None,
            )
            .await
            .expect("phase D read");
    assert_eq!(data.len(), part);
    assert!(
        dest.slice(part)
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(3 * BS + j as u64)),
        "phase D content"
    );
    let d = delta(&s0);
    assert_eq!(d.fill_dma, BS, "phase D: one whole-block pooled fill");
    assert_eq!(
        d.dest, part as u64,
        "phase D: one serve copy (fill → dest) — the EXA cold slice_out"
    );
    assert_eq!(d.bounce, 0, "phase D: no intermediate copy");
    assert_eq!(
        d.dest_dma, 0,
        "phase D: partial slice — raw dest leg ineligible"
    );
    drop(data);

    // ---- Phase E (the ranged zero-copy leg): 4 KiB aligned first touch
    // with a dest — window DMA straight into the dest.
    let s0 = snap();
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                0,
                4096,
                Some(dest.addr()),
                ReadClassHint::default(),
                None,
            )
            .await
            .expect("phase E read");
    assert_eq!(data.len(), 4096);
    assert!(
        dest.slice(4096)
            .iter()
            .enumerate()
            .all(|(j, &x)| x == pat(j as u64)),
        "phase E content"
    );
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        1,
        "phase E rides the ranged primitive"
    );
    let d = delta(&s0);
    assert_eq!(
        d.dest_dma, 4096,
        "phase E: the ranged zero-copy leg DMAs the window into the dest"
    );
    assert_eq!(d.dest, 0, "phase E: no serve copy");
    assert_eq!(d.bounce, 0, "phase E: no bounce");
    assert_eq!(d.fill_dma, 0, "phase E: no pooled fill");
    drop(data);
}

// ---------------------------------------------------------------------------
// Contract 3 fixture: the raw-protocol ring client over a real IpcHost
// (the ipc_op_economy_tests harness shape).
// ---------------------------------------------------------------------------

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
        ring_entries: 4,
        slots: 4,
        arena_bytes: 4 * 1024 * 1024,
        max_op_bytes: 1024 * 1024,
        _pad: 0,
    }
}

struct Fixture {
    fs: SqueezefsFilesystem,
    host: Arc<IpcHost>,
    cfg: IpcHostConfig,
    sink: Arc<InoMapSink>,
    _backing: NamedTempFile,
    _meta: NamedTempFile,
    _staging: TempDir,
}

impl Fixture {
    async fn new(name: &str) -> Fixture {
        let h = make_with("524288", *b"rcledger-il-2026", &format!("rcl_il_{name}")).await;
        let H {
            fs,
            req: _,
            _b,
            _m,
            _s,
        } = h;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone()),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-rcl-{}-{}", std::process::id(), name),
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
            _backing: _b,
            _meta: _m,
            _staging: _s.expect("staging dir"),
        }
    }
}

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

/// Contract 3 (E-IL2): a cold miss-demoted ring read serves IN PLACE
/// into the validated arena window — the intermediate reply bounce AND
/// the `payload.write` arena copy are both elided; the ledger shows one
/// pooled fill + one dest copy (into the arena) and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn il_cold_read_serves_in_place_into_the_arena_window() {
    let fx = Fixture::new("dest").await;
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
    };

    // Striped cold fixture (12 × BS pattern, fsync, purge).
    let ino = {
        let create = fx
            .fs
            .create(req, 1, OsStr::new("il_cold.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create")
            .attr
            .ino;
        let mut off = 0u64;
        while off < 12 * BS {
            let chunk = std::cmp::min(BS, 12 * BS - off) as usize;
            let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
            let w = fx
                .fs
                .write(
                    req,
                    create,
                    0,
                    off,
                    bytes::Bytes::copy_from_slice(&data),
                    0,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(w.written as usize, chunk);
            off += chunk as u64;
        }
        fx.fs.fsync(req, create, 0, false).await.unwrap();
        let path = squeezefs::keys::inode_path(create);
        let map = fx
            .fs
            .router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
        assert!(!map.is_empty(), "fixture must promote to striped");
        for key in map.values() {
            fx.fs.router.cache.purge_block_key(key);
        }
        create
    };

    let dir = tempdir().unwrap();
    let fd = buffered_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let part = 384 * 1024u32; // > ranged threshold, < BS: the whole-block cold shape
    let s0 = snap();
    let arena0 = METRICS.ipc_arena_copy_bytes.load(Ordering::Relaxed);
    let dest_serves0 = METRICS.ipc_read_dest_serves.load(Ordering::Relaxed);
    let handoffs0 = METRICS.ipc_async_handoffs.load(Ordering::Relaxed);
    let nt0 = METRICS.nt_read_serve_bytes.load(Ordering::Relaxed);

    let r = tokio::task::block_in_place(|| session.ring_pread_spin(binding, BS, part));
    assert_eq!(r, i64::from(part), "cold ring read serves full length");

    assert_eq!(
        METRICS.ipc_async_handoffs.load(Ordering::Relaxed) - handoffs0,
        1,
        "the cold read must have taken the miss-demotion handoff (the path under test)"
    );

    // Content: the arena window holds the pattern.
    let mut out = vec![0u8; part as usize];
    session.arena_read_into(0, &mut out);
    assert!(
        out.iter()
            .enumerate()
            .all(|(j, &x)| x == pat(BS + j as u64)),
        "arena window content after the cold serve"
    );

    // E-IL2 engagement: served in place, no arena bounce copy.
    assert_eq!(
        METRICS.ipc_read_dest_serves.load(Ordering::Relaxed) - dest_serves0,
        1,
        "the cold il read must serve IN PLACE into the arena window \
         (ipc_read_dest_serves is the E-IL2 engagement gauge)"
    );
    assert_eq!(
        METRICS.ipc_arena_copy_bytes.load(Ordering::Relaxed) - arena0,
        0,
        "an in-place serve elides the payload.write arena copy"
    );
    let d = delta(&s0);
    assert_eq!(d.fill_dma, BS, "one whole-block pooled fill");
    assert_eq!(
        d.dest,
        u64::from(part),
        "one serve copy, fill → arena dest (the lawful il serve copy)"
    );
    assert_eq!(d.bounce, 0, "no intermediate reply bounce (E-IL1 + E-IL2)");
    // The ARENA-dest NT exemption (2026-08-02 brackets: NT into the
    // client-visible arena LOSES — the client's slab_read consumes the
    // lines within ~one op): even under the default-ON policy, an
    // arena-dest serve stays cached.
    assert_eq!(
        METRICS.nt_read_serve_bytes.load(Ordering::Relaxed) - nt0,
        0,
        "arena-dest serves are NT-exempt (structural, not policy)"
    );

    fx.host.shutdown();
}
