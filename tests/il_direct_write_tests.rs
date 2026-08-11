//! IL **direct-drive WRITE lane** rails (docs/design-il-direct-write.md
//! §3 the lane / §5 instruments — PR-2+PR-3 of the ladder).
//!
//! A ring write dequeued on a service thread historically ALWAYS severed
//! and handed off to the fuse3-tpc handler lanes. The lane under test
//! executes the **W1 sole-owner patch shape** (design-random-small-writes
//! §5.1 — LBA-aligned sub-block overwrites of exclusively-owned,
//! passthrough, whole-block-mapped striped blocks) directly on the svc
//! thread's dd shard: eligibility probe → §5.1 fence →
//! `authorize_zc_store` (RES-6) → in-place WRITE SQE from the arena →
//! CQE postlude (the patch path's re-stabilize + purge-before-ACK law,
//! verbatim ordering) → ring completion.
//!
//! Rails pinned here (each names its design clause):
//!
//! * **(a) decision-ledger closure** — an ineligible shape falls back to
//!   the sever→handoff path UNCHANGED, counts exactly its
//!   `ipc_dd_write_ineligible_*` clause bucket, and serves 0 direct.
//! * **(b) the RES-6 fence rail** — a fenced writer guard refuses the
//!   direct write LOUD (errno to the client, `ipc_dd_write_fence_refusals`
//!   moves) and never falls back to a second submission path.
//! * **(c) purge-before-ACK** — when the ring completion lands, the block
//!   key is already absent from every read tier (the stale-serve law:
//!   `purge_block_key`'s 4-arm sweep + the whole-file LRU drops run
//!   BEFORE `completion.complete`).
//! * **(d) coverage/ledger** — a direct write IS a patch write:
//!   `patch_writes` and `ipc_dd_write_serves` both account for it, and
//!   the engagement law `dd_write_serves + ipc_async_handoffs ≈
//!   ipc_ops_write` closes over the row.
//! * **(e) the A/B lever** — `SQUEEZEFS_IL_DIRECT_WRITE=0` (the runtime
//!   cell seam) routes EVERY ring write to the handoff path: serves stay
//!   0, handoffs carry the row, the ledger stays silent.
//! * **(f) session machinery untouched** — the KD-7 session
//!   establishment, ring reads, and byte parity across transports keep
//!   working around direct writes (the harness IS the session path).
//!
//! Harness: the ipc_op_economy_tests raw-protocol client (real host →
//! service thread → sink; no kernel in the loop) over the
//! extent_patch_tests striped sandbox (64 KiB blocks — the patch anatomy
//! is block-size-relative; every offset speaks the 4096-byte LBA
//! quantum).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::fuse_client::{set_patch_max_bytes, METRICS};
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::{set_il_direct_write_enabled, DataPlaneSink};
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_WRITE,
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

/// Sandbox block size (the extent_patch_tests quantum): the W1 predicate
/// is block-size-relative — 64 KiB here, 4 MiB on the scoreboard.
const BS: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// fixture: extent-patch striped sandbox + the op-economy session host
// ---------------------------------------------------------------------------

async fn sandbox_fs(
    tag: &str,
) -> (
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

    // Block-size + patch-cap pins (per-fixture, the extent_patch_tests
    // discipline — the §6 A/B levers must never leak across tests).
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);

    let dlm = DlmClient::new().unwrap();
    let backing_temp = tempfile::NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(BlockAllocator::new(tag).await.expect("block allocator"));
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, block_alloc.clone(), nvme_dev);
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta_temp = tempfile::NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        meta_temp.path(),
        128 * 1024 * 1024,
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
    fs.meta_backend = Some(routed.clone());
    // v3 refcount recovery (the extent_patch_tests fixture step): the
    // RAM-authoritative refcounts the §5.1 predicate-4 re-check reads.
    for kv in &routed.volumes {
        block_alloc
            .recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    (fs, backing_temp, meta_temp, staging)
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
        ..Default::default()
    }
}

