//! The transient stream window (read-saturation campaign, 2026-07-29 —
//! `.benchmarks/2026-07-29-read-saturation.md` §8): governor-arbitrated
//! stream RE-FILL admission with an honest payback basis.
//!
//! The field conviction this pins closed (user's 4-node 2×200GbE
//! cluster, sustained beyond-budget sequential infloop; reproduced on
//! the rsat nvmet-tcp rig): once ring reads classify and ride
//! whole-block fetches, every pass-2+ fill of the loop GHOST-HITS at the
//! fill site — the admission decision never consulted the classifier —
//! so definitionally-transient stream refills admitted PROTECTED into
//! the long-term tier: `read_admission_wasted_bytes` grew at ~4 GB/s
//! (~42 % of device reads; the field mount's 659 GB lifetime face), the
//! within-pass consumption credit diluted the governor's waste ratio
//! (holding the clamp open for random co-tenants — the scan-resistance
//! hole re-opened), and cache-ful mounts would re-pay the R1b publish
//! tax once per block per pass.
//!
//! The contract (design-read-path §5.3 amendment):
//! 1. A classified-streaming fill's ghost hit admits protected+publish
//!    ONLY through the admission governor (`allow_stream_admission` —
//!    same clamp + token reservation as the ranged escalation site;
//!    refusals count `read_admission_stream_transients`, never
//!    `read_admission_governor_denials`). Denied ⇒ the transient stream
//!    window: hot probation, publish skipped — first in eviction line,
//!    invisible to the waste ledger.
//! 2. Stream-admitted entries carry an honest payback basis: within-pass
//!    consumption (`get_serving`) credits NOTHING — probation would have
//!    served those sub-reads identically, so the admission's marginal
//!    value is cross-pass retention only. Victims report full shortfall
//!    (the clamp sees beyond-budget stream admissions as the waste they
//!    are) but are EXEMPT from `read_admission_evicted_unhit` (that
//!    tripwire keeps meaning admitted-and-never-touched).
//! 3. Stream fills fund the trickle: a foreground streaming fill is the
//!    workload's own device spend (`note_foreground`), so under clamp
//!    the admission trickle is bounded to `fill_pct` % (default 5) of
//!    the stream's own bandwidth — the governor's documented bound, now
//!    holding on the streaming shape too. Prefetch fills never
//!    self-fund (the ranged site's no-self-funding rule).
//! 4. Fitting / disk-tier-converging re-read streams still admit and
//!    converge (the 9.2-vs-16.6 GiB/s lineage guard): unclamped grants
//!    flow verbatim; under clamp the trickle + 2-epoch window release
//!    converge the set instead of freezing it out.
//!
//! Counter-asserting phases share ONE test fn per fixture (the churn
//! suite's counter-isolation discipline — METRICS is process-global).

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

/// Fixture: `staging` selects cache-ful (disk read tier present — the
/// lineage-guard shape) vs cache-less (the rig/field posture).
async fn make_with(uuid: [u8; 16], alloc_ns: &str, staging: bool) -> H {
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = if staging {
        Some(TempDir::new().unwrap())
    } else {
        None
    };
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
            hash_seed: 0xC0FF_EE00_5EA5_1DE5,
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

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, libc::O_DIRECT as u32)
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

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

/// One CONTIGUOUS sequential pass over `blocks` blocks in 128 KiB
/// requests (4 per block) — each read starts where the previous ended,
/// so the §5.3 lane classifies at request 4 and STAYS classified for
/// the whole pass, and every fill is fully consumed via the hot-tier
/// serve path (`get_serving`) like a real stream.
async fn stream_pass(h: &H, ino: u64, blocks: u64, expect: impl Fn(u64) -> u8) {
    let req = 128 * 1024u64;
    for i in 0..(blocks * BS / req) {
        let off = i * req;
        let d = read_at(h, ino, off, req as u32).await;
        let b = off / BS;
        assert!(
            d.iter().all(|&x| x == expect(b)),
            "byte parity at block {b} offset {off}"
        );
    }
}

