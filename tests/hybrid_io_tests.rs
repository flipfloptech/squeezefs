//! Hybrid I/O read policy (USER DIRECTIVE 2026-07-15: "hybrid I/O between
//! buffered and O_DIRECT, best of both worlds, regardless of if the user is
//! requesting O_DIRECT or not").
//!
//! The directive supersedes the read-path program's record-only stance for
//! ranged (rand-4k-class) misses (docs/design-read-path.md §5.6 "never
//! consulted for ranged dispatch") — the ghost table is exactly what makes
//! superseding it safe. Policy pinned here, O_DIRECT and buffered alike:
//!
//! 1. SERVE FROM TIER ON HIT — ALWAYS. O_DIRECT tier hits serve from RAM
//!    (binding-validated like every serve; the kernel page cache stays
//!    bypassed on the kernel's side — our tiers are ours). Counted:
//!    `read_odirect_tier_serves`.
//! 2. ADMISSION ON MISS — EVIDENCE-BASED, NOT BLIND. Ranged misses consult
//!    the ghost table: first touch = device-true window read + record (no
//!    admit — streaming pollution protection); a SECOND touch within the
//!    two-epoch ghost window ESCALATES to one whole-block fetch through the
//!    single-flight, which ghost-admits (protected hot put + validated NVMe
//!    publish) — re-read heat converges to RAM. Counted:
//!    `ranged_read_ghost_escalations` / `read_odirect_ghost_admits`.
//!    Re-admission churn is bounded by a per-key escalation COOLDOWN
//!    (~32–64 s window): beyond-tier working sets degrade to device-true
//!    ranged reads between windows instead of re-fetching + re-publishing
//!    4 MiB per eviction.
//! 3. BUDGET AUTHORITY UNCHANGED: mem-budget Red pauses the escalation
//!    (admission), never correctness; heat recording continues.
//! 4. DIAGNOSTIC ESCAPE (`-o direct_device_true` /
//!    `SQUEEZEFS_DIRECT_DEVICE_TRUE=1`): O_DIRECT reads become strictly
//!    device-true — NO tier serve, NO admission (no ghost record, no hot
//!    put, no publish), no pipeline classification — the measurement ruler
//!    for `.benchmarks` amplification methodology. Buffered traffic on the
//!    same mount is unaffected. Counted: `read_device_true_reads`.
//! 5. O_DIRECT WRITES UNCHANGED (durability, ack points, alignment,
//!    write-path publish policy — out of scope, pinned by the write suites).
//!
//! Counter-asserting phases share ONE test fn (`hybrid_phases`) — the churn
//! suite's counter-isolation discipline (`get_obj` and the ranged/odirect
//! counters are process-global).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS, STATS_INODE};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(uuid: [u8; 16], alloc_ns: &str) -> H {
    // Default W1 patch posture per test (a knob=0 CoW-machinery pin in a
    // prior test of this binary must never leak forward).
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

/// Write carrying O_DIRECT in the request's open flags (the daemon-side
/// shape of a kernel O_DIRECT write; the write path is deliberately
/// flag-agnostic — pinned by the coherence test).
async fn write_odirect_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            libc::O_DIRECT as u32,
        )
        .await
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short O_DIRECT write");
}

/// Read through the FUSE surface with explicit open flags (the per-request
/// O_DIRECT visibility plumb, landed in read-path PR 4).
async fn read_flags(h: &H, ino: u64, off: u64, size: u32, flags: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, flags)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    read_flags(h, ino, off, size, 0).await
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

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

/// Non-promoting probe — asserts must never perturb clock/class state.
fn hot_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.hot_block.get_no_promote(key).is_some()
}

const OD: u32 = libc::O_DIRECT as u32;

