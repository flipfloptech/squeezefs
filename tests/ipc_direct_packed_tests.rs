//! PK8 — the interposer's direct-drive READ arm for PACKED (decorated)
//! mappings (`docs/design-small-file-packing.md` §5.10 — the read funnel;
//! `docs/design-preload-interception.md` §5.5/§12 — the DIALED P1
//! prelude: synchronous, RAM-authoritative, no inode guards, no node
//! locks, the 795 custody snapshot revalidated at the CQE, any miss ⇒ the
//! handler).
//!
//! Since 1.2.3 packs small files by default, every promoted small file is
//! a STAGED-layout inode whose `block_map[0]` is the size-carrying
//! `bk[@inc]:off:len` tenant mapping. The direct-drive prelude refused
//! every such shape to the handler (correct, but the shim's cold read of a
//! packed tenant forgoes the one ranged device read on the ipc-host uring
//! a whole-block striped file gets — the 2026-08 direct-drive campaigns'
//! IOPS class). This suite pins the ARM:
//!
//!  1. A packed tenant at a NON-ZERO `off` on the default volume plans ONE
//!     ranged read at `base + off + floor_grain(req)` … `ceil_grain(req_end)`,
//!     capped at the tenant's slot (`window_cap = ceil_grain(tenant_len)`);
//!     full-tenant / head / tail / interior (bounce) windows serve byte-exact
//!     on the direct path; `ipc_direct_packed_serves`/`_bytes`, `ipc_ops_read`
//!     and the funnel's `packed_reads` account.
//!  2. **FIND-PK-0 for the planner**: a tenant placed on a NON-default data
//!     volume reads byte-exact through the direct path — the device fd is
//!     the MAPPING's backend, never the default device.
//!  3. A request crossing the tenant's end is REFUSED (`PackedShape`) and the
//!     handler serves the short read exactly to size — never a neighbour's
//!     bytes; a truncate-up tail (size > image) is the same class.
//!  4. A transformed (compressed) volume's tenant stays on the handler
//!     (the image must decode whole), byte-exact.
//!  5. A newer ring-resident image (the RMW's stage→publish window) refuses
//!     the arm (`Overlay`), fails the CQE revalidation, and the handler
//!     serves the NEW bytes.
//!  6. `SQUEEZEFS_IPC_DD_PACKED=0` is today's handler fallback verbatim
//!     (`Meta`, bytes identical, the packed gauges silent).
//!  7. The CQE revalidation refuses a tenant whose block's incarnation word
//!     transitioned between plan and CQE (the re-mint model: retire →
//!     publish under a new generation); the prelude refuses while unstable.
//!  8. `ipc_direct_phase_ns` accounts every packed direct op (the `total`
//!     count delta ≡ ops) — on a DEFAULT mount through the governed-miss
//!     ladder, where a re-read keeps direct-driving (the handler's staged
//!     arm has no admission arm a GRANT could hand it to).
//!
//! In-process only (the `ipc_direct_drive_tests` raw-protocol client over
//! the REAL host → service → sink path; no kernel, no mount) — not
//! mount-class, so it rides no require-mount gate.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsync_economy;
use squeezefs::fuse_client::{IpcDirectIneligible, SqueezefsFilesystem, METRICS};
use squeezefs::ipc_host::{
    abstract_connect, futex_wake, recv_ctl, send_ctl, DataOp, IpcHost, IpcHostConfig, SessionSink,
    SlotCompletion,
};
use squeezefs::ipc_service::{set_ipc_dd_packed_enabled, DataPlaneSink};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{clean_block_key, pack_slot_len, DataRouter, LBA_GRAIN};
use squeezefs::{DataVolumeRecord, FormatConfig};
use squeezefs_ipc::layout::{
    Geometry, IpcSlot, SessionHeader, SessionLayout, SlotDescriptor, OP_READ,
};
use squeezefs_ipc::ring_core::{MpscRingView, RingCell};
use squeezefs_ipc::wire::CtlMsg;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const KIB: usize = 1024;
const BLOCK: usize = 4 * 1024 * KIB;
const GRAIN: u64 = LBA_GRAIN;

// ---------------------------------------------------------------------------
// process-global seams serialize
// ---------------------------------------------------------------------------

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Lever seams return to the knob on drop.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::test_set_small_file_packing(None);
        squeezefs::routing::set_inline_max_bytes_override(None);
        fsync_economy::test_set_promote_staged(None);
        set_ipc_dd_packed_enabled(true);
    }
}

fn arm_levers() -> LeverGuard {
    squeezefs::routing::set_inline_max_bytes_override(Some(squeezefs::routing::INLINE_MAX_FLOOR));
    squeezefs::routing::test_set_small_file_packing(Some(true));
    fsync_economy::test_set_promote_staged(Some(true));
    set_ipc_dd_packed_enabled(true);
    LeverGuard
}

