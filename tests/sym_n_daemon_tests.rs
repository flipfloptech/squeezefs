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
/// The session peer id a production joiner dials with — its MEMBER id
/// derived from its identity (`cowriter::node_member_id()`); the manager's
/// identity-carrying verbs bind the frame's identity to it (review round
/// 1, Issue 6).
fn peer_of(identity: &AppenderIdentity) -> String {
    squeezefs::cowriter::node_member_id_of(identity.node_token, identity.mount_slot)
}

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
            peer_id: peer_of(&joiner_identity(manager, n).await),
            identity: joiner_identity(manager, n).await,
        },
    )
    .await;
    Knobs::clear();
    r.expect("the joined open")
}

/// [`join`] under the contract's own knobs (a small rotor, a short
/// `T_idle`) — the process-global knobs are what `open_routed_meta_set_
/// joined` reads, so the joiner's derivations follow `knobs`.
async fn join_knobs(
    knobs: &Knobs,
    uris: &[String],
    venue: &HoldersVenue,
    manager: &KvMetaBackend,
    n: u32,
) -> Arc<RoutedMetaBackend> {
    knobs.apply();
    let r = open_routed_meta_set_joined(
        uris,
        &JoinedSetAdmission {
            manager_endpoint: venue.endpoint(),
            secret: VENUE_SECRET.to_vec(),
            peer_id: peer_of(&joiner_identity(manager, n).await),
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
            peer_id: peer_of(&joiner_identity(manager, n).await),
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

/// **A live image's extent is NEVER returned by a dead joiner's recovery —
/// the storm leg's P0 (round 3, F1).** The shape the fleet ran: a joiner
/// fills a directory it leases (its tree's leaves land in extents of ITS
/// grant), RELEASES the slot over the wire (an LRU release), keeps
/// writing, DIES; the manager records the death, recovers its remaining
/// slots and RELEASES the region (step 8 returns "the unclaimed grant"
/// and "the orphan images"); the SAME identity rejoins (a fresh region —
/// the released one is `Free`), first-touches the directory's slot again
/// and STORMS it. Red before the fix at two lines: the served wire
/// `ReleaseSlot` walked the manager's STALE RAM tree of the slot for the
/// images to move out of the lessee's grant record (`grant_record_minus_
/// images` BEFORE the cross-daemon adoption barrier), so the lessee's
/// live images stayed CLAIMED in its record; at its death the orphan
/// census walked only the slots it still leased — the released slot's
/// live images read "claimed, reached by no root" and were RETURNED to
/// the heap (`… 2 orphan image extent(s) returned` on the one volume that
/// fail-stopped), re-granted to the rejoined writer, whose first SMO on
/// the re-acquired tree wrote into its own source extent (`CoW violation
/// … in place`, ×3,276, then the fail-stop). Pinned: every extent the
/// released tree reaches stays CLAIMED across the recovery, the rejoined
/// writer's storm lands, every record resolves, fsck reads clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_joiners_recovery_never_returns_an_extent_a_released_trees_root_still_reaches() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "d"), (SLOT_B, "keep")]).await;
    let (d, keep) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 5).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;

    // The joiner's directory tree, several leaves deep in ITS grant's
    // extents; then the slot RELEASED over the wire (the cadence's LRU
    // shape) while the joiner keeps a second slot and keeps writing.
    let files = create_files(&joiner, d, "f", 700).await;
    jvol.checkpoint_now().await.unwrap();
    jvol.release_slot_handover(jid, SLOT_A)
        .await
        .expect("the joiner releases the directory's slot over the wire");
    assert!(matches!(
        tree0_state(&mvol, SLOT_A).await,
        Some(SlotState::Unleased { .. })
    ));
    let kept = create_files(&joiner, keep, "k", 40).await;
    jvol.checkpoint_now().await.unwrap();
    // The released tree's live images, as the MANAGER now reaches them
    // (its tree adopted at the released root).
    let live_images = mvol.slot_tree_image_extents(SLOT_A).await.unwrap();
    assert!(live_images.len() >= 2, "several leaves: {live_images:?}");
    for e in &live_images {
        assert!(
            mvol.heap_extent_allocated(*e),
            "live image {e} claimed before the death"
        );
    }

    // The joiner dies; the death is recorded; the manager recovers its
    // remaining slots and RELEASES the region (the storm's 20 s window).
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 13, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert!(
        rep.regions_released >= 1,
        "the recovered region is released: {rep:?}"
    );
    for e in &live_images {
        assert!(
            mvol.heap_extent_allocated(*e),
            "live image extent {e} of the released slot's tree was RETURNED by the recovery"
        );
    }
    assert_all_resolve(&manager, d, &files).await;
    assert_all_resolve(&manager, keep, &kept).await;

    // The SAME identity rejoins (a fresh region), re-acquires the
    // directory's slot first-touch and STORMS it: every SMO lands in a
    // fresh extent, never its own source.
    let again = join(&uris, &venue, &mvol, 5).await;
    let avol = Arc::clone(&again.volumes[0]);
    let more = create_files(&again, d, "g", 700).await;
    avol.checkpoint_now().await.unwrap();
    avol.checkpoint_now().await.unwrap();
    assert_all_resolve(&again, d, &more).await;
    assert_all_resolve(&again, d, &files).await;
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .data_dma_fence_refusals
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(
        !avol.is_failed(),
        "the rejoined writer's volume never fail-stopped"
    );
    assert_must_stay_zero(&avol, "rejoined");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&again).await;
    drop(avol);
    drop(again);
    assert_all_resolve(&manager, d, &more).await;
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

/// **A rejoined identity's FRESH region is never recovered under its
/// predecessor's death record** (PR 12b round 3, F7 — PR 10 review round
/// 3's stated obligation for PR 12, Issue 25, met in-shard by the storm
/// leg: the victim's region was recovered and RELEASED, the same identity
/// remounted 16 s later and `JoinAppender` reused the Free page (term 2),
/// and the manager's next ledger poll — the `dead_member:` record still
/// standing, the member not yet re-listed live (its membership join is
/// the ladder's rung 3, AFTER the open) — RECOVERED the live rejoiner's
/// fresh region: `RECOVERING appender 1 (term 2) … 0 window entries
/// replayed`, its page left `Recovered`, the joiner's own region open
/// refusing `the manager's reply and the directory disagree`; the remount
/// FAILED). The law: the join IS the identity's newer incarnation — the
/// manager retires its standing death record at volume 0 BEFORE any page
/// goes Live under it, so a poll that reads the fresh Live page re-reads
/// the record and finds it gone. Here: kill → record → recover → release
/// → the same identity rejoins → the poll's body runs → nothing recovered,
/// the page `Live`, the record retired, the rejoiner writes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoined_identitys_fresh_region_is_never_recovered_under_its_predecessors_death_record()
{
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    let joiner = join(&uris, &venue, &mvol, 5).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let files = create_files(&joiner, shared, "x", 16).await;
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();

    // The death: recorded, recovered, and the region RELEASED (page Free)
    // by the next projection — the storm leg's shape.
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert!(rep.regions_released >= 1, "{rep:?}");
    assert_eq!(
        page_of(&uris[0], &mvol, id).await.map(|p| p.state),
        Some(AppenderState::Free)
    );
    assert!(
        mvol.dead_member_record(&identity).await.unwrap().is_some(),
        "the record stands until a retirement arm runs"
    );
    assert_all_resolve(&manager, shared, &files).await;

    // The same identity REJOINS (a fresh region — the Free page reused).
    let back = join(&uris, &venue, &mvol, 5).await;
    let bvol = Arc::clone(&back.volumes[0]);
    let bs = bvol.appender_stats().unwrap();
    assert_eq!(bs.self_recoveries, 0, "a Recovered ring is never rejoined");
    let bid = bs.appender_id;
    let bpage = page_of(&uris[0], &mvol, bid)
        .await
        .expect("the rejoiner's page");
    assert_eq!(bpage.state, AppenderState::Live);
    assert_eq!(bpage.identity.node_token, identity.node_token);
    assert_eq!(bpage.identity.mount_slot, identity.mount_slot);
    assert!(
        mvol.dead_member_record(&identity).await.unwrap().is_none(),
        "the join retired the identity's death record — it is the newer incarnation"
    );

    // The poll's body right after the join (the storm leg's schedule):
    // nothing is recovered, the fresh page stays Live.
    let recoveries0 = recovery_stats().recoveries;
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(
        rep.recovered(),
        0,
        "the live rejoiner's fresh region was RECOVERED under its predecessor's record: {rep:?}"
    );
    assert_eq!(recovery_stats().recoveries, recoveries0);
    assert_eq!(
        page_of(&uris[0], &mvol, bid).await.map(|p| p.state),
        Some(AppenderState::Live)
    );
    // The rejoiner writes on and every record reads everywhere.
    let more = create_files(&back, shared, "y", 8).await;
    assert_all_resolve(&back, shared, &more).await;
    assert_all_resolve(&back, shared, &files).await;
    assert_must_stay_zero(&bvol, "rejoiner");
    assert_must_stay_zero(&mvol, "manager");

    shutdown(&back).await;
    drop(bvol);
    drop(back);
    assert_all_resolve(&manager, shared, &more).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// The first slot of the page-budget contracts' seeded directories; 128
/// of them, one per forest slot — past `SLOT_PAGE_BUDGET` (108) by 20
/// even before the joiner's rotor.
const OVERFLOW_SEED_BASE: ForestSlot = 12;
const OVERFLOW_SEED_COUNT: u32 = 128;

/// `OVERFLOW_SEED_COUNT` directories seeded one per slot from
/// `OVERFLOW_SEED_BASE`, every slot `Unleased` afterwards.
async fn overflow_seeded_volume(dir: &std::path::Path) -> (Vec<String>, Vec<u64>, Vec<ForestSlot>) {
    let slots: Vec<ForestSlot> =
        (OVERFLOW_SEED_BASE..OVERFLOW_SEED_BASE + OVERFLOW_SEED_COUNT).collect();
    let names: Vec<String> = slots.iter().map(|s| format!("d{s:03}")).collect();
    let pairs: Vec<(ForestSlot, &str)> = slots
        .iter()
        .zip(names.iter())
        .map(|(s, n)| (*s, n.as_str()))
        .collect();
    let (uris, dirs) = seeded_volume(dir, &pairs).await;
    (uris, dirs, slots)
}

/// **A wire lessee past its PAGE BUDGET keeps committing, and every
/// overflow root rides tree 0 through the manager** (PR 12b round 4, F9
/// / Issue 23 — the storm leg's remaining red: the rejoined joiner's
/// `rm -rf` of its dead incarnation's tree first-touched 64 `Unleased`
/// slots inside one `T_idle`, 128 held against a page that names 108; a
/// wire lessee had no tree-0 publication for the 20 overflow roots (PR
/// 4's overflow law, `publish_forest_roots`, is in-process), an
/// unpublished root is a ring-tail FLOOR, the ring filled and D1.b
/// fail-stopped the volume — 7,713 "absent" files that were one FAILED
/// volume's refusals). The wire form: the joiner's checkpoint ships the
/// roots its page cannot hold as `PublishRoots` (the manager's own
/// overflow selection), the manager writes them into tree 0's `Leased`
/// records under ONE durable entry, the floors lift at its reply. Here,
/// with a one-slot rotor: 128 first touches (129 held), two cycles, the
/// ring COVERED (`head == reusable_upto` — red before: the tail pinned
/// behind 21 floors for ever), every overflow slot's tree-0 record naming
/// the joiner's live root, a further storm landing, no manager refusal or
/// rejection; then the joiner DIES after a checkpoint and the recovery
/// finds every file through tree 0's roots (red before: "NO page entry —
/// the grant-time root 0x0 stands", every flushed file of the overflow
/// slots lost), and a manager remount opens every one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wire_lessee_past_its_page_budget_keeps_committing_and_its_overflow_roots_ride_tree_zero()
{
    use squeezefs::meta_backend::kv::appender::SLOT_PAGE_BUDGET;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join_knobs(&knobs, &uris, &venue, &mvol, 91).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let plane = jvol.slot_leases().expect("armed");
    let rotor = plane.rotor.load().len();
    assert!(
        rotor <= 2,
        "a one-slot rotor keeps the count the first touches': {rotor}"
    );

    // 128 first touches: one file in each seeded directory (affinity —
    // the child lands in its parent's slot; the parent's slot is
    // `Unleased`, so the door acquires it over the wire).
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate() {
        files.push((*d, create_files(&joiner, *d, &format!("s{i:03}-"), 1).await));
    }
    let held = plane.gate.leased_count();
    assert!(
        held > SLOT_PAGE_BUDGET,
        "the lessee holds {held} slots — past the page budget {SLOT_PAGE_BUDGET}"
    );
    for slot in &slots {
        assert!(plane.gate.is_leased(*slot), "slot {slot} is the joiner's");
    }
    // The records flushed, then every held tree's ROOT MOVES (a forced
    // compaction — the storm's shape is a fresh mint or a split; any move
    // leaves the root unpublished until a durable home names it): 129
    // roots ahead of their publication, 21 of them past the page.
    jvol.checkpoint_now().await.unwrap();
    for slot in &slots {
        let root = jvol.slot_tree(*slot).expect("the joiner's tree").root();
        let mut attempts = 0;
        loop {
            match jvol.defrag_compact_nodes(&[(0, root.addr)]).await {
                Ok(n) => {
                    assert_eq!(n, 1, "slot {slot}'s root compacts");
                    break;
                }
                // The joiner's grant refills at its cadence (the reactive
                // refill is the flush pass's, not the D4 arm's).
                Err(squeezefs::meta_backend::kv::KvError::GrantExhausted { .. })
                    if attempts < 8 =>
                {
                    attempts += 1;
                    jvol.checkpoint_now().await.unwrap();
                }
                Err(e) => panic!("slot {slot}: {e}"),
            }
        }
    }

    // Two cycles: the page names its budget, the overflow ships, the
    // floors lift at the reply, the ring drains.
    jvol.checkpoint_now().await.unwrap();
    jvol.checkpoint_now().await.unwrap();
    let (head, upto) = jvol.region_ring_window(jid).expect("the joined region");
    assert_eq!(
        head, upto,
        "the joiner's ring is COVERED after two cycles — a pinned tail is the overflow floors \
         (head {head}, reusable_upto {upto})"
    );
    // At least `seeded − budget` of the SEEDED slots are past the page
    // (the one-slot rotor may sit on either side of the cut).
    let overflow_min = slots.len() - SLOT_PAGE_BUDGET;
    let shipped = plane
        .roots_shipped
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        shipped as usize >= overflow_min,
        "every overflow root shipped ({shipped} ≥ {overflow_min})"
    );
    let mplane = mvol.slot_leases().expect("armed");
    assert!(
        mplane
            .roots_published_for_lessees
            .load(std::sync::atomic::Ordering::Relaxed)
            >= overflow_min as u64
    );
    // Every seeded slot's tree-0 record names the joiner's LIVE root — the
    // page-held ones through the page's own publication law (their record
    // keeps the grant-time root; the page is their home), the overflow
    // ones through the wire.
    let mut published_by_wire = 0usize;
    for slot in &slots {
        let live = jvol.slot_tree(*slot).expect("the joiner's tree").root();
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Leased {
                appender_id, root, ..
            }) => {
                assert_eq!(appender_id, jid);
                if root == live {
                    published_by_wire += 1;
                }
            }
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    assert!(
        published_by_wire >= overflow_min,
        "tree 0 names the live root of at least every overflow slot ({published_by_wire} ≥ \
         {overflow_min})"
    );
    let ms = mvol.appender_stats().unwrap();
    assert_eq!(ms.manager_verb_refusals, 0, "{ms:?}");
    assert_eq!(ms.manager_verb_rejected, 0, "{ms:?}");

    // The storm goes on: a further burst lands and the ring drains again.
    let more = create_files(&joiner, dirs[0], "more", 300).await;
    jvol.checkpoint_now().await.unwrap();
    jvol.checkpoint_now().await.unwrap();
    let (head, upto) = jvol.region_ring_window(jid).expect("the joined region");
    assert_eq!(head, upto, "covered after the burst");
    assert!(!jvol.is_failed(), "the volume never fail-stopped");
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    // The lessee DIES after its checkpoint: the recovery's root for an
    // overflow slot is tree 0's published one (red before: "NO page entry
    // — the grant-time root 0x0 stands", the flushed files gone).
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 31, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    for (d, fs) in &files {
        assert_all_resolve(&manager, *d, fs).await;
    }
    assert_all_resolve(&manager, dirs[0], &more).await;
    assert_must_stay_zero(&mvol, "manager after the recovery");

    // A manager remount opens every one of them.
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let again = open_under(&uris, &knobs).await;
    for (d, fs) in &files {
        assert_all_resolve(&again, *d, fs).await;
    }
    assert_all_resolve(&again, dirs[0], &more).await;
    shutdown(&again).await;
    drop(again);
    fsck_clean(&uris).await;
}

/// **The manager's raw C1 walk skips the slot trees a LIVE joiner leases**
/// (PR 12b round 4 — the `sym-storm` leg's round-3 red on the fixed tree:
/// after three rounds the manager's online fsck reported `C1Torn` on
/// `vol1/slot312` — a slot a live joiner leases and appends into. The
/// manager holds that tree only as a PROJECTION: the joiner moves its
/// root and rewrites its images under grants the manager handed out, so
/// the manager's raw walk from ITS root word read a routing loop —
/// `root-seq` restarts to the traversal budget — over a healthy tree.
/// Round 1 scoped the DENTRY pass out of a live lessee's trees by the S6
/// owner's word; the raw C1 units take the same law here. A lessee not
/// known live stays PR 10's frozen-tree class and is walked.
///
/// The shape: the F9 pin's joiner (128 first touches past the page
/// budget, every root moved by a forced compaction, two cycles) LIVE and
/// listed live by the installed owner; the FULL fsck engine at the
/// manager → no finding, the skipped trees counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_managers_c1_walk_skips_the_slot_trees_a_live_joiner_leases() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;
    // The manager is the S6 owner of the joiner's shard; the joiner is a
    // LIVE member of it (the mount path's rung 3).
    let owner = MembershipOwner::arm(
        "c1-owner",
        3,
        2,
        LeaseClocks::derive(std::time::Duration::from_micros(250)).expect("derived clocks"),
        LeaseClock::monotonic(),
    )
    .expect("arm the owner");
    membership::install_owner(Arc::clone(&owner));

    let joiner = join_knobs(&knobs, &uris, &venue, &mvol, 92).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let identity = jvol.joined_wire().unwrap().identity;
    let member = squeezefs::cowriter::node_member_id_of(identity.node_token, identity.mount_slot);
    let JoinOutcome::Granted(_) = owner.join(JoinRequest {
        id: member.clone(),
        role: MemberRole::Writer,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-c1".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("the joiner joins the shard");
    };
    assert!(owner.member_is_live(&member));

    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate() {
        files.push((*d, create_files(&joiner, *d, &format!("c{i:03}-"), 1).await));
    }
    jvol.checkpoint_now().await.unwrap();
    for slot in &slots {
        let root = jvol.slot_tree(*slot).expect("the joiner's tree").root();
        let mut attempts = 0;
        loop {
            match jvol.defrag_compact_nodes(&[(0, root.addr)]).await {
                Ok(n) => {
                    assert_eq!(n, 1, "slot {slot}'s root compacts");
                    break;
                }
                Err(squeezefs::meta_backend::kv::KvError::GrantExhausted { .. })
                    if attempts < 8 =>
                {
                    attempts += 1;
                    jvol.checkpoint_now().await.unwrap();
                }
                Err(e) => panic!("slot {slot}: {e}"),
            }
        }
    }
    jvol.checkpoint_now().await.unwrap();
    jvol.checkpoint_now().await.unwrap();
    let more = create_files(&joiner, dirs[0], "more", 200).await;
    jvol.checkpoint_now().await.unwrap();

    // The FULL engine at the manager with the joiner live.
    let report = fsck_all_classes_over(&manager).await;
    assert!(
        !report.has_findings(),
        "the manager's fsck takes no verdict over a live lessee's tree: {:?}",
        report.findings
    );
    assert!(
        report.counters.c1_foreign_live_slots_scoped >= slots.len() as u64,
        "every slot tree the live joiner leases was skipped by the raw C1 walk ({} ≥ {})",
        report.counters.c1_foreign_live_slots_scoped,
        slots.len()
    );
    // The joiner is unharmed by the census: every file resolves there.
    for (d, fs) in &files {
        assert_all_resolve(&joiner, *d, fs).await;
    }
    assert_all_resolve(&joiner, dirs[0], &more).await;

    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// The FULL fsck engine (every class, the offline options' settle) over
/// an already-open WRITER set — the manager's online census the fleet
/// legs run after each round.
async fn fsck_all_classes_over(routed: &Arc<RoutedMetaBackend>) -> squeezefs::fsck::FsckReport {
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-c1")
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
    squeezefs::fsck::run(&ctx, &opts)
        .await
        .expect("the engine runs")
}

/// **A wire lessee sheds its overflow to the page budget within one
/// beat** (PR 12b round 4, Issue 23's bound — PR 4's LRU release on a
/// joined appender): past `SLOT_PAGE_BUDGET` the cadence releases
/// non-rotor slots no requester is shipping to (`holder_ops == 0` — a
/// dominated slot is a handover candidate, never an LRU one), least-
/// recently-written first, through the region's own wire `ReleaseSlot`
/// (flush-then-transfer: the door drained, the covering cycles, tree 0
/// `Unleased` at the manager with the root the lessee moved), back to
/// the budget in ONE tick once `T_idle` has passed (a slot written inside
/// the window is a live holder's — the count alone never moves it). The
/// arm was wired on a joined appender already (green-first — the bound's
/// contract); what starved it on the storm leg was F9's floors: a release
/// waits for the region's tail to pass the slot's frontier, and a tail
/// pinned behind the sibling overflow floors never does (`slot_lru_
/// releases` stayed 0 through the 15-s wedge — the first contract's
/// ring-drain assertion is that pin). Here, `T_idle` = 300 ms: 128 first
/// touches, one checkpoint, one cadence tick past the window → held ≤ the
/// budget, every
/// released slot `Unleased` at the manager naming a live root, every
/// file readable at both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wire_lessee_sheds_its_idle_overflow_to_the_page_budget_within_one_beat() {
    use squeezefs::meta_backend::kv::appender::SLOT_PAGE_BUDGET;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1").t_idle_ms("300");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join_knobs(&knobs, &uris, &venue, &mvol, 92).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let plane = jvol.slot_leases().expect("armed");
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate() {
        files.push((*d, create_files(&joiner, *d, &format!("s{i:03}-"), 1).await));
    }
    let held0 = plane.gate.leased_count();
    assert!(held0 > SLOT_PAGE_BUDGET);
    jvol.checkpoint_now().await.unwrap();
    // Past `T_idle` (a slot written inside the window is a live holder's
    // — the count alone never moves it): ONE beat brings the held set to
    // the budget.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    jvol.slot_lease_cadence().await.unwrap();
    let held = plane.gate.leased_count();
    assert!(
        held <= SLOT_PAGE_BUDGET,
        "one beat past T_idle sheds the overflow: held {held} ≤ {SLOT_PAGE_BUDGET} (was {held0})"
    );
    assert!(
        plane
            .lru_releases
            .load(std::sync::atomic::Ordering::Relaxed) as usize
            >= held0 - SLOT_PAGE_BUDGET
    );
    let mut released = 0usize;
    for slot in &slots {
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Unleased { root, .. }) => {
                released += 1;
                assert_ne!(
                    root.addr, 0,
                    "the released tree's root travelled: slot {slot}"
                );
            }
            Some(SlotState::Leased { appender_id, .. }) => assert_eq!(appender_id, jid),
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    assert!(
        released >= held0 - SLOT_PAGE_BUDGET,
        "{released} released at the manager"
    );
    // Every file reads at the joiner (the released directories' dentries
    // through the manager's plane, the inode records — its rotor slot's —
    // locally); the manager reads them all once the leave handed the rest
    // over (this fixture's manager has no custody arm to dial a lessee).
    for (d, fs) in &files {
        assert_all_resolve(&joiner, *d, fs).await;
    }
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    for (d, fs) in &files {
        assert_all_resolve(&manager, *d, fs).await;
    }
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
                peer_id: peer_of(&joiner_identity(&mvol, 31).await),
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

/// **A dead JOINER's half-applied cross-owner unlink is rolled forward at
/// the manager** (review round 1, Issue 4(i) — the PR 6/10 dead-INITIATOR
/// row, on the two-backend venue; the storm's `rm -rf` on a joiner is
/// exactly N such intents): the joiner unlinks a file whose dentry lives
/// in the MANAGER's directory (PR 6's `RemoveDentry` ships) and whose
/// record lives in the joiner's rotor — the seam, scoped to the joiner's
/// appender id (`TEST_XV_SEAM_INITIATOR`, so the manager's own ops run on),
/// severs its plan after the shipped step: the name is gone at the
/// manager, the child's `nlink` still 1, the intent open in the joiner's
/// ring. The joiner dies; the manager's death-ledger recovery takes its
/// slots and — the recovery's own roll-forward arm — adopts the intent
/// (its home slot is the manager's now, KD-SYM-2/3) and completes the
/// plan: the child reads `nlink 0`, the intent is retired
/// (`recovery_intents_rolled_forward` +1, `xv_cross_owner_intents_open`
/// back to 0), fsck reads no C9/C10 finding. Red before the scoped seam:
/// the process-global seam halted the manager's recovery-side ops too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_joiners_half_applied_cross_owner_unlink_is_rolled_forward_at_the_manager() {
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, install_xv_shipper, uninstall_xv_shipper, TEST_XV_SEAM_AFTER_STEPS,
        TEST_XV_SEAM_INITIATOR,
    };
    use squeezefs::meta_ship::MetaShipRouter;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-dead-init").await;
    // The MANAGER holds the directory's slot (its first touch).
    manager
        .create(shared, "m0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    assert!(matches!(
        tree0_state(&mvol, SLOT_A).await,
        Some(SlotState::Leased { appender_id: 0, .. })
    ));

    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 51).await),
                identity: joiner_identity(&mvol, 51).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    // The JOINER is the initiator: its shipper over its set.
    install_xv_shipper(MetaShipRouter::new(
        Arc::clone(&joiner),
        "joiner-node",
        VENUE_SECRET.to_vec(),
    ));
    // The joiner's file in the manager's directory: the dentry at the
    // manager (shipped), the record in the joiner's rotor.
    let shipped0 = cross_owner_stats().steps_shipped;
    let victim = joiner
        .create(shared, "victim", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a create into the manager's directory ships its dentry");
    assert!(cross_owner_stats().steps_shipped > shipped0);
    let victim_slot =
        squeezefs::meta_backend::kv::record::forest_slot_of_ino(manager.route_ino(victim.ino).1);
    assert!(
        matches!(
            tree0_state(&mvol, victim_slot).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
        ),
        "the child was minted in the joiner's rotor"
    );
    // The manager's directory names it (the record itself lives in the
    // joiner's slot — a token read on a custody-armed manager; here the
    // directory's own tree is what the manager owns).
    let names = |list: Vec<squeezefs::meta_backend::DirEntry>| -> Vec<String> {
        list.into_iter().map(|d| d.name).collect()
    };
    assert!(names(manager.readdir(shared, 0, 100).await.unwrap()).contains(&"victim".to_string()));

    // The joiner reads the manager's directory through the manager's
    // TOKENS (PR 9's custody arm over the joiner's set — the mount path's
    // `arm_mount_slot_custody`); the dentry it shipped is exact there.
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &joiner,
        &squeezefs::cowriter::node_member_id_of(identity.node_token, identity.mount_slot),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    assert!(names(joiner.readdir(shared, 0, 100).await.unwrap()).contains(&"victim".to_string()));

    // The severed unlink: ONE step commits (the shipped `RemoveDentry` at
    // the manager), the plan dies before the child's `SetNlink`.
    let open0 = cross_owner_stats().intents_open;
    TEST_XV_SEAM_INITIATOR.store(u64::from(jid) + 1, Relaxed);
    TEST_XV_SEAM_AFTER_STEPS.store(2, Relaxed);
    let e = joiner
        .unlink(shared, "victim")
        .await
        .expect_err("the severed plan errors like a dead process");
    TEST_XV_SEAM_AFTER_STEPS.store(0, Relaxed);
    TEST_XV_SEAM_INITIATOR.store(0, Relaxed);
    assert!(e.to_string().contains("seam"), "{e}");
    assert!(
        !names(manager.readdir(shared, 0, 100).await.unwrap()).contains(&"victim".to_string()),
        "the shipped RemoveDentry landed at the manager"
    );
    assert_eq!(
        joiner.getattr(victim.ino).await.unwrap().nlink,
        1,
        "the child's SetNlink never ran — the half-applied shape"
    );
    assert_eq!(cross_owner_stats().intents_open, open0 + 1);
    // The manager's own ops ran on under the joiner-scoped seam.
    manager
        .create(shared, "m1", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the manager's plan is never severed by the joiner's seam");

    // The joiner dies (no leave, no shutdown); the manager records it and
    // recovers: its slots are the manager's, and the recovery's roll-
    // forward adopts and completes the dead initiator's intent.
    uninstall_xv_shipper();
    squeezefs::data_grant::disarm_slot_custody().await;
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let rolled0 = recovery_stats().intents_rolled_forward;
    assert!(!mvol.record_death_with_key(identity, 11, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert!(
        recovery_stats().intents_rolled_forward > rolled0,
        "the dead initiator's intent was rolled forward"
    );
    assert_eq!(cross_owner_stats().intents_open, open0, "retired");
    // The child's slot is the manager's now: released by the recovery,
    // then first-touched by the roll-forward's own `SetNlink`.
    assert!(
        matches!(
            tree0_state(&mvol, victim_slot).await,
            Some(SlotState::Unleased { .. }) | Some(SlotState::Leased { appender_id: 0, .. })
        ),
        "the child's slot is the manager's to maintain now: {:?}",
        tree0_state(&mvol, victim_slot).await
    );
    // (An `Err` here is the corpse sweep having taken the record already.)
    if let Ok(attr) = manager.getattr(victim.ino).await {
        assert_eq!(attr.nlink, 0, "the roll-forward ran the child's SetNlink");
    }
    assert!(manager.lookup(shared, "victim").await.is_err());
    manager.lookup(shared, "m0").await.unwrap();
    manager.lookup(shared, "m1").await.unwrap();
    assert_must_stay_zero(&mvol, "manager");
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
                    peer_id: peer_of(&joiner_identity(&mvol, n).await),
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

/// **A redirect to a DEAD holder is never handed out; a peer's lookup of
/// a removed name answers within the bound** (round 3, F2 — the storm
/// leg's 24-minute park): joiner 3 holds a slot with files; joiner 2 reads
/// them through joiner 3's token (the manager's `NotHolder { 3 }`
/// redirect). Joiner 3 DIES and the S6 owner (the manager) EVICTS it —
/// tree 0 still leases the slot to the dead id until the ledger poll
/// recovers it. Joiner 2's next read: the manager answers `HolderDead`
/// (never `NotHolder` to an address nobody answers at —
/// `slot_resolve_dead_redirects`), the reader refuses EAGAIN INSIDE the
/// bound with no dial; after the recovery the slot is the manager's and
/// the read is exact there; the manager removes a name and the peer's
/// lookup of it is ENOENT, never a park. Red before: `NotHolder { 3 }`
/// handed out for a dead lessee (`slot_resolve_dead_redirects == 0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_lookup_through_a_dead_holder_is_refused_inside_the_bound_and_exact_after_recovery()
{
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let other = dirs[1];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-f2").await;
    enroll_manager(&mvol, &mvenue.endpoint).await;
    // The manager is the S6 owner of the joiners' shard (the mount path's
    // rung 3): liveness is its word.
    let owner = MembershipOwner::arm(
        "f2-owner",
        3,
        2,
        LeaseClocks::derive(std::time::Duration::from_micros(250)).expect("derived clocks"),
        LeaseClock::monotonic(),
    )
    .expect("arm the owner");
    membership::install_owner(Arc::clone(&owner));
    let enroll = |ident: AppenderIdentity| {
        let owner = Arc::clone(&owner);
        move || {
            let member = squeezefs::cowriter::node_member_id_of(ident.node_token, ident.mount_slot);
            let JoinOutcome::Granted(_) = owner.join(JoinRequest {
                id: member.clone(),
                role: MemberRole::Writer,
                endpoint: None,
                pid: std::process::id(),
                boot: "boot-f2".to_string(),
                prior_epoch: None,
                pr_key: 0,
                mount: None,
            }) else {
                panic!("the joiner joins the shard");
            };
            member
        }
    };
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
                    peer_id: peer_of(&joiner_identity(&mvol, n).await),
                    identity: joiner_identity(&mvol, n).await,
                },
            )
            .await;
            Knobs::clear();
            r.expect("the joined open")
        }
    };
    let j2 = join_at(71, mvenue.endpoint.clone()).await;
    let j2vol = Arc::clone(&j2.volumes[0]);
    let j2venue = DaemonVenue::stand_up(&j2, false, "joiner-2-f2").await;
    j2vol
        .joined_publish_endpoint(&j2venue.endpoint, 0)
        .await
        .expect("joiner 2 publishes");
    let _m2 = enroll(j2vol.joined_wire().unwrap().identity)();
    squeezefs::sym_join::bind_live_appender_endpoints(&j2).await;
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

    let j3 = join_at(72, mvenue.endpoint.clone()).await;
    let j3vol = Arc::clone(&j3.volumes[0]);
    let j3id = j3vol.appender_stats().unwrap().appender_id;
    let j3ident = j3vol.joined_wire().unwrap().identity;
    let j3venue = DaemonVenue::stand_up(&j3, false, "joiner-3-f2").await;
    j3vol
        .joined_publish_endpoint(&j3venue.endpoint, 0)
        .await
        .expect("joiner 3 publishes");
    let m3 = enroll(j3ident)();
    let files = create_files(&j3, other, "f2", 6).await;
    j3vol.checkpoint_now().await.unwrap();
    assert!(matches!(
        tree0_state(&mvol, SLOT_B).await,
        Some(SlotState::Leased { appender_id, .. }) if appender_id == j3id
    ));
    // Joiner 2 reads joiner 3's files through its token (the redirect).
    assert_all_resolve(&j2, other, &files).await;
    let mholder = mvol.token_holder().expect("the manager is a token holder");
    let redirects0 = mholder.stats().not_holder_redirects;
    let dead0 = mholder.stats().dead_redirects;
    assert!(
        redirects0 >= 1,
        "the redirect to joiner 3 was handed out while it lived"
    );

    // Joiner 3 DIES (its listener with it); the owner EVICTS it — tree 0
    // still leases the slot to the dead id (no recovery yet).
    j3venue.tear_down();
    drop(j3vol);
    drop(j3);
    park_gate::test_reset();
    assert!(owner.evict(&m3, "the F2 pin's kill").is_some());
    assert!(!owner.member_is_live(&m3));
    assert!(matches!(
        tree0_state(&mvol, SLOT_B).await,
        Some(SlotState::Leased { appender_id, .. }) if appender_id == j3id
    ));
    // Joiner 2's read now: refused INSIDE the bound, no redirect to the
    // dead address, no dial.
    let (name, _) = &files[0];
    let t0 = std::time::Instant::now();
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), j2.lookup(other, name))
        .await
        .expect("the lookup returns inside the bound — never a park");
    let e = out.expect_err("a dead holder's slot is refused, never served stale");
    assert!(
        e.to_string().contains("DEAD"),
        "the manager's word, not a dial's failure: {e} (after {:?})",
        t0.elapsed()
    );
    let st = mholder.stats();
    assert!(
        st.dead_redirects > dead0,
        "the manager withheld the redirect to the dead holder (slot_resolve_dead_redirects): {st:?}"
    );
    assert_eq!(
        st.not_holder_redirects, redirects0,
        "no NotHolder to a dead lessee was handed out"
    );

    // The recovery makes the slot the manager's: the read is exact there
    // (joiner 3's files), and a name the manager removes is ENOENT at the
    // peer — never a park.
    assert!(!mvol.record_death_with_key(j3ident, 21, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    j2vol.refresh_control_projection().await.unwrap();
    assert_all_resolve(&j2, other, &files).await;
    manager
        .unlink(other, name)
        .await
        .expect("the manager removes a recovered name");
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), j2.lookup(other, name))
        .await
        .expect("bounded");
    let e = out.expect_err("a removed name is ENOENT at the peer");
    assert!(
        e.to_string().contains("not found") || e.to_string().contains("ENOENT"),
        "{e}"
    );
    assert_all_resolve(&j2, other, &files[1..]).await;

    squeezefs::data_grant::disarm_slot_custody().await;
    membership::uninstall();
    assert_must_stay_zero(&j2vol, "joiner 2");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&j2).await;
    drop(j2vol);
    drop(j2);
    j2venue.tear_down();
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
///
/// **And its DATA plane follows too** (review round 1, Issue 2): the
/// joiner's allocator mints from the manager's ranged block grants over
/// the wire (`arm_joined_allocation`'s sink) and ships its terminal frees
/// to the holder's venue — after the failover its next block ask fails at
/// the dead venue once, RE-RESOLVES the allocation holder off durable
/// state (`alloc_lease:` → the successor's page-0 identity → its
/// published listener — `sym_join::joined_holder_venue`) and lands at the
/// SUCCESSOR's holding (`block_grants` advanced there, the minted blocks
/// SET in its bitmap), and the data volume's FREE TARGET moves with it.
/// Red before: the sink and the free target kept the join-time endpoint
/// for the mount's life — every post-failover striped write was refused a
/// block and every free shipped to a dead address (the leg's
/// post-failover write was 7 inline bytes, never a block).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_joiner_follows_a_manager_failover_to_the_successors_listener() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::meta_backend::kv::alloc_lease;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;
    // The manager HOLDS the data volume's allocation lease (PR 8's arm).
    let data_id = "vol-failover-data";
    let data_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(data_id);
    let data_blocks = 4096u64;
    let a = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    a.set_capacity_bytes(data_blocks * a.chunk_size());
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&manager, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );

    let joiner = join(&uris, &venue, &mvol, 71).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jwire = Arc::clone(jvol.joined_wire().expect("joined"));
    let old_endpoint = venue.endpoint();
    assert_eq!(jwire.endpoint(), old_endpoint);
    let files = create_files(&joiner, shared, "fo", 8).await;
    assert_eq!(jvol.joined_stats().unwrap().wire_redials, 0);
    // The joiner's DATA plane: the production venue (re-resolved off
    // durable state) behind the wire grant sink + the free target.
    let hv = squeezefs::sym_join::joined_holder_venue(&joiner, data_tag, old_endpoint.clone());
    let b = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    b.set_capacity_bytes(data_blocks * b.chunk_size());
    assert!(b.install_block_grant_arm(
        data_tag,
        alloc_lease::wire_block_grant_sink(
            Arc::clone(&hv),
            VENUE_SECRET.to_vec(),
            jwire.identity.into(),
            0,
            data_tag,
        ),
    ));
    assert!(alloc_lease::install_wire_free_target(
        data_tag,
        &old_endpoint
    ));
    let mut minted_before = std::collections::BTreeSet::new();
    for _ in 0..8 {
        minted_before.insert(b.allocate_block().await.unwrap() / b.chunk_size());
    }
    assert_eq!(
        minted_before.len(),
        8,
        "the joiner mints from the manager's grants"
    );
    let grants_at_old = alloc_lease::holding(data_tag).unwrap().stats().block_grants;
    assert!(grants_at_old >= 1);

    // The manager leaves; its listener dies; a SUCCESSOR wins the ladder
    // and publishes at a new address.
    venue.tear_down();
    shutdown(&manager).await;
    alloc_lease::disarm_symmetric_roles();
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
    // The successor re-holds the data volume's lease (the same identity's
    // record; PR 8's own-residue re-hold).
    let a2 = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    a2.set_capacity_bytes(data_blocks * a2.chunk_size());
    let released0 = squeezefs::data_alloc_bitmap::DATA_ALLOC_BITMAP_LEAKS_RELEASED.load(Relaxed);
    let deferred0 = squeezefs::data_alloc_bitmap::DATA_ALLOC_BITMAP_LEAKS_DEFERRED.load(Relaxed);
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&successor, &[Arc::clone(&a2)])
            .await
            .unwrap(),
        1
    );
    let sholding = alloc_lease::holding(data_tag).expect("the successor holds the lease");
    // The LIVE joiner's window remainder reads exactly like a dead
    // incarnation's at the re-hold (RAM at the joiner, granted by a ledger
    // that died with the manager): with the joiner's page `Live` the
    // release is DEFERRED, never cleared under its feet — before it, the
    // re-hold cleared the remainder and re-carved the same blocks (200
    // mints → 144 distinct: a double allocation).
    assert_eq!(
        squeezefs::data_alloc_bitmap::DATA_ALLOC_BITMAP_LEAKS_RELEASED.load(Relaxed),
        released0,
        "a re-hold beside a LIVE peer releases nothing"
    );
    assert!(
        squeezefs::data_alloc_bitmap::DATA_ALLOC_BITMAP_LEAKS_DEFERRED.load(Relaxed) > deferred0,
        "the joiner's window remainder is a DEFERRED candidate"
    );
    for blk in &minted_before {
        assert!(
            sholding.bitmap.is_set(*blk),
            "the joiner's pre-failover block {blk} stays SET at the successor"
        );
    }
    let grants_at_successor0 = sholding.stats().block_grants;
    // In one process the manager's leave uninstalled the free target it
    // shared with the joiner (the process-global table): the re-resolve
    // must RE-HOME it — a second daemon's stale entry is the same word.
    assert!(squeezefs::block_grant::free_target_for(data_tag).is_none());

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

    // The joiner's DATA plane after the failover: enough mints to exhaust
    // the window the dead manager granted — the ask fails at the dead
    // venue once, re-resolves the holder to the successor and lands there;
    // every minted block is SET in the successor's bitmap; the free target
    // moved with the venue.
    let mut minted_after = std::collections::BTreeSet::new();
    for _ in 0..200 {
        minted_after.insert(b.allocate_block().await.unwrap() / b.chunk_size());
    }
    assert_eq!(minted_after.len(), 200);
    assert!(
        minted_before.is_disjoint(&minted_after),
        "a block minted twice across the failover"
    );
    assert_eq!(
        hv.moves.load(Relaxed),
        1,
        "the holder venue MOVED once (to the successor)"
    );
    assert_eq!(hv.current(), venue2.endpoint());
    let sstats = sholding.stats();
    assert!(
        sstats.block_grants > grants_at_successor0,
        "block_grants advanced at the SUCCESSOR's holding: {sstats:?}"
    );
    for blk in &minted_after {
        assert!(
            sholding.bitmap.is_set(*blk),
            "minted block {blk} not SET at the successor"
        );
    }
    assert_eq!(
        squeezefs::block_grant::free_target_for(data_tag).as_deref(),
        Some(venue2.endpoint().as_str()),
        "the data volume's free target followed the holder"
    );
    // A terminal free of a post-failover block clears the bit at the
    // successor (the joiner's frees route to the holder's ladder).
    let freed = *minted_after.iter().next().unwrap();
    b.begin_free(freed * b.chunk_size());
    b.finish_free(freed * b.chunk_size());
    assert!(
        !sholding.bitmap.is_set(freed),
        "the free reached the successor's bitmap"
    );
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

