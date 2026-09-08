//! E2E perf audit campaign **D-5** — DLM board **#7**: the owner's
//! `spawn_meta_join` dispatch hop (`docs/design-e2e-perf-audit.md` §3.4,
//! §5.3 row 18, Appendix D #7).
//!
//! **The finding** (C-2's fleet attribution, `.benchmarks/2026-09-03-c2-
//! uring-fs-completion-hop.md` §Owed 3): a served frame is admitted on the
//! connection's own thread (`sqz-clw-conn`), then DISPATCHED onto the two
//! shared `sqz-meta` lanes and awaited — a cross-thread wake into the
//! lanes the co-writers' publish storms saturate, and back. Quiet it reads
//! ≤ 32 µs; on the fleet `meta_ship_owner_phase_ns.dispatch` read **2.0–
//! 2.3 ms per verb**, the owner's largest remaining term once the conveyor
//! stopped binding.
//!
//! **The instrument** (`meta_ship_owner_dispatch_ns`, always-on, exact-sum,
//! zero-alloc — the `uring_fs_write_phase_ns` pattern): every owner-side
//! dispatch, on BOTH planes (the S8 verb frame, the S9 publish call /
//! group / free / harvest), records `queue_hop` (submitted → the lane's
//! first poll), `run` (first poll → the work's last instruction),
//! `wake_hop` (done → the awaiting connection thread resumed) and `total`
//! (≡ Σ). On the S8 plane the frame-level `meta_ship_owner_phase_ns.
//! dispatch` is that hop's `total` on every single-chain frame.
//!
//! **The lever** (`SQUEEZEFS_META_SHIP_INLINE_SERVE`, default on): execute
//! on the ACCEPTING venue — the connection's thread is dedicated and
//! parked for exactly this reply, so the frame's future is polled there
//! and every wake inside it (the conveyor's fan-out, a 4a guard, a
//! blocking hop) unparks that thread directly. `queue_hop` and `wake_hop`
//! are 0 by construction, and the engagement pair `meta_ship.
//! owner_dispatch_{inline,hops}` says which arm served. `0` = the shipped
//! hop (the same-binary A/B control). Everything the hop carried is
//! preserved and pinned here: chain order, the dedup window, one conveyor
//! group per publish frame, and the STATUS_PANIC containment (a verb that
//! unwinds on the accepting venue answers PANIC and the session survives).
//!
//! Suite runs `--test-threads=1` (process-global ownership + ledgers).

