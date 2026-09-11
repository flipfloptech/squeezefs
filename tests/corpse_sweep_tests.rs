//! The mount-time corpse sweep's **double release** — fstests
//! `generic/749` on the 1.2.3 release chain (2026-09-11), the shipped
//! default path (`SQUEEZEFS_SMALL_FILE_PACKING` ON since PK7).
//!
//! The mechanism, as the scratch daemon's tape shows it at every mount of
//! the run's last hour: `sweep_unlinked_corpses` ran `delete_file` for
//! ~68 k prior-era corpses — each releasing its durable block references
//! (its own commit) and `begin_free`ing every block its layout maps — then
//! ONE `destroy_inodes` for the whole population, a 6 MB journal entry
//! against the 128 KiB whole-entry cap: refused, so the corpse RECORDS
//! (with their layouts) survived while their references were gone and
//! their blocks free-listed. The next mount seeded RAM refcounts from the
//! ledger alone (the two live tenants of the pack block at offset 0), re-ran
//! the sweep, and every surviving corpse whose layout named offset 0
//! decremented the LIVE owners' count: at 0 the pack block was TERMINALLY
//! freed — its bytes punched — under the live layouts (`file2` read 12,291
//! zeros right after the first cycle mount; `mwrite` then re-staged the
//! zeroed image, the `.out.bad`'s 0x58 fill). Thirty more corpses per
//! mount hit "begin_free REFUSED untracked offset 0" (6,372 lines).
//!
//! Contracts (mount-class, the real daemon on an unprivileged file-backed
//! sandbox; red-first):
//!
//! (a) **The ledger gate.** A corpse whose reference a prior sweep released
//!     but whose record survived (the failed destroy, armed here through
//!     `SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY`) plus a LIVE file that
//!     re-minted its offset: the next mount's sweep leaves the live file
//!     byte-exact, `meta_kv_block_refs_drift == 0`, the stale releases
//!     COUNTED skipped (`block_release_skipped_no_record`), no
//!     untracked-free refusals, `invariant_tripwires == 0`, and the
//!     corpse destroyed on that pass. Pre-fix the live file reads zeros.
//! (b) **The chunked destroy.** A corpse population whose destroy records
//!     exceed the whole-entry cap is fully reclaimed in ONE sweep
//!     (`corpse sweep reclaimed N`, N = the population), the next mount
//!     finds no corpse, and the online fsck is clean. Pre-fix the sweep
//!     fails "journal entry length … exceeds the 131072-byte whole-entry
//!     cap" at every mount, forever.
//! (c) **The exact `generic/749` shape** after (a)'s population: `falloc
//!     11k` + 25 × 512 B `pwrite` to 12,291 bytes + `syncfs` + cycle mount
//!     (the dismount pass packs the file at offset 0) + an mmap read of
//!     the EOF page tail + cycle mount + a 3-byte `mwrite` past EOF +
//!     `syncfs` → the content is unchanged. Pre-fix the file is zeros
//!     after the FIRST cycle.
//! (d) **The single over-cap corpse, crashed between entries** (the
//!     record's residual B, RECLAIM-ATOMIC): a corpse whose own destroy —
//!     its releases + thousands of xattrs + the `layout` xattr + the
//!     record — exceeds the whole-entry cap is destroyed ACROSS entries
//!     (releases first, the layout and the record last); stopped after its
//!     first entry (`SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER=1`) and
//!     SIGKILLed, the next mount's sweep finishes it — the surviving
//!     layout's releases witness no record (counted skipped, never a
//!     second free), no leak, no double free, drift 0, fsck clean. Pre-fix
//!     that corpse fails "exceeds the … whole-entry cap" at every mount.
//!
//! Self-skips through the testkit where a mount is not possible and rides
//! the require-mount gate (`tests/run_require_mount_gate.sh`).

