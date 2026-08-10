//! R3 of the zc-payload-retention program — the daemon ACK-early
//! posture over the device overlay (design-zc-write-kernel-v2 §6.2–6.4,
//! design-device-overlay §8's accelerator entry). Red-first: compiles
//! against the ack-early wiring (`set_ack_early_for_tests`, the
//! `ZcWriteSlot` retain/release closures, the test slot-wrap seam, the
//! `overlay_ack_early_*` metrics), which does not exist until R3 lands.
//!
//! The contracts, verbatim from the specs:
//!
//! * **ACK-early** — an eligible overlay store's write(2) returns while
//!   the DMA is in flight (the reply detaches from the store CQE; the
//!   retained slot carries the payload — kernel 0029's mechanism,
//!   live-verified by the kmbuf_smoke retention round-trip ×5).
//! * **CQE-anchored coverage** — `complete_store`/coverage publication
//!   stay anchored to the actual store completion, never the ACK
//!   (overlay law 3 unchanged).
//! * **The fsync law** (§3.4/§6.2 step 2) — a durability boundary
//!   awaits stores "the kernel believes are already done".
//! * **The read-wait law** (§8's future law, recorded before this PR) —
//!   a read intersecting an ACKed-but-incomplete store must wait for
//!   it; it may never serve the range as an uncovered gap (zeros).
//! * **Never-lossy** — an ACKed store that fails transiently retries
//!   until it lands (acked custody is retry-forever; only a fence
//!   drops it, counted on `overlay_fence_drops`).
//! * **Release exactly-once** — every retained slot is released once,
//!   success and failure arms alike (the transport ledger's closure
//!   law lives fuse3-side; here the closure-call count pins it).
//! * **The §3.4 stability-law class split** — sound-class (page-cache /
//!   buffered) writes ACK early 0-copy via COMMIT_RETAIN; O_DIRECT
//!   (GUP) writes only under the opt-in, which SNAPSHOTS (extract)
//!   before the reply so a post-ACK buffer reuse cannot change what
//!   lands (the 2026-08-09 live-smoke aliasing).
//!
//! Vehicle: the test slot-wrap seam (`set_test_zc_slot_wrap`) turns the
//! in-process `WritePayload::Bytes` into a mock `ZcWriteSlot` whose
//! store closure pwrites through the device's own zc fd — the SAME
//! install/claim/coverage/publish laws as the transport slot leg, with
//! a test-controlled gate/failure schedule. Backing files live under
//! `target/` (tmpfs refuses O_DIRECT, which the zc_write_fd screen
//! requires).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::device_overlay::{set_ack_early_for_tests, set_device_overlay_for_tests};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::{clear_test_zc_slot_wrap, set_test_zc_slot_wrap, DataRouter, ZcWriteSlot};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

const BS: u64 = 64 * 1024;

/// Process-global METRICS + seam state: serialize tests.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Per-test mock-slot control block: a store gate (closed ⇒ stores
/// park), a scripted failure count, and the retain/release call ledger.
struct SlotCtl {
    gate: tokio::sync::Semaphore,
    fail_first: AtomicU32,
    retains: AtomicUsize,
    releases: AtomicUsize,
    /// The §3.4 class the mock slot reports (true = page-cache-sound).
    sound: bool,
    /// Dead-vehicle script (the generic/464 unmount wedge): stores fail
    /// `Unsupported` FOREVER — the fuse3 "session not zc-armed" class a
    /// torn-down ring returns. Never counts against `fail_first`.
    store_dead: std::sync::atomic::AtomicBool,
    /// The extract half of the dead ring: `materialize()` fails
    /// `Unsupported` too (bytes unreachable — the honest-loss arm).
    extract_dead: std::sync::atomic::AtomicBool,
}