/// ALL counter-asserting phases in one fn (process-global counters).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hybrid_phases() {
    let h = make_with(*b"hybrid-io-a-v001", "hyb_ns_a").await;
    let ino = create(&h, "hyb_a").await;
    for b in 0..6u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();
    let k1 = map.get(&1).unwrap().clone();
    let k2 = map.get(&2).unwrap().clone();
    let k5 = map.get(&5).unwrap().clone();

    // ---- Phase A: O_DIRECT tier hit SERVES from RAM — device counter
    // flat, binding-validated like every serve, counted as an O_DIRECT
    // tier serve. (The kernel page cache stays bypassed kernel-side;
    // SqueezeFS's tiers are SqueezeFS's.)
    let d = read_at(&h, ino, 0, BS as u32).await; // buffered warm: hot probation
    assert!(d.iter().all(|&x| x == 1));
    assert!(hot_has(&h, &k0), "fixture: block 0 hot-resident");
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let ots0 = METRICS.read_odirect_tier_serves.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 4096, 4096, OD).await;
    assert!(d.iter().all(|&x| x == 1), "O_DIRECT tier-hit content");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0,
        "an O_DIRECT read of a tier-resident block must SERVE from the \
         tier — zero device reads (the 416–492k IOPS class's signature)"
    );
    assert!(
        METRICS.read_odirect_tier_serves.load(Ordering::Relaxed) > ots0,
        "O_DIRECT tier serves are counted (read_odirect_tier_serves)"
    );

    // ---- Phase B: first-touch rand-4k O_DIRECT miss stays DEVICE-TRUE —
    // one ranged window read, no admission anywhere (the anti-pollution
    // half of the policy: blind first-touch admission is what the ghost
    // table exists to prevent).
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 5 * BS + 8192, 4096, OD).await;
    assert!(d.iter().all(|&x| x == 6), "first-touch content");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        1,
        "first touch = exactly one device read"
    );
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        1,
        "first touch is a ranged window read (device-true, 1.00×)"
    );
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        esc0,
        "first touch must NOT escalate"
    );
    assert!(!tier_has(&h, &k5), "first touch admits nothing (disk tier)");
    assert!(!hot_has(&h, &k5), "first touch admits nothing (hot tier)");

    // ---- Phase C: SECOND touch within the ghost window ESCALATES — one
    // whole-block fetch, ghost-admitted (protected hot + validated NVMe
    // publish); the third touch serves from RAM with the device flat.
    // The warm-up arithmetic pinned: 2 device ops for the block's whole
    // lifetime, everything after is RAM.
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let oga0 = METRICS.read_odirect_ghost_admits.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, BS, 4096, OD).await; // touch 1: record
    assert!(d.iter().all(|&x| x == 2));
    let d = read_flags(&h, ino, BS + 32_768, 4096, OD).await; // touch 2: escalate
    assert!(d.iter().all(|&x| x == 2));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed)
            - esc0,
        1,
        "the second touch within the ghost window escalates to ONE \
         whole-block fetch (evidence-based admission)"
    );
    assert!(
        METRICS.read_odirect_ghost_admits.load(Ordering::Relaxed) > oga0,
        "O_DIRECT-initiated ghost admissions are counted"
    );
    assert!(
        tier_has(&h, &k1),
        "the escalated fill ghost-admits: validated NVMe publish"
    );
    assert!(
        hot_has(&h, &k1),
        "the escalated fill lands hot (protected — proven warmth)"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        2,
        "warm-up cost: 1 ranged window + 1 whole block, nothing more"
    );
    let g1 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, BS + 65_536, 4096, OD).await; // touch 3: RAM
    assert!(d.iter().all(|&x| x == 2));
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g1,
        "post-admission O_DIRECT reads serve from RAM — device flat"
    );

    // ---- Phase C2: escalation-churn bound (the cooldown). A key that
    // just escalated must NOT re-escalate within the cooldown window even
    // if its admitted copy is evicted while its ghost entry is still hot
    // — on working sets beyond the tiers every eviction would otherwise
    // re-fetch + re-publish 4 MiB per ranged touch (the R-5 spiral class
    // in tier form). Between windows the workload degrades to the
    // device-true ranged path.
    h.fs.router.cache.purge_block_key(&k1); // simulate eviction of the admitted copy
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let rr0 = METRICS.ranged_reads.load(Ordering::Relaxed);
    for i in 0..3u64 {
        let d = read_flags(&h, ino, BS + 131_072 + i * 4096, 4096, OD).await;
        assert!(d.iter().all(|&x| x == 2), "cooldown-window content");
    }
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        esc0,
        "a just-escalated key must not re-escalate within the cooldown \
         window (bounded re-admission churn)"
    );
    assert_eq!(
        METRICS.ranged_reads.load(Ordering::Relaxed) - rr0,
        3,
        "cooled-down touches stay ranged device reads"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        3,
        "cooled-down touches are device-true windows"
    );

    // ---- Phase D: budget-Red pauses the escalation (admission rides the
    // existing mem-budget arbitration; heat RECORDING continues, exactly
    // like the whole-block fill site under the §5.7 publish pause).
    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Red);
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 2 * BS, 4096, OD).await; // touch 1 (records)
    assert!(d.iter().all(|&x| x == 3));
    let d = read_flags(&h, ino, 2 * BS + 32_768, 4096, OD).await; // ghost hit, Red
    assert!(d.iter().all(|&x| x == 3));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        esc0,
        "Red pauses O_DIRECT admission: a ghost hit must NOT escalate"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        2,
        "under Red both touches stay ranged device reads"
    );
    assert!(!tier_has(&h, &k2), "nothing admitted under Red");
    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Green);
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 2 * BS + 65_536, 4096, OD).await; // Green: escalate
    assert!(d.iter().all(|&x| x == 3));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed)
            - esc0,
        1,
        "back at Green the recorded heat admits on the next touch"
    );
    assert!(tier_has(&h, &k2), "post-Red admission converges");

    // ---- Phase E: buffered traffic rides the SAME policy (hybrid is not
    // O_DIRECT-special-cased): second buffered 4k touch escalates too.
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let d = read_at(&h, ino, 3 * BS, 4096).await; // touch 1
    assert!(d.iter().all(|&x| x == 4));
    let d = read_at(&h, ino, 3 * BS + 32_768, 4096).await; // touch 2
    assert!(d.iter().all(|&x| x == 4));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed)
            - esc0,
        1,
        "buffered ranged misses use the same evidence-based admission"
    );

    // ---- Phase F: the stats surface carries the new fields (mode +
    // adoption counters) — the operator-visible contract.
    let stats = String::from_utf8(read_at(&h, STATS_INODE, 0, 1 << 20).await).unwrap();
    for field in [
        "\"read_odirect_tier_serves\"",
        "\"read_odirect_ghost_admits\"",
        "\"ranged_read_ghost_escalations\"",
        "\"read_device_true_reads\"",
        "\"direct_device_true\"",
    ] {
        assert!(
            stats.contains(field),
            "stats inode must expose {field} (hybrid I/O observability)"
        );
    }
}

