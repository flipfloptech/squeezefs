//! DLM S10 rung 14 — **client-owned-slot placement** (design-full-multi-writer
//! §8.2 lever 2, KD-MW-6; PR-plan row 14).
//!
//! # The laws under test (each arm red-first, per the charter)
//!
//! * **The mint-targeting law**: a placement-armed client's mints land in a
//!   slot DEDICATED to that client — stable across supply refills, distinct
//!   between clients, chosen OUTSIDE the volume's mint set (so the owner's
//!   own rotor never interleaves into it) — which is what makes "the
//!   client's slots" a real, migratable unit. The pick composes UNDER the
//!   mint constraint (`constrain_mint_volume` / `mint_redirects`), never
//!   above it: the volume is constrained first (one appender per volume,
//!   §6.2 items 2/3/4), the client slot is picked within it.
//! * **The migration policy**: sustained concentration (the supply-event
//!   run — the rung-11 pattern-vs-coincidence constant) toward a client
//!   that OWNS a volume (the ownership-map inversion — the
//!   fleet-of-authorities shape, §6.10 R4's recipe) counts a CANDIDATE and
//!   — since **KD-PV-13** — triggers nothing: every migration this policy
//!   can select is cross-owner by construction and sweep row 13 refuses
//!   it, so the launch arm is inert while a multi-owner plane is armed
//!   (`docs/design-per-volume-claim-admission.md` §5.5). With no
//!   client-owned candidate volume the policy is dark one step earlier —
//!   the inversion finds nothing at all.
//! * **The valve (never thrash)**: two clients alternating on one
//!   directory must not ping-pong its slot — migration episodes inside the
//!   thrash window count cycles, and at the rung-11 constant the slot
//!   DEMOTES to stay-put for the derived cooldown (`thrash_demotions`),
//!   re-promoting with the evidence reset after it.
//! * **Era fencing**: a fenced client's placement state dies with its
//!   incarnation (the rung-13 `note_client_incarnation` law) — a zombie's
//!   half-run must never compose with its successor's into a trigger.
//! * **The dark posture**: an unarmed mount has no placement state, no
//!   gauge movement, and the rotor path untouched; `SQUEEZEFS_SLOT_PLACEMENT=0`
//!   on an armed mount is the A/B control (mints ride the rotor exactly as
//!   rung 13 shipped them).
//!
//! Red-first: this suite lands BEFORE the implementation and fails to
//! compile at its commit (the rung-11/12/13 discipline).

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend, MINT_SPREAD,
};
use squeezefs::meta_ship::{
    self as ship, intents, placement, MetaCall, MetaOp, MetaShipRouter, MetaShipService, OwnerMap,
    PeerOwner,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"s10-slot-placement-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const FILE: u32 = libc::S_IFREG | 0o644;
const DIR: u32 = libc::S_IFDIR | 0o755;
/// The router/custody client identity (KD-MW-2 pair form).
const NODE: &str = "node_cafe.m0001";
/// The second client (the distinct-slot and ping-pong arms).
const NODE_B: &str = "node_beef.m0002";

/// Process-global planes — every test takes this exclusively (the
/// mw_intent_batch_tests discipline).
static PLANE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the solo posture on drop, so a panicking test can never leave
/// the binary's other tests armed, delegated, holding intents, or holding
/// placement state.
struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ship::uninstall_delegation_host();
        ship::disarm_ownership();
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        ship::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
        intents::TEST_INTENTS_OVERRIDE.store(0, Ordering::SeqCst);
        placement::TEST_PLACEMENT_OVERRIDE.store(0, Ordering::SeqCst);
        placement::TEST_MIGRATION_DISARM_OVERRIDE.store(0, Ordering::SeqCst);
        ship::TEST_INTENT_SUPPLY_CHUNK.store(0, Ordering::SeqCst);
        placement::uninstall_migration_executor();
        placement::test_clear_placement();
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

/// The placement venue: the rung-13 intent fixture over `vols` metadata
/// volumes, with the ownership map naming "owner-a" for every volume NOT
/// listed in `client_owned` and the named CLIENT id for each `(v_idx,
/// client)` pair — the in-process fleet-of-authorities shape the policy's
/// candidate inversion reads.
struct Fixture {
    _dir: tempfile::TempDir,
    owner_be: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    svc: Arc<MetaShipService>,
    /// Held for the custody plane's lifetime (the era gate's authority).
    _custody_client: Arc<WriteCustodyClient>,
    endpoint: String,
    router: Arc<MetaShipRouter>,
    _arm: ArmGuard,
}

async fn fixture(vols: usize, client_owned: &[(usize, &str)]) -> Fixture {
    std::env::set_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS", "500");
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = plan_meta_slot_set(vols).expect("derived plan");
    let mut paths = Vec::new();
    for (i, stamp) in plan.stamps.iter().enumerate().take(vols) {
        let p = make_file(dir.path(), &format!("meta{i}"), VOL_LEN);
        squeezefs::meta_backend::kv::builder::format_v3_stamped_single_writer(
            &p,
            VOL_LEN,
            &opts(),
            stamp.clone(),
        )
        .await
        .expect("format stamped meta volume");
        paths.push(p.display().to_string());
    }

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
    let verb_router = Arc::new(
        squeezefs::data_grant::AsyncVerbRouter::new()
            .with_meta(Arc::clone(&svc))
            .with_custody(custody_owner),
    );
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), verb_router)
        .expect("owner-side listener starts");
    let endpoint = listener.endpoint().to_string();

    ship::install_delegation_host(Arc::clone(&svc));

    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| {
            match client_owned.iter().find(|(idx, _)| *idx == v) {
                // The fleet-of-authorities entry: this volume is owned by
                // the CLIENT (its endpoint is never dialed by these arms —
                // the policy inversion reads the identity, the engine runs
                // on the physically-local set).
                Some((_, client)) => (v, PeerOwner::new(*client, "127.0.0.1:9")),
                None => (v, PeerOwner::new("owner-a", &endpoint)),
            }
        })
        .collect();
    let map = OwnerMap::for_volumes(&client_be, foreign).expect("owner map");
    ship::global_recall_lane().test_clear_state();
    ship::test_clear_delegations();
    intents::test_clear_intents();
    placement::test_clear_placement();
    ship::arm_ownership(map);
    let _arm = ArmGuard;
    ship::TEST_DELEGATION_OVERRIDE.store(1, Ordering::SeqCst);
    intents::TEST_INTENTS_OVERRIDE.store(1, Ordering::SeqCst);
    placement::TEST_PLACEMENT_OVERRIDE.store(1, Ordering::SeqCst);

    let custody_client = WriteCustodyClient::connect(&endpoint, SECRET, NODE)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&custody_client));

    let router = MetaShipRouter::new(Arc::clone(&client_be), NODE, SECRET.to_vec());
    Fixture {
        _dir: dir,
        owner_be,
        listener,
        svc,
        _custody_client: custody_client,
        endpoint,
        router,
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

fn pstats() -> placement::PlacementStats {
    placement::placement_stats()
}

/// Owner-side mkdir + the grant-earning SHIPPED create from the client
/// (the mw_intent_batch_tests helper verbatim): after this the client
/// holds the EXCLUSIVE UPDATE grant on the directory and mints locally.
async fn earn_update(fx: &Fixture, dirname: &str) -> (u64, u64) {
    let d = Metadata::create(fx.owner_be.as_ref(), 1, dirname, DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    let grants0 = intents::intent_stats().update_grants;
    let f0 = Metadata::create(fx.router.as_ref(), d, "f0", FILE, 0, 0)
        .await
        .expect("the grant-earning shipped create")
        .ino;
    wait_for("the UPDATE grant to install", || {
        intents::intent_stats().update_grants > grants0 && intents::holds_update_authority(d)
    })
    .await;
    (d, f0)
}

/// The volume's MINT SET recomputed from the live slot map (the
/// derive_mint_slots law: first `min(MINT_SPREAD, hosted)` hosted slots
/// ascending) — what the rotor rotates over, and what a client-dedicated
/// slot must sit OUTSIDE of.
fn mint_set_of(be: &RoutedMetaBackend, v_idx: usize) -> Vec<u16> {
    be.slot_map_snapshot()
        .iter()
        .enumerate()
        .filter(|(_, &v)| v == v_idx)
        .map(|(s, _)| s as u16)
        .take(MINT_SPREAD)
        .collect()
}

// ===========================================================================
// 1. The dark posture
// ===========================================================================

/// An UNARMED mount has no placement state: local creates ride the rotor,
/// every placement gauge stays 0, and no per-client assignment exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dark_posture_an_unarmed_mount_has_no_placement_state() {
    let _plane = PLANE.lock().await;
    placement::test_clear_placement();
    assert!(
        !ship::ownership_armed(),
        "precondition: the plane is unarmed"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = plan_meta_slot_set(1).expect("plan");
    let p = make_file(dir.path(), "meta0", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped_single_writer(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format");
    let be = open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("set");
    for i in 0..8 {
        Metadata::create(be.as_ref(), 1, &format!("f{i}"), FILE, 0, 0)
            .await
            .expect("local create");
    }
    let s = pstats();
    assert_eq!(s.client_slot_mints, 0, "no client-targeted mints unarmed");
    assert_eq!(s.client_slots, 0, "no assignments unarmed");
    assert_eq!(s.supply_events, 0, "no policy events unarmed");
    assert_eq!(s.sustain_evidence, 0, "no policy evidence unarmed");
    assert_eq!(s.migrations_triggered, 0);
    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// `SQUEEZEFS_SLOT_PLACEMENT=0` (the force-off seam) on an ARMED mount is
/// the A/B control: mints ride the rotor exactly as rung 13 shipped them —
/// the supply lands INSIDE the mint set and no placement state forms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lever_off_control_is_dark_on_an_armed_mount() {
    let _plane = PLANE.lock().await;
    let fx = fixture(1, &[]).await;
    placement::TEST_PLACEMENT_OVERRIDE.store(2, Ordering::SeqCst);
    let (d, _f0) = earn_update(&fx, "pl-off").await;
    let child = Metadata::create(fx.router.as_ref(), d, "c0", FILE, 0, 0)
        .await
        .expect("local mint")
        .ino;
    let slot = fx.owner_be.slot_of_ino(child) as u16;
    let mint_set = mint_set_of(&fx.owner_be, 0);
    assert!(
        mint_set.contains(&slot),
        "lever off: the supply must ride the rotor (slot {slot} inside the mint set)"
    );
    let s = pstats();
    assert_eq!(s.client_slot_mints, 0, "the control is dark");
    assert_eq!(s.client_slots, 0, "no assignment forms under the control");
    shutdown(&fx).await;
}

// ===========================================================================
// 2. The mint-targeting law
// ===========================================================================

/// A placement-armed client's mints land in its DEDICATED slot: outside
/// the volume's mint set (rotor-clean), stable across a supply refill, on
/// the volume the mint CONSTRAINT chose (the `mint_redirects` machinery is
/// composed under, never bypassed — every supply ino routes to the granted
/// directory's own volume), and the engagement counter accounts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_placement_armed_clients_mints_land_in_its_dedicated_slot_stably() {
    let _plane = PLANE.lock().await;
    let fx = fixture(1, &[]).await;
    // A tiny supply chunk so a refill happens within a handful of mints.
    ship::TEST_INTENT_SUPPLY_CHUNK.store(2, Ordering::SeqCst);
    let (d, _f0) = earn_update(&fx, "pl-stable").await;
    let (d_vol, _) = fx.owner_be.route_ino(d);

    let mut slots = std::collections::BTreeSet::new();
    let mut minted = 0u32;
    for i in 0..6 {
        let name = format!("c{i}");
        let mints0 = intents::intent_stats().mints;
        let child = Metadata::create(fx.router.as_ref(), d, &name, FILE, 0, 0)
            .await
            .expect("create under the grant")
            .ino;
        // Only LOCALLY MINTED children witness the supply's slot (a dry
        // pool ships the create, which mints on the owner's rotor —
        // excluded from the stability assertion by construction).
        if intents::intent_stats().mints > mints0 {
            minted += 1;
            slots.insert(fx.owner_be.slot_of_ino(child) as u16);
            let (v, _) = fx.owner_be.route_ino(child);
            assert_eq!(
                v, d_vol,
                "the mint constraint holds: the supply rode the granted dir's volume"
            );
        }
        // Drain + refill so the next mints draw from a FRESH supply
        // reservation (the stability-across-refills law).
        intents::fsync_dir_barrier(d).await.expect("flush");
    }
    assert!(minted >= 3, "the arm must exercise ≥ 2 supply reservations");
    assert_eq!(
        slots.len(),
        1,
        "ONE dedicated slot across refills (got {slots:?})"
    );
    let slot = *slots.iter().next().expect("one slot");
    let mint_set = mint_set_of(&fx.owner_be, d_vol);
    assert!(
        !mint_set.contains(&slot),
        "the dedicated slot sits OUTSIDE the mint set (rotor-clean; slot {slot})"
    );
    assert_ne!(slot, 0, "slot 0 is the root pin — never a client slot");
    let s = pstats();
    assert!(
        s.client_slot_mints >= 2,
        "client_slot_mints accounts the reservations (got {})",
        s.client_slot_mints
    );
    assert_eq!(s.client_slots, 1, "one live assignment");
    shutdown(&fx).await;
}

/// Two clients get DISTINCT dedicated slots — the per-client unit is what
/// the migration policy moves, so two clients sharing one slot would make
/// the unit a lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_placement_armed_clients_get_distinct_dedicated_slots() {
    let _plane = PLANE.lock().await;
    let fx = fixture(1, &[]).await;
    let (d, _f0) = earn_update(&fx, "pl-two").await;
    let child_a = Metadata::create(fx.router.as_ref(), d, "a0", FILE, 0, 0)
        .await
        .expect("client A mints")
        .ino;
    let slot_a = fx.owner_be.slot_of_ino(child_a) as u16;

    // The SECOND client earns its own grant over the raw wire (the
    // mw_intent_batch two-client pattern): the reply's grant carries its
    // supply, whose first ino names B's dedicated slot.
    let db = Metadata::create(fx.owner_be.as_ref(), 1, "pl-two-b", DIR, 0, 0)
        .await
        .expect("owner mkdir for B")
        .ino;
    let mut raw = cw::RpcClient::connect(&fx.endpoint, SECRET, NODE_B, None)
        .await
        .expect("raw client B connects");
    let frame = ship::MetaRequestFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 7,
        client_id: NODE_B.to_string(),
        owner_term: fx.svc.term(),
        ops: vec![MetaOp {
            id: 1,
            call: MetaCall::CreateWithRdev {
                parent: db,
                name: "b0".to_string(),
                mode: FILE,
                uid: 0,
                gid: 0,
                rdev: 0,
            },
        }],
    };
    let reply = raw
        .call(
            ship::VERB_META_BATCH,
            ship::encode_request(&frame).expect("encode"),
        )
        .await
        .expect("B's grant-earning create round-trips");
    assert_eq!(reply.status, cw::RPC_OK);
    let results = ship::decode_reply(&reply.body).expect("decode").results;
    let grant = results[0]
        .intent_grant
        .as_ref()
        .expect("B earned the UPDATE grant");
    let supply = grant.supply.as_ref().expect("B's grant carries a supply");
    let slot_b = fx.owner_be.slot_of_ino(supply.first_global) as u16;

    assert_ne!(
        slot_a, slot_b,
        "distinct clients get distinct dedicated slots"
    );
    assert_eq!(pstats().client_slots, 2, "two live assignments");
    shutdown(&fx).await;
}

// ===========================================================================
// 3. The migration policy
// ===========================================================================

/// **KD-PV-13 — `the_migration_half_is_disarmed_under_multi_owner`.**
///
/// Sustained concentration from a client that OWNS a volume (the
/// fleet-of-authorities inversion) still counts its CANDIDATE — that is the
/// follow-on's demand signal — but launches nothing at all.
///
/// The reason is structural, not a policy preference: the policy's target
/// is a client-owned volume (`owned[0]`) and its victim's home is by
/// construction NOT in that set, so **every** migration it can select is a
/// CROSS-OWNER one, which sweep row 13 of
/// `docs/design-per-volume-claim-admission.md` refuses (D19 defers the
/// two-party hand-off; the move would also relocate an inode record away
/// from the dentry naming it, CREATING cross-owner names). Left armed, the
/// composition is a permanent trigger → refuse → fail retry loop and a
/// counter that grows for ever on a healthy fleet. So the launch arm is
/// inert while a multi-owner plane is armed, and
/// `migrations_triggered`/`migrations_failed`/`migrations_completed` are
/// **structurally 0** (§11.2: nonzero means the disarm broke).
///
/// PR 4 re-scoped this pin from "the engine completes" to "the engine
/// refuses"; this is the second half of the same flip. The ENGINE's own
/// coverage lives in
/// `the_migration_engine_moves_a_slot_when_no_multi_owner_plane_is_armed`
/// and the POLICY's launch machinery — the valve the follow-on inherits —
/// in `two_clients_alternating_on_one_directory_never_ping_pong_the_slot`,
/// which drives it through the declared test override.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migration_half_is_disarmed_under_multi_owner() {
    let _plane = PLANE.lock().await;
    // Volume 1 is OWNED BY THE CLIENT (the §6.10 R4 recipe, in-process).
    let fx = fixture(2, &[(1, NODE)]).await;
    placement::install_migration_executor(placement::authority_migration_executor(Arc::clone(
        &fx.owner_be,
    )));
    ship::TEST_INTENT_SUPPLY_CHUNK.store(2, Ordering::SeqCst);

    let (d, _f0) = earn_update(&fx, "pl-policy").await;
    // Drive supply events (grant + refills) well past the sustain
    // threshold: mint through the real machinery, fsync forcing each
    // refill.
    for i in 0..10 {
        let _ = Metadata::create(fx.router.as_ref(), d, &format!("c{i}"), FILE, 0, 0).await;
        intents::fsync_dir_barrier(d).await.expect("flush");
    }
    wait_for("the candidate inversion to fire", || {
        pstats().migration_candidates >= 1
    })
    .await;
    let s = pstats();
    assert!(
        s.migration_candidates >= 1,
        "the candidate inversion found the client-owned volume — the follow-on's demand signal \
         keeps counting"
    );
    assert_eq!(
        s.migrations_triggered, 0,
        "KD-PV-13: nothing may be TRIGGERED while a multi-owner plane is armed"
    );
    assert_eq!(s.migrations_failed, 0, "and therefore nothing fails");
    assert_eq!(s.migrations_completed, 0, "and nothing completes");
    assert_eq!(
        s.thrash_demotions, 0,
        "a valve that charges episodes for moves that never launch would demote slots on a \
         healthy fleet"
    );

    // Nothing moved: the client's assignment stands where it was.
    let map = fx.owner_be.slot_map_snapshot();
    let assigned = placement::client_assigned_slots(NODE);
    assert!(
        assigned.iter().all(|&s| map[usize::from(s)] != 1),
        "a disarmed policy must leave the slot map untouched (assigned {assigned:?})"
    );
    shutdown(&fx).await;
}

