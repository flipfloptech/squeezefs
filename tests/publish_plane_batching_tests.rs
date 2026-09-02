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
//!   delta (the M7 conveyor's pass count);
//! * **journal entries** — `META_KV_JOURNAL_ENTRIES` delta (one tx = one
//!   checksummed entry: the count that must NOT collapse);
//! * **wall** — spawn-to-join over the whole concurrent set.

use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::{META_CONVEYOR_LEADER_PASSES, META_KV_JOURNAL_ENTRIES};
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

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

fn restore() -> Restore {
    Restore
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

/// The three counters the rows read, sampled together.
struct Counters {
    frames: u64,
    passes: u64,
    entries: u64,
}

fn counters(auth: &Authority) -> Counters {
    Counters {
        frames: auth.listener.stats().requests_served,
        passes: META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed),
        entries: META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
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

    /// Warm the publish session so measured frames pay no connect (the
    /// D-1 discipline), against an ino the row never touches.
    async fn warm(&self) {
        let warm = mint(&self.owner_be, 1, "warm").await[0];
        publish::set_layout_and_size(&self.client_be, warm, &layout_bytes(1), 1, &[])
            .await
            .expect("the warm publish lands");
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
        let handles: Vec<_> = inos
            .iter()
            .enumerate()
            .map(|(i, &ino)| {
                let be = Arc::clone(&self.client_be);
                let size = size_of(i);
                tokio::spawn(async move {
                    publish::set_layout_and_size(&be, ino, &layout_bytes(size), size, &[]).await
                })
            })
            .collect();
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.expect("a publish task never panics"));
        }
        (out, started.elapsed())
    }

    async fn stop(self) {
        self.auth.listener.shutdown();
        shutdown(&self.owner_be).await;
        shutdown(&self.client_be).await;
    }
}

// ===========================================================================
// 1. The measurement contract — the campaign's in-process row
// ===========================================================================

/// **The publish plane's shape under a 24-ino concurrent co-writer.**
///
/// Today (dev tip): `PublishClient::ship` encodes ONE call per
/// `PublishRequestFrame` and serializes every ship on the endpoint's
/// session mutex, so 24 concurrent publishes are 24 stop-and-wait round
/// trips — **frames/publish = 1.0** — and the owner sees them one at a
/// time, so its conveyor runs **one pass per publish** (passes/publish
/// ≈ 1.0: nothing co-queues). The journal-entry count is N by law (one
/// tx = one checksummed entry) and must stay N under any fix.
///
/// The row is printed as the in-process face of the S9-a wall; the
/// assertions pin today's shape so the D-1b fix FLIPS them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measurement_row_24_concurrent_publishes_from_one_co_writer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    let nodes = TwoNodes::start(dir.path(), pc).await;
    nodes.warm().await;
    let inos = mint(&nodes.owner_be, INOS, "stream").await;

    let before = counters(&nodes.auth);
    let (outcomes, wall) = nodes
        .publish_concurrently(&inos, |i| 4096 * (i as u64 + 1))
        .await;
    let after = counters(&nodes.auth);

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
    println!(
        "D-1b in-process row (debug build, file-backed KV sandbox, loopback wire): \
         {INOS} concurrent publishes from one co-writer -> frames {frames} \
         ({:.2}/publish), owner conveyor passes {passes} ({:.2}/publish), journal entries \
         {entries}, wall {:.2} ms ({:.1} us/publish)",
        frames as f64 / n,
        passes as f64 / n,
        wall.as_secs_f64() * 1e3,
        wall.as_secs_f64() * 1e6 / n
    );

    assert_eq!(
        entries, INOS as u64,
        "one tx = one checksummed journal entry — the count that never collapses"
    );
    // TODAY's shape (the finding's signature): one frame per publish and
    // one conveyor pass per publish — the plane is stop-and-wait depth 1.
    assert_eq!(
        frames, INOS as u64,
        "dev tip: every publish is its own stop-and-wait frame"
    );
    assert!(
        passes >= INOS as u64 - 2,
        "dev tip: the owner commits the publishes one pass at a time (got {passes} for {INOS})"
    );

    nodes.stop().await;
}
