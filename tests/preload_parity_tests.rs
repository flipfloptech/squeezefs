//! PR L4-4 — IPC data plane: byte parity, cross-transport RYW, durability
//! ordering, arena-lease severance, adversarial mid-serve mutation, and the
//! §5.5.1 demote rule (`docs/design-preload-interception.md` §5.3.1,
//! §5.5.1, §5.5.2, §5.6, §8).
//!
//! The client here is a **protocol-speaking harness** (the L4-5 shim does
//! not exist yet): raw ctl socket + mapped session shm, driving the REAL
//! host → service → sink → daemon-handler path end to end. The kernel is
//! not in the loop, so the "kernel FUSE" side of every parity row is the
//! daemon's own FUSE handler invoked directly (`fuse3::raw::Filesystem`)
//! — which is exactly the state kernel requests reach, and under KD-11
//! (interception forces kernel write-through, pinned by
//! `kd11_interception_forces_write_through_and_refuses_explicit_writeback`
//! in tests/ipc_host_tests.rs) a buffered kernel `write(2)` reaches that
//! handler *synchronously before acking* — so the FUSE-write → ring-read
//! rows here pin the §5.6.2 buffered-kernel-write → ring-read direction
//! at the daemon boundary. The mount-level leg (real kernel, real shim)
//! joins the fstests/LTP tiers at PR L4-5.
//!
//! Ino translation: production bindings take their ino from `fstat` of a
//! kernel-granted fd ON THE MOUNT (§5.2). The harness has no mount, so
//! its bound fds are tempdir files whose `st_ino` differ from the fixture
//! filesystem's inos; [`InoMapSink`] translates st_ino → fs-ino at the
//! sink boundary — a test seam wrapping the production
//! [`DataPlaneSink`], never a production code path.

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ, OP_WRITE,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::CtlMsg;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// fixture: sandbox fs + host + service sink + raw-protocol client
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
        BlockAllocator::new(dlm.meta_client().clone(), "preload_parity_tests")
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

/// The harness ino-translation sink (module docs): wraps the PRODUCTION
/// [`DataPlaneSink`], rewriting each op's binding ino from the bound
/// tempdir file's `st_ino` to the fixture filesystem's ino.
struct InoMapSink {
    inner: DataPlaneSink,
    map: std::sync::Mutex<HashMap<u64, u64>>,
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
            map: std::sync::Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-parity-{}-{}", std::process::id(), name),
            build_commit: "a".repeat(40),
            allow_dev: false,
            geometry: test_geometry(),
            arena_cap_bytes: 64 * 1024 * 1024,
            per_uid_session_cap: 8,
        };
        let host = IpcHost::spawn(cfg.clone(), sink.clone()).expect("host must spawn");
        let dir = tempfile::tempdir().expect("tempdir");
        let st_dev = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(dir.path()).expect("metadata").dev()
        };
        host.set_expected_st_dev(st_dev);
        Fixture {
            fs,
            host,
            cfg,
            sink,
            dir,
            _backing: backing,
            _meta: meta,
            _staging: staging,
        }
    }

    /// Create a file in the fixture fs AND a same-name tempdir stand-in
    /// whose fd the harness binds; registers the st_ino → fs-ino mapping.
    async fn create_file(&self, name: &str) -> (u64, OwnedFd) {
        let create = self
            .fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create");
        let fs_ino = create.attr.ino;
        let path = self.dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create stand-in");
        f.write_all(&[0u8; 16]).expect("stand-in bytes");
        drop(f);
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

    /// FUSE-side read via the daemon's own handler (the kernel's target).
    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        let reply = self
            .fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read");
        reply.data.to_vec()
    }

    /// FUSE-side write via the daemon's own handler. Under KD-11 this is
    /// exactly where a buffered kernel `write(2)` lands synchronously
    /// before acking (module docs).
    async fn fuse_write(&self, ino: u64, offset: u64, data: &[u8]) -> u32 {
        let reply = self
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
        reply.written
    }

    fn shutdown(&self) {
        self.host.shutdown();
    }
}

// ---------------------------------------------------------------------------
// raw-protocol client (mapped session + multi-slot submit)
// ---------------------------------------------------------------------------

struct ClientSession {
    base: *mut u8,
    layout: SessionLayout,
    geometry: Geometry,
    _sock: UnixStream,
}

