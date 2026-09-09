//! The **fsync-promotes-staged lever** (`SQUEEZEFS_FSYNC_PROMOTE_STAGED`,
//! default off — `.benchmarks/2026-09-09-dismount-staged-residue.md` §7
//! item 3, the owner's step (3)).
//!
//! A STAGED-layout file (4 KiB < size ≤ block_size on a volume with a
//! staging dir) keeps its whole payload in this host's local staging ring.
//! Its `fsync(2)` syncs that ring shard and commits the staged layout —
//! durable HERE — and nothing else: until staging-pool pressure or this
//! mount's clean unmount promotes the entry, every other client of the
//! volume set reads the file as size-consistent zeros. With the lever on,
//! the fsync ladder ALSO promotes the file (one striped block write + one
//! layout commit, `DataRouter::promote_staged_file` — the merge worker's
//! and the dismount pass's own primitive) inside its own `fsync_phase_ns`
//! leg (`staged_promote`), after the local flush and before the data
//! barrier that covers the promoted block and the meta barrier that names
//! it — so "fsync'd" means "visible to every client of the set". The
//! default is decided on a counted same-binary A/B on squeeze-test; `1` is
//! the pricing arm.
//!
//! Contracts (red-first, live mount):
//! (a) lever OFF (the shipped default): N staged-layout files, each
//!     fsync'd, stay resident — `nvme_staged_write_file_count == N`,
//!     `fsync_promoted_files == 0`. Today's behavior, pinned so the default
//!     cannot drift silently.
//! (b) lever ON: N files each fsync'd (twice, concurrently — the
//!     exactly-once pin) → the resident count decays to 0,
//!     `fsync_promoted_files == N` exactly, `fsync_promote_failures == 0`,
//!     `fsync_phase_ns` stays exact-sum with `staged_promote` populated;
//!     then the daemon is SIGKILLed (no clean unmount — the dismount pass
//!     cannot be what promoted) and a mount at a DIFFERENT mount point (a
//!     client the origin's staging root is invisible to) reads every file
//!     byte-exact with `staged_payload_lost_reads == 0`. RED before the
//!     mechanism: the second mount reads zeros.
//! (c) lever ON, a file written but NOT fsync'd stays staged — the lever
//!     is fsync-scoped, not a write-path change.
//!
//! Mount-class: self-skips through the testkit where a mount is not
//! possible and rides the require-mount gate
//! (`tests/run_require_mount_gate.sh`).

use squeezefs_testkit::{mount_supported, site};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The lever under test.
const LEVER: &str = "SQUEEZEFS_FSYNC_PROMOTE_STAGED";
/// Files per contract: enough to make a missed promotion or a double
/// promotion visible in the gauges, small enough for a per-commit suite.
const FILES: usize = 40;
/// Sizes strictly above `MAX_INLINE_SIZE` (4 KiB) and far below the 4 MiB
/// block — the staged layout by construction.
const SIZES_KIB: [usize; 4] = [8, 16, 32, 64];
/// The mount's default `--dismount-wait` — the daemon-side bound the
/// fixture's teardown waits against.
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic per-file content (a zeros read or a cross-file mix-up is
/// caught).
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

fn file_name(idx: usize) -> String {
    format!("fsynced_{idx:04}.bin")
}

/// Scratch under the system temp dir, CANONICALIZED before anything is
/// mounted under it (the `commit_wake_loss_tests` venue note).
fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_fpromote_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + one data volume WITH a staging dir.
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

impl Mount {
    /// `squeezefs umount`, waiting for the daemon's clean exit.
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
                    "daemon exited {status} on unmount (teardown crash); umount said:\n{}{}\nlog:\n{}",
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

    /// The crash: SIGKILL the daemon (no teardown of any kind runs), then
    /// detach the dead mount so the mount point can be reused.
    fn kill9(&mut self) {
        self.child.kill().expect("SIGKILL the daemon");
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let ok = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mnt)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok || !is_mounted(&self.mnt) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "dead mount at {} could not be detached",
                self.mnt.display()
            );
            std::thread::sleep(Duration::from_millis(100));
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

fn is_mounted(mnt: &Path) -> bool {
    let want = mnt.to_string_lossy();
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|t| {
            t.lines()
                .any(|l| l.split(' ').nth(4).is_some_and(|p| p == want))
        })
        .unwrap_or(false)
}

