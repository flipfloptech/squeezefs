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
//!   Custody refusals attribute PER CAUSE (`lease` / `range` /
//!   `block_lock` / `killpriv` / `fence_backoff` — the rig ran the
//!   retired bundled `..._custody` counter at 17 % of ops, and the fix
//!   design needs to know WHICH cause dominates): closure/silence
//!   assertions read the SUM of the five, the per-arm rails below each
//!   assert their own arm.
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
        // 64 slots/entries: the block-conveyor train-bound rail packs one
        // leader + 33 followers in flight at once (bound = one ring
        // depth's worth = 32); the pre-conveyor suites use slot 0 only.
        ring_entries: 64,
        slots: 64,
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

    /// Stage + submit one pwrite on `slot_idx` WITHOUT waiting (the
    /// conveyor rails submit trains of in-flight same-block ops): each
    /// slot owns its private 4 KiB arena window (`slot_idx × 4096` —
    /// page-aligned, so the direct-arena DMA leg stays engaged) and the
    /// staged bytes must stay untouched until the op completes (the
    /// arena is the DMA SOURCE). Returns the claim generation for
    /// [`Self::wait_slot`].
    fn stage_pwrite(&self, slot_idx: u32, binding: u64, offset: u64, data: &[u8]) -> u64 {
        assert!(data.len() <= 4096, "conveyor-rail ops are 4 KiB class");
        let arena_off = u64::from(slot_idx) * 4096;
        self.arena_write(arena_off, data);
        let slot = self.slot(slot_idx);
        let gen = slot.core.try_claim().expect("slot must be FREE");
        slot.publish_descriptor(&SlotDescriptor {
            op: OP_WRITE,
            flags: 0,
            binding,
            offset,
            len: data.len() as u32,
            arena_off,
        });
        slot.core.publish_submitted();
        assert!(self.ring().push(slot_idx), "ring must accept");
        self.header().doorbell.fetch_add(1, Ordering::Release);
        futex_wake(&self.header().doorbell, 1);
        gen
    }

    /// Bounded wait for `slot_idx`'s completion; returns the slot result
    /// verbatim (bytes or -errno) and releases the slot.
    fn wait_slot(&self, slot_idx: u32, gen: u64, what: &str) -> i64 {
        let slot = self.slot(slot_idx);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !slot.core.is_done_for(gen) {
            assert!(Instant::now() < deadline, "{what}: op never completed");
            std::hint::spin_loop();
        }
        let r = slot.result();
        slot.core.release();
        r
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
    dd_lease: u64,
    dd_range: u64,
    dd_block_lock: u64,
    dd_killpriv: u64,
    dd_fence_backoff: u64,
    dd_overlay: u64,
    dd_backend: u64,
    dd_align: u64,
    patch_writes: u64,
    patch_write_bytes: u64,
    handoffs: u64,
    ops_write: u64,
    bytes_in: u64,
    data_fence_refusals: u64,
    /// Block-conveyor ledger (perf/ddw-block-conveyor): ops parked on an
    /// open same-block train / parked ops popped back out (any
    /// disposition). Closure law: `parks ≡ redrives` at quiesce — a
    /// parked op whose train ends without a pop is a WEDGE.
    dd_parks: u64,
    dd_redrives: u64,
}

impl Snap {
    /// The retired bundled `ipc_dd_write_ineligible_custody` population —
    /// the SUM of its five split arms. Ledger-closure/silence assertions
    /// read this; the per-arm rails assert their specific cause.
    fn dd_custody_sum(&self) -> u64 {
        self.dd_lease
            + self.dd_range
            + self.dd_block_lock
            + self.dd_killpriv
            + self.dd_fence_backoff
    }
}

