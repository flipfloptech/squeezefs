//! Rung 9 — **ARM DLM stage S8** (metadata function shipping) and pin the
//! arming seams (`docs/design-full-multi-writer.md` §7 rung 9, rows
//! **S8-a**/**S8-b**; `docs/pre-rc-engineering-spec.md` §6.9 S8 + §6.10 R1).
//!
//! S8's machinery (`src/meta_ship/`) was built and suite-proven but never
//! ARMED in production: the authority's `AsyncVerbRouter` served only the
//! custody and publish blocks (never `VERB_META_BATCH`), and the co-writer
//! daemon's `Metadata`-trait verbs (`unlink`/`rename`/`setattr`/xattrs/…)
//! executed against the local backend, where the co-writer write gate
//! refuses them (`cowriter.local_commit_refusals` — "S8's un-routed-daemon
//! gap meeting a real workload", AGENTS.md). The design's S8-b falsifier is
//! "any un-routed local commit", so the arm has two halves and this file
//! pins both:
//!
//! 1. **The authority serves the S8 verb block** on the SAME listener that
//!    serves custody + publish (`AsyncVerbRouter::with_meta` — the
//!    production composition `arm_multi_writer` registers).
//! 2. **The daemon's trait verbs route through the ship plane** — the hook
//!    lives at the `Metadata` impl on `RoutedMetaBackend` itself (correct
//!    by construction for every caller: no call site can be forgotten), it
//!    consults the process-global router `cowriter::arm` installs, and it
//!    costs one relaxed load on every unarmed mount (the solo re-gate).
//!
//! What one process cannot pin — the netem RTT ladder, the published
//! serial `tar -x` A/B (spec R1: "published even if it regresses") and the
//! K=5 crucible — is the rig's (`tests/run_mw_matrix.sh s8-serial-ab` /
//! `s8-crucible`), evidence `.benchmarks/2026-08-16-mw-s8-arm.md`.

use squeezefs::cluster_wire as cw;
use squeezefs::cowriter::{
    self, AdmissionRequest, AuthorityLeaseEvidence, RegistrantEvidence, VolumeAdmissionEvidence,
};
use squeezefs::data_grant::AsyncVerbRouter;
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::WriterClaim;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_co_writer, Metadata, RoutedMetaBackend,
};
use squeezefs::meta_ship::{self as ship, MetaShipRouter, MetaShipService, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const SECRET: &[u8] = b"s8-arm-storage-trust-enrollment-secret";
const FILE: u32 = libc::S_IFREG | 0o644;

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global plane everywhere)
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

/// A panicking assertion must never leave the binary armed.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ship::uninstall_daemon_verb_router();
        ship::disarm_ownership();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes + admission evidence (the dlm_cowriter_tests construction)
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn stamp_capabilities(path: &Path) {
    for (what, res) in [
        ("durable-term", sb::set_durable_term_bit(path).await),
        (
            "durable-block-refcounts",
            sb::set_block_refcounts_bit(path).await,
        ),
        (
            "durable-layout-versions",
            sb::set_layout_versions_bit(path).await,
        ),
        ("ino-lanes", sb::set_ino_lanes_bit(path).await),
        (
            "block-key-incarnation",
            sb::set_block_key_incarnation_bit(path).await,
        ),
        (
            "partitioned-append",
            sb::set_partitioned_append_bit(path).await,
        ),
        (
            "writer-scoped-staging",
            sb::set_writer_scoped_staging_bit(path).await,
        ),
        ("claim-set", sb::set_claim_set_bit(path).await),
        (
            "multi-writer-data",
            sb::set_multi_writer_data_bit(path).await,
        ),
    ] {
        res.unwrap_or_else(|e| panic!("stamping {what} failed: {e}"));
    }
}

