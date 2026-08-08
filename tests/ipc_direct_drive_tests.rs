//! DIALED P1 — direct-drive ranged reads on the shim miss path
//! (`docs/design-preload-interception.md` §5.5/§12 pre-agreed fallback
//! shape; the `.benchmarks/2026-07-26-ipc-handoff-economy.md` §7
//! residual). The contract under test:
//!
//! - On a `direct_device_true` mount, an O_DIRECT binding's ranged
//!   (single-block, device-class 4–64 KiB) read of a striped,
//!   whole-block-mapped, non-overlay block is served by the IPC service
//!   thread submitting the device read DIRECTLY on an ipc-host-owned
//!   io_uring — **no task, no tokio, no handler** — completing the ring
//!   slot from the CQE (`ipc_direct_drive_serves`), with the governed
//!   read accounting intact (`ranged_reads`, `read_device_true_reads`,
//!   `ipc_ops_read` — the engagement instruments stay observable).
//! - The policy prelude is exact, synchronous, and complete: any miss
//!   (shape, RAM-metadata residency, layout/hole/decoration, overlay or
//!   staged-sibling or extent-record presence, backend health) falls
//!   back to the existing handler path — fallback-is-correctness, and
//!   the decision ledger (`ipc_direct_ineligible_*`) records why.
//! - Post-DMA, the 795 moving-custody protocol governs the serve: the
//!   prelude snapshot (binding key + custody epoch + fill incarnation)
//!   must revalidate at the CQE or the op falls back to the handler
//!   (`ipc_direct_drive_fallbacks_post`).
//!
//! DIALED P1.5 (2026-07-27, section 7 below): on DEFAULT mounts the
//! prelude runs the R1b admission decision synchronously — tier probe
//! (hit ⇒ sync fast path, unchanged), ghost touch recording, governor
//! token check (non-reserving peek). DENIED ⇒ direct-drive (+ denial
//! accounting, no cooldown); GRANTED ⇒ handler path (admission fetch +
//! publish machinery unchanged). O_DIRECT bindings only.
//!
//! Harness: the preload_parity_tests raw-protocol client (no kernel in
//! the loop; the daemon side is the REAL host → service → sink path).

