//! Read-saturation campaign (2026-07-29) — ring reads feed the stream
//! machinery (`docs/design-read-path.md` §5.3/§5.5 classifier + pipeline).
//!
//! The field signature this file pins closed: sequential 4k il reads
//! burst at the warm RAM-tier ceiling for a few seconds, then collapse
//! to the cold-miss fabric-RTT floor (user's 4-node 2×200GbE cluster:
//! 900k–1.2M → 200–300k sustained; reproduced exactly on the rsat
//! nvmet-tcp rig: 1.8M → ~560k). Root cause, measured: the §5.3 stream
//! classifier and the §5.5 prefetch pipeline are fed ONLY from the
//! kernel-lane read handler (`pipeline_touch` in
//! `read_file_range_zero_copy`) — ring ops never observe into the
//! lanes, so a sequential il stream never classifies
//! (`read_streams_classified = 0` on the baseline row), the §5.6
//! streaming veto never engages, and every 4k miss pays one ranged
//! fabric round trip via direct-drive forever (`prefetch_issued = 0`,
//! 11.7M direct-drive serves on the 25 s baseline row).
//!
//! The contract:
//! 1. EVERY ring read of a striped file feeds the stream lanes exactly
//!    once, at the sink — warm fast-path serves included (silent
//!    consumption must advance the lane's consume edge or the pipeline
//!    wedges at its window), and BEFORE the miss ladder runs (the 4th
//!    contiguous op's classification must veto direct-drive for the
//!    5th).
//! 2. A classified il stream rides whole-block fetches + the prefetch
//!    pipeline: bounded ranged window reads, `get_obj` ≈ one whole-block
//!    fetch per unique block, the bulk of ops as sync fast-path serves.
//! 3. Ring ops that demote to the handler must NOT feed twice (the
//!    handler's own `pipeline_touch` skips ring-originated requests) —
//!    double-observation looks like foreign traffic and declassifies
//!    the very stream that routed them (the §5.3 16-foreign rule).
//!
//! Harness: the ipc_direct_drive_tests raw-protocol client (no kernel in
//! the loop; the daemon side is the REAL host → service → sink path).

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
// fixture (the ipc_direct_drive_tests shape; larger read-mem so the hot
// tier holds a whole stream's working set: 128 MB read mem ⇒ 32 MiB hot
// budget ⇒ 8 default blocks)
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
        BlockAllocator::new(dlm.meta_client().clone(), "read_saturation_tests")
            .await
            .expect("block allocator"),
    );
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("128MB"),
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
            socket_name: format!("sqz-il0-rsat-{}-{}", std::process::id(), name),
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

    /// Shift ino allocation (fresh v3 volumes hand out the same monotonic
    /// inos; the local DLM lock map is process-global).
    async fn salt_inos(&self, n: usize) {
        for i in 0..n {
            let name = format!("salt{i}");
            self.fs
                .create(req(), 1, OsStr::new(&name), libc::S_IFREG | 0o644, 0)
                .await
                .expect("salt create");
        }
    }

    /// Striped fixture: complete blocks written highest-offset-first.
    async fn create_striped(&self, name: &str, blocks: &[u32]) -> u64 {
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

    fn shutdown(&self) {
        self.host.shutdown();
    }
}

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

/// Purge every block-key tier so the sync fast path genuinely misses.
fn purge_tiers(fx: &Fixture, ino: u64) -> std::collections::HashMap<u32, String> {
    let meta = fx
        .fs
        .router
        .metadata_cache
        .get(&ino)
        .expect("metadata cache entry must be resident");
    let map = meta.block_map.clone().expect("striped fixture map");
    for key in map.values() {
        fx.fs.router.cache.purge_block_key(key);
    }
    (*map).clone()
}

fn deterministic_bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64).wrapping_mul(31).wrapping_add(seed * 17) % 251) as u8)
        .collect()
}

// ---------------------------------------------------------------------------
// raw-protocol client session
// ---------------------------------------------------------------------------

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
// counter snapshots
// ---------------------------------------------------------------------------

struct Deltas {
    classified: u64,
    dd_serves: u64,
    fast_path: u64,
    handoffs: u64,
    ranged: u64,
    prefetch_issued: u64,
    get_obj: u64,
}

fn snap() -> Deltas {
    Deltas {
        classified: METRICS.read_streams_classified.load(Ordering::Relaxed),
        dd_serves: METRICS.ipc_direct_drive_serves.load(Ordering::Relaxed),
        fast_path: METRICS.ipc_fast_path_serves.load(Ordering::Relaxed),
        handoffs: METRICS.ipc_async_handoffs.load(Ordering::Relaxed),
        ranged: METRICS.ranged_reads.load(Ordering::Relaxed),
        prefetch_issued: METRICS.prefetch_issued.load(Ordering::Relaxed),
        get_obj: METRICS.get_obj.load(Ordering::Relaxed),
    }
}

