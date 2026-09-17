//! **Symmetric metadata PR 12 — the mount posture: every RW mount of an
//! ARMED set is a writer** (docs/design-symmetric-metadata.md §7.3, §6.1,
//! §6.2, §5.1.4 the Issue-31 obligation, §8 gate 1, §11;
//! `src/sym_join.rs`).
//!
//! What the contracts pin, red-first:
//!
//! 1. A plain mount of a bit-17-ABSENT set arms nothing: the ladder is
//!    not walked, no report exists, the retired posture knobs keep their
//!    shipped meaning, `dlm_rpcs == 0`, `mount_posture == writer`.
//! 2. Under `SQUEEZEFS_SYMMETRIC_META=1` the four posture knobs are
//!    RETIRED spellings — refused in the registry's own form, naming the
//!    successor; without the knob they are not.
//! 3. `volume set-owners` is refused on a symmetric-forest set (ownership
//!    is a slot LEASE), verbatim on a flat set.
//! 4. Rung 2 names the missing bit; rung 3 names the membership plane.
//! 5. **The headline**: a solo armed mount walks the ladder to its planes
//!    — the WERO joined on a PR-capable data namespace, the custody owner
//!    installed, the listener up, `manager_lease == held`, the report's
//!    rungs — with `dlm_rpcs == 0` by construction; the detection-grade
//!    lab posture takes no hold and says so.
//! 6. `plane_gate` keys on the held ALLOCATION LEASE on a grant-armed
//!    allocator: the holder frees and W1-patches; a non-holder's terminal
//!    free is refused naming the lease while its fresh allocation still
//!    passes on the grant window.
//! 7. **PR 4 review round 6, Issue 31**: an appender's OWN checkpoint
//!    refreshes its page's `head_hint` / `seq_offset` / `ckpt_seq`, the
//!    manager's release screen reads them (a > 2-lap release lands under
//!    the DERIVED bound, which the join-time page would have refused),
//!    and the manager never rewrites a page an appender's checkpoint owns.
//!
//! Process-global state (the knobs, the posture latch, the holdings, the
//! custody owner) serializes the file through `SEAM`.

mod common;

use common::sym::{
    data_file, format_flat_member, format_stamped_member, format_stamped_set_with_config,
    fsck_clean, mount_data, open_under, rewrite_page, seed_dir_in_slot, shutdown, HoldersVenue,
    Knobs,
};
use squeezefs::meta_backend::kv::alloc_lease;
use squeezefs::meta_backend::kv::appender::{
    read_directory, release_seq_floor_bound, screen_release_words, AppenderIdentity,
    ReleaseWordBounds, SEQ_FRONTIER_SANE_MAX,
};
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV;
use squeezefs::meta_backend::kv::slot_state::ExtentGrantRecord;
use squeezefs::meta_backend::reservation::{
    clear_override, install_override, FakeNvmeNamespace, FakeReservationClient, ReservationClient,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::slot_lease_core::SlotWords;
use std::sync::Arc;

static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ROOT: u64 = 1;
/// The declared region's slot in the Issue-31 contract (PR 6's `SLOT_B`).
const SLOT_B: ForestSlot = 4;
const TWO_HOLDERS: &str = "1:4";

/// Restores every process-global the contracts touch.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        Knobs::clear();
        for (k, _) in squeezefs::sym_join::RETIRED_ON_ARMED {
            std::env::remove_var(k);
        }
        std::env::remove_var("SQUEEZEFS_MEMBERSHIP_BIND");
        std::env::remove_var("SQUEEZEFS_MW_BIND");
        squeezefs::sym_join::clear_report();
        squeezefs::fuse_client::set_mount_posture(squeezefs::fuse_client::MountPosture::Writer);
        alloc_lease::test_clear_holdings();
        squeezefs::data_grant::uninstall_custody_owner();
        squeezefs::data_grant::uninstall_custody_client();
        squeezefs::meta_ship::publish::uninstall_client();
        squeezefs::meta_ship::disarm_ownership();
        squeezefs::membership::uninstall();
    }
}

/// Plant the `job:enroll` secret the cluster wire trusts (possession of
/// volume access IS membership — ruling D2): the ladder's rungs 3 and 7
/// read it.
async fn plant_secret(routed: &RoutedMetaBackend, secret: &[u8]) {
    let enroll = serde_json::json!({ "secret": squeezefs::cluster_wire::hex_encode(secret) });
    routed.volumes[0]
        .setxattr_internal(
            ROOT,
            squeezefs::job_wire::JOB_ENROLL_XATTR,
            enroll.to_string().as_bytes(),
        )
        .await
        .expect("write the enroll record");
}

