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
    squeezefs::membership::uninstall_window_decl_source();
    squeezefs::membership::uninstall();
    // The S8 phase table seeds every later plane's `N_floor` (its `Rtt`
    // mean): a contract that parks a served step (F-R4's pin) must not
    // hand the next one a seconds-long handover seed.
    squeezefs::meta_ship::test_reset_ship_phases();
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

/// Poll `cond` to true, loud past a bound (a seam's park is what it
/// waits for — never a product wait).
async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
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
    assert_must_stay_zero_with(vol, who, true);
}

/// [`assert_must_stay_zero`] with the flush-ceiling word judged by the
/// caller (`ceiling = false`): the cadence-timing pins judge
/// `flush_ceiling_overruns` over the WINDOW under test with the venue's
/// attribution beside it (PR 13g review round 3, Issue 22 — the storm's
/// TAIL after the verdict is the steady-state class on this venue, stated
/// by the pin, never a silent pass and never its law).
fn assert_must_stay_zero_with(vol: &KvMetaBackend, who: &str, ceiling: bool) {
    let s = vol.appender_stats().expect("a forest volume");
    assert_eq!(s.manager_verb_refusals, 0, "{who}: manager_verb_refusals");
    if ceiling {
        assert_eq!(s.flush_ceiling_overruns, 0, "{who}: flush_ceiling_overruns");
    }
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

/// **PR 13 (found by the `sym-scale` leg's N ladder — a PR 12b defect)**: a
/// joiner that LEAVES cleanly and REJOINS under the same identity keeps
/// committing — the manager never grants it an extent a live slot-tree
/// image sits in. The fleet's second incarnation hit `granted extent
/// barrier: node … is DIRTY inside an extent the manager just granted`
/// (EINVAL on its 27th create): a leaf of a tree it inherited back at the
/// rejoin was handed out again as a fresh grant, so the live image had
/// reached the manager's free bitmap somewhere between the first
/// incarnation's release of the slot and its `LeaveAppender`. Pinned as
/// the whole lifecycle: join → many creates (leaves, refills) → clean
/// leave → rejoin → many more creates, every acked name present, fsck +
/// C8 clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_leave_and_rejoin_of_one_identity_never_regrants_a_live_image() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;

    // Incarnation 1: enough creates that the slot trees span several
    // leaves and the grant refills over the wire; then the clean leave.
    let joiner = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let own_dir = joiner
        .create(shared, "job", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the joiner's own directory")
        .ino;
    let first = create_files(&joiner, own_dir, "a", 900).await;
    jvol.checkpoint_now().await.unwrap();
    let refills_1 = jvol.joined_stats().unwrap().wire_extent_grants;
    assert_must_stay_zero(&jvol, "joiner (incarnation 1)");
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert!(
        matches!(tree0_state(&mvol, SLOT_A).await, Some(SlotState::Unleased { root, .. }) if root.addr != 0),
        "SLOT_A released with its root: {:?}",
        tree0_state(&mvol, SLOT_A).await
    );

    // Incarnation 2: the same identity rejoins over its Free page, takes
    // its trees back at the first touch and keeps creating — every mint
    // and refill lands in extents no live image occupies.
    let joiner = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    assert_eq!(
        jvol.appender_stats().unwrap().self_recoveries,
        0,
        "a rejoin over a Free page recovers nothing"
    );
    assert_all_resolve(&joiner, own_dir, &first).await;
    let second = create_files(&joiner, own_dir, "b", 900).await;
    jvol.checkpoint_now().await.unwrap();
    let refills_2 = jvol.joined_stats().unwrap().wire_extent_grants;
    assert!(
        refills_1 + refills_2 >= 1,
        "the row exercised the wire refill ({refills_1} + {refills_2})"
    );
    assert_all_resolve(&joiner, own_dir, &first).await;
    assert_all_resolve(&joiner, own_dir, &second).await;
    assert_must_stay_zero(&jvol, "joiner (incarnation 2)");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert_all_resolve(&manager, own_dir, &first).await;
    assert_all_resolve(&manager, own_dir, &second).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **PR 13 (found by the `sym-scale` leg — a PR 12b defect, the granted
/// extents' cache barrier)**: a joiner that OPENS over a non-empty ring-0
/// window folds the manager's records into its PROJECTION of the
/// manager's slot-tree leaves, which then read DIRTY at the records' ring
/// positions (the writer-style replay — `discard_tree_nodes` states it for
/// tree 0). When the manager later compacts such a leaf, retires its
/// extent and GRANTS it to the joiner, the joiner's `drop_nodes_in_extents`
/// met its own projection's dirty node under the granted extent and
/// refused the grant as "this mount wrote to an image it did not own" —
/// `EINVAL` on the user's create (the fleet: the second incarnation's 27th
/// create under the manager's storm). A dirty node of a tree this mount
/// does not lease is a projection fold, dropped like a clean one; only a
/// dirty node of a LEASED tree is the defect the barrier refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_dirty_projection_of_a_retired_manager_leaf_never_refuses_its_grant() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager's own tree: many leaves, flushed once, then a window of
    // records over them that is NOT checkpointed when the joiner opens.
    let own = manager
        .create(1, "mgr", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the manager's directory")
        .ino;
    let _first = create_files(&manager, own, "m", 1_200).await;
    mvol.checkpoint_now().await.unwrap();
    let _window = create_files(&manager, own, "w", 600).await;
    // The joiner's open replays ring 0's window into its projection —
    // dirty images of the manager's leaves at the records' positions.
    let joiner = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jdir = joiner
        .create(shared, "job", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the joiner's directory")
        .ino;
    let seed = create_files(&joiner, jdir, "s", 40).await;
    // The manager compacts every leaf of its own tree (each swap retires
    // the old image) and checkpoints until the retired extents are back
    // in the free heap — the lowest addresses of the heap, exactly what
    // the joiner's next carve is handed.
    let mslot = squeezefs::meta_backend::kv::record::forest_slot_of_ino(manager.route_ino(own).1);
    let leaves: Vec<(u8, u64)> = mvol
        .slot_tree(mslot)
        .expect("the manager's slot tree")
        .reachable_node_addrs()
        .await
        .unwrap()
        .into_iter()
        .map(|addr| (0u8, addr))
        .collect();
    assert!(
        leaves.len() >= 3,
        "a multi-leaf tree ({} nodes)",
        leaves.len()
    );
    let compacted = mvol
        .defrag_compact_nodes(&leaves)
        .await
        .expect("the manager compacts its tree");
    assert!(compacted >= 1, "{compacted} node(s) compacted");
    for _ in 0..3 {
        mvol.checkpoint_now().await.unwrap();
    }
    // The joiner keeps creating: its grant refills carve from the heap the
    // manager just returned those images to. Every create must land.
    let more = create_files(&joiner, jdir, "j", 1_200).await;
    assert_all_resolve(&joiner, jdir, &seed).await;
    assert_all_resolve(&joiner, jdir, &more).await;
    let jw = jvol.joined_stats().unwrap();
    assert!(jw.wire_extent_grants >= 1, "the refill travelled the wire");
    assert_eq!(jw.wire_failures, 0);
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **PR 13 (the `sym-scale` N = 8 fleet row, found and pinned red-first)**:
/// four writers — the manager and three joiners — each storming its OWN
/// directory with concurrent creators while every one checkpoints on its
/// cadence (extent refills, returns, compactions — the extents churning
/// between appenders through the manager's heap) never lands another
/// appender's record in one of its leaves: every acked name resolves at
/// every daemon, no compaction is refused on finding 41's bounds check
/// ("carries records outside its key bounds" — the fleet read a joiner's
/// storm-directory dentries inside another joiner's leaf, then the D1.b
/// fail-stop of that volume), fsck + C8 clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_storms_on_four_writers_never_cross_a_record_into_another_appenders_leaf() {
    concurrent_storms(4, 3, 700, 1, true).await;
}

/// The fleet's `sym-scale` N = 8 shape in process (PR 13): eight writers,
/// four creators each, deep enough to spill past `A_max` into the rotor
/// slots and to cycle the grant (refills, returns, compactions, splits).
/// Heavy — `--ignored`; the four-writer pin above is the gate's.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn concurrent_storms_on_eight_writers_at_the_fleets_depth() {
    concurrent_storms(8, 4, 6_000, 2, false).await;
}

/// The same shape at N = 2 and N = 1 — the bisection instruments for a
/// finding the eight-writer form reports (which writer count first shows
/// it); `--ignored` like their parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn concurrent_storms_on_two_writers_at_the_fleets_depth() {
    concurrent_storms(2, 4, 6_000, 2, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn concurrent_storms_on_one_writer_at_the_fleets_depth() {
    concurrent_storms(1, 4, 6_000, 2, false).await;
}

async fn concurrent_storms(
    writers: usize,
    creators: usize,
    per_creator: usize,
    rounds: usize,
    strict_wire: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // The appender capacity is `heap/16 ÷ ring`: eight writers need a
    // wider volume than the contracts' default (never a client count).
    let uris =
        format_stamped_set_with_config_len(dir.path(), 1, VOL_LEN * (writers as u64).max(1)).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mut daemons: Vec<Arc<RoutedMetaBackend>> = vec![Arc::clone(&manager)];
    for n in 1..writers {
        daemons.push(join(&uris, &venue, &mvol, n as u32).await);
    }
    // Every daemon's own directory under `/` per round (a shipped step to
    // the manager — the root's dentries are slot 0's; the child a rotor
    // mint); `dirs` / `files` hold the LAST round's (every round's names
    // are verified as they land).
    let mut dirs: Vec<u64> = Vec::new();
    let mut files: Vec<Vec<(String, u64)>> = vec![Vec::new(); daemons.len()];
    // Every name a round REMOVED, per (dir, name): "deleted stays deleted"
    // is judged on them after every daemon left — at the census AND at a
    // fresh writer's lookup (a durable dentry that resolves is a
    // resurrection, not a census artifact).
    let mut removed: Vec<(u64, String)> = Vec::new();
    for round in 0..rounds {
        if round > 0 {
            // The fleet's row boundary: every writer REMOVES its previous
            // round's tree (the unlink storm — tombstones, leaf merges,
            // retired extents returned to the manager at the cadence and
            // re-granted) — then the idle cadence between rows.
            let mut rms = Vec::new();
            for (i, d) in daemons.iter().enumerate() {
                let d = Arc::clone(d);
                let dir = dirs[i];
                let names: Vec<String> = files[i].iter().map(|(n, _)| n.clone()).collect();
                removed.extend(names.iter().map(|n| (dir, n.clone())));
                rms.push(tokio::spawn(async move {
                    for name in names {
                        d.unlink(dir, &name)
                            .await
                            .unwrap_or_else(|e| panic!("writer {i} unlink {name}: {e}"));
                    }
                    // The round's directory stays (empty): a joiner's
                    // removal of its own directory under `/` answers
                    // `Dentry not found` in this fixture — noted for the
                    // cross-owner unlink's owner, not this pin's law.
                }));
            }
            for r in rms {
                r.await.expect("an unlink storm task");
            }
            tokio::time::sleep(std::time::Duration::from_millis(3_000)).await;
        }
        dirs.clear();
        for (i, d) in daemons.iter().enumerate() {
            dirs.push(
                d.create(
                    1,
                    &format!("w{i}-r{round}"),
                    libc::S_IFDIR | 0o755,
                    1000,
                    1000,
                )
                .await
                .expect("the writer's directory")
                .ino,
            );
        }
        // The storm: `creators` × `per_creator` files per daemon, ONE cadence task per
        // daemon checkpointing every 20 ms beside them (the production shape:
        // one checkpoint task per volume — the refill / return / compaction
        // churn the fleet's cadence drives).
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let corrupt: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let mut cadences = Vec::new();
        for (i, d) in daemons.iter().enumerate() {
            let d = Arc::clone(d);
            let stop = Arc::clone(&stop);
            let corrupt = Arc::clone(&corrupt);
            cadences.push(tokio::spawn(async move {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if let Err(e) = d.volumes[0].checkpoint_now().await {
                        let mut c = corrupt.lock().await;
                        if c.is_none() {
                            *c = Some(format!("writer {i} checkpoint: {e}"));
                        }
                        return;
                    }
                }
            }));
        }
        let mut tasks = Vec::new();
        for (i, d) in daemons.iter().enumerate() {
            for c in 0..creators {
                let d = Arc::clone(d);
                let dir = dirs[i];
                tasks.push(tokio::spawn(async move {
                    let mut out = Vec::with_capacity(per_creator);
                    for k in 0..per_creator {
                        let name = format!("c{c}-f{k:05}");
                        let ino = d
                            .create(dir, &name, libc::S_IFREG | 0o644, 1000, 1000)
                            .await
                            .unwrap_or_else(|e| panic!("writer {i} create {name}: {e}"))
                            .ino;
                        out.push((name, ino));
                    }
                    (i, out)
                }));
            }
        }
        for f in files.iter_mut() {
            f.clear();
        }
        for t in tasks {
            let (i, out) = t.await.expect("a creator task");
            files[i].extend(out);
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        for c in cadences {
            c.await.expect("a cadence task");
        }
        // ATTRIBUTION on a corrupt node: which daemon's trees reach the
        // address, and whose region grant claims its extent — the two words
        // that name the second custodian.
        if let Some(msg) = corrupt.lock().await.clone() {
            let addr: Option<u64> = msg
                .split("node at 0x")
                .nth(1)
                .and_then(|s| s.split(|c: char| !c.is_ascii_hexdigit()).next())
                .and_then(|h| u64::from_str_radix(h, 16).ok());
            if let Some(addr) = addr {
                for (i, d) in daemons.iter().enumerate() {
                    let v = &d.volumes[0];
                    let mut slots: Vec<ForestSlot> = v
                        .slot_leases()
                        .map(|p| p.gate.leased_slots())
                        .unwrap_or_default();
                    slots.push(squeezefs::meta_backend::kv::record::NATIVE_FOREST_SLOT);
                    for s in slots {
                        if let Some(t) = v.slot_tree(s) {
                            if let Ok(set) = t.reachable_node_addrs().await {
                                if set.contains(&addr) {
                                    eprintln!(
                                        "ATTRIBUTION: writer {i}'s slot {s} tree reaches {addr:#x}"
                                    );
                                }
                            }
                        }
                    }
                    if let Some(c) = v.forest_control_tree() {
                        if let Ok(set) = c.reachable_node_addrs().await {
                            if set.contains(&addr) {
                                eprintln!("ATTRIBUTION: writer {i}'s tree 0 reaches {addr:#x}");
                            }
                        }
                    }
                    let s = v.appender_stats().unwrap();
                    for r in &s.regions {
                        eprintln!(
                            "ATTRIBUTION: writer {i} region {} leases {} ring_entries {}",
                            r.id, r.leases, r.ring_entries
                        );
                    }
                }
            }
            panic!("{msg}");
        }
        for d in &daemons {
            d.volumes[0]
                .checkpoint_now()
                .await
                .expect("the final checkpoint");
        }
        for (i, d) in daemons.iter().enumerate() {
            assert_all_resolve(d, dirs[i], &files[i]).await;
            let s = d.volumes[0].appender_stats().unwrap();
            assert_eq!(
                s.extent_grant_conflicts, 0,
                "writer {i}: extent_grant_conflicts"
            );
            assert_eq!(
                s.extent_return_live_refusals, 0,
                "writer {i}: extent_return_live_refusals (a live image was about to be returned)"
            );
            if strict_wire {
                assert_must_stay_zero(&d.volumes[0], &format!("writer {i}"));
            } else {
                assert_eq!(
                    s.manager_verb_refusals, 0,
                    "writer {i}: manager_verb_refusals"
                );
                if let Some(j) = d.volumes[0].joined_stats() {
                    assert_eq!(j.control_refusals, 0, "writer {i}: joined_control_refusals");
                    if j.wire_failures > 0 {
                        eprintln!(
                            "writer {i}: {} wire refusal(s) retried (the manager's ring window)",
                            j.wire_failures
                        );
                    }
                }
            }
        }
    } // rounds
      // Deleted stays deleted at EVERY daemon while all are live (each
      // unlink was acked at its own creator).
    let resurrected_at = |r: &Arc<RoutedMetaBackend>, removed: &Vec<(u64, String)>| {
        let r = Arc::clone(r);
        let removed = removed.clone();
        async move {
            let mut out = Vec::new();
            for (dir, name) in &removed {
                if let Ok(ino) = r.lookup(*dir, name).await {
                    out.push((*dir, name.clone(), ino.ino, ino.nlink));
                }
            }
            out
        }
    };
    for (i, d) in daemons.iter().enumerate() {
        let r = resurrected_at(d, &removed).await;
        assert!(
            r.is_empty(),
            "writer {i} resolves {} of {} unlinked names while every daemon is live (first: {:?})",
            r.len(),
            removed.len(),
            &r[..r.len().min(4)]
        );
    }
    // Every joiner leaves cleanly; the manager reads every acked name —
    // and still none of the removed ones (the leave's flush-then-transfer
    // carried every tombstone). The screen's gauges before/after name the
    // mechanism if a name comes back.
    let screened0 = squeezefs::meta_backend::kv::META_KV_FOREIGN_FRAMES_SCREENED
        .load(std::sync::atomic::Ordering::Relaxed);
    let breach0 = squeezefs::meta_backend::kv::META_KV_APPENDER_FENCE_BREACH
        .load(std::sync::atomic::Ordering::Relaxed);
    for d in daemons.drain(1..) {
        // No pre-leave checkpoint here, by design: defect 6's attribution
        // found that a ROUTINE cycle before the leave carried the records
        // the leave's own did not, so a cycle at this point is exactly the
        // arm that hides the loss this pin exists to catch.
        let left_dirs: Vec<u64> = dirs.clone();
        // BEFORE the leave: where the leaving daemon's OWN tree routes each
        // removed name and what its fold says (the joiner's RAM truth the
        // release must carry) — compared below against the manager's
        // post-leave leaf for every name that comes back.
        // Only the TAIL of each directory's removed set (the loss has always
        // been the last unlinks): a full walk of 192,000 names delayed the
        // leave by seconds and the daemon's own cadence flushed the leaf
        // first — the pin went green (the Heisenbug that says the records
        // ARE in RAM and a ROUTINE cycle carries them where the LEAVE's do
        // not).
        let before: std::collections::HashMap<(u64, String), (u64, u64, String)> = {
            let mut m = std::collections::HashMap::new();
            let mut by_dir: std::collections::BTreeMap<u64, Vec<&String>> = Default::default();
            for (dir, name) in &removed {
                by_dir.entry(*dir).or_default().push(name);
            }
            for (dir, names) in by_dir {
                for name in names.iter().rev().take(300) {
                    if let Some(v) = locate_name(&d, &d.volumes[0], dir, name).await {
                        m.insert((dir, (*name).clone()), v);
                    }
                }
            }
            m
        };
        shutdown(&d).await;
        let r = resurrected_at(&manager, &removed).await;
        for (dir, name, _, _) in r.iter().take(3) {
            let after = locate_name(&manager, &manager.volumes[0], *dir, name).await;
            eprintln!(
                "LEAVE-DIFF {name} in dir {dir}: at the leaving daemon before its leave {:?}; \
                 at the manager after {:?}  ((leaf addr, leaf node_seq, fold))",
                before.get(&(*dir, name.clone())),
                after
            );
        }
        let mut by_dir: std::collections::BTreeMap<u64, Vec<String>> = Default::default();
        for (dir, name, _, _) in &r {
            by_dir.entry(*dir).or_default().push(name.clone());
        }
        eprintln!(
            "after a leave (this daemon's round dirs {left_dirs:?}): by dir {:?}",
            by_dir
                .iter()
                .map(|(d, v)| (*d, v.len(), v.first().cloned(), v.last().cloned()))
                .collect::<Vec<_>>()
        );
        eprintln!(
            "after a leave: the manager resolves {} removed name(s); foreign_frames_screened +{} \
             fence_breach +{}",
            r.len(),
            squeezefs::meta_backend::kv::META_KV_FOREIGN_FRAMES_SCREENED
                .load(std::sync::atomic::Ordering::Relaxed)
                - screened0,
            squeezefs::meta_backend::kv::META_KV_APPENDER_FENCE_BREACH
                .load(std::sync::atomic::Ordering::Relaxed)
                - breach0
        );
    }
    for (i, dir) in dirs.iter().enumerate() {
        assert_all_resolve(&manager, *dir, &files[i]).await;
    }
    // The manager's verdict is GATHERED, not asserted yet: whether the
    // DURABLE tree (a fresh writer's) resolves the same names tells a
    // stale RAM image at the manager (the adoption barrier's class) from
    // lost tombstones (the leave's), so both are read before either fails.
    let at_manager = resurrected_at(&manager, &removed).await;
    if let Some((dir, name, _, _)) = at_manager.first() {
        eprintln!("ATTRIBUTION at the MANAGER (its RAM tree after the leaves):");
        attribute_resurrection(&manager, &manager.volumes[0], dir, name).await;
    }
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    drop(daemons);
    // Deleted stays deleted: a fresh writer resolves NONE of the removed
    // names (each was unlinked by its own creator, the unlink acked).
    let mut at_fresh = Vec::new();
    if !removed.is_empty() {
        let fresh = open_under(&uris, &Knobs::armed()).await;
        for (dir, name) in &removed {
            if let Ok(ino) = fresh.lookup(*dir, name).await {
                at_fresh.push((*dir, name.clone(), ino.ino, ino.nlink));
            }
        }
        if let Some((dir, name, _, _)) = at_fresh.first() {
            eprintln!("ATTRIBUTION at a FRESH writer (the durable tree):");
            attribute_resurrection(&fresh, &fresh.volumes[0], dir, name).await;
        } else if let Some((dir, name, _, _)) = at_manager.first() {
            eprintln!(
                "ATTRIBUTION at a FRESH writer of the MANAGER's first resurrected name (absent \
                 here — the manager's image was stale):"
            );
            attribute_resurrection(&fresh, &fresh.volumes[0], dir, name).await;
        }
        shutdown(&fresh).await;
    }
    assert!(
        at_manager.is_empty() && at_fresh.is_empty(),
        "deleted did not stay deleted: the manager resolves {} of {} unlinked names after the \
         joiners' clean leaves (first: {:?}); a fresh writer resolves {} (first: {:?}) — a \
         fresh-writer count of 0 is a STALE image at the manager (the transfer's adoption \
         barrier), a nonzero one is lost tombstones (the leave's flush-then-transfer)",
        at_manager.len(),
        removed.len(),
        &at_manager[..at_manager.len().min(4)],
        at_fresh.len(),
        &at_fresh[..at_fresh.len().min(4)]
    );
    fsck_clean(&uris).await;
}

/// **Node incarnation seqs are ONE SPACE PER APPENDER INCARNATION**
/// (`kv::node_seq`, PR 13 — the fleet's `sym-scale` N = 8 row, defect 5).
/// Before it every appender seeded its node-seq handle from the SAME
/// ledger watermark at its open, so two joiners' first mints carried the
/// SAME `node_seq` and every seq guard the CoW law rests on — the §4.2
/// child pointer check, the root pointer check, the §4.5 frame-incarnation
/// check that ends a recycled extent's log at a previous node's frames —
/// was void ACROSS appenders. The device showed it: one node extent with
/// appender 3's header + base frame and three frames appender 4 appended
/// into it, all under one `node_seq`, both appenders folding each other's
/// records (finding 41's refusal, the tail pinned, D1.b). The law: the
/// manager (incarnation 0) mints in `[B, B + 2^K)` over the volume's uuid
/// base exactly as before; every JOIN — a rejoin included — is minted a
/// fresh incarnation `o ≥ 1` from the durable tree-0 counter and mints in
/// the disjoint `[B + o·2^K, B + (o+1)·2^K)`; no two appenders ever mint
/// an equal seq. RED on `8af38eda` (both joiners' roots read one seq).
/// **Issue 6 (PR 13 review round 1) — the `Joined.node_seq_base` wire word
/// is SCREENED at the joiner before anything is installed** (PR 3's
/// bounded-execution law: the `screen_release_words` shape). A manager
/// answering the volume's own base (incarnation 0 — its OWN space), an
/// off-stride word, or a word past the volume's capacity would put the
/// joiner back into (or straddling) another appender's node-seq space —
/// defect 5(a)'s P0 class from one buggy or hostile frame. Forged through
/// the served side's seam: the join REFUSES (`Rejected`, naming the word
/// and the base), `joined_wire_words_rejected` + 1, no joined backend
/// exists, and the next honest join lands with a screened base above the
/// volume's. RED before: the forged word was installed verbatim and the
/// join succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_node_seq_base_in_the_join_reply_is_refused_before_anything_is_installed() {
    use squeezefs::meta_backend::kv::backend::joined::JOINED_WIRE_WORDS_REJECTED;
    use squeezefs::meta_backend::kv::builder::node_seq_base;
    use squeezefs::meta_backend::kv::node_seq::{INCARNATION_ORDINAL_MAX, INCARNATION_SPACE};
    use squeezefs::meta_ship::manager::TEST_JOIN_FORGE_NODE_SEQ_BASE;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let b = node_seq_base(mvol.superblock().uuid);
    let forged = [
        ("incarnation 0 — the manager's own space", b),
        ("off the stride", b + INCARNATION_SPACE + 1),
        (
            "past the capacity",
            b + (INCARNATION_ORDINAL_MAX + 1) * INCARNATION_SPACE,
        ),
    ];
    for (what, word) in forged {
        let before = JOINED_WIRE_WORDS_REJECTED.load(Ordering::Relaxed);
        TEST_JOIN_FORGE_NODE_SEQ_BASE.store(word, Ordering::SeqCst);
        let err = try_join(&uris, &venue, &mvol, 7)
            .await
            .err()
            .unwrap_or_else(|| panic!("a forged node_seq_base ({what}) joined"));
        assert!(
            err.contains("node-seq base this joiner refuses") && err.contains("rejected"),
            "{what}: the refusal names the screen: {err}"
        );
        assert_eq!(
            JOINED_WIRE_WORDS_REJECTED.load(Ordering::Relaxed),
            before + 1,
            "{what}: counted on joined_wire_words_rejected"
        );
        assert_eq!(
            TEST_JOIN_FORGE_NODE_SEQ_BASE.load(Ordering::SeqCst),
            0,
            "the seam is consumed once"
        );
    }
    // The next honest join lands, its base screened: strictly above the
    // volume's base, on the stride.
    let j = join(&uris, &venue, &mvol, 8).await;
    // The handle starts at the screened base and mints upward inside its
    // incarnation's span, so `(now − B) / 2^K` is the incarnation ordinal
    // the manager minted — never 0, never past the capacity.
    let now = j.volumes[0].test_node_seq_now();
    let ordinal = (now - b) / INCARNATION_SPACE;
    assert!(
        now > b && (1..=INCARNATION_ORDINAL_MAX).contains(&ordinal),
        "an honest base: the handle {now:#x} over {b:#x} sits in incarnation {ordinal}"
    );
    let _ = create_files(&j, 1, "ok", 2).await;
    shutdown(&j).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **PR 13 review round 1, Issue 14 — the traversal's projection refresh
/// runs under the SMO guard its `try_lock` WON, and never parks behind a
/// held mutex.** The first build probed `smo.try_lock()`, DROPPED the
/// guard, and called `refresh_control_projection()`, which re-took the
/// mutex with a blocking `lock().await` — a holder arriving between the
/// probe and the lock (the checkpoint cycle, the wire re-dial's refresh)
/// parked the traversal behind it for the holder's whole pass, the exact
/// wait the probe existed to refuse (and the shape F3 deadlocked on). Two
/// arms: (a) the mutex HELD by the contract — the hook answers `false` at
/// once, no wait; (b) the mutex free and a newer ledger record standing —
/// the hook's refresh runs, and while it is parked inside its root moves
/// the mutex reads HELD from outside (the guard the probe won is the one
/// the refresh runs under; a re-take would self-deadlock here, a dropped
/// guard would read FREE — both RED), then it completes on release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traversal_refresh_runs_under_the_smo_guard_its_probe_won_and_never_parks_behind_a_holder(
) {
    use squeezefs::meta_backend::kv::backend::joined::{
        TEST_PROJECTION_REFRESH_NOTIFY, TEST_PROJECTION_REFRESH_PARK,
        TEST_PROJECTION_REFRESH_PARKED,
    };
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    TEST_PROJECTION_REFRESH_PARK.store(false, Ordering::SeqCst);
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&j.volumes[0]);
    let hook = Arc::clone(
        jvol.node_cache()
            .projection_refresh()
            .expect("a joined appender installs the traversal refresh"),
    );
    // The joiner's own checkpoint cadence takes the mutex for a cycle now
    // and then — take it when it is free (bounded).
    async fn hold_eventually(
        v: &KvMetaBackend,
    ) -> squeezefs::meta_backend::kv::backend::joined::SmoHold<'_> {
        for _ in 0..10_000 {
            if let Some(h) = v.test_try_hold_smo() {
                return h;
            }
            tokio::task::yield_now().await;
        }
        panic!("the joiner's SMO mutex never came free");
    }
    // (a) held: the hook refuses at once — never a park.
    let held = hold_eventually(&jvol).await;
    let answered = tokio::time::timeout(Duration::from_secs(10), hook.refresh())
        .await
        .expect("a refresh against a HELD mutex never waits for the holder");
    assert!(
        !answered,
        "a held mutex answers `false` — nothing refreshed"
    );
    drop(held);
    // (b) free, with a newer ledger record for the refresh to adopt.
    let _ = create_files(&manager, 1, "m", 3).await;
    mvol.checkpoint_now()
        .await
        .expect("the manager's checkpoint");
    TEST_PROJECTION_REFRESH_PARK.store(true, Ordering::SeqCst);
    let parked_before = TEST_PROJECTION_REFRESH_PARKED.load(Ordering::Acquire);
    let refresh = {
        let hook = Arc::clone(&hook);
        tokio::spawn(async move { hook.refresh().await })
    };
    // Register-recheck-await for the refresh's arrival at the seam.
    loop {
        let notified = TEST_PROJECTION_REFRESH_NOTIFY.notified();
        if TEST_PROJECTION_REFRESH_PARKED.load(Ordering::Acquire) > parked_before {
            break;
        }
        tokio::time::timeout(Duration::from_secs(30), notified)
            .await
            .expect("the refresh reaches its root moves");
    }
    assert!(
        jvol.test_try_hold_smo().is_none(),
        "while the traversal refresh moves roots, the SMO mutex is HELD — by the guard its \
         try_lock won"
    );
    TEST_PROJECTION_REFRESH_PARK.store(false, Ordering::SeqCst);
    TEST_PROJECTION_REFRESH_NOTIFY.notify_waiters();
    let moved = tokio::time::timeout(Duration::from_secs(30), refresh)
        .await
        .expect("the released refresh completes — no self-deadlock on a re-take")
        .expect("the refresh task");
    assert!(
        moved,
        "the projection advanced to the manager's newer ledger record"
    );
    drop(hold_eventually(&jvol).await); // the guard is released with the refresh
    shutdown(&j).await;
    venue.tear_down();
    shutdown(&manager).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_joined_appenders_never_mint_an_equal_node_seq() {
    use squeezefs::meta_backend::kv::node_seq::{incarnation_base, INCARNATION_SPACE};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    // One rotor mint each (a directory under `/` lands in the creator's
    // rotor — a fresh slot tree, one node minted from the handle) — and
    // one for the manager.
    for (r, name) in [(&j1, "j1"), (&j2, "j2"), (&manager, "m")] {
        r.create(1, name, libc::S_IFDIR | 0o755, 1000, 1000)
            .await
            .expect("a directory");
    }
    let seqs = |r: &Arc<RoutedMetaBackend>, who: &str| -> Vec<u64> {
        let v = &r.volumes[0];
        let mut out = Vec::new();
        for s in v
            .slot_leases()
            .map(|p| p.gate.leased_slots())
            .unwrap_or_default()
        {
            if let Some(t) = v.slot_tree(s) {
                out.push(t.root().seq);
            }
        }
        assert!(!out.is_empty(), "{who} minted no slot tree");
        out
    };
    let s0 = seqs(&manager, "the manager");
    let s1 = seqs(&j1, "joiner 1");
    let s2 = seqs(&j2, "joiner 2");
    let shared: Vec<u64> = s1.iter().copied().filter(|s| s2.contains(s)).collect();
    assert!(
        shared.is_empty(),
        "two joined appenders minted the same node_seq(s) {shared:?} (joiner 1 roots {s1:?}, \
         joiner 2 roots {s2:?}) — one per-volume seq space, the cross-appender CoW hazard"
    );
    // The spaces by construction: the manager's below incarnation 1's
    // base (the legacy space), joiner 1's inside incarnation 1's span,
    // joiner 2's inside incarnation 2's — every seq inside its own span.
    let b = squeezefs::meta_backend::kv::builder::node_seq_base(mvol.superblock().uuid);
    let (b1, b2) = (
        incarnation_base(b, 1).unwrap(),
        incarnation_base(b, 2).unwrap(),
    );
    assert!(
        s0.iter().all(|s| *s < b1),
        "the manager mints in incarnation 0: {s0:?} < {b1:#x}"
    );
    assert!(
        s1.iter().all(|s| (b1..b1 + INCARNATION_SPACE).contains(s)),
        "joiner 1 mints in incarnation 1: {s1:?} ∈ [{b1:#x}, +2^K)"
    );
    assert!(
        s2.iter().all(|s| (b2..b2 + INCARNATION_SPACE).contains(s)),
        "joiner 2 mints in incarnation 2: {s2:?} ∈ [{b2:#x}, +2^K)"
    );
    // A REJOIN is a new incarnation: joiner 1 leaves and joins again — its
    // next mint sits in incarnation 3's span, never back in 1's.
    shutdown(&j1).await;
    let j1b = join(&uris, &venue, &mvol, 1).await;
    j1b.create(1, "j1-again", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("a directory after the rejoin");
    let b3 = incarnation_base(b, 3).unwrap();
    let s1b = seqs(&j1b, "joiner 1 rejoined");
    assert!(
        s1b.iter().any(|s| (b3..b3 + INCARNATION_SPACE).contains(s)),
        "the rejoined joiner mints in incarnation 3's span: {s1b:?} vs [{b3:#x}, +2^K)"
    );
    assert!(
        !s1b.iter()
            .any(|s| (b1..b1 + INCARNATION_SPACE).contains(s) && !s1.contains(s)),
        "the rejoin minted nothing new in its dead incarnation's space: {s1b:?}"
    );
    shutdown(&j2).await;
    shutdown(&j1b).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A joined holder resolves a LATER joiner's slot to its holder at a
/// served step** (PR 13 — found by the fleet's first joiner→joiner
/// cross-owner create: `sym-shared-dir`, m61 creating into m60's
/// directory, EINVAL at every file). A joiner's lease table is its tree-0
/// PROJECTION, loaded at its open and advanced only by an event (a
/// re-dial, a divert failure, a redirect) — so joiner 1 read every slot
/// joiner 2 acquired afterwards as `Unleased`, and `screen_insert_child`
/// (the served insert's Issue-8a screen) refused the child "a dentry
/// nobody could have minted a target for". The one resolve the screen
/// reads now refreshes the projection once on `Unleased`
/// (`resolve_slot_holder_fresh`). RED before: the raw table answers
/// `Unleased` for joiner 2's rotor at joiner 1 (asserted as the premise),
/// the fresh resolve answers `Holder { joiner 2 }`, and joiner 2's create
/// into joiner 1's directory lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joined_holder_resolves_a_later_joiners_slot_at_a_served_step() {
    use squeezefs::slot_lease_core::Resolved;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // The directory is SEEDED (every daemon holds its record from the
    // format); joiner 1 acquires its slot by first touch, so joiner 2's
    // open projection names joiner 1 as its lessee.
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, shared, "j1", 2).await;
    // Joiner 2 joins AFTER joiner 1's projection was loaded and mints in
    // its own rotor.
    let j2 = join(&uris, &venue, &mvol, 2).await;
    let probe = j2
        .create(1, "j2-probe", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("joiner 2's directory")
        .ino;
    let (_, local) = j2.route_ino(probe);
    let slot2 = squeezefs::meta_backend::kv::record::forest_slot_of_ino(local);
    // The premise: joiner 1's RAW table has not seen joiner 2's lease.
    let raw = j1.volumes[0]
        .slot_leases()
        .expect("armed")
        .table
        .resolve(slot2);
    assert!(
        matches!(raw, Resolved::Unleased { .. }),
        "the premise — joiner 1's projection predates joiner 2's acquire: {raw:?}"
    );
    // The fresh resolve refreshes the projection once and names joiner 2.
    let fresh = j1.volumes[0].resolve_slot_holder_fresh(slot2).await;
    assert!(
        matches!(fresh, Some(Resolved::Holder { holder, .. }) if holder == 2),
        "joiner 1 resolves joiner 2's slot to its holder after the refresh: {fresh:?}"
    );
    // The served step itself: joiner 2 creates into joiner 1's directory
    // (a cross-owner create whose InsertDentry is served at joiner 1's
    // listener). The bindings rung 7 makes: joiner 1 serves on its own
    // venue, joiner 2 dials it where tree 0 names appender 1; the step
    // shipper is process-global, so it is the INITIATOR's (joiner 2's).
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let rejected0 = squeezefs::meta_backend::crossvol_tx::cross_owner_stats().steps_rejected;
    let child = j2
        .create(shared, "from-j2", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a cross-owner create served at joiner 1")
        .ino;
    // The served InsertDentry landed in joiner 1's tree (the dentry is
    // the holder's; the child's RECORD is joiner 2's and a read of it at
    // joiner 1 is a token read — the custody arm this fixture does not
    // stand up, so the record is asserted at its creator).
    assert_eq!(
        j1.lookup_dentry_exact_unguarded(shared, "from-j2")
            .await
            .expect("joiner 1 reads its dentry")
            .map(|(ino, _)| ino),
        Some(child),
        "the served insert is in joiner 1's tree"
    );
    assert_eq!(
        j2.getattr(child)
            .await
            .expect("the creator holds its child's record")
            .ino,
        child
    );
    assert_eq!(
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().steps_rejected,
        rejected0,
        "the served insert's child screen refused nothing"
    );
    shutdown(&j2).await;
    shutdown(&j1).await;
    j1venue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A foreign create into a STRIPED directory judges the stripe's record
/// by its HOLDER's word, never this mount's projection** (PR 13 — found by
/// the fleet's `sym-shared-dir`: every foreign writer created 1–3 files
/// into m60's directory, m60 flipped it to 64 stripes, and every later
/// foreign create was refused `ENOENT` "directory … was removed (no
/// record)" with `dir_stripe_dying_refusals` +1 at each initiator). The
/// stripes are minted in the holder's rotor AFTER the other daemons'
/// projections loaded, so `refuse_dying_parent`'s routed LOCAL read of the
/// key parent (the stripe) found no record at the initiator. The record
/// is read through `getattr` — the writer's read divert — now; in this
/// fixture (no custody arm) the divert reads locally, so the pin's
/// premise is the shape, and the fleet leg is the wire's row: joiner 2
/// creates into joiner 1's directory, joiner 1 flips it explicitly, joiner
/// 2's next creates route to stripes and land, `dying_refusals` unmoved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_create_into_a_striped_directory_reads_the_stripes_record_at_its_holder() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, shared, "j1", 2).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    // Joiner 2 is this process's READING writer (PR 9's custody arm —
    // process-global): its reads of joiner 1's slot are token reads at
    // joiner 1's listener, the fleet's shape — the map's markers and the
    // stripe's record come from the holder, never joiner 2's projection.
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let j2vol = Arc::clone(&j2.volumes[0]);
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
    // Before the flip: an ordinary foreign create, served at joiner 1.
    j2.create(shared, "pre-flip", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a cross-owner create before the flip");
    // Joiner 1 stripes the directory it holds (the stripes minted in ITS
    // rotor, after joiner 2's projection loaded); joiner 2 re-reads the
    // map through its token of the directory's dentries.
    j1.stripe_dir(shared, 4).await.expect("the holder's flip");
    assert!(
        j1.stripe_map(shared)
            .await
            .expect("map")
            .is_some_and(|m| m.stripes.len() == 4),
        "the holder reads its 4-stripe map"
    );
    let dying0 = squeezefs::meta_backend::dir_stripe::DIR_STRIPE_DYING_REFUSALS
        .load(std::sync::atomic::Ordering::Relaxed);
    for k in 0..12 {
        let name = format!("post-flip-{k}");
        let child = j2
            .create(shared, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap_or_else(|e| panic!("a foreign create into the striped directory ({name}): {e}"))
            .ino;
        let route = j2
            .stripe_route(shared, &name)
            .await
            .expect("route")
            .expect("the directory is striped at joiner 2 too");
        assert_eq!(
            j1.lookup_dentry_exact_unguarded(shared, &name)
                .await
                .expect("the holder reads the name")
                .map(|(ino, _)| ino),
            Some(child),
            "{name} landed in stripe {} of joiner 1's directory",
            route.index
        );
    }
    assert_eq!(
        squeezefs::meta_backend::dir_stripe::DIR_STRIPE_DYING_REFUSALS
            .load(std::sync::atomic::Ordering::Relaxed),
        dying0,
        "no stripe was judged dying by a stale projection"
    );
    squeezefs::data_grant::disarm_slot_custody().await;
    drop(j2vol);
    shutdown(&j2).await;
    shutdown(&j1).await;
    j1venue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **PR 13 fix round 1 — the explicit flip of a mount's OWN fresh directory
/// walks no projection** (found by the fix-round storm's round 8 from zero
/// on `16408a2f`: a rejoined joiner's `setfattr user.squeezefs.stripes` on
/// the round directory it had just made was refused `EINVAL` — the flip's
/// stripe check ran the reverse dentry scan over EVERY slot tree of the
/// volume, its PROJECTIONS of slots other appenders lease included, and
/// slot 10's tree, whose root its lessee had recycled, exhausted the
/// traversal budget (`restarts [root-seq] = 256`, a leased slot's tree no
/// refresh heals — KD-SYM-3); the leg died at 7/10 GREEN). A stripe has NO
/// ordinary name, so the directory-parent memo — fed at every directory
/// mint now — settles "is this a stripe?" for a directory this mount made
/// without a scan (`stripe_parent_dir`). Pinned: a joiner's `mkdir` + its
/// explicit flip move `meta_parent_scans` by 0 (RED before: +1, the scan
/// over the projections) and the map lands. The cold-directory case (a
/// memo miss — a directory another incarnation made) keeps the scan and
/// stays record §7 item 1's owed divert-aware form.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_flip_of_a_joiners_own_fresh_directory_walks_no_projection() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager holds a few slots the joiner will only project.
    let _ = create_files(&manager, 1, "m", 4).await;
    mvol.checkpoint_now().await.unwrap();
    let j = join(&uris, &venue, &mvol, 1).await;
    let d = j
        .create(1, "storm-w1-r1", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the joiner's own directory")
        .ino;
    let scans0 = squeezefs::fuse_client::METRICS
        .meta_parent_scans
        .load(Relaxed);
    j.stripe_dir(d, 4)
        .await
        .expect("the explicit flip of an own fresh directory lands");
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .meta_parent_scans
            .load(Relaxed),
        scans0,
        "the flip's stripe check read the parent memo, never the reverse scan over the projections"
    );
    assert!(
        j.stripe_map(d)
            .await
            .expect("map")
            .is_some_and(|m| m.stripes.len() == 4),
        "the map landed"
    );
    shutdown(&j).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A joiner's `stat` of a striped directory folds the stripes' records
/// as their HOLDER states them** (PR 13, defect 24 — found by `sym-scale`
/// N = 8 from zero on the defect-23 binary: the manager auto-striped `/`
/// under the seven joiners' `mkdir`s, and a joiner's next `stat /` folded
/// the 61 holder-minted stripes through `read_inode_value_routed` — its
/// PROJECTION of the manager's rotor slots, whose roots the lessee's
/// compaction had retired and the manager re-granted: `restarts
/// [root-seq] = 256`, `EIO` on the storm's create; PR 12b's refresh
/// re-adopts tree 0 and the native tree alone — a LEASED slot's root
/// rides its lessee's page, KD-SYM-3, so no refresh could heal it). The
/// fold and the rmdir's count probe read every stripe through `getattr`
/// — the writer's read divert, the holder's token plane — never the
/// projection. Pinned in the failure's observable shape: the stripes are
/// minted AFTER the reading joiner's projection loaded, so on the base
/// the fold saw NO stripe record and the joiner's `nlink` of the
/// directory lagged the holder's by the subdirectory the holder created
/// into a stripe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_stat_of_a_striped_directory_folds_the_stripes_at_their_holder() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, shared, "j1", 2).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let j2vol = Arc::clone(&j2.volumes[0]);
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
    // The holder stripes its directory AFTER joiner 2's projection
    // loaded (the stripes' records are invisible to that projection), then
    // creates a subdirectory into one stripe — the stripe's nlink 2 → 3.
    j1.stripe_dir(shared, 4).await.expect("the holder's flip");
    j1.create(shared, "sub", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("a subdirectory into a stripe");
    let holder_view = j1.getattr(shared).await.expect("the holder's fold");
    // Joiner 2 learns the map (the routing arm — its token of the
    // directory's dentries) and folds.
    assert!(
        j2.stripe_route(shared, "sub")
            .await
            .expect("route")
            .is_some(),
        "the directory is striped at joiner 2 too"
    );
    let joiner_view = j2.getattr(shared).await.expect("the joiner's fold");
    assert_eq!(
        joiner_view.nlink, holder_view.nlink,
        "the joiner's fold reads every stripe's record at its holder (the subdirectory's link)"
    );
    assert_eq!(
        joiner_view.mtime, holder_view.mtime,
        "the max over the stripes"
    );
    squeezefs::data_grant::disarm_slot_custody().await;
    drop(j2vol);
    shutdown(&j2).await;
    shutdown(&j1).await;
    j1venue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A striped directory's `stat` survives a SUPPLIER's death** (PR 13,
/// defect 24's second face — found by `sym-storm` from zero on the
/// defect-24 binary: seven joiners killed, `/` auto-striped with three
/// stripes they supplied, and the MANAGER's `stat /` — every path walk
/// across it, `.stats` included — failed `EIO` for the 15 s until the
/// recovery: the fold now read each stripe's record at its holder, and
/// three holders were dead). A holder that cannot be reached contributes
/// NOTHING to the fold for the window (`dir_stripe_fold_unreachable`) —
/// a dead lessee's stripe cannot move, the missing term is bounded by
/// the recovery — instead of failing the `stat`; the stripe's DENTRIES
/// stay exact-or-nothing. Pinned: the manager's `stat D` serves after the
/// supplier's listener died (RED: `EIO`), the fold lacking exactly that
/// stripe's subdirectory link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_striped_directorys_stat_at_the_holder_survives_a_suppliers_death() {
    use squeezefs::meta_backend::dir_stripe::DIR_STRIPE_FOLD_UNREACHABLE;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager's OWN hot directory.
    let hot = manager
        .create(1, "hot", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the manager's directory")
        .ino;
    // Checkpointed before the join: the served `SupplyStripeIno` at
    // joiner 1 reads `hot`'s record through its divert on a real mount;
    // this process's one custody arm is the MANAGER's (below), so joiner
    // 1 reads its projection — which the manager's page must name.
    mvol.checkpoint_now().await.expect("checkpoint");
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    mvol.slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&manager),
            "node-manager",
            VENUE_SECRET.to_vec(),
        ),
    );
    // The MANAGER is this process's reading writer (PR 9's custody arm —
    // process-global): its reads of joiner 1's slot are token reads at
    // joiner 1's listener.
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &manager,
        &squeezefs::cowriter::node_member_id().expect("this node's member id"),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    // The flip names joiner 1 as a SUPPLIER: one stripe minted in ITS slot.
    manager
        .stripe_dir_with_suppliers(hot, 4, &[1])
        .await
        .expect("the flip with a supplied stripe");
    let map = manager
        .stripe_map(hot)
        .await
        .expect("map")
        .expect("striped");
    let supplied: Vec<u64> = map
        .stripes
        .iter()
        .copied()
        .filter(|s| {
            j1.volumes[0]
                .slot_leases()
                .expect("armed")
                .gate
                .is_leased(slot_of_global(&manager, *s))
        })
        .collect();
    assert_eq!(supplied.len(), 1, "one stripe lives in joiner 1's slot");
    // A subdirectory whose name routes into the SUPPLIED stripe: the
    // fold's term that stripe carries.
    let mut into_supplied = None;
    for i in 0..256 {
        let name = format!("sub{i}");
        let route = manager
            .stripe_route(hot, &name)
            .await
            .expect("route")
            .expect("striped");
        if route.stripe == supplied[0] {
            manager
                .create(hot, &name, libc::S_IFDIR | 0o755, 1000, 1000)
                .await
                .expect("a subdirectory into the supplied stripe");
            into_supplied = Some(name);
            break;
        }
    }
    into_supplied.expect("a name routing into the supplied stripe");
    let alive = manager
        .getattr(hot)
        .await
        .expect("the fold with every holder alive");
    // The supplier DIES un-recovered: its listener gone, its page Live.
    j1venue.tear_down();
    drop(j1);
    let unreachable0 = DIR_STRIPE_FOLD_UNREACHABLE.load(Relaxed);
    let dead = manager
        .getattr(hot)
        .await
        .expect("the fold serves while the supplier is dead (RED: EIO)");
    assert!(
        DIR_STRIPE_FOLD_UNREACHABLE.load(Relaxed) > unreachable0,
        "the unreachable stripe was skipped, counted"
    );
    assert_eq!(
        dead.nlink,
        alive.nlink - 1,
        "the fold lacks exactly the dead supplier's stripe's subdirectory link"
    );
    squeezefs::data_grant::disarm_slot_custody().await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A consumed checkpoint seq is a LEDGER seq — a token reader's poll
/// walks across a joiner's leave** (PR 13, defect 25 — found by the
/// fleet's `sym-storm` round 1: seven regions released at once consumed
/// seven checkpoint seqs with no ledger record, PR 5's predicted-slot
/// poll stopped on the older record at `(adopted + 1) % 32` — "the writer
/// has not written that seq" — and the `-o ro` reader adopted NOTHING for
/// 41 s, until the writer's seq wrapped the ring: its tree 0 named the
/// dead lessees the whole time and every read of a recovered slot failed
/// at the dead address). Here the served `LeaveAppender` consumes the seq
/// (its bitmap write); the manager then writes on and checkpoints; the
/// reader's next poll must adopt the NEWEST record — the ledger is dense
/// (`meta_kv_ledger_restatements` +1), the predicted walk crosses the
/// consumed seq, the belt never fires. RED before: the reader stayed on
/// the record it adopted before the leave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readers_ledger_poll_walks_across_a_consumed_checkpoint_seq() {
    use squeezefs::meta_backend::kv::checkpoint::read_newest_ledger;
    use squeezefs::meta_backend::kv::revalidate::{read_newest_ledger_from, revalidation_stats};
    use squeezefs::meta_backend::kv::META_KV_LEDGER_RESTATEMENTS;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, shared, "j1", 2).await;
    mvol.checkpoint_now().await.expect("checkpoint");
    // The reader adopts the newest record `k`.
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    rv.arm_reader_revalidation(None)
        .expect("the reader's revalidation arms");
    rv.revalidate_reader().await.expect("poll");
    let path = rv.device_path().to_path_buf();
    let base = rv.superblock().root_ledger.start;
    let k = read_newest_ledger(&path, base)
        .await
        .expect("ledger")
        .expect("a record")
        .seq;
    assert_eq!(
        rv.reader_epoch(),
        k,
        "the reader stands on the newest record"
    );
    let restatements0 = META_KV_LEDGER_RESTATEMENTS.load(Relaxed);
    let gap_scans0 = revalidation_stats().gap_scans;
    // The joiner LEAVES: the served `LeaveAppender` returns its ring and
    // writes the bitmap at a CONSUMED checkpoint seq — which now writes
    // its ledger record too.
    shutdown(&j1).await;
    assert!(
        META_KV_LEDGER_RESTATEMENTS.load(Relaxed) > restatements0,
        "the consumed seq restated the ledger"
    );
    // The manager writes on: records PAST the consumed seq.
    manager
        .create(1, "after-the-leave", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a create after the leave");
    mvol.checkpoint_now().await.expect("checkpoint");
    let newest = read_newest_ledger(&path, base)
        .await
        .expect("ledger")
        .expect("a record")
        .seq;
    assert!(
        newest >= k + 2,
        "a consumed seq and a cycle past it ({k} → {newest})"
    );
    // DENSE: the predicted walk from `k` reaches the newest record.
    assert_eq!(
        read_newest_ledger_from(&path, base, k)
            .await
            .expect("walk")
            .map(|r| r.seq),
        Some(newest),
        "no gap between {k} and {newest}"
    );
    rv.revalidate_reader().await.expect("poll");
    assert_eq!(
        rv.reader_epoch(),
        newest,
        "the reader adopted the newest record across the consumed seq"
    );
    assert_eq!(
        revalidation_stats().gap_scans,
        gap_scans0,
        "the belt never fired: the writer kept the ledger dense"
    );
    for v in &reader.volumes {
        v.shutdown().await.unwrap();
    }
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A dominating requester earns an IDLE joined holder's tree through
/// the served ships** (§5.1.4 on the wire — gate 3c's IDLE row; PR 13,
/// found by the fleet's `sym-foreign-touch`: 12 dominating bursts of 64
/// over 133 s, `slot_offers` 0 everywhere, no handover). Two defects
/// under it, each red-first here: (1) PR 4's holder-side dominance
/// evaluation `note_slot_ship` had NO product caller — PR 6's served
/// step never noted the ship it served, so `ops_q` never accumulated on
/// any fleet; (2) a JOINED holder's offer reached `manager_offer_slot`,
/// a manager verb's executor, and refused (`joined_control_refusals`) —
/// it travels as the wire `OfferSlot` now. Pinned: joiner 2 ships
/// `N_floor × 4` creates into joiner 1's idle directory (served at
/// joiner 1's listener); joiner 1's plane counts the ships and ONE idle
/// offer, the manager's table holds the slot `Offered` to joiner 2 with
/// the offer on joiner 2's carriage, `joined_control_refusals` stays 0;
/// the accept recalls the slot from joiner 1 (the wire holder's law),
/// joiner 1's carriage sink runs the flush-then-transfer, and joiner 2's
/// retry holds the slot at `g + 1` with every acked name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dominating_requester_earns_an_idle_joined_holders_tree_through_served_ships() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a")]).await;
    let a = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let holder = join(&uris, &venue, &mvol, 1).await;
    let hvol = Arc::clone(&holder.volumes[0]);
    let own = create_files(&holder, a, "own", 4).await;
    let g_lease = match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased {
            appender_id: 1, g, ..
        }) => g,
        other => panic!("joiner 1 holds the directory's slot: {other:?}"),
    };
    let requester = join(&uris, &venue, &mvol, 2).await;
    let rvol = Arc::clone(&requester.volumes[0]);
    let rident = rvol.joined_wire().unwrap().identity;
    let hvenue = DaemonVenue::stand_up(&holder, false, "joiner-1").await;
    rvol.slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &hvenue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&requester),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let hplane = Arc::clone(hvol.slot_leases().expect("armed"));
    let mplane = Arc::clone(mvol.slot_leases().expect("armed"));
    let ships0 = hplane.ships.load(std::sync::atomic::Ordering::Relaxed);
    let offers = |p: &squeezefs::meta_backend::kv::slot_lease::SlotLeasePlane| {
        p.offers_idle.load(std::sync::atomic::Ordering::Relaxed)
            + p.offers_dominated
                .load(std::sync::atomic::Ordering::Relaxed)
    };
    let offers0 = offers(&hplane);
    // The dominating burst: the holder is IDLE on the slot (its own 4 ops
    // are the window's `ops_h`); `ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor`.
    let n_floor = hplane.n_floor();
    let burst = usize::try_from(n_floor.max(2) * 4).unwrap().max(16);
    let shipped = create_files(&requester, a, "touch", burst).await;
    let ships1 = hplane.ships.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        ships1 - ships0 >= burst as u64,
        "the holder noted every served ship ({ships0} → {ships1}, burst {burst})"
    );
    // The holder's own 4 creates sit inside the window, so the verdict is
    // the DOMINATED arm here (`ops_q ≥ 2 × ops_h`); the fleet's idle
    // holder ages out over `T_idle` and takes the idle arm — one law.
    assert_eq!(
        offers(&hplane),
        offers0 + 1,
        "ONE offer at the holder (N_floor {n_floor}; busy {}, wire failures {})",
        hplane
            .offers_busy
            .load(std::sync::atomic::Ordering::Relaxed),
        hvol.joined_stats().unwrap().wire_failures
    );
    assert_eq!(
        hvol.joined_stats().unwrap().control_refusals,
        0,
        "the joined holder's offer travelled — no local manager verb was attempted"
    );
    let routing_a = squeezefs::meta_backend::kv::appender::page_slot_of_forest_slot(
        SLOT_A,
        mvol.appender_stats().unwrap().native_slot,
    )
    .unwrap();
    let carriage = mplane.carriage_for(rident.node_token, rident.mount_slot);
    assert!(
        carriage.offered.iter().any(|(r, _)| *r == routing_a),
        "the manager's table offers the slot to joiner 2 on its carriage: {:?}",
        carriage.offered
    );
    // The accept: a wire holder's accepted offer is a RECALL on its
    // renewal; the holder's carriage sink runs the handover; the
    // requester's retry takes the slot at g + 1.
    let (_, accepted) = requester.act_on_slot_carriage(&[], &carriage.offered).await;
    let hident = hvol.joined_wire().unwrap().identity;
    let hcarriage = mplane.carriage_for(hident.node_token, hident.mount_slot);
    if accepted == 0 {
        assert_eq!(
            hcarriage.release_notices,
            vec![routing_a],
            "the accept recalled the slot from the wire holder"
        );
        // The MANAGER counts the handover: the wire holder's release
        // spends the recall its accepted offer raised (before PR 13 only
        // the in-process accept counted, so `slot_handovers` read 0 on
        // every fleet handover).
        let handovers0 = mplane.handovers.load(std::sync::atomic::Ordering::Relaxed);
        let ewma_h0 = hplane
            .ewma_handover_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        let (released, _) = holder
            .act_on_slot_carriage(&hcarriage.release_notices, &[])
            .await;
        assert_eq!(released, 1, "the holder's flush-then-transfer ran");
        // Defect 10's second half (PR 13 review round 1, Issue 10): the WIRE
        // holder's measured handover wall — flush + page + tree 0 — feeds
        // ITS `N_floor` (before it only the manager's in-process accept
        // folded a wall, and a wire holder kept the cold-start seed for
        // its life).
        assert_ne!(
            hplane
                .ewma_handover_ns
                .load(std::sync::atomic::Ordering::Relaxed),
            ewma_h0,
            "the wire holder's ewma_handover_ns moved with its flush-then-transfer"
        );
        assert_eq!(
            mplane.handovers.load(std::sync::atomic::Ordering::Relaxed),
            handovers0 + 1,
            "the manager counts the wire handover"
        );
        let (_, accepted_again) = requester.act_on_slot_carriage(&[], &carriage.offered).await;
        assert_eq!(accepted_again, 1, "the requester's retry holds the slot");
        let now = squeezefs::mono_core::monotonic_ns_u64();
        assert!(
            rvol.slot_leases().unwrap().in_cooldown(SLOT_A, now),
            "the new holder's never-thrash cooldown stands on ITS plane"
        );
    }
    match tree0_state(&mvol, SLOT_A).await {
        Some(SlotState::Leased { appender_id, g, .. }) => {
            assert_eq!(appender_id, 2, "joiner 2 holds the slot now");
            assert_eq!(g, g_lease + 1, "at the next generation");
        }
        other => panic!("tree 0 after the handover: {other:?}"),
    }
    // Every acked name — the holder's own and the shipped ones — is in
    // the tree the new holder received (the transfer is exact); the
    // shipped children's records are joiner 2's own, the holder's own
    // children live in joiner 1's rotor (a token read this fixture's
    // custody-armless requester cannot make — the dentry is asserted).
    for (name, ino) in &own {
        assert_eq!(
            requester
                .lookup_dentry_exact_unguarded(a, name)
                .await
                .expect("the new holder reads the transferred tree")
                .map(|(i, _)| i),
            Some(*ino),
            "{name} travelled with the slot"
        );
    }
    assert_all_resolve(&requester, a, &shipped).await;
    assert_eq!(hvol.joined_stats().unwrap().control_refusals, 0);
    assert_eq!(rvol.joined_stats().unwrap().control_refusals, 0);
    shutdown(&requester).await;
    drop(rvol);
    shutdown(&holder).await;
    drop(hvol);
    hvenue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 10 (PR 13; review round 1, Issue 10 — the pin the fix
/// lacked): a JOINED holder's `N_floor` leaves its absolute floor at the
/// FIRST served ship.** A joined appender arms before its first device
/// write or S8 ship, so both EWMA tables read 0 at the arm's seed and
/// `N_floor = max(2, ceil(0 / 0))` sat at 2 for the mount's life — the
/// fleet's shared-directory row handed the directory to the first
/// requester whose 2 ships beat the holder's second own op. `note_slot_
/// ship` re-seeds while `ewma_handover_ns` is 0 (the tables are populated
/// by then) and folds the ship's measured wall. Pinned: the joined
/// holder's plane in the fleet's cold-arm state (both EWMAs 0 — in this
/// one-process fixture the manager's writes had populated the tables the
/// arm seeds from, so the state is set explicitly); ONE served ship later
/// `ewma_handover_ns` is non-zero, `ewma_ship_ns` carries the measured
/// ship, and `n_floor()` is `max(2, ceil(ewma_handover / ewma_ship))` over
/// them. RED on the base: `ewma_handover_ns` 0 and `n_floor()` 2 for the
/// mount's life.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joined_holders_n_floor_is_seeded_at_its_first_served_ship() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "a")]).await;
    let a = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let holder = join(&uris, &venue, &mvol, 1).await;
    let hvol = Arc::clone(&holder.volumes[0]);
    let _ = create_files(&holder, a, "own", 2).await;
    let hplane = Arc::clone(hvol.slot_leases().expect("armed"));
    // The fleet's cold arm made explicit: a joined DAEMON arms with the
    // process-global tables empty (no device write, no S8 ship yet), so
    // both EWMAs read 0 at its arm. In this one-process fixture the
    // manager's writes populated the tables before the join, so the arm
    // seeded them — set the joiner's plane back to the cold state the
    // fleet's joiner is in.
    hplane.ewma_handover_ns.store(0, Relaxed);
    hplane.ewma_ship_ns.store(0, Relaxed);
    assert_eq!(hplane.n_floor(), 2, "the absolute floor before any ship");
    let requester = join(&uris, &venue, &mvol, 2).await;
    let rvol = Arc::clone(&requester.volumes[0]);
    let hvenue = DaemonVenue::stand_up(&holder, false, "joiner-1").await;
    rvol.slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &hvenue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&requester),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let ships0 = hplane.ships.load(Relaxed);
    let _ = create_files(&requester, a, "one", 1).await;
    assert_eq!(
        hplane.ships.load(Relaxed),
        ships0 + 1,
        "one served ship noted"
    );
    let h = hplane.ewma_handover_ns.load(Relaxed);
    let s = hplane.ewma_ship_ns.load(Relaxed);
    assert_ne!(h, 0, "ewma_handover_ns is seeded at the first served ship");
    assert_ne!(s, 0, "ewma_ship_ns carries the measured ship");
    assert_eq!(
        hplane.n_floor(),
        squeezefs::slot_lease_core::n_floor(h, s),
        "N_floor is the measured handover over the measured ship"
    );
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&requester).await;
    drop(rvol);
    shutdown(&holder).await;
    drop(hvol);
    hvenue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 13 (PR 13; review round 1, Issue 10 — the pin the fix
/// lacked): a ship into a STRIPED directory or one of its stripes feeds
/// NO dominance window.** Gate 3b's row: a stripe the manager supplied
/// moved to the first requester whose few ships beat the manager's own
/// few into that 1/K shard — a legal verdict of the law over a slot the
/// striping already spread K ways (§5.6.5's split: ONE dominating creator
/// is a handover candidate, MANY are a striping one). `is_striping_domain`
/// exempts the served ship. Pinned: joiner 1 stripes its directory (the
/// stripes minted in its rotor), joiner 2 ships `N_floor × 16` creates
/// into it by one requester; joiner 1's `offers_idle + offers_dominated`
/// AND its `ships` stay where they were — RED on the base: the ships fed
/// each stripe slot's window and an idle offer fired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ships_into_a_striped_directory_feed_no_dominance_window() {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, shared, "j1", 2).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let j2vol = Arc::clone(&j2.volumes[0]);
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
    j1.stripe_dir(shared, 4).await.expect("the holder's flip");
    let j1plane = Arc::clone(j1.volumes[0].slot_leases().expect("armed"));
    let offers = |p: &squeezefs::meta_backend::kv::slot_lease::SlotLeasePlane| {
        p.offers_idle.load(Relaxed) + p.offers_dominated.load(Relaxed)
    };
    let offers0 = offers(&j1plane);
    let ships0 = j1plane.ships.load(Relaxed);
    let burst = usize::try_from(j1plane.n_floor().max(2) * 16).unwrap();
    for k in 0..burst {
        let name = format!("striped-{k}");
        j2.create(shared, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap_or_else(|e| {
                panic!("a foreign create into the striped directory ({name}): {e}")
            });
        assert!(
            j2.stripe_route(shared, &name)
                .await
                .expect("route")
                .is_some(),
            "{name} routed to a stripe"
        );
    }
    assert_eq!(
        offers(&j1plane),
        offers0,
        "ships into a striped directory never earn an offer (burst {burst})"
    );
    assert_eq!(
        j1plane.ships.load(Relaxed),
        ships0,
        "ships into a striping domain feed no window"
    );
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    drop(j2vol);
    shutdown(&j2).await;
    shutdown(&j1).await;
    j1venue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **A creator whose projection names a slot's OLD holder re-resolves and
/// retries, never surfaces EAGAIN** (PR 13 — the fleet's shared-directory
/// row on the defect-9 binary: a stripe's slot moved to a dominating
/// requester between two of m61's creates; the holder answered its
/// travelling `XvGuards` "this mount does not lease the slot — the
/// initiator re-resolves through tree 0", and nothing re-resolved: the
/// refusal reached the application as EAGAIN, 52 of 2,500 creates). The
/// re-resolve is `reresolve_slot_holder` (one wire `ResolveSlot`, its
/// answer learnt into the projection) around the acquisition, counted on
/// `xv_cross_owner_guard_stale_reresolves`. Pinned: joiner 2's projection
/// names the MANAGER for a directory's slot; the manager hands the slot to
/// joiner 1 (an in-process accept: release + grant); joiner 2's next create
/// into it lands, served at joiner 1, ONE re-resolve counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_holder_view_at_the_guards_re_resolves_and_lands_the_create() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "d")]).await;
    let d = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager first-touches the directory's slot.
    let _ = create_files(&manager, d, "m", 2).await;
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    // Both joiners' projections name the manager as the slot's holder.
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 0, .. }
        ),
        "the premise: joiner 2's projection names the manager"
    );
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(1, &j1venue.endpoint);
    let mvenue_ep = venue.endpoint();
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(0, &mvenue_ep);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    // The slot MOVES to joiner 1 (its wire first touch is refused while
    // the manager holds it; an explicit offer + accept moves it — the
    // manager's in-process release + grant under one mutex hold).
    mvol.manager_offer_slot(0, SLOT_A, 1)
        .await
        .expect("the manager offers its slot to joiner 1");
    let routing_a = squeezefs::meta_backend::kv::appender::page_slot_of_forest_slot(
        SLOT_A,
        mvol.appender_stats().unwrap().native_slot,
    )
    .unwrap();
    let (_, accepted) = j1.act_on_slot_carriage(&[], &[(routing_a, 0)]).await;
    assert_eq!(accepted, 1, "joiner 1 holds the slot now");
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 0, .. }
        ),
        "joiner 2's projection is STALE — it still names the manager"
    );
    let stale0 = squeezefs::meta_backend::crossvol_tx::cross_owner_stats().guard_stale_reresolves;
    let child = j2
        .create(d, "after-the-move", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the create lands after ONE re-resolve, never EAGAIN")
        .ino;
    assert_eq!(
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().guard_stale_reresolves,
        stale0 + 1,
        "exactly one stale-holder re-resolve"
    );
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 1, .. }
        ),
        "joiner 2's projection learnt the new holder"
    );
    assert_eq!(
        j1.lookup_dentry_exact_unguarded(d, "after-the-move")
            .await
            .expect("the new holder reads its tree")
            .map(|(i, _)| i),
        Some(child)
    );
    shutdown(&j2).await;
    shutdown(&j1).await;
    j1venue.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 30 (PR 13; PR 6/12b's local arm of defect 29)** — found by the
/// fleet's `sym-storm` round 2 from zero: a joiner's `mkdir` into the
/// striped `/` read the name's stripe slot UNLEASED in its projection (the
/// previous lessee had just LRU-released it), took the LOCAL create path,
/// and its commit door's wire first touch lost to the manager, which had
/// first-touched the slot a moment earlier; the door surfaced the
/// manager's `SlotRefused { holder: 0 }` as `EAGAIN` to `mkdir(2)` — the
/// round died. The schedule here is the same and legal: the joiner's
/// projection reads the slot unleased (it was released before the join),
/// the manager first-touches it, the joiner's create is dispatched locally.
/// RED before: `EAGAIN "forest slot 4 is leased by appender 0"`. GREEN: the
/// door's refusal LEARNT the holder, the op re-ran ONCE through the
/// cross-owner arm (`xv_cross_owner_op_slot_moved_redispatches` + 1) and
/// the child resolves at the manager.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locally_dispatched_create_whose_slot_the_manager_took_is_redispatched_through_the_cross_owner_arm(
) {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "d")]).await;
    let d = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Unleased { .. }
        ),
        "the premise: the joiner's projection reads the directory's slot UNLEASED"
    );
    // The manager first-touches the slot AFTER the joiner's projection was
    // loaded (a tree-0 control entry in ring 0 the joiner has not read).
    let _ = create_files(&manager, d, "m", 1).await;
    assert!(
        matches!(
            mvol.slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 0, .. }
        ),
        "the manager leases the slot now"
    );
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Unleased { .. }
        ),
        "the joiner's projection is STALE — still unleased"
    );
    let mvenue_ep = venue.endpoint();
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(0, &mvenue_ep);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let redispatches0 =
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().op_slot_moved_redispatches;
    let child = j2
        .create(d, "after-the-touch", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the create lands through the cross-owner arm, never EAGAIN")
        .ino;
    assert_eq!(
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().op_slot_moved_redispatches,
        redispatches0 + 1,
        "exactly one re-dispatch after the door's SlotBusy"
    );
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 0, .. }
        ),
        "the door's wire refusal taught the joiner the holder"
    );
    assert_eq!(
        manager
            .lookup_dentry_exact_unguarded(d, "after-the-touch")
            .await
            .expect("the holder reads its tree")
            .map(|(i, _)| i),
        Some(child),
        "the child was inserted at the manager (the slot's holder)"
    );
    // The mkdir shape of the fleet's failure — a directory into the same
    // (now foreign) slot — lands on the first run: the projection knows.
    j2.create(d, "sub", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("a mkdir into the foreign directory ships on its first run");
    assert_eq!(
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().op_slot_moved_redispatches,
        redispatches0 + 1,
        "no second re-dispatch: the learnt holder routes the next op"
    );
    shutdown(&j2).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 35 (PR 13; PR 6 under PR 12b — defect 29/30's MID-PLAN arm)**
/// — found by the fleet's `sym-storm` round 4 from zero on `7ac89d24`: a
/// joiner's cross-owner plan (a rename into the manager's directory) had
/// its step 1 dispatched LOCALLY — the destination's slot read UNLEASED in
/// its projection — and the door's wire first touch lost to another
/// appender (`forest slot 10 is leased by appender 7 (g 3)`); defect 29's
/// classifier read the LOCAL dispatch as a device error and the S3.5
/// lattice FAIL-STOPPED both volumes (`crossvol_tx_midplan_escalations`;
/// every later op `Metadata volume 1 is disabled`, the round's explicit
/// stripe flip refused). The door refuses before any effect, so a local
/// step's `SlotBusy` is the slot-moved class exactly like a shipped
/// step's: re-resolved and re-dispatched (it ships now), and past the
/// bound the retryable class (the intent stays open) — never the lattice.
/// Here: the joiner's projection reads the destination slot unleased, the
/// manager first-touches it, the joiner renames across. RED before: the
/// rename `EAGAIN` and BOTH volumes disabled at the joiner. GREEN: the
/// rename lands at the manager after one step re-dispatch; nothing is
/// disabled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_steps_slot_busy_mid_plan_redispatches_and_never_fail_stops_the_initiator() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "src"), (SLOT_B, "dst")]).await;
    let (src, dst) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager holds the SOURCE (first touch) before the joiner reads
    // tree 0; the destination stays unleased in the joiner's projection.
    let files = create_files(&manager, src, "f", 1).await;
    let f = files[0].1;
    mvol.checkpoint_now()
        .await
        .expect("tree 0 names the source lease");
    let j2 = join(&uris, &venue, &mvol, 2).await;
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 0, .. }
        ),
        "the premise: the joiner knows the manager holds the source"
    );
    assert!(
        matches!(
            j2.volumes[0].slot_leases().unwrap().table.resolve(SLOT_B),
            squeezefs::slot_lease_core::Resolved::Unleased { .. }
        ),
        "the premise: the destination reads unleased at the joiner"
    );
    // The manager first-touches the destination AFTER the joiner's
    // projection was loaded.
    let _ = create_files(&manager, dst, "m", 1).await;
    let mvenue_ep = venue.endpoint();
    j2.volumes[0]
        .slot_leases()
        .expect("armed")
        .holders
        .set_endpoint(0, &mvenue_ep);
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            "node-j2",
            VENUE_SECRET.to_vec(),
        ),
    );
    let retries0 =
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().step_slot_moved_retries;
    let escalations0 = squeezefs::meta_backend::crossvol_tx::XV_MIDPLAN_ESCALATIONS
        .load(std::sync::atomic::Ordering::Relaxed);
    j2.rename(src, &files[0].0, dst, "g", 0)
        .await
        .expect("the rename lands: its local step re-dispatches to the holder, never EAGAIN");
    assert_eq!(
        squeezefs::meta_backend::crossvol_tx::XV_MIDPLAN_ESCALATIONS
            .load(std::sync::atomic::Ordering::Relaxed),
        escalations0,
        "no mid-plan escalation — the lattice never fires on a moved slot"
    );
    assert!(
        squeezefs::meta_backend::crossvol_tx::cross_owner_stats().step_slot_moved_retries
            > retries0,
        "the local step was re-dispatched after the door's SlotBusy"
    );
    assert_eq!(
        manager
            .lookup_dentry_exact_unguarded(dst, "g")
            .await
            .expect("the holder reads its tree")
            .map(|(i, _)| i),
        Some(f),
        "the name landed in the destination at its holder"
    );
    assert!(
        manager
            .lookup_dentry_exact_unguarded(src, &files[0].0)
            .await
            .expect("the holder reads its tree")
            .is_none(),
        "the source name is gone"
    );
    // Nothing fail-stopped: the joiner still writes.
    j2.create(1, "alive", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's volumes are not disabled");
    shutdown(&j2).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 31 (PR 13; PR M6's pending-times drain under PR 4's slot
/// leases)** — found beside defect 30 on the same fleet round: the
/// manager's `kv pending-times drain failed … forest slot 474 is leased by
/// appender 3` at every tick, and every `fsync` of the manager's OWN
/// files answered `EAGAIN` — the drain commits every parked refinement
/// of the volume as ONE transaction, and one ino whose slot had moved to
/// another appender refused the whole batch at the door, for ever (the
/// map is RAM: only a remount emptied it). Two laws: (a) a slot's parked
/// refinements are DRAINED before its release (the flush-then-transfer's
/// step 0 — while the door still admits them; RED before: the refinement
/// outlived the lease, `pending_times_len() == 1` after the release); (b)
/// a refinement whose slot another appender leases at the drain is
/// DROPPED and counted (`meta_kv_times_echo_foreign_dropped`), and the
/// own refinements beside it commit (RED before: `Err(SlotBusy)`, the own
/// refinement never durable).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_slots_pending_times_are_drained_first_and_a_foreign_slots_are_dropped_not_wedged(
) {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "d")]).await;
    let d = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager first-touches the slot and parks a refinement on the
    // DIRECTORY (an object in the slot by construction — a child may mint
    // into the rotor; the SETATTR-echo absorber's park, a second past the
    // record).
    let _ = create_files(&manager, d, "m", 1).await;
    let f = d;
    let local_f = manager.route_ino(f).1;
    assert_eq!(
        squeezefs::meta_backend::kv::record::forest_slot_of_ino(local_f),
        SLOT_A,
        "the premise: the refined object lives in the released slot"
    );
    let base = manager.getattr(f).await.expect("the record").mtime;
    let later = base + 1_000_000_000;
    mvol.park_times_refinement(local_f, later, later);
    assert_eq!(
        mvol.pending_times_len(),
        1,
        "the premise: one parked refinement"
    );
    let dropped0 =
        squeezefs::meta_backend::kv::META_KV_TIMES_ECHO_FOREIGN_DROPPED.load(Ordering::Relaxed);
    // (a) The release drains the slot's refinement FIRST.
    mvol.release_slot_handover(0, SLOT_A)
        .await
        .expect("the manager releases the slot to unleased");
    assert_eq!(
        mvol.pending_times_len(),
        0,
        "the slot's refinement was drained before the release, not left behind"
    );
    assert_eq!(
        manager.getattr(f).await.expect("the record").mtime,
        later,
        "the refinement is DURABLE (the map is empty; the record carries it)"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_TIMES_ECHO_FOREIGN_DROPPED.load(Ordering::Relaxed),
        dropped0,
        "nothing was dropped on the ordinary path"
    );
    // (b) A joiner first-touches the slot; the manager parks a STALE
    // refinement on the file (a times-only echo through its image of the
    // tree) beside a refinement on its OWN file.
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let _ = create_files(&j1, d, "j", 1).await;
    assert!(
        matches!(
            mvol.slot_leases().unwrap().table.resolve(SLOT_A),
            squeezefs::slot_lease_core::Resolved::Holder { holder: 1, .. }
        ),
        "the joiner leases the slot now"
    );
    let own = manager
        .create(1, "own", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the manager's own file")
        .ino;
    let own_base = manager.getattr(own).await.expect("the record").mtime;
    let own_later = own_base + 2_000_000_000;
    mvol.park_times_refinement(manager.route_ino(own).1, own_later, own_later);
    mvol.park_times_refinement(local_f, later + 1_000_000_000, later + 1_000_000_000);
    assert_eq!(mvol.pending_times_len(), 2);
    let drained = mvol
        .drain_pending_times_now()
        .await
        .expect("the drain commits the own refinement and drops the foreign one — never SlotBusy");
    assert_eq!(drained, 1, "exactly the own refinement was made durable");
    assert_eq!(
        mvol.pending_times_len(),
        0,
        "the map is empty after the drain"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_TIMES_ECHO_FOREIGN_DROPPED.load(Ordering::Relaxed),
        dropped0 + 1,
        "the foreign slot's refinement was dropped, counted"
    );
    assert_eq!(
        manager.getattr(own).await.expect("the record").mtime,
        own_later,
        "the own refinement is durable"
    );
    // The fsync path's drain is the same function: a second drain with an
    // empty map is a no-op that touches no gate.
    assert_eq!(
        mvol.drain_pending_times_now().await.expect("empty drain"),
        0
    );
    shutdown(&j1).await;
    venue.tear_down();
    shutdown(&manager).await;
}

/// **Defect 33 (PR 13; PR 2's KD-SYM-10 audit × PR 10's recovery)** — a
/// dead appender's recovery holds the volume's SMO mutex through its
/// per-region steps 4–7, and the flush pass that covers a MANAGER leaf
/// dirty at that instant waits it out; the recovery's own published bound
/// (`appender_recovery_bound_ms`, 0.2–1.2 s) exceeds the landing
/// ceiling's 2-tick margin by design, so every recovery that met a dirty
/// manager leaf tripped `appender_flush_ceiling_overruns` (must-stay-0) —
/// the fleet's `sym-storm` round 4 from zero read the manager's leaf at
/// 1,101 ms against 1,100 during a seven-region recovery. The audit now
/// judges a leaf whose dirty window a recovery hold overlapped against
/// `ceiling + appender_recovery_bound_ms` (both published) and counts it
/// on `appender_flush_ceiling_recovery_extensions`; past THAT it is still
/// an overrun. Here: a joiner dies with acked records, the manager's
/// recovery parks under its hold (`TEST_RECOVERY_HOLD_BEFORE_TREE0`), the
/// manager commits into its own tree (a leaf dirty under the hold), the
/// leaf ages 200 ms past the 1,100 ms ceiling (the aging IS the measured
/// quantity — the one sleep the contract takes), the hold releases and
/// the next cycle covers it. RED before: `flush_ceiling_overruns == 1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_manager_leaf_that_aged_under_a_recoverys_hold_is_a_counted_extension_not_an_overrun() {
    use squeezefs::meta_backend::kv::backend::recovery;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager's own directory, flushed clean before the recovery so
    // the leaf's next dirtying is the one under the hold.
    let mine = manager
        .create(1, "mine", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the manager's directory")
        .ino;
    let _ = create_files(&manager, mine, "before", 1).await;
    // The joiner writes into the shared slot and dies.
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let identity = jvol.joined_wire().unwrap().identity;
    let _ = create_files(&joiner, shared, "x", 8).await;
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    mvol.checkpoint_now()
        .await
        .expect("clean before the recovery");
    let s0 = mvol.appender_stats().unwrap();
    assert_eq!(s0.flush_ceiling_overruns, 0, "the premise");
    assert_eq!(s0.flush_ceiling_recovery_extensions, 0, "the premise");
    let ceiling_ms = s0.flush_ceiling_ms;
    let bound_ms = mvol.appender_recovery_bound_ms();
    assert!(
        ceiling_ms <= 2_000,
        "the contract runs at the shipped cadence (ceiling {ceiling_ms} ms)"
    );
    mvol.record_death_with_key(identity, 9, 0)
        .await
        .expect("the death record");
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(true, Ordering::SeqCst);
    let m2 = Arc::clone(&manager);
    let poll = tokio::spawn(async move { recover_dead_appenders_set(&m2).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !recovery::TEST_RECOVERY_HELD.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the recovery never parked"
        );
        tokio::task::yield_now().await;
    }
    // A manager commit under the hold: admitted (its own slot), its leaf
    // dirty from now — unflushable while the recovery holds the mutex.
    manager
        .create(mine, "under-the-hold", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("an own-slot create commits while a recovery holds the mutex");
    let aged_ms = ceiling_ms + 200;
    assert!(
        aged_ms <= ceiling_ms + bound_ms,
        "the aging sits inside the extended bound (ceiling {ceiling_ms} + bound {bound_ms} ms)"
    );
    tokio::time::sleep(std::time::Duration::from_millis(aged_ms)).await;
    recovery::TEST_RECOVERY_HOLD_BEFORE_TREE0.store(false, Ordering::SeqCst);
    recovery::TEST_RECOVERY_HOLD_RELEASE.notify_waiters();
    let rep = poll.await.unwrap().unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    // The covering cycle: the manager's leaf lands past the ceiling and
    // inside the extended bound.
    mvol.checkpoint_now().await.expect("the covering cycle");
    let s1 = mvol.appender_stats().unwrap();
    assert_eq!(
        s1.flush_ceiling_overruns, 0,
        "a leaf that aged under a recovery's hold is not an overrun (ceiling {ceiling_ms} ms, \
         recovery bound {bound_ms} ms, aged ≥ {aged_ms} ms)"
    );
    assert!(
        s1.flush_ceiling_recovery_extensions >= 1,
        "the extension is counted (got {})",
        s1.flush_ceiling_recovery_extensions
    );
    manager
        .lookup(mine, "under-the-hold")
        .await
        .expect("the commit under the hold is served");
    venue.tear_down();
    shutdown(&manager).await;
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
    // The recovery's tree-0 release is a ring-0 control entry; the LEDGER
    // names the moved root only at the manager's next cycle, and the
    // third's projection reads the ledger — cycle it first, or the
    // projection races the cadence and names the dead lessee (a
    // suite-order flake: 1 in 4 full runs).
    mvol.checkpoint_now().await.unwrap();
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

/// **The cadence PARKED for the page-budget pins** (the crash matrix's
/// fixture, `SQUEEZEFS_META_FLUSH_INTERVAL_MS=60000` read by every
/// checkpoint task spawned while it stands): a pin that reads a slot's
/// root and asserts it again later — "the appends left the root where the
/// page named it", "the victim's root did not move on its own", tree 0
/// naming the root a compaction recorded — presumes NO cycle ran in
/// between; with the live cadence a tick's flush pass (a split of the
/// loaded leaf, the maintenance arms) could move it early under a slower
/// box, and the premise read the moved root (the batch `task check` on
/// `13a009dd`: the root-split pin red at its premise, 12/12 green alone).
/// Every cycle of these pins is one the pin runs (`checkpoint_now`, or
/// the one it parks and releases). The successor's open inherits the
/// posture; its assertions are direct reads. Dropped at the pin's end.
struct ParkedCadence;

impl ParkedCadence {
    fn arm() -> Self {
        std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
        Self
    }
}

impl Drop for ParkedCadence {
    fn drop(&mut self) {
        std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
    }
}

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
        report.counters.c1_projection_slots_scoped >= slots.len() as u64,
        "every slot tree the live joiner leases was skipped by the raw C1 walk ({} ≥ {})",
        report.counters.c1_projection_slots_scoped,
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

/// **A member's census shard walks only its OWN slot trees** (PR 12b
/// review round 2, Issue 24): KD-MW-16's fleet fsck dispatches a census
/// shard to a MEMBER — the `-o ro` reader in every fleet fsck, a joined
/// writer where one is idle — which runs `fsck::run` over its own view
/// and whose C1 findings the coordinator admits whole. Round 4's law
/// ("a slot tree a LIVE lessee holds is not censused here") read ONE
/// owner's word — the process's installed S6 owner — and a member has
/// none, so its raw C1 walk covered every slot tree it holds as a
/// PROJECTION: a tree another appender leases at the root tree 0 named
/// when the lease was granted, appended into and re-rooted since (F12's
/// `C1Torn` shape — clean in the round-4 tapes only because the reader's
/// roots happened not to move under its walk). ONE predicate now
/// (`SlotCoverage::unjudged_slots`, read by the dentry pass and the raw
/// C1 walk alike): at the manager the live lessees' trees; at a member
/// every tree not leased by it — no liveness word exists there, so
/// "leased to anyone else (or unleased — the manager's)" is its honest
/// bound. Coverage INCOMPLETE over them, counted, never a finding.
///
/// The shape: the joiner joined at the seeded roots of two unleased
/// slots; the MANAGER then first-touches both, grows and compacts their
/// trees (the roots move, the retired extents return and are re-claimed
/// under other images) while the joiner's projection stands at the
/// seeded roots; the joiner runs a census shard. RED before: both of the
/// manager's trees WALKED by the member (0 skipped — the false finding
/// itself needs the retired root extents re-claimed under other images,
/// which the fleet's storm supplies and this shape only sometimes does);
/// green: 0 findings, both trees counted as projections skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_members_census_shard_walks_only_its_own_slot_trees() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let (shared, other) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;
    let joiner = join(&uris, &venue, &mvol, 93).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    // The joiner holds both seeded trees as projections at their seeded
    // roots (tree 0 named them at its open).
    assert!(jvol.slot_tree(SLOT_A).is_some() && jvol.slot_tree(SLOT_B).is_some());

    // The manager first-touches both slots and re-roots their trees,
    // three compactions apart, the retired extents returned and re-used.
    let mut files = Vec::new();
    for round in 0..3 {
        files.extend(create_files(&manager, shared, &format!("ma{round}-"), 60).await);
        files.extend(create_files(&manager, other, &format!("mb{round}-"), 60).await);
        mvol.checkpoint_now().await.unwrap();
        for slot in [SLOT_A, SLOT_B] {
            let root = mvol.slot_tree(slot).expect("the manager's tree").root();
            assert_eq!(
                mvol.defrag_compact_nodes(&[(0, root.addr)]).await.unwrap(),
                1,
                "slot {slot}'s root compacts"
            );
        }
        mvol.checkpoint_now().await.unwrap();
        mvol.checkpoint_now().await.unwrap();
    }
    for slot in [SLOT_A, SLOT_B] {
        assert!(
            matches!(
                tree0_state(&mvol, slot).await,
                Some(SlotState::Leased { appender_id: 0, .. })
            ),
            "slot {slot} is the manager's"
        );
    }

    // The MEMBER's census shard (the fleet worker's shape: the census
    // partition, no inode plane) over its own view.
    let report = census_shard_over(&joiner).await;
    assert!(
        !report.has_findings(),
        "a member's census takes no verdict over the trees it holds as projections: {:?}",
        report.findings
    );
    assert!(
        report.counters.c1_projection_slots_scoped >= 2,
        "both of the manager's trees were skipped as projections ({} ≥ 2)",
        report.counters.c1_projection_slots_scoped
    );
    // The lessee's own census judges them — and finds them healthy.
    let mine = fsck_all_classes_over(&manager).await;
    assert!(!mine.has_findings(), "{:?}", mine.findings);

    // The S5 `-o ro` READER — the fleet's actual census-shard venue
    // (`mw_fleet.sh` mounts its reader without the plane: no token client,
    // no lease, every slot tree a poll-refreshed projection). Round 5's
    // first fleet run found it judged EVERY tree as its own (the member
    // predicate named joined appenders and token readers only): a
    // non-writer that is not an offline probe holds nothing and judges
    // nothing.
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("an S5 reader opens beside the live writers");
    let report = census_shard_over(&reader).await;
    assert!(
        !report.has_findings(),
        "an S5 reader's census takes no verdict over the writers' trees: {:?}",
        report.findings
    );
    assert!(
        report.counters.c1_projection_slots_scoped >= 2,
        "the S5 reader skipped both writers' trees as projections ({} ≥ 2)",
        report.counters.c1_projection_slots_scoped
    );
    shutdown(&reader).await;
    drop(reader);

    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    assert_all_resolve(&manager, shared, &files[..60]).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// KD-MW-16's fleet CENSUS shard as a member runs it (`fleet_worker.rs`):
/// the census partition, no inode plane, over the member's own view.
async fn census_shard_over(routed: &Arc<RoutedMetaBackend>) -> squeezefs::fsck::FsckReport {
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-shard")
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
    opts.shard = Some((1, 2));
    opts.staging_full = true;
    opts.inode_plane = false;
    squeezefs::fsck::run(&ctx, &opts)
        .await
        .expect("the engine runs")
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
/// steps AND record-level verbs served on ITS backend — PR 13b), the S9
/// publish service judging by ITS custody owner (a shipped layout
/// publish, PR 13b), the token service over ITS holder planes; the
/// manager verbs on the MANAGER's alone.
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
        use squeezefs::meta_ship::publish::PublishService;
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
            .with_publish(PublishService::with_custody_owner(
                Arc::clone(routed),
                Arc::clone(&owner),
            ))
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

/// The record-ship ledger (`meta_ship.record_*` / `foreign_publish_*`).
fn record_ship_stats() -> squeezefs::meta_backend::record_ship::RecordShipStats {
    squeezefs::meta_backend::record_ship::stats()
}

/// The two halves of PR 13b's in-process WRITER: PR 9's custody arm over
/// `writer`'s set (the per-holder custody lease a shipped publish rides)
/// and the S9 publish client (process-global, as on the mount path —
/// `arm_authority_planes` installs it on every writer). The arm's identity
/// is `writer`'s KD-MW-2 member id, the join's word at every holder.
async fn arm_publish_writer(
    writer: &Arc<RoutedMetaBackend>,
    identity: &AppenderIdentity,
) -> Arc<squeezefs::data_grant::SlotCustodyArm> {
    let member = squeezefs::cowriter::node_member_id_of(identity.node_token, identity.mount_slot);
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let arm = squeezefs::data_grant::arm_slot_custody(
        writer,
        &member,
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    squeezefs::meta_ship::publish::install_client(
        squeezefs::meta_ship::publish::PublishClient::new(&member, VENUE_SECRET.to_vec()),
    );
    arm
}

async fn disarm_publish_writer() {
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_ship::publish::uninstall_client();
}

/// The DATA face's layout: one striped 4 MiB block on the data volume the
/// fixtures name, and its durable reference (the C8 ledger rides the same
/// tx as the layout it justifies).
fn striped_layout_for(
    ino: u64,
) -> (
    Vec<u8>,
    Vec<squeezefs::meta_backend::kv::block_refs::BlockRefOp>,
) {
    use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
    let tag = volume_tag("vol-00000000000000a7");
    let mut map = std::collections::HashMap::new();
    map.insert(0u32, "vol-00000000000000a7://0".to_string());
    let layout = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".into(),
        size: 4 * 1024 * 1024,
        block_map: Some(map),
        ..Default::default()
    })
    .unwrap();
    let refs = vec![BlockRefOp::taken(BlockRef {
        vol_tag: tag,
        block_idx: 0,
        owner_ino: ino,
        block_index: 0,
    })];
    (layout, refs)
}

/// **PR 13b — the record-level metanode ship (defect 32, PR 13's first
/// flip blocker, record §4.4z; design §5.10's "`write` to a FOREIGN-owned
/// file" row).** A JOINED appender mutates a file the MANAGER's slot holds
/// — `setattr` (a `chmod`'s mode + ctime, a `chown`'s uid/gid + ctime, a
/// `touch`'s mtime, a truncate's size), `setxattr` / `removexattr`, and the
/// DATA face (the layout publish through the daemon's publish funnel,
/// `meta_ship::publish::set_layout_and_size` — a `write` + `fsync` at the
/// routed layer) — and every verb LANDS at the holder: shipped as the S8
/// `MetaCall` it already is (`record_ship::ship_record_verb`) / the S9
/// publish plane re-keyed by SLOT HOLDER with the per-holder custody lease
/// PR 9's arm dialed (`publish_target`), applied under the holder's lease
/// and door, its tokens recalled by the holder's commit. RED on the base
/// (`5bd47813`): every verb answered PR 13's interim `EREMOTE`
/// (`ForeignSlotFileMutation`). The write-intent OPEN passes (the write
/// path ships); the ledger closes (`record_ships ≡ record_served`,
/// `record_refusals == 0`, `foreign_publish_ships ≡
/// foreign_publish_served`); the kernel's ctime-only times echo stays
/// absorbed against the holder's record (never a wire trip for a no-op);
/// an OWN file's verbs stay local (the ledger unmoved).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_setattr_of_the_managers_file_lands_at_the_holder() {
    use squeezefs::meta_backend::record_ship::ServedMutation;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-13b").await;
    let f = manager
        .create(shared, "m", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    mvol.checkpoint_now().await.unwrap();
    let j = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 61).await),
                identity: joiner_identity(&mvol, 61).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&j.volumes[0]);
    let jidentity = jvol.joined_wire().unwrap().identity;
    // The joiner's step shipper — the ladder's rung 7 (keyed by its member
    // id so the served verb's requester is known to the holder).
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j),
            &peer_of(&jidentity),
            VENUE_SECRET.to_vec(),
        ),
    );
    let _arm = arm_publish_writer(&j, &jidentity).await;
    // The HOLDER's served-mutation sink (the FUSE layer's hook — the
    // `sym-foreign-file` leg's first run read the holder's colleague's
    // append as the old bytes for the mount's life: the served verb lands
    // at the KV below the daemon's own caches): every served record verb
    // and every served publish names the object and its scope here.
    let served_sink: Arc<std::sync::Mutex<Vec<(u64, ServedMutation)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = Arc::clone(&served_sink);
        squeezefs::meta_backend::record_ship::install_served_mutation_sink(Arc::new(
            move |ino, kind| {
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    seen.lock().unwrap().push((ino, kind));
                })
            },
        ));
    }
    let s0 = record_ship_stats();
    let echoes0 = squeezefs::meta_backend::FOREIGN_FILE_TIMES_ECHO_ABSORBED.load(Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // chmod: mode + ctime (the writeback cache's `trust_local_cmtime`).
    let got = j
        .setattr(
            f,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            Some(now_ns),
        )
        .await
        .expect("PR 13b: a foreign-slot file's chmod ships to its slot holder");
    assert_eq!(got.ino, f, "the reply names the global ino");
    assert_eq!(got.mode, libc::S_IFREG | 0o600);
    assert_eq!(
        manager.getattr(f).await.unwrap().mode,
        libc::S_IFREG | 0o600,
        "the holder's record carries the mode"
    );
    // chown: uid/gid + ctime.
    j.setattr(f, None, Some(0), Some(0), None, None, None, Some(now_ns))
        .await
        .expect("PR 13b: a foreign-slot file's chown ships");
    let at_holder = manager.getattr(f).await.unwrap();
    assert_eq!((at_holder.uid, at_holder.gid), (0, 0));
    // touch: an mtime the record does not carry.
    j.setattr(
        f,
        None,
        None,
        None,
        None,
        None,
        Some(at_holder.mtime + 1_000_000_000),
        Some(now_ns),
    )
    .await
    .expect("PR 13b: a foreign-slot file's touch ships");
    assert_eq!(
        manager.getattr(f).await.unwrap().mtime,
        at_holder.mtime + 1_000_000_000
    );
    // truncate: a size.
    j.setattr(f, None, None, None, Some(4096), None, None, Some(now_ns))
        .await
        .expect("PR 13b: a foreign-slot file's truncate ships");
    assert_eq!(manager.getattr(f).await.unwrap().size, 4096);
    // The xattr faces.
    j.setxattr(f, "user.pr13b", b"shipped")
        .await
        .expect("PR 13b: a foreign-slot file's setxattr ships to its slot holder");
    assert_eq!(
        manager.getxattr(f, "user.pr13b").await.unwrap().as_deref(),
        Some(&b"shipped"[..]),
        "the holder reads the shipped xattr"
    );
    j.removexattr(f, "user.pr13b")
        .await
        .expect("PR 13b: a foreign-slot file's removexattr ships");
    assert_eq!(manager.getxattr(f, "user.pr13b").await.unwrap(), None);
    let s1 = record_ship_stats();
    assert_eq!(
        s1.record_ships - s0.record_ships,
        6,
        "six record-level verbs shipped"
    );
    assert_eq!(
        s1.record_served - s0.record_served,
        6,
        "the holder applied every one (record_ships ≡ record_served)"
    );
    assert_eq!(s1.record_refusals, s0.record_refusals, "must stay 0");
    assert_eq!(s1.record_unreachable, s0.record_unreachable);
    assert_eq!(s1.record_ship_redirects, s0.record_ship_redirects);
    assert_eq!(
        served_sink.lock().unwrap().as_slice(),
        &[(f, ServedMutation::Attrs); 6][..],
        "the holder's sink saw six attrs-class served mutations of the object"
    );

    // The write-intent OPEN passes: the holder is reachable, the write
    // path ships its publish there (PR 13's interim gate refused here).
    for flags in [
        libc::O_WRONLY,
        libc::O_RDWR,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
        libc::O_RDWR | libc::O_TRUNC,
    ] {
        j.refuse_foreign_slot_open(f, flags as u32)
            .await
            .unwrap_or_else(|e| panic!("a write-intent open ({flags:#o}) passes: {e}"));
    }
    // The DATA face: the joiner's write + fsync is a layout publish
    // through the daemon's publish funnel — shipped to the slot holder
    // under the per-holder custody lease PR 9's arm dialed, applied under
    // the holder's lease; the holder's record reads the new size and
    // layout.
    let (layout, refs) = striped_layout_for(f);
    let verdict =
        squeezefs::meta_ship::publish::set_layout_and_size(&j, f, &layout, 4 * 1024 * 1024, &refs)
            .await
            .expect("PR 13b: a foreign-slot file's layout publish ships to its slot holder");
    assert!(
        !verdict.recomputed,
        "whole-file custody: the shipped layout applies verbatim"
    );
    assert_eq!(
        manager.getattr(f).await.unwrap().size,
        4 * 1024 * 1024,
        "the holder reads the published size"
    );
    assert!(
        manager
            .getxattr(f, "layout")
            .await
            .unwrap()
            .is_some_and(|l| l == layout),
        "the holder reads the published layout"
    );
    let s2 = record_ship_stats();
    assert_eq!(s2.foreign_publish_ships - s0.foreign_publish_ships, 1);
    assert_eq!(
        s2.foreign_publish_served - s0.foreign_publish_served,
        1,
        "foreign_publish_ships ≡ foreign_publish_served"
    );
    assert_eq!(
        served_sink.lock().unwrap().last().copied(),
        Some((f, ServedMutation::Data)),
        "the holder's sink saw the served publish as a DATA-class mutation of the object"
    );
    // The joiner's own view is exact at its next resolve (the holder's
    // commit recalled its token; the divert re-fetches).
    assert_eq!(j.getattr(f).await.unwrap().size, 4 * 1024 * 1024);
    // The INLINE face (the fleet's small-file shape — an append of a
    // ≤ 4 KiB file is a layout publish whose record IS the payload): the
    // holder's size and its inline record follow the shipped words.
    let inline = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "inline".into(),
        size: 7,
        data_key: Some(b"abcDEFG".to_vec()),
        ..Default::default()
    })
    .unwrap();
    squeezefs::meta_ship::publish::set_layout_and_size(&j, f, &inline, 7, &[])
        .await
        .expect("PR 13b: an inline layout publish ships to its slot holder");
    assert_eq!(
        manager.getattr(f).await.unwrap().size,
        7,
        "the holder reads the inline publish's size"
    );
    assert!(
        manager
            .getxattr(f, "layout")
            .await
            .unwrap()
            .is_some_and(|l| l == inline),
        "the holder reads the inline record"
    );
    assert_eq!(j.getattr(f).await.unwrap().size, 7);
    let s3 = record_ship_stats();
    assert_eq!(s3.foreign_publish_ships - s0.foreign_publish_ships, 2);
    assert_eq!(s3.foreign_publish_served - s0.foreign_publish_served, 2);
    assert_eq!(s3.foreign_publish_refusals, s0.foreign_publish_refusals);

    // A shipped publish the holder REFUSES (review round 1, Issue 4): an
    // ino that routes to the holder's slot but has no record there. The
    // ship travels and fails terminally — counted on
    // `foreign_publish_refusals`, never on `foreign_publish_ships`, which
    // counts publishes that LANDED (at the terminal reply, once per
    // logical publish) so `ships ≡ served` holds at rest. RED on the
    // first build, which counted the ship at the dispatch: a refusal
    // read as a ship with no served twin.
    let ghost = {
        let (v, local) = j.route_ino(f);
        let width = j.routing_width();
        let routing = squeezefs::meta_backend::kv::record::forest_slot_of_ino(local) - 1;
        // A raw local ino on the same routing slot, far past the cursor:
        // never minted at the holder (`route_ino` re-derives the slot's
        // guest keyspace from the routing slot).
        assert_eq!(v, 0);
        squeezefs::meta_backend::make_global_ino_width(4_000_000_000, u64::from(routing), width)
    };
    let refused =
        squeezefs::meta_ship::publish::set_layout_and_size(&j, ghost, &inline, 7, &[]).await;
    assert!(
        refused.is_err(),
        "a publish of an ino the holder has no record for is refused: {refused:?}"
    );
    let s4 = record_ship_stats();
    assert_eq!(
        s4.foreign_publish_refusals - s3.foreign_publish_refusals,
        1,
        "the refused publish is counted on foreign_publish_refusals"
    );
    assert_eq!(
        s4.foreign_publish_ships, s3.foreign_publish_ships,
        "a refused publish is NOT a ship — foreign_publish_ships ≡ foreign_publish_served"
    );
    assert_eq!(s4.foreign_publish_served, s3.foreign_publish_served);

    // The kernel's ctime-only times ECHO stays absorbed against the
    // holder's record — never shipped (the storm's oracle read 14 k per
    // round).
    let cur = j.getattr(f).await.unwrap();
    let echoed = j
        .setattr(
            f,
            None,
            None,
            None,
            None,
            None,
            Some(cur.mtime),
            Some(now_ns),
        )
        .await
        .expect("the echo is absorbed");
    assert_eq!(echoed.mtime, cur.mtime);
    assert_eq!(
        squeezefs::meta_backend::FOREIGN_FILE_TIMES_ECHO_ABSORBED.load(Relaxed),
        echoes0 + 1
    );
    assert_eq!(
        record_ship_stats().record_ships,
        s2.record_ships,
        "no ship for a no-op"
    );

    // An OWN file's verbs stay local — the ledger does not move.
    let own = j
        .create(1, "own", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's own file")
        .ino;
    j.setattr(
        own,
        Some(libc::S_IFREG | 0o600),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("an own-slot file's setattr lands locally");
    j.setxattr(own, "user.x", b"1")
        .await
        .expect("an own-slot file's setxattr lands locally");
    j.refuse_foreign_slot_open(own, (libc::O_RDWR | libc::O_APPEND) as u32)
        .await
        .expect("an own-slot file's write-intent open passes");
    assert_eq!(record_ship_stats().record_ships, s2.record_ships);
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    disarm_publish_writer().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&j).await;
    drop(jvol);
    drop(j);
    mvenue.tear_down();
    shutdown(&manager).await;
    // No offline fsck here: the DATA face's layout names a data volume this
    // metadata-only fixture has no backend for (fsck's C2 reads it as a
    // lost block by construction); the fleet leg is the data plane's row.
}

/// A `SqueezefsFilesystem` — the FUSE layer — in front of an already-open
/// routed set: the handler-driven write / fsync / read path over a
/// file-backed data volume (no staging: the inline shape needs none).
struct FsFront {
    fs: squeezefs::fuse_client::SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _dev: tempfile::NamedTempFile,
}

async fn fs_in_front_of(routed: &Arc<RoutedMetaBackend>, vol_id: &str) -> FsFront {
    let dev_file = tempfile::NamedTempFile::new().unwrap();
    dev_file.as_file().set_len(64 * 1024 * 1024).unwrap();
    let dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        dev_file.path().to_str().unwrap(),
    ));
    let alloc = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(vol_id)
            .await
            .unwrap(),
    );
    let cache = squeezefs::cache::TieredCache::new(
        Vec::new(),
        Some("16MB"),
        Some("16MB"),
        Some("8MB"),
        Some("16MB"),
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, alloc, dev);
    router.set_meta_backend(Arc::clone(routed));
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.meta_backend = Some(Arc::clone(routed));
    let req = fuse3::raw::Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 1,
        ..Default::default()
    };
    FsFront {
        fs,
        req,
        _dev: dev_file,
    }
}

/// **The HOLDER's served-mutation sink never drops the holder's DIRTY
/// layout entry** (review round 1, Issue 1 — the holder-side mirror of
/// `85e42408`). The holder appends to its OWN file through the FUSE write
/// handler (inline, unfsynced: the `layout_dirty` entry's `data_key` is
/// the ONLY copy of the acked bytes — "RAM only until fsync/release")
/// while a colleague `chmod`s the file through the ship (a record verb —
/// no custody grant stands between them). The served verb lands at the
/// holder's KV and runs the FUSE sink; before the fix the sink ran
/// `discard_layout_cache` UNCONDITIONALLY, the holder's `fsync` found
/// nothing dirty and returned 0 with the bytes in no KV. The law is the
/// recall sink's — ONE function, `DataRouter::discard_clean_layout_entry`:
/// decided under the ino's (3.5) guard in its non-parking form, a DIRTY
/// entry is KEPT (counted, `served_mutation_dirty_kept`), a CLEAN entry is
/// dropped, a held stripe skips the discard. The holder's fsync then lands
/// the bytes and every mount reads them; the served mode stands beside
/// them. A second file whose entry is CLEAN (fsynced) loses its entry at
/// the served verb, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_chmod_never_drops_the_holders_dirty_layout_entry() {
    use fuse3::raw::prelude::Filesystem;
    use std::ffi::OsStr;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-13b-dirty").await;
    mvol.checkpoint_now().await.unwrap();
    let j = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 62).await),
                identity: joiner_identity(&mvol, 62).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&j.volumes[0]);
    let jidentity = jvol.joined_wire().unwrap().identity;
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j),
            &peer_of(&jidentity),
            VENUE_SECRET.to_vec(),
        ),
    );
    let _arm = arm_publish_writer(&j, &jidentity).await;

    // The FUSE layer in front of the HOLDER, with the REAL served-mutation
    // sink installed (the mount arm's act on an armed set).
    let h = fs_in_front_of(&manager, "vol-13b-dirty").await;
    h.fs.install_served_mutation_sink();
    let s0 = record_ship_stats();
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // The holder's own files in its slot, made through the FUSE layer.
    let dirty_ino =
        h.fs.create(h.req, shared, OsStr::new("dirty"), libc::S_IFREG | 0o644, 0)
            .await
            .expect("the holder creates its file")
            .attr
            .ino;
    let clean_ino =
        h.fs.create(h.req, shared, OsStr::new("clean"), libc::S_IFREG | 0o644, 0)
            .await
            .expect("the holder creates its second file")
            .attr
            .ino;
    // An inline append the holder has NOT fsynced: its dirty layout entry
    // is the acked bytes' only home. The second file is fsynced — clean.
    for ino in [dirty_ino, clean_ino] {
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                0,
                bytes::Bytes::from_static(b"abcDEFG"),
                0,
                0,
            )
            .await
            .expect("the holder's inline write");
        assert_eq!(w.written, 7);
    }
    h.fs.fsync(h.req, clean_ino, 0, false)
        .await
        .expect("the clean file's fsync");
    let entry =
        h.fs.router
            .metadata_cache
            .get(&dirty_ino)
            .expect("the unfsynced write left a layout entry");
    assert!(entry.layout_dirty, "the entry is DIRTY — the acked write");
    assert!(
        h.fs.router
            .metadata_cache
            .get(&clean_ino)
            .is_some_and(|m| !m.layout_dirty),
        "the fsynced file's entry is CLEAN"
    );
    assert_eq!(
        manager.getattr(dirty_ino).await.unwrap().size,
        0,
        "the KV has not seen the append yet"
    );
    assert_eq!(manager.getattr(clean_ino).await.unwrap().size, 7);

    // The colleague chmods BOTH files through the ship: served at the
    // holder under its lease, the FUSE sink runs for each.
    for ino in [dirty_ino, clean_ino] {
        j.setattr(
            ino,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            Some(now_ns),
        )
        .await
        .expect("PR 13b: the colleague's chmod ships to the holder");
        assert_eq!(
            manager.getattr(ino).await.unwrap().mode,
            libc::S_IFREG | 0o600,
            "the holder's record carries the served mode"
        );
    }
    let s1 = record_ship_stats();
    assert_eq!(s1.record_served - s0.record_served, 2, "two served verbs");

    // The DIRTY entry survives the served mutation; the CLEAN one is gone.
    let kept = h.fs.router.metadata_cache.get(&dirty_ino).expect(
        "Issue 1: the served chmod's sink must KEEP the holder's DIRTY layout entry — it is \
         the acked write, and its data_key is the bytes' only copy",
    );
    assert!(kept.layout_dirty);
    assert_eq!(kept.size, 7);
    assert_eq!(kept.data_key.as_deref(), Some(&b"abcDEFG"[..]));
    assert!(
        h.fs.router.metadata_cache.get(&clean_ino).is_none(),
        "the CLEAN entry is dropped at the served verb (the holder's next read refetches)"
    );
    assert_eq!(
        s1.served_mutation_dirty_kept - s0.served_mutation_dirty_kept,
        1,
        "the kept dirty entry is counted (meta_ship.served_mutation_dirty_kept)"
    );
    assert_eq!(
        s1.served_mutation_discard_skipped, s0.served_mutation_discard_skipped,
        "no stripe was held: nothing skipped"
    );
    assert_eq!(
        squeezefs::invariant_tripwire_count("served_publish_over_dirty_entry"),
        0,
        "a record verb beside a dirty entry is legal — never the tripwire"
    );

    // The holder's fsync lands the bytes; every mount reads them, the
    // served mode beside them.
    h.fs.fsync(h.req, dirty_ino, 0, false)
        .await
        .expect("the holder's fsync lands the acked bytes");
    let at_holder = manager.getattr(dirty_ino).await.unwrap();
    assert_eq!(
        at_holder.size, 7,
        "the holder's KV carries the appended size"
    );
    assert_eq!(
        at_holder.mode,
        libc::S_IFREG | 0o600,
        "the served mode stands"
    );
    let layout = manager
        .getxattr(dirty_ino, "layout")
        .await
        .unwrap()
        .expect("the persisted inline record");
    let decoded = squeezefs::layout_wire::decode_layout_any(&layout).unwrap();
    assert_eq!(decoded.data_key.as_deref(), Some(&b"abcDEFG"[..]));
    let at_joiner = j.getattr(dirty_ino).await.unwrap();
    assert_eq!(at_joiner.size, 7, "the colleague reads the landed size");
    assert_eq!(at_joiner.mode, libc::S_IFREG | 0o600);
    let got =
        h.fs.read(h.req, dirty_ino, 0, 0, 7, 0)
            .await
            .expect("the holder's own read")
            .data
            .to_vec();
    assert_eq!(got, b"abcDEFG", "the holder reads its own bytes back");
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    squeezefs::meta_backend::record_ship::install_served_mutation_sink(Arc::new(|_, _| {
        Box::pin(async {})
    }));
    disarm_publish_writer().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&j).await;
    drop(jvol);
    drop(j);
    drop(h);
    mvenue.tear_down();
    shutdown(&manager).await;
}

/// **PR 13b — the REVERSE direction, and the holder's dominance window
/// fed by the served verb.** The MANAGER mutates a file the JOINER's slot
/// holds: the record-level verbs ship to the joiner's listener (tree 0's
/// lessee, the endpoint bound as the ladder binds it), the joiner applies
/// them under ITS lease and ring, and the JOINER — the holder, the
/// authority for the record — reads every one. The served verb is the
/// requester's op for the slot's dominance window (§5.1.4 — the defect
/// 9/13 law on this wire): `note_slot_ship` at the holder names the
/// manager's appender id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_managers_setattr_of_a_joiners_file_lands_at_the_joiner() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-13b-rev").await;
    let j = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 62).await),
                identity: joiner_identity(&mvol, 62).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&j.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jvenue = DaemonVenue::stand_up(&j, false, "joiner-custody-13b-rev").await;
    // The joiner's first touch takes the seeded slot; its file lives there.
    let f = j
        .create(shared, "jf", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_A).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
        ),
        "the joiner leases the slot"
    );
    // The bindings the ladders make: the manager dials the joiner where it
    // serves (rung 7's census binding); the manager's step shipper carries
    // its member id so the served verb's requester is known.
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    let midentity = read_directory(mvol.device_path(), mvol.superblock())
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.appender_id == 0)
        .and_then(|e| e.page)
        .expect("the manager's page")
        .identity;
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&manager),
            &peer_of(&midentity),
            VENUE_SECRET.to_vec(),
        ),
    );
    let s0 = record_ship_stats();
    manager
        .setattr(
            f,
            Some(libc::S_IFREG | 0o640),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("PR 13b: the manager's setattr of a joiner-slot file ships to the joiner");
    manager
        .setxattr(f, "user.from-manager", b"1")
        .await
        .expect("PR 13b: the manager's setxattr ships");
    let at_holder = j.getattr(f).await.expect("the holder reads its record");
    assert_eq!(at_holder.mode, libc::S_IFREG | 0o640);
    assert_eq!(
        j.getxattr(f, "user.from-manager").await.unwrap().as_deref(),
        Some(&b"1"[..])
    );
    let s1 = record_ship_stats();
    assert_eq!(s1.record_ships - s0.record_ships, 2);
    assert_eq!(s1.record_served - s0.record_served, 2);
    assert_eq!(s1.record_refusals, s0.record_refusals);
    // The dominance window: the served verbs counted as the MANAGER's ops
    // on the slot at the holder (the requester off the shipper's member id).
    let ships_after_verbs = jvol.slot_lease_stats().map_or(0, |s| s.ships);
    assert!(
        ships_after_verbs >= 2,
        "the holder's window counts the manager's two ships (slot_ships)"
    );
    // The DATA face feeds the window too (review round 1, Issue 8): the
    // manager's layout publish of the joiner's file ships to the joiner
    // under the per-holder custody lease and is the manager's op on the
    // slot at the holder — RED on the first build, where only the record
    // verbs' served note reached `note_slot_ship`.
    let _arm = arm_publish_writer(&manager, &midentity).await;
    let inline = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "inline".into(),
        size: 5,
        data_key: Some(b"hello".to_vec()),
        ..Default::default()
    })
    .unwrap();
    squeezefs::meta_ship::publish::set_layout_and_size(&manager, f, &inline, 5, &[])
        .await
        .expect("PR 13b: the manager's layout publish of a joiner-slot file ships to the joiner");
    assert_eq!(j.getattr(f).await.unwrap().size, 5, "the holder's record");
    let s2 = record_ship_stats();
    assert_eq!(s2.foreign_publish_ships - s1.foreign_publish_ships, 1);
    assert_eq!(s2.foreign_publish_served - s1.foreign_publish_served, 1);
    assert_eq!(
        jvol.slot_lease_stats().map_or(0, |s| s.ships),
        ships_after_verbs + 1,
        "the served publish is the requester's op on the slot (slot_ships +1)"
    );
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    disarm_publish_writer().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&j).await;
    drop(jvol);
    drop(j);
    jvenue.tear_down();
    // The released slot's record is exact at the manager after the leave.
    assert_eq!(
        manager.getattr(f).await.unwrap().mode,
        libc::S_IFREG | 0o640
    );
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **PR 13b — joiner → joiner (three daemons), and the ONE refusal the ship
/// keeps: an UNREACHABLE holder.** Joiner 1 holds a file in its slot;
/// joiner 2 knows no endpoint for joiner 1 (its ladder published none in
/// this fixture — the shape of a joiner whose rung 7 has not run, or a
/// dead member's until PR 10 re-leases its slots). Every record-level verb
/// and the write-intent open answer the RETRYABLE class
/// (`Retryable { HolderUnreachable }`, `EAGAIN` — never PR 13's `EREMOTE`,
/// never `ENOENT`, and not in coreutils' `is_ENOTSUP` set), counted on
/// `record_unreachable`; nothing moves at either daemon. Once joiner 1's
/// endpoint is bound (the ladder's act) the SAME verbs land at joiner 1
/// and joiner 1 reads them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_slot_verb_whose_holder_is_unreachable_refuses_retryable_then_lands_once_bound() {
    use squeezefs::error::{RefusalClass, SqueezefsError};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-13b-3").await;
    let j1 = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 63).await),
                identity: joiner_identity(&mvol, 63).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let j1vol = Arc::clone(&j1.volumes[0]);
    let j1id = j1vol.appender_stats().unwrap().appender_id;
    let f = j1
        .create(shared, "j1f", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    j1vol.checkpoint_now().await.unwrap();
    let j2 = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 64).await),
                identity: joiner_identity(&mvol, 64).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let j2vol = Arc::clone(&j2.volumes[0]);
    let j2identity = j2vol.joined_wire().unwrap().identity;
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&j2),
            &peer_of(&j2identity),
            VENUE_SECRET.to_vec(),
        ),
    );
    assert!(
        j2vol
            .slot_leases()
            .unwrap()
            .holders
            .endpoint(j1id)
            .is_none(),
        "joiner 1 published no endpoint in this fixture"
    );
    let s0 = record_ship_stats();
    let retryable = |e: SqueezefsError, what: &str| {
        assert!(
            matches!(
                e.refusal_class(),
                Some(RefusalClass::HolderUnreachable { holder }) if holder == j1id
            ),
            "{what}: the typed retryable class naming the holder: {e:?}"
        );
        let errno = e.to_errno();
        assert_eq!(errno, libc::EAGAIN, "{what}: the retryable class");
        assert_ne!(errno, libc::EREMOTE, "{what}: the interim word is retired");
        assert_ne!(errno, libc::ENOENT, "{what}: the file exists");
        assert!(
            ![libc::ENOTSUP, libc::EOPNOTSUPP].contains(&errno),
            "{what}: coreutils' chmod/chown swallow is_ENOTSUP as 'not applied'"
        );
    };
    retryable(
        j2.setattr(
            f,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("setattr toward an unreachable holder refuses retryable"),
        "setattr",
    );
    retryable(
        j2.setxattr(f, "user.x", b"1")
            .await
            .expect_err("setxattr toward an unreachable holder refuses retryable"),
        "setxattr",
    );
    retryable(
        j2.removexattr(f, "user.x")
            .await
            .expect_err("removexattr toward an unreachable holder refuses retryable"),
        "removexattr",
    );
    retryable(
        j2.refuse_foreign_slot_open(f, libc::O_WRONLY as u32)
            .await
            .expect_err("a write-intent open toward an unreachable holder refuses retryable"),
        "open for write",
    );
    j2.refuse_foreign_slot_open(f, libc::O_RDONLY as u32)
        .await
        .expect("a read-only open passes");
    let s1 = record_ship_stats();
    assert_eq!(s1.record_unreachable - s0.record_unreachable, 4);
    assert_eq!(s1.record_ships, s0.record_ships, "nothing shipped");
    assert_eq!(
        j1.getattr(f).await.unwrap().mode,
        libc::S_IFREG | 0o644,
        "nothing moved at the holder"
    );
    // Joiner 1 stands up its listener and the binding lands (the ladder's
    // act); the same verbs now travel and land.
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1-custody-13b").await;
    j2vol
        .slot_leases()
        .unwrap()
        .holders
        .set_endpoint(j1id, &j1venue.endpoint);
    j2.setattr(
        f,
        Some(libc::S_IFREG | 0o600),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("PR 13b: joiner 2's setattr of joiner 1's file lands at joiner 1");
    j2.setxattr(f, "user.x", b"1")
        .await
        .expect("PR 13b: joiner 2's setxattr lands at joiner 1");
    j2.refuse_foreign_slot_open(f, libc::O_WRONLY as u32)
        .await
        .expect("the write-intent open passes once the holder is reachable");
    assert_eq!(j1.getattr(f).await.unwrap().mode, libc::S_IFREG | 0o600);
    assert_eq!(
        j1.getxattr(f, "user.x").await.unwrap().as_deref(),
        Some(&b"1"[..])
    );
    let s2 = record_ship_stats();
    assert_eq!(s2.record_ships - s1.record_ships, 2);
    assert_eq!(s2.record_served - s1.record_served, 2);
    assert_eq!(s2.record_refusals, s0.record_refusals);
    assert_must_stay_zero(&j1vol, "joiner 1");
    assert_must_stay_zero(&j2vol, "joiner 2");
    assert_must_stay_zero(&mvol, "manager");
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&j2).await;
    drop(j2vol);
    drop(j2);
    shutdown(&j1).await;
    drop(j1vol);
    drop(j1);
    j1venue.tear_down();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **PR 13 review round 1, Issue 13 — the roll-forward's LOCAL slot-moved
/// arm.** `execute` (defect 35) leaves an intent OPEN when a local step's
/// door answers `SlotBusy` past the retry bound; `recover_one` had the
/// shipped half of that arm only (`Err(e) if armed && shipped`), so the
/// same refusal on a LOCAL step of an intent being rolled forward
/// propagated as the cadence's error — and at the mount path's recovery
/// as the mount refusal a local device failure earns — for a slot the
/// plane moved on purpose. Here the joiner's severed unlink leaves its
/// child's `SetNlink` (LOCAL — the joiner's rotor) open; the door refuses
/// that step `SlotBusy` past the bound at the next roll-forward. RED
/// before: `roll_forward_open_intents` → `Err(EAGAIN …)`. GREEN: `Ok(0)`,
/// the intent stays open and is retired by the pass after the slot
/// settles (`Ok(1)`, `nlink 0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_roll_forwards_local_step_refused_slot_busy_past_the_bound_leaves_the_intent_open() {
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, install_xv_shipper, roll_forward_open_intents, uninstall_xv_shipper,
        TEST_XV_LOCAL_STEP_SLOT_BUSY, TEST_XV_SEAM_AFTER_STEPS, TEST_XV_SEAM_INITIATOR,
    };
    use squeezefs::meta_ship::MetaShipRouter;
    use std::sync::atomic::Ordering::{Relaxed, SeqCst};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    TEST_XV_LOCAL_STEP_SLOT_BUSY.store(0, SeqCst);
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-issue13").await;
    manager
        .create(shared, "m0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 53).await),
                identity: joiner_identity(&mvol, 53).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    install_xv_shipper(MetaShipRouter::new(
        Arc::clone(&joiner),
        "joiner-node",
        VENUE_SECRET.to_vec(),
    ));
    // The joiner reads the manager's directory through the manager's
    // TOKENS (PR 9's custody arm — the mount path's `arm_mount_slot_
    // custody`), so the dentry it ships is exact at its next read.
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
    let victim = joiner
        .create(shared, "victim", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a create into the manager's directory ships its dentry");
    // The severed unlink: the shipped `RemoveDentry` lands at the manager,
    // the child's LOCAL `SetNlink` never runs — the intent is open in the
    // joiner's ring, abandoned by its op.
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
    assert_eq!(cross_owner_stats().intents_open, open0 + 1);
    assert_eq!(joiner.getattr(victim.ino).await.unwrap().nlink, 1);
    // The roll-forward meets the door's `SlotBusy` on the LOCAL step past
    // the retry bound (2 retries + the final refusal = 3 words per pass;
    // the set's own cadence may run a pass beside this one, so the seam
    // holds words for every pass that reaches the door).
    TEST_XV_LOCAL_STEP_SLOT_BUSY.store(300, SeqCst);
    let rolled = roll_forward_open_intents(&joiner).await.expect(
        "a local step's slot-moved refusal is the retryable class — never the cadence's error",
    );
    assert_eq!(rolled, 0, "the intent stays open");
    assert!(
        TEST_XV_LOCAL_STEP_SLOT_BUSY.load(SeqCst) <= 297,
        "the door refused the step at every attempt of the bound"
    );
    assert_eq!(cross_owner_stats().intents_open, open0 + 1, "still open");
    assert_eq!(
        joiner.getattr(victim.ino).await.unwrap().nlink,
        1,
        "nothing applied under the refusals"
    );
    // The slot settled: the next pass (this one, or the cadence's beside
    // it — a pass that finds the record gone under its guards applies
    // nothing) completes the plan.
    TEST_XV_LOCAL_STEP_SLOT_BUSY.store(0, SeqCst);
    roll_forward_open_intents(&joiner)
        .await
        .expect("the next pass");
    assert_eq!(cross_owner_stats().intents_open, open0, "retired");
    assert_eq!(joiner.getattr(victim.ino).await.unwrap().nlink, 0);
    // The joiner still writes — nothing fail-stopped.
    joiner
        .create(1, "alive", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's volumes are not disabled");
    uninstall_xv_shipper();
    squeezefs::data_grant::disarm_slot_custody().await;
    shutdown(&joiner).await;
    mvenue.tear_down();
    shutdown(&manager).await;
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

/// **The served shipped free's reference count reads the trees the
/// manager WRITES, never a joiner's PROJECTION** (PR 13 — found by the
/// fleet's `sym-walls` row (a): every joiner's terminal free under
/// `w_rewrite` was ABANDONED after 3 attempts — the manager's count for
/// the shipped free walked EVERY slot tree, its projection of a joiner's
/// tree included, and that projection's root (the grant-time image) had
/// been retired by the joiner, returned through `ReturnExtents` and
/// re-granted: `tree 0 (slot Some(1042), root …, a PROJECTION here):
/// traversal retry budget exhausted`; 5,426 replays at the manager, 0
/// blocks served). The projection is also STALE by design — the lessee
/// appends into its images and moves its root under its own page — so
/// the union count answered a reference the joiner had RELEASED as still
/// held: a free judged `NonTerminal` for ever, the block leaked. PR 7
/// §5.4.3 law 2: an unshared block's references live in its owner's slot
/// tree and the lessee's terminal free carries that tree's verdict; a
/// block two slots share is the index's, never a count's. Pinned on the
/// two-backend fixture: the joiner publishes a block reference on its own
/// file, the manager's projection of the joiner's tree LOADS it (the
/// union count reads 1), the joiner RELEASES it (a displacing publish in
/// its ring) — the manager's union count still reads the stale 1 while
/// `block_ref_count_maintained` (the served free's word) reads 0, and a
/// reference in a tree the manager writes counts on both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_served_frees_refcount_skips_a_joiners_projection_tree() {
    use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let other = dirs[1];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 71).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let tag = volume_tag("vol-00000000000000a7");
    let layout = |block_idx: u64| -> Vec<u8> {
        let mut map = std::collections::HashMap::new();
        map.insert(
            0u32,
            format!("vol-00000000000000a7://{}", block_idx * 4 * 1024 * 1024),
        );
        bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
            file_type: "striped".into(),
            size: 4 * 1024 * 1024,
            block_map: Some(map),
            ..Default::default()
        })
        .unwrap()
    };
    let taken = |blk: u64, owner: u64| {
        BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: blk,
            owner_ino: owner,
            block_index: 0,
        })
    };
    let released = |blk: u64, owner: u64| {
        BlockRefOp::released(BlockRef {
            vol_tag: tag,
            block_idx: blk,
            owner_ino: owner,
            block_index: 0,
        })
    };
    // The shape the fleet had: a tree the manager HOLDS a projection of —
    // one of its own ROTOR slots' (the slot `bf` mints into), the
    // manager's until it releases the slot and the joiner acquires it;
    // the manager's `KvTree` for the slot stays, FOREIGN now, at the
    // grant-time image. The manager's file `bf` carries block 17 in that
    // tree; its own file `mf` in another slot carries block 18.
    let bfile = manager
        .create(other, "bf", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let slot_b = slot_of_global(&manager, bfile);
    manager
        .set_layout_and_size(bfile, &layout(17), 4 * 1024 * 1024, &[taken(17, bfile)])
        .await
        .expect("the manager publishes block 17 in SLOT_B's tree");
    let mfile = manager
        .create(1, "mf", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    manager
        .set_layout_and_size(mfile, &layout(18), 4 * 1024 * 1024, &[taken(18, mfile)])
        .await
        .expect("the manager publishes its block");
    assert_eq!(mvol.block_ref_count(tag, 17).await.unwrap(), 1);
    assert_ne!(
        slot_of_global(&manager, mfile),
        slot_b,
        "mf sits in another slot"
    );
    mvol.release_slot_handover(0, slot_b)
        .await
        .expect("the manager releases bf's slot to unleased");
    // The joiner takes the slot (an offer accepted — the wire first
    // touch): the transfer barrier adopts the manager's live root at the
    // joiner; the manager's tree of the slot is a PROJECTION from here.
    let routing_b = mvol
        .routing_slot_of_forest(slot_b)
        .expect("the slot's routing slot");
    assert_eq!(
        jvol.joined_accept_offers(&[(routing_b, 0)]).await,
        1,
        "the joiner acquires the slot over the wire"
    );
    assert!(jvol.slot_leases().unwrap().gate.is_leased(slot_b));
    assert!(mvol.slot_leases().unwrap().gate.is_foreign(slot_b));
    assert_eq!(
        jvol.block_ref_count(tag, 17).await.unwrap(),
        1,
        "the joiner's adopted tree holds the reference"
    );
    // The joiner RELEASES it (a displacing publish of the file it now
    // holds, in ITS ring).
    joiner
        .set_layout_and_size(
            bfile,
            &layout(19),
            4 * 1024 * 1024,
            &[released(17, bfile), taken(19, bfile)],
        )
        .await
        .expect("the joiner's displacing publish");
    jvol.checkpoint_now()
        .await
        .expect("the joiner's checkpoint");
    assert_eq!(
        jvol.block_ref_count(tag, 17).await.unwrap(),
        0,
        "the joiner's own tree released it"
    );
    // The manager's union count still reads the STALE 1 (its projection
    // never learns a lessee's release — the class the served free judged
    // `NonTerminal` for ever); the served free's word reads the trees the
    // manager writes: 0.
    assert_eq!(
        mvol.block_ref_count(tag, 17).await.unwrap(),
        1,
        "the union over projections is not the truth (stated, not relied on)"
    );
    assert_eq!(
        mvol.block_ref_count_maintained(tag, 17).await.unwrap(),
        0,
        "the served free counts what the manager WRITES"
    );
    assert_eq!(
        mvol.block_ref_count_maintained(tag, 18).await.unwrap(),
        1,
        "a reference in a tree the manager writes counts"
    );
    assert_eq!(
        jvol.block_ref_count_maintained(tag, 19).await.unwrap(),
        1,
        "the joiner counts its own leased tree"
    );
    shutdown(&joiner).await;
    venue.tear_down();
    shutdown(&manager).await;
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

/// **A re-hold's deferred leak release CONVERGES on the live peers'
/// declared windows — across two failovers** (PR 12b review round 2,
/// Issue 25): round 1 deferred every SET-but-unreferenced-and-ungranted
/// bit while any peer page was `Live` and released it only at a
/// peer-less re-hold — on a fleet that keeps writing the set grew ≈ G/2
/// blocks per writer per manager failover for ever (53 → 340 of 4,096
/// per volume across two failovers in the acceptance tape). The
/// predecessor's ledger — a window's only record — died with it, so the
/// bitmap alone cannot tell a live writer's remainder from a dead one's;
/// the live writer's OWN word can: its membership renewal carries its
/// unconsumed ranges every beat (`RenewFrame::block_grant_windows`, the
/// source installed by the joined allocation arm), the holder ADOPTS a
/// declared range into its ledger under the writer's name (revocable at
/// its death, returnable at its leave) and, once every `Live` peer page's
/// writer has declared (or its page is no longer `Live` — dead,
/// recovered), releases what is still pending as provably nobody's. The
/// verdict runs at the ledger poll's cadence and at the re-hold.
///
/// Two failovers over the REAL membership wire (an owner + plane per
/// manager, the joiner a writer member re-pointed by the successor
/// observation): at each successor's re-hold the joiner's remainder and
/// the dead manager's own remainder are DEFERRED; the joiner's next
/// renewal at the successor declares; pending → 0 within a beat, the
/// joiner's remainder ADOPTED (its blocks stay SET, its later mints stay
/// disjoint from everything minted before), the dead manager's
/// RELEASED (CLEAR); `deferred ≡ released + adopted + pending` exact at
/// every step. Red before: `pending` stayed at the deferred count and
/// nothing was released while the joiner lived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_holds_deferred_leaks_converge_on_the_live_peers_declared_windows() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::block_grant::WindowDecl;
    use squeezefs::data_alloc_bitmap::{
        DATA_ALLOC_BITMAP_LEAKS_ADOPTED as ADOPTED, DATA_ALLOC_BITMAP_LEAKS_DEFERRED as DEFERRED,
        DATA_ALLOC_BITMAP_LEAKS_RELEASED as RELEASED,
    };
    use squeezefs::membership::{self, LeaseClock, LeaseClocks, MembershipOwner, OwnerRecord};
    use squeezefs::membership_wire::{MembershipPlane, MembershipPlaneConfig};
    use squeezefs::meta_backend::kv::alloc_lease;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    membership::note_successor_observed(None);
    let clocks = LeaseClocks::with_params(
        std::time::Duration::from_millis(1_500),
        std::time::Duration::from_millis(50),
        std::time::Duration::from_millis(100),
    )
    .expect("short clocks");
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let beat_ms = clocks.renew_interval.as_millis() as u64;
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "other")]).await;
    let shared = dirs[0];
    let data_id = "vol-converge-data";
    let data_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(data_id);
    let data_blocks = 4096u64;
    // The closure over THIS pin's holdings: the counters are process-wide
    // and cumulative (an earlier contract's holding, reset with its
    // pending set, breaks the absolute identity), the law is per holding
    // lifetime — so deltas from the pin's base, with `pending` absolute (0
    // at the base: a fresh process state, the first hold has no leaks).
    let base0 = (
        DEFERRED.load(Relaxed),
        RELEASED.load(Relaxed),
        ADOPTED.load(Relaxed),
    );
    let closure = move || {
        (
            DEFERRED.load(Relaxed) - base0.0,
            RELEASED.load(Relaxed) - base0.1,
            ADOPTED.load(Relaxed) - base0.2,
            alloc_lease::leaks_pending_total(),
        )
    };
    let assert_closure = |(d, r, a, p): (u64, u64, u64, u64)| {
        assert_eq!(
            d,
            r + a + p,
            "deferred ≡ released + adopted + pending (deferred {d}, released {r}, adopted {a}, \
             pending {p})"
        );
    };

    // Manager 1, the holder; the joiner mints from its grants.
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;
    let a = Arc::new(BlockAllocator::new(data_id).await.unwrap());
    a.set_capacity_bytes(data_blocks * a.chunk_size());
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&manager, &[Arc::clone(&a)])
            .await
            .unwrap(),
        1
    );
    let base = closure();
    assert_closure(base);
    let joiner = join(&uris, &venue, &mvol, 72).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jwire = Arc::clone(jvol.joined_wire().expect("joined"));
    let writer = squeezefs::cowriter::node_member_id_of(
        jwire.identity.node_token,
        jwire.identity.mount_slot,
    );
    let hv = squeezefs::sym_join::joined_holder_venue(&joiner, data_tag, venue.endpoint());
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
    // The member's declaration source — what the joined allocation arm
    // installs on a mount.
    let decl_b = Arc::clone(&b);
    membership::install_window_decl_source(Arc::new(move || {
        let ranges = alloc_lease::held_block_ranges(&decl_b);
        if ranges.is_empty() {
            Vec::new()
        } else {
            vec![WindowDecl {
                vol_tag: data_tag,
                ranges,
            }]
        }
    }));
    // The joiner's mints are IN FLIGHT (minted, their publish not yet
    // durable — the write path's registry guard held across the DMA):
    // neither in its window nor referenced, and its to declare.
    let mut minted = std::collections::BTreeSet::new();
    let mut inflight_guards = Vec::new();
    for _ in 0..8 {
        let off = b.allocate_block().await.unwrap();
        inflight_guards.push(b.inflight_register(off));
        minted.insert(off / b.chunk_size());
    }
    // The manager minted too and never published — the dead
    // incarnation's remainder once it dies: nobody's.
    let mut manager_minted = std::collections::BTreeSet::new();
    for _ in 0..8 {
        manager_minted.insert(a.allocate_block().await.unwrap() / a.chunk_size());
    }
    assert_eq!(minted.len() + manager_minted.len(), 16);
    let files = create_files(&joiner, shared, "cv", 4).await;

    // The membership plane at manager 1; the joiner a WRITER member whose
    // renewal carries its window.
    let owner_id = |term: u64| format!("converge-owner-{term}");
    let stand_owner = |term: u64| {
        let owner = MembershipOwner::arm(
            &owner_id(term),
            term,
            term - 1,
            clocks.clone(),
            LeaseClock::monotonic(),
        )
        .expect("the owner arms");
        let plane = MembershipPlane::start(
            MembershipPlaneConfig::loopback(),
            VENUE_SECRET.to_vec(),
            Arc::clone(&owner),
        )
        .expect("the plane binds");
        (owner, plane)
    };
    let (owner1, plane1) = stand_owner(3);
    membership::install_owner(Arc::clone(&owner1));
    let rec = OwnerRecord {
        v: 1,
        id: owner_id(3),
        term: 3,
        endpoint: plane1.endpoint().to_string(),
        ttl_ms: t_owner_ms,
        owner_claim_id: String::new(),
        ts: 0,
        pid: std::process::id(),
        boot: "boot-converge".to_string(),
    };
    let arm = membership::join_as_writer_member(&rec, VENUE_SECRET.to_vec(), &writer, 0, None)
        .await
        .expect("the join is admitted")
        .expect("a rendezvous record exists");
    assert!(owner1.member_is_live(&writer));

    let mut prior_holding = alloc_lease::holding(data_tag).expect("manager 1 holds");
    let mut plane = plane1;
    let mut current_manager = manager;
    let mut current_vol = mvol;
    let mut current_venue = venue;
    // The successor's membership term: 3 was the first manager's, each
    // failover bumps it.
    for (failover, successor_term) in (1..=2u32).zip(4u64..) {
        // The manager DIES with its listener and its membership plane; the
        // joiner's remainder and the dead manager's own remainder are RAM
        // at a ledger that died.
        let joiner_remainder: std::collections::BTreeSet<u64> = b
            .block_grant_unconsumed()
            .iter()
            .flat_map(|g| g.start..g.end())
            .collect();
        assert!(
            !joiner_remainder.is_empty(),
            "failover {failover}: the joiner holds a window remainder to declare"
        );
        current_venue.tear_down();
        shutdown(&current_manager).await;
        alloc_lease::disarm_symmetric_roles();
        drop(current_vol);
        drop(current_manager);
        drop(prior_holding);
        plane.shutdown();
        membership::uninstall();

        let successor = open_under(&uris, &Knobs::armed()).await;
        let svol = Arc::clone(&successor.volumes[0]);
        let venue2 = HoldersVenue::stand_up(&successor, &[]).await;
        squeezefs::multi_writer::publish_symmetric_endpoint(&successor, &venue2.endpoint()).await;
        svol.checkpoint_now().await.unwrap();
        let (owner2, plane2) = stand_owner(successor_term);
        owner2.open_grace(vec![writer.clone()]);
        membership::install_owner(Arc::clone(&owner2));
        membership::note_successor_observed(Some(plane2.endpoint().to_string()));
        let a2 = Arc::new(BlockAllocator::new(data_id).await.unwrap());
        a2.set_capacity_bytes(data_blocks * a2.chunk_size());
        let before = closure();
        assert_eq!(
            alloc_lease::arm_symmetric_allocation(&successor, &[Arc::clone(&a2)])
                .await
                .unwrap(),
            1
        );
        let sholding = alloc_lease::holding(data_tag).expect("the successor holds");
        let after_arm = closure();
        assert_closure(after_arm);
        assert!(
            after_arm.0 > before.0,
            "failover {failover}: the re-hold deferred the dead ledger's remainders"
        );
        assert!(
            after_arm.3 >= joiner_remainder.len() as u64,
            "failover {failover}: the joiner's remainder ({}) is among the {} pending",
            joiner_remainder.len(),
            after_arm.3
        );
        assert_eq!(
            after_arm.1, before.1,
            "failover {failover}: nothing released under a live peer's feet at the arm"
        );

        // The joiner's reclaim lands at the successor's grace window and
        // its next renewal DECLARES; the verdict (the poll's cadence here,
        // run by hand) converges within a beat of it.
        let landed = std::time::Instant::now() + std::time::Duration::from_millis(t_owner_ms);
        while owner2.epoch_of(&writer).is_none() {
            assert!(
                std::time::Instant::now() < landed,
                "failover {failover}: the joiner's reclaim lands at the successor"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let converged =
            std::time::Instant::now() + std::time::Duration::from_millis(3 * beat_ms + t_owner_ms);
        loop {
            alloc_lease::converge_deferred_leaks().await;
            if alloc_lease::leaks_pending_total() == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < converged,
                "failover {failover}: the deferred set never converged ({:?})",
                closure()
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let done = closure();
        assert_closure(done);
        assert_eq!(done.3, 0, "failover {failover}: pending → 0");
        assert!(
            done.2 - after_arm.2 >= joiner_remainder.len() as u64,
            "failover {failover}: the joiner's declared remainder was ADOPTED ({} ≥ {})",
            done.2 - after_arm.2,
            joiner_remainder.len()
        );
        assert!(
            done.1 > after_arm.1,
            "failover {failover}: the dead manager's own remainder was RELEASED"
        );
        for blk in &joiner_remainder {
            assert!(
                sholding.bitmap.is_set(*blk),
                "failover {failover}: the joiner's window block {blk} stays SET"
            );
        }
        let adopted_ranges = sholding.ledger.grants_of(&writer);
        for blk in &joiner_remainder {
            assert!(
                adopted_ranges.iter().any(|g| g.contains(*blk)),
                "failover {failover}: block {blk} is the joiner's grant in the successor's ledger"
            );
        }
        for blk in &minted {
            assert!(
                sholding.bitmap.is_set(*blk),
                "failover {failover}: the joiner's in-flight block {blk} stays SET"
            );
        }
        for blk in &manager_minted {
            assert!(
                !sholding.bitmap.is_set(*blk),
                "failover {failover}: the dead manager's unpublished mint {blk} is CLEAR — \
                 nobody's"
            );
        }
        // The joiner keeps minting — its window first, then the successor's
        // grants — with no block minted twice against anything it holds.
        let mut after: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for _ in 0..(joiner_remainder.len() + 40) {
            let off = b.allocate_block().await.unwrap();
            inflight_guards.push(b.inflight_register(off));
            after.insert(off / b.chunk_size());
        }
        assert!(
            minted.is_disjoint(&after),
            "failover {failover}: a block minted twice across the failover"
        );
        assert!(
            joiner_remainder.is_subset(&after),
            "failover {failover}: the joiner minted its declared window through"
        );
        minted.extend(after.iter().copied());
        manager_minted.clear();
        for _ in 0..8 {
            manager_minted.insert(a2.allocate_block().await.unwrap() / a2.chunk_size());
        }
        assert_all_resolve(&joiner, shared, &files).await;

        prior_holding = sholding;
        plane = plane2;
        current_manager = successor;
        current_vol = svol;
        current_venue = venue2;
    }

    arm.disarm().await;
    drop(inflight_guards);
    membership::note_successor_observed(None);
    membership::uninstall_window_decl_source();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    drop(prior_holding);
    plane.shutdown();
    current_venue.tear_down();
    shutdown(&current_manager).await;
    drop(current_vol);
    drop(current_manager);
    membership::uninstall();
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
/// commit lands after it). Under PR 13g's pool law a retired image
/// RECYCLES into a pool below its target and the cadence ships nothing,
/// so the pool is grown ABOVE its target while the manager lives — the
/// quiet cadence's SHRINK is then the cycle's wire verb, and the retired
/// image is surplus with it (PR 13g review round 1, Issue 5).
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
    // The pool above its target (the join's cost class): the cadence's
    // shrink returns the surplus — and the retired image with it — as the
    // cycle's `ReturnExtents`.
    let got = jvol.joined_extent_grant(200).await.unwrap();
    let pool_floor = squeezefs::meta_backend::kv::appender::joined_pool_floor(
        jvol.slot_lease_stats().expect("the plane").rotor,
    );
    assert!(
        got >= 16 && region_stats(&jvol).grant_unclaimed > pool_floor + 16,
        "the pool stands above its target (got {got}, unclaimed {}, floor {pool_floor} — the \
         heap-share cap bounds the ask on this small volume)",
        region_stats(&jvol).grant_unclaimed
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
        // `a_clean_remount_recovers_the_grant_from_tree_zero_…`); the
        // first cycle's shrink already ships the pool's surplus — the
        // loop runs until the retired IMAGE itself left this mount's
        // grant (the pool law: never break on the first return alone).
        for _ in 0..4 {
            jvol.checkpoint_now().await.unwrap();
            let r = region_stats(&jvol);
            if jvol.joined_stats().unwrap().wire_extent_returns > returns0
                && r.grant_pending == 0
                && r.grant_returnable == 0
            {
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

/// Where `routed`'s tree routes the dentry `(dir, name)` and what its RAM
/// fold says: `(leaf addr, leaf node_seq, "Live" | "Tombstone" | "Absent")`.
async fn locate_name(
    routed: &RoutedMetaBackend,
    kv: &KvMetaBackend,
    dir: u64,
    name: &str,
) -> Option<(u64, u64, String)> {
    let (_, local_dir) = routed.route_ino(dir);
    use squeezefs::meta_backend::kv::record::{dentry_key, dentry_name_hash54};
    let hash = dentry_name_hash54(name.as_bytes(), kv.superblock().hash_seed);
    let legacy = dentry_key(local_dir, hash, 0);
    let (tree, fkey) = kv
        .record_locator(squeezefs::meta_backend::kv::record::TREE_DENTRIES, &legacy)
        .ok()??;
    let leaf = tree.resolve_leaf(&fkey).await.ok()?;
    let snap = leaf.snapshot();
    let fold = match snap.lookup(&fkey) {
        Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Live(_)) => "Live",
        Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Tombstone) => "Tombstone",
        Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Absent) => "Absent",
        Err(_) => "Err",
    };
    Some((leaf.addr(), leaf.node_seq(), fold.to_string()))
}

/// ATTRIBUTION of a resurrected name (a removed name a fresh reader still
/// resolves): the leaf that holds its dentry — the RAM fold for the key
/// and every frame of its extent on the DEVICE (kind + seq per record) —
/// so the record says whether the tombstone is missing from the image,
/// sits in a frame the walk does not reach, or was never written.
async fn attribute_resurrection(
    routed: &RoutedMetaBackend,
    kv: &KvMetaBackend,
    dir: &u64,
    name: &str,
) {
    // The dentry's key is in the LOCAL KEY form the routed layer frames
    // (`route_ino`) — a global ino would route to the native slot.
    let (_, local_dir) = routed.route_ino(*dir);
    use squeezefs::meta_backend::kv::record::{dentry_key, dentry_name_hash54};
    let hash = dentry_name_hash54(name.as_bytes(), kv.superblock().hash_seed);
    let legacy = dentry_key(local_dir, hash, 0);
    if let Ok(Some((tree, fkey))) =
        kv.record_locator(squeezefs::meta_backend::kv::record::TREE_DENTRIES, &legacy)
    {
        if let Ok(leaf) = tree.resolve_leaf(&fkey).await {
            let addr = leaf.addr();
            let snap = leaf.snapshot();
            let ram = format!(
                "newest_seq {:?}, fold {}",
                snap.newest_seq_of(&fkey),
                match snap.lookup(&fkey) {
                    Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Live(_)) => "Live",
                    Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Tombstone) =>
                        "Tombstone",
                    Ok(squeezefs::meta_backend::kv::node_cache::LiveLookup::Absent) => "Absent",
                    Err(_) => "Err",
                }
            );
            let layout = kv.node_cache().config().layout;
            let node_size = layout.node_size();
            let buf = squeezefs::uring_fs::read_at(kv.device_path(), addr, node_size)
                .await
                .unwrap();
            let loaded =
                squeezefs::meta_backend::kv::node::verify_node_extent(buf, &layout, addr, 0)
                    .unwrap();
            let mut disk: Vec<String> = Vec::new();
            let mut census: Vec<String> = Vec::new();
            for (fi, view) in loaded.bset_views_newest_first().unwrap().iter().enumerate() {
                for i in view.find(&fkey) {
                    let r = view.record(i);
                    disk.push(format!("frame{fi}:{:?}@{}", r.kind, r.seq));
                }
                // Every frame's kind census: does ANY tombstone frame exist
                // in this leaf, and how many records of each kind?
                let (mut puts, mut dels, mut deltas, mut lo, mut hi) =
                    (0u32, 0u32, 0u32, u64::MAX, 0u64);
                for i in 0..view.len() {
                    let r = view.record(i);
                    match r.kind {
                        squeezefs::meta_backend::kv::record::RecordKind::Put => puts += 1,
                        squeezefs::meta_backend::kv::record::RecordKind::Delete => dels += 1,
                        _ => deltas += 1,
                    }
                    lo = lo.min(r.seq);
                    hi = hi.max(r.seq);
                }
                census.push(format!(
                    "frame{fi}: puts {puts} dels {dels} deltas {deltas} seqs [{lo}, {hi}]"
                ));
            }
            eprintln!("ATTRIBUTION per-frame census of {addr:#x} (newest first): {census:#?}");
            // Every frame in the extent RAW (past the walk's stop too):
            // offset, node_seq_at_write, appender id, g.
            let raw = squeezefs::uring_fs::read_at(kv.device_path(), addr, node_size)
                .await
                .unwrap();
            let mut frames: Vec<String> = Vec::new();
            let mut pos = 4096usize;
            while pos + 40 <= raw.len() {
                let h = &raw[pos..pos + 40];
                let magic = u32::from_le_bytes(h[0..4].try_into().unwrap());
                if magic == 0 {
                    break;
                }
                let nsw = u64::from_le_bytes(h[8..16].try_into().unwrap());
                let padded = u32::from_le_bytes(h[16..20].try_into().unwrap()) as usize;
                let blen = u32::from_le_bytes(h[20..24].try_into().unwrap());
                let app = u32::from_le_bytes(h[24..28].try_into().unwrap());
                let g = u32::from_le_bytes(h[28..32].try_into().unwrap());
                frames.push(format!(
                    "@{pos}: seq {nsw}{} padded {padded} bset {blen} appender {app} g {g}",
                    if nsw == leaf.node_seq() {
                        "(SAME)"
                    } else {
                        "(OTHER)"
                    }
                ));
                if padded == 0 || frames.len() > 64 {
                    break;
                }
                pos += padded;
            }
            eprintln!("ATTRIBUTION raw frames of {addr:#x}: {frames:#?}");
            // The release's own words for the slot: tree 0's state (the
            // root the departing holder named) and the recorded tail of
            // THIS leaf (what the holder said the log ended at).
            let slot = leaf.forest_slot();
            let (t0_state, recorded_tail) = match slot {
                Some(s) => (
                    tree0_state(kv, s).await,
                    kv.slot_tails(s).await.ok().flatten().map(|(g, tails)| {
                        (
                            g,
                            tails.iter().find(|(a, _)| *a == addr).map(|(_, t)| *t),
                            tails.len(),
                        )
                    }),
                ),
                None => (None, None),
            };
            eprintln!(
                "ATTRIBUTION {name} in dir {dir}: leaf {addr:#x} (slot {:?}, node_seq {}, \
                         level {}, tail {} of {}) RAM {ram}; DEVICE frames (newest \
                         first) {disk:?}; residue past the walk: {}; tree 0 {:?}; recorded \
                         (g, tail of this leaf, tails) {:?}; tree root {:?}",
                slot,
                leaf.node_seq(),
                leaf.level(),
                loaded.tail_offset(),
                node_size,
                squeezefs::meta_backend::kv::node::residue_seq_ceiling(
                    &squeezefs::uring_fs::read_at(kv.device_path(), addr, node_size)
                        .await
                        .unwrap()
                ),
                t0_state,
                recorded_tail,
                tree.root()
            );
        }
    }
}

/// **A join whose manager cannot be REACHED is its own class** (PR 13,
/// defect 26 — found by `sym-crash` round 1 from zero: the successor
/// remounted while the killed manager was still exiting — its
/// heartbeat-fresh claim named it LIVE, its pid not yet provably dead —
/// dialed the dying listener for `JoinAppender`, got `Connection reset by
/// peer` and the mount REFUSED as if the manager had refused it). The
/// joined door's dial (and the join call itself, the open's first act)
/// answer `KvError::ManagerUnreachable` — errno `EHOSTUNREACH`, which
/// `meta_backend::join_dial_failed` keys on — so the mount path re-reads
/// the join target once and walks the D0 ladder when the manager is no
/// longer live-looking; a manager that REFUSES the join (`Busy`) is not
/// that class. Pinned against a closed port: nothing joined, nothing
/// written, the volume mounts as the manager afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_at_an_unreachable_manager_is_the_transport_class_the_mount_path_retries() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, _dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    // A listener nobody serves: bind, learn the port, drop it.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let identity = AppenderIdentity {
        node_token: 0x5150_2626,
        mount_slot: 26,
        writer_id: 0,
    };
    Knobs::armed().apply();
    let r = open_routed_meta_set_joined(
        &uris,
        &JoinedSetAdmission {
            manager_endpoint: dead.clone(),
            secret: VENUE_SECRET.to_vec(),
            peer_id: peer_of(&identity),
            identity,
        },
    )
    .await;
    Knobs::clear();
    let e = r
        .err()
        .expect("the join refuses: nobody answers at the dead address");
    assert!(
        squeezefs::meta_backend::join_dial_failed(&e),
        "the transport class (EHOSTUNREACH): {e}"
    );
    assert_eq!(e.to_errno(), libc::EHOSTUNREACH);
    // Not the class: a refusal the manager ANSWERED (the D0 posture word).
    let busy: squeezefs::error::SqueezefsError =
        squeezefs::meta_backend::kv::KvError::Busy("the manager refused".into()).into();
    assert!(!squeezefs::meta_backend::join_dial_failed(&busy));
    // Nothing of the join exists: the volume opens as the manager.
    let manager = open_under(&uris, &Knobs::armed()).await;
    assert_eq!(manager.volumes[0].appenders_public().unwrap().live(), 1);
    shutdown(&manager).await;
}

/// **A token READER follows a `NotHolder` redirect once** (PR 13, defect
/// 28 — found by `sym-storm` round 4 from zero: a joiner rejoined, its
/// slots granted at `g + 1`, its new storm directory's first acked file
/// resolved at the reader `EIO` — `NotHolderRedirect { object, holder: 2
/// }` — for the whole poll interval until the reader's tree 0 caught up;
/// R-SYM-4's "clients aware, 2 s is too long" read back as an I/O error
/// at every grant). The redirect's `holder` is the LEASE's word, fresher
/// than any ledger record: the reader dials that holder's plane through
/// its per-holder binding and serves — exact — instead of failing closed;
/// the writer's divert had this since PR 12b round 3. Pinned: the joiner's
/// slots are granted AFTER the reader's last poll; its file resolves at
/// the reader without a poll (RED: `EIO`), `dlm_token_reader_redirects_
/// followed` +1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reader_follows_a_not_holder_redirect_to_the_lessee() {
    use squeezefs::meta_ship::token_plane::{test_reader_redirects_followed, TokenClientConfig};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // The manager's TOKEN service (the reader's default plane; the join
    // venue above serves the manager verbs alone).
    let mtokens = DaemonVenue::stand_up(&manager, true, "manager").await;
    mvol.checkpoint_now().await.expect("checkpoint");
    // The reader: armed at the manager's token venue, polled ONCE — its
    // tree 0 knows no joiner.
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    rv.arm_reader_revalidation(None).expect("arms");
    rv.revalidate_reader().await.expect("poll");
    let default = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: mtokens.endpoint.clone(),
            secret: VENUE_SECRET.to_vec(),
            client_id: "pr13-redirect-reader".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    reader
        .getattr(shared)
        .await
        .expect("served under a token from the manager");
    assert_eq!(default.stats().grants, 1);
    // The joiner joins NOW — its 64 slots granted after the reader's
    // poll — and creates a file in its rotor (a cross-owner create into
    // the manager's directory: the child mints in the creator's slot).
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    rv.bind_reader_holder_endpoint(1, &j1venue.endpoint);
    let files = create_files(&j1, shared, "fresh", 1).await;
    let fresh = files[0].1;
    assert!(
        j1.volumes[0]
            .slot_leases()
            .expect("armed")
            .gate
            .is_leased(slot_of_global(&j1, fresh)),
        "premise: the file lives in the joiner's slot"
    );
    let followed0 = test_reader_redirects_followed();
    // No poll in between: the reader's tree 0 still says the slot is
    // nobody's, its default plane asks the manager, the manager answers
    // NotHolder { 1 } — followed to the joiner's plane.
    let attrs = reader
        .getattr(fresh)
        .await
        .expect("served under a token from the JOINER via the redirect (RED: EIO)");
    assert_eq!(attrs.ino, fresh);
    assert_eq!(
        test_reader_redirects_followed(),
        followed0 + 1,
        "one redirect followed"
    );
    for v in &reader.volumes {
        v.shutdown().await.unwrap();
    }
    shutdown(&j1).await;
    j1venue.tear_down();
    mtokens.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

/// Move `slot`'s root by a forced compaction on `jvol` (the storm's shape
/// is a lazy mint or a split — any move leaves the root unpublished until
/// a durable home names it); the joiner's grant refills at its cadence.
async fn compact_root_of(
    jvol: &KvMetaBackend,
    slot: ForestSlot,
) -> squeezefs::meta_backend::kv::tree::RootPtr {
    let before = jvol.slot_tree(slot).expect("the joiner's tree").root();
    let mut attempts = 0;
    loop {
        match jvol.defrag_compact_nodes(&[(0, before.addr)]).await {
            Ok(n) => {
                assert_eq!(n, 1, "slot {slot}'s root compacts");
                break;
            }
            Err(squeezefs::meta_backend::kv::KvError::GrantExhausted { .. }) if attempts < 8 => {
                attempts += 1;
                jvol.checkpoint_now().await.unwrap();
            }
            Err(e) => panic!("slot {slot}: {e}"),
        }
    }
    let moved = jvol.slot_tree(slot).expect("the joiner's tree").root();
    assert_ne!(moved, before, "slot {slot}'s root moved");
    moved
}

/// **§4.4af — a PAGE-published root that falls off the page keeps no
/// durable home, and the lessee's death loses the slot whole** (PR 13b;
/// the `sym-storm` round-4 acked-writes loss: 15 fsynced files of writer
/// m60 at a 128-name stride — every inode the rotor round-robin minted
/// into ONE slot, 3549 — absent at the manager after it recovered m60's
/// 14 regions; `recovery of appender 6: slot 3549 has NO page entry — the
/// grant-time root 0x0 (seq 0) stands`; `slot_roots_shipped` 18 of the 19
/// overflow slots on that volume).
///
/// The law as built before this pin: a LEASED slot's root rides its
/// lessee's page (KD-SYM-3); a region past `SLOT_PAGE_BUDGET` ships the
/// roots the page cannot hold to tree 0 (`PublishRoots`, F9); a root the
/// page NAMED is `published` and its floor lifts. The hole: the page cut
/// is DYNAMIC — the page named the 108 lowest slots, so a later first
/// touch of a LOWER slot pushed a page-published slot INTO the overflow.
/// Its `published` mark (the page's) still equalled its live root, so
/// `roots_to_publish` excluded it, `PublishRoots` never shipped it, the
/// page no longer named it and the ring tail had long passed its records:
/// the root was named NOWHERE durable. The lessee's death recovered the
/// slot at tree 0's grant-time root — every record flushed under the
/// current root, and every one the tail passed, was gone.
///
/// The law as of review round 1, Issue 2 (the ONE page writer's
/// invariant): a publication remembers its HOME (page / tree 0) and the
/// page ranks by what a slot loses if dropped — a PAGE-homed root
/// outranks every unpublished one, which outranks every tree-0-named one
/// — so twenty LOWER first touches with moved roots (unpublished, low
/// page slots) evict NO page-homed root: the page keeps naming all 108
/// (107 upper + the rotor holding every inode), the lower slots stay off
/// it and their stale roots ship to tree 0 (`PublishRoots`), and nothing
/// is demoted (a page-homed publication is demoted only when a page
/// writer must drop it — `prepare_page_entries` publishes it first; the
/// handover and first-touch-race shapes are the two pins after this one).
/// Pinned at the durable level (every slot off the page has its current
/// root in tree 0; every page-homed slot is still named) and at the
/// acked-writes level (the lessee's death loses nothing; a manager
/// remount opens every file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_published_root_pushed_off_the_page_by_a_lower_first_touch_rides_tree_zero_before_the_lessee_dies(
) {
    use squeezefs::meta_backend::kv::appender::{
        forest_slot_of_page_slot, SlotEntryState, SLOT_PAGE_BUDGET,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Every cycle below is this pin's (see `ParkedCadence`).
    let _cadence = ParkedCadence::arm();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let native = mvol.appender_stats().unwrap().native_slot;
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join_knobs(&knobs, &uris, &venue, &mvol, 93).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let plane = jvol.slot_leases().expect("armed");
    let page_slots =
        |page: &squeezefs::meta_backend::kv::appender::AppenderPage| -> Vec<ForestSlot> {
            page.slots
                .iter()
                .filter(|e| e.state == SlotEntryState::Live)
                .map(|e| forest_slot_of_page_slot(e.slot, native))
                .collect()
        };

    // 107 UPPER seeded slots first-touched (one file each — the inodes
    // land in the joiner's rotor slot) and every one's root MOVED: the
    // joiner holds 108 with the rotor, the page names all 108 moved roots
    // — 108 PAGE publications, tree 0 keeping the grant-time roots.
    let low = dirs.len() - (SLOT_PAGE_BUDGET - 1);
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    let mut grant_roots = std::collections::BTreeMap::new();
    for (i, d) in dirs.iter().enumerate().skip(low) {
        files.push((*d, create_files(&joiner, *d, &format!("s{i:03}-"), 1).await));
        match tree0_state(&mvol, slots[i]).await {
            Some(SlotState::Leased {
                appender_id, root, ..
            }) => {
                assert_eq!(appender_id, jid);
                grant_roots.insert(slots[i], root);
            }
            other => panic!("slot {}: {other:?}", slots[i]),
        }
    }
    assert_eq!(plane.gate.leased_count(), SLOT_PAGE_BUDGET);
    jvol.checkpoint_now().await.unwrap();
    let mut moved_roots = std::collections::BTreeMap::new();
    for slot in slots.iter().skip(low) {
        moved_roots.insert(*slot, compact_root_of(&jvol, *slot).await);
    }
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &jvol, jid)
        .await
        .expect("the joiner's page");
    let named = page_slots(&page);
    assert_eq!(named.len(), SLOT_PAGE_BUDGET, "the page names its budget");
    for slot in slots.iter().skip(low) {
        let entry = page
            .slots
            .iter()
            .find(|e| forest_slot_of_page_slot(e.slot, native) == *slot)
            .unwrap_or_else(|| panic!("the page names slot {slot}"));
        assert_eq!(
            entry.root, moved_roots[slot],
            "the page's word is the moved root"
        );
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Leased { root, .. }) => assert_eq!(
                root, grant_roots[slot],
                "tree 0 keeps slot {slot}'s grant-time root: the page is its home"
            ),
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    let homed = page_homed_entries(&mvol, &page, native).await;
    assert_eq!(
        homed.len(),
        SLOT_PAGE_BUDGET,
        "premise: every named slot is PAGE-homed — {homed:?}"
    );
    // More acked records under the highest seeded slot's moved root, the
    // ring COVERED past them: their only durable home is the page's word.
    let victim = *slots.last().unwrap();
    let victim_dir = *dirs.last().unwrap();
    let after = create_files(&joiner, victim_dir, "after-", 40).await;
    jvol.checkpoint_now().await.unwrap();
    jvol.checkpoint_now().await.unwrap();
    let (head, upto) = jvol.region_ring_window(jid).expect("the joined region");
    assert_eq!(head, upto, "covered (head {head}, reusable_upto {upto})");
    let demoted0 = squeezefs::meta_backend::kv::META_KV_FOREST_PAGE_PUBLICATIONS_DEMOTED
        .load(std::sync::atomic::Ordering::Relaxed);

    // Twenty LOWER slots first-touched, each root MOVED too (unpublished
    // — low page slots, the first build's cut moved down past the 20
    // highest page-homed slots). The LAW: a page-homed root outranks an
    // unpublished one — the page still names every page-homed slot, the
    // lower slots stay off it with their current roots shipped to tree 0
    // — a slot is never off every durable home.
    let mut lower: Vec<(u64, Vec<(String, u64)>)> = Vec::new();
    for (i, d) in dirs.iter().enumerate().take(low) {
        lower.push((*d, create_files(&joiner, *d, &format!("l{i:03}-"), 1).await));
        moved_roots.insert(slots[i], compact_root_of(&jvol, slots[i]).await);
    }
    jvol.checkpoint_now().await.unwrap();
    jvol.checkpoint_now().await.unwrap();
    let named = page_slots(
        &page_of(&uris[0], &jvol, jid)
            .await
            .expect("the joiner's page"),
    );
    for (slot, _) in &homed {
        assert!(
            named.contains(slot),
            "page-homed slot {slot} is still named — an unpublished first touch never evicts it"
        );
    }
    assert!(named.contains(&victim), "the highest page-homed slot stays");
    let demoted = squeezefs::meta_backend::kv::META_KV_FOREST_PAGE_PUBLICATIONS_DEMOTED
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        demoted, demoted0,
        "nothing left the page, nothing was demoted"
    );
    for slot in &slots {
        if named.contains(slot) {
            continue;
        }
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Leased { root, .. }) => assert_eq!(
                root, moved_roots[slot],
                "slot {slot} is off the page: tree 0 names its current root (RED on the base: \
                 the grant-time root — the page's publication mark survived the page)"
            ),
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    assert!(!jvol.is_failed());
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    // The lessee DIES; the manager recovers its region: every acked file
    // resolves (RED on the base: the recovery installs the grant-time root
    // of every slot that left the page — the storm's stride-128
    // population).
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 33, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&manager, victim_dir, &after).await;
    for (d, fs) in files.iter().chain(lower.iter()) {
        assert_all_resolve(&manager, *d, fs).await;
    }
    assert_must_stay_zero(&mvol, "manager after the recovery");

    // A manager remount opens every one of them.
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let again = open_under(&uris, &knobs).await;
    assert_all_resolve(&again, victim_dir, &after).await;
    for (d, fs) in files.iter().chain(lower.iter()) {
        assert_all_resolve(&again, *d, fs).await;
    }
    shutdown(&again).await;
    drop(again);
    fsck_clean(&uris).await;
}

/// The slots `page` names whose CURRENT root tree 0 does NOT name — the
/// page is their only durable home (page-homed) — with the page's root
/// word, in page-slot order. The native slot rides the fixed ledger and is
/// never counted.
async fn page_homed_entries(
    mvol: &KvMetaBackend,
    page: &squeezefs::meta_backend::kv::appender::AppenderPage,
    native: u16,
) -> Vec<(ForestSlot, squeezefs::meta_backend::kv::tree::RootPtr)> {
    use squeezefs::meta_backend::kv::appender::{forest_slot_of_page_slot, SlotEntryState};
    let mut out = Vec::new();
    for e in &page.slots {
        if e.root.addr == 0
            || !(e.state == SlotEntryState::Live || e.state == SlotEntryState::Releasing)
        {
            continue;
        }
        let slot = forest_slot_of_page_slot(e.slot, native);
        if slot == squeezefs::meta_backend::kv::record::NATIVE_FOREST_SLOT {
            continue;
        }
        let tree0_root = match tree0_state(mvol, slot).await {
            Some(SlotState::Leased { root, .. }) | Some(SlotState::Unleased { root, .. }) => {
                Some(root)
            }
            _ => None,
        };
        if tree0_root != Some(e.root) {
            out.push((slot, e.root));
        }
    }
    out
}

/// **§4.4af, the HANDOVER shape (review round 1, Issue 2 — the ONE page
/// writer's invariant): releasing an OVERFLOW slot of a region holding
/// its whole page budget in page-homed roots publishes the slot the
/// `Releasing` entry pushes off the page BEFORE the page drops it.** The
/// joiner holds 108 page-homed slots — 107 first-touched slots whose
/// roots moved plus its rotor slot, which holds every created file's
/// INODE (a child of a parent the creator did not lease yet mints into
/// the rotor — the storm's own shape: one rotor slot's population at a
/// 128-name stride); the page names every one, tree 0 keeps the
/// grant-time roots — plus ONE lower slot whose root never moved: tree 0
/// names it, the page leaves it off. The cadence releases that lower
/// slot: the handover's step 3 puts it on the page in `Releasing` (the
/// two-homes law), the 109th entry — the first build's step 3 re-ranked
/// against the live lease set and cut the highest page slot (the rotor)
/// with its publication mark intact: named on no page, on tree 0 at the
/// grant-time root, its records below the ring tail. The holder dies
/// right after that page write (`TEST_HANDOVER_HOLD_AFTER_PAGE`); the
/// manager's recovery installed the grant-time root and lost every file's
/// inode. The law: every page write of a leased region takes its entries
/// from `prepare_page_entries`, which ships the page-homed slot's root to
/// tree 0 (`PublishRoots`, awaited) before the page stops naming it —
/// pinned at the durable level (tree 0 names the pushed slot's current
/// root once the page no longer does) and at the acked-writes level (the
/// death loses nothing; a manager remount opens every file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releasing_an_overflow_slot_of_a_page_full_region_publishes_the_slot_it_pushes_off_first() {
    use squeezefs::meta_backend::kv::appender::{
        forest_slot_of_page_slot, SlotEntryState, SLOT_PAGE_BUDGET,
    };
    use squeezefs::meta_backend::kv::backend::TEST_HANDOVER_HOLD_AFTER_PAGE;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Every cycle below is this pin's (see `ParkedCadence`).
    let _cadence = ParkedCadence::arm();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let native = mvol.appender_stats().unwrap().native_slot;
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    enroll_manager(&mvol, &venue.endpoint()).await;

    let joiner = join_knobs(&knobs, &uris, &venue, &mvol, 94).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let page_slots =
        |page: &squeezefs::meta_backend::kv::appender::AppenderPage| -> Vec<ForestSlot> {
            page.slots
                .iter()
                .filter(|e| e.state == SlotEntryState::Live || e.state == SlotEntryState::Releasing)
                .map(|e| forest_slot_of_page_slot(e.slot, native))
                .collect()
        };

    // 108 page-homed slots: 107 upper seeded slots first-touched (one
    // file each — the inodes land in the joiner's ROTOR slot), every
    // upper root MOVED; the page names all 107 and the rotor.
    let low = dirs.len() - (SLOT_PAGE_BUDGET - 1);
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate().skip(low) {
        files.push((*d, create_files(&joiner, *d, &format!("s{i:03}-"), 1).await));
    }
    jvol.checkpoint_now().await.unwrap();
    for slot in slots.iter().skip(low) {
        compact_root_of(&jvol, *slot).await;
    }
    jvol.checkpoint_now().await.unwrap();
    // ONE lower slot first-touched, its root NOT moved: tree 0 names its
    // current root, so it is the one the page leaves off (the overflow).
    let overflow_slot = slots[0];
    let overflow_dir = dirs[0];
    let overflow_files = create_files(&joiner, overflow_dir, "o-", 1).await;
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &jvol, jid)
        .await
        .expect("the joiner's page");
    let named = page_slots(&page);
    assert_eq!(named.len(), SLOT_PAGE_BUDGET, "the page names its budget");
    assert!(
        !named.contains(&overflow_slot),
        "premise: the unmoved lower slot is the page's overflow"
    );
    for slot in slots.iter().skip(low) {
        assert!(
            named.contains(slot),
            "the page names page-homed slot {slot}"
        );
    }
    let homed = page_homed_entries(&mvol, &page, native).await;
    assert_eq!(
        homed.len(),
        SLOT_PAGE_BUDGET,
        "premise: every named slot is PAGE-homed (tree 0 names another root) — {homed:?}"
    );
    let overflow_root = jvol.slot_tree(overflow_slot).unwrap().root();
    match tree0_state(&mvol, overflow_slot).await {
        Some(SlotState::Leased { root, .. }) => {
            assert_eq!(root, overflow_root, "tree 0 names the overflow slot's root")
        }
        other => panic!("slot {overflow_slot}: {other:?}"),
    }
    // The victim: the highest PAGE SLOT among the page-homed — the one a
    // 109th entry at the page's head pushes off (the rotor, holding every
    // file's inode). The ring is COVERED past its records: the page's word
    // is their only home.
    let (victim, victim_root) = *homed
        .iter()
        .max_by_key(|(s, _)| {
            squeezefs::meta_backend::kv::appender::page_slot_of_forest_slot(*s, native).unwrap()
        })
        .unwrap();
    jvol.checkpoint_now().await.unwrap();
    let (head, upto) = jvol.region_ring_window(jid).expect("the joined region");
    assert_eq!(head, upto, "covered (head {head}, reusable_upto {upto})");
    assert_eq!(jvol.slot_tree(victim).unwrap().root(), victim_root);

    // The handover of the overflow slot, the holder dying right after its
    // step-3 page write.
    let demoted0 =
        squeezefs::meta_backend::kv::META_KV_FOREST_PAGE_PUBLICATIONS_DEMOTED.load(Relaxed);
    TEST_HANDOVER_HOLD_AFTER_PAGE.store(true, Relaxed);
    let out = jvol.release_slot_handover(jid, overflow_slot).await;
    TEST_HANDOVER_HOLD_AFTER_PAGE.store(false, Relaxed);
    assert!(
        out.is_err(),
        "the seam ends the handover after the page write"
    );
    assert!(
        squeezefs::meta_backend::kv::META_KV_FOREST_PAGE_PUBLICATIONS_DEMOTED.load(Relaxed)
            > demoted0,
        "the pushed slot's page-homed publication was demoted before the page dropped it \
         (meta_kv_forest_page_publications_demoted)"
    );
    let page = page_of(&uris[0], &jvol, jid)
        .await
        .expect("the joiner's page after step 3");
    let named = page_slots(&page);
    assert!(
        named.contains(&overflow_slot),
        "step 3 put the releasing slot on the page"
    );
    assert!(
        !named.contains(&victim),
        "premise: the 109th entry pushed the highest page slot ({victim}) off"
    );
    // The LAW (RED on the first build): a slot the page no longer names
    // has its CURRENT root in tree 0.
    match tree0_state(&mvol, victim).await {
        Some(SlotState::Leased { root, .. }) => assert_eq!(
            root, victim_root,
            "slot {victim} left the page at the handover's step 3: tree 0 must name its \
             current root first (RED: the grant-time root stands — the Releasing entry \
             evicted a page-homed slot un-published)"
        ),
        other => panic!("slot {victim}: {other:?}"),
    }
    assert!(!jvol.is_failed());

    // The lessee DIES after step 3; the manager recovers its region:
    // every acked file resolves (RED: the victim's grant-time root — every
    // inode gone).
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 34, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    assert_all_resolve(&manager, overflow_dir, &overflow_files).await;
    for (d, fs) in &files {
        assert_all_resolve(&manager, *d, fs).await;
    }
    assert_must_stay_zero(&mvol, "manager after the recovery");

    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let again = open_under(&uris, &knobs).await;
    assert_all_resolve(&again, overflow_dir, &overflow_files).await;
    for (d, fs) in &files {
        assert_all_resolve(&again, *d, fs).await;
    }
    shutdown(&again).await;
    drop(again);
    fsck_clean(&uris).await;
}

/// **§4.4af, the FIRST-TOUCH-DURING-THE-CYCLE shape (review round 1,
/// Issue 2 — the ONE page writer's invariant on the MANAGER's own
/// region).** The manager holds its page budget in page-homed roots (107
/// first-touched slots whose roots moved + its rotor slot with every
/// file's inode; page 0 names every one, tree 0 keeps the grant-time
/// roots). Its checkpoint cycle is PARKED between its tree-0 publication
/// (`publish_forest_roots`, which found nothing off the page) and its page
/// write (`TEST_CHECKPOINT_PARK_BEFORE_PAGES`), and inside that window the
/// manager first-touches a LOWER slot with no tree yet and mints it — the
/// manager's own first touch takes no SMO mutex, so it lands under the
/// parked cycle exactly as the storm's load does. The first build's page
/// write re-ranked against the live lease set: the fresh unpublished slot
/// (a low page slot) ranked ahead of the page-homed class and evicted the
/// highest page slot (the rotor) with its publication mark intact — a
/// root on no page, tree 0 at root 0, its records below the tail. The
/// manager DIES after that page write; its successor (re-open, own
/// residue) adopts page 0's roots and tree 0's for the rest — and lost
/// every file's inode. The law: a page-homed root outranks every
/// unpublished one, and a page-homed slot the page still cannot name is
/// published into tree 0 by the page writer before the page drops it —
/// pinned at the durable level and at the acked-writes level.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_touch_between_the_cycles_publication_and_its_page_write_evicts_no_page_homed_root()
{
    use squeezefs::meta_backend::kv::appender::{
        forest_slot_of_page_slot, page_slot_of_forest_slot, SlotEntryState, SLOT_PAGE_BUDGET,
    };
    use squeezefs::meta_backend::kv::checkpoint::{
        test_checkpoint_parked_count, test_release_checkpoint_park,
        TEST_CHECKPOINT_PARK_BEFORE_PAGES,
    };
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Every cycle below is this pin's (see `ParkedCadence`).
    let _cadence = ParkedCadence::arm();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let native = mvol.appender_stats().unwrap().native_slot;
    let page_slots =
        |page: &squeezefs::meta_backend::kv::appender::AppenderPage| -> Vec<ForestSlot> {
            page.slots
                .iter()
                .filter(|e| e.state == SlotEntryState::Live)
                .map(|e| forest_slot_of_page_slot(e.slot, native))
                .collect()
        };

    // The manager's page budget in page-homed roots: 107 UPPER seeded
    // slots first-touched (one file each — the inodes in the rotor) with
    // their roots moved, plus the rotor (its native slot's root is the
    // ledger's — the entry the full page drops).
    let first = dirs.len() - (SLOT_PAGE_BUDGET - 1);
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate().skip(first) {
        files.push((
            *d,
            create_files(&manager, *d, &format!("s{i:03}-"), 1).await,
        ));
    }
    mvol.checkpoint_now().await.unwrap();
    for slot in slots.iter().skip(first) {
        compact_root_of(&mvol, *slot).await;
    }
    mvol.checkpoint_now().await.unwrap();
    mvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &mvol, 0).await.expect("page 0");
    let named = page_slots(&page);
    assert_eq!(named.len(), SLOT_PAGE_BUDGET, "page 0 names its budget");
    for slot in slots.iter().skip(first) {
        assert!(named.contains(slot), "page 0 names page-homed slot {slot}");
    }
    let homed = page_homed_entries(&mvol, &page, native).await;
    assert_eq!(
        homed.len(),
        SLOT_PAGE_BUDGET,
        "premise: every named slot is PAGE-homed — {homed:?}"
    );
    let (victim, victim_root) = *homed
        .iter()
        .max_by_key(|(s, _)| page_slot_of_forest_slot(*s, native).unwrap())
        .unwrap();
    let ring0 = mvol.journal_ring().core();
    assert_eq!(
        ring0.head(),
        ring0.reusable_upto(),
        "ring 0 covered (head {}, reusable_upto {})",
        ring0.head(),
        ring0.reusable_upto()
    );

    // The cycle parked between its publication and its page write; the
    // manager's first touch + MINT of a slot with no tree lands inside.
    let parked0 = test_checkpoint_parked_count();
    TEST_CHECKPOINT_PARK_BEFORE_PAGES.store(true, Relaxed);
    let cycle = {
        let mvol = Arc::clone(&mvol);
        tokio::spawn(async move { mvol.checkpoint_now().await })
    };
    wait_until("the cycle parked before its page write", || {
        test_checkpoint_parked_count() > parked0
    })
    .await;
    // A slot LOWER than every page-homed one (a low page slot — the
    // first build's rank sorted it ahead of them), with no tree yet.
    let plane = mvol.slot_leases().expect("armed");
    let fresh_slot: ForestSlot = (2..OVERFLOW_SEED_BASE)
        .find(|s| {
            mvol.slot_tree(*s).is_none()
                && !plane.gate.is_leased(*s)
                && page_slot_of_forest_slot(*s, native).is_ok()
        })
        .expect("a low slot with no tree");
    let minted = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        seed_dir_in_slot(&manager, 0, fresh_slot, "parked-mint"),
    )
    .await
    .expect("the manager's own first touch + mint lands under the parked cycle");
    let fresh_root = mvol.slot_tree(fresh_slot).expect("the minted tree").root();
    assert_ne!(fresh_root.addr, 0, "the mint gave the slot a root");
    test_release_checkpoint_park();
    cycle.await.unwrap().unwrap();

    // The LAW (RED on the first build): the fresh unpublished slot never
    // evicts a page-homed root — every page-homed slot is still named,
    // and whatever page 0 does not name has its current root in tree 0
    // or stands unpublished behind its floor.
    let page = page_of(&uris[0], &mvol, 0).await.expect("page 0");
    let named = page_slots(&page);
    for (slot, root) in &homed {
        if named.contains(slot) {
            continue;
        }
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Leased { root: t0, .. }) => assert_eq!(
                t0, *root,
                "slot {slot} left page 0 at the cycle's page write: tree 0 must name its \
                 current root first (RED: the fresh mint evicted the page-homed slot {victim} \
                 with its publication mark intact)"
            ),
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    assert_eq!(
        mvol.slot_tree(victim).unwrap().root(),
        victim_root,
        "the victim's root did not move on its own"
    );
    assert!(!mvol.is_failed());

    // The manager DIES after that page write; its successor opens the
    // volume (own residue: page 0's roots, tree 0's for the rest, the
    // window replayed) and every acked file resolves.
    drop(mvol);
    drop(manager);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let again = open_under_retry(&uris, &knobs)
        .await
        .expect("the successor's open");
    for (d, fs) in &files {
        assert_all_resolve(&again, *d, fs).await;
    }
    again
        .getattr(minted)
        .await
        .expect("the parked mint's directory resolves too");
    assert_must_stay_zero(&again.volumes[0], "successor");
    shutdown(&again).await;
    drop(again);
    fsck_clean(&uris).await;
}

/// **§4.4af through the predicate's gap (review round 2, Issue 12): a
/// page-homed slot whose root MOVES in the parked cycle's flush pass is
/// still the page's to keep.** The manager holds its page budget in
/// page-homed roots (107 first-touched slots whose roots moved + its rotor
/// slot holding every file's inode; page 0 names every one at its root
/// `R1`, tree 0 keeps the grant-time roots). The rotor's one leaf is then
/// loaded with more records than a node holds (xattrs of a file whose
/// inode lives there — the same tree), the cycle is PARKED between its
/// tree-0 publication and its page write, and inside the window (a) the
/// flush pass SPLITS the rotor's root — `R1 → R2`, unpublished, the split's
/// floor covering only the records since the move — and (b) the manager
/// first-touches + mints a LOWER slot (a rank-2 competitor at a low page
/// slot). The first predicate read a page-homed publication at an OLDER
/// root as "unpublished" — rank 2 beside the never-published — so the
/// competitor cut the rotor off the page, `off_page_homed` excluded it,
/// nothing shipped its root to tree 0, the page dropped `R1`; every record
/// flushed under `R1` (the 107 inodes) sat below the tail with tree 0 at
/// root 0 — the manager's death lost them whole. The law: a PAGE-homed
/// publication at ANY root ranks first (`SlotTrees::page_homed`); the
/// rotor stays named at `R2`, or — where a rank-0 entry pushes it off —
/// ships `R2` to tree 0 before the page drops it. Pinned at the durable
/// level (every page-homed slot is still named, or tree 0 names its current
/// root) and at the acked-writes level (the death loses nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_split_in_the_parked_cycles_flush_pass_never_drops_the_slots_page_home() {
    use squeezefs::meta_backend::kv::appender::{
        forest_slot_of_page_slot, page_slot_of_forest_slot, SlotEntryState, SLOT_PAGE_BUDGET,
    };
    use squeezefs::meta_backend::kv::checkpoint::{
        test_checkpoint_parked_count, test_release_checkpoint_park,
        TEST_CHECKPOINT_PARK_BEFORE_PAGES,
    };
    use squeezefs::meta_backend::kv::META_KV_NODE_SPLITS;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // Every cycle below is this pin's (see `ParkedCadence`).
    let _cadence = ParkedCadence::arm();
    let (uris, dirs, slots) = overflow_seeded_volume(dir.path()).await;
    let knobs = Knobs::armed().mint_slots("1");
    let manager = open_under(&uris, &knobs).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let native = mvol.appender_stats().unwrap().native_slot;
    let page_slots =
        |page: &squeezefs::meta_backend::kv::appender::AppenderPage| -> Vec<ForestSlot> {
            page.slots
                .iter()
                .filter(|e| e.state == SlotEntryState::Live)
                .map(|e| forest_slot_of_page_slot(e.slot, native))
                .collect()
        };

    // 107 upper seeded slots first-touched (one file each — the inodes in
    // the rotor) with their roots moved, plus the rotor: page 0's budget in
    // page-homed roots.
    let first = dirs.len() - (SLOT_PAGE_BUDGET - 1);
    let mut files: Vec<(u64, Vec<(String, u64)>)> = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate().skip(first) {
        files.push((
            *d,
            create_files(&manager, *d, &format!("s{i:03}-"), 1).await,
        ));
    }
    mvol.checkpoint_now().await.unwrap();
    for slot in slots.iter().skip(first) {
        compact_root_of(&mvol, *slot).await;
    }
    mvol.checkpoint_now().await.unwrap();
    mvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &mvol, 0).await.expect("page 0");
    let named = page_slots(&page);
    assert_eq!(named.len(), SLOT_PAGE_BUDGET, "page 0 names its budget");
    let homed = page_homed_entries(&mvol, &page, native).await;
    assert_eq!(
        homed.len(),
        SLOT_PAGE_BUDGET,
        "premise: every named slot is PAGE-homed — {homed:?}"
    );
    // The victim: the ROTOR — the highest page slot among the page-homed,
    // and the tree every created file's inode lives in.
    let (victim, r1) = *homed
        .iter()
        .max_by_key(|(s, _)| page_slot_of_forest_slot(*s, native).unwrap())
        .unwrap();
    let a_file = files[0].1[0].1;
    let (_v, a_file_local) = manager.route_ino(a_file);
    assert_eq!(
        squeezefs::meta_backend::kv::record::forest_slot_of_ino(a_file_local),
        victim,
        "premise: the files' inodes live in the victim's tree (the rotor)"
    );
    let ring0 = mvol.journal_ring().core();
    assert_eq!(
        ring0.head(),
        ring0.reusable_upto(),
        "ring 0 covered (head {}, reusable_upto {})",
        ring0.head(),
        ring0.reusable_upto()
    );

    // More records into the victim's one leaf than a node holds — under a
    // file whose inode lives there — so its root SPLITS (`R1 → R2`) with
    // page 0 still naming `R1`. WHICH pass splits it is the schedule's:
    // the cadence is parked (`ParkedCadence`), but the appends cross §4.6
    // pt 1's threshold trigger, and the SMO task's wake-driven maintenance
    // pass may run the split at once — or lose the race to the cycle this
    // pin parks, whose flush pass then splits it. The law holds in both
    // (the batch `task check` on `13a009dd` read the first premise, "the
    // appends left the root where the page named it", RED on the early
    // split — the premise's window, never the law's), so the premise is
    // taken where it is invariant: at the parked cycle's page write the
    // root has moved and page 0 still says `R1`.
    let splits0 = META_KV_NODE_SPLITS.load(Relaxed);
    let wide = vec![0x5au8; 15 * 1024];
    for k in 0..12 {
        mvol.setxattr_internal(a_file_local, &format!("user.wide{k}"), &wide)
            .await
            .expect("an xattr into the victim's leaf");
    }
    let before_park = mvol.slot_tree(victim).unwrap().root();

    // The cycle parked between its publication and its page write.
    let parked0 = test_checkpoint_parked_count();
    TEST_CHECKPOINT_PARK_BEFORE_PAGES.store(true, Relaxed);
    let cycle = {
        let mvol = Arc::clone(&mvol);
        tokio::spawn(async move { mvol.checkpoint_now().await })
    };
    wait_until("the cycle parked before its page write", || {
        test_checkpoint_parked_count() > parked0
    })
    .await;
    // (a) The victim's root moved — by the threshold maintenance the
    // appends woke (`before_park != r1`) or by the parked cycle's flush
    // pass (`before_park == r1`) — and page 0, last written before the
    // appends, still names `R1`: the stale PAGE-homed shape at the page
    // write this pin controls.
    let r2 = mvol.slot_tree(victim).unwrap().root();
    assert_ne!(
        r2, r1,
        "premise: the victim's root split before the page write (before the park: \
         {before_park:?})"
    );
    assert!(
        META_KV_NODE_SPLITS.load(Relaxed) > splits0,
        "premise: a node split ran"
    );
    let page_at_park = page_of(&uris[0], &mvol, 0).await.expect("page 0");
    let word_at_park = page_at_park
        .slots
        .iter()
        .find(|e| forest_slot_of_page_slot(e.slot, native) == victim)
        .map(|e| e.root);
    assert_eq!(
        word_at_park,
        Some(r1),
        "premise: page 0 still names the victim at R1 while its live root is R2"
    );
    // (b) A lower rank-2 competitor: a slot with no tree, first-touched +
    // minted under the parked cycle.
    let plane = mvol.slot_leases().expect("armed");
    let fresh_slot: ForestSlot = (2..OVERFLOW_SEED_BASE)
        .find(|s| {
            mvol.slot_tree(*s).is_none()
                && !plane.gate.is_leased(*s)
                && page_slot_of_forest_slot(*s, native).is_ok()
        })
        .expect("a low slot with no tree");
    let minted = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        seed_dir_in_slot(&manager, 0, fresh_slot, "parked-mint"),
    )
    .await
    .expect("the manager's own first touch + mint lands under the parked cycle");
    test_release_checkpoint_park();
    cycle.await.unwrap().unwrap();

    // The LAW (RED on the `root == live` predicate): every page-homed slot
    // is still named — the stale one at its CURRENT root — or tree 0
    // names its current root.
    let page = page_of(&uris[0], &mvol, 0).await.expect("page 0");
    let named = page_slots(&page);
    for (slot, _) in &homed {
        if named.contains(slot) {
            continue;
        }
        match tree0_state(&mvol, *slot).await {
            Some(SlotState::Leased { root, .. }) => assert_eq!(
                root,
                mvol.slot_tree(*slot).unwrap().root(),
                "slot {slot} left page 0 at the cycle's page write: tree 0 must name its \
                 current root first (RED: a page-homed publication at an OLDER root ranked \
                 with the never-published and the fresh mint evicted it — R1 dropped, R0..R1's \
                 records below the tail)"
            ),
            other => panic!("slot {slot}: {other:?}"),
        }
    }
    let victim_entry = page
        .slots
        .iter()
        .find(|e| forest_slot_of_page_slot(e.slot, native) == victim);
    if let Some(e) = victim_entry {
        assert_eq!(e.root, r2, "page 0's word for the victim is the moved root");
    }
    assert!(!mvol.is_failed());

    // The manager DIES after that page write; its successor opens the
    // volume and every acked file's inode — the victim's R0..R1 records —
    // resolves, the wide xattrs beside them.
    drop(mvol);
    drop(manager);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let again = open_under_retry(&uris, &knobs)
        .await
        .expect("the successor's open");
    for (d, fs) in &files {
        assert_all_resolve(&again, *d, fs).await;
    }
    for k in 0..12 {
        assert_eq!(
            again
                .getxattr(a_file, &format!("user.wide{k}"))
                .await
                .unwrap()
                .as_deref(),
            Some(&wide[..]),
            "the split leaf's records survive too"
        );
    }
    again
        .getattr(minted)
        .await
        .expect("the parked mint's directory resolves too");
    assert_must_stay_zero(&again.volumes[0], "successor");
    shutdown(&again).await;
    drop(again);
    fsck_clean(&uris).await;
}

/// **A COLD `ls -l` of a STRIPED directory at a token reader pays ONE
/// token per stripe — and a striped ROOT adds its own `K_root` at the
/// reader's first `stat /` AFTER the map is learnt** (PR 13d — found by
/// PR 15's local functional pass of the matrix's own `sym-shared-dir-ls`
/// leg on the gated `77f4da1d`: `dlm_token_grants` read `2K + C + 3`
/// (20,131 for K = 64, C = 20,000) where every PR 13-era run read
/// `K + C + 3`; design §8 gate 3b's law is `K_D + K_root + C + [0, 4]`
/// since the adjudication). Attributed to FLEET STATE, not a commit: that
/// fleet's writers had `mkdir`ed their per-leg directories into `/` on
/// ONE fleet, which auto-striped `/` at the manager (`dir_striped_dirs 1`
/// at m0 before the leg; 0 in every `K + C + 3` run), and the kernel
/// revalidates the mount root's attrs on every path walk (every TTL is 0
/// under tokens), so the reader's `stat /` folds over the root's stripes
/// (`getattr_local` → `stripe_map_cached`, learnt by `lookup(/, D)`'s
/// `stripe_route` → `fold_striped_attrs` → `stripe_record`) and pays ONE
/// records-only grant per root stripe — the class the acceptance record's
/// §7 item 7 prices ("one grant each per holder per token lifetime").
/// PR 13b's `7b2ef9e9` reads every arithmetic below identically. The
/// fleet's shape in one process: the directory held by the manager (this
/// process's one cross-owner shipper and custody arm — both
/// process-global), its stripes SUPPLIED by two joiners over the S8 wire
/// (the remainder the manager's own), its children the manager's — the
/// ones routing into a supplied stripe shipped to that holder — two
/// metadata volumes (a directory's children mint round-robin over the
/// set), and a `-o ro` reader whose planes dial each holder's listener.
///
/// **The constant beside `K_D + K_root + C` is the WALK ORDER's.** The
/// routed verbs the FUSE handlers call: `lookup(/, D)`, `stat /`, `stat
/// D`, the paged `readdir` (the K-way merge), `lookup + stat` per child,
/// `stat D` again (the fold). Lookup-first (phases 1–2): `+3` = the root's
/// dentry-bearing token (the lookup's `stripe_route(/)` marker read), `D`'s
/// record token (the lookup's inner `getattr`), `D`'s dentry-bearing
/// re-grant (the readdir's map read — a records-only token lacks the
/// dentries). The KERNEL's order (phase 3) GETATTRs `/` BEFORE `LOOKUP(/,
/// D)` (the `default_permissions` walk): that pre-lookup `stat /` pays one
/// records-only root grant and folds NOTHING (the map is not learnt yet),
/// so a truly cold reader in kernel order reads `+4` — the law's ceiling —
/// and the fleet reads `+3` only because the harness's pre-leg `.stats`
/// snapshot (a `GETATTR(1)`) absorbs the root's records grant before the
/// leg's first reading (PR 15's `m1_pls0`: `dlm_token_grants [2, 0]`).
///
/// Phase 1 (the leg's law, lookup-first): the root unstriped — EXACTLY
/// `K_D + C + 3`. Phase 2: the root striped over `K_root ≠ K_D` stripes
/// (the discriminating shape — with `K_root = K_D` the reading equals `2K
/// + C + 3`, which a "second token per `D` stripe" theory passes too), a
/// second cold reader, lookup-first — EXACTLY `K_D + C + 3 + K_root`, and
/// the per-volume `dlm_token_cached` split puts the `K_root` extra objects
/// on the ROOT's volume alone (the fleet's own split, PR 15's and PR 13d
/// run 2's, made a law). Phase 3: the KERNEL's order on the striped root, a
/// third cold reader — EXACTLY `K_D + C + 4 + K_root`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_ls_of_a_striped_directory_at_a_token_reader_pays_one_token_per_stripe() {
    use squeezefs::meta_ship::token_plane::{reader_stats_json, TokenClientConfig};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 2).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mtokens = DaemonVenue::stand_up(&manager, true, "manager").await;
    // The manager's OWN directory, checkpointed before the joins (a
    // served `SupplyStripeIno` reads the directory's record off the
    // joiner's projection — the joiners are no custody arm here).
    let d = manager
        .create(1, "hot", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the manager's directory")
        .ino;
    let mut files = create_files(&manager, d, "pre-", 8).await;
    for v in &manager.volumes {
        v.checkpoint_now().await.expect("checkpoint");
    }
    let j1 = join(&uris, &venue, &mvol, 1).await;
    let j2 = join(&uris, &venue, &mvol, 2).await;
    let j1venue = DaemonVenue::stand_up(&j1, false, "joiner-1").await;
    let j2venue = DaemonVenue::stand_up(&j2, false, "joiner-2").await;
    for v in &manager.volumes {
        let holders = &v.slot_leases().expect("armed").holders;
        holders.set_endpoint(1, &j1venue.endpoint);
        holders.set_endpoint(2, &j2venue.endpoint);
    }
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&manager),
            "node-manager",
            VENUE_SECRET.to_vec(),
        ),
    );
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    let for_arm = Arc::clone(&sink);
    let _arm = squeezefs::data_grant::arm_slot_custody(
        &manager,
        &squeezefs::cowriter::node_member_id().expect("this node's member id"),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&for_arm) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    );
    // The flip at the holder over `k` stripes: stripes 0 and 1 supplied by
    // joiners 1 and 2 (their slots, their listeners), the remainder the
    // manager's own — three stripe holders. `K_D` for the directory,
    // `K_ROOT ≠ K_D` for the root (Issue 2: the discriminating shape).
    const K_D: u16 = 4;
    const K_ROOT: u16 = 6;
    // The closures below run more than once: they capture REFERENCES
    // (copied into each `async move` block), never the handles.
    let (mgr, uris_ref, mtokens_ref, j1venue_ref, j2venue_ref) =
        (&manager, &uris, &mtokens, &j1venue, &j2venue);
    let flip_and_migrate = |dir: u64, k: u16| async move {
        mgr.stripe_dir_with_suppliers(dir, k, &[1, 2])
            .await
            .unwrap_or_else(|e| panic!("the flip of {dir} with two supplied stripes: {e}"));
        // Every name re-homed before a reader looks (the finished shape).
        let started = std::time::Instant::now();
        loop {
            let _ = mgr.migrate_dir(dir).await.expect("the migration");
            let m = mgr.stripe_map(dir).await.expect("read").expect("striped");
            if !m.migrating {
                return m;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "the migration of {dir} did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let map = flip_and_migrate(d, K_D).await;
    assert_eq!(map.stripes.len(), usize::from(K_D));
    for (j, stripe) in [(&j1, map.stripes[0]), (&j2, map.stripes[1])] {
        assert!(
            j.volumes[0]
                .slot_leases()
                .expect("armed")
                .gate
                .is_leased(slot_of_global(&manager, stripe)),
            "premise: stripe {stripe} lives in a joiner's slot"
        );
    }
    // Post-flip children: the ones routing into a supplied stripe ship to
    // its holder (PR 6's step through the manager's shipper).
    files.extend(create_files(&manager, d, "post-", 24).await);
    let c = files.len() as u64;
    let (dv, _) = manager.route_ino(d);
    let (root_v, _) = manager.route_ino(1);
    let off_volume = files
        .iter()
        .filter(|(_, ino)| manager.route_ino(*ino).0 != dv)
        .count();
    assert!(
        off_volume > 0,
        "premise: some children live on the volume D does not ({off_volume} of {c})"
    );
    let k = u64::from(K_D);
    let files_ref = &files;

    // A COLD reader: nothing of the directory cached, every holder's
    // listener bound, its tree 0 polled after the manager's checkpoint
    // (the lessees published). `kernel_order` puts the mount root's
    // GETATTR BEFORE the lookup (the `default_permissions` walk). Returns
    // the grants its `ls -l D` paid, the per-volume `dlm_token_cached` at
    // the end (a fresh reader starts at 0) and the face.
    let cold_ls = |client_id: &'static str, kernel_order: bool| async move {
        for v in &mgr.volumes {
            v.checkpoint_now().await.expect("checkpoint");
        }
        let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(uris_ref)
            .await
            .expect("read-only open");
        for (vi, rv) in reader.volumes.iter().enumerate() {
            rv.arm_reader_revalidation(None).expect("arms");
            rv.revalidate_reader().await.expect("poll");
            let _default = rv
                .arm_token_reader(TokenClientConfig {
                    endpoint: mtokens_ref.endpoint.clone(),
                    secret: VENUE_SECRET.to_vec(),
                    client_id: client_id.to_string(),
                    volume: u16::try_from(vi).expect("ordinal"),
                })
                .expect("the manager's plane arms");
            rv.bind_reader_holder_endpoint(1, &j1venue_ref.endpoint);
            rv.bind_reader_holder_endpoint(2, &j2venue_ref.endpoint);
        }
        let per_volume = |reader: &RoutedMetaBackend, key: &str| -> Vec<u64> {
            reader_stats_json(&reader.volumes)[key]
                .as_array()
                .expect("the reader's Token family")
                .iter()
                .map(|v| v.as_u64().expect("a count"))
                .collect()
        };
        let g0: u64 = per_volume(&reader, "dlm_token_grants").iter().sum();
        // `ls -l D` — the routed verbs the FUSE handlers call.
        if kernel_order {
            reader
                .getattr(1)
                .await
                .expect("stat / (the permission walk)");
        }
        let looked = reader.lookup(1, "hot").await.expect("lookup D").ino;
        assert_eq!(looked, d);
        // The kernel revalidates the mount ROOT's attrs on every path
        // walk (every TTL is 0 under tokens).
        reader.getattr(1).await.expect("stat / (the kernel's walk)");
        reader.getattr(d).await.expect("stat D (cold)");
        let mut listed: Vec<String> = Vec::new();
        let mut offset = 0u64;
        loop {
            let page = reader
                .readdir_stream(d, offset, 7)
                .await
                .expect("the reader's readdir page");
            let Some((last, _)) = page.last() else {
                break;
            };
            offset = *last;
            listed.extend(page.iter().map(|(_, e)| e.name.clone()));
            if page.len() < 7 {
                break;
            }
        }
        let mut want: Vec<String> = files_ref.iter().map(|(n, _)| n.clone()).collect();
        want.sort();
        listed.sort();
        assert_eq!(listed, want, "the merge lists every child once");
        for (name, ino) in files_ref {
            let got = reader
                .lookup(d, name)
                .await
                .unwrap_or_else(|e| panic!("the reader resolves {name}: {e}"));
            assert_eq!(got.ino, *ino, "{name}");
            reader.getattr(*ino).await.expect("stat child");
        }
        let attrs = reader.getattr(d).await.expect("stat D (the fold)");
        assert_eq!(attrs.nlink, 2, "a directory of files folds to nlink 2");
        let paid = per_volume(&reader, "dlm_token_grants").iter().sum::<u64>() - g0;
        let cached = per_volume(&reader, "dlm_token_cached");
        let face = reader_stats_json(&reader.volumes);
        for v in &reader.volumes {
            v.shutdown().await.unwrap();
        }
        (paid, cached, face)
    };

    // Phase 1 — the leg's law, the root unstriped, lookup-first.
    let (paid, cached1, face) = cold_ls("pr13d-ls-reader", false).await;
    assert!(
        paid >= k + c && paid <= k + c + 4,
        "dlm_token_grants = {paid} ∉ [K + C, K + C + 4] = [{}, {}] for K = {k}, C = {c} (one \
         token per stripe, one per child, + the root's dentry token, D's record, D's re-grant \
         with dentries): {face}",
        k + c,
        k + c + 4
    );
    assert_eq!(
        paid,
        k + c + 3,
        "lookup-first, the root unstriped: the constant beside K + C is 3: {face}"
    );

    // Phase 2 — the ROOT striped over K_ROOT ≠ K_D (the fleet after its
    // writers' per-leg `mkdir`s into `/`): a second cold reader's first
    // `stat /` AFTER its lookup learnt the root's map folds over the
    // root's stripes and pays one records-only grant per root stripe.
    let root_map = flip_and_migrate(1, K_ROOT).await;
    let k_root = root_map.stripes.len() as u64;
    assert_eq!(k_root, u64::from(K_ROOT));
    assert_ne!(k_root, k, "the discriminating shape: K_root ≠ K_D");
    let (paid_striped_root, cached2, face) = cold_ls("pr13d-ls-reader-striped-root", false).await;
    assert_eq!(
        paid_striped_root,
        k + c + 3 + k_root,
        "a striped root adds exactly K_root = {k_root} records-only grants at the reader's \
         first stat / after the map is learnt (the record's §7 item 7 class — the fleet's \
         2K + C + 3 at K_root = K_D): {face}"
    );
    // The extra objects live on the ROOT's volume alone — the fleet's
    // per-volume `dlm_token_cached` split (PR 15's `+20,064` beside the
    // root's 20,000 children; PR 13d run 2's the same).
    assert_eq!(cached1.len(), 2);
    for v in 0..2 {
        let grew = cached2[v] - cached1[v];
        let want = if v == root_v { k_root } else { 0 };
        assert_eq!(
            grew, want,
            "volume {v}: the cached-object delta between the phases is the root's stripes on \
             the root's volume ({root_v}) and nothing elsewhere: {cached1:?} → {cached2:?}"
        );
    }

    // Phase 3 — the KERNEL's order on the striped root: the pre-lookup
    // `stat /` pays the root's records-only grant and folds nothing (the
    // map is not learnt yet); the lookup's marker read re-grants the root
    // with dentries — the law's `+4` ceiling.
    let (paid_kernel_order, _, face) = cold_ls("pr13d-ls-reader-kernel-order", true).await;
    assert_eq!(
        paid_kernel_order,
        k + c + 4 + k_root,
        "a cold reader in the kernel's order (GETATTR / before LOOKUP) reads the law's +4 \
         ceiling: the fleet's +3 is this minus the root's records grant the harness's \
         pre-leg .stats snapshot absorbs: {face}"
    );

    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&j2).await;
    shutdown(&j1).await;
    j2venue.tear_down();
    j1venue.tear_down();
    mtokens.tear_down();
    venue.tear_down();
    shutdown(&manager).await;
}

// ---------------------------------------------------------------------------
// PR 13e — the box re-run's findings (record §3.9.4.3): F-R3, the cross-owner
// plan's child witness read off this daemon's PROJECTION of a foreign
// lessee's slot.
// ---------------------------------------------------------------------------

/// Stand the initiator's halves up for `writer`: the process-global step
/// shipper (the ladder's rung 7, keyed by its member id) and PR 9's custody
/// arm over ITS set — the mount path's `arm_mount_slot_custody`, which is
/// what dials the writer's read divert to a foreign slot's HOLDER. Both
/// are process-global, so a contract that moves the initiator role between
/// two in-process daemons re-stands them per phase.
async fn stand_up_initiator(
    writer: &Arc<RoutedMetaBackend>,
    identity: &AppenderIdentity,
) -> Arc<squeezefs::data_grant::SlotCustodyArm> {
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(writer),
            &peer_of(identity),
            VENUE_SECRET.to_vec(),
        ),
    );
    let sink = Arc::new(ProbeSink {
        calls: std::sync::atomic::AtomicU64::new(0),
    });
    squeezefs::data_grant::arm_slot_custody(
        writer,
        &peer_of(identity),
        VENUE_SECRET.to_vec(),
        0,
        Arc::new(move |_volume| {
            Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
        }),
    )
}

/// The child's record at ITS HOLDER after a cross-owner removal: `nlink 0`
/// (the count step landed) or already destroyed — never `nlink ≥ 1` with
/// no name (the orphan).
async fn assert_counted_out(holder: &RoutedMetaBackend, ino: u64, what: &str) {
    if let Ok(a) = holder.getattr(ino).await {
        assert_eq!(
            a.nlink, 0,
            "{what}: the child's count step never landed at its holder — nlink {} with no \
             name is the orphan F-R3 leaked (430 / 512 per `rm -rf` on the box)",
            a.nlink
        );
    }
}

/// **F-R3 (record §3.9.4.3 / §4.4al; a FLIP PRECONDITION): a cross-owner
/// unlink of a child ANOTHER appender minted reads the child's witness AT
/// ITS HOLDER, never this daemon's projection.** The joiner creates into
/// the MANAGER's directory (PR 6: the dentry ships, the child is minted in
/// the joiner's rotor) and the manager `rm`s them; then the reverse (the
/// manager creates into the joiner's directory, the joiner `rm`s); then a
/// joiner-minted directory `rmdir`ed by the manager. RED on `7f4b007e`: the
/// plan builder read the witness with `read_inode_value_routed` — a LOCAL
/// KV read of this daemon's PROJECTION of the lessee's slot, which by
/// KD-SYM-3 never sees the leased root (it rides the lessee's page) — found
/// `None`, DROPPED the `SetNlink` step, shipped the `RemoveDentry` alone,
/// and logged "child ino … has no inode record — removing the dangling
/// name and accounting nothing" once per victim (m60: 430 / 512 lines in
/// one `rm -rf`, `rm -rf` reporting success); every child stayed `nlink 1`
/// with zero names at its creator, invisible to fsck at every censusing
/// mount (the inode plane scopes a live lessee's slots out; after the
/// leave C9's era floor exempted a joiner's mints — review round 1, Issue
/// 2, fixed: an unleased slot's records are prior-era candidates). GREEN:
/// the witness is read through the writer's read divert (the holder's
/// token plane — one grant, already held from the `lookup` that precedes
/// an `rm`, recalled by the very step the plan ships), every child reads
/// `nlink 0` at its holder, `xv_cross_owner_dangling_names` stays 0, and
/// the offline fsck after every writer LEAVES reads no C9.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_unlink_of_a_foreign_minted_child_reads_its_witness_at_the_holder() {
    use squeezefs::meta_backend::crossvol_tx::cross_owner_stats;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "jdir")]).await;
    let (shared, jdir) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-f-r3").await;
    let midentity = page_of(&uris[0], &mvol, 0).await.expect("page 0").identity;
    // The MANAGER holds `shared` (its first touch).
    manager
        .create(shared, "m0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 71).await),
                identity: joiner_identity(&mvol, 71).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jidentity = jvol.joined_wire().unwrap().identity;
    let jvenue = DaemonVenue::stand_up(&joiner, false, "joiner-custody-f-r3").await;
    // The bindings the ladders make (the manager dials the joiner where it
    // serves; the joiner learnt the manager's at its join).
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    // The JOINER holds `jdir` (its first touch, over the wire).
    joiner
        .create(jdir, "j0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    assert!(matches!(
        tree0_state(&mvol, SLOT_B).await,
        Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
    ));

    // A. The joiner creates into the MANAGER's directory: six children in
    // the joiner's rotor, their names at the manager.
    let _jarm = stand_up_initiator(&joiner, &jidentity).await;
    let from_joiner = create_files(&joiner, shared, "jf", 6).await;
    for (name, ino) in &from_joiner {
        let slot = slot_of_global(&manager, *ino);
        assert!(
            matches!(
                tree0_state(&mvol, slot).await,
                Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
            ),
            "{name} was minted in the joiner's rotor (slot {slot})"
        );
    }
    let sub = joiner
        .create(shared, "jsub", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("a joiner's mkdir into the manager's directory")
        .ino;
    let shared_nlink_with_sub = manager.getattr(shared).await.unwrap().nlink;

    // The MANAGER unlinks them (the box's `rm -rf` shape): its shipper, its
    // read divert. Every witness is the JOINER's word.
    let _marm = stand_up_initiator(&manager, &midentity).await;
    let s0 = cross_owner_stats();
    for (name, _) in &from_joiner {
        manager
            .unlink(shared, name)
            .await
            .unwrap_or_else(|e| panic!("the manager's unlink of {name}: {e}"));
    }
    // rmdir of the joiner-minted directory (the `is_dir` arm: post 0).
    manager
        .unlink(shared, "jsub")
        .await
        .expect("the manager's rmdir of a joiner-minted directory");
    let s1 = cross_owner_stats();
    assert_eq!(
        s1.dangling_names, s0.dangling_names,
        "the manager's plan builder found every child's record at its holder (RED: the \
         'no inode record — accounting nothing' arm fired once per victim off the projection)"
    );
    for (name, ino) in &from_joiner {
        assert!(
            manager.lookup(shared, name).await.is_err(),
            "{name}: the name is gone at the manager"
        );
        assert_counted_out(&joiner, *ino, name).await;
    }
    assert!(manager.lookup(shared, "jsub").await.is_err());
    assert_counted_out(&joiner, sub, "jsub").await;
    assert_eq!(
        manager.getattr(shared).await.unwrap().nlink,
        shared_nlink_with_sub - 1,
        "the rmdir's parent bump landed"
    );

    // B. The reverse: the manager creates into the JOINER's directory
    // (children in the manager's rotor), the joiner unlinks them — the
    // witness is the MANAGER's word (holder 0 of a joiner's foreign slot).
    let from_manager = create_files(&manager, jdir, "mf", 4).await;
    for (name, ino) in &from_manager {
        let slot = slot_of_global(&manager, *ino);
        assert!(
            matches!(
                tree0_state(&mvol, slot).await,
                Some(SlotState::Leased { appender_id: 0, .. })
            ),
            "{name} was minted in the manager's rotor (slot {slot})"
        );
    }
    let _jarm = stand_up_initiator(&joiner, &jidentity).await;
    let s2 = cross_owner_stats();
    for (name, _) in &from_manager {
        joiner
            .unlink(jdir, name)
            .await
            .unwrap_or_else(|e| panic!("the joiner's unlink of {name}: {e}"));
    }
    let s3 = cross_owner_stats();
    assert_eq!(s3.dangling_names, s2.dangling_names);
    for (name, ino) in &from_manager {
        assert!(joiner.lookup(jdir, name).await.is_err());
        assert_counted_out(&manager, *ino, name).await;
    }
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    assert_eq!(cross_owner_stats().intents_open, 0, "every plan retired");

    // The leave, then the census the lessee's life hid: an offline fsck
    // over every tree (every slot unleased now) must read no C9.
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    jvenue.tear_down();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    // The census after every writer LEFT: no finding, and NOTHING exempted
    // as current-era — a probe has nothing in flight, so an exemption is an
    // inode the census did not judge (review round 1, Issue 2: every
    // joiner-minted ino read exempt here before the era floor consulted
    // tree 0 — the box's 430 would have passed this census).
    fsck_clean_no_exempt(&uris).await;
}

/// **F-R3's other witness classes** (the brief's audit of every local
/// witness read in the cross-owner plan): a `link` of a foreign-minted
/// file (its `pre` read answered `NotFound` off the projection — the link
/// failed `ENOENT` for a file that exists), a `rename` OVER a foreign-
/// minted name (the destination's `SetNlink` dropped — the orphan again),
/// and a DIRECTORY MOVE out of a foreign-held parent (the old parent's
/// nlink shift read a stale or absent parent record — the shift step was
/// skipped or the underflow guard fired, and the parent's `nlink` drifted
/// at its holder for ever). Every witness is the holder's word now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_link_rename_over_and_directory_move_read_their_witnesses_at_the_holder() {
    use squeezefs::meta_backend::crossvol_tx::cross_owner_stats;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "jdir")]).await;
    let (shared, jdir) = (dirs[0], dirs[1]);
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-f-r3b").await;
    let midentity = page_of(&uris[0], &mvol, 0).await.expect("page 0").identity;
    manager
        .create(shared, "m0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 72).await),
                identity: joiner_identity(&mvol, 72).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jidentity = jvol.joined_wire().unwrap().identity;
    let jvenue = DaemonVenue::stand_up(&joiner, false, "joiner-custody-f-r3b").await;
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    joiner
        .create(jdir, "j0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();

    // The joiner's two files in the manager's directory.
    let _jarm = stand_up_initiator(&joiner, &jidentity).await;
    let jf = joiner
        .create(shared, "jf", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let jg = joiner
        .create(shared, "jg", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;

    let _marm = stand_up_initiator(&manager, &midentity).await;
    let s0 = cross_owner_stats();
    // (a) link: the manager links the joiner-minted file under a second
    // name in its own directory.
    let linked = manager.link(jf, shared, "jf-link").await.expect(
        "a link of a foreign-minted file is an ordinary op (RED: ENOENT off the projection)",
    );
    assert_eq!(linked.nlink, 2);
    assert_eq!(
        joiner.getattr(jf).await.unwrap().nlink,
        2,
        "the count step landed at the holder"
    );
    // (b) rename OVER the joiner-minted name: the manager's own file takes
    // `jg`'s name; `jg` is counted out at its holder.
    manager
        .create(shared, "mf", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    manager
        .rename(shared, "mf", shared, "jg", 0)
        .await
        .expect("a rename over a foreign-minted name");
    assert_counted_out(&joiner, jg, "jg (rename-over)").await;
    // (c) a directory MOVE out of the joiner-held parent: the manager
    // mkdirs `sub` INTO `jdir` (the bump ships: jdir nlink 3 at the
    // joiner), then moves it into `shared` — the old parent's nlink shift
    // is read at the joiner.
    let jdir_nlink0 = joiner.getattr(jdir).await.unwrap().nlink;
    manager
        .create(jdir, "sub", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("a mkdir into a foreign-held directory");
    assert_eq!(joiner.getattr(jdir).await.unwrap().nlink, jdir_nlink0 + 1);
    let shared_nlink0 = manager.getattr(shared).await.unwrap().nlink;
    manager
        .rename(jdir, "sub", shared, "sub", 0)
        .await
        .expect("a directory move out of a foreign-held parent");
    assert_eq!(
        joiner.getattr(jdir).await.unwrap().nlink,
        jdir_nlink0,
        "the old parent's nlink shift landed at its holder (RED: the shift read a stale or \
         absent parent record off the projection and was skipped)"
    );
    assert_eq!(
        manager.getattr(shared).await.unwrap().nlink,
        shared_nlink0 + 1
    );
    let s1 = cross_owner_stats();
    assert_eq!(s1.dangling_names, s0.dangling_names);
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .dir_nlink_underflows
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no underflow guard fired on a stale parent witness"
    );
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    assert_eq!(cross_owner_stats().intents_open, 0);

    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    jvenue.tear_down();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean_no_exempt(&uris).await;
}

/// **PR 13e review round 1, Issue 2 — the C9 census's era floor on a
/// forest: a JOINED appender's mints were exempt at every censusing
/// mount.** fsck C9's candidate filter is `minted_in_prior_era`, whose
/// per-slot floor was a snapshot of the censusing mount's `guest_cursors`
/// at OPEN — seeded from the ledger STAMP's slot cursors and the replay
/// window. A joined appender's rotor slots never reach the MANAGER's
/// cursors (it never `install_lease`s them), so the manager's stamp never
/// carries them; the slot's cursor lives in tree 0 alone (`Leased {
/// cursor }`, `Unleased { cursor }` after the leave). Every joiner-minted
/// ino therefore had NO floor entry → `current_era_exempted` → never a C9
/// candidate — at the online manager AND at an offline probe after every
/// writer left (the reviewer read the F-R3 pin's own census at its RED
/// commit: the 4 manager-minted orphans reported, the 7 joiner-minted
/// ones exempted). The box's 430 orphans were the OTHER writers' mints:
/// the post-leave census as first built read C9 = 0 on the broken binary
/// too. The law: an UNLEASED slot's records are ALL prior-era candidates
/// (nobody leases it ⇒ no create is in flight there — tree 0 / the lease
/// table is the witness), and a PROBE's are all candidates (nothing is in
/// flight in a probe; an offline census refuses a live set). This
/// contract plants a joiner-minted orphan (the joiner's own file, its
/// naming dentry deleted through the dentry tree — fsck C1's own repair
/// shape, `fsck_c9_tests`' plant) and, after both writers leave, the
/// offline probe must REPORT it as C9 with nothing exempted. RED on the
/// branch: 0 findings, `current_era_exempted` 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_minted_orphan_is_a_c9_finding_at_the_offline_census_after_every_writer_left() {
    use squeezefs::meta_backend::kv::record::{
        dentry_key, dentry_name_hash54, DentryValue, TREE_DENTRIES,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_B, "jdir")]).await;
    let jdir = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-c9-era").await;
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 73).await),
                identity: joiner_identity(&mvol, 73).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    // The joiner's own directory (its first touch) and two files minted in
    // ITS rotor — records no manager stamp ever names.
    let kept = joiner
        .create(jdir, "kept", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let orphan = joiner
        .create(jdir, "orphan", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap()
        .ino;
    jvol.checkpoint_now().await.unwrap();
    // The plant: the orphan's naming dentry deleted through the dentry
    // tree at the JOINER (the slot's lessee), its record left at nlink 1.
    // The key is the lookup's own: `(parent local ino, seeded hash54 of
    // the name, coll)`, the window compared by full name.
    let (_v, local_orphan) = joiner.route_ino(orphan);
    let (_v, local_jdir) = joiner.route_ino(jdir);
    let hash = dentry_name_hash54(b"orphan", jvol.superblock().hash_seed);
    let start = dentry_key(local_jdir, hash, 0);
    let end = dentry_key(local_jdir, hash, u8::MAX);
    let window = jvol
        .range_kind(TREE_DENTRIES, &start, &end, 64)
        .await
        .expect("the name's hash window");
    let mut planted = false;
    for (k, v) in &window {
        let d = DentryValue::decode(v).expect("a dentry record");
        if d.name == b"orphan" {
            jvol.delete_kind(TREE_DENTRIES, k)
                .await
                .expect("drop the naming dentry");
            planted = true;
            break;
        }
    }
    assert!(planted, "the orphan's dentry was found and dropped");
    jvol.checkpoint_now().await.unwrap();
    assert_eq!(
        jvol.read_inode_value_routed(local_orphan)
            .await
            .unwrap()
            .map(|v| v.nlink),
        Some(1),
        "the orphan: a record at nlink 1 with no name"
    );

    // Both writers LEAVE (the joiner's slots go Unleased at tree 0 with
    // their cursors); the offline probe judges every slot.
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let report = fsck_offline(&uris).await;
    let c9: Vec<_> = report.findings.iter().filter(|f| f.class == "C9").collect();
    assert_eq!(
        report.counters.current_era_exempted, 0,
        "a probe exempts nothing — every joiner-minted ino is judged (RED: the joiner's slot \
         had no era floor at the probe and its records read current-era)"
    );
    assert_eq!(
        c9.len(),
        1,
        "exactly the planted orphan is a C9 finding: {:?}",
        report.findings
    );
    assert!(
        c9[0].object.contains(&orphan.to_string()),
        "the finding names the joiner-minted orphan {orphan}: {}",
        c9[0].object
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.object.contains(&kept.to_string())),
        "the named sibling {kept} is no finding"
    );
}

/// **PR 13e review round 2, Issue 10 — C9 exempts the inos an OPEN
/// cross-owner plan names.** A cross-owner create commits the child's
/// record (step 0, the creator's rotor) under one door token and SHIPS
/// the `InsertDentry` afterwards; a ship the holder refuses leaves the
/// intent OPEN for the roll-forward cadence and the record standing at
/// `nlink 1` with no name. With tree 0 as C9's era witness (Issue 2) the
/// child's slot reads UNLEASED the moment its lessee releases it (a forced
/// shrink, a dominance handover, the creator's LEAVE), every record in it
/// is a prior-era candidate, and the fresh dentry pass finds no name — so
/// the first build REPORTED the child as C9 while the plan that names it
/// stood, and `--repair` would have destroyed the record the roll-forward
/// re-names (the roll-forward's insert then dangles: C10). C10 has had
/// the open-intent exemption since its birth (`open_intent_inos`); C9 now
/// reads the same set in its evaluate AND its confirm, counted on
/// `unreferenced_intent_exempted`. Shape: the joiner ships two creates
/// into the MANAGER's directory under `TEST_XV_SERVE_REFUSE` (the holder
/// down before its commit — both intents open, both records at nlink 1
/// with no name); `pending-b`'s intent record is then deleted AT ITS
/// LESSEE with the name never landed — the kill-before-the-inverse window
/// PR 6 leaves to the census, the shape C9 exists for; the joiner LEAVES
/// (its rotor slot `Unleased` at tree 0), and the manager's inode-plane
/// census must report `pending-b` ALONE: `pending-a` is exempted by its
/// open intent (counted), never reported. Then the manager's cadence rolls
/// `pending-a` FORWARD (an abandoned intent with no live owner here): its
/// name lands under `shared`, the census reads it named and exempts
/// nothing, `pending-b` stays the finding. The offline probe after the
/// manager leaves agrees. RED on the branch: BOTH children reported at
/// the first census (`--repair` would have destroyed `pending-a`'s record,
/// the one the roll-forward re-names). Found beside it: the roll-forward's
/// scan now RECONCILES the intent register — `pending-b`'s abandoned entry,
/// its record gone without this process's retirement (what another
/// daemon's roll-forward leaves behind on a fleet), is forgotten instead
/// of riding `intents_open` / `intents_stuck` for the mount's life.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_creates_child_is_never_a_c9_finding_while_its_intent_stands() {
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, intent_key_at, roll_forward_open_intents, IntentRecord, XvStep,
        TEST_XV_SERVE_REFUSE,
    };
    use squeezefs::meta_backend::kv::record::TREE_XATTRS;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-c9-intent").await;
    let midentity = page_of(&uris[0], &mvol, 0).await.expect("page 0").identity;
    // The MANAGER holds `shared` (its first touch).
    manager
        .create(shared, "m0", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .unwrap();
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 74).await),
                identity: joiner_identity(&mvol, 74).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jidentity = jvol.joined_wire().unwrap().identity;
    let jvenue = DaemonVenue::stand_up(&joiner, false, "joiner-c9-intent").await;
    mvol.slot_leases()
        .unwrap()
        .holders
        .set_endpoint(jid, &jvenue.endpoint);
    let _jarm = stand_up_initiator(&joiner, &jidentity).await;
    // A control: a joiner-minted child whose name LANDED at the manager.
    let landed = joiner
        .create(shared, "landed", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a joiner's create into the manager's directory")
        .ino;
    let s0 = cross_owner_stats();
    // Two creates whose ship the holder refuses BEFORE its commit: the
    // child records stand in the joiner's rotor, the intents stay OPEN.
    TEST_XV_SERVE_REFUSE.store(true, Ordering::SeqCst);
    for name in ["pending-a", "pending-b"] {
        let e = joiner
            .create(shared, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect_err("the holder refused before committing");
        assert!(
            !joiner.disabled_volumes.contains_key(&0),
            "a refused ship never fail-stops the initiator: {e}"
        );
    }
    TEST_XV_SERVE_REFUSE.store(false, Ordering::SeqCst);
    assert_eq!(
        cross_owner_stats().intents_open - s0.intents_open,
        2,
        "both intents stay open for the roll-forward"
    );
    // The children's inos and tx ids, off the intent records themselves.
    let mut pending: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    for (_home, tx_id, image) in jvol.xv_scan_intents_homed().await.unwrap() {
        let rec = IntentRecord::decode(&image).expect("an intent record");
        for step in &rec.steps {
            if let XvStep::InsertDentry { name, child, .. } = step {
                pending.insert(name.clone(), (*child, tx_id));
            }
        }
    }
    assert_eq!(
        pending.keys().cloned().collect::<Vec<_>>(),
        vec!["pending-a".to_string(), "pending-b".to_string()],
        "the two open intents name the two children"
    );
    let (pending_a, _tx_a) = pending["pending-a"];
    let (pending_b, tx_b) = pending["pending-b"];
    for (name, (ino, _)) in &pending {
        let (_v, local) = joiner.route_ino(*ino);
        assert_eq!(
            jvol.read_inode_value_routed(local)
                .await
                .unwrap()
                .map(|v| v.nlink),
            Some(1),
            "{name}: the child's record stands at nlink 1 with no name"
        );
        assert!(
            manager.lookup(shared, name).await.is_err(),
            "{name}: the name never landed at the holder"
        );
    }

    // `pending-b`'s intent retired at its LESSEE with the name NEVER
    // landing (the kill-before-the-inverse window): the record is C9's
    // object from here — the roll-forward can no longer name it.
    let home_b = jvol
        .xv_scan_intents_homed()
        .await
        .unwrap()
        .into_iter()
        .find(|(_, tx, _)| *tx == tx_b)
        .map(|(home, _, _)| home)
        .expect("pending-b's intent at its home");
    jvol.delete_kind(TREE_XATTRS, &intent_key_at(home_b, tx_b))
        .await
        .expect("retire the intent without its inverse");
    jvol.checkpoint_now().await.unwrap();

    // The creator LEAVES with `pending-a`'s intent open: its rotor slot
    // goes Unleased at tree 0 — every record in it a prior-era candidate.
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    jvenue.tear_down();
    let slot_a = slot_of_global(&manager, pending_a);
    assert!(
        matches!(
            tree0_state(&mvol, slot_a).await,
            Some(SlotState::Unleased { .. })
        ),
        "the children's slot {slot_a} is unleased after the creator's leave"
    );
    let c9_naming = |report: &squeezefs::fsck::FsckReport, ino: u64| {
        report
            .findings
            .iter()
            .any(|f| f.class == "C9" && f.object.contains(&ino.to_string()))
    };

    // (1) The census: `pending-a`'s intent STANDS — never a finding;
    // `pending-b`'s is gone with its name never landed — the finding.
    let report = inode_plane_over(&manager).await;
    assert!(
        !c9_naming(&report, pending_a),
        "a child an OPEN cross-owner plan names is never a C9 finding (RED: reported, and \
         --repair would destroy the record the roll-forward re-names): {:?}",
        report.findings
    );
    assert!(
        c9_naming(&report, pending_b),
        "with its intent gone and its name never landed, pending-b IS the orphan C9 exists \
         for: {:?}",
        report.findings
    );
    assert!(!c9_naming(&report, landed), "the landed child is named");
    assert_eq!(
        report.counters.unreferenced_intent_exempted, 1,
        "pending-a was exempted by its open intent, counted"
    );
    assert_eq!(report.counters.current_era_exempted, 0);

    // (2) The manager's cadence rolls `pending-a` FORWARD (an abandoned
    // intent with no live owner in this process): the name lands.
    let _marm = stand_up_initiator(&manager, &midentity).await;
    let rolled = roll_forward_open_intents(&manager)
        .await
        .expect("the roll-forward over the manager's own directory");
    assert_eq!(rolled, 1, "exactly pending-a's intent rolled forward");
    assert_eq!(
        manager
            .lookup(shared, "pending-a")
            .await
            .expect("named now")
            .ino,
        pending_a,
        "the roll-forward landed the name the census waited for"
    );
    // No durable intent stands, and the REGISTER agrees: `pending-a`'s
    // entry retired through the protocol, and `pending-b`'s — abandoned in
    // this process, its record gone without the protocol (as another
    // daemon's roll-forward leaves it on a real fleet) — was reconciled
    // away by the scan (found by this pin: before it the entry rode
    // `xv_cross_owner_intents_open`, then `_stuck`, for the mount's life).
    assert!(
        mvol.xv_scan_intents_homed().await.unwrap().is_empty(),
        "no intent record stands after the roll-forward"
    );
    assert_eq!(
        cross_owner_stats().intents_open,
        s0.intents_open,
        "the register holds no ghost: an abandoned intent the durable scan no longer lists is \
         forgotten"
    );
    // The forget credits `retired` for every intent it forgets (PR 13e review
    // round 3, Issue 13): the closure `minted ≡ retired + open` must hold after
    // the reconcile, or a later forget that skips the credit reads as a leak.
    let after = cross_owner_stats();
    assert_eq!(
        after.intents_minted,
        after.intents_retired + after.intents_open,
        "minted ≡ retired + open after the roll-forward's reconcile"
    );
    let report = inode_plane_over(&manager).await;
    assert!(!c9_naming(&report, pending_a), "named: no finding");
    assert!(
        c9_naming(&report, pending_b),
        "the retired-unnamed one stays the finding"
    );
    assert_eq!(
        report.counters.unreferenced_intent_exempted, 0,
        "nothing left to exempt"
    );

    // The offline probe after the manager leaves agrees: exactly pending-b.
    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    let report = fsck_offline(&uris).await;
    let c9: Vec<_> = report.findings.iter().filter(|f| f.class == "C9").collect();
    assert_eq!(c9.len(), 1, "exactly pending-b: {:?}", report.findings);
    assert!(c9[0].object.contains(&pending_b.to_string()));
    assert_eq!(report.counters.current_era_exempted, 0);
    assert_eq!(report.counters.unreferenced_intent_exempted, 0);
}

// ---------------------------------------------------------------------------
// PR 13e — F-R4 (record §3.9.4.3): a create into a directory whose slot
// MOVES to the creator mid-plan answered `ENOENT` once on the box.
// ---------------------------------------------------------------------------

/// **F-R4: a create into a directory whose slot moves TO the creator
/// mid-plan never answers `ENOENT` for a parent that exists.** Three
/// daemons (the box's shape): the manager, J1 (the OLD holder — its
/// directory `moving` lives in a slot it leases, minted after J2's
/// projection was taken, so J2's tree of that slot holds no record of it —
/// the box's `job-w60`, local 2 of slot 131), J2 (the requester). J2's
/// create into `moving` ships its `InsertDentry` to J1, where the seam
/// parks it after the lease check; J1 RELEASES the slot; J2's wire first
/// touch of it parks MID-INSTALL (the table word written, the tree not yet
/// adopted); the served step resumes. RED on `7f4b007e`: J1's dying-parent
/// verdict read `moving` through its divert — the manager redirected to
/// J2, whose table already named it the holder — and J2 served `Gone`
/// from its un-adopted projection; J1 answered the witness refusal, J2's
/// create surfaced `ENOENT` to the application. GREEN: the served step's
/// parent verdict is judged only while the holder LEASES the slot (before
/// and after the read), else the typed slot-moved class the initiator
/// re-dispatches on; a wire grant adopts the tree BEFORE naming this mount
/// the holder — the create lands at its new holder or answers a retryable
/// class, never `ENOENT`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_into_a_directory_whose_slot_moves_to_the_creator_mid_plan_never_answers_enoent() {
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, test_xv_serve_park_release, test_xv_serve_parked,
        TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK,
    };
    use squeezefs::meta_backend::kv::backend::{
        test_wire_grant_park_release, test_wire_grant_parked, TEST_WIRE_GRANT_PARK_MID_INSTALL,
    };
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "j1dir")]).await;
    let j1dir = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-f-r4").await;
    let mut joined = Vec::new();
    for n in [81u32, 82] {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, n).await),
                identity: joiner_identity(&mvol, n).await,
            },
        )
        .await;
        Knobs::clear();
        let j = r.expect("the joined open");
        // J1 first-touches the seeded slot and mints `moving` BEFORE J2
        // opens: J2's projection of that slot is tree 0's grant-time root,
        // which holds no record of `moving`.
        if n == 81 {
            j.create(j1dir, "moving", libc::S_IFDIR | 0o755, 1000, 1000)
                .await
                .expect("J1's mkdir in its own directory");
        }
        joined.push(j);
    }
    let (j1, j2) = (Arc::clone(&joined[0]), Arc::clone(&joined[1]));
    let (j1vol, j2vol) = (Arc::clone(&j1.volumes[0]), Arc::clone(&j2.volumes[0]));
    let (j1id, j2id) = (
        j1vol.appender_stats().unwrap().appender_id,
        j2vol.appender_stats().unwrap().appender_id,
    );
    let (j1identity, j2identity) = (
        j1vol.joined_wire().unwrap().identity,
        j2vol.joined_wire().unwrap().identity,
    );
    let j1venue = DaemonVenue::stand_up(&j1, false, "j1-custody-f-r4").await;
    let j2venue = DaemonVenue::stand_up(&j2, false, "j2-custody-f-r4").await;
    for vol in [&mvol, &j1vol, &j2vol] {
        let holders = &vol.slot_leases().unwrap().holders;
        holders.set_endpoint(j1id, &j1venue.endpoint);
        holders.set_endpoint(j2id, &j2venue.endpoint);
    }
    let moving = j1.lookup(j1dir, "moving").await.unwrap().ino;
    let slot = slot_of_global(&j1, moving);
    assert!(
        matches!(
            tree0_state(&mvol, slot).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == j1id
        ),
        "J1 leases `moving`'s slot {slot}"
    );
    let (_, local_moving) = j2.route_ino(moving);
    assert!(
        j2vol
            .read_inode_value_routed(local_moving)
            .await
            .unwrap()
            .is_none(),
        "the premise: J2's projection of slot {slot} holds no record of `moving`"
    );

    // J2 is the requester: its shipper, its read divert. Its plan reads the
    // parent at J1 and ships the InsertDentry there, where the seam parks
    // the served step after the lease check.
    let _j2arm = stand_up_initiator(&j2, &j2identity).await;
    TEST_XV_SERVE_PARK_AFTER_LEASE_CHECK.store(true, Relaxed);
    let parked0 = test_xv_serve_parked();
    let t1 = {
        let j2 = Arc::clone(&j2);
        tokio::spawn(async move {
            j2.create(moving, "moving-46", libc::S_IFREG | 0o644, 1000, 1000)
                .await
        })
    };
    wait_until("the served step parked at J1", || {
        test_xv_serve_parked() > parked0
    })
    .await;

    // The slot moves: J1 releases it (tree 0 Unleased at the manager); J2
    // learns it and first-touches it over the wire — the install parks
    // MID-INSTALL. J1's divert now dials the new holder (the process-global
    // arm re-stood as J1's: the box's daemons each had one).
    j1vol
        .release_slot_handover(j1id, slot)
        .await
        .expect("J1's release of the slot");
    mvol.checkpoint_now().await.unwrap();
    j2vol.refresh_control_projection().await.unwrap();
    let _j1arm = {
        let sink = Arc::new(ProbeSink {
            calls: std::sync::atomic::AtomicU64::new(0),
        });
        squeezefs::data_grant::arm_slot_custody(
            &j1,
            &peer_of(&j1identity),
            VENUE_SECRET.to_vec(),
            0,
            Arc::new(move |_volume| {
                Arc::clone(&sink) as Arc<dyn squeezefs::meta_ship::token_plane::RecallDataSink>
            }),
        )
    };
    TEST_WIRE_GRANT_PARK_MID_INSTALL.store(true, Relaxed);
    let installs0 = test_wire_grant_parked();
    let routing = u16::try_from(u64::from(slot) - 1).unwrap();
    let t2 = {
        let j2vol = Arc::clone(&j2vol);
        tokio::spawn(async move { j2vol.joined_accept_offers(&[(routing, 0)]).await })
    };
    wait_until("J2's wire grant install parked mid-install", || {
        test_wire_grant_parked() > installs0
    })
    .await;
    assert!(
        matches!(
            tree0_state(&mvol, slot).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == j2id
        ),
        "the manager granted the slot to J2"
    );

    // The served step resumes at J1 with the slot GONE from its lease set.
    let retries0 = cross_owner_stats().step_slot_moved_retries;
    test_xv_serve_park_release();
    // The served step answers (the RED shape: the witness refusal, the
    // create's errno) or is re-dispatched (the GREEN shape: the slot-moved
    // class, the initiator waiting on the install); the install lands
    // after either.
    let started = std::time::Instant::now();
    while !t1.is_finished() && cross_owner_stats().step_slot_moved_retries == retries0 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the served step neither answered nor was re-dispatched"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    test_wire_grant_park_release();
    t2.await.expect("the accept task");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), t1)
        .await
        .expect("the create completes once the install lands")
        .expect("the create task");
    match outcome {
        Ok(created) => match j2.lookup(moving, "moving-46").await {
            Ok(l) => assert_eq!(l.ino, created.ino, "the create landed at its new holder"),
            Err(e) => panic!(
                "F-R4: the create ACKED and its name exists nowhere ({e}) — the served step's \
                 witness refusal off a not-yet-adopted projection was read as a COMPLETED plan \
                 once the slot had moved to the initiator (the child minted, no dentry: the \
                 box's ENOENT wearing an ack)"
            ),
        },
        Err(e) => {
            assert_ne!(
                e.to_errno(),
                libc::ENOENT,
                "F-R4: a parent that EXISTS at its new holder must never read `ENOENT` — \
                 the OLD holder's dying-parent verdict off a not-yet-adopted projection ({e})"
            );
            assert!(
                e.refusal_class().is_some() || e.to_errno() == libc::EAGAIN,
                "only a retryable class may surface ({e})"
            );
        }
    }
    assert!(
        matches!(
            tree0_state(&mvol, slot).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == j2id
        ),
        "J2 holds the slot"
    );
    // The directory's record moved with its slot (J2's own tree now); its
    // name stays in J1's directory (J1's slot — read there: the process's
    // one custody arm is J1's at this point, so J2 would read `j1dir`'s
    // projection).
    assert_eq!(
        j2.getattr(moving)
            .await
            .expect("`moving` at its new holder")
            .nlink,
        2
    );
    assert_eq!(j1.lookup(j1dir, "moving").await.unwrap().ino, moving);
    assert_must_stay_zero(&mvol, "manager");
    assert_must_stay_zero(&j1vol, "j1");
    assert_must_stay_zero(&j2vol, "j2");
    assert_eq!(cross_owner_stats().intents_open, 0, "every plan retired");

    squeezefs::data_grant::disarm_slot_custody().await;
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    drop(joined);
    shutdown(&j2).await;
    drop(j2vol);
    drop(j2);
    j2venue.tear_down();
    shutdown(&j1).await;
    drop(j1vol);
    drop(j1);
    j1venue.tear_down();
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g — F-R5: the joiner's extent supply under a create storm on a
// floor-sized ring (the record's §3.9.5.2 / §7 item 16).
// ---------------------------------------------------------------------------

/// The cadence-timing pins' volume home: RAM-backed where the box has one
/// (`/dev/shm`), else the process's temp dir. The fixtures' default home is
/// a file on the laptop's btrfs, whose `fdatasync` is 100+ ms and variable
/// (the substrate bracket the metadata-throughput baseline measured at
/// 165× on the journal barrier) — a venue term no cadence can anticipate,
/// and not the box's (nvmet, µs-class).
fn cadence_venue_dir() -> tempfile::TempDir {
    let shm = std::path::Path::new("/dev/shm");
    if shm.is_dir() {
        tempfile::tempdir_in(shm).unwrap()
    } else {
        tempfile::tempdir().unwrap()
    }
}

/// One storming daemon: its backend, its directory, the names it acked.
type StormDaemon = (Arc<RoutedMetaBackend>, u64, Vec<(String, u64)>);

/// One joiner's supply faces, read off its own volume and the manager's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SupplyFaces {
    ring_bytes: u64,
    ring_grows: u64,
    grow_declined: u64,
    stalls: u64,
    pressure_cycles: u64,
    checkpoints: u64,
    /// The grant closure's terms of the joiner's own region.
    grant_claimed: u64,
    grant_unclaimed: u64,
    grant_returned: u64,
    /// The wire verbs this joiner issued for its supply.
    wire_grants: u64,
    wire_returns: u64,
    wire_reactive_grants: u64,
    wire_ring_grows: u64,
    compactions: u64,
    splits: u64,
    /// The cadence trigger in force (`meta_kv_checkpoint_trigger_ms`) and
    /// the F-B1 projection's faces (`meta_kv_checkpoint_projected_ms`,
    /// `meta_kv_checkpoint_{node,image}_unit_ns` in µs).
    trigger_ms: u64,
    projected_ms: u64,
    node_unit_us: u64,
    image_unit_us: u64,
}

/// [`assert_must_stay_zero`] without the flush-ceiling law — the JOINERS'
/// check in the two cadence-timing pins: the ceiling law is asserted on
/// the MANAGER (the box's trip site — F-B1's class reproduced there), while
/// a joiner's reading on this venue carries the dev profile's tick
/// lateness under CPU saturation (a decision 300 ms past its trigger with
/// the SMO mutex free — the tick's own pre-decision work, PR 13e's
/// "26–272 ms against a 50 ms tick"), a venue term the box does not have;
/// the joiners' ceiling is the fleet proof's read (`sym-scale`).
fn assert_supply_gauges_zero(vol: &KvMetaBackend, who: &str) {
    let s = vol.appender_stats().expect("a forest volume");
    assert_eq!(s.manager_verb_refusals, 0, "{who}: manager_verb_refusals");
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

fn supply_faces(vol: &KvMetaBackend) -> SupplyFaces {
    use std::sync::atomic::Ordering::Relaxed;
    let s = vol.appender_stats().expect("a forest volume");
    let own = s
        .regions
        .iter()
        .find(|r| r.id == s.appender_id)
        .expect("the joiner's own region");
    let j = vol.joined_stats().expect("a joined appender");
    SupplyFaces {
        ring_bytes: own.ring_bytes,
        ring_grows: s.ring_grows,
        grow_declined: j.ring_grow_declined,
        stalls: own.stalls,
        pressure_cycles: s.pressure_cycles,
        checkpoints: vol.checkpoint_seq(),
        grant_claimed: own.grant_claimed,
        grant_unclaimed: own.grant_unclaimed,
        grant_returned: s.grant_returned,
        wire_grants: j.wire_extent_grants,
        wire_returns: j.wire_extent_returns,
        wire_reactive_grants: j.wire_reactive_grants,
        wire_ring_grows: j.wire_ring_grows,
        compactions: squeezefs::meta_backend::kv::META_KV_NODE_COMPACTIONS.load(Relaxed),
        splits: squeezefs::meta_backend::kv::META_KV_NODE_SPLITS.load(Relaxed),
        trigger_ms: vol.checkpoint_trigger_ms(
            squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64,
        ),
        projected_ms: vol.checkpoint_projected_ms(),
        node_unit_us: vol.checkpoint_node_unit_ns() / 1_000,
        image_unit_us: vol.checkpoint_image_unit_ns() / 1_000,
    }
}

/// **The box's `sym-scale` shape in process**: `joiners` real joined
/// appenders on one volume, each storming ITS OWN directory with
/// `creators` unpaced creators for `storm` — the PRODUCT cadence alone
/// (the checkpoint task's tick, its pressure law) drives every cycle; no
/// test-side `checkpoint_now`. Returns every joiner with its directory
/// and the faces read at the storm's start, its first quarter, its third
/// quarter and its end, plus the manager's `(extent_grants, extent_returns,
/// manager_verbs)` deltas over the storm.
async fn floor_ring_storm(
    joiners: usize,
    creators: usize,
    storm: std::time::Duration,
) -> (
    tempfile::TempDir,
    Vec<String>,
    Arc<RoutedMetaBackend>,
    Arc<KvMetaBackend>,
    HoldersVenue,
    Vec<StormDaemon>,
    Vec<[SupplyFaces; 4]>,
    (u64, u64, u64),
) {
    let dir = cadence_venue_dir();
    // A volume wide enough that the derived grant's heap-share cap (a
    // quarter of the free heap over the appenders) is not the storm's
    // bottleneck: the pool must hold a cycle's promised images beside the
    // derived size (a production volume is GiB-class; this is the small
    // fixture's equivalent).
    let uris = format_stamped_set_with_config_len(
        dir.path(),
        1,
        VOL_LEN * 4 * (joiners as u64 + 1).max(2),
    )
    .await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mut daemons: Vec<StormDaemon> = Vec::new();
    for n in 1..=joiners {
        let j = join(&uris, &venue, &mvol, n as u32).await;
        let d = j
            .create(1, &format!("storm-w{n}"), libc::S_IFDIR | 0o755, 1000, 1000)
            .await
            .expect("the joiner's directory")
            .ino;
        daemons.push((j, d, Vec::new()));
    }
    let m0 = mvol.appender_stats().unwrap();
    let start: Vec<SupplyFaces> = daemons
        .iter()
        .map(|(j, _, _)| supply_faces(&j.volumes[0]))
        .collect();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks = Vec::new();
    for (i, (j, d, _)) in daemons.iter().enumerate() {
        for c in 0..creators {
            let j = Arc::clone(j);
            let d = *d;
            let stop = Arc::clone(&stop);
            tasks.push(tokio::spawn(async move {
                let mut out = Vec::new();
                let mut k = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let name = format!("c{c}-f{k:06}");
                    let ino = j
                        .create(d, &name, libc::S_IFREG | 0o644, 1000, 1000)
                        .await
                        .unwrap_or_else(|e| panic!("joiner {i} create {name}: {e}"))
                        .ino;
                    out.push((name, ino));
                    k += 1;
                    // The venue's pace (the dev profile's SMO costs 5–14 ms
                    // and the laptop's heat soak doubles it): one tick of
                    // the timer's grain every sixteen creates keeps a
                    // joiner near 3k creates/s — well past the ring floor's
                    // growth threshold, inside its checkpoint task's
                    // capacity.
                    if k.is_multiple_of(16) {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }
                (i, out)
            }));
        }
    }
    // Faces at the storm's quarters: the first quarter is the onset (the
    // floor ring, the cold pool), the last is the sized steady state.
    tokio::time::sleep(storm / 4).await;
    let q1: Vec<SupplyFaces> = daemons
        .iter()
        .map(|(j, _, _)| supply_faces(&j.volumes[0]))
        .collect();
    tokio::time::sleep(storm / 2).await;
    let q3: Vec<SupplyFaces> = daemons
        .iter()
        .map(|(j, _, _)| supply_faces(&j.volumes[0]))
        .collect();
    tokio::time::sleep(storm / 4).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    for t in tasks {
        let (i, out) = t.await.expect("a creator task");
        daemons[i].2.extend(out);
    }
    let end: Vec<SupplyFaces> = daemons
        .iter()
        .map(|(j, _, _)| supply_faces(&j.volumes[0]))
        .collect();
    let m1 = mvol.appender_stats().unwrap();
    let faces = start
        .into_iter()
        .zip(q1)
        .zip(q3)
        .zip(end)
        .map(|(((a, b), c), d)| [a, b, c, d])
        .collect();
    let mgr = (
        m1.extent_grants - m0.extent_grants,
        m1.extent_returns - m0.extent_returns,
        m1.manager_verbs - m0.manager_verbs,
    );
    (dir, uris, manager, mvol, venue, daemons, faces, mgr)
}

/// **F-R5 (the third box campaign, record §3.9.5.2 / §7 item 16 — PR
/// 2 / PR 3 / PR 12b; the box record's review, Issue 3): a joiner's
/// extent supply under a create storm runs at the one-SMO grain on a
/// floor-sized ring.** On the box (N = 8, 40k creates per writer in
/// ≈ 13 s) every joiner's ring sat at the 512 KiB floor
/// (`appender_ring_grows` 0 — PR 2's drain-then-grow owed, ring growth
/// DECLINED on a joiner), so it checkpointed ≈ 8×/s on the ring's
/// pressure law; every compaction's RETIRED image travelled to the manager
/// as `ReturnExtents` (`extent_grant_returned` +193 ≈ the compactions) and
/// came back one cycle later as a fresh carve — claim-and-retire churn at
/// the SMO grain, ≈ 105 manager verbs per joiner per storm, each a ring-0
/// control entry + barrier at the manager (3.6 ms). **The root**: the
/// manager derives a WIRE appender's grant off `set.region(id)`'s SMO-rate
/// EWMA, which is `None` for a joiner (it holds no region for one) — a
/// rate of 0, the FLOOR (8), whatever the joiner's storm; the joiner's own
/// rate never travelled (its `smos_this_cycle` was never even fed — the
/// manager's flush pass folds it). So the proactive 50 % refill DID engage
/// (309 of 1,483 grants asked the derived size) and answered the floor;
/// the flush pass's reactive ask (`needed.max(4)`) answered 1–4; a
/// fragmented heap trimmed both to the page's four runs.
///
/// The laws, on the box's shape in process (two joiners, one unpaced
/// creator each — the dev profile's SMO costs 5–14 ms, so the load is
/// venue-shaped — the PRODUCT cadence, no test-side `checkpoint_now`):
/// 1. **the ring**: a joiner whose cadence is pressure-driven GROWS its
///    ring past the floor (drain-then-grow over the wire — `GrowRing`,
///    the derived size off the measured commit rate) within the storm,
///    `joined_ring_grow_declined` stays 0, and the storm's last quarter
///    runs at the cadence's rate (bounded by the trigger in force);
/// 2. **the grant SIZE follows the joiner's rate**: the joiner measures
///    its SMO rate and asks ITS derived size — the extents landed per wire
///    grant read well above the floor (the join's grant is the rotor it
///    mints plus the SMO floor; the storm's asks the derived pool);
/// 3. **the pool**: retired images RECYCLE into the joiner's own unclaimed
///    set up to the derived size (a pressure-driven cadence returns
///    nothing), so `extent_grant_returned` stays flat while its
///    compactions run, and the flush pass never asks one SMO's images on a
///    healthy heap (`joined_wire_reactive_grants` 0); the manager's verbs
///    per joiner per storm fall by an order of magnitude against the
///    base's (≈ 195 → ≈ 10 on this fixture — the record's §4.4ao).
/// Every acked name resolves at every daemon, fsck clean after every
/// joiner left.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_joiners_extent_supply_under_a_create_storm_grows_its_ring_and_recycles_its_grant() {
    use squeezefs::meta_backend::kv::appender::{GRANT_EXTENTS_FLOOR, SYM_RING_FLOOR_BYTES};
    let _g = SEAM.lock().await;
    reset_process_state();
    let storm = std::time::Duration::from_secs(12);
    let (dir, uris, manager, mvol, venue, daemons, faces, (mgr_grants, mgr_returns, mgr_verbs)) =
        floor_ring_storm(2, 1, storm).await;
    for (i, f) in faces.iter().enumerate() {
        let [a, b, c, d] = f;
        eprintln!("F-R5 joiner {i}: start {a:?}");
        eprintln!("F-R5 joiner {i}: q1    {b:?}");
        eprintln!("F-R5 joiner {i}: q3    {c:?}");
        eprintln!("F-R5 joiner {i}: end   {d:?}");
    }
    eprintln!(
        "F-R5 manager over the storm: extent_grants +{mgr_grants}, extent_returns \
         +{mgr_returns}, manager_verbs +{mgr_verbs} ({} joiners, {} s, creates per joiner {:?})",
        daemons.len(),
        storm.as_secs(),
        daemons.iter().map(|(_, _, f)| f.len()).collect::<Vec<_>>()
    );
    for (i, [a, b, c3, c]) in faces.iter().enumerate() {
        let compactions = c.compactions - a.compactions;
        let cycles = c.checkpoints - a.checkpoints;
        assert!(
            a.ring_bytes == SYM_RING_FLOOR_BYTES,
            "joiner {i} joined at the floor ring ({} B)",
            a.ring_bytes
        );
        assert!(
            cycles >= 8 && compactions >= 4,
            "joiner {i}'s storm ran the cadence hard ({cycles} cycles, {compactions} \
             compactions) — the fixture's premise"
        );
        // Law 1 — the ring.
        assert!(
            c.ring_bytes > SYM_RING_FLOOR_BYTES && c.ring_grows >= 1,
            "joiner {i}: a pressure-driven cadence grows the ring past the floor (ring {} B, \
             grows {}, declined {}, stalls {}, pressure cycles +{} over {cycles} cycles) — RED: \
             the floor ring stood for the whole storm",
            c.ring_bytes,
            c.ring_grows,
            c.grow_declined,
            c.stalls - a.stalls,
            c.pressure_cycles - a.pressure_cycles
        );
        assert_eq!(
            c.grow_declined - a.grow_declined,
            0,
            "joiner {i}: a healthy storm declines no growth"
        );
        // The cycle rate falls to the cadence's: at the derived ring
        // (two max-ages of the stream) the pressure law — half the
        // admissible window — fires about once per max-age, coincident
        // with the age law, so the sized steady state runs at the TRIGGER
        // in force (the age law's, with F-B1's projection shortening it
        // where a cycle's SMO work approaches the ceiling — this venue's
        // 5–14 ms per SMO). The last quarter's count is bounded by twice
        // that trigger's rate (the two laws may both fire inside one
        // interval) plus one; the first quarter (the floor ring, the
        // growth steps) is printed beside it for the record.
        let onset = b.checkpoints - a.checkpoints;
        let steady = c.checkpoints - c3.checkpoints;
        let quarter_ms = storm.as_millis() as u64 / 4;
        let trigger_ms = c.trigger_ms.min(c3.trigger_ms).max(1);
        let cadence_bound = 2 * quarter_ms.div_ceil(trigger_ms) + 1;
        let pressure_onset = b.pressure_cycles - a.pressure_cycles;
        let pressure_steady = c.pressure_cycles - c3.pressure_cycles;
        // The law's premise is a ring SIZED before the quarter: a growth
        // landing inside it runs its drain-then-grow's cover cycles between
        // the drain waits (PR 13g, F-R5 mechanism 1), cycles the sized
        // cadence's bound does not price — the quarter is the sizing's,
        // stated (the venue decides how fast a saturated ring drains), and
        // law 1 above judged the growth itself.
        let grew_inside = c.ring_grows - c3.ring_grows;
        if grew_inside > 0 {
            eprintln!(
                "F-R5 joiner {i}: the ring grew {grew_inside}× INSIDE the last quarter ({} → {} \
                 B) — {steady} cycles there (pressure +{pressure_steady}) are the sizing's, not \
                 the sized cadence's; the steady-state law is not judged on this quarter",
                c3.ring_bytes, c.ring_bytes
            );
        } else {
            assert!(
                steady <= cadence_bound,
                "joiner {i}: the cycle rate is the cadence's once the ring is sized — {onset} \
                 cycles in the first quarter (the floor ring: pressure +{pressure_onset}), \
                 {steady} in the last (pressure +{pressure_steady}; the trigger in force \
                 {trigger_ms} ms, bound {cadence_bound} over {quarter_ms} ms)"
            );
        }
        // Law 2 — the grant SIZE follows the joiner's rate.
        let landed = (c.grant_claimed + c.grant_unclaimed) - (a.grant_claimed + a.grant_unclaimed);
        let grants = c.wire_grants - a.wire_grants;
        assert!(
            grants >= 1 && landed / grants >= 2 * GRANT_EXTENTS_FLOOR,
            "joiner {i}: {landed} extents landed over {grants} wire grants — the size the \
             joiner's measured rate derives, never the floor {GRANT_EXTENTS_FLOOR} the manager \
             derived off a rate it never saw (RED: ≈ 4 per grant)"
        );
        // Law 3 — the pool: the returns an order of magnitude under the
        // compactions (the surplus above the derived pool alone), the
        // reactive ask at most the cold-start belt — the storm's FIRST
        // flush pass, before any rate is measured (the join's grant covers
        // the rotor's mints; that pass's splits are what it cannot size).
        let returned = c.grant_returned - a.grant_returned;
        assert!(
            returned * 10 <= compactions,
            "joiner {i}: retired images recycle into the joiner's own pool — `extent_grant_\
             returned` moved +{returned} over {compactions} compactions (RED: +≈ compactions, \
             the claim-and-retire churn)"
        );
        assert!(
            c.wire_reactive_grants - a.wire_reactive_grants <= 1,
            "joiner {i}: the flush pass asked one SMO's images {} times on a healthy heap — at \
             most the cold-start belt (RED: 93 — the only supply path a floor-ring joiner ran)",
            c.wire_reactive_grants - a.wire_reactive_grants
        );
        let verbs = (c.wire_grants - a.wire_grants)
            + (c.wire_returns - a.wire_returns)
            + (c.wire_ring_grows - a.wire_ring_grows);
        // The base (`84520cb5`'s RED run of this fixture at 8 creators per
        // joiner): 197 verbs per joiner per 8 s storm — 93 reactive grants,
        // 54 derived-size grants (every one the floor), 50 returns; scaled
        // to this storm's length, the law is an order of magnitude under it.
        let base_verbs = 197 * storm.as_secs() / 8;
        assert!(
            verbs * 10 <= base_verbs,
            "joiner {i}: its supply cost the manager {verbs} verbs over the storm ({} grants, \
             {} returns, {} grows; {cycles} cycles, {compactions} compactions) — an order of \
             magnitude under the base's {base_verbs} per joiner per {} s storm",
            c.wire_grants - a.wire_grants,
            c.wire_returns - a.wire_returns,
            c.wire_ring_grows - a.wire_ring_grows,
            storm.as_secs()
        );
    }
    // Every acked name resolves at its creator, the manager reads them
    // too after the leaves, nothing lost.
    for (i, (j, d, files)) in daemons.iter().enumerate() {
        assert!(
            files.len() >= 500,
            "joiner {i} stormed ({} creates)",
            files.len()
        );
        assert_all_resolve(j, *d, files).await;
        assert_supply_gauges_zero(&j.volumes[0], &format!("joiner {i}"));
    }
    assert_must_stay_zero(&mvol, "manager");
    let dirs: Vec<(u64, Vec<(String, u64)>)> =
        daemons.iter().map(|(_, d, f)| (*d, f.clone())).collect();
    for (j, _, _) in daemons {
        shutdown(&j).await;
    }
    for (d, files) in &dirs {
        assert_all_resolve(&manager, *d, files).await;
    }
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
    drop(dir);
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 1 — `GrowRing` is idempotent against a
// DURABLE witness, its carve owner-named from its first instant.
// ---------------------------------------------------------------------------

/// **`GrowRing` re-asked after a lost reply carves ONCE; a segment no page
/// ever names is RETURNED by the death path; a colleague's id is peer-
/// bound** (PR 13g review round 1, Issue 1 — §5.3.5's law for the verb).
/// The first build's carve was an alloc delta in ring 0 named by nobody
/// durable until the JOINER's page wrote the grown table: a reply lost
/// after the manager's carve (a manager crash, a dropped session) had
/// the joiner's retry door re-ask and a SECOND segment carved, the first
/// leaked for the volume's life; a joiner dying between the reply and
/// its page write leaked the segment; and fsck C13 — the census the
/// design named as the detector — walks grant-CLAIMED extents, so no
/// census this binary has could see either. Now the carve's control entry
/// writes the segment into the identity's `appender_hint` as a
/// `PendingSegment` bound to `(appender_id, term)`: the same
/// incarnation's re-ask whose page does not name it is answered VERBATIM
/// (`manager_verb_replays`), and the leave, the death ledger's recovery
/// (`appender clear`'s path too) and a rejoin RETURN a segment no page
/// named (`appender_pending_segments_returned`). The pin drives the
/// manager's verb directly (the joiner's retry door re-issues the same
/// frame): ask, ask again — one carve, the same segment, the free heap
/// moved once, the hint naming it; a frame from a session whose peer is
/// ANOTHER member id is `Rejected`; the joiner dies with the segment
/// unnamed and the recovery returns it — the hint cleared, the heap back.
/// RED before the fix: the second ask carves a second segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grow_ring_re_asked_after_a_lost_reply_carves_once_and_the_death_path_returns_an_unnamed_segment(
) {
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
    let files = create_files(&joiner, shared, "x", 8).await;
    let node_size = NODE_SIZE as u64;
    let want = 4 * node_size;
    let free0 = mvol.free_extents();
    let m0 = mvol.appender_stats().unwrap();
    // The ask, from the joiner's own session (an ad-hoc peer keeps the
    // page's witness laws).
    let first = mvol
        .manager_grow_ring_wire(id, want, &peer_of(&identity))
        .await
        .expect("the first GrowRing")
        .expect("a segment");
    assert_eq!(first.len, want, "the whole ask on a fresh heap");
    let free1 = mvol.free_extents();
    assert_eq!(free0 - free1, want / node_size, "one carve");
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.pending.map(|p| (p.appender_id, p.start, p.len)),
        Some((id, first.start, first.len)),
        "the carve's own control entry names the segment for the identity ({hint:?})"
    );
    // The retry door's re-ask after a lost `RingGrown` reply: the joiner's
    // page does not name the segment yet.
    let second = mvol
        .manager_grow_ring_wire(id, want, &peer_of(&identity))
        .await
        .expect("the re-asked GrowRing")
        .expect("a segment");
    assert_eq!(second, first, "the pending segment is answered VERBATIM");
    assert_eq!(mvol.free_extents(), free1, "nothing more carved");
    let m1 = mvol.appender_stats().unwrap();
    assert_eq!(
        m1.manager_verb_replays - m0.manager_verb_replays,
        1,
        "the re-ask is a replay"
    );
    // A frame from a session whose authenticated peer is ANOTHER member id
    // names a colleague's page: rejected, nothing carved.
    let other = squeezefs::cowriter::node_member_id_of(identity.node_token ^ 0x5a5a, 7);
    let refused = mvol.manager_grow_ring_wire(id, want, &other).await;
    assert!(
        matches!(
            refused,
            Err(squeezefs::meta_backend::kv::KvError::Rejected(_))
        ),
        "a colleague's GrowRing is Rejected: {refused:?}"
    );
    assert_eq!(mvol.free_extents(), free1);
    assert_eq!(
        mvol.appender_stats().unwrap().manager_verb_rejected - m1.manager_verb_rejected,
        1
    );
    // The joiner dies before its page names the segment; the death path
    // returns it.
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let m2 = mvol.appender_stats().unwrap();
    assert_eq!(
        m2.pending_segments_returned, 1,
        "the unnamed segment is returned by the recovery"
    );
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(hint.pending, None, "the witness is cleared ({hint:?})");
    assert!(
        mvol.free_extents() >= free1 + want / node_size,
        "the segment's extents are back in the heap ({} → {})",
        free1,
        mvol.free_extents()
    );
    assert_all_resolve(&manager, shared, &files).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 2, Issue 20 — the death path's settle never fails
// silently, and a released segment is never freed twice.
// ---------------------------------------------------------------------------

/// **A refused death-path settle ABORTS the recovery step, the re-run
/// completes it, and the segment the dead page names is released exactly
/// ONCE** (PR 13g review round 2, Issue 20). The round-1 death path ran
/// `settle_pending_ring_segment` `Try`-admitted under the SMO mutex and
/// treated a refusal as a WARN: the page still went `Recovered`, the
/// release freed its ring — the named segment included — while the
/// pending word STOOD, and the identity's rejoin settled against no page
/// ("not named") and FREED the segment's extents a second time, extents
/// the manager may have carved into another appender's ring by then
/// (lowest-free-first) — two custodians, a live ring freed under its
/// writer. The pin: a joiner GROWS its ring for real (a stall bumped, its
/// cadence asks `GrowRing`, its page names the segment, the witness
/// stands), dies, the death path's settle is REFUSED once (the seam — a
/// refusal no cover cycle discharges; a full ring's `JournalReserve
/// Exhausted` is the drain-and-retry class and recovers by design): the
/// recovery is DEFERRED with the page `Recovering`, nothing released;
/// the re-run settles (the word cleared, the segment released with the
/// ring, `appender_pending_segments_returned` unmoved), the region
/// released, a second joiner carves a ring from the freed extents, the
/// identity rejoins — the second joiner's ring stays allocated and the
/// witness is clear. RED before the fix: the first run recovers with the
/// word standing, and the rejoin frees the second joiner's ring extents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_death_path_settle_aborts_the_step_and_a_released_segment_is_freed_once() {
    use squeezefs::meta_backend::kv::backend::recovery::TEST_RECOVERY_SETTLE_REFUSE_N;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    TEST_RECOVERY_SETTLE_REFUSE_N.store(0, Ordering::SeqCst);
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let files = create_files(&joiner, shared, "x", 8).await;
    jvol.checkpoint_now().await.unwrap();
    // A REAL growth: a stall bumped, the cadence's drain-then-grow asks the
    // manager, the page names the segment, the witness stands.
    let segments0 = page_of(&uris[0], &mvol, id).await.unwrap().segments.len();
    jvol.appenders_public()
        .unwrap()
        .region(id)
        .unwrap()
        .stalls
        .fetch_add(1, Ordering::Relaxed);
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], &mvol, id).await.unwrap();
    assert_eq!(
        page.segments.len(),
        segments0 + 1,
        "the ring grew by one segment"
    );
    let segment = *page.segments.last().unwrap();
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.pending.map(|p| (p.start, p.len)),
        Some((segment.start, segment.len)),
        "the premise: the pending witness names the segment the page names"
    );
    let node_size = NODE_SIZE as u64;
    let heap_start = mvol.superblock().heap.start;
    let segment_extents: Vec<u64> = (0..segment.len / node_size)
        .map(|i| (segment.start - heap_start) / node_size + i)
        .collect();
    // The death, the settle REFUSED once (the seam's class is one no cover
    // cycle discharges).
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    let returned0 = mvol.appender_stats().unwrap().pending_segments_returned;
    TEST_RECOVERY_SETTLE_REFUSE_N.store(1, Ordering::SeqCst);
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(
        TEST_RECOVERY_SETTLE_REFUSE_N.load(Ordering::SeqCst),
        0,
        "the seam refused the one settle"
    );
    assert_eq!(
        rep.recovered(),
        0,
        "a refused settle ABORTS the recovery step ({rep:?})"
    );
    assert_eq!(
        page_of(&uris[0], &mvol, id).await.unwrap().state,
        AppenderState::Recovering,
        "the page stays Recovering for the re-run"
    );
    for e in &segment_extents {
        assert!(
            mvol.allocator().is_allocated(*e),
            "nothing released under a standing witness ({e})"
        );
    }
    // The re-run: settled (the named segment's word CLEARED, nothing
    // returned by the settle), recovered.
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "{rep:?}");
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.pending, None,
        "the witness is cleared by the re-run ({hint:?})"
    );
    assert_eq!(
        mvol.appender_stats().unwrap().pending_segments_returned,
        returned0,
        "a segment the page names is released with the ring, never 'returned'"
    );
    // The release: the next projection frees the ring (the segment with
    // it) — exactly once.
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.regions_released, 1, "{rep:?}");
    for e in &segment_extents {
        assert!(
            !mvol.allocator().is_allocated(*e),
            "released with the ring ({e})"
        );
    }
    // A SECOND joiner carves its ring and its join grant lowest-free-first
    // — from the released extents.
    let second = join(&uris, &venue, &mvol, 4).await;
    let svol = Arc::clone(&second.volumes[0]);
    let sid = svol.appender_stats().unwrap().appender_id;
    let mut second_held: std::collections::BTreeSet<u64> = page_of(&uris[0], &mvol, sid)
        .await
        .unwrap()
        .segments
        .iter()
        .flat_map(|s| {
            let first = (s.start.max(heap_start) - heap_start) / node_size;
            first..(s.end() - heap_start) / node_size
        })
        .collect();
    second_held.extend(record_extents(&mvol, sid).await);
    let reused: Vec<u64> = segment_extents
        .iter()
        .copied()
        .filter(|e| second_held.contains(e))
        .collect();
    assert!(
        !reused.is_empty(),
        "the premise: the second joiner's ring or grant reuses released segment extents"
    );
    // The identity's rejoin: a fresh region over the Free page — the
    // witness clear, nothing freed under the second joiner.
    let again = join(&uris, &venue, &mvol, 3).await;
    let avol = Arc::clone(&again.volumes[0]);
    for e in &second_held {
        assert!(
            mvol.allocator().is_allocated(*e),
            "the second joiner's extent {e} was freed under it by the rejoin's settle"
        );
    }
    assert_eq!(
        mvol.appender_stats().unwrap().pending_segments_returned,
        returned0,
        "the rejoin returned nothing — no witness stood"
    );
    assert_all_resolve(&manager, shared, &files).await;
    assert_supply_gauges_zero(&avol, "the rejoined joiner");
    shutdown(&again).await;
    shutdown(&second).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 3, Issue 23 — the death-path settle's DRAIN-AND-RETRY
// arm: a transient admission refusal is healed by the recovery's own cover
// cycles; one past the bound is the tail's class.
// ---------------------------------------------------------------------------

/// A joiner that GROWS its ring for real (a stall bumped, the cadence's
/// drain-then-grow, the page naming the segment, the witness standing),
/// then dies with its death recorded — the Issue 20 pin's premise, shared.
/// Answers `(identity, appender id, the segment's extents)`.
async fn grown_ring_then_death(
    uris: &[String],
    venue: &HoldersVenue,
    mvol: &Arc<KvMetaBackend>,
    n: u32,
) -> (AppenderIdentity, u32, Vec<u64>) {
    use std::sync::atomic::Ordering;
    let joiner = join(uris, venue, mvol, n).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    jvol.checkpoint_now().await.unwrap();
    let segments0 = page_of(&uris[0], mvol, id).await.unwrap().segments.len();
    jvol.appenders_public()
        .unwrap()
        .region(id)
        .unwrap()
        .stalls
        .fetch_add(1, Ordering::Relaxed);
    jvol.checkpoint_now().await.unwrap();
    let page = page_of(&uris[0], mvol, id).await.unwrap();
    assert_eq!(
        page.segments.len(),
        segments0 + 1,
        "the ring grew by one segment"
    );
    let segment = *page.segments.last().unwrap();
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.pending.map(|p| (p.start, p.len)),
        Some((segment.start, segment.len)),
        "the premise: the pending witness names the segment the page names"
    );
    let node_size = NODE_SIZE as u64;
    let heap_start = mvol.superblock().heap.start;
    let extents: Vec<u64> = (0..segment.len / node_size)
        .map(|i| (segment.start - heap_start) / node_size + i)
        .collect();
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    assert!(!mvol.record_death_with_key(identity, 9, 0).await.unwrap());
    (identity, id, extents)
}

/// **The death-path settle's drain-and-retry arm, both sides** (PR 13g
/// review round 3, Issue 23 — round 2's pin proved the ABORT on a refusal
/// no cycle discharges; this one proves the arm the abort sits beside).
/// A `JournalReserveExhausted` on the settle's admission — ring 0 full
/// under the recovery's own hold of the SMO mutex, which the checkpoint
/// task cannot drain for it — is healed by a barriered cover cycle of the
/// recovery's OWN task and the settle retried (`admit_control_drain_and_
/// retry`'s law): (a) TRANSIENT — two refusals (`TEST_RECOVERY_SETTLE_
/// EXHAUST_N` = 2) → the settle lands at the third attempt in the SAME
/// run (`recovered` 1, nothing aborted, the seam consumed, the ledger seq
/// advanced by the cover cycles), the witness cleared, the page-named
/// segment released with its ring ONCE (`appender_pending_segments_
/// returned` unmoved; the extents free after the release, a rejoin frees
/// nothing under whoever holds them next); (b) PERSISTENT — a count past
/// `COVER_CYCLES_MAX` → every cycle of the bound run, the seam consumed to
/// the bound, the step ABORTED by the tail's class (the page `Recovering`,
/// the segment allocated), and the re-run with the seam clear completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_settle_admission_refusal_is_healed_by_the_recoverys_own_cover_cycles() {
    use squeezefs::meta_backend::kv::backend::recovery::{
        TEST_RECOVERY_SETTLE_EXHAUST_N, TEST_RECOVERY_SETTLE_REFUSE_N,
    };
    use squeezefs::meta_backend::kv::checkpoint::COVER_CYCLES_MAX;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    TEST_RECOVERY_SETTLE_EXHAUST_N.store(0, Ordering::SeqCst);
    TEST_RECOVERY_SETTLE_REFUSE_N.store(0, Ordering::SeqCst);
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let files = create_files(&manager, shared, "m", 4).await;
    // (a) The TRANSIENT shape: two admission refusals, healed in one run.
    let (identity_a, id_a, segment_a) = grown_ring_then_death(&uris, &venue, &mvol, 3).await;
    let returned0 = mvol.appender_stats().unwrap().pending_segments_returned;
    let seq0 = mvol.checkpoint_seq();
    TEST_RECOVERY_SETTLE_EXHAUST_N.store(2, Ordering::SeqCst);
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(
        TEST_RECOVERY_SETTLE_EXHAUST_N.load(Ordering::SeqCst),
        0,
        "both admission refusals were met — the settle was retried past them"
    );
    assert_eq!(
        rep.recovered(),
        1,
        "a transient admission refusal never aborts the step — the recovery completes in ONE \
         run ({rep:?})"
    );
    assert!(
        mvol.checkpoint_seq() >= seq0 + 2,
        "the arm ran a cover cycle per refusal ({} → {})",
        seq0,
        mvol.checkpoint_seq()
    );
    let hint = mvol
        .appender_hint_for(identity_a.node_token, identity_a.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.pending, None,
        "the witness is cleared by the settle ({hint:?})"
    );
    assert_eq!(
        mvol.appender_stats().unwrap().pending_segments_returned,
        returned0,
        "a page-named segment is released with the ring, never 'returned'"
    );
    assert_eq!(
        page_of(&uris[0], &mvol, id_a).await.unwrap().state,
        AppenderState::Recovered
    );
    for e in &segment_a {
        assert!(
            mvol.allocator().is_allocated(*e),
            "the segment goes back with the ring at the release, not before ({e})"
        );
    }
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.regions_released, 1, "{rep:?}");
    for e in &segment_a {
        assert!(
            !mvol.allocator().is_allocated(*e),
            "released with the ring ({e})"
        );
    }
    // Exactly once: the identity's rejoin over the Free page frees nothing
    // (the word is clear; whoever carved the extents since keeps them).
    let free_before_rejoin = mvol.free_extents();
    let again = join(&uris, &venue, &mvol, 3).await;
    let avol = Arc::clone(&again.volumes[0]);
    assert_eq!(
        mvol.appender_stats().unwrap().pending_segments_returned,
        returned0,
        "the rejoin returned nothing — no witness stood"
    );
    assert!(
        mvol.free_extents() < free_before_rejoin,
        "the rejoin only CLAIMED (its ring, its grant) — nothing of the old segment was freed \
         a second time"
    );
    // (b) The PERSISTENT shape: a count past the bound — every cycle of the
    // bound runs, then the tail's class aborts the step; the re-run with
    // the seam clear completes.
    let (identity_b, id_b, segment_b) = grown_ring_then_death(&uris, &venue, &mvol, 4).await;
    let seq1 = mvol.checkpoint_seq();
    TEST_RECOVERY_SETTLE_EXHAUST_N.store(COVER_CYCLES_MAX + 1, Ordering::SeqCst);
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(
        TEST_RECOVERY_SETTLE_EXHAUST_N.load(Ordering::SeqCst),
        0,
        "the settle was attempted once per cycle of the bound, and once more"
    );
    assert_eq!(
        rep.recovered(),
        0,
        "an admission refusal that outlives the bound ABORTS the step ({rep:?})"
    );
    assert!(
        mvol.checkpoint_seq() >= seq1 + COVER_CYCLES_MAX as u64,
        "the whole bound of cover cycles ran ({} → {})",
        seq1,
        mvol.checkpoint_seq()
    );
    assert_eq!(
        page_of(&uris[0], &mvol, id_b).await.unwrap().state,
        AppenderState::Recovering,
        "the page stays Recovering for the re-run"
    );
    for e in &segment_b {
        assert!(
            mvol.allocator().is_allocated(*e),
            "nothing released under a standing witness ({e})"
        );
    }
    let rep = recover_dead_appenders_set(&manager).await.unwrap();
    assert_eq!(rep.recovered(), 1, "the re-run completes ({rep:?})");
    let hint = mvol
        .appender_hint_for(identity_b.node_token, identity_b.mount_slot)
        .await
        .unwrap();
    assert_eq!(hint.pending, None, "{hint:?}");
    assert_eq!(
        mvol.appender_stats().unwrap().pending_segments_returned,
        returned0
    );
    assert_all_resolve(&manager, shared, &files).await;
    assert_supply_gauges_zero(&avol, "the rejoined joiner");
    shutdown(&again).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 2 — the unnamed POOL survives a crash-rejoin.
// ---------------------------------------------------------------------------

/// **A joiner killed with a pool wider than its page names rejoins with
/// the pool WHOLE — `claimed ≡ reachable`, no C13 orphan, no fsck** (PR
/// 13g review round 1, Issue 2). The page names the pool's largest
/// `GRANT_RUNS_MAX` = 4 runs; the rest stays unclaimed in RAM. A DEAD
/// joiner's recovery returns them; an own-residue REJOIN
/// (`RegionGrant::recover` — every record extent the page does not name
/// lands CLAIMED) routed nothing to them and freed nothing: C13
/// candidates only at an fsck run at the joiner, its repair gated — the
/// crash-class leak PR 3 review round 1 Issue 9 had closed by making the
/// page name EVERY unclaimed extent, reopened by the pool. Now the own-
/// residue open runs the death path's orphan census for its OWN region
/// (`restore_own_pools`: a claimed extent no tree of this mount reaches
/// and no in-window claim named is the pool — back to UNCLAIMED,
/// `appender_pool_restored_extents`). The shape: the joiner's pool grown
/// in SIX asks with the manager claiming between them (non-adjacent
/// runs — more than the four the page names), the joiner killed (dropped
/// without its leave), the same identity rejoined over its `Live` page.
/// RED before the fix: the unnamed runs read claimed at the rejoin, the
/// joiner's C13 census names them, the pool is short by their extents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiner_killed_with_a_pool_wider_than_its_page_names_rejoins_with_the_pool_whole() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // A 512 MiB member: the manager's interleaving claims and six pool
    // grants never near the fixture heap's reserve.
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mdir = manager
        .create(1, "mgr", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let shared = joiner
        .create(1, "own", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let files = create_files(&joiner, shared, "x", 8).await;
    // (claimed, unclaimed, pending + returnable) of the joiner's region;
    // (returned, granted) of its set — the joiner's one own region.
    let region_faces = |vol: &KvMetaBackend| {
        let s = vol.appender_stats().unwrap();
        let own = s
            .regions
            .iter()
            .find(|r| r.id == id)
            .expect("the region")
            .clone();
        (
            own.grant_claimed,
            own.grant_unclaimed,
            own.grant_pending + own.grant_returnable,
            s.grant_returned,
            s.grant_granted,
        )
    };
    // Six asks, the manager claiming between them: the pool grows by a
    // non-adjacent run each time.
    for round in 0..6u32 {
        let (_, unclaimed, _, _, _) = region_faces(&jvol);
        let got = jvol
            .joined_extent_grant(u32::try_from(unclaimed + 16).unwrap())
            .await
            .expect("a grant");
        assert!(got > 0, "round {round} grew the pool ({got})");
        create_files(&manager, mdir, &format!("m{round}-"), 400).await;
        mvol.checkpoint_now().await.unwrap();
    }
    let runs = jvol
        .appender_stats()
        .unwrap()
        .regions
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.grant_unclaimed_runs)
        .unwrap();
    assert!(
        runs > squeezefs::meta_backend::kv::appender::GRANT_RUNS_MAX as u64,
        "the premise: the pool holds more runs than the page names ({runs})"
    );
    // No quiet joiner cycle here: the class is a joiner killed with its
    // storm's pool still standing — a quiet cadence would SHRINK the pool
    // to its target first (Issue 5), returning the surplus durably. Every
    // wire refill already wrote the page naming the pool's largest runs.
    let (claimed0, unclaimed0, _, _, granted0) = region_faces(&jvol);
    assert!(unclaimed0 >= 16 * 5, "the pool: {unclaimed0}");
    // What the page NAMES of the pool — the census must restore the rest.
    let named0: u64 = page_of(&uris[0], &mvol, id)
        .await
        .unwrap()
        .grant
        .iter()
        .map(|r| u64::from(r.len))
        .sum();
    assert!(
        named0 < unclaimed0,
        "the premise: the page names {named0} of a pool of {unclaimed0}"
    );
    // The kill: dropped without its leave — its page stays Live.
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    // The rejoin over own residue.
    let again = join(&uris, &venue, &mvol, 3).await;
    let avol = Arc::clone(&again.volumes[0]);
    assert_eq!(
        avol.appender_stats().unwrap().appender_id,
        id,
        "the same region"
    );
    let (claimed1, unclaimed1, queued1, returned1, granted1) = region_faces(&avol);
    let s = avol.appender_stats().unwrap();
    assert_eq!(granted1, granted0, "the record is the grant");
    // The closure at the rejoin over EVERY pool state: the own-residue
    // cover is a joiner cycle, and a quiet cycle shrinks the pool's
    // surplus above its target — queued for return, or already returned
    // over the wire (Issue 5) — never leaked.
    assert_eq!(
        granted1,
        claimed1 + returned1 + unclaimed1 + queued1,
        "the closure holds at the rejoin"
    );
    assert_eq!(
        unclaimed1 + queued1 + returned1,
        unclaimed0,
        "the pool is WHOLE at the rejoin — kept, queued or returned by the cover's shrink, \
         nothing claimed and nothing lost (claimed {claimed0} → {claimed1}; restored {})",
        s.pool_restored_extents
    );
    assert_eq!(claimed1, claimed0, "no pool extent reads claimed");
    // The census restored AT LEAST the unnamed runs; beside them it may
    // restore a pre-cycle lazy mint's image (a root the replay re-mints —
    // its claim is journaled by the next cycle, which never ran, so no
    // record names it: dead, back to the pool).
    assert!(
        s.pool_restored_extents >= unclaimed0 - named0,
        "the census restored the unnamed runs ({} of at least {})",
        s.pool_restored_extents,
        unclaimed0 - named0
    );
    let orphans = avol.c13_orphan_image_extents().await.unwrap();
    assert!(
        orphans.is_empty(),
        "no C13 orphan at the rejoined joiner: {orphans:?}"
    );
    // At rest — one wire cadence returns the queued surplus — the
    // three-term closure the review named holds against the record.
    avol.checkpoint_now().await.unwrap();
    let (claimed2, unclaimed2, queued2, returned2, granted2) = region_faces(&avol);
    assert_eq!(queued2, 0, "the surplus went back");
    assert_eq!(
        granted2,
        claimed2 + returned2 + unclaimed2,
        "extent_grant_extents ≡ claimed + returned + unclaimed at rest"
    );
    assert_all_resolve(&again, shared, &files).await;
    assert_supply_gauges_zero(&avol, "the rejoined joiner");
    shutdown(&again).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 4 — a sized join on a FRAGMENTED heap never
// refuses.
// ---------------------------------------------------------------------------

/// **A sized join on a fragmented heap carves the largest runs that fit
/// the page's table, or the floor — never a refusal** (PR 13g review round
/// 1, Issue 4). `manager_join_appender` carved `ring_bytes` lowest-free-
/// first and refused `Corrupt("… would take N segments … lower
/// SQUEEZEFS_SYM_RING_KB or raise --meta-node-kib")` when the claims
/// coalesced into more than `RING_SEGMENTS_MAX` = 8 runs. Every PR-2 join
/// carved the 512 KiB floor (two extents — eight runs unreachable in
/// practice); the identity's hint now carves its GROWN size (4–32 MiB) at
/// the very moment the heap is most fragmented — after the storm that
/// grew it — so the crash-rejoin the hint exists for was the case that
/// refused, naming a knob the operator never set. Now the carve keeps the
/// LARGEST runs that fit half the table (growth's room), or the whole
/// table when the half is under the floor, and releases the rest
/// (`appender::ring_segments_that_fit`); a heap so fragmented that even
/// eight runs are under the floor still joins at what fits. The pin:
/// every other extent of a joiner's join grant returned (36 one-extent
/// holes below every other free run), then a fresh identity's join
/// asking a 2 MiB ring — 32 lowest-free extents in 32 holes. RED before
/// the fix: `Corrupt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sized_join_on_a_fragmented_heap_carves_what_fits_the_table_never_refusing() {
    use squeezefs::meta_backend::kv::appender::{
        GrantRun, RING_SEGMENTS_MAX, SYM_RING_FLOOR_BYTES,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    // Every other extent of the joiner's join grant returned by the
    // manager: 36 one-extent holes at the bottom of the free heap (the
    // pool a quiet cadence keeps — no shrink re-shapes the heap under the
    // pin; the joiner stays idle).
    let record = mvol.extent_grant_record(id).await.unwrap();
    let extents: Vec<u64> = record.extents().collect();
    let holes: Vec<GrantRun> = extents
        .iter()
        .step_by(2)
        .map(|e| GrantRun { start: *e, len: 1 })
        .collect();
    assert!(
        holes.len() >= 32,
        "the join grant leaves {} holes",
        holes.len()
    );
    let (cleared, _) = mvol.manager_return_runs(id, &holes).await.unwrap();
    assert_eq!(cleared, holes.len() as u64, "the holes are free");
    // A fresh identity's SIZED join: a 2 MiB ring = 32 extents, carved
    // lowest-free-first into the 32 holes.
    let fresh = joiner_identity(&mvol, 7).await;
    let want = 2 * 1024 * 1024u64;
    let out = mvol
        .manager_join_appender(fresh, want)
        .await
        .expect("a sized join on a fragmented heap never refuses");
    assert!(
        out.ring_segments.len() <= RING_SEGMENTS_MAX,
        "the ring fits the page's table ({} segments)",
        out.ring_segments.len()
    );
    let ring_bytes: u64 = out.ring_segments.iter().map(|s| s.len).sum();
    assert!(
        ring_bytes >= SYM_RING_FLOOR_BYTES,
        "the ring is at least the floor ({ring_bytes} bytes in {} segments)",
        out.ring_segments.len()
    );
    assert!(
        ring_bytes <= want,
        "…and never more than the ask ({ring_bytes})"
    );
    // What the carve did not keep went back to the heap: the free count
    // moved by exactly the ring (+ the join's directory extent, when the
    // chain grew) — read through the grant closure at the manager.
    let m = mvol.appender_stats().unwrap();
    assert_eq!(
        m.grant_granted,
        m.grant_claimed + m.grant_returned + m.grant_unclaimed,
        "the grant closure holds"
    );
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 5 — the pool SHRINKS on a quiet cadence to
// its target; the ask fires on headroom alone.
// ---------------------------------------------------------------------------

/// **A quiet joiner's standing pool above its target returns the surplus
/// at the cadence, smallest runs first, and asks for nothing while its
/// headroom holds** (PR 13g review round 1, Issue 5). The recycle
/// re-pooled released images only while `unclaimed < keep`, and NOTHING
/// returned a standing pool above the derived size: the carve overshoots
/// by the unnamed part (the page names ≤ 4 runs of a larger pool),
/// pressure cycles inflate it, the join's hint starts a fresh incarnation
/// at the last storm's size — the fleet row read 430–457 unclaimed beside
/// 66–93 claimed on a quiet joiner for its lifetime, up to `free/(4N)`.
/// And the due law `(refill_due && unclaimed < derived) || unclaimed <
/// derived + promised` fired at every cadence a promise was outstanding
/// against a pool the recycle had capped at `derived`. Now ONE target
/// (`appender::joined_pool_target`: `max(derived + promised, the join's
/// cost class FLOOR + M)`, cap-bounded) is the recycle's keep, the
/// shrink's mark and the ask's size, and the ask fires on `headroom() <
/// derived / 2` alone. The pin: the joiner's pool grown to 200 by an
/// explicit ask, then three quiet cadences — the pool at the target (the
/// join's own size, the rotor unminted), the manager's record shrunk with
/// it, `ReturnExtents` issued once, no `ExtentGrant` issued. RED before
/// the fix: the pool stays at 200 for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_joiners_pool_above_its_target_shrinks_at_the_cadence_and_asks_for_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let faces = |vol: &KvMetaBackend| {
        let s = vol.appender_stats().unwrap();
        let own = s
            .regions
            .iter()
            .find(|r| r.id == id)
            .expect("the region")
            .clone();
        let j = vol.joined_stats().unwrap();
        (
            own.grant_unclaimed,
            j.wire_extent_grants,
            j.wire_extent_returns,
        )
    };
    let rotor = jvol.slot_lease_stats().expect("the plane").rotor;
    let pool_floor = squeezefs::meta_backend::kv::appender::GRANT_EXTENTS_FLOOR + rotor;
    let (unclaimed0, _, _) = faces(&jvol);
    assert_eq!(unclaimed0, pool_floor, "the join's grant is the pool floor");
    // The pool grown well past every target: an explicit ask.
    jvol.joined_extent_grant(200).await.unwrap();
    let (grown, grants1, returns1) = faces(&jvol);
    assert!(grown >= 200, "the pool grew ({grown})");
    // Three QUIET cadences (the joiner's own cycles; nothing dirty, no
    // pressure): the surplus returns, the pool settles at its target.
    for _ in 0..3 {
        jvol.checkpoint_now().await.unwrap();
    }
    let (settled, grants2, returns2) = faces(&jvol);
    assert_eq!(
        settled, pool_floor,
        "a quiet pool shrinks to its target — the join's cost class (grown {grown})"
    );
    assert!(
        returns2 > returns1,
        "the surplus went back as ReturnExtents ({returns1} → {returns2})"
    );
    assert_eq!(grants2, grants1, "no ExtentGrant on a quiet cadence");
    let record = mvol.extent_grant_record(id).await.unwrap();
    let own = jvol
        .appender_stats()
        .unwrap()
        .regions
        .iter()
        .find(|r| r.id == id)
        .cloned()
        .unwrap();
    assert_eq!(
        record.len(),
        own.grant_claimed + own.grant_unclaimed + own.grant_pending + own.grant_returnable,
        "the manager's record shrank with the pool"
    );
    // The published closure law holds across the shrink: what left the
    // pool for the manager reads RETURNED (`extent_grant_extents ≡
    // claimed + returned + unclaimed`, §11).
    let s = jvol.appender_stats().unwrap();
    assert_eq!(
        s.grant_granted,
        s.grant_claimed + s.grant_returned + s.grant_unclaimed,
        "the shrink's surplus counts as returned (granted {}, held {}, returned {}, unclaimed {})",
        s.grant_granted,
        s.grant_claimed,
        s.grant_returned,
        s.grant_unclaimed
    );
    // Three more: nothing moves.
    for _ in 0..3 {
        jvol.checkpoint_now().await.unwrap();
    }
    let (still, grants3, returns3) = faces(&jvol);
    assert_eq!(still, pool_floor);
    assert_eq!(grants3, grants2, "still no ask");
    assert_eq!(returns3, returns2, "nothing more to return");
    assert_supply_gauges_zero(&jvol, "joiner");
    shutdown(&joiner).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 2, Issue 14 — a carve whose reply was lost is never a
// fresh join's phantom claim.
// ---------------------------------------------------------------------------

/// **A wire `ExtentGrant` carve whose REPLY was lost is reconciled at the
/// id's next FRESH join, never inherited as a phantom claim** (PR 13g
/// review round 2, Issue 14). The verb's idempotency witness is the
/// joiner's PAGE word (§5.3.5): a carve that lands while the reply is
/// lost is in the record and in no page word; the joiner's RAM never
/// learns it, its leave returns what it knows, and the record keeps the
/// carve under a `Free` page. A fresh identity taking that page (the
/// lowest Free) recovered its grant as `record ∖ page word` = CLAIMED —
/// phantoms no tree reaches, no census at a fresh join (only an
/// own-residue open runs one), extents lost to the heap for the id's
/// life. The law: a fresh join over a `Free` page whose record is not
/// empty RETURNS the residue before its own grant
/// (`appender_join_residue_returned`), so the fresh joiner's grant is its
/// grant alone and its RAM claims nothing. RED before the fix: the fresh
/// joiner's `grant_claimed` reads the residue's count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carve_whose_reply_was_lost_is_reconciled_at_the_ids_fresh_join() {
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
    let files = create_files(&joiner, shared, "x", 8).await;
    jvol.checkpoint_now().await.unwrap();
    let record0 = record_extents(&mvol, id).await;
    // The LOST reply: the manager serves the joiner's wire ask (the served
    // side, verbatim) and the reply never reaches the joiner — its RAM
    // pool, and so its page word and its leave, never learn the carve.
    let want = u32::try_from(record0.len() + 24).unwrap();
    mvol.manager_extent_grant(id, want).await.unwrap();
    let lost: std::collections::BTreeSet<u64> = record_extents(&mvol, id)
        .await
        .difference(&record0)
        .copied()
        .collect();
    assert!(
        !lost.is_empty(),
        "the premise: the served ask carved extents the joiner never heard of"
    );
    let residue0 = mvol.appender_stats().unwrap().join_residue_returned;
    // The joiner leaves: the ring back, the pool IT knows returned, the
    // page Free — the lost carve stays in the record.
    shutdown(&joiner).await;
    drop(jvol);
    let page = page_of(&uris[0], &mvol, id).await.unwrap();
    assert_eq!(page.state, AppenderState::Free, "the joiner left");
    let residue = record_extents(&mvol, id).await;
    assert_eq!(
        residue, lost,
        "the premise: the leave returned what the joiner knew; the lost carve stands in the record"
    );
    for e in &lost {
        assert!(
            mvol.allocator().is_allocated(*e),
            "held by the record ({e})"
        );
    }
    // A FRESH identity joins and takes the Free page (the lowest).
    let fresh = join(&uris, &venue, &mvol, 4).await;
    let fvol = Arc::clone(&fresh.volumes[0]);
    let fid = fvol.appender_stats().unwrap().appender_id;
    assert_eq!(fid, id, "the premise: the fresh join reuses the Free page");
    // The law: the record at the fresh join is the fresh grant alone — the
    // residue returned first (a lost extent is then anybody's to claim
    // lowest-free-first: the fresh grant's, the manager's own images'; a
    // PHANTOM is one still in a grant record no RAM set answers for),
    // the fresh RAM claims nothing, the gauge names the residue.
    let record_fresh = record_extents(&mvol, fid).await;
    let fresh_stats = fvol.appender_stats().unwrap();
    let region = fresh_stats
        .regions
        .iter()
        .find(|r| r.id == fid)
        .expect("the fresh joiner's own region");
    assert_eq!(
        region.grant_claimed, 0,
        "a fresh join claims nothing — the residue is not its images ({region:?})"
    );
    assert_eq!(
        record_fresh.len() as u64,
        region.grant_claimed
            + region.grant_unclaimed
            + region.grant_pending
            + region.grant_returnable,
        "the record IS the fresh joiner's RAM sets (PR 3's law)"
    );
    let granted_elsewhere: std::collections::BTreeSet<u64> = mvol
        .extent_grant_records()
        .await
        .unwrap()
        .into_iter()
        .filter(|(id, _)| *id != fid)
        .flat_map(|(_, r)| r.extents().collect::<Vec<_>>())
        .collect();
    for e in &lost {
        assert!(
            !granted_elsewhere.contains(e),
            "a lost extent was returned — it sits in no other appender's grant record ({e})"
        );
    }
    assert_eq!(
        mvol.appender_stats().unwrap().join_residue_returned - residue0,
        lost.len() as u64,
        "the residue's count on appender_join_residue_returned"
    );
    assert_all_resolve(&manager, shared, &files).await;
    assert_supply_gauges_zero(&fvol, "the fresh joiner");
    shutdown(&fresh).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 2, Issue 16 — the shrink's return never leaves a
// stale page word, and a rejoin adopts the page word ∩ the record.
// ---------------------------------------------------------------------------

/// The extents appender `id`'s tree-0 grant record names at the manager.
async fn record_extents(mvol: &KvMetaBackend, id: u32) -> std::collections::BTreeSet<u64> {
    mvol.extent_grant_record(id)
        .await
        .unwrap()
        .extents()
        .collect()
}

/// **The shrink's return follows a page rewrite — the device page never
/// names an extent the record no longer grants** (PR 13g review round 2,
/// Issue 16b — the ordering half). The Issue 5 shrink took UNCLAIMED
/// extents out of the pool and shipped them as `ReturnExtents` AFTER the
/// cycle's page write had named the pool's largest runs: the manager
/// cleared their bits and re-granted them lowest-free-first while the
/// joiner's page still named them until its NEXT page write — a cycle
/// away. A joiner killed inside that window rejoined over the stale word
/// (`RegionGrant::recover` adopted the page word whole) able to CLAIM
/// extents another appender already held: two custodians of one extent,
/// a node image overwritten, acked loss. PR 3's trim never left the
/// window — the page was set after the excess moved to the returnable
/// batch. Now the shrink rewrites the page (the pool without the surplus,
/// both directory slots, barriered) BEFORE `ReturnExtents` — the
/// discipline `wire_extent_refill` keeps before every ask. The pin: a
/// pool above its target, ONE quiet cadence whose shrink returns a batch
/// R, then the DEVICE page's word ∩ R = ∅ and ⊆ the record; the joiner
/// killed before its next page write, R re-granted to a second joiner,
/// the identity rejoined — the rejoined pool holds none of R and the two
/// grants are disjoint. RED before the fix: the page names R after the
/// cadence, and the rejoin holds R beside the second joiner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shrinks_return_never_leaves_the_page_naming_a_returned_extent() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let own = joiner
        .create(1, "own", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let files = create_files(&joiner, own, "f", 8).await;
    jvol.checkpoint_now().await.unwrap();
    // The pool above its target: an explicit ask (the refill's page write
    // names the pool's largest runs — the returned extents among them).
    let got = jvol.joined_extent_grant(200).await.unwrap();
    assert!(got >= 64, "the pool grew by {got}");
    let record0 = record_extents(&mvol, id).await;
    let page0 = page_of(&uris[0], &mvol, id).await.unwrap();
    let named0: std::collections::BTreeSet<u64> = page0
        .grant
        .iter()
        .flat_map(|r| r.start..r.start + u64::from(r.len))
        .collect();
    assert!(
        named0.is_subset(&record0),
        "the page word is inside the record before the cadence"
    );
    let returns0 = jvol.joined_stats().unwrap().wire_extent_returns;
    // ONE quiet cadence: the shrink returns the surplus.
    jvol.checkpoint_now().await.unwrap();
    assert!(
        jvol.joined_stats().unwrap().wire_extent_returns > returns0,
        "the shrink returned a batch"
    );
    let record1 = record_extents(&mvol, id).await;
    let returned: std::collections::BTreeSet<u64> = record0.difference(&record1).copied().collect();
    assert!(
        returned.len() >= 32,
        "the premise: the shrink returned {} extent(s)",
        returned.len()
    );
    assert!(
        named0.intersection(&returned).count() > 0,
        "the premise: the pre-cadence page named some of the returned extents"
    );
    // The ordering law on the DEVICE: the page after the cadence names no
    // returned extent, and every extent it names is the record's.
    let page1 = page_of(&uris[0], &mvol, id).await.unwrap();
    let named1: std::collections::BTreeSet<u64> = page1
        .grant
        .iter()
        .flat_map(|r| r.start..r.start + u64::from(r.len))
        .collect();
    assert!(
        named1.is_disjoint(&returned),
        "the device page names {} returned extent(s) after the shrink's return — the stale word \
         a crash-rejoin would adopt",
        named1.intersection(&returned).count()
    );
    assert!(
        named1.is_subset(&record1),
        "the page word is inside the record"
    );
    // The kill — before the joiner's next page write.
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    // The returned extents re-granted: a SECOND joiner's ask carves
    // lowest-free-first.
    let second = join(&uris, &venue, &mvol, 4).await;
    let svol = Arc::clone(&second.volumes[0]);
    let sid = svol.appender_stats().unwrap().appender_id;
    assert_ne!(sid, id);
    svol.joined_extent_grant(200).await.unwrap();
    let second_record = record_extents(&mvol, sid).await;
    assert!(
        !second_record.is_disjoint(&returned),
        "the premise: the second joiner was granted {} of the returned extents",
        second_record.intersection(&returned).count()
    );
    // The identity's rejoin over its Live page: no returned extent in its
    // RAM grant, the two grants disjoint.
    let again = join(&uris, &venue, &mvol, 3).await;
    let avol = Arc::clone(&again.volumes[0]);
    assert_eq!(avol.appender_stats().unwrap().appender_id, id);
    let held: Vec<u64> = returned
        .iter()
        .copied()
        .filter(|e| avol.grant_holds(id, *e))
        .collect();
    assert!(
        held.is_empty(),
        "the rejoined joiner holds {} returned extent(s) another appender was granted — two \
         custodians ({held:?})",
        held.len()
    );
    let mine = record_extents(&mvol, id).await;
    assert!(
        mine.is_disjoint(&second_record),
        "the two grant records are disjoint"
    );
    assert_all_resolve(&again, own, &files).await;
    assert_supply_gauges_zero(&avol, "the rejoined joiner");
    assert_supply_gauges_zero(&svol, "the second joiner");
    shutdown(&again).await;
    shutdown(&second).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **A rejoin adopts the page word ∩ the record — a page-named extent the
/// record no longer grants is DROPPED, counted** (PR 13g review round 2,
/// Issue 16a — the structural belt). The record is the manager's truth
/// (PR 3's law `record ⊆ claimed ∪ unclaimed ∪ pending ∪ returnable`);
/// the manager already screens ITS answers against a stale word
/// (`extent_grant_stale_page_words`), the joiner's own `recover` did not.
/// The pin forges the window Issue 16b closes: after a shrink's return
/// and the kill, the joiner's page is rewritten with its PRE-cadence word
/// (naming the returned extents, now a second joiner's), the identity
/// rejoins — none of them in its grant, `appender_stale_page_words_
/// dropped` moved by their count, the record whole in the RAM sets. RED
/// before the fix: the forged word's extents read UNCLAIMED at the
/// rejoin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoin_adopts_the_page_word_intersected_with_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let own = joiner
        .create(1, "own", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let files = create_files(&joiner, own, "f", 8).await;
    jvol.checkpoint_now().await.unwrap();
    jvol.joined_extent_grant(200).await.unwrap();
    let record0 = record_extents(&mvol, id).await;
    let stale_word = page_of(&uris[0], &mvol, id).await.unwrap().grant;
    let stale_named: std::collections::BTreeSet<u64> = stale_word
        .iter()
        .flat_map(|r| r.start..r.start + u64::from(r.len))
        .collect();
    jvol.checkpoint_now().await.unwrap();
    let record1 = record_extents(&mvol, id).await;
    let returned: std::collections::BTreeSet<u64> = record0.difference(&record1).copied().collect();
    let phantom: std::collections::BTreeSet<u64> =
        stale_named.intersection(&returned).copied().collect();
    assert!(
        phantom.len() >= 16,
        "the premise: the pre-cadence word names {} returned extent(s)",
        phantom.len()
    );
    drop(jvol);
    drop(joiner);
    park_gate::test_reset();
    squeezefs::meta_backend::kv::alloc_lease::test_clear_holdings();
    let second = join(&uris, &venue, &mvol, 4).await;
    let svol = Arc::clone(&second.volumes[0]);
    let sid = svol.appender_stats().unwrap().appender_id;
    svol.joined_extent_grant(200).await.unwrap();
    let second_record = record_extents(&mvol, sid).await;
    assert!(
        !second_record.is_disjoint(&phantom),
        "the premise: the second joiner holds {} of the phantom extents",
        second_record.intersection(&phantom).count()
    );
    // The forged window: the identity's page with its PRE-cadence word.
    rewrite_page(&uris[0], id, |p| {
        p.identity = identity;
        p.grant = stale_word.clone();
    })
    .await;
    let dropped0 = mvol.appender_stats().unwrap().stale_page_words_dropped;
    let again = join(&uris, &venue, &mvol, 3).await;
    let avol = Arc::clone(&again.volumes[0]);
    assert_eq!(avol.appender_stats().unwrap().appender_id, id);
    let held: Vec<u64> = phantom
        .iter()
        .copied()
        .filter(|e| avol.grant_holds(id, *e))
        .collect();
    assert!(
        held.is_empty(),
        "the rejoin adopted {} phantom extent(s) off the stale page word ({held:?})",
        held.len()
    );
    let s = avol.appender_stats().unwrap();
    assert_eq!(
        s.stale_page_words_dropped - dropped0,
        phantom.len() as u64,
        "every phantom extent counted (appender_stale_page_words_dropped)"
    );
    // The record whole in the RAM sets (PR 3's law).
    for e in record_extents(&mvol, id).await {
        assert!(
            avol.grant_holds(id, e),
            "record extent {e} in the RAM grant"
        );
    }
    assert_all_resolve(&again, own, &files).await;
    shutdown(&again).await;
    shutdown(&second).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 11 — the joiner's SMO rate reads its OWN
// volume's images.
// ---------------------------------------------------------------------------

/// **A joiner's SMO-rate input is the images ITS OWN passes wrote — exact
/// per volume** (PR 13g review round 1, Issue 11). The round-0 fold fed
/// `smos_this_cycle` from the process-wide `META_KV_NODE_{COMPACTIONS,
/// SPLITS,MERGES}` + `ROOT_COLLAPSES` deltas around each node's flush: a
/// mount's OTHER volumes' concurrent SMOs inflated this volume's derived
/// grant (the safe direction, bounded by the cap — and wrong), a split
/// counted one for its two or three images, and the threshold
/// maintenance pass's SMOs — the same grant's consumers — counted for
/// nothing; `SmoContext::images_written`, added by this PR for exactly the
/// per-volume attribution, is exact and is what a grant extent is
/// consumed as. The pin: a joiner storms one directory past its leaf
/// splits (its cadence running as it will — the storm's cycles do the
/// SMOs), then one final cycle; Σ the counts its folds consumed
/// (`smo_folded_total`) ≡ Σ the images its passes measured
/// (`checkpoint_pass_images_total` — the flush pass and the maintenance
/// pass alike), and the rate moved. RED before the fix: the folds summed
/// SMOs — a split is one SMO writing two or three images, so the sums
/// differ whenever the storm split a leaf (asserted: `meta_kv_node_
/// splits` moved).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_smo_rate_reads_its_own_volumes_images() {
    use squeezefs::meta_backend::kv::META_KV_NODE_SPLITS;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let region = |vol: &KvMetaBackend| {
        vol.appender_stats()
            .unwrap()
            .regions
            .iter()
            .find(|r| r.id == id)
            .expect("the region")
            .clone()
    };
    let own = joiner
        .create(1, "own", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .unwrap()
        .ino;
    let splits0 = META_KV_NODE_SPLITS.load(Relaxed);
    // The storm: enough dentries under one directory to split its leaf
    // (the joiner's slot tree) more than once — the joiner's cycles run
    // their SMOs as the storm goes.
    let files = create_files(&joiner, own, "s", 6_000).await;
    jvol.checkpoint_now().await.unwrap();
    let r = region(&jvol);
    let images = jvol.checkpoint_pass_images_total();
    assert!(
        META_KV_NODE_SPLITS.load(Relaxed) > splits0,
        "the premise: the storm split a leaf"
    );
    assert!(
        images >= 2,
        "the premise: the passes wrote images ({images})"
    );
    assert_eq!(
        r.smo_folded_total, images,
        "the folds consumed exactly the images this volume's passes measured (last cycle {}, \
         rate EWMA {} milli/s)",
        r.smo_last_cycle, r.smo_ewma_milli
    );
    assert!(r.smo_ewma_milli > 0, "the rate moved");
    assert_all_resolve(&joiner, own, &files).await;
    assert_supply_gauges_zero(&jvol, "joiner");
    shutdown(&joiner).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 13 — one directory read per wire grant.
// ---------------------------------------------------------------------------

/// **A wire appender's derived-size `ExtentGrant` walks the appender
/// directory ONCE** (PR 13g review round 1, Issue 13). The manager's verb
/// wall is the F-B1 term this rung prices, and every whole-directory walk
/// on a wire verb is a term of it: the round-0 grant read the directory
/// for the caller's unclaimed remainder, AGAIN for the hint's identity
/// (`wire_appender_identity`) and a third time to rewrite the wire page's
/// grant word — three walks of the same pages. The pin: the process-wide
/// census `appender::directory_reads()` moves by exactly one across a
/// joiner's explicit ask above the floor (the hint path) that carves.
/// RED before the fix: three.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wire_extent_grant_walks_the_appender_directory_once() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let unclaimed = |vol: &KvMetaBackend| {
        vol.appender_stats()
            .unwrap()
            .regions
            .iter()
            .find(|r| r.id == id)
            .expect("the region")
            .grant_unclaimed
    };
    let before = unclaimed(&jvol);
    let want = u32::try_from(before).unwrap() + 24;
    assert!(
        u64::from(want) > squeezefs::meta_backend::kv::appender::GRANT_EXTENTS_FLOOR,
        "the ask rides the hint path"
    );
    let reads0 = squeezefs::meta_backend::kv::appender::directory_reads();
    let got = jvol.joined_extent_grant(want).await.unwrap();
    let reads1 = squeezefs::meta_backend::kv::appender::directory_reads();
    assert!(got > 0, "the ask carved");
    assert!(unclaimed(&jvol) > before, "the pool grew");
    assert_eq!(
        reads1 - reads0,
        1,
        "a wire ExtentGrant walks the directory once (appender_directory_reads)"
    );
    // The hint followed the ask (the path the second read served).
    let identity = jvol.joined_wire().unwrap().identity;
    let hint = mvol
        .appender_hint_for(identity.node_token, identity.mount_slot)
        .await
        .unwrap();
    assert_eq!(
        hint.grant_extents,
        u64::from(want),
        "the hint rode the carve"
    );
    assert_supply_gauges_zero(&jvol, "joiner");
    shutdown(&joiner).await;
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 8 — `GrowRing` honours the set-wide ring
// budget.
// ---------------------------------------------------------------------------

/// **`GrowRing` is bounded by the volume's RING BUDGET, not the per-
/// appender ceiling alone** (PR 13g review round 1, Issue 8; design §5.3.1
/// / §1.6: `heap/16` is "the ONE hard resource a join refuses on" —
/// `appenders_capacity`). The first build clamped the ask to `ceiling −
/// the page's ring` and never to the budget: N rings grown toward the
/// 32 MiB ceiling exceed `heap/16` (32 × 32 MiB against a 4 GiB heap's
/// 256 MiB), eating the image heap the budget reserves. Now the room is
/// `min(ceiling − ring, budget − Σ every Live page's ring)` and an ask
/// past it answers `None`; the remainder is published
/// (`appender_ring_budget_remaining_bytes`). The pin: on the 64 MiB
/// fixture the budget is 3.75 MiB against the manager's 1 MiB fixed ring
/// and the joiner's 512 KiB — an 8 MiB ask (the per-appender ceiling) is
/// answered the budget's remainder, the next ask nothing. RED before the
/// fix: the first ask carves 7.5 MiB.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grow_ring_never_carves_past_the_volumes_ring_budget() {
    use squeezefs::meta_backend::kv::appender::ring_budget_bytes;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    let budget = ring_budget_bytes(mvol.superblock().heap.len);
    // Every Live page's ring: the manager's (its fixed ring less the page
    // slots) and the joiner's.
    let in_use =
        mvol.appender_stats().unwrap().ring_bytes + jvol.appender_stats().unwrap().ring_bytes;
    let remaining = budget.saturating_sub(in_use);
    assert!(
        remaining > 0 && remaining < 8 * 1024 * 1024,
        "the premise: the budget ({budget}) leaves {remaining} bytes under the 8 MiB ceiling"
    );
    let seg = mvol
        .manager_grow_ring_wire(id, 8 * 1024 * 1024, &peer_of(&identity))
        .await
        .expect("GrowRing")
        .expect("the budget's remainder is carvable");
    assert!(
        seg.len <= remaining,
        "the carve is bounded by the ring budget's remainder ({} ≤ {remaining})",
        seg.len
    );
    assert!(
        seg.len + 65_536 > remaining,
        "…and takes it to the last extent ({} of {remaining})",
        seg.len
    );
    let m = mvol.appender_stats().unwrap();
    assert!(
        m.ring_budget_remaining_bytes < 65_536,
        "the published remainder reads the budget spent ({})",
        m.ring_budget_remaining_bytes
    );
    // The joiner names the segment (as its own growth would) so the next
    // ask is a fresh one, not the pending witness's replay…
    // …which the pin cannot do from outside the joiner; the pending
    // witness answers the same segment VERBATIM and the budget arithmetic
    // holds either way: nothing more is carved.
    let free_before = mvol.free_extents();
    let again = mvol
        .manager_grow_ring_wire(id, 8 * 1024 * 1024, &peer_of(&identity))
        .await
        .expect("GrowRing");
    assert_eq!(again, Some(seg), "the pending segment, verbatim");
    assert_eq!(mvol.free_extents(), free_before, "nothing more carved");
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 9 — a short carve spends no table slot.
// ---------------------------------------------------------------------------

/// **`GrowRing` answers `None` for a run under half the ask, keeping the
/// page's table for a doubling-class segment** (PR 13g review round 1,
/// Issue 9). The verb keeps the LONGEST adjacent run of its claims; on a
/// fragmented heap that is one extent, the joiner's table
/// (`RING_SEGMENTS_MAX` = 8) is spent on eight such answers and the ring
/// is pinned at floor + 2 MiB with `joined_ring_grow_declined` for its
/// life. Now a run below half the ask (PR 2's doubling step — a segment
/// of the ring's own size) is released and the ask answered `None`: the
/// ring stays, the table stays, the joiner asks again at its next cycle
/// (a heap that de-fragments answers it then; one that does not leaves
/// the ring at its size under pressure cycles — the stated outcome). The
/// pin: 36 one-extent holes at the bottom of the free heap (every other
/// extent of the joiner's join grant, returned), a 2 MiB ask — `None`,
/// nothing claimed, the page's table untouched. RED before the fix: one
/// 64 KiB segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grow_ring_on_a_fragmented_heap_spends_no_table_slot_on_a_short_run() {
    use squeezefs::meta_backend::kv::appender::GrantRun;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 512 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 3).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let id = jvol.appender_stats().unwrap().appender_id;
    let identity = jvol.joined_wire().unwrap().identity;
    // The holes: every other extent of the joiner's join grant returned
    // by the manager (the lowest free extents of the heap, one apart —
    // the pool the joiner's quiet cadence keeps, so no shrink re-shapes
    // the heap under the pin; the joiner stays idle).
    let extents: Vec<u64> = mvol
        .extent_grant_record(id)
        .await
        .unwrap()
        .extents()
        .collect();
    let holes: Vec<GrantRun> = extents
        .iter()
        .step_by(2)
        .map(|e| GrantRun { start: *e, len: 1 })
        .collect();
    assert!(
        holes.len() >= 32,
        "the join grant leaves {} holes",
        holes.len()
    );
    let (cleared, _) = mvol.manager_return_runs(id, &holes).await.unwrap();
    assert_eq!(cleared, holes.len() as u64);
    let segments_before = page_of(&uris[0], &mvol, id).await.unwrap().segments.len();
    let free_before = mvol.free_extents();
    let out = mvol
        .manager_grow_ring_wire(id, 2 * 1024 * 1024, &peer_of(&identity))
        .await
        .expect("GrowRing");
    assert_eq!(out, None, "a run under half the ask spends no table slot");
    assert_eq!(mvol.free_extents(), free_before, "nothing claimed");
    assert_eq!(
        page_of(&uris[0], &mvol, id).await.unwrap().segments.len(),
        segments_before,
        "the table is untouched"
    );
    assert_must_stay_zero(&mvol, "manager");
    shutdown(&joiner).await;
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g review round 1, Issue 3 — a carve or a return wider than ONE
// control entry.
// ---------------------------------------------------------------------------

/// **A derived-size carve past one control entry lands WHOLE, and the
/// leave returns the whole pool** (PR 13g review round 1, Issue 3). An
/// allocator delta frames at `record_frame_len(8, 1)` = 25 B and a
/// control entry holds `MAX_ENTRY_LEN` = 128 KiB, so a carve past ≈ 5,200
/// extents — the joiner's `derived + promised` ask at the pin's own 92
/// SMO/s is ≈ 8,300 — was `EntryTooLarge`, undone, and re-asked at every
/// cadence and by the reactive ladder: every refill failing on a heap
/// with room (`free > 4N × 5,200` extents — production geometry), the
/// grant exhausting, SMOs deferring, the ring filling — the EAGAIN class
/// on a healthy heap. The leave's return of such a pool was one entry
/// too, and left the page `Live`. Now the manager CHUNKS the carve and
/// the return by `journal::pack_entries` under a derived per-entry bound
/// (the entry cap less the rewritten grant record's frame — each extent
/// adds at most one run — over the delta's frame; PR 4's leave law), each
/// chunk a consistent record state: a heap 4 GiB wide (the wire cap
/// `free/(4 × appenders)` admits the ask), a wire joiner, ONE
/// `ExtentGrant` of its own for a third more than one entry's worth of
/// deltas — answered whole in more than one entry, the grant closure
/// exact, the pool holding it — then
/// the joiner's clean leave returns the whole pool, the page `Free`, the
/// record gone. RED before the fix: `EntryTooLarge`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carve_past_one_control_entry_lands_whole_and_the_leave_returns_it_whole() {
    use squeezefs::meta_backend::kv::journal::{entry_payload_cap, record_frame_len};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // A SPARSE 4 GiB member: 65,536 extents of 64 KiB — the wire cap
    // admits an ask past one entry's worth at two appenders.
    let uris = format_stamped_set_with_config_len(dir.path(), 1, 4 * 1024 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let joiner = join(&uris, &venue, &mvol, 1).await;
    let jvol = Arc::clone(&joiner.volumes[0]);
    let js = jvol.appender_stats().expect("a joined appender");
    let id = js.appender_id;
    assert_ne!(id, 0, "the joiner is a wire appender");
    let per_entry = entry_payload_cap() / record_frame_len(8, 1);
    let want = per_entry + per_entry / 3;
    let cap = squeezefs::meta_backend::kv::appender::grant_extents_wire_cap(mvol.free_extents(), 2);
    assert!(
        want < cap,
        "the premise: the heap-share cap ({cap}) admits an ask of {want} extents"
    );
    let before = mvol.appender_stats().expect("the manager's faces");
    let entries_before = mvol.journal_ring().written_entries();
    // The joiner's page names its whole (contiguous) initial pool, so the
    // manager's remainder word is the joiner's unclaimed count.
    let remainder = jvol
        .appender_stats()
        .unwrap()
        .regions
        .iter()
        .find(|r| r.id == id)
        .expect("the joiner's region")
        .grant_unclaimed;
    // The joiner's OWN ask over the wire (the cadence's and the reactive
    // ladder's one function): the manager carves, the runs land in the
    // joiner's pool.
    let carved = jvol
        .joined_extent_grant(want as u32)
        .await
        .expect("a carve wider than one control entry lands");
    let after = mvol.appender_stats().expect("the manager's faces");
    assert_eq!(
        carved,
        want - remainder,
        "the carve tops the pool up to the ask"
    );
    assert_eq!(
        after.extent_grant_extents - before.extent_grant_extents,
        carved,
        "every carved extent is counted granted"
    );
    let entries = mvol.journal_ring().written_entries() - entries_before;
    assert!(
        entries >= 2,
        "a carve of {carved} deltas past one entry's {per_entry} rides more than one control \
         entry ({entries})"
    );
    let record = mvol
        .extent_grant_record(id)
        .await
        .expect("the joiner's grant record");
    assert_eq!(
        record.len(),
        want,
        "the durable record names the whole pool"
    );
    // The joiner's leave returns the whole pool — more than one entry of
    // free deltas — and the record goes with the page.
    shutdown(&joiner).await;
    let record = mvol
        .extent_grant_record(id)
        .await
        .expect("the joiner's grant record after its leave");
    assert!(
        record.is_empty(),
        "the leave returned the whole pool ({} extents still granted)",
        record.len()
    );
    let m = mvol.appender_stats().unwrap();
    assert_eq!(m.live, 1, "the joiner's page is Free");
    assert_eq!(
        m.grant_granted,
        m.grant_claimed + m.grant_returned + m.grant_unclaimed,
        "the grant closure holds at the manager"
    );
    assert_must_stay_zero(&mvol, "manager");
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13g — F-B1: the manager's flush-ceiling term at a storm's ONSET after a
// QUIET horizon (the record's §4.4an; the box record's review, Issue 2).
// ---------------------------------------------------------------------------

/// The manager's cadence faces at one instant.
#[derive(Debug, Clone, Copy)]
struct CadenceFaces {
    checkpoints: u64,
    overruns: u64,
    term_ms: u64,
    trigger_ms: u64,
    projected_ms: u64,
    node_unit_us: u64,
    image_unit_us: u64,
}

fn cadence_faces(vol: &KvMetaBackend) -> CadenceFaces {
    let s = vol.appender_stats().expect("a forest volume");
    CadenceFaces {
        checkpoints: vol.checkpoint_seq(),
        overruns: s.flush_ceiling_overruns,
        term_ms: vol.checkpoint_term_ms(),
        trigger_ms: vol.checkpoint_trigger_ms(
            squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS as u64,
        ),
        projected_ms: vol.checkpoint_projected_ms(),
        node_unit_us: vol.checkpoint_node_unit_ns() / 1_000,
        image_unit_us: vol.checkpoint_image_unit_ns() / 1_000,
    }
}

/// One `sym-scale` row in process: the manager storms `dir` with
/// `manager_creators` unpaced creators while every joiner storms its own
/// directory with one, for `storm`, under the PRODUCT cadence alone (no
/// test-side `checkpoint_now`). Returns the creates.
async fn scale_row(
    row: u32,
    manager: &Arc<RoutedMetaBackend>,
    manager_dirs: &[u64],
    joiners: &[(Arc<RoutedMetaBackend>, u64)],
    storm: std::time::Duration,
    pace: std::time::Duration,
) -> u64 {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks = Vec::new();
    let mut writers: Vec<(Arc<RoutedMetaBackend>, u64, usize)> = Vec::new();
    for (c, dir) in manager_dirs.iter().enumerate() {
        writers.push((Arc::clone(manager), *dir, c));
    }
    for (j, d) in joiners {
        writers.push((Arc::clone(j), *d, 0));
    }
    for (w, (writer, d, c)) in writers.into_iter().enumerate() {
        let stop = Arc::clone(&stop);
        tasks.push(tokio::spawn(async move {
            let mut k = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                writer
                    .create(
                        d,
                        &format!("r{row}-w{w}-c{c}-f{k:06}"),
                        libc::S_IFREG | 0o644,
                        1000,
                        1000,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("writer {w} create: {e}"));
                k += 1;
                if !pace.is_zero() {
                    tokio::time::sleep(pace).await;
                }
            }
            k
        }));
    }
    tokio::time::sleep(storm).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    let mut created = 0u64;
    for t in tasks {
        created += t.await.expect("a creator task");
    }
    created
}

/// **F-B1's class, as the box record's review re-read it (Issue 2): the
/// FIRST storm cycle after a quiet horizon.** On the box the manager's
/// second volume tripped twice — 1,127 / 1,125 ms — each time inside the
/// first seconds of a `sym-scale` row's storm (the manager one of its N
/// writers), with the anticipated term reading 11 / 4 ms (triggers 989 /
/// 996) WHEN it tripped: the 199 quiet cycles between the rows (the
/// `rm -rf`, the joins) had pushed the previous row's 133 ms term out of
/// the 64-CYCLE horizon (`CycleTermWindow`), so the first storm cycle ran
/// at the shipped trigger with a term the horizon had forgotten, carrying
/// ≈ 130 ms of the storm's first second — and the storm's steady state
/// (the 127 / 133 ms read AFTER) did not trip again: a horizon measured
/// in CYCLES forgets a burst that quiet cycles push out.
///
/// The remedy that survives quiet is a DERIVATION off the pending work,
/// never a widened constant or a longer memory: at every tick the cadence
/// anticipates `max(horizon term, projection)`, the projection = the
/// dirty nodes × the measured per-node append wall + the images the
/// pending commits PROMISED (§4.7's admission) × the measured per-image
/// SMO wall (`checkpoint::projected_flush_wall_ns`; the units horizon
/// maxima per class, KEPT across passes that run none of the class) —
/// the storm's first cycle is priced from what it CARRIES.
///
/// The shape, the box's row sequence in process under PR 13e's own
/// method — a PARKED DEVICE (`uring_fs::arm_device_latency`, 3 ms per
/// write, no barrier latency), so a cycle's work has a wall the ceiling's
/// two-tick margin cannot absorb and the class is DETERMINISTIC. The
/// manager storms `MINT_SPREAD` FRESH directories per row (one PACED
/// creator each, 25 ms between creates — a directory under `/` mints by
/// the rotor, so 64 directories are 64 slot trees and the storm's first
/// tick dirties 64 root leaves: ≈ 260 ms of appends, two and a half
/// margins, and the pace keeps every cycle's work at that — an unpaced
/// creator set dirties hundreds of split leaves per cycle, a second of
/// flush that overruns whatever the horizon holds, the steady-state
/// shape and not the class; one directory's children land in its own
/// slot under PR 4's affinity, a handful of leaves the margin absorbs),
/// beside a joiner storming its own. A warm-up cycle
/// measures the manager's per-node AND per-image units (an appends burst
/// over the directories, a splitting burst into one more) and puts a
/// term in the horizon;
/// row 1 runs under the product cadence with the horizon HOLDING its term
/// (gate 7's evidence — no trip; the premise), the QUIET horizon (more
/// cycles than the window holds, each with nothing to flush) forgets it
/// — asserted — and row 2's ONSET into the second set of fresh
/// directories is the class. The law: no overrun on the manager's volume
/// through the onset. RED on the horizon term alone (the pin's own commit
/// — the instrument published, the trigger not yet anticipating it): the
/// onset cycle fires at the shipped trigger and lands its leaves 1,270 –
/// 1,430 ms old (3/3 at the pin's commit; 6/6 GREEN at the fix's).
///
/// **The venue's term, attributed INSIDE the pin** (review round 2, Issue
/// 19 — the reshaped pin flaked 1/9 at HEAD: one overrun with the onset
/// term 471 ms against a 557 ms projection and a 443 ms trigger, the age
/// decision ≈ 190 ms late — the dev profile's tick under CPU saturation,
/// a term the box does not have): an overrun beside a decision later
/// than the ceiling's two-tick MARGIN (`meta_kv_checkpoint_late_max_ms`
/// over the onset's cycles) lands its leaves past the ceiling whatever
/// the trigger anticipated, so that sample is VOID — logged with its
/// lateness and re-drawn into a fresh row (bounded at three draws, each
/// behind its own quiet horizon; a run that draws no valid sample fails
/// loud naming the venue) — and an overrun INSIDE the margin is the
/// cadence's, the pin's failure. Never a silent retry: every void draw
/// is stated with its rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn a_storms_onset_after_a_quiet_horizon_lands_inside_the_managers_ceiling() {
    use squeezefs::meta_backend::kv::checkpoint::{CHECKPOINT_MAX_AGE_MS, TERM_HORIZON_CYCLES};
    /// The onset draws a run may take: the first is the sample, the rest
    /// re-draws after a venue-voided one.
    const DRAWS: u32 = 3;
    let dir = cadence_venue_dir();
    let _g = SEAM.lock().await;
    reset_process_state();
    // One joiner: the manager serves a wire appender's verbs beside its
    // own storm (the box's shape); more of them on the dev profile is
    // CPU saturation, whose tick lateness is the venue's, not the class.
    let joiners = 1usize;
    // The box's 32 MiB fixed ring: the manager's cycles under the storm
    // are the AGE law's (the fixtures' 1 MiB ring would make every one
    // the ring-pressure law's, and the class here is the age decision).
    let uris = format_stamped_set_with_ring_len(
        dir.path(),
        1,
        VOL_LEN * 4 * (joiners as u64 + 1),
        32 * 1024 * 1024,
    )
    .await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    let mut daemons: Vec<(Arc<RoutedMetaBackend>, u64)> = Vec::new();
    for n in 1..=joiners {
        daemons.push((join(&uris, &venue, &mvol, n as u32).await, 0));
    }
    // The directories per row — fresh leaves at every onset: the manager's
    // `MINT_SPREAD` (one slot tree each, minted by the rotor under `/`),
    // one per joiner.
    let spread = squeezefs::meta_backend::MINT_SPREAD;
    let mut row_dirs: Vec<Vec<(Arc<RoutedMetaBackend>, u64)>> = Vec::new();
    let mut mdirs: Vec<Vec<u64>> = Vec::new();
    for row in 1..=1 + DRAWS {
        let mut dirs = Vec::new();
        for (n, (j, _)) in daemons.iter().enumerate() {
            let d = j
                .create(
                    1,
                    &format!("row{row}-w{}", n + 1),
                    libc::S_IFDIR | 0o755,
                    1000,
                    1000,
                )
                .await
                .expect("the joiner's directory")
                .ino;
            dirs.push((Arc::clone(j), d));
        }
        row_dirs.push(dirs);
        let mut mine = Vec::with_capacity(spread);
        for c in 0..spread {
            mine.push(
                manager
                    .create(
                        1,
                        &format!("row{row}-w0-d{c:02}"),
                        libc::S_IFDIR | 0o755,
                        1000,
                        1000,
                    )
                    .await
                    .expect("the manager's directory")
                    .ino,
            );
        }
        mdirs.push(mine);
    }
    // The PARKED device (PR 13e's method): every write of this volume's
    // file costs 3 ms — the onset's 64 dirty root leaves alone are ≈ 200
    // ms of flush, twice the ceiling's two-tick margin. No barrier latency:
    // a quiet cycle (the ledger, the page, the bitmap) stays a few writes.
    let path = std::path::PathBuf::from(&uris[0]);
    squeezefs::uring_fs::arm_device_latency(
        &path,
        std::time::Duration::from_millis(3),
        std::time::Duration::ZERO,
    );
    // The warm-up measures BOTH units under the parked device and puts a
    // term in the horizon (the box's manager had run SMOs for an hour when
    // its rows began; a fresh mount's first pass with a class is the
    // shipped posture — the class here is the horizon's, not the unit's):
    // one create per row-1 directory (64 appends), and a burst into one
    // more directory wide enough to split its leaf (the SMO class —
    // sixteen creators, so the conveyor batches the parked journal
    // writes), then ONE test-side cycle.
    for (c, d) in mdirs[0].iter().enumerate() {
        manager
            .create(
                *d,
                &format!("warm-{c:02}"),
                libc::S_IFREG | 0o644,
                1000,
                1000,
            )
            .await
            .expect("a warm-up create");
    }
    let warm_smo = manager
        .create(1, "warm-smo", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the SMO warm-up directory")
        .ino;
    let mut burst = Vec::new();
    for c in 0..16u32 {
        let m = Arc::clone(&manager);
        burst.push(tokio::spawn(async move {
            for k in 0..60u32 {
                m.create(
                    warm_smo,
                    &format!("warm-smo-{c:02}-{k:03}"),
                    libc::S_IFREG | 0o644,
                    1000,
                    1000,
                )
                .await
                .expect("an SMO warm-up create");
            }
        }));
    }
    for t in burst {
        t.await.expect("an SMO warm-up creator");
    }
    mvol.checkpoint_now().await.expect("the warm-up cycle");
    let f0 = cadence_faces(&mvol);
    eprintln!("F-B1 onset: warmed — the manager at {f0:?}");
    assert!(
        f0.node_unit_us >= 3_000 && f0.image_unit_us >= f0.node_unit_us,
        "the premise: the warm-up measured the manager's per-node AND per-image units under the \
         parked device ({f0:?})"
    );
    // Row 1 — the horizon HOLDS the warm-up cycle's term through the
    // storm's first cycle (gate 7's row (a): the derivation lands when the
    // horizon holds the term).
    let created1 = scale_row(
        1,
        &manager,
        &mdirs[0],
        &row_dirs[0],
        std::time::Duration::from_secs(3),
        std::time::Duration::from_millis(25),
    )
    .await;
    // The storm's tail is the PRODUCT cadence's to cover (a test-side
    // cycle on its heels would race the due tick for the mutex and judge
    // the tail's leaves at whichever cycle won).
    tokio::time::sleep(std::time::Duration::from_millis(
        2 * mvol.appender_stats().unwrap().flush_ceiling_ms,
    ))
    .await;
    let f1 = cadence_faces(&mvol);
    eprintln!("F-B1 onset: row 1 — {created1} creates; the manager at {f1:?}");
    assert!(created1 >= 1_000, "row 1 stormed ({created1} creates)");
    assert_eq!(
        f1.overruns - f0.overruns,
        0,
        "the premise: with the horizon holding the warm-up's term, row 1 lands inside the \
         ceiling"
    );
    // The ceiling's two-tick MARGIN: a decision later than it lands leaves
    // past the ceiling whatever the trigger anticipated — the venue's
    // tick term, what voids a sample (Issue 19).
    let ceiling_ms = mvol.appender_stats().unwrap().flush_ceiling_ms;
    let late_bound_ms = ceiling_ms - CHECKPOINT_MAX_AGE_MS as u64;
    let mut prev_term_ms = f1.term_ms;
    let mut void_draws = 0u32;
    let mut verdict: Option<u32> = None;
    for draw in 0..DRAWS {
        // The QUIET horizon: more cycles than the window holds, each with
        // nothing to flush — the row's term leaves the horizon.
        for _ in 0..TERM_HORIZON_CYCLES + 8 {
            mvol.checkpoint_now().await.expect("a quiet cycle");
        }
        let fq = cadence_faces(&mvol);
        eprintln!(
            "F-B1 onset: draw {draw} — after {} quiet cycles the manager at {fq:?}",
            TERM_HORIZON_CYCLES + 8
        );
        // The premise: the horizon FORGOT the storm's term (a quiet cycle's
        // few writes are what it remembers), and nothing of the storm is
        // pending — at most a straggler leaf or two (the kernel's times
        // echo drains behind the storm), never the row's 64.
        assert!(
            fq.term_ms * 4 < prev_term_ms,
            "the premise: the quiet horizon FORGOT the previous row's term ({} → {} ms)",
            prev_term_ms,
            fq.term_ms
        );
        assert!(
            fq.projected_ms <= 2 * fq.node_unit_us.div_ceil(1_000),
            "the premise: nothing of the storm is pending at the onset ({fq:?})"
        );
        assert_eq!(
            fq.trigger_ms + fq.term_ms.max(fq.projected_ms),
            CHECKPOINT_MAX_AGE_MS as u64,
            "the trigger is the max age less the term in force"
        );
        // The ONSET under the product cadence, into a fresh set of
        // directories — the onset cycle and two more. Bounded short of the
        // shape's own cliff: sixty-four leaves filled in LOCKSTEP reach
        // their log-full compaction in the same cycle, an SMO storm the
        // promise ledger never named (a log-full compaction is the flush
        // pass's decision, not a commit's promise) and the horizon prices
        // only from its second occurrence — the steady-state class the
        // box's uneven directories never take at once, stated here, not
        // the class under test.
        let row = 2 + draw;
        let created2 = scale_row(
            row,
            &manager,
            &mdirs[row as usize - 1],
            &row_dirs[row as usize - 1],
            std::time::Duration::from_millis(2_500),
            std::time::Duration::from_millis(25),
        )
        .await;
        let f2 = cadence_faces(&mvol);
        let late_ms = mvol.checkpoint_late_max_ms();
        eprintln!(
            "F-B1 onset: draw {draw} (row {row}) — {created2} creates over 2.5 s; the manager \
             at {f2:?} ({} cycles; the decision at most {late_ms} ms late, bound {late_bound_ms})",
            f2.checkpoints - fq.checkpoints
        );
        assert!(created2 >= 1_000, "the onset stormed ({created2} creates)");
        prev_term_ms = f2.term_ms;
        if f2.overruns == fq.overruns {
            verdict = Some(draw);
            // The storm's TAIL, waited out UNDER the parked device with the
            // joiners still live (review round 3, Issue 22): the product
            // cadence covers the row's last cycles at the row-1 discipline
            // (two ceilings), then one test-side cycle leaves nothing
            // dirty for the closing belt. The tail is read for
            // attribution and STATED — the pin's law is the ONSET (the
            // window `fq → f2` above); a tail cycle's overrun here is the
            // steady-state class on a venue whose per-node cost moves
            // within one run (the reviewer's tape: a 457 ms pass against a
            // 342 ms projection over 64 leaves, the decision 22 ms late),
            // never the onset's verdict and never a silent pass.
            tokio::time::sleep(std::time::Duration::from_millis(2 * ceiling_ms)).await;
            mvol.checkpoint_now().await.expect("the tail's cover cycle");
            let ft = cadence_faces(&mvol);
            let tail_late_ms = mvol.checkpoint_late_max_ms();
            if ft.overruns > f2.overruns {
                eprintln!(
                    "F-B1 onset: TAIL after draw {draw}'s verdict — {} overrun(s) in the row's \
                     tail cycles (the manager at {ft:?}; the decision at most {tail_late_ms} ms \
                     late, bound {late_bound_ms}): the steady-state class after the onset law \
                     was judged — {}",
                    ft.overruns - f2.overruns,
                    if tail_late_ms > late_bound_ms {
                        "the venue's tick lateness"
                    } else {
                        "an under-projection of the tail's pass on this venue (its per-node cost \
                         moves within a run); the onset verdict above stands"
                    }
                );
            } else {
                eprintln!("F-B1 onset: TAIL after draw {draw} clean (the manager at {ft:?})");
            }
            break;
        }
        if late_ms > late_bound_ms {
            // The venue's term: the tick itself landed past the margin the
            // ceiling gives it — no trigger lands such a cycle inside the
            // ceiling. The sample is VOID, stated, and re-drawn.
            void_draws += 1;
            eprintln!(
                "F-B1 onset: draw {draw} VOID — {} overrun(s) beside an age decision {late_ms} \
                 ms late (the ceiling's margin is {late_bound_ms} ms): the executor's tick under \
                 CPU saturation, the venue's term; re-drawn",
                f2.overruns - fq.overruns
            );
            // The storm's tail is the product cadence's to cover before the
            // next quiet horizon.
            tokio::time::sleep(std::time::Duration::from_millis(2 * ceiling_ms)).await;
            continue;
        }
        squeezefs::uring_fs::disarm_device_latency(&path);
        panic!(
            "the manager's leaves land inside the {ceiling_ms} ms ceiling through a storm's \
             onset after a quiet horizon — the cadence priced the first storm cycle off its \
             pending work (anticipated {} ms before the onset; {} overrun(s) with the decision \
             at most {late_ms} ms late, inside the {late_bound_ms} ms margin: the cadence's, \
             not the venue's; RED on the horizon term alone: the box's 1,127 / 1,125 ms with \
             11 / 4 ms anticipated)",
            fq.term_ms,
            f2.overruns - fq.overruns
        );
    }
    squeezefs::uring_fs::disarm_device_latency(&path);
    match verdict {
        Some(draw) => eprintln!(
            "F-B1 onset: GREEN at draw {draw} — {void_draws} of {} draw(s) voided by the venue's \
             tick lateness",
            draw + 1
        ),
        None => panic!(
            "the venue produced no valid onset sample in {DRAWS} draws — every one overran with \
             the age decision past the {late_bound_ms} ms margin (the dev profile's tick under \
             CPU saturation); re-run on a quiet box"
        ),
    }
    for (j, _) in &daemons {
        assert_supply_gauges_zero(&j.volumes[0], "joiner");
        shutdown(j).await;
    }
    // The ceiling word was judged over the draw window with its
    // attribution above; the tail is stated there (Issue 22).
    assert_must_stay_zero_with(&mvol, "manager", false);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13h — the deferred-flush barrier between the age decision and its
// cycle is a real gap the landing term must carry (the fourth box pass,
// the record's §3.9.6.1 / §4.4an).
// ---------------------------------------------------------------------------

/// **The covering barrier a due tick runs BETWEEN its age decision and
/// its cycle was priced by nothing.** In the code that barrier is the
/// tick's step 2 — the deferred-mode flush barrier (`needs_flush`, set by
/// every non-strict commit group) — which PR 13g's re-order put AFTER the
/// decision (the decision is read before the threshold drain now) and
/// which the cycle's clock (`cycle_started`, taken after it) never saw:
/// `late` was measured before it, the term after it. A leaf dirtied right
/// after the previous collection ages `trigger + late + B_deferred +
/// cycle` while the horizon anticipated `cycle + (late − tick)⁺` — one
/// barrier's wall is the gap. On the box it is SMALL (the manager's
/// `meta_barrier` mean 1.3–1.9 ms, device 15 µs): the fourth pass's trips
/// (1,127 / 1,125 ms at a storm's END) were the LIVE projection
/// under-pricing a lockstep wave of promised compactions (the record's
/// corrected attribution — the wave pin below), and this gap is the
/// second, smaller term the same re-read named; the record's first
/// "≈ 40 ms barrier" reading was a pre-trip snapshot, RETRACTED.
///
/// The law: the TERM is measured at the LANDING from the DECISION — the
/// same instant `note_flush_ceiling` judges, from the instant the tick
/// fired — so every barrier between the two is in the horizon; and the
/// live projection prices the next cycle's COVERING BARRIERS from the
/// measured barrier unit (`meta_kv_checkpoint_barrier_ms`, the horizon
/// maximum of one barrier's wall) — two on a deferred-mode volume (the
/// due tick's, then barrier #1), one strict — so a device whose barrier is
/// slow at REST is priced before its first storm cycle. The ceiling never
/// widens; a flat volume's age verdict is unchanged (`flat_age_due`).
///
/// The shape, deterministic under `uring_fs::arm_device_latency` on the
/// BARRIER alone (150 ms per `fdatasync`, writes unparked): the manager
/// creates ONE file the instant a cycle COLLECTS (the aged leaf, dirtied
/// while that cycle's barriers run; its commit sets `needs_flush`, which
/// the next quiet tick consumes) and ONE more 8 ms before the trigger
/// fires (sets `needs_flush` for the DUE tick), so the due tick runs the
/// deferred barrier, then its cycle with barrier #1: the leaf's age is
/// `trigger + late + 2 × 150 + s`. RED on `230e95dd`: the horizon holds
/// `150 + s` (a cycle's wall), the trigger is `≈ 850` and the leaf lands
/// `≈ 1,150 + late` old — a trip by ≥ 150 ms on every cycle, whatever the
/// tick's lateness. GREEN: the barrier unit read 150 ms at the warm-up,
/// the projection prices `2 × 150 + s`, the trigger ≈ 700 and the leaf
/// lands `≈ 1,000 + late` old — 0 trips. The draws PIPELINE: a draw's
/// landing cycle is the next draw's reference collection (its verdict is
/// read once its ledger record lands, after the next draw's aged create
/// went out).
///
/// **The venue's term, attributed INSIDE the pin** (PR 13g's law): a cycle
/// whose age decision was later than the ceiling's two-tick margin lands
/// its leaves past the ceiling whatever the trigger anticipated — VOID,
/// stated, re-drawn; and a draw whose deferred barrier did not land on
/// the DUE tick (the late create's tick fell just before the trigger and
/// barriered a non-due tick — the decision then a barrier late, past the
/// margin; or no deferred barrier landed at all — `meta_flush_deferred`
/// unmoved) did not form the shape — VOID too. Bounded at `DRAWS` valid
/// samples in `DRAWS + 4` draws; a run that draws none fails loud naming
/// the venue.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_deferred_flush_barrier_between_the_decision_and_its_cycle_is_priced_into_the_term() {
    use squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS;
    use std::sync::atomic::Ordering::Relaxed;
    /// The parked barrier: one `fdatasync` of the volume's file.
    const BARRIER_MS: u64 = 150;
    /// Valid samples the verdict needs, and the draws it may take.
    const DRAWS: u32 = 3;
    const MAX_DRAWS: u32 = DRAWS + 4;
    let dir = cadence_venue_dir();
    let _g = SEAM.lock().await;
    reset_process_state();
    // The box's 32 MiB fixed ring: the cycles are the AGE law's, never
    // ring pressure's.
    let uris = format_stamped_set_with_ring_len(dir.path(), 1, VOL_LEN * 8, 32 * 1024 * 1024).await;
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        shutdown(&routed).await;
    }
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let venue = HoldersVenue::stand_up(&manager, &[]).await;
    // One idle joiner: the N-daemon shape (the manager serves a wire
    // appender beside its own cadence).
    let joiner = join(&uris, &venue, &mvol, 1).await;
    // The manager's directory (its rotor slot — one leaf of its own).
    let d = manager
        .create(1, "landing", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the manager's directory")
        .ino;
    let path = std::path::PathBuf::from(&uris[0]);
    squeezefs::uring_fs::arm_device_latency(
        &path,
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(BARRIER_MS),
    );
    // The warm-up: two cycles under the parked barrier — the horizon holds
    // a cycle's wall, the barrier unit is measured.
    for _ in 0..2 {
        mvol.checkpoint_now().await.expect("a warm-up cycle");
    }
    let f0 = cadence_faces(&mvol);
    let barrier0 = mvol.checkpoint_barrier_ms();
    eprintln!("F-B1 landing: warmed — the manager at {f0:?}, barrier unit {barrier0} ms");
    assert!(
        barrier0 >= BARRIER_MS,
        "the premise: the warm-up measured the parked barrier ({barrier0} ms)"
    );
    let ceiling_ms = mvol.appender_stats().unwrap().flush_ceiling_ms;
    let late_bound_ms = ceiling_ms - CHECKPOINT_MAX_AGE_MS as u64;
    let deferred_face = || {
        squeezefs::fuse_client::METRICS
            .meta_flush_deferred
            .load(Relaxed)
    };
    /// A finer poll than `wait_until` (1 ms): the aged create must land
    /// while the reference cycle's barriers still run.
    async fn poll_until(what: &str, mut cond: impl FnMut() -> bool) {
        let started = std::time::Instant::now();
        while !cond() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    // Draw 0's reference cycle, started in the background so the aged
    // create can land right after its collection.
    let seed = {
        let v = Arc::clone(&mvol);
        tokio::spawn(async move { v.checkpoint_now().await.expect("the reference cycle") })
    };
    let mut collected_prev = mvol.checkpoint_collected_ns();
    let mut seq_prev = mvol.checkpoint_seq();
    let mut overruns_prev = mvol.appender_stats().unwrap().flush_ceiling_overruns;
    // The draw whose landing cycle is the NEXT collection: `(draw, the
    // deferred count when its late create went out)`.
    let mut in_flight: Option<(u32, u64)> = None;
    let mut valid = 0u32;
    let mut trips = 0u64;
    let mut void_draws = 0u32;
    for draw in 0..=MAX_DRAWS {
        // The reference collection: the previous draw's landing cycle
        // (draw 0's: the seeded cycle).
        poll_until("a cycle's collection", || {
            mvol.checkpoint_collected_ns() != collected_prev
        })
        .await;
        let c0 = mvol.checkpoint_collected_ns();
        collected_prev = c0;
        let aged_at = squeezefs::mono_core::monotonic_ns_u64();
        if draw < MAX_DRAWS && valid < DRAWS {
            // The aged leaf, dirtied while the reference cycle's barriers
            // run.
            manager
                .create(
                    d,
                    &format!("aged-{draw}"),
                    libc::S_IFREG | 0o644,
                    1000,
                    1000,
                )
                .await
                .expect("the aged create");
        }
        // The reference cycle LANDS (its audit, its term fold, its ledger
        // record): the previous draw's verdict.
        poll_until("the cycle's ledger record", || {
            mvol.checkpoint_seq() > seq_prev
        })
        .await;
        seq_prev = mvol.checkpoint_seq();
        let overruns_now = mvol.appender_stats().unwrap().flush_ceiling_overruns;
        if let Some((judged, deferred_at_late)) = in_flight.take() {
            let f1 = cadence_faces(&mvol);
            let late_ms = mvol.checkpoint_last_late_ms();
            let deferred_since_late = deferred_face() - deferred_at_late;
            let tripped = overruns_now - overruns_prev;
            eprintln!(
                "F-B1 landing: draw {judged} — the cycle landed (the manager at {f1:?}; the \
                 decision {late_ms} ms late, bound {late_bound_ms}; {deferred_since_late} \
                 deferred barrier(s) since the late create; barrier unit {} ms; {tripped} \
                 overrun(s))",
                mvol.checkpoint_barrier_ms()
            );
            if deferred_since_late == 0 || late_ms > late_bound_ms {
                void_draws += 1;
                eprintln!(
                    "F-B1 landing: draw {judged} VOID — {} (the venue's tick / the shape's \
                     timing, never the cadence's pricing); re-drawn",
                    if deferred_since_late == 0 {
                        "no deferred barrier landed on the due tick"
                    } else {
                        "the age decision was later than the ceiling's margin"
                    }
                );
            } else {
                valid += 1;
                trips += tripped;
            }
        }
        overruns_prev = overruns_now;
        if draw == MAX_DRAWS || valid == DRAWS {
            break;
        }
        // The trigger the due tick will read (the reference cycle's term
        // folded).
        let trigger_ms = mvol.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64);
        let fk = cadence_faces(&mvol);
        eprintln!(
            "F-B1 landing: draw {draw} — the aged create {} ms after the collection; trigger \
             {trigger_ms} ms (the manager at {fk:?}, barrier unit {} ms)",
            aged_at.saturating_sub(c0) / 1_000_000,
            mvol.checkpoint_barrier_ms()
        );
        // The late create 8 ms before the trigger fires: its commit sets
        // `needs_flush` for the DUE tick.
        let fire_at_ns = c0 + trigger_ms.saturating_sub(8) * 1_000_000;
        let now_ns = squeezefs::mono_core::monotonic_ns_u64();
        if fire_at_ns > now_ns {
            tokio::time::sleep(std::time::Duration::from_nanos(fire_at_ns - now_ns)).await;
        }
        let deferred_at_late = deferred_face();
        manager
            .create(
                d,
                &format!("late-{draw}"),
                libc::S_IFREG | 0o644,
                1000,
                1000,
            )
            .await
            .expect("the late create");
        in_flight = Some((draw, deferred_at_late));
    }
    seed.await.expect("the seeded cycle");
    squeezefs::uring_fs::disarm_device_latency(&path);
    assert_eq!(
        valid, DRAWS,
        "the venue produced {valid} valid landing samples in {MAX_DRAWS} draws ({void_draws} \
         void) — re-run on a quiet box"
    );
    eprintln!(
        "F-B1 landing: {trips} overrun(s) over {valid} valid cycles ({void_draws} void draws); \
         the manager at {:?}, barrier unit {} ms",
        cadence_faces(&mvol),
        mvol.checkpoint_barrier_ms()
    );
    assert_eq!(
        trips, 0,
        "the deferred-barrier gap: a due tick's deferred-flush barrier ({BARRIER_MS} ms here) \
         between its decision and its cycle must be inside the term the cadence anticipates \
         — a real gap the term clocked from the cycle's start left priced nowhere (≈ 2 ms on \
         the box); RED on the horizon term alone (the trigger ≈ 1000 − ({BARRIER_MS} + s), \
         the leaf lands ≈ 1,150 + late old)"
    );
    // The tail: the product cadence covers whatever the last draw left.
    tokio::time::sleep(std::time::Duration::from_millis(2 * ceiling_ms)).await;
    shutdown(&joiner).await;
    drop(joiner);
    assert_must_stay_zero_with(&mvol, "manager", false);
    venue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **The per-cycle tape** (PR 13h — the box-pass review's Issue 1 named the
/// instrument): `meta_kv_checkpoint_last_cycle` says what the LAST cycle
/// decided with and paid, so a trip attributes itself from the WARN line
/// that rides it. The fourth box pass's one trip had no such words: the
/// record read a snapshot taken on the wrong side of the trip and filled
/// the gap with a barrier no face measured. Pinned: a `checkpoint_now`
/// cycle's tape carries NO decision words (no age decision fired it) and
/// its paid walls sum to its landing; a CADENCE cycle's tape carries the
/// decision's words (the trigger it fired at, the dirty count it priced,
/// the projection) beside the same sum law, and the collection found what
/// the decision counted. Each wall is a floor in ms, so the sum bounds
/// the landing from below by construction and from above by the five
/// roundings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_cycles_tape_names_the_decisions_words_and_what_the_cycle_paid() {
    let dir = cadence_venue_dir();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_ring_len(dir.path(), 1, VOL_LEN * 8, 32 * 1024 * 1024).await;
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let d = manager
        .create(1, "tape", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("the directory")
        .ino;
    let sum_law = |t: &squeezefs::meta_backend::kv::checkpoint::CycleTape| {
        let parts = t.pre_start_ms + t.publish_ms + t.flush_ms + t.pages_ms + t.barrier_ms;
        assert!(
            (parts..=parts + 5).contains(&t.landing_ms),
            "the paid walls sum to the landing (parts {parts}, landing {}): {t}",
            t.landing_ms
        );
        assert_eq!(
            t.dirty_collected,
            t.nodes_appended + t.smo_nodes,
            "every node the collection found was appended or SMO'd: {t}"
        );
    };
    // A `checkpoint_now` cycle: no age decision fired it.
    for i in 0..3 {
        manager
            .create(d, &format!("now-{i}"), libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("a create");
    }
    mvol.checkpoint_now().await.expect("the explicit cycle");
    let now_tape = mvol.checkpoint_last_cycle();
    eprintln!("tape (checkpoint_now): {now_tape}");
    assert_eq!(
        now_tape.seq,
        mvol.checkpoint_seq(),
        "the tape names the cycle's seq"
    );
    assert!(
        now_tape.dirty_collected >= 1,
        "the cycle flushed the creates: {now_tape}"
    );
    assert_eq!(
        (
            now_tape.trigger_ms,
            now_tape.dirty_at_decision,
            now_tape.projected_ms,
            now_tape.late_ms
        ),
        (0, 0, 0, 0),
        "an explicit cycle carries no decision words: {now_tape}"
    );
    sum_law(&now_tape);
    // A CADENCE cycle: the age decision's words ride the tape.
    let seq0 = mvol.checkpoint_seq();
    manager
        .create(d, "cadence", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("a create");
    wait_until("the cadence's own cycle", || mvol.checkpoint_seq() > seq0).await;
    let tape = mvol.checkpoint_last_cycle();
    eprintln!("tape (cadence): {tape}");
    assert_eq!(tape.seq, mvol.checkpoint_seq());
    assert!(
        tape.trigger_ms >= 1,
        "the age decision's trigger rides the tape: {tape}"
    );
    assert!(
        tape.dirty_at_decision >= 1 && tape.dirty_collected >= tape.dirty_at_decision,
        "the decision counted the dirty leaf and the collection found it: {tape}"
    );
    assert!(
        tape.node_unit_us >= 1 && tape.projected_ms <= tape.trigger_ms.max(1) * 1000,
        "the projection's inputs ride the tape: {tape}"
    );
    sum_law(&tape);
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **F-B1's box class, read off the fourth pass's own snapshots (record
/// §3.9.6.1 re-read; the review's Issue 1): a WAVE of promised SMO images
/// priced at a per-image unit the small passes before it UNDER-MEASURED.**
/// `m0_pc41.json` (the create's end, taken < 1 s before the trip) reads
/// volume 1 at `heap_promised` **38**, image unit **2.04 ms**, projection
/// 84 (≈ 38 × 2.04 + 65 dirty × 0.093); `m0_pn41.json` (after the trip)
/// reads the image unit **3.97 ms**, `node_compactions` +45 and the term
/// **151** — and 38 × 3.97 = 151, the trip cycle's own wall: the wave cost
/// what the trip pass then measured, twice what the projection had
/// multiplied by. The under-measurement is the unit's GRAIN FLOOR
/// (`flush_unit_ns` = `wall / max(count, 4)`, review round 1 Issue 10b's
/// noise bound): a pass of two images reads HALF its per-image cost, one
/// image a QUARTER — and under a create storm the passes that measure the
/// image unit are the threshold DRAIN's, one or two compactions per tick,
/// while the storm's 64 rotor leaves fill in LOCKSTEP (round-robin mints,
/// equal bytes) and cross the node size within one interval: a wave the
/// cycle inherits at 38 — priced at the halved unit. A bound must bound:
/// the unit the horizon-max law multiplies by is the pass's mean per item,
/// `wall / count`; a hiccup on a small pass then OVER-prices the horizon
/// (earlier cycles — the shipped saturation posture's cost) where the
/// floor UNDER-priced the wave (the ceiling — the promise). The ceiling
/// never widens; the flat path is untouched.
///
/// The shape in process, deterministic under `uring_fs::arm_device_latency`
/// on the BARRIER (every SMO barriers its successor image — §4.10 — so an
/// image costs `B_MS` where an append costs a write; the box's 43× ratio
/// between the two units): `LEAVES` files in `LEAVES` rotor slots, each
/// leaf's log filled in LOCKSTEP by rounds of one sub-threshold same-key
/// xattr put (below the drain's 4 KiB enqueue — the drain never visits;
/// the cycle owns the compaction) + one explicit cycle, until the round
/// whose puts PROMISE every leaf's compaction at once — the wave, staged
/// for the CADENCE's next cycle. Before it a PILOT leaf, filled and
/// compacted alone, is the small pass that measures the image unit: one
/// image, `B_MS` of wall. RED on `bcf4ede5`: the pilot's pass reads a
/// QUARTER unit, the projection prices the wave at it, the trigger sits
/// ≈ 300 ms too late and the oldest wave leaf lands ≈ 1,250 ms old. GREEN:
/// the pilot's pass reads the exact unit, the trigger anticipates the
/// wave, the leaf lands well inside the ceiling. The judged cycle is the
/// first wave cycle (its term then enters the horizon and every later
/// cycle is anticipated from it — the box's class is the first wave after
/// the horizon forgot); a void draw (the decision later than the ceiling's
/// margin) re-opens the manager for a fresh horizon.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_wave_of_promised_images_is_priced_at_the_per_image_cost_the_passes_measured() {
    use squeezefs::meta_backend::kv::checkpoint::CHECKPOINT_MAX_AGE_MS;
    /// The parked barrier: one `fdatasync` of the volume's file.
    const B_MS: u64 = 30;
    /// The wave's leaves — one rotor slot each.
    const LEAVES: usize = 12;
    /// One fill put: below the drain's 4 KiB enqueue threshold.
    const PUT: usize = 3 * 1024;
    /// Fill rounds before the wave forms are a fixture failure.
    const ROUNDS_MAX: u32 = 200;
    const MAX_ATTEMPTS: u32 = 3;
    /// The pass's measured wall per image, µs (0 for a pass with none).
    fn per_image_us(t: &squeezefs::meta_backend::kv::checkpoint::CycleTape) -> u64 {
        (t.image_ms * 1000).checked_div(t.images).unwrap_or(0)
    }
    let dir = cadence_venue_dir();
    let _g = SEAM.lock().await;
    reset_process_state();
    let uris = format_stamped_set_with_ring_len(dir.path(), 1, VOL_LEN * 8, 32 * 1024 * 1024).await;
    let path = std::path::PathBuf::from(&uris[0]);
    let mut verdict: Option<(u64, squeezefs::meta_backend::kv::checkpoint::CycleTape)> = None;
    for attempt in 0..MAX_ATTEMPTS {
        let manager = open_under(&uris, &Knobs::armed()).await;
        let mvol = Arc::clone(&manager.volumes[0]);
        // A file in its own rotor slot: the leaf `stat + layout + xattrs`
        // share (its directory's slot by affinity).
        let mint = |name: String| {
            let m = Arc::clone(&manager);
            async move {
                let d = m
                    .create(1, &name, libc::S_IFDIR | 0o755, 1000, 1000)
                    .await
                    .expect("a directory")
                    .ino;
                m.create(d, "f", libc::S_IFREG | 0o644, 1000, 1000)
                    .await
                    .expect("a file")
                    .ino
            }
        };
        let pilot = mint(format!("pilot-{attempt}")).await;
        let mut leaves = Vec::with_capacity(LEAVES);
        for i in 0..LEAVES {
            leaves.push(mint(format!("wave-{attempt}-{i}")).await);
        }
        mvol.checkpoint_now().await.expect("the mints covered");
        squeezefs::uring_fs::arm_device_latency(
            &path,
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(B_MS),
        );
        // Fill `files` in lockstep: one sub-threshold same-key put per
        // leaf per round, an explicit cycle appending them, until the
        // round whose puts promise every leaf's compaction.
        let fill = |files: Vec<u64>| {
            let m = Arc::clone(&manager);
            let v = Arc::clone(&mvol);
            async move {
                let put = vec![0x5au8; PUT];
                for round in 0..ROUNDS_MAX {
                    let promised0 = v.heap_promised();
                    for &f in &files {
                        m.setxattr(f, "user.fill", &put).await.expect("a fill put");
                    }
                    let promised = v.heap_promised() - promised0;
                    if promised > 0 {
                        assert_eq!(
                            promised,
                            files.len() as u64,
                            "the fill is lockstep: every leaf crosses the node size in the same \
                             round (round {round})"
                        );
                        return round;
                    }
                    v.checkpoint_now().await.expect("a fill cycle");
                }
                panic!(
                    "no promise after {ROUNDS_MAX} fill rounds — the fixture's leaf never filled"
                );
            }
        };
        // The pilot: the small pass that measures the image unit.
        let pilot_rounds = fill(vec![pilot]).await;
        mvol.checkpoint_now().await.expect("the pilot's compaction");
        let pilot_tape = mvol.checkpoint_last_cycle();
        let unit_us = mvol.checkpoint_image_unit_ns() / 1_000;
        eprintln!(
            "F-B1 wave: attempt {attempt} — the pilot filled in {pilot_rounds} rounds; its pass: \
             {pilot_tape}; image unit in force {unit_us} µs against the pass's {} µs per image",
            per_image_us(&pilot_tape)
        );
        assert!(
            pilot_tape.images >= 1,
            "the premise: the pilot's cycle compacted its leaf ({pilot_tape})"
        );
        // The wave: staged for the cadence's next cycle (the fill's own
        // cycles precede the staging round; the seq is read after it).
        let ceiling_ms = mvol.appender_stats().unwrap().flush_ceiling_ms;
        let late_bound_ms = ceiling_ms - CHECKPOINT_MAX_AGE_MS as u64;
        let wave_rounds = fill(leaves.clone()).await;
        let staged_at = squeezefs::mono_core::monotonic_ns_u64();
        let seq0 = mvol.checkpoint_seq();
        let overruns0 = mvol.appender_stats().unwrap().flush_ceiling_overruns;
        let trigger_ms = mvol.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64);
        eprintln!(
            "F-B1 wave: attempt {attempt} — {LEAVES} leaves promised after {wave_rounds} rounds \
             ({} ms after the last collection); trigger {trigger_ms} ms, projected {} ms, \
             anticipated {} ms",
            staged_at.saturating_sub(mvol.checkpoint_collected_ns()) / 1_000_000,
            mvol.checkpoint_projected_ms(),
            mvol.checkpoint_term_ms()
        );
        wait_until("the cadence's wave cycle", || mvol.checkpoint_seq() > seq0).await;
        let tape = mvol.checkpoint_last_cycle();
        let overruns = mvol.appender_stats().unwrap().flush_ceiling_overruns - overruns0;
        eprintln!(
            "F-B1 wave: attempt {attempt} — the wave cycle's tape: {tape}; {overruns} overrun(s); \
             the pass's per-image wall {} µs against the unit it was priced at {} µs",
            per_image_us(&tape),
            tape.image_unit_us
        );
        squeezefs::uring_fs::disarm_device_latency(&path);
        assert!(
            tape.promised_at_decision >= LEAVES as u64 && tape.images >= LEAVES as u64,
            "the premise: the cadence's cycle found the wave it was decided over ({tape})"
        );
        let valid = tape.late_ms <= late_bound_ms;
        if !valid {
            eprintln!(
                "F-B1 wave: attempt {attempt} VOID — the age decision was later than the \
                 ceiling's margin (the venue's tick, never the cadence's pricing); the manager is \
                 re-opened for a fresh horizon"
            );
        }
        if valid {
            // The UPPER cycle bound AFTER the wave (review round 1, Issue
            // 4 — the floor deletion's other direction): the wave's term
            // and the `B_MS` image unit now sit in the horizon, so a paced
            // trickle over the next window must run at the trigger those
            // words derive — never a cycle per tick — and the unit in
            // force reads the pass's per-image cost, not an outlier. The
            // trickle's own commits are the promise-free class (a fresh
            // xattr on the pilot below the drain), so the projection is
            // the dirty nodes' alone.
            let window_ms = 1_500u64;
            let seq1 = mvol.checkpoint_seq();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let trickle = {
                let m = Arc::clone(&manager);
                let stop = Arc::clone(&stop);
                tokio::spawn(async move {
                    let mut i = 0u32;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        m.setxattr(pilot, &format!("user.t{i}"), b"1")
                            .await
                            .expect("a trickle put");
                        i += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    i
                })
            };
            tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let puts = trickle.await.unwrap();
            let cycles = mvol.checkpoint_seq() - seq1;
            let (term, projected) = (mvol.checkpoint_term_ms(), mvol.checkpoint_projected_ms());
            let bound = max_cadence_cycles(window_ms, term, projected);
            eprintln!(
                "F-B1 wave: attempt {attempt} — after the wave {cycles} cycles over {window_ms} \
                 ms ({puts} puts), bound {bound} (term {term} ms, projected {projected} ms, \
                 trigger {} ms, image unit {} µs)",
                mvol.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64),
                mvol.checkpoint_image_unit_ns() / 1_000
            );
            assert!(puts >= 10, "the trickle ran ({puts} puts)");
            assert!(
                cycles <= bound,
                "the cadence after the wave runs at its trigger, never a cycle per tick: \
                 {cycles} cycles over {window_ms} ms against a bound of {bound} (term {term} ms, \
                 projected {projected} ms) — an over-priced unit is bounded to the horizon, \
                 and here the horizon holds the wave's own words"
            );
            assert!(
                mvol.checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64) > 0,
                "the trigger stands after the wave (term {term} ms, projected {projected} ms)"
            );
        }
        assert_must_stay_zero_with(&mvol, "manager", false);
        shutdown(&manager).await;
        drop(mvol);
        drop(manager);
        if valid {
            verdict = Some((overruns, tape));
            break;
        }
    }
    let (trips, tape) = verdict
        .expect("the venue produced no valid wave cycle in 3 attempts — re-run on a quiet box");
    assert_eq!(
        trips,
        0,
        "F-B1's box class: a wave of {} promised images was priced at {} µs each and cost {} µs \
         each — the unit's grain floor read the pilot's one-image pass at a quarter of its \
         per-image cost (the box: 38 images at 2.04 ms projected, 3.97 ms paid, 151 ms = the \
         trip cycle's term); the tape: {tape}",
        tape.promised_at_decision,
        tape.image_unit_us,
        per_image_us(&tape)
    );
    fsck_clean(&uris).await;
}

// ---------------------------------------------------------------------------
// PR 13h — F-R6: a token client's FORGET-driven reclaim on a foreign slot
// (the fourth box pass, the record's §3.9.6.3).
// ---------------------------------------------------------------------------

fn reclaim_faces() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let m = &squeezefs::fuse_client::METRICS;
    (
        m.reclaim_destroy_refused_release_failed.load(Relaxed),
        m.reclaim_foreign_slot_forgets.load(Relaxed),
    )
}

/// **F-R6 (the fourth box pass, §3.9.6.3 — PR 12b's reclaim path × PR 5's
/// token planes): a joined writer's FORGET-driven reclaim of an ino in a
/// slot it does NOT lease reclaims NOTHING.** On the box m60 — a joiner
/// whose kernel had instantiated 40 k of the MANAGER's inodes as a token
/// client (the leg's acked-writes check read the manager's tree through
/// it) — met the manager's `rm -rf`: the unlink commits recalled its
/// tokens, its recall sink invalidated + pruned, the kernel FORGOT, and
/// its reclaim ADMITTED every foreign ino (a divert read of the corpse at
/// the holder answered `nlink 0`), PRICED its destroy off this daemon's
/// own stale PROJECTION of the manager's trees (`destroy_entry_bytes`'s
/// xattr walk — pointers into extents the holder had retired, freed and
/// re-granted: zeros, rule-4-screened frames, defect 18 / 34's 256-restart
/// loop on 60+ slots) and, where the pricing read anything, drove the
/// destroy into its own commit path — `destroy_inodes`' live-nlink skip
/// judged the corpse LIVE off the same stale projection, or the door
/// refused the foreign slot (`SlotBusy`): 6,782 / 7,266 `destroy WITHHELD`
/// WARNs per row set, `reclaim_destroy_refused_release_failed` +121 /
/// +742 — nothing destroyed (the withhold is the fail-safe), a CPU + log
/// storm on a path a token client must never take.
///
/// The law (design §5.1 — the slot is the ownership unit): a non-holder
/// never prices, releases or destroys a foreign slot's object; a FORGET of
/// one is a TOKEN CLIENT's forget — the attr cache goes (as every FORGET's
/// does), the token stays under the plane's own recall / eviction law, and
/// the reclaim accounts NOTHING here (`reclaim_foreign_slot_forgets`);
/// since review round 1 (Issue 1) the forget TRAVELS to the slot's holder
/// as a reclaim hint the holder runs as its own FORGET — this fixture
/// installs no hint sink at the manager, so the hint is counted misrouted
/// there and the holder's own FORGET path below is what destroys the
/// corpses (the hint law's own pins are the two after this one). The
/// predicate is the mount-time corpse sweep's (`inode_plane_owns_slot` —
/// leased here, or unleased on the manager), read at the reclaim's entry
/// BEFORE any layout, xattr or reference read; the witnesses that it was
/// (Issue 7) are READ-side — the holder's `grants_served` unmoved across
/// the batch (no divert getattr, no layout fetch) and
/// `meta_kv_projection_walk_exhaustions` unmoved — beside the apply belt
/// `meta_kv_leaf_lease_refusals`, which counts refused APPLIES and says
/// only that no priced destroy reached a commit.
///
/// Two sets of the manager's corpses, one reclaim batch at the joiner:
/// the FRESH set — unlinked and checkpointed BEFORE the joiner opened, so
/// its projection is exact and the base's pricing walk succeeds into the
/// door, which refuses the foreign slot: the deterministic RED
/// (`reclaim_destroy_refused_release_failed` +N, N `destroy WITHHELD`
/// WARNs — the box's second face); and the TOKEN set — created after the
/// join, read by the joiner through its divert (the manager's
/// `grants_served` moves), unlinked at the holder (the recall), standing
/// as `nlink 0` corpses there (this fixture has no kernel at the manager,
/// so no FORGET reaches its reclaim — the window between the box's `rm
/// -rf` and the manager's own FORGETs): the base's pricing walks the
/// STALE projection and `destroy_inodes`' live-nlink skip reads the corpse
/// as `nlink 1` there — nothing destroyed, nothing counted, the foreign
/// tree WALKED (the box's first face, where that walk met recycled
/// extents). GREEN: the refused gauge unmoved, `reclaim_foreign_slot_
/// forgets` +2N, every corpse standing at the holder with `nlink 0` until
/// the HOLDER's own reclaim destroys it; the joiner's OWN file's forget
/// reclaims as before (its slot), the manager's own forgets never count as
/// foreign.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_forget_of_a_foreign_slots_ino_prices_no_destroy_and_reclaims_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared"), (SLOT_B, "mine")]).await;
    let shared = dirs[0];
    let mine = dirs[1];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, "manager-custody-13h-forget").await;
    // The manager's files in a slot IT leases (its first touch of the
    // seeded directory takes it — the box's shape: the manager's rotor
    // slots, never the joiner's). The FRESH set is unlinked before the
    // join: `nlink 0` corpses the joiner's projection will name exactly.
    let fresh = create_files(&manager, shared, "fresh", 8).await;
    let token = create_files(&manager, shared, "tok", 8).await;
    assert!(
        matches!(
            tree0_state(&mvol, SLOT_A).await,
            Some(SlotState::Leased { appender_id: 0, .. })
        ),
        "the premise: the manager leases the files' slot"
    );
    for (name, _) in &fresh {
        manager
            .unlink(shared, name)
            .await
            .expect("the holder's unlink");
    }
    mvol.checkpoint_now().await.expect("checkpoint");

    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            &uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, 71).await),
                identity: joiner_identity(&mvol, 71).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    assert!(
        !jvol.inode_plane_owns_slot(joiner.route_ino(token[0].1).1),
        "the premise: the files' slot is FOREIGN to the joiner's reclaim"
    );
    // The JOINER is this process's reading writer: PR 9's custody arm over
    // ITS set (the mount path's `arm_mount_slot_custody`) — its divert
    // dials the manager's token plane.
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
    // The joiner reads every TOKEN-set file through its divert — a token
    // client of the manager's slot (the box: the acked-writes check
    // through m60).
    for (name, ino) in &token {
        let got = joiner
            .lookup(shared, name)
            .await
            .expect("the joiner's lookup");
        assert_eq!(got.ino, *ino);
        joiner.getattr(*ino).await.expect("the joiner's getattr");
    }
    assert!(
        mholder.stats().grants_served > served0,
        "the premise: the files came to the joiner as TOKENS ({} → {})",
        served0,
        mholder.stats().grants_served
    );

    // The FUSE layer in front of the JOINER: the FORGET path's reclaim.
    let jf = fs_in_front_of(&joiner, "vol-13h-forget-j").await;

    // **A close / forget of a colleague's LIVE file ships NO hint and asks
    // the manager NOTHING** (review round 3, Issue 17): the joiner's
    // STANDING TOKEN on each file reads `nlink 1` — an unlink at the
    // holder recalls the token before it commits, so a standing token
    // with `nlink ≥ 1` is proof the file is live and there is no corpse
    // to reclaim; the RELEASE handler's unlink-while-open probe fires on
    // every last close, and before the arm each such close of a foreign
    // file resolved the slot off the manager (`ResolveSlot`, ≈ 1 manager
    // verb per close over a 64-slot rotor) and shipped a hint the holder
    // answered with a local read. RED on `6a5b0ff1`: hint_inos +N and
    // the manager's `slot_resolve_rpcs` moving.
    let live: Vec<u64> = token.iter().map(|(_, ino)| *ino).collect();
    let l0 = reclaim_faces_all();
    let resolves = || {
        mvol.slot_leases()
            .map(|p| p.resolve_rpcs.load(std::sync::atomic::Ordering::Relaxed))
    };
    let resolves0 = resolves();
    jf.fs.reclaim_orphaned_batch(live.clone()).await;
    let l1 = reclaim_faces_all();
    assert_eq!(
        (l1.hints_shipped, l1.hint_inos, l1.hint_failures),
        (l0.hints_shipped, l0.hint_inos, l0.hint_failures),
        "a live file is no corpse: the forget ships no hint (RED: {} inos hinted)",
        l1.hint_inos - l0.hint_inos
    );
    assert_eq!(
        resolves(),
        resolves0,
        "the forget asked the manager for no slot's lessee (slot_resolve_rpcs unmoved)"
    );
    assert_eq!(
        l1.skipped_live - l0.skipped_live,
        live.len() as u64,
        "every live-file forget is counted on reclaim_hint_skipped_live"
    );
    assert_eq!(l1.refused, l0.refused, "nothing priced or withheld");
    for (name, ino) in &token {
        let got = joiner.lookup(shared, name).await.expect("still there");
        assert_eq!(got.ino, *ino, "the live file stands at the holder");
    }

    // The manager unlinks the TOKEN set: `nlink 0` corpses standing at the
    // holder; the unlink commits recall the joiner's tokens.
    let recalls0 = mholder.stats().recalls;
    for (name, _) in &token {
        manager
            .unlink(shared, name)
            .await
            .expect("the holder's unlink");
    }
    assert!(
        mholder.stats().recalls > recalls0,
        "the unlinks recalled the joiner's tokens"
    );
    let corpses: Vec<u64> = fresh.iter().chain(&token).map(|(_, ino)| *ino).collect();
    for ino in &corpses {
        assert_eq!(
            manager
                .getattr(*ino)
                .await
                .expect("the corpse stands")
                .nlink,
            0,
            "the premise: the unlinked record stands at the holder with nlink 0"
        );
    }

    // The joiner's kernel FORGETs every one → its reclaim path. The
    // read-side witnesses (review round 1, Issue 7): the manager's token
    // plane serves NO grant across the batch — the admission `getattr`
    // and the plan's layout read are divert round trips, so an unmoved
    // `grants_served` says neither ran — and no projection walk exhausted
    // its budget (the box's first face).
    let (refused0, foreign0) = reclaim_faces();
    let grants_before = mholder.stats().grants_served;
    let exhaustions0 = squeezefs::meta_backend::kv::META_KV_PROJECTION_WALK_EXHAUSTIONS
        .load(std::sync::atomic::Ordering::Relaxed);
    jf.fs.reclaim_orphaned_batch(corpses.clone()).await;
    let (refused1, foreign1) = reclaim_faces();
    assert_eq!(
        mholder.stats().grants_served,
        grants_before,
        "the joiner's forgets took NO grant at the holder: no divert getattr, no layout fetch \
         (the read-side witness — grants_served)"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_PROJECTION_WALK_EXHAUSTIONS
            .load(std::sync::atomic::Ordering::Relaxed),
        exhaustions0,
        "no projection walk ran to its budget (meta_kv_projection_walk_exhaustions)"
    );
    assert_eq!(
        refused1 - refused0,
        0,
        "F-R6: a joiner's FORGET of a FOREIGN slot's ino must never price or drive a destroy — \
         the box's 6,782 `destroy WITHHELD` WARNs per row set were this reclaim reading its \
         stale projection and then meeting its own door (SlotBusy) \
         (reclaim_destroy_refused_release_failed)"
    );
    assert_eq!(
        foreign1 - foreign0,
        corpses.len() as u64,
        "every foreign-slot forget is COUNTED and dropped (reclaim_foreign_slot_forgets)"
    );
    // Nothing destroyed by the joiner: the corpses stand at the holder.
    for ino in &corpses {
        assert_eq!(
            manager
                .getattr(*ino)
                .await
                .expect("the corpse still stands at the holder")
                .nlink,
            0
        );
    }
    // The APPLY belt (`meta_kv_leaf_lease_refusals` counts a commit's apply
    // refused at a leaf this mount does not lease — never a read): stays 0
    // because no priced destroy reached the joiner's commit path either.
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_LEAF_LEASE_REFUSALS
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the apply belt never fired: no foreign destroy reached a commit"
    );

    // The HOLDER's own reclaim is the law: its FORGET path destroys them.
    let mf = fs_in_front_of(&manager, "vol-13h-forget-m").await;
    mf.fs.reclaim_orphaned_batch(corpses.clone()).await;
    let (refused2, foreign2) = reclaim_faces();
    assert_eq!(refused2, refused1, "the holder's destroys commit");
    assert_eq!(
        foreign2, foreign1,
        "the holder's own slot never counts as foreign"
    );
    for ino in &corpses {
        assert!(
            manager.getattr(*ino).await.is_err(),
            "the holder destroyed its corpse"
        );
    }

    // The joiner's OWN file (its first touch of the second seeded
    // directory takes that slot over the wire) reclaims as before.
    let own = joiner
        .create(mine, "own", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's own file")
        .ino;
    assert!(
        jvol.inode_plane_owns_slot(joiner.route_ino(own).1),
        "the joiner leases its own file's slot"
    );
    joiner
        .unlink(mine, "own")
        .await
        .expect("the joiner's unlink");
    jf.fs.reclaim_orphaned_batch(vec![own]).await;
    let (refused3, foreign3) = reclaim_faces();
    assert_eq!(refused3, refused2, "an own-slot destroy commits");
    assert_eq!(foreign3, foreign2, "an own-slot forget is never foreign");
    assert!(
        joiner.getattr(own).await.is_err(),
        "the joiner destroyed its own corpse"
    );
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");

    squeezefs::data_grant::disarm_slot_custody().await;
    drop(jf);
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    drop(mf);
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(&uris).await;
}

/// **The unarmed law, byte-identical** (F-R6's other half): on a FLAT
/// volume and on an UNARMED forest volume (the knob off — the PR 1–3
/// forest, every slot the mount's) the reclaim's slot gate is one relaxed
/// load answering "mine" for every ino: a FORGET-driven reclaim destroys
/// every corpse exactly as shipped, `reclaim_foreign_slot_forgets` never
/// moves, nothing is withheld.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forget_on_an_unarmed_mount_reclaims_every_corpse_as_shipped() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    // A FLAT member carrying the format config (the offline fsck's input),
    // the seam cleared; an unarmed forest member beside it.
    let flat = {
        let p = dir.path().join("flat");
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        let plan = squeezefs::meta_backend::plan_meta_slot_set(1).expect("derived plan");
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        let opts = squeezefs::meta_backend::kv::builder::FormatV3Options {
            format_config_xattr: Some(format_config_for(dir.path())),
            ..set_opts()
        };
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            &p,
            VOL_LEN,
            &opts,
            plan.stamps[0].clone(),
        )
        .await
        .expect("format flat member");
        p.display().to_string()
    };
    let forest_dir = tempfile::tempdir().unwrap();
    let forest = format_stamped_set_with_config(forest_dir.path(), 1)
        .await
        .remove(0);
    for (uri, tag) in [(flat, "vol-13h-flat"), (forest, "vol-13h-forest-unarmed")] {
        let routed = open_under(std::slice::from_ref(&uri), &Knobs::unarmed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let files = create_files(&routed, 1, "c", 8).await;
        for (name, _) in &files {
            routed.unlink(1, name).await.expect("unlink");
        }
        let inos: Vec<u64> = files.iter().map(|(_, ino)| *ino).collect();
        for ino in &inos {
            assert!(
                routed.owns_inode_reclaim(*ino),
                "{tag}: an unarmed mount reclaims every ino"
            );
            assert_eq!(routed.getattr(*ino).await.expect("the corpse").nlink, 0);
        }
        let f = fs_in_front_of(&routed, tag).await;
        let f0 = reclaim_faces_all();
        let reader0 = squeezefs::fuse_client::METRICS
            .reclaim_reader_forgets
            .load(std::sync::atomic::Ordering::Relaxed);
        f.fs.reclaim_orphaned_batch(inos.clone()).await;
        let f1 = reclaim_faces_all();
        assert_eq!(f1.refused, f0.refused, "{tag}: every destroy commits");
        // The whole peer / reader family stays put (review round 2, Issue
        // 11): no forget reads as foreign or unleased, nothing ships, the
        // reader face is the reader posture's alone.
        assert_eq!(
            (
                f1.foreign,
                f1.unleased,
                f1.hints_shipped,
                f1.hint_inos,
                f1.hint_failures
            ),
            (
                f0.foreign,
                f0.unleased,
                f0.hints_shipped,
                f0.hint_inos,
                f0.hint_failures
            ),
            "{tag}: an unarmed mount's forgets are all its own — nothing counted foreign or \
             unleased, no hint shipped; faces {f0:?} → {f1:?}"
        );
        assert_eq!(
            squeezefs::fuse_client::METRICS
                .reclaim_reader_forgets
                .load(std::sync::atomic::Ordering::Relaxed),
            reader0,
            "{tag}: a write mount is no reader"
        );
        for ino in &inos {
            assert!(
                routed.getattr(*ino).await.is_err(),
                "{tag}: the corpse is destroyed by the FORGET path, as shipped"
            );
        }
        drop(f);
        shutdown(&routed).await;
        drop(vol);
        drop(routed);
        fsck_clean(std::slice::from_ref(&uri)).await;
    }
}

// ---------------------------------------------------------------------------
// PR 13h review round 1, Issue 1 — the corpse classes F-R6's gate left with
// no live reclaimer: the reclaim HINT to the slot's reclaimer.
// ---------------------------------------------------------------------------

/// The reclaim family's faces, in one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReclaimFaces {
    refused: u64,
    foreign: u64,
    unleased: u64,
    hints_shipped: u64,
    hint_inos: u64,
    hint_failures: u64,
    served: u64,
    forwarded: u64,
    misrouted: u64,
    skipped_live: u64,
}

fn reclaim_faces_all() -> ReclaimFaces {
    use std::sync::atomic::Ordering::Relaxed;
    let m = &squeezefs::fuse_client::METRICS;
    ReclaimFaces {
        refused: m.reclaim_destroy_refused_release_failed.load(Relaxed),
        foreign: m.reclaim_foreign_slot_forgets.load(Relaxed),
        unleased: m.reclaim_unleased_slot_forgets.load(Relaxed),
        hints_shipped: m.reclaim_hints_shipped.load(Relaxed),
        hint_inos: m.reclaim_hint_inos_shipped.load(Relaxed),
        hint_failures: m.reclaim_hint_failures.load(Relaxed),
        served: m.reclaim_hints_served.load(Relaxed),
        forwarded: m.reclaim_hints_forwarded.load(Relaxed),
        misrouted: m.reclaim_hints_misrouted.load(Relaxed),
        skipped_live: m.reclaim_hint_skipped_live.load(Relaxed),
    }
}

/// Wait for `ino`'s record to be GONE at `at` (the reclaimer's destroy
/// landed), bounded — the reclaim pool's cadence is a 20 ms batch window.
async fn wait_destroyed(at: &Arc<RoutedMetaBackend>, ino: u64, what: &str) {
    let started = std::time::Instant::now();
    loop {
        if at.getattr(ino).await.is_err() {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{what}: ino {ino} still stands after 10 s — nobody reclaimed the corpse"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The manager + one joiner with the reclaim wire between them: the
/// joiner's step shipper (the ladder's rung 7) and, in front of each, the
/// FUSE layer — the manager's installed as its reclaim-hint sink.
struct HintFixture {
    manager: Arc<RoutedMetaBackend>,
    mvol: Arc<KvMetaBackend>,
    mvenue: DaemonVenue,
    joiner: Arc<RoutedMetaBackend>,
    jvol: Arc<KvMetaBackend>,
    jid: u32,
    jf: FsFront,
    mf: FsFront,
}

async fn hint_fixture(uris: &[String], venue_name: &str, slot_seed: u32) -> HintFixture {
    let manager = open_under(uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mvenue = DaemonVenue::stand_up(&manager, true, venue_name).await;
    mvol.checkpoint_now().await.unwrap();
    let joiner = {
        Knobs::armed().apply();
        let r = open_routed_meta_set_joined(
            uris,
            &JoinedSetAdmission {
                manager_endpoint: mvenue.endpoint.clone(),
                secret: VENUE_SECRET.to_vec(),
                peer_id: peer_of(&joiner_identity(&mvol, slot_seed).await),
                identity: joiner_identity(&mvol, slot_seed).await,
            },
        )
        .await;
        Knobs::clear();
        r.expect("the joined open")
    };
    let jvol = Arc::clone(&joiner.volumes[0]);
    let jid = jvol.appender_stats().unwrap().appender_id;
    let jidentity = jvol.joined_wire().unwrap().identity;
    squeezefs::meta_backend::crossvol_tx::install_xv_shipper(
        squeezefs::meta_ship::MetaShipRouter::new(
            Arc::clone(&joiner),
            &peer_of(&jidentity),
            VENUE_SECRET.to_vec(),
        ),
    );
    let jf = fs_in_front_of(&joiner, &format!("vol-13h-hint-j-{slot_seed}")).await;
    let mf = fs_in_front_of(&manager, &format!("vol-13h-hint-m-{slot_seed}")).await;
    mf.fs.install_reclaim_hint_sink();
    HintFixture {
        manager,
        mvol,
        mvenue,
        joiner,
        jvol,
        jid,
        jf,
        mf,
    }
}

async fn tear_down_hint_fixture(fx: HintFixture, uris: &[String]) {
    let HintFixture {
        manager,
        mvol,
        mvenue,
        joiner,
        jvol,
        jf,
        mf,
        ..
    } = fx;
    assert_must_stay_zero(&jvol, "joiner");
    assert_must_stay_zero(&mvol, "manager");
    squeezefs::meta_backend::crossvol_tx::uninstall_xv_shipper();
    drop(jf);
    shutdown(&joiner).await;
    drop(jvol);
    drop(joiner);
    drop(mf);
    mvenue.tear_down();
    shutdown(&manager).await;
    drop(mvol);
    drop(manager);
    fsck_clean(uris).await;
}

/// **Issue 1(a) — the UNLEASED corpse: a joiner's file, unlinked while
/// OPEN (the tmpfile pattern), whose slot the cadence RELEASED before the
/// close's FORGET.** F-R6's gate answers "not mine" for an unleased slot on
/// a joiner and dropped the forget; the manager owns an unleased slot by
/// the corpse sweep's law but sweeps only at MOUNT, and its kernel never
/// FORGETs an inode it never held — so the record (and its blocks, on a
/// data volume) leaked until the manager's next remount, where the
/// pre-PR door's first touch had destroyed it (a legal schedule: the
/// forced shrink of idle rotor slots as `writers_known` grows, the LRU
/// release past the page budget, the region release). The law: a FORGET
/// of a corpse in a slot this mount does not reclaim is SHIPPED as a
/// reclaim HINT to the slot's reclaimer — the manager for an unleased slot
/// — which runs it as its own FORGET (its admission, its plan, its destroy
/// in its own ring under its own lease); counted `reclaim_unleased_slot_
/// forgets` at the forgetter, `reclaim_hints_served` at the reclaimer.
/// RED on `00edad1a`: the forget is counted FOREIGN, nothing ships, the
/// corpse stands at the manager for ever. GREEN: destroyed within the
/// manager's reclaim cadence (one 20 ms batch window) — the record gone
/// at the manager, its destroy committed (nothing withheld), the
/// post-leave census clean. The fixture is metadata-only, so the record
/// is the witness; the blocks a data-volume corpse holds ride the same
/// destroy (the manager's own FORGET path, releases in the entry).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joiners_forget_of_a_corpse_in_a_released_slot_is_reclaimed_by_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "jd")]).await;
    let jd = dirs[0];
    let fx = hint_fixture(&uris, "manager-13h-hint-a", 73).await;
    // The joiner's file: its first touch of the seeded directory leases
    // the slot over the wire.
    let f = fx
        .joiner
        .create(jd, "tmp", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's file")
        .ino;
    let jid = fx.jid;
    // The file's record lives in the joiner's ROTOR (the creator's mint —
    // the directory's slot took the dentry alone); that slot is the one
    // the cadence releases.
    let local = fx.joiner.route_ino(f).1;
    let fslot = squeezefs::meta_backend::kv::record::forest_slot_of_ino(local);
    assert!(
        matches!(
            tree0_state(&fx.mvol, fslot).await,
            Some(SlotState::Leased { appender_id, .. }) if appender_id == jid
        ),
        "the premise: the joiner leases the file's slot {fslot}"
    );
    // Unlinked while OPEN: `nlink 0`, the kernel still holds the ino — no
    // FORGET yet.
    fx.joiner.unlink(jd, "tmp").await.expect("the unlink");
    assert_eq!(fx.joiner.getattr(f).await.unwrap().nlink, 0);
    fx.jvol.checkpoint_now().await.unwrap();
    // The cadence releases the idle slot BEFORE the close (the forced
    // shrink / LRU shape): unleased at tree 0.
    fx.jvol
        .release_slot_handover(jid, fslot)
        .await
        .expect("the cadence's release over the wire");
    assert!(matches!(
        tree0_state(&fx.mvol, fslot).await,
        Some(SlotState::Unleased { .. })
    ));
    assert!(
        !fx.jvol.inode_plane_owns_slot(local),
        "the premise: an unleased slot is nobody's on a joiner"
    );
    assert!(
        fx.mvol.inode_plane_owns_slot(local),
        "the premise: an unleased slot is the manager's to reclaim"
    );
    assert_eq!(
        fx.manager.getattr(f).await.unwrap().nlink,
        0,
        "the corpse stands at the manager, exact"
    );
    // The close → the joiner's kernel FORGETs → its reclaim path.
    let f0 = reclaim_faces_all();
    fx.jf.fs.reclaim_orphaned_batch(vec![f]).await;
    let f1 = reclaim_faces_all();
    assert_eq!(
        f1.refused, f0.refused,
        "nothing priced or withheld at the joiner"
    );
    assert_eq!(
        f1.unleased - f0.unleased,
        1,
        "the forget of an UNLEASED slot's corpse is counted as such — never as a holder's \
         (reclaim_unleased_slot_forgets); faces {f0:?} → {f1:?}"
    );
    assert_eq!(f1.foreign, f0.foreign, "…and never as a foreign holder's");
    assert_eq!(
        (
            f1.hints_shipped - f0.hints_shipped,
            f1.hint_inos - f0.hint_inos
        ),
        (1, 1),
        "ONE reclaim hint carrying the ino travelled to the manager"
    );
    assert_eq!(f1.hint_failures, f0.hint_failures, "…and landed");
    // The manager reclaims it as its own forget, within its reclaim pool's
    // cadence.
    wait_destroyed(
        &fx.manager,
        f,
        "Issue 1(a): the manager reclaims the hinted corpse",
    )
    .await;
    let f2 = reclaim_faces_all();
    assert_eq!(
        f2.served - f0.served,
        1,
        "the manager's reclaim admitted the hinted ino as its own forget (reclaim_hints_served)"
    );
    assert_eq!(f2.misrouted, f0.misrouted, "nothing misrouted");
    assert_eq!(f2.refused, f0.refused, "the manager's destroy committed");
    assert_eq!(
        f2.hint_inos - f0.hint_inos,
        (f2.served - f0.served) + (f2.forwarded - f0.forwarded) + (f2.misrouted - f0.misrouted),
        "the family closes in one unit (inos): shipped ≡ served + forwarded + misrouted"
    );
    // (The joiner's own read of the ino here is its PROJECTION — this
    // fixture arms no custody divert; the manager's word above is the
    // record's.)
    tear_down_hint_fixture(fx, &uris).await;
}

/// **Issue 1(b) — the MOVED-slot corpse: a slot handed over between the
/// unlink and the close's FORGET.** The joiner unlinks its open file, its
/// slot is released and the MANAGER first-touches it (a handover TO
/// another appender — dominance, an offer, a peer's first touch are the
/// same shape); the joiner's FORGET then names an ino in a slot ANOTHER
/// appender leases — F-R6's foreign arm — and the new holder's kernel
/// never held the ino, so nothing reclaimed it until the holder's next
/// remount (pre-existing: the departing holder's destroy met `SlotBusy`
/// at its door and was withheld). The hint travels to the slot's CURRENT
/// holder, which reclaims it as its own forget. RED on `00edad1a`: the
/// forget is counted foreign and dropped, the corpse stands at the holder
/// for ever. GREEN: destroyed at the holder within its reclaim cadence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corpse_whose_slot_moved_between_the_unlink_and_the_forget_is_reclaimed_by_its_new_holder(
) {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "jd")]).await;
    let jd = dirs[0];
    let fx = hint_fixture(&uris, "manager-13h-hint-b", 74).await;
    let jid = fx.jid;
    let f = fx
        .joiner
        .create(jd, "tmp", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the joiner's file")
        .ino;
    fx.joiner.unlink(jd, "tmp").await.expect("the unlink");
    fx.jvol.checkpoint_now().await.unwrap();
    let local = fx.joiner.route_ino(f).1;
    let fslot = squeezefs::meta_backend::kv::record::forest_slot_of_ino(local);
    // The slot moves: released by the joiner, first-touched by the
    // manager — a `chmod` of the unlinked-but-open file (its door's first
    // touch of the released slot; any mutation of a record in it is the
    // same act).
    fx.jvol
        .release_slot_handover(jid, fslot)
        .await
        .expect("the release over the wire");
    fx.manager
        .setattr(
            f,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("the manager's first touch of the released slot");
    assert!(
        matches!(
            tree0_state(&fx.mvol, fslot).await,
            Some(SlotState::Leased { appender_id: 0, .. })
        ),
        "the premise: slot {fslot} moved to the manager between the unlink and the forget"
    );
    assert!(!fx.jvol.inode_plane_owns_slot(local));
    assert_eq!(fx.manager.getattr(f).await.unwrap().nlink, 0);
    // The joiner's FORGET.
    let f0 = reclaim_faces_all();
    fx.jf.fs.reclaim_orphaned_batch(vec![f]).await;
    let f1 = reclaim_faces_all();
    assert_eq!(
        f1.refused, f0.refused,
        "nothing priced or withheld at the joiner"
    );
    assert_eq!(
        f1.foreign - f0.foreign,
        1,
        "the forget of a slot another appender leases is counted foreign (reclaim_foreign_slot_forgets)"
    );
    assert_eq!(
        (
            f1.hints_shipped - f0.hints_shipped,
            f1.hint_inos - f0.hint_inos
        ),
        (1, 1),
        "ONE reclaim hint travelled to the slot's holder"
    );
    wait_destroyed(
        &fx.manager,
        f,
        "Issue 1(b): the new holder reclaims the moved-slot corpse",
    )
    .await;
    let f2 = reclaim_faces_all();
    assert_eq!(
        f2.served - f0.served,
        1,
        "the holder admitted the hinted ino"
    );
    assert_eq!(
        (f2.forwarded, f2.misrouted),
        (f0.forwarded, f0.misrouted),
        "the forgetter asked the manager's word for the lessee: nothing forwarded, nothing dropped"
    );
    assert_eq!(f2.refused, f0.refused, "the holder's destroy committed");
    assert_eq!(
        f2.hint_inos - f0.hint_inos,
        (f2.served - f0.served) + (f2.forwarded - f0.forwarded) + (f2.misrouted - f0.misrouted),
        "the family closes in one unit (inos)"
    );
    tear_down_hint_fixture(fx, &uris).await;
}

// ---------------------------------------------------------------------------
// PR 13h review round 1, Issue 2 — the `-o ro` token reader is inside the
// law: a reader owns no slot and its FORGET reclaims nothing.
// ---------------------------------------------------------------------------

/// The process-wide READER posture (`-o ro` sets it at the mount), held
/// for a contract's body and RESET on drop — a red assertion never leaks
/// the posture into the suite's next test.
struct ReaderPosture;

impl ReaderPosture {
    fn enter() -> Self {
        squeezefs::fuse_client::set_read_only_mount(true);
        Self
    }
}

impl Drop for ReaderPosture {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_read_only_mount(false);
    }
}

/// **A `-o ro` token reader's FORGET of a corpse accounts NOTHING** (PR
/// 13h, review round 1, Issue 2). The archetypal token client is the
/// reader, and on a reader the lease gate is never armed, so F-R6's gate
/// (`owns_inode_reclaim`) answered `true` for every ino and the FORGET-
/// driven reclaim ran as shipped: the admission `getattr` DIVERTED to the
/// holder's plane (the recall had retired the reader's entry, so every
/// forgotten corpse was a fresh Grant RPC at the holder — `grants_served`
/// moves), the plan fetched the layout, the pricing walked the reader's
/// own KV image, and the read-only `destroy_inodes_releasing` was REFUSED
/// into `destroy WITHHELD` (`reclaim_destroy_refused_release_failed` + a
/// WARN) — under the holder's `rm -rf` of a tree the reader had cached,
/// the wire storm the token deviation exists to avoid, plus the WARN
/// storm, at the reader. The law: a reader reclaims nothing by POSTURE
/// (S5 — its session writes ZERO bytes): its forget drops the cache entry
/// and the side maps (the FORGET handler's own acts, before the reclaim
/// entry) and stops — counted `reclaim_reader_forgets`, at
/// `queue_reclaim_inode` (no pool spawned) and at the batch entry alike.
/// RED on `da8542fc`: `grants_served` +1 at the holder and one withheld
/// destroy per forgotten corpse. GREEN: `grants_served` unmoved, refused
/// unmoved, the reader face +1 per forget; the corpse stands at the
/// holder for its own FORGET path, exact — and that path DESTROYS it
/// (review round 2, Issue 15): a reader never unlinks, so the reclaiming
/// FORGET is always the unlinker's or the holder's own; a file only a
/// reader had open leaks nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readers_forget_of_a_corpse_takes_no_grant_and_withholds_nothing() {
    use squeezefs::meta_ship::token_plane::TokenClientConfig;
    use std::sync::atomic::Ordering::Relaxed;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    reset_process_state();
    let (uris, dirs) = seeded_volume(dir.path(), &[(SLOT_A, "shared")]).await;
    let shared = dirs[0];
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mvol = Arc::clone(&manager.volumes[0]);
    let mtokens = DaemonVenue::stand_up(&manager, true, "manager-13h-reader").await;
    let victim = manager
        .create(shared, "victim", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("the holder's file")
        .ino;
    mvol.checkpoint_now().await.expect("checkpoint");
    // The reader: read-only, its token client armed at the manager's
    // plane, the process in the READER posture (what `-o ro` sets).
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    rv.arm_reader_revalidation(None).expect("arms");
    rv.revalidate_reader().await.expect("poll");
    let plane = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: mtokens.endpoint.clone(),
            secret: VENUE_SECRET.to_vec(),
            client_id: "pr13h-reader-forget".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    let _posture = ReaderPosture::enter();
    // The reader instantiated the file (a token), then the holder unlinks
    // it: the commit recalls the reader's token; the record stands at the
    // holder with `nlink 0` (unlinked-but-open there, or simply not yet
    // forgotten).
    reader
        .getattr(victim)
        .await
        .expect("served under a token from the manager");
    assert_eq!(plane.stats().grants, 1);
    manager.unlink(shared, "victim").await.expect("the unlink");
    assert_eq!(manager.getattr(victim).await.unwrap().nlink, 0);
    let holder = mvol.token_holder().expect("the manager holds tokens");
    let rf = fs_in_front_of(&reader, "sqz-13h-reader").await;
    let grants0 = holder.stats().grants_served;
    let f0 = reclaim_faces_all();
    let reader0 = squeezefs::fuse_client::METRICS
        .reclaim_reader_forgets
        .load(Relaxed);
    // The reader's kernel FORGETs the corpse → its reclaim path, both
    // entries: the FORGET handler's queue and the pool's batch.
    rf.fs.queue_reclaim_inode(victim);
    rf.fs.reclaim_orphaned_batch(vec![victim]).await;
    // Anything the entries might have spawned has run by now (the pool's
    // batch window is 20 ms).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let f1 = reclaim_faces_all();
    assert_eq!(
        holder.stats().grants_served,
        grants0,
        "the reader's forget took NO grant at the holder (RED: a divert getattr per corpse)"
    );
    assert_eq!(
        f1.refused, f0.refused,
        "nothing priced or withheld at the reader (RED: destroy WITHHELD, read-only)"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .reclaim_reader_forgets
            .load(Relaxed)
            - reader0,
        2,
        "both entries count the reader's forget (reclaim_reader_forgets)"
    );
    assert_eq!(
        (f1.foreign, f1.unleased, f1.hints_shipped),
        (f0.foreign, f0.unleased, f0.hints_shipped),
        "a reader hints nobody — it owns no slot and the holder's own FORGET reclaims"
    );
    assert_eq!(
        manager.getattr(victim).await.unwrap().nlink,
        0,
        "the corpse stands at the holder, exact, for its own FORGET path"
    );
    // …and that path is what reclaims it (review round 2, Issue 15; round
    // 3, Issue 18 — the body the round-2 commit lost): the HOLDER's kernel
    // FORGETs — the unlinker's forget, never a reader's — and its own
    // reclaim destroys the corpse; a file only a reader had open leaks
    // nothing.
    // The posture word is PROCESS-wide (`-o ro` sets it at the mount): the
    // holder's daemon is another process on a fleet, so its reclaim runs
    // outside the reader posture here.
    drop(_posture);
    let mf = fs_in_front_of(&manager, "sqz-13h-reader-holder").await;
    mf.fs.reclaim_orphaned_batch(vec![victim]).await;
    wait_destroyed(
        &manager,
        victim,
        "Issue 15: the holder's own FORGET path reclaims the corpse the reader forgot",
    )
    .await;
    let f2 = reclaim_faces_all();
    assert_eq!(f2.refused, f0.refused, "the holder's destroy committed");
    assert_eq!(
        (f2.served, f2.foreign, f2.unleased, f2.hints_shipped),
        (f1.served, f1.foreign, f1.unleased, f1.hints_shipped),
        "the holder's own forget is nobody's hint"
    );
    assert!(
        reader.getattr(victim).await.is_err(),
        "the reader reads the corpse gone at its next resolve (a divert to the holder)"
    );
    drop(mf);
    drop(rf);
    for v in &reader.volumes {
        v.shutdown().await.unwrap();
    }
    drop(rv);
    drop(reader);
    mtokens.tear_down();
    shutdown(&manager).await;
}
