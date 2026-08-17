//! DLM S10 rung 12 — **LOOKUP-class subtree delegations** (the engine whose
//! brake — the rung-11 recall lane + thrash valve — is already merged).
//!
//! Charter: `docs/design-full-multi-writer.md` §8.2 lever 1 + PR row 12 —
//! *"LOOKUP-class delegations: piggybacked grant, coherence law
//! (recall-before-conflicting-publish), grace re-assertion; red-first
//! stale-serve test; `SQUEEZEFS_DELEGATION` lever"* — consuming the frozen
//! rung-11 API verbatim (`.benchmarks/2026-08-17-s10-recall-valve.md`:
//! `try_grant` at grant issuance, `recall_object` + `issue_pass` →
//! `RecallFrame`s as the DelegRecall payload, `ack_frame`, `expire_overdue`
//! as the eviction-escalation input).
//!
//! # The semantics under test (from the design, §8.2)
//!
//! * A delegation is a **capability token over an object** (class
//!   `DELEG_LOOKUP` — `UPDATE`/`PERM`/`XATTR` are rows 13+'s), granted by
//!   the owning authority and **piggybacked on the reply of a metadata RPC
//!   the client was already issuing** (the intent-lock law: acquisition is
//!   never a separate round trip).
//! * What it buys: the holder serves lookups / getattrs / readdir for the
//!   delegated object from its **reader-revalidation view** — locally,
//!   zero round trips — because the delegation is the *coherence promise*:
//!   **the owner recalls before any conflicting mutation publishes**
//!   (recall-before-conflicting-publish, enforced OWNER-side), and the
//!   grant's **stamp** (the object's ctime/mtime/size at grant) gates the
//!   warming window (a view that has not caught up to the grant never
//!   serves).
//! * Recall rides the rung-11 lane under its valve/deadline law; a timeout
//!   is terminal, LOUD, and escalates to membership eviction (the
//!   `transport_lease_overlong` precedent — never a silent wait).
//! * Delegations are RAM (KD-MW-5): across an authority restart the
//!   holder re-asserts in the successor's grace window (the S6/S8 law —
//!   reclaim admitted, conflicting fresh mutations refused in-window).
//! * Everything rides `SQUEEZEFS_DELEGATION` (ENG-10; static default `on`,
//!   read only when the mw plane is armed — announced-inert otherwise).
//!
//! Red-first: this suite landed BEFORE the implementation and fails to
//! compile at that commit (the rung-11 discipline — the red state is
//! captured in git, and the spec-R5-style red HALF here is permanent:
//! `red_half_without_the_law_a_delegated_holder_serves_stale` runs the
//! identical shape against the law-disabled seam and proves the stale
//! serve HAPPENS, so the green half can never pass vacuously).

