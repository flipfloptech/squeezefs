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

/// The sudo-shape contracts need a non-root invoker with passwordless sudo.
fn sudo_available() -> bool {
    if unsafe { libc::getuid() } == 0 {
        eprintln!("[SKIP] running as root — the sudo-shape contracts need a non-root invoker");
        return false;
    }
    match Command::new("sudo").args(["-n", "true"]).output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("[SKIP] passwordless sudo unavailable");
            false
        }
    }
}

/// FUSE-over-io_uring mount support gate (same shape as the other real-CLI
/// suites): tests that need a live mount skip cleanly where they cannot run.
fn transport_supported() -> bool {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("[SKIP] /dev/fuse not present");
        return false;
    }
    match std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring") {
        Ok(v)
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "y" | "1" | "yes" | "true" | "on"
            ) => {}
        other => {
            eprintln!("[SKIP] kernel fuse.enable_uring not enabled ({other:?})");
            return false;
        }
    }
    if Command::new("fusermount3").arg("-V").output().is_err() {
        eprintln!("[SKIP] fusermount3 not available");
        return false;
    }
    true
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
        for _ in 0..10 {
            let st = Command::new("fusermount3")
                .arg("-u")
                .arg(&self.mnt)
                .status()
                .expect("run fusermount3 -u");
            if st.success() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait").is_some() {
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
        let _ = Command::new("fusermount3")
            .arg("-uz")
            .arg(&self.mnt)
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
    if !sudo_available() || !transport_supported() {
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
    if !sudo_available() {
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
    if !sudo_available() {
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
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("[SKIP] permission-simulation contract needs a non-root test identity");
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
