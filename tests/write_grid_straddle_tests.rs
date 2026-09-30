//! §4.4ci — a sequential stream on a MISALIGNED writeback grid must land
//! every block exactly once (`.benchmarks/2026-09-19-sym-acceptance.md`
//! §4.4ci; the cloud row's 64 GiB ingest, §3.11).
//!
//! The kernel's writeback batches a dirty file into `max_write` (1 MiB)
//! WRITEs starting from an arbitrary page, so once a chunk boundary
//! falls off the 1 MiB grid every fourth WRITE of a 4 MiB block STRADDLES
//! a block boundary, and the 256-deep FUSE queue delivers the segments
//! of one block out of order. `write_file_staged` splits a straddling
//! WRITE into two single-block, page-aligned pieces — but the device
//! overlay's shape screen judged the WHOLE write (`start_block ==
//! end_block`, the length floor), so both pieces declined into the
//! accumulation path: the tail piece settled the block's half-filled
//! overlay record (published with a zero-seeded gap) and started an
//! active buffer whose coverage union could never complete, and the
//! fsync's flush then re-read the whole block from the device and
//! uploaded it whole again, one block at a time (the cloud: 7,286 of
//! 16,384 blocks, 1,068 s; the laptop repro: fsync 0.13 s → 6.2 s).
//!
//! The law pinned here: for a stream whose every segment is page-
//! aligned, a block is landed ONCE — by the overlay when its first
//! segment opened a record (later segments JOIN the open record at any
//! aligned length; the length floor governs the INSTALL of a record,
//! never a join), by write-through when its first segment took the
//! accumulation path — so the fsync flush finds no partial buffer, reads
//! nothing back and uploads nothing twice; device bytes ≡ user bytes.
//!
//! Geometry: 64 KiB blocks (derived W1 cap 8 KiB), 16 KiB segments (the
//! 1 MiB analog), a ONE-PAGE skew (the cloud's grid: 534 short chunk
//! ends over 65,536 MiB). The skewed stream is delivered as the kernel
//! delivers it: the inner segments of every block first, the straddles
//! after (phase 2) — the shape that filled half a record before the
//! decline. Two controls: the aligned grid and the skewed grid in order.
//! ACK-early is pinned OFF so every counter is settled when `write`
//! returns (the device_overlay_tests convention).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::device_overlay::{
    set_ack_early_for_tests, set_device_overlay_for_tests, set_overlay_overwrite_for_tests,
};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    derived_patch_max_bytes, set_patch_max_bytes, SqueezefsFilesystem, METRICS,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::NamedTempFile;

const BS: u64 = 64 * 1024;
const SEG: u64 = BS / 4;
const PAGE: u64 = 4096;
/// Blocks per stream — enough that a per-block leak reads as a count,
/// small enough for the file-backed sandbox.
const BLOCKS: u64 = 24;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
}

async fn make(tag: &str) -> (H, LeverGuard) {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    set_device_overlay_for_tests(true, true);
    set_overlay_overwrite_for_tests(true);
    set_ack_early_for_tests(false, false);
    squeezefs::routing::set_rewrite_shadow(true);
    set_patch_max_bytes(derived_patch_max_bytes(BS));
    let guard = LeverGuard;

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let mut uuid = *b"grid-straddle-ci";
    uuid[15] = tag.len() as u8;
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0x44C1_44C1_44C1_44C1,
        uuid,
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    // CACHE-LESS (the cloud format's posture, `format` without
    // `--disk-cache-paths`): a beyond-inline write routes striped at
    // once — no staged layout, no promotion path in front of block 0.
    let cache = TieredCache::new(
        vec![],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        Some("v3:grid-straddle-test-generation"),
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
    assert_eq!(
        fs.router.block_size.load(Ordering::Relaxed),
        BS,
        "harness premise: the router runs the requested block size"
    );
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    (
        H {
            fs,
            req,
            _b: b,
            _m: m,
        },
        guard,
    )
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off}: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ tag | 1).collect()
}

fn m(v: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    v.load(Ordering::Relaxed)
}

