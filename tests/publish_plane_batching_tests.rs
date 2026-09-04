//! E2E perf audit campaign **D-1b** — the S9 **publish plane** is
//! stop-and-wait depth 1 (`docs/design-e2e-perf-audit.md` §3 board DLM
//! #8; D-1's scoping finding, `.benchmarks/2026-09-02-d1-owner-concurrent-
//! dispatch.md` §Owed 2): D-1 collapsed the METADATA-verb frame (64 verbs:
//! 64 conveyor passes → 3), but the LAYOUT-PUBLISH plane the co-writer
//! ingest wall rides is a different wire — `PublishRequestFrame` carries
//! ONE call and `PublishClient` holds ONE mutex-serialized session per
//! endpoint, so every publish is a full stop-and-wait round trip and the
//! owner sees one call per frame. F-A's co-queuing never engages there,
//! and a co-writer publishing per 4 MiB block at RTT-bound depth 1 caps
//! near a few GB/s regardless of device speed (the S9-a ≈ 2.6 GiB/s wall).
//!
//! # The rig
//!
//! The `mw_publish_era_gate_tests` two-node shape: one authority (custody
//! owner + publish service on a real `cluster_wire` listener over
//! `127.0.0.1`) and one co-writer (all-foreign ownership, a publish
//! client, a REAL custody join so every publish carries a live lease
//! epoch), both backends file-backed KV sandboxes in one process. The 24
//! inos model the streaming co-writer whose 24 per-block saves arrive at
//! the publish client concurrently (each save holds its own ino's 3.5
//! stripe, so distinct inos never serialize on the client).
//!
//! # Instruments
//!
//! * **frames** — the listener's `requests_served` delta (one per wire
//!   `Call` frame the owner served);
//! * **owner passes** — the process-global `META_CONVEYOR_LEADER_PASSES`
//!   delta (the M7 conveyor's pass count). Before D-1c this was a
//!   measurement on an un-held conveyor (since C-2 the pass drains
//!   arrivals the instant they land, so the count was arrival spread ÷
//!   pass latency — 3–14 passes per 24 publishes over 51 runs) and a
//!   contract only under the held pass (`PassHold`). **Since D-1c (e2e
//!   perf audit §5.3 row 1 — one conveyor group per shipped frame) it is
//!   a contract on the NATURAL row too**: the owner stages a frame's
//!   independent layout publishes concurrently and enqueues them on the
//!   conveyor as ONE group (one queue-lock acquisition), so a frame is
//!   one pass by construction;
//! * **groups** — `META_CONVEYOR_GROUP_{COMMITS,TXS}` (groups enqueued /
//!   member txs) and the publish ledger's `frame_groups` (frames that
//!   committed ≥ 1 group) — the rung's engagement instruments;
//! * **journal entries** — `META_KV_JOURNAL_ENTRIES` delta (one tx = one
//!   checksummed entry: the count that must NOT collapse);
//! * **wall** — spawn-to-join over the whole concurrent set.

use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_DRAIN,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::{
    META_CONVEYOR_GROUP_COMMITS, META_CONVEYOR_GROUP_TXS, META_CONVEYOR_LEADER_PASSES,
    META_KV_JOURNAL_ENTRIES,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend,
};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The `job:enroll`-class storage-trust secret (S3's root of trust).
const SECRET: &[u8] = b"d1b-publish-plane-storage-trust-secret";

const VOL_LEN: u64 = 256 * 1024 * 1024;

const NODE: &str = "node-d1b-cowriter";
const AUTHORITY: &str = "d1b-authority";

/// The streaming co-writer's ino population (the 24-file field shape).
const INOS: usize = 24;

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global state everywhere)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

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

/// Disarm every process-global half a rig arms (between rows and at exit).
fn reset_planes() {
    data_grant::uninstall_custody_client();
    data_grant::uninstall_custody_owner();
    publish::uninstall_client();
    ship::disarm_ownership();
    data_custody::test_reset_custody_generation();
    data_custody::test_clear_poison();
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        reset_planes();
    }
}

fn restore() -> Restore {
    Restore
}

/// Scoped env override (the conveyor_tests pattern). The publish owner
/// reads its group lever per served frame, so a guard wrapping the row is
/// what flips it; `serial()` keeps rows from overlapping.
struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn opts() -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn sandbox(dir: &Path, tag: &str) -> Arc<RoutedMetaBackend> {
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir, &format!("{tag}-meta0"), VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format meta volume");
    open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set")
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

struct Authority {
    listener: Arc<squeezefs::cluster_wire::RpcListener>,
    endpoint: String,
}

fn start_authority(inner: Arc<RoutedMetaBackend>) -> Authority {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        AUTHORITY,
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&owner));
    let router = data_grant::AsyncVerbRouter::new()
        .with_custody(Arc::clone(&owner))
        .with_publish(publish::PublishService::new(inner));
    let listener = squeezefs::cluster_wire::RpcListener::start_async(
        squeezefs::cluster_wire::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..squeezefs::cluster_wire::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority { listener, endpoint }
}