/// **A joiner's FREE TARGET follows a manager failover with NO grant ask
/// in between** (PR 12b round 3, F5 — the `sym-crash` leg's round 2: the
/// second failover's successor moved the holder's listener while the
/// joiner's grant WINDOW still covered its writes, so the grant sink —
/// the only arm that re-resolved the venue — never ran, the data volume's
/// free target kept the dead holder's address for the mount's life, the
/// per-holder custody JOIN there was refused (`Connection refused …
/// declined until the next renewal beat`) and `free_shipped_blocks` sat
/// flat 60 s after the rewrite). The free path re-resolves the venue off
/// durable state at its own transport failure
/// (`alloc_lease::refresh_free_target`): here the joiner's allocation arm
/// registers the venue, the manager leaves and a successor publishes a
/// new listener, nothing mints, and the refresh moves the free target to
/// the successor (`moves` = 1) — the grant sink untouched (`block_grant_
/// topups` flat).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_free_target_follows_a_failover_the_grant_window_covered() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::meta_backend::kv::alloc_lease;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;
    let data_id = "vol-free-follows-data";
    let data_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(data_id);
    let data_blocks = 4096u64;
    let a = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    a.set_capacity_bytes(data_blocks * a.chunk_size());
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&manager, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );

    let joiner = join(&uris, &venue, &mvol, 77).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jwire = Arc::clone(jvol.joined_wire().expect("joined"));
    let old_endpoint = venue.endpoint();
    // The production arm: the venue registered behind the grant sink AND
    // the free target.
    let hv = squeezefs::sym_join::joined_holder_venue(&joiner, data_tag, old_endpoint.clone());
    let b = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    b.set_capacity_bytes(data_blocks * b.chunk_size());
    assert_eq!(
        alloc_lease::arm_joined_allocation(
            &[Arc::clone(&b)],
            &|_| Arc::clone(&hv),
            VENUE_SECRET,
            jwire.identity,
        ),
        1
    );
    assert_eq!(
        squeezefs::block_grant::free_target_for(data_tag).as_deref(),
        Some(old_endpoint.as_str())
    );
    let topups0 = b.block_grant_topups();

    // The failover: the manager leaves, a successor publishes elsewhere.
    // (In one process the manager's leave uninstalled the free target it
    // shared with the joiner — the process-global table; a second daemon's
    // would stand at the dead address. Either way the refresh re-homes it.)
    venue.tear_down();
    shutdown(&manager).await;
    alloc_lease::disarm_symmetric_roles();
    drop(mvol);
    drop(manager);
    let successor = open_under(&uris, &Knobs::armed()).await;
    let svol = Arc::clone(&successor.volumes[0]);
    let venue2 = HoldersVenue::stand_up(&successor, &[]).await;
    assert_ne!(venue2.endpoint(), old_endpoint);
    squeezefs::multi_writer::publish_symmetric_endpoint(&successor, &venue2.endpoint()).await;
    svol.checkpoint_now().await.unwrap();

    // No mint, no ask: the free path's re-resolve alone moves the target.
    assert_eq!(hv.moves.load(Relaxed), 0);
    let (endpoint, moved) = alloc_lease::refresh_free_target(data_tag)
        .await
        .expect("the joiner's venue is registered behind its free target");
    assert!(moved, "the venue re-resolved to the successor");
    assert_eq!(endpoint, venue2.endpoint());
    assert_eq!(hv.moves.load(Relaxed), 1);
    assert_eq!(
        squeezefs::block_grant::free_target_for(data_tag).as_deref(),
        Some(venue2.endpoint().as_str()),
        "the free target followed the holder with no grant ask"
    );
    assert_eq!(b.block_grant_topups(), topups0, "the grant sink never ran");
    // A second refresh is a no-op (the venue stands).
    assert_eq!(
        alloc_lease::refresh_free_target(data_tag).await,
        Some((venue2.endpoint(), false))
    );

    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    venue2.tear_down();
    shutdown(&successor).await;
    drop(svol);
    drop(successor);
    fsck_clean(&uris).await;
}

