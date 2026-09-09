//! The dismount **staged-layout residue** contracts
//! (`.benchmarks/2026-09-09-dismount-staged-residue.md`).
//!
//! A file whose size lands in `(inline ceiling, block_size]` (the shipped
//! 4 KiB ceiling here — the suite pins the A/B control) on a
//! volume formatted with a staging dir takes the STAGED layout: its whole
//! payload is one entry in this host's local staging ring, keyed by the
//! layout's `file_id`, and the shared data backend holds nothing for it
//! until the entry is promoted. The 1.2.2 release gate's fstests TEST
//! device carried exactly 2,193 such files through every cycle and paid
//! the full `--dismount-wait` on every unmount for them (§1.3): the drain
//! wait counted EVERY ring key (`staged_writes_in_flight`), and no
//! teardown step ever retires a staged-layout entry, so the loop could
//! only exit on its timer — 10.08 s measured against ~80 ms of real
//! teardown work. Nothing was lost; the wait was spent on a counter
//! nothing decrements.
//!
//! Contracts (red-first):
//! 1. **The dismount wait counts only what the teardown retires.** A mount
//!    holding N resident staged-layout files and zero active blocks
//!    unmounts in ≪ the default `dismount_wait` (10 s): the wall from
//!    `squeezefs umount` to the teardown's census line in the daemon log
//!    stays under 3 s. Pre-fix it is 10.08 s. The retired "dismounted with
//!    unflushed data" WARN — which described this steady state as a loss —
//!    must not appear.
//! 2. **A clean unmount is a durability boundary for staged-layout files**
//!    (§7 item 3), as it already is for rewrite epochs, overlays and
//!    active blocks: the teardown promotes every resident staged-layout
//!    entry to the shared backend, so (a) the census reads "Dismount
//!    clean" with no resident staged-layout report, (b) a remount at a
//!    DIFFERENT mount point — a client the origin's staging root is
//!    invisible to — reads all N files byte-exact (pre-fix: size-consistent
//!    ZEROS, §3 legs A/B), (c) that remount's `staged_payload_lost_reads`
//!    stays 0 and it reports no moved-mount-point residue, and (d) the
//!    unmount still lands its census well inside the default wait (each
//!    promotion is one block + one metadata commit).
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate
//! (`tests/run_require_mount_gate.sh`).

use squeezefs_testkit::{mount_supported, site};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The probe's population (§3): 200 small files, all staged-layout.
const FILES: usize = 200;
/// Sizes strictly above the shipped 4 KiB inline ceiling and far below the 4 MiB
/// block — the staged layout by construction (§1). 4 KiB itself would be
/// inline, so it is excluded.
const SIZES_KIB: [usize; 4] = [8, 16, 32, 64];
/// The mount's default `--dismount-wait`, which this suite must NOT run to.
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
/// The unmount wall bound: the real teardown work for this population is
/// tens of milliseconds; 3 s leaves room for a slow laptop's fsyncs while
/// staying far below the 10 s timer whose expiry is the bug.
const UNMOUNT_BOUND: Duration = Duration::from_secs(3);
/// The retired census WARN — the misdescription this suite pins gone.
const RETIRED_WARN: &str = "dismounted with unflushed data";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic per-file content (sha-free: a rolling byte pattern
/// salted by the index, so a zeros read or a cross-file mix-up is caught).
fn pattern(idx: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (idx.wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
        })
        .collect()
}

fn file_len(idx: usize) -> usize {
    SIZES_KIB[idx % SIZES_KIB.len()] * 1024
}

