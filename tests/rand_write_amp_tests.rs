//! PR RW1 of docs/design-random-small-writes.md: the rand-write attribution
//! rig + the standing-RED device-byte amplification gate.
//!
//! Contracts under test (rig ARMED — every test in this binary resolves the
//! memoized `SQUEEZEFS_OP_PROFILE` gate ON before any op; the disabled-cost
//! twin lives in `tests/rand_write_rig_off_tests.rs`):
//!
//! - **The §1.2 device-byte LEDGER** (always-on counters): every device byte
//!   the rand-small-write shape moves has an attributed bucket keyed to the
//!   CORRECTED leg drivers (review Issue 4) — the inline victim spill
//!   (`fetch_seed_image` + `put_active_block` inside
//!   `insert_active_block_buffer`), the R5-pressure parked drain
//!   (`drain_parked_toward`), the fsync/close flush family, and same-key
//!   re-stage churn — and NEVER the foreground writeback queue
//!   (`writeback_enqueued_wt_fallback == 0` on the shape).
//! - **The G-RW2 gate** (shipped standing-RED by RW1, **flipped green by
//!   PR RW2's W1 sole-owner extent patch**, now per-commit): device write
//!   bytes ≤ 4× user bytes, device read bytes ≤ 1× user bytes on the
//!   aligned rand-write shape, plus the decision-ledger tripwires
//!   (`patch_writes == ops`, `patch_edge_rmw_reads == 0`). The
//!   accumulation-pipeline pins below run with the patch knob at 0 and
//!   keep documenting the §1.2 RMW arithmetic every patch-ineligible
//!   shape still pays.
//! - **The mapping-form sanity line** (review Issue 19, permanent): the
//!   fixture's freshly-striped block map carries the undecorated 2-part
//!   whole-block form (`persist_block_key`'s bare offset strings) — printed
//!   verbatim and pinned, so the `exact`-flag polarity inversion class can
//!   never silently return.
//! - **The staged-sibling probe cost pin** (H1 baseline): every striped
//!   block checkout dispatches the `spawn_blocking` staged-sibling remove
//!   hop — probes == block writes. RW3's lock-free probe flips this pin
//!   when it elides the hop.
//! - **The rig itself**: write sub-phase histograms, per-site
//!   `BLOCK_FLUSH_LOCKS` wait attribution, the H2b stripe-collision audit
//!   (cross-key vs same-key classification), the in-flight WRITE histogram,
//!   and the armed-only stats-JSON surface.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    block_lock_acquire, block_lock_site_json, block_lock_stripe_audit_json, block_lock_try_note,
    op_profile_enabled, write_inflight_json, write_profile_phase_json, BlockLockSite,
    SqueezefsFilesystem, WriteInflight, BLOCK_FLUSH_LOCKS, METRICS, STATS_INODE,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{block_mapping_form, DataRouter};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Arm the M2/RW1 rig for this whole test binary BEFORE the memoized gate
/// first resolves (`--test-threads=1` is the suite convention; every test
/// calls this first so ordering never matters).
fn rig_on() {
    static ARM: OnceLock<()> = OnceLock::new();
    ARM.get_or_init(|| std::env::set_var("SQUEEZEFS_OP_PROFILE", "1"));
    assert!(
        op_profile_enabled(),
        "harness invariant: the rig must be armed before any profiled op"
    );
}

/// Ledger tests assert process-global METRICS deltas; serialize them (the
/// write_through_tests / writeback_tests pattern).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

/// Sandbox block size: the RMW anatomy is block-size-relative, so the
/// cargo-tier shape uses 64 KiB blocks (the scoreboard's is 4 MiB — same
/// pipeline, 64× the bytes). 4 KiB ops against 64 KiB blocks keep every
/// storm write strictly partial (never write-through).
const BS: u64 = 64 * 1024;
/// `MAX_ACTIVE_BLOCK_BUFFERS` (fuse_client, private const): the parked-map
/// cap whose saturation drives the inline victim spill. The storm dataset
/// must exceed it in DISTINCT blocks.
const PARKED_CAP: u64 = 256;

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Default W1 patch posture; accumulation-pipeline pins set 0 AFTER
    // their make() (never leaks forward).
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
    ]));
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 + seed as u64) % 251) as u8)
        .collect()
}

/// Authoritative backend meta (bypasses the RAM TTL cache).
async fn backend_meta(h: &H, ino: u64) -> squeezefs::routing::CachedMetadata {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    h.fs.router.fetch_metadata(&path).await.unwrap()
}

/// Make `ino` a striped file of `blocks` × [`BS`] via one whole-file write
/// (the fresh-file direct striped route — no staging leftovers, no parked
/// buffers; every block lands mapped in the undecorated whole-block form).
async fn make_striped(h: &H, ino: u64, blocks: u64, seed: u8) -> Vec<u8> {
    let p = pattern((blocks * BS) as usize, seed);
    write_at(h, ino, 0, &p).await;
    let meta = backend_meta(h, ino).await;
    assert_eq!(
        meta.file_type, "striped",
        "premise: fixture must be striped"
    );
    p
}