use squeezefs::cluster_wire as cw;
use squeezefs::meta_backend::kv::revalidate::RevalidationPoller;
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::{
    self as ship, DelegPollFrame, DelegReassertFrame, MetaCall, MetaOp, MetaRequestFrame,
    MetaShipRouter, MetaShipService, OwnerMap, PeerOwner, DELEG_CLASS_LOOKUP,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"s10-delegation-storage-trust-secret";
const VOL_LEN: u64 = 256 * 1024 * 1024;
const FILE: u32 = libc::S_IFREG | 0o644;
const DIR: u32 = libc::S_IFDIR | 0o755;

/// Process-global planes (ownership arm, the delegation host, the global
/// recall lane, the client caches) — every test takes this exclusively,
/// exactly as `meta_ship_tests.rs` guards its arm.
static PLANE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the solo posture on drop, so a panicking test can never leave
/// the binary's other tests armed or delegated.
struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ship::uninstall_daemon_verb_router();
        ship::uninstall_delegation_host();
        ship::disarm_ownership();
        ship::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
        ship::TEST_DELEG_COHERENCE_LAW.store(true, Ordering::SeqCst);
        ship::test_clear_delegations();
        ship::test_clear_token_cache();
        // The GLOBAL lane's grant/thrash state must not leak into the
        // next test's identically-numbered inos (fresh volume per test —
        // ino numbering restarts).
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

/// The delegation venue: ONE volume set, opened TWICE in this process —
/// the authority's writer set and the holder's **reader** set over the
/// same files (the co-writer's §6.8 item-2 revalidation view, which is
/// exactly what a delegated serve reads from — design §8.2: "the co-writer
/// serves lookups/getattrs/readdir from its reader-revalidation view").
struct Fixture {
    _dir: tempfile::TempDir,
    owner_be: Arc<RoutedMetaBackend>,
    client_be: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    svc: Arc<MetaShipService>,
    endpoint: String,
    router: Arc<MetaShipRouter>,
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

    let svc = MetaShipService::new(Arc::clone(&owner_be));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), svc.clone())
        .expect("owner-side listener starts");
    let endpoint = listener.endpoint().to_string();

    // The delegation HOST: the owner-side arm — grant issuance, the recall
    // channel, and the coherence gate the RoutedMetaBackend mutation
    // surface consults. Installing it is what `arm_multi_writer` does in
    // production.
    ship::install_delegation_host(Arc::clone(&svc));

    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("owner-a", &endpoint)))
        .collect();
    let map = OwnerMap::for_volumes(&client_be, foreign).expect("owner map");
    // A fresh plane for this fixture: the previous test's lane state
    // names THIS volume's ino numbers (every fixture volume starts at 1).
    ship::global_recall_lane().test_clear_state();
    ship::test_clear_delegations();
    ship::arm_ownership(map);
    let _arm = ArmGuard;
    ship::TEST_DELEGATION_OVERRIDE.store(1, Ordering::SeqCst);

    let router = MetaShipRouter::new(Arc::clone(&client_be), "node_cafe.m0001", SECRET.to_vec());
    // The LIVE co-writer shape (rung-12 finding #1's repro): the daemon
    // verb router is INSTALLED, so the holder's own backend trait verbs
    // route through the ship plane exactly as a mounted co-writer's do.
    // Pre-fix, every delegated serve recursed through this hook into
    // itself (the fleet's fuse3-lane stack overflow); the fix reads the
    // local view through the hook-free `getattr_local`/`readdir_local`.
    ship::install_daemon_verb_router(Arc::clone(&router));
    Fixture {
        _dir: dir,
        owner_be,
        client_be,
        listener,
        svc,
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

/// Advance the holder's reader view past the authority's latest commits:
/// checkpoint every volume, then one revalidation pass (a FRESH poller per
/// call so its cadence mark never suppresses a repeat catch-up).
async fn catch_up(fx: &Fixture) {
    for vol in &fx.owner_be.volumes {
        vol.checkpoint_now().await.expect("checkpoint");
    }
    let poller = RevalidationPoller::new(1);
    let outcomes = poller
        .poll_set_at(&fx.client_be.volumes, Instant::now())
        .await;
    for (idx, res) in &outcomes {
        res.as_ref()
            .unwrap_or_else(|e| panic!("volume {idx} revalidation failed: {e:?}"));
    }
}

/// Bounded wait for an asynchronous plane transition (channel spin-up,
/// reconnect, re-assert). A bounded RETRY loop over an observable counter
/// — never a bare sleep standing in for the assertion itself.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..600 {
        if cond() {
            return;
        }
        squeezefs_ipc::sqz_time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn deleg() -> ship::DelegationStats {
    ship::delegation_stats()
}

fn lane() -> ship::RecallLaneStats {
    ship::global_recall_lane().stats()
}

// ===========================================================================
// 1. The wire vocabulary (schema 2): DelegGrant / DelegRecall / DelegReassert
// ===========================================================================

/// The delegation verbs join the SAME vocabulary and era discipline as the
/// S8 verbs: schema bumped to 2 (versioned, refusal on unknown — KD-MW-11:
/// no incompat bit, wire schema versions carry compatibility), new frames
/// round-trip, untrusted bytes refuse loud with the bounded decode, and
/// the verb block collides with none of S3/S4/S8/S9's.
#[test]
fn the_wire_carries_the_delegation_vocabulary_and_refuses_untrusted_bytes() {
    assert_eq!(
        ship::META_SHIP_SCHEMA,
        2,
        "the delegation verbs are the schema-2 bump (design §11: 'schema +1')"
    );
    // Verb block: its own range, disjoint from ping (0), meta batch
    // (16/17), custody (0x0200), publish (0x0300).
    assert_eq!(ship::VERB_DELEG_RECALL, 0x0400);
    assert_eq!(ship::VERB_DELEG_REASSERT, 0x0401);
    const _: () = assert!(ship::VERB_DELEG_RECALL > ship::VERB_RECLAIM);
    assert_eq!(DELEG_CLASS_LOOKUP, 1, "the LOOKUP capability bit");

    let poll = DelegPollFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 7,
        client_id: "node_cafe.m0001".to_string(),
        acks: vec![3, 4],
    };
    let bytes = ship::encode_deleg_poll(&poll).expect("encode");
    assert_eq!(ship::decode_deleg_poll(&bytes).expect("decode"), poll);

    let reply = ship::DelegPollReply {
        schema: ship::META_SHIP_SCHEMA,
        owner_term: 9,
        frames: vec![ship::WireRecallFrame {
            frame_id: 11,
            inos: vec![2, 3],
        }],
        fence_seq: 42,
        park_ms: 1000,
        deadline_ms: 45_000,
    };
    let bytes = ship::encode_deleg_poll_reply(&reply).expect("encode");
    assert_eq!(
        ship::decode_deleg_poll_reply(&bytes).expect("decode"),
        reply
    );

    let re = DelegReassertFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 7,
        client_id: "node_cafe.m0001".to_string(),
        inos: vec![2, 5, 9],
    };
    let bytes = ship::encode_deleg_reassert(&re).expect("encode");
    assert_eq!(ship::decode_deleg_reassert(&bytes).expect("decode"), re);

    // Untrusted bytes refuse loud (the bounded-decode law); a lying
    // in-body length can never become an allocation authority.
    assert!(ship::decode_deleg_poll(&[0xff; 64]).is_err());
    assert!(ship::decode_deleg_poll_reply(&[0xff; 64]).is_err());
    assert!(ship::decode_deleg_reassert(&[0xff; 64]).is_err());
    assert!(ship::decode_deleg_reassert_reply(&[0xff; 64]).is_err());
}

