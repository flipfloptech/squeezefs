//! The staging-root lock handover between a dismounting mount and its
//! successor at the same mount point (record §4.4bx).
//!
//! The 1.3.0 release chain's attempt 6 went RED at fstests generic/752
//! without running it: `_scratch_mount` was refused with "staging root …
//! is HELD by a live process" because generic/751's daemon — on a
//! genuinely FULL 24 GiB volume, its dismount's writeback-retire wait
//! spinning its whole `dismount_wait` on custody that could never land —
//! held the root's liveness flock 13 s past `umount(8)`'s return, against
//! a successor that waited 2 s (the exit-grade bound sized for a daemon
//! exit's ms-grade release). Two laws close it, each pinned here:
//!
//! 1. the PREDECESSOR releases the locks when its dismount teardown
//!    completes — the staging ownership ends there, never at process exit;
//! 2. the SUCCESSOR tells a live co-located collision (a FUSE mount still
//!    at the mount point — refused loud) from a dismounting predecessor
//!    (its mount gone — waited for, up to the predecessor's own exit
//!    guard, the ONE law `dismount_exit_guard`).
//!
//! The live contract is the fstests shape itself: unmount, remount at once,
//! with the predecessor's EXIT held past its teardown by the
//! `SQUEEZEFS_TEST_EXIT_HOLD_MS` seam (the field's 74 GiB address space
//! does it without a seam). RED on the pre-fix tree: the successor refuses
//! at 2 s while the predecessor still holds the lock. The external unmount
//! itself is the bounded retry `unmount_external` (record §4.4by): the
//! desktop's volume monitor holds a transient fd on every fresh mount under
//! `$HOME`, and the one-shot form was the release chain's attempt-7 red.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use squeezefs::config_ops::{
    fuse_mount_present_in, hold_staging_root_lock, hold_staging_root_lock_waiting,
    release_staging_root_locks, staging_root_owner_is_live, SuccessorWait, STAGING_OWNER_LOCK,
};
use squeezefs::fuse_client::dismount_exit_guard;
use squeezefs_testkit::{mount_supported, site};

/// A successor's wait with the holder's class injected.
fn wait<'a>(dismounting: &'a dyn Fn() -> bool, bound: Duration) -> SuccessorWait<'a> {
    SuccessorWait {
        holder_is_dismounting: dismounting,
        dismounting_bound: bound,
    }
}

const MIB: u64 = 1024 * 1024;

/// The mountinfo reader's law over a fixture table: the mount point is
/// field 5 (octal-escaped), the type the first word after ` - `.
#[test]
fn a_fuse_mount_is_read_off_mountinfo_by_mount_point_and_type() {
    let table = "\
36 24 0:32 / /proc rw,nosuid - proc proc rw
2891 24 0:1088 / /home/justin/tmp/sqfs\\040a/mnt rw,nosuid,nodev,relatime - fuse.squeezefs squeezefs rw,user_id=1000
2900 24 0:1090 / /mnt/plain rw,relatime - ext4 /dev/sda1 rw
garbage line without the separator
2901 24 0:1091 /";
    assert!(fuse_mount_present_in(
        table,
        Path::new("/home/justin/tmp/sqfs a/mnt")
    ));
    assert!(
        !fuse_mount_present_in(table, Path::new("/mnt/plain")),
        "an ext4 mount at the path is not a FUSE collision"
    );
    assert!(!fuse_mount_present_in(table, Path::new("/proc")));
    assert!(!fuse_mount_present_in(
        table,
        Path::new("/home/justin/tmp/sqfs a")
    ));
    assert!(!fuse_mount_present_in("", Path::new("/mnt/x")));
}

