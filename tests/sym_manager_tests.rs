//! Symmetric metadata program, PR 3 — **the manager lease**
//! (`docs/design-symmetric-metadata.md` §5.3.3 extents, §5.3.5
//! idempotent verbs, §5.8.1 WERO on metadata namespaces, §5.9 the
//! successor, §6.3 the `ManagerCall` wire, §11 the Manager family;
//! KD-SYM-3/7/13).
//!
//! Under bit 17 the writer that wins the D0 ladder IS the volume's
//! manager: it grants extents to every other appender (a run carved from
//! the free heap, journaled as `extent_grant:{appender}` in tree 0 and
//! recorded on the appender's page), takes them back in `ReturnExtents`
//! batches, and answers `JoinAppender`. With a grant a declared region's
//! interior flips and allocator deltas journal into ITS OWN ring — PR 2's
//! deviation 3 ("structure stays the manager's ring") reversed, with PR
//! 2's `Lease` / `Extent` violation classes as the tripwires.
//!
//! **Bit 17 stays DARK**: the only stamp is the PR-1 seam, and a
//! bit-17-absent volume is byte-for-byte the shipped format — every
//! Manager-family gauge is absent there.

use squeezefs::meta_backend::kv::appender::{
    grant_extents_derived, manager_failover_bound_ms, manager_should_release_role, read_directory,
    resolve_grant_extents, test_set_manager_unreachable, AppenderStats, GrantRun, ManagerLease,
    RegionGrant, GRANT_EXTENTS_FLOOR, GRANT_EXTENTS_MAX, GRANT_RUNS_MAX, SYM_GRANT_EXTENTS_ENV,
    TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{
    digest_backend, format_v3_stamped, FormatV3Options, ROOT_INO,
};
use squeezefs::meta_backend::kv::slot_state::{
    decode_extent_grant_key, extent_grant_key, ExtentGrantRecord, EXTENT_GRANT_KEY_LEN,
};
use squeezefs::meta_backend::kv::{
    META_KV_NODE_COMPACTIONS, META_KV_NODE_SPLITS, META_KV_REPLAY_EXTENT_VIOLATIONS,
    META_KV_REPLAY_KEY_VIOLATIONS, META_KV_REPLAY_LEASE_VIOLATIONS,
};
use squeezefs::meta_backend::{
    guest_local_ino, open_routed_meta_set, open_volume_for_mount, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness (the sym_appender_tests shape: 64 KiB nodes, a 1 MiB fixed ring;
// the declared partition `1:4` — appender 1 leases forest slot 4).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const PARTITION: &str = "1:4";

/// The seam is process-global; format calls and knob writes serialize.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    format_stamped_member_sized(dir, name, VOL_LEN).await
}

async fn format_stamped_member_sized(dir: &std::path::Path, name: &str, len: u64) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, len, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

async fn format_flat_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format flat member");
    p.display().to_string()
}

/// Open with the declared partition in force (KD-SYM-13: a file-backed
/// volume is a non-PR substrate, so the symmetric arm needs the loud
/// opt-in; the env is read at the writer's open and cleared after).
async fn open_with_partition(uris: &[String], partition: Option<&str>) -> Arc<RoutedMetaBackend> {
    match partition {
        Some(p) => {
            std::env::set_var(TEST_APPENDER_SLOTS_ENV, p);
            std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
        }
        None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
    }
    let r = open_routed_meta_set(uris).await;
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    r.expect("open routed set")
}

fn stats(vol: &KvMetaBackend) -> AppenderStats {
    vol.appender_stats()
        .expect("a forest volume has an appender set")
}

/// `n` block references of `owner` on volume tag `tag`, blocks
/// `base..base+n`.
fn refs(tag: u64, owner: u64, base: u64, n: u64) -> Vec<BlockRefOp> {
    (0..n)
        .map(|i| {
            BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: base + i,
                owner_ino: owner,
                block_index: i as u32,
            })
        })
        .collect()
}

/// The grant closure law (§11): `granted ≡ claimed + returned + unclaimed`.
fn assert_grant_closure(s: &AppenderStats) {
    assert_eq!(
        s.grant_granted,
        s.grant_claimed + s.grant_returned + s.grant_unclaimed,
        "extent_grant_extents ≡ claimed + returned + granted_unclaimed: {s:?}"
    );
}