/// Arm the CO-WRITER half: all-foreign ownership over `client_be`, the
/// publish client under test, and a REAL custody join (the lease epoch
/// every mutating publish presents).
async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
    pc: Arc<publish::PublishClient>,
) -> Arc<WriteCustodyClient> {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new(AUTHORITY, &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(client_be, foreign).expect("owner map"));
    publish::install_client(pc);
    let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&client));
    client
}

/// A DECODABLE layout `Put` (the owner's compose arms decode it).
fn layout_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: Some("be://data".into()),
        file_id: None,
        data_key: None,
        block_map: Some(std::collections::HashMap::new()),
    })
    .expect("serialize layout")
}

async fn owner_size(be: &Arc<RoutedMetaBackend>, ino: u64) -> u64 {
    be.getattr(ino).await.expect("the owner has the ino").size
}

/// Mint `n` regular files ON THE OWNER (its own local path — the objects
/// the shipped publishes name).
async fn mint(owner_be: &Arc<RoutedMetaBackend>, n: usize, prefix: &str) -> Vec<u64> {
    let mut inos = Vec::with_capacity(n);
    for i in 0..n {
        let ino = owner_be
            .create_with_rdev(1, &format!("{prefix}{i}"), libc::S_IFREG | 0o644, 0, 0, 0)
            .await
            .expect("owner-local create")
            .ino;
        inos.push(ino);
    }
    inos
}

/// The counters the rows read, sampled together.
struct Counters {
    frames: u64,
    passes: u64,
    entries: u64,
    /// D-1c: conveyor groups enqueued / their member txs (process-global).
    groups: u64,
    group_txs: u64,
    /// D-1c: served frames that committed ≥ 1 group (the publish ledger).
    frame_groups: u64,
}

fn counters(auth: &Authority) -> Counters {
    Counters {
        frames: auth.listener.stats().requests_served,
        passes: META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed),
        entries: META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        groups: META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed),
        group_txs: META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed),
        frame_groups: publish::stats().frame_groups,
    }
}

struct TwoNodes {
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    auth: Authority,
    _client: Arc<WriteCustodyClient>,
}

impl TwoNodes {
    async fn start(dir: &Path, pc: Arc<publish::PublishClient>) -> Self {
        let owner_be = sandbox(dir, "own").await;
        let client_be = sandbox(dir, "cli").await;
        let auth = start_authority(Arc::clone(&owner_be));
        let client = arm_client(&auth, &client_be, pc).await;
        Self {
            owner_be,
            client_be,
            auth,
            _client: client,
        }
    }

    /// Warm the publish session POOL so measured frames pay no connect
    /// (the D-1 discipline): a burst wide enough to dial every session
    /// the derived depth allows, against inos the rows never touch.
    async fn warm(&self) {
        let warm = mint(&self.owner_be, 16, "warm").await;
        for round in 0..3 {
            let (outcomes, _) = self.publish_concurrently(&warm, |_| round + 1).await;
            for out in outcomes {
                out.expect("the warm publish lands");
            }
        }
    }

    /// Ship `inos.len()` layout publishes CONCURRENTLY from the co-writer
    /// (one task per ino — the streaming shape), returning the per-ino
    /// outcomes in ino order and the wall over the whole set.
    async fn publish_concurrently(
        &self,
        inos: &[u64],
        size_of: impl Fn(usize) -> u64,
    ) -> (Vec<squeezefs::error::Result<bool>>, Duration) {
        let started = Instant::now();
        let out = join_publishes(self.spawn_publishes(inos, size_of)).await;
        (out, started.elapsed())
    }

    /// The spawn half of [`Self::publish_concurrently`]: one task per ino,
    /// returned un-joined so a contract can place a barrier between "every
    /// publish is queued" and "every publish answered".
    fn spawn_publishes(
        &self,
        inos: &[u64],
        size_of: impl Fn(usize) -> u64,
    ) -> Vec<tokio::task::JoinHandle<squeezefs::error::Result<bool>>> {
        inos.iter()
            .enumerate()
            .map(|(i, &ino)| {
                let be = Arc::clone(&self.client_be);
                let size = size_of(i);
                tokio::spawn(async move {
                    publish::set_layout_and_size(&be, ino, &layout_bytes(size), size, &[]).await
                })
            })
            .collect()
    }

