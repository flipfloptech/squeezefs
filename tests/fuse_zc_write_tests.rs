//! The D14 **write-side zc leg** (2026-08-06, rc-manifest §3f ruling
//! D14): on a `FUSE_URING_ZERO_COPY` session the kernel registers the
//! WRITE payload's pages `ITER_SOURCE` in the transport ring's sparse
//! slot — an eligible shape can therefore DMA the payload STRAIGHT from
//! the caller's pages to the device (`WRITE_FIXED(device fd ← slot)`),
//! deleting the slot→memfd extraction round trip AND every daemon copy.
//! The memfd bounce extraction remains the ineligible-shape vehicle,
//! requested LAZILY (dispatch-before-extraction) so eligible shapes
//! never pay it.
//!
//! This suite pins the ROUTER half — the eligibility composition with
//! the W1 sole-owner patch, the materialize fallback, the engagement
//! ledger — with an INJECTED slot source (`ZcWriteSlot::new` takes the
//! store/extract fns; the live ones are the fuse3 queue ring's
//! `WRITE_FIXED` ops, reachable only on the sqz kernel — the field
//! A-B-B-A bracket is their acceptance venue).
//!
//! Contracts (red-first):
//! 1. **Direct engagement**: a patch-class write (aligned, sub-block,
//!    non-extending, non-adjacent, sole-owner, passthrough, whole-block
//!    mapping) carried by a slot source rides ONE `store(fd, dev_off)`
//!    — extraction never runs, `patch_writes` counts it, the ledger
//!    pair `fuse3_zc_write_directs/_bytes` accounts op + bytes, and the
//!    bytes are device-honest (cold read-back exact).
//! 2. **Ineligible shapes materialize**: an unaligned window carried by
//!    a slot source extracts ONCE (the lazy vehicle), never stores, and
//!    stays byte-exact.
//! 3. **A failed store falls back**: the pooled patch path completes
//!    under the same §5.1 sole-owner window — never a lost or wrong
//!    write, never EIO for a healthy fallback.
//! 4. **Materialization is memoized**: the fencing-retry loop re-enters
//!    `write_file_staged` with the same payload — one extraction ever.
//! 5. **Multi-block slot writes materialize** (the patch predicates are
//!    single-block by construction): one extraction, zero stores,
//!    byte-exact across the span.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{set_patch_max_bytes, SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::{DataRouter, WritePayload, ZcWriteSlot};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Sandbox block size (the extent_patch_tests convention): the patch
/// anatomy is block-size-relative; offsets/lengths speak the 4096-byte
/// LBA quantum of §5.1 predicate 5.
const BS: u64 = 64 * 1024;

/// Process-global METRICS deltas: serialize tests (the cargo gate runs
/// `--test-threads=1`; this keeps the suite order-robust on its own).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_patch_max_bytes(512 * 1024);
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(m.path())
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_D14D_0001,
        uuid,
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let s = tempdir().unwrap();

    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
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
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

/// Drop every RAM/NVMe read tier for `ino` so later reads are
/// device-honest.
async fn purge_read_tiers(h: &H, ino: u64) {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
        if let Some(bm) = m.block_map.as_ref() {
            for bk in bm.values() {
                h.fs.router.cache.purge_block_key(bk);
            }
        }
    }
}

/// A durable, cold, freshly-striped fixture (whole-block undecorated
/// mappings — the W1-eligible population).
async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
    let ino = create(h, name).await;
    let base = pattern((blocks * BS) as usize, tag);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    purge_read_tiers(h, ino).await;
    (ino, base)
}

/// The injected slot source: holds the "registered pages" (the payload
/// bytes), counts stores/extractions, and executes a REAL aligned pwrite
/// on the fd the patch path resolved (standing in for the fuse3 ring's
/// `WRITE_FIXED(device fd ← slot)`).
struct FakeSlot {
    payload: Vec<u8>,
    stores: Arc<AtomicU32>,
    extracts: Arc<AtomicU32>,
    store_offsets: Arc<Mutex<Vec<u64>>>,
    fail_store: bool,
}

impl FakeSlot {
    fn new(payload: Vec<u8>, fail_store: bool) -> Self {
        Self {
            payload,
            stores: Arc::new(AtomicU32::new(0)),
            extracts: Arc::new(AtomicU32::new(0)),
            store_offsets: Arc::new(Mutex::new(Vec::new())),
            fail_store,
        }
    }