/// **A joiner's CHECKPOINT CYCLE whose wire verb meets a dead manager
/// completes — and follows the successor** (PR 12b round 3, F3 — the
/// `sym-storm` fleet leg's second face: the manager closed a joiner's
/// idle wire session, the joiner's next cadence `ReturnExtents` failed
/// transport-class inside its checkpoint cycle, and the re-dial's
/// projection refresh took the volume's SMO mutex — the mutex the
/// checkpoint task HOLDS for the whole cycle. The task deadlocked on
/// itself, its ring never drained, the conveyor parked at admission and
/// the D1.b lattice fail-stopped the volume 15 s later: `3 consecutive
/// journal write failures — volume marked FAILED` on a healthy device,
/// under an `rm -rf`). Here: the joiner leases a slot, a forced
/// compaction retires its root image (a returnable extent for its next
/// cadence), the manager LEAVES and a successor publishes a NEW listener,
/// and the joiner's very next act is `checkpoint_now` — the cycle must
/// COMPLETE inside a bound (red before: never), re-dial ONCE to the
/// successor, land the return there (the retired extent free in the
/// successor's bitmap), and the joiner's ring must keep draining (a
/// commit lands after it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_checkpoint_cycle_meeting_a_dead_manager_completes_and_follows_the_successor() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join(&uris, &venue, &mvol, 73).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jwire = Arc::clone(jvol.joined_wire().expect("joined"));
    let old_endpoint = venue.endpoint();
    // The joiner leases SLOT_A (a first touch over the wire) and its tree
    // has a durable root once a cycle flushed it.
    let files = create_files(&joiner, shared, "cc", 8).await;
    jvol.checkpoint_now().await.unwrap();
    let tree = jvol.slot_tree(SLOT_A).expect("the joiner's tree of SLOT_A");
    let old_root = tree.root();
    assert_ne!(old_root.addr, 0, "a flushed root");
    let sb = jvol.superblock();
    let old_ext = (old_root.addr - sb.heap.start) / u64::from(sb.node_size);
    // The custody transfer's wire half: the manager's grant entry moved
    // SLOT_A's live image into the joiner's `extent_grant` record, and the
    // joiner's RAM grant CLAIMS it (before: the record named it, the RAM
    // grant did not — every retirement of an inherited image was dropped
    // at `free_pending`'s claimed guard, an orphan only C13 reached).
    assert!(
        mvol.extent_grant_record(1).await.unwrap().contains(old_ext),
        "the manager's record names the inherited image {old_ext}"
    );
    let region_stats = |v: &KvMetaBackend| {
        v.appender_stats()
            .unwrap()
            .regions
            .into_iter()
            .find(|r| r.id == 1)
            .expect("the joiner's own region")
    };
    let before = region_stats(&jvol);
    // A forced compaction retires the root image: parked on the joiner's
    // tail, returnable at its next cycle's barrier — the cadence's
    // `ReturnExtents` is that cycle's wire verb.
    assert_eq!(
        jvol.defrag_compact_nodes(&[(0, old_root.addr)])
            .await
            .unwrap(),
        1
    );
    assert_ne!(tree.root().addr, old_root.addr);
    let after = region_stats(&jvol);
    assert_eq!(
        after.grant_pending,
        before.grant_pending + 1,
        "the retired inherited image is PARKED on the joiner's grant: before {before:?} after \
         {after:?}"
    );
    let returns0 = jvol.joined_stats().unwrap().wire_extent_returns;
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

    // The joiner's NEXT act is its checkpoint cycle: the barrier makes the
    // retired image returnable, the cadence's `ReturnExtents` fails at
    // the dead endpoint, and the re-dial runs INSIDE the cycle — under
    // the SMO mutex the cycle holds. Red before: the refresh re-took it
    // and the task never returned (the bound is the storm leg's 15 s
    // fail-stop, generously).
    let cycled = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        // Two cycles: the first's barrier parks the retirement past the
        // tail, the second's cadence returns it (the manager's own shape,
        // `a_clean_remount_recovers_the_grant_from_tree_zero_…`).
        for _ in 0..4 {
            jvol.checkpoint_now().await.unwrap();
            if jvol.joined_stats().unwrap().wire_extent_returns > returns0 {
                break;
            }
        }
    })
    .await;
    assert!(
        cycled.is_ok(),
        "the joiner's checkpoint cycle DEADLOCKED meeting the dead manager (its re-dial's \
         projection refresh re-took the SMO mutex the cycle holds)"
    );
    let js = jvol.joined_stats().unwrap();
    assert_eq!(js.wire_redials, 1, "ONE re-dial, inside the cycle: {js:?}");
    assert!(
        js.wire_extent_returns > returns0,
        "the cadence's ReturnExtents landed at the successor: {js:?}"
    );
    assert_eq!(
        jwire.endpoint(),
        venue2.endpoint(),
        "the wire follows the successor"
    );
    assert!(
        !svol.allocator().is_allocated(old_ext),
        "the retired image {old_ext} returned to the SUCCESSOR's bitmap"
    );
    // The ring keeps draining: a commit after the cycle lands and every
    // acked record reads at the successor.
    let more = create_files(&joiner, other, "cd", 4).await;
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

