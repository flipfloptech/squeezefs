//! The **lost-commit-wake wedge** (generic/795, 2026-09-08 —
//! `.benchmarks/2026-09-08-generic-795-lookup-wedge.md`).
//!
//! Field tape (the 1.2.2 release gate's fstests leg, `generic/795`:
//! drop_caches × fsstress × rm/cp/cmp on a fresh mount): two LOOKUPs
//! delivered on queue 16 were never COMMITted for 23 minutes. The per-op
//! watchdog stayed silent (the handlers began and finished within 30 s —
//! they REPLIED), `transport_slots_overdue` named the two slots every
//! 5 s, and all 32 queue workers sat parked UNBOUNDED in
//! `submit_and_wait(want=1)`. The reply's wake chain (commit_tx push →
//! WakeCoalescer arm → eventfd write → wake-fd PollAdd → task_work →
//! worker) lost exactly one wake, and with nothing bounding the park the
//! commit message sat unpumped until the harness aborted the connection.
//!
//! The 2026-08-07 zc bridge campaign closed this class for zc pends with
//! the BOUNDED-OUTCOME law — the worker's park is EXT_ARG-bounded
//! (100 ms) so the pass is self-clocked — but the bound engaged ONLY
//! while a bridge pend or a fused resident lived. This suite generalizes
//! it: **a worker whose drain group OWES any reply parks bounded**, and
//! the pass after a tick attributes what a wake should have delivered
//! (`transport_park_tick_commit_rescues` — messages pumped after a tick
//! with no wake CQE; `transport_park_tick_cqe_rescues` — completions the
//! tick's own enter surfaced). Idle workers keep the unbounded park.
//!
//! The lost wake is selected DETERMINISTICALLY through the registered
//! test seam `SQUEEZEFS_TEST_DROP_COMMIT_WAKES=N`: the reply path skips
//! the first N eventfd wake writes after arming the coalescer — the
//! commit message stays queued and the worker's PollAdd never fires,
//! the exact interleave of the field capture.
//!
//! Contracts (red-first):
//! 1. a lost commit wake resolves within the bounded park — the `stat`
//!    whose reply wake the seam dropped completes inside 3 s (pre-fix it
//!    strands until an unrelated CQE on that worker, i.e. forever on a
//!    quiet queue), the rescue ledger accounts it, the 5 s slot watchdog
//!    was never needed, and the mount stays serviceable;
//! 2. a healthy mount ticks nothing it owes — a burst of stats and reads
//!    with the seam unloaded leaves both rescue counters at 0 (a nonzero
//!    value IS the lost-wake tripwire).
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate
//! (`tests/run_require_mount_gate.sh`).

use squeezefs_testkit::{mount_supported, site};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The seam's own log marker (the daemon names every dropped wake).
const SEAM_MARKER: &str = "TEST SEAM dropping commit wake";
/// The rescue ledger's attribution WARN (first rescue per worker).
const RESCUE_MARKER: &str = "transport_park_tick_commit_rescues";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch base under the system temp dir, CANONICALIZED before anything
/// is mounted under it.
///
/// `/tmp`, never `$HOME`: a desktop's volume monitor (GVfs) probes every
/// `$HOME`-rooted mount within milliseconds of the arm — a burst of
/// GETATTR/LOOKUP/READDIRPLUS across several CPUs' queues — and the
/// FIRST over-uring reply is the one the seam drops. A prober's victim
/// sits on a BUSY queue, where the prober's own next delivery pumps the
/// stranded commit before the 100 ms tick can (measured: no rescue in 1
/// of 3 runs); mounts under `/tmp` receive no such probe (measured: zero
/// unprompted deliveries), so the victim is deterministically this
/// test's request on a queue only it uses. Canonicalized here because the
/// mountpoint is matched against `/proc/self/mountinfo` verbatim later,
/// and resolving it after the mount would stat the mounted root — a FUSE
/// request, which is the very thing a wedged mount never answers.
fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_cwake_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

fn format_volume(base: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * 1024 * 1024)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(2 * 1024 * 1024 * 1024)
        .expect("size data file");
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    meta
}

/// The fusectl connection directory of OUR mount, resolved through
/// `/proc/self/mountinfo` (device `major:minor` → `/sys/fs/fuse/connections/<minor>`)
/// so the lookup never issues a FUSE request. `None` once unmounted.
fn own_fuse_connection(mnt: &Path) -> Option<PathBuf> {
    let want = mnt.to_str()?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        // mountinfo escapes spaces/tabs/newlines/backslashes octally;
        // the scratch path carries none, so a literal compare suffices.
        if fields[4] != want {
            continue;
        }
        let minor = fields[2].split(':').nth(1)?;
        return Some(PathBuf::from("/sys/fs/fuse/connections").join(minor));
    }
    None
}

