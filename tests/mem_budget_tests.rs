//! R5 — the joint memory authority (docs/design-read-path.md §5.7 / PR 7).
//!
//! Contracts pinned (red-first):
//! - Budget resolution order: flag → env → cgroup×0.8 → 70 % RAM (pure fn).
//! - Floor validation: Σ floors ≤ 0.9 × budget, else proportional clamp
//!   (loud, never a mount failure).
//! - Pressure = max(Σ component gauges, windowed-max RSS over 5 samples) —
//!   a decaying window, explicitly NOT a ratchet (spike ages out in 5
//!   ticks).
//! - Level transitions with hysteresis (enter 80/95, exit 76/91) and
//!   entry-edge event counters (no re-count inside a band).
//! - Red sheds to weights: per-component targets proportional to weight
//!   over the floor, floors respected, called once per Red tick, only for
//!   components above target.
//! - `level()` is a single relaxed atomic load usable from every growth
//!   path (&self, immediate visibility, no lock).
//! - Advisory integration: Yellow pauses dehydration entirely (protected
//!   victims DROPPED, not written); Yellow freezes prefetch window growth;
//!   Red stops prefetch issue; Red halves the parked-buffer cap.
//!
//! Convergence-under-admission-storm contracts (the 2026-07-12 saturation-
//! suite cage-OOM class, `.benchmarks/2026-07-12-saturation-suite-oom-
//! finding2.md`: Red sheds fired but gauge_sum rode 2.1 GiB over budget for
//! 100+ s until the kernel killed 7.94 GiB of anon — advisory-at-admission
//! was outrun by the two heaviest growth paths):
//! - Pressure gains a THIRD arm: windowed cgroup-v2 unreclaimable bytes
//!   (anon + dirty + writeback + shmem + unevictable + unreclaimable slab)
//!   — the kill-relevant set the statm sampler under-counts (dirty page
//!   cache is invisible to statm; clean cache is reclaimable and must NOT
//!   count). Same 5-slot decaying window as RSS, never a ratchet.
//! - Disk-tier publishes PAUSE while the unreclaimable arm sits in the Red
//!   band (enter ≥ 95 %, release < 91 % — the dirty-flood signature); the
//!   tier is a read cache, skipping a publish is never-lossy.
//! - HARD BACKSTOP (§5.7 Red semantics escalation): the unreclaimable arm
//!   ≥ 100 % of budget for 3 consecutive ticks escalates Red sheds from
//!   the 85 % target to the FLOORS and is counted (`hard_backstops`) —
//!   the defense that holds even when a component gauge undercounts.
//! - Red parked-buffer admission BLOCKS (awaited drain backpressure, the
//!   §4.4-pt-5 ring-admission precedent — never a spin, never a held-lock
//!   wait): past the halved cap with staging refusing spills, the writer
//!   awaits the never-lossy drain instead of parking unbounded (measured
//!   pre-fix: 1,937 parked buffers = 7.6 GiB anon against a 256 cap).
//!
//! Counter-asserting integration phases share one test fn (process-global
//! counters + the process-global level — churn-suite counter-isolation
//! discipline). Pure-arithmetic tests use private `MemBudget` instances.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::mem_budget::{self, Component, Level, MemBudget};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, NamedTempFile, TempDir};

