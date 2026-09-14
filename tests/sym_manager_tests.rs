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
use squeezefs::meta_backend::kv::tree::KvTree;
use squeezefs::meta_backend::kv::{
    META_KV_NODE_COMPACTIONS, META_KV_NODE_SPLITS, META_KV_REPLAY_EXTENT_VIOLATIONS,
    META_KV_REPLAY_KEY_VIOLATIONS, META_KV_REPLAY_LEASE_VIOLATIONS,
    META_KV_REPLAY_ROOT_FREES_DROPPED,
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
    // The page names the WHOLE unclaimed remainder as runs; the page
    // writer fits it to GRANT_RUNS_MAX by moving the excess to the
    // returnable batch (Issue 9), never by truncating.
    assert_eq!(g.unclaimed_runs(), vec![GrantRun { start: 102, len: 2 }]);
    g.add_runs(&[GrantRun { start: 200, len: 1 }]);
    g.add_runs(&[GrantRun { start: 300, len: 1 }]);
    g.add_runs(&[GrantRun { start: 400, len: 1 }]);
    g.add_runs(&[GrantRun { start: 500, len: 1 }]);
    assert_eq!(g.unclaimed_runs().len(), GRANT_RUNS_MAX + 1);
    assert_eq!(g.trim_to_page_runs(), 1);
    assert_eq!(g.unclaimed_runs().len(), GRANT_RUNS_MAX);
    assert_eq!(g.returnable(), 1);
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
        volume: 0,
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
            volume: 3,
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
    eprintln!(
        "wire join row: manager_service_ns admit/execute/reply/total = {:?} over {} verbs (dev \
         box — scoping)",
        s.manager_service_ns, s.manager_verbs
    );
    // ExtentGrant / ReturnExtents for the wire joiner: an unconsumed
    // remainder covering the ask is answered VERBATIM (§5.3.5, Issue 4 —
    // the join's grant, read off the joiner's page), returned, returned
    // again ⇒ already; with the remainder gone a fresh ask CARVES exactly
    // the (cap-clamped) want, and the joiner's page names it.
    let more = client.extent_grant(2, 4).await.unwrap();
    assert_eq!(
        more.iter().map(|r| (r.start, r.len)).collect::<Vec<_>>(),
        grant,
        "the unconsumed join grant, verbatim"
    );
    let n = more.iter().map(|r| u64::from(r.len)).sum::<u64>();
    let (cleared, already) = client.return_extents(2, &more).await.unwrap();
    assert_eq!((cleared, already), (n, 0));
    let page = read_directory(path, &sb).await.unwrap()[2]
        .page
        .clone()
        .expect("the joiner's page");
    assert!(
        page.grant.is_empty(),
        "the return rewrote the wire joiner's page remainder: {:?}",
        page.grant
    );
    let (cleared, already) = client.return_extents(2, &more).await.unwrap();
    assert_eq!((cleared, already), (0, n));
    let fresh = client.extent_grant(2, 4).await.unwrap();
    assert_eq!(fresh.iter().map(|r| u64::from(r.len)).sum::<u64>(), 4);
    let page = read_directory(path, &sb).await.unwrap()[2]
        .page
        .clone()
        .expect("the joiner's page");
    assert_eq!(page.grant, fresh, "the joiner's page names the fresh grant");
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
    // The row (`--nocapture`): the dev box is scoping, the design venue is
    // the box.
    eprintln!("join storm: 32 joins in {wall:?} ({per_join:?} per join, dev box — scoping)");
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
// §5.8.5 — fsck C13, the orphan image extent: an extent claimed inside a
// grant that no slot-tree root reaches and no pending-free names.
// ---------------------------------------------------------------------------

/// The heap extent of a node address on `kv`'s volume.
fn extent_of(kv: &KvMetaBackend, addr: u64) -> u64 {
    let sb = kv.superblock();
    (addr - sb.heap.start) / u64::from(sb.node_size)
}

/// The slot tree of forest `slot` (appender 1's is slot 4 under the
/// declared partition).
fn slot_tree(kv: &KvMetaBackend, slot: u32) -> Arc<KvTree> {
    kv.all_trees()
        .into_iter()
        .find(|t| t.forest_slot() == Some(slot))
        .expect("the slot tree exists once a record landed in it")
}

/// Drive the crash window C13 exists for (design §5.3.4 "root swap done
/// in RAM, page not yet naming the new root"): a quiesced slot tree, a
/// FORCED root-leaf compaction (the D4 nudge — a root swap outside any
/// checkpoint cycle, its `alloc(new)` + `free(old)` in appender 1's ring,
/// no page naming the successor), then a crash-equivalent drop. Returns
/// `(uris, guest owner, old root extent, successor extent)`.
async fn crash_after_an_unpublished_root_swap(
    dir: &std::path::Path,
) -> (Vec<String>, u64, u64, u64) {
    let uris = vec![format_stamped_member(dir, "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 79); // forest slot 4 — appender 1's
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    for i in 0..4u64 {
        va.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 100, 40))
            .await
            .unwrap();
    }
    va.checkpoint_now().await.unwrap();
    va.checkpoint_now().await.unwrap();
    let tree = slot_tree(&va, 4);
    let old_root = tree.root();
    let compacted = va
        .defrag_compact_nodes(&[(0, old_root.addr)])
        .await
        .expect("forced compaction of appender 1's root leaf");
    assert_eq!(compacted, 1, "the nudge compacts the slot tree's root");
    let new_root = tree.root();
    assert_ne!(old_root.addr, new_root.addr, "a root swap happened");
    let old_ext = extent_of(&va, old_root.addr);
    let new_ext = extent_of(&va, new_root.addr);
    let s = stats(&va);
    assert!(
        s.regions[1].grant_claimed >= 1 && s.regions[1].grant_pending == 1,
        "the successor is claimed and the predecessor parked on appender 1's tail: {:?}",
        s.regions[1]
    );
    // A LIVE in-window image is never a finding: the RAM root reaches the
    // successor, the predecessor is a pending-free.
    assert!(
        va.c13_orphan_image_extents().await.unwrap().is_empty(),
        "the live swap is in-window and covered by the coverage gate — no finding"
    );
    drop(va);
    drop(ra);
    (uris, guest_owner, old_ext, new_ext)
}

/// The pin: crash after `alloc(extent)` landed but before the root swap
/// publishes ⇒ C13 finds EXACTLY that extent (the successor image); the
/// predecessor — the page's root, live again after the replay — keeps
/// its bit (its replayed free is dropped, `meta_kv_replay_root_frees_
/// dropped`); repair returns the orphan to the bitmap through the
/// appender's own ring (a `free` gated on its tail, the ordinary SMO
/// retirement shape — the cadence's `ReturnExtents` clears the bit and
/// rewrites the grant record), the closure law holds throughout, a second
/// sweep finds nothing, and the remount reads every acked record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c13_finds_exactly_the_unpublished_root_swaps_successor_and_repair_returns_it() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let tag = volume_tag("vol-0011223344556677");
    let dropped_before = META_KV_REPLAY_ROOT_FREES_DROPPED.load(Ordering::Relaxed);
    let (uris, guest_owner, old_ext, new_ext) =
        crash_after_an_unpublished_root_swap(dir.path()).await;

    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(META_KV_REPLAY_EXTENT_VIOLATIONS.load(Ordering::Relaxed), 0);
    let tree = slot_tree(&va, 4);
    assert_eq!(
        extent_of(&va, tree.root().addr),
        old_ext,
        "the page names the predecessor: the mount replays through it"
    );
    assert!(
        va.allocator().is_allocated(old_ext),
        "the LIVE root's extent stays allocated: its replayed free is the unpublished swap's"
    );
    assert_eq!(
        META_KV_REPLAY_ROOT_FREES_DROPPED.load(Ordering::Relaxed),
        dropped_before + 1,
        "exactly one replayed root free dropped"
    );
    let orphans = va.c13_orphan_image_extents().await.unwrap();
    assert_eq!(
        orphans
            .iter()
            .map(|o| (o.appender, o.extent))
            .collect::<Vec<_>>(),
        vec![(1u32, new_ext)],
        "C13 finds exactly the unpublished successor image"
    );
    let s = stats(&va);
    assert_grant_closure(&s);
    assert!(
        va.extent_grant_record(1).await.unwrap().contains(new_ext),
        "the orphan is still inside appender 1's grant"
    );

    // Repair: return it to the bitmap.
    assert!(va.c13_return_orphan(1, new_ext).await.unwrap());
    assert!(
        va.c13_orphan_image_extents().await.unwrap().is_empty(),
        "repaired: the extent is a pending-free now, no longer a finding"
    );
    assert!(
        !va.c13_return_orphan(1, new_ext).await.unwrap(),
        "a second repair of the same extent is a no-op (verify-before-repair)"
    );
    let mut cycles = 0;
    while va.allocator().is_allocated(new_ext) && cycles < 6 {
        va.checkpoint_now().await.unwrap();
        cycles += 1;
    }
    assert!(
        !va.allocator().is_allocated(new_ext),
        "the cadence returned the orphan to the bitmap within {cycles} cycles"
    );
    assert!(
        !va.extent_grant_record(1).await.unwrap().contains(new_ext),
        "the grant record no longer names it"
    );
    let s = stats(&va);
    assert!(s.extent_returns >= 1, "{s:?}");
    assert_grant_closure(&s);
    assert!(
        va.allocator().is_allocated(old_ext),
        "the live root is untouched"
    );
    assert!(va.block_ref_count(tag, 0).await.unwrap() >= 1);
    let live = digest_backend(&va).await.unwrap();
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
    drop(va);
    drop(ra);
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = &ra.volumes[0];
    assert_eq!(META_KV_REPLAY_EXTENT_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(digest_backend(va).await.unwrap(), live);
    // The returned extent is back in the pool: free, or legitimately
    // re-claimed by a later image / re-granted (lowest-free-first) — but
    // never an orphan again — and the sweep is a REAL one: the clean
    // remount recovered the grant from tree 0, so every extent the
    // record names is in a RAM set (review round 1, Issue 1: the first
    // build forgot the record on a clean remount and this `is_empty()`
    // passed vacuously).
    assert_durable_ram_grant_law(va, 1).await;
    assert!(va.c13_orphan_image_extents().await.unwrap().is_empty());
    assert_grant_closure(&stats(va));
    let _ = guest_owner;
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// The class through fsck's ladder: the census nominates the suspect,
/// the re-check under the SMO serialization confirms it, the report
/// carries `C13` with its identity, the dry run plans
/// `return-orphan-image-extent` and mutates nothing, `--apply` repairs it
/// once (`fsck_repair_classC13` +1), and the re-fsck is clean. A flat
/// volume's fsck never produces the class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_c13_reports_plans_and_repairs_the_orphan_image_extent() {
    use squeezefs::fsck::{repair, run, FsckOptions, RepairOptions};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let (uris, _guest_owner, _old_ext, new_ext) =
        crash_after_an_unpublished_root_swap(dir.path()).await;

    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let ctx = fsck_ctx(dir.path(), Arc::clone(&ra)).await;
    let mut opts = FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let report = run(&ctx, &opts).await.expect("fsck runs");
    let c13: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.class == "C13")
        .collect();
    assert_eq!(c13.len(), 1, "one C13 finding: {:?}", report.findings);
    assert!(
        c13[0].object.contains(&format!("extent{new_ext}")) && c13[0].object.contains("appender1"),
        "the finding names the appender and the extent: {:?}",
        c13[0]
    );
    assert!(
        report.findings.iter().all(|f| f.class == "C13"),
        "nothing else is wrong with the volume: {:?}",
        report.findings
    );
    let dry = repair(
        &ctx,
        &report,
        &RepairOptions {
            apply: false,
            quarantine_dir: None,
            multi_owner: false,
        },
    )
    .await
    .expect("dry run");
    assert_eq!(dry.planned.len(), 1, "{dry:?}");
    assert_eq!(dry.planned[0].action, "return-orphan-image-extent");
    assert_eq!(dry.counters.applied, 0);
    assert!(
        ra.volumes[0].allocator().is_allocated(new_ext),
        "the dry run mutated nothing"
    );
    let before = squeezefs::fuse_client::METRICS
        .fsck_repair_class_c13
        .load(Ordering::Relaxed);
    let applied = repair(
        &ctx,
        &report,
        &RepairOptions {
            apply: true,
            quarantine_dir: None,
            multi_owner: false,
        },
    )
    .await
    .expect("apply");
    assert_eq!(applied.counters.applied, 1, "{applied:?}");
    assert_eq!(applied.counters.refused, 0, "{applied:?}");
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .fsck_repair_class_c13
            .load(Ordering::Relaxed),
        before + 1
    );
    let va = &ra.volumes[0];
    let mut cycles = 0;
    while va.allocator().is_allocated(new_ext) && cycles < 6 {
        va.checkpoint_now().await.unwrap();
        cycles += 1;
    }
    assert!(
        !va.allocator().is_allocated(new_ext),
        "returned to the bitmap"
    );
    let again = run(&ctx, &opts).await.expect("re-fsck");
    assert!(
        again.findings.is_empty(),
        "the repaired volume is clean: {:?}",
        again.findings
    );
    // Re-applying the stale report refuses (verify-before-repair).
    let stale = repair(
        &ctx,
        &report,
        &RepairOptions {
            apply: true,
            quarantine_dir: None,
            multi_owner: false,
        },
    )
    .await
    .expect("stale apply");
    assert_eq!(stale.counters.applied, 0, "{stale:?}");
    assert_eq!(stale.counters.refused, 1, "{stale:?}");
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// A mount-shaped fsck context over an already-open routed set: one
/// file-backed data volume, the router registered the way a mount does.
async fn fsck_ctx(dir: &std::path::Path, meta: Arc<RoutedMetaBackend>) -> squeezefs::fsck::FsckCtx {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::dlm::DlmClient;
    use squeezefs::nvme_dev::NvmeBlockDev;
    use squeezefs::routing::DataRouter;
    const BLOCK: u64 = 4096;
    let oss = dir.join("oss0");
    std::fs::File::create(&oss)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![oss.display().to_string()]),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    };
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let records = cfg.resolved_data_volumes();
    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let cache = TieredCache::new(
        vec![staging.clone()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc, dev);
    for rec in &records {
        router.backend_router.register_backend(rec).await.unwrap();
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(meta.clone());
    squeezefs::fsck::FsckCtx {
        meta,
        router,
        staging_dirs: vec![staging],
        expected_generation: None,
    }
}

// ---------------------------------------------------------------------------
// Review round 1 — the grant across a CLEAN remount (Issue 1), the wire's
// integers (Issue 2), the join's already-arm (3), ExtentGrant's
// idempotency (4), the space class on a leased slot (6), the page's
// remainder (9), the leased leaf's promise (10), a return's RAM face (11),
// the cap's appender count (12).
// ---------------------------------------------------------------------------

/// The durable-vs-RAM grant law after ANY open: every extent tree 0's
/// `extent_grant:{id}` record names is in one of the region's RAM sets —
/// `claimed ∪ unclaimed ∪ pending ∪ returnable` — so a retirement of a
/// pre-remount image parks and returns like any other, and the C13 census
/// (over `claimed`) is never vacuous.
async fn assert_durable_ram_grant_law(vol: &KvMetaBackend, id: u32) {
    let record = vol.extent_grant_record(id).await.unwrap();
    let s = stats(vol);
    let r = &s.regions[id as usize];
    assert_eq!(
        record.len(),
        r.grant_claimed + r.grant_unclaimed + r.grant_pending + r.grant_returnable,
        "tree 0's record ({} extents) must be exactly the RAM sets of appender {id}: {r:?}",
        record.len()
    );
    for e in record.extents() {
        assert!(
            vol.grant_holds(id, e),
            "extent {e} is in tree 0's record but in no RAM set of appender {id}"
        );
    }
}

/// Issue 1: three CLEAN unmount/remount cycles, a retirement in each —
/// the grant is recovered from tree 0 at every open (a `Free` page names
/// no remainder, so everything the record names is CLAIMED, the truth
/// after a clean leave returned the rest), the retired pre-remount image
/// parks on the region's tail, the cadence returns it (bit cleared,
/// record rewritten), and the closure law holds across the remounts. Then
/// an orphan PLANTED after the third remount is found by C13 — the census
/// is a real one on a remounted region.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_remount_recovers_the_grant_from_tree_zero_and_pre_remount_images_return() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 80); // forest slot 4 — appender 1's
    for cycle in 0..3u64 {
        std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
        let ra = open_with_partition(&uris, Some(PARTITION)).await;
        std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        let va = Arc::clone(&ra.volumes[0]);
        for i in 0..4u64 {
            va.commit_block_refs(
                guest_owner,
                &refs(tag, guest_owner, cycle * 10_000 + i * 100, 40),
            )
            .await
            .unwrap();
        }
        va.checkpoint_now().await.unwrap();
        va.checkpoint_now().await.unwrap();
        assert_durable_ram_grant_law(&va, 1).await;
        if cycle > 0 {
            let s = stats(&va);
            assert!(
                s.regions[1].grant_claimed >= 1,
                "cycle {cycle}: the slot tree's live images are CLAIMED after the clean remount \
                 (tree 0's record named them): {:?}",
                s.regions[1]
            );
        }
        // The retirement: a forced compaction of the slot tree's root —
        // the old root (a pre-remount image from cycle 1 on) must park on
        // appender 1's tail and return within a few cycles.
        let tree = slot_tree(&va, 4);
        let old_root = tree.root();
        let old_ext = extent_of(&va, old_root.addr);
        let returns_before = stats(&va).extent_returns;
        assert_eq!(
            va.defrag_compact_nodes(&[(0, old_root.addr)])
                .await
                .unwrap(),
            1
        );
        assert_ne!(tree.root().addr, old_root.addr);
        let s = stats(&va);
        assert_eq!(
            s.regions[1].grant_pending, 1,
            "cycle {cycle}: the retired root is PARKED on appender 1's tail (a remount-forgotten \
             grant drops the free): {:?}",
            s.regions[1]
        );
        let mut cycles = 0;
        while va.allocator().is_allocated(old_ext) && cycles < 6 {
            va.checkpoint_now().await.unwrap();
            cycles += 1;
        }
        assert!(
            !va.allocator().is_allocated(old_ext),
            "cycle {cycle}: the retired pre-remount image {old_ext} returned to the bitmap \
             within {cycles} cycles"
        );
        assert!(
            !va.extent_grant_record(1).await.unwrap().contains(old_ext),
            "cycle {cycle}: the record dropped the returned extent"
        );
        assert!(stats(&va).extent_returns > returns_before);
        assert_grant_closure(&stats(&va));
        assert_durable_ram_grant_law(&va, 1).await;
        assert!(va.c13_orphan_image_extents().await.unwrap().is_empty());
        for v in &ra.volumes {
            v.shutdown().await.unwrap();
        }
    }
    // The planted orphan AFTER the remounts: a root swap the crash leaves
    // unpublished — C13 on the remounted region finds exactly it.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    assert_durable_ram_grant_law(&va, 1).await;
    let tree = slot_tree(&va, 4);
    let old_root = tree.root();
    assert_eq!(
        va.defrag_compact_nodes(&[(0, old_root.addr)])
            .await
            .unwrap(),
        1
    );
    let planted = extent_of(&va, tree.root().addr);
    assert!(
        va.extent_grant_record(1).await.unwrap().contains(planted),
        "the successor was claimed INSIDE appender 1's grant"
    );
    drop(va);
    drop(ra);
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    let va = &ra.volumes[0];
    assert_durable_ram_grant_law(va, 1).await;
    let orphans = va.c13_orphan_image_extents().await.unwrap();
    assert_eq!(
        orphans
            .iter()
            .map(|o| (o.appender, o.extent))
            .collect::<Vec<_>>(),
        vec![(1u32, planted)],
        "C13 finds the orphan planted after the remounts — a real census over a recovered grant"
    );
    assert!(va.c13_return_orphan(1, planted).await.unwrap());
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 2: a wire integer is never an allocation authority. A
/// `ReturnExtents` run past the volume's extents or one that overflows
/// is REJECTED before any expansion (`manager_verb_rejected` — the
/// wire-invalid class, kept apart from the durable-witness
/// `manager_verb_refusals` must-stay-0); a geometrically valid run is
/// intersected with the record as intervals, so the materialized list is
/// bounded by the record and never by the frame (a run over the WHOLE
/// volume clears the caller's record and answers the rest `already`); an
/// explicit `ExtentGrant { want }` is clamped to the derivation's cap and
/// never carves the free heap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_integers_are_never_allocation_authority() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let total = vol.allocator().total_extents();
    let free_before = vol.free_extents();
    // Past the volume, overflowing: rejected before anything proportional
    // to the wire integer happens.
    for runs in [
        vec![GrantRun {
            start: 0,
            len: u32::MAX,
        }],
        vec![GrantRun {
            start: u64::MAX - 1,
            len: 4,
        }],
        vec![GrantRun {
            start: total,
            len: 1,
        }],
    ] {
        let err = vol
            .manager_return_runs(1, &runs)
            .await
            .expect_err("a wire-invalid run is rejected before any allocation");
        assert!(
            err.to_string().contains("rejected"),
            "the rejection names its class: {err}"
        );
    }
    let s = stats(&vol);
    assert_eq!(
        s.manager_verb_rejected, 3,
        "three wire-invalid frames rejected"
    );
    assert_eq!(
        s.manager_verb_refusals, 0,
        "a wire-invalid frame is not the durable-witness class"
    );
    assert_eq!(vol.free_extents(), free_before, "nothing was cleared");
    // A run partly outside the record: only the intersection returns.
    let record = vol.extent_grant_record(1).await.unwrap();
    let first = record.extents().next().unwrap();
    let (cleared, already) = vol
        .manager_return_runs(
            1,
            &[GrantRun {
                start: first,
                len: 1,
            }],
        )
        .await
        .unwrap();
    assert_eq!((cleared, already), (1, 0));
    // Every extent of the volume, in two geometrically valid runs: the
    // intersection with the record is materialized (bounded by the
    // record, never by the frame — here appender 1's unclaimed remainder,
    // which returns), the rest is `already`; nothing outside the record
    // is touched.
    let record = vol.extent_grant_record(1).await.unwrap();
    let n = record.len();
    let free_before = vol.free_extents();
    let (cleared, already) = vol
        .manager_return_runs(
            1,
            &[
                GrantRun {
                    start: 0,
                    len: (total / 2) as u32,
                },
                GrantRun {
                    start: total / 2,
                    len: (total - total / 2) as u32,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!((cleared, already), (n, total - n));
    assert_eq!(vol.free_extents(), free_before + n);
    assert!(vol.extent_grant_record(1).await.unwrap().is_empty());
    // An explicit `want` past the cap is clamped: the grant never CARVES
    // more than the derivation's cap, never the free heap (the reply may
    // be larger — it names the whole remainder, §5.3.5).
    let free_before = vol.free_extents();
    let cap = resolve_grant_extents(
        0,
        stats(&vol).failover_bound_ms,
        free_before,
        stats(&vol).appenders_known,
    );
    let granted = vol.manager_extent_grant(1, u32::MAX).await.unwrap();
    assert!(!granted.is_empty());
    let carved = free_before - vol.free_extents();
    assert!(
        carved <= cap.max(GRANT_EXTENTS_FLOOR),
        "want = u32::MAX carved {carved} extents — past the derivation's cap {cap}"
    );
    assert!(
        vol.free_extents() >= free_before - cap.max(GRANT_EXTENTS_FLOOR),
        "the free heap survives a hostile want"
    );
    // The same laws over the wire.
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .unwrap();
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "peer-w")
        .await
        .unwrap();
    let err = client
        .return_extents(
            1,
            &[GrantRun {
                start: 0,
                len: u32::MAX,
            }],
        )
        .await
        .expect_err("rejected over the wire");
    assert!(err.to_string().contains("rejected"), "{err}");
    let free_before = vol.free_extents();
    let granted = client.extent_grant(1, u32::MAX).await.unwrap();
    assert!(!granted.is_empty());
    assert!(free_before - vol.free_extents() <= cap.max(GRANT_EXTENTS_FLOOR));
    assert_eq!(stats(&vol).manager_verb_rejected, 4);
    host.shutdown();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 3: `Joined { already }` answers the joiner's UNCLAIMED remainder
/// (its page's grant), never the whole record — and a join interrupted
/// between its page going Live and its initial grant (the seam
/// `TEST_JOIN_HOLD_AFTER_PAGE`) is completed by the replay: the successor
/// mints the grant the interrupted join owed and answers it, so
/// `Joined.grant` means the same thing on every reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_interrupted_after_its_page_replays_already_with_the_grant_it_was_owed() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let identity = joiner_identity(9);
    // The interrupted join: the page is Live, the grant never minted.
    squeezefs::meta_backend::kv::backend::TEST_JOIN_HOLD_AFTER_PAGE.store(true, Ordering::SeqCst);
    let err = vol
        .manager_join_appender(identity, 0)
        .await
        .expect_err("the seam fails the join after its page");
    squeezefs::meta_backend::kv::backend::TEST_JOIN_HOLD_AFTER_PAGE.store(false, Ordering::SeqCst);
    assert!(
        err.to_string().contains("TEST_JOIN_HOLD_AFTER_PAGE"),
        "{err}"
    );
    let entries = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .unwrap();
    let page = entries
        .iter()
        .find_map(|e| {
            e.page
                .as_ref()
                .filter(|p| p.identity == identity && p.state == AppenderState::Live)
        })
        .expect("the page is Live");
    assert!(page.grant.is_empty(), "no grant was minted");
    assert!(vol
        .extent_grant_record(page.appender_id)
        .await
        .unwrap()
        .is_empty());
    // The replay completes the join: already, AND a grant.
    let out = vol.manager_join_appender(identity, 0).await.unwrap();
    assert!(out.already, "KD-SYM-7: the Live page is the witness");
    let granted: u64 = out.grant.iter().map(|r| u64::from(r.len)).sum();
    assert!(
        granted >= GRANT_EXTENTS_FLOOR,
        "the interrupted join's grant is minted by the replay: {:?}",
        out.grant
    );
    let record = vol.extent_grant_record(out.appender_id).await.unwrap();
    assert_eq!(record.len(), granted, "the record names it");
    // A second replay answers the UNCLAIMED remainder (the page's grant),
    // not the record — after the joiner claims one, they differ.
    let again = vol.manager_join_appender(identity, 0).await.unwrap();
    assert!(again.already);
    assert_eq!(
        again.grant, out.grant,
        "unconsumed: the remainder IS the grant"
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 4 / §5.3.5: `ExtentGrant` is idempotent against DURABLE state —
/// a caller whose unclaimed remainder (its page's grant) already covers
/// the size it asks for is answered that remainder VERBATIM (a replay
/// after a lost reply carves nothing; `manager_verb_replays`); a caller
/// that has consumed past half gets a fresh carve (the 50 % law).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconsumed_grant_is_answered_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let s0 = stats(&vol);
    assert_eq!(s0.extent_grants, 1, "the join's initial grant");
    let first = vol.manager_extent_grant(1, 0).await.unwrap();
    let s1 = stats(&vol);
    assert_eq!(
        s1.extent_grants, 1,
        "an unconsumed grant is answered verbatim — nothing carved"
    );
    assert_eq!(s1.manager_verb_replays, s0.manager_verb_replays + 1);
    let n: u64 = first.iter().map(|r| u64::from(r.len)).sum();
    assert_eq!(n, s1.regions[1].grant_unclaimed, "the remainder, verbatim");
    let again = vol.manager_extent_grant(1, 0).await.unwrap();
    assert_eq!(again, first);
    // A wire joiner's remainder is read off ITS page.
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .unwrap();
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "peer-i")
        .await
        .unwrap();
    let joined = client.join(joiner_identity(11), 0).await.unwrap();
    let ManagerReply::Joined {
        appender_id, grant, ..
    } = joined
    else {
        panic!("{joined:?}");
    };
    let grants_before = stats(&vol).extent_grants;
    let g1 = client.extent_grant(appender_id, 0).await.unwrap();
    assert_eq!(
        g1.iter().map(|r| (r.start, r.len)).collect::<Vec<_>>(),
        grant,
        "the wire joiner's unconsumed grant, verbatim"
    );
    assert_eq!(stats(&vol).extent_grants, grants_before, "nothing carved");
    host.shutdown();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 6: a FULL heap on a leased slot is the SPACE class, never the
/// manager dependency — a grant the bitmap cannot serve answers
/// `NoSpace`, the flush pass counts it on the space class and
/// `manager_dependency_stalls` stays 0; and a COMPACTION-class grant
/// draws down to the compaction floor like the manager's own SMOs, so the
/// heap-full recovery can return extents through a leased slot tree too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_heap_on_a_leased_slot_is_the_space_class_not_a_manager_stall() {
    use squeezefs::meta_backend::kv::alloc_ext_core::AllocClass;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // Appender 1 returns its unconsumed initial grant first — §5.3.5's
    // idempotency would otherwise answer that remainder verbatim, and the
    // contract here is the CARVE's answer on a full heap.
    let remainder = vol.appenders_public().unwrap().regions[1]
        .grant()
        .unclaimed_runs();
    assert!(!remainder.is_empty());
    vol.manager_return_runs(1, &remainder).await.unwrap();
    assert_eq!(stats(&vol).regions[1].grant_unclaimed, 0);
    // Drain the USER-claimable heap through the manager's own claimer.
    let mut drained = Vec::new();
    loop {
        match vol.allocator().claim_user() {
            Ok(e) => drained.push(e),
            Err(squeezefs::meta_backend::kv::KvError::NoSpace { .. }) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    // A USER-class grant on the full heap is the space class.
    let err = vol
        .manager_extent_grant(1, 8)
        .await
        .expect_err("no user-claimable extent: NoSpace, never Ok(empty)");
    assert!(
        matches!(err, squeezefs::meta_backend::kv::KvError::NoSpace { .. }),
        "{err:?}"
    );
    // A COMPACTION-class grant draws the reserve down to the floor.
    let granted = vol
        .manager_extent_grant_class(1, 2, AllocClass::Internal)
        .await
        .unwrap();
    assert!(
        granted.iter().map(|r| u64::from(r.len)).sum::<u64>() >= 1,
        "the compaction class carves from the reserve: {granted:?}"
    );
    let s = stats(&vol);
    assert_eq!(s.dependency_stalls, 0, "a space condition is not a stall");
    for e in drained {
        vol.allocator().release_unpublished(e);
    }
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 9: the page names the WHOLE unclaimed remainder. A RAM remainder
/// in more runs than the page carries (`GRANT_RUNS_MAX`) trims its
/// smallest runs into the returnable batch before the page is written,
/// so a crash-class open never recovers legitimately unclaimed extents as
/// claimed (a two-cycle C13 round trip).
#[test]
fn the_page_remainder_is_never_truncated_the_excess_returns() {
    let mut g = RegionGrant::default();
    // Six single-extent runs (every other extent).
    g.add_runs(
        &(0..6u64)
            .map(|i| GrantRun {
                start: 100 + 2 * i,
                len: 1,
            })
            .collect::<Vec<_>>(),
    );
    assert_eq!(g.unclaimed(), 6);
    let trimmed = g.trim_to_page_runs();
    assert_eq!(trimmed, 6 - GRANT_RUNS_MAX as u64);
    assert_eq!(g.unclaimed_runs().len(), GRANT_RUNS_MAX);
    assert_eq!(
        g.unclaimed(),
        GRANT_RUNS_MAX as u64,
        "the page's runs ARE the remainder"
    );
    assert_eq!(g.returnable(), trimmed, "the excess is returnable");
    assert_eq!(g.held() + g.unclaimed(), 6, "closure: nothing lost");
    // The trim keeps the LARGEST runs.
    let mut g = RegionGrant::default();
    g.add_runs(&[
        GrantRun { start: 0, len: 4 },
        GrantRun { start: 10, len: 1 },
        GrantRun { start: 20, len: 3 },
        GrantRun { start: 30, len: 1 },
        GrantRun { start: 40, len: 2 },
    ]);
    assert_eq!(g.trim_to_page_runs(), 1);
    assert_eq!(g.returnable(), 1, "one single-extent run trimmed");
    assert_eq!(g.unclaimed(), 10);
    assert_eq!(g.unclaimed_runs().len(), GRANT_RUNS_MAX);
}

/// Issue 10: a leased slot's leaf promises its SMO's extents against the
/// REGION's grant (`grant_promised`), not the heap ledger — its SMO
/// draws the grant, and a heap promise was a transient over-count on the
/// manager's claimable; the promise is released when the SMO claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leased_leafs_promise_rides_the_grant_not_the_heap() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 81);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    let heap_promised_before = va.heap_promised();
    // Enough refs into appender 1's slot that the leaf's fold overflows
    // its log: the admission promises the SMO's extents.
    for i in 0..6u64 {
        va.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 1000, 400))
            .await
            .unwrap();
    }
    assert_eq!(
        va.heap_promised(),
        heap_promised_before,
        "a leased leaf never promises on the heap ledger"
    );
    va.checkpoint_now().await.unwrap();
    let s = stats(&va);
    assert!(
        s.regions[1].grant_claimed >= 2,
        "the folds ran as SMOs of appender 1's tree: {:?}",
        s.regions[1]
    );
    assert_eq!(
        s.regions[1].grant_promised, 0,
        "every promise the admission recorded on the grant was released by its SMO: {:?}",
        s.regions[1]
    );
    assert_eq!(va.heap_promised(), heap_promised_before);
    assert_grant_closure(&s);
    // The arithmetic, on the RAM grant itself — the heap ledger's
    // lifecycle verbatim: a promise reserves headroom, the SMO's claim
    // draws the remainder (the promise stands until the SMO takes the
    // overlay and its node RELEASES it), a retraction / release returns
    // it.
    let mut g = RegionGrant::default();
    g.add_runs(&[GrantRun { start: 0, len: 4 }]);
    assert_eq!(g.headroom(), 4);
    g.promise(3);
    assert_eq!((g.promised(), g.headroom()), (3, 1));
    assert_eq!(g.claim(), Some(0));
    assert_eq!((g.promised(), g.headroom()), (3, 0));
    g.retract_promise(3);
    assert_eq!((g.promised(), g.headroom()), (0, 3));
    // The shared ledger: a node pointed at it promises HERE.
    let ledger = g.promise_ledger();
    ledger.fetch_add(2, Ordering::AcqRel);
    assert_eq!((g.promised(), g.headroom()), (2, 1));
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 11: a `ReturnExtents` of extents an IN-PROCESS region's RAM
/// grant still holds drops them from RAM too (unclaimed / returnable /
/// pending — never a live image: a CLAIMED extent is refused), so the
/// bitmap and the RAM grant cannot disagree and a later `claim()` can
/// never hand out an extent the manager re-granted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_return_drops_the_extents_from_the_ram_grant_and_refuses_a_live_image() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 82);
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, 0, 8))
        .await
        .unwrap();
    vol.checkpoint_now().await.unwrap();
    let s = stats(&vol);
    let unclaimed_before = s.regions[1].grant_unclaimed;
    assert!(s.regions[1].grant_claimed >= 1);
    let record = vol.extent_grant_record(1).await.unwrap();
    // The live image (the slot tree's root) is CLAIMED: refused.
    let root_ext = extent_of(&vol, slot_tree(&vol, 4).root().addr);
    assert!(record.contains(root_ext));
    let err = vol
        .manager_return_runs(
            1,
            &[GrantRun {
                start: root_ext,
                len: 1,
            }],
        )
        .await
        .expect_err("a claimed extent holds a live image — refused");
    assert!(err.to_string().contains("claimed"), "{err}");
    assert!(vol.allocator().is_allocated(root_ext));
    // Two UNCLAIMED extents: returned AND dropped from RAM.
    let two: Vec<u64> = record
        .extents()
        .filter(|e| *e != root_ext && vol.grant_holds(1, *e))
        .take(2)
        .collect();
    assert_eq!(two.len(), 2);
    let (cleared, already) = vol
        .manager_return_runs(
            1,
            &two.iter()
                .map(|e| GrantRun { start: *e, len: 1 })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert_eq!((cleared, already), (2, 0));
    let s = stats(&vol);
    assert_eq!(s.regions[1].grant_unclaimed, unclaimed_before - 2);
    for e in &two {
        assert!(!vol.grant_holds(1, *e), "extent {e} left every RAM set");
        assert!(!vol.allocator().is_allocated(*e));
    }
    assert_grant_closure(&s);
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 12: the grant cap's appender count is the directory's LIVE
/// count (in-process regions AND wire joiners), published as
/// `appenders_known`; a wire join grows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_grant_cap_counts_every_live_appender_wire_joiners_included() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(stats(&vol).appenders_known, 2, "the manager and appender 1");
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .unwrap();
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "peer-k")
        .await
        .unwrap();
    client.join(joiner_identity(12), 0).await.unwrap();
    assert_eq!(stats(&vol).appenders_known, 3, "the wire joiner counts");
    client.join(joiner_identity(12), 0).await.unwrap();
    assert_eq!(stats(&vol).appenders_known, 3, "a replay does not");
    host.shutdown();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Review round 2 — Issue 16: the clean leave over an SMO in the FINAL cycle.
// ---------------------------------------------------------------------------

/// Drive the reviewer's shape: `rounds` of 3 × 400 refs into appender
/// 1's slot with a checkpoint between rounds and the LAST round left
/// uncheckpointed, so the final cycle's flush pass may run an SMO on the
/// leased slot tree (its records ride the REGION's ring since PR 3).
/// Returns the acked bases and how many SMOs the shutdown ran.
async fn final_cycle_shape(
    uris: &[String],
    rounds: u64,
    tag: u64,
    guest_owner: u64,
) -> (Vec<u64>, u64) {
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let ra = open_with_partition(uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let va = Arc::clone(&ra.volumes[0]);
    let mut acked: Vec<u64> = Vec::new();
    for round in 0..rounds {
        for i in 0..3u64 {
            let base = round * 10_000 + i * 600;
            va.commit_block_refs(guest_owner, &refs(tag, guest_owner, base, 400))
                .await
                .unwrap();
            acked.push(base);
        }
        if round + 1 < rounds {
            va.checkpoint_now().await.unwrap();
        }
    }
    let smos_before = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed);
    for v in &ra.volumes {
        v.shutdown().await.unwrap();
    }
    let smos_in_final = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed)
        - smos_before;
    (acked, smos_in_final)
}

