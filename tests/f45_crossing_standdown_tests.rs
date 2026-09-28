//! **Finding 45 — the crossing stand-down regression** (PR 6c-i live
//! smoke, 2026-09-02): on a REAL mount (default-format bits, staging
//! dirs, the FUSE write handlers + publish conveyor), 3 GiB fio files
//! whose inline map sized past the xattr cap took the LEGACY blob arm —
//! zero kvmap engagement, `meta_kv_block_map_*` all zero after remount —
//! while the in-process 6c-i contracts (direct `merge_block_mappings`,
//! pre-stamped bit 16) stayed green.
//!
//! THE LAW (design §14/§16): `needs_indirect` + kvmap-eligible ⇒ the
//! TREE, at ANY size — an over-threshold ino's crossing still CROSSES,
//! into PARTIAL mode (the crossing train is chunked; its one whole-map
//! input is the RAM map the ino already grew up with — O(existing) once,
//! then the store flips partial and the RAM map releases). This venue
//! reproduces the LIVE shape end-to-end: real FUSE writes through the
//! conveyor on a live-default format, with the budget arithmetic forced
//! down so the ino reads OVER budget at the crossing decision.

use fuse3::raw::{Filesystem, Request};
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use tempfile::{tempdir, NamedTempFile, TempDir};

const DATA_VOL_ID: &str = "vol-00000000000000f5";
const BS: u64 = 65536;

// ---------------------------------------------------------------------------
// Serialization (process-global METRICS deltas + mem-budget + env mutation)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

// ---------------------------------------------------------------------------
// Log capture (the f45 observability contract): finding 45's live evidence
// was a GREP over the session log for "kvmap" — the crossing's only
// engagement line said "block-map tree", so a fully-engaged run read as a
// stand-down. The pin: engagement must be greppable by the ladder's own
// name.
// ---------------------------------------------------------------------------

static LOG_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CaptureLog;

impl log::Log for CaptureLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            LOG_LINES
                .lock()
                .unwrap()
                .push(format!("{} {}", r.level(), r.args()));
        }
    }
    fn flush(&self) {}
}

static CAPTURE: CaptureLog = CaptureLog;

fn arm_log_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log::set_logger(&CAPTURE).expect("first logger");
        log::set_max_level(log::LevelFilter::Info);
    });
    LOG_LINES.lock().unwrap().clear();
}

fn captured_kvmap_lines() -> Vec<String> {
    LOG_LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.to_ascii_lowercase().contains("kvmap"))
        .cloned()
        .collect()
}

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

/// 64 KiB flag budget ⇒ `kvmap_write_map_budget_bytes()` ≤ 4 KiB ⇒ any
/// map past 64 entries reads OVER budget (the live 3-GiB-class shape,
/// scaled into the fixture). Restores on drop.
struct BudgetGuard;
impl Drop for BudgetGuard {
    fn drop(&mut self) {
        squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(0);
        squeezefs::mem_budget::MEM_BUDGET.tick();
    }
}

fn shrink_budget() -> BudgetGuard {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(64 * 1024);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    assert!(squeezefs::routing::kvmap_write_map_budget_bytes() <= 4 * 1024);
    BudgetGuard
}

// ---------------------------------------------------------------------------
// The LIVE-shape FUSE fixture: live-DEFAULT format (whatever `format_v3`
// stamps — bit 16 is NOT pre-stamped; the f43 self-arm ratchet is the
// crossing's own act, exactly as on a real mount), staging dirs present,
// real write handlers + publish conveyor.
// ---------------------------------------------------------------------------

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    routed: Arc<RoutedMetaBackend>,
    alloc: Arc<BlockAllocator>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _staging: TempDir,
}

async fn mount_live_shape() -> H {
    arm_log_capture();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("256MB"),
        Some("256MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    // The LIVE default format: exactly what `squeezefs format` stamps —
    // no test-side bit surgery (the deviation that hid f45: every 6c-i
    // fixture pre-stamped bit 16, so the ratchet never ran in-process).
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
        alloc: ba,
        _b: b,
        _m: m,
        _staging: staging,
    }
}