/// **A joiner's rotor SHRINKS to the derived `M`, and its census is the
/// set's** (review round 1, Issue 10 — a PR 4 manager-only assumption the
/// §3 sweep missed): the §5.1.3 forced shrink released idle rotor slots
/// through region id 0 — the MANAGER's — so on a joiner every release
/// answered "not one of this mount's regions", swallowed as "not this
/// tick", and a joiner NEVER shrank its rotor; and its `appenders_known`
/// was frozen at the join, so `M` never re-derived there either.
/// Unreachable below 512 writers (`M = clamp(W/(2 × writers), 1, 64)`
/// stays 64) — the rung's headline is N UNBOUNDED, and at ≥ 512 writers
/// the joiners hoarded 64 each while the derivation said 32. Here: the
/// census seam names 1,024 writers on the joiner, one cadence tick
/// releases its idle rotor down to 32 (`slot_forced_shrinks`, tree 0
/// `Unleased` at the manager), and a THIRD daemon's join is read by the
/// first joiner's next cadence (`appenders_known` 2 → 3). Red before:
/// the rotor stayed at 64 with `forced_shrinks == 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_rotor_shrinks_to_the_derived_m_and_its_census_follows_the_set() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 81).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jplane = Arc::clone(jvol.slot_leases().expect("armed"));
    let jid = jvol.appender_stats().unwrap().appender_id;
    let rotor0 = jplane.rotor.load_full();
    assert_eq!(rotor0.len(), 64, "a fresh joiner's rotor is the full M");
    assert_eq!(jvol.appender_stats().unwrap().appenders_known, 2);

    // The census seam: 1,024 writers ⇒ M = 65,536 / 2,048 = 32.
    jplane
        .test_writers_known
        .store(1024, std::sync::atomic::Ordering::Relaxed);
    jvol.slot_lease_cadence()
        .await
        .expect("the joiner's cadence");
    let st = jvol.slot_lease_stats().unwrap();
    let rotor1 = jplane.rotor.load_full();
    assert_eq!(
        rotor1.len(),
        32,
        "the rotor shrank to the derived M: {st:?}"
    );
    assert!(
        st.forced_shrinks >= 32,
        "the shrink released through the joiner's OWN region: {st:?}"
    );
    let released: Vec<ForestSlot> = rotor0
        .iter()
        .copied()
        .filter(|s| !rotor1.contains(s))
        .collect();
    assert_eq!(released.len(), 32);
    for s in &released {
        assert!(
            matches!(
                tree0_state(&mvol, *s).await,
                Some(SlotState::Unleased { .. })
            ),
            "slot {s} released at the manager"
        );
    }
    for s in rotor1.iter() {
        assert!(
            matches!(
                tree0_state(&mvol, *s).await,
                Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
            ),
            "slot {s} still the joiner's"
        );
    }
    jplane
        .test_writers_known
        .store(0, std::sync::atomic::Ordering::Relaxed);

    // A THIRD daemon joins: the first joiner's next cadence reads the
    // census in force from the directory.
    let third = join(&uris, &venue, &mvol, 82).await;
    let tvol = Arc::clone(&third.volumes[0]);
    jvol.checkpoint_now()
        .await
        .expect("the joiner's checkpoint");
    assert_eq!(
        jvol.appender_stats().unwrap().appenders_known,
        3,
        "the joiner's census followed the third daemon's join"
    );
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&tvol, "third");
    assert_must_stay_zero(&mvol, "manager");

    shutdown(&third).await;
    drop(tvol);
    drop(third);
    shutdown(&joiner).await;
    drop(jplane);
    drop(jvol);
    drop(joiner);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
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
    // Review round 1, Issue 6: the identity-carrying verbs bind the
    // frame's identity to the SESSION's authenticated peer — an
    // authenticated IMPOSTOR (volume access, another peer id) cannot
    // `PublishEndpoint` a listener for the joiner (redirecting every
    // peer's traffic to it) nor `LeaveAppender` its region; both are
    // REJECTED on `manager_verb_rejected`, nothing written. The joiner's
    // own session (its member id as the peer) is what the door admits.
    {
        let rejected0 = mvol.appender_stats().unwrap().manager_verb_rejected;
        let mut impostor = squeezefs::meta_ship::manager::ManagerClient::connect(
            &venue.endpoint(),
            VENUE_SECRET,
            "node_deadbeefdeadbeef.m00000007",
            0,
        )
        .await
        .expect("an authenticated session under another peer id");
        let e = impostor
            .publish_endpoint(identity, id, "127.0.0.1:9", 0)
            .await
            .err()
            .map(|e| e.to_string())
            .expect("PublishEndpoint for another appender's identity is REJECTED");
        assert!(
            e.contains("REJECTED") && e.contains("speaks for itself"),
            "{e}"
        );
        let e = impostor
            .leave_appender(identity, id, &[])
            .await
            .err()
            .map(|e| e.to_string())
            .expect("LeaveAppender for another appender's identity is REJECTED");
        assert!(
            e.contains("REJECTED") && e.contains("speaks for itself"),
            "{e}"
        );
        assert_eq!(
            mvol.appender_stats().unwrap().manager_verb_rejected,
            rejected0 + 2,
            "two rejections, the buggy/hostile-peer class"
        );
        assert_eq!(
            squeezefs::sym_join::resolve_holder_endpoint(&mvol, id)
                .await
                .as_deref(),
            None,
            "nothing was published for the joiner by the impostor"
        );
        assert!(
            matches!(
                page_of(&uris[0], &mvol, id).await.map(|p| p.state),
                Some(AppenderState::Live)
            ),
            "the joiner's page stays Live"
        );
    }
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
        jvol.joined_stats().unwrap().meta_hold_standing,
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

