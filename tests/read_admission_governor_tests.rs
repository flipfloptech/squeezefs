//! Read-admission scan resistance (2026-07-26 finding: cold-dominated
//! random 4k over a working set ≫ budget collapsed the DEFAULT hybrid
//! posture ~20× under its own ghost-escalation admission traffic — every
//! second touch fetched a WHOLE 4 MiB block that evicted before reuse, so
//! admission bandwidth was pure waste competing with foreground reads for
//! device queue slots; rig 12.4k vs 230k IOPS device-true, field
//! `rareq-sz` ~20 KB at >90 % util with ~0.1 % hit rate).
//!
//! The fix under test: the **admission governor** — eviction-payback
//! feedback plus a bandwidth clamp on ghost escalations:
//!
//! 1. WASTE SIGNAL (the `prefetch_evicted_unconsumed` sibling for
//!    admissions): a ghost-admitted (protected) hot-tier victim evicted
//!    without paying back its fill cost (`served_bytes < len`) is waste;
//!    a victim never served at all counts `read_admission_evicted_unhit`.
//! 2. CLAMP: when windowed waste ≥ half of windowed admitted bytes (floor
//!    2 blocks), escalations are bounded to
//!    `SQUEEZEFS_READ_ADMISSION_FILL_PCT` (default 5) percent of windowed
//!    foreground ranged device bytes; denials count
//!    `read_admission_governor_denials` and the denied read stays a
//!    device-true ranged window read (always correctness-safe).
//! 3. FIT UNCHANGED: a working set the hot tier retains produces no
//!    waste ⇒ the governor never clamps ⇒ warm-up and steady state are
//!    byte-identical to the pre-governor hybrid policy.
//! 4. SKEW: under clamp the hot subset still converges to RAM — denied
//!    keys record no escalation cooldown, so they retry and win the
//!    trickle (which is paid for by their own foreground traffic), and
//!    clock `referenced` re-arming keeps served blocks resident.
//!
//! Counter-asserting phases share ONE test fn (`governor_phases`) — the
//! churn suite's counter-isolation discipline (METRICS is process-global).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