fn delta(b: &Deltas) -> Deltas {
    let a = snap();
    Deltas {
        classified: a.classified - b.classified,
        dd_serves: a.dd_serves - b.dd_serves,
        fast_path: a.fast_path - b.fast_path,
        handoffs: a.handoffs - b.handoffs,
        ranged: a.ranged - b.ranged,
        prefetch_issued: a.prefetch_issued - b.prefetch_issued,
        get_obj: a.get_obj - b.get_obj,
    }
}

/// Stream `blocks` whole blocks of `ino` as sequential 4 KiB ring reads,
/// verifying byte parity on every op (block patterns precomputed once —
/// a per-op 4 MiB pattern regeneration dominated the debug-build wall
/// clock and would time the suite out).
fn stream_4k(session: &ClientSession, binding: u64, blocks: u64, what: &str) {
    let patterns: Vec<Vec<u8>> = (0..blocks)
        .map(|b| deterministic_bytes(BS as usize, 100 + b))
        .collect();
    for op in 0..(blocks * (BS / 4096)) {
        let off = op * 4096;
        let got = session.ring_pread(binding, off, 4096, 0, what);
        let block = (off / BS) as usize;
        let rel = (off % BS) as usize;
        assert_eq!(
            got,
            &patterns[block][rel..rel + 4096],
            "{what}: byte parity at op {op} (offset {off})"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. THE collapse pin: with the admission governor clamped (the
//    beyond-budget churn regime — the field's sustained state), a
//    sequential 4k O_DIRECT ring stream must classify at the 4th
//    contiguous op and ride whole-block fetches + the prefetch pipeline
//    from the 5th on — never one ranged fabric round trip per op.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clamped_sequential_ring_stream_classifies_and_rides_whole_blocks() {
    let fx = Fixture::new("seq-clamped").await;
    fx.salt_inos(3).await;
    let ino = fx.create_striped("seqclamp.bin", &[0, 1, 2, 3]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "seqclamp.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Engage the clamp deterministically (zero-payback whole-block
    // victims, empty token grant): on the baseline binary EVERY
    // ghost-hit miss is a governor-denied direct-drive — the field's
    // sustained-collapse state.
    let gov = &fx.fs.router.cache.admission_governor;
    let block = fx.fs.router.block_size.load(Ordering::Relaxed);
    for _ in 0..2 {
        gov.on_eviction(
            block,
            &squeezefs::tiering::memory::EvictClass::Protected { served_bytes: 0 },
        );
    }

    let before = snap();
    let blocks = 3u64;
    tokio::task::block_in_place(|| stream_4k(&session, binding, blocks, "clamped seq stream"));
    let d = delta(&before);
    let ops = blocks * (BS / 4096);

    assert_eq!(
        d.classified, 1,
        "a sequential ring stream must classify exactly ONCE (0 = ring ops \
         never feed the lanes — the field collapse; >1 = the lane keeps \
         getting destroyed, e.g. handler double-feeding declassifies)"
    );
    assert!(
        d.dd_serves <= 8,
        "after classification (4 contiguous ops) misses must ride the \
         whole-block handler path, never per-op direct-drive (got {} \
         direct-drive serves for {ops} ops)",
        d.dd_serves
    );
    assert!(
        d.ranged <= 8,
        "a classified stream pays at most the pre-classification ranged \
         window reads (got {})",
        d.ranged
    );
    assert!(
        d.get_obj <= 2 * blocks + 8,
        "device fetches must be whole-block-per-unique-block class, not \
         per-op ({} device read ops for {blocks} blocks)",
        d.get_obj
    );
    assert!(
        d.prefetch_issued >= 1,
        "the §5.5 pipeline must engage from a ring-classified stream \
         (prefetch_issued = 0 is the baseline collapse signature)"
    );
    assert!(
        d.fast_path >= ops - 64,
        "the bulk of a pipelined stream serves on the sync fast path \
         ({} of {ops})",
        d.fast_path
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 2. Unclamped stream: same law without the clamp (escalations GRANT on
//    this shape today, which hides the collapse in-process — the pin is
//    classification + pipeline engagement + fetch economy).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_ring_stream_engages_the_prefetch_pipeline() {
    let fx = Fixture::new("seq-open").await;
    fx.salt_inos(11).await;
    let ino = fx.create_striped("seqopen.bin", &[0, 1, 2, 3]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "seqopen.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let before = snap();
    let blocks = 3u64;
    tokio::task::block_in_place(|| stream_4k(&session, binding, blocks, "open seq stream"));
    let d = delta(&before);
    let ops = blocks * (BS / 4096);

    assert_eq!(d.classified, 1, "exactly one classification per stream");
    assert!(
        d.prefetch_issued >= 1,
        "pipeline engaged (prefetch_issued = {})",
        d.prefetch_issued
    );
    assert!(
        d.get_obj <= 2 * blocks + 8,
        "fetch economy: {} device read ops for {blocks} unique blocks",
        d.get_obj
    );
    assert!(
        d.fast_path >= ops - 64,
        "warm serves dominate ({} of {ops})",
        d.fast_path
    );
    fx.shutdown();
}

// ---------------------------------------------------------------------------
// 3. Buffered ring streams classify exactly once too (classification is
//    behavior-based — the hybrid directive; and the anti-double-feed
//    pin: on the baseline the HANDLER feeds these lanes for every
//    buffered miss, so a sink-side feed that does not suppress the
//    handler's observation destroys its own classification).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 4. Warm-path economy: fully-resident ring traffic must not pay the
//    lane-claim machinery. A warm serve can CONTINUE a lane a miss
//    started (the granted-escalation regime classifies through warm
//    ops 3-4), but it must never CLAIM one: a fully-warm stream has
//    nothing to prefetch, and on the 1M-IOPS warm rows the per-op
//    foreign-scan/claim path measured -6..-16 % vs the
//    SQUEEZEFS_READ_PREFETCH_WINDOW=0 kill switch (rsat rig,
//    interleaved same-binary A/B).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fully_warm_sequential_ring_rereads_stay_laneless() {
    let fx = Fixture::new("warm-laneless").await;
    fx.salt_inos(27).await;
    let ino = fx.create_striped("warmlane.bin", &[0, 1]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = odirect_standin(&fx, &dir, "warmlane.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    // Make block 0 resident WITHOUT classifying: two spaced (non-
    // contiguous) handler reads — the second is the ghost second touch
    // that escalates the whole-block admission. OFF the stream's 4 KiB
    // grid (+1 KiB), so the lanes these misses legitimately claim can
    // never be CONTINUED by the aligned warm stream below (continuing
    // a miss-started lane is allowed by design; this test pins the
    // CLAIM path only).
    let _ = fx
        .fs
        .read(req(), ino, 0, 64 * 4096 + 1024, 4096, libc::O_DIRECT as u32)
        .await
        .expect("warm-up read 1");
    let _ = fx
        .fs
        .read(
            req(),
            ino,
            0,
            128 * 4096 + 1024,
            4096,
            libc::O_DIRECT as u32,
        )
        .await
        .expect("warm-up read 2");

    // Sequential 4k ring re-reads over the resident block: every op is
    // a sync fast-path serve; NONE of them may claim a lane or
    // classify (nothing to prefetch on a fully-warm stream — and the
    // claim/foreign scan is the measured warm-row tax).
    let before = snap();
    let pattern = deterministic_bytes(BS as usize, 100);
    for op in 0..256u64 {
        let off = op * 4096;
        let got = session.ring_pread(binding, off, 4096, 0, "warm laneless stream");
        assert_eq!(
            got,
            &pattern[(off as usize)..(off as usize + 4096)],
            "byte parity at warm op {op}"
        );
    }
    let d = delta(&before);
    assert_eq!(
        d.fast_path, 256,
        "engagement: every op must be a warm fast-path serve"
    );
    assert_eq!(
        d.classified, 0,
        "fully-warm ring re-reads must never classify (a warm serve may \
         CONTINUE a miss-claimed lane, never CLAIM one)"
    );
    assert_eq!(d.get_obj, 0, "no device traffic on a fully-warm stream");
    fx.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffered_ring_stream_classifies_exactly_once() {
    let fx = Fixture::new("seq-buffered").await;
    fx.salt_inos(19).await;
    let ino = fx.create_striped("seqbuf.bin", &[0, 1, 2]).await;
    purge_tiers(&fx, ino);
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("repo-dir tempdir");
    let fd = buffered_standin(&fx, &dir, "seqbuf.bin", ino);
    let (session, binding) = ClientSession::establish(&fx, &fd);

    let before = snap();
    let blocks = 2u64;
    tokio::task::block_in_place(|| stream_4k(&session, binding, blocks, "buffered seq stream"));
    let d = delta(&before);
    let ops = blocks * (BS / 4096);

    assert_eq!(
        d.classified, 1,
        "buffered ring stream classifies exactly once (a handler \
         double-feed shows here as repeated classify/declassify cycles)"
    );
    assert_eq!(d.dd_serves, 0, "buffered bindings never direct-drive");
    assert!(
        d.get_obj <= 2 * blocks + 8,
        "fetch economy holds for buffered streams too ({} device ops)",
        d.get_obj
    );
    assert!(
        d.fast_path >= ops - 64,
        "warm serves dominate ({} of {ops})",
        d.fast_path
    );
    fx.shutdown();
}