/// The pure wait law with the holder modeled in-process (a second fd of
/// one lock file is a second owner under `flock`): a DISMOUNTING holder is
/// waited for past the exit-grade bound and the lock lands once it frees;
/// a LIVE collision is refused right after that bound; a dismounting
/// holder that outlives the exit guard is refused as wedged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_waits_for_a_dismounting_holder_and_refuses_a_live_collision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();

    // Arm 1: the holder is dismounting and frees the lock after 4 s —
    // past the 2 s exit-grade bound, inside the exit guard.
    hold_staging_root_lock(&root).expect("the holder takes the lock");
    assert!(staging_root_owner_is_live(&root));
    let releaser = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(4));
        release_staging_root_locks();
    });
    let start = Instant::now();
    hold_staging_root_lock_waiting(&root, &wait(&|| true, dismount_exit_guard(10)))
        .await
        .expect("a dismounting holder is waited for and the lock lands");
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(9),
        "the wait ends when the holder frees the lock (waited {waited:?})"
    );
    releaser.join().unwrap();
    release_staging_root_locks();

    // Arm 2: a live collision — a FUSE mount still stands at the mount
    // point — is refused right after the exit-grade bound.
    hold_staging_root_lock(&root).expect("the collider takes the lock");
    let start = Instant::now();
    let err = hold_staging_root_lock_waiting(&root, &wait(&|| false, dismount_exit_guard(10)))
        .await
        .expect_err("a live collision is refused");
    let waited = start.elapsed();
    assert!(
        err.to_string().contains("HELD by a live process"),
        "the refusal names the holder: {err}"
    );
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_secs(4),
        "the collision pays the exit-grade bound once (waited {waited:?})"
    );

    // Arm 3: a dismounting holder past the exit guard is wedged — refused,
    // naming the guard.
    let start = Instant::now();
    let err = hold_staging_root_lock_waiting(&root, &wait(&|| true, Duration::from_secs(3)))
        .await
        .expect_err("a holder past the exit guard is refused");
    let waited = start.elapsed();
    assert!(
        err.to_string().contains("wedged") && err.to_string().contains("3s"),
        "the refusal names the exhausted guard: {err}"
    );
    assert!(
        waited >= Duration::from_secs(3) && waited < Duration::from_secs(5),
        "the wedged refusal lands at the guard (waited {waited:?})"
    );
    release_staging_root_locks();
    assert!(!staging_root_owner_is_live(&root));
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_handover_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn format_volume(base: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .expect("create meta file")
        .set_len(256 * MIB)
        .expect("size meta file");
    std::fs::File::create(&data)
        .expect("create data file")
        .set_len(512 * MIB)
        .expect("size data file");
    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force")
        .arg("--disk-cache-paths")
        .arg(base.join("staging"))
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

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A foreground `mount` (the daemon is the child itself), optionally with
/// the exit held `exit_hold_ms` past its teardown.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, exit_hold_ms: u64) -> Mount {
    let hold = exit_hold_ms.to_string();
    let env: &[(&str, &str)] = if exit_hold_ms > 0 {
        &[("SQUEEZEFS_TEST_EXIT_HOLD_MS", hold.as_str())]
    } else {
        &[]
    };
    spawn_mount_with_env(meta, mnt, log, env)
}

/// A foreground `mount` with the given seams in its environment; returns
/// once the mount serves `.stats`.
fn spawn_mount_with_env(meta: &Path, mnt: &Path, log: &Path, env: &[(&str, &str)]) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn squeezefs mount");
    let mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
        log: log.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready in 90s; log:\n{}",
            mount.log_text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    mount
}

/// Event-driven wait for process exit (pidfd), bounded.
fn wait_exit(child: &mut Child, bound: Duration) -> Option<Duration> {
    let started = Instant::now();
    // SAFETY: pidfd_open on our own child's pid; the fd is closed below.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
    assert!(
        pidfd >= 0,
        "pidfd_open: {}",
        std::io::Error::last_os_error()
    );
    let deadline = started + bound;
    let exited = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        let mut pfd = libc::pollfd {
            fd: pidfd as libc::c_int,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is valid for the duration of the call.
        match unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().clamp(1, 60_000) as i32) } {
            1 => break true,
            0 => continue,
            _ if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) => continue,
            rc => panic!("poll(pidfd) rc {rc}: {}", std::io::Error::last_os_error()),
        }
    };
    // SAFETY: closing the fd this function opened.
    unsafe { libc::close(pidfd as libc::c_int) };
    if exited {
        let _ = child.wait();
        Some(started.elapsed())
    } else {
        None
    }
}

fn is_mounted(mnt: &Path) -> bool {
    let needle = format!(" {} ", mnt.display());
    std::fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(&needle))
}

/// The external unmount's retry bound (record §4.4by): a desktop volume
/// monitor (`gvfs-udisks2-volume-monitor`, with `gvfsd-trash` probing
/// `.Trash-<uid>`) enumerates every new mount under `$HOME` for about its
/// first second, so a non-lazy `umount2` inside that window is EBUSY with
/// no user-visible holder — measured ≈ 15 ms after the readiness read and
/// gone after 1 s of mount age. The bound is the sibling live-mount suites'
/// 5 s (`cache_path_policy_tests`, `encrypt_key_handling_tests`), at a
/// grain below the window; the daemon's own self-unmount carries the same
/// law (`fuse3::raw::session`, "desktop volume monitors inspect every new
/// mount").
const UNMOUNT_RETRY_TICK: Duration = Duration::from_millis(100);
const UNMOUNT_RETRY_ATTEMPTS: u32 = 50;