// ===========================================================================
// 1. A bit-17-absent set: the ladder is never walked
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_mount_of_a_bit17_absent_set_arms_nothing() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_flat_member(dir.path(), "meta0").await];
    let before_rpcs = squeezefs::dlm_slot::dlm_rpcs();
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    assert!(
        !squeezefs::sym_join::set_armed(&routed),
        "a flat set is never armed"
    );
    let data = data_file();
    let armed = squeezefs::sym_join::arm(&routed, &[data.path().to_path_buf()], None, None)
        .await
        .expect("the ladder answers Ok on an unarmed set");
    assert!(armed.is_none(), "and arms NOTHING (the shipped posture)");
    assert!(squeezefs::sym_join::report().is_none(), "no report exists");
    // The posture knobs keep their shipped meaning: no retired refusal
    // without the plane's knob, whatever they say.
    std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
    std::env::set_var("SQUEEZEFS_MW_ROLE", "co-writer");
    assert!(
        squeezefs::sym_join::retired_knob_refusal().is_none(),
        "the retirement fires only beside SQUEEZEFS_SYMMETRIC_META=1"
    );
    std::env::remove_var("SQUEEZEFS_MULTI_WRITER");
    std::env::remove_var("SQUEEZEFS_MW_ROLE");
    assert_eq!(
        squeezefs::fuse_client::mount_posture().as_str(),
        "writer",
        "a plain mount is a writer"
    );
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        before_rpcs,
        "dlm_rpcs == 0"
    );
    routed
        .create(ROOT, "plain", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a plain mount writes");
    shutdown(&routed).await;
}

// ===========================================================================
// 2. The retired posture knobs
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retired_posture_knobs_refuse_only_beside_the_plane_naming_the_successor() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    std::env::set_var(SYMMETRIC_META_ENV, "1");
    assert!(
        squeezefs::sym_join::retired_knob_refusal().is_none(),
        "the plane alone refuses nothing"
    );
    for (key, successor) in squeezefs::sym_join::RETIRED_ON_ARMED {
        std::env::set_var(key, "1");
        let refusal = squeezefs::sym_join::retired_knob_refusal()
            .unwrap_or_else(|| panic!("{key} beside the plane must refuse"));
        assert!(refusal.contains(key), "names the knob: {refusal}");
        assert!(
            refusal.contains("RETIRED") && refusal.contains("forward-only"),
            "the registry's retired-spelling form: {refusal}"
        );
        assert!(
            refusal.contains(successor),
            "names the successor: {refusal}"
        );
        assert!(
            refusal.contains("design-symmetric-metadata"),
            "cites the law: {refusal}"
        );
        std::env::remove_var(key);
    }
    // Every offender at once (the registry gate's law).
    std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
    std::env::set_var("SQUEEZEFS_MW_ROLE", "set-authority");
    let refusal = squeezefs::sym_join::retired_knob_refusal().expect("refuses");
    assert!(refusal.contains("SQUEEZEFS_MULTI_WRITER") && refusal.contains("SQUEEZEFS_MW_ROLE"));
    // An EMPTY value is unset (the ONE knob convention).
    std::env::set_var("SQUEEZEFS_MW_AUTHORITY", "   ");
    std::env::remove_var("SQUEEZEFS_MULTI_WRITER");
    std::env::remove_var("SQUEEZEFS_MW_ROLE");
    assert!(squeezefs::sym_join::retired_knob_refusal().is_none());
    // `SQUEEZEFS_MW_BIND` is NOT retired: every writer serves, and the
    // bind is where (§6.1 retires the four, never the bind).
    std::env::remove_var("SQUEEZEFS_MW_AUTHORITY");
    std::env::set_var("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    assert!(squeezefs::sym_join::retired_knob_refusal().is_none());
    std::env::remove_var(SYMMETRIC_META_ENV);
    std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
    assert!(
        squeezefs::sym_join::retired_knob_refusal().is_none(),
        "without the plane the knobs keep their shipped meaning"
    );
}

// ===========================================================================
// 3. volume set-owners on a symmetric-forest set
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volume_set_owners_is_refused_on_a_symmetric_forest_set_naming_the_lease() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let stamped = vec![format_stamped_member(dir.path(), "sym0").await];
    // The set's durable volume id, off a probe (the verb keys on it).
    let routed = open_under(&stamped, &Knobs::unarmed()).await;
    let vol_id = squeezefs::meta_backend::kv::backend::durable_volume_id_of(
        &routed.volumes[0].superblock().uuid,
    );
    shutdown(&routed).await;
    drop(routed);
    let specs = vec![squeezefs::config_ops::OwnerAssignSpec {
        volume_id: vol_id.clone(),
        owner: "node_0123456789abcdef".to_string(),
        successors: Vec::new(),
        subtree_root: None,
    }];
    let opts = squeezefs::config_ops::SetOwnersOptions::default();
    let err = squeezefs::config_ops::set_owners(&stamped, &specs, &opts)
        .await
        .expect_err("set-owners on a bit-17 set is refused");
    let msg = err.to_string();
    assert!(msg.contains("RETIRED"), "the retired-spelling form: {msg}");
    assert!(msg.contains("incompat bit 17"), "names the bit: {msg}");
    assert!(msg.contains("slot LEASE"), "names the successor: {msg}");
    assert!(
        msg.contains("SQUEEZEFS_SYMMETRIC_META=1"),
        "and the remedy: {msg}"
    );

    // A FLAT multi-writer-class set never sees that text: the verb runs
    // its shipped gates verbatim (here it reaches the durable-id check,
    // since the spec names the stamped set's volume).
    let flat = vec![format_flat_member(dir.path(), "flat0").await];
    let err = squeezefs::config_ops::set_owners(&flat, &specs, &opts)
        .await
        .expect_err("a spec naming another set's volume is refused by the shipped gate");
    let msg = err.to_string();
    assert!(
        !msg.contains("RETIRED for a symmetric-forest set"),
        "a flat set keeps the verb's shipped meaning: {msg}"
    );
    assert!(
        msg.contains("no volume of this set carries the durable id"),
        "the shipped gate's own text: {msg}"
    );
}

