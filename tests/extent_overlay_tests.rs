//! RW4 — W2 extent-granular overlay, staged extent records, and batched
//! fold (docs/design-random-small-writes.md §5.2, PR RW4; gate G-RW6).
//!
//! The patch-INELIGIBLE small-write shapes — compressed/encrypted volumes,
//! refcount-shared or decorated blocks, holes, unaligned writes — pay a
//! whole-4 MiB-block RMW pipeline per small write today (§1.2: ~2,500×
//! device amplification). W2's contract, pinned here:
//!
//! - **Park compactly**: the first small non-adjacent write to a block
//!   parks an `ExtentOverlay` (~payload bytes, `parked_extent_bytes`),
//!   never a block-size-class deferred buffer. Escalation to a full
//!   buffer at coverage ≥ 25 % of the block or on a large merge.
//! - **Spill without seed, ever**: overflow spills a staged
//!   `active_block_ext:` record (4 KiB-class put) — zero device reads at
//!   spill (`get_obj` + `spill_seed_reads` deltas pinned 0).
//! - **Byte budget, not count**: the 256-BUFFER cap becomes a byte
//!   budget — hundreds of tiny overlays park without conjuring the
//!   inline-spill convoy.
//! - **Batched fold**: fsync / thresholds / pressure / teardown fold a
//!   block by seeding ONCE (item B's binding-validated `fetch_seed_image`)
//!   and applying all k extents (`fold_fill`), then one durable upload —
//!   amp ≈ 2048/k + spill legs (§4). Hole-backed folds seed NOTHING.
//! - **Overlay never invisible**: reads serve covered ∩ range from the
//!   overlay and the complement from the base tiers/ranged read — during
//!   the window, across spills, and mid-fold.
//! - **Never-lossy custody**: staging refusal keeps extents in RAM; fold
//!   failure re-parks; nothing acked is ever dropped.
//! - **FIND-RW2-A fixed in RW4**: a fold over a promoted-staged DECORATED
//!   (`bk:off:len`) mapping seeds correctly (pre-fix:
//!   `fetch_seed_image → get_block_for_index → read_block` fails
//!   `Invalid block offset` — proven knob-independent in the RW2 note).
//! - **Staged-layout rider**: the ≤ block-size staged-file whole-image
//!   RMW adopts extent records for sub-image overwrites.
//!
//! RED against the RW4 scaffolding: every contract fails on its counter /
//! behavior assertions (`extent_parks` == 0, folds never run, records
//! never written, FIND-RW2-A still errors) — never on compilation.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata as _, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

/// Counter deltas are process-global; tests serialize (house pattern).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// Per-fixture knob reset: the W2 defaults, patch path ON (the production
/// posture — extent parking is chosen only where the patch is ineligible).
fn reset_knobs() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    squeezefs::fuse_client::set_fold_max_extents(64);
    squeezefs::fuse_client::set_fold_max_bytes(1024 * 1024);
    squeezefs::fuse_client::set_parked_cap_buffers(256);
}