// ===========================================================================
// 2. The dark lever + solo posture
// ===========================================================================

/// `SQUEEZEFS_DELEGATION=0` (the A/B control) keeps the plane structurally
/// unchanged: no grant rides any reply, nothing serves locally, every
/// delegation stat stays 0, and the rung-11 lane never moves. The
/// instrument stays alive on both sides — the design's A/B-lever law.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dark_lever_off_keeps_the_plane_structurally_unchanged() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    ship::TEST_DELEG_COHERENCE_LAW.store(true, Ordering::SeqCst);
    ship::TEST_DELEGATION_OVERRIDE.store(2, Ordering::SeqCst); // force OFF

    let d0 = deleg();
    let l0 = lane();
    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "dark", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "f", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;

    let shipped0 = ship::stats().shipped_verbs;
    for _ in 0..4 {
        fx.router
            .lookup(dir_ino, "f")
            .await
            .expect("shipped lookup");
    }
    let shipped_delta = ship::stats().shipped_verbs - shipped0;
    assert!(
        shipped_delta >= 4,
        "with the lever OFF every repeat lookup must SHIP (got {shipped_delta} shipped verbs)"
    );
    let d1 = deleg();
    let l1 = lane();
    assert_eq!(d1.grants, d0.grants, "no grant may be issued lever-off");
    assert_eq!(d1.installs, d0.installs);
    assert_eq!(d1.hits, d0.hits, "no delegated serve lever-off");
    assert_eq!(d1.entries, 0, "client delegation cache stays empty");
    assert_eq!(
        l1.grants, l0.grants,
        "the rung-11 lane must not move lever-off (its population is the delegation plane's)"
    );
    // The stats surface exports zeros rather than disappearing (the
    // instrument stays alive on both sides of the A/B).
    let json = ship::delegation_stats_json();
    assert_eq!(json["dlm_delegation_grants"].as_u64(), Some(d0.grants));
    shutdown(&fx).await;
}

// ===========================================================================
// 3. The piggyback law + delegated local serves
// ===========================================================================

/// Grants ride the replies of metadata RPCs the client was already issuing
/// — ZERO additional round trips (`dlm_rpcs_meta` accounts every wire
/// round; its delta over a granted sequence equals the frames the verbs
/// themselves cost) — and once the holder's reader view has caught up to
/// the grant's stamp, lookups/getattrs/readdir serve LOCALLY: hits grow,
/// shipped verbs stay flat. That non-shipping is the entire point of S10.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grants_piggyback_on_replies_and_delegated_serves_stop_shipping() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "hotdir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    let child_ino = Metadata::create(fx.owner_be.as_ref(), dir_ino, "child", FILE, 0, 0)
        .await
        .expect("owner create")
        .ino;
    catch_up(&fx).await;

    // The granted sequence: one lookup earns the parent AND the child
    // delegations (over-issue on grant — Ceph's move, design §8.2), with
    // zero round trips beyond the verbs themselves.
    let d0 = deleg();
    let rpcs0 = ship::stats().dlm_rpcs_meta;
    let inode = fx.router.lookup(dir_ino, "child").await.expect("lookup");
    assert_eq!(inode.ino, child_ino);
    let rpcs_delta = ship::stats().dlm_rpcs_meta - rpcs0;
    let d1 = deleg();
    assert!(
        d1.grants > d0.grants,
        "the lookup's reply must carry delegation grants"
    );
    assert!(
        d1.installs > d0.installs,
        "the client must install the piggybacked grants"
    );
    assert!(d1.entries >= 1, "client cache holds the delegation(s)");
    assert!(
        rpcs_delta <= 2,
        "grant acquisition must never be its own round trip: the lookup \
         sequence cost {rpcs_delta} wire rounds (the verb's own frames only)"
    );

    // The view already matches the grant stamps (nothing mutated since the
    // catch-up), so the SAME verbs now serve locally.
    let shipped0 = ship::stats().shipped_verbs;
    let hits0 = deleg().hits;
    for _ in 0..8 {
        let got = fx.router.lookup(dir_ino, "child").await.expect("lookup");
        assert_eq!(got.ino, child_ino);
        let attr = fx.router.getattr(dir_ino).await.expect("getattr");
        assert_eq!(attr.ino, dir_ino);
        let entries = fx.router.readdir(dir_ino, 0, 128).await.expect("readdir");
        assert!(entries.iter().any(|e| e.name == "child"));
    }
    let shipped_delta = ship::stats().shipped_verbs - shipped0;
    let hits_delta = deleg().hits - hits0;
    assert_eq!(
        shipped_delta, 0,
        "delegated LOOKUP-class verbs must NOT ship — that is the point \
         (shipped {shipped_delta} verbs during the delegated loop)"
    );
    assert!(
        hits_delta >= 24,
        "the delegated serves must account on the hits ledger (got {hits_delta})"
    );

    // A delegated NEGATIVE lookup is authoritative (the coherence law
    // guarantees the view's dentry set is exact while the grant is live).
    let err = fx
        .router
        .lookup(dir_ino, "never-created")
        .await
        .expect_err("negative lookup");
    assert_eq!(err.to_errno(), libc::ENOENT);
    assert_eq!(
        ship::stats().shipped_verbs,
        shipped0,
        "the authoritative negative serve must not ship either"
    );

    assert_eq!(deleg().stale_serves, 0, "the must-stay-0 tripwire");
    shutdown(&fx).await;
}