// ===========================================================================
// 4. The rungs' refusals name themselves
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rung_2_names_the_missing_bit_and_rung_3_the_membership_plane() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    // A flat multi-writer-class set carries every bit but 17.
    let flat = vec![format_flat_member(dir.path(), "flat0").await];
    let routed = open_under(&flat, &Knobs::unarmed()).await;
    let err = squeezefs::sym_join::check_bits(&routed).expect_err("bit 17 missing");
    let msg = err.to_string();
    assert!(msg.contains("rung 2 (bits)"), "names the rung: {msg}");
    assert!(msg.contains("incompat bit 17"), "names the bit: {msg}");
    assert!(msg.contains("enable-symmetric"), "names the act: {msg}");
    shutdown(&routed).await;

    // A stamped, ARMED set passes rung 2 and meets rung 3: the membership
    // plane is off and no ladder arms it here.
    let stamped = vec![format_stamped_member(dir.path(), "sym0").await];
    let routed = open_under(&stamped, &Knobs::armed()).await;
    assert!(squeezefs::sym_join::set_armed(&routed));
    squeezefs::sym_join::check_bits(&routed).expect("every bit present");
    let data = data_file();
    let err = squeezefs::sym_join::arm(&routed, &[data.path().to_path_buf()], None, None)
        .await
        .expect_err("rung 3 refuses with the membership plane off");
    let msg = err.to_string();
    assert!(msg.contains("rung 3 (membership)"), "names the rung: {msg}");
    assert!(
        msg.contains("SQUEEZEFS_MEMBERSHIP_BIND"),
        "names the knob: {msg}"
    );
    assert!(
        squeezefs::sym_join::report().is_none(),
        "a refused ladder leaves no report"
    );
    shutdown(&routed).await;
}

// ===========================================================================
// 5. The headline: a solo armed mount walks the ladder
// ===========================================================================

/// The recipe every armed-ladder contract shares: a stamped volume with
/// the enroll secret, opened armed, its membership shard armed at `auto`
/// the way the mount path's rung 3 does.
async fn armed_with_membership(
    dir: &std::path::Path,
) -> (
    Vec<String>,
    Arc<RoutedMetaBackend>,
    squeezefs::membership::MembershipArm,
) {
    let uris = format_stamped_set_with_config(dir, 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    plant_secret(&routed, b"pr12-ladder-secret").await;
    assert_eq!(squeezefs::membership::membership_mode(), "off");
    let arm = squeezefs::sym_join::arm_membership_shard(&routed, None)
        .await
        .expect("rung 3 arms at auto")
        .expect("an owner arm");
    assert_eq!(arm.mode(), "owner", "the manager is its shard's S6 owner");
    (uris, routed, arm)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_armed_mount_walks_the_ladder_to_its_planes_with_dlm_rpcs_zero() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let (uris, routed, membership) = armed_with_membership(dir.path()).await;
    let before_rpcs = squeezefs::dlm_slot::dlm_rpcs();
    // A PR-capable data namespace (the fidelity tier's fake device).
    let data = dir.path().join("data-pr");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::new();
    install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-pr12", "host-pr12"),
    );
    std::env::set_var("SQUEEZEFS_MW_BIND", "127.0.0.1:0");

    let (arm, report) = squeezefs::sym_join::arm(&routed, std::slice::from_ref(&data), None, None)
        .await
        .expect("the ladder walks")
        .expect("an armed set arms");
    assert_eq!(
        report.rungs,
        vec![
            "declaration",
            "bits",
            "membership",
            "registrant",
            "join_appender",
            "acquire_slots",
            "planes"
        ],
        "every rung, in the design's order"
    );
    assert_eq!(report.data_namespaces_registered, 1);
    assert!(!report.detection_grade, "a PR substrate is device-enforced");
    assert!(
        ns.holder().is_some(),
        "rung 4 joined the data namespace's WERO hold"
    );
    assert!(!report.endpoint.is_empty() && report.endpoint == arm.endpoint());
    assert_eq!(
        squeezefs::sym_join::report().as_deref(),
        Some(&report),
        "the report is what the stats inode publishes"
    );
    assert!(
        squeezefs::data_grant::custody_owner().is_some(),
        "rung 7 installed the custody owner on this writer (PR 9's deviation 5)"
    );
    // The role gauges say what the writer holds; there is no role word.
    let stats = routed.volumes[0].appender_stats().expect("a forest volume");
    assert_eq!(stats.manager_lease.word(), "held");
    assert!(
        routed.volumes[0]
            .slot_lease_stats()
            .expect("armed")
            .leases_held
            >= 1
    );
    assert_eq!(squeezefs::fuse_client::mount_posture().as_str(), "writer");
    // Gate 1's law: a solo writer pays no lock round trip.
    for i in 0..8 {
        routed
            .create(ROOT, &format!("own{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
    }
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        before_rpcs,
        "dlm_rpcs == 0"
    );

    arm.disarm().await;
    assert!(
        squeezefs::data_grant::custody_owner().is_none(),
        "the leave uninstalls the planes the ladder stood up"
    );
    // The leave RELEASES the data namespace's reservation and unregisters
    // this writer's key before `disarm` returns — zero residue, the
    // fidelity tier's law. The S9 custody sweep held a strong clone of the
    // hold across its 10 s cadence, so the arm's release never reached the
    // device before the process exited (the real-device leg read
    // `regctl data=1` after every clean leave — the successor's OWN key).
    assert!(
        ns.holder().is_none(),
        "the data namespace's WERO is released at the leave, not at process exit"
    );
    assert_eq!(
        squeezefs::data_custody::live_wero_key(),
        None,
        "no live hold after the leave"
    );
    membership.disarm().await;
    clear_override(&data);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ladder_on_a_non_pr_substrate_is_detection_grade_only_under_the_lab_opt_in() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let (uris, routed, membership) = armed_with_membership(dir.path()).await;
    let data = dir.path().join("data-nonpr");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::without_pr_support();
    install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-pr12n", "host-pr12n"),
    );
    std::env::set_var("SQUEEZEFS_MW_BIND", "127.0.0.1:0");

    // Without the opt-in rung 4 refuses, naming the substrate and the knob.
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let err = squeezefs::sym_join::arm(&routed, std::slice::from_ref(&data), None, None)
        .await
        .expect_err("a non-PR data namespace refuses without the opt-in");
    let msg = err.to_string();
    assert!(msg.contains("rung 4 (registrant)"), "names the rung: {msg}");
    assert!(
        msg.contains("SQUEEZEFS_SYM_ALLOW_NON_PR"),
        "names the opt-in: {msg}"
    );
    assert!(ns.holder().is_none(), "a refused rung takes nothing");

    // With it the ladder walks, detection-grade, announced in the report.
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let (arm, report) = squeezefs::sym_join::arm(&routed, std::slice::from_ref(&data), None, None)
        .await
        .expect("the lab posture arms")
        .expect("armed");
    assert!(report.detection_grade);
    assert_eq!(report.data_namespaces_registered, 0);
    assert!(
        ns.holder().is_none(),
        "no reservation on a substrate that has none"
    );
    arm.disarm().await;
    membership.disarm().await;
    clear_override(&data);
    shutdown(&routed).await;
    fsck_clean(&uris).await;
}

