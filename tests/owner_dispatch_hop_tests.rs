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
//! **The lever** (`SQUEEZEFS_META_SHIP_INLINE_SERVE`, ships OFF): `1` =
//! execute on the ACCEPTING venue — the connection's thread is dedicated
//! and parked for exactly this reply, so the frame's future is polled there
//! and every wake inside it (the conveyor's fan-out, a 4a guard, a
//! blocking hop) unparks that thread directly. `queue_hop` and `wake_hop`
//! are 0 by construction, and the engagement pair `meta_ship.
//! owner_dispatch_{inline,hops}` says which arm served. `0` = the shipped
//! hop — the default since the 2026-09-08 squeeze-test fleet row priced
//! the inline venue's `run` 0.6–0.9 ms slower than the hops it deletes
//! (`src/meta_ship/mod.rs`, the dispatch section). Every contract below
//! sets the arm it judges explicitly. Everything the hop carried is
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
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, TEST_CONVEYOR_HOLD_PRE_DRAIN, TEST_CONVEYOR_HOLD_STAGE,
};
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

/// Contract 1 — **The split sums exactly, and the frame's `dispatch` IS the hop's
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

/// Contract 2 — **The lever's engagement and its zero hops**: on the accepting venue
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

/// Contract 3 — **Chain order + the dedup window + the co-queue law hold on the
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

/// Contract 4 — **The publish plane's dispatches ride the same split, and a frame is
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

/// Contract 5 — **STATUS_PANIC containment on the accepting venue**: a shipped verb
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

/// Contract 5b — **DLM #8's single-connection half** (`SQUEEZEFS_PUBLISH_SHIP_
/// MULTIPLEX`, default on): at depth 4, a burst of 24 concurrent publishes
/// ships ≥ 2 frames in flight on ONE session — `ship_session_dials` grows
/// by exactly 1 for the endpoint, `ship_mux_frames ≡ ship_frames`, every
/// publish lands, one tx = one journal entry. On the control (`0`) the
/// same burst dials up to `depth` request/reply sessions and ships no
/// multiplexed frame — the D-1b shape, byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_depth_costs_one_connection_when_multiplexed_and_depth_when_pooled() {
    let _serial = serial();
    let _restore = restore();
    for (arm, expect_mux) in [("1", true), ("0", false)] {
        let _lever = EnvVarGuard::set(publish::SHIP_MULTIPLEX_ENV, arm);
        let dir = tempfile::tempdir().unwrap();
        let owner_be = sandbox(dir.path(), "own").await;
        let client_be = sandbox(dir.path(), "cli").await;
        let auth = start_authority(Arc::clone(&owner_be));
        let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
            .map(|v| (v, PeerOwner::new(AUTHORITY, &auth.endpoint)))
            .collect();
        ship::arm_ownership(OwnerMap::for_volumes(&client_be, foreign).expect("owner map"));
        // Depth 4, explicitly: the shape the pool would pay 4 sessions for.
        publish::install_client(publish::PublishClient::with_depth(NODE, SECRET.to_vec(), 4));
        let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
            .await
            .expect("the co-writer joins the custody plane");
        data_grant::install_custody_client(Arc::clone(&client));
        let nodes = PublishNodes {
            owner_be,
            client_be,
            auth,
            client,
        };
        let inos = mint(&nodes.owner_be, 24, "mux").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The owner's apply pass is HELD (the M7 seam), so the frames the
        // burst forms stay in flight until the depth bound parks the drain
        // — depth frames provably in flight at once, on both arms.
        TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
        let s0 = publish::stats();
        let live_0 = nodes.auth.listener.stats().live_connections;
        let entries_0 = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst);
        // Arrivals spaced apart so each early one finds the drain idle and
        // ships as its own frame (the streaming shape, one save at a time)
        // until `depth` frames are parked at the held owner.
        let mut handles = Vec::with_capacity(inos.len());
        for &ino in &inos {
            let be = Arc::clone(&nodes.client_be);
            handles.push(tokio::spawn(async move {
                publish::set_layout_and_size(&be, ino, &layout_bytes(8192), 8192, &[]).await
            }));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while publish::stats().ship_depth_waits == s0.ship_depth_waits {
            assert!(
                std::time::Instant::now() < deadline,
                "arm {arm}: the drain never reached the depth bound"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let inflight_dials = publish::stats().ship_session_dials - s0.ship_session_dials;
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
        let mut outcomes = Vec::with_capacity(handles.len());
        for h in handles {
            outcomes.push(h.await.expect("a publish task never panics"));
        }
        let s1 = publish::stats();
        let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst) - entries_0;

        for (ino, out) in inos.iter().zip(&outcomes) {
            assert!(
                out.is_ok(),
                "arm {arm}: ino {ino}: the publish lands: {out:?}"
            );
        }
        assert_eq!(entries, 24, "arm {arm}: one tx = one journal entry");
        let frames = s1.ship_frames - s0.ship_frames;
        let dials = s1.ship_session_dials - s0.ship_session_dials;
        let mux = s1.ship_mux_frames - s0.ship_mux_frames;
        println!(
            "D-5 mux row (multiplex={arm}): 24 publishes — frames {frames}, dials {dials} (at \
             the depth bound: {inflight_dials}), mux frames {mux}, depth waits {}",
            s1.ship_depth_waits - s0.ship_depth_waits
        );
        assert!(
            frames >= 4,
            "arm {arm}: depth frames were in flight before the drain parked: {frames}"
        );
        if expect_mux {
            assert_eq!(
                inflight_dials, 1,
                "multiplexed: depth 4 in flight costs ONE connection"
            );
            assert_eq!(dials, 1, "…and the burst's tail reused it");
            assert_eq!(mux, frames, "every frame rode the pipelined session");
        } else {
            assert_eq!(mux, 0, "the pool ships no multiplexed frame");
            assert_eq!(
                inflight_dials, 4,
                "the pool dials one session per in-flight frame — depth connections"
            );
        }
        // Every dialed publish session is a live connection — one owner
        // thread each — on top of the custody plane's.
        assert_eq!(
            nodes.auth.listener.stats().live_connections - live_0,
            dials,
            "arm {arm}: the authority holds one thread per live session"
        );
        nodes.stop().await;
        reset_planes();
    }
}

