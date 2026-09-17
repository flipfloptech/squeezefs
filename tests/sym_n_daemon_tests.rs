//! **Symmetric PR 12b — the N-daemon posture** (`docs/design-symmetric-
//! metadata.md` §7.3 / §5.1 / §5.3 / §5.9; PR-plan row 12b): the fifth
//! door, `KvMetaBackend::open_joined_appender`, exercised by TWO and THREE
//! REAL `KvMetaBackend`s open on ONE file-backed volume AT ONCE in one
//! process — the manager through `open` (the D0 ladder), every joiner
//! through the joined door over a real `cluster_wire` session to the
//! manager's listener — all committing into their OWN rings.
//!
//! PR 10's two-backend fixture put a recoverer's RAM tree in another
//! daemon's state and found four defects; this venue is stronger — the
//! joiner is a live writer whose every act travels the wire, so every
//! "this mount is the manager" assumption in PR 2–12's code meets its
//! first NON-manager here.
//!
//! Identity: on one host every daemon shares the node token, so the
//! fixture's joiners carry the manager's node token with their OWN mount
//! slot — exactly the production shape of N daemons on one box.

mod common;

use common::sym::*;
use squeezefs::meta_backend::kv::appender::{read_directory, AppenderIdentity, AppenderState};
use squeezefs::meta_backend::kv::backend::recovery::{recover_dead_appenders_set, recovery_stats};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::kv::slot_state::SlotState;
use squeezefs::meta_backend::{
    open_routed_meta_set_joined, JoinedSetAdmission, Metadata, RoutedMetaBackend,
};
use squeezefs::park_gate;
use std::sync::Arc;

/// Forest slot 4 (routing slot 3) — the seeded directory's slot.
const SLOT_A: ForestSlot = 4;
/// Forest slot 9 (routing slot 8) — a second seeded directory's slot.
const SLOT_B: ForestSlot = 9;

fn reset_process_state() {
    let _ = env_logger::builder().is_test(true).try_init();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    squeezefs::data_alloc_bitmap::test_clear_replayed_deltas();
    squeezefs::block_grant::test_clear_free_targets();
    park_gate::test_reset();
    squeezefs::data_custody::test_clear_poison();
    squeezefs::data_grant::test_clear_custody_quarantine();
    squeezefs::membership::test_clear_death_sinks();
    squeezefs::membership::uninstall();
}

/// The joiner's identity: the manager's NODE (one host) at its own mount
/// slot `n` — what tells N daemons' pages apart on one box.
async fn joiner_identity(manager: &KvMetaBackend, n: u32) -> AppenderIdentity {
    let node = read_directory(manager.device_path(), manager.superblock())
        .await
        .expect("directory")
        .iter()
        .find(|e| e.appender_id == 0)
        .and_then(|e| e.page.as_ref())
        .map(|p| p.identity.node_token)
        .expect("the manager's page");
    AppenderIdentity {
        node_token: node,
        mount_slot: 0x5000 + n,
        writer_id: 0,
    }
}

/// Open joiner `n` against the manager's venue.
async fn join(
    uris: &[String],
    venue: &HoldersVenue,
    manager: &KvMetaBackend,
    n: u32,
) -> Arc<RoutedMetaBackend> {
    Knobs::armed().apply();
    let r = open_routed_meta_set_joined(
        uris,
        &JoinedSetAdmission {
            manager_endpoint: venue.endpoint(),
            secret: VENUE_SECRET.to_vec(),
            peer_id: format!("joiner-{n}"),
            identity: joiner_identity(manager, n).await,
        },
    )
    .await;
    Knobs::clear();
    r.expect("the joined open")
}