/// §5.8.1 on the PRODUCT shape: the knob-armed writer — no declared test
/// partition, the shape every field mount takes — holds WERO (rtype 3) on
/// a PR-capable METADATA namespace, so a second host's appender can
/// REGISTER under it to write its own ring and be fenced by a preempt of
/// its key. PR 3 keyed the posture on PR 2's declared partition alone, so
/// the knob-armed manager held the shipped rtype 1 (found by the fidelity
/// tier's `sym-join-ladder` leg on the real nvmet target: `meta_pr_wero=0`,
/// device rtype 1, beside `data_plane_fence_mode=1`). The unarmed forest
/// keeps rtype 1 verbatim, and the knob-armed writer on a NON-PR namespace
/// is the detection-grade lab posture under the opt-in (KD-SYM-13),
/// refused without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_knob_armed_writer_holds_wero_on_a_pr_capable_metadata_namespace() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let meta = std::path::PathBuf::from(&uris[0]);
    let ns = FakeNvmeNamespace::lenient_register();
    install_override(
        &meta,
        FakeReservationClient::new(Arc::clone(&ns), "nqn.pr12.meta", "pr12-meta-host"),
    );
    // Unarmed (the knob off): the shipped Write Exclusive, rtype 1.
    {
        let routed = open_under(&uris, &Knobs::unarmed()).await;
        let s = routed.volumes[0].appender_stats().expect("a forest volume");
        assert!(!s.meta_pr_wero, "an unarmed forest keeps rtype 1: {s:?}");
        let report = FakeReservationClient::new(Arc::clone(&ns), "nqn.probe", "probe")
            .report()
            .expect("report");
        assert_eq!(report.rtype, 1, "the shipped Write Exclusive");
        shutdown(&routed).await;
    }
    // Armed by the knob alone (no partition): WERO, rtype 3 — the device
    // can fence a registered peer appender.
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let s = routed.volumes[0].appender_stats().expect("a forest volume");
        assert!(
            s.meta_pr_wero,
            "the knob-armed manager holds WERO on the metadata namespace: {s:?}"
        );
        assert_eq!(s.manager_lease.word(), "held");
        assert_eq!(routed.volumes[0].writer_guard_mode(), "flock+pr");
        let report = FakeReservationClient::new(Arc::clone(&ns), "nqn.probe", "probe")
            .report()
            .expect("report");
        assert_eq!(report.rtype, 3, "Write Exclusive – Registrants Only");
        assert!(report.is_wero());
        shutdown(&routed).await;
        assert!(
            ns.holder().is_none(),
            "the clean leave releases the hold — zero residue"
        );
    }
    clear_override(&meta);
    // The same knob-armed writer on a NON-PR namespace: refused without the
    // opt-in, detection-grade with it (the in-process suites' posture).
    let ns_nonpr = FakeNvmeNamespace::without_pr_support();
    install_override(
        &meta,
        FakeReservationClient::new(Arc::clone(&ns_nonpr), "nqn.pr12.nonpr", "pr12-nonpr-host"),
    );
    {
        Knobs::armed().apply();
        std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
        let err = squeezefs::meta_backend::open_routed_meta_set(&uris)
            .await
            .err()
            .map(|e| e.to_string())
            .expect("an armed writer on a non-PR metadata namespace refuses without the opt-in");
        assert!(
            err.contains("SQUEEZEFS_SYM_ALLOW_NON_PR") && err.contains("KD-SYM-13"),
            "names the opt-in and the rule: {err}"
        );
    }
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let s = routed.volumes[0].appender_stats().expect("a forest volume");
        assert!(!s.meta_pr_wero, "detection-grade: no reservation to hold");
        shutdown(&routed).await;
    }
    clear_override(&meta);
    fsck_clean(&uris).await;
}

