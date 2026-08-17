//! DLM S10 rung 13 — **per-directory EXCLUSIVE UPDATE grants + asynchronous
//! create-intent batches** (KD-MW-13; design-full-multi-writer §8.2 lever 1,
//! the UPDATE half; PR-plan row 13).
//!
//! # The laws under test (each arm red-first, per the charter)
//!
//! * **UPDATE is EXCLUSIVE per directory — recall-on-conflict** (§8.2 law
//!   1): one holder per directory, and a second wanter's metadata RPC
//!   recalls the first (its intent batch FLUSHES before the ack — OQ-2's
//!   resolved form), then the grant moves. Exclusivity + the grant-carried
//!   dentry census (the "revalidate D's entries to the grant's version"
//!   requirement, discharged as a bounded name census snapshotted at grant
//!   — a volume-wide watermark never settles under a create storm, the
//!   rung-12 tar-x lesson) is what makes `O_EXCL` decidable locally: a
//!   local negative IS authoritative, and two clients racing one name get
//!   exactly ONE ack, ever.
//! * **The deferred-error law** (§8.2 law 2, the POSIX-16 errseq
//!   precedent): a locally-minted name is PROVISIONAL until its batch
//!   applies; an apply refusal (the injected ENOSPC seam) latches onto the
//!   DIRECTORY, surfaces at `fsync(dir)`/close, DESTROYS the local mint,
//!   and counts `meta_ship_intent_refusals` — never a silent success,
//!   never a stale local name surviving the refusal.
//! * **Foreign visibility** (§8.2 law 3): after the minter's `fsync(dir)`
//!   a foreign negative lookup goes positive within the PUBLISHED bound
//!   (`meta_ship_intent_visibility_bound_ms` — post-fsync the batch-flush
//!   term is 0, so the bound is the foreign kernel's negative-entry TTL,
//!   ≤ this mount's own published negative TTL by the S5 TTL law; reader
//!   mounts add their own published `reader_staleness_bound_ms`).
//! * **recall-forces-flush** (OQ-2, orchestrator-adopted 2026-08-15): a
//!   foreign client's lookup/readdir under D recalls the UPDATE grant,
//!   which flushes the batch BEFORE the foreign serve — coherence over
//!   latency on the foreign path. The storm row prices it; the permanent
//!   red half proves the gate is load-bearing.
//! * **MW-8** (the crash window): an unshipped batch dies with the client
//!   (acked-un-fsynced class, disclosed); `fsync(dir)` is the contract
//!   point — a flushed batch is durable.
//! * **Era gate + witness FROM BIRTH** (the rung-9 finding-#6 precedent):
//!   the intent-batch verb carries the custody `lease_epoch` (intents die
//!   with the custody fence) and the `(lease_epoch, request_id)` witness
//!   (a replay answers the winner, never double-applies); a stale
//!   owner-term frame refuses WHOLE; a fenced holder's batch refuses.
//!
//! Red-first: this suite lands BEFORE the implementation and fails to
//! compile at its commit (the rung-11/12 discipline). The OQ-2 red half is
//! permanent: `red_half_without_the_read_gate_a_foreign_lookup_misses_
//! acked_names` runs the exact foreign-read shape against the gate-off
//! seam and proves the stale NEGATIVE happens — the gate IS the mechanism.

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::{
    self as ship, intents, IntentBatchFrame, IntentCall, IntentOp, MetaCall, MetaOp, MetaReply,
    MetaRequestFrame, MetaShipRouter, MetaShipService, OwnerMap, PeerOwner, DELEG_CLASS_LOOKUP,
    DELEG_CLASS_UPDATE,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const SECRET: &[u8] = b"s10-intent-batch-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const FILE: u32 = libc::S_IFREG | 0o644;
const DIR: u32 = libc::S_IFDIR | 0o755;
/// The router/custody client identity (KD-MW-2 pair form).
const NODE: &str = "node_cafe.m0001";
/// The raw-wire SECOND client (arm 1's racer, arm 3's foreign reader).
const NODE_B: &str = "node_beef.m0002";

/// Process-global planes — every test takes this exclusively (the
/// mw_delegation_tests discipline).
static PLANE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the solo posture on drop, so a panicking test can never leave
/// the binary's other tests armed, delegated, or holding intents.
struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ship::uninstall_daemon_verb_router();
        ship::uninstall_delegation_host();
        ship::disarm_ownership();
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        ship::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
        ship::TEST_DELEG_COHERENCE_LAW.store(true, Ordering::SeqCst);
        intents::TEST_INTENTS_OVERRIDE.store(0, Ordering::SeqCst);
        ship::TEST_INTENT_READ_GATE.store(true, Ordering::SeqCst);
        ship::TEST_INTENT_APPLY_ERRNO.store(0, Ordering::SeqCst);
        ship::TEST_INTENT_SUPPLY_CHUNK.store(0, Ordering::SeqCst);
        ship::test_clear_delegations();
        ship::test_clear_token_cache();
        intents::test_clear_intents();
        ship::global_recall_lane().test_clear_state();
        ship::set_deleg_inval_sink(None);
        std::env::remove_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS");
    }
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

/// The intent venue: the rung-12 delegation fixture PLUS a real custody
/// plane (owner + joined client), because the intent-batch verb is
/// era-gated on the custody `lease_epoch` from birth (the charter's
/// finding-#6 law) — an unverifiable epoch must refuse, so the fixture
/// arms the authority the validator checks against.
struct Fixture {
    _dir: tempfile::TempDir,
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    svc: Arc<MetaShipService>,
    custody_owner: Arc<WriteCustodyOwner>,
    custody_client: Arc<WriteCustodyClient>,
    endpoint: String,
    _router: Arc<MetaShipRouter>,
    _arm: ArmGuard,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir.path(), "meta0", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format stamped meta volume");
    let paths = vec![p.display().to_string()];