    /// The owner's one meta volume — the conveyor the shipped publishes
    /// commit on.
    fn owner_volume(&self) -> &KvMetaBackend {
        &self.owner_be.volumes[0]
    }

    async fn stop(self) {
        self.auth.listener.shutdown();
        shutdown(&self.owner_be).await;
        shutdown(&self.client_be).await;
    }
}

/// Join the spawned publishes in ino order (a publish task never panics).
async fn join_publishes(
    handles: Vec<tokio::task::JoinHandle<squeezefs::error::Result<bool>>>,
) -> Vec<squeezefs::error::Result<bool>> {
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("a publish task never panics"));
    }
    out
}

/// Poll `cond` at 1 ms until it holds or `within` elapses (a bounded
/// condition wait, never a sleep as coordination).
async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The owner conveyor's `meta_txpass_phase_ns` means over a row (exact
/// sum ÷ count deltas — the audit A1 discipline), for the D-1c rows'
/// before/after print: `tx_queue_wait` (a member's park before its pass),
/// `pass_total` (the apply stage's service time) and `window_total`.
fn txpass_means(before: &serde_json::Value, after: &serde_json::Value) -> String {
    ["tx_queue_wait", "pass_total", "window_total"]
        .iter()
        .map(|ph| {
            let get = |v: &serde_json::Value, k: &str| v[ph][k].as_u64().unwrap_or(0);
            let sum = get(after, "sum_ns").saturating_sub(get(before, "sum_ns"));
            let n = get(after, "count").saturating_sub(get(before, "count"));
            format!(
                "{ph} {:.1} us (n={n})",
                if n == 0 {
                    0.0
                } else {
                    sum as f64 / n as f64 / 1e3
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The drain-hold seam, armed for one scope.
struct DrainHold;

impl DrainHold {
    fn arm(ms: u64) -> Self {
        publish::TEST_PUBLISH_DRAIN_HOLD_MS.store(ms, Ordering::SeqCst);
        DrainHold
    }
}

impl Drop for DrainHold {
    fn drop(&mut self) {
        publish::TEST_PUBLISH_DRAIN_HOLD_MS.store(0, Ordering::SeqCst);
    }
}

/// The hold every framing contract arms: long enough that a burst of
/// concurrent submissions is provably queued before the drain takes it.
const HOLD_MS: u64 = 150;

/// The owner-side seam (M7 §5.5 D5, the `conveyor_tests::group_forms_
/// under_held_pass` protocol): park the owner's apply pass BEFORE it
/// drains, so a frame's calls provably co-queue before one pass takes
/// them. Since C-2 the apply pass runs on the volume's own journal lane
/// and drains arrivals the instant they land, so an un-held pass count is
/// the frame's ARRIVAL SPREAD (24 calls through one `join_all`, each with
/// its own custody/base prelude) against the pass's latency — a venue
/// ratio (3–14 passes for 24 publishes over 51 runs on the all-features
/// debug build), not a publish-plane property. The held pass makes the
/// co-queue law itself the contract. Drop disarms + releases, so a panic
/// mid-test cannot strand the next test's conveyor.
struct PassHold;

impl PassHold {
    fn arm() -> Self {
        TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
        PassHold
    }

    fn release(&self) {
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
    }
}

impl Drop for PassHold {
    fn drop(&mut self) {
        self.release();
    }
}

// ===========================================================================
// 1. The measurement rows — the campaign's in-process face
// ===========================================================================

/// **The publish plane's shape under a 24-ino concurrent co-writer**, at
/// the derived depth and at depth 1 (the two levers isolated).
///
/// Dev tip `4c596ec2`: `PublishClient::ship` encoded ONE call per
/// `PublishRequestFrame` and serialized every ship on the endpoint's
/// session mutex, so 24 concurrent publishes were 24 stop-and-wait round
/// trips — **frames/publish = 1.0** — and the owner saw them one at a
/// time, so its conveyor ran **one pass per publish** (nothing
/// co-queued): measured 24 / 24 / 24 entries / 291–378 µs per publish.
/// The journal-entry count is N by law (one tx = one checksummed entry)
/// and must stay N under any fix.
///
/// The contract here is the NATURAL (seam-free) collapse: a busy pipe
/// self-batches, so a concurrent set must ride well under one frame per
/// publish and well under one owner pass per publish. The strict bound
/// lives in the seam-controlled contract below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measurement_rows_24_concurrent_publishes_from_one_co_writer() {
    let _serial = serial();
    let _restore = restore();
    for (label, pc) in [
        (
            "derived depth",
            publish::PublishClient::new(NODE, SECRET.to_vec()),
        ),
        (
            "depth 1",
            publish::PublishClient::with_depth(NODE, SECRET.to_vec(), 1),
        ),
    ] {
        let dir = TempDir::new().unwrap();
        let nodes = TwoNodes::start(dir.path(), pc).await;
        nodes.warm().await;
        let inos = mint(&nodes.owner_be, INOS, "stream").await;

        let dials_before = publish::stats().ship_session_dials;
        let before = counters(&nodes.auth);
        let (outcomes, wall) = nodes
            .publish_concurrently(&inos, |i| 4096 * (i as u64 + 1))
            .await;
        let after = counters(&nodes.auth);
        let dials = publish::stats().ship_session_dials - dials_before;

        for (i, out) in outcomes.iter().enumerate() {
            out.as_ref()
                .unwrap_or_else(|e| panic!("publish {i} failed: {e}"));
        }
        for (i, &ino) in inos.iter().enumerate() {
            assert_eq!(
                owner_size(&nodes.owner_be, ino).await,
                4096 * (i as u64 + 1),
                "every caller's own publish landed on its own ino"
            );
        }

        let frames = after.frames - before.frames;
        let passes = after.passes - before.passes;
        let entries = after.entries - before.entries;
        let n = INOS as f64;
        // A session dialed INSIDE the row pays the cluster wire's 100 ms
        // accept-poll tick (`ACCEPT_POLL_TICK`) — label it so the wall is
        // read as a cold-pool roll, not a steady-state one.
        println!(
            "D-1b in-process row [{label}] ({} build, file-backed KV sandbox, loopback \
             wire): {INOS} concurrent publishes from one co-writer -> frames {frames} \
             ({:.2}/publish), owner conveyor passes {passes} ({:.2}/publish), journal \
             entries {entries}, wall {:.2} ms ({:.1} us/publish), in-row session dials \
             {dials}{}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            frames as f64 / n,
            passes as f64 / n,
            wall.as_secs_f64() * 1e3,
            wall.as_secs_f64() * 1e6 / n,
            if dials > 0 {
                " (COLD POOL: each dial pays the wire's 100 ms accept tick)"
            } else {
                ""
            }
        );

        assert_eq!(
            entries, INOS as u64,
            "[{label}] one tx = one checksummed journal entry — the count that never collapses"
        );
        assert!(
            frames * 2 <= INOS as u64,
            "[{label}] a busy publish pipe self-batches: {frames} frames for {INOS} concurrent \
             publishes is the stop-and-wait shape"
        );
        // `passes` is a MEASUREMENT in this row (the client-side drain is
        // un-held, so frames form by arrival and a straggler frame is its
        // own pass); the pass-per-frame law is the contract in
        // `a_framed_burst_is_one_owner_conveyor_pass_by_construction`.
        assert!(
            passes >= 1,
            "[{label}] the owner conveyor ran at least one pass for {INOS} publishes"
        );

        nodes.stop().await;
        // Between rows the process-global halves must be re-armable.
        reset_planes();
    }
}

// ===========================================================================
// 2. The framing contract (seam-controlled): 24 concurrent publishes ride
//    ≤ 4 frames and co-queue into ONE owner conveyor pass
// ===========================================================================

/// The frame ceiling the drain hold makes deterministic: all 24 are queued
/// before the lane's drain takes the queue, so the ideal is ONE frame; the
/// slack is the per-frame call cap (`router::batch_max`) on a small box.
const MAX_FRAMES: u64 = 4;

/// Contract: a burst of 24 concurrent publishes from one co-writer to one
/// authority is framed into **at most [`MAX_FRAMES`] wire frames**, and
/// the frames' independent inos **co-queue on the owner's M7 conveyor**
/// (D-1's mechanism, applied to the publish plane): with the owner's apply
/// pass held pre-drain, every one of the 24 calls is queued behind it
/// BEFORE it runs, and the release drains them in ONE pass (≤ 2 — the
/// `group_forms_under_held_pass` tolerance for one ambient singleton).
/// The journal-entry count stays 24 (one tx = one checksummed entry) and
/// every caller gets its own reply (its own ino's size lands).
///
/// Both seams are what make the two halves deterministic: the client
/// lane's drain hold for "concurrent" (frames), the owner's pass hold for
/// "co-queued" (passes). The client's frame depth is pinned to
/// [`MAX_FRAMES`] so every frame is in flight at once — a frame waiting
/// on a held pass must never gate the next frame's departure, or the
/// barrier below could not form.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_four_concurrent_publishes_ride_at_most_four_frames_and_one_owner_pass() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::with_depth(NODE, SECRET.to_vec(), MAX_FRAMES as usize);
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, INOS, "burst").await;

    let _hold = DrainHold::arm(HOLD_MS);
    let pass_hold = PassHold::arm();
    let before = counters(&nodes.auth);
    let handles = nodes.spawn_publishes(&inos, |i| 8192 * (i as u64 + 1));

    // The barrier: every call parked on the owner's conveyor behind the
    // held pass. The frames were served (decoded + dispatched) to get
    // here, so the frame count is final at this point too.
    let owner = nodes.owner_volume();
    let queued = eventually(Duration::from_secs(30), || {
        owner.conveyor_pending_len() >= INOS
    })
    .await;
    assert!(
        queued,
        "all {INOS} framed publishes must queue behind the held owner pass; {} queued \
         after 30 s (frames served so far: {})",
        owner.conveyor_pending_len(),
        counters(&nodes.auth).frames - before.frames
    );
    let frames = counters(&nodes.auth).frames - before.frames;

    pass_hold.release();
    let outcomes = join_publishes(handles).await;
    let after = counters(&nodes.auth);

    for (i, out) in outcomes.iter().enumerate() {
        out.as_ref()
            .unwrap_or_else(|e| panic!("publish {i} failed: {e}"));
    }
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(
            owner_size(&nodes.owner_be, ino).await,
            8192 * (i as u64 + 1),
            "reply correlation: caller {i}'s publish landed on caller {i}'s ino"
        );
    }
    let passes = after.passes - before.passes;
    assert_eq!(after.entries - before.entries, INOS as u64);
    assert!(
        frames <= MAX_FRAMES,
        "24 queued publishes must ride ≤ {MAX_FRAMES} frames (got {frames})"
    );
    assert!(
        (1..=2).contains(&passes),
        "24 framed publishes queued behind one held pass must drain in 1–2 owner conveyor \
         passes, not {passes}"
    );

    drop(pass_hold);
    nodes.stop().await;
}

// ===========================================================================
// 2b. D-1c — one conveyor group per shipped frame, WITHOUT a held pass
// ===========================================================================

/// Contract (D-1c, `docs/design-e2e-perf-audit.md` §5.3 row 1): a frame of
/// independent layout publishes is ONE owner conveyor pass **by
/// construction** — no `PassHold`. The owner prepares the frame's calls
/// concurrently, stages every tx under one canonical 4a acquisition, and
/// enqueues the staged set on the conveyor as ONE group (one queue-lock
/// acquisition — the loom-modeled `enqueue_many`), so the drain that takes
/// the group's head takes the whole group; arrival spread no longer
/// fragments a frame (the 3–14 passes per 24 publishes the C-2 note
/// measured). Only the client-side drain hold is armed (it makes
/// "concurrent" deterministic: the 24 queue into ≤ [`MAX_FRAMES`] frames);
/// the owner side runs its natural path.
///
/// Laws pinned: passes ≤ frames (+ 1 ambient singleton, the
/// `group_forms_under_held_pass` tolerance); one group per multi-call
/// frame (`META_CONVEYOR_GROUP_COMMITS`), every framed call a group member
/// except a single-call frame's, which rides alone (`GROUP_TXS + single
/// frames == 24`); `frame_groups` engagement; journal entries stay 24 (one
/// tx = one checksummed entry); every caller's reply lands on its own ino.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_framed_burst_is_one_owner_conveyor_pass_by_construction() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::with_depth(NODE, SECRET.to_vec(), MAX_FRAMES as usize);
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, INOS, "grouped").await;

    let _hold = DrainHold::arm(HOLD_MS);
    let before = counters(&nodes.auth);
    let phases_before = squeezefs::fuse_client::meta_txpass_phase_json();
    let (outcomes, wall) = nodes
        .publish_concurrently(&inos, |i| 8192 * (i as u64 + 1))
        .await;
    let after = counters(&nodes.auth);
    let phases_after = squeezefs::fuse_client::meta_txpass_phase_json();

    for (i, out) in outcomes.iter().enumerate() {
        out.as_ref()
            .unwrap_or_else(|e| panic!("publish {i} failed: {e}"));
    }
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(
            owner_size(&nodes.owner_be, ino).await,
            8192 * (i as u64 + 1),
            "reply correlation: caller {i}'s publish landed on caller {i}'s ino"
        );
    }
    let frames = after.frames - before.frames;
    let passes = after.passes - before.passes;
    let entries = after.entries - before.entries;
    let groups = after.groups - before.groups;
    let group_txs = after.group_txs - before.group_txs;
    let frame_groups = after.frame_groups - before.frame_groups;
    println!(
        "D-1c in-process row [lever on] ({} build): {INOS} concurrent publishes -> frames \
         {frames}, owner conveyor passes {passes}, conveyor groups {groups} carrying \
         {group_txs} txs, frame_groups {frame_groups}, journal entries {entries}, wall {:.2} ms \
         ({:.1} us/publish); meta_txpass_phase_ns: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        wall.as_secs_f64() * 1e3,
        wall.as_secs_f64() * 1e6 / INOS as f64,
        txpass_means(&phases_before, &phases_after)
    );

    assert_eq!(
        entries, INOS as u64,
        "one tx = one checksummed journal entry — the count that never collapses"
    );
    assert!(
        frames <= MAX_FRAMES,
        "24 queued publishes must ride ≤ {MAX_FRAMES} frames (got {frames})"
    );
    assert!(
        passes <= frames + 1,
        "one conveyor pass per frame BY CONSTRUCTION: {frames} frames drained in {passes} \
         owner passes (≤ 1 ambient singleton tolerated) — arrival spread fragmented a frame"
    );
    assert!(
        (1..=frames).contains(&groups),
        "every multi-call frame commits as exactly one group: {groups} groups for {frames} \
         frames"
    );
    // A single-call frame (a straggler) has no group and rides alone; every
    // other framed call is a group member.
    assert_eq!(
        group_txs + (frames - groups),
        INOS as u64,
        "every framed call is a group member except a single-call frame's own \
         ({group_txs} member txs, {frames} frames, {groups} groups)"
    );
    assert_eq!(
        frame_groups, groups,
        "engagement: each grouped frame is one round here (chains of length 1), so \
         frame_groups == groups"
    );

    nodes.stop().await;
}