/// **The joiner's durable writer ERA is its appender page's TERM** (found
/// by the fidelity tier's first real second daemon: rung 7's custody
/// owner refused to arm on a process with `dlm_term = 0` — the D0 claim
/// gate publishes the manager's era, and a joiner runs no claim). The
/// page term is bumped at every join of the identity and barriered by
/// the manager's page write, so the successor of a dead joiner at the
/// same mount point dominates its predecessor's tokens on their shared
/// staging root (§6.11); `adopt_durable_term` never regresses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_durable_writer_term_is_its_page_term_and_rises_at_a_rejoin() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let manager_term = squeezefs::dlm::durable_term();

    let joiner = join(&uris, &venue, &mvol, 81).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let page_term = page_of(&uris[0], &mvol, id).await.expect("the page").term;
    assert!(page_term >= 1, "a joined page carries a term");
    assert!(
        squeezefs::dlm::durable_term() >= page_term && squeezefs::dlm::durable_term() != 0,
        "the process era covers the joiner's page term ({} ≥ {page_term})",
        squeezefs::dlm::durable_term()
    );
    assert!(
        squeezefs::dlm::durable_term() >= manager_term,
        "the era never regresses below the manager's (one process here)"
    );
    // The joiner dies; its successor at the same identity rejoins over
    // the Live page: term + 1, and the era rises with it.
    let files = create_files(&joiner, dirs[0], "era", 4).await;
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let back = join(&uris, &venue, &mvol, 81).await;
    let bvol = Arc::clone(&back.volumes[0]);
    let page_term2 = page_of(&uris[0], &mvol, id).await.expect("the page").term;
    assert_eq!(page_term2, page_term + 1, "the rejoin bumped the term");
    assert!(
        squeezefs::dlm::durable_term() >= page_term2,
        "the successor's era dominates its predecessor's"
    );
    assert_all_resolve(&back, dirs[0], &files).await;
    shutdown(&back).await;
    drop(bvol);
    drop(back);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}

