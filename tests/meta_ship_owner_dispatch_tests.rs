//! E2E perf audit campaign **D-1** — DLM structural finding **F-A**
//! (`docs/design-e2e-perf-audit.md` §3 board #1 + Appendix D): the S8
//! owner executed a shipped frame's verbs SERIALLY — `run_batch` awaited
//! each mutating verb's `commit_tx` before starting the next, so a frame
//! of N independent verbs paid N conveyor round trips (N passes, N
//! journal writes, N barriers) instead of co-queuing into ONE M7 pass.
//! That serial per-verb latency is the 9,473 verbs/s authority ceiling
//! and the root of the ≈ 2.6 GiB/s co-writer ingest wall.
//!
//! The lever: dispatch a frame's INDEPENDENT verbs concurrently on the
//! sqz-meta pool so their commits co-queue into the same conveyor pass
//! (the M7 conveyor already batches concurrent committers — union leaf
//! locks, one journal write, one barrier). Verbs that NAME a common inode
//! (`MetaCall::named_inos` overlap, transitively) stay in submission
//! order — the documented in-batch causality ("a create and a lookup of
//! the same name in one frame see each other") is exactly the same-ino
//! case, because production frames coalesce independently in-flight
//! callers and no caller can name an inode a same-frame create has not
//! yet minted.
//!
//! What this binary pins, in order:
//!
//! 1. **The measurement contract** — a 64-verb independent frame commits
//!    in ≤ 4 conveyor passes (`META_CONVEYOR_LEADER_PASSES` delta) and
//!    the group-size instrument reads > 1 tx/pass. RED on the serial
//!    owner (64 passes, group size 1 per verb — the finding's signature),
//!    GREEN with concurrent dispatch. The owner-side wall per frame is
//!    printed as the campaign's in-process row.
//! 2. **Reply ordering + id correlation** are unchanged: result `i` is
//!    op `i`'s, whatever order the verbs completed in.
//! 3. **Per-verb isolation**: one failing verb (ENOENT ino) never fails
//!    its siblings, and the failure lands in ITS slot.
//! 4. **Same-ino verbs stay ordered**: two setattrs on one ino in one
//!    frame — the LATER one's value is what the inode ends with, and a
//!    create → lookup → unlink → lookup chain on one parent still sees
//!    each step (the pre-existing `a_batch_executes_in_submission_order`
//!    contract, re-asserted here under a frame that ALSO carries
//!    independent verbs, so the chain runs beside concurrent siblings).
//! 5. **Exactly-once under concurrent dispatch**: a replayed frame (same
//!    request ids) of concurrently-dispatched mutations answers from the
//!    dedup window — `dedup_hits` grows by N and nothing double-applies.

use squeezefs::cluster_wire as cw;
use squeezefs::meta_backend::kv::{META_CONVEYOR_LEADER_PASSES, META_KV_JOURNAL_ENTRIES};
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend,
};
use squeezefs::meta_ship::{
    self as ship, MetaCall, MetaOp, MetaReply, MetaShipRouter, MetaShipService, OwnerMap, PeerOwner,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const SECRET: &[u8] = b"s8-storage-trust-enrollment-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const FILE: u32 = libc::S_IFREG | 0o644;

/// The ownership plane is process-global; every test here arms it, so
/// they serialize on this lock.
static OWNERSHIP: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ship::disarm_ownership();
        ship::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
    }
}

fn arm(map: Arc<OwnerMap>) -> ArmGuard {
    // Foreign sandboxes (the S8 suite's shape): delegations pinned OFF.
    ship::TEST_DELEGATION_OVERRIDE.store(2, Ordering::SeqCst);
    ship::arm_ownership(map);
    ArmGuard
}

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
    .expect("format stamped meta volume");
    open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set")
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

fn start_owner(
    inner: Arc<RoutedMetaBackend>,
) -> (Arc<cw::RpcListener>, Arc<MetaShipService>, String) {
    let svc = MetaShipService::new(inner);
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), svc.clone())
        .expect("owner-side listener starts");
    let endpoint = listener.endpoint().to_string();
    (listener, svc, endpoint)
}