async fn fuse_create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn fuse_write(h: &H, ino: u64, off: u64, data: &[u8]) {
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn fuse_read(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
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

/// Drive the live venue's write half: sequential whole-block writes until
/// the inline map sizes past the cap and the ino crosses — or the bound
/// says it never will.
async fn write_until_crossed(h: &H, ino: u64) -> (bool, u64) {
    let chunk = vec![b'f'; 8 * BS as usize];
    let mut blocks = 0u64;
    loop {
        fuse_write(h, ino, blocks * BS, &chunk).await;
        blocks += 8;
        // The live venue's writeback beat: fio + the kernel flush the
        // dirty span; without it the fixture parks in accumulation and
        // no layout publish (hence no crossing decision) ever runs.
        if blocks.is_multiple_of(32) {
            h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
        }
        let head = durable_head_id(h, ino).await;
        match head.as_deref() {
            Some(id) if id.starts_with("kvmap:") => return (true, blocks),
            _ if blocks >= 1536 => return (false, blocks),
            _ => {}
        }
    }
}

// ===========================================================================
// Finding 45: the live shape — an OVER-BUDGET ino's crossing must still
// CROSS (into partial mode), never regress to the blob plane
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_budget_crossing_still_crosses_into_partial_mode() {
    let _serial = serial();
    let _budget = shrink_budget();
    let h = mount_live_shape().await;
    let ino = fuse_create(&h, "f45.bin").await;

    let migrated0 = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    let (crossed, blocks) = write_until_crossed(&h, ino).await;
    assert!(
        crossed,
        "FINDING 45: the over-budget crossing stood down to the blob plane — \
         after {blocks} blocks the head is {:?}, not kvmap:1 (design §14/§16: \
         needs_indirect + kvmap-eligible ⇒ the TREE at ANY size, mode chosen \
         by the threshold)",
        durable_head_id(&h, ino).await
    );
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed) - migrated0,
        1,
        "exactly one crossing engaged"
    );
    // The f45 observability law: the crossing announces itself GREPPABLY
    // — one INFO line carrying the ladder's own name ("kvmap"), the ino,
    // and the landed mode. Finding 45's live evidence was `grep kvmap
    // session1.log` reading 0 over a FULLY-ENGAGED session (the only
    // engagement line said "block-map tree"), which made a correct run
    // indistinguishable from the feared blob-plane regression.
    let lines = captured_kvmap_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("crossing") && l.contains(&format!("ino {ino}"))),
        "FINDING 45 (observability half): the crossing must log a greppable          'kvmap' line naming the ino — captured kvmap lines: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("ino {ino}")) && l.contains("partial")),
        "the crossing line names the landed MODE (partial here): {lines:?}"
    );

    // The mode the threshold chose: over budget ⇒ PARTIAL, RAM map
    // released (the §16 flip point runs on the crossing's own republish).
    assert!(
        h.fs.router.kvmap_partial_mode(ino),
        "the over-budget crossing lands in PARTIAL mode"
    );
    let entry =
        h.fs.router
            .metadata_cache
            .peek_with(&ino, |m| m.block_map.is_some());
    assert_eq!(
        entry,
        Some(false),
        "the RAM map RELEASED at the flip (the §14 S1 boundedness)"
    );

    // The content law holds through the partial store: first and last
    // written blocks read back exactly (fsync first — the live venue's
    // verify runs post-flush).
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert_eq!(
        fuse_read(&h, ino, 0, BS as u32).await,
        vec![b'f'; BS as usize],
        "block 0 reads through the partial store"
    );
    assert_eq!(
        fuse_read(&h, ino, (blocks - 1) * BS, BS as u32).await,
        vec![b'f'; BS as usize],
        "the last written block reads through the partial store"
    );
}

// ===========================================================================
// The under-budget control: the same venue with the REAL budget crosses
// whole-map (the pre-6c-i posture, live-default bits — the f43 self-arm
// ratchet exercised in-process for the first time on this suite)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_under_budget_crossing_crosses_whole_map_on_live_default_bits() {
    let _serial = serial();
    let h = mount_live_shape().await;
    let ino = fuse_create(&h, "f45-control.bin").await;

    let migrated0 = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    let (crossed, blocks) = write_until_crossed(&h, ino).await;
    assert!(
        crossed,
        "the live-default-format crossing engages (the f43 self-arm ratchet) — \
         head after {blocks} blocks: {:?}",
        durable_head_id(&h, ino).await
    );
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed) - migrated0,
        1
    );
    assert!(
        !h.fs.router.kvmap_partial_mode(ino),
        "under the real budget the crossing keeps whole-map RAM (byte-identity)"
    );
    // The observability law, whole-map face.
    let lines = captured_kvmap_lines();
    assert!(
        lines.iter().any(|l| l.contains("crossing")
            && l.contains(&format!("ino {ino}"))
            && l.contains("whole-map")),
        "the crossing line names the ino and the whole-map mode: {lines:?}"
    );
    assert_eq!(
        fuse_read(&h, ino, (blocks - 1) * BS, BS as u32).await,
        vec![b'f'; BS as usize],
        "pre-fsync last block (whole-map control)"
    );
    assert_eq!(
        fuse_read(&h, ino, 0, BS as u32).await,
        vec![b'f'; BS as usize]
    );
}

// ===========================================================================
// Record §7 item 24, review round 2 (Issue 19): the PARTIAL store's hole
// under exhaustion — the one striped shape whose map the write path's
// RAM probe cannot read
// ===========================================================================

