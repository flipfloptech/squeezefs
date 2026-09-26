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
//! 3. (retired at PR 14 with `volume set-owners` — ownership is a slot
//!    LEASE on every armed set; the verb is gone, `sym_default_flip_tests`
//!    pins the class law.)
//! 4. Rung 2 names the lowest missing bit of a `--single-writer` set; rung
//!    3 names the membership plane.
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
        for k in [
            "SQUEEZEFS_MULTI_WRITER",
            "SQUEEZEFS_MW_ROLE",
            "SQUEEZEFS_MW_AUTHORITY",
            "SQUEEZEFS_MW_MEMBERS",
        ] {
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
    // The posture knobs are RETIRED spellings since PR 14, on every class
    // (the registry's startup gate — `sym_default_flip_tests` pins the
    // four).
    assert!(
        !squeezefs::env_knobs::validate_vars([("SQUEEZEFS_MULTI_WRITER", "1")])
            .errors
            .is_empty(),
        "a retired knob refuses at startup whatever the volume's class"
    );
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
// 4. The rungs' refusals name themselves
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rung_2_names_the_missing_bit_and_rung_3_the_membership_plane() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    // The flat class since PR 14 is `--single-writer`: the LOWEST missing
    // bit of the multi-writer class is named, with the default `format` as
    // the act (the class is a declaration — no offline verb re-stamps it).
    let flat = vec![format_flat_member(dir.path(), "flat0").await];
    let routed = open_under(&flat, &Knobs::unarmed()).await;
    let err = squeezefs::sym_join::check_bits(&routed).expect_err("the flat class fails rung 2");
    let msg = err.to_string();
    assert!(msg.contains("rung 2 (bits)"), "names the rung: {msg}");
    let missing =
        squeezefs::sym_join::required_bits() & !routed.volumes[0].superblock().features_incompat;
    assert!(
        msg.contains(&format!("incompat bit {}", missing.trailing_zeros())),
        "names the lowest missing bit: {msg}"
    );
    assert!(
        msg.contains("default `format`") && msg.contains("--single-writer"),
        "names the act and the class: {msg}"
    );
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

/// Rung 3's knob law (ENG-10: explicit wins verbatim, never a silent
/// override): `SQUEEZEFS_MEMBERSHIP_BIND` UNSET on an armed set is the
/// ladder's `auto` — the shard's S6 owner armed by rung 3 itself — while an
/// EXPLICIT `off` is REFUSED at rung 3 naming the knob, exactly as every
/// document said and the first build did not do (`membership::resolve_bind`
/// folds unset and `off` into one word, and rung 3 read only
/// `membership_mode() == "off"`, so an operator's `off` was armed at `auto`
/// on `0.0.0.0:0` — review round 1, Issue 2). An explicit `auto` or
/// `addr:port` is the mount path's own arm and rung 3 leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rung_3_refuses_an_explicit_membership_off_and_arms_auto_only_when_unset() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    plant_secret(&routed, b"pr12-rung3-secret").await;
    assert_eq!(squeezefs::membership::membership_mode(), "off");

    // Explicit `off`: refused, naming the rung and the knob; nothing armed.
    std::env::set_var("SQUEEZEFS_MEMBERSHIP_BIND", "off");
    let err = squeezefs::sym_join::arm_membership_shard(&routed, None)
        .await
        .expect_err("an explicit off is a declaration the ladder never overrides");
    let msg = err.to_string();
    assert!(msg.contains("rung 3 (membership)"), "names the rung: {msg}");
    assert!(
        msg.contains("SQUEEZEFS_MEMBERSHIP_BIND=off"),
        "names the knob and the value: {msg}"
    );
    assert_eq!(
        squeezefs::membership::membership_mode(),
        "off",
        "a refused rung arms nothing"
    );
    // The case-insensitive spelling is the same declaration.
    std::env::set_var("SQUEEZEFS_MEMBERSHIP_BIND", " OFF ");
    squeezefs::sym_join::arm_membership_shard(&routed, None)
        .await
        .expect_err("`OFF` is `off`");
    assert_eq!(squeezefs::membership::membership_mode(), "off");

    // Unset: the ladder's `auto`.
    std::env::remove_var("SQUEEZEFS_MEMBERSHIP_BIND");
    let arm = squeezefs::sym_join::arm_membership_shard(&routed, None)
        .await
        .expect("unset ⇒ the ladder arms at auto")
        .expect("an owner arm");
    assert_eq!(arm.mode(), "owner");
    assert_eq!(squeezefs::membership::membership_mode(), "owner");
    arm.disarm().await;
    shutdown(&routed).await;
}

