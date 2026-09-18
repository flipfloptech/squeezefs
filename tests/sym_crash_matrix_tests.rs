//! **Symmetric PR 10 — the crash matrix** (`docs/design-symmetric-
//! metadata.md` §5.3.4 rows, §5.5.2, §5.5.3, §5.8.3, §5.8.4, §5.8.5 C14 /
//! C15, §5.9, §6.2): every row a DETERMINISTIC kill through the existing
//! seams, and after every recovery the §5.8.4 verdict — acked-loss 0,
//! `manager_verb_refusals` 0, `fsck_findings` 0, the must-stay-0 set.
//!
//! The dead-appender fixture: a declared region (PR 2's seam) leases a
//! slot, writes its ring, and the mount is DROPPED without a shutdown
//! (the in-process kill −9 every symmetric suite uses); the region's page
//! is then re-stamped with a FOREIGN node's identity — the only word that
//! distinguishes another node's dead region from this node's own residue
//! — so the next open sees exactly what a manager sees after a peer's
//! death: a `Live` page it did not write, whose ring holds acked records
//! nobody has replayed. The death ledger names it (the S6 owner's
//! eviction in production, `record_death_with_key` here — the same
//! writer) and the driver recovers it.

mod common;

use common::sym::*;
use squeezefs::fsck::FindingId;
use squeezefs::meta_backend::kv::alloc_lease;
use squeezefs::meta_backend::kv::appender::{read_directory, AppenderIdentity, AppenderState};
use squeezefs::meta_backend::kv::backend::recovery::{
    self, mount_path_custody_gate, recover_dead_appenders_set, recovery_stats,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::ROOT_INO;
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::kv::slot_state::SlotState;
use squeezefs::meta_backend::kv::KvError;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::park_gate;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Forest slot 4 (routing slot 3) — the declared appender 1's.
const SLOT_A: ForestSlot = 4;

fn reset_process_state() {
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    squeezefs::data_alloc_bitmap::test_clear_replayed_deltas();
    squeezefs::block_grant::test_clear_free_targets();
    park_gate::test_reset();
    squeezefs::data_custody::test_clear_poison();
    squeezefs::data_grant::test_clear_custody_quarantine();
    squeezefs::membership::test_clear_death_sinks();
    squeezefs::membership::uninstall();
}

fn foreign(n: u64) -> AppenderIdentity {
    AppenderIdentity {
        node_token: 0xDEAD_0000_0000_0000 | n,
        mount_slot: 0x7000 + n as u32,
        writer_id: 0xF00D_0000 + u128::from(n),
    }
}

fn recoveries() -> u64 {
    recovery_stats().recoveries
}

fn acted() -> u64 {
    alloc_lease::DEAD_MEMBERS_ACTED.load(Ordering::Relaxed)
}

async fn tree0_state(vol: &KvMetaBackend, slot: ForestSlot) -> Option<SlotState> {
    let control = vol.forest_control_tree()?;
    control
        .lookup(&squeezefs::meta_backend::kv::slot_state::slot_state_key(
            slot,
        ))
        .await
        .unwrap()
        .map(|v| SlotState::decode(&v).unwrap())
}

async fn page_state(uri: &str, vol: &KvMetaBackend, id: u32) -> Option<AppenderState> {
    read_directory(std::path::Path::new(uri), vol.superblock())
        .await
        .unwrap()
        .iter()
        .find(|e| e.appender_id == id)
        .and_then(|e| e.page.as_ref())
        .map(|p| p.state)
}

/// The dead-region fixture on `uris` (already seeded: one directory per
/// `(volume, slot)` in `dirs`): reopen with the partition, create
/// `files` files under every directory (their records ride region 1's
/// ring on that volume), DROP the set, re-stamp every region-1 page
/// with `identity`. Returns the file names per directory.
async fn kill_with_region_one_live(
    uris: &[String],
    partition: &'static str,
    dirs: &[(usize, u64)],
    files: usize,
    identity: AppenderIdentity,
) -> Vec<Vec<(String, u64)>> {
    let routed = open_under_retry(uris, &Knobs::armed().partition(partition))
        .await
        .expect("open with the partition");
    // The declared region is a FOREIGN holder to the creator (PR 6): the
    // create in its directory ships the dentry to it over the wire and
    // the served step commits into ITS ring — the acked record whose only
    // home is the dead ring after the kill.
    let venue = HoldersVenue::stand_up(&routed, &[1]).await;
    let mut out = Vec::new();
    for (i, (_vol_idx, dir)) in dirs.iter().enumerate() {
        let mut names = Vec::new();
        for f in 0..files {
            let name = format!("d{i}_f{f}");
            let ino = routed
                .create(*dir, &name, libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .expect("create under the seeded dir")
                .ino;
            names.push((name, ino));
        }
        out.push(names);
    }
    // The kill: no shutdown, no leave — the ring windows stay.
    venue.tear_down();
    drop(routed);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    for (vol_idx, _) in dirs {
        restamp_page_identity(&uris[*vol_idx], 1, identity).await;
    }
    out
}

/// One stamped volume with a directory seeded in `slot` and the slot
/// released to tree 0 (the next open's declared region takes it).
async fn seeded_volume(dir: &std::path::Path, slot: ForestSlot) -> (Vec<String>, u64) {
    let uris = format_stamped_set_with_config(dir, 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let d = seed_dir_in_slot(&routed, 0, slot, "shared").await;
    let vol = Arc::clone(&routed.volumes[0]);
    vol.release_slot_handover(0, slot)
        .await
        .expect("release to unleased");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    (uris, d)
}

/// [`seeded_volume`] with `n` files created under the directory by the
/// manager while it holds the slot and checkpointed before the release —
/// a slot tree of several leaves ON THE DEVICE before any region takes it.
async fn seeded_volume_with_files(
    dir: &std::path::Path,
    slot: ForestSlot,
    n: usize,
) -> (Vec<String>, u64, Vec<(String, u64)>) {
    let uris = format_stamped_set_with_config(dir, 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let d = seed_dir_in_slot(&routed, 0, slot, "shared").await;
    let mut files = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("k{i:04}");
        let ino = routed
            .create(d, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("seed create")
            .ino;
        files.push((name, ino));
    }
    let vol = Arc::clone(&routed.volumes[0]);
    vol.checkpoint_now().await.unwrap();
    vol.release_slot_handover(0, slot)
        .await
        .expect("release to unleased");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    (uris, d, files)
}

/// [`seeded_volume`] with `n` REGULAR FILES preset INTO the slot under the
/// root by the manager while it holds the slot (their inode records live
/// in the slot's tree — the custody pin's objects), checkpointed before
/// the release.
async fn seeded_volume_with_slot_files(
    dir: &std::path::Path,
    slot: ForestSlot,
    n: usize,
) -> (Vec<String>, u64, Vec<(String, u64)>) {
    let uris = format_stamped_set_with_config(dir, 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let d = seed_dir_in_slot(&routed, 0, slot, "shared").await;
    let mut files = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("s{i:04}");
        let ino = seed_file_in_slot(&routed, 0, slot, &name).await;
        files.push((name, ino));
    }
    let vol = Arc::clone(&routed.volumes[0]);
    vol.checkpoint_now().await.unwrap();
    vol.release_slot_handover(0, slot)
        .await
        .expect("release to unleased");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    (uris, d, files)
}

async fn assert_all_resolve(routed: &RoutedMetaBackend, dir: u64, files: &[(String, u64)]) {
    for (name, ino) in files {
        let got = routed
            .lookup(dir, name)
            .await
            .unwrap_or_else(|e| panic!("acked file {name} lost: {e}"));
        assert_eq!(got.ino, *ino, "{name} resolves to another ino");
        routed.getattr(*ino).await.expect("its inode record");
    }
}

// ---------------------------------------------------------------------------
// §5.9 — the driver, end to end (the ledger poll's body)
// ---------------------------------------------------------------------------

/// The headline (§5.9, §5.8.4): a foreign appender dies with acked
/// records in its ring; the ledger names it; ONE poll of the manager
/// recovers the region in §5.9's order — every record readable, its
/// slot `Unleased` in tree 0 at the lease's `g` with a cursor above every
/// ino it minted, its page `Recovered`, `recovered:{X, v}` written, one
/// act on the ledger; the slot is leasable again (a create under the
/// directory first-touch acquires it); the remount never replays the
/// `Recovered` ring and still serves every byte; fsck clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_appenders_ring_is_recovered_by_the_ledger_poll_with_no_acked_loss() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(1);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 12, x).await;
    let files = &files[0];

    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    let s = vol.appender_stats().unwrap();
    // Two Live pages at mount: our own dead incarnation's page 0 (own
    // residue, recovered by the open) and the foreign page 1 (nobody's).
    assert_eq!(s.live_pages_at_mount, 2, "the foreign Live page is listed");
    assert_eq!(
        s.self_recoveries, 1,
        "page 0 was our own residue; page 1 is nobody's"
    );
    assert!(
        matches!(
            tree0_state(&vol, SLOT_A).await,
            Some(SlotState::Leased { appender_id: 1, .. })
        ),
        "tree 0 still leases the slot to the dead appender"
    );
    // Nothing recovers an appender the ledger does not name.
    let before = (recoveries(), acted());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 0);
    assert_eq!((recoveries(), acted()), before);

    // The ledger names it (the production writer's call).
    assert!(!vol.record_death_with_key(x, 9, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let r = &rep.per_volume[0].1.recovered[0];
    assert_eq!(r.appender_id, 1);
    assert_eq!(r.slots, vec![SLOT_A]);
    assert!(r.entries >= 1, "the window held the creates");
    assert_eq!((recoveries(), acted()), (before.0 + 1, before.1 + 1));

    // Zero acked loss: every create resolves and its inode reads.
    assert_all_resolve(&routed, shared, files).await;
    // Tree 0: Unleased at the lease's g, cursor above every minted ino.
    let g_lease = match tree0_state(&vol, SLOT_A).await {
        Some(SlotState::Unleased {
            g, cursor, root, ..
        }) => {
            assert!(root.addr != 0, "the recovered tree's root is named");
            let max_local = files
                .iter()
                .map(|(_, ino)| routed.route_ino(*ino).1 & ((1u64 << 40) - 1))
                .max()
                .unwrap();
            assert!(
                cursor > max_local,
                "cursor {cursor} ≤ minted local {max_local}"
            );
            g
        }
        other => panic!("tree 0 after recovery: {other:?}"),
    };
    assert!(g_lease >= 1);
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    assert!(vol.recovered_record(&x, 0).await.unwrap().is_some());
    assert!(
        vol.slot_tails(SLOT_A).await.unwrap().is_some(),
        "the death path recorded the slot's tails (PR 5's screen reads them)"
    );
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    let ph = recovery_stats().phase_ns;
    assert!(
        ph[0] + ph[1] + ph[2] + ph[3] + ph[4] + ph[5] <= ph[6] + 6,
        "phases exact-sum within the total: {ph:?}"
    );
    assert!(ph[6] > 0);

    // The slot is leasable again: a create under the directory first-
    // touch acquires it for the manager (its affinity slot).
    let fresh = routed
        .create(shared, "after_recovery", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a create in the recovered slot")
        .ino;
    assert!(matches!(
        tree0_state(&vol, SLOT_A).await,
        Some(SlotState::Leased { appender_id: 0, .. })
    ));
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;

    // The remount: the Recovered page is honoured, never replayed
    // (§5.8.3); everything acked is still there. The next ledger
    // projection finds no allocation lease naming the dead region and
    // RELEASES it — its ring back to the heap, the page Free with its id
    // and term kept (§5.5.1's Recovered-until law).
    let again = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&again.volumes[0]);
    assert_eq!(vol.appender_stats().unwrap().live_pages_at_mount, 0);
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    assert_all_resolve(&again, shared, files).await;
    assert_eq!(
        again.lookup(shared, "after_recovery").await.unwrap().ino,
        fresh
    );
    let released_before = recovery_stats().regions_released;
    let free_before = vol.allocator().free_extents();
    let rep = mount_path_custody_gate(&again).await.unwrap();
    assert_eq!(rep.recovered(), 0, "a Recovered ring is never replayed");
    assert_eq!(rep.regions_released, 1);
    assert_eq!(recovery_stats().regions_released, released_before + 1);
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Free)
    );
    assert!(
        vol.allocator().free_extents() > free_before,
        "the dead ring's extents returned to the heap"
    );
    assert_all_resolve(&again, shared, files).await;
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    shutdown(&again).await;
    reset_process_state();
}

/// C15 at the MOUNT PATH (§5.8.5): the death is recorded while the
/// manager is up, the manager dies before its poll acts; the next mount
/// finds a `Live` page the ledger names and recovers it BEFORE serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mount_path_recovers_a_ledgered_dead_appender_before_serving() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(2);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 6, x).await;
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        assert!(!routed.volumes[0]
            .record_death_with_key(x, 3, 0)
            .await
            .unwrap());
        // The manager dies with the record durable and the poll unrun.
        drop(routed);
        park_gate::test_reset();
    }
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let before = recoveries();
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1);
    assert_eq!(recoveries(), before + 1);
    assert_all_resolve(&routed, shared, &files[0]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(matches!(
        tree0_state(&vol, SLOT_A).await,
        Some(SlotState::Unleased { .. })
    ));
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// §5.5.1's row "home manager dies mid-recovery": the recovering manager
/// dies after the page went `Recovering` and before tree 0 moved — the
/// successor re-runs the recovery idempotently (per-key LWW replay; the
/// RAM table rebuilt from tree 0's `Leased{X}`), nothing acked is lost,
/// and `recovered:` lands exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_that_died_after_the_recovering_page_write_is_re_run_by_the_successor() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(3);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 8, x).await;
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        assert!(!routed.volumes[0]
            .record_death_with_key(x, 5, 0)
            .await
            .unwrap());
        recovery::TEST_RECOVERY_HALT_AFTER_RECOVERING_PAGE.store(true, Ordering::SeqCst);
        let rep = recover_dead_appenders_set(&routed).await.unwrap();
        recovery::TEST_RECOVERY_HALT_AFTER_RECOVERING_PAGE.store(false, Ordering::SeqCst);
        assert_eq!(rep.recovered(), 0);
        assert_eq!(rep.per_volume[0].1.deferred, 1, "the halt fired");
        let vol = Arc::clone(&routed.volumes[0]);
        assert_eq!(
            page_state(&uris[0], &vol, 1).await,
            Some(AppenderState::Recovering)
        );
        assert!(matches!(
            tree0_state(&vol, SLOT_A).await,
            Some(SlotState::Leased { appender_id: 1, .. })
        ));
        drop(vol);
        drop(routed);
        park_gate::test_reset();
    }
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "the successor re-ran it: {rep:?}");
    assert_all_resolve(&routed, shared, &files[0]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    assert!(vol.manager_record_recovered(x, 0).await.unwrap(), "already");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// §5.5.3 item 2's other half at the driver: a `Live` page the ledger
/// does NOT name is a joined appender (or a reclaimer in a successor's
/// grace) and is never recovered, however many polls run; a `Recovering`
/// page the ledger does not name is a shape no legal schedule writes —
/// the mount path refuses loud naming `appender clear`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unledgered_live_page_is_never_recovered_and_an_unledgered_recovering_page_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(4);
    let _ = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 3, x).await;
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let before = (recoveries(), acted());
    for _ in 0..3 {
        assert_eq!(
            recover_dead_appenders_set(&routed)
                .await
                .unwrap()
                .recovered(),
            0
        );
    }
    assert_eq!((recoveries(), acted()), before);
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Live)
    );
    let census = vol.slot_custody_census(Some(&vol)).await.unwrap();
    assert!(census.conflicts.is_empty() && census.unrecovered.is_empty());
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    // Plant the unledgered Recovering shape.
    rewrite_page(&uris[0], 1, |p| p.state = AppenderState::Recovering).await;
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    match mount_path_custody_gate(&routed).await {
        Err(KvError::Corrupt(m)) => {
            assert!(m.contains("RECOVERING"), "{m}");
            assert!(m.contains("appender clear"), "{m}");
        }
        other => panic!("an unledgered Recovering page was admitted: {other:?}"),
    }
    let census = routed.volumes[0]
        .slot_custody_census(Some(&routed.volumes[0]))
        .await
        .unwrap();
    assert_eq!(census.recovering_unledgered.len(), 1);
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.5.2 / §5.5.3 — the ledger's WRITER: eviction and grace
// ---------------------------------------------------------------------------