/// The ENGINE's own coverage, preserved from the case above: with NO
/// multi-owner plane armed — every shipped fleet — `migrate_slot` moves a
/// slot between volumes exactly as it always has. Row 13's refusal is
/// scoped to the armed plane, and this is what says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migration_engine_moves_a_slot_when_no_multi_owner_plane_is_armed() {
    let _plane = PLANE.lock().await;
    let fx = fixture(2, &[(1, NODE)]).await;
    // The engine runs on the physically-local owner set; the plane is the
    // policy's input, not the engine's, so disarming it is exactly the
    // shipped topology.
    ship::disarm_ownership();
    // Never slot 0: ino 1 pins to it and KD-PV-6 makes it non-migratable
    // under an armed plane — the engine's coverage is about any OTHER slot.
    let victim = fx
        .owner_be
        .slot_map_snapshot()
        .iter()
        .enumerate()
        .find(|&(slot, &v)| slot != 0 && v == 0)
        .map(|(slot, _)| slot as u16)
        .expect("volume 0 hosts a non-zero slot");
    squeezefs::meta_backend::slot_migration::migrate_slot(
        &fx.owner_be,
        victim,
        1,
        &squeezefs::meta_backend::slot_migration::MigrationOptions::default(),
        &squeezefs::meta_backend::slot_migration::MigrationTestHooks::default(),
    )
    .await
    .expect("the online migration engine still moves a slot");
    assert_eq!(
        fx.owner_be.slot_map_snapshot()[usize::from(victim)],
        1,
        "the slot's home volume moved"
    );
    shutdown(&fx).await;
}