/// Abort OUR connection iff it holds waiting (lost-reply) requests — the
/// unwedge primitive, scoped to this suite's mount so a live foreign
/// mount on the box (a storm rig's, an operator's) is never touched.
fn abort_own_connection_if_waiting(mnt: &Path) {
    let Some(conn) = own_fuse_connection(mnt) else {
        return;
    };
    let waiting = std::fs::read_to_string(conn.join("waiting"))
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if waiting > 0 {
        eprintln!(
            "aborting FUSE connection {} (waiting={waiting}) — a lost reply is being unwedged",
            conn.display()
        );
        let _ = std::fs::write(conn.join("abort"), "1");
    }
}

struct Mount {
    child: Child,
    mnt: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        // A serviceable mount unmounts cleanly. A wedged one (pre-fix)
        // refuses EBUSY behind the stranded path walk or parks `umount`
        // behind the unanswered request: bound the attempt, then abort
        // OUR connection and unmount again.
        let exited = |child: &mut Child, bound: Duration| -> bool {
            let deadline = Instant::now() + bound;
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    return true;
                }
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        let mnt = self.mnt.clone();
        let umount = std::thread::spawn(move || {
            let _ = Command::new(bin()).arg("umount").arg(&mnt).output();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !umount.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !exited(&mut self.child, Duration::from_secs(5)) {
            abort_own_connection_if_waiting(&self.mnt);
            let _ = Command::new(bin()).arg("umount").arg(&self.mnt).output();
            if !exited(&mut self.child, Duration::from_secs(20)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        // A dying pre-fix daemon can strand a fresh batch of waiting
        // requests (the unmount's own FLUSH/RELEASE) on its way out.
        abort_own_connection_if_waiting(&self.mnt);
    }
}

/// Spawn the real daemon on the field venue (zc OFF — the fstests runner's
/// default, the capture's posture) with the commit-wake drop seam loaded
/// for `drop_n` wakes. Readiness is read off the LOG ("transport armed for
/// this session"), never off a `.stats` read: with the seam loaded the
/// first over-uring reply's wake is the one that is dropped, and a probe
/// read would be that reply — pre-fix the fixture itself would hang.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, drop_n: u64) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        // SAFETY: getuid/getgid are trivially safe.
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .env("SQUEEZEFS_FUSE_ZC", "0")
        .env("SQUEEZEFS_TEST_DROP_COMMIT_WAKES", drop_n.to_string())
        // Per-request deliver/reply/commit tracing: when this suite fails
        // the daemon log is the evidence (which request the seam hit, on
        // which queue, and what its worker did next).
        .env("SQUEEZEFS_TRANSPORT_DEBUG", "1")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
    let mut mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if log_contains(log, "transport armed for this session") {
            break;
        }
        if let Ok(Some(status)) = mount.child.try_wait() {
            panic!(
                "mount exited before arming the transport ({status}); log: {}\n{}",
                log.display(),
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "transport did not arm within 90 s (log: {})",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    mount
}

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

/// Poll the daemon log for `needle` up to `bound` (the daemon's logger
/// writes a line per event, but a line can trail the event by a
/// scheduler quantum).
fn wait_log_contains(log: &Path, needle: &str, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        if log_contains(log, needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stats_metric(mnt: &Path, key: &str) -> Option<u64> {
    let raw = std::fs::read_to_string(mnt.join(".stats")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("metrics")?.get(key)?.as_u64()
}

/// A `stat` with a BOUNDED wait: the call runs on a helper thread and the
/// caller waits at most `bound` for its outcome. `None` = the stat is
/// stranded (the caller is in uninterruptible sleep on a reply that never
/// came — the wedge). The helper thread is unblocked by the fixture's
/// connection abort at teardown.
fn bounded_stat(path: PathBuf, bound: Duration) -> Option<std::io::Result<std::fs::Metadata>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::metadata(&path));
    });
    rx.recv_timeout(bound).ok()
}

/// Poll a stats-inode metric through BOUNDED reads until `accept` holds
/// or `bound` elapses. Each `.stats` read runs on its own helper thread
/// (a read that lands on the wedged queue strands pre-fix — exactly the
/// "stats reads from every CPU hung" face of the field capture). Returns
/// the last value read (`None` = never readable inside the bound).
fn wait_metric(
    mnt: &Path,
    key: &str,
    accept: impl Fn(u64) -> bool,
    bound: Duration,
) -> Option<u64> {
    let deadline = Instant::now() + bound;
    let mut last = None;
    loop {
        let (tx, rx) = mpsc::channel();
        let (m, k) = (mnt.to_path_buf(), key.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(stats_metric(&m, &k));
        });
        let left = deadline.saturating_duration_since(Instant::now());
        if let Ok(Some(v)) = rx.recv_timeout(left) {
            last = Some(v);
            if accept(v) {
                return last;
            }
        }
        if Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Contract 1: the seam drops the wake of the FIRST reply to travel the
/// ring — this test's `stat` (the LOOKUP, or the root GETATTR the path
/// walk issues first; either way the stat is what strands), on a queue
/// nothing else uses (`scratch` explains the venue). The stranded reply
/// must be committed inside the bounded park (one 100 ms tick and the
/// pass that follows — well under 3 s), the rescue ledger must account
/// it, the 5 s slot watchdog must never have been needed, and the mount
/// must be serviceable afterwards. Pre-fix the victim's queue is wedged
/// for good: the stat strands until the harness aborts the connection.
#[test]
fn a_lost_commit_wake_resolves_within_the_bounded_park() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("lost");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mount = spawn_mount(&meta, &mnt, &log, 1);

    // Drive replies over the ring until the daemon names the dropped
    // wake. Every stat is bounded: a strand IS the wedge.
    let mut seam_fired = false;
    for attempt in 0..10u32 {
        let started = Instant::now();
        let outcome = bounded_stat(
            mnt.join(format!("no-such-entry-{attempt}")),
            Duration::from_secs(3),
        );
        assert!(
            outcome.is_some(),
            "stat #{attempt} STRANDED for 3 s against a dropped commit wake — the worker \
             parked unbounded with a reply owed (the generic/795 wedge; the lost-wake \
             class has no bounded park); daemon log: {}",
            log.display()
        );
        let err = outcome
            .expect("checked is_some")
            .expect_err("a fresh name must not exist");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "the LOOKUP must carry its real answer (ENOENT), not a synthesized one"
        );
        eprintln!("stat #{attempt} answered in {:?}", started.elapsed());
        if wait_log_contains(&log, SEAM_MARKER, Duration::from_millis(500)) {
            seam_fired = true;
            break;
        }
    }
    assert!(
        seam_fired,
        "the drop seam never fired — no over-uring reply travelled (log: {})",
        log.display()
    );

    // The rescue ledger: within the bounded park's cadence, the pass after
    // a tick found the commit message a wake should have delivered.
    let commit_rescues = wait_metric(
        &mnt,
        "transport_park_tick_commit_rescues",
        |v| v >= 1,
        Duration::from_secs(3),
    );
    assert!(
        commit_rescues.is_some_and(|v| v >= 1),
        "the stranded reply was never rescued inside 3 s (transport_park_tick_commit_rescues \
         = {commit_rescues:?}; None = the stats inode itself never answered) — the \
         generic/795 wedge; daemon log: {}",
        log.display()
    );
    assert!(
        wait_log_contains(&log, RESCUE_MARKER, Duration::from_secs(2)),
        "the first rescue on a worker must log its attribution snapshot (log: {})",
        log.display()
    );
    // The 100 ms bound resolved it long before the 5 s watchdog could
    // name the slot.
    assert_eq!(
        stats_metric(&mnt, "transport_slots_overdue").unwrap_or(u64::MAX),
        0,
        "the slot watchdog must never have been needed (log: {})",
        log.display()
    );

    // Serviceable, not merely unwedged: the seam budget is spent, a fresh
    // stat and a real file round-trip both return promptly.
    let again = bounded_stat(mnt.join("no-such-entry-after"), Duration::from_secs(3));
    assert!(
        again.is_some(),
        "a stat AFTER the rescue must return (log: {})",
        log.display()
    );
    let file = mnt.join("probe.bin");
    std::fs::File::create(&file)
        .expect("create probe")
        .write_all(b"bounded park")
        .expect("write probe");
    assert_eq!(
        std::fs::read(&file).expect("read probe"),
        b"bounded park",
        "the mount must serve byte-exact after the rescue"
    );

    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}

/// Contract 2 — the must-stay-0 half: with the seam unloaded, a burst of
/// stats and reads finds nothing a tick had to rescue. Ticks themselves
/// are legal (a handler slower than the 100 ms bound earns one); a tick
/// that FINDS work is the tripwire.
#[test]
fn a_healthy_mount_ticks_nothing_it_owes() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("healthy");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mount = spawn_mount(&meta, &mnt, &log, 0);

    let file = mnt.join("burst.bin");
    let payload = vec![0x5Au8; 64 * 1024];
    std::fs::File::create(&file)
        .expect("create burst file")
        .write_all(&payload)
        .expect("write burst file");
    for i in 0..200u32 {
        let missing = mnt.join(format!("no-such-entry-{i}"));
        let err = std::fs::metadata(&missing).expect_err("fresh names do not exist");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::metadata(&file).expect("stat the burst file");
        assert_eq!(
            std::fs::read(&file).expect("read the burst file").len(),
            payload.len()
        );
    }

    assert_eq!(
        stats_metric(&mnt, "transport_park_tick_commit_rescues").unwrap_or(u64::MAX),
        0,
        "a healthy mount's ticks must find no unpumped commit (the lost-wake tripwire; \
         log: {})",
        log.display()
    );
    assert_eq!(
        stats_metric(&mnt, "transport_park_tick_cqe_rescues").unwrap_or(u64::MAX),
        0,
        "a healthy mount's ticks must surface no unreaped completion (the lost-wake \
         tripwire; log: {})",
        log.display()
    );
    assert_eq!(
        stats_metric(&mnt, "transport_slots_overdue").unwrap_or(u64::MAX),
        0,
        "no slot may go overdue on a healthy burst (log: {})",
        log.display()
    );
    assert!(
        !log_contains(&log, SEAM_MARKER),
        "the seam must be inert at its default (log: {})",
        log.display()
    );

    drop(mount);
    let _ = std::fs::remove_dir_all(&base);
}
