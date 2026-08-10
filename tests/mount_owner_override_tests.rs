//! Staging/cache-dir OWNERSHIP contracts (user-hit papercut, 2026-07-13).
//!
//! A user ran `sudo squeezefs format … --disk-cache-paths ~/.squeeze/` and
//! the staging roots came out stamped with raw `getuid()` ownership — root
//! under sudo, `SUDO_UID`/`SUDO_GID` ignored. Their later USER-MODE mount
//! then hit EACCES on its own staging and they had to `chown -R` by hand.
//! This suite pins the decided contract (recreated fresh from the prior
//! agent's cleaned red-WIP, per
//! `.benchmarks/2026-07-13-rootd-eio-residue-history-exclusion.md`):
//!
//! 1. **format under sudo** (`SUDO_UID`/`SUDO_GID` present) creates the
//!    staging/cache roots owned by the INVOKING user — a user-mode mount
//!    just works, no manual chown.
//! 2. **format as genuine root** (no `SUDO_*` env) keeps root ownership —
//!    a real root deployment is not second-guessed.
//! 3. **mount `--uid`/`--gid` are FUSE-presentation-only** and grant no
//!    staging access: when the format-declared staging roots are
//!    unwritable by the daemon identity (the user that runs
//!    `squeezefs mount`), the mount preflight FAILS LOUD with a chown
//!    remedy instead of surfacing a raw EACCES mid-bootstrap. Mount stays
//!    side-effect-free on the staging roots (cache-path policy): no chown
//!    magic at mount time.
//! 4. **`config set-cache-paths`** (the admin op that wipes + stamps new
//!    dirs) follows the same SUDO_UID ownership rule as format.
//!
//! All behavioral tests drive the real binary (`CARGO_BIN_EXE_squeezefs`).
//! The sudo legs skip cleanly when passwordless sudo is unavailable; the
//! mount legs skip where FUSE-over-io_uring cannot run.

use squeezefs_testkit::{mount_supported, site};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = ".squeezefs_generation";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline; this effort's rail: `~/tmp/ownfix_*`).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("ownfix_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Best-effort cleanup that survives root-owned residue on a RED run.
fn cleanup(base: &Path) {
    if std::fs::remove_dir_all(base).is_err() {
        let _ = Command::new("sudo")
            .args(["-n", "rm", "-rf"])
            .arg(base)
            .status();
    }
}

fn owner(path: &Path) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).unwrap_or_else(|e| panic!("stat {path:?}: {e}"));
    (md.uid(), md.gid())
}

fn me() -> (u32, u32) {
    (unsafe { libc::getuid() }, unsafe { libc::getgid() })
}

/// The sudo-shape contracts need a non-root invoker with passwordless
/// sudo. Both halves are ledgered skip classes (TEST-2) so a gate run
/// can promote either to a failure.
fn sudo_available(site: squeezefs_testkit::Site) -> bool {
    squeezefs_testkit::non_root(site, "the sudo-shape contracts")
        && squeezefs_testkit::passwordless_sudo(site)
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

/// Create one file-backed meta + data volume pair (as the CURRENT user, so
/// the volume files stay user-writable regardless of who formats them).
fn make_volumes(base: &Path) -> (PathBuf, PathBuf) {
    let meta = base.join("meta.bin");
    let data = base.join("data.bin");
    std::fs::File::create(&meta)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    std::fs::File::create(&data)
        .unwrap()
        .set_len(2 * 1024 * 1024 * 1024)
        .unwrap();
    (meta, data)
}

/// `format` invocation shapes:
/// - `Invoker::User`      — plain user-mode format.
/// - `Invoker::Sudo`      — `sudo -n squeezefs format …` (SUDO_* present).
/// - `Invoker::PlainRoot` — `sudo -n env -u SUDO_UID -u SUDO_GID … squeezefs
///   format …`: root WITHOUT the sudo markers, i.e. a genuine root shell.
enum Invoker {
    User,
    Sudo,
    PlainRoot,
}

fn format_volumes(invoker: Invoker, meta: &Path, data: &Path, staging: &[&Path]) -> Output {
    let mut cmd = match invoker {
        Invoker::User => Command::new(bin()),
        Invoker::Sudo => {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(bin());
            c
        }
        Invoker::PlainRoot => {
            let mut c = Command::new("sudo");
            c.args([
                "-n",
                "env",
                "-u",
                "SUDO_UID",
                "-u",
                "SUDO_GID",
                "-u",
                "SUDO_USER",
            ])
            .arg(bin());
            c
        }
    };
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force");
    if !staging.is_empty() {
        let joined = staging
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",");
        cmd.arg("--disk-cache-paths").arg(joined);
    }
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(120), "squeezefs format");
    out
}