fn hist_total(hist: &serde_json::Value) -> u64 {
    hist.as_object()
        .expect("histogram object")
        .values()
        .map(|v| v.as_u64().unwrap_or(0))
        .sum()
}

fn phase_total(phase: &str) -> u64 {
    hist_total(&write_profile_phase_json()[phase])
}

fn site_total(site: &str) -> u64 {
    hist_total(&block_lock_site_json()[site])
}

/// Snapshot of the always-on RW1 ledger counters.
#[derive(Clone, Copy, Debug, Default)]
struct Ledger {
    spill_seed_reads: u64,
    spill_seed_read_bytes: u64,
    spill_staging_puts: u64,
    spill_staging_put_bytes: u64,
    flush_seed_read_bytes: u64,
    write_path_seed_read_bytes: u64,
    staging_put_bytes_drain: u64,
    staging_put_bytes_flush: u64,
    staging_put_bytes_teardown: u64,
    staging_put_bytes_wt_fallback: u64,
    writeback_enqueued_drain: u64,
    writeback_enqueued_flush: u64,
    writeback_enqueued_teardown: u64,
    writeback_enqueued_wt_fallback: u64,
    durable_upload_bytes_writeback: u64,
    durable_upload_bytes_self_flush: u64,
    durable_upload_bytes_escalation: u64,
    restage_churn_removes: u64,
    restage_churn_bytes: u64,
    write_block_revisits: u64,
    staging_sibling_probes: u64,
    write_through_bytes: u64,
    /// RW2 W1 patch legs — the in-place DMAs are DEVICE writes and the
    /// G-RW2 gate counts them; `edge_rmw_reads` must stay 0 (v1
    /// aligned-only — a gate clause, not slack the ≤1× read bound absorbs).
    patch_writes: u64,
    patch_write_bytes: u64,
    patch_edge_rmw_reads: u64,
}

fn ledger() -> Ledger {
    let l = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    Ledger {
        spill_seed_reads: l(&METRICS.spill_seed_reads),
        spill_seed_read_bytes: l(&METRICS.spill_seed_read_bytes),
        spill_staging_puts: l(&METRICS.spill_staging_puts),
        spill_staging_put_bytes: l(&METRICS.spill_staging_put_bytes),
        flush_seed_read_bytes: l(&METRICS.flush_seed_read_bytes),
        write_path_seed_read_bytes: l(&METRICS.write_path_seed_read_bytes),
        staging_put_bytes_drain: l(&METRICS.staging_put_bytes_drain),
        staging_put_bytes_flush: l(&METRICS.staging_put_bytes_flush),
        staging_put_bytes_teardown: l(&METRICS.staging_put_bytes_teardown),
        staging_put_bytes_wt_fallback: l(&METRICS.staging_put_bytes_wt_fallback),
        writeback_enqueued_drain: l(&METRICS.writeback_enqueued_drain),
        writeback_enqueued_flush: l(&METRICS.writeback_enqueued_flush),
        writeback_enqueued_teardown: l(&METRICS.writeback_enqueued_teardown),
        writeback_enqueued_wt_fallback: l(&METRICS.writeback_enqueued_wt_fallback),
        durable_upload_bytes_writeback: l(&METRICS.durable_upload_bytes_writeback),
        durable_upload_bytes_self_flush: l(&METRICS.durable_upload_bytes_self_flush),
        durable_upload_bytes_escalation: l(&METRICS.durable_upload_bytes_escalation),
        restage_churn_removes: l(&METRICS.restage_churn_removes),
        restage_churn_bytes: l(&METRICS.restage_churn_bytes),
        write_block_revisits: l(&METRICS.write_block_revisits),
        staging_sibling_probes: l(&METRICS.staging_sibling_probes),
        write_through_bytes: l(&METRICS.write_through_bytes),
        patch_writes: l(&METRICS.patch_writes),
        patch_write_bytes: l(&METRICS.patch_write_bytes),
        patch_edge_rmw_reads: l(&METRICS.patch_edge_rmw_reads),
    }
}