use squeezefs::cluster_wire as cw;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::{META_CONVEYOR_LEADER_PASSES, META_KV_JOURNAL_ENTRIES};
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend,
};
use squeezefs::meta_ship::{
    self as ship, publish, MetaCall, MetaOp, MetaReply, MetaShipRouter, MetaShipService,
    OwnerDispatchPhase, OwnerMap, OwnerPhase, PeerOwner,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SECRET: &[u8] = b"d5-owner-hop-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const FILE: u32 = libc::S_IFREG | 0o644;
const NODE: &str = "node-d5-cowriter";
const AUTHORITY: &str = "d5-authority";

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

fn reset_planes() {
    data_grant::uninstall_custody_client();
    data_grant::uninstall_custody_owner();
    publish::uninstall_client();
    publish::uninstall_free_executor();
    ship::disarm_ownership();
    ship::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
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

/// Scoped env override (the conveyor_tests pattern). The owner reads the
/// venue lever per served frame, so a guard wrapping the row flips it.
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

/// Mint `n` regular files ON THE OWNER (the objects shipped verbs name).
async fn mint(owner_be: &Arc<RoutedMetaBackend>, n: usize, prefix: &str) -> Vec<u64> {
    let mut inos = Vec::with_capacity(n);
    for i in 0..n {
        let ino = owner_be
            .create_with_rdev(1, &format!("{prefix}{i}"), FILE, 0, 0, 0)
            .await
            .expect("owner-local create")
            .ino;
        inos.push(ino);
    }
    inos
}

// ---------------------------------------------------------------------------
// The instrument's words
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Split {
    queue_hop: (u64, u64),
    run: (u64, u64),
    wake_hop: (u64, u64),
    total: (u64, u64),
    /// The S8 frame-level `meta_ship_owner_phase_ns.dispatch`.
    dispatch: (u64, u64),
    inline: u64,
    hops: u64,
}

fn split() -> Split {
    let s = ship::stats();
    Split {
        queue_hop: ship::owner_dispatch_totals(OwnerDispatchPhase::QueueHop),
        run: ship::owner_dispatch_totals(OwnerDispatchPhase::Run),
        wake_hop: ship::owner_dispatch_totals(OwnerDispatchPhase::WakeHop),
        total: ship::owner_dispatch_totals(OwnerDispatchPhase::Total),
        dispatch: ship::owner_phase_totals(OwnerPhase::Dispatch),
        inline: s.owner_dispatch_inline,
        hops: s.owner_dispatch_hops,
    }
}

fn delta(after: (u64, u64), before: (u64, u64)) -> (u64, u64) {
    (after.0 - before.0, after.1 - before.1)
}

fn mean_us(d: (u64, u64)) -> f64 {
    if d.1 == 0 {
        0.0
    } else {
        d.0 as f64 / d.1 as f64 / 1e3
    }
}

/// The exact-sum law over a row: `queue_hop + run + wake_hop ≡ total`, to
/// the ns, and one sample per phase per dispatch.
fn assert_exact_sum(before: &Split, after: &Split, dispatches: u64) {
    let q = delta(after.queue_hop, before.queue_hop);
    let r = delta(after.run, before.run);
    let w = delta(after.wake_hop, before.wake_hop);
    let t = delta(after.total, before.total);
    assert_eq!(t.1, dispatches, "one `total` sample per dispatch");
    assert_eq!(q.1, dispatches, "one `queue_hop` sample per dispatch");
    assert_eq!(r.1, dispatches, "one `run` sample per dispatch");
    assert_eq!(w.1, dispatches, "one `wake_hop` sample per dispatch");
    assert_eq!(
        q.0 + r.0 + w.0,
        t.0,
        "queue_hop + run + wake_hop ≡ total (exact, ns): {} + {} + {} vs {}",
        q.0,
        r.0,
        w.0,
        t.0
    );
}

// ===========================================================================
// The S8 verb plane
// ===========================================================================

struct S8Nodes {
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    svc: Arc<MetaShipService>,
    router: Arc<MetaShipRouter>,
}

impl S8Nodes {
    async fn start(dir: &Path) -> Self {
        let owner_be = sandbox(dir, "owner").await;
        let client_be = sandbox(dir, "client").await;
        let svc = MetaShipService::new(Arc::clone(&owner_be));
        let cfg = cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        };
        let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), svc.clone())
            .expect("owner-side listener starts");
        let endpoint = listener.endpoint().to_string();
        // Foreign sandboxes (the S8 suite's shape): delegations pinned OFF.
        ship::TEST_DELEGATION_OVERRIDE.store(2, Ordering::SeqCst);
        let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
            .map(|v| (v, PeerOwner::new("owner-a", &endpoint)))
            .collect();
        ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign).expect("owner map"));
        let router = MetaShipRouter::new(Arc::clone(&client_be), "client-1", SECRET.to_vec());
        // Warm the session so measured frames pay no connect.
        router.getattr(1).await.expect("warm");
        Self {
            owner_be,
            client_be,
            listener,
            svc,
            router,
        }
    }

    fn setattr_mode(&self, ino: u64, mode: u32) -> MetaOp {
        MetaOp {
            id: self.router.next_request_id(),
            call: MetaCall::Setattr {
                ino,
                mode: Some(mode),
                uid: None,
                gid: None,
                size: None,
                atime: None,
                mtime: None,
                ctime: None,
            },
        }
    }

    async fn stop(self) {
        self.listener.shutdown();
        shutdown(&self.client_be).await;
        shutdown(&self.owner_be).await;
    }
}