fn wasted() -> u64 {
    METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed)
}
fn unhit() -> u64 {
    METRICS.read_admission_evicted_unhit.load(Ordering::Relaxed)
}
fn denials() -> u64 {
    METRICS
        .read_admission_governor_denials
        .load(Ordering::Relaxed)
}
fn transients() -> u64 {
    METRICS
        .read_admission_stream_transients
        .load(Ordering::Relaxed)
}
fn get_obj() -> u64 {
    METRICS.get_obj.load(Ordering::Relaxed)
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

// ---------------------------------------------------------------------------
// THE transient-window pin (cache-less — the rig/field posture): a
// beyond-budget classified stream looping its working set must not
// flood the governor's ledger with protected admissions. Pre-fix (tip
// 0e984a8): every pass-2+ fill ghost-hits → protected put → evicted
// with a payback shortfall → wasted_bytes grows by ~¼ block per block
// per pass and the clamp never engages (within-pass consumption
// dilutes the ratio). Post-fix: the first grants' full-shortfall
// victims engage the clamp, the remainder ride the transient window
// (probation, publish skipped, ledger-invisible), and the whole-block
// serve economy is untouched.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn beyond_budget_stream_loop_stays_transient_and_bounds_the_waste_ledger() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1"); // 2 slots
                                                                 // Read-lane campaign (2026-08-01): this phase pins the TRANSIENT
                                                                 // WINDOW mechanism — governor-arbitrated stream re-fill admission
                                                                 // at the fill site — which requires the refills to REACH the fill
                                                                 // site. With the read lane armed, this exact shape's pass
                                                                 // boundaries drop the per-lane resident share to 0 and the lane
                                                                 // covers the refills ledger-invisibly (never a ghost decision at
                                                                 // all; economy + bounded waste hold by construction — pinned in
                                                                 // tests/read_lane_tests.rs). Pin the pre-lane regime explicitly:
                                                                 // the transient window still governs every demand refill the lane
                                                                 // does not front-run (fitting shares, mixed shapes, lane-off
                                                                 // mounts).
    std::env::set_var("SQUEEZEFS_READ_LANE", "0");
    let h = make_with(*b"stream-trans-001", "strans_ns_a", false).await;
    std::env::remove_var("SQUEEZEFS_READ_LANE");
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let blocks = 16u64; // 8 MiB working set vs 1 MiB hot budget
    let (ino, _map) = striped_file(&h, "loop", blocks).await;

    // Pass 1: cold, unclassified→classified at request 4; first-touch
    // fills skip publish both pre- and post-fix (ghost first touch).
    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;

    // Passes 2..4: the sustained-loop regime — every fill ghost-hits.
    let (w0, u0, d0, t0, g0) = (wasted(), unhit(), denials(), transients(), get_obj());
    for _ in 0..3 {
        stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    }
    let refills = 3 * blocks;
    let (dw, du, dd, dt, dg) = (
        wasted() - w0,
        unhit() - u0,
        denials() - d0,
        transients() - t0,
        get_obj() - g0,
    );

    // The ledger bound: pre-fix every refill admitted protected and its
    // eviction reported a shortfall (~128 KiB × 48 refills ≈ 6 MiB).
    // Post-fix at most the pre-clamp grants' full shortfalls are in the
    // ledger — a handful of blocks, never per-refill growth.
    assert!(
        dw <= 4 * BS,
        "beyond-budget stream loop: wasted_bytes must be clamp-bounded, \
         not per-refill (got {dw} bytes across {refills} refills — the \
         field's 4 GB/s ledger-pollution face)"
    );
    // The transient window must actually engage (0 = the mechanism is
    // not wired; this is the new-counter face of the same pin).
    assert!(
        dt >= refills / 2,
        "the transient stream window must carry the bulk of the loop's \
         refills (read_admission_stream_transients = {dt} of {refills})"
    );
    // Instrument separation: stream transients are NOT ranged-path
    // governor denials, and stream victims are NOT the unhit tripwire.
    assert_eq!(
        dd, 0,
        "stream transients must never pollute read_admission_governor_denials"
    );
    assert!(
        du <= 2,
        "stream-admitted victims are exempt from the evicted-unhit \
         tripwire (admitted-and-never-touched keeps its meaning; got {du})"
    );
    // The serve economy is untouched: one whole-block fetch per block
    // per pass (the 1024×-amortization face the campaign shipped),
    // prefetch dedupe slack only.
    assert!(
        dg <= refills + refills / 2 + 4,
        "whole-block fetch economy must hold through the transient \
         window ({dg} device fetches for {refills} refills)"
    );
    drop(h);
}

// ---------------------------------------------------------------------------
// The lineage guard (cache-ful — the 9.2-vs-16.6 GiB/s measured shape):
// a re-read stream whose set exceeds the hot budget but fits the DISK
// tier must still converge — ghost-hit refills publish while grants
// flow (unclamped start + the trickle funded by the stream's own
// foreground spend + the 2-epoch window release), and once converged
// the set serves from the tier with the device flat. GREEN pre- and
// post-fix; it exists so the transient window can never be "fixed"
// into the full ghost bypass that measured 9.2.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fitting_disk_tier_reread_stream_still_converges() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1"); // 2 slots
    squeezefs::routing::TEST_ADMISSION_EPOCH_MS.store(100, Ordering::Relaxed);
    let h = make_with(*b"stream-trans-002", "strans_ns_b", true).await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let blocks = 6u64;
    let (ino, map) = striped_file(&h, "converge", blocks).await;

    // Re-read passes with the epoch seam at 100 ms: grants flow while
    // unclamped, the trickle + window release cover the rest. A bounded
    // deadline loop is convergence-waiting, not sleep-synchronization.
    //
    // Convergence, defined honestly: a full pass with the DEVICE FLAT —
    // the whole set serving from the tiers. Residency is legitimately
    // MIXED: disk-tier blocks (published grants) plus hot-RAM residents
    // (a block the 2-slot hot tier retains never re-fills, so it never
    // gets another publish decision — that is service, not starvation).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut converged = false;
    while std::time::Instant::now() < deadline {
        let g0 = get_obj();
        stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
        if get_obj() == g0 {
            converged = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        converged,
        "a disk-tier-fitting re-read stream must converge to tier service \
         (the 9.2-vs-16.6 lineage: a full stream ghost bypass never \
         converges and stays device-bound forever)"
    );
    // The disk tier must hold real convergence evidence (published stream
    // grants), not just hot-RAM luck: with a 2-slot hot tier and a
    // 6-block set, at least 4 blocks must have published.
    let tier_resident = (0..blocks as u32)
        .filter(|b| tier_has(&h, map.get(b).unwrap()))
        .count();
    assert!(
        tier_resident >= 4,
        "granted stream admissions must publish to the disk tier \
         ({tier_resident} of {blocks} resident)"
    );
    // Converged steady state holds: another pass, still device-flat.
    let g0 = get_obj();
    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    assert_eq!(
        get_obj() - g0,
        0,
        "converged re-read stream keeps serving from the tiers"
    );
    squeezefs::routing::TEST_ADMISSION_EPOCH_MS.store(0, Ordering::Relaxed);
    drop(h);
}

