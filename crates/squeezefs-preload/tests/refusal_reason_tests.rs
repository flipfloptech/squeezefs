//! Reason-bearing establish/bind refusal lines, red-first (user
//! directive 2026-07-25: "make it so the shim prints out the stderr
//! line of the reason it isn't being used i.e. the version mismatch").
//!
//! Contracts pinned here:
//!
//! - **The refusal line names the actual cause** ([`SessionError::describe`]
//!   + [`refuse_reason`]): a version skew carries BOTH build commits, an
//!   unarmed mount names the `--interception` remedy, a degenerate dev
//!   identity names the `SQUEEZEFS_IPC_ALLOW_DEV` override, every daemon
//!   refusal class has distinct text, and unknown classes surface their
//!   number instead of hiding.
//! - **Once per (mount, reason), process lifetime** ([`RefusalOnce`]):
//!   the establish ladder retries on every eligible open BY DESIGN (the
//!   mount stays ours); the LOGGING must not spam — the field report's
//!   32-thread run printed a line per open. Exactly one `true` per
//!   distinct (st_dev, reason-code) key, races included.
//! - **Distinct causes get distinct dedup codes**
//!   ([`SessionError::reason_code`]): a mount that first refuses for one
//!   reason and later for another prints both.

use squeezefs_il::session::{refuse_reason, RefusalOnce, SessionError};
use squeezefs_ipc::layout::IPC_ABI;
use squeezefs_ipc::wire::RefuseClass;

const SHIM: &str = "c318b29000000000000000000000000000000000";
const DAEMON: &str = "94382fd000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// describe(): the reason text names the actual cause
// ---------------------------------------------------------------------------

#[test]
fn version_skew_names_both_build_commits() {
    let s = SessionError::VersionSkew.describe(IPC_ABI, DAEMON, SHIM);
    assert!(s.contains("build mismatch"), "cause named: {s}");
    assert!(s.contains(SHIM), "shim commit surfaced: {s}");
    assert!(s.contains(DAEMON), "daemon commit surfaced: {s}");
}

#[test]
fn version_skew_abi_mismatch_names_both_abis() {
    let s = SessionError::VersionSkew.describe(IPC_ABI + 3, SHIM, SHIM);
    assert!(s.contains("abi"), "abi cause named: {s}");
    assert!(
        s.contains(&IPC_ABI.to_string()) && s.contains(&(IPC_ABI + 3).to_string()),
        "both abi values surfaced: {s}"
    );
}

#[test]
fn version_skew_degenerate_identity_names_the_dev_override() {
    // Equal commits, both degenerate (-dirty): the skew is the KD-7
    // degenerate-identity refusal — the line must name the counted
    // override so a dev-box user knows the remedy.
    let dirty = format!("{}-dirty", "a".repeat(40));
    let s = SessionError::VersionSkew.describe(IPC_ABI, &dirty, &dirty);
    assert!(
        s.contains("SQUEEZEFS_IPC_ALLOW_DEV"),
        "dev-override remedy named: {s}"
    );
}

#[test]
fn disabled_refusal_names_the_interception_remedy() {
    let s = refuse_reason(RefuseClass::Disabled as u32);
    assert!(s.contains("interception not armed"), "cause named: {s}");
    assert!(s.contains("--interception"), "remedy named: {s}");
    // And through the SessionError face the establish ladder sees:
    let e = SessionError::Refused(RefuseClass::Disabled as u32);
    assert!(e
        .describe(IPC_ABI, SHIM, SHIM)
        .contains("interception not armed"));
}

#[test]
fn socket_reasons_distinguish_eof_from_errno() {
    let eof = SessionError::Socket(0).describe(IPC_ABI, SHIM, SHIM);
    let refused = SessionError::Socket(libc::ECONNREFUSED).describe(IPC_ABI, SHIM, SHIM);
    assert!(eof.contains("closed"), "EOF named: {eof}");
    assert!(
        refused.contains(&libc::ECONNREFUSED.to_string()),
        "errno surfaced: {refused}"
    );
    assert_ne!(eof, refused);
}

#[test]
fn every_daemon_refusal_class_has_distinct_text_and_unknown_carries_its_number() {
    let classes: Vec<u32> = (1..=8).collect();
    let texts: Vec<String> = classes.iter().map(|c| refuse_reason(*c)).collect();
    for (i, a) in texts.iter().enumerate() {
        for b in texts.iter().skip(i + 1) {
            assert_ne!(a, b, "refusal classes must be tellable apart");
        }
    }
    let unknown = refuse_reason(999);
    assert!(unknown.contains("999"), "unknown class number surfaced: {unknown}");
}

// ---------------------------------------------------------------------------
// reason_code(): distinct causes, distinct dedup keys
// ---------------------------------------------------------------------------

#[test]
fn reason_codes_are_distinct_per_cause() {
    let causes = [
        SessionError::VersionSkew,
        SessionError::Socket(0),
        SessionError::Socket(libc::ECONNREFUSED),
        SessionError::Refused(RefuseClass::Version as u32),
        SessionError::Refused(RefuseClass::Disabled as u32),
        SessionError::Protocol,
        SessionError::Map(libc::ENOMEM),
        SessionError::Poisoned,
    ];
    let codes: Vec<u32> = causes.iter().map(|e| e.reason_code()).collect();
    for (i, a) in codes.iter().enumerate() {
        for (j, b) in codes.iter().enumerate().skip(i + 1) {
            assert_ne!(
                a, b,
                "distinct causes need distinct dedup codes ({:?} vs {:?})",
                causes[i], causes[j]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// RefusalOnce: once per (mount, reason), races included
// ---------------------------------------------------------------------------

#[test]
fn refusal_once_dedups_per_mount_and_reason() {
    let once = RefusalOnce::new();
    assert!(once.first(7, 1), "first (mount, reason) prints");
    assert!(!once.first(7, 1), "repeat is suppressed");
    assert!(once.first(7, 2), "same mount, new reason prints");
    assert!(once.first(8, 1), "new mount, same reason prints");
    assert!(!once.first(8, 1));
    assert!(!once.first(7, 2));
}

#[test]
fn refusal_once_is_exactly_once_under_racing_threads() {
    // The field shape: 32 threads all failing establish on the same
    // mount concurrently — exactly ONE may print.
    let once = std::sync::Arc::new(RefusalOnce::new());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
    let mut handles = Vec::new();
    for _ in 0..32 {
        let (once, hits, barrier) = (once.clone(), hits.clone(), barrier.clone());
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..64 {
                if once.first(42, 9) {
                    hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("no panics");
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "exactly one thread prints per (mount, reason)"
    );
}

#[test]
fn refusal_once_full_set_never_silently_loses_a_new_reason() {
    // Beyond capacity the set stops REMEMBERING, never stops PRINTING:
    // a brand-new reason must stay loud even in a pathological process
    // that saw hundreds of distinct (mount, reason) pairs.
    let once = RefusalOnce::new();
    for k in 0..10_000u32 {
        once.first(u64::from(k), 1);
    }
    assert!(
        once.first(999_999, 7),
        "a NEW key past capacity still reports first=true (loud beats lossy)"
    );
}