impl SlotCtl {
    fn new(sound: bool) -> Arc<Self> {
        Arc::new(SlotCtl {
            gate: tokio::sync::Semaphore::new(0),
            fail_first: AtomicU32::new(0),
            retains: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            sound,
            store_dead: std::sync::atomic::AtomicBool::new(false),
            extract_dead: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn open_gate(&self) {
        self.gate.add_permits(1024);
    }
}

/// Install the slot-wrap seam: every subsequent write's payload becomes
/// a mock `ZcWriteSlot` driven by `ctl`.
fn arm_slot_seam(ctl: Arc<SlotCtl>) {
    set_test_zc_slot_wrap(Arc::new(move |bytes: bytes::Bytes| {
        let len = bytes.len() as u32;
        let c_store = ctl.clone();
        let store_bytes = bytes.clone();
        let c_extract = ctl.clone();
        let c_retain = ctl.clone();
        let c_release = ctl.clone();
        ZcWriteSlot::new_with_ack_early(
            len,
            ctl.sound,
            Box::new(move |fd, dev_off| {
                let c = c_store.clone();
                let b = store_bytes.clone();
                Box::pin(async move {
                    let permit = c.gate.acquire().await.expect("gate closed for good");
                    permit.forget();
                    if c.store_dead.load(Ordering::SeqCst) {
                        // The dead ring: fuse3's not-zc-armed class.
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "zc store: session not zc-armed",
                        ));
                    }
                    if c.fail_first
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                        .is_ok()
                    {
                        return Err(std::io::Error::from_raw_os_error(libc::EIO));
                    }
                    let n = unsafe {
                        libc::pwrite(fd, b.as_ptr().cast(), b.len(), dev_off as libc::off_t)
                    };
                    if n < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(n as u32)
                })
            }),
            Box::new(move || {
                let b = bytes.clone();
                let c = c_extract.clone();
                Box::pin(async move {
                    if c.extract_dead.load(Ordering::SeqCst) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "zc extract: session not zc-armed",
                        ));
                    }
                    Ok(b)
                })
            }),
            Box::new(move || {
                c_retain.retains.fetch_add(1, Ordering::SeqCst);
                true
            }),
            Box::new(move || {
                c_release.releases.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }));
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: tempfile::NamedTempFile,
    _m: tempfile::NamedTempFile,
    _s: TempDir,
}

async fn format_meta(path: &std::path::Path, uuid: [u8; 16]) {
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xACE0_FACE,
        uuid,
    })
    .unwrap()
    .build(path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    set_device_overlay_for_tests(true, false); // SLOT vehicle, not bytes
    set_ack_early_for_tests(true, true); // enabled + O_DIRECT opt-in
                                         // Backing files under target/ — tmpfs refuses the O_DIRECT open the
                                         // zc_write_fd screen requires.
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let b = tempfile::Builder::new()
        .prefix("ackearly-b")
        .tempfile_in(dir)
        .unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    let m = tempfile::Builder::new()
        .prefix("ackearly-m")
        .tempfile_in(dir)
        .unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    format_meta(m.path(), *b"ack-early-r3!!!!").await;
    let s = tempfile::Builder::new()
        .prefix("ackearly-s")
        .tempdir_in(dir)
        .unwrap();

    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
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

async fn read_at(h: &H, ino: u64, off: u64, len: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Promote `ino` to striped authority (overlays install only AFTER the
/// promotion drained and published — §7.1) with the seam DISARMED, so
/// the promotion itself rides the ordinary bytes path.
async fn promote_striped(h: &H, ino: u64) {
    clear_test_zc_slot_wrap();
    let base: Vec<u8> = (0..2 * BS).map(|i| (i % 251) as u8).collect();
    let w =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&base), 0, 0)
            .await
            .unwrap();
    assert_eq!(w.written as usize, base.len());
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

/// Await a condition with a bounded poll (never a bare sleep-for-sync:
/// the poll is the assertion's clock, the deadline its failure).
async fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    for _ in 0..2000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never held: {what}");
}

/// ACK-early: an eligible overlay write RETURNS while its store is
/// parked behind the gate; coverage/publication stay CQE-anchored; the
/// release fires exactly once after the store lands; the bytes read
/// back exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ack_early_write_returns_before_store_cqe() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    let h = make("ackearly-rt").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let acked0 = METRICS.overlay_ack_early_stores.load(Ordering::Relaxed);
    let data = vec![0xA5u8; BS as usize];
    // The write must return with the gate CLOSED — the ACK detached
    // from the store CQE.
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::copy_from_slice(&data),
            0,
            0,
        )
        .await
        .expect("ack-early write");
    assert_eq!(w.written, BS as u32);
    assert_eq!(
        ctl.retains.load(Ordering::SeqCst),
        1,
        "the reply must have armed RETAIN"
    );
    assert_eq!(
        ctl.releases.load(Ordering::SeqCst),
        0,
        "no release before the store CQE"
    );
    assert_eq!(
        METRICS.overlay_ack_early_stores.load(Ordering::Relaxed),
        acked0 + 1,
        "engagement counter"
    );

    // CQE-anchored: the store has not completed, so the overlay must
    // still be open with an in-flight claim (fsync residue would trip —
    // pinned separately below).
    ctl.open_gate();
    let c = ctl.clone();
    eventually(
        move || c.releases.load(Ordering::SeqCst) == 1,
        "release after store CQE",
    )
    .await;

    // Publication converged: the bytes are readable and exact.
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data, "readback after ack-early publish");
    clear_test_zc_slot_wrap();
}