/// 1. **The split sums exactly, and the frame's `dispatch` IS the hop's
/// `total`** on single-chain frames — on both arms of the lever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_dispatch_split_sums_exactly_and_the_frame_dispatch_is_its_total() {
    let _serial = serial();
    let _restore = restore();
    for arm in ["1", "0"] {
        let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, arm);
        let dir = tempfile::tempdir().unwrap();
        let nodes = S8Nodes::start(dir.path()).await;
        let inos = mint(&nodes.owner_be, 16, "one").await;

        let before = split();
        for &ino in &inos {
            nodes.router.getattr(ino).await.expect("a one-verb frame");
        }
        let after = split();

        let frames = inos.len() as u64;
        assert_exact_sum(&before, &after, frames);
        let d = delta(after.dispatch, before.dispatch);
        let t = delta(after.total, before.total);
        assert_eq!(d.1, frames, "arm {arm}: one frame-level dispatch per frame");
        assert_eq!(
            d.0, t.0,
            "arm {arm}: the frame's `dispatch` ≡ its one hop's `total` (exact, ns)"
        );
        println!(
            "D-5 split row (inline={arm}): frames {frames} — queue_hop {:.1} µs, run {:.1} µs, \
             wake_hop {:.1} µs, total {:.1} µs",
            mean_us(delta(after.queue_hop, before.queue_hop)),
            mean_us(delta(after.run, before.run)),
            mean_us(delta(after.wake_hop, before.wake_hop)),
            mean_us(t),
        );
        nodes.stop().await;
    }
}

/// 2. **The lever's engagement and its zero hops**: on the accepting venue
/// `queue_hop` and `wake_hop` are 0 to the ns and `owner_dispatch_inline`
/// accounts every frame; the control arm pays the hops and accounts them on
/// `owner_dispatch_hops`. Nothing else in the ledger moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_inline_serve_has_zero_hops_and_the_control_pays_them() {
    let _serial = serial();
    let _restore = restore();
    let dir = tempfile::tempdir().unwrap();
    let nodes = S8Nodes::start(dir.path()).await;
    let inos = mint(&nodes.owner_be, 8, "eng").await;
    let frames = inos.len() as u64;

    {
        let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "1");
        let before = split();
        for &ino in &inos {
            nodes
                .router
                .setattr(ino, Some(FILE), None, None, None, None, None, None)
                .await
                .expect("inline-served setattr");
        }
        let after = split();
        assert_eq!(after.inline - before.inline, frames, "every frame inline");
        assert_eq!(after.hops - before.hops, 0, "no hop on the inline arm");
        assert_eq!(
            delta(after.queue_hop, before.queue_hop).0,
            0,
            "inline: queue_hop ≡ 0 by construction"
        );
        assert_eq!(
            delta(after.wake_hop, before.wake_hop).0,
            0,
            "inline: wake_hop ≡ 0 by construction"
        );
        assert!(
            delta(after.run, before.run).0 > 0,
            "inline: the work itself is what `run` reads"
        );
    }
    {
        let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "0");
        let before = split();
        for &ino in &inos {
            nodes
                .router
                .setattr(ino, Some(FILE), None, None, None, None, None, None)
                .await
                .expect("hop-served setattr");
        }
        let after = split();
        assert_eq!(after.hops - before.hops, frames, "every frame hopped");
        assert_eq!(after.inline - before.inline, 0, "no inline on the control");
        assert!(
            delta(after.queue_hop, before.queue_hop).0 + delta(after.wake_hop, before.wake_hop).0
                > 0,
            "the control arm crosses two threads — its hops are never 0"
        );
    }
    nodes.stop().await;
}