    let owner_be = open_routed_meta_set(&paths).await.expect("writer set");
    for vol in &owner_be.volumes {
        vol.checkpoint_now().await.expect("settle checkpoint");
    }
    let client_be = open_routed_meta_set_read_only(&paths)
        .await
        .expect("reader set over the same volumes");
    for vol in &client_be.volumes {
        vol.arm_reader_revalidation(None)
            .expect("the reader declaration");
    }

    // The CUSTODY plane: the era the intent-batch verb is gated on.
    let clocks = LeaseClocks::with_params(
        Duration::from_secs(45),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let custody_owner = WriteCustodyOwner::arm(
        "owner-a",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&custody_owner));

    let svc = MetaShipService::new(Arc::clone(&owner_be));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    // The PRODUCTION verb router (rung-12 finding #2's law: an unrouted
    // verb turns the suite red), carrying the meta block AND the custody
    // block (the lease join below needs it).
    let verb_router = Arc::new(
        squeezefs::data_grant::AsyncVerbRouter::new()
            .with_meta(Arc::clone(&svc))
            .with_custody(Arc::clone(&custody_owner)),
    );
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), verb_router)
        .expect("owner-side listener starts");
    let endpoint = listener.endpoint().to_string();

    ship::install_delegation_host(Arc::clone(&svc));

    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", &endpoint)))
        .collect();
    let map = OwnerMap::for_volumes(&client_be, foreign).expect("owner map");
    ship::global_recall_lane().test_clear_state();
    ship::test_clear_delegations();
    intents::test_clear_intents();
    ship::arm_ownership(map);
    let _arm = ArmGuard;
    ship::TEST_DELEGATION_OVERRIDE.store(1, Ordering::SeqCst);
    intents::TEST_INTENTS_OVERRIDE.store(1, Ordering::SeqCst);

    // The custody JOIN: the co-writer's live lease epoch — what every
    // intent batch presents and the owner's era gate verifies.
    let custody_client = WriteCustodyClient::connect(&endpoint, SECRET, NODE)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&custody_client));

    let router = MetaShipRouter::new(Arc::clone(&client_be), NODE, SECRET.to_vec());
    ship::install_daemon_verb_router(Arc::clone(&router));
    Fixture {
        _dir: dir,
        owner_be,
        client_be,
        listener,
        svc,
        custody_owner,
        custody_client,
        endpoint,
        _router: router,
        _arm,
    }
}

async fn shutdown(fx: &Fixture) {
    fx.listener.shutdown();
    for vol in &fx.owner_be.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

/// Bounded wait over an observable condition — never a bare sleep standing
/// in for the assertion.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..600 {
        if cond() {
            return;
        }
        squeezefs_ipc::sqz_time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn istats() -> intents::IntentStats {
    intents::intent_stats()
}

/// Owner-side mkdir + the grant-earning SHIPPED create from the client,
/// returning `(dir_ino, first_child_ino)`. After this the client holds the
/// EXCLUSIVE UPDATE grant on the directory (census + ino supply) and mints
/// locally.
async fn earn_update(fx: &Fixture, dirname: &str) -> (u64, u64) {
    let d = Metadata::create(fx.owner_be.as_ref(), 1, dirname, DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    // The FIRST create ships (no grant yet) and EARNS the UPDATE grant on
    // its reply (the intent-lock law: acquisition rides the RPC the
    // client was already issuing).
    let grants0 = istats().update_grants;
    let f0 = Metadata::create(fx.client_be.as_ref(), d, "f0", FILE, 0, 0)
        .await
        .expect("the grant-earning shipped create")
        .ino;
    wait_for("the UPDATE grant to install", || {
        istats().update_grants > grants0 && intents::holds_update_authority(d)
    })
    .await;
    (d, f0)
}

/// Ship one raw S8 batch as a SECOND client (the two-client arms).
async fn ship_raw(
    endpoint: &str,
    client_id: &str,
    epoch: u64,
    ops: Vec<MetaOp>,
) -> Vec<ship::MetaOpResult> {
    let mut raw = cw::RpcClient::connect(endpoint, SECRET, client_id, None)
        .await
        .expect("raw client connects");
    let frame = MetaRequestFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: epoch,
        client_id: client_id.to_string(),
        owner_term: 0,
        ops,
    };
    let reply = raw
        .call(ship::VERB_META_BATCH, ship::encode_request(&frame).expect("encode"))
        .await
        .expect("raw batch round-trips");
    assert_eq!(reply.status, cw::RPC_OK, "raw batch admitted");
    ship::decode_reply(&reply.body).expect("decode").results
}

/// Ship one raw INTENT batch (the era/witness arms drive the wire form
/// directly).
async fn ship_raw_intents(
    endpoint: &str,
    client_id: &str,
    frame: &IntentBatchFrame,
) -> cw::RpcResponse {
    let mut raw = cw::RpcClient::connect(endpoint, SECRET, client_id, None)
        .await
        .expect("raw intent client connects");
    raw.call(
        ship::VERB_DELEG_INTENT,
        ship::encode_intent_batch(frame).expect("encode"),
    )
    .await
    .expect("intent batch round-trips")
}

// ===========================================================================
// 1. The wire vocabulary (schema 3): UPDATE grants, intent batches, supply
// ===========================================================================

/// The intent vocabulary is the schema-3 bump of the S8 wire (KD-MW-11: no
/// incompat bit — wire schema versions carry compatibility): the new verb
/// takes the next slot of the delegation block, the UPDATE capability bit
/// is defined (bitmask with LOOKUP), frames round-trip, and untrusted
/// bytes refuse loud under the bounded decode.
#[test]
fn the_wire_carries_the_intent_vocabulary_and_refuses_untrusted_bytes() {
    assert_eq!(
        ship::META_SHIP_SCHEMA,
        3,
        "UPDATE intents are the schema-3 bump"
    );
    assert_eq!(ship::VERB_DELEG_INTENT, 0x0402);
    assert_eq!(DELEG_CLASS_LOOKUP, 1);
    assert_eq!(
        DELEG_CLASS_UPDATE,
        2,
        "the UPDATE capability bit (bitmask with LOOKUP)"
    );

    let frame = IntentBatchFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 7,
        client_id: NODE.to_string(),
        owner_term: 3,
        lease_epoch: 11,
        supply_request: 64,
        ops: vec![
            IntentOp {
                request_id: 1,
                call: IntentCall::CreateAt {
                    parent: 2,
                    name: "f1".to_string(),
                    ino: 4096,
                    mode: FILE,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    initial_size: 0,
                    ts_ns: 1_700_000_000_000_000_000,
                },
            },
            IntentOp {
                request_id: 2,
                call: IntentCall::SetattrAt {
                    ino: 4096,
                    mode: None,
                    uid: None,
                    gid: None,
                    atime: Some(1),
                    mtime: Some(2),
                    ctime: Some(3),
                },
            },
        ],
    };
    let bytes = ship::encode_intent_batch(&frame).expect("encode");
    assert_eq!(ship::decode_intent_batch(&bytes).expect("decode"), frame);

    let reply = ship::IntentBatchReply {
        schema: ship::META_SHIP_SCHEMA,
        owner_term: 3,
        results: vec![ship::IntentResult {
            request_id: 1,
            outcome: Ok(()),
            revokes: vec![4096],
            revoke_fence: 9,
        }],
        supply: Some(ship::InoSupply {
            first_global: 1 << 20,
            stride: 1 << 16,
            count: 64,
        }),
    };
    let bytes = ship::encode_intent_batch_reply(&reply).expect("encode");
    assert_eq!(
        ship::decode_intent_batch_reply(&bytes).expect("decode"),
        reply
    );

    assert!(ship::decode_intent_batch(&[0xff; 64]).is_err());
    assert!(ship::decode_intent_batch_reply(&[0xff; 64]).is_err());
}