/// The production writer: the S6 owner's eviction past `T_owner` ships
/// the member's death — with the registrant key its join presented — to
/// volume 0's manager (the installed sink), and the record lands; a
/// successor's grace window records a NON-RECLAIMER's death at the
/// deadline and never a reclaimer's (§5.5.3 item 2 — "only after grace",
/// "no region recovered for a reclaimer"). Eight home-shard members: 7
/// reclaim, 1 does not — exactly one death record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owners_eviction_and_the_grace_deadline_write_the_ledger_never_a_reclaimer() {
    use squeezefs::membership::{
        JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    recovery::install_death_ledger_writer(&routed);
    let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
    let clock = LeaseClock::manual(Arc::clone(&now));
    let clocks = LeaseClocks::derive(std::time::Duration::ZERO).unwrap();
    let t_owner = clocks.t_owner.as_millis() as u64;
    let grace = clocks.grace.as_millis() as u64;
    let join = |n: u64, prior: Option<u64>| JoinRequest {
        id: squeezefs::cowriter::node_member_id_of(0xA11C_0000_0000_0000 | n, 0x3000 + n as u32),
        role: MemberRole::Writer,
        endpoint: None,
        pid: 1,
        boot: "b".into(),
        prior_epoch: prior,
        pr_key: 0x5100 + n,
        mount: None,
    };
    let ident = |n: u64| AppenderIdentity {
        node_token: 0xA11C_0000_0000_0000 | n,
        mount_slot: 0x3000 + n as u32,
        writer_id: 0,
    };
    let dead_records =
        |vol: Arc<KvMetaBackend>| async move { vol.dead_member_records().await.unwrap() };

    // (a) A live owner evicts a member past T_owner: one record, keyed.
    let owner = MembershipOwner::arm("o1", 1, 0, clocks.clone(), clock.clone()).unwrap();
    squeezefs::membership::install_owner(Arc::clone(&owner));
    let JoinOutcome::Granted(gr) = owner.join(join(1, None)) else {
        panic!("join")
    };
    now.fetch_add(t_owner + 1, Ordering::SeqCst);
    let evicted = owner.expire_due();
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].pr_key, 0x5101);
    assert_eq!(evicted[0].epoch, gr.epoch);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let recs = dead_records(Arc::clone(&vol)).await;
        if recs.iter().any(|(id, r)| {
            id.node_token == ident(1).node_token
                && id.mount_slot == ident(1).mount_slot
                && r.pr_key == 0x5101
                && r.epoch == gr.epoch
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sink never wrote: {recs:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // (b) A successor's grace over eight prior members: 7 reclaim, 1 dies
    // at the deadline — after grace, with its claim-set key.
    let succ = MembershipOwner::arm("o2", 2, 1, clocks.clone(), clock.clone()).unwrap();
    squeezefs::membership::install_owner(Arc::clone(&succ));
    let expected: Vec<(String, u64)> = (10..18u64)
        .map(|n| (join(n, None).id, 0x5100 + n))
        .collect();
    succ.open_grace_with_keys(expected);
    assert!(succ.grace_active());
    for n in 10..17u64 {
        let JoinOutcome::Granted(_) = succ.join(join(n, Some(1))) else {
            panic!("reclaim {n} refused inside grace")
        };
    }
    let before: usize = dead_records(Arc::clone(&vol)).await.len();
    now.fetch_add(grace + 1, Ordering::SeqCst);
    assert!(!succ.grace_active(), "the window closed on its deadline");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let recs = dead_records(Arc::clone(&vol)).await;
        let named17 = recs.iter().any(|(id, r)| {
            id.node_token == ident(17).node_token && r.pr_key == 0x5100 + 17 && r.epoch == 0
        });
        if named17 {
            assert_eq!(
                recs.len(),
                before + 1,
                "exactly the non-reclaimer: {recs:?}"
            );
            for n in 10..17u64 {
                assert!(
                    !recs
                        .iter()
                        .any(|(id, _)| id.node_token == ident(n).node_token),
                    "reclaimer {n} was recorded dead"
                );
            }
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the grace deadline never wrote: {recs:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // (c) The wire screen: a member the owner lists LIVE is never declared
    // dead by a peer's word; this mount's own identity neither.
    let JoinOutcome::Granted(_) = succ.join(join(20, None)) else {
        panic!("join")
    };
    match vol.screen_record_death(&ident(20).into(), 0) {
        Err(KvError::Rejected(m)) => assert!(m.contains("LIVE"), "{m}"),
        other => panic!("a live member's death was admitted: {other:?}"),
    }
    let me = vol.appenders_public().unwrap().identity;
    match vol.screen_record_death(&me.into(), 0) {
        Err(KvError::Rejected(m)) => assert!(m.contains("own identity"), "{m}"),
        other => panic!("our own death was admitted: {other:?}"),
    }
    // (d) The KEY word's screen (review round 1, Issue 7 — the wire-word
    // law over `pr_key`, the word that drives a PREEMPT): a departed
    // member's word must match the key the census REGISTERED for it
    // (evicted 17 registered 0x5100 + 17) — a contradiction is REJECTED,
    // nothing written; a live member's key (20's) and this process's own
    // key are rejected whatever member they ride; a member no census here
    // knows (99) has its key DROPPED to 0 — recorded, the tail scan the
    // fence — while a zero word is accepted verbatim.
    let rejected_before = vol.appender_stats().unwrap().manager_verb_rejected;
    assert_eq!(
        vol.screen_record_death(&ident(17).into(), 0x5100 + 17)
            .unwrap(),
        0x5100 + 17,
        "the registered key is stored as carried"
    );
    match vol.screen_record_death(&ident(17).into(), 0xBAD) {
        Err(KvError::Rejected(m)) => assert!(m.contains("contradicts"), "{m}"),
        other => panic!("a contradicting key word was admitted: {other:?}"),
    }
    match vol.screen_record_death(&ident(17).into(), 0x5100 + 20) {
        Err(KvError::Rejected(m)) => assert!(m.contains("live member"), "{m}"),
        other => panic!("a LIVE member's key was admitted on a dead member: {other:?}"),
    }
    match vol.screen_record_death(&ident(17).into(), vol.test_pr_key()) {
        Err(KvError::Rejected(_)) => {}
        other => panic!("this process's own key was admitted: {other:?}"),
    }
    assert_eq!(
        vol.appender_stats().unwrap().manager_verb_rejected,
        rejected_before + 3,
        "every key-word rejection is counted"
    );
    assert_eq!(
        vol.screen_record_death(&ident(99).into(), 0x7777).unwrap(),
        0,
        "an unknown member's key is dropped, never stored"
    );
    assert_eq!(vol.screen_record_death(&ident(99).into(), 0).unwrap(), 0);
    // The pure screen the wire's fuzz arm drives: the same verdicts.
    use squeezefs::meta_backend::kv::backend::recovery::{screen_death_key, DeathKeyVerdict};
    assert_eq!(
        screen_death_key(7, Some(7), &[1], &[2]),
        DeathKeyVerdict::Accept
    );
    assert_eq!(
        screen_death_key(8, Some(7), &[1], &[2]),
        DeathKeyVerdict::Reject
    );
    assert_eq!(
        screen_death_key(1, Some(1), &[1], &[2]),
        DeathKeyVerdict::Reject,
        "our own key is refused even when the census agrees"
    );
    assert_eq!(
        screen_death_key(2, None, &[1], &[2]),
        DeathKeyVerdict::Reject
    );
    assert_eq!(
        screen_death_key(9, None, &[1], &[2]),
        DeathKeyVerdict::Unvalidated
    );
    assert_eq!(screen_death_key(0, None, &[], &[]), DeathKeyVerdict::Accept);
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.9 — multi-volume, multi-death, the empty-window wire lessee
// ---------------------------------------------------------------------------

/// The forest slot of the first routing slot in `from..` that the set
/// hosts on volume `vol_idx`.
fn hosted_slot_on(routed: &RoutedMetaBackend, vol_idx: usize, from: u16) -> ForestSlot {
    use squeezefs::meta_backend::make_global_ino_width;
    let width = routed.routing_width();
    for r in from..from + 4096 {
        let ino = make_global_ino_width(2, u64::from(r), width);
        if routed.route_ino(ino).0 == vol_idx {
            return ForestSlot::from(r) + 1;
        }
    }
    panic!("volume {vol_idx} hosts no routing slot in {from}..");
}

/// §5.5.2's multi-volume leg (§8 gate 4 (f)): a node holding a region on
/// THREE volumes dies; ONE death record; ONE projection recovers all
/// three rings (parallel across managers in a fleet, serial here), every
/// volume's `recovered:{X, v}` written, three acts on the ledger
/// (`acted ≡ recorded × regions held`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_holding_regions_on_three_volumes_is_recovered_on_all_three() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 3).await;
    let (slots, dirs, partition) = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let slots: Vec<ForestSlot> = (0..3).map(|v| hosted_slot_on(&routed, v, 3)).collect();
        let mut dirs = Vec::new();
        for (v, slot) in slots.iter().enumerate() {
            dirs.push((
                v,
                seed_dir_in_slot(&routed, v, *slot, &format!("shared{v}")).await,
            ));
            routed.volumes[v]
                .release_slot_handover(0, *slot)
                .await
                .expect("release to unleased");
        }
        shutdown(&routed).await;
        let partition: &'static str =
            Box::leak(format!("1:{},{},{}", slots[0], slots[1], slots[2]).into_boxed_str());
        (slots, dirs, partition)
    };
    let x = foreign(30);
    let files = kill_with_region_one_live(&uris, partition, &dirs, 4, x).await;
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol0 = Arc::clone(&routed.volumes[0]);
    let (rec0, act0) = (recoveries(), acted());
    assert!(!vol0.record_death_with_key(x, 1, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 3, "{rep:?}");
    assert_eq!((recoveries(), acted()), (rec0 + 3, act0 + 3));
    for v in 0..3u16 {
        assert!(
            vol0.recovered_record(&x, v).await.unwrap().is_some(),
            "recovered:{{X, {v}}}"
        );
        let vol = &routed.volumes[v as usize];
        assert!(matches!(
            tree0_state(vol, slots[v as usize]).await,
            Some(SlotState::Unleased { .. })
        ));
        assert_eq!(
            page_state(&uris[v as usize], vol, 1).await,
            Some(AppenderState::Recovered)
        );
        assert_all_resolve(&routed, dirs[v as usize].1, &files[v as usize]).await;
        assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    }
    shutdown(&routed).await;
    drop(vol0);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// Eight appenders die at once (§8 gate 4 (e)): seven declared regions
/// with windows (the directory's first extent holds seven pairs at 64 KiB
/// nodes — the eighth would be `JoinAppender`'s chain growth) plus one
/// wire joiner (an empty window), eight foreign identities, eight
/// records; ONE projection recovers them all, every file of every region
/// resolves, eight acts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_simultaneous_deaths_are_recovered_by_one_projection() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerSetService};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Eight appenders need `appenders_capacity ≥ 8` — heap/16 ÷ ring — so
    // the member is 128 MiB (the 64 MiB fixture admits seven).
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 128 * 1024 * 1024).await;
    // Seven regions on seven slots the rotor never takes (routing ≥ 100).
    let nreg: u32 = 7;
    let slots: Vec<ForestSlot> = (0..nreg).map(|i| 101 + i * 3).collect();
    let dirs = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let mut dirs = Vec::new();
        for (i, slot) in slots.iter().enumerate() {
            dirs.push(seed_dir_in_slot(&routed, 0, *slot, &format!("d{i}")).await);
            routed.volumes[0]
                .release_slot_handover(0, *slot)
                .await
                .expect("release to unleased");
        }
        shutdown(&routed).await;
        dirs
    };
    let partition: &'static str = Box::leak(
        slots
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}:{s}", i + 1))
            .collect::<Vec<_>>()
            .join(";")
            .into_boxed_str(),
    );
    let routed = open_under_retry(&uris, &Knobs::armed().partition(partition))
        .await
        .unwrap();
    let ids: Vec<u32> = (1..=nreg).collect();
    let venue = HoldersVenue::stand_up(&routed, &ids).await;
    let mut files: Vec<Vec<(String, u64)>> = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let mut names = Vec::new();
        for f in 0..3 {
            let name = format!("r{i}_f{f}");
            let ino = routed
                .create(*d, &name, libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .expect("create")
                .ino;
            names.push((name, ino));
        }
        files.push(names);
    }
    venue.tear_down();
    // The eighth: a wire joiner on the same volume, one slot, no window.
    let host = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        VENUE_SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(ManagerSetService::new(&routed.volumes))),
    )
    .unwrap();
    let endpoint8 = host.endpoint().to_string();
    let mut j = ManagerClient::connect(&endpoint8, VENUE_SECRET, "joiner-8", 0)
        .await
        .unwrap();
    let jid = foreign(48);
    let reply = j.join(jid, 0).await.unwrap();
    let ManagerReply::Joined {
        appender_id: wire_id,
        ..
    } = reply
    else {
        panic!("join: {reply:?}")
    };
    match j.acquire_slot(wire_id, 300).await.unwrap() {
        ManagerReply::SlotsGranted { .. } => {}
        other => panic!("{other:?}"),
    }
    // The listener holds the volumes through its service: shut AND drop
    // it (and the client) before the kill, or the flock outlives the set.
    host.shutdown();
    drop(host);
    drop(j);
    drop(routed);
    park_gate::test_reset();
    for id in &ids {
        restamp_page_identity(&uris[0], *id, foreign(40 + u64::from(*id))).await;
    }
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    let (rec0, act0) = (recoveries(), acted());
    for id in &ids {
        assert!(!vol
            .record_death_with_key(foreign(40 + u64::from(*id)), 2, 0)
            .await
            .unwrap());
    }
    assert!(!vol.record_death_with_key(jid, 2, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 8, "{rep:?}");
    assert_eq!((recoveries(), acted()), (rec0 + 8, act0 + 8));
    for (i, d) in dirs.iter().enumerate() {
        assert_all_resolve(&routed, *d, &files[i]).await;
        assert!(matches!(
            tree0_state(&vol, slots[i]).await,
            Some(SlotState::Unleased { .. })
        ));
    }
    assert!(matches!(
        tree0_state(&vol, 301).await,
        Some(SlotState::Unleased { g: 1, .. })
    ));
    assert_eq!(
        page_state(&uris[0], &vol, wire_id).await,
        Some(AppenderState::Recovered)
    );
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// A dead WIRE lessee (an empty window — a wire appender journals
/// nothing in PR 4): the ledger record arrives OVER THE WIRE
/// (`RecordDeath`, PR 8's reservation activated — served, idempotent);
/// the poll releases its slots to tree 0 at their `g`, releases the
/// set-wide directory-rename lock it held (§5.6.4's expiry law — PR 6's
/// owed driver), returns its unclaimed grant and marks its page
/// `Recovered`; a live member the owner lists is never declared dead by
/// the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_wire_lessees_slots_grant_and_dir_rename_lock_are_released_by_the_ledger() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerSetService};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let host = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        VENUE_SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(ManagerSetService::new(&routed.volumes))),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let mut j = ManagerClient::connect(&endpoint, VENUE_SECRET, "joiner-j", 0)
        .await
        .unwrap();
    let jid = foreign(50);
    let ManagerReply::Joined { appender_id, .. } = j.join(jid, 0).await.unwrap() else {
        panic!("join")
    };
    let routing: u16 = 120;
    let fslot = ForestSlot::from(routing) + 1;
    match j.acquire_slot(appender_id, routing).await.unwrap() {
        ManagerReply::SlotsGranted { slots, .. } => assert_eq!(slots[0].g, 1),
        other => panic!("{other:?}"),
    }
    match j.dir_rename_lock(appender_id).await.unwrap() {
        ManagerReply::DirRenameLocked { already } => assert!(!already),
        other => panic!("{other:?}"),
    }
    let grant_before = vol.extent_grant_record(appender_id).await.unwrap();
    assert!(!grant_before.is_empty(), "the join minted a grant");
    // The wire screen: a live member is never declared dead by a peer's
    // word; this mount's own identity neither.
    {
        use squeezefs::membership::{
            JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
        };
        let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let owner = MembershipOwner::arm(
            "o",
            1,
            0,
            LeaseClocks::derive(std::time::Duration::ZERO).unwrap(),
            LeaseClock::manual(Arc::clone(&now)),
        )
        .unwrap();
        squeezefs::membership::install_owner(Arc::clone(&owner));
        let live = foreign(51);
        let JoinOutcome::Granted(_) = owner.join(JoinRequest {
            id: squeezefs::cowriter::node_member_id_of(live.node_token, live.mount_slot),
            role: MemberRole::Writer,
            endpoint: None,
            pid: 1,
            boot: "b".into(),
            prior_epoch: None,
            pr_key: 0,
            mount: None,
        }) else {
            panic!("join")
        };
        let rejected = vol.appender_stats().unwrap().manager_verb_rejected;
        assert!(
            j.record_death(live.into(), 1, 0).await.is_err(),
            "a live member"
        );
        let me = vol.appenders_public().unwrap().identity;
        assert!(
            j.record_death(me.into(), 1, 0).await.is_err(),
            "our own identity"
        );
        assert_eq!(
            vol.appender_stats().unwrap().manager_verb_rejected,
            rejected + 2
        );
        squeezefs::membership::uninstall();
    }
    // The death, over the wire, idempotent.
    let recorded = alloc_lease::DEAD_MEMBERS_RECORDED.load(Ordering::Relaxed);
    assert!(!j.record_death(jid.into(), 4, 0x77).await.unwrap());
    assert!(
        j.record_death(jid.into(), 4, 0x77).await.unwrap(),
        "already"
    );
    assert_eq!(
        alloc_lease::DEAD_MEMBERS_RECORDED.load(Ordering::Relaxed),
        recorded + 1
    );
    // The key word (review round 1, Issue 7): no census here registered
    // the joiner (the owner is uninstalled), so the wire's 0x77 is
    // UNVALIDATED — recorded as 0 (the tail scan is the fence), never a
    // key a preempt would act on.
    assert_eq!(
        vol.dead_member_record(&jid).await.unwrap().unwrap().pr_key,
        0
    );
    let (rec0, act0) = (recoveries(), acted());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let r = &rep.per_volume[0].1.recovered[0];
    assert_eq!((r.appender_id, r.entries), (appender_id, 0));
    assert_eq!(r.slots, vec![fslot]);
    assert_eq!((recoveries(), acted()), (rec0 + 1, act0 + 1));
    match tree0_state(&vol, fslot).await {
        Some(SlotState::Unleased { g, .. }) => assert_eq!(g, 1, "g never moves at a release"),
        other => panic!("{other:?}"),
    }
    assert!(
        vol.dir_rename_record().await.unwrap().is_none(),
        "the dead holder's lock released"
    );
    assert_eq!(
        page_state(&uris[0], &vol, appender_id).await,
        Some(AppenderState::Recovered)
    );
    assert!(
        vol.extent_grant_record(appender_id)
            .await
            .unwrap()
            .is_empty(),
        "the unclaimed grant returned"
    );
    assert!(vol.recovered_record(&jid, 0).await.unwrap().is_some());
    // The slot is leasable: the manager's own explicit ask lands at g + 1.
    let g = vol
        .manager_acquire_slots(
            0,
            0,
            &[fslot],
            squeezefs::meta_backend::kv::backend::ControlAdmit::Try,
        )
        .await
        .unwrap();
    assert_eq!(g[0].g, 2);
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    host.shutdown();
    drop(host);
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.9's last note — the manager-death composition; the vol-0 rule
// ---------------------------------------------------------------------------

/// `set_wide_roles_resume_after_a_volume_0_manager_failover`: volume 0's
/// manager dies holding the set-wide directory-rename lock, with a file
/// acked in its ring; the successor wins the D0 ladder (PR 3's arm
/// adopts the foreign page 0 and replays the dead manager's ring), the
/// mount-path gate releases the dead incarnation's lock, and every set-
/// wide role answers on the successor: the manager lease held on both
/// volumes, the coordinator predicate open, the ledger writable, the
/// acked file present.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_wide_roles_resume_after_a_volume_0_manager_failover() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 2).await;
    let (acked, term_before) = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol0 = Arc::clone(&routed.volumes[0]);
        let ino = routed
            .create(
                ROOT_INO,
                "acked_before_the_death",
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
            .unwrap()
            .ino;
        let term = vol0.writer_term();
        // The manager dies holding the lock — the record as a dead
        // incarnation leaves it (the RAII lease would release it on its
        // drop, and a held one keeps the backend alive through the kill).
        vol0.test_plant_dir_rename_record(0, term).await.unwrap();
        drop(vol0);
        drop(routed);
        park_gate::test_reset();
        (ino, term)
    };
    // Another NODE's dead manager: page 0 of every volume re-stamped.
    for uri in &uris {
        restamp_page_identity(uri, 0, foreign(60)).await;
    }
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol0 = Arc::clone(&routed.volumes[0]);
    assert!(vol0.writer_term() > term_before, "the successor's era");
    let held = vol0
        .dir_rename_record()
        .await
        .unwrap()
        .expect("the dead manager's lock stands");
    assert_eq!(held.holder, 0);
    assert!(held.term < vol0.writer_term());
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(
        rep.recovered(),
        0,
        "page 0 passed with the role, not through the ledger"
    );
    assert!(
        vol0.dir_rename_record().await.unwrap().is_none(),
        "released at the successor's arm"
    );
    for v in &routed.volumes {
        assert_eq!(v.appender_stats().unwrap().manager_lease.word(), "held");
        assert!(!v.manager_role_released());
    }
    assert!(alloc_lease::symmetric_coordinator_refusal().is_none());
    assert!(
        !vol0.record_death_with_key(foreign(61), 1, 0).await.unwrap(),
        "the ledger writes"
    );
    assert_eq!(
        routed
            .lookup(ROOT_INO, "acked_before_the_death")
            .await
            .unwrap()
            .ino,
        acked
    );
    shutdown(&routed).await;
    reset_process_state();
}