/// **The manager's claim-set entry carries its DATA registrant key** (found
/// by the fidelity tier's first real second daemon: under the join ladder
/// the membership arm enrolls the node at rung 3 — BEFORE rung 4 takes the
/// data WERO hold — so the entry read `pr_key 0`, and every co-located
/// joiner's adoption found no enrolled key to cross-check the standing
/// holder against). The rung-7 publish refreshes the key from the live
/// hold, so the entry names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_managers_published_claim_set_entry_names_its_live_data_registrant_key() {
    use squeezefs::membership::{ClaimSet, MemberIdentity, MemberRole};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    // Rung 3's shape: the node enrolled with NO key (no hold stands yet).
    let node = squeezefs::cowriter::node_member_id().unwrap();
    squeezefs::membership::upsert_writer_member(
        &mvol,
        &MemberIdentity {
            id: node.clone(),
            role: MemberRole::Writer,
            pid: std::process::id(),
            boot: squeezefs::meta_backend::kv::backend::read_boot_id(),
            endpoint: None,
            pr_key: 0,
        },
        squeezefs::dlm::durable_term(),
    )
    .await
    .unwrap();
    // Rung 4: a PR-capable data namespace, the hold taken.
    let data = dir.path().join("data-pr");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::new();
    reservation::install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-pr12b-data", "host-pr12b-data"),
    );
    let _restore = ClearOverride(vec![data.clone()]);
    let hold = squeezefs::data_custody::acquire_wero(std::slice::from_ref(&data))
        .expect("the manager's data hold");
    let key = ns.holder().expect("held");
    assert_eq!(squeezefs::data_custody::live_wero_key(), Some(key));
    // Rung 7: the publish names the key beside the endpoint.
    squeezefs::multi_writer::publish_symmetric_endpoint(&manager, "127.0.0.1:4242").await;
    let set = ClaimSet::load(&mvol).await.expect("the claim set");
    let me = set
        .members
        .iter()
        .find(|m| squeezefs::membership::member_id_matches(&m.identity.id, &node))
        .expect("enrolled");
    assert_eq!(me.identity.endpoint.as_deref(), Some("127.0.0.1:4242"));
    assert_eq!(
        me.identity.pr_key, key,
        "the entry names the live data hold's key — what a co-located joiner's adoption \
         cross-checks the standing holder against"
    );
    assert_eq!(set.registrant_keys(), vec![key]);
    drop(hold);
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}

