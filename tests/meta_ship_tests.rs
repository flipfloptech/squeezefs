//! DLM stage **S8 — metadata function shipping**
//! (`docs/pre-rc-engineering-spec.md` §6.7 decision 1 + §6.9 stage S8 +
//! §6.10 R1; `docs/pre-rc-execution-plan.md` Phase 4, rulings **D10**
//! (R1's serial-latency risk ACCEPTED) and **D11** (the measured half —
//! the published `tar -x` A/B — deferred)).
//!
//! Spec §6.7 decision 1, verbatim: *"Metadata authority is **ownable, not
//! lockable**. The KV engine is RAM-authoritative and single-writer by
//! construction, so metadata mutations are **function-shipped to the
//! volume's owner**, not lock-shipped. The 4a `DlmGuard`s stay exactly
//! where they are."* This binary pins that sentence, in the order the
//! shipping layer performs it:
//!
//! 1. **Routing decides LOCALLY and cheaply.** A verb's target volume is
//!    the slot map's answer (`RoutedMetaBackend::route_ino` — one
//!    arc-swap load) and ownership is one more lock-free load. An
//!    unarmed mount — every mount that ships today — pays ONE relaxed
//!    load and then takes today's path verbatim: no session, no frame,
//!    no round trip.
//! 2. **The verbs ride the ONE cluster transport** (S3 `cluster_wire`):
//!    authenticated frames on the pinned `sqz-cluster-svc{n}` lanes,
//!    never a second port and never a mock.
//! 3. **Pipelining is the batch**, not the connection: concurrent
//!    submissions to one owner coalesce into one frame, and a frame's
//!    ops execute in submission order on the owner, so causality inside
//!    a batch is the submission order.
//! 4. **Idempotency is a request id plus an owner-side dedup window**: a
//!    retried `create`/`unlink`/`rename` after a lost reply returns the
//!    ORIGINAL outcome and applies exactly once.
//! 5. **Fencing and failure**: a stale-term request is refused (never
//!    executed), the successor's grace window admits reclaim and refuses
//!    fresh mutations, and a fencing READ on a foreign-home object never
//!    serves the local view (**the S4 contract #2 this stage owes**).
//! 6. **Cross-OWNER shapes refuse loud naming S3.5** (ruling D4's
//!    cross-volume transaction machinery, which is NOT built) — never a
//!    second distributed-tx mechanism invented here.
//! 7. **Ownership granularity is the VOLUME** (spec §6.10 R4). An
//!    intra-volume split is *unrepresentable* in the API, because one
//!    volume still has one journal ring, one bitmap, one root ledger
//!    (§6.2 items 2/3/4 — bit 8's partitioned append is built but NOT
//!    stamped) and one node cache (the `revalidate.rs` third-gate-state
//!    residual).
//!
//! Test-process discipline: the ownership plane is process-global (it is
//! a mount-wide property), so every test that arms it takes
//! [`OWNERSHIP`] exclusively and restores solo on drop. Two "nodes" live
//! in one process — the owner's backend plus a client whose own set is
//! never touched — so what is genuinely exercised is the wire, the
//! encoding, the batching, the dedup window, the term gate and the grace
//! window. What one process CANNOT exercise is two independent node
//! caches diverging; that is a real-fabric row, and D11 defers it.

use squeezefs::cluster_wire as cw;
use squeezefs::meta_backend::kv::superblock::FEATURES_INCOMPAT_KNOWN;
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend,
};
use squeezefs::meta_ship::{
    self as ship, MetaCall, MetaOp, MetaReply, MetaShipRouter, MetaShipService, MetaVerb, OwnerMap,
    PeerOwner, VerbRoute,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// The `job:enroll` storage-trust secret both halves prove possession of
/// (S3's root of trust — whoever can read the shared metadata volume is
/// inside the trust domain).
const SECRET: &[u8] = b"s8-storage-trust-enrollment-secret";

const VOL_LEN: u64 = 256 * 1024 * 1024;

/// Arming the ownership plane is process-global: writers take this
/// exclusively, readers (which only assert the solo posture) share it.
static OWNERSHIP: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Restores solo mode on drop, so a panicking test can never leave the
/// binary's other tests armed.
struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ship::disarm_ownership();
    }
}