fn snap() -> Snap {
    let l = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Snap {
        dd_serves: l(&METRICS.ipc_dd_write_serves),
        dd_bytes: l(&METRICS.ipc_dd_write_bytes),
        dd_fence_refusals: l(&METRICS.ipc_dd_write_fence_refusals),
        dd_shape: l(&METRICS.ipc_dd_write_ineligible_shape),
        dd_lease: l(&METRICS.ipc_dd_write_ineligible_lease),
        dd_range: l(&METRICS.ipc_dd_write_ineligible_range),
        dd_block_lock: l(&METRICS.ipc_dd_write_ineligible_block_lock),
        dd_killpriv: l(&METRICS.ipc_dd_write_ineligible_killpriv),
        dd_fence_backoff: l(&METRICS.ipc_dd_write_ineligible_fence_backoff),
        dd_overlay: l(&METRICS.ipc_dd_write_ineligible_overlay),
        dd_backend: l(&METRICS.ipc_dd_write_ineligible_backend),
        dd_align: l(&METRICS.ipc_dd_write_ineligible_align),
        patch_writes: l(&METRICS.patch_writes),
        patch_write_bytes: l(&METRICS.patch_write_bytes),
        handoffs: l(&METRICS.ipc_async_handoffs),
        ops_write: l(&METRICS.ipc_ops_write),
        bytes_in: l(&METRICS.ipc_bytes_in),
        data_fence_refusals: l(&METRICS.data_dma_fence_refusals),
        dd_parks: l(&METRICS.ipc_dd_write_block_parks),
        dd_redrives: l(&METRICS.ipc_dd_write_park_redrives),
    }
}

/// RAII over the conveyor rails' determinism seam
/// (`ipc_direct::set_test_ddw_cqe_hold`): while held, the dd engine's CQE
/// consumption pauses — the leader's guard tenure is pinned open so
/// followers deterministically observe a lane-held block. Drop REOPENS
/// the gate even on a panicking assertion, so a red run can never wedge
/// the engine's shutdown drain behind a closed gate.
struct CqeHold;

impl CqeHold {
    fn hold() -> CqeHold {
        squeezefs::ipc_direct::set_test_ddw_cqe_hold(true);
        CqeHold
    }
    fn release(self) {
        drop(self);
    }
}

impl Drop for CqeHold {
    fn drop(&mut self) {
        squeezefs::ipc_direct::set_test_ddw_cqe_hold(false);
    }
}

/// Bounded counter wait (never a sleep-synchronized assertion): spin
/// until `probe()` reaches `target` or the deadline names the failure.
fn wait_counter_at_least(probe: &dyn Fn() -> u64, target: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe() < target {
        assert!(
            Instant::now() < deadline,
            "{what}: counter never reached {target} (now {})",
            probe()
        );
        std::thread::yield_now();
    }
}

macro_rules! delta {
    ($after:expr, $before:expr, $field:ident) => {
        $after.$field - $before.$field
    };
}