impl Ledger {
    fn delta(&self, before: &Ledger) -> Ledger {
        Ledger {
            spill_seed_reads: self.spill_seed_reads - before.spill_seed_reads,
            spill_seed_read_bytes: self.spill_seed_read_bytes - before.spill_seed_read_bytes,
            spill_staging_puts: self.spill_staging_puts - before.spill_staging_puts,
            spill_staging_put_bytes: self.spill_staging_put_bytes - before.spill_staging_put_bytes,
            flush_seed_read_bytes: self.flush_seed_read_bytes - before.flush_seed_read_bytes,
            write_path_seed_read_bytes: self.write_path_seed_read_bytes
                - before.write_path_seed_read_bytes,
            staging_put_bytes_drain: self.staging_put_bytes_drain - before.staging_put_bytes_drain,
            staging_put_bytes_flush: self.staging_put_bytes_flush - before.staging_put_bytes_flush,
            staging_put_bytes_teardown: self.staging_put_bytes_teardown
                - before.staging_put_bytes_teardown,
            staging_put_bytes_wt_fallback: self.staging_put_bytes_wt_fallback
                - before.staging_put_bytes_wt_fallback,
            writeback_enqueued_drain: self.writeback_enqueued_drain
                - before.writeback_enqueued_drain,
            writeback_enqueued_flush: self.writeback_enqueued_flush
                - before.writeback_enqueued_flush,
            writeback_enqueued_teardown: self.writeback_enqueued_teardown
                - before.writeback_enqueued_teardown,
            writeback_enqueued_wt_fallback: self.writeback_enqueued_wt_fallback
                - before.writeback_enqueued_wt_fallback,
            durable_upload_bytes_writeback: self.durable_upload_bytes_writeback
                - before.durable_upload_bytes_writeback,
            durable_upload_bytes_self_flush: self.durable_upload_bytes_self_flush
                - before.durable_upload_bytes_self_flush,
            durable_upload_bytes_escalation: self.durable_upload_bytes_escalation
                - before.durable_upload_bytes_escalation,
            restage_churn_removes: self.restage_churn_removes - before.restage_churn_removes,
            restage_churn_bytes: self.restage_churn_bytes - before.restage_churn_bytes,
            write_block_revisits: self.write_block_revisits - before.write_block_revisits,
            staging_sibling_probes: self.staging_sibling_probes - before.staging_sibling_probes,
            write_through_bytes: self.write_through_bytes - before.write_through_bytes,
            patch_writes: self.patch_writes - before.patch_writes,
            patch_write_bytes: self.patch_write_bytes - before.patch_write_bytes,
            patch_edge_rmw_reads: self.patch_edge_rmw_reads - before.patch_edge_rmw_reads,
        }
    }

    /// Device read bytes the ledger attributes (seed materializations).
    fn read_bytes(&self) -> u64 {
        self.spill_seed_read_bytes + self.flush_seed_read_bytes + self.write_path_seed_read_bytes
    }

    /// Device/staging write bytes the ledger attributes (incl. the RW2
    /// in-place patch DMAs — device writes like any other).
    fn write_bytes(&self) -> u64 {
        self.spill_staging_put_bytes
            + self.staging_put_bytes_drain
            + self.staging_put_bytes_flush
            + self.staging_put_bytes_teardown
            + self.staging_put_bytes_wt_fallback
            + self.durable_upload_bytes_writeback
            + self.durable_upload_bytes_self_flush
            + self.durable_upload_bytes_escalation
            + self.write_through_bytes
            + self.patch_write_bytes
    }

    fn print(&self, label: &str, user_bytes: u64) {
        eprintln!("=== RW1 device-byte ledger [{label}] (user bytes {user_bytes}) ===");
        eprintln!(
            "  read leg : spill_seed {} ({} reads) | flush_seed {} | write_path_seed {}  => total {} ({:.1}x user)",
            self.spill_seed_read_bytes,
            self.spill_seed_reads,
            self.flush_seed_read_bytes,
            self.write_path_seed_read_bytes,
            self.read_bytes(),
            self.read_bytes() as f64 / user_bytes.max(1) as f64,
        );
        eprintln!(
            "  write leg: spill_staging {} ({} puts) | drain {} | flush {} | teardown {} | wt_fallback {} | durable(writeback {} / self_flush {} / escalation {}) | write_through {}  => total {} ({:.1}x user)",
            self.spill_staging_put_bytes,
            self.spill_staging_puts,
            self.staging_put_bytes_drain,
            self.staging_put_bytes_flush,
            self.staging_put_bytes_teardown,
            self.staging_put_bytes_wt_fallback,
            self.durable_upload_bytes_writeback,
            self.durable_upload_bytes_self_flush,
            self.durable_upload_bytes_escalation,
            self.write_through_bytes,
            self.write_bytes(),
            self.write_bytes() as f64 / user_bytes.max(1) as f64,
        );
        eprintln!(
            "  drivers  : writeback_enqueued drain {} / flush {} / teardown {} / wt_fallback {} | restage_churn {} removes ({} B) | revisits {} | sibling_probes {}",
            self.writeback_enqueued_drain,
            self.writeback_enqueued_flush,
            self.writeback_enqueued_teardown,
            self.writeback_enqueued_wt_fallback,
            self.restage_churn_removes,
            self.restage_churn_bytes,
            self.write_block_revisits,
            self.staging_sibling_probes,
        );
        eprintln!(
            "  patch    : patch_writes {} ({} B) | edge_rmw_reads {}",
            self.patch_writes, self.patch_write_bytes, self.patch_edge_rmw_reads,
        );
    }
}