// ---------------------------------------------------------------------------
// §5.3.3 — the extent grant: record codec, derivation, the RAM grant.
// ---------------------------------------------------------------------------

#[test]
fn the_extent_grant_record_round_trips_and_is_normalized() {
    let key = extent_grant_key(7);
    assert_eq!(key.len(), EXTENT_GRANT_KEY_LEN);
    assert_eq!(decode_extent_grant_key(&key).unwrap(), 7);
    assert!(decode_extent_grant_key(b"slot_state:xxxx").is_err());
    // Unsorted, duplicated, adjacent extents coalesce into normalized runs.
    let rec = ExtentGrantRecord::from_extents([10u64, 12, 11, 11, 20, 21, 30]);
    assert_eq!(
        rec.runs,
        vec![
            GrantRun { start: 10, len: 3 },
            GrantRun { start: 20, len: 2 },
            GrantRun { start: 30, len: 1 },
        ]
    );
    assert_eq!(rec.len(), 6);
    assert!(rec.contains(12) && !rec.contains(13) && rec.contains(30));
    let img = rec.encode().unwrap();
    assert_eq!(ExtentGrantRecord::decode(&img).unwrap(), rec);
    // Total decode: empty, bad version, truncated, an empty run, an
    // un-normalized pair.
    assert!(ExtentGrantRecord::decode(&[]).is_err());
    assert!(ExtentGrantRecord::decode(&[9, 0, 0]).is_err());
    assert!(ExtentGrantRecord::decode(&img[..img.len() - 1]).is_err());
    let mut empty_run = img.clone();
    empty_run[3 + 8..3 + 12].copy_from_slice(&0u32.to_le_bytes());
    assert!(ExtentGrantRecord::decode(&empty_run).is_err());
    let adjacent = ExtentGrantRecord {
        runs: vec![
            GrantRun { start: 10, len: 2 },
            GrantRun { start: 12, len: 1 },
        ],
    };
    assert!(ExtentGrantRecord::decode(&adjacent.encode().unwrap()).is_err());
    assert!(ExtentGrantRecord::default().is_empty());
}

#[test]
fn grant_size_derivation_floor_cap_and_the_knob_tie() {
    assert_eq!(
        GRANT_EXTENTS_FLOOR, 8,
        "2 extents per SMO × 4 pending swaps per cycle"
    );
    assert_eq!(GRANT_EXTENTS_MAX, 65_536);
    // No measured SMO rate ⇒ the floor.
    assert_eq!(
        grant_extents_derived(0, 46_500, 1_000_000, 195),
        GRANT_EXTENTS_FLOOR
    );
    // 5 SMO/s over a 46.5 s bound, doubled = 465 extents.
    assert_eq!(grant_extents_derived(5_000, 46_500, 1_000_000, 1), 465);
    // The cap: a quarter of the free heap over the appenders.
    assert_eq!(grant_extents_derived(5_000, 46_500, 1_000, 10), 1_000 / 40);
    // The cap never falls below the floor.
    assert_eq!(
        grant_extents_derived(5_000, 46_500, 4, 10),
        GRANT_EXTENTS_FLOOR
    );
    // The knob wins verbatim.
    std::env::set_var(SYM_GRANT_EXTENTS_ENV, "64");
    assert_eq!(resolve_grant_extents(0, 46_500, 1_000_000, 1), 64);
    std::env::remove_var(SYM_GRANT_EXTENTS_ENV);
    assert_eq!(
        resolve_grant_extents(0, 46_500, 1_000_000, 1),
        GRANT_EXTENTS_FLOOR
    );
    // The registry: an int knob over floor..=max.
    let k = squeezefs::env_knobs::lookup(SYM_GRANT_EXTENTS_ENV).expect("registered");
    match k.kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!(lo, GRANT_EXTENTS_FLOOR as i128);
            assert_eq!(hi, GRANT_EXTENTS_MAX as i128);
        }
        other => panic!("SQUEEZEFS_SYM_GRANT_EXTENTS must be an int knob, got {other:?}"),
    }
    // The failover bound: TTL + the ladder's wall + the replay's wall.
    assert_eq!(manager_failover_bound_ms(45, 1_000, 500), 46_500);
    // The vol-0 rule fires strictly past T_owner.
    assert!(!manager_should_release_role(45_000, 45_000));
    assert!(manager_should_release_role(45_001, 45_000));
}