fn comp(
    name: &'static str,
    gauge: Arc<AtomicU64>,
    floor: u64,
    weight: u64,
    targets: Arc<Mutex<Vec<(&'static str, u64)>>>,
) -> Component {
    let g = gauge.clone();
    Component::new(
        name,
        floor,
        weight,
        Arc::new(move || g.load(Ordering::Relaxed)),
        Arc::new(move |target| targets.lock().unwrap().push((name, target))),
    )
}

// ---------------------------------------------------------------------------
// Pure arithmetic (private instances — no process-global interference)
// ---------------------------------------------------------------------------

#[test]
fn budget_resolution_order_flag_env_cgroup_ram() {
    let gib = 1024 * 1024 * 1024u64;
    // Flag wins over everything.
    assert_eq!(
        mem_budget::resolve_budget_from(Some(2 * gib), Some(3 * gib), Some(8 * gib), 64 * gib),
        2 * gib
    );
    // Env next.
    assert_eq!(
        mem_budget::resolve_budget_from(None, Some(3 * gib), Some(8 * gib), 64 * gib),
        3 * gib
    );
    // Cgroup memory.max × 0.8.
    assert_eq!(
        mem_budget::resolve_budget_from(None, None, Some(10 * gib), 64 * gib),
        8 * gib
    );
    // RAM fraction fallback: 70 %.
    assert_eq!(
        mem_budget::resolve_budget_from(None, None, None, 10 * gib),
        7 * gib
    );
}

#[test]
fn floor_sum_clamp_is_proportional_and_loud() {
    let b = MemBudget::new_for_test();
    let t = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("a", Arc::new(AtomicU64::new(0)), 600, 1, t.clone()));
    b.register(comp("b", Arc::new(AtomicU64::new(0)), 300, 1, t.clone()));
    // Σ floors = 900 ≤ 0.9 × 1000 — no clamp.
    let f = b.effective_floors(1000);
    assert_eq!(f, vec![600, 300]);
    assert!(!b.floors_clamped());
    // Σ floors = 900 > 0.9 × 800 = 720 — proportional clamp (× 0.8).
    let f = b.effective_floors(800);
    assert_eq!(f, vec![480, 240], "proportional, not truncation-ordered");
    assert!(b.floors_clamped(), "clamp must be observable (loud)");
}

#[test]
fn level_transitions_hysteresis_and_entry_events() {
    let b = MemBudget::new_for_test();
    let g = Arc::new(AtomicU64::new(0));
    let t = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("g", g.clone(), 0, 1, t));

    let step = |pressure: u64, expect: Level| {
        g.store(pressure, Ordering::Relaxed);
        b.tick_inner(1000, 0, 0);
        assert_eq!(b.level(), expect, "pressure {pressure}");
    };

    step(790, Level::Green);
    step(800, Level::Yellow); // enter at >= 80%
    step(940, Level::Yellow);
    step(950, Level::Red); // enter at >= 95%
    step(920, Level::Red); // hysteresis: stays Red until < 91%
    step(905, Level::Yellow); // exit Red below 91%
    step(770, Level::Yellow); // hysteresis: stays Yellow until < 76%
    step(759, Level::Green); // exit Yellow below 76%
    step(800, Level::Yellow);
    step(700, Level::Green);

    assert_eq!(
        b.yellow_events(),
        2,
        "yellow entry edges only (no re-count inside the band)"
    );
    assert_eq!(b.red_events(), 1, "red entry edges only");
}

#[test]
fn windowed_rss_max_decays_not_ratchets() {
    let b = MemBudget::new_for_test();
    // No components: pressure = windowed RSS max alone.
    b.tick_inner(1000, 960, 0);
    assert_eq!(b.level(), Level::Red, "spike enters Red");
    for i in 0..4 {
        b.tick_inner(1000, 100, 0);
        assert_eq!(
            b.level(),
            Level::Red,
            "tick {i}: 5-sample window still holds the spike"
        );
    }
    b.tick_inner(1000, 100, 0);
    assert_eq!(
        b.level(),
        Level::Green,
        "spike aged out after 5 samples — a ratchet would pin Red forever"
    );
    assert!(b.pressure_bytes() <= 100);
}

#[test]
fn red_sheds_to_weights_over_floors_once_per_tick() {
    let b = MemBudget::new_for_test();
    let ga = Arc::new(AtomicU64::new(500));
    let gb = Arc::new(AtomicU64::new(400));
    let gc = Arc::new(AtomicU64::new(80));
    let targets = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("a", ga, 100, 3, targets.clone()));
    b.register(comp("b", gb, 100, 1, targets.clone()));
    // c sits at its floor — must receive NO shed call.
    b.register(comp("c", gc, 80, 1, targets.clone()));

    // pressure 980 / budget 1000 = 98% ⇒ Red. Shed target-total = 85% of
    // budget ⇒ excess 130, split by weight over the sheddable set
    // {a:3, b:1, c:1 → but c is at floor}.
    b.tick_inner(1000, 0, 0);
    assert_eq!(b.level(), Level::Red);
    let t = targets.lock().unwrap().clone();
    assert_eq!(t.len(), 2, "exactly one shed call per over-floor component");
    let a = t.iter().find(|(n, _)| *n == "a").expect("a shed").1;
    let bb = t.iter().find(|(n, _)| *n == "b").expect("b shed").1;
    assert!(
        t.iter().all(|(n, _)| *n != "c"),
        "a component at its floor gets no shed call"
    );
    // Weight-proportional over floors: a takes 3/4 of the excess, b 1/4.
    assert!(a >= 100 && bb >= 100, "targets never below floors");
    let cut_a = 500 - a;
    let cut_b = 400 - bb;
    assert!(
        (cut_a as i64 - 3 * cut_b as i64).abs() <= 3,
        "weight proportionality: cut_a {cut_a} ≈ 3 × cut_b {cut_b}"
    );
    assert_eq!(cut_a + cut_b, 130, "sheds sum to the excess over 85%");

    // Yellow must NOT shed.
    targets.lock().unwrap().clear();
    b.tick_inner(1200, 0, 0); // 980/1200 = 82% ⇒ Yellow
    assert_eq!(b.level(), Level::Yellow);
    assert!(
        targets.lock().unwrap().is_empty(),
        "Yellow is advisory-only: growth stops, nothing sheds"
    );
}