fn all_foreign(client: &Arc<RoutedMetaBackend>, endpoint: &str) -> Arc<OwnerMap> {
    let foreign: Vec<(usize, PeerOwner)> = (0..client.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", endpoint)))
        .collect();
    OwnerMap::for_volumes(client, foreign).expect("a volume-aligned owner map")
}

struct TwoNodes {
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    svc: Arc<MetaShipService>,
    router: Arc<MetaShipRouter>,
    _armed: ArmGuard,
}

impl TwoNodes {
    async fn start(dir: &Path) -> Self {
        let owner_be = sandbox(dir, "owner").await;
        let client_be = sandbox(dir, "client").await;
        let (listener, svc, endpoint) = start_owner(owner_be.clone());
        let armed = arm(all_foreign(&client_be, &endpoint));
        let router = MetaShipRouter::new(client_be.clone(), "client-1", SECRET.to_vec());
        // Warm the session so measured frames pay no connect.
        router.getattr(1).await.expect("warm");
        Self {
            owner_be,
            client_be,
            listener,
            svc,
            router,
            _armed: armed,
        }
    }

    /// Mint `n` regular files under the root ON THE OWNER (its own local
    /// path — the objects the shipped verbs will name).
    async fn mint(&self, n: usize, prefix: &str) -> Vec<u64> {
        let mut inos = Vec::with_capacity(n);
        for i in 0..n {
            let ino = self
                .owner_be
                .create_with_rdev(1, &format!("{prefix}{i}"), FILE, 0, 0, 0)
                .await
                .expect("owner-local create")
                .ino;
            inos.push(ino);
        }
        inos
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

/// The campaign's frame width: the M7 batch-cap floor (`max(64, cpus×2)`)
/// and the S8 batch cap's floor — the widest frame every machine ships.
const FRAME: usize = 64;

/// The contract's pass ceiling: one pass is the ideal (every verb parks
/// on the conveyor before the pass task drains); the slack covers the
/// frame's ARRIVAL SPREAD — 64 verbs dispatched across the two `sqz-meta`
/// lanes reach the conveyor over ~100s of µs — against the pass task's
/// responsiveness. On the shared pool the pass waited for a lane behind
/// the serves and drained 2–3 batches; since C-2 the pass runs on the
/// volume's own journal lane and takes the first arrivals the instant
/// they land (4–8 passes measured, debug and release). `FRAME / 4` keeps
/// the pin's meaning — ≥ 4 verbs co-queued per pass, an order of magnitude
/// under the serial owner loop's one pass per verb — without encoding one
/// venue's dispatch timing.
const MAX_PASSES: u64 = (FRAME / 4) as u64;

// ---------------------------------------------------------------------------
// 1. The measurement contract — passes per frame
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_frame_of_independent_verbs_commits_in_few_conveyor_passes() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let nodes = TwoNodes::start(dir.path()).await;
    let inos = nodes.mint(FRAME, "f").await;

    // Let the mint's own commits settle so the measured deltas are the
    // frame's alone.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let ops: Vec<MetaOp> = inos
        .iter()
        .map(|&ino| nodes.setattr_mode(ino, libc::S_IFREG | 0o600))
        .collect();
    let peer = nodes
        .router
        .owner_for_ino(1)
        .expect("ino 1 is foreign here");

    let passes_0 = META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst);
    let entries_0 = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst);
    let served_0 = nodes.svc.stats().served;
    let t = Instant::now();
    let results = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("one frame");
    let wall = t.elapsed();
    let passes = META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst) - passes_0;
    let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::SeqCst) - entries_0;
    let served = nodes.svc.stats().served - served_0;

    println!(
        "D-1 row: frame={FRAME} verbs — wall {:?} ({:.1} µs/verb) — conveyor passes {passes} \
         — journal entries {entries} — served {served}",
        wall,
        wall.as_secs_f64() * 1e6 / FRAME as f64
    );

    assert_eq!(results.len(), FRAME, "one result per op");
    for (op, res) in ops.iter().zip(&results) {
        assert_eq!(op.id, res.id, "results are id-correlated, in order");
        assert!(
            matches!(&res.outcome, Ok(MetaReply::Inode(i)) if i.mode & 0o777 == 0o600),
            "verb {} must apply: {:?}",
            op.id,
            res.outcome
        );
    }
    assert_eq!(served, FRAME as u64, "every verb accounted served");
    // One tx = one checksummed journal entry is UNCHANGED: entries per
    // frame stays N. What collapses is the PASS count.
    assert_eq!(
        entries, FRAME as u64,
        "one tx = one journal entry (the on-disk law is untouched)"
    );
    assert!(
        passes <= MAX_PASSES,
        "F-A: a {FRAME}-verb frame of INDEPENDENT verbs must co-queue into ≤ {MAX_PASSES} \
         conveyor passes, got {passes} (≈ 1 per verb = the serial owner loop)"
    );

    nodes.stop().await;
}

