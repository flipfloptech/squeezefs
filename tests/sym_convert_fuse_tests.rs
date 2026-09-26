//! Symmetric metadata program, PR 11 — the **live-FUSE leg of the offline
//! conversion** (`docs/design-symmetric-metadata.md` §6.2 / §7.2):
//! a flat set is formatted and populated THROUGH A MOUNT (staged small
//! files promoted by the dismount pass, striped multi-block files,
//! directories, hard links, renames, removals), unmounted cleanly,
//! converted by `squeezefs volume enable-symmetric` through the binary,
//! mounted again — a forest — and every byte reads back exactly; a
//! create/rename/unlink storm on the converted mount, a clean unmount,
//! and an offline `squeezefs fsck` reports nothing.
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate. Layout-blind like its
//! in-process sibling: the source is formatted through the plain `format`
//! verb (no seam is read by the binary's format arm), the verb is what
//! stamps.

use squeezefs_testkit::{mount_supported, site};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
/// Small files (staged, promoted at dismount), medium files (one block),
/// and two multi-block striped files.
const SIZES: [usize; 6] = [
    3 * 1024,
    16 * 1024,
    200 * 1024,
    4 * 1024 * 1024,
    9 * 1024 * 1024 + 4096,
    13 * 1024 * 1024,
];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn pattern(idx: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (idx.wrapping_mul(137)
                .wrapping_add(i.wrapping_mul(11))
                .wrapping_add(i >> 9)
                % 241) as u8
        })
        .collect()
}

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_symconv_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

