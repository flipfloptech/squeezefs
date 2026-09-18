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
use squeezefs::meta_backend::reservation::{self, FakeNvmeNamespace, FakeReservationClient};
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

/// Enroll this process's manager identity in the volume's claim set with
/// `endpoint` as its published listener — what the mount path's
/// membership arm + `publish_symmetric_endpoint` write for a real manager
/// (the in-process venue arms no membership plane). The entry is what
/// `sym_join::resolve_holder_endpoint(vol, 0)` reads on every mount.
async fn enroll_manager(vol: &KvMetaBackend, endpoint: &str) {
    use squeezefs::membership::{MemberIdentity, MemberRole};
    let identity = MemberIdentity {
        id: squeezefs::cowriter::node_member_id().expect("this node's member id"),
        role: MemberRole::Writer,
        pid: std::process::id(),
        boot: squeezefs::meta_backend::kv::backend::read_boot_id(),
        endpoint: Some(endpoint.to_string()),
        pr_key: 0,
    };
    squeezefs::membership::upsert_writer_member(vol, &identity, squeezefs::dlm::durable_term())
        .await
        .expect("the claim-set entry");
    vol.checkpoint_now().await.expect("checkpoint");
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

// ---------------------------------------------------------------------------
// Deliverables 5–6: foreign reads under tokens; serving from the joiner's
// listener (a shipped step initiated by the manager).
// ---------------------------------------------------------------------------

/// A writer's recall sink: counts the data-plane drains a recall runs
/// before its ack travels (the in-process stand-in for `MountRecallSink`).
struct ProbeSink {
    calls: std::sync::atomic::AtomicU64,
}

impl squeezefs::meta_ship::token_plane::RecallDataSink for ProbeSink {
    fn drain_and_purge<'a>(
        &'a self,
        _objects: &'a [squeezefs::meta_ship::token_plane::RecalledObject],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    }
}

/// ONE daemon's listener: what the join ladder's rung 7 stands up on
/// every writer — the S9 custody owner, the S8 meta service (shipped
/// steps served on ITS backend), the token service over ITS holder
/// planes; the manager verbs on the MANAGER's alone.
struct DaemonVenue {
    host: Arc<squeezefs::cluster_wire::RpcListener>,
    endpoint: String,
}

impl DaemonVenue {
    async fn stand_up(routed: &Arc<RoutedMetaBackend>, manager: bool, name: &str) -> Self {
        use squeezefs::cluster_wire as cw;
        use squeezefs::data_grant::{AsyncVerbRouter, WriteCustodyOwner};
        use squeezefs::membership::{LeaseClock, LeaseClocks};
        use squeezefs::meta_ship::manager::ManagerSetService;
        use squeezefs::meta_ship::token_plane::TokenSetService;
        use squeezefs::meta_ship::MetaShipService;
        let clocks = LeaseClocks::with_params(
            std::time::Duration::from_millis(3_000),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(400),
        )
        .expect("2*skew + purge < TTL");
        let owner = WriteCustodyOwner::arm(
            name,
            squeezefs::dlm::durable_term() + 1,
            squeezefs::dlm::durable_term(),
            clocks,
            LeaseClock::monotonic(),
            None,
        )
        .expect("the custody authority arms");
        let mut router = AsyncVerbRouter::new()
            .with_custody(Arc::clone(&owner))
            .with_meta(MetaShipService::new(Arc::clone(routed)))
            .with_tokens(TokenSetService::with_custody_owner(
                &routed.volumes,
                Arc::clone(&owner),
            ));
        if manager {
            router = router.with_manager(ManagerSetService::new(&routed.volumes));
        }
        let host = cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            VENUE_SECRET.to_vec(),
            Arc::new(router),
        )
        .expect("listener");
        let endpoint = host.endpoint().to_string();
        Self { host, endpoint }
    }

    fn tear_down(self) {
        self.host.shutdown();
    }
}