/// The routing ledger the law reads.
#[derive(Clone, Copy, Debug)]
struct Snap {
    overlay_publishes: u64,
    overlay_stores: u64,
    overlay_store_bytes: u64,
    overlay_gap_seed_bytes: u64,
    overlay_teardown_waits: u64,
    write_through_blocks: u64,
    write_through_bytes: u64,
    flush_seed_read_bytes: u64,
    overwrite_seed_materialized: u64,
    durable_upload_escalation: u64,
    durable_upload_other: u64,
    patch_write_bytes: u64,
    fold_passes: u64,
}

fn snap() -> Snap {
    Snap {
        overlay_publishes: m(&METRICS.overlay_publishes),
        overlay_stores: m(&METRICS.overlay_stores),
        overlay_store_bytes: m(&METRICS.overlay_store_bytes),
        overlay_gap_seed_bytes: m(&METRICS.overlay_gap_seed_bytes),
        overlay_teardown_waits: m(&METRICS.overlay_teardown_waits),
        write_through_blocks: m(&METRICS.write_through_blocks),
        write_through_bytes: m(&METRICS.write_through_bytes),
        flush_seed_read_bytes: m(&METRICS.flush_seed_read_bytes),
        overwrite_seed_materialized: m(&METRICS.overwrite_seed_materialized),
        durable_upload_escalation: m(&METRICS.durable_upload_bytes_escalation),
        durable_upload_other: m(&METRICS.durable_upload_bytes_writeback)
            + m(&METRICS.durable_upload_bytes_self_flush),
        patch_write_bytes: METRICS.patch_write_bytes.load(Ordering::Relaxed),
        fold_passes: m(&METRICS.fold_passes),
    }
}

macro_rules! delta {
    ($after:expr, $before:expr, $field:ident) => {
        $after.$field - $before.$field
    };
}

/// Data-namespace device write bytes, composed from the ledger faces
/// (the in-process twin of the rig's `/proc/diskstats` column).
fn device_write_bytes(after: Snap, before: Snap) -> u64 {
    delta!(after, before, overlay_store_bytes)
        + delta!(after, before, overlay_gap_seed_bytes)
        + delta!(after, before, patch_write_bytes)
        + delta!(after, before, write_through_bytes)
        + delta!(after, before, durable_upload_escalation)
        + delta!(after, before, durable_upload_other)
        + delta!(after, before, fold_passes) * BS
}

/// One WRITE as the kernel would deliver it: `(offset, len)`.
type Seg = (u64, u64);

/// The kernel's writeback grid for `blocks` whole blocks, skewed by
/// `skew` bytes: a first short piece `[0, skew)`, then `SEG`-long
/// segments from `skew` (every fourth STRADDLES a block boundary), the
/// last one clipped at the file end. `skew = 0` is the aligned grid.
fn grid(blocks: u64, skew: u64) -> Vec<Seg> {
    let end = blocks * BS;
    let mut v = Vec::new();
    if skew > 0 {
        v.push((0, skew));
    }
    let mut off = skew;
    while off < end {
        let len = SEG.min(end - off);
        v.push((off, len));
        off += len;
    }
    v
}

/// The delivery order the class needs: every segment that lies INSIDE
/// one block first (the inner three per block), then every segment
/// that straddles a boundary and the short head piece — the 256-deep
/// queue's reordering, made deterministic.
fn inner_first(segs: &[Seg]) -> Vec<Seg> {
    let inner = |&(off, len): &Seg| off / BS == (off + len - 1) / BS && len == SEG;
    let mut out: Vec<Seg> = segs.iter().copied().filter(inner).collect();
    out.extend(segs.iter().copied().filter(|s| !inner(s)));
    out
}

async fn deliver(h: &H, ino: u64, data: &[u8], order: &[Seg]) {
    for &(off, len) in order {
        write_at(h, ino, off, &data[off as usize..(off + len) as usize]).await;
    }
}