fn req() -> Request {
    Request {
        unique: 1,
        // SAFETY: getuid/getgid are trivially safe.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: std::process::id(),
        ..Default::default()
    }
}

/// Deterministic per-tenant content salted by `tag` (a zeros read, a
/// prefix read, a neighbour's bytes or a device mix-up is caught).
fn pattern(tag: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (tag.wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
        })
        .collect()
}

fn metric(a: &squeezefs::fuse_client::Align64<AtomicU64>) -> u64 {
    a.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// the packing fixture (the small_file_packing_tests in-process venue,
// minus the job fabric) with the ipc host on top
// ---------------------------------------------------------------------------

fn make_dev_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path], compression: &str) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: compression.to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
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

struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    records: Vec<DataVolumeRecord>,
    dev_paths: Vec<PathBuf>,
    host: Arc<IpcHost>,
    cfg: IpcHostConfig,
    sink: Arc<InoMapSink>,
    _staging: TempDir,
}

/// A fresh format + open: `n` data volumes (the first is the default
/// slot), the FUSE layer, and the ipc host with the real data-plane sink.
async fn open_fresh(dir: &Path, n: usize, tag: &str, compression: &str) -> Fx {
    let meta = make_dev_file(dir, &format!("meta-{tag}"), 256 * 1024 * 1024);
    let dev_paths: Vec<PathBuf> = (0..n)
        .map(|i| make_dev_file(dir, &format!("oss{}-{tag}", i + 1), 1 << 30))
        .collect();
    let refs: Vec<&Path> = dev_paths.iter().map(|p| p.as_path()).collect();
    let cfg = base_format_config(&refs, compression);
    squeezefs::meta_backend::kv::builder::format_v3(
        &meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
    let records = cfg.resolved_data_volumes();

    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in &records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.clone());
    if compression != "none" {
        router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            compression.to_string(),
            "none".to_string(),
            None,
        ));
    }

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    let fs = Arc::new(fs);

    let sink = Arc::new(InoMapSink {
        inner: DataPlaneSink::new((*fs).clone()),
        map: std::sync::Mutex::new(HashMap::new()),
    });
    let host_cfg = IpcHostConfig {
        socket_name: format!("sqz-il0-pk8-{}-{}", std::process::id(), tag),
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
    let host = IpcHost::spawn(host_cfg.clone(), sink.clone()).expect("host must spawn");
    Fx {
        fs,
        meta: routed,
        records,
        dev_paths,
        host,
        cfg: host_cfg,
        sink,
        _staging: staging,
    }
}

impl Fx {
    /// Mount-faithful close: the dismount seal, the reclaim drain, the
    /// volumes, the host.
    async fn close(self) {
        self.host.shutdown();
        self.fs.router.seal_open_packs().await;
        self.fs.router.backend_router.reclaim_drain().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }

    fn alloc(&self, idx: usize) -> Arc<BlockAllocator> {
        self.fs
            .router
            .backend_router
            .backends
            .get(&self.records[idx].id)
            .expect("registered backend")
            .block_allocator
            .clone()
    }