fn arm(map: Arc<OwnerMap>) -> ArmGuard {
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

/// A formatted, opened metadata volume set — one "node's" view.
async fn sandbox(dir: &Path, tag: &str, volumes: usize) -> Arc<RoutedMetaBackend> {
    let plan = plan_meta_slot_set(volumes).expect("derived plan");
    let mut uris = Vec::new();
    for i in 0..volumes {
        let p = make_file(dir, &format!("{tag}-meta{i}"), VOL_LEN);
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            &p,
            VOL_LEN,
            &opts(),
            plan.stamps[i].clone(),
        )
        .await
        .expect("format stamped meta volume");
        uris.push(p.display().to_string());
    }
    open_routed_meta_set(&uris).await.expect("open routed set")
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

/// The owner half: a `MetaShipService` over `inner`, served on the S3
/// wire's pinned lanes.
fn start_owner(
    inner: Arc<RoutedMetaBackend>,
) -> (Arc<cw::RpcListener>, Arc<MetaShipService>, String) {
    let svc = MetaShipService::new(inner, tokio::runtime::Handle::current());
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

fn router(inner: Arc<RoutedMetaBackend>, id: &str) -> Arc<MetaShipRouter> {
    MetaShipRouter::new(inner, id, SECRET.to_vec())
}

/// Every volume of `client` is owned by the peer at `endpoint`.
fn all_foreign(client: &Arc<RoutedMetaBackend>, endpoint: &str) -> Arc<OwnerMap> {
    let foreign: Vec<(usize, PeerOwner)> = (0..client.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", endpoint)))
        .collect();
    OwnerMap::for_volumes(client, foreign).expect("a volume-aligned owner map")
}

fn dir_names(entries: &[squeezefs::meta_backend::DirEntry]) -> HashSet<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

const FILE: u32 = libc::S_IFREG | 0o644;
const DIR: u32 = libc::S_IFDIR | 0o755;

// ---------------------------------------------------------------------------
// 1. The wire vocabulary IS the trait surface
// ---------------------------------------------------------------------------

/// Contract: every `Metadata` member the stage ships has a wire verb, the
/// verb numbers do not collide with S3's ping, and the mutating/read
/// classification — which is what the dedup window and the grace gate key
/// on — is the documented one.
///
/// The census (from `src/meta_backend/mod.rs`): 13 required trait members
/// plus the PROVIDED `create` (which delegates to `create_with_rdev`, so
/// it needs no wire verb of its own). `lookup` ships as **two** verbs —
/// `LookupDentry` ⊕ `Getattr` — because its two participants (the
/// parent's dentry, the child's inode) can home on different owners; the
/// composition is what the trait's `lookup` already does internally, and
/// it was never atomic (mod.rs says so). There is no `statfs` member on
/// the trait, so none ships.
#[test]
fn the_wire_vocabulary_covers_every_shipped_trait_member() {
    assert_eq!(
        MetaVerb::ALL.len(),
        13,
        "13 wire verbs cover the trait's 13 required members ({:?})",
        MetaVerb::ALL
    );
    let mut seen = HashSet::new();
    for verb in MetaVerb::ALL {
        assert!(seen.insert(verb.code()), "duplicate verb code for {verb:?}");
        assert_eq!(
            MetaVerb::from_code(verb.code()),
            Some(*verb),
            "verb codes must round trip"
        );
        assert!(!verb.name().is_empty());
    }
    // S8's RPC verbs must not collide with S3's ping (a const-evaluable
    // fact, so it is asserted in a const block — the compiler is the gate).
    const _: () = assert!(ship::VERB_META_BATCH != cw::VERB_PING);
    const _: () = assert!(ship::VERB_RECLAIM != cw::VERB_PING);
    assert_eq!(ship::META_SHIP_SCHEMA, 1, "the vocabulary's schema");

    // The dedup window and the grace gate both key on this classification.
    for (verb, mutating) in [
        (MetaVerb::LookupDentry, false),
        (MetaVerb::Getattr, false),
        (MetaVerb::Readdir, false),
        (MetaVerb::Getxattr, false),
        (MetaVerb::Listxattr, false),
        (MetaVerb::CreateWithRdev, true),
        (MetaVerb::Unlink, true),
        (MetaVerb::Link, true),
        (MetaVerb::Rename, true),
        (MetaVerb::Setattr, true),
        (MetaVerb::Setxattr, true),
        (MetaVerb::Removexattr, true),
        (MetaVerb::DestroyInode, true),
    ] {
        assert_eq!(
            verb.mutating(),
            mutating,
            "{verb:?} mutating classification drifted"
        );
    }
}

/// Contract: the frame codec round-trips every verb shape, and an
/// untrusted frame is bounded input — a truncated or garbage body refuses
/// loud instead of panicking or allocating from a lying length.
#[test]
fn frames_round_trip_and_untrusted_bytes_refuse_loud() {
    let ops: Vec<MetaOp> = [
        MetaCall::LookupDentry {
            parent: 1,
            name: "a".into(),
        },
        MetaCall::CreateWithRdev {
            parent: 1,
            name: "b".into(),
            mode: FILE,
            uid: 7,
            gid: 9,
            rdev: 0,
        },
        MetaCall::Unlink {
            parent: 1,
            name: "b".into(),
        },
        MetaCall::Link {
            ino: 4,
            new_parent: 1,
            new_name: "c".into(),
        },
        MetaCall::Rename {
            old_parent: 1,
            old_name: "c".into(),
            new_parent: 1,
            new_name: "d".into(),
            flags: 0,
        },
        MetaCall::Readdir {
            dir: 1,
            offset: 0,
            max: 64,
        },
        MetaCall::Getattr { ino: 1 },
        MetaCall::Setattr {
            ino: 1,
            mode: Some(DIR),
            uid: None,
            gid: None,
            size: Some(11),
            atime: None,
            mtime: Some(3),
            ctime: None,
        },
        MetaCall::Getxattr {
            ino: 1,
            name: "user.x".into(),
        },
        MetaCall::Setxattr {
            ino: 1,
            name: "user.x".into(),
            value: vec![1, 2, 3],
        },
        MetaCall::Removexattr {
            ino: 1,
            name: "user.x".into(),
        },
        MetaCall::Listxattr { ino: 1 },
        MetaCall::DestroyInode { ino: 4 },
    ]
    .into_iter()
    .enumerate()
    .map(|(i, call)| MetaOp {
        id: i as u64 + 1,
        call,
    })
    .collect();
    assert_eq!(ops.len(), MetaVerb::ALL.len(), "one op per wire verb");

    let frame = ship::MetaRequestFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 0xdead_beef,
        owner_term: 3,
        ops,
    };
    let bytes = ship::encode_request(&frame).expect("encode");
    let back = ship::decode_request(&bytes).expect("decode");
    assert_eq!(back.ops, frame.ops, "every verb shape must round trip");
    assert_eq!(back.owner_term, 3);
    assert_eq!(back.client_epoch, 0xdead_beef);

    for bad in [
        &bytes[..bytes.len() / 2],
        &bytes[1..],
        b"not a frame at all",
        &[],
    ] {
        assert!(
            ship::decode_request(bad).is_err(),
            "an untrusted frame must refuse loud, never panic"
        );
    }

    let reply = ship::MetaReplyFrame {
        schema: ship::META_SHIP_SCHEMA,
        owner_term: 4,
        results: vec![
            ship::MetaOpResult {
                id: 1,
                outcome: Ok(MetaReply::Ino(42)),
                grant: Some(ship::TokenGrant {
                    ino: 42,
                    token: 7,
                    term: 4,
                }),
            },
            ship::MetaOpResult {
                id: 2,
                outcome: Err(ship::WireError {
                    errno: libc::ENOENT,
                    msg: "no such thing".into(),
                }),
                grant: None,
            },
        ],
    };
    let rb = ship::encode_reply(&reply).expect("encode reply");
    let rback = ship::decode_reply(&rb).expect("decode reply");
    assert_eq!(rback.results.len(), 2);
    assert!(ship::decode_reply(&rb[..3]).is_err());
}

// ---------------------------------------------------------------------------
// 2. Requirement 1 — a locally-owned target takes TODAY's path
// ---------------------------------------------------------------------------

/// Contract (requirement 1): an **unarmed** mount routes every verb
/// locally with ZERO round trips, ZERO sessions and today's exact
/// semantics. There is no listener anywhere in this test, so a single
/// shipped verb would fail — success IS the proof of locality.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_router_takes_todays_path_with_zero_round_trips() {
    let _serial = OWNERSHIP.read().await;
    let dir = tempfile::tempdir().unwrap();
    let inner = sandbox(dir.path(), "solo", 1).await;
    let r = router(inner.clone(), "solo-node");
    let before = ship::stats();

    assert!(
        !ship::ownership_armed(),
        "no test may leave the plane armed"
    );
    assert!(matches!(
        r.route_verb(&MetaCall::Getattr { ino: 1 }).expect("route"),
        VerbRoute::Local
    ));

    let f = r.create(1, "f", FILE, 1000, 1000).await.expect("create");
    assert_eq!(r.lookup(1, "f").await.expect("lookup").ino, f.ino);
    assert_eq!(r.getattr(f.ino).await.expect("getattr").ino, f.ino);
    r.setxattr(f.ino, "user.k", b"v").await.expect("setxattr");
    assert_eq!(
        r.getxattr(f.ino, "user.k").await.expect("getxattr"),
        Some(b"v".to_vec())
    );
    assert_eq!(r.listxattr(f.ino).await.expect("listxattr"), vec!["user.k"]);
    r.removexattr(f.ino, "user.k").await.expect("removexattr");
    let sized = r
        .setattr(f.ino, None, None, None, Some(4096), None, None, None)
        .await
        .expect("setattr");
    assert_eq!(sized.size, 4096);
    r.link(f.ino, 1, "hard").await.expect("link");
    r.rename(1, "hard", 1, "moved", 0).await.expect("rename");
    let entries = r.readdir(1, 0, 64).await.expect("readdir");
    assert_eq!(
        dir_names(&entries),
        HashSet::from(["f".into(), "moved".into()])
    );
    assert_eq!(r.unlink(1, "moved").await.expect("unlink"), f.ino);
    assert_eq!(r.unlink(1, "f").await.expect("unlink"), f.ino);
    r.destroy_inode(f.ino).await.expect("destroy");

    let after = ship::stats();
    assert_eq!(
        after.shipped_verbs, before.shipped_verbs,
        "an unarmed mount must ship NOTHING"
    );
    assert_eq!(
        after.dlm_rpcs_meta, before.dlm_rpcs_meta,
        "zero metadata round trips on the local path"
    );
    assert!(
        after.local_verbs > before.local_verbs,
        "the local ledger must account for the verbs"
    );
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        0,
        "S8 must not disturb the S4 LOCK ledger on a solo mount"
    );
    shutdown(&inner).await;
}