/// **Foreign reads under tokens across two daemons, and a shipped step
/// served from the joiner's listener** (deliverables 5 and 6; PR 9's
/// deviation 4 and PR 12's owed "the writer's read divert"): the joiner
/// reads the ROOT directory — the manager's native slot — and an UNLEASED
/// directory through TOKENS granted by the manager (`dlm_token_grants_
/// served` at the manager, `grants` at the joiner's plane), a manager
/// create into the root RECALLS the joiner's token before it applies and
/// the joiner's next lookup is exact; the joiner's first touch of the
/// unleased directory acquires its slot and the manager's grant recalls
/// the tokens it had granted on that slot (the transfer's token half);
/// then the MANAGER creates a file in the directory the JOINER now holds
/// — PR 6's shipped `XvStep`, dialed through tree 0's lessee and the
/// bound endpoint, served on the joiner's listener under the joiner's
/// lease and ring — and the joiner reads it locally. The custody arm is
/// process-global, so the in-process venue arms the JOINER as the reading
/// writer; the reverse direction is the two-process fleet leg's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_reads_foreign_slots_through_tokens_and_serves_the_managers_shipped_step() {
    use squeezefs::meta_backend::crossvol_tx::{cross_owner_stats, install_xv_shipper};
    use squeezefs::meta_ship::MetaShipRouter;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody").await;
    let venue_endpoint = mvenue.endpoint.clone();
    // The manager's own files in its native slot (the root's), made
    // BEFORE the joiner exists: the joiner's projection at its open holds
    // them, so a token-served read is told apart by what lands AFTER.
    manager
        .create(1, "before", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();

    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: venue_endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: "joiner-31".into(),
                identity: joiner_identity(&mvol, 31).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jvenue = DaemonVenue::stand_up(&joiner, false, "joiner-custody").await;
    // The bindings the ladders make: the manager dials the joiner where it
    // serves; the joiner learnt the manager's at its join.
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    // The step shipper (process-global): the manager's, over its set.
    install_xv_shipper(MetaShipRouter::new(
        Arc::clone(&manager),
        "manager-node",
        VENUE_SECRET.to_vec(),
    ));
    // The JOINER is this process's reading writer: PR 9's custody arm
    // over ITS set (the mount path's `arm_mount_slot_custody`).
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &joiner,
        &squeezefs::cowriter::node_member_id_of(
            jvol.joined_wire().unwrap().identity.node_token,
            jvol.joined_wire().unwrap().identity.mount_slot,
        ),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    let mholder = mvol.token_holder().expect("the manager is a token holder");
    let served0 = mholder.stats().grants_served;

    // A. The joiner reads the manager's ROOT through a token: a file the
    // manager creates AFTER the joiner's open is visible at the joiner's
    // next resolve — never the projection.
    manager
        .create(1, "after", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    let got = joiner
        .lookup(1, "after")
        .await
        .expect("the joiner reads the root through the manager's token");
    assert!(got.ino > 1);
    joiner.lookup(1, "before").await.unwrap();
    let served1 = mholder.stats().grants_served;
    assert!(
        served1 > served0,
        "the root's dentries were GRANTED by the manager ({served0} → {served1})"
    );
    // The recall before the conflicting commit: the manager's next create
    // in the root recalls the joiner's token and waits for its ack; the
    // joiner's next lookup re-fetches and is exact.
    let recalls0 = mholder.stats().recalls;
    manager
        .create(1, "after2", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    assert!(
        mholder.stats().recalls > recalls0,
        "the manager's commit recalled the joiner's token on the root"
    );
    joiner
        .lookup(1, "after2")
        .await
        .expect("exact at the next resolve after the recall");
    assert_eq!(
        mholder.stats().timeouts_live,
        0,
        "the joiner acked every recall inside the deadline"
    );

    // B. An UNLEASED slot's directory is read through the manager (it
    // maintains what nobody leases); the joiner's first touch then takes
    // the slot and the manager's grant recalls the token it had granted
    // on it (the transfer's token half).
    let listed = joiner.readdir(shared, 0, 100).await.unwrap();
    assert!(listed.iter().all(|d| !d.name.starts_with('j')));
    let served_before_touch = mholder.stats().grants_served;
    assert!(
        served_before_touch > served1,
        "the unleased directory came as a token"
    );
    let recalls_before_touch = mholder.stats().recalls;
    let files = create_files(&joiner, shared, "j", 4).await;
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_A).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
        ),
        "the first touch took the slot over the wire"
    );
    assert!(
        mholder.stats().recalls > recalls_before_touch,
        "the grant recalled the manager's tokens on the moved slot"
    );
    assert_all_resolve(&joiner, shared, &files).await;

    // C. The MANAGER creates into the directory the JOINER holds: PR 6's
    // shipped step, served from the joiner's listener.
    let shipped0 = cross_owner_stats().steps_shipped;
    let served_steps0 = cross_owner_stats().steps_served;
    let from_manager = manager
        .create(shared, "from_manager", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a create into a foreign-held directory is an ordinary op");
    assert!(
        cross_owner_stats().steps_shipped > shipped0,
        "the step travelled"
    );
    assert!(
        cross_owner_stats().steps_served > served_steps0,
        "the joiner served it"
    );
    let seen = joiner
        .lookup(shared, "from_manager")
        .await
        .expect("the joiner holds the slot: the dentry is in its tree");
    assert_eq!(seen.ino, from_manager.ino);
    assert_all_resolve(&joiner, shared, &files).await;
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    assert_eq!(
        squeezefs::invariant_tripwire_count("xv_local_step_unguarded"),
        0
    );

    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    jvenue.tear_down();
    // Every file — the joiner's and the manager's shipped one — resolves
    // at the manager after the leave (the released root is exact).
    assert_all_resolve(&manager, shared, &files).await;
    manager.lookup(shared, "from_manager").await.unwrap();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **N ≥ 3: a daemon that joined AFTER another's ladder is bound ON
/// DEMAND at its first foreign act** (the third daemon's endpoint): the
/// slot holder table knows the appenders that were Live when a mount's
/// rung 7 ran (`bind_live_appender_endpoints`) and, on the manager, every
/// `PublishEndpoint` it served — but joiner 3 is unknown to joiner 2,
/// whose ladder ran first. Joiner 2's first read of a file in joiner 3's
/// slot resolves the endpoint off durable state — the page's identity,
/// the claim-set entry joiner 3's publish wrote (read through the
/// manager's tokens, so fresh) — binds it once
/// (`sym_holder_binds_on_demand` +1) and reads through joiner 3's token;
/// `dlm_token_reader_unbound_holders` never moves. Red before the
/// binding: the read refused `EAGAIN` ("no endpoint bound") for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_joined_after_anothers_ladder_is_bound_on_demand_at_its_first_foreign_act() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-n3").await;
    // The manager's claim-set entry (its rung 7's publish) — what a
    // joiner's `PublishEndpoint` lands beside.
    enroll_manager(&mvol, &mvenue.endpoint).await;

    let join_at = |n: u32, ep: String| {
        let uris = uris.clone();
        let mvol = Arc::clone(&mvol);
        async move {
            Knobs::armed().apply();
            let r = open_routed_meta_set_joined(
                &uris,
                &JoinedSetAdmission {
                    manager_endpoint: ep,
                    secret: VENUE_SECRET.to_vec(),
                    peer_id: format!("joiner-{n}"),
                    identity: joiner_identity(&mvol, n).await,
                },
            )
            .await;
            Knobs::clear();
            r.expect("the joined open")
        }
    };
    // Joiner 2 first: its ladder's census binds NOTHING but the manager.
    let j2 = join_at(61, mvenue.endpoint.clone()).await;
    let j2vol = Arc::clone(&j2.volumes[0]);
    let j2venue = DaemonVenue::stand_up(&j2, false, "joiner-2-custody").await;
    j2vol
        .joined_publish_endpoint(&j2venue.endpoint, 0)
        .await
        .expect("joiner 2 publishes its listener through the manager");
    squeezefs::sym_join::bind_live_appender_endpoints(&j2).await;
    // Joiner 2 is this process's reading writer (PR 9's custody arm).
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &j2,
        &squeezefs::cowriter::node_member_id_of(
            j2vol.joined_wire().unwrap().identity.node_token,
            j2vol.joined_wire().unwrap().identity.mount_slot,
        ),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );

    // Joiner 3 joins AFTER joiner 2's ladder, publishes, and takes SLOT_B
    // first-touch with 6 creates.
    let j3 = join_at(62, mvenue.endpoint.clone()).await;
    let j3vol = Arc::clone(&j3.volumes[0]);
    let j3id = j3vol.appender_stats().unwrap().appender_id;
    let j3venue = DaemonVenue::stand_up(&j3, false, "joiner-3-custody").await;
    j3vol
        .joined_publish_endpoint(&j3venue.endpoint, 0)
        .await
        .expect("joiner 3 publishes its listener through the manager");
    let files = create_files(&j3, other, "n3", 6).await;
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_B).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == j3id
        ),
        "joiner 3 holds SLOT_B"
    );
    j2vol.refresh_control_projection().await.unwrap();
    assert!(
        j2vol
            .slot_leases()
            .unwrap()
            .holders
            .endpoint(j3id)
            .is_none(),
        "joiner 2's table does not know joiner 3 — it joined after joiner 2's ladder"
    );
    let binds0 = squeezefs::sym_join::holder_binds_on_demand();
    let unbound0 = squeezefs::meta_ship::token_plane::test_reader_unbound_holders();
    let j3holder = j3vol.token_holder().expect("joiner 3 is a token holder");
    let served0 = j3holder.stats().grants_served;

    // Joiner 2's first foreign act on joiner 3's slot: bound on demand,
    // read through joiner 3's token, exact.
    assert_all_resolve(&j2, other, &files).await;
    assert_eq!(
        squeezefs::sym_join::holder_binds_on_demand(),
        binds0 + 1,
        "ONE on-demand binding — the table knows joiner 3 from here"
    );
    assert_eq!(
        j2vol
            .slot_leases()
            .unwrap()
            .holders
            .endpoint(j3id)
            .as_deref(),
        Some(j3venue.endpoint.as_str()),
        "the bound endpoint is the one joiner 3 published"
    );
    assert!(
        j3holder.stats().grants_served > served0,
        "joiner 3 served the grants"
    );
    assert_eq!(
        squeezefs::meta_ship::token_plane::test_reader_unbound_holders(),
        unbound0,
        "no unbound-holder refusal"
    );
    // A second read pays no resolve.
    assert_all_resolve(&j2, other, &files).await;
    assert_eq!(squeezefs::sym_join::holder_binds_on_demand(), binds0 + 1);
    // And joiner 2's slot from joiner 3's side, the other direction: the
    // shipped step (a create by joiner 3 into joiner 2's directory) finds
    // joiner 2 through the same durable binding.
    let mine = create_files(&j2, shared, "n2", 2).await;
    assert_all_resolve(&j2, shared, &mine).await;

    squeezefs::data_grant::disarm_slot_custody().await;
    assert_must_stay_zero(&j2vol, "joiner 2");
    assert_must_stay_zero(&j3vol, "joiner 3");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&j3).await;
    drop(j3vol);
    drop(j3);
    j3venue.tear_down();
    shutdown(&j2).await;
    drop(j2vol);
    drop(j2);
    j2venue.tear_down();
    assert_all_resolve(&manager, other, &files).await;
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **A live joiner follows a manager FAILOVER to the successor's
/// listener** (PR 10's "busy appender across a manager failover" row, the
/// N-daemon shape it named as PR 12's): the manager leaves and a
/// SUCCESSOR wins the D0 ladder with the joiner still mounted, publishing
/// its listener at a NEW address; the joiner's next wire verb fails at
/// the dead endpoint ONCE, re-dials the manager at the endpoint the
/// successor's claim-set entry names (the projection of the manager's
/// native tree refreshed off the successor's ledger record — no wire, no
/// knob) and the verb lands: a first-touch acquire over the re-dialed
/// wire, `joined_wire_redials` = 1, the holder table's word for appender 0
/// moved, every acked record readable by the successor. Red before the
/// re-dial: every joiner verb failed at the dead endpoint for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_joiner_follows_a_manager_failover_to_the_successors_listener() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join(&uris, &venue, &mvol, 71).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jwire = Arc::clone(jvol.joined_wire().expect("joined"));
    let old_endpoint = venue.endpoint();
    assert_eq!(jwire.endpoint(), old_endpoint);
    let files = create_files(&joiner, shared, "fo", 8).await;
    assert_eq!(jvol.joined_stats().unwrap().wire_redials, 0);

    // The manager leaves; its listener dies; a SUCCESSOR wins the ladder
    // and publishes at a new address.
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let successor = open_under(&uris, &Knobs::armed()).await;
    let svol = Arc::clone(&successor.volumes[0]);
    let venue2 = HoldersVenue::stand_up(&successor, &[]).await;
    assert_ne!(venue2.endpoint(), old_endpoint, "a new listener address");
    squeezefs::multi_writer::publish_symmetric_endpoint(&successor, &venue2.endpoint()).await;
    svol.checkpoint_now()
        .await
        .expect("the successor's checkpoint names the new entry");
    assert_eq!(svol.appender_stats().unwrap().manager_lease.word(), "held");

    // The joiner's next wire act: a first-touch acquire of SLOT_B (the
    // dead endpoint fails once, the re-dial follows the successor).
    let more = create_files(&joiner, other, "fb", 4).await;
    let js = jvol.joined_stats().unwrap();
    assert_eq!(js.wire_redials, 1, "ONE re-dial: {js:?}");
    assert_eq!(
        jwire.endpoint(),
        venue2.endpoint(),
        "the wire follows the successor"
    );
    assert_eq!(
        jvol.slot_leases().unwrap().holders.endpoint(0).as_deref(),
        Some(venue2.endpoint().as_str()),
        "the holder table's word for the manager moved with it"
    );
    assert!(
        matches!(
            tree0_state(&svol, SLOT_B).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == js.appender_id
        ),
        "the acquire landed at the successor"
    );
    assert_all_resolve(&joiner, other, &more).await;
    assert_all_resolve(&joiner, shared, &files).await;
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&svol, "successor");

    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert_all_resolve(&successor, shared, &files).await;
    assert_all_resolve(&successor, other, &more).await;
    venue2.tear_down();
    shutdown(&successor).await;
    drop(svol);
    drop(successor);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// Deliverable 7: the wire holder's flush-then-transfer on a release