// ===========================================================================
// The lane hog — the fleet's saturated sqz-meta lanes, made a controlled
// load (the C-2 harness's shape: N detached tasks on the sqz-meta pool,
// each spinning `burst` of CPU per poll and yielding). Every wake delivered
// onto a hogged lane waits behind the bursts queued ahead of it — the
// run-queue term the finding names.
// ===========================================================================

fn spin_for(d: Duration) {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < d {
        std::hint::spin_loop();
    }
}

struct LaneHog {
    stop: Arc<AtomicBool>,
}

impl LaneHog {
    fn start(tasks: usize, burst: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..tasks {
            let stop = stop.clone();
            squeezefs::meta_exec::spawn_meta("d5_lane_hog", async move {
                while !stop.load(Ordering::Relaxed) {
                    spin_for(burst);
                    tokio::task::yield_now().await;
                }
            });
        }
        Self { stop }
    }
}

impl Drop for LaneHog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Let the last bursts drain before the next row arms its own.
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The per-bucket DELTA of a histogram export.
fn bucket_delta(after: &serde_json::Value, before: &serde_json::Value) -> Vec<u64> {
    squeezefs::latency_core::LATENCY_BUCKET_LABELS
        .iter()
        .map(|l| {
            after["buckets"][*l].as_u64().unwrap_or(0) - before["buckets"][*l].as_u64().unwrap_or(0)
        })
        .collect()
}

/// The share (0..=1) of a bucketed delta's samples in buckets whose LOWER
/// bound is ≥ `threshold_us` — samples that took at least that long
/// (bucket `i` covers `(2^(i-1), 2^i]` µs; the C-2 harness's statistic: a
/// mean is skewed by one multi-ms stall on a loaded gate box, a share of
/// burst-late samples is not).
fn share_at_or_above_us(d: &[u64], threshold_us: u64) -> f64 {
    let total: u64 = d.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let above: u64 = d
        .iter()
        .enumerate()
        .filter(|(i, _)| (if *i == 0 { 0 } else { 1u64 << (i - 1) }) >= threshold_us)
        .map(|(_, c)| *c)
        .sum();
    above as f64 / total as f64
}