// ===========================================================================
// 2. The engine: grant-earning ship, then local mints (zero round trips)
// ===========================================================================

/// The tar-x collapse's mechanism: the FIRST create into a cold directory
/// ships and earns the EXCLUSIVE UPDATE grant (census + ino supply) on its
/// own reply; every subsequent create MINTS locally — zero shipped verbs —
/// visible to this client immediately (lookup serves the pending image,
/// getattr serves it by ino), with a local authoritative NEGATIVE for
/// absent names (the grant census is exact under exclusivity), and the
/// batch flushes as ONE frame at `fsync(dir)` (the coalesce law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_update_grant_rides_the_first_shipped_create_and_mints_answer_locally() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, f0) = earn_update(&fx, "tarx").await;
    assert!(f0 > 1);

    let ship0 = ship::stats().shipped_verbs;
    let mints0 = istats().mints;

    // Local mints: no wire.
    let mut minted = Vec::new();
    for n in 0..8 {
        let ino = Metadata::create(fx.client_be.as_ref(), d, &format!("m{n}"), FILE, 0, 0)
            .await
            .expect("local mint")
            .ino;
        minted.push(ino);
    }
    assert_eq!(
        istats().mints,
        mints0 + 8,
        "eight creates minted locally under the UPDATE grant"
    );
    assert_eq!(
        ship::stats().shipped_verbs,
        ship0,
        "local mints ship NOTHING (the §8.2 zero-round-trip law)"
    );
    // Distinct inos, all from the grant-carried supply.
    let mut dedup = minted.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), minted.len(), "supply inos never collide");

    // Read-your-own-mints: lookup + getattr serve the pending image with
    // zero wire.
    let ship1 = ship::stats().shipped_verbs;
    let looked = Metadata::lookup(fx.client_be.as_ref(), d, "m3")
        .await
        .expect("pending lookup serves locally");
    assert_eq!(looked.ino, minted[3]);
    let got = Metadata::getattr(fx.client_be.as_ref(), minted[3])
        .await
        .expect("pending getattr serves locally");
    assert_eq!(got.ino, minted[3]);
    // The census-backed authoritative NEGATIVE (exclusivity makes it
    // exact): an absent name answers ENOENT with zero wire.
    let neg0 = istats().local_negatives;
    let err = Metadata::lookup(fx.client_be.as_ref(), d, "never-created")
        .await
        .expect_err("census negative");
    assert_eq!(
        squeezefs::error::SqueezefsError::to_errno(&err),
        libc::ENOENT
    );
    assert!(
        istats().local_negatives > neg0,
        "the census answered the negative locally"
    );
    assert_eq!(
        ship::stats().shipped_verbs,
        ship1,
        "pending lookup/getattr + census negative are all LOCAL"
    );

    // Invisible on the owner until the flush.
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "m0").await.is_err(),
        "an un-flushed mint must not be visible owner-side"
    );

    // fsync(dir): ONE frame carries the batch (the coalesce law).
    let batches0 = istats().batches;
    let verbs0 = istats().verbs;
    intents::fsync_dir_barrier(d)
        .await
        .expect("fsync(dir) flushes clean");
    assert_eq!(istats().batches, batches0 + 1, "one flush frame");
    assert_eq!(istats().verbs, verbs0 + 8, "carrying all eight intents");

    // Applied owner-side, exactly the minted inos.
    for (n, ino) in minted.iter().enumerate() {
        let inode = Metadata::lookup(fx.owner_be.as_ref(), d, &format!("m{n}"))
            .await
            .expect("applied name resolves owner-side");
        assert_eq!(inode.ino, *ino, "the applied ino IS the minted ino");
    }
    // Post-flush, the applied name still resolves for the minter (the
    // census remembers applied names; the serve ships — owner-current).
    let looked = Metadata::lookup(fx.client_be.as_ref(), d, "m0")
        .await
        .expect("applied name resolves for the minter");
    assert_eq!(looked.ino, minted[0]);

    assert_eq!(istats().refusals, 0, "no deferred refusals on this path");
    shutdown(&fx).await;
}