// ===========================================================================
// 5b. The reader's PER-SLOT binding (PR 5's declared-authority seam collapsed)
// ===========================================================================

/// A `-o ro` token reader resolves every object's HOLDER through its tree 0
/// (§5.1.6): an object in a slot the manager holds (or nobody does) is
/// served by the manager's plane — the one the reader dialed first — while
/// an object in a slot ANOTHER appender leases is served by a plane dialed
/// to THAT holder's bound endpoint, created at the first resolve (one plane
/// per LISTENER — a holder the manager's daemon also serves rides the
/// manager's plane and its one recall channel); an object
/// whose holder has no bound endpoint REFUSES loud (`EAGAIN`, naming the
/// holder, `dlm_token_reader_unbound_holders`), never the projection. The
/// contracts bind the endpoints directly (the join ladder's rung 7 publishes
/// a writer's into its claim-set entry, which `resolve_holder_endpoint`
/// reads on any mount). In one process the second holder is the declared
/// region, whose objects this process's token service serves too; a second
/// PROCESS serving them is the N-daemon venue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reader_dials_each_objects_slot_holder_and_refuses_an_unbound_one() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::meta_ship::token_plane::{TokenClientConfig, TokenSetService};
    const SECRET: &[u8] = b"pr12-per-slot-binding-secret";
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_stamped_member(dir.path(), "sym0").await];
    // A directory in slot B seeded while the slot is the manager's, then
    // released, then leased to the declared region (PR 6's fixture).
    let (shared, mine) = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let shared = seed_dir_in_slot(&routed, 0, SLOT_B, "shared").await;
        let mine = routed
            .create(ROOT, "mine", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("an own file")
            .ino;
        routed.volumes[0]
            .release_slot_handover(0, SLOT_B)
            .await
            .expect("release to unleased");
        shutdown(&routed).await;
        (shared, mine)
    };
    let writer = open_under(&uris, &Knobs::armed().partition(TWO_HOLDERS)).await;
    let holder_vol = Arc::clone(&writer.volumes[0]);
    assert_eq!(
        holder_vol
            .slot_leases()
            .expect("armed")
            .holders
            .holder(SLOT_B)
            .map(|h| h.appender_id),
        Some(1),
        "slot B is the declared region's"
    );
    holder_vol.checkpoint_now().await.expect("publish tree 0");
    // Two listeners: A is the manager's (appender 0), B stands for the
    // second holder's — both served by this process's token service.
    let listener = |name: &'static str| {
        cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            TokenSetService::new(&writer.volumes),
        )
        .unwrap_or_else(|e| panic!("listener {name}: {e}"))
    };
    let a = listener("A");
    let b = listener("B");
    let endpoint_a = a.endpoint().to_string();
    let endpoint_b = b.endpoint().to_string();

    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    let default = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint_a.clone(),
            secret: SECRET.to_vec(),
            client_id: "pr12-reader".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    assert_eq!(default.endpoint(), endpoint_a);
    let wait_fresh = |plane: Arc<squeezefs::meta_ship::token_plane::TokenReaderPlane>| async move {
        let started = std::time::Instant::now();
        while !plane.stats().channel_fresh {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "the recall channel never completed its first round"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    wait_fresh(Arc::clone(&default)).await;

    // An own-slot object: the manager's plane, and it SERVES.
    let (_, mine_local) = reader.route_ino(mine);
    let plane = rv
        .token_reader_for(mine_local)
        .await
        .expect("resolves")
        .expect("a token reader");
    assert!(
        Arc::ptr_eq(&plane, &default),
        "the manager's plane serves its own slot"
    );
    let grants_before = default.stats().grants;
    reader
        .getattr(mine)
        .await
        .expect("served under a token from A");
    assert_eq!(default.stats().grants, grants_before + 1);
    assert!(rv.reader_holder_planes().is_empty(), "no second plane yet");

    // A foreign-held object with NO endpoint bound for its holder: refused
    // loud, naming the holder — never the projection.
    let (_, shared_local) = reader.route_ino(shared);
    let unbound_before = squeezefs::meta_ship::token_plane::reader_unbound_holders();
    let err = rv
        .token_reader_for(shared_local)
        .await
        .expect_err("an unbound holder refuses");
    let msg = err.to_string();
    assert!(msg.contains("appender 1"), "names the holder: {msg}");
    assert!(
        msg.contains("never served in its place"),
        "and refuses the projection: {msg}"
    );
    assert_eq!(
        squeezefs::meta_ship::token_plane::reader_unbound_holders(),
        unbound_before + 1
    );
    let err = reader
        .getattr(shared)
        .await
        .expect_err("the divert refuses too");
    assert!(err.to_string().contains("appender 1"), "{err}");

    // Bind holder 1 → B: the first resolve dials a SEPARATE plane to B,
    // while the manager's plane keeps dialing A.
    rv.bind_reader_holder_endpoint(1, &endpoint_b);
    let plane_b = rv
        .token_reader_for(shared_local)
        .await
        .expect("resolves")
        .expect("a token reader");
    assert!(!Arc::ptr_eq(&plane_b, &default), "a per-holder plane");
    assert_eq!(
        plane_b.endpoint(),
        endpoint_b,
        "dialed to the holder's endpoint"
    );
    assert_eq!(
        default.endpoint(),
        endpoint_a,
        "the manager's plane unchanged"
    );
    assert_eq!(rv.reader_holder_planes().len(), 1);
    let again = rv
        .token_reader_for(shared_local)
        .await
        .expect("resolves")
        .expect("a token reader");
    assert!(
        Arc::ptr_eq(&again, &plane_b),
        "one plane per listener, reused"
    );
    wait_fresh(Arc::clone(&plane_b)).await;
    // A holder bound to the manager's OWN listener rides the manager's
    // plane — one recall channel per (client, listener).
    rv.bind_reader_holder_endpoint(2, &endpoint_a);
    assert_eq!(rv.reader_holder_planes().len(), 1);
    // The foreign object is served under a token from B (this process
    // holds the declared region too, so its service answers for it), and
    // the manager's plane was never asked.
    reader
        .getattr(shared)
        .await
        .expect("served under a token from the holder's plane");
    assert_eq!(plane_b.stats().grants, 1, "one grant from B");
    assert_eq!(
        default.stats().grants,
        grants_before + 1,
        "the manager's plane was never asked for the foreign object"
    );
    assert!(plane_b.holds(shared_local) && !default.holds(shared_local));

    shutdown(&reader).await;
    a.shutdown();
    b.shutdown();
    shutdown(&writer).await;
}

// ===========================================================================
// 6. plane_gate keys on the held ALLOCATION LEASE
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plane_gate_keys_on_the_allocation_lease_of_a_grant_armed_allocator() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let data = data_file();
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    let tag = rig.tag();
    let held = alloc_lease::arm_symmetric_allocation(&rig.routed, &[Arc::clone(&rig.alloc)])
        .await
        .expect("the allocation lease arms");
    assert_eq!(held, 1, "this writer holds its data volume's lease");
    assert!(rig.alloc.block_grant_armed(), "and mints from grants");
    assert!(alloc_lease::holding(tag).is_some());

    // The HOLDER: fresh allocation, the W1 sole-owner patch and the
    // terminal free all pass — every mount runs them for what it holds.
    let ino = rig.mk_file("own").await;
    let offset = rig.publish_block(ino, 0).await;
    assert!(
        rig.alloc.begin_patch_sole_owner(offset),
        "W1 runs on the holder for its own block"
    );
    // The patch's DMA would follow; the caller restores stability under a
    // new generation exactly as the write path does.
    rig.alloc.publish_block(offset);
    let spare = rig.alloc.allocate_block().await.expect("allocate");
    rig.alloc.publish_block(spare);
    rig.alloc
        .free_block(spare)
        .await
        .expect("the holder's terminal free runs the ladder locally");

    // A NON-HOLDER of this volume's lease (the holding dropped — the shape
    // a second writer whose data volume's lease another node holds has):
    // the accounting arms refuse naming the LEASE, while fresh allocation
    // still passes on the grant window.
    let dropped = alloc_lease::drop_holding(tag).expect("the holding");
    let refusals_before = squeezefs::fuse_client::METRICS
        .cowriter_accounting_refusals
        .load(std::sync::atomic::Ordering::Relaxed);
    let minted = rig
        .alloc
        .allocate_block()
        .await
        .expect("fresh allocation is admitted from the grant window");
    rig.alloc.publish_block(minted);
    let err = rig
        .alloc
        .free_block(minted)
        .await
        .expect_err("a terminal free is the lease holder's");
    let msg = err.to_string();
    assert!(msg.contains("ALLOCATION LEASE"), "names the lease: {msg}");
    assert!(
        msg.contains(&format!("{tag:#018x}")),
        "and the data volume: {msg}"
    );
    assert!(
        !rig.alloc.begin_patch_sole_owner(minted),
        "W1 declines on a volume whose accounting this mount does not hold"
    );
    assert!(
        squeezefs::fuse_client::METRICS
            .cowriter_accounting_refusals
            .load(std::sync::atomic::Ordering::Relaxed)
            > refusals_before,
        "the refusal rides the accounting gauge"
    );
    // Restore the holding so the leave accounts the volume.
    alloc_lease::test_install_holding(dropped);
    rig.alloc
        .free_block(minted)
        .await
        .expect("the holder again");
    assert!(
        rig.drift().await.is_empty(),
        "the C8 oracle reads the ledger and the layouts in agreement"
    );
    rig.shutdown().await;
}