/// `fusermount3 -u`, retried on a transient EBUSY within the bound above
/// (the release chain's attempt 7 went RED on the one-shot form). Returns
/// the instant the kernel unmount completed — every handover law below is
/// clocked from it, so the retries never enter a measured window. The last
/// attempt's stderr is the evidence when the bound is exhausted.
fn unmount_external(mnt: &Path) -> Instant {
    let started = Instant::now();
    let mut last_err = String::new();
    for attempt in 0..UNMOUNT_RETRY_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(UNMOUNT_RETRY_TICK);
        }
        let out = Command::new("fusermount3")
            .arg("-u")
            .arg(mnt)
            .output()
            .expect("run fusermount3 -u");
        if out.status.success() {
            if attempt > 0 {
                eprintln!(
                    "note: fusermount3 -u {} landed at attempt {} (+{:?}) after a transient \
                     EBUSY — a bystander's fd on the fresh mount (record §4.4by)",
                    mnt.display(),
                    attempt + 1,
                    started.elapsed()
                );
            }
            return Instant::now();
        }
        last_err = String::from_utf8_lossy(&out.stderr).into_owned();
    }
    panic!(
        "fusermount3 -u {} stayed busy for {:?} ({UNMOUNT_RETRY_ATTEMPTS} attempts): {last_err}",
        mnt.display(),
        started.elapsed()
    );
}

/// The one staging root a scoped mount owns under `staging/squeezefs/`
/// (the container holds it beside the shared `cache_segment`).
fn owned_staging_root(base: &Path) -> PathBuf {
    let container = base.join("staging").join("squeezefs");
    let mut roots: Vec<PathBuf> = std::fs::read_dir(&container)
        .expect("staging container exists after the mount")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "cache_segment"))
        .filter(|p| p.join(STAGING_OWNER_LOCK).exists())
        .collect();
    assert_eq!(roots.len(), 1, "one owned staging root: {roots:?}");
    roots.pop().unwrap()
}

/// The fstests shape: unmount, then mount again at the same mount point at
/// once, while the predecessor's process is still alive. Law 1 makes the
/// lock free within the exit-grade bound of the unmount (the predecessor's
/// teardown released it — the process is still up, held by the seam); the
/// successor mounts, reads back the predecessor's file, and unmounts
/// cleanly. Every mount-class self-skip rides the testkit.
#[test]
fn a_successor_mount_lands_while_its_predecessors_exit_outlives_the_dismount() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("live");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");

    // The predecessor: its exit held 12 s past its teardown.
    let mut first = spawn_mount(&meta, &mnt, &base.join("first.log"), 12_000);
    std::fs::write(mnt.join("f"), b"predecessor bytes").expect("write through the mount");
    let root = owned_staging_root(&base);
    assert!(
        staging_root_owner_is_live(&root),
        "the live mount holds its staging root's lock"
    );

    let unmounted_at = unmount_external(&mnt);
    assert!(!is_mounted(&mnt), "the kernel mount is gone");

    // Law 1: the lock frees with the TEARDOWN, while the process lives on.
    let freed_at = loop {
        if !staging_root_owner_is_live(&root) {
            break Instant::now();
        }
        assert!(
            unmounted_at.elapsed() < Duration::from_secs(8),
            "the predecessor still holds its staging-root lock {:?} after the unmount \
             (alive={}) — the lock must release with the dismount teardown, never with \
             the process exit; log tail:\n{}",
            unmounted_at.elapsed(),
            first.alive(),
            first
                .log_text()
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .join("\n")
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        first.alive(),
        "the predecessor must still be alive when its lock frees (the seam holds its exit) \
         — otherwise this pin observed the process exit, not the teardown's release"
    );
    assert!(
        freed_at.duration_since(unmounted_at) < Duration::from_secs(8),
        "the release trails the unmount by the teardown alone"
    );

    // The successor, at once: the fstests `_scratch_unmount; _scratch_mount`
    // shape. It must land while the predecessor is still up.
    let second = spawn_mount(&meta, &mnt, &base.join("second.log"), 0);
    assert!(
        first.alive(),
        "the successor landed after the predecessor exited — the handover was not exercised"
    );
    assert_eq!(
        std::fs::read(mnt.join("f")).expect("read back through the successor"),
        b"predecessor bytes",
        "the successor serves the predecessor's durable bytes"
    );
    let second_log = second.log_text();
    assert!(
        !second_log.contains("HELD by a live process"),
        "the successor must never meet the collision refusal; log:\n{second_log}"
    );
    drop(second);
    assert!(!is_mounted(&mnt));
    let _ = first.child.wait();
    let _ = std::fs::remove_dir_all(&base);
}