/// Supply exhaustion is a DECLINE (the priced fallback: the create ships
/// as today and the flush's `supply_request` refills the pool) — never a
/// wrong answer, never a wedge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supply_exhaustion_declines_to_ship_and_refills_on_flush() {
    let _plane = PLANE.lock().await;
    ship::TEST_INTENT_SUPPLY_CHUNK.store(2, Ordering::SeqCst);
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "supply").await;

    // Two mints drain the pinned 2-ino chunk.
    for n in 0..2 {
        Metadata::create(fx.client_be.as_ref(), d, &format!("s{n}"), FILE, 0, 0)
            .await
            .expect("supplied mint");
    }
    assert_eq!(istats().supply_remaining, 0, "the chunk is drained");
    // The third create DECLINES to mint (supply dry) and ships.
    let declines0 = istats().declines;
    let ship0 = ship::stats().shipped_verbs;
    Metadata::create(fx.client_be.as_ref(), d, "s2", FILE, 0, 0)
        .await
        .expect("the declined create ships and succeeds");
    assert!(istats().declines > declines0, "the dry pool declines");
    assert!(
        ship::stats().shipped_verbs > ship0,
        "the declined create rode the wire"
    );
    // The shipped create's own barrier flushed the pending two first
    // (order: a shipped mutation naming a dir with pending intents flushes
    // before it ships), and the flush's supply_request refilled the pool.
    wait_for("the flush-carried refill", || istats().supply_remaining > 0).await;
    Metadata::create(fx.client_be.as_ref(), d, "s3", FILE, 0, 0)
        .await
        .expect("post-refill mint");
    shutdown(&fx).await;
}

// ===========================================================================
// 3. Arm 1 — two clients racing O_EXCL under recall-on-conflict
// ===========================================================================

/// **Order A — the holder minted first.** Client A (UPDATE holder) acks
/// `race` locally; client B's SHIPPED create of the same name triggers the
/// mutation gate, which recalls A's grant; the recall FORCES A's flush
/// (OQ-2's resolved form) BEFORE the ack, so A's create applies first and
/// B's answers EEXIST. Exactly ONE ack, and the surviving name is A's
/// minted ino.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_racing_o_excl_holder_minted_first_never_double_acks() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "race-a").await;

    // A mints (the local O_EXCL ack).
    let a_ino = Metadata::create(fx.client_be.as_ref(), d, "race", FILE, 0, 0)
        .await
        .expect("A's local mint acks")
        .ino;
    let flushes0 = istats().flush_forces;

    // B ships the SAME name: the gate recalls A (flush-before-ack), then
    // B's create executes — and must lose to A's already-acked mint.
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        41,
        vec![MetaOp {
            id: 1,
            call: MetaCall::CreateWithRdev {
                parent: d,
                name: "race".to_string(),
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        }],
    )
    .await;
    let b_err = results[0]
        .outcome
        .as_ref()
        .expect_err("B's create of the already-acked name must refuse");
    assert_eq!(b_err.errno, libc::EEXIST, "exactly one ack — B gets EEXIST");
    assert!(
        istats().flush_forces > flushes0,
        "the recall FORCED A's flush before B's create applied (OQ-2)"
    );

    // The surviving name is A's minted ino, owner-durable.
    let inode = Metadata::lookup(fx.owner_be.as_ref(), d, "race")
        .await
        .expect("the name resolves owner-side");
    assert_eq!(inode.ino, a_ino, "A's ack is the one that survived");
    shutdown(&fx).await;
}

/// **Order B — the recall lands first.** B's shipped create recalls A's
/// grant BEFORE A mints; A's create then declines the local mint (grant
/// revoked) and ships — the owner serializes, B's ack stands, A gets
/// EEXIST. Exactly ONE ack in this order too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_racing_o_excl_recall_first_never_double_acks() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "race-b").await;

    // B ships the name FIRST: the gate recalls A's (idle) grant, B's
    // create applies and acks.
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        42,
        vec![MetaOp {
            id: 1,
            call: MetaCall::CreateWithRdev {
                parent: d,
                name: "race".to_string(),
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        }],
    )
    .await;
    let b_ino = match results[0].outcome.as_ref().expect("B's create acks") {
        MetaReply::Inode(i) => i.ino,
        other => panic!("create answered {other:?}"),
    };

    // A's grant is gone; its create must NOT local-mint — it ships and
    // answers EEXIST (the owner's serialized truth).
    wait_for("A's recalled authority to drop", || {
        !intents::holds_update_authority(d)
    })
    .await;
    let err = Metadata::create(fx.client_be.as_ref(), d, "race", FILE, 0, 0)
        .await
        .expect_err("A's create of B's name must refuse");
    assert_eq!(
        squeezefs::error::SqueezefsError::to_errno(&err),
        libc::EEXIST,
        "exactly one ack — A gets EEXIST"
    );
    let inode = Metadata::lookup(fx.owner_be.as_ref(), d, "race")
        .await
        .expect("the name resolves owner-side");
    assert_eq!(inode.ino, b_ino, "B's ack is the one that survived");
    shutdown(&fx).await;
}

// ===========================================================================
// 4. Arm 2 — the deferred-error law (§8.2 law 2, POSIX-16 precedent)
// ===========================================================================