    /// Force every NEW placement onto volume `idx`.
    fn place_only_on(&self, idx: usize) {
        for (i, rec) in self.records.iter().enumerate() {
            self.fs
                .router
                .backend_router
                .set_health_override(&rec.id, i != idx)
                .unwrap();
        }
    }

    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino
    }

    async fn write_at(&self, ino: u64, off: u64, data: &[u8]) {
        let written = self
            .fs
            .write(
                req(),
                ino,
                0,
                off,
                bytes::Bytes::copy_from_slice(data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"))
            .written;
        assert_eq!(written as usize, data.len(), "short write at {off}");
    }

    async fn fuse_read(&self, ino: u64, off: u64, len: usize) -> Vec<u8> {
        self.fs
            .read(req(), ino, 0, off, len as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read ino {ino} at {off} failed: {e:?}"))
            .data
            .to_vec()
    }

    /// A staged-layout file of `pattern(tag, len)` PROMOTED into the open
    /// pack through the fsync lever (`promote_staged_file`, the merge
    /// worker's / dismount pass's own primitive). Returns `(ino, file_id,
    /// mapping)`; the ring entry is gone, `block_map[0]` is the tenant.
    async fn packed_tenant(&self, name: &str, len: usize, tag: usize) -> (u64, String, String) {
        let packed0 = metric(&METRICS.layout_promoted_packed);
        let ino = self.create(name).await;
        self.write_at(ino, 0, &pattern(tag, len)).await;
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        let fid = m.file_id.as_deref().expect("file_id").to_string();
        self.fs
            .fsync(req(), ino, 0, false)
            .await
            .unwrap_or_else(|e| panic!("fsync ino {ino} failed: {e:?}"));
        assert_eq!(
            metric(&METRICS.layout_promoted_packed),
            packed0 + 1,
            "fixture premise: {name} promoted INTO the pack"
        );
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_none(),
            "fixture premise: the promotion released the ring entry"
        );
        let mapping = self.mapping0(ino);
        (ino, fid, mapping)
    }

    /// The RAM layout's `block_map[0]` (the prelude's binding authority).
    fn mapping0(&self, ino: u64) -> String {
        self.fs
            .router
            .metadata_cache
            .get(&ino)
            .expect("RAM layout")
            .block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[0]"))
    }

    /// `(base device offset, rel_off, packed_len)` of a tenant mapping
    /// through the router's own decoder.
    fn decode(&self, mapping: &str) -> (u64, u64, usize) {
        let (base, off, sz, exact) = self
            .fs
            .router
            .parse_block_mapping(mapping)
            .expect("tenant mapping decodes");
        assert!(exact, "a tenant mapping is size-carrying: {mapping}");
        (base, off, sz)
    }

    /// The bytes the DEFAULT device holds at `[dev_off, dev_off + len)`.
    fn default_device_bytes(&self, dev_off: u64, len: usize) -> Vec<u8> {
        let mut f = std::fs::File::open(&self.dev_paths[0]).expect("open default device file");
        f.seek(std::io::SeekFrom::Start(dev_off)).unwrap();
        let mut out = vec![0u8; len];
        f.read_exact(&mut out).unwrap();
        out
    }

    async fn stats(&self) -> serde_json::Value {
        serde_json::from_str(&self.fs.generate_stats_json().await).expect("stats json parses")
    }
}

// ---------------------------------------------------------------------------
// the raw-protocol client (the ipc_direct_drive_tests harness shape)
// ---------------------------------------------------------------------------

/// O_DIRECT stand-in on a real filesystem (the repo dir — /tmp is often
/// tmpfs, which refuses O_DIRECT); re-points the host's expected st_dev
/// and registers the ino translation.
fn odirect_standin(fx: &Fx, dir: &TempDir, name: &str, fs_ino: u64) -> OwnedFd {
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
    fn establish(fx: &Fx, fd: &OwnedFd) -> (ClientSession, u64) {
        let sock = abstract_connect(&fx.cfg.socket_name).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("SO_RCVTIMEO");
        send_ctl(
            &sock,
            &CtlMsg::Hello {
                abi: squeezefs_ipc::layout::IPC_ABI,
                pid: std::process::id(),
                // SAFETY: getuid is trivially safe.
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

// ---------------------------------------------------------------------------
// the ledger
// ---------------------------------------------------------------------------

struct Deltas {
    dd_serves: u64,
    dd_bounces: u64,
    dd_fallbacks_post: u64,
    handoffs: u64,
    ops_read: u64,
    packed_serves: u64,
    packed_bytes: u64,
    packed_reads: u64,
    packed_read_bytes: u64,
    inel_meta: u64,
    inel_overlay: u64,
    inel_packed_shape: u64,
    escalations: u64,
}

fn snap() -> Deltas {
    Deltas {
        dd_serves: metric(&METRICS.ipc_direct_drive_serves),
        dd_bounces: metric(&METRICS.ipc_direct_drive_bounces),
        dd_fallbacks_post: metric(&METRICS.ipc_direct_drive_fallbacks_post),
        handoffs: metric(&METRICS.ipc_async_handoffs),
        ops_read: metric(&METRICS.ipc_ops_read),
        packed_serves: metric(&METRICS.ipc_direct_packed_serves),
        packed_bytes: metric(&METRICS.ipc_direct_packed_bytes),
        packed_reads: metric(&METRICS.packed_reads),
        packed_read_bytes: metric(&METRICS.packed_read_bytes),
        inel_meta: metric(&METRICS.ipc_direct_ineligible_meta),
        inel_overlay: metric(&METRICS.ipc_direct_ineligible_overlay),
        inel_packed_shape: metric(&METRICS.ipc_direct_ineligible_packed_shape),
        escalations: METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
    }
}

fn delta(before: &Deltas) -> Deltas {
    let now = snap();
    Deltas {
        dd_serves: now.dd_serves - before.dd_serves,
        dd_bounces: now.dd_bounces - before.dd_bounces,
        dd_fallbacks_post: now.dd_fallbacks_post - before.dd_fallbacks_post,
        handoffs: now.handoffs - before.handoffs,
        ops_read: now.ops_read - before.ops_read,
        packed_serves: now.packed_serves - before.packed_serves,
        packed_bytes: now.packed_bytes - before.packed_bytes,
        packed_reads: now.packed_reads - before.packed_reads,
        packed_read_bytes: now.packed_read_bytes - before.packed_read_bytes,
        inel_meta: now.inel_meta - before.inel_meta,
        inel_overlay: now.inel_overlay - before.inel_overlay,
        inel_packed_shape: now.inel_packed_shape - before.inel_packed_shape,
        escalations: now.escalations - before.escalations,
    }
}

fn phase_count(v: &serde_json::Value, phase: &str) -> u64 {
    v.get("metrics")
        .expect("stats JSON carries a metrics object")
        .get("ipc_direct_phase_ns")
        .unwrap_or_else(|| panic!("stats inode must export ipc_direct_phase_ns (phase {phase})"))
        .get(phase)
        .unwrap_or_else(|| panic!("ipc_direct_phase_ns must carry phase '{phase}'"))["count"]
        .as_u64()
        .expect("histogram count word")
}

fn now_ns() -> u64 {
    squeezefs::mono_core::monotonic_ns_u64()
}

// ---------------------------------------------------------------------------
// 1. a packed tenant at a NON-ZERO off on the default volume direct-drives
//    byte-exact on every window shape; the window arithmetic is the slot's
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_packed_tenant_at_a_nonzero_off_direct_drives_byte_exact_on_every_window() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "nonzero-off", "none").await;

    // Three tenants of one pack: t0 at slot 0, t1 (a NON-grain length) at
    // 16 KiB, t2 (64 KiB) at 16 KiB + ceil(50 000).
    let (_t0, _, m0) = fx.packed_tenant("t0.bin", 16 * KIB, 10).await;
    let (t1, _, m1) = fx.packed_tenant("t1.bin", 50_000, 11).await;
    let (t2, _, m2) = fx.packed_tenant("t2.bin", 64 * KIB, 12).await;
    let (base0, off0, _) = fx.decode(&m0);
    let (base1, off1, sz1) = fx.decode(&m1);
    let (base2, off2, sz2) = fx.decode(&m2);
    assert_eq!((base0, base1, base2), (base0, base0, base0), "one pack block");
    assert_eq!(off0, 0);
    assert_eq!(off1, 16 * KIB as u64, "t1 sits after t0's slot");
    assert_eq!(
        off2,
        off1 + pack_slot_len(50_000),
        "t2 sits after t1's GRAIN-rounded slot"
    );
    assert_eq!((sz1, sz2), (50_000, 64 * KIB));

    // The plan (RED today: the prelude refuses the promoted STAGED
    // tenant): ONE ranged read on the tenant's own slot.
    let snap_full = fx
        .fs
        .ipc_direct_read_probe(t2, 0, sz2 as u32, now_ns())
        .expect("a packed tenant on a passthrough volume plans a direct read");
    assert_eq!(
        snap_full.dev_off,
        base2 + off2,
        "the plan's device base is the TENANT's (block base + rel_off)"
    );
    assert_eq!(
        snap_full.window_cap,
        pack_slot_len(sz2 as u64),
        "the window is capped at the tenant's slot, never the block"
    );
    assert_eq!(snap_full.key, m2, "the snapshot binds the tenant mapping");
    assert!(
        snap_full.packed_file_id.is_some(),
        "the snapshot is the packed arm's"
    );
    assert!(fx.fs.ipc_direct_revalidate(&snap_full));
    // t1's tail: the request ends INSIDE a grain — the window rounds out
    // to the slot's end (`ceil_grain(50 000)`), still inside the slot.
    let snap_tail = fx
        .fs
        .ipc_direct_read_probe(t1, 45_056, (sz1 - 45_056) as u32, now_ns())
        .expect("a tail request inside the tenant plans");
    assert_eq!(snap_tail.dev_off, base1 + off1);
    assert_eq!(snap_tail.window_cap, pack_slot_len(50_000));
    assert!(
        off1 + snap_tail.window_cap <= off2,
        "t1's window can never reach t2's slot"
    );

    // The ring: every window shape serves byte-exact on the direct path.
    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd2 = odirect_standin(&fx, &sdir, "t2.bin", t2);
    let (s2, b2) = ClientSession::establish(&fx, &fd2);
    fx.fs.router.set_direct_device_true(true);
    let want2 = pattern(12, sz2);
    // (offset, len, arena_off): full tenant, head, tail, interior
    // (LBA-unaligned offset ⇒ the bounce leg), an unaligned arena dest.
    let shapes: &[(u64, usize, u64)] = &[
        (0, sz2, 0),
        (0, 4096, 0),
        ((sz2 - 4096) as u64, 4096, 0),
        (8192 + 512, 4096, 0),
        (16 * 4096, 8192, 512),
    ];
    let before = snap();
    for (i, (off, len, arena)) in shapes.iter().enumerate() {
        let got = tokio::task::block_in_place(|| {
            s2.ring_pread(b2, *off, *len, *arena, "packed direct read")
        });
        assert_eq!(
            got,
            want2[*off as usize..*off as usize + len].to_vec(),
            "byte parity on packed direct-drive shape {i} (off {off} len {len})"
        );
    }
    let d = delta(&before);
    let n = shapes.len() as u64;
    let bytes: u64 = shapes.iter().map(|(_, l, _)| *l as u64).sum();
    assert_eq!(
        d.dd_serves, n,
        "every packed read must be SERVED direct-drive (no task, no handler) — got {} of {n}",
        d.dd_serves
    );
    assert_eq!(d.packed_serves, n, "ipc_direct_packed_serves counts the arm");
    assert_eq!(d.packed_bytes, bytes, "ipc_direct_packed_bytes = the request bytes");
    assert_eq!(d.ops_read, n, "ipc_ops_read accounts direct packed serves");
    assert_eq!(d.handoffs, 0, "no async handoff on the packed arm");
    assert_eq!(d.dd_fallbacks_post, 0, "a quiet tenant never fails revalidation");
    assert_eq!(
        d.packed_reads, n,
        "the funnel's engagement gauge stays live on the direct arm"
    );
    assert!(
        d.packed_read_bytes >= bytes && d.packed_read_bytes <= n * pack_slot_len(sz2 as u64),
        "packed_read_bytes is the GRAIN window, bounded by the slot"
    );
    assert_eq!(d.dd_bounces, 2, "the two unaligned shapes ride the bounce leg");
    drop(s2);

    // t1's tail: the served bytes stop at the tenant's end even though the
    // device window ran to the slot's end.
    let fd1 = odirect_standin(&fx, &sdir, "t1.bin", t1);
    let (s1, b1) = ClientSession::establish(&fx, &fd1);
    let before = snap();
    let got = tokio::task::block_in_place(|| {
        s1.ring_pread(b1, 45_056, sz1 - 45_056, 0, "packed tail read")
    });
    assert_eq!(got, pattern(11, sz1)[45_056..].to_vec(), "tail parity");
    let d = delta(&before);
    assert_eq!(d.packed_serves, 1);
    assert_eq!(d.packed_bytes, (sz1 - 45_056) as u64);
    fx.fs.router.set_direct_device_true(false);
    drop(s1);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 2. FIND-PK-0 for the planner: the device fd is the MAPPING's backend
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_on_a_non_default_volume_direct_drives_from_its_own_device() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 2, "second-vol", "none").await;
    fx.place_only_on(1);

    let (_t0, _, _) = fx.packed_tenant("v0.bin", 16 * KIB, 20).await;
    let (t1, _, m1) = fx.packed_tenant("v1.bin", 32 * KIB, 21).await;
    assert!(
        m1.starts_with(&format!("{}://", fx.records[1].id)),
        "premise: the tenant was placed on the SECOND volume ({m1})"
    );
    let (base1, off1, sz1) = fx.decode(&m1);
    assert_eq!(off1, 16 * KIB as u64, "non-zero slot");

    let plan = fx
        .fs
        .ipc_direct_read_probe(t1, 4096, 8192, now_ns())
        .expect("a tenant on a registered non-default volume plans");
    assert_eq!(
        plan.be_id.as_str(),
        fx.records[1].id,
        "the plan names the MAPPING's backend — never the default slot"
    );
    assert_eq!(plan.dev_off, base1 + off1);

    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "v1.bin", t1);
    let (s, b) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);
    let want = pattern(21, sz1);
    let before = snap();
    for (off, len) in [(0u64, sz1), (4096, 8192), (sz1 as u64 - 4096, 4096)] {
        let got =
            tokio::task::block_in_place(|| s.ring_pread(b, off, len, 0, "second-volume read"));
        assert_eq!(
            got,
            want[off as usize..off as usize + len].to_vec(),
            "byte parity through the direct path on the second volume (off {off})"
        );
        // The default device holds something ELSE at that offset (or
        // nothing) — the old arm's wrong-device read is what FIND-PK-0
        // caught.
        assert_ne!(
            fx.default_device_bytes(base1 + off1 + off, len),
            want[off as usize..off as usize + len].to_vec(),
            "the default device does not hold the tenant's bytes at this offset"
        );
    }
    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(d.dd_serves, 3, "all three reads served direct-drive");
    assert_eq!(d.packed_serves, 3);
    assert_eq!(d.handoffs, 0);
    drop(s);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 3. a request crossing the tenant's end is refused; the handler serves the
//    short read exactly to size — never a neighbour's bytes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_crossing_the_tenant_end_is_refused_and_served_short_by_the_handler() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "eof-cross", "none").await;

    let (t0, _, _) = fx.packed_tenant("e0.bin", 50_000, 30).await;
    // A NEIGHBOUR right after t0's slot — the bytes a window that ran past
    // the slot would hand back.
    let (_t1, _, m1) = fx.packed_tenant("e1.bin", 16 * KIB, 31).await;
    let (_, off1, _) = fx.decode(&m1);
    assert_eq!(off1, pack_slot_len(50_000), "the neighbour starts at t0's slot end");

    // (a) EOF inside the request window (size == image): PackedShape.
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(t0, 45_056, 8192, now_ns()),
            Err(IpcDirectIneligible::PackedShape)
        ),
        "a request past the tenant's end must be refused as PackedShape"
    );
    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "e0.bin", t0);
    let (s, b) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);
    let before = snap();
    let got = tokio::task::block_in_place(|| s.ring_pread(b, 45_056, 8192, 0, "eof cross"));
    assert_eq!(
        got,
        pattern(30, 50_000)[45_056..].to_vec(),
        "the handler serves exactly to size — 4 944 bytes, never the neighbour's"
    );
    let d = delta(&before);
    assert_eq!(d.packed_serves, 0, "EOF-crossing shapes never direct-drive");
    assert_eq!(d.dd_serves, 0);
    assert!(d.inel_packed_shape >= 1, "the ledger names the refusal class");
    assert!(d.handoffs >= 1, "the op rode the handler");

    // (b) truncate-UP: size > image — the tail past the image is an
    // implicit-zero hole the handler fills; the arm refuses the shape.
    fx.fs
        .setattr(
            req(),
            t0,
            None,
            fuse3::SetAttr {
                size: Some(60_000),
                ..Default::default()
            },
        )
        .await
        .expect("truncate up");
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(t0, 45_056, 8192, now_ns()),
            Err(IpcDirectIneligible::PackedShape)
        ),
        "a window past the IMAGE (inside the size) is the same refusal"
    );
    let before = snap();
    let got = tokio::task::block_in_place(|| s.ring_pread(b, 45_056, 8192, 0, "image cross"));
    let mut want = pattern(30, 50_000)[45_056..].to_vec();
    want.resize(8192, 0);
    assert_eq!(got, want, "zeros past the image, never the neighbour's bytes");
    let d = delta(&before);
    assert_eq!(d.packed_serves, 0);
    assert!(d.inel_packed_shape >= 1);
    // …while a window INSIDE the image still direct-drives after the
    // truncate-up (the size grew; the bytes below the image are unchanged).
    let before = snap();
    let got = tokio::task::block_in_place(|| s.ring_pread(b, 4096, 4096, 0, "inside"));
    assert_eq!(got, pattern(30, 50_000)[4096..8192].to_vec());
    let d = delta(&before);
    assert_eq!(d.packed_serves, 1, "an in-image window keeps the arm");
    fx.fs.router.set_direct_device_true(false);
    drop(s);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 4. a transformed volume's tenant stays on the handler
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transformed_volumes_tenant_stays_on_the_handler_byte_exact() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "lz4", "lz4").await;
    assert!(
        !fx.fs.router.get_crypto().is_passthrough(),
        "premise: a transformed volume"
    );

    let (t0, _, _) = fx.packed_tenant("c0.bin", 16 * KIB, 40).await;
    let (t1, _, m1) = fx.packed_tenant("c1.bin", 32 * KIB, 41).await;
    let (_, off1, _) = fx.decode(&m1);
    assert!(off1 > 0, "premise: a non-zero slot ({m1})");
    let _ = t0;

    // The image must decode WHOLE: the ranged arm is not the shape. The
    // refusal is the prelude's shared transform screen.
    let probe = fx.fs.ipc_direct_read_probe(t1, 4096, 8192, now_ns());
    assert!(
        matches!(
            probe,
            Err(IpcDirectIneligible::Meta) | Err(IpcDirectIneligible::Layout)
        ),
        "a transformed volume's tenant must not plan a ranged direct read: {probe:?}"
    );

    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "c1.bin", t1);
    let (s, b) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);
    let before = snap();
    let got = tokio::task::block_in_place(|| s.ring_pread(b, 4096, 8192, 0, "transformed"));
    assert_eq!(
        got,
        pattern(41, 32 * KIB)[4096..12288].to_vec(),
        "the handler decodes the slot and serves byte-exact"
    );
    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(d.dd_serves, 0, "transformed volumes never direct-drive");
    assert_eq!(d.packed_serves, 0);
    assert!(d.handoffs >= 1);
    drop(s);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 5. a newer ring-resident image: Overlay at the prelude, refused at the