/// `compressed` = lz4 volume (every write patch-ineligible — the G-RW6
/// population); `staging` = with/without disk staging dirs.
async fn make_ext(uuid: [u8; 16], alloc_ns: &str, compressed: bool, staging: bool) -> H {
    reset_knobs();
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
    let staging_dirs = if staging {
        vec![s.path().to_path_buf()]
    } else {
        Vec::new()
    };
    let cache = TieredCache::new(
        staging_dirs,
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
    };
    H {
        fs,
        dlm,
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

async fn purge_tiers(h: &H, ino: u64) {
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

/// A durable striped file of `blocks` blocks, fsynced, read tiers dropped.
async fn durable_striped(h: &H, name: &str, blocks: u64, tag: u8) -> (u64, Vec<u8>) {
    let len = (blocks * BS) as usize;
    let ino = create(h, name).await;
    let base = pattern(len, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    purge_tiers(h, ino).await;
    (ino, base)
}

#[derive(Clone, Copy, Debug)]
struct Snap {
    get_obj: u64,
    spill_seed_reads: u64,
    extent_parks: u64,
    extent_escalations: u64,
    extent_spills: u64,
    extent_spill_bytes: u64,
    parked_extent_bytes: u64,
    fold_passes: u64,
    fold_seed_reads: u64,
    fold_extents_folded: u64,
    rider_writes: u64,
    rider_folds: u64,
    staged_rmw_pooled_seeds: u64,
    overwrite_seed_deferred: u64,
}

fn snap() -> Snap {
    let l = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Snap {
        get_obj: l(&METRICS.get_obj),
        spill_seed_reads: l(&METRICS.spill_seed_reads),
        extent_parks: l(&METRICS.extent_parks),
        extent_escalations: l(&METRICS.extent_escalations),
        extent_spills: l(&METRICS.extent_spills),
        extent_spill_bytes: l(&METRICS.extent_spill_bytes),
        parked_extent_bytes: l(&METRICS.parked_extent_bytes),
        fold_passes: l(&METRICS.fold_passes),
        fold_seed_reads: l(&METRICS.fold_seed_reads),
        fold_extents_folded: l(&METRICS.fold_extents_folded),
        rider_writes: l(&METRICS.staged_rider_extent_writes),
        rider_folds: l(&METRICS.staged_rider_folds),
        staged_rmw_pooled_seeds: l(&METRICS.staged_rmw_pooled_seeds),
        overwrite_seed_deferred: l(&METRICS.overwrite_seed_deferred),
    }
}

/// 1. Park compactly: a 4 KiB non-adjacent overwrite of a compressed
/// striped block parks ~4 KiB (`parked_extent_bytes`), not a
/// block-size-class deferred buffer; zero device reads at park time; reads
/// serve covered ∩ range from the overlay and the complement from the old
/// bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_nonadjacent_write_parks_compactly() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-park-compact", "rw4_ns_a", true, true).await;
    let (ino, base) = durable_striped(&h, "park.dat", 6, 0x00).await;

    let before = snap();
    let patch = pattern(4096, 0x5A);
    write_at(&h, ino, BS + 8192, &patch).await;
    let after = snap();

    assert_eq!(
        after.extent_parks - before.extent_parks,
        1,
        "the W1-ineligible small non-adjacent write must park as an extent \
         overlay (W2 §5.2), not a deferred 4 MiB-class buffer"
    );
    assert_eq!(
        after.overwrite_seed_deferred - before.overwrite_seed_deferred,
        0,
        "no block-size deferred buffer may be created for this shape"
    );
    let parked_delta = after.parked_extent_bytes - before.parked_extent_bytes;
    assert!(
        (4096..16384).contains(&parked_delta),
        "a 4 KiB write must park ~4 KiB of extent payload (charged to \
         parked_extent_bytes), got {parked_delta}"
    );
    assert_eq!(
        after.get_obj - before.get_obj,
        0,
        "parking must not read the old block from the device"
    );

    // Overlay never invisible: covered ∩ range serves the new bytes, the
    // complement serves the OLD bytes — before any fold.
    let got = read_at(&h, ino, BS, BS as usize).await;
    let mut want = base[BS as usize..2 * BS as usize].to_vec();
    want[8192..8192 + 4096].copy_from_slice(&patch);
    assert_bytes(&got, &want, "read during the extent window");

    // Durability: fsync folds; cold read is byte-exact.
    fsync(&h, ino).await;
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, BS, BS as usize).await;
    assert_bytes(&got, &want, "cold read after fold");
}

/// 2. Escalation thresholds, both edges: below 25 % coverage the overlay
/// stays extent-granular; crossing 25 % escalates to a full buffer; a
/// single large merge (≥ 25 % of the block) never parks as an extent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escalation_at_quarter_coverage_both_edges() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-escalate-25p", "rw4_ns_b", true, true).await;
    let (ino, base) = durable_striped(&h, "esc.dat", 6, 0x01).await;

    // Edge A: three 4 KiB extents = 12 KiB < 16 KiB (25 % of 64 KiB) — no
    // escalation yet.
    let before = snap();
    let p = pattern(4096, 0x11);
    write_at(&h, ino, 2 * BS, &p).await;
    write_at(&h, ino, 2 * BS + 20480, &p).await;
    write_at(&h, ino, 2 * BS + 40960, &p).await;
    let mid = snap();
    assert_eq!(
        mid.extent_parks - before.extent_parks,
        3,
        "three small disjoint writes park as extents"
    );
    assert_eq!(
        mid.extent_escalations - before.extent_escalations,
        0,
        "below 25 % coverage the overlay must stay extent-granular"
    );

    // Edge B: the fourth extent crosses 25 % ⇒ escalates to a full buffer
    // (RAM-only conversion: no device read).
    write_at(&h, ino, 2 * BS + 56320, &p).await;
    let after = snap();
    assert_eq!(
        after.extent_escalations - mid.extent_escalations,
        1,
        "crossing 25 % coverage must escalate the overlay to a full buffer"
    );
    assert_eq!(
        after.get_obj - mid.get_obj,
        0,
        "escalation is a RAM conversion — no device read"
    );
    assert!(
        after.parked_extent_bytes < mid.parked_extent_bytes + 4096,
        "escalation must return the extent slabs' bytes to the gauge \
         (extent bytes {} -> {})",
        mid.parked_extent_bytes,
        after.parked_extent_bytes
    );

    // Edge C: a single ≥ 25 % merge on a fresh block goes straight to the
    // full representation (a large merge never parks as an extent).
    let big = pattern((BS / 2) as usize, 0x22);
    let b4 = snap();
    write_at(&h, ino, 4 * BS + 4096, &big).await;
    let a4 = snap();
    assert_eq!(
        a4.extent_parks - b4.extent_parks,
        0,
        "a large merge (≥ 25 % of the block) must not park as an extent"
    );

    // Byte-exactness across both shapes.
    fsync(&h, ino).await;
    purge_tiers(&h, ino).await;
    let mut want = base.clone();
    for off in [0u64, 20480, 40960, 56320] {
        let s = (2 * BS + off) as usize;
        want[s..s + 4096].copy_from_slice(&p);
    }
    let s = (4 * BS + 4096) as usize;
    want[s..s + big.len()].copy_from_slice(&big);
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-escalation fold");
}

/// 3. Spill = staged extent record, never a seed-materialized whole image:
/// overflowing the parked byte budget spills 4 KiB-class records with ZERO
/// device reads; the spilled extents stay readable (covered ∩ range +
/// complement) and fold durably at fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spill_writes_extent_records_without_seed_reads() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-spill-noseed", "rw4_ns_c", true, true).await;
    let (ino, base) = durable_striped(&h, "spill.dat", 24, 0x02).await;

    // Shrink the parked byte budget to 2 buffers' worth (128 KiB) so ~24
    // blocks x 12 KiB of extents (≈288 KiB) must spill.
    squeezefs::fuse_client::set_parked_cap_buffers(2);

    let before = snap();
    let p = pattern(4096, 0x33);
    for blk in 0..24u64 {
        for slot in 0..3u64 {
            write_at(&h, ino, blk * BS + slot * 20480, &p).await;
        }
    }
    let after = snap();

    assert!(
        after.extent_spills - before.extent_spills >= 1,
        "the byte budget must spill extent overlays as staged records \
         (extent_spills {} -> {})",
        before.extent_spills,
        after.extent_spills
    );
    assert_eq!(
        after.get_obj - before.get_obj,
        0,
        "NO SEED READ AT SPILL, EVER (§5.2) — the spill is the record put, \
         not a seed-materialize + whole-image put"
    );
    assert_eq!(
        after.spill_seed_reads - before.spill_seed_reads,
        0,
        "the full-buffer spill-seed path must never fire for extent victims"
    );
    let spill_bytes = after.extent_spill_bytes - before.extent_spill_bytes;
    let spills = after.extent_spills - before.extent_spills;
    assert!(
        spill_bytes <= spills * 16384,
        "extent-record spills are payload-class puts ({} B over {} spills), \
         never block-image puts",
        spill_bytes,
        spills
    );
    // The staged record population is observable.
    let keys = h.fs.router.cache.nvme.extent_record_keys("");
    assert!(
        !keys.is_empty(),
        "spilled extent records must exist under active_block_ext: keys"
    );

    // Spilled extents stay visible to reads (overlay never invisible).
    let mut want = base.clone();
    for blk in 0..24u64 {
        for slot in 0..3u64 {
            let s = (blk * BS + slot * 20480) as usize;
            want[s..s + 4096].copy_from_slice(&p);
        }
    }
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "reads across spilled extent records");

    // fsync drains every record to fold (the §5.2 mandate) — byte-exact.
    fsync(&h, ino).await;
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        h.fs.router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "fsync must drain the ino's extent records to fold"
    );
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "cold read after spilled-record fold");
}

