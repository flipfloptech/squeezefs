//! **Finding 46 — the kvmap streaming-extend publish collapse** (E2E
//! performance audit baseline on squeeze-test, 2026-09-02): 24 fio jobs
//! × 8 GiB sequential 1 MiB direct writes opened at 28.7 GB/s, every file
//! crossed into the block-map tree at t+7 s, and bandwidth fell ~2,000×
//! to ~0.36 GiB/s for the rest of the row (clat 986 ms per 1 MiB write).
//! The REWRITE pass over the same kvmap-headed files ran 32 GiB/s.
//!
//! Attribution from the row's stats deltas: ≈2,830 post-crossing
//! publishes paid 13,037 `block_map_range` pages (≈4.6 pages ≈ the ino's
//! WHOLE ~1,500-record population per publish) and ~275 ms each — the
//! Rev 1.3 #2 whole-map delete-by-absence diff running on EVERY
//! steady-state save of a whole-map kvmap head, its O(map) CPU (RAM-map
//! clone + record encode + tree page + decode) landing on the 2-thread
//! `sqz-meta` pool the publish pass runs on, so 24 inos' publishes
//! serialized across inos as well as within.
//!
//! THE LAW (design §3 Publish / §14 S2 / §16): a steady-state publish of
//! a kvmap-headed ino ships ONLY its window — tree operations
//! proportional to the blocks published, never to the file. The
//! whole-map diff is legitimate at the CROSSING (the tree is empty), at
//! re-canonicalization, and on the non-publish saves that own deletes
//! (truncate/punch/fsync-persist) — never per streaming publish.
//!
//! The venue is the f44/f45 FUSE-level shape: live-default format bits,
//! real write handlers + the publish conveyor, whole-block sequential
//! writes, the field's device-overlay posture (ON). The instrument is
//! the tree-7 read ledger: `meta_kv_block_map_{lookup_exact,
//! lookup_floor,range_records}` deltas per publish.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_KV_BLOCK_MAP_LOOKUP_EXACT, META_KV_BLOCK_MAP_LOOKUP_FLOOR, META_KV_BLOCK_MAP_LOOKUP_RANGE,
    META_KV_BLOCK_MAP_RANGE_RECORDS,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;