fn assert_format_ok(out: &Output) {
    assert!(
        out.status.success(),
        "format failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn unmount(&mut self) {
        // FUSE RELEASE/writeback on the test's just-closed fds is async,
        // so early attempts can be transiently EBUSY — retry, capturing
        // the noise (evidence only if the LAST attempt still failed).
        let mut unmounted = false;
        let mut last_err = String::new();
        for _ in 0..10 {
            let out = Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .output()
                .expect("run fusermount3 -u");
            if out.status.success() {
                unmounted = true;
                break;
            }
            last_err = String::from_utf8_lossy(&out.stderr).into_owned();
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(
            unmounted,
            "fusermount3 -u {:?} failed after 10 attempts: {last_err}",
            self.mnt
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                // A clean unmount must be a CLEAN daemon exit — a
                // panic/abort during teardown is a bug an `ok` verdict
                // must not absorb.
                assert!(
                    status.success(),
                    "mount daemon exited {status} on unmount (teardown crash); log:\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        panic!("mount daemon did not exit within 30s of unmount");
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // Drop-time double-unmount: already-unmounted is the EXPECTED case —
        // silence the mtab noise; the explicit unmount path stays loud.
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a USER-MODE `squeezefs mount <meta> <mnt>` (with the presentation
/// `--uid`/`--gid` the user recipe passes) and wait for the stats inode.
fn spawn_user_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
    std::fs::create_dir_all(mnt).unwrap();
    let logf = std::fs::File::create(log).unwrap();
    let child = Command::new(bin())
        .arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .stdout(Stdio::from(logf.try_clone().unwrap()))
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
            "user-mode mount did not become ready in 90s (the sudo-format \
             ownership papercut EACCES shape?); log:\n{}",
            std::fs::read_to_string(&mount.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    mount
}

fn stats_json(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".stats")).expect("read .stats");
    serde_json::from_str(&raw).expect(".stats must be valid JSON")
}

/// Mirror of the mount-time staging isolation naming (`src/main.rs`):
/// non-alphanumeric → '_', collapse runs, trim ends; isolated dir =
/// `<staging>/squeezefs/<sanitized>`.
fn isolated_staging_dir(staging: &Path, mnt: &Path) -> PathBuf {
    let sanitized: String = mnt
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let mut clean = String::new();
    let mut last_us = false;
    for c in sanitized.chars() {
        if c == '_' {
            if !last_us {
                clean.push(c);
                last_us = true;
            }
        } else {
            clean.push(c);
            last_us = false;
        }
    }
    let clean = clean.trim_matches('_');
    staging.join("squeezefs").join(clean)
}

fn write_read_delete(mnt: &Path, name: &str, len: usize, seed: u8) {
    let path = mnt.join(name);
    let payload: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_add(seed)).collect();
    {
        let mut f =
            std::fs::File::create(&path).unwrap_or_else(|e| panic!("create {name} ({len} B): {e}"));
        f.write_all(&payload)
            .unwrap_or_else(|e| panic!("write {name} ({len} B): {e}"));
        f.sync_all()
            .unwrap_or_else(|e| panic!("fsync {name} ({len} B): {e}"));
    }
    {
        let mut f =
            std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {name} for read: {e}"));
        let mut back = Vec::with_capacity(len);
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_to_end(&mut back)
            .unwrap_or_else(|e| panic!("read {name}: {e}"));
        assert_eq!(back, payload, "{name}: read-back bytes mismatch");
    }
    std::fs::remove_file(&path).unwrap_or_else(|e| panic!("delete {name}: {e}"));
}

// ---------------------------------------------------------------------------
// Contract 1 — the CLI smoke: sudo format stamps the INVOKING user; a
// user-mode mount then just works, no manual chown.
// ---------------------------------------------------------------------------

/// `sudo squeezefs format … --disk-cache-paths a,b` (SUDO_UID/SUDO_GID set
/// by sudo itself) must leave BOTH a pre-existing and a missing-at-format
/// staging root owned by the invoking user — and a subsequent user-mode
/// mount must come up and route staged-window writes through that staging
/// WITHOUT any chown in between (the exact user-hit recipe).
#[test]
fn test_sudo_format_stamps_invoking_user_then_user_mount_works_without_chown() {
    if !sudo_available(site!()) || !mount_supported(site!()) {
        return;
    }
    let base = scratch("sudo_smoke");
    let staging_pre = base.join("staging_pre");
    let staging_new = base.join("staging_new");
    std::fs::create_dir_all(&staging_pre).unwrap();
    let (meta, data) = make_volumes(&base);

    let out = format_volumes(Invoker::Sudo, &meta, &data, &[&staging_pre, &staging_new]);
    assert_format_ok(&out);

    let expect = me();
    for (dir, tag) in [
        (&staging_pre, "pre-existing"),
        (&staging_new, "missing-at-format"),
    ] {
        assert!(
            dir.is_dir(),
            "sudo format must create/keep the {tag} staging root {dir:?}"
        );
        assert_eq!(
            owner(dir),
            expect,
            "sudo format must stamp the {tag} staging root {dir:?} with the \
             INVOKING user's ownership (SUDO_UID:SUDO_GID = {expect:?}), not \
             raw getuid() (= root under sudo)"
        );
    }

    // The payoff: a user-mode mount uses its own staging with NO chown.
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_user_mount(&meta, &mnt, &log);

    // The filesystem ROOT INODE must also belong to the invoking user
    // (same raw-getuid() bug one layer deeper: a 0:0 root dir +
    // default_permissions EACCES'd every user-mode create).
    assert_eq!(
        owner(&mnt),
        expect,
        "sudo format must stamp the root inode with the INVOKING user \
         (SUDO_UID:SUDO_GID), or user-mode mounts cannot create anything"
    );

    let isolated = isolated_staging_dir(&staging_pre, &mnt);
    assert!(
        isolated.is_dir() && isolated.join(MARKER).is_file(),
        "the user-mode daemon must generation-stamp its isolated staging dir \
         under the sudo-formatted root: {isolated:?}"
    );

    // A staged-window write (16 KiB: > inline, < block size) must actually
    // ride the staging tier — proving the dirs are usable, not just present.
    write_read_delete(&mnt, "staged_probe.bin", 16 * 1024, 0x5A);
    std::fs::write(mnt.join("staged_resident.bin"), vec![0xA5u8; 16 * 1024])
        .expect("staged-window write on the sudo-formatted staging");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let staged = stats_json(&mnt)["metrics"]["layout_staged_writes"]
            .as_u64()
            .expect("layout_staged_writes");
        if staged >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "staged-window writes never took the staged layout on the \
             sudo-formatted staging roots"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    mount.unmount();
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// Contract 2 — genuine root (no SUDO_* env) keeps root ownership.
// ---------------------------------------------------------------------------

/// A format run by a REAL root identity (no `SUDO_UID`/`SUDO_GID` in the
/// environment — here: sudo + `env -u`) must keep the staging roots owned
/// by root: a deliberate root deployment is never second-guessed.
#[test]
fn test_genuine_root_format_keeps_root_ownership() {
    if !sudo_available(site!()) {
        return;
    }
    let base = scratch("plain_root");
    let staging_pre = base.join("staging_pre");
    let staging_new = base.join("staging_new");
    std::fs::create_dir_all(&staging_pre).unwrap();
    let (meta, data) = make_volumes(&base);

    let out = format_volumes(
        Invoker::PlainRoot,
        &meta,
        &data,
        &[&staging_pre, &staging_new],
    );
    assert_format_ok(&out);

    for (dir, tag) in [
        (&staging_pre, "pre-existing"),
        (&staging_new, "missing-at-format"),
    ] {
        assert!(
            dir.is_dir(),
            "genuine-root format must create/keep the {tag} staging root {dir:?}"
        );
        assert_eq!(
            owner(dir),
            (0, 0),
            "genuine-root format (no SUDO_* env) must keep the {tag} staging \
             root {dir:?} root-owned"
        );
    }

    cleanup(&base);
}

// ---------------------------------------------------------------------------
// Contract 4 — `config set-cache-paths` follows the same SUDO_UID rule.
// ---------------------------------------------------------------------------

/// The admin op that wipes + stamps NEW cache dirs must apply the same
/// invoking-user ownership rule as format when run under sudo.
#[test]
fn test_sudo_set_cache_paths_stamps_invoking_user() {
    if !sudo_available(site!()) {
        return;
    }
    let base = scratch("sudo_setpaths");
    let staging_a = base.join("staging_a");
    std::fs::create_dir_all(&staging_a).unwrap();
    let (meta, data) = make_volumes(&base);
    let out = format_volumes(Invoker::User, &meta, &data, &[&staging_a]);
    assert_format_ok(&out);

    let staging_b = base.join("staging_b");
    let mut cmd = Command::new("sudo");
    cmd.args(["-n", bin(), "config", "set-cache-paths"])
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&staging_b);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(60), "sudo set-cache-paths");
    assert!(
        out.status.success(),
        "sudo set-cache-paths failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let expect = me();
    assert!(
        staging_b.is_dir(),
        "set-cache-paths must create the new staging root {staging_b:?}"
    );
    assert_eq!(
        owner(&staging_b),
        expect,
        "sudo set-cache-paths must stamp the new staging root {staging_b:?} \
         with the INVOKING user's ownership (SUDO_UID:SUDO_GID = {expect:?}), \
         not root"
    );

    cleanup(&base);
}

// ---------------------------------------------------------------------------
// Contract 3 — mount preflight fails LOUD (chown remedy) on staging roots
// unwritable by the daemon identity; --uid/--gid never paper over it.
// ---------------------------------------------------------------------------

/// When the format-declared staging root is unwritable by the daemon
/// identity (simulated unprivileged: write bit stripped — the same access
/// failure shape as a root-owned root + user daemon), `mount` must refuse
/// FAST and LOUD, naming the offending directory and a chown remedy —
/// never a raw `Permission denied (os error 13)` from deep inside
/// bootstrap, and never a silent chown at mount time (the mountpoint must
/// stay unmounted, the staging root untouched).
#[test]
fn test_mount_preflight_fails_loud_with_chown_remedy_on_unwritable_staging() {
    use std::os::unix::fs::PermissionsExt;
    if !squeezefs_testkit::non_root(site!(), "the permission-simulation contract") {
        return;
    }
    let base = scratch("preflight");
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (meta, data) = make_volumes(&base);
    let out = format_volumes(Invoker::User, &meta, &data, &[&staging]);
    assert_format_ok(&out);

    // Simulate the user-hit shape: the daemon identity cannot write the
    // staging root (root-owned dir ⇒ user sees r-x; strip w here).
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o555)).unwrap();

    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("--uid")
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string());
    let (out, _elapsed) = run_with_deadline(
        cmd,
        Duration::from_secs(60),
        "mount over unwritable staging",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "mount over an unwritable staging root must FAIL\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !mnt.join(".stats").exists(),
        "a refused mount must not leave a live filesystem behind"
    );
    assert!(
        stderr.contains(staging.to_str().unwrap()),
        "the refusal must NAME the offending staging root {staging:?}; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("chown"),
        "the refusal must carry the chown remedy (loud preflight, not a raw \
         EACCES later); stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--uid") || stderr.contains("presentation"),
        "the refusal must document that --uid/--gid are presentation-only and \
         grant no staging access; stderr:\n{stderr}"
    );

    // Mount must stay side-effect-free on the staging root: no chown magic.
    let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o555,
        "mount must not mutate staging-root permissions/ownership (cache-path \
         policy: side-effect-free preflight)"
    );

    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// Unit-level pins for the library helpers (no sudo, no mount required).
