//! Disk-cache-path POLICY contracts (user-decided semantics, 2026-07-10).
//!
//! Background: a user was bitten by mount-time `--disk-cache-paths`
//! conjuring/reusing caches that format never declared (the stale-staging
//! poisoning incident). Generation-binding now protects the CONTENT
//! (`tests/staging_generation_tests.rs`); this suite pins the POLICY:
//!
//! 1. **`--disk-cache-paths` is declared at FORMAT** and recorded in the
//!    format config (the single source of truth). Format WITHOUT the flag
//!    ⇒ the filesystem is **permanently cache-less**: mounts run with no
//!    NVMe staging/read-cache tier (RAM tiers + direct block I/O only) and
//!    small+large I/O routes through the inline/striped paths.
//! 2. **Mount can NOT set or override cache paths**: `--disk-cache-paths`
//!    (and its `--cache-dir` alias) on `mount` is a LOUD, INSTANT error —
//!    never a silent ignore. Mount reads paths from the format config only.
//! 3. **Changing paths is an explicit admin op**:
//!    `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` — guarded
//!    like format (live-mounted volumes refuse), rewrites the format
//!    config, and wipes the NEW dirs so the next mount stamps a fresh
//!    generation. `config get-cache-paths <sqmeta-uri>` for symmetry.
//!    (Since ENG-4 the wipe itself is guarded: non-empty dirs with no
//!    staging marker need `--force`/`--yes` — `staging_wipe_guard_tests`.)
//!
//! All CLI tests drive the real binary (`CARGO_BIN_EXE_squeezefs`); the
//! library-level contract test pins the cache-less `TieredCache` surface
//! that the routing layer's layout decisions depend on.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = ".squeezefs_generation";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_cachepolicy_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
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

/// Format one meta + one data volume; `staging` declares cache paths.
fn format_volume(base: &Path, staging: Option<&Path>) -> (PathBuf, PathBuf) {
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
    let mut cmd = Command::new(bin());
    cmd.arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--force");
    if let Some(s) = staging {
        cmd.arg("--disk-cache-paths").arg(s);
    }
    let out = cmd.output().expect("run squeezefs format");
    assert!(
        out.status.success(),
        "format failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (meta, data)
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

/// Spawn `squeezefs mount <meta> <mnt>` (NO cache-path flag — the policy
/// under test) and wait for the stats inode.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
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
            "mount did not become ready in 90s; log:\n{}",
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

fn config_json(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".config")).expect("read .config");
    serde_json::from_str(&raw).expect(".config must be valid JSON")
}

/// Mirror of the mount-time staging isolation naming
/// (`src/main.rs`): non-alphanumeric → '_', collapse runs, trim ends;
/// isolated dir = `<staging>/squeezefs/<sanitized>`.
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
        assert_eq!(
            back.len(),
            payload.len(),
            "{name}: read-back length mismatch"
        );
        assert_eq!(back, payload, "{name}: read-back bytes mismatch");
    }
    std::fs::remove_file(&path).unwrap_or_else(|e| panic!("delete {name}: {e}"));
}

// ---------------------------------------------------------------------------
// Contract 2 — mount can NOT set or override cache paths: LOUD instant error.
// ---------------------------------------------------------------------------