/// 3. **Chain order + the dedup window + the co-queue law hold on the
/// accepting venue** — the D-1 contracts re-asserted with the lever ON: a
/// same-ino chain applies in submission order beside 61 independent
/// siblings that co-queue (≤ FRAME/4 passes, FRAME journal entries), and
/// the replayed frame answers from the window without a second apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_inline_keeps_chain_order_the_dedup_window_and_the_co_queue() {
    let _serial = serial();
    let _restore = restore();
    let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "1");
    let dir = tempfile::tempdir().unwrap();
    let nodes = S8Nodes::start(dir.path()).await;
    const FRAME: usize = 64;
    let inos = mint(&nodes.owner_be, FRAME, "chain").await;
    let target = inos[3];
    let peer = nodes.router.owner_for_ino(1).expect("foreign");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 3 same-ino setattrs (0o400 → 0o440 → 0o444) spread across 61
    // independent setattrs: two chains, one of length 3.
    let mut ops: Vec<MetaOp> = Vec::new();
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o400));
    let others: Vec<u64> = inos.iter().copied().filter(|&i| i != target).collect();
    for &ino in &others[..30] {
        ops.push(nodes.setattr_mode(ino, libc::S_IFREG | 0o600));
    }
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o440));
    for &ino in &others[30..] {
        ops.push(nodes.setattr_mode(ino, libc::S_IFREG | 0o600));
    }
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o444));
    assert_eq!(ops.len(), FRAME + 2);

    let passes_0 = META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst);
    let entries_0 = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst);
    let hits_0 = nodes.svc.stats().dedup_hits;
    let before = split();
    let first = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("the frame");
    let passes = META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst) - passes_0;
    let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst) - entries_0;
    let after = split();
    assert_eq!(
        after.inline - before.inline,
        1,
        "one inline dispatch per frame"
    );
    assert_eq!(after.hops - before.hops, 0);
    assert_exact_sum(&before, &after, 1);

    let modes: Vec<u32> = first
        .iter()
        .zip(&ops)
        .filter(|(_, op)| matches!(op.call, MetaCall::Setattr { ino, .. } if ino == target))
        .map(|(res, _)| match &res.outcome {
            Ok(MetaReply::Inode(i)) => i.mode & 0o777,
            other => panic!("setattr on the target returned {other:?}"),
        })
        .collect();
    assert_eq!(modes, vec![0o400, 0o440, 0o444], "same-ino verbs in order");
    let durable = nodes.owner_be.getattr(target).await.expect("owner getattr");
    assert_eq!(
        durable.mode & 0o777,
        0o444,
        "the LAST same-ino verb is durable"
    );
    for &ino in &others {
        let inode = nodes.owner_be.getattr(ino).await.expect("owner getattr");
        assert_eq!(inode.mode & 0o777, 0o600, "sibling {ino} applied");
    }
    assert_eq!(
        entries,
        (FRAME + 2) as u64,
        "one tx = one journal entry, untouched"
    );
    assert!(
        passes <= (FRAME / 4) as u64,
        "the independent siblings co-queue on the accepting venue: {passes} passes for \
         {FRAME} independent verbs (D-1's ≤ FRAME/4 law)"
    );

    // The replay: the window answers every mutation from the winner.
    let entries_1 = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst);
    let replay = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("the replayed frame");
    assert_eq!(
        nodes.svc.stats().dedup_hits - hits_0,
        ops.len() as u64,
        "every replayed mutation is a dedup hit"
    );
    assert_eq!(
        META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst),
        entries_1,
        "the replay applies nothing"
    );
    for (i, (a, b)) in first.iter().zip(&replay).enumerate() {
        assert_eq!(a.id, b.id, "slot {i}");
        assert_eq!(a.outcome, b.outcome, "slot {i}: the ORIGINAL outcome");
    }
    nodes.stop().await;
}

// ===========================================================================
// The S9 publish plane
// ===========================================================================

struct Authority {
    listener: Arc<cw::RpcListener>,
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
    let listener = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority { listener, endpoint }
}

struct PublishNodes {
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    auth: Authority,
    client: Arc<WriteCustodyClient>,
}