/// [`join`] whose refusal is the contract's subject.
async fn try_join(
    uris: &[String],
    venue: &HoldersVenue,
    manager: &KvMetaBackend,
    n: u32,
) -> Result<Arc<RoutedMetaBackend>, String> {
    Knobs::armed().apply();
    let r = open_routed_meta_set_joined(
        uris,
        &JoinedSetAdmission {
            manager_endpoint: venue.endpoint(),
            secret: VENUE_SECRET.to_vec(),
            peer_id: format!("joiner-{n}"),
            identity: joiner_identity(manager, n).await,
        },
    )
    .await;
    Knobs::clear();
    r.map_err(|e| e.to_string())
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

async fn page_of(
    uri: &str,
    vol: &KvMetaBackend,
    id: u32,
) -> Option<squeezefs::meta_backend::kv::appender::AppenderPage> {
    read_directory(std::path::Path::new(uri), vol.superblock())
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.appender_id == id)
        .and_then(|e| e.page)
}

/// One stamped volume with a directory seeded in `slot` and the slot
/// released to tree 0 (the joiner's first touch takes it).
async fn seeded_volume(
    dir: &std::path::Path,
    slots: &[(ForestSlot, &str)],
) -> (Vec<String>, Vec<u64>) {
    let uris = format_stamped_set_with_config(dir, 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let mut dirs = Vec::new();
    for (slot, name) in slots {
        dirs.push(seed_dir_in_slot(&routed, 0, *slot, name).await);
    }
    let vol = Arc::clone(&routed.volumes[0]);
    for (slot, _) in slots {
        vol.release_slot_handover(0, *slot)
            .await
            .expect("release to unleased");
    }
    shutdown(&routed).await;
    drop(vol);
    drop(routed);
    (uris, dirs)
}

async fn create_files(
    routed: &RoutedMetaBackend,
    dir: u64,
    prefix: &str,
    n: usize,
) -> Vec<(String, u64)> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("{prefix}{i:04}");
        let ino = routed
            .create(dir, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"))
            .ino;
        out.push((name, ino));
    }
    out
}

/// Every acked `(name, ino)` resolves through the routed layer and has its
/// inode record. On a miss the panic names the slot's lease state and the
/// tree's root: the two words that attribute "the tree moved away" versus
/// "the tree is stale in this daemon's cache" (the class the first pin
/// found — see `KvMetaBackend::adopt_transferred_slot_tree`).
async fn assert_all_resolve(routed: &RoutedMetaBackend, dir: u64, files: &[(String, u64)]) {
    for (name, ino) in files {
        let got = match routed.lookup(dir, name).await {
            Ok(g) => g,
            Err(e) => {
                let (v, local) = routed.route_ino(dir);
                let vol = &routed.volumes[v];
                let slot = squeezefs::meta_backend::kv::record::forest_slot_of_ino(local);
                let listing = vol.readdir_page(local, 0, 1000).await.map(|p| p.len());
                let leased = vol
                    .slot_leases()
                    .map(|p| (p.gate.is_leased(slot), p.table.resolve(slot)));
                let root = vol.slot_tree(slot).map(|t| t.root());
                panic!(
                    "acked file {name} lost on {}: {e}; listing {listing:?}; slot {slot} lease \
                     {leased:?}; root {root:?}",
                    vol.device_path().display()
                );
            }
        };
        assert_eq!(got.ino, *ino, "{name} resolves to another ino");
        routed.getattr(*ino).await.expect("its inode record");
    }
}

/// The symmetric must-stay-0 set on one volume.
fn assert_must_stay_zero(vol: &KvMetaBackend, who: &str) {
    let s = vol.appender_stats().expect("a forest volume");
    assert_eq!(s.manager_verb_refusals, 0, "{who}: manager_verb_refusals");
    assert_eq!(s.flush_ceiling_overruns, 0, "{who}: flush_ceiling_overruns");
    if let Some(l) = vol.slot_lease_stats() {
        assert_eq!(l.conflicts, 0, "{who}: slot_lease_conflicts");
    }
    if let Some(j) = vol.joined_stats() {
        assert_eq!(j.control_refusals, 0, "{who}: joined_control_refusals");
        assert_eq!(j.wire_failures, 0, "{who}: joined_wire_failures");
    }
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_LEAF_LEASE_REFUSALS
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "{who}: meta_kv_leaf_lease_refusals"
    );
}