/// Scratch under the system temp dir, CANONICALIZED before anything is
/// mounted under it (`/proc/self/mountinfo` compares paths verbatim; a
/// desktop's volume monitor probes `$HOME`-rooted mounts — the
/// `commit_wake_loss_tests` venue note).
fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_dresidue_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + one data volume WITH a staging dir (the cache-path
/// policy: paths are declared at format, never at mount).
fn format_volume(base: &Path, staging: &Path) -> PathBuf {
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
    let out: Output = Command::new(bin())
        .arg("format")
        .arg(format!("sqmeta://{}", meta.display()))
        .arg(format!("sqdata://{}", data.display()))
        .arg("--disk-cache-paths")
        .arg(staging)
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

struct Mount {
    child: Child,
    mnt: PathBuf,
    log: PathBuf,
}

/// The line the dismount teardown logs when it reaches its census — the
/// instant the drain wait (and, since step 2, the promotion pass) is over.
/// Three spellings: the clean verdict, the retired WARN (the pre-fix
/// binary this suite is red against), and the two-class report's "at
/// dismount" phrase.
const TEARDOWN_CENSUS_MARKERS: &[&str] = &["Dismount clean", RETIRED_WARN, "at dismount"];

impl Mount {
    /// `squeezefs umount` on the DEFAULT dismount wait, non-interactive
    /// (stdin is not a TTY, so the CLI takes its "continue" arm). Returns
    /// the wall from the invocation to the teardown's CENSUS line in the
    /// daemon log — the teardown runs after the kernel mount is gone, so
    /// only that instant includes the drain wait; the daemon's process
    /// exit is not the instrument because `--all-features` builds dump a
    /// dhat heap profile on exit (seconds, unrelated to the teardown).
    /// Then waits for the daemon's exit and asserts it clean (a teardown
    /// panic is a bug an `ok` must not absorb).
    fn umount_timed(&mut self) -> Duration {
        let started = Instant::now();
        let mut umount = Command::new(bin())
            .arg("umount")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn squeezefs umount");
        // The teardown is bounded by the dismount wait plus its own
        // margin; anything past that is a wedge, not a slow unmount.
        let deadline = started + DEFAULT_DISMOUNT_WAIT * 3;
        let census_wall = loop {
            if TEARDOWN_CENSUS_MARKERS
                .iter()
                .any(|m| log_contains(&self.log, m))
            {
                break started.elapsed();
            }
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                panic!(
                    "daemon exited ({status}) without logging its dismount census; log:\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            if Instant::now() > deadline {
                let _ = umount.kill();
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "dismount census not reached within {:?} of `squeezefs umount`; log:\n{}",
                    DEFAULT_DISMOUNT_WAIT * 3,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait daemon") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = umount.kill();
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "daemon did not exit within {:?} of `squeezefs umount`; log:\n{}",
                    DEFAULT_DISMOUNT_WAIT * 3,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        eprintln!(
            "umount → census {census_wall:?}, → daemon exit {:?}",
            started.elapsed()
        );
        let out = umount.wait_with_output().expect("collect umount output");
        assert!(
            status.success(),
            "daemon exited {status} on unmount (teardown crash); umount said:\n{}{}\nlog:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
        census_wall
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
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

/// Spawn the real daemon on the probe's posture (zc OFF — the fstests
/// runner's default; a modest staging ring; the DEFAULT dismount wait) and
/// wait for the stats inode.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path) -> Mount {
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
        .arg("--disk-cache-size")
        .arg("500MB")
        .env("SQUEEZEFS_FUSE_ZC", "0")
        // The staged-layout contracts run at the SHIPPED inline ceiling
        // (the A/B control): the 2026-09-09 inline raise derives 60 KiB
        // from the KV geometry, under which this suite's 8–64 KiB files
        // would be inline and never staged.
        .env("SQUEEZEFS_INLINE_MAX_BYTES", "4096")
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf))
        .spawn()
        .expect("spawn squeezefs mount");
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

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

fn file_name(idx: usize) -> String {
    format!("staged_{idx:04}.bin")
}

/// Write the probe's population (open → write → close, no fsync — the
/// application shape) and `syncfs` the mount (scoped to this filesystem,
/// never a box-wide `sync(2)`). Then wait for the stats inode to report
/// the whole population resident in the staging ring with NO active-block
/// custody — the exact state the drain wait can never drain.
fn populate_staged_files(mnt: &Path) {
    for idx in 0..FILES {
        let path = mnt.join(file_name(idx));
        let mut f = std::fs::File::create(&path)
            .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        f.write_all(&pattern(idx, file_len(idx)))
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    {
        let root = std::fs::File::open(mnt).expect("open mount root");
        use std::os::fd::AsRawFd;
        // SAFETY: syncfs on a live fd; the return is checked.
        let rc = unsafe { libc::syncfs(root.as_raw_fd()) };
        assert_eq!(rc, 0, "syncfs({}) failed", mnt.display());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v = stats_json(mnt);
        let staged = v["nvme_staged_write_file_count"].as_u64().unwrap_or(0) as usize;
        let active = v["active_write_block_count"].as_u64().unwrap_or(0) as usize;
        if staged == FILES && active == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the population never settled as {FILES} staged-layout files with 0 active \
             blocks (nvme_staged_write_file_count = {staged}, active_write_block_count = \
             {active})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Contract 1: with N resident staged-layout files and zero active
/// blocks, `squeezefs umount` (default 10 s dismount wait) reaches its
/// teardown census in ≪ the wait. Pre-fix the drain loop waits on
/// `staged_writes_in_flight`, which counts every ring key and which no
/// teardown step decrements for this population, so the unmount runs to
/// the timer: 10.08 s on the probe.
#[test]
fn a_mount_holding_only_staged_layout_files_unmounts_without_the_drain_wait() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("stall");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log);

    populate_staged_files(&mnt);

    let elapsed = mount.umount_timed();
    assert!(
        elapsed < UNMOUNT_BOUND,
        "unmount of a mount holding {FILES} staged-layout files and 0 active blocks took \
         {elapsed:?} — the dismount drain wait ran to its timer on entries no teardown step \
         retires (bound {UNMOUNT_BOUND:?}, default dismount wait {DEFAULT_DISMOUNT_WAIT:?}); \
         log: {}",
        log.display()
    );
    assert!(
        !log_contains(&log, RETIRED_WARN),
        "the census must not describe resident staged-layout files as a loss (\"{RETIRED_WARN}\"); \
         log: {}",
        log.display()
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// The census arm that names an un-promoted population (must not fire on
/// a clean unmount once the teardown promotes).
const RESIDENT_REPORT: &str = "staged-layout files resident in local staging at dismount";
/// The clean verdict.
const CLEAN_VERDICT: &str = "Dismount clean";
/// The remount-side residue report a stranded origin root would earn.
const RESIDUE_REPORT: &str = "MOVED-MOUNT-POINT STAGING RESIDUE";

/// Contract 2: the clean unmount PROMOTES the staged ledger. After N
/// staged-layout files + syncfs + `squeezefs umount`: the census is
/// "Dismount clean" with no resident report, and a remount at a
/// DIFFERENT mount point — which opens its own staging root and can see
/// nothing of the origin's ring, exactly another host's view — reads
/// every file byte-exact from the shared backend with
/// `staged_payload_lost_reads == 0`. Pre-fix the promotion never happens
/// (only pool pressure promotes), the census reports N resident, and the
/// second mount serves size-consistent zeros for all N.
#[test]
fn a_clean_unmount_promotes_every_staged_layout_file_for_other_clients() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("promote");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log);

    populate_staged_files(&mnt);

    // (d) the promotions ride the teardown, still well inside the wait.
    let elapsed = mount.umount_timed();
    assert!(
        elapsed < UNMOUNT_BOUND,
        "unmount with {FILES} staged-layout files to promote took {elapsed:?} to its census \
         (bound {UNMOUNT_BOUND:?}); log: {}",
        log.display()
    );
    // (a) the census: nothing resident, the ledger promoted.
    assert!(
        !log_contains(&log, RESIDENT_REPORT),
        "a clean unmount left staged-layout files resident in local staging — the \
         teardown did not promote the staged ledger; log: {}",
        log.display()
    );
    assert!(
        log_contains(&log, CLEAN_VERDICT),
        "the census must read \"{CLEAN_VERDICT}\" after the promotions; log: {}",
        log.display()
    );
    assert!(
        log_contains(&log, &format!("promoted {FILES} staged-layout file(s)")),
        "the teardown must report promoting all {FILES} files (the engagement line); log: {}",
        log.display()
    );

    // (b) another client's view: a fresh mount point ⇒ a fresh staging
    // slot; the origin's ring is not adopted, so the shared backend is
    // the only source of every byte.
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2);
    let mut zero_reads = 0usize;
    let mut mismatches = 0usize;
    for idx in 0..FILES {
        let path = mnt2.join(file_name(idx));
        let got = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let want = pattern(idx, file_len(idx));
        if got != want {
            mismatches += 1;
            if got.len() == want.len() && got.iter().all(|&b| b == 0) {
                zero_reads += 1;
            }
        }
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches} of {FILES} staged-layout files read wrong from a second mount point \
         ({zero_reads} as size-consistent ZEROS — the un-promoted staged-layout shape: the \
         bytes exist only in the origin's staging root); remount log: {}",
        log2.display()
    );
    // (c) the reader never degraded, and the origin root holds no live
    // custody the second mount had to report.
    let lost = stats_json(&mnt2)["metrics"]["staged_payload_lost_reads"]
        .as_u64()
        .expect("staged_payload_lost_reads exported");
    assert_eq!(
        lost,
        0,
        "the second mount degraded {lost} reads to zeros (staged_payload_lost_reads); log: {}",
        log2.display()
    );
    assert!(
        !log_contains(&log2, RESIDUE_REPORT),
        "the origin's staging root still held live staged custody after a clean unmount \
         (\"{RESIDUE_REPORT}\"); log: {}",
        log2.display()
    );

    mount2.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}