/// The A/B lever: `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=0` restores the
/// pre-rung per-call path — every framed call commits its own tx (the
/// group gauges stay flat), and the row still lands every publish with 24
/// entries. The lever is a measurement control, never an operational
/// escape; the owner reads it per served frame (one getenv per wire round
/// trip), which is what lets this row flip it in-process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_group_lever_off_is_the_pre_rung_per_call_shape() {
    let _serial = serial();
    let _restore = restore();
    let _lever = EnvVarGuard::set("SQUEEZEFS_PUBLISH_CONVEYOR_GROUP", "0");
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::with_depth(NODE, SECRET.to_vec(), MAX_FRAMES as usize);
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, INOS, "ungrouped").await;

    let _hold = DrainHold::arm(HOLD_MS);
    let before = counters(&nodes.auth);
    let phases_before = squeezefs::fuse_client::meta_txpass_phase_json();
    let (outcomes, wall) = nodes
        .publish_concurrently(&inos, |i| 8192 * (i as u64 + 1))
        .await;
    let after = counters(&nodes.auth);
    let phases_after = squeezefs::fuse_client::meta_txpass_phase_json();

    for (i, out) in outcomes.iter().enumerate() {
        out.as_ref()
            .unwrap_or_else(|e| panic!("publish {i} failed under the lever: {e}"));
    }
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(
            owner_size(&nodes.owner_be, ino).await,
            8192 * (i as u64 + 1)
        );
    }
    println!(
        "D-1c in-process row [lever OFF — the pre-rung shape] ({} build): {INOS} concurrent \
         publishes -> frames {}, owner conveyor passes {}, journal entries {}, wall {:.2} ms \
         ({:.1} us/publish); meta_txpass_phase_ns: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        after.frames - before.frames,
        after.passes - before.passes,
        after.entries - before.entries,
        wall.as_secs_f64() * 1e3,
        wall.as_secs_f64() * 1e6 / INOS as f64,
        txpass_means(&phases_before, &phases_after)
    );
    assert_eq!(after.entries - before.entries, INOS as u64);
    assert!(after.frames - before.frames <= MAX_FRAMES);
    assert!(after.passes - before.passes >= 1);
    assert_eq!(
        after.groups - before.groups,
        0,
        "lever off: no conveyor group is enqueued (the per-call path)"
    );
    assert_eq!(after.group_txs - before.group_txs, 0);
    assert_eq!(
        after.frame_groups - before.frame_groups,
        0,
        "lever off: no served frame committed a group"
    );

    nodes.stop().await;
}

