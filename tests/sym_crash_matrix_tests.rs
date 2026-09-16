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
use squeezefs::meta_backend::kv::alloc_lease;
use squeezefs::meta_backend::kv::appender::{read_directory, AppenderIdentity, AppenderState};
use squeezefs::meta_backend::kv::backend::recovery::{
    self, mount_path_custody_gate, recover_dead_appenders_set, recovery_stats,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
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
    squeezefs::membership::clear_death_sinks();
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
    assert_eq!(
        ph[0] + ph[1] + ph[2] + ph[3] + ph[4] + ph[5] <= ph[6] + 6,
        true,
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
    restamp_page_state(&uris[0], 1, AppenderState::Recovering).await;
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

/// Re-stamp appender `id`'s page STATE (the census fixture).
async fn restamp_page_state(uri: &str, id: u32, state: AppenderState) {
    use squeezefs::meta_backend::kv::appender::write_page;
    use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
    let path = std::path::Path::new(uri);
    let VolumeFormat::V3(sb) = classify_volume(path).await.unwrap() else {
        panic!("v3");
    };
    let entries = read_directory(path, &sb).await.unwrap();
    let e = entries.iter().find(|e| e.appender_id == id).unwrap();
    let mut page = e.page.clone().unwrap();
    page.state = state;
    for off in e.dir_offsets {
        page.generation += 1;
        write_page(path, off, page.encode().unwrap()).await.unwrap();
    }
    squeezefs::uring_fs::fdatasync(path.to_path_buf())
        .await
        .unwrap();
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
    match vol.screen_record_death(&ident(20).into()) {
        Err(KvError::Rejected(m)) => assert!(m.contains("LIVE"), "{m}"),
        other => panic!("a live member's death was admitted: {other:?}"),
    }
    let me = vol.appenders_public().unwrap().identity;
    match vol.screen_record_death(&me.into()) {
        Err(KvError::Rejected(m)) => assert!(m.contains("own identity"), "{m}"),
        other => panic!("our own death was admitted: {other:?}"),
    }
    assert!(vol.screen_record_death(&ident(99).into()).is_ok());
    shutdown(&routed).await;
    reset_process_state();
}