/// Issue 16 (bug): a CLEAN unmount whose final checkpoint cycle's flush
/// pass runs an SMO on a LEASED slot tree must lose nothing. The
/// shutdown fixpoint was ring-0-only, so with the SMO's records in the
/// REGION's ring (PR 3's own-ring structure) the loop exited after one
/// cycle, the leave cleared the page's roots and declared the uncovered
/// window covered, and the next open routed through the PREDECESSOR
/// image — acked block references read back at count 0. The fixpoint
/// must cover EVERY ring (`rings_uncovered`), the leave leaves nothing
/// pending (a region's last-cycle retirements would otherwise recover as
/// CLAIMED-unreached C13 candidates on a healthy lifecycle), and every
/// acked base survives. Every round count 1..=8 is driven so the shape
/// is reached whatever the fold's exact spacing; at least one must run
/// an SMO inside the final cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leased_trees_smo_inside_the_final_cycle_survives_a_clean_leave() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 90);
    let mut hit = 0u64;
    for rounds in 1..=8u64 {
        let uris = vec![format_stamped_member(dir.path(), &format!("r{rounds}")).await];
        let (acked, smos_in_final) = final_cycle_shape(&uris, rounds, tag, guest_owner).await;
        let rb = open_with_partition(&uris, Some(PARTITION)).await;
        let vb = Arc::clone(&rb.volumes[0]);
        let s = stats(&vb);
        assert_eq!(
            s.self_recoveries, 0,
            "rounds={rounds}: a clean leave leaves no own residue: {s:?}"
        );
        let mut missing: Vec<u64> = Vec::new();
        for base in &acked {
            if vb.block_ref_count(tag, *base).await.unwrap() != 1 {
                missing.push(*base);
            }
        }
        assert!(
            missing.is_empty(),
            "rounds={rounds}: {} of {} acked block-ref bases unreadable after a CLEAN unmount \
             whose final cycle ran {smos_in_final} SMO(s) on the leased slot tree: {missing:?}",
            missing.len(),
            acked.len()
        );
        assert_eq!(
            s.regions[1].grant_pending, 0,
            "rounds={rounds}: the leave drained the region's last-cycle retirements"
        );
        assert!(
            vb.c13_orphan_image_extents().await.unwrap().is_empty(),
            "rounds={rounds}: a healthy clean lifecycle plants no C13 candidate"
        );
        if smos_in_final > 0 {
            hit += 1;
        }
        for v in &rb.volumes {
            v.shutdown().await.unwrap();
        }
    }
    assert!(
        hit > 0,
        "no round count produced an SMO inside the final cycle — the shape was not reached"
    );
}