/// The scoreboard rand-write SHAPE at sandbox scale: LBA-aligned 4 KiB
/// overwrites at mid-block offsets (never block-complete, never adjacent)
/// over `blocks` distinct striped blocks, `passes` shuffled passes.
async fn rand_write_storm(h: &H, ino: u64, blocks: u64, passes: u64) -> u64 {
    let payload = pattern(4096, 99);
    let mut user_bytes = 0u64;
    for pass in 0..passes {
        for i in 0..blocks {
            // Deterministic coprime stride = shuffled, non-adjacent order.
            let b = (i * 173 + pass * 61) % blocks;
            let off = b * BS + 8192; // mid-block, 4 KiB-aligned
            write_at(h, ino, off, &payload).await;
            user_bytes += payload.len() as u64;
        }
    }
    user_bytes
}

// ---------------------------------------------------------------------------
// The mapping-form sanity line (review Issue 19 — permanent polarity tripwire)
// ---------------------------------------------------------------------------

/// A freshly-created striped file's block map must carry the UNDECORATED
/// 2-part whole-block form (`persist_block_key`'s bare offset strings) —
/// the W1-eligible population. Printed verbatim: this line is the rig
/// output's pinned sanity artifact (a predicate keyed on `exact == true`
/// would patch NOTHING, this fixture included).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mapping_form_sanity_line_is_undecorated_2part() {
    rig_on();
    let _g = serial().await;
    let h = make("rw1_mapform").await;
    let ino = create(&h, "mapform.dat").await;
    make_striped(&h, ino, 8, 7).await;

    let meta = backend_meta(&h, ino).await;
    let bm = meta
        .block_map
        .as_ref()
        .expect("striped fixture must carry a block map");
    assert!(!bm.is_empty(), "premise: block map populated");
    let m0 = bm.get(&0).expect("block 0 mapped").clone();
    eprintln!(
        "RW1 mapping-form sanity: fixture block 0 mapping = {:?} form = {}",
        m0,
        block_mapping_form(&m0)
    );
    for (b, m) in bm.iter() {
        assert_eq!(
            block_mapping_form(m),
            "undecorated-2part",
            "block {b} mapping {m:?}: a freshly-striped file must map every \
             block in the undecorated whole-block form (Issue-19 polarity \
             tripwire — decorated means the eligible population inverted)"
        );
    }

    // Classifier pins for the other forms (unit-level).
    assert_eq!(block_mapping_form("12345"), "undecorated-2part");
    assert_eq!(block_mapping_form("be1://12345"), "undecorated-2part");
    assert_eq!(block_mapping_form("12345:0:65536"), "decorated-3part");
    assert_eq!(
        block_mapping_form("be1://12345:4096:512"),
        "decorated-3part"
    );
    assert_eq!(block_mapping_form("a:b"), "unknown");
}

// ---------------------------------------------------------------------------
// The staged-sibling probe cost pin (H1 baseline)
// ---------------------------------------------------------------------------

/// Every striped block checkout dispatches the staged-sibling
/// `spawn_blocking` remove hop whether or not anything is staged — the H1
/// pure-overhead face. Pinned as EQUALITY so RW3's lock-free probe flips
/// this pin the day it elides the hop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_sibling_probe_fires_once_per_block_write() {
    rig_on();
    let _g = serial().await;
    let h = make("rw1_probe").await;
    // RW1's §1.2 pipeline pin documents the ACCUMULATION path — the
    // pipeline every W1-patch-ineligible shape still rides. The aligned
    // storm shape became patch-eligible in RW2 (zero spills, zero seeds,
    // zero churn), so this pin runs with the patch OFF; the flipped
    // G-RW2 gate below measures the same shape with the patch ON.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let ino = create(&h, "probe.dat").await;
    make_striped(&h, ino, 8, 3).await;

    let before = ledger();
    let payload = pattern(4096, 5);
    for b in 0..8u64 {
        write_at(&h, ino, b * BS + 4096, &payload).await;
    }
    let d = ledger().delta(&before);
    assert_eq!(
        d.staging_sibling_probes, 8,
        "one spawn_blocking staged-sibling probe per striped block write \
         (the H1 hop; nothing is staged — the probe is pure overhead today)"
    );
}

// ---------------------------------------------------------------------------
// Ledger buckets + driver attribution (green — the rig's own contract)
// ---------------------------------------------------------------------------