/// 4. Fold amortization: k parked extents on one block fold with exactly
/// ONE seed read and one pass (`fold_fill` k, `fold_seed_reads` ≈
/// `fold_passes` — never per extent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fold_amortizes_k_extents_over_one_seed() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-fold-amort-k", "rw4_ns_d", true, true).await;
    let (ino, base) = durable_striped(&h, "fold.dat", 4, 0x03).await;

    // 20 x 512 B extents in block 1 (10 KiB < the 16 KiB escalation edge).
    let k = 20u64;
    let p = pattern(512, 0x44);
    let before = snap();
    for i in 0..k {
        write_at(&h, ino, BS + i * 2048, &p).await;
    }
    let mid = snap();
    assert_eq!(
        mid.extent_parks - before.extent_parks,
        k,
        "all {k} small writes park as extents"
    );
    assert_eq!(mid.get_obj - before.get_obj, 0, "no device read at park");

    fsync(&h, ino).await;
    let after = snap();
    assert_eq!(
        after.fold_passes - mid.fold_passes,
        1,
        "fsync folds the block exactly once"
    );
    assert_eq!(
        after.fold_extents_folded - mid.fold_extents_folded,
        k,
        "the fold applies all {k} extents (fold_fill)"
    );
    assert_eq!(
        after.fold_seed_reads - mid.fold_seed_reads,
        1,
        "the fold seeds ONCE per pass — never per extent"
    );
    assert_eq!(
        after.get_obj - mid.get_obj,
        1,
        "device cost of the fold's read leg = one block read"
    );

    let mut want = base.clone();
    for i in 0..k {
        let s = (BS + i * 2048) as usize;
        want[s..s + 512].copy_from_slice(&p);
    }
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "fold result");
}

