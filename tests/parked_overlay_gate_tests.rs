//! O(1) parked-overlay gate — the read hot path's occupancy proof over
//! `active_block_buffers` (perf/odirect-randread-concurrency, 2026-07-25;
//! evidence `.benchmarks/2026-07-25-odirect-randread-concurrency.md`).
//!
//! The convicted mechanism: `capture_parked_runs` ran TWICE per FUSE READ
//! and gated on `DashMap::is_empty()`, which read-locks EVERY shard
//! (dashmap 5.5 `_len` sums `s.read().len()` over all shards — 128 on a
//! 32-CPU box). At 4k-randread saturation that is ~50 M shard-rwlock
//! acquisitions/second of pure coherence traffic: 45 % of daemon CPU in
//! `capture_parked_runs` + 12 % in `RawRwLock::lock_shared_slow`, and the
//! "more client threads = slower" collapse from the 2026-07-25 cluster
//! report.
//!
//! The replacement gate is a lock-free relaxed-cost atomic count of live
//! parked overlays whose safety contract is pinned here:
//!
//! - **`0` proves empty**: the count increments BEFORE a park publishes
//!   into the map and decrements AFTER a retire removes — so a reader
//!   observing `0` holds a proof no overlay exists, and skipping every
//!   per-block probe is exactly as correct as probing an empty map.
//!   (A transient over-count is conservative: the reader pays the
//!   ordinary per-block probe, never loses bytes.)
//! - **Every park path counts**: the extent park, the escalated
//!   record-absorb park, and the full-repr checkout park (deferred /
//!   fresh / staged-seeded) all raise the gate.
//! - **Every retire path counts down**: fold, flush, write-through, and
//!   drain all route through `retire_parked_overlay` (the mandated single
//!   removal path) and return the gate to exactly zero when the map
//!   empties — the gate can never wedge above zero on an idle mount
//!   (which would only cost probes), and never undercount (which would
//!   serve stale/zero bytes for acked writes).
//! - **Overlay never invisible, gate edition**: while the gate is
//!   raised, reads over parked blocks serve the overlay-merged bytes;
//!   after the gate returns to zero, reads serve the durable bytes.
//!
//! RED against the scaffolding (accessor present, counter never bumped):
//! the park assertions fail with gate == 0 while an overlay is live.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

/// Knobs are process-global; tests serialize (house pattern).
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

fn reset_knobs(patch_max: u64) {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(patch_max);
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    squeezefs::fuse_client::set_parked_cap_buffers(256);
}

/// `compressed` = lz4 volume (every write patch-ineligible — the extent-park
/// population); `patch_max` = the W1 lever (0 disables the sole-owner patch
/// so small striped overwrites park instead).
async fn make(uuid: [u8; 16], alloc_ns: &str, compressed: bool, patch_max: u64) -> H {
    reset_knobs(patch_max);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        Vec::new(), // cache-less: beyond-inline writes route STRIPED
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
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    if compressed {
        router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    }
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
        .unwrap()
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at {i}: got {:#04x} want {:#04x}",
            got[i], want[i]
        );
    }
}

/// A durable striped file of `blocks` blocks, fsynced.
async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
    let len = (blocks * BS) as usize;
    let ino = create(h, name).await;
    let base = pattern(len, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    (ino, base)
}

/// 1. Baseline: a clean mount holds gate == 0, covering writes that
///    write-through + fsync return it to 0, and reads stay correct — the
///    state in which the fixed read path skips every overlay probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn covering_writes_leave_gate_zero_and_reads_correct() {
    let _g = serial().await;
    let h = make([0xA1; 16], "pog_alloc_1", false, 512 * 1024).await;
    assert_eq!(
        h.fs.parked_overlay_gate_count(),
        0,
        "clean mount must hold gate == 0"
    );

    let (ino, base) = durable_striped(&h, "clean.bin", 3, 0x11).await;
    assert_eq!(
        h.fs.parked_overlay_gate_count(),
        0,
        "covering write-through + fsync must return the gate to 0 \
         (a wedged gate makes every read pay overlay probes forever)"
    );
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "durable read with gate == 0");
}

/// 2. The extent park (patch-ineligible small write on a compressed
///    volume) raises the gate while the overlay is live; reads over the
///    parked window serve the overlay-merged bytes; fsync's fold retires
///    the overlay and returns the gate to exactly 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extent_park_raises_gate_and_fold_returns_it_to_zero() {
    let _g = serial().await;
    let h = make([0xA2; 16], "pog_alloc_2", true, 512 * 1024).await;
    let (ino, mut base) = durable_striped(&h, "ext.bin", 2, 0x22).await;
    assert_eq!(h.fs.parked_overlay_gate_count(), 0, "fixture must be clean");

    // Small non-adjacent overwrite inside block 1: patch-ineligible
    // (compressed) => parks an extent overlay.
    let patch = pattern(4096, 0x77);
    let off = BS + 8192;
    write_at(&h, ino, off, &patch).await;
    base[off as usize..off as usize + patch.len()].copy_from_slice(&patch);

    assert!(
        h.fs.parked_overlay_gate_count() > 0,
        "a live parked extent overlay must raise the O(1) gate — gate 0 \
         while an overlay is parked means readers skip acked bytes"
    );

    // Overlay never invisible while the gate is raised.
    let got = read_at(&h, ino, off - 4096, patch.len() + 8192).await;
    assert_bytes(
        &got,
        &base[(off - 4096) as usize..(off - 4096) as usize + patch.len() + 8192],
        "read spanning the parked overlay window",
    );

    fsync(&h, ino).await;
    assert_eq!(
        h.fs.parked_overlay_gate_count(),
        0,
        "fold/flush must return the gate to exactly 0 (no leak, no wedge)"
    );
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_bytes(&got, &base, "durable read after fold with gate == 0");
}

/// 3. The full-repr checkout park (deferred-seed overwrite, patch lever
///    off) raises the gate; flush retires it back to exactly 0; repeated
///    park/retire cycles never drift the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_seed_park_raises_gate_and_flush_returns_it_to_zero() {
    let _g = serial().await;
    // patch_max = 0: the W1 acceptance lever — aligned small overwrites
    // become patch-ineligible and go through the checkout park.
    let h = make([0xA3; 16], "pog_alloc_3", false, 0).await;
    let (ino, mut base) = durable_striped(&h, "seed.bin", 2, 0x33).await;
    assert_eq!(h.fs.parked_overlay_gate_count(), 0, "fixture must be clean");

    for cycle in 0u8..3 {
        // 50 % of block 1: past the extent-escalation edge => the full
        // checkout path parks a deferred-seed ActiveBlockBuf.
        let patch = pattern((BS / 2) as usize, 0x40 ^ cycle);
        let off = BS + BS / 4;
        write_at(&h, ino, off, &patch).await;
        base[off as usize..off as usize + patch.len()].copy_from_slice(&patch);

        assert!(
            h.fs.parked_overlay_gate_count() > 0,
            "cycle {cycle}: a live deferred-seed park must raise the gate"
        );

        fsync(&h, ino).await;
        assert_eq!(
            h.fs.parked_overlay_gate_count(),
            0,
            "cycle {cycle}: flush must return the gate to exactly 0 — \
             any drift breaks the '0 proves empty' contract"
        );
        let got = read_at(&h, ino, 0, base.len()).await;
        assert_bytes(&got, &base, "durable read after flush");
    }
}