/// The warming window: a grant whose stamp the holder's view has NOT yet
/// caught up to never serves locally (it ships instead). The stamp is what
/// makes "serves from the reader-revalidation view" sound without
/// tightening the reader staleness bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_view_behind_the_grant_stamp_never_serves() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "warmdir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    catch_up(&fx).await;
    // Mutate AFTER the catch-up so the owner's stamp is ahead of the view,
    // then earn the grant: its stamp names a state the reader has not seen.
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "ahead", FILE, 0, 0)
        .await
        .expect("owner create");
    fx.router
        .lookup(dir_ino, "ahead")
        .await
        .expect("shipped lookup earns the grant (owner-current answer)");

    let hits0 = deleg().hits;
    let shipped0 = ship::stats().shipped_verbs;
    // The view is BEHIND the stamp: this must ship, not serve stale.
    let got = fx.router.lookup(dir_ino, "ahead").await.expect("lookup");
    assert!(got.ino > 0);
    assert_eq!(
        deleg().hits,
        hits0,
        "a behind-the-stamp view must never produce a delegated serve"
    );
    assert!(ship::stats().shipped_verbs > shipped0, "it ships instead");

    // Catch the view up: the SAME grant now serves.
    catch_up(&fx).await;
    let shipped1 = ship::stats().shipped_verbs;
    fx.router.lookup(dir_ino, "ahead").await.expect("lookup");
    assert!(
        deleg().hits > hits0,
        "once the view reaches the stamp the delegation serves"
    );
    assert_eq!(ship::stats().shipped_verbs, shipped1, "and stops shipping");
    assert_eq!(deleg().stale_serves, 0);
    shutdown(&fx).await;
}

// ===========================================================================
// 4. THE RED HALF (permanent): without the law, the holder serves stale
// ===========================================================================

/// **The charter's red-first stale-serve test.** With the coherence law
/// DISABLED (a test-only seam, deliberately not a knob — the
/// `RecallConfig{valve:false}` precedent), a foreign mutation applies
/// without recalling the delegated holder, and the holder — whose reader
/// view still matches the grant stamp — keeps serving the PRE-mutation
/// truth: a stale dentry set, demonstrably wrong against the owner. This
/// half proves the suite can DETECT the staleness the law exists to
/// prevent, so the green half can never pass vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn red_half_without_the_law_a_delegated_holder_serves_stale() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    ship::TEST_DELEG_COHERENCE_LAW.store(false, Ordering::SeqCst);

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "staledir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "old", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;
    // Earn the delegation and prove it serves.
    fx.router.lookup(dir_ino, "old").await.expect("lookup");
    let hits0 = deleg().hits;
    fx.router.lookup(dir_ino, "old").await.expect("lookup");
    assert!(deleg().hits > hits0, "the delegation must be serving");

    // THE CONFLICTING MUTATION, law disabled: no recall reaches the holder.
    let revokes0 = lane().issued;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "fresh", FILE, 0, 0)
        .await
        .expect("owner create applies WITHOUT recalling (the law is off)");
    assert_eq!(
        lane().issued,
        revokes0,
        "control: the disabled law must issue no recall — the gate IS the mechanism"
    );

    // The owner's truth has the name...
    assert!(fx
        .owner_be
        .lookup_dentry(dir_ino, "fresh")
        .await
        .expect("owner dentry read")
        .is_some());
    // ...and the delegated holder serves the STALE answer (its view still
    // matches the grant stamp — both lag together; that is the hole).
    let err = fx
        .router
        .lookup(dir_ino, "fresh")
        .await
        .expect_err("THE STALE SERVE: the holder answers ENOENT for a name the owner has");
    assert_eq!(err.to_errno(), libc::ENOENT);
    let entries = fx.router.readdir(dir_ino, 0, 128).await.expect("readdir");
    assert!(
        !entries.iter().any(|e| e.name == "fresh"),
        "the stale readdir omits the foreign mutation too"
    );

    ship::TEST_DELEG_COHERENCE_LAW.store(true, Ordering::SeqCst);
    shutdown(&fx).await;
}

// ===========================================================================
// 5. THE COHERENCE LAW: recall-before-conflicting-publish
// ===========================================================================

