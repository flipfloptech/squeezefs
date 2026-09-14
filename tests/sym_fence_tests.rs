//! Symmetric metadata program, PR 3 — **the fencing family**
//! (`docs/design-symmetric-metadata.md` §5.8.1, §5.8.2, §6.1, §11
//! "Fencing family"; KD-SYM-13, KD-SYM-18, KD-SYM-23).
//!
//! Contracts pinned:
//! - the Reservation Report is read SIZED BY `REGCTL` — a report with more
//!   than 63 extended registrants decodes completely (the shipped fixed
//!   4 KiB buffer truncated at the 63rd, so S7's rung-5 check mis-read the
//!   64th co-writer as unregistered — the repro-port of that finding);
//! - a declared registrant cap (`SQUEEZEFS_PR_REGISTRANT_CAP`) refuses
//!   the next join LOUD, naming the count, the namespace and the remedy;
//!   `pr_registrant_cap_refusals` counts it;
//! - KD-SYM-13: the symmetric arm refuses to arm on a non-PR substrate
//!   unless `SQUEEZEFS_SYM_ALLOW_NON_PR=1`, announced loudly;
//!   `SQUEEZEFS_META_PR_WERO=0` is refused on a PR-capable substrate
//!   without the same opt-in;
//! - the manager holds WERO (rtype 3) on the metadata namespace and
//!   `manager_lease` reads `held`; a flat mount's reservation posture is
//!   byte-for-byte today's (Write Exclusive, rtype 1).

use squeezefs::meta_backend::kv::appender::{
    appender0_page_offsets, write_page, AppenderIdentity, AppenderPage, AppenderState,
    ManagerLease, TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
use squeezefs::meta_backend::reservation::{
    install_override, parse_reservation_report, report_len_for, FakeNvmeNamespace,
    FakeReservationClient, ReservationClient, PR_REGISTRANT_CAP_ENV,
};
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set};
use std::sync::Arc;

/// Knob reads are process-global; the contracts that set one serialize.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const VOL_LEN: u64 = 64 * 1024 * 1024;
const PARTITION: &str = "1:4";

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format one member at `dir/name`, stamped (bit 17) or flat; the ENV
/// guard is the caller's.
async fn format_member(dir: &std::path::Path, name: &str, stamped: bool) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    if stamped {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    } else {
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    }
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format member");
    p.display().to_string()
}

/// The knobs the arm reads, set for one open and cleared after.
struct ArmEnv;
impl ArmEnv {
    fn set(partition: Option<&str>, allow_non_pr: Option<&str>, wero: Option<&str>) -> Self {
        match partition {
            Some(p) => std::env::set_var(TEST_APPENDER_SLOTS_ENV, p),
            None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
        }
        match allow_non_pr {
            Some(v) => std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", v),
            None => std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR"),
        }
        match wero {
            Some(v) => std::env::set_var("SQUEEZEFS_META_PR_WERO", v),
            None => std::env::remove_var("SQUEEZEFS_META_PR_WERO"),
        }
        Self
    }
}
impl Drop for ArmEnv {
    fn drop(&mut self) {
        std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
        std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
        std::env::remove_var("SQUEEZEFS_META_PR_WERO");
    }
}