/// CACHE-LESS fixture (no staging dirs — the rig/field shape: no NVMe
/// read tier, so admitted warmth lives in the hot RAM tier only and the
/// working-set-vs-budget regime is exactly the hot budget).
async fn make_with(uuid: [u8; 16], alloc_ns: &str) -> H {
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let cache = TieredCache::new(
        Vec::new(), // cache-less
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
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
        req,
        _b: b,
        _m: m,
        _s: None,
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_flags(h: &H, ino: u64, off: u64, size: u32, flags: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, flags)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn block_map_of(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map = block_map_of(h, ino).await;
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

/// Non-promoting, non-crediting probe — asserts must never perturb
/// clock/class/payback state.
fn hot_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.hot_block.get_no_promote(key).is_some()
}

const OD: u32 = libc::O_DIRECT as u32;

fn esc() -> u64 {
    METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed)
}
fn denials() -> u64 {
    METRICS
        .read_admission_governor_denials
        .load(Ordering::Relaxed)
}
fn unhit() -> u64 {
    METRICS.read_admission_evicted_unhit.load(Ordering::Relaxed)
}

/// Fixture-file writer: N blocks, byte pattern = block index + 1.
async fn striped_file(
    h: &H,
    name: &str,
    blocks: u64,
) -> (u64, std::sync::Arc<std::collections::HashMap<u32, String>>) {
    let ino = create(h, name).await;
    for b in 0..blocks {
        write_at(h, ino, b * BS, &vec![(b % 250) as u8 + 1; BS as usize]).await;
    }
    let map = make_cold(h, ino).await;
    (ino, map)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn governor_phases() {
    // ---- Phase FIT (bar b): working set ≤ budget ⇒ the pre-governor
    // hybrid policy verbatim — every key's second touch escalates, zero
    // denials, zero waste, warm reads at RAM speed with the device flat.
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "8"); // 16 slots
    let h = make_with(*b"admission-gov-01", "gov_ns_a").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    let (ino, map) = striped_file(&h, "fit", 4).await;

    let (e0, d0, u0) = (esc(), denials(), unhit());
    for b in 0..4u64 {
        let d = read_flags(&h, ino, b * BS + 4096, 4096, OD).await; // touch 1
        assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
    }
    for b in 0..4u64 {
        let d = read_flags(&h, ino, b * BS + 12288, 4096, OD).await; // touch 2
        assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
    }
    assert_eq!(
        esc() - e0,
        4,
        "fitting set: every second touch escalates exactly as today \
         (the governor must be invisible when the cache is earning)"
    );
    assert_eq!(denials() - d0, 0, "fitting set: zero governor denials");
    assert_eq!(unhit() - u0, 0, "fitting set: zero admission waste");
    for b in 0..4u32 {
        assert!(hot_has(&h, map.get(&b).unwrap()), "fit block {b} hot");
    }
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    for b in 0..4u64 {
        let d = read_flags(&h, ino, b * BS + 20480, 4096, OD).await;
        assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
    }
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0,
        "fitting set steady state: RAM serves, device flat"
    );
    drop(h);

    // ---- Phase CHURN (bar a): working set ≫ budget ⇒ the governor
    // clamps escalation bandwidth after the waste signal fires. 16 blocks
    // vs a 2-slot hot tier: pass 2's second touches must NOT all escalate
    // (pre-fix: 16 whole-block fetches = 8 MiB of admission traffic for
    // 64 KiB of user reads); the first few admissions evict-unserved,
    // the clamp engages, and the rest are denied into device-true ranged
    // reads (correct bytes, no admission).
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1"); // 2 slots
    std::env::set_var("SQUEEZEFS_READ_ADMISSION_FILL_PCT", "5");
    let h = make_with(*b"admission-gov-02", "gov_ns_b").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_ADMISSION_FILL_PCT");
    let (ino, _map) = striped_file(&h, "churn", 16).await;

    let (e0, d0, u0) = (esc(), denials(), unhit());
    for b in 0..16u64 {
        let d = read_flags(&h, ino, b * BS + 4096, 4096, OD).await; // touch 1
        assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
    }
    for b in 0..16u64 {
        let d = read_flags(&h, ino, b * BS + 12288, 4096, OD).await; // touch 2
        assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
    }
    let esc_delta = esc() - e0;
    assert!(
        esc_delta <= 6,
        "churn: escalations must be governor-bounded once admissions \
         stop paying back (got {esc_delta}, pre-fix 16 — every second \
         touch fetched a whole block into a 2-slot tier)"
    );
    assert!(
        esc_delta >= 2,
        "churn: the governor must still explore (trickle), never zero \
         (got {esc_delta})"
    );
    assert!(
        denials() - d0 >= 8,
        "churn: denied escalations are counted \
         (read_admission_governor_denials; got {})",
        denials() - d0
    );
    assert!(
        unhit() - u0 >= 2,
        "churn: admitted-but-never-served evictions are the waste signal \
         (read_admission_evicted_unhit; got {})",
        unhit() - u0
    );
    drop(h);

    // ---- Phase SKEW (bar c): hot subset + cold tail under an engaged
    // clamp — the hot pair must still get admitted (denied keys record no
    // cooldown; their own foreground traffic accrues the trickle) and
    // then serve from RAM with the device flat. The token grant is a
    // wall-clock mechanism: shrink the epoch via the test seam so the
    // trickle mints inside the loop instead of sleeping out 2 s windows.
    squeezefs::routing::TEST_ADMISSION_EPOCH_MS.store(100, Ordering::Relaxed);
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1"); // 2 slots
    std::env::set_var("SQUEEZEFS_READ_ADMISSION_FILL_PCT", "100");
    let h = make_with(*b"admission-gov-03", "gov_ns_c").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_ADMISSION_FILL_PCT");
    let (ino, map) = striped_file(&h, "skew", 8).await;
    let k0 = map.get(&0).unwrap().clone();
    let k1 = map.get(&1).unwrap().clone();

    // Cold-tail churn engages the clamp (blocks 2..8: record + second
    // touch; the early admissions evict-unserved in the 2-slot tier).
    for b in 2..8u64 {
        read_flags(&h, ino, b * BS + 4096, 4096, OD).await;
    }
    for b in 2..8u64 {
        read_flags(&h, ino, b * BS + 12288, 4096, OD).await;
    }
    let d_clamped = denials();
    assert!(
        denials() > 0,
        "skew fixture: the clamp must be engaged before the hot phase"
    );

    // Hot pair: keep reading. Their ghost second touches are initially
    // denied, but denial records no cooldown — the pair retries, its own
    // ranged foreground accrues the fill budget, and both blocks must
    // reach RAM within a bounded number of reads.
    let mut warm = false;
    for i in 0..3000u64 {
        let off0 = (i % 100) * 4096;
        read_flags(&h, ino, off0, 4096, OD).await;
        read_flags(&h, ino, BS + off0, 4096, OD).await;
        if hot_has(&h, &k0) && hot_has(&h, &k1) {
            warm = true;
            break;
        }
        // Pace the loop across shimmed epoch boundaries (the mint is
        // wall-clock; this is the mechanism under test, not a sync hack).
        if i % 50 == 49 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    assert!(
        warm,
        "skew: the hot subset must still get admitted under an engaged \
         clamp (bar c — scan resistance must not become admission death)"
    );
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    for i in 0..10u64 {
        let d = read_flags(&h, ino, i * 8192, 4096, OD).await;
        assert!(d.iter().all(|&x| x == 1));
        let d = read_flags(&h, ino, BS + i * 8192, 4096, OD).await;
        assert!(d.iter().all(|&x| x == 2));
    }
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0,
        "skew steady state: the admitted hot pair serves from RAM"
    );
    let _ = d_clamped;
    drop(h);
    squeezefs::routing::TEST_ADMISSION_EPOCH_MS.store(0, Ordering::Relaxed);
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    // ---- Phase PAYBACK (plumbing, deterministic): the waste signal is
    // BYTES-based — an admitted victim that served at least its own size
    // before eviction is not waste; one that never served counts unhit.
    let gov = Arc::new(squeezefs::routing::AdmissionGovernor::new(5));
    let cache = squeezefs::cache::lru::LruCache::with_capacity(1024 * 1024)
        .with_admission_governor(gov.clone());
    let payload = bytes::Bytes::from(vec![7u8; 512 * 1024]);
    let (u0, w0) = (
        unhit(),
        METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed),
    );
    cache.put("pk0", payload.clone());
    cache.put("pk1", payload.clone());
    // pk1 pays back its fill cost before eviction; pk0 never serves.
    cache.get_serving("pk1", 512 * 1024);
    cache.put("pk2", payload.clone()); // clock evicts pk0 (never served)
    cache.put("pk3", payload.clone()); // clock evicts pk1 (paid back)
    assert!(cache.get_no_promote("pk0").is_none(), "pk0 evicted");
    assert!(cache.get_no_promote("pk1").is_none(), "pk1 evicted");
    assert_eq!(
        unhit() - u0,
        1,
        "exactly the never-served victim counts read_admission_evicted_unhit"
    );
    assert_eq!(
        METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed) - w0,
        512 * 1024,
        "waste is bytes-based: pk0's full length, pk1 zero (paid back)"
    );

    // ---- Phase SHARD-GEOMETRY (fixture-level, kept inside this fn: the
    // env-driven fixtures race across parallel test fns): 4 MiB hot
    // values in shards sized below a few blocks cannot coexist (32
    // shards × 128 MiB budget = ONE block per shard — measured on the
    // rig as perpetual protected churn at 50 % budget fill: 6.1k
    // whole-block refetches / 20 s on a FITTING 64 MiB set). The
    // TieredCache hot tier must keep per-shard capacity ≥ 4 default
    // blocks (16 MiB).
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "128");
    let h = make_with(*b"admission-gov-04", "gov_ns_d").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let shards = h.fs.router.cache.hot_block.num_shards();
    assert!(
        shards <= 8,
        "128 MiB hot budget must keep ≥ 16 MiB (4 blocks) per shard, got \
         {shards} shards"
    );
    assert!(shards >= 1);
    drop(h);

    // ---- Phase HERD (reservation concurrency): pure-API, own epoch shim.
    tokio::task::spawn_blocking(herd_phase).await.unwrap();
}