const DATA_VOL_ID: &str = "vol-00000000000000f6";
/// The steady-state window under test: whole blocks appended AFTER the
/// crossing, one FUSE write each (the field's one-publish-per-block
/// shape).
const STREAM_BLOCKS: u64 = 256;

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
    routed: Arc<RoutedMetaBackend>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The f44 fixture shape: live-DEFAULT format (no bit surgery — the f43
/// self-arm ratchet is the crossing's own act), staging dir present, the
/// shipped device-overlay posture (ON, ACK-early), whole-block writes.
async fn mount_live_shape() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    squeezefs::device_overlay::set_ack_early_for_tests(true, true);
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    // Backing under target/ — tmpfs refuses the O_DIRECT open the
    // overlay's zc_write_fd screen requires (the f44 rule).
    let b = tempfile::Builder::new()
        .prefix("f46-b")
        .tempfile_in(dir)
        .unwrap();
    b.as_file().set_len(256 * 1024 * 1024).unwrap();
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();

    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    let s = tempdir().unwrap();
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
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    format_v3(
        m.path(),
        128 * 1024 * 1024,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let routed: Arc<RoutedMetaBackend> = {
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
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
        routed,
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

fn pattern(block: u64) -> Vec<u8> {
    (0..BS as usize)
        .map(|i| ((i as u64 + block * 7) % 251) as u8 | 1)
        .collect()
}

async fn write_block(h: &H, ino: u64, block: u64) {
    let data = pattern(block);
    let len = data.len();
    let w =
        h.fs.write(h.req, ino, 0, block * BS, bytes::Bytes::from(data), 0, 0)
            .await
            .unwrap_or_else(|e| panic!("write block {block}: {e:?}"));
    assert_eq!(w.written as usize, len, "short write at block {block}");
}

async fn read_block(h: &H, ino: u64, block: u64) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, block * BS, BS as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read block {block}: {e:?}"))
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
}

async fn durable_head_id(h: &H, ino: u64) -> Option<String> {
    let bytes = h
        .routed
        .getxattr(ino, "layout")
        .await
        .expect("layout read")?;
    squeezefs::layout_wire::decode_layout_any(&bytes)
        .ok()
        .and_then(|l| l.block_map_id)
}

async fn is_kvmap_head(h: &H, ino: u64) -> bool {
    durable_head_id(h, ino)
        .await
        .is_some_and(|id| id.starts_with("kvmap:"))
}

/// INTERLEAVED sequential whole-block writes on two inos until BOTH
/// durable heads are kvmap heads (the f45 beat: an fsync every 64 blocks
/// flushes the dirty span so the publish — hence the crossing decision —
/// runs). Returns the per-ino block count at which the crossings were
/// OBSERVED.
///
/// Two writers, not one: a lone sequential writer's allocations are
/// stride-contiguous with consecutive stamps, so PR 6a's run seam
/// collapses its whole map into ONE run record and the tree-read face of
/// the whole-map diff vanishes (one record per page). The field row had
/// 24 concurrent writers interleaving one allocator — ~1,500 POINT
/// records per ino at the crossing, 98 runs across 38,582 records —
/// which is what alternating two inos over one allocator reproduces.
async fn write_until_both_crossed(h: &H, inos: [u64; 2]) -> u64 {
    let mut blocks = 0u64;
    loop {
        for ino in inos {
            write_block(h, ino, blocks).await;
        }
        blocks += 1;
        if blocks.is_multiple_of(64) {
            for ino in inos {
                fsync(h, ino).await;
            }
        }
        if blocks.is_multiple_of(16)
            && is_kvmap_head(h, inos[0]).await
            && is_kvmap_head(h, inos[1]).await
        {
            return blocks;
        }
        assert!(
            blocks < 2048,
            "fixture: the inos never crossed into the block-map tree"
        );
    }
}

/// The ino's durable map census: distinct records and the indices they
/// COVER (a run record covers `run_len` indices — the record count alone
/// under-reads a coalesced map).
async fn tree_census(h: &H, ino: u64) -> (u64, u64) {
    let page = h.routed.volumes[0]
        .block_map_range(ino, 0, 1 << 20)
        .await
        .expect("record census");
    let covered: u64 = page.iter().map(|(_, e)| u64::from(e.run_len())).sum();
    (page.len() as u64, covered)
}

/// The tree-7 read ledger — every tree operation a publish can pay:
/// exact lookups, floor probes, and the RECORDS paged by range scans
/// (a page count would hide the page width; the field's 4.6 pages per
/// publish were the ino's whole population).
#[derive(Clone, Copy, Debug)]
struct Ledger {
    exact: u64,
    floor: u64,
    range_pages: u64,
    range_records: u64,
    publishes: u64,
    published_blocks: u64,
    window_saves: u64,
}

fn ledger() -> Ledger {
    Ledger {
        exact: META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed),
        floor: META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed),
        range_pages: META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed),
        range_records: META_KV_BLOCK_MAP_RANGE_RECORDS.load(Ordering::Relaxed),
        publishes: METRICS.layout_publish_batches.load(Ordering::Relaxed),
        published_blocks: METRICS
            .layout_publish_batched_blocks
            .load(Ordering::Relaxed),
        window_saves: METRICS.kvmap_window_saves.load(Ordering::Relaxed),
    }
}

fn delta(a: Ledger, b: Ledger) -> Ledger {
    Ledger {
        exact: b.exact - a.exact,
        floor: b.floor - a.floor,
        range_pages: b.range_pages - a.range_pages,
        range_records: b.range_records - a.range_records,
        publishes: b.publishes - a.publishes,
        published_blocks: b.published_blocks - a.published_blocks,
        window_saves: b.window_saves - a.window_saves,
    }
}

fn tree_reads(d: Ledger) -> u64 {
    d.exact + d.floor + d.range_records
}

