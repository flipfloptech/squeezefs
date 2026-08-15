//! External mount supervisor (reference-client survey P1-B, JuiceFS
//! `cmd/mount_unix.go:109-283` precedent): a **parent-process** watchdog
//! that (a) periodically stats the mount's virtual `.stats` inode, (b) on
//! sustained unresponsiveness logs loudly and dumps daemon state, and
//! (c) can write `/sys/fs/fuse/connections/<id>/abort` to unwedge blocked
//! kernel callers.
//!
//! This complements the in-daemon D1.b watchdog (PR M4), which can *log* a
//! wedge but cannot *clear* one — once the daemon itself is the problem,
//! only an external actor can release the kernel waiters. The daemon-side
//! surface is deliberately zero: everything here runs in the `squeezefs
//! mount --supervise` parent, std-only (no tokio — the parent never starts
//! a runtime), and kills nothing. Killing/restarting the daemon stays a
//! manual runbook step; the supervisor's escalation prints the exact PID
//! and commands (kill-by-PID discipline — never pattern-kill).
//!
//! Unit-testable pieces (pinned by `tests/supervisor_tests.rs`): the
//! escalation state machine, the sysfs abort plumbing (rooted at a
//! parameterized directory so tests use a tempdir), and the FUSE
//! connection-id derivation (`st_dev` minor of the mount root — the id
//! `/sys/fs/fuse/connections/` keys by).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The real sysfs root the mount verb passes; tests pass a tempdir.
pub const FUSE_CONNECTIONS_SYSFS: &str = "/sys/fs/fuse/connections";

/// Supervisor cadence + escalation policy.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorPolicy {
    /// How often the `.stats` inode is probed (JuiceFS: 5 s).
    pub probe_interval: Duration,
    /// Sustained unresponsiveness that triggers escalation (JuiceFS: 30 s).
    pub unresponsive_after: Duration,
    /// Whether escalation may write the FUSE connection abort file.
    pub write_abort: bool,
}

impl Default for SupervisorPolicy {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_secs(5),
            unresponsive_after: Duration::from_secs(30),
            write_abort: true,
        }
    }
}

/// What the supervisor loop must do after one probe observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Mount healthy (or still inside the grace window) — keep probing.
    None,
    /// Mount recovered after being unresponsive — log the recovery.
    Recovered,
    /// Sustained unresponsiveness crossed the policy threshold: dump
    /// daemon state, then (policy-gated) abort the FUSE connection.
    /// One-shot until a successful probe re-arms it.
    Escalate {
        /// How long the mount has been unresponsive.
        unresponsive_for: Duration,
    },
}

/// Pure escalation state machine — deterministic, no clocks of its own
/// (the loop feeds `Instant`s), so tests drive it without sleeping.
#[derive(Debug)]
pub struct SupervisorState {
    last_ok: Instant,
    /// Set once the escalation fired; a successful probe re-arms.
    escalated: bool,
    /// True while at least one probe has failed since the last success.
    failing: bool,
}

impl SupervisorState {
    pub fn new(now: Instant) -> Self {
        Self {
            last_ok: now,
            escalated: false,
            failing: false,
        }
    }

    /// Feed one probe result; returns the action the loop must take.
    ///
    /// - success → resets the silence clock and re-arms escalation;
    ///   reports `Recovered` iff probes had been failing.
    /// - failure inside the grace window (`unresponsive_after` since the
    ///   last success) → `None` (the loop's per-failure warn log carries
    ///   the running silence).
    /// - failure past the window → `Escalate` exactly once; further
    ///   failures stay `None` until a success re-arms (the abort is
    ///   destructive — re-firing it on a dead connection is noise).
    pub fn observe(
        &mut self,
        probe_ok: bool,
        now: Instant,
        policy: &SupervisorPolicy,
    ) -> SupervisorAction {
        if probe_ok {
            let was_failing = self.failing;
            self.last_ok = now;
            self.failing = false;
            self.escalated = false;
            return if was_failing {
                SupervisorAction::Recovered
            } else {
                SupervisorAction::None
            };
        }
        self.failing = true;
        let unresponsive_for = now.saturating_duration_since(self.last_ok);
        if !self.escalated && unresponsive_for >= policy.unresponsive_after {
            self.escalated = true;
            return SupervisorAction::Escalate { unresponsive_for };
        }
        SupervisorAction::None
    }
}