/// Ino-translating sink (the op-economy harness shape: stand-in fds carry
/// the stand-in file's `st_ino`; rewrite to the sandbox ino before serving).
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
        let (fs, backing, meta, staging) = sandbox_fs(name).await;
        let sink = Arc::new(InoMapSink {
            inner: DataPlaneSink::new(fs.clone()),
            map: Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-ddw-{}-{}", std::process::id(), name),
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

    /// Shift ino allocation so sequential tests never contend on the
    /// process-global DLM lock map for the same `inode_N` key (the
    /// op-economy salting discipline).
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
        assert_eq!(w.written as usize, data.len());
    }

    async fn fuse_read(&self, ino: u64, offset: u64, size: u32) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, offset, size, 0)
            .await
            .expect("fuse read")
            .data
            .to_vec()
    }

    async fn fsync(&self, ino: u64) {
        self.fs.fsync(req(), ino, 0, false).await.expect("fsync");
    }

    /// A durable, cold, freshly-striped fixture file (`blocks` × [`BS`],
    /// one pass, fsynced, read tiers purged): every block maps in the
    /// undecorated whole-block form — the W1-eligible population.
    async fn durable_striped(&self, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
        let ino = self.create_file(name).await;
        let base = pattern((blocks * BS) as usize, tag);
        self.fuse_write(ino, 0, &base).await;
        self.fsync(ino).await;
        let path = squeezefs::keys::inode_path(ino);
        let m = self.fs.router.fetch_metadata(&path).await.unwrap();
        assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
        self.purge_read_tiers(ino).await;
        (ino, base)
    }

    async fn purge_read_tiers(&self, ino: u64) {
        let path = squeezefs::keys::inode_path(ino);
        self.fs.router.cache.write_lru.remove(&path);
        self.fs.router.cache.read_lru.remove(&path);
        if let Ok(m) = self.fs.router.fetch_metadata(&path).await {
            if let Some(bm) = m.block_map.as_ref() {
                for bk in bm.values() {
                    self.fs.router.cache.purge_block_key(bk);
                }
            }
        }
    }

    async fn block_key(&self, ino: u64, b: u32) -> String {
        let path = squeezefs::keys::inode_path(ino);
        let m = self.fs.router.fetch_metadata(&path).await.unwrap();
        m.block_map
            .as_ref()
            .and_then(|bm| bm.get(&b))
            .expect("block mapping")
            .clone()
    }

    fn shutdown(&self) {
        self.host.shutdown();
    }
}

/// Buffered stand-in fd on a real filesystem, host expected st_dev
/// re-pointed, ino translation registered (the op-economy shape).
fn rw_standin(fx: &Fixture, dir: &tempfile::TempDir, name: &str, fs_ino: u64) -> OwnedFd {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("create stand-in");
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
    assert!(fd >= 0, "stand-in open failed");
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

    fn arena_write(&self, off: u64, data: &[u8]) {
        assert!(off + data.len() as u64 <= self.geometry.arena_bytes);
        // SAFETY: bounds asserted against the arena region.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.base.add((self.layout.arena_off + off) as usize),
                data.len(),
            )
        };
    }

    /// One ring pwrite on slot 0: arena stage → claim → publish → push →
    /// doorbell → bounded spin. Returns the slot result verbatim (bytes
    /// or -errno — the fence rail asserts the negative face).
    fn ring_pwrite(&self, binding: u64, offset: u64, data: &[u8]) -> i64 {
        assert!(data.len() <= self.geometry.max_op_bytes as usize);
        self.arena_write(0, data);
        let slot = self.slot(0);
        let gen = slot.core.try_claim().expect("slot 0 must be FREE");
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset,
            len: data.len() as u32,
            arena_off: 0,
        });
        slot.core.publish_submitted();
        assert!(self.ring().push(0), "ring must accept");
        self.header().doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&self.header().doorbell, 1);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !slot.core.is_done_for(gen) {
            assert!(Instant::now() < deadline, "ring write never completed");
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

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

// ---------------------------------------------------------------------------
// counter snapshots (the extent_patch_tests Snap discipline)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Snap {
    dd_serves: u64,
    dd_bytes: u64,
    dd_fence_refusals: u64,
    dd_shape: u64,
    dd_custody: u64,
    dd_overlay: u64,
    dd_backend: u64,
    dd_align: u64,
    patch_writes: u64,
    patch_write_bytes: u64,
    handoffs: u64,
    ops_write: u64,
    bytes_in: u64,
    data_fence_refusals: u64,
}