/// An injected apply-ENOSPC surfaces at `fsync(dir)` and DESTROYS the
/// local mint: the errno latches onto the directory (errseq — reported
/// once, then clean), `meta_ship_intent_refusals` counts it, the pending
/// image/name are GONE (a lookup no longer serves them), and nothing was
/// applied owner-side. Never a silent success.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_injected_apply_enospc_surfaces_at_fsync_dir_and_destroys_the_local_mint() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "enospc").await;

    let ino = Metadata::create(fx.client_be.as_ref(), d, "doomed", FILE, 0, 0)
        .await
        .expect("the provisional ack")
        .ino;

    ship::TEST_INTENT_APPLY_ERRNO.store(libc::ENOSPC, Ordering::SeqCst);
    let refusals0 = istats().refusals;
    let destroys0 = istats().mint_destroys;
    let errno = intents::fsync_dir_barrier(d)
        .await
        .expect_err("fsync(dir) surfaces the deferred refusal");
    assert_eq!(errno, libc::ENOSPC, "the owner's errno, verbatim");
    assert_eq!(istats().refusals, refusals0 + 1, "meta_ship_intent_refusals counts it");
    assert_eq!(istats().mint_destroys, destroys0 + 1, "the local mint is DESTROYED");
    ship::TEST_INTENT_APPLY_ERRNO.store(0, Ordering::SeqCst);

    // The destroyed mint no longer serves: the lookup misses the image
    // (and the owner never applied the name).
    assert!(
        Metadata::lookup(fx.client_be.as_ref(), d, "doomed").await.is_err(),
        "a stale local name must not survive the refusal"
    );
    assert!(
        Metadata::getattr(fx.client_be.as_ref(), ino).await.is_err(),
        "the destroyed image must not serve by ino"
    );
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "doomed").await.is_err(),
        "nothing was applied owner-side"
    );

    // The latch is errseq: reported ONCE — the next fsync(dir) is clean.
    intents::fsync_dir_barrier(d)
        .await
        .expect("the second fsync(dir) is clean (the latch was consumed)");
    shutdown(&fx).await;
}

// ===========================================================================
// 5. Arm 3 — foreign visibility after fsync(dir), and the published bound
// ===========================================================================

/// A foreign client's negative lookup goes positive immediately after the
/// minter's `fsync(dir)` (the flush applied on the owner, whose serve is
/// current), and the composition bound is PUBLISHED: post-fsync the
/// batch-flush term is 0, so `meta_ship_intent_visibility_bound_ms` is the
/// foreign kernel's negative-entry TTL term (this mount publishes its own
/// negative TTL; foreign kernels' TTLs are ≤ the reader bound by the S5
/// TTL law — the reader-staleness-bound pattern: publish the arithmetic so
/// the docs cannot drift from the number in force).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_negative_lookup_goes_positive_after_fsync_dir_within_the_published_bound() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "vis").await;

    // The foreign negative BEFORE the mint (B's shipped lookup).
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        43,
        vec![MetaOp {
            id: 1,
            call: MetaCall::LookupDentry {
                parent: d,
                name: "appears".to_string(),
            },
        }],
    )
    .await;
    assert_eq!(
        results[0].outcome.as_ref().expect_err("negative").errno,
        libc::ENOENT
    );

    let ino = Metadata::create(fx.client_be.as_ref(), d, "appears", FILE, 0, 0)
        .await
        .expect("the local mint")
        .ino;
    intents::fsync_dir_barrier(d)
        .await
        .expect("fsync(dir) — the contract point");

    // Post-fsync: the foreign lookup is positive IMMEDIATELY (the owner's
    // serve is current; the remaining foreign term is kernel TTL aging,
    // which the Metadata layer does not carry).
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        43,
        vec![MetaOp {
            id: 2,
            call: MetaCall::LookupDentry {
                parent: d,
                name: "appears".to_string(),
            },
        }],
    )
    .await;
    match results[0].outcome.as_ref().expect("positive after fsync") {
        MetaReply::Inode(i) => assert_eq!(i.ino, ino),
        MetaReply::Ino(i) => assert_eq!(*i, ino),
        other => panic!("lookup answered {other:?}"),
    }

    // The published bound: the mount's own negative TTL (default 1000 ms —
    // SQUEEZEFS_FUSE_NEGATIVE_TTL_MS), the arithmetic's foreign-kernel
    // term. Post-fsync the batch-flush term is 0 by construction.
    assert_eq!(
        istats().visibility_bound_ms,
        1000,
        "the published bound is the negative-TTL term at its default"
    );
    shutdown(&fx).await;
}

// ===========================================================================
// 6. OQ-2 — recall-forces-flush on the foreign read path (+ permanent red)
// ===========================================================================

/// **The permanent red half** (the gate IS the mechanism): with the OQ-2
/// read gate disabled through the test seam, a foreign lookup of a name
/// the minter has ACKED answers a stale NEGATIVE — the §8.2 law-3
/// violation the gate exists to prevent. This test proves the violation
/// HAPPENS without the gate, so the green half can never pass vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn red_half_without_the_read_gate_a_foreign_lookup_misses_acked_names() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "oq2-red").await;

    ship::TEST_INTENT_READ_GATE.store(false, Ordering::SeqCst);
    Metadata::create(fx.client_be.as_ref(), d, "hidden", FILE, 0, 0)
        .await
        .expect("the local ack");
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        44,
        vec![MetaOp {
            id: 1,
            call: MetaCall::LookupDentry {
                parent: d,
                name: "hidden".to_string(),
            },
        }],
    )
    .await;
    assert_eq!(
        results[0]
            .outcome
            .as_ref()
            .expect_err("the STALE NEGATIVE the law prevents")
            .errno,
        libc::ENOENT,
        "without the gate the foreign reader misses an acked name — the red half"
    );
    ship::TEST_INTENT_READ_GATE.store(true, Ordering::SeqCst);
    shutdown(&fx).await;
}