/// Minor number of a `dev_t` — the id `/sys/fs/fuse/connections/` keys by
/// (FUSE assigns each connection an anonymous device; the mount root's
/// `st_dev` minor is the connection id, the JuiceFS derivation). Uses the
/// glibc extended encoding so >8-bit minors round-trip.
pub fn minor_of_dev(dev: u64) -> u64 {
    libc::minor(dev) as u64
}

/// Resolve the FUSE connection id for a mounted path (stat → st_dev →
/// minor). Fails if the path cannot be statted (a wedged mount wedges
/// stat too — resolve at supervisor START, while the mount is healthy).
pub fn fuse_connection_id(mountpoint: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(mountpoint)?;
    Ok(minor_of_dev(meta.dev()))
}

fn conn_dir(sysfs_root: &Path, conn_id: u64) -> PathBuf {
    sysfs_root.join(conn_id.to_string())
}

/// Read the connection's `waiting` count (requests blocked in the kernel).
/// `None` when the file is missing/unreadable/unparseable.
pub fn connection_waiting(sysfs_root: &Path, conn_id: u64) -> Option<u64> {
    let raw = std::fs::read_to_string(conn_dir(sysfs_root, conn_id).join("waiting")).ok()?;
    raw.trim().parse::<u64>().ok()
}

/// Write `1` to the connection's `abort` file — the kernel then fails all
/// in-flight and future requests on this connection with ECONNABORTED,
/// releasing blocked callers (the "unwedge"). Destructive by design: the
/// mount is dead afterwards and must be remounted. Fails loudly when the
/// connection directory is gone (already unmounted) or unwritable (needs
/// root on the real sysfs).
pub fn abort_fuse_connection(sysfs_root: &Path, conn_id: u64) -> io::Result<()> {
    std::fs::write(conn_dir(sysfs_root, conn_id).join("abort"), "1")
}

/// Best-effort daemon state dump for the escalation log: /proc status,
/// per-task wchan and kernel stacks (stacks need root; unreadable pieces
/// are skipped, never fatal — the JuiceFS `printThreadsStack` shape).
pub fn dump_daemon_state(pid: u32) -> String {
    let mut out = format!("=== daemon state dump: pid {pid} ===\n");
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => out.push_str(&status),
        Err(e) => {
            out.push_str(&format!("/proc/{pid}/status unavailable: {e}\n"));
            return out;
        }
    }
    let tasks = match std::fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(t) => t,
        Err(e) => {
            out.push_str(&format!("/proc/{pid}/task unavailable: {e}\n"));
            return out;
        }
    };
    for task in tasks.flatten() {
        let tid = task.file_name();
        let tid = tid.to_string_lossy();
        let base = task.path();
        let comm = std::fs::read_to_string(base.join("comm")).unwrap_or_default();
        let wchan = std::fs::read_to_string(base.join("wchan")).unwrap_or_default();
        out.push_str(&format!(
            "--- tid {tid} ({}) wchan={}\n",
            comm.trim(),
            wchan.trim()
        ));
        // Kernel stacks are root-only; skip silently when unreadable.
        if let Ok(stack) = std::fs::read_to_string(base.join("stack")) {
            out.push_str(&stack);
        }
    }
    out
}

/// Probe the mount's `.stats` inode with a hard timeout. The stat runs on
/// a detached thread because a wedged FUSE mount blocks `stat(2)`
/// uninterruptibly — on timeout the thread is intentionally leaked (it
/// unblocks when the connection is aborted or the daemon recovers; the
/// JuiceFS supervisor accepts the same).
pub fn probe_stats_inode(mountpoint: &Path, timeout: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let target = mountpoint.join(".stats");
    std::thread::Builder::new()
        .name(squeezefs_ipc::comm_core::comm_name("sqfs-supervise-probe"))
        .spawn(move || {
            let ok = std::fs::metadata(&target).is_ok();
            let _ = tx.send(ok);
        })
        .map(|_| ())
        .unwrap_or(());
    matches!(rx.recv_timeout(timeout), Ok(true))
}