// notice; the offer's acceptance.
// ---------------------------------------------------------------------------

/// **The wire holder's flush-then-transfer** (§5.1.4 with a REAL wire
/// holder — PR 4's owed member side): the manager recalls a slot the
/// joiner holds (a requester's accepted offer — `note_recall`); the
/// recall rides the joiner's renewal carriage (`carriage_for` its
/// identity → `slot_release_notices`); the joiner's carriage sink runs
/// the handover: door closed and drained, ring flushed clear, tokens
/// recalled, page `Releasing`, `ReleaseSlot { root, cursor, seq_floor,
/// tails }` over the wire, page without the slot — the manager's tree 0
/// reads `Unleased` at the lease's `g` with the joiner's root, the recall
/// is spent, ring 0's seq space stands above the departing ring's
/// frontier, and the manager's first touch takes the slot at `g + 1` and
/// reads every acked record. The carriage's OTHER word — an offer standing
/// for the joiner — is accepted with `AcquireSlot`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wire_holder_releases_a_slot_on_the_managers_notice_and_accepts_an_offer() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a"), (SLOT_B, "b")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 41).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let ident = jvol.joined_wire().unwrap().identity;
    // The MANAGER is this process's reading writer here: the children the
    // joiner mints live in ITS rotor slots, which it keeps after the
    // release — the manager reads them through the joiner's tokens.
    let jvenue = DaemonVenue::stand_up(&joiner, false, "joiner-custody-41").await;
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &manager,
        &squeezefs::cowriter::node_member_id_of(ident.node_token, 0),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    let files = create_files(&joiner, dirs[0], "n", 16).await;
    let g_lease = match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { appender_id, g, .. }) if appender_id == jid => g,
        other => panic!("{other:?}"),
    };
    let mplane = Arc::clone(mvol.slot_leases().unwrap());
    let routing_a = squeezefs::meta_backend::kv::appender::page_slot_of_forest_slot(
        SLOT_A,
        mvol.appender_stats().unwrap().native_slot,
    )
    .unwrap();
    let routing_b = squeezefs::meta_backend::kv::appender::page_slot_of_forest_slot(
        SLOT_B,
        mvol.appender_stats().unwrap().native_slot,
    )
    .unwrap();

    // The manager's recall (a requester's accepted offer) and what the
    // joiner's next renewal grant carries for its identity.
    mplane.note_recall(jid, SLOT_A);
    let carriage = mplane.carriage_for(ident.node_token, ident.mount_slot);
    assert_eq!(carriage.release_notices, vec![routing_a]);
    assert!(
        carriage
            .leases
            .iter()
            .any(|(r, g)| *r == routing_a && *g == g_lease),
        "the carriage attests the lease: {:?}",
        carriage.leases
    );
    let departing_frontier = jvol.ring_of_region(jid).seq_frontier();
    let releases0 = jvol.joined_stats().unwrap().wire_releases;

    // The carriage's member side: the release notice and an offer of the
    // (unleased) second directory's slot, acted on by the joined set.
    let (released, accepted) = joiner
        .act_on_slot_carriage(&carriage.release_notices, &[(routing_b, 0)])
        .await;
    assert_eq!((released, accepted), (1, 1));
    assert_eq!(jvol.joined_stats().unwrap().wire_releases, releases0 + 1);
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Unleased { g, root, .. }) => {
            assert_eq!(g, g_lease, "a release keeps g");
            assert!(root.addr != 0, "the joiner's root travelled");
        }
        other => panic!("tree 0 after the wire release: {other:?}"),
    }
    assert!(
        mplane.recalls_of(jid).is_empty(),
        "the recall is spent by the ReleaseSlot"
    );
    assert!(
        !jvol.slot_leases().unwrap().gate.is_leased(SLOT_A),
        "the joiner's door no longer passes the slot"
    );
    assert!(
        mvol.journal_ring().seq_frontier() >= departing_frontier,
        "ring 0's seq space stands above the departing ring's frontier"
    );
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_B).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
        ),
        "the offer was accepted with AcquireSlot"
    );
    // The manager's first touch takes the released slot at g + 1 and reads
    // every record the joiner acked (the transferred tree is exact).
    let more = create_files(&manager, dirs[0], "m", 2).await;
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { appender_id, g, .. }) => {
            assert_eq!(appender_id, 0);
            assert_eq!(g, g_lease + 1);
        }
        other => panic!("{other:?}"),
    }
    assert_all_resolve(&manager, dirs[0], &files).await;
    assert_all_resolve(&manager, dirs[0], &more).await;
    let jholder = jvol.token_holder().expect("the joiner is a token holder");
    assert!(
        jholder.stats().grants_served >= 16,
        "the children in the joiner's rotor slots were read through ITS tokens: {}",
        jholder.stats().grants_served
    );
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    squeezefs::data_grant::disarm_slot_custody().await;
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    jvenue.tear_down();
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