// ---------------------------------------------------------------------------
// 2 + 3. Ordered replies, per-verb isolation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_failing_verb_isolates_and_replies_stay_in_op_order() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let nodes = TwoNodes::start(dir.path()).await;
    let inos = nodes.mint(16, "iso").await;
    let peer = nodes.router.owner_for_ino(1).expect("foreign");

    // Slot 5 names an ino that does not exist; slot 11 too — both must
    // fail in THEIR slots and nothing else.
    let missing_a = 0xF00_0001u64;
    let missing_b = 0xF00_0002u64;
    let mut ops: Vec<MetaOp> = inos
        .iter()
        .map(|&ino| nodes.setattr_mode(ino, libc::S_IFREG | 0o640))
        .collect();
    ops.insert(5, nodes.setattr_mode(missing_a, libc::S_IFREG | 0o640));
    ops.insert(11, nodes.setattr_mode(missing_b, libc::S_IFREG | 0o640));

    let results = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("one frame");
    assert_eq!(results.len(), ops.len());
    for (i, (op, res)) in ops.iter().zip(&results).enumerate() {
        assert_eq!(op.id, res.id, "slot {i}: id-correlated in op order");
        if i == 5 || i == 11 {
            assert!(
                res.outcome.is_err(),
                "slot {i} names a missing ino: must fail"
            );
        } else {
            assert!(
                matches!(&res.outcome, Ok(MetaReply::Inode(inode)) if inode.mode & 0o777 == 0o640),
                "slot {i}: a sibling's failure must never fail this verb: {:?}",
                res.outcome
            );
        }
    }
    // The durable state agrees with the replies.
    for &ino in &inos {
        let inode = nodes.owner_be.getattr(ino).await.expect("owner getattr");
        assert_eq!(inode.mode & 0o777, 0o640, "ino {ino} applied");
    }

    nodes.stop().await;
}

// ---------------------------------------------------------------------------
// 4. Same-ino verbs stay in submission order beside concurrent siblings
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn same_ino_verbs_stay_ordered_while_independent_siblings_run_concurrently() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let nodes = TwoNodes::start(dir.path()).await;
    let inos = nodes.mint(32, "ord").await;
    let peer = nodes.router.owner_for_ino(1).expect("foreign");
    let target = inos[7];

    // Three setattrs on ONE ino spread across the frame, each carrying a
    // different mode; the last-submitted must be the durable one. Between
    // them, the create → lookup → unlink → lookup chain on parent 1 (the
    // S8 causality contract) plus 31 independent setattrs.
    let mut ops: Vec<MetaOp> = Vec::new();
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o400));
    for &ino in inos.iter().filter(|&&i| i != target).take(10) {
        ops.push(nodes.setattr_mode(ino, libc::S_IFREG | 0o644));
    }
    ops.push(MetaOp {
        id: nodes.router.next_request_id(),
        call: MetaCall::CreateWithRdev {
            parent: 1,
            name: "chain".into(),
            mode: FILE,
            uid: 0,
            gid: 0,
            rdev: 0,
        },
    });
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o440));
    ops.push(MetaOp {
        id: nodes.router.next_request_id(),
        call: MetaCall::LookupDentry {
            parent: 1,
            name: "chain".into(),
        },
    });
    for &ino in inos.iter().filter(|&&i| i != target).skip(10).take(10) {
        ops.push(nodes.setattr_mode(ino, libc::S_IFREG | 0o644));
    }
    ops.push(MetaOp {
        id: nodes.router.next_request_id(),
        call: MetaCall::Unlink {
            parent: 1,
            name: "chain".into(),
        },
    });
    ops.push(nodes.setattr_mode(target, libc::S_IFREG | 0o444));
    ops.push(MetaOp {
        id: nodes.router.next_request_id(),
        call: MetaCall::LookupDentry {
            parent: 1,
            name: "chain".into(),
        },
    });
    for &ino in inos.iter().filter(|&&i| i != target).skip(20) {
        ops.push(nodes.setattr_mode(ino, libc::S_IFREG | 0o644));
    }

    let results = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("one frame");
    assert_eq!(results.len(), ops.len());
    for (i, (op, res)) in ops.iter().zip(&results).enumerate() {
        assert_eq!(op.id, res.id, "slot {i} id-correlated");
    }
    // The same-ino chain: each reply reflects ITS write, in order.
    let modes: Vec<u32> = results
        .iter()
        .zip(&ops)
        .filter(|(_, op)| matches!(op.call, MetaCall::Setattr { ino, .. } if ino == target))
        .map(|(res, _)| match &res.outcome {
            Ok(MetaReply::Inode(i)) => i.mode & 0o777,
            other => panic!("setattr on the target returned {other:?}"),
        })
        .collect();
    assert_eq!(
        modes,
        vec![0o400, 0o440, 0o444],
        "same-ino verbs in submission order"
    );
    let durable = nodes.owner_be.getattr(target).await.expect("owner getattr");
    assert_eq!(
        durable.mode & 0o777,
        0o444,
        "the LAST same-ino verb is durable"
    );

    // The parent-1 chain: create → lookup sees it → unlink → lookup ENOENT.
    let chain: Vec<&squeezefs::meta_ship::MetaOpResult> = results
        .iter()
        .zip(&ops)
        .filter(|(_, op)| {
            matches!(
                op.call,
                MetaCall::CreateWithRdev { .. }
                    | MetaCall::LookupDentry { .. }
                    | MetaCall::Unlink { .. }
            )
        })
        .map(|(res, _)| res)
        .collect();
    assert_eq!(chain.len(), 4);
    let created = match &chain[0].outcome {
        Ok(MetaReply::Inode(i)) => i.ino,
        other => panic!("create returned {other:?}"),
    };
    let looked_up = match &chain[1].outcome {
        Ok(MetaReply::Inode(i)) => i.ino,
        Ok(MetaReply::Ino(ino)) => *ino,
        other => panic!("lookup returned {other:?}"),
    };
    assert_eq!(
        looked_up, created,
        "in-frame lookup observes the earlier create"
    );
    assert!(
        matches!(&chain[2].outcome, Ok(MetaReply::Ino(ino)) if *ino == created),
        "unlink returns the child ino"
    );
    assert!(
        chain[3].outcome.is_err(),
        "lookup after the in-frame unlink is ENOENT"
    );

    // Every independent sibling applied.
    for &ino in inos.iter().filter(|&&i| i != target) {
        let inode = nodes.owner_be.getattr(ino).await.expect("owner getattr");
        assert_eq!(inode.mode & 0o777, 0o644, "sibling ino {ino} applied");
    }

    nodes.stop().await;
}

