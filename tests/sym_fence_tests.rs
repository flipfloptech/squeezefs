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
//! - KD-SYM-13: the symmetric plane refuses to arm on a non-PR BLOCK
//!   DEVICE unless `SQUEEZEFS_SYM_ALLOW_NON_PR=1`, announced loudly (a
//!   regular file with no device modelled over it is one kernel by
//!   construction and needs no opt-in — PR 14's sandbox arm);
//!   `SQUEEZEFS_META_PR_WERO=0` is refused on a PR-capable substrate
//!   without the same opt-in;
//! - the manager holds WERO (rtype 3) on the metadata namespace and
//!   `manager_lease` reads `held` — the solo default writer included
//!   (PR 14); a `--single-writer` mount's reservation posture is
//!   byte-for-byte the shipped one (Write Exclusive, rtype 1).

use squeezefs::meta_backend::kv::appender::{
    appender0_page_offsets, write_page, AppenderIdentity, AppenderPage, AppenderState,
    ManagerLease, TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{
    format_v3_stamped_single_writer, format_v3_stamped_symmetric, FormatV3Options,
};
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
    // The forest EXPLICITLY (the default class since PR 14) or the flat
    // `--single-writer` class — the builder is told, never the environment.
    let r = if stamped {
        format_v3_stamped_symmetric(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await
    } else {
        format_v3_stamped_single_writer(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await
    };
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
    let meta = std::path::PathBuf::from(&uris[0]);
    // A BLOCK DEVICE advertising no reservation support (the fake models
    // one over the file — `reservation::single_kernel_substrate` reads an
    // installed override as the device it models): the armed plane —
    // every writer's since PR 14, solo or partitioned — refuses to arm,
    // naming the knob.
    let ns = FakeNvmeNamespace::without_pr_support();
    install_override(
        &meta,
        FakeReservationClient::new(Arc::clone(&ns), "nqn.nonpr", "nonpr-host"),
    );
    for partition in [Some(PARTITION), None] {
        let err = {
            let _e = ArmEnv::set(partition, None, None);
            match open_routed_meta_set(&uris).await {
                Ok(_) => {
                    panic!(
                        "the symmetric plane mounted on a non-PR block device without the opt-in"
                    )
                }
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
    }
    // The loud opt-in arms it, detection-grade.
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
    squeezefs::meta_backend::reservation::clear_override(&meta);
    // A regular FILE with no device over it is one kernel by construction
    // (the sandbox class): the solo default mount arms and holds nothing,
    // no opt-in asked.
    {
        let _e = ArmEnv::set(None, None, None);
        let routed = open_routed_meta_set(&uris)
            .await
            .expect("a file-backed forest volume needs no opt-in");
        let s = routed.volumes[0].appender_stats().unwrap();
        assert_eq!(s.manager_lease, ManagerLease::Held);
        assert!(
            !s.meta_pr_wero,
            "no reservation at all on a file-backed volume"
        );
        assert!(
            routed.volumes[0].slot_lease_armed(),
            "and the plane IS armed"
        );
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
    // The solo stamped mount is the SAME armed writer since PR 14 (no
    // partition, no knob): WERO too — the second host's appender can
    // register under it.
    {
        let _e = ArmEnv::set(None, None, None);
        let routed = open_routed_meta_set(std::slice::from_ref(&stamped))
            .await
            .expect("solo");
        assert!(routed.volumes[0].appender_stats().unwrap().meta_pr_wero);
        let report = FakeReservationClient::new(Arc::clone(&ns_stamped), "nqn.probe", "probe")
            .report()
            .unwrap();
        assert_eq!(report.rtype, 3, "the solo default writer holds WERO");
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

/// What the adversary's REGISTER answers.
#[derive(Debug, Clone, Copy)]
enum RegisterAnswer {
    Ok,
    Errno(i32),
    Status(u16),
}

/// A `ReservationClient` whose REGISTER answers a configured error while
/// every other verb is the fake namespace's — the learn arm's adversary.
#[derive(Debug)]
struct RegisterAnswers {
    inner: Arc<FakeReservationClient>,
    answer: std::sync::Mutex<RegisterAnswer>,
}

impl RegisterAnswers {
    fn set(&self, a: RegisterAnswer) {
        *self.answer.lock().unwrap() = a;
    }
}

impl ReservationClient for RegisterAnswers {
    fn rescap(&self) -> std::io::Result<u8> {
        self.inner.rescap()
    }
    fn host_identity(&self) -> std::io::Result<squeezefs::meta_backend::reservation::HostIdentity> {
        self.inner.host_identity()
    }
    fn wire_host_id(&self) -> std::io::Result<Vec<u8>> {
        self.inner.wire_host_id()
    }
    fn register(&self, key: u64) -> std::io::Result<()> {
        match *self.answer.lock().unwrap() {
            RegisterAnswer::Ok => self.inner.register(key),
            RegisterAnswer::Errno(code) => Err(std::io::Error::from_raw_os_error(code)),
            RegisterAnswer::Status(status) => Err(
                squeezefs::meta_backend::reservation::nvme_status_error(0x0d, status),
            ),
        }
    }
    fn unregister(&self, key: u64) -> std::io::Result<()> {
        self.inner.unregister(key)
    }
    fn acquire_write_exclusive(&self, key: u64) -> std::io::Result<()> {
        self.inner.acquire_write_exclusive(key)
    }
    fn acquire_write_exclusive_registrants_only(&self, key: u64) -> std::io::Result<()> {
        self.inner.acquire_write_exclusive_registrants_only(key)
    }
    fn preempt(&self, key: u64, victim: u64) -> std::io::Result<()> {
        self.inner.preempt(key, victim)
    }
    fn preempt_registrants_only(&self, key: u64, victim: u64) -> std::io::Result<()> {
        self.inner.preempt_registrants_only(key, victim)
    }
    fn preempt_and_abort_registrants_only(&self, key: u64, victim: u64) -> std::io::Result<()> {
        self.inner.preempt_and_abort_registrants_only(key, victim)
    }
    fn release(&self, key: u64) -> std::io::Result<()> {
        self.inner.release(key)
    }
    fn release_registrants_only(&self, key: u64) -> std::io::Result<()> {
        self.inner.release_registrants_only(key)
    }
    fn report(&self) -> std::io::Result<squeezefs::meta_backend::reservation::ReservationReport> {
        self.inner.report()
    }
    fn report_bytes(&self) -> u64 {
        self.inner.report_bytes()
    }
}

/// Review round 1, Issue 5: the registrant cap is learned ONLY from a
/// device-answered NVMe STATUS refusal of REGISTER that is neither the
/// reservation conflict nor a transient class (`Namespace Not Ready`,
/// `Command Interrupted`, `Transient Transport`, the abort classes, the
/// path classes), and only once the SAME refusal REPEATS at the same
/// registrant count; a transport errno — `EIO`, `ENXIO`, `ENOTTY`,
/// `EAGAIN`, `ETIMEDOUT`, `EBUSY`, `ENODEV`, `ECONNRESET` … — never
/// learns; a later REGISTER that SUCCEEDS at or past the learned count
/// unlearns it (`pr_registrant_cap` falls back to unbounded).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cap_is_learned_only_from_a_repeated_status_refusal_and_unlearned_on_success() {
    use squeezefs::meta_backend::reservation::{
        clear_learned_registrant_cap, pr_registrant_cap, register_ladder,
        NVME_SC_NAMESPACE_NOT_READY, NVME_SC_TRANSIENT_TRANSPORT,
    };
    let _g = ENV.lock().await;
    std::env::remove_var(PR_REGISTRANT_CAP_ENV);
    clear_learned_registrant_cap();
    let ns = FakeNvmeNamespace::lenient_register();
    // Three registrants stand on the namespace.
    for i in 0..3u64 {
        FakeReservationClient::new(Arc::clone(&ns), "nqn.peer", &format!("peer-{i}"))
            .register(0xC0 + i)
            .unwrap();
    }
    let client = RegisterAnswers {
        inner: FakeReservationClient::new(Arc::clone(&ns), "nqn.learn", "learn-host"),
        answer: std::sync::Mutex::new(RegisterAnswer::Ok),
    };
    // Every transport errno class: refused, nothing learned.
    for errno in [
        libc::EIO,
        libc::ENXIO,
        libc::ENOTTY,
        libc::EAGAIN,
        libc::ETIMEDOUT,
        libc::EBUSY,
        libc::ENODEV,
        libc::ECONNRESET,
        libc::EINTR,
        libc::ENOENT,
    ] {
        client.set(RegisterAnswer::Errno(errno));
        register_ladder(&client, 0xD0).expect_err("register fails");
        register_ladder(&client, 0xD0).expect_err("register fails again");
        assert_eq!(
            pr_registrant_cap(),
            0,
            "errno {errno} must never learn a cap"
        );
    }
    // The transient NVMe status classes: never learn either.
    for status in [NVME_SC_NAMESPACE_NOT_READY, NVME_SC_TRANSIENT_TRANSPORT] {
        client.set(RegisterAnswer::Status(status));
        register_ladder(&client, 0xD0).expect_err("register fails");
        register_ladder(&client, 0xD0).expect_err("register fails again");
        assert_eq!(
            pr_registrant_cap(),
            0,
            "status {status:#x} is transient — never a cap"
        );
    }
    // A device-answered command-specific refusal ONCE: a candidate, not a
    // cap; the SAME refusal again at the same count: learned (3).
    client.set(RegisterAnswer::Status(0x0102));
    register_ladder(&client, 0xD0).expect_err("refused");
    assert_eq!(
        pr_registrant_cap(),
        0,
        "one refusal is a candidate, not a cap"
    );
    register_ladder(&client, 0xD0).expect_err("refused again");
    assert_eq!(
        pr_registrant_cap(),
        3,
        "the repeated refusal at 3 registrants learns 3"
    );
    // A REGISTER that SUCCEEDS at or past the learned count unlearns it.
    client.set(RegisterAnswer::Ok);
    register_ladder(&client, 0xD0).expect("the device admits the 4th registrant");
    assert_eq!(client.report().unwrap().regctl(), 4);
    assert_eq!(
        pr_registrant_cap(),
        0,
        "a success at regctl ≥ learned proves the cap wrong — unlearned"
    );
    clear_learned_registrant_cap();
}