/// With NO client-owned candidate volume the policy is dark for a reason
/// INDEPENDENT of KD-PV-13's disarm: the candidate inversion itself finds
/// nothing, so no demand is even signalled. Both halves matter and this pin
/// carries the first — `migration_candidates` is the follow-on's demand
/// signal, and a fleet where the shipping client owns nothing must not
/// manufacture one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_policy_stays_dark_when_no_client_owned_volume_exists() {
    let _plane = PLANE.lock().await;
    let fx = fixture(1, &[]).await;
    ship::TEST_INTENT_SUPPLY_CHUNK.store(2, Ordering::SeqCst);
    let (d, _f0) = earn_update(&fx, "pl-dark").await;
    for i in 0..8 {
        let _ = Metadata::create(fx.router.as_ref(), d, &format!("c{i}"), FILE, 0, 0).await;
        intents::fsync_dir_barrier(d).await.expect("flush");
    }
    let s = pstats();
    assert!(s.supply_events >= 3, "the run was sustained");
    assert_eq!(s.migration_candidates, 0, "no candidate on this topology");
    assert_eq!(s.migrations_triggered, 0, "nothing migrates");
    assert_eq!(s.thrash_demotions, 0, "the valve never engages");
    shutdown(&fx).await;
}

// ===========================================================================
// 4. The valve (never thrash) — direct-drive with injected time, the
//    rung-11 storm-table discipline (no sleeps; instants are fabricated).
// ===========================================================================