// ---------------------------------------------------------------------------
// Deliverables 1–3: the fifth door, its own ring, its own checkpoint.
// ---------------------------------------------------------------------------

/// **The first pin**: a second daemon JOINS the armed volume over the
/// wire (`JoinAppender` + `AcquireSlots` to the manager's listener), takes
/// no flock and no claim, reads the manager's ledger + tree 0 as a
/// projection, first-touch-acquires the released slot over the wire at
/// its first create, commits every create into ITS OWN ring (the
/// manager's ring 0 head does not move for them), runs its OWN checkpoint
/// (its page: `ckpt_seq ≥ 1`, `head_hint` = its head, the slot's root
/// named — PR 4's Issue-31 words), and its gauges read as a NON-manager's
/// (`appender_id ≠ 0`, `manager_lease == peer:…`, `slot_leases_held ≥ M`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_daemon_joins_over_the_wire_and_commits_into_its_own_ring() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];

    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let ms0 = mvol.appender_stats().unwrap();
    assert_eq!(ms0.appender_id, 0);
    let ring0_head_before = mvol.journal_ring().core().head();

    let joiner = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    assert!(jvol.is_joined_appender(), "the joined door's posture");
    let js = jvol.appender_stats().unwrap();
    assert_ne!(js.appender_id, 0, "a joiner is never appender 0");
    assert_eq!(js.live, 1, "one region of its own");
    assert!(
        matches!(
            js.manager_lease,
            squeezefs::meta_backend::kv::appender::ManagerLease::Peer { .. }
        ),
        "the manager lease is the manager's: {:?}",
        js.manager_lease
    );
    assert_eq!(js.self_recoveries, 0, "a fresh join recovers nothing");
    assert_eq!(
        jvol.writer_guard_mode(),
        "joined-appender",
        "its own guarantee row"
    );
    let jl = jvol
        .slot_lease_stats()
        .expect("the plane is armed on the joiner");
    assert!(
        jl.leases_held >= 1,
        "the rotor was acquired over the wire: {}",
        jl.leases_held
    );
    let jw = jvol.joined_stats().expect("the Joined family");
    assert!(jw.wire_acquires >= 1, "AcquireSlots travelled the wire");
    assert_eq!(jw.wire_failures, 0);
    // The manager counted the join and knows one more appender.
    let ms1 = mvol.appender_stats().unwrap();
    assert_eq!(ms1.joins, ms0.joins + 1, "the manager served JoinAppender");
    assert!(ms1.appenders_known >= 2, "{}", ms1.appenders_known);
    // The joiner's projection learns the manager released SLOT_A.
    assert!(
        matches!(
            tree0_state(&jvol, SLOT_A).await,
            Some(SlotState::Unleased { .. })
        ),
        "the projection reads the release"
    );

    // Creates under the seeded directory: the first touch acquires SLOT_A
    // over the wire; every record lands in the joiner's ring.
    let files = create_files(&joiner, shared, "j", 24).await;
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_A).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == js.appender_id
        ),
        "the manager's tree 0 leases SLOT_A to the joiner: {:?}",
        tree0_state(&mvol, SLOT_A).await
    );
    let own = jvol
        .appender_stats()
        .unwrap()
        .regions
        .into_iter()
        .find(|r| r.id == js.appender_id)
        .expect("the own region");
    assert!(
        own.ring_entries >= 24,
        "{} entries in the own ring",
        own.ring_entries
    );
    assert!(own.leases >= 2, "SLOT_A + the rotor: {}", own.leases);
    // The manager's ring moved only for its own control entries (the
    // grants): never a content record of the joiner's.
    let ring0_head_after = mvol.journal_ring().core().head();
    assert!(ring0_head_after >= ring0_head_before);
    let mown = mvol
        .appender_stats()
        .unwrap()
        .regions
        .into_iter()
        .find(|r| r.id == 0)
        .unwrap();
    assert!(
        mown.ring_entries < 24,
        "the joiner's 24 creates did not ride ring 0 ({} entries)",
        mown.ring_entries
    );
    assert_all_resolve(&joiner, shared, &files).await;

    // The joiner's OWN checkpoint writes ITS page with the Issue-31 words.
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &jvol, js.appender_id)
        .await
        .expect("the joiner's page");
    assert_eq!(page.state, AppenderState::Live);
    assert!(page.ckpt_seq >= 1, "ckpt_seq {}", page.ckpt_seq);
    let jring = jvol.ring_of_region(js.appender_id);
    assert_eq!(page.head_hint, jring.core().head(), "head_hint = its head");
    assert_eq!(page.seq_offset, jring.seq_offset(), "seq_offset in force");
    assert!(
        page.slots.iter().any(|e| {
            squeezefs::meta_backend::kv::appender::forest_slot_of_page_slot(e.slot, 0) == SLOT_A
                && e.root.addr != 0
        }),
        "the page names SLOT_A's live root: {:?}",
        page.slots
    );
    assert_eq!(
        page.identity.mount_slot, 0x5001,
        "the joiner's own identity"
    );
    // The manager's page 0 was never written by the joiner: it carries
    // the manager's identity and the manager bit.
    let page0 = page_of(&uris[0], &mvol, 0).await.unwrap();
    assert!(page0.is_manager);
    assert_ne!(page0.identity.mount_slot, 0x5001);
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    // The clean leave: every slot handed to nobody over the wire, the
    // page Free — the manager's tree 0 reads SLOT_A Unleased with the
    // joiner's root, and the joiner's ring extents are back in the heap.
    let free_before = mvol.allocator().free_extents();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert_eq!(
        page_of(&uris[0], &mvol, js.appender_id)
            .await
            .map(|p| p.state),
        Some(AppenderState::Free),
        "LeaveAppender freed the page"
    );
    assert!(
        matches!(tree0_state(&mvol, SLOT_A).await, Some(SlotState::Unleased { root, .. }) if root.addr != 0),
        "SLOT_A released with its root: {:?}",
        tree0_state(&mvol, SLOT_A).await
    );
    assert!(
        mvol.allocator().free_extents() > free_before,
        "the joiner's ring returned to the heap"
    );
    assert_eq!(mvol.appender_stats().unwrap().manager_verb_refusals, 0);
    // The manager re-acquires the slot first-touch and reads every byte
    // the joiner acked (the released root is exact).
    assert_all_resolve(&manager, shared, &files).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **Own residue** (PR 2's law on a wire region, deliverable 4): the joiner
/// commits, checkpoints, commits MORE into its window, and dies (dropped
/// without a shutdown). The same identity's rejoin presents its Live
/// page to `JoinAppender` (`already`), replays the window as its own
/// residue (`appender_self_recoveries == 1`) and serves every acked
/// record; the manager's ring was never involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_rejoin_over_its_live_page_replays_its_window_as_own_residue() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    let joiner = join(&uris, &venue, &mvol, 2).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let covered = create_files(&joiner, shared, "c", 10).await;
    jvol.checkpoint_now().await.unwrap();
    let windowed = create_files(&joiner, shared, "w", 12).await;
    // The kill: no shutdown, no leave — the window stays in the ring.
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert_eq!(
        page_of(&uris[0], &mvol, id).await.map(|p| p.state),
        Some(AppenderState::Live),
        "the dead joiner's page stays Live"
    );

    let again = join(&uris, &venue, &mvol, 2).await;
    let avol = Arc::clone(&again.volumes[0]);
    let s = avol.appender_stats().unwrap();
    assert_eq!(s.appender_id, id, "the rejoin answers the SAME region");
    assert_eq!(s.self_recoveries, 1, "own residue recovered");
    let own = s.regions.iter().find(|r| r.id == id).unwrap();
    assert!(own.self_recovered);
    assert_all_resolve(&again, shared, &covered).await;
    assert_all_resolve(&again, shared, &windowed).await;
    let page = page_of(&uris[0], &avol, id).await.unwrap();
    assert_eq!(page.identity.mount_slot, 0x5002);
    assert_eq!(page.term, 2, "the rejoin bumped the term");
    assert_must_stay_zero(&avol, "rejoined");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&again).await;
    drop(avol);
    drop(again);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **The joiner's death, the manager's recovery, a third daemon** (§5.9
/// end to end with a REAL second daemon — deliverable 4b): the joiner dies
/// with acked records in its window; the death ledger names its identity
/// (the S6 owner's eviction in production — `record_death_with_key` is the
/// same writer); ONE recovery pass of the manager replays the dead ring
/// into the slot tree, unleases the slot at the lease's `g`, marks the
/// page `Recovered`; a THIRD daemon (a new identity) joins, acquires the
/// slot first-touch over the wire at `g + 1` and reads every acked record
/// — and the dead identity's later rejoin gets a FRESH region (§5.8.3: a
/// Recovered ring is never replayed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_joiners_region_is_recovered_by_the_manager_and_a_third_daemon_takes_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let files = create_files(&joiner, shared, "x", 16).await;
    let g_lease = match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { g, .. }) => g,
        other => panic!("{other:?}"),
    };
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();

    let before = recovery_stats().recoveries;
    // Nothing recovers an appender the ledger does not name.
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 0);
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let r = &rep.per_volume[0].1.recovered[0];
    assert_eq!(r.appender_id, id);
    assert!(r.slots.contains(&SLOT_A), "{:?}", r.slots);
    assert!(
        r.entries >= 16,
        "the window held the creates: {}",
        r.entries
    );
    assert_eq!(recovery_stats().recoveries, before + 1);
    assert_eq!(
        page_of(&uris[0], &mvol, id).await.map(|p| p.state),
        Some(AppenderState::Recovered)
    );
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Unleased { g, root, .. }) => {
            assert_eq!(g, g_lease);
            assert!(root.addr != 0);
        }
        other => panic!("tree 0 after recovery: {other:?}"),
    }
    // The manager reads the recovered records itself (the tree is its own
    // to maintain now).
    assert_all_resolve(&manager, shared, &files).await;

    // A THIRD daemon takes the slot at g + 1 and reads every byte.
    let third = join(&uris, &venue, &mvol, 4).await;
    let tvol = Arc::clone(&third.volumes[0]);
    let tid = tvol.appender_stats().unwrap().appender_id;
    assert_ne!(
        tid, id,
        "a Recovered page is never re-adopted by another identity"
    );
    tvol.refresh_control_projection().await.unwrap();
    let more = create_files(&third, shared, "t", 4).await;
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { appender_id, g, .. }) => {
            assert_eq!(appender_id, tid);
            assert_eq!(g, g_lease + 1);
        }
        other => panic!("{other:?}"),
    }
    assert_all_resolve(&third, shared, &files).await;
    assert_all_resolve(&third, shared, &more).await;
    assert_must_stay_zero(&tvol, "third");
    assert_must_stay_zero(&mvol, "manager");

    // The dead identity's rejoin: a FRESH region, never the Recovered one.
    let back = join(&uris, &venue, &mvol, 3).await;
    let bvol = Arc::clone(&back.volumes[0]);
    let bs = bvol.appender_stats().unwrap();
    assert_ne!(
        bs.appender_id, id,
        "§5.8.3: the Recovered ring is never rejoined"
    );
    assert_eq!(bs.self_recoveries, 0);
    shutdown(&back).await;
    drop(bvol);
    drop(back);
    shutdown(&third).await;
    drop(tvol);
    drop(third);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// One daemon's storm into `dir`: `n` creates, every other one renamed