// ===========================================================================
// 6b. PR 7's owed window — the served MarkShared under the W1 site's guard
// ===========================================================================

/// The served `MarkShared` runs UNDER the source file's block guard
/// (`BLOCK_FLUSH_LOCKS(global ino, block index)`, lock-order rung 3 — the
/// guard the local W1 patch site holds from its sole-owner predicate through
/// its DMA), so a foreign cloner's mark lands wholly before a patch's fenced
/// mark load or wholly after its DMA — never between the patcher's durable
/// probe and its mark load (PR 7 review round 2, Issue 20's window). The
/// detection tripwire `served_mark_shared_under_patch` stays as the
/// must-stay-0 belt behind the guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_mark_shared_waits_out_the_patch_sites_block_guard() {
    use squeezefs::meta_backend::kv::block_refs::BlockRef;
    use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply};
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let data = data_file();
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let offset = rig.publish_block(src, 0).await;
    let idx = offset / rig.alloc.chunk_size();
    let (_v, local) = rig.routed.route_ino(src);
    let venue = HoldersVenue::stand_up(&rig.routed, &[]).await;
    let endpoint = venue.endpoint();
    let tripwires_before = squeezefs::fuse_client::METRICS
        .invariant_tripwires
        .load(std::sync::atomic::Ordering::Relaxed);

    // A W1 patch in flight on (src, block 0): the patch site's guard held,
    // the incarnation word unstable.
    let guard = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS
        .get_lock(src, 0)
        .lock()
        .await;
    assert!(
        rig.alloc.begin_patch_sole_owner(offset),
        "the patch is admitted"
    );

    // A foreign cloner's MarkShared over the wire: it must WAIT for the
    // guard — the mark lands only after the patch completes.
    let mut client = ManagerClient::connect(&endpoint, common::sym::VENUE_SECRET, "pr12-cloner", 0)
        .await
        .expect("dial the manager");
    let reference = BlockRef {
        vol_tag: rig.tag(),
        block_idx: idx,
        owner_ino: local,
        block_index: 0,
    };
    let marked = tokio::spawn(async move { client.mark_shared(reference).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !marked.is_finished(),
        "the served mark waits on the patch site's guard"
    );
    assert!(
        !rig.alloc.is_shared(offset),
        "no RAM mark under a patch in flight"
    );

    // The patch completes (its DMA lands, stability restored) and releases
    // the guard: the mark lands, the RAM mark set, the tripwire untouched.
    rig.alloc.publish_block(offset);
    drop(guard);
    let reply = marked
        .await
        .unwrap()
        .expect("the mark lands after the patch");
    assert_eq!(reply, ManagerReply::Marked { already: false });
    assert!(
        rig.alloc.is_shared(offset),
        "the RAM mark landed after the guard"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .invariant_tripwires
            .load(std::sync::atomic::Ordering::Relaxed),
        tripwires_before,
        "served_mark_shared_under_patch is unreachable under the guard"
    );
    // And the NEXT patch attempt declines: the block is SHARED.
    let guard = squeezefs::fuse_client::BLOCK_FLUSH_LOCKS
        .get_lock(src, 0)
        .lock()
        .await;
    assert!(
        !rig.alloc.begin_patch_sole_owner(offset),
        "W1 declines a shared block"
    );
    rig.alloc.publish_block(offset);
    drop(guard);
    venue.tear_down();
    rig.shutdown().await;
}