// ===========================================================================
// 3. Per-call isolation inside one frame
// ===========================================================================

/// Contract: a call that FAILS at the owner (here: a publish naming an
/// ino the owner does not have) lands its error in ITS caller's reply and
/// nothing else's — its 8 siblings in the same frame all apply (journal
/// entries += 8), and the frame count says they WERE one frame. The
/// caller-side never-lossy refill (the f38 law) hangs off that per-call
/// `Err`, so isolating it is what keeps a sibling's accounting from being
/// refilled for a failure it did not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_call_isolates_from_its_siblings_in_one_frame() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let mut inos = mint(&nodes.owner_be, 8, "iso").await;
    // The doomed sibling: a routable ino no one minted.
    let bogus = inos[7] + 500;
    inos.insert(4, bogus);

    let _hold = DrainHold::arm(HOLD_MS);
    let before = counters(&nodes.auth);
    let (outcomes, _wall) = nodes.publish_concurrently(&inos, |_| 4096).await;
    let after = counters(&nodes.auth);

    for (i, (out, &ino)) in outcomes.iter().zip(&inos).enumerate() {
        if ino == bogus {
            assert!(
                out.is_err(),
                "the doomed call's failure lands in ITS slot (slot {i})"
            );
        } else {
            out.as_ref()
                .unwrap_or_else(|e| panic!("sibling {i} must apply, got {e}"));
            assert_eq!(owner_size(&nodes.owner_be, ino).await, 4096);
        }
    }
    assert_eq!(
        after.entries - before.entries,
        8,
        "the 8 siblings committed; the doomed call staged nothing"
    );
    // The burst is ONE frame; the doomed call's bounded witnessed ladder
    // (`PUBLISH_SHIP_ATTEMPTS` = 3) re-ships it alone twice more, each
    // answered from the owner's window — the only extra frames allowed.
    let frames = after.frames - before.frames;
    assert!(
        frames <= 3,
        "all 9 travelled as ONE frame (+ the doomed call's two re-ships) — isolation is per \
         call, not per frame (got {frames})"
    );

    nodes.stop().await;
}