use squeezefs::fuse_client::{
    block_custody_epoch, bump_block_custody_epoch, IpcDirectIneligible, METRICS,
};
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
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BS: u64 = 4 * 1024 * 1024; // router default block size

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

    let dlm = DlmClient::new().unwrap();
    let backing_temp = tempfile::NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new("ipc_direct_drive_tests")
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
            panic!(
                "harness bound an untranslated st_ino {} — register it in the InoMapSink",
                op.binding.ino
            );
        };
        op.binding.ino = fs_ino;
        SessionSink::serve_data(&self.inner, op, completion);
    }

    fn flush(&self) {
        // Forward the end-of-sweep hook (SessionSink::flush liveness
        // rule): direct-drive SQEs published during serve_data must
        // reach the kernel before the service thread parks.
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
            map: std::sync::Mutex::new(HashMap::new()),
        });
        let cfg = IpcHostConfig {
            socket_name: format!("sqz-il0-dd-{}-{}", std::process::id(), name),
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

    /// Shift this fixture's ino allocation by `n` (fresh v3 volumes hand
    /// out the same monotonic inos, and the local DLM lock map is
    /// process-global — without a per-test salt, test B's ino 2 contends
    /// on test A's still-TTL'd lease for the SAME key).
    async fn salt_inos(&self, n: usize) {
        for i in 0..n {
            let name = format!("salt{i}");
            self.fs
                .create(req(), 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                .await
                .expect("salt create");
        }
    }

    /// Create a fixture file and write a STRIPED layout: complete blocks
    /// are written highest-offset-first so the very first write already
    /// exceeds the staged threshold (`expected_new_size > block_size`).
    /// Callers must pass ≥ 2 blocks (a ≤-block-size file stays staged).
    async fn create_striped(&self, name: &str, blocks: &[u32]) -> u64 {
        assert!(
            blocks.len() >= 2 || blocks.iter().any(|b| *b >= 1),
            "a striped fixture needs size > block_size"
        );
        let create = self
            .fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .expect("create");
        let ino = create.attr.ino;
        let mut order: Vec<u32> = blocks.to_vec();
        order.sort_unstable_by(|a, b| b.cmp(a));
        for b in order {
            let data = deterministic_bytes(BS as usize, 100 + b as u64);
            let w = self
                .fs
                .write(req(), ino, 0, b as u64 * BS, bytes::Bytes::from(data), 0, 0)
                .await
                .expect("striped block write");
            assert_eq!(w.written as u64, BS);
        }
        self.fs.fsync(req(), ino, 0, false).await.expect("fsync");
        let meta = self
            .fs
            .router
            .metadata_cache
            .get(&ino)
            .expect("metadata cache entry after write");
        assert_eq!(meta.file_type, "striped", "fixture file must be striped");
        ino
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

/// Buffered (no O_DIRECT) stand-in: the binding's read class is captured
/// at the §5.2 fd screen, so a plain open yields `odirect = false` — the
/// hybrid-directive control (buffered ring reads must never direct-drive).
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
    assert!(fd >= 0, "buffered open on {} failed", path.display());
    // SAFETY: fresh owned fd.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Cold-fixture helper for the DEFAULT-mount rows: purge every block-key
/// tier (RAM LRU / hot / NVMe / GDS via the unified helper) so the sync
/// fast path genuinely misses, and return the striped map.
fn purge_tiers(fx: &Fixture, ino: u64) -> std::collections::HashMap<u32, String> {
    let meta = fx
        .fs
        .router
        .metadata_cache
        .get(&ino)
        .expect("metadata cache entry (prelude authority) must be resident");
    let map = meta.block_map.clone().expect("striped fixture map");
    for key in map.values() {
        fx.fs.router.cache.purge_block_key(key);
    }
    (*map).clone()
}

/// The fixture writer's deterministic block content (seed = 100 + block).
fn expect_bytes(block: u32, rel: usize, len: usize) -> Vec<u8> {
    deterministic_bytes(BS as usize, 100 + block as u64)[rel..rel + len].to_vec()
}

/// O_DIRECT stand-in on a real filesystem (the repo dir — /tmp is often
/// tmpfs, which refuses O_DIRECT); re-points the host's expected st_dev
/// and registers the ino translation.
fn odirect_standin(fx: &Fixture, dir: &tempfile::TempDir, name: &str, fs_ino: u64) -> OwnedFd {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("create O_DIRECT stand-in");
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
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_DIRECT) };
    assert!(
        fd >= 0,
        "O_DIRECT open on {} failed ({})",
        path.display(),
        std::io::Error::last_os_error()
    );
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

    fn arena_read(&self, off: u64, len: usize) -> Vec<u8> {
        assert!(off + len as u64 <= self.geometry.arena_bytes);
        // SAFETY: bounds asserted against the arena region.
        let src = unsafe { self.base.add((self.layout.arena_off + off) as usize) };
        let mut out = vec![0u8; len];
        // SAFETY: bounds checked above.
        unsafe { std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len) };
        out
    }

    fn submit_on(&self, slot_idx: u32, d: &SlotDescriptor) -> u64 {
        let slot = self.slot(slot_idx);
        let gen = slot.core.try_claim().expect("slot must be FREE");
        slot.publish_descriptor(d);
        // The shim's publish shape (reap-fanin 2026-08-08): stamp the
        // ingress instant between the descriptor and the submit publish.
        slot.stamp_ingress(squeezefs::mono_core::monotonic_stamp_ns_u32());
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

    /// One ring pread at `arena_off` (aligned or not — the test picks).
    fn ring_pread(
        &self,
        binding: u64,
        offset: u64,
        len: usize,
        arena_off: u64,
        what: &str,
    ) -> Vec<u8> {
        let r = {
            let gen = self.submit_on(
                0,
                &SlotDescriptor {
                    op: OP_READ,
                    flags: 0,
                    binding,
                    offset,
                    len: len as u32,
                    arena_off,
                },
            );
            self.wait_done(0, gen, what)
        };
        assert!(r >= 0, "{what}: ring read failed with {r}");
        self.arena_read(arena_off, r as usize)
    }

    /// `n` CONCURRENT ring preads on distinct slots/arena windows (the
    /// fusion suite's burst shape): submit all, then reap all. Returns
    /// `(offset, len, bytes)` per op, submit order.
    fn ring_pread_burst(&self, binding: u64, n: u32) -> Vec<(u64, usize, Vec<u8>)> {
        let len = 4096usize;
        let mut gens = Vec::with_capacity(n as usize);
        for i in 0..n {
            let offset = (16 + u64::from(i)) * 4096;
            let arena_off = u64::from(i) * 8192;
            let gen = self.submit_on(
                i,
                &SlotDescriptor {
                    op: OP_READ,
                    flags: 0,
                    binding,
                    offset,
                    len: len as u32,
                    arena_off,
                },
            );
            gens.push((i, offset, arena_off, gen));
        }
        gens.into_iter()
            .map(|(i, offset, arena_off, gen)| {
                let r = self.wait_done(i, gen, "burst read");
                assert!(r >= 0, "burst op {i} failed with {r}");
                (offset, len, self.arena_read(arena_off, r as usize))
            })
            .collect()
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

struct Deltas {
    dd_serves: u64,
    dd_bounces: u64,
    handoffs: u64,
    ops_read: u64,
    ranged: u64,
    device_true: u64,
    inel_shape: u64,
    inel_meta: u64,
    inel_layout: u64,
    inel_overlay: u64,
    inel_policy: u64,
    denials: u64,
    escalations: u64,
    fast_path: u64,
}

fn snap() -> Deltas {
    Deltas {
        dd_serves: METRICS.ipc_direct_drive_serves.load(Ordering::Relaxed),
        dd_bounces: METRICS.ipc_direct_drive_bounces.load(Ordering::Relaxed),
        handoffs: METRICS.ipc_async_handoffs.load(Ordering::Relaxed),
        ops_read: METRICS.ipc_ops_read.load(Ordering::Relaxed),
        ranged: METRICS.ranged_reads.load(Ordering::Relaxed),
        device_true: METRICS.read_device_true_reads.load(Ordering::Relaxed),
        inel_shape: METRICS.ipc_direct_ineligible_shape.load(Ordering::Relaxed),
        inel_meta: METRICS.ipc_direct_ineligible_meta.load(Ordering::Relaxed),
        inel_layout: METRICS.ipc_direct_ineligible_layout.load(Ordering::Relaxed),
        inel_overlay: METRICS
            .ipc_direct_ineligible_overlay
            .load(Ordering::Relaxed),
        inel_policy: METRICS.ipc_direct_ineligible_policy.load(Ordering::Relaxed),
        denials: METRICS
            .read_admission_governor_denials
            .load(Ordering::Relaxed),
        escalations: METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        fast_path: METRICS.ipc_fast_path_serves.load(Ordering::Relaxed),
    }
}

fn delta(before: &Deltas) -> Deltas {
    let now = snap();
    Deltas {
        dd_serves: now.dd_serves - before.dd_serves,
        dd_bounces: now.dd_bounces - before.dd_bounces,
        handoffs: now.handoffs - before.handoffs,
        ops_read: now.ops_read - before.ops_read,
        ranged: now.ranged - before.ranged,
        device_true: now.device_true - before.device_true,
        inel_shape: now.inel_shape - before.inel_shape,
        inel_meta: now.inel_meta - before.inel_meta,
        inel_layout: now.inel_layout - before.inel_layout,
        inel_overlay: now.inel_overlay - before.inel_overlay,
        inel_policy: now.inel_policy - before.inel_policy,
        denials: now.denials - before.denials,
        escalations: now.escalations - before.escalations,
        fast_path: now.fast_path - before.fast_path,
    }
}

// ---------------------------------------------------------------------------
// 1. the engine: governed ranged reads direct-drive, engagement exact
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ddt_ranged_ring_read_direct_drives_with_exact_engagement() {
    let fx = Fixture::new("engine").await;
    fx.salt_inos(0).await;
    let ino = fx.create_striped("dd.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "dd.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    // Aligned device-class shapes inside one block, arena_off 4 KiB-
    // aligned (the direct-arena-DMA leg: window == request).
    let shapes: &[(u64, usize)] = &[
        (4096, 4096),
        (64 * 1024, 16 * 1024),
        (BS + 128 * 1024, 64 * 1024),
        (256 * 1024, 4096),
    ];
    // Deltas bracket ONLY the ring reads (the parity fuse_read calls
    // below drive the handler's own ranged accounting).
    let before = snap();
    let mut got_all = Vec::new();
    for (offset, len) in shapes {
        let got = tokio::task::block_in_place(|| {
            session.ring_pread(binding, *offset, *len, 0, "dd aligned read")
        });
        got_all.push(got);
    }
    let d = delta(&before);
    for (i, (offset, len)) in shapes.iter().enumerate() {
        let want = fx.fuse_read(ino, *offset, *len as u32).await;
        assert_eq!(
            got_all[i], want,
            "byte parity on direct-drive shape {i} (offset {offset} len {len})"
        );
    }
    fx.fs.router.set_direct_device_true(false);

    let n = shapes.len() as u64;
    assert_eq!(
        d.dd_serves, n,
        "every governed ranged read must be SERVED direct-drive (no task, \
         no tokio, no handler) — got {} of {n}",
        d.dd_serves
    );
    assert_eq!(
        d.handoffs, 0,
        "direct-drive serves must not ride the async handoff (got {})",
        d.handoffs
    );
    // The governed accounting stays observable (amplification bounds +
    // the §3 rule-4 engagement instrument).
    assert_eq!(d.ops_read, n, "ipc_ops_read counts direct-drive serves");
    assert_eq!(d.ranged, n, "ranged_reads counts direct-drive device reads");
    assert_eq!(
        d.device_true, n,
        "read_device_true_reads counts direct-drive device reads"
    );
    assert_eq!(d.dd_bounces, 0, "aligned shapes never bounce");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 2. unaligned request or unaligned arena destination: bounce leg
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unaligned_shapes_direct_drive_through_the_bounce_leg() {
    let fx = Fixture::new("bounce").await;
    fx.salt_inos(8).await;
    let ino = fx.create_striped("bounce.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "bounce.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    let before = snap();
    // (a) LBA-unaligned file offset: window ≠ request ⇒ bounce.
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 12 * 4096 + 512, 4096, 0, "unaligned offset")
    });
    let want = fx.fuse_read(ino, 12 * 4096 + 512, 4096).await;
    assert_eq!(got, want, "byte parity on the unaligned-offset bounce leg");

    // (b) aligned file window but arena_off NOT 4 KiB-aligned: the arena
    // cannot take direct O_DIRECT-class DMA ⇒ bounce via the ranged pool.
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 32 * 4096, 4096, 512, "unaligned arena")
    });
    let want = fx.fuse_read(ino, 32 * 4096, 4096).await;
    assert_eq!(got, want, "byte parity on the unaligned-arena bounce leg");

    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(
        d.dd_serves, 2,
        "unaligned shapes still direct-drive (via bounce), got {}",
        d.dd_serves
    );
    assert_eq!(d.dd_bounces, 2, "both shapes must be counted as bounces");
    assert_eq!(d.handoffs, 0, "no handler involvement on the bounce leg");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 3. prelude misses fall back to the handler — correctness owns ambiguity
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prelude_misses_fall_back_to_the_handler_with_ledger() {
    let fx = Fixture::new("prelude").await;
    // Striped file with a HOLE at block 1 (blocks 0 and 2 written).
    fx.salt_inos(16).await;
    let ino = fx.create_striped("prelude.bin", &[0, 2]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "prelude.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    // (a) sub-device-class length (< 4 KiB) ⇒ shape.
    let before = snap();
    let got =
        tokio::task::block_in_place(|| session.ring_pread(binding, 4096, 2048, 0, "short len"));
    let want = fx.fuse_read(ino, 4096, 2048).await;
    assert_eq!(got, want, "short-len fallback parity");
    let d = delta(&before);
    assert_eq!(d.dd_serves, 0, "sub-4 KiB reads are not the governed shape");
    assert!(d.inel_shape >= 1, "shape ledger must record the refusal");
    assert!(d.handoffs >= 1, "the op must be served by the handler path");

    // (b) EOF-crossing read ⇒ shape (in-bounds contract is strict).
    let file_size = 3 * BS;
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, file_size - 2048, 4096, 0, "eof cross")
    });
    let want = fx.fuse_read(ino, file_size - 2048, 4096).await;
    assert_eq!(got, want, "EOF-crossing fallback parity");
    let d = delta(&before);
    assert_eq!(d.dd_serves, 0, "EOF-crossing reads fall back");
    assert!(d.inel_shape >= 1, "shape ledger must record the refusal");

    // (c) hole block (block 1 has no binding) ⇒ layout.
    let before = snap();
    let got =
        tokio::task::block_in_place(|| session.ring_pread(binding, BS + 8192, 4096, 0, "hole"));
    assert_eq!(
        got,
        vec![0u8; 4096],
        "hole reads serve zeros via the handler"
    );
    let d = delta(&before);
    assert_eq!(d.dd_serves, 0, "hole blocks fall back");
    assert!(d.inel_layout >= 1, "layout ledger must record the hole");

    // (d) RAM overlay present (partial in-place write parks an overlay on
    // block 2) ⇒ overlay — ANY overlay presence falls back.
    let patch = deterministic_bytes(50_000, 7);
    fx.fs
        .write(
            req(),
            ino,
            0,
            2 * BS + 100_000,
            bytes::Bytes::from(patch.clone()),
            0,
            0,
        )
        .await
        .expect("partial write parks an overlay");
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 2 * BS + 100_352, 4096, 0, "overlay")
    });
    assert_eq!(
        got,
        patch[352..352 + 4096].to_vec(),
        "overlay bytes must be served (acked custody, item B)"
    );
    let d = delta(&before);
    assert_eq!(d.dd_serves, 0, "overlay blocks must NOT direct-drive");
    assert!(
        d.inel_overlay >= 1,
        "overlay ledger must record the refusal"
    );

    // (e) staged (non-striped) layout ⇒ meta.
    let create = fx
        .fs
        .create(req(), 1, OsStr::new("staged.bin"), libc::S_IFREG | 0o644, 0)
        .await
        .expect("create");
    let sino = create.attr.ino;
    let sdata = deterministic_bytes(256 * 1024, 9);
    fx.fs
        .write(req(), sino, 0, 0, bytes::Bytes::from(sdata.clone()), 0, 0)
        .await
        .expect("staged write");
    let sfd = odirect_standin(&fx, &dir, "staged.bin", sino);
    let (s2, b2) = ClientSession::establish(&fx, &sfd);
    let before = snap();
    let got = tokio::task::block_in_place(|| s2.ring_pread(b2, 4096, 4096, 0, "staged"));
    assert_eq!(got, sdata[4096..8192].to_vec(), "staged fallback parity");
    let d = delta(&before);
    assert_eq!(d.dd_serves, 0, "non-striped layouts must NOT direct-drive");
    assert!(d.inel_meta >= 1, "meta ledger must record the refusal");

    fx.fs.router.set_direct_device_true(false);
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 4. the 795 protocol: prelude snapshot revalidation at the CQE
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custody_snapshot_revalidation_governs_the_serve() {
    let fx = Fixture::new("custody").await;
    fx.salt_inos(26).await;
    let ino = fx.create_striped("custody.bin", &[0, 1]).await;
    fx.fs.router.set_direct_device_true(true);

    // The prelude snapshot on a clean striped block is eligible…
    let snap0 = fx
        .fs
        .ipc_direct_read_probe(
            ino,
            16 * 4096,
            4096,
            squeezefs::mono_core::monotonic_ns_u64(),
        )
        .expect("clean striped block must probe eligible");
    assert!(
        fx.fs.ipc_direct_revalidate(&snap0),
        "an unmoved snapshot must revalidate"
    );

    // …a custody-epoch bump (every overlay/sibling/record retire bumps
    // it — the 795 moving-custody seqlock) invalidates it…
    bump_block_custody_epoch(ino, 0);
    assert!(
        !fx.fs.ipc_direct_revalidate(&snap0),
        "a custody-transfer epoch bump between submit and CQE must force \
         the handler fallback (the 795 protocol)"
    );

    // …a fresh snapshot sees the new epoch and revalidates again…
    let snap1 = fx
        .fs
        .ipc_direct_read_probe(
            ino,
            16 * 4096,
            4096,
            squeezefs::mono_core::monotonic_ns_u64(),
        )
        .expect("probe after epoch bump");
    assert_eq!(snap1.epoch, block_custody_epoch(ino, 0));
    assert!(fx.fs.ipc_direct_revalidate(&snap1));

    // …an overlay parked mid-flight (partial write) invalidates, and the
    // prelude refuses the block outright while it exists…
    let patch = deterministic_bytes(8192, 3);
    fx.fs
        .write(req(), ino, 0, 200_000, bytes::Bytes::from(patch), 0, 0)
        .await
        .expect("partial write parks an overlay");
    assert!(
        !fx.fs.ipc_direct_revalidate(&snap1),
        "an overlay parked between submit and CQE must force the fallback"
    );
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(
                ino,
                16 * 4096,
                4096,
                squeezefs::mono_core::monotonic_ns_u64()
            ),
            Err(IpcDirectIneligible::Overlay)
        ),
        "the prelude must refuse a block with a live overlay"
    );

    // …and a truncate-to-zero (layout gone) invalidates every snapshot.
    fx.fs
        .setattr(
            req(),
            ino,
            None,
            fuse3::SetAttr {
                size: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect("truncate");
    assert!(
        !fx.fs.ipc_direct_revalidate(&snap1),
        "a truncate between submit and CQE must force the fallback"
    );
    fx.fs.router.set_direct_device_true(false);
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 5. teardown/shutdown promptness with direct-drive in flight
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_shutdown_is_prompt_with_direct_drive_traffic() {
    let fx = Fixture::new("teardown").await;
    fx.salt_inos(34).await;
    let ino = fx.create_striped("teardown.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "teardown.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    // Drive a stream of direct-drive reads from a plain thread, then tear
    // the session + host down mid-stream: the arena mapping must stay
    // alive under any in-flight DMA (SlotCompletion pins it — §5.3.1 rule
    // 4), and shutdown must stay prompt.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let driver = std::thread::spawn(move || {
        let mut i = 0u64;
        while !stop2.load(Ordering::Relaxed) {
            let off = 4096 * (1 + (i % 512));
            let _ = session.ring_pread(binding, off, 4096, 0, "teardown stream");
            i += 1;
        }
        drop(session); // client unmaps its side mid-life
    });
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    driver.join().expect("driver thread must not panic");

    let t0 = Instant::now();
    fx.shutdown();
    let took = t0.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "host shutdown with direct-drive traffic must be prompt (took {took:?})"
    );
    fx.fs.router.set_direct_device_true(false);

    // Direct-drive must have actually engaged during the stream.
    assert!(
        METRICS.ipc_direct_drive_serves.load(Ordering::Relaxed) > 0,
        "the teardown stream must have exercised direct-drive"
    );
}