/// Issue 16's belt: a leave over an UNCOVERED region window must never
/// write that region's page `Free` (that declares the window covered and
/// frees the ring). The seam `TEST_SHUTDOWN_FIXPOINT_CYCLES = 1` caps the
/// shutdown at ONE final cycle with no fixpoint iteration — the defective
/// loop's exact shape when ring 0 is idle — so a final-cycle SMO leaves
/// the region uncovered at the leave: the page stays `Live` with its
/// roots and tail, the ring's extents stay claimed, and the next open
/// recovers the region as OWN RESIDUE (`appender_self_recoveries ≥ 1`,
/// region 1 among them) with every acked base readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leave_over_an_uncovered_window_keeps_the_page_live_for_the_next_open() {
    use squeezefs::meta_backend::kv::backend::TEST_SHUTDOWN_FIXPOINT_CYCLES;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 91);
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            TEST_SHUTDOWN_FIXPOINT_CYCLES.store(0, Ordering::SeqCst);
        }
    }
    let _cleanup = Cleanup;
    let mut hit = false;
    for rounds in 1..=8u64 {
        let uris = vec![format_stamped_member(dir.path(), &format!("u{rounds}")).await];
        let path = std::path::Path::new(&uris[0]);
        TEST_SHUTDOWN_FIXPOINT_CYCLES.store(1, Ordering::SeqCst);
        let (acked, smos_in_final) = final_cycle_shape(&uris, rounds, tag, guest_owner).await;
        TEST_SHUTDOWN_FIXPOINT_CYCLES.store(0, Ordering::SeqCst);
        if smos_in_final == 0 {
            continue;
        }
        hit = true;
        let sb = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(path)
            .await
            .unwrap()
            .superblock()
            .clone();
        let page = read_directory(path, &sb).await.unwrap()[1]
            .page
            .clone()
            .expect("appender 1 has a page");
        assert_eq!(
            page.state,
            AppenderState::Live,
            "rounds={rounds}: the leave REFUSED to free a region whose window was uncovered — \
             the page stays Live for the next open"
        );
        assert!(!page.segments.is_empty(), "its ring is still named");
        let rb = open_with_partition(&uris, Some(PARTITION)).await;
        let vb = Arc::clone(&rb.volumes[0]);
        let s = stats(&vb);
        assert!(
            s.regions[1].self_recovered && s.self_recoveries >= 1,
            "rounds={rounds}: the next open recovers appender 1 as its own residue (region 0 \
             too when the cadence left ring 0 uncovered): {s:?}"
        );
        for base in &acked {
            assert_eq!(
                vb.block_ref_count(tag, *base).await.unwrap(),
                1,
                "rounds={rounds}: acked base {base} served after the own-residue recovery"
            );
        }
        for v in &rb.volumes {
            v.shutdown().await.unwrap();
        }
        break;
    }
    assert!(hit, "no round count produced an SMO inside the final cycle");
}