/// §5.5.3 item 2 — the parked member's successor observation, its
/// PRODUCTION writer (PR 8 left it a seam): the home volume's rendezvous
/// record naming an owner at a NEWER era than the one the member joined
/// under IS the successor, and its endpoint becomes the parked reclaim's
/// venue; the joined era's record (the owner, merely slow) and a stale
/// predecessor's observe nothing. (Every member parking and reclaiming
/// against it is PR 8's `a_parked_member_reclaims_against_the_successor_
/// the_ledger_names`; the 8-member storm on N daemons is PR 12's venue.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_members_reclaim_venue_is_the_successor_the_home_volumes_rendezvous_names() {
    use squeezefs::membership::{
        note_successor_observed, observe_successor, publish_owner_record, successor_endpoint,
        OwnerRecord,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    note_successor_observed(None);
    let rec = |term: u64, endpoint: &str| OwnerRecord {
        v: 1,
        id: format!("owner-{term}"),
        term,
        endpoint: endpoint.to_string(),
        ttl_ms: 45_000,
        owner_claim_id: String::new(),
        ts: alloc_lease::unix_now_ms() / 1_000,
        pid: std::process::id(),
        boot: String::new(),
    };
    // The owner we joined under (era 7): nothing to observe.
    publish_owner_record(&vol, &rec(7, "127.0.0.1:7001"))
        .await
        .unwrap();
    assert_eq!(observe_successor(&vol, 7).await, None);
    assert_eq!(successor_endpoint(), None);
    // A stale predecessor's record (era 5): nothing.
    publish_owner_record(&vol, &rec(5, "127.0.0.1:5001"))
        .await
        .unwrap();
    assert_eq!(observe_successor(&vol, 7).await, None);
    assert_eq!(successor_endpoint(), None);
    // The successor (era 8) at a new venue: observed, the venue in force.
    publish_owner_record(&vol, &rec(8, "127.0.0.1:8001"))
        .await
        .unwrap();
    assert_eq!(
        observe_successor(&vol, 7).await.as_deref(),
        Some("127.0.0.1:8001")
    );
    assert_eq!(successor_endpoint().as_deref(), Some("127.0.0.1:8001"));
    // Idempotent at the next beat.
    assert_eq!(
        observe_successor(&vol, 7).await.as_deref(),
        Some("127.0.0.1:8001")
    );
    note_successor_observed(None);
    shutdown(&routed).await;
    reset_process_state();
}

/// §5.5.2's vol-0 rule composed with the D0 ladder: a manager whose probe
/// of volume 0's ledger fails for longer than `T_owner` RELEASES its role
/// (PR 8's decision) and, from then, STOPS refreshing its writer claim —
/// the claim ages past the TTL and the ladder re-elects a successor that
/// can read the ledger (PR 10's successor ladder IS the D0 ladder).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manager_that_released_its_role_under_the_vol0_rule_stops_heartbeating() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 2).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol1 = Arc::clone(&routed.volumes[1]);
    let t_owner = 45_000u64;
    assert!(!vol1.note_vol0_ledger_probe(false, 1_000, t_owner));
    assert!(
        vol1.note_vol0_ledger_probe(false, 1_000 + t_owner + 1, t_owner),
        "released"
    );
    assert!(vol1.manager_role_released());
    assert_eq!(
        vol1.appender_stats().unwrap().manager_lease.word(),
        "vacant"
    );
    let ts0 = vol1.read_writer_claim().await.unwrap().ts;
    // Two heartbeat beats a second apart: the claim's timestamp never
    // moves (the heartbeat's stamp is whole seconds).
    vol1.guard_heartbeat().await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    vol1.guard_heartbeat().await;
    let ts1 = vol1.read_writer_claim().await.unwrap().ts;
    assert_eq!(ts0, ts1, "a released manager refreshes no claim");
    // Volume 0's own manager keeps heartbeating: its beat moves the stamp.
    let vol0 = Arc::clone(&routed.volumes[0]);
    let t0 = vol0.read_writer_claim().await.unwrap().ts;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    vol0.guard_heartbeat().await;
    assert!(vol0.read_writer_claim().await.unwrap().ts > t0);
    assert!(!vol0.manager_role_released());
    shutdown(&routed).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §6.2 — `squeezefs appender clear`, the attested offline remedy
// ---------------------------------------------------------------------------

/// The `claim clear` law for an appender page: a live-mounted volume
/// refuses; a fresh heartbeat of the appender's node refuses (it may be
/// alive); this node's own residue refuses (the next mount recovers it
/// itself); otherwise the attestation writes the death record to volume
/// 0's ledger and marks the page `Recovering`, and the next mount's gate
/// recovers the window before serving — the operator's path for a death
/// the membership plane never recorded, and C14's remedy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appender_clear_refuses_a_live_lease_and_attests_a_dead_one() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(70);
    let member = squeezefs::cowriter::node_member_id_of(x.node_token, x.mount_slot);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 5, x).await;
    let path = std::path::Path::new(&uris[0]);
    let clear = || KvMetaBackend::appender_clear(path, path, 1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // (0) The manager's claim went stale ONE second ago (PR 12b review
    // round 1, Issue 4(ii)): a live JOINED appender may be PARKED at
    // `T_self` reclaiming against the successor — the verb refuses inside
    // the derived `T_park_max`, naming it; a clear there would recover a
    // live writer's ring.
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(
        squeezefs::fuse_client::CLIENT_STALE_TTL_SECS + 1,
        Ordering::SeqCst,
    );
    let parked_window = clear().await;
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(0, Ordering::SeqCst);
    match parked_window {
        Err(KvError::Busy(m)) => assert!(m.contains("T_park_max"), "{m}"),
        other => panic!("cleared inside the parked-joiner window: {other:?}"),
    }
    // (a) A fresh heartbeat of the appender's node: refused.
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        routed.volumes[0]
            .setxattr_internal(
                1,
                &format!("client:{member}"),
                format!("{{\"ts\":{now},\"pid\":7}}").as_bytes(),
            )
            .await
            .unwrap();
        // (b) A live-mounted volume: refused.
        match clear().await {
            Err(KvError::Busy(m)) => assert!(m.contains("live-mounted"), "{m}"),
            other => panic!("cleared under a live mount: {other:?}"),
        }
        shutdown(&routed).await;
    }
    match clear().await {
        Err(KvError::Busy(m)) => assert!(m.contains("heartbeat"), "{m}"),
        other => panic!("cleared a heartbeating node: {other:?}"),
    }
    // (c) This node's own residue: refused (its own mount recovers it).
    match KvMetaBackend::appender_clear(path, path, 0).await {
        Ok(recovery::AppenderClearOutcome::NothingToClear) => {}
        Err(KvError::Busy(m)) => assert!(m.contains("OWN residue"), "{m}"),
        other => panic!("{other:?}"),
    }
    // (d) The heartbeat aged past the TTL: the attestation lands.
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        routed.volumes[0]
            .setxattr_internal(
                1,
                &format!("client:{member}"),
                format!("{{\"ts\":{},\"pid\":7}}", now - 3_600).as_bytes(),
            )
            .await
            .unwrap();
        shutdown(&routed).await;
    }
    let runs = recovery_stats().clear_runs;
    match clear().await {
        Ok(recovery::AppenderClearOutcome::Cleared {
            identity,
            was,
            window_entries,
        }) => {
            assert_eq!(
                (identity.node_token, identity.mount_slot),
                (x.node_token, x.mount_slot)
            );
            assert_eq!(was, AppenderState::Live);
            assert!(window_entries >= 1);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(recovery_stats().clear_runs, runs + 1);
    // Idempotent: the page is Recovering now, the record present.
    match clear().await {
        Ok(recovery::AppenderClearOutcome::Cleared { was, .. }) => {
            assert_eq!(was, AppenderState::Recovering)
        }
        other => panic!("{other:?}"),
    }
    // The next mount's gate recovers it before serving.
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(vol.dead_member_record(&x).await.unwrap().is_some());
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1);
    assert_all_resolve(&routed, shared, &files[0]).await;
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// **Review round 7, Issue 34 — the custody QUARANTINE on the death
/// path.** The plane's own recorders (the S6 eviction, the grace
/// deadline) cannot write a death record before `T_owner`, and a writer
/// holding custody from the dead holder self-fences at `T_self <
/// T_owner` — so a slot they recover grants fresh custody at once (the
/// seam-(b) pin's tail). The two EARLY recorders — `squeezefs appender
/// clear` (the operator's attestation) and PR 8's same-node takeover —
/// can run INSIDE a surviving writer's `T_self`, and the recovered slot's
/// arbiter granting a NEW writer then would put two custody holders on
/// one file (PR 9 Issue 20's class, which the handover defers for). The
/// record carries its recorder class; a slot recovered from an EARLY
/// record is QUARANTINED until `ts_ms + T_self`: both grant paths refuse
/// the retryable class (`Refused { EAGAIN }` locally, `CUSTODY_DEFERRED`
/// on the wire) naming the quarantine, then grant. The surviving writer
/// is the premise (its grant lived in the dead holder's process); the
/// observable is the arbiter's window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_early_death_record_quarantines_the_recovered_slots_custody_for_t_self() {
    use squeezefs::data_grant::{self, WriteCustodyOwner};
    use squeezefs::membership::{LeaseClock, LeaseClocks};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared, seeded) = seeded_volume_with_slot_files(dir.path(), SLOT_A, 2).await;
    let x = foreign(71);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 5, x).await;
    let path = std::path::Path::new(&uris[0]);
    // The killed manager's claim is heartbeat-fresh: one clean mount cycle
    // of this node releases it (the verb refuses a fresh claim by law).
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        shutdown(&routed).await;
    }
    // The operator's attestation — an EARLY record.
    let t0 = squeezefs::meta_backend::kv::alloc_lease::unix_now_ms();
    match KvMetaBackend::appender_clear(path, path, 1).await {
        Ok(recovery::AppenderClearOutcome::Cleared { was, .. }) => {
            assert_eq!(was, AppenderState::Live)
        }
        other => panic!("{other:?}"),
    }
    // The next mount: PR 9's custody owner + slot-custody arm (the
    // recovered slot's arbiter), short clocks so the window is measurable.
    let _hygiene = CustodyArmGuard;
    let clocks = LeaseClocks::with_params(
        std::time::Duration::from_millis(3_000),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(400),
    )
    .expect("2*skew + purge < TTL");
    // The bound is the OWNER's: `T_owner + 2 × skew_max` past the record
    // (round 8, Issue 36 — never the member's stricter `T_self`).
    let bound_ms = data_grant::custody_quarantine_bound_for(&clocks);
    assert!(bound_ms > clocks.t_self.as_millis() as u64);
    let owner = WriteCustodyOwner::arm(
        "recoverer",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        LeaseClock::monotonic(),
        None,
    )
    .expect("the recoverer's custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&owner));
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    let sink = Arc::new(NoopRecallSink);
    data_grant::arm_slot_custody(
        &routed,
        "recoverer",
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    let rec = vol
        .dead_member_record(&x)
        .await
        .unwrap()
        .expect("the attestation's record");
    assert!(rec.early, "appender clear writes an EARLY record");
    assert!(rec.ts_ms >= t0);
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&routed, shared, &files[0]).await;
    assert_all_resolve(&routed, ROOT_INO, &seeded).await;
    // The recovered slot is the manager's, yet its custody is quarantined:
    // a fresh acquire on a file OF the slot refuses the retryable class
    // naming the quarantine — not the local arbiter's grant.
    let (_, ino) = seeded[0];
    let (_, local) = routed.route_ino(ino);
    assert_eq!(
        squeezefs::meta_backend::kv::record::forest_slot_of_ino(local),
        SLOT_A
    );
    assert_eq!(vol.foreign_slot_holder(local), None, "the slot is ours");
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let (refusals0, live) = data_grant::custody_quarantine_stats();
    assert_eq!(live, 1, "one slot under quarantine");
    let err = dlm
        .acquire_lock(
            &squeezefs::keys::inode_path(ino),
            None,
            std::time::Duration::from_millis(500),
        )
        .await
        .expect_err("custody of a quarantined slot's file is refused");
    match &err {
        squeezefs::error::SqueezefsError::Refused { errno, msg } => {
            assert_eq!(*errno, libc::EAGAIN, "{err:?}");
            assert!(msg.contains("quarantined"), "{msg}");
        }
        other => panic!("the quarantine's refusal is typed EAGAIN: {other:?}"),
    }
    assert!(data_grant::custody_quarantine_stats().0 > refusals0);
    assert!(
        squeezefs::meta_backend::kv::alloc_lease::unix_now_ms() < rec.ts_ms + bound_ms,
        "premise: the refusal happened inside the bound"
    );
    // The quarantine is DURABLE (Issue 36): the record beside the slot's
    // `Unleased` names the same deadline the RAM word holds.
    let control = vol.forest_control_tree().expect("a forest volume");
    let durable = control
        .lookup(&squeezefs::meta_backend::kv::slot_state::custody_quarantine_key(SLOT_A))
        .await
        .unwrap()
        .expect("the quarantine record beside the slot's Unleased");
    assert_eq!(
        squeezefs::meta_backend::kv::slot_state::decode_custody_quarantine(&durable).unwrap(),
        rec.ts_ms + bound_ms,
        "the durable deadline is the record's ts + the owner-side bound"
    );
    // Then granted — once the bound has elapsed since the record.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let lease = loop {
        match dlm
            .acquire_lock(
                &squeezefs::keys::inode_path(ino),
                None,
                std::time::Duration::from_millis(500),
            )
            .await
        {
            Ok(lease) => break lease,
            Err(squeezefs::error::SqueezefsError::Refused { errno, .. })
                if errno == libc::EAGAIN =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the quarantine never lifted"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(other) => panic!("{other:?}"),
        }
    };
    assert!(lease.is_held().await);
    assert!(
        squeezefs::meta_backend::kv::alloc_lease::unix_now_ms() >= rec.ts_ms + bound_ms,
        "granted no earlier than the bound past the record"
    );
    assert_eq!(
        data_grant::custody_quarantine_stats().1,
        0,
        "the quarantine expired"
    );
    // The expired record is retired at the next load (one control entry).
    assert_eq!(vol.load_custody_quarantines().await.unwrap(), 0);
    assert!(
        control
            .lookup(&squeezefs::meta_backend::kv::slot_state::custody_quarantine_key(SLOT_A))
            .await
            .unwrap()
            .is_none(),
        "the expired quarantine record is gone"
    );
    drop(lease);
    data_grant::disarm_slot_custody().await;
    data_grant::uninstall_custody_owner();
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// **Review round 8, Issue 36 — the quarantine survives a manager restart
/// inside the window.** The RAM quarantine is a process word; a manager
/// that dies and restarts inside `T_owner + 2 × skew_max` of an early death
/// record would otherwise grant fresh custody at once. The recovery writes
/// `custody_quarantine:{slot}` beside the slot's `Unleased` in the same
/// tree-0 entry, and the mount path's C15 gate re-derives the RAM word from
/// it (`load_custody_quarantines`): after the restart the acquire is
/// refused until the bound, then granted, and the expired record retired.
/// The restart is modelled as the manager's clean leave + the process
/// word CLEARED (`test_clear_custody_quarantine`) + the reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_custody_quarantine_survives_a_manager_restart_inside_the_window() {
    use squeezefs::data_grant::{self, WriteCustodyOwner};
    use squeezefs::membership::{LeaseClock, LeaseClocks};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared, seeded) = seeded_volume_with_slot_files(dir.path(), SLOT_A, 2).await;
    let x = foreign(72);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 5, x).await;
    let path = std::path::Path::new(&uris[0]);
    {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        shutdown(&routed).await;
    }
    match KvMetaBackend::appender_clear(path, path, 1).await {
        Ok(recovery::AppenderClearOutcome::Cleared { was, .. }) => {
            assert_eq!(was, AppenderState::Live)
        }
        other => panic!("{other:?}"),
    }
    let _hygiene = CustodyArmGuard;
    // A wide window: the restart must land inside it.
    let clocks = LeaseClocks::with_params(
        std::time::Duration::from_millis(8_000),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(400),
    )
    .expect("2*skew + purge < TTL");
    let bound_ms = data_grant::custody_quarantine_bound_for(&clocks);
    let owner = WriteCustodyOwner::arm(
        "recoverer",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        LeaseClock::monotonic(),
        None,
    )
    .expect("the recoverer's custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&owner));
    let arm_on = |routed: &Arc<RoutedMetaBackend>| {
        let sink = Arc::new(NoopRecallSink);
        data_grant::arm_slot_custody(
            routed,
            "recoverer",
            VENUE_SECRET.to_vec(),
            0,
            Arc::new(move |_volume| {
                Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
            }),
        );
    };
    let (_, ino) = seeded[0];
    let lock = squeezefs::keys::inode_path(ino);
    let ttl = std::time::Duration::from_millis(500);
    // ---- The first incarnation recovers and quarantines.
    let ts_ms = {
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        let vol = Arc::clone(&routed.volumes[0]);
        arm_on(&routed);
        let rec = vol
            .dead_member_record(&x)
            .await
            .unwrap()
            .expect("the record");
        let rep = mount_path_custody_gate(&routed).await.unwrap();
        assert_eq!(rep.recovered(), 1, "{rep:?}");
        assert_eq!(data_grant::custody_quarantine_stats().1, 1);
        let dlm = squeezefs::dlm::DlmClient::new().unwrap();
        let err = dlm
            .acquire_lock(&lock, None, ttl)
            .await
            .expect_err("quarantined at the first incarnation");
        assert!(matches!(
            &err,
            squeezefs::error::SqueezefsError::Refused { errno, .. } if *errno == libc::EAGAIN
        ));
        // The restart: the clean leave, the process word lost.
        data_grant::disarm_slot_custody().await;
        shutdown(&routed).await;
        drop(vol);
        drop(routed);
        data_grant::test_clear_custody_quarantine();
        assert_eq!(data_grant::custody_quarantine_stats().1, 0);
        rec.ts_ms
    };
    assert!(
        squeezefs::meta_backend::kv::alloc_lease::unix_now_ms() < ts_ms + bound_ms,
        "premise: the restart lands inside the window"
    );
    // ---- The second incarnation: the gate re-derives the quarantine from
    // tree 0 (RED before: the word was RAM alone — granted at once).
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    arm_on(&routed);
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 0, "nothing left to recover: {rep:?}");
    assert_eq!(
        data_grant::custody_quarantine_stats().1,
        1,
        "the quarantine is re-derived from the durable record"
    );
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let err = dlm
        .acquire_lock(&lock, None, ttl)
        .await
        .expect_err("still quarantined after the restart");
    match &err {
        squeezefs::error::SqueezefsError::Refused { errno, msg } => {
            assert_eq!(*errno, libc::EAGAIN, "{err:?}");
            assert!(msg.contains("quarantined"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    // Then granted — no earlier than the bound past the ORIGINAL record.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let lease = loop {
        match dlm.acquire_lock(&lock, None, ttl).await {
            Ok(lease) => break lease,
            Err(squeezefs::error::SqueezefsError::Refused { errno, .. })
                if errno == libc::EAGAIN =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the quarantine never lifted"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(other) => panic!("{other:?}"),
        }
    };
    assert!(lease.is_held().await);
    assert!(squeezefs::meta_backend::kv::alloc_lease::unix_now_ms() >= ts_ms + bound_ms);
    drop(lease);
    // A third load retires the expired record.
    assert_eq!(vol.load_custody_quarantines().await.unwrap(), 0);
    assert!(vol
        .forest_control_tree()
        .unwrap()
        .lookup(&squeezefs::meta_backend::kv::slot_state::custody_quarantine_key(SLOT_A))
        .await
        .unwrap()
        .is_none());
    data_grant::disarm_slot_custody().await;
    data_grant::uninstall_custody_owner();
    assert_all_resolve(&routed, shared, &files[0]).await;
    assert_all_resolve(&routed, ROOT_INO, &seeded).await;
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

/// The `claim clear` law over EVERY volume the verb touches (review round
/// 1, Issue 6): `appender clear` on volume 1 of a set whose VOLUME 0 holds
/// a heartbeat-fresh `writer_claim` — a live manager, on this host or a
/// foreign one — is refused naming volume 0's claim, before anything of
/// the verb's is opened; volume 1's own aged claim did not admit it. The
/// first build gated the claim check on the cleared page being the
/// manager's and never read volume 0's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appender_clear_refuses_a_fresh_writer_claim_on_volume_0() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 2).await;
    // A clean writer over both volumes (every claim removed at its leave)…
    {
        let routed = open_under(&uris, &Knobs::unarmed()).await;
        let d = routed
            .create(ROOT_INO, "d", libc::S_IFDIR | 0o755, 1000, 1000)
            .await
            .unwrap()
            .ino;
        routed
            .create(d, "f", libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap();
        shutdown(&routed).await;
    }
    let (vol0, vol1) = (
        std::path::Path::new(&uris[0]),
        std::path::Path::new(&uris[1]),
    );
    // …then a FOREIGN host's heartbeat-fresh claim planted on volume 0
    // alone (what a live manager elsewhere looks like from this host).
    let foreign_claim = squeezefs::meta_backend::kv::backend::WriterClaim {
        id: "foreign-manager".to_string(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        pid: 1,
        boot: "another-boot".to_string(),
        term: 7,
    };
    KvMetaBackend::test_plant_writer_claim(vol0, &foreign_claim)
        .await
        .unwrap();
    match KvMetaBackend::appender_clear(vol1, vol0, 0).await {
        Err(KvError::Busy(m)) => {
            assert!(m.contains("writer claim"), "{m}");
            assert!(m.contains(&uris[0]), "the refusal names volume 0: {m}");
            assert!(m.contains("foreign-manager"), "{m}");
        }
        other => panic!("cleared under volume 0's fresh claim: {other:?}"),
    }
    // The operator's wait (the TTL passed on volume 0's claim): the verb
    // reaches its page checks — page 0 is `Free` after the clean leave.
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(3_600, Ordering::SeqCst);
    let out = KvMetaBackend::appender_clear(vol1, vol0, 0).await;
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(0, Ordering::SeqCst);
    match out {
        Ok(recovery::AppenderClearOutcome::NothingToClear) => {}
        other => panic!("{other:?}"),
    }
    reset_process_state();
}

/// C14 (§5.8.5) meets its remedy: one slot attested `Live` on TWO dead
/// pages at tree 0's `g` — the non-PR §5.8.2 class (iii) shape a zombie's
/// page write can leave — is the slot custody conflict the census
/// reports and the mount refuses at its arm; `appender clear` attests
/// the forging page dead, and the next mount recovers what tree 0 leases
/// to it while DROPPING the stale attestation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_c14_conflict_is_reported_refused_and_cleared_by_the_attestation() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Two seeded directories, two declared regions (slots 4 and 6).
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let (d4, d6) = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let d4 = seed_dir_in_slot(&routed, 0, 4, "shared4").await;
        let d6 = seed_dir_in_slot(&routed, 0, 6, "shared6").await;
        for s in [4u32, 6] {
            routed.volumes[0].release_slot_handover(0, s).await.unwrap();
        }
        shutdown(&routed).await;
        (d4, d6)
    };
    let routed = open_under_retry(&uris, &Knobs::armed().partition("1:4;2:6"))
        .await
        .unwrap();
    let venue = HoldersVenue::stand_up(&routed, &[1, 2]).await;
    let mut files = Vec::new();
    for (d, tag) in [(d4, "a"), (d6, "b")] {
        for f in 0..3 {
            let name = format!("{tag}{f}");
            let ino = routed
                .create(d, &name, libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .unwrap()
                .ino;
            files.push((d, name, ino));
        }
    }
    venue.tear_down();
    drop(routed);
    park_gate::test_reset();
    let (x, y) = (foreign(80), foreign(81));
    restamp_page_identity(&uris[0], 1, x).await;
    // Y's page forged: it attests slot 4 (X's, routing slot 3) at X's g.
    let x_entry = {
        use squeezefs::meta_backend::kv::appender::read_directory;
        use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
        let path = std::path::Path::new(&uris[0]);
        let VolumeFormat::V3(sb) = classify_volume(path).await.unwrap() else {
            panic!()
        };
        read_directory(path, &sb)
            .await
            .unwrap()
            .iter()
            .find(|e| e.appender_id == 1)
            .and_then(|e| e.page.as_ref())
            .and_then(|p| p.slots.iter().find(|s| s.slot == 3).copied())
            .expect("X attests routing slot 3")
    };
    rewrite_page(&uris[0], 2, |p| {
        p.identity = y;
        p.slots.push(x_entry);
        p.slots.sort_by_key(|s| s.slot);
    })
    .await;
    // The census names the conflict; the mount refuses it at the arm.
    match open_under_retry(&uris, &Knobs::armed()).await {
        Err(m) => assert!(m.contains("custody conflict"), "{m}"),
        Ok(_) => panic!("a C14 conflict was admitted"),
    }
    // fsck C14 (the probe): ONE finding, report-only, naming the remedy.
    {
        let report = fsck_probe(&uris).await;
        assert_eq!(report.counters.slot_custody_conflicts, 1, "{report:?}");
        assert_eq!(report.counters.unrecovered_appenders, 0);
        let c14: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.class == "C14")
            .collect();
        assert_eq!(c14.len(), 1, "{:?}", report.findings);
        match &c14[0].identity {
            Some(FindingId::C14SlotCustodyConflict {
                vol: 0, slot: 4, ..
            }) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            c14[0].evidence.contains("appender clear"),
            "{}",
            c14[0].evidence
        );
    }
    // The remedy: attest the forging page dead. The KILLED writer's claim
    // is heartbeat-fresh for the TTL and the verb refuses it (the `claim
    // clear` law — review round 1, Issue 6); the operator's wait is the
    // seam that ages it.
    let path = std::path::Path::new(&uris[0]);
    match KvMetaBackend::appender_clear(path, path, 2).await {
        Err(KvError::Busy(m)) => assert!(m.contains("writer claim"), "{m}"),
        other => panic!("cleared under a fresh writer claim: {other:?}"),
    }
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(3_600, Ordering::SeqCst);
    let cleared = KvMetaBackend::appender_clear(path, path, 2).await;
    recovery::TEST_CLAIM_CLOCK_SKEW_SECS.store(0, Ordering::SeqCst);
    match cleared {
        Ok(recovery::AppenderClearOutcome::Cleared { .. }) => {}
        other => panic!("{other:?}"),
    }
    // fsck C15 (the probe): the ledgered Recovering page is ONE finding
    // whose plan is the recovery; an OFFLINE apply refuses naming the
    // mount path (a probe holds no ring and no lease to recover under).
    {
        let report = fsck_probe(&uris).await;
        assert_eq!(report.counters.slot_custody_conflicts, 0, "{report:?}");
        assert_eq!(report.counters.unrecovered_appenders, 1, "{report:?}");
        let c15: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.class == "C15")
            .collect();
        assert_eq!(c15.len(), 1, "{:?}", report.findings);
        match &c15[0].identity {
            Some(FindingId::C15UnrecoveredAppender {
                vol: 0,
                appender: 2,
                node_token,
                mount_slot,
                ..
            }) => assert_eq!((*node_token, *mount_slot), (y.node_token, y.mount_slot)),
            other => panic!("{other:?}"),
        }
        for apply in [false, true] {
            let ropts = squeezefs::fsck::RepairOptions {
                apply,
                quarantine_dir: Some(dir.path().join("quarantine")),
                multi_owner: false,
            };
            let rep = fsck_probe_with(&uris, Some(&ropts)).await;
            let repair = rep.repair.expect("repair ran");
            let planned: Vec<_> = repair.planned.iter().filter(|a| a.class == "C15").collect();
            assert_eq!(planned.len(), 1, "{repair:?}");
            assert_eq!(planned[0].action, "recover-dead-appender");
            if apply {
                assert!(repair.applied.iter().all(|a| a.class != "C15"));
                let refused = repair
                    .refused
                    .iter()
                    .find(|a| a.class == "C15")
                    .expect("the offline apply refuses");
                assert!(refused.detail.contains("mount"), "{}", refused.detail);
            }
        }
        park_gate::test_reset();
    }
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    let census = vol.slot_custody_census(Some(&vol)).await.unwrap();
    assert!(
        census.conflicts.is_empty(),
        "a Recovering page attests nothing: {census:?}"
    );
    assert_eq!(census.unrecovered.len(), 1);
    let stale_before = vol.slot_lease_stats().unwrap().stale_entries;
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1);
    let r = &rep.per_volume[0].1.recovered[0];
    assert_eq!(r.appender_id, 2);
    assert_eq!(r.slots, vec![6], "tree 0 leased Y slot 6 alone");
    assert_eq!(
        r.stale_entries, 1,
        "the forged slot-4 entry was dropped, not released"
    );
    assert!(vol.slot_lease_stats().unwrap().stale_entries > stale_before);
    // X still leases slot 4 (a joined appender the ledger never named);
    // Y's files resolve; X's too (its dentries ride its own live page's
    // ring — served after ITS recovery, which nothing has asked for).
    assert!(matches!(
        tree0_state(&vol, 4).await,
        Some(SlotState::Leased { appender_id: 1, .. })
    ));
    for (d, name, ino) in files.iter().filter(|(d, _, _)| *d == d6) {
        assert_eq!(routed.lookup(*d, name).await.unwrap().ino, *ino);
    }
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    // Recovered: both classes read 0 and the set is clean.
    let report = fsck_probe(&uris).await;
    assert_eq!(report.counters.slot_custody_conflicts, 0);
    assert_eq!(report.counters.unrecovered_appenders, 0);
    assert!(!report.has_findings(), "{:?}", report.findings);
    reset_process_state();
}