/// Contract: arming alone does not ship. An owner node's own map lists no
/// foreign volume, so every verb still takes the local path — the same
/// assertion as above with the plane ARMED, which is what proves the
/// routing decision keys on ownership rather than on arming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_owner_of_every_volume_still_takes_todays_path() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let inner = sandbox(dir.path(), "owner", 1).await;
    let map = OwnerMap::for_volumes(&inner, Vec::new()).expect("all-local map");
    let _armed = arm(map);
    assert!(ship::ownership_armed());

    let r = router(inner.clone(), "owner-node");
    let before = ship::stats();
    let f = r.create(1, "own", FILE, 0, 0).await.expect("create");
    assert_eq!(r.lookup(1, "own").await.expect("lookup").ino, f.ino);
    let after = ship::stats();
    assert_eq!(
        after.shipped_verbs, before.shipped_verbs,
        "an owned volume is never shipped to"
    );
    shutdown(&inner).await;
}

// ---------------------------------------------------------------------------
// 3. The full round trip against an in-process owner
// ---------------------------------------------------------------------------

/// Contract: every shipped verb round-trips over the S3 wire and lands in
/// the OWNER's metadata state — asserted through the owner's own backend,
/// not through the reply, so a lying reply cannot pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_shipped_verb_round_trips_against_an_in_process_owner() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, svc, endpoint) = start_owner(owner_be.clone());

    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");
    let before = ship::stats();

    match r.route_verb(&MetaCall::Getattr { ino: 1 }).expect("route") {
        VerbRoute::Ship(peer) => assert_eq!(peer.endpoint, endpoint),
        VerbRoute::Local => panic!("a foreign volume must route to its owner"),
    }

    // create → the owner's tree holds it
    let f = r.create(1, "shipped", FILE, 5, 6).await.expect("create");
    let on_owner = owner_be.lookup(1, "shipped").await.expect("owner lookup");
    assert_eq!(on_owner.ino, f.ino, "the create landed on the OWNER");
    assert_eq!((on_owner.uid, on_owner.gid), (5, 6));

    // lookup (LookupDentry ⊕ Getattr) / getattr
    assert_eq!(r.lookup(1, "shipped").await.expect("lookup").ino, f.ino);
    assert_eq!(r.getattr(f.ino).await.expect("getattr").mode, FILE);
    assert!(
        r.lookup(1, "absent").await.is_err(),
        "a missing dentry is ENOENT across the wire"
    );

    // xattrs
    r.setxattr(f.ino, "user.a", b"1").await.expect("setxattr");
    assert_eq!(
        r.getxattr(f.ino, "user.a").await.expect("getxattr"),
        Some(b"1".to_vec())
    );
    assert_eq!(r.listxattr(f.ino).await.expect("listxattr"), vec!["user.a"]);
    r.removexattr(f.ino, "user.a").await.expect("removexattr");
    assert_eq!(
        owner_be.getxattr(f.ino, "user.a").await.expect("owner get"),
        None,
        "the removal landed on the owner"
    );

    // setattr
    let s = r
        .setattr(f.ino, None, Some(11), None, Some(8192), None, None, None)
        .await
        .expect("setattr");
    assert_eq!((s.uid, s.size), (11, 8192));

    // mkdir + link + rename + readdir + unlink
    let d = r.create(1, "dir", DIR, 0, 0).await.expect("mkdir");
    r.link(f.ino, d.ino, "hard").await.expect("link");
    r.rename(d.ino, "hard", d.ino, "moved", 0)
        .await
        .expect("rename");
    let entries = r.readdir(d.ino, 0, 64).await.expect("readdir");
    assert_eq!(dir_names(&entries), HashSet::from(["moved".to_string()]));
    assert_eq!(r.unlink(d.ino, "moved").await.expect("unlink"), f.ino);
    assert_eq!(r.unlink(1, "shipped").await.expect("unlink"), f.ino);
    r.destroy_inode(f.ino).await.expect("destroy");
    assert!(
        owner_be.getattr(f.ino).await.is_err(),
        "the destroy landed on the owner"
    );

    let after = ship::stats();
    let shipped = after.shipped_verbs - before.shipped_verbs;
    assert!(shipped >= 15, "every verb above shipped: {shipped}");
    assert_eq!(
        after.served_verbs - before.served_verbs,
        shipped,
        "engagement is EXACT: the owner served exactly what the client shipped"
    );
    assert!(
        after.dlm_rpcs_meta > before.dlm_rpcs_meta,
        "shipped frames must be counted as metadata RPCs"
    );
    assert_eq!(after.owner_panics, 0, "must-stay-0 tripwire");
    assert_eq!(after.stale_term_refusals, before.stale_term_refusals);
    assert_eq!(svc.stats().panics, 0);

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 4. S4 contract #2 — the fencing reads
// ---------------------------------------------------------------------------