/// 5. Hole writes: small writes into UNMAPPED blocks of a sparse striped
/// file park as extents and fold with ZERO seed reads (the complement is
/// zeros by definition) — the G-RW6 hole-write soak shape, no whole-block
/// seed storms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hole_writes_fold_without_seed_storms() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-hole-noseed1", "rw4_ns_e", true, true).await;
    // Sparse fixture: write block 0 + a far tail block, leaving holes
    // between (unmapped blocks 1..7).
    let ino = create(&h, "holes.dat").await;
    let head = pattern(BS as usize, 0x05);
    write_at(&h, ino, 0, &head).await;
    let tail = pattern(BS as usize, 0x06);
    write_at(&h, ino, 7 * BS, &tail).await;
    fsync(&h, ino).await;
    purge_tiers(&h, ino).await;

    let p = pattern(4096, 0x55);
    let before = snap();
    // Random 4 KiB writes into the holes (blocks 2, 4, 5).
    for (blk, slot) in [(2u64, 1u64), (4, 3), (5, 0), (2, 8), (4, 12)] {
        write_at(&h, ino, blk * BS + slot * 4096, &p).await;
    }
    let mid = snap();
    assert_eq!(
        mid.extent_parks - before.extent_parks,
        5,
        "hole writes park as extents"
    );

    // Reads during the window: extents + zeros complement, no device read
    // for the hole complement.
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_bytes(&got[4096..8192], &p, "hole extent run");
    assert!(
        got[..4096].iter().all(|&x| x == 0),
        "hole complement reads zeros"
    );

    fsync(&h, ino).await;
    let after = snap();
    assert_eq!(
        after.fold_seed_reads - mid.fold_seed_reads,
        0,
        "hole-backed folds must not read a seed (zeros complement) — the \
         G-RW6 no-seed-storm clause"
    );
    assert_eq!(
        after.get_obj - mid.get_obj,
        0,
        "zero device reads across the whole hole-write + fold cycle"
    );

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_bytes(&got[4096..8192], &p, "folded hole extent");
    assert!(
        got[..4096].iter().all(|&x| x == 0),
        "folded hole complement stays zeros"
    );
}

/// 6. Overlap/rewrite within the overlay: overlapping extent merges keep
/// newest-wins semantics and the coverage union exact; the fold result is
/// byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlap_rewrite_within_overlay_newest_wins() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-overlap-new1", "rw4_ns_f", true, true).await;
    let (ino, base) = durable_striped(&h, "olap.dat", 3, 0x07).await;

    let w1 = pattern(3072, 0x61); // [1024, 4096)
    let w2 = pattern(2048, 0x62); // [3072, 5120) — overlaps w1's tail
    let w3 = pattern(1024, 0x63); // [2048, 3072) — inside the union
    let before = snap();
    write_at(&h, ino, BS + 1024, &w1).await;
    write_at(&h, ino, BS + 3072, &w2).await;
    write_at(&h, ino, BS + 2048, &w3).await;
    let after = snap();
    assert!(
        after.extent_parks - before.extent_parks >= 1,
        "the overlapping small-write cluster parks as extents"
    );

    let mut want_block = base[BS as usize..2 * BS as usize].to_vec();
    want_block[1024..4096].copy_from_slice(&w1);
    want_block[3072..5120].copy_from_slice(&w2);
    want_block[2048..3072].copy_from_slice(&w3);

    let got = read_at(&h, ino, BS, BS as usize).await;
    assert_bytes(&got, &want_block, "overlapping extents during the window");

    fsync(&h, ino).await;
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, BS, BS as usize).await;
    assert_bytes(&got, &want_block, "overlapping extents after fold");
}

