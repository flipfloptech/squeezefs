//! **The reader's layout-cache epoch-step gate** (ladder re-derivation
//! item 2, user decision 2026-09-06 —
//! `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`; the ladder
//! contracts live in `tests/reader_free_grace_tests.rs`).
//!
//! The acknowledgement ladder's drain window carried the reader's
//! staleness bound `S` because the daemon's layout cache could serve a
//! PRE-STEP block binding for up to its freshness horizon after the R-6
//! purge dropped the block-key census — a reader could resolve `b → K`
//! from a map the writer had already superseded, then read `K` from the
//! device after the writer reused it. The gate makes that structurally
//! impossible: every layout-cache entry is stamped with the reader's purge
//! generation READ BEFORE its backend read, the epoch step's last act bumps
//! the generation, and every binding-serving read refuses an entry stamped
//! below it (`ro_coherence::layout_entry_pre_step` — one relaxed load and a
//! compare; structurally false on a mount that never steps).
//!
//! Contracts:
//!
//! 1. **The handler resolve**: a stale entry inside its 1 s horizon serves
//!    (the pre-change shape) until an epoch step; after it,
//!    `fetch_metadata` re-resolves from the backend and the gate's
//!    engagement counts the miss. `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0`
//!    restores the TTL-only serve verbatim.
//! 2. **The sync legs**: the il/R-2 sync probe demotes a pre-step entry
//!    (never a tier serve through a stale map) and the direct-drive
//!    prelude refuses it as `Meta`; a current entry serves as before.
//! 3. **The stamp is captured before the read**: a refill that ran across
//!    a step is stamped with the pre-step generation and is a miss again
//!    (pinned through the public seam — a fetch on the pre-step generation
//!    followed by a step).
//! 4. **A plain writer is untouched**: with no step ever taken the gate
//!    reads false for every stamp and no gauge moves.
//!
//! RED against the parent: `CachedMetadata::reader_step_gen`,
//! `ro_coherence::{layout_entry_pre_step, reader_step_generation,
//! test_note_epoch_step, reader_layout_step_misses}` do not exist.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{IpcDirectIneligible, IpcReadProbe, SqueezefsFilesystem};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata as _, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::ro_coherence;
use squeezefs::routing::{metadata_entry_fresh_or_dirty, CachedMetadata, DataRouter};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 524_288;

/// Process-global state (the generation, the gate's gauges, the lever
/// latch) — one contract at a time (the `reader_free_grace_tests` shape).
static SERIAL: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    ro_coherence::reset_for_test();
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        ro_coherence::reset_for_test();
        SERIAL.store(false, Ordering::Release);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(tag: &[u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("layout_step_gate").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0x5EED_0000_0000_0001,
        uuid: *tag,
    })
    .unwrap()
    .build(m.path(), 64 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
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

/// A striped one-block layout binding block 0 → `key`.
fn striped(key: &str, stamp: u64) -> CachedMetadata {
    let mut map = std::collections::HashMap::new();
    map.insert(0u32, key.to_string());
    CachedMetadata {
        file_type: "striped".into(),
        size: BS,
        block_map: Some(Arc::new(map)),
        reader_step_gen: stamp,
        ..Default::default()
    }
}

/// Persist a striped layout binding block 0 → `key` in the backend.
async fn persist_binding(h: &H, ino: u64, key: &str) {
    let mut map = std::collections::HashMap::new();
    map.insert(0u32, key.to_string());
    let layout = squeezefs::routing::LayoutMetadata {
        file_type: "striped".to_string(),
        size: BS,
        block_map_id: Some(format!("block_map_{ino}")),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    };
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .setxattr(ino, "layout", &bincode::serialize(&layout).unwrap())
        .await
        .unwrap();
}

fn binding0(m: &CachedMetadata) -> String {
    m.block_map
        .as_ref()
        .and_then(|bm| bm.get(&0).cloned())
        .unwrap_or_default()
}

struct VecSink(std::sync::Mutex<Vec<u8>>);
impl squeezefs::PayloadSink for VecSink {
    fn write_at(&self, off: usize, bytes: &[u8]) {
        let mut v = self.0.lock().unwrap();
        let end = (off + bytes.len()).min(v.len());
        if off < end {
            v[off..end].copy_from_slice(&bytes[..end - off]);
        }
    }
    fn zero_at(&self, off: usize, len: usize) {
        let mut v = self.0.lock().unwrap();
        let end = (off + len).min(v.len());
        if off < end {
            v[off..end].fill(0);
        }
    }
}

/// Contract 1 — the handler's `fetch_metadata`: a fresh-by-TTL entry
/// serves until the step; after the step it is a miss and the backend's
/// binding is what the reader resolves. Lever off = TTL only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_step_layout_entry_is_a_miss_on_the_handler_resolve() {
    let _serial = serial();
    let h = make(b"layout-step-gt-1").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("f"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let path = squeezefs::keys::inode_path(ino);
    persist_binding(&h, ino, "1048576").await;

    // The pre-change shape: a cached entry inside its 1 s horizon is the
    // binding authority, whatever the backend now says.
    h.fs.router.publish_layout_cache_entry(
        ino,
        striped("524288", ro_coherence::reader_step_generation()),
    );
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        binding0(&m),
        "524288",
        "inside the horizon the cache serves"
    );
    assert_eq!(ro_coherence::reader_layout_step_misses(), 0);

    // The epoch step: the entry predates it and is a miss — the resolve
    // goes to the backend, whose binding is the new one.
    ro_coherence::test_note_epoch_step();
    assert!(
        !metadata_entry_fresh_or_dirty(&h.fs.router.metadata_cache.get(&ino).unwrap()),
        "the gate: a pre-step entry inside its TTL is NOT fresh"
    );
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        binding0(&m),
        "1048576",
        "after the step the reader resolves the backend's binding"
    );
    assert!(
        ro_coherence::reader_layout_step_misses() >= 1,
        "the gate's engagement counted the miss"
    );
    assert_eq!(
        m.reader_step_gen,
        ro_coherence::reader_step_generation(),
        "the refilled entry is stamped with the generation it was resolved under"
    );
    // And it now serves until the next step.
    let again = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(binding0(&again), "1048576");

    // The lever off: the pre-change TTL-only serve verbatim — a step does
    // not invalidate anything.
    ro_coherence::test_set_drain_epoch_stamp(Some(false));
    let misses_before = ro_coherence::reader_layout_step_misses();
    h.fs.router.publish_layout_cache_entry(
        ino,
        striped("524288", ro_coherence::reader_step_generation()),
    );
    ro_coherence::test_note_epoch_step();
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        binding0(&m),
        "524288",
        "lever off: the cache serves to its TTL across the step"
    );
    assert_eq!(ro_coherence::reader_layout_step_misses(), misses_before);
    assert!(ro_coherence::test_clear_drain_epoch_stamp());
}