/// Contract (**S4 contract #2**, the one this stage owes): with a foreign
/// home possible, a fencing READ must not keep serving the local view —
/// that would be a stale-generation answer. The resolution is the
/// **client token cache fed by the piggybacked grant**: every shipped
/// reply carries the object's generation as the owner knows it.
///
/// Proven the hard way: mint a LOCAL grant on the ino first (so the local
/// view has a token that would be served if nothing changed), then arm
/// ownership so the ino homes foreign, then assert the read moves to the
/// owner's answer — and that a COLD read never invents a local token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fencing_read_on_a_foreign_home_never_serves_the_local_view() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, _svc, endpoint) = start_owner(owner_be.clone());

    // A local grant, minted BEFORE arming: this is the value the
    // unhomed read would keep serving.
    let locks = squeezefs::dlm::DlmClient::new().expect("dlm");
    let f = owner_be
        .create(1, "fenced", FILE, 0, 0)
        .await
        .expect("create");
    let path = format!("inode_{}", f.ino);
    let lease = locks
        .acquire_lock(&path, None, Duration::from_secs(5))
        .await
        .expect("local grant");
    let local_token = lease.fencing_token();
    lease.release().await.expect("release");
    assert_eq!(locks.get_fencing_token_ino(f.ino), local_token);

    let _armed = arm(all_foreign(&client_be, &endpoint));
    ship::test_clear_token_cache();
    let cold = ship::token_cache_stats();

    // COLD: no grant has been learned for this object. The read must not
    // serve the local grant, and it must not invent one — it serves the
    // owner-era floor and trips the must-stay-0 miss counter loudly.
    let cold_read = locks.get_fencing_token_ino(f.ino);
    assert_ne!(
        cold_read, local_token,
        "a foreign-home fencing read served the LOCAL view — the S4 #2 bug"
    );
    assert_eq!(
        ship::token_cache_stats().misses,
        cold.misses + 1,
        "a cold foreign read is the must-stay-0 tripwire, counted"
    );

    // WARM: a shipped verb's reply carries the grant, which is what makes
    // the read sound without a separate lock round trip (intent locks).
    let r = router(client_be.clone(), "client-1");
    let seen = r.getattr(f.ino).await.expect("shipped getattr");
    assert_eq!(seen.ino, f.ino);
    let warm = locks.get_fencing_token_ino(f.ino);
    assert_eq!(
        warm,
        ship::owner_authority_token(f.ino),
        "the read must serve the OWNER's generation for the object"
    );
    assert!(
        ship::token_cache_stats().hits > cold.hits,
        "the warm read must be a token-cache hit"
    );

    // The refresh path is a metadata RPC, never a lock RPC (requirement 2).
    let rpcs_before = squeezefs::dlm_slot::dlm_rpcs();
    let refreshed = r.refresh_token(f.ino).await.expect("token refresh");
    assert_eq!(refreshed, warm);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs_before,
        "a token refresh must not become a lock RPC"
    );

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 5. Pipelining — the batch is the unit, order is submission order
// ---------------------------------------------------------------------------