/// The green half: with the law ON, the conflicting mutation's apply is
/// gated owner-side on the recall completing (acked through the rung-11
/// lane's real wire) — by the time the mutation returns, the holder has
/// surrendered the delegation, and its next serve is CURRENT (it ships,
/// owner-current). `dlm_delegation_stale_serves` stays 0 — the must-stay-0
/// coherence tripwire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_coherence_law_recalls_before_the_conflicting_mutation_applies() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "lawdir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "old", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;
    let rounds0 = deleg().channel_rounds;
    fx.router.lookup(dir_ino, "old").await.expect("lookup");
    let hits0 = deleg().hits;
    fx.router.lookup(dir_ino, "old").await.expect("lookup");
    assert!(deleg().hits > hits0, "the delegation must be serving");
    // The recall channel must be live before the conflict fires (it spun
    // up on grant absorption; the first poll parks at the owner). DELTA
    // against this fixture: the counter is process-global cumulative.
    wait_for("the recall channel's first round", || {
        deleg().channel_rounds > rounds0
    })
    .await;

    let acked0 = lane().acked;
    let recalls0 = deleg().recalls;
    // THE CONFLICTING MUTATION: an owner-local create in the delegated
    // directory. The gate recalls first; this call returning IS the proof
    // the recall completed (ack or nothing — the deadline is the dark
    // 45 s lease TTL, so an un-acked recall would hang far past the
    // suite's patience).
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "fresh", FILE, 0, 0)
        .await
        .expect("the gated create");
    assert!(
        lane().acked > acked0,
        "the recall must have been ACKED before the mutation applied \
         (recall-before-conflicting-publish)"
    );
    assert!(
        deleg().recalls > recalls0,
        "the holder processed the recall (dropped the delegation)"
    );

    // The holder's next serve is CURRENT: the entry is gone, the verb
    // ships, and the owner answers with the fresh name.
    let got = fx
        .router
        .lookup(dir_ino, "fresh")
        .await
        .expect("the post-recall serve sees the mutation");
    assert!(got.ino > 0);
    assert_eq!(
        deleg().stale_serves,
        0,
        "the must-stay-0 coherence tripwire"
    );
    shutdown(&fx).await;
}

/// The mutating holder's OWN grant dies with the mutation's reply, never
/// through a wire recall: the owner retires it as a SURRENDER (the lane's
/// additive client-return arm) and the reply carries the revocation, so a
/// serial mutate-then-lookup stream pays zero added round trips and
/// read-your-own-writes holds by construction (the reply processes the
/// revoke before the caller's await returns).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mutating_holders_own_grant_dies_with_the_reply_not_the_wire() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "owndir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "seed", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;
    fx.router.lookup(dir_ino, "seed").await.expect("lookup");
    let hits0 = deleg().hits;
    fx.router.lookup(dir_ino, "seed").await.expect("lookup");
    assert!(deleg().hits > hits0, "the delegation must be serving");

    let issued0 = lane().issued;
    let surrenders0 = lane().surrenders;
    let revokes0 = deleg().reply_revokes;
    // The holder's OWN conflicting mutation, shipped through the router.
    let created = fx
        .router
        .create_with_rdev(dir_ino, "own", FILE, 0, 0, 0)
        .await
        .expect("the holder's own create ships and applies");
    assert!(created.ino > 0);
    assert_eq!(
        lane().issued,
        issued0,
        "a self-conflict must never ride the wire recall lane"
    );
    assert!(
        lane().surrenders > surrenders0,
        "the owner retires the mutator's own grant as a SURRENDER"
    );
    assert!(
        deleg().reply_revokes > revokes0,
        "the revocation rides the mutation's own reply"
    );

    // Read-your-own-writes: the next lookup must see the create (the
    // delegation is gone, so it ships owner-current).
    let got = fx.router.lookup(dir_ino, "own").await.expect("lookup");
    assert_eq!(got.ino, created.ino);
    assert_eq!(deleg().stale_serves, 0);
    shutdown(&fx).await;
}

// ===========================================================================
// 6. Recall timeout: terminal, loud, eviction-escalated; fenced holders
//    refuse (era fencing)
// ===========================================================================