/// The §1.2 buckets fire under their OWN drivers and reconcile: the inline
/// victim spill pays seed reads + staging puts; the parked drain stages and
/// enqueues under the `drain` driver; the fsync family lands in `flush`;
/// re-stage churn counts the staged siblings a revisit discards; and the
/// foreground write path never enqueues writeback on this shape
/// (`wt_fallback == 0` — the Issue-4 correction made assertable).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ledger_buckets_reconcile_and_attribute_drivers() {
    rig_on();
    let _g = serial().await;
    let h = make("rw1_buckets").await;
    // RW1's §1.2 pipeline pin documents the ACCUMULATION path — the
    // pipeline every W1-patch-ineligible shape still rides. The aligned
    // storm shape became patch-eligible in RW2 (zero spills, zero seeds,
    // zero churn), so this pin runs with the patch OFF; the flipped
    // G-RW2 gate below measures the same shape with the patch ON.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let ino = create(&h, "buckets.dat").await;
    let blocks: u64 = PARKED_CAP + 24; // 280 distinct blocks > the parked cap
    make_striped(&h, ino, blocks, 11).await;

    let before = ledger();
    let user = rand_write_storm(&h, ino, blocks, 2).await;
    let storm = ledger().delta(&before);
    storm.print("storm (2 passes, pre-drain)", user);

    // Read-your-writes across the parked/spilled overlay mix: the storm
    // payload serves back regardless of which side (RAM buffer or staged
    // sibling) owns each block right now.
    let payload = pattern(4096, 99);
    for b in [0u64, blocks / 2, blocks - 1] {
        let got = read_at(&h, ino, b * BS + 8192, 4096).await;
        assert_eq!(got, payload, "block {b}: storm payload must read back");
    }

    // Bucket 1 — the inline victim spill fired and paid both legs.
    assert!(
        storm.spill_staging_puts > 0,
        "past the parked cap every insert spills a victim inline"
    );
    assert!(
        storm.spill_seed_reads > 0 && storm.spill_seed_read_bytes >= storm.spill_seed_reads * BS,
        "deferred victims materialize their seed at the spill (one whole-block \
         read each): reads={} bytes={}",
        storm.spill_seed_reads,
        storm.spill_seed_read_bytes
    );
    assert_eq!(
        storm.spill_staging_put_bytes,
        storm.spill_staging_puts * BS,
        "spill staging puts are whole-block images"
    );

    // Bucket 4 — pass 2 revisits re-checked out spilled siblings: churn.
    assert!(
        storm.restage_churn_removes > 0,
        "block revisits over spilled keys must count same-key re-stage churn"
    );
    assert_eq!(
        storm.restage_churn_bytes,
        storm.restage_churn_removes * BS,
        "churned staged siblings are whole-block images"
    );
    assert!(
        storm.write_block_revisits >= blocks,
        "pass 2 revisits every block (an overlay — parked or staged — owns \
         it): revisits={} blocks={blocks}",
        storm.write_block_revisits
    );

    // Issue-4 correction, assertable: the foreground write path enqueued
    // NOTHING (no write-through fallback, no fsync, no drain yet).
    assert_eq!(storm.writeback_enqueued_wt_fallback, 0);
    assert_eq!(storm.writeback_enqueued_drain, 0);
    assert_eq!(storm.writeback_enqueued_flush, 0);
    assert_eq!(storm.staging_put_bytes_wt_fallback, 0);
    assert_eq!(storm.staging_put_bytes_drain, 0);
    assert_eq!(storm.staging_put_bytes_flush, 0);
    assert_eq!(
        storm.write_through_bytes, 0,
        "mid-block 4 KiB writes never complete a block — no write-through"
    );

    // Bucket 2 — the R5-pressure parked drain: stages the parked backlog
    // under the `drain` driver and enqueues its durable-upload chain.
    let before_drain = ledger();
    h.fs.drain_parked_toward(0).await;
    let drain = ledger().delta(&before_drain);
    drain.print("drain_parked_toward(0)", user);
    assert!(
        drain.staging_put_bytes_drain > 0,
        "the parked drain stages under the DRAIN driver"
    );
    assert_eq!(
        drain.writeback_enqueued_drain,
        drain.staging_put_bytes_drain / BS,
        "every drain-staged block requests exactly one writeback flush unit \
         — the durable-upload leg's driver is the drain, never the \
         foreground write"
    );
    assert_eq!(
        drain.staging_put_bytes_flush, 0,
        "no fsync-family attribution from the drain driver"
    );
    // The durable-upload leg: units past the queue cap flush SYNCHRONOUSLY
    // (enqueue_writeback's queue-full fallback — the same per-block flush
    // unit the live worker runs), so the drain's enqueue overflow moves
    // `durable_upload_bytes_writeback` right here in the sandbox.
    let cap = h.fs.writeback_queue_cap as u64;
    if drain.writeback_enqueued_drain > cap {
        assert!(
            drain.durable_upload_bytes_writeback >= (drain.writeback_enqueued_drain - cap) * BS,
            "queue-full drain units flush durably through \
             flush_one_active_block: enqueued={} cap={cap} durable_bytes={}",
            drain.writeback_enqueued_drain,
            drain.durable_upload_bytes_writeback
        );
    }

    // Bucket 3 driver isolation — the fsync/close family lands in `flush`,
    // not in `drain`, and requests its own durable-upload chain.
    let ino2 = create(&h, "buckets2.dat").await;
    make_striped(&h, ino2, 4, 13).await;
    let p = pattern(4096, 17);
    write_at(&h, ino2, BS + 8192, &p).await; // parks one partial block
    let before_flush = ledger();
    let token = h.dlm.get_fencing_token_ino(ino2);
    h.fs.flush_inode_to_backend(ino2, token)
        .await
        .expect("fsync-family flush");
    let flush = ledger().delta(&before_flush);
    flush.print("flush_inode_to_backend (fsync family)", p.len() as u64);
    assert!(
        flush.staging_put_bytes_flush >= BS,
        "the fsync family stages under the FLUSH driver"
    );
    assert!(
        flush.writeback_enqueued_flush >= 1,
        "the fsync family's durable chain is requested under the FLUSH driver"
    );
    assert_eq!(flush.staging_put_bytes_drain, 0);
    assert_eq!(flush.writeback_enqueued_drain, 0);
}