/// Warm the lane: the FIRST ring write may legitimately fall back once
/// (cold killpriv-clean latch on an unprivileged peer, counted on the
/// `killpriv` arm — the clearing obligation is async metadata work the
/// sync probe must not pay; the handler latches it). After one warm-up,
/// eligible writes MUST serve direct. Returns when a probe write
/// direct-serves.
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
        // Ledger silence: an all-eligible window counts no fallback clause
        // (the custody face reads the SUM of its five split arms).
        assert_eq!(delta!(after, before, dd_shape), 0);
        assert_eq!(
            after.dd_custody_sum() - before.dd_custody_sum(),
            0,
            "no custody arm (lease/range/block_lock/killpriv/fence_backoff) \
             may move on an all-eligible window"
        );
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

        // lease: drop the cached lease — the sync probe must not pay the
        // async lease acquisition; the op demotes and the handler
        // re-acquires. The refusal attributes to the LEASE arm exactly —
        // no sibling custody arm may absorb it.
        fx.fs.invalidate_local_lease(ino);
        let before = snap();
        let p = pattern(4096, 0xB3);
        let off = 2 * BS + 24576;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096);
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();
        assert!(
            delta!(after, before, dd_lease) >= 1,
            "a lease-less ino counts the LEASE arm (an await-needing \
             custody shape is INELIGIBLE — v1 stays synchronous)"
        );
        assert_eq!(
            delta!(after, before, dd_range)
                + delta!(after, before, dd_block_lock)
                + delta!(after, before, dd_killpriv)
                + delta!(after, before, dd_fence_backoff),
            0,
            "exactly one custody arm per refusal: the lease miss must not \
             leak into a sibling arm (per-cause attribution is the whole \
             point of the split)"
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
// (a) continued: the split custody arms, one rail per cause
//
// The retired bundled `..._custody` counter ran 17 % of ops on the rig
// (6.78 M/38.9 M — .benchmarks/2026-08-11-op-registry-shard.md) across
// FIVE distinct refusal causes; the rails below pin each cause to its own
// counter so the field number attributes.
//
// The RANGE arm (`ipc_dd_write_ineligible_range` — the W1 clause-7
// `span_range_shared` refusal) has NO reachable seam in this harness by
// construction, not by omission: the probe consults the clause with the
// ino's CACHED whole-file lease's token, and whole-file EXCLUSIVE custody
// conflicts with every byte-range acquire in the shipped lock table
// (`FileCustody::conflicts` checks `wholes` first; S9's
// `adopt_remote_grant` refuses conflicting adoptions too), so "cached
// whole-file lease AND a live overlapping range grant" is unconstructible
// without new active_leases seams. The arm exists for the S9 co-writer
// posture (an adopted byte-range lease serving as the cached custody).
// The predicate itself is pinned by
// `tests/dlm_range_custody_tests.rs::span_range_shared_classifies_custody`;
// HERE the arm is covered by the ledger-closure/silence sums
// (`Snap::dd_custody_sum`) and by every sibling rail's
// exactly-one-arm-per-refusal assertion.
// ---------------------------------------------------------------------------

/// `block_lock` arm: an otherwise-eligible write whose block's
/// `BLOCK_FLUSH_LOCKS` stripe is HELD demotes (the probe's ONE
/// non-blocking `try_lock` — the serve_read lock-demotion posture),
/// counts exactly the block_lock arm, and serves 0 direct. The test IS
/// the mid-flight writer: it holds the block's guard across the ring
/// write's probe and releases it once the arm has counted (the demoted
/// handler write then takes the same lock and lands byte-exact).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contended_block_lock_counts_its_own_arm_and_demotes() {
    let fx = Fixture::new("ddwblock").await;
    fx.salt_inos(11).await;
    let (ino, mut want) = fx.durable_striped("blklock.bin", 4, 0x50).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 3 * BS + 8192]);

        // Hold block 2's flush lock — the probe's try_lock must observe
        // the contention (a writer mid-flight on the block).
        let lock = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS.get_lock(ino, 2);
        let guard = lock.try_lock().expect("the test takes block 2's guard");

        let before = snap();
        let base_block_lock = before.dd_block_lock;
        // The guard travels into a watcher thread: the demoted handler
        // write blocks on this same stripe, so the guard must drop only
        // AFTER the probe counted the arm (deadline-bounded — never a
        // sleep-synchronized release).
        let watcher = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let moved = loop {
                if METRICS
                    .ipc_dd_write_ineligible_block_lock
                    .load(Ordering::Relaxed)
                    > base_block_lock
                {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::yield_now();
            };
            drop(guard);
            moved
        });

        let p = pattern(4096, 0xF1);
        let off = 2 * BS + 8192;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "the demoted write must still ack via the handler");
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        assert!(
            watcher.join().expect("watcher thread must not panic"),
            "the probe never counted the block_lock arm while the test \
             held the block's BLOCK_FLUSH_LOCKS guard"
        );

        let after = snap();
        assert_eq!(
            delta!(after, before, dd_block_lock),
            1,
            "a contended block lock counts exactly the block_lock arm"
        );
        assert_eq!(
            delta!(after, before, dd_parks),
            0,
            "a NON-lane holder must never park a follower (the conveyor \
             trains only behind the dd-write lane's own guard tenure)"
        );
        assert_eq!(
            delta!(after, before, dd_lease)
                + delta!(after, before, dd_range)
                + delta!(after, before, dd_killpriv)
                + delta!(after, before, dd_fence_backoff),
            0,
            "exactly one custody arm per refusal (attribution law)"
        );
        assert_eq!(delta!(after, before, dd_serves), 0, "no direct serve");
        assert_eq!(
            delta!(after, before, handoffs),
            1,
            "the contended op rides the sever→handoff path"
        );
    });

    fx.fsync(ino).await;
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 2 * BS, BS as u32).await;
    assert_eq!(
        got,
        want[(2 * BS) as usize..(3 * BS) as usize].to_vec(),
        "the demoted write stays byte-exact"
    );
    fx.shutdown();
}