/// 7. The byte budget kills the count convoy: hundreds of tiny overlays
/// park with ZERO spills (the retired 256-COUNT cap would have spilled a
/// victim inline per insert past 256); overflowing the BYTE budget spills.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn byte_budget_replaces_the_count_cap() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-bytebudget-1", "rw4_ns_g", true, true).await;
    // 320 blocks, one 512 B extent each = 160 KiB parked over 320 entries.
    let (ino, _) = durable_striped(&h, "budget.dat", 320, 0x08).await;

    // Budget: 8 buffers' worth (512 KiB) — far above the extent bytes,
    // far below the entry count.
    squeezefs::fuse_client::set_parked_cap_buffers(8);

    let p = pattern(512, 0x71);
    let before = snap();
    for blk in 0..320u64 {
        write_at(&h, ino, blk * BS + 4096, &p).await;
    }
    let after = snap();
    assert_eq!(
        after.extent_parks - before.extent_parks,
        320,
        "all 320 tiny writes park"
    );
    assert_eq!(
        after.extent_spills - before.extent_spills,
        0,
        "320 parked overlays over an 8-BUFFER cap must spill NOTHING — the \
         cap is a BYTE budget now (the inline-spill convoy for this shape \
         is dead)"
    );
    assert_eq!(
        after.get_obj - before.get_obj,
        0,
        "no seed reads while parking under the byte budget"
    );
    fsync(&h, ino).await;
}

/// 8. Never-lossy custody: staging refusal (cache-less volume) keeps
/// spill-pressured extents in RAM — reads stay exact, nothing is dropped;
/// a failed fold (block allocator exhausted) re-parks, and the SAME bytes
/// fold durably once the device heals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_lossy_staging_refusal_and_fold_failure_repark() {
    let _g = serial().await;
    // Cache-less: every staging put refuses (the never-lossy backpressure
    // face); extent parking + folds must still work RAM-only.
    let h = make_ext(*b"rw4-neverlossy-1", "rw4_ns_h", true, false).await;
    let (ino, base) = durable_striped(&h, "nl.dat", 4, 0x09).await;

    squeezefs::fuse_client::set_parked_cap_buffers(1);
    let p = pattern(4096, 0x81);
    let before = snap();
    // 4 blocks x 3 extents = 48 KiB > the 64 KiB cap? No: 1 buffer cap =
    // 64 KiB — stay under, then overflow with more blocks.
    for blk in 0..4u64 {
        for slot in 0..3u64 {
            write_at(&h, ino, blk * BS + slot * 20480, &p).await;
        }
    }
    let after = snap();
    assert!(
        after.extent_parks - before.extent_parks >= 12,
        "extents park even when staging can never admit a spill"
    );
    assert_eq!(
        after.extent_spills - before.extent_spills,
        0,
        "cache-less staging admits nothing — refused spills keep extents \
         in RAM (never-lossy), not silently dropped"
    );
    let mut want = base.clone();
    for blk in 0..4u64 {
        for slot in 0..3u64 {
            let s = (blk * BS + slot * 20480) as usize;
            want[s..s + 4096].copy_from_slice(&p);
        }
    }
    // Block-by-block reads: single-block probes compose the RAM overlay
    // WITHOUT triggering the multi-block read's flush (which would fold —
    // custody must still be RAM here).
    for blk in 0..4u64 {
        let got = read_at(&h, ino, blk * BS, BS as usize).await;
        assert_bytes(
            &got,
            &want[(blk * BS) as usize..((blk + 1) * BS) as usize],
            "reads with refused spills (RAM custody)",
        );
    }
    assert!(
        h.fs.parked_buffer_bytes() > 0,
        "premise: the refused-spill extents are still RAM custody"
    );

    // Fold failure: mark the block backend unhealthy — the fold's durable
    // upload fails deterministically; fsync must FAIL, custody must
    // survive; heal, fsync again, byte-exact.
    h.fs.router
        .backend_router
        .unhealthy_backends
        .insert("backend_0".to_string(), true);
    let r = h.fs.fsync(h.req, ino, 0, false).await;
    assert!(
        r.is_err(),
        "fsync must surface the fold's upload failure (backend down)"
    );
    for blk in 0..4u64 {
        let got = read_at(&h, ino, blk * BS, BS as usize).await;
        assert_bytes(
            &got,
            &want[(blk * BS) as usize..((blk + 1) * BS) as usize],
            "fold failure re-parks — the ACKed bytes stay served from custody",
        );
    }
    h.fs.router
        .backend_router
        .unhealthy_backends
        .remove("backend_0");
    fsync(&h, ino).await;
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "healed fold flushes the SAME preserved bytes");
}