// ---------------------------------------------------------------------------
// The routed Issue 1 (PR 2's base tree): the own page one root ahead
// ---------------------------------------------------------------------------

/// A kill on an UNARMED stamped writer between a checkpoint's page write
/// and the next cycle's tree-0 publication: the flush pass split a guest
/// slot's root leaf (a create storm), so region 0's page names the NEW
/// root while tree 0 still names the pre-split one — the shape every
/// post-PR-14 default mount has under write load. The remount opens the
/// guest at the newest durable root (the page's) through the WRITER-legal
/// install and serves every acked name; before the fix `adopt_root`
/// refused the remount ("this mount has not armed reader revalidation" —
/// `corpse_sweep_tests` stamped 1-in-4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_between_a_page_write_and_the_next_publication_remounts_an_unarmed_forest() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    // The cadence parked: the TWO cycles below are the test's, so the
    // second flush pass's root move lands on the page and nowhere else.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    // A GUEST slot's directory (the native slot's root rides the ledger,
    // never tree 0 or the page — KD-SYM-3).
    let slot = hosted_slot_on(&routed, 0, 7);
    let d = seed_dir_in_slot(&routed, 0, slot, "storm").await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(slot_of_global(&routed, d), slot);
    // The write load: rounds of creates (dentries into the directory's
    // slot tree, inode records across the rotor's slot trees) each
    // followed by ONE cycle. Threshold maintenance appends a leaf's
    // delta into its log during the commits; the flush pass COMPACTS a
    // leaf whose frozen delta no longer fits its log (`Log area full` in
    // `checkpoint_flush_node`) — a root leaf's compaction MOVES the root
    // after tree 0's publication step and before the page write, so the
    // page names a root tree 0 does not. Per round the shape lands on
    // any of ~64 dirtied root leaves; the bound is loud.
    let page_roots_ahead = |vol: Arc<KvMetaBackend>, uri: String| async move {
        use squeezefs::meta_backend::kv::appender::{forest_slot_of_page_slot, read_directory};
        let native = vol.appenders_public().unwrap().native_slot;
        let dir = read_directory(std::path::Path::new(&uri), vol.superblock())
            .await
            .unwrap();
        let page = dir
            .iter()
            .find(|e| e.appender_id == 0)
            .and_then(|e| e.page.as_ref())
            .expect("page 0");
        let mut ahead = Vec::new();
        for s in &page.slots {
            let fs = forest_slot_of_page_slot(s.slot, native);
            let recorded = match tree0_state(&vol, fs).await {
                Some(SlotState::Unleased { root, .. }) => root,
                Some(SlotState::Leased { root, .. }) => root,
                None => continue,
            };
            if s.root.seq > recorded.seq {
                ahead.push((fs, s.root, recorded));
            }
        }
        ahead
    };
    let mut files = Vec::new();
    let mut ahead = Vec::new();
    for round in 0..64 {
        for i in 0..300 {
            let name = format!("s{round:02}_{i:04}");
            let ino = routed
                .create(d, &name, libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .unwrap()
                .ino;
            files.push((name, ino));
        }
        vol.checkpoint_now().await.unwrap();
        ahead = page_roots_ahead(Arc::clone(&vol), uris[0].clone()).await;
        if !ahead.is_empty() {
            break;
        }
    }
    assert!(
        !ahead.is_empty(),
        "the fixture's premise: some page-0 root ahead of tree 0's within the bound"
    );
    // The kill: no shutdown, no leave.
    drop(vol);
    drop(routed);
    park_gate::test_reset();
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    let again = open_under_retry(&uris, &Knobs::unarmed())
        .await
        .expect("the remount opens the guest at the page's root");
    for (name, ino) in &files {
        assert_eq!(again.lookup(d, name).await.unwrap().ino, *ino);
    }
    assert_eq!(
        again.readdir(d, 0, 1 << 20).await.unwrap().len(),
        files.len()
    );
    // The guest opened AHEAD of tree 0 reads as UNPUBLISHED: the remount's
    // first checkpoint publishes the page's root into tree 0 (the first
    // build seeded the forest's `published` map with the OPENED root, so
    // tree 0 kept the stale one for ever and the clean leave dropped the
    // records the moved root alone held — 25 dentries of this storm).
    let live = again.volumes[0].slot_tree(ahead[0].0).map(|t| t.root());
    shutdown(&again).await;
    drop(again);
    {
        let probe = squeezefs::meta_backend::open_probe_routed_meta_set(&uris)
            .await
            .unwrap();
        match tree0_state(&probe.volumes[0], ahead[0].0).await {
            Some(SlotState::Unleased { root, .. }) => assert_eq!(Some(root), live),
            other => panic!("{other:?}"),
        }
        for (name, ino) in &files {
            assert_eq!(probe.lookup(d, name).await.unwrap().ino, *ino);
        }
        for v in &probe.volumes {
            v.shutdown().await.unwrap();
        }
    }
    fsck_clean(&uris).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.6 × §5.9 — a dead holder's open cross-owner intent
// ---------------------------------------------------------------------------

/// PR 6's window under PR 10's death path: a cross-owner create whose
/// shipped step the holder refused BEFORE its commit leaves the intent
/// OPEN (never a fail-stop); the holder then dies with the slot leased to
/// it. The remount adopts the intent and cannot complete it (the slot's
/// holder is unreachable — `Abandoned`, counted stuck past the grace
/// window); the ledger names the holder; the recovery unleases the slot
/// to the manager and the SAME projection rolls the intent forward
/// LOCALLY (`recovery_intents_rolled_forward`): the name resolves, the
/// intent is retired, `xv_cross_owner_intents_stuck` reads 0 again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_holders_open_intent_is_rolled_forward_by_its_recovery() {
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, TEST_XV_SERVE_REFUSE, TEST_XV_STUCK_AFTER_MS,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(95);
    // The holder (region 1) serves one create, then refuses the next
    // before committing: one acked name, one open intent.
    let routed = open_under_retry(&uris, &Knobs::armed().partition("1:4"))
        .await
        .unwrap();
    let venue = HoldersVenue::stand_up(&routed, &[1]).await;
    let acked = routed
        .create(shared, "acked", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    TEST_XV_SERVE_REFUSE.store(true, Ordering::SeqCst);
    routed
        .create(shared, "open_intent", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect_err("the holder refused before committing");
    TEST_XV_SERVE_REFUSE.store(false, Ordering::SeqCst);
    let mut open = 0;
    for v in &routed.volumes {
        open += v.xv_scan_intents().await.unwrap().len();
    }
    assert_eq!(open, 1, "the intent stays open for roll-forward");
    // The kill: holder and initiator die together (one process); the
    // holder's page is restamped a foreign node's.
    venue.tear_down();
    drop(routed);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    restamp_page_identity(&uris[0], 1, x).await;

    // The remount adopts the intent; with the slot leased to a holder no
    // endpoint names, it stays open — stuck past the (seam-shortened)
    // grace window.
    TEST_XV_STUCK_AFTER_MS.store(1, Ordering::SeqCst);
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(vol.xv_scan_intents().await.unwrap().len(), 1);
    // The cadence's body, re-run until the seam-shortened grace (1 ms) has
    // passed and the intent counts stuck — never a sleep (Issue 21).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let n = squeezefs::meta_backend::crossvol_tx::roll_forward_open_intents(&routed)
            .await
            .unwrap();
        assert_eq!(n, 0, "no holder serves slot 4");
        if cross_owner_stats().intents_stuck == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the open intent never counted stuck"
        );
        tokio::task::yield_now().await;
    }
    // The ledger names the holder; the recovery unleases its slot and the
    // projection rolls the intent forward locally.
    let rolled0 = recovery_stats().intents_rolled_forward;
    assert!(!vol.record_death_with_key(x, 2, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_eq!(rep.intents_rolled_forward, 1, "{rep:?}");
    TEST_XV_STUCK_AFTER_MS.store(0, Ordering::SeqCst);
    assert_eq!(recovery_stats().intents_rolled_forward, rolled0 + 1);
    assert_eq!(vol.xv_scan_intents().await.unwrap().len(), 0);
    assert_eq!(cross_owner_stats().intents_stuck, 0);
    assert_eq!(routed.lookup(shared, "acked").await.unwrap().ino, acked);
    let later = routed.lookup(shared, "open_intent").await.unwrap();
    assert_eq!(routed.getattr(later.ino).await.unwrap().nlink, 1);
    assert!(matches!(
        tree0_state(&vol, SLOT_A).await,
        Some(SlotState::Unleased { .. } | SlotState::Leased { appender_id: 0, .. })
    ));
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// §5.8.2 / §5.8.3 — the non-PR zombie after the full tail scan
// ---------------------------------------------------------------------------

/// §5.8.2 on the death path without a device fence: the recoverer
/// records EVERY leaf's tail of the dead appender's slot trees (the full
/// tail scan, `recovery_full_tail_scan_bytes`), so a zombie of the dead
/// mount that appends one more frame under its old generation — onto a
/// leaf the recovery never touched, at exactly the recorded tail — is
/// SCREENED at the next load by PR 5's rule 2 (`g ≤ tails_g` at/past the
/// tail): the frame is never folded, every acked record still resolves,
/// and fsck is clean. §5.8.3: what the zombie wrote was never acked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zombie_frame_on_an_untouched_leaf_is_screened() {
    use squeezefs::meta_backend::kv::node::{
        append_bset, load_node, AppendDest, FrameStamp, NodeLayout,
    };
    use squeezefs::meta_backend::kv::record::{inode_key, InodeValue, Record};
    use squeezefs::meta_backend::kv::META_KV_FOREIGN_FRAMES_SCREENED;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // A slot tree of SEVERAL leaves on the device (the manager's seed,
    // checkpointed), then the dead region's window over its first leaf
    // alone (`d0_*` sorts before `k*`): the other leaves are untouched.
    let (uris, shared, seeded) = seeded_volume_with_files(dir.path(), SLOT_A, 1_500).await;
    let x = foreign(90);
    let files = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], 8, x).await;
    let files = &files[0];

    // The recovery: every leaf of slot 4's tree gets its tail recorded —
    // the untouched ones READ for it (the full tail scan).
    let scan0 = recovery_stats().full_tail_scan_bytes;
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(!vol.record_death_with_key(x, 3, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(
        recovery_stats().full_tail_scan_bytes > scan0,
        "no device fence: the recoverer read every untouched leaf for its tail"
    );
    let (g, tails) = vol
        .slot_tails(SLOT_A)
        .await
        .unwrap()
        .expect("the death path recorded the slot's tails");
    assert!(tails.len() >= 2, "a multi-leaf tree: {tails:?}");
    let node_size = vol.superblock().node_size as usize;
    shutdown(&routed).await;
    drop(vol);
    drop(routed);

    // The zombie: appender 1 at its old generation g, one frame at every
    // recorded tail (the recorded tail IS the leaf's tail — pinned).
    let layout = NodeLayout::new_symmetric(node_size).unwrap();
    let zombie = layout.stamped(FrameStamp { appender_id: 1, g });
    let path = std::path::Path::new(&uris[0]);
    for (addr, tail) in &tails {
        let loaded = load_node(path, &layout, *addr, 0).await.unwrap();
        let dest = loaded.append_dest();
        assert_eq!(dest.tail_offset, *tail as usize, "leaf {addr:#x}");
        let bogus = InodeValue {
            mode: libc::S_IFREG | 0o600,
            uid: 0,
            gid: 0,
            nlink: 1,
            flags: 0,
            rdev: 0,
            size: 0xDEAD,
            atime: 0,
            mtime: 0,
            ctime: 0,
        };
        let rec = Record::put(
            inode_key(ino_in_slot(SLOT_A, 0xFFF0)).to_vec(),
            u64::MAX / 4,
            bogus.encode(),
        );
        let dest = AppendDest {
            node_addr: dest.node_addr,
            node_seq: dest.node_seq,
            tail_offset: *tail as usize,
        };
        append_bset(path, &zombie, &dest, &[rec], u64::MAX / 4)
            .await
            .expect("the zombie's append lands on the device");
    }

    // The next open screens it: every acked record resolves, the screen
    // counted the frame(s), nothing else changed, fsck clean.
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    assert_all_resolve(&routed, shared, files).await;
    assert_all_resolve(&routed, shared, &seeded).await;
    let names = routed.readdir(shared, 0, 4096).await.unwrap();
    assert_eq!(
        names.len(),
        files.len() + seeded.len(),
        "the zombie's record is not a name and nothing acked is missing"
    );
    assert!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed) > screened0,
        "the zombie frame past the recorded tail was screened"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed),
        0,
        "no device fence, no breach class"
    );
    assert_eq!(
        routed.volumes[0]
            .appender_stats()
            .unwrap()
            .manager_verb_refusals,
        0
    );
    shutdown(&routed).await;
    drop(routed);
    fsck_clean(&uris).await;
    reset_process_state();
}