/// `mount --disk-cache-paths` (and the `--cache-dir` alias) must fail fast
/// (< 5 s), with a non-zero exit and the exact policy message pointing at
/// `config set-cache-paths` — in both foreground and `--daemon` mode. No
/// daemon may be left behind and nothing may be mounted.
#[test]
fn test_mount_rejects_disk_cache_paths_flag_loud_and_instant() {
    let base = scratch("mount_reject");
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (meta, _data) = format_volume(&base, Some(&staging));
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();

    for (variant, extra) in [
        ("foreground --disk-cache-paths", vec![]),
        ("--daemon --disk-cache-paths", vec!["--daemon"]),
    ] {
        let mut cmd = Command::new(bin());
        cmd.arg("mount")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(&mnt)
            .arg("--disk-cache-paths")
            .arg(&staging);
        for e in &extra {
            cmd.arg(e);
        }
        let (out, elapsed) = run_with_deadline(cmd, Duration::from_secs(10), variant);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !out.status.success(),
            "{variant}: mount with --disk-cache-paths must FAIL, but exited success\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "{variant}: rejection must be instant (< 5s), took {elapsed:?}"
        );
        assert!(
            stderr.contains("cache paths are fixed at format"),
            "{variant}: missing the policy error; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("config set-cache-paths"),
            "{variant}: error must point at `squeezefs config set-cache-paths`; stderr:\n{stderr}"
        );
        assert!(
            !mnt.join(".stats").exists(),
            "{variant}: a refused mount must not leave a live filesystem behind"
        );
    }

    // The historical `--cache-dir` alias must be refused identically.
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(&mnt)
        .arg("--cache-dir")
        .arg(&staging);
    let (out, elapsed) = run_with_deadline(cmd, Duration::from_secs(10), "--cache-dir alias");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("cache paths are fixed at format"),
        "--cache-dir alias must be refused with the policy error; stderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "--cache-dir alias rejection took {elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 1a — format WITH paths: mount picks them up from the format
// config (verify + pin).
// ---------------------------------------------------------------------------

/// Format declares the cache paths; a flag-less mount must adopt exactly
/// those (isolated per-mount dir + generation marker under the declared
/// root), and a staged-window write must actually ride the staging tier.
#[test]
fn test_mount_adopts_format_declared_cache_paths() {
    if !transport_supported() {
        return;
    }
    let base = scratch("declared");
    let staging = base.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (meta, _data) = format_volume(&base, Some(&staging));
    let mnt = base.join("mnt");
    let log = base.join("mount.log");

    let mut mount = spawn_mount(&meta, &mnt, &log);

    // .config format.disk_cache_paths points under the DECLARED root.
    let cfg = config_json(&mnt);
    let cfg_paths = cfg["format"]["disk_cache_paths"]
        .as_str()
        .expect("format.disk_cache_paths string")
        .to_string();
    assert!(
        cfg_paths.contains(staging.to_str().unwrap()),
        ".config cache paths must derive from the format-declared root; got {cfg_paths:?}"
    );

    // The isolated per-mount staging dir exists and is generation-stamped.
    let isolated = isolated_staging_dir(&staging, &mnt);
    assert!(
        isolated.is_dir(),
        "mount must create the isolated staging dir under the declared root: {isolated:?}"
    );
    assert!(
        isolated.join(MARKER).is_file(),
        "the isolated staging dir must carry the generation marker: {isolated:?}"
    );

    // A staged-window write (16 KiB: > inline, < block size) rides staging.
    let f = mnt.join("staged_probe.bin");
    std::fs::write(&f, vec![0x5Au8; 16 * 1024]).expect("staged-window write");
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
            "staged-window write never took the staged layout on a cache-declared volume"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    mount.unmount();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 1b — format WITHOUT paths: permanently cache-less mounts, full
// small+large I/O cycle through the inline/striped paths (the risky leg).
// ---------------------------------------------------------------------------

/// Format without `--disk-cache-paths` ⇒ the mount runs cache-less: no
/// default staging dir is conjured, `.config` reports no cache paths, small
/// and large writes route inline/striped (ZERO staged-layout writes), and a
/// small+large write/read/delete cycle is green.
#[test]
fn test_cacheless_mount_small_large_io_cycle() {
    if !transport_supported() {
        return;
    }
    let base = scratch("cacheless");
    let (meta, _data) = format_volume(&base, None);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");

    let mut mount = spawn_mount(&meta, &mnt, &log);

    // No cache paths: .config must report none…
    let cfg = config_json(&mnt);
    let cfg_paths = cfg["format"]["disk_cache_paths"]
        .as_str()
        .expect("format.disk_cache_paths string")
        .to_string();
    assert!(
        cfg_paths.is_empty(),
        "cache-less mount must run with NO staging paths; .config reports {cfg_paths:?}"
    );

    // …and the historical default staging root must NOT be conjured for
    // this mount (the policy hole this suite exists to close).
    let uid = unsafe { libc::getuid() };
    let default_root = if uid == 0 {
        PathBuf::from("/tmp/squeezefs_staging")
    } else {
        PathBuf::from(format!("/tmp/squeezefs_staging_{uid}"))
    };
    let conjured = isolated_staging_dir(&default_root, &mnt);
    assert!(
        !conjured.exists(),
        "cache-less mount conjured the default staging dir {conjured:?}"
    );

    // Small + large I/O cycle: inline (1 KiB), staged-window (16 KiB),
    // multi-block striped with a partial tail (10 MiB @ 4 MiB blocks).
    write_read_delete(&mnt, "small_inline.bin", 1024, 0x11);
    write_read_delete(&mnt, "small_would_be_staged.bin", 16 * 1024, 0x22);
    write_read_delete(&mnt, "large_striped.bin", 10 * 1024 * 1024, 0x33);

    // Layout routing: the staged layout must never fire; inline and striped
    // must both have carried traffic.
    let metrics = &stats_json(&mnt)["metrics"];
    let staged = metrics["layout_staged_writes"].as_u64().unwrap();
    let inline = metrics["layout_inline_writes"].as_u64().unwrap();
    let striped = metrics["layout_striped_writes"].as_u64().unwrap();
    assert_eq!(
        staged, 0,
        "cache-less mount routed writes through the staged layout"
    );
    assert!(
        inline >= 1,
        "inline writes must ride the inline path (got {inline})"
    );
    assert!(
        striped >= 1,
        "beyond-inline writes must ride the striped path (got {striped})"
    );

    mount.unmount();
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Contract 3 — `config set-cache-paths` / `get-cache-paths` admin ops.
// ---------------------------------------------------------------------------

/// The full admin flow: refused while live-mounted; succeeds unmounted
/// (rewrites the format config and WIPES the new dirs); `get-cache-paths`
/// reflects the change; the next mount adopts the new root and stamps a
/// fresh generation marker there.
#[test]
fn test_set_cache_paths_admin_op_end_to_end() {
    if !transport_supported() {
        return;
    }
    let base = scratch("setpaths");
    let staging_a = base.join("staging_a");
    let staging_b = base.join("staging_b");
    std::fs::create_dir_all(&staging_a).unwrap();
    let (meta, _data) = format_volume(&base, Some(&staging_a));
    let meta_uri = format!("sqmeta://{}", meta.display());
    let mnt = base.join("mnt");
    let log = base.join("mount.log");

    // Live-mounted: the op must refuse like format does.
    let mut mount = spawn_mount(&meta, &mnt, &log);
    let mut cmd = Command::new(bin());
    cmd.arg("config")
        .arg("set-cache-paths")
        .arg(&meta_uri)
        .arg(&staging_b);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(30), "set-cache-paths (mounted)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "set-cache-paths must refuse while the volume is live-mounted"
    );
    assert!(
        stderr.contains("actively mounted"),
        "the live-mount refusal must name the cause; stderr:\n{stderr}"
    );
    mount.unmount();
    drop(mount);

    // Unmounted: pre-seed junk into the NEW dir — the op must wipe it.
    // `--force` = the ENG-4 wipe-guard consent: a non-empty dir carrying
    // no staging marker only wipes with explicit consent (the refusal
    // classes are pinned in `tests/staging_wipe_guard_tests.rs`).
    std::fs::create_dir_all(&staging_b).unwrap();
    std::fs::write(staging_b.join("stale_junk.bin"), b"poison").unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("config")
        .arg("set-cache-paths")
        .arg(&meta_uri)
        .arg(&staging_b)
        .arg("--force");
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(30), "set-cache-paths (unmounted)");
    assert!(
        out.status.success(),
        "set-cache-paths on an unmounted volume failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !staging_b.join("stale_junk.bin").exists(),
        "set-cache-paths must wipe the NEW cache dirs"
    );

    // get-cache-paths reflects the rewrite.
    let mut cmd = Command::new(bin());
    cmd.arg("config").arg("get-cache-paths").arg(&meta_uri);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(30), "get-cache-paths");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains(staging_b.to_str().unwrap()),
        "get-cache-paths must report the new path; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains(staging_a.to_str().unwrap()),
        "get-cache-paths must not report the replaced path; stdout:\n{stdout}"
    );

    // Next mount adopts the new root: isolated dir + fresh generation stamp
    // live under staging_b, and staged traffic flows there.
    let mut mount = spawn_mount(&meta, &mnt, &log);
    let cfg_paths = config_json(&mnt)["format"]["disk_cache_paths"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        cfg_paths.contains(staging_b.to_str().unwrap())
            && !cfg_paths.contains(staging_a.to_str().unwrap()),
        "mount after set-cache-paths must use ONLY the new root; got {cfg_paths:?}"
    );
    let isolated_b = isolated_staging_dir(&staging_b, &mnt);
    assert!(
        isolated_b.is_dir() && isolated_b.join(MARKER).is_file(),
        "the relocated staging root must be generation-stamped at {isolated_b:?}"
    );
    write_read_delete(&mnt, "relocated_staged.bin", 16 * 1024, 0x44);
    mount.unmount();

    let _ = std::fs::remove_dir_all(&base);
}