// ===========================================================================
// 4. A replayed frame is absorbed by the witness, call by call
// ===========================================================================

/// Contract: a frame re-sent with the SAME `(lease_epoch, request_id)`
/// witnesses — the lost-reply retry shape — answers every call from the
/// owner's dedup window: `replays` grows by N, the journal stays where it
/// was (nothing re-applied), and each caller's outcome is byte-identical
/// to the winner's. Both the original and the replay travel as ONE frame
/// each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_frame_is_absorbed_by_the_witness_call_by_call() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    let nodes = TwoNodes::start(dir.path(), Arc::clone(&pc)).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, 8, "replay").await;
    let epoch = nodes._client.lease_epoch();
    let calls: Vec<publish::PublishCall> = inos
        .iter()
        .enumerate()
        .map(|(i, &ino)| publish::PublishCall::SetLayoutAndSize {
            ino,
            layout: layout_bytes(16384),
            size: 16384,
            refs: Vec::new(),
            lease_epoch: epoch,
            request_id: 0xB00 + i as u64,
        })
        .collect();
    let ship_all = |calls: Vec<publish::PublishCall>| {
        let pc = Arc::clone(&pc);
        let endpoint = nodes.auth.endpoint.clone();
        async move {
            let handles: Vec<_> = calls
                .into_iter()
                .map(|call| {
                    let pc = Arc::clone(&pc);
                    let endpoint = endpoint.clone();
                    tokio::spawn(async move { pc.ship(&endpoint, call).await })
                })
                .collect();
            let mut out = Vec::new();
            for h in handles {
                out.push(h.await.expect("no panic").expect("the call is answered"));
            }
            out
        }
    };

    let _hold = DrainHold::arm(HOLD_MS);
    let before = counters(&nodes.auth);
    let replays_before = publish::stats().replays;
    let first = ship_all(calls.clone()).await;
    let mid = counters(&nodes.auth);
    assert_eq!(mid.entries - before.entries, 8, "the originals applied");
    assert_eq!(
        mid.frames - before.frames,
        1,
        "the originals were ONE frame"
    );

    let replayed = ship_all(calls).await;
    let after = counters(&nodes.auth);
    assert_eq!(
        replayed, first,
        "every replay answers its winner's own outcome"
    );
    assert_eq!(
        publish::stats().replays - replays_before,
        8,
        "every call of the replayed frame is a witness hit"
    );
    assert_eq!(
        after.entries, mid.entries,
        "journal-entry equality: the replay staged NOTHING"
    );
    assert_eq!(after.frames - mid.frames, 1, "the replay was ONE frame");
    for &ino in &inos {
        assert_eq!(owner_size(&nodes.owner_be, ino).await, 16384);
    }

    nodes.stop().await;
}