/// Two clients alternating on ONE directory must not ping-pong its slot:
/// migration episodes inside the thrash window count cycles, the slot
/// demotes at the rung-11 constant, holds through the cooldown, and
/// re-promotes with the evidence reset after it.
///
/// **This is the launch machinery KD-PV-13 disarms and D19's named
/// follow-on inherits**, so the case drives it through the declared
/// override (`TEST_MIGRATION_DISARM_OVERRIDE = 2`, the `arm_ownership`
/// law: reachable so the behaviour is TESTED rather than commented). The
/// disarm itself is pinned by `the_migration_half_is_disarmed_under_multi_owner`
/// above, which runs the same fixture with the override off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_alternating_on_one_directory_never_ping_pong_the_slot() {
    let _plane = PLANE.lock().await;
    // X owns volume 1, Y owns volume 2 — the contended slot is a shared
    // directory's, associated to BOTH clients.
    let fx = fixture(3, &[(1, NODE), (2, NODE_B)]).await;
    placement::TEST_MIGRATION_DISARM_OVERRIDE.store(2, Ordering::SeqCst);
    let calls: Arc<parking_lot::Mutex<Vec<(u16, usize)>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    {
        let calls = Arc::clone(&calls);
        placement::install_migration_executor(Arc::new(move |slot, target| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                calls.lock().push((slot, target));
                Ok(())
            })
        }));
    }
    let cfg = placement::PolicyConfig {
        sustain_runs: 3,
        thrash_cycles: 3,
        window: Duration::from_secs(60),
        cooldown: Duration::from_secs(480),
    };
    let hot_slot: u16 = 4242;
    placement::note_client_dir(NODE, hot_slot);
    placement::note_client_dir(NODE_B, hot_slot);

    let t0 = Instant::now();
    let mut now = t0;
    // The contended slot's live home never reaches a client-owned volume
    // (the recording executor moves nothing) — the ping-pong shape.
    let drive = |client: &str, now: Instant| {
        for _ in 0..cfg.sustain_runs {
            placement::note_supply_event(client, &cfg, now, |_| Some(0));
        }
    };

    // Alternating sustained runs, all inside one thrash window: the
    // rung-11 table's shape — cycles 1, 2, 3 issue, the 3rd ENGAGES the
    // valve, and every later attempt holds.
    drive(NODE, now); // cycle 1: migrate hot_slot -> vol 1
    wait_for("move 1", || pstats().migrations_completed >= 1).await;
    now += Duration::from_secs(1);
    drive(NODE_B, now); // cycle 2: migrate hot_slot -> vol 2
    wait_for("move 2", || pstats().migrations_completed >= 2).await;
    now += Duration::from_secs(1);
    drive(NODE, now); // cycle 3: issues AND demotes (engaged-at-3)
    wait_for("move 3", || pstats().migrations_completed >= 3).await;
    assert_eq!(pstats().thrash_demotions, 1, "the valve engaged at cycle 3");
    now += Duration::from_secs(1);
    drive(NODE_B, now); // held: demoted
    drive(NODE, now + Duration::from_secs(1)); // held: demoted
    assert_eq!(
        pstats().migrations_triggered,
        3,
        "migrations FLAT after demotion (the never-thrash law)"
    );
    assert!(pstats().valve_holds >= 2, "the holds are counted");
    assert_eq!(
        calls.lock().len(),
        3,
        "the executor saw exactly the pre-demotion moves"
    );

    // Past the cooldown the slot re-promotes with the evidence reset: one
    // fresh sustained run migrates again, and ONLY one cycle is counted.
    let past = now + cfg.cooldown + Duration::from_secs(1);
    drive(NODE_B, past);
    wait_for("the re-promoted move", || {
        pstats().migrations_completed >= 4
    })
    .await;
    assert_eq!(
        pstats().thrash_demotions,
        1,
        "re-promotion reset the evidence — no second demotion from one move"
    );
    shutdown(&fx).await;
}