// ---------------------------------------------------------------------------

/// The pure ownership-resolution rule behind every format-grade stamp
/// (staging roots + root inode): sudo ⇒ invoking user; genuine root ⇒
/// root; non-root ⇒ self; garbage SUDO_* falls back per-field.
#[test]
fn test_resolve_invoking_owner_rule() {
    use squeezefs::config_ops::resolve_invoking_owner;
    // Root via sudo: the INVOKING user wins.
    assert_eq!(
        resolve_invoking_owner(0, 0, Some("1000"), Some("1000")),
        (1000, 1000),
        "sudo (SUDO_UID/SUDO_GID present) must resolve to the invoking user"
    );
    assert_eq!(
        resolve_invoking_owner(0, 0, Some(" 1234 "), Some(" 4321 ")),
        (1234, 4321),
        "whitespace-padded SUDO_* values must parse"
    );
    // Genuine root: no SUDO_* env ⇒ stays root.
    assert_eq!(
        resolve_invoking_owner(0, 0, None, None),
        (0, 0),
        "genuine root (no SUDO_* env) must not be second-guessed"
    );
    // Non-root: always self — even with stray SUDO_* markers (a non-root
    // process could not chown anyway).
    assert_eq!(resolve_invoking_owner(1000, 1000, None, None), (1000, 1000));
    assert_eq!(
        resolve_invoking_owner(1000, 1000, Some("0"), Some("0")),
        (1000, 1000),
        "stray SUDO_* env on a non-root invoker must be ignored"
    );
    // Hostile/garbage SUDO_* falls back per-field to the effective ids.
    assert_eq!(
        resolve_invoking_owner(0, 0, Some("not-a-uid"), Some("1000")),
        (0, 1000),
        "unparseable SUDO_UID must fall back to euid without poisoning gid"
    );
    assert_eq!(
        resolve_invoking_owner(0, 0, Some("1000"), Some("")),
        (1000, 0),
        "empty SUDO_GID must fall back to egid"
    );
}

