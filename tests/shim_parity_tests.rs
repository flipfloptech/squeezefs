//! Shim-parity campaign (2026-07-28) — the **placed sever**: ring WRITE
//! payloads sever DIRECTLY into the block's future `ActiveBlockBuf`
//! backing at dequeue, deleting the merge copy the ring path paid on top
//! of the kernel path's single lease→merge copy
//! (`.benchmarks/2026-07-28-ingest-economy.md` §8 board item 1: kernel
//! out-streamed the ring path ~15 % at t16×4MiB because a ring write paid
//! shim-copy + sever-copy + merge vs the kernel's payload-lease + merge).
//!
//! The contracts pinned here:
//!
//! 1. **1-copy streaming ring writes** — a striped-file whole-block chunk
//!    stream severs into ONE shared per-(ino, block) assembly at dequeue;
//!    the first merging handler ADOPTS the assembly as the overlay
//!    backing (`placed_adoptions`) and every sibling chunk's merge is a
//!    coverage-record with the copy ELIDED (`placed_merge_elides`) — the
//!    payload region IS the backing region, proven by pointer identity.
//! 2. **The §5.5.2 custody law survives verbatim** — placed severs still
//!    leave client-writable arena memory synchronously at dequeue: a
//!    post-sever arena scribble can never reach acked bytes.
//! 3. **SeveredPool remains for every shape that cannot target the block
//!    buffer** — unaligned / small / non-striped / entry-present ops ride
//!    the pooled 2-copy path unchanged, with placed counters silent.
//! 4. **Re-writes of a parked block fall back safely** — an existing
//!    overlay entry means the assembly can never be adopted; the merge
//!    copies (newest-wins), content stays exact.
//!
//! Harness: the preload_parity_tests raw-protocol client (real host →
//! service thread → sink → REAL write handler; InoMapSink translates
//! harness st_ino → fs ino at the sink boundary — a test seam, never a
//! production path).

use squeezefs::fuse_client::METRICS;
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::DataPlaneSink;
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_WRITE,
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
// fixture (the preload_parity_tests harness, sized for whole-block streams)
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

    // Pin the overlay fresh-write store OFF (default ON since ab74d1ad;
    // the custody-pinning suites' shared pin): this suite pins the
    // placed-sever ADOPTION law of the accumulation path — with the
    // overlay ON, a ring-severed fresh block rides the severed Bytes
    // straight into the overlay DMA (18c5fbb6's IL-sever aligned
    // passthrough, covered by tests/overlay_ack_early_tests.rs), no
    // ActiveBlockBuf is born, and the adoption accounting legitimately
    // never fires. The accumulation path stays live product code
    // (overlay-off mounts, ineligible shapes).
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    let dlm = DlmClient::new().unwrap();
    let backing_temp = tempfile::NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new("shim_parity_tests")
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

fn req() -> Request {
    Request {
        unique: 1,
        // SAFETY: plain getuid/getgid.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
        ..Default::default()
    }
}

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
            panic!("harness bound an untranslated st_ino {}", op.binding.ino);
        };
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn flush(&self) {
        SessionSink::flush(&self.inner);
    }
}