use squeezefs_testkit::{mount_supported, site};
use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const KIB: usize = 1024;
const BLOCK: usize = 4 * 1024 * KIB;
/// The mount's default `--dismount-wait`.
const DEFAULT_DISMOUNT_WAIT: Duration = Duration::from_secs(10);
/// The seam that fails the sweep's destroy after its `delete_file`s.
const FAIL_DESTROY_SEAM: &str = "SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY";
/// The seam that stops a single-ino chunked destroy after N committed
/// entries (contract (d)).
const STOP_AFTER_SEAM: &str = "SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER";
/// The sweep's log faces.
const SWEEP_FOUND: &str = "mount-time corpse sweep:";
const SWEEP_FAILED: &str = "mount-time corpse sweep failed";
const SWEEP_RECLAIMED: &str = "mount-time corpse sweep reclaimed";
const UNTRACKED_REFUSED: &str = "begin_free REFUSED untracked offset";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

/// Deterministic content salted by the index (a zeros read or a cross-file
/// mix-up is caught).
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

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("sqfs_corpse_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    std::fs::canonicalize(&base).expect("canonicalize scratch dir")
}

/// Format one meta + one data volume WITH a staging dir (the default
/// format: bit 9 stamped, packing ON).
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

const TEARDOWN_CENSUS_MARKERS: &[&str] = &["Dismount clean", "at dismount"];

fn is_mounted(mnt: &Path) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|s| {
            s.lines()
                .any(|l| l.split(' ').nth(4) == Some(mnt.to_str().unwrap()))
        })
        .unwrap_or(false)
}

impl Mount {
    /// `squeezefs umount` on the default wait; waits for the teardown
    /// census and the daemon's clean exit.
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
                    "dismount census not reached within {:?}; log:\n{}",
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