/// A holder that cannot ack inside the deadline loses the grant (the
/// rung-11 law: the grant is DEAD and the object grantable again) and the
/// timeout ESCALATES: `dlm_delegation_recall_timeouts` counts it loud, the
/// holder is fenced on the delegation plane, and — when the membership
/// plane is armed — the escalation is wired to `MembershipOwner::evict`
/// (the rung-11 residual #2 this rung discharges). A fenced holder's poll
/// AND re-assert refuse with `STATUS_DELEG_FENCED`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recall_timeout_is_terminal_loud_and_escalates_to_eviction() {
    use squeezefs::membership::{self, JoinRequest, LeaseClock, LeaseClocks, MemberRole};

    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    // A short, pinned deadline (the registered measurement lever): the
    // manual holder below never acks, and the gate must not park the
    // mutation for the dark 45 s TTL.
    std::env::set_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS", "300");

    // The membership plane, armed with the manual holder as a member —
    // the eviction-escalation target.
    let clocks = LeaseClocks::with_params(
        Duration::from_secs(45),
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .expect("clocks");
    let owner = membership::MembershipOwner::arm(
        "owner-a",
        fx.svc.term().max(1),
        fx.svc.term().max(1).saturating_sub(1),
        clocks,
        LeaseClock::monotonic(),
    )
    .expect("membership owner");
    membership::install_owner(Arc::clone(&owner));
    let join = owner.join(JoinRequest {
        id: "manual-holder".to_string(),
        role: MemberRole::Writer,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    });
    assert!(
        matches!(join, membership::JoinOutcome::Granted(_)),
        "the manual holder must be a member before it can be evicted"
    );
    assert_eq!(owner.len(), 1);

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "deaddir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    for vol in &fx.owner_be.volumes {
        vol.checkpoint_now().await.expect("checkpoint");
    }

    // The MANUAL holder: a raw wire client that earns a grant and then
    // never services its recall channel (the partitioned-holder shape,
    // driven deterministically instead of with netem).
    let mut raw = cw::RpcClient::connect(&fx.endpoint, SECRET, "manual-holder", None)
        .await
        .expect("raw client");
    let frame = MetaRequestFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 99,
        client_id: "manual-holder".to_string(),
        owner_term: 0,
        ops: vec![MetaOp {
            id: 1,
            call: MetaCall::Getattr { ino: dir_ino },
        }],
    };
    let body = ship::encode_request(&frame).expect("encode");
    let reply = raw
        .call(ship::VERB_META_BATCH, body)
        .await
        .expect("the grant-earning getattr");
    assert_eq!(reply.status, cw::RPC_OK);
    let results = ship::decode_reply(&reply.body).expect("decode").results;
    assert!(
        !results[0].delegs.is_empty(),
        "the getattr's reply must carry the delegation grant"
    );

    // THE CONFLICT: the holder never acks; the mutation must complete at
    // the deadline (grant dead), loudly, within a bounded wall.
    let timeouts0 = deleg().recall_timeouts;
    let t0 = Instant::now();
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "fresh", FILE, 0, 0)
        .await
        .expect("the gated create proceeds at the deadline");
    let waited = t0.elapsed();
    assert!(
        waited < Duration::from_secs(10),
        "the deadline bounds the gate ({waited:?})"
    );
    assert!(
        deleg().recall_timeouts > timeouts0,
        "the timeout is LOUD — dlm_delegation_recall_timeouts must count it"
    );
    // The escalation: the holder was EVICTED from the membership plane
    // (minting the S7 dead epoch — rung-11 residual #2, discharged here).
    assert_eq!(
        owner.len(),
        0,
        "the timed-out holder must be membership-EVICTED, never silently forgotten"
    );

    // Era fencing: the fenced holder's poll and re-assert refuse loud.
    let poll = DelegPollFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 99,
        client_id: "manual-holder".to_string(),
        acks: vec![],
    };
    let reply = raw
        .call(
            ship::VERB_DELEG_RECALL,
            ship::encode_deleg_poll(&poll).expect("encode"),
        )
        .await
        .expect("the poll round-trips");
    assert_eq!(
        reply.status,
        ship::STATUS_DELEG_FENCED,
        "a fenced holder's recall channel refuses"
    );
    let re = DelegReassertFrame {
        schema: ship::META_SHIP_SCHEMA,
        client_epoch: 99,
        client_id: "manual-holder".to_string(),
        inos: vec![dir_ino],
    };
    let reply = raw
        .call(
            ship::VERB_DELEG_REASSERT,
            ship::encode_deleg_reassert(&re).expect("encode"),
        )
        .await
        .expect("the reassert round-trips");
    assert_eq!(
        reply.status,
        ship::STATUS_DELEG_FENCED,
        "a fenced holder's re-assert refuses — its grants died with the eviction"
    );

    membership::uninstall();
    std::env::remove_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS");
    shutdown(&fx).await;
}

// ===========================================================================
// 7. Grace re-assertion across an authority restart
// ===========================================================================