async fn fresh_volume(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    format_v3(&p, VOL_LEN, &opts()).await.unwrap();
    stamp_capabilities(&p).await;
    p
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn member(id: &str, role: MemberRole, pr_key: u64) -> ClaimSetMember {
    ClaimSetMember {
        identity: MemberIdentity {
            id: id.to_string(),
            role,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key,
        },
        ts: now_secs(),
    }
}

fn full_request(paths: &[PathBuf]) -> AdmissionRequest {
    let node_id = "node_00000000deadbeef";
    let owner_id = "authority-membership-owner";
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.members
        .push(member(owner_id, MemberRole::Writer, 0xA0A0));
    set.members
        .push(member(node_id, MemberRole::Writer, 0xB0B0));
    AdmissionRequest {
        multi_writer: true,
        role_co_writer: true,
        read_only: false,
        node_id: node_id.to_string(),
        custody_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: paths
            .iter()
            .map(|p| VolumeAdmissionEvidence {
                path: p.to_path_buf(),
                features_incompat: cowriter::REQUIRED_INCOMPAT,
                claim: Some(WriterClaim {
                    id: "authority-claim".to_string(),
                    ts: now_secs(),
                    pid: 4242,
                    boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
                    term: 7,
                }),
                claim_set: Some(set.clone()),
            })
            .collect(),
        authority: Some(AuthorityLeaseEvidence {
            owner_id: owner_id.to_string(),
            endpoint: "127.0.0.1:7000".to_string(),
            owner_claim_id: String::new(),
            term: 7,
            live: true,
            member_epoch: 3,
        }),
        registrant: Some(RegistrantEvidence {
            pr_capable: true,
            wero: true,
            reservation_held: true,
            registered: true,
            key: 0xB0B0,
            namespaces: 1,
        }),
    }
}

/// The AUTHORITY half exactly as `arm_multi_writer` composes it since the
/// S8 arm: ONE listener whose `AsyncVerbRouter` carries the S8 metadata
/// block beside the (here elided) custody + publish blocks.
fn start_authority_listener(
    authority: &Arc<RoutedMetaBackend>,
) -> (Arc<cw::RpcListener>, Arc<MetaShipService>, String) {
    let svc = MetaShipService::new(Arc::clone(authority));
    let router = AsyncVerbRouter::new().with_meta(Arc::clone(&svc));
    let listener = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority's verb listener starts");
    let endpoint = listener.endpoint().to_string();
    (listener, svc, endpoint)
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

// ===========================================================================
// 1. The authority serves the S8 verb block on the production listener
// ===========================================================================

/// Contract (arm half 1): `AsyncVerbRouter::with_meta` claims
/// `VERB_META_BATCH..=VERB_RECLAIM` for the `MetaShipService`, so the ONE
/// listener `arm_multi_writer` starts answers shipped metadata frames
/// beside custody + publish — a `MetaShipRouter` client can execute a
/// create on the authority through it, end to end on the real wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_authority_listener_serves_the_s8_verb_block() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "auth-meta0").await;
    let authority = open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("the authority mounts");
    let (listener, svc, endpoint) = start_authority_listener(&authority);

    // A second set standing in for the client node.
    let cvol = fresh_volume(dir.path(), "client-meta0").await;
    let client = open_routed_meta_set(&[cvol.display().to_string()])
        .await
        .expect("client set opens");
    let map = OwnerMap::for_volumes(
        &client,
        vec![(0, PeerOwner::new("the-authority", endpoint.clone()))],
    )
    .expect("all-foreign owner map");
    ship::arm_ownership(map);
    let router = MetaShipRouter::new(Arc::clone(&client), "client-a", SECRET.to_vec());

    let inode = router
        .create(1, "s8-armed.txt", FILE, 0, 0)
        .await
        .expect("a shipped create executes through the production verb router");
    assert!(inode.ino >= 2);
    let on_owner = authority
        .lookup(1, "s8-armed.txt")
        .await
        .expect("the create landed on the AUTHORITY");
    assert_eq!(on_owner.ino, inode.ino);
    assert_eq!(
        svc.stats().served,
        1,
        "the owner's service ledger counts it"
    );

    ship::disarm_ownership();
    listener.shutdown();
    shutdown(&client).await;
    shutdown(&authority).await;
}

// ===========================================================================
// 2. The co-writer daemon's trait verbs SHIP instead of refusing
// ===========================================================================

/// Contract (arm half 2 — the core of the rung): with the ownership plane
/// armed and the daemon verb router installed (exactly what
/// `cowriter::arm` does since the S8 arm), the `Metadata`-trait verbs
/// called on the co-writer's OWN `RoutedMetaBackend` — the FUSE daemon's
/// exact receiver — ship to the authority and execute there. Before the
/// arm they refused at the co-writer write gate
/// (`cowriter.local_commit_refusals`), which is the "un-routed local
/// commit" the S8-b falsifier names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_daemons_trait_verbs_ship_instead_of_refusing() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0").await;

    let authority = open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("the authority mounts");
    let (listener, svc, endpoint) = start_authority_listener(&authority);

    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let co = open_routed_meta_set_co_writer(&[vol.display().to_string()], &admission)
        .await
        .expect("the co-writer's routed set opens");

    // What `cowriter::arm` installs: the all-foreign map + the daemon verb
    // router over the co-writer's own backend.
    let map = OwnerMap::for_volumes(
        &co,
        vec![(0, PeerOwner::new("the-authority", endpoint.clone()))],
    )
    .expect("all-foreign owner map");
    ship::arm_ownership(map);
    ship::install_daemon_verb_router(MetaShipRouter::new(
        Arc::clone(&co),
        "node_00000000deadbeef.m00000001",
        SECRET.to_vec(),
    ));

    let refusals_before = METRICS
        .cowriter_local_commit_refusals
        .load(Ordering::SeqCst);
    let before = ship::stats();

    // The daemon's shape: trait verbs on the backend Arc itself.
    let created = co
        .create(1, "crucible.txt", FILE, 0, 0)
        .await
        .expect("create SHIPS instead of refusing at the write gate");
    let after_set = co
        .setattr(
            created.ino,
            Some(0o600),
            None,
            None,
            None,
            None,
            Some(4242),
            None,
        )
        .await
        .expect("setattr ships");
    assert_eq!(after_set.mode & 0o777, 0o600);
    co.setxattr(created.ino, "user.s8", b"armed")
        .await
        .expect("setxattr ships");
    co.rename(1, "crucible.txt", 1, "crucible-renamed.txt", 0)
        .await
        .expect("rename ships");
    let seen = co
        .lookup(1, "crucible-renamed.txt")
        .await
        .expect("the co-writer READS ITS OWN SHIPPED WRITES through the plane");
    assert_eq!(seen.ino, created.ino);
    let gone = co
        .unlink(1, "crucible-renamed.txt")
        .await
        .expect("unlink ships");
    assert_eq!(gone, created.ino);

    // Owner-current state: the authority saw every one of them.
    assert!(
        authority.lookup(1, "crucible-renamed.txt").await.is_err(),
        "the unlink landed on the authority"
    );
    let after = ship::stats();
    assert!(
        after.shipped_verbs - before.shipped_verbs >= 6,
        "every trait verb rode the ship plane (shipped {} -> {})",
        before.shipped_verbs,
        after.shipped_verbs
    );
    assert_eq!(
        after.shipped_verbs - before.shipped_verbs,
        svc.stats().served,
        "the engagement law: the client's shipped == the owner's served"
    );
    assert_eq!(
        METRICS
            .cowriter_local_commit_refusals
            .load(Ordering::SeqCst),
        refusals_before,
        "zero un-routed local commits (the S8-b falsifier)"
    );
    assert_eq!(svc.stats().panics, 0, "owner_panics stays 0");

    ship::uninstall_daemon_verb_router();
    ship::disarm_ownership();
    listener.shutdown();
    shutdown(&co).await;
    shutdown(&authority).await;
}

