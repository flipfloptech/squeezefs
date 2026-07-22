//! Mountpoint preflight + daemon-handshake contracts (live-diagnosed,
//! 2026-07-10).
//!
//! Repro: `touch $MNT/stray; squeezefs mount ... $MNT --daemon` — the child
//! daemon refused the non-empty mountpoint and exited early, while the
//! parent discarded the collected error and printed the misleading
//! "The mount point is not ready in 30 seconds, exiting". The user never
//! learned WHY the mount failed.
//!
//! Contracts pinned here:
//! 1. The mountpoint is validated in the PARENT, before daemonizing and
//!    before any volume is touched: a non-empty / missing / non-directory
//!    mountpoint fails INSTANTLY (< 5 s) with a precise error naming the
//!    mountpoint (and the stray entry) — in `--daemon` AND foreground
//!    mode. Nothing is mounted, no daemon is left behind.
//! 2. When the daemonized child exits before readiness for any other
//!    reason, the parent surfaces the child's actual exit reason (pipe
//!    content / log tail) instead of the generic timeout line.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_preflight_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Run a CLI invocation with a hard deadline; a hang is converted into a
/// loud failure instead of wedging the suite. Returns (output, elapsed).
fn run_with_deadline(mut cmd: Command, deadline: Duration, what: &str) -> (Output, Duration) {
    let start = Instant::now();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if start.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what} did not exit within {deadline:?} — must fail fast and loud");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let elapsed = start.elapsed();
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("collect {what} output: {e}"));
    (out, elapsed)
}

/// Format one meta + one data volume (staging declared at format).
fn format_volume(base: &Path) -> PathBuf {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let out = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging)
        .arg("--force")
        .output()
        .expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    meta
}

fn mount_cmd(meta: &Path, mnt: &Path, daemon: bool, log: Option<&Path>) -> Command {
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt);
    if daemon {
        cmd.arg("--daemon");
    }
    if let Some(log) = log {
        cmd.arg("--log-file").arg(log);
    }
    cmd
}