/// `killpriv` arm: a `kill_priv`-flagged binding whose ino's
/// killpriv-clean latch is NOT held is custody-INELIGIBLE (the clearing
/// obligation is async metadata work the sync probe must not pay — the
/// VFS privs-before-write order). A chmod re-arms the obligation
/// (`setattr` removes the latch after its commit); the next eligible
/// ring write counts exactly the killpriv arm and falls back, the
/// handler re-latches, and the write after that direct-serves again.
///
/// The binding's `kill_priv` class is the HELLO-time
/// `peer_kill_priv(uid, pid)` of THIS test process: an exempt peer
/// (root / CAP_FSETID) makes the arm structurally unreachable — that
/// branch asserts the exemption face instead (the flagless write
/// direct-serves with the arm silent), so the test is deterministic on
/// both identities without an environment skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlatched_killpriv_counts_its_own_arm_then_relatches() {
    let fx = Fixture::new("ddwkpriv").await;
    fx.salt_inos(13).await;
    let (ino, mut want) = fx.durable_striped("kpriv.bin", 4, 0x60).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // The same classification the host computed at HELLO for this peer.
    let kill = squeezefs::ipc_host::peer_kill_priv(unsafe { libc::getuid() }, std::process::id());

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);
    });

    // Re-arm the obligation: a mode-touching setattr removes the latch
    // AFTER its commit (the latch's write-then-remove race law).
    fx.fs
        .setattr(
            req(),
            ino,
            None,
            fuse3::SetAttr {
                mode: Some(0o644),
                ..Default::default()
            },
        )
        .await
        .expect("chmod must succeed");
    assert!(
        !fx.fs.killpriv_clean_holds(ino),
        "a mode-touching setattr must drop the killpriv-clean latch"
    );

    tokio::task::block_in_place(|| {
        let before = snap();
        let p = pattern(4096, 0xF2);
        let off = 3 * BS + 16384;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "the write must ack on either identity");
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();

        if kill {
            assert_eq!(
                delta!(after, before, dd_killpriv),
                1,
                "an unlatched killpriv obligation counts exactly the \
                 killpriv arm (one fallback lets the handler clear privs \
                 BEFORE the data lands)"
            );
            assert_eq!(
                delta!(after, before, dd_lease)
                    + delta!(after, before, dd_range)
                    + delta!(after, before, dd_block_lock)
                    + delta!(after, before, dd_fence_backoff),
                0,
                "exactly one custody arm per refusal (attribution law)"
            );
            assert_eq!(delta!(after, before, dd_serves), 0, "no direct serve");
            assert_eq!(delta!(after, before, handoffs), 1);

            // The fallback latched the ino clean: steady state pays the
            // contains-check and direct-serves again.
            let before = snap();
            let p = pattern(4096, 0xF3);
            let off = BS + 32768;
            let r = session.ring_pwrite(binding, off, &p);
            assert_eq!(r, 4096);
            want[off as usize..off as usize + 4096].copy_from_slice(&p);
            let after = snap();
            assert_eq!(
                delta!(after, before, dd_serves),
                1,
                "the latch is re-held after ONE fallback: the next \
                 eligible write direct-serves"
            );
            assert_eq!(delta!(after, before, dd_killpriv), 0);
        } else {
            // Exempt peer (root / CAP_FSETID): the binding never carries
            // the obligation, so the un-latched ino direct-serves and the
            // arm stays structurally silent.
            assert_eq!(
                delta!(after, before, dd_serves),
                1,
                "an exempt peer's write carries no killpriv obligation"
            );
            assert_eq!(delta!(after, before, dd_killpriv), 0);
        }
    });

    fx.fsync(ino).await;
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 0, want.len() as u32).await;
    assert_eq!(got, want, "both identities stay byte-exact");
    fx.shutdown();
}