/// **The green half**: a foreign lookup under D recalls the UPDATE grant,
/// which flushes the intent batch BEFORE the foreign serve (OQ-2's
/// resolved form: coherence over latency on the foreign path) — the
/// foreign reader sees the acked name, the flush was FORCED (counted), and
/// the recall was acked (never timed out).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_read_recalls_the_update_grant_and_forces_the_flush() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "oq2").await;

    let ino = Metadata::create(fx.client_be.as_ref(), d, "forced", FILE, 0, 0)
        .await
        .expect("the local ack")
        .ino;
    let flushes0 = istats().flush_forces;
    let reads0 = istats().read_recalls;
    let results = ship_raw(
        &fx.endpoint,
        NODE_B,
        45,
        vec![MetaOp {
            id: 1,
            call: MetaCall::LookupDentry {
                parent: d,
                name: "forced".to_string(),
            },
        }],
    )
    .await;
    match results[0].outcome.as_ref().expect("the flushed name serves") {
        MetaReply::Inode(i) => assert_eq!(i.ino, ino),
        MetaReply::Ino(i) => assert_eq!(*i, ino),
        other => panic!("lookup answered {other:?}"),
    }
    assert!(istats().read_recalls > reads0, "the foreign read RECALLED");
    assert!(
        istats().flush_forces > flushes0,
        "the recall FORCED the flush before the serve"
    );
    assert_eq!(ship::delegation_stats().recall_timeouts, 0);
    shutdown(&fx).await;
}

// ===========================================================================
// 7. The era gate + witness (from birth — the rung-9 finding-#6 law)
// ===========================================================================

/// A replayed intent batch — same `(lease_epoch, request_id)` — answers
/// the winner's own outcome from the dedup window and never double-applies
/// (one dentry, one inode record, `replays` counted).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_intent_batch_answers_the_winner_and_never_double_applies() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "replay").await;

    // Reserve one supply ino through the real machinery: mint, then
    // capture the pending op's identity by flushing RAW twice.
    let ino = Metadata::create(fx.client_be.as_ref(), d, "once", FILE, 0, 0)
        .await
        .expect("mint")
        .ino;
    intents::fsync_dir_barrier(d).await.expect("first flush");

    // Rebuild the SAME logical frame by hand and send it twice more: the
    // witness must absorb both (the winner's outcome, no re-apply).
    let epoch = fx.custody_client.lease_epoch();
    let frame = IntentBatchFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 77,
        client_id: NODE.to_string(),
        owner_term: fx.svc.term(),
        lease_epoch: epoch,
        supply_request: 0,
        ops: vec![IntentOp {
            request_id: 424_242,
            call: IntentCall::CreateAt {
                parent: d,
                name: "twice".to_string(),
                ino: ino + (1 << 20), // an unused supply-shaped ino
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
                initial_size: 0,
                ts_ns: 1,
            },
        }],
    };
    let replays0 = istats().replays;
    let r1 = ship_raw_intents(&fx.endpoint, NODE, &frame).await;
    assert_eq!(r1.status, cw::RPC_OK);
    let d1 = ship::decode_intent_batch_reply(&r1.body).expect("decode");
    assert!(d1.results[0].outcome.is_ok(), "the winner applies");
    let r2 = ship_raw_intents(&fx.endpoint, NODE, &frame).await;
    assert_eq!(r2.status, cw::RPC_OK);
    let d2 = ship::decode_intent_batch_reply(&r2.body).expect("decode");
    assert!(
        d2.results[0].outcome.is_ok(),
        "the replay answers the winner's own outcome"
    );
    assert!(istats().replays > replays0, "the witness counted the replay");

    // Never double-applied: exactly one dentry, resolving to the op's ino.
    let entries = Metadata::readdir(fx.owner_be.as_ref(), d, 0, 1024)
        .await
        .expect("owner readdir");
    assert_eq!(
        entries.iter().filter(|e| e.name == "twice").count(),
        1,
        "one dentry, not two"
    );
    shutdown(&fx).await;
}

/// A stale-owner-term intent frame refuses WHOLE (nothing applied) — the
/// S8 era discipline on the intent verb from birth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_term_intent_frame_refuses_whole() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "stale-term").await;

    let epoch = fx.custody_client.lease_epoch();
    let stale = fx.svc.term();
    fx.svc.bump_term(stale + 1);
    let frame = IntentBatchFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 78,
        client_id: NODE.to_string(),
        owner_term: stale,
        lease_epoch: epoch,
        supply_request: 0,
        ops: vec![IntentOp {
            request_id: 1,
            call: IntentCall::CreateAt {
                parent: d,
                name: "stale".to_string(),
                ino: 1 << 21,
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
                initial_size: 0,
                ts_ns: 1,
            },
        }],
    };
    let r = ship_raw_intents(&fx.endpoint, NODE, &frame).await;
    assert_eq!(
        r.status,
        ship::STATUS_STALE_TERM,
        "an old-era intent frame is stale by construction — refused whole"
    );
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "stale").await.is_err(),
        "nothing was applied"
    );
    shutdown(&fx).await;
}

/// Intents die with the CUSTODY fence: a lease epoch the authority cannot
/// verify as live custody refuses the batch whole (`STATUS_INTENT_LEASE`),
/// nothing applies, and the counted face is `stale_refusals` — the
/// publish-path `PUBLISH_STALE_LEASE` law on this verb from birth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intents_die_with_the_custody_fence() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "fence").await;

    let stale0 = istats().stale_refusals;
    let frame = IntentBatchFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 79,
        client_id: NODE.to_string(),
        owner_term: fx.svc.term(),
        lease_epoch: 0, // never a live epoch
        supply_request: 0,
        ops: vec![IntentOp {
            request_id: 1,
            call: IntentCall::CreateAt {
                parent: d,
                name: "zombie".to_string(),
                ino: 1 << 22,
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
                initial_size: 0,
                ts_ns: 1,
            },
        }],
    };
    let r = ship_raw_intents(&fx.endpoint, NODE, &frame).await;
    assert_eq!(
        r.status,
        ship::STATUS_INTENT_LEASE,
        "a fenced-era batch refuses BEFORE the witness window"
    );
    assert!(istats().stale_refusals > stale0);
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "zombie").await.is_err(),
        "a fenced holder's intent never applies"
    );
    shutdown(&fx).await;
}