/// Contract (requirement 3): concurrent submissions to one owner
/// **coalesce into one frame**. The drain hold makes it deterministic
/// (the `TEST_LAYOUT_MERGE_HOLD_MS` precedent — a head-of-iteration delay
/// so arrivals accumulate, never a sleep used as coordination).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_submissions_coalesce_into_one_batch() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, _svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");

    // Warm the session so the measured batch pays no connect.
    r.getattr(1).await.expect("warm");
    let before = ship::stats();

    const FAN: u64 = 16;
    ship::TEST_SHIP_DRAIN_HOLD_MS.store(400, Ordering::SeqCst);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..FAN {
        let r = r.clone();
        set.spawn(async move { r.getattr(1).await.map(|i| i.ino) });
    }
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        assert_eq!(joined.expect("task").expect("getattr"), 1);
        ok += 1;
    }
    ship::TEST_SHIP_DRAIN_HOLD_MS.store(0, Ordering::SeqCst);
    assert_eq!(ok, FAN);

    let after = ship::stats();
    let verbs = after.batched_verbs - before.batched_verbs;
    let batches = after.batches - before.batches;
    assert_eq!(verbs, FAN, "every verb must be accounted to a batch");
    assert!(
        (1..FAN).contains(&batches),
        "the pipelining unit is the BATCH: {verbs} verbs in {batches} batches"
    );

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