// ---------------------------------------------------------------------------
// Rung 4 on a joiner: the device half the file-backed fixture cannot see.
// ---------------------------------------------------------------------------

/// The fidelity tier's fake PR device on the METADATA volume: the manager
/// holds WERO (rtype 3) under its derived key, the joiner's `resolve_for_
/// mount` answers the SAME association (one host, one head — every daemon
/// on a box).
fn install_meta_pr_fake(uri: &str) -> (Arc<FakeNvmeNamespace>, std::path::PathBuf) {
    let ns = FakeNvmeNamespace::new();
    let p = std::path::PathBuf::from(uri);
    reservation::install_override(
        &p,
        FakeReservationClient::new(ns.clone(), "nqn-pr12b-host", "host-pr12b"),
    );
    (ns, p)
}

/// **Rung 4 on a JOINED appender never ACQUIRES** (design-symmetric-
/// metadata §5.8.1 — the manager holds, every other appender is a
/// REGISTRANT; KD-SYM-22 — co-located appenders SHARE one registrant):
/// on a PR-capable metadata namespace a CO-LOCATED joiner ADOPTS the
/// manager's standing hold (the holder key cross-checked against the key
/// the manager's durable `writer_claim` derives — `WriterClaim::pr_key`,
/// the ONE derivation the manager's own guard uses), performs ZERO device
/// mutations at the join and at the leave, and publishes NO key of its
/// own (there is none to preempt — a same-host death is the flock's
/// proof); a REMOTE joiner REGISTERS under the hold with one key for
/// every namespace class and unregisters exactly that at its leave. The
/// data half is the same law (`join_wero_as_appender`), never
/// `arm_data_plane`'s acquire — the first build's second acquire under a
/// spec-strict target unregistered the manager's HOLDER key through the
/// register ladder's own-stale proof and took the fence for itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_registrant_rung_adopts_co_located_and_registers_remote_never_acquires() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let (ns, p) = install_meta_pr_fake(&uris[0]);
    let _restore = ClearOverride(vec![p.clone()]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mkey = mvol.writer_guard_pr_key();
    assert_ne!(mkey, 0, "a PR-capable namespace: the manager registered");
    assert_eq!(ns.holder(), Some(mkey), "the manager HOLDS (rtype 3)");
    assert_eq!(
        mvol.read_writer_claim().await.expect("the claim").pr_key(),
        mkey,
        "the durable claim derives the manager's key — what a joiner cross-checks the holder \
         against"
    );
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    // The co-located joiner (the manager's flock is held on this host):
    // ADOPTS — the device untouched, no key of its own.
    let joiner = join(&uris, &venue, &mvol, 31).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let wire = jvol.joined_wire().expect("joined");
    assert!(wire.colocated, "the same host: the flock's proof");
    assert_eq!(wire.registrant_key, 0, "an adopted hold is not OUR key");
    assert!(
        wire.meta_hold_standing(),
        "rung 4 ran at the door: the adopted hold is held for the mount's life"
    );
    assert_eq!(
        jvol.joined_stats().unwrap().registrant_posture,
        "adopted",
        "the posture word the stats inode publishes"
    );
    assert_eq!(ns.holder(), Some(mkey), "the hold stands");
    assert!(ns.is_registered(mkey));
    assert_eq!(ns.unregister_count(), 0, "adoption unregisters NOTHING");
    assert_eq!(
        squeezefs::data_custody::own_registered_key(),
        None,
        "the key a joiner publishes (membership, PublishEndpoint) is 0 when it adopted"
    );
    let files = create_files(&joiner, dirs[0], "pr", 8).await;
    assert_all_resolve(&joiner, dirs[0], &files).await;
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert_eq!(
        ns.holder(),
        Some(mkey),
        "the leave leaves the device as found"
    );
    assert!(ns.is_registered(mkey));
    assert_eq!(ns.unregister_count(), 0);

    // The REMOTE shape's device act (another host's association on the
    // same namespace): REGISTER under the hold — the holder untouched,
    // one key, unregistered at the leave.
    let remote = FakeReservationClient::new(ns.clone(), "nqn-remote", "host-remote");
    reservation::install_override(&p, remote);
    let key = 0x12b0_0000_0000_0001;
    let joined = squeezefs::data_custody::join_wero_as_appender(
        std::slice::from_ref(&p),
        false,
        &[],
        Some(key),
    )
    .expect("a remote appender registers under the standing WERO");
    assert_eq!(
        joined.evidence().key,
        key,
        "the caller's key, not a fresh mint"
    );
    assert!(ns.is_registered(key), "registered");
    assert_eq!(ns.holder(), Some(mkey), "the manager still holds");
    assert_eq!(
        squeezefs::data_custody::own_registered_key(),
        Some(key),
        "a registered key IS the joiner's to publish"
    );
    drop(joined);
    assert!(!ns.is_registered(key), "the leave unregisters exactly ours");
    assert_eq!(ns.holder(), Some(mkey));
    assert!(ns.is_registered(mkey));
    assert_eq!(
        ns.unregister_count(),
        1,
        "the remote leave's ONE unregister — its own key"
    );
    // The co-located shape through the same door: adopt, never register.
    let adopted = squeezefs::data_custody::join_wero_as_appender(
        std::slice::from_ref(&p),
        true,
        &[mkey],
        None,
    )
    .expect("adopts");
    assert_eq!(adopted.evidence().key, mkey);
    drop(adopted);
    assert_eq!(
        ns.unregister_count(),
        1,
        "adoption's leave unregisters nothing"
    );
    assert_eq!(ns.holder(), Some(mkey));

    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}