#[test]
fn the_ram_grant_claims_lowest_first_parks_frees_on_its_tail_and_returns_past_it() {
    let mut g = RegionGrant::default();
    g.add_runs(&[GrantRun { start: 100, len: 4 }]);
    assert_eq!((g.unclaimed(), g.claimed(), g.granted), (4, 0, 4));
    assert!(!g.refill_due(), "a fresh grant is not due");
    assert_eq!(g.claim(), Some(100));
    assert_eq!(g.claim(), Some(101));
    assert!(g.refill_due(), "half consumed ⇒ due (the ahead-refill law)");
    g.release_unpublished(101);
    assert_eq!(g.claim(), Some(101), "an abandoned claim is re-claimable");
    // A free parks on the gate; the tail releases it into the return batch.
    g.free_pending(100, 500);
    assert_eq!((g.claimed(), g.pending()), (1, 1));
    assert_eq!(g.advance_durable(499), 0);
    assert_eq!(g.advance_durable(500), 1);
    assert_eq!(g.take_returnable(), vec![100]);
    assert_eq!(g.returned, 1);
    // The page names the unclaimed remainder as runs, ≤ GRANT_RUNS_MAX.
    assert_eq!(g.unclaimed_runs(), vec![GrantRun { start: 102, len: 2 }]);
    g.add_runs(&[GrantRun { start: 200, len: 1 }]);
    g.add_runs(&[GrantRun { start: 300, len: 1 }]);
    g.add_runs(&[GrantRun { start: 400, len: 1 }]);
    g.add_runs(&[GrantRun { start: 500, len: 1 }]);
    assert_eq!(g.unclaimed_runs().len(), GRANT_RUNS_MAX);
    // Closure over every op so far: granted ≡ held + returned + unclaimed.
    assert_eq!(g.granted, g.held() + g.returned + g.unclaimed());
    // Recovery: the record's whole grant against the page's remainder.
    let r = RegionGrant::recover([10u64, 11, 12, 13], &[GrantRun { start: 12, len: 2 }]);
    assert_eq!((r.unclaimed(), r.claimed()), (2, 2));
    assert!(r.contains(10) && r.contains(13));
    let mut r = r;
    r.claim_exact(12);
    r.free_exact(10, 77);
    assert_eq!((r.unclaimed(), r.claimed(), r.pending()), (1, 2, 1));
}

// ---------------------------------------------------------------------------
// The mounted manager: the join's grant, own-ring SMOs, returns, the stall.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_region_joins_with_a_grant_in_tree_zero_and_on_its_page() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let s = stats(&vol);
    assert_eq!(
        s.manager_lease,
        ManagerLease::Held,
        "the D0 winner IS the manager"
    );
    assert_eq!(s.extent_grants, 1, "the join grants once");
    assert_eq!(
        s.extent_grant_extents, GRANT_EXTENTS_FLOOR,
        "no measured rate ⇒ the floor"
    );
    assert_eq!(s.regions[1].grant_unclaimed, GRANT_EXTENTS_FLOOR);
    assert_grant_closure(&s);
    assert!(
        s.failover_bound_ms >= 45_000,
        "the failover bound carries the 45 s TTL: {}",
        s.failover_bound_ms
    );
    // Tree 0 attests the whole grant; the page names the unclaimed
    // remainder — the same runs at the join.
    let record = vol.extent_grant_record(1).await.unwrap();
    assert_eq!(record.len(), GRANT_EXTENTS_FLOOR);
    let sb = vol.superblock().clone();
    let page = read_directory(path, &sb).await.unwrap()[1]
        .page
        .clone()
        .expect("appender 1's page");
    assert_eq!(
        page.grant, record.runs,
        "page remainder = the whole grant, unclaimed"
    );
    // The manager takes no grant: appender 0 refuses (manager_verb_refusals).
    assert!(vol.manager_extent_grant(0, 1).await.is_err());
    assert_eq!(stats(&vol).manager_verb_refusals, 1);
    // Every granted extent is ALLOCATED in the bitmap — the grant's
    // carve rode ring 0 as allocator deltas.
    for e in record.extents() {
        assert!(
            vol.allocator().is_allocated(e),
            "granted extent {e} is claimed"
        );
    }
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    // The leave returned the unclaimed grant: the record is gone.
    let probe = KvMetaBackend::open_probe(path).await.unwrap();
    assert!(
        probe.extent_grant_record(1).await.unwrap().is_empty(),
        "a released region's unclaimed grant returns to the heap (§5.1.3)"
    );
}