/// Whole-block-stream geometry: 1 MiB ops (above the placed-sever floor —
/// `len > patch_max_bytes()`, default 512 KiB), slab = arena/slots =
/// 1 MiB, so a 4 MiB block is a 4-chunk stream (the field's shape).
fn test_geometry() -> Geometry {
    Geometry {
        ring_entries: 16,
        slots: 16,
        arena_bytes: 16 * 1024 * 1024,
        max_op_bytes: 1024 * 1024,
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
            inner: DataPlaneSink::new(fs.clone()),
            map: std::sync::Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-parity-{}-{}", std::process::id(), name),
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

    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        let reply = self
            .fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read");
        reply.data.to_vec()
    }

    /// Grow `ino` into a STRIPED file of `mib` MiB via the FUSE handler
    /// (sequential 1 MiB writes promote inline→staged→striped), fsync it
    /// durable, wait for the parked overlays to retire (the ring
    /// overwrite under test needs entry-absent blocks), and warm the
    /// metadata cache (the placed-sever screen reads it synchronously).
    async fn make_striped(&self, ino: u64, mib: u64) {
        let chunk = vec![0x11u8; 1024 * 1024];
        for i in 0..mib {
            // Growth-phase writes can hit transient EAGAIN under the
            // sandbox's tiny budgets (writeback saturation) — retry like
            // the kernel's writeback would; anything else is a failure.
            let mut attempts = 0u32;
            loop {
                match self
                    .fs
                    .write(
                        req(),
                        ino,
                        0,
                        i * 1024 * 1024,
                        bytes::Bytes::copy_from_slice(&chunk),
                        0,
                        0,
                    )
                    .await
                {
                    Ok(_) => break,
                    // A prior test's fixture can pin this ino's DLM lease
                    // until its TTL lapses (process-global "local" DLM,
                    // fresh meta volume per fixture ⇒ colliding inos) —
                    // ride it out; anything persistent still fails loud.
                    Err(e) if libc::c_int::from(e).abs() == libc::EAGAIN && attempts < 120 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Err(e) => panic!("growth write failed: {e:?}"),
                }
            }
        }
        self.fs
            .fsync(req(), ino, 0, false)
            .await
            .expect("fsync after growth");
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.fs.parked_buffer_bytes() != 0 {
            assert!(
                Instant::now() < deadline,
                "parked overlays must retire after fsync (still {} bytes)",
                self.fs.parked_buffer_bytes()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let meta = self
            .fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("fetch metadata");
        assert_eq!(
            meta.file_type.as_str(),
            "striped",
            "growth must have promoted the file to striped"
        );
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

// SAFETY: the harness drives ops from test threads; the mapping is plain
// shared memory (the daemon side uses atomics + bounded raw copies).
unsafe impl Send for ClientSession {}
unsafe impl Sync for ClientSession {}

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
                // SAFETY: plain getuid.
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
            MpscRingView::from_parts(tail, cells).expect("geometry validated")
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
        let dst = self.arena_ptr(off, data.len());
        // SAFETY: bounds checked by arena_ptr; harness is the only writer
        // on this side.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
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

    fn is_done(&self, slot_idx: u32, gen: u64) -> bool {
        self.slot(slot_idx).core.is_done_for(gen)
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
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in establish.
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
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 131) % 251) as u8)
        .collect()
}

fn wait_counter_at_least(read: &dyn Fn() -> u64, target: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while read() < target {
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {what} (at {}, want ≥ {target})",
            read()
        );
        std::thread::yield_now();
    }
}

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// 1. + 2. — the 1-copy streaming contract + the custody law
// ---------------------------------------------------------------------------