/// **The manager's custody owner admits a joiner's JOIN with NO lane**
/// (found by the fidelity tier's first real second daemon: every op on
/// the joiner's mount answered EAGAIN because the owner's join gate read
/// it as "not in this era's allocation partition"). Under the armed plane
/// the S9 lane partition is SUPERSEDED by PR 8's block grants, so the
/// owner runs NO partition at all — `arm_authority_planes` installs no
/// lane map — and answers `(0, 1)` (SOLO, no partition) to every member;
/// a map naming nobody (the first build's "SOLO") refused everyone. The
/// owner's law pinned at its own door: with no map installed the join
/// lands SOLO; with an empty map installed it refuses naming the
/// partition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_custody_owner_with_no_lane_map_admits_every_joiner_solo() {
    use squeezefs::data_grant::{JoinFrame, WriteCustodyOwner, CUSTODY_SCHEMA};
    use squeezefs::membership::{LeaseClock, LeaseClocks};
    let clocks = LeaseClocks::with_params(
        std::time::Duration::from_millis(3_000),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(400),
    )
    .expect("clocks");
    let owner = WriteCustodyOwner::arm(
        "manager-no-lanes",
        7,
        6,
        clocks,
        LeaseClock::monotonic(),
        None,
    )
    .expect("arms");
    let join = |client: &str| JoinFrame {
        schema: CUSTODY_SCHEMA,
        client: client.to_string(),
        pr_key: 0,
        prior_epoch: None,
    };
    let lease = owner
        .join(&join("node_joiner.m1"))
        .expect("no partition: every member joins SOLO");
    assert_eq!(
        (lease.writer_lane, lease.writers),
        (0, 1),
        "SOLO — no partition"
    );
    // The first build's shape: a map naming NOBODY refuses everyone.
    owner.install_lane_assignment(
        squeezefs::alloc_lane_grant::LaneAssignment::derive("", &[]).expect("derives"),
    );
    let err = owner
        .join(&join("node_joiner.m2"))
        .err()
        .map(|e| e.to_string())
        .expect("an empty map refuses");
    assert!(err.contains("allocation partition"), "{err}");
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

/// **A dead joiner's root, swapped into an extent the MANAGER once
/// retired, is recovered whole** (the `sym-storm` fleet leg's class — 84
/// of a dead joiner's 1,300 acked files absent at the manager, four of
/// its slot trees; found RED here by the fixture: the recovery FAILED with
/// "node … is a retired extent (stale pointer outside a traversal)"). The
/// node cache's `retired` veto — an extent this mount retired stays dead
/// until THIS mount publishes a node there — is a one-writer law: under
/// the plane a retired extent returns to the heap and is GRANTED to a
/// joiner, whose node there the manager must read back at the joiner's
/// death (and at every transfer). The carve un-retires what it hands out
/// (`NodeCache::unretire_extent`); a joiner's `ReturnExtents` does the
/// same for its projection. The shape: the manager swaps its root 120
/// times (120 retired extents, freed, back in the heap), releases the
/// slot; the joiner acquires it, grows it, swaps ITS root (into one of
/// those extents — its grant carves lowest-free-first), checkpoints
/// (the records live under its page root alone), dies; the manager
/// recovers every flushed record and its own. KD-SYM-3's page-root rule
/// (by generation, never node seq) rides the same path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_joiners_root_swapped_into_an_extent_the_manager_once_retired_is_recovered_whole() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    // The joiner opens FIRST: its node-seq handle starts at the ledger's
    // watermark of this instant and advances by ITS mints alone.
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;

    // The MANAGER grows the slot's tree (first touch, many mints — its
    // seqs run far past the joiner's watermark), flushes it and RELEASES
    // the slot: tree 0's `Unleased { root }` names a manager-minted root.
    let managers = create_files(&manager, shared, "m", 1_500).await;
    mvol.checkpoint_now().await.unwrap();
    // The manager's handle runs AHEAD of the joiner's: a long-lived
    // manager mints node seqs at every SMO of every tree it maintains —
    // here its own root swapped 120 times (the D4 nudge), a checkpoint
    // between each dozen to keep the grant and the reserve honest.
    for i in 0..120 {
        let root = mvol.slot_tree(SLOT_A).expect("the slot's tree").root();
        let n = mvol
            .defrag_compact_nodes(&[(0, root.addr)])
            .await
            .expect("the manager compacts its root");
        assert_eq!(n, 1, "root swap {i}");
        if i % 12 == 11 {
            mvol.checkpoint_now().await.unwrap();
        }
    }
    mvol.checkpoint_now().await.unwrap();
    mvol.release_slot_handover(0, SLOT_A)
        .await
        .expect("the manager releases the slot it grew");
    // The release is a ring-0 control entry; the joiner's projection is
    // keyed on the ledger seq, so the manager's next checkpoint is what
    // lets the joiner see the slot unleased.
    mvol.checkpoint_now().await.unwrap();
    let record_root = match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Unleased { root, .. }) => root,
        other => panic!("{other:?}"),
    };

    // The JOINER acquires it at g + 1 (first touch), grows it enough to
    // SPLIT (a root move — an append alone leaves the root pointer where
    // it was) and CHECKPOINTS: its records leave its window and live under
    // its page root alone — a root whose seq is BELOW the record's.
    jvol.refresh_control_projection().await.unwrap();
    let flushed = create_files(&joiner, shared, "j", 400).await;
    // The root MOVES under the joiner's own SMO (the D4 compaction nudge
    // = `smo_replace` on the root: a root swap in ITS ring under ITS
    // grant, the fleet's shape where the joiner's flush pass compacted a
    // leaf the manager had grown), then its checkpoint publishes it.
    // (The nudge draws on the region's grant, which the flush pass's
    // cadence refills — one checkpoint first.)
    jvol.checkpoint_now().await.unwrap();
    let root_before = jvol.slot_tree(SLOT_A).expect("the slot's tree").root();
    let compacted = jvol
        .defrag_compact_nodes(&[(0, root_before.addr)])
        .await
        .expect("the joiner compacts its own root");
    assert_eq!(compacted, 1, "the root was compacted (a root swap)");
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &mvol, id)
        .await
        .expect("the joiner's page");
    let page_root = page
        .slots
        .iter()
        .find(|e| {
            squeezefs::meta_backend::kv::appender::forest_slot_of_page_slot(
                e.slot,
                mvol.appender_stats().unwrap().native_slot,
            ) == SLOT_A
        })
        .map(|e| e.root)
        .expect("the page names the slot's root");
    assert_ne!(
        page_root, record_root,
        "the fixture's premise: the joiner's own SMO MOVED the root"
    );
    // In ONE process the two handles cannot be pulled apart: every install
    // of a manager root at the joiner (its projection refresh, the grant's
    // transfer barrier) RAISES its handle to that root's seq, so its later
    // mints always sit above the record's — the seq rule and the
    // generation rule coincide here, and the rule's RED venue was the
    // fleet (two processes, two handles). What this pin holds is the path:
    // a joiner's own root swap in its ring under its grant, flushed under
    // its page root alone, recovered whole by the manager.
    let g_lease = match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { g, appender_id, .. }) => {
            assert_eq!(appender_id, id);
            g
        }
        other => panic!("{other:?}"),
    };

    // The joiner dies; the ledger names it; the manager recovers it.
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Unleased { g, root, .. }) => {
            assert_eq!(g, g_lease);
            assert_eq!(
                root, page_root,
                "the recovered slot stands at the dead lessee's PAGE root (KD-SYM-3), not the \
                 grant-time root a seq comparison preferred"
            );
        }
        other => panic!("tree 0 after recovery: {other:?}"),
    }
    // Every flushed record of the dead joiner AND every manager record it
    // inherited resolve at the manager.
    assert_all_resolve(&manager, shared, &flushed).await;
    assert_all_resolve(&manager, shared, &managers).await;
    assert_must_stay_zero(&mvol, "manager");

    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **The KV loader's single-flight table is taken in ONE acquisition