// ===========================================================================
// 3. The solo re-gate: an unarmed mount never consults the installed hook
// ===========================================================================

/// Contract: the daemon hook is one relaxed load on every unarmed mount —
/// with the plane DISARMED (every mount that ships today), trait verbs on
/// the backend execute locally and the whole `meta_ship` ledger stays 0,
/// even with a stale router still installed (a disarm crossed with an
/// in-flight op must fail safe toward local).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_mounts_trait_verbs_never_touch_the_ship_plane() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "solo-meta0").await;
    let solo = open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("solo mounts");

    // A stale router install with NO armed plane: the relaxed-load gate
    // must stop everything before the router is even loaded.
    ship::install_daemon_verb_router(MetaShipRouter::new(
        Arc::clone(&solo),
        "stale-install",
        SECRET.to_vec(),
    ));

    let before = ship::stats();
    let inode = solo
        .create(1, "solo.txt", FILE, 0, 0)
        .await
        .expect("a solo create executes locally");
    solo.setattr(inode.ino, Some(0o600), None, None, None, None, None, None)
        .await
        .expect("a solo setattr executes locally");
    solo.unlink(1, "solo.txt").await.expect("a solo unlink");
    let after = ship::stats();
    assert_eq!(
        after.shipped_verbs, before.shipped_verbs,
        "nothing shipped on an unarmed mount"
    );
    assert_eq!(after.batches, before.batches, "no frame was even assembled");

    ship::uninstall_daemon_verb_router();
    shutdown(&solo).await;
}

// ===========================================================================
// 4. The hook never bounces the router's deliberate local arms
// ===========================================================================

/// Contract: `MetaShipRouter::lookup` keeps `".."` on the inner backend
/// (the reverse-dentry walk is not expressible on the wire until S10's
/// placement work) — the daemon hook must mirror that carve-out, or the
/// two would bounce a `".."` lookup between each other forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dotdot_lookup_on_an_armed_co_writer_stays_local_and_terminates() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "dotdot-meta0").await;
    let authority = open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("the authority mounts");
    let (listener, _svc, endpoint) = start_authority_listener(&authority);

    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let co = open_routed_meta_set_co_writer(&[vol.display().to_string()], &admission)
        .await
        .expect("co-writer set opens");
    let map = OwnerMap::for_volumes(
        &co,
        vec![(0, PeerOwner::new("the-authority", endpoint.clone()))],
    )
    .expect("all-foreign owner map");
    ship::arm_ownership(map);
    ship::install_daemon_verb_router(MetaShipRouter::new(
        Arc::clone(&co),
        "node_00000000deadbeef.m00000002",
        SECRET.to_vec(),
    ));

    // Must terminate (bounded) and answer the root, not recurse.
    let answered = tokio::time::timeout(std::time::Duration::from_secs(10), co.lookup(1, ".."))
        .await
        .expect("a '..' lookup terminates on an armed co-writer");
    let root = answered.expect("'..' of the root answers the root");
    assert_eq!(root.ino, 1);

    ship::uninstall_daemon_verb_router();
    ship::disarm_ownership();
    listener.shutdown();
    shutdown(&co).await;
    shutdown(&authority).await;
}