#[test]
fn level_read_is_lock_free_and_immediately_visible() {
    let b = Arc::new(MemBudget::new_for_test());
    let g = Arc::new(AtomicU64::new(990));
    b.register(comp("g", g, 0, 1, Arc::new(Mutex::new(Vec::new()))));
    // Concurrent readers during ticks: never blocks, always a valid level.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let b = b.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..10_000 {
                let l = b.level();
                assert!(matches!(l, Level::Green | Level::Yellow | Level::Red));
            }
        }));
    }
    for _ in 0..100 {
        b.tick_inner(1000, 0, 0);
    }
    for h in handles {
        h.join().unwrap();
    }
    // Immediate visibility (&self store → &self load, no locks in the API).
    assert_eq!(b.level(), Level::Red);
}

#[test]
fn dehydration_channel_is_byte_bounded_at_the_send_side() {
    // The QUICK cage-OOM class, root-caused (p7_quick_trace2): the
    // dehydration channel held up to 16,384 full payloads of LIVE Bytes
    // (multi-GiB anon in <30 s during the 617 fsx soak) — ungauged, so
    // the authority sat Green while RSS ramped 2.1 -> 7.5 GiB. The
    // channel must be BYTE-bounded at the send side: past the bound,
    // victims are dropped (dehydration is best-effort warmth — the PR 4
    // source-drop precedent), the parked-bytes gauge never exceeds the
    // bound, and draining restores admission.
    use squeezefs::cache::lru::LruCache;
    let cache = LruCache::with_capacity(8 * 1024 * 1024);
    let payload = bytes::Bytes::from(vec![7u8; 1024 * 1024]);
    // Take the receiver FIRST (arming the send side — un-taken channels
    // drop at the source by construction) but do not drain: the flood
    // must hit the BYTE bound, not the arming gate.
    let mut rx = cache.take_evict_rx().expect("rx");
    for i in 0..600 {
        cache.put(&format!("k{i}"), payload.clone());
    }
    let parked = cache.evict_channel_bytes();
    assert!(
        parked <= LruCache::EVICT_CHANNEL_BYTE_BOUND,
        "channel payload bytes must stay <= the bound (got {parked})"
    );
    assert!(
        cache.evict_channel_drops() > 0,
        "past the bound, victims are dropped at the send side (counted)"
    );

    assert!(
        cache.evict_channel_bytes() > 0,
        "armed channel must actually park victims below the bound"
    );

    // Drain half the channel; admission resumes.
    let mut drained = 0u64;
    while drained < LruCache::EVICT_CHANNEL_BYTE_BOUND / 2 {
        let Ok((_k, v, _c)) = rx.try_recv() else {
            break;
        };
        drained += v.len() as u64;
        cache.evict_channel_sub(v.len() as u64);
    }
    let before = cache.evict_channel_bytes();
    cache.put("post-drain", payload.clone());
    assert!(
        cache.evict_channel_bytes() >= before,
        "after a drain, victims are admitted to the channel again"
    );
}

#[test]
fn parked_cap_halves_under_red() {
    assert_eq!(mem_budget::effective_parked_cap(256, Level::Green), 256);
    assert_eq!(mem_budget::effective_parked_cap(256, Level::Yellow), 256);
    assert_eq!(
        mem_budget::effective_parked_cap(256, Level::Red),
        128,
        "Red halves the parked-buffer spill threshold (early flush)"
    );
}