/// The fsync law: a durability boundary must await ACKed-but-in-flight
/// stores (design-device-overlay §6.2 step 2's COMPLETE arm restated
/// for ACK-early — the §3.4 charter).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_awaits_acked_inflight_store() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    let h = make("ackearly-fsync").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let data = vec![0x5Cu8; BS as usize];
    h.fs.write(
        h.req,
        ino,
        0,
        2 * BS,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("ack-early write");
    assert_eq!(ctl.releases.load(Ordering::SeqCst), 0);

    // fsync with the store parked: must NOT complete.
    let fs2 = h.fs.clone();
    let req = h.req;
    let fsync = tokio::spawn(async move { fs2.fsync(req, ino, 0, false).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !fsync.is_finished(),
        "fsync completed while an ACKed store was still in flight — \
         the §6.2 step-2 COMPLETE arm lost the retained store"
    );

    ctl.open_gate();
    fsync
        .await
        .expect("fsync task")
        .expect("fsync result after the store landed");
    assert_eq!(
        METRICS.overlay_unpublished_at_fsync.load(Ordering::Relaxed),
        0,
        "a successful fsync leaves no overlay unpublished"
    );
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data);
    clear_test_zc_slot_wrap();
}

/// The read-wait law: a read intersecting an ACKed-but-incomplete store
/// waits for it — it never serves the range as an uncovered gap
/// (zeros) and never returns before the store's bytes are the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_waits_for_acked_inflight_store() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    let h = make("ackearly-ryw").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let data = vec![0x3Du8; BS as usize];
    h.fs.write(
        h.req,
        ino,
        0,
        2 * BS,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("ack-early write");

    let fs2 = h.fs.clone();
    let req = h.req;
    let read = tokio::spawn(async move { fs2.read(req, ino, 0, 2 * BS, BS as u32, 0).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !read.is_finished(),
        "a read of an ACKed-but-in-flight range returned early — it \
         either served stale zeros (the law-5 gap face) or tore the store"
    );

    ctl.open_gate();
    let out = read.await.expect("read task").expect("read result");
    assert_eq!(
        out.data.to_vec(),
        data,
        "RYW: the read's answer is the ACKed write's bytes"
    );
    clear_test_zc_slot_wrap();
}

/// Never-lossy: an ACKed store that fails transiently retries until it
/// lands. The data is acked custody — only a fence may drop it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_acked_store_retries_until_landed() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    ctl.fail_first.store(2, Ordering::SeqCst);
    let h = make("ackearly-retry").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let r0 = METRICS.overlay_ack_early_retries.load(Ordering::Relaxed);
    let data = vec![0x77u8; BS as usize];
    h.fs.write(
        h.req,
        ino,
        0,
        2 * BS,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("ack-early write");
    ctl.open_gate();

    let c = ctl.clone();
    eventually(
        move || c.releases.load(Ordering::SeqCst) == 1,
        "release after the retried store landed",
    )
    .await;
    assert!(
        METRICS.overlay_ack_early_retries.load(Ordering::Relaxed) >= r0 + 2,
        "two scripted failures ⇒ at least two retries"
    );
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(
        back, data,
        "acked custody landed despite transient failures"
    );
    clear_test_zc_slot_wrap();
}

/// The §3.4 stability-law class split: with the O_DIRECT opt-in OFF, an
/// unsound-class (GUP) write keeps ACK-after-CQE — the write BLOCKS on
/// the gated store; a sound-class write still ACKs early.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn odirect_class_needs_the_unstable_write_opt_in() {
    let _g = serial().await;
    let ctl = SlotCtl::new(false); // unsound class (O_DIRECT/GUP)
    let h = make("ackearly-class").await;
    set_ack_early_for_tests(true, false); // opt-in OFF
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let data = vec![0x11u8; BS as usize];
    let fs2 = h.fs.clone();
    let req = h.req;
    let d2 = data.clone();
    let w = tokio::spawn(async move {
        fs2.write(
            req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::copy_from_slice(&d2),
            0,
            0,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !w.is_finished(),
        "an unsound-class write ACKed early without the opt-in — the \
         §3.4 stability law is not being enforced"
    );
    assert_eq!(
        ctl.retains.load(Ordering::SeqCst),
        0,
        "no RETAIN for the declined class"
    );
    ctl.open_gate();
    w.await.expect("write task").expect("awaited write");
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data);
    clear_test_zc_slot_wrap();
}

/// Live-smoke conviction (2026-08-09 tcp-devsub): O_DIRECT ACK-early
/// that DMAs from the GUP pages *after* write(2) returns observes a
/// reused buffer's later contents (dd/fio class — 6553/8192 pages
/// aliased, first striped block exact because it still rode
/// ACK-after-CQE promotion). §3.4 named this "unstable-write"; the
/// NFS UNSTABLE analogy is wrong (NFS samples at ACK). The opt-in
/// must SNAPSHOT at ACK (extract) and DMA the snapshot, so a
/// post-ACK reuse cannot change what lands.
///
/// The mock's default store clones at mint and cannot express this;
/// this test's store reads a shared buffer at DMA time (the GUP),
/// and extract clones at the materialize instant (the snapshot).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn odirect_ack_early_must_not_observe_post_ack_reuse() {
    let _g = serial().await;
    set_device_overlay_for_tests(true, false);
    set_ack_early_for_tests(true, true);
    let h = make("ackearly-gup-reuse").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;

    let original = vec![0xA5u8; BS as usize];
    let reused = vec![0x5Cu8; BS as usize];
    let live = std::sync::Arc::new(std::sync::Mutex::new(original.clone()));
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let stores = std::sync::Arc::new(AtomicUsize::new(0));
    let extracts = std::sync::Arc::new(AtomicUsize::new(0));

    {
        let live_s = live.clone();
        let live_e = live.clone();
        let gate_s = gate.clone();
        let stores_c = stores.clone();
        let extracts_c = extracts.clone();
        set_test_zc_slot_wrap(std::sync::Arc::new(move |bytes: bytes::Bytes| {
            let len = bytes.len() as u32;
            let live_s = live_s.clone();
            let live_e = live_e.clone();
            let gate_s = gate_s.clone();
            let stores_c = stores_c.clone();
            let extracts_c = extracts_c.clone();
            ZcWriteSlot::new_with_ack_early(
                len,
                false, // unsound = O_DIRECT/GUP
                Box::new(move |fd, dev_off| {
                    let live = live_s.clone();
                    let gate = gate_s.clone();
                    let stores_c = stores_c.clone();
                    Box::pin(async move {
                        let permit = gate.acquire().await.expect("gate");
                        permit.forget();
                        stores_c.fetch_add(1, Ordering::SeqCst);
                        let b = live.lock().expect("live").clone();
                        let n = unsafe {
                            libc::pwrite(fd, b.as_ptr().cast(), b.len(), dev_off as libc::off_t)
                        };
                        if n < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(n as u32)
                    })
                }),
                Box::new(move || {
                    let live = live_e.clone();
                    let extracts_c = extracts_c.clone();
                    Box::pin(async move {
                        extracts_c.fetch_add(1, Ordering::SeqCst);
                        let b = live.lock().expect("live").clone();
                        Ok(bytes::Bytes::from(b))
                    })
                }),
                Box::new(|| true),
                Box::new(|| {}),
            )
        }));
    }

    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::copy_from_slice(&original),
            0,
            0,
        )
        .await
        .expect("ack-early O_DIRECT write");
    assert_eq!(w.written, BS as u32);

    // Application reuses the GUP buffer after write(2) returned.
    *live.lock().expect("live") = reused;

    // Unblock a DMA-time store if the path still takes that arm.
    gate.add_permits(1024);
    let stores_w = stores.clone();
    let extracts_w = extracts.clone();
    eventually(
        move || {
            stores_w.load(Ordering::SeqCst) + extracts_w.load(Ordering::SeqCst) >= 1
                && METRICS.overlay_stores.load(Ordering::Relaxed) > 0
        },
        "acked O_DIRECT store published",
    )
    .await;

    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert!(
        back == original,
        "O_DIRECT ACK-early must persist the ACK-time bytes, not a \
         post-ACK reuse (got {:#x} want {:#x} — DMA sampled the GUP \
         pages after write(2) returned)",
        back.first().copied().unwrap_or(0),
        original.first().copied().unwrap_or(0)
    );
    assert_eq!(
        extracts.load(Ordering::SeqCst),
        1,
        "O_DIRECT ACK-early must snapshot via extract (not DMA the GUP)"
    );
    assert_eq!(
        stores.load(Ordering::SeqCst),
        0,
        "O_DIRECT ACK-early must not take the retained-slot store arm"
    );
    clear_test_zc_slot_wrap();
}