/// A holder FENCED on the delegation plane (recall deadline expired) has
/// its intent batches refused with `STATUS_DELEG_FENCED` — the batch dies
/// with the grant, exactly as its poll and re-assert already do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fenced_holders_intent_batch_refuses() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    std::env::set_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS", "200");

    let d = Metadata::create(fx.owner_be.as_ref(), 1, "fencedir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    // A manual holder earns a LOOKUP grant and never services its recall
    // channel; the conflicting create fences it at the deadline.
    let results = ship_raw(
        &fx.endpoint,
        "manual-holder",
        99,
        vec![MetaOp {
            id: 1,
            call: MetaCall::Getattr { ino: d },
        }],
    )
    .await;
    assert!(!results[0].delegs.is_empty(), "the grant rode the reply");
    Metadata::create(fx.owner_be.as_ref(), d, "conflict", FILE, 0, 0)
        .await
        .expect("the gated create proceeds at the deadline");

    let frame = IntentBatchFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 99,
        client_id: "manual-holder".to_string(),
        owner_term: fx.svc.term(),
        lease_epoch: fx.custody_client.lease_epoch(),
        supply_request: 0,
        ops: vec![],
    };
    let r = ship_raw_intents(&fx.endpoint, "manual-holder", &frame).await;
    assert_eq!(
        r.status,
        ship::STATUS_DELEG_FENCED,
        "a fenced holder's intent batch refuses"
    );
    std::env::remove_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS");
    shutdown(&fx).await;
}

// ===========================================================================
// 8. Deferred setattr (the tar utimensat shape) + ordering barriers
// ===========================================================================

/// A setattr on a PENDING ino defers INTO the batch (zero wire — the tar
/// `utimensat` shape), updates the local image, and applies IN ORDER at
/// the flush: the owner's durable record carries the deferred times.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_setattr_on_a_pending_ino_defers_into_the_batch_and_applies_in_order() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "utime").await;

    let ino = Metadata::create(fx.client_be.as_ref(), d, "timed", FILE, 0, 0)
        .await
        .expect("mint")
        .ino;
    let ship0 = ship::stats().shipped_verbs;
    let def0 = istats().deferred_setattrs;
    let updated = Metadata::setattr(
        fx.client_be.as_ref(),
        ino,
        None,
        None,
        None,
        None,
        Some(1_111),
        Some(2_222),
        Some(3_333),
    )
    .await
    .expect("the deferred setattr acks locally");
    assert_eq!(updated.mtime, 2_222, "the local image carries the times");
    assert_eq!(ship::stats().shipped_verbs, ship0, "zero wire — deferred");
    assert!(istats().deferred_setattrs > def0);

    intents::fsync_dir_barrier(d).await.expect("flush");
    let inode = Metadata::getattr(fx.owner_be.as_ref(), ino)
        .await
        .expect("the applied record");
    assert_eq!(inode.atime, 1_111);
    assert_eq!(inode.mtime, 2_222);
    assert_eq!(inode.ctime, 3_333, "the deferred times applied in order");
    shutdown(&fx).await;
}

/// A shipped mutation naming pending state flushes FIRST (the ordering
/// barrier): an unlink of a pending name must apply AFTER the create it
/// names — never a NotFound, never a self-surrender racing its own batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shipped_mutation_naming_pending_state_flushes_first() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "barrier").await;

    let ino = Metadata::create(fx.client_be.as_ref(), d, "ephemeral", FILE, 0, 0)
        .await
        .expect("mint")
        .ino;
    let flushes0 = istats().flush_forces;
    let gone = Metadata::unlink(fx.client_be.as_ref(), d, "ephemeral")
        .await
        .expect("the unlink barriers, flushes, then applies");
    assert_eq!(gone, ino, "the unlink removed the minted child");
    assert!(
        istats().flush_forces > flushes0,
        "the barrier flushed before the ship"
    );
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "ephemeral").await.is_err(),
        "created-then-unlinked: nothing survives"
    );
    shutdown(&fx).await;
}

/// A create into a PENDING directory (the tar mkdir-then-populate shape)
/// barriers: the mkdir intent flushes, the create ships (earning the new
/// directory its own UPDATE grant), and subsequent creates in it mint
/// locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_into_a_pending_directory_flushes_then_earns_its_own_grant() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "subtree").await;

    // The mkdir MINTS (a directory intent under D's grant).
    let sub = Metadata::create(fx.client_be.as_ref(), d, "sub", DIR, 0, 0)
        .await
        .expect("the mkdir intent")
        .ino;
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "sub").await.is_err(),
        "the pending mkdir is not owner-visible yet"
    );
    // The first create INTO the pending directory barriers + ships +
    // earns.
    let f = Metadata::create(fx.client_be.as_ref(), sub, "first", FILE, 0, 0)
        .await
        .expect("the barriered create into the flushed directory")
        .ino;
    assert!(f > 1);
    wait_for("the subdirectory's own UPDATE grant", || {
        intents::holds_update_authority(sub)
    })
    .await;
    // Subsequent creates in the subdirectory mint locally.
    let ship0 = ship::stats().shipped_verbs;
    Metadata::create(fx.client_be.as_ref(), sub, "second", FILE, 0, 0)
        .await
        .expect("the local mint in the earned subdirectory");
    assert_eq!(ship::stats().shipped_verbs, ship0, "zero wire");

    intents::fsync_dir_barrier(sub).await.expect("flush");
    Metadata::lookup(fx.owner_be.as_ref(), sub, "second")
        .await
        .expect("the subtree applied");
    shutdown(&fx).await;
}

// ===========================================================================
// 9. MW-8 — the crash law (cargo shape; the kill-9 halves live in the leg)
// ===========================================================================