/// The bucketed p99 (µs, upper bucket bound) of a histogram DELTA.
fn p99_us(after: &serde_json::Value, before: &serde_json::Value) -> u64 {
    let d = bucket_delta(after, before);
    let total: u64 = d.iter().sum();
    if total == 0 {
        return 0;
    }
    let rank = total - total / 100;
    let mut seen = 0;
    for (i, c) in d.iter().enumerate() {
        seen += c;
        if seen >= rank {
            return if i == 0 { 1 } else { 1u64 << i };
        }
    }
    unreachable!()
}

/// One S8 row: `clients` concurrent routers (each its own identity and
/// session) × `per` one-verb setattr frames against the one authority.
struct S8Row {
    frames: u64,
    wall: Duration,
    split: (Split, Split),
    dispatch_p99_us: u64,
    /// The row's `dispatch` bucket delta (the burst-share statistic's input).
    dispatch_buckets: Vec<u64>,
}

impl S8Row {
    fn verbs_per_s(&self) -> f64 {
        self.frames as f64 / self.wall.as_secs_f64()
    }
    fn mean(&self, f: impl Fn(&Split) -> (u64, u64)) -> f64 {
        mean_us(delta(f(&self.split.1), f(&self.split.0)))
    }
    fn line(&self, label: &str) -> String {
        format!(
            "{label:<22} verbs/s {:>8.0} | dispatch mean {:>8.1} µs p99 {:>6} µs | queue_hop \
             {:>7.1} run {:>8.1} wake_hop {:>7.1} total {:>8.1} µs | inline {} hops {}",
            self.verbs_per_s(),
            self.mean(|s| s.dispatch),
            self.dispatch_p99_us,
            self.mean(|s| s.queue_hop),
            self.mean(|s| s.run),
            self.mean(|s| s.wake_hop),
            self.mean(|s| s.total),
            self.split.1.inline - self.split.0.inline,
            self.split.1.hops - self.split.0.hops,
        )
    }
}