/// The diagnostic escape (`SQUEEZEFS_DIRECT_DEVICE_TRUE=1` /
/// `-o direct_device_true`): O_DIRECT reads are strictly device-true —
/// no tier serve, no admission, no ghost recording — while buffered
/// traffic on the same mount keeps the full hybrid behavior. Pinned
/// against the current counters: this is the measurement ruler for the
/// `.benchmarks` amplification methodology.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escape_direct_device_true_is_device_true() {
    std::env::set_var("SQUEEZEFS_DIRECT_DEVICE_TRUE", "1");
    let h = make_with(*b"hybrid-io-e-v001", "hyb_ns_e").await;
    std::env::remove_var("SQUEEZEFS_DIRECT_DEVICE_TRUE");
    assert!(
        h.fs.router.direct_device_true(),
        "env knob must arm the escape at router construction"
    );

    let ino = create(&h, "hyb_e").await;
    for b in 0..3u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 0x11; BS as usize]).await;
    }
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();
    let k1 = map.get(&1).unwrap().clone();

    // Warm block 0 hot via a BUFFERED read (buffered keeps hybrid).
    let d = read_at(&h, ino, 0, BS as u32).await;
    assert!(d.iter().all(|&x| x == 0x11));
    assert!(hot_has(&h, &k0), "fixture: block 0 hot-resident");

    // NO-SERVE: an O_DIRECT read of the hot-resident block goes to the
    // DEVICE (byte-identical to the pre-hybrid device-true posture).
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let hh0 = METRICS.hot_block_hits.load(Ordering::Relaxed);
    let dtr0 = METRICS.read_device_true_reads.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, 4096, 4096, OD).await;
    assert!(d.iter().all(|&x| x == 0x11), "device-true content");
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        1,
        "escape mode: O_DIRECT reads the DEVICE even when tier-resident"
    );
    assert_eq!(
        METRICS.hot_block_hits.load(Ordering::Relaxed),
        hh0,
        "escape mode: no hot-tier serve for O_DIRECT"
    );
    assert!(
        METRICS.read_device_true_reads.load(Ordering::Relaxed) > dtr0,
        "escape adoption is observable (read_device_true_reads)"
    );

    // NO-PUBLISH + NO-GHOST: repeated O_DIRECT 4k touches of one cold
    // block never escalate, never admit, never record.
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    for i in 0..4u64 {
        let off = BS + i * 32_768;
        let d = read_flags(&h, ino, off, 4096, OD).await;
        assert!(d.iter().all(|&x| x == 0x12), "content at off {off}");
    }
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        4,
        "escape mode: N O_DIRECT touches = N device reads (1.00×, forever)"
    );
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        esc0,
        "escape mode: no escalation"
    );
    assert!(!tier_has(&h, &k1), "escape mode: no disk-tier publish");
    assert!(!hot_has(&h, &k1), "escape mode: no hot put");

    // BUFFERED UNAFFECTED on the same mount: hot serve, device flat.
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_at(&h, ino, 65_536, 4096).await;
    assert!(d.iter().all(|&x| x == 0x11));
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed),
        g0,
        "buffered reads still serve from the tier under the escape"
    );

    // NO-GHOST pinned end-to-end: flip the escape OFF (the mount-option
    // setter path) — block 1's O_DIRECT touches above must have left NO
    // heat: the next touch is a FIRST touch (ranged, no escalation), and
    // only the one after that admits.
    h.fs.router.set_direct_device_true(false);
    assert!(!h.fs.router.direct_device_true());
    let esc0 = METRICS
        .ranged_read_ghost_escalations
        .load(Ordering::Relaxed);
    let g0 = METRICS.get_obj.load(Ordering::Relaxed);
    let d = read_flags(&h, ino, BS + 200_704, 4096, OD).await; // first touch
    assert!(d.iter().all(|&x| x == 0x12));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed),
        esc0,
        "escape-mode touches recorded no ghost heat — hybrid restart is a \
         first touch"
    );
    let d = read_flags(&h, ino, BS + 233_472, 4096, OD).await; // second touch
    assert!(d.iter().all(|&x| x == 0x12));
    assert_eq!(
        METRICS
            .ranged_read_ghost_escalations
            .load(Ordering::Relaxed)
            - esc0,
        1,
        "hybrid resumes verbatim once the escape is dropped"
    );
    assert_eq!(
        METRICS.get_obj.load(Ordering::Relaxed) - g0,
        2,
        "first touch ranged + second touch whole-block"
    );
}