/// MW-8: an UN-FLUSHED batch dies with the client — the acked-un-fsynced
/// class, disclosed ("fsync(dir) was the contract point; recall on death
/// is trivially complete") — while a FLUSHED batch is durable. The cargo
/// shape drops the client's intent state (the process-death analog); the
/// live leg kill-9s the daemon on both sides of fsync(dir).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unflushed_batch_dies_with_the_client_and_a_flushed_batch_is_durable() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "mw8").await;

    // Flushed half: durable.
    Metadata::create(fx.client_be.as_ref(), d, "durable", FILE, 0, 0)
        .await
        .expect("mint");
    intents::fsync_dir_barrier(d).await.expect("the contract point");

    // Un-flushed half: acked, then the client DIES (state dropped).
    Metadata::create(fx.client_be.as_ref(), d, "lost", FILE, 0, 0)
        .await
        .expect("the provisional ack");
    intents::test_clear_intents(); // the process-death analog

    Metadata::lookup(fx.owner_be.as_ref(), d, "durable")
        .await
        .expect("the fsynced name survived — fsync(dir) IS the contract point");
    assert!(
        Metadata::lookup(fx.owner_be.as_ref(), d, "lost").await.is_err(),
        "the un-fsynced batch died with the client (MW-8, disclosed)"
    );
    shutdown(&fx).await;
}

// ===========================================================================
// 10. The storm row (OQ-2's price + the valve) — spec R5's shape
// ===========================================================================

/// The hot-directory grant/recall storm: two clients alternating shipped
/// creates in ONE directory ping-pong the exclusive UPDATE grant; the
/// rung-11 valve must DEMOTE the object before the fan-out hurts (spec
/// R5), after which creates ship grant-less (correct, priced) — the
/// recall volume is bounded by `thrash_cycles`, never by the storm's
/// length. This is the cargo face of the leg's storm row (the OQ-2
/// reopening trigger's instrument).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_grant_ping_pong_storm_engages_the_valve_before_fanout_hurts() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    let (d, _f0) = earn_update(&fx, "storm").await;

    let demotions0 = ship::global_recall_lane().stats().thrash_demotions;
    // Alternate: A mints one name (holding the grant), B ships one (the
    // conflict — recall + re-grant to B... and back). Each B create is a
    // grant→recall episode on D.
    for round in 0..12 {
        if intents::holds_update_authority(d) {
            Metadata::create(fx.client_be.as_ref(), d, &format!("a{round}"), FILE, 0, 0)
                .await
                .expect("A's create");
        } else {
            Metadata::create(fx.client_be.as_ref(), d, &format!("a{round}"), FILE, 0, 0)
                .await
                .expect("A's (shipped) create");
        }
        let results = ship_raw(
            &fx.endpoint,
            NODE_B,
            50,
            vec![MetaOp {
                id: round + 1,
                call: MetaCall::CreateWithRdev {
                    parent: d,
                    name: format!("b{round}"),
                    mode: FILE,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                },
            }],
        )
        .await;
        assert!(results[0].outcome.is_ok(), "B's storm create applies");
    }
    let lane = ship::global_recall_lane().stats();
    assert!(
        lane.thrash_demotions > demotions0,
        "the valve engaged before the fan-out hurt (spec R5) — \
         demotions {} -> {}",
        demotions0,
        lane.thrash_demotions
    );
    assert_eq!(ship::delegation_stats().recall_timeouts, 0, "no dead recalls");
    // Post-demotion the directory still WORKS — creates ship, exactly one
    // ack per name (the demoted posture is owner-served, never wedged).
    Metadata::create(fx.client_be.as_ref(), d, "post-demotion", FILE, 0, 0)
        .await
        .expect("the demoted directory still serves");
    shutdown(&fx).await;
}

// ===========================================================================
// 11. The dark posture + the lever
// ===========================================================================

/// The dark posture pays nothing: on an unarmed process every intent
/// gauge exports as zero, `update_intents_enabled()` is false (one relaxed
/// load), and the fsync barrier is a no-op — the shipped mount's shape,
/// structurally unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dark_posture_exports_zeros_and_pays_nothing() {
    let _plane = PLANE.lock().await;
    let _arm = ArmGuard; // restore statics even on panic
    ship::test_clear_delegations();
    intents::test_clear_intents();

    assert!(
        !intents::update_intents_enabled(),
        "unarmed ⇒ intents structurally off (the knob is read only when armed)"
    );
    let s = istats();
    assert_eq!(
        (s.batches, s.verbs, s.flush_forces, s.refusals, s.mints, s.pending),
        (0, 0, 0, 0, 0, 0),
        "every intent counter is 0 on the dark posture BY CONSTRUCTION"
    );
    intents::fsync_dir_barrier(1)
        .await
        .expect("the dark fsync barrier is a clean no-op");

    // The stats-inode object exports (the design-§13 spellings) as zeros.
    let json = intents::intent_stats_json();
    assert_eq!(json["meta_ship_intent_batches"], 0);
    assert_eq!(json["meta_ship_intent_verbs"], 0);
    assert_eq!(json["meta_ship_intent_flush_forces"], 0);
    assert_eq!(json["meta_ship_intent_refusals"], 0);
}

/// The A/B lever: `TEST_INTENTS_OVERRIDE = 2` (the `SQUEEZEFS_UPDATE_INTENTS=0`
/// face) keeps the armed plane grant-less on the UPDATE class — creates
/// ship exactly as rung 9's, no supply, no mints, every intent stat flat —
/// while LOOKUP delegations keep working (the levers are independent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lever_off_keeps_the_armed_plane_shipping_creates() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    intents::TEST_INTENTS_OVERRIDE.store(2, Ordering::SeqCst); // force OFF

    let d = Metadata::create(fx.owner_be.as_ref(), 1, "leveroff", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    let mints0 = istats().mints;
    let grants0 = istats().update_grants;
    let ship0 = ship::stats().shipped_verbs;
    for n in 0..4 {
        Metadata::create(fx.client_be.as_ref(), d, &format!("f{n}"), FILE, 0, 0)
            .await
            .expect("the lever-off create ships");
    }
    assert_eq!(istats().mints, mints0, "no mints under the lever");
    assert_eq!(istats().update_grants, grants0, "no UPDATE grants issued");
    assert!(
        ship::stats().shipped_verbs >= ship0 + 4,
        "every create rode the wire — rung 9's shape, verbatim"
    );
    shutdown(&fx).await;
}