// ---------------------------------------------------------------------------
// Review round 3 — Issue 21: the final tick waits for EVERY ring's head.
// ---------------------------------------------------------------------------

/// Issue 21: a declared region's window IN FLIGHT at the final tick (its
/// committer passed the gate; its ring write is on a slow device) is
/// WAITED for — the same head-stability wait ring 0 gets — so the
/// fixpoint covers it and the leave is clean (the guarantee:
/// `appender_self_recoveries == 0` at the next open), instead of burning
/// the fixpoint bound against an uncovered region and landing on the belt
/// (page `Live`, own-residue recovery — sound, but the guarantee missed).
///
/// The schedule that reaches the shape: the shutdown's own writer-claim
/// removal (ring 0) and the region commit ride ONE conveyor batch (the
/// pass parked before its drain), so the ring-0 group is handed off
/// FIRST and acks — the shutdown proceeds to its flag and the final tick
/// — while the region group's ring write is PARKED on the device
/// (`arm_write_stall`); the write is released a second later, long after
/// the pre-fix fixpoint (fast cycles on tmpfs) had burnt its 16 cycles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_region_window_in_flight_at_shutdown_is_waited_for_and_the_leave_is_clean() {
    use squeezefs::meta_backend::kv::backend::{
        test_conveyor_hold_release, TEST_CONVEYOR_HOLD_PRE_DRAIN, TEST_CONVEYOR_HOLD_STAGE,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::PathBuf::from(&uris[0]);
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 94);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let vol = Arc::clone(&routed.volumes[0]);
    vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, 0, 40))
        .await
        .unwrap();
    vol.checkpoint_now().await.unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
            test_conveyor_hold_release();
            squeezefs::uring_fs::release_write_stall(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());
    // Park appender 1's NEXT ring write (the window the region commit
    // below reserves), and the pass before its drain.
    let ring1 = vol.appenders_public().unwrap().regions[1].ring();
    let head = ring1.core().head();
    let mut arrived =
        squeezefs::uring_fs::arm_write_stall(&path, ring1.physical_offset_of(head), 8);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    // The shutdown enqueues its writer-claim removal first …
    let shutdown = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move {
            for v in &routed.volumes {
                v.shutdown().await.unwrap();
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while vol.conveyor_pending_len() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the claim removal enqueues"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // … then the region commit, behind it in the same batch.
    let committer = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move {
            vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, 5000, 40))
                .await
        })
    };
    while vol.conveyor_pending_len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the region commit enqueues"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    tokio::time::timeout(std::time::Duration::from_secs(20), arrived.recv())
        .await
        .expect("the region window's ring write reaches the stall")
        .expect("stall arrival channel");
    // The ring-0 group acked, the shutdown is at its final tick, the
    // region window is in flight: a second on the parked device.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    squeezefs::uring_fs::release_write_stall(&path);
    committer.await.unwrap().expect("the in-flight commit acks");
    tokio::time::timeout(std::time::Duration::from_secs(60), shutdown)
        .await
        .expect("shutdown completes once the window lands")
        .unwrap();
    drop(vol);
    drop(routed);
    let sb = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(&path)
        .await
        .unwrap()
        .superblock()
        .clone();
    let page = read_directory(&path, &sb).await.unwrap()[1]
        .page
        .clone()
        .expect("appender 1 has a page");
    assert_eq!(
        page.state,
        AppenderState::Free,
        "the final tick waited for the region's window: the leave was CLEAN (not the belt)"
    );
    let rb = open_with_partition(&uris, Some(PARTITION)).await;
    let vb = Arc::clone(&rb.volumes[0]);
    let s = stats(&vb);
    assert_eq!(
        s.self_recoveries, 0,
        "the guarantee held — no own residue at the next open: {s:?}"
    );
    assert_eq!(vb.block_ref_count(tag, 5000).await.unwrap(), 1);
    assert_eq!(vb.block_ref_count(tag, 0).await.unwrap(), 1);
    for v in &rb.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Review round 2 — Issues 17 and 18.