// ===========================================================================
// 7. PR 4 review round 6, Issue 31 — the appender's own checkpoint refreshes
//    its page's words, and the release bound reads them
// ===========================================================================

/// The DECLARED region is the appender whose OWN checkpoint writes its
/// page (`write_appender_pages`, PR 2): after it journals past two laps
/// of its ring, its page carries `ckpt_seq ≥ 1`, `head_hint` = its ring's
/// head and `seq_offset` in force — so the manager's derived release
/// bound admits the region's real stamp frontier where the join-time page
/// (`head_hint` = start, `ckpt_seq` 0) would have refused it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_appenders_own_checkpoint_refreshes_its_page_and_the_release_bound_reads_it() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_stamped_member(dir.path(), "sym0").await];
    // Seed a directory in slot B while the slot is the manager's, release
    // it, and reopen with the declared region leasing it (PR 6's fixture).
    let shared = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let shared = seed_dir_in_slot(&routed, 0, SLOT_B, "shared").await;
        routed.volumes[0]
            .release_slot_handover(0, SLOT_B)
            .await
            .expect("release to unleased");
        shutdown(&routed).await;
        shared
    };
    let routed = open_under(&uris, &Knobs::armed().partition(TWO_HOLDERS)).await;
    // The declared region is ANOTHER appender for every cross-owner
    // decision (PR 6): a create under `shared` ships to it and lands in
    // ITS ring — exactly the storm that fills region 1.
    let venue = HoldersVenue::stand_up(&routed, &[1]).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let set = vol.appenders_public().expect("a forest volume");
    let region = set.region(1).expect("the declared region");
    let ring = region.ring();
    let ring_len = ring.core().geometry().logical_len();
    let start_head = ring.core().head();

    // Journal PAST two laps of the region's ring (the storm rides the
    // declared region: every create under `shared` is slot B's, and slot
    // B is region 1's) — the checkpoint cadence covers the window as it
    // fills.
    let mut i = 0u64;
    while ring.core().head() < start_head + 2 * ring_len + ring_len / 4 {
        routed
            .create(shared, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create in slot B");
        i += 1;
        assert!(i < 200_000, "the ring must lap under the storm");
    }
    vol.checkpoint_now().await.expect("checkpoint");
    let head = ring.core().head();
    let frontier = ring.seq_frontier();
    assert!(head >= start_head + 2 * ring_len, "> 2 laps journaled");

    // The page as the region's OWN checkpoint wrote it.
    let page = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .expect("directory")
        .into_iter()
        .find(|e| e.appender_id == 1)
        .and_then(|e| e.page)
        .expect("region 1's page");
    assert!(page.ckpt_seq >= 1, "the checkpoint stamped its seq");
    assert!(
        page.head_hint >= head.saturating_sub(ring_len / 4),
        "head_hint tracks the ring head ({} vs {head})",
        page.head_hint
    );
    assert_eq!(page.seq_offset, ring.seq_offset(), "the offset in force");
    let page_ring_len: u64 = page.segments.iter().map(|s| s.len).sum();

    // The release screen's derived bound admits the region's frontier…
    let derived = release_seq_floor_bound(page.seq_offset, None, page.head_hint, page_ring_len);
    assert!(
        derived >= frontier && derived < SEQ_FRONTIER_SANE_MAX,
        "the derived bound ({derived}) covers the frontier ({frontier}) and is TIGHT"
    );
    // …where the JOIN-TIME words (head_hint at the start, ckpt_seq 0 —
    // what a wire joiner's page carried before PR 12) would have refused
    // it, which is why the screen fell back to the sane cap there.
    let join_time = release_seq_floor_bound(page.seq_offset, None, start_head, page_ring_len);
    assert!(
        join_time < frontier,
        "the stale hint's bound ({join_time}) sits below the frontier ({frontier})"
    );
    let bounds = |max: u64| ReleaseWordBounds {
        seq_floor_recorded: 0,
        seq_floor_max: max,
        cursor_recorded: 0,
        cursor_max: squeezefs::meta_backend::GUEST_NS_BASE,
        root_recorded: (0, 0),
        heap_base: 0,
        node_size: u64::from(vol.superblock().node_size),
        total_extents: u64::MAX,
    };
    let words = SlotWords {
        root: (0, 0),
        cursor: 0,
        extents: 0,
        seq_floor: frontier,
    };
    let grant = ExtentGrantRecord::default();
    screen_release_words(&words, &bounds(derived), &grant)
        .expect("a > 2-lap release lands under the derived bound");
    screen_release_words(&words, &bounds(join_time), &grant)
        .expect_err("and the join-time hint would have rejected it");
    venue.tear_down();
    shutdown(&routed).await;
}