/// O_DIRECT overlay must NOT HOLD the zc slot (delivery extracts on the
/// worker's batched pass). Holding + handler `materialize` is the late
/// extract that pinned the field 1 MiB row at 23 GiB/s (zcws-8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn odirect_overlay_is_not_hold_eligible() {
    let _g = serial().await;
    let h = make("ackearly-nohold").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    // Next block is fresh/unmapped — overlay-eligible.
    assert!(
        h.fs.zc_write_hold_eligible(ino, 2 * BS, BS as u32, false),
        "page-cache overlay still HOLDs for 0-copy retain"
    );
    assert!(
        !h.fs.zc_write_hold_eligible(ino, 2 * BS, BS as u32, true),
        "O_DIRECT overlay must extract at delivery, not HOLD"
    );
    // Mapped block 0 is W1-eligible — O_DIRECT must still HOLD so the
    // sole-owner patch keeps the 0-copy slot→device vehicle.
    assert!(
        h.fs.zc_write_hold_eligible(ino, 0, 4096, true),
        "W1 O_DIRECT must still HOLD (only overlay O_DIRECT extracts at delivery)"
    );
}

/// Bytes payloads (at-delivery extract, IL severs) ACK-early when the
/// overlay is armed — they are daemon-owned snapshots, so the GUP
/// opt-in does not apply. The write returns while device DMA is stalled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_overlay_ack_early_returns_before_device_cqe() {
    let _g = serial().await;
    let h = make("ackearly-bytes").await;
    set_device_overlay_for_tests(true, true); // Bytes vehicle
    set_ack_early_for_tests(true, true);
    clear_test_zc_slot_wrap();
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;

    struct StallReset;
    impl Drop for StallReset {
        fn drop(&mut self) {
            squeezefs::nvme_dev::set_test_write_stall(0, 0);
        }
    }
    squeezefs::nvme_dev::set_test_write_stall(4, 2_000);
    let _stall = StallReset;
    let data = vec![0xE1u8; BS as usize];
    let t0 = std::time::Instant::now();
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            2 * BS,
            bytes::Bytes::copy_from_slice(&data),
            0,
            0,
        )
        .await
        .expect("bytes ack-early write");
    let dt = t0.elapsed();
    assert_eq!(w.written, BS as u32);
    assert!(
        dt < Duration::from_millis(500),
        "Bytes overlay ACK-early waited {dt:?} — still on the device CQE \
         (the late-extract/ACK-after-CQE tax)"
    );
    eventually(
        || METRICS.overlay_stores.load(Ordering::Relaxed) > 0,
        "bytes ack-early store published",
    )
    .await;
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data);
    squeezefs::nvme_dev::set_test_write_stall(0, 0);
}