/// `fence_backoff` arm: the probe passes (the prelude has NO refcount
/// screen — sole ownership is the ENGINE's §5.1 fence to prove), but
/// `begin_patch_sole_owner`'s refcount re-check observes a clone pin ⇒
/// the engine re-stabilizes the word (content never changed), counts
/// exactly the fence_backoff arm, and falls back to the handler's CoW
/// arm — never an in-place scribble on a shared block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_pinned_block_counts_the_fence_backoff_arm_and_cows() {
    let fx = Fixture::new("ddwpin").await;
    fx.salt_inos(15).await;
    let (ino, mut want) = fx.durable_striped("pin.bin", 4, 0x70).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);
    });

    // Pin block 3 clone-shared (refcount 1 → 2): the inplace/refcount
    // test seam — the §5.1 sole-owner predicate must now fail.
    let bk = fx.block_key(ino, 3).await;
    assert!(
        fx.fs.router.backend_router.increment_refcount(&bk),
        "clone pin on a live block must succeed"
    );

    tokio::task::block_in_place(|| {
        let before = snap();
        let p = pattern(4096, 0xF4);
        let off = 3 * BS + 24576;
        let r = session.ring_pwrite(binding, off, &p);
        assert_eq!(r, 4096, "the CoW fallback must still ack the write");
        want[off as usize..off as usize + 4096].copy_from_slice(&p);
        let after = snap();

        assert_eq!(
            delta!(after, before, dd_fence_backoff),
            1,
            "a clone-pinned block counts exactly the fence_backoff arm \
             (the engine-side §5.1 back-off, not a prelude class)"
        );
        assert_eq!(
            delta!(after, before, dd_lease)
                + delta!(after, before, dd_range)
                + delta!(after, before, dd_block_lock)
                + delta!(after, before, dd_killpriv),
            0,
            "exactly one custody arm per refusal (attribution law)"
        );
        assert_eq!(delta!(after, before, dd_serves), 0, "no direct serve");
        assert_eq!(
            delta!(after, before, handoffs),
            1,
            "the backed-off op rides the sever→handoff path (re-stabilized \
             word, unchanged content — the handler re-runs the whole patch \
             protocol and takes its CoW arm)"
        );
        assert_eq!(
            delta!(after, before, patch_writes),
            0,
            "no in-place patch may land on a clone-shared block"
        );
    });

    // The CoW displaced the mapping; the write is byte-exact and durable.
    fx.fsync(ino).await;
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 3 * BS, BS as u32).await;
    assert_eq!(
        got,
        want[(3 * BS) as usize..(4 * BS) as usize].to_vec(),
        "the CoW fallback stays byte-exact"
    );
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
        // control, never an 'ineligible' verdict) — the custody face reads
        // the SUM of its five split arms.
        assert_eq!(delta!(after, before, dd_shape), 0);
        assert_eq!(after.dd_custody_sum() - before.dd_custody_sum(), 0);
        assert_eq!(delta!(after, before, dd_overlay), 0);
        assert_eq!(delta!(after, before, dd_backend), 0);
        assert_eq!(delta!(after, before, dd_align), 0);
    });

    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 0, (4 * BS) as u32).await;
    assert_eq!(got, want, "the A0 control stays byte-exact");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// The per-block follower conveyor (perf/ddw-block-conveyor): same-block
// dd writes TRAIN on the guard holder instead of falling back to the
// handler and then blocking on the same stripe anyway (the rig's 99.8%
// block_lock attribution: 6.90M of 39.2M ops at qd32 same-block).
// Determinism seam: `CqeHold` pins the leader's guard tenure open (the
// engine's CQE consumption pauses), so followers observe a lane-held
// block on every run — never a sleep-synchronized race.
// ---------------------------------------------------------------------------