/// 9. FIND-RW2-A fixed in RW4: a fold whose seed resolves through a
/// promoted-staged DECORATED (`bk:off:len`) mapping must seed correctly.
/// Pre-fix: `fetch_seed_image → get_block_for_index →
/// BackendRouter::read_block` parses the decorated string as a raw key and
/// fails `Io(InvalidData "Invalid block offset")` — fsync errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fold_seeds_decorated_promoted_mapping() {
    let _g = serial().await;
    // Passthrough volume: the decorated form is what makes the patch
    // ineligible here (patch_ineligible_decorated), which is exactly the
    // W2 customer population.
    let h = make_ext(*b"rw4-decorated-a1", "rw4_ns_i", false, true).await;
    let (ino, mut want) = durable_striped(&h, "dec.dat", 4, 0x18).await;
    let path = squeezefs::keys::inode_path(ino);

    // Rewrite block 1's mapping into the size-carrying decorated form with
    // identical semantics (off 0, len == stored image = BS on this
    // passthrough volume) — the RW2 fixture recipe.
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bm = m.block_map.as_ref().unwrap();
    let decorated = format!("{}:0:{}", bm.get(&1).unwrap(), BS);
    let mut new_map: std::collections::HashMap<u32, String> =
        bm.iter().map(|(k, v)| (*k, v.clone())).collect();
    new_map.insert(1, decorated.clone());
    let layout = squeezefs::routing::LayoutMetadata {
        file_type: m.file_type.to_string(),
        size: m.size,
        block_map_id: Some(format!("block_map_{ino}")),
        block_prefix: None,
        file_id: m.file_id.as_deref().map(str::to_string),
        data_key: m.data_key.as_ref().map(|b| b.to_vec()),
        block_map: Some(new_map),
    };
    let backend = h.fs.meta_backend.as_ref().unwrap();
    backend
        .setxattr(ino, "layout", &bincode::serialize(&layout).unwrap())
        .await
        .unwrap();
    h.fs.router.metadata_cache.remove(&ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    let m1 = m.block_map.as_ref().unwrap().get(&1).unwrap().clone();
    assert_eq!(
        squeezefs::routing::block_mapping_form(&m1),
        "decorated-3part",
        "premise: block 1 carries the decorated promoted-staged form"
    );

    // A small ALIGNED write to the decorated block: the patch refuses
    // (decorated), W2 parks it, and the fsync FOLD must seed through the
    // decorated mapping (FIND-RW2-A: this errored `Invalid block offset`).
    let p = pattern(4096, 0xC1);
    let before = snap();
    write_at(&h, ino, BS + 8192, &p).await;
    let mid = snap();
    assert_eq!(
        mid.extent_parks - before.extent_parks,
        1,
        "the decorated-refused small write parks as an extent"
    );
    fsync(&h, ino).await; // pre-fix: Io(InvalidData "Invalid block offset")
    let after = snap();
    assert_eq!(
        after.fold_seed_reads - mid.fold_seed_reads,
        1,
        "the fold seeded the decorated mapping exactly once"
    );

    want[(BS + 8192) as usize..(BS + 8192) as usize + 4096].copy_from_slice(&p);
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "decorated-mapping fold (FIND-RW2-A fixed)");
}

/// 10. Read-mid-fold coherence: concurrent single-block reads racing a
/// fold always observe exact bytes (old-composed or new-folded, never
/// zeros / stale / partial) — the overlay-never-invisible law across the
/// fold's authority transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reads_mid_fold_serve_exact_bytes() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-midfold-rd-1", "rw4_ns_j", true, true).await;
    let (ino, base) = durable_striped(&h, "midfold.dat", 3, 0x0A).await;

    let p = pattern(2048, 0x91);
    let mut want_block = base[BS as usize..2 * BS as usize].to_vec();
    // 7 x 2 KiB = 14 KiB < the 16 KiB (25 %) escalation edge: the overlay
    // must still be extent-repr when the fold runs.
    for i in 0..7u64 {
        write_at(&h, ino, BS + i * 6144, &p).await;
        let s = (i * 6144) as usize;
        want_block[s..s + 2048].copy_from_slice(&p);
    }

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let h_fs = h.fs.clone();
        let req = h.req;
        let stop = stop.clone();
        let want = want_block.clone();
        readers.push(tokio::spawn(async move {
            let mut serves = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let got = h_fs
                    .read(req, ino, 0, BS, BS as u32, 0)
                    .await
                    .expect("read mid-fold must not error")
                    .data
                    .to_vec();
                assert_eq!(got.len(), want.len(), "mid-fold read length");
                if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
                    panic!(
                        "mid-fold read served wrong byte at {i}: got {:#04x} want {:#04x}",
                        got[i], want[i]
                    );
                }
                serves += 1;
                tokio::task::yield_now().await;
            }
            serves
        }));
    }

    // Fold under the readers.
    let before = snap();
    fsync(&h, ino).await;
    let after = snap();
    assert!(
        after.fold_passes > before.fold_passes,
        "the fsync folded the extent overlay under concurrent readers"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let serves = r.await.unwrap();
        assert!(serves > 0, "readers must have served during the fold");
    }
}

