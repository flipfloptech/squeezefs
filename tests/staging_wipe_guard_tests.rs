//! ENG-4 — the staging-wipe guard (pre-RC spec §10, P0).
//!
//! `stamp_staging_dir` called `remove_dir_all` on an operator-supplied
//! path AS ROOT with no precondition — reachable from
//! `format --disk-cache-paths` and `config set-cache-paths`. One typo
//! (`--disk-cache-paths /home` instead of `/home/x/staging`) was an
//! unrecoverable recursive delete.
//!
//! Contracts pinned:
//! 1. **Non-empty directories that do not look like a previously used
//!    squeezefs staging root are refused** unless the caller passes the
//!    explicit consent flag (`--force`, spelled like `format --force`;
//!    `--yes` accepted as an alias on `config set-cache-paths`).
//! 2. **A curated system-path denylist is hard-refused** — consent never
//!    overrides it (`config_ops::STAGING_WIPE_DENYLIST` is the single
//!    source of the list).
//! 3. **The deletion plan is printed before acting** whenever existing
//!    content is about to be removed.
//! 4. **The wipe-for-clean-generation behavior survives** for
//!    legitimately-stamped staging roots: a root whose only content is
//!    the `squeezefs/` isolation container with generation-marked mount
//!    dirs below re-stamps without consent (the cache-path-policy
//!    contract, `tests/cache_path_policy_tests.rs`).
//! 5. Missing and empty directories keep stamping without consent
//!    (nothing is destroyed).
//!
//! CLI tests drive the real binary; library tests pin the guard helper
//! surface directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const GENERATION_MARKER: &str = ".squeezefs_generation";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_wipeguard_{tag}_{}", std::process::id()));
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

/// Create blank file-backed meta+data volumes (NOT formatted).
fn make_volumes(base: &Path) -> (PathBuf, PathBuf) {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    (meta, data)
}

/// Format with a declared staging dir (passes `--force` — blank volumes
/// do not need it, but keeps the helper reusable on re-formats).
fn format_volume(base: &Path, staging: &Path) -> PathBuf {
    let (meta, data) = make_volumes(base);
    std::fs::create_dir_all(staging).unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(staging)
        .arg("--force");
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "format");
    assert!(
        out.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    meta
}

fn set_cache_paths_cmd(meta: &Path, new_dir: &Path, consent: Option<&str>) -> Command {
    let mut cmd = Command::new(bin());
    cmd.arg("config")
        .arg("set-cache-paths")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(new_dir);
    if let Some(flag) = consent {
        cmd.arg(flag);
    }
    cmd
}

/// Seed a directory with junk that carries NO staging marker.
fn seed_junk(dir: &Path) {
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("stale_junk.bin"), b"poison").unwrap();
    std::fs::write(dir.join("nested/deep.bin"), b"poison2").unwrap();
}

/// Seed a directory shaped like a legitimately used staging root: the
/// `squeezefs/` isolation container with a generation-marked per-mount
/// dir (mirrors the mount-side layout the cache-path suite pins).
fn seed_marked_staging_root(dir: &Path) {
    let mount_dir = dir.join("squeezefs").join("home_user_mnt");
    std::fs::create_dir_all(&mount_dir).unwrap();
    std::fs::write(mount_dir.join(GENERATION_MARKER), b"v3:test-generation").unwrap();
    std::fs::write(mount_dir.join("leftover_segment.bin"), vec![7u8; 8192]).unwrap();
}

// ---------------------------------------------------------------------------
// Contract 1 — non-empty unmarked dirs refuse without consent (CLI).
// ---------------------------------------------------------------------------