// ===========================================================================
// Finding 46: a whole-map kvmap ino under sequential EXTEND writes publishes
// in O(window) tree operations — never O(map)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streaming_extend_on_a_kvmap_ino_publishes_in_o_window_tree_reads() {
    let _g = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = mount_live_shape().await;
    let ino = create(&h, "f46.bin").await;
    let sibling = create(&h, "f46-sibling.bin").await;

    let crossed_at = write_until_both_crossed(&h, [ino, sibling]).await;
    for i in [ino, sibling] {
        assert!(
            !h.fs.router.kvmap_partial_mode(i),
            "fixture: under the real budget the crossing keeps whole-map RAM — the \
             whole-map steady-state save is the arm under test"
        );
    }
    // Settle the crossings' own tails (their publishes + the fsync
    // persists) so the steady-state window starts from a clean ledger.
    fsync(&h, ino).await;
    fsync(&h, sibling).await;
    let (records_at_cross, covered_at_cross) = tree_census(&h, ino).await;
    assert!(
        records_at_cross >= crossed_at / 2,
        "fixture: the interleaved allocations must leave a POINT-dominated map \
         ({records_at_cross} records covering {covered_at_cross} indices at the \
         crossing) — a run-collapsed map has no whole-map tree-read face to measure"
    );

    // THE FIELD SHAPE: sequential whole-block extend writes on the
    // now-kvmap-headed inos, interleaved, no fsync inside the window (fio
    // never fsynced; the publishes here are the overlay settle's own).
    // The ledger is process-global, so the window's reads cover BOTH
    // inos' publishes — the bound below is per published block either
    // way.
    let l0 = ledger();
    for b in crossed_at..crossed_at + STREAM_BLOCKS {
        write_block(&h, ino, b).await;
        write_block(&h, sibling, b).await;
    }
    let l1 = ledger();
    // The writeback beat AFTER the window: flushes any block still parked
    // in the overlay and persists the dirty heads — NON-publish saves,
    // measured separately (a legitimate whole-map arm).
    fsync(&h, ino).await;
    fsync(&h, sibling).await;
    let l2 = ledger();
    let stream = delta(l0, l1);
    let flush = delta(l1, l2);
    let (records_now, covered_now) = tree_census(&h, ino).await;
    eprintln!(
        "[f46] crossed at {crossed_at} blocks/ino ({records_at_cross} records covering \
         {covered_at_cross}); stream window {STREAM_BLOCKS} blocks × 2 inos: {stream:?}; \
         post-window fsyncs: {flush:?}; ino {ino} now {records_now} records covering \
         {covered_now}"
    );

    // Row validity: the window PUBLISHED (a green with zero publishes
    // would be a silent passthrough).
    assert!(
        stream.publishes >= 2,
        "fixture: the streaming window produced no publishes — nothing measured"
    );
    assert!(
        stream.published_blocks >= STREAM_BLOCKS,
        "fixture: the window published only {} of {} blocks",
        stream.published_blocks,
        2 * STREAM_BLOCKS
    );

    // THE LAW: tree reads over the window are proportional to the blocks
    // published (≤ 2 per published block + 4 per publish for head/gen
    // probes) — NOT to the inos' record populations. On the finding-46
    // tip every publish pages the ino's whole map: reads ≈ publishes ×
    // records (≥ records_at_cross per publish).
    let bound = 2 * stream.published_blocks + 4 * stream.publishes;
    assert!(
        tree_reads(stream) <= bound,
        "FINDING 46: the steady-state publishes of whole-map kvmap inos paid \
         {} tree reads ({} exact, {} floor, {} range records over {} pages) for \
         {} publishes / {} blocks — O(map) per publish (ino {ino} held \
         {records_at_cross} records at the crossing); the law is O(window): ≤ {bound}",
        tree_reads(stream),
        stream.exact,
        stream.floor,
        stream.range_records,
        stream.range_pages,
        stream.publishes,
        stream.published_blocks,
    );
    // The engagement face: every steady-state publish rode the WINDOW
    // arm (`kvmap_window_saves` is the row-validity gauge for the field
    // verification — a green here with the gauge flat would be a
    // whole-map save that happened to read cheaply).
    assert_eq!(
        stream.window_saves, stream.publishes,
        "every steady-state publish must account as a window save"
    );

    // Content beside economy: the window's first and last blocks read
    // back exactly on both inos (the durable map names every published
    // block).
    for i in [ino, sibling] {
        assert_eq!(
            read_block(&h, i, crossed_at).await,
            pattern(crossed_at),
            "ino {i}: first window block reads back"
        );
        let last = crossed_at + STREAM_BLOCKS - 1;
        assert_eq!(
            read_block(&h, i, last).await,
            pattern(last),
            "ino {i}: last window block reads back"
        );
    }
    // And the durable tree covers every block written (the window's
    // records landed — never a silent no-op publish).
    assert_eq!(
        covered_now,
        crossed_at + STREAM_BLOCKS,
        "ino {ino}: the tree covers {covered_now} indices for {} written blocks",
        crossed_at + STREAM_BLOCKS
    );
}