// ---------------------------------------------------------------------------
// 5. Exactly-once under concurrent dispatch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_replayed_concurrent_frame_answers_from_the_dedup_window() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let nodes = TwoNodes::start(dir.path()).await;
    let peer = nodes.router.owner_for_ino(1).expect("foreign");

    // 16 creates under 16 distinct parents — independent mutations whose
    // replay would answer EEXIST without the window.
    let parents = {
        let mut p = Vec::new();
        for i in 0..16 {
            p.push(
                nodes
                    .owner_be
                    .create_with_rdev(1, &format!("d{i}"), libc::S_IFDIR | 0o755, 0, 0, 0)
                    .await
                    .expect("owner-local mkdir")
                    .ino,
            );
        }
        p
    };
    let ops: Vec<MetaOp> = parents
        .iter()
        .map(|&parent| MetaOp {
            id: nodes.router.next_request_id(),
            call: MetaCall::CreateWithRdev {
                parent,
                name: "once".into(),
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        })
        .collect();

    let hits_0 = nodes.svc.stats().dedup_hits;
    let first = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("first frame");
    let replay = nodes
        .router
        .ship_ops(&peer, ops.clone())
        .await
        .expect("replayed frame");
    let hits = nodes.svc.stats().dedup_hits - hits_0;

    assert_eq!(hits, 16, "every replayed mutation is a dedup hit");
    for (i, (a, b)) in first.iter().zip(&replay).enumerate() {
        assert_eq!(a.id, b.id, "slot {i}");
        assert_eq!(
            a.outcome, b.outcome,
            "slot {i}: the replay answers the ORIGINAL outcome"
        );
        assert!(a.outcome.is_ok(), "slot {i}: the original applied");
    }
    for &parent in &parents {
        let entries = nodes
            .owner_be
            .readdir(parent, 0, 16)
            .await
            .expect("readdir");
        assert_eq!(
            entries.iter().filter(|e| e.name == "once").count(),
            1,
            "parent {parent}: applied exactly once"
        );
    }

    nodes.stop().await;
}