impl PublishNodes {
    async fn start(dir: &Path) -> Self {
        let owner_be = sandbox(dir, "own").await;
        let client_be = sandbox(dir, "cli").await;
        let auth = start_authority(Arc::clone(&owner_be));
        let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
            .map(|v| (v, PeerOwner::new(AUTHORITY, &auth.endpoint)))
            .collect();
        ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign).expect("owner map"));
        publish::install_client(publish::PublishClient::new(NODE, SECRET.to_vec()));
        let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
            .await
            .expect("the co-writer joins the custody plane");
        data_grant::install_custody_client(Arc::clone(&client));
        Self {
            owner_be,
            client_be,
            auth,
            client,
        }
    }

    /// Ship `inos.len()` layout publishes CONCURRENTLY (the streaming
    /// co-writer's shape), returning every outcome.
    async fn publish_concurrently(
        &self,
        inos: &[u64],
        size: u64,
    ) -> Vec<squeezefs::error::Result<publish::OwnerVerdict>> {
        let handles: Vec<_> = inos
            .iter()
            .map(|&ino| {
                let be = Arc::clone(&self.client_be);
                tokio::spawn(async move {
                    publish::set_layout_and_size(&be, ino, &layout_bytes(size), size, &[]).await
                })
            })
            .collect();
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.expect("a publish task never panics"));
        }
        out
    }

    async fn stop(self) {
        self.auth.listener.shutdown();
        shutdown(&self.owner_be).await;
        shutdown(&self.client_be).await;
    }
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

/// 4. **The publish plane's dispatches ride the same split, and a frame is
/// still one conveyor group on the accepting venue**: 24 concurrent
/// publishes → every outcome lands, 24 journal entries, ≥ 1 frame group,
/// every dispatch inline (0 hops, `queue_hop ≡ wake_hop ≡ 0`), the split
/// exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_dispatches_ride_the_split_and_a_frame_is_one_group_inline() {
    let _serial = serial();
    let _restore = restore();
    let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "1");
    let dir = tempfile::tempdir().unwrap();
    let nodes = PublishNodes::start(dir.path()).await;
    // Warm the session pool against inos the row never touches.
    let warm = mint(&nodes.owner_be, 8, "warm").await;
    for round in 1..=3u64 {
        for out in nodes.publish_concurrently(&warm, round).await {
            out.expect("the warm publish lands");
        }
    }
    let inos = mint(&nodes.owner_be, 24, "pub").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let before = split();
    let entries_0 = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst);
    let groups_0 = publish::stats().frame_groups;
    let outcomes = nodes.publish_concurrently(&inos, 4096).await;
    let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst) - entries_0;
    let after = split();

    for (ino, out) in inos.iter().zip(&outcomes) {
        assert!(out.is_ok(), "ino {ino}: the publish lands: {out:?}");
    }
    for &ino in &inos {
        assert_eq!(
            nodes
                .owner_be
                .getattr(ino)
                .await
                .expect("owner has it")
                .size,
            4096
        );
    }
    assert_eq!(entries, 24, "one tx = one journal entry, untouched");
    assert!(
        publish::stats().frame_groups > groups_0,
        "the D-1c group law holds on the accepting venue"
    );
    let dispatches = after.inline - before.inline;
    assert!(
        dispatches >= 1,
        "the publish plane dispatches through the instrument"
    );
    assert_eq!(after.hops - before.hops, 0, "no hop under the lever");
    assert_exact_sum(&before, &after, dispatches);
    assert_eq!(delta(after.queue_hop, before.queue_hop).0, 0);
    assert_eq!(delta(after.wake_hop, before.wake_hop).0, 0);
    println!(
        "D-5 publish row (inline): 24 publishes — dispatches {dispatches}, run {:.1} µs mean",
        mean_us(delta(after.run, before.run))
    );
    nodes.stop().await;
}

