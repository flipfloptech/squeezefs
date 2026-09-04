//! SIGTERM ends the mount: the daemon's OWN shutdown must unmount and exit
//! without anyone else touching the mountpoint.
//!
//! Found 2026-09-04 while probing the `umount` verb after the 1.2 release
//! gate: on EVERY mount (root or unprivileged) the daemon reached
//! "Dismount clean" within a second of SIGTERM and then sat, still mounted,
//! with the `fuse3-mount` session thread parked and no `fusermount3` /
//! `umount2` ever issued — until something external destroyed the FUSE
//! connection (the verb's kernel-side fallback, which is why the verb
//! still "worked" after its full SIGTERM timeout, and why the runners
//! never saw it). Cause: `MountHandle::unmount` woke the destroy
//! notification with ONE permit while the primary session and every
//! FUSE-over-io_uring queue worker (one per possible CPU) waited on the
//! SAME notify; one arbitrary worker woke and ran the dismount, the
//! primary session — whose completion the unmount awaits — never did.
//!
//! The contract below is the daemon-level shape: send SIGTERM, touch
//! nothing else, and the mount must be gone and the process exited within
//! a bound far below the verb's fallback window. Mount-class (self-skips
//! via the testkit where FUSE-over-io_uring is unavailable).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use squeezefs_testkit::{mount_supported, site};

const MIB: u64 = 1024 * 1024;

/// SIGTERM → process exit + mountpoint gone. The dismount itself is a
/// sub-second flush on an idle sandbox; the bound covers a loaded gate box
/// and stays well under the `umount` verb's 5 s SIGTERM window so the
/// verb's kernel-abort fallback can never be what makes this pass.
const EXIT_BOUND: Duration = Duration::from_secs(4);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_sigterm_{tag}_{}", std::process::id()));
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
        .set_len(1024 * MIB)
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

fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
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

fn is_mounted(mnt: &Path) -> bool {
    let needle = format!(" {} ", mnt.display());
    std::fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .any(|l| l.contains(&needle))
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

/// Contract: SIGTERM alone unmounts and exits the daemon within
/// [`EXIT_BOUND`]. Nothing else touches the mountpoint — the fork's
/// `fusermount3 -u` / `umount2` must be what removes the mount, and the
/// daemon's own log must record it ("Cleanly unmounted filesystem on
/// exit"), so a kernel-side abort by a bystander can never be the
/// mechanism that satisfies this test.
#[test]
fn sigterm_unmounts_and_exits_without_external_help() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("plain");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    let mut mount = spawn_mount(&meta, &mnt, &base.join("mount.log"));
    std::fs::write(mnt.join("f"), b"hello").expect("write through the mount");
    assert!(is_mounted(&mnt), "sandbox is mounted before SIGTERM");

    // SAFETY: SIGTERM to our own child.
    let rc = unsafe { libc::kill(mount.child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM): {}", std::io::Error::last_os_error());

    let exited_in = wait_exit(&mut mount.child, EXIT_BOUND);
    let log = mount.log_text();
    assert!(
        exited_in.is_some(),
        "daemon still alive {EXIT_BOUND:?} after SIGTERM (mounted={}); the destroy \
         notification must wake EVERY session — the primary whose completion the unmount \
         awaits, not one arbitrary queue worker. log tail:\n{}",
        is_mounted(&mnt),
        log.lines()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        !is_mounted(&mnt),
        "daemon exited in {:?} but the mountpoint is still mounted — the daemon never \
         issued its own unmount",
        exited_in.unwrap()
    );
    assert!(
        log.contains("Cleanly unmounted filesystem on exit"),
        "the daemon's own unmount must be what ended the mount; log:\n{log}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
