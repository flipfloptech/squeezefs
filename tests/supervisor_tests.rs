//! PR M5 (reference-client survey P1-B, JuiceFS `cmd/mount_unix.go`
//! precedent): the external mount supervisor — `squeezefs mount
//! --supervise` keeps the parent process alive to probe the mount and, on
//! sustained unresponsiveness, dump daemon state and abort the FUSE
//! connection to release blocked kernel callers.
//!
//! Unit tier (this file): the pure escalation state machine, the sysfs
//! abort/waiting plumbing against a tempdir root, and the connection-id
//! derivation. The full wedge→dump→abort→manual-restart loop against a
//! real wedged mount is a manual-verify runbook item (the abort file
//! requires a live /sys/fs/fuse/connections entry and root).

use squeezefs::supervisor::{
    abort_fuse_connection, connection_waiting, dump_daemon_state, minor_of_dev, probe_stats_inode,
    SupervisorAction, SupervisorPolicy, SupervisorState,
};
use std::time::{Duration, Instant};

fn policy() -> SupervisorPolicy {
    SupervisorPolicy {
        probe_interval: Duration::from_secs(5),
        unresponsive_after: Duration::from_secs(30),
        write_abort: true,
    }
}

// ---------------------------------------------------------------------------
// Escalation state machine (pure — driven with synthetic clocks)
// ---------------------------------------------------------------------------

/// Healthy probes never escalate; failures escalate exactly once when the
/// unresponsive window crosses the policy threshold.
#[test]
fn state_machine_escalates_once_after_threshold() {
    let t0 = Instant::now();
    let p = policy();
    let mut st = SupervisorState::new(t0);

    // Healthy probes: no action.
    assert_eq!(
        st.observe(true, t0 + Duration::from_secs(5), &p),
        SupervisorAction::None
    );
    assert_eq!(
        st.observe(true, t0 + Duration::from_secs(10), &p),
        SupervisorAction::None
    );

    // Failures inside the grace window: no action yet.
    assert_eq!(
        st.observe(false, t0 + Duration::from_secs(15), &p),
        SupervisorAction::None,
        "5 s of silence is inside the 30 s grace window"
    );
    assert_eq!(
        st.observe(false, t0 + Duration::from_secs(35), &p),
        SupervisorAction::None,
        "25 s of silence is still inside the window (last ok at t+10)"
    );

    // Crossing the threshold escalates, reporting the true silence span.
    match st.observe(false, t0 + Duration::from_secs(41), &p) {
        SupervisorAction::Escalate { unresponsive_for } => {
            assert_eq!(
                unresponsive_for,
                Duration::from_secs(31),
                "silence is measured from the last SUCCESSFUL probe"
            );
        }
        other => panic!("expected escalation at 31 s of silence, got {other:?}"),
    }

    // Escalation is one-shot: continued failure does not re-fire.
    assert_eq!(
        st.observe(false, t0 + Duration::from_secs(50), &p),
        SupervisorAction::None,
        "escalation must be one-shot while the wedge persists (abort is destructive)"
    );
}

/// A successful probe after failures logs recovery and re-arms the
/// escalation.
#[test]
fn state_machine_recovery_rearms() {
    let t0 = Instant::now();
    let p = policy();
    let mut st = SupervisorState::new(t0);

    assert_eq!(
        st.observe(false, t0 + Duration::from_secs(31), &p),
        SupervisorAction::Escalate {
            unresponsive_for: Duration::from_secs(31)
        }
    );

    // Recovery is reported.
    assert_eq!(
        st.observe(true, t0 + Duration::from_secs(36), &p),
        SupervisorAction::Recovered,
        "the first healthy probe after failures must report recovery"
    );
    assert_eq!(
        st.observe(true, t0 + Duration::from_secs(41), &p),
        SupervisorAction::None,
        "steady healthy state is quiet"
    );

    // And the escalation can fire again after a fresh wedge.
    assert_eq!(
        st.observe(false, t0 + Duration::from_secs(50), &p),
        SupervisorAction::None
    );
    match st.observe(false, t0 + Duration::from_secs(72), &p) {
        SupervisorAction::Escalate { unresponsive_for } => {
            assert_eq!(unresponsive_for, Duration::from_secs(31));
        }
        other => panic!("re-armed escalation must fire on a fresh wedge, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Sysfs plumbing (tempdir-rooted)
// ---------------------------------------------------------------------------

/// `abort_fuse_connection` writes exactly "1" to
/// `<root>/<conn>/abort` — the kernel's unwedge trigger.
#[test]
fn abort_write_plumbing() {
    let root = tempfile::tempdir().unwrap();
    let conn = root.path().join("42");
    std::fs::create_dir(&conn).unwrap();
    std::fs::write(conn.join("abort"), "").unwrap();

    abort_fuse_connection(root.path(), 42).expect("abort write must succeed");
    let content = std::fs::read_to_string(conn.join("abort")).unwrap();
    assert_eq!(content, "1", "the abort file protocol is a literal '1'");

    // Missing connection dir → loud error, never silent success.
    abort_fuse_connection(root.path(), 43)
        .expect_err("aborting a nonexistent connection must fail loudly");
}

/// `connection_waiting` parses the kernel's `waiting` count and returns
/// None for missing/garbage files.
#[test]
fn waiting_count_parsing() {
    let root = tempfile::tempdir().unwrap();
    let conn = root.path().join("7");
    std::fs::create_dir(&conn).unwrap();

    assert_eq!(connection_waiting(root.path(), 7), None, "missing file");
    std::fs::write(conn.join("waiting"), "13\n").unwrap();
    assert_eq!(connection_waiting(root.path(), 7), Some(13));
    std::fs::write(conn.join("waiting"), "garbage").unwrap();
    assert_eq!(connection_waiting(root.path(), 7), None, "garbage file");
}

/// The connection id is the mount root's st_dev MINOR (what
/// /sys/fs/fuse/connections keys by).
#[test]
fn connection_id_is_dev_minor() {
    let dev = libc::makedev(0, 123);
    assert_eq!(minor_of_dev(dev), 123);
    // High minors use the extended encoding — must round-trip too.
    let dev = libc::makedev(0, 1048575);
    assert_eq!(minor_of_dev(dev), 1048575);
}

// ---------------------------------------------------------------------------
// Probe + dump smoke (real fs, no mount needed)
// ---------------------------------------------------------------------------

/// The probe returns false when `.stats` does not exist and true when it
/// does — with the timeout bounding the wait either way.
#[test]
fn probe_stats_inode_smoke() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !probe_stats_inode(dir.path(), Duration::from_secs(2)),
        "no .stats -> unhealthy"
    );
    std::fs::write(dir.path().join(".stats"), "{}").unwrap();
    assert!(
        probe_stats_inode(dir.path(), Duration::from_secs(2)),
        ".stats present -> healthy"
    );
}

/// The daemon state dump is best-effort but must at least carry the
/// process status of a live PID (ourselves) and never panic on a dead one.
#[test]
fn dump_daemon_state_smoke() {
    let me = std::process::id();
    let dump = dump_daemon_state(me);
    assert!(
        dump.contains(&format!("pid {me}")) && dump.contains("State:"),
        "dump must include the pid header and /proc status, got: {dump:?}"
    );
    // Dead PID: no panic, explicit unavailability.
    let dump = dump_daemon_state(u32::MAX - 1);
    assert!(dump.contains("unavailable"), "dead pid must degrade loudly");
}