/// Contract (requirement 3, the causality half): ops inside ONE frame
/// execute in **submission order** on the owner, so a create followed by
/// a lookup of the same name in the same batch observes the create.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_executes_in_submission_order() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, _svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");
    let peer = r.owner_for_ino(1).expect("ino 1 is foreign here");

    let ops = vec![
        MetaOp {
            id: r.next_request_id(),
            call: MetaCall::CreateWithRdev {
                parent: 1,
                name: "ordered".into(),
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        },
        MetaOp {
            id: r.next_request_id(),
            call: MetaCall::LookupDentry {
                parent: 1,
                name: "ordered".into(),
            },
        },
        MetaOp {
            id: r.next_request_id(),
            call: MetaCall::Unlink {
                parent: 1,
                name: "ordered".into(),
            },
        },
        MetaOp {
            id: r.next_request_id(),
            call: MetaCall::LookupDentry {
                parent: 1,
                name: "ordered".into(),
            },
        },
    ];
    let results = r.ship_ops(&peer, ops.clone()).await.expect("one frame");
    assert_eq!(results.len(), 4, "one result per op, in order");
    for (op, res) in ops.iter().zip(&results) {
        assert_eq!(op.id, res.id, "results are id-correlated");
    }
    let created = match &results[0].outcome {
        Ok(MetaReply::Inode(i)) => i.ino,
        other => panic!("create returned {other:?}"),
    };
    assert!(
        matches!(&results[1].outcome, Ok(MetaReply::Ino(ino)) if *ino == created),
        "the in-batch lookup must observe the earlier create: {:?}",
        results[1].outcome
    );
    assert!(
        matches!(&results[2].outcome, Ok(MetaReply::Ino(ino)) if *ino == created),
        "unlink returns the child ino"
    );
    assert!(
        results[3].outcome.is_err(),
        "the lookup after the in-batch unlink must be ENOENT"
    );

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 6. Idempotency — request id + owner-side dedup window
// ---------------------------------------------------------------------------

/// Contract (requirement 6): a retried request id after a lost reply
/// returns the ORIGINAL outcome and applies exactly once. Without the
/// window a replayed `create` returns EEXIST (the caller's create
/// succeeded!) and a replayed `unlink` returns ENOENT — both are lies
/// about an operation that DID happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_request_id_is_served_from_the_dedup_window() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");
    let peer = r.owner_for_ino(1).expect("foreign");
    let before = ship::stats();

    let create = MetaOp {
        id: r.next_request_id(),
        call: MetaCall::CreateWithRdev {
            parent: 1,
            name: "once".into(),
            mode: FILE,
            uid: 0,
            gid: 0,
            rdev: 0,
        },
    };
    let first = r
        .ship_ops(&peer, vec![create.clone()])
        .await
        .expect("first");
    // The reply is "lost": the client re-issues the SAME id.
    let replay = r
        .ship_ops(&peer, vec![create.clone()])
        .await
        .expect("replay");
    assert_eq!(
        first[0].outcome, replay[0].outcome,
        "a replay must return the original outcome, not EEXIST"
    );
    assert_eq!(
        ship::stats().dedup_hits - before.dedup_hits,
        1,
        "the replay must be served from the window"
    );
    let entries = owner_be.readdir(1, 0, 64).await.expect("readdir");
    assert_eq!(
        entries.iter().filter(|e| e.name == "once").count(),
        1,
        "exactly one apply"
    );

    let unlink = MetaOp {
        id: r.next_request_id(),
        call: MetaCall::Unlink {
            parent: 1,
            name: "once".into(),
        },
    };
    let u1 = r
        .ship_ops(&peer, vec![unlink.clone()])
        .await
        .expect("unlink");
    let u2 = r
        .ship_ops(&peer, vec![unlink.clone()])
        .await
        .expect("replay");
    assert!(u1[0].outcome.is_ok(), "the unlink succeeded");
    assert_eq!(
        u1[0].outcome, u2[0].outcome,
        "a replayed unlink must not become ENOENT"
    );
    assert_eq!(ship::stats().dedup_hits - before.dedup_hits, 2);

    // A read verb needs no window — it is naturally idempotent, and the
    // window is not spent on it.
    let g = MetaOp {
        id: r.next_request_id(),
        call: MetaCall::Getattr { ino: 1 },
    };
    r.ship_ops(&peer, vec![g.clone()]).await.expect("read");
    r.ship_ops(&peer, vec![g]).await.expect("read again");
    assert_eq!(
        ship::stats().dedup_hits - before.dedup_hits,
        2,
        "reads do not consume the dedup window"
    );
    assert!(
        svc.stats().dedup_entries <= svc.dedup_cap(),
        "the window is bounded"
    );

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 7. Fencing: stale terms, and the successor's grace window
// ---------------------------------------------------------------------------

/// Contract (requirement 5): a request carrying a term the owner has
/// moved past is REFUSED — never executed — and the client relearns the
/// term from the refusal and retries. This is what makes an owner
/// failover safe against a client that saw a pre-fence answer: the
/// successor bumps `term` durably before arming, so every old-term
/// request is stale by construction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_term_request_is_refused_and_the_client_relearns() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");
    let before = ship::stats();

    // One verb so the client learns the owner's term.
    r.getattr(1).await.expect("learn the term");
    let learned = r
        .owner_term(&endpoint)
        .expect("term learned from the reply");
    assert_eq!(learned, svc.term());

    // The successor's era: bumped durably before arming (spec §6.7
    // "Recovery"). Every token and every request from the old era is now
    // stale by construction.
    svc.bump_term(learned + 5);
    assert!(svc.term() > learned);

    // A request built on the stale term must be refused, not executed.
    let stale = r
        .ship_ops_with_term(
            &r.owner_for_ino(1).expect("foreign"),
            vec![MetaOp {
                id: r.next_request_id(),
                call: MetaCall::CreateWithRdev {
                    parent: 1,
                    name: "stale-era".into(),
                    mode: FILE,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                },
            }],
            learned,
        )
        .await;
    assert!(
        stale.is_err(),
        "a stale-term frame must be refused whole, never partly applied"
    );
    assert_eq!(
        ship::stats().stale_term_refusals - before.stale_term_refusals,
        1,
        "the refusal is counted on the side that ISSUED it"
    );
    assert_eq!(
        ship::stats().era_relearns - before.era_relearns,
        1,
        "and the client's observation is its own counter, never a double count"
    );
    assert!(
        owner_be.lookup(1, "stale-era").await.is_err(),
        "the refused verb must not have executed"
    );

    // The client relearns and the ordinary path succeeds again.
    let f = r.create(1, "fresh-era", FILE, 0, 0).await.expect("create");
    assert_eq!(r.owner_term(&endpoint), Some(svc.term()));
    assert_eq!(
        owner_be.lookup(1, "fresh-era").await.expect("landed").ino,
        f.ino
    );

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

/// Contract (requirement 5, the grace half): after a successor arms, the
/// grace window **admits reclaim** and **refuses fresh mutations**, while
/// reads keep serving (a reader takes no grant). `dlm_grace_conflicts`
/// must stay 0 on a healthy failover — a well-behaved client reclaims and
/// waits — so growth is the visible statement that some client tried to
/// mutate through the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_grace_window_admits_reclaim_and_refuses_fresh_mutations() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));
    let r = router(client_be.clone(), "client-1");

    let held = r.create(1, "held", FILE, 0, 0).await.expect("pre-failover");
    let before = ship::stats();

    svc.bump_term(svc.term() + 1);
    svc.open_grace(Duration::from_secs(60));
    assert!(svc.in_grace(), "the window is open");

    // Reclaim: admitted, and it returns fresh-era grants for the objects
    // the client held.
    let grants = r.reclaim(&endpoint, &[held.ino]).await.expect("reclaim");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].ino, held.ino);
    assert_eq!(grants[0].term, svc.term(), "reclaim mints in the NEW era");
    assert_eq!(
        ship::stats().grace_reclaims - before.grace_reclaims,
        1,
        "the reclaim is counted"
    );

    // A read still serves.
    assert_eq!(
        r.getattr(held.ino).await.expect("read in grace").ino,
        held.ino
    );

    // A fresh mutation is refused, loudly and by name.
    let err = r
        .create(1, "fresh-in-grace", FILE, 0, 0)
        .await
        .expect_err("a fresh mutation must be refused during grace");
    let msg = format!("{err}");
    assert!(
        msg.contains("grace"),
        "the refusal must name the grace window: {msg}"
    );
    assert_eq!(
        ship::stats().grace_conflicts - before.grace_conflicts,
        1,
        "dlm_grace_conflicts counts the conflicting fresh acquire"
    );
    assert!(
        owner_be.lookup(1, "fresh-in-grace").await.is_err(),
        "the refused create must not have executed"
    );

    // The window closes when reclaim drains (or elapses) and mutations
    // resume.
    svc.close_grace();
    assert!(!svc.in_grace());
    r.create(1, "after-grace", FILE, 0, 0)
        .await
        .expect("mutations resume after the window");

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 8. Cross-owner shapes — the S3.5 refusal
// ---------------------------------------------------------------------------