/// Delegations are RAM (KD-MW-5): across an authority restart the holder
/// re-asserts in the successor's grace window and keeps serving — while
/// the window refuses conflicting fresh mutations (the S6/S8 grace law)
/// and zero stale serves happen at any point. Un-reasserted grants are
/// simply gone (the NFSv4 law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grace_reassertion_across_an_authority_restart() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "gracedir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "seed", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;
    let rounds0 = deleg().channel_rounds;
    fx.router.lookup(dir_ino, "seed").await.expect("lookup");
    let hits0 = deleg().hits;
    fx.router.lookup(dir_ino, "seed").await.expect("lookup");
    assert!(deleg().hits > hits0, "the delegation must be serving");
    wait_for("the recall channel's first round", || {
        deleg().channel_rounds > rounds0
    })
    .await;

    // THE RESTART: the authority dies and a successor arms on the SAME
    // endpoint with a bumped era and an open grace window.
    let port = fx.listener.endpoint().port();
    fx.listener.shutdown();
    let svc2 = MetaShipService::new(Arc::clone(&fx.owner_be));
    svc2.bump_term(fx.svc.term() + 1);
    svc2.open_grace(Duration::from_secs(30));
    ship::install_delegation_host(Arc::clone(&svc2));
    let mut listener2 = None;
    for _ in 0..100 {
        match cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: format!("127.0.0.1:{port}").parse().expect("addr"),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            svc2.clone(),
        ) {
            Ok(l) => {
                listener2 = Some(l);
                break;
            }
            Err(_) => squeezefs_ipc::sqz_time::sleep(Duration::from_millis(50)).await,
        }
    }
    let listener2 = listener2.expect("the successor rebinds the endpoint");

    // The holder's channel reconnects and RE-ASSERTS its delegations in
    // the successor's grace window (fresh-era grants, fresh stamps).
    let reasserts0 = deleg().reasserts;
    wait_for("the holder's re-assertion", || {
        deleg().reasserts > reasserts0
    })
    .await;

    // In-window: a conflicting FRESH mutation refuses (the grace law).
    // The FIRST attempt may meet the ERA gate instead (the client still
    // believes the predecessor's term; the refusal teaches it — "learn
    // the new era so the caller's retry is admissible"), so the grace
    // refusal is asserted on the post-relearn retry.
    let mut grace_refusal = None;
    for _ in 0..3 {
        let err = fx
            .router
            .create_with_rdev(dir_ino, "conflict", FILE, 0, 0, 0)
            .await
            .expect_err("a fresh mutation must refuse inside the grace window");
        let msg = format!("{err}");
        if msg.contains("stale writer era") {
            continue; // the era relearn — retry now names the successor
        }
        grace_refusal = Some(msg);
        break;
    }
    let msg = grace_refusal.expect("every attempt met the era gate — the relearn never landed");
    assert!(msg.contains("grace"), "the refusal names the window: {msg}");

    // The re-asserted delegation serves — current, zero stale serves.
    let hits1 = deleg().hits;
    let got = fx.router.lookup(dir_ino, "seed").await.expect("lookup");
    assert!(got.ino > 0);
    assert!(
        deleg().hits > hits1,
        "the re-asserted delegation must serve under the successor era"
    );
    assert_eq!(deleg().stale_serves, 0);

    // Window closed: mutations flow again, under the coherence law.
    svc2.close_grace();
    let acked0 = lane().acked;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "fresh", FILE, 0, 0)
        .await
        .expect("post-grace create");
    assert!(
        lane().acked > acked0,
        "the post-restart coherence law recalls through the re-asserted grant"
    );
    let got = fx
        .router
        .lookup(dir_ino, "fresh")
        .await
        .expect("current serve after the post-grace mutation");
    assert!(got.ino > 0);
    assert_eq!(deleg().stale_serves, 0);

    listener2.shutdown();
    for vol in &fx.owner_be.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

// ===========================================================================
// 8. Valve composition: the rung-11 brake through the REAL wire
// ===========================================================================

/// A hot delegated object cycling grant → recall demotes to owner-served
/// through the REAL wire (grants earned by shipped verbs, recalls
/// delivered on the recall channel, acks from the live holder): the
/// rung-11 storm extended end-to-end. After `RECALL_THRASH_CYCLES` the
/// object demotes — grants stop riding replies — and the recall volume
/// flattens (bounded by cycles × holders, never by the storm's length).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn valve_composition_a_hot_delegated_object_demotes_through_the_real_wire() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;
    // Pin the deadline high so the thrash WINDOW (= the deadline) covers
    // the storm's real wall-clock rounds (the lane derives window from
    // deadline; live in-process evidence would shrink it below a round).
    std::env::set_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS", "60000");

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "thrashdir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "seed", FILE, 0, 0)
        .await
        .expect("owner create");
    catch_up(&fx).await;

    let demotions0 = lane().thrash_demotions;
    let issued0 = lane().issued;
    let mut demoted_at = None;
    for round in 0..(ship::RECALL_THRASH_CYCLES as usize + 4) {
        // The holder re-earns the delegation (grant → the thrash slot's
        // grant-after-recall evidence)...
        fx.router
            .getattr(dir_ino)
            .await
            .expect("grant-earning getattr");
        // ...and a conflicting owner-local mutation recalls it.
        Metadata::create(
            fx.owner_be.as_ref(),
            dir_ino,
            &format!("f{round}"),
            FILE,
            0,
            0,
        )
        .await
        .expect("the recalling create");
        if lane().thrash_demotions > demotions0 {
            demoted_at = Some(round);
            break;
        }
    }
    let demoted_at = demoted_at.expect(
        "the valve must engage before the fan-out hurts (spec R5): no \
         demotion after thrash_cycles + slack grant→recall rounds",
    );
    assert!(
        demoted_at + 1 >= ship::RECALL_THRASH_CYCLES as usize,
        "one cycle is never a verdict (demoted after round {demoted_at})"
    );
    let issued_at_demotion = lane().issued - issued0;

    // Demoted: the object is owner-served — grants stop riding replies,
    // and the storm's continuation adds NO recall volume.
    let installs0 = deleg().installs;
    let refusals0 = lane().grant_refusals;
    for round in 0..6 {
        fx.router
            .getattr(dir_ino)
            .await
            .expect("owner-served getattr while demoted");
        Metadata::create(
            fx.owner_be.as_ref(),
            dir_ino,
            &format!("g{round}"),
            FILE,
            0,
            0,
        )
        .await
        .expect("un-gated create while demoted");
    }
    assert_eq!(
        deleg().installs,
        installs0,
        "a demoted object must earn NO grants for the cooldown"
    );
    assert!(
        lane().grant_refusals > refusals0,
        "the refusals are counted (the valve's refusal gauge)"
    );
    let flat = lane().issued - issued0;
    assert_eq!(
        flat, issued_at_demotion,
        "the recall volume must FLATTEN at demotion (was {issued_at_demotion}, grew to {flat})"
    );
    assert_eq!(deleg().stale_serves, 0);

    std::env::remove_var("SQUEEZEFS_DLM_RECALL_DEADLINE_MS");
    shutdown(&fx).await;
}