async fn s8_row(
    owner_endpoint: &str,
    client_be: &Arc<RoutedMetaBackend>,
    inos: &[u64],
    clients: usize,
    per: usize,
) -> S8Row {
    let routers: Vec<Arc<MetaShipRouter>> = (0..clients)
        .map(|c| {
            MetaShipRouter::new(
                Arc::clone(client_be),
                &format!("client-{c}"),
                SECRET.to_vec(),
            )
        })
        .collect();
    for r in &routers {
        r.getattr(1).await.expect("warm the session");
    }
    let _ = owner_endpoint;
    let before = split();
    let disp_before = ship::owner_phase_json()["dispatch"].clone();
    let t0 = std::time::Instant::now();
    let tasks: Vec<_> = routers
        .into_iter()
        .enumerate()
        .map(|(c, r)| {
            let inos = inos.to_vec();
            tokio::spawn(async move {
                for i in 0..per {
                    let ino = inos[(c * per + i) % inos.len()];
                    r.setattr(ino, Some(FILE), None, None, None, None, None, None)
                        .await
                        .unwrap_or_else(|e| panic!("client {c} frame {i}: {e}"));
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.expect("client task");
    }
    let wall = t0.elapsed();
    let after = split();
    let disp_after = ship::owner_phase_json()["dispatch"].clone();
    S8Row {
        frames: (clients * per) as u64,
        wall,
        split: (before, after),
        dispatch_p99_us: p99_us(&disp_after, &disp_before),
        dispatch_buckets: bucket_delta(&disp_after, &disp_before),
    }
}

/// Contract 7 — **The isolation contract**: under four 2 ms serve bursts saturating
/// both `sqz-meta` lanes, a frame served on the accepting venue never waits
/// for a lane, while the hop control lands behind the bursts (the RED shape
/// this campaign attacks: on the fleet 2.0–2.3 ms per verb). The statistic
/// is the C-2 harness's burst SHARE — the fraction of dispatches that took
/// at least one full burst: a hop-landed frame waits ≥ a burst nearly every
/// time (share ≈ 1), an isolated one only on a scheduler stall of the
/// loaded gate box (a mean would flip on one multi-ms stall) — beside the
/// ratio of the two arms' means.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s8_inline_serve_is_isolated_from_the_lane_hog() {
    let _serial = serial();
    let _restore = restore();
    let dir = tempfile::tempdir().unwrap();
    let nodes = S8Nodes::start(dir.path()).await;
    let inos = mint(&nodes.owner_be, 32, "iso").await;
    let endpoint = nodes.listener.endpoint().to_string();
    const BURST: Duration = Duration::from_millis(2);

    let hog = LaneHog::start(4, BURST);
    let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "0");
    let control = s8_row(&endpoint, &nodes.client_be, &inos, 2, 32).await;
    drop(_lever);
    let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, "1");
    let inline = s8_row(&endpoint, &nodes.client_be, &inos, 2, 32).await;
    drop(hog);
    println!("D-5 isolation (4 × 2 ms lane hog, 2 clients × 32 one-verb frames):");
    println!("  {}", control.line("hop (control)"));
    println!("  {}", inline.line("inline"));
    let burst_us = BURST.as_micros() as u64;
    let inline_share = share_at_or_above_us(&inline.dispatch_buckets, burst_us);
    let control_share = share_at_or_above_us(&control.dispatch_buckets, burst_us);
    println!(
        "  burst-late share (≥ {BURST:?}): control {control_share:.2}, inline {inline_share:.2}"
    );
    assert!(
        inline_share < 0.25,
        "a frame served on the accepting venue never waits for a hogged lane: {:.0} % of its \
         dispatches took ≥ one {BURST:?} burst (the hop control: {:.0} %)",
        inline_share * 100.0,
        control_share * 100.0
    );
    assert!(
        inline.mean(|s| s.dispatch) * 2.0 < control.mean(|s| s.dispatch),
        "the lane's cost is removed, not moved: inline dispatch mean {:.1} µs vs the hop's {:.1}",
        inline.mean(|s| s.dispatch),
        control.mean(|s| s.dispatch)
    );
    assert_eq!(
        inline.split.1.hops, inline.split.0.hops,
        "inline paid no hop"
    );
    nodes.stop().await;
}

/// **The counted A/B rows** (run by name, release — a measurement is not a
/// gate):
///
/// ```text
/// cargo test --release --all-features --test owner_dispatch_hop_tests -- \
///     --ignored --nocapture ab_rows
/// ```
///
/// Four legs in A-B-B-A order per venue (inline / hop / hop / inline):
/// quiet, then under a 4 × 200 µs serve hog on the `sqz-meta` lanes (the
/// fleet's saturated lanes at a controlled burst). Columns: verbs/s,
/// `meta_ship_owner_phase_ns.dispatch` mean/p99, the split's means, and
/// the engagement pair. Plus the publish plane's row: 24 concurrent layout
/// publishes × 8 rounds from one co-writer, inline vs hop, under the hog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement, not a gate: run by name (see the doc comment)"]
async fn ab_rows() {
    let _serial = serial();
    let _restore = restore();
    let dir = tempfile::tempdir().unwrap();
    let nodes = S8Nodes::start(dir.path()).await;
    let inos = mint(&nodes.owner_be, 64, "ab").await;
    let endpoint = nodes.listener.endpoint().to_string();
    const CLIENTS: usize = 4;
    const PER: usize = 256;
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "D-5 A/B rows ({profile}, {} cpus): {CLIENTS} clients × {PER} one-verb setattr frames",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    for (venue, hog) in [
        ("quiet", None),
        ("hog 4 × 200 µs", Some((4usize, Duration::from_micros(200)))),
    ] {
        let _hog = hog.map(|(tasks, burst)| LaneHog::start(tasks, burst));
        println!("-- {venue}");
        for arm in ["1", "0", "0", "1"] {
            let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, arm);
            let row = s8_row(&endpoint, &nodes.client_be, &inos, CLIENTS, PER).await;
            println!("  {}", row.line(if arm == "1" { "inline" } else { "hop" }));
        }
    }
    nodes.stop().await;

    // The publish plane.
    let dir = tempfile::tempdir().unwrap();
    let nodes = PublishNodes::start(dir.path()).await;
    let warm = mint(&nodes.owner_be, 8, "warm").await;
    for round in 1..=3u64 {
        for out in nodes.publish_concurrently(&warm, round).await {
            out.expect("the warm publish lands");
        }
    }
    let inos = mint(&nodes.owner_be, 24, "pub").await;
    println!("-- publish plane: 24 concurrent layout publishes × 8 rounds, one co-writer");
    for (venue, hog) in [
        ("quiet", None),
        ("hog 4 × 200 µs", Some((4usize, Duration::from_micros(200)))),
    ] {
        let _hog = hog.map(|(tasks, burst)| LaneHog::start(tasks, burst));
        for arm in ["1", "0", "0", "1"] {
            let _lever = EnvVarGuard::set(ship::INLINE_SERVE_ENV, arm);
            let before = split();
            let frames_0 = nodes.auth.listener.stats().requests_served;
            let t0 = std::time::Instant::now();
            for round in 1..=8u64 {
                for out in nodes.publish_concurrently(&inos, round * 4096).await {
                    out.expect("the publish lands");
                }
            }
            let wall = t0.elapsed();
            let after = split();
            let frames = nodes.auth.listener.stats().requests_served - frames_0;
            println!(
                "  {venue:<16} {:<7} publishes/s {:>7.0} | wall/round {:>8.1} µs | frames {frames} \
                 | dispatch run {:>8.1} µs queue_hop {:>7.1} wake_hop {:>7.1} total {:>8.1} µs \
                 (n={}) | inline {} hops {}",
                if arm == "1" { "inline" } else { "hop" },
                (24 * 8) as f64 / wall.as_secs_f64(),
                wall.as_secs_f64() * 1e6 / 8.0,
                mean_us(delta(after.run, before.run)),
                mean_us(delta(after.queue_hop, before.queue_hop)),
                mean_us(delta(after.wake_hop, before.wake_hop)),
                mean_us(delta(after.total, before.total)),
                after.total.1 - before.total.1,
                after.inline - before.inline,
                after.hops - before.hops,
            );
        }
    }

    // DLM #8's single-connection half: the same publish burst on ONE
    // pipelined session vs the D-1b session pool, equal depth, A-B-B-A. A
    // fresh client per leg (the lever is read at the lane's first frame);
    // each leg warms its own session(s) first.
    println!("-- publish plane: multiplexed session vs session pool (equal depth)");
    for arm in ["1", "0", "0", "1"] {
        let _lever = EnvVarGuard::set(publish::SHIP_MULTIPLEX_ENV, arm);
        publish::install_client(publish::PublishClient::new(NODE, SECRET.to_vec()));
        for round in 1..=3u64 {
            for out in nodes.publish_concurrently(&warm, round * 64).await {
                out.expect("the warm publish lands");
            }
        }
        let s0 = publish::stats();
        let t0 = std::time::Instant::now();
        for round in 1..=8u64 {
            for out in nodes.publish_concurrently(&inos, round * 8192).await {
                out.expect("the publish lands");
            }
        }
        let wall = t0.elapsed();
        let s1 = publish::stats();
        println!(
            "  {:<7} publishes/s {:>7.0} | wall/round {:>8.1} µs | frames {} mux {} dials {} depth \
             waits {}",
            if arm == "1" { "mux" } else { "pool" },
            (24 * 8) as f64 / wall.as_secs_f64(),
            wall.as_secs_f64() * 1e6 / 8.0,
            s1.ship_frames - s0.ship_frames,
            s1.ship_mux_frames - s0.ship_mux_frames,
            s1.ship_session_dials - s0.ship_session_dials,
            s1.ship_depth_waits - s0.ship_depth_waits,
        );
    }
    nodes.stop().await;
}

/// Contract 6 — **The stats-inode shape**: `meta_ship_owner_dispatch_ns` exports the
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