/// `stamp_staging_dir` (the shared format/set-cache-paths stamp): creates
/// a missing root, wipes pre-existing content, and — as a non-root
/// invoker — leaves it owned by self (the chown is a structural no-op).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stamp_staging_dir_creates_wipes_and_owns() {
    let base = scratch("stamp_unit");

    // Missing at stamp: created (no wipe consent needed — ENG-4 guard).
    let fresh = base.join("fresh");
    squeezefs::config_ops::stamp_staging_dir(&fresh, false)
        .await
        .expect("stamp a missing staging root");
    assert!(fresh.is_dir(), "stamp must create a missing staging root");
    assert_eq!(owner(&fresh), me(), "non-root stamp keeps self-ownership");

    // Pre-existing with junk: wiped empty — with explicit consent (the
    // ENG-4 staging-wipe guard refuses unmarked non-empty dirs otherwise;
    // `tests/staging_wipe_guard_tests.rs` pins the refusal classes).
    let dirty = base.join("dirty");
    std::fs::create_dir_all(dirty.join("nested")).unwrap();
    std::fs::write(dirty.join("nested/stale.bin"), b"poison").unwrap();
    squeezefs::config_ops::stamp_staging_dir(&dirty, true)
        .await
        .expect("stamp a pre-existing staging root");
    assert!(dirty.is_dir(), "stamped root must exist");
    assert_eq!(
        std::fs::read_dir(&dirty).unwrap().count(),
        0,
        "stamp must wipe pre-existing content (format-grade cleanliness)"
    );

    cleanup(&base);
}