/// 4 KiB-aligned extract/sever Bytes skip the BUFFER_POOL memcpy
/// and DMA as `WriteData::Aligned` (the A-leg tax).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aligned_overlay_bytes_skip_the_pool_copy() {
    let _g = serial().await;
    let h = make("ackearly-aligned-dma").await;
    set_device_overlay_for_tests(true, true);
    set_ack_early_for_tests(true, true);
    clear_test_zc_slot_wrap();
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;

    let mut buf = squeezefs::cache::pool::BUFFER_POOL.alloc();
    buf.resize(BS as usize, 0xE2);
    let data = buf.into_bytes();
    assert_eq!(
        data.as_ptr() as usize % squeezefs::cache::pool::POOLED_BUF_ALIGN,
        0
    );
    let pass0 = METRICS
        .overlay_dma_passthrough_bytes
        .load(Ordering::Relaxed);
    let copy0 = METRICS.overlay_dma_pool_copy_bytes.load(Ordering::Relaxed);
    let w =
        h.fs.write(h.req, ino, 0, 2 * BS, data.clone(), 0, 0)
            .await
            .expect("aligned bytes overlay write");
    assert_eq!(w.written, BS as u32);
    eventually(
        || METRICS.overlay_stores.load(Ordering::Relaxed) > 0,
        "aligned overlay store published",
    )
    .await;
    assert_eq!(
        METRICS
            .overlay_dma_passthrough_bytes
            .load(Ordering::Relaxed)
            - pass0,
        BS,
        "aligned overlay Bytes must DMA without a pool copy"
    );
    assert_eq!(
        METRICS.overlay_dma_pool_copy_bytes.load(Ordering::Relaxed),
        copy0,
        "aligned overlay A-leg must not pay the pool bounce"
    );
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data.as_ref());
}