/// 5. **STATUS_PANIC containment on the accepting venue**: a shipped verb
/// whose executor UNWINDS answers the loud PANIC outcome (counted on
/// `owner_panics`, cached in the witness window), and the connection
/// thread SURVIVES — the next call on the same pooled session lands with
/// no re-dial. Before the lever this rode `contain` on the sqz-meta lane;
/// inline it must be the same contract or the session dies with the verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_panic_inline_answers_status_panic_and_the_session_survives() {
    let _serial = serial();
    let _restore = restore();
    let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "1");
    let dir = tempfile::tempdir().unwrap();
    let nodes = PublishNodes::start(dir.path()).await;
    let epoch = nodes.client.lease_epoch();
    let vol_tag = 0xD5u64;

    // Warm ONE session (a benign executor).
    publish::install_free_executor(Arc::new(|_, blocks: Vec<u64>| {
        Box::pin(async move { Ok(vec![publish::FreeVerdict::Freed; blocks.len()]) })
    }));
    publish::ship_free_blocks(&nodes.auth.endpoint, vol_tag, vec![1], epoch, 1)
        .await
        .expect("the warm free lands");

    // The unwinding executor.
    publish::install_free_executor(Arc::new(|_, _blocks: Vec<u64>| {
        Box::pin(async move { panic!("d5: an executor that unwinds") })
    }));
    let panics_0 = publish::stats().panics;
    let dials_0 = publish::stats().ship_session_dials;
    let live_0 = nodes.auth.listener.stats().live_connections;
    let err = publish::ship_free_blocks(&nodes.auth.endpoint, vol_tag, vec![2], epoch, 2)
        .await
        .expect_err("the unwound verb refuses loud");
    assert!(
        format!("{err}").contains("panicked"),
        "the outcome names the unwind: {err}"
    );
    assert_eq!(publish::stats().panics - panics_0, 1, "counted once");

    // The session survived: the next call rides the SAME pooled session.
    publish::install_free_executor(Arc::new(|_, blocks: Vec<u64>| {
        Box::pin(async move { Ok(vec![publish::FreeVerdict::Freed; blocks.len()]) })
    }));
    let verdicts = publish::ship_free_blocks(&nodes.auth.endpoint, vol_tag, vec![3], epoch, 3)
        .await
        .expect("the session serves on");
    assert_eq!(verdicts, vec![publish::FreeVerdict::Freed]);
    assert_eq!(
        publish::stats().ship_session_dials - dials_0,
        0,
        "no re-dial: the connection thread survived the unwind"
    );
    assert_eq!(
        nodes.auth.listener.stats().live_connections,
        live_0,
        "the listener's live sessions are unchanged"
    );
    // The replay of the unwound id answers the SAME cached failure.
    let again = publish::ship_free_blocks(&nodes.auth.endpoint, vol_tag, vec![2], epoch, 2)
        .await
        .expect_err("the witness caches the unwind");
    assert!(format!("{again}").contains("panicked"));
    assert_eq!(
        publish::stats().panics - panics_0,
        1,
        "the replay re-runs nothing"
    );
    nodes.stop().await;
}

/// 6. **The stats-inode shape**: `meta_ship_owner_dispatch_ns` exports the
/// four phases in the ONE histogram shape (buckets + count + sum_ns +
/// mean_ns), Σ buckets ≡ count; the engagement pair is on `meta_ship`.
#[test]
fn owner_dispatch_family_exports_the_exact_shape() {
    let fam = ship::owner_dispatch_json();
    let obj = fam.as_object().expect("family object");
    assert_eq!(obj.len(), 4);
    for phase in ["queue_hop", "run", "wake_hop", "total"] {
        let h = obj
            .get(phase)
            .unwrap_or_else(|| panic!("phase {phase} present"));
        for k in ["buckets", "count", "sum_ns", "mean_ns"] {
            assert!(h.get(k).is_some(), "phase {phase} lacks {k}");
        }
        let buckets: u64 = h["buckets"]
            .as_object()
            .expect("buckets")
            .values()
            .map(|v| v.as_u64().unwrap_or(0))
            .sum();
        assert_eq!(buckets, h["count"].as_u64().unwrap(), "Σ buckets ≡ count");
    }
    let ledger = ship::stats_json();
    assert!(ledger.get("owner_dispatch_inline").is_some());
    assert!(ledger.get("owner_dispatch_hops").is_some());
}