/// `staging_write_preflight`: Ok on writable roots and on missing roots
/// under writable ancestors; loud Err — naming the dir, the presentation-
/// only rule, and the chown remedy — when the daemon identity cannot
/// write (existing-root and missing-root-under-unwritable-ancestor legs).
#[test]
fn test_staging_write_preflight_unit() {
    use squeezefs::config_ops::staging_write_preflight;
    use std::os::unix::fs::PermissionsExt;
    if !squeezefs_testkit::non_root(site!(), "the permission-simulation contract") {
        return;
    }
    let base = scratch("preflight_unit");

    // Writable root: Ok.
    let ok_dir = base.join("writable");
    std::fs::create_dir_all(&ok_dir).unwrap();
    staging_write_preflight(std::slice::from_ref(&ok_dir)).expect("writable root must pass");

    // Missing root under a writable ancestor: Ok (mount creates the chain).
    let missing = base.join("not_yet/there");
    staging_write_preflight(std::slice::from_ref(&missing))
        .expect("missing root under a writable ancestor must pass");

    // Unwritable existing root: loud Err with the remedy.
    let locked = base.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let err = staging_write_preflight(std::slice::from_ref(&locked))
        .expect_err("unwritable root must fail the preflight");
    assert!(
        err.contains(locked.to_str().unwrap()) && err.contains("chown"),
        "refusal must name the dir and the chown remedy: {err}"
    );
    assert!(
        err.contains("--uid"),
        "refusal must document the presentation-only --uid/--gid rule: {err}"
    );

    // Missing root under an UNWRITABLE ancestor: loud Err naming both.
    let under_locked = locked.join("sub/root");
    let err = staging_write_preflight(std::slice::from_ref(&under_locked))
        .expect_err("missing root under an unwritable ancestor must fail");
    assert!(
        err.contains(under_locked.to_str().unwrap())
            && err.contains(locked.to_str().unwrap())
            && err.contains("chown"),
        "refusal must name the missing root, the blocking ancestor, and the \
         chown remedy: {err}"
    );

    // First-failure semantics with a mixed set: the offender is reported.
    let err = staging_write_preflight(&[ok_dir, locked.clone()])
        .expect_err("a mixed set with one unwritable root must fail");
    assert!(err.contains(locked.to_str().unwrap()));

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    cleanup(&base);
}