/// The generic/464 unmount wedge (release-gate battery, 2026-08-10; the
/// runner's fail-fast at test 458/787): an external umount kills the
/// uring connection, fuse3's `shutdown()` disarms zc, and every pending
/// ACK-early retained-slot DMA starts failing `Unsupported` ("session
/// not zc-armed") — a VEHICLE-PERMANENT class the continuation retried
/// forever ("acked custody retries until it lands" can never land on a
/// dead ring). The spinning in-flight claims wedged the dismount
/// teardown's overlay drain, the daemon lingered past its 70 s bound,
/// and the next mount refused ("previous daemon still running after
/// 60s"). The recoverable half, pinned here: while the ACK-time bytes
/// are still reachable (`materialize()` — memoized extraction), the
/// continuation must fall back to the SESSION-INDEPENDENT device write
/// and land the acked custody: bounded, never-lossy, no spin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_ring_store_falls_back_to_device_write() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    ctl.store_dead.store(true, Ordering::SeqCst); // ring dead from op 1
    let h = make("ackearly-deadring").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let data = vec![0x4Au8; BS as usize];
    h.fs.write(
        h.req,
        ino,
        0,
        2 * BS,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("ack-early write acks");
    ctl.open_gate();

    // The continuation must terminate via the fallback (release fires
    // exactly once) — the pre-fix loop spins on the dead vehicle and
    // this deadline is the red assertion.
    let c = ctl.clone();
    eventually(
        move || c.releases.load(Ordering::SeqCst) == 1,
        "dead-ring continuation must terminate through the device-write \
         fallback (spinning 'until it lands' on a disarmed session is \
         the generic/464 teardown wedge)",
    )
    .await;
    // Never-lossy: the acked bytes landed via the fallback.
    clear_test_zc_slot_wrap();
    let back = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(back, data, "acked custody must land via the fallback");
}

/// The honest-loss half: ring dead AND the ACK-time bytes unreachable
/// (extraction rides the same dead session — the field shape, where the
/// retained folios die with the connection). The continuation must
/// terminate LOUD (coverage never published — law 3; the loss counted
/// on `overlay_ack_early_lost`) instead of wedging the teardown: this
/// is exactly the "dismounted with unflushed data" posture, and the
/// teardown drain must converge afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_ring_and_dead_extract_terminate_loud_never_wedge() {
    let _g = serial().await;
    let ctl = SlotCtl::new(true);
    ctl.store_dead.store(true, Ordering::SeqCst);
    ctl.extract_dead.store(true, Ordering::SeqCst);
    let h = make("ackearly-deadboth").await;
    let ino = create(&h, "f").await;
    promote_striped(&h, ino).await;
    arm_slot_seam(ctl.clone());

    let lost0 = METRICS.overlay_ack_early_lost.load(Ordering::Relaxed);
    let data = vec![0x5Bu8; BS as usize];
    h.fs.write(
        h.req,
        ino,
        0,
        2 * BS,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("ack-early write acks");
    ctl.open_gate();

    let c = ctl.clone();
    eventually(
        move || c.releases.load(Ordering::SeqCst) == 1,
        "dead-vehicle continuation must terminate (loud loss), never spin",
    )
    .await;
    assert_eq!(
        METRICS.overlay_ack_early_lost.load(Ordering::Relaxed),
        lost0 + 1,
        "the unrecoverable acked store is COUNTED (the unflushed-at-\
         unmount class, must stay 0 outside teardown races)"
    );
    // Teardown liveness — the wedge's other half: with the claim
    // released, the drain boundary (fsync runs the same freeze → await
    // in-flight → publish ladder the dismount teardown runs) converges
    // instead of waiting forever on the spinning in-flight set.
    clear_test_zc_slot_wrap();
    let drained =
        tokio::time::timeout(Duration::from_secs(30), h.fs.fsync(h.req, ino, 0, false)).await;
    assert!(
        drained.is_ok(),
        "the drain boundary must converge after the loud terminal \
         (generic/464: it waited forever on the spinning claim)"
    );
}