    fn slot(&self) -> Arc<ZcWriteSlot> {
        let len = self.payload.len() as u32;
        let payload_s = self.payload.clone();
        let payload_x = self.payload.clone();
        let stores = Arc::clone(&self.stores);
        let extracts = Arc::clone(&self.extracts);
        let offsets = Arc::clone(&self.store_offsets);
        let fail_store = self.fail_store;
        ZcWriteSlot::new(
            len,
            Box::new(move |fd, dev_off| {
                let payload = payload_s.clone();
                let stores = Arc::clone(&stores);
                let offsets = Arc::clone(&offsets);
                Box::pin(async move {
                    stores.fetch_add(1, Ordering::Relaxed);
                    offsets.lock().unwrap().push(dev_off);
                    if fail_store {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "injected zc store failure",
                        ));
                    }
                    // O_DIRECT-aligned scratch (the live slot pages are
                    // page-aligned user pages).
                    let layout =
                        std::alloc::Layout::from_size_align(payload.len().max(1), 4096)
                            .expect("layout");
                    // SAFETY: nonzero aligned allocation, freed below.
                    let buf = unsafe { std::alloc::alloc(layout) };
                    assert!(!buf.is_null());
                    // SAFETY: buf is payload.len() writable bytes.
                    unsafe {
                        std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len());
                    }
                    // SAFETY: fd is the patch-resolved device write fd;
                    // buf holds payload.len() initialized bytes.
                    let n = unsafe {
                        libc::pwrite(
                            fd,
                            buf.cast(),
                            payload.len(),
                            dev_off as libc::off_t,
                        )
                    };
                    // SAFETY: allocated with this layout above.
                    unsafe { std::alloc::dealloc(buf, layout) };
                    if n == payload.len() as isize {
                        Ok(payload.len() as u32)
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
            }),
            Box::new(move || {
                let payload = payload_x.clone();
                let extracts = Arc::clone(&extracts);
                Box::pin(async move {
                    extracts.fetch_add(1, Ordering::Relaxed);
                    Ok(bytes::Bytes::from(payload))
                })
            }),
        )
    }
}

/// Contract 1 — direct engagement + ledger + device-honest bytes; then
/// contract 2 (unaligned materializes) and contract 5 (multi-block
/// materializes) in the same fn (process-global counters, the ledger
/// suites' counter-isolation discipline).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_store_engagement_and_materialize_fallbacks() {
    let _g = serial().await;
    let h = make(*b"zcw-direct-00001", "zcw_ns_a").await;
    let (ino, mut want) = durable_striped(&h, "zcw_a.bin", 4, 0x31).await;
    let size = want.len() as u64;
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);

    // ---- Contract 1: aligned patch-class write via the slot source. ----
    let p = pattern(8192, 0xA1);
    let fake = FakeSlot::new(p.clone(), false);
    let d0 = fuse3::zc_write_directs();
    let db0 = fuse3::zc_write_direct_bytes();
    let pw0 = METRICS.patch_writes.load(Ordering::Relaxed);
    h.fs.write_file_staged(
        ino,
        BS + 16384,
        WritePayload::Slot(fake.slot()),
        size,
        token,
    )
    .await
    .expect("direct patch-class write");
    want[(BS + 16384) as usize..(BS + 16384) as usize + 8192].copy_from_slice(&p);

    assert_eq!(
        fake.stores.load(Ordering::Relaxed),
        1,
        "exactly one slot→device store for the eligible shape"
    );
    assert_eq!(
        fake.extracts.load(Ordering::Relaxed),
        0,
        "the eligible shape must never pay the extraction round trip — \
         that IS the D14 leg"
    );
    assert_eq!(
        METRICS.patch_writes.load(Ordering::Relaxed) - pw0,
        1,
        "the direct vehicle is still a W1 patch (decision ledger intact)"
    );
    assert_eq!(
        fuse3::zc_write_directs() - d0,
        1,
        "fuse3_zc_write_directs must count the direct DMA"
    );
    assert_eq!(
        fuse3::zc_write_direct_bytes() - db0,
        8192,
        "fuse3_zc_write_direct_bytes must account the payload bytes"
    );
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, BS, BS as usize).await;
    assert_eq!(
        got,
        want[BS as usize..2 * BS as usize].to_vec(),
        "cold read-back must be byte-exact (device-honest direct DMA)"
    );

    // ---- Contract 2: unaligned window materializes (extract once). ----
    let p = pattern(4096, 0xA2);
    let fake = FakeSlot::new(p.clone(), false);
    let d0 = fuse3::zc_write_directs();
    h.fs.write_file_staged(
        ino,
        2 * BS + 1234,
        WritePayload::Slot(fake.slot()),
        size,
        token,
    )
    .await
    .expect("unaligned slot write");
    want[(2 * BS + 1234) as usize..(2 * BS + 1234) as usize + 4096].copy_from_slice(&p);
    assert_eq!(
        fake.stores.load(Ordering::Relaxed),
        0,
        "unaligned windows never store direct"
    );
    assert_eq!(
        fake.extracts.load(Ordering::Relaxed),
        1,
        "the ineligible shape rides ONE lazy extraction"
    );
    assert_eq!(fuse3::zc_write_directs(), d0, "direct ledger flat");
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_eq!(
        got,
        want[2 * BS as usize..3 * BS as usize].to_vec(),
        "materialized write must stay byte-exact"
    );

    // ---- Contract 5: multi-block slot write materializes. ----
    let p = pattern((BS + 8192) as usize, 0xA3);
    let fake = FakeSlot::new(p.clone(), false);
    h.fs.write_file_staged(ino, BS - 4096, WritePayload::Slot(fake.slot()), size, token)
        .await
        .expect("multi-block slot write");
    want[(BS - 4096) as usize..(BS - 4096) as usize + p.len()].copy_from_slice(&p);
    assert_eq!(
        fake.stores.load(Ordering::Relaxed),
        0,
        "multi-block spans never store direct (patch is single-block)"
    );
    assert_eq!(
        fake.extracts.load(Ordering::Relaxed),
        1,
        "one extraction serves the whole span"
    );
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_eq!(got, want, "full-file audit after the mixed vehicles");
}