//    CQE, the handler serves the NEW bytes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_ring_resident_image_refuses_the_arm_and_the_handler_serves_it() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "ring-newer", "none").await;

    let (_t0, _, _) = fx.packed_tenant("r0.bin", 16 * KIB, 50).await;
    let (t1, fid1, m1) = fx.packed_tenant("r1.bin", 32 * KIB, 51).await;
    let snap0 = fx
        .fs
        .ipc_direct_read_probe(t1, 4096, 8192, now_ns())
        .expect("a quiet tenant plans");
    assert!(fx.fs.ipc_direct_revalidate(&snap0));

    // The RMW's mid-write state: a NEWER image staged under the tenant's
    // file_id while `block_map[0]` still names the durable slot (the
    // publish that drops the mapping has not run yet).
    let newer = pattern(99, 32 * KIB);
    fx.fs
        .router
        .cache
        .nvme
        .stage_write(
            &squeezefs::keys::inode_path(t1),
            &fid1,
            bytes::Bytes::copy_from_slice(&newer),
            7,
        )
        .await
        .expect("stage the newer image");
    assert_eq!(fx.mapping0(t1), m1, "premise: the durable slot is still bound");

    assert!(
        !fx.fs.ipc_direct_revalidate(&snap0),
        "a newer ring image landing between plan and CQE must fail revalidation"
    );
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(t1, 4096, 8192, now_ns()),
            Err(IpcDirectIneligible::Overlay)
        ),
        "the prelude refuses a tenant with a live newer image (Overlay)"
    );

    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "r1.bin", t1);
    let (s, b) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);
    let before = snap();
    let got = tokio::task::block_in_place(|| s.ring_pread(b, 4096, 8192, 0, "newer image"));
    assert_eq!(
        got,
        newer[4096..12288].to_vec(),
        "the handler serves the NEW bytes (ring first)"
    );
    let d = delta(&before);
    fx.fs.router.set_direct_device_true(false);
    assert_eq!(d.packed_serves, 0, "the stale slot must NOT be served");
    assert!(d.inel_overlay >= 1, "the overlay ledger records the refusal");
    drop(s);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 6. the lever: 0 = today's handler fallback, byte-identical
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_is_todays_handler_fallback_byte_identical() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "lever-off", "none").await;

    let (_t0, _, _) = fx.packed_tenant("l0.bin", 16 * KIB, 60).await;
    let (t1, _, _) = fx.packed_tenant("l1.bin", 32 * KIB, 61).await;

    set_ipc_dd_packed_enabled(false);
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(t1, 4096, 8192, now_ns()),
            Err(IpcDirectIneligible::Meta)
        ),
        "lever off = the pre-PK8 refusal verbatim (a non-striped layout is `meta`)"
    );
    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "l1.bin", t1);
    let (s, b) = ClientSession::establish(&fx, &fd);
    fx.fs.router.set_direct_device_true(true);
    let before = snap();
    let off_bytes = tokio::task::block_in_place(|| s.ring_pread(b, 4096, 8192, 0, "lever off"));
    let d = delta(&before);
    assert_eq!(off_bytes, pattern(61, 32 * KIB)[4096..12288].to_vec());
    assert_eq!(d.packed_serves, 0, "the packed gauge stays silent under 0");
    assert_eq!(d.dd_serves, 0);
    assert!(d.inel_meta >= 1);
    assert!(d.handoffs >= 1, "the op rides the handler under 0");

    set_ipc_dd_packed_enabled(true);
    let before = snap();
    let on_bytes = tokio::task::block_in_place(|| s.ring_pread(b, 4096, 8192, 0, "lever on"));
    let d = delta(&before);
    assert_eq!(on_bytes, off_bytes, "the two arms are byte-identical");
    assert_eq!(d.packed_serves, 1, "the arm engages under 1");
    assert_eq!(d.handoffs, 0);
    fx.fs.router.set_direct_device_true(false);
    drop(s);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 7. the CQE revalidation: an incarnation transition between plan and CQE
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cqe_revalidation_refuses_an_incarnation_transition_between_plan_and_cqe() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "remint", "none").await;

    let (_t0, _, _) = fx.packed_tenant("i0.bin", 16 * KIB, 70).await;
    let (t1, _, m1) = fx.packed_tenant("i1.bin", 32 * KIB, 71).await;
    let (base, _, _) = fx.decode(&m1);
    let alloc = fx.alloc(0);
    assert!(
        alloc.fill_incarnation(base).is_some(),
        "premise: the pack block's word is STABLE while open (KD-3)"
    );

    let snap0 = fx
        .fs
        .ipc_direct_read_probe(t1, 8192, 4096, now_ns())
        .expect("a quiet tenant plans");
    assert!(snap0.tracked, "a pack block is allocator-tracked");
    assert!(fx.fs.ipc_direct_revalidate(&snap0));

    // The re-mint model: the block's word is RETIRED (content changing
    // under the key — a shared pack refuses the sole-owner patch, but the
    // word has already transitioned) …
    assert!(
        !alloc.begin_patch_sole_owner(base),
        "premise: a shared pack block is never sole-owned"
    );
    assert!(
        matches!(
            fx.fs.ipc_direct_read_probe(t1, 8192, 4096, now_ns()),
            Err(IpcDirectIneligible::Overlay)
        ),
        "the prelude refuses while the word is unstable"
    );
    // … and re-published under a NEW generation: a plan taken under the
    // old generation must fail its CQE revalidation.
    alloc.publish_block(base);
    assert!(
        !fx.fs.ipc_direct_revalidate(&snap0),
        "a retire→publish transition between plan and CQE forces the handler fallback"
    );
    let snap1 = fx
        .fs
        .ipc_direct_read_probe(t1, 8192, 4096, now_ns())
        .expect("a fresh plan under the new generation");
    assert!(fx.fs.ipc_direct_revalidate(&snap1));

    // A layout flip (truncate to zero) invalidates every plan.
    fx.fs
        .setattr(
            req(),
            t1,
            None,
            fuse3::SetAttr {
                size: Some(0),
                ..Default::default()
            },
        )
        .await
        .expect("truncate");
    assert!(!fx.fs.ipc_direct_revalidate(&snap1));
    fx.close().await;
}