// ---------------------------------------------------------------------------
// 6. stats surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_drive_stats_fields_export() {
    let fx = Fixture::new("stats").await;
    let stats = fx.fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json parses");
    let m = v
        .get("metrics")
        .expect("stats JSON carries a metrics object");
    for key in [
        "ipc_direct_drive_submits",
        "ipc_direct_drive_serves",
        "ipc_direct_drive_bounces",
        "ipc_direct_drive_fallbacks_post",
        "ipc_direct_ineligible_shape",
        "ipc_direct_ineligible_meta",
        "ipc_direct_ineligible_layout",
        "ipc_direct_ineligible_overlay",
        "ipc_direct_ineligible_backend",
        "ipc_direct_shards",
    ] {
        assert!(m.get(key).is_some(), "stats inode must export {key}");
    }
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 6b. residence decomposition — `ipc_direct_phase_ns` (shim-iops campaign,
//     2026-08-07): the direct-drive path is the rand-4k il hot path and had
//     NO residence instrument — `read_serve_phase_ns` never sees these ops
//     (no handler), so the fio-clat-vs-daemon-residence split the 1M-IOPS
//     decomposition needs was unmeasurable on production mounts. The
//     family is ALWAYS-ON (the `write_pipeline_phase_ns` cost contract:
//     one `Instant` read + one relaxed `fetch_add` per boundary crossed)
//     and buckets through the SAME shared `latency_core`, so it composes
//     with the fuse3/read tables. Phases:
//       admit    = prelude probe entry → in-flight slab insert (eligibility
//                  + custody snapshot + lane route + slab/SQE-prep)
//       inflight = slab insert → CQE popped by the shard reaper (SQE push +
//                  flush-batch wait + device/fabric service + reap batch)
//       finish   = CQE popped → slot completion posted (revalidate + serve
//                  accounting + bounce-leg copy)
//       total    = probe entry → completion posted (the DAEMON residence;
//                  fio clat − total = client + ring-ingress residence, the
//                  subtraction the campaign ledger keys on)
//     Containment: total ≈ admit + inflight + finish (same-op spans, no
//     unexplained residue). Only SERVED ops record finish/total (fallbacks
//     ride the handler, whose own family times them).
// ---------------------------------------------------------------------------

/// Sample count of the `ipc_drain_pass_ns` histogram (drain-funnel
/// campaign, 2026-08-08 r3: non-empty svc-thread drain sweeps).
fn drain_pass_count(v: &serde_json::Value) -> u64 {
    v.get("metrics")
        .expect("stats JSON carries a metrics object")
        .get("ipc_drain_pass_ns")
        .expect("stats inode must export ipc_drain_pass_ns")
        .as_object()
        .expect("drain-pass histogram is a bucket map")
        .values()
        .map(|n| n.as_u64().unwrap_or(0))
        .sum()
}

/// Sample count of the `ipc_ingress_ns` histogram (reap-fanin campaign,
/// 2026-08-08: the measured client→daemon ring-ingress residence).
fn ingress_count(v: &serde_json::Value) -> u64 {
    v.get("metrics")
        .expect("stats JSON carries a metrics object")
        .get("ipc_ingress_ns")
        .expect("stats inode must export ipc_ingress_ns")
        .as_object()
        .expect("ingress histogram is a bucket map")
        .values()
        .map(|n| n.as_u64().unwrap_or(0))
        .sum()
}

fn phase_counts(v: &serde_json::Value, phase: &str) -> u64 {
    v.get("metrics")
        .expect("stats JSON carries a metrics object")
        .get("ipc_direct_phase_ns")
        .unwrap_or_else(|| panic!("stats inode must export ipc_direct_phase_ns (phase {phase})"))
        .get(phase)
        .unwrap_or_else(|| panic!("ipc_direct_phase_ns must carry phase '{phase}'"))
        .as_object()
        .expect("phase histogram is a bucket map")
        .values()
        .map(|n| n.as_u64().unwrap_or(0))
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_drive_serves_record_residence_phases() {
    let fx = Fixture::new("phases").await;
    fx.salt_inos(3).await;
    let ino = fx.create_striped("phase.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "phase.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    let stats0: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let before: Vec<u64> = ["admit", "inflight", "finish", "total"]
        .iter()
        .map(|p| phase_counts(&stats0, p))
        .collect();

    let n = 4u64;
    let d0 = snap();
    for i in 0..n {
        let got = tokio::task::block_in_place(|| {
            session.ring_pread(binding, (16 + i) * 4096, 4096, 0, "phase read")
        });
        assert_eq!(got.len(), 4096, "served read {i}");
    }
    let d = delta(&d0);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(d.dd_serves, n, "phase rows require direct-drive engagement");

    let stats1: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    for (i, phase) in ["admit", "inflight", "finish", "total"].iter().enumerate() {
        let grew = phase_counts(&stats1, phase) - before[i];
        assert!(
            grew >= n,
            "every direct-drive serve must record phase '{phase}' \
             (grew {grew}, want ≥ {n})"
        );
    }
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 6b'. ingress residence (reap-fanin campaign, 2026-08-08): every stamped
//      ring op records ONE measured client→daemon ingress sample at the
//      dequeue — the term the shim-iops ledger could only derive by
//      subtraction (fio clat − daemon total). Contract:
//      - the stats inode exports `ipc_ingress_ns` (26-bucket histogram);
//      - a stamped submit grows its count by exactly the ops popped
//        (never-stamped slots and implausible deltas record NOTHING —
//        the delta law's discard classes, unit-pinned in squeezefs-ipc).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ring_ops_record_ingress_residence() {
    let fx = Fixture::new("ingress").await;
    fx.salt_inos(9).await;
    let ino = fx.create_striped("ingress.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "ingress.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    let stats0: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let before = ingress_count(&stats0);

    let n = 4u64;
    for i in 0..n {
        let got = tokio::task::block_in_place(|| {
            session.ring_pread(binding, (16 + i) * 4096, 4096, 0, "ingress read")
        });
        assert_eq!(got.len(), 4096, "served read {i}");
    }
    fx.fs.router.set_direct_device_true(false);

    let stats1: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let grew = ingress_count(&stats1) - before;
    assert!(
        grew >= n,
        "every stamped ring op must record one ingress sample \
         (grew {grew}, want ≥ {n})"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 6b''. drain-pass decomposition (drain-funnel campaign, 2026-08-08 r3):
//       the svc-thread dequeue rate is the funnel the field ingress
//       histogram convicted (~85 % of clat pooled pre-dequeue), so the
//       pass itself gets an always-on instrument. Contract:
//       - `ipc_drain_pass_ns` exports (26-bucket histogram): duration of
//         every NON-EMPTY drain sweep (drain + flush + inline reap —
//         the whole per-pass ceremony); its count is the pass count, so
//         ops ÷ count is the live ops/pass and mean ns ÷ (ops/pass) the
//         per-op svc-thread cost;
//       - `ipc_drain_empty_passes` exports (the spin-phase cadence
//         gauge, pairs with ipc_service_parks);
//       - a served ring burst grows the pass count by ≥ 1.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_passes_record_duration_and_counts() {
    let fx = Fixture::new("drainpass").await;
    fx.salt_inos(11).await;
    let ino = fx.create_striped("drainpass.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "drainpass.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    let stats0: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let passes0 = drain_pass_count(&stats0);
    assert!(
        stats0
            .get("metrics")
            .expect("metrics object")
            .get("ipc_drain_empty_passes")
            .is_some(),
        "stats inode must export ipc_drain_empty_passes"
    );

    let n = 4u64;
    for i in 0..n {
        let got = tokio::task::block_in_place(|| {
            session.ring_pread(binding, (16 + i) * 4096, 4096, 0, "drainpass read")
        });
        assert_eq!(got.len(), 4096, "served read {i}");
    }
    fx.fs.router.set_direct_device_true(false);

    let stats1: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let grew = drain_pass_count(&stats1) - passes0;
    assert!(
        (1..=n).contains(&grew) || grew > n,
        "served ring ops must record non-empty drain passes (grew {grew})"
    );
    // The flush half (funnel attribution r3): every recorded pass also
    // records its sink-flush span, so pass − flush = the drain half by
    // subtraction and the fixed-ceremony term is attributable.
    let flush_count: u64 = stats1
        .get("metrics")
        .expect("metrics object")
        .get("ipc_drain_flush_ns")
        .expect("stats inode must export ipc_drain_flush_ns")
        .as_object()
        .expect("flush histogram is a bucket map")
        .values()
        .map(|v| v.as_u64().unwrap_or(0))
        .sum();
    assert!(
        flush_count >= grew,
        "every non-empty pass records its flush half (flush {flush_count} < passes {grew})"
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 6c. reaper/drain fusion (shim-iops campaign, 2026-08-07): a shard's CQ
//     may be consumed OPPORTUNISTICALLY by the service thread's flush pass
//     (userspace CQ peek — zero syscall) so a continuously-loaded lane's
//     completions stop paying the dedicated reaper's per-batch
//     `io_uring_enter` wake + ctx switch (the decomposition's measured
//     0.72 enters/op + 2.36 ctx/op economy at the 32×8 ceiling shape).
//     The shard reaper stays the blocking backstop (park-time serves,
//     shutdown drain). Contracts:
//     - correctness under fusion: a concurrent burst of governed ring
//       reads serves exactly, engagement intact, regardless of which
//       consumer took each CQE;
//     - `ipc_direct_inline_reaps` exports (the fusion engagement gauge —
//       brackets key on its delta; a fixture burst cannot assert it
//       deterministically, the reaper legitimately races);
//     - shutdown stays prompt with fusion armed (the NOP wake must stay
//       reaper-only — an inline consumer eating it would strand the join).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_reap_fusion_serves_bursts_exactly_and_exports() {
    let fx = Fixture::new("fusion").await;
    fx.salt_inos(7).await;
    let ino = fx.create_striped("fusion.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "fusion.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);

    let before = snap();
    // Concurrent burst: distinct slots on one session, all in flight
    // together — the shape where the sweep's inline reap can race the
    // reaper for CQEs. Byte parity per op proves whoever consumed the
    // CQE ran the identical revalidate+serve.
    let n = 8u64;
    let got = tokio::task::block_in_place(|| session.ring_pread_burst(binding, n as u32));
    for (i, (offset, len, bytes)) in got.iter().enumerate() {
        let block = (*offset / BS) as u32;
        let rel = (*offset % BS) as usize;
        assert_eq!(
            bytes,
            &expect_bytes(block, rel, *len),
            "byte parity on fused burst op {i}"
        );
    }
    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(d.dd_serves, n, "every burst op direct-drives");
    assert_eq!(d.handoffs, 0, "no handoff under fusion");

    let stats: serde_json::Value =
        serde_json::from_str(&fx.fs.generate_stats_json().await).expect("stats json parses");
    let m = stats.get("metrics").expect("metrics object");
    assert!(
        m.get("ipc_direct_inline_reaps").is_some(),
        "stats inode must export ipc_direct_inline_reaps (the fusion \
         engagement gauge)"
    );
    // Shutdown promptness with fusion armed (the NOP stays reaper-only).
    let t0 = Instant::now();
    fx.shutdown();
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "shutdown must stay prompt with fusion armed"
    );
}

// ---------------------------------------------------------------------------
// 7. DIALED P1.5 (2026-07-27): direct-drive for governor-denied misses on
//    DEFAULT mounts. Post-governor, a denied miss on the default hybrid
//    posture is semantically identical to a device-true serve — ranged
//    device read, no tier publish, nothing to invalidate — so it must
//    direct-drive. The prelude runs the admission decision synchronously:
//    tier probe (hit ⇒ sync fast path, unchanged), ghost TOUCH RECORDING
//    (skew evidence must still accumulate), governor token check.
//    Denied ⇒ direct-drive (+ denial accounting, no cooldown). Granted ⇒
//    handler path (the admission fetch + publish machinery stays there).
//    O_DIRECT bindings only — buffered ring reads are untouched (the
//    2026-07-15 hybrid-serve directive stays law).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_mount_governed_miss_ladder() {
    let fx = Fixture::new("dflt-ladder").await;
    fx.salt_inos(42).await;
    let ino = fx.create_striped("dflt.bin", &[0, 1]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "dflt.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);
    // DEFAULT posture: direct_device_true stays false for the whole test.

    // (a) FIRST touch (ghost miss ⇒ no admission candidacy ⇒ denied
    // shape) must DIRECT-DRIVE: no task, no tokio, no handler — and the
    // op is accounted as a plain ranged device read, NEVER device-true
    // (that family is the ddt escape's).
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 16 * 4096, 4096, 0, "default first touch")
    });
    assert_eq!(
        got,
        expect_bytes(0, 16 * 4096, 4096),
        "byte parity on the default-mount direct-drive serve"
    );
    let d = delta(&before);
    assert_eq!(
        d.dd_serves, 1,
        "a governor-denied (first-touch) miss on a DEFAULT mount must \
         direct-drive (got {} serves)",
        d.dd_serves
    );
    assert_eq!(d.handoffs, 0, "no async handoff on the denied slice");
    assert_eq!(d.ops_read, 1, "ipc_ops_read counts the direct serve");
    assert_eq!(d.ranged, 1, "the serve is a governed ranged device read");
    assert_eq!(
        d.device_true, 0,
        "default-posture direct-drive must NOT count read_device_true_reads \
         (that counter is the ddt escape's family)"
    );
    assert_eq!(
        d.denials, 0,
        "an unclamped first touch is not a governor denial"
    );

    // (b) SECOND touch of the same block key: the prelude's ghost
    // recording in (a) is the evidence — the touch is now a GRANT-shaped
    // escalation candidate (governor unclamped), and grants ride the
    // HANDLER path where the admission fetch + publish machinery lives.
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 20 * 4096, 4096, 0, "default second touch")
    });
    assert_eq!(
        got,
        expect_bytes(0, 20 * 4096, 4096),
        "byte parity on the granted-escalation handler serve"
    );
    let d = delta(&before);
    assert_eq!(
        d.dd_serves, 0,
        "a GRANTED escalation must not direct-drive (admission fetch + \
         publish stay on the handler)"
    );
    assert!(d.handoffs >= 1, "the granted op rides the async handoff");
    assert!(
        d.inel_policy >= 1,
        "the policy ledger must record the grant-shaped routing"
    );
    assert_eq!(
        d.escalations, 1,
        "the handler must actually ESCALATE the granted second touch \
         (whole-block ghost admission) — if the prelude had skipped the \
         ghost bookkeeping this would still look like a first touch and \
         hot subsets would never earn admission"
    );

    // (c) THIRD touch: the escalated block is hot-tier resident — an
    // O_DIRECT tier hit on a default mount serves from the tier via the
    // sync fast path (the hybrid-serve directive), never direct-drives.
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 24 * 4096, 4096, 0, "default tier hit")
    });
    assert_eq!(
        got,
        expect_bytes(0, 24 * 4096, 4096),
        "byte parity on the post-admission tier serve"
    );
    let d = delta(&before);
    assert_eq!(
        d.fast_path, 1,
        "an admitted (hot-tier-resident) block must serve on the sync \
         fast path — direct-drive only reroutes the DENIED-miss slice"
    );
    assert_eq!(d.dd_serves, 0, "tier hits never direct-drive");
    assert_eq!(d.handoffs, 0, "tier hits never hand off");

    fx.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_mount_clamped_denials_direct_drive_with_denial_accounting() {
    let fx = Fixture::new("dflt-clamp").await;
    fx.salt_inos(50).await;
    let ino = fx.create_striped("clamp.bin", &[0, 1]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "clamp.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // First touch records the ghost evidence (and direct-drives).
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 4096, 4096, 0, "clamp first touch")
    });
    assert_eq!(got, expect_bytes(0, 4096, 4096));
    let d = delta(&before);
    assert_eq!(d.dd_serves, 1, "first touch direct-drives");

    // Engage the clamp deterministically: two whole-block admitted
    // victims evicted with ZERO payback (the churn steady state) — the
    // governor's waste window now refuses escalations that don't fit the
    // (empty) token grant.
    let gov = &fx.fs.router.cache.admission_governor;
    let block = fx.fs.router.block_size.load(Ordering::Relaxed);
    for _ in 0..2 {
        gov.on_eviction(
            block,
            &squeezefs::tiering::memory::EvictClass::Protected {
                served_bytes: 0,
                stream_admitted: false,
            },
        );
    }

    // Ghost-hit touches under an engaged clamp with no tokens: DENIED ⇒
    // direct-drive, one governor denial per op, NO cooldown recorded (a
    // cooldown would silently strip the key's candidacy — the next read
    // would direct-drive WITHOUT a denial, breaking the accounting).
    for i in 0..3u64 {
        let off = (2 + i) * 8192;
        let before = snap();
        let got = tokio::task::block_in_place(|| {
            session.ring_pread(binding, off, 4096, 0, "clamped denied touch")
        });
        assert_eq!(got, expect_bytes(0, off as usize, 4096));
        let d = delta(&before);
        assert_eq!(
            d.dd_serves, 1,
            "clamped governor-denied miss {i} must direct-drive"
        );
        assert_eq!(
            d.denials, 1,
            "denied miss {i} must carry exactly one governor denial \
             (denials ≈ direct-drive serves is the churn-row coherence \
             tripwire)"
        );
        assert_eq!(d.escalations, 0, "denied misses never escalate");
        assert_eq!(d.handoffs, 0, "denied misses never hand off");
        assert_eq!(d.device_true, 0, "default posture stays non-device-true");
    }
    fx.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffered_ring_misses_never_direct_drive() {
    let fx = Fixture::new("dflt-buffered").await;
    fx.salt_inos(58).await;
    let ino = fx.create_striped("buf.bin", &[0, 1]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = buffered_standin(&fx, &dir, "buf.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, 16 * 4096, 4096, 0, "buffered miss")
    });
    assert_eq!(got, expect_bytes(0, 16 * 4096, 4096), "buffered parity");
    let d = delta(&before);
    assert_eq!(
        d.dd_serves, 0,
        "buffered bindings must NOT direct-drive (O_DIRECT class only — \
         the hybrid-serve directive stays law)"
    );
    assert!(d.handoffs >= 1, "buffered misses ride the handler");
    assert_eq!(
        d.inel_policy, 0,
        "buffered ops never enter the prelude (not a policy routing)"
    );
    fx.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_classified_files_ride_the_handler_by_policy() {
    let fx = Fixture::new("dflt-stream").await;
    fx.salt_inos(66).await;
    let ino = fx.create_striped("stream.bin", &[0, 1]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "stream.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Classify the file: 4 contiguous handler reads (the §5.3 run rule).
    let sc0 = METRICS.read_streams_classified.load(Ordering::Relaxed);
    for i in 0..4u64 {
        let want = expect_bytes(0, (i * 4096) as usize, 4096);
        let got = fx.fuse_read(ino, i * 4096, 4096).await;
        assert_eq!(got, want, "classification run parity");
    }
    assert!(
        METRICS.read_streams_classified.load(Ordering::Relaxed) > sc0,
        "fixture: the contiguous run must classify a lane"
    );

    // A governed-shape O_DIRECT ring miss on the CLASSIFIED file inside
    // the freshness window: the handler's §5.6 dispatch would not range
    // this read (streams want whole blocks + the pipeline), so the
    // prelude must route it to the handler — policy, not prelude rot.
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        session.ring_pread(binding, BS + 128 * 4096, 4096, 0, "classified-file miss")
    });
    assert_eq!(
        got,
        expect_bytes(1, 128 * 4096, 4096),
        "classified-file fallback parity"
    );
    let d = delta(&before);
    assert_eq!(
        d.dd_serves, 0,
        "stream-classified files must not direct-drive (whole-block + \
         pipeline dispatch parity)"
    );
    assert!(d.inel_policy >= 1, "the policy ledger records the routing");
    assert!(d.handoffs >= 1, "the op rides the handler");
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 8. the governor peek is NON-RESERVING (pure API): the prelude's token
//    check must not spend the grant — the ONLY reservation site stays the
//    handler's authoritative `allow_escalation` (the herd-safety argument
//    of the governor design is preserved verbatim).
// ---------------------------------------------------------------------------

#[test]
fn governor_peek_is_non_reserving_and_counts_denials() {
    use squeezefs::routing::{AdmissionGovernor, TEST_ADMISSION_EPOCH_MS};
    const EPOCH_MS: u64 = 200;
    TEST_ADMISSION_EPOCH_MS.store(EPOCH_MS, Ordering::Relaxed);
    let gov = AdmissionGovernor::new(5);
    let block: u64 = 4 * 1024 * 1024;

    // Unclamped: the peek admits, no denial.
    let d0 = METRICS
        .read_admission_governor_denials
        .load(Ordering::Relaxed);
    assert!(
        gov.escalation_would_admit(block),
        "unclamped peek must admit"
    );
    assert_eq!(
        METRICS
            .read_admission_governor_denials
            .load(Ordering::Relaxed),
        d0,
        "unclamped peek records no denial"
    );

    // Engage the clamp (zero-payback whole-block victims) with an empty
    // token grant: the peek DENIES and counts it.
    for _ in 0..2 {
        gov.on_eviction(
            block,
            &squeezefs::tiering::memory::EvictClass::Protected {
                served_bytes: 0,
                stream_admitted: false,
            },
        );
    }
    assert!(
        !gov.escalation_would_admit(block),
        "clamped + empty grant ⇒ the peek denies"
    );
    assert_eq!(
        METRICS
            .read_admission_governor_denials
            .load(Ordering::Relaxed),
        d0 + 1,
        "the peek's denial is accounted (denials ≈ direct-drive serves)"
    );

    // Fund the NEXT epoch's grant, cross the boundary, keep the clamp
    // engaged (fresh waste in the new window): N peeks must all admit —
    // non-reserving — and the authoritative reservation must still find
    // the FULL grant afterwards.
    gov.note_foreground(80 * block * 20); // 5 % ⇒ 80 blocks
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };
    let e0 = now_ms() / EPOCH_MS;
    while now_ms() / EPOCH_MS == e0 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for _ in 0..2 {
        gov.on_eviction(
            block,
            &squeezefs::tiering::memory::EvictClass::Protected {
                served_bytes: 0,
                stream_admitted: false,
            },
        );
    }
    for i in 0..64 {
        assert!(
            gov.escalation_would_admit(block),
            "peek {i} must admit while the grant covers a block"
        );
    }
    let mut reserved = 0u64;
    while gov.allow_escalation(block) {
        reserved += 1;
        assert!(reserved <= 80, "reservation must stop at the grant");
    }
    assert_eq!(
        reserved, 80,
        "64 peeks must not have consumed ANY of the 80-block grant \
         (non-reserving — the handler stays the only reservation site)"
    );
    TEST_ADMISSION_EPOCH_MS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// 9. sharded rings + reapers (the D12 randread-shim residual, 2026-08-05):
//    the single-shared-ring + single-reaper engine serialized EVERY
//    CQE-side serve (revalidate + accounting + slot completion) on ONE
//    unpinned thread — at the field's 235 µs fabric RTT and 256 in-flight
//    (fio libaio rand-4k 32×qd8) that one wake-serve loop was the
//    208k-IOPS-flat ceiling (~4.8 µs/op of reaper CPU) while the kernel
//    FUSE path spread reply work over per-CPU queues (273–280k). The
//    module's own docs recorded per-thread rings as the fallback "if SQ
//    contention ever shows on a profile" — this section pins that shape,
//    BY DERIVATION: shard width = the ONE drain-parallelism slope
//    (`il_sessions_default`, the SAME function that ceilings the service
//    threads feeding this engine), one lane per service thread, reapers
//    pinned per the numa_core owner partition.
// ---------------------------------------------------------------------------

/// The derivation tie (drift-is-red, the ingest-economy paired-pin law),
/// RE-GRADED by the counted 2026-08-06 field width sweep (squeeze-test,
/// 32 CPUs / 2 nodes, il rand-4k 32×qd32, 3×30 s rows per width, W8
/// brackets at BOTH ends — no drift — engagement + shards/svc gauges
/// exact per width): W8 (the old cpus/4) 622–636k, **W12 695–700k
/// (+10.4 %, clat 1461–1472 µs — best)**, W16 675–690k (+7 %), W24
/// 616–626k (−2 % — REGRESSION). The shard width must ride the drain-
/// LANE derivation `il_drain_lanes_default` = clamp(3×cpus/8, 2, 64):
/// one lane = TWO OS threads (svc submitter + `sqz-ipc-ddN` reaper), so
/// the sweep's grid in lane-thread/core terms is 2W/cpus ∈ {0.5, 0.75,
/// 1.0, 1.5} — the optimum sits at the ¾-core-budget point and the one
/// sampled point past 1.0× is the one regression (oversubscription of
/// the co-located client fleet). Never a constant; the env form
/// (`SQUEEZEFS_IPC_DD_SHARDS`) stays an override/measurement lever with
/// the service-ceiling clamp parity (1..=64), unparseable ⇒ derived.
#[test]
fn dd_shard_width_derivation_ties_to_drain_lane_width() {
    use squeezefs::ipc_direct::dd_shards_from;
    use squeezefs_ipc::sizing::il_drain_lanes_default;
    for cpus in [1usize, 2, 4, 8, 16, 25, 32, 64, 96, 128, 256] {
        assert_eq!(
            dd_shards_from(None, cpus),
            il_drain_lanes_default(cpus),
            "shard width must ride the ONE drain-lane derivation \
             (cpus={cpus}) — a flat constant is the DEFAULTS-MISMATCH class"
        );
    }
    // Canonical shapes (drift-is-red): the counted-optimum field box, a
    // big-box slope point, the floor.
    assert_eq!(
        dd_shards_from(None, 32),
        12,
        "the 32-CPU field box: the counted sweep's interior optimum"
    );
    assert_eq!(dd_shards_from(None, 96), 36, "big-box slope: 3×96/8");
    assert_eq!(
        dd_shards_from(None, 4),
        2,
        "floor 2: the pre-L4-8 single-consumer plateau"
    );
    assert_eq!(dd_shards_from(Some("1"), 32), 1, "explicit wins verbatim");
    assert_eq!(dd_shards_from(Some("12"), 32), 12);
    assert_eq!(dd_shards_from(Some("999"), 32), 64, "env clamp ceiling 64");
    assert_eq!(dd_shards_from(Some("0"), 32), 1, "env clamp floor 1");
    assert_eq!(
        dd_shards_from(Some("nope"), 32),
        il_drain_lanes_default(32),
        "unparseable falls back to the derivation"
    );
}

/// The structural contract: sessions pin to service threads (owners fill
/// lightest-first ⇒ two sessions land owners 0 and 1), each service
/// thread owns its own direct-drive LANE, so governed traffic through two
/// sessions must engage TWO shard reapers — the pre-fix engine reads 1
/// here (its single shared ring is the field ceiling's structure). Serves
/// stay exact and byte-true on both lanes (engagement instruments never
/// go dark across the sharding).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_service_lanes_engage_distinct_shards() {
    let fx = Fixture::new("shards").await;
    fx.salt_inos(74).await;
    let ino_a = fx.create_striped("shard-a.bin", &[0, 1]).await;
    let ino_b = fx.create_striped("shard-b.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd_a = odirect_standin(&fx, &dir, "shard-a.bin", ino_a);
    let fd_b = odirect_standin(&fx, &dir, "shard-b.bin", ino_b);
    let (session_a, binding_a) = ClientSession::establish(&fx, &fd_a);
    let (session_b, binding_b) = ClientSession::establish(&fx, &fd_b);
    fx.fs.router.set_direct_device_true(true);

    let shapes: &[(u64, usize)] = &[(4096, 4096), (64 * 1024, 16 * 1024)];
    let before = snap();
    let (got_a, got_b) = tokio::task::block_in_place(|| {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for (offset, len) in shapes {
            a.push(session_a.ring_pread(binding_a, *offset, *len, 0, "lane A read"));
            b.push(session_b.ring_pread(binding_b, *offset, *len, 0, "lane B read"));
        }
        (a, b)
    });
    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);

    for (i, (offset, len)) in shapes.iter().enumerate() {
        assert_eq!(
            got_a[i],
            expect_bytes(0, *offset as usize, *len),
            "byte parity on lane A shape {i}"
        );
        assert_eq!(
            got_b[i],
            expect_bytes(0, *offset as usize, *len),
            "byte parity on lane B shape {i}"
        );
    }
    let n = 2 * shapes.len() as u64;
    assert_eq!(
        d.dd_serves, n,
        "every governed read on BOTH lanes must direct-drive"
    );
    assert_eq!(d.handoffs, 0, "no handler involvement on either lane");
    assert_eq!(
        METRICS.ipc_direct_shards.load(Ordering::Relaxed),
        2,
        "two service lanes must engage two direct-drive shard reapers \
         (1 here = the single-shared-ring 208k-flat field structure)"
    );
    fx.shutdown();
}

