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
/// (FUSE assigns each connection an anonymous block device; the mount
/// root's `st_dev` minor is the connection id, the JuiceFS derivation).
pub fn minor_of_dev(dev: u64) -> u64 {
    // RED scaffold.
    let _ = dev;
    0
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
/// `None` when the file is missing/unreadable.
pub fn connection_waiting(sysfs_root: &Path, conn_id: u64) -> Option<u64> {
    // RED scaffold.
    let _ = (sysfs_root, conn_id);
    None
}

/// Write `1` to the connection's `abort` file — the kernel then fails all
/// in-flight and future requests on this connection with ECONNABORTED,
/// releasing blocked callers (the "unwedge"). Destructive by design: the
/// mount is dead afterwards and must be remounted.
pub fn abort_fuse_connection(sysfs_root: &Path, conn_id: u64) -> io::Result<()> {
    // RED scaffold: no write.
    let _ = conn_dir(sysfs_root, conn_id);
    Ok(())
}

/// Best-effort daemon state dump for the escalation log: /proc status,
/// wchan and per-task kernel stacks (stacks need root; unreadable pieces
/// are skipped, never fatal).
pub fn dump_daemon_state(pid: u32) -> String {
    // RED scaffold.
    let _ = pid;
    String::new()
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
        .name("sqfs-supervise-probe".into())
        .spawn(move || {
            let ok = std::fs::metadata(&target).is_ok();
            let _ = tx.send(ok);
        })
        .map(|_| ())
        .unwrap_or(());
    matches!(rx.recv_timeout(timeout), Ok(true))
}