/// Spawn the real daemon (zc OFF — the fstests runner's posture; a modest
/// staging ring, far from its high-water mark so pool pressure can never
/// be what promotes) with the lever set as asked, and wait for the stats
/// inode.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, lever: Option<&str>) -> Mount {
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
        .env_remove(LEVER)
        .stdout(Stdio::from(logf.try_clone().expect("clone log fd")))
        .stderr(Stdio::from(logf));
    if let Some(v) = lever {
        cmd.env(LEVER, v);
    }
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

fn staged_count(stats: &serde_json::Value) -> u64 {
    stats["nvme_staged_write_file_count"]
        .as_u64()
        .expect("nvme_staged_write_file_count exported")
}

/// Write the population (open → write → close — the application shape;
/// every file lands staged-layout) and return the paths.
fn write_files(mnt: &Path) -> Vec<PathBuf> {
    (0..FILES)
        .map(|idx| {
            let path = mnt.join(file_name(idx));
            std::fs::File::create(&path)
                .unwrap_or_else(|e| panic!("create {}: {e}", path.display()))
                .write_all(&pattern(idx, file_len(idx)))
                .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            path
        })
        .collect()
}

/// fsync every file from `threads` threads at once (each thread opens its
/// own fd): the racing-fsync shape the exactly-once pin needs.
fn fsync_all_concurrently(paths: &[PathBuf], threads: usize) {
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let paths = paths.to_vec();
            std::thread::spawn(move || {
                for p in &paths {
                    std::fs::File::open(p)
                        .unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
                        .sync_all()
                        .unwrap_or_else(|e| panic!("fsync {}: {e}", p.display()));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("fsync thread");
    }
}

/// Read every file at `mnt` and count wrong reads (zeros separately).
fn verify_files(mnt: &Path) -> (usize, usize) {
    let mut mismatches = 0usize;
    let mut zero_reads = 0usize;
    for idx in 0..FILES {
        let path = mnt.join(file_name(idx));
        let got = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let want = pattern(idx, file_len(idx));
        if got != want {
            mismatches += 1;
            if got.len() == want.len() && got.iter().all(|&b| b == 0) {
                zero_reads += 1;
            }
        }
    }
    (mismatches, zero_reads)
}

/// Contract (a): the shipped default. fsync makes a staged-layout file
/// durable on THIS host's ring and promotes nothing.
#[test]
fn lever_off_fsync_leaves_staged_layout_files_resident() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("off");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, None);

    let paths = write_files(&mnt);
    fsync_all_concurrently(&paths, 1);

    let stats = stats_json(&mnt);
    assert_eq!(
        staged_count(&stats),
        FILES as u64,
        "with the lever off every fsync'd staged-layout file must stay resident in local \
         staging (the shipped default drifted); log: {}",
        log.display()
    );
    assert_eq!(
        metric(&stats, "fsync_promoted_files").unwrap_or(0),
        0,
        "the lever is off: fsync must promote nothing"
    );

    mount.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}