/// (a `Delta` on the moved inode + two dentry records), every fourth one
/// unlinked — the record shapes a replay must fold in seq order. Returns
/// the names that must resolve afterwards and those that must not.
async fn storm(
    routed: &RoutedMetaBackend,
    dir: u64,
    prefix: &str,
    n: usize,
) -> (Vec<(String, u64)>, Vec<String>) {
    let created = create_files(routed, dir, prefix, n).await;
    let mut live = Vec::new();
    let mut gone = Vec::new();
    for (i, (name, ino)) in created.into_iter().enumerate() {
        if i % 4 == 3 {
            routed
                .unlink(dir, &name)
                .await
                .unwrap_or_else(|e| panic!("unlink {name}: {e}"));
            gone.push(name);
        } else if i % 2 == 1 {
            let renamed = format!("{name}.moved");
            routed
                .rename(dir, &name, dir, &renamed, 0)
                .await
                .unwrap_or_else(|e| panic!("rename {name}: {e}"));
            gone.push(name);
            live.push((renamed, ino));
        } else {
            live.push((name, ino));
        }
    }
    (live, gone)
}

fn replay_violations() -> (u64, u64, u64) {
    use squeezefs::meta_backend::kv::{
        META_KV_REPLAY_EXTENT_VIOLATIONS, META_KV_REPLAY_KEY_VIOLATIONS,
        META_KV_REPLAY_LEASE_VIOLATIONS,
    };
    use std::sync::atomic::Ordering::Relaxed;
    (
        META_KV_REPLAY_KEY_VIOLATIONS.load(Relaxed),
        META_KV_REPLAY_LEASE_VIOLATIONS.load(Relaxed),
        META_KV_REPLAY_EXTENT_VIOLATIONS.load(Relaxed),
    )
}