/// style** (review round 1, Issue 13 — pre-existing, made ordinary by the
/// N-daemon posture's C1 population): `load_for_slot` took `inflight`'s
/// bucket ASYNC (`entry_async`) while the guard's `Drop` and every other
/// user took it SYNC — on a two-lane meta pool an async waiter GRANTED the
/// bucket resumes only when its task is polled, and the lanes that would
/// poll it sat in a sync wait on that very bucket: the fleet's `-o ro`
/// reader wedged both meta lanes under the fsck C1 walk's fan-out
/// (hundreds of slot-tree walks colliding on the same uncached nodes) and
/// `squeezefs fsck` hung 31 minutes. The shape: a forest of many slot
/// trees, every cached image dropped, hundreds of concurrent loads of the
/// SAME addresses through the meta pool — bounded here; a wedge is a
/// timeout, never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundreds_of_colliding_node_loads_on_the_meta_lanes_never_wedge() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let slots: Vec<ForestSlot> = (4..20).collect();
    let names: Vec<String> = slots.iter().map(|s| format!("d{s}")).collect();
    let seeds: Vec<(ForestSlot, &str)> = slots
        .iter()
        .zip(names.iter())
        .map(|(s, n)| (*s, n.as_str()))
        .collect();
    let (uris, dirs) = seeded_volume(dir.path(), &seeds).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    for d in &dirs {
        create_files(&manager, *d, "f", 60).await;
    }
    mvol.checkpoint_now().await.unwrap();
    mvol.checkpoint_now().await.unwrap();
    // Every slot tree's images dropped from the cache: the loads below
    // MISS and collide on the single-flight table.
    let roots: Vec<(ForestSlot, u64)> = slots
        .iter()
        .map(|s| (*s, mvol.slot_tree(*s).expect("seeded tree").root().addr))
        .collect();
    for (s, _) in &roots {
        mvol.node_cache()
            .drop_slot_nodes(*s)
            .expect("clean nodes drop");
    }
    let mut joins = Vec::new();
    for round in 0..40u32 {
        for (s, addr) in &roots {
            let cache = Arc::clone(mvol.node_cache());
            let (s, addr) = (*s, *addr);
            joins.push(squeezefs::meta_exec::spawn_meta_join(
                "colliding_load",
                async move {
                    let loaded = cache.load_for_slot(addr, Some(s)).await;
                    if round % 8 == 7 {
                        // Some loaders drop the image again behind the
                        // others — the miss/collide cycle repeats.
                        let _ = cache.drop_slot_nodes(s);
                    }
                    loaded.map(|n| n.is_some()).unwrap_or(false)
                },
            ));
        }
    }
    let all = async {
        let mut ok = 0usize;
        for j in joins {
            if j.await.unwrap_or(false) {
                ok += 1;
            }
        }
        ok
    };
    let ok = tokio::time::timeout(std::time::Duration::from_secs(120), all)
        .await
        .expect("640 colliding loads on the meta lanes must complete — a wedge here is the mixed-style single-flight lock");
    assert_eq!(ok, 640, "every load answered its node");
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
}

/// The fsck engine's INODE PLANE over an already-open WRITER set (the
/// `sym_crash_matrix_tests` door: a data router with no staging, the
/// inode-plane-only options).
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

/// **The manager's inode-plane census over a LIVE foreign lessee's
/// directory** (review round 1, Issue 1 — the `sym-storm` round-3 finding
/// ATTRIBUTED: 442 false C10 dangling names). The dentry pass walked
/// every slot tree of the volume from the manager's own cache — for a slot
/// a live JOINER leases that is a PROJECTION whose staleness the census
/// cannot bound (KD-SYM-5), while the child slots the joiner had just
/// released were re-read FRESH at the transfer — so every unlink the
/// joiner had performed in its directory read as a dangling name at the
/// manager. PR 8's lessee-shard law scoped the INODE side alone; the NAME
/// side is scoped now: with any slot of the volume leased to a live
/// foreign appender the plane records NO verdict, counted on
/// `fsck_inode_plane_foreign_dentry_scoped`, never a finding. The storm's
/// shape: the joiner first-touches D (slot A) and fills it past the
/// affinity cap so its children SPILL into its rotor slots, unlinks them
/// all (the dentries removed in A's tree, the records destroyed in the
/// rotor slots — every tx the joiner's own), releases the rotor slots that
/// held children (the manager re-reads them at the transfer: records
/// gone) and keeps A. Pre-fix: one `C10` dangling finding per spilled
/// child; post-fix: none, the plane scoped out; after the joiner's clean
/// leave the plane judges again and finds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_managers_census_takes_no_verdict_over_a_live_joiners_stale_projected_dentries() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "d")]).await;
    let d = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    // The manager is the S6 owner of the joiner's shard (the mount path's
    // rung 3): the joiner is a LIVE writer member — the census's liveness
    // word for a foreign lessee (a lessee not known live is PR 10's
    // frozen-tree class and IS judged).
    {
        use squeezefs::membership::{
            self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
        };
        let owner = MembershipOwner::arm(
            "census-owner",
            3,
            2,
            LeaseClocks::derive(std::time::Duration::from_micros(250)).expect("derived clocks"),
            LeaseClock::monotonic(),
        )
        .expect("arm the owner");
        membership::install_owner(Arc::clone(&owner));
        let ident = jvol.joined_wire().unwrap().identity;
        let JoinOutcome::Granted(_) = owner.join(JoinRequest {
            id: squeezefs::cowriter::node_member_id_of(ident.node_token, ident.mount_slot),
            role: MemberRole::Writer,
            endpoint: None,
            pid: std::process::id(),
            boot: "boot-census".to_string(),
            prior_epoch: None,
            pr_key: 0,
            mount: None,
        }) else {
            panic!("the joiner joins the manager's shard as a writer member");
        };
    }

    // The joiner's directory, filled past the affinity cap: the children
    // spill into the joiner's ROTOR slots (their slot ≠ A).
    let files = create_files(&joiner, d, "f", 900).await;
    let spilled: std::collections::BTreeSet<ForestSlot> = files
        .iter()
        .map(|(_, ino)| {
            let (_, local) = joiner.route_ino(*ino);
            squeezefs::meta_backend::kv::record::forest_slot_of_ino(local)
        })
        .filter(|s| *s != SLOT_A)
        .collect();
    assert!(
        !spilled.is_empty(),
        "the fixture's premise: children spilled past the parent's slot"
    );
    jvol.checkpoint_now().await.unwrap();
    // The manager's cache of A's tree holds the dentries: the storm's
    // manager had flushed them itself at the first incarnation's RECOVERY
    // — here the joiner releases A (the manager adopts the tree FRESH, its
    // 900 dentries in its images) and re-acquires it at its next touch.
    jvol.release_slot_handover(jid, SLOT_A)
        .await
        .expect("the joiner releases A");
    mvol.checkpoint_now().await.unwrap();
    assert_eq!(
        manager.readdir(d, 0, 2_000).await.unwrap().len(),
        files.len()
    );
    jvol.refresh_control_projection().await.unwrap();
    // The storm's `rm -rf`: every child unlinked at the joiner (A first-
    // touched again at g + 1) — the dentries out of A's tree, the records
    // destroyed in their slots — every tx the joiner's own.
    for (name, _) in &files {
        joiner.unlink(d, name).await.expect("unlink");
    }
    // The corpse sweep's act (the FUSE layer's batched destroys): the
    // unlinked records leave their slot trees.
    let inos: Vec<u64> = files.iter().map(|(_, ino)| *ino).collect();
    for chunk in inos.chunks(64) {
        joiner
            .destroy_inodes(chunk)
            .await
            .expect("destroy the corpses");
    }
    for _ in 0..3 {
        jvol.checkpoint_now().await.unwrap();
    }
    // The child slots released (the LRU release's act); A stays leased.
    for s in &spilled {
        jvol.release_slot_handover(jid, *s)
            .await
            .expect("release a child slot");
    }
    mvol.checkpoint_now().await.unwrap();
    assert!(
        matches!(tree0_state(&mvol, SLOT_A).await, Some(SlotState::Leased { appender_id, .. }) if appender_id == jid),
        "slot A stays the joiner's"
    );

    let report = inode_plane_over(&manager).await;
    // The defect's observable: the plane took a VERDICT over the volume
    // (`inode_plane_volumes_covered` 1) off a projection — 901 stale
    // dentries indexed in the storm's direction (A's names present, the
    // released children's records gone: 442 dangling), the inverse here
    // (A's names gone at the manager, the released children's records
    // still cached: 838 era-floor-shielded C9 ghosts) — the SAME defect,
    // a census over a tree whose staleness it cannot bound. Post-fix: no
    // verdict, no finding, the reason counted.
    assert_eq!(
        report.counters.inode_plane_volumes_covered, 0,
        "a volume with a LIVE foreign lessee's slot is NOT covered by this mount's inode plane"
    );
    assert_eq!(report.findings.len(), 0, "{:?}", report.findings);
    assert!(
        report.counters.inode_plane_foreign_dentry_scoped >= 1,
        "the plane records why it took no verdict (fsck_inode_plane_foreign_dentry_scoped)"
    );

    // The joiner's clean leave makes the set the manager's again: the
    // census judges, and finds nothing.
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    squeezefs::membership::uninstall();
    mvol.checkpoint_now().await.unwrap();
    let report = inode_plane_over(&manager).await;
    assert_eq!(report.findings.len(), 0, "{:?}", report.findings);
    assert!(report.counters.inode_plane_volumes_covered >= 1);
    assert!(joiner_gone_names(&manager, d, &files).await);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// Every unlinked name is gone through `routed` ("deleted stays deleted").
async fn joiner_gone_names(routed: &RoutedMetaBackend, dir: u64, files: &[(String, u64)]) -> bool {
    for (name, _) in files.iter().take(64) {
        if routed.lookup(dir, name).await.is_ok() {
            return false;
        }
    }
    true
}