/// A SECOND RW open of an ARMED set through the PLAIN writer door
/// (`open_routed_meta_set` — the D0 ladder, never the mount path's join
/// target) is refused at the D0 gate — loud, at the OPEN, before any page
/// goes `Live` or any claim-set entry is written (nothing half-joins —
/// witnessed on the DEVICE directory) — and the refusal NAMES the posture
/// the class has: a second RW MOUNT joins the manager as a writer over
/// the wire (PR 12b; the mount path re-reads the join target when the D0
/// claim is refused), `-o ro` readers join as token clients, and a
/// `--single-writer` volume has one writer by format class. Since PR 14
/// the note is the CLASS's, so `SQUEEZEFS_SYMMETRIC_META=0` reads the
/// same text at Layer A (the flock is taken before any door law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_rw_open_through_the_plain_door_is_refused_at_d0_naming_the_join() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    let first = open_under(&uris, &Knobs::armed()).await;
    let pages_before = live_pages_on_device(&uris[0], &first).await;

    // The default posture: refused, naming the join; nothing half-joined
    // (`open_under` clears the knobs after its open — the second open
    // declares nothing, as a second daemon would).
    Knobs::armed().apply();
    let err = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .err()
        .map(|e| e.to_string())
        .expect("a second RW open of an armed set is refused at the D0 gate");
    assert!(
        err.contains("single-writer guard"),
        "the D0 guard's own text stands: {err}"
    );
    assert!(
        err.contains("symmetric plane") && err.contains("JOINS the manager as a WRITER"),
        "names the posture a second RW mount takes: {err}"
    );
    assert!(
        err.contains("re-reads the join target") && err.contains("-o ro"),
        "names the mount path's arm and the reader's: {err}"
    );
    assert!(
        err.contains("--single-writer"),
        "names the one class with a single writer by declaration: {err}"
    );
    assert_eq!(
        live_pages_on_device(&uris[0], &first).await,
        pages_before,
        "the refused open left no Live page ON THE DEVICE — nothing half-joined"
    );

    // `=0`: Layer A refuses first, with the same class note — the knob
    // names no posture (its own refusal is the door's, `sym_default_flip_
    // tests`), so it changes nothing here.
    std::env::set_var(SYMMETRIC_META_ENV, "0");
    let err = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .err()
        .map(|e| e.to_string())
        .expect("refused");
    std::env::remove_var(SYMMETRIC_META_ENV);
    assert!(err.contains("single-writer guard"), "{err}");
    assert!(
        err.contains("symmetric plane"),
        "the note is the class's, whatever the knob says: {err}"
    );
    shutdown(&first).await;
}