// ---------------------------------------------------------------------------
// KD-SYM-13 — the arm refuses a non-PR substrate without the loud opt-in.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_symmetric_arm_refuses_a_non_pr_substrate_without_the_loud_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let _g = ENV.lock().await;
    let uris = vec![format_member(dir.path(), "meta0", true).await];
    // A file-backed volume advertises no reservation support: the arm
    // (a declared partition) refuses to arm, naming the knob.
    let err = {
        let _e = ArmEnv::set(Some(PARTITION), None, None);
        match open_routed_meta_set(&uris).await {
            Ok(_) => panic!("the symmetric arm mounted on a non-PR substrate without the opt-in"),
            Err(e) => e.to_string(),
        }
    };
    assert!(
        err.contains("SQUEEZEFS_SYM_ALLOW_NON_PR") && err.contains("KD-SYM-13"),
        "names the opt-in and the rule: {err}"
    );
    assert!(
        err.contains("detection-grade"),
        "says what the substrate gives — detection-grade, not loss-free: {err}"
    );
    // The solo stamped mount (no partition, no join) is NOT the arm: it
    // mounts on any substrate exactly as PR 1/2 left it.
    {
        let _e = ArmEnv::set(None, None, None);
        let routed = open_routed_meta_set(&uris)
            .await
            .expect("a solo forest mount is unarmed");
        let s = routed.volumes[0].appender_stats().unwrap();
        assert_eq!(s.manager_lease, ManagerLease::Held);
        assert!(
            !s.meta_pr_wero,
            "no reservation at all on a file-backed volume"
        );
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
    // The loud opt-in arms it.
    {
        let _e = ArmEnv::set(Some(PARTITION), Some("1"), None);
        let routed = open_routed_meta_set(&uris)
            .await
            .expect("the opt-in arms on non-PR");
        let s = routed.volumes[0].appender_stats().unwrap();
        assert_eq!(s.live, 2);
        assert!(!s.meta_pr_wero);
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// §5.8.1 — WERO on the metadata namespace; the flat posture untouched.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manager_holds_wero_on_the_metadata_namespace_and_a_flat_mount_keeps_write_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let _g = ENV.lock().await;
    let stamped = format_member(dir.path(), "meta0", true).await;
    let flat = format_member(dir.path(), "flat0", false).await;
    // A PR-capable "device" under each volume.
    let ns_stamped = FakeNvmeNamespace::lenient_register();
    let ns_flat = FakeNvmeNamespace::lenient_register();
    install_override(
        &stamped,
        FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.mgr", "mgr-host"),
    );
    install_override(
        &flat,
        FakeReservationClient::new(Arc::clone(&ns_flat), "nqn.flat", "flat-host"),
    );
    // The armed forest mount: WERO (rtype 3) held by the manager.
    {
        let _e = ArmEnv::set(Some(PARTITION), None, None);
        let routed = open_routed_meta_set(std::slice::from_ref(&stamped))
            .await
            .expect("a PR-capable substrate arms without the opt-in");
        let vol = &routed.volumes[0];
        let s = vol.appender_stats().unwrap();
        assert!(
            s.meta_pr_wero,
            "the manager holds WERO on the metadata namespace: {s:?}"
        );
        assert_eq!(s.manager_lease, ManagerLease::Held);
        assert_eq!(vol.writer_guard_mode(), "flock+pr");
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.rtype, 3, "Write Exclusive – Registrants Only");
        assert!(report.is_wero());
        // A second appender REGISTERS under the manager's standing hold —
        // the S9 registrant law one namespace class over — and can be
        // fenced by preempting its key; the manager's hold stands.
        let peer_path = dir.path().join("peer-view-of-meta0");
        std::fs::File::create(&peer_path).unwrap();
        install_override(
            &peer_path,
            FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.peer", "peer-host"),
        );
        let join = squeezefs_ipc::sqz_blocking::run_blocking({
            let p = peer_path.clone();
            move || squeezefs::data_custody::join_wero_as_registrant(&[p])
        })
        .await
        .expect("a registrant joins the manager's WERO");
        assert!(join.evidence().wero && join.evidence().registered);
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.regctl(), 2, "manager + one registrant");
        assert_eq!(report.holder_key, Some(vol.writer_guard_pr_key()));
        drop(join);
        squeezefs::meta_backend::reservation::clear_override(&peer_path);
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
        // A clean unmount releases the WERO hold and its registration.
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.holder_key, None);
        assert_eq!(report.regctl(), 0);
    }
    // `SQUEEZEFS_META_PR_WERO=0` on a PR-capable substrate: refused
    // without the opt-in (detection-grade is not loss-free); with it,
    // the shipped Write Exclusive.
    {
        let _e = ArmEnv::set(Some(PARTITION), None, Some("0"));
        let err = match open_routed_meta_set(std::slice::from_ref(&stamped)).await {
            Ok(_) => panic!("WERO=0 on a PR-capable substrate must refuse without the opt-in"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("SQUEEZEFS_META_PR_WERO") && err.contains("SQUEEZEFS_SYM_ALLOW_NON_PR")
        );
    }
    {
        let _e = ArmEnv::set(Some(PARTITION), Some("1"), Some("0"));
        let routed = open_routed_meta_set(std::slice::from_ref(&stamped))
            .await
            .expect("opt-in");
        assert!(!routed.volumes[0].appender_stats().unwrap().meta_pr_wero);
        assert!(ns_stamped.holder().is_some());
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.rtype, 1, "the shipped Write Exclusive under WERO=0");
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
    // The flat mount's reservation posture is byte-for-byte today's:
    // Write Exclusive (rtype 1), whatever the knobs say.
    {
        let _e = ArmEnv::set(Some(PARTITION), None, None);
        let routed = open_routed_meta_set(std::slice::from_ref(&flat))
            .await
            .expect("flat mounts");
        assert!(routed.volumes[0].appender_stats().is_none());
        assert_eq!(routed.volumes[0].writer_guard_mode(), "flock+pr");
        let report = FakeReservationClient::new(Arc::clone(&ns_flat), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(
            report.rtype, 1,
            "a bit-17-absent mount takes rtype 1 exactly as shipped"
        );
        assert!(!report.is_wero());
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
    // The solo stamped mount — unarmed — keeps rtype 1 too.
    {
        let _e = ArmEnv::set(None, None, None);
        let routed = open_routed_meta_set(std::slice::from_ref(&stamped))
            .await
            .expect("solo");
        assert!(!routed.volumes[0].appender_stats().unwrap().meta_pr_wero);
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.rtype, 1);
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
    squeezefs::meta_backend::reservation::clear_override(&stamped);
    squeezefs::meta_backend::reservation::clear_override(&flat);
}

/// The posture word: `held` on the joined manager, `vacant` off a Free
/// page 0, `peer:<node>` where another node's page 0 is Live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_lease_reads_held_vacant_and_peer() {
    let dir = tempfile::tempdir().unwrap();
    let _g = ENV.lock().await;
    let uri = format_member(dir.path(), "meta0", true).await;
    let path = std::path::Path::new(&uri);
    let probe = KvMetaBackend::open_probe(path).await.unwrap();
    assert_eq!(
        probe.appender_stats().unwrap().manager_lease,
        ManagerLease::Vacant,
        "a fresh volume's page 0 is Free"
    );
    drop(probe);
    let sb = match classify_volume(path).await.unwrap() {
        VolumeFormat::V3(sb) => sb,
        other => panic!("{other:?}"),
    };
    let offs = appender0_page_offsets(&sb.journal);
    let mut page = AppenderPage::free(0, 0);
    page.generation = 1000;
    page.state = AppenderState::Live;
    page.is_manager = true;
    page.term = 3;
    page.identity = AppenderIdentity {
        node_token: 0xF0E1_D2C3_B4A5_9687,
        mount_slot: 1,
        writer_id: 42,
    };
    write_page(path, offs[0], page.encode().unwrap())
        .await
        .unwrap();
    let probe = KvMetaBackend::open_probe(path).await.unwrap();
    assert_eq!(
        probe.appender_stats().unwrap().manager_lease,
        ManagerLease::Peer {
            node_token: 0xF0E1_D2C3_B4A5_9687
        }
    );
    assert_eq!(
        probe.appender_stats().unwrap().manager_lease.word(),
        "peer:0xf0e1d2c3b4a59687"
    );
    drop(probe);
    // The successor: no claim exists, so the D0 ladder is won outright —
    // the foreign Live page 0 is adopted with the role (§5.9).
    let _e = ArmEnv::set(None, None, None);
    let routed = open_routed_meta_set(std::slice::from_ref(&uri))
        .await
        .expect("successor");
    let s = routed.volumes[0].appender_stats().unwrap();
    assert_eq!(s.manager_lease, ManagerLease::Held);
    assert_eq!(s.manager_lease.word(), "held");
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// One extended (EDS = 1, 128-bit host id) Reservation Report image with
/// `regctl` registrants, the NVMe Base Spec layout: a 64 B header
/// (`gen u32 ‖ rtype u8 ‖ regctl u16 ‖ … ‖ ptpls`) followed by 64 B
/// registered-controller-extended structures (`cntlid u16 ‖ rcsts u8 ‖
/// rsvd ‖ rkey u64 @8 ‖ hostid[16] @16`).
fn extended_report_image(regctl: u16, holder: Option<u16>) -> Vec<u8> {
    let mut img = vec![0u8; report_len_for(regctl, true)];
    img[0..4].copy_from_slice(&7u32.to_le_bytes());
    img[4] = if holder.is_some() { 3 } else { 0 };
    img[5..7].copy_from_slice(&regctl.to_le_bytes());
    for i in 0..regctl {
        let base = 64 + usize::from(i) * 64;
        img[base..base + 2].copy_from_slice(&i.to_le_bytes());
        img[base + 2] = u8::from(holder == Some(i));
        img[base + 8..base + 16].copy_from_slice(&(0x1000 + u64::from(i)).to_le_bytes());
        let mut hostid = [0u8; 16];
        hostid[..2].copy_from_slice(&i.to_le_bytes());
        hostid[15] = 0xA5;
        img[base + 16..base + 32].copy_from_slice(&hostid);
    }
    img
}

#[test]
fn a_reservation_report_with_more_than_sixty_three_registrants_decodes_completely() {
    // The extended form: 64 B header + 64 B per registrant.
    assert_eq!(report_len_for(0, true), 64);
    assert_eq!(report_len_for(70, true), 64 + 64 * 70);
    assert_eq!(report_len_for(70, false), 24 + 24 * 70);
    let img = extended_report_image(70, Some(69));
    let report = parse_reservation_report(&img, true).expect("a well-formed report parses");
    assert_eq!(
        report.registrants.len(),
        70,
        "every registrant the header's REGCTL names is decoded"
    );
    assert_eq!(report.rtype, 3);
    assert_eq!(
        report.holder_key,
        Some(0x1000 + 69),
        "the 70th registrant — beyond the 63rd — is reported as the holder"
    );
    assert!(
        report.registered(0x1000 + 63),
        "the 64th registrant is REPORTED"
    );
    // The shipped truncation, stated: a fixed 4 KiB buffer holds the
    // header plus 63 extended structures, so the 64th and every later
    // registrant fell off the parse.
    let truncated = parse_reservation_report(&img[..4096], true).expect("parses what fits");
    assert_eq!(
        truncated.registrants.len(),
        63,
        "(4096 − 64) / 64 = 63 — the fixed buffer's ceiling"
    );
    assert!(!truncated.registered(0x1000 + 63));
}

#[test]
fn the_report_parse_is_total_over_short_and_malformed_images() {
    // Shorter than a header: refused, never a panic.
    assert!(parse_reservation_report(&[0u8; 8], true).is_err());
    assert!(parse_reservation_report(&[0u8; 8], false).is_err());
    // A header naming more registrants than the image holds parses the
    // ones that fit (the two-step read re-reads at the new size).
    let mut img = extended_report_image(3, None);
    img.truncate(64 + 64 * 2 + 7);
    let r = parse_reservation_report(&img, true).unwrap();
    assert_eq!(r.registrants.len(), 2);
    assert_eq!(r.holder_key, None);
    // The short form (64-bit host ids): 24 B header + 24 B structures
    // with hostid @8 and rkey @16.
    let mut short = vec![0u8; report_len_for(2, false)];
    short[4] = 1;
    short[5..7].copy_from_slice(&2u16.to_le_bytes());
    for i in 0..2usize {
        let base = 24 + i * 24;
        short[base + 2] = u8::from(i == 1);
        short[base + 8..base + 16].copy_from_slice(&(0xB0B0 + i as u64).to_le_bytes());
        short[base + 16..base + 24].copy_from_slice(&(0xCAFE + i as u64).to_le_bytes());
    }
    let r = parse_reservation_report(&short, false).unwrap();
    assert_eq!(r.registrants.len(), 2);
    assert_eq!(r.holder_key, Some(0xCAFF));
    assert_eq!(r.registrants[0].host_id, 0xB0B0u64.to_le_bytes().to_vec());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_registrant_cap_refuses_the_next_join_loud() {
    use squeezefs::data_custody::join_wero_as_registrant;
    use squeezefs::meta_backend::reservation::pr_registrant_cap_refusals;
    let _g = ENV.lock().await;
    let ns = FakeNvmeNamespace::lenient_register();
    // An authority holds WERO; seven more hosts are registered under it —
    // eight registrants on the namespace.
    let authority = FakeReservationClient::new(Arc::clone(&ns), "nqn.auth", "auth-host");
    authority.register(0xA0).unwrap();
    authority
        .acquire_write_exclusive_registrants_only(0xA0)
        .unwrap();
    for i in 1..8u64 {
        let c = FakeReservationClient::new(Arc::clone(&ns), "nqn.peer", &format!("peer-{i}"));
        c.register(0xA0 + i).unwrap();
    }
    assert_eq!(authority.report().unwrap().registrants.len(), 8);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data0");
    std::fs::File::create(&path).unwrap();
    let joiner = FakeReservationClient::new(Arc::clone(&ns), "nqn.joiner", "joiner-host");
    squeezefs::meta_backend::reservation::install_override(&path, joiner);
    let before = pr_registrant_cap_refusals();
    std::env::set_var(PR_REGISTRANT_CAP_ENV, "8");
    let r = squeezefs_ipc::sqz_blocking::run_blocking({
        let p = path.clone();
        move || join_wero_as_registrant(&[p])
    })
    .await;
    std::env::remove_var(PR_REGISTRANT_CAP_ENV);
    squeezefs::meta_backend::reservation::clear_override(&path);
    let err = match r {
        Ok(_) => panic!("the 9th registrant joined past a declared cap of 8"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("8 registrant"), "names the count: {err}");
    assert!(err.contains("data0"), "names the namespace: {err}");
    assert!(
        err.contains("nvmet") && err.contains("fewer hosts"),
        "names the remedy — nvmet in front of the array, or fewer hosts per namespace: {err}"
    );
    assert_eq!(pr_registrant_cap_refusals(), before + 1);
    // Nothing was registered: the device is exactly as it was found.
    assert_eq!(ns.holder(), Some(0xA0));
    assert_eq!(authority.report().unwrap().registrants.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_the_declared_cap_the_join_lands_and_the_gauges_read_the_namespace() {
    use squeezefs::data_custody::join_wero_as_registrant;
    use squeezefs::meta_backend::reservation::{pr_registrant_cap, pr_report_gauges};
    let _g = ENV.lock().await;
    let ns = FakeNvmeNamespace::lenient_register();
    let authority = FakeReservationClient::new(Arc::clone(&ns), "nqn.auth", "auth-host");
    authority.register(0xA0).unwrap();
    authority
        .acquire_write_exclusive_registrants_only(0xA0)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data1");
    std::fs::File::create(&path).unwrap();
    let joiner = FakeReservationClient::new(Arc::clone(&ns), "nqn.joiner", "joiner-host");
    squeezefs::meta_backend::reservation::install_override(&path, joiner);
    std::env::set_var(PR_REGISTRANT_CAP_ENV, "8");
    assert_eq!(pr_registrant_cap(), 8, "declared wins");
    let r = squeezefs_ipc::sqz_blocking::run_blocking({
        let p = path.clone();
        move || join_wero_as_registrant(&[p])
    })
    .await;
    std::env::remove_var(PR_REGISTRANT_CAP_ENV);
    squeezefs::meta_backend::reservation::clear_override(&path);
    let join = r.expect("the 2nd registrant is under the cap");
    assert!(join.evidence().registered);
    assert_eq!(
        pr_registrant_cap(),
        0,
        "unset = unbounded (the nvmet posture): published as 0"
    );
    // The per-namespace gauges: the last report's REGCTL and the bytes
    // the REGCTL-sized read transferred.
    let gauges = pr_report_gauges();
    let (regctl, bytes) = gauges
        .get(&path.display().to_string())
        .copied()
        .expect("the joined namespace has a row");
    assert_eq!(regctl, 2);
    assert_eq!(bytes, report_len_for(2, true) as u64);
    drop(join);
}