/// Contract (b) + (c): the lever on. Every fsync'd file is promoted
/// exactly once (two racing fsyncs per file), the phase family stays
/// exact-sum with the new leg populated, an un-fsync'd file stays staged,
/// and after a SIGKILL — no dismount pass — a mount at another mount point
/// reads the fsync'd files byte-exact from the shared backend.
#[test]
fn lever_on_fsync_promotes_staged_layout_files_for_other_clients() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("on");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");
    let log = base.join("mount.log");
    let mut mount = spawn_mount(&meta, &mnt, &log, Some("1"));

    let paths = write_files(&mnt);
    assert_eq!(
        staged_count(&stats_json(&mnt)),
        FILES as u64,
        "the population must be staged-layout before the fsyncs"
    );
    let before = stats_json(&mnt);
    fsync_all_concurrently(&paths, 2);
    let after = stats_json(&mnt);

    // (c) the lever is fsync-scoped: a written, un-fsync'd file stays put.
    let unsynced = mnt.join("unsynced.bin");
    std::fs::File::create(&unsynced)
        .expect("create unsynced")
        .write_all(&pattern(FILES, file_len(FILES)))
        .expect("write unsynced");
    let with_unsynced = stats_json(&mnt);

    // (b) the resident count decayed to exactly the un-fsync'd file; every
    // fsync'd file promoted exactly once across the racing fsyncs.
    assert_eq!(
        staged_count(&after),
        0,
        "with the lever on every fsync'd staged-layout file must be promoted (resident \
         count must decay to 0); log: {}",
        log.display()
    );
    assert_eq!(
        staged_count(&with_unsynced),
        1,
        "a written, un-fsync'd file must stay staged (the lever is fsync-scoped)"
    );
    let promoted = metric(&after, "fsync_promoted_files").expect("fsync_promoted_files exported");
    assert_eq!(
        promoted,
        FILES as u64,
        "exactly one promotion per file across two racing fsyncs (fsync_promote_noops = {:?})",
        metric(&after, "fsync_promote_noops")
    );
    assert_eq!(
        metric(&after, "fsync_promote_failures").expect("fsync_promote_failures exported"),
        0,
        "no promotion may fail on a healthy mount; log: {}",
        log.display()
    );
    assert!(
        metric(&after, "fsync_promoted_bytes").expect("fsync_promoted_bytes exported")
            >= (0..FILES).map(|i| file_len(i) as u64).sum::<u64>(),
        "promoted bytes must account for the population"
    );
    assert_eq!(
        metric(&with_unsynced, "fsync_promoted_files").unwrap_or(0),
        promoted,
        "a write without fsync must not promote"
    );

    // The phase family: exact-sum, and the new leg is populated.
    let fam = &after["metrics"]["fsync_phase_ns"];
    let total_count = fam["total"]["count"].as_u64().expect("total count");
    let total_sum = fam["total"]["sum_ns"].as_u64().expect("total sum_ns");
    let calls = metric(&after, "fsync_calls").expect("fsync_calls")
        - metric(&before, "fsync_calls").unwrap_or(0);
    assert_eq!(
        calls,
        (2 * FILES) as u64,
        "every fsync(2) issued must be counted"
    );
    let promote_leg = &fam["staged_promote"];
    assert!(
        promote_leg.is_object(),
        "fsync_phase_ns must carry the staged_promote leg: {fam}"
    );
    assert_eq!(
        promote_leg["count"].as_u64().expect("staged_promote count"),
        total_count,
        "every phase records once per fsync"
    );
    assert!(
        promote_leg["sum_ns"]
            .as_u64()
            .expect("staged_promote sum_ns")
            > 0,
        "the promotion leg must carry the promotions' time"
    );
    let mut leg_sum = 0u64;
    for (name, h) in fam.as_object().expect("fsync_phase_ns object") {
        assert_eq!(
            h["count"].as_u64().expect("phase count"),
            total_count,
            "{name}: every phase's count moves with total's"
        );
        if name != "total" {
            leg_sum += h["sum_ns"].as_u64().expect("phase sum_ns");
        }
    }
    assert_eq!(
        leg_sum, total_sum,
        "Σ legs ≡ total to the ns (the exact-sum law)"
    );

    // The crash: no clean unmount, so nothing but the fsyncs can have
    // promoted. Another client's view: a fresh mount point ⇒ a fresh
    // staging slot; the origin's ring is not adopted.
    mount.kill9();
    let mnt2 = base.join("mnt2");
    let log2 = base.join("mount2.log");
    let mut mount2 = spawn_mount(&meta, &mnt2, &log2, None);
    let (mismatches, zero_reads) = verify_files(&mnt2);
    assert_eq!(
        mismatches,
        0,
        "{mismatches} of {FILES} fsync'd staged-layout files read wrong from a second mount \
         point ({zero_reads} as size-consistent ZEROS — the bytes exist only in the origin's \
         staging root: fsync did not promote); remount log: {}",
        log2.display()
    );
    assert_eq!(
        metric(&stats_json(&mnt2), "staged_payload_lost_reads").expect("staged_payload_lost_reads"),
        0,
        "the second mount degraded reads to zeros; log: {}",
        log2.display()
    );

    mount2.umount_clean();
    let _ = std::fs::remove_dir_all(&base);
}