// ---------------------------------------------------------------------------
// The standing-RED G-RW2 amplification gate
// ---------------------------------------------------------------------------

/// THE G-RW2 GATE (design-random-small-writes §2), **flipped green by PR
/// RW2's W1 sole-owner extent patch**: on the aligned rand-write shape,
/// device write bytes ≤ 4× user bytes and device read bytes ≤ 1× user
/// bytes, measured FROM the attribution ledger — plus the gate's own
/// decision-ledger tripwires (review Issue 7): `patch_writes` ≈ the row's
/// op count (deviation = predicate rot) and `patch_edge_rmw_reads` == 0
/// (v1 is aligned-only — the ≤1× read slack may not absorb one silently).
///
/// RW1 shipped this `#[ignore]`d standing-RED (the whole-block RMW
/// pipeline paid ~2 blocks of device I/O per 4 KiB op — write 10.7×,
/// read 8.0× at sandbox scale); the W1 patch pays ONE in-place 4 KiB DMA
/// per op — zero seed reads, zero staging, zero writeback, zero meta —
/// and the gate now runs at per-commit cadence, permanently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g_rw2_device_byte_ledger_on_rand_write_shape() {
    rig_on();
    let _g = serial().await;
    let h = make("rw1_red").await;
    let ino = create(&h, "red.dat").await;
    let blocks: u64 = PARKED_CAP + 128; // 384 distinct blocks
    make_striped(&h, ino, blocks, 23).await;

    // Mapping-form sanity line rides the red gate's own fixture too.
    let meta = backend_meta(&h, ino).await;
    let m0 = meta
        .block_map
        .as_ref()
        .and_then(|bm| bm.get(&0).cloned())
        .expect("block 0 mapped");
    eprintln!(
        "RW1 mapping-form sanity: red-gate fixture block 0 mapping = {m0:?} form = {}",
        block_mapping_form(&m0)
    );
    assert_eq!(block_mapping_form(&m0), "undecorated-2part");

    let before = ledger();
    let user_bytes = rand_write_storm(&h, ino, blocks, 2).await;
    // The R5-pressure machinery's drain — on the live daemon the budget
    // sampler drives this; the sandbox invokes the same entry point.
    h.fs.drain_parked_toward(0).await;
    let d = ledger().delta(&before);
    d.print("rand-write shape (2 passes + parked drain)", user_bytes);

    let ops = user_bytes / 4096;
    let per_op = (d.read_bytes() + d.write_bytes()) / ops.max(1);
    // The queued durable-upload leg: the drain enqueued one flush unit per
    // staged block; on the live daemon the writeback worker uploads each
    // (one more whole-block device write — the §1.2 third leg).
    let pending_durable = d.writeback_enqueued_drain * BS;
    eprintln!(
        "per-op attributed device bytes: {per_op} B/op over {ops} ops \
         (block size {BS}; scoreboard scale = 4 MiB blocks => ~12 MiB/op); \
         R:W = 1:{:.2} counted, 1:{:.2} with the enqueued durable leg",
        d.write_bytes() as f64 / d.read_bytes().max(1) as f64,
        (d.write_bytes() + pending_durable) as f64 / d.read_bytes().max(1) as f64,
    );

    let read_amp = d.read_bytes() as f64 / user_bytes as f64;
    let write_amp = d.write_bytes() as f64 / user_bytes as f64;
    assert!(
        write_amp <= 4.0 && read_amp <= 1.0,
        "G-RW2 amplification gate: device write bytes {write_amp:.1}x user \
         (gate <= 4x), device read bytes {read_amp:.1}x user (gate <= 1x) — \
         the whole-block RMW pipeline is back: a 4 KiB op is paying \
         seed-read/staging/upload legs the W1 patch removed"
    );

    // The gate's decision-ledger tripwires (G-RW2 clauses, review Issue 7).
    assert_eq!(
        d.patch_writes, ops,
        "patch_writes must equal the row's op count — every aligned \
         isolated 4 KiB overwrite of a whole-block-mapped sole-owned block \
         must ride the patch (deviation = predicate rot)"
    );
    assert_eq!(
        d.patch_edge_rmw_reads, 0,
        "patch_edge_rmw_reads must be 0 in v1 (aligned-only): any nonzero \
         is an alignment/predicate regression BY DEFINITION"
    );
}