/// Rail (1): two-plus eligible writes to the SAME block while the lane
/// holds the guard — the followers PARK (never the block_lock fallback),
/// the CQE postlude re-drives them under the already-held guard in FIFO
/// order, every op direct-serves byte-exact, and the ledger closes:
/// serves == ops, handoffs == 0, parks == redrives == followers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_block_followers_park_and_train_on_the_guard_holder() {
    let fx = Fixture::new("ddwtrain").await;
    fx.salt_inos(19).await;
    let (ino, mut want) = fx.durable_striped("train.bin", 4, 0x60).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 3 * BS + 8192]);

        // Pin the leader's tenure open, then submit leader + 3 followers
        // to block 2 (scattered, pairwise non-adjacent in dequeue order).
        let hold = CqeHold::hold();
        let before = snap();
        let writes: &[(u64, u8)] = &[
            (2 * BS, 0xA1),
            (2 * BS + 8192, 0xA2),
            (2 * BS + 16384, 0xA3),
            (2 * BS + 24576, 0xA4),
        ];
        let mut gens = Vec::new();
        for (i, &(off, tag)) in writes.iter().enumerate() {
            let p = pattern(4096, tag);
            gens.push(session.stage_pwrite(i as u32, binding, off, &p));
            want[off as usize..off as usize + 4096].copy_from_slice(&p);
        }
        // The followers must PARK while the leader's guard is pinned —
        // the conveyor's whole contract (red pre-conveyor: this deadline
        // fires, the ops fell back on the block_lock arm instead).
        wait_counter_at_least(
            &|| METRICS.ipc_dd_write_block_parks.load(Ordering::Relaxed),
            before.dd_parks + 3,
            "same-block followers never parked on the lane-held guard",
        );
        hold.release();
        for (i, gen) in gens.iter().enumerate() {
            let r = session.wait_slot(i as u32, *gen, "train op");
            assert_eq!(r, 4096, "train op {i} must ack its full length");
        }
        let after = snap();

        assert_eq!(
            delta!(after, before, dd_serves),
            writes.len() as u64,
            "the whole train direct-serves (leader + re-driven followers)"
        );
        assert_eq!(
            delta!(after, before, handoffs),
            0,
            "a trained same-block burst pays zero handler handoffs"
        );
        assert_eq!(
            delta!(after, before, dd_block_lock),
            0,
            "lane-held contention parks — the block_lock arm stays for \
             foreign holders only"
        );
        assert_eq!(delta!(after, before, dd_parks), 3, "three followers parked");
        assert_eq!(
            delta!(after, before, dd_redrives),
            3,
            "every parked follower was popped back out (closure: parks == \
             redrives — a parked op left behind is a WEDGE)"
        );
        assert_eq!(
            delta!(after, before, ops_write),
            writes.len() as u64,
            "engagement law: serves + handoffs accounts for the row"
        );
    });

    // Byte parity through the kernel venue, device-honest.
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, 2 * BS, BS as u32).await;
    assert_eq!(
        got,
        want[(2 * BS) as usize..(3 * BS) as usize].to_vec(),
        "trained writes stay byte-exact in FIFO order"
    );
    fx.shutdown();
}

/// Rail (2): the train BOUND (one ring depth's worth — Q_DEPTH_DESIRED
/// = 32 ops per guard tenure, so a fold/flush waiter parks behind at
/// most one train). Pack MORE followers than the bound: the queue caps
/// at the bound (the overflow op falls back on the block_lock arm), the
/// tenure closes at 32 served ops, residual followers re-drive through
/// the normal probe from the reap thread — never dropped — and the
/// ledger closes: parks == redrives, serves + handoffs == ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn train_bound_closes_the_tenure_and_residuals_redrive() {
    let fx = Fixture::new("ddwbound").await;
    fx.salt_inos(21).await;
    let (ino, mut want) = fx.durable_striped("bound.bin", 4, 0x70).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[2 * BS + 16384, 3 * BS + 8192]);

        // 34 ops on block 1 = 1 leader + 32 parked (the queue cap) + 1
        // overflow fallback. Alternating disjoint windows A/B keep every
        // consecutive pair non-adjacent AND make the final content
        // last-writer-per-window regardless of where the boundary ops
        // (overflow / residual) execute relative to the train.
        const OPS: u32 = 34;
        let win_a = BS;
        let win_b = BS + 8192;
        let hold = CqeHold::hold();
        let before = snap();
        let mut gens = Vec::new();
        for i in 0..OPS {
            let off = if i % 2 == 0 { win_a } else { win_b };
            let p = pattern(4096, 0x80 + i as u8);
            gens.push(session.stage_pwrite(i, binding, off, &p));
            want[off as usize..off as usize + 4096].copy_from_slice(&p);
        }
        wait_counter_at_least(
            &|| METRICS.ipc_dd_write_block_parks.load(Ordering::Relaxed),
            before.dd_parks + 32,
            "the queue must park exactly one ring depth's worth",
        );
        hold.release();
        for (i, gen) in gens.iter().enumerate() {
            let r = session.wait_slot(i as u32, *gen, "bound op");
            assert_eq!(r, 4096, "bound op {i} must ack its full length");
        }
        let after = snap();

        assert_eq!(
            delta!(after, before, dd_parks),
            32,
            "the park queue is bounded at one ring depth's worth"
        );
        assert_eq!(
            delta!(after, before, dd_redrives),
            32,
            "every parked follower left the queue (closure: parks == \
             redrives — residuals past the tenure bound re-drive, never \
             strand)"
        );
        // The tenure bound guarantees at least the bound's worth of ops
        // served under the train; the boundary ops (overflow + any
        // residual that lost the post-release lock race to the queued
        // handler waiter) legitimately land either way.
        assert!(
            delta!(after, before, dd_serves) >= 32,
            "at least one full tenure's worth of ops direct-serves \
             (got {})",
            delta!(after, before, dd_serves)
        );
        assert_eq!(
            delta!(after, before, dd_serves) + delta!(after, before, handoffs),
            u64::from(OPS),
            "ledger closure: every op is exactly one of served / handed off"
        );
        assert_eq!(
            delta!(after, before, dd_block_lock),
            delta!(after, before, handoffs),
            "every handoff in this row is a block_lock-attributed fallback \
             (overflow past the bounded queue, or a residual that lost the \
             post-release lock race)"
        );
        assert_eq!(
            delta!(after, before, ops_write),
            u64::from(OPS),
            "engagement law over the whole row"
        );
    });

    fx.fsync(ino).await;
    fx.purge_read_tiers(ino).await;
    let got = fx.fuse_read(ino, BS, BS as u32).await;
    assert_eq!(
        got,
        want[BS as usize..(2 * BS) as usize].to_vec(),
        "last-writer-per-window content holds across the tenure boundary"
    );
    fx.shutdown();
}