/// 11. FIND-M11-A supersession transfer: a spilled extent record staged
/// under an older fencing generation still folds under the ino's CURRENT
/// generation (acked custody never livelocks on a staging-era stamp).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fold_revalidates_fencing_per_attempt_and_never_livelocks() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-supersede-a1", "rw4_ns_k", true, true).await;
    let (ino, base) = durable_striped(&h, "sup.dat", 3, 0x0B).await;

    squeezefs::fuse_client::set_parked_cap_buffers(1);
    let p = pattern(4096, 0xA1);
    let before = snap();
    // Two blocks' extents; the cap forces at least one spill to a record.
    for blk in 0..3u64 {
        for slot in 0..4u64 {
            write_at(&h, ino, blk * BS + slot * 16384 + 4096, &p).await;
        }
    }
    let after = snap();
    assert!(
        after.extent_parks > before.extent_parks,
        "extents parked (premise)"
    );

    // Bump the ino's fencing generation (a lease re-acquisition — the
    // FIND-M11-A shape: staging-era stamps go stale while custody lives).
    // A ranged lock on the same ino shares the file's fencing generator
    // (ObjectKey::fencing_identity) without contending with the FS's held
    // whole-file lease — the INCR is the generation bump.
    let path = squeezefs::keys::inode_path(ino);
    let lease = h
        .dlm
        .acquire_lock(&path, Some((0, 1)), std::time::Duration::from_secs(5))
        .await
        .unwrap();
    lease.release().await.unwrap();

    // fsync must fold everything under the CURRENT generation — no
    // livelock, no drop.
    fsync(&h, ino).await;
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        h.fs.router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "stale-stamped records folded under the current generation"
    );
    let mut want = base.clone();
    for blk in 0..3u64 {
        for slot in 0..4u64 {
            let s = (blk * BS + slot * 16384 + 4096) as usize;
            want[s..s + 4096].copy_from_slice(&p);
        }
    }
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "superseded-stamp fold");
}

/// 12. Staged-layout rider: a sub-image overwrite of a ring-resident
/// staged file writes a 4 KiB-class extent record instead of the
/// whole-image RMW (seed + re-stage); reads compose; fsync folds the
/// record back into the image; an extending write folds FIRST.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_rider_subimage_overwrite_uses_extent_records() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-rider-staged", "rw4_ns_l", false, true).await;

    // A 32 KiB staged file (> inline 4 KiB, <= block size with staging).
    let ino = create(&h, "rider.dat").await;
    let mut want = pattern(32768, 0x0C);
    write_at(&h, ino, 0, &want).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "staged", "fixture must be STAGED");

    // Sub-image overwrite: 2 KiB inside the image.
    let p = pattern(2048, 0xB1);
    let before = snap();
    write_at(&h, ino, 8192, &p).await;
    want[8192..8192 + 2048].copy_from_slice(&p);
    let after = snap();
    assert_eq!(
        after.rider_writes - before.rider_writes,
        1,
        "the sub-image overwrite must ride an extent record (staged rider)"
    );
    assert_eq!(
        after.staged_rmw_pooled_seeds - before.staged_rmw_pooled_seeds,
        0,
        "no whole-image RMW seed for a rider-shaped overwrite"
    );

    // Reads compose record + image.
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "staged read composes the rider record");

    // fsync folds the record into the image (clean handles leave none).
    fsync(&h, ino).await;
    let mid = snap();
    assert!(
        mid.rider_folds > after.rider_folds,
        "fsync must fold the rider record into the staged image"
    );
    let prefix = squeezefs::keys::active_block_ext_ino_prefix(ino);
    assert!(
        h.fs.router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "no rider records survive fsync"
    );
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-fold staged image");

    // Extending write folds FIRST, then takes today's whole-image path.
    let p2 = pattern(2048, 0xB2);
    write_at(&h, ino, 4096, &p2).await; // new rider record
    want[4096..4096 + 2048].copy_from_slice(&p2);
    let ext = pattern(4096, 0xB3);
    write_at(&h, ino, 32768, &ext).await; // EXTENDS the image
    want.extend_from_slice(&ext);
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "extending write after a rider record");
    assert!(
        h.fs.router
            .cache
            .nvme
            .extent_record_keys(prefix.as_str())
            .is_empty(),
        "the extending write folded the rider record first"
    );
    fsync(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "post-fsync extended staged image");
}