// ---------------------------------------------------------------------------
// The scoping instrument (dev box — SCOPING, never acceptance)
// ---------------------------------------------------------------------------

/// `appender_recovery_phase_ns` vs the dead window's size, the derived
/// bound beside the measured total, and `dead_member_propagation_ms` —
/// the evidence note's table. `#[ignore]`d: it prints, it asserts only
/// the exact-sum law. Run with
/// `cargo test --release --test sym_crash_matrix_tests -- --ignored --nocapture scoping_`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn scoping_recovery_phase_ns_vs_window() {
    let _g = SEAM.lock().await;
    println!(
        "{:>7} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>9} {:>7}",
        "files",
        "entries",
        "preempt",
        "read",
        "replay",
        "flush",
        "tails",
        "tree0",
        "total_us",
        "bound_ms",
        "prop_ms"
    );
    for (i, files) in [12usize, 200, 600].into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        reset_process_state();
        let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
        let x = foreign(200 + i as u64);
        let _ = kill_with_region_one_live(&uris, "1:4", &[(0, shared)], files, x).await;
        let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
        let vol = Arc::clone(&routed.volumes[0]);
        let p0 = recovery_stats().phase_ns;
        assert!(!vol.record_death_with_key(x, 1, 0).await.unwrap());
        let rep = recover_dead_appenders_set(&routed).await.unwrap();
        assert_eq!(rep.recovered(), 1);
        let entries = rep.per_volume[0].1.recovered[0].entries;
        let p1 = recovery_stats().phase_ns;
        let d: Vec<u64> = p0.iter().zip(p1.iter()).map(|(a, b)| b - a).collect();
        assert!(d[..6].iter().sum::<u64>() <= d[6] + 6, "exact-sum: {d:?}");
        let prop = alloc_lease::DEAD_MEMBER_PROPAGATION_MS.load(Ordering::Relaxed);
        println!(
            "{:>7} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>9} {:>7}",
            files,
            entries,
            d[0] / 1_000,
            d[1] / 1_000,
            d[2] / 1_000,
            d[3] / 1_000,
            d[4] / 1_000,
            d[5] / 1_000,
            d[6] / 1_000,
            vol.appender_recovery_bound_ms(),
            prop
        );
        shutdown(&routed).await;
        drop(vol);
        drop(routed);
    }
    reset_process_state();
}

/// The offline fsck over a probe THIS harness opens (the `fsck_clean`
/// walk with the findings kept): right after a kill the dead holder's
/// `writer_claim` is heartbeat-fresh for the TTL and `run_offline`'s
/// live-client preflight refuses by design, so the census is reached
/// through `run_offline_over` with the probe held here.
async fn fsck_probe_with(
    uris: &[String],
    repair: Option<&squeezefs::fsck::RepairOptions>,
) -> squeezefs::fsck::FsckReport {
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let probe = squeezefs::meta_backend::open_probe_routed_meta_set(uris)
        .await
        .expect("probe opens");
    let report = squeezefs::fsck::run_offline_over(&probe, uris, &opts, repair)
        .await
        .expect("offline fsck runs");
    for v in &probe.volumes {
        v.shutdown().await.unwrap();
    }
    report
}

async fn fsck_probe(uris: &[String]) -> squeezefs::fsck::FsckReport {
    fsck_probe_with(uris, None).await
}

// ---------------------------------------------------------------------------
// The two-backend fixture (review round 2 — the reviewer's structural
// finding): the RECOVERER is a different `KvMetaBackend` than the dead
// lessee, with its own RAM tree and cache, STALE against the device.
// ---------------------------------------------------------------------------

/// What the fixture stands up.
struct TwoBackends {
    uris: Vec<String>,
    /// The recoverer (the manager, opened AFTER the lessee's first
    /// checkpoint image; its tree of `slot` is at the OLDER root).
    routed: Arc<RoutedMetaBackend>,
    vol: Arc<KvMetaBackend>,
    slot: ForestSlot,
    dir: u64,
    /// The dead lessee's identity (the ledger's key).
    x: AppenderIdentity,
    /// Every file the lessee acked: the ones in its first image, the ones
    /// its LATER checkpoint moved the root for, and the window past its
    /// last page write.
    files: Vec<(String, u64)>,
    /// The root the recoverer opened the slot at / the root the device
    /// names after the lessee's later checkpoint.
    stale_root: squeezefs::meta_backend::kv::tree::RootPtr,
    newer_root: squeezefs::meta_backend::kv::tree::RootPtr,
    /// The lease generation tree 0 records for the dead lessee (the
    /// seeding manager held `g = 1` and released; region 1 took `g = 2`).
    g: u32,
    /// Regular files the seeding manager preset INTO the slot (under the
    /// root) before its release — their inode records live in the dead
    /// lessee's tree, where the lessee's own creates put only the
    /// dentries (PR 6 mints a shipped create's child in the CREATOR's
    /// rotor). Empty unless the fixture was asked to seed them.
    seeded: Vec<(String, u64)>,
}

/// Stand the shape up (the cadence parked throughout — every cycle is
/// the fixture's):
///
/// 1. seed a directory in `slot`, released to tree 0;
/// 2. backend A (the manager) with region 1 declared on `slot` — the
///    lessee, PR 6's wire holder in this process: creates under the
///    directory ship to it and commit into ITS ring; A checkpoints (page 1
///    names root R1); its region image is CAPTURED (image 1);
/// 3. A keeps writing until the flush pass MOVES the root (a compaction of
///    the root leaf — bounded rounds), checkpoints (page 1 names R2 > R1),
///    then writes a few more files that stay in the WINDOW past page 1's
///    tail; A is KILLED (no leave); image 2 captured;
/// 4. image 1 is re-applied under the foreign identity X, so the device
///    reads as "lessee X, checkpointed at R1";
/// 5. backend B — the recoverer — opens on the volume: its tree of `slot`
///    is at R1 (tree 0's `Leased { 1 }`, the page's root);
/// 6. image 2 is applied under X while B is open: the lessee's later
///    checkpoint and window landed under B; B's RAM tree is STALE.
///
/// The death record is the caller's — every pin below stands its own.
async fn two_backends(dir: &std::path::Path) -> TwoBackends {
    two_backends_with(dir, TwoBackendsWindow::Creates(15)).await
}

/// What the lessee writes into its ring AFTER its last page write — the
/// window the recovery replays.
#[derive(Clone, Copy)]
enum TwoBackendsWindow {
    /// `n` creates under the directory (the fixture's default shape).
    Creates(usize),
    /// Nothing: the lessee died right after its checkpoint (Issue 24's
    /// shape — no window record raises the recoverer's handle).
    Empty,
    /// One UNLINK of a file the lessee's last checkpoint holds (Issue
    /// 26's shape): the dentry DELETE rides the lessee's ring, the
    /// child's destroy the creator's — ring 0, replayed by the recoverer.
    UnlinkOne,
    /// `n` creates, with the lessee's rounds continued until the slot's
    /// tree has an INTERIOR root (a leaf split under it) — Issue 31's
    /// shape: the recovery's flush of a wide window compacts or splits a
    /// leaf UNDER the root, and the parent's pointer flip is an interior
    /// record the recoverer journals into ring 0 (a root-leaf compaction
    /// alone is a root swap, which journals no pointer record). Whether
    /// the fold overflows a leaf depends on where the lessee's own
    /// threshold maintenance left each log at the kill (the dentry keys
    /// hash across the leaves) — the row asserts the premise and retries
    /// the fixture, bounded, when a run's flush appended without an SMO.
    CreatesUnderInterior(usize),
}

async fn two_backends_with(dir: &std::path::Path, window: TwoBackendsWindow) -> TwoBackends {
    two_backends_seeded(dir, window, 0).await
}