/// The combined output a user sees on the console (stdout + stderr).
fn console(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ---------------------------------------------------------------------------
// Contract 1 — non-empty mountpoint: LOUD and INSTANT, daemon + foreground.
// ---------------------------------------------------------------------------

#[test]
fn test_nonempty_mountpoint_fails_instant_and_precise() {
    let base = scratch("nonempty");
    let meta = format_volume(&base);
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    std::fs::write(mnt.join("stray"), b"leftover").unwrap();

    for daemon in [true, false] {
        let variant = if daemon {
            "--daemon non-empty mountpoint"
        } else {
            "foreground non-empty mountpoint"
        };
        let (out, elapsed) = run_with_deadline(
            mount_cmd(&meta, &mnt, daemon, None),
            Duration::from_secs(40),
            variant,
        );
        let text = console(&out);
        assert!(
            !out.status.success(),
            "{variant}: must fail, but exited success\n{text}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "{variant}: refusal must be instant (< 5s), took {elapsed:?}\n{text}"
        );
        assert!(
            text.contains("is not empty"),
            "{variant}: error must say the mountpoint is not empty; got:\n{text}"
        );
        assert!(
            text.contains(mnt.to_str().unwrap()),
            "{variant}: error must name the mountpoint; got:\n{text}"
        );
        assert!(
            text.contains("stray"),
            "{variant}: error must name the offending entry; got:\n{text}"
        );
        assert!(
            text.contains("refusing to mount"),
            "{variant}: error must state the refusal; got:\n{text}"
        );
        assert!(
            !text.contains("not ready in 30 seconds"),
            "{variant}: the misleading generic timeout line must be gone; got:\n{text}"
        );
        assert!(
            !mnt.join(".stats").exists(),
            "{variant}: nothing may be mounted after the refusal"
        );
    }

    // The mountpoint content must be untouched by the refused mounts.
    assert_eq!(
        std::fs::read(mnt.join("stray")).unwrap(),
        b"leftover",
        "a refused mount must leave the mountpoint contents alone"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn test_missing_and_nondir_mountpoint_fail_instant_and_precise() {
    let base = scratch("badmnt");
    let meta = format_volume(&base);

    // Missing mountpoint.
    let missing = base.join("does_not_exist");
    let (out, elapsed) = run_with_deadline(
        mount_cmd(&meta, &missing, true, None),
        Duration::from_secs(40),
        "--daemon missing mountpoint",
    );
    let text = console(&out);
    assert!(
        !out.status.success() && text.contains("does not exist"),
        "missing mountpoint must fail with a precise error; got:\n{text}"
    );
    assert!(
        text.contains(missing.to_str().unwrap()),
        "missing-mountpoint error must name the path; got:\n{text}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "missing-mountpoint refusal took {elapsed:?}"
    );

    // Mountpoint is a regular file.
    let file_mnt = base.join("file_mountpoint");
    std::fs::write(&file_mnt, b"x").unwrap();
    let (out, elapsed) = run_with_deadline(
        mount_cmd(&meta, &file_mnt, true, None),
        Duration::from_secs(40),
        "--daemon file mountpoint",
    );
    let text = console(&out);
    assert!(
        !out.status.success() && text.contains("not a directory"),
        "file mountpoint must fail with a precise error; got:\n{text}"
    );
    assert!(
        text.contains(file_mnt.to_str().unwrap()),
        "file-mountpoint error must name the path; got:\n{text}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "file-mountpoint refusal took {elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 2 — child-early-exit surfaces the child's actual reason.
// ---------------------------------------------------------------------------

/// A daemonized mount whose child fails AFTER the fork (here: an
/// unformatted metadata volume, which the child's bootstrap probe refuses)
/// must surface the child's exit reason on the parent's console — not the
/// generic "Child process exited early." / 30 s timeout lines.
#[test]
fn test_daemon_child_exit_reason_is_surfaced() {
    let base = scratch("childreason");
    let blank = base.join("blank.bin");
    std::fs::File::create(&blank)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let log = base.join("mount.log");

    let (out, elapsed) = run_with_deadline(
        mount_cmd(&blank, &mnt, true, Some(&log)),
        Duration::from_secs(40),
        "--daemon unformatted meta",
    );
    let text = console(&out);
    assert!(
        !out.status.success(),
        "mounting an unformatted volume must fail\n{text}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "child-failure surfacing must not wait out the 30s handshake; took {elapsed:?}"
    );
    assert!(
        text.contains("not formatted"),
        "the parent must surface the child's actual refusal (\"not formatted\"); got:\n{text}"
    );
    assert!(
        !text.contains("not ready in 30 seconds"),
        "the misleading generic timeout line must be gone; got:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// fstests generic/128 repro-port (VL10 release gate; kernel-interface
/// half documented in the fix commit): `-o nosuid`/`nodev`/`noexec` were
/// silently DROPPED — `filter_kernel_mount_options` passed neither the
/// option strings nor the MS_* flags, so a `mount -o nosuid` produced a
/// suid-honoring mount and generic/128's fsgqa `ls` of a 0700 root dir
/// succeeded through the setuid binary. The daemon-side contract pinned
/// here: the option tokens parse into the mount security flags exactly.
#[test]
fn mount_security_flags_parse_the_posix_mount_tokens() {
    use squeezefs::fuse_client::mount_security_flags;

    assert_eq!(mount_security_flags(""), (false, false, false));
    assert_eq!(
        mount_security_flags("fsname=/dev/x,rw"),
        (false, false, false)
    );
    assert_eq!(
        mount_security_flags("nosuid"),
        (true, false, false),
        "generic/128's exact shape"
    );
    assert_eq!(
        mount_security_flags("rw, nosuid ,nodev"),
        (true, true, false),
        "whitespace + companions"
    );
    assert_eq!(
        mount_security_flags("noexec,nosuid,nodev,max_read=1048576"),
        (true, true, true)
    );
    // Never confused with value-carrying or prefixed keys.
    assert_eq!(
        mount_security_flags("nosuidX,nodev=1"),
        (false, false, false)
    );
}