/// Contract 3 — a failed direct store falls back to the pooled patch
/// path under the same sole-owner window: write succeeds, bytes exact,
/// direct ledger flat (failed attempts never count).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_store_falls_back_to_the_pooled_patch() {
    let _g = serial().await;
    let h = make(*b"zcw-fallbk-00002", "zcw_ns_b").await;
    let (ino, mut want) = durable_striped(&h, "zcw_b.bin", 4, 0x32).await;
    let size = want.len() as u64;
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);

    let p = pattern(8192, 0xB1);
    let fake = FakeSlot::new(p.clone(), true);
    let d0 = fuse3::zc_write_directs();
    let pw0 = METRICS.patch_writes.load(Ordering::Relaxed);
    h.fs.write_file_staged(
        ino,
        3 * BS + 16384,
        WritePayload::Slot(fake.slot()),
        size,
        token,
    )
    .await
    .expect("a failed store must fall back, never EIO");
    want[(3 * BS + 16384) as usize..(3 * BS + 16384) as usize + 8192].copy_from_slice(&p);

    assert_eq!(
        fake.stores.load(Ordering::Relaxed),
        1,
        "the direct leg was genuinely attempted"
    );
    assert_eq!(
        fake.extracts.load(Ordering::Relaxed),
        1,
        "the fallback materializes through the extraction vehicle"
    );
    assert_eq!(
        METRICS.patch_writes.load(Ordering::Relaxed) - pw0,
        1,
        "the pooled patch completed under the same sole-owner window"
    );
    assert_eq!(
        fuse3::zc_write_directs(),
        d0,
        "failed direct attempts never count on the direct ledger"
    );
    purge_read_tiers(&h, ino).await;
    let got = read_at(&h, ino, 3 * BS, BS as usize).await;
    assert_eq!(
        got,
        want[3 * BS as usize..4 * BS as usize].to_vec(),
        "fallback bytes must be exact"
    );
}

/// Contract 4 — materialization is memoized on the handle: the fencing
/// retry loop re-enters with a CLONE of the same payload and must not
/// pay a second extraction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn materialization_is_memoized_across_retries() {
    let _g = serial().await;
    let fake = FakeSlot::new(pattern(4096, 0xC1), false);
    let slot = fake.slot();
    let a = slot.materialize().await.expect("first materialize");
    let b = slot.materialize().await.expect("second materialize");
    assert_eq!(a, b, "memoized bytes are the same bytes");
    assert_eq!(
        fake.extracts.load(Ordering::Relaxed),
        1,
        "one extraction ever — the retry loop must not re-extract"
    );
    let payload = WritePayload::Slot(slot);
    assert_eq!(payload.len(), 4096, "payload length rides the slot");
    let c = payload.clone();
    assert_eq!(c.len(), 4096, "clones share the memoized handle");
}