/// The appender directory's `Live` page count read off the DEVICE (the
/// page slots — `appender::read_directory`), never a backend's RAM stat: a
/// page another open wrote is visible only there (review round 2, Issue
/// 25).
async fn live_pages_on_device(uri: &str, routed: &RoutedMetaBackend) -> usize {
    read_directory(std::path::Path::new(uri), routed.volumes[0].superblock())
        .await
        .expect("directory")
        .iter()
        .filter(|e| {
            e.page.as_ref().is_some_and(|p| {
                p.state == squeezefs::meta_backend::kv::appender::AppenderState::Live
            })
        })
        .count()
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
        "every rung the ladder requires, named in the design's numbering — a checklist, not a \
         trace (rungs 5–6 ran inside the open, before 3 and before 2 / 4 / 7)"
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

/// §5.8.1 on the PRODUCT shape: the default writer — no declared test
/// partition, no knob, the shape every field mount takes since PR 14 —
/// holds WERO (rtype 3) on a PR-capable METADATA namespace, so a second
/// host's appender can REGISTER under it to write its own ring and be
/// fenced by a preempt of its key. PR 3 keyed the posture on PR 2's
/// declared partition alone, so the knob-armed manager held the shipped
/// rtype 1 (found by the fidelity tier's `sym-join-ladder` leg on the
/// real nvmet target: `meta_pr_wero=0`, device rtype 1, beside
/// `data_plane_fence_mode=1`). A `--single-writer` volume keeps rtype 1
/// verbatim (one writer by format class), and the default writer on a
/// NON-PR block device is the detection-grade lab posture under the
/// opt-in (KD-SYM-13), refused without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_writer_holds_wero_on_a_pr_capable_metadata_namespace() {
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
    // The flat class (`--single-writer`): the shipped Write Exclusive,
    // rtype 1 — there is no plane to register a peer under.
    {
        let flat = vec![format_flat_member(dir.path(), "flat0").await];
        let flat_ns = FakeNvmeNamespace::lenient_register();
        install_override(
            &flat[0],
            FakeReservationClient::new(Arc::clone(&flat_ns), "nqn.pr12.flat", "pr12-flat-host"),
        );
        let routed = open_under(&flat, &Knobs::armed()).await;
        assert!(
            routed.volumes[0].appender_stats().is_none(),
            "a single-writer volume has no appender region"
        );
        let report = FakeReservationClient::new(Arc::clone(&flat_ns), "nqn.probe", "probe")
            .report()
            .expect("report");
        assert_eq!(report.rtype, 1, "the shipped Write Exclusive");
        shutdown(&routed).await;
        clear_override(&flat[0]);
    }
    // The default (no knob, no partition): WERO, rtype 3 — the device
    // can fence a registered peer appender.
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let s = routed.volumes[0].appender_stats().expect("a forest volume");
        assert!(
            s.meta_pr_wero,
            "the default manager holds WERO on the metadata namespace: {s:?}"
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
    // The same default writer on a NON-PR BLOCK DEVICE (the fake models
    // one over the file — `reservation::single_kernel_substrate` reads an
    // installed override as the device it models): refused without the
    // opt-in, detection-grade with it (the fleet rig's loop / null_blk
    // posture under the lab opt-in).
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
            .expect("a writer on a non-PR metadata block device refuses without the opt-in");
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
    // A regular FILE with no device modelled over it is the sandbox
    // class: one kernel by construction, admitted without the opt-in and
    // holding nothing (the in-process suites' posture since PR 14).
    {
        Knobs::armed().apply();
        std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
        let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
            .await
            .expect("a file-backed metadata volume needs no opt-in");
        Knobs::clear();
        let s = routed.volumes[0].appender_stats().expect("a forest volume");
        assert!(!s.meta_pr_wero, "nothing to hold on a file");
        assert_eq!(s.manager_lease.word(), "held");
        shutdown(&routed).await;
    }
    fsck_clean(&uris).await;
}

/// The binding's WRITER half (PR 6's owed "the endpoint binding + the wire
/// initiator's lock take" — review round 1, Issue 8 (i)): rung 7 installs
/// the cross-owner step shipper under the set's cluster secret and binds
/// every Live appender's PUBLISHED endpoint into the slot holder table
/// `step_home` reads — so a create under a directory another appender
/// leases SHIPS through the ladder's own wiring, with no contract-side
/// `set_endpoint` / `install_xv_shipper`. In one process the second
/// appender is the declared region, whose page carries THIS node's
/// identity, so its published endpoint IS this writer's listener and the
/// step is served by this daemon's own S8 service (PR 12b's second daemon
/// is served by its own). The leave uninstalls the shipper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ladder_binds_every_live_appenders_published_endpoint_and_installs_the_step_shipper() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = format_stamped_set_with_config(dir.path(), 1).await;
    // Seed a directory in slot B while the slot is the manager's, release
    // it, reopen with the declared region leasing it (PR 6's fixture).
    let shared = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        plant_secret(&routed, b"pr12-binding-secret").await;
        let shared = seed_dir_in_slot(&routed, 0, SLOT_B, "shared").await;
        routed.volumes[0]
            .release_slot_handover(0, SLOT_B)
            .await
            .expect("release to unleased");
        shutdown(&routed).await;
        shared
    };
    let routed = open_under(&uris, &Knobs::armed().partition(TWO_HOLDERS)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(
        plane.holders.holder(SLOT_B).map(|h| h.appender_id),
        Some(1),
        "the declared region leases the shared directory's slot"
    );
    assert!(
        plane.holders.endpoint(1).is_none(),
        "before the ladder the holder is unbound"
    );
    let membership = squeezefs::sym_join::arm_membership_shard(&routed, None)
        .await
        .expect("rung 3 arms at auto")
        .expect("an owner arm");
    let data = dir.path().join("data-pr");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();
    let ns = FakeNvmeNamespace::new();
    install_override(
        &data,
        FakeReservationClient::new(ns.clone(), "nqn-pr12b", "host-pr12b"),
    );
    std::env::set_var("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    let (arm, report) = squeezefs::sym_join::arm(&routed, std::slice::from_ref(&data), None, None)
        .await
        .expect("the ladder walks")
        .expect("armed");

    // The binding landed off DURABLE state: the declared region's page
    // carries this node's identity, whose claim-set entry names the
    // listener rung 7 just published.
    assert_eq!(
        plane.holders.endpoint(1).as_deref(),
        Some(report.endpoint.as_str()),
        "rung 7 bound the Live appender's published endpoint"
    );
    let before = squeezefs::meta_backend::crossvol_tx::cross_owner_stats();
    let file = routed
        .create(shared, "out.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a create in a foreign directory ships through the ladder's own wiring");
    let after = squeezefs::meta_backend::crossvol_tx::cross_owner_stats();
    assert_eq!(
        after.steps_shipped,
        before.steps_shipped + 1,
        "the insert shipped to the holder's bound endpoint"
    );
    assert_eq!(
        after.steps_served,
        before.steps_served + 1,
        "and this daemon's own S8 service served it"
    );
    assert_eq!(
        after.intents_open, 0,
        "the intent retired — nothing left for the roll-forward cadence"
    );
    assert_eq!(
        routed.lookup(shared, "out.bin").await.expect("lookup").ino,
        file.ino
    );

    // The leave takes the shipper with the planes: the same create is now
    // the un-shippable class the cadence retries, never a silent local
    // apply.
    arm.disarm().await;
    squeezefs::sym_join::clear_report();
    let err = routed
        .create(shared, "after-leave", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("no shipper after the leave");
    assert!(
        err.to_string().contains("appender 1"),
        "names the holder it could not reach: {err}"
    );
    membership.disarm().await;
    clear_override(&data);
    shutdown(&routed).await;
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
    // The S5 poll stays the reader's control plane under tokens (the
    // mount path arms it in `ro_coherence`); here it is what the
    // negative-cache pin below steps.
    rv.arm_reader_revalidation(None)
        .expect("the reader's revalidation arms");
    let default = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint_a.clone(),
            secret: SECRET.to_vec(),
            client_id: "pr12-reader".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    assert_eq!(*default.endpoint(), endpoint_a);
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
    let unbound_before = squeezefs::meta_ship::token_plane::test_reader_unbound_holders();
    let resolves_before = squeezefs::meta_ship::token_plane::test_reader_holder_resolves();
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
        squeezefs::meta_ship::token_plane::test_reader_unbound_holders(),
        unbound_before + 1
    );
    assert_eq!(
        squeezefs::meta_ship::token_plane::test_reader_holder_resolves(),
        resolves_before + 1,
        "the first miss paid ONE durable resolve"
    );
    // The refusal's COST is bounded (review round 1, Issue 10): a second
    // divert on the same holder is refused off the negative cache — no
    // appender-directory read, no claim-set getxattr — while the refusal
    // gauge keeps counting refusals.
    let err = reader
        .getattr(shared)
        .await
        .expect_err("the divert refuses too");
    assert!(err.to_string().contains("appender 1"), "{err}");
    assert_eq!(
        squeezefs::meta_ship::token_plane::test_reader_unbound_holders(),
        unbound_before + 2,
        "every refusal counts"
    );
    assert_eq!(
        squeezefs::meta_ship::token_plane::test_reader_holder_resolves(),
        resolves_before + 1,
        "the second refusal paid NO device read"
    );
    // The epoch step re-reads tree 0 and is the negative cache's
    // invalidation: after the writer's next checkpoint the reader's poll
    // advances, and the next divert resolves again (the holder may have
    // published since).
    writer.volumes[0]
        .checkpoint_now()
        .await
        .expect("the writer's checkpoint");
    let out = rv.revalidate_reader().await.expect("the reader polls");
    assert!(out.advanced, "the poll adopted the writer's new epoch");
    let _ = rv
        .token_reader_for(shared_local)
        .await
        .expect_err("still unbound");
    assert_eq!(
        squeezefs::meta_ship::token_plane::test_reader_holder_resolves(),
        resolves_before + 2,
        "the epoch step cleared the negative cache — one resolve per holder per step"
    );

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
        *plane_b.endpoint(),
        endpoint_b,
        "dialed to the holder's endpoint"
    );
    assert_eq!(
        *default.endpoint(),
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

/// **PR 13 (found by the first `--token-readers` fleet — a PR 12 defect)**:
/// a `-o ro` reader's FIRST resolve of an object in a slot another appender
/// leases dials that holder's plane lazily and must SERVE — never fail
/// closed on "the recall channel to the holder is not fresh". The channel
/// task's first round is the dial's own latency, so the divert parks on it
/// (the writer's `foreign_read_plane` already did, PR 12b round 1 Issue 8;
/// the reader's `token_reader_for` returned the plane the instant it was
/// spawned). The fleet read `stat: Input/output error` on a joiner's fresh
/// directory at the reader's first touch; the contract above waited for
/// freshness by hand before its `getattr` — the defect's own shape. The
/// same law holds for the manager's plane right after the arm: the first
/// resolve after `arm_token_reader` serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readers_first_resolve_of_a_freshly_dialed_holder_serves_without_a_hand_wait() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::meta_ship::token_plane::{TokenClientConfig, TokenSetService};
    const SECRET: &[u8] = b"pr13-fresh-dial-secret";
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_stamped_member(dir.path(), "sym0").await];
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
    writer.volumes[0]
        .checkpoint_now()
        .await
        .expect("publish tree 0");
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
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    rv.arm_reader_revalidation(None)
        .expect("the reader's revalidation arms");
    let default = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: a.endpoint().to_string(),
            secret: SECRET.to_vec(),
            client_id: "pr13-reader".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    // The manager's plane: the FIRST resolve after the arm, no wait.
    reader
        .getattr(mine)
        .await
        .expect("the first resolve after the arm serves under a token from A");
    assert_eq!(default.stats().grants, 1);
    // The holder's plane: bound, then the FIRST resolve, no wait.
    rv.bind_reader_holder_endpoint(1, &b.endpoint().to_string());
    reader
        .getattr(shared)
        .await
        .expect("the first resolve of a freshly dialed holder serves under a token from B");
    let planes = rv.reader_holder_planes();
    assert_eq!(planes.len(), 1, "one per-holder plane dialed");
    assert_eq!(planes[0].stats().grants, 1, "one grant from B");
    assert_eq!(
        planes[0].stats().serve_refusals,
        0,
        "the dial's first round was awaited, never refused"
    );
    assert_eq!(default.stats().serve_refusals, 0);
    // The reader-side Token family FOLDS every plane of the volume (the
    // manager's + the per-holder ones): `dlm_token_grants` on the reader
    // ≡ its foreign first touches — gate 5's engagement law — read 1 of
    // the 2 grants above when the face read the manager's plane alone.
    let face = squeezefs::meta_ship::token_plane::reader_stats_json(&reader.volumes);
    assert_eq!(
        face["dlm_token_grants"][0].as_u64(),
        Some(2),
        "one grant from A + one from B: {face}"
    );
    assert_eq!(face["dlm_token_cached"][0].as_u64(), Some(2));
    shutdown(&reader).await;
    a.shutdown();
    b.shutdown();
    shutdown(&writer).await;
}