fn snap() -> Snap {
    let l = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Snap {
        dd_serves: l(&METRICS.ipc_dd_write_serves),
        dd_bytes: l(&METRICS.ipc_dd_write_bytes),
        dd_fence_refusals: l(&METRICS.ipc_dd_write_fence_refusals),
        dd_shape: l(&METRICS.ipc_dd_write_ineligible_shape),
        dd_custody: l(&METRICS.ipc_dd_write_ineligible_custody),
        dd_overlay: l(&METRICS.ipc_dd_write_ineligible_overlay),
        dd_backend: l(&METRICS.ipc_dd_write_ineligible_backend),
        dd_align: l(&METRICS.ipc_dd_write_ineligible_align),
        patch_writes: l(&METRICS.patch_writes),
        patch_write_bytes: l(&METRICS.patch_write_bytes),
        handoffs: l(&METRICS.ipc_async_handoffs),
        ops_write: l(&METRICS.ipc_ops_write),
        bytes_in: l(&METRICS.ipc_bytes_in),
        data_fence_refusals: l(&METRICS.data_dma_fence_refusals),
    }
}

macro_rules! delta {
    ($after:expr, $before:expr, $field:ident) => {
        $after.$field - $before.$field
    };
}

/// Warm the lane: the FIRST ring write may legitimately fall back once
/// (cold killpriv-clean latch on an unprivileged peer — the clearing
/// obligation is async metadata work the sync probe must not pay; the
/// handler latches it). After one warm-up, eligible writes MUST serve
/// direct. Returns when a probe write direct-serves.
fn warm_direct_lane(session: &ClientSession, binding: u64, base: &mut [u8], offsets: &[u64]) {
    let deadline = Instant::now() + Duration::from_secs(20);
    for (i, &off) in offsets.iter().enumerate() {
        let before = snap();
        let p = pattern(4096, 0xE0 + i as u8);
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "warm write must land");
        base[off as usize..off as usize + 4096].copy_from_slice(&p);
        if delta!(snap(), before, dd_serves) == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "direct-write lane never engaged across the warm-up offsets"
        );
    }
    panic!("direct-write lane never engaged (every warm-up write fell back)");
}

// ---------------------------------------------------------------------------
// (d) + (f): engagement, byte-exactness, ledger accounting
// ---------------------------------------------------------------------------