// ---------------------------------------------------------------------------
// Convergence under admission storm (the saturation-suite cage-OOM class)
// ---------------------------------------------------------------------------

#[test]
fn pressure_counts_windowed_unreclaimable_arm() {
    // The gauge-undercount defense: gauges ~0, statm RSS ~0 (dirty page
    // cache is invisible to statm), but the cgroup unreclaimable sample
    // says the process is at the kill boundary — the authority must see
    // it as pressure and enter Red.
    let b = MemBudget::new_for_test();
    b.tick_inner(1000, 0, 960);
    assert_eq!(
        b.level(),
        Level::Red,
        "unreclaimable arm alone must drive the level"
    );
    assert!(b.pressure_bytes() >= 960);
    assert_eq!(b.unreclaimable_bytes(), 960, "windowed max is observable");
    // Decays exactly like the RSS window — never a ratchet.
    for i in 0..4 {
        b.tick_inner(1000, 0, 100);
        assert_eq!(
            b.level(),
            Level::Red,
            "tick {i}: 5-sample window still holds the spike"
        );
    }
    b.tick_inner(1000, 0, 100);
    assert_eq!(
        b.level(),
        Level::Green,
        "unreclaimable spike ages out after 5 samples"
    );
    assert!(b.unreclaimable_bytes() <= 100);
}

#[test]
fn tier_publish_pause_tracks_unreclaimable_hysteresis() {
    // The disk-tier publish gate keys on the UNRECLAIMABLE arm — the
    // dirty-flood / anon-balloon signature — never on gauge-driven Red
    // alone (clean tier page cache is kernel-reclaimable; pausing warm-up
    // publishes on healthy boxes would regress the PR 3 knee scenario).
    let b = MemBudget::new_for_test();
    let g = Arc::new(AtomicU64::new(990));
    b.register(comp(
        "hot",
        g.clone(),
        0,
        1,
        Arc::new(Mutex::new(Vec::new())),
    ));

    // Gauge-driven Red, unreclaimable low: publishes keep flowing.
    b.tick_inner(1000, 0, 100);
    assert_eq!(b.level(), Level::Red);
    assert!(
        !b.tier_publish_paused(),
        "gauge-only Red must NOT pause tier publishes"
    );

    // Unreclaimable enters the Red band: pause.
    b.tick_inner(1000, 0, 950);
    assert!(
        b.tier_publish_paused(),
        "unreclaimable >= 95% of budget pauses tier publishes"
    );

    // Hysteresis: inside the band (>= 91%) the pause HOLDS...
    g.store(0, Ordering::Relaxed);
    for _ in 0..5 {
        b.tick_inner(1000, 0, 920);
    }
    assert!(
        b.tier_publish_paused(),
        "pause holds inside the hysteresis band (no flap)"
    );
    // ...and releases below the Red-exit edge once the window decays.
    for _ in 0..5 {
        b.tick_inner(1000, 0, 900);
    }
    assert!(
        !b.tier_publish_paused(),
        "pause releases below 91% of budget"
    );
}