// ===========================================================================
// 5. Same-ino calls inside one frame keep submission order
// ===========================================================================

/// Contract: two publishes naming ONE ino that land in the same frame
/// execute in submission order — the later one's state is what the ino
/// ends with — while independent siblings in the frame run beside them.
/// Enqueue order is made deterministic by a current-thread runtime: each
/// submitter is driven to its park (the enqueue is before its first
/// pending point) before the next is spawned.
#[tokio::test]
async fn same_ino_calls_in_one_frame_keep_submission_order() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, 7, "order").await;
    let hot = inos[3];

    let _hold = DrainHold::arm(HOLD_MS);
    let before = counters(&nodes.auth);
    let submit = |ino: u64, size: u64| {
        let be = Arc::clone(&nodes.client_be);
        tokio::spawn(async move {
            publish::set_layout_and_size(&be, ino, &layout_bytes(size), size, &[]).await
        })
    };
    let mut handles = Vec::new();
    // Submission order: hot@1000, six siblings, hot@2000, one sibling.
    handles.push(submit(hot, 1000));
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    for &ino in inos.iter().filter(|&&i| i != hot).take(5) {
        handles.push(submit(ino, 4096));
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    handles.push(submit(hot, 2000));
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    handles.push(submit(inos[6], 4096));
    for h in handles {
        h.await
            .expect("no panic")
            .expect("every publish in the frame is answered Ok");
    }
    let after = counters(&nodes.auth);
    assert_eq!(
        owner_size(&nodes.owner_be, hot).await,
        2000,
        "the LATER same-ino publish is what the ino ends with"
    );
    assert_eq!(after.entries - before.entries, 8);
    assert_eq!(
        after.frames - before.frames,
        1,
        "the whole burst — both same-ino calls included — was ONE frame"
    );

    nodes.stop().await;
}