/// The lane's core contract: after one warm-up, every W1-shaped ring
/// write direct-serves (`ipc_dd_write_serves == ops`, ZERO handoffs),
/// each one IS a patch write (`patch_writes`/`patch_write_bytes` move in
/// lockstep), `ipc_ops_write`/`ipc_bytes_in` account for the row (the
/// charter-rule-4 instrument), and the file stays byte-exact through the
/// kernel-venue read path (which also proves the session machinery — f).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_writes_serve_on_the_svc_lane_and_stay_byte_exact() {
    let fx = Fixture::new("ddwcore").await;
    fx.salt_inos(1).await;
    let (ino, mut want) = fx.durable_striped("core.bin", 4, 0x00).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(
            &session,
            binding,
            &mut want,
            &[BS + 16384, 3 * BS + 8192, 2 * BS + 32768],
        );

        // The measured window: scattered, non-adjacent, LBA-aligned
        // in-block overwrites — the W1 population.
        let writes: &[(u64, usize, u8)] = &[
            (0, 4096, 0xA1),                    // block 0 start
            (2 * BS + 8192, 4096, 0xA2),        // block 2 interior
            (3 * BS + (BS - 4096), 4096, 0xA3), // block 3 tail
            (BS + 32768, 8192, 0xA4),           // block 1, 8 KiB
        ];
        let before = snap();
        let mut user_bytes = 0u64;
        for &(off, len, tag) in writes {
            let p = pattern(len, tag);
            let r = session.ring_pwrite(binding, off, &p);
            assert_eq!(r, len as i64, "direct write must ack its full length");
            want[off as usize..off as usize + len].copy_from_slice(&p);
            user_bytes += len as u64;
        }
        let after = snap();

        let ops = writes.len() as u64;
        assert_eq!(
            delta!(after, before, dd_serves),
            ops,
            "every W1-shaped ring write must serve on the direct lane \
             (design-il-direct-write §3)"
        );
        assert_eq!(
            delta!(after, before, dd_bytes),
            user_bytes,
            "ipc_dd_write_bytes accounts the served bytes"
        );
        assert_eq!(
            delta!(after, before, handoffs),
            0,
            "a direct-served window pays zero handler handoffs"
        );
        // (d) coverage/ledger: a direct write IS a patch write.
        assert_eq!(
            delta!(after, before, patch_writes),
            ops,
            "patch_writes must account for direct writes (it IS the W1 patch)"
        );
        assert_eq!(delta!(after, before, patch_write_bytes), user_bytes);
        // Engagement law: dd_write_serves + async_handoffs ≈ ipc_ops_write.
        assert_eq!(
            delta!(after, before, ops_write),
            ops,
            "ipc_ops_write must account for direct serves (engagement law)"
        );
        assert_eq!(delta!(after, before, bytes_in), user_bytes);
        // Ledger silence: an all-eligible window counts no fallback clause.
        assert_eq!(delta!(after, before, dd_shape), 0);
        assert_eq!(delta!(after, before, dd_custody), 0);
        assert_eq!(delta!(after, before, dd_overlay), 0);
        assert_eq!(delta!(after, before, dd_backend), 0);
        assert_eq!(delta!(after, before, dd_align), 0);
        assert_eq!(delta!(after, before, dd_fence_refusals), 0);
    });

    // (f) + byte parity: the kernel-venue read path serves the direct
    // writes' bytes back device-honest (tiers were purged before ACK).
    fx.purge_read_tiers(ino).await;
    for b in 0..4u64 {
        let got = fx.fuse_read(ino, b * BS, BS as u32).await;
        assert_eq!(
            got,
            want[(b * BS) as usize..((b + 1) * BS) as usize].to_vec(),
            "block {b} byte parity after direct writes"
        );
    }
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// (a): decision-ledger closure — ineligible shapes fall back, counted
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ineligible_shapes_fall_back_and_count_their_clause() {
    let fx = Fixture::new("ddwledger").await;
    fx.salt_inos(3).await;
    let (ino, mut want) = fx.durable_striped("ledger.bin", 4, 0x10).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);

        // align: LBA-misaligned offset — falls back, byte-exact, counted.
        let before = snap();
        let p = pattern(4096, 0xB1);
        let off = 3 * BS + 4096 + 512;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "the fallback path must still ack the write");
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();
        assert_eq!(
            delta!(after, before, dd_align),
            1,
            "a misaligned shape counts exactly its align clause"
        );
        assert_eq!(delta!(after, before, dd_serves), 0, "no direct serve");
        assert_eq!(
            delta!(after, before, handoffs),
            1,
            "the ineligible op rides today's sever→handoff path"
        );

        // shape: an EXTENDING write (end > size — a grown i_size owes a
        // meta commit; the lane must refuse it).
        let before = snap();
        let p = pattern(4096, 0xB2);
        let off = 4 * BS; // exactly at EOF: extends
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "the extending write lands via the handler");
        want.extend_from_slice(&p);
        let after = snap();
        assert_eq!(
            delta!(after, before, dd_shape),
            1,
            "an extending shape counts exactly its shape clause"
        );
        assert_eq!(delta!(after, before, dd_serves), 0);
        assert_eq!(delta!(after, before, handoffs), 1);

        // custody: drop the cached lease — the sync probe must not pay the
        // async lease acquisition; the op demotes and the handler
        // re-acquires.
        fx.fs.invalidate_local_lease(ino);
        let before = snap();
        let p = pattern(4096, 0xB3);
        let off = 2 * BS + 24576;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096);
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();
        assert!(
            delta!(after, before, dd_custody) >= 1,
            "a lease-less ino counts the custody clause (an await-needing \
             custody shape is INELIGIBLE — v1 stays synchronous)"
        );
        assert_eq!(delta!(after, before, dd_serves), 0);
        assert_eq!(delta!(after, before, handoffs), 1);

        // overlay: park a RAM extent overlay on block 1 (an unaligned
        // small kernel write parks — the W2 machinery), then an
        // aligned ring write to the SAME block must refuse: an in-place
        // patch under a newer overlay would be re-applied over.
        let h = tokio::runtime::Handle::current();
        h.block_on(async {
            let p = pattern(1024, 0xB4);
            let off = BS + 512; // unaligned + small ⇒ extent park
            fx.fuse_write(ino, off, &p).await;
            want[off as usize..off as usize + 1024].copy_from_slice(&p);
        });
        let before = snap();
        let p = pattern(4096, 0xB5);
        let off = BS + 40960;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096);
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();
        assert_eq!(
            delta!(after, before, dd_overlay),
            1,
            "a live overlay on the block counts exactly its overlay clause"
        );
        assert_eq!(delta!(after, before, dd_serves), 0);
        assert_eq!(delta!(after, before, handoffs), 1);
    });

    // Every fallback stayed byte-exact through the handler path.
    fx.fsync(ino).await;
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 0, want.len() as u32).await;
    assert_eq!(got, want, "fallback writes must stay byte-exact");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// (b): the RES-6 fence rail — loud refusal, never a second submission