/// Law 2 LIVE, in the fstests shape, composed with the `mount --daemon`
/// parent's readiness deadline (review round 1, Issues 2 and 5): the
/// predecessor's dismount teardown is held 34 s open before its data-plane close
/// (`SQUEEZEFS_TEST_DISMOUNT_HOLD_MS` — mount gone, staging root still its
/// own), `fusermount3 -u` returns at once, and a `--daemon` successor is
/// started IMMEDIATELY. It meets the held lock, classifies the holder as
/// dismounting (no FUSE mount at the mount point), waits past the 2-s
/// exit-grade bound AND past its parent's fixed 30-s readiness deadline
/// (the child reports the bound it waits under on the handshake pipe and
/// the parent stretches its deadline), lands when the teardown completes,
/// and serves the predecessor's bytes. RED on the round-1 build: the
/// successor was SIGKILLed by its own parent at 30 s ("mount did not
/// become ready within 30 seconds") while the predecessor was healthy
/// inside its guard. On the base the seam is an unregistered knob (the
/// teardown is instant and the successor lands inside the 2-s wait), so
/// the base RED is the `landed_after ≥ 30 s` and INFO assertions.
#[test]
fn a_daemon_successor_waits_out_a_dismounting_predecessor_past_the_parents_deadline() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("daemon");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");

    let mut first = spawn_mount_with_env(
        &meta,
        &mnt,
        &base.join("first.log"),
        &[("SQUEEZEFS_TEST_DISMOUNT_HOLD_MS", "34000")],
    );
    std::fs::write(mnt.join("f"), b"predecessor bytes").expect("write through the mount");
    let root = owned_staging_root(&base);

    let unmounted_at = unmount_external(&mnt);
    assert!(!is_mounted(&mnt), "the kernel mount is gone");
    assert!(
        staging_root_owner_is_live(&root),
        "the predecessor still owns its staging root inside its held teardown"
    );

    // The successor, at once, through the daemonizing parent. It is a
    // DETACHED daemon: the guard unmounts its path on every exit of this
    // test, so a failed assertion leaves no live mount behind.
    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    let _successor = Unmount(mnt.clone());
    let second_log = base.join("second.log");
    let out = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("--daemon")
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .arg("--log-file")
        .arg(&second_log)
        .output()
        .expect("run the successor mount --daemon");
    let landed_after = unmounted_at.elapsed();
    let console = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(&second_log).unwrap_or_default();
    assert!(
        out.status.success(),
        "the successor must land once the predecessor's teardown completes; console:\n\
         {console}\nlog tail:\n{}",
        log.lines().rev().take(15).collect::<Vec<_>>().join("\n")
    );
    assert!(
        landed_after >= Duration::from_secs(30) && landed_after < Duration::from_secs(60),
        "the successor waited out the 34-s held teardown, past its parent's 30-s deadline \
         (landed {landed_after:?} after the unmount)"
    );
    assert!(
        log.contains("still held by the previous mount's daemon"),
        "the successor announces the wait once; log:\n{log}"
    );
    assert!(
        !console.contains("not ready within 30 seconds") && !console.contains("HELD by a live"),
        "neither the parent's deadline nor the collision refusal may fire; console:\n{console}"
    );
    assert_eq!(
        std::fs::read(mnt.join("f")).expect("read back through the successor"),
        b"predecessor bytes",
        "the successor serves the predecessor's durable bytes"
    );
    assert!(
        wait_exit(&mut first.child, Duration::from_secs(30)).is_some(),
        "the predecessor exits once its held teardown completes"
    );

    // The successor is a detached daemon: unmount it (the same bounded
    // retry — the read-back above is a fresh touch) and wait for the
    // mount to go.
    unmount_external(&mnt);
    let deadline = Instant::now() + Duration::from_secs(30);
    while is_mounted(&mnt) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!is_mounted(&mnt), "the successor unmounts cleanly");
    let _ = std::fs::remove_dir_all(&base);
}