/// The law: `blocks` blocks of a page-aligned stream land ONCE each —
/// the fsync flush finds no partial buffer (no seed read, no whole-block
/// re-upload, no flush-time seed materialized, no overlay teardown
/// waited on), and the device bytes match the user bytes to within ONE
/// block: a fresh cache-less file's first segment lands through the
/// router's striped route (no byte face on this ledger) and block 0's
/// remaining segments overwrite it through the overlay (one merge of
/// the first segment's old image) — a per-file constant the class would
/// dwarf by `blocks` whole blocks.
fn assert_landed_once(label: &str, before: Snap, after: Snap, blocks: u64) {
    let seed = delta!(after, before, flush_seed_read_bytes);
    let re_upload = delta!(after, before, durable_upload_escalation);
    let materialized = delta!(after, before, overwrite_seed_materialized);
    let waits = delta!(after, before, overlay_teardown_waits);
    let dev = device_write_bytes(after, before);
    let user = blocks * BS;
    assert_eq!(
        seed,
        0,
        "{label}: the fsync flush re-read {} block(s) from the device ({seed} B) — a partial \
         active buffer beside an overlay record (§4.4ci); re-uploaded {re_upload} B, \
         seeds materialized {materialized}, teardown waits {waits}",
        seed / BS
    );
    assert_eq!(
        re_upload, 0,
        "{label}: the fsync re-uploaded whole blocks (§4.4ci)"
    );
    assert_eq!(
        materialized, 0,
        "{label}: flush-time seeds materialized (§4.4ci)"
    );
    assert_eq!(
        waits, 0,
        "{label}: the flush waited on an overlay teardown (§4.4ci)"
    );
    assert!(
        dev.abs_diff(user) <= BS,
        "{label}: device bytes {dev} vs user bytes {user} (amplification {:.3}×) — more than \
         the first block's constant apart",
        dev as f64 / user as f64
    );
}

async fn run(label: &str, skew: u64, reorder: bool) {
    let _g = serial().await;
    let (h, _lever) = make(label).await;
    let ino = create(&h, &format!("{label}.bin")).await;
    let data = pattern((BLOCKS * BS) as usize, 0xC1);
    let segs = grid(BLOCKS, skew);
    let order = if reorder {
        inner_first(&segs)
    } else {
        segs.clone()
    };
    assert_eq!(
        order.iter().map(|s| s.1).sum::<u64>(),
        BLOCKS * BS,
        "harness premise: the grid covers the file exactly once"
    );

    let before = snap();
    deliver(&h, ino, &data, &order).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    let after = snap();

    // The data law first — never a routing verdict over wrong bytes.
    let back = read_at(&h, ino, 0, data.len()).await;
    assert!(
        back == data,
        "{label}: read-back differs from the written stream"
    );

    eprintln!(
        "{label} ledger: publishes {} stores {} store_bytes {} gaps {} waits {} wt {} wt_bytes {} \
         seed_read {} materialized {} escalation {} other_uploads {} patch {} folds {}",
        delta!(after, before, overlay_publishes),
        delta!(after, before, overlay_stores),
        delta!(after, before, overlay_store_bytes),
        delta!(after, before, overlay_gap_seed_bytes),
        delta!(after, before, overlay_teardown_waits),
        delta!(after, before, write_through_blocks),
        delta!(after, before, write_through_bytes),
        delta!(after, before, flush_seed_read_bytes),
        delta!(after, before, overwrite_seed_materialized),
        delta!(after, before, durable_upload_escalation),
        delta!(after, before, durable_upload_other),
        delta!(after, before, patch_write_bytes),
        delta!(after, before, fold_passes),
    );
    assert_landed_once(label, before, after, BLOCKS);
}

/// The cloud row's shape (§3.11 / §4.4ci): a one-page-skewed grid whose
/// straddling segments arrive after the inner ones. RED on the base:
/// every block re-read and re-uploaded at fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skewed_grid_delivered_out_of_order_lands_every_block_once() {
    run("skewed_ooo", PAGE, true).await;
}

/// Control: the same skewed grid delivered in order — every block's
/// first piece opens an active buffer, the rest join it, write-through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skewed_grid_delivered_in_order_lands_every_block_once() {
    run("skewed_inorder", PAGE, false).await;
}

/// Control: the aligned grid, in order — every block rides the overlay
/// whole (the laptop's shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aligned_grid_lands_every_block_once() {
    run("aligned", 0, false).await;
}