/// **PR 13 (found by the fleet's `sym-crash` leg — a PR 12 defect): a
/// `-o ro` token reader FOLLOWS a manager failover to the successor's
/// listener.** The manager's plane is the reader's default (appender 0's,
/// dialed at the arm on the endpoint the manager's claim-set entry named);
/// after the manager is killed and a SUCCESSOR at the same identity
/// re-walks the ladder, the successor publishes a NEW listener into that
/// same entry (an ephemeral port — `SQUEEZEFS_MW_BIND=auto`), the
/// reader's S5 poll adopts the checkpoint carrying it, and its membership
/// re-points to the successor — but the plane kept dialing the dead
/// address for ever (`token recall channel to <dead> could not connect …
/// retry in 5s`), every read `EIO` "the recall channel to the holder is
/// not fresh", `.stats` unreadable, `membership_readers` never 1 again.
/// The writer's per-holder plane already follows a moved holder
/// (`data_grant::foreign_read_plane` → `rebind_holder_endpoint_if_moved`);
/// the READER's planes did not. Now a resolve whose plane's channel is
/// dead or never freshens re-resolves the holder's endpoint off DURABLE
/// state — appender 0's page identity → its claim-set entry, read off the
/// reader's own projection, no wire — and a MOVED endpoint RE-POINTS the
/// plane in place (its identity, its gauges, its data sink and its R5
/// registration kept; every cached token dropped — a holder that moved
/// may have re-granted; the channel re-dials the successor at once):
/// the next resolve serves, `dlm_token_holder_repoints` = 1, the plane
/// `Arc::ptr_eq` the one armed. An unmoved endpoint keeps the shipped
/// fail-closed window verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reader_follows_a_manager_failover_to_the_successors_listener() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::meta_ship::token_plane::{TokenClientConfig, TokenSetService};
    const SECRET: &[u8] = b"pr13-reader-failover-secret";
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempfile::tempdir().unwrap();
    let uris = vec![format_stamped_member(dir.path(), "sym0").await];
    let listener = |vols: &[Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>]| {
        cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            TokenSetService::new(vols),
        )
        .expect("token listener")
    };
    // The MANAGER: its token service on listener A, A published into its
    // claim-set entry (the ladder's rung 7 on a real mount).
    let manager = open_under(&uris, &Knobs::armed()).await;
    let mine = manager
        .create(ROOT, "mine", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("an own file")
        .ino;
    let a = listener(&manager.volumes);
    let endpoint_a = a.endpoint().to_string();
    // The ladder's rungs 3 + 7 on a real mount: this node's claim-set
    // entry carrying its listener (`enroll_manager`'s shape in the
    // N-daemon suite), checkpointed so a reader's poll sees it.
    let enroll = |vol: Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>,
                  endpoint: String| async move {
        use squeezefs::membership::{MemberIdentity, MemberRole};
        let identity = MemberIdentity {
            id: squeezefs::cowriter::node_member_id().expect("this node's member id"),
            role: MemberRole::Writer,
            pid: std::process::id(),
            boot: squeezefs::meta_backend::kv::backend::read_boot_id(),
            endpoint: Some(endpoint),
            pr_key: 0,
        };
        squeezefs::membership::upsert_writer_member(
            &vol,
            &identity,
            squeezefs::dlm::durable_term(),
        )
        .await
        .expect("the claim-set entry");
        vol.checkpoint_now().await.expect("checkpoint");
    };
    enroll(Arc::clone(&manager.volumes[0]), endpoint_a.clone()).await;

    // The READER: armed at the endpoint the entry names (the mount path's
    // `arm_token_readers` resolves exactly this), served under a token.
    let reader = squeezefs::meta_backend::open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only open");
    let rv = Arc::clone(&reader.volumes[0]);
    rv.arm_reader_revalidation(None)
        .expect("the reader's revalidation arms");
    assert_eq!(
        squeezefs::sym_join::resolve_holder_endpoint(&rv, 0).await,
        Some(endpoint_a.clone()),
        "the manager's entry names A"
    );
    let default = rv
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint_a.clone(),
            secret: SECRET.to_vec(),
            client_id: "pr13-failover-reader".to_string(),
            volume: 0,
        })
        .expect("the manager's plane arms");
    reader
        .getattr(mine)
        .await
        .expect("served under a token from the manager");
    let grants_before = default.stats().grants;
    assert_eq!(grants_before, 1);
    assert_eq!(default.stats().holder_repoints, 0);

    // THE FAILOVER: the manager dies (its listener with it); a successor
    // at the same identity wins the ladder and publishes listener B.
    a.shutdown();
    shutdown(&manager).await;
    drop(manager);
    let successor = open_under(&uris, &Knobs::armed()).await;
    let b = listener(&successor.volumes);
    let endpoint_b = b.endpoint().to_string();
    assert_ne!(endpoint_b, endpoint_a, "a new listener address");
    enroll(Arc::clone(&successor.volumes[0]), endpoint_b.clone()).await;
    // The reader's poll adopts it (the mount path's cadence).
    let out = rv.revalidate_reader().await.expect("the reader polls");
    assert!(out.advanced, "the poll adopted the successor's epoch");
    assert_eq!(
        squeezefs::sym_join::resolve_holder_endpoint(&rv, 0).await,
        Some(endpoint_b.clone()),
        "durable state names the successor's listener"
    );

    // The read after the failover SERVES — the plane followed the entry.
    let attrs = reader
        .getattr(mine)
        .await
        .expect("served under a token from the SUCCESSOR (the plane re-pointed)");
    assert_eq!(attrs.mode & 0o777, 0o644);
    assert_eq!(
        *default.endpoint(),
        endpoint_b,
        "the manager's plane dials the successor"
    );
    assert!(
        Arc::ptr_eq(rv.token_reader().expect("still armed"), &default),
        "the plane is re-pointed IN PLACE — its identity and gauges continue"
    );
    let s = default.stats();
    assert_eq!(s.holder_repoints, 1, "one re-point: {s:?}");
    assert_eq!(
        s.grants,
        grants_before + 1,
        "the gauge continued across the re-point (the post-failover grant): {s:?}"
    );
    assert!(
        s.channel_fresh,
        "the channel to the successor completed a round"
    );
    assert_eq!(
        rv.reader_holder_endpoint(0).as_deref(),
        Some(endpoint_b.as_str()),
        "holder 0's binding moved with it"
    );
    // A second read pays no second re-point and serves from the cache.
    reader.getattr(mine).await.expect("served");
    assert_eq!(default.stats().holder_repoints, 1);
    let face = squeezefs::meta_ship::token_plane::reader_stats_json(&reader.volumes);
    assert_eq!(
        face["dlm_token_holder_repoints"][0].as_u64(),
        Some(1),
        "the face carries the re-point: {face}"
    );

    // **The reader's CONTROL-class xattr listing reads its own projection**
    // (the same leg's next gate): the fleet worker re-discovers the
    // successor's coordinator through `mount_registrations` — `listxattr(1)`
    // + `getxattr` of the `client:` records — and on a token reader the
    // divert served the token's CARRIED names alone, so the reader saw no
    // registration at all ("no coordinator endpoint published yet" for
    // ever; its census shard never re-enrolled). The successor's
    // registration carries its job endpoint; the reader lists it.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    successor.volumes[0]
        .setxattr_internal(
            ROOT,
            "client:pr13-successor-client",
            format!(
                "{{\"ts\":{ts},\"pid\":{},\"job_endpoint\":\"10.0.0.9:4242\"}}",
                std::process::id()
            )
            .as_bytes(),
        )
        .await
        .expect("the successor's registration (the mount heartbeat's shape)");
    successor.volumes[0]
        .checkpoint_now()
        .await
        .expect("the successor's checkpoint");
    let out = rv.revalidate_reader().await.expect("the reader polls");
    assert!(out.advanced);
    let names = rv.listxattr(ROOT).await.expect("the reader lists ino 1");
    assert!(
        names.iter().any(|n| n == "client:pr13-successor-client"),
        "a token reader lists the CONTROL names off its projection: {names:?}"
    );
    let regs = rv.mount_registrations().await;
    assert!(
        regs.iter().any(|r| r.id == "pr13-successor-client"
            && r.job_endpoint.as_deref() == Some("10.0.0.9:4242")),
        "the registration is discoverable on the reader: {regs:?}"
    );
    assert_eq!(
        squeezefs::cluster_wire::discover_endpoint(&reader)
            .await
            .as_deref(),
        Some("10.0.0.9:4242"),
        "the fleet worker's discovery finds the successor's coordinator"
    );

    shutdown(&reader).await;
    b.shutdown();
    shutdown(&successor).await;
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
    // The ring's PHYSICAL size at the join — the word a wire joiner's page
    // carried before PR 12 (the stale-hint arm below). Under the sector-
    // pad law (PR 13i) the storm fills the region's ring fast enough for
    // PR 2's stall-driven growth to REPLACE it mid-storm, so the join-time
    // arithmetic reads the join-time length, never the grown page's.
    let start_ring_bytes = ring.ring_bytes();

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
    // A growth mid-storm replaced the region's ring: read the LIVE one.
    let ring = region.ring();
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
    let join_time = release_seq_floor_bound(page.seq_offset, None, start_head, start_ring_bytes);
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