/// **A joiner refuses a manager whose metadata hold is not
/// registrants-only** (`SQUEEZEFS_META_PR_WERO=0` — rtype 1, the lab
/// posture): under rtype 1 no registration grants write access, so a
/// join would produce a writer whose every ring write the device rejects.
/// The refusal names the knob; nothing is registered, nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_refuses_a_manager_holding_rtype_1_on_the_metadata_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let (ns, p) = install_meta_pr_fake(&uris[0]);
    let _restore = ClearOverride(vec![p.clone()]);
    std::env::set_var("SQUEEZEFS_META_PR_WERO", "0");
    let manager = open_under(&uris, &Knobs::armed()).await;
    std::env::remove_var("SQUEEZEFS_META_PR_WERO");
    let mvol = Arc::clone(&manager.volumes[0]);
    let mkey = mvol.writer_guard_pr_key();
    assert_eq!(ns.holder(), Some(mkey));
    assert!(!mvol.appender_stats().unwrap().meta_pr_wero, "rtype 1");
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let dir_before = read_directory(mvol.device_path(), mvol.superblock())
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            e.page
                .as_ref()
                .is_some_and(|p| p.state == AppenderState::Live)
        })
        .count();
    let err = try_join(&uris, &venue, &mvol, 41)
        .await
        .err()
        .expect("refused");
    assert!(
        err.contains("SQUEEZEFS_META_PR_WERO") && err.contains("rtype"),
        "names the knob and the rtype: {err}"
    );
    assert_eq!(ns.holder(), Some(mkey), "nothing moved on the device");
    assert_eq!(ns.unregister_count(), 0);
    let dir_after = read_directory(mvol.device_path(), mvol.superblock())
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            e.page
                .as_ref()
                .is_some_and(|p| p.state == AppenderState::Live)
        })
        .count();
    assert_eq!(
        dir_after, dir_before,
        "the refusal precedes JoinAppender: no page went Live"
    );
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}