/// A 4-chunk whole-block ring stream against a striped file: every chunk
/// severs into the shared block assembly at dequeue (`ipc_placed_severs`),
/// the first merge ADOPTS the assembly as the overlay backing
/// (`placed_adoptions` = 1) and every merge elides its copy
/// (`placed_merge_elides` = 4); post-sever arena scribbles are inert
/// (§5.5.2 verbatim); the block's coverage-complete transition still
/// skips the overwrite seed read (RW3b unchanged); read-back is exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_ring_write_severs_into_block_buffer_one_copy() {
    let fx = Fixture::new("placed-stream").await;
    let (ino, fd) = fx.create_file("stream.bin").await;
    fx.make_striped(ino, 8).await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let severs0 = METRICS.ipc_placed_severs.load(Ordering::Relaxed);
    let adopts0 = METRICS.placed_adoptions.load(Ordering::Relaxed);
    let elides0 = METRICS.placed_merge_elides.load(Ordering::Relaxed);
    let seedskip0 = METRICS.overwrite_seed_skipped.load(Ordering::Relaxed);
    let seedread0 = METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed);

    // Hold the inode write lock: handlers park at the guard while the
    // service thread dequeues + severs every chunk — the deterministic
    // all-severed-before-any-merge window (the saturated-stream schedule).
    let lock = fx.fs.get_inode_lock_ref(ino);
    let guard = lock.write().await;

    // EXTEND into block 2 (offset 8..12 MiB) as 4 × 1 MiB chunks — the
    // field's streaming-ingest shape: a fresh block of a striped file
    // (no staged sibling, no existing bytes; a previously-staged block
    // legitimately seeds from its NEWER staged image instead — that
    // shape stays on the copy path by design).
    let payload = deterministic_bytes(4 * MIB as usize, 7);
    let mut gens = Vec::new();
    for c in 0..4u64 {
        let chunk = &payload[(c * MIB) as usize..((c + 1) * MIB) as usize];
        session.arena_write(c * MIB, chunk);
        let gen = session.submit_on(
            c as u32,
            &SlotDescriptor {
                op: OP_WRITE,
                flags: 0,
                binding,
                offset: 8 * MIB + c * MIB,
                len: MIB as u32,
                arena_off: c * MIB,
            },
        );
        gens.push(gen);
    }

    // Every chunk must sever at DEQUEUE — before any handler can merge
    // (they are all parked behind the held inode lock).
    wait_counter_at_least(
        &|| METRICS.ipc_placed_severs.load(Ordering::Relaxed),
        severs0 + 4,
        "placed severs at dequeue",
    );
    for (c, gen) in gens.iter().enumerate() {
        assert!(
            !session.is_done(c as u32, *gen),
            "chunk {c} must be parked behind the held inode lock"
        );
    }

    // §5.5.2 verbatim: custody left the arena at dequeue — scribble it.
    session.arena_fill(0, 4 * MIB as usize, 0xAA);

    drop(guard);
    for (c, gen) in gens.iter().enumerate() {
        let r = session.wait_done(c as u32, *gen, "streamed chunk");
        assert_eq!(r, MIB as i64, "chunk {c} must ack its full length");
    }

    // The 1-copy accounting: one adoption, EVERY chunk's merge copy
    // elided (the adopter's own merge included — its bytes were already
    // in the backing at dequeue).
    assert_eq!(
        METRICS.placed_adoptions.load(Ordering::Relaxed) - adopts0,
        1,
        "the first merging chunk must adopt the assembly as the overlay backing"
    );
    assert_eq!(
        METRICS.placed_merge_elides.load(Ordering::Relaxed) - elides0,
        4,
        "every chunk's merge must elide its copy (pointer-proof)"
    );
    // RW3b unchanged: a fresh extending block never touches the seed
    // machinery, and the write path never pays a seed read.
    assert_eq!(
        METRICS.overwrite_seed_skipped.load(Ordering::Relaxed),
        seedskip0,
        "a fresh (extending) block arms no seed deferral"
    );
    assert_eq!(
        METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed),
        seedread0,
        "write_path_seed_read_bytes is a must-stay-0 tripwire"
    );

    // Content: the acked (pre-scribble) bytes, exactly.
    let read_back = fx.fuse_read(ino, 8 * MIB, 4 * MIB as u32).await;
    assert_eq!(
        read_back, payload,
        "read-back must be the severed pre-scribble payload"
    );
    // The neighbour blocks are untouched.
    let before = fx.fuse_read(ino, 3 * MIB, MIB as u32).await;
    assert!(before.iter().all(|&b| b == 0x11), "block 0 tail untouched");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 3. — pooled fallback for every shape that cannot target the block buffer
// ---------------------------------------------------------------------------