/// **The two-daemon storm and the manager's remount** (deliverable 2's
/// pin, §5.3.4's violation classes on two REAL rings): the manager and two
/// joiners storm THREE directories at once (creates, renames, unlinks —
/// every record shape), all three die without a leave, the manager
/// remounts: its own page 0 is own residue (`self_recoveries` 1), the two
/// joiners' `Live` pages under this node's OTHER mount slots are NOT
/// adopted (PR 2's "a same-node page is own residue" law narrowed to page
/// 0 — a same-host joiner is a live daemon as readily as a dead one; the
/// death ledger decides), their slots stay leased to them (the manager's
/// door refuses `SlotBusy`), and once the ledger names both dead ONE
/// recovery pass replays both rings into the trees: every acked name
/// resolves, every unlinked one is gone, the Key / Lease / Extent
/// violation classes read 0, fsck is clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_daemon_storm_survives_the_managers_remount_with_zero_violations() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a"), (SLOT_B, "b")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mdir = manager
        .create(1, "m", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let j1 = join(&uris, &venue, &mvol, 21).await;
    let j2 = join(&uris, &venue, &mvol, 22).await;
    let v1 = Arc::clone(&j1.volumes[0]);
    let v2 = Arc::clone(&j2.volumes[0]);
    let (id1, id2) = (
        v1.appender_stats().unwrap().appender_id,
        v2.appender_stats().unwrap().appender_id,
    );
    let (ident1, ident2) = (
        v1.joined_wire().unwrap().identity,
        v2.joined_wire().unwrap().identity,
    );
    let violations_before = replay_violations();

    // The storm: three daemons at once, one checkpoint in the middle of
    // each joiner's stream so its window holds records on both sides.
    let ((la, ga), (lb, gb), (lm, gm)) = tokio::join!(
        async {
            let first = storm(&j1, dirs[0], "a", 24).await;
            v1.checkpoint_now().await.unwrap();
            let second = storm(&j1, dirs[0], "A", 24).await;
            ([first.0, second.0].concat(), [first.1, second.1].concat())
        },
        async {
            let first = storm(&j2, dirs[1], "b", 24).await;
            v2.checkpoint_now().await.unwrap();
            let second = storm(&j2, dirs[1], "B", 24).await;
            ([first.0, second.0].concat(), [first.1, second.1].concat())
        },
        storm(&manager, mdir, "m", 48),
    );
    for (v, who) in [(&v1, "j1"), (&v2, "j2"), (&mvol, "manager")] {
        assert_must_stay_zero(v, who);
    }
    // The syncs the acks stood on: every ring's entries are on the device
    // (a joiner's fsync-class durability is its ring write + barrier).
    v1.sync_device().await.unwrap();
    v2.sync_device().await.unwrap();
    mvol.sync_device().await.unwrap();

    // The kill: every daemon dies without a leave.
    venue.tear_down();
    drop((v1, v2));
    drop((j1, j2));
    drop(mvol);
    drop(manager);
    reset_process_state();

    // The manager's remount.
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let ms = mvol.appender_stats().unwrap();
    assert_eq!(
        ms.self_recoveries, 1,
        "page 0 alone is own residue; the joiners' pages are other daemons'"
    );
    assert_eq!(
        ms.live_pages_at_mount, 3,
        "page 0 + both joiners' Live pages listed, the joiners' adopted by nobody"
    );
    for (id, slot) in [(id1, SLOT_A), (id2, SLOT_B)] {
        assert_eq!(
            page_of(&uris[0], &mvol, id).await.map(|p| p.state),
            Some(AppenderState::Live),
            "appender {id}'s page stays Live until the ledger names it"
        );
        assert!(
            matches!(
                tree0_state(&mvol, slot).await,
                Some(SlotState::Leased { appender_id, .. }) if appender_id == id
            ),
            "slot {slot} stays leased to appender {id}: {:?}",
            tree0_state(&mvol, slot).await
        );
    }
    // The manager's door refuses a slot another daemon leases.
    let refused = manager
        .create(dirs[0], "intruder", libc::S_IFREG | 0o644, 1000, 1000)
        .await;
    assert!(
        matches!(&refused, Err(e) if e.to_string().contains("EAGAIN") || e.to_string().contains("lease")),
        "a create into a foreign-leased slot refuses at the door: {refused:?}"
    );
    // The manager's own storm survived its own ring's replay.
    assert_all_resolve(&manager, mdir, &lm).await;
    for name in &gm {
        assert!(
            manager.lookup(mdir, name).await.is_err(),
            "{name} was unlinked"
        );
    }

    // The ledger names both dead; ONE projection recovers both rings.
    assert!(!mvol.record_death_with_key(ident1, 7, 0).await.unwrap());
    assert!(!mvol.record_death_with_key(ident2, 7, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 2, "{rep:?}");
    for (dir, live, gone) in [(dirs[0], &la, &ga), (dirs[1], &lb, &gb)] {
        assert_all_resolve(&manager, dir, live).await;
        for name in gone {
            assert!(
                manager.lookup(dir, name).await.is_err(),
                "{name} was unlinked or renamed away"
            );
        }
    }
    assert_eq!(
        replay_violations(),
        violations_before,
        "Key / Lease / Extent violation classes never moved"
    );
    assert_must_stay_zero(&mvol, "manager after recovery");
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **N = 3 in one process** (N is unbounded by design; three is the pin):
/// two joiners beside the manager, each acquiring a DIFFERENT released
/// slot first-touch, each committing into its own ring; `dlm_rpcs`
/// unmoved by own-slot ops on every daemon; a create storm on all three
/// and a clean leave of both joiners leave the manager reading every
/// acked record and fsck clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_daemons_hold_disjoint_slots_and_commit_into_three_rings() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a"), (SLOT_B, "b")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let dlm_rpcs_before = squeezefs::dlm_slot::dlm_rpcs();

    let j1 = join(&uris, &venue, &mvol, 11).await;
    let j2 = join(&uris, &venue, &mvol, 12).await;
    let v1 = Arc::clone(&j1.volumes[0]);
    let v2 = Arc::clone(&j2.volumes[0]);
    let (id1, id2) = (
        v1.appender_stats().unwrap().appender_id,
        v2.appender_stats().unwrap().appender_id,
    );
    assert!(
        id1 != 0 && id2 != 0 && id1 != id2,
        "three distinct appenders"
    );
    let held1: std::collections::BTreeSet<ForestSlot> = v1
        .slot_leases()
        .unwrap()
        .gate
        .leased_slots()
        .into_iter()
        .collect();
    let held2: std::collections::BTreeSet<ForestSlot> = v2
        .slot_leases()
        .unwrap()
        .gate
        .leased_slots()
        .into_iter()
        .collect();
    let held0: std::collections::BTreeSet<ForestSlot> = mvol
        .slot_leases()
        .unwrap()
        .gate
        .leased_slots()
        .into_iter()
        .collect();
    assert!(held1.is_disjoint(&held2), "{held1:?} ∩ {held2:?}");
    assert!(held1.is_disjoint(&held0) && held2.is_disjoint(&held0));

    let fa = create_files(&j1, dirs[0], "a", 12).await;
    let fb = create_files(&j2, dirs[1], "b", 12).await;
    assert!(matches!(
        tree0_state(&mvol, SLOT_A).await,
        Some(SlotState::Leased { appender_id, .. }) if appender_id == id1
    ));
    assert!(matches!(
        tree0_state(&mvol, SLOT_B).await,
        Some(SlotState::Leased { appender_id, .. }) if appender_id == id2
    ));
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        dlm_rpcs_before,
        "own-slot ops on three daemons take no lock RPC"
    );
    v1.checkpoint_now().await.unwrap();
    v2.checkpoint_now().await.unwrap();
    assert_all_resolve(&j1, dirs[0], &fa).await;
    assert_all_resolve(&j2, dirs[1], &fb).await;
    for (v, who) in [(&v1, "j1"), (&v2, "j2"), (&mvol, "manager")] {
        assert_must_stay_zero(v, who);
    }
    shutdown(&j1).await;
    shutdown(&j2).await;
    drop((v1, v2));
    drop((j1, j2));
    assert_all_resolve(&manager, dirs[0], &fa).await;
    assert_all_resolve(&manager, dirs[1], &fb).await;
    let ms = mvol.appender_stats().unwrap();
    assert_eq!(ms.leaves, 2, "two LeaveAppenders served");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **The refusals**: a joined open needs the plane (`SQUEEZEFS_SYMMETRIC_
/// META=1`) — without it the D0 guard's refusal stands, never a join; a
/// joiner's control writes refuse loud (the must-stay-0 gauge moves
/// nowhere on a healthy joiner); a `LeaveAppender` naming a page still
/// leasing slots is `Busy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_joined_door_refuses_without_the_plane_and_never_writes_a_control_entry() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    // Without the plane: refused naming the knob.
    Knobs::unarmed().apply();
    let r = open_routed_meta_set_joined(
        &uris,
        &JoinedSetAdmission {
            manager_endpoint: venue.endpoint(),
            secret: VENUE_SECRET.to_vec(),
            peer_id: "joiner-x".to_string(),
            identity: joiner_identity(&mvol, 21).await,
        },
    )
    .await;
    Knobs::clear();
    let err = r.err().map(|e| e.to_string()).expect("refused");
    assert!(
        err.contains("SQUEEZEFS_SYMMETRIC_META=1"),
        "names the plane: {err}"
    );

    // A joiner's manager verbs refuse as a control write.
    let joiner = try_join(&uris, &venue, &mvol, 22).await.expect("joins");
    let jvol = Arc::clone(&joiner.volumes[0]);
    let before = jvol.joined_stats().unwrap().control_refusals;
    let e = jvol
        .manager_extent_grant(1, 1)
        .await
        .err()
        .map(|e| e.to_string())
        .expect("a joiner grants nothing");
    assert!(e.contains("JOINED appender"), "{e}");
    assert_eq!(jvol.joined_stats().unwrap().control_refusals, before + 1);
    // LeaveAppender while slots are leased: Busy at the manager.
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let e = mvol
        .manager_leave_appender(identity, id, &[])
        .await
        .err()
        .map(|e| e.to_string())
        .expect("busy");
    assert!(e.contains("release them first"), "{e}");
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}