/// Contract (requirement 4): a verb whose participants live on volumes
/// owned by DIFFERENT owners is refused **loud, naming S3.5** — the
/// cross-volume intent-record/compensation machinery (ruling D4) that is
/// not built. Inventing a second distributed-tx mechanism here is
/// forbidden, and shipping the op to one of the two owners would be
/// strictly worse than today: it would be non-atomic across owners with
/// no compensation record and no D0 guard covering both halves.
///
/// The same shape with ONE owner is not refused — it is today's
/// (non-atomic, DUR-7-tracked) cross-volume path, executed on the owner
/// exactly as it executes today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_owner_verbs_refuse_loud_naming_s3_5() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let client_be = sandbox(dir.path(), "client2v", 2).await;

    // Volume 0 → owner A, volume 1 → owner B. Nothing listens: the
    // refusal must happen BEFORE any round trip.
    let map = OwnerMap::for_volumes(
        &client_be,
        vec![
            (0, PeerOwner::new("owner-a", "127.0.0.1:1")),
            (1, PeerOwner::new("owner-b", "127.0.0.1:2")),
        ],
    )
    .expect("volume-aligned two-owner map");
    let _armed = arm(map);
    let r = router(client_be.clone(), "client-1");
    let before = ship::stats();

    // Two inos on different volumes (the derived width stripes
    // consecutive inos across slots, hence across volumes).
    let (v_a, v_b) = (0usize, 1usize);
    let ino_a = ino_on_volume(&client_be, v_a);
    let ino_b = ino_on_volume(&client_be, v_b);

    let err = r
        .route_verb(&MetaCall::Link {
            ino: ino_a,
            new_parent: ino_b,
            new_name: "x".into(),
        })
        .expect_err("a cross-owner link must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("S3.5"),
        "the refusal must name the machinery it needs: {msg}"
    );
    assert_eq!(
        squeezefs::error::SqueezefsError::to_errno(&err),
        libc::EXDEV,
        "a cross-owner shape presents EXDEV"
    );

    assert!(r
        .route_verb(&MetaCall::Rename {
            old_parent: ino_a,
            old_name: "a".into(),
            new_parent: ino_b,
            new_name: "b".into(),
            flags: 0,
        })
        .is_err());
    assert_eq!(
        ship::stats().cross_owner_refusals - before.cross_owner_refusals,
        2,
        "both refusals are counted"
    );

    // Same shape, one owner: routable, not refused.
    let same = OwnerMap::for_volumes(
        &client_be,
        vec![
            (0, PeerOwner::new("owner-a", "127.0.0.1:1")),
            (1, PeerOwner::new("owner-a", "127.0.0.1:1")),
        ],
    )
    .expect("one owner, two volumes");
    ship::arm_ownership(same);
    match r
        .route_verb(&MetaCall::Link {
            ino: ino_a,
            new_parent: ino_b,
            new_name: "x".into(),
        })
        .expect("one owner must be routable")
    {
        VerbRoute::Ship(peer) => assert_eq!(peer.peer_id, "owner-a"),
        VerbRoute::Local => panic!("both volumes are foreign here"),
    }

    shutdown(&client_be).await;
}

/// The lowest ino that routes to `v_idx` on this set (the routed map is
/// the single source of truth for the test's premise, not arithmetic).
fn ino_on_volume(routed: &Arc<RoutedMetaBackend>, v_idx: usize) -> u64 {
    (2..2 + 4096)
        .find(|&ino| routed.route_ino(ino).0 == v_idx)
        .unwrap_or_else(|| panic!("no ino routes to volume {v_idx}"))
}

// ---------------------------------------------------------------------------
// 9. Two nodes, concurrently, through the wire
// ---------------------------------------------------------------------------