/// **The recovery driver never preempts a key this process holds** (PR
/// 10's manager-only assumption under KD-SYM-22): a dead co-located
/// appender's published key can be the manager's OWN (an adopted hold's
/// word — or a peer's `RecordDeath` carrying it), and a preempt-and-abort
/// of one's own key is the S9 sweep's own-key law
/// (`multi_writer.rs`: "would take down the authority's own fence") —
/// the recovery takes the full tail scan instead, `pr_fenced` false,
/// `appender_recovery_preempts` unmoved, the hold and the registration
/// exactly as they stood. Red before the fix: the fake's self-preempt
/// dropped the manager's own registration under its standing hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recovery_never_preempts_the_managers_own_key() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let (ns, p) = install_meta_pr_fake(&uris[0]);
    let _restore = ClearOverride(vec![p.clone()]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mkey = mvol.writer_guard_pr_key();
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    let joiner = join(&uris, &venue, &mvol, 51).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let files = create_files(&joiner, dirs[0], "own", 8).await;
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();

    let preempts_before = recovery_stats().preempts;
    let device_preempts = ns.preempt_count();
    // The death recorded with the MANAGER's own key — the adopted shape's
    // word.
    assert!(!mvol.record_death_with_key(identity, 3, mkey).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_eq!(rep.per_volume[0].1.recovered[0].appender_id, id);
    assert_eq!(
        recovery_stats().preempts,
        preempts_before,
        "no preempt was driven"
    );
    assert_eq!(
        ns.preempt_count(),
        device_preempts,
        "none reached the device"
    );
    assert_eq!(ns.holder(), Some(mkey), "the manager's hold stands");
    assert!(
        ns.is_registered(mkey),
        "the manager's registration stands — a self-preempt would have removed it"
    );
    assert_all_resolve(&manager, dirs[0], &files).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// Restore the process-global reservation overrides after a contract.
struct ClearOverride(Vec<std::path::PathBuf>);

impl Drop for ClearOverride {
    fn drop(&mut self) {
        for p in &self.0 {
            reservation::clear_override(p);
        }
    }
}