// ===========================================================================
// 6. The in-flight depth is pipelined AND bounded
// ===========================================================================

/// Contract: with depth K = 2, two frames are in flight to the owner at
/// once (a second session is dialed while the first frame is parked at
/// the owner), and the K+1'th frame WAITS — no third session is ever
/// dialed; it ships on the first session the moment that frame
/// completes. The owner is parked by holding the served inos' serve
/// stripes (`test_lock_serve_ino`), which is exactly where a served
/// layout publish parks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_in_flight_frame_depth_is_pipelined_and_bounded() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::with_depth(NODE, SECRET.to_vec(), 2);
    // No warm-up: a fresh lane, so every session this test sees is one
    // the depth bound admitted.
    let nodes = TwoNodes::start(dir.path(), pc).await;
    let inos = mint(&nodes.owner_be, 3, "depth").await;
    let (x, y, z) = (inos[0], inos[1], inos[2]);
    // The publish plane's own dial count (the listener's admissions also
    // count the custody plane's sessions).
    let dials = || publish::stats().ship_session_dials;
    let served_frames = || publish::stats().served_frames;
    let (base, frames0) = (dials(), served_frames());

    let park_x = publish::test_lock_serve_ino(x).await;
    let park_y = publish::test_lock_serve_ino(y).await;
    let submit = |ino: u64| {
        let be = Arc::clone(&nodes.client_be);
        tokio::spawn(async move {
            publish::set_layout_and_size(&be, ino, &layout_bytes(4096), 4096, &[]).await
        })
    };

    // Frame 1 (X) dials the first session and parks at the owner.
    let hx = submit(x);
    assert!(
        eventually(Duration::from_secs(3), || served_frames() == frames0 + 1).await,
        "X's frame reached the owner"
    );
    // Frame 2 (Y) must go out WHILE X is parked: a second session dials
    // and the owner decodes a second frame.
    let hy = submit(y);
    assert!(
        eventually(Duration::from_secs(3), || served_frames() == frames0 + 2).await,
        "depth 2: the second frame is in flight beside the parked first one — got {} \
         frames at the owner, {} publish sessions dialed",
        served_frames() - frames0,
        dials() - base
    );
    assert_eq!(dials(), base + 2, "two frames in flight = two sessions");
    // Frame 3 (Z) is the K+1'th: it waits — never reaches the owner and
    // never dials a third session while two are in flight.
    let waits0 = publish::stats().ship_depth_waits;
    let hz = submit(z);
    assert!(
        !eventually(Duration::from_millis(300), || {
            served_frames() > frames0 + 2 || dials() > base + 2
        })
        .await,
        "the depth bound: a third frame neither reaches the owner nor dials a third session \
         while two are in flight"
    );
    assert!(!hz.is_finished(), "Z has not shipped while X and Y park");
    assert_eq!(
        publish::stats().ship_depth_waits - waits0,
        1,
        "the drain parked once on the depth bound (Z)"
    );

    // Release X: its frame completes, and Z ships on the SAME session.
    drop(park_x);
    hx.await.expect("no panic").expect("X applies");
    hz.await
        .expect("no panic")
        .expect("Z applies once a slot frees");
    assert_eq!(dials(), base + 2, "Z reused X's session — still two");
    assert_eq!(served_frames(), frames0 + 3);
    drop(park_y);
    hy.await.expect("no panic").expect("Y applies");
    for &ino in &inos {
        assert_eq!(owner_size(&nodes.owner_be, ino).await, 4096);
    }

    nodes.stop().await;
}