#[test]
fn test_set_cache_paths_refuses_nonempty_unmarked_dir_without_consent() {
    let base = scratch("refuse_unmarked");
    let staging_a = base.join("staging_a");
    let meta = format_volume(&base, &staging_a);

    let staging_b = base.join("staging_b");
    seed_junk(&staging_b);

    let cmd = set_cache_paths_cmd(&meta, &staging_b, None);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "set-cache-paths (no consent)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "set-cache-paths naming a non-empty unmarked dir must refuse \
         without consent\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(staging_b.to_str().unwrap()) && stderr.contains("--force"),
        "the refusal must name the directory and the consent remedy; stderr:\n{stderr}"
    );
    assert!(
        staging_b.join("stale_junk.bin").exists() && staging_b.join("nested/deep.bin").exists(),
        "a refused wipe must leave the directory contents untouched"
    );

    // The config must NOT have been rewritten: still the old root.
    let mut cmd = Command::new(bin());
    cmd.arg("config")
        .arg("get-cache-paths")
        .arg(format!("sqmeta://{}", meta.display()));
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "get-cache-paths");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success()
            && stdout.contains(staging_a.to_str().unwrap())
            && !stdout.contains(staging_b.to_str().unwrap()),
        "a refused set-cache-paths must leave the recorded paths unchanged; stdout:\n{stdout}"
    );

    // A marked root with FOREIGN top-level content beside the container
    // is NOT recognized: refusal protects the foreign bytes.
    let staging_c = base.join("staging_c");
    seed_marked_staging_root(&staging_c);
    std::fs::write(staging_c.join("foreign_top_level.txt"), b"not ours").unwrap();
    let cmd = set_cache_paths_cmd(&meta, &staging_c, None);
    let (out, _) = run_with_deadline(
        cmd,
        Duration::from_secs(60),
        "set-cache-paths (foreign content beside container)",
    );
    assert!(
        !out.status.success() && staging_c.join("foreign_top_level.txt").exists(),
        "foreign top-level content beside the staging container must refuse \
         without consent and stay intact\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contracts 1+3 — consent wipes, and the deletion plan prints first.
// ---------------------------------------------------------------------------

#[test]
fn test_set_cache_paths_consent_wipes_and_prints_deletion_plan() {
    let base = scratch("consent_wipes");
    let staging_a = base.join("staging_a");
    let meta = format_volume(&base, &staging_a);

    // --force (the house consent spelling, matching `format --force`).
    let staging_b = base.join("staging_b");
    seed_junk(&staging_b);
    let cmd = set_cache_paths_cmd(&meta, &staging_b, Some("--force"));
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "set-cache-paths --force");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "set-cache-paths --force must wipe a junk dir after consent\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !staging_b.join("stale_junk.bin").exists(),
        "consented wipe must remove the junk"
    );
    assert!(
        stdout.contains("wipe plan") && stdout.contains("stale_junk.bin"),
        "the deletion plan must print BEFORE acting, naming what is removed; \
         stdout:\n{stdout}"
    );

    // --yes is accepted as an alias for the same consent.
    let staging_c = base.join("staging_c");
    seed_junk(&staging_c);
    let cmd = set_cache_paths_cmd(&meta, &staging_c, Some("--yes"));
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "set-cache-paths --yes");
    assert!(
        out.status.success() && !staging_c.join("stale_junk.bin").exists(),
        "--yes must be accepted as the consent alias\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 4 — legitimately-stamped staging roots re-stamp without consent.
// ---------------------------------------------------------------------------

#[test]
fn test_marked_staging_root_restamps_without_consent() {
    let base = scratch("marked_restamp");
    let staging_a = base.join("staging_a");
    let meta = format_volume(&base, &staging_a);

    let staging_b = base.join("staging_b");
    seed_marked_staging_root(&staging_b);

    let cmd = set_cache_paths_cmd(&meta, &staging_b, None);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "set-cache-paths (marked)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a generation-marked staging root must re-stamp WITHOUT consent \
         (the wipe-for-clean-generation contract)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        staging_b.is_dir() && std::fs::read_dir(&staging_b).unwrap().count() == 0,
        "the re-stamped root must come up empty (fresh staging generation)"
    );
    assert!(
        stdout.contains("wipe plan"),
        "even the marker-sanctioned wipe must print its plan first; stdout:\n{stdout}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 2 — the system-path denylist is hard-refused, consent or not.
// ---------------------------------------------------------------------------

#[test]
fn test_denylist_system_paths_hard_refused_even_with_consent() {
    let base = scratch("denylist_cli");
    let staging_a = base.join("staging_a");
    let meta = format_volume(&base, &staging_a);

    let cmd = set_cache_paths_cmd(&meta, Path::new("/usr"), Some("--force"));
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "set-cache-paths /usr --force");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "set-cache-paths /usr must be hard-refused even with --force\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("/usr") && stderr.contains("protected system path"),
        "the denylist refusal must name the path and the rule; stderr:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// Library-level denylist matrix: every curated entry (and trailing-slash
/// / prefix-of-an-entry spellings) refuses with consent=true, BEFORE any
/// filesystem mutation; ordinary user paths stay allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_denylist_matrix_library_level() {
    for denied in [
        "/", "/home", "/etc", "/usr", "/var", "/boot", "/root", "/usr/", "/etc/",
    ] {
        let err = squeezefs::config_ops::stamp_staging_dir(Path::new(denied), true)
            .await
            .expect_err(&format!("stamping {denied} must be hard-refused"));
        let msg = err.to_string();
        assert!(
            msg.contains("protected system path"),
            "{denied}: the refusal must state the denylist rule, got: {msg}"
        );
    }

    // The const is the single source of the list — pin its contents.
    assert_eq!(
        squeezefs::config_ops::STAGING_WIPE_DENYLIST,
        &["/", "/home", "/etc", "/usr", "/var", "/boot", "/root"],
        "the curated denylist lives in ONE const"
    );

    // An ordinary user path is NOT denylisted (and stamps fine).
    let base = scratch("denylist_ok");
    let fine = base.join("staging_ok");
    seed_junk(&fine);
    squeezefs::config_ops::stamp_staging_dir(&fine, true)
        .await
        .expect("a user path with consent must stamp");
    assert_eq!(
        std::fs::read_dir(&fine).unwrap().count(),
        0,
        "consented stamp wipes to empty"
    );
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 5 — missing/empty dirs and marked roots need no consent (library).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_missing_empty_and_marked_dirs_stamp_without_consent() {
    let base = scratch("no_consent_needed");

    // Missing: created.
    let missing = base.join("missing");
    squeezefs::config_ops::stamp_staging_dir(&missing, false)
        .await
        .expect("a missing dir must stamp without consent");
    assert!(missing.is_dir());

    // Empty: fine.
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    squeezefs::config_ops::stamp_staging_dir(&empty, false)
        .await
        .expect("an empty dir must stamp without consent");

    // Marked staging root: wiped without consent (contract 4, library face).
    let marked = base.join("marked");
    seed_marked_staging_root(&marked);
    squeezefs::config_ops::stamp_staging_dir(&marked, false)
        .await
        .expect("a generation-marked root must stamp without consent");
    assert_eq!(std::fs::read_dir(&marked).unwrap().count(), 0);

    // Unmarked junk without consent: refused, named, intact.
    let junk = base.join("junk");
    seed_junk(&junk);
    let err = squeezefs::config_ops::stamp_staging_dir(&junk, false)
        .await
        .expect_err("unmarked junk without consent must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("junk") && msg.contains("--force"),
        "the refusal must name the dir and the consent remedy, got: {msg}"
    );
    assert!(
        junk.join("stale_junk.bin").exists(),
        "refusal must not delete"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// format --disk-cache-paths rides the same guard (consent = format --force).
// ---------------------------------------------------------------------------

#[test]
fn test_format_refuses_nonempty_unmarked_staging_without_force() {
    let base = scratch("format_guard");
    let (meta, data) = make_volumes(&base);
    let staging = base.join("staging");
    seed_junk(&staging);

    // Without --force: blank volumes would format fine — the refusal must
    // come from the staging guard, before any volume is touched.
    let mut cmd = Command::new(bin());
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "format (no --force)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "format naming a non-empty unmarked staging dir must refuse without \
         --force\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let console = format!("{stdout}{stderr}");
    assert!(
        console.contains(staging.to_str().unwrap()) && console.contains("--force"),
        "the format refusal must name the directory and the consent remedy; \
         output:\n{console}"
    );
    assert!(
        staging.join("stale_junk.bin").exists(),
        "a refused format must leave the staging contents untouched"
    );

    // With --force: consent given — format succeeds and the junk is wiped.
    let mut cmd = Command::new(bin());
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(&staging)
        .arg("--force");
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "format --force");
    assert!(
        out.status.success(),
        "format --force must proceed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !staging.join("stale_junk.bin").exists(),
        "format --force must wipe the consented staging dir"
    );

    let _ = std::fs::remove_dir_all(&base);
}