/// One writer per page: the manager rewrites a WIRE joiner's page ONLY
/// while no checkpoint of the joiner's own has written it (`ckpt_seq ==
/// 0`). Once the page carries a checkpoint seq, a grant to that appender
/// leaves its page to its owner — a manager rewrite racing the owner's
/// checkpoint on one A/B pair would land an older root and `head_hint` at
/// a newer generation, the regression the derived bound presumes away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_manager_never_rewrites_a_page_an_appenders_own_checkpoint_owns() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_stamped_member(dir.path(), "sym0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let joiner = AppenderIdentity {
        node_token: 0x5ECC_0000_0000_0012,
        mount_slot: 0x2012,
        writer_id: 0xBEEF_0012,
    };
    let joined = vol
        .manager_join_appender(joiner, 0)
        .await
        .expect("a wire join");
    let id = joined.appender_id;
    let page_of = |vol: Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>, uri: String| async move {
        read_directory(std::path::Path::new(&uri), vol.superblock())
            .await
            .expect("directory")
            .into_iter()
            .find(|e| e.appender_id == id)
            .and_then(|e| e.page)
            .expect("the joiner's page")
    };
    let fresh = page_of(Arc::clone(&vol), uris[0].clone()).await;
    assert_eq!(fresh.ckpt_seq, 0, "a join writes no checkpoint seq");

    // While the joiner has no checkpoint the manager writes its slots.
    let (grants, _) = vol
        .manager_acquire_slots_wire(id, 1)
        .await
        .expect("a wire grant");
    assert_eq!(grants.len(), 1);
    let after_first = page_of(Arc::clone(&vol), uris[0].clone()).await;
    assert!(
        after_first.generation > fresh.generation && after_first.slots.len() == 1,
        "the manager's slot-only rewrite (ckpt_seq 0)"
    );

    // The joiner's own checkpoint wrote the page (the fixture stands in
    // for it): a further grant leaves the page alone.
    rewrite_page(&uris[0], id, |p| {
        p.ckpt_seq = 7;
        p.head_hint = 4096 * 300;
    })
    .await;
    let owned = page_of(Arc::clone(&vol), uris[0].clone()).await;
    let (grants, _) = vol
        .manager_acquire_slots_wire(id, 1)
        .await
        .expect("a second wire grant");
    assert_eq!(grants.len(), 1, "the grant itself is served");
    let untouched = page_of(Arc::clone(&vol), uris[0].clone()).await;
    assert_eq!(
        untouched.generation, owned.generation,
        "the manager never rewrote a page an appender checkpoint owns"
    );
    assert_eq!(untouched.ckpt_seq, 7);
    assert_eq!(untouched.head_hint, 4096 * 300, "head_hint never regresses");
    shutdown(&routed).await;
}