// ---------------------------------------------------------------------------
// The rig: write sub-phases + per-site lock waits (wired by this PR)
// ---------------------------------------------------------------------------

/// The armed rig records every striped-write sub-phase and attributes
/// BLOCK_FLUSH_LOCKS waits per call site: partial writes exercise
/// route-classify / checkout / sibling-remove / merge-copy / park-spill;
/// spills exercise seed-fetch + staging-put; complete blocks exercise
/// upload-DMA + map-merge; the drain exercises the flush-exit site.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_rig_phases_and_sites_record() {
    rig_on();
    let _g = serial().await;
    let h = make("rw1_rig").await;
    // RW1's §1.2 pipeline pin documents the ACCUMULATION path — the
    // pipeline every W1-patch-ineligible shape still rides. The aligned
    // storm shape became patch-eligible in RW2 (zero spills, zero seeds,
    // zero churn), so this pin runs with the patch OFF; the flipped
    // G-RW2 gate below measures the same shape with the patch ON.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let ino = create(&h, "rig.dat").await;
    let blocks: u64 = PARKED_CAP + 16;
    make_striped(&h, ino, blocks, 29).await;

    let p0: std::collections::HashMap<&str, u64> = [
        "route_classify",
        "checkout",
        "sibling_remove",
        "merge_copy",
        "seed_fetch",
        "upload_dma",
        "upload_map_merge",
        "park_spill",
        "staging_put",
    ]
    .into_iter()
    .map(|p| (p, phase_total(p)))
    .collect();
    let s0_checkout = site_total("write_checkout");
    let s0_flush = site_total("flush_exit");

    let n = blocks; // one partial write per block (one pass)
    let user = rand_write_storm(&h, ino, blocks, 1).await;
    assert_eq!(user, n * 4096);

    // One complete block write => write-through (upload DMA + map merge).
    let full = pattern(BS as usize, 31);
    write_at(&h, ino, 0, &full).await;

    // Flush-exit site: the parked drain's stage exits.
    h.fs.drain_parked_toward(0).await;

    let expect_ge = |phase: &str, min: u64| {
        let got = phase_total(phase) - p0[phase];
        assert!(
            got >= min,
            "phase {phase}: {got} samples recorded, expected >= {min}"
        );
    };
    expect_ge("route_classify", n);
    expect_ge("checkout", n);
    expect_ge("sibling_remove", n);
    expect_ge("merge_copy", n);
    expect_ge("park_spill", n - 1);
    expect_ge("seed_fetch", 1); // spill victims materialize seeds
    expect_ge("staging_put", 1); // spill staging puts
    expect_ge("upload_dma", 1); // the complete-block write-through
    expect_ge("upload_map_merge", 1);

    assert!(
        site_total("write_checkout") - s0_checkout >= n,
        "every striped block write records the write_checkout lock site"
    );
    assert!(
        site_total("flush_exit") > s0_flush,
        "the drain's stage exits record the flush_exit lock site"
    );

    // The armed stats surface carries the RW1 families (and the ledger).
    let stats = read_stats_json(&h).await;
    let m = &stats["metrics"];
    for key in [
        "fuse_write_phase_ns",
        "block_lock_wait_by_site",
        "block_lock_stripe_audit",
        "fuse_write_inflight",
        "spill_seed_read_bytes",
        "staging_put_bytes_drain",
        "restage_churn_removes",
        "write_block_revisits",
    ] {
        assert!(!m[key].is_null(), "armed .stats surface must carry {key}");
    }
}

async fn read_stats_json(h: &H) -> serde_json::Value {
    let opened =
        h.fs.open(h.req, STATS_INODE, libc::O_RDONLY as u32)
            .await
            .expect("open .stats");
    let data =
        h.fs.read(h.req, STATS_INODE, opened.fh, 0, 16 * 1024 * 1024, 0)
            .await
            .expect("read .stats")
            .data
            .to_vec();
    h.fs.release(h.req, STATS_INODE, opened.fh, 0, 0, false)
        .await
        .expect("release .stats");
    serde_json::from_slice(&data).expect("stats JSON parses")
}