/// `-o direct_device_true` is a DAEMON-level option: it must be consumed
/// by the daemon and stripped from the kernel mount string (the kernel
/// would reject it), exactly like the TTL options (post-M5 convention).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mount_option_is_daemon_level_and_stripped() {
    let parsed = squeezefs::fuse_client::parse_custom_options(
        "allow_other,direct_device_true,max_read=1048576",
    );
    let parsed = parsed.to_string_lossy().to_string();
    assert!(
        !parsed.contains("direct_device_true"),
        "direct_device_true must be stripped from the unprivileged kernel \
         option string (got: {parsed})"
    );
    assert!(parsed.contains("allow_other") && parsed.contains("max_read=1048576"));

    let filtered =
        squeezefs::fuse_client::filter_kernel_mount_options("direct_device_true,max_read=1048576");
    assert!(
        !filtered.contains("direct_device_true"),
        "root-path kernel options must not carry the daemon flag"
    );
}

/// The generic/091 shape as a cargo test: mixed O_DIRECT + buffered I/O on
/// ONE file stays coherent in both directions, INCLUDING through the new
/// hybrid serve side (a warmed tier entry must never outlive its block's
/// COW displacement).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_direct_buffered_coherence_091_shape() {
    let h = make_with(*b"hybrid-io-c-v001", "hyb_ns_c").await;
    // This test pins the hybrid warm/purge machinery across COW
    // DISPLACEMENT (a warmed entry dies with its incarnation) — its warm
    // fixture assumes each overwrite mints a FRESH key with a clean
    // ghost/escalation-cooldown slate. Under the W1 sole-owner patch (RW2)
    // this 512 KiB-block shape patches IN PLACE: the key never displaces,
    // so the per-key `EscalationCooldown` from the test's own earlier
    // phases suppresses the second-touch re-admission window (FIND-RW2-B —
    // a bounded admission-heuristic effect, not a correctness one: reads
    // stay device-correct via the ranged path; the G-RW3 mixed rand-R/W +
    // hybrid warm rows measure the live impact). Pin the CoW machinery
    // explicitly; extent_patch_tests owns patched-shape read coherence.
    squeezefs::fuse_client::set_patch_max_bytes(0);

    let ino = create(&h, "hyb_c").await;

    // Buffered write → O_DIRECT read sees it (pre-durable: overlay serve).
    write_at(&h, ino, 0, &vec![0xA1u8; BS as usize]).await;
    write_at(&h, ino, BS, &vec![0xA2u8; BS as usize]).await;
    let d = read_flags(&h, ino, 0, 4096, OD).await;
    assert!(
        d.iter().all(|&x| x == 0xA1),
        "buffered write → O_DIRECT read (pre-fsync overlay)"
    );

    // O_DIRECT write → buffered read sees it.
    write_odirect_at(&h, ino, 0, &vec![0xB1u8; BS as usize]).await;
    let d = read_at(&h, ino, 0, 4096).await;
    assert!(
        d.iter().all(|&x| x == 0xB1),
        "O_DIRECT write → buffered read"
    );

    // Durable + cold, then WARM the tier through hybrid O_DIRECT admission.
    let map = make_cold(&h, ino).await;
    let k0 = map.get(&0).unwrap().clone();
    let d = read_flags(&h, ino, 0, 4096, OD).await; // touch 1
    assert!(d.iter().all(|&x| x == 0xB1));
    let d = read_flags(&h, ino, 32_768, 4096, OD).await; // touch 2: admit
    assert!(d.iter().all(|&x| x == 0xB1));
    assert!(
        tier_has(&h, &k0) || hot_has(&h, &k0),
        "fixture: block 0 warmed through hybrid admission"
    );

    // Buffered overwrite of the warmed block → O_DIRECT read must see the
    // NEW bytes immediately (overlay precedence) …
    write_at(&h, ino, 0, &vec![0xC1u8; BS as usize]).await;
    let d = read_flags(&h, ino, 0, 4096, OD).await;
    assert!(
        d.iter().all(|&x| x == 0xC1),
        "overwrite visible to O_DIRECT through the overlay"
    );

    // … and after the COW displacement lands (fsync), the stale warmed
    // entry must be purged/rebound — an O_DIRECT read must NEVER serve the
    // displaced incarnation from the tier it warmed (the new-serve-side
    // 091/074-family pin).
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    for probe_off in [0u64, 32_768, 65_536, 131_072] {
        let d = read_flags(&h, ino, probe_off, 4096, OD).await;
        assert!(
            d.iter().all(|&x| x == 0xC1),
            "stale tier serve after COW displacement at off {probe_off} \
             (hybrid warmth must die with its incarnation)"
        );
        let d = read_at(&h, ino, probe_off, 4096).await;
        assert!(
            d.iter().all(|&x| x == 0xC1),
            "buffered agrees at off {probe_off}"
        );
    }

    // Sibling block untouched by the overwrite stays intact.
    let d = read_flags(&h, ino, BS + 4096, 4096, OD).await;
    assert!(d.iter().all(|&x| x == 0xA2), "sibling block unperturbed");
}

