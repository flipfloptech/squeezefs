//! ENG-3 — the daemon must be audible (pre-RC spec §10, P0).
//!
//! Root cause pinned here: `env_logger::Builder::from_default_env()` with
//! no default filter means a stock invocation (no `RUST_LOG`) logs at
//! **Error** only — reservation preemptions, the job-wire security
//! notice, shard lease expiries, the O_DIRECT→buffered degradation, the
//! checkpoint bitmap-write failure and meta-volume teardown failures were
//! all silently invisible. Compounding: a failed `--log-file` open was
//! swallowed (`if let Ok(file)` with no else), and daemonized stdio is
//! already `/dev/null` — the daemon ran with ALL logging discarded.
//!
//! Contracts pinned:
//! 1. **Default filter is `info`** when `RUST_LOG` is unset: an ordinary
//!    CLI run emits its `log::info!` lines on stderr.
//! 2. **Explicit `RUST_LOG` wins verbatim** (the env-knob convention):
//!    `RUST_LOG=warn` suppresses the same info line.
//! 3. **A failed `--log-file` open FAILS the command loudly** — never a
//!    silent fallback to discarded logging — in foreground AND `--daemon`
//!    mode, instantly (< 5 s), naming the log file.
//!
//! All tests drive the real binary (`CARGO_BIN_EXE_squeezefs`); no mount
//! transport is required (the contracts fire before any volume/FUSE work).

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
        .join(format!("sqfs_logging_{tag}_{}", std::process::id()));
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

/// A `format` invocation over fresh file-backed volumes with a declared
/// staging dir: the cheapest real CLI run that deterministically crosses
/// a `log::info!` site (the staging-dir stamping line).
fn format_cmd(base: &Path) -> Command {
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
    let mut cmd = Command::new(bin());
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging)
        .arg("--force");
    cmd
}

/// The deterministic info-level line the format run must cross.
const STAMP_INFO_LINE: &str = "Stamping local staging/cache directory";

// ---------------------------------------------------------------------------
// Contract 1 — default filter is `info` when RUST_LOG is unset.
// ---------------------------------------------------------------------------

#[test]
fn test_default_log_filter_is_info_when_rust_log_unset() {
    let base = scratch("default_info");
    let mut cmd = format_cmd(&base);
    cmd.env_remove("RUST_LOG");
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "format (no RUST_LOG)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "format must succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(STAMP_INFO_LINE),
        "with RUST_LOG unset the default filter must be `info`: the \
         `log::info!` stamping line must be visible on stderr (ENG-3 — the \
         stock daemon logged at Error only).\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 2 — explicit RUST_LOG wins verbatim over the info default.
// ---------------------------------------------------------------------------

#[test]
fn test_explicit_rust_log_overrides_the_info_default() {
    let base = scratch("override_wins");
    let mut cmd = format_cmd(&base);
    cmd.env("RUST_LOG", "warn");
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "format (RUST_LOG=warn)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "format must succeed under RUST_LOG=warn\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(STAMP_INFO_LINE),
        "RUST_LOG=warn must win verbatim over the info default (explicit \
         env always wins — the env-knob convention); the info-level \
         stamping line leaked through.\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 3 — a failed --log-file open FAILS the mount loudly.
// ---------------------------------------------------------------------------

/// Foreground and `--daemon` mount with an unopenable `--log-file` target
/// (missing parent directory) must exit nonzero, instantly, with an error
/// that NAMES the log file — never proceed with logging discarded.
#[test]
fn test_mount_refuses_unopenable_log_file() {
    let base = scratch("logfile_refusal");
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    // Parent dir does not exist: OpenOptions::create does not mkdir -p,
    // so the open fails deterministically with NotFound.
    let bad_log = base.join("no_such_dir").join("daemon.log");
    // The meta volume is deliberately bogus: the log-file preflight must
    // fire FIRST — the refusal below must be about the log file, proving
    // no volume (and no daemonization) is touched with logging broken.
    let bogus_meta = format!("sqmeta://{}", base.join("does_not_exist.bin").display());

    for (variant, daemon) in [("foreground", false), ("--daemon", true)] {
        let mut cmd = Command::new(bin());
        cmd.arg("mount")
            .arg(&bogus_meta)
            .arg(&mnt)
            .arg("--log-file")
            .arg(&bad_log);
        if daemon {
            cmd.arg("--daemon");
        }
        let (out, elapsed) = run_with_deadline(
            cmd,
            Duration::from_secs(10),
            &format!("mount with unopenable --log-file ({variant})"),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success(),
            "{variant}: a mount whose --log-file cannot be opened must FAIL \
             (never run with all logging discarded)\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "{variant}: the log-file refusal must be instant (< 5 s), took {elapsed:?}"
        );
        assert!(
            stderr.contains("--log-file") && stderr.contains(bad_log.to_str().unwrap()),
            "{variant}: the refusal must name --log-file and the path (loud, \
             actionable); stderr:\n{stderr}"
        );
        assert!(
            !mnt.join(".stats").exists(),
            "{variant}: a refused mount must not leave a live filesystem behind"
        );
    }

    // A directory as the --log-file target is equally unopenable (EISDIR).
    let dir_as_log = base.join("dir_as_log");
    std::fs::create_dir_all(&dir_as_log).unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(&bogus_meta)
        .arg(&mnt)
        .arg("--log-file")
        .arg(&dir_as_log);
    let (out, _) = run_with_deadline(
        cmd,
        Duration::from_secs(10),
        "mount with a directory as --log-file",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("--log-file"),
        "a directory --log-file target must refuse loudly; stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