/// Multi-shard teardown promptness + accounting closure: streams on two
/// lanes, host shutdown mid-life joins EVERY shard reaper (drains
/// in-flight CQEs first — bounded by device latency), and the engine's
/// closure law `serves + fallbacks_post == submits` holds across shards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_joins_every_shard_reaper_promptly() {
    let fx = Fixture::new("shard-teardown").await;
    fx.salt_inos(84).await;
    let ino_a = fx.create_striped("shard-td-a.bin", &[0, 1]).await;
    let ino_b = fx.create_striped("shard-td-b.bin", &[0, 1]).await;
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd_a = odirect_standin(&fx, &dir, "shard-td-a.bin", ino_a);
    let fd_b = odirect_standin(&fx, &dir, "shard-td-b.bin", ino_b);
    let (session_a, binding_a) = ClientSession::establish(&fx, &fd_a);
    let (session_b, binding_b) = ClientSession::establish(&fx, &fd_b);
    fx.fs.router.set_direct_device_true(true);

    let submits0 = METRICS.ipc_direct_drive_submits.load(Ordering::Relaxed);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut drivers = Vec::new();
    for (session, binding, name) in [
        (session_a, binding_a, "lane A stream"),
        (session_b, binding_b, "lane B stream"),
    ] {
        let stop2 = Arc::clone(&stop);
        drivers.push(std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                let off = 4096 * (1 + (i % 512));
                let _ = session.ring_pread(binding, off, 4096, 0, name);
                i += 1;
            }
            drop(session);
        }));
    }
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    for d in drivers {
        d.join().expect("driver thread must not panic");
    }

    assert_eq!(
        METRICS.ipc_direct_shards.load(Ordering::Relaxed),
        2,
        "both lanes' shard reapers must be live before teardown"
    );
    let t0 = Instant::now();
    fx.shutdown();
    let took = t0.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "shutdown must join every shard reaper promptly (took {took:?})"
    );
    fx.fs.router.set_direct_device_true(false);

    let submits = METRICS.ipc_direct_drive_submits.load(Ordering::Relaxed) - submits0;
    assert!(submits > 0, "the streams must have exercised direct-drive");
    // Closure across ALL shards: nothing stranded, nothing double-served.
    let serves = METRICS.ipc_direct_drive_serves.load(Ordering::Relaxed);
    let fallbacks = METRICS
        .ipc_direct_drive_fallbacks_post
        .load(Ordering::Relaxed);
    let total_submits = METRICS.ipc_direct_drive_submits.load(Ordering::Relaxed);
    assert_eq!(
        serves + fallbacks,
        total_submits,
        "every submitted op resolves exactly once (serve or post-CQE fallback)"
    );
}