/// Contract: two client "nodes" ship concurrently to one owner and every
/// op lands exactly once with a unique ino. This is the shape D1's
/// workload has (many clients, disjoint files) and it is the only place
/// the batching, the dedup window, the term gate and the owner's 4a
/// guards all interact under load.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_client_nodes_ship_concurrently_to_one_owner() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let owner_be = sandbox(dir.path(), "owner", 1).await;
    let client_be = sandbox(dir.path(), "client", 1).await;
    let (listener, svc, endpoint) = start_owner(owner_be.clone());
    let _armed = arm(all_foreign(&client_be, &endpoint));

    let a = router(client_be.clone(), "client-a");
    let b = router(client_be.clone(), "client-b");
    let da = a.create(1, "dir-a", DIR, 0, 0).await.expect("dir a");
    let db = b.create(1, "dir-b", DIR, 0, 0).await.expect("dir b");
    let before = ship::stats();

    const PER: u64 = 12;
    let mut set = tokio::task::JoinSet::new();
    for (r, parent, tag) in [(a.clone(), da.ino, "a"), (b.clone(), db.ino, "b")] {
        for k in 0..PER {
            let r = r.clone();
            let tag = tag.to_string();
            set.spawn(async move {
                r.create(parent, &format!("{tag}-{k}"), FILE, 0, 0)
                    .await
                    .map(|i| i.ino)
            });
        }
    }
    let mut inos = HashSet::new();
    while let Some(joined) = set.join_next().await {
        let ino = joined.expect("task").expect("concurrent shipped create");
        assert!(inos.insert(ino), "ino {ino} was minted twice");
    }
    assert_eq!(inos.len() as u64, PER * 2);

    for (parent, tag) in [(da.ino, "a"), (db.ino, "b")] {
        let entries = owner_be.readdir(parent, 0, 256).await.expect("readdir");
        assert_eq!(
            entries.len() as u64,
            PER,
            "the owner's directory must hold exactly the shipped children ({tag})"
        );
    }
    let after = ship::stats();
    assert_eq!(
        after.served_verbs - before.served_verbs,
        after.shipped_verbs - before.shipped_verbs,
        "engagement stays exact under concurrency"
    );
    assert_eq!(after.owner_panics, 0, "must-stay-0");
    assert_eq!(svc.stats().panics, 0);

    listener.shutdown();
    shutdown(&client_be).await;
    shutdown(&owner_be).await;
}

// ---------------------------------------------------------------------------
// 10. Ownership granularity and the format posture
// ---------------------------------------------------------------------------

/// Contract (requirement 7 of the design list, spec §6.10 **R4**):
/// ownership granularity is the **volume**. The API takes per-VOLUME
/// assignments, so an intra-volume split is unrepresentable rather than
/// merely refused — one volume still has one journal ring, one extent
/// bitmap, one root ledger (§6.2 items 2/3/4; bit 8's partitioned append
/// is built but NOT stamped) and one node cache (the third-gate-state
/// residual in `kv/revalidate.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_is_volume_granular_and_malformed_maps_refuse() {
    let _serial = OWNERSHIP.write().await;
    let dir = tempfile::tempdir().unwrap();
    let be = sandbox(dir.path(), "gran", 2).await;

    let map = OwnerMap::for_volumes(&be, vec![(1, PeerOwner::new("owner-b", "127.0.0.1:2"))])
        .expect("one foreign volume");
    assert_eq!(map.volume_count(), 2);
    assert!(map.owner_of_volume(0).is_none(), "volume 0 stays local");
    assert_eq!(map.owner_of_volume(1).expect("foreign").peer_id, "owner-b");
    // Every slot hosted by a foreign volume leaves the LOCK plane's
    // local set, so the lock master and the metadata authority stay the
    // same process (spec §6.7 decision 2).
    let _armed = arm(map);
    let foreign_ino = ino_on_volume(&be, 1);
    let slot = squeezefs::dlm_slot::slot_of_ino(foreign_ino, be.routing_width());
    assert!(
        !squeezefs::dlm_slot::is_local_slot(slot),
        "a foreign volume's slots must leave the lock plane's local set"
    );
    assert_ne!(squeezefs::dlm_slot::dlm_mode(), "solo");

    // A malformed map refuses loud, naming the volume count.
    let err = OwnerMap::for_volumes(&be, vec![(7, PeerOwner::new("ghost", "127.0.0.1:3"))])
        .expect_err("a volume index past the set must refuse");
    assert!(format!("{err}").contains('7'));

    shutdown(&be).await;
}

/// Contract (D9 / the format-honesty pin): **S8 introduces no incompat
/// bit.** Function shipping changes no on-disk structure — ownership is
/// discoverable from the per-volume D0 `writer_claim` record, which
/// already names its holder and (since S2) its durable term — so there is
/// nothing to stamp. Bit 11 stays free for the stage that genuinely needs
/// one, and this test is what keeps that claim honest.
#[test]
fn s8_introduces_no_incompat_bit_and_bit_11_stays_free() {
    assert_eq!(
        FEATURES_INCOMPAT_KNOWN & (1u64 << 11),
        0,
        "S8 stamps nothing; bit 11 must still be free"
    );
    assert_eq!(
        FEATURES_INCOMPAT_KNOWN.count_ones(),
        11,
        "bits 0..=10 are the known set; S8 added none"
    );
}