// ===========================================================================
// The window arm composes with the arms that keep the whole-map diff: a
// REWRITE (the rewrite program's shadow-epoch swap) and a TRUNCATE (the
// non-publish save that owns deletes) both run against a tree the window
// saves left ≡ RAM, and an extend after them rides the window again
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_saves_compose_with_the_rewrite_and_truncate_arms() {
    let _g = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    let h = mount_live_shape().await;
    let ino = create(&h, "f46-rw.bin").await;
    let sibling = create(&h, "f46-rw-sibling.bin").await;
    let crossed_at = write_until_both_crossed(&h, [ino, sibling]).await;
    fsync(&h, ino).await;
    fsync(&h, sibling).await;
    // Extend through the window arm first, so the arms below run against
    // a tree the WINDOW saves produced.
    let l0 = ledger();
    for b in crossed_at..crossed_at + 64 {
        write_block(&h, ino, b).await;
    }
    let ext = delta(l0, ledger());
    assert!(
        ext.window_saves >= 1 && ext.window_saves == ext.publishes && ext.range_records == 0,
        "the extend rides the window arm: {ext:?}"
    );
    fsync(&h, ino).await;
    let size_blocks = crossed_at + 64;
    let (_, covered0) = tree_census(&h, ino).await;
    assert_eq!(covered0, size_blocks, "the tree covers the extended map");

    // A rewrite pass over 32 blocks in the middle of the map (the f44
    // shape): the rewrite program's shadow-epoch swap owns this vehicle
    // (its publish is the epoch close's, never a window save) — the law
    // here is that it composes onto the window-built tree: coverage
    // unchanged, the rewritten bytes served, no duplicate records.
    let lo = crossed_at / 4;
    let rewritten: Vec<u64> = (lo..lo + 32).collect();
    for &b in &rewritten {
        let data: Vec<u8> = pattern(b).into_iter().map(|x| x ^ 0x55 | 1).collect();
        let len = data.len();
        let w =
            h.fs.write(h.req, ino, 0, b * BS, bytes::Bytes::from(data), 0, 0)
                .await
                .unwrap_or_else(|e| panic!("rewrite block {b}: {e:?}"));
        assert_eq!(w.written as usize, len);
    }
    fsync(&h, ino).await;
    let (records1, covered1) = tree_census(&h, ino).await;
    assert_eq!(
        covered1, size_blocks,
        "a same-index rewrite supersedes — coverage unchanged ({records1} records)"
    );
    for &b in &rewritten {
        let want: Vec<u8> = pattern(b).into_iter().map(|x| x ^ 0x55 | 1).collect();
        assert_eq!(read_block(&h, ino, b).await, want, "rewritten block {b}");
    }

    // The delete law: a truncate to half the file removes the tail's
    // records — the NON-publish save's whole-map diff (delete-by-absence)
    // is what owns that; the window arm's release set is empty by law.
    let keep = size_blocks / 2;
    let l0 = ledger();
    let attr =
        h.fs.setattr(
            h.req,
            ino,
            None,
            fuse3::SetAttr {
                size: Some(keep * BS),
                ..Default::default()
            },
        )
        .await
        .expect("truncate");
    assert_eq!(attr.attr.size, keep * BS);
    fsync(&h, ino).await;
    let tr = delta(l0, ledger());
    assert_eq!(
        tr.window_saves, 0,
        "a truncate never rides the window arm (it owns deletes): {tr:?}"
    );
    let (_, covered2) = tree_census(&h, ino).await;
    assert_eq!(
        covered2, keep,
        "the truncate's save removed the tail records (tree covers {covered2}, kept {keep})"
    );
    // And an EXTEND after the truncate publishes windows again, onto the
    // shrunk map, with the reads exact.
    let l0 = ledger();
    for b in keep..keep + 16 {
        write_block(&h, ino, b).await;
    }
    let ext = delta(l0, ledger());
    assert!(
        ext.window_saves >= 1 && ext.range_records == 0,
        "the post-truncate extend rides the window arm again: {ext:?}"
    );
    fsync(&h, ino).await;
    let (_, covered3) = tree_census(&h, ino).await;
    assert_eq!(covered3, keep + 16, "the tree covers the re-extended map");
    assert_eq!(read_block(&h, ino, keep + 15).await, pattern(keep + 15));
    assert_eq!(read_block(&h, ino, keep - 1).await, pattern(keep - 1));
}