// ---------------------------------------------------------------------------

/// A fenced writer guard refuses the direct write LOUD: errno to the
/// client, `ipc_dd_write_fence_refusals` + `data_dma_fence_refusals`
/// move, and the op is NEVER re-submitted through the handler (a fenced
/// holder must not fall back to a second submission path — the patch
/// path's own law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_guard_refuses_direct_writes_loud_never_a_fallback() {
    struct PoisonGuard;
    impl Drop for PoisonGuard {
        fn drop(&mut self) {
            squeezefs::data_custody::test_clear_poison();
        }
    }
    let _poison = PoisonGuard;

    let fx = Fixture::new("ddwfence").await;
    fx.salt_inos(5).await;
    let (ino, mut want) = fx.durable_striped("fence.bin", 4, 0x20).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);

        // Fence the mount: poison process data-plane custody — the D0
        // fail-stop face `authorize_dma` (THE authorization door the
        // engine's `authorize_zc_store` passes) refuses on. The device
        // probe seam (`set_fence_signal`) is unusable on a full fixture:
        // `set_meta_backend` already installed the real D0 probe in that
        // OnceLock, and poisoning is exactly what its first latch
        // observation performs anyway.
        squeezefs::data_custody::poison("test: il-direct-write fence rail");

        let before = snap();
        let p = pattern(4096, 0xC1);
        let r = session.ring_pwrite(binding, 3 * BS + 8192, &p);
        assert!(
            r < 0,
            "RES-6: a fenced daemon must refuse the direct write with an \
             errno to the client, got {r}"
        );
        let after = snap();
        assert_eq!(
            delta!(after, before, dd_fence_refusals),
            1,
            "the fence refusal is counted (must-stay-0 tripwire class)"
        );
        assert!(
            delta!(after, before, data_fence_refusals) >= 1,
            "the refusal passed THE authorization door (data_custody)"
        );
        assert_eq!(
            delta!(after, before, dd_serves),
            0,
            "nothing served while fenced"
        );
        assert_eq!(
            delta!(after, before, handoffs),
            0,
            "NEVER a silent fallback to a second submission path"
        );
    });
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// (c): purge-before-ACK — the stale-serve law
// ---------------------------------------------------------------------------