/// `get-cache-paths` on a cache-less volume reports none; `set-cache-paths`
/// on a blank (never formatted) volume fails loud. Neither needs a mount.
#[test]
fn test_cache_path_ops_edge_cases() {
    let base = scratch("edges");
    let (meta, _data) = format_volume(&base, None);
    let meta_uri = format!("sqmeta://{}", meta.display());

    let mut cmd = Command::new(bin());
    cmd.arg("config").arg("get-cache-paths").arg(&meta_uri);
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(30), "get-cache-paths (cacheless)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("none"),
        "a cache-less volume must report no cache paths; stdout:\n{stdout}"
    );

    let blank = base.join("blank.bin");
    std::fs::File::create(&blank)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let mut cmd = Command::new(bin());
    cmd.arg("config")
        .arg("set-cache-paths")
        .arg(format!("sqmeta://{}", blank.display()))
        .arg(base.join("staging_x"));
    let (out, _) = run_with_deadline(cmd, Duration::from_secs(30), "set-cache-paths (blank)");
    assert!(
        !out.status.success(),
        "set-cache-paths on a blank volume must fail loud"
    );

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Library contract — cache-less TieredCache surface (what routing keys off).
// ---------------------------------------------------------------------------

/// With NO staging dirs the tiering surface must be inert and never-lossy:
/// `staging_dirs()` empty (routing's layout gate), `stage_write` fails loud
/// (StorageFull → spill), `put_active_block` refuses (callers keep the RAM
/// buffer / escalate to a durable upload), read-block caching is a no-op,
/// and nothing is ever readable back from the staging tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cacheless_tiered_cache_library_contract() {
    let data = tempfile::NamedTempFile::new().unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let dlm = squeezefs::dlm::DlmClient::new("local").unwrap();
    let nvme_dev = std::sync::Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        data.path().to_str().unwrap(),
    ));
    let ba = std::sync::Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(dlm.meta_client().clone(), "cacheless_lib")
            .await
            .unwrap(),
    );
    let cache = squeezefs::cache::TieredCache::new(
        vec![],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("8MB"),
        dlm.meta_client().clone(),
        ba,
        nvme_dev,
        Some("v3:cacheless-test-generation"),
    )
    .await
    .expect("cache-less TieredCache construction must succeed");

    assert!(
        cache.nvme.staging_dirs().is_empty(),
        "cache-less mount must expose no staging dirs to the routing layer"
    );

    let err = cache
        .nvme
        .stage_write(
            "inode_9",
            "file-id-9",
            bytes::Bytes::from(vec![1u8; 8192]),
            1,
        )
        .await
        .expect_err("stage_write must fail loud with no staging dirs");
    assert!(
        matches!(
            &err,
            squeezefs::error::SqueezefsError::Io(e)
                if e.kind() == std::io::ErrorKind::StorageFull
        ),
        "stage_write must fail StorageFull (the spill trigger), got {err:?}"
    );

    assert!(
        !cache
            .nvme
            .put_active_block("active_block:inode_9:block_0", &[2u8; 4096], 1),
        "put_active_block must refuse admission with no staging dirs (never-lossy RAM path)"
    );
    assert!(
        cache
            .nvme
            .read_staged("active_block:inode_9:block_0")
            .is_none(),
        "nothing may be readable from a cache-less staging tier"
    );

    cache
        .nvme
        .cache_read_block("blocks/17", bytes::Bytes::from(vec![3u8; 4096]))
        .expect("read-block caching must be a clean no-op when cache-less");
    assert!(
        cache.nvme.read_cached_block("blocks/17").is_none(),
        "the cache-less read tier must not retain blocks"
    );
    assert_eq!(
        cache.nvme.current_staged_write_bytes(),
        0,
        "no staged budget may accrue on a cache-less mount"
    );
}