/// Is the daemon PID still alive? (`kill(pid, 0)` — the supervisor exits
/// when its child is gone; restarting is the operator's manual step.)
fn daemon_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The `squeezefs mount --supervise` parent loop (survey P1-B; JuiceFS
/// `cmd/mount_unix.go` ladder minus the kill rungs — killing/restarting
/// stays a manual runbook step, printed at escalation with the exact PID;
/// this process never pattern-kills anything).
///
/// Runs until the supervised daemon exits. std-only by design: the parent
/// never starts a tokio runtime.
pub fn run_supervisor(
    mountpoint: &Path,
    daemon_pid: u32,
    policy: SupervisorPolicy,
    sysfs_root: &Path,
) {
    eprintln!(
        "squeezefs-supervise: watching {} (daemon pid {daemon_pid}; probe every {:?}, \
         escalate after {:?} unresponsive, abort {})",
        mountpoint.display(),
        policy.probe_interval,
        policy.unresponsive_after,
        if policy.write_abort {
            "enabled"
        } else {
            "disabled"
        }
    );
    // Resolve the connection id NOW, while the mount answers stat —
    // a wedged mount later would wedge the resolution too.
    let conn_id = match fuse_connection_id(mountpoint) {
        Ok(id) => {
            eprintln!("squeezefs-supervise: FUSE connection id {id}");
            Some(id)
        }
        Err(e) => {
            eprintln!(
                "squeezefs-supervise: WARNING: cannot resolve FUSE connection id ({e}); \
                 the abort rung is disabled for this session"
            );
            None
        }
    };

    let mut state = SupervisorState::new(Instant::now());
    loop {
        std::thread::sleep(policy.probe_interval);
        if !daemon_alive(daemon_pid) {
            eprintln!(
                "squeezefs-supervise: daemon pid {daemon_pid} exited — supervisor \
                 stopping (remount manually: squeezefs mount …)"
            );
            return;
        }
        let ok = probe_stats_inode(mountpoint, policy.probe_interval);
        let now = Instant::now();
        match state.observe(ok, now, &policy) {
            SupervisorAction::None => {
                if !ok {
                    eprintln!(
                        "squeezefs-supervise: probe of {}/.stats timed out (grace window)",
                        mountpoint.display()
                    );
                }
            }
            SupervisorAction::Recovered => {
                eprintln!(
                    "squeezefs-supervise: mount {} recovered",
                    mountpoint.display()
                );
            }
            SupervisorAction::Escalate { unresponsive_for } => {
                eprintln!(
                    "squeezefs-supervise: mount {} UNRESPONSIVE for {:?} (threshold {:?}) — \
                     dumping daemon state",
                    mountpoint.display(),
                    unresponsive_for,
                    policy.unresponsive_after
                );
                eprintln!("{}", dump_daemon_state(daemon_pid));
                if let Some(id) = conn_id {
                    let waiting = connection_waiting(sysfs_root, id);
                    eprintln!(
                        "squeezefs-supervise: connection {id} waiting = {waiting:?} \
                         (kernel callers blocked on the wedged daemon)"
                    );
                    if policy.write_abort && waiting.unwrap_or(0) > 0 {
                        match abort_fuse_connection(sysfs_root, id) {
                            Ok(()) => eprintln!(
                                "squeezefs-supervise: WROTE {}/{id}/abort — blocked callers \
                                 released with ECONNABORTED; the mount is dead. Manual \
                                 recovery: kill the daemon BY PID (kill {daemon_pid}), \
                                 `squeezefs umount <mountpoint>`, then remount",
                                sysfs_root.display()
                            ),
                            Err(e) => eprintln!(
                                "squeezefs-supervise: abort write failed ({e}) — run as \
                                 root for the abort rung; manual: echo 1 > {}/{id}/abort",
                                sysfs_root.display()
                            ),
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Contracts (spec §11 TEST-9)
// ---------------------------------------------------------------------------
//
// The supervisor is the external watchdog that unwedges a hung mount, and
// it had ONE test reference tree-wide. Its escalation state machine is
// deliberately pure ("no clocks of its own ... so tests drive it without
// sleeping") and its sysfs helpers all take an explicit `sysfs_root` — so
// everything below runs unprivileged, deterministically, in microseconds.
// The only untestable rung is the real `/sys/fs/fuse/connections` write,
// which needs root and a live wedged mount.
#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn policy(unresponsive_secs: u64) -> SupervisorPolicy {
        SupervisorPolicy {
            probe_interval: Duration::from_millis(1),
            unresponsive_after: Duration::from_secs(unresponsive_secs),
            write_abort: true,
        }
    }

    #[test]
    fn healthy_probes_never_escalate_and_never_report_recovery() {
        let start = t0();
        let mut st = SupervisorState::new(start);
        let p = policy(30);
        for i in 1..=10u64 {
            let now = start + Duration::from_secs(i * 5);
            assert_eq!(
                st.observe(true, now, &p),
                SupervisorAction::None,
                "a healthy mount is silent — a 'Recovered' log with no prior \
                 failure is a false alarm operators learn to ignore"
            );
        }
    }

    #[test]
    fn failures_inside_the_grace_window_do_not_escalate() {
        let start = t0();
        let mut st = SupervisorState::new(start);
        let p = policy(30);
        // 29 s of silence: still inside the window. Aborting here would
        // destroy a mount that is merely slow.
        for secs in [1u64, 5, 10, 20, 29] {
            assert_eq!(
                st.observe(false, start + Duration::from_secs(secs), &p),
                SupervisorAction::None,
                "escalated at {secs}s, inside a 30s grace window"
            );
        }
    }

    #[test]
    fn sustained_silence_escalates_exactly_once_until_a_success_rearms() {
        let start = t0();
        let mut st = SupervisorState::new(start);
        let p = policy(30);
        assert_eq!(
            st.observe(false, start + Duration::from_secs(10), &p),
            SupervisorAction::None
        );
        match st.observe(false, start + Duration::from_secs(31), &p) {
            SupervisorAction::Escalate { unresponsive_for } => {
                assert!(
                    unresponsive_for >= Duration::from_secs(31),
                    "the escalation must carry the true silence duration"
                );
            }
            other => panic!("31s of silence past a 30s window must escalate, got {other:?}"),
        }
        // The abort is DESTRUCTIVE: re-firing it on a dead connection is
        // noise, so escalation is one-shot until a probe succeeds.
        for secs in [32u64, 60, 600] {
            assert_eq!(
                st.observe(false, start + Duration::from_secs(secs), &p),
                SupervisorAction::None,
                "re-escalated at {secs}s without an intervening success"
            );
        }
        // A success re-arms and reports the recovery once.
        assert_eq!(
            st.observe(true, start + Duration::from_secs(601), &p),
            SupervisorAction::Recovered
        );
        assert_eq!(
            st.observe(true, start + Duration::from_secs(606), &p),
            SupervisorAction::None,
            "recovery is reported once, not on every subsequent probe"
        );
        // ...and a NEW sustained outage escalates again.
        assert_eq!(
            st.observe(false, start + Duration::from_secs(610), &p),
            SupervisorAction::None
        );
        assert!(matches!(
            st.observe(false, start + Duration::from_secs(700), &p),
            SupervisorAction::Escalate { .. }
        ));
    }

    #[test]
    fn the_silence_clock_measures_from_the_last_success_not_the_first_failure() {
        // A flapping mount (fail, succeed, fail, succeed …) must never
        // accumulate its way to an abort: each success resets the clock.
        let start = t0();
        let mut st = SupervisorState::new(start);
        let p = policy(30);
        for i in 0..20u64 {
            let base = start + Duration::from_secs(i * 20);
            assert_eq!(
                st.observe(false, base + Duration::from_secs(10), &p),
                SupervisorAction::None
            );
            let a = st.observe(true, base + Duration::from_secs(19), &p);
            assert_eq!(a, SupervisorAction::Recovered, "flap {i} recovered");
        }
    }

    #[test]
    fn a_boundary_exact_window_escalates() {
        let start = t0();
        let mut st = SupervisorState::new(start);
        let p = policy(30);
        assert!(
            matches!(
                st.observe(false, start + Duration::from_secs(30), &p),
                SupervisorAction::Escalate { .. }
            ),
            "the threshold is >=, not > — a mount silent for exactly the \
             configured window is unresponsive"
        );
    }

    #[test]
    fn the_default_policy_matches_the_documented_cadence() {
        let p = SupervisorPolicy::default();
        assert_eq!(p.probe_interval, Duration::from_secs(5));
        assert_eq!(p.unresponsive_after, Duration::from_secs(30));
        assert!(p.write_abort, "the abort rung is armed by default");
    }

    #[test]
    fn connection_waiting_reads_the_sysfs_counter_and_tolerates_every_absence() {
        let root = tempfile::tempdir().expect("tempdir");
        let sysfs = root.path();
        // No connection directory at all (already unmounted).
        assert_eq!(connection_waiting(sysfs, 42), None);

        let dir = sysfs.join("42");
        std::fs::create_dir_all(&dir).expect("conn dir");
        // Directory present, file absent.
        assert_eq!(connection_waiting(sysfs, 42), None);

        std::fs::write(dir.join("waiting"), "7\n").expect("write");
        assert_eq!(
            connection_waiting(sysfs, 42),
            Some(7),
            "waiting >= 1 is the umount-EBUSY / wedge signal — it must parse"
        );
        std::fs::write(dir.join("waiting"), "  0  \n").expect("write");
        assert_eq!(connection_waiting(sysfs, 42), Some(0));
        // Unparseable content must be None, never a panic and never a
        // fabricated zero (which would read as "healthy").
        std::fs::write(dir.join("waiting"), "not-a-number").expect("write");
        assert_eq!(connection_waiting(sysfs, 42), None);
        std::fs::write(dir.join("waiting"), "").expect("write");
        assert_eq!(connection_waiting(sysfs, 42), None);
    }

    #[test]
    fn abort_writes_exactly_one_to_the_connections_abort_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let sysfs = root.path();
        let dir = sysfs.join("99");
        std::fs::create_dir_all(&dir).expect("conn dir");
        abort_fuse_connection(sysfs, 99).expect("abort write");
        assert_eq!(
            std::fs::read_to_string(dir.join("abort")).expect("abort file"),
            "1",
            "the kernel's unwedge is literally the byte '1'"
        );
    }

    #[test]
    fn abort_on_a_vanished_connection_fails_loud_rather_than_silently() {
        let root = tempfile::tempdir().expect("tempdir");
        // No directory: the mount is already gone. The supervisor must
        // learn that from an Err, not treat it as a successful unwedge.
        let err = abort_fuse_connection(root.path(), 1234)
            .expect_err("aborting a connection that does not exist must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn conn_dir_keys_by_the_decimal_connection_id() {
        let d = conn_dir(Path::new("/sys/fs/fuse/connections"), 4242);
        assert_eq!(d, Path::new("/sys/fs/fuse/connections/4242"));
    }

    #[test]
    fn minor_of_dev_round_trips_wide_minors() {
        // FUSE connection ids are the mount root's st_dev MINOR, and the
        // glibc extended encoding puts minor bits on both sides of the
        // major field — an 8-bit-only decode silently aliases connections
        // on a busy host.
        for minor in [0u64, 1, 255, 256, 4095, 1_048_575] {
            let dev = libc::makedev(0, minor as libc::c_uint);
            assert_eq!(
                minor_of_dev(dev),
                minor,
                "minor {minor} must survive the dev_t round trip"
            );
        }
    }

    #[test]
    fn probe_of_a_missing_mountpoint_is_a_failed_probe_not_a_hang() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(
            !probe_stats_inode(&root.path().join("no-such-mount"), Duration::from_secs(5)),
            "a missing .stats inode is an unhealthy probe"
        );
    }

    #[test]
    fn probe_of_a_real_directory_with_a_stats_entry_succeeds() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(".stats"), "{}").expect("stats");
        assert!(probe_stats_inode(root.path(), Duration::from_secs(5)));
    }

    #[test]
    fn daemon_alive_tracks_a_real_pid() {
        assert!(
            daemon_alive(std::process::id()),
            "our own pid must read alive"
        );
        // pid 0 is 'the caller's process group' for kill(2) — never used
        // as a daemon pid; a very high pid is reliably absent.
        assert!(
            !daemon_alive(0x7FFF_FFFE),
            "an absent pid must read dead so the supervisor exits"
        );
    }

    #[test]
    fn dump_daemon_state_is_best_effort_and_never_panics() {
        let mine = dump_daemon_state(std::process::id());
        assert!(mine.contains("daemon state dump"), "header present");
        assert!(mine.contains("tid "), "at least this thread is listed");
        let gone = dump_daemon_state(0x7FFF_FFFE);
        assert!(
            gone.contains("unavailable"),
            "a dead pid produces a note, never a panic: {gone}"
        );
    }
}