// ===========================================================================
// 9. Reader-TTL stretch (the kernel face) + solo/export pins
// ===========================================================================

/// The design's TTL-stretch clause, attr face: a live, channel-fresh,
/// view-current delegation stretches the object's kernel attr TTL past
/// the per-class default (the S5 machinery's per-class TTLs stay the
/// floor), and the RECALL is what bounds the stretched staleness — the
/// drop pushes a kernel invalidation through the installed sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kernel_ttl_stretch_rides_delegations_and_recall_invalidates() {
    let _plane = PLANE.lock().await;
    let fx = fixture().await;

    let invalidated: Arc<parking_lot::Mutex<Vec<u64>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink_log = Arc::clone(&invalidated);
    ship::set_deleg_inval_sink(Some(Arc::new(move |ino| {
        sink_log.lock().push(ino);
    })));

    let dir_ino = Metadata::create(fx.owner_be.as_ref(), 1, "ttldir", DIR, 0, 0)
        .await
        .expect("owner mkdir")
        .ino;
    catch_up(&fx).await;
    assert_eq!(
        ship::deleg_kernel_ttl_stretch(dir_ino),
        None,
        "no delegation, no stretch"
    );
    let rounds0 = deleg().channel_rounds;
    fx.router
        .getattr(dir_ino)
        .await
        .expect("grant-earning getattr");
    wait_for("the recall channel's first round", || {
        deleg().channel_rounds > rounds0
    })
    .await;
    let stretch = ship::deleg_kernel_ttl_stretch(dir_ino)
        .expect("a live delegation stretches the kernel TTL");
    assert!(
        stretch >= Duration::from_secs(1),
        "the stretch must exceed the 1 s per-class default it relaxes (got {stretch:?})"
    );

    // The recall bounds the stretched staleness: the drop invalidates.
    Metadata::create(fx.owner_be.as_ref(), dir_ino, "conflict", FILE, 0, 0)
        .await
        .expect("the recalling create");
    assert_eq!(
        ship::deleg_kernel_ttl_stretch(dir_ino),
        None,
        "a recalled delegation stretches nothing"
    );
    assert!(
        invalidated.lock().contains(&dir_ino),
        "the recall must push the kernel invalidation for the stretched object"
    );

    ship::set_deleg_inval_sink(None);
    shutdown(&fx).await;
}

/// The solo re-gate's face: with nothing armed and nothing installed,
/// owner-side mutations never touch the delegation plane or the recall
/// lane, and the stats surface exports zeros. One relaxed load is the
/// whole solo cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_solo_posture_pays_nothing_and_exports_zeros() {
    let _plane = PLANE.lock().await;
    // Deliberately NO fixture: nothing armed, no host installed.
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = plan_meta_slot_set(1).expect("plan");
    let p = make_file(dir.path(), "solo-meta", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format");
    let be = open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("solo set");

    let l0 = lane();
    let d0 = deleg();
    let ino = Metadata::create(be.as_ref(), 1, "d", DIR, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    Metadata::create(be.as_ref(), ino, "f", FILE, 0, 0)
        .await
        .expect("create");
    be.unlink(ino, "f").await.expect("unlink");
    let l1 = lane();
    let d1 = deleg();
    assert_eq!(l1.issued, l0.issued, "solo mutations never recall");
    assert_eq!(l1.grants, l0.grants, "solo mutations never grant");
    assert_eq!(d1.grants, d0.grants);
    assert_eq!(d1.recall_timeouts, d0.recall_timeouts);
    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// The `dlm_delegation` stats family exports through the process surface
/// the stats inode renders (the rung-11 `dlm_recall` precedent): every
/// design-named field present, zero on an untouched process.
#[test]
fn the_delegation_stats_family_exports_the_design_named_fields() {
    let json = ship::delegation_stats_json();
    for field in [
        "dlm_delegation_grants",
        "dlm_delegation_hits",
        "dlm_delegation_recalls",
        "dlm_delegation_reasserts",
        "dlm_delegation_entries",
        "dlm_delegation_bytes",
        "dlm_delegation_stale_serves",
        "dlm_delegation_recall_timeouts",
    ] {
        assert!(
            json.get(field).is_some(),
            "design §13 names {field} — the family must export it"
        );
    }
    let phases = ship::delegation_recall_phase_json();
    assert!(
        phases.get("gate_wait").is_some() && phases.get("holder_drain").is_some(),
        "dlm_delegation_recall_phase_ns carries the gate/drain decomposition"
    );
}