// ---------------------------------------------------------------------------
// 8. the default mount: the governed-miss ladder engages the arm, re-reads
//    keep direct-driving, and `ipc_direct_phase_ns` accounts every op
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_packed_direct_op_records_its_residence_on_a_default_mount() {
    let _g = serial().await;
    let _l = arm_levers();
    let dir = tempfile::tempdir().unwrap();
    let fx = open_fresh(dir.path(), 1, "default-phases", "none").await;

    let (_t0, _, _) = fx.packed_tenant("p0.bin", 16 * KIB, 80).await;
    let (t1, _, _) = fx.packed_tenant("p1.bin", 64 * KIB, 81).await;
    let sdir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &sdir, "p1.bin", t1);
    let (s, b) = ClientSession::establish(&fx, &fd);
    // DEFAULT posture: direct_device_true stays false.

    let stats0 = fx.stats().await;
    let before_total = phase_count(&stats0, "total");
    let before_admit = phase_count(&stats0, "admit");
    let want = pattern(81, 64 * KIB);
    let before = snap();
    // Two touches per window: the SECOND touch of a striped block would be
    // a GRANT-shaped escalation on the handler; a packed tenant has no
    // admission arm to be handed to and keeps direct-driving.
    let n = 6u64;
    for i in 0..n {
        let off = (i % 3) * 16 * 4096;
        let got = tokio::task::block_in_place(|| s.ring_pread(b, off, 4096, 0, "default packed"));
        assert_eq!(got, want[off as usize..off as usize + 4096].to_vec(), "op {i}");
    }
    let d = delta(&before);
    assert_eq!(
        d.dd_serves, n,
        "on a DEFAULT mount every governed packed miss direct-drives (got {})",
        d.dd_serves
    );
    assert_eq!(d.packed_serves, n);
    assert_eq!(d.handoffs, 0, "no GRANT diversion on re-reads");
    assert_eq!(d.escalations, 0, "a packed tenant never escalates");
    let stats1 = fx.stats().await;
    assert_eq!(
        phase_count(&stats1, "total") - before_total,
        n,
        "ipc_direct_phase_ns.total accounts every packed direct op"
    );
    assert_eq!(phase_count(&stats1, "admit") - before_admit, n);
    let m = stats1.get("metrics").expect("metrics");
    for key in [
        "ipc_direct_packed_serves",
        "ipc_direct_packed_bytes",
        "ipc_direct_ineligible_packed_shape",
    ] {
        assert!(m.get(key).is_some(), "stats inode must export {key}");
    }
    drop(s);
    fx.close().await;
}

/// The slot arithmetic the plan rides is the wire law's (`LBA_GRAIN`, the
/// allocator chunk) — pinned so a grain change cannot drift one side.
#[test]
fn the_plan_window_grain_is_the_wire_laws() {
    assert_eq!(GRAIN, 4096);
    assert_eq!(pack_slot_len(1), GRAIN);
    assert_eq!(pack_slot_len(50_000), 53_248);
    assert!(pack_slot_len(CHUNK_SIZE) == CHUNK_SIZE);
    assert_eq!(clean_block_key("4194304@1a:16384:50000"), "4194304@1a");
}
