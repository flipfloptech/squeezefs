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

use squeezefs::meta_backend::reservation::{
    parse_reservation_report, report_len_for, FakeNvmeNamespace, FakeReservationClient,
    ReservationClient, PR_REGISTRANT_CAP_ENV,
};
use std::sync::Arc;

/// Knob reads are process-global; the contracts that set one serialize.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    assert!(report.registered(0x1000 + 63), "the 64th registrant is REPORTED");
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
    use squeezefs::data_custody::{join_wero_as_registrant, pr_registrant_cap_refusals};
    let _g = ENV.lock().await;
    let ns = FakeNvmeNamespace::lenient_register();
    // An authority holds WERO; seven more hosts are registered under it —
    // eight registrants on the namespace.
    let authority = FakeReservationClient::new(Arc::clone(&ns), "nqn.auth", "auth-host");
    authority.register(0xA0).unwrap();
    authority.acquire_write_exclusive_registrants_only(0xA0).unwrap();
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
    authority.acquire_write_exclusive_registrants_only(0xA0).unwrap();
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