#[test]
fn hard_backstop_escalates_to_floors_after_sustained_overage() {
    // §5.7 Red-semantics escalation: when the unreclaimable arm sits AT or
    // OVER the full budget for BACKSTOP_SUSTAIN_TICKS consecutive ticks —
    // Red sheds demonstrably not converging against admission — the shed
    // target collapses from 85% of budget to the FLOORS, and the event is
    // counted. This must hold even when every component gauge undercounts
    // (the arm, not the gauges, drives it).
    let b = MemBudget::new_for_test();
    let g = Arc::new(AtomicU64::new(500));
    let targets = Arc::new(Mutex::new(Vec::new()));
    b.register(comp("a", g.clone(), 100, 1, targets.clone()));

    // Two ticks at/over budget: Red sheds run at the normal 85% target,
    // backstop NOT yet tripped (a transient spike is not a failure).
    b.tick_inner(1000, 0, 1100);
    b.tick_inner(1000, 0, 1100);
    assert_eq!(b.level(), Level::Red);
    assert_eq!(b.hard_backstops(), 0, "2 ticks is a spike, not a failure");
    assert!(!b.backstop_active());
    {
        let t = targets.lock().unwrap();
        assert!(
            t.iter().all(|&(_, target)| target > 100),
            "pre-backstop sheds aim at the 85% distribution, not floors: {t:?}"
        );
    }

    // Third consecutive tick: the backstop engages — shed target is the
    // component's FLOOR.
    targets.lock().unwrap().clear();
    b.tick_inner(1000, 0, 1100);
    assert_eq!(
        b.hard_backstops(),
        1,
        "sustained overage trips the backstop"
    );
    assert!(b.backstop_active());
    {
        let t = targets.lock().unwrap();
        assert!(
            t.iter().any(|&(n, target)| n == "a" && target == 100),
            "backstop sheds aim at the floor: {t:?}"
        );
    }

    // Release: the unreclaimable window must decay below the Red-exit edge.
    for _ in 0..5 {
        b.tick_inner(1000, 0, 200);
    }
    assert!(!b.backstop_active(), "backstop releases with the pressure");
    assert_eq!(b.hard_backstops(), 1, "release does not re-count");

    // A gauge-only overage (unreclaimable low) never trips the backstop:
    // phantom logical bytes (e.g. reclaimed-clean tier mmap) are not a
    // kill signature.
    let b2 = MemBudget::new_for_test();
    let g2 = Arc::new(AtomicU64::new(1500));
    b2.register(comp("phantom", g2, 0, 1, Arc::new(Mutex::new(Vec::new()))));
    for _ in 0..5 {
        b2.tick_inner(1000, 0, 100);
    }
    assert_eq!(b2.level(), Level::Red, "gauge pressure still drives Red");
    assert_eq!(
        b2.hard_backstops(),
        0,
        "gauge-only overage must not trip the unreclaimable backstop"
    );
}

#[test]
fn red_convergence_under_admission_storm() {
    // Model the outrun-admission shape: a component whose gauge REGROWS
    // between ticks (16 O_DIRECT streams re-filling faster than 1 Hz
    // sheds) but whose shed lever works (clamps to target). Plain Red
    // keeps it sawtoothing at the 85% target; once the unreclaimable arm
    // rides >= budget for the sustain window, the backstop pins it to the
    // FLOOR — convergence by escalation, not by hope.
    let b = MemBudget::new_for_test();
    let g = Arc::new(AtomicU64::new(0));
    let floor = 100u64;
    {
        let g_gauge = g.clone();
        let g_shed = g.clone();
        b.register(Component::new(
            "storm",
            floor,
            1,
            Arc::new(move || g_gauge.load(Ordering::Relaxed)),
            Arc::new(move |target| g_shed.store(target, Ordering::Relaxed)),
        ));
    }

    // Storm phase: regrow +400 before every tick, unreclaimable pinned
    // over budget (the anon balloon).
    let mut post_shed = Vec::new();
    for _ in 0..6 {
        let cur = g.load(Ordering::Relaxed);
        g.store(cur + 400, Ordering::Relaxed);
        b.tick_inner(1000, 0, 1200);
        post_shed.push(g.load(Ordering::Relaxed));
    }
    assert_eq!(b.level(), Level::Red);
    assert!(
        post_shed.iter().all(|&v| v <= 850),
        "every Red tick must shed the storm back to <= the 85% target: {post_shed:?}"
    );
    assert!(
        b.hard_backstops() >= 1,
        "sustained storm trips the backstop"
    );
    assert_eq!(
        g.load(Ordering::Relaxed),
        floor,
        "backstopped sheds clamp the storm component to its floor"
    );
}

#[test]
fn memory_stat_unreclaimable_parser() {
    // The kill-relevant set: anon + dirty + writeback + shmem +
    // unevictable + unreclaimable slab. Clean file cache (`file` minus
    // dirty/writeback), reclaimable slab, and unknown keys must NOT
    // count — counting clean cache would pin healthy warm mounts in
    // permanent Red (the PR 3 knee scenario serves 5 GiB of clean tier
    // mmap by design).
    let fixture = "\
anon 1048576
file 8388608
kernel 262144
kernel_stack 65536
pagetables 131072
shmem 4096
file_mapped 2097152
file_dirty 524288
file_writeback 262144
anon_thp 0
inactive_anon 1000000
active_anon 48576
inactive_file 6000000
active_file 2388608
unevictable 8192
slab_reclaimable 100000
slab_unreclaimable 200000
slab 300000
workingset_refault_anon 5
pgscan 12345
";
    let sum = mem_budget::parse_unreclaimable_memory_stat(fixture);
    assert_eq!(
        sum,
        1048576 + 524288 + 262144 + 4096 + 8192 + 200000,
        "anon + file_dirty + file_writeback + shmem + unevictable + slab_unreclaimable"
    );
    // Malformed / empty input degrades to 0 (the arm disengages, never
    // panics the sampler).
    assert_eq!(mem_budget::parse_unreclaimable_memory_stat(""), 0);
    assert_eq!(
        mem_budget::parse_unreclaimable_memory_stat("anon garbage\nfile_dirty 12\n"),
        12
    );
}