/// Scoreboard loss-1 follow-up (2026-07-18,
/// `.benchmarks/2026-07-18-multi-reference-scoreboard.md` §Loss 1): the
/// inaugural R2 `seq_write_1m` row correlated `-o direct_device_true` with
/// a halved write-phase device drain. Root-caused as NOT causal — the
/// escape's only consumers are the three read-side sites
/// (`src/routing.rs`), the original row's own `.stats` deltas show the
/// write path executed identical work flag-on vs flag-off, and targeted
/// A/B measurement shows drain parity (report addendum, 2026-07-18).
///
/// This test PINS that innocence as the standing contract: **the escape
/// must never gate, serialize, or reroute the write/flush path.** It runs
/// one identical write script twice — escape off, then escape armed via
/// the mount-option path — and requires byte-identical write-side
/// routing:
///
/// - identical write-through / patch / layout / flush-spill counter
///   deltas (a flag-coupled route change diverges here — 6 write-through
///   blocks vs 0 is the loudest failure shape);
/// - ZERO device reads and ZERO `write_path_seed_read_bytes` inside the
///   write window in both modes (pins the chartered hypothesis "the flag
///   makes drains serial-read-bound via RMW seed serving" to dead);
/// - ZERO `read_device_true_reads` movement during pure writes in both
///   modes (the escape's consumers are read-only — a write-side consumer
///   regression trips this), while a post-window O_DIRECT read proves the
///   armed fixture's escape was genuinely live;
/// - identical durable content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escape_never_touches_the_write_path() {
    #[derive(Debug, PartialEq, Eq)]
    struct WriteSideDeltas {
        write_through_blocks: u64,
        write_through_bytes: u64,
        patch_writes: u64,
        patch_write_bytes: u64,
        layout_striped_writes: u64,
        get_obj: u64,
        write_path_seed_read_bytes: u64,
        staging_put_bytes_flush: u64,
        writeback_enqueued_flush: u64,
        read_device_true_reads: u64,
    }

    fn counters() -> [u64; 10] {
        [
            METRICS.write_through_blocks.load(Ordering::Relaxed),
            METRICS.write_through_bytes.load(Ordering::Relaxed),
            METRICS.patch_writes.load(Ordering::Relaxed),
            METRICS.patch_write_bytes.load(Ordering::Relaxed),
            METRICS.layout_striped_writes.load(Ordering::Relaxed),
            METRICS.get_obj.load(Ordering::Relaxed),
            METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed),
            METRICS.staging_put_bytes_flush.load(Ordering::Relaxed),
            METRICS.writeback_enqueued_flush.load(Ordering::Relaxed),
            METRICS.read_device_true_reads.load(Ordering::Relaxed),
        ]
    }

    /// One write script = the loss row's shapes at fixture scale: striped
    /// base, full-block O_DIRECT overwrites (the write-through drain), an
    /// aligned interior O_DIRECT small overwrite (the W1 patch shape), a
    /// partial tail + fsync (the staging-spill/flush leg).
    async fn run_script(h: &H, name: &str) -> WriteSideDeltas {
        let ino = create(h, name).await;
        for b in 0..6u64 {
            write_at(h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
        }
        make_cold(h, ino).await;

        let c0 = counters();
        // Patch lever OFF for the overwrite leg so every full-block
        // O_DIRECT overwrite deterministically takes the accumulation →
        // coverage-complete → write-through route (the R2 row's 4,064-
        // block drain path), not the in-place patch.
        squeezefs::fuse_client::set_patch_max_bytes(0);
        for b in 0..6u64 {
            write_odirect_at(h, ino, b * BS, &vec![b as u8 + 0x21; BS as usize]).await;
        }
        // Patch posture back to default for the small-overwrite shape.
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        write_odirect_at(h, ino, 2 * BS + 8192, &vec![0x77u8; 4096]).await;
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        // Partial tail: parks, then spills/flushes at fsync.
        write_at(h, ino, 6 * BS, &vec![0x55u8; (BS / 4) as usize]).await;
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        let c1 = counters();

        // Durable content (identical route ⇒ identical bytes) — read
        // AFTER the measured window (buffered, so no device-true motion).
        let d = read_at(h, ino, 5 * BS, 4096).await;
        assert!(
            d.iter().all(|&x| x == 5 + 0x21),
            "{name}: overwritten block bytes"
        );
        let d = read_at(h, ino, 2 * BS + 8192, 4096).await;
        assert!(d.iter().all(|&x| x == 0x77), "{name}: patched bytes");
        let d = read_at(h, ino, 2 * BS, 4096).await;
        assert!(
            d.iter().all(|&x| x == 0x23),
            "{name}: unpatched bytes of the patched block"
        );
        let d = read_at(h, ino, 6 * BS, 4096).await;
        assert!(d.iter().all(|&x| x == 0x55), "{name}: tail bytes");

        WriteSideDeltas {
            write_through_blocks: c1[0] - c0[0],
            write_through_bytes: c1[1] - c0[1],
            patch_writes: c1[2] - c0[2],
            patch_write_bytes: c1[3] - c0[3],
            layout_striped_writes: c1[4] - c0[4],
            get_obj: c1[5] - c0[5],
            write_path_seed_read_bytes: c1[6] - c0[6],
            staging_put_bytes_flush: c1[7] - c0[7],
            writeback_enqueued_flush: c1[8] - c0[8],
            read_device_true_reads: c1[9] - c0[9],
        }
    }

    // Phase A — escape OFF (default posture). The baseline arm-proof
    // probe (an O_DIRECT read staying hybrid) runs before the fixture is
    // dropped; fixtures must not overlap (fresh volumes mint the same ino
    // sequence, and per-ino process globals — DLM object locks, stripe
    // shards — are keyed by ino).
    let base;
    let dtr_probe0 = METRICS.read_device_true_reads.load(Ordering::Relaxed);
    {
        let h_off = make_with(*b"hybrid-io-w-v001", "hyb_ns_w_off").await;
        assert!(!h_off.fs.router.direct_device_true());
        base = run_script(&h_off, "wa_off").await;

        let ino_off = create(&h_off, "wa_off_probe").await;
        for b in 0..3u64 {
            write_at(&h_off, ino_off, b * BS, &vec![0xEEu8; BS as usize]).await;
        }
        make_cold(&h_off, ino_off).await;
        let d = read_flags(&h_off, ino_off, 0, 4096, OD).await;
        assert!(d.iter().all(|&x| x == 0xEE));
        assert_eq!(
            METRICS.read_device_true_reads.load(Ordering::Relaxed),
            dtr_probe0,
            "baseline fixture: O_DIRECT reads stay hybrid"
        );
    }

    // Phase B — escape ARMED via the mount-option path (`start_mount`'s
    // `-o direct_device_true` setter).
    let h_on = make_with(*b"hybrid-io-w-v002", "hyb_ns_w_on").await;
    h_on.fs.router.set_direct_device_true(true);
    assert!(h_on.fs.router.direct_device_true());
    let armed = run_script(&h_on, "wa_on").await;

    // THE contract: byte-identical write-side routing.
    assert_eq!(
        base, armed,
        "the direct_device_true escape must be write-path inert: identical \
         script ⇒ identical write-side counter deltas"
    );

    // The write-through drain leg genuinely ran (10 full blocks), with no
    // device reads, no seed reads, and no device-true motion — in BOTH
    // modes (equality above makes one set of absolutes cover both).
    assert_eq!(base.write_through_blocks, 6, "write-through drain leg");
    assert_eq!(base.write_through_bytes, 6 * BS, "write-through bytes");
    assert_eq!(base.get_obj, 0, "pure writes never read the device");
    assert_eq!(
        base.write_path_seed_read_bytes, 0,
        "write-path seed-read tripwire (must stay 0 — AGENTS.md)"
    );
    assert_eq!(
        base.read_device_true_reads, 0,
        "the escape's consumers are read-side only; a write-side consumer \
         would move this during a pure-write window"
    );

    // Arm proof: the SAME O_DIRECT-read probe was hybrid on the baseline
    // fixture (asserted above) and is device-true here — the phases' one
    // difference is demonstrably live, so the equality above is a real
    // A/B.
    let dtr1 = METRICS.read_device_true_reads.load(Ordering::Relaxed);
    let ino_on = create(&h_on, "wa_on_probe").await;
    for b in 0..3u64 {
        write_at(&h_on, ino_on, b * BS, &vec![0xEDu8; BS as usize]).await;
    }
    make_cold(&h_on, ino_on).await;
    let d = read_flags(&h_on, ino_on, 0, 4096, OD).await;
    assert!(d.iter().all(|&x| x == 0xED));
    assert!(
        METRICS.read_device_true_reads.load(Ordering::Relaxed) > dtr1,
        "armed fixture: the escape is live (device-true read counted)"
    );
}