// ---------------------------------------------------------------------------
// The H2b stripe-collision audit + the in-flight WRITE histogram
// ---------------------------------------------------------------------------

/// Contended acquisitions classify CROSS-KEY (a different (ino, block) key
/// last held the shared stripe — the splitmix-spread defect signature H2b
/// hunts) vs SAME-KEY (true per-block serialization), and the
/// waiters-at-arrival depth records the convoy shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stripe_collision_audit_classifies_cross_key_vs_same_key() {
    rig_on();
    let _g = serial().await;

    // Find a distinct (ino, block) pair colliding on one stripe.
    let base = (7_001u64, 3u32);
    let stripe = BLOCK_FLUSH_LOCKS.block_shard_index(base.0, base.1);
    let mut other = None;
    'search: for ino in 8_000u64..200_000 {
        for b in 0..64u32 {
            if (ino, b) != base && BLOCK_FLUSH_LOCKS.block_shard_index(ino, b) == stripe {
                other = Some((ino, b));
                break 'search;
            }
        }
    }
    let other = other.expect("4096 stripes: a colliding key exists in the search space");

    let audit0 = block_lock_stripe_audit_json();
    let (cross0, same0, depth0) = (
        hist_total(&audit0["cross_key_waits"]),
        hist_total(&audit0["same_key_waits"]),
        hist_total(&audit0["waiters_at_arrival"]),
    );

    // CROSS-KEY: `base` holds the stripe; `other` must wait on it.
    {
        let guard = block_lock_acquire(base.0, base.1, BlockLockSite::WriteCheckout).await;
        let waiter = tokio::spawn(async move {
            let _g = block_lock_acquire(other.0, other.1, BlockLockSite::WriteCheckout).await;
        });
        // The waiter parks (try_lock fails, records arrival depth), then we
        // release and it classifies against the last-acquirer word (= base).
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(guard);
        waiter.await.unwrap();
    }
    let audit1 = block_lock_stripe_audit_json();
    assert_eq!(
        hist_total(&audit1["cross_key_waits"]),
        cross0 + 1,
        "a wait behind a DIFFERENT key on the shared stripe is a cross-key \
         collision (H2b)"
    );
    assert!(
        hist_total(&audit1["waiters_at_arrival"]) > depth0,
        "contended arrivals record the stripe waiter depth"
    );

    // SAME-KEY: two acquirers of the SAME (ino, block).
    {
        let guard = block_lock_acquire(base.0, base.1, BlockLockSite::WriteCheckout).await;
        let waiter = tokio::spawn(async move {
            let _g = block_lock_acquire(base.0, base.1, BlockLockSite::WritebackFlush).await;
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(guard);
        waiter.await.unwrap();
    }
    let audit2 = block_lock_stripe_audit_json();
    assert_eq!(
        hist_total(&audit2["same_key_waits"]),
        same0 + 1,
        "a wait behind the SAME key is true block serialization, not a \
         stripe collision"
    );

    // Spill-victim try_lock accounting: refusal counts a skip, acquisition
    // records a zero-wait site sample.
    let skips0 = audit2["spill_victim_lock_skips"].as_u64().unwrap();
    block_lock_try_note(BlockLockSite::SpillVictim, base.0, base.1, false);
    block_lock_try_note(BlockLockSite::SpillVictim, base.0, base.1, true);
    let audit3 = block_lock_stripe_audit_json();
    assert_eq!(
        audit3["spill_victim_lock_skips"].as_u64().unwrap(),
        skips0 + 1
    );
    assert!(site_total("spill_victim") >= 1);
}

/// The in-flight WRITE histogram samples the live WRITE-handler depth at
/// each arrival (the §12 OQ2 instrument: does the kernel actually dispatch
/// iodepth×threads concurrent WRITEs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inflight_write_histogram_samples_depth() {
    rig_on();
    let _g = serial().await;
    let before = write_inflight_json();
    let d1 = WriteInflight::enter();
    let d2 = WriteInflight::enter();
    let d3 = WriteInflight::enter();
    drop(d3);
    drop(d2);
    drop(d1);
    let after = write_inflight_json();
    assert_eq!(
        hist_total(&after),
        hist_total(&before) + 3,
        "each WRITE arrival samples the in-flight depth"
    );
    // Depths 1, 2, 3 landed in the 1 / 2 / <=4 buckets.
    assert!(after["1"].as_u64().unwrap() > before["1"].as_u64().unwrap());
    assert!(after["2"].as_u64().unwrap() > before["2"].as_u64().unwrap());
    assert!(after["<=4"].as_u64().unwrap() > before["<=4"].as_u64().unwrap());
}