    /// SIGKILL the daemon (no teardown, no FORGET sweep — the lost-FORGET
    /// corpse's birth) and detach the dead mount.
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

/// Spawn the real daemon on the fstests runner's posture (zc OFF, the
/// one-page inline ceiling pinned, the admin lane's dev override for the
/// online fsck). `extra_env` rides on top — the seams.
fn spawn_mount(meta: &Path, mnt: &Path, log: &Path, extra_env: &[(&str, &str)]) -> Mount {
    std::fs::create_dir_all(mnt).expect("create mountpoint");
    let logf = std::fs::File::create(log).expect("create log");
    let child = Command::new(bin())
        .arg("mount")
        .envs(extra_env.iter().copied())
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
        .env("SQUEEZEFS_INLINE_MAX_BYTES", "4096")
        .env("SQUEEZEFS_FSCK_SETTLE_MS", "200")
        .env("SQUEEZEFS_IPC_ALLOW_DEV", "1")
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

fn stat_u64(mnt: &Path, key: &str) -> u64 {
    let v = stats_json(mnt);
    v["metrics"][key]
        .as_u64()
        .or_else(|| v[key].as_u64())
        .unwrap_or_else(|| panic!("{key} exported on the stats inode"))
}

fn log_contains(log: &Path, needle: &str) -> bool {
    std::fs::read_to_string(log)
        .map(|t| t.contains(needle))
        .unwrap_or(false)
}

fn syncfs(mnt: &Path) {
    let root = std::fs::File::open(mnt).expect("open mount root");
    // SAFETY: syncfs on a live fd; the return is checked.
    let rc = unsafe { libc::syncfs(root.as_raw_fd()) };
    assert_eq!(rc, 0, "syncfs({}) failed", mnt.display());
}

/// `squeezefs fsck <mnt> --json` against the live mount.
fn online_fsck(mnt: &Path) -> serde_json::Value {
    let out = Command::new(bin())
        .arg("fsck")
        .arg(mnt)
        .arg("--json")
        .env("SQUEEZEFS_IPC_ALLOW_DEV", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run squeezefs fsck");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "fsck report is not JSON ({e}): stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert!(
        out.status.success(),
        "fsck exited {:?} — findings: {}",
        out.status.code(),
        report["findings"]
    );
    report
}

/// Wait until the sweep's queued block frees have completed
/// (`finish_free` counts `del_obj`): the freed offsets are back on the
/// free list, so the next allocation re-mints the LOWEST of them.
fn wait_reclaim_drained(mnt: &Path, expect_freed: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let queued = stat_u64(mnt, "block_free_reclaim_queued");
        let done = stat_u64(mnt, "del_obj");
        if queued >= expect_freed && done >= queued {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the sweep's frees never drained (block_free_reclaim_queued = {queued}, \
             del_obj = {done}, expected ≥ {expect_freed})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_fsync(path: &Path, bytes: &[u8]) -> File {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    f.sync_all()
        .unwrap_or_else(|e| panic!("fsync {}: {e}", path.display()));
    f
}

/// The corpse's size: two blocks — past the one-block staged ceiling, so
/// the layout is STRIPED and both blocks are published (write-through) and
/// durably referenced at fsync; a one-block file would be a staged-layout
/// entry in the local ring, owning no block at all.
const CORPSE_BLOCKS: usize = 2;

/// Birth a lost-FORGET corpse on a fresh volume: write the striped corpse
/// (fsync'd), HOLD the file open, unlink it (nlink 0, no FORGET while a
/// handle lives), and SIGKILL the daemon. The corpse's blocks are the
/// volume's first `CORPSE_BLOCKS` offsets.
fn birth_striped_corpse(meta: &Path, mnt: &Path, log: &Path) {
    let mut m1 = spawn_mount(meta, mnt, log, &[]);
    let corpse = mnt.join("corpse.bin");
    let held = write_fsync(&corpse, &pattern(1, CORPSE_BLOCKS * BLOCK));
    std::fs::remove_file(&corpse).expect("unlink the held-open corpse");
    m1.kill9();
    drop(held);
}

/// The live file's read after the sweep, with the two pre-fix faces named:
/// an EIO (the terminal free retired the offset's incarnation — a striped
/// read's binding validation refuses; the tripwire fires) or zeros (the
/// punched pack block a decorated-mapping read serves verbatim).
fn read_live(path: &Path, want: &[u8], log: &Path, what: &str) {
    match std::fs::read(path) {
        Ok(got) if got == want => {}
        Ok(got) => panic!(
            "{what} reads {} after the corpse sweep — the corpse's stale release, whose \
             durable record was already gone, decremented the live owner's refcount and \
             terminally freed its block (the generic/749 data loss); log: {}",
            if got.len() == want.len() && got.iter().all(|&b| b == 0) {
                "size-consistent ZEROS (the block was punched)"
            } else {
                "corrupt"
            },
            log.display()
        ),
        Err(e) => panic!(
            "{what} is unreadable after the corpse sweep ({e}) — the corpse's stale release \
             terminally freed the live owner's block and retired its incarnation (the \
             generic/749 data loss, the striped face); log: {}",
            log.display()
        ),
    }
}

/// The sweep-with-a-failed-destroy mount: the corpses' `delete_file`s
/// release their references and free their blocks; the destroy refuses
/// (the seam). Returns the mount, its frees drained.
fn mount_with_failed_destroy(
    meta: &Path,
    mnt: &Path,
    log: &Path,
    corpses: usize,
    freed_blocks: u64,
) -> Mount {
    let m = spawn_mount(meta, mnt, log, &[(FAIL_DESTROY_SEAM, "1")]);
    assert!(
        log_contains(log, &format!("{SWEEP_FOUND} {corpses} unlinked inode(s)")),
        "premise: the sweep found the {corpses} corpse(s); log: {}",
        log.display()
    );
    assert!(
        log_contains(log, SWEEP_FAILED),
        "premise: the seam failed the destroy; log: {}",
        log.display()
    );
    wait_reclaim_drained(mnt, freed_blocks);
    m
}

/// Contract (a): a corpse whose reference a prior sweep released — record
/// surviving — never frees a LIVE file's block on the next mount.
#[test]
fn a_released_corpses_stale_free_never_touches_a_live_files_block() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("gate");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");

    // Mount 1: the corpse owns blocks 0 and 1.
    birth_striped_corpse(&meta, &mnt, &base.join("m1.log"));

    // Mount 2: the sweep releases the corpse's two references and frees
    // its blocks; the destroy fails (the seam). A LIVE file then re-mints
    // the freed offsets (the free list serves before any fresh mint).
    let log2 = base.join("m2.log");
    let mut m2 = mount_with_failed_destroy(&meta, &mnt, &log2, 1, CORPSE_BLOCKS as u64);
    let live = mnt.join("live.bin");
    let want = pattern(2, CORPSE_BLOCKS * BLOCK);
    drop(write_fsync(&live, &want));
    assert_eq!(
        std::fs::read(&live).expect("read live"),
        want,
        "premise: the live file reads back on the writing mount"
    );
    assert!(
        stat_u64(&mnt, "alloc_from_freelist") >= 1,
        "premise: the live file re-minted at least one of the corpse's freed offsets"
    );
    m2.umount_timed();

    // Mount 3: the ledger seeds the live file's TWO references; the sweep
    // finds the surviving corpse, whose layout still names offsets 0 and 1.
    let log3 = base.join("m3.log");
    let mut m3 = spawn_mount(&meta, &mnt, &log3, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert!(
        log_contains(&log3, &format!("{SWEEP_FOUND} 1 unlinked inode(s)")),
        "premise: the released corpse survived to this mount; log: {}",
        log3.display()
    );
    // Whatever the sweep freed has drained — the punch, if any, has landed.
    wait_reclaim_drained(&mnt, 0);
    read_live(&live, &want, &log3, "the LIVE file");
    assert_eq!(
        stat_u64(&mnt, "meta_kv_block_refs_drift"),
        0,
        "the C8 oracle reads clean after the sweep"
    );
    assert_eq!(
        stat_u64(&mnt, "block_release_skipped_no_record"),
        CORPSE_BLOCKS as u64,
        "both stale releases are counted skipped (no record behind them)"
    );
    assert_eq!(
        stat_u64(&mnt, "block_untracked_free_refusals"),
        0,
        "the skip happens BEFORE the free funnel — no untracked-free refusal storm"
    );
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    assert!(
        log_contains(&log3, &format!("{SWEEP_RECLAIMED} 1 unlinked")),
        "the retry destroys the corpse; log: {}",
        log3.display()
    );
    assert!(
        !log_contains(&log3, UNTRACKED_REFUSED),
        "no refused-untracked line; log: {}",
        log3.display()
    );
    m3.umount_timed();

    // Mount 4: no corpse remains; the live file is still intact.
    let log4 = base.join("m4.log");
    let mut m4 = spawn_mount(&meta, &mnt, &log4, &[]);
    assert!(
        !log_contains(&log4, SWEEP_FOUND),
        "a swept volume carries no corpses; log: {}",
        log4.display()
    );
    assert_eq!(std::fs::read(&live).expect("read live"), want);
    m4.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

/// Contract (b): a corpse population whose destroy records exceed the
/// journal's whole-entry cap is reclaimed in ONE sweep, across chunked
/// destroys — never one refused 6 MB entry, forever.
#[test]
fn an_overcap_corpse_population_is_fully_reclaimed_in_one_sweep() {
    if !mount_supported(site!()) {
        return;
    }
    use squeezefs::meta_backend::kv::journal::{ENTRY_HDR_LEN, MAX_ENTRY_LEN};
    use squeezefs::meta_backend::kv::record::{INODE_KEY_LEN, RECORD_HEADER_LEN};

    // Empty corpses: each destroy stages exactly the inode record —
    // `1 (tree id) + header + key` bytes, the cheapest corpse there is —
    // so the population is sized off the cap and that cost to overshoot
    // ONE entry by an eighth (the in-process twin's derivation).
    let fits = ((MAX_ENTRY_LEN - ENTRY_HDR_LEN) as usize) / (1 + RECORD_HEADER_LEN + INODE_KEY_LEN);
    let corpses = fits + fits / 8;
    let base = scratch("chunks");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");

    // Mount 1: birth the population — every file held open across its
    // unlink, then SIGKILL (no FORGET ever arrives).
    let mut m1 = spawn_mount(&meta, &mnt, &base.join("m1.log"), &[]);
    let mut held: Vec<File> = Vec::with_capacity(corpses);
    for i in 0..corpses {
        let path = mnt.join(format!("corpse_{i:05}"));
        held.push(File::create(&path).unwrap_or_else(|e| panic!("create {}: {e}", path.display())));
    }
    syncfs(&mnt);
    for i in 0..corpses {
        std::fs::remove_file(mnt.join(format!("corpse_{i:05}"))).expect("unlink held-open file");
    }
    m1.kill9();
    drop(held);

    // Mount 2: the sweep reclaims the WHOLE population.
    let log2 = base.join("m2.log");
    let mut m2 = spawn_mount(&meta, &mnt, &log2, &[]);
    assert!(
        log_contains(&log2, &format!("{SWEEP_FOUND} {corpses} unlinked inode(s)")),
        "premise: the sweep found the population; log: {}",
        log2.display()
    );
    assert!(
        !log_contains(&log2, SWEEP_FAILED),
        "the sweep must not fail — pre-fix: one destroy entry for the whole population, \
         refused past the whole-entry cap; log: {}",
        log2.display()
    );
    assert!(
        log_contains(&log2, &format!("{SWEEP_RECLAIMED} {corpses} unlinked")),
        "the whole population is reclaimed in ONE sweep; log: {}",
        log2.display()
    );
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    let report = online_fsck(&mnt);
    assert_eq!(
        report["findings"].as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "fsck is clean after the chunked sweep: {}",
        report["findings"]
    );
    m2.umount_timed();

    // Mount 3: nothing left to sweep.
    let log3 = base.join("m3.log");
    let mut m3 = spawn_mount(&meta, &mnt, &log3, &[]);
    assert!(
        !log_contains(&log3, SWEEP_FOUND),
        "a swept volume carries no corpses; log: {}",
        log3.display()
    );
    m3.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

/// fstests generic/749 case 2's file: `falloc 0 11k` then `pwrite -b 512
/// 0 12291` (0xaa) — 24 full 512-byte writes and a 3-byte tail.
const G749_LEN: usize = 12_291;
const G749_FILL: u8 = 0xaa;

fn write_g749_file(path: &Path) -> Vec<u8> {
    let f = File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    // SAFETY: fallocate on a live fd; the return is checked.
    let rc = unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, 11 * KIB as libc::off_t) };
    assert_eq!(
        rc,
        0,
        "fallocate({}) failed: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    let want = vec![G749_FILL; G749_LEN];
    let mut off = 0usize;
    while off < G749_LEN {
        let end = (off + 512).min(G749_LEN);
        // SAFETY: pwrite from a live slice at a valid offset; the return is checked.
        let n = unsafe {
            libc::pwrite(
                f.as_raw_fd(),
                want[off..end].as_ptr().cast(),
                end - off,
                off as libc::off_t,
            )
        };
        assert_eq!(n as usize, end - off, "pwrite({}) short", path.display());
        off = end;
    }
    want
}

struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(f: &File, len: usize, prot: libc::c_int) -> Self {
        // SAFETY: a fresh shared mapping of a live fd; the result is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            ptr,
            libc::MAP_FAILED,
            "mmap failed: {}",
            std::io::Error::last_os_error()
        );
        Self { ptr, len }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is live for `len` bytes until `Drop`.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is live and writable for `len` bytes until `Drop`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the range `new` mapped.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// Contract (c): the exact generic/749 case-2 shape on top of (a)'s
/// population — the released corpse owns offset 0, the file is packed
/// there at the first cycle's dismount, and the next mount's sweep must
/// leave it byte-exact through the mmap tail read and the past-EOF
/// `mwrite`.
#[test]
fn the_generic_749_shape_survives_a_released_corpse_at_the_pack_offset() {
    if !mount_supported(site!()) {
        return;
    }
    let base = scratch("g749");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");

    // Mount 1: the corpse owns the volume's first blocks — the offsets the
    // next mount's promotions pack into.
    birth_striped_corpse(&meta, &mnt, &base.join("m1.log"));

    // Mount 2 (the test's "mount"): the sweep releases the corpse's
    // references and frees its blocks; the destroy fails. Then
    // generic/749's file: falloc + 25 × 512 B pwrite + syncfs. Its
    // dismount promotion packs it into one of the freed offsets.
    let log2 = base.join("m2.log");
    let mut m2 = mount_with_failed_destroy(&meta, &mnt, &log2, 1, CORPSE_BLOCKS as u64);
    let file2 = mnt.join("file2");
    let want = write_g749_file(&file2);
    syncfs(&mnt);
    assert_eq!(
        std::fs::read(&file2).expect("read file2"),
        want,
        "premise: the file reads back on the writing mount"
    );
    m2.umount_timed();
    assert!(
        log_contains(&log2, "promoted 1 staged-layout file(s)"),
        "premise: the dismount pass promoted the file; log: {}",
        log2.display()
    );

    // Cycle 1: the sweep re-runs over the surviving corpse. Pre-fix its
    // stale release of the pack block's offset terminally frees the block
    // under the live tenant.
    let log3 = base.join("m3.log");
    let mut m3 = spawn_mount(&meta, &mnt, &log3, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert!(
        log_contains(&log3, &format!("{SWEEP_FOUND} 1 unlinked inode(s)")),
        "premise: the released corpse survived to the first cycle; log: {}",
        log3.display()
    );
    wait_reclaim_drained(&mnt, 0);
    read_live(&file2, &want, &log3, "generic/749's packed file");
    // The mmap read of the EOF page tail (case 2's first act).
    let page = 4 * KIB;
    let ml = G749_LEN.div_ceil(page) * page;
    {
        let f = File::open(&file2).expect("open file2 read-only");
        let map = Mapping::new(&f, ml, libc::PROT_READ);
        assert_eq!(&map.bytes()[..G749_LEN], &want[..]);
        assert!(
            map.bytes()[G749_LEN..].iter().all(|&b| b == 0),
            "the page tail past EOF reads as zeros"
        );
    }
    assert_eq!(stat_u64(&mnt, "meta_kv_block_refs_drift"), 0);
    assert_eq!(
        stat_u64(&mnt, "block_release_skipped_no_record"),
        CORPSE_BLOCKS as u64,
        "the corpse's stale releases are counted skipped, never freed"
    );
    assert_eq!(stat_u64(&mnt, "block_untracked_free_refusals"), 0);
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    assert_eq!(stat_u64(&mnt, "packed_mapping_refusals"), 0);
    assert!(
        log_contains(&log3, &format!("{SWEEP_RECLAIMED} 1 unlinked")),
        "the retry destroys the corpse; log: {}",
        log3.display()
    );
    m3.umount_timed();

    // Cycle 2: `mwrite` 3 bytes into the page tail past EOF, then syncfs.
    let log4 = base.join("m4.log");
    let mut m4 = spawn_mount(&meta, &mnt, &log4, &[]);
    assert!(
        !log_contains(&log4, SWEEP_FOUND),
        "no corpse survives to the second cycle; log: {}",
        log4.display()
    );
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file2)
            .expect("open file2 read-write");
        let mut map = Mapping::new(&f, ml, libc::PROT_READ | libc::PROT_WRITE);
        for b in &mut map.bytes_mut()[G749_LEN..ml] {
            *b = 0x58;
        }
    }
    syncfs(&mnt);
    let got = std::fs::read(&file2).expect("read file2 after the mwrite");
    assert_eq!(
        got.len(),
        G749_LEN,
        "the past-EOF mwrite must not extend the file"
    );
    assert!(
        got == want,
        "generic/749: the file's content changed after the mwrite tail + sync (the \
         `.out.bad` shape); log: {}",
        log4.display()
    );
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    m4.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}

/// How many `user.*` xattrs push the striped corpse's destroy footprint —
/// `CORPSE_BLOCKS` release `Delete`s + one `Delete` per xattr + the
/// `layout` xattr's + the inode record's, each `1 (tree id) + header + key`
/// bytes — past ONE journal entry by an eighth. Derived from the cap and
/// the record framing (the in-process twin's arithmetic).
fn overcap_xattr_count() -> usize {
    use squeezefs::meta_backend::kv::block_refs::BLOCK_REF_KEY_LEN;
    use squeezefs::meta_backend::kv::journal::{ENTRY_HDR_LEN, MAX_ENTRY_LEN};
    use squeezefs::meta_backend::kv::record::{INODE_KEY_LEN, RECORD_HEADER_LEN, XATTR_KEY_LEN};
    let per_record = |key_len: usize| 1 + RECORD_HEADER_LEN + key_len;
    let cap = (MAX_ENTRY_LEN - ENTRY_HDR_LEN) as usize;
    let fixed = CORPSE_BLOCKS * per_record(BLOCK_REF_KEY_LEN)
        + per_record(XATTR_KEY_LEN)
        + per_record(INODE_KEY_LEN);
    (cap + cap / 8 - fixed).div_ceil(per_record(XATTR_KEY_LEN))
}

/// `fsetxattr(fd, name, value)` on a live fd; the return is checked.
fn fsetxattr(f: &File, name: &str, value: &[u8]) {
    let cname = std::ffi::CString::new(name).expect("xattr name");
    // SAFETY: a live fd, a NUL-terminated name and a valid value slice.
    let rc = unsafe {
        libc::fsetxattr(
            f.as_raw_fd(),
            cname.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(
        rc,
        0,
        "fsetxattr({name}) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// Contract (d): one corpse whose destroy footprint exceeds the whole-entry
/// cap, its chunked destroy stopped after the first entry and the daemon
/// SIGKILLed — the next mount finishes it without a leak or a double free.
#[test]
fn a_single_overcap_corpse_stopped_between_entries_is_finished_by_the_next_mount() {
    if !mount_supported(site!()) {
        return;
    }
    let xattrs = overcap_xattr_count();
    let base = scratch("chunked");
    let staging = base.join("staging");
    let meta = format_volume(&base, &staging);
    let mnt = base.join("mnt");

    // Mount 1: the striped corpse (blocks 0 and 1, fsync'd) carrying
    // enough xattrs that its destroy cannot fit one entry; held open across
    // the unlink, then SIGKILL (no FORGET ever arrives). An ANCHOR file
    // keeps the ledger non-empty once the corpse's references are gone
    // (an empty ledger is declined at mount — §6.1 — and the layout walk,
    // which counts a corpse's layout, would backfill the very references
    // whose absence this contract observes).
    let anchor = mnt.join("anchor.bin");
    let anchor_want = pattern(3, CORPSE_BLOCKS * BLOCK);
    {
        let mut m1 = spawn_mount(&meta, &mnt, &base.join("m1.log"), &[]);
        drop(write_fsync(&anchor, &anchor_want));
        let corpse = mnt.join("corpse.bin");
        let held = write_fsync(&corpse, &pattern(1, CORPSE_BLOCKS * BLOCK));
        for i in 0..xattrs {
            fsetxattr(&held, &format!("user.k{i}"), b"v");
        }
        syncfs(&mnt);
        std::fs::remove_file(&corpse).expect("unlink the held-open corpse");
        m1.kill9();
        drop(held);
    }

    // Mount 2: the sweep finds the corpse; its chunked destroy commits the
    // FIRST entry (the releases lead) and stops — the record and its layout
    // survive. Then SIGKILL: the durable state is exactly a crash between
    // two entries of the destroy.
    let log2 = base.join("m2.log");
    let mut m2 = spawn_mount(&meta, &mnt, &log2, &[(STOP_AFTER_SEAM, "1")]);
    assert!(
        log_contains(&log2, &format!("{SWEEP_FOUND} 1 unlinked inode(s)")),
        "premise: the sweep found the corpse; log: {}",
        log2.display()
    );
    assert_eq!(
        stat_u64(&mnt, "reclaim_single_ino_chunked_destroys"),
        1,
        "the over-cap corpse's destroy must run CHUNKED across entries — pre-fix its one \
         entry was refused at the whole-entry cap ({}); log: {}",
        if log_contains(&log2, SWEEP_FAILED) {
            "the sweep FAILED at the cap, as at every mount before this fix"
        } else {
            "no failure logged"
        },
        log2.display()
    );
    assert!(
        !log_contains(&log2, &format!("{SWEEP_RECLAIMED} 1 unlinked")),
        "premise: the stopped destroy completed nothing; log: {}",
        log2.display()
    );
    // The first entry's releases were witnessed and committed: their
    // blocks free on THIS mount (the ledger says free; RAM follows).
    wait_reclaim_drained(&mnt, CORPSE_BLOCKS as u64);
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    m2.kill9();

    // Mount 3: the sweep re-reads the surviving layout; its releases
    // witness NO record (skipped, never a second free) and the destroy
    // completes.
    let log3 = base.join("m3.log");
    let mut m3 = spawn_mount(&meta, &mnt, &log3, &[("SQUEEZEFS_BLOCK_REFS_VERIFY", "1")]);
    assert!(
        log_contains(&log3, &format!("{SWEEP_FOUND} 1 unlinked inode(s)")),
        "premise: the half-destroyed corpse survived to this mount; log: {}",
        log3.display()
    );
    assert!(
        log_contains(&log3, &format!("{SWEEP_RECLAIMED} 1 unlinked")),
        "the next mount FINISHES the destroy; log: {}",
        log3.display()
    );
    wait_reclaim_drained(&mnt, 0);
    assert_eq!(
        stat_u64(&mnt, "meta_kv_block_refs_drift"),
        0,
        "the C8 oracle reads clean"
    );
    assert_eq!(
        stat_u64(&mnt, "block_release_skipped_no_record"),
        CORPSE_BLOCKS as u64,
        "the layout's releases had no record behind them (the first entry committed them) \
         and are counted skipped"
    );
    assert_eq!(stat_u64(&mnt, "block_untracked_free_refusals"), 0);
    assert_eq!(stat_u64(&mnt, "block_double_frees"), 0);
    assert_eq!(stat_u64(&mnt, "invariant_tripwires"), 0);
    assert!(
        !log_contains(&log3, UNTRACKED_REFUSED),
        "no refused-untracked line; log: {}",
        log3.display()
    );
    // The freed offsets are re-mintable by a live file, byte-exact; the
    // anchor is untouched.
    let live = mnt.join("live.bin");
    let want = pattern(2, CORPSE_BLOCKS * BLOCK);
    drop(write_fsync(&live, &want));
    assert_eq!(std::fs::read(&live).expect("read live"), want);
    assert_eq!(std::fs::read(&anchor).expect("read anchor"), anchor_want);
    let report = online_fsck(&mnt);
    assert_eq!(
        report["findings"].as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "fsck is clean after the finished destroy: {}",
        report["findings"]
    );
    m3.umount_timed();

    // Mount 4: nothing left to sweep; the live file and the anchor intact.
    let log4 = base.join("m4.log");
    let mut m4 = spawn_mount(&meta, &mnt, &log4, &[]);
    assert!(
        !log_contains(&log4, SWEEP_FOUND),
        "a swept volume carries no corpses; log: {}",
        log4.display()
    );
    assert_eq!(std::fs::read(&live).expect("read live"), want);
    assert_eq!(std::fs::read(&anchor).expect("read anchor"), anchor_want);
    m4.umount_timed();
    let _ = std::fs::remove_dir_all(&base);
}