// ---------------------------------------------------------------------------
// Integration phases (process-global level + counters: ONE test fn)
// ---------------------------------------------------------------------------

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    make_disk(uuid, alloc_ns, "128MB").await
}

async fn make_disk(uuid: [u8; 16], alloc_ns: &str, write_disk: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    // Whole-block machinery under test — the ranged/churn pin precedent.
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
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
        Some(write_disk),
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
        _s: s,
    }
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
    assert_eq!(w.written as usize, data.len());
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// Force the PROCESS level for advisory-integration phases.
fn force_level(l: Level) {
    mem_budget::MEM_BUDGET.force_level_for_test(l);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisory_integration_phases() {
    // ---- Phase A: Yellow pauses dehydration ENTIRELY — protected victims
    // dropped at the worker, never written to the tier (frees the eviction
    // channel's Bytes refs; disk-tier warmth is the cheapest sacrifice).
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    let h = make(*b"membudget-a-pr70", "mb_ns_a").await;
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");

    let ino =
        h.fs.create(h.req, 1, OsStr::new("mb_a"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    for b in 0..6u64 {
        write_at(&h, ino, b * BS, &vec![b as u8 + 1; BS as usize]).await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    // Block 0 → PROTECTED via ghost-admitted second miss, then remove its
    // tier copy so a dehydration WOULD have something to do.
    let d = read_at(&h, ino, 0, 65536).await;
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router.cache.hot_block.remove(map.get(&0).unwrap());
    let d = read_at(&h, ino, 128 * 1024, 65536).await;
    assert!(d.iter().all(|&x| x == 1));
    h.fs.router
        .cache
        .nvme
        .remove_cached_read_block(map.get(&0).unwrap());

    force_level(Level::Yellow);
    let paused0 = METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed);
    // Evict block 0 (protected) via probation churn.
    for b in 1..6u64 {
        let d = read_at(&h, ino, b * BS, 65536).await;
        assert!(d.iter().all(|&x| x == b as u8 + 1));
    }
    let mut paused_seen = false;
    for _ in 0..30 {
        if METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed) > paused0 {
            paused_seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        paused_seen,
        "Yellow must DROP protected victims at the dehydration worker \
         (counter mem_budget_dehydrate_paused)"
    );
    assert!(
        h.fs.router
            .cache
            .nvme
            .get_cached_read_block(map.get(&0).unwrap())
            .is_none(),
        "the paused victim must NOT reach the tier"
    );
    force_level(Level::Green);
    drop(h);

    // ---- Phase B: Red stops prefetch issue outright; Green resumes.
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
    let h2 = make(*b"membudget-b-pr70", "mb_ns_b").await;
    let ino_b = h2
        .fs
        .create(h2.req, 1, OsStr::new("mb_b"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    for b in 0..16u64 {
        write_at(&h2, ino_b, b * BS, &vec![b as u8 + 10; BS as usize]).await;
    }
    h2.fs.fsync(h2.req, ino_b, 0, false).await.unwrap();
    let path_b = squeezefs::keys::inode_path(ino_b);
    let map_b = h2
        .fs
        .router
        .fetch_metadata(&path_b)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    for key in map_b.values() {
        h2.fs.router.cache.purge_block_key(key);
    }

    force_level(Level::Red);
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    for b in 0..8u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_b, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 10));
        }
    }
    assert_eq!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) - issued0,
        0,
        "Red must stop prefetch issue (the reads still serve, foreground-\
         driven)"
    );

    force_level(Level::Green);
    let issued0 = METRICS.prefetch_issued.load(Ordering::Relaxed);
    for b in 8..16u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_b, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 10));
        }
    }
    assert!(
        METRICS.prefetch_issued.load(Ordering::Relaxed) > issued0,
        "Green resumes the pipeline"
    );
    // Settle all pipeline tasks before the next phase.
    for _ in 0..100 {
        let issued = METRICS.prefetch_issued.load(Ordering::Relaxed);
        let done = METRICS.prefetch_completed.load(Ordering::Relaxed)
            + METRICS.prefetch_wasted.load(Ordering::Relaxed);
        if issued == done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ---- Phase C: Yellow freezes window GROWTH (no ×2 on foreground
    // wait) but keeps the pipeline alive at its current window.
    force_level(Level::Yellow);
    let hwm0 = METRICS.prefetch_window_hwm.load(Ordering::Relaxed);
    let g0 = METRICS.prefetch_foreground_waits.load(Ordering::Relaxed);
    let ino_c = h2
        .fs
        .create(h2.req, 1, OsStr::new("mb_c"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    for b in 0..12u64 {
        write_at(&h2, ino_c, b * BS, &vec![b as u8 + 30; BS as usize]).await;
    }
    h2.fs.fsync(h2.req, ino_c, 0, false).await.unwrap();
    let path_c = squeezefs::keys::inode_path(ino_c);
    let map_c = h2
        .fs
        .router
        .fetch_metadata(&path_c)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    for key in map_c.values() {
        h2.fs.router.cache.purge_block_key(key);
    }
    for b in 0..12u64 {
        for half in 0..2u64 {
            let d = read_at(&h2, ino_c, b * BS + half * 262_144, 262_144).await;
            assert!(d.iter().all(|&x| x == b as u8 + 30));
        }
    }
    let _ = g0;
    assert_eq!(
        METRICS.prefetch_window_hwm.load(Ordering::Relaxed),
        hwm0,
        "Yellow freezes window growth: the high-water mark must not move"
    );
    force_level(Level::Green);

    // ---- Phase D: the parked-buffer DRAIN (§5.7 Red row: "early
    // flush_memory_buffers_* — the existing never-lossy staging path,
    // just earlier"). Cap-halving alone only gates INSERTS; the row-5
    // trace showed the backlog itself must drain (spill-to-staging
    // 42%-refused under 4 MiB entries + the revisit carousel pulls
    // entries straight back). The drain uploads parked partials through
    // the durable path: parked bytes fall to ~0 and the data stays
    // byte-exact.
    let ino_d = h2
        .fs
        .create(h2.req, 1, OsStr::new("mb_d"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    // Striped file first (the row-5 shape), then sub-block overwrites:
    // each RMW parks its 512 KiB block buffer.
    for b in 0..6u64 {
        write_at(&h2, ino_d, b * BS, &vec![0u8; BS as usize]).await;
    }
    h2.fs.fsync(h2.req, ino_d, 0, false).await.unwrap();
    for b in 0..6u64 {
        write_at(&h2, ino_d, b * BS, &vec![b as u8 + 50; 4096]).await;
    }
    assert!(
        h2.fs.parked_buffer_bytes() >= 6 * BS,
        "fixture: six parked partial blocks"
    );
    h2.fs.drain_parked_toward(0).await;
    assert_eq!(
        h2.fs.parked_buffer_bytes(),
        0,
        "the drain must flush every parked buffer through the durable path"
    );
    for b in 0..6u64 {
        let d = read_at(&h2, ino_d, b * BS, 4096).await;
        assert!(
            d.iter().all(|&x| x == b as u8 + 50),
            "drained block {b} must stay byte-exact (never-lossy)"
        );
    }
    drop(h2);

    // ---- Phase E: the disk-tier publish PAUSE (the saturation-suite
    // finding-2 fix). While the authority's unreclaimable arm rides the
    // Red band, fill-path tier publishes are SKIPPED (counted) — a read
    // cache absence is never a correctness event; the next reader goes to
    // the device. Resume republishes on the next ghost-admitted fill.
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    let h3 = make(*b"membudget-e-oom2", "mb_ns_e").await;
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    let ino_e = h3
        .fs
        .create(h3.req, 1, OsStr::new("mb_e"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    for b in 0..2u64 {
        write_at(&h3, ino_e, b * BS, &vec![b as u8 + 70; BS as usize]).await;
    }
    h3.fs.fsync(h3.req, ino_e, 0, false).await.unwrap();
    let map_e = h3
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino_e))
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    let k0 = map_e.get(&0).unwrap().clone();
    h3.fs.router.cache.purge_block_key(&k0);
    // Miss 1: ghost RECORD (first touch — publish skipped by admission).
    let d = read_at(&h3, ino_e, 0, 4096).await;
    assert!(d.iter().all(|&x| x == 70));
    h3.fs.router.cache.purge_block_key(&k0);

    // Miss 2 under PAUSE: ghost-admitted, but the tier publish is paused.
    mem_budget::MEM_BUDGET.force_tier_publish_paused_for_test(true);
    let paused0 = METRICS.read_tier_publishes_paused.load(Ordering::Relaxed);
    let d = read_at(&h3, ino_e, 0, 4096).await;
    assert!(d.iter().all(|&x| x == 70), "paused fill still serves");
    assert!(
        h3.fs.router.cache.nvme.get_cached_read_block(&k0).is_none(),
        "a ghost-admitted fill must NOT reach the tier while paused"
    );
    assert!(
        METRICS.read_tier_publishes_paused.load(Ordering::Relaxed) > paused0,
        "paused publishes are counted (observability)"
    );

    // Unpause: the next ghost-admitted fill publishes again.
    mem_budget::MEM_BUDGET.force_tier_publish_paused_for_test(false);
    h3.fs.router.cache.purge_block_key(&k0);
    let d = read_at(&h3, ino_e, 0, 4096).await;
    assert!(d.iter().all(|&x| x == 70));
    assert!(
        h3.fs.router.cache.nvme.get_cached_read_block(&k0).is_some(),
        "publishes resume after the pause releases"
    );
    drop(h3);

    // ---- Phase F: Red parked-buffer admission BLOCKS (awaited drain
    // backpressure — §4.4-pt-5 ring-admission precedent). Pre-fix the cap
    // was advisory-soft: with staging refusing spills (1 MiB write ring vs
    // 512 KiB entries — the measured production shape: 100% refusals,
    // staging_mmap=0 the whole run) inserts proceeded past the cap
    // unbounded (1,937 parked = 7.6 GiB anon at the kill). At Red the
    // writer must instead AWAIT the never-lossy drain until the parked
    // set is back under the halved cap — bounded, byte-exact, no spin.
    let h4 = make_disk(*b"membudget-f-oom2", "mb_ns_f", "1MB").await;
    let ino_f = h4
        .fs
        .create(h4.req, 1, OsStr::new("mb_f"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    const STORM_BLOCKS: u64 = 140; // > the Red cap of 128
    for b in 0..STORM_BLOCKS {
        write_at(&h4, ino_f, b * BS, &vec![0x20; BS as usize]).await;
    }
    h4.fs.fsync(h4.req, ino_f, 0, false).await.unwrap();
    assert_eq!(h4.fs.parked_buffer_bytes(), 0, "fixture: fsync drained");

    force_level(Level::Red);
    let gate_waits0 = METRICS.parked_gate_waits.load(Ordering::Relaxed);
    let red_cap_bytes = mem_budget::effective_parked_cap(256, Level::Red) as u64 * BS;
    let mut peak_parked = 0u64;
    for b in 0..STORM_BLOCKS {
        write_at(&h4, ino_f, b * BS, &[b as u8; 4096]).await;
        peak_parked = peak_parked.max(h4.fs.parked_buffer_bytes());
    }
    assert!(
        peak_parked <= red_cap_bytes,
        "Red admission must hold the parked set at/under the halved cap \
         (peak {peak_parked} B vs cap {red_cap_bytes} B) — advisory-soft \
         caps are the measured OOM engine"
    );
    assert!(
        METRICS.parked_gate_waits.load(Ordering::Relaxed) > gate_waits0,
        "the blocking admission gate must be observable (parked_gate_waits)"
    );
    force_level(Level::Green);
    h4.fs.fsync(h4.req, ino_f, 0, false).await.unwrap();
    for b in 0..STORM_BLOCKS {
        let d = read_at(&h4, ino_f, b * BS, 4096).await;
        assert!(
            d.iter().all(|&x| x == b as u8),
            "block {b}: gated writes must stay byte-exact (never-lossy)"
        );
        let tail = read_at(&h4, ino_f, b * BS + 8192, 4096).await;
        assert!(
            tail.iter().all(|&x| x == 0x20),
            "block {b}: RMW remainder must survive the gated drain"
        );
    }
}