/// Rail (4): park never strands — a teardown initiated while a follower
/// is PARKED completes (or fails loudly with an errno) every parked op:
/// the engine's shutdown drain processes the guard holder's CQE, the
/// conveyor pops the follower, and the ledger closes (parks ==
/// redrives). Silence — a completion that never lands — is the wedge
/// this rail exists to make impossible (the queue-entry-co-owns-guards
/// law's conveyor face).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_completes_parked_followers_never_silence() {
    let fx = Fixture::new("ddwteardown").await;
    fx.salt_inos(23).await;
    let (ino, mut want) = fx.durable_striped("teardown.bin", 4, 0x90).await;
    let dir = tempfile::tempdir().unwrap();
    let fd = rw_standin(&fx, &dir, "standin.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    tokio::task::block_in_place(|| {
        warm_direct_lane(&session, binding, &mut want, &[BS + 16384, 2 * BS + 8192]);

        let hold = CqeHold::hold();
        let before = snap();
        let p1 = pattern(4096, 0xB1);
        let p2 = pattern(4096, 0xB2);
        let g1 = session.stage_pwrite(0, binding, 3 * BS + 8192, &p1);
        let g2 = session.stage_pwrite(1, binding, 3 * BS + 24576, &p2);
        want[(3 * BS + 8192) as usize..(3 * BS + 8192) as usize + 4096].copy_from_slice(&p1);
        want[(3 * BS + 24576) as usize..(3 * BS + 24576) as usize + 4096].copy_from_slice(&p2);
        wait_counter_at_least(
            &|| METRICS.ipc_dd_write_block_parks.load(Ordering::Relaxed),
            before.dd_parks + 1,
            "the follower never parked on the lane-held guard",
        );

        // Teardown WHILE the follower is parked: shutdown blocks on the
        // engine's drain (the gate holds the leader's CQE), so the
        // parked state provably overlaps the teardown.
        let host = Arc::clone(&fx.host);
        let shutdown = std::thread::spawn(move || host.shutdown());
        // Race-widener only (assertions below are completion-based,
        // never sleep-synchronized): let the shutdown reach its drain.
        std::thread::sleep(Duration::from_millis(100));
        hold.release();
        shutdown
            .join()
            .expect("shutdown must complete once the drain runs");

        // NEVER SILENCE: both ops complete — served through the drain
        // (4096) or failed LOUD with an errno; a hang here is the wedge.
        let r1 = session.wait_slot(0, g1, "teardown leader");
        let r2 = session.wait_slot(1, g2, "teardown parked follower");
        assert!(
            r1 == 4096 || r1 < 0,
            "leader must ack or fail loud, got {r1}"
        );
        assert!(
            r2 == 4096 || r2 < 0,
            "parked follower must ack or fail loud, got {r2}"
        );

        let after = snap();
        assert_eq!(
            delta!(after, before, dd_parks),
            1,
            "exactly the follower parked"
        );
        assert_eq!(
            delta!(after, before, dd_redrives),
            1,
            "the parked follower was popped through the teardown (closure: \
             parks == redrives — never stranded in the queue)"
        );
    });
    // fx.shutdown() is idempotent with the raced teardown above.
    fx.shutdown();
}