fn run_ok(cmd: &mut Command, what: &str) -> Output {
    let out = cmd.stdin(Stdio::null()).output().expect(what);
    assert!(
        out.status.success(),
        "{what} failed ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn format_flat(base: &Path, staging: &Path) -> PathBuf {
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
    std::fs::create_dir_all(staging).expect("create staging dir");
    // The binary's format arm reads the seam: clear it so the source is
    // FLAT whichever leg of the matrix runs this suite (the verb stamps).
    run_ok(
        Command::new(bin())
            .arg("format")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(format!("sqdata://{}", data.display()))
            .arg("--disk-cache-paths")
            .arg(staging)
            .arg("--force")
            .arg("--single-writer"),
        "squeezefs format",
    );
    meta
}

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mount {
    fn umount_clean(&mut self) {
        let out = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .output()
            .expect("run squeezefs umount");
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 3;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                assert!(
                    status.success(),
                    "daemon exited {status} on unmount; umount said:\n{}{}\nlog:\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not exit within {:?} of `squeezefs umount`; log:\n{}",
                DEFAULT_DISMOUNT_WAIT * 3,
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return;
        }
        let _ = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + DEFAULT_DISMOUNT_WAIT * 2;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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

fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let mut cmd = Command::new(bin());
    cmd.arg("mount")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(mnt)
        .arg("--uid")
        // SAFETY: getuid/getgid are trivially safe.
        .arg(unsafe { libc::getuid() }.to_string())
        .arg("--gid")
        .arg(unsafe { libc::getgid() }.to_string())
        .arg("--disk-cache-size")
        .arg("500MB")
        .env("SQUEEZEFS_FUSE_ZC", "0")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf));
    let child = cmd.spawn().expect("spawn squeezefs mount");
    let mut mount = Mount {
        child,
        mnt: mnt.to_path_buf(),
        log: log.to_path_buf(),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if std::fs::read_to_string(mount.mnt.join(".stats")).is_ok() {
            break;
        }
        if let Ok(Some(status)) = mount.child.try_wait() {
            panic!(
                "mount exited before becoming ready ({status}); log:\n{}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "mount did not become ready within 90 s; log:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    mount
}

fn stats_json(mnt: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(mnt.join(".stats")).expect("read .stats");
    serde_json::from_str(&raw).expect(".stats must be valid JSON")
}

fn metric(stats: &serde_json::Value, key: &str) -> Option<u64> {
    stats["metrics"][key].as_u64()
}

/// Every path written and the bytes it must read back.
fn population(mnt: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for d in 0..3 {
        let dir = mnt.join(format!("dir{d}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (i, len) in SIZES.iter().enumerate() {
            let idx = d * SIZES.len() + i;
            let path = dir.join(format!("file_{idx:03}.bin"));
            let bytes = pattern(idx, *len);
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(&bytes).expect("write");
            f.sync_all().expect("fsync");
            out.push((path, bytes));
        }
    }
    // A hard link, a rename across directories, a removal.
    let (src, bytes) = out[1].clone();
    let link = mnt.join("dir0").join("hard.lnk");
    std::fs::hard_link(&src, &link).expect("link");
    out.push((link, bytes));
    let (moving, moved_bytes) = out[SIZES.len()].clone();
    let moved = mnt.join("dir2").join("moved.bin");
    std::fs::rename(&moving, &moved).expect("rename");
    out.retain(|(p, _)| *p != moving);
    out.push((moved, moved_bytes));
    let (gone, _) = out[2].clone();
    std::fs::remove_file(&gone).expect("unlink");
    out.retain(|(p, _)| *p != gone);
    out
}

fn assert_population(mnt_from: &Path, mnt_to: &Path, pop: &[(PathBuf, Vec<u8>)]) {
    for (path, want) in pop {
        let rel = path.strip_prefix(mnt_from).expect("under the source mount");
        let got = std::fs::read(mnt_to.join(rel))
            .unwrap_or_else(|e| panic!("read {}: {e}", rel.display()));
        assert!(
            got == *want,
            "{} reads back {} B, wanted {} B{}",
            rel.display(),
            got.len(),
            want.len(),
            if got.len() == want.len() {
                " (same length, different bytes)"
            } else {
                ""
            }
        );
    }
    let names: Vec<String> = std::fs::read_dir(mnt_to.join("dir0"))
        .expect("readdir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"hard.lnk".to_string()),
        "readdir lists the link: {names:?}"
    );
    assert!(
        !names.contains(&"file_002.bin".to_string()),
        "readdir omits the removed file: {names:?}"
    );
}

/// A create / rename / unlink storm. Every file is fsync'd before it is
/// renamed: the write path's staged-writeback publish (`Put` on the
/// file's inode under `I{ino}`) and a rename's ctime `Delta` on the SAME
/// inode (rename locks the two parents and the two names, never the moved
/// inode) can otherwise co-queue in one conveyor pass — the same-key
/// exclusion debug assertion fires and the pass fails the batch (EIO).
/// That race is SHIPPED and layout-independent (reproduced on flat, on a
/// fresh `--symmetric` forest and on a converted volume alike —
/// `.benchmarks/2026-09-14-sym-pr11-convert.md` §5b); this suite's
/// contract is the conversion, so the storm keeps the publish ahead of
/// the rename.
fn storm(mnt: &Path, rounds: u32) {
    let d = mnt.join("storm");
    std::fs::create_dir_all(&d).expect("mkdir storm");
    for i in 0..rounds {
        let p = d.join(format!("s{i:04}"));
        let mut f = std::fs::File::create(&p).expect("create");
        f.write_all(&pattern(i as usize, 512 + (i as usize % 7) * 1024))
            .expect("write");
        f.sync_all().expect("fsync");
        drop(f);
        if i % 3 == 2 {
            std::fs::rename(&p, d.join(format!("r{i:04}"))).expect("rename");
        }
        if i % 4 == 3 {
            let prev = i - 1;
            let name = if prev % 3 == 2 {
                format!("r{prev:04}")
            } else {
                format!("s{prev:04}")
            };
            std::fs::remove_file(d.join(name)).expect("unlink");
        }
    }
}

#[test]
fn a_populated_flat_set_converts_and_reads_back_byte_exact_through_a_forest_mount() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("convert");
    let staging = base.join("staging");
    let meta = format_flat(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");

    // Populate through the FLAT mount; the clean unmount's dismount pass
    // promotes the staged small files into blocks.
    let mut writer = spawn_mount(&meta, &mnt, &log);
    let pop = population(&mnt);
    assert_population(&mnt, &mnt, &pop);
    let flat_stats = stats_json(&mnt);
    assert_eq!(
        metric(&flat_stats, "meta_kv_forest_slot_trees_minted").unwrap_or(0),
        0,
        "the source mount is flat"
    );
    writer.umount_clean();

    // The conversion, through the binary. A dry run first, writing nothing.
    let dry = run_ok(
        Command::new(bin())
            .arg("volume")
            .arg("enable-symmetric")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg("--dry-run"),
        "squeezefs volume enable-symmetric --dry-run",
    );
    assert!(
        String::from_utf8_lossy(&dry.stdout).contains("PLAN"),
        "the dry run prints the plan: {}",
        String::from_utf8_lossy(&dry.stdout)
    );
    let conv = run_ok(
        Command::new(bin())
            .arg("volume")
            .arg("enable-symmetric")
            .arg(format!("sqmeta://{}", meta.display())),
        "squeezefs volume enable-symmetric",
    );
    let stdout = String::from_utf8_lossy(&conv.stdout);
    assert!(
        stdout.contains("converted") && stdout.contains("records/s"),
        "the verb reports the conversion and its rate: {stdout}"
    );
    // Idempotence: a second run refuses as already symmetric.
    let again = Command::new(bin())
        .arg("volume")
        .arg("enable-symmetric")
        .arg(format!("sqmeta://{}", meta.display()))
        .stdin(Stdio::null())
        .output()
        .expect("run the verb again");
    assert!(
        !again.status.success(),
        "a converted set refuses a second conversion"
    );
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already symmetric"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // The forest mount: every byte back, then a storm, then a clean leave.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut forest = spawn_mount(&meta, &mnt2, &log2);
    assert_population(&mnt, &mnt2, &pop);
    let forest_stats = stats_json(&mnt2);
    assert_eq!(
        forest_stats["metrics"]["appenders_live"][0].as_u64(),
        Some(1),
        "the converted volume mounts as a forest: this mount joined appender 0"
    );
    assert_eq!(
        forest_stats["metrics"]["appender_joins"][0].as_u64(),
        Some(1),
        "one appender join on the converted volume"
    );
    let rpcs = forest_stats["metrics"]["dlm_rpcs"]
        .as_u64()
        .or_else(|| forest_stats["dlm_rpcs"].as_u64());
    assert_eq!(rpcs, Some(0), "a solo forest mount pays no lock RPC");
    storm(&mnt2, 200);
    assert_population(&mnt, &mnt2, &pop);
    forest.umount_clean();

    // Offline fsck through the binary: nothing to report.
    let fsck = Command::new(bin())
        .arg("fsck")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .expect("run squeezefs fsck");
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&fsck.stdout).trim()).unwrap_or_else(|e| {
            panic!(
                "fsck report is not JSON ({e}): stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&fsck.stdout),
                String::from_utf8_lossy(&fsck.stderr)
            )
        });
    assert!(
        fsck.status.success(),
        "fsck exited {:?} on the converted set — findings: {}",
        fsck.status.code(),
        report["findings"]
    );
    assert_eq!(
        report["findings"].as_array().map_or(0, |a| a.len()),
        0,
        "fsck is clean on the converted set: {}",
        report["findings"]
    );
    let _ = std::fs::remove_dir_all(&base);
}