/// Deviation 3 reversed: with a grant, a declared region's SMOs — the
/// interior flips and the allocator deltas of its slot tree — journal into
/// ITS OWN ring and claim inside its grant; ring 0 carries none of them,
/// and the remount replays them from that ring with the `Lease` / `Extent`
/// tripwires at 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_regions_smos_journal_into_its_own_ring_and_claim_inside_its_grant() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 77); // forest slot 4 — appender 1's
                                              // The cadence parked: every SMO below is driven by `checkpoint_now`.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    let ring0_before = stats(&va).regions[0].ring_entries;
    let smos_before = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed);
    // Enough same-slot content to fill the slot tree's leaf log several
    // times over: each checkpoint's flush folds it — compactions and
    // splits, every one an SMO of appender 1's tree.
    const ROUNDS: u64 = 10;
    for round in 0..ROUNDS {
        for i in 0..3u64 {
            va.commit_block_refs(
                guest_owner,
                &refs(tag, guest_owner, round * 10_000 + i * 600, 400),
            )
            .await
            .unwrap();
        }
        va.checkpoint_now().await.unwrap();
    }
    let smos = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed)
        - smos_before;
    assert!(smos > 0, "the storm must drive SMOs of appender 1's tree");
    let s = stats(&va);
    assert!(
        s.regions[1].grant_claimed >= 1,
        "the SMO images are claimed INSIDE the grant: {:?}",
        s.regions[1]
    );
    assert!(
        s.extent_grants >= 2,
        "consumption past 50 % refilled the grant at the cadence: {s:?}"
    );
    assert_grant_closure(&s);
    // Ring 0 saw only the manager's own work (the grant/return control
    // entries, the root publications) — never appender 1's flips or
    // deltas, which ride appender 1's ring.
    let ring0_grew = s.regions[0].ring_entries - ring0_before;
    assert!(
        ring0_grew <= s.extent_grants + s.extent_returns + ROUNDS + 2,
        "ring 0 grew by {ring0_grew} entries — more than the manager's control entries and \
         publications ({} grants, {} returns, {ROUNDS} cycles): appender 1's structure leaked \
         into the manager's ring",
        s.extent_grants,
        s.extent_returns
    );
    assert_eq!(s.dependency_stalls, 0, "the sized grant never stalled");
    let live = digest_backend(&va).await.unwrap();
    va.sync_device().await.unwrap();
    drop(va);
    drop(ra);
    // Crash-equivalent remount: appender 1's ring replays its own interior
    // flips and allocator deltas (phase 1) and content (phase 2) — no
    // Lease, no Extent, no Key violation — to the same digest, with the
    // grant's claimed set recovered from tree 0's record and the page.
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = &ra.volumes[0];
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(META_KV_REPLAY_EXTENT_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(digest_backend(va).await.unwrap(), live);
    let s = stats(va);
    assert_eq!(s.self_recoveries, 2);
    assert!(s.regions[1].grant_claimed >= 1, "{:?}", s.regions[1]);
    assert_grant_closure(&s);
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// R7 / §5.3.3 the stall bound: an appender that consumes its whole grant
/// while the manager cannot refill defers its compactions (the records
/// stay in its ring — never lost, never `ENOSPC`) and counts
/// `manager_dependency_stalls`; a user commit that needs the extent
/// refuses EAGAIN-class; once the manager answers again the refill lands
/// and the deferred SMOs run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_grant_with_the_manager_unreachable_stalls_loud_and_never_loses_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 78);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    test_set_manager_unreachable(true);
    let mut refused_eagain = 0u64;
    for round in 0..16u64 {
        for i in 0..3u64 {
            match va
                .commit_block_refs(
                    guest_owner,
                    &refs(tag, guest_owner, round * 10_000 + i * 600, 400),
                )
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("grant is exhausted"),
                        "the only refusal is the grant's EAGAIN class: {msg}"
                    );
                    refused_eagain += 1;
                }
            }
        }
        va.checkpoint_now().await.unwrap();
    }
    let s = stats(&va);
    assert!(
        s.dependency_stalls > 0,
        "the floor-sized grant ran out under the storm with no refill: {s:?}"
    );
    assert_eq!(
        s.extent_grants, 1,
        "the unreachable manager granted nothing past the join's grant"
    );
    assert_eq!(
        va.enospc_refusals(),
        0,
        "a grant stall is never the heap's space class"
    );
    assert!(
        !va.heap_full(),
        "the heap is not full — the appender is out of grant"
    );
    // Every ACKED record is readable: the deferred compactions left them
    // in the ring / the leaf's overlay.
    assert!(va.block_ref_count(tag, 0).await.unwrap() >= 1);
    let live = digest_backend(&va).await.unwrap();
    // The manager answers again: the next cycle refills and the deferred
    // SMOs run; the stall counter stops moving.
    test_set_manager_unreachable(false);
    va.checkpoint_now().await.unwrap();
    let stalls_at_recovery = stats(&va).dependency_stalls;
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let s = stats(&va);
    assert!(s.extent_grants >= 2, "the refill landed: {s:?}");
    assert_eq!(
        s.dependency_stalls, stalls_at_recovery,
        "no stall after the refill"
    );
    assert_eq!(
        digest_backend(&va).await.unwrap(),
        live,
        "the stall lost nothing"
    );
    assert_grant_closure(&s);
    log::info!(
        "stall row: {refused_eagain} EAGAIN refusals, {} stalls, bound {} ms",
        s.dependency_stalls,
        s.failover_bound_ms
    );
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// `ReturnExtents` is idempotent against the bitmap and tree 0: a second
/// return of the same extents clears nothing and answers `already`
/// (`manager_verb_replays`), never a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_return_replayed_against_durable_state_answers_already() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let record = vol.extent_grant_record(1).await.unwrap();
    let first: Vec<u64> = record.extents().take(2).collect();
    let free_before = vol.free_extents();
    let (cleared, already) = vol.manager_return_extents(1, &first).await.unwrap();
    assert_eq!((cleared, already), (2, 0));
    assert_eq!(vol.free_extents(), free_before + 2);
    let (cleared, already) = vol.manager_return_extents(1, &first).await.unwrap();
    assert_eq!((cleared, already), (0, 2), "the replay is a durable no-op");
    assert_eq!(vol.free_extents(), free_before + 2);
    let s = stats(&vol);
    assert_eq!(s.manager_verb_replays, 1);
    assert_eq!(s.manager_verb_refusals, 0);
    assert_eq!(s.extent_returns, 1);
    assert_eq!(
        vol.extent_grant_record(1).await.unwrap().len(),
        GRANT_EXTENTS_FLOOR - 2,
        "the record dropped the returned extents"
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// §6.3 / §5.3.5 — the ManagerCall wire: JoinAppender over the S8 wire,
// idempotency against DURABLE state, the directory chain's growth, the
// join storm, the service phases.
// ---------------------------------------------------------------------------

use squeezefs::cluster_wire as cw;
use squeezefs::meta_backend::kv::appender::{
    dir_pairs_per_extent, read_directory_chain, AppenderIdentity, AppenderState,
};
use squeezefs::meta_ship::manager::{
    decode_reply, decode_request, encode_reply, encode_request, joined_segments, ManagerCall,
    ManagerClient, ManagerReply, ManagerReplyFrame, ManagerRequestFrame, ManagerService,
    WireIdentity, MANAGER_SCHEMA, VERB_MANAGER_CALL,
};

const SECRET: &[u8] = b"sym-manager-tests-enroll-secret";

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

fn joiner_identity(n: u64) -> AppenderIdentity {
    AppenderIdentity {
        node_token: 0x5EED_0000_0000_0000 | n,
        mount_slot: 0x1000 + n as u32,
        writer_id: 0xABCD_0000 + u128::from(n),
    }
}

#[test]
fn the_manager_call_frames_round_trip_and_the_wire_schema_is_five() {
    assert_eq!(
        cw::CLUSTER_WIRE_SCHEMA,
        5,
        "bumped ONCE for the program's wire"
    );
    assert_eq!(VERB_MANAGER_CALL, 0x0500, "its own verb block");
    let req = ManagerRequestFrame {
        schema: MANAGER_SCHEMA,
        request_id: 7,
        call: ManagerCall::JoinAppender {
            identity: WireIdentity {
                node_token: 1,
                mount_slot: 2,
                writer_id: 3,
            },
            ring_want_bytes: 512 * 1024,
        },
    };
    let bytes = encode_request(&req).unwrap();
    assert_eq!(decode_request(&bytes).unwrap(), req);
    for call in [
        ManagerCall::ExtentGrant {
            appender_id: 3,
            want: 8,
        },
        ManagerCall::ReturnExtents {
            appender_id: 3,
            runs: vec![(100, 4), (200, 1)],
        },
    ] {
        let f = ManagerRequestFrame {
            schema: MANAGER_SCHEMA,
            request_id: 1,
            call,
        };
        assert_eq!(decode_request(&encode_request(&f).unwrap()).unwrap(), f);
    }
    let rep = ManagerReplyFrame {
        schema: MANAGER_SCHEMA,
        request_id: 7,
        reply: ManagerReply::Joined {
            appender_id: 1,
            page_addr: 0x1000,
            ring_segments: vec![(0x2000, 0x10000)],
            grant: vec![(50, 8)],
            already: false,
        },
    };
    let bytes = encode_reply(&rep).unwrap();
    assert_eq!(decode_reply(&bytes).unwrap(), rep);
    assert_eq!(
        joined_segments(&rep.reply),
        vec![squeezefs::meta_backend::kv::superblock::ExtentRef {
            start: 0x2000,
            len: 0x10000
        }]
    );
    // Total decode: garbage never panics.
    assert!(decode_request(&[0xFF; 40]).is_err());
    assert!(decode_reply(&[]).is_err());
}

/// `JoinAppender` over the S8 wire: the manager allocates the lowest Free
/// page, a ring from the heap, an initial grant; the joiner's page reads
/// Live under its identity in the directory; a repeated join answers
/// `already` from the PAGE (KD-SYM-7 — the durable witness), not a RAM
/// window; the service phases are recorded exact-sum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_appender_over_the_wire_allocates_a_page_ring_and_grant_and_replays_already() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .expect("manager listener");
    let endpoint = host.endpoint().to_string();
    let mut client = ManagerClient::connect(&endpoint, SECRET, "joiner-a")
        .await
        .expect("storage-trust enrollment");
    let me = joiner_identity(1);
    let free_before = vol.free_extents();
    let reply = client.join(me, 0).await.unwrap();
    let (id, page_addr, grant) = match &reply {
        ManagerReply::Joined {
            appender_id,
            page_addr,
            grant,
            already,
            ..
        } => {
            assert!(!already, "a first join");
            (*appender_id, *page_addr, grant.clone())
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        id, 2,
        "appender 1 is the declared region; the join takes the next Free page"
    );
    let segments = joined_segments(&reply);
    assert!(!segments.is_empty() && segments.len() <= 8);
    let ring_bytes: u64 = segments.iter().map(|s| s.len).sum();
    assert!(ring_bytes >= squeezefs::meta_backend::kv::appender::SYM_RING_FLOOR_BYTES);
    assert_eq!(
        grant.iter().map(|(_, l)| u64::from(*l)).sum::<u64>(),
        GRANT_EXTENTS_FLOOR
    );
    // The heap paid for the ring and the grant.
    let ring_extents = ring_bytes / NODE_SIZE as u64;
    assert_eq!(
        vol.free_extents(),
        free_before - ring_extents - GRANT_EXTENTS_FLOOR
    );
    // The directory: page 2 Live under the joiner, its ring and grant named.
    let sb = vol.superblock().clone();
    let entries = read_directory(path, &sb).await.unwrap();
    assert_eq!(entries[2].dir_offsets[0], page_addr);
    let page = entries[2].page.clone().expect("the joiner's page");
    assert_eq!(page.state, AppenderState::Live);
    assert_eq!(page.identity, me);
    assert!(!page.is_manager);
    assert_eq!(page.term, 1);
    assert_eq!(page.segments, segments);
    assert_eq!(
        page.grant
            .iter()
            .map(|r| (r.start, r.len))
            .collect::<Vec<_>>(),
        grant
    );
    assert_eq!(
        vol.extent_grant_record(2).await.unwrap().runs.len(),
        grant.len()
    );
    // The replay: the same identity ⇒ `already`, the same page and ring.
    let again = client.join(me, 0).await.unwrap();
    match &again {
        ManagerReply::Joined {
            appender_id,
            already,
            page_addr: p2,
            ..
        } => {
            assert!(already, "KD-SYM-7: the Live page IS the witness");
            assert_eq!((*appender_id, *p2), (id, page_addr));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(joined_segments(&again), segments);
    let s = stats(&vol);
    assert_eq!(s.manager_verbs, 2, "two verbs served over the wire");
    assert_eq!(s.manager_verb_replays, 1);
    assert_eq!(s.manager_verb_refusals, 0);
    assert_eq!(
        s.manager_service_ns[3],
        s.manager_service_ns[0] + s.manager_service_ns[1] + s.manager_service_ns[2],
        "admit + execute + reply ≡ total"
    );
    assert!(s.manager_service_ns[1] > 0);
    // ExtentGrant / ReturnExtents for the wire joiner: granted, returned,
    // returned again ⇒ already.
    let more = client.extent_grant(2, 4).await.unwrap();
    assert_eq!(more.iter().map(|r| u64::from(r.len)).sum::<u64>(), 4);
    let (cleared, already) = client.return_extents(2, &more).await.unwrap();
    assert_eq!((cleared, already), (4, 0));
    let (cleared, already) = client.return_extents(2, &more).await.unwrap();
    assert_eq!((cleared, already), (0, 4));
    // The manager takes no grant over the wire either.
    assert!(client.extent_grant(0, 1).await.is_err());
    assert_eq!(stats(&vol).manager_verb_refusals, 1);
    host.shutdown();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Idempotency is against DURABLE state, never the S8 RAM window: the
/// manager dies between the join's durable step and its reply, a
/// successor is elected (the D0 ladder over the same volume), and the
/// replayed verb answers `already` off the page the dead manager wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_replayed_against_a_successor_manager_answers_already() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let me = joiner_identity(2);
    let (id, segments, grant) = {
        let routed = open_with_partition(&uris, Some(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let out = vol.manager_join_appender(me, 0).await.unwrap();
        assert!(!out.already);
        vol.sync_device().await.unwrap();
        // The manager dies before any reply reaches the joiner: no
        // shutdown, no leave — the page stays Live in the directory.
        drop(vol);
        drop(routed);
        (out.appender_id, out.ring_segments, out.grant)
    };
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let s = stats(&vol);
    assert_eq!(
        s.manager_lease,
        ManagerLease::Held,
        "the successor holds the role"
    );
    assert_eq!(s.self_recoveries, 2, "its own two regions' residue");
    let again = vol.manager_join_appender(me, 0).await.unwrap();
    assert!(
        again.already,
        "the successor answers from the page, with no RAM window"
    );
    assert_eq!(again.appender_id, id);
    assert_eq!(again.ring_segments, segments);
    assert_eq!(again.grant, grant, "the grant record survived the failover");
    assert_eq!(stats(&vol).manager_verb_replays, 1);
    assert_eq!(stats(&vol).manager_verb_refusals, 0);
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// The ONE hard refusal (§5.11, R-SYM-6): a join past `appenders_capacity`
/// (`heap/16 ÷ ring` — 7 on this 64 MiB volume at the 512 KiB floor ring)
/// refuses naming the volume count as the lever, never a format-time
/// client count; `manager_verb_refusals` counts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_past_appenders_capacity_refuses_naming_the_volume_count_lever() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let capacity = stats(&vol).capacity;
    assert_eq!(capacity, 7, "heap/16 ÷ 512 KiB on a 64 MiB volume");
    // Two live already (the manager + the declared region).
    let mut n = 0u64;
    let refusal = loop {
        match vol.manager_join_appender(joiner_identity(200 + n), 0).await {
            Ok(out) => {
                assert!(!out.already);
                n += 1;
                assert!(n < 64, "no capacity refusal");
            }
            Err(e) => break e.to_string(),
        }
    };
    assert_eq!(n, capacity - 2, "every page up to the capacity joined");
    assert!(
        refusal.contains("appenders_capacity") && refusal.contains("volume COUNT"),
        "{refusal}"
    );
    assert_eq!(stats(&vol).manager_verb_refusals, 1);
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// A join storm of 32 completes inside the bound and grows the directory
/// chain past its first extent (7 pairs at 64 KiB nodes): every joiner
/// gets a distinct page, ring and grant; the chain's headers link. A
/// 512 MiB volume: `appenders_capacity` = 32 MiB ÷ 512 KiB = 64.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_storm_of_32_completes_inside_the_bound_and_grows_the_directory_chain() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member_sized(dir.path(), "meta0", 512 * 1024 * 1024).await];
    let path = std::path::Path::new(&uris[0]);
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .expect("manager listener");
    let endpoint = host.endpoint().to_string();
    let pairs = dir_pairs_per_extent(NODE_SIZE as u64);
    assert_eq!(pairs, 7, "64 KiB nodes: 16 pages − the header = 7 pairs");
    let t0 = std::time::Instant::now();
    let mut ids = std::collections::BTreeSet::new();
    let mut client = ManagerClient::connect(&endpoint, SECRET, "storm")
        .await
        .unwrap();
    for n in 0..32u64 {
        match client.join(joiner_identity(100 + n), 0).await.unwrap() {
            ManagerReply::Joined {
                appender_id,
                already,
                ..
            } => {
                assert!(!already);
                assert!(ids.insert(appender_id), "distinct pages");
            }
            other => panic!("{other:?}"),
        }
    }
    let wall = t0.elapsed();
    let per_join = wall / 32;
    // §1.6: ≈ 2–5 ms per join incl. the barrier on the design's venue;
    // the dev box (tmpfs, debug) is scoping — the bound here is loose.
    assert!(
        per_join < std::time::Duration::from_millis(200),
        "32 joins took {wall:?} ({per_join:?} each)"
    );
    log::info!("join storm: 32 joins in {wall:?} ({per_join:?} per join, dev box — scoping)");
    let sb = vol.superblock().clone();
    let chain = read_directory_chain(path, &sb).await.unwrap();
    assert!(
        chain.len() >= (33u64).div_ceil(pairs) as usize,
        "33 appenders (the declared + 32) need {} extents of {pairs} pairs: {} in the chain",
        (33u64).div_ceil(pairs),
        chain.len()
    );
    for (i, (_, hdr)) in chain.iter().enumerate() {
        assert_eq!(hdr.chain_index, i as u32);
        assert_eq!(u64::from(hdr.pairs), pairs);
    }
    let entries = read_directory(path, &sb).await.unwrap();
    let live = entries
        .iter()
        .filter(|e| {
            e.page
                .as_ref()
                .is_some_and(|p| p.state == AppenderState::Live)
        })
        .count();
    assert_eq!(live, 34, "the manager, the declared region and 32 joiners");
    let s = stats(&vol);
    assert_eq!(s.manager_verb_refusals, 0);
    assert_eq!(s.extent_grants, 1 + 32);
    assert!(s.manager_verbs_per_s > 0);
    assert!(s.manager_load_pct <= 100);
    // The whole thing survives a remount: the same 34 Live pages, the same
    // chain — and the successor still answers each joiner `already`. The
    // listener's service holds the backend: it goes first, or the flock
    // it pins refuses the remount.
    host.shutdown();
    drop(host);
    vol.sync_device().await.unwrap();
    drop(vol);
    drop(routed);
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = &routed.volumes[0];
    let again = vol
        .manager_join_appender(joiner_identity(131), 0)
        .await
        .unwrap();
    assert!(again.already);
    let entries = read_directory(path, &sb).await.unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e
                .page
                .as_ref()
                .is_some_and(|p| p.state == AppenderState::Live))
            .count(),
        34
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// The negative contract: a bit-17-absent volume carries none of it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flat_mount_has_no_manager_lease_and_no_grants() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uri = format_flat_member(dir.path(), "flat0").await;
    let b = open_volume_for_mount(&uri).await.unwrap();
    assert!(
        b.appender_stats().is_none(),
        "no region, no manager, no grant"
    );
    assert!(
        b.extent_grant_records().await.unwrap().is_empty(),
        "tree 0 does not exist on a flat volume"
    );
    assert!(b.manager_extent_grant(1, 8).await.is_err());
    Metadata::create(b.as_ref(), ROOT_INO, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    b.checkpoint_now().await.unwrap();
    b.shutdown().await.unwrap();
}