/// [`two_backends_with`] with `seed_files` regular files minted in the
/// slot by the seeding manager before its release (the custody pin's
/// objects: a custody object is a regular file OF the recovering slot).
async fn two_backends_seeded(
    dir: &std::path::Path,
    window: TwoBackendsWindow,
    seed_files: usize,
) -> TwoBackends {
    // A previous contract's panic may have left a seam armed.
    recovery::TEST_RECOVERY_FAIL_AT_STEP.store(0, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(false, Ordering::SeqCst);
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let slot = SLOT_A;
    let (uris, d, seeded) = if seed_files == 0 {
        let (uris, d) = seeded_volume(dir, slot).await;
        (uris, d, Vec::new())
    } else {
        seeded_volume_with_slot_files(dir, slot, seed_files).await
    };
    let x = foreign(31);
    // ---- A: the lessee.
    let routed = open_under_retry(&uris, &Knobs::armed().partition("1:4"))
        .await
        .expect("A opens with the partition");
    let venue = HoldersVenue::stand_up(&routed, &[1]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    async fn storm(
        routed: &RoutedMetaBackend,
        d: u64,
        count: usize,
        files: &mut Vec<(String, u64)>,
        n: &mut usize,
    ) {
        for _ in 0..count {
            let name = format!("w{:05}", *n);
            *n += 1;
            let ino = routed
                .create(d, &name, libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .expect("create under the lessee's directory")
                .ino;
            files.push((name, ino));
        }
    }
    let mut files = Vec::new();
    let mut n = 0usize;
    storm(&routed, d, 60, &mut files, &mut n).await;
    vol.checkpoint_now().await.unwrap();
    let native = vol.appenders_public().unwrap().native_slot;
    let page_root = move |img: &RegionImage| {
        img.page
            .slots
            .iter()
            .find(|s| {
                squeezefs::meta_backend::kv::appender::forest_slot_of_page_slot(s.slot, native)
                    == slot
            })
            .map(|s| s.root)
            .expect("page 1 names the slot's root")
    };
    let image1 = capture_region_image(&uris[0], &vol, 1).await;
    let stale_root = page_root(&image1);
    // ---- A's later work: until the flush pass moves the root (or, for
    // `CreatesUnderInterior`, until the root is an interior node).
    let want_interior = matches!(window, TwoBackendsWindow::CreatesUnderInterior(_));
    let mut newer_root = stale_root;
    let mut root_level = 0u8;
    for _round in 0..(if want_interior { 60 } else { 40 }) {
        storm(&routed, d, 120, &mut files, &mut n).await;
        vol.checkpoint_now().await.unwrap();
        let img = capture_region_image(&uris[0], &vol, 1).await;
        newer_root = page_root(&img);
        root_level = vol
            .node_cache()
            .get(newer_root.addr)
            .await
            .expect("the page's root node loads")
            .level();
        if newer_root.seq > stale_root.seq && (!want_interior || root_level >= 1) {
            break;
        }
    }
    assert!(
        newer_root.seq > stale_root.seq,
        "the fixture's premise: the lessee's checkpoints moved the slot's root"
    );
    if want_interior {
        assert!(
            root_level >= 1,
            "the fixture's premise: the slot's root is an interior node (level {root_level})"
        );
    }
    // The window past the last page write.
    match window {
        TwoBackendsWindow::Creates(count) => storm(&routed, d, count, &mut files, &mut n).await,
        TwoBackendsWindow::CreatesUnderInterior(count) => {
            storm(&routed, d, count, &mut files, &mut n).await
        }
        TwoBackendsWindow::Empty => {}
        TwoBackendsWindow::UnlinkOne => {
            // The oldest file (in the lessee's FIRST image): its dentry's
            // DELETE is the window's one record; its record is destroyed
            // in ring 0 (the creator's — nlink 1 → 0).
            let (name, _) = files.remove(0);
            routed
                .unlink(d, &name)
                .await
                .expect("unlink under the lessee's directory");
        }
    }
    // ---- The kill.
    venue.tear_down();
    drop(vol);
    drop(routed);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let image2 = {
        let probe = squeezefs::meta_backend::open_probe_routed_meta_set(&uris)
            .await
            .expect("probe");
        let img = capture_region_image(&uris[0], &probe.volumes[0], 1).await;
        for v in &probe.volumes {
            v.shutdown().await.unwrap();
        }
        img
    };
    assert_eq!(page_root(&image2), newer_root);
    // ---- The device at the lessee's FIRST checkpoint, under X.
    apply_region_image(&uris[0], 1, &image1, x).await;
    // ---- B: the recoverer, its tree of the slot at R1.
    let routed = open_under_retry(&uris, &Knobs::armed())
        .await
        .expect("B opens over the foreign lessee's page");
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        vol.slot_tree(slot).map(|t| t.root()),
        Some(stale_root),
        "B holds the slot at the root the page named when it opened"
    );
    let g = match tree0_state(&vol, slot).await {
        Some(SlotState::Leased {
            appender_id: 1, g, ..
        }) => g,
        other => panic!("tree 0 leases the slot to the lessee: {other:?}"),
    };
    // ---- The lessee's later checkpoint and window land under B.
    apply_region_image(&uris[0], 1, &image2, x).await;
    TwoBackends {
        uris,
        routed,
        vol,
        slot,
        dir: d,
        x,
        files,
        stale_root,
        newer_root,
        g,
        seeded,
    }
}

fn two_backends_teardown() {
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER.store(0, Ordering::SeqCst);
    recovery::TEST_RECOVERY_FAIL_AT_STEP.store(0, Ordering::SeqCst);
    squeezefs::meta_backend::kv::checkpoint::TEST_CHECKPOINT_HALT_BEFORE_LEDGER
        .store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    reset_process_state();
}

/// **Issue 2 — the recoverer's stale RAM tree.** Another backend leased
/// the slot, checkpointed twice (the root moved) and died with acked
/// records in its window; the recovering MANAGER holds the slot's tree at
/// the root it opened with. The poll's recovery installs the page's newer
/// root writer-legal AFTER dropping every cached node of the slot (the
/// cache barrier), replays the window onto it, and every acked file —
/// image 1's, the moved root's, the window's — resolves through the
/// recoverer; tree 0 `Unleased` at the lease's `g`; fsck clean. Before the
/// fix `adopt_root` refused the install on the writer's cache and every
/// poll-path recovery in this shape failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recoverer_whose_ram_tree_is_stale_installs_the_dead_lessees_newer_root() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    let recs0 = recoveries();
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_eq!(rep.per_volume[0].1.deferred, 0, "{rep:?}");
    assert_eq!(recoveries(), recs0 + 1);
    let live = fx.vol.slot_tree(fx.slot).map(|t| t.root()).unwrap();
    assert!(
        live.seq >= fx.newer_root.seq,
        "the recoverer's tree moved to (or past) the lessee's newer root: {live:?} vs {:?} \
         (stale {:?})",
        fx.newer_root,
        fx.stale_root
    );
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    match tree0_state(&fx.vol, fx.slot).await {
        Some(SlotState::Unleased { g, .. }) => assert_eq!(g, fx.g, "g never moves at a release"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        page_state(&fx.uris[0], &fx.vol, 1).await,
        Some(AppenderState::Recovered)
    );
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Issue 3 — the handover order under every failure.** The recovery
/// FAILS once at each step boundary in turn (the ring read, the RAM table
/// gone `Releasing` + roots installed, the replay, the flush, the tails,
/// tree 0 written + the table released, the page `Recovered`); after
/// every failure the page stays `Recovering`, tree 0 still leases the
/// slot to the dead appender until the step that writes it, and the RAM
/// table has REFUSED nothing durable — the re-run resumes from the durable
/// state. The clean run then completes with every acked record; `g` never
/// regressed. Before the fix a `Try` refusal at step 7 left tree 0
/// `Leased { dead }` for ever under a `Recovered` page.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_that_fails_at_any_step_resumes_from_the_durable_state_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    for step in [3u32, 4, 5, 6, 7, 8, 9] {
        recovery::TEST_RECOVERY_FAIL_AT_STEP.store(step, Ordering::SeqCst);
        let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
        assert_eq!(
            recovery::TEST_RECOVERY_FAIL_AT_STEP.load(Ordering::SeqCst),
            0,
            "step {step} was reached"
        );
        assert_eq!(rep.recovered(), 0, "step {step}: {rep:?}");
        assert_eq!(rep.per_volume[0].1.deferred, 1, "step {step}: {rep:?}");
        // The page stays `Recovering` until the step that writes it (9 =
        // after the page went `Recovered`, before the `recovered:` record).
        assert_eq!(
            page_state(&fx.uris[0], &fx.vol, 1).await,
            Some(if step < 9 {
                AppenderState::Recovering
            } else {
                AppenderState::Recovered
            }),
            "step {step}"
        );
        assert!(
            fx.vol.recovered_record(&fx.x, 0).await.unwrap().is_none(),
            "step {step}: the recovered record is the last write"
        );
        match tree0_state(&fx.vol, fx.slot).await {
            Some(SlotState::Leased {
                appender_id: 1, g, ..
            }) if step < 8 => {
                assert_eq!(g, fx.g, "step {step}");
                // The RAM table agrees with tree 0 (the rollback): the dead
                // appender still the holder, no first-touch acquire admitted.
                let plane = fx.vol.slot_leases().unwrap();
                assert_eq!(
                    plane.table.resolve(fx.slot),
                    squeezefs::slot_lease_core::Resolved::Holder { holder: 1, g: fx.g },
                    "step {step}"
                );
            }
            Some(SlotState::Unleased { g, .. }) if step >= 8 => {
                assert_eq!(g, fx.g, "step {step}")
            }
            other => panic!("step {step}: {other:?}"),
        }
        assert!(
            fx.vol.dead_member_record(&fx.x).await.unwrap().is_some(),
            "step {step}: the record stands"
        );
    }
    // The clean run: the page is `Recovered` already, so the pass COMPLETES
    // the missing `recovered:` record (the poll's arm for the recoverer
    // that died between its two last writes) — PR 8's re-grant gate opens.
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.per_volume[0].1.completed, 1, "{rep:?}");
    assert!(fx.vol.recovered_record(&fx.x, 0).await.unwrap().is_some());
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    match tree0_state(&fx.vol, fx.slot).await {
        Some(SlotState::Unleased { g, .. }) => assert_eq!(g, fx.g, "g never regressed"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        page_state(&fx.uris[0], &fx.vol, 1).await,
        Some(AppenderState::Recovered)
    );
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **The PR 7b seam (the rebase onto `6c80d70f`): a dead lessee's slot
/// tree may hold a STRIPED directory — its `K + 2` reserved-name marker
/// dentries (the map, the commit marker, the migration flag) and the
/// stripe inos' own dentry sets — and the §5.9 steps are kind-blind.**
/// The holder flips its directory into 4 stripes, names land in the
/// stripes (cross-owner shipped steps into the stripes' holders — the
/// declared region's ring and ring 0), the holder dies; the recovery
/// replays every record by `(kind, key)` with no dentry-name inspection,
/// records the tails, unleases the slot; after it the map reads with
/// `k = 4`, every name resolves through the stripes, and fsck C17 —
/// stripe consistency over the recovered map and stripes — is clean
/// (`fsck_stripe_findings` 0) beside C9/C10/C14/C15.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_striped_directory_in_a_dead_lessees_slot_recovers_with_its_map_and_c17_clean() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, shared) = seeded_volume(dir.path(), SLOT_A).await;
    let x = foreign(41);
    let routed = open_under_retry(&uris, &Knobs::armed().partition("1:4"))
        .await
        .expect("open with the partition");
    let venue = HoldersVenue::stand_up(&routed, &[1]).await;
    routed
        .stripe_dir(shared, 4)
        .await
        .expect("the explicit flip on the declared holder's directory");
    for _ in 0..800 {
        let map = routed
            .stripe_map(shared)
            .await
            .expect("read")
            .expect("striped");
        if !map.migrating {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let map = routed.stripe_map(shared).await.unwrap().unwrap();
    assert_eq!(map.k(), 4);
    assert!(!map.migrating, "the seed's names migrated");
    let mut files = Vec::new();
    for f in 0..24 {
        let name = format!("s{f:03}");
        let ino = routed
            .create(shared, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create into the striped directory")
            .ino;
        files.push((name, ino));
    }
    // The kill: no shutdown, no leave — the ring windows stay.
    venue.tear_down();
    drop(routed);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    restamp_page_identity(&uris[0], 1, x).await;

    let routed = open_under_retry(&uris, &Knobs::armed()).await.unwrap();
    let vol = Arc::clone(&routed.volumes[0]);
    vol.record_death_with_key(x, 3, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(
        rep.per_volume[0].1.recovered[0].entries >= 1,
        "the dead window held the stripes' dentries: {rep:?}"
    );
    match tree0_state(&vol, SLOT_A).await {
        Some(SlotState::Unleased { .. }) => {}
        other => panic!("{other:?}"),
    }
    let map = routed
        .stripe_map(shared)
        .await
        .expect("read")
        .expect("the recovered directory is still striped");
    assert_eq!(map.k(), 4, "the map's K + 2 markers replayed kind-blind");
    assert_all_resolve(&routed, shared, &files).await;
    for s in &map.stripes {
        let rec = routed
            .getattr(*s)
            .await
            .expect("every stripe's S_IFDIR record");
        assert_eq!(rec.mode & libc::S_IFMT, libc::S_IFDIR);
    }
    // A create after the recovery routes into its stripe as before.
    let after = routed
        .create(shared, "after-recovery", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create after the recovery")
        .ino;
    assert_eq!(
        routed.lookup(shared, "after-recovery").await.unwrap().ino,
        after
    );
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let report = squeezefs::fsck::run_offline(&uris, &opts)
        .await
        .expect("offline fsck runs");
    assert!(!report.has_findings(), "{:?}", report.findings);
    assert_eq!(
        report.counters.stripe_findings, 0,
        "C17 clean after the recovery"
    );
    assert_eq!(report.counters.slot_custody_conflicts, 0);
    assert_eq!(report.counters.unrecovered_appenders, 0);
    assert_eq!(
        report.counters.inode_plane_volumes_covered, 1,
        "the inode plane recorded a verdict"
    );
}

/// **Review round 2, Issue 23 — a FAILED recovery restores EVERY RAM word
/// it changed, as one RAII.** Step 4 moves the lease table to `Releasing
/// { dead }` AND clears the gate's `foreign` bit (the recovery's own flush
/// is the manager's structure); before the fix the rollback restored the
/// table alone, so after a failure at steps 5–7 the manager's structural
/// passes read the dead lessee's tree as THEIRS to maintain until the
/// re-run — the merge sweep / D4 arm / heap-full recovery could run an
/// SMO on a tree tree 0 still leases to the dead appender, journaling
/// ring-0 interior records the next open's `Lease` detector refuses (PR 4
/// round 5's Issue-28 class). Now: after each failure the bit reads
/// foreign again, the table says `Leased { dead }`, and one merge sweep
/// SKIPS the tree (`merge_sweep_foreign_skips` moves, `META_KV_NODE_MERGES`
/// does not, the must-stay-0 refusal gauge does not) — then the clean run
/// completes and the tree is the manager's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_recovery_leaves_the_dead_lessees_tree_foreign_to_the_managers_sweep() {
    use squeezefs::meta_backend::kv::{META_KV_LEAF_LEASE_REFUSALS, META_KV_NODE_MERGES};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    let plane = fx.vol.slot_leases().unwrap();
    assert!(
        plane.gate.is_foreign(fx.slot),
        "the premise: the dead lessee's slot is FOREIGN to the recoverer at its arm"
    );
    let refusals0 = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    for step in [5u32, 6, 7] {
        recovery::TEST_RECOVERY_FAIL_AT_STEP.store(step, Ordering::SeqCst);
        let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
        assert_eq!(rep.per_volume[0].1.deferred, 1, "step {step}: {rep:?}");
        assert!(
            plane.gate.is_foreign(fx.slot),
            "step {step}: the rollback restored the gate's foreign bit"
        );
        assert_eq!(
            plane.table.resolve(fx.slot),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 1, g: fx.g },
            "step {step}: the table says Leased {{ dead }}"
        );
        let skips0 = fx.vol.slot_lease_stats().unwrap().merge_sweep_foreign_skips;
        let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);
        let report = fx.vol.defrag_merge_sweep(None).await.unwrap();
        assert!(
            report.lap_complete,
            "step {step}: a foreign tree never blocks the lap"
        );
        assert!(
            fx.vol.slot_lease_stats().unwrap().merge_sweep_foreign_skips > skips0,
            "step {step}: the sweep SKIPPED the dead lessee's tree (counted)"
        );
        assert_eq!(
            META_KV_NODE_MERGES.load(Ordering::Relaxed),
            merges0,
            "step {step}: no SMO ran on a tree tree 0 leases to the dead appender"
        );
        assert_eq!(
            META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
            refusals0,
            "step {step}: skipped, never refused"
        );
    }
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(
        !plane.gate.is_foreign(fx.slot),
        "released: the tree is the manager's to maintain"
    );
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Review round 3, Issue 29 — a FAILED recovery with NO successful re-run
/// leaves no floor on ring 0.** Step 4 installs the dead lessee's page root
/// with a `root_floor` at the run's ring-0 head; the table back at
/// `Leased { dead }`, that root is published nowhere and unpublishable —
/// before the fix the rollback left it, so the floor clamped ring 0's
/// checkpoint tail until a re-run succeeded, and a recovery that never
/// re-ran successfully (here: the record RETIRED by the member's rejoin)
/// pinned the tail for ever — the ring fills, every commit parks. Now the
/// rollback discards the slot's cached nodes and restores the tree's
/// pre-install `(root, floor)`: after a failure at step 5 and at step 7
/// (the flush done, the root moved) `unpublished_root_floors()` is EMPTY
/// and the tree reads its grant-time root; the record retires, the poll
/// recovers nothing, and after a storm of the manager's own commits ONE
/// cadence's cycles advance ring 0's tail past the storm's head with
/// `meta_kv_journal_full_stalls` flat. RED before at the floors assert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_recovery_with_no_re_run_leaves_no_floor_on_ring_0() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    assert!(
        fx.vol.test_unpublished_root_floors().is_empty(),
        "the premise: no guest root is unpublished before the recovery"
    );
    for step in [5u32, 7] {
        recovery::TEST_RECOVERY_FAIL_AT_STEP.store(step, Ordering::SeqCst);
        let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
        assert_eq!(rep.per_volume[0].1.deferred, 1, "step {step}: {rep:?}");
        assert_eq!(
            fx.vol.test_unpublished_root_floors(),
            Default::default(),
            "step {step}: the rollback left no unpublishable root's floor on ring 0"
        );
        assert_eq!(
            fx.vol.slot_tree(fx.slot).map(|t| t.root()),
            Some(fx.stale_root),
            "step {step}: the tree is back at the root it held before the install"
        );
        assert_eq!(
            page_state(&fx.uris[0], &fx.vol, 1).await,
            Some(AppenderState::Recovering),
            "step {step}: the durable state is the re-run's"
        );
    }
    // The member REJOINS elsewhere: its record retires — no re-run, ever.
    assert!(fx.vol.retire_death_record(&fx.x).await.unwrap());
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 0, "{rep:?}");
    assert!(fx.vol.test_unpublished_root_floors().is_empty());
    // The manager's own storm, then one cadence's cycles: the tail passes
    // the storm's head — nothing pins ring 0.
    let stalls0 = fx.vol.journal_full_stalls();
    for i in 0..40 {
        fx.routed
            .create(
                ROOT_INO,
                &format!("own-{i:03}"),
                libc::S_IFREG | 0o644,
                1000,
                1000,
            )
            .await
            .expect("a create in the manager's own slots");
    }
    let head = fx.vol.journal_ring().core().head();
    fx.vol.checkpoint_now().await.unwrap();
    fx.vol.checkpoint_now().await.unwrap();
    let tail = fx.vol.journal_ring().core().reusable_upto();
    assert!(
        tail >= head,
        "ring 0's tail ({tail}) advanced past the storm's head ({head}) within one cadence"
    );
    assert_eq!(fx.vol.journal_full_stalls(), stalls0, "no full-ring stall");
    // The re-run law still holds once a record names the member again: a
    // fresh run from the durable state recovers every acked record.
    fx.vol.record_death_with_key(fx.x, 2, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Review round 7, Issue 32 — a LIVE foreign lessee's page root ahead of
/// tree 0 floors nothing at the manager's open.** The manager RESTARTS
/// over a wire lessee whose checkpoints moved its slot's root past tree
/// 0's grant-time record (PR 12's steady state; here appender X's `Live`
/// page at R2 over tree 0's `Leased { 1, root: R1 }`). Before the fix the
/// open took round 2's "root ahead of tree 0 ⇒ unpublished" law for the
/// leased slot too and floored it at the open's ledger tail; nothing can
/// publish a leased slot's root (`publish_forest_roots` skips it by law —
/// the lessee's page is the venue), so ring 0's tail was clamped for the
/// mount's life: every commit past the ring's admissible window parked —
/// the wedge. Now the slot opens at its page root PUBLISHED (the lessee's
/// page IS its publication; its records sit in ITS ring — a ring-0 floor
/// protects nothing), the checkpoint's floor view carries no entry for it,
/// and after a storm in the manager's own slots ring 0's tail passes the
/// storm's start within the cover bound; tree 0 still names R1 under the
/// lease (the manager publishes nothing for it). The lessee's later death
/// recovers the published-at-open tree whole (the recovery's own floors
/// are its SMOs' and its tree-0 step's, Issue 31's hold untouched — its
/// pin runs beside this one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_foreign_lessees_page_root_ahead_of_tree_0_floors_nothing_at_the_managers_open() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    // The manager's clean leave, then its restart over the device as the
    // lessee left it: X's page LIVE at R2, tree 0 `Leased { 1, root: R1 }`.
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris,
        routed,
        vol,
        slot,
        dir: d,
        x,
        files,
        stale_root,
        newer_root,
        ..
    } = fx;
    drop(vol);
    drop(routed);
    assert!(
        newer_root.seq > stale_root.seq,
        "premise: the page root is ahead of tree 0's"
    );
    // The reopen's FIRST root publication is deferred (the reserve-
    // exhausted arm, `TEST_FOREST_PUBLISH_DEFER` — a legal cycle): before
    // the fix the bring-up cycle's publish skipped the leased slot's
    // record and still NOTED it published, which lifted the floor by
    // accident on the in-process shape; a deferral keeps every floor the
    // open took standing, which is the wire venue's steady state (the
    // manager never publishes a leased slot's root at all).
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER.store(1, Ordering::SeqCst);
    let routed = open_under_retry(&uris, &Knobs::armed())
        .await
        .expect("the manager reopens over the live foreign lessee's page");
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        vol.slot_tree(slot).map(|t| t.root()),
        Some(newer_root),
        "the slot opens at the lessee's page root"
    );
    match tree0_state(&vol, slot).await {
        Some(SlotState::Leased {
            appender_id: 1,
            root,
            ..
        }) => assert_eq!(
            root, stale_root,
            "tree 0 keeps the grant-time root under the lease"
        ),
        other => panic!("tree 0 leases the slot to the lessee: {other:?}"),
    }
    let floors = vol.test_unpublished_root_floors();
    assert!(
        !floors.contains_key(&slot),
        "a slot a LIVE foreign appender leases takes no ring-0 floor at the open: {floors:?}"
    );
    // A storm in the manager's own slots, then the cadence: ring 0's tail
    // passes the storm's start within the cover bound (a standing floor
    // would hold it at the open's tail for ever).
    let ring = vol.journal_ring().core();
    let h0 = ring.head();
    for i in 0..200u32 {
        routed
            .create(
                ROOT_INO,
                &format!("storm{i:04}"),
                libc::S_IFREG | 0o644,
                1000,
                1000,
            )
            .await
            .expect("a create in the manager's own slots");
    }
    let mut cleared = false;
    for _ in 0..squeezefs::meta_backend::kv::checkpoint::COVER_CYCLES_MAX {
        vol.checkpoint_now().await.unwrap();
        if ring.reusable_upto() >= h0 {
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "ring 0's tail never passed the storm's start (reusable_upto {} < head-before-storm {h0}) \
         — a floor stands for the foreign lessee's slot: {:?}",
        ring.reusable_upto(),
        vol.test_unpublished_root_floors()
    );
    assert!(
        vol.slot_tree(slot).is_some_and(|t| t.root() == newer_root),
        "the lessee's tree is untouched by the manager's cadence"
    );
    squeezefs::meta_backend::kv::backend::TEST_FOREST_PUBLISH_DEFER.store(0, Ordering::SeqCst);
    // The lessee's death: the published-at-open tree recovers whole (the
    // window's records included), and nothing is left floored.
    vol.record_death_with_key(x, 1, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(
        vol.test_unpublished_root_floors().is_empty(),
        "{:?}",
        vol.test_unpublished_root_floors()
    );
    assert_all_resolve(&routed, d, &files).await;
    match tree0_state(&vol, slot).await {
        Some(SlotState::Unleased { .. }) => {}
        other => panic!("tree 0 after the recovery: {other:?}"),
    }
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Review round 4, Issue 31 — the recoverer DIES inside its step-6 flush,
/// between a cycle's SMO records and that cycle's ledger record.** In the
/// two-backend shape the dead lessee's slot is no region of the
/// recoverer's set, so the flush's compactions journal their interior
/// records into RING 0 (the manager's) — a leaf compaction UNDER the slot
/// tree's interior root, whose parent pointer flip is the record (a
/// root-leaf compaction alone is a root swap, which journals none; the
/// fixture's `CreatesUnderInterior` rounds give the tree an interior
/// root first). The recoverer dies before the cycle's covering ledger
/// record (`TEST_CHECKPOINT_HALT_BEFORE_LEDGER`, then B dropped without a
/// shutdown): tree 0 still `Leased { dead }`, the page `Recovering`, the
/// flips UNCOVERED in ring 0. Before the fix the NEXT open's
/// `detect_appender_violations` read them as `Lease` violations and
/// REFUSED the mount — on every later open, with no verb to clear ring 0.
/// Now: the open judges the manager's interior records for a slot whose
/// tree-0 lessee's page is `Recovering` as the recovery's own
/// (`meta_kv_replay_lease_violations` flat), installs the page's root
/// before the replayed frees are judged, STASHES those records instead
/// of folding them into the foreign tree (witnessed), and the mount
/// path's C15 arm re-runs the recovery: the stash applies onto the
/// installed root, the dead window folds, every acked record resolves,
/// tree 0 `Unleased`, the page `Recovered`, fsck clean. (A death AFTER
/// step 6 completes leaves NO such record: the flush loops until every
/// dirty node is covered, and the slot's floor — the manager holds no
/// lease on it — never clamps ring 0.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recoverer_dying_after_its_flush_leaves_a_mount_the_next_open_admits() {
    use squeezefs::meta_backend::kv::checkpoint::TEST_CHECKPOINT_HALT_BEFORE_LEDGER;
    use squeezefs::meta_backend::kv::{
        META_KV_NODE_COMPACTIONS, META_KV_NODE_SPLITS, META_KV_REPLAY_LEASE_VIOLATIONS,
    };
    let _g = SEAM.lock().await;
    let smos = || {
        META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
            + META_KV_NODE_SPLITS.load(Ordering::Relaxed)
    };
    // The shape's premise — an SMO in the recovery's first flush cycle —
    // holds on most fixtures (the fold of a 600-record window over leaves
    // the lessee's own maintenance left at random fills); a fixture whose
    // flush only appended is torn down and rebuilt, bounded, loud on the
    // bound (the Issue-1 pin's own pattern).
    let mut attempts = 0;
    let (_dir, fx) = loop {
        attempts += 1;
        assert!(
            attempts <= 6,
            "the premise never held in {} fixtures: the recovery's first flush cycle compacted \
             or split nothing under the slot tree's root",
            attempts - 1
        );
        let dir = tempfile::tempdir().unwrap();
        reset_process_state();
        let fx = two_backends_with(dir.path(), TwoBackendsWindow::CreatesUnderInterior(600)).await;
        fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
        let smos0 = smos();
        // The recoverer's first step-6 cycle: the flush pass journals its
        // SMOs, then the cycle halts before its ledger record — the
        // recovery fails (the rollback runs), the SMO records stay
        // UNCOVERED in ring 0.
        TEST_CHECKPOINT_HALT_BEFORE_LEDGER.store(true, Ordering::SeqCst);
        let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
        TEST_CHECKPOINT_HALT_BEFORE_LEDGER.store(false, Ordering::SeqCst);
        assert_eq!(rep.per_volume[0].1.deferred, 1, "{rep:?}");
        if smos() > smos0 {
            break (dir, fx);
        }
        shutdown(&fx.routed).await;
        drop(fx);
        two_backends_teardown();
    };
    // ---- The recoverer dies: no shutdown, no leave.
    let TwoBackends {
        uris,
        routed,
        vol,
        slot,
        dir: d,
        x,
        files,
        newer_root: fx_newer_root,
        ..
    } = fx;
    drop(vol);
    drop(routed);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let lease_violations0 = META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed);
    // ---- The next open ADMITS ring 0's window (RED before: refused with
    // `appender partition violated … lease`).
    let routed = open_under_retry(&uris, &Knobs::armed())
        .await
        .expect("the next open admits the dead recoverer's structure for a slot mid-recovery");
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed),
        lease_violations0,
        "meta_kv_replay_lease_violations flat"
    );
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovering),
        "the durable state is the re-run's"
    );
    assert!(
        matches!(
            tree0_state(&vol, slot).await,
            Some(SlotState::Leased { appender_id: 1, .. })
        ),
        "tree 0 still leases the slot to the dead appender until the re-run"
    );
    let stashed = vol.test_recovering_structure_len(slot);
    assert!(
        stashed >= 1,
        "the premise, witnessed: ring 0's window carried the dead recoverer's interior \
         record(s) for the slot — stashed for the re-run, not folded into the foreign tree"
    );
    assert_eq!(
        vol.slot_tree(slot).map(|t| t.root()),
        Some(fx_newer_root),
        "the page's root is installed at the open (before the replayed frees are judged)"
    );
    // The HOLD is real across the open (review round 7, Issue 32): the
    // slot's floor stands after the bring-up cycles — the dead recoverer's
    // records stay in ring 0's window until the re-run publishes the root
    // (before round 7 the bring-up's publish step lifted it by accident).
    let hold = vol.test_unpublished_root_floors();
    assert!(
        hold.contains_key(&slot),
        "the recovery hold stands for the slot at the open: {hold:?}"
    );
    assert!(
        vol.journal_ring().core().reusable_upto() <= hold[&slot],
        "ring 0's tail never passed the hold"
    );
    // ---- The mount path's C15 arm re-runs the recovery from the durable
    // state (the stash applies first, then the dead window).
    let rep = mount_path_custody_gate(&routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_eq!(
        vol.test_recovering_structure_len(slot),
        0,
        "the stash was consumed by the re-run"
    );
    assert!(
        vol.test_unpublished_root_floors().is_empty(),
        "the re-run's tree-0 step lifted the hold: {:?}",
        vol.test_unpublished_root_floors()
    );
    assert_all_resolve(&routed, d, &files).await;
    match tree0_state(&vol, slot).await {
        Some(SlotState::Unleased { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(
        page_state(&uris[0], &vol, 1).await,
        Some(AppenderState::Recovered)
    );
    assert!(vol.recovered_record(&x, 0).await.unwrap().is_some());
    assert_eq!(
        META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed),
        lease_violations0
    );
    // The slot is leasable again: a create under the directory.
    routed
        .create(
            d,
            "after-the-second-recoverer",
            libc::S_IFREG | 0o644,
            1000,
            1000,
        )
        .await
        .expect("a create under the recovered directory");
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Review round 2, Issue 24 — the recovered root install raises the
/// node-seq handle and verifies the pointer.** The fn-level pin of the
/// ONE install both the own-residue open and the recovery driver run: a
/// tree opened at a root of seq 7 with the handle at 0 takes a page root
/// of seq 42 — the handle reads ≥ 42 after (RED before the fix: the
/// driver's arm stored the root and left the handle where it was, so a
/// lessee that died with an EMPTY window — no window record to raise it
/// — left the recoverer minting below the dead tree's stamps, PR 11's
/// residue-seq collision class); a pointer whose seq the extent does not
/// hold is REFUSED (`open_inner`'s law — a torn page word never installs
/// an unverified root) and changes nothing; the fresh-tree arm
/// (`open_unpublished_slot_tree`) raises the same word and sets the floor.
#[tokio::test]
async fn the_recovered_root_install_raises_the_node_seq_handle_and_refuses_a_stale_pointer() {
    use squeezefs::meta_backend::kv::node::{
        write_node, NodeLayout, NodeWriteParams, MIN_NODE_SIZE,
    };
    use squeezefs::meta_backend::kv::node_cache::{
        NodeCache, NodeCacheConfig, DEFAULT_WRITEBACK_DELTA_BYTES,
    };
    use squeezefs::meta_backend::kv::record::{guest_forest_slot, KIND_INTERIOR};
    use squeezefs::meta_backend::kv::tree::{KvTree, RootPtr};
    use std::sync::atomic::AtomicU64;
    let file = tempfile::NamedTempFile::new().unwrap();
    let node_size = MIN_NODE_SIZE;
    file.as_file().set_len(4 * node_size as u64).unwrap();
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout: NodeLayout::new(node_size).unwrap(),
        heap_base: 0,
        budget_bytes: 4 * node_size as u64,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    let slot = guest_forest_slot(3);
    for (e, seq) in [(0u64, 7u64), (1, 42)] {
        write_node(
            cache.config().path.clone(),
            &cache.config().layout,
            &NodeWriteParams {
                node_addr: cache.extent_addr(e),
                node_seq: seq,
                tree_id: KIND_INTERIOR,
                level: 0,
                min_key: b"",
                max_key: &[0xff; 8],
            },
            &[],
            0,
        )
        .await
        .unwrap();
    }
    let stale = RootPtr {
        addr: cache.extent_addr(0),
        seq: 7,
    };
    let newer = RootPtr {
        addr: cache.extent_addr(1),
        seq: 42,
    };
    let handle = Arc::new(AtomicU64::new(0));
    let tree = KvTree::open_slot_tree(Arc::clone(&cache), slot, stale, Arc::clone(&handle))
        .await
        .unwrap();
    // A pointer the extent does not hold: refused, nothing installed.
    let torn = RootPtr {
        addr: newer.addr,
        seq: 41,
    };
    assert!(
        matches!(
            tree.install_recovered_root(torn, 9).await,
            Err(KvError::Corrupt(_))
        ),
        "a stale page word never installs an unverified root"
    );
    assert_eq!(tree.root(), stale);
    assert_eq!(tree.root_floor(), 0);
    // The verified install: the root, the floor AND the handle.
    tree.install_recovered_root(newer, 9).await.unwrap();
    assert_eq!(tree.root(), newer);
    assert_eq!(tree.root_floor(), 9);
    assert!(
        handle.load(Ordering::Acquire) >= 42,
        "the node-seq handle is raised to the installed root's seq (read {})",
        handle.load(Ordering::Acquire)
    );
    // The fresh-tree arm: the same word, the same floor.
    let handle2 = Arc::new(AtomicU64::new(0));
    let fresh = KvTree::open_unpublished_slot_tree(
        Arc::clone(&cache),
        slot,
        newer,
        Arc::clone(&handle2),
        11,
    )
    .await
    .unwrap();
    assert_eq!(fresh.root(), newer);
    assert_eq!(fresh.root_floor(), 11);
    assert!(handle2.load(Ordering::Acquire) >= 42);
}

/// **Review round 2, Issue 24 — the driver's arm: an EMPTY-window death.**
/// The lessee dies right after its checkpoint (no window record raises
/// the recoverer's handle); the recovery installs its newer root and the
/// recoverer's node-seq handle reads at or above every node seq the
/// recovered tree carries — so its next mint is strictly above them
/// (pinned: a create + checkpoint after the recovery moves the handle
/// past that maximum, and every reachable node still reads at or below
/// it). In THIS process the lessee and the recoverer shared one handle
/// through the ledger's watermark (the manager's checkpoints wrote it),
/// so the row is green on both sides of the fix; the fn-level pin above
/// is the red one — this row pins that the driver's arm runs it on the
/// empty-window shape and that the recovered tree is exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_window_death_leaves_the_recoverers_node_seqs_above_the_recovered_trees() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends_with(dir.path(), TwoBackendsWindow::Empty).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let tree = fx.vol.slot_tree(fx.slot).unwrap();
    assert!(tree.root().seq >= fx.newer_root.seq);
    let max_seq = async |t: &Arc<squeezefs::meta_backend::kv::tree::KvTree>| -> u64 {
        let mut max = 0u64;
        for addr in t.reachable_node_addrs().await.unwrap() {
            max = max.max(fx.vol.node_cache().get(addr).await.unwrap().node_seq());
        }
        max
    };
    let tree_max = max_seq(&tree).await;
    let handle = fx.vol.test_node_seq_now();
    assert!(
        handle >= tree_max,
        "the recoverer's handle ({handle}) is at or above the recovered tree's max node seq \
         ({tree_max}) with NO window record to raise it"
    );
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    // The next mints sit strictly above: a create + checkpoint.
    fx.routed
        .create(fx.dir, "after-recovery", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    fx.vol.checkpoint_now().await.unwrap();
    let handle_after = fx.vol.test_node_seq_now();
    assert!(handle_after >= handle);
    let tree_max_after = max_seq(&fx.vol.slot_tree(fx.slot).unwrap()).await;
    assert!(
        tree_max_after <= handle_after && tree_max_after >= tree_max,
        "every reachable node reads at or below the handle ({tree_max_after} ≤ {handle_after})"
    );
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Review round 2, Issue 26 — a dentry DELETE in a foreign window scopes
/// its child out of the inode plane.** The lessee UNLINKS a file its last
/// checkpoint holds and dies: the dentry's DELETE sits in its ring (no
/// value — the child's ino is in the tree's pre-delete dentry alone) while
/// the child's `nlink 1 → 0` rode ring 0, which the recoverer's open
/// replayed. The recoverer's online inode plane sees a name still
/// referencing an `nlink 0` record — C10's LOSS-direction finding
/// (`C10ZeroNlinkNamed`, "DATA-LOSS RISK") on a healthy fleet, until the
/// holder's checkpoint — unless the window's DELETE scopes the child:
/// `foreign_window_inos` resolves the deleted key in the parent's tree
/// (RED before: the DELETE contributed nothing and the plane reported
/// exactly that finding). After the recovery the name is gone and the
/// plane has nothing to scope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dentry_delete_in_a_foreign_window_scopes_its_child_out_of_the_inode_plane() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends_with(dir.path(), TwoBackendsWindow::UnlinkOne).await;
    let width = fx.routed.routing_width();
    let scoped = fx.vol.foreign_window_inos(width).await.unwrap();
    assert!(
        !scoped.is_empty(),
        "the window's one record is a dentry DELETE — its child is scoped"
    );
    let report = inode_plane_over(&fx.routed).await;
    assert_eq!(
        report.counters.dangling_dentries, 0,
        "no dangling-dentry finding for the unlinked child while its DELETE is in flight: \
         {:?}",
        report.findings
    );
    assert_eq!(
        report.counters.nlink_mismatch_low, 0,
        "{:?}",
        report.findings
    );
    // The arm the un-fixed tree tripped: the child's record stands at
    // `nlink 0` (its destroy is the reclaim's) under the name the DELETE
    // has not yet removed here — `C10ZeroNlinkNamed`, "DATA-LOSS RISK".
    assert_eq!(report.counters.nlink_zero_named, 0, "{:?}", report.findings);
    assert!(
        report.counters.inode_plane_window_scoped >= 1,
        "the child was scoped out ({:?})",
        report.counters
    );
    // No C10 finding of any direction. (C9 reads the fixture's premise
    // here — the recoverer's RAM tree of the slot is at the grant-time
    // root while the lessee's later checkpoint named the newer one on its
    // page alone, so the names it holds are invisible to this plane until
    // the recovery installs it; the class this row pins is C10's.)
    assert!(
        report.findings.iter().all(|f| f.class != "C10"),
        "{:?}",
        report.findings
    );
    // The recovery replays the DELETE: the name is gone, the plane clean
    // with nothing to scope.
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(fx.vol.foreign_window_inos(width).await.unwrap().is_empty());
    let report = inode_plane_over(&fx.routed).await;
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.counters.inode_plane_window_scoped, 0);
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// The fsck engine's INODE PLANE over an already-open WRITER set (the
/// recoverer's online plane — where PR 10's window scoping governs; a
/// probe's plane records no verdict over a `Live` page, PR 7b's law): a
/// data router with no staging, the inode-plane-only options.
async fn inode_plane_over(routed: &Arc<RoutedMetaBackend>) -> squeezefs::fsck::FsckReport {
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-plane")
            .await
            .unwrap(),
    );
    let dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new("/dev/null"));
    let cache = squeezefs::cache::TieredCache::new(
        Vec::new(),
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm, cache, alloc, dev);
    router.set_meta_backend(Arc::clone(routed));
    let ctx = squeezefs::fsck::FsckCtx {
        meta: Arc::clone(routed),
        router,
        staging_dirs: Vec::new(),
        expected_generation: None,
    };
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    opts.inode_plane_only = true;
    squeezefs::fsck::run(&ctx, &opts)
        .await
        .expect("the engine runs")
}

/// **Issue 3 — the door during a recovery.** While the recovery is parked
/// right before its tree-0 write (the slot `Releasing { dead }` in the RAM
/// table, every durable step but tree 0 done), a create in the dead
/// directory — a FIRST-TOUCH acquire of the recovering slot at the commit
/// door — is REFUSED (the mid-handover verdict), tree 0 still `Leased {
/// dead, g }`; once the recovery completes the same create lands as the
/// manager's first-touch acquire at exactly `g + 1`. Before the fix the
/// table was released at step 4 and the acquire in this window wrote
/// `Leased { 0, g + 1 }` that step 7 then regressed to `Unleased { g }`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_touch_acquire_during_a_recovery_is_refused_and_never_regresses_tree_0() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(true, Ordering::SeqCst);
    let routed = Arc::clone(&fx.routed);
    let poll = tokio::spawn(async move { recover_dead_appenders_set(&routed).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !recovery::TEST_RECOVERY_HELD.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the recovery never parked"
        );
        tokio::task::yield_now().await;
    }
    // The concurrent create: refused, and tree 0 untouched.
    let late = fx
        .routed
        .create(fx.dir, "late", libc::S_IFREG | 0o644, 1000, 1000)
        .await;
    assert!(
        late.is_err(),
        "a first-touch acquire mid-recovery was admitted"
    );
    match tree0_state(&fx.vol, fx.slot).await {
        Some(SlotState::Leased {
            appender_id: 1, g, ..
        }) => assert_eq!(g, fx.g),
        other => panic!("tree 0 moved under the recovery: {other:?}"),
    }
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    let rep = poll.await.unwrap().unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    // The same create lands now — the manager's first-touch acquire at
    // exactly g + 1.
    let late = fx
        .routed
        .create(fx.dir, "late", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the create lands after the recovery");
    assert_eq!(
        fx.routed.lookup(fx.dir, "late").await.unwrap().ino,
        late.ino
    );
    match tree0_state(&fx.vol, fx.slot).await {
        Some(SlotState::Leased {
            appender_id: 0, g, ..
        }) => assert_eq!(g, fx.g + 1),
        other => panic!("{other:?}"),
    }
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// PR 9's writer-side recall sink for the recoverer's slot-custody arm: a
/// recall drains nothing here (no data plane under the fixture).
struct NoopRecallSink;

impl squeezefs::meta_ship::token_plane::RecallDataSink for NoopRecallSink {
    fn drain_and_purge<'a>(
        &'a self,
        _objects: &'a [squeezefs::meta_ship::token_plane::RecalledObject],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

/// Failure hygiene for the custody pin: the arm and the owner are
/// process globals — a panic mid-contract must not leave them to the next
/// one.
struct CustodyArmGuard;

impl Drop for CustodyArmGuard {
    fn drop(&mut self) {
        squeezefs::data_grant::uninstall_slot_custody();
        squeezefs::data_grant::uninstall_custody_owner();
    }
}

/// **The rebase onto PR 9 (custody by the slot holder) — seams (a)/(b): a
/// custody grant INSIDE a recovery is the same class as one inside a
/// handover.** The recovery arms no mid-handover mark
/// (`HandoverCustodyMark` is `release_slot_handover_locked`'s, which the
/// driver never runs): its door is the lease TABLE — the slot `Releasing
/// { dead }` from step 4 to the terminal outcome — and BOTH grant paths
/// consult it before anything is granted: the served `CustodyGrant`'s
/// `foreign_slot_holder` answers `NotHolder { dead }` (nothing granted at
/// the recoverer), and the local acquire's `slot_holder_home` resolves the
/// DEAD holder — unbound here, the natural post-death state (PR 9's holder
/// fence forgets a dead holder's dial slot; the census binding is PR 12's)
/// — so `SlotLockManager::acquire_lock` refuses EAGAIN-class without
/// dialing anybody, never falling to the local arbiter. The recoverer
/// holds no custody of the dead slot's files (nothing to recall: the dead
/// holder's grants died with its process and are retired at their WRITERS
/// by the S9 law — PR 9's `a_dead_holders_t_self_fence_…`; the driver's
/// preempt fences the dead HOLDER's registrant). Once the recovery
/// completes the slot is the manager's: the same acquire lands at the
/// local arbiter with no holder dialed, and no mark was ever armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_custody_grant_of_a_slot_mid_recovery_is_the_dead_holders_never_the_recoverers() {
    use squeezefs::data_grant::{self, CustodyHome, WriteCustodyOwner};
    use squeezefs::membership::{LeaseClock, LeaseClocks};
    use squeezefs::meta_backend::crossvol_tx::{step_home, StepHome};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Two regular files OF the slot (preset by the seeding manager): the
    // custody objects. The lessee's own creates put only DENTRIES in its
    // tree — a shipped create's child is minted in the creator's rotor.
    let fx = two_backends_seeded(dir.path(), TwoBackendsWindow::Creates(15), 2).await;
    // The recoverer as a slot holder: PR 9's S9 custody owner (the S9
    // multi-writer arm's half) + the slot-custody arm (the mount path's),
    // both process globals.
    let _hygiene = CustodyArmGuard;
    let volume_uuid = u128::from_le_bytes(fx.vol.superblock().uuid);
    let (_, ino) = fx.seeded[0];
    let (v, local) = fx.routed.route_ino(ino);
    assert_eq!(
        squeezefs::meta_backend::kv::record::forest_slot_of_ino(local),
        fx.slot,
        "premise: the custody object is a regular file OF the recovering slot"
    );
    // The premise read runs BEFORE the custody arm: with it armed, a
    // writer's read of an object in a slot a dead appender leases is a
    // TOKEN read at that holder (PR 12b's divert) and refuses EAGAIN
    // while no endpoint is bound — never the projection (KD-SYM-19).
    assert_eq!(
        fx.routed.getattr(ino).await.expect("its record").mode & libc::S_IFMT,
        libc::S_IFREG,
        "premise: a custody object is a regular file"
    );
    let owner = WriteCustodyOwner::arm(
        "recoverer",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        LeaseClocks::with_params(
            std::time::Duration::from_millis(3_000),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(400),
        )
        .expect("2*skew + purge < TTL"),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the recoverer's custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&owner));
    let sink = Arc::new(NoopRecallSink);
    data_grant::arm_slot_custody(
        &fx.routed,
        "recoverer",
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let via0 = data_grant::stats().via_slot_holder;

    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(true, Ordering::SeqCst);
    let routed = Arc::clone(&fx.routed);
    let poll = tokio::spawn(async move { recover_dead_appenders_set(&routed).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !recovery::TEST_RECOVERY_HELD.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the recovery never parked"
        );
        tokio::task::yield_now().await;
    }
    // Mid-recovery: the table names the dead holder, both consults follow
    // it, no mark is armed, and the recoverer holds nothing to recall.
    assert_eq!(
        fx.vol.foreign_slot_holder(local),
        Some(1),
        "the served CustodyGrant answers NotHolder {{ dead }} mid-recovery"
    );
    assert!(
        matches!(
            step_home(&fx.routed, v, local),
            StepHome::Unreachable { holder: 1 }
        ),
        "the local acquire resolves the dead holder (unbound)"
    );
    assert!(
        matches!(
            data_grant::slot_holder_home(ino),
            Some(CustodyHome::Unbound { holder: 1 })
        ),
        "the custody home is the dead holder's, never this mount's"
    );
    assert_eq!(
        data_grant::handover_recalls_pending(),
        0,
        "the recovery arms no mid-handover mark — the table is its door"
    );
    assert!(
        !data_grant::slot_custody_live(volume_uuid, fx.slot),
        "the recoverer issued no grant on the dead slot's files — nothing to recall"
    );
    let err = dlm
        .acquire_lock(
            &squeezefs::keys::inode_path(ino),
            None,
            std::time::Duration::from_millis(500),
        )
        .await
        .expect_err("no custody of a recovering slot's file is granted at the recoverer");
    assert!(
        matches!(
            &err,
            squeezefs::error::SqueezefsError::Refused { errno, .. } if *errno == libc::EAGAIN
        ),
        "the dead holder's refusal is typed EAGAIN (the caller retries): {err:?}"
    );
    assert_eq!(
        data_grant::stats().via_slot_holder,
        via0,
        "no holder was dialed (the dead one is unbound)"
    );
    assert_eq!(owner.held(), 0, "the recoverer's authority granted nothing");
    match tree0_state(&fx.vol, fx.slot).await {
        Some(SlotState::Leased {
            appender_id: 1, g, ..
        }) => assert_eq!(g, fx.g),
        other => panic!("tree 0 moved under the recovery: {other:?}"),
    }

    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    let rep = poll.await.unwrap().unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");

    // After the terminal outcome: the slot is the manager's — every
    // consult answers "ours", the same acquire lands at the local arbiter
    // with no holder dialed, and still no mark.
    assert_eq!(fx.vol.foreign_slot_holder(local), None);
    assert!(matches!(step_home(&fx.routed, v, local), StepHome::Local));
    assert!(data_grant::slot_holder_home(ino).is_none());
    let lease = dlm
        .acquire_lock(
            &squeezefs::keys::inode_path(ino),
            None,
            std::time::Duration::from_secs(2),
        )
        .await
        .expect("custody of the recovered slot's file is the local arbiter's");
    assert!(lease.is_held().await);
    assert_eq!(data_grant::stats().via_slot_holder, via0);
    assert_eq!(data_grant::handover_recalls_pending(), 0);
    assert!(!data_grant::slot_custody_live(volume_uuid, fx.slot));
    drop(lease);
    data_grant::disarm_slot_custody().await;
    data_grant::uninstall_custody_owner();
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    assert_all_resolve(&fx.routed, ROOT_INO, &fx.seeded).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Issue 8 — the decision under the mutex.** The poll snapshots the
/// dead page, then — before it takes the handover mutex — the page MOVES
/// (another actor's act: here the operator's `Recovered` write, the shape
/// the mount-path gate or an online fsck repair leaves). The recovery
/// re-reads the page under the mutex, finds it no longer `Live` /
/// `Recovering` and SKIPS (`skipped`, nothing written, the tree untouched);
/// the same skip covers a page whose identity or term moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_that_moves_under_the_polls_snapshot_is_skipped_not_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    fx.vol.record_death_with_key(fx.x, 1, 0).await.unwrap();
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(true, Ordering::SeqCst);
    let routed = Arc::clone(&fx.routed);
    let poll = tokio::spawn(async move { recover_dead_appenders_set(&routed).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !recovery::TEST_RECOVERY_HELD.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the poll never parked"
        );
        tokio::task::yield_now().await;
    }
    // (a) the page's TERM moves (a rejoin of the same identity elsewhere).
    rewrite_page(&fx.uris[0], 1, |p| p.term += 1).await;
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    let recs0 = recoveries();
    let rep = poll.await.unwrap().unwrap();
    assert_eq!(rep.recovered(), 0, "{rep:?}");
    assert_eq!(rep.per_volume[0].1.skipped, 1, "{rep:?}");
    assert_eq!(recoveries(), recs0);
    assert_eq!(
        page_state(&fx.uris[0], &fx.vol, 1).await,
        Some(AppenderState::Live),
        "nothing written to the page"
    );
    assert_eq!(
        fx.vol.slot_tree(fx.slot).map(|t| t.root()),
        Some(fx.stale_root)
    );
    // (b) the page's STATE moves (another actor recovered it).
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(true, Ordering::SeqCst);
    let routed = Arc::clone(&fx.routed);
    let poll = tokio::spawn(async move { recover_dead_appenders_set(&routed).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !recovery::TEST_RECOVERY_HELD.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the poll never parked"
        );
        tokio::task::yield_now().await;
    }
    rewrite_page(&fx.uris[0], 1, |p| p.state = AppenderState::Recovered).await;
    recovery::TEST_RECOVERY_HOLD_BEFORE_REREAD.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    let rep = poll.await.unwrap().unwrap();
    assert_eq!(rep.recovered(), 0, "{rep:?}");
    assert_eq!(rep.per_volume[0].1.skipped, 1, "{rep:?}");
    // The snapshot's own state is restored for the teardown's fsck (the
    // page back to Live, the term as the lessee left it) and the region
    // recovered for real.
    rewrite_page(&fx.uris[0], 1, |p| {
        p.state = AppenderState::Live;
        p.term -= 1;
    })
    .await;
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Issue 9 — the incarnation.** A member the ledger names dead REJOINS:
/// (a) the installed owner lists its id LIVE again — the poll RETIRES the
/// record (`dead_member_records_retired`) and recovers nothing, the page
/// untouched; (b) a writer arming on the set retires any record naming
/// ITS OWN identity (`retire_own_death_records`) — idempotent, `false` on
/// a set that names it nowhere. Before the fix a rejoined `(node, slot)`
/// was recovered while live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoined_member_is_never_recovered_and_its_record_is_retired() {
    use squeezefs::membership::{
        JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    // The fixture's parked cadence (60 s) would derive a `D_purge` no
    // lease clock admits; B read it at its open, the owner below must not.
    std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    fx.vol.record_death_with_key(fx.x, 3, 0).await.unwrap();
    // (a) X rejoins the owner: live again.
    let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let owner = MembershipOwner::arm(
        "o",
        1,
        0,
        LeaseClocks::derive(std::time::Duration::ZERO).unwrap(),
        LeaseClock::manual(Arc::clone(&now)),
    )
    .unwrap();
    squeezefs::membership::install_owner(Arc::clone(&owner));
    let member = squeezefs::cowriter::node_member_id_of(fx.x.node_token, fx.x.mount_slot);
    let JoinOutcome::Granted(_) = owner.join(JoinRequest {
        id: member.clone(),
        role: MemberRole::Writer,
        endpoint: None,
        pid: 1,
        boot: "b".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("the rejoin is granted")
    };
    let retired0 = alloc_lease::DEAD_MEMBER_RECORDS_RETIRED.load(Ordering::Relaxed);
    let recs0 = recoveries();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 0, "{rep:?}");
    assert_eq!(rep.per_volume[0].1.skipped, 1, "{rep:?}");
    assert_eq!(recoveries(), recs0);
    assert!(
        fx.vol.dead_member_record(&fx.x).await.unwrap().is_none(),
        "the record is retired"
    );
    assert_eq!(
        alloc_lease::DEAD_MEMBER_RECORDS_RETIRED.load(Ordering::Relaxed),
        retired0 + 1
    );
    assert_eq!(
        page_state(&fx.uris[0], &fx.vol, 1).await,
        Some(AppenderState::Live),
        "the live region is left to its holder"
    );
    assert!(matches!(
        tree0_state(&fx.vol, fx.slot).await,
        Some(SlotState::Leased { appender_id: 1, .. })
    ));
    // The next poll: nothing named, nothing recovered.
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!((rep.recovered(), rep.per_volume[0].1.skipped), (0, 0));
    squeezefs::membership::uninstall();
    // (b) the writer's own retirement at its arm: a record naming THIS
    // mount's identity (a predecessor incarnation's death) is retired.
    let me = fx.vol.appenders_public().unwrap().identity;
    fx.vol.record_death_with_key(me, 9, 0).await.unwrap();
    assert!(recovery::retire_own_death_records(&fx.routed)
        .await
        .unwrap());
    assert!(fx.vol.dead_member_record(&me).await.unwrap().is_none());
    assert!(!recovery::retire_own_death_records(&fx.routed)
        .await
        .unwrap());
    // Teardown: X really dies now, the region recovered for the fsck.
    fx.vol.record_death_with_key(fx.x, 4, 0).await.unwrap();
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Issue 5 — the death write retried until durable.** A death the sink
/// could not write is PARKED (`dead_member_write_deferrals`) and the next
/// ledger projection lands it (`deaths_landed`), then acts on it in the
/// same pass; the pending set is empty after. (The admission itself PARKS
/// on a full ring — the PR-4 door's pre-admission — so this arm is the
/// device-error class.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_death_record_lands_at_the_next_ledger_poll() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let fx = two_backends(dir.path()).await;
    let deferrals0 = alloc_lease::DEAD_MEMBER_WRITE_DEFERRALS.load(Ordering::Relaxed);
    alloc_lease::defer_death_record(alloc_lease::PendingDeath {
        member: fx.x,
        epoch: 5,
        pr_key: 0,
    });
    // Idempotent over the same identity.
    alloc_lease::defer_death_record(alloc_lease::PendingDeath {
        member: fx.x,
        epoch: 5,
        pr_key: 0,
    });
    assert_eq!(alloc_lease::test_pending_deaths().len(), 1);
    assert_eq!(
        alloc_lease::DEAD_MEMBER_WRITE_DEFERRALS.load(Ordering::Relaxed),
        deferrals0 + 2
    );
    assert!(fx.vol.dead_member_record(&fx.x).await.unwrap().is_none());
    let rep = recover_dead_appenders_set(&fx.routed).await.unwrap();
    assert_eq!(rep.deaths_landed, 1, "{rep:?}");
    assert_eq!(
        rep.recovered(),
        1,
        "the landed death is acted on in the same pass: {rep:?}"
    );
    assert!(alloc_lease::test_pending_deaths().is_empty());
    assert_eq!(
        fx.vol
            .dead_member_record(&fx.x)
            .await
            .unwrap()
            .unwrap()
            .epoch,
        5
    );
    assert_all_resolve(&fx.routed, fx.dir, &fx.files).await;
    shutdown(&fx.routed).await;
    let TwoBackends {
        uris, routed, vol, ..
    } = fx;
    drop(vol);
    drop(routed);
    fsck_clean(&uris).await;
    two_backends_teardown();
}

/// **Issue 13 — the unarmed pin the AGENTS paragraph claims.** On a FLAT
/// volume and on an UNARMED forest the driver is inert: `recovery::arm`
/// installs nothing and recovers nothing, the projection is empty, the
/// published bound reads 0 on flat, every Recovery gauge stays where it
/// was, and fsck's foreign-window scoping has nothing to scope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flat_volume_and_an_unarmed_forest_arm_no_recovery_driver() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let before = recovery_stats();
    let polls0 = before.ledger_polls;
    // (a) flat — the seam cleared.
    {
        let p = dir.path().join("flat");
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        let plan = squeezefs::meta_backend::plan_meta_slot_set(1).unwrap();
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            &p,
            VOL_LEN,
            &set_opts(),
            plan.stamps[0].clone(),
        )
        .await
        .unwrap();
        let uris = vec![p.display().to_string()];
        let routed = open_under(&uris, &Knobs::unarmed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        assert!(!vol.superblock().symmetric_forest_stamped());
        let rep = recovery::arm(&routed).await.unwrap();
        assert_eq!(rep, recovery::RecoverySetReport::default());
        assert_eq!(vol.appender_recovery_bound_ms(), 0);
        assert!(vol
            .foreign_window_inos(routed.routing_width())
            .await
            .unwrap()
            .is_empty());
        assert!(vol.dead_member_records().await.unwrap().is_empty());
        shutdown(&routed).await;
    }
    // (b) an unarmed forest.
    {
        let fdir = dir.path().join("forest");
        std::fs::create_dir_all(&fdir).unwrap();
        let uris = format_stamped_set_with_config(&fdir, 1).await;
        let routed = open_under(&uris, &Knobs::unarmed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        assert!(!vol.slot_lease_armed());
        let rep = recovery::arm(&routed).await.unwrap();
        assert_eq!(rep, recovery::RecoverySetReport::default());
        assert!(vol
            .foreign_window_inos(routed.routing_width())
            .await
            .unwrap()
            .is_empty());
        let rep = recover_dead_appenders_set(&routed).await.unwrap();
        assert_eq!(rep.recovered(), 0);
        shutdown(&routed).await;
    }
    let after = recovery_stats();
    assert_eq!(after.recoveries, before.recoveries);
    assert_eq!(after.preempts, before.preempts);
    assert_eq!(after.regions_released, before.regions_released);
    assert_eq!(after.intents_rolled_forward, before.intents_rolled_forward);
    // The unarmed forest's explicit projection counted one poll; the arm
    // counted none (inert).
    assert_eq!(after.ledger_polls, polls0 + 1);
    reset_process_state();
}