// ===========================================================================
// 5. Era fencing
// ===========================================================================

/// A fenced client's placement state dies with its incarnation: a frame
/// from a NEW client_epoch (the rung-13 `note_client_incarnation` law)
/// clears the old incarnation's assignments and policy evidence — a
/// zombie's half-run can never compose with its successor's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fenced_clients_placement_state_dies_with_its_incarnation() {
    let _plane = PLANE.lock().await;
    let fx = fixture(1, &[]).await;
    let (d, _f0) = earn_update(&fx, "pl-fence").await;
    let _ = Metadata::create(fx.router.as_ref(), d, "c0", FILE, 0, 0)
        .await
        .expect("mint");
    intents::fsync_dir_barrier(d).await.expect("flush");
    let before = pstats();
    assert!(before.client_slots >= 1, "state exists before the fence");
    assert!(
        before.supply_events >= 1,
        "evidence exists before the fence"
    );

    // The SAME identity presents a NEW incarnation over the wire: the
    // owner's incarnation hook fires and the placement state dies.
    let mut raw = cw::RpcClient::connect(&fx.endpoint, SECRET, NODE, None)
        .await
        .expect("successor incarnation connects");
    let frame = ship::MetaRequestFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 0xDEAD_BEEF, // ≠ the router's epoch
        client_id: NODE.to_string(),
        owner_term: fx.svc.term(),
        ops: vec![MetaOp {
            id: 999,
            call: MetaCall::Getattr { ino: 1 },
        }],
    };
    let reply = raw
        .call(
            ship::VERB_META_BATCH,
            ship::encode_request(&frame).expect("encode"),
        )
        .await
        .expect("successor frame round-trips");
    assert_eq!(reply.status, cw::RPC_OK);

    wait_for("the placement fence", || pstats().fences >= 1).await;
    let after = pstats();
    assert_eq!(
        after.client_slots, 0,
        "the fenced incarnation's assignments died"
    );
    assert_eq!(
        after.sustain_evidence, 0,
        "the policy evidence died with it"
    );
    shutdown(&fx).await;
}