/// Contract 2 — the sync legs demote a pre-step entry: the il/R-2 probe
/// answers `Miss` (and hands no stale map back) where a current entry
/// serves the hot tier, and the direct-drive prelude refuses `Meta`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_step_layout_entry_demotes_the_sync_probe_and_the_direct_drive_prelude() {
    let _serial = serial();
    let h = make(b"layout-step-gt-2").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("g"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let key = "524288";
    h.fs.router
        .cache
        .hot_block
        .put(key, bytes::Bytes::from(vec![0xA5u8; BS as usize]));
    h.fs.router
        .publish_layout_cache_entry(ino, striped(key, ro_coherence::reader_step_generation()));

    // Current entry: the sync probe serves the hot tier through the map.
    let sink = VecSink(std::sync::Mutex::new(vec![0u8; 4096]));
    let (probe, meta) = h.fs.ipc_read_probe_locked(ino, 0, 4096, &sink);
    assert!(
        matches!(probe, IpcReadProbe::Served(4096, _)),
        "a current entry serves the hot tier: {probe:?}"
    );
    assert!(meta.is_some());
    assert!(sink.0.lock().unwrap().iter().all(|&b| b == 0xA5));
    assert!(
        h.fs.ipc_direct_read_probe(ino, 0, 4096, 0).is_ok(),
        "a current striped entry admits the direct-drive prelude"
    );

    // After the step the same entry is pre-step: no tier serve through
    // it, no map handed back, and the direct-drive prelude refuses.
    ro_coherence::test_note_epoch_step();
    let sink = VecSink(std::sync::Mutex::new(vec![0u8; 4096]));
    let (probe, meta) = h.fs.ipc_read_probe_locked(ino, 0, 4096, &sink);
    assert!(
        matches!(probe, IpcReadProbe::Miss),
        "a pre-step entry demotes: {probe:?}"
    );
    assert!(meta.is_none(), "no stale map is handed to the lane touch");
    assert!(
        sink.0.lock().unwrap().iter().all(|&b| b == 0),
        "nothing was served"
    );
    assert_eq!(
        h.fs.ipc_direct_read_probe(ino, 0, 4096, 0).err(),
        Some(IpcDirectIneligible::Meta),
        "the direct-drive prelude refuses a pre-step entry"
    );
}

/// Contract 3 — the stamp is the generation BEFORE the read: an entry
/// resolved under generation g and published after a step to g+1 is a
/// miss at once (pre-step content never carries a post-step stamp).
#[test]
fn the_stamp_is_captured_before_the_read_so_a_step_mid_fetch_leaves_a_miss() {
    let _serial = serial();
    let before = ro_coherence::reader_step_generation();
    let entry = striped("524288", before);
    assert!(metadata_entry_fresh_or_dirty(&entry));
    ro_coherence::test_note_epoch_step();
    assert!(
        !metadata_entry_fresh_or_dirty(&entry),
        "stamped under the old generation ⇒ a miss after the step"
    );
    let current = striped("524288", ro_coherence::reader_step_generation());
    assert!(metadata_entry_fresh_or_dirty(&current));
    // A DIRTY entry is the local authority regardless (the aged-fsx law).
    let mut dirty = striped("524288", before);
    dirty.layout_dirty = true;
    assert!(metadata_entry_fresh_or_dirty(&dirty));
}

/// Contract 4 — a mount that never steps an epoch (every plain writer)
/// is byte-identical: the gate is `stamp ≥ 0`, always true, and no gauge
/// moves.
#[test]
fn a_mount_that_never_steps_pays_the_gate_nothing() {
    let _serial = serial();
    assert_eq!(ro_coherence::reader_layout_step_gen(), 0);
    for stamp in [0u64, 1, u64::MAX] {
        assert!(!ro_coherence::layout_entry_pre_step(stamp));
        assert!(metadata_entry_fresh_or_dirty(&striped("524288", stamp)));
    }
    assert_eq!(ro_coherence::reader_layout_step_misses(), 0);
}