// SAFETY: the harness drives ops from one test thread; the mapping is
// plain shared memory (same-process daemon side uses atomics).
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

        // BIND the same fd for the data plane.
        send_ctl(&session._sock, &CtlMsg::Bind, Some(fd.as_raw_fd())).expect("send BIND");
        let (reply, none) = recv_ctl(&session._sock).expect("recv BIND reply");
        assert!(none.is_none());
        let binding = match reply {
            CtlMsg::BindOk {
                binding_id,
                read_ok,
                write_ok,
                ..
            } => {
                assert!(read_ok && write_ok, "O_RDWR grants both directions");
                binding_id
            }
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

    fn arena_ptr(&self, off: u64, len: usize) -> *mut u8 {
        assert!(off + len as u64 <= self.geometry.arena_bytes);
        // SAFETY: bounds asserted against the arena region.
        unsafe { self.base.add((self.layout.arena_off + off) as usize) }
    }

    fn arena_write(&self, off: u64, data: &[u8]) {
        let dst = self.arena_ptr(off, data.len() as u64 as usize);
        // SAFETY: bounds checked by arena_ptr; harness is the only writer
        // on this side.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
    }

    fn arena_read(&self, off: u64, len: usize) -> Vec<u8> {
        let src = self.arena_ptr(off, len);
        let mut out = vec![0u8; len];
        // SAFETY: bounds checked by arena_ptr.
        unsafe { std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len) };
        out
    }

    fn arena_fill(&self, off: u64, len: usize, byte: u8) {
        let dst = self.arena_ptr(off, len);
        // SAFETY: bounds checked by arena_ptr.
        unsafe { std::ptr::write_bytes(dst, byte, len) };
    }

    /// Claim + publish + ring-push one op on `slot_idx`; returns the claim
    /// generation for the wait.
    fn submit_on(&self, slot_idx: u32, d: &SlotDescriptor) -> u64 {
        let slot = self.slot(slot_idx);
        let gen = slot.core.try_claim().expect("slot must be FREE");
        slot.publish_descriptor(d);
        slot.core.publish_submitted();
        assert!(self.ring().push(slot_idx), "ring must accept");
        self.header().doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&self.header().doorbell, 1);
        gen
    }

    fn wait_done(&self, slot_idx: u32, gen: u64, what: &str) -> i64 {
        let slot = self.slot(slot_idx);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !slot.core.is_done_for(gen) {
            assert!(Instant::now() < deadline, "{what}: op never completed");
            std::hint::spin_loop();
        }
        let result = slot.result();
        slot.core.release();
        result
    }

    fn is_done(&self, slot_idx: u32, gen: u64) -> bool {
        self.slot(slot_idx).core.is_done_for(gen)
    }

    fn submit_wait(&self, d: &SlotDescriptor, what: &str) -> i64 {
        let gen = self.submit_on(0, d);
        self.wait_done(0, gen, what)
    }

    /// Ring pread into a fresh Vec (chunks ≤ max_op_bytes).
    fn ring_read(&self, binding: u64, offset: u64, len: usize, what: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut off = offset;
        let mut remaining = len;
        while remaining > 0 {
            let chunk = remaining.min(self.geometry.max_op_bytes as usize);
            let r = self.submit_wait(
                &SlotDescriptor {
                    op: OP_READ,
                    flags: 0,
                    binding,
                    offset: off,
                    len: chunk as u32,
                    arena_off: 0,
                },
                what,
            );
            assert!(r >= 0, "{what}: ring read failed with {r}");
            let n = r as usize;
            out.extend_from_slice(&self.arena_read(0, n));
            if n < chunk {
                break; // EOF short read
            }
            off += n as u64;
            remaining -= n;
        }
        out
    }

    /// Ring pwrite from `data` (chunks ≤ max_op_bytes).
    fn ring_write(&self, binding: u64, offset: u64, data: &[u8], what: &str) {
        let mut off = offset;
        for chunk in data.chunks(self.geometry.max_op_bytes as usize) {
            self.arena_write(0, chunk);
            let r = self.submit_wait(
                &SlotDescriptor {
                    op: OP_WRITE,
                    flags: 0,
                    binding,
                    offset: off,
                    len: chunk.len() as u32,
                    arena_off: 0,
                },
                what,
            );
            assert_eq!(r, chunk.len() as i64, "{what}: ring write short/failed");
            off += chunk.len() as u64;
        }
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

fn wait_counter_at_least(counter: &dyn Fn() -> u64, floor: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while counter() < floor {
        assert!(
            Instant::now() < deadline,
            "{what}: counter never reached {floor} (now {})",
            counter()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------
// byte parity + engagement accounting
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ring_read_matches_fuse_read_byte_parity() {
    let fx = Fixture::new("parity").await;
    let (ino, fd) = fx.create_file("parity.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Mixed shapes: small (inline→staged), block-interior, and a
    // block-boundary-spanning region on a striped file (block size is the
    // router default 4 MiB — offset 4 MiB - 8 KiB spans two blocks).
    let a = deterministic_bytes(3000, 1);
    let b = deterministic_bytes(96 * 1024, 2);
    let c = deterministic_bytes(16 * 1024, 3);
    fx.fuse_write(ino, 0, &a).await;
    fx.fuse_write(ino, 64 * 1024, &b).await;
    fx.fuse_write(ino, 4 * 1024 * 1024 - 8 * 1024, &c).await;

    let ops_before = METRICS.ipc_ops_read.load(Ordering::Relaxed);
    let bytes_before = METRICS.ipc_bytes_out.load(Ordering::Relaxed);

    let mut ring_ops = 0u64;
    let mut ring_bytes = 0u64;
    for (offset, len) in [
        (0u64, 3000usize),
        (64 * 1024, 96 * 1024),
        (4 * 1024 * 1024 - 8 * 1024, 16 * 1024),
        (1000, 2000),
        (0, 128 * 1024),
    ] {
        let ring =
            tokio::task::block_in_place(|| session.ring_read(binding, offset, len, "parity read"));
        let fuse = fx.fuse_read(ino, offset, len as u32).await;
        assert_eq!(
            ring, fuse,
            "byte parity ring vs FUSE at offset {offset} len {len}"
        );
        ring_ops += len.div_ceil(session.geometry.max_op_bytes as usize) as u64;
        ring_bytes += ring.len() as u64;
    }

    // §3 rule 4 engagement proof: daemon-side ops/bytes account for every
    // ring op the harness issued.
    assert!(
        METRICS.ipc_ops_read.load(Ordering::Relaxed) >= ops_before + ring_ops,
        "ipc_ops_read must count every served ring read"
    );
    assert!(
        METRICS.ipc_bytes_out.load(Ordering::Relaxed) >= bytes_before + ring_bytes,
        "ipc_bytes_out must count every served ring byte"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// RYW both directions (incl. the KD-11 buffered-kernel-write direction)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ryw_across_transports_both_directions() {
    let fx = Fixture::new("ryw").await;
    let (ino, fd) = fx.create_file("ryw.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Direction 1: ring write → FUSE read (kernel readers observe ring
    // writes — daemon state is the one authority).
    let w1 = deterministic_bytes(48 * 1024, 11);
    let ops_w_before = METRICS.ipc_ops_write.load(Ordering::Relaxed);
    tokio::task::block_in_place(|| session.ring_write(binding, 4096, &w1, "ryw ring write"));
    let fuse = fx.fuse_read(ino, 4096, w1.len() as u32).await;
    assert_eq!(fuse, w1, "ring write must be visible to a FUSE read");
    assert!(
        METRICS.ipc_ops_write.load(Ordering::Relaxed) > ops_w_before,
        "ipc_ops_write counts ring writes"
    );

    // Direction 2 (KD-11): buffered kernel write(2) → ring read. Under
    // write-through (forced by -o interception; pinned by the KD-11
    // posture test) the kernel delivers the WRITE to this handler
    // synchronously before acking — the handler call IS the
    // daemon-arrival point of a buffered kernel write.
    let w2 = deterministic_bytes(32 * 1024, 12);
    fx.fuse_write(ino, 100 * 1024, &w2).await;
    let ring = tokio::task::block_in_place(|| {
        session.ring_read(binding, 100 * 1024, w2.len(), "ryw ring read")
    });
    assert_eq!(
        ring, w2,
        "a kernel-write-through write must be visible to a ring read (KD-11)"
    );

    // Overwrite through the OTHER transport and re-read through both.
    let w3 = deterministic_bytes(32 * 1024, 13);
    tokio::task::block_in_place(|| session.ring_write(binding, 100 * 1024, &w3, "overwrite"));
    let ring = tokio::task::block_in_place(|| {
        session.ring_read(binding, 100 * 1024, w3.len(), "re-read ring")
    });
    let fuse = fx.fuse_read(ino, 100 * 1024, w3.len() as u32).await;
    assert_eq!(ring, w3);
    assert_eq!(fuse, w3);
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// fsync-through-FUSE ordering (§5.6.3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_through_fuse_after_ring_writes() {
    let fx = Fixture::new("fsync").await;
    let (ino, fd) = fx.create_file("fsync.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Program order: ring writes returned (acked into daemon write state)
    // → app issues fsync through kernel FUSE → the daemon flushes THAT
    // SAME state (happens-before chain, §5.6.3). Soundness here = fsync
    // succeeds and the bytes remain byte-identical through both
    // transports afterwards.
    let w = deterministic_bytes(72 * 1024, 21);
    tokio::task::block_in_place(|| session.ring_write(binding, 0, &w, "pre-fsync ring write"));
    fx.fs
        .fsync(req(), ino, 0, false)
        .await
        .expect("fsync through FUSE after ring writes must succeed");

    let ring =
        tokio::task::block_in_place(|| session.ring_read(binding, 0, w.len(), "post-fsync ring"));
    let fuse = fx.fuse_read(ino, 0, w.len() as u32).await;
    assert_eq!(ring, w, "ring read parity after fsync");
    assert_eq!(fuse, w, "FUSE read parity after fsync");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// arena-lease severance (§5.5.2): acked bytes are immune to later client
// scribbles of the arena slab
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arena_severance_client_scribble_never_reaches_acked_bytes() {
    let fx = Fixture::new("sever").await;
    let (ino, fd) = fx.create_file("sever.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let w = deterministic_bytes(64 * 1024, 31);
    tokio::task::block_in_place(|| session.ring_write(binding, 0, &w, "sever write"));

    // The op is DONE: the daemon's custody of these bytes must be a
    // severed private copy (or already-merged state) — scribbling the
    // arena slab the write used can never alter acked content.
    session.arena_fill(0, 64 * 1024, 0xAA);

    let ring =
        tokio::task::block_in_place(|| session.ring_read(binding, 0, w.len(), "sever ring read"));
    // ring_read reuses arena_off 0 — it just overwrote the scribble; the
    // CONTENT must be the acked bytes, not 0xAA.
    assert_eq!(
        ring, w,
        "ring read must return acked bytes, never scribbles"
    );
    let fuse = fx.fuse_read(ino, 0, w.len() as u32).await;
    assert_eq!(
        fuse, w,
        "FUSE read must return acked bytes, never scribbles"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// interleaved transports against a byte oracle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interleaved_transports_match_oracle() {
    let fx = Fixture::new("interleave").await;
    let (ino, fd) = fx.create_file("interleave.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let mut oracle: Vec<u8> = Vec::new();
    let apply = |oracle: &mut Vec<u8>, offset: usize, data: &[u8]| {
        if oracle.len() < offset + data.len() {
            oracle.resize(offset + data.len(), 0);
        }
        oracle[offset..offset + data.len()].copy_from_slice(data);
    };

    // Alternating, overlapping writes — ack order is program order (each
    // op completes before the next is issued), so the oracle is exact.
    for i in 0..12u64 {
        let data = deterministic_bytes(20 * 1024, 100 + i);
        let offset = (i * 12 * 1024) as usize; // overlaps the previous write
        if i % 2 == 0 {
            tokio::task::block_in_place(|| {
                session.ring_write(binding, offset as u64, &data, "interleaved ring write")
            });
        } else {
            fx.fuse_write(ino, offset as u64, &data).await;
        }
        apply(&mut oracle, offset, &data);
    }

    let ring = tokio::task::block_in_place(|| {
        session.ring_read(binding, 0, oracle.len(), "oracle ring read")
    });
    let fuse = fx.fuse_read(ino, 0, oracle.len() as u32).await;
    assert_eq!(ring, oracle, "ring view matches the oracle");
    assert_eq!(fuse, oracle, "FUSE view matches the oracle");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// adversarial mid-serve mutation (§5.3.1 rules 1–2): the served op is the
// snapshot; the written content is the severed copy
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_mid_serve_mutation_serves_the_snapshot() {
    let fx = Fixture::new("adversarial").await;
    let (ino, fd) = fx.create_file("adversarial.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Seed content.
    let seed = deterministic_bytes(64 * 1024, 41);
    fx.fuse_write(ino, 0, &seed).await;

    // --- WRITE variant -------------------------------------------------
    // Hold the inode write lock: the ring write demotes to the async
    // handoff and parks behind us — a deterministic mid-serve window.
    let lock = fx.fs.get_inode_lock_ref(ino);
    let guard = lock.write().await;

    let original = deterministic_bytes(8 * 1024, 42);
    session.arena_write(4096, &original);
    let handoffs_before = METRICS.ipc_async_handoffs.load(Ordering::Relaxed);
    let gen = session.submit_on(
        1,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset: 1024,
            len: original.len() as u32,
            arena_off: 4096,
        },
    );
    // Wait until the service thread has DEQUEUED + SNAPSHOTTED + SEVERED
    // (the handoff counter increments after the sever, before the park).
    wait_counter_at_least(
        &|| METRICS.ipc_async_handoffs.load(Ordering::Relaxed),
        handoffs_before + 1,
        "write handoff",
    );
    assert!(
        !session.is_done(1, gen),
        "op must be parked behind the writer"
    );

    // Mid-serve hostile mutation: descriptor fields AND payload bytes.
    session.slot(1).publish_descriptor(&SlotDescriptor {
        op: OP_WRITE,
        flags: 0,
        binding,
        offset: 999_999,       // must NOT be served
        len: 16,               // must NOT be served
        arena_off: 512 * 1024, // must NOT be served
    });
    session.arena_fill(4096, original.len(), 0x5A); // post-sever scribble

    drop(guard);
    let r = session.wait_done(1, gen, "adversarial write");
    assert_eq!(
        r,
        original.len() as i64,
        "result reflects the SNAPSHOT length, not the mutated descriptor"
    );

    // Served content = the severed copy of the ORIGINAL payload at the
    // SNAPSHOT offset; the descriptor mutation and the scribble are inert.
    let read_back = fx.fuse_read(ino, 1024, original.len() as u32).await;
    assert_eq!(
        read_back, original,
        "written bytes must be the severed pre-mutation payload"
    );
    let at_mutated = fx.fuse_read(ino, 999_999, 16).await;
    assert!(
        at_mutated.iter().all(|b| *b == 0) || at_mutated.is_empty(),
        "nothing may be written at the mutated offset (got {at_mutated:?})"
    );

    // --- READ variant ----------------------------------------------------
    let guard = lock.write().await;
    let lock_demotes_before = METRICS.ipc_fast_path_lock_demotions.load(Ordering::Relaxed);
    session.arena_fill(64 * 1024, 8 * 1024, 0x11); // pre-fill the dest window
    let gen = session.submit_on(
        2,
        &SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding,
            offset: 0,
            len: 8 * 1024,
            arena_off: 64 * 1024,
        },
    );
    wait_counter_at_least(
        &|| METRICS.ipc_fast_path_lock_demotions.load(Ordering::Relaxed),
        lock_demotes_before + 1,
        "read lock demotion",
    );
    assert!(
        !session.is_done(2, gen),
        "read must be parked behind the writer"
    );
    // Mutate the descriptor mid-serve: the served op must be the snapshot.
    session.slot(2).publish_descriptor(&SlotDescriptor {
        op: OP_READ,
        flags: 0,
        binding,
        offset: 32 * 1024,
        len: 16,
        arena_off: 0,
    });
    drop(guard);
    let r = session.wait_done(2, gen, "adversarial read");
    assert_eq!(r, 8 * 1024, "read served the SNAPSHOT length");
    let got = session.arena_read(64 * 1024, 8 * 1024);
    // The file at [0, 8 KiB) is the seed EXCEPT [1024, 8 KiB): the write
    // variant above committed `original` at offset 1024.
    let mut expected = seed[0..8 * 1024].to_vec();
    expected[1024..].copy_from_slice(&original[..7 * 1024]);
    assert_eq!(
        got, expected,
        "read payload landed at the SNAPSHOT arena_off with the SNAPSHOT range"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// the §5.5.1 demote rule: lock demotions (queued writer), miss demotions
// (cold caches), fast-path serves — each observable and deadlock-free
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_writer_lock_demotion_completes_after_release_no_deadlock() {
    let fx = Fixture::new("demote-lock").await;
    let (ino, fd) = fx.create_file("demote-lock.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let w = deterministic_bytes(32 * 1024, 51);
    tokio::task::block_in_place(|| session.ring_write(binding, 0, &w, "warm write"));

    // Hold the inode write lock (the queued-writer shape): the service
    // thread's try_read MUST fail → demote-lock → the handoff parks on
    // lock.read() BEHIND this writer. Drop-guard-before-enqueue is what
    // makes this safe: the service thread holds nothing while we hold
    // the write lock, so release ⇒ progress, never deadlock.
    let lock = fx.fs.get_inode_lock_ref(ino);
    let guard = lock.write().await;

    let lock_demotes_before = METRICS.ipc_fast_path_lock_demotions.load(Ordering::Relaxed);
    let mut gens = Vec::new();
    for i in 0..3u32 {
        let gen = session.submit_on(
            i,
            &SlotDescriptor {
                op: OP_READ,
                flags: 0,
                binding,
                offset: (i as u64) * 4096,
                len: 4096,
                arena_off: u64::from(i) * 8192,
            },
        );
        gens.push(gen);
    }
    wait_counter_at_least(
        &|| METRICS.ipc_fast_path_lock_demotions.load(Ordering::Relaxed),
        lock_demotes_before + 3,
        "three lock demotions",
    );
    // Bounded negative check: nothing completes while the writer holds.
    std::thread::sleep(Duration::from_millis(150));
    for (i, gen) in gens.iter().enumerate() {
        assert!(
            !session.is_done(i as u32, *gen),
            "op {i} must stay parked while the writer holds the lock"
        );
    }

    drop(guard);
    for (i, gen) in gens.iter().enumerate() {
        let r = session.wait_done(i as u32, *gen, "post-release read");
        assert_eq!(r, 4096, "parked read {i} completes after release");
        let got = session.arena_read(i as u64 * 8192, 4096);
        assert_eq!(
            got,
            &w[i * 4096..(i + 1) * 4096],
            "parity for parked read {i}"
        );
    }

    // No-deadlock soak: interleaved FUSE writes + ring reads on one inode.
    for i in 0..50u64 {
        let data = deterministic_bytes(4096, 200 + i);
        let offset = (i % 8) * 4096;
        fx.fuse_write(ino, offset, &data).await;
        let ring = tokio::task::block_in_place(|| {
            session.ring_read(binding, offset, 4096, "soak ring read")
        });
        assert_eq!(ring, data, "soak parity at iteration {i}");
    }
    fx.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_attr_miss_demotes_and_eof_fast_path_serves() {
    let fx = Fixture::new("demote-miss").await;
    let (ino, fd) = fx.create_file("demote-miss.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let w = deterministic_bytes(16 * 1024, 61);
    tokio::task::block_in_place(|| session.ring_write(binding, 0, &w, "warm write"));

    // Cold attr cache ⇒ the fast path's in-guard attr probe misses ⇒
    // demote-miss (the handler's in-guard async getattr fallback is
    // unreachable from a sync service thread — §5.5.1 normative demote),
    // and the async handoff still serves parity.
    fx.fs.attr_cache.invalidate(&ino);
    let miss_before = METRICS.ipc_fast_path_miss_demotions.load(Ordering::Relaxed);
    let ring = tokio::task::block_in_place(|| session.ring_read(binding, 0, w.len(), "cold read"));
    assert_eq!(ring, w, "cold-cache ring read still serves parity");
    assert!(
        METRICS.ipc_fast_path_miss_demotions.load(Ordering::Relaxed) > miss_before,
        "attr-cache miss must be counted as a MISS demotion"
    );

    // EOF short-circuit is a deterministic sync fast-path serve: the attr
    // is warm again (the handoff re-seeded it), offset ≥ size ⇒ 0 bytes,
    // no handler, no runtime.
    let _ = tokio::task::block_in_place(|| session.ring_read(binding, 0, 16, "re-warm attrs"));
    let serves_before = METRICS.ipc_fast_path_serves.load(Ordering::Relaxed);
    let r = session.submit_wait(
        &SlotDescriptor {
            op: OP_READ,
            flags: 0,
            binding,
            offset: 1024 * 1024 * 1024, // far past EOF
            len: 4096,
            arena_off: 0,
        },
        "eof read",
    );
    assert_eq!(r, 0, "read past EOF returns 0 bytes");
    assert!(
        METRICS.ipc_fast_path_serves.load(Ordering::Relaxed) > serves_before,
        "the EOF short-circuit is a sync fast-path serve"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// §8 stats surface for the data-plane families
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_plane_stats_fields_export() {
    let fx = Fixture::new("stats").await;
    let stats = fx.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json parses");
    let m = v
        .get("metrics")
        .expect("stats JSON carries a metrics object");
    for key in [
        "ipc_ops_read",
        "ipc_ops_write",
        "ipc_bytes_in",
        "ipc_bytes_out",
        "ipc_fast_path_serves",
        "ipc_async_handoffs",
        "ipc_fast_path_lock_demotions",
        "ipc_fast_path_miss_demotions",
        "ipc_service_threads",
    ] {
        assert!(m.get(key).is_some(), "stats inode must export {key}");
    }
    fx.shutdown();
}