// ---------------------------------------------------------------------------

/// Issue 17: a `ReturnExtents` naming an extent whose free is PENDING on
/// the region's tail is REFUSED like a claimed one (the witness class):
/// until the tail passes the free record a crash replays through the
/// structure that still routes to the retired image (§4.7's coverage
/// gate), so the extent becomes returnable only through
/// `advance_durable` — the cadence returns it a few cycles later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_return_of_a_pending_extent_is_refused_until_the_tail_covers_its_free() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 92);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let vol = Arc::clone(&routed.volumes[0]);
    for i in 0..4u64 {
        vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, i * 100, 40))
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();
    vol.checkpoint_now().await.unwrap();
    let tree = slot_tree(&vol, 4);
    let old_root = tree.root();
    let old_ext = extent_of(&vol, old_root.addr);
    assert_eq!(
        vol.defrag_compact_nodes(&[(0, old_root.addr)])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        stats(&vol).regions[1].grant_pending,
        1,
        "the old root parked"
    );
    let refusals_before = stats(&vol).manager_verb_refusals;
    let err = vol
        .manager_return_runs(
            1,
            &[GrantRun {
                start: old_ext,
                len: 1,
            }],
        )
        .await
        .expect_err("a pending extent is not returnable");
    assert!(err.to_string().contains("pending"), "{err}");
    assert!(vol.allocator().is_allocated(old_ext), "the bit stays set");
    assert!(vol.grant_holds(1, old_ext), "it stays in the RAM grant");
    assert_eq!(stats(&vol).regions[1].grant_pending, 1);
    assert_eq!(stats(&vol).manager_verb_refusals, refusals_before + 1);
    // The tail passes the free: the cadence returns it normally.
    let mut cycles = 0;
    while vol.allocator().is_allocated(old_ext) && cycles < 6 {
        vol.checkpoint_now().await.unwrap();
        cycles += 1;
    }
    assert!(
        !vol.allocator().is_allocated(old_ext),
        "returned by the cadence within {cycles} cycles once covered"
    );
    assert_grant_closure(&stats(&vol));
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// Issue 18: the flush pass's REACTIVE refill of an exhausted grant asks
/// the compaction reserve for ONE SMO's images (`SMO_IMAGES_MAX`), never
/// the derived user grant (the floor 8): on a heap drained to its reserve
/// the refill carves at most `SMO_IMAGES_MAX` per grant, the reserve is
/// not drained by one refill, and the deferred node completes without a
/// manager stall.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reactive_refill_asks_the_reserve_for_one_smos_images_not_the_derived_grant() {
    use squeezefs::meta_backend::kv::appender::SMO_IMAGES_MAX;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0011223344556677");
    let guest_owner = guest_local_ino(3, 93);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_with_partition(&uris, Some(PARTITION)).await;
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let vol = Arc::clone(&routed.volumes[0]);
    // One leaf in the leased slot, quiesced.
    vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, 0, 40))
        .await
        .unwrap();
    vol.checkpoint_now().await.unwrap();
    vol.checkpoint_now().await.unwrap();
    // Appender 1 hands back its whole unclaimed remainder, then the
    // USER-claimable heap is drained: only the reserve is left.
    let remainder = vol.appenders_public().unwrap().regions[1]
        .grant()
        .unclaimed_runs();
    vol.manager_return_runs(1, &remainder).await.unwrap();
    assert_eq!(stats(&vol).regions[1].grant_unclaimed, 0);
    let mut drained = Vec::new();
    loop {
        match vol.allocator().claim_user() {
            Ok(e) => drained.push(e),
            Err(squeezefs::meta_backend::kv::KvError::NoSpace { .. }) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    let reserve_left = vol.free_extents();
    let s0 = stats(&vol);
    // Enough records that the leaf's flush overflows its log: the flush
    // pass's SMO meets an EMPTY grant and refills reactively.
    for i in 0..6u64 {
        vol.commit_block_refs(guest_owner, &refs(tag, guest_owner, 1000 + i * 1000, 400))
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();
    vol.checkpoint_now().await.unwrap();
    let s1 = stats(&vol);
    let grants = s1.extent_grants - s0.extent_grants;
    let carved = s1.extent_grant_extents - s0.extent_grant_extents;
    assert!(grants >= 1, "the reactive refill engaged: {s1:?}");
    assert!(
        carved <= u64::from(SMO_IMAGES_MAX) * grants,
        "{carved} extents over {grants} reactive grant(s) — more than one SMO's images \
         ({SMO_IMAGES_MAX}) per refill from the reserve"
    );
    assert!(
        carved < GRANT_EXTENTS_FLOOR * grants,
        "a reactive refill must never take the derived user grant ({GRANT_EXTENTS_FLOOR}) from \
         the reserve: {carved} over {grants}"
    );
    assert!(
        vol.free_extents() + carved >= reserve_left,
        "the reserve only paid the refills"
    );
    assert_eq!(s1.dependency_stalls, 0, "no manager stall");
    assert!(
        s1.regions[1].grant_claimed >= 1,
        "the deferred SMO completed inside the refill: {:?}",
        s1.regions[1]
    );
    for e in drained {
        vol.allocator().release_unpublished(e);
    }
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