/// When the ring completion lands, the block key is already absent from
/// the read tiers (`purge_block_key`'s sweep + the whole-file LRU drops
/// run BEFORE `completion.complete` — the patch path's invalidation set
/// and ordering, verbatim).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_write_purges_read_tiers_before_the_ack() {
    let fx = Fixture::new("ddwpurge").await;
    fx.salt_inos(7).await;
    let (ino, mut want) = fx.durable_striped("purge.bin", 4, 0x30).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let bk = fx.block_key(ino, 2).await;
    let path = squeezefs::keys::inode_path(ino);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 3 * BS + 8192]);

        // Seed STALE tier state under the block key + the whole-file path.
        let stale = bytes::Bytes::from(vec![0xDDu8; BS as usize]);
        fx.fs.router.cache.read_lru.put(&bk, stale.clone());
        fx.fs.router.cache.hot_block.put(&bk, stale.clone());
        fx.fs.router.cache.write_lru.put(&path, stale.clone());
        fx.fs.router.cache.read_lru.put(&path, stale);

        let before = snap();
        let p = pattern(4096, 0xD1);
        let off = 2 * BS + 8192;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(
            r, 4096,
            "the seeded tiers must not make the write fall back"
        );
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        assert_eq!(
            delta!(snap(), before, dd_serves),
            1,
            "the purge law is only proven on a DIRECT serve"
        );

        // The ACK has landed ⇒ the purge already ran (ordering, not
        // eventual consistency): every tier arm is empty NOW.
        assert!(
            fx.fs.router.cache.read_lru.get_no_promote(&bk).is_none(),
            "read LRU must not hold the block key after the ACK"
        );
        assert!(
            fx.fs.router.cache.hot_block.get_no_promote(&bk).is_none(),
            "hot tier must not hold the block key after the ACK"
        );
        assert!(
            fx.fs.router.cache.write_lru.get_no_promote(&path).is_none(),
            "stale whole-file write_lru snapshot must be dropped"
        );
        assert!(
            fx.fs.router.cache.read_lru.get_no_promote(&path).is_none(),
            "stale whole-file read_lru snapshot must be dropped"
        );
    });

    // And the read path serves the NEW bytes.
    let got = fx.fuse_read(ino, 2 * BS, BS as u32).await;
    assert_eq!(
        got,
        want[(2 * BS) as usize..(3 * BS) as usize].to_vec(),
        "post-ACK reads serve the patched bytes, never a stale tier"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// (e): the A/B lever — =0 routes everything to the handoff path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_routes_every_write_to_the_handoff_path() {
    let fx = Fixture::new("ddwlever").await;
    fx.salt_inos(9).await;
    let (ino, mut want) = fx.durable_striped("lever.bin", 4, 0x40).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        // Prove the lane works first (so "0 serves" below is the lever,
        // not a broken lane).
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);

        set_il_direct_write_enabled(false);
        let before = snap();
        let writes: &[(u64, usize, u8)] = &[
            (0, 4096, 0xE1),
            (3 * BS + 8192, 4096, 0xE2),
            (BS + 32768, 4096, 0xE3),
        ];
        for &(off, len, tag) in writes {
            let p = pattern(len, tag);
            let r = session.ring_pwrite(binding, off, &p);
            assert_eq!(r, len as i64, "the A0 control must still ack");
            want[off as usize..off as usize + len].copy_from_slice(&p);
        }
        let after = snap();
        set_il_direct_write_enabled(true);

        assert_eq!(
            delta!(after, before, dd_serves),
            0,
            "SQUEEZEFS_IL_DIRECT_WRITE=0 ⇒ serves stay 0 (the A0 control)"
        );
        assert_eq!(
            delta!(after, before, handoffs),
            writes.len() as u64,
            "the handoff path carries the whole row under the lever"
        );
        // The lever is silent: no ledger class moves (the knob is the A/B
        // control, never an 'ineligible' verdict).
        assert_eq!(delta!(after, before, dd_shape), 0);
        assert_eq!(delta!(after, before, dd_custody), 0);
        assert_eq!(delta!(after, before, dd_overlay), 0);
        assert_eq!(delta!(after, before, dd_backend), 0);
        assert_eq!(delta!(after, before, dd_align), 0);
    });

    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 0, (4 * BS) as u32).await;
    assert_eq!(got, want, "the A0 control stays byte-exact");
    fx.shutdown();
}