/// A full set refuses a write that would CREATE custody of a block nothing
/// can allocate BEFORE it acks it (record §7 item 24). The RAM-only probe
/// reads the entry's inline map; a PARTIAL-store ino carries none, so the
/// first build read every block inside such a file's size as HELD and
/// both a hole and a rewrite there kept the RAM sink verbatim. The
/// partial store has NO in-place arm — `try_inplace_rewrite` and the W1
/// patch resolve the mapping from the RAM map alone (the first version of
/// this pin asked the brim rewrite to land on such a file and read
/// `ENOSPC` at its fsync: the write had been ACKED into the sink) — so a
/// tree-mapped block's rewrite CoWs exactly as a hole allocates: on an
/// exhausted set BOTH are refused `ENOSPC` before the ack, nothing acked,
/// nothing parked, the file's bytes intact, its fsync clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hole_and_a_rewrite_inside_a_partial_store_file_are_refused_when_the_set_is_exhausted() {
    let _serial = serial();
    let _budget = shrink_budget();
    let h = mount_live_shape().await;
    let ino = fuse_create(&h, "f45-item24.bin").await;
    let (crossed, blocks) = write_until_crossed(&h, ino).await;
    assert!(crossed, "premise: the ino crossed into the tree");
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.router.kvmap_partial_mode(ino),
        "premise: the over-budget crossing landed in PARTIAL mode"
    );
    assert_eq!(
        h.fs.router
            .metadata_cache
            .peek_with(&ino, |m| m.block_map.is_some()),
        Some(false),
        "premise: no inline map in RAM — the probe's blind shape"
    );

    // Grow the size by four blocks with no write: blocks `blocks..blocks+4`
    // are holes INSIDE the file's size.
    let grown = (blocks + 4) * BS;
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(grown),
            ..Default::default()
        },
    )
    .await
    .expect("truncate grows the size with no blocks");

    // Exhaust the store exactly where it stands: cap the allocator at its
    // dense frontier and drain whatever the free list holds — the next
    // fresh mint is the terminal `StorageFull` that latches the volume.
    h.alloc
        .set_capacity_bytes(h.alloc.highest_block_index() * h.alloc.chunk_size());
    let mut drained = 0u32;
    while h.alloc.allocate_block().await.is_ok() {
        drained += 1;
        assert!(
            drained < 10_000,
            "the free list never drained under the cap"
        );
    }
    assert!(
        h.alloc.fresh_supply_exhausted(),
        "premise: latched by the terminal refusal"
    );

    let refused0 = METRICS
        .write_fresh_block_enospc_refusals
        .load(Ordering::Relaxed);
    let parked0 = SqueezefsFilesystem::parked_gauge_bytes();
    // The hole: no custody of any kind — fresh, refused before the ack.
    let hole = blocks + 1;
    let r =
        h.fs.write(
            h.req,
            ino,
            0,
            hole * BS,
            bytes::Bytes::from(vec![b'h'; BS as usize]),
            0,
            0,
        )
        .await;
    assert_eq!(
        r.map(|w| w.written).map_err(|e| i32::from(e).abs()),
        Err(libc::ENOSPC),
        "a hole inside a partial-store file on an exhausted set is refused before the ack"
    );
    assert_eq!(
        METRICS
            .write_fresh_block_enospc_refusals
            .load(Ordering::Relaxed)
            - refused0,
        1
    );
    assert_eq!(
        SqueezefsFilesystem::parked_gauge_bytes(),
        parked0,
        "nothing parked"
    );
    assert_eq!(
        fuse_read(&h, ino, hole * BS, BS as u32).await,
        vec![0u8; BS as usize],
        "the hole still reads as zeros"
    );

    // A MAPPED block of the same file: no in-place arm exists for the
    // partial store, so the rewrite would CoW into a block nothing can
    // allocate — refused like the hole, the original bytes untouched.
    let r =
        h.fs.write(
            h.req,
            ino,
            0,
            0,
            bytes::Bytes::from(vec![b'r'; BS as usize]),
            0,
            0,
        )
        .await;
    assert_eq!(
        r.map(|w| w.written).map_err(|e| i32::from(e).abs()),
        Err(libc::ENOSPC),
        "a partial-store file's mapped block has no in-place arm: its rewrite is refused too"
    );
    assert_eq!(
        METRICS
            .write_fresh_block_enospc_refusals
            .load(Ordering::Relaxed)
            - refused0,
        2
    );
    assert_eq!(SqueezefsFilesystem::parked_gauge_bytes(), parked0);
    h.fs.fsync(h.req, ino, 0, false)
        .await
        .expect("nothing was acked that cannot land: the fsync is clean");
    assert_eq!(
        fuse_read(&h, ino, 0, BS as u32).await,
        vec![b'f'; BS as usize],
        "the mapped block's bytes stand"
    );
    assert_eq!(
        fuse_read(&h, ino, (blocks - 1) * BS, BS as u32).await,
        vec![b'f'; BS as usize]
    );
}