// ---------------------------------------------------------------------------
// Unit pins for the new tier/governor surface (new-API contracts; the
// integration red lives above): the stream-admitted entry's payback
// basis and the governor's stream-vs-ranged instrument separation.
// ---------------------------------------------------------------------------

#[test]
fn stream_admitted_entries_credit_no_within_pass_payback() {
    use squeezefs::tiering::memory::{EvictClass, MemoryCache};
    let c = MemoryCache::new(1024 * 1024, 1);
    let key = bytes::Bytes::from_static(b"stream-key");
    let val = bytes::Bytes::from(vec![0u8; 700 * 1024]);
    let ev = c.put_protected_stream(key.clone(), val);
    assert!(ev.is_empty(), "single entry fits the shard");
    // Within-pass consumption: get_serving credits NOTHING on a
    // stream-admitted entry (probation would have served identically).
    for _ in 0..4 {
        assert!(c.get_serving(&key, 128 * 1024).is_some());
    }
    // Displace it (the clock may spend a lap consuming the serve-time
    // `referenced` re-arms before the entry is evictable): the victim
    // must classify Protected with ZERO credit and carry the
    // stream_admitted marker for the unhit exemption.
    let mut evicted = Vec::new();
    for i in 0..4u8 {
        let k = bytes::Bytes::from(format!("displacer-{i}"));
        let v = bytes::Bytes::from(vec![i + 1; 700 * 1024]);
        evicted.extend(c.put_probationary(k, v));
        if evicted.iter().any(|(k, _, _)| k == &key) {
            break;
        }
    }
    let victim = evicted
        .iter()
        .find(|(k, _, _)| k == &key)
        .expect("the stream-admitted entry must be displaced");
    match victim.2 {
        EvictClass::Protected {
            served_bytes,
            stream_admitted,
        } => {
            assert_eq!(
                served_bytes, 0,
                "get_serving must not credit a stream-admitted entry — \
                 within-pass consumption is not re-read payback"
            );
            assert!(stream_admitted, "victim carries the stream marker");
        }
        EvictClass::Probation => panic!("stream admission is protected-class"),
    }
}

#[test]
fn governor_stream_denials_engage_clamp_and_stay_off_the_ranged_instrument() {
    use squeezefs::routing::AdmissionGovernor;
    use squeezefs::tiering::memory::EvictClass;
    let gov = AdmissionGovernor::new(5);
    let block = 512 * 1024u64;
    let (d0, t0, u0) = (denials(), transients(), unhit());

    // Full-shortfall stream victims: the honest basis — the clamp MUST
    // engage on them (pre-fix their within-pass credit diluted the
    // ratio and held the clamp open — the re-opened scan-resistance
    // hole).
    for _ in 0..4 {
        gov.on_eviction(
            block,
            &EvictClass::Protected {
                served_bytes: 0,
                stream_admitted: true,
            },
        );
    }
    assert!(
        !gov.allow_stream_admission(block),
        "all-waste stream admissions must clamp (no tokens: no foreground \
         spend recorded)"
    );
    assert_eq!(
        denials() - d0,
        0,
        "stream refusals never count read_admission_governor_denials"
    );
    assert!(
        transients() - t0 >= 1,
        "stream refusals count read_admission_stream_transients"
    );
    assert_eq!(
        unhit() - u0,
        0,
        "stream-admitted victims are exempt from the unhit tripwire"
    );

    // The ranged path on the same governor still reserves through
    // allow_escalation with ITS instrument.
    assert!(
        !gov.allow_escalation(block),
        "clamped + zero tokens denies the ranged escalation too"
    );
    assert!(
        denials() - d0 >= 1,
        "ranged refusals keep counting read_admission_governor_denials"
    );
}