/// Clamped-mode admission is an atomic token RESERVATION, not
/// check-then-add: under real mount concurrency (256-deep O_DIRECT), a
/// thundering herd of simultaneous escalation attempts must not all pass
/// the bandwidth check before any of their spends land, and epoch-roll
/// boundaries must not re-open budget already spent (both measured on the
/// rig: ~6x the fill budget pre-reservation, 2x residual from the
/// boundary race). Kept a helper of the serial phases fn: the epoch shim
/// is process-global.
fn herd_phase() {
    use squeezefs::routing::TEST_ADMISSION_EPOCH_MS;
    const EPOCH_MS: u64 = 200;
    TEST_ADMISSION_EPOCH_MS.store(EPOCH_MS, Ordering::Relaxed);
    let gov = std::sync::Arc::new(squeezefs::routing::AdmissionGovernor::new(5));
    let block: u64 = 4 * 1024 * 1024;
    // Fund the PREVIOUS epoch: the grant mints at the roll from the
    // foreground the workload actually paid. 5 % of 8 GiB = ~409 MiB =>
    // ~102 blocks for the epoch.
    gov.note_foreground(8 * 1024 * 1024 * 1024);
    let budget_blocks = (8u64 * 1024 * 1024 * 1024 * 5 / 100) / block; // 102
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };
    // Wait for the next epoch boundary, then run the whole phase well
    // inside the fresh epoch (the herd itself is microseconds).
    let e0 = now_ms() / EPOCH_MS;
    while now_ms() / EPOCH_MS == e0 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Engage the clamp: waste ~= admitted (the churn steady state). The
    // first admissions ride unclamped (free) until the waste floor.
    for _ in 0..4 {
        assert!(gov.allow_escalation(block), "pre-clamp admissions flow");
        gov.on_eviction(
            block,
            &squeezefs::tiering::memory::EvictClass::Protected { served_bytes: 0 },
        );
    }

    let admitted = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..64 {
        let gov = gov.clone();
        let admitted = admitted.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..64 {
                if gov.allow_escalation(block) {
                    admitted.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let got = admitted.load(Ordering::Relaxed);
    assert!(
        got <= budget_blocks,
        "herd of 4096 concurrent attempts must never overshoot the \
         epoch's token grant (~{budget_blocks} blocks): admitted {got}"
    );
    assert!(
        got >= budget_blocks / 2,
        "the reservation must still spend most of the grant (got {got} of \
         ~{budget_blocks})"
    );
    TEST_ADMISSION_EPOCH_MS.store(0, Ordering::Relaxed);
}

/// Pure-API shard-geometry pin (env-free — safe under parallel test fns):
/// the min-shard constructor honors per-shard capacity floors.
#[test]
fn min_shard_constructor_reduces_shards() {
    let c = squeezefs::cache::lru::LruCache::with_capacity_min_shard(
        128 * 1024 * 1024,
        16 * 1024 * 1024,
    );
    assert!(
        c.num_shards() <= 8,
        "128 MiB / 16 MiB min shard ⇒ ≤ 8 shards, got {}",
        c.num_shards()
    );
    let tiny =
        squeezefs::cache::lru::LruCache::with_capacity_min_shard(1024 * 1024, 16 * 1024 * 1024);
    assert_eq!(tiny.num_shards(), 1, "min-shard floor never drops below 1");
}