/// Unaligned, small, and non-striped writes never engage the placed
/// sever (counters silent) and stay byte-exact on the pooled path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ineligible_shapes_ride_the_pooled_sever_unchanged() {
    let fx = Fixture::new("placed-fallback").await;
    let (ino, fd) = fx.create_file("fallback.bin").await;
    fx.make_striped(ino, 8).await;
    let (small_ino, small_fd) = fx.create_file("small.bin").await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let severs0 = METRICS.ipc_placed_severs.load(Ordering::Relaxed);
    let adopts0 = METRICS.placed_adoptions.load(Ordering::Relaxed);

    // (a) Unaligned rel offset (4 MiB + 512) — placed severs are
    // page-aligned by contract.
    let w_unaligned = deterministic_bytes(MIB as usize, 3);
    session.arena_write(0, &w_unaligned);
    let gen = session.submit_on(
        0,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset: 4 * MIB + 512,
            len: MIB as u32,
            arena_off: 0,
        },
    );
    assert_eq!(session.wait_done(0, gen, "unaligned"), MIB as i64);
    let rb = fx.fuse_read(ino, 4 * MIB + 512, MIB as u32).await;
    assert_eq!(rb, w_unaligned, "unaligned write content");

    // (b) Small op (64 KiB < the placed floor).
    let w_small = deterministic_bytes(64 * 1024, 4);
    session.arena_write(0, &w_small);
    let gen = session.submit_on(
        0,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset: 0,
            len: w_small.len() as u32,
            arena_off: 0,
        },
    );
    assert_eq!(session.wait_done(0, gen, "small"), w_small.len() as i64);

    // (c) Non-striped file (small stand-in, cached type inline/staged).
    let (session2, binding2) = ClientSession::establish(&fx, &small_fd);
    let w_tiny = deterministic_bytes(1024 * 1024, 5);
    session2.arena_write(0, &w_tiny);
    let gen = session2.submit_on(
        0,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding: binding2,
            offset: 0,
            len: w_tiny.len() as u32,
            arena_off: 0,
        },
    );
    assert_eq!(
        session2.wait_done(0, gen, "non-striped"),
        w_tiny.len() as i64
    );
    let rb = fx.fuse_read(small_ino, 0, w_tiny.len() as u32).await;
    assert_eq!(rb, w_tiny, "non-striped write content");

    assert_eq!(
        METRICS.ipc_placed_severs.load(Ordering::Relaxed),
        severs0,
        "ineligible shapes must never engage the placed sever"
    );
    assert_eq!(
        METRICS.placed_adoptions.load(Ordering::Relaxed),
        adopts0,
        "no assembly may be adopted for ineligible shapes"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 4. — an existing overlay entry forces the safe copy fallback
// ---------------------------------------------------------------------------

/// A partial-coverage parked block (entry present) makes later placed
/// shapes fall back: the re-write of the same region lands via the merge
/// copy, newest-wins, byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_of_parked_block_falls_back_to_merge_copy() {
    let fx = Fixture::new("placed-rewrite").await;
    let (ino, fd) = fx.create_file("rewrite.bin").await;
    fx.make_striped(ino, 8).await;
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let severs0 = METRICS.ipc_placed_severs.load(Ordering::Relaxed);

    // First 1 MiB chunk of block 1: placed sever + adoption, coverage
    // stays partial → the overlay entry stays parked.
    let first = deterministic_bytes(MIB as usize, 8);
    session.arena_write(0, &first);
    let gen = session.submit_on(
        0,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset: 4 * MIB,
            len: MIB as u32,
            arena_off: 0,
        },
    );
    assert_eq!(session.wait_done(0, gen, "first placed write"), MIB as i64);
    assert_eq!(
        METRICS.ipc_placed_severs.load(Ordering::Relaxed) - severs0,
        1,
        "the first chunk must place-sever"
    );

    // Re-write the SAME region with different bytes: the parked entry
    // exists, so the op must fall back (pooled sever or merge copy) and
    // the newest bytes must win.
    let second = deterministic_bytes(MIB as usize, 9);
    session.arena_write(0, &second);
    let gen = session.submit_on(
        0,
        &SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset: 4 * MIB,
            len: MIB as u32,
            arena_off: 0,
        },
    );
    assert_eq!(session.wait_done(0, gen, "rewrite"), MIB as i64);

    let rb = fx.fuse_read(ino, 4 * MIB, MIB as u32).await;
    assert_eq!(rb, second, "newest write must win byte-exactly");
    fx.shutdown();
}