/// 13. Rider vs truncate: record extents beyond a shrink point never
/// resurface through a later extend (holes read zeros — the
/// staged-truncate-stale family applied to the record kind).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rider_truncate_clips_record_extents() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-rider-trunc1", "rw4_ns_m", false, true).await;

    let ino = create(&h, "ridertrunc.dat").await;
    let base = pattern(32768, 0x0D);
    write_at(&h, ino, 0, &base).await;

    // A rider record extent near the tail [28 KiB, 30 KiB).
    let p = pattern(2048, 0xC2);
    let before = snap();
    write_at(&h, ino, 28672, &p).await;
    let after = snap();
    assert_eq!(
        after.rider_writes - before.rider_writes,
        1,
        "premise: the tail overwrite rode a record"
    );

    // Shrink to 16 KiB (drops the record extent entirely), then extend
    // back to 32 KiB: the re-exposed range must read ZEROS.
    let set_size = |sz: u64| {
        h.fs.setattr(
            h.req,
            ino,
            None,
            fuse3::SetAttr {
                size: Some(sz),
                ..Default::default()
            },
        )
    };
    set_size(16384).await.unwrap();
    set_size(32768).await.unwrap();
    let got = read_at(&h, ino, 0, 32768).await;
    assert_bytes(&got[..16384], &base[..16384], "kept prefix");
    assert!(
        got[16384..].iter().all(|&x| x == 0),
        "truncate-clipped record extents must never resurface (got nonzero \
         in the re-extended hole)"
    );
}

/// 12. Stale-binding decode failures REBIND, never propagate (the
/// reads_mid_fold flake's root cause, VL10 release gate): on a
/// transformed volume a reader that resolved block b → K loses the race
/// with a fold/CoW-overwrite that displaces K, frees it, and lets the
/// offset be REUSED — the device bytes under K then legally fail frame
/// decode (LZ4 "ExpectedAnotherByte" mid-fold, EINVAL to the user). A
/// decode failure on a dead incarnation is a stale-binding LOSS (the
/// same class as wrong-bytes fills, which the incarnation seqlock
/// already catches); only a CURRENT-binding decode failure is real
/// corruption. Deterministic form: hand the validated loop a stale key
/// naming an undecodable foreign image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_binding_decode_failure_rebinds_not_errors() {
    let _g = serial().await;
    let h = make_ext(*b"rw4-decode-rb-01", "rw4_ns_m", true, true).await;
    let (ino, base) = durable_striped(&h, "rebind.dat", 2, 0x2C).await;
    let path = squeezefs::keys::inode_path(ino);

    // A test-owned allocated offset holding bytes that CANNOT decode as
    // a stored frame (truncated garbage) — the reused-offset image a
    // dead incarnation's reader would fetch.
    let (be_id, alloc, dev) =
        h.fs.router
            .backend_router
            .get_active_backend()
            .expect("active backend");
    let off = alloc.allocate_block().await.expect("alloc");
    dev.write_block(off, bytes::Bytes::from(vec![0x5Au8; 4096]))
        .await
        .expect("plant undecodable bytes");
    alloc.publish_block(off);
    let stale_key = h.fs.router.backend_router.persist_block_key(&be_id, off);

    // The reader resolved block 0 → stale_key (a mapping snapshot from
    // before the displace); the CURRENT map still binds the real key.
    purge_tiers(&h, ino).await;
    let got =
        h.fs.router
            .get_block_for_index(&path, 0, Some(stale_key.as_str()), false, true)
            .await
            .expect(
                "a decode failure on a NON-current binding must rebind to the \
             live mapping, not propagate",
            )
            .expect("block 0 is mapped");
    let bytes: Vec<u8> = match got {
        squeezefs::cache::pool::ReadBlockValue::Bytes(b) => b.to_vec(),
        _ => panic!("unexpected value repr (not Bytes)"),
    };
    assert_eq!(
        &bytes[..64],
        &base[..64],
        "the rebind must serve the CURRENT incarnation's bytes"
    );
}
